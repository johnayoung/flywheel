//! Closing a session's directory (see [`crate::close`]): its checklist
//! now, and the cleanups an end carries, applied once it ended (on a
//! thread of their own: sizing a build dir may take a while), recorded
//! on its end record and in the log (`close.checked`, `close.recorded`).
//!
//! The operator's close (`fw hub close`, the API) ends a session at rest
//! at once (never while it works or a tool call is open) and applies its
//! choices; on a session that already exited it applies them alone.

use super::{Hub, HubError, HubEvent};
use crate::close::{CloseChoices, CloseRecord};
use crate::state::{Actor, EndMode, EndRecord, SessionRecord, SessionState, StateError};
use crate::telemetry::now_ms;
use chrono::Utc;
use std::path::PathBuf;
use std::sync::Arc;

impl Hub {
    /// The other sessions working in `record`'s directory or on its
    /// branch (names).
    fn close_neighbours(&self, record: &SessionRecord) -> Vec<String> {
        let store = self.store();
        let dir = record
            .worktree
            .as_ref()
            .map_or(record.cwd.as_path(), |w| w.path.as_path());
        store
            .sessions()
            .iter()
            .filter(|s| s.id != record.id)
            .filter(|s| s.companion_of.as_deref() != Some(record.id.as_str()))
            .filter(|s| s.review_of.as_deref() != Some(record.id.as_str()))
            .filter(|s| s.state == SessionState::Running || s.ended.is_none())
            .filter(|s| {
                s.cwd.starts_with(dir)
                    || match (&s.worktree, &record.worktree) {
                        (Some(theirs), Some(mine)) => {
                            theirs.path == mine.path
                                || (theirs.repo == mine.repo && theirs.branch == mine.branch)
                        }
                        _ => false,
                    }
            })
            .map(|s| s.name.clone())
            .collect()
    }

    /// The directory a session's close acts on: its worktree, else its
    /// cwd.
    fn close_dir(record: &SessionRecord) -> PathBuf {
        record
            .worktree
            .as_ref()
            .map_or_else(|| record.cwd.clone(), |w| w.path.clone())
    }

    /// The close checklist of session `id`, now (runs git and sizes its
    /// build output: call off the request thread).
    pub fn close_checklist(&self, id: &str) -> Result<crate::close::Checklist, HubError> {
        let record = self.store().session(id)?.clone();
        let dir = Self::close_dir(&record);
        let list = crate::close::checklist(
            &dir,
            record.worktree.as_ref(),
            self.close_neighbours(&record),
        );
        self.log_hub_event(
            "close.checked",
            Some(id),
            serde_json::json!({
                "path": list.path,
                "merged": list.merged,
                "dirty": list.dirty,
                "untracked": list.untracked_count,
                "build_bytes": list.build.as_ref().map(|b| b.bytes),
            }),
        );
        Ok(list)
    }

    /// `choices` for ending `target`, its report made absolute (relative
    /// to its directory); `None` when they ask for nothing.
    pub(super) fn close_choices(
        &self,
        target: &SessionRecord,
        choices: Option<CloseChoices>,
    ) -> Result<Option<CloseChoices>, HubError> {
        let Some(mut choices) = choices.filter(|c| !c.is_empty()) else {
            return Ok(None);
        };
        if let Some(report) = choices.report.take() {
            let absolute = if report.is_relative() {
                Self::close_dir(target).join(report)
            } else {
                report
            };
            choices.report = Some(absolute);
        }
        if choices.worktree == crate::close::WorktreeChoice::Remove && target.worktree.is_none() {
            return Err(StateError::Invalid(format!(
                "{} does not run in a worktree of its own: there is none to remove",
                target.name
            ))
            .into());
        }
        Ok(Some(choices))
    }

    /// Apply `choices` to `record`'s directory now that it ended, on a
    /// thread; `by`: the agent that asked (told the outcome).
    pub(super) fn close_after_end(
        self: &Arc<Self>,
        record: &SessionRecord,
        choices: CloseChoices,
        end_id: &str,
        by: Option<&str>,
    ) {
        let hub = Arc::clone(self);
        let id = record.id.clone();
        let record = record.clone();
        let end_id = end_id.to_owned();
        let by = by.map(str::to_owned);
        let spawned = std::thread::Builder::new()
            .name("hub-close".into())
            .spawn(move || {
                let mut said = Vec::new();
                if !choices.removes_only() {
                    let done = hub.apply_close(&record, &choices, &end_id);
                    said.push(close_words(&done));
                }
                if choices.session == Some(crate::close::SessionChoice::Remove) {
                    said.push(match hub.remove_session(&record.id) {
                        Ok(()) => format!(
                            "removed it from the hub (its conversation stays resumable from {}; \
                             its history stays in telemetry)",
                            record.cwd.display()
                        ),
                        Err(e) => format!("kept it on the hub (not removed: {e})"),
                    });
                }
                if let Some(by) = by {
                    hub.tell(
                        &by,
                        &format!(
                            "From the flywheel hub: closed {} (id {}): {}",
                            record.name,
                            record.id,
                            said.join("; ")
                        ),
                    );
                }
            });
        if let Err(e) = spawned {
            eprintln!("fw hub: session {id}: close not applied: {e}");
        }
    }

    /// Apply `choices` to ended session `record`; recorded on its end
    /// record.
    fn apply_close(
        &self,
        record: &SessionRecord,
        choices: &CloseChoices,
        end_id: &str,
    ) -> CloseRecord {
        let folders: Vec<PathBuf> = self
            .store()
            .workspace(&record.workspace_id)
            .map(|w| w.folders.iter().map(|f| f.path.clone()).collect())
            .unwrap_or_default();
        let roots = crate::reports::roots_of(
            &record.cwd,
            record.worktree.as_ref().map(|w| w.path.as_path()),
            &folders,
        );
        let done = crate::close::apply(
            choices,
            &Self::close_dir(record),
            record.worktree.as_ref(),
            self.close_neighbours(record),
            self.state_dir(),
            &format!("{}-{}", record.id, &end_id[..end_id.len().min(8)]),
            Some(&roots),
            now_ms(),
        );
        let saved = done.clone();
        if let Err(e) = self.store().update_session(&record.id, |r| {
            if let Some(ended) = r.ended.as_mut() {
                ended.close = Some(saved);
            }
        }) {
            eprintln!(
                "fw hub: session {}: closed, but not recorded: {e}",
                record.id
            );
        }
        self.emit(HubEvent::Sessions);
        self.log_hub_event(
            "close.recorded",
            Some(&record.id),
            serde_json::json!({
                "worktree": done.worktree,
                "build": done.build,
                "report": done.report.as_ref().map(|r| &r.stored),
                "report_error": done.report_error,
            }),
        );
        done
    }

    /// The operator closes session `id` (`fw hub close`): a running one is
    /// ended at once if at rest (refused while it works, a tool call is
    /// open, or something waits), then `choices` applied; an exited one
    /// gets `choices` alone. `dry_run`: the checklist only.
    pub fn operator_close(
        self: &Arc<Self>,
        id: &str,
        choices: CloseChoices,
        dry_run: bool,
    ) -> Result<serde_json::Value, HubError> {
        let record = self.store().session(id)?.clone();
        if dry_run {
            let list = self.close_checklist(id)?;
            return Ok(serde_json::json!({ "checklist": list, "text": list.text() }));
        }
        let choices = self
            .close_choices(&record, Some(choices))?
            .unwrap_or_default();
        if record.ending.is_some() {
            return Err(StateError::Conflict(format!(
                "{} is ending already: wait for it, or call that end off first",
                record.name
            ))
            .into());
        }
        if record.state == SessionState::Running {
            if record.runs_agent() {
                self.at_rest(&record, false).map_err(|why| {
                    StateError::Conflict(format!(
                        "{} is not at rest ({why}): nothing is ended mid-flight; try again when \
                         it is",
                        record.name
                    ))
                })?;
            }
            let runtime = self.queue_runtime();
            let turn = runtime.paste_turn();
            let fresh = self.store().session(id)?.clone();
            if fresh.runs_agent() {
                self.at_rest(&fresh, false).map_err(|why| {
                    StateError::Conflict(format!("{} is not at rest ({why})", fresh.name))
                })?;
            }
            self.note_ending(id, &crate::state::Actor::Operator);
            self.kill_session(id)?;
            drop(turn);
        }
        let end = EndRecord {
            by: Actor::Operator,
            at: Utc::now(),
            mode: EndMode::AtRest,
            killed: Vec::new(),
            dirty: None,
            successor: None,
            by_name: None,
            successor_name: None,
            close: None,
        };
        self.store().update_session(id, |r| {
            if r.ended.is_none() {
                r.ended = Some(end);
            }
        })?;
        // Its processes are gone before anything is removed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self
            .live_session(id)
            .is_some_and(|pty| pty.exit_info().is_none())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let done = self.apply_close(&record, &choices, &crate::state::new_id());
        Ok(serde_json::json!({ "close": done, "text": close_words(&done) }))
    }
}

/// A close's outcome in words.
pub fn close_words(done: &CloseRecord) -> String {
    let mut parts = vec![
        format!("worktree {}", done.worktree.text()),
        format!("build {}", done.build.text()),
    ];
    match (&done.report, &done.report_error) {
        (Some(report), _) => parts.push(format!(
            "report kept at {} ({})",
            report.stored.display(),
            report.title
        )),
        (None, Some(why)) => parts.push(format!("report not kept: {why}")),
        (None, None) => {}
    }
    parts.join("; ")
}
