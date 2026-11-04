//! The fan-out engine: launches a fan-out's agents, follows their turns,
//! queues and runs gradings (at most [`MAX_GRADING`] at once, hub-wide),
//! builds the ranked board, and carries out the board's actions (re-grade,
//! ask to fix, rebase, pick the winner, clean up the losers).
//!
//! It holds the hub weakly: it never keeps a hub alive (a hub's serve lock
//! is released when it drops), and its threads end when the hub does or
//! when the hub's [`GradingControl`] is stopped.

use super::facts::TreeState;
use super::{FanoutView, GraderRunner, GradingControl};
use crate::agent::SessionKind;
use crate::hub::{Hub, HubError, SessionView};
use crate::review::{ActionReport, Plan, Tools};
use crate::state::{SessionState, StateError};
use crate::transcript::PasteOutcome;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use super::{
    CandidateView, MAX_FANOUTS, Promotion, default_name, finished_payload, hidden_error,
    pin_graders, pins_broken, sent_prompt, unique_name,
};

mod actions;
mod create;
mod grading;
mod views;

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
