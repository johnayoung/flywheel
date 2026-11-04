//! A session's terminal WebSocket (`/api/sessions/{id}/ws`): the replay
//! on attach, then output, input, resizes, and theme reports both ways.

use super::deflate::Deflater;
use super::{
    AppState, Client, CredentialWatch, LAGGED_SNAPSHOT_HISTORY, SNAPSHOT_FRAME,
    WS_CLOSE_RELAUNCHED, WS_CLOSE_RESTART, WS_MAX_MESSAGE, WS_PING_INTERVAL, error_response,
    login_recheck, origin_ok, upgrade_refused, write_within,
};
use crate::hub::{Hub, HubEvent};
use crate::pty::{
    Attachment, ExitInfo, PtySession, Reattach, RecentItem, SessionEvent, Update, View,
    WriteOutcome,
};
use crate::vt::{SNAPSHOT_HISTORY_BYTES, ViewerColors};
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::SinkExt;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

/// Options a terminal socket's URL may carry (all optional): see the
/// module docs of `server.rs` and docs/hub/api.md.
#[derive(Debug, Default, Deserialize)]
pub(super) struct TermQuery {
    /// `2`: offsets, resume, frames, acks.
    proto: Option<u8>,
    /// `1`: lite pacing from the start (`fps` updates a second at most).
    lite: Option<u8>,
    fps: Option<u32>,
    /// `1`: deflated binary messages.
    z: Option<u8>,
    /// Resume after output offset `since` of stream `epoch`.
    since: Option<u64>,
    epoch: Option<String>,
    /// Names this viewer's input ids (1-64 of `[A-Za-z0-9_-]`).
    client: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn session_ws(
    State(app): State<AppState>,
    Path(id): Path<String>,
    client: Client,
    headers: HeaderMap,
    query: Result<Query<TermQuery>, QueryRejection>,
    credential: Option<axum::Extension<CredentialWatch>>,
    gate: Option<axum::Extension<crate::handoff::Gate>>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if !origin_ok(&headers, client, &app.options) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin WebSocket refused");
    }
    let Ok(Query(query)) = query else {
        return error_response(StatusCode::BAD_REQUEST, "bad terminal socket options");
    };
    let options = match Options::from_query(&query) {
        Ok(options) => options,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
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
    let send_timeout = app.options.send_timeout();
    ws.max_message_size(WS_MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            let ends = Ends {
                shutdown,
                credential,
                gate,
                send_timeout,
            };
            pump(socket, Arc::clone(&session), ends, &hub, &id, options).await;
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
        /// Protocol 2: this viewer key's input number (acknowledged; a
        /// number taken already is dropped).
        id: Option<u64>,
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
    /// Pacing (protocol 2): lite updates at most `fps` a second, or the raw
    /// stream; `paused` (lite): none until unpaused (a hidden pane).
    Mode {
        lite: bool,
        fps: Option<u32>,
        paused: Option<bool>,
    },
    /// Protocol 2: answered with a `pong` (a browser cannot see WebSocket
    /// pings; this is how it tells a link that went silent from one that
    /// is only quiet).
    Ping,
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

/// A terminal focus-in or focus-out report, alone.
fn is_focus_report(bytes: &[u8]) -> bool {
    bytes == b"\x1b[I" || bytes == b"\x1b[O"
}

/// Type client input. A protocol 2 viewer numbers its input (`n`): it is
/// acknowledged once the session took it (or, the viewer naming itself
/// with `key`, had it already: sent again after a drop, not typed twice);
/// dropped, it is refused by id (`nack`; `retry` mid-handoff: the viewer
/// sends it again and the next hub takes it). Returns false when the
/// socket is gone.
async fn forward_input(
    viewer: &mut Viewer,
    session: &PtySession,
    hub: &Hub,
    id: &str,
    (key, n): (Option<&str>, Option<u64>),
    bytes: Vec<u8>,
) -> bool {
    // A browser's focus report (sent once a program asked for them) that
    // reaches a terminal still in cooked mode -- an agent TUI enables
    // focus reports before it takes the terminal raw -- would only be
    // echoed onto the screen as `^[[I`.
    let typed = !(is_focus_report(&bytes) && session.echoes_input());
    let noted = typed.then(|| bytes.clone());
    let outcome = session.write_input(key.zip(n), typed.then_some(bytes));
    let ack = |id: u64, ok: bool| match (ok, outcome) {
        (true, _) => json!({ "type": "ack", "id": id }),
        (false, o) => {
            json!({ "type": "nack", "id": id, "retry": o == Some(WriteOutcome::HandingOff) })
        }
    };
    match outcome {
        // Here already: its ack was lost.
        None => match n {
            Some(n) => viewer.text(&ack(n, true)).await,
            None => true,
        },
        Some(WriteOutcome::Queued | WriteOutcome::Closed) => {
            if let Some(bytes) = &noted {
                hub.note_typing(id, bytes);
            }
            match n {
                Some(n) => viewer.text(&ack(n, true)).await,
                None => true,
            }
        }
        Some(outcome) => {
            if let Some(n) = n
                && !viewer.text(&ack(n, false)).await
            {
                return false;
            }
            let message = if outcome == WriteOutcome::HandingOff {
                if n.is_some() {
                    // Sent again once the next hub has the session.
                    return true;
                }
                "input dropped: the hub is handing over to a new process; type it again once reconnected"
            } else {
                "input dropped: the terminal is not reading its input"
            };
            viewer
                .text(&json!({ "type": "error", "message": message }))
                .await
        }
    }
}

/// Lite updates a second unless the viewer says otherwise.
const LITE_FPS: u32 = 4;
/// Most lite updates a second a viewer may ask for.
const MAX_FPS: u32 = 30;
/// A lite update sends the output since the last one as it came while it
/// is at most this many bytes (exact, and as cheap as a frame), else a
/// frame of what changed on screen.
const LITE_RAW_MAX: usize = 1024;
/// A lite viewer resuming gets the output it missed while it is at most
/// this many bytes, else a frame (its scrollback keeps what it had).
const LITE_RESUME_MAX: usize = 16 * 1024;
/// Live events queued for a raw (protocol 2) viewer before it is behind:
/// it then gets one update in their place (a frame when they are many
/// bytes) and the raw stream from there.
const BEHIND_EVENTS: usize = 64;
/// A behind viewer's update is the raw output while it is at most this.
const CATCH_UP_RAW_MAX: usize = 64 * 1024;

/// A socket's options (from its URL).
#[derive(Debug, Clone, Default)]
struct Options {
    v2: bool,
    lite: Option<u32>,
    deflate: bool,
    since: Option<(u64, u64)>,
    client: Option<String>,
}

impl Options {
    fn from_query(query: &TermQuery) -> Result<Self, &'static str> {
        let v2 = match query.proto {
            None | Some(1) => false,
            Some(2) => true,
            Some(_) => return Err("unsupported proto (1 or 2)"),
        };
        let epoch = match &query.epoch {
            Some(hex) => Some(u64::from_str_radix(hex, 16).map_err(|_| "bad epoch")?),
            None => None,
        };
        let since = match (query.since, epoch) {
            (Some(seq), Some(epoch)) => Some((seq, epoch)),
            (None, None) => None,
            _ => return Err("since and epoch go together"),
        };
        if !v2
            && (query.lite.is_some()
                || query.z.is_some()
                || since.is_some()
                || query.client.is_some())
        {
            return Err("lite, z, since and client need proto=2");
        }
        let client = match &query.client {
            Some(key)
                if (1..=64).contains(&key.len())
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') =>
            {
                Some(key.clone())
            }
            Some(_) => return Err("bad client"),
            None => None,
        };
        Ok(Self {
            v2,
            lite: (query.lite == Some(1)).then(|| clamp_fps(query.fps)),
            deflate: query.z == Some(1),
            since,
            client,
        })
    }
}

fn clamp_fps(fps: Option<u32>) -> u32 {
    fps.unwrap_or(LITE_FPS).clamp(1, MAX_FPS)
}

/// How a viewer's output is paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    /// Every byte as it comes.
    Raw,
    /// At most one update per `every`; none while `paused`.
    Lite { every: Duration, paused: bool },
}

impl Pace {
    fn lite(fps: u32, paused: bool) -> Self {
        Pace::Lite {
            every: Duration::from_millis(u64::from(1000 / fps.max(1))),
            paused,
        }
    }
}

/// One viewer's socket, sending binary messages deflated when it asked.
pub(super) struct Viewer {
    pub(super) socket: WebSocket,
    deflate: Option<Deflater>,
    /// The exit was sent: it is heard once, whichever path (a snapshot, a
    /// resume, the live stream, a catch-up) brought it.
    exit_said: bool,
    /// How long one message may take to reach the socket
    /// ([`super::ServerOptions::send_timeout`]).
    send_timeout: Duration,
}

impl Viewer {
    pub(super) fn new(socket: WebSocket, deflate: bool, send_timeout: Duration) -> Self {
        Self {
            socket,
            deflate: deflate.then(Deflater::new),
            exit_said: false,
            send_timeout,
        }
    }

    /// Send one message within the send timeout; false when the socket is
    /// gone (or stalled).
    pub(super) async fn send(&mut self, message: Message) -> bool {
        write_within(self.send_timeout, self.socket.send(message)).await
    }

    /// Flush within the send timeout.
    pub(super) async fn flush(&mut self) {
        write_within(self.send_timeout, self.socket.flush()).await;
    }

    /// Close for a live handoff (see [`close_for_restart`]).
    pub(super) async fn close_for_restart(&mut self) {
        close_for_restart(&mut self.socket, self.send_timeout).await;
    }

    /// Say that the process ended (once per socket); false when the
    /// socket is gone.
    pub(super) async fn exit(&mut self, info: &ExitInfo) -> bool {
        if self.exit_said {
            return true;
        }
        self.exit_said = true;
        self.send(exit_message(info)).await
    }

    /// Whether the exit was sent.
    pub(super) fn exited(&self) -> bool {
        self.exit_said
    }

    pub(super) async fn text(&mut self, value: &serde_json::Value) -> bool {
        self.send(Message::Text(value.to_string().into())).await
    }

    pub(super) async fn binary(&mut self, bytes: Bytes) -> bool {
        let payload = match &mut self.deflate {
            Some(deflate) => Bytes::from(deflate.message(&bytes)),
            None => bytes,
        };
        self.send(Message::Binary(payload)).await
    }

    /// Output and resizes as they came; the output between two resizes
    /// in one message.
    async fn items(&mut self, items: Vec<RecentItem>) -> bool {
        let mut run: Vec<u8> = Vec::new();
        for item in items {
            match item {
                RecentItem::Output(bytes) => run.extend_from_slice(&bytes),
                RecentItem::Resize { cols, rows } => {
                    if !run.is_empty() && !self.binary(Bytes::from(std::mem::take(&mut run))).await
                    {
                        return false;
                    }
                    let size = json!({ "type": "size", "cols": cols, "rows": rows });
                    if !self.text(&size).await {
                        return false;
                    }
                }
            }
        }
        run.is_empty() || self.binary(Bytes::from(run)).await
    }

    /// What brings the viewer to offset `seq`.
    async fn update(&mut self, update: Update, seq: u64) -> bool {
        match update {
            Update::Current => true,
            Update::Raw(items) => self.items(items).await,
            Update::Frame { bytes, cols, rows } => {
                let header = json!({
                    "type": "frame",
                    "seq": seq,
                    "cols": cols,
                    "rows": rows,
                    "bytes": bytes.len(),
                });
                self.text(&header).await && self.binary(Bytes::from(bytes)).await
            }
        }
    }

    /// A snapshot (its header -- with the offset and stream on protocol 2,
    /// `v2` -- then its bytes) and the exit when there is one; returns the
    /// live receiver. The snapshot is released once sent.
    pub(super) async fn snapshot(
        &mut self,
        attachment: Attachment,
        v2: bool,
        continued: Option<bool>,
    ) -> Option<broadcast::Receiver<SessionEvent>> {
        let Attachment {
            replay,
            seq,
            epoch,
            cols,
            rows,
            alt_screen,
            exit,
            events,
        } = attachment;
        let mut header = json!({
            "type": "snapshot",
            "cols": cols,
            "rows": rows,
            "alt_screen": alt_screen,
            "bytes": replay.len(),
        });
        if v2 {
            header["seq"] = json!(seq);
            header["epoch"] = json!(format!("{epoch:x}"));
        }
        if let Some(continued) = continued {
            header["continued"] = json!(continued);
        }
        if !self.text(&header).await {
            return None;
        }
        let replay = Bytes::from(replay);
        let mut start = 0;
        while start < replay.len() {
            let end = replay.len().min(start + SNAPSHOT_FRAME);
            if !self.binary(replay.slice(start..end)).await {
                return None;
            }
            start = end;
        }
        drop(replay);
        if let Some(info) = &exit
            && !self.exit(info).await
        {
            return None;
        }
        Some(events)
    }

    /// A protocol 1 viewer fell behind: `reset`, then a screen-only
    /// snapshot (the live stream from there). Its events are dropped
    /// first, so the backlog it will never read is not kept for it.
    pub(super) async fn resnapshot(
        &mut self,
        session: &Arc<PtySession>,
        events: broadcast::Receiver<SessionEvent>,
    ) -> Option<broadcast::Receiver<SessionEvent>> {
        drop(events);
        if !self.text(&json!({ "type": "reset" })).await {
            return None;
        }
        let fresh = attach(session, LAGGED_SNAPSHOT_HISTORY).await?;
        self.snapshot(fresh, false, None).await
    }
}

/// Resolves once session `id` was relaunched in place: the hub's live
/// session for it is another process than `session` (never when it was
/// removed). `events` subscribes on first use. Cancel-safe.
pub(super) async fn relaunched(
    events: &mut Option<broadcast::Receiver<HubEvent>>,
    hub: &Hub,
    id: &str,
    session: &Arc<PtySession>,
) {
    let events = events.get_or_insert_with(|| hub.subscribe());
    loop {
        if hub
            .live_session(id)
            .is_some_and(|now| !Arc::ptr_eq(&now, session))
        {
            return;
        }
        if let Err(broadcast::error::RecvError::Closed) = events.recv().await {
            std::future::pending::<()>().await;
        }
    }
}

/// Close a viewer of a session that was relaunched in place: it
/// reconnects to the new process.
pub(super) async fn close_for_relaunch(viewer: &mut Viewer) {
    let close = CloseFrame {
        code: WS_CLOSE_RELAUNCHED,
        reason: "the session was relaunched; reconnect".into(),
    };
    viewer.send(Message::Close(Some(close))).await;
    viewer.flush().await;
}

/// What ends a socket besides its viewer: the hub stopping, the
/// credential going, a live handoff.
struct Ends {
    shutdown: watch::Receiver<bool>,
    credential: CredentialWatch,
    gate: Option<crate::handoff::Gate>,
    send_timeout: Duration,
}

/// Run blocking session work (a snapshot, a frame) off the async workers,
/// with the viewer's state moved in and back.
async fn off_runtime<T: Send + 'static>(
    session: &Arc<PtySession>,
    view: View,
    work: impl FnOnce(&PtySession, &mut View) -> T + Send + 'static,
) -> Option<(View, T)> {
    let session = Arc::clone(session);
    tokio::task::spawn_blocking(move || {
        let mut view = view;
        let out = work(&session, &mut view);
        (view, out)
    })
    .await
    .ok()
}

/// The first messages: a resume when the viewer is back at an offset the
/// session still has (or, lite, any offset of this output: a frame then),
/// else a snapshot. Returns the live receiver.
async fn open(
    viewer: &mut Viewer,
    session: &Arc<PtySession>,
    view: &mut View,
    since: Option<(u64, u64)>,
    lite: bool,
) -> Option<broadcast::Receiver<SessionEvent>> {
    let mut continued = None;
    if let Some((since, epoch)) = since {
        let max = if lite { LITE_RESUME_MAX } else { usize::MAX };
        let (back, reattach) = off_runtime(session, std::mem::take(view), move |s, v| {
            s.reattach(since, epoch, max, v)
        })
        .await?;
        *view = back;
        let resumed = json!({ "type": "resume", "seq": since, "epoch": format!("{epoch:x}") });
        match reattach {
            Reattach::Delta {
                items,
                cols,
                rows,
                exit,
                events,
            } => {
                let size = json!({ "type": "size", "cols": cols, "rows": rows });
                if !viewer.text(&resumed).await || !viewer.items(items).await {
                    return None;
                }
                // A resize the viewer already had is not in what was kept.
                if !viewer.text(&size).await {
                    return None;
                }
                if let Some(info) = &exit
                    && !viewer.exit(info).await
                {
                    return None;
                }
                return Some(events);
            }
            Reattach::Fresh { continued: true } if lite => {
                // Its screen is this output's: repaint it (no reset, so
                // its scrollback stays), from a cursor that knows nothing.
                let events = session.subscribe();
                let mut fresh = View {
                    seq: since,
                    ..View::default()
                };
                let (back, update) = off_runtime(session, fresh, |s, v| s.update(v, 0)).await?;
                fresh = back;
                *view = fresh;
                if !viewer.text(&resumed).await || !viewer.update(update, view.seq).await {
                    return None;
                }
                if let Some(info) = session.exit_info()
                    && !viewer.exit(&info).await
                {
                    return None;
                }
                return Some(events);
            }
            Reattach::Fresh { continued: c } => continued = Some(c),
        }
    }
    let (back, attachment) = off_runtime(session, std::mem::take(view), |s, v| {
        s.attach_view(SNAPSHOT_HISTORY_BYTES, Some(v))
    })
    .await?;
    *view = back;
    viewer.snapshot(attachment, true, continued).await
}

async fn pump(
    socket: WebSocket,
    session: Arc<PtySession>,
    ends: Ends,
    hub: &Hub,
    id: &str,
    options: Options,
) {
    let Ends {
        mut shutdown,
        mut credential,
        gate,
        send_timeout,
    } = ends;
    let mut viewer = Viewer::new(socket, options.deflate, send_timeout);
    let mut pace = match options.lite {
        Some(fps) => Pace::lite(fps, false),
        None => Pace::Raw,
    };
    let mut view = View::default();
    let events = if options.v2 {
        open(
            &mut viewer,
            &session,
            &mut view,
            options.since,
            pace != Pace::Raw,
        )
        .await
    } else {
        match attach(&session, SNAPSHOT_HISTORY_BYTES).await {
            Some(attachment) => viewer.snapshot(attachment, false, None).await,
            None => None,
        }
    };
    let Some(mut events) = events else {
        return;
    };
    // One viewer more (`live.clients` in session listings).
    hub.emit(HubEvent::Sessions);
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.reset();
    let mut recheck = login_recheck();
    // Lite: output came since the last update; the exit, to send after
    // the update that carries the last output; the earliest next update.
    let mut dirty = false;
    let mut exit: Option<ExitInfo> = None;
    let mut next = Instant::now();
    let mut relaunches = None;
    loop {
        // An exit goes out at once (after the output before it), paused
        // or not: a hidden pane still hears that its process ended.
        let due = match pace {
            Pace::Lite { paused, .. } => exit.is_some() || (dirty && !paused),
            Pace::Raw => false,
        };
        tokio::select! {
            event = events.recv() => {
                if let Pace::Lite { .. } = pace {
                    match event {
                        Ok(SessionEvent::Exit(info)) => exit = Some(info),
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => dirty = true,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                    continue;
                }
                let behind = options.v2
                    && (events.len() > BEHIND_EVENTS
                        || matches!(event, Err(broadcast::error::RecvError::Lagged(_))));
                if behind {
                    // One update for all it missed (the raw output when it
                    // is little, else a frame), then the stream from there.
                    drop(events);
                    let Some((back, (update, fresh, ended))) = off_runtime(&session, std::mem::take(&mut view), |s, v| {
                        v.frame.forget();
                        s.update_and_subscribe(v, CATCH_UP_RAW_MAX)
                    })
                    .await else {
                        break;
                    };
                    view = back;
                    events = fresh;
                    if !viewer.update(update, view.seq).await {
                        break;
                    }
                    // An exit queued in the receiver just dropped: said here.
                    if let Some(info) = ended
                        && !viewer.exit(&info).await
                    {
                        break;
                    }
                    continue;
                }
                let sent = match event {
                    Ok(SessionEvent::Output(bytes)) => {
                        view.seq += bytes.len() as u64;
                        viewer.binary(bytes).await
                    }
                    Ok(SessionEvent::Exit(info)) => viewer.exit(&info).await,
                    Ok(SessionEvent::Resize { cols, rows }) => {
                        viewer.text(&json!({ "type": "size", "cols": cols, "rows": rows })).await
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(fresh) = viewer.resnapshot(&session, events).await else {
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
            () = tokio::time::sleep_until(next), if due => {
                if dirty {
                    // Everything queued is covered by this update.
                    while events.try_recv().is_ok_and(|e| {
                        if let SessionEvent::Exit(info) = e {
                            exit = Some(info);
                        }
                        true
                    }) {}
                    let Some((back, update)) = off_runtime(&session, std::mem::take(&mut view), |s, v| {
                        s.update(v, LITE_RAW_MAX)
                    })
                    .await else {
                        break;
                    };
                    view = back;
                    dirty = false;
                    if !viewer.update(update, view.seq).await {
                        break;
                    }
                }
                if let Some(info) = exit.take()
                    && !viewer.exit(&info).await
                {
                    break;
                }
                if let Pace::Lite { every, .. } = pace {
                    next = Instant::now() + every;
                }
            }
            message = viewer.socket.recv() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(text.as_str()) {
                            Ok(ClientMessage::Input { data, id: input }) => {
                                let input = match input.filter(|_| options.v2) {
                                    Some(n) => (options.client.as_deref(), Some(n)),
                                    None => (None, None),
                                };
                                if !forward_input(&mut viewer, &session, hub, id, input, data.into_bytes()).await {
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
                                        if !viewer.text(&error).await {
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(ClientMessage::Mode { lite, fps, paused }) if options.v2 => {
                                let paused = paused.unwrap_or(false);
                                match (pace, lite) {
                                    (Pace::Raw, true) => {
                                        pace = Pace::lite(clamp_fps(fps), paused);
                                        // What is queued goes by the next update.
                                        dirty = true;
                                    }
                                    (Pace::Lite { .. }, false) => {
                                        // Up to date, then the raw stream from there.
                                        drop(events);
                                        let Some((back, (update, fresh, ended))) = off_runtime(&session, std::mem::take(&mut view), |s, v| {
                                            s.update_and_subscribe(v, usize::MAX)
                                        })
                                        .await else {
                                            break;
                                        };
                                        view = back;
                                        events = fresh;
                                        pace = Pace::Raw;
                                        dirty = false;
                                        if !viewer.update(update, view.seq).await {
                                            break;
                                        }
                                        // Heard already, or still queued in the
                                        // receiver just dropped: once either way.
                                        if let Some(info) = exit.take().or(ended)
                                            && !viewer.exit(&info).await
                                        {
                                            break;
                                        }
                                    }
                                    (Pace::Lite { paused: was, .. }, true) => {
                                        pace = Pace::lite(clamp_fps(fps), paused);
                                        if was && !paused {
                                            // Back on screen: what changed, now.
                                            dirty = true;
                                            next = Instant::now();
                                        }
                                    }
                                    (Pace::Raw, false) => {}
                                }
                            }
                            Ok(ClientMessage::Ping) if options.v2 => {
                                if !viewer.text(&json!({ "type": "pong" })).await {
                                    break;
                                }
                            }
                            Ok(ClientMessage::Ping) => {
                                let error = json!({ "type": "error", "message": "bad message: ping needs proto=2" });
                                if !viewer.text(&error).await {
                                    break;
                                }
                            }
                            Ok(ClientMessage::Mode { .. }) => {
                                let error = json!({ "type": "error", "message": "bad message: mode needs proto=2" });
                                if !viewer.text(&error).await {
                                    break;
                                }
                            }
                            Err(e) => {
                                let error = json!({ "type": "error", "message": format!("bad message: {e}") });
                                if !viewer.text(&error).await {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if !forward_input(&mut viewer, &session, hub, id, (None, None), bytes.to_vec()).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        // Complete the closing handshake: tungstenite queued
                        // the reply Close on receipt but only writes it on a
                        // flush (a send would be refused after closing), so
                        // the client sees a clean close instead of 1006.
                        viewer.flush().await;
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
            close = credential.revoked(hub, &mut recheck) => {
                viewer.send(Message::Close(Some(close))).await;
                break;
            }
            () = relaunched(&mut relaunches, hub, id, &session), if viewer.exited() => {
                close_for_relaunch(&mut viewer).await;
                break;
            }
            () = handoff_restarting(gate.as_ref()) => {
                viewer.close_for_restart().await;
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

/// Resolves once a live handoff froze the hub (never without a gate).
pub(super) async fn handoff_restarting(gate: Option<&crate::handoff::Gate>) {
    match gate {
        Some(gate) => gate.restarting().await,
        None => std::future::pending().await,
    }
}

/// Close a WebSocket for a live handoff: clients reconnect to whichever
/// process serves next.
pub(super) async fn close_for_restart(socket: &mut WebSocket, send_timeout: Duration) {
    let close = CloseFrame {
        code: WS_CLOSE_RESTART,
        reason: "the hub is restarting (upgrade); reconnect".into(),
    };
    let _ = tokio::time::timeout(send_timeout, async {
        let _ = socket.send(Message::Close(Some(close))).await;
        let _ = socket.flush().await;
    })
    .await;
}
