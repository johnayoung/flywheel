//! The hub-wide events WebSocket (`/api/events`): pushed agent status,
//! list changes, and suggested commands; what each page shows, for
//! seen-tracking.

use super::ApiQuery;
use super::commands_api;
use super::flywheel_api;
use super::terminal_ws::{close_for_restart, handoff_restarting};
use super::{
    AppState, Client, CredentialWatch, WS_PING_INTERVAL, error_response, login_recheck, origin_ok,
    upgrade_refused, write_within,
};
use crate::hub::{Hub, HubEvent};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::SinkExt;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

/// Sleep until `at`; never, without one.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

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
    ApiQuery(query): ApiQuery<EventsQuery>,
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
    let ends = Ends {
        shutdown,
        credential,
        gate,
        limit: app.options.send_timeout(),
    };
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| events_pump(socket, hub, client, ends, flywheel))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventsClientMessage {
    /// `session_id`: the focused pane's session; `visible`: the other
    /// panes on screen (a split layout), which count as viewed too.
    /// `present`: someone is at the page (visible, focused, touched
    /// lately); absent, it is present while it shows a session.
    View {
        session_id: Option<String>,
        #[serde(default)]
        visible: Vec<String>,
        #[serde(default)]
        present: Option<bool>,
    },
}

/// Unregisters a viewer (and its presence) however the pump ends.
struct ViewerGuard {
    hub: Arc<Hub>,
    viewer: u64,
    client: String,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.hub.disconnect_viewer(self.viewer);
        self.hub.activity().disconnect(&self.client, self.viewer);
    }
}

/// The `away` message: this client is back after a while; its digest
/// (`GET /api/activity/digest?since=&until=`) covers the span.
fn away_message(back: crate::activity::Return) -> Message {
    text(json!({
        "type": "away",
        "since": back.since,
        "until": back.until,
    }))
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
        "graded": snapshot.graded,
        "fanouts": snapshot.fanouts,
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

/// Least time between two `sessions` pushes to one client: each makes it
/// re-list every workspace and session, and changes come in bursts (a
/// fan-out's launch, badges after a turn, an import). One that comes
/// sooner is sent when the interval is up (never dropped).
pub const SESSIONS_PUSH_INTERVAL: Duration = Duration::from_millis(250);
/// Most queued events read in one batch.
const BATCH: usize = 512;

/// What one batch of hub events comes to for one client: the events
/// queued together, with repeats folded. A `status`, `usage`, or `fanout`
/// push carries (or makes the client fetch) the current state, so one per
/// session or fan-out says all that repeats would; `sessions` is a re-list
/// signal, sent once (and paced, see [`SESSIONS_PUSH_INTERVAL`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Batch {
    /// In order, repeats folded into their first place.
    pub(crate) events: Vec<HubEvent>,
    /// Some event in it said to re-list.
    pub(crate) sessions: bool,
    /// The queue overflowed: what was lost is unknown.
    pub(crate) lagged: bool,
}

impl Batch {
    pub(crate) fn push(&mut self, event: HubEvent) {
        let repeat = match &event {
            HubEvent::Sessions => {
                self.sessions = true;
                return;
            }
            // Nothing for clients.
            HubEvent::Unwatched { .. }
            | HubEvent::Lifecycle { .. }
            | HubEvent::TurnFinished { .. }
            | HubEvent::PluginRan { .. } => return,
            HubEvent::Status { session_id } => self
                .events
                .iter()
                .any(|e| matches!(e, HubEvent::Status { session_id: s } if s == session_id)),
            HubEvent::Usage { session_id } => self
                .events
                .iter()
                .any(|e| matches!(e, HubEvent::Usage { session_id: s } if s == session_id)),
            HubEvent::Fanout { id } => self
                .events
                .iter()
                .any(|e| matches!(e, HubEvent::Fanout { id: f } if f == id)),
            HubEvent::Plugins => self.events.contains(&HubEvent::Plugins),
            _ => false,
        };
        if !repeat {
            self.events.push(event);
        }
    }

    /// The first event and whatever is queued behind it (at most
    /// [`BATCH`]).
    pub(crate) fn drain(
        first: Result<HubEvent, broadcast::error::RecvError>,
        events: &mut broadcast::Receiver<HubEvent>,
    ) -> Result<Self, ()> {
        let mut batch = Self::default();
        match first {
            Ok(event) => batch.push(event),
            Err(broadcast::error::RecvError::Lagged(_)) => batch.lagged = true,
            Err(broadcast::error::RecvError::Closed) => return Err(()),
        }
        for _ in 1..BATCH {
            match events.try_recv() {
                Ok(event) => batch.push(event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => batch.lagged = true,
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => return Err(()),
            }
        }
        Ok(batch)
    }
}

/// The message for one (folded) event; `None`: nothing to send.
fn event_message(hub: &Hub, client: &str, event: HubEvent) -> Option<Message> {
    Some(match event {
        HubEvent::Status { session_id } => status_message(hub, client, &session_id),
        HubEvent::Commands { session_id, added } => commands_api::event_message(&session_id, added),
        HubEvent::Usage { session_id } => {
            let usage = hub.session(&session_id).ok().and_then(|v| v.usage);
            text(json!({ "type": "usage", "session_id": session_id, "usage": usage }))
        }
        HubEvent::Fanout { id } => text(json!({ "type": "fanout", "id": id })),
        HubEvent::Settings { rev } => text(json!({ "type": "settings", "rev": rev })),
        HubEvent::Config { rev } => text(json!({ "type": "config", "rev": rev })),
        HubEvent::Plugins => text(json!({ "type": "plugins" })),
        HubEvent::PluginToast {
            plugin,
            message,
            tone,
        } => text(
            json!({ "type": "plugin_toast", "plugin": plugin, "message": message, "tone": tone }),
        ),
        HubEvent::Layout { workspace_id, rev } => {
            text(json!({ "type": "layout", "workspace_id": workspace_id, "rev": rev }))
        }
        HubEvent::Activity { row } => text(json!({ "type": "activity", "row": row })),
        HubEvent::Digest { id } => text(json!({ "type": "digest", "id": id })),
        HubEvent::Sessions
        | HubEvent::Unwatched { .. }
        | HubEvent::Lifecycle { .. }
        | HubEvent::TurnFinished { .. }
        | HubEvent::PluginRan { .. } => {
            return None;
        }
    })
}

fn sessions_message() -> Message {
    text(json!({ "type": "sessions" }))
}

/// What ends an events socket besides its client, and how long one write
/// to it may take (a reader that stopped reading is dropped after it).
struct Ends {
    shutdown: watch::Receiver<bool>,
    credential: CredentialWatch,
    gate: Option<crate::handoff::Gate>,
    limit: Duration,
}

async fn events_pump(
    mut socket: WebSocket,
    hub: Arc<Hub>,
    client: String,
    ends: Ends,
    mut flywheel: Option<broadcast::Receiver<crate::flywheel::FlywheelEvent>>,
) {
    let Ends {
        mut shutdown,
        mut credential,
        gate,
        limit,
    } = ends;
    let mut events = hub.subscribe();
    let guard = ViewerGuard {
        viewer: hub.connect_viewer(&client),
        hub: Arc::clone(&hub),
        client: client.clone(),
    };
    if !write_within(limit, socket.send(snapshot_message(&hub, &client))).await {
        return;
    }
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.reset();
    let mut recheck = login_recheck();
    // When the last `sessions` push went out, and whether one waits for
    // the interval to pass.
    let mut sessions_sent: Option<Instant> = None;
    let mut sessions_due = false;
    loop {
        let sessions_at = sessions_sent
            .filter(|_| sessions_due)
            .map(|at| at + SESSIONS_PUSH_INTERVAL);
        tokio::select! {
            first = events.recv() => {
                let Ok(batch) = Batch::drain(first, &mut events) else {
                    break;
                };
                let mut sent = true;
                if batch.lagged {
                    // What was lost is unknown: everything afresh.
                    sent = write_within(limit, socket.send(snapshot_message(&hub, &client))).await
                        && write_within(limit, socket.send(sessions_message())).await;
                    sessions_sent = Some(Instant::now());
                    sessions_due = false;
                }
                // A fan-out change can change what the hub grades (a pick,
                // a cleanup) and what needs you (a grading, a look): every
                // rollup afresh after it.
                let fanout = !batch.lagged
                    && batch.events.iter().any(|e| matches!(e, HubEvent::Fanout { .. }));
                for event in batch.events {
                    if !sent {
                        break;
                    }
                    if let Some(message) = event_message(&hub, &client, event) {
                        sent = write_within(limit, socket.send(message)).await;
                    }
                }
                if sent && fanout {
                    sent = write_within(limit, socket.send(snapshot_message(&hub, &client))).await;
                }
                if sent && batch.sessions && !batch.lagged {
                    if sessions_sent.is_none_or(|at| at.elapsed() >= SESSIONS_PUSH_INTERVAL) {
                        sent = write_within(limit, socket.send(sessions_message())).await;
                        sessions_sent = Some(Instant::now());
                        sessions_due = false;
                    } else {
                        sessions_due = true;
                    }
                }
                if !sent {
                    break;
                }
            }
            () = sleep_until(sessions_at) => {
                sessions_sent = Some(Instant::now());
                sessions_due = false;
                if !write_within(limit, socket.send(sessions_message())).await {
                    break;
                }
            }
            event = flywheel_api::next_event(&mut flywheel) => {
                if !write_within(limit, socket.send(flywheel_api::event_message(event))).await {
                    break;
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Text(body))) => {
                        match serde_json::from_str::<EventsClientMessage>(body.as_str()) {
                            Ok(EventsClientMessage::View { session_id, visible, present }) => {
                                let ids: Vec<&str> = session_id
                                    .iter()
                                    .chain(visible.iter())
                                    .map(String::as_str)
                                    .collect();
                                hub.set_viewing_many(guard.viewer, &ids);
                                let present = present.unwrap_or(!ids.is_empty());
                                let back = hub.activity().set_present(&client, guard.viewer, present);
                                if let Some(back) = back
                                    && !write_within(limit, socket.send(away_message(back))).await
                                {
                                    break;
                                }
                            }
                            Err(e) => {
                                let error = json!({ "type": "error", "message": format!("bad message: {e}") });
                                if !write_within(limit, socket.send(text(error))).await {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        write_within(limit, socket.flush()).await;
                        break;
                    }
                    Some(Err(_)) | None => break,
                }
            }
            _ = ping.tick() => {
                if !write_within(limit, socket.send(Message::Ping(Default::default()))).await {
                    break;
                }
            }
            close = credential.revoked(&hub, &mut recheck) => {
                write_within(limit, socket.send(Message::Close(Some(close)))).await;
                break;
            }
            () = handoff_restarting(gate.as_ref()) => {
                close_for_restart(&mut socket, limit).await;
                break;
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    write_within(limit, socket.send(Message::Close(None))).await;
                    break;
                }
            }
        }
    }
    drop(guard);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(id: &str) -> HubEvent {
        HubEvent::Status {
            session_id: id.into(),
        }
    }

    #[test]
    fn a_batch_folds_repeats_into_their_first_place_and_keeps_payloads() {
        let mut batch = Batch::default();
        let added = HubEvent::Commands {
            session_id: "a".into(),
            added: Vec::new(),
        };
        for _ in 0..1000 {
            batch.push(status("a"));
            batch.push(HubEvent::Sessions);
        }
        batch.push(added.clone());
        batch.push(status("b"));
        batch.push(status("a"));
        batch.push(HubEvent::Usage {
            session_id: "a".into(),
        });
        batch.push(HubEvent::Usage {
            session_id: "a".into(),
        });
        batch.push(HubEvent::Fanout { id: "f".into() });
        batch.push(HubEvent::Fanout { id: "f".into() });
        batch.push(HubEvent::Plugins);
        batch.push(HubEvent::Plugins);
        batch.push(added.clone());
        batch.push(HubEvent::Unwatched {
            session_id: "a".into(),
        });
        assert!(batch.sessions);
        assert!(!batch.lagged);
        assert_eq!(
            batch.events,
            vec![
                status("a"),
                added.clone(),
                status("b"),
                HubEvent::Usage {
                    session_id: "a".into()
                },
                HubEvent::Fanout { id: "f".into() },
                HubEvent::Plugins,
                // Suggested commands carry what was added: none folded.
                added,
            ]
        );
    }

    #[test]
    fn draining_takes_what_is_queued_and_reports_an_overflow() {
        let (tx, mut rx) = broadcast::channel(8);
        for i in 0..5 {
            tx.send(status(&format!("s{i}"))).unwrap();
            tx.send(status("s0")).unwrap();
        }
        // 10 sent into 8 slots: the first read reports the overflow.
        let first = rx.try_recv().map_err(|e| match e {
            broadcast::error::TryRecvError::Lagged(n) => broadcast::error::RecvError::Lagged(n),
            other => panic!("{other:?}"),
        });
        let batch = Batch::drain(first, &mut rx).unwrap();
        assert!(batch.lagged);
        assert_eq!(batch.events.len(), 5, "{:?}", batch.events);
        assert!(rx.try_recv().is_err(), "everything queued was drained");
        drop(tx);
        assert!(Batch::drain(Err(broadcast::error::RecvError::Closed), &mut rx).is_err());
    }
}
