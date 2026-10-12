//! Evidence-derived status rollup -- the read-only projection behind
//! `flywheel status --rollup`.
//!
//! This is a pure projection over state the store already holds. It
//! introduces no authoritative state of its own (consistent with
//! `docs/data-taxonomy.md`): every node's status is *computed* from
//! lifecycle state and grader receipts, never set by hand. That is the whole
//! point -- a rollup whose "verified" can only be earned by a green grader,
//! not asserted by an operator dragging a card.
//!
//! The honest distinction this surface draws, and a plain status table does
//! not: a task that reached `done` with passing graders is `verified`; a
//! task that reached `done` with *no* graders is `accepted` -- the agent's
//! own claim, structurally unverifiable. The rollup never lets the second
//! masquerade as the first.
//!
//! The per-task classification the rollup consumes ([`TaskState`],
//! [`TaskStatusRow`], [`status_rows_for_items`]) lives here as well: it is
//! the `status` verb's row shape, derived from the latest lifecycle of each
//! work item through the public cross-task store seam.

use crate::sources::WorkItem;
use flywheel_core::lifecycle::{Lifecycle, Status};
use flywheel_core::store::{GraderResultStore, LifecycleFilter, LifecycleStore, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// A task's status as the rollup *derives* it from evidence.
///
/// The declaration order of the variants is the report's column legend
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollupStatus {
    /// Done, every recorded grader passed.
    Verified,
    /// Done, but no graders ran -- agent claim, unverified.
    Accepted,
    /// An active lifecycle exists.
    InProgress,
    /// Interrupted or parked on a manual gate.
    Blocked,
    /// Last lifecycle failed (retryable or terminal).
    Failed,
    /// Never started; a prerequisite is not done.
    BlockedByPrereq,
    /// Never started; prerequisites are satisfied.
    NotStarted,
}

impl RollupStatus {
    /// Legend order.
    pub const ALL: [RollupStatus; 7] = [
        RollupStatus::Verified,
        RollupStatus::Accepted,
        RollupStatus::InProgress,
        RollupStatus::Blocked,
        RollupStatus::Failed,
        RollupStatus::BlockedByPrereq,
        RollupStatus::NotStarted,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            RollupStatus::Verified => "verified",
            RollupStatus::Accepted => "accepted",
            RollupStatus::InProgress => "in_progress",
            RollupStatus::Blocked => "blocked",
            RollupStatus::Failed => "failed",
            RollupStatus::BlockedByPrereq => "blocked_by_prereq",
            RollupStatus::NotStarted => "not_started",
        }
    }

    /// Statuses that count as "this task reached done" for prerequisite
    /// resolution. Both verified and accepted reached the DONE lifecycle
    /// state; the rollup only distinguishes them on *evidence quality*, not
    /// on completion.
    pub const fn is_done(self) -> bool {
        matches!(self, RollupStatus::Verified | RollupStatus::Accepted)
    }
}

impl fmt::Display for RollupStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Width of the widest status label (`blocked_by_prereq`), so the status
/// column aligns the way the plain `status` renderer pads its own state
/// column.
const STATUS_WIDTH: usize = 17;

// --- Status rows ----------------------------------------------------------------

/// Task-level status derived from the latest lifecycle, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Never attempted.
    Fresh,
    /// An active lifecycle exists.
    InProgress,
    /// The last lifecycle failed.
    Retryable,
    /// The last lifecycle paused for an operator.
    Interrupted,
    /// Parked on a manual gate.
    AwaitingApproval,
    /// At least one lifecycle reached DONE.
    Done,
}

impl TaskState {
    pub const fn as_str(self) -> &'static str {
        match self {
            TaskState::Fresh => "fresh",
            TaskState::InProgress => "in_progress",
            TaskState::Retryable => "retryable",
            TaskState::Interrupted => "interrupted",
            TaskState::AwaitingApproval => "awaiting_approval",
            TaskState::Done => "done",
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-task status snapshot used by `status` and the rollup.
///
/// `task_file` is the on-disk path for file-backed sources and `None`
/// otherwise -- path-deriving consumers treat `None` as "no file".
/// `prerequisites` is the task's dependency edges (other task ids that must
/// reach DONE first), an orchestration-layer concept carried on the row.
/// `source_ref` is the work source's opaque handle for the item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStatusRow {
    pub task_file: Option<PathBuf>,
    pub task_id: String,
    pub state: TaskState,
    pub latest_run_id: Option<String>,
    pub latest_status: Option<Status>,
    pub latest_error: String,
    pub blocked_requires: Option<String>,
    pub awaiting_manual_ordinal: Option<u32>,
    pub prerequisites: Vec<String>,
    pub source_ref: String,
}

impl TaskStatusRow {
    /// A never-attempted row with no file, no edges, and an empty source
    /// ref.
    pub fn fresh(task_id: impl Into<String>) -> Self {
        Self {
            task_file: None,
            task_id: task_id.into(),
            state: TaskState::Fresh,
            latest_run_id: None,
            latest_status: None,
            latest_error: String::new(),
            blocked_requires: None,
            awaiting_manual_ordinal: None,
            prerequisites: Vec::new(),
            source_ref: String::new(),
        }
    }
}

/// Classify `task_id` from the lifecycles the store holds for it, newest
/// first (the `(updated_at DESC, run_id DESC)` order
/// [`LifecycleStore::list_lifecycles`] guarantees).
///
/// DONE wins whenever *any* lifecycle reached it; otherwise the latest
/// lifecycle's status decides. A persisted INTERNAL_ERROR is a dead-worker
/// strand (the harness's own loop never exits in that state), so it is
/// recoverable exactly like FAILED_VALIDATION.
fn classify_lifecycles(task_id: &str, lifecycles: &[Lifecycle]) -> TaskStatusRow {
    let Some(latest) = lifecycles.first() else {
        return TaskStatusRow::fresh(task_id);
    };
    let state = if lifecycles.iter().any(|lc| lc.status == Status::Done) {
        TaskState::Done
    } else {
        match latest.status {
            Status::Ready | Status::Running | Status::Validating => TaskState::InProgress,
            Status::AwaitingApproval => TaskState::AwaitingApproval,
            Status::Interrupted => TaskState::Interrupted,
            Status::Failed | Status::FailedValidation | Status::InternalError => {
                TaskState::Retryable
            }
            Status::Pending | Status::Done => TaskState::InProgress,
        }
    };
    TaskStatusRow {
        task_file: None,
        task_id: task_id.to_owned(),
        state,
        latest_run_id: Some(latest.run_id.clone()),
        latest_status: Some(latest.status),
        latest_error: latest.error.clone(),
        blocked_requires: latest.blocked_requires_json.clone(),
        awaiting_manual_ordinal: latest.awaiting_manual_ordinal,
        prerequisites: Vec::new(),
        source_ref: String::new(),
    }
}

/// Classify one task against `store` with a single cross-task read.
pub fn task_state<S: LifecycleStore + ?Sized>(
    store: &S,
    task_id: &str,
) -> Result<TaskStatusRow, StoreError> {
    let lifecycles = store.list_lifecycles(&LifecycleFilter::default().with_task_id(task_id))?;
    Ok(classify_lifecycles(task_id, &lifecycles))
}

/// Classify each work item's task against `store`, in item order.
///
/// Items come from any `WorkSource`; the row carries the item's
/// `source_ref` and (for file-backed sources) its `local_path` so
/// downstream consumers keep their path-derived behavior.
pub fn status_rows_for_items<S: LifecycleStore + ?Sized>(
    items: &[WorkItem],
    store: &S,
) -> Result<Vec<TaskStatusRow>, StoreError> {
    items
        .iter()
        .map(|item| {
            let mut row = task_state(store, &item.task.id)?;
            row.task_file = item.local_path.clone();
            row.prerequisites = item.prerequisites.clone();
            row.source_ref = item.source_ref.clone();
            Ok(row)
        })
        .collect()
}

/// Whether `store` holds a DONE lifecycle for `task_id`.
fn has_done_lifecycle<S: LifecycleStore + ?Sized>(
    store: &S,
    task_id: &str,
) -> Result<bool, StoreError> {
    let filter = LifecycleFilter::default()
        .with_statuses([Status::Done])
        .with_task_id(task_id);
    Ok(!store.list_lifecycles(&filter)?.is_empty())
}

/// The `archive/` sibling of the rows' `active/` tasks tree, if any.
///
/// Derived from the listed rows' own task-file paths: a directory-source row
/// lives at `<tasks>/active/<phase>/<id>.json`, so the first path with an
/// `active` ancestor names the tasks root. Non-directory sources have no
/// such shape and yield `None`, which keeps the store as the sole
/// authority.
fn archive_root_for_rows(rows: &[TaskStatusRow]) -> Option<PathBuf> {
    for row in rows {
        let Some(task_file) = &row.task_file else {
            continue;
        };
        for parent in task_file.ancestors().skip(1) {
            if parent.file_name().is_some_and(|name| name == "active") {
                let archive = parent.parent()?.join("archive");
                return archive.is_dir().then_some(archive);
            }
        }
    }
    None
}

/// Whether `archive/*/<task_id>.json` exists under `archive_root`.
fn archived_task_file_exists(archive_root: &Path, task_id: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(archive_root) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.path().join(format!("{task_id}.json")).is_file())
}

/// Prerequisite ids absent from `rows` yet DONE in `store`.
///
/// The store is the authoritative record of completion
/// (`docs/data-taxonomy.md`); the source listing is an input surface, not
/// the record. A prerequisite whose defining task has left the listing --
/// e.g. its phase archived, moving the task JSON out of `active/` -- is
/// still satisfied when its lifecycle reached [`Status::Done`].
///
/// Only ids that no listed row provides are consulted: a listed
/// prerequisite's DONE-ness is already folded into its row state. A DONE
/// lifecycle satisfies; so does an archived task file (cross-store
/// continuity): a task completed under an earlier store backend has no
/// lifecycle row in the policy-selected store, but its task JSON under
/// `archive/` is durable proof of verified completion, because a phase
/// archives only after every task passed the landed predicate and the
/// phase-exit gates. An id absent from BOTH the store and the archive stays
/// unsatisfied (fail closed).
pub fn satisfied_prerequisites_from_store<S: LifecycleStore + ?Sized>(
    rows: &[TaskStatusRow],
    store: &S,
) -> Result<BTreeSet<String>, StoreError> {
    let listed: BTreeSet<&str> = rows.iter().map(|row| row.task_id.as_str()).collect();
    let candidates: BTreeSet<&str> = rows
        .iter()
        .flat_map(|row| row.prerequisites.iter().map(String::as_str))
        .filter(|id| !listed.contains(id))
        .collect();
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let archive_root = archive_root_for_rows(rows);
    let mut satisfied = BTreeSet::new();
    for id in candidates {
        let done = has_done_lifecycle(store, id)?
            || archive_root
                .as_deref()
                .is_some_and(|root| archived_task_file_exists(root, id));
        if done {
            satisfied.insert(id.to_owned());
        }
    }
    Ok(satisfied)
}

/// A dependent held because a DONE prerequisite's landed work is not yet
/// reachable from the base the dependent would branch from (spec 00079,
/// #7).
///
/// `blocking_phase` names the prerequisite's phase whose integration branch
/// has not merged into the true base -- the phase whose PR must merge before
/// the dependent becomes claimable. `held_by` lists the DONE-but-unreachable
/// prerequisite ids driving the hold, in the dependent's prerequisite order.
/// Computed by the phase-strategy scheduler from git state; the rollup only
/// renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrerequisiteReachabilityHold {
    pub task_id: String,
    pub blocking_phase: String,
    pub held_by: Vec<String>,
}

// --- The projection --------------------------------------------------------------

/// One task's derived status plus the evidence that earned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRollup {
    pub task_id: String,
    pub phase: String,
    pub status: RollupStatus,
    pub passed_graders: usize,
    pub total_graders: usize,
    pub prerequisites: Vec<String>,
    pub unsatisfied_prerequisites: Vec<String>,
    pub detail: String,
}

/// All tasks under one phase, in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseRollup {
    pub name: String,
    pub tasks: Vec<TaskRollup>,
}

impl PhaseRollup {
    pub fn verified(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status == RollupStatus::Verified)
            .count()
    }
}

/// The full projection: phases in first-seen order plus status totals in
/// first-seen order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rollup {
    pub phases: Vec<PhaseRollup>,
    pub totals: Vec<(RollupStatus, usize)>,
}

impl Rollup {
    /// The count for `status` (zero when no task carries it).
    pub fn total(&self, status: RollupStatus) -> usize {
        self.totals
            .iter()
            .find(|(s, _)| *s == status)
            .map_or(0, |(_, n)| *n)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &TaskRollup> {
        self.phases.iter().flat_map(|p| p.tasks.iter())
    }
}

/// Group key for a row -- mirrors the plain `status` renderer.
///
/// File-backed rows group under their phase directory; external items (no
/// local path) group under their source ref, falling back to `"external"`.
fn phase_of(row: &TaskStatusRow) -> String {
    let dir_name = row
        .task_file
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty());
    if let Some(name) = dir_name {
        return name;
    }
    if !row.source_ref.is_empty() {
        return row.source_ref.clone();
    }
    "external".to_owned()
}

/// `(passed, total)` grader receipts for the verifying attempt of the most
/// recent DONE run, or `(0, 0)` when the run recorded none.
///
/// The verifying attempt is the highest-numbered attempt that recorded any
/// grader receipt: a DONE run's final attempt is the one whose graders all
/// passed. Reading the *latest* receipt-bearing attempt avoids counting an
/// earlier failed attempt's red graders against a task that has since
/// passed.
fn verification_evidence<S: LifecycleStore + GraderResultStore + ?Sized>(
    store: &S,
    task_id: &str,
) -> Result<(usize, usize), StoreError> {
    let filter = LifecycleFilter::default()
        .with_statuses([Status::Done])
        .with_task_id(task_id);
    let done = store.list_lifecycles(&filter)?;
    let Some(lc) = done.first() else {
        return Ok((0, 0));
    };
    let mut attempts: Vec<u32> = lc.attempts.iter().map(|a| a.number).collect();
    attempts.sort_unstable_by(|a, b| b.cmp(a));
    for number in attempts {
        let results = store.list_grader_results(&lc.run_id, number)?;
        if !results.is_empty() {
            let passed = results.iter().filter(|r| r.passed).count();
            return Ok((passed, results.len()));
        }
    }
    Ok((0, 0))
}

/// Map one status row to its base rollup status plus grader evidence.
///
/// The prerequisite refinement (NOT_STARTED -> BLOCKED_BY_PREREQ) happens in
/// a second pass in [`build_rollup`], once every task's status is known.
fn classify<S: LifecycleStore + GraderResultStore + ?Sized>(
    row: &TaskStatusRow,
    store: &S,
) -> Result<(RollupStatus, usize, usize, String), StoreError> {
    Ok(match row.state {
        TaskState::Done => {
            let (passed, total) = verification_evidence(store, &row.task_id)?;
            if total > 0 && passed == total {
                (RollupStatus::Verified, passed, total, String::new())
            } else {
                (
                    RollupStatus::Accepted,
                    passed,
                    total,
                    "done with no graders -- agent claim, unverified".to_owned(),
                )
            }
        }
        TaskState::InProgress => {
            let detail = row
                .latest_status
                .map_or_else(|| "active".to_owned(), |s| s.as_str().to_owned());
            (RollupStatus::InProgress, 0, 0, detail)
        }
        TaskState::AwaitingApproval => (
            RollupStatus::Blocked,
            0,
            0,
            "awaiting operator approval".to_owned(),
        ),
        TaskState::Interrupted => {
            let detail = if row.latest_error.is_empty() {
                "interrupted".to_owned()
            } else {
                row.latest_error.clone()
            };
            (RollupStatus::Blocked, 0, 0, detail)
        }
        TaskState::Retryable => (RollupStatus::Failed, 0, 0, row.latest_error.clone()),
        TaskState::Fresh => (RollupStatus::NotStarted, 0, 0, String::new()),
    })
}

/// Project status rows into a phase-grouped, evidence-derived rollup.
///
/// `reachability_holds` arms the phase-prerequisite reachability surface
/// (spec 00079, #7): under the phase strategy a not-started dependent whose
/// DONE prerequisite landed on an *unmerged* sibling phase is reported
/// `blocked_by_prereq` naming the blocking phase, not idle `not_started` --
/// the same visible-hold verdict the scheduler withholds it on. Pass an
/// empty map (what [`compute_rollup`] does) for the plain projection.
pub fn build_rollup<S: LifecycleStore + GraderResultStore + ?Sized>(
    rows: &[TaskStatusRow],
    store: &S,
    reachability_holds: &BTreeMap<String, PrerequisiteReachabilityHold>,
) -> Result<Rollup, StoreError> {
    // First pass: classify each task in isolation, recording its status so
    // the prerequisite pass can resolve dependencies by id.
    let mut classified = Vec::with_capacity(rows.len());
    let mut status_by_id: BTreeMap<&str, RollupStatus> = BTreeMap::new();
    for row in rows {
        let (status, passed, total, detail) = classify(row, store)?;
        classified.push((row, status, passed, total, detail));
        status_by_id.insert(&row.task_id, status);
    }

    // Second pass: a not-started task whose prerequisites have not all
    // reached done is blocked *by* those prerequisites, not merely idle. A
    // prerequisite is satisfied when a listed row classified it as done OR
    // -- for an id no listed row provides -- the store still carries its
    // DONE lifecycle (or its archived task file).
    let store_done = satisfied_prerequisites_from_store(rows, store)?;

    let mut phases: Vec<PhaseRollup> = Vec::new();
    let mut phase_index: BTreeMap<String, usize> = BTreeMap::new();
    let mut totals: Vec<(RollupStatus, usize)> = Vec::new();
    for (row, mut status, passed, total, mut detail) in classified {
        let mut unsatisfied: Vec<String> = Vec::new();
        if status == RollupStatus::NotStarted && !row.prerequisites.is_empty() {
            unsatisfied = row
                .prerequisites
                .iter()
                .filter(|id| {
                    !status_by_id.get(id.as_str()).is_some_and(|s| s.is_done())
                        && !store_done.contains(id.as_str())
                })
                .cloned()
                .collect();
            if !unsatisfied.is_empty() {
                status = RollupStatus::BlockedByPrereq;
                detail = format!("waiting on: {}", unsatisfied.join(", "));
            }
        }
        // A dependent whose prerequisites are all DONE but not yet reachable
        // is blocked by phase merge order, not by an incomplete
        // prerequisite. Gated on NOT_STARTED so a genuinely incomplete
        // prerequisite (handled above) keeps precedence.
        if status == RollupStatus::NotStarted
            && let Some(hold) = reachability_holds.get(&row.task_id)
        {
            status = RollupStatus::BlockedByPrereq;
            unsatisfied = hold.held_by.clone();
            detail = format!(
                "prerequisite landed but not reachable; waiting on phase {} to merge: {}",
                hold.blocking_phase,
                hold.held_by.join(", ")
            );
        }
        let phase = phase_of(row);
        let idx = *phase_index.entry(phase.clone()).or_insert_with(|| {
            phases.push(PhaseRollup {
                name: phase.clone(),
                tasks: Vec::new(),
            });
            phases.len() - 1
        });
        match totals.iter_mut().find(|(s, _)| *s == status) {
            Some((_, n)) => *n += 1,
            None => totals.push((status, 1)),
        }
        phases[idx].tasks.push(TaskRollup {
            task_id: row.task_id.clone(),
            phase,
            status,
            passed_graders: passed,
            total_graders: total,
            prerequisites: row.prerequisites.clone(),
            unsatisfied_prerequisites: unsatisfied,
            detail,
        });
    }
    Ok(Rollup { phases, totals })
}

/// Classify `items` against `store` and project them into the rollup, with
/// no phase-reachability holds armed (the merge/pr-strategy projection).
pub fn compute_rollup<S: LifecycleStore + GraderResultStore + ?Sized>(
    items: &[WorkItem],
    store: &S,
) -> Result<Rollup, StoreError> {
    let rows = status_rows_for_items(items, store)?;
    build_rollup(&rows, store, &BTreeMap::new())
}

// --- Renderers --------------------------------------------------------------------

fn detail_for(task: &TaskRollup) -> String {
    if task.status == RollupStatus::Verified {
        format!(
            "graders {}/{} passed",
            task.passed_graders, task.total_graders
        )
    } else {
        task.detail.clone()
    }
}

/// Render the rollup as an aligned, evidence-first text report.
pub fn render_rollup_text(rollup: &Rollup) -> String {
    let mut lines =
        vec!["rollup -- status is derived from grader evidence, not self-reported".to_owned()];
    if rollup.phases.is_empty() {
        lines.push("(no tasks)".to_owned());
        return lines.join("\n");
    }
    let width = rollup
        .tasks()
        .map(|t| t.task_id.chars().count())
        .max()
        .unwrap_or(0);
    for phase in &rollup.phases {
        lines.push(String::new());
        lines.push(format!(
            "  {}  ({}/{} verified)",
            phase.name,
            phase.verified(),
            phase.tasks.len()
        ));
        for task in &phase.tasks {
            let detail = detail_for(task);
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!("  {detail}")
            };
            lines.push(format!(
                "    {:<STATUS_WIDTH$}  {:<width$}{suffix}",
                task.status.as_str(),
                task.task_id,
            ));
        }
    }
    lines.push(String::new());
    let ordered: Vec<String> = RollupStatus::ALL
        .iter()
        .filter_map(|status| {
            let count = rollup.total(*status);
            (count > 0).then(|| format!("{} {count}", status.as_str()))
        })
        .collect();
    let totals = if ordered.is_empty() {
        "(none)".to_owned()
    } else {
        ordered.join("  ")
    };
    lines.push(format!("  totals: {totals}"));
    lines.join("\n")
}

/// Serialize the rollup for `status --rollup --json` consumers.
///
/// The shape is the stakeholder-legible read projection: a stable
/// machine-readable face over the same evidence the text report renders.
pub fn rollup_to_json(rollup: &Rollup) -> Value {
    let totals: serde_json::Map<String, Value> = rollup
        .totals
        .iter()
        .map(|(status, n)| (status.as_str().to_owned(), json!(n)))
        .collect();
    json!({
        "totals": totals,
        "phases": rollup.phases.iter().map(|phase| json!({
            "name": phase.name,
            "verified": phase.verified(),
            "task_count": phase.tasks.len(),
            "tasks": phase.tasks.iter().map(|task| json!({
                "task_id": task.task_id,
                "status": task.status.as_str(),
                "graders": {
                    "passed": task.passed_graders,
                    "total": task.total_graders,
                },
                "prerequisites": task.prerequisites,
                "unsatisfied_prerequisites": task.unsatisfied_prerequisites,
                "detail": task.detail,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use flywheel_core::lifecycle::{Attempt, Outcome};
    use flywheel_core::store::{AttemptStore, GraderResultRecord};
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::{CommandGrader, Grader, GraderType, Task};
    use serde_json::Map;
    use tempfile::TempDir;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 19, 0, 0, 0).unwrap()
    }

    fn task(task_id: &str) -> Task {
        let mut t = Task::new(
            format!("Goal {task_id}"),
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        );
        t.id = task_id.to_owned();
        t
    }

    /// A file-backed work item so the rollup derives `phase` from the path.
    fn item(task_id: &str, phase: &str, prerequisites: &[&str]) -> WorkItem {
        let mut item = WorkItem::new(task(task_id), format!("{phase}/{task_id}.json"))
            .with_prerequisites(prerequisites.iter().copied());
        item.local_path = Some(
            Path::new(".flywheel/tasks/active")
                .join(phase)
                .join(format!("{task_id}.json")),
        );
        item
    }

    fn lifecycle_through(task_id: &str, run_id: &str, path: &[(Status, &str)]) -> Lifecycle {
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        for (status, error) in path {
            lc.transition_to(*status, error, Some(now())).unwrap();
        }
        lc
    }

    fn seed_done(store: &InMemoryStore, task_id: &str) -> String {
        let run_id = format!("run-{task_id}-ok");
        let lc = lifecycle_through(
            task_id,
            &run_id,
            &[
                (Status::Ready, ""),
                (Status::Running, ""),
                (Status::Validating, ""),
                (Status::Done, ""),
            ],
        );
        store.create_lifecycle(&lc).unwrap();
        run_id
    }

    fn attempt(run_id: &str, number: u32, outcome: Outcome) -> Attempt {
        let mut a = Attempt::new(number, now(), run_id);
        a.ended_at = Some(now());
        a.outcome = Some(outcome);
        a
    }

    fn grader(run_id: &str, number: u32, ordinal: u32, passed: bool) -> GraderResultRecord {
        GraderResultRecord {
            run_id: run_id.to_owned(),
            attempt_number: number,
            ordinal,
            grader_type: GraderType::Command,
            grader_spec: Map::new(),
            passed,
            duration_ms: 1,
            payload: Map::new(),
            ts: now(),
            grader_name: None,
            id: None,
        }
    }

    /// A DONE run with a recorded grader receipt -- the verifiable shape.
    fn seed_verified(store: &InMemoryStore, task_id: &str, passed: bool) {
        let run_id = seed_done(store, task_id);
        store
            .save_attempt(&run_id, &attempt(&run_id, 1, Outcome::Succeeded), None)
            .unwrap();
        store
            .append_grader_result(&grader(&run_id, 1, 0, passed))
            .unwrap();
    }

    fn seed_running(store: &InMemoryStore, task_id: &str) {
        let lc = lifecycle_through(
            task_id,
            &format!("run-{task_id}-running"),
            &[(Status::Ready, ""), (Status::Running, "")],
        );
        store.create_lifecycle(&lc).unwrap();
    }

    fn seed_failed(store: &InMemoryStore, task_id: &str) {
        let lc = lifecycle_through(
            task_id,
            &format!("run-{task_id}-fail"),
            &[
                (Status::Ready, ""),
                (Status::Running, ""),
                (Status::Failed, "boom"),
            ],
        );
        store.create_lifecycle(&lc).unwrap();
    }

    fn find<'a>(rollup: &'a Rollup, task_id: &str) -> &'a TaskRollup {
        rollup.tasks().find(|t| t.task_id == task_id).unwrap()
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    // --- classification -----------------------------------------------------

    #[test]
    fn done_with_graders_is_verified_not_accepted() {
        let store = InMemoryStore::new();
        seed_verified(&store, "graded", true);
        let rollup = compute_rollup(&[item("graded", "phase-x", &[])], &store).unwrap();
        let task = &rollup.phases[0].tasks[0];
        assert_eq!(task.status, RollupStatus::Verified);
        assert_eq!((task.passed_graders, task.total_graders), (1, 1));
    }

    #[test]
    fn done_without_graders_is_accepted_not_verified() {
        let store = InMemoryStore::new();
        seed_done(&store, "ungraded");
        let rollup = compute_rollup(&[item("ungraded", "phase-x", &[])], &store).unwrap();
        let task = &rollup.phases[0].tasks[0];
        assert_eq!(task.status, RollupStatus::Accepted);
        assert_eq!((task.passed_graders, task.total_graders), (0, 0));
    }

    #[test]
    fn done_with_a_red_grader_is_accepted_with_partial_evidence() {
        let store = InMemoryStore::new();
        seed_verified(&store, "red", false);
        let rollup = compute_rollup(&[item("red", "phase-x", &[])], &store).unwrap();
        let task = &rollup.phases[0].tasks[0];
        assert_eq!(task.status, RollupStatus::Accepted);
        assert_eq!((task.passed_graders, task.total_graders), (0, 1));
    }

    #[test]
    fn evidence_reads_the_latest_receipt_bearing_attempt() {
        // Attempt 1 failed its grader; attempt 2 passed; attempt 3 recorded
        // no receipts at all and must be skipped, not counted as (0, 0).
        let store = InMemoryStore::new();
        let run_id = seed_done(&store, "retried");
        for (n, outcome) in [
            (1, Outcome::ValidationFailed),
            (2, Outcome::Succeeded),
            (3, Outcome::Succeeded),
        ] {
            store
                .save_attempt(&run_id, &attempt(&run_id, n, outcome), None)
                .unwrap();
        }
        store
            .append_grader_result(&grader(&run_id, 1, 0, false))
            .unwrap();
        store
            .append_grader_result(&grader(&run_id, 2, 0, true))
            .unwrap();
        store
            .append_grader_result(&grader(&run_id, 2, 1, true))
            .unwrap();
        let rollup = compute_rollup(&[item("retried", "p", &[])], &store).unwrap();
        let task = find(&rollup, "retried");
        assert_eq!(task.status, RollupStatus::Verified);
        assert_eq!((task.passed_graders, task.total_graders), (2, 2));
    }

    #[test]
    fn running_failed_and_fresh_classification() {
        let store = InMemoryStore::new();
        seed_running(&store, "active");
        seed_failed(&store, "broken");
        let rollup = compute_rollup(
            &[
                item("active", "phase-x", &[]),
                item("broken", "phase-x", &[]),
                item("fresh", "phase-x", &[]),
            ],
            &store,
        )
        .unwrap();
        assert_eq!(find(&rollup, "active").status, RollupStatus::InProgress);
        assert_eq!(find(&rollup, "active").detail, "running");
        assert_eq!(find(&rollup, "broken").status, RollupStatus::Failed);
        assert_eq!(find(&rollup, "broken").detail, "boom");
        assert_eq!(find(&rollup, "fresh").status, RollupStatus::NotStarted);
    }

    #[test]
    fn interrupted_and_awaiting_approval_are_blocked() {
        let store = InMemoryStore::new();
        let paused = lifecycle_through(
            "g",
            "run-g",
            &[
                (Status::Ready, ""),
                (Status::Running, ""),
                (Status::Interrupted, ""),
            ],
        );
        store.create_lifecycle(&paused).unwrap();
        let gated = lifecycle_through(
            "h",
            "run-h",
            &[
                (Status::Ready, ""),
                (Status::Running, ""),
                (Status::Validating, ""),
                (Status::AwaitingApproval, ""),
            ],
        );
        store.create_lifecycle(&gated).unwrap();
        let rollup = compute_rollup(&[item("g", "p", &[]), item("h", "p", &[])], &store).unwrap();
        assert_eq!(find(&rollup, "g").status, RollupStatus::Blocked);
        assert_eq!(find(&rollup, "g").detail, "interrupted");
        assert_eq!(find(&rollup, "h").status, RollupStatus::Blocked);
        assert_eq!(find(&rollup, "h").detail, "awaiting operator approval");
    }

    #[test]
    fn internal_error_and_failed_validation_are_retryable_and_done_wins() {
        let store = InMemoryStore::new();
        let strand = lifecycle_through(
            "s",
            "run-s",
            &[
                (Status::Ready, ""),
                (Status::Running, ""),
                (Status::InternalError, "worker died"),
            ],
        );
        store.create_lifecycle(&strand).unwrap();
        // An older DONE run plus a newer FAILED run: DONE still wins.
        seed_done(&store, "d");
        seed_failed(&store, "d");
        let rows =
            status_rows_for_items(&[item("s", "p", &[]), item("d", "p", &[])], &store).unwrap();
        assert_eq!(rows[0].state, TaskState::Retryable);
        assert_eq!(rows[0].latest_error, "worker died");
        assert_eq!(rows[0].latest_status, Some(Status::InternalError));
        assert_eq!(rows[1].state, TaskState::Done);
        assert_eq!(rows[1].latest_run_id.as_deref(), Some("run-d-fail"));
        assert_eq!(rows[1].task_file, item("d", "p", &[]).local_path);
    }

    // --- prerequisites ------------------------------------------------------

    #[test]
    fn unsatisfied_prerequisite_blocks_a_fresh_task() {
        let store = InMemoryStore::new();
        seed_failed(&store, "dep");
        let rollup = compute_rollup(
            &[item("dep", "p", &[]), item("dependent", "p", &["dep"])],
            &store,
        )
        .unwrap();
        let dependent = find(&rollup, "dependent");
        assert_eq!(dependent.status, RollupStatus::BlockedByPrereq);
        assert_eq!(dependent.unsatisfied_prerequisites, strs(&["dep"]));
        assert_eq!(dependent.detail, "waiting on: dep");
    }

    #[test]
    fn satisfied_prerequisite_leaves_dependent_ready() {
        let store = InMemoryStore::new();
        seed_verified(&store, "dep", true);
        let rollup = compute_rollup(
            &[item("dep", "p", &[]), item("dependent", "p", &["dep"])],
            &store,
        )
        .unwrap();
        let dependent = find(&rollup, "dependent");
        assert_eq!(dependent.status, RollupStatus::NotStarted);
        assert!(dependent.unsatisfied_prerequisites.is_empty());
    }

    #[test]
    fn accepted_prerequisite_counts_as_done() {
        let store = InMemoryStore::new();
        seed_done(&store, "dep");
        let rollup = compute_rollup(
            &[item("dep", "p", &[]), item("dependent", "p", &["dep"])],
            &store,
        )
        .unwrap();
        assert_eq!(find(&rollup, "dependent").status, RollupStatus::NotStarted);
    }

    #[test]
    fn unlisted_store_done_prereq_unblocks_dependent() {
        let store = InMemoryStore::new();
        seed_done(&store, "A");
        let rollup = compute_rollup(&[item("dependent", "p", &["A"])], &store).unwrap();
        let dependent = find(&rollup, "dependent");
        assert_eq!(dependent.status, RollupStatus::NotStarted);
        assert!(dependent.unsatisfied_prerequisites.is_empty());
    }

    #[test]
    fn unlisted_prereq_only_failed_keeps_dependent_blocked() {
        let store = InMemoryStore::new();
        seed_failed(&store, "A");
        let rollup = compute_rollup(&[item("dependent", "p", &["A"])], &store).unwrap();
        let dependent = find(&rollup, "dependent");
        assert_eq!(dependent.status, RollupStatus::BlockedByPrereq);
        assert_eq!(dependent.unsatisfied_prerequisites, strs(&["A"]));
        assert!(dependent.detail.contains('A'));
    }

    #[test]
    fn unlisted_prereq_with_no_lifecycle_keeps_dependent_blocked() {
        let store = InMemoryStore::new();
        let rollup = compute_rollup(&[item("dependent", "p", &["A"])], &store).unwrap();
        let dependent = find(&rollup, "dependent");
        assert_eq!(dependent.status, RollupStatus::BlockedByPrereq);
        assert_eq!(dependent.unsatisfied_prerequisites, strs(&["A"]));
    }

    #[test]
    fn mixed_unlisted_prereqs_list_only_the_missing_id() {
        let store = InMemoryStore::new();
        seed_done(&store, "A");
        let rollup = compute_rollup(&[item("dependent", "p", &["A", "B"])], &store).unwrap();
        let dependent = find(&rollup, "dependent");
        assert_eq!(dependent.status, RollupStatus::BlockedByPrereq);
        assert_eq!(dependent.unsatisfied_prerequisites, strs(&["B"]));
        assert_eq!(dependent.detail, "waiting on: B");
    }

    #[test]
    fn all_unlisted_prereqs_store_done_reports_not_started() {
        let store = InMemoryStore::new();
        seed_done(&store, "A");
        seed_done(&store, "B");
        let rollup = compute_rollup(&[item("dependent", "p", &["A", "B"])], &store).unwrap();
        let dependent = find(&rollup, "dependent");
        assert_eq!(dependent.status, RollupStatus::NotStarted);
        assert!(dependent.unsatisfied_prerequisites.is_empty());
    }

    #[test]
    fn archived_task_file_satisfies_an_unlisted_prereq() {
        // Cross-store continuity: no lifecycle for "old", but its task JSON
        // sits under archive/<phase>/ next to the rows' active/ tree.
        let dir = TempDir::new().unwrap();
        let tasks = dir.path().join("tasks");
        let archived = tasks.join("archive").join("00-old");
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::write(archived.join("old.json"), "{}").unwrap();
        let active = tasks.join("active").join("10-new");
        std::fs::create_dir_all(&active).unwrap();
        let mut dependent = WorkItem::new(task("dependent"), "10-new/dependent.json")
            .with_prerequisites(["old", "gone"]);
        dependent.local_path = Some(active.join("dependent.json"));
        let store = InMemoryStore::new();
        let rows = status_rows_for_items(&[dependent], &store).unwrap();
        let satisfied = satisfied_prerequisites_from_store(&rows, &store).unwrap();
        assert_eq!(satisfied, BTreeSet::from(["old".to_owned()]));
        let rollup = build_rollup(&rows, &store, &BTreeMap::new()).unwrap();
        assert_eq!(
            find(&rollup, "dependent").unsatisfied_prerequisites,
            strs(&["gone"])
        );
    }

    #[test]
    fn reachability_hold_blocks_an_otherwise_ready_dependent() {
        let store = InMemoryStore::new();
        seed_verified(&store, "dep", true);
        seed_failed(&store, "other");
        let rows = status_rows_for_items(
            &[
                item("dep", "p1", &[]),
                item("ready", "p2", &["dep"]),
                item("still-blocked", "p2", &["other"]),
            ],
            &store,
        )
        .unwrap();
        let holds = BTreeMap::from([
            (
                "ready".to_owned(),
                PrerequisiteReachabilityHold {
                    task_id: "ready".to_owned(),
                    blocking_phase: "p1".to_owned(),
                    held_by: strs(&["dep"]),
                },
            ),
            (
                "still-blocked".to_owned(),
                PrerequisiteReachabilityHold {
                    task_id: "still-blocked".to_owned(),
                    blocking_phase: "p1".to_owned(),
                    held_by: strs(&["other"]),
                },
            ),
        ]);
        let rollup = build_rollup(&rows, &store, &holds).unwrap();
        let held = find(&rollup, "ready");
        assert_eq!(held.status, RollupStatus::BlockedByPrereq);
        assert_eq!(held.unsatisfied_prerequisites, strs(&["dep"]));
        assert_eq!(
            held.detail,
            "prerequisite landed but not reachable; waiting on phase p1 to merge: dep"
        );
        // A genuinely incomplete prerequisite keeps precedence over the hold.
        assert_eq!(find(&rollup, "still-blocked").detail, "waiting on: other");
    }

    // --- grouping, totals, renderers ----------------------------------------

    #[test]
    fn phase_grouping_and_totals() {
        let store = InMemoryStore::new();
        seed_verified(&store, "a", true);
        seed_running(&store, "b");
        let rollup = compute_rollup(
            &[
                item("a", "phase-1", &[]),
                item("b", "phase-1", &[]),
                item("c", "phase-2", &[]),
            ],
            &store,
        )
        .unwrap();
        let names: Vec<&str> = rollup.phases.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["phase-1", "phase-2"]);
        assert_eq!(rollup.phases[0].verified(), 1);
        assert_eq!(
            rollup.totals,
            vec![
                (RollupStatus::Verified, 1),
                (RollupStatus::InProgress, 1),
                (RollupStatus::NotStarted, 1),
            ]
        );
        assert_eq!(rollup.total(RollupStatus::Failed), 0);
    }

    #[test]
    fn external_items_group_under_their_source_ref() {
        let store = InMemoryStore::new();
        let external = WorkItem::new(task("ext"), "octo/widgets#7");
        let nameless = WorkItem::new(task("anon"), "");
        let rollup = compute_rollup(&[external, nameless], &store).unwrap();
        let names: Vec<&str> = rollup.phases.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["octo/widgets#7", "external"]);
    }

    #[test]
    fn json_shape_is_evidence_first() {
        let store = InMemoryStore::new();
        seed_verified(&store, "a", true);
        let rollup = compute_rollup(&[item("a", "phase-1", &[])], &store).unwrap();
        let payload = rollup_to_json(&rollup);
        assert_eq!(payload["totals"], json!({"verified": 1}));
        let phase = &payload["phases"][0];
        assert_eq!(phase["name"], "phase-1");
        assert_eq!(phase["verified"], 1);
        assert_eq!(
            phase["tasks"][0],
            json!({
                "task_id": "a",
                "status": "verified",
                "graders": {"passed": 1, "total": 1},
                "prerequisites": [],
                "unsatisfied_prerequisites": [],
                "detail": "",
            })
        );
    }

    #[test]
    fn empty_rollup_renders_cleanly() {
        let store = InMemoryStore::new();
        let rollup = compute_rollup(&[], &store).unwrap();
        assert!(rollup.phases.is_empty());
        assert!(rollup.totals.is_empty());
        assert_eq!(
            render_rollup_text(&rollup),
            "rollup -- status is derived from grader evidence, not self-reported\n(no tasks)"
        );
        assert_eq!(
            serde_json::to_string_pretty(&rollup_to_json(&rollup)).unwrap(),
            "{\n  \"totals\": {},\n  \"phases\": []\n}"
        );
    }

    /// The fixture `packages/flywheel-orchestrator`'s Python renderers were
    /// run against; the expected strings below are their verbatim output.
    fn fixture_rollup() -> Rollup {
        let store = InMemoryStore::new();
        // a: verified across a retry -- attempt 1 red, attempt 2 all green.
        let rid = seed_done(&store, "a");
        store
            .save_attempt(&rid, &attempt(&rid, 1, Outcome::ValidationFailed), None)
            .unwrap();
        store
            .save_attempt(&rid, &attempt(&rid, 2, Outcome::Succeeded), None)
            .unwrap();
        store
            .append_grader_result(&grader(&rid, 1, 0, false))
            .unwrap();
        store
            .append_grader_result(&grader(&rid, 2, 0, true))
            .unwrap();
        store
            .append_grader_result(&grader(&rid, 2, 1, true))
            .unwrap();
        seed_done(&store, "b");
        seed_running(&store, "c");
        seed_failed(&store, "d");
        let paused = lifecycle_through(
            "g",
            "run-g",
            &[
                (Status::Ready, ""),
                (Status::Running, ""),
                (Status::Interrupted, "paused"),
            ],
        );
        store.create_lifecycle(&paused).unwrap();
        let gated = lifecycle_through(
            "h",
            "run-h",
            &[
                (Status::Ready, ""),
                (Status::Running, ""),
                (Status::Validating, ""),
                (Status::AwaitingApproval, ""),
            ],
        );
        store.create_lifecycle(&gated).unwrap();
        let rid = seed_done(&store, "i");
        store
            .save_attempt(&rid, &attempt(&rid, 1, Outcome::Succeeded), None)
            .unwrap();
        store
            .append_grader_result(&grader(&rid, 1, 0, true))
            .unwrap();
        store
            .append_grader_result(&grader(&rid, 1, 1, false))
            .unwrap();
        seed_done(&store, "z");
        let items = vec![
            item("a", "p1", &[]),
            item("b", "p1", &[]),
            item("c", "p1", &[]),
            item("d", "p2", &[]),
            item("e", "p2", &["d", "z", "missing"]),
            item("f-long-task-id", "p2", &["z"]),
            item("g", "p2", &[]),
            item("h", "p2", &[]),
            item("i", "p1", &[]),
            WorkItem::new(task("ext"), "octo/widgets#7"),
        ];
        compute_rollup(&items, &store).unwrap()
    }

    #[test]
    fn render_text_matches_python_byte_for_byte() {
        let expected = "\
rollup -- status is derived from grader evidence, not self-reported

  p1  (1/4 verified)
    verified           a               graders 2/2 passed
    accepted           b               done with no graders -- agent claim, unverified
    in_progress        c               running
    accepted           i               done with no graders -- agent claim, unverified

  p2  (0/5 verified)
    failed             d               boom
    blocked_by_prereq  e               waiting on: d, missing
    not_started        f-long-task-id
    blocked            g               paused
    blocked            h               awaiting operator approval

  octo/widgets#7  (0/1 verified)
    not_started        ext           \n
  totals: verified 1  accepted 2  in_progress 1  blocked 2  failed 1  blocked_by_prereq 1  not_started 2";
        let text = render_rollup_text(&fixture_rollup());
        assert_eq!(text, expected);
        assert!(text.is_ascii());
    }

    #[test]
    fn render_json_matches_python_byte_for_byte() {
        let expected = r#"{
  "totals": {
    "verified": 1,
    "accepted": 2,
    "in_progress": 1,
    "failed": 1,
    "blocked_by_prereq": 1,
    "not_started": 2,
    "blocked": 2
  },
  "phases": [
    {
      "name": "p1",
      "verified": 1,
      "task_count": 4,
      "tasks": [
        {
          "task_id": "a",
          "status": "verified",
          "graders": {
            "passed": 2,
            "total": 2
          },
          "prerequisites": [],
          "unsatisfied_prerequisites": [],
          "detail": ""
        },
        {
          "task_id": "b",
          "status": "accepted",
          "graders": {
            "passed": 0,
            "total": 0
          },
          "prerequisites": [],
          "unsatisfied_prerequisites": [],
          "detail": "done with no graders -- agent claim, unverified"
        },
        {
          "task_id": "c",
          "status": "in_progress",
          "graders": {
            "passed": 0,
            "total": 0
          },
          "prerequisites": [],
          "unsatisfied_prerequisites": [],
          "detail": "running"
        },
        {
          "task_id": "i",
          "status": "accepted",
          "graders": {
            "passed": 1,
            "total": 2
          },
          "prerequisites": [],
          "unsatisfied_prerequisites": [],
          "detail": "done with no graders -- agent claim, unverified"
        }
      ]
    },
    {
      "name": "p2",
      "verified": 0,
      "task_count": 5,
      "tasks": [
        {
          "task_id": "d",
          "status": "failed",
          "graders": {
            "passed": 0,
            "total": 0
          },
          "prerequisites": [],
          "unsatisfied_prerequisites": [],
          "detail": "boom"
        },
        {
          "task_id": "e",
          "status": "blocked_by_prereq",
          "graders": {
            "passed": 0,
            "total": 0
          },
          "prerequisites": [
            "d",
            "z",
            "missing"
          ],
          "unsatisfied_prerequisites": [
            "d",
            "missing"
          ],
          "detail": "waiting on: d, missing"
        },
        {
          "task_id": "f-long-task-id",
          "status": "not_started",
          "graders": {
            "passed": 0,
            "total": 0
          },
          "prerequisites": [
            "z"
          ],
          "unsatisfied_prerequisites": [],
          "detail": ""
        },
        {
          "task_id": "g",
          "status": "blocked",
          "graders": {
            "passed": 0,
            "total": 0
          },
          "prerequisites": [],
          "unsatisfied_prerequisites": [],
          "detail": "paused"
        },
        {
          "task_id": "h",
          "status": "blocked",
          "graders": {
            "passed": 0,
            "total": 0
          },
          "prerequisites": [],
          "unsatisfied_prerequisites": [],
          "detail": "awaiting operator approval"
        }
      ]
    },
    {
      "name": "octo/widgets#7",
      "verified": 0,
      "task_count": 1,
      "tasks": [
        {
          "task_id": "ext",
          "status": "not_started",
          "graders": {
            "passed": 0,
            "total": 0
          },
          "prerequisites": [],
          "unsatisfied_prerequisites": [],
          "detail": ""
        }
      ]
    }
  ]
}"#;
        let json = serde_json::to_string_pretty(&rollup_to_json(&fixture_rollup())).unwrap();
        assert_eq!(json, expected);
    }
}
