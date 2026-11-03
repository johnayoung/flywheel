//! The accept loop under [`super::serve`]: hyper's HTTP/1 server with the
//! bounds `axum::serve` leaves out, so a client that opens connections and
//! never finishes a request cannot pin the hub's file descriptors.
//!
//! * A request head must arrive whole within [`HEADER_READ_TIMEOUT`] (the
//!   same clock closes an idle keep-alive connection).
//! * At most [`connection_cap`] authenticated connections are served at
//!   once; an upgraded WebSocket no longer counts. Connections no request
//!   has authenticated get a separate, small budget with a per-client cap
//!   and shorter deadlines ([`super::slots`]), and a stop or handoff never
//!   waits on them -- a flood of slow unauthenticated clients cannot
//!   starve the operator.
//! * [`raise_fd_limit`] lifts the soft `RLIMIT_NOFILE` to the hard one.

use super::Peer;
use super::slots::{Admission, Phase, Slots, UNAUTH_PER_CLIENT, unauthenticated_cap};
use crate::handoff::GatedListener;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use std::future::Future;
use std::net::IpAddr;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;

/// How long a client has to send a complete request head, and how long an
/// idle keep-alive connection is kept.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a stopping server waits for requests in flight before it lets
/// their connections go (a client that never finishes must not hold the
/// hub's exit past `fw hub stop`'s wait).
pub const CLOSE_GRACE: Duration = Duration::from_secs(10);
/// Bounds of [`connection_cap`].
const MIN_CONNECTIONS: usize = 16;
const MAX_CONNECTIONS: usize = 1024;

/// Authenticated connections served at once: a quarter of the descriptors this process
/// may open (the rest are PTYs, state files, and git), within
/// `MIN_CONNECTIONS..=MAX_CONNECTIONS`.
pub fn connection_cap() -> usize {
    let soft = fd_limit().map_or(1024, |(soft, _)| soft);
    usize::try_from(soft / 4)
        .unwrap_or(MAX_CONNECTIONS)
        .clamp(MIN_CONNECTIONS, MAX_CONNECTIONS)
}

/// The `(soft, hard)` descriptor limits.
fn fd_limit() -> Option<(libc::rlim_t, libc::rlim_t)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one rlimit through a valid pointer.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) };
    (rc == 0).then_some((limit.rlim_cur, limit.rlim_max))
}

/// Raise the soft descriptor limit to the hard one (a systemd user unit
/// starts at 1024). Best effort: the cap follows whatever is in force.
pub fn raise_fd_limit() {
    let Some((soft, hard)) = fd_limit() else {
        return;
    };
    if soft >= hard {
        return;
    }
    let limit = libc::rlimit {
        rlim_cur: hard,
        rlim_max: hard,
    };
    // SAFETY: setrlimit reads one rlimit through a valid pointer.
    let _ = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) };
}

/// Resolves once the server is closing (or its sender is gone).
async fn closed(close: &mut watch::Receiver<bool>) {
    let _ = close.wait_for(|closing| *closing).await;
}

/// Serve `app` on `listener` until `shutdown` resolves; then set `closing`
/// (WebSockets watch it), drop every unauthenticated connection, let
/// authenticated requests in flight finish, and return once every
/// connection task has, or after [`CLOSE_GRACE`]. `cap` sizes the main
/// pool (see [`super::slots`]); `trusted_proxies` (loopback always) are
/// counted per client by the `X-Forwarded-For` of their requests, not as
/// one address.
pub async fn run(
    mut listener: GatedListener,
    app: Router,
    cap: usize,
    trusted_proxies: Vec<IpAddr>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    closing: watch::Sender<bool>,
) {
    let slots = Arc::new(Slots::new(cap, unauthenticated_cap(cap), UNAUTH_PER_CLIENT));
    let gate = listener.gate().clone();
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let mut shutdown = pin!(shutdown);
    loop {
        // Accept errors are retried (or backed off) inside; a frozen gate
        // (a live handoff) leaves connections in the backlog.
        let (stream, remote) = tokio::select! {
            accepted = axum::serve::Listener::accept(&mut listener) => accepted,
            () = &mut shutdown => break,
        };
        let ip = remote.ip().to_canonical();
        let proxy = ip.is_loopback() || trusted_proxies.contains(&ip);
        // Over its share of unauthenticated connections: hung up at once.
        let Some(conn) = slots.admit((!proxy).then_some(ip)) else {
            continue;
        };
        let peer = Peer {
            remote,
            local: stream.local_addr().ok(),
        };
        let admission = Admission::new(Arc::clone(&slots), Arc::clone(&conn));
        let request_slots = Arc::clone(&slots);
        let request_conn = Arc::clone(&conn);
        let service = app
            .clone()
            .map_request(move |mut request: hyper::Request<Incoming>| {
                request_slots.request_started(&request_conn);
                request.extensions_mut().insert(ConnectInfo(peer));
                request.extensions_mut().insert(admission.clone());
                request.map(Body::new)
            });
        let mut close = closing.subscribe();
        let gate = gate.clone();
        let slots = Arc::clone(&slots);
        let done = done_tx.clone();
        tokio::spawn(async move {
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT);
            let mut served = pin!(
                builder
                    .serve_connection(TokioIo::new(stream), TowerToHyperService::new(service))
                    .with_upgrades()
            );
            loop {
                let (phase, deadline) = conn.standing();
                if phase == Phase::Closed {
                    // Evicted, past its deadline, or dropped by a stop or
                    // freeze: hang up mid-request.
                    break;
                }
                let pending = phase == Phase::Pending;
                tokio::select! {
                    _ = served.as_mut() => break,
                    () = closed(&mut close) => {
                        if !slots.close_if_pending(&conn) {
                            served.as_mut().graceful_shutdown();
                            let _ = served.as_mut().await;
                        }
                        break;
                    }
                    () = gate.frozen(), if pending => {
                        slots.close_if_pending(&conn);
                    }
                    () = tokio::time::sleep_until(deadline), if pending => {
                        slots.close_if_pending(&conn);
                    }
                    () = conn.wake.notified() => {}
                }
            }
            // An upgraded connection lives on in its WebSocket task; it no
            // longer holds a slot.
            slots.release(&conn);
            drop(done);
        });
    }
    drop(listener);
    let _ = closing.send(true);
    drop(done_tx);
    let _ = tokio::time::timeout(CLOSE_GRACE, done_rx.recv()).await;
}
