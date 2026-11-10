//! The workflow pieces the loop depends on: the latest-lifecycle read, the stranded-run query, the
//! retry / turn defaults, the landed-only prerequisite predicate (spec
//! 00086, D-4), and the git-backed phase-prerequisite reachability hold
//! (spec 00079, #7).

use crate::claims::{ClaimStore, ClaimStoreError, RESOLUTION_ATTRIBUTIONS, STOP_RESOLVED};
use crate::rollup::{PrerequisiteReachabilityHold, TaskState, TaskStatusRow};
use chrono::{DateTime, Utc};
use flywheel_core::events::{DomainEventKind, DomainEventPayload};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{HarnessStore, LifecycleFilter, LifecycleStore, StoreError};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;

/// Default agent turn cap per iteration (`flywheel_core.workflow`).
pub const DEFAULT_MAX_TURNS: u32 = 500;
/// Default harness retry budget after `failed_validation`.
pub const DEFAULT_MAX_RETRIES: u32 = 1;

/// The statuses a lifecycle is stranded in when no worker owns it.
pub const STRANDED_STATUSES: [Status; 2] = [Status::Running, Status::Validating];

/// The most recent lifecycle for a task, as the pull-based CLI reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestLifecycleRow {
    pub run_id: String,
    pub status: Status,
    pub error: String,
    pub blocked_requires_json: Option<String>,
    pub awaiting_manual_ordinal: Option<u32>,
}

/// `(run_id, status, error, blocked_requires_json, awaiting_manual_ordinal)`
/// of the most recent lifecycle for `task_id`, or `None`.
///
/// The by-task-id lookup goes through the public `list_lifecycles` surface:
/// the most-recently-updated lifecycle is the first element of the
/// `(updated_at DESC, run_id DESC)` ordered result on every backend.
pub fn latest_lifecycle_row<S: LifecycleStore + ?Sized>(
    store: &S,
    task_id: &str,
) -> Result<Option<LatestLifecycleRow>, StoreError> {
    let lifecycles = store.list_lifecycles(&LifecycleFilter::default().with_task_id(task_id))?;
    Ok(lifecycles.into_iter().next().map(|lc| LatestLifecycleRow {
        run_id: lc.run_id,
        status: lc.status,
        error: lc.error,
        blocked_requires_json: lc.blocked_requires_json,
        awaiting_manual_ordinal: lc.awaiting_manual_ordinal,
    }))
}

/// Run ids whose lifecycle is mid-attempt (`running` / `validating`) with
/// no live worker, oldest-updated first (the historical contract), optionally
/// filtered to one task.
///
/// There is no process-level liveness check here: the caller uses this only
/// at boundaries where it knows which runs it owns (the claim is the
/// liveness signal the orchestrate loop uses).
pub fn stranded_run_ids<S: LifecycleStore + ?Sized>(
    store: &S,
    task_id: Option<&str>,
) -> Result<Vec<String>, StoreError> {
    let mut filter = LifecycleFilter::default().with_statuses(STRANDED_STATUSES);
    if let Some(task_id) = task_id {
        filter = filter.with_task_id(task_id);
    }
    let lifecycles = store.list_lifecycles(&filter)?;
    Ok(lifecycles.into_iter().rev().map(|lc| lc.run_id).collect())
}

// --- Landed-only prerequisite satisfaction (spec 00086, D-4) -------------------------

/// A failed read of the records the landed-only predicate consults.
#[derive(Debug, thiserror::Error)]
pub enum LandingRecordError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Claims(#[from] ClaimStoreError),
}

impl From<LandingRecordError> for super::OrchestrateError {
    fn from(err: LandingRecordError) -> Self {
        match err {
            LandingRecordError::Store(err) => Self::Store(err),
            LandingRecordError::Claims(err) => Self::Claims(err),
        }
    }
}

impl From<LandingRecordError> for super::ScopeError {
    fn from(err: LandingRecordError) -> Self {
        match err {
            LandingRecordError::Store(err) => Self::Store(err),
            LandingRecordError::Claims(err) => Self::Claims(err),
        }
    }
}

impl From<LandingRecordError> for crate::workflow::WorkflowError {
    fn from(err: LandingRecordError) -> Self {
        match err {
            LandingRecordError::Store(err) => Self::Store(err),
            LandingRecordError::Claims(err) => Self::Claims(err),
        }
    }
}

/// A DONE prerequisite whose work has not landed, so it does not satisfy
/// its dependents yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlandedPrerequisite {
    /// The prerequisite's task id.
    pub task_id: String,
    /// Its latest DONE run.
    pub run_id: String,
    /// The latest unresolved landing park's kind; empty while the landing
    /// is still in flight in a live worker.
    pub park_kind: String,
}

impl UnlandedPrerequisite {
    /// True while the prerequisite's landing is still in progress (no park
    /// yet, its claim held by a live worker).
    pub fn in_flight(&self) -> bool {
        self.park_kind.is_empty()
    }

    /// The operator-facing hold reason: the park kind and the `fw reland`
    /// hint, or that the landing is still in progress.
    pub fn describe(&self) -> String {
        if self.in_flight() {
            format!("{} (landing in progress)", self.task_id)
        } else {
            format!(
                "{} (park kind {}; run `fw reland {}`)",
                self.task_id, self.park_kind, self.task_id
            )
        }
    }
}

/// True when an attributed `stop-resolved` marker for `task_id` (the
/// operator's `resolve`, the archive probe, or `reland`) is at least as
/// recent as `run_id`'s latest landing park -- the recency rule the
/// re-driver, `status`, and `resolve` apply. A park appended after the
/// marker is a fresh strand.
pub fn resolved_since_latest_park(
    control: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    task_id: &str,
    run_id: &str,
) -> Result<bool, LandingRecordError> {
    let Some(resolved_at) = claims
        .list_subject_stop_events(task_id)?
        .into_iter()
        .filter(|event| {
            event.kind == STOP_RESOLVED
                && RESOLUTION_ATTRIBUTIONS.contains(&event.attribution.as_str())
        })
        .map(|event| event.occurred_at)
        .next_back()
    else {
        return Ok(false);
    };
    Ok(control
        .latest_domain_event_of_kind(run_id, DomainEventKind::LandingParked)?
        .is_some_and(|park| park.ts <= resolved_at))
}

/// The ONE landed-only prerequisite predicate: `Some` when `task_id`'s
/// latest DONE run does not (yet) satisfy its dependents.
///
/// Unsatisfied when that run has no `Landed` witness and either its latest
/// landing park has no attributed resolution at least as recent as it, or
/// it never parked and the task's claim is still held by a live worker
/// (the DONE-before-landed window between harness completion and submit).
/// `None` -- satisfied exactly as before -- when the run landed (including
/// `Landed{pr}` under the PR strategy, D-8), never parked and is not
/// mid-landing, or its park was resolved; also `None` when the task has no
/// DONE run at all (that is ordinary not-done, not this predicate's
/// concern).
pub fn unlanded_prerequisite(
    control: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    task_id: &str,
    now: DateTime<Utc>,
) -> Result<Option<UnlandedPrerequisite>, LandingRecordError> {
    let Some(done) = control
        .list_lifecycles(
            &LifecycleFilter::default()
                .with_task_id(task_id)
                .with_statuses([Status::Done]),
        )?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let run_id = done.run_id;
    if control
        .latest_domain_event_of_kind(&run_id, DomainEventKind::Landed)?
        .is_some()
    {
        return Ok(None);
    }
    if let Some(park) =
        control.latest_domain_event_of_kind(&run_id, DomainEventKind::LandingParked)?
    {
        if resolved_since_latest_park(control, claims, task_id, &run_id)? {
            return Ok(None);
        }
        let park_kind = match park.payload {
            DomainEventPayload::LandingParked(parked) if !parked.park_kind.is_empty() => {
                parked.park_kind
            }
            _ => "unknown".to_owned(),
        };
        return Ok(Some(UnlandedPrerequisite {
            task_id: task_id.to_owned(),
            run_id,
            park_kind,
        }));
    }
    let mid_landing = claims
        .load_claim(task_id)?
        .is_some_and(|claim| claim.lease_expires_at > now);
    Ok(mid_landing.then(|| UnlandedPrerequisite {
        task_id: task_id.to_owned(),
        run_id,
        park_kind: String::new(),
    }))
}

/// The unlanded prerequisites of `prerequisites`, in the given order.
///
/// Only a prerequisite `rows` lists as DONE is consulted: a not-done one
/// is already unsatisfied, and an unlisted (archived) one stays satisfied
/// -- a phase archives only after its landed predicate passed.
pub fn unlanded_prerequisites_of(
    control: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    rows: &[TaskStatusRow],
    prerequisites: &[String],
    now: DateTime<Utc>,
) -> Result<Vec<UnlandedPrerequisite>, LandingRecordError> {
    let mut unlanded = Vec::new();
    for prereq in prerequisites {
        let listed_done = rows
            .iter()
            .any(|row| &row.task_id == prereq && row.state == TaskState::Done);
        if !listed_done {
            continue;
        }
        if let Some(hold) = unlanded_prerequisite(control, claims, prereq, now)? {
            unlanded.push(hold);
        }
    }
    Ok(unlanded)
}

/// Every not-done listed task held behind at least one unlanded
/// prerequisite, keyed by the dependent, each with its unlanded
/// prerequisites in its prerequisite order. The row-level form status,
/// rollup and the hub render; selection and scope admission apply the
/// same predicate.
pub fn unlanded_prerequisite_holds(
    control: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    rows: &[TaskStatusRow],
    now: DateTime<Utc>,
) -> Result<BTreeMap<String, Vec<UnlandedPrerequisite>>, LandingRecordError> {
    let state_by_id: HashMap<&str, TaskState> = rows
        .iter()
        .map(|row| (row.task_id.as_str(), row.state))
        .collect();
    let mut verdicts: HashMap<String, Option<UnlandedPrerequisite>> = HashMap::new();
    let mut holds = BTreeMap::new();
    for row in rows {
        if row.state == TaskState::Done {
            continue;
        }
        let mut unlanded = Vec::new();
        for prereq in &row.prerequisites {
            if state_by_id.get(prereq.as_str()) != Some(&TaskState::Done) {
                continue;
            }
            let verdict = match verdicts.get(prereq) {
                Some(verdict) => verdict.clone(),
                None => {
                    let verdict = unlanded_prerequisite(control, claims, prereq, now)?;
                    verdicts.insert(prereq.clone(), verdict.clone());
                    verdict
                }
            };
            if let Some(hold) = verdict {
                unlanded.push(hold);
            }
        }
        if !unlanded.is_empty() {
            holds.insert(row.task_id.clone(), unlanded);
        }
    }
    Ok(holds)
}

/// Run `git -C <repo_root> <args>` and return `(exit_code, stdout)`.
/// Failure here is data (the probe degrades to "not reachable") rather than
/// an operator-actionable error, so stderr is discarded.
fn git_capture(repo_root: &Path, args: &[&str]) -> (i32, String) {
    match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
    {
        Ok(output) => (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        ),
        Err(_) => (-1, String::new()),
    }
}

/// The phase directory name a file-backed row belongs to, or `""`.
///
/// Mirrors the phase key the archive sweep derives from `active/<phase>`:
/// the task file's parent directory name; a pathless row (external source)
/// has no phase directory, so the reachability hold does not apply.
fn phase_of_row(row: &TaskStatusRow) -> String {
    row.task_file
        .as_deref()
        .and_then(|path| path.parent())
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// True when any `flywheel/phase/<phase>` integration branch exists.
fn phase_branches_present(repo_root: &Path) -> bool {
    let (rc, out) = git_capture(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/flywheel/phase/",
        ],
    );
    rc == 0 && !out.trim().is_empty()
}

/// Resolve the phase integration branch tip commit, or `None` when the ref
/// does not exist.
fn resolve_phase_branch_tip(repo_root: &Path, phase_branch: &str) -> Option<String> {
    let reference = format!("refs/heads/{phase_branch}");
    let (rc, out) = git_capture(repo_root, &["rev-parse", "--verify", "--quiet", &reference]);
    if rc != 0 {
        return None;
    }
    let tip = out.trim();
    (!tip.is_empty()).then(|| tip.to_owned())
}

/// True when `ancestor` is reachable from `rev`. Non-zero (including an
/// unresolvable revision) reads as "not an ancestor" so an unresolvable base
/// fails closed.
fn is_ancestor(repo_root: &Path, ancestor: &str, rev: &str) -> bool {
    let (rc, _) = git_capture(repo_root, &["merge-base", "--is-ancestor", ancestor, rev]);
    rc == 0
}

/// Dependents to withhold because a DONE cross-phase prerequisite's landed
/// work is not yet reachable from the base they would branch from.
///
/// The subtractive complement of `satisfied_prerequisites_from_store`: that
/// extends satisfaction to unlisted store-DONE ids; this removes it from a
/// listed prerequisite that is DONE but whose phase has not merged. Under
/// the phase strategy each phase lands onto `flywheel/phase/<phase>` and a
/// dependent in another phase branches from the true base, which cannot see
/// that work until the phase PR merges -- so the dependent is held (never
/// parked/failed) until reachability holds.
///
/// Returns an empty map (a total no-op) when `repo_root`/`true_base` is
/// absent or no `flywheel/phase/*` branch exists. A hold is recorded only
/// for a listed prerequisite in a *different* phase than the dependent,
/// classified DONE, whose phase branch tip is not an ancestor of
/// `true_base`. A prerequisite whose phase branch no longer resolves is
/// reachable and never holds; so is an unlisted one.
pub fn reachability_held_prerequisites(
    rows: &[TaskStatusRow],
    repo_root: Option<&Path>,
    true_base: Option<&str>,
) -> BTreeMap<String, PrerequisiteReachabilityHold> {
    let (Some(root), Some(base)) = (repo_root, true_base) else {
        return BTreeMap::new();
    };
    if rows.is_empty() || !phase_branches_present(root) {
        return BTreeMap::new();
    }
    let phase_by_id: HashMap<&str, String> = rows
        .iter()
        .map(|row| (row.task_id.as_str(), phase_of_row(row)))
        .collect();
    let state_by_id: HashMap<&str, TaskState> = rows
        .iter()
        .map(|row| (row.task_id.as_str(), row.state))
        .collect();
    let mut tip_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut ancestor_cache: HashMap<String, bool> = HashMap::new();

    let mut holds = BTreeMap::new();
    for row in rows {
        let dep_phase = &phase_by_id[row.task_id.as_str()];
        if dep_phase.is_empty() {
            continue;
        }
        let mut held_by: Vec<String> = Vec::new();
        let mut blocking_phase = String::new();
        for prereq_id in &row.prerequisites {
            let Some(prereq_phase) = phase_by_id.get(prereq_id.as_str()) else {
                continue;
            };
            if prereq_phase.is_empty() || prereq_phase == dep_phase {
                continue;
            }
            if state_by_id.get(prereq_id.as_str()) != Some(&TaskState::Done) {
                continue;
            }
            let tip = tip_cache
                .entry(prereq_phase.clone())
                .or_insert_with(|| {
                    resolve_phase_branch_tip(root, &format!("flywheel/phase/{prereq_phase}"))
                })
                .clone();
            let Some(tip) = tip else {
                continue;
            };
            let reachable = *ancestor_cache
                .entry(tip.clone())
                .or_insert_with(|| is_ancestor(root, &tip, base));
            if reachable {
                continue;
            }
            held_by.push(prereq_id.clone());
            if blocking_phase.is_empty() {
                blocking_phase = prereq_phase.clone();
            }
        }
        if !held_by.is_empty() {
            holds.insert(
                row.task_id.clone(),
                PrerequisiteReachabilityHold {
                    task_id: row.task_id.clone(),
                    blocking_phase,
                    held_by,
                },
            );
        }
    }
    holds
}
