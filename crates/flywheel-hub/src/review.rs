//! Review from anywhere: a session's git work -- its branch against its
//! base (commits, changed files with +/- counts, per-file diffs) and its
//! working tree (staged, unstaged, untracked) -- and guarded actions on
//! it: commit all, push the branch, open a pull request (`gh`), rebase
//! the branch onto its base, and fast-forward the base onto the branch in
//! the main checkout.
//!
//! Every git and gh call goes through [`runner::run`] (argv only, no
//! terminal, capped output, a deadline, bounded concurrency). Blocking:
//! call off the async executor.
//!
//! Actions are two steps. A dry run returns a [`Plan`]: the exact commands
//! (and the directory each runs in), what they will do, why they cannot
//! run now if so, and a fingerprint of the state it was planned against.
//! Running needs that fingerprint back; when the repository moved since
//! (a commit, an edit, another push) the plan is stale and nothing runs,
//! so what runs is what was confirmed.

mod actions;
mod integrate;
mod model;
mod plan;
pub mod runner;
mod session;

pub use actions::{abort, check_message, commit, merge, pr, push, rebase};
pub use integrate::{PrRequest, gh_status, plan_abort, plan_merge, plan_pr, plan_rebase};
pub use model::{
    BaseView, Commit, DiffPage, FileChange, FileSet, Remote, Review, Scope, Untracked,
    UpstreamView, diff, read,
};
pub use plan::{Outcome, Plan, Planned, PrDefaults, Ran, plan_commit, plan_push};
pub use session::{Action, ActionReport, AgentRef, AskKind, AskPlan, SessionReview};

use crate::git::WorktreeRef;
use runner::{Output, RunError, Spec};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Commits listed (newest first); more are counted, not listed.
pub const MAX_COMMITS: usize = 200;
/// Files listed per set (committed, staged, unstaged, untracked).
pub const MAX_FILES: usize = 1000;
/// Diff text read per file (the rest is cut and said so).
pub const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;
/// Diff lines served per page ("load more" asks for the next).
pub const DIFF_PAGE_LINES: usize = 2000;
/// Output kept per command for an action's report.
const RAN_OUTPUT_BYTES: usize = 8 * 1024;
/// stdout read by the read-only git calls.
const READ_LIMIT: usize = 4 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Commit hooks and push to a slow remote can take a while; still bounded.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(300);
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);
const REBASE_TIMEOUT: Duration = Duration::from_secs(120);
const MERGE_TIMEOUT: Duration = Duration::from_secs(120);
const GH_TIMEOUT: Duration = Duration::from_secs(60);
const GH_STATUS_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest commit message / PR title / PR body accepted.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_TITLE_CHARS: usize = 256;

/// The programs review runs (names searched on `PATH`, or paths).
#[derive(Debug, Clone)]
pub struct Tools {
    pub git: String,
    pub gh: String,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            git: "git".into(),
            gh: "gh".into(),
        }
    }
}

/// What to review: the work tree, its base, and (for a worktree session)
/// the worktree record.
#[derive(Debug, Clone)]
pub struct Target {
    /// The work tree's top directory.
    pub dir: PathBuf,
    /// What the branch is compared against: a worktree's base, a base the
    /// caller named, else the branch's upstream (resolved by [`read`]).
    pub base: Option<String>,
    pub base_source: BaseSource,
    pub worktree: Option<WorktreeRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseSource {
    /// The base the session's worktree was cut from.
    Worktree,
    /// Named by the caller (`?base=`).
    Given,
    /// The branch's upstream (no worktree, no base named).
    Upstream,
    /// Nothing to compare against.
    None,
}

/// Why an action did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// The request itself is wrong (400).
    Invalid(String),
    /// The repository's state refuses it, it is stale, or a command failed
    /// (409).
    Conflict(String),
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Conflict(m) => f.write_str(m),
        }
    }
}

impl From<RunError> for ActionError {
    fn from(e: RunError) -> Self {
        Self::Conflict(e.to_string())
    }
}

// --- the review ----------------------------------------------------------

/// A git call in `dir`.
struct Git<'a> {
    program: &'a str,
    dir: &'a Path,
}

impl<'a> Git<'a> {
    fn new(tools: &'a Tools, dir: &'a Path) -> Self {
        Self {
            program: &tools.git,
            dir,
        }
    }

    fn spec(&self, args: &[&str], limit: usize, timeout: Duration) -> Spec<'a> {
        Spec {
            program: self.program,
            dir: self.dir,
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            stdin: None,
            timeout,
            limit,
        }
    }

    /// Run a read-only git command (no optional locks), any exit.
    fn raw(&self, args: &[&str], limit: usize) -> Result<Output, String> {
        let mut full = vec!["--no-optional-locks"];
        full.extend_from_slice(args);
        runner::run(&self.spec(&full, limit, READ_TIMEOUT)).map_err(|e| e.to_string())
    }

    /// stdout of a read-only command that must succeed (trailing newline
    /// trimmed).
    fn out(&self, args: &[&str]) -> Result<String, String> {
        let output = self.raw(args, READ_LIMIT)?;
        if !output.success() {
            return Err(output.error_text());
        }
        Ok(output.stdout_text().trim_end().to_owned())
    }

    /// stdout of a read-only command, `None` when it fails.
    fn maybe(&self, args: &[&str]) -> Option<String> {
        self.out(args).ok().filter(|s| !s.is_empty())
    }
}

/// The top directory of the work tree `dir` is in; `None` outside git.
pub fn toplevel(tools: &Tools, dir: &Path) -> Option<PathBuf> {
    Git::new(tools, dir)
        .maybe(&["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
}

/// Whether `base` names a commit in the repository `dir` is in.
pub fn resolves(tools: &Tools, dir: &Path, base: &str) -> bool {
    Git::new(tools, dir)
        .maybe(&["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")])
        .is_some()
}

// --- per-file diffs ------------------------------------------------------

// --- plans ---------------------------------------------------------------

// --- running a plan ------------------------------------------------------

#[cfg(test)]
mod tests;
