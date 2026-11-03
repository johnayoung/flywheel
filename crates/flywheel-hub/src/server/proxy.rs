//! Multiple machines over HTTP: this hub's side (`/api/machines`, the
//! `/m/<machine>/api/...` proxy) and the remote side (`/api/peers`,
//! `/api/peer`, `/api/whoami`). See `machines.rs` and `peers.rs`.
//!
//! The proxy forwards a browser's (or the CLI's) API calls and WebSockets
//! to a registered machine with that machine's peer credential: the
//! browser authenticates to this hub only, never sees a remote credential,
//! and never talks to another origin. Only `/api/...` on a registered
//! machine's base URL is ever reached (`Machine::target`). WebSockets are
//! relayed frame by frame, each direction awaiting the other side's
//! write, so a slow browser slows the remote socket down (whose own flow
//! control then skips ahead with a fresh snapshot) instead of queueing.

use super::{
    ApiJson, AppState, Client, CredentialWatch, WS_MAX_MESSAGE, WS_PING_INTERVAL, error_response,
    handoff_restarting, login_recheck, origin_ok, upgrade_refused,
};
use crate::hub::HubEvent;
use crate::machines::{
    CALL_TIMEOUT, LinkError, Machine, MachineError, error_text, host_name, record,
};
use crate::peers::{PeerError, PeerInfo};
use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite;

/// Largest request body forwarded to a machine.
const MAX_FORWARD_BODY: usize = 8 * 1024 * 1024;
/// How long removing a machine waits for it to revoke its credential.
const REVOKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Close code: the machine refused this hub's credential (revoked there).
pub const WS_CLOSE_MACHINE_REFUSED: u16 = 4010;
/// Close code: the machine was removed from this hub.
pub const WS_CLOSE_MACHINE_REMOVED: u16 = 4011;
/// Close code: the connection to the machine was lost.
pub const WS_CLOSE_MACHINE_LOST: u16 = 1011;

// --- the remote side: who am I, peers ---------------------------------------

/// `GET /api/whoami`: what the presented credential is here.
pub(super) async fn whoami(peer: Option<axum::Extension<PeerInfo>>) -> Response {
    let peer = peer.map(|axum::Extension(p)| p);
    Json(json!({
        "scope": if peer.is_some() { "peer" } else { "full" },
        "peer": peer,
        "version": env!("CARGO_PKG_VERSION"),
        "host": host_name(),
    }))
    .into_response()
}

fn peer_error(e: &PeerError) -> Response {
    let status = match e {
        PeerError::Invalid(_) => StatusCode::BAD_REQUEST,
        PeerError::NotFound(_) => StatusCode::NOT_FOUND,
        PeerError::Conflict(_) => StatusCode::CONFLICT,
        PeerError::Io { .. } | PeerError::Token(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, &e.to_string())
}

/// `DELETE /api/peer`: a peer credential revokes itself (the other hub
/// removing this machine).
pub(super) async fn revoke_self(
    State(app): State<AppState>,
    peer: Option<axum::Extension<PeerInfo>>,
) -> Response {
    let Some(axum::Extension(peer)) = peer else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "only a peer credential revokes itself; revoke peers with DELETE /api/peers/{id}",
        );
    };
    match app.peers.revoke(&peer.id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => peer_error(&e),
    }
}

/// `GET /api/peers`.
pub(super) async fn list_peers(State(app): State<AppState>) -> Response {
    match app.peers.list() {
        Ok(peers) => {
            let peers: Vec<_> = peers
                .into_iter()
                .map(|(peer, last_used)| {
                    json!({
                        "id": peer.id,
                        "name": peer.name,
                        "created": peer.created,
                        "last_used": last_used,
                    })
                })
                .collect();
            Json(json!({ "peers": peers })).into_response()
        }
        Err(e) => peer_error(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IssuePeer {
    name: String,
}

/// `POST /api/peers {"name"}` -> 201 `{"peer","credential"}` (shown once).
pub(super) async fn issue_peer(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<IssuePeer>,
) -> Response {
    match app.peers.issue(&body.name) {
        Ok((peer, credential)) => (
            StatusCode::CREATED,
            Json(json!({ "peer": peer, "credential": credential })),
        )
            .into_response(),
        Err(e) => peer_error(&e),
    }
}

/// `DELETE /api/peers/{id or name}`: its requests stop at once and its
/// sockets close (4003).
pub(super) async fn revoke_peer(
    State(app): State<AppState>,
    Path(which): Path<String>,
) -> Response {
    match app.peers.revoke(&which) {
        Ok(peer) => Json(json!({ "peer": peer })).into_response(),
        Err(e) => peer_error(&e),
    }
}

// --- this side: machines -------------------------------------------------------

fn machine_error(e: &MachineError) -> Response {
    let status = match e {
        MachineError::Invalid(_) => StatusCode::BAD_REQUEST,
        MachineError::NotFound(_) => StatusCode::NOT_FOUND,
        MachineError::Conflict(_) => StatusCode::CONFLICT,
        MachineError::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, &e.to_string())
}

fn link_error(machine: &str, e: &LinkError) -> Response {
    let (status, state) = match e {
        LinkError::Invalid(_) => (StatusCode::NOT_FOUND, None),
        LinkError::Refused(_) => (StatusCode::BAD_GATEWAY, Some("refused")),
        LinkError::Unreachable(_) => (StatusCode::BAD_GATEWAY, Some("offline")),
    };
    (
        status,
        Json(json!({
            "error": e.to_string(),
            "machine": machine,
            "machine_state": state,
        })),
    )
        .into_response()
}

/// `GET /api/machines` -> `{"host","machines":[view]}`.
pub(super) async fn list_machines(State(app): State<AppState>) -> Response {
    let machines: Vec<_> = app.machines.list().iter().map(|m| m.view()).collect();
    Json(json!({ "host": host_name(), "machines": machines })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AddMachine {
    name: String,
    url: String,
    /// A peer credential minted there (`fw hub peer issue`).
    credential: Option<String>,
    /// Or the remote hub's token, used once to mint a peer credential
    /// (never stored).
    token: Option<String>,
    /// The peer's name on the remote when minting (default: this host).
    peer_name: Option<String>,
    fingerprint: Option<String>,
}

/// A peer name for this hub on another: its host name, as a peer name.
fn default_peer_name() -> String {
    let name: String = host_name()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .take(64)
        .collect();
    if name.is_empty() { "hub".into() } else { name }
}

/// `POST /api/machines` -> 201 the machine's view, once its credential was
/// checked to be a peer credential there (`whoami`).
pub(super) async fn add_machine(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<AddMachine>,
) -> Response {
    let name = match crate::machines::validate_machine_name(&body.name) {
        Ok(name) => name,
        Err(e) => return machine_error(&e),
    };
    if app.machines.get(&name).is_some() {
        return error_response(
            StatusCode::CONFLICT,
            &format!("a machine named {name} exists (remove it first)"),
        );
    }
    let fingerprint = body.fingerprint.as_deref().filter(|f| !f.trim().is_empty());
    let given = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let (credential, minted) = match (given(&body.credential), given(&body.token)) {
        (Some(_), Some(_)) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "give a peer credential or the machine's token, not both",
            );
        }
        (None, None) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "give a peer credential (`fw hub peer issue --name <this hub>` on that machine) \
                 or its token (used once to mint one; never stored)",
            );
        }
        (Some(credential), None) => (credential, false),
        (None, Some(token)) => {
            let minter = match Machine::new(record(&name, &body.url, &token, fingerprint)) {
                Ok(m) => m,
                Err(e) => return machine_error(&e),
            };
            let peer_name = body.peer_name.clone().unwrap_or_else(default_peer_name);
            match minter.mint_peer(&peer_name).await {
                Ok((_, credential)) => (credential, true),
                Err(e) => return error_response(StatusCode::BAD_GATEWAY, &e),
            }
        }
    };
    let machine = match Machine::new(record(&name, &body.url, &credential, fingerprint)) {
        Ok(m) => m,
        Err(e) => return machine_error(&e),
    };
    let who = match machine.whoami().await {
        Ok(who) => who,
        Err(e) => return link_error(&name, &e),
    };
    if who.scope != "peer" {
        return error_response(
            StatusCode::BAD_REQUEST,
            "that is the machine's token, not a peer credential: this hub never stores a token. \
             Mint a peer credential with it instead (`token` in the API, --mint on the CLI), \
             or run `fw hub peer issue --name <this hub>` there",
        );
    }
    let mut stored = machine.record().clone();
    if let Some(peer) = &who.peer {
        stored.peer_id = Some(peer.id.clone());
        stored.peer_name = Some(peer.name.clone());
    }
    let machine = match Machine::new(stored) {
        Ok(m) => m,
        Err(e) => return machine_error(&e),
    };
    // Carry over what the check learned.
    let _ = machine.whoami().await;
    match app.machines.insert(machine) {
        Ok(machine) => {
            app.hub.emit(HubEvent::Sessions);
            (StatusCode::CREATED, Json(machine.view())).into_response()
        }
        Err(e) => {
            if minted
                && let Ok(orphan) = Machine::new(record(&name, &body.url, &credential, fingerprint))
            {
                let _ = tokio::time::timeout(REVOKE_TIMEOUT, orphan.revoke_self()).await;
            }
            machine_error(&e)
        }
    }
}

/// `DELETE /api/machines/{name}`: forget it, and ask it to revoke the
/// credential (best effort: an unreachable machine keeps the peer until
/// `fw hub peer revoke` there). Open proxied sockets close (4011).
pub(super) async fn remove_machine(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let machine = match app.machines.remove(&name) {
        Ok(m) => m,
        Err(e) => return machine_error(&e),
    };
    app.hub.emit(HubEvent::Sessions);
    let revoked = match tokio::time::timeout(REVOKE_TIMEOUT, machine.revoke_self()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("machine {name} did not answer in time")),
    };
    Json(json!({
        "machine": name,
        "revoked": revoked.is_ok(),
        "note": revoked.err().map(|why| format!(
            "{why}; revoke it there with `fw hub peer revoke {}`",
            machine.record().peer_name.as_deref().unwrap_or("<peer>")
        )),
    }))
    .into_response()
}

/// `POST /api/machines/{name}/test` -> `{"ok","latency_ms","whoami"|"error","machine"}`.
pub(super) async fn test_machine(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let Some(machine) = app.machines.get(&name) else {
        return error_response(StatusCode::NOT_FOUND, &format!("no machine named {name}"));
    };
    let started = Instant::now();
    let outcome = machine.whoami().await;
    let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let body = match outcome {
        Ok(who) => {
            json!({ "ok": true, "latency_ms": latency, "whoami": who, "machine": machine.view() })
        }
        Err(e) => json!({ "ok": false, "error": e.to_string(), "machine": machine.view() }),
    };
    Json(body).into_response()
}

// --- the proxy -------------------------------------------------------------------

fn no_machine(name: &str) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        &format!("no machine named {name} on this hub (fw hub machine ls)"),
    )
}

/// `/m/{machine}/api/...`, any method; WebSocket upgrades too.
pub(super) async fn forward(
    State(app): State<AppState>,
    client: Client,
    request: Request,
) -> Response {
    let path = request.uri().path().to_owned();
    let Some((name, rest)) = path
        .strip_prefix("/m/")
        .and_then(|p| p.split_once('/'))
        .map(|(name, rest)| (name.to_owned(), format!("/{rest}")))
    else {
        return no_machine(path.trim_start_matches("/m/"));
    };
    let Some(machine) = app.machines.get(&name) else {
        return no_machine(&name);
    };
    let path_and_query = match request.uri().query() {
        Some(query) => format!("{rest}?{query}"),
        None => rest,
    };
    if let Err(e) = machine.target(&path_and_query) {
        return link_error(&name, &e);
    }
    let upgrade = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if upgrade {
        return forward_socket(app, client, machine, &path_and_query, request).await;
    }
    if let Some(held) = machine.held_refusal() {
        return link_error(&name, &held);
    }
    let method = request.method().clone();
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = match axum::body::to_bytes(request.into_body(), MAX_FORWARD_BODY).await {
        Ok(body) => body,
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large to forward",
            );
        }
    };
    let method = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::METHOD_NOT_ALLOWED, "unsupported method"),
    };
    let body = (!body.is_empty() || content_type.is_some()).then(|| {
        (
            body.to_vec(),
            content_type.unwrap_or_else(|| "application/octet-stream".into()),
        )
    });
    let response = match machine
        .send(method, &path_and_query, body, CALL_TIMEOUT)
        .await
    {
        Ok(response) => response,
        Err(e) => return link_error(&name, &e),
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .cloned();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            return link_error(
                &name,
                &LinkError::Unreachable(format!("machine {name}: reading its answer failed: {e}")),
            );
        }
    };
    let mut out = (status, Bytes::from(bytes.to_vec())).into_response();
    let headers = out.headers_mut();
    // Only what the API's clients read: never cookies or redirects.
    if let Some(value) = content_type.and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok()) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Some(value) = retry_after.and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok()) {
        headers.insert(header::RETRY_AFTER, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // A 3xx would be followed by a browser to wherever it points: the
    // machine's API never redirects, so none is passed on.
    if status.is_redirection() {
        return error_response(
            StatusCode::BAD_GATEWAY,
            &format!("machine {name} answered with a redirect: refused"),
        );
    }
    out
}

async fn forward_socket(
    app: AppState,
    client: Client,
    machine: Arc<Machine>,
    path_and_query: &str,
    request: Request,
) -> Response {
    if !origin_ok(request.headers(), client, &app.options) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin WebSocket refused");
    }
    let (mut parts, _) = request.into_parts();
    let credential = parts.extensions.get::<CredentialWatch>().cloned();
    let gate = parts.extensions.get::<crate::handoff::Gate>().cloned();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &app).await {
        Ok(ws) => ws,
        Err(rejection) => return upgrade_refused(&rejection),
    };
    let name = machine.name().to_owned();
    // The machine first: its refusal (a gone session, a revoked
    // credential) becomes this handshake's answer.
    let upstream = match machine.websocket(path_and_query).await {
        Ok(socket) => socket,
        Err(Ok((status, body))) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            return (
                status,
                Json(json!({ "error": error_text(&body), "machine": name })),
            )
                .into_response();
        }
        Err(Err(e)) => return link_error(&name, &e),
    };
    let credential = credential.unwrap_or_else(|| CredentialWatch::new(&app.hub));
    let hub = Arc::clone(&app.hub);
    let shutdown = app.shutdown.clone();
    let machines = Arc::clone(&app.machines);
    let epoch = machine.epoch();
    ws.max_message_size(WS_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            relay(
                socket,
                upstream,
                Relay {
                    name,
                    hub,
                    shutdown,
                    credential,
                    gate,
                    machines,
                    epoch,
                },
            )
            .await;
        })
}

struct Relay {
    name: String,
    hub: Arc<crate::Hub>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    credential: CredentialWatch,
    gate: Option<crate::handoff::Gate>,
    machines: Arc<crate::machines::Machines>,
    epoch: u64,
}

/// How a relay ended, and what each side is told.
enum End {
    /// The browser closed or went away.
    Browser,
    /// The machine closed (its close frame) or the link broke (`None`).
    Machine(Option<(u16, String)>),
    /// This hub ends it (a credential, a removed machine, shutdown).
    Local(CloseFrame),
}

/// What the browser is told when the machine closed with `code`: the
/// machine's own credential codes (4001-4003) mean this hub's credential
/// there, never the browser's sign-in, so they become
/// [`WS_CLOSE_MACHINE_REFUSED`].
fn browser_close(name: &str, upstream: Option<(u16, String)>) -> CloseFrame {
    match upstream {
        Some((4001..=4003, _)) => CloseFrame {
            code: WS_CLOSE_MACHINE_REFUSED,
            reason: format!("machine {name} refused this hub's credential").into(),
        },
        Some((code, reason)) => CloseFrame {
            code,
            reason: reason.into(),
        },
        None => CloseFrame {
            code: WS_CLOSE_MACHINE_LOST,
            reason: format!("lost the connection to machine {name}").into(),
        },
    }
}

async fn machine_removed(machines: &crate::machines::Machines, epoch: u64) {
    let mut changes = machines.subscribe();
    loop {
        if !machines.alive(epoch) {
            return;
        }
        if changes.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn relay(browser: WebSocket, upstream: crate::endpoint::Socket, mut ctx: Relay) {
    let (mut to_browser, mut from_browser) = browser.split();
    let (mut to_machine, mut from_machine) = upstream.split();
    let mut recheck = login_recheck();
    let end = {
        // Machine -> browser, one frame at a time (each awaits the
        // browser's write: backpressure all the way to the machine).
        let down = async {
            let mut ping = tokio::time::interval(WS_PING_INTERVAL);
            ping.reset();
            loop {
                tokio::select! {
                    message = from_machine.next() => {
                        let message = match message {
                            Some(Ok(message)) => message,
                            Some(Err(_)) | None => return End::Machine(None),
                        };
                        let out = match message {
                            tungstenite::Message::Text(text) => Message::Text(text.as_str().into()),
                            tungstenite::Message::Binary(bytes) => Message::Binary(bytes),
                            tungstenite::Message::Close(frame) => {
                                return End::Machine(Some(frame.map_or((1000, String::new()), |f| {
                                    (u16::from(f.code), f.reason.as_str().to_owned())
                                })));
                            }
                            tungstenite::Message::Ping(_)
                            | tungstenite::Message::Pong(_)
                            | tungstenite::Message::Frame(_) => continue,
                        };
                        if to_browser.send(out).await.is_err() {
                            return End::Browser;
                        }
                    }
                    _ = ping.tick() => {
                        if to_browser.send(Message::Ping(Bytes::new())).await.is_err() {
                            return End::Browser;
                        }
                    }
                }
            }
        };
        // Browser -> machine.
        let up = async {
            loop {
                let message = match from_browser.next().await {
                    Some(Ok(message)) => message,
                    Some(Err(_)) | None => return End::Browser,
                };
                let out = match message {
                    Message::Text(text) => tungstenite::Message::Text(text.as_str().into()),
                    Message::Binary(bytes) => tungstenite::Message::Binary(bytes),
                    Message::Close(_) => return End::Browser,
                    Message::Ping(_) | Message::Pong(_) => continue,
                };
                if to_machine.send(out).await.is_err() {
                    return End::Machine(None);
                }
            }
        };
        let hub = Arc::clone(&ctx.hub);
        tokio::select! {
            end = down => end,
            end = up => end,
            close = ctx.credential.revoked(&hub, &mut recheck) => End::Local(close),
            () = machine_removed(&ctx.machines, ctx.epoch) => End::Local(CloseFrame {
                code: WS_CLOSE_MACHINE_REMOVED,
                reason: format!("machine {} was removed from this hub", ctx.name).into(),
            }),
            () = handoff_restarting(ctx.gate.as_ref()) => End::Local(CloseFrame {
                code: super::WS_CLOSE_RESTART,
                reason: "the hub is restarting (upgrade); reconnect".into(),
            }),
            () = async {
                loop {
                    if ctx.shutdown.changed().await.is_err() || *ctx.shutdown.borrow() {
                        return;
                    }
                }
            } => End::Local(CloseFrame { code: 1001, reason: "the hub is shutting down".into() }),
        }
    };
    let browser_frame = match end {
        End::Browser => None,
        End::Machine(upstream) => Some(browser_close(&ctx.name, upstream)),
        End::Local(frame) => Some(frame),
    };
    if let Some(frame) = browser_frame {
        let _ = to_browser.send(Message::Close(Some(frame))).await;
    }
    let _ = to_browser.flush().await;
    let _ = to_machine.send(tungstenite::Message::Close(None)).await;
    let _ = to_machine.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_credential_closes_never_sign_the_browser_out() {
        for code in [4001, 4002, 4003] {
            assert_eq!(
                browser_close("b", Some((code, String::new()))).code,
                WS_CLOSE_MACHINE_REFUSED
            );
        }
        assert_eq!(browser_close("b", Some((1012, String::new()))).code, 1012);
        assert_eq!(browser_close("b", None).code, WS_CLOSE_MACHINE_LOST);
    }
}
