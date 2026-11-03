//! A session's terminal WebSocket (`/api/sessions/{id}/ws`): the replay
//! on attach, then output, input, resizes, and theme reports both ways.

use super::{
    AppState, Client, CredentialWatch, LAGGED_SNAPSHOT_HISTORY, SNAPSHOT_FRAME, WS_CLOSE_RESTART,
    WS_MAX_MESSAGE, WS_PING_INTERVAL, error_response, login_recheck, origin_ok, upgrade_refused,
};
use crate::hub::{Hub, HubEvent};
use crate::pty::{Attachment, ExitInfo, PtySession, SessionEvent, WriteOutcome};
use crate::vt::{SNAPSHOT_HISTORY_BYTES, ViewerColors};
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::SinkExt;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};

pub(super) async fn session_ws(
    State(app): State<AppState>,
    Path(id): Path<String>,
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
    let Some(session) = app.hub.live_session(&id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("session {id} has no terminal on this server"),
        );
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return upgrade_refused(&rejection),
    };
    let shutdown = app.shutdown.clone();
    // Taken before the credentials were checked (see `require_auth`).
    let credential = match credential {
        Some(axum::Extension(watch)) => watch,
        None => CredentialWatch::new(&app.hub),
    };
    let hub = Arc::clone(&app.hub);
    ws.max_message_size(WS_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            pump(
                socket,
                Arc::clone(&session),
                shutdown,
                credential,
                gate,
                &hub,
            )
            .await;
            // One viewer fewer: panes that follow another's size may now be
            // the only viewer and fit themselves (re-list for `live.clients`).
            drop(session);
            hub.emit(HubEvent::Sessions);
        })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Input {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Theme {
        foreground: String,
        background: String,
        cursor: String,
        ansi: Vec<String>,
    },
}

pub(super) fn exit_message(info: &ExitInfo) -> Message {
    Message::Text(
        json!({ "type": "exit", "code": info.code, "signal": info.signal })
            .to_string()
            .into(),
    )
}

/// Snapshot a session (and subscribe) off the async workers.
pub(super) async fn attach(session: &Arc<PtySession>, history_budget: usize) -> Option<Attachment> {
    let session = Arc::clone(session);
    tokio::task::spawn_blocking(move || session.attach_with(history_budget))
        .await
        .ok()
}

/// Send an attachment's snapshot (and exit, if already exited), returning
/// its live receiver. The snapshot is released once sent.
pub(super) async fn send_attachment(
    socket: &mut WebSocket,
    attachment: Attachment,
) -> Option<broadcast::Receiver<SessionEvent>> {
    let Attachment {
        replay,
        cols,
        rows,
        alt_screen,
        exit,
        events,
    } = attachment;
    let header = json!({
        "type": "snapshot",
        "cols": cols,
        "rows": rows,
        "alt_screen": alt_screen,
        "bytes": replay.len(),
    });
    socket
        .send(Message::Text(header.to_string().into()))
        .await
        .ok()?;
    let replay = Bytes::from(replay);
    let mut start = 0;
    while start < replay.len() {
        let end = replay.len().min(start + SNAPSHOT_FRAME);
        socket
            .send(Message::Binary(replay.slice(start..end)))
            .await
            .ok()?;
        start = end;
    }
    drop(replay);
    if let Some(info) = &exit {
        socket.send(exit_message(info)).await.ok()?;
    }
    Some(events)
}

/// A terminal focus-in or focus-out report, alone.
fn is_focus_report(bytes: &[u8]) -> bool {
    bytes == b"\x1b[I" || bytes == b"\x1b[O"
}

/// Queue client input; tell the client when it was dropped because the
/// terminal is not reading. Returns false when the socket is gone.
async fn forward_input(socket: &mut WebSocket, session: &PtySession, bytes: Vec<u8>) -> bool {
    // A browser's focus report (sent once a program asked for them) that
    // reaches a terminal still in cooked mode -- an agent TUI enables
    // focus reports before it takes the terminal raw -- would only be
    // echoed onto the screen as `^[[I`.
    if is_focus_report(&bytes) && session.echoes_input() {
        return true;
    }
    match session.write(bytes) {
        WriteOutcome::Queued | WriteOutcome::Closed => true,
        outcome @ (WriteOutcome::Backlogged | WriteOutcome::HandingOff) => {
            let message = if outcome == WriteOutcome::HandingOff {
                "input dropped: the hub is handing over to a new process; type it again once reconnected"
            } else {
                "input dropped: the terminal is not reading its input"
            };
            let error = json!({
                "type": "error",
                "message": message,
            });
            socket
                .send(Message::Text(error.to_string().into()))
                .await
                .is_ok()
        }
    }
}

async fn pump(
    mut socket: WebSocket,
    session: Arc<PtySession>,
    mut shutdown: watch::Receiver<bool>,
    mut credential: CredentialWatch,
    gate: Option<crate::handoff::Gate>,
    hub: &Hub,
) {
    let Some(attachment) = attach(&session, SNAPSHOT_HISTORY_BYTES).await else {
        return;
    };
    let Some(mut events) = send_attachment(&mut socket, attachment).await else {
        return;
    };
    // One viewer more (`live.clients` in session listings).
    hub.emit(HubEvent::Sessions);
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.reset();
    let mut recheck = login_recheck();
    loop {
        tokio::select! {
            event = events.recv() => {
                let sent = match event {
                    Ok(SessionEvent::Output(bytes)) => socket.send(Message::Binary(bytes)).await.is_ok(),
                    Ok(SessionEvent::Exit(info)) => socket.send(exit_message(&info)).await.is_ok(),
                    Ok(SessionEvent::Resize { cols, rows }) => {
                        let size = json!({ "type": "size", "cols": cols, "rows": rows });
                        socket.send(Message::Text(size.to_string().into())).await.is_ok()
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Unsubscribe first, so the backlog this client
                        // will never read is not kept alive for it.
                        drop(events);
                        let reset = Message::Text(json!({ "type": "reset" }).to_string().into());
                        if socket.send(reset).await.is_err() {
                            break;
                        }
                        let Some(fresh) = attach(&session, LAGGED_SNAPSHOT_HISTORY).await else {
                            break;
                        };
                        let Some(fresh) = send_attachment(&mut socket, fresh).await else {
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
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(text.as_str()) {
                            Ok(ClientMessage::Input { data }) => {
                                if !forward_input(&mut socket, &session, data.into_bytes()).await {
                                    break;
                                }
                            }
                            Ok(ClientMessage::Resize { cols, rows }) => {
                                let _ = session.resize(cols, rows);
                            }
                            Ok(ClientMessage::Theme { foreground, background, cursor, ansi }) => {
                                match ViewerColors::from_hex(&foreground, &background, &cursor, &ansi) {
                                    Ok(colors) => session.set_viewer_colors(colors),
                                    Err(e) => {
                                        let error = json!({ "type": "error", "message": format!("bad theme: {e}") });
                                        if socket.send(Message::Text(error.to_string().into())).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let error = json!({ "type": "error", "message": format!("bad message: {e}") });
                                if socket.send(Message::Text(error.to_string().into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if !forward_input(&mut socket, &session, bytes.to_vec()).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        // Complete the closing handshake: tungstenite queued
                        // the reply Close on receipt but only writes it on a
                        // flush (a send would be refused after closing), so
                        // the client sees a clean close instead of 1006.
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
            close = credential.revoked(hub, &mut recheck) => {
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
}

/// Resolves once a live handoff froze the hub (never without a gate).
pub(super) async fn handoff_restarting(gate: Option<&crate::handoff::Gate>) {
    match gate {
        Some(gate) => gate.restarting().await,
        None => std::future::pending().await,
    }
}

/// Close a WebSocket for a live handoff: clients reconnect to whichever
/// process serves next.
pub(super) async fn close_for_restart(socket: &mut WebSocket) {
    let close = CloseFrame {
        code: WS_CLOSE_RESTART,
        reason: "the hub is restarting (upgrade); reconnect".into(),
    };
    let _ = socket.send(Message::Close(Some(close))).await;
    let _ = socket.flush().await;
}
