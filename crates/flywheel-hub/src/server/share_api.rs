//! Read-only share links (see [`crate::share`]): `/api/shares` (mint, list,
//! revoke; the hub's token or a login), and what a share link reaches --
//! `/share/<credential>` (the viewer page), `/share/<credential>/info`
//! (the session's title and status), `/share/<credential>/ws` (its
//! terminal, read-only), and the viewer's own assets. Nothing else: any
//! other path under it is 403, and the credential presented anywhere else
//! is refused by `require_auth` (403).

use super::ApiQuery;
use super::terminal_ws::{
    Viewer, attach, close_for_relaunch, close_for_restart, handoff_restarting, relaunched,
};
use super::{
    ApiJson, ApiResult, AppState, Client, WS_CLOSE_SHARE_ENDED, WS_MAX_MESSAGE, WS_PING_INTERVAL,
    blocking, error_response, origin_ok, upgrade_refused,
};
use crate::hub::{Hub, HubError, HubEvent};
use crate::pty::{PtySession, SessionEvent};
use crate::share::{ShareError, ShareInfo, Verdict, parse_ttl};
use crate::state::StateError;
use crate::vt::{SNAPSHOT_HISTORY_BYTES, TextSource};
use crate::web;
use axum::Json;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use futures_util::SinkExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, watch};

impl From<ShareError> for HubError {
    fn from(e: ShareError) -> Self {
        match e {
            ShareError::Invalid(m) => StateError::Invalid(m).into(),
            ShareError::NotFound(m) => StateError::NotFound(m).into(),
            ShareError::Conflict(m) => StateError::Conflict(m).into(),
            e @ (ShareError::Io { .. } | ShareError::Token(_)) => HubError::Internal(e.to_string()),
        }
    }
}

// --- /api/shares ------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewShare {
    /// Session id (or a unique prefix / name, as the CLI resolves it
    /// first).
    session_id: String,
    /// Seconds it lasts.
    expires_in: Option<u64>,
    /// Or as a duration (`30m`, `1h`, `2d`).
    expires: Option<String>,
    name: Option<String>,
}

/// A share as the API lists it: plus its session's name.
fn share_view(hub: &Hub, info: &ShareInfo, viewers: usize, last_used: Option<u64>) -> Value {
    let session = hub.session(&info.session_id).ok();
    json!({
        "id": info.id,
        "session_id": info.session_id,
        "session": session.as_ref().map(|s| s.record.name.clone()),
        "name": info.name,
        "created": info.created,
        "expires": info.expires,
        "viewers": viewers,
        "last_used": last_used,
    })
}

pub(super) async fn create(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<NewShare>,
) -> ApiResult<Response> {
    let asked = match (body.expires_in, body.expires.as_deref()) {
        (Some(_), Some(_)) => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "give expires_in or expires, not both",
            ));
        }
        (Some(seconds), None) => Some(Duration::from_secs(seconds)),
        (None, Some(text)) => match parse_ttl(text) {
            Ok(ttl) => Some(ttl),
            Err(message) => return Ok(error_response(StatusCode::BAD_REQUEST, &message)),
        },
        (None, None) => None,
    };
    // hub.toml `[shares]`: the default and the longest allowed.
    let ttl = match app.hub.tunables().share_policy.expiry(asked) {
        Ok(ttl) => ttl,
        Err(message) => return Ok(error_response(StatusCode::BAD_REQUEST, &message)),
    };
    let hub = Arc::clone(&app.hub);
    let (view, info, credential) = blocking(move || {
        let view = hub.session(&body.session_id)?;
        let (info, credential) = hub
            .shares()
            .issue(&view.record.id, ttl, body.name.as_deref())?;
        Ok((view, info, credential))
    })
    .await?;
    // Plugins hear of it without the credential or any link to it.
    app.hub.emit_lifecycle(
        "share.created",
        json!({
            "session": app.hub.event_session(&info.session_id),
            "share": share_event(&info),
        }),
    );
    let path = format!("{}{credential}", crate::share::SHARE_PATH);
    let mut response = (
        StatusCode::CREATED,
        Json(json!({
            "share": share_view(&app.hub, &info, 0, None),
            "session": view.record.name,
            "credential": credential,
            "path": path,
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// A share as plugin events carry it (see [`ShareInfo::event`]).
fn share_event(info: &ShareInfo) -> Value {
    info.event()
}

/// `share.revoked` for each of `revoked`.
fn announce_revoked(hub: &Hub, revoked: &[ShareInfo]) {
    for info in revoked {
        hub.emit_lifecycle(
            "share.revoked",
            json!({
                "session": hub.event_session(&info.session_id),
                "share": share_event(info),
            }),
        );
    }
}

pub(super) async fn list(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let hub = Arc::clone(&app.hub);
    let shares = blocking(move || {
        let listed = hub.shares().list()?;
        Ok(listed
            .iter()
            .map(|s| share_view(&hub, &s.info, s.viewers, s.last_used))
            .collect::<Vec<_>>())
    })
    .await?;
    let policy = app.hub.tunables().share_policy;
    Ok(Json(json!({
        "shares": shares,
        "policy": {
            "default_expires_in": policy.default_ttl.as_secs(),
            "max_expires_in": policy.max_ttl.as_secs(),
        },
    })))
}

pub(super) async fn revoke(
    State(app): State<AppState>,
    Path(which): Path<String>,
) -> ApiResult<Json<Value>> {
    let hub = Arc::clone(&app.hub);
    let revoked = blocking(move || Ok(hub.shares().revoke(&which)?)).await?;
    announce_revoked(&app.hub, &revoked);
    Ok(Json(json!({ "revoked": revoked })))
}

#[derive(Deserialize)]
pub(super) struct RevokeAll {
    /// Only this session's shares.
    session: Option<String>,
}

pub(super) async fn revoke_all(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<RevokeAll>,
) -> ApiResult<Json<Value>> {
    let hub = Arc::clone(&app.hub);
    let revoked = blocking(move || {
        let session = match &query.session {
            Some(s) => Some(hub.session(s)?.record.id),
            None => None,
        };
        Ok(hub.shares().revoke_all(session.as_deref())?)
    })
    .await?;
    announce_revoked(&app.hub, &revoked);
    Ok(Json(json!({ "revoked": revoked })))
}

// --- what a share link reaches ----------------------------------------------

/// A share link's refusal, as a page (the viewer's URL) or as JSON.
fn refused(status: StatusCode, message: &str, page: bool) -> Response {
    let mut response = if page {
        let mut response = Html(web::share_refused_page(message)).into_response();
        *response.status_mut() = status;
        response
    } else {
        error_response(status, message)
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Check the credential in a share path: the live share, or the answer.
/// A wrong credential counts against the client like a wrong token
/// (expired ones do not: their secret matched); a locked-out client is
/// refused unchecked.
fn share_auth(
    app: &AppState,
    client: Client,
    credential: &str,
    page: bool,
) -> Result<ShareInfo, Box<Response>> {
    let now = Instant::now();
    if !client.same_machine {
        let mut failures = app.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(wait) = failures.locked(client.ip, now) {
            let mut response = refused(
                StatusCode::TOO_MANY_REQUESTS,
                "too many failed attempts from this address; try again later",
                page,
            );
            if let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            return Err(Box::new(response));
        }
    }
    match app.hub.shares().verify(credential) {
        Verdict::Live(info) => Ok(info),
        Verdict::Expired => Err(Box::new(refused(
            StatusCode::GONE,
            "This share link has expired.",
            page,
        ))),
        Verdict::Unknown => {
            if !client.same_machine {
                app.failures
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .record_failure(client.ip, now);
            }
            Err(Box::new(refused(
                StatusCode::NOT_FOUND,
                "This share link is not valid (it may have been revoked).",
                page,
            )))
        }
    }
}

pub(super) async fn viewer_page(
    State(app): State<AppState>,
    client: Client,
    Path(credential): Path<String>,
) -> Response {
    if let Err(response) = share_auth(&app, client, &credential, true) {
        return *response;
    }
    let mut response = Html(web::viewer_page(&credential)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The viewer page's scripts and styles (and nothing else the app has).
pub(super) async fn viewer_asset(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
    Path((credential, path)): Path<(String, String)>,
) -> Response {
    if let Err(response) = share_auth(&app, client, &credential, false) {
        return *response;
    }
    if !web::is_viewer_asset(&path) {
        return error_response(StatusCode::FORBIDDEN, "not reachable with a share link");
    }
    match web::asset_entry(&path) {
        Some(asset) => super::auth::asset_response(&headers, None, &asset),
        None => error_response(StatusCode::NOT_FOUND, "no such asset"),
    }
}

/// Anything else under a share path: a valid link reaches nothing more.
pub(super) async fn elsewhere(
    State(app): State<AppState>,
    client: Client,
    Path((credential, _rest)): Path<(String, String)>,
) -> Response {
    match share_auth(&app, client, &credential, false) {
        Ok(_) => error_response(
            StatusCode::FORBIDDEN,
            "a share link reaches only its read-only view",
        ),
        Err(response) => *response,
    }
}

fn unix_now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// The shared session's title and status: `{"share":{"name","expires",
/// "now"},"session":{...}|null}` (`null`: the session was removed).
pub(super) async fn info(
    State(app): State<AppState>,
    client: Client,
    Path(credential): Path<String>,
) -> Response {
    let share = match share_auth(&app, client, &credential, false) {
        Ok(share) => share,
        Err(response) => return *response,
    };
    let hub = Arc::clone(&app.hub);
    let id = share.session_id.clone();
    let session = tokio::task::spawn_blocking(move || {
        let view = hub.session(&id).ok()?;
        let workspace = hub
            .workspace(&view.record.workspace_id)
            .map(|w| w.name)
            .unwrap_or_default();
        let live = hub.live_session(&id);
        let title = live
            .as_ref()
            .and_then(|s| s.screen_text(TextSource::Visible).title);
        let record = serde_json::to_value(&view.record).unwrap_or(Value::Null);
        Some(json!({
            "name": view.record.name,
            "workspace": workspace,
            "kind": view.record.kind,
            "state": record["state"],
            "code": record["code"],
            "signal": record["signal"],
            "title": title,
            "agent": view.agent,
            "live": live.is_some(),
            "cols": view.live.as_ref().map(|l| l.cols),
            "rows": view.live.as_ref().map(|l| l.rows),
        }))
    })
    .await
    .ok()
    .flatten();
    let mut response = Json(json!({
        "share": { "name": share.name, "expires": share.expires, "now": unix_now_f64() },
        "session": session,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) async fn viewer_ws(
    State(app): State<AppState>,
    Path(credential): Path<String>,
    client: Client,
    headers: HeaderMap,
    gate: Option<axum::Extension<crate::handoff::Gate>>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    // Subscribed before the check: a revocation racing it still closes
    // the socket.
    let changes = app.hub.shares().subscribe();
    let share = match share_auth(&app, client, &credential, false) {
        Ok(share) => share,
        Err(response) => return *response,
    };
    if !origin_ok(&headers, client, &app.options) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin WebSocket refused");
    }
    let Some(session) = app.hub.live_session(&share.session_id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "the shared session has no terminal on this hub",
        );
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return upgrade_refused(&rejection),
    };
    let Some(hold) = app.hub.shares().hold_viewer(&share) else {
        return error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "this share link has too many viewers open; close one and retry",
        );
    };
    let gate = gate.map(|axum::Extension(gate)| gate);
    let shutdown = app.shutdown.clone();
    let hub = Arc::clone(&app.hub);
    ws.max_message_size(WS_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            hub.emit(HubEvent::Sessions);
            read_only_pump(socket, &session, &share, changes, shutdown, gate, &hub).await;
            drop(hold);
            drop(session);
            hub.emit(HubEvent::Sessions);
        })
}

/// When `share` expires, as an instant.
fn expiry(share: &ShareInfo) -> tokio::time::Instant {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let left = Duration::from_secs(share.expires).saturating_sub(now);
    tokio::time::Instant::now() + left
}

fn ended(reason: &str) -> CloseFrame {
    CloseFrame {
        code: WS_CLOSE_SHARE_ENDED,
        reason: reason.to_owned().into(),
    }
}

/// Resolves once share `id` is no longer live (revoked here or in the
/// file); never while it is. Cancel-safe.
async fn revoked(changes: &mut watch::Receiver<u64>, hub: &Hub, id: &str) {
    loop {
        if changes.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if !hub.shares().is_live(id) {
            return;
        }
    }
}

/// What a read-only viewer is told about anything it sends.
const READ_ONLY: &str = "read-only view: input, resize, and paste are refused";

/// The share link's terminal stream: the snapshot, then output, size
/// changes, and the exit -- never input, resizes, or theme reports (each
/// answered with an error), and never counted as viewing.
async fn read_only_pump(
    socket: WebSocket,
    session: &Arc<PtySession>,
    share: &ShareInfo,
    mut changes: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
    gate: Option<crate::handoff::Gate>,
    hub: &Hub,
) {
    let mut viewer = Viewer::new(socket, false);
    let Some(attachment) = attach(session, SNAPSHOT_HISTORY_BYTES).await else {
        return;
    };
    let Some(mut events) = viewer.snapshot(attachment, false, None).await else {
        return;
    };
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.reset();
    let expires = expiry(share);
    let mut relaunches = None;
    loop {
        tokio::select! {
            event = events.recv() => {
                let sent = match event {
                    Ok(SessionEvent::Output(bytes)) => viewer.binary(bytes).await,
                    Ok(SessionEvent::Exit(info)) => viewer.exit(&info).await,
                    Ok(SessionEvent::Resize { cols, rows }) => {
                        viewer.text(&json!({ "type": "size", "cols": cols, "rows": rows })).await
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(fresh) = viewer.resnapshot(session, events).await else {
                            break;
                        };
                        events = fresh;
                        true
                    }
                    Err(broadcast::error::RecvError::Closed) => false,
                };
                if !sent {
                    break;
                }
            }
            message = viewer.socket.recv() => {
                match message {
                    Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                        if !viewer.text(&json!({ "type": "error", "message": READ_ONLY })).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        let _ = viewer.socket.flush().await;
                        break;
                    }
                    Some(Err(_)) | None => break,
                }
            }
            _ = ping.tick() => {
                if !viewer.send(Message::Ping(Default::default())).await {
                    break;
                }
            }
            () = revoked(&mut changes, hub, &share.id) => {
                viewer.send(Message::Close(Some(ended("share link revoked")))).await;
                break;
            }
            () = tokio::time::sleep_until(expires) => {
                viewer.send(Message::Close(Some(ended("share link expired")))).await;
                break;
            }
            () = relaunched(&mut relaunches, hub, &share.session_id, session), if viewer.exited() => {
                close_for_relaunch(&mut viewer).await;
                break;
            }
            () = handoff_restarting(gate.as_ref()) => {
                close_for_restart(&mut viewer.socket).await;
                break;
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    viewer.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    }
}
