//! The out-of-loop operator verbs: `recheck-blocked`, `recover`, and
//! `resolve`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use flywheel_core::harness::{
    RecheckOptions, RecheckOutcome, SharedSink, recheck_blocked_lifecycle,
};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{HarnessStore, JsonObject, LifecycleFilter, StoreError};
use flywheel_core::task::Task;
use flywheel_core::telemetry_file::FileTelemetrySink;
pub use flywheel_core::workflow::recover_stranded_lifecycles;
use serde_json::Value;

use crate::claims::{ClaimStore, RESOLUTION_ATTRIBUTION_OPERATOR, STOP_RESOLVED};
use crate::sources::WorkItem;

use super::args::{RecheckBlockedArgs, RecoverArgs, ResolveArgs};
use super::{WorkflowError, py_repr};

/// `(run_id, task_id)` for every recheckable blocked lifecycle: INTERRUPTED
/// rows with a persisted requires snapshot, oldest-updated first.
/// SIGINT-paused lifecycles (a NULL snapshot) are excluded; they resume
/// through `run_task`'s entry-time normalization instead.
pub fn list_blocked_lifecycles(
    store: &dyn HarnessStore,
) -> Result<Vec<(String, String)>, StoreError> {
    let interrupted =
        store.list_lifecycles(&LifecycleFilter::default().with_statuses([Status::Interrupted]))?;
    Ok(interrupted
        .into_iter()
        .rev()
        .filter(|lc| lc.blocked_requires_json.is_some())
        .map(|lc| (lc.run_id, lc.task_id))
        .collect())
}

/// Render the unmet predicates of a `RecheckOutcome.per_predicate` as
/// `type=identifier, ...` (an identifier-less miss renders its type alone).
pub fn format_unsatisfied(per_predicate: &[JsonObject]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for pred in per_predicate {
        if pred
            .get("satisfied")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let type_str = match pred.get("type").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => t.to_owned(),
            _ => "?".to_owned(),
        };
        match pred.get("identifier").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => parts.push(format!("{type_str}={id}")),
            _ => parts.push(type_str),
        }
    }
    parts.join(", ")
}

/// One CLI line for a recheck outcome: `unblocked`, `would unblock` (a
/// fully satisfied dry run), or `still blocked (...)` naming the misses.
pub fn format_recheck_line(run_id: &str, outcome: &RecheckOutcome, dry_run: bool) -> String {
    if outcome.applied {
        return format!("{run_id}: unblocked");
    }
    if dry_run
        && outcome.reason == "dry_run"
        && outcome
            .per_predicate
            .iter()
            .all(|p| p.get("satisfied").and_then(Value::as_bool).unwrap_or(false))
    {
        return format!("{run_id}: would unblock");
    }
    let mut summary = format_unsatisfied(&outcome.per_predicate);
    if summary.is_empty() {
        summary = outcome.reason.clone();
    }
    format!("{run_id}: still blocked ({summary})")
}

fn file_sink(logs_root: &Path) -> SharedSink {
    Arc::new(FileTelemetrySink::new(logs_root))
}

/// Re-evaluate blocked lifecycles' persisted requires and, when every
/// predicate is satisfied, transition INTERRUPTED -> READY. `--run-id`
/// targets one lifecycle (a missing or unblocked run prints a clear line
/// and exits 0); `--dry-run` reports without transitioning. A blocked task
/// absent from the active listing is skipped with a stderr warning.
pub fn cmd_recheck_blocked(
    args: &RecheckBlockedArgs,
    items: &[WorkItem],
    store: Arc<dyn HarnessStore>,
    logs_root: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let task_by_id: BTreeMap<&str, &Task> = items
        .iter()
        .map(|item| (item.task.id.as_str(), &item.task))
        .collect();
    let sink = file_sink(logs_root);
    let options = || {
        RecheckOptions::default()
            .with_dry_run(args.dry_run)
            .with_sink(sink.clone())
    };

    if let Some(run_id) = &args.run_id {
        let Some(lifecycle) = store.load_lifecycle(run_id)? else {
            writeln!(out, "{run_id}: not found")?;
            return Ok(0);
        };
        if lifecycle.status != Status::Interrupted || lifecycle.blocked_requires_json.is_none() {
            writeln!(out, "{run_id}: not blocked")?;
            return Ok(0);
        }
        let Some(task) = task_by_id.get(lifecycle.task_id.as_str()) else {
            writeln!(
                err,
                "warning: {run_id}: task {} not found in active tasks; skipping",
                py_repr(&lifecycle.task_id)
            )?;
            return Ok(0);
        };
        let outcome = recheck_blocked_lifecycle(store.clone(), run_id, task, options())?;
        writeln!(
            out,
            "{}",
            format_recheck_line(run_id, &outcome, args.dry_run)
        )?;
        return Ok(0);
    }

    let targets = list_blocked_lifecycles(&*store)?;
    if targets.is_empty() {
        writeln!(out, "(no blocked lifecycles)")?;
        return Ok(0);
    }
    for (run_id, task_id) in targets {
        let Some(task) = task_by_id.get(task_id.as_str()) else {
            writeln!(
                err,
                "warning: {run_id}: task {} not found in active tasks; skipping",
                py_repr(&task_id)
            )?;
            continue;
        };
        let outcome = recheck_blocked_lifecycle(store.clone(), &run_id, task, options())?;
        writeln!(
            out,
            "{}",
            format_recheck_line(&run_id, &outcome, args.dry_run)
        )?;
    }
    Ok(0)
}

/// Finalize stranded lifecycles and print each transitioned run id, or
/// `(no stranded lifecycles)`.
pub fn cmd_recover(
    args: &RecoverArgs,
    store: Arc<dyn HarnessStore>,
    logs_root: &Path,
    out: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let finalized =
        recover_stranded_lifecycles(store, args.task_id.as_deref(), Some(file_sink(logs_root)))?;
    if finalized.is_empty() {
        writeln!(out, "(no stranded lifecycles)")?;
        return Ok(0);
    }
    for run_id in finalized {
        writeln!(out, "{run_id}")?;
    }
    Ok(0)
}

/// Deliberately abandon a strand (spec 00077, criterion 5 / D-3): record an
/// operator-attributed `stop-resolved` marker keyed to the task id carrying
/// `--reason` verbatim. A strand is either an unresolved pre-run stop or a
/// ledger-only landing strand (a parked DONE run with no stop row, which
/// `status` surfaces); the marker clears both. Refuses (exit 1) when the
/// subject has neither; an empty reason exits 2.
pub fn cmd_resolve(
    args: &ResolveArgs,
    store: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    now: DateTime<Utc>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    if args.reason.trim().is_empty() {
        writeln!(err, "error: --reason must not be empty")?;
        return Ok(2);
    }
    let events = claims.list_subject_stop_events(&args.task_id)?;
    let latest = events.last();
    let superseded = match latest.filter(|e| e.kind != STOP_RESOLVED) {
        Some(stop) => stop.kind.clone(),
        None => match super::status::unresolved_landing_strand(store, &events, &args.task_id)? {
            Some(park) => park.park_kind,
            None => {
                let state = if latest.is_some() {
                    "already resolved"
                } else {
                    "no stop event"
                };
                writeln!(
                    err,
                    "error: task {} has no unresolved stop event to abandon ({state})",
                    py_repr(&args.task_id)
                )?;
                return Ok(1);
            }
        },
    };
    claims.record_stop_event(
        STOP_RESOLVED,
        &args.task_id,
        &format!("operator abandoned the strand: {}", args.reason),
        now,
        RESOLUTION_ATTRIBUTION_OPERATOR,
    )?;
    writeln!(
        out,
        "resolved {}: abandoned strand (superseded {})",
        args.task_id,
        py_repr(&superseded)
    )?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::claims::{InMemoryClaimStore, STOP_PREPARE_SKIP};
    use crate::sources::{DirectoryWorkSource, WorkSource};
    use flywheel_core::lifecycle::{Attempt, Outcome};
    use flywheel_core::store::{AttemptStore, LifecycleStore};
    use flywheel_core::store_memory::InMemoryStore;
    use serde_json::json;
    use tempfile::TempDir;

    fn pred(kind: &str, identifier: Value, satisfied: bool) -> JsonObject {
        match json!({"type": kind, "identifier": identifier, "satisfied": satisfied, "detail": ""})
        {
            Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    fn outcome(applied: bool, reason: &str, per_predicate: Vec<JsonObject>) -> RecheckOutcome {
        RecheckOutcome {
            applied,
            reason: reason.into(),
            per_predicate,
        }
    }

    #[test]
    fn recheck_lines_are_byte_identical_to_python() {
        assert_eq!(
            format_recheck_line("r1", &outcome(true, "unblocked", vec![]), false),
            "r1: unblocked"
        );
        assert_eq!(
            format_recheck_line(
                "r1",
                &outcome(
                    false,
                    "dry_run",
                    vec![pred("file_exists", json!("x"), true)]
                ),
                true
            ),
            "r1: would unblock"
        );
        assert_eq!(
            format_recheck_line(
                "r1",
                &outcome(
                    false,
                    "dry_run",
                    vec![
                        pred("file_exists", json!("x"), false),
                        pred("env_var_set", json!("V"), true)
                    ]
                ),
                true
            ),
            "r1: still blocked (file_exists=x)"
        );
        assert_eq!(
            format_recheck_line(
                "r1",
                &outcome(
                    false,
                    "unsatisfied",
                    vec![
                        pred("command_grader", json!(""), false),
                        pred("", json!("q"), false)
                    ]
                ),
                false
            ),
            "r1: still blocked (command_grader, ?=q)"
        );
        assert_eq!(
            format_recheck_line(
                "r1",
                &outcome(
                    false,
                    "parse_error: bad",
                    vec![pred("parse_error", Value::Null, false)]
                ),
                false
            ),
            "r1: still blocked (parse_error)"
        );
        assert_eq!(
            format_recheck_line("r1", &outcome(false, "unsatisfied", vec![]), false),
            "r1: still blocked (unsatisfied)"
        );
    }

    fn run_file_kinds(logs_root: &Path, run_id: &str) -> Vec<String> {
        let path = logs_root.join("runs").join(format!("{run_id}.jsonl"));
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    fn blocked_task(path: &Path, task_id: &str, grader_name: &str) {
        write_task_json(
            path,
            &json!({"id": task_id, "goal": format!("Goal for {task_id}."), "graders": [{"type": "command", "run": "true", "name": grader_name}]}),
        );
    }

    fn recheck(
        tmp: &TempDir,
        store: &Arc<InMemoryStore>,
        run_id: Option<&str>,
        dry_run: bool,
    ) -> (String, String) {
        let items = DirectoryWorkSource::new(tmp.path()).list_work().unwrap();
        let args = RecheckBlockedArgs {
            run_id: run_id.map(str::to_owned),
            dry_run,
            ..RecheckBlockedArgs::default()
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let store: Arc<dyn HarnessStore> = store.clone();
        let code = cmd_recheck_blocked(
            &args,
            &items,
            store,
            &tmp.path().join("logs"),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(code, 0);
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn recheck_empty_store_prints_placeholder_and_run_id_misses_are_clear() {
        let tmp = TempDir::new().unwrap();
        let store = memory_store();
        assert_eq!(
            recheck(&tmp, &store, None, false).0,
            "(no blocked lifecycles)\n"
        );
        assert_eq!(
            recheck(&tmp, &store, Some("nope"), false).0,
            "nope: not found\n"
        );
        seed_interrupted(&store, "paused");
        assert_eq!(
            recheck(&tmp, &store, Some("run-paused-interrupted"), false).0,
            "run-paused-interrupted: not blocked\n"
        );
        // A SIGINT-paused lifecycle (NULL snapshot) is not a scan target.
        assert_eq!(
            recheck(&tmp, &store, None, false).0,
            "(no blocked lifecycles)\n"
        );
    }

    #[test]
    fn recheck_all_satisfied_transitions_and_partial_names_only_the_misses() {
        let tmp = TempDir::new().unwrap();
        blocked_task(
            &tmp.path().join("active/01/task-a.json"),
            "task-a",
            "full-suite",
        );
        blocked_task(
            &tmp.path().join("active/01/task-p.json"),
            "task-p",
            "full-suite",
        );
        let store = memory_store();
        let flag = tmp.path().join("ready.flag");
        std::fs::write(&flag, "ok").unwrap();
        let seeded = seed_blocked(
            &store,
            "task-a",
            &json!([{"type": "file_exists", "path": flag.display().to_string(), "present": true}]),
            None,
        );
        let partial = seed_blocked(
            &store,
            "task-p",
            &json!([
                {"type": "file_exists", "path": tmp.path().join("ignored").display().to_string(), "present": false},
                {"type": "env_var_set", "name": "FLYWHEEL_RECHECK_TEST_UNSET_VAR_7F3A"}
            ]),
            None,
        );
        let (out, err) = recheck(&tmp, &store, None, false);
        assert!(err.is_empty());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], format!("{}: unblocked", seeded.run_id));
        assert!(lines[1].starts_with(&format!("{}: still blocked (", partial.run_id)));
        assert!(lines[1].contains("env_var_set=FLYWHEEL_RECHECK_TEST_UNSET_VAR_7F3A"));
        assert!(!lines[1].contains("file_exists="));
        let reloaded = store.load_lifecycle(&seeded.run_id).unwrap().unwrap();
        assert_eq!(reloaded.status, Status::Ready);
        assert!(reloaded.blocked_requires_json.is_none());
        let still = store.load_lifecycle(&partial.run_id).unwrap().unwrap();
        assert_eq!(still.status, Status::Interrupted);
        assert!(still.blocked_requires_json.is_some());
    }

    #[test]
    fn recheck_run_id_targets_one_and_dry_run_never_transitions() {
        let tmp = TempDir::new().unwrap();
        blocked_task(
            &tmp.path().join("active/01/task-a.json"),
            "task-a",
            "full-suite",
        );
        blocked_task(
            &tmp.path().join("active/01/task-b.json"),
            "task-b",
            "full-suite",
        );
        let store = memory_store();
        let flag = tmp.path().join("ready.flag");
        std::fs::write(&flag, "ok").unwrap();
        let requires =
            json!([{"type": "file_exists", "path": flag.display().to_string(), "present": true}]);
        seed_blocked(&store, "task-a", &requires, Some("run-target"));
        seed_blocked(&store, "task-b", &requires, Some("run-other"));
        let (out, _) = recheck(&tmp, &store, Some("run-target"), true);
        assert_eq!(out, "run-target: would unblock\n");
        let target = store.load_lifecycle("run-target").unwrap().unwrap();
        assert_eq!(target.status, Status::Interrupted);
        assert!(target.blocked_requires_json.is_some());
        let kinds = run_file_kinds(&tmp.path().join("logs"), "run-target");
        assert!(kinds.contains(&"harness.recheck_attempted".to_owned()));
        assert!(!kinds.contains(&"harness.unblocked".to_owned()));

        let (out, _) = recheck(&tmp, &store, Some("run-target"), false);
        assert_eq!(out, "run-target: unblocked\n");
        assert_eq!(
            store.load_lifecycle("run-target").unwrap().unwrap().status,
            Status::Ready
        );
        let other = store.load_lifecycle("run-other").unwrap().unwrap();
        assert_eq!(other.status, Status::Interrupted);
        assert!(other.blocked_requires_json.is_some());
        assert!(
            run_file_kinds(&tmp.path().join("logs"), "run-target")
                .contains(&"harness.unblocked".to_owned())
        );
    }

    #[test]
    fn recheck_skips_tasks_missing_from_the_listing_with_a_warning() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("active")).unwrap();
        let store = memory_store();
        seed_blocked(
            &store,
            "ghost",
            &json!([{"type": "env_var_set", "name": "X"}]),
            Some("run-ghost"),
        );
        let (out, err) = recheck(&tmp, &store, None, false);
        assert!(out.is_empty());
        assert_eq!(
            err,
            "warning: run-ghost: task 'ghost' not found in active tasks; skipping\n"
        );
        let (out, err) = recheck(&tmp, &store, Some("run-ghost"), false);
        assert!(out.is_empty());
        assert_eq!(
            err,
            "warning: run-ghost: task 'ghost' not found in active tasks; skipping\n"
        );
    }

    #[test]
    fn recover_finalizes_running_and_validating_and_closes_open_attempts() {
        let tmp = TempDir::new().unwrap();
        let store = memory_store();
        let running = seed_running(&store, "task-a");
        let done = seed_done(&store, "task-b");
        let validating = walk(
            "task-v",
            "run-task-v-validating",
            &[Status::Ready, Status::Running, Status::Validating],
            "",
        );
        store.create_lifecycle(&validating).unwrap();
        store
            .save_attempt(
                &validating.run_id,
                &Attempt::new(1, t0(), &validating.run_id),
                None,
            )
            .unwrap();
        let logs = tmp.path().join("logs");
        let dyn_store: Arc<dyn HarnessStore> = store.clone();
        let finalized =
            recover_stranded_lifecycles(dyn_store, None, Some(file_sink(&logs))).unwrap();
        assert_eq!(
            finalized,
            vec![running.run_id.clone(), validating.run_id.clone()]
        );
        assert_eq!(
            store
                .load_lifecycle(&running.run_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Interrupted
        );
        assert_eq!(
            store.load_lifecycle(&done.run_id).unwrap().unwrap().status,
            Status::Done
        );
        let reloaded = store.load_lifecycle(&validating.run_id).unwrap().unwrap();
        assert_eq!(reloaded.status, Status::Interrupted);
        assert_eq!(reloaded.retries, 0);
        let attempts = store.list_attempts(&validating.run_id).unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(attempts[0].ended_at.is_some());
        assert_eq!(attempts[0].outcome, Some(Outcome::InternalError));
        assert!(run_file_kinds(&logs, &validating.run_id).contains(&"harness.crash".to_owned()));
    }

    #[test]
    fn cmd_recover_prints_run_ids_filters_by_task_and_has_a_placeholder() {
        let tmp = TempDir::new().unwrap();
        let store = memory_store();
        let a = seed_running(&store, "task-a");
        let b = seed_running(&store, "task-b");
        let logs = tmp.path().join("logs");
        let args = RecoverArgs {
            task_id: Some("task-a".into()),
            ..RecoverArgs::default()
        };
        let mut out = Vec::new();
        let dyn_store: Arc<dyn HarnessStore> = store.clone();
        assert_eq!(
            cmd_recover(&args, dyn_store.clone(), &logs, &mut out).unwrap(),
            0
        );
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n", a.run_id));
        assert_eq!(
            store.load_lifecycle(&b.run_id).unwrap().unwrap().status,
            Status::Running
        );
        let mut out = Vec::new();
        assert_eq!(
            cmd_recover(&RecoverArgs::default(), dyn_store.clone(), &logs, &mut out).unwrap(),
            0
        );
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n", b.run_id));
        let mut out = Vec::new();
        assert_eq!(
            cmd_recover(&RecoverArgs::default(), dyn_store, &logs, &mut out).unwrap(),
            0
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "(no stranded lifecycles)\n"
        );
    }

    #[test]
    fn resolve_records_an_operator_marker_once_and_refuses_otherwise() {
        let store = InMemoryStore::new();
        let claims = InMemoryClaimStore::new();
        claims
            .record_stop_event(STOP_PREPARE_SKIP, "e", "skip cause", t0(), "")
            .unwrap();
        let run = |task_id: &str, reason: &str| {
            let args = ResolveArgs {
                task_id: task_id.into(),
                reason: reason.into(),
                policy: None,
                db: None,
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cmd_resolve(&args, &store, &claims, t0(), &mut out, &mut err).unwrap();
            (
                code,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        };
        assert_eq!(
            run("e", "  "),
            (
                2,
                String::new(),
                "error: --reason must not be empty\n".into()
            )
        );
        assert_eq!(
            run("e", "operator gave up"),
            (
                0,
                "resolved e: abandoned strand (superseded 'prepare-skip')\n".into(),
                String::new()
            )
        );
        let events = claims.list_subject_stop_events("e").unwrap();
        let marker = events.last().unwrap();
        assert_eq!(marker.kind, STOP_RESOLVED);
        assert_eq!(marker.attribution, RESOLUTION_ATTRIBUTION_OPERATOR);
        assert_eq!(
            marker.detail,
            "operator abandoned the strand: operator gave up"
        );
        assert_eq!(
            run("e", "again"),
            (
                1,
                String::new(),
                "error: task 'e' has no unresolved stop event to abandon (already resolved)\n"
                    .into()
            )
        );
        assert_eq!(
            run("nope", "x"),
            (
                1,
                String::new(),
                "error: task 'nope' has no unresolved stop event to abandon (no stop event)\n"
                    .into()
            )
        );
    }
}
