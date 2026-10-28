//! The git-worktree submit strategy: the reference consumer of the
//! orchestrator's landing seam (`docs/strategy.md`).
//!
//! Each task runs in its own worktree on branch `flywheel/<phase>/<task-id>`;
//! on `done` the branch is fast-forward-merged into the base and the worktree
//! removed, otherwise it is parked for forensics. When the base advanced
//! under a finished task, the branch is rebased once and its command graders
//! re-run against the rebased tree before the merge -- nothing lands that was
//! not verified against the exact base it lands on. When the rebase itself
//! conflicts the merge-fallback recovery rung merges the base into the branch
//! and re-verifies the merged tree; when that merge conflicts an optional
//! bounded agent session resolves it (spec 00076). Every leaf of the ladder is
//! a land or a park-with-reason, witnessed on the run's ledger as a
//! [`Landed`] or [`LandingParked`] event.
//!
//! The worker never commits to the operator's branch: the only commits this
//! module authors are the merge-fallback merge commit on the *task branch*
//! (its own landing bookkeeping) and the message-only trailer rewrite
//! (`crate::trailers`). Git is driven through `std::process::Command("git")`
//! with the same argv as the Python worker so ref names and behavior match.
//!
//! Port of the landing half of the Python `flywheel_worktree.worker` module
//! (`GitWorktreeSubmitter` and its plumbing). The per-phase integration
//! branch variant lives in `crate::phase`.

use crate::trailers::{TrailerError, provenance_trailers, stamp_commit_messages};
use chrono::Utc;
use flywheel_core::events::{
    DomainEvent, DomainEventPayload, GATE_EXCERPT_MAX_BYTES, GateGraderReceipt,
    LANDING_STRATEGY_MERGE, Landed, LandingParked, PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_HELD_OUT_GATE, PARK_KIND_MERGE_CONFLICT, PARK_KIND_PROTECTED_PATHS,
    PARK_KIND_STANDING_VERIFY, PARK_KIND_SUBMIT_ERROR, PARK_KIND_UNCOMMITTED_WORK,
    RUNG_AGENT_RESOLVED, RUNG_FAST_FORWARD, RUNG_MERGE_FALLBACK, RUNG_REBASE,
};
use flywheel_core::grader_command::{CommandGraderOptions, bounded_tail, run_command_graders};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{
    DomainEventStore, GraderResultRecord, GraderResultStore, JsonObject, LifecycleStore, StoreError,
};
use flywheel_core::task::Grader;
use flywheel_orchestrator::held_out_gate::{
    GateOptions, HeldOutGraderSource, evaluate_held_out_gate,
};
use flywheel_orchestrator::sources::{LogSink, ROOT_PHASE, phase_of_task_file};
use flywheel_orchestrator::strategy::{
    LandabilityVerdict, PrepareError, SandboxHandle, SandboxRequest, SubmitOutcome, SubmitRequest,
    SubmitStrategy,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Fixed, deterministic commit identity established on every worktree the
/// worker provisions, so the agent's own in-sandbox `git commit` resolves an
/// author/committer even on a host with no global or system git identity.
/// Constant across every worktree in a repo (never random/per-run): the
/// worker authors no task commit itself; this only lets the agent's commit
/// succeed.
pub const WORKTREE_COMMIT_IDENTITY_NAME: &str = "Flywheel Worker";
pub const WORKTREE_COMMIT_IDENTITY_EMAIL: &str = "worker@flywheel.invalid";

/// The submitter's log seam: one line per message, shared with the
/// orchestrator's sources.
pub type Logger = LogSink;

// --- errors ------------------------------------------------------------------------------

/// A git invocation the worker expected to succeed did not, or could not be
/// spawned at all.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git {args} failed in {}: {stderr}", cwd.display())]
    Failed {
        cwd: PathBuf,
        args: String,
        stderr: String,
    },
    #[error("cannot run git {args} in {}: {source}", cwd.display())]
    Spawn {
        cwd: PathBuf,
        args: String,
        #[source]
        source: std::io::Error,
    },
}

/// A task's worktree could not be provisioned. The orchestrator skips that
/// task for the session (never starving peers).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PrepareSandboxError(pub String);

impl From<GitError> for PrepareSandboxError {
    fn from(err: GitError) -> Self {
        PrepareSandboxError(err.to_string())
    }
}

impl From<PrepareSandboxError> for PrepareError {
    fn from(err: PrepareSandboxError) -> Self {
        PrepareError::new(err.0)
    }
}

/// Everything the landing ladder can "raise" (the Python `except Exception`
/// in `submit`): contained by [`GitWorktreeSubmitter::submit`] into a
/// `submit-error` park so a landing failure never unwinds the orchestrator.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Trailers(#[from] TrailerError),
    #[error("cannot acquire the merge lock: {0}")]
    Lock(std::io::Error),
    #[error("cannot start the grader runtime: {0}")]
    Runtime(std::io::Error),
}

impl SubmitError {
    /// The error's kind name, for the park detail (`submit raised <kind>:`).
    pub fn kind(&self) -> &'static str {
        match self {
            SubmitError::Git(_) => "GitError",
            SubmitError::Trailers(_) => "TrailerError",
            SubmitError::Lock(_) => "MergeLockError",
            SubmitError::Runtime(_) => "RuntimeError",
        }
    }
}

// --- git plumbing ------------------------------------------------------------------------

/// One git invocation's result. Most callers branch on [`ok`](Self::ok): a
/// non-zero exit is data, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// `stdout` with surrounding whitespace trimmed.
    pub fn trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

/// Run `git -C <cwd> <args>`. A non-zero exit is returned as data; only a
/// spawn failure (no `git` binary) is an error.
pub fn git(cwd: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| GitError::Spawn {
            cwd: cwd.to_path_buf(),
            args: args.join(" "),
            source,
        })?;
    Ok(GitOutput {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run git and treat a non-zero exit as [`GitError::Failed`], for the few
/// calls whose failure is unexpected.
pub fn git_checked(cwd: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
    let out = git(cwd, args)?;
    if !out.ok() {
        return Err(GitError::Failed {
            cwd: cwd.to_path_buf(),
            args: args.join(" "),
            stderr: out.stderr.trim().to_owned(),
        });
    }
    Ok(out)
}

/// The checkpoint-nudge progress probe type: an opaque token source the
/// harness compares across an iteration.
pub type ProgressProbe = Arc<dyn Fn() -> Result<String, GitError> + Send + Sync>;

/// Build the checkpoint-nudge progress probe bound to `sandbox`.
///
/// Git awareness lives ONLY here (the landability-probe discipline, spec
/// 00061): the orchestrator and core treat the returned closure as an opaque
/// token source. The token is the sandbox branch's resolved HEAD revision,
/// so it changes IFF a new commit lands. The probe FAILS on any git failure
/// (a deleted worktree, a non-repo dir): the harness contains that failure
/// and simply skips the nudge, so a broken probe never fires on a false "no
/// progress" signal.
pub fn git_progress_probe(sandbox: impl Into<PathBuf>) -> ProgressProbe {
    let sandbox = sandbox.into();
    Arc::new(move || {
        Ok(git_checked(&sandbox, &["rev-parse", "HEAD"])?
            .trimmed()
            .to_owned())
    })
}

/// The operator's currently-checked-out branch in `repo_root`, or `None` on
/// a detached HEAD.
pub fn checked_out_branch(repo_root: &Path) -> Result<Option<String>, GitError> {
    let out = git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = out.trimmed();
    if !out.ok() || branch.is_empty() || branch == "HEAD" {
        return Ok(None);
    }
    Ok(Some(branch.to_owned()))
}

// --- merge lock -----------------------------------------------------------------------------

/// Exclusive cross-process lock around base-branch mutations, held until
/// dropped.
///
/// Per-task leases keep two workers off the same task, but two workers
/// finishing *different* tasks would both fast-forward the one shared base.
/// This `flock(2)` serializes those merges (and the archive move): peers
/// still run tasks, only the merge is exclusive. The same mechanism the
/// Python worker used (`fcntl.flock`), so a Rust and a Python worker sharing
/// a repo exclude each other too.
#[derive(Debug)]
pub struct MergeLock {
    file: File,
    path: PathBuf,
}

impl MergeLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        // SAFETY: `flock` on an owned, open descriptor; releasing a lock the
        // process holds has no memory-safety implications and its failure
        // (the fd is closed right after) is not actionable.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Acquire the exclusive merge lock at `lock_path` (created on demand,
/// parent directories included), blocking until it is free.
pub fn merge_lock(lock_path: &Path) -> std::io::Result<MergeLock> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    // SAFETY: `flock` on a descriptor this function owns; the call blocks
    // until the lock is granted and touches no memory.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(MergeLock {
        file,
        path: lock_path.to_path_buf(),
    })
}

// --- bounded excerpts and the re-verify recorder --------------------------------------------

/// Combine a deciding check's captured output into one raw tail bounded to
/// `bound` bytes, mirroring the held-out gate's excerpt builder (spec 00073,
/// criterion 11): non-empty segments join with newlines, then the combined
/// text is re-capped as a *tail* so the final content survives when the
/// check emitted more than the bound. Stored raw -- redaction is a
/// render-time concern (spec 00074, D-3).
pub fn bounded_output_excerpt(segments: &[&str], bound: usize) -> String {
    let combined = segments
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if combined.len() <= bound {
        return combined;
    }
    bounded_tail(combined.as_bytes(), bound).0
}

/// Bounded output tail for a re-verify command grader's receipt, read from
/// the same `stdout_tail` / `stderr_tail` / `spawn_error` payload keys the
/// command runner records (identical to the held-out gate's source).
pub fn payload_excerpt(payload: &JsonObject) -> String {
    let segments: Vec<&str> = ["stdout_tail", "stderr_tail", "spawn_error"]
        .iter()
        .filter_map(|key| payload.get(*key).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect();
    bounded_output_excerpt(&segments, GATE_EXCERPT_MAX_BYTES)
}

/// Minimal in-memory [`GraderResultStore`] for submit-time re-verification.
///
/// Post-rebase grader runs are a merge gate, not lifecycle history: the
/// run's authoritative receipts were persisted in-run, and the store's
/// `(run_id, attempt_number, ordinal)` key has no slot for a re-check.
/// Outcomes are logged instead.
#[derive(Debug, Default)]
pub struct ReverifyRecorder {
    records: Mutex<Vec<GraderResultRecord>>,
}

impl ReverifyRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record appended so far, in append order.
    pub fn records(&self) -> Vec<GraderResultRecord> {
        self.records
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl GraderResultStore for ReverifyRecorder {
    fn append_grader_result(
        &self,
        result: &GraderResultRecord,
    ) -> Result<GraderResultRecord, StoreError> {
        let mut records = self.records.lock().unwrap_or_else(|p| p.into_inner());
        let mut stored = result.clone();
        stored.id = Some(records.len() as i64 + 1);
        records.push(stored.clone());
        Ok(stored)
    }

    fn list_grader_results(
        &self,
        run_id: &str,
        attempt_number: u32,
    ) -> Result<Vec<GraderResultRecord>, StoreError> {
        Ok(self
            .records
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|r| r.run_id == run_id && r.attempt_number == attempt_number)
            .cloned()
            .collect())
    }
}

// --- the conflict-resolution seam ------------------------------------------------------------

/// Inputs a bounded conflict-resolution session runs against (spec 00076,
/// criterion 4).
///
/// `prompt` scopes the agent to resolving the in-progress merge's conflict
/// markers; `worktree` is the conflicted tree it works in (the session's
/// cwd); `max_turns` / `max_wall_seconds` are the session's hard bounds
/// (`[submit] recovery_agent_max_turns` / `recovery_agent_max_wall_seconds`);
/// `model` is the agent model id (`None` => the runtime default).
#[derive(Debug, Clone, PartialEq)]
pub struct ConflictResolutionRequest {
    pub prompt: String,
    pub worktree: PathBuf,
    pub max_turns: u32,
    pub max_wall_seconds: f64,
    pub model: Option<String>,
}

/// Usage a bounded conflict-resolution session recorded (spec 00076, D-4).
///
/// The driver reports usage only -- never a resolution verdict, since the
/// agent's claim is untrusted and the worker decides whether the tree is
/// resolved from git state alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConflictResolutionReport {
    pub turns: u32,
    pub wall_seconds: f64,
}

/// A conflict-resolution session crashed (an SDK or runtime error). The
/// rung contains it into a `merge-conflict` park; it never unwinds `submit`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConflictResolutionError(pub String);

/// The injectable conflict-resolution seam: given a request it drives one
/// bounded agent session inside the conflicted worktree and returns the
/// recorded usage. The agents-runtime driver is wired by the worker daemon;
/// tests inject a synchronous closure so the rung is exercised offline.
pub type ConflictResolver = Arc<
    dyn Fn(&ConflictResolutionRequest) -> Result<ConflictResolutionReport, ConflictResolutionError>
        + Send
        + Sync,
>;

/// The instruction a bounded conflict-resolution session runs under.
///
/// Names the in-progress merge, scopes the agent strictly to resolving the
/// conflict markers (never re-doing the task or editing beyond the
/// conflict), and asks it to stage the resolutions. The worker authors the
/// merge commit and owns every landing gate afterwards, so the prompt does
/// not ask the agent to commit, verify, or judge -- its claim is untrusted.
/// Agent-facing: byte-identical to the Python renderer.
pub fn render_conflict_resolution_prompt(req: &SubmitRequest, branch: &str, base: &str) -> String {
    format!(
        "You are resolving a git merge conflict, nothing more.\n\n\
         The branch `{branch}` implements this already-completed and verified \
         task:\n\n{goal}\n\n\
         A merge of the base branch `{base}` into `{branch}` is in progress in \
         this worktree and has left conflict markers. Your ONLY job is to \
         resolve every conflicted file so the merged result preserves the \
         task's completed work and the base's changes together, then stage the \
         resolved files with `git add`.\n\n\
         Strict rules:\n\
         - Resolve only the merge conflicts. Do NOT re-implement the task, add \
         features, or edit anything unrelated to a conflict.\n\
         - Leave no conflict markers (<<<<<<<, =======, >>>>>>>).\n\
         - Do NOT run `git commit`, `git merge --abort`, or `git reset`; the \
         harness commits and verifies the result.\n\
         - Do NOT touch verification config, graders, or CI.\n",
        goal = req.task.goal
    )
}

// --- the ledger seam ------------------------------------------------------------------------

/// The store surface the submitter needs to record a landing outcome: read
/// the run's current version, then append the audit-witness
/// [`LandingParked`] / [`Landed`] event under optimistic concurrency. Every
/// core store (`InMemoryStore`, `SqliteStore`, ...) satisfies it.
pub trait LandingLedger: LifecycleStore + DomainEventStore + Send + Sync {}

impl<T: LifecycleStore + DomainEventStore + Send + Sync + ?Sized> LandingLedger for T {}

/// A ledger append was declined by the preflight or failed in the store.
#[derive(Debug, thiserror::Error)]
pub enum LedgerWriteError {
    #[error("ledger write declined: {0}")]
    Declined(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The disk/inode preflight seam guarding every authoritative ledger write
/// the submitter makes.
///
/// `guard` runs `append` unless the write should be declined (free space or
/// inodes below threshold), in which case it records its own degraded-space
/// witness and returns [`LedgerWriteError::Declined`] WITHOUT attempting the
/// append -- declining the crashing write before it can tear the store row.
/// `crate::disk_preflight` ships the implementation; a submitter with no
/// guard wired appends directly.
pub trait LedgerWriteGuard: Send + Sync {
    fn guard(
        &self,
        repo_root: &Path,
        run_id: &str,
        append: &mut dyn FnMut() -> Result<(), StoreError>,
    ) -> Result<(), LedgerWriteError>;
}

// --- the landing-base seam ---------------------------------------------------------------------

/// Which ref a task in a phase verifies against and lands onto.
///
/// The merge strategy lands onto the single configured base regardless of
/// phase ([`ConfiguredBase`]); the phase strategy derives a per-phase
/// integration branch (`crate::phase`). `resolve` is read-only (never
/// materializes a ref, so a task that parks never creates one);
/// `materialize` runs at the actual fast-forward, under the merge lock, to
/// create the ref on a phase's first landing.
pub trait LandingBase: Send + Sync {
    fn resolve(&self, submitter: &GitWorktreeSubmitter, phase: &str) -> Result<String, GitError>;

    fn materialize(
        &self,
        submitter: &GitWorktreeSubmitter,
        phase: &str,
        base: &str,
    ) -> Result<String, GitError>;
}

/// The merge/pr landing base: always the configured `phase_base`, which
/// always exists, so materialization is the identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfiguredBase;

impl LandingBase for ConfiguredBase {
    fn resolve(&self, submitter: &GitWorktreeSubmitter, _phase: &str) -> Result<String, GitError> {
        Ok(submitter.phase_base.clone())
    }

    fn materialize(
        &self,
        _submitter: &GitWorktreeSubmitter,
        _phase: &str,
        base: &str,
    ) -> Result<String, GitError> {
        Ok(base.to_owned())
    }
}

// --- check verdicts ---------------------------------------------------------------------------

/// One landing check's verdict: pass/fail plus one [`GateGraderReceipt`]
/// per executed check so a park it decides carries the deciding output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckVerdict {
    pub passed: bool,
    pub receipts: Vec<GateGraderReceipt>,
}

impl CheckVerdict {
    fn pass_without_receipts() -> Self {
        Self {
            passed: true,
            receipts: Vec::new(),
        }
    }
}

/// A bounded conflict-resolution session's recorded usage, carried onto
/// whichever outcome (land or park) follows it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgentUsage {
    pub turns: u32,
    pub wall_seconds: f64,
}

/// The glob matcher behind `[submit] protected_paths`: Python
/// `PurePosixPath.full_match` semantics. `**` matches zero or more whole
/// path segments, `*` and `?` never cross `/`, `[...]` is a character class
/// (`[!...]` negated). The whole path must match.
pub fn protected_path_matches(path: &str, pattern: &str) -> bool {
    let path: Vec<&str> = path.split('/').collect();
    let pattern: Vec<&str> = pattern.split('/').collect();
    match_segments(&path, &pattern)
}

fn match_segments(path: &[&str], pattern: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(&"**") => (0..=path.len()).any(|skip| match_segments(&path[skip..], &pattern[1..])),
        Some(segment) => {
            !path.is_empty()
                && fnmatch(
                    &path[0].chars().collect::<Vec<_>>(),
                    &segment.chars().collect::<Vec<_>>(),
                )
                && match_segments(&path[1..], &pattern[1..])
        }
    }
}

fn fnmatch(s: &[char], p: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|skip| fnmatch(&s[skip..], &p[1..])),
        Some('?') => !s.is_empty() && fnmatch(&s[1..], &p[1..]),
        Some('[') => match parse_class(p) {
            Some((negated, members, rest)) => {
                let Some(&c) = s.first() else {
                    return false;
                };
                let hit = members.iter().any(|m| match m {
                    ClassMember::Single(x) => *x == c,
                    ClassMember::Range(lo, hi) => *lo <= c && c <= *hi,
                });
                hit != negated && fnmatch(&s[1..], rest)
            }
            // An unterminated `[` is a literal bracket, as in fnmatch(3).
            None => s.first() == Some(&'[') && fnmatch(&s[1..], &p[1..]),
        },
        Some(c) => s.first() == Some(c) && fnmatch(&s[1..], &p[1..]),
    }
}

enum ClassMember {
    Single(char),
    Range(char, char),
}

/// Parse a `[...]` class at the head of `p`; returns `(negated, members,
/// remainder)` or `None` when the class is never closed.
fn parse_class(p: &[char]) -> Option<(bool, Vec<ClassMember>, &[char])> {
    let mut i = 1;
    let negated = matches!(p.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut members = Vec::new();
    let mut first = true;
    while let Some(&c) = p.get(i) {
        if c == ']' && !first {
            return Some((negated, members, &p[i + 1..]));
        }
        first = false;
        if let (Some('-'), Some(&hi)) = (p.get(i + 1), p.get(i + 2))
            && hi != ']'
        {
            members.push(ClassMember::Range(c, hi));
            i += 3;
            continue;
        }
        members.push(ClassMember::Single(c));
        i += 1;
    }
    None
}

/// Drive an async grader run to completion from the synchronous landing
/// ladder, on a dedicated thread with its own runtime so it behaves the
/// same whether or not the caller already sits inside a tokio runtime
/// (the orchestrator's drive does; a bare test does not).
fn block_on_dedicated_thread<T, F, Fut>(make: F) -> Result<T, std::io::Error>
where
    T: Send,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = T>,
{
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| -> std::io::Result<T> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            Ok(runtime.block_on(make()))
        });
        handle
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}

/// Run `command` through `/bin/sh -c` in `cwd` with an optional replacement
/// environment, capturing both streams. A spawn failure is folded into a
/// non-zero exit with the OS error on stderr so every caller sees one shape.
fn run_shell(command: &str, cwd: &Path, env: Option<&BTreeMap<String, String>>) -> GitOutput {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null());
    if let Some(env) = env {
        cmd.env_clear().envs(env);
    }
    match cmd.output() {
        Ok(output) => GitOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(err) => GitOutput {
            code: None,
            stdout: String::new(),
            stderr: format!("cannot spawn {command:?}: {err}"),
        },
    }
}

/// The last `limit` characters of `text` after trimming.
fn char_tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    trimmed.chars().skip(count.saturating_sub(limit)).collect()
}

/// Which merged-tree recovery rung is being verified; selects the rung name
/// on the `Landed` record and the wording of its park details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergedTreeRung {
    MergeFallback,
    AgentResolved,
}

impl MergedTreeRung {
    fn rung(self) -> &'static str {
        match self {
            MergedTreeRung::MergeFallback => RUNG_MERGE_FALLBACK,
            MergedTreeRung::AgentResolved => RUNG_AGENT_RESOLVED,
        }
    }

    fn label(self) -> &'static str {
        match self {
            MergedTreeRung::MergeFallback => "merge-fallback",
            MergedTreeRung::AgentResolved => "agent-resolved",
        }
    }

    fn detail_prefix(self) -> &'static str {
        match self {
            MergedTreeRung::MergeFallback => "",
            MergedTreeRung::AgentResolved => "was resolved by a bounded agent session but ",
        }
    }

    fn tree_word(self) -> &'static str {
        match self {
            MergedTreeRung::MergeFallback => "merged",
            MergedTreeRung::AgentResolved => "resolved",
        }
    }
}

/// Everything one landing attempt needs to know about the task it lands,
/// resolved once per submit so no leaf re-derives it.
struct Landing<'a> {
    req: &'a SubmitRequest,
    phase: String,
    worktree: PathBuf,
    branch: String,
    base: String,
    commit_count: u64,
}

// --- the submitter ---------------------------------------------------------------------------

/// Provisions per-task worktrees and merges/parks them on completion.
///
/// The reference [`SubmitStrategy`]: [`prepare_sandbox`](SubmitStrategy::prepare_sandbox)
/// may fail (skips that task); [`submit`](SubmitStrategy::submit) never
/// fails (records its own park/merge outcome).
#[derive(Clone)]
pub struct GitWorktreeSubmitter {
    pub repo_root: PathBuf,
    pub tasks_dir: PathBuf,
    pub worktrees_dir: PathBuf,
    pub phase_base: String,
    pub lock_path: PathBuf,
    pub log: Logger,
    pub protected_paths: Vec<String>,
    pub setup_command: Option<String>,
    /// Submit-time retention (`[sandbox.retention] on_done`): `destroy`
    /// (default) removes a landed worktree+branch; `preserve` keeps them.
    pub on_done: String,
    /// `[sandbox.retention] on_failure`: `park` (default) keeps a non-DONE
    /// worktree for forensics; `destroy` removes it.
    pub on_failure: String,
    /// The run ledger the submitter witnesses every park and land on;
    /// `None` degrades to the log-only park the worktree still preserves.
    pub store: Option<Arc<dyn LandingLedger>>,
    /// The full environment submit-time re-verification runs command
    /// graders with (resolved `[sandbox.env]`); `None` inherits the worker's.
    pub grader_env: Option<BTreeMap<String, String>>,
    /// The standing build invariant (`[submit] verify`, spec 00064), re-run
    /// under the merge lock against the exact tree about to become the base.
    pub verify_command: Option<String>,
    /// The declared held-out landing gate (spec 00051), re-run against the
    /// MERGED candidate tree on the merge-fallback recovery rungs (spec
    /// 00076, D-2); `None` => no gate.
    pub held_out_source: Option<Arc<dyn HeldOutGraderSource>>,
    /// The disk/inode preflight guarding ledger writes; `None` appends
    /// directly.
    pub ledger_guard: Option<Arc<dyn LedgerWriteGuard>>,
    /// Bounded agentic conflict-resolution rung bounds (spec 00076,
    /// criterion 4). `max_turns == 0` disables the rung: a merge conflict
    /// parks exactly as merge-fallback does.
    pub recovery_agent_max_turns: u32,
    pub recovery_agent_max_wall_seconds: f64,
    pub recovery_agent_model: Option<String>,
    /// The session driver the rung escalates to; with none wired the rung
    /// stays disabled even when `recovery_agent_max_turns > 0`.
    pub resolve_conflict: Option<ConflictResolver>,
    /// The landing-base seam (configured base vs. phase integration branch).
    pub landing_base: Arc<dyn LandingBase>,
}

impl fmt::Debug for GitWorktreeSubmitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitWorktreeSubmitter")
            .field("repo_root", &self.repo_root)
            .field("tasks_dir", &self.tasks_dir)
            .field("worktrees_dir", &self.worktrees_dir)
            .field("phase_base", &self.phase_base)
            .field("lock_path", &self.lock_path)
            .field("protected_paths", &self.protected_paths)
            .field("setup_command", &self.setup_command)
            .field("on_done", &self.on_done)
            .field("on_failure", &self.on_failure)
            .field("store", &self.store.is_some())
            .field("verify_command", &self.verify_command)
            .field("held_out_source", &self.held_out_source.is_some())
            .field("recovery_agent_max_turns", &self.recovery_agent_max_turns)
            .field(
                "recovery_agent_max_wall_seconds",
                &self.recovery_agent_max_wall_seconds,
            )
            .field("resolve_conflict", &self.resolve_conflict.is_some())
            .finish_non_exhaustive()
    }
}

impl GitWorktreeSubmitter {
    /// A submitter with the required layout; every optional seam unset and
    /// the retention dispositions at their defaults (`destroy` / `park`).
    pub fn new(
        repo_root: impl Into<PathBuf>,
        tasks_dir: impl Into<PathBuf>,
        worktrees_dir: impl Into<PathBuf>,
        phase_base: impl Into<String>,
        lock_path: impl Into<PathBuf>,
        log: Logger,
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            tasks_dir: tasks_dir.into(),
            worktrees_dir: worktrees_dir.into(),
            phase_base: phase_base.into(),
            lock_path: lock_path.into(),
            log,
            protected_paths: Vec::new(),
            setup_command: None,
            on_done: "destroy".to_owned(),
            on_failure: "park".to_owned(),
            store: None,
            grader_env: None,
            verify_command: None,
            held_out_source: None,
            ledger_guard: None,
            recovery_agent_max_turns: 0,
            recovery_agent_max_wall_seconds: 900.0,
            recovery_agent_model: None,
            resolve_conflict: None,
            landing_base: Arc::new(ConfiguredBase),
        }
    }

    pub fn with_protected_paths<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.protected_paths = paths.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_setup_command(mut self, command: impl Into<String>) -> Self {
        self.setup_command = Some(command.into());
        self
    }

    pub fn with_retention(
        mut self,
        on_done: impl Into<String>,
        on_failure: impl Into<String>,
    ) -> Self {
        self.on_done = on_done.into();
        self.on_failure = on_failure.into();
        self
    }

    pub fn with_store(mut self, store: Arc<dyn LandingLedger>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_grader_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.grader_env = Some(env);
        self
    }

    pub fn with_verify_command(mut self, command: impl Into<String>) -> Self {
        self.verify_command = Some(command.into());
        self
    }

    pub fn with_held_out_source(mut self, source: Arc<dyn HeldOutGraderSource>) -> Self {
        self.held_out_source = Some(source);
        self
    }

    pub fn with_ledger_guard(mut self, guard: Arc<dyn LedgerWriteGuard>) -> Self {
        self.ledger_guard = Some(guard);
        self
    }

    pub fn with_recovery_agent(
        mut self,
        max_turns: u32,
        max_wall_seconds: f64,
        model: Option<String>,
    ) -> Self {
        self.recovery_agent_max_turns = max_turns;
        self.recovery_agent_max_wall_seconds = max_wall_seconds;
        self.recovery_agent_model = model;
        self
    }

    pub fn with_resolve_conflict(mut self, resolver: ConflictResolver) -> Self {
        self.resolve_conflict = Some(resolver);
        self
    }

    pub fn with_landing_base(mut self, landing_base: Arc<dyn LandingBase>) -> Self {
        self.landing_base = landing_base;
        self
    }

    fn log(&self, message: impl AsRef<str>) {
        (self.log)(message.as_ref());
    }

    // --- naming -------------------------------------------------------------------------

    /// The task branch: `flywheel/<phase>/<task-id>`.
    pub fn branch(&self, task_id: &str, phase: &str) -> String {
        format!("flywheel/{phase}/{task_id}")
    }

    /// The task worktree: `<worktrees_dir>/<task-id>`.
    pub fn worktree(&self, task_id: &str) -> PathBuf {
        self.worktrees_dir.join(task_id)
    }

    /// The phase a task file lives under (`_root` for a file-less source).
    pub fn phase_of(&self, task_file: Option<&Path>) -> String {
        match task_file {
            Some(path) => phase_of_task_file(path, &self.tasks_dir),
            None => ROOT_PHASE.to_owned(),
        }
    }

    /// The ref a task in `phase` verifies against and lands onto (read-only).
    pub fn landing_base_for(&self, phase: &str) -> Result<String, GitError> {
        Arc::clone(&self.landing_base).resolve(self, phase)
    }

    /// Ensure the ref a passing task fast-forwards onto exists and return
    /// it, under the caller's merge lock.
    pub fn materialize_landing_base(&self, phase: &str, base: &str) -> Result<String, GitError> {
        Arc::clone(&self.landing_base).materialize(self, phase, base)
    }

    // --- git state probes -----------------------------------------------------------

    pub fn branch_exists(&self, branch: &str) -> Result<bool, GitError> {
        Ok(git(
            &self.repo_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )?
        .ok())
    }

    pub fn is_registered_worktree(&self, worktree: &Path) -> Result<bool, GitError> {
        let listing = git(&self.repo_root, &["worktree", "list", "--porcelain"])?;
        let registered: Vec<&str> = listing
            .stdout
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .collect();
        let literal = worktree.to_string_lossy();
        let resolved = fs::canonicalize(worktree)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(registered
            .iter()
            .any(|r| *r == literal.as_ref() || (!resolved.is_empty() && *r == resolved)))
    }

    /// Clear stale git state a SIGKILL'd run leaves in a reused worktree
    /// (`index.lock`/`HEAD.lock`, a half-finished rebase/merge/cherry-pick)
    /// so the retry's first git call doesn't fail confusingly.
    pub fn scrub_worktree_locks(&self, worktree: &Path, branch: &str) -> Result<(), GitError> {
        let res = git(worktree, &["rev-parse", "--git-dir"])?;
        if !res.ok() {
            return Ok(());
        }
        let worktree_git_dir = absolutize(worktree, res.trimmed());
        let main = git(&self.repo_root, &["rev-parse", "--git-dir"])?;
        let main_git_dir = main
            .ok()
            .then(|| absolutize(&self.repo_root, main.trimmed()));

        for name in ["index.lock", "HEAD.lock"] {
            remove_if_present(&worktree_git_dir.join(name));
        }
        if let Some(main_git_dir) = main_git_dir {
            remove_if_present(
                &main_git_dir
                    .join("refs")
                    .join("heads")
                    .join(format!("{branch}.lock")),
            );
        }
        if worktree_git_dir.join("rebase-merge").is_dir()
            || worktree_git_dir.join("rebase-apply").is_dir()
        {
            self.log(format!(
                "Scrub: aborting in-progress rebase in {}",
                worktree.display()
            ));
            git(worktree, &["rebase", "--abort"])?;
        }
        if worktree_git_dir.join("MERGE_HEAD").is_file() {
            self.log(format!(
                "Scrub: aborting in-progress merge in {}",
                worktree.display()
            ));
            git(worktree, &["merge", "--abort"])?;
        }
        if worktree_git_dir.join("CHERRY_PICK_HEAD").is_file() {
            self.log(format!(
                "Scrub: aborting in-progress cherry-pick in {}",
                worktree.display()
            ));
            git(worktree, &["cherry-pick", "--abort"])?;
        }
        Ok(())
    }

    /// Rebase a parked branch onto the current base. `true` if up to date
    /// or rebased cleanly, `false` on conflict (the rebase is aborted).
    pub fn rebase_parked_branch(
        &self,
        worktree: &Path,
        branch: &str,
        base: &str,
    ) -> Result<bool, GitError> {
        let behind = self.count_commits(&format!("{branch}..{base}"))?;
        if behind == 0 {
            self.log(format!(
                "Reusing parked worktree on {branch}; prior commits carry forward."
            ));
            return Ok(true);
        }
        self.log(format!(
            "Parked {branch} is {behind} commit(s) behind {base}; rebasing..."
        ));
        if git(worktree, &["rebase", base])?.ok() {
            self.log("Rebase clean; prior commits carry forward.");
            return Ok(true);
        }
        git(worktree, &["rebase", "--abort"])?;
        self.log(format!(
            "Rebase failed against {base}; discarding parked worktree+branch and starting fresh."
        ));
        Ok(false)
    }

    fn count_commits(&self, range: &str) -> Result<u64, GitError> {
        let res = git(&self.repo_root, &["rev-list", "--count", range])?;
        Ok(if res.ok() {
            res.trimmed().parse().unwrap_or(0)
        } else {
            0
        })
    }

    /// Commits on `branch` beyond `base`.
    pub fn commit_count(&self, branch: &str, base: &str) -> Result<u64, GitError> {
        self.count_commits(&format!("{base}..{branch}"))
    }

    /// True when `ancestor` is reachable from `rev` -- a fast-forward of the
    /// base to `rev` is possible because `rev` already contains `ancestor`.
    /// Under the merge lock the base cannot move, so this predicts the FF
    /// outcome exactly.
    pub fn is_ancestor(&self, ancestor: &str, rev: &str) -> Result<bool, GitError> {
        Ok(git(
            &self.repo_root,
            &["merge-base", "--is-ancestor", ancestor, rev],
        )?
        .ok())
    }

    // --- provisioning -----------------------------------------------------------------

    fn add_worktree(&self, worktree: &Path, args: &[&str]) -> Result<(), PrepareSandboxError> {
        let mut argv = vec!["worktree", "add"];
        let worktree_str = worktree.to_string_lossy().into_owned();
        argv.push(worktree_str.as_str());
        argv.extend_from_slice(args);
        let proc = git(&self.repo_root, &argv)?;
        if !proc.ok() {
            return Err(PrepareSandboxError(format!(
                "git worktree add failed for {}: {}",
                worktree.display(),
                proc.stderr.trim()
            )));
        }
        self.set_commit_identity(worktree)?;
        Ok(())
    }

    /// Establish the fixed commit identity on a provisioned worktree so an
    /// in-sandbox `git commit` succeeds with no global or system git
    /// identity present. Written to the worktree's repo-local config.
    pub fn set_commit_identity(&self, worktree: &Path) -> Result<(), GitError> {
        git(
            worktree,
            &["config", "user.name", WORKTREE_COMMIT_IDENTITY_NAME],
        )?;
        git(
            worktree,
            &["config", "user.email", WORKTREE_COMMIT_IDENTITY_EMAIL],
        )?;
        Ok(())
    }

    fn discard_and_recreate(
        &self,
        worktree: &Path,
        branch: &str,
        base: &str,
    ) -> Result<(), PrepareSandboxError> {
        let worktree_str = worktree.to_string_lossy().into_owned();
        git(
            &self.repo_root,
            &["worktree", "remove", "--force", worktree_str.as_str()],
        )?;
        git(&self.repo_root, &["branch", "-D", branch])?;
        self.add_worktree(worktree, &["-b", branch, base])
    }

    /// Refuse to discard a parked worktree+branch that belongs to a DONE
    /// run (spec 00076, D-3): automation never deletes verified work. Such
    /// a branch carries verified commits pending a landing re-drive, so the
    /// task is skipped for the session with its branch and worktree intact.
    fn guard_done_branch(
        &self,
        run_id: Option<&str>,
        branch: &str,
        worktree: &Path,
        base: &str,
    ) -> Result<(), PrepareSandboxError> {
        if !self.run_is_done(run_id) {
            return Ok(());
        }
        self.log(format!(
            "parked branch {branch} belongs to a DONE run and cannot rebase onto {base}; \
             preserving branch+worktree for landing recovery instead of discarding verified work"
        ));
        Err(PrepareSandboxError(format!(
            "{branch} is a DONE run's branch that cannot rebase onto {base}; refusing to \
             discard verified work, worktree preserved at {}",
            worktree.display()
        )))
    }

    /// Whether the run identified by `run_id` finalized `Done`. Best-effort:
    /// no store, no run id, or a store error reads as not-done so a status
    /// read never wedges provisioning.
    pub fn run_is_done(&self, run_id: Option<&str>) -> bool {
        let (Some(store), Some(run_id)) = (&self.store, run_id) else {
            return false;
        };
        matches!(
            store.load_lifecycle(run_id),
            Ok(Some(lifecycle)) if lifecycle.status == Status::Done
        )
    }

    /// Provision (or reuse) the worktree a task runs in; return its path.
    ///
    /// Reuses a parked worktree+branch on retry (rebasing onto base first),
    /// recreates it when only the branch survived a sweep, and refuses to
    /// clobber half-present operator state. Every freshly created directory
    /// gets the policy's sandbox setup command; a reused parked worktree
    /// skips it (its environment survived with it).
    pub fn prepare_worktree(&self, req: &SandboxRequest) -> Result<PathBuf, PrepareSandboxError> {
        let task_id = &req.task_id;
        let phase = self.phase_of(req.task_file.as_deref());
        let worktree = self.worktree(task_id);
        let branch = self.branch(task_id, &phase);
        let base = self.landing_base_for(&phase)?;

        let worktree_present = worktree.is_dir();
        let branch_present = self.branch_exists(&branch)?;

        if worktree_present && branch_present {
            if !self.is_registered_worktree(&worktree)? {
                return Err(PrepareSandboxError(format!(
                    "{} exists but is not a registered worktree; refusing to clobber",
                    worktree.display()
                )));
            }
            self.scrub_worktree_locks(&worktree, &branch)?;
            if self.rebase_parked_branch(&worktree, &branch, &base)? {
                return Ok(worktree);
            }
            self.guard_done_branch(req.run_id.as_deref(), &branch, &worktree, &base)?;
            self.discard_and_recreate(&worktree, &branch, &base)?;
            self.run_setup(&worktree)?;
            return Ok(worktree);
        }

        if !worktree_present && branch_present {
            self.log(format!(
                "Recreating worktree on existing branch {branch} (directory was removed; ref survived)."
            ));
            self.add_worktree(&worktree, &[&branch])?;
            if !self.rebase_parked_branch(&worktree, &branch, &base)? {
                self.guard_done_branch(req.run_id.as_deref(), &branch, &worktree, &base)?;
                self.discard_and_recreate(&worktree, &branch, &base)?;
            }
            self.run_setup(&worktree)?;
            return Ok(worktree);
        }

        if worktree_present && !branch_present {
            return Err(PrepareSandboxError(format!(
                "{} exists but no branch {branch}; refusing to clobber. Remove the directory manually.",
                worktree.display()
            )));
        }

        self.add_worktree(&worktree, &["-b", &branch, &base])?;
        self.run_setup(&worktree)?;
        Ok(worktree)
    }

    /// Run the policy's `[sandbox] setup` command (shell) inside a newly
    /// provisioned worktree before the agent enters. Failure skips the task
    /// this session rather than running it in a half-provisioned sandbox.
    fn run_setup(&self, worktree: &Path) -> Result<(), PrepareSandboxError> {
        let Some(command) = &self.setup_command else {
            return Ok(());
        };
        self.log(format!(
            "sandbox setup: {command:?} in {}",
            worktree.display()
        ));
        let proc = run_shell(command, worktree, None);
        if !proc.ok() {
            let tail = char_tail(
                if proc.stderr.is_empty() {
                    &proc.stdout
                } else {
                    &proc.stderr
                },
                2000,
            );
            return Err(PrepareSandboxError(format!(
                "sandbox setup {command:?} failed in {} (exit {}): {tail}",
                worktree.display(),
                proc.code
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string())
            )));
        }
        Ok(())
    }

    // --- landability -----------------------------------------------------------------------

    /// Whether the run produced a committed, non-empty change (spec 00061,
    /// D-3): the worktree is clean and there is at least one commit with a
    /// non-empty diff against the branch's base. Strictly read-only.
    pub fn landability(&self, req: &SubmitRequest) -> Result<LandabilityVerdict, GitError> {
        let phase = self.phase_of(req.task_file.as_deref());
        let worktree = self.worktree(&req.task_id);
        let branch = self.branch(&req.task_id, &phase);
        let base = self.landing_base_for(&phase)?;

        let porcelain = git(&worktree, &["status", "--porcelain"])?;
        if !porcelain.trimmed().is_empty() {
            return Ok(LandabilityVerdict::not_landable(format!(
                "uncommitted changes in the worktree on {branch}: the agent finished without \
                 committing its edits"
            )));
        }
        if self.commit_count(&branch, &base)? == 0 {
            return Ok(LandabilityVerdict::not_landable(format!(
                "no commits beyond {base} on {branch}: the run produced an empty diff against \
                 its base"
            )));
        }
        Ok(LandabilityVerdict::landable())
    }

    // --- the ladder ----------------------------------------------------------------------------

    /// The landing ladder proper. Every leaf returns an outcome; an `Err` is
    /// the Python "raise" that [`SubmitStrategy::submit`] contains.
    pub fn submit_inner(&self, req: &SubmitRequest) -> Result<SubmitOutcome, SubmitError> {
        let phase = self.phase_of(req.task_file.as_deref());
        let worktree = self.worktree(&req.task_id);
        let branch = self.branch(&req.task_id, &phase);

        if req.status != Status::Done {
            // failed / interrupted / any non-done terminal: park for forensics
            // (default), or destroy when [sandbox.retention] on_failure asks.
            return Ok(self.teardown_on_failure(&worktree, &branch, req.status)?);
        }

        // All base-branch mutations are serialized across worker processes.
        let _lock = merge_lock(&self.lock_path).map_err(SubmitError::Lock)?;
        // The ref this task lands onto, resolved per request under the lock.
        let base = self.landing_base_for(&phase)?;
        let porcelain = git(&worktree, &["status", "--porcelain"])?;
        if !porcelain.trimmed().is_empty() {
            self.log(format!(
                "DONE with uncommitted changes on {branch}; parking worktree at {}",
                worktree.display()
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_UNCOMMITTED_WORK,
                format!(
                    "DONE with an uncommitted tree on {branch}; worktree preserved at {}",
                    worktree.display()
                ),
                Vec::new(),
                None,
            ));
        }

        let commit_count = self.commit_count(&branch, &base)?;
        if commit_count == 0 {
            // Legitimate no-op: work already on base, or inspection-only
            // graders with no diff. The lifecycle row is the source of truth;
            // just clean up the empty branch+worktree.
            let reason = format!(
                "{} reached DONE with no commits beyond {base}; nothing to merge",
                req.task_id
            );
            self.log(&reason);
            self.cleanup(&worktree, &branch)?;
            return Ok(SubmitOutcome::no_op(reason));
        }

        let landing = Landing {
            req,
            phase,
            worktree,
            branch,
            base,
            commit_count,
        };

        let violations = self.protected_violations(&landing.branch, &landing.base)?;
        if !violations.is_empty() {
            let joined = violations.join(", ");
            self.log(format!(
                "{} touches protected path(s) {joined}; refusing to merge, parking worktree at {}",
                req.task_id,
                landing.worktree.display()
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_PROTECTED_PATHS,
                format!(
                    "{} touches protected path(s) {joined}; refusing to merge, worktree preserved at {}",
                    landing.branch,
                    landing.worktree.display()
                ),
                Vec::new(),
                None,
            ));
        }

        // Clean-FF path: when the branch already contains the current base
        // its tree IS the tree that will become the base, so the standing
        // build invariant gates it here, before the FF.
        if self.is_ancestor(&landing.base, &landing.branch)? {
            let verify = self.standing_verify(req, &landing.worktree);
            if !verify.passed {
                self.log(format!(
                    "standing verify failed for {}; refusing to merge, parking worktree at {}",
                    landing.branch,
                    landing.worktree.display()
                ));
                return Ok(self.park(
                    &req.run_id,
                    PARK_KIND_STANDING_VERIFY,
                    format!(
                        "{} failed the standing build invariant ([submit] verify) against the \
                         tree it would land; worktree preserved at {}",
                        landing.branch,
                        landing.worktree.display()
                    ),
                    verify.receipts,
                    None,
                ));
            }
            // Only now that the tree has passed do we materialize the ref it
            // lands onto (a no-op for merge/pr; the phase strategy creates the
            // integration branch on the phase's first landing -- never on a
            // park). Stamp the exact tree just verified against that ref,
            // then FF it (spec 00078).
            let land_ref = self.materialize_landing_base(&landing.phase, &landing.base)?;
            self.stamp_trailers(
                req,
                &landing.worktree,
                &landing.branch,
                &land_ref,
                &landing.phase,
            )?;
            if self.ff_merge(&landing.branch, &land_ref)? {
                self.log(format!(
                    "Merged {} into {land_ref} ({} commit(s))",
                    landing.branch, landing.commit_count
                ));
                return self.land(&landing, &land_ref, RUNG_FAST_FORWARD, None);
            }
        }

        // FF failed (base advanced): rebase once, re-verify, retry FF, and
        // when the rebase itself conflicts fall through to the merge-fallback
        // recovery rung (spec 00076, D-1) rather than parking here.
        self.log(format!(
            "FF failed for {}; rebasing onto {}",
            landing.branch, landing.base
        ));
        if !git(&landing.worktree, &["rebase", &landing.base])?.ok() {
            git(&landing.worktree, &["rebase", "--abort"])?;
            return self.merge_fallback(&landing);
        }
        let reverify = self.reverify(req, &landing.worktree)?;
        if !reverify.passed {
            self.log(format!(
                "post-rebase re-verification failed for {}; parking worktree at {}",
                landing.branch,
                landing.worktree.display()
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_DIVERGENT_BASE,
                format!(
                    "{} failed post-rebase re-verification against the advanced {}; its command \
                     graders no longer pass on the rebased tree; worktree preserved at {}",
                    landing.branch,
                    landing.base,
                    landing.worktree.display()
                ),
                reverify.receipts,
                None,
            ));
        }
        // The rebased worktree is now the exact post-merge tree: gate it on
        // the standing build invariant before the FF, same as the clean path.
        let verify = self.standing_verify(req, &landing.worktree);
        if !verify.passed {
            self.log(format!(
                "post-rebase standing verify failed for {}; parking worktree at {}",
                landing.branch,
                landing.worktree.display()
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_STANDING_VERIFY,
                format!(
                    "{} failed the standing build invariant ([submit] verify) against the rebased \
                     tree it would land; worktree preserved at {}",
                    landing.branch,
                    landing.worktree.display()
                ),
                verify.receipts,
                None,
            ));
        }
        let land_ref = self.materialize_landing_base(&landing.phase, &landing.base)?;
        self.stamp_trailers(
            req,
            &landing.worktree,
            &landing.branch,
            &land_ref,
            &landing.phase,
        )?;
        if self.ff_merge(&landing.branch, &land_ref)? {
            self.log(format!(
                "Merged {} into {land_ref} after rebase ({} commit(s))",
                landing.branch, landing.commit_count
            ));
            return self.land(&landing, &land_ref, RUNG_REBASE, None);
        }
        self.log(format!(
            "post-rebase FF failed for {}; parking worktree at {}",
            landing.branch,
            landing.worktree.display()
        ));
        Ok(self.park(
            &req.run_id,
            PARK_KIND_DIVERGENT_BASE,
            format!(
                "{} cannot fast-forward {land_ref} even after a clean rebase + re-verify; \
                 worktree preserved at {}",
                landing.branch,
                landing.worktree.display()
            ),
            Vec::new(),
            None,
        ))
    }

    /// Merge-fallback recovery rung (spec 00076, D-1/D-2): land a DONE
    /// branch that can neither fast-forward nor cleanly rebase onto the
    /// advanced base by merging the base into the branch (`--no-ff`) and
    /// re-verifying the MERGED candidate tree at rebase parity before the
    /// fast-forward. Any failure resets the branch to its pre-merge tip
    /// (base ref byte-identical), records no `Landed`, and parks under a
    /// queryable kind. When the merge itself conflicts and the bounded
    /// agentic rung is armed, escalates to it with the conflicted worktree
    /// left in place; otherwise aborts and parks `merge-conflict`.
    fn merge_fallback(&self, landing: &Landing<'_>) -> Result<SubmitOutcome, SubmitError> {
        let Landing {
            req,
            worktree,
            branch,
            base,
            ..
        } = landing;
        self.log(format!(
            "rebase failed for {branch}; attempting merge-fallback of {base} into {branch}"
        ));
        let pre_merge = git(worktree, &["rev-parse", "HEAD"])?.trimmed().to_owned();
        // The worker authors this merge commit (its own landing bookkeeping,
        // not a change to the agent's work); a reused parked worktree may
        // lack a commit identity, so (re)establish the fixed one.
        self.set_commit_identity(worktree)?;
        let merge = git(
            worktree,
            &[
                "merge",
                "--no-ff",
                "-m",
                &format!("flywheel: merge-fallback land of {branch} onto {base}"),
                base,
            ],
        )?;
        if !merge.ok() {
            if self.recovery_agent_max_turns > 0 {
                if let Some(resolver) = self.resolve_conflict.clone() {
                    return self.agent_resolve(landing, &pre_merge, resolver);
                }
                self.log(format!(
                    "recovery_agent_max_turns={} but no conflict resolver is wired; parking \
                     {branch} as merge-conflict",
                    self.recovery_agent_max_turns
                ));
            }
            git(worktree, &["merge", "--abort"])?;
            self.log(format!(
                "merge-fallback of {base} into {branch} conflicted; parking worktree at {}",
                worktree.display()
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_MERGE_CONFLICT,
                format!(
                    "{branch} cannot fast-forward or rebase onto {base}; the merge-fallback of \
                     the base into the branch conflicted; worktree preserved at {}",
                    worktree.display()
                ),
                Vec::new(),
                None,
            ));
        }
        self.verify_merged_tree_and_land(landing, &pre_merge, MergedTreeRung::MergeFallback, None)
    }

    /// Bounded agentic conflict-resolution rung (spec 00076, criterion 4).
    /// Reached only when the rung is armed and the merge-fallback merge
    /// itself conflicted, leaving an in-progress merge with conflict markers
    /// in the worktree. Drives exactly one bounded session, then treats the
    /// result exactly like the merge-fallback rung, recording the session's
    /// usage on whichever outcome results. Never fails: a session crash is
    /// contained here.
    fn agent_resolve(
        &self,
        landing: &Landing<'_>,
        pre_merge: &str,
        resolver: ConflictResolver,
    ) -> Result<SubmitOutcome, SubmitError> {
        let Landing {
            req,
            worktree,
            branch,
            base,
            ..
        } = landing;
        self.log(format!(
            "merge-fallback of {base} into {branch} conflicted; escalating to a bounded agent \
             session (max_turns={}, max_wall_seconds={})",
            self.recovery_agent_max_turns, self.recovery_agent_max_wall_seconds
        ));
        let prompt = render_conflict_resolution_prompt(req, branch, base);
        let started = Instant::now();
        let report = resolver(&ConflictResolutionRequest {
            prompt,
            worktree: worktree.clone(),
            max_turns: self.recovery_agent_max_turns,
            max_wall_seconds: self.recovery_agent_max_wall_seconds,
            model: self.recovery_agent_model.clone(),
        });
        let usage = match report {
            Ok(report) => AgentUsage {
                turns: report.turns,
                wall_seconds: report.wall_seconds,
            },
            Err(err) => {
                // A session crash must not unwind submit: abort the merge,
                // reset to the pre-merge tip (base untouched), and park
                // preserved with the wall we observed.
                let wall = started
                    .elapsed()
                    .as_secs_f64()
                    .min(self.recovery_agent_max_wall_seconds);
                git(worktree, &["merge", "--abort"])?;
                git(worktree, &["reset", "--hard", pre_merge])?;
                self.log(format!(
                    "conflict-resolution session for {branch} crashed (ConflictResolutionError: \
                     {err}); parking worktree at {}",
                    worktree.display()
                ));
                return Ok(self.park(
                    &req.run_id,
                    PARK_KIND_MERGE_CONFLICT,
                    format!(
                        "{branch} merge-fallback conflicted and the bounded conflict-resolution \
                         session crashed (ConflictResolutionError: {err}); worktree preserved at {}",
                        worktree.display()
                    ),
                    Vec::new(),
                    Some(AgentUsage {
                        turns: 0,
                        wall_seconds: wall,
                    }),
                ));
            }
        };

        // The agent's claim is untrusted: decide from git state whether the
        // merge is resolved and, if so, author the merge commit ourselves.
        if !self.complete_agent_merge(worktree)? {
            git(worktree, &["merge", "--abort"])?;
            git(worktree, &["reset", "--hard", pre_merge])?;
            self.log(format!(
                "bounded agent session did not resolve the merge conflict on {branch} within its \
                 bounds ({} turn(s)/{:.1}s); parking worktree at {}",
                usage.turns,
                usage.wall_seconds,
                worktree.display()
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_MERGE_CONFLICT,
                format!(
                    "{branch} merge-fallback conflicted and the bounded conflict-resolution \
                     session did not produce a resolved tree within {} turn(s)/{:.1}s; worktree \
                     preserved at {}",
                    usage.turns,
                    usage.wall_seconds,
                    worktree.display()
                ),
                Vec::new(),
                Some(usage),
            ));
        }
        self.verify_merged_tree_and_land(
            landing,
            pre_merge,
            MergedTreeRung::AgentResolved,
            Some(usage),
        )
    }

    /// Decide from git state alone whether the bounded session resolved the
    /// in-progress merge -- and, if so, author the merge commit. A resolved
    /// merge has no unmerged index entries left; a leftover dirty tree after
    /// the commit is rejected too, so the tree that is re-verified is
    /// byte-identical to the tree that lands.
    fn complete_agent_merge(&self, worktree: &Path) -> Result<bool, GitError> {
        if !git(worktree, &["ls-files", "--unmerged"])?
            .trimmed()
            .is_empty()
        {
            return Ok(false);
        }
        let merge_in_progress = git(worktree, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])?.ok();
        if merge_in_progress && !git(worktree, &["commit", "--no-edit"])?.ok() {
            return Ok(false);
        }
        Ok(git(worktree, &["status", "--porcelain"])?
            .trimmed()
            .is_empty())
    }

    /// The merged-tree landing bar shared by the merge-fallback and
    /// agent-resolved rungs: the protected-path check on the merged tree,
    /// then task command graders, then the standing build invariant, then
    /// the declared held-out gate, then a fast-forward-only advance of the
    /// base. Any failure resets the branch to `pre_merge`
    /// (base ref untouched, no `Landed`) and parks with the deciding
    /// check's receipts; a clean pass fast-forwards and records a `Landed`
    /// at the rung.
    fn verify_merged_tree_and_land(
        &self,
        landing: &Landing<'_>,
        pre_merge: &str,
        rung: MergedTreeRung,
        usage: Option<AgentUsage>,
    ) -> Result<SubmitOutcome, SubmitError> {
        let Landing {
            req,
            phase,
            worktree,
            branch,
            base,
            commit_count,
        } = landing;
        let label = rung.label();
        let prefix = rung.detail_prefix();
        let tree = rung.tree_word();
        let wt = worktree.display();

        // The pre-rung check cleared the task's own commits of every
        // protected path, so any protected path the merged tree changes
        // relative to the base was introduced by the merge itself -- for the
        // agent-resolved rung, by an untrusted session. Checked before the
        // graders run so a rewritten grader script never executes.
        let violations = self.merged_tree_protected_violations(branch, base)?;
        if !violations.is_empty() {
            let joined = violations.join(", ");
            git(worktree, &["reset", "--hard", pre_merge])?;
            self.log(format!(
                "{label} tree for {branch} touches protected path(s) {joined}; refusing to merge, \
                 parking worktree at {wt}"
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_PROTECTED_PATHS,
                format!(
                    "{branch} {prefix}touches protected path(s) {joined} on the {tree} tree it \
                     would land; refusing to merge, worktree preserved at {wt}"
                ),
                Vec::new(),
                usage,
            ));
        }
        let reverify = self.reverify(req, worktree)?;
        if !reverify.passed {
            git(worktree, &["reset", "--hard", pre_merge])?;
            self.log(format!(
                "{label} re-verification failed for {branch}; parking worktree at {wt}"
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_DIVERGENT_BASE,
                format!(
                    "{branch} {prefix}failed post-merge re-verification against the merged {base}; \
                     its command graders no longer pass on the {tree} tree; worktree preserved at {wt}"
                ),
                reverify.receipts,
                usage,
            ));
        }
        let verify = self.standing_verify(req, worktree);
        if !verify.passed {
            git(worktree, &["reset", "--hard", pre_merge])?;
            self.log(format!(
                "{label} standing verify failed for {branch}; parking worktree at {wt}"
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_STANDING_VERIFY,
                format!(
                    "{branch} {prefix}failed the standing build invariant ([submit] verify) against \
                     the {tree} tree it would land; worktree preserved at {wt}"
                ),
                verify.receipts,
                usage,
            ));
        }
        let gate = self.held_out_gate(req, worktree)?;
        if !gate.passed {
            git(worktree, &["reset", "--hard", pre_merge])?;
            self.log(format!(
                "{label} held-out gate blocked {branch}; parking worktree at {wt}"
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_HELD_OUT_GATE,
                format!(
                    "{branch} {prefix}was blocked by the declared held-out landing gate on the \
                     {tree} tree it would land; worktree preserved at {wt}"
                ),
                gate.receipts,
                usage,
            ));
        }

        // Every rung passed against the merged tree. Materialize the landing
        // ref, then FF it: the base is an ancestor of the merge commit, so
        // under the lock the fast-forward is exact.
        let land_ref = self.materialize_landing_base(phase, base)?;
        if !self.ff_merge(branch, &land_ref)? {
            git(worktree, &["reset", "--hard", pre_merge])?;
            self.log(format!(
                "{label} FF of {branch} onto {land_ref} was refused; parking worktree at {wt}"
            ));
            return Ok(self.park(
                &req.run_id,
                PARK_KIND_DIVERGENT_BASE,
                format!(
                    "{branch} passed {label} re-verification but the fast-forward onto {land_ref} \
                     was refused; worktree preserved at {wt}"
                ),
                Vec::new(),
                usage,
            ));
        }
        match usage {
            Some(usage) => self.log(format!(
                "Merged {branch} into {land_ref} via agent-resolved conflict resolution \
                 ({commit_count} commit(s); {} turn(s)/{:.1}s)",
                usage.turns, usage.wall_seconds
            )),
            None => self.log(format!(
                "Merged {branch} into {land_ref} via merge-fallback ({commit_count} commit(s))"
            )),
        }
        self.land(landing, &land_ref, rung.rung(), usage)
    }

    /// Record the landed witness and dispose of the worktree per retention.
    fn land(
        &self,
        landing: &Landing<'_>,
        land_ref: &str,
        rung: &str,
        usage: Option<AgentUsage>,
    ) -> Result<SubmitOutcome, SubmitError> {
        let landed_ref = git(&self.repo_root, &["rev-parse", land_ref])?
            .trimmed()
            .to_owned();
        self.record_landing(
            &landing.req.run_id,
            &Landed {
                strategy: LANDING_STRATEGY_MERGE.to_owned(),
                landed_ref: landed_ref.clone(),
                rung: rung.to_owned(),
                agent_turns: usage.map(|u| u.turns),
                agent_wall_seconds: usage.map(|u| u.wall_seconds),
            },
        );
        self.teardown_on_done(&landing.worktree, &landing.branch)?;
        Ok(SubmitOutcome::landed(LANDING_STRATEGY_MERGE, landed_ref))
    }

    /// Record the park witness and return the park outcome; the worktree and
    /// branch are left exactly as they are.
    fn park(
        &self,
        run_id: &str,
        park_kind: &str,
        detail: String,
        receipts: Vec<GateGraderReceipt>,
        usage: Option<AgentUsage>,
    ) -> SubmitOutcome {
        self.record_landing_park(
            run_id,
            &LandingParked {
                park_kind: park_kind.to_owned(),
                detail: detail.clone(),
                receipts,
                agent_turns: usage.map(|u| u.turns),
                agent_wall_seconds: usage.map(|u| u.wall_seconds),
            },
        );
        SubmitOutcome::parked(park_kind, detail)
    }

    // --- the checks ----------------------------------------------------------------------------

    /// Run the declared held-out landing gate against the merged candidate
    /// tree (spec 00076, D-2). No source wired => passes with no receipts. A
    /// blocking verdict (a fail, or a fail-closed evaluation error) returns
    /// one receipt per executed held-out grader.
    fn held_out_gate(
        &self,
        req: &SubmitRequest,
        worktree: &Path,
    ) -> Result<CheckVerdict, SubmitError> {
        let Some(source) = &self.held_out_source else {
            return Ok(CheckVerdict::pass_without_receipts());
        };
        let options = GateOptions {
            env: self.grader_env.clone(),
            per_grader_timeout: None,
        };
        let source = Arc::clone(source);
        let verdict = block_on_dedicated_thread(|| async {
            evaluate_held_out_gate(
                &req.task,
                source.as_ref(),
                worktree,
                &req.run_id,
                &options,
                Utc::now,
            )
            .await
        })
        .map_err(SubmitError::Runtime)?;
        let receipts = verdict
            .results
            .iter()
            .map(|record| GateGraderReceipt {
                grader_name: Some(grader_label(record)),
                passed: record.passed,
                output_excerpt: payload_excerpt(&record.payload),
            })
            .collect();
        if verdict.blocks_landing() {
            self.log(format!(
                "held-out gate BLOCKED {} on the merged tree: {}",
                req.task_id, verdict.reason
            ));
        }
        Ok(CheckVerdict {
            passed: !verdict.blocks_landing(),
            receipts,
        })
    }

    /// Repo-relative paths the branch touches that match a protected
    /// pattern. The diff is merge-base scoped (`base...branch`) so only the
    /// branch's own changes count. This is the merge-time half of the
    /// verification trust boundary: work that rewrites the verification
    /// surface itself never lands, regardless of what its graders said.
    pub fn protected_violations(&self, branch: &str, base: &str) -> Result<Vec<String>, GitError> {
        self.protected_in_diff(&format!("{base}...{branch}"))
    }

    /// Repo-relative protected paths whose content in the merged `branch`
    /// tree differs from `base`. A direct tree diff rather than the
    /// merge-base scoped one: after a merge the branch contains the base, so
    /// this is exactly what landing would change -- including anything a
    /// conflict resolution wrote that the task's own commits never touched.
    pub fn merged_tree_protected_violations(
        &self,
        branch: &str,
        base: &str,
    ) -> Result<Vec<String>, GitError> {
        self.protected_in_diff(&format!("{base}..{branch}"))
    }

    fn protected_in_diff(&self, range: &str) -> Result<Vec<String>, GitError> {
        if self.protected_paths.is_empty() {
            return Ok(Vec::new());
        }
        let res = git(&self.repo_root, &["diff", "--name-only", range])?;
        if !res.ok() {
            // Cannot establish what the branch touches: fail closed.
            return Ok(vec![format!("<diff failed: {}>", res.stderr.trim())]);
        }
        Ok(res
            .stdout
            .lines()
            .filter(|path| !path.is_empty())
            .filter(|path| {
                self.protected_paths
                    .iter()
                    .any(|pattern| protected_path_matches(path, pattern))
            })
            .map(str::to_owned)
            .collect())
    }

    /// Re-run the task's command graders against the rebased/merged tree,
    /// cwd'd to the worktree and still under the merge lock. Transcript /
    /// rubric / manual graders judge the agent's work, not the tree, and
    /// stay valid; a task with no command graders has nothing
    /// tree-dependent to re-check.
    fn reverify(&self, req: &SubmitRequest, worktree: &Path) -> Result<CheckVerdict, SubmitError> {
        let command_count = req
            .task
            .graders
            .iter()
            .filter(|g| matches!(g, Grader::Command(_)))
            .count();
        if command_count == 0 {
            self.log(format!(
                "{} has no command graders; nothing to re-verify after rebase",
                req.task_id
            ));
            return Ok(CheckVerdict::pass_without_receipts());
        }
        let options = CommandGraderOptions {
            cwd: Some(worktree.to_path_buf()),
            env: self.grader_env.clone(),
            ..CommandGraderOptions::default()
        };
        let recorder = ReverifyRecorder::new();
        let run = block_on_dedicated_thread(|| async {
            // Merge-gate checks sit outside the lifecycle's attempt numbering;
            // receipts are logged, not persisted.
            run_command_graders(&req.task, &recorder, &req.run_id, 0, &options, Utc::now).await
        })
        .map_err(SubmitError::Runtime)?;
        let records = match run {
            Ok(records) => records,
            Err(err) => {
                // The in-memory recorder never fails and no artifacts dir is
                // set, so this is unreachable in practice; fail closed anyway.
                self.log(format!(
                    "re-verify {}: grader runner errored ({err}); treating as failed",
                    req.task_id
                ));
                recorder.records()
            }
        };
        let mut receipts = Vec::with_capacity(records.len());
        for record in &records {
            let label = grader_label(record);
            let verdict = if record.passed { "pass" } else { "FAIL" };
            self.log(format!(
                "re-verify {}: {verdict} [{label}] ({}ms)",
                req.task_id, record.duration_ms
            ));
            receipts.push(GateGraderReceipt {
                grader_name: Some(record.grader_name.clone().unwrap_or(label)),
                passed: record.passed,
                output_excerpt: payload_excerpt(&record.payload),
            });
        }
        let passed = records.len() == command_count && records.iter().all(|r| r.passed);
        Ok(CheckVerdict { passed, receipts })
    }

    /// Run the policy's standing build invariant (`[submit] verify`, spec
    /// 00064) against the exact tree about to become the base: the
    /// repo-wide "the trunk must build" gate, independent of the task's own
    /// graders. Unset => passes with no receipt. Never fails into `submit`:
    /// a command that cannot even spawn is a non-zero exit, i.e. a park.
    fn standing_verify(&self, req: &SubmitRequest, worktree: &Path) -> CheckVerdict {
        let Some(command) = &self.verify_command else {
            return CheckVerdict::pass_without_receipts();
        };
        self.log(format!(
            "standing verify for {}: {command:?} in {}",
            req.task_id,
            worktree.display()
        ));
        let proc = run_shell(command, worktree, self.grader_env.as_ref());
        let passed = proc.ok();
        if !passed {
            let tail = char_tail(
                if proc.stderr.is_empty() {
                    &proc.stdout
                } else {
                    &proc.stderr
                },
                2000,
            );
            self.log(format!(
                "standing verify FAILED (exit {}) for {}: {tail}",
                proc.code
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                req.task_id
            ));
        }
        CheckVerdict {
            passed,
            receipts: vec![GateGraderReceipt {
                grader_name: Some(command.clone()),
                passed,
                output_excerpt: bounded_output_excerpt(
                    &[proc.stdout.as_str(), proc.stderr.as_str()],
                    GATE_EXCERPT_MAX_BYTES,
                ),
            }],
        }
    }

    // --- stamping, fast-forward, teardown -------------------------------------------------

    /// Stamp harness-authoritative provenance trailers onto the task
    /// branch's `base..branch` commits, then advance the branch and
    /// worktree onto the rewritten tip (spec 00078). Message-only, so the
    /// tree that just passed verification is byte-identically the tree
    /// that lands; `reset --hard` onto the tree-identical tip changes no
    /// file on disk, only the branch ref the FF reads.
    pub fn stamp_trailers(
        &self,
        req: &SubmitRequest,
        worktree: &Path,
        branch: &str,
        base: &str,
        phase: &str,
    ) -> Result<(), SubmitError> {
        let new_tip = stamp_commit_messages(
            &self.repo_root,
            base,
            branch,
            &provenance_trailers(&req.task_id, &req.run_id, phase),
        )?;
        git_checked(worktree, &["reset", "--hard", &new_tip])?;
        Ok(())
    }

    /// True when `base` is the operator's checked-out branch in
    /// `repo_root`. A phase integration branch is never checked out, so the
    /// phase strategy always takes the out-of-tree advance path.
    pub fn base_is_checked_out(&self, base: &str) -> Result<bool, GitError> {
        Ok(checked_out_branch(&self.repo_root)?.as_deref() == Some(base))
    }

    /// Fast-forward `base` to `branch`'s tip. When `base` is the operator's
    /// checked-out branch, advance it in-tree with `git merge --ff-only`;
    /// otherwise advance its ref out-of-tree via `git fetch . <branch>:<base>`
    /// so the operator's working tree, index, and HEAD are never touched.
    /// Both paths are fast-forward-only: a non-FF advance returns `false`.
    pub fn ff_merge(&self, branch: &str, base: &str) -> Result<bool, GitError> {
        if self.base_is_checked_out(base)? {
            return Ok(git(&self.repo_root, &["merge", "--ff-only", branch])?.ok());
        }
        Ok(git(
            &self.repo_root,
            &["fetch", ".", &format!("{branch}:{base}")],
        )?
        .ok())
    }

    /// Remove the worktree and force-delete the branch. Every caller has
    /// already proven `branch` is contained in the landing base
    /// (`commit_count == 0`, or a successful FF-merge); `git branch -d`
    /// checks mergedness against the *checked-out HEAD*, not the landing
    /// base, so it would refuse and silently leak the ref in the
    /// out-of-tree layout. `-D` deletes against the established containment.
    pub fn cleanup(&self, worktree: &Path, branch: &str) -> Result<(), GitError> {
        let worktree_str = worktree.to_string_lossy().into_owned();
        git(
            &self.repo_root,
            &["worktree", "remove", worktree_str.as_str()],
        )?;
        git(&self.repo_root, &["branch", "-D", branch])?;
        Ok(())
    }

    /// Dispose a worktree whose branch has just landed in the base, per
    /// `on_done`. The empty-branch (zero-commit-DONE) case is not routed
    /// here; it always cleans up.
    fn teardown_on_done(&self, worktree: &Path, branch: &str) -> Result<(), GitError> {
        if self.on_done == "preserve" {
            self.log(format!(
                "on_done=preserve; keeping landed worktree at {} for inspection",
                worktree.display()
            ));
            return Ok(());
        }
        self.cleanup(worktree, branch)
    }

    /// Dispose a non-DONE terminal worktree per `on_failure`.
    fn teardown_on_failure(
        &self,
        worktree: &Path,
        branch: &str,
        status: Status,
    ) -> Result<SubmitOutcome, GitError> {
        if self.on_failure == "destroy" {
            let reason = format!(
                "Lifecycle {status}; on_failure=destroy, removing worktree at {}",
                worktree.display()
            );
            self.log(&reason);
            self.cleanup(worktree, branch)?;
            return Ok(SubmitOutcome::no_op(reason));
        }
        let reason = format!(
            "Lifecycle {status}; worktree preserved at {}",
            worktree.display()
        );
        self.log(&reason);
        Ok(SubmitOutcome::no_op(reason))
    }

    // --- ledger witnesses ------------------------------------------------------------------

    /// Append a queryable `landing_parked` audit-witness event for a parked
    /// DONE run. The run already finalized `Done` (terminal); this records
    /// the park cause on the run's ledger WITHOUT any lifecycle transition
    /// (`LandingParked` folds to the identity). Best-effort: a missing
    /// store, a declined preflight, or any store error is logged, never
    /// raised, so `submit` cannot escape into the orchestrator.
    pub fn record_landing_park(&self, run_id: &str, park: &LandingParked) {
        self.append_witness(
            run_id,
            DomainEventPayload::LandingParked(park.clone()),
            "landing-parked",
        );
    }

    /// Append a queryable `landed` audit-witness event for a run whose
    /// branch actually landed. The caller invokes it only *after* the land
    /// completed, so an incomplete land leaves no `Landed` record.
    pub fn record_landing(&self, run_id: &str, landed: &Landed) {
        self.append_witness(run_id, DomainEventPayload::Landed(landed.clone()), "landed");
    }

    fn append_witness(&self, run_id: &str, payload: DomainEventPayload, label: &str) {
        let Some(store) = &self.store else {
            return;
        };
        let mut append = || -> Result<(), StoreError> {
            let Some(lifecycle) = store.load_lifecycle(run_id)? else {
                self.log(format!(
                    "cannot record {label} event: no lifecycle for {run_id}"
                ));
                return Ok(());
            };
            let event = DomainEvent::new(run_id, Utc::now(), payload.clone());
            store.append_domain_event(&event, lifecycle.version)?;
            Ok(())
        };
        let result = match &self.ledger_guard {
            Some(guard) => guard.guard(&self.repo_root, run_id, &mut append),
            None => append().map_err(LedgerWriteError::Store),
        };
        if let Err(err) = result {
            self.log(format!(
                "failed to record {label} event for {run_id} ({err})"
            ));
        }
    }
}

impl SubmitStrategy for GitWorktreeSubmitter {
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
        Ok(SandboxHandle::new(self.prepare_worktree(request)?))
    }

    /// Merge a DONE task's branch into the base, or park the worktree.
    /// Never fails (the orchestrator calls this under the task's lease);
    /// any git failure is logged, witnessed as a `submit-error` park, and
    /// the worktree left parked for inspection.
    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome {
        match self.submit_inner(request) {
            Ok(outcome) => outcome,
            Err(err) => {
                let worktree = self.worktree(&request.task_id);
                self.log(format!(
                    "submit error for {} ({}: {err}); worktree left parked at {}",
                    request.task_id,
                    err.kind(),
                    worktree.display()
                ));
                self.park(
                    &request.run_id,
                    PARK_KIND_SUBMIT_ERROR,
                    format!(
                        "submit raised {}: {err}; worktree left parked at {}",
                        err.kind(),
                        worktree.display()
                    ),
                    Vec::new(),
                    None,
                )
            }
        }
    }

    fn is_landable(&self, request: &SubmitRequest) -> LandabilityVerdict {
        match self.landability(request) {
            Ok(verdict) => verdict,
            Err(err) => LandabilityVerdict::not_landable(format!(
                "cannot inspect the worktree for {}: {err}",
                request.task_id
            )),
        }
    }
}

/// Build the merge backend (the registry's `merge` target) from the shared
/// builder request: the `[submit] verify` standing invariant and the
/// bounded conflict-resolution rung's bounds/model come off `policy`
/// (spec 00064 / 00076); everything else is the request's own fields. A
/// `None` policy leaves the rung disabled, byte-identical to a
/// merge-conflict park.
pub fn build_merge_submitter(
    request: &crate::submit_registry::SubmitBuildRequest,
) -> GitWorktreeSubmitter {
    let policy = request.policy.as_deref();
    let mut submitter = GitWorktreeSubmitter::new(
        request.repo_root.clone(),
        request.tasks_dir.clone(),
        request.worktrees_dir.clone(),
        request.phase_base.clone(),
        request.lock_path.clone(),
        Arc::clone(&request.log),
    )
    .with_protected_paths(request.protected_paths.iter().cloned())
    .with_retention(request.on_done.clone(), request.on_failure.clone());
    submitter.setup_command = request.setup_command.clone();
    submitter.store = request.store.clone();
    submitter.grader_env = request.grader_env.clone();
    submitter.held_out_source = request.held_out_source.clone();
    submitter.resolve_conflict = request.resolve_conflict.clone();
    if let Some(policy) = policy {
        submitter.verify_command = policy.submit_verify.clone();
        submitter.recovery_agent_max_turns =
            u32::try_from(policy.submit_recovery_agent_max_turns.max(0)).unwrap_or(u32::MAX);
        submitter.recovery_agent_max_wall_seconds = policy.submit_recovery_agent_max_wall_seconds;
        submitter.recovery_agent_model = policy.model.clone();
    }
    submitter
}

/// The label a grader record is reported under: its name, else its `run`
/// command.
fn grader_label(record: &GraderResultRecord) -> String {
    record.grader_name.clone().unwrap_or_else(|| {
        record
            .grader_spec
            .get("run")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    })
}

fn absolutize(anchor: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        fs::canonicalize(anchor.join(path)).unwrap_or_else(|_| anchor.join(path))
    }
}

fn remove_if_present(path: &Path) {
    if let Err(err) = fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        // A lock file the process cannot remove is not fatal here: the next
        // git call reports it with the actual path.
        let _ = err;
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Real-git fixtures shared with the phase-strategy tests.

    use super::*;
    use flywheel_core::events::LifecycleInitialized;
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::{CommandGrader, Task};

    pub fn git_ok(cwd: &Path, args: &[&str]) -> String {
        let out = git(cwd, args).expect("git spawns");
        assert!(
            out.ok(),
            "git {args:?} failed in {}: {}",
            cwd.display(),
            out.stderr
        );
        out.trimmed().to_owned()
    }

    pub fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git_ok(path, &["init", "-b", "main"]);
        git_ok(path, &["config", "user.email", "landing-test@example.com"]);
        git_ok(path, &["config", "user.name", "landing test"]);
        git_ok(path, &["config", "commit.gpgsign", "false"]);
        fs::write(path.join("README.md"), "base\n").unwrap();
        fs::write(path.join("file.txt"), "line1\n").unwrap();
        git_ok(path, &["add", "-A"]);
        git_ok(path, &["commit", "-m", "init"]);
    }

    pub fn rev(repo: &Path, r: &str) -> String {
        git_ok(repo, &["rev-parse", r])
    }

    pub fn commit(cwd: &Path, file: &str, body: &str, message: &str) -> String {
        fs::write(cwd.join(file), body).unwrap();
        git_ok(cwd, &["add", "-A"]);
        git_ok(cwd, &["commit", "-m", message]);
        rev(cwd, "HEAD")
    }

    pub fn task_file(repo: &Path, phase: &str, task_id: &str, run: &str) -> PathBuf {
        let tf = repo
            .join(".flywheel")
            .join("tasks")
            .join("active")
            .join(phase)
            .join(format!("{task_id}.json"));
        fs::create_dir_all(tf.parent().unwrap()).unwrap();
        fs::write(
            &tf,
            serde_json::json!({
                "id": task_id,
                "goal": format!("Goal for {task_id}."),
                "graders": [{"type": "command", "run": run}],
            })
            .to_string(),
        )
        .unwrap();
        tf
    }

    pub fn sandbox_req(tf: &Path, task_id: &str) -> SandboxRequest {
        SandboxRequest {
            task_id: task_id.to_owned(),
            task_file: Some(tf.to_path_buf()),
            run_id: None,
            mode: flywheel_orchestrator::strategy::SandboxMode::Fresh,
            source_ref: String::new(),
        }
    }

    pub fn submit_req(
        tf: &Path,
        task_id: &str,
        sandbox: &Path,
        status: Status,
        runs: &[&str],
    ) -> SubmitRequest {
        let graders = runs
            .iter()
            .map(|run| Grader::Command(CommandGrader::new(*run).unwrap()))
            .collect();
        let task = Task::new(format!("Goal for {task_id}."), graders).with_id(task_id);
        let mut req = SubmitRequest::new(task, "run-1", status, sandbox);
        req.task_file = Some(tf.to_path_buf());
        req
    }

    /// A ledger seeded with a lifecycle for `run-1` so witnesses append.
    pub fn ledger() -> Arc<InMemoryStore> {
        let store = InMemoryStore::new();
        let seed = DomainEvent::new(
            "run-1",
            Utc::now(),
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: "t1".into(),
                ..LifecycleInitialized::default()
            }),
        );
        store.append_domain_event(&seed, 0).unwrap();
        Arc::new(store)
    }

    pub fn parked(store: &InMemoryStore) -> Vec<LandingParked> {
        store
            .list_domain_events("run-1")
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                DomainEventPayload::LandingParked(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    pub fn landed(store: &InMemoryStore) -> Vec<Landed> {
        store
            .list_domain_events("run-1")
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                DomainEventPayload::Landed(l) => Some(l),
                _ => None,
            })
            .collect()
    }

    pub fn quiet() -> Logger {
        Arc::new(|_m: &str| {})
    }

    pub fn submitter(repo: &Path, store: Arc<InMemoryStore>) -> GitWorktreeSubmitter {
        let worktrees = repo.join(".flywheel").join("worktrees");
        fs::create_dir_all(&worktrees).unwrap();
        GitWorktreeSubmitter::new(
            repo,
            repo.join(".flywheel").join("tasks"),
            worktrees,
            "main",
            repo.join(".flywheel").join(".merge.lock"),
            quiet(),
        )
        .with_store(store)
    }

    pub fn is_ancestor(repo: &Path, ancestor: &str, rev: &str) -> bool {
        git(repo, &["merge-base", "--is-ancestor", ancestor, rev])
            .unwrap()
            .ok()
    }

    pub fn branch_exists(repo: &Path, branch: &str) -> bool {
        git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .unwrap()
        .ok()
    }

    pub fn trailer_value(repo: &Path, sha: &str, key: &str) -> String {
        git_ok(
            repo,
            &[
                "show",
                "-s",
                &format!("--format=%(trailers:key={key},valueonly)"),
                sha,
            ],
        )
    }

    pub fn range_shas(repo: &Path, range: &str) -> Vec<String> {
        git_ok(repo, &["rev-list", "--reverse", range])
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    /// Edit `file.txt` then revert it: the branch's net tree leaves the line
    /// untouched (so an advanced base merges cleanly) while a rebase replays
    /// the intermediate edit and conflicts.
    pub fn merge_bearing_branch(wt: &Path) -> String {
        commit(wt, "file.txt", "line1-branch\n", "diverge file.txt");
        commit(wt, "file.txt", "line1\n", "revert file.txt")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use flywheel_core::events::LANDING_PARK_KINDS;
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_orchestrator::held_out_gate::FilesystemHeldOutGraderSource;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        init_repo(&path);
        (dir, path)
    }

    fn prepared(
        s: &GitWorktreeSubmitter,
        repo: &Path,
        phase: &str,
        task_id: &str,
        run: &str,
    ) -> (PathBuf, PathBuf) {
        let tf = task_file(repo, phase, task_id, run);
        let wt = s.prepare_worktree(&sandbox_req(&tf, task_id)).unwrap();
        (tf, wt)
    }

    // --- plumbing -------------------------------------------------------------------

    #[test]
    fn git_outputs_are_data_and_checked_calls_fail_loudly() {
        let (_dir, repo) = repo();
        let out = git(&repo, &["rev-parse", "--verify", "nope"]).unwrap();
        assert!(!out.ok());
        let err = git_checked(&repo, &["rev-parse", "--verify", "nope"]).unwrap_err();
        assert!(matches!(err, GitError::Failed { .. }));
        assert!(err.to_string().contains("rev-parse --verify nope"));
        assert_eq!(
            git_checked(&repo, &["rev-parse", "main"])
                .unwrap()
                .trimmed(),
            rev(&repo, "main")
        );
    }

    #[test]
    fn progress_probe_tracks_head_and_errs_on_a_missing_sandbox() {
        let (_dir, repo) = repo();
        let probe = git_progress_probe(&repo);
        let before = probe().unwrap();
        assert_eq!(before, rev(&repo, "HEAD"));
        fs::write(repo.join("dirty.txt"), "x").unwrap();
        assert_eq!(
            probe().unwrap(),
            before,
            "an uncommitted edit is not progress"
        );
        commit(&repo, "a.txt", "1", "feat: a");
        assert_ne!(probe().unwrap(), before);
        let missing = git_progress_probe(repo.join("absent"));
        assert!(missing().is_err());
    }

    #[test]
    fn checked_out_branch_reports_detached_head_as_none() {
        let (_dir, repo) = repo();
        assert_eq!(checked_out_branch(&repo).unwrap().as_deref(), Some("main"));
        git_ok(&repo, &["checkout", "-q", "--detach"]);
        assert_eq!(checked_out_branch(&repo).unwrap(), None);
    }

    #[test]
    fn merge_lock_is_exclusive_across_holders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(".merge.lock");
        let first = merge_lock(&path).unwrap();
        assert_eq!(first.path(), path);
        let acquired = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&acquired);
        let p = path.clone();
        let waiter = std::thread::spawn(move || {
            let _second = merge_lock(&p).unwrap();
            flag.store(1, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            acquired.load(Ordering::SeqCst),
            0,
            "second holder must block"
        );
        drop(first);
        waiter.join().unwrap();
        assert_eq!(acquired.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn excerpts_join_segments_and_keep_the_tail() {
        assert_eq!(bounded_output_excerpt(&["a", "", "b"], 100), "a\nb");
        let big = format!("{}TAIL", "A".repeat(50));
        let excerpt = bounded_output_excerpt(&[&big], 8);
        assert_eq!(excerpt, "AAAATAIL");
        let mut payload = JsonObject::new();
        payload.insert("stdout_tail".into(), Value::String("out".into()));
        payload.insert("stderr_tail".into(), Value::String(String::new()));
        payload.insert("spawn_error".into(), Value::String("boom".into()));
        assert_eq!(payload_excerpt(&payload), "out\nboom");
    }

    #[test]
    fn protected_path_glob_matches_like_pure_posix_path_full_match() {
        assert!(protected_path_matches("conftest.py", "conftest.py"));
        assert!(!protected_path_matches("sub/conftest.py", "conftest.py"));
        assert!(protected_path_matches(
            ".github/workflows/ci.yml",
            ".github/**"
        ));
        assert!(protected_path_matches(".github", ".github/**"));
        assert!(protected_path_matches("a/b/c.py", "**/*.py"));
        assert!(protected_path_matches("c.py", "**/*.py"));
        assert!(!protected_path_matches("a/b/c.pyc", "**/*.py"));
        assert!(protected_path_matches("scripts/x", "scripts/*"));
        assert!(!protected_path_matches("scripts/x/y", "scripts/*"));
        assert!(protected_path_matches("a1", "a[0-9]"));
        assert!(!protected_path_matches("ab", "a[0-9]"));
        assert!(protected_path_matches("ab", "a[!0-9]"));
        assert!(protected_path_matches("ab", "a?"));
        assert!(protected_path_matches("a[", "a["));
    }

    #[test]
    fn conflict_prompt_is_byte_identical_to_the_python_renderer() {
        let req = SubmitRequest::new(
            flywheel_core::task::Task::new("Add the thing.", Vec::new()).with_id("t1"),
            "run-1",
            Status::Done,
            "/sb",
        );
        let prompt = render_conflict_resolution_prompt(&req, "flywheel/01/t1", "main");
        assert_eq!(
            prompt,
            "You are resolving a git merge conflict, nothing more.\n\n\
             The branch `flywheel/01/t1` implements this already-completed and verified task:\n\n\
             Add the thing.\n\n\
             A merge of the base branch `main` into `flywheel/01/t1` is in progress in this \
             worktree and has left conflict markers. Your ONLY job is to resolve every \
             conflicted file so the merged result preserves the task's completed work and the \
             base's changes together, then stage the resolved files with `git add`.\n\n\
             Strict rules:\n\
             - Resolve only the merge conflicts. Do NOT re-implement the task, add features, or \
             edit anything unrelated to a conflict.\n\
             - Leave no conflict markers (<<<<<<<, =======, >>>>>>>).\n\
             - Do NOT run `git commit`, `git merge --abort`, or `git reset`; the harness commits \
             and verifies the result.\n\
             - Do NOT touch verification config, graders, or CI.\n"
        );
    }

    // --- provisioning -----------------------------------------------------------------

    #[test]
    fn fresh_prepare_creates_worktree_on_task_branch_with_identity() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger());
        let (_tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        assert_eq!(wt, repo.join(".flywheel/worktrees/t1"));
        assert!(wt.is_dir());
        assert_eq!(
            git_ok(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "flywheel/01-phase/t1"
        );
        assert_eq!(rev(&wt, "HEAD"), rev(&repo, "main"));
        assert_eq!(
            git_ok(&wt, &["config", "user.name"]),
            WORKTREE_COMMIT_IDENTITY_NAME
        );
        assert_eq!(
            git_ok(&wt, &["config", "user.email"]),
            WORKTREE_COMMIT_IDENTITY_EMAIL
        );
        assert!(s.is_registered_worktree(&wt).unwrap());
        // Through the trait: a bare handle with the same path.
        let handle = s
            .prepare_sandbox(&sandbox_req(&repo.join("x.json"), "t2"))
            .unwrap();
        assert_eq!(handle.path, repo.join(".flywheel/worktrees/t2"));
        assert!(!handle.has_contributions());
        assert!(branch_exists(&repo, "flywheel/_root/t2"));
    }

    #[test]
    fn setup_command_runs_in_fresh_worktrees_and_its_failure_skips_the_task() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger()).with_setup_command("echo provisioned > setup.txt");
        let (_tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        assert_eq!(
            fs::read_to_string(wt.join("setup.txt")).unwrap().trim(),
            "provisioned"
        );

        let failing = submitter(&repo, ledger()).with_setup_command("echo nope >&2; exit 3");
        let tf = task_file(&repo, "01-phase", "t2", "true");
        let err = failing
            .prepare_worktree(&sandbox_req(&tf, "t2"))
            .unwrap_err();
        assert!(err.0.contains("exit 3"), "{err}");
        assert!(err.0.contains("nope"), "{err}");
        let prepare_err: PrepareError = err.into();
        assert!(prepare_err.message.contains("sandbox setup"));
    }

    #[test]
    fn reused_parked_worktree_rebases_onto_advanced_base_and_skips_setup() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger()).with_setup_command("touch setup-ran");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        fs::remove_file(wt.join("setup-ran")).unwrap();
        commit(&wt, "work.txt", "w", "agent work");
        commit(&repo, "other.txt", "o", "base advances");
        let again = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        assert_eq!(again, wt);
        assert!(
            wt.join("other.txt").exists(),
            "rebased onto the advanced base"
        );
        assert!(wt.join("work.txt").exists(), "prior commits carry forward");
        assert!(
            !wt.join("setup-ran").exists(),
            "a reused worktree skips setup"
        );
    }

    #[test]
    fn conflicting_parked_worktree_is_discarded_unless_its_run_is_done() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "file.txt", "line1-branch\n", "agent edit");
        commit(&repo, "file.txt", "line1-main\n", "base edit");
        // A non-DONE retry: the conflicting branch is discarded and recreated.
        let fresh = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        assert_eq!(fresh, wt);
        assert_eq!(rev(&wt, "HEAD"), rev(&repo, "main"));
        assert_eq!(
            fs::read_to_string(wt.join("file.txt")).unwrap(),
            "line1-main\n"
        );

        // A DONE run's branch that cannot rebase is preserved instead.
        commit(&wt, "file.txt", "line1-verified\n", "verified work");
        let verified_tip = rev(&wt, "HEAD");
        commit(&repo, "file.txt", "line1-main-2\n", "base edit 2");
        let mut lc = flywheel_core::lifecycle::Lifecycle::new("t1").with_run_id("run-done");
        for st in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::Done,
        ] {
            lc.transition_to(st, "", Some(Utc::now())).unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        let mut req = sandbox_req(&tf, "t1");
        req.run_id = Some("run-done".into());
        let err = s.prepare_worktree(&req).unwrap_err();
        assert!(err.0.contains("refusing to discard verified work"), "{err}");
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), verified_tip);
        assert!(wt.is_dir());
    }

    #[test]
    fn worktree_dir_removed_recreates_on_surviving_branch() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger());
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let tip = commit(&wt, "work.txt", "w", "agent work");
        fs::remove_dir_all(&wt).unwrap();
        git_ok(&repo, &["worktree", "prune"]);
        let again = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        assert_eq!(again, wt);
        assert_eq!(rev(&wt, "HEAD"), tip);
    }

    #[test]
    fn half_present_state_is_refused_not_clobbered() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger());
        let tf = task_file(&repo, "01-phase", "t1", "true");
        let wt = s.worktree("t1");
        fs::create_dir_all(&wt).unwrap();
        let err = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap_err();
        assert!(err.0.contains("no branch flywheel/01-phase/t1"), "{err}");
        git_ok(&repo, &["branch", "flywheel/01-phase/t1", "main"]);
        let err = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap_err();
        assert!(err.0.contains("not a registered worktree"), "{err}");
    }

    #[test]
    fn scrub_clears_stale_locks_and_aborts_in_progress_merge() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger());
        let (_tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "file.txt", "line1-branch\n", "agent edit");
        commit(&repo, "file.txt", "line1-main\n", "base edit");
        assert!(
            !git(&wt, &["merge", "main"]).unwrap().ok(),
            "merge conflicts"
        );
        let git_dir = PathBuf::from(git_ok(&wt, &["rev-parse", "--git-dir"]));
        fs::write(git_dir.join("index.lock"), "").unwrap();
        s.scrub_worktree_locks(&wt, "flywheel/01-phase/t1").unwrap();
        assert!(!git_dir.join("index.lock").exists());
        assert!(
            !git(&wt, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
                .unwrap()
                .ok()
        );
        assert_eq!(git_ok(&wt, &["status", "--porcelain"]), "");
    }

    // --- landability ------------------------------------------------------------------

    #[test]
    fn landability_requires_a_clean_tree_with_commits_and_never_mutates() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger());
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let empty = s.is_landable(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(!empty.landable);
        assert!(empty.reason.contains("main"), "{}", empty.reason);

        fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();
        let dirty = s.is_landable(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(!dirty.landable);
        assert!(dirty.reason.contains("uncommitted"));
        fs::remove_file(wt.join("dirty.txt")).unwrap();

        commit(&wt, "feature.txt", "x", "feat");
        fs::write(wt.join("feature.txt"), "x-modified").unwrap();
        let precedence = s.is_landable(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(!precedence.landable);
        assert!(precedence.reason.contains("uncommitted"));
        git_ok(&wt, &["checkout", "--", "feature.txt"]);

        let base_before = rev(&repo, "main");
        let branch_before = rev(&repo, "flywheel/01-phase/t1");
        let ok = s.is_landable(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert_eq!(ok, LandabilityVerdict::landable());
        assert!(wt.exists());
        assert_eq!(rev(&repo, "main"), base_before);
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_before);
        assert!(!repo.join("feature.txt").exists());
    }

    // --- flowchart: non-DONE, uncommitted, empty, protected ----------------------------

    #[test]
    fn non_done_parks_by_default_and_destroys_on_request() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Failed, &["true"]));
        assert!(matches!(&outcome, SubmitOutcome::NoOp { reason } if reason.contains("preserved")));
        assert!(wt.exists());
        assert!(landed(&store).is_empty());
        assert!(
            parked(&store).is_empty(),
            "a non-DONE park is not a landing park"
        );

        let destroy = s.clone().with_retention("destroy", "destroy");
        let outcome = destroy.submit(&submit_req(&tf, "t1", &wt, Status::Failed, &["true"]));
        assert!(matches!(&outcome, SubmitOutcome::NoOp { reason } if reason.contains("removing")));
        assert!(!wt.exists());
        assert!(!branch_exists(&repo, "flywheel/01-phase/t1"));
    }

    #[test]
    fn done_with_uncommitted_tree_parks_uncommitted_work() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        fs::write(wt.join("stray.txt"), "left behind").unwrap();
        let base_before = rev(&repo, "main");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert_eq!(
            outcome,
            SubmitOutcome::parked(
                PARK_KIND_UNCOMMITTED_WORK,
                format!(
                    "DONE with an uncommitted tree on flywheel/01-phase/t1; worktree preserved at {}",
                    wt.display()
                )
            )
        );
        assert_eq!(rev(&repo, "main"), base_before);
        assert!(wt.exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_UNCOMMITTED_WORK);
        assert!(parks[0].receipts.is_empty());
    }

    #[test]
    fn done_with_empty_diff_cleans_up_without_a_witness() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::NoOp { reason } if reason.contains("nothing to merge"))
        );
        assert!(!wt.exists());
        assert!(!branch_exists(&repo, "flywheel/01-phase/t1"));
        assert!(landed(&store).is_empty());
        assert!(parked(&store).is_empty());
    }

    #[test]
    fn protected_path_touch_parks_without_receipts() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store))
            .with_protected_paths(["conftest.py", ".github/**"]);
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        commit(&wt, "conftest.py", "tampered", "edit conftest");
        let base_before = rev(&repo, "main");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, detail }
            if park_kind == PARK_KIND_PROTECTED_PATHS && detail.contains("conftest.py"))
        );
        assert_eq!(rev(&repo, "main"), base_before);
        assert!(wt.exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_PROTECTED_PATHS);
        assert!(parks[0].receipts.is_empty());
        assert!(landed(&store).is_empty());
    }

    // --- flowchart: clean fast-forward ----------------------------------------------------

    #[test]
    fn clean_ff_lands_stamps_trailers_and_tears_down() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "a.txt", "1", "feat: a");
        commit(&wt, "b.txt", "2", "feat: b");
        let pre_trees: Vec<String> = range_shas(&wt, "main..HEAD")
            .iter()
            .map(|c| git_ok(&wt, &["rev-parse", &format!("{c}^{{tree}}")]))
            .collect();
        let before = rev(&repo, "main");

        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));

        let after = rev(&repo, "main");
        assert_ne!(after, before);
        assert_eq!(
            outcome,
            SubmitOutcome::landed(LANDING_STRATEGY_MERGE, after.clone())
        );
        assert!(repo.join("a.txt").exists() && repo.join("b.txt").exists());
        assert!(!wt.exists());
        assert!(!branch_exists(&repo, "flywheel/01-phase/t1"));
        let shas = range_shas(&repo, &format!("{before}..{after}"));
        assert_eq!(shas.len(), 2);
        for sha in &shas {
            assert_eq!(trailer_value(&repo, sha, "Flywheel-Task"), "t1");
            assert_eq!(trailer_value(&repo, sha, "Flywheel-Run"), "run-1");
            assert_eq!(trailer_value(&repo, sha, "Flywheel-Phase"), "01-phase");
        }
        let post_trees: Vec<String> = shas
            .iter()
            .map(|c| git_ok(&repo, &["rev-parse", &format!("{c}^{{tree}}")]))
            .collect();
        assert_eq!(post_trees, pre_trees);
        let lands = landed(&store);
        assert_eq!(lands.len(), 1);
        assert_eq!(lands[0].strategy, LANDING_STRATEGY_MERGE);
        assert_eq!(lands[0].rung, RUNG_FAST_FORWARD);
        assert_eq!(lands[0].landed_ref, after);
        assert_eq!(lands[0].agent_turns, None);
        assert!(parked(&store).is_empty());
    }

    #[test]
    fn clean_ff_out_of_tree_when_base_not_checked_out() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "feat: work");
        git_ok(&repo, &["checkout", "-q", "-b", "operator"]);
        let before = rev(&repo, "main");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        let after = rev(&repo, "main");
        assert_ne!(after, before);
        assert_eq!(
            git_ok(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "operator"
        );
        assert!(
            !repo.join("feature.txt").exists(),
            "operator checkout untouched"
        );
        let shas = range_shas(&repo, &format!("{before}..{after}"));
        assert_eq!(trailer_value(&repo, &shas[0], "Flywheel-Run"), "run-1");
        assert!(
            !branch_exists(&repo, "flywheel/01-phase/t1"),
            "-D deletes in the out-of-tree layout"
        );
        assert_eq!(landed(&store)[0].rung, RUNG_FAST_FORWARD);
    }

    #[test]
    fn on_done_preserve_keeps_the_landed_worktree() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger()).with_retention("preserve", "park");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(outcome.is_landed());
        assert!(repo.join("feature.txt").exists());
        assert!(wt.exists());
        assert!(branch_exists(&repo, "flywheel/01-phase/t1"));
    }

    #[test]
    fn standing_verify_failure_on_clean_ff_parks_with_the_check_output() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store))
            .with_verify_command("cat check_output.txt; exit 1");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        commit(
            &wt,
            "check_output.txt",
            "STANDING_CLEAN_FF_NONCE_5F3",
            "probe",
        );
        let before = rev(&repo, "main");
        // Zero command graders: the standing gate is independent of them.
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &[]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_STANDING_VERIFY)
        );
        assert_eq!(rev(&repo, "main"), before);
        assert!(!repo.join("feature.txt").exists());
        assert!(wt.exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_STANDING_VERIFY);
        assert_eq!(parks[0].receipts.len(), 1);
        assert!(!parks[0].receipts[0].passed);
        assert_eq!(
            parks[0].receipts[0].grader_name.as_deref(),
            Some("cat check_output.txt; exit 1")
        );
        assert!(
            parks[0].receipts[0]
                .output_excerpt
                .contains("STANDING_CLEAN_FF_NONCE_5F3")
        );
        assert!(landed(&store).is_empty());
    }

    #[test]
    fn standing_verify_inspects_the_to_be_landed_tree_and_passes_land() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger()).with_verify_command("test ! -f broken");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "broken", "x", "work");
        let before = rev(&repo, "main");
        assert!(
            s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]))
                .is_parked()
        );
        assert_eq!(rev(&repo, "main"), before);

        let ok = submitter(&repo, ledger()).with_verify_command("true");
        let (tf2, wt2) = prepared(&ok, &repo, "01-phase", "t2", "true");
        commit(&wt2, "fine.txt", "x", "work");
        assert!(
            ok.submit(&submit_req(&tf2, "t2", &wt2, Status::Done, &["true"]))
                .is_landed()
        );
        assert!(!wt2.exists());
    }

    #[test]
    fn standing_verify_excerpt_is_bounded_to_a_tail() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s =
            submitter(&repo, Arc::clone(&store)).with_verify_command("cat big_output.txt; exit 1");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let big = format!(
            "{}TRUNCATION_TAIL_NONCE_7A9",
            "A".repeat(GATE_EXCERPT_MAX_BYTES + 1000)
        );
        commit(&wt, "big_output.txt", &big, "oversize probe");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        let parks = parked(&store);
        let excerpt = &parks[0].receipts[0].output_excerpt;
        assert!(excerpt.len() <= GATE_EXCERPT_MAX_BYTES);
        assert!(excerpt.ends_with("TRUNCATION_TAIL_NONCE_7A9"));
        assert!(excerpt.matches('A').count() < GATE_EXCERPT_MAX_BYTES + 1000);
    }

    #[test]
    fn unspawnable_verify_command_parks_and_never_raises() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger()).with_verify_command("this-command-does-not-exist --x");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        let before = rev(&repo, "main");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_STANDING_VERIFY)
        );
        assert_eq!(rev(&repo, "main"), before);
        assert!(wt.exists());
    }

    // --- flowchart: base advanced, rebase ------------------------------------------------

    #[test]
    fn base_advanced_rebases_reverifies_and_lands_at_the_rebase_rung() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store)).with_verify_command("true");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "feat: work");
        commit(&repo, "other.txt", "y", "base moves on");
        let before = rev(&repo, "main");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        let after = rev(&repo, "main");
        assert_ne!(after, before);
        assert_eq!(
            outcome,
            SubmitOutcome::landed(LANDING_STRATEGY_MERGE, after.clone())
        );
        assert!(repo.join("feature.txt").exists() && repo.join("other.txt").exists());
        assert!(!wt.exists());
        let shas = range_shas(&repo, &format!("{before}..{after}"));
        assert_eq!(shas.len(), 1);
        assert_eq!(trailer_value(&repo, &shas[0], "Flywheel-Task"), "t1");
        let lands = landed(&store);
        assert_eq!(lands[0].rung, RUNG_REBASE);
        assert_eq!(lands[0].landed_ref, after);
    }

    #[test]
    fn post_rebase_reverify_failure_parks_divergent_base_with_grader_output() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        commit(
            &wt,
            "reverify_output.txt",
            "REVERIFY_GRADER_NONCE_C42",
            "probe",
        );
        commit(&repo, "peer.txt", "advanced", "advance base with peer.txt");
        let base_advanced = rev(&repo, "main");
        // Passes on the branch's original tree; the rebase brings peer.txt in.
        let grader = "cat reverify_output.txt; test ! -f peer.txt";
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &[grader]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_DIVERGENT_BASE)
        );
        assert_eq!(rev(&repo, "main"), base_advanced);
        assert!(wt.exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_DIVERGENT_BASE);
        assert_eq!(parks[0].receipts.len(), 1);
        assert!(!parks[0].receipts[0].passed);
        assert_eq!(parks[0].receipts[0].grader_name.as_deref(), Some(grader));
        assert!(
            parks[0].receipts[0]
                .output_excerpt
                .contains("REVERIFY_GRADER_NONCE_C42")
        );
        assert!(landed(&store).is_empty());
    }

    #[test]
    fn post_rebase_standing_verify_failure_parks_standing_verify() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store))
            .with_verify_command("cat check_output.txt; exit 1");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        commit(
            &wt,
            "check_output.txt",
            "STANDING_REBASE_NONCE_A17",
            "probe",
        );
        commit(&repo, "peer.txt", "advanced", "advance base");
        let base_advanced = rev(&repo, "main");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert_eq!(rev(&repo, "main"), base_advanced);
        assert!(!repo.join("feature.txt").exists());
        assert!(wt.exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_STANDING_VERIFY);
        assert!(
            parks[0].receipts[0]
                .output_excerpt
                .contains("STANDING_REBASE_NONCE_A17")
        );
    }

    // --- flowchart: merge-fallback rung --------------------------------------------------

    #[test]
    fn merge_fallback_lands_a_rebase_conflicting_branch() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let branch_head = merge_bearing_branch(&wt);
        commit(&repo, "file.txt", "line1-main\n", "base advances");
        let base_before = rev(&repo, "main");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        let lands = landed(&store);
        assert_eq!(lands.len(), 1);
        assert_eq!(lands[0].strategy, LANDING_STRATEGY_MERGE);
        assert_eq!(lands[0].rung, RUNG_MERGE_FALLBACK);
        assert_ne!(rev(&repo, "main"), base_before);
        assert!(is_ancestor(&repo, &branch_head, "main"));
        assert_eq!(lands[0].landed_ref, rev(&repo, "main"));
        assert_eq!(
            outcome,
            SubmitOutcome::landed(LANDING_STRATEGY_MERGE, rev(&repo, "main"))
        );
        assert!(parked(&store).is_empty());
        assert!(!wt.exists());
    }

    #[test]
    fn merge_fallback_lands_when_base_not_checked_out() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let branch_head = merge_bearing_branch(&wt);
        commit(&repo, "file.txt", "line1-main\n", "base advances");
        git_ok(&repo, &["checkout", "-q", "-b", "operator"]);
        let base_before = rev(&repo, "main");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert_eq!(landed(&store)[0].rung, RUNG_MERGE_FALLBACK);
        assert_ne!(rev(&repo, "main"), base_before);
        assert!(is_ancestor(&repo, &branch_head, "main"));
    }

    #[test]
    fn merge_fallback_grader_failure_on_merged_tree_parks_preserved() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let grader = "test ! -f poison.txt";
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", grader);
        let branch_head = merge_bearing_branch(&wt);
        fs::write(repo.join("file.txt"), "line1-main\n").unwrap();
        commit(&repo, "poison.txt", "boom\n", "base advances and poisons");
        let base_before = rev(&repo, "main");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &[grader]));
        assert_eq!(rev(&repo, "main"), base_before);
        assert!(landed(&store).is_empty());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_DIVERGENT_BASE);
        assert!(LANDING_PARK_KINDS.contains(&parks[0].park_kind.as_str()));
        assert!(parks[0].receipts.iter().any(|r| !r.passed));
        assert!(branch_exists(&repo, "flywheel/01-phase/t1"));
        assert_eq!(
            rev(&repo, "flywheel/01-phase/t1"),
            branch_head,
            "reset off the merge commit"
        );
        assert!(wt.is_dir());
    }

    #[test]
    fn merge_fallback_standing_verify_failure_resets_and_parks() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store)).with_verify_command("test ! -f poison.txt");
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let branch_head = merge_bearing_branch(&wt);
        fs::write(repo.join("file.txt"), "line1-main\n").unwrap();
        commit(&repo, "poison.txt", "boom\n", "base advances and poisons");
        let base_before = rev(&repo, "main");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert_eq!(rev(&repo, "main"), base_before);
        let parks = parked(&store);
        assert_eq!(parks[0].park_kind, PARK_KIND_STANDING_VERIFY);
        assert!(
            parks[0].detail.contains("against the merged tree"),
            "{}",
            parks[0].detail
        );
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
    }

    #[test]
    fn merge_fallback_held_out_gate_blocks_the_merged_tree() {
        let (_dir, repo) = repo();
        let store = ledger();
        let root = repo.parent().unwrap().join("held-out");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("t1.json"),
            r#"[{"type": "command", "run": "echo ORACLE_SAW_POISON; test ! -f poison.txt", "name": "oracle"}]"#,
        )
        .unwrap();
        let s = submitter(&repo, Arc::clone(&store))
            .with_held_out_source(Arc::new(FilesystemHeldOutGraderSource::new(&root)));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let branch_head = merge_bearing_branch(&wt);
        fs::write(repo.join("file.txt"), "line1-main\n").unwrap();
        commit(&repo, "poison.txt", "boom\n", "base advances and poisons");
        let base_before = rev(&repo, "main");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_HELD_OUT_GATE)
        );
        assert_eq!(rev(&repo, "main"), base_before);
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_HELD_OUT_GATE);
        assert_eq!(parks[0].receipts.len(), 1);
        assert_eq!(parks[0].receipts[0].grader_name.as_deref(), Some("oracle"));
        assert!(!parks[0].receipts[0].passed);
        assert!(
            parks[0].receipts[0]
                .output_excerpt
                .contains("ORACLE_SAW_POISON")
        );
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);

        // A passing oracle lets the merged tree land through the same rung.
        fs::write(
            root.join("t2.json"),
            r#"[{"type": "command", "run": "test -f file.txt"}]"#,
        )
        .unwrap();
        let (tf2, wt2) = prepared(&s, &repo, "01-phase", "t2", "true");
        // Edit-then-revert against the CURRENT base content so the net tree
        // merges cleanly while the rebase still conflicts.
        commit(&wt2, "file.txt", "line1-branch-2\n", "diverge file.txt");
        commit(&wt2, "file.txt", "line1-main\n", "revert file.txt");
        commit(&repo, "file.txt", "line1-main-2\n", "base advances again");
        let mut req = submit_req(&tf2, "t2", &wt2, Status::Done, &["true"]);
        req.task_id = "t2".into();
        assert!(s.submit(&req).is_landed());
    }

    #[test]
    fn merge_conflict_with_rung_disabled_parks_merge_conflict_cleanly() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        let branch_head = commit(&wt, "file.txt", "line1-branch\n", "edit file.txt");
        commit(&repo, "file.txt", "line1-main\n", "base advances");
        let base_before = rev(&repo, "main");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_MERGE_CONFLICT)
        );
        assert!(landed(&store).is_empty());
        assert_eq!(rev(&repo, "main"), base_before);
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_MERGE_CONFLICT);
        assert_eq!(parks[0].agent_turns, None);
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
        assert!(wt.is_dir());
        assert!(
            !git(&wt, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
                .unwrap()
                .ok()
        );
        assert_eq!(git_ok(&wt, &["status", "--porcelain"]), "");
    }

    // --- flowchart: agent-resolved rung --------------------------------------------------

    fn resolving_driver(
        turns: u32,
        wall: f64,
        calls: Arc<Mutex<Vec<ConflictResolutionRequest>>>,
    ) -> ConflictResolver {
        Arc::new(move |request| {
            calls.lock().unwrap().push(request.clone());
            assert!(
                git(
                    &request.worktree,
                    &["rev-parse", "-q", "--verify", "MERGE_HEAD"]
                )
                .unwrap()
                .ok()
            );
            fs::write(request.worktree.join("file.txt"), "line1-resolved\n").unwrap();
            git_ok(&request.worktree, &["add", "-A"]);
            Ok(ConflictResolutionReport {
                turns,
                wall_seconds: wall,
            })
        })
    }

    fn drive_to_conflict(
        s: &GitWorktreeSubmitter,
        repo: &Path,
        run: &str,
    ) -> (PathBuf, PathBuf, String, String) {
        let (tf, wt) = prepared(s, repo, "01-phase", "t1", run);
        let branch_head = commit(&wt, "file.txt", "line1-branch\n", "edit file.txt");
        commit(repo, "file.txt", "line1-main\n", "base advances");
        let base_before = rev(repo, "main");
        (tf, wt, branch_head, base_before)
    }

    #[test]
    fn agent_resolution_lands_a_merge_conflicting_branch_with_usage() {
        let (_dir, repo) = repo();
        let store = ledger();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let s = submitter(&repo, Arc::clone(&store))
            .with_recovery_agent(30, 900.0, Some("claude-test".into()))
            .with_resolve_conflict(resolving_driver(4, 2.0, Arc::clone(&calls)));
        let (tf, wt, branch_head, base_before) = drive_to_conflict(&s, &repo, "true");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].worktree, wt);
        assert_eq!(calls[0].max_turns, 30);
        assert_eq!(calls[0].max_wall_seconds, 900.0);
        assert_eq!(calls[0].model.as_deref(), Some("claude-test"));
        assert!(calls[0].prompt.contains("`flywheel/01-phase/t1`"));
        let lands = landed(&store);
        assert_eq!(lands.len(), 1);
        assert_eq!(lands[0].rung, RUNG_AGENT_RESOLVED);
        assert_eq!(lands[0].agent_turns, Some(4));
        assert_eq!(lands[0].agent_wall_seconds, Some(2.0));
        assert_ne!(rev(&repo, "main"), base_before);
        assert!(is_ancestor(&repo, &branch_head, "main"));
        assert_eq!(lands[0].landed_ref, rev(&repo, "main"));
        assert!(outcome.is_landed());
        assert!(parked(&store).is_empty());
        assert_eq!(
            fs::read_to_string(repo.join("file.txt")).unwrap(),
            "line1-resolved\n"
        );
    }

    #[test]
    fn zero_turns_disables_the_rung_and_no_session_runs() {
        let (_dir, repo) = repo();
        let store = ledger();
        let never: ConflictResolver =
            Arc::new(|_| panic!("resolver must not run when the rung is disabled"));
        let s = submitter(&repo, Arc::clone(&store))
            .with_recovery_agent(0, 900.0, None)
            .with_resolve_conflict(never);
        let (tf, wt, branch_head, base_before) = drive_to_conflict(&s, &repo, "true");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(landed(&store).is_empty());
        assert_eq!(rev(&repo, "main"), base_before);
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_MERGE_CONFLICT);
        assert_eq!(parks[0].agent_turns, None);
        assert_eq!(parks[0].agent_wall_seconds, None);
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
        assert_eq!(git_ok(&wt, &["status", "--porcelain"]), "");
    }

    #[test]
    fn armed_rung_without_a_wired_resolver_parks_merge_conflict() {
        let (_dir, repo) = repo();
        let store = ledger();
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let mut s = submitter(&repo, Arc::clone(&store)).with_recovery_agent(5, 10.0, None);
        s.log = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
        let (tf, wt, _head, base_before) = drive_to_conflict(&s, &repo, "true");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert_eq!(rev(&repo, "main"), base_before);
        assert_eq!(parked(&store)[0].park_kind, PARK_KIND_MERGE_CONFLICT);
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("no conflict resolver is wired"))
        );
    }

    #[test]
    fn bound_exhaustion_parks_preserved_with_usage() {
        let (_dir, repo) = repo();
        let store = ledger();
        let giveup: ConflictResolver = Arc::new(|_| {
            Ok(ConflictResolutionReport {
                turns: 30,
                wall_seconds: 900.0,
            })
        });
        let s = submitter(&repo, Arc::clone(&store))
            .with_recovery_agent(30, 900.0, None)
            .with_resolve_conflict(giveup);
        let (tf, wt, branch_head, base_before) = drive_to_conflict(&s, &repo, "true");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(landed(&store).is_empty());
        assert_eq!(rev(&repo, "main"), base_before);
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_MERGE_CONFLICT);
        assert_eq!(parks[0].agent_turns, Some(30));
        assert_eq!(parks[0].agent_wall_seconds, Some(900.0));
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
        assert!(wt.is_dir());
        assert_eq!(git_ok(&wt, &["status", "--porcelain"]), "");
    }

    #[test]
    fn resolution_that_edits_a_protected_path_parks_and_nothing_lands() {
        let (_dir, repo) = repo();
        let store = ledger();
        let tampering: ConflictResolver = Arc::new(|request| {
            fs::write(request.worktree.join("file.txt"), "line1-resolved\n").unwrap();
            fs::write(request.worktree.join("conftest.py"), "tampered\n").unwrap();
            git_ok(&request.worktree, &["add", "-A"]);
            Ok(ConflictResolutionReport {
                turns: 3,
                wall_seconds: 1.5,
            })
        });
        let s = submitter(&repo, Arc::clone(&store))
            .with_protected_paths(["conftest.py"])
            .with_recovery_agent(30, 900.0, None)
            .with_resolve_conflict(tampering);
        let (tf, wt, branch_head, base_before) = drive_to_conflict(&s, &repo, "true");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, detail }
            if park_kind == PARK_KIND_PROTECTED_PATHS
                && detail.contains("conftest.py")
                && detail.contains("was resolved by a bounded agent session"))
        );
        assert!(landed(&store).is_empty());
        assert_eq!(rev(&repo, "main"), base_before);
        assert!(!repo.join("conftest.py").exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_PROTECTED_PATHS);
        assert_eq!(parks[0].agent_turns, Some(3));
        assert_eq!(parks[0].agent_wall_seconds, Some(1.5));
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
        assert!(wt.is_dir());
        assert_eq!(git_ok(&wt, &["status", "--porcelain"]), "");
    }

    #[test]
    fn resolved_tree_failing_reverify_parks_with_receipts_and_usage() {
        let (_dir, repo) = repo();
        let store = ledger();
        let grader = "test ! -f poison.txt";
        let s = submitter(&repo, Arc::clone(&store))
            .with_recovery_agent(30, 900.0, None)
            .with_resolve_conflict(resolving_driver(5, 3.0, Arc::new(Mutex::new(Vec::new()))));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", grader);
        let branch_head = commit(&wt, "file.txt", "line1-branch\n", "edit file.txt");
        fs::write(repo.join("file.txt"), "line1-main\n").unwrap();
        commit(&repo, "poison.txt", "boom\n", "base advances and poisons");
        let base_before = rev(&repo, "main");
        s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &[grader]));
        assert_eq!(rev(&repo, "main"), base_before);
        assert!(landed(&store).is_empty());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_DIVERGENT_BASE);
        assert!(
            parks[0]
                .detail
                .contains("was resolved by a bounded agent session")
        );
        assert!(parks[0].receipts.iter().any(|r| !r.passed));
        assert_eq!(parks[0].agent_turns, Some(5));
        assert_eq!(parks[0].agent_wall_seconds, Some(3.0));
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
        assert!(wt.is_dir());
    }

    #[test]
    fn session_crash_parks_and_submit_does_not_fail() {
        let (_dir, repo) = repo();
        let store = ledger();
        let crashing: ConflictResolver =
            Arc::new(|_| Err(ConflictResolutionError("agent session crashed".into())));
        let s = submitter(&repo, Arc::clone(&store))
            .with_recovery_agent(30, 900.0, None)
            .with_resolve_conflict(crashing);
        let (tf, wt, branch_head, base_before) = drive_to_conflict(&s, &repo, "true");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, detail }
            if park_kind == PARK_KIND_MERGE_CONFLICT && detail.contains("agent session crashed"))
        );
        assert!(landed(&store).is_empty());
        assert_eq!(rev(&repo, "main"), base_before);
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].agent_turns, Some(0));
        assert!(parks[0].agent_wall_seconds.is_some());
        assert_eq!(rev(&repo, "flywheel/01-phase/t1"), branch_head);
        assert!(
            !git(&wt, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
                .unwrap()
                .ok()
        );
        assert_eq!(git_ok(&wt, &["status", "--porcelain"]), "");
    }

    // --- submit-error containment and the ledger seam -------------------------------------

    #[test]
    fn unexpected_failure_is_contained_as_a_submit_error_park() {
        let (_dir, repo) = repo();
        let store = ledger();
        let mut s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        // A lock path whose parent is a regular file cannot be created.
        let blocker = repo.join(".flywheel").join("blocker");
        fs::write(&blocker, "file").unwrap();
        s.lock_path = blocker.join(".merge.lock");
        let before = rev(&repo, "main");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, detail }
            if park_kind == PARK_KIND_SUBMIT_ERROR && detail.contains("MergeLockError"))
        );
        assert_eq!(rev(&repo, "main"), before);
        assert!(wt.exists());
        let parks = parked(&store);
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, PARK_KIND_SUBMIT_ERROR);
    }

    struct DecliningGuard(AtomicUsize);

    impl LedgerWriteGuard for DecliningGuard {
        fn guard(
            &self,
            _repo_root: &Path,
            _run_id: &str,
            _append: &mut dyn FnMut() -> Result<(), StoreError>,
        ) -> Result<(), LedgerWriteError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(LedgerWriteError::Declined("disk below threshold".into()))
        }
    }

    #[test]
    fn ledger_guard_can_decline_the_witness_without_changing_the_outcome() {
        let (_dir, repo) = repo();
        let store = ledger();
        let guard = Arc::new(DecliningGuard(AtomicUsize::new(0)));
        let s = submitter(&repo, Arc::clone(&store)).with_ledger_guard(guard.clone());
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(outcome.is_landed());
        assert_eq!(guard.0.load(Ordering::SeqCst), 1);
        assert!(
            landed(&store).is_empty(),
            "the declined append was never attempted"
        );
    }

    #[test]
    fn missing_store_or_lifecycle_degrades_to_log_only() {
        let (_dir, repo) = repo();
        let store = Arc::new(InMemoryStore::new());
        let mut s = submitter(&repo, Arc::clone(&store));
        let (tf, wt) = prepared(&s, &repo, "01-phase", "t1", "true");
        commit(&wt, "feature.txt", "x", "work");
        fs::write(wt.join("stray"), "x").unwrap();
        // No lifecycle for run-1: the park still happens, nothing is appended.
        assert!(
            s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]))
                .is_parked()
        );
        assert!(store.list_domain_events("run-1").unwrap().is_empty());
        s.store = None;
        assert!(
            s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]))
                .is_parked()
        );
        assert!(wt.exists());
    }

    #[test]
    fn reverify_recorder_lists_by_run_and_attempt() {
        let recorder = ReverifyRecorder::new();
        let mut record = GraderResultRecord {
            run_id: "run-1".into(),
            attempt_number: 0,
            ordinal: 0,
            grader_type: flywheel_core::task::GraderType::Command,
            grader_spec: JsonObject::new(),
            passed: true,
            duration_ms: 1,
            payload: JsonObject::new(),
            ts: Utc::now(),
            grader_name: None,
            id: None,
        };
        let stored = recorder.append_grader_result(&record).unwrap();
        assert_eq!(stored.id, Some(1));
        record.attempt_number = 1;
        recorder.append_grader_result(&record).unwrap();
        assert_eq!(recorder.list_grader_results("run-1", 0).unwrap().len(), 1);
        assert_eq!(recorder.list_grader_results("run-1", 1).unwrap().len(), 1);
        assert_eq!(recorder.list_grader_results("other", 0).unwrap().len(), 0);
        assert_eq!(recorder.records().len(), 2);
    }
}
