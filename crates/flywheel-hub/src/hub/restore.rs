//! Restarting a session and bringing sessions back after a hub restart.

use super::{Hub, HubError, HubEvent, RestoreOutcome, SessionView, badge_key, forget};
use crate::agent::SessionKind;
use crate::restore::RestoreHow;
use crate::state::{SessionRecord, SessionState, StateError};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

impl Hub {
    /// Relaunch an ended session in place: same id, name, kind, command,
    /// and repo, a fresh PTY and scrollback. Works for a session that
    /// exited on this server and for one left stopped by a hub restart.
    /// Refused (409) while it still runs.
    ///
    /// The record's state gates this: it leaves `running` only once the
    /// old PTY's exit callback has run (or the hub restarted), so no late
    /// exit of the old process can land on the relaunched one.
    pub fn restart_session(
        self: &Arc<Self>,
        id: &str,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<SessionView, HubError> {
        let mut store = self.store();
        let record = store.session(id)?.clone();
        if record.state.is_running() {
            return Err(StateError::Conflict(format!(
                "session {:?} is still running",
                record.name
            ))
            .into());
        }
        let workspace = store.workspace(&record.workspace_id)?;
        if !workspace.has_folder(record.folder()) {
            return Err(StateError::Invalid(format!(
                "folder {} is no longer part of workspace {}",
                record.folder().display(),
                workspace.name
            ))
            .into());
        }
        let mut record = record;
        // Relaunching runs the session as itself again (`mark_running`
        // clears a restore's fallback in the record too).
        record.fallback_command = None;
        if record.kind == SessionKind::Claude && record.agent_session_id.is_none() {
            // A record from before the hub assigned conversation ids: give
            // it one, or it would launch without its status hooks.
            let conversation = uuid::Uuid::new_v4().hyphenated().to_string();
            store.set_agent_session_id(id, &conversation)?;
            record.agent_session_id = Some(conversation);
        }
        let session = self.spawn_pty(&record, cols, rows, false, None)?;
        let record = match store.mark_running(id) {
            Ok(record) => record,
            Err(e) => {
                drop(store);
                session.kill(Duration::ZERO);
                session.release();
                return Err(e.into());
            }
        };
        // Relaunching is what a stalled queue asked for: it carries on
        // once the new agent is at rest.
        if store.queue(id).is_some_and(|q| q.stalled.is_some()) {
            match store.update_queue(id, |q| q.stalled = None) {
                Ok(()) => {
                    eprintln!("fw hub: queue {id}: relaunched, so its stall is cleared");
                    self.queue_runtime().wakeup.nudge();
                }
                Err(e) => eprintln!("fw hub: queue {id}: stall not cleared: {e}"),
            }
        }
        let previous = self.live().insert(record.id.clone(), session);
        drop(store);
        if let Some(previous) = previous {
            forget(&previous);
        }
        self.git.nudge(badge_key(&record));
        self.emit(HubEvent::Sessions);
        Ok(self.view(record))
    }

    /// Restore every session a clean shutdown stopped (see
    /// [`crate::restore`]), oldest first: each keeps its id, name, and
    /// place, with a fresh PTY in its saved working directory. One that
    /// cannot come back (its directory is gone, its repo left the
    /// workspace, the spawn failed) stays stopped with the reason recorded
    /// as `restore_error`, and is retried on the next start.
    pub fn restore_sessions(self: &Arc<Self>) -> Vec<RestoreOutcome> {
        let candidates: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.state.is_restorable())
            .cloned()
            .collect();
        candidates
            .into_iter()
            .map(|record| {
                let (cwd, result, note) = match self.restore_one(&record) {
                    Ok((how, cwd, note)) => (cwd, Ok(how), note),
                    Err(message) => {
                        let _ = self.store().set_restore_error(&record.id, &message);
                        (record.cwd, Err(message), None)
                    }
                };
                RestoreOutcome {
                    id: record.id,
                    name: record.name,
                    cwd,
                    result,
                    note,
                }
            })
            .collect()
    }

    /// Drop the pending restore of every cleanly stopped session (the hub
    /// started with restore off): they stay stopped, on later starts too,
    /// until relaunched. Returns how many.
    pub fn forgo_restore(&self) -> Result<usize, HubError> {
        Ok(self.store().forgo_restore()?)
    }

    /// Restore one session: how, the directory it started in, and a note
    /// for the operator.
    fn restore_one(
        self: &Arc<Self>,
        record: &SessionRecord,
    ) -> Result<(RestoreHow, PathBuf, Option<String>), String> {
        if !record.cwd.is_dir() {
            return Err(format!(
                "its working directory {} no longer exists",
                record.cwd.display()
            ));
        }
        // Where its shell had `cd`'d to, while that still exists; else the
        // session's repo.
        let (cwd, note) = match &record.state {
            SessionState::Stopped {
                last_cwd: Some(last),
                ..
            } if last.is_dir() => (last.clone(), None),
            SessionState::Stopped {
                last_cwd: Some(last),
                ..
            } if *last != record.cwd => (
                record.cwd.clone(),
                Some(format!(
                    "its last directory {} no longer exists",
                    last.display()
                )),
            ),
            _ => (record.cwd.clone(), None),
        };
        let resume = crate::restore::Resumable {
            enabled: self.options.agent_resume,
            conversation_exists: record.kind.is_agent() && self.conversation_exists(record),
        };
        let plan = crate::restore::plan(record, &self.options.default_shell, resume);
        // An agent resumes where its CLI filed the conversation: the
        // directory it was started in (its repo), not one it reported.
        let (cwd, note) = if plan.how == RestoreHow::Resume {
            (record.cwd.clone(), None)
        } else {
            (cwd, note)
        };
        let mut store = self.store();
        let workspace = store
            .workspace(&record.workspace_id)
            .map_err(|e| e.to_string())?;
        if !workspace.has_folder(record.folder()) {
            return Err(format!(
                "folder {} is no longer part of workspace {}",
                record.folder().display(),
                workspace.name
            ));
        }
        let fallback = (plan.how == RestoreHow::Shell).then_some(plan.command);
        let mut relaunched = record.clone();
        relaunched.fallback_command = fallback.clone();
        relaunched.cwd = cwd.clone();
        let session = self
            .spawn_pty(
                &relaunched,
                None,
                None,
                plan.how == RestoreHow::Resume,
                None,
            )
            .map_err(|e| e.to_string())?;
        if let Err(e) = store.mark_restored(&record.id, fallback) {
            drop(store);
            session.kill(Duration::ZERO);
            session.release();
            return Err(e.to_string());
        }
        let previous = self.live().insert(record.id.clone(), session);
        drop(store);
        if let Some(previous) = previous {
            forget(&previous);
        }
        Ok((plan.how, cwd, note))
    }
}
