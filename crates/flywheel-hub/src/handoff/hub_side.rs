//! The hub's side of a live handoff (see [`crate::handoff`]): freezing
//! this hub's live state for another hub process to take over (and
//! thawing it when the handoff fails), and building a hub from what an
//! old hub handed over.

use super::{Hub, HubError, HubOptions, ServeLock, badge_key};
use crate::pty::PtySession;
use crate::pty::handoff::{Adopt, PtyExport};
use crate::report::ReportServer;
use crate::state::HubStore;
use crate::status::StatusBoard;
use crate::status::handoff::BoardExport;
use std::collections::HashSet;
use std::os::fd::RawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

/// One live session as frozen for a handoff.
pub(crate) struct FrozenSession {
    pub id: String,
    pub session: Arc<PtySession>,
    pub export: PtyExport,
}

/// Everything a frozen hub hands over besides the listening sockets.
pub(crate) struct Frozen {
    pub sessions: Vec<FrozenSession>,
    pub board: BoardExport,
}

/// The descriptors a frozen hub hands over (valid while it lives).
#[derive(Debug, Clone, Copy)]
pub(crate) struct HubFds {
    pub serve_lock: RawFd,
    pub report: Option<RawFd>,
}

/// One session received from the old hub.
#[derive(Debug)]
pub(crate) struct IncomingSession {
    pub id: String,
    pub adopt: Adopt,
}

/// What a new hub is built from.
#[derive(Debug)]
pub(crate) struct Incoming {
    pub serve_lock: ServeLock,
    /// The report socket's path (where agents' environment points) and
    /// its listening socket.
    pub report: Option<(PathBuf, UnixListener)>,
    pub board: BoardExport,
    pub sessions: Vec<IncomingSession>,
}

impl Hub {
    /// Freeze this hub for a handoff: stop taking agent reports (waiting
    /// for the ones being handled), stop following transcripts (saving
    /// where each stopped), and pause every live session (see
    /// [`PtySession::pause`]), then export them and the status board.
    /// Anything that fails thaws what was frozen.
    pub(crate) fn handoff_freeze(self: &Arc<Self>, timeout: Duration) -> Result<Frozen, String> {
        let deadline = Instant::now() + timeout;
        let left = || deadline.saturating_duration_since(Instant::now());
        let reports_idle = self
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_none_or(|server| server.pause(left()));
        self.commands.stop();
        self.commands.save_all();
        // The next hub reads the state file from here on; what changes
        // meanwhile (a session exiting) is written on a thaw.
        let _ = self.store().set_read_only(true);
        let live: Vec<(String, Arc<PtySession>)> = self
            .live()
            .iter()
            .filter(|(_, s)| !s.is_released())
            .map(|(id, s)| (id.clone(), Arc::clone(s)))
            .collect();
        let mut stuck = Vec::new();
        for (id, session) in &live {
            if !session.pause(left()) {
                stuck.push(id.clone());
            }
        }
        if !reports_idle || !stuck.is_empty() {
            self.handoff_thaw();
            return Err(if reports_idle {
                format!(
                    "session {} did not pause within {}s",
                    stuck.join(", "),
                    timeout.as_secs()
                )
            } else {
                format!(
                    "agent reports were still being handled after {}s",
                    timeout.as_secs()
                )
            });
        }
        let sessions = live
            .into_iter()
            .map(|(id, session)| FrozenSession {
                export: session.handoff_export(),
                id,
                session,
            })
            .collect();
        let board = self.board().handoff_export();
        Ok(Frozen { sessions, board })
    }

    /// Undo [`Hub::handoff_freeze`] (also safe after a partial freeze).
    pub(crate) fn handoff_thaw(self: &Arc<Self>) {
        if let Err(e) = self.store().set_read_only(false) {
            eprintln!("fw hub: {e}");
        }
        let live: Vec<Arc<PtySession>> = self.live().values().cloned().collect();
        for session in live {
            session.resume();
        }
        if let Some(server) = &*self.report.lock().unwrap_or_else(PoisonError::into_inner) {
            server.resume();
        }
        crate::transcript::spawn_watcher(self);
    }

    /// The serve lock's and the report socket's descriptors.
    pub(crate) fn handoff_fds(&self) -> HubFds {
        HubFds {
            serve_lock: self._serve_lock.raw_fd(),
            report: self
                .report
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(ReportServer::listener_fd),
        }
    }

    /// The handoff went through: this process must leave the report
    /// socket file (now the new hub's) alone.
    pub(crate) fn handoff_release(&self) {
        if let Some(server) = &*self.report.lock().unwrap_or_else(PoisonError::into_inner) {
            server.set_unlink(false);
        }
    }

    /// Build a hub from what an old hub handed over: its state file (the
    /// handed-over sessions stay `running`), status board, report socket,
    /// and sessions -- every one adopted **paused** and nothing followed or
    /// recorded until [`Hub::handoff_commit`].
    pub(crate) fn adopt_handoff(
        options: HubOptions,
        incoming: Incoming,
    ) -> Result<Arc<Self>, HubError> {
        let Incoming {
            serve_lock,
            report,
            board,
            sessions,
        } = incoming;
        let ids: HashSet<String> = sessions.iter().map(|s| s.id.clone()).collect();
        let store = HubStore::open_keeping(&options.state_dir, &ids)?;
        let hub = Self::assemble(
            options,
            serve_lock,
            store,
            StatusBoard::handoff_import(board),
        )?;
        if let Some((path, listener)) = report {
            let server = ReportServer::adopt(path, listener, hub.report_handler(), true)?;
            *hub.report.lock().unwrap_or_else(PoisonError::into_inner) = Some(server);
        }
        for incoming in sessions {
            let Ok(record) = hub.store().session(&incoming.id).cloned() else {
                // Removed meanwhile (cannot happen while the old hub is
                // frozen): leave its terminal alone, it is not ours.
                continue;
            };
            let generation = hub.board().generation(&record.id);
            let weak = Arc::downgrade(&hub);
            let exit_id = record.id.clone();
            let monitor_generation = generation.filter(|_| record.runs_agent());
            let session = PtySession::adopt(incoming.adopt, move |exit| {
                if let Some(hub) = weak.upgrade() {
                    hub.on_session_exit(&exit_id, monitor_generation, exit);
                }
            })
            .map_err(|e| HubError::Internal(format!("adopting session {}: {e}", record.id)))?;
            if let Some(generation) = monitor_generation
                && session.exit_info().is_none()
            {
                hub.monitor_agent(record.kind, &record.id, generation, &session);
            }
            hub.live().insert(record.id.clone(), session);
            hub.git.nudge(badge_key(&record));
        }
        crate::git::spawn_watcher(&hub);
        Ok(hub)
    }

    /// The old hub let go: start reading and writing every adopted
    /// session, taking agent reports, and following transcripts.
    pub(crate) fn handoff_commit(self: &Arc<Self>) {
        let live: Vec<Arc<PtySession>> = self.live().values().cloned().collect();
        for session in live {
            session.resume();
        }
        if let Some(server) = &*self.report.lock().unwrap_or_else(PoisonError::into_inner) {
            server.set_unlink(true);
            server.resume();
        }
        crate::transcript::spawn_watcher(self);
        self.emit(super::HubEvent::Sessions);
    }
}
