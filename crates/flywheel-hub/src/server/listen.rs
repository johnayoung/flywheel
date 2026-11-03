//! The accept loop under [`super::serve`]: hyper's HTTP/1 server with the
//! bounds `axum::serve` leaves out, so a client that opens connections and
//! never finishes a request cannot pin the hub's file descriptors.
//!
//! * A request head must arrive whole within [`HEADER_READ_TIMEOUT`] (the
//!   same clock closes an idle keep-alive connection).
//! * At most [`connection_cap`] connections are being served at once; an
//!   upgraded WebSocket (authenticated by then) no longer counts.
//! * [`raise_fd_limit`] lifts the soft `RLIMIT_NOFILE` to the hard one.

use super::Peer;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, watch};
use tower::ServiceExt;

/// How long a client has to send a complete request head, and how long an
/// idle keep-alive connection is kept.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds of [`connection_cap`].
const MIN_CONNECTIONS: usize = 16;
const MAX_CONNECTIONS: usize = 1024;

/// Connections served at once: a quarter of the descriptors this process
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

/// Accept errors that concern one connection only (retry at once); any
/// other (EMFILE, ENFILE, ENOBUFS) backs off a moment, as `axum::serve`
/// does.
fn connection_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

/// Serve `app` on `listener` until `shutdown` resolves; then set `closing`
/// (WebSockets watch it), let in-flight requests finish, and return once
/// every connection task has.
pub async fn run(
    listener: TcpListener,
    app: Router,
    cap: usize,
    shutdown: impl Future<Output = ()> + Send + 'static,
    closing: watch::Sender<bool>,
) {
    let slots = Arc::new(Semaphore::new(cap));
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let mut shutdown = pin!(shutdown);
    loop {
        let permit = tokio::select! {
            permit = Arc::clone(&slots).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
            () = &mut shutdown => break,
        };
        let (stream, remote) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) if connection_error(&e) => continue,
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let peer = Peer {
            remote,
            local: stream.local_addr().ok(),
        };
        let service = app
            .clone()
            .map_request(move |mut request: hyper::Request<Incoming>| {
                request.extensions_mut().insert(ConnectInfo(peer));
                request.map(Body::new)
            });
        let mut close = closing.subscribe();
        let done = done_tx.clone();
        tokio::spawn(async move {
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT);
            let mut conn = pin!(
                builder
                    .serve_connection(TokioIo::new(stream), TowerToHyperService::new(service))
                    .with_upgrades()
            );
            let closed = async move { close.wait_for(|closing| *closing).await.is_ok() };
            let shutting_down = tokio::select! {
                _ = conn.as_mut() => false,
                _ = closed => true,
            };
            if shutting_down {
                conn.as_mut().graceful_shutdown();
                let _ = conn.await;
            }
            // An upgraded connection lives on in its WebSocket task; it no
            // longer holds a slot.
            drop(permit);
            drop(done);
        });
    }
    drop(listener);
    let _ = closing.send(true);
    drop(done_tx);
    let _ = done_rx.recv().await;
}
