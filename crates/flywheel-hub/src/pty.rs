//! One server-owned PTY session: a child process on a pseudo-terminal, a
//! terminal emulator tracking its screen (bounded scrollback), and a
//! broadcast of live output to every attached client.
//!
//! Threads per session (all blocking, none on the async runtime):
//!
//! * **reader** -- drains the PTY master into the emulator and the
//!   broadcast. Emulating and broadcasting happen under one lock, and
//!   [`PtySession::attach`] snapshots + subscribes under that same lock, so an
//!   attaching client sees the state up to some byte, then every later byte
//!   exactly once: snapshot, then live. The emulator's replies to terminal
//!   queries are queued as input (the server is the only answerer, so a
//!   detached session gets them, and once however many clients watch).
//! * **writer** -- owns the PTY writer and drains a queue, so a child that
//!   stops reading its stdin can never block an HTTP handler.
//! * **waiter** -- reaps the child, lets the reader drain the final output,
//!   then records the exit status and broadcasts [`SessionEvent::Exit`].
//!
//! The child is a session leader (portable-pty `setsid`s it), so its process
//! group id and session id are its pid. [`PtySession::kill`] signals the
//! leader's group and every other process still in its session -- background
//! jobs a shell put in their own groups, and processes that outlived the
//! leader -- with SIGHUP, then SIGKILL after a grace period.
//!
//! [`PtySession::release`] stops the reader and writer threads even while
//! some escaped process (one that `setsid`ed away but kept the terminal open)
//! still holds the PTY, so a removed session's fds and threads are freed.

use crate::vt::{Emulator, SNAPSHOT_HISTORY_BYTES, ScreenText, TextSource, ViewerColors};
use axum::body::Bytes;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Live-output broadcast depth (chunks). A subscriber that falls further
/// behind than this receives a fresh snapshot instead (see [`Attachment`]).
const BROADCAST_DEPTH: usize = 1024;
/// How long the waiter lets the reader drain output after the child exits.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);
/// Terminal size bounds accepted from clients.
pub const MAX_DIMENSION: u16 = 1000;
/// Input bytes queued for a child that is not reading its stdin before
/// further input is refused.
pub const INPUT_QUEUE_LIMIT: usize = 4 * 1024 * 1024;
/// How often the reader and writer threads re-check [`PtySession::release`].
const RELEASE_POLL: Duration = Duration::from_millis(200);
/// Largest single write to the PTY after it reports writable.
const WRITE_SLICE: usize = 512;

/// How a session's child process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    /// The exit code, when the process exited normally.
    pub code: Option<i32>,
    /// The terminating signal's name, when it was killed by one.
    pub signal: Option<String>,
}

impl ExitInfo {
    fn from_status(status: &portable_pty::ExitStatus) -> Self {
        match status.signal() {
            Some(signal) => Self {
                code: None,
                signal: Some(signal.to_owned()),
            },
            None => Self {
                code: Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
                signal: None,
            },
        }
    }
}

/// One event on a session's live broadcast.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Raw PTY output.
    Output(Bytes),
    /// The child exited; no further output follows.
    Exit(ExitInfo),
    /// A viewer resized the terminal (in order with the output: what
    /// follows is drawn at this size).
    Resize { cols: u16, rows: u16 },
}

/// What to spawn.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// argv; `argv[0]` is resolved on `PATH`.
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Extra environment on top of the hub's own.
    pub env: Vec<(String, String)>,
    /// Variables of the hub's own environment the child must not inherit.
    pub env_remove: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    /// Scrollback lines the emulator keeps (see [`crate::vt`]).
    pub scrollback_lines: usize,
}

/// Failure to start a session.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("empty command")]
    EmptyCommand,
    #[error("failed to open a pty: {0}")]
    OpenPty(String),
    #[error("failed to spawn {program}: {reason}")]
    Spawn { program: String, reason: String },
}

/// What happened to input handed to [`PtySession::write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Queued for the child's stdin.
    Queued,
    /// The session has exited (or was released); the input was discarded.
    Closed,
    /// The child has [`INPUT_QUEUE_LIMIT`] bytes of unread input queued;
    /// this input was discarded.
    Backlogged,
}

/// A point-in-time attach: a snapshot of the terminal plus a live receiver
/// positioned exactly after the output it reflects.
pub struct Attachment {
    /// Escape sequences rebuilding the terminal (scrollback, screens,
    /// cursor, modes) in a freshly reset client.
    pub replay: Vec<u8>,
    /// The terminal size the snapshot was taken at.
    pub cols: u16,
    pub rows: u16,
    pub alt_screen: bool,
    pub exit: Option<ExitInfo>,
    pub events: broadcast::Receiver<SessionEvent>,
}

struct Shared {
    screen: Emulator,
    exit: Option<ExitInfo>,
    /// The leader has been reaped (its pid may be recycled from here on).
    reaped: bool,
    /// Output bytes read from the child so far.
    output_bytes: u64,
    size: PtySize,
}

/// State shared with the writer thread.
struct InputQueue {
    queued: AtomicUsize,
    released: AtomicBool,
}

/// A running (or exited) PTY session.
pub struct PtySession {
    shared: Mutex<Shared>,
    exited: Condvar,
    events: broadcast::Sender<SessionEvent>,
    input: std_mpsc::Sender<Vec<u8>>,
    queue: Arc<InputQueue>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    pid: Option<u32>,
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtySession")
            .field("pid", &self.pid)
            .field("exit", &self.exit_info())
            .finish()
    }
}

fn clamp_dimension(value: u16) -> u16 {
    value.clamp(1, MAX_DIMENSION)
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: clamp_dimension(rows),
        cols: clamp_dimension(cols),
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl PtySession {
    /// Spawn `spec` on a fresh PTY. `on_exit` runs once, on the waiter
    /// thread, after the exit status is recorded and broadcast.
    pub fn spawn(
        spec: SpawnSpec,
        on_exit: impl FnOnce(ExitInfo) + Send + 'static,
    ) -> Result<Arc<Self>, SpawnError> {
        let program = spec.argv.first().cloned().ok_or(SpawnError::EmptyCommand)?;
        let size = pty_size(spec.cols, spec.rows);
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| SpawnError::OpenPty(e.to_string()))?;
        let mut cmd = CommandBuilder::from_argv(spec.argv.iter().map(Into::into).collect());
        cmd.cwd(&spec.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for key in &spec.env_remove {
            cmd.env_remove(key);
        }
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| SpawnError::Spawn {
                program: program.clone(),
                reason: e.to_string(),
            })?;
        // The child holds its own copy of the slave; dropping ours lets the
        // master read EOF once the child (and its descendants) close it.
        drop(pair.slave);
        let spawn_err = |reason: String| SpawnError::Spawn {
            program: program.clone(),
            reason,
        };
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| spawn_err(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| spawn_err(e.to_string()))?;
        let pid = child.process_id();
        let fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| spawn_err("the pty master has no file descriptor".into()))?;
        let (events, _) = broadcast::channel(BROADCAST_DEPTH);
        let (input, input_rx) = std_mpsc::channel::<Vec<u8>>();
        let queue = Arc::new(InputQueue {
            queued: AtomicUsize::new(0),
            released: AtomicBool::new(false),
        });
        let session = Arc::new(Self {
            shared: Mutex::new(Shared {
                screen: Emulator::new(size.cols, size.rows, spec.scrollback_lines),
                exit: None,
                reaped: false,
                output_bytes: 0,
                size,
            }),
            exited: Condvar::new(),
            events,
            input,
            queue: Arc::clone(&queue),
            master: Mutex::new(pair.master),
            pid,
        });

        let (drained_tx, drained_rx) = std_mpsc::channel::<()>();
        let reader_session = Arc::clone(&session);
        thread::Builder::new()
            .name("hub-pty-reader".into())
            .spawn(move || {
                reader_session.read_loop(fd, reader);
                let _ = drained_tx.send(());
            })
            .map_err(|e| spawn_err(e.to_string()))?;

        thread::Builder::new()
            .name("hub-pty-writer".into())
            .spawn(move || write_loop(fd, writer, &input_rx, &queue))
            .map_err(|e| spawn_err(e.to_string()))?;

        let waiter_session = Arc::clone(&session);
        thread::Builder::new()
            .name("hub-pty-waiter".into())
            .spawn(move || {
                let info = match child.wait() {
                    Ok(status) => ExitInfo::from_status(&status),
                    Err(_) => ExitInfo {
                        code: None,
                        signal: None,
                    },
                };
                waiter_session.lock().reaped = true;
                // Let the reader flush the child's final output first so
                // the exit event is ordered after it.
                let _ = drained_rx.recv_timeout(DRAIN_AFTER_EXIT);
                waiter_session.record_exit(info.clone());
                on_exit(info);
            })
            .map_err(|e| spawn_err(e.to_string()))?;

        Ok(session)
    }

    /// Drain the master until EOF or [`PtySession::release`]. `fd` is the
    /// master (kept open by `self`); `reader` is a dup of it, so readiness on
    /// one is readiness on the other.
    fn read_loop(&self, fd: libc::c_int, mut reader: Box<dyn Read + Send>) {
        let mut buf = vec![0u8; 64 * 1024];
        while wait_ready(fd, libc::POLLIN, &self.queue.released) {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = Bytes::copy_from_slice(&buf[..n]);
                    let replies = {
                        let mut shared = self.lock();
                        shared.screen.process(&chunk);
                        shared.output_bytes += chunk.len() as u64;
                        // No receivers is not an error: output is retained.
                        let _ = self.events.send(SessionEvent::Output(chunk));
                        shared.screen.take_replies()
                    };
                    if !replies.is_empty() {
                        // Dropped like any input when the child is not
                        // reading (or gone).
                        let _ = self.write(replies);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // EIO is how Linux reports "slave side closed".
                Err(_) => break,
            }
        }
    }

    fn record_exit(&self, info: ExitInfo) {
        let mut shared = self.lock();
        shared.exit = Some(info.clone());
        let _ = self.events.send(SessionEvent::Exit(info));
        self.exited.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Snapshot the terminal (scrollback up to [`SNAPSHOT_HISTORY_BYTES`])
    /// and subscribe to live events atomically. Takes milliseconds; call
    /// off async workers.
    pub fn attach(&self) -> Attachment {
        self.attach_with(SNAPSHOT_HISTORY_BYTES)
    }

    /// [`PtySession::attach`] with a scrollback byte budget for the
    /// snapshot (0: the screen only).
    pub fn attach_with(&self, history_budget: usize) -> Attachment {
        let mut shared = self.lock();
        Attachment {
            replay: shared.screen.snapshot(history_budget),
            cols: shared.size.cols,
            rows: shared.size.rows,
            alt_screen: shared.screen.alt_screen(),
            exit: shared.exit.clone(),
            events: self.events.subscribe(),
        }
    }

    /// Queue `bytes` for the child's stdin, refusing (and discarding) input
    /// once the session has exited or while [`INPUT_QUEUE_LIMIT`] bytes the
    /// child has not read are already queued.
    pub fn write(&self, bytes: Vec<u8>) -> WriteOutcome {
        if bytes.is_empty() {
            return WriteOutcome::Queued;
        }
        if self.lock().exit.is_some() || self.queue.released.load(Ordering::Acquire) {
            return WriteOutcome::Closed;
        }
        let len = bytes.len();
        let reserved =
            self.queue
                .queued
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    // A single message larger than the limit is still accepted
                    // into an empty queue (a big paste), never on top of one.
                    (queued == 0 || queued + len <= INPUT_QUEUE_LIMIT).then_some(queued + len)
                });
        if reserved.is_err() {
            return WriteOutcome::Backlogged;
        }
        if self.input.send(bytes).is_err() {
            self.queue.queued.fetch_sub(len, Ordering::AcqRel);
            return WriteOutcome::Closed;
        }
        WriteOutcome::Queued
    }

    /// Input bytes queued but not yet written to the PTY.
    pub fn queued_input(&self) -> usize {
        self.queue.queued.load(Ordering::Acquire)
    }

    /// Resize the terminal (clamped to `1..=MAX_DIMENSION`). The kernel
    /// delivers SIGWINCH to the foreground process group.
    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let size = pty_size(cols, rows);
        {
            let mut shared = self.lock();
            if shared.exit.is_some() {
                return Ok(());
            }
            let changed = shared.size.cols != size.cols || shared.size.rows != size.rows;
            shared.size = size;
            shared.screen.resize(size.cols, size.rows);
            if changed {
                // Under the lock, like output, so every viewer hears of it
                // between the same two chunks.
                let _ = self.events.send(SessionEvent::Resize {
                    cols: size.cols,
                    rows: size.rows,
                });
            }
        }
        self.master
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resize(size)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    /// Current `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let shared = self.lock();
        (shared.size.cols, shared.size.rows)
    }

    /// The child's pid (its process-group id).
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The exit status, once the child has exited.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.lock().exit.clone()
    }

    /// Clients currently subscribed to live output.
    pub fn subscriber_count(&self) -> usize {
        self.events.receiver_count()
    }

    /// Scrollback lines retained by the active screen.
    pub fn scrollback_lines(&self) -> usize {
        self.lock().screen.history_lines()
    }

    /// Answer colour queries with a viewer's theme from now on (see
    /// [`ViewerColors`]).
    pub fn set_viewer_colors(&self, colors: ViewerColors) {
        self.lock().screen.set_viewer_colors(colors);
    }

    /// Output bytes the child has written so far (a cheap "is it still
    /// drawing" probe).
    pub fn output_bytes(&self) -> u64 {
        self.lock().output_bytes
    }

    /// Whether the program in the terminal enabled bracketed paste.
    pub fn bracketed_paste(&self) -> bool {
        self.lock().screen.bracketed_paste()
    }

    /// The terminal's rendered plain text.
    pub fn screen_text(&self, source: TextSource) -> ScreenText {
        self.lock().screen.text(source)
    }

    /// Hang up the session -- the leader's process group and every process
    /// still in its session, whether or not the leader has exited --
    /// escalating to SIGKILL for anything still alive after `grace`. Returns
    /// immediately; the escalation runs on a background thread.
    pub fn kill(self: &Arc<Self>, grace: Duration) {
        if !self.signal_session(libc::SIGHUP) {
            return;
        }
        let session = Arc::clone(self);
        let _ = thread::Builder::new()
            .name("hub-pty-kill".into())
            .spawn(move || {
                if !session.wait_quiet(grace) {
                    session.signal_session(libc::SIGKILL);
                }
            });
    }

    /// Stop the reader and writer threads and refuse further input. The
    /// threads notice within [`RELEASE_POLL`]; once they and every other
    /// holder drop their `Arc`, the PTY master is closed. Use after
    /// [`PtySession::kill`] when the session is being forgotten.
    pub fn release(&self) {
        self.queue.released.store(true, Ordering::Release);
    }

    /// Whether [`PtySession::release`] has been called.
    pub fn is_released(&self) -> bool {
        self.queue.released.load(Ordering::Acquire)
    }

    /// Every process still in this session, other than an already-reaped
    /// leader. Empty off Linux (only the leader's group is reachable there).
    pub fn session_processes(&self) -> Vec<u32> {
        let Some(sid) = self.pid else {
            return Vec::new();
        };
        let reaped = self.lock().reaped;
        session_members(sid)
            .into_iter()
            .filter(|&p| !(reaped && p == sid))
            .collect()
    }

    /// Send `signal` to the leader's process group (while the leader is
    /// unreaped, so its pid cannot have been recycled) and to every other
    /// process in its session. Returns whether anything was signalled.
    fn signal_session(&self, signal: libc::c_int) -> bool {
        let Some(pid) = self.pid.and_then(|p| libc::pid_t::try_from(p).ok()) else {
            return false;
        };
        let mut sent = false;
        {
            let shared = self.lock();
            if !shared.reaped {
                // SAFETY: kill(2) has no memory-safety preconditions. A
                // negative pid addresses the group the session leader made.
                let rc = unsafe { libc::kill(-pid, signal) };
                if rc != 0 {
                    // The group may already be gone; fall back to the leader.
                    // SAFETY: as above.
                    unsafe { libc::kill(pid, signal) };
                }
                sent = true;
            }
        }
        for member in self.session_processes() {
            let Ok(member) = libc::pid_t::try_from(member) else {
                continue;
            };
            // SAFETY: as above; `member` was in this session when listed.
            if unsafe { libc::kill(member, signal) } == 0 {
                sent = true;
            }
        }
        sent
    }

    /// Wait up to `timeout` for the leader to exit and the rest of the
    /// session to go away; returns whether both happened.
    fn wait_quiet(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        if !self.wait_exit(timeout) {
            return false;
        }
        loop {
            if self.session_processes().is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Block until the child has exited or `timeout` elapses; returns whether
    /// it exited.
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        let shared = self.lock();
        let (shared, _) = self
            .exited
            .wait_timeout_while(shared, timeout, |s| s.exit.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        shared.exit.is_some()
    }
}

/// Block until `fd` reports `events` (or an error/hangup, which the next
/// read or write surfaces), checking `released` every [`RELEASE_POLL`].
/// Returns false once released.
fn wait_ready(fd: libc::c_int, events: libc::c_short, released: &AtomicBool) -> bool {
    let timeout = libc::c_int::try_from(RELEASE_POLL.as_millis()).unwrap_or(200);
    loop {
        if released.load(Ordering::Acquire) {
            return false;
        }
        let mut pfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid pollfd for the duration of the call and
        // nfds is 1. `fd` stays open: the session owning it outlives us.
        let rc = unsafe { libc::poll(&raw mut pfd, 1, timeout) };
        if rc > 0 {
            return true;
        }
        if rc < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // poll itself failed: let the read/write report the error.
            return true;
        }
    }
}

/// Drain the input queue into the PTY. Writes go out in small slices, each
/// after the PTY reports writable, so a child that stops reading parks this
/// thread in `poll` (where it can see `release`) rather than in `write`.
fn write_loop(
    fd: libc::c_int,
    mut writer: Box<dyn Write + Send>,
    input: &std_mpsc::Receiver<Vec<u8>>,
    queue: &InputQueue,
) {
    loop {
        if queue.released.load(Ordering::Acquire) {
            return;
        }
        let bytes = match input.recv_timeout(RELEASE_POLL) {
            Ok(bytes) => bytes,
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let len = bytes.len();
        let mut ok = true;
        for slice in bytes.chunks(WRITE_SLICE) {
            if !wait_ready(fd, libc::POLLOUT, &queue.released)
                || writer
                    .write_all(slice)
                    .and_then(|()| writer.flush())
                    .is_err()
            {
                ok = false;
                break;
            }
        }
        queue.queued.fetch_sub(len, Ordering::AcqRel);
        if !ok {
            return;
        }
    }
}

/// Live (non-zombie) pids whose session id is `sid` (Linux: from
/// `/proc/<pid>/stat`).
#[cfg(target_os = "linux")]
fn session_members(sid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat_session(&stat))
                .is_some_and(|(state, session)| session == sid && state != 'Z' && state != 'X')
        })
        .collect()
}

/// The state and session id fields of a `/proc/<pid>/stat` line. The
/// command name (field 2) is parenthesised and may contain spaces or parens,
/// so fields are counted from the last `)`: state, ppid, pgrp, session.
#[cfg(target_os = "linux")]
fn stat_session(stat: &str) -> Option<(char, u32)> {
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    let session = fields.nth(2)?.parse().ok()?;
    Some((state, session))
}

#[cfg(not(target_os = "linux"))]
fn session_members(_sid: u32) -> Vec<u32> {
    Vec::new()
}

// --- typing into a terminal ------------------------------------------------

/// Pause between sent text and the Enter that follows it, so a program
/// reads the Enter as a keypress rather than as part of the text (TUIs
/// treat one fast burst of input as a paste).
pub const ENTER_DELAY: Duration = Duration::from_millis(50);
const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// The bytes a terminal sends when `text` is pasted: line breaks become
/// `CR` (what Enter sends); with `bracketed` the text is wrapped in paste
/// markers, minus any markers inside it (which would end the paste early
/// and let the rest run as typed input). Removal repeats until none is
/// left: removing one marker must not join the bytes around it into
/// another (`ESC[20` + `ESC[201~` + `1~`).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if !bracketed {
        return text.into_bytes();
    }
    let mut inner = text;
    loop {
        let stripped = inner
            .replace(BRACKETED_PASTE_START, "")
            .replace(BRACKETED_PASTE_END, "");
        if stripped == inner {
            break;
        }
        inner = stripped;
    }
    format!("{BRACKETED_PASTE_START}{inner}{BRACKETED_PASTE_END}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn spec(script: &str) -> SpawnSpec {
        SpawnSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env: vec![("FW_HUB_TEST".into(), "marker-value".into())],
            env_remove: Vec::new(),
            cols: 80,
            rows: 24,
            scrollback_lines: 1000,
        }
    }

    /// Collect live output until `needle` appears or the deadline passes.
    fn read_until(att: &mut Attachment, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut seen = String::from_utf8_lossy(&att.replay).into_owned();
        while !seen.contains(needle) && Instant::now() < deadline {
            match att.events.try_recv() {
                Ok(SessionEvent::Output(bytes)) => seen.push_str(&String::from_utf8_lossy(&bytes)),
                Ok(SessionEvent::Exit(_)) => break,
                Ok(SessionEvent::Resize { .. }) => {}
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        seen
    }

    #[test]
    fn captures_output_env_and_exit_code() {
        let (tx, rx) = std_mpsc::channel();
        let session = PtySession::spawn(spec("echo out-$FW_HUB_TEST; exit 7"), move |info| {
            tx.send(info).unwrap();
        })
        .unwrap();
        let info = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(info.code, Some(7));
        assert_eq!(info.signal, None);
        assert_eq!(session.exit_info(), Some(info));
        let att = session.attach();
        assert!(String::from_utf8_lossy(&att.replay).contains("out-marker-value"));
        assert!(att.exit.is_some());
        assert_eq!(session.write(b"ignored".to_vec()), WriteOutcome::Closed);
    }

    #[test]
    fn removed_variables_are_not_inherited() {
        // HOME is in every test environment worth the name.
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let (tx, rx) = std_mpsc::channel();
        let mut spec = spec("echo home=[${HOME-unset}] marker=[$FW_HUB_TEST]");
        spec.env_remove = vec!["HOME".into()];
        let session = PtySession::spawn(spec, move |info| {
            tx.send(info).unwrap();
        })
        .unwrap();
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let replay = String::from_utf8_lossy(&session.attach().replay).into_owned();
        assert!(
            replay.contains("home=[unset] marker=[marker-value]"),
            "{replay:?}"
        );
    }

    #[test]
    fn input_reaches_the_child_and_output_is_broadcast_to_every_subscriber() {
        let session =
            PtySession::spawn(spec("read line; echo got:$line; sleep 30"), |_| {}).expect("spawn");
        let mut first = session.attach();
        let mut second = session.attach();
        assert_eq!(session.subscriber_count(), 2);
        assert_eq!(session.write(b"ping\r".to_vec()), WriteOutcome::Queued);
        assert!(read_until(&mut first, "got:ping", Duration::from_secs(10)).contains("got:ping"));
        assert!(read_until(&mut second, "got:ping", Duration::from_secs(10)).contains("got:ping"));
        // A late attach replays what both live subscribers saw.
        let late = session.attach();
        assert!(String::from_utf8_lossy(&late.replay).contains("got:ping"));
        session.kill(Duration::from_secs(2));
        assert!(session.wait_exit(Duration::from_secs(10)));
    }

    #[test]
    fn terminal_queries_are_answered_once_with_or_without_viewers() {
        // Raw mode; ask for status and the cursor position; give a second
        // (duplicate) answer time to arrive, then show what came back.
        let script = "stty raw -echo min 0 time 5; printf 'xy\\033[5n\\033[6n'; sleep 1; \
                      r=$(dd bs=256 count=1 2>/dev/null); stty sane; \
                      printf '\\nreply:'; printf '%s' \"$r\" | od -An -c | tr -s ' '; \
                      echo end; sleep 30";
        for viewers in [0, 2] {
            let session = PtySession::spawn(spec(script), |_| {}).unwrap();
            let held: Vec<Attachment> = (0..viewers).map(|_| session.attach()).collect();
            let deadline = Instant::now() + Duration::from_secs(10);
            let text = loop {
                let text = session
                    .screen_text(TextSource::Recent { lines: 10 })
                    .lines
                    .join("\n");
                if text.contains("end") || Instant::now() >= deadline {
                    break text;
                }
                thread::sleep(Duration::from_millis(50));
            };
            assert!(
                text.contains("reply: 033 [ 0 n 033 [ 1 ; 3 R\nend"),
                "{viewers} viewers: {text}"
            );
            drop(held);
            session.kill(Duration::from_secs(2));
            assert!(session.wait_exit(Duration::from_secs(10)));
        }
    }

    #[test]
    fn resize_is_visible_to_the_child() {
        let session = PtySession::spawn(spec("read _; stty size; sleep 30"), |_| {}).unwrap();
        session.resize(132, 43).unwrap();
        assert_eq!(session.size(), (132, 43));
        let mut att = session.attach();
        session.write(b"\r".to_vec());
        assert!(read_until(&mut att, "43 132", Duration::from_secs(10)).contains("43 132"));
        session.kill(Duration::from_secs(2));
        assert!(session.wait_exit(Duration::from_secs(10)));
    }

    #[test]
    fn kill_escalates_when_sighup_is_ignored() {
        let session = PtySession::spawn(
            spec("trap '' HUP; echo armed; while :; do sleep 1; done"),
            |_| {},
        )
        .unwrap();
        let mut att = session.attach();
        assert!(read_until(&mut att, "armed", Duration::from_secs(10)).contains("armed"));
        session.kill(Duration::from_millis(300));
        assert!(session.wait_exit(Duration::from_secs(10)));
        let info = session.exit_info().unwrap();
        assert_eq!(info.code, None);
        assert!(info.signal.is_some(), "{info:?}");
        // A second kill on an exited session is a no-op.
        session.kill(Duration::from_millis(10));
    }

    #[test]
    fn empty_command_and_missing_program_are_errors() {
        let mut empty = spec("true");
        empty.argv.clear();
        assert!(matches!(
            PtySession::spawn(empty, |_| {}),
            Err(SpawnError::EmptyCommand)
        ));
        let mut missing = spec("true");
        missing.argv = vec!["/nonexistent/flywheel-hub-test-binary".into()];
        assert!(matches!(
            PtySession::spawn(missing, |_| {}),
            Err(SpawnError::Spawn { .. })
        ));
    }

    #[cfg(target_os = "linux")]
    fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        done()
    }

    #[cfg(target_os = "linux")]
    fn marked_pid(text: &str, marker: &str) -> u32 {
        let start = text.find(marker).expect("marker") + marker.len();
        text[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .expect("pid")
    }

    #[cfg(target_os = "linux")]
    fn pid_alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat_session(&stat))
            .is_some_and(|(state, _)| state != 'Z' && state != 'X')
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kill_after_the_leader_exits_reaches_its_background_jobs_and_frees_the_session() {
        // Job control puts the background job in its own process group, so
        // only a session-wide kill reaches it once the leader is gone.
        let session =
            PtySession::spawn(spec("set -m; sleep 300 & echo bg=$!; exit 0"), |_| {}).unwrap();
        let mut att = session.attach();
        let seen = read_until(&mut att, "bg=", Duration::from_secs(10));
        assert!(session.wait_exit(Duration::from_secs(10)));
        let seen = seen + &read_until(&mut att, "\n", Duration::from_secs(2));
        let bg = marked_pid(&seen, "bg=");
        assert!(pid_alive(bg), "background job should outlive the shell");
        assert_eq!(session.session_processes(), vec![bg]);
        drop(att);
        session.kill(Duration::from_millis(300));
        assert!(
            wait_for(Duration::from_secs(10), || !pid_alive(bg)),
            "kill must reach the background job"
        );
        // With the slave closed the reader hits EOF; every thread lets go.
        assert!(
            wait_for(Duration::from_secs(10), || Arc::strong_count(&session) == 1),
            "threads still hold the session"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn release_frees_a_session_whose_terminal_an_escaped_process_holds() {
        let session = PtySession::spawn(
            spec("setsid sh -c 'echo esc=$$; exec sleep 300' & sleep 1; exit 0"),
            |_| {},
        )
        .unwrap();
        let mut att = session.attach();
        let seen = read_until(&mut att, "esc=", Duration::from_secs(10));
        let seen = seen + &read_until(&mut att, "\n", Duration::from_secs(2));
        let escaped = marked_pid(&seen, "esc=");
        drop(att);
        assert!(session.wait_exit(Duration::from_secs(10)));
        // The escaped process left the session: kill cannot reach it, and
        // it keeps the slave open, so the reader would block forever.
        session.kill(Duration::from_millis(100));
        thread::sleep(Duration::from_millis(500));
        assert!(Arc::strong_count(&session) > 1);
        session.release();
        assert_eq!(session.write(b"x".to_vec()), WriteOutcome::Closed);
        let freed = wait_for(Duration::from_secs(5), || Arc::strong_count(&session) == 1);
        // SAFETY: test cleanup of the process this test started.
        unsafe { libc::kill(libc::pid_t::try_from(escaped).unwrap(), libc::SIGKILL) };
        assert!(freed, "release must stop the reader and writer threads");
    }

    #[test]
    fn input_queue_is_bounded_while_the_child_is_not_reading() {
        let session = PtySession::spawn(spec("echo ready; sleep 300"), |_| {}).unwrap();
        let mut att = session.attach();
        assert!(read_until(&mut att, "ready", Duration::from_secs(10)).contains("ready"));
        let chunk = vec![b'a'; 1024 * 1024];
        let mut outcomes = Vec::new();
        for _ in 0..16 {
            outcomes.push(session.write(chunk.clone()));
        }
        assert!(outcomes.contains(&WriteOutcome::Backlogged), "{outcomes:?}");
        assert!(session.queued_input() <= INPUT_QUEUE_LIMIT + chunk.len());
        session.kill(Duration::from_millis(200));
        assert!(session.wait_exit(Duration::from_secs(10)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_session_reads_past_a_parenthesised_command_name() {
        let stat = "4242 (we(ird) name) S 1 4240 4241 34816 4240 4194560";
        assert_eq!(stat_session(stat), Some(('S', 4241)));
        assert_eq!(stat_session("garbage"), None);
    }

    #[test]
    fn pasted_text_uses_cr_and_cannot_escape_bracketed_paste() {
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(
            paste_bytes("x\n\x1b[201~rm -rf /\n", true),
            b"\x1b[200~x\rrm -rf /\r\x1b[201~"
        );
        assert_eq!(paste_bytes("", true), b"\x1b[200~\x1b[201~");
        // A marker split around another comes back together when the
        // inner one is removed; it must not survive either.
        for nested in [
            "x\x1b[20\x1b[201~1~rm -rf ~\n",
            "x\x1b[2\x1b[20\x1b[201~1~01~rm -rf ~\n",
            "x\x1b[20\x1b[200~1~rm -rf ~\n",
            "x\x1b[20\x1b[201~0~rm -rf ~\n",
        ] {
            let bytes = paste_bytes(nested, true);
            let inner = &bytes[6..bytes.len() - 6];
            assert!(
                !inner
                    .windows(6)
                    .any(|w| w == b"\x1b[201~" || w == b"\x1b[200~"),
                "{nested:?} -> {:?}",
                String::from_utf8_lossy(&bytes)
            );
            assert!(bytes.starts_with(b"\x1b[200~") && bytes.ends_with(b"\x1b[201~"));
        }
    }
}
