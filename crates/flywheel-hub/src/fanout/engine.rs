//! The fan-out engine: launches a fan-out's agents, follows their turns,
//! queues and runs gradings (at most [`MAX_GRADING`] at once, hub-wide),
//! builds the ranked board, and carries out the board's actions (re-grade,
//! ask to fix, rebase, pick the winner, clean up the losers).
//!
//! It holds the hub weakly: it never keeps a hub alive (a hub's serve lock
//! is released when it drops), and its threads end when the hub does or
//! when the hub's [`GradingControl`] is stopped.

use super::facts::{self, TreeState};
use super::{
    Cancel, Candidate, Cleaned, Counts, CreateFanout, Fanout, FanoutView, Grade, GradeRequest,
    GraderRun, GraderRunner, GradingControl, MAX_GRADING, Pending, Phase, RunnerError, TAIL_BYTES,
    Trigger, Verdict, Winner, fix_prompt, phase_of, rank, tampered, task_name, verdict_of,
};
use crate::agent::SessionKind;
use crate::hub::{CreateSession, Hub, HubError, HubEvent, NewWorktree, RemoveOptions, SessionView};
use crate::review::{Action, ActionReport, MergeVia, Plan, Tools};
use crate::state::{SessionState, StateError, new_id};
use crate::status::AgentStatus;
use crate::transcript::PasteOutcome;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

/// How long a worker waits for a job (or for a pause to end) before it
/// looks again whether the hub is still there.
const IDLE_WAIT: Duration = Duration::from_secs(1);
/// How long a worktree's status read is reused by one board.
const TREE_CACHE: Duration = Duration::from_secs(2);
/// ... and by the list of every fan-out (refetched on every change by
/// every browser: its git reads must not scale with events).
const LIST_TREE_CACHE: Duration = Duration::from_secs(30);

/// The fan-outs of one hub (see the module doc).
pub struct Fanouts {
    hub: Weak<Hub>,
    runner: Option<Arc<dyn GraderRunner>>,
    tools: Tools,
    control: Arc<GradingControl>,
    inner: Mutex<Inner>,
    wake: Condvar,
}

impl std::fmt::Debug for Fanouts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fanouts")
            .field("runner", &self.runner.as_ref().map(|r| r.name()))
            .finish_non_exhaustive()
    }
}

/// One grading to run: (fan-out id, candidate label).
type Job = (String, String);

#[derive(Default)]
struct Inner {
    queue: VecDeque<Job>,
    /// Session id -> the status spell of the finished turn last acted on
    /// (a new spell is a new turn).
    spells: HashMap<String, u64>,
    /// Sessions whose agent finished another turn while being graded:
    /// graded again once that grading ends.
    again: HashSet<String>,
    /// Worktree -> its last status read, for the board.
    trees: HashMap<PathBuf, (Instant, Result<TreeState, String>)>,
    /// Sessions a cleanup is removing: their agent's exit grades nothing.
    retiring: HashSet<String>,
    /// Fan-out id -> the gradings (`label`, count) its last
    /// `fanout.finished` reported: two gradings ending together announce
    /// once.
    announced: HashMap<String, Vec<(String, u32)>>,
}

/// How a grading job ended.
enum JobEnd {
    Graded,
    /// Cancelled (a pause or a stop): it runs again later.
    Cancelled,
    /// Nothing to do any more (removed, cleaned, already graded).
    Dropped,
}

/// What "ask candidate to fix" pastes, and into which session.
#[derive(Debug, Clone, Serialize)]
pub struct FixPlan {
    pub candidate: String,
    pub session_id: String,
    pub session: String,
    pub prompt: String,
    /// Set once pasted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste: Option<PasteOutcome>,
}

/// What picking a candidate would do.
#[derive(Debug, Clone, Serialize)]
pub struct PickPlan {
    pub fanout: String,
    pub candidate: String,
    pub session_id: String,
    /// Why it cannot be merged now (the fan-out's check, then the merge
    /// plan's).
    pub refusal: Option<String>,
    /// The base moved: rebase the candidate first (it is graded again).
    pub needs_rebase: bool,
    /// The review merge plan (`git merge --ff-only` in the main checkout);
    /// confirm with its fingerprint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge: Option<Plan>,
    /// What cleaning up the others would do afterwards.
    pub losers: Vec<CleanupItem>,
}

/// What picking did.
#[derive(Debug, Clone, Serialize)]
pub struct PickReport {
    pub merge: ActionReport,
    pub fanout: FanoutView,
    pub losers: Vec<CleanupItem>,
}

/// A candidate's rebase onto the base: the plan, or what it did.
#[derive(Debug, Clone, Serialize)]
pub struct RebaseReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebase: Option<ActionReport>,
    pub fanout: FanoutView,
}

/// `POST /api/fanouts/{id}/cleanup`.
#[derive(Debug, Clone, Deserialize)]
pub struct CleanupRequest {
    /// Labels; default: every candidate but the winner.
    #[serde(default)]
    pub candidates: Option<Vec<String>>,
    /// Remove the worktrees (and their sessions); `false` only stops the
    /// sessions.
    #[serde(default = "super::default_true")]
    pub remove_worktrees: bool,
    /// Remove worktrees that hold work nothing else has.
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub dry_run: bool,
}

/// One candidate's cleanup.
#[derive(Debug, Clone, Serialize)]
pub struct CleanupItem {
    pub candidate: String,
    pub session: Option<String>,
    pub path: PathBuf,
    pub branch: String,
    /// `remove_worktree` or `stop`.
    pub action: &'static str,
    /// It was carried out (never on a dry run).
    pub done: bool,
    /// Why it needs `force` (unique work would be lost) or cannot run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_deleted: Option<bool>,
}

/// What a cleanup did.
#[derive(Debug, Clone, Serialize)]
pub struct CleanupReport {
    pub items: Vec<CleanupItem>,
    pub fanout: FanoutView,
}

fn invalid(message: impl Into<String>) -> HubError {
    StateError::Invalid(message.into()).into()
}

fn conflict(message: impl Into<String>) -> HubError {
    StateError::Conflict(message.into()).into()
}

fn session_state(state: &SessionState) -> &'static str {
    match state {
        SessionState::Running => "running",
        SessionState::Exited { .. } => "exited",
        SessionState::Stopped { .. } => "stopped",
    }
}

impl Fanouts {
    /// The engine for `hub`; `runner` grades (`None`: fan-outs can be
    /// listed but not started or graded). Call [`Fanouts::start`] to run it.
    pub fn new(hub: &Arc<Hub>, runner: Option<Arc<dyn GraderRunner>>, tools: Tools) -> Arc<Self> {
        Arc::new(Self {
            hub: Arc::downgrade(hub),
            runner,
            tools,
            control: Arc::clone(hub.grading()),
            inner: Mutex::new(Inner::default()),
            wake: Condvar::new(),
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn hub(&self) -> Result<Arc<Hub>, HubError> {
        self.hub
            .upgrade()
            .ok_or_else(|| HubError::Internal("the hub is shutting down".into()))
    }

    /// What runs the graders, as the board shows it.
    pub fn runner_name(&self) -> Option<String> {
        self.runner.as_ref().map(|r| r.name())
    }

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
    fn on_turn(&self, session_id: &str) {
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
    fn on_sessions(&self) {
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
    fn queue(&self, hub: &Hub, id: &str, label: &str, trigger: Trigger) -> Result<bool, HubError> {
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

    // --- create -------------------------------------------------------------

    /// Launch a fan-out: every candidate's worktree cut from one commit of
    /// the base branch, its agent started on the prompt. All or nothing:
    /// a failure removes what was started.
    pub fn create(&self, request: CreateFanout) -> Result<FanoutView, HubError> {
        let hub = self.hub()?;
        let valid = request.validated().map_err(invalid)?;
        if self.runner.is_none() {
            return Err(conflict(
                "this hub cannot grade (no grader runner): start it with `fw hub start`",
            ));
        }
        if hub.store().fanouts().len() >= super::MAX_FANOUTS {
            return Err(conflict(format!(
                "the hub keeps at most {} fan-outs; remove finished ones first (fw hub fanout rm)",
                super::MAX_FANOUTS
            )));
        }
        let workspace = hub.workspace(&request.workspace_id)?;
        let folder = match &request.folder {
            Some(path) => {
                let canonical = path
                    .canonicalize()
                    .map_err(|e| invalid(format!("folder {}: {e}", path.display())))?;
                if !workspace.has_folder(&canonical) {
                    return Err(invalid(format!(
                        "folder {} is not part of workspace {}",
                        canonical.display(),
                        workspace.name
                    )));
                }
                canonical
            }
            None => workspace
                .folders
                .iter()
                .find(|f| f.git && !f.missing)
                .map(|f| f.path.clone())
                .ok_or_else(|| {
                    invalid(format!(
                        "workspace {} has no git folder to fan out in",
                        workspace.name
                    ))
                })?,
        };
        if !crate::state::is_git_root(&folder) {
            return Err(invalid(crate::git::not_a_repo_root(&folder)));
        }
        let base = match request
            .base
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            Some(base) => base.to_owned(),
            None => facts::current_branch(&self.tools, &folder).ok_or_else(|| {
                invalid(format!(
                    "{}'s HEAD is detached: name the base branch to fan out from",
                    folder.display()
                ))
            })?,
        };
        if base.starts_with('-') || !facts::is_local_branch(&self.tools, &folder, &base) {
            return Err(invalid(format!(
                "base {base:?} is not a local branch of {} (the winner is merged into it)",
                folder.display()
            )));
        }
        let base_sha = facts::resolve_commit(&self.tools, &folder, &base)
            .ok_or_else(|| invalid(format!("base {base:?} names no commit")))?;
        let kinds = hub.kinds();
        for (kind, _) in &valid.candidates {
            if !kinds.iter().any(|k| k.kind == *kind && k.available) {
                return Err(invalid(format!(
                    "{} is not installed on this hub's PATH",
                    kind.as_str()
                )));
            }
        }
        let id = new_id();
        let tag = &id[..4];
        let sent = super::sent_prompt(&valid.prompt, request.commit_instruction);
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut started: Vec<SessionView> = Vec::new();
        // A launch that fails part-way takes back everything it started:
        // the sessions, their worktrees, and the branches cut for them
        // (whatever an agent committed there in the meantime is not any
        // fan-out's work).
        let undo = |started: &[SessionView]| {
            for view in started {
                let _ = hub.remove_session_with(
                    &view.record.id,
                    RemoveOptions {
                        remove_worktree: true,
                        force: true,
                    },
                );
                if let Some(worktree) = &view.record.worktree
                    && !worktree.path.exists()
                {
                    crate::git::delete_branch(&worktree.repo, &worktree.branch);
                }
            }
        };
        let (pins, protect) =
            super::pin_graders(&valid.graders, &folder, valid.protect.clone()).map_err(invalid)?;
        for (i, (kind, launch)) in valid.candidates.iter().enumerate() {
            let label = format!("c{}", i + 1);
            let task = task_name(&valid.name, tag, &label);
            // The label first: a sidebar cuts long names at the end.
            let session_name = format!(
                "{label}-{}",
                task.strip_suffix(&format!("-{label}")).unwrap_or(&task)
            );
            let created = hub.create_session(CreateSession {
                workspace_id: workspace.id.clone(),
                repo: Some(folder.clone()),
                name: Some(session_name),
                kind: *kind,
                command: None,
                cols: request.cols,
                rows: request.rows,
                worktree: Some(NewWorktree {
                    task,
                    base: Some(base.clone()),
                    // Cut at the commit read above, whatever the base does
                    // meanwhile: every candidate starts from it.
                    at: Some(base_sha.clone()),
                }),
                prompt: Some(sent.clone()),
                launch: launch.clone(),
            });
            let view = match created {
                Ok(view) => view,
                Err(e) => {
                    undo(&started);
                    return Err(e);
                }
            };
            started.push(view.clone());
            let Some(worktree) = view.record.worktree.clone() else {
                undo(&started);
                return Err(HubError::Internal(
                    "a fan-out session has no worktree".into(),
                ));
            };
            candidates.push(Candidate {
                label,
                session_id: view.record.id.clone(),
                kind: *kind,
                // As launched: hub.toml's [agents.*] defaults fill what the
                // request left unset.
                launch: view.record.launch.clone().unwrap_or_default(),
                worktree,
                pending: None,
                grade: None,
                gradings: 0,
                fixes: 0,
                cleaned: None,
            });
        }
        let fanout = Fanout {
            id: id.clone(),
            name: valid.name,
            workspace_id: workspace.id.clone(),
            repo: folder,
            base,
            base_sha,
            prompt: valid.prompt,
            commit_instruction: request.commit_instruction,
            graders: valid.graders,
            timeout_secs: valid.timeout_secs,
            protect,
            pins,
            created_at: Utc::now(),
            candidates,
            winner: None,
            origin: request.origin.clone(),
            promoted: Vec::new(),
        };
        if let Err(e) = hub.store().insert_fanout(fanout.clone()) {
            undo(&started);
            return Err(e.into());
        }
        eprintln!(
            "fw hub: fan-out {} ({id}): {} agents on {} at {} ({})",
            fanout.name,
            fanout.candidates.len(),
            fanout.base,
            &fanout.base_sha[..12.min(fanout.base_sha.len())],
            fanout
                .candidates
                .iter()
                .map(|c| format!(
                    "{} {}{}",
                    c.label,
                    c.kind.as_str(),
                    c.launch
                        .model
                        .as_deref()
                        .map(|m| format!(":{m}"))
                        .unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        hub.emit(HubEvent::Fanout { id: id.clone() });
        // A fast agent may have finished before the record existed.
        for candidate in &fanout.candidates {
            self.on_turn(&candidate.session_id);
        }
        self.on_sessions();
        self.view(&id)
    }

    // --- views --------------------------------------------------------------

    /// A worktree's status, reused for `max_age`.
    fn tree(&self, dir: &Path, max_age: Duration) -> Result<TreeState, String> {
        if let Some((at, state)) = self.inner().trees.get(dir)
            && at.elapsed() < max_age
        {
            return state.clone();
        }
        let state = facts::tree_state(&self.tools, dir);
        self.inner()
            .trees
            .insert(dir.to_path_buf(), (Instant::now(), state.clone()));
        state
    }

    /// Why `grade` no longer describes the worktree (`None`: it does).
    /// `max_age`: how old a cached status read may be (`None`: read now).
    fn staleness(
        &self,
        candidate: &Candidate,
        grade: &Grade,
        max_age: Option<Duration>,
    ) -> Option<String> {
        if grade.moved {
            return Some("it changed while its graders ran".into());
        }
        let dir = &candidate.worktree.path;
        let now = match max_age {
            None => facts::tree_state(&self.tools, dir),
            Some(age) => self.tree(dir, age),
        };
        match now {
            Err(e) => Some(format!("its worktree cannot be read: {e}")),
            Ok(now) if now.fingerprint == grade.tree => None,
            Ok(now) if now.head != grade.head => Some("new commits since it was graded".into()),
            Ok(_) => Some("its working tree changed since it was graded".into()),
        }
    }

    /// Every fan-out, newest first, as summaries: no grader output (fetch
    /// one board for it) and worktree reads reused for [`LIST_TREE_CACHE`].
    pub fn list(&self) -> Result<Vec<FanoutView>, HubError> {
        let hub = self.hub()?;
        let fanouts: Vec<Fanout> = hub.store().fanouts().to_vec();
        let sessions = session_map(&hub);
        let workspaces = hub.workspaces();
        let mut views: Vec<FanoutView> = fanouts
            .iter()
            .map(|f| {
                let mut view = self.build(f, &sessions, &workspaces, LIST_TREE_CACHE);
                view.summarize();
                view
            })
            .collect();
        views.reverse();
        Ok(views)
    }

    /// One fan-out as stored.
    pub fn record(&self, id: &str) -> Result<Fanout, HubError> {
        Ok(self.hub()?.store().fanout(id)?.clone())
    }

    /// Note a task file written from fan-out `id`.
    pub fn note_promotion(&self, id: &str, promotion: super::Promotion) -> Result<(), HubError> {
        let hub = self.hub()?;
        hub.store()
            .update_fanout(id, |f| f.promoted.push(promotion))?;
        hub.emit(HubEvent::Fanout { id: id.to_owned() });
        Ok(())
    }

    /// One fan-out's board.
    pub fn view(&self, id: &str) -> Result<FanoutView, HubError> {
        let hub = self.hub()?;
        let fanout = hub.store().fanout(id)?.clone();
        Ok(self.build(&fanout, &session_map(&hub), &hub.workspaces(), TREE_CACHE))
    }

    fn build(
        &self,
        fanout: &Fanout,
        sessions: &HashMap<String, SessionView>,
        workspaces: &[crate::state::Workspace],
        tree_age: Duration,
    ) -> FanoutView {
        let mut views: Vec<super::CandidateView> = fanout
            .candidates
            .iter()
            .map(|c| {
                let session = sessions.get(&c.session_id);
                let stale = match (&c.grade, &c.pending, &c.cleaned) {
                    (Some(grade), None, None) => self.staleness(c, grade, Some(tree_age)),
                    _ => None,
                };
                let phase = phase_of(
                    c,
                    session.map(|s| {
                        (
                            session_state(&s.record.state),
                            s.record.runs_agent(),
                            s.agent.as_ref().map(|a| a.status),
                        )
                    }),
                );
                let verdict = verdict_of(c, stale.as_deref());
                let (cost_usd, tokens) = match session.and_then(|s| s.usage.as_ref()) {
                    Some(u) => (
                        u.total.priced.then_some(u.total.cost_usd),
                        u.total.total_tokens,
                    ),
                    None => (
                        c.grade.as_ref().and_then(|g| g.cost_usd),
                        c.grade.as_ref().map_or(0, |g| g.tokens),
                    ),
                };
                let head = if c.cleaned.is_some() {
                    None
                } else {
                    self.tree(&c.worktree.path, tree_age)
                        .ok()
                        .and_then(|t| t.head)
                };
                let winner = fanout.winner.as_ref().is_some_and(|w| w.label == c.label);
                super::CandidateView {
                    label: c.label.clone(),
                    session_id: c.session_id.clone(),
                    session: session.map(|s| s.record.name.clone()),
                    kind: c.kind,
                    model: c.launch.model.clone(),
                    branch: c.worktree.branch.clone(),
                    path: c.worktree.path.clone(),
                    phase,
                    verdict,
                    rank: 0,
                    why: Vec::new(),
                    agent: session.and_then(|s| s.agent.clone()),
                    session_state: session.map(|s| {
                        if s.record.state.is_running() && !s.record.runs_agent() {
                            // Restored without its agent (a fresh shell).
                            "shell".to_owned()
                        } else {
                            session_state(&s.record.state).to_owned()
                        }
                    }),
                    pickable: verdict == Verdict::Pass
                        && fanout.winner.is_none()
                        && session.is_some()
                        && c.grade
                            .as_ref()
                            .is_some_and(|g| g.clean() && !g.no_change()),
                    grade: c.grade.clone(),
                    stale,
                    pending: c.pending.clone(),
                    head,
                    cost_usd,
                    tokens,
                    gradings: c.gradings,
                    fixes: c.fixes,
                    winner,
                    cleaned: c.cleaned.clone(),
                }
            })
            .collect();
        rank(&mut views);
        let mut counts = Counts {
            candidates: views.len(),
            ..Counts::default()
        };
        for v in &views {
            match v.verdict {
                Verdict::Pass => counts.passing += 1,
                Verdict::Fail => counts.failing += 1,
                Verdict::Pending => counts.grading += 1,
                Verdict::Stale | Verdict::None => {}
            }
            if matches!(v.phase, Phase::Working | Phase::Blocked | Phase::Waiting) {
                counts.running += 1;
            }
        }
        let mut warnings = Vec::new();
        if let Some(noop) = views.iter().find(|v| {
            v.verdict == Verdict::Pass
                && v.grade.as_ref().is_some_and(|g| g.clean() && g.no_change())
        }) {
            warnings.push(format!(
                "the graders pass on {} unchanged ({} changes nothing and passes): they do not \
                 tell the candidates apart",
                fanout.base, noop.label
            ));
        }
        let settled = views.iter().all(|v| {
            matches!(v.verdict, Verdict::Pass | Verdict::Fail)
                || matches!(v.phase, Phase::Ended | Phase::Removed)
        });
        FanoutView {
            id: fanout.id.clone(),
            name: fanout.name.clone(),
            workspace_id: fanout.workspace_id.clone(),
            workspace: workspaces
                .iter()
                .find(|w| w.id == fanout.workspace_id)
                .map(|w| w.name.clone()),
            folder: fanout.repo.clone(),
            repo: fanout.repo.clone(),
            base: fanout.base.clone(),
            base_sha: fanout.base_sha.clone(),
            prompt: fanout.prompt.clone(),
            sent_prompt: fanout.sent_prompt(),
            graders: fanout.graders.clone(),
            timeout_secs: fanout.timeout_secs,
            protect: fanout.protect.clone(),
            created_at: fanout.created_at,
            winner: fanout.winner.clone(),
            leader: views
                .first()
                .filter(|v| v.verdict == Verdict::Pass)
                .map(|v| v.label.clone()),
            candidates: views,
            counts,
            settled,
            graded_at: fanout.graded_at(),
            runner: self.runner_name(),
            warnings,
            summary: false,
            origin: fanout.origin.clone(),
            promoted: fanout.promoted.iter().map(super::Promotion::view).collect(),
        }
    }

    /// Forget a fan-out (its sessions and worktrees stay).
    pub fn remove(&self, id: &str) -> Result<(), HubError> {
        let hub = self.hub()?;
        let removed = hub.store().remove_fanout(id)?;
        {
            let mut inner = self.inner();
            inner.queue.retain(|(f, _)| f != id);
            inner.announced.remove(id);
        }
        eprintln!("fw hub: fan-out {} ({id}) forgotten", removed.name);
        hub.emit(HubEvent::Fanout { id: id.to_owned() });
        Ok(())
    }

    fn get(&self, hub: &Hub, id: &str, label: &str) -> Result<(Fanout, Candidate), HubError> {
        let fanout = hub.store().fanout(id)?.clone();
        let candidate = fanout.candidate(label).cloned().ok_or_else(|| {
            HubError::from(StateError::NotFound(format!(
                "candidate {label} of fan-out {}",
                fanout.name
            )))
        })?;
        Ok((fanout, candidate))
    }

    // --- actions ------------------------------------------------------------

    /// Grade `label` again now.
    pub fn grade(&self, id: &str, label: &str) -> Result<FanoutView, HubError> {
        let hub = self.hub()?;
        let (_, candidate) = self.get(&hub, id, label)?;
        if candidate.cleaned.is_some() {
            return Err(conflict(format!("{label}'s worktree was removed")));
        }
        if self.runner.is_none() {
            return Err(conflict("this hub cannot grade (no grader runner)"));
        }
        if !self.queue(&hub, id, label, Trigger::Manual)? {
            return Err(conflict(format!("{label} is being graded already")));
        }
        self.view(id)
    }

    /// The prompt asking `label`'s agent to fix what failed; with `run`,
    /// paste it (then Enter). The agent's next finished turn is graded.
    pub fn fix(&self, id: &str, label: &str, run: bool) -> Result<FixPlan, HubError> {
        let hub = self.hub()?;
        let (fanout, candidate) = self.get(&hub, id, label)?;
        let grade = candidate
            .grade
            .as_ref()
            .ok_or_else(|| conflict(format!("{label} has not been graded")))?;
        if grade.operator {
            return Err(conflict(format!(
                "{label} could not be graded, and that is not its agent's to fix: {}",
                grade.error.as_deref().unwrap_or("grading failed")
            )));
        }
        let prompt =
            fix_prompt(grade).ok_or_else(|| conflict(format!("{label} passes: nothing to fix")))?;
        let session = hub.session(&candidate.session_id)?;
        if session.record.state.is_running() && !session.record.runs_agent() {
            return Err(conflict(format!(
                "{label}'s agent did not come back after the hub restarted ({} runs a plain shell \
                 now): relaunch the agent from its session, then ask again",
                session.record.name
            )));
        }
        if !session.record.runs_agent() || !session.record.state.is_running() {
            return Err(conflict(format!(
                "{label}'s agent ({}) is not running: relaunch it first",
                session.record.name
            )));
        }
        let mut plan = FixPlan {
            candidate: label.to_owned(),
            session_id: candidate.session_id.clone(),
            session: session.record.name.clone(),
            prompt,
            paste: None,
        };
        if run {
            let pasted = hub.paste(&candidate.session_id, &plan.prompt, true)?;
            plan.paste = Some(pasted);
            let _ = hub.store().update_fanout(id, |f| {
                if let Some(c) = f.candidate_mut(label) {
                    c.fixes += 1;
                }
            });
            eprintln!(
                "fw hub: fan-out {} {label}: asked {} to fix: {}",
                fanout.name,
                session.record.name,
                grade.failure().unwrap_or_default()
            );
            hub.emit(HubEvent::Fanout { id: id.to_owned() });
        }
        Ok(plan)
    }

    /// Why `candidate` cannot be picked now (`None`: it can), reading its
    /// worktree afresh.
    fn pick_refusal(&self, fanout: &Fanout, candidate: &Candidate) -> Option<String> {
        if let Some(winner) = &fanout.winner {
            return Some(format!("{} was picked already", winner.label));
        }
        if candidate.cleaned.is_some() {
            return Some("its worktree was removed".into());
        }
        if candidate.pending.is_some() {
            return Some("a grading is queued or running: wait for it".into());
        }
        let Some(grade) = &candidate.grade else {
            return Some("it has not been graded".into());
        };
        if let Some(why) = grade.failure() {
            return Some(format!("it does not pass: {why}"));
        }
        if !grade.clean() {
            return Some(
                "it was graded with uncommitted changes: commit them (Review, Commit all), then \
                 re-grade"
                    .into(),
            );
        }
        if grade.no_change() {
            return Some(format!(
                "it changes nothing against {}: there is nothing to merge",
                fanout.base
            ));
        }
        if let Some(why) = self.staleness(candidate, grade, None) {
            return Some(format!("{why}: re-grade it first"));
        }
        // The merge lands the candidate's branch: it must be the commit
        // that passed, checked out in its worktree.
        let dir = &candidate.worktree.path;
        let branch = &candidate.worktree.branch;
        match facts::current_branch(&self.tools, dir) {
            Some(on) if on == *branch => {}
            on => {
                return Some(format!(
                    "its worktree is on {}, not its fan-out branch {branch}: what passed is not \
                     what would be merged (git switch {branch} there, then re-grade)",
                    on.as_deref().unwrap_or("a detached HEAD")
                ));
            }
        }
        let tip = facts::resolve_commit(&self.tools, dir, &format!("refs/heads/{branch}"));
        if tip.is_none() || tip != grade.head {
            return Some(format!(
                "branch {branch} is at {}, not the commit that passed ({}): re-grade it first",
                tip.as_deref().map_or("nothing", short_sha),
                grade.head.as_deref().map_or("none", short_sha)
            ));
        }
        if let Some(why) = super::pins_broken(&fanout.pins) {
            return Some(why);
        }
        None
    }

    /// The merge plan's refusal when it would not land exactly `head`.
    fn merges_graded(merge: &Plan, head: Option<&str>) -> Option<String> {
        let planned = planned_merge(merge);
        if planned.is_some() && planned == head {
            return None;
        }
        Some(format!(
            "the merge would land {}, not the commit that passed ({}): re-grade it first",
            planned.map_or("nothing", short_sha),
            head.map_or("none", short_sha)
        ))
    }

    fn losers(&self, hub: &Hub, fanout: &Fanout, winner: &str) -> Vec<CleanupItem> {
        fanout
            .candidates
            .iter()
            .filter(|c| c.label != winner && c.cleaned.is_none())
            .map(|c| self.cleanup_one(hub, fanout, c, true, false, true))
            .collect()
    }

    /// What merging `label` into the base would run, or why it cannot.
    pub fn pick_plan(&self, id: &str, label: &str) -> Result<PickPlan, HubError> {
        let hub = self.hub()?;
        let (fanout, candidate) = self.get(&hub, id, label)?;
        let mut plan = PickPlan {
            fanout: fanout.id.clone(),
            candidate: label.to_owned(),
            session_id: candidate.session_id.clone(),
            refusal: self.pick_refusal(&fanout, &candidate),
            needs_rebase: false,
            merge: None,
            losers: self.losers(&hub, &fanout, label),
        };
        if plan.refusal.is_some() {
            return Ok(plan);
        }
        if hub.session(&candidate.session_id).is_err() {
            plan.refusal = Some("its session was removed: merge it from the main checkout".into());
            return Ok(plan);
        }
        let merge = hub.review_plan_via(
            &self.tools,
            &candidate.session_id,
            &Action::Merge,
            MergeVia::FanoutPick,
        )?;
        plan.needs_rebase = merge.needs_rebase;
        let graded = candidate.grade.as_ref().and_then(|g| g.head.as_deref());
        plan.refusal = merge
            .refusal
            .clone()
            .or_else(|| self.pick_refusal(&fanout, &candidate))
            // The plan merges the commit it read: the one that passed?
            .or_else(|| Self::merges_graded(&merge, graded));
        plan.merge = Some(merge);
        Ok(plan)
    }

    /// Merge `label` (the plan with fingerprint `confirm`) and record it
    /// the winner.
    pub fn pick(&self, id: &str, label: &str, confirm: &str) -> Result<PickReport, HubError> {
        let hub = self.hub()?;
        let (fanout, candidate) = self.get(&hub, id, label)?;
        if let Some(why) = self.pick_refusal(&fanout, &candidate) {
            return Err(conflict(format!("cannot pick {label}: {why}")));
        }
        let head = candidate
            .grade
            .as_ref()
            .and_then(|g| g.head.clone())
            .ok_or_else(|| conflict(format!("{label} has no commit to merge")))?;
        // `confirm` must be a plan that merges exactly the graded commit: the
        // run below re-plans and refuses any other fingerprint, and equal
        // fingerprints plan the same commit.
        let planned = hub.review_plan_via(
            &self.tools,
            &candidate.session_id,
            &Action::Merge,
            MergeVia::FanoutPick,
        )?;
        if planned.fingerprint != confirm {
            return Err(conflict(format!(
                "{label}'s merge plan changed since it was shown: look again (fw hub fanout pick \
                 without --yes, or reopen the dialog)"
            )));
        }
        if let Some(why) = Self::merges_graded(&planned, Some(&head)) {
            return Err(conflict(format!("cannot pick {label}: {why}")));
        }
        let merge = hub.review_run_via(
            &self.tools,
            &candidate.session_id,
            &Action::Merge,
            confirm,
            MergeVia::FanoutPick,
        )?;
        if merge.outcome.ok {
            let winner = Winner {
                label: label.to_owned(),
                head: merge.outcome.head.clone().unwrap_or(head),
                base: fanout.base.clone(),
                at: Utc::now(),
            };
            hub.store().update_fanout(id, |f| f.winner = Some(winner))?;
            eprintln!(
                "fw hub: fan-out {} ({id}): picked {label}, merged into {}",
                fanout.name, fanout.base
            );
            hub.emit(HubEvent::Fanout { id: id.to_owned() });
        }
        let fanout = hub.store().fanout(id)?.clone();
        Ok(PickReport {
            losers: self.losers(&hub, &fanout, label),
            merge,
            fanout: self.view(id)?,
        })
    }

    /// Rebase `label` onto the base (dry run: the plan); a rebase that
    /// went through queues a grading of the new commits.
    pub fn rebase(
        &self,
        id: &str,
        label: &str,
        confirm: Option<&str>,
    ) -> Result<RebaseReport, HubError> {
        let hub = self.hub()?;
        let (fanout, candidate) = self.get(&hub, id, label)?;
        if let Some(winner) = &fanout.winner {
            return Err(conflict(format!("{} was picked already", winner.label)));
        }
        if candidate.cleaned.is_some() {
            return Err(conflict(format!("{label}'s worktree was removed")));
        }
        if candidate.pending.is_some() {
            return Err(conflict(format!(
                "{label} is being graded: wait for it to finish, then rebase"
            )));
        }
        let Some(confirm) = confirm else {
            let plan = hub.review_plan(&self.tools, &candidate.session_id, &Action::Rebase)?;
            return Ok(RebaseReport {
                plan: Some(plan),
                rebase: None,
                fanout: self.view(id)?,
            });
        };
        let report =
            hub.review_run(&self.tools, &candidate.session_id, &Action::Rebase, confirm)?;
        if report.outcome.ok {
            self.inner().trees.remove(&candidate.worktree.path);
            if !self.queue(&hub, id, label, Trigger::Rebase)? {
                // A grading was queued meanwhile (a finished turn): it may
                // read the tree from before the rebase, so grade again after.
                self.inner().again.insert(candidate.session_id.clone());
            }
        }
        Ok(RebaseReport {
            plan: None,
            rebase: Some(report),
            fanout: self.view(id)?,
        })
    }

    /// One candidate's cleanup: remove its worktree (and session), or stop
    /// its session; `dry_run` only says what would happen.
    fn cleanup_one(
        &self,
        hub: &Hub,
        fanout: &Fanout,
        candidate: &Candidate,
        remove: bool,
        force: bool,
        dry_run: bool,
    ) -> CleanupItem {
        let session = hub.session(&candidate.session_id).ok();
        let mut item = CleanupItem {
            candidate: candidate.label.clone(),
            session: session.as_ref().map(|s| s.record.name.clone()),
            path: candidate.worktree.path.clone(),
            branch: candidate.worktree.branch.clone(),
            action: if remove { "remove_worktree" } else { "stop" },
            done: false,
            refusal: None,
            error: None,
            branch_deleted: None,
        };
        if !remove {
            let running = session.as_ref().is_some_and(|s| {
                s.record.state.is_running() && s.live.as_ref().is_some_and(|l| l.pid.is_some())
            });
            if dry_run || !running {
                item.done = !dry_run;
                return item;
            }
            match hub.kill_session(&candidate.session_id) {
                Ok(()) => item.done = true,
                Err(e) => item.error = Some(e.to_string()),
            }
            return item;
        }
        // Unique work in a candidate's worktree is that candidate's attempt:
        // keep it, or let it go knowingly.
        let unique = || {
            if !candidate.worktree.path.is_dir() {
                return None;
            }
            match crate::git::removal_check(&candidate.worktree) {
                Ok(check) => {
                    check.refusal_advising("force the removal if that attempt is not wanted")
                }
                Err(e) => Some(e),
            }
        };
        let refusal = match &session {
            Some(_) => match hub.worktree_info(&candidate.session_id) {
                Ok(info) => info
                    .blocking
                    .first()
                    .map(|other| format!("session {other:?} also uses it"))
                    .or_else(|| info.refusal.and_then(|_| unique())),
                Err(e) => Some(e.to_string()),
            },
            None => unique(),
        };
        item.refusal = refusal.clone();
        if dry_run {
            return item;
        }
        if refusal.is_some() && !force {
            return item;
        }
        // Hanging up its agent must not grade a worktree being removed.
        self.inner().retiring.insert(candidate.session_id.clone());
        let removed = match &session {
            Some(_) => hub
                .remove_session_with(
                    &candidate.session_id,
                    RemoveOptions {
                        remove_worktree: true,
                        force,
                    },
                )
                .map(|outcome| outcome.worktree.map(|w| w.branch_deleted)),
            None if candidate.worktree.path.is_dir() => {
                crate::git::remove(&candidate.worktree, force)
                    .map(|r| Some(r.branch_deleted))
                    .map_err(HubError::Internal)
            }
            None => Ok(None),
        };
        match removed {
            Ok(branch_deleted) => {
                item.done = true;
                item.branch_deleted = branch_deleted;
                let cleaned = Cleaned {
                    at: Utc::now(),
                    branch_deleted: branch_deleted.unwrap_or(false),
                };
                let label = candidate.label.clone();
                let _ = hub.store().update_fanout(&fanout.id, |f| {
                    if let Some(c) = f.candidate_mut(&label) {
                        c.cleaned = Some(cleaned);
                        c.pending = None;
                    }
                });
                self.inner()
                    .queue
                    .retain(|(f, l)| !(f == &fanout.id && l == &candidate.label));
                eprintln!(
                    "fw hub: fan-out {} {}: removed worktree {}{}",
                    fanout.name,
                    candidate.label,
                    candidate.worktree.path.display(),
                    if force && refusal.is_some() {
                        " (forced)"
                    } else {
                        ""
                    }
                );
            }
            Err(e) => {
                self.inner().retiring.remove(&candidate.session_id);
                item.error = Some(e.to_string());
            }
        }
        item
    }

    /// Remove (or stop) the candidates `request` names -- by default every
    /// one but the winner.
    pub fn cleanup(&self, id: &str, request: &CleanupRequest) -> Result<CleanupReport, HubError> {
        let hub = self.hub()?;
        let fanout = hub.store().fanout(id)?.clone();
        let labels: Vec<String> = match &request.candidates {
            Some(labels) => {
                for label in labels {
                    if fanout.candidate(label).is_none() {
                        return Err(StateError::NotFound(format!("candidate {label}")).into());
                    }
                }
                labels.clone()
            }
            None => {
                let Some(winner) = &fanout.winner else {
                    return Err(invalid(
                        "no winner yet: pick one first, or name the candidates to clean up",
                    ));
                };
                fanout
                    .candidates
                    .iter()
                    .filter(|c| c.label != winner.label && c.cleaned.is_none())
                    .map(|c| c.label.clone())
                    .collect()
            }
        };
        let mut items = Vec::new();
        for label in &labels {
            let Some(candidate) = fanout.candidate(label) else {
                continue;
            };
            if candidate.cleaned.is_some() {
                continue;
            }
            items.push(self.cleanup_one(
                &hub,
                &fanout,
                candidate,
                request.remove_worktrees,
                request.force,
                request.dry_run,
            ));
        }
        if !request.dry_run {
            hub.emit(HubEvent::Fanout { id: id.to_owned() });
        }
        Ok(CleanupReport {
            items,
            fanout: self.view(id)?,
        })
    }
}

fn short_sha(sha: &str) -> &str {
    &sha[..12.min(sha.len())]
}

/// The commit a review merge plan fast-forwards to (`git merge --ff-only
/// <sha>`).
fn planned_merge(plan: &Plan) -> Option<&str> {
    plan.commands
        .iter()
        .find(|c| c.args.first().is_some_and(|a| a == "merge"))
        .and_then(|c| c.args.last())
        .map(String::as_str)
        .filter(|sha| !sha.is_empty() && !sha.starts_with('-'))
}

fn session_map(hub: &Hub) -> HashMap<String, SessionView> {
    hub.sessions(None)
        .into_iter()
        .filter(|v| v.record.kind != SessionKind::Shell)
        .map(|v| (v.record.id.clone(), v))
        .collect()
}
