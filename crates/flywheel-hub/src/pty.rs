//! One server-owned PTY session: a child process on a pseudo-terminal, its
//! bounded scrollback, and a broadcast of live output to every attached
//! client.
//!
//! Threads per session (all blocking, none on the async runtime):
//!
//! * **reader** -- drains the PTY master into the scrollback and the
//!   broadcast. Appending and broadcasting happen under one lock, and
//!   [`PtySession::attach`] snapshots + subscribes under that same lock, so an
//!   attaching client sees every byte exactly once: replay, then live.
//! * **writer** -- owns the PTY writer and drains a queue, so a child that
//!   stops reading its stdin can never block an HTTP handler.
//! * **waiter** -- reaps the child, lets the reader drain the final output,
//!   then records the exit status and broadcasts [`SessionEvent::Exit`].
//!
//! The child is a session leader (portable-pty `setsid`s it), so its process
//! group id is its pid: [`PtySession::kill`] signals the whole group (SIGHUP,
//! then SIGKILL after a grace period).

use crate::scrollback::Scrollback;
use axum::body::Bytes;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;
use tokio::sync::broadcast;

/// Live-output broadcast depth (chunks). A subscriber that falls further
/// behind than this receives a fresh replay instead (see [`Attachment`]).
const BROADCAST_DEPTH: usize = 1024;
/// How long the waiter lets the reader drain output after the child exits.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);
/// Terminal size bounds accepted from clients.
pub const MAX_DIMENSION: u16 = 1000;

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
}

/// What to spawn.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// argv; `argv[0]` is resolved on `PATH`.
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Extra environment on top of the hub's own.
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_bytes: usize,
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

/// A point-in-time attach: everything retained so far plus a live receiver
/// positioned exactly after it.
pub struct Attachment {
    pub replay: Vec<u8>,
    pub exit: Option<ExitInfo>,
    pub events: broadcast::Receiver<SessionEvent>,
}

struct Shared {
    scrollback: Scrollback,
    exit: Option<ExitInfo>,
    size: PtySize,
}

/// A running (or exited) PTY session.
pub struct PtySession {
    shared: Mutex<Shared>,
    exited: Condvar,
    events: broadcast::Sender<SessionEvent>,
    input: std_mpsc::Sender<Vec<u8>>,
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
        let (events, _) = broadcast::channel(BROADCAST_DEPTH);
        let (input, input_rx) = std_mpsc::channel::<Vec<u8>>();
        let session = Arc::new(Self {
            shared: Mutex::new(Shared {
                scrollback: Scrollback::new(spec.scrollback_bytes),
                exit: None,
                size,
            }),
            exited: Condvar::new(),
            events,
            input,
            master: Mutex::new(pair.master),
            pid,
        });

        let (drained_tx, drained_rx) = std_mpsc::channel::<()>();
        let reader_session = Arc::clone(&session);
        thread::Builder::new()
            .name("hub-pty-reader".into())
            .spawn(move || {
                reader_session.read_loop(reader);
                let _ = drained_tx.send(());
            })
            .map_err(|e| spawn_err(e.to_string()))?;

        thread::Builder::new()
            .name("hub-pty-writer".into())
            .spawn(move || write_loop(writer, input_rx))
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
                // Let the reader flush the child's final output first so
                // the exit event is ordered after it.
                let _ = drained_rx.recv_timeout(DRAIN_AFTER_EXIT);
                waiter_session.record_exit(info.clone());
                on_exit(info);
            })
            .map_err(|e| spawn_err(e.to_string()))?;

        Ok(session)
    }

    fn read_loop(&self, mut reader: Box<dyn Read + Send>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = Bytes::copy_from_slice(&buf[..n]);
                    let mut shared = self.lock();
                    shared.scrollback.push(&chunk);
                    // No receivers is not an error: output is retained.
                    let _ = self.events.send(SessionEvent::Output(chunk));
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

    /// Snapshot the scrollback and subscribe to live events atomically.
    pub fn attach(&self) -> Attachment {
        let shared = self.lock();
        Attachment {
            replay: shared.scrollback.snapshot(),
            exit: shared.exit.clone(),
            events: self.events.subscribe(),
        }
    }

    /// Queue `bytes` for the child's stdin. Returns false once the session
    /// has exited (input is discarded).
    pub fn write(&self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if self.lock().exit.is_some() {
            return false;
        }
        self.input.send(bytes).is_ok()
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
            shared.size = size;
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

    /// Bytes currently retained in the scrollback.
    pub fn scrollback_len(&self) -> usize {
        self.lock().scrollback.len()
    }

    /// Hang up the session's process group, escalating to SIGKILL if it is
    /// still alive after `grace`. Returns immediately; the escalation runs on
    /// a background thread.
    pub fn kill(self: &Arc<Self>, grace: Duration) {
        if !self.signal_group(libc::SIGHUP) {
            return;
        }
        let session = Arc::clone(self);
        let _ = thread::Builder::new()
            .name("hub-pty-kill".into())
            .spawn(move || {
                if !session.wait_exit(grace) {
                    session.signal_group(libc::SIGKILL);
                }
            });
    }

    /// Signal the process group if the child has not been reaped yet (the
    /// pid cannot have been recycled while unreaped). Returns whether a
    /// signal was sent.
    fn signal_group(&self, signal: libc::c_int) -> bool {
        let shared = self.lock();
        if shared.exit.is_some() {
            return false;
        }
        let Some(pid) = self.pid.and_then(|p| libc::pid_t::try_from(p).ok()) else {
            return false;
        };
        // SAFETY: kill(2) has no memory-safety preconditions. A negative pid
        // addresses the process group the session leader created.
        let rc = unsafe { libc::kill(-pid, signal) };
        if rc != 0 {
            // The group may already be gone; fall back to the leader alone.
            // SAFETY: as above.
            unsafe { libc::kill(pid, signal) };
        }
        drop(shared);
        true
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

fn write_loop(mut writer: Box<dyn Write + Send>, input: std_mpsc::Receiver<Vec<u8>>) {
    while let Ok(bytes) = input.recv() {
        if writer
            .write_all(&bytes)
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
    }
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
            cols: 80,
            rows: 24,
            scrollback_bytes: 64 * 1024,
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
        assert!(!session.write(b"ignored".to_vec()));
    }

    #[test]
    fn input_reaches_the_child_and_output_is_broadcast_to_every_subscriber() {
        let session =
            PtySession::spawn(spec("read line; echo got:$line; sleep 30"), |_| {}).expect("spawn");
        let mut first = session.attach();
        let mut second = session.attach();
        assert_eq!(session.subscriber_count(), 2);
        assert!(session.write(b"ping\r".to_vec()));
        assert!(read_until(&mut first, "got:ping", Duration::from_secs(10)).contains("got:ping"));
        assert!(read_until(&mut second, "got:ping", Duration::from_secs(10)).contains("got:ping"));
        // A late attach replays what both live subscribers saw.
        let late = session.attach();
        assert!(String::from_utf8_lossy(&late.replay).contains("got:ping"));
        session.kill(Duration::from_secs(2));
        assert!(session.wait_exit(Duration::from_secs(10)));
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
}
