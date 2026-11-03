//! Verified fan-out: one prompt, 2-6 agents (Claude Code and/or Codex,
//! each with its own model), each in its own sibling worktree cut from one
//! base commit, plus grader commands the hub runs itself -- out of band, in
//! every candidate's worktree, once its agent reports a finished turn. The
//! agent's report only *triggers* grading (agent claims are untrusted); the
//! graders' exit codes are the verdict. The graders are held out of the
//! prompt only: an agent runs as the same user as the hub, so it could read
//! them from the hub's state or API, and "ask to fix" shows it the failing
//! command. What the hub enforces is that the checks themselves stay put:
//! protected paths are compared by content against the base, tracked files
//! hidden from git status (skip-worktree, assume-unchanged) fail a
//! grading, and grader scripts named by absolute path are pinned by hash
//! at launch (a changed one fails every grading after it); a script named
//! by a path inside the repo is protected.
//!
//! The board ranks candidates: every grader passing first, then the smaller
//! diff against the base, then the lower cost, and says why. The winner
//! lands through the review merge path (`git merge --ff-only` of its
//! planned commit into the base in the main checkout, a rebase offered when
//! the base moved, which re-grades) -- only while its fan-out branch is
//! checked out at exactly the commit that passed, clean, and that commit
//! changes something. Losing worktrees are then removed on
//! request (refused while they hold unique work unless forced).
//!
//! Graders run through a [`GraderRunner`] the embedding binary injects (the
//! `flywheel` crate wires flywheel-core's command-grader runner: `/bin/sh
//! -c`, its own process group, pass = exit 0, a wall-clock timeout kills
//! the group, the first failure skips the rest), so this crate never
//! depends on the loop's crates. Fan-outs persist in `state.json` (schema
//! 4); a grading cut short by a stop or a live handoff runs again on the
//! next start. Operator doc: `docs/hub/workflows.md` (Verified fan-out).

mod engine;
mod facts;
pub mod promote;

pub use engine::{
    CleanupItem, CleanupReport, CleanupRequest, Fanouts, FixPlan, PickPlan, PickReport,
    RebaseReport,
};
pub use promote::{Promotion, TaskOrigin};

use crate::agent::{AgentLaunch, SessionKind};
use crate::git::WorktreeRef;
use crate::status::{AgentStatus, StatusView};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Candidates per fan-out.
pub const MIN_CANDIDATES: usize = 2;
pub const MAX_CANDIDATES: usize = 6;
/// Grader commands per fan-out, and the longest one.
pub const MAX_GRADERS: usize = 8;
pub const MAX_GRADER_BYTES: usize = 4096;
pub const MAX_GRADER_NAME_CHARS: usize = 64;
/// Per-grader wall-clock limit: the default and the most allowed.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
pub const MAX_TIMEOUT_SECS: u64 = 4 * 3600;
/// Fan-outs the hub keeps (finished ones must be removed past it).
pub const MAX_FANOUTS: usize = 50;
/// Protected paths per fan-out.
pub const MAX_PROTECT: usize = 32;
/// Captured tail of each grader's stdout and of its stderr.
/// Only a failing grader keeps it (a pass's output is dropped).
pub const TAIL_BYTES: usize = 8 * 1024;
/// A grader command's absolute-path argument up to this size is pinned by
/// hash (a script; larger files are taken to be installed programs).
pub const MAX_PIN_BYTES: u64 = 1024 * 1024;
/// Gradings running at once, hub-wide (graders are usually builds).
pub const MAX_GRADING: usize = 2;
/// Appended to the prompt unless turned off: the merge path lands commits,
/// so a candidate's work must be committed to be picked.
pub const COMMIT_INSTRUCTION: &str = "When you are done, commit all of your changes on the \
     current branch (git add -A, then git commit with a short message) and stop.";

/// One grader command (run with `/bin/sh -c` in the candidate's worktree).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraderSpec {
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A held-out grader (a flywheel task's `[held_out]` oracle, see
    /// `promote`): run like any other, but "ask to fix" never shows the
    /// agent its command or output, and promoting the fan-out back to a
    /// task leaves it in the held-out registry instead of the task file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_out: bool,
}

impl GraderSpec {
    /// What the board calls it: its name, else its command.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.run)
    }
}

/// A grader as a request gives it: the command alone, or `{"run","name"?}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GraderInput {
    Run(String),
    Spec(GraderSpec),
}

/// How one grader ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraderStatus {
    /// Exit code 0.
    Passed,
    /// A non-zero exit code.
    Failed,
    /// Killed at the per-grader time limit.
    Timeout,
    /// Ended by a signal it was not sent by the runner.
    Signal,
    /// The command could not be started.
    SpawnFailure,
    /// Not run: an earlier grader failed.
    Skipped,
}

/// One grader's run in a candidate's worktree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraderRun {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub run: String,
    pub status: GraderStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub duration_ms: u64,
    /// The last [`TAIL_BYTES`] of each stream.
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stdout_truncated: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stderr_truncated: bool,
    /// Why it could not start, or how long it was allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Its grader is held out (see [`GraderSpec::held_out`]); set by the
    /// engine from the fan-out's graders, whatever the runner said.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_out: bool,
}

impl GraderRun {
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.run)
    }

    pub fn passed(&self) -> bool {
        self.status == GraderStatus::Passed
    }

    /// `exit 1`, `timed out after 900s`, ...
    pub fn outcome(&self) -> String {
        match self.status {
            GraderStatus::Passed => "passed".to_owned(),
            GraderStatus::Failed => match self.exit_code {
                Some(code) => format!("exit {code}"),
                None => "failed".to_owned(),
            },
            GraderStatus::Timeout => match &self.detail {
                Some(detail) => format!("timed out ({detail})"),
                None => "timed out".to_owned(),
            },
            GraderStatus::Signal => match self.signal {
                Some(signal) => format!("killed by signal {signal}"),
                None => "killed by a signal".to_owned(),
            },
            GraderStatus::SpawnFailure => match &self.detail {
                Some(detail) => format!("could not start: {detail}"),
                None => "could not start".to_owned(),
            },
            GraderStatus::Skipped => "skipped (an earlier grader failed)".to_owned(),
        }
    }

    /// A grader that never ran.
    pub fn skipped(spec: &GraderSpec) -> Self {
        Self {
            name: spec.name.clone(),
            run: spec.run.clone(),
            status: GraderStatus::Skipped,
            exit_code: None,
            signal: None,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            detail: None,
            held_out: spec.held_out,
        }
    }
}

/// What asked for a grading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The agent reported a finished turn.
    Done,
    /// The agent's process ended.
    Exit,
    /// Someone asked (`fw hub fanout grade`, the board's Re-grade).
    Manual,
    /// The candidate was rebased onto its base.
    Rebase,
}

/// A grading asked for and not finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub trigger: Trigger,
    pub queued_at: DateTime<Utc>,
    /// Set while its graders run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
}

/// Lines and files a candidate changed from where it left the base (the
/// fan-out's commit, or the base's tip once rebased): commits, staged and
/// unstaged changes, and untracked files (counted as added lines).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStat {
    pub files: u32,
    pub insertions: u64,
    pub deletions: u64,
    /// Untracked files among `files`.
    #[serde(default)]
    pub untracked: u32,
}

impl DiffStat {
    pub fn lines(&self) -> u64 {
        self.insertions + self.deletions
    }
}

/// One finished grading of a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grade {
    pub trigger: Trigger,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    /// The worktree's HEAD when grading started.
    pub head: Option<String>,
    /// Tracked files with changes, and untracked files, then.
    pub dirty: u32,
    pub untracked: u32,
    /// HEAD plus `git status` at the start: what the verdict describes.
    pub tree: String,
    /// The worktree changed while its graders ran (the verdict may not
    /// describe what is there now).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub moved: bool,
    /// Every grader exited 0 and no protected path changed.
    pub passed: bool,
    pub graders: Vec<GraderRun>,
    /// Protected paths the candidate changed (each one fails it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tampered: Vec<String>,
    pub diff: DiffStat,
    /// The agent's spend when graded (list price; `None`: unpriced model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub tokens: u64,
    /// Grading itself failed (git, the runner): the candidate fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `error` is the operator's to fix, not the agent's (a pinned grader
    /// script changed, no grader runner): nothing is asked of the agent,
    /// whose prompt must never hear of the held-out graders involved.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub operator: bool,
}

impl Grade {
    /// The tree was clean (nothing uncommitted): HEAD is all that passed.
    pub fn clean(&self) -> bool {
        self.dirty == 0 && self.untracked == 0
    }

    /// It changed no file against where it left the base: picking it
    /// would merge nothing.
    pub fn no_change(&self) -> bool {
        self.diff.files == 0
    }

    pub fn graders_passed(&self) -> usize {
        self.graders.iter().filter(|g| g.passed()).count()
    }

    /// The first grader that did not pass.
    pub fn first_failure(&self) -> Option<&GraderRun> {
        self.graders
            .iter()
            .find(|g| !g.passed() && g.status != GraderStatus::Skipped)
    }

    /// Why it failed, in a line (`None` when it passed).
    pub fn failure(&self) -> Option<String> {
        if self.passed {
            return None;
        }
        if let Some(error) = &self.error {
            return Some(format!("grading failed: {error}"));
        }
        if !self.tampered.is_empty() {
            return Some(format!(
                "changed protected path{} {}",
                if self.tampered.len() == 1 { "" } else { "s" },
                self.tampered.join(", ")
            ));
        }
        self.first_failure()
            .map(|g| format!("fails `{}` ({})", g.label(), g.outcome()))
    }
}

/// A candidate's worktree removed after the pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cleaned {
    pub at: DateTime<Utc>,
    pub branch_deleted: bool,
}

/// One agent of a fan-out, in its own worktree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// `c1`, `c2`, ... in launch order.
    pub label: String,
    pub session_id: String,
    pub kind: SessionKind,
    #[serde(default, skip_serializing_if = "AgentLaunch::is_empty")]
    pub launch: AgentLaunch,
    pub worktree: WorktreeRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// The latest finished grading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grade: Option<Grade>,
    /// Gradings finished so far, and fixes asked of the agent.
    #[serde(default)]
    pub gradings: u32,
    #[serde(default)]
    pub fixes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaned: Option<Cleaned>,
}

/// The picked candidate, merged into the base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Winner {
    pub label: String,
    /// The commit merged (what passed).
    pub head: String,
    pub base: String,
    pub at: DateTime<Utc>,
}

/// A grader script outside the worktrees, pinned at launch: a grading only
/// runs while it still hashes the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    pub path: PathBuf,
    pub sha256: String,
    /// A held-out grader names it: its path is never put in an error an
    /// agent may be shown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_out: bool,
}

/// A fan-out as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fanout {
    pub id: String,
    pub name: String,
    pub workspace_id: String,
    /// The workspace folder (main checkout side) the worktrees were cut
    /// from, and the base branch with the commit every candidate started at.
    pub repo: PathBuf,
    pub base: String,
    pub base_sha: String,
    pub prompt: String,
    /// [`COMMIT_INSTRUCTION`] was appended to what the agents got.
    #[serde(default)]
    pub commit_instruction: bool,
    pub graders: Vec<GraderSpec>,
    pub timeout_secs: u64,
    /// Paths (relative to the repo) a candidate must leave unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protect: Vec<String>,
    /// Grader scripts pinned by hash (see [`pin_graders`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<Pin>,
    pub created_at: DateTime<Utc>,
    pub candidates: Vec<Candidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<Winner>,
    /// The flywheel task it was seeded from ("Fan out this task").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TaskOrigin>,
    /// The flywheel task files written from it ("Promote to flywheel
    /// task"), oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promoted: Vec<Promotion>,
}

impl Fanout {
    /// What every agent was sent as its first turn.
    pub fn sent_prompt(&self) -> String {
        sent_prompt(&self.prompt, self.commit_instruction)
    }

    pub fn candidate(&self, label: &str) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.label == label)
    }

    pub fn candidate_mut(&mut self, label: &str) -> Option<&mut Candidate> {
        self.candidates.iter_mut().find(|c| c.label == label)
    }

    /// When the latest grading finished.
    pub fn graded_at(&self) -> Option<DateTime<Utc>> {
        self.candidates
            .iter()
            .filter_map(|c| c.grade.as_ref().map(|g| g.finished_at))
            .max()
    }
}

/// The prompt plus, when asked, the commit instruction.
pub fn sent_prompt(prompt: &str, commit_instruction: bool) -> String {
    if commit_instruction {
        format!("{}\n\n{COMMIT_INSTRUCTION}", prompt.trim_end())
    } else {
        prompt.to_owned()
    }
}

// --- the grader runner seam -------------------------------------------------

/// What to grade: the candidate's worktree and the fan-out's graders.
#[derive(Debug, Clone)]
pub struct GradeRequest<'a> {
    pub dir: &'a Path,
    pub graders: &'a [GraderSpec],
    /// Per grader.
    pub timeout: Duration,
    /// Kept of each stream.
    pub tail_bytes: usize,
}

/// Set to stop a grading (its graders are killed; the result is dropped).
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Why a runner returned no verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerError {
    /// [`Cancel`] was set: the graders were killed.
    Cancelled,
    /// The runner itself failed (it could not start a runtime, ...).
    Failed(String),
}

/// Runs grader commands with flywheel's command-grader semantics: in list
/// order, each `/bin/sh -c <run>` in `dir` in its own process group, pass =
/// exit 0, the group killed at `timeout`, and the first grader that does
/// not pass ends the run (the rest are not returned). Implemented by the
/// embedding binary over flywheel-core (see the module doc).
pub trait GraderRunner: Send + Sync + 'static {
    /// Shown on the board: what ran the graders.
    fn name(&self) -> String;

    /// Run `request`'s graders; stop (killing them) once `cancel` is set.
    fn run(
        &self,
        request: &GradeRequest<'_>,
        cancel: &Cancel,
    ) -> Result<Vec<GraderRun>, RunnerError>;
}

/// Pauses, resumes, and stops the hub's gradings: a live handoff pauses
/// them (cancelling what runs, to run again in whichever hub carries on)
/// and a shutdown stops them. Held by the hub.
#[derive(Debug, Default)]
pub struct GradingControl {
    state: Mutex<ControlState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct ControlState {
    paused: bool,
    stopped: bool,
    active: Vec<Cancel>,
}

impl GradingControl {
    fn lock(&self) -> std::sync::MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Cancel what runs and hold new gradings until [`Self::resume`].
    pub fn pause(&self) {
        let mut state = self.lock();
        state.paused = true;
        for cancel in &state.active {
            cancel.cancel();
        }
    }

    pub fn resume(&self) {
        self.lock().paused = false;
        self.changed.notify_all();
    }

    /// Cancel what runs and start nothing more.
    pub fn stop(&self) {
        let mut state = self.lock();
        state.stopped = true;
        for cancel in &state.active {
            cancel.cancel();
        }
        drop(state);
        self.changed.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        self.lock().stopped
    }

    /// A token for a grading about to start, once not paused (`None`:
    /// stopped, or still paused after `wait`).
    fn begin(&self, wait: Duration) -> Option<Cancel> {
        let mut state = self.lock();
        let deadline = std::time::Instant::now() + wait;
        while state.paused && !state.stopped {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if state.stopped || state.paused {
            return None;
        }
        let cancel = Cancel::new();
        state.active.push(cancel.clone());
        Some(cancel)
    }

    fn end(&self, cancel: &Cancel) {
        self.lock().active.retain(|c| !c.same(cancel));
    }
}

// --- requests ---------------------------------------------------------------

/// One agent to launch.
#[derive(Debug, Clone, Deserialize)]
pub struct CandidateSpec {
    pub kind: SessionKind,
    #[serde(flatten)]
    pub launch: AgentLaunch,
}

/// `POST /api/fanouts`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateFanout {
    pub workspace_id: String,
    /// One of the workspace's git folders (default: the first git one).
    #[serde(default, alias = "repo")]
    pub folder: Option<PathBuf>,
    /// The base branch (default: the folder's current branch).
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub prompt: String,
    pub candidates: Vec<CandidateSpec>,
    pub graders: Vec<GraderInput>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub protect: Vec<String>,
    #[serde(default = "default_true")]
    pub commit_instruction: bool,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// Seed from this flywheel task of `folder` (its directory source):
    /// the server re-reads the task and its held-out graders at launch
    /// (see `promote::seeded`); `prompt` and `graders` may then be empty.
    #[serde(default)]
    pub from_task: Option<String>,
    /// Set by the server from `from_task`, never by a request.
    #[serde(skip)]
    pub origin: Option<TaskOrigin>,
}

fn default_true() -> bool {
    true
}

/// A [`CreateFanout`] checked: what the engine launches.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub name: String,
    pub prompt: String,
    pub candidates: Vec<(SessionKind, AgentLaunch)>,
    pub graders: Vec<GraderSpec>,
    pub timeout_secs: u64,
    pub protect: Vec<String>,
}

impl CreateFanout {
    /// Every field checked and normalized (no git or disk access).
    pub fn validated(&self) -> Result<Validated, String> {
        if self.from_task.is_some() && self.origin.is_none() {
            return Err("a fan-out from a flywheel task is seeded by the server first".into());
        }
        let prompt = crate::agent::validate_prompt(Some(&self.prompt))?
            .filter(|p| !p.trim().is_empty())
            .ok_or("a fan-out needs a prompt: the task every agent gets")?;
        if !(MIN_CANDIDATES..=MAX_CANDIDATES).contains(&self.candidates.len()) {
            return Err(format!(
                "a fan-out runs {MIN_CANDIDATES} to {MAX_CANDIDATES} agents, not {}",
                self.candidates.len()
            ));
        }
        let mut candidates = Vec::with_capacity(self.candidates.len());
        for (i, spec) in self.candidates.iter().enumerate() {
            if !spec.kind.is_agent() {
                return Err(format!(
                    "candidate {}: a fan-out runs agents (claude or codex), not a {}",
                    i + 1,
                    spec.kind.as_str()
                ));
            }
            let launch = spec
                .launch
                .validated(spec.kind)
                .map_err(|e| format!("candidate {}: {e}", i + 1))?;
            candidates.push((spec.kind, launch));
        }
        if self.graders.is_empty() {
            return Err(
                "a fan-out needs at least one grader command: it is what verifies the agents"
                    .into(),
            );
        }
        if self.graders.len() > MAX_GRADERS {
            return Err(format!("at most {MAX_GRADERS} graders"));
        }
        let graders = self
            .graders
            .iter()
            .enumerate()
            .map(|(i, input)| validate_grader(input).map_err(|e| format!("grader {}: {e}", i + 1)))
            .map(|grader| {
                // Only a task's held-out registry makes a grader held out
                // (`promote::seeded`), never a client's say-so.
                grader.map(|mut g| {
                    g.held_out &= self.origin.is_some();
                    g
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let timeout_secs = self.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !(1..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
            return Err(format!(
                "the grader timeout is 1 to {MAX_TIMEOUT_SECS} seconds, not {timeout_secs}"
            ));
        }
        if self.protect.len() > MAX_PROTECT {
            return Err(format!("at most {MAX_PROTECT} protected paths"));
        }
        let mut protect: Vec<String> = Vec::new();
        for raw in &self.protect {
            let path = validate_protect(raw)?;
            if !protect.contains(&path) {
                protect.push(path);
            }
        }
        let name = match self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(name) => crate::state::validate_name(name).map_err(|e| e.to_string())?,
            None => default_name(&prompt),
        };
        Ok(Validated {
            name,
            prompt,
            candidates,
            graders,
            timeout_secs,
            protect,
        })
    }
}

fn validate_grader(input: &GraderInput) -> Result<GraderSpec, String> {
    let spec = match input {
        GraderInput::Run(run) => GraderSpec {
            run: run.clone(),
            name: None,
            held_out: false,
        },
        GraderInput::Spec(spec) => spec.clone(),
    };
    let run = spec.run.trim().to_owned();
    if run.is_empty() {
        return Err("the command is empty".into());
    }
    if run.len() > MAX_GRADER_BYTES {
        return Err(format!(
            "the command is longer than {MAX_GRADER_BYTES} bytes"
        ));
    }
    if run.contains('\0') {
        return Err("the command contains a NUL byte".into());
    }
    let name = match spec
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        None => None,
        Some(name) if name.chars().count() > MAX_GRADER_NAME_CHARS => {
            return Err(format!(
                "its name is longer than {MAX_GRADER_NAME_CHARS} characters"
            ));
        }
        Some(name) if name.chars().any(char::is_control) => {
            return Err("its name contains control characters".into());
        }
        Some(name) => Some(name.to_owned()),
    };
    Ok(GraderSpec {
        run,
        name,
        held_out: spec.held_out,
    })
}

/// A protected path: relative to the repo, no `..`, normalized.
fn validate_protect(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > 256 {
        return Err(format!("protected path {raw:?} is empty or too long"));
    }
    let path = Path::new(trimmed);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "protected path {raw:?} must be relative to the repo, without `..`"
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(format!("protected path {raw:?} names the whole repo"));
    }
    Ok(parts.join("/"))
}

/// The words of a grader command that could name a file: whitespace-split,
/// quotes and a leading `NAME=` assignment stripped.
fn path_words(run: &str) -> impl Iterator<Item = &str> {
    run.split_whitespace()
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"' || c == ';' || c == '('))
        .map(|w| match w.split_once('=') {
            Some((name, value))
                if !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                value
            }
            _ => w,
        })
        .filter(|w| !w.is_empty())
}

fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    Some(
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// What a fan-out's graders lean on that a candidate could change: every
/// absolute path a command names that is a regular file of at most
/// [`MAX_PIN_BYTES`] (a script) is pinned by its hash; every relative path
/// (with a `/`) naming a file in `repo` joins `protect` (each worktree has
/// its own copy, compared against the base like any protected path).
pub fn pin_graders(
    graders: &[GraderSpec],
    repo: &Path,
    mut protect: Vec<String>,
) -> Result<(Vec<Pin>, Vec<String>), String> {
    let mut pins: Vec<Pin> = Vec::new();
    for grader in graders {
        for word in path_words(&grader.run) {
            let path = Path::new(word);
            if path.is_absolute() {
                let Ok(meta) = std::fs::metadata(path) else {
                    continue;
                };
                if !meta.is_file() || meta.len() > MAX_PIN_BYTES {
                    continue;
                }
                let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                if let Some(pin) = pins.iter_mut().find(|p| p.path == canonical) {
                    pin.held_out |= grader.held_out;
                    continue;
                }
                let sha256 = sha256_file(&canonical)
                    .ok_or_else(|| format!("grader script {word}: cannot be read"))?;
                pins.push(Pin {
                    path: canonical,
                    sha256,
                    held_out: grader.held_out,
                });
            } else if word.contains('/')
                && repo.join(path).is_file()
                && let Ok(relative) = validate_protect(word)
                && !protect.contains(&relative)
                && protect.len() < MAX_PROTECT
            {
                protect.push(relative);
            }
        }
    }
    Ok((pins, protect))
}

/// Why the pinned grader scripts no longer are what the fan-out started
/// with (`None`: every one hashes the same).
/// A held-out grader's script is named only as such.
pub fn pins_broken(pins: &[Pin]) -> Option<String> {
    let broken: Vec<&Pin> = pins
        .iter()
        .filter(|p| sha256_file(&p.path).as_deref() != Some(p.sha256.as_str()))
        .collect();
    if broken.is_empty() {
        return None;
    }
    let named: Vec<String> = broken
        .iter()
        .filter(|p| !p.held_out)
        .map(|p| p.path.display().to_string())
        .collect();
    let held = broken.len() - named.len();
    let mut what = Vec::new();
    if !named.is_empty() {
        what.push(format!(
            "grader script{} {}",
            if named.len() == 1 { "" } else { "s" },
            named.join(", ")
        ));
    }
    if held > 0 {
        what.push(format!(
            "{held} held-out grader script{}",
            if held == 1 { "" } else { "s" }
        ));
    }
    Some(format!(
        "{} changed (or went) since the fan-out started: the check is no longer the one every \
         candidate is held to; restore it, or start a new fan-out",
        what.join(" and ")
    ))
}

/// `text` with every held-out grader command of `graders`, and the paths
/// and script names in it, replaced: what an agent may be shown.
fn redact_held_out(text: &str, graders: &[GraderRun]) -> String {
    let mut words: Vec<&str> = graders
        .iter()
        .filter(|g| g.held_out)
        .flat_map(|g| promote::secret_words(&g.run))
        .collect();
    // Longest first: a command before the paths inside it.
    words.sort_by_key(|w| std::cmp::Reverse(w.len()));
    let mut text = text.to_owned();
    for word in words {
        text = text.replace(word, "[held-out]");
    }
    text
}

/// The grading error for tracked files flagged out of git status's sight.
pub fn hidden_error(hidden: &[String]) -> String {
    let shown: Vec<&str> = hidden.iter().take(5).map(String::as_str).collect();
    format!(
        "{} tracked file{} hidden from git status (skip-worktree or assume-unchanged): {}{}; \
         clear the flags (git update-index --no-skip-worktree --no-assume-unchanged <path>) \
         and commit",
        hidden.len(),
        if hidden.len() == 1 { " is" } else { "s are" },
        shown.join(", "),
        if hidden.len() > shown.len() {
            ", ..."
        } else {
            ""
        }
    )
}

/// The protected paths `changed` touches (a protected directory covers
/// everything under it).
pub fn tampered(protect: &[String], changed: &[String]) -> Vec<String> {
    let mut hit: Vec<String> = Vec::new();
    for path in changed {
        let path = path.trim_end_matches('/');
        if protect.iter().any(|p| {
            path == p
                || path
                    .strip_prefix(p.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        }) && !hit.iter().any(|h| h == path)
        {
            hit.push(path.to_owned());
        }
    }
    hit.sort();
    hit
}

/// `name`, or `name-2`, `name-3`, ... when `taken` has it already.
pub(crate) fn unique_name(name: &str, taken: &[&str]) -> String {
    if !taken.contains(&name) {
        return name.to_owned();
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            // Within the name limit, the number kept.
            let keep = crate::state::MAX_NAME_CHARS.saturating_sub(suffix.len());
            let stem: String = name.chars().take(keep).collect();
            format!("{stem}{suffix}")
        })
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .unwrap_or_else(|| name.to_owned())
}

/// A fan-out named after its prompt's first words.
fn default_name(prompt: &str) -> String {
    let mut name = String::new();
    for word in prompt.split_whitespace().take(8) {
        let word: String = word.chars().filter(|c| !c.is_control()).collect();
        if name.chars().count() + word.chars().count() + 1 > 48 {
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(&word);
    }
    if name.is_empty() {
        "fan-out".to_owned()
    } else {
        name
    }
}

/// The worktree task (directory and branch) name of a candidate:
/// `<slug>-<tag>-<label>` within [`crate::git::validate_task`]'s rules.
pub fn task_name(name: &str, tag: &str, label: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 32 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "fanout" } else { slug };
    format!("{slug}-{tag}-{label}")
}

/// The prompt "ask candidate to fix" pastes: what failed and its output
/// (`None`: it passes, or the failure is the operator's to fix).
pub fn fix_prompt(grade: &Grade) -> Option<String> {
    if grade.passed || grade.operator {
        return None;
    }
    if let Some(error) = &grade.error {
        let error = redact_held_out(error, &grade.graders);
        return Some(format!(
            "The hub could not grade your work in this worktree: {error}. Check that the \
             worktree is in a usable state (no rebase or merge left half-way), then commit \
             your changes."
        ));
    }
    if !grade.tampered.is_empty() {
        return Some(format!(
            "Your changes touch files that must stay as they are: {}. Restore them to their \
             original contents (git checkout <base> -- <path>, or revert the change), keep \
             the rest of your work, and commit.",
            grade.tampered.join(", ")
        ));
    }
    let failed = grade.first_failure()?;
    if failed.held_out {
        // A held-out check's command and output are never the agent's to
        // see (flywheel's held-out gate): only that one failed.
        return Some(format!(
            "A held-out check the hub ran on your work in this worktree failed ({outcome}). \
             Held-out checks and their output are not shown to you. Re-read the task, make \
             sure your work fully does what it asks (edge cases included), then commit your \
             changes.",
            outcome = failed.outcome(),
        ));
    }
    let mut output = String::new();
    for (stream, text) in [("stdout", &failed.stdout), ("stderr", &failed.stderr)] {
        let text = text.trim_end();
        if text.is_empty() {
            continue;
        }
        let tail = tail_from_line_start(text, 3000);
        output.push_str(&format!("--- {stream} (last lines) ---\n{tail}\n"));
    }
    if output.is_empty() {
        output.push_str("(it printed nothing)\n");
    }
    let fence = if output.contains("```") {
        "~~~~"
    } else {
        "```"
    };
    Some(format!(
        "A check the hub ran on your work in this worktree failed.\n\n\
         $ {run}\n{outcome}\n\n{fence}\n{output}{fence}\n\n\
         Fix the code so this check passes. Do not change the check itself or what it \
         reads to decide; fix the work it checks. Then commit your changes.",
        run = failed.run,
        outcome = failed.outcome(),
    ))
}

/// The last `max` characters of `text`, from a line start where one is
/// near.
fn tail_from_line_start(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let start = text.char_indices().nth(count - max).map_or(0, |(i, _)| i);
    let cut = &text[start..];
    match cut.find('\n') {
        Some(nl) if nl < 200 => &cut[nl + 1..],
        _ => cut,
    }
}

// --- views and ranking ------------------------------------------------------

/// Where a candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Its agent is working on the prompt.
    Working,
    /// Its agent waits on a person.
    Blocked,
    /// Its agent has not reported a finished turn yet (starting, idle).
    Waiting,
    Queued,
    Grading,
    Graded,
    /// Its agent is no longer running and it was never graded.
    Ended,
    /// Its worktree was removed (after the pick).
    Removed,
}

/// What the board says of a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Graded, every grader passed, nothing changed since.
    Pass,
    /// Graded and failed (a grader, a protected path, or grading itself).
    Fail,
    /// Graded, but its worktree changed since: re-grade.
    Stale,
    /// A grading is queued or running.
    Pending,
    /// Not graded yet.
    None,
}

/// A candidate on the board.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateView {
    pub label: String,
    pub session_id: String,
    /// The session's name (`None`: removed).
    pub session: Option<String>,
    pub kind: SessionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub branch: String,
    pub path: PathBuf,
    pub phase: Phase,
    pub verdict: Verdict,
    /// 1 = best.
    pub rank: usize,
    /// Why it stands where it does.
    pub why: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<StatusView>,
    /// The session's state (`running`, `exited`, `stopped`; `None`:
    /// removed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<Grade>,
    /// Why the grade no longer describes the worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// The worktree's HEAD now (`None`: unknown or gone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Spend so far (live while the session exists, else as last graded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub tokens: u64,
    pub gradings: u32,
    pub fixes: u32,
    pub winner: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleaned: Option<Cleaned>,
    /// It could be picked now (fresh pass, clean, no winner yet).
    pub pickable: bool,
}

/// Counts over a fan-out's candidates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub candidates: usize,
    pub passing: usize,
    pub failing: usize,
    /// Queued or grading.
    pub grading: usize,
    /// Agents still at work (working, blocked, waiting).
    pub running: usize,
}

/// A fan-out with its ranked board.
#[derive(Debug, Clone, Serialize)]
pub struct FanoutView {
    pub id: String,
    pub name: String,
    pub workspace_id: String,
    pub workspace: Option<String>,
    /// The workspace folder the worktrees were cut from (what a launch
    /// names `folder`).
    pub folder: PathBuf,
    /// Deprecated mirror of `folder`.
    pub repo: PathBuf,
    pub base: String,
    pub base_sha: String,
    pub prompt: String,
    pub sent_prompt: String,
    pub graders: Vec<GraderSpec>,
    pub timeout_secs: u64,
    pub protect: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner: Option<Winner>,
    /// Best first.
    pub candidates: Vec<CandidateView>,
    pub counts: Counts,
    /// The best candidate, when it passes.
    pub leader: Option<String>,
    /// Nothing is left to wait for: no agent at work, no grading queued.
    pub settled: bool,
    pub graded_at: Option<DateTime<Utc>>,
    /// What runs the graders (`None`: this hub cannot grade).
    pub runner: Option<String>,
    /// What the board should say of the fan-out as a whole (the graders
    /// pass on the base unchanged, ...).
    pub warnings: Vec<String>,
    /// A list entry: no grader output, worktree reads up to
    /// half a minute old (`GET /api/fanouts/{id}` has the full board).
    pub summary: bool,
    /// The flywheel task it was seeded from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<TaskOrigin>,
    /// The flywheel task files written from it, and where each is now.
    pub promoted: Vec<promote::PromotedView>,
}

impl FanoutView {
    /// Drop what only a board needs (grader output).
    pub fn summarize(&mut self) {
        for candidate in &mut self.candidates {
            if let Some(grade) = &mut candidate.grade {
                for run in &mut grade.graders {
                    run.stdout.clear();
                    run.stderr.clear();
                }
            }
        }
        self.summary = true;
    }
}

/// A candidate's phase from its record and session.
pub fn phase_of(
    candidate: &Candidate,
    session: Option<(&str, bool, Option<AgentStatus>)>,
) -> Phase {
    if candidate.cleaned.is_some() {
        return Phase::Removed;
    }
    if let Some(pending) = &candidate.pending {
        return if pending.started_at.is_some() {
            Phase::Grading
        } else {
            Phase::Queued
        };
    }
    if candidate.grade.is_some() {
        return Phase::Graded;
    }
    match session {
        Some(("running", true, status)) => match status {
            Some(AgentStatus::Working) => Phase::Working,
            Some(AgentStatus::Blocked) => Phase::Blocked,
            _ => Phase::Waiting,
        },
        _ => Phase::Ended,
    }
}

/// The verdict from a candidate's grade, pending grading, and staleness.
pub fn verdict_of(candidate: &Candidate, stale: Option<&str>) -> Verdict {
    match (&candidate.grade, &candidate.pending) {
        (_, Some(_)) => Verdict::Pending,
        (Some(_), None) if stale.is_some() => Verdict::Stale,
        (Some(grade), None) if grade.passed => Verdict::Pass,
        (Some(_), None) => Verdict::Fail,
        (None, None) => Verdict::None,
    }
}

fn money(cost: Option<f64>) -> String {
    match cost {
        Some(cost) => crate::script::dollars(cost),
        None => "unpriced".to_owned(),
    }
}

/// Order the board and say why: passing first (smaller diff, then lower
/// cost), then failing (more graders passed, smaller diff, lower cost),
/// then stale, pending, and ungraded ones in launch order. Fills `rank`
/// and `why`.
pub fn rank(views: &mut [CandidateView]) {
    let diff = |v: &CandidateView| v.grade.as_ref().map_or(u64::MAX, |g| g.diff.lines());
    let noop = |v: &CandidateView| v.grade.as_ref().is_some_and(Grade::no_change);
    let passed = |v: &CandidateView| v.grade.as_ref().map_or(0, Grade::graders_passed);
    let cost = |v: &CandidateView| v.cost_usd.unwrap_or(f64::INFINITY);
    let launch = |v: &CandidateView| {
        v.label
            .trim_start_matches('c')
            .parse::<usize>()
            .unwrap_or(usize::MAX)
    };
    views.sort_by(|a, b| {
        a.verdict
            .cmp(&b.verdict)
            .then_with(|| match a.verdict {
                // A pass that changes nothing has nothing to merge: last.
                Verdict::Pass => noop(a)
                    .cmp(&noop(b))
                    .then_with(|| diff(a).cmp(&diff(b)))
                    .then_with(|| cost(a).total_cmp(&cost(b))),
                Verdict::Fail => passed(b)
                    .cmp(&passed(a))
                    .then_with(|| diff(a).cmp(&diff(b)))
                    .then_with(|| cost(a).total_cmp(&cost(b))),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| launch(a).cmp(&launch(b)))
    });
    let passing = views.iter().filter(|v| v.verdict == Verdict::Pass).count();
    for i in 0..views.len() {
        let mut why = Vec::new();
        let v = &views[i];
        let graders = v.grade.as_ref().map_or(0, |g| g.graders.len());
        match v.verdict {
            Verdict::Pass => {
                why.push(format!(
                    "passes all {graders} grader{}",
                    if graders == 1 { "" } else { "s" }
                ));
                if noop(v) {
                    why.push("changes nothing vs base: nothing to merge".to_owned());
                }
            }
            Verdict::Fail => {
                if let Some(failure) = v.grade.as_ref().and_then(Grade::failure) {
                    why.push(failure);
                }
                if let Some(grade) = &v.grade {
                    why.push(format!("{}/{graders} graders pass", grade.graders_passed()));
                }
            }
            Verdict::Stale => {
                why.push(format!(
                    "{}: re-grade",
                    v.stale.as_deref().unwrap_or("changed since graded")
                ));
            }
            Verdict::Pending => why.push(
                if v.phase == Phase::Grading {
                    "grading now"
                } else {
                    "queued for grading"
                }
                .to_owned(),
            ),
            Verdict::None => why.push(
                match v.phase {
                    Phase::Working => "its agent is still working",
                    Phase::Blocked => "its agent waits on you",
                    Phase::Waiting => "waiting for its agent to report a finished turn",
                    Phase::Removed => "its worktree was removed",
                    _ => "its agent ended without a finished turn: grade it by hand",
                }
                .to_owned(),
            ),
        }
        if let Some(grade) = &v.grade
            && matches!(v.verdict, Verdict::Pass | Verdict::Fail)
        {
            why.push(format!(
                "{} line{} in {} file{} vs base",
                grade.diff.lines(),
                if grade.diff.lines() == 1 { "" } else { "s" },
                grade.diff.files,
                if grade.diff.files == 1 { "" } else { "s" }
            ));
        }
        if matches!(v.verdict, Verdict::Pass | Verdict::Fail) {
            if let Some(above) = i.checked_sub(1).map(|j| &views[j])
                && above.verdict == v.verdict
            {
                why.push(versus(v, above, "below", &diff, &cost, &passed));
            } else if let Some(below) = views.get(i + 1)
                && below.verdict == v.verdict
            {
                why.push(versus(v, below, "ahead of", &diff, &cost, &passed));
            }
        }
        if v.verdict == Verdict::Pass && i == 0 && passing > 1 {
            why.push(format!("best of {passing} passing"));
        }
        views[i].why = why;
        views[i].rank = i + 1;
    }
}

/// Why `v` stands `relation` (`below` / `ahead of`) `other` of the same
/// verdict.
fn versus(
    v: &CandidateView,
    other: &CandidateView,
    relation: &str,
    diff: &dyn Fn(&CandidateView) -> u64,
    cost: &dyn Fn(&CandidateView) -> f64,
    passed: &dyn Fn(&CandidateView) -> usize,
) -> String {
    let who = &other.label;
    let noop = |c: &CandidateView| c.grade.as_ref().is_some_and(Grade::no_change);
    if v.verdict == Verdict::Pass && noop(v) != noop(other) {
        return if noop(v) {
            format!("{relation} {who}: it changes nothing")
        } else {
            format!("{relation} {who}: {who} changes nothing")
        };
    }
    if v.verdict == Verdict::Fail && passed(v) != passed(other) {
        let word = if passed(v) > passed(other) {
            "more"
        } else {
            "fewer"
        };
        return format!(
            "{relation} {who}: {word} graders pass ({} vs {})",
            passed(v),
            passed(other)
        );
    }
    if diff(v) != diff(other) {
        let word = if diff(v) < diff(other) {
            "smaller"
        } else {
            "larger"
        };
        return format!(
            "{relation} {who}: {word} diff ({} vs {} lines)",
            diff(v),
            diff(other)
        );
    }
    if cost(v) != cost(other) {
        let word = if cost(v) < cost(other) {
            "cheaper"
        } else {
            "costs more"
        };
        return format!(
            "{relation} {who}: same diff, {word} ({} vs {})",
            money(v.cost_usd),
            money(other.cost_usd)
        );
    }
    format!("{relation} {who}: same diff and cost (launch order)")
}

// --- the plugin event -------------------------------------------------------

/// Most characters of a failing grader's output a `fanout.finished` event
/// carries (the board and `fw hub fanout show` have the rest).
pub const FINISHED_TAIL_CHARS: usize = 400;

/// The `fanout.finished` plugin event's payload: a settled fan-out's ranked
/// board without grader output beyond a short tail of the first failing
/// grader. Shape (`payload.fanout`):
///
/// ```json
/// {"id", "name", "workspace_id", "workspace", "repo", "base", "base_sha",
///  "graded_at", "leader": "c2" | null, "winner": {"label","head","base","at"} | null,
///  "counts": {"candidates","passing","failing","grading","running"},
///  "warnings": [..],
///  "candidates": [{"label", "rank", "verdict": "pass"|"fail"|"stale"|"pending"|"none",
///    "passed", "kind", "model", "session_id", "session", "branch", "path", "head",
///    "graders_passed", "graders_total", "diff": {"files","insertions","deletions","untracked"},
///    "failure": "fails `unit` (exit 1)" | null, "tail": "..." | null,
///    "cost_usd", "tokens", "why": [..], "pickable"}]}
/// ```
///
/// Best first; `tail` is at most [`FINISHED_TAIL_CHARS`] characters of the
/// first failing grader's stderr (its stdout when stderr is empty).
pub fn finished_payload(view: &FanoutView) -> serde_json::Value {
    let candidates: Vec<serde_json::Value> = view
        .candidates
        .iter()
        .map(|c| {
            let grade = c.grade.as_ref();
            let tail = grade.and_then(Grade::first_failure).and_then(|run| {
                let text = [&run.stderr, &run.stdout]
                    .into_iter()
                    .map(|t| t.trim_end())
                    .find(|t| !t.is_empty())?;
                Some(tail_from_line_start(text, FINISHED_TAIL_CHARS).to_owned())
            });
            serde_json::json!({
                "label": c.label,
                "rank": c.rank,
                "verdict": c.verdict,
                "passed": c.verdict == Verdict::Pass,
                "kind": c.kind,
                "model": c.model,
                "session_id": c.session_id,
                "session": c.session,
                "branch": c.branch,
                "path": c.path,
                "head": grade.and_then(|g| g.head.clone()).or_else(|| c.head.clone()),
                "graders_passed": grade.map_or(0, Grade::graders_passed),
                "graders_total": grade.map_or(0, |g| g.graders.len()),
                "diff": grade.map(|g| &g.diff),
                "failure": grade.and_then(Grade::failure),
                "tail": tail,
                "cost_usd": c.cost_usd,
                "tokens": c.tokens,
                "why": c.why,
                "pickable": c.pickable,
            })
        })
        .collect();
    serde_json::json!({
        "fanout": {
            "id": view.id,
            "name": view.name,
            "workspace_id": view.workspace_id,
            "workspace": view.workspace,
            "folder": view.folder,
            "repo": view.repo,
            "base": view.base,
            "base_sha": view.base_sha,
            "graded_at": view.graded_at,
            "leader": view.leader,
            "winner": view.winner,
            "counts": view.counts,
            "warnings": view.warnings,
            "candidates": candidates,
        }
    })
}

#[cfg(test)]
mod tests;
