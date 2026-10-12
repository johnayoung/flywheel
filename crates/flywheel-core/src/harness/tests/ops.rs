//! Blocked-`requires` recheck and the per-run budget ceilings.

use super::support::*;
use crate::envelope::{BlockedRequirement, Intent};
use crate::harness::{
    HarnessConfig, RecheckOptions, RecheckOutcome, parse_blocked_requires_json,
    recheck_blocked_lifecycle, requires_json, serialize_requires,
};
use crate::invoker::IterationResult;
use crate::lifecycle::{Lifecycle, Status};
use crate::store::{GraderResultStore, LifecycleStore};
use crate::task::Task;
use chrono::Duration as ChronoDuration;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const ENV_VAR: &str = "FLYWHEEL_RECHECK_TEST_VAR";

fn blocked_lifecycle(
    h: &Harness,
    run_id: &str,
    requires: Option<Value>,
    status: Status,
) -> Lifecycle {
    let mut lc = lifecycle(run_id);
    lc.status = status;
    lc.timestamps.insert(status, utc(2026, 5, 28, 0, 0));
    if let Some(requires) = requires {
        lc.blocked_requires_json = Some(crate::telemetry_file::python_json_dumps(
            &requires,
            &crate::telemetry_file::JsonStyle::PYTHON_DEFAULT,
        ));
    }
    h.store.create_lifecycle(&lc).unwrap();
    lc
}

fn recheck(h: &Harness, run_id: &str, task: &Task, options: RecheckOptions) -> RecheckOutcome {
    recheck_blocked_lifecycle(
        h.store.clone(),
        run_id,
        task,
        options.with_sink(h.sink.clone()),
    )
    .unwrap()
}

fn full_suite_task(run: &str) -> Task {
    task(vec![command(run, Some("full-suite"))])
}

fn satisfied(outcome: &RecheckOutcome) -> Vec<(String, bool)> {
    outcome
        .per_predicate
        .iter()
        .map(|p| {
            (
                p["type"].as_str().unwrap().to_owned(),
                p["satisfied"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[test]
fn all_satisfied_transitions_clears_column_and_emits_both_events() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("present.flag"), "ok").unwrap();
    let h = Harness::new();
    let task = full_suite_task("true");
    let lc = blocked_lifecycle(
        &h,
        "run-ok",
        Some(json!([
            {"type": "command_grader", "name": "full-suite"},
            {"type": "file_exists", "path": "present.flag", "present": true},
            {"type": "env_var_set", "name": "PATH"},
        ])),
        Status::Interrupted,
    );
    let outcome = recheck(
        &h,
        &lc.run_id,
        &task,
        RecheckOptions::default().with_cwd(dir.path()),
    );
    assert!(outcome.applied);
    assert_eq!(outcome.reason, "unblocked");
    assert!(satisfied(&outcome).iter().all(|(_, s)| *s));
    let reloaded = h.store.load_lifecycle("run-ok").unwrap().unwrap();
    assert_eq!(reloaded.status, Status::Ready);
    assert_eq!(reloaded.blocked_requires_json, None);
    assert_eq!(
        h.sink.kinds("run-ok"),
        ["harness.recheck_attempted", "harness.unblocked"]
    );
    let attempted = h.sink.of_kind("run-ok", "harness.recheck_attempted");
    assert_eq!(attempted[0].payload["all_satisfied"], true);
    assert_eq!(attempted[0].payload["dry_run"], false);
    let unblocked = h.sink.of_kind("run-ok", "harness.unblocked");
    assert_eq!(
        Value::Object(unblocked[0].payload.clone()),
        json!({"from_status": "interrupted", "to_status": "ready"})
    );
    let mirrors: Vec<_> = h
        .sink
        .domain_mirrors("run-ok")
        .into_iter()
        .map(|m| m.kind)
        .collect();
    assert_eq!(mirrors, ["domain.transitioned_to"]);
}

#[test]
fn cwd_argument_resolves_predicates_against_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = dir.path().join("sandbox");
    std::fs::create_dir(&sandbox).unwrap();
    std::fs::write(sandbox.join("present.flag"), "ok").unwrap();
    let h = Harness::new();
    let task = full_suite_task("test -f present.flag");
    let lc = blocked_lifecycle(
        &h,
        "run-cwd",
        Some(json!([
            {"type": "command_grader", "name": "full-suite"},
            {"type": "file_exists", "path": "present.flag", "present": true},
        ])),
        Status::Interrupted,
    );
    let outcome = recheck(
        &h,
        &lc.run_id,
        &task,
        RecheckOptions::default().with_cwd(&sandbox),
    );
    assert!(outcome.applied);
    // Against the wrong directory the same predicates are unsatisfied.
    let h2 = Harness::new();
    let lc2 = blocked_lifecycle(
        &h2,
        "run-cwd-wrong",
        Some(json!([{"type": "file_exists", "path": "present.flag"}])),
        Status::Interrupted,
    );
    let outcome = recheck(
        &h2,
        &lc2.run_id,
        &task,
        RecheckOptions::default().with_cwd(dir.path()),
    );
    assert_eq!(outcome.reason, "unsatisfied");
    assert_eq!(
        outcome.per_predicate[0]["detail"],
        "observed=absent, expected=present"
    );
}

#[test]
fn any_unsatisfied_emits_only_recheck_attempted_and_preserves_column() {
    let h = Harness::new();
    let task = full_suite_task("true");
    let payload = json!([
        {"type": "command_grader", "name": "full-suite"},
        {"type": "env_var_set", "name": "FLYWHEEL_RECHECK_DEFINITELY_UNSET_VAR"},
    ]);
    let lc = blocked_lifecycle(
        &h,
        "run-partial",
        Some(payload.clone()),
        Status::Interrupted,
    );
    let outcome = recheck(&h, &lc.run_id, &task, RecheckOptions::default());
    assert!(!outcome.applied);
    assert_eq!(outcome.reason, "unsatisfied");
    assert_eq!(
        satisfied(&outcome),
        [
            ("command_grader".to_owned(), true),
            ("env_var_set".to_owned(), false)
        ]
    );
    let reloaded = h.store.load_lifecycle("run-partial").unwrap().unwrap();
    assert_eq!(reloaded.status, Status::Interrupted);
    assert_eq!(
        reloaded.blocked_requires_json.as_deref(),
        Some(
            r#"[{"type": "command_grader", "name": "full-suite"}, {"type": "env_var_set", "name": "FLYWHEEL_RECHECK_DEFINITELY_UNSET_VAR"}]"#
        )
    );
    assert_eq!(h.sink.kinds("run-partial"), ["harness.recheck_attempted"]);
    let attempted = h.sink.of_kind("run-partial", "harness.recheck_attempted");
    assert_eq!(attempted[0].payload["all_satisfied"], false);
    assert_eq!(
        attempted[0].payload["per_predicate"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn non_interrupted_or_snapshotless_lifecycle_is_silent_noop() {
    let h = Harness::new();
    let task = full_suite_task("true");
    let ready = blocked_lifecycle(
        &h,
        "run-ready",
        Some(json!([{"type": "env_var_set", "name": "X"}])),
        Status::Ready,
    );
    let outcome = recheck(&h, &ready.run_id, &task, RecheckOptions::default());
    assert_eq!(
        outcome,
        RecheckOutcome {
            applied: false,
            reason: "not_blocked".into(),
            per_predicate: vec![]
        }
    );
    assert!(h.sink.events("run-ready").is_empty());
    assert_eq!(
        h.store.load_lifecycle("run-ready").unwrap().unwrap().status,
        Status::Ready
    );

    let sigint = blocked_lifecycle(&h, "run-sigint", None, Status::Interrupted);
    let outcome = recheck(&h, &sigint.run_id, &task, RecheckOptions::default());
    assert_eq!(outcome.reason, "not_blocked");
    assert!(h.sink.events("run-sigint").is_empty());
    assert_eq!(
        h.store
            .load_lifecycle("run-sigint")
            .unwrap()
            .unwrap()
            .status,
        Status::Interrupted
    );

    let outcome = recheck(&h, "run-missing", &task, RecheckOptions::default());
    assert_eq!(outcome.reason, "not_blocked");
}

#[test]
fn command_grader_predicate_runs_named_grader_and_surfaces_exit_code() {
    let h = Harness::new();
    let task = full_suite_task("exit 7");
    let lc = blocked_lifecycle(
        &h,
        "run-runs",
        Some(json!([{"type": "command_grader", "name": "full-suite"}])),
        Status::Interrupted,
    );
    let outcome = recheck(&h, &lc.run_id, &task, RecheckOptions::default());
    assert!(!outcome.applied);
    assert_eq!(outcome.reason, "unsatisfied");
    assert_eq!(outcome.per_predicate.len(), 1);
    let predicate = &outcome.per_predicate[0];
    assert_eq!(predicate["type"], "command_grader");
    assert_eq!(predicate["identifier"], "full-suite");
    assert_eq!(predicate["satisfied"], false);
    assert_eq!(predicate["detail"], "exit_code=7");
    // Recheck never writes a grader_results row.
    assert!(
        h.store
            .list_grader_results("run-runs", 1)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn command_grader_predicate_is_bounded_by_the_command_timeout() {
    let h = Harness::new();
    let task = full_suite_task("sleep 30");
    let lc = blocked_lifecycle(
        &h,
        "run-timeout",
        Some(json!([{"type": "command_grader", "name": "full-suite"}])),
        Status::Interrupted,
    );
    let started = std::time::Instant::now();
    let outcome = recheck(
        &h,
        &lc.run_id,
        &task,
        RecheckOptions::default().with_command_timeout(Some(std::time::Duration::from_millis(100))),
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(outcome.reason, "unsatisfied");
    assert_eq!(outcome.per_predicate[0]["satisfied"], false);
    assert_eq!(outcome.per_predicate[0]["detail"], "timed out after 0.1s");
    assert!(RecheckOptions::default().command_timeout.is_some());
}

#[test]
fn command_grader_predicate_for_unknown_or_wrong_type_name_is_unsatisfied() {
    let h = Harness::new();
    let task = task(vec![
        command("true", Some("other")),
        crate::task::Grader::Rubric(crate::task::RubricGrader {
            assertions: vec!["a".into()],
            rubric: None,
            name: Some("judge".into()),
            judge_model: None,
            retry_on_fail: true,
        }),
    ]);
    let lc = blocked_lifecycle(
        &h,
        "run-missing-grader",
        Some(json!([
            {"type": "command_grader", "name": "full-suite"},
            {"type": "command_grader", "name": "judge"},
        ])),
        Status::Interrupted,
    );
    let outcome = recheck(&h, &lc.run_id, &task, RecheckOptions::default());
    assert_eq!(outcome.reason, "unsatisfied");
    assert!(
        outcome
            .per_predicate
            .iter()
            .all(|p| p["detail"] == "grader not found")
    );
    let reloaded = h
        .store
        .load_lifecycle("run-missing-grader")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, Status::Interrupted);
    assert!(reloaded.blocked_requires_json.is_some());
    assert_eq!(
        h.sink.kinds("run-missing-grader"),
        ["harness.recheck_attempted"]
    );
}

#[test]
fn dry_run_emits_recheck_attempted_only_even_when_all_satisfied() {
    let h = Harness::new();
    let task = full_suite_task("true");
    let payload = json!([
        {"type": "command_grader", "name": "full-suite"},
        {"type": "env_var_set", "name": "PATH"},
    ]);
    let lc = blocked_lifecycle(&h, "run-dry", Some(payload.clone()), Status::Interrupted);
    let outcome = recheck(
        &h,
        &lc.run_id,
        &task,
        RecheckOptions::default().with_dry_run(true),
    );
    assert!(!outcome.applied);
    assert_eq!(outcome.reason, "dry_run");
    assert!(satisfied(&outcome).iter().all(|(_, s)| *s));
    let reloaded = h.store.load_lifecycle("run-dry").unwrap().unwrap();
    assert_eq!(reloaded.status, Status::Interrupted);
    assert!(reloaded.blocked_requires_json.is_some());
    assert_eq!(h.sink.kinds("run-dry"), ["harness.recheck_attempted"]);
    let attempted = h.sink.of_kind("run-dry", "harness.recheck_attempted");
    assert_eq!(attempted[0].payload["dry_run"], true);
    assert_eq!(attempted[0].payload["all_satisfied"], true);
}

#[test]
fn env_var_set_empty_string_is_unsatisfied() {
    // SAFETY: the test process owns its environment; the variable name is
    // unique to this test.
    unsafe { std::env::set_var(ENV_VAR, "") };
    let h = Harness::new();
    let task = full_suite_task("true");
    let lc = blocked_lifecycle(
        &h,
        "run-empty-env",
        Some(json!([{"type": "env_var_set", "name": ENV_VAR}])),
        Status::Interrupted,
    );
    let outcome = recheck(&h, &lc.run_id, &task, RecheckOptions::default());
    assert!(!outcome.applied);
    assert_eq!(outcome.per_predicate[0]["satisfied"], false);
    assert_eq!(outcome.per_predicate[0]["detail"], "unset_or_empty");
    unsafe { std::env::remove_var(ENV_VAR) };
}

#[test]
fn file_exists_present_false_is_unsatisfied_when_path_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("must-not-exist"), "oops").unwrap();
    let h = Harness::new();
    let task = full_suite_task("true");
    let lc = blocked_lifecycle(
        &h,
        "run-absent",
        Some(json!([{"type": "file_exists", "path": "must-not-exist", "present": false}])),
        Status::Interrupted,
    );
    let outcome = recheck(
        &h,
        &lc.run_id,
        &task,
        RecheckOptions::default().with_cwd(dir.path()),
    );
    assert!(!outcome.applied);
    assert_eq!(outcome.per_predicate[0]["satisfied"], false);
    assert_eq!(
        outcome.per_predicate[0]["detail"],
        "observed=present, expected=absent"
    );
    // Absolute paths ignore the cwd.
    let abs = dir
        .path()
        .join("must-not-exist")
        .to_string_lossy()
        .into_owned();
    let lc2 = blocked_lifecycle(
        &h,
        "run-absent-abs",
        Some(json!([{"type": "file_exists", "path": abs, "present": true}])),
        Status::Interrupted,
    );
    let outcome = recheck(
        &h,
        &lc2.run_id,
        &task,
        RecheckOptions::default().with_cwd("/nonexistent"),
    );
    assert!(outcome.applied);
    assert_eq!(outcome.per_predicate[0]["detail"], "present");
}

#[test]
fn corrupted_blocked_requires_json_surfaces_parse_error() {
    let h = Harness::new();
    let task = full_suite_task("true");
    let mut lc = lifecycle("run-corrupt");
    lc.status = Status::Interrupted;
    lc.timestamps
        .insert(Status::Interrupted, utc(2026, 5, 28, 0, 0));
    lc.blocked_requires_json = Some("{not json".into());
    h.store.create_lifecycle(&lc).unwrap();
    let outcome = recheck(&h, "run-corrupt", &task, RecheckOptions::default());
    assert!(!outcome.applied);
    assert!(outcome.reason.starts_with("parse_error:"));
    assert_eq!(outcome.per_predicate[0]["type"], "parse_error");
    assert_eq!(outcome.per_predicate[0]["identifier"], Value::Null);
    assert_eq!(
        h.store
            .load_lifecycle("run-corrupt")
            .unwrap()
            .unwrap()
            .status,
        Status::Interrupted
    );
    assert_eq!(h.sink.kinds("run-corrupt"), ["harness.recheck_attempted"]);
}

#[test]
fn blocked_requires_round_trip_and_structural_errors() {
    let requires = vec![
        BlockedRequirement::CommandGrader { name: "x".into() },
        BlockedRequirement::FileExists {
            path: "p".into(),
            present: false,
        },
        BlockedRequirement::EnvVarSet { name: "V".into() },
    ];
    let serialized = serialize_requires(&requires);
    let text = requires_json(&serialized);
    assert_eq!(
        text,
        r#"[{"type": "command_grader", "name": "x"}, {"type": "file_exists", "path": "p", "present": false}, {"type": "env_var_set", "name": "V"}]"#
    );
    assert_eq!(parse_blocked_requires_json(&text).unwrap(), requires);
    // `present` defaults to true when omitted.
    assert_eq!(
        parse_blocked_requires_json(r#"[{"type": "file_exists", "path": "p"}]"#).unwrap(),
        vec![BlockedRequirement::FileExists {
            path: "p".into(),
            present: true
        }]
    );
    let cases = [
        (r#"{"a": 1}"#, "must decode to a list"),
        (r#"[1]"#, "is not a JSON object"),
        (
            r#"[{"type": "command_grader"}]"#,
            "command_grader 'name' must be a non-empty string",
        ),
        (
            r#"[{"type": "file_exists", "path": ""}]"#,
            "file_exists 'path' must be a non-empty string",
        ),
        (
            r#"[{"type": "file_exists", "path": "p", "present": "yes"}]"#,
            "file_exists 'present' must be a bool",
        ),
        (
            r#"[{"type": "env_var_set", "name": 3}]"#,
            "env_var_set 'name' must be a non-empty string",
        ),
        (
            r#"[{"type": "tool_loop_block", "tool_name": "Bash"}]"#,
            "has unknown 'type' \"tool_loop_block\"",
        ),
    ];
    for (input, expected) in cases {
        let err = parse_blocked_requires_json(input).unwrap_err();
        assert!(err.contains(expected), "{input}: {err}");
    }
}

// --- Budget ceilings (specs 00039 / 00042) --------------------------------------

fn breaches(h: &Harness, run_id: &str, ceiling: &str) -> Vec<crate::store::TelemetryRecord> {
    h.sink
        .of_kind(run_id, "harness.budget_ceiling_breached")
        .into_iter()
        .filter(|r| r.payload["ceiling"] == ceiling)
        .collect()
}

fn cost_iteration(cost: f64) -> IterationResult {
    let mut result = iteration(valid(Intent::Verify), vec![]);
    result.signals.total_cost_usd = Some(cost);
    result
}

fn token_iteration(input_tokens: u64) -> IterationResult {
    let mut result = iteration(
        valid(Intent::Verify),
        vec![
            assistant_usage(json!({"input_tokens": input_tokens})),
            result_msg(),
        ],
    );
    result.signals.total_cost_usd = Some(0.0);
    result
}

fn ceiling_config(max_cost_usd: f64, max_tokens: u64, wall_clock_seconds: u64) -> HarnessConfig {
    HarnessConfig {
        max_retries: 1,
        max_cost_usd,
        max_tokens,
        wall_clock_seconds,
        ..HarnessConfig::default()
    }
}

#[tokio::test]
async fn cost_breach_is_terminal_and_non_retryable() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-cost-1");
    let (invoke, _) = scripted_invoker(vec![cost_iteration(0.10)]);
    let outcome = h
        .run(&task, &mut lc, &ceiling_config(0.05, 0, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(
        outcome.attempts[0].outcome,
        Some(crate::lifecycle::Outcome::AgentError)
    );
    assert_eq!(
        outcome.attempts[0].error,
        "budget ceiling breached: cost_usd 0.1 >= limit 0.05"
    );
    let found = breaches(&h, "run-cost-1", "cost_usd");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].payload["limit"], 0.05);
    assert_eq!(found[0].payload["observed"], 0.1);
    // The breach pre-empts the grade: no grader rows.
    assert!(
        h.store
            .list_grader_results("run-cost-1", 1)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cost_accumulates_across_attempts() {
    let h = Harness::new();
    let task = task(vec![fail_command()]);
    let mut lc = lifecycle("run-cost-2");
    let (invoke, _) = scripted_invoker(vec![cost_iteration(0.03), cost_iteration(0.03)]);
    let outcome = h
        .run(&task, &mut lc, &ceiling_config(0.05, 0, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(breaches(&h, "run-cost-2", "cost_usd").len(), 1);
}

#[tokio::test]
async fn under_ceiling_and_zero_ceiling_complete_done() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-cost-3");
    let (invoke, _) = scripted_invoker(vec![cost_iteration(0.01)]);
    let outcome = h
        .run(
            &task,
            &mut lc,
            &ceiling_config(1.0, 1_000_000, 86_400),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert!(
        h.sink
            .of_kind("run-cost-3", "harness.budget_ceiling_breached")
            .is_empty()
    );

    let mut lc = lifecycle("run-cost-4");
    let (invoke, _) = scripted_invoker(vec![cost_iteration(100.0)]);
    let outcome = h
        .run(&task, &mut lc, &ceiling_config(0.0, 0, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert!(
        h.sink
            .of_kind("run-cost-4", "harness.budget_ceiling_breached")
            .is_empty()
    );
}

#[tokio::test]
async fn token_breach_is_terminal_and_accumulates_across_attempts() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-tok-1");
    let (invoke, _) = scripted_invoker(vec![token_iteration(2000)]);
    let outcome = h
        .run(&task, &mut lc, &ceiling_config(0.0, 1500, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(
        outcome.attempts[0].error,
        "budget ceiling breached: tokens 2000 >= limit 1500"
    );
    assert_eq!(breaches(&h, "run-tok-1", "tokens").len(), 1);

    let task = super::support::task(vec![fail_command()]);
    let mut lc = lifecycle("run-tok-2");
    let (invoke, _) = scripted_invoker(vec![token_iteration(1000), token_iteration(1000)]);
    let outcome = h
        .run(&task, &mut lc, &ceiling_config(0.0, 1500, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(breaches(&h, "run-tok-2", "tokens").len(), 1);
}

#[tokio::test]
async fn wall_clock_breach_is_terminal_and_zero_ceilings_are_unenforced() {
    let advancing = || -> crate::invoker_client::Clock {
        let base = utc(2026, 1, 1, 0, 0);
        let counter = Arc::new(Mutex::new(0i64));
        Arc::new(move || {
            let mut n = counter.lock().unwrap();
            *n += 1;
            base + ChronoDuration::seconds(100 * *n)
        })
    };
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-wc-1");
    let (invoke, _) = scripted_invoker(vec![token_iteration(1)]);
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &ceiling_config(0.0, 0, 1),
            invoke,
            h.options().with_now(advancing()),
        )
        .await
        .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    let found = breaches(&h, "run-wc-1", "wall_clock_seconds");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].payload["limit"], 1);
    assert!(found[0].payload["observed"].as_f64().unwrap() >= 100.0);

    let mut lc = lifecycle("run-zero");
    let (invoke, _) = scripted_invoker(vec![token_iteration(10_000_000)]);
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &ceiling_config(0.0, 0, 0),
            invoke,
            h.options().with_now(advancing()),
        )
        .await
        .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert!(
        h.sink
            .of_kind("run-zero", "harness.budget_ceiling_breached")
            .is_empty()
    );
}
