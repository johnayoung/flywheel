//! Grading: resume what the last hub left, follow the agents' turns,
//! queue gradings and run them on the worker threads.

use super::{Fanouts, IDLE_WAIT, Job, JobEnd, TREE_CACHE, session_map};
use crate::fanout::facts::{self};
use crate::fanout::{
    Cancel, Candidate, Fanout, Grade, GradeRequest, GraderRun, MAX_GRADING, Pending, RunnerError,
    TAIL_BYTES, Trigger, tampered,
};
use crate::hub::{Hub, HubError, HubEvent};
use crate::state::{SessionState, StateError};
use crate::status::AgentStatus;
use chrono::Utc;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

impl Fanouts {
    /// Pick up where the last hub process left off (gradings it did not
    /// finish run again; turns finished meanwhile are graded), then follow
    /// the hub's agents and run gradings until the hub goes away.
    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| std::io::Error::other("the hub is gone"))?;
        let events = hub.subscribe();
        let fanouts: Vec<Fanout> = hub.store().fanouts().to_vec();
        for fanout in &fanouts {
            for candidate in &fanout.candidates {
                if candidate.cleaned.is_some() {
                    continue;
                }
                if let Some(pending) = &candidate.pending {
                    if pending.started_at.is_some() {
                        let label = candidate.label.clone();
                        let _ = hub.store().update_fanout(&fanout.id, |f| {
                            if let Some(p) =
                                f.candidate_mut(&label).and_then(|c| c.pending.as_mut())
                            {
                                p.started_at = None;
                            }
                        });
                    }
                    eprintln!(
                        "fw hub: fan-out {} {}: grading again (cut short when the last hub process stopped or handed over)",
                        fanout.name, candidate.label
                    );
                    self.inner()
                        .queue
                        .push_back((fanout.id.clone(), candidate.label.clone()));
                    continue;
                }
                if let Some((AgentStatus::Done, spell)) = hub.turn_state(&candidate.session_id) {
                    self.inner()
                        .spells
                        .insert(candidate.session_id.clone(), spell);
                    if candidate.grade.is_none() {
                        let _ = self.queue(&hub, &fanout.id, &candidate.label, Trigger::Done);
                    }
                }
            }
        }
        drop(hub);
        self.on_sessions();
        let engine = Arc::clone(self);
        std::thread::Builder::new()
            .name("fw-hub-fanout".into())
            .spawn(move || engine.follow(events))?;
        for n in 0..MAX_GRADING {
            let engine = Arc::clone(self);
            std::thread::Builder::new()
                .name(format!("fw-hub-grade-{n}"))
                .spawn(move || engine.work())?;
        }
        Ok(())
    }

    /// Follow the hub's events: finished turns and ended agents queue
    /// gradings.
    fn follow(self: Arc<Self>, mut events: tokio::sync::broadcast::Receiver<HubEvent>) {
        loop {
            match events.blocking_recv() {
                Ok(HubEvent::Status { session_id }) => self.on_turn(&session_id),
                Ok(HubEvent::Sessions) => self.on_sessions(),
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    let sessions: Vec<String> = match self.hub.upgrade() {
                        Some(hub) => hub
                            .store()
                            .fanouts()
                            .iter()
                            .flat_map(|f| f.candidates.iter().map(|c| c.session_id.clone()))
                            .collect(),
                        None => break,
                    };
                    for session in sessions {
                        self.on_turn(&session);
                    }
                    self.on_sessions();
                }
                Err(RecvError::Closed) => break,
            }
            if self.control.is_stopped() {
                break;
            }
        }
    }

    /// The fan-out and candidate a session is, if any.
    fn candidate_of(hub: &Hub, session_id: &str) -> Option<(String, Candidate)> {
        hub.store().fanouts().iter().find_map(|f| {
            f.candidates
                .iter()
                .find(|c| c.session_id == session_id)
                .map(|c| (f.id.clone(), c.clone()))
        })
    }

    /// A status change: a new finished turn queues a grading.
    pub(crate) fn on_turn(&self, session_id: &str) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let Some((fanout, candidate)) = Self::candidate_of(&hub, session_id) else {
            return;
        };
        if candidate.cleaned.is_some() {
            return;
        }
        let Some((AgentStatus::Done, spell)) = hub.turn_state(session_id) else {
            return;
        };
        {
            let mut inner = self.inner();
            if inner.retiring.contains(session_id) {
                return;
            }
            if inner.spells.get(session_id) == Some(&spell) {
                return;
            }
            inner.spells.insert(session_id.to_owned(), spell);
            if candidate.pending.is_some() {
                inner.again.insert(session_id.to_owned());
                return;
            }
        }
        let _ = self.queue(&hub, &fanout, &candidate.label, Trigger::Done);
    }

    /// Sessions changed: a candidate whose agent ended before it was ever
    /// graded is graded as it is.
    pub(crate) fn on_sessions(&self) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let retiring = self.inner().retiring.clone();
        let ended: Vec<(String, String)> = {
            let store = hub.store();
            store
                .fanouts()
                .iter()
                .flat_map(|f| {
                    f.candidates
                        .iter()
                        .filter(|c| c.grade.is_none() && c.pending.is_none() && c.cleaned.is_none())
                        .filter(|c| !retiring.contains(&c.session_id))
                        .filter(|c| {
                            store
                                .session(&c.session_id)
                                .is_ok_and(|s| matches!(s.state, SessionState::Exited { .. }))
                        })
                        .map(|c| (f.id.clone(), c.label.clone()))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        for (fanout, label) in ended {
            let _ = self.queue(&hub, &fanout, &label, Trigger::Exit);
        }
    }

    /// Ask for a grading of `label` (`false`: one is pending already, or
    /// it was cleaned up).
    pub(crate) fn queue(
        &self,
        hub: &Hub,
        id: &str,
        label: &str,
        trigger: Trigger,
    ) -> Result<bool, HubError> {
        if self.control.is_stopped() {
            return Ok(false);
        }
        let queued = hub.store().update_fanout(id, |f| {
            let Some(candidate) = f.candidate_mut(label) else {
                return Err(StateError::NotFound(format!("candidate {label}")));
            };
            if candidate.cleaned.is_some() || candidate.pending.is_some() {
                return Ok(false);
            }
            candidate.pending = Some(Pending {
                trigger,
                queued_at: Utc::now(),
                started_at: None,
            });
            Ok(true)
        })??;
        if queued {
            self.inner()
                .queue
                .push_back((id.to_owned(), label.to_owned()));
            self.wake.notify_one();
            hub.emit(HubEvent::Fanout { id: id.to_owned() });
        }
        Ok(queued)
    }

    /// A grading worker: run jobs until the hub goes or gradings stop.
    fn work(self: Arc<Self>) {
        loop {
            if self.control.is_stopped() || self.hub.strong_count() == 0 {
                return;
            }
            let job = {
                let mut inner = self.inner();
                if inner.queue.is_empty() {
                    inner = self
                        .wake
                        .wait_timeout(inner, IDLE_WAIT)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                inner.queue.pop_front()
            };
            let Some(job) = job else { continue };
            let Some(cancel) = self.control.begin(IDLE_WAIT) else {
                // Paused (a handoff) or stopped: it stays pending.
                self.inner().queue.push_front(job);
                continue;
            };
            let end = match self.hub.upgrade() {
                Some(hub) => self.grade_one(&hub, &job, &cancel),
                None => JobEnd::Cancelled,
            };
            self.control.end(&cancel);
            match end {
                JobEnd::Cancelled => {
                    if !self.control.is_stopped() {
                        self.inner().queue.push_front(job);
                    }
                }
                JobEnd::Graded | JobEnd::Dropped => {}
            }
        }
    }

    /// Run one grading and record it.
    fn grade_one(&self, hub: &Arc<Hub>, job: &Job, cancel: &Cancel) -> JobEnd {
        let (id, label) = job;
        let Ok(fanout) = hub.store().fanout(id).cloned() else {
            return JobEnd::Dropped;
        };
        let Some(candidate) = fanout.candidate(label).cloned() else {
            return JobEnd::Dropped;
        };
        let Some(pending) = candidate.pending.clone() else {
            return JobEnd::Dropped;
        };
        if candidate.cleaned.is_some() {
            let _ = hub.store().update_fanout(id, |f| {
                if let Some(c) = f.candidate_mut(label) {
                    c.pending = None;
                }
            });
            return JobEnd::Dropped;
        }
        let started_at = Utc::now();
        let started = Instant::now();
        let _ = hub.store().update_fanout(id, |f| {
            if let Some(p) = f.candidate_mut(label).and_then(|c| c.pending.as_mut()) {
                p.started_at = Some(started_at);
            }
        });
        hub.emit(HubEvent::Fanout { id: id.clone() });
        let dir = candidate.worktree.path.clone();
        let before = facts::tree_state(&self.tools, &dir);
        let mut error = before.as_ref().err().cloned();
        // Its own work: from where it left the base (the fan-out's commit,
        // or the base's newer tip once rebased onto it).
        let since = facts::merge_base(&self.tools, &dir, &fanout.base)
            .unwrap_or_else(|| fanout.base_sha.clone());
        let (diff, changed) = match &before {
            Ok(state) => {
                match facts::diff_against(&self.tools, &dir, &since, &state.untracked_paths) {
                    Ok(found) => found,
                    Err(e) => {
                        error.get_or_insert(e);
                        Default::default()
                    }
                }
            }
            Err(_) => Default::default(),
        };
        // Checks git status cannot be trusted with: tracked files flagged
        // out of its sight, protected files compared by content, and the
        // grader scripts the fan-out pinned.
        let mut hashed: Vec<String> = Vec::new();
        if error.is_none() {
            match facts::hidden_paths(&self.tools, &dir) {
                Ok(hidden) if !hidden.is_empty() => {
                    error = Some(super::hidden_error(&hidden));
                }
                Ok(_) => {}
                Err(e) => error = Some(e),
            }
        }
        if error.is_none() {
            match facts::protected_changes(&self.tools, &dir, &since, &fanout.protect) {
                Ok(found) => hashed = found,
                Err(e) => error = Some(e),
            }
        }
        // Errors no agent can fix: the board offers it no Ask to fix.
        let mut operator = false;
        if error.is_none()
            && let Some(why) = super::pins_broken(&fanout.pins)
        {
            error = Some(why);
            operator = true;
        }
        let mut graders: Vec<GraderRun> = Vec::new();
        if error.is_none() {
            match &self.runner {
                None => {
                    error = Some("this hub has no grader runner (start it with `fw hub`)".into());
                    operator = true;
                }
                Some(runner) => {
                    let request = GradeRequest {
                        dir: &dir,
                        graders: &fanout.graders,
                        timeout: Duration::from_secs(fanout.timeout_secs),
                        tail_bytes: TAIL_BYTES,
                    };
                    match runner.run(&request, cancel) {
                        Ok(mut runs) => {
                            // Held out is the fan-out's to say, not the
                            // runner's.
                            for (run, spec) in runs.iter_mut().zip(&fanout.graders) {
                                run.held_out = spec.held_out;
                            }
                            graders = runs;
                        }
                        Err(RunnerError::Cancelled) => {
                            self.unstart(hub, id, label);
                            return JobEnd::Cancelled;
                        }
                        Err(RunnerError::Failed(e)) => error = Some(e),
                    }
                }
            }
        }
        if cancel.is_cancelled() {
            self.unstart(hub, id, label);
            return JobEnd::Cancelled;
        }
        // The graders not run (the first failure ends a run).
        for spec in fanout.graders.iter().skip(graders.len()) {
            graders.push(GraderRun::skipped(spec));
        }
        let after = facts::tree_state(&self.tools, &dir);
        let moved = match (&before, &after) {
            (Ok(b), Ok(a)) => b.fingerprint != a.fingerprint,
            _ => false,
        };
        let usage = hub
            .session(&candidate.session_id)
            .ok()
            .and_then(|v| v.usage);
        let (cost_usd, tokens) = match &usage {
            Some(u) => (
                u.total.priced.then_some(u.total.cost_usd),
                u.total.total_tokens,
            ),
            None => (
                candidate.grade.as_ref().and_then(|g| g.cost_usd),
                candidate.grade.as_ref().map_or(0, |g| g.tokens),
            ),
        };
        let mut tampered = tampered(&fanout.protect, &changed);
        tampered.extend(hashed);
        tampered.sort();
        tampered.dedup();
        let passed =
            error.is_none() && tampered.is_empty() && graders.iter().all(GraderRun::passed);
        for run in &mut graders {
            if run.passed() {
                // Only a failure's output is shown or pasted: a pass keeps
                // none (state.json stays small).
                run.stdout.clear();
                run.stderr.clear();
                run.stdout_truncated = false;
                run.stderr_truncated = false;
            }
        }
        let state = before.ok();
        let grade = Grade {
            trigger: pending.trigger,
            started_at,
            finished_at: Utc::now(),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            head: state.as_ref().and_then(|s| s.head.clone()),
            dirty: state.as_ref().map_or(0, |s| s.dirty),
            untracked: state.as_ref().map_or(0, |s| {
                u32::try_from(s.untracked_paths.len()).unwrap_or(u32::MAX)
            }),
            tree: state
                .as_ref()
                .map(|s| s.fingerprint.clone())
                .unwrap_or_default(),
            moved,
            passed,
            graders,
            tampered,
            diff,
            cost_usd,
            tokens,
            error,
            operator,
        };
        let summary = match grade.failure() {
            None => format!("pass ({} lines changed)", grade.diff.lines()),
            Some(why) => format!("fail: {why}"),
        };
        let recorded = hub.store().update_fanout(id, |f| {
            if let Some(c) = f.candidate_mut(label) {
                c.grade = Some(grade);
                c.pending = None;
                c.gradings += 1;
            }
        });
        self.inner().trees.remove(&dir);
        let saved = recorded.is_ok();
        match recorded {
            Ok(()) => eprintln!("fw hub: fan-out {} {label}: graded {summary}", fanout.name),
            Err(e) => eprintln!(
                "fw hub: fan-out {} {label}: grade not saved: {e}",
                fanout.name
            ),
        }
        hub.emit(HubEvent::Fanout { id: id.clone() });
        let again = self.inner().again.remove(&candidate.session_id);
        if again {
            let _ = self.queue(hub, id, label, Trigger::Done);
        }
        if saved {
            self.announce_if_settled(hub, id);
        }
        JobEnd::Graded
    }

    /// `fanout.finished` (for plugins) when the grading just recorded left
    /// the fan-out settled: no agent at work, nothing queued or grading.
    /// Each grading that settles it announces it again (a re-grade, a fix
    /// round's grade).
    fn announce_if_settled(&self, hub: &Arc<Hub>, id: &str) {
        let Ok(fanout) = hub.store().fanout(id).cloned() else {
            return;
        };
        let view = self.build(&fanout, &session_map(hub), &hub.workspaces(), TREE_CACHE);
        if !view.settled {
            return;
        }
        let gradings: Vec<(String, u32)> = fanout
            .candidates
            .iter()
            .map(|c| (c.label.clone(), c.gradings))
            .collect();
        {
            let mut inner = self.inner();
            if inner.announced.get(id) == Some(&gradings) {
                return;
            }
            inner.announced.insert(id.to_owned(), gradings);
        }
        hub.emit_lifecycle("fanout.finished", super::finished_payload(&view));
    }

    /// A cancelled grading is queued again, not running.
    fn unstart(&self, hub: &Hub, id: &str, label: &str) {
        let _ = hub.store().update_fanout(id, |f| {
            if let Some(p) = f.candidate_mut(label).and_then(|c| c.pending.as_mut()) {
                p.started_at = None;
            }
        });
        hub.emit(HubEvent::Fanout { id: id.to_owned() });
    }
}
