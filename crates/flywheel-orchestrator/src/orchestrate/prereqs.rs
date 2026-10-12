//! The pieces of the Python `_workflow` / `workflow` modules the loop
//! depends on: the latest-lifecycle read, the stranded-run query, the
//! retry / turn defaults, and the git-backed phase-prerequisite
//! reachability hold (spec 00079, #7).

use crate::rollup::{PrerequisiteReachabilityHold, TaskState, TaskStatusRow};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{LifecycleFilter, LifecycleStore, StoreError};
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
