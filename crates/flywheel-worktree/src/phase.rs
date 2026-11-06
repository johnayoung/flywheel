//! Phase bookkeeping: the per-phase integration-branch landing strategy
//! (spec 00079), the `refs/flywheel/loop-base/<phase>` base-capture refs,
//! and the archive sweep that moves a completed `active/<phase>` to
//! `archive/` once every exit gate passes.
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
    Logger, git,
    merge_lock,
};
use chrono::{DateTime, Utc};
use flywheel_core::events::{DomainEventKind, DomainEventPayload, LANDING_STRAND_KINDS};
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
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// The store surface the sweep reads: DONE-ness and the latest run by task
/// id, plus the run's domain events for the `Landed` receipt fast path.
pub trait ArchiveStore: LifecycleStore + DomainEventStore {}

impl<T: LifecycleStore + DomainEventStore + ?Sized> ArchiveStore for T {}

/// Everything beyond the tasks dir and store that arms an archive gate.
/// Every field defaults to unset, which preserves the legacy DONE-only
/// archival contract.
#[derive(Clone, Copy, Default)]
pub struct ArchiveOptions<'a> {
    /// Enables the loop-path gate, the landed predicate, the phase-verify
    /// gate, and the archival-move commit.
    pub repo_root: Option<&'a Path>,
    pub log: Option<&'a Logger>,
    /// The phase-exit integration gate (spec 00035): a shell command run in
    /// `repo_root` once a phase is eligible; non-zero leaves it active.
    pub phase_verify: Option<&'a str>,
    /// Arms the per-task landed predicate (spec 00077) with `repo_root`.
    pub landing_base: Option<&'a str>,
    /// The stop-event ledger: stamps `stop-resolved` markers on archive and
    /// surfaces indeterminate-landing strands.
    pub claims: Option<&'a dyn ClaimStore>,
    /// The marker clock; wall-clock UTC when unset.
    pub now: Option<&'a (dyn Fn() -> DateTime<Utc> + 'a)>,
}

impl std::fmt::Debug for ArchiveOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveOptions")
            .field("repo_root", &self.repo_root)
            .field("phase_verify", &self.phase_verify)
            .field("landing_base", &self.landing_base)
            .field("claims", &self.claims.is_some())
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
/// 3. the loop-path gate (`repo_root`): a diff vs the recorded base that
///    hits a watched signal needs a DONE `in-loop-verification` task or a
///    valid `loop-path-exempt.md`;
/// 4. the phase-verify gate (`repo_root` + `phase_verify`);
/// 5. the destination: `archive/<phase>`, or for a recurring phase name
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
                    && has_operator_resolution(claims, store, &task.id)?
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

/// The worker-side archive sweep: runs [`archive_completed_phases`] with
/// the policy's phase-exit verify gate armed, and announces each archived
/// phase.
pub fn archive_phases(
    tasks_dir: &Path,
    store: &dyn ArchiveStore,
    claims: Option<&dyn ClaimStore>,
    log: &Logger,
    sweep: &ArchiveSweep<'_>,
) -> Result<Vec<PathBuf>, ArchiveError> {
    let options = ArchiveOptions {
        repo_root: sweep.repo_root,
        log: Some(log),
        phase_verify: sweep.policy.and_then(|p| p.phase_verify.as_deref()),
        landing_base: sweep.landing_base,
        claims,
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
    pub policy: Option<&'a WorkPolicy>,
}

impl std::fmt::Debug for ArchiveSweep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveSweep")
            .field("repo_root", &self.repo_root)
            .field("landing_base", &self.landing_base)
            .field("policy", &self.policy.is_some())
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
/// `resolve` verb deliberately abandoning a strand) that no landing strand
/// postdates. Read off the record's own `attribution` column, never parsed
/// from the detail prose. A landing-strand park on the task's latest DONE run
/// appended after the resolution (the landing re-driver re-parking the run)
/// is a fresh strand -- the rule `status` and `resolve` apply -- so the
/// stale marker no longer exempts the task from the landing probe.
fn has_operator_resolution(
    claims: &dyn ClaimStore,
    store: &dyn ArchiveStore,
    task_id: &str,
) -> Result<bool, ArchiveError> {
    let events = claims.list_subject_stop_events(task_id)?;
    let Some(latest) = events.last() else {
        return Ok(false);
    };
    if latest.kind != STOP_RESOLVED || latest.attribution != RESOLUTION_ATTRIBUTION_OPERATOR {
        return Ok(false);
    }
    let filter = LifecycleFilter::default()
        .with_task_id(task_id)
        .with_statuses([Status::Done]);
    let Some(latest_done) = store.list_lifecycles(&filter)?.into_iter().next() else {
        return Ok(true);
    };
    let newer_strand = store
        .latest_domain_event_of_kind(&latest_done.run_id, DomainEventKind::LandingParked)?
        .is_some_and(|event| match &event.payload {
            DomainEventPayload::LandingParked(parked) => {
                LANDING_STRAND_KINDS.contains(&parked.park_kind.as_str())
                    && event.ts > latest.occurred_at
            }
            _ => false,
        });
    Ok(!newer_strand)
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
    use std::sync::Arc;
    use crate::submitter::test_support::{
        commit, git_ok, init_repo, is_ancestor as git_is_ancestor, rev,
    };
    use flywheel_core::events::{
        DomainEvent, LANDING_STRATEGY_MERGE, Landed, PARK_KIND_DIVERGENT_BASE,
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

    fn capture_log() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: Logger = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
        (log, lines)
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

    // --- mutation-survey kills (loop-base refs, sweep gates, destinations) -------------

    fn write_task_json(path: &Path, task_id: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            serde_json::json!({
                "id": task_id,
                "goal": "g",
                "graders": [{"type": "command", "run": "true"}],
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn an_empty_loop_base_dotfile_falls_back_to_the_ref() {
        let f = Fixture::new();
        let phase_dir = f.phase("01-phase");
        assert!(write_phase_base_if_missing(&f.repo, &phase_dir));
        let head = rev(&f.repo, "HEAD");
        for blank in ["", " \n"] {
            fs::write(phase_dir.join(LOOP_BASE_FILENAME), blank).unwrap();
            assert_eq!(
                read_phase_base(&phase_dir, Some(&f.repo)).as_deref(),
                Some(head.as_str())
            );
            assert_eq!(read_phase_base(&phase_dir, None), None);
        }
    }

    #[test]
    fn a_phase_name_git_rejects_as_a_ref_records_no_base() {
        let f = Fixture::new();
        let phase_dir = f.phase("bad..name");
        assert!(!write_phase_base_if_missing(&f.repo, &phase_dir));
        let (log, lines) = capture_log();
        let lock_path = f.repo.join(".flywheel").join(".merge.lock");
        assert_eq!(
            record_phase_bases(&f.repo, &f.tasks_dir, &lock_path, &log).unwrap(),
            0
        );
        assert!(lines.lock().unwrap().is_empty());
    }

    #[test]
    fn record_phase_bases_waits_for_the_merge_lock() {
        let f = Fixture::new();
        let phase_dir = f.phase("01-phase");
        let lock_path = f.repo.join(".flywheel").join(".merge.lock");
        let held = merge_lock(&lock_path).unwrap();
        let (repo, tasks_dir, thread_lock) = (f.repo.clone(), f.tasks_dir.clone(), lock_path);
        let recorder = std::thread::spawn(move || {
            let (log, _) = capture_log();
            record_phase_bases(&repo, &tasks_dir, &thread_lock, &log).unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(
            read_phase_base(&phase_dir, Some(&f.repo)),
            None,
            "a base was recorded while another worker held the merge lock"
        );
        drop(held);
        assert_eq!(recorder.join().unwrap(), 1);
        assert!(read_phase_base(&phase_dir, Some(&f.repo)).is_some());
    }

    #[test]
    fn materialize_keeps_a_committed_dotfile_and_retries_a_failed_write() {
        let f = Fixture::new();
        let head = rev(&f.repo, "HEAD");
        let reference = format!("{LOOP_BASE_REF_PREFIX}01-phase");
        let dest = f.tasks_dir.join("archive").join("01-phase");
        fs::create_dir_all(&dest).unwrap();

        // The dotfile cannot be written (a directory squats its path): the
        // ref survives so a later sweep can retry.
        git_ok(&f.repo, &["update-ref", &reference, &head]);
        fs::create_dir(dest.join(LOOP_BASE_FILENAME)).unwrap();
        materialize_loop_base(&f.repo, "01-phase", &dest);
        assert_eq!(rev(&f.repo, &reference), head);

        // Writable again: the dotfile carries the SHA plus a newline and the
        // ref is dropped.
        fs::remove_dir(dest.join(LOOP_BASE_FILENAME)).unwrap();
        materialize_loop_base(&f.repo, "01-phase", &dest);
        assert_eq!(
            fs::read_to_string(dest.join(LOOP_BASE_FILENAME)).unwrap(),
            format!("{head}\n")
        );
        assert!(!branch_or_ref_exists(&f.repo, &reference));

        // A phase that already carries a committed dotfile keeps it verbatim.
        fs::write(dest.join(LOOP_BASE_FILENAME), "legacy-sha\n").unwrap();
        git_ok(&f.repo, &["update-ref", &reference, &head]);
        materialize_loop_base(&f.repo, "01-phase", &dest);
        assert_eq!(
            fs::read_to_string(dest.join(LOOP_BASE_FILENAME)).unwrap(),
            "legacy-sha\n"
        );
    }

    fn branch_or_ref_exists(repo: &Path, reference: &str) -> bool {
        git(repo, &["rev-parse", "--verify", "--quiet", reference])
            .unwrap()
            .ok()
    }

    #[test]
    fn a_phase_with_any_unfinished_task_stays_active() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.task("01-phase", "t2", &[]);
        f.done("t1", "run-t1");
        f.running("t2", "run-t2");
        let (moved, _) = f.sweep(ArchiveOptions::default());
        assert!(moved.is_empty());
        assert!(f.tasks_dir.join("active").join("01-phase").is_dir());
    }

    #[test]
    fn a_determinate_strand_is_logged_but_not_surfaced_as_indeterminate() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        git_ok(&f.repo, &["branch", "flywheel/01-phase/t1", "main"]);
        git_ok(&f.repo, &["checkout", "-q", "flywheel/01-phase/t1"]);
        commit(&f.repo, "work.txt", "w", "task work");
        git_ok(&f.repo, &["checkout", "-q", "main"]);
        let (moved, logged) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            claims: Some(&f.claims),
            ..ArchiveOptions::default()
        });
        assert!(moved.is_empty());
        assert!(logged[0].contains("is not landed"), "{logged:?}");
        assert!(f.claims.list_subject_stop_events("t1").unwrap().is_empty());
    }

    #[test]
    fn a_partly_absorbed_branch_is_not_landed() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        git_ok(&f.repo, &["checkout", "-q", "-b", "flywheel/01-phase/t1"]);
        let first = commit(&f.repo, "a.txt", "a", "first half");
        commit(&f.repo, "b.txt", "b", "second half");
        git_ok(&f.repo, &["checkout", "-q", "main"]);
        // Diverge first so the pick is a patch-identical copy, not `first`.
        commit(&f.repo, "main.txt", "m", "main moves on");
        git_ok(&f.repo, &["cherry-pick", &first]);
        assert!(!git_is_ancestor(&f.repo, &first, "main"));
        let (moved, logged) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            ..ArchiveOptions::default()
        });
        assert!(moved.is_empty());
        assert!(logged[0].contains("is not landed"), "{logged:?}");
    }

    fn armed_landing(f: &Fixture, with_claims: bool) -> ArchiveOptions<'_> {
        ArchiveOptions {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            claims: with_claims.then_some(&f.claims as &dyn ClaimStore),
            ..ArchiveOptions::default()
        }
    }

    #[test]
    fn the_landed_receipt_is_read_from_the_latest_run_only() {
        // A retry landed: the newest run's receipt counts, the older run
        // carrying none does not mask it.
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-a");
        std::thread::sleep(std::time::Duration::from_millis(5));
        f.done("t1", "run-b");
        f.landed_receipt("run-b");
        let (moved, logged) = f.sweep(armed_landing(&f, false));
        assert_eq!(moved.len(), 1, "{logged:?}");

        // A domain event that is not a `Landed` receipt proves nothing.
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        let lc = f.store.load_lifecycle("run-t1").unwrap().unwrap();
        let parked = DomainEvent::new(
            "run-t1",
            Utc::now(),
            DomainEventPayload::LandingParked(flywheel_core::events::LandingParked {
                park_kind: PARK_KIND_DIVERGENT_BASE.into(),
                ..flywheel_core::events::LandingParked::default()
            }),
        );
        f.store.append_domain_event(&parked, lc.version).unwrap();
        let (moved, logged) = f.sweep(armed_landing(&f, false));
        assert!(moved.is_empty());
        assert!(logged[0].contains("cannot be determined"), "{logged:?}");
    }

    #[test]
    fn only_the_latest_operator_resolution_skips_the_landing_probe() {
        // A resolution stamped by archival or the probe is not the operator
        // abandoning the strand.
        for attribution in ["", RESOLUTION_ATTRIBUTION_PROBE] {
            let f = Fixture::new();
            f.task("01-phase", "t1", &[]);
            f.done("t1", "run-t1");
            f.claims
                .record_stop_event(STOP_RESOLVED, "t1", "earlier", Utc::now(), attribution)
                .unwrap();
            let (moved, logged) = f.sweep(armed_landing(&f, true));
            assert!(moved.is_empty(), "attribution {attribution:?}");
            assert!(logged[0].contains("cannot be determined"), "{logged:?}");
        }
        // An operator resolution superseded by a newer strand no longer
        // applies.
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        f.claims
            .record_stop_event(
                STOP_RESOLVED,
                "t1",
                "abandoned",
                Utc::now(),
                RESOLUTION_ATTRIBUTION_OPERATOR,
            )
            .unwrap();
        f.claims
            .record_stop_event(PARK_KIND_DIVERGENT_BASE, "t1", "parked", Utc::now(), "")
            .unwrap();
        let (moved, logged) = f.sweep(armed_landing(&f, true));
        assert!(moved.is_empty());
        assert!(logged[0].contains("cannot be determined"), "{logged:?}");
    }

    /// Append a `LandingParked` of `park_kind` at `ts` to `run_id`'s ledger.
    fn park(f: &Fixture, run_id: &str, park_kind: &str, ts: DateTime<Utc>) {
        let lc = f.store.load_lifecycle(run_id).unwrap().unwrap();
        let parked = DomainEvent::new(
            run_id,
            ts,
            DomainEventPayload::LandingParked(flywheel_core::events::LandingParked {
                park_kind: park_kind.into(),
                ..flywheel_core::events::LandingParked::default()
            }),
        );
        f.store.append_domain_event(&parked, lc.version).unwrap();
    }

    #[test]
    fn an_operator_resolution_older_than_the_latest_landing_park_does_not_skip_the_probe() {
        // `status` and `resolve` treat a landing park appended after the
        // operator's resolution (the re-driver re-parking the run) as a fresh
        // strand; the sweep must not archive the phase on the stale marker.
        let t0 = Utc::now();
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        park(&f, "run-t1", PARK_KIND_DIVERGENT_BASE, t0);
        f.claims
            .record_stop_event(
                STOP_RESOLVED,
                "t1",
                "abandoned",
                t0 + chrono::Duration::seconds(1),
                RESOLUTION_ATTRIBUTION_OPERATOR,
            )
            .unwrap();
        park(
            &f,
            "run-t1",
            PARK_KIND_DIVERGENT_BASE,
            t0 + chrono::Duration::seconds(2),
        );
        let (moved, logged) = f.sweep(armed_landing(&f, true));
        assert!(moved.is_empty(), "{logged:?}");
        assert!(logged[0].contains("cannot be determined"), "{logged:?}");

        // The operator resolves the fresh strand: the phase archives.
        f.claims
            .record_stop_event(
                STOP_RESOLVED,
                "t1",
                "abandoned again",
                t0 + chrono::Duration::seconds(3),
                RESOLUTION_ATTRIBUTION_OPERATOR,
            )
            .unwrap();
        let (moved, logged) = f.sweep(armed_landing(&f, true));
        assert_eq!(moved.len(), 1, "{logged:?}");

        // A newer park that is not a landing strand (merge conflict) keeps
        // the plain operator-resolution contract.
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        f.claims
            .record_stop_event(
                STOP_RESOLVED,
                "t1",
                "abandoned",
                t0,
                RESOLUTION_ATTRIBUTION_OPERATOR,
            )
            .unwrap();
        park(
            &f,
            "run-t1",
            flywheel_core::events::PARK_KIND_MERGE_CONFLICT,
            t0 + chrono::Duration::seconds(1),
        );
        let (moved, logged) = f.sweep(armed_landing(&f, true));
        assert_eq!(moved.len(), 1, "{logged:?}");

        // Only the latest DONE run's park counts: a superseded run's newer
        // park does not re-open the strand.
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-a");
        park(
            &f,
            "run-a",
            PARK_KIND_DIVERGENT_BASE,
            t0 + chrono::Duration::seconds(5),
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        f.done("t1", "run-b");
        park(&f, "run-b", PARK_KIND_DIVERGENT_BASE, t0);
        f.claims
            .record_stop_event(
                STOP_RESOLVED,
                "t1",
                "abandoned",
                t0 + chrono::Duration::seconds(1),
                RESOLUTION_ATTRIBUTION_OPERATOR,
            )
            .unwrap();
        let (moved, logged) = f.sweep(armed_landing(&f, true));
        assert_eq!(moved.len(), 1, "{logged:?}");
    }

    #[test]
    fn an_already_resolved_stop_is_not_stamped_again_on_archive() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        f.claims
            .record_stop_event(STOP_DANGLING_PREREQUISITE, "t1", "stranded", Utc::now(), "")
            .unwrap();
        f.claims
            .record_stop_event(STOP_RESOLVED, "t1", "handled", Utc::now(), "")
            .unwrap();
        let (moved, _) = f.sweep(ArchiveOptions {
            claims: Some(&f.claims),
            ..ArchiveOptions::default()
        });
        assert_eq!(moved.len(), 1);
        assert_eq!(f.claims.list_subject_stop_events("t1").unwrap().len(), 2);
    }

    #[test]
    fn phase_verify_fails_closed_on_a_signal_and_runs_in_the_repo_root() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        let (moved, logged) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            phase_verify: Some("kill -9 $$"),
            ..ArchiveOptions::default()
        });
        assert!(moved.is_empty());
        assert!(logged[0].contains("(exit signal)"), "{logged:?}");

        // The refusal quotes the output's tail, not its head.
        let noisy = "printf START; i=0; while [ $i -lt 700 ]; do printf xyz; i=$((i+1)); done; \
                     printf END; exit 3";
        let (moved, logged) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            phase_verify: Some(noisy),
            ..ArchiveOptions::default()
        });
        assert!(moved.is_empty());
        let tail = logged[0].split_once("(exit 3): ").unwrap().1;
        assert_eq!(tail.chars().count(), 2000);
        assert!(tail.ends_with("xyzEND"), "{tail}");
        assert!(!tail.contains("START"), "{tail}");

        // The command runs against the repo root.
        fs::write(f.repo.join("verify-marker"), "").unwrap();
        let (moved, logged) = f.sweep(ArchiveOptions {
            repo_root: Some(&f.repo),
            phase_verify: Some("test -f verify-marker"),
            ..ArchiveOptions::default()
        });
        assert_eq!(moved.len(), 1, "{logged:?}");
    }

    #[test]
    fn a_resurrected_duplicate_is_matched_by_task_id_or_by_file_stem() {
        let f = Fixture::new();
        let archive = f.tasks_dir.join("archive");
        let active = f.tasks_dir.join("active");
        // (phase, archived file, archived id, active file, active id)
        let cases = [
            ("p1", "old-name", "t1", "t1", "t1"),
            ("p2", "t2", "renamed", "t2", "t2-new"),
            ("p3", "t3", "t3", "new-file", "t3"),
        ];
        for (phase, archived_file, archived_id, active_file, active_id) in cases {
            write_task_json(
                &archive.join(phase).join(format!("{archived_file}.json")),
                archived_id,
            );
            write_task_json(
                &active.join(phase).join(format!("{active_file}.json")),
                active_id,
            );
            f.done(active_id, &format!("run-{phase}"));
        }
        let (moved, logged) = f.sweep(ArchiveOptions::default());
        assert!(moved.is_empty(), "{moved:?}");
        assert_eq!(logged.len(), 3, "{logged:?}");
        for (line, phase) in logged.iter().zip(["p1", "p2", "p3"]) {
            assert!(
                line.starts_with(&format!("Refusing to archive phase {phase}: task ")),
                "{line}"
            );
            assert!(active.join(phase).is_dir());
        }
    }

    #[test]
    fn a_non_directory_generation_name_is_skipped_not_scanned_or_reused() {
        let f = Fixture::new();
        let archive = f.tasks_dir.join("archive");
        write_task_json(&archive.join("auto").join("a.json"), "a");
        fs::write(archive.join("auto-2"), "not a phase").unwrap();
        f.task("auto", "b", &[]);
        f.done("b", "run-b");
        let (moved, logged) = f.sweep(ArchiveOptions::default());
        assert_eq!(moved, vec![archive.join("auto-3")], "{logged:?}");
        assert!(archive.join("auto-2").is_file());
    }

    #[test]
    fn archive_phases_wrapper_arms_the_landed_predicate() {
        let f = Fixture::new();
        f.task("01-phase", "t1", &[]);
        f.done("t1", "run-t1");
        let (log, lines) = capture_log();
        let sweep = ArchiveSweep {
            repo_root: Some(&f.repo),
            landing_base: Some("main"),
            ..ArchiveSweep::default()
        };
        let moved = archive_phases(&f.tasks_dir, &f.store, None, &log, &sweep).unwrap();
        assert!(moved.is_empty());
        assert!(lines.lock().unwrap()[0].contains("cannot be determined"));
    }
}
