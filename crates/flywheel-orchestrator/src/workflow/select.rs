//! Status rows carrying their work item, next-task selection over the
//! prerequisite DAG, the phase-prerequisite reachability hold, and the
//! `next` verb.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use flywheel_core::lifecycle::Status;
use flywheel_core::store::{LifecycleFilter, LifecycleStore, StoreError};

use crate::policy::WorkPolicy;
use crate::rollup::{
    PrerequisiteReachabilityHold, TaskState, TaskStatusRow, satisfied_prerequisites_from_store,
    status_rows_for_items,
};
use crate::sources::{DirectoryWorkSource, WorkItem, WorkSource};

use super::{WorkflowError, python_path_str};

/// A status row paired with the work item it classifies.
///
/// `crate::rollup::TaskStatusRow` carries the lifecycle-derived snapshot
/// keyed by task id; the verbs also need the item's `Task` (manual-gate
/// instructions, grader counts) and its scheduling metadata (`priority`,
/// `required_capabilities`), so the two travel together.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusRow {
    pub item: WorkItem,
    pub status: TaskStatusRow,
}

impl StatusRow {
    pub fn task_id(&self) -> &str {
        &self.status.task_id
    }

    pub fn state(&self) -> TaskState {
        self.status.state
    }

    /// The on-disk path for file-backed items, `None` otherwise.
    pub fn task_file(&self) -> Option<&Path> {
        self.status.task_file.as_deref()
    }
}

/// The columns of the most recent lifecycle for a task, or `None` when no
/// lifecycle exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestLifecycleRow {
    pub run_id: String,
    pub status: Status,
    pub error: String,
    pub blocked_requires_json: Option<String>,
    pub awaiting_manual_ordinal: Option<u32>,
}

/// The most-recently-updated lifecycle for `task_id` through the public
/// `list_lifecycles` surface (the first element of its
/// `(updated_at DESC, run_id DESC)` order on every backend).
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

/// Classify each work item against `store`, in item order, keeping the
/// item alongside its snapshot.
pub fn status_rows<S: LifecycleStore + ?Sized>(
    items: &[WorkItem],
    store: &S,
) -> Result<Vec<StatusRow>, StoreError> {
    let snapshots = status_rows_for_items(items, store)?;
    Ok(items
        .iter()
        .cloned()
        .zip(snapshots)
        .map(|(item, status)| StatusRow { item, status })
        .collect())
}

/// Walk the active tasks under `tasks_dir` and classify them, in walk
/// order (the directory-source form of [`status_rows`]).
pub fn build_status_rows<S: LifecycleStore + ?Sized>(
    tasks_dir: &Path,
    store: &S,
) -> Result<Vec<StatusRow>, WorkflowError> {
    let items = DirectoryWorkSource::new(tasks_dir).list_work()?;
    Ok(status_rows(&items, store)?)
}

/// The plain snapshots of `rows`, for the consumers keyed on
/// [`TaskStatusRow`] (the rollup, the reachability hold).
pub fn snapshots(rows: &[StatusRow]) -> Vec<TaskStatusRow> {
    rows.iter().map(|row| row.status.clone()).collect()
}

/// The caller-supplied inputs to [`select_next_task`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectOptions {
    /// Tasks removed from candidacy (but still resolving as prerequisites).
    pub exclude_ids: BTreeSet<String>,
    /// This worker's advertised capability set.
    pub worker_capabilities: BTreeSet<String>,
    /// Prerequisite ids the store already resolves to DONE even though no
    /// listed row provides them.
    pub satisfied_prerequisites: BTreeSet<String>,
}

/// Pick the highest-priority eligible task from `rows`.
///
/// A task is eligible when its id is not excluded, its state is FRESH,
/// RETRYABLE, or INTERRUPTED, every prerequisite is satisfied (a listed
/// DONE row or an id in `satisfied_prerequisites`), and its
/// `required_capabilities` is a subset of `worker_capabilities`. Among the
/// candidates the offer order is descending priority with ties broken by
/// walk order (a stable sort), so an all-default set returns the first
/// eligible candidate in walk order. A task whose prerequisite is missing
/// from the listing (and not in `satisfied_prerequisites`) is never
/// selected.
pub fn select_next_task<'a>(
    rows: &'a [StatusRow],
    options: &SelectOptions,
) -> Option<&'a StatusRow> {
    // Last listing wins for a duplicated id, as a dict comprehension does.
    let mut by_id: BTreeMap<&str, &StatusRow> = BTreeMap::new();
    let mut order: Vec<&str> = Vec::new();
    for row in rows {
        if by_id.insert(row.task_id(), row).is_none() {
            order.push(row.task_id());
        }
    }
    let mut candidates: Vec<&StatusRow> = Vec::new();
    for id in order {
        let row = by_id[id];
        if options.exclude_ids.contains(id) {
            continue;
        }
        if !matches!(
            row.state(),
            TaskState::Fresh | TaskState::Retryable | TaskState::Interrupted
        ) {
            continue;
        }
        if !row
            .item
            .required_capabilities
            .is_subset(&options.worker_capabilities)
        {
            continue;
        }
        let satisfied = row.item.prerequisites.iter().all(|prereq| {
            by_id
                .get(prereq.as_str())
                .is_some_and(|dep| dep.state() == TaskState::Done)
                || options.satisfied_prerequisites.contains(prereq)
        });
        if !satisfied {
            continue;
        }
        candidates.push(row);
    }
    candidates.sort_by(|a, b| b.item.priority.cmp(&a.item.priority));
    candidates.into_iter().next()
}

// --- Reachability hold ---------------------------------------------------------------

/// Run `git -C <repo_root> <args>` and return `(exit code, stdout)`;
/// stderr is discarded (failure here is data, not an operator error).
pub fn git_capture(repo_root: &Path, args: &[&str]) -> (i32, String) {
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

/// `git merge-base --is-ancestor ancestor rev`: exit 0 is "reachable";
/// every non-zero exit (including an unresolvable revision) reads as "not
/// an ancestor" so an unresolvable base fails closed.
fn is_ancestor(repo_root: &Path, ancestor: &str, rev: &str) -> bool {
    git_capture(repo_root, &["merge-base", "--is-ancestor", ancestor, rev]).0 == 0
}

/// The phase integration branch tip commit (`refs/heads/<phase_branch>`),
/// or `None` when the ref does not exist.
fn resolve_phase_branch_tip(repo_root: &Path, phase_branch: &str) -> Option<String> {
    let (code, out) = git_capture(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{phase_branch}"),
        ],
    );
    if code != 0 {
        return None;
    }
    let tip = out.trim();
    (!tip.is_empty()).then(|| tip.to_owned())
}

/// Whether any `flywheel/phase/<phase>` integration branch exists -- the
/// only landing strategy that creates them is the phase strategy, so this
/// short-circuits the hold to a no-op for merge/pr repos.
fn phase_branches_present(repo_root: &Path) -> bool {
    let (code, out) = git_capture(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/flywheel/phase/",
        ],
    );
    code == 0 && !out.trim().is_empty()
}

/// The phase directory name a file-backed row belongs to, or `""` for a
/// pathless (external) row.
fn phase_of_row(row: &TaskStatusRow) -> String {
    row.task_file
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Dependents to withhold because a DONE cross-phase prerequisite's landed
/// work is not yet reachable from the base they would branch from (spec
/// 00079, #7).
///
/// Returns an empty map -- a total no-op preserving merge/pr scheduling --
/// when `repo_root` / `true_base` is absent or no `flywheel/phase/*` branch
/// exists. A hold is recorded only for a listed prerequisite that is in a
/// *different* phase than the dependent, classified DONE, and whose phase
/// branch tip is not an ancestor of `true_base`. A prerequisite whose phase
/// branch no longer resolves is reachable and never holds; so is an
/// unlisted one.
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
    let phase_by_id: BTreeMap<&str, String> = rows
        .iter()
        .map(|row| (row.task_id.as_str(), phase_of_row(row)))
        .collect();
    let state_by_id: BTreeMap<&str, TaskState> = rows
        .iter()
        .map(|row| (row.task_id.as_str(), row.state))
        .collect();
    let mut tip_cache: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut ancestor_cache: BTreeMap<String, bool> = BTreeMap::new();
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

// --- The `next` verb --------------------------------------------------------------------

/// Print the next eligible task's path (or source ref) and return 0; return
/// 1 when nothing is eligible.
///
/// Prerequisites absent from the listing but DONE in the store are
/// satisfied off the authoritative record, and a dependent held by the
/// phase-prerequisite reachability hold is withheld exactly as the worker
/// would refuse to claim it.
pub fn cmd_next<S: LifecycleStore + ?Sized>(
    items: &[WorkItem],
    store: &S,
    policy: Option<&WorkPolicy>,
    repo_root: Option<&Path>,
    true_base: Option<&str>,
    out: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let rows = status_rows(items, store)?;
    let plain = snapshots(&rows);
    let satisfied = satisfied_prerequisites_from_store(&plain, store)?;
    let held = reachability_held_prerequisites(&plain, repo_root, true_base);
    let options = SelectOptions {
        exclude_ids: held.into_keys().collect(),
        worker_capabilities: policy
            .map(|p| p.execution_capabilities.clone())
            .unwrap_or_default(),
        satisfied_prerequisites: satisfied,
    };
    let Some(pick) = select_next_task(&rows, &options) else {
        return Ok(1);
    };
    match pick.task_file() {
        Some(path) => writeln!(out, "{}", python_path_str(path))?,
        None => writeln!(out, "{}", pick.item.source_ref)?,
    }
    Ok(0)
}

/// The path-or-ref `next` prints for a pick, as a `PathBuf` for callers
/// that consume the file.
pub fn pick_output(pick: &StatusRow) -> PathBuf {
    match pick.task_file() {
        Some(path) => PathBuf::from(python_path_str(path)),
        None => PathBuf::from(&pick.item.source_ref),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use flywheel_core::store_memory::InMemoryStore;
    use tempfile::TempDir;

    fn rows_for(tmp: &TempDir, store: &InMemoryStore) -> Vec<StatusRow> {
        build_status_rows(tmp.path(), store).unwrap()
    }

    fn pick_id(rows: &[StatusRow]) -> Option<String> {
        select_next_task(rows, &SelectOptions::default()).map(|r| r.task_id().to_owned())
    }

    #[test]
    fn classifies_fresh_done_failed_running_interrupted() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("01-phase");
        for id in ["fresh", "done", "failed", "running", "interrupted", "ierr"] {
            write_task(&phase.join(format!("{id}.json")), id, &[]);
        }
        let store = InMemoryStore::new();
        seed_done(&store, "done");
        seed_failed(&store, "failed");
        seed_running(&store, "running");
        seed_interrupted(&store, "interrupted");
        seed_internal_error(&store, "ierr");
        let rows = rows_for(&tmp, &store);
        let states: BTreeMap<&str, TaskState> =
            rows.iter().map(|r| (r.task_id(), r.state())).collect();
        assert_eq!(states["fresh"], TaskState::Fresh);
        assert_eq!(states["done"], TaskState::Done);
        assert_eq!(states["failed"], TaskState::Retryable);
        assert_eq!(states["running"], TaskState::InProgress);
        assert_eq!(states["interrupted"], TaskState::Interrupted);
        assert_eq!(states["ierr"], TaskState::Retryable);
        let latest = latest_lifecycle_row(&store, "failed").unwrap().unwrap();
        assert_eq!(latest.run_id, "run-failed-fail");
        assert_eq!(latest.status, Status::Failed);
        assert_eq!(latest.error, "boom");
        assert!(latest_lifecycle_row(&store, "fresh").unwrap().is_none());
    }

    #[test]
    fn later_done_wins_over_earlier_failure() {
        let tmp = TempDir::new().unwrap();
        write_task(
            &tmp.path().join("active/01-phase/retried.json"),
            "retried",
            &[],
        );
        let store = InMemoryStore::new();
        seed_failed(&store, "retried");
        seed_done(&store, "retried");
        assert_eq!(rows_for(&tmp, &store)[0].state(), TaskState::Done);
    }

    #[test]
    fn selection_honors_walk_order_prerequisites_and_states() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("01-phase");
        write_task(&phase.join("a.json"), "a", &[]);
        write_task(&phase.join("b.json"), "b", &[]);
        write_task(&phase.join("second.json"), "second", &["a"]);
        let store = InMemoryStore::new();
        assert_eq!(pick_id(&rows_for(&tmp, &store)).as_deref(), Some("a"));

        seed_done(&store, "a");
        seed_running(&store, "b");
        assert_eq!(pick_id(&rows_for(&tmp, &store)).as_deref(), Some("second"));

        seed_failed(&store, "second");
        let rows = rows_for(&tmp, &store);
        let pick = select_next_task(&rows, &SelectOptions::default()).unwrap();
        assert_eq!(pick.task_id(), "second");
        assert_eq!(pick.state(), TaskState::Retryable);
    }

    #[test]
    fn interrupted_and_internal_error_tasks_are_reselectable() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("01-phase");
        write_task(&phase.join("root.json"), "root", &[]);
        write_task(&phase.join("leaf.json"), "leaf", &["root"]);
        let store = InMemoryStore::new();
        seed_interrupted(&store, "root");
        let rows = rows_for(&tmp, &store);
        let pick = select_next_task(&rows, &SelectOptions::default()).unwrap();
        assert_eq!(
            (pick.task_id(), pick.state()),
            ("root", TaskState::Interrupted)
        );

        let tmp2 = TempDir::new().unwrap();
        write_task(
            &tmp2.path().join("active/01/stranded.json"),
            "stranded",
            &[],
        );
        let store2 = InMemoryStore::new();
        seed_internal_error(&store2, "stranded");
        let rows = rows_for(&tmp2, &store2);
        assert_eq!(pick_id(&rows).as_deref(), Some("stranded"));
    }

    #[test]
    fn missing_prerequisite_blocks_unless_store_satisfies_it() {
        let tmp = TempDir::new().unwrap();
        write_task(&tmp.path().join("active/01/only.json"), "only", &["ghost"]);
        let store = InMemoryStore::new();
        let rows = rows_for(&tmp, &store);
        assert_eq!(pick_id(&rows), None);
        let options = SelectOptions {
            satisfied_prerequisites: ["ghost".to_owned()].into_iter().collect(),
            ..SelectOptions::default()
        };
        assert_eq!(
            select_next_task(&rows, &options).map(|r| r.task_id()),
            Some("only")
        );
    }

    #[test]
    fn selection_spans_phases_and_returns_none_when_all_done() {
        let tmp = TempDir::new().unwrap();
        write_task(&tmp.path().join("active/01-first/a.json"), "a", &[]);
        write_task(&tmp.path().join("active/02-second/b.json"), "b", &[]);
        let store = InMemoryStore::new();
        seed_done(&store, "a");
        assert_eq!(pick_id(&rows_for(&tmp, &store)).as_deref(), Some("b"));
        seed_done(&store, "b");
        assert_eq!(pick_id(&rows_for(&tmp, &store)), None);
    }

    #[test]
    fn priority_and_capabilities_gate_the_offer() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("01");
        write_task_json(
            &phase.join("low.json"),
            &serde_json::json!({"id": "low", "goal": "g", "graders": [{"type": "command", "run": "true"}]}),
        );
        write_task_json(
            &phase.join("high.json"),
            &serde_json::json!({"id": "high", "goal": "g", "graders": [{"type": "command", "run": "true"}], "priority": 5, "required_capabilities": ["gpu"]}),
        );
        let store = InMemoryStore::new();
        let rows = rows_for(&tmp, &store);
        assert_eq!(pick_id(&rows).as_deref(), Some("low"));
        let options = SelectOptions {
            worker_capabilities: ["gpu".to_owned()].into_iter().collect(),
            ..SelectOptions::default()
        };
        assert_eq!(
            select_next_task(&rows, &options).map(|r| r.task_id()),
            Some("high")
        );
        let excluded = SelectOptions {
            exclude_ids: ["high".to_owned(), "low".to_owned()].into_iter().collect(),
            ..options
        };
        assert!(select_next_task(&rows, &excluded).is_none());
    }

    #[test]
    fn cmd_next_prints_the_path_or_exits_one() {
        let tmp = TempDir::new().unwrap();
        let written = write_task(&tmp.path().join("active/01-phase/a.json"), "a", &[]);
        let store = InMemoryStore::new();
        let items = DirectoryWorkSource::new(tmp.path()).list_work().unwrap();
        let mut out = Vec::new();
        let code = cmd_next(&items, &store, None, None, None, &mut out).unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(out).unwrap().trim(),
            written.display().to_string()
        );

        let empty = TempDir::new().unwrap();
        let items = DirectoryWorkSource::new(empty.path()).list_work().unwrap();
        let mut out = Vec::new();
        assert_eq!(
            cmd_next(&items, &store, None, None, None, &mut out).unwrap(),
            1
        );
        assert!(out.is_empty());
    }

    #[test]
    fn reachability_hold_is_a_no_op_without_phase_branches() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("01");
        write_task(&phase.join("a.json"), "a", &[]);
        write_task(&tmp.path().join("active/02/b.json"), "b", &["a"]);
        let store = InMemoryStore::new();
        seed_done(&store, "a");
        let rows = snapshots(&rows_for(&tmp, &store));
        assert!(reachability_held_prerequisites(&rows, None, None).is_empty());
        assert!(reachability_held_prerequisites(&rows, Some(tmp.path()), Some("HEAD")).is_empty());
    }

    #[test]
    fn reachability_hold_names_the_unmerged_phase() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(status.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        git(&["branch", "flywheel/phase/01"]);
        git(&["checkout", "-q", "flywheel/phase/01"]);
        git(&["commit", "-q", "--allow-empty", "-m", "phase work"]);
        git(&["checkout", "-q", "main"]);
        let tasks = repo.join(".flywheel").join("tasks");
        write_task(&tasks.join("active/01/a.json"), "a", &[]);
        write_task(&tasks.join("active/02/b.json"), "b", &["a"]);
        write_task(&tasks.join("active/01/c.json"), "c", &["a"]);
        let store = InMemoryStore::new();
        seed_done(&store, "a");
        let rows = snapshots(&build_status_rows(&tasks, &store).unwrap());
        let holds = reachability_held_prerequisites(&rows, Some(repo), Some("main"));
        assert_eq!(
            holds.get("b"),
            Some(&PrerequisiteReachabilityHold {
                task_id: "b".into(),
                blocking_phase: "01".into(),
                held_by: vec!["a".into()],
            })
        );
        // Same-phase chains share one integration branch and never hold.
        assert!(!holds.contains_key("c"));
        // Once the phase branch is reachable from the base, the hold lifts.
        git(&["merge", "-q", "--ff-only", "flywheel/phase/01"]);
        assert!(reachability_held_prerequisites(&rows, Some(repo), Some("main")).is_empty());
    }
}
