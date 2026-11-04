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
mod request;
mod runner;
mod view;

pub use engine::{
    CleanupItem, CleanupReport, CleanupRequest, Fanouts, FixPlan, PickPlan, PickReport,
    RebaseReport,
};
pub use promote::{Promotion, TaskOrigin};
use request::default_true;
pub(crate) use request::unique_name;
pub use request::{
    CandidateSpec, CreateFanout, Validated, fix_prompt, hidden_error, pin_graders, pins_broken,
    tampered, task_name,
};
pub use runner::{Cancel, GradeRequest, GraderRunner, GradingControl, RunnerError};
pub use view::{
    CandidateView, Counts, FINISHED_TAIL_CHARS, FanoutView, Phase, Verdict, finished_payload,
    phase_of, rank, verdict_of,
};

use crate::agent::{AgentLaunch, SessionKind};
use crate::git::WorktreeRef;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    /// The grading someone last looked at on the board (see
    /// [`Fanout::needs_you`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_graded_at: Option<DateTime<Utc>>,
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

    /// The hub grades its candidates' finished turns: no winner picked
    /// yet. Once one is, they are ordinary sessions again.
    pub fn open(&self) -> bool {
        self.winner.is_none()
    }

    /// Sessions whose finished turns the hub grades (not a person's to
    /// read): the candidates of an open fan-out whose worktrees remain.
    pub fn graded_sessions(&self) -> impl Iterator<Item = &str> {
        self.candidates
            .iter()
            .filter(|c| self.open() && c.cleaned.is_none())
            .map(|c| c.session_id.as_str())
    }

    /// The fan-out's own "done": an open fan-out whose latest grading
    /// finished after the one someone last looked at on its board. It
    /// counts as one done in its workspace (every client alike), in place
    /// of its candidates' finished turns.
    pub fn needs_you(&self) -> bool {
        self.open()
            && self
                .graded_at()
                .is_some_and(|at| self.seen_graded_at.is_none_or(|seen| seen < at))
    }

    /// Someone looked at the board's grading of `graded_at`; whether that
    /// changed anything. A mark never runs past the latest grading (one
    /// that finishes later still needs you) and never goes back.
    pub fn mark_seen(&mut self, graded_at: DateTime<Utc>) -> bool {
        let Some(latest) = self.graded_at() else {
            return false;
        };
        let mark = graded_at.min(latest);
        if self.seen_graded_at.is_some_and(|seen| seen >= mark) {
            return false;
        }
        self.seen_graded_at = Some(mark);
        true
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

#[cfg(test)]
mod tests;
