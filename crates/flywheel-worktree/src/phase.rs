//! Phase bookkeeping: the per-phase integration-branch landing strategy
//! (spec 00079), the `refs/flywheel/loop-base/<phase>` base-capture refs,
//! and the archive sweep that moves a completed `active/<phase>` to
//! `archive/` once every exit gate passes.
//!
//! Port of the phase half of the Python `flywheel_worktree.worker` module
//! (`PhaseBranchSubmitter`, `record_phase_bases`, `archive_phases`) together
//! with the archive machinery it drove in
//! `flywheel_orchestrator._workflow` (`archive_completed_phases`, the
//! loop-base ref helpers, and the `loop-path-exempt.md` opt-out parser),
//! which the Rust orchestrator does not yet ship.
//!
//! Invariants:
//!
//! * The worker never commits to the operator's branch. Base capture is pure
//!   ref plumbing; the only commit the sweep authors is the archival move of
//!   the queue files themselves (bookkeeping, never task work), scoped by
//!   pathspec to the two moved directories.
//! * A phase integration branch is created only at an actual fast-forward,
//!   never on a park, and the true base is never advanced by the phase
//!   strategy.
//! * The archive sweep never silently skips: every refusal (an unlanded
//!   task, an unmerged phase PR, a loop-path signal without its gate, a
//!   failing phase-verify, a same-named archive already present) is
//!   reported through the log seam.

use crate::submitter::{
    GitError, GitWorktreeSubmitter, LandingBase, Logger, checked_out_branch, git, git_checked,
    merge_lock,
};
use chrono::{DateTime, Utc};
use flywheel_core::events::{DomainEventPayload, LANDING_STRAND_KINDS};
use flywheel_core::lifecycle::Status;
use flywheel_core::loaders::{TaskLoadError, load_task_file};
use flywheel_core::loop_path_marker::{LoopPathSignal, detect_loop_path_signals};
use flywheel_core::store::{DomainEventStore, LifecycleFilter, LifecycleStore, StoreError};
use flywheel_core::task::Task;
use flywheel_orchestrator::claims::{
    ClaimStore, ClaimStoreError, RESOLUTION_ATTRIBUTION_OPERATOR, RESOLUTION_ATTRIBUTION_PROBE,
    STOP_INDETERMINATE_LANDING, STOP_RESOLVED,
};
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::sources::{WorkSourceError, iter_active_phase_dirs, phase_task_files};
use flywheel_orchestrator::strategy::{
    LandabilityVerdict, PrepareError, SandboxHandle, SandboxRequest, SubmitOutcome, SubmitRequest,
    SubmitStrategy,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

/// The tag a DONE task carries to satisfy the loop-path archive gate.
pub const IN_LOOP_VERIFICATION_TAG: &str = "in-loop-verification";

/// The legacy committed dotfile recording an archived phase's base SHA.
pub const LOOP_BASE_FILENAME: &str = ".loop-base";

/// Active phases record their base SHA under this ref namespace rather than
/// as a committed file: the worker must never create commits on the
/// operator's branch. Archiving materializes the ref into the phase dir as
/// the legacy dotfile so audits keep one contract.
pub const LOOP_BASE_REF_PREFIX: &str = "refs/flywheel/loop-base/";

/// The opt-out artifact a phase carries to clear the loop-path gate.
pub const LOOP_PATH_OPTOUT_FILENAME: &str = "loop-path-exempt.md";

const LOOP_PATH_OPTOUT_REQUIRED_KEYS: [&str; 3] = ["phase", "author", "reason"];

// --- the phase-branch landing strategy ---------------------------------------------------

/// The integration branch a phase lands its tasks onto.
pub fn phase_branch(phase: &str) -> String {
    format!("flywheel/phase/{phase}")
}

/// The phase strategy's [`LandingBase`]: the phase's integration branch
/// once it exists (so tasks stack and measure only their own commits), else
/// the true base -- the branch point for the phase's first task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhaseIntegrationBase;

impl LandingBase for PhaseIntegrationBase {
    fn resolve(&self, submitter: &GitWorktreeSubmitter, phase: &str) -> Result<String, GitError> {
        let integration = phase_branch(phase);
        if submitter.branch_exists(&integration)? {
            return Ok(integration);
        }
        Ok(submitter.phase_base.clone())
    }

    /// Called only at an actual fast-forward (never on a park), so the first
    /// passing task in a phase forks `flywheel/phase/<phase>` from the
    /// then-current true base. The true base is never advanced -- only the
    /// integration ref is (criterion 1). Idempotent once the branch exists.
    fn materialize(
        &self,
        submitter: &GitWorktreeSubmitter,
        phase: &str,
        base: &str,
    ) -> Result<String, GitError> {
        let integration = phase_branch(phase);
        if !submitter.branch_exists(&integration)? {
            git_checked(&submitter.repo_root, &["branch", &integration, base])?;
            (submitter.log)(&format!(
                "created phase integration branch {integration} at {base}"
            ));
        }
        Ok(integration)
    }
}

/// Land each task onto a per-phase integration branch (spec 00079).
///
/// The merge strategy's identical provisioning and full verify ladder, with
/// one behavioral change: the ref a passing task fast-forwards onto is the
/// phase's integration branch `flywheel/phase/<phase>`, never the single
/// configured base. The true base is never advanced; the integration branch
/// is a separate ref advanced out-of-tree (it is never checked out), so the
/// operator's checkout stays untouched. Tasks in the same phase stack
/// because provisioning branches a fresh worktree off the integration branch
/// once it exists.
#[derive(Debug, Clone)]
pub struct PhaseBranchSubmitter {
    inner: GitWorktreeSubmitter,
}

impl PhaseBranchSubmitter {
    /// Wrap `inner`, pointing its landing-base seam at the phase branch.
    pub fn new(inner: GitWorktreeSubmitter) -> Self {
        Self {
            inner: inner.with_landing_base(Arc::new(PhaseIntegrationBase)),
        }
    }

    pub fn inner(&self) -> &GitWorktreeSubmitter {
        &self.inner
    }
}

impl std::ops::Deref for PhaseBranchSubmitter {
    type Target = GitWorktreeSubmitter;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl SubmitStrategy for PhaseBranchSubmitter {
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
        self.inner.prepare_sandbox(request)
    }

    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome {
        self.inner.submit(request)
    }

    fn is_landable(&self, request: &SubmitRequest) -> LandabilityVerdict {
        self.inner.is_landable(request)
    }
}

/// Build the phase backend (the registry's `phase` target, spec 00079):
/// the merge builder's exact configuration (same `[submit] verify`,
/// recovery-agent bounds, and model), only the landing ref differs. Logs the
/// resolved landing target so an operator can see the strategy that armed.
pub fn build_phase_submitter(
    request: &crate::submit_registry::SubmitBuildRequest,
) -> PhaseBranchSubmitter {
    let submitter = PhaseBranchSubmitter::new(crate::submitter::build_merge_submitter(request));
    (request.log)(&format!(
        "landing strategy: phase (integration branch flywheel/phase/<phase> per task's phase; \
         true base {} never advanced)",
        request.phase_base
    ));
    submitter
}

// --- loop-base refs ----------------------------------------------------------------------

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The git ref recording `phase_dir`'s base SHA while it is active. Keyed by
/// directory name. A recurring name (autopilot's `active/autopilot`) reuses
/// the ref across generations safely: archiving materializes it into the
/// archived dir and drops it, so the next generation records a fresh base.
pub fn loop_base_ref(phase_dir: &Path) -> String {
    format!("{LOOP_BASE_REF_PREFIX}{}", dir_name(phase_dir))
}

/// `git -C <repo_root> <args>` as `(ok, stdout)`; stderr is discarded since
/// failure here is data (the diff degrades to empty), never an error.
fn git_capture(repo_root: &Path, args: &[&str]) -> (bool, String) {
    match git(repo_root, args) {
        Ok(out) => (out.ok(), out.stdout),
        Err(_) => (false, String::new()),
    }
}

/// The SHA recorded as `phase_dir`'s base, or `None`.
///
/// The legacy committed `phase_dir/.loop-base` dotfile wins when present
/// (pre-ref phases, and every archived phase). Otherwise, when `repo_root`
/// is supplied, the `refs/flywheel/loop-base/<phase>` ref is consulted.
/// Missing, empty, or unreadable sources all map to `None`.
pub fn read_phase_base(phase_dir: &Path, repo_root: Option<&Path>) -> Option<String> {
    let base_file = phase_dir.join(LOOP_BASE_FILENAME);
    if base_file.is_file() {
        let sha = fs::read_to_string(&base_file)
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        if !sha.is_empty() {
            return Some(sha);
        }
    }
    let repo_root = repo_root?;
    let (ok, out) = git_capture(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &loop_base_ref(phase_dir),
        ],
    );
    if !ok {
        return None;
    }
    let sha = out.trim();
    (!sha.is_empty()).then(|| sha.to_owned())
}

/// Record the current `HEAD` SHA as `phase_dir`'s base if absent, as the
/// `refs/flywheel/loop-base/<phase>` ref -- pure ref plumbing, no
/// working-tree write and nothing for anyone to commit. Returns `true` when
/// a fresh base was recorded, `false` when one already existed (the
/// first-seen SHA is the true base; a re-run must never move it forward),
/// when `phase_dir` does not exist, or when `HEAD` cannot be resolved.
pub fn write_phase_base_if_missing(repo_root: &Path, phase_dir: &Path) -> bool {
    if !phase_dir.is_dir() {
        return false;
    }
    if read_phase_base(phase_dir, Some(repo_root)).is_some() {
        return false;
    }
    let (ok, out) = git_capture(repo_root, &["rev-parse", "HEAD"]);
    if !ok {
        return false;
    }
    let sha = out.trim();
    if sha.is_empty() {
        return false;
    }
    git_capture(repo_root, &["update-ref", &loop_base_ref(phase_dir), sha]).0
}

/// Carry the archived `phase_name`'s loop-base ref into `dest` as the
/// legacy `.loop-base` dotfile, then drop the ref (so a recurring phase name
/// records a fresh base for its next generation). `dest` may be a
/// generation-suffixed `archive/<phase>-<n>`, so the ref is keyed by the
/// active phase name, not `dest`. A phase that already carries a committed
/// dotfile is left as-is.
fn materialize_loop_base(repo_root: &Path, phase_name: &str, dest: &Path) {
    if dest.join(LOOP_BASE_FILENAME).is_file() {
        return;
    }
    let reference = format!("{LOOP_BASE_REF_PREFIX}{phase_name}");
    let (ok, out) = git_capture(repo_root, &["rev-parse", "--verify", "--quiet", &reference]);
    if !ok {
        return;
    }
    let sha = out.trim();
    if !sha.is_empty() {
        // A dotfile that cannot be written leaves the ref in place so the
        // next sweep (or an operator) can retry; the ref is dropped only
        // once the file carries the SHA.
        if fs::write(dest.join(LOOP_BASE_FILENAME), format!("{sha}\n")).is_err() {
            return;
        }
    }
    git_capture(repo_root, &["update-ref", "-d", &reference]);
}

/// `git diff <recorded-base> HEAD` for `repo_root` as text; empty when no
/// base has been recorded for the phase or the diff fails (e.g. the
/// recorded SHA has been garbage-collected).
pub fn phase_diff_vs_base(repo_root: &Path, phase_dir: &Path) -> String {
    let Some(base) = read_phase_base(phase_dir, Some(repo_root)) else {
        return String::new();
    };
    let (ok, out) = git_capture(repo_root, &["diff", &base, "HEAD"]);
    if ok { out } else { String::new() }
}

/// Capture each active phase's base SHA as a `refs/flywheel/loop-base` ref.
///
/// Runs once per cycle, before any task branch is merged. Idempotent: a
/// phase whose base is already recorded is left untouched. Held under the
/// merge lock so two workers do not race the check-and-set. Returns the
/// number of phases whose base was freshly recorded.
pub fn record_phase_bases(
    repo_root: &Path,
    tasks_dir: &Path,
    lock_path: &Path,
    log: &Logger,
) -> Result<usize, ArchiveError> {
    if !tasks_dir.join("active").is_dir() {
        return Ok(0);
    }
    let _lock = merge_lock(lock_path).map_err(|source| ArchiveError::Io {
        path: lock_path.to_path_buf(),
        source,
    })?;
    let mut recorded = 0;
    for phase_dir in iter_active_phase_dirs(tasks_dir)? {
        if write_phase_base_if_missing(repo_root, &phase_dir) {
            recorded += 1;
        }
    }
    if recorded > 0 {
        log(&format!("Recorded base sha for {recorded} phase(s)"));
    }
    Ok(recorded)
}

// --- the loop-path opt-out artifact ------------------------------------------------------------

/// An opt-out artifact exists but its front-matter is invalid. The message
/// identifies the offending file. Absent files are NOT an error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LoopPathOptOutError(pub String);

/// One parsed `loop-path-exempt.md` artifact: who opted which phase out,
/// and why. All three fields are required and non-empty -- a silently-empty
/// opt-out is rejected at load time so the audit re-check has something
/// falsifiable to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopPathOptOut {
    pub phase: String,
    pub author: String,
    pub reason: String,
}

/// Locate and parse the opt-out artifact for `phase_dir`. `Ok(None)` when
/// no artifact exists (the common case); `Err` when one exists but its
/// front-matter is missing, malformed, or lacks a required key.
pub fn load_loop_path_optout(
    phase_dir: &Path,
) -> Result<Option<LoopPathOptOut>, LoopPathOptOutError> {
    let artifact = phase_dir.join(LOOP_PATH_OPTOUT_FILENAME);
    if !artifact.is_file() {
        return Ok(None);
    }
    let source = artifact.display().to_string();
    let text = fs::read_to_string(&artifact)
        .map_err(|err| LoopPathOptOutError(format!("{source}: cannot read opt-out: {err}")))?;
    let fields = parse_optout_frontmatter(&text, &source)?;
    let missing: Vec<&str> = LOOP_PATH_OPTOUT_REQUIRED_KEYS
        .iter()
        .copied()
        .filter(|key| fields.get(*key).is_none_or(String::is_empty))
        .collect();
    if !missing.is_empty() {
        return Err(LoopPathOptOutError(format!(
            "{source}: opt-out front-matter is missing required key(s): {}",
            missing.join(", ")
        )));
    }
    Ok(Some(LoopPathOptOut {
        phase: fields["phase"].clone(),
        author: fields["author"].clone(),
        reason: fields["reason"].clone(),
    }))
}

/// Parse a leading `---` ... `---` block into `key: value` fields. Blank
/// lines and `#` comments inside the block are skipped; anything after the
/// closing `---` is prose and ignored. Unknown keys are tolerated.
fn parse_optout_frontmatter(
    text: &str,
    source: &str,
) -> Result<BTreeMap<String, String>, LoopPathOptOutError> {
    let mut lines = text.lines();
    if lines.next().is_none_or(|first| first.trim() != "---") {
        return Err(LoopPathOptOutError(format!(
            "{source}: opt-out must start with a '---' front-matter delimiter"
        )));
    }
    let mut fields = BTreeMap::new();
    let mut closed = false;
    for raw in lines {
        if raw.trim() == "---" {
            closed = true;
            break;
        }
        let stripped = raw.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let Some((key, value)) = raw.split_once(':') else {
            return Err(LoopPathOptOutError(format!(
                "{source}: malformed front-matter line (expected 'key: value'): {raw:?}"
            )));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(LoopPathOptOutError(format!(
                "{source}: malformed front-matter line (empty key): {raw:?}"
            )));
        }
        fields.insert(key.to_owned(), value.trim().to_owned());
    }
    if !closed {
        return Err(LoopPathOptOutError(format!(
            "{source}: opt-out front-matter is not closed by a matching '---' line"
        )));
    }
    Ok(fields)
}

// --- the archive sweep --------------------------------------------------------------------------

/// The archive sweep's per-task landing verdict (spec 00077, D-1/D-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandingState {
    /// A `Landed` receipt exists on the task's latest run, or the ancestry
    /// / patch-identity probe confirmed its recorded work reachable from
    /// the landing base.
    Landed,
    /// The work resolved to a commit that is NOT reachable (a determinate
    /// strand).
    NotLanded,
    /// No receipt and no resolvable head: landing state cannot be
    /// determined, so it fails closed.
    Indeterminate,
}

/// The archive sweep's phase-branch merge verdict (spec 00079, D-2/D-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseMergeState {
    /// The integration branch tip is an ancestor of the true base: the
    /// phase PR merged as a merge commit.
    Merged,
    /// The branch exists, is not an ancestor, and carries changes the true
    /// base lacks (an open phase PR).
    Unmerged,
    /// The branch's content is already applied to the true base but its tip
    /// is not an ancestor: a squash/rebase merge discarded the commit.
    MergeMethodMismatch,
}

/// The store surface the sweep reads: DONE-ness and the latest run by task
/// id, plus the run's domain events for the `Landed` receipt fast path.
pub trait ArchiveStore: LifecycleStore + DomainEventStore {}

impl<T: LifecycleStore + DomainEventStore + ?Sized> ArchiveStore for T {}

/// The phase-branch-landing completion seam (spec 00079, criterion 3):
/// handed `(phase_dir, loaded_tasks)` for a completed, landed phase whose
/// integration branch has not merged into the true base.
pub type PhaseCompletion<'a> = &'a (dyn Fn(&Path, &[Task]) + 'a);

/// Everything beyond the tasks dir and store that arms an archive gate.
/// Every field defaults to unset, which preserves the legacy DONE-only
/// archival contract.
#[derive(Clone, Copy, Default)]
pub struct ArchiveOptions<'a> {
    /// Enables the loop-path gate, the landed predicate, the phase-merge
    /// predicate, the phase-verify gate, and the archival-move commit.
    pub repo_root: Option<&'a Path>,
    pub log: Option<&'a Logger>,
    /// The phase-exit integration gate (spec 00035): a shell command run in
    /// `repo_root` once a phase is eligible; non-zero leaves it active.
    pub phase_verify: Option<&'a str>,
    /// Arms the per-task landed predicate (spec 00077) with `repo_root`.
    pub landing_base: Option<&'a str>,
    /// Arms the phase-branch merge predicate (spec 00079) with `repo_root`.
    pub true_base: Option<&'a str>,
    /// The stop-event ledger: stamps `stop-resolved` markers on archive and
    /// surfaces indeterminate-landing strands.
    pub claims: Option<&'a dyn ClaimStore>,
    pub phase_completion: Option<PhaseCompletion<'a>>,
    /// The marker clock; wall-clock UTC when unset.
    pub now: Option<&'a (dyn Fn() -> DateTime<Utc> + 'a)>,
}

impl std::fmt::Debug for ArchiveOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveOptions")
            .field("repo_root", &self.repo_root)
            .field("phase_verify", &self.phase_verify)
            .field("landing_base", &self.landing_base)
            .field("true_base", &self.true_base)
            .field("claims", &self.claims.is_some())
            .field("phase_completion", &self.phase_completion.is_some())
            .finish_non_exhaustive()
    }
}

/// What stops an archive sweep outright (a refusal is a logged skip, not an
/// error).
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Source(#[from] WorkSourceError),
    #[error(transparent)]
    TaskLoad(#[from] TaskLoadError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Claims(#[from] ClaimStoreError),
}

/// Move `active/<phase>` dirs to `archive/` when every task is done and
/// every armed exit gate passes. Returns the moved phase directories
/// (post-move paths). Idempotent. Gates run in this order per phase:
///
/// 1. every task has a DONE lifecycle;
/// 2. the landed predicate (`repo_root` + `landing_base`): each task has a
///    `Landed` receipt on its latest run, or its `flywheel/<phase>/<task>`
///    head is an ancestor of (or patch-absorbed into) `landing_base`; an
///    operator-resolved strand is skipped; an indeterminate one is surfaced
///    through `claims` once;
/// 3. the phase-branch merge predicate (`repo_root` + `true_base`): when
///    `flywheel/phase/<phase>` exists its tip must be an ancestor of the
///    true base, else the `phase_completion` seam fires (or the refusal is
///    logged);
/// 4. the loop-path gate (`repo_root`): a diff vs the recorded base that
///    hits a watched signal needs a DONE `in-loop-verification` task or a
///    valid `loop-path-exempt.md`;
/// 5. the phase-verify gate (`repo_root` + `phase_verify`);
/// 6. the destination: `archive/<phase>`, or for a recurring phase name
///    (autopilot's fixed `active/autopilot`) the next free
///    `archive/<phase>-<n>`; a phase sharing a task with an archived
///    generation is a resurrected duplicate and is refused loudly (never a
///    silent skip: that is how resurrected duplicate phases hid), as is an
///    `archive/<phase>` that holds no task file.
///
/// After the move the loop-base ref is materialized into the archived dir,
/// the move is committed when the queue was git-tracked, and `claims`
/// stop rows are superseded with `stop-resolved` markers (landing strands
/// only when this sweep's own probe confirmed the land).
pub fn archive_completed_phases(
    tasks_dir: &Path,
    store: &dyn ArchiveStore,
    options: &ArchiveOptions<'_>,
) -> Result<Vec<PathBuf>, ArchiveError> {
    let mut moved = Vec::new();
    let archive_root = tasks_dir.join("archive");
    fs::create_dir_all(&archive_root).map_err(|source| ArchiveError::Io {
        path: archive_root.clone(),
        source,
    })?;
    let clock = || options.now.map_or_else(Utc::now, |now| now());
    let log = |message: String| {
        if let Some(log) = options.log {
            log(&message);
        }
    };

    for phase_dir in iter_active_phase_dirs(tasks_dir)? {
        let task_files = phase_task_files(&phase_dir)?;
        if task_files.is_empty() {
            continue;
        }
        let mut loaded_tasks = Vec::with_capacity(task_files.len());
        for path in &task_files {
            loaded_tasks.push(load_task_file(path)?);
        }
        let done = done_task_ids(store, &loaded_tasks)?;
        if !loaded_tasks.iter().all(|task| done.contains(&task.id)) {
            continue;
        }
        let phase_name = dir_name(&phase_dir);

        // Landed predicate (spec 00077, D-1/D-4).
        let mut landing_states: BTreeMap<String, LandingState> = BTreeMap::new();
        if let (Some(repo_root), Some(landing_base)) = (options.repo_root, options.landing_base) {
            let mut blocking = false;
            for task in &loaded_tasks {
                if let Some(claims) = options.claims
                    && has_operator_resolution(claims, &task.id)?
                {
                    continue;
                }
                let state =
                    probe_task_landing(store, repo_root, &phase_dir, &task.id, landing_base)?;
                landing_states.insert(task.id.clone(), state);
                if state == LandingState::Landed {
                    continue;
                }
                blocking = true;
                log(format_landing_refusal(&phase_name, &task.id, state));
                if state == LandingState::Indeterminate
                    && let Some(claims) = options.claims
                {
                    record_indeterminate_landing(claims, &task.id, &phase_name, clock())?;
                }
            }
            if blocking {
                continue;
            }
        }

        // Phase-branch merge predicate (spec 00079, criteria 4/5/6/8).
        if let (Some(repo_root), Some(true_base)) = (options.repo_root, options.true_base) {
            let branch = phase_branch(&phase_name);
            if let Some(tip) = resolve_ref(repo_root, &branch) {
                let merge_state = probe_phase_merge(repo_root, &tip, true_base);
                if merge_state != PhaseMergeState::Merged {
                    match options.phase_completion {
                        Some(completion) => completion(&phase_dir, &loaded_tasks),
                        None => log(format_phase_merge_refusal(
                            &phase_name,
                            &branch,
                            merge_state,
                        )),
                    }
                    continue;
                }
            }
        }

        // Loop-path archive gate (FR-2).
        if let Some(repo_root) = options.repo_root {
            let signals = detect_loop_path_signals(&phase_diff_vs_base(repo_root, &phase_dir));
            if !signals.is_empty() && !loop_path_gate_satisfied(&phase_dir, &loaded_tasks, store)? {
                log(format_gate_refusal(&phase_name, &signals));
                continue;
            }
        }

        // Phase-exit integration gate (spec 00035).
        if let (Some(repo_root), Some(command)) = (options.repo_root, options.phase_verify) {
            let (code, tail) = run_phase_verify(command, repo_root);
            if code != Some(0) {
                log(format_phase_verify_refusal(
                    &phase_name,
                    command,
                    code,
                    &tail,
                ));
                continue;
            }
        }

        let dest =
            match archive_destination(&archive_root, &phase_name, &task_files, &loaded_tasks)? {
                Ok(dest) => dest,
                Err(refusal) => {
                    log(refusal);
                    continue;
                }
            };
        fs::rename(&phase_dir, &dest).map_err(|source| ArchiveError::Io {
            path: phase_dir.clone(),
            source,
        })?;
        if let Some(repo_root) = options.repo_root {
            materialize_loop_base(repo_root, &phase_name, &dest);
            commit_archive_move(repo_root, &phase_dir, &dest, &log);
        }
        if let Some(claims) = options.claims {
            resolve_surfaced_stops(
                claims,
                &loaded_tasks,
                &phase_name,
                &landing_states,
                options.landing_base,
                &clock,
            )?;
        }
        moved.push(dest);
    }
    Ok(moved)
}

/// Task ids an archived phase dir defines: each task file's stem and its
/// `id` field (the `archive/*/<id>.json` rule the prerequisite resolver and
/// autopilot's emission dedupe use).
fn archived_task_ids(dir: &Path) -> Result<BTreeSet<String>, ArchiveError> {
    let mut ids = BTreeSet::new();
    for path in phase_task_files(dir)? {
        if let Some(stem) = path.file_stem() {
            ids.insert(stem.to_string_lossy().into_owned());
        }
        if let Some(id) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|data| data.get("id").and_then(|id| id.as_str().map(str::to_owned)))
        {
            ids.insert(id);
        }
    }
    Ok(ids)
}

/// Where the done phase `phase_name` archives, or the refusal to log.
///
/// `archive/<phase>` when free. A phase name can recur -- autopilot always
/// emits into `active/autopilot` -- so when earlier generations already
/// occupy `archive/<phase>` (and `archive/<phase>-<n>`), the phase archives
/// to the next free `archive/<phase>-<n>` (`n >= 2`). Refused instead when
/// the active phase shares a task (file stem or id) with any archived
/// generation -- an archival whose `active/` deletion was never committed
/// resurrects exactly those files on the next checkout -- or when
/// `archive/<phase>` holds no task file, so it cannot be told apart from
/// such a leftover.
fn archive_destination(
    archive_root: &Path,
    phase_name: &str,
    task_files: &[PathBuf],
    tasks: &[Task],
) -> Result<Result<PathBuf, String>, ArchiveError> {
    let base = archive_root.join(phase_name);
    if !base.exists() {
        return Ok(Ok(base));
    }
    let resurrected = |dest: &Path| {
        format!(
            "Refusing to archive phase {phase_name}: {} already exists; resolve the duplicate \
             by hand (an earlier archive whose active/ deletion was never committed \
             resurrects on the next checkout)",
            dest.display()
        )
    };
    let active_ids: BTreeSet<String> = tasks
        .iter()
        .map(|task| task.id.clone())
        .chain(task_files.iter().filter_map(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        }))
        .collect();
    let mut generations = vec![base.clone()];
    let prefix = format!("{phase_name}-");
    for entry in fs::read_dir(archive_root).map_err(|source| ArchiveError::Io {
        path: archive_root.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| ArchiveError::Io {
            path: archive_root.to_path_buf(),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_generation = name
            .strip_prefix(&prefix)
            .is_some_and(|n| n.parse::<u32>().is_ok_and(|n| n >= 2) && !n.starts_with('0'));
        if is_generation && entry.path().is_dir() {
            generations.push(entry.path());
        }
    }
    for generation in &generations {
        let archived = archived_task_ids(generation)?;
        if generation == &base && archived.is_empty() {
            return Ok(Err(resurrected(&base)));
        }
        if let Some(shared) = active_ids.intersection(&archived).next() {
            return Ok(Err(format!(
                "Refusing to archive phase {phase_name}: task {shared} is already archived under \
                 {}; resolve the duplicate by hand (an earlier archive whose active/ deletion \
                 was never committed resurrects on the next checkout)",
                generation.display()
            )));
        }
    }
    let mut n: u32 = 2;
    loop {
        let dest = archive_root.join(format!("{phase_name}-{n}"));
        if !dest.exists() {
            return Ok(Ok(dest));
        }
        n += 1;
    }
}

/// The worker-side archive sweep: resolves the phase-merge predicate's true
/// base from the policy (`[submit] base`, else the operator's checked-out
/// branch), suppresses the repo-root phase-verify gate when a phase
/// publisher owns it (pure phase strategy, no tiers), runs
/// [`archive_completed_phases`], and announces each archived phase.
pub fn archive_phases(
    tasks_dir: &Path,
    store: &dyn ArchiveStore,
    claims: Option<&dyn ClaimStore>,
    log: &Logger,
    sweep: &ArchiveSweep<'_>,
) -> Result<Vec<PathBuf>, ArchiveError> {
    let resolved_true_base: Option<String> = sweep
        .true_base
        .map(str::to_owned)
        .or_else(|| sweep.policy.and_then(|p| p.submit_base.clone()))
        .or_else(|| {
            sweep
                .repo_root
                .and_then(|root| checked_out_branch(root).ok().flatten())
        });
    // Under the pure phase strategy every landing reaches the phase branch,
    // so the publisher's phase-branch-tree gate covers every archival and the
    // repo-root gate would judge the wrong tree. Under tier routing a phase
    // may archive without ever growing an integration branch, so the
    // repo-root gate stays armed.
    let publisher_owns_phase_verify = sweep.phase_completion.is_some()
        && sweep
            .policy
            .is_some_and(|p| p.submit_strategy == "phase" && p.submit_tiers.is_empty());
    let phase_verify = if publisher_owns_phase_verify {
        None
    } else {
        sweep.policy.and_then(|p| p.phase_verify.as_deref())
    };
    let options = ArchiveOptions {
        repo_root: sweep.repo_root,
        log: Some(log),
        phase_verify,
        landing_base: sweep.landing_base,
        true_base: resolved_true_base.as_deref(),
        claims,
        phase_completion: sweep.phase_completion,
        now: None,
    };
    let moved = archive_completed_phases(tasks_dir, store, &options)?;
    for dest in &moved {
        log(&format!("Archived phase: {}", dest.display()));
    }
    Ok(moved)
}

/// The worker-side inputs to [`archive_phases`].
#[derive(Clone, Copy, Default)]
pub struct ArchiveSweep<'a> {
    pub repo_root: Option<&'a Path>,
    /// The worker's resolved submit base (arms the landed predicate).
    pub landing_base: Option<&'a str>,
    /// Overrides the policy/checkout-derived true base.
    pub true_base: Option<&'a str>,
    pub policy: Option<&'a WorkPolicy>,
    /// The phase PR publisher under the phase strategy / tier routing.
    pub phase_completion: Option<PhaseCompletion<'a>>,
}

impl std::fmt::Debug for ArchiveSweep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveSweep")
            .field("repo_root", &self.repo_root)
            .field("landing_base", &self.landing_base)
            .field("true_base", &self.true_base)
            .field("policy", &self.policy.is_some())
            .field("phase_completion", &self.phase_completion.is_some())
            .finish()
    }
}

// --- sweep helpers ------------------------------------------------------------------------------

fn has_done_lifecycle(store: &dyn ArchiveStore, task_id: &str) -> Result<bool, StoreError> {
    let filter = LifecycleFilter::default()
        .with_statuses([Status::Done])
        .with_task_id(task_id);
    Ok(!store.list_lifecycles(&filter)?.is_empty())
}

/// The subset of `tasks` holding a DONE lifecycle, in one store read rather
/// than one per task.
fn done_task_ids(store: &dyn ArchiveStore, tasks: &[Task]) -> Result<HashSet<String>, StoreError> {
    let filter = LifecycleFilter::default()
        .with_statuses([Status::Done])
        .with_task_ids(tasks.iter().map(|task| task.id.as_str()));
    Ok(store
        .list_lifecycles(&filter)?
        .into_iter()
        .map(|lc| lc.task_id)
        .collect())
}

/// The most-recently-updated lifecycle's run id for `task_id`.
fn latest_run_id(store: &dyn ArchiveStore, task_id: &str) -> Result<Option<String>, StoreError> {
    let filter = LifecycleFilter::default().with_task_id(task_id);
    Ok(store
        .list_lifecycles(&filter)?
        .into_iter()
        .next()
        .map(|lc| lc.run_id))
}

fn run_has_landed_receipt(store: &dyn ArchiveStore, run_id: &str) -> Result<bool, StoreError> {
    Ok(store
        .list_domain_events(run_id)?
        .iter()
        .any(|event| matches!(event.payload, DomainEventPayload::Landed(_))))
}

fn resolve_ref(repo_root: &Path, branch: &str) -> Option<String> {
    let (ok, out) = git_capture(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    );
    if !ok {
        return None;
    }
    let sha = out.trim();
    (!sha.is_empty()).then(|| sha.to_owned())
}

/// `git merge-base --is-ancestor`: exit 0 for ancestor; 1 (not) and 128
/// (unresolvable) both read as "not an ancestor" so an unresolvable landing
/// base fails closed rather than blessing the work as landed.
fn is_ancestor(repo_root: &Path, ancestor: &str, rev: &str) -> bool {
    git_capture(repo_root, &["merge-base", "--is-ancestor", ancestor, rev]).0
}

/// True when every commit unique to `head` has a patch-identical equivalent
/// already reachable from `landing_base` (`git cherry`: all `-` lines, at
/// least one). The absorbed-landing case: a sibling's submit-time rebase
/// carried the task's commits onto the base as rebased copies.
fn all_patches_absorbed(repo_root: &Path, head: &str, landing_base: &str) -> bool {
    let (ok, out) = git_capture(repo_root, &["cherry", landing_base, head]);
    if !ok {
        return false;
    }
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    !lines.is_empty() && lines.iter().all(|line| line.starts_with('-'))
}

/// Decide a DONE task's landing state for the archive sweep (D-1/D-4):
/// receipt first, then ancestry, then patch identity; an unresolvable head
/// with no receipt is indeterminate.
fn probe_task_landing(
    store: &dyn ArchiveStore,
    repo_root: &Path,
    phase_dir: &Path,
    task_id: &str,
    landing_base: &str,
) -> Result<LandingState, StoreError> {
    if let Some(run_id) = latest_run_id(store, task_id)?
        && run_has_landed_receipt(store, &run_id)?
    {
        return Ok(LandingState::Landed);
    }
    let branch = format!("flywheel/{}/{task_id}", dir_name(phase_dir));
    let Some(head) = resolve_ref(repo_root, &branch) else {
        return Ok(LandingState::Indeterminate);
    };
    if is_ancestor(repo_root, &head, landing_base)
        || all_patches_absorbed(repo_root, &head, landing_base)
    {
        return Ok(LandingState::Landed);
    }
    Ok(LandingState::NotLanded)
}

fn format_landing_refusal(phase_name: &str, task_id: &str, state: LandingState) -> String {
    let cause = if state == LandingState::Indeterminate {
        "its landing state cannot be determined (no landing receipt and neither its branch nor \
         a recorded head resolves for the ancestry probe)"
    } else {
        "its recorded work is not landed (no landing receipt and its branch head is not an \
         ancestor of the landing base)"
    };
    format!("Refusing to archive phase {phase_name}: task {task_id} is not landed -- {cause}")
}

/// Merged-ness is git ancestry, never a remote PR state string: a squash /
/// rebase merge applies the phase's content while discarding the branch
/// commit, so equal trees with broken ancestry is the distinct mismatch.
fn probe_phase_merge(repo_root: &Path, phase_tip: &str, true_base: &str) -> PhaseMergeState {
    if is_ancestor(repo_root, phase_tip, true_base) {
        return PhaseMergeState::Merged;
    }
    if git_capture(repo_root, &["diff", "--quiet", true_base, phase_tip]).0 {
        return PhaseMergeState::MergeMethodMismatch;
    }
    PhaseMergeState::Unmerged
}

fn format_phase_merge_refusal(phase_name: &str, branch: &str, state: PhaseMergeState) -> String {
    let cause = if state == PhaseMergeState::MergeMethodMismatch {
        format!(
            "its integration branch {branch} is not an ancestor of the true base but its content \
             is already applied there -- a squash or rebase merge discarded the branch commit \
             (merge-method mismatch); re-merge the phase PR with a merge commit or resolve the \
             strand"
        )
    } else {
        format!(
            "its integration branch {branch} is not an ancestor of the true base -- the phase PR \
             is still open; archival waits for it to merge (the worker performs no local merge \
             of the phase branch)"
        )
    };
    format!("Refusing to archive phase {phase_name}: the phase PR is not merged -- {cause}")
}

/// Append one indeterminate-landing marker for `task_id`, idempotently:
/// skipped when the subject's latest stop row is already an unresolved
/// indeterminate-landing marker, so repeated sweeps surface the strand once.
fn record_indeterminate_landing(
    claims: &dyn ClaimStore,
    task_id: &str,
    phase_name: &str,
    now: DateTime<Utc>,
) -> Result<(), ClaimStoreError> {
    let events = claims.list_subject_stop_events(task_id)?;
    if events
        .last()
        .is_some_and(|latest| latest.kind == STOP_INDETERMINATE_LANDING)
    {
        return Ok(());
    }
    claims.record_stop_event(
        STOP_INDETERMINATE_LANDING,
        task_id,
        &format!(
            "phase {phase_name:?} cannot archive: task {task_id:?} has no landing receipt and \
             neither its branch nor a recorded head resolves for the ancestry probe"
        ),
        now,
        "",
    )
}

/// True when `task_id`'s latest stop event is an operator resolution (the
/// `resolve` verb deliberately abandoning a strand). Read off the record's
/// own `attribution` column, never parsed from the detail prose.
fn has_operator_resolution(
    claims: &dyn ClaimStore,
    task_id: &str,
) -> Result<bool, ClaimStoreError> {
    Ok(claims
        .list_subject_stop_events(task_id)?
        .last()
        .is_some_and(|latest| {
            latest.kind == STOP_RESOLVED && latest.attribution == RESOLUTION_ATTRIBUTION_OPERATOR
        }))
}

fn resolve_surfaced_stops(
    claims: &dyn ClaimStore,
    tasks: &[Task],
    phase_name: &str,
    landing_states: &BTreeMap<String, LandingState>,
    landing_base: Option<&str>,
    clock: &dyn Fn() -> DateTime<Utc>,
) -> Result<(), ClaimStoreError> {
    for task in tasks {
        let events = claims.list_subject_stop_events(&task.id)?;
        let Some(latest) = events.last() else {
            continue;
        };
        if latest.kind == STOP_RESOLVED {
            continue;
        }
        let latest_kind = latest.kind.as_str();
        if LANDING_STRAND_KINDS.contains(&latest_kind) || latest_kind == STOP_INDETERMINATE_LANDING
        {
            // Landing strands are not resolvable by the act of archiving;
            // this sweep stamps a resolution only when its own probe
            // confirmed the task landed, attributed to the probe.
            if landing_states.get(&task.id) == Some(&LandingState::Landed) {
                claims.record_stop_event(
                    STOP_RESOLVED,
                    &task.id,
                    &format!(
                        "phase {phase_name:?} archived after the landing probe confirmed this \
                         task's work reachable from landing base {:?}; supersedes the surfaced \
                         landing strand ({latest_kind:?})",
                        landing_base.unwrap_or_default()
                    ),
                    clock(),
                    RESOLUTION_ATTRIBUTION_PROBE,
                )?;
            }
            continue;
        }
        // Non-landing stops keep the archival supersession marker: archival
        // IS their verified resolution. No attribution.
        claims.record_stop_event(
            STOP_RESOLVED,
            &task.id,
            &format!(
                "phase {phase_name:?} archived with every task done and its exit gates passed; \
                 supersedes the surfaced stop ({latest_kind:?})"
            ),
            clock(),
            "",
        )?;
    }
    Ok(())
}

/// The gate is satisfied by a valid opt-out artifact or a DONE task carrying
/// the `in-loop-verification` tag. A malformed artifact is treated as no
/// opt-out: the refusal still fires and the operator must fix it.
fn loop_path_gate_satisfied(
    phase_dir: &Path,
    tasks: &[Task],
    store: &dyn ArchiveStore,
) -> Result<bool, StoreError> {
    if let Ok(Some(_)) = load_loop_path_optout(phase_dir) {
        return Ok(true);
    }
    for task in tasks {
        if task.tags.iter().any(|t| t == IN_LOOP_VERIFICATION_TAG)
            && has_done_lifecycle(store, &task.id)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn format_gate_refusal(phase_name: &str, signals: &BTreeSet<LoopPathSignal>) -> String {
    let names = signals
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Refusing to archive phase {phase_name}: loop-path signal(s) [{names}] detected and \
         neither a DONE {IN_LOOP_VERIFICATION_TAG} task nor a {LOOP_PATH_OPTOUT_FILENAME} opt-out \
         is present"
    )
}

/// Run the phase-verify command through `/bin/sh -c` in `repo_root`,
/// capturing output so the refusal can quote the failure's tail.
fn run_phase_verify(command: &str, repo_root: &Path) -> (Option<i32>, String) {
    match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let text = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            let trimmed = text.trim();
            let count = trimmed.chars().count();
            let tail: String = trimmed.chars().skip(count.saturating_sub(2000)).collect();
            (output.status.code(), tail)
        }
        Err(err) => (None, format!("cannot spawn {command:?}: {err}")),
    }
}

fn format_phase_verify_refusal(
    phase_name: &str,
    command: &str,
    code: Option<i32>,
    tail: &str,
) -> String {
    let exit = code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
    let mut message = format!(
        "Refusing to archive phase {phase_name}: phase-verify command {command:?} failed against \
         the merged base (exit {exit})"
    );
    if !tail.is_empty() {
        message.push_str(": ");
        message.push_str(tail);
    }
    message
}

/// Commit an archival move so the queue's git truth matches the disk.
/// Fires only when the active phase was git-tracked before the move; scoped
/// strictly to the two moved paths via pathspec so unrelated staged operator
/// state is left untouched. Best-effort: a failure is logged with a
/// commit-by-hand pointer, never raised.
fn commit_archive_move(repo_root: &Path, active_dir: &Path, dest: &Path, log: &dyn Fn(String)) {
    let active = active_dir.to_string_lossy().into_owned();
    let archived = dest.to_string_lossy().into_owned();
    let (ok, tracked) = git_capture(repo_root, &["ls-files", "--", &active]);
    if !ok || tracked.trim().is_empty() {
        return;
    }
    let name = dir_name(dest);
    let (mut ok, mut out) = git_capture(repo_root, &["add", "-A", "--", &active, &archived]);
    if ok {
        (ok, out) = git_capture(
            repo_root,
            &[
                "commit",
                "-m",
                &format!("chore: archive completed phase {name}"),
                "--",
                &active,
                &archived,
            ],
        );
    }
    if !ok {
        let last = out.trim().lines().last().unwrap_or("git failed").to_owned();
        log(format!(
            "archival move of {name} left uncommitted ({last}); commit .flywheel/tasks by hand \
             to keep the queue's git truth in step"
        ));
        return;
    }
    log(format!("Committed archival of phase {name}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submitter::test_support::{
        branch_exists, commit, git_ok, init_repo, is_ancestor as git_is_ancestor, landed, ledger,
        parked, range_shas, rev, sandbox_req, submit_req, submitter, task_file, trailer_value,
    };
    use flywheel_core::events::{
        DomainEvent, LANDING_STRATEGY_MERGE, Landed, PARK_KIND_DIVERGENT_BASE,
        PARK_KIND_PROTECTED_PATHS, PARK_KIND_STANDING_VERIFY,
    };
    use flywheel_core::lifecycle::Lifecycle;
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_orchestrator::claims::{InMemoryClaimStore, STOP_DANGLING_PREREQUISITE};
    use std::sync::Mutex;

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        init_repo(&path);
        (dir, path)
    }

    fn phase_submitter(repo: &Path, store: Arc<InMemoryStore>) -> PhaseBranchSubmitter {
        PhaseBranchSubmitter::new(submitter(repo, store))
    }

    fn prepare_with_commit(
        s: &PhaseBranchSubmitter,
        repo: &Path,
        phase: &str,
        task_id: &str,
        file: &str,
    ) -> (PathBuf, PathBuf) {
        let tf = task_file(repo, phase, task_id, "true");
        let wt = s.prepare_worktree(&sandbox_req(&tf, task_id)).unwrap();
        commit(&wt, file, "x", &format!("feat: {file}"));
        (tf, wt)
    }

    fn capture_log() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: Logger = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
        (log, lines)
    }

    // --- the phase-branch landing strategy ---------------------------------------------

    #[test]
    fn two_tasks_stack_on_the_phase_branch_and_the_true_base_never_moves() {
        let (_dir, repo) = repo();
        let store = ledger();
        let s = phase_submitter(&repo, Arc::clone(&store));
        let true_base = rev(&repo, "main");
        let branch = phase_branch("01-phase");
        assert!(!branch_exists(&repo, &branch));

        let (tf1, wt1) = prepare_with_commit(&s, &repo, "01-phase", "t1", "a.txt");
        assert!(
            s.submit(&submit_req(&tf1, "t1", &wt1, Status::Done, &["true"]))
                .is_landed()
        );
        assert!(branch_exists(&repo, &branch));
        let first_tip = rev(&repo, &branch);
        assert!(!wt1.exists());
        assert_eq!(rev(&repo, "main"), true_base);
        assert_eq!(landed(&store)[0].landed_ref, first_tip);
        assert_eq!(landed(&store)[0].strategy, LANDING_STRATEGY_MERGE);

        // The second task forks from the integration branch and stacks.
        let (tf2, wt2) = prepare_with_commit(&s, &repo, "01-phase", "t2", "b.txt");
        assert!(git_is_ancestor(&repo, &first_tip, &rev(&wt2, "HEAD")));
        let mut req = submit_req(&tf2, "t2", &wt2, Status::Done, &["true"]);
        req.task_id = "t2".into();
        assert!(s.submit(&req).is_landed());
        let second_tip = rev(&repo, &branch);
        assert_ne!(second_tip, first_tip);
        assert!(git_is_ancestor(&repo, &first_tip, &second_tip));
        assert_eq!(rev(&repo, "main"), true_base);
        assert!(!repo.join("a.txt").exists(), "operator checkout untouched");
        assert_eq!(
            git_ok(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        // Trailers are stamped on the phase path too.
        let shas = range_shas(&repo, &format!("{true_base}..{second_tip}"));
        assert_eq!(shas.len(), 2);
        assert_eq!(trailer_value(&repo, &shas[1], "Flywheel-Task"), "t2");
    }

    #[test]
    fn first_landing_creates_the_branch_at_the_true_base_tip() {
        let (_dir, repo) = repo();
        let s = phase_submitter(&repo, ledger());
        let (tf, wt) = prepare_with_commit(&s, &repo, "01-phase", "t1", "a.txt");
        commit(&repo, "advance.txt", "x", "base advances after fork");
        let true_base_tip = rev(&repo, "main");
        assert!(
            s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]))
                .is_landed()
        );
        let branch = phase_branch("01-phase");
        // The branch forks from the then-current true base (so the rebase
        // path ran) and ends at the task's rebased commit.
        assert!(git_is_ancestor(&repo, &true_base_tip, &rev(&repo, &branch)));
        assert_eq!(
            git_ok(&repo, &["rev-list", "--count", &format!("main..{branch}")]),
            "1"
        );
        assert_eq!(rev(&repo, "main"), true_base_tip);
    }

    #[test]
    fn two_phases_get_distinct_integration_branches() {
        let (_dir, repo) = repo();
        let s = phase_submitter(&repo, ledger());
        let (tf1, wt1) = prepare_with_commit(&s, &repo, "01-phase", "t1", "a.txt");
        s.submit(&submit_req(&tf1, "t1", &wt1, Status::Done, &["true"]));
        let (tf2, wt2) = prepare_with_commit(&s, &repo, "02-phase", "t2", "b.txt");
        let mut req = submit_req(&tf2, "t2", &wt2, Status::Done, &["true"]);
        req.task_id = "t2".into();
        s.submit(&req);
        assert!(branch_exists(&repo, "flywheel/phase/01-phase"));
        assert!(branch_exists(&repo, "flywheel/phase/02-phase"));
        assert_ne!(
            rev(&repo, "flywheel/phase/01-phase"),
            rev(&repo, "flywheel/phase/02-phase")
        );
        assert_eq!(
            git_ok(
                &repo,
                &["rev-list", "--count", "main..flywheel/phase/02-phase"]
            ),
            "1"
        );
    }

    #[test]
    fn parks_leave_the_phase_branch_unchanged_or_uncreated() {
        let (_dir, repo) = repo();
        let store = ledger();
        // Protected path on the first task: no branch is ever created.
        let s = phase_submitter(&repo, Arc::clone(&store))
            .inner()
            .clone()
            .with_protected_paths(["conftest.py"]);
        let s = PhaseBranchSubmitter::new(s);
        let (tf, wt) = prepare_with_commit(&s, &repo, "01-phase", "t1", "conftest.py");
        let outcome = s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]));
        assert!(
            matches!(&outcome, SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_PROTECTED_PATHS)
        );
        assert!(!branch_exists(&repo, "flywheel/phase/01-phase"));
        assert!(wt.exists());

        // Clean-FF standing-verify failure: still no branch.
        let failing = PhaseBranchSubmitter::new(
            submitter(&repo, Arc::clone(&store)).with_verify_command("false"),
        );
        let (tf2, wt2) = prepare_with_commit(&failing, &repo, "01-phase", "t2", "b.txt");
        let mut req = submit_req(&tf2, "t2", &wt2, Status::Done, &["true"]);
        req.task_id = "t2".into();
        assert!(
            matches!(failing.submit(&req), SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_STANDING_VERIFY)
        );
        assert!(!branch_exists(&repo, "flywheel/phase/01-phase"));

        // A landed first task creates the branch; a base-advanced
        // standing-verify failure on the next leaves it byte-identical.
        let ok = phase_submitter(&repo, Arc::clone(&store));
        let (tf3, wt3) = prepare_with_commit(&ok, &repo, "01-phase", "t3", "c.txt");
        let mut req = submit_req(&tf3, "t3", &wt3, Status::Done, &["true"]);
        req.task_id = "t3".into();
        assert!(ok.submit(&req).is_landed());
        let branch_tip = rev(&repo, "flywheel/phase/01-phase");
        let (tf4, wt4) = prepare_with_commit(&failing, &repo, "01-phase", "t4", "d.txt");
        // Advance the integration branch out-of-band so t4 must rebase.
        let (tf5, wt5) = prepare_with_commit(&ok, &repo, "01-phase", "t5", "e.txt");
        let mut req5 = submit_req(&tf5, "t5", &wt5, Status::Done, &["true"]);
        req5.task_id = "t5".into();
        assert!(ok.submit(&req5).is_landed());
        let advanced_tip = rev(&repo, "flywheel/phase/01-phase");
        assert_ne!(advanced_tip, branch_tip);
        let mut req4 = submit_req(&tf4, "t4", &wt4, Status::Done, &["true"]);
        req4.task_id = "t4".into();
        assert!(
            matches!(failing.submit(&req4), SubmitOutcome::Parked { park_kind, .. } if park_kind == PARK_KIND_STANDING_VERIFY)
        );
        assert_eq!(rev(&repo, "flywheel/phase/01-phase"), advanced_tip);
        assert!(wt4.exists());
        assert!(
            parked(&store)
                .iter()
                .all(|p| p.park_kind != PARK_KIND_DIVERGENT_BASE)
        );
    }

    #[test]
    fn merge_strategy_creates_no_phase_branch() {
        let (_dir, repo) = repo();
        let s = submitter(&repo, ledger());
        let tf = task_file(&repo, "01-phase", "t1", "true");
        let wt = s.prepare_worktree(&sandbox_req(&tf, "t1")).unwrap();
        commit(&wt, "a.txt", "x", "feat: a");
        assert!(
            s.submit(&submit_req(&tf, "t1", &wt, Status::Done, &["true"]))
                .is_landed()
        );
        assert!(!branch_exists(&repo, "flywheel/phase/01-phase"));
        assert!(repo.join("a.txt").exists());
    }

    #[test]
    fn build_phase_submitter_logs_the_landing_target() {
        let (_dir, repo) = repo();
        let (log, lines) = capture_log();
        let req = crate::submit_registry::SubmitBuildRequest::new(
            &repo,
            repo.join(".flywheel/tasks"),
            repo.join(".flywheel/worktrees"),
            "main",
            repo.join(".flywheel/.merge.lock"),
            log,
        );
        let s = build_phase_submitter(&req);
        assert_eq!(s.phase_base, "main");
        assert!(lines.lock().unwrap()[0].contains("landing strategy: phase"));
        assert_eq!(s.landing_base_for("01-phase").unwrap(), "main");
    }

    // --- loop-base refs ---------------------------------------------------------------

    #[test]
    fn record_phase_bases_captures_once_and_is_idempotent() {
        let (_dir, repo) = repo();
        let tasks_dir = repo.join(".flywheel").join("tasks");
        let phase_dir = tasks_dir.join("active").join("01-phase");
        fs::create_dir_all(&phase_dir).unwrap();
        let lock_path = repo.join(".flywheel").join(".merge.lock");
        let (log, lines) = capture_log();
        let head_at_capture = rev(&repo, "main");

        assert_eq!(
            record_phase_bases(&repo, &tasks_dir, &lock_path, &log).unwrap(),
            1
        );
        assert!(!phase_dir.join(".loop-base").exists());
        assert_eq!(
            rev(&repo, "refs/flywheel/loop-base/01-phase"),
            head_at_capture
        );
        assert_eq!(rev(&repo, "main"), head_at_capture);
        assert_eq!(
            read_phase_base(&phase_dir, Some(&repo)).as_deref(),
            Some(head_at_capture.as_str())
        );
        assert_eq!(read_phase_base(&phase_dir, None), None);
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l == "Recorded base sha for 1 phase(s)")
        );

        commit(&repo, "advance.txt", "x", "advance main");
        assert_eq!(
            record_phase_bases(&repo, &tasks_dir, &lock_path, &log).unwrap(),
            0
        );
        assert_eq!(
            rev(&repo, "refs/flywheel/loop-base/01-phase"),
            head_at_capture
        );
        // A legacy dotfile wins over the ref.
        fs::write(phase_dir.join(".loop-base"), "deadbeef\n").unwrap();
        assert_eq!(
            read_phase_base(&phase_dir, Some(&repo)).as_deref(),
            Some("deadbeef")
        );
        // No active dir: nothing to record.
        assert_eq!(
            record_phase_bases(&repo, &repo.join("nope"), &lock_path, &log).unwrap(),
            0
        );
        assert!(!write_phase_base_if_missing(
            &repo,
            &tasks_dir.join("active").join("absent")
        ));
    }

    // --- the opt-out artifact ---------------------------------------------------------

    #[test]
    fn opt_out_parses_valid_front_matter_and_rejects_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let phase = dir.path().join("01-phase");
        fs::create_dir_all(&phase).unwrap();
        assert_eq!(load_loop_path_optout(&phase).unwrap(), None);
        assert_eq!(
            load_loop_path_optout(&dir.path().join("absent")).unwrap(),
            None
        );

        let artifact = phase.join(LOOP_PATH_OPTOUT_FILENAME);
        fs::write(
            &artifact,
            "---\n# a comment\nphase: 01-phase\nauthor: op\n\nreason: no new loop path\nextra: ok\n---\nProse: ignored\n",
        )
        .unwrap();
        assert_eq!(
            load_loop_path_optout(&phase).unwrap(),
            Some(LoopPathOptOut {
                phase: "01-phase".into(),
                author: "op".into(),
                reason: "no new loop path".into(),
            })
        );

        fs::write(&artifact, "phase: x\n").unwrap();
        assert!(
            load_loop_path_optout(&phase)
                .unwrap_err()
                .0
                .contains("must start with")
        );
        fs::write(&artifact, "---\nphase: x\n").unwrap();
        assert!(
            load_loop_path_optout(&phase)
                .unwrap_err()
                .0
                .contains("not closed")
        );
        fs::write(&artifact, "---\nnot a pair\n---\n").unwrap();
        assert!(
            load_loop_path_optout(&phase)
                .unwrap_err()
                .0
                .contains("expected 'key: value'")
        );
        fs::write(&artifact, "---\n: value\n---\n").unwrap();
        assert!(
            load_loop_path_optout(&phase)
                .unwrap_err()
                .0
                .contains("empty key")
        );
        fs::write(&artifact, "---\nphase: x\nauthor:\n---\n").unwrap();
        let err = load_loop_path_optout(&phase).unwrap_err();
        assert!(
            err.0.contains("missing required key(s): author, reason"),
            "{err}"
        );
    }

    // --- the archive sweep --------------------------------------------------------------

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        tasks_dir: PathBuf,
        store: InMemoryStore,
        claims: InMemoryClaimStore,
    }

    impl Fixture {
        fn new() -> Self {
            let (dir, repo) = repo();
            let tasks_dir = repo.join(".flywheel").join("tasks");
            fs::create_dir_all(tasks_dir.join("active")).unwrap();
            // The queue is untracked by default so a checkout in the repo
            // never sweeps it onto a task branch; the tracked-queue test
            // opts in explicitly.
            fs::write(repo.join(".gitignore"), "/.flywheel/\n").unwrap();
            git_ok(&repo, &["add", ".gitignore"]);
            git_ok(&repo, &["commit", "-q", "-m", "ignore queue"]);
            Self {
                _dir: dir,
                repo,
                tasks_dir,
                store: InMemoryStore::new(),
                claims: InMemoryClaimStore::new(),
            }
        }

        fn phase(&self, name: &str) -> PathBuf {
            let dir = self.tasks_dir.join("active").join(name);
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn task(&self, phase: &str, task_id: &str, tags: &[&str]) -> PathBuf {
            let path = self.phase(phase).join(format!("{task_id}.json"));
            fs::write(
                &path,
                serde_json::json!({
                    "id": task_id,
                    "goal": "g",
                    "graders": [{"type": "command", "run": "true"}],
                    "tags": tags,
                })
                .to_string(),
            )
            .unwrap();
            path
        }

        fn done(&self, task_id: &str, run_id: &str) {
            let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
            for st in [
                Status::Ready,
                Status::Running,
                Status::Validating,
                Status::Done,
            ] {
                lc.transition_to(st, "", Some(Utc::now())).unwrap();
            }
            self.store.create_lifecycle(&lc).unwrap();
        }

        fn running(&self, task_id: &str, run_id: &str) {
            let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
            for st in [Status::Ready, Status::Running] {
                lc.transition_to(st, "", Some(Utc::now())).unwrap();
            }
            self.store.create_lifecycle(&lc).unwrap();
        }

        fn landed_receipt(&self, run_id: &str) {
            let lc = self.store.load_lifecycle(run_id).unwrap().unwrap();
            let event = DomainEvent::new(
                run_id,
                Utc::now(),
                DomainEventPayload::Landed(Landed {
                    strategy: LANDING_STRATEGY_MERGE.into(),
                    landed_ref: "abc".into(),
                    ..Landed::default()
                }),
            );
            self.store.append_domain_event(&event, lc.version).unwrap();
        }

        fn sweep(&self, options: ArchiveOptions<'_>) -> (Vec<PathBuf>, Vec<String>) {
            let (log, lines) = capture_log();
            let options = ArchiveOptions {
                log: Some(&log),
                ..options
            };
            let moved = archive_completed_phases(&self.tasks_dir, &self.store, &options).unwrap();
            let logged = lines.lock().unwrap().clone();
            (moved, logged)
        }

        fn archived(&self, name: &str) -> bool {
            !self.tasks_dir.join("active").join(name).exists()
                && self.tasks_dir.join("archive").join(name).is_dir()
        }
    }

    #[test]
    fn all_done_phase_archives_and_non_done_phase_stays() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.task("01-phase", "t2", &[]);
        f.task("02-phase", "t3", &[]);
        f.phase("03-empty");
        f.done("t1", "run-t1");
        f.done("t2", "run-t2");
        f.running("t3", "run-t3");
        let (moved, logged) = f.sweep(ArchiveOptions::default());
        assert_eq!(moved, vec![f.tasks_dir.join("archive").join("01-phase")]);
        assert!(f.archived("01-phase"));
        assert!(!f.archived("02-phase"));
        assert!(f.tasks_dir.join("active").join("03-empty").is_dir());
        assert!(logged.is_empty());
        // Idempotent: a second sweep moves nothing.
        assert!(f.sweep(ArchiveOptions::default()).0.is_empty());
        // Once t3 is done it archives too; a t3 retry that failed does not
        // mask the earlier DONE run.
        f.done("t3", "run-t3-again");
        assert_eq!(f.sweep(ArchiveOptions::default()).0.len(), 1);
    }

    #[test]
    fn existing_archive_destination_is_refused_loudly() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        fs::create_dir_all(f.tasks_dir.join("archive").join("01-phase")).unwrap();
        let (moved, logged) = f.sweep(ArchiveOptions::default());
        assert!(moved.is_empty());
        assert!(f.tasks_dir.join("active").join("01-phase").is_dir());
        assert_eq!(logged.len(), 1);
        assert!(
            logged[0].starts_with("Refusing to archive phase 01-phase:"),
            "{}",
            logged[0]
        );
        assert!(logged[0].contains("already exists"));
    }

    /// Emit one autopilot task into the fixed `active/autopilot` phase, mark
    /// it DONE, and record the phase's loop-base ref at the current HEAD.
    fn emit_autopilot_generation(f: &Fixture, task_id: &str) -> String {
        use flywheel_core::task::{CommandGrader, Grader};
        use flywheel_orchestrator::autopilot::{
            AUTOPILOT_PHASE, EmittedTask, Finding, GraderSource, ScoreBreakdown, Tier,
            emit_emitted_task,
        };
        let task = Task::new(
            format!("Goal for {task_id}."),
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id(task_id);
        let emitted = EmittedTask::new(
            Finding::new(format!("find-{task_id}"), Tier::TestCoverage, task_id),
            task,
            "true",
            GraderSource::RepoCommand,
            "Cargo.toml",
        );
        let breakdown = ScoreBreakdown {
            tier: Tier::TestCoverage,
            tier_weight: 1,
            urgency: 1,
            importance: 1,
            blocks: 0,
            effort: 1,
            final_score: 10.0,
            preemptive: false,
        };
        let written = emit_emitted_task(&emitted, &breakdown, &f.tasks_dir, AUTOPILOT_PHASE)
            .unwrap()
            .expect("a fresh id is emitted");
        assert_eq!(
            written.parent().unwrap(),
            f.tasks_dir.join("active").join(AUTOPILOT_PHASE)
        );
        let phase = written.parent().unwrap().to_path_buf();
        assert!(write_phase_base_if_missing(&f.repo, &phase));
        f.done(task_id, &format!("run-{task_id}"));
        rev(&f.repo, "HEAD")
    }

    #[test]
    fn a_recurring_phase_name_archives_each_generation_to_its_own_destination() {
        // Autopilot always emits into `active/autopilot`; once the first
        // generation archives, every later one must archive too (never a
        // permanent "already exists" refusal) without touching the earlier
        // archive.
        let f = Fixture::new();
        let armed = ArchiveOptions {
            repo_root: Some(&f.repo),
            ..ArchiveOptions::default()
        };
        let archive = f.tasks_dir.join("archive");
        let mut bases = Vec::new();
        for (generation, task_id) in ["add-retry", "add-parser-tests", "fix-timeout"]
            .into_iter()
            .enumerate()
        {
            commit(
                &f.repo,
                &format!("gen{generation}.txt"),
                "x\n",
                &format!("generation {generation}"),
            );
            bases.push(emit_autopilot_generation(&f, task_id));
            let (moved, logged) = f.sweep(armed);
            assert!(logged.is_empty(), "generation {generation}: {logged:?}");
            let dest = if generation == 0 {
                archive.join("autopilot")
            } else {
                archive.join(format!("autopilot-{}", generation + 1))
            };
            assert_eq!(moved, vec![dest.clone()]);
            assert!(!f.tasks_dir.join("active").join("autopilot").exists());
            assert!(dest.join(format!("{task_id}.json")).is_file());
            // Each generation keeps its own recorded base, and the shared
            // loop-base ref is dropped so the next generation records afresh.
            assert_eq!(
                fs::read_to_string(dest.join(LOOP_BASE_FILENAME))
                    .unwrap()
                    .trim(),
                bases[generation]
            );
            assert!(
                !git(
                    &f.repo,
                    &[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        "refs/flywheel/loop-base/autopilot"
                    ]
                )
                .unwrap()
                .ok()
            );
        }
        // Earlier generations are untouched.
        assert!(archive.join("autopilot/add-retry.json").is_file());
        assert!(archive.join("autopilot-2/add-parser-tests.json").is_file());
        assert!(!archive.join("autopilot/add-parser-tests.json").exists());
    }

    #[test]
    fn a_resurrected_copy_of_any_archived_generation_is_still_refused() {
        // An archival whose active/ deletion was never committed resurrects
        // the same task files on the next checkout: that is a duplicate of
        // the archived generation, not a new one, and stays refused loudly.
        let f = Fixture::new();
        f.task("autopilot", "add-retry", &[]);
        f.done("add-retry", "run-add-retry");
        assert_eq!(f.sweep(ArchiveOptions::default()).0.len(), 1);
        f.task("autopilot", "add-parser-tests", &[]);
        f.done("add-parser-tests", "run-add-parser-tests");
        assert_eq!(
            f.sweep(ArchiveOptions::default()).0,
            vec![f.tasks_dir.join("archive").join("autopilot-2")]
        );
        for resurrected in ["add-retry", "add-parser-tests"] {
            f.task("autopilot", resurrected, &[]);
            let (moved, logged) = f.sweep(ArchiveOptions::default());
            assert!(moved.is_empty(), "{resurrected}: {moved:?}");
            assert_eq!(logged.len(), 1);
            assert!(
                logged[0].starts_with("Refusing to archive phase autopilot:"),
                "{}",
                logged[0]
            );
            assert!(logged[0].contains(resurrected), "{}", logged[0]);
            assert!(
                f.tasks_dir
                    .join("active/autopilot")
                    .join(format!("{resurrected}.json"))
                    .is_file()
            );
            fs::remove_dir_all(f.tasks_dir.join("active/autopilot")).unwrap();
        }
        assert!(!f.tasks_dir.join("archive/autopilot-3").exists());
    }

    #[test]
    fn landed_predicate_blocks_unlanded_and_surfaces_indeterminate_once() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        // No branch, no receipt: indeterminate, surfaced exactly once.
        let armed = ArchiveOptions {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            claims: Some(&f.claims),
            ..ArchiveOptions::default()
        };
        let (moved, logged) = f.sweep(armed);
        assert!(moved.is_empty());
        assert!(logged[0].contains("cannot be determined"), "{}", logged[0]);
        f.sweep(armed);
        let stops = f.claims.list_subject_stop_events("t1").unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].kind, STOP_INDETERMINATE_LANDING);

        // A branch that is not an ancestor: a determinate strand.
        git_ok(&f.repo, &["branch", "flywheel/01-phase/t1", "main"]);
        git_ok(&f.repo, &["checkout", "-q", "flywheel/01-phase/t1"]);
        commit(&f.repo, "work.txt", "w", "task work");
        git_ok(&f.repo, &["checkout", "-q", "main"]);
        let (moved, logged) = f.sweep(armed);
        assert!(moved.is_empty());
        assert!(
            logged[0].contains("not an ancestor of the landing base"),
            "{}",
            logged[0]
        );

        // Landed by git truth: the probe confirms and the strand resolves.
        git_ok(&f.repo, &["merge", "--ff-only", "flywheel/01-phase/t1"]);
        let (moved, _) = f.sweep(armed);
        assert_eq!(moved.len(), 1);
        let stops = f.claims.list_subject_stop_events("t1").unwrap();
        assert_eq!(stops.last().unwrap().kind, STOP_RESOLVED);
        assert_eq!(
            stops.last().unwrap().attribution,
            RESOLUTION_ATTRIBUTION_PROBE
        );
    }

    #[test]
    fn landed_receipt_fast_path_and_operator_resolution_unblock() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.task("01-phase", "t2", &[]);
        f.done("t1", "run-t1");
        f.done("t2", "run-t2");
        f.landed_receipt("run-t1");
        let armed = ArchiveOptions {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            claims: Some(&f.claims),
            ..ArchiveOptions::default()
        };
        let (moved, logged) = f.sweep(armed);
        assert!(moved.is_empty());
        assert_eq!(logged.len(), 1);
        assert!(logged[0].contains("task t2"), "{}", logged[0]);
        // The operator abandons t2's strand: it no longer blocks.
        f.claims
            .record_stop_event(
                STOP_RESOLVED,
                "t2",
                "abandoned",
                Utc::now(),
                RESOLUTION_ATTRIBUTION_OPERATOR,
            )
            .unwrap();
        let (moved, _) = f.sweep(armed);
        assert_eq!(moved.len(), 1);
    }

    #[test]
    fn patch_absorbed_branch_counts_as_landed() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        git_ok(&f.repo, &["checkout", "-q", "-b", "flywheel/01-phase/t1"]);
        commit(&f.repo, "work.txt", "w", "task work");
        git_ok(&f.repo, &["checkout", "-q", "main"]);
        // A sibling's rebase carried the same patch onto main as a new sha.
        commit(&f.repo, "other.txt", "o", "sibling");
        git_ok(&f.repo, &["cherry-pick", "flywheel/01-phase/t1"]);
        assert!(!git_is_ancestor(&f.repo, "flywheel/01-phase/t1", "main"));
        let (moved, _) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            ..ArchiveOptions::default()
        });
        assert_eq!(moved.len(), 1);
    }

    #[test]
    fn phase_merge_predicate_distinguishes_open_squash_and_merged() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        f.landed_receipt("run-t1");
        git_ok(
            &f.repo,
            &["checkout", "-q", "-b", "flywheel/phase/01-phase"],
        );
        commit(&f.repo, "phase.txt", "p", "phase work");
        git_ok(&f.repo, &["checkout", "-q", "main"]);
        let armed = ArchiveOptions {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            true_base: Some("main"),
            ..ArchiveOptions::default()
        };
        let (moved, logged) = f.sweep(armed);
        assert!(moved.is_empty());
        assert!(
            logged[0].contains("the phase PR is still open"),
            "{}",
            logged[0]
        );

        // The completion seam fires instead of the refusal.
        let seen: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());
        let completion = |dir: &Path, tasks: &[Task]| {
            seen.lock().unwrap().push((dir.to_path_buf(), tasks.len()));
        };
        let (moved, logged) = f.sweep(ArchiveOptions {
            phase_completion: Some(&completion),
            ..armed
        });
        assert!(moved.is_empty());
        assert!(logged.is_empty());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(f.tasks_dir.join("active/01-phase"), 1)]
        );

        // A squash merge applies the content but breaks ancestry.
        git_ok(&f.repo, &["merge", "--squash", "flywheel/phase/01-phase"]);
        git_ok(&f.repo, &["commit", "-m", "squash"]);
        let (moved, logged) = f.sweep(armed);
        assert!(moved.is_empty());
        assert!(logged[0].contains("merge-method mismatch"), "{}", logged[0]);

        // A merge commit keeps ancestry: the phase archives.
        git_ok(
            &f.repo,
            &[
                "merge",
                "--no-ff",
                "-m",
                "merge phase",
                "flywheel/phase/01-phase",
            ],
        );
        let (moved, _) = f.sweep(armed);
        assert_eq!(moved.len(), 1);
    }

    #[test]
    fn loop_path_gate_requires_verification_task_or_opt_out() {
        let f = Fixture::new();
        let phase = f.phase("01-phase");
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        // Record the base, then land a watched schema change past it.
        assert!(write_phase_base_if_missing(&f.repo, &phase));
        fs::create_dir_all(f.repo.join("pkg/schema")).unwrap();
        commit(
            &f.repo,
            "pkg/schema/x.sql",
            "CREATE TABLE foo (id INTEGER);\n",
            "schema",
        );
        let armed = ArchiveOptions {
            repo_root: Some(&f.repo),
            ..ArchiveOptions::default()
        };
        let (moved, logged) = f.sweep(armed);
        assert!(moved.is_empty());
        assert!(
            logged[0].contains("loop-path signal(s) [schema_column_or_table]"),
            "{}",
            logged[0]
        );

        // A malformed opt-out does not clear the gate.
        fs::write(
            phase.join(LOOP_PATH_OPTOUT_FILENAME),
            "---\nphase: 01-phase\n",
        )
        .unwrap();
        assert!(f.sweep(armed).0.is_empty());
        fs::write(
            phase.join(LOOP_PATH_OPTOUT_FILENAME),
            "---\nphase: 01-phase\nauthor: op\nreason: covered elsewhere\n---\n",
        )
        .unwrap();
        let (moved, _) = f.sweep(armed);
        assert_eq!(moved.len(), 1);
        // Archiving materialized the ref into the dotfile and dropped the ref.
        let dest = f.tasks_dir.join("archive/01-phase");
        assert!(dest.join(LOOP_BASE_FILENAME).is_file());
        assert!(
            !git(
                &f.repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/flywheel/loop-base/01-phase"
                ]
            )
            .unwrap()
            .ok()
        );

        // A DONE in-loop-verification task satisfies the gate too.
        let phase2 = f.phase("02-phase");
        f.task("02-phase", "t2", &[IN_LOOP_VERIFICATION_TAG]);
        f.done("t2", "run-t2");
        assert!(write_phase_base_if_missing(&f.repo, &phase2));
        commit(
            &f.repo,
            "pkg/schema/y.sql",
            "CREATE TABLE bar (id INTEGER);\n",
            "schema 2",
        );
        assert_eq!(f.sweep(armed).0.len(), 1);
    }

    #[test]
    fn phase_verify_gate_blocks_on_failure_and_quotes_the_tail() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        let (moved, logged) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            phase_verify: Some("echo integration broke >&2; exit 7"),
            ..ArchiveOptions::default()
        });
        assert!(moved.is_empty());
        assert!(
            logged[0].contains("(exit 7): integration broke"),
            "{}",
            logged[0]
        );
        let (moved, _) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            phase_verify: Some("test -f README.md"),
            ..ArchiveOptions::default()
        });
        assert_eq!(moved.len(), 1);
    }

    #[test]
    fn archival_move_is_committed_when_the_queue_is_tracked() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        git_ok(&f.repo, &["rm", "-q", ".gitignore"]);
        git_ok(&f.repo, &["add", "-A"]);
        git_ok(&f.repo, &["commit", "-m", "queue"]);
        fs::write(f.repo.join("unrelated.txt"), "staged but untouched").unwrap();
        git_ok(&f.repo, &["add", "unrelated.txt"]);
        let before = rev(&f.repo, "main");
        let (moved, logged) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            ..ArchiveOptions::default()
        });
        assert_eq!(moved.len(), 1);
        assert!(
            logged
                .iter()
                .any(|l| l == "Committed archival of phase 01-phase"),
            "{logged:?}"
        );
        assert_ne!(rev(&f.repo, "main"), before);
        assert_eq!(
            git_ok(&f.repo, &["log", "-1", "--format=%s"]),
            "chore: archive completed phase 01-phase"
        );
        assert!(git_ok(&f.repo, &["ls-files", ".flywheel/tasks/active"]).is_empty());
        assert!(!git_ok(&f.repo, &["ls-files", ".flywheel/tasks/archive/01-phase"]).is_empty());
        // The unrelated staged file was not swept into the commit.
        assert!(git_ok(&f.repo, &["diff", "--cached", "--name-only"]).contains("unrelated.txt"));
    }

    #[test]
    fn archive_resolves_surfaced_non_landing_stops_and_leaves_unproven_strands() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.task("01-phase", "t2", &[]);
        f.done("t1", "run-t1");
        f.done("t2", "run-t2");
        f.claims
            .record_stop_event(STOP_DANGLING_PREREQUISITE, "t1", "stranded", Utc::now(), "")
            .unwrap();
        f.claims
            .record_stop_event(PARK_KIND_DIVERGENT_BASE, "t2", "parked", Utc::now(), "")
            .unwrap();
        // Unarmed landed predicate: proves nothing about t2's strand.
        let (moved, _) = f.sweep(ArchiveOptions {
            claims: Some(&f.claims),
            ..ArchiveOptions::default()
        });
        assert_eq!(moved.len(), 1);
        let t1 = f.claims.list_subject_stop_events("t1").unwrap();
        assert_eq!(t1.last().unwrap().kind, STOP_RESOLVED);
        assert_eq!(t1.last().unwrap().attribution, "");
        let t2 = f.claims.list_subject_stop_events("t2").unwrap();
        assert_eq!(t2.last().unwrap().kind, PARK_KIND_DIVERGENT_BASE);
    }

    #[test]
    fn archive_phases_wrapper_resolves_the_true_base_and_announces() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        f.landed_receipt("run-t1");
        // An unmerged phase branch blocks via the checkout-derived true base.
        git_ok(
            &f.repo,
            &["checkout", "-q", "-b", "flywheel/phase/01-phase"],
        );
        commit(&f.repo, "phase.txt", "p", "phase work");
        git_ok(&f.repo, &["checkout", "-q", "main"]);
        let (log, lines) = capture_log();
        let policy = WorkPolicy {
            phase_verify: Some("exit 9".into()),
            submit_strategy: "phase".into(),
            ..WorkPolicy::default()
        };
        let sweep = ArchiveSweep {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            policy: Some(&policy),
            ..ArchiveSweep::default()
        };
        let moved = archive_phases(&f.tasks_dir, &f.store, Some(&f.claims), &log, &sweep).unwrap();
        assert!(moved.is_empty());
        assert!(lines.lock().unwrap()[0].contains("phase PR is still open"));

        // Merge it; with a publisher wired under the pure phase strategy the
        // repo-root phase-verify gate is suppressed and the phase archives.
        git_ok(
            &f.repo,
            &[
                "merge",
                "--no-ff",
                "-m",
                "merge phase",
                "flywheel/phase/01-phase",
            ],
        );
        let completion = |_: &Path, _: &[Task]| {};
        let sweep = ArchiveSweep {
            phase_completion: Some(&completion),
            ..sweep
        };
        let moved = archive_phases(&f.tasks_dir, &f.store, Some(&f.claims), &log, &sweep).unwrap();
        assert_eq!(moved.len(), 1);
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.starts_with("Archived phase: "))
        );
        assert!(format!("{sweep:?}").contains("phase_completion: true"));
    }

    #[test]
    fn archive_phases_wrapper_keeps_phase_verify_armed_without_a_publisher() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        let (log, lines) = capture_log();
        let policy = WorkPolicy {
            phase_verify: Some("exit 9".into()),
            ..WorkPolicy::default()
        };
        let sweep = ArchiveSweep {
            repo_root: Some(&f.repo),
            policy: Some(&policy),
            ..ArchiveSweep::default()
        };
        let moved = archive_phases(&f.tasks_dir, &f.store, None, &log, &sweep).unwrap();
        assert!(moved.is_empty());
        assert!(lines.lock().unwrap()[0].contains("phase-verify command"));
    }
}
