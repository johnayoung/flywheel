//! `fw hub attach [SESSION]`: the hub's sessions in this terminal (local,
//! or over SSH, or against a remote hub by `--url`).
//!
//! Without SESSION a picker lists workspaces > sessions with live agent
//! status (`/api/events`). Attached, the terminal is the session's: the
//! hub's snapshot is replayed, then live output passes through raw (only
//! terminal queries are held back -- the hub answers them -- see
//! [`filter`]), every typed byte is forwarded verbatim (bracketed pastes,
//! mouse reports, function keys), and the PTY follows this terminal's size
//! (SIGWINCH). The prefix key (Ctrl+B unless `--prefix`) then:
//!
//! * `d` detach (the session keeps running), `s` switch session (picker),
//!   `t` status bar on/off, `r` redraw (fresh snapshot), `?` the key table;
//! * the prefix again sends it once.
//!
//! When the attached session's process ends the client ends too (as tmux
//! does), its final screen left in place. A session that had already ended
//! stays attached, to read its last screen.
//!
//! Output is flow-controlled: at most [`OUTPUT_CREDIT`] bytes are between
//! the socket and this terminal, so a terminal slower than the session
//! (a slow SSH link) stalls the socket, and the hub's own handling of a
//! lagging viewer (a fresh snapshot) takes over instead of a backlog here.
//!
//! The status bar (one row under the session, which gets one row less)
//! names the session and its status and counts blocked / done sessions
//! hub-wide. It is drawn outside the session's scroll region and repainted
//! with the session's own cursor and modes restored after it, from a local
//! mirror of the session's terminal ([`crate::vt::Emulator`]).
//!
//! Attached counts as viewing the session, as a browser tab does: a turn
//! that finishes is `idle` for this client, not `done`. The client id is
//! the same on every run for this user on this machine, so what one
//! attach saw stays seen in the next.
//!
//! With the token from the state dir, a rotation (`fw hub token --rotate`)
//! re-reads it and reconnects.
//!
//! The terminal is restored on every way out: detach, errors, the hub
//! going away, SIGTERM/SIGHUP, and panics (a hook).

mod filter;
mod keys;
mod picker;
mod term;

use crate::endpoint::{Endpoint, EndpointArgs, Socket, stable_client_id};
use crate::util::str_field as str_of;
use crate::vt::Emulator;
use filter::OutputFilter;
use futures_util::{SinkExt, StreamExt};
use keys::{Action, Command, Keys, Prefix};
use picker::Picked;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use term::{Input, StdinReader};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

/// How long a status-bar message (help, an unbound key) stays.
const MESSAGE_FOR: Duration = Duration::from_secs(4);
/// How long a resize for a mismatched snapshot waits for the hub's word.
const SIZE_WAIT: Duration = Duration::from_secs(2);
/// Wait between reconnects of the events socket.
const EVENTS_RETRY: Duration = Duration::from_secs(2);
/// Most session output bytes received but not yet written to the terminal.
const OUTPUT_CREDIT: usize = 256 * 1024;
/// The hub's close code for a rotated token.
const CLOSE_ROTATED: u16 = crate::server::WS_CLOSE_ROTATED;
/// How long a restarting hub (`fw hub upgrade`) gets to come back.
const RESTART_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug, clap::Args)]
pub(crate) struct AttachArgs {
    /// Session id, unique id prefix, or name. Default: pick one.
    session: Option<String>,
    /// The prefix key for commands (tmux-like): C-b, C-a, ^], ...
    #[arg(long, default_value = "C-b", value_name = "KEY")]
    prefix: String,
    /// Start with the status bar off (prefix t toggles it).
    #[arg(long)]
    no_status: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// What the hub looks like to this client (kept by the events task).
#[derive(Debug, Clone, Default)]
pub struct HubView {
    pub workspaces: Vec<Value>,
    pub sessions: Vec<Value>,
    /// Agent status per session id, as this client sees it.
    pub status: HashMap<String, String>,
    pub workspace_status: HashMap<String, String>,
    /// Sessions the hub grades itself (open fan-outs' candidates): their
    /// finished turns are not counted done.
    pub graded: HashSet<String>,
    /// Open fan-outs whose latest grading nobody has looked at: one done
    /// each.
    pub fanouts: usize,
    /// The events socket is connected.
    pub online: bool,
    pub error: Option<String>,
}

/// Blocked and done, hub-wide, counted as the browser's Needs you lists
/// them: a graded session's finished turn is the hub's, and a fan-out
/// needing you is one done.
pub fn status_counts(view: &HubView) -> (usize, usize) {
    let blocked = view.status.values().filter(|s| *s == "blocked").count();
    let done = view
        .status
        .iter()
        .filter(|(id, s)| *s == "done" && !view.graded.contains(*id))
        .count();
    (blocked, done + view.fanouts)
}

/// Everything the attach loop waits on.
pub enum Ev {
    Input(Input),
    /// Traffic of the session connection `generation`; `credit`: output
    /// bytes to give back to it once handled.
    Session {
        generation: u64,
        msg: SessionMsg,
        credit: u32,
    },
    /// The hub view changed.
    Hub,
    /// The terminal was resized.
    Winch,
    /// A signal asks the client to end: why.
    Terminate(String),
}

/// What a session's WebSocket said.
pub enum SessionMsg {
    /// A snapshot follows: its size and length.
    Snapshot {
        cols: u16,
        rows: u16,
        bytes: usize,
    },
    Bytes(Vec<u8>),
    /// The PTY's size changed (any viewer).
    Size {
        cols: u16,
        rows: u16,
    },
    Exit(String),
    Error(String),
    /// The connection ended: why.
    Closed(String),
    /// The hub rotated its token and closed the connection.
    Rotated,
    /// The hub process is being replaced (`fw hub upgrade`): reconnect.
    Restart,
}

pub(crate) fn attach(args: &AttachArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let fail = |err: &mut dyn Write, message: &str| {
        let _ = writeln!(err, "fw hub attach: {message}");
        1
    };
    let prefix = match Prefix::parse(&args.prefix) {
        Ok(prefix) => prefix,
        Err(message) => return fail(err, &message),
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return fail(err, "needs a terminal (stdin and stdout)");
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("fw-hub-attach")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(err, &format!("cannot start: {e}")),
    };
    let endpoint = match Endpoint::resolve(&args.endpoint) {
        Ok(endpoint) => Arc::new(endpoint),
        Err(message) => return fail(err, &message),
    };
    // Read the hub once up front: a bad token or session fails here, in
    // cooked mode.
    let first = runtime.block_on(async {
        let sessions = endpoint.sessions().await?;
        let workspaces = endpoint.workspaces().await?;
        Ok::<_, String>((sessions, workspaces))
    });
    let (sessions, workspaces) = match first {
        Ok(lists) => lists,
        Err(message) => return fail(err, &message),
    };
    let target = match &args.session {
        Some(query) => match crate::script::resolve(&sessions, query, "session") {
            Ok(session) if session.get("live").is_some_and(|l| !l.is_null()) => {
                Some(str_of(session, "id").to_owned())
            }
            Ok(session) => {
                return fail(
                    err,
                    &format!(
                        "session {} has no terminal on the hub ({}); `fw hub relaunch` starts it again",
                        crate::script::session_label(session),
                        crate::script::state_label(session)
                    ),
                );
            }
            Err(message) => return fail(err, &message),
        },
        None => None,
    };
    let view = Arc::new(Mutex::new(HubView {
        workspaces,
        sessions,
        ..HubView::default()
    }));
    let (tx, rx) = std::sync::mpsc::channel();
    let (viewing, viewing_rx) = watch::channel(None::<String>);
    runtime.spawn(events_task(
        Arc::clone(&endpoint),
        Arc::clone(&view),
        tx.clone(),
        viewing_rx,
    ));
    runtime.spawn(signals_task(tx.clone()));

    let tty = match term::Tty::enter() {
        Ok(tty) => tty,
        Err(e) => return fail(err, &format!("cannot set the terminal to raw mode: {e}")),
    };
    let mut client = Client {
        runtime: &runtime,
        endpoint: &endpoint,
        view: &view,
        tx,
        rx,
        viewing,
        prefix,
        bar: !args.no_status,
        generation: 0,
    };
    let ending = client.run(target);
    drop(client);
    drop(tty);
    // Background tasks hold sockets; do not wait for their goodbyes.
    runtime.shutdown_timeout(Duration::from_millis(200));
    match ending {
        Ending::Detached(name) => {
            let _ = term::write(b"\x1b[H\x1b[2J");
            let _ = writeln!(out, "[detached from {name}]");
            0
        }
        Ending::Ended { label, how, park } => {
            let _ = term::write(park.as_bytes());
            let _ = writeln!(out, "[{label} {how}]");
            0
        }
        Ending::Quit => 0,
        Ending::Failed(message) => {
            let _ = term::write(b"\x1b[H\x1b[2J");
            fail(err, &message)
        }
    }
}

/// How the client ended.
enum Ending {
    /// Detached from this session (`workspace/name`).
    Detached(String),
    /// The attached session's process ended: its name, how, and where to
    /// put the cursor for the goodbye.
    Ended {
        label: String,
        how: String,
        park: String,
    },
    /// Left the picker.
    Quit,
    Failed(String),
}

/// How an attachment ended.
enum Leave {
    Detach,
    Switch,
    /// The session's process ended while attached: how, and where the
    /// cursor goes for the goodbye (set on the way out).
    Exited {
        how: String,
        park: String,
    },
    Failed(String),
}

struct Client<'a> {
    runtime: &'a tokio::runtime::Runtime,
    endpoint: &'a Arc<Endpoint>,
    view: &'a Arc<Mutex<HubView>>,
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    viewing: watch::Sender<Option<String>>,
    prefix: Prefix,
    bar: bool,
    generation: u64,
}

impl Client<'_> {
    fn run(&mut self, target: Option<String>) -> Ending {
        let mut current = target;
        let mut from: Option<String> = None;
        loop {
            let id = match current.take() {
                Some(id) => id,
                None => {
                    let _ = self.viewing.send(None);
                    match picker::pick(self.view, &self.rx, from.as_deref(), self.endpoint.base()) {
                        Picked::Attach(id) => id,
                        Picked::Back => match from.take() {
                            Some(id) => id,
                            None => return Ending::Quit,
                        },
                        // Quit from the switch picker: as a detach from the
                        // session it was opened on.
                        Picked::Quit => match from {
                            Some(id) => return Ending::Detached(self.label(&id)),
                            None => return Ending::Quit,
                        },
                        Picked::Abort(why) => return Ending::Failed(why),
                    }
                }
            };
            let label = self.label(&id);
            match self.attached(&id) {
                Leave::Detach => return Ending::Detached(label),
                Leave::Switch => from = Some(id),
                Leave::Exited { how, park } => return Ending::Ended { label, how, park },
                Leave::Failed(message) => return Ending::Failed(message),
            }
        }
    }

    /// `workspace/name` of a session.
    fn label(&self, id: &str) -> String {
        let view = self.view.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(session) = view.sessions.iter().find(|s| str_of(s, "id") == id) else {
            return id.get(..8).unwrap_or(id).to_owned();
        };
        let ws = view
            .workspaces
            .iter()
            .find(|w| str_of(w, "id") == str_of(session, "workspace_id"))
            .map(|w| str_of(w, "name"))
            .unwrap_or("");
        if ws.is_empty() {
            str_of(session, "name").to_owned()
        } else {
            format!("{ws}/{}", str_of(session, "name"))
        }
    }

    fn connect(&mut self, id: &str) -> Result<Link, String> {
        self.generation += 1;
        let generation = self.generation;
        let socket = self
            .runtime
            .block_on(self.endpoint.websocket(&format!("/api/sessions/{id}/ws")))?;
        Ok(Link::spawn(
            self.runtime,
            socket,
            generation,
            id.to_owned(),
            self.tx.clone(),
        ))
    }

    /// Attach to session `id` until detach, switch, or failure.
    fn attached(&mut self, id: &str) -> Leave {
        let _ = self.viewing.send(Some(id.to_owned()));
        let link = match self.connect(id) {
            Ok(link) => link,
            Err(message) => return Leave::Failed(message),
        };
        let reader = StdinReader::start(self.tx.clone(), Ev::Input);
        let running = {
            let view = self.view.lock().unwrap_or_else(PoisonError::into_inner);
            view.sessions
                .iter()
                .find(|s| str_of(s, "id") == id)
                .and_then(|s| s.get("live")?.get("pid"))
                .is_some_and(|pid| !pid.is_null())
        };
        let mut screen = Screen::new(self.bar, link, self.label(id), running);
        let leave = self.pump(id, &mut screen);
        drop(reader);
        screen.link.close();
        // Leave the terminal clean for the picker or the shell.
        let _ = term::write(term::RESET);
        if let Leave::Exited { how, .. } = leave {
            // The session's last screen stays; the bar row (or a new last
            // line) takes the client's goodbye, once the terminal is
            // restored (which homes the cursor).
            let rows = screen.term.1;
            let park = if screen.bar_shown() {
                format!("\x1b[{rows};1H\x1b[2K")
            } else {
                format!("\x1b[{rows};1H\r\n")
            };
            return Leave::Exited { how, park };
        }
        leave
    }

    fn pump(&mut self, id: &str, screen: &mut Screen) -> Leave {
        let mut keys = Keys::new(self.prefix);
        loop {
            let now = Instant::now();
            let timeout = [
                screen.message.as_ref().map(|(_, until)| *until),
                screen.awaiting_size,
            ]
            .into_iter()
            .flatten()
            .min()
            .map_or(Duration::from_secs(3600), |at| {
                at.saturating_duration_since(now)
            });
            let ev = match self.rx.recv_timeout(timeout) {
                Ok(ev) => ev,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    if screen
                        .message
                        .as_ref()
                        .is_some_and(|(_, until)| *until <= now)
                    {
                        screen.message = None;
                    }
                    if screen.awaiting_size.is_some_and(|at| at <= now) {
                        screen.awaiting_size = None;
                    }
                    screen.bar_dirty = true;
                    if let Err(leave) = screen.flush_bar(self) {
                        return leave;
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Leave::Failed("internal channel closed".into());
                }
            };
            let step = match ev {
                Ev::Input(Input::Bytes(_)) if screen.help => {
                    // Any key leaves the key table; a fresh snapshot
                    // repaints the session under it.
                    screen.help = false;
                    let _ = term::write(b"\x1b[0m\x1b[H\x1b[2J");
                    if let Err(message) = self.reattach(id, screen) {
                        return Leave::Failed(message);
                    }
                    Ok(())
                }
                Ev::Input(Input::Bytes(bytes)) => {
                    let was_armed = keys.armed();
                    let mut step = Ok(());
                    for action in keys.feed(&bytes) {
                        match action {
                            Action::Send(bytes) => screen.send_input(bytes),
                            Action::Run(command) => {
                                if let Some(leave) = self.command(id, screen, command) {
                                    return leave;
                                }
                            }
                        }
                    }
                    if keys.armed() != was_armed {
                        screen.armed = keys.armed();
                        screen.bar_dirty = true;
                        step = screen.flush_bar(self);
                    }
                    step
                }
                Ev::Input(Input::Closed(why)) => return Leave::Failed(why),
                Ev::Session {
                    generation,
                    msg,
                    credit,
                } => {
                    if generation != screen.link.generation {
                        continue;
                    }
                    let step = if screen.help
                        && !matches!(
                            msg,
                            SessionMsg::Exit(_)
                                | SessionMsg::Closed(_)
                                | SessionMsg::Rotated
                                | SessionMsg::Restart
                        ) {
                        // Under the key table: dropped (redrawn after).
                        Ok(())
                    } else {
                        self.session_msg(id, screen, msg)
                    };
                    // Written (or dropped): the socket may read on.
                    screen.link.credit.add_permits(credit as usize);
                    step
                }
                Ev::Hub => {
                    screen.label = self.label(id);
                    screen.bar_dirty = true;
                    screen.flush_bar(self)
                }
                Ev::Winch => {
                    screen.resize_to_terminal();
                    screen.flush_bar(self)
                }
                Ev::Terminate(why) => return Leave::Failed(why),
            };
            if let Err(leave) = step {
                return leave;
            }
        }
    }

    fn command(&mut self, id: &str, screen: &mut Screen, command: Command) -> Option<Leave> {
        screen.armed = false;
        match command {
            Command::Detach => return Some(Leave::Detach),
            Command::Switch => return Some(Leave::Switch),
            Command::ToggleBar => {
                self.bar = !self.bar;
                screen.set_bar(self.bar);
            }
            Command::Redraw => {
                if let Err(message) = self.reattach(id, screen) {
                    return Some(Leave::Failed(message));
                }
            }
            Command::Help => {
                screen.help = true;
                let out = help_bytes(screen.term, &self.prefix.label());
                if let Err(e) = term::write(&out) {
                    return Some(Leave::Failed(format!("terminal: {e}")));
                }
                return None;
            }
            Command::Unbound(byte) => {
                screen.flash(format!(
                    "{} {} is not bound ({} ? for help)",
                    self.prefix.label(),
                    printable(byte),
                    self.prefix.label()
                ));
            }
        }
        screen.bar_dirty = true;
        screen.flush_bar(self).err()
    }

    /// A new connection to the same session (a fresh snapshot).
    fn reattach(&mut self, id: &str, screen: &mut Screen) -> Result<(), String> {
        let link = self.connect(id)?;
        let old = std::mem::replace(&mut screen.link, link);
        old.close();
        screen.awaiting_size = None;
        Ok(())
    }

    fn session_msg(&mut self, id: &str, screen: &mut Screen, msg: SessionMsg) -> Result<(), Leave> {
        match msg {
            SessionMsg::Snapshot { cols, rows, bytes } => {
                screen.snapshot(cols, rows, bytes);
                let want = screen.wanted_size();
                if (cols, rows) != want && !screen.resized_for_snapshot {
                    // Attach at this terminal's size: resize, then take a
                    // fresh snapshot at it (once).
                    screen.resized_for_snapshot = true;
                    screen.awaiting_size = Some(Instant::now() + SIZE_WAIT);
                    screen.link.resize(want);
                }
                Ok(())
            }
            SessionMsg::Bytes(bytes) => {
                screen.output(&bytes).map_err(Leave::Failed)?;
                screen.flush_bar(self)
            }
            SessionMsg::Size { cols, rows } => {
                if screen.awaiting_size.is_some() {
                    return self.reattach(id, screen).map_err(Leave::Failed);
                }
                screen.server_resized(cols, rows);
                screen.flush_bar(self)
            }
            SessionMsg::Exit(how) => {
                if screen.running {
                    // It ended while attached: so does the client.
                    return Err(Leave::Exited {
                        how,
                        park: String::new(),
                    });
                }
                // An ended session's PTY is not resized: no size will come.
                screen.awaiting_size = None;
                screen.exited = Some(how.clone());
                if screen.pty != screen.wanted_size() && screen.snapshot_left == 0 {
                    // Its final screen, at a size this terminal is not:
                    // show the text top-aligned rather than scrolled.
                    screen.repaint_text().map_err(Leave::Failed)?;
                }
                let p = self.prefix.label();
                if screen.bar {
                    screen.bar_dirty = true;
                    screen.flush_bar(self)
                } else {
                    term::write(
                        format!("\r\n[session {how}: {p} s switch, {p} d detach]\r\n").as_bytes(),
                    )
                    .map_err(|e| Leave::Failed(format!("terminal: {e}")))
                }
            }
            SessionMsg::Error(message) => {
                screen.flash(message);
                screen.bar_dirty = true;
                screen.flush_bar(self)
            }
            SessionMsg::Closed(why) => Err(Leave::Failed(why)),
            SessionMsg::Restart => {
                screen.flash("the hub is restarting (upgrade); reconnecting".into());
                screen.bar_dirty = true;
                let _ = screen.flush_bar(self);
                // The next process answers once the handoff completes (or
                // this one again after a rollback).
                let deadline = Instant::now() + RESTART_WINDOW;
                let mut delay = Duration::from_millis(200);
                loop {
                    match self.reattach(id, screen) {
                        Ok(()) => return Ok(()),
                        Err(e) if Instant::now() >= deadline => {
                            return Err(Leave::Failed(format!(
                                "the hub restarted and did not come back: {e}"
                            )));
                        }
                        Err(_) => {
                            std::thread::sleep(delay);
                            delay = (delay * 2).min(Duration::from_secs(2));
                        }
                    }
                }
            }
            SessionMsg::Rotated => {
                if self.endpoint.reload_token().is_some() {
                    // The token came from the state dir: the new one is
                    // there; carry on with it.
                    self.reattach(id, screen).map_err(Leave::Failed)
                } else {
                    Err(Leave::Failed(
                        "the hub's token was rotated: get the new one with `fw hub token`".into(),
                    ))
                }
            }
        }
    }
}

fn printable(byte: u8) -> String {
    match byte {
        0x20..=0x7e => char::from(byte).to_string(),
        0x00..=0x1f => format!("C-{}", char::from(byte + 0x60)),
        other => format!("0x{other:02x}"),
    }
}

/// The attached session as this terminal shows it.
struct Screen {
    link: Link,
    filter: OutputFilter,
    /// A mirror of the session's terminal (cursor, pen, modes) to restore
    /// after drawing the bar.
    mirror: Emulator,
    /// The PTY's size as the hub last said.
    pty: (u16, u16),
    /// This terminal's size.
    term: (u16, u16),
    bar: bool,
    bar_dirty: bool,
    /// Snapshot bytes still to come.
    snapshot_left: usize,
    /// Resized for a mismatched snapshot: re-attach once the hub confirms
    /// the size (an exited session never does; given up at the instant).
    awaiting_size: Option<Instant>,
    resized_for_snapshot: bool,
    armed: bool,
    exited: Option<String>,
    /// The session's process ran when attached (its exit ends the client).
    running: bool,
    /// The key table is up.
    help: bool,
    message: Option<(String, Instant)>,
    label: String,
}

impl Screen {
    fn new(bar: bool, link: Link, label: String, running: bool) -> Self {
        let term = term::size();
        Self {
            link,
            filter: OutputFilter::new(None),
            mirror: Emulator::new(term.0, term.1, 0),
            pty: term,
            term,
            bar,
            bar_dirty: false,
            snapshot_left: 0,
            awaiting_size: None,
            resized_for_snapshot: false,
            armed: false,
            exited: None,
            running,
            help: false,
            message: None,
            label,
        }
    }

    /// Whether a bar row exists (on, and the terminal has room).
    fn bar_shown(&self) -> bool {
        self.bar && self.term.1 >= 3
    }

    /// The PTY size this terminal wants.
    fn wanted_size(&self) -> (u16, u16) {
        let rows = if self.bar_shown() {
            self.term.1 - 1
        } else {
            self.term.1
        };
        (self.term.0, rows)
    }

    /// The session rows above the bar (the scroll region's bottom).
    fn region_rows(&self) -> Option<u16> {
        self.bar_shown()
            .then(|| self.pty.1.min(self.term.1 - 1).max(1))
    }

    fn snapshot(&mut self, cols: u16, rows: u16, bytes: usize) {
        self.pty = (cols, rows);
        self.mirror = Emulator::new(cols.max(1), rows.max(1), 0);
        self.filter.reset();
        self.filter.set_rows(self.region_rows());
        self.snapshot_left = bytes;
        self.exited = None;
        self.bar_dirty = true;
    }

    fn output(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.mirror.process(bytes);
        // The hub answers queries; the mirror's replies go nowhere.
        let _ = self.mirror.take_replies();
        let filtered = self.filter.feed(bytes);
        if filtered.bar_damaged {
            self.bar_dirty = true;
        }
        if self.snapshot_left > 0 {
            self.snapshot_left = self.snapshot_left.saturating_sub(bytes.len());
            if self.snapshot_left == 0 {
                self.bar_dirty = true;
            }
        }
        term::write(&filtered.out).map_err(|e| format!("terminal: {e}"))
    }

    /// Paint the mirror's text (no colours), top-aligned and clipped to
    /// the rows above the bar.
    fn repaint_text(&mut self) -> Result<(), String> {
        let (cols, _) = self.term;
        let rows = self.wanted_size().1;
        let lines = self.mirror.text(crate::vt::TextSource::Visible).lines;
        let mut out = b"\x1b[0m\x1b[r\x1b[H\x1b[2J".to_vec();
        for (i, line) in lines.iter().take(usize::from(rows)).enumerate() {
            out.extend(format!("\x1b[{};1H", i + 1).into_bytes());
            out.extend(truncate(line, usize::from(cols)).into_bytes());
        }
        self.bar_dirty = true;
        term::write(&out).map_err(|e| format!("terminal: {e}"))
    }

    fn server_resized(&mut self, cols: u16, rows: u16) {
        self.pty = (cols, rows);
        self.mirror.resize(cols.max(1), rows.max(1));
        self.filter.set_rows(self.region_rows());
        self.bar_dirty = true;
    }

    /// SIGWINCH: ask for the new size.
    fn resize_to_terminal(&mut self) {
        self.term = term::size();
        self.filter.set_rows(self.region_rows());
        self.link.resize(self.wanted_size());
        self.bar_dirty = true;
    }

    fn set_bar(&mut self, on: bool) {
        self.bar = on;
        if !self.bar_shown() {
            // Free the bar row: full-height region, row cleared, cursor
            // and modes back.
            let mut out = format!("\x1b[r\x1b[{};1H\x1b[0m\x1b[2K", self.term.1).into_bytes();
            out.extend(self.mirror.restore_sequence());
            let _ = term::write(&out);
        }
        self.filter.set_rows(self.region_rows());
        self.link.resize(self.wanted_size());
        self.bar_dirty = true;
    }

    fn send_input(&mut self, bytes: Vec<u8>) {
        if self.exited.is_some() {
            return;
        }
        // Typing here claims the PTY size back from another viewer.
        let want = self.wanted_size();
        if self.pty != want && self.awaiting_size.is_none() {
            self.link.resize(want);
        }
        self.link.input(bytes);
    }

    fn flash(&mut self, message: String) {
        self.message = Some((message, Instant::now() + MESSAGE_FOR));
    }

    /// Redraw the bar if due and safe now (between sequences, snapshot
    /// done).
    fn flush_bar(&mut self, client: &Client<'_>) -> Result<(), Leave> {
        if !self.bar_dirty || !self.bar_shown() {
            self.bar_dirty = false;
            return Ok(());
        }
        if self.help
            || self.snapshot_left > 0
            || self.awaiting_size.is_some()
            || !self.filter.at_boundary()
        {
            return Ok(());
        }
        self.bar_dirty = false;
        let text = {
            let view = client.view.lock().unwrap_or_else(PoisonError::into_inner);
            self.bar_text(&view, client.prefix)
        };
        let mut out = Vec::new();
        bar_bytes(&mut out, self.term, self.region_rows().unwrap_or(1), &text);
        out.extend(self.mirror.restore_sequence());
        term::write(&out).map_err(|e| Leave::Failed(format!("terminal: {e}")))
    }

    fn bar_text(&self, view: &HubView, prefix: Prefix) -> BarText {
        let p = prefix.label();
        let id = self.link.session.as_str();
        let status = view.status.get(id).cloned();
        let (blocked, done) = status_counts(view);
        let left = match (&self.exited, &status) {
            (Some(how), _) => format!(" {} [{how}] ", self.label),
            (None, Some(status)) => format!(" {} [{status}] ", self.label),
            (None, None) => format!(" {} ", self.label),
        };
        let right = if self.armed {
            format!(" {p}: d detach  s switch  t bar  r redraw  ? help ")
        } else if let Some((message, _)) = &self.message {
            format!(" {message} ")
        } else if self.exited.is_some() {
            format!(" {p} s switch  {p} d detach ")
        } else {
            let mut right = String::new();
            if !view.online {
                right.push_str(" events offline |");
            }
            if self.pty != self.wanted_size() {
                right.push_str(&format!(
                    " {}x{} (another viewer's size; type to take it) |",
                    self.pty.0, self.pty.1
                ));
            }
            right.push_str(&format!(" {p} ? help "));
            right
        };
        BarText {
            left,
            blocked,
            done,
            right,
        }
    }
}

struct BarText {
    left: String,
    blocked: usize,
    done: usize,
    right: String,
}

/// Draw the bar on the terminal's last row: everything that could change
/// how text lands is neutralized first (origin and insert modes, charsets,
/// pen, hyperlink), and the session's scroll region is set back after
/// (its cursor and modes are the caller's to restore).
fn bar_bytes(out: &mut Vec<u8>, (cols, rows): (u16, u16), region: u16, text: &BarText) {
    use std::fmt::Write as _;
    let width = usize::from(cols.saturating_sub(1));
    let mut s = String::new();
    let _ = write!(
        s,
        "\x1b[?25l\x1b[?6l\x1b[4l\x1b(B\x0f\x1b]8;;\x1b\\\x1b[0m\x1b[{rows};1H\x1b[48;5;236;38;5;252m"
    );
    let mut used = 0;
    let push = |s: &mut String, piece: &str, style: &str, used: &mut usize| {
        let piece = truncate(piece, width.saturating_sub(*used));
        *used += display_width(&piece);
        s.push_str(style);
        s.push_str(&piece);
    };
    push(&mut s, &text.left, "\x1b[1;38;5;255;48;5;24m", &mut used);
    push(&mut s, " ", "\x1b[22;38;5;252;48;5;236m", &mut used);
    let blocked_style = if text.blocked > 0 {
        "\x1b[1;38;5;220m"
    } else {
        "\x1b[22;38;5;245m"
    };
    let done_style = if text.done > 0 {
        "\x1b[1;38;5;114m"
    } else {
        "\x1b[22;38;5;245m"
    };
    push(
        &mut s,
        &format!(" {} blocked ", text.blocked),
        blocked_style,
        &mut used,
    );
    push(
        &mut s,
        &format!(" {} done ", text.done),
        done_style,
        &mut used,
    );
    let right_width = display_width(&text.right);
    let gap = width.saturating_sub(used + right_width);
    push(&mut s, &" ".repeat(gap), "\x1b[22;38;5;252m", &mut used);
    push(&mut s, &text.right, "\x1b[22;38;5;250m", &mut used);
    push(
        &mut s,
        &" ".repeat(width.saturating_sub(used)),
        "",
        &mut used,
    );
    let _ = write!(s, "\x1b[K\x1b[0m\x1b[1;{region}r");
    out.extend_from_slice(s.as_bytes());
}

/// The key table (`prefix ?`), one binding a line, clipped to the terminal.
fn help_bytes((cols, rows): (u16, u16), p: &str) -> Vec<u8> {
    let lines = [
        format!("fw hub attach: keys (prefix {p})"),
        String::new(),
        format!("  {p} d   detach (the session keeps running)"),
        format!("  {p} s   switch session"),
        format!("  {p} t   status bar on/off"),
        format!("  {p} r   redraw"),
        format!("  {p} ?   this table"),
        format!("  {p} {p}   send {p}"),
        String::new(),
        "any key to return".to_owned(),
    ];
    let mut out = b"\x1b[r\x1b[0m\x1b[?25l\x1b[H\x1b[2J".to_vec();
    for (i, line) in lines.iter().take(usize::from(rows)).enumerate() {
        out.extend(format!("\x1b[{};2H", i + 1).into_bytes());
        out.extend(truncate(line, usize::from(cols.saturating_sub(1))).into_bytes());
    }
    out
}

fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// `s` cut to `max` columns, control characters dropped.
fn truncate(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars().filter(|c| !c.is_control()) {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// One WebSocket to one session: a reader task feeding [`Ev::Session`] and
/// a writer fed by [`Link::input`] / [`Link::resize`].
struct Link {
    generation: u64,
    session: String,
    out: mpsc::UnboundedSender<Message>,
    reader: tokio::task::JoinHandle<()>,
    /// Output bytes the reader may still pass on ([`OUTPUT_CREDIT`]).
    credit: Arc<Semaphore>,
}

impl Link {
    fn spawn(
        runtime: &tokio::runtime::Runtime,
        socket: Socket,
        generation: u64,
        session: String,
        tx: Sender<Ev>,
    ) -> Self {
        let (mut sink, mut stream) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
        runtime.spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let close = matches!(message, Message::Close(_));
                if sink.send(message).await.is_err() || close {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let credit = Arc::new(Semaphore::new(OUTPUT_CREDIT));
        let budget = Arc::clone(&credit);
        let reader = runtime.spawn(async move {
            let send = |msg, credit| {
                tx.send(Ev::Session {
                    generation,
                    msg,
                    credit,
                })
                .is_ok()
            };
            let closed = loop {
                let message = match stream.next().await {
                    Some(Ok(message)) => message,
                    Some(Err(e)) => {
                        break SessionMsg::Closed(format!("connection to the hub lost: {e}"));
                    }
                    None => break SessionMsg::Closed("the hub closed the connection".into()),
                };
                let msg = match message {
                    Message::Binary(bytes) => SessionMsg::Bytes(bytes.to_vec()),
                    Message::Text(text) => match parse_text(text.as_str()) {
                        Some(msg) => msg,
                        None => continue,
                    },
                    Message::Close(frame) => break close_reason(frame.as_ref()),
                    _ => continue,
                };
                // Wait for the terminal to take earlier output first: the
                // socket is not read meanwhile, so a slow terminal slows
                // the hub's sending to it.
                let cost = match &msg {
                    SessionMsg::Bytes(bytes) => bytes.len().min(OUTPUT_CREDIT),
                    _ => 0,
                };
                let cost = u32::try_from(cost).unwrap_or(u32::MAX);
                if cost > 0 {
                    match budget.acquire_many(cost).await {
                        Ok(permit) => permit.forget(),
                        Err(_) => return,
                    }
                }
                if !send(msg, cost) {
                    return;
                }
            };
            let _ = send(closed, 0);
        });
        Self {
            generation,
            session,
            out,
            reader,
            credit,
        }
    }

    fn input(&self, bytes: Vec<u8>) {
        let _ = self.out.send(Message::Binary(bytes.into()));
    }

    fn resize(&self, (cols, rows): (u16, u16)) {
        let message = json!({ "type": "resize", "cols": cols, "rows": rows });
        let _ = self.out.send(Message::Text(message.to_string().into()));
    }

    fn close(&self) {
        self.reader.abort();
        let _ = self.out.send(Message::Close(None));
    }
}

fn close_reason(frame: Option<&CloseFrame>) -> SessionMsg {
    match frame {
        Some(f) if u16::from(f.code) == CLOSE_ROTATED => SessionMsg::Rotated,
        Some(f) if u16::from(f.code) == crate::server::WS_CLOSE_RESTART => SessionMsg::Restart,
        Some(f) if !f.reason.is_empty() => {
            SessionMsg::Closed(format!("the hub closed the session: {}", f.reason))
        }
        _ => SessionMsg::Closed("the hub closed the connection (stopped?)".into()),
    }
}

fn parse_text(text: &str) -> Option<SessionMsg> {
    let v: Value = serde_json::from_str(text).ok()?;
    let num = |field: &str| {
        v.get(field)
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
    };
    Some(match v.get("type")?.as_str()? {
        "snapshot" => SessionMsg::Snapshot {
            cols: num("cols")?,
            rows: num("rows")?,
            bytes: v
                .get("bytes")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0),
        },
        "size" => SessionMsg::Size {
            cols: num("cols")?,
            rows: num("rows")?,
        },
        "exit" => SessionMsg::Exit(
            match (
                v.get("signal").and_then(Value::as_str),
                v.get("code").and_then(Value::as_i64),
            ) {
                (Some(signal), _) => format!("killed ({signal})"),
                (None, Some(code)) => format!("exited {code}"),
                (None, None) => "exited".into(),
            },
        ),
        "error" => SessionMsg::Error(
            v.get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_owned(),
        ),
        // A lagging client is re-sent a snapshot: its header follows.
        _ => return None,
    })
}

/// Keep [`HubView`] current from `/api/events` (reconnecting), and tell
/// the hub which session this client views.
async fn events_task(
    endpoint: Arc<Endpoint>,
    view: Arc<Mutex<HubView>>,
    tx: Sender<Ev>,
    mut viewing: watch::Receiver<Option<String>>,
) {
    let client = stable_client_id("attach");
    let path = format!("/api/events?client={client}");
    let update = |change: &dyn Fn(&mut HubView)| {
        let mut v = view.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut v);
        drop(v);
        tx.send(Ev::Hub).is_ok()
    };
    loop {
        let mut socket = match endpoint.websocket(&path).await {
            Ok(socket) => socket,
            Err(message) => {
                if !update(&|v| {
                    v.online = false;
                    v.error = Some(message.clone());
                }) {
                    return;
                }
                if endpoint.reload_token() != Some(true) {
                    tokio::time::sleep(EVENTS_RETRY).await;
                }
                continue;
            }
        };
        let current = viewing.borrow_and_update().clone();
        let _ = socket.send(view_message(current.as_deref())).await;
        let lists = refresh_lists(&endpoint).await;
        if !update(&|v| {
            v.online = true;
            v.error = None;
            if let Ok((sessions, workspaces)) = &lists {
                v.sessions.clone_from(sessions);
                v.workspaces.clone_from(workspaces);
            }
        }) {
            return;
        }
        loop {
            tokio::select! {
                changed = viewing.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let current = viewing.borrow_and_update().clone();
                    if socket.send(view_message(current.as_deref())).await.is_err() {
                        break;
                    }
                }
                message = socket.next() => {
                    let text = match message {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(_)) => continue,
                    };
                    let Ok(event) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    let alive = match event.get("type").and_then(Value::as_str) {
                        Some("snapshot") => update(&|v| apply_snapshot(v, &event)),
                        Some("status") => update(&|v| apply_status(v, &event)),
                        Some("sessions") => match refresh_lists(&endpoint).await {
                            Ok((sessions, workspaces)) => update(&|v| {
                                v.sessions.clone_from(&sessions);
                                v.workspaces.clone_from(&workspaces);
                            }),
                            Err(_) => true,
                        },
                        _ => true,
                    };
                    if !alive {
                        return;
                    }
                }
            }
        }
        if !update(&|v| {
            v.online = false;
            v.error = Some("events connection lost; retrying".into());
        }) {
            return;
        }
        // A rotated token (read from the state dir): reconnect with the
        // new one at once.
        if endpoint.reload_token() != Some(true) {
            tokio::time::sleep(EVENTS_RETRY).await;
        }
    }
}

fn view_message(session: Option<&str>) -> Message {
    Message::Text(
        json!({ "type": "view", "session_id": session })
            .to_string()
            .into(),
    )
}

async fn refresh_lists(endpoint: &Endpoint) -> Result<(Vec<Value>, Vec<Value>), String> {
    Ok((endpoint.sessions().await?, endpoint.workspaces().await?))
}

fn status_name(view: &Value) -> Option<String> {
    view.get("status")?.as_str().map(str::to_owned)
}

fn apply_snapshot(v: &mut HubView, event: &Value) {
    v.status = event
        .get("sessions")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(id, view)| Some((id.clone(), status_name(view)?)))
                .collect()
        })
        .unwrap_or_default();
    v.workspace_status = event
        .get("workspaces")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(id, s)| Some((id.clone(), s.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    v.graded = event
        .get("graded")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    v.fanouts = event
        .get("fanouts")
        .and_then(Value::as_object)
        .map(|m| m.values().filter_map(Value::as_u64).sum::<u64>())
        .unwrap_or(0)
        .try_into()
        .unwrap_or(usize::MAX);
}

fn apply_status(v: &mut HubView, event: &Value) {
    let Some(id) = event.get("session_id").and_then(Value::as_str) else {
        return;
    };
    match event.get("status").and_then(status_name) {
        Some(status) => {
            v.status.insert(id.to_owned(), status);
        }
        None => {
            v.status.remove(id);
        }
    }
    if let Some(ws) = event.get("workspace_id").and_then(Value::as_str) {
        match event.get("workspace_status").and_then(Value::as_str) {
            Some(status) => {
                v.workspace_status.insert(ws.to_owned(), status.to_owned());
            }
            None => {
                v.workspace_status.remove(ws);
            }
        }
    }
}

/// SIGWINCH -> [`Ev::Winch`]; SIGTERM / SIGHUP / SIGQUIT end the client.
async fn signals_task(tx: Sender<Ev>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut winch), Ok(mut term), Ok(mut hup), Ok(mut quit)) = (
        signal(SignalKind::window_change()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
        signal(SignalKind::quit()),
    ) else {
        return;
    };
    loop {
        let ev = tokio::select! {
            _ = winch.recv() => Ev::Winch,
            _ = term.recv() => Ev::Terminate("terminated (SIGTERM)".into()),
            _ = hup.recv() => Ev::Terminate("hung up (SIGHUP)".into()),
            _ = quit.recv() => Ev::Terminate("quit (SIGQUIT)".into()),
        };
        if tx.send(ev).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_events_update_the_view_and_counts() {
        let mut v = HubView::default();
        apply_snapshot(
            &mut v,
            &json!({ "type": "snapshot",
                "sessions": { "a": { "status": "blocked" }, "b": { "status": "done" }, "c": { "status": "working" } },
                "workspaces": { "w": "blocked" } }),
        );
        assert_eq!(status_counts(&v), (1, 1));
        apply_status(
            &mut v,
            &json!({ "type": "status", "session_id": "a", "status": { "status": "working" },
                "workspace_id": "w", "workspace_status": "working" }),
        );
        assert_eq!(status_counts(&v), (0, 1));
        assert_eq!(v.workspace_status["w"], "working");
        apply_status(
            &mut v,
            &json!({ "type": "status", "session_id": "b", "status": null }),
        );
        assert_eq!(status_counts(&v), (0, 0));
    }

    #[test]
    fn graded_sessions_and_fanouts_count_as_needs_you_lists_them() {
        let mut v = HubView::default();
        // Two candidates finished (the hub's to grade), one plain session
        // finished, and the fan-out's grading is unseen: two done.
        apply_snapshot(
            &mut v,
            &json!({ "type": "snapshot",
                "sessions": { "c1": { "status": "done" }, "c2": { "status": "done" },
                    "s": { "status": "done" } },
                "workspaces": { "w": "done" },
                "graded": ["c1", "c2"],
                "fanouts": { "w": 1 } }),
        );
        assert_eq!(status_counts(&v), (0, 2));
        // Looked at: the next snapshot no longer lists it.
        apply_snapshot(
            &mut v,
            &json!({ "type": "snapshot",
                "sessions": { "c1": { "status": "done" }, "c2": { "status": "done" },
                    "s": { "status": "idle" } },
                "workspaces": { "w": "idle" },
                "graded": ["c1", "c2"],
                "fanouts": {} }),
        );
        assert_eq!(status_counts(&v), (0, 0));
    }

    #[test]
    fn session_messages_parse() {
        assert!(matches!(
            parse_text(r#"{"type":"snapshot","cols":80,"rows":23,"alt_screen":false,"bytes":12}"#),
            Some(SessionMsg::Snapshot {
                cols: 80,
                rows: 23,
                bytes: 12
            })
        ));
        assert!(matches!(
            parse_text(r#"{"type":"exit","code":3,"signal":null}"#),
            Some(SessionMsg::Exit(how)) if how == "exited 3"
        ));
        assert!(matches!(
            parse_text(r#"{"type":"exit","code":null,"signal":"Hangup"}"#),
            Some(SessionMsg::Exit(how)) if how == "killed (Hangup)"
        ));
        assert!(parse_text(r#"{"type":"reset"}"#).is_none());
        assert!(parse_text("junk").is_none());
    }

    #[test]
    fn the_bar_fits_the_row_and_restores_the_region() {
        let text = BarText {
            left: " ws/agent [done] ".into(),
            blocked: 2,
            done: 1,
            right: " C-b ? help ".into(),
        };
        let mut out = Vec::new();
        bar_bytes(&mut out, (40, 24), 23, &text);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[24;1H"));
        assert!(s.ends_with("\x1b[K\x1b[0m\x1b[1;23r"));
        // Visible text: exactly cols - 1 columns.
        let visible: String = strip_escapes(&s);
        assert_eq!(display_width(&visible), 39, "{visible:?}");
        assert!(visible.contains("2 blocked") && visible.contains("1 done"));
        // Narrow terminals truncate rather than wrap.
        let mut out = Vec::new();
        bar_bytes(&mut out, (10, 5), 4, &text);
        assert_eq!(
            display_width(&strip_escapes(&String::from_utf8(out).unwrap())),
            9
        );
    }

    fn strip_escapes(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                match chars.next() {
                    Some('[') => {
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\x07' {
                                break;
                            }
                            if c == '\x1b' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some('(') => {
                        chars.next();
                    }
                    _ => {}
                }
            } else if !c.is_control() {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn truncation_counts_columns() {
        assert_eq!(truncate("ab\u{4e16}cd", 3), "ab");
        assert_eq!(truncate("ab\u{4e16}cd", 4), "ab\u{4e16}");
        assert_eq!(truncate("a\x1b[1mb", 9), "a[1mb");
    }
}
