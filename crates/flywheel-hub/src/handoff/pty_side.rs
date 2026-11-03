//! The PTY side of a live handoff (see [`crate::handoff`]): the master a
//! session holds, what a paused session hands over, and adopting one that
//! another hub process handed over.
//!
//! An adopted session's child is not this process's child: it cannot be
//! waited for, so its exit is noticed through the pidfd the old hub
//! opened at spawn (Linux), else by polling the pid, and is recorded with
//! no code or signal (unknown).

use super::{
    BROADCAST_DEPTH, DRAIN_AFTER_EXIT, Emulator, ExitInfo, InputQueue, PtySession, Shared, pty_size,
};
use portable_pty::{MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::broadcast;

/// How often an adopted session's leader is checked for when there is no
/// pidfd to wait on.
const EXIT_POLL: Duration = Duration::from_millis(250);

/// The PTY master a session holds: portable-pty's for a session this
/// process spawned, a plain descriptor for an adopted one.
pub(crate) enum Master {
    Spawned(Box<dyn MasterPty + Send>),
    Adopted(OwnedFd),
}

impl Master {
    pub(super) fn resize(&self, size: PtySize) -> std::io::Result<()> {
        match self {
            Self::Spawned(master) => master
                .resize(size)
                .map_err(|e| std::io::Error::other(e.to_string())),
            Self::Adopted(fd) => set_winsize(fd.as_raw_fd(), size),
        }
    }

    fn raw_fd(&self) -> Option<RawFd> {
        match self {
            Self::Spawned(master) => master.as_raw_fd(),
            Self::Adopted(fd) => Some(fd.as_raw_fd()),
        }
    }
}

fn set_winsize(fd: RawFd, size: PtySize) -> std::io::Result<()> {
    let winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };
    // SAFETY: TIOCSWINSZ reads one `winsize`, valid for the call; `fd` is
    // the open master this session owns.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &raw const winsize) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A pidfd for process `pid` (Linux 5.3+); `None` elsewhere, on older
/// kernels, or when the process is gone. Close-on-exec.
#[cfg(target_os = "linux")]
pub(crate) fn pidfd_open(pid: u32) -> Option<OwnedFd> {
    use std::os::fd::FromRawFd;
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: pidfd_open(2) takes a pid and flags and returns a new
    // descriptor or -1; it touches no memory of ours.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let fd = RawFd::try_from(fd).ok().filter(|&fd| fd >= 0)?;
    // SAFETY: the kernel just returned this descriptor, owned by nobody else.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pidfd_open(_pid: u32) -> Option<OwnedFd> {
    None
}

/// Whether process `pid` exists and is not a zombie.
pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                super::stat_session(&stat).is_some_and(|(state, _)| state != 'Z' && state != 'X')
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: kill(2) with signal 0 only checks for the process.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Block until the leader has exited: its pidfd turns readable, or (no
/// pidfd) the pid stops naming a live process.
fn wait_leader_exit(pidfd: Option<RawFd>, pid: Option<u32>) {
    if let Some(fd) = pidfd {
        loop {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the call; the session owning the
            // pidfd outlives this thread's use of it.
            let rc = unsafe { libc::poll(&raw mut pfd, 1, 1000) };
            if rc > 0 {
                return;
            }
            if rc < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
    }
    if let Some(pid) = pid {
        while process_alive(pid) {
            thread::sleep(EXIT_POLL);
        }
    }
}

/// What a paused session hands over besides its descriptors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PtyMeta {
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub output_bytes: u64,
    /// Set when the leader had already exited.
    pub exit: Option<ExitInfo>,
}

/// A paused session's state: [`PtyMeta`], the emulator as a snapshot
/// (every line of scrollback), and input not yet written.
#[derive(Debug, Clone)]
pub(crate) struct PtyExport {
    pub meta: PtyMeta,
    pub snapshot: Vec<u8>,
    pub held_input: Vec<u8>,
}

/// A session to adopt: the descriptors received plus its export.
#[derive(Debug)]
pub(crate) struct Adopt {
    pub master: OwnedFd,
    pub pidfd: Option<OwnedFd>,
    pub export: PtyExport,
    pub scrollback_lines: usize,
}

impl PtySession {
    /// The state to hand over. Call while paused (so nothing moves): the
    /// held input is copied, not taken, so a rolled-back handoff resumes
    /// with it. From here until [`PtySession::resume`] input is refused
    /// ([`WriteOutcome::HandingOff`](super::WriteOutcome::HandingOff)):
    /// what is typed after the copy would reach neither hub.
    pub(crate) fn handoff_export(&self) -> PtyExport {
        let held_input = {
            let mut pause = self.queue.pause_state();
            pause.sealed = true;
            pause.held.clone()
        };
        let mut shared = self.lock();
        PtyExport {
            meta: PtyMeta {
                pid: self.pid,
                cols: shared.size.cols,
                rows: shared.size.rows,
                output_bytes: shared.output_bytes,
                exit: shared.exit.clone(),
            },
            snapshot: shared.screen.snapshot(usize::MAX),
            held_input,
        }
    }

    /// Drop the held input (a rollback after the new hub may already have
    /// written it: never type it twice).
    pub(crate) fn handoff_forget_held(&self) {
        let mut pause = self.queue.pause_state();
        let len = pause.held.len();
        pause.held.clear();
        self.queue.queued.fetch_sub(len, Ordering::AcqRel);
    }

    /// The master's descriptor (valid while the session lives).
    pub(crate) fn master_fd(&self) -> Option<RawFd> {
        self.master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .raw_fd()
    }

    /// The leader's pidfd, when there is one (valid while the session
    /// lives).
    pub(crate) fn pidfd(&self) -> Option<RawFd> {
        self.pidfd.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Adopt a session handed over by another hub, **paused**: its
    /// emulator is rebuilt from the snapshot, but nothing is read from or
    /// written to the PTY until [`PtySession::resume`] (the old hub must
    /// have let go first). `on_exit` runs once, when the leader is seen to
    /// exit (not for a session whose leader had already exited), with an
    /// unknown status.
    pub(crate) fn adopt(
        adopt: Adopt,
        on_exit: impl FnOnce(ExitInfo) + Send + 'static,
    ) -> std::io::Result<Arc<Self>> {
        let Adopt {
            master,
            pidfd,
            export,
            scrollback_lines,
        } = adopt;
        let size = pty_size(export.meta.cols, export.meta.rows);
        let mut screen = Emulator::new(size.cols, size.rows, scrollback_lines);
        screen.process(&export.snapshot);
        // Rebuilding asks nothing of the program.
        let _ = screen.take_replies();
        let fd = master.as_raw_fd();
        let reader = File::from(master.try_clone()?);
        // A plain descriptor: dropping it sends the program nothing.
        let writer = File::from(master.try_clone()?);
        let (events, _) = broadcast::channel(BROADCAST_DEPTH);
        let (input, input_rx) = std_mpsc::channel::<Vec<u8>>();
        let queue = Arc::new(InputQueue::new());
        {
            let mut state = queue.pause_state();
            state.requested = true;
            state.held = export.held_input;
            queue.queued.store(state.held.len(), Ordering::Release);
        }
        queue.pausing.store(true, Ordering::Release);
        let exited = export.meta.exit.clone();
        let session = Arc::new(Self {
            shared: Mutex::new(Shared {
                screen,
                exit: exited.clone(),
                reaped: exited.is_some(),
                output_bytes: export.meta.output_bytes,
                size,
            }),
            exited: Condvar::new(),
            events,
            input,
            queue,
            master: Mutex::new(Master::Adopted(master)),
            pid: export.meta.pid,
            pidfd,
        });
        let drained_rx = session.start_io(fd, Box::new(reader), Box::new(writer), input_rx)?;
        if exited.is_some() {
            return Ok(session);
        }
        let waiter = Arc::clone(&session);
        thread::Builder::new()
            .name("hub-pty-watch".into())
            .spawn(move || {
                wait_leader_exit(waiter.pidfd(), waiter.pid);
                waiter.lock().reaped = true;
                // Nothing is recorded before the handoff completes (it may
                // still roll back to the old hub).
                while waiter.is_paused() && !waiter.is_released() {
                    thread::sleep(EXIT_POLL);
                }
                let _ = drained_rx.recv_timeout(DRAIN_AFTER_EXIT);
                let info = ExitInfo {
                    code: None,
                    signal: None,
                };
                waiter.record_exit(info.clone());
                on_exit(info);
            })?;
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::{SessionEvent, SpawnSpec, WriteOutcome};
    use std::time::Instant;

    fn shell() -> Arc<PtySession> {
        PtySession::spawn(
            SpawnSpec {
                argv: vec!["/bin/sh".into()],
                cwd: std::env::temp_dir(),
                env: vec![("PS1".into(), "$ ".into())],
                env_remove: Vec::new(),
                cols: 80,
                rows: 24,
                scrollback_lines: 1000,
            },
            |_| {},
        )
        .unwrap()
    }

    fn screen_has(session: &PtySession, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let text = session
                .screen_text(crate::vt::TextSource::Visible)
                .lines
                .join("\n");
            if text.contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_paused_session_holds_input_in_order_and_resumes() {
        let session = shell();
        assert!(screen_has(&session, "$"));
        assert!(session.pause(Duration::from_secs(5)));
        assert!(session.is_paused());
        assert_eq!(session.write(b"echo one-".to_vec()), WriteOutcome::Queued);
        assert_eq!(session.write(b"two\r".to_vec()), WriteOutcome::Queued);
        // Nothing reaches the child (or the screen) while paused.
        thread::sleep(Duration::from_millis(300));
        assert!(!screen_has_now(&session, "one-two"));
        assert_eq!(session.handoff_export().held_input, b"echo one-two\r");
        // Exported: what is typed now would reach neither hub.
        assert_eq!(session.write(b"lost".to_vec()), WriteOutcome::HandingOff);
        assert_eq!(session.queued_input(), b"echo one-two\r".len());
        session.resume();
        assert!(screen_has(&session, "one-two"));
        assert_eq!(session.queued_input(), 0);
        session.kill(Duration::ZERO);
        session.release();
    }

    #[test]
    fn a_rollback_after_the_new_hub_resumed_never_types_held_input_twice() {
        let session = shell();
        assert!(screen_has(&session, "$"));
        assert!(session.pause(Duration::from_secs(5)));
        assert_eq!(
            session.write(b"echo twice-$((1+1))\r".to_vec()),
            WriteOutcome::Queued
        );
        assert!(!session.handoff_export().held_input.is_empty());
        // The new hub said `resuming` (it may have typed it), then died.
        session.handoff_forget_held();
        assert_eq!(session.queued_input(), 0);
        session.resume();
        assert_eq!(
            session.write(b"echo after\r".to_vec()),
            WriteOutcome::Queued
        );
        assert!(screen_has(&session, "after"));
        assert!(!screen_has_now(&session, "twice-2"));
        session.kill(Duration::ZERO);
        session.release();
    }

    fn screen_has_now(session: &PtySession, needle: &str) -> bool {
        session
            .screen_text(crate::vt::TextSource::Visible)
            .lines
            .join("\n")
            .contains(needle)
    }

    /// A descriptor of our own for `fd` (open while its session lives).
    fn dup(fd: RawFd) -> OwnedFd {
        // SAFETY: the session owning `fd` outlives this borrow.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .unwrap()
    }

    #[test]
    fn an_adopted_session_shows_the_same_screen_and_takes_input() {
        let original = shell();
        assert_eq!(
            original.write(b"for i in $(seq 1 200); do echo row-$i; done\r".to_vec()),
            WriteOutcome::Queued
        );
        assert!(screen_has(&original, "row-200"));
        assert!(original.pause(Duration::from_secs(5)));
        assert_eq!(
            original.write(b"echo typed-".to_vec()),
            WriteOutcome::Queued
        );
        let export = original.handoff_export();
        let master = dup(original.master_fd().unwrap());
        let pidfd = original.pidfd().map(dup);
        let (exit_tx, exit_rx) = std_mpsc::channel();
        let adopted = PtySession::adopt(
            Adopt {
                master,
                pidfd,
                export: export.clone(),
                scrollback_lines: 1000,
            },
            move |info| {
                let _ = exit_tx.send(info);
            },
        )
        .unwrap();
        assert!(adopted.is_paused());
        let mut before = original.attach();
        let mut after = adopted.attach();
        assert_eq!(before.replay, after.replay, "same snapshot");
        assert_eq!((after.cols, after.rows), (80, 24));
        assert!(adopted.scrollback_lines() > 150);
        adopted.resume();
        assert_eq!(adopted.write(b"42\r".to_vec()), WriteOutcome::Queued);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = String::new();
        while !seen.contains("typed-42") && Instant::now() < deadline {
            match after.events.try_recv() {
                Ok(SessionEvent::Output(bytes)) => seen.push_str(&String::from_utf8_lossy(&bytes)),
                _ => thread::sleep(Duration::from_millis(10)),
            }
        }
        assert!(seen.contains("typed-42"), "{seen:?}");
        // The paused original saw none of it.
        assert!(matches!(
            before.events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(adopted.write(b"exit\r".to_vec()), WriteOutcome::Queued);
        let info = exit_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            info,
            ExitInfo {
                code: None,
                signal: None
            }
        );
        assert!(adopted.wait_exit(Duration::from_secs(1)));
        original.release();
        adopted.release();
    }

    #[test]
    fn process_liveness_sees_exits() {
        assert!(process_alive(std::process::id()));
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!process_alive(pid));
    }
}
