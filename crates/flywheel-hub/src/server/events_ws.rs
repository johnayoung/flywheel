//! The hub-wide events WebSocket (`/api/events`): pushed agent status,
//! list changes, and suggested commands; what each page shows, for
//! seen-tracking.

use super::commands_api;
use super::flywheel_api;
use super::terminal_ws::{close_for_restart, handoff_restarting};
use super::{
    AppState, Client, CredentialWatch, WS_PING_INTERVAL, error_response, login_recheck, origin_ok,
    upgrade_refused,
};
use crate::hub::{Hub, HubEvent};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::SinkExt;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    client: Option<String>,
}

/// A client id: 1..=64 of `[A-Za-z0-9_-]`.
fn valid_client_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(super) async fn events_ws(
    State(app): State<AppState>,
    Query(query): Query<EventsQuery>,
    client: Client,
    headers: HeaderMap,
    credential: Option<axum::Extension<CredentialWatch>>,
    gate: Option<axum::Extension<crate::handoff::Gate>>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if !origin_ok(&headers, client, &app.options) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin WebSocket refused");
    }
    let gate = gate.map(|axum::Extension(gate)| gate);
    let client = match query.client {
        Some(id) if valid_client_id(&id) => id,
        Some(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "client must be 1-64 letters, digits, '-' or '_'",
            );
        }
        None => crate::state::new_id(),
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return upgrade_refused(&rejection),
    };
    let hub = Arc::clone(&app.hub);
    let shutdown = app.shutdown.clone();
    let flywheel = app.options.flywheel.as_ref().map(|m| m.subscribe());
    // Taken before the credentials were checked (see `require_auth`).
    let credential = match credential {
        Some(axum::Extension(watch)) => watch,
        None => CredentialWatch::new(&app.hub),
    };
    ws.max_message_size(64 * 1024).on_upgrade(move |socket| {
        events_pump(socket, hub, client, shutdown, credential, gate, flywheel)
    })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventsClientMessage {
    /// `session_id`: the focused pane's session; `visible`: the other
    /// panes on screen (a split layout), which count as viewed too.
    View {
        session_id: Option<String>,
        #[serde(default)]
        visible: Vec<String>,
    },
}

/// Unregisters a viewer however the pump ends.
struct ViewerGuard {
    hub: Arc<Hub>,
    viewer: u64,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.hub.disconnect_viewer(self.viewer);
    }
}

pub(super) fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

fn snapshot_message(hub: &Hub, client: &str) -> Message {
    let snapshot = hub.status_snapshot(client);
    text(json!({
        "type": "snapshot",
        "sessions": snapshot.sessions,
        "workspaces": snapshot.workspaces,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

fn status_message(hub: &Hub, client: &str, session_id: &str) -> Message {
    match hub.status_for(client, session_id) {
        Some((view, workspace_id, rollup)) => text(json!({
            "type": "status",
            "session_id": session_id,
            "status": view,
            "workspace_id": workspace_id,
            "workspace_status": rollup,
        })),
        None => text(json!({
            "type": "status",
            "session_id": session_id,
            "status": null,
        })),
    }
}

async fn events_pump(
    mut socket: WebSocket,
    hub: Arc<Hub>,
    client: String,
    mut shutdown: watch::Receiver<bool>,
    mut credential: CredentialWatch,
    gate: Option<crate::handoff::Gate>,
    mut flywheel: Option<broadcast::Receiver<crate::flywheel::FlywheelEvent>>,
) {
    let mut events = hub.subscribe();
    let guard = ViewerGuard {
        viewer: hub.connect_viewer(&client),
        hub: Arc::clone(&hub),
    };
    if socket.send(snapshot_message(&hub, &client)).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.reset();
    let mut recheck = login_recheck();
    loop {
        tokio::select! {
            event = events.recv() => {
                let sent = match event {
                    Ok(HubEvent::Status { session_id }) => {
                        socket.send(status_message(&hub, &client, &session_id)).await.is_ok()
                    }
                    Ok(HubEvent::Sessions) => {
                        socket.send(text(json!({ "type": "sessions" }))).await.is_ok()
                    }
                    Ok(HubEvent::Commands { session_id, added }) => {
                        socket.send(commands_api::event_message(&session_id, added)).await.is_ok()
                    }
                    Ok(HubEvent::Unwatched { .. }) => true,
                    Ok(HubEvent::Usage { session_id }) => {
                        let usage = hub.session(&session_id).ok().and_then(|v| v.usage);
                        socket
                            .send(text(json!({ "type": "usage", "session_id": session_id, "usage": usage })))
                            .await
                            .is_ok()
                    }
                    Ok(HubEvent::Layout { workspace_id, rev }) => {
                        socket.send(text(json!({ "type": "layout", "workspace_id": workspace_id, "rev": rev }))).await.is_ok()
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        socket.send(snapshot_message(&hub, &client)).await.is_ok()
                            && socket.send(text(json!({ "type": "sessions" }))).await.is_ok()
                    }
                    Err(broadcast::error::RecvError::Closed) => false,
                };
                if !sent {
                    break;
                }
            }
            event = flywheel_api::next_event(&mut flywheel) => {
                if socket.send(flywheel_api::event_message(event)).await.is_err() {
                    break;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(body))) => {
                        match serde_json::from_str::<EventsClientMessage>(body.as_str()) {
                            Ok(EventsClientMessage::View { session_id, visible }) => {
                                let ids: Vec<&str> = session_id
                                    .iter()
                                    .chain(visible.iter())
                                    .map(String::as_str)
                                    .collect();
                                hub.set_viewing_many(guard.viewer, &ids);
                            }
                            Err(e) => {
                                let error = json!({ "type": "error", "message": format!("bad message: {e}") });
                                if socket.send(text(error)).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        let _ = socket.flush().await;
                        break;
                    }
                    Some(Err(_)) | None => break,
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            close = credential.revoked(&hub, &mut recheck) => {
                let _ = socket.send(Message::Close(Some(close))).await;
                break;
            }
            () = handoff_restarting(gate.as_ref()) => {
                close_for_restart(&mut socket).await;
                break;
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    }
    drop(guard);
}
