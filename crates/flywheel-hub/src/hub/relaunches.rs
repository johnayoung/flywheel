//! Relaunch on upgrade: a live `fw hub upgrade` keeps every session's
//! process, so an agent keeps the MCP tools and hooks its old hub gave it.
//! The new hub marks each agent session it took over stale
//! ([`Hub::mark_stale`]) and relaunches it in place, resuming its
//! conversation (same id, name, and folder), once it is safe:
//!
//! * never mid-flight: only at rest ([`Hub::at_rest`] with its shown
//!   state done, idle, or failed: not working, compacting, asking,
//!   blocked, or with subagents in the background), its last turn ended
//!   (an `idle_prompt` inside a turn is not its end), nothing it started
//!   still running, and no prompt sent to it still unstarted;
//! * one at a time, the next once the last one's new process reported;
//! * before its queue delivers again: the queue holds while a relaunch is
//!   due or under way ([`Hub::relaunch_hold`]), so the next prompt reaches
//!   the new process.
//!
//! Never relaunched (the note stays): a session opted out (`fw hub
//! relaunch --never`), an upgrade with `--relaunch off`, one whose
//! conversation cannot be resumed, one ending. With `--relaunch idle` only
//! sessions at rest right after the upgrade go. A relaunch that fails
//! leaves the session exited with the reason on its note and timeline; it
//! never starts a blank conversation. A session busy for hours just keeps
//! its note. Driven from the queue's thread ([`Hub::relaunch_tick`]),
//! before the queue's own tick.

use super::{Hub, HubEvent};
use crate::agent::SessionKind;
use crate::state::{RelaunchMode, SessionRecord, Stale};
use crate::status::AgentStatus;
use crate::telemetry::{Draft, What, now_ms};
use chrono::{DateTime, Utc};
use std::time::{Duration, Instant};

/// With `--relaunch idle`: how long after the upgrade a session may come
/// to rest and still be relaunched.
const IDLE_GRACE: Duration = Duration::from_secs(10);
/// How long a hung-up session may take to exit before the relaunch is
/// given up (its kill escalates to SIGKILL after [`super::KILL_GRACE`]).
const EXIT_WAIT: Duration = Duration::from_secs(30);
/// The longest the next relaunch waits for the last one's new process to
/// report.
const PACE: Duration = Duration::from_secs(30);
/// How long a look at the processes an agent started stands (the look
/// walks `/proc`).
const PROCESS_LOOK: Duration = Duration::from_secs(10);

/// What relaunching after an upgrade remembers between ticks (in memory:
/// the record keeps what must survive a handoff).
#[derive(Debug, Default)]
pub(crate) struct RelaunchRuntime {
    /// The last relaunch: its session and when.
    last: Option<(String, Instant)>,
    /// Per session, the last look at the processes its agent started:
    /// the rest it was taken in (status since), when, and what ran.
    processes: std::collections::HashMap<String, (DateTime<Utc>, Instant, Option<String>)>,
}

/// The note a stale session carries, for people (`ls`, `session_status`).
pub fn stale_note(stale: &Stale) -> String {
    match &stale.held {
        Some(why) => format!("relaunch pending (not automatic: {why})"),
        None if stale.hung_up_at.is_some() => "relaunching".to_owned(),
        None => match &stale.waits {
            Some(why) => format!("relaunch pending (waits: {why})"),
            None => "relaunch pending (at its next safe point)".to_owned(),
        },
    }
}

/// What the timeline says of a relaunch after an upgrade.
fn why_text(stale: &Stale) -> String {
    format!("after the hub upgrade from flywheel {}", stale.from)
}

impl Hub {
    fn relaunches(&self) -> std::sync::MutexGuard<'_, RelaunchRuntime> {
        self.relaunch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mark every running agent session stale: its process was taken over
    /// from the hub at flywheel `from` (a live upgrade). Returns how many,
    /// and the mode they relaunch by.
    pub fn mark_stale(&self, from: &str) -> (usize, RelaunchMode) {
        let mode = self.options.relaunch_on_upgrade;
        let now = Utc::now();
        let records: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.state.is_running() && s.runs_agent())
            .cloned()
            .collect();
        let mut marked = 0;
        for record in &records {
            let held = self.relaunch_held(record, mode);
            let saved = self
                .store()
                .update_session(&record.id, |r| match r.stale.as_mut() {
                    // A relaunch under way carries on.
                    Some(stale) if stale.hung_up_at.is_some() => {}
                    _ => {
                        r.stale = Some(Stale {
                            since: now,
                            from: from.to_owned(),
                            mode,
                            held: held.clone(),
                            hung_up_at: None,
                            gave_up: None,
                            waits: None,
                        });
                    }
                });
            match saved {
                Ok(()) => marked += 1,
                Err(e) => eprintln!("fw hub: session {}: not marked stale: {e}", record.id),
            }
        }
        if marked > 0 {
            eprintln!(
                "fw hub: {marked} agent session{} from flywheel {from} to relaunch ({})",
                if marked == 1 { "" } else { "s" },
                mode.as_str()
            );
            self.queue_runtime().wakeup.nudge();
            self.emit(HubEvent::Sessions);
        }
        (marked, mode)
    }

    /// Why stale `record` is never relaunched on its own (`None`: it is,
    /// at its next safe point).
    fn relaunch_held(&self, record: &SessionRecord, mode: RelaunchMode) -> Option<String> {
        if record.never_relaunch {
            return Some("opted out: `fw hub relaunch --auto` opts it back in".to_owned());
        }
        if mode == RelaunchMode::Off {
            return Some("the upgrade ran with --relaunch off".to_owned());
        }
        if !self.options.agent_resume {
            return Some("agent resume is off (sessions.agent_resume)".to_owned());
        }
        if !matches!(record.kind, SessionKind::Claude | SessionKind::Codex)
            || record.agent_session_id.is_none()
            || !self.conversation_exists(record)
        {
            return Some("its conversation cannot be resumed".to_owned());
        }
        None
    }

    /// Opt session `id` out of relaunches after upgrades (`never`), or
    /// back in.
    pub fn set_never_relaunch(
        &self,
        id: &str,
        never: bool,
    ) -> Result<super::SessionView, super::HubError> {
        let record = self.store().update_session(id, |r| {
            r.never_relaunch = never;
            r.clone()
        })?;
        self.queue_runtime().wakeup.nudge();
        self.emit(HubEvent::Sessions);
        Ok(self.view(record))
    }

    /// Why stale `record` cannot be relaunched right now (it is not at a
    /// safe point), else `Ok`.
    fn relaunch_ready(&self, record: &SessionRecord) -> Result<(), String> {
        self.at_rest(record, true)?;
        let since = self
            .agent_status(&record.id)
            .map_or_else(Utc::now, |s| s.since);
        let cached = self
            .relaunches()
            .processes
            .get(&record.id)
            .filter(|(rest, at, _)| *rest == since && at.elapsed() < PROCESS_LOOK)
            .map(|(_, _, why)| why.clone());
        let running = match cached {
            Some(why) => why,
            None => {
                let why = self.work_running(record);
                self.relaunches()
                    .processes
                    .insert(record.id.clone(), (since, Instant::now(), why.clone()));
                why
            }
        };
        match running {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }

    /// Why session `record`'s queue holds for a relaunch after an upgrade:
    /// one is under way, or due now (it is stale and at a safe point).
    pub(crate) fn relaunch_hold(&self, record: &SessionRecord) -> Option<&'static str> {
        let stale = record.stale.as_ref()?;
        if stale.hung_up_at.is_some() {
            return Some("it is being relaunched after the hub upgrade");
        }
        (stale.held.is_none()
            && record.ending.is_none()
            && record.state.is_running()
            && self.relaunch_ready(record).is_ok())
        .then_some("it is relaunched after the hub upgrade first")
    }

    fn set_stale(&self, id: &str, edit: impl FnOnce(&mut Stale)) {
        let saved = self.store().update_session(id, |r| {
            if let Some(stale) = r.stale.as_mut() {
                edit(stale);
            }
        });
        if let Err(e) = saved {
            eprintln!("fw hub: session {id}: relaunch: not recorded: {e}");
        }
    }

    /// One look at every stale session; whether to look again in a tick
    /// (one may be relaunched yet). Runs on the queue's thread, before
    /// its tick.
    pub(crate) fn relaunch_tick(self: &Arc<Self>) -> bool {
        let stale: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.stale.is_some())
            .cloned()
            .collect();
        if stale.is_empty() {
            return false;
        }
        if let Some(record) = stale
            .iter()
            .find(|r| r.stale.as_ref().is_some_and(|s| s.hung_up_at.is_some()))
        {
            self.finish_relaunch(record);
            return true;
        }
        let mut changed = false;
        let mut candidates = Vec::new();
        for record in stale {
            if !record.state.is_running() || !record.runs_agent() {
                // Exited or restored meanwhile: its next process is new.
                let _ = self.store().update_session(&record.id, |r| r.stale = None);
                changed = true;
                continue;
            }
            let Some(stale) = record.stale.as_ref() else {
                continue;
            };
            let held = stale
                .gave_up
                .clone()
                .or_else(|| self.relaunch_held(&record, stale.mode));
            if held != stale.held {
                self.set_stale(&record.id, |s| s.held = held.clone());
                changed = true;
            }
            if held.is_none() {
                if record.ending.is_some() {
                    changed |= self.note_wait(&record, "an end of it is in progress");
                } else {
                    candidates.push(record);
                }
            }
        }
        if changed {
            self.emit(HubEvent::Sessions);
        }
        if candidates.is_empty() {
            return false;
        }
        if self.pacing() {
            return true;
        }
        for record in candidates {
            let Some(stale) = record.stale.clone() else {
                continue;
            };
            match self.relaunch_ready(&record) {
                Ok(()) => {
                    self.begin_relaunch(&record);
                    return true;
                }
                Err(why) => {
                    if self.note_wait(&record, &why) {
                        self.emit(HubEvent::Sessions);
                    }
                    let after = (Utc::now() - stale.since).to_std().unwrap_or_default();
                    if stale.mode == RelaunchMode::Idle && after >= IDLE_GRACE {
                        let held = format!("busy at the upgrade (--relaunch idle): {why}");
                        self.set_stale(&record.id, |s| {
                            s.gave_up = Some(held.clone());
                            s.held = Some(held);
                        });
                        self.emit(HubEvent::Sessions);
                    }
                }
            }
        }
        true
    }

    /// Record why stale `record` waits for its relaunch; whether that
    /// changed.
    fn note_wait(&self, record: &SessionRecord, why: &str) -> bool {
        let stale = record.stale.as_ref();
        if stale.and_then(|s| s.waits.as_deref()) == Some(why) {
            return false;
        }
        self.set_stale(&record.id, |s| s.waits = Some(why.to_owned()));
        true
    }

    /// Whether the last relaunch's new process has yet to report (the next
    /// waits for it, at most [`PACE`]).
    fn pacing(&self) -> bool {
        let last = self.relaunches().last.clone();
        let Some((id, at)) = last else {
            return false;
        };
        let reported = self
            .agent_status(&id)
            .is_some_and(|s| s.status != AgentStatus::Unknown);
        if reported || at.elapsed() >= PACE {
            self.relaunches().last = None;
            return false;
        }
        true
    }

    /// Hang stale `record` up for its relaunch, looked at again under the
    /// paste turn (nothing types into it meanwhile).
    fn begin_relaunch(&self, record: &SessionRecord) {
        let runtime = self.queue_runtime();
        let _turn = runtime.paste_turn();
        let Ok(fresh) = self.store().session(&record.id).cloned() else {
            return;
        };
        if fresh.stale.as_ref().is_none_or(|s| s.held.is_some())
            || self.relaunch_ready(&fresh).is_err()
        {
            return;
        }
        self.set_stale(&fresh.id, |s| s.hung_up_at = Some(Utc::now()));
        eprintln!(
            "fw hub: session {} ({}): relaunching after the upgrade",
            fresh.name, fresh.id
        );
        if let Err(e) = self.kill_session(&fresh.id) {
            let why = format!("it could not be hung up: {e}");
            self.set_stale(&fresh.id, |s| {
                s.hung_up_at = None;
                s.gave_up = Some(why.clone());
                s.held = Some(why);
            });
        }
        self.emit(HubEvent::Sessions);
    }

    /// The relaunch of `record` under way: once its process exited, start
    /// it again resuming its conversation; give up when it does not exit.
    fn finish_relaunch(self: &Arc<Self>, record: &SessionRecord) {
        let Some(stale) = record.stale.clone() else {
            return;
        };
        let Some(hung_up_at) = stale.hung_up_at else {
            return;
        };
        if record.state.is_running() {
            let waited = (Utc::now() - hung_up_at).to_std().unwrap_or_default();
            if waited >= EXIT_WAIT {
                let why = format!(
                    "it did not exit within {}s of a hangup",
                    EXIT_WAIT.as_secs()
                );
                eprintln!("fw hub: session {}: relaunch given up: {why}", record.id);
                self.set_stale(&record.id, |s| {
                    s.hung_up_at = None;
                    s.gave_up = Some(why.clone());
                    s.held = Some(why);
                });
                self.emit(HubEvent::Sessions);
            }
            return;
        }
        let size = self.live_session(&record.id).map(|pty| pty.size());
        // Its conversation is looked for again: a relaunch never starts a
        // blank one.
        let result = if self.conversation_exists(record) {
            self.restart_in_place(&record.id, size.map(|s| s.0), size.map(|s| s.1), true)
                .map_err(|e| e.to_string())
        } else {
            Err("its conversation is gone, so it was not started again".to_owned())
        };
        let why = why_text(&stale);
        match result {
            Ok(_) => {
                eprintln!(
                    "fw hub: session {} ({}): relaunched {why}",
                    record.name, record.id
                );
                self.relaunches().last = Some((record.id.clone(), Instant::now()));
                self.relaunches().processes.remove(&record.id);
                self.telemetry().record(
                    &record.id,
                    vec![Draft::new(now_ms(), What::Relaunched { why, error: None })],
                );
            }
            Err(error) => {
                eprintln!(
                    "fw hub: session {} ({}): relaunch failed, it stays exited: {error}",
                    record.name, record.id
                );
                let held = format!("the relaunch failed: {error}");
                self.set_stale(&record.id, |s| {
                    s.hung_up_at = None;
                    s.gave_up = Some(held.clone());
                    s.held = Some(held);
                });
                self.telemetry().record(
                    &record.id,
                    vec![Draft::new(
                        now_ms(),
                        What::Relaunched {
                            why,
                            error: Some(error),
                        },
                    )],
                );
            }
        }
        self.emit(HubEvent::Sessions);
    }
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    fn stale(held: Option<&str>, hung_up: bool) -> Stale {
        Stale {
            since: Utc::now(),
            from: "0.6.0".into(),
            mode: RelaunchMode::SafePoints,
            held: held.map(str::to_owned),
            hung_up_at: hung_up.then(Utc::now),
            gave_up: None,
            waits: None,
        }
    }

    #[test]
    fn the_note_says_whether_and_when_it_relaunches() {
        assert_eq!(
            stale_note(&stale(None, false)),
            "relaunch pending (at its next safe point)"
        );
        assert_eq!(stale_note(&stale(None, true)), "relaunching");
        assert_eq!(
            stale_note(&stale(Some("opted out"), false)),
            "relaunch pending (not automatic: opted out)"
        );
    }

    #[test]
    fn modes_parse_from_their_names_only() {
        for mode in RelaunchMode::ALL {
            assert_eq!(RelaunchMode::parse(mode.as_str()), Ok(mode));
        }
        assert_eq!(RelaunchMode::default(), RelaunchMode::SafePoints);
        assert!(RelaunchMode::parse("always").is_err());
    }
}
