//! Grader integration: rubric judges, the manual-approval gate and its
//! resolver, and the landability gate.

use super::support::*;
use crate::envelope::Intent;
use crate::events::{DomainEventPayload, replay};
use crate::grader_rubric::RubricJudgeError;
use crate::harness::{
    HarnessConfig, ResolveApprovalOptions, ResolveApprovalOutcome, resolve_manual_approval,
};
use crate::invoker_client::{
    CONTROL_COMMAND_APPROVE, CONTROL_COMMAND_REJECT, enqueue_approve, enqueue_reject,
};
use crate::lifecycle::{Lifecycle, Outcome, Status};
use crate::store::{
    AttemptStore, ControlCommandStore, DomainEventStore, GraderResultStore, HarnessStore,
    JsonObject, LifecycleStore,
};
use crate::task::{Grader, GraderType, ManualGrader, RubricGrader, Task, TranscriptGrader};
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;

mod gate_binding;

fn rubric(name: &str, retry_on_fail: bool) -> Grader {
    Grader::Rubric(RubricGrader {
        assertions: vec!["a".into()],
        rubric: None,
        name: Some(name.into()),
        judge_model: None,
        retry_on_fail,
    })
}

fn manual(name: &str, instruction: &str) -> Grader {
    Grader::Manual(ManualGrader {
        instruction: instruction.into(),
        name: Some(name.into()),
    })
}

fn judge_config(
    dir: &tempfile::TempDir,
    judge: Arc<ScriptedJudge>,
    max_retries: u32,
) -> HarnessConfig {
    HarnessConfig {
        max_retries,
        rubric_judge_invoke: Some(judge),
        ..worktree_config(dir)
    }
}

fn fail_verdict(summary: &str) -> Result<String, RubricJudgeError> {
    Ok(rubric_wrap(&format!(
        "{{\"passed\": false, \"summary\": \"{summary}\"}}"
    )))
}

fn pass_verdict(summary: &str) -> Result<String, RubricJudgeError> {
    Ok(rubric_wrap(&format!(
        "{{\"passed\": true, \"summary\": \"{summary}\"}}"
    )))
}

// --- Rubric integration -----------------------------------------------------

#[tokio::test]
async fn all_pass_rubric_reaches_done() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("r0", true)]);
    let mut lc = lifecycle("run-rubric-done");
    let judge = ScriptedJudge::passing("ok");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Succeeded));
    let rows = h.store.list_grader_results("run-rubric-done", 1).unwrap();
    let types: Vec<_> = rows.iter().map(|r| r.grader_type).collect();
    assert_eq!(types, [GraderType::Command, GraderType::Rubric]);
    assert!(rows.iter().all(|r| r.passed));
}

#[tokio::test]
async fn rubric_fail_with_retry_on_fail_true_consumes_retry() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("semantics", true)]);
    let mut lc = lifecycle("run-rubric-retry");
    let judge = ScriptedJudge::new(vec![fail_verdict("wrong file"), pass_verdict("fixed")]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 1), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.lifecycle.retries, 1);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::ValidationFailed));
    assert_eq!(
        outcome.attempts[0].error,
        "rubric grader 'semantics' failed"
    );
}

#[tokio::test]
async fn rubric_fail_with_retry_on_fail_false_interrupts() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("halt-me", false)]);
    let mut lc = lifecycle("run-rubric-interrupt");
    let judge = ScriptedJudge::new(vec![fail_verdict("park me")]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 5), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.lifecycle.retries, 0);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::ValidationFailed));
    assert!(outcome.attempts[0].error.contains("halt-me"));
}

#[tokio::test]
async fn rubric_fail_with_retries_exhausted_reaches_failed() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("strict", true)]);
    let mut lc = lifecycle("run-rubric-exhaust");
    let judge = ScriptedJudge::new(vec![fail_verdict("first"), fail_verdict("second")]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 1), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert!(outcome.lifecycle.error.contains("strict"));
    assert_eq!(outcome.lifecycle.retries, 1);
    assert_eq!(outcome.attempts.len(), 2);
}

#[tokio::test]
async fn command_fail_short_circuits_before_rubric() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![command("false", Some("bad")), rubric("r0", true)]);
    let mut lc = lifecycle("run-rubric-skip");
    let judge = ScriptedJudge::passing("should not run");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(
            &task,
            &mut lc,
            &judge_config(&dir, judge.clone(), 0),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(judge.call_count(), 0);
    assert!(
        h.sink
            .of_kind("run-rubric-skip", "harness.rubric_invoked")
            .is_empty()
    );
}

#[tokio::test]
async fn transcript_fail_short_circuits_before_rubric() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![
        ok_command(),
        Grader::Transcript(TranscriptGrader {
            max_turns: Some(0),
            name: Some("caps".into()),
            ..TranscriptGrader::default()
        }),
        rubric("r0", true),
    ]);
    let mut lc = lifecycle("run-transcript-skip");
    let judge = ScriptedJudge::passing("should not run");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(
            &task,
            &mut lc,
            &judge_config(&dir, judge.clone(), 0),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(judge.call_count(), 0);
}

#[tokio::test]
async fn rubric_invoked_and_verdict_events_emit_with_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![
        ok_command(),
        Grader::Rubric(RubricGrader {
            assertions: vec!["a".into()],
            rubric: None,
            name: Some("semantics".into()),
            judge_model: Some("claude-x".into()),
            retry_on_fail: true,
        }),
    ]);
    let mut lc = lifecycle("run-rubric-events");
    let judge = ScriptedJudge::passing("great");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    h.run(&task, &mut lc, &judge_config(&dir, judge, 0), invoke)
        .await;
    let invoked = h
        .sink
        .of_kind("run-rubric-events", "harness.rubric_invoked");
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].payload["grader_name"], "semantics");
    assert_eq!(invoked[0].payload["judge_model"], "claude-x");
    assert_eq!(invoked[0].payload["attempt_number"], 1);
    assert_eq!(invoked[0].attempt_number, Some(1));
    let verdicts = h
        .sink
        .of_kind("run-rubric-events", "harness.rubric_verdict");
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].payload["grader_name"], "semantics");
    assert_eq!(verdicts[0].payload["passed"], true);
    assert_eq!(verdicts[0].payload["summary"], "great");
    assert_eq!(verdicts[0].payload["unknown"], false);
}

#[tokio::test]
async fn judge_infra_recovers_in_place_without_burning_the_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("rflaky", true)]);
    let mut lc = lifecycle("run-rubric-flaky");
    let judge = ScriptedJudge::new(vec![
        Err(RubricJudgeError::new("rflaky", "judge hit its turn budget")),
        pass_verdict("ok"),
    ]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(
            &task,
            &mut lc,
            &judge_config(&dir, judge.clone(), 0),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(judge.call_count(), 2);
    let retries = h
        .sink
        .of_kind("run-rubric-flaky", "harness.rubric_judge_retry");
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].payload["grader_name"], "rflaky");
    assert_eq!(retries[0].payload["judge_attempt"], 1);
    assert!(
        retries[0].payload["reason"]
            .as_str()
            .unwrap()
            .contains("turn budget")
    );
    assert!(
        h.sink
            .of_kind("run-rubric-flaky", "harness.crash")
            .is_empty()
    );
}

#[tokio::test]
async fn judge_crash_routes_to_internal_error() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("rcrash", true)]);
    let mut lc = lifecycle("run-rubric-crash");
    let crash = || Err(RubricJudgeError::new("rcrash", "network down"));
    let judge = ScriptedJudge::new(vec![crash(), crash(), crash()]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    assert_eq!(attempt.error, "rubric judge failed: rcrash: network down");
    let crashes = h.sink.of_kind("run-rubric-crash", "harness.crash");
    assert_eq!(crashes.len(), 1);
    assert_eq!(crashes[0].payload["classification"], "rubric_judge_error");
    assert_eq!(crashes[0].payload["grader_name"], "rcrash");
    assert_eq!(crashes[0].payload["reason"], "network down");
    // No rubric receipt is persisted for a judge-infrastructure failure.
    let rows = h.store.list_grader_results("run-rubric-crash", 1).unwrap();
    assert!(rows.iter().all(|r| r.grader_type != GraderType::Rubric));
}

#[tokio::test]
async fn judge_crash_repeated_retries_exhaust_to_failed() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("rcrash", true)]);
    let mut lc = lifecycle("run-rubric-crash-x");
    let judge = ScriptedJudge::new(vec![
        Err(RubricJudgeError::new("rcrash", "boom-1")),
        Err(RubricJudgeError::new("rcrash", "boom-2")),
    ]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let config = HarnessConfig {
        rubric_judge_retries: 0,
        ..judge_config(&dir, judge, 1)
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 2);
    assert!(
        outcome
            .attempts
            .iter()
            .all(|a| a.outcome == Some(Outcome::InternalError))
    );
    assert_eq!(outcome.lifecycle.retries, 1);
}

#[tokio::test]
async fn unknown_verdict_reaches_done_and_emits_warning() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("speculative", true)]);
    let mut lc = lifecycle("run-rubric-unknown");
    let judge = ScriptedJudge::new(vec![Ok(rubric_wrap(
        r#"{"passed": false, "summary": "punted", "unknown": true}"#,
    ))]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let unknown = h
        .sink
        .of_kind("run-rubric-unknown", "harness.rubric_unknown");
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].payload["grader_name"], "speculative");
    assert_eq!(unknown[0].payload["summary"], "punted");
}

#[tokio::test]
async fn missing_worktree_with_rubric_grader_is_a_judge_infra_failure() {
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("r0", true)]);
    let mut lc = lifecycle("run-rubric-no-worktree");
    let judge = ScriptedJudge::passing("ok");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let config = HarnessConfig {
        rubric_judge_invoke: Some(judge.clone()),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    assert!(outcome.attempts[0].error.contains("worktree not available"));
    assert_eq!(judge.call_count(), 0);
}

#[tokio::test]
async fn retry_attempt_carries_prior_rubric_findings_into_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("semantics", true)]);
    let mut lc = lifecycle("run-rubric-feedback");
    let judge = ScriptedJudge::new(vec![
        fail_verdict("modified wrong file"),
        pass_verdict("fixed"),
    ]);
    let (invoke, calls) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 1), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let second = prompt_of(&calls, 1);
    assert!(second.contains("# Reviewer feedback"));
    assert!(second.contains("semantics"));
    assert!(second.contains("modified wrong file"));
    assert!(!prompt_of(&calls, 0).contains("# Reviewer feedback"));
}

// --- Manual gate entry ------------------------------------------------------

#[tokio::test]
async fn all_pass_with_manual_gate_parks_at_awaiting_approval() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![
        ok_command(),
        rubric("r0", true),
        manual("confirm-migration", "Confirm the migration is safe."),
    ]);
    let mut lc = lifecycle("run-manual-park");
    let judge = ScriptedJudge::passing("ok");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::AwaitingApproval);
    assert_eq!(outcome.lifecycle.awaiting_manual_ordinal, Some(2));
    assert_eq!(outcome.attempts.len(), 1);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::Succeeded));
    assert!(attempt.ended_at.is_some());
    assert_eq!(attempt.error, "");
    let events = h
        .sink
        .of_kind("run-manual-park", "harness.awaiting_approval");
    assert_eq!(events.len(), 1);
    let payload = &events[0].payload;
    assert_eq!(payload["instructions"], "Confirm the migration is safe.");
    assert_eq!(payload["awaiting_ordinal"], 2);
    assert_eq!(payload["grader_name"], "confirm-migration");
    assert_eq!(payload["run_id"], "run-manual-park");
    assert_eq!(payload["attempt_number"], 1);
    assert_eq!(payload["artifacts_dir"], "");
    assert!(!outcome.lifecycle.timestamps.contains_key(&Status::Done));
    // Event-sourced park: loaded == folded.
    let mut loaded = h.store.load_lifecycle("run-manual-park").unwrap().unwrap();
    assert_eq!(loaded.awaiting_manual_ordinal, Some(2));
    let mut folded = replay(&h.store.list_domain_events("run-manual-park").unwrap()).unwrap();
    assert_eq!(folded.awaiting_manual_ordinal, Some(2));
    strip_attempt_aggregates(&mut loaded);
    strip_attempt_aggregates(&mut folded);
    assert_eq!(loaded, folded);
}

#[tokio::test]
async fn all_pass_with_no_manual_gate_still_reaches_done() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), rubric("r0", true)]);
    let mut lc = lifecycle("run-no-manual");
    let judge = ScriptedJudge::passing("ok");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.lifecycle.awaiting_manual_ordinal, None);
    assert!(
        h.sink
            .of_kind("run-no-manual", "harness.awaiting_approval")
            .is_empty()
    );
    assert!(
        !outcome
            .lifecycle
            .timestamps
            .contains_key(&Status::AwaitingApproval)
    );
}

#[tokio::test]
async fn failures_never_enter_the_manual_gate() {
    let dir = tempfile::tempdir().unwrap();
    // Rubric failure (retry_on_fail=false) -> INTERRUPTED, no gate.
    let h = Harness::new();
    let task = task(vec![
        ok_command(),
        rubric("strict", false),
        manual("never", "Should never be reached."),
    ]);
    let mut lc = lifecycle("run-rubric-fail-no-gate");
    let judge = ScriptedJudge::new(vec![fail_verdict("bad")]);
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(&dir, judge, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.lifecycle.awaiting_manual_ordinal, None);
    assert!(
        h.sink
            .of_kind("run-rubric-fail-no-gate", "harness.awaiting_approval")
            .is_empty()
    );

    // Command failure -> FAILED, no gate.
    let task = super::support::task(vec![command("false", Some("bad")), manual("never", "x")]);
    let mut lc = lifecycle("run-cmd-fail-no-gate");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h.run(&task, &mut lc, &worktree_config(&dir), invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert!(
        h.sink
            .of_kind("run-cmd-fail-no-gate", "harness.awaiting_approval")
            .is_empty()
    );

    // Transcript breach -> FAILED, no gate.
    let task = super::support::task(vec![
        ok_command(),
        Grader::Transcript(TranscriptGrader {
            max_turns: Some(0),
            name: Some("caps".into()),
            ..TranscriptGrader::default()
        }),
        manual("never", "x"),
    ]);
    let mut lc = lifecycle("run-transcript-fail-no-gate");
    let mut result = iteration(
        valid(Intent::Verify),
        vec![assistant(), result_msg_with(5, None)],
    );
    result.signals.num_turns = Some(5);
    let (invoke, _) = scripted_invoker(vec![result]);
    let outcome = h.run(&task, &mut lc, &worktree_config(&dir), invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.lifecycle.awaiting_manual_ordinal, None);
    assert!(
        h.sink
            .of_kind("run-transcript-fail-no-gate", "harness.awaiting_approval")
            .is_empty()
    );
}

// --- Resolver ----------------------------------------------------------------

async fn park(
    h: &Harness,
    dir: &tempfile::TempDir,
    run_id: &str,
    gates: Vec<(&str, &str)>,
) -> (Task, Lifecycle) {
    let mut graders = vec![ok_command(), rubric("r0", true)];
    for (name, instruction) in gates {
        graders.push(manual(name, instruction));
    }
    let task = task(graders);
    let mut lc = lifecycle(run_id);
    let judge = ScriptedJudge::passing("ok");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(dir, judge, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::AwaitingApproval);
    (task, outcome.lifecycle)
}

fn resolve(
    h: &Harness,
    lc: &mut Lifecycle,
    task: &Task,
    max_retries: u32,
) -> ResolveApprovalOutcome {
    resolve_manual_approval(
        lc,
        h.store.clone(),
        task,
        ResolveApprovalOptions::default()
            .with_max_retries(max_retries)
            .with_sink(h.sink.clone()),
    )
    .unwrap()
}

fn manual_rows(h: &Harness, run_id: &str, attempt: u32) -> Vec<crate::store::GraderResultRecord> {
    h.store
        .list_grader_results(run_id, attempt)
        .unwrap()
        .into_iter()
        .filter(|r| r.grader_type == GraderType::Manual)
        .collect()
}

#[tokio::test]
async fn approve_single_gate_reaches_done_with_passed_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let (task, mut lc) = park(
        &h,
        &dir,
        "run-resolver",
        vec![("confirm-migration", "Confirm.")],
    )
    .await;
    let attempt = h
        .store
        .list_attempts("run-resolver")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(attempt.outcome, Some(Outcome::Succeeded));
    enqueue_approve(h.store.as_ref(), "run-resolver", Utc::now()).unwrap();

    let result = resolve(&h, &mut lc, &task, 0);
    assert!(result.applied);
    assert_eq!(result.reason, "approved_done");
    assert!(result.command_id.is_some());
    assert_eq!(lc.status, Status::Done);
    assert_eq!(lc.awaiting_manual_ordinal, None);
    let rows = manual_rows(&h, "run-resolver", attempt.number);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].passed);
    assert_eq!(rows[0].grader_name.as_deref(), Some("confirm-migration"));
    assert_eq!(rows[0].ordinal, 2);
    assert_eq!(rows[0].payload["summary"], "approved");
    // Attempt NOT re-finalized.
    let reloaded = h.store.list_attempts("run-resolver").unwrap();
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].outcome, Some(Outcome::Succeeded));
    assert_eq!(reloaded[0].ended_at, attempt.ended_at);
    assert_eq!(
        h.sink
            .of_kind("run-resolver", "harness.attempt_finalized")
            .len(),
        1
    );
    let approved = h.sink.of_kind("run-resolver", "harness.manual_approved");
    assert_eq!(approved.len(), 1);
    assert_eq!(
        serde_json::Value::Object(approved[0].payload.clone()),
        json!({"grader_name": "confirm-migration", "awaiting_ordinal": 2})
    );
    let applied = h
        .sink
        .of_kind("run-resolver", "harness.control_command_applied");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].payload["kind"], "approve");
    assert_eq!(applied[0].payload["command_id"], json!(result.command_id));
    // Steering ledgered and the queue row deleted.
    let steering: Vec<_> = h
        .store
        .list_domain_events("run-resolver")
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.payload, DomainEventPayload::CommandApplied(_)))
        .collect();
    assert_eq!(steering.len(), 1);
    assert!(
        h.store
            .claim_commands("run-resolver", Utc::now())
            .unwrap()
            .is_empty()
    );
    // The persisted row agrees with the caller's handle.
    let stored = h.store.load_lifecycle("run-resolver").unwrap().unwrap();
    assert_eq!(stored.status, Status::Done);
    assert_eq!(stored.version, lc.version);
}

#[tokio::test]
async fn approve_multi_gate_reparks_then_final_approve_reaches_done() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let (task, mut lc) = park(
        &h,
        &dir,
        "run-resolver-multi",
        vec![
            ("review-migration", "Confirm the migration."),
            ("review-rollout", "Confirm the rollout."),
        ],
    )
    .await;
    assert_eq!(lc.awaiting_manual_ordinal, Some(2));
    let attempt = h
        .store
        .list_attempts("run-resolver-multi")
        .unwrap()
        .pop()
        .unwrap();

    enqueue_approve(h.store.as_ref(), "run-resolver-multi", Utc::now()).unwrap();
    let first = resolve(&h, &mut lc, &task, 0);
    assert!(first.applied);
    assert_eq!(first.reason, "approved_next_gate");
    assert_eq!(lc.status, Status::AwaitingApproval);
    assert_eq!(lc.awaiting_manual_ordinal, Some(3));
    let rows = manual_rows(&h, "run-resolver-multi", attempt.number);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ordinal, 2);
    let awaiting = h
        .sink
        .of_kind("run-resolver-multi", "harness.awaiting_approval");
    assert_eq!(awaiting.len(), 2);
    assert_eq!(awaiting[1].payload["awaiting_ordinal"], 3);
    assert_eq!(awaiting[1].payload["grader_name"], "review-rollout");

    enqueue_approve(h.store.as_ref(), "run-resolver-multi", Utc::now()).unwrap();
    let second = resolve(&h, &mut lc, &task, 0);
    assert_eq!(second.reason, "approved_done");
    assert_eq!(lc.status, Status::Done);
    assert_eq!(lc.awaiting_manual_ordinal, None);
    let rows = manual_rows(&h, "run-resolver-multi", attempt.number);
    assert_eq!(rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(), [2, 3]);
    assert!(rows.iter().all(|r| r.passed));
    assert_eq!(
        h.store.list_attempts("run-resolver-multi").unwrap().len(),
        1
    );
}

#[tokio::test]
async fn resolver_noop_when_no_pending_command_or_not_awaiting() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let (task, mut lc) = park(&h, &dir, "run-resolver-idle", vec![("g", "x")]).await;
    let before = lc.version;
    let result = resolve(&h, &mut lc, &task, 0);
    assert_eq!(
        result,
        ResolveApprovalOutcome {
            applied: false,
            reason: "no_pending_command".into(),
            command_id: None
        }
    );
    assert_eq!(lc.status, Status::AwaitingApproval);
    assert_eq!(lc.awaiting_manual_ordinal, Some(2));
    assert_eq!(lc.version, before);
    assert!(manual_rows(&h, "run-resolver-idle", 1).is_empty());
    assert!(
        h.sink
            .of_kind("run-resolver-idle", "harness.manual_approved")
            .is_empty()
    );

    // A DONE lifecycle is not awaiting.
    let task2 = task_clone_done(&h, &dir).await;
    let mut done = h
        .store
        .load_lifecycle("run-resolver-done")
        .unwrap()
        .unwrap();
    let result = resolve(&h, &mut done, &task2, 0);
    assert_eq!(result.reason, "not_awaiting");

    // Task drift: the persisted ordinal no longer points at a manual grader.
    let drifted = crate::harness::tests::support::task(vec![ok_command()]);
    let result = resolve(&h, &mut lc, &drifted, 0);
    assert_eq!(result.reason, "missing_gate");
}

async fn task_clone_done(h: &Harness, dir: &tempfile::TempDir) -> Task {
    let task = task(vec![ok_command(), rubric("r", true)]);
    let mut lc = lifecycle("run-resolver-done");
    let judge = ScriptedJudge::passing("ok");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &judge_config(dir, judge, 0), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    task
}

#[tokio::test]
async fn reject_with_retries_remaining_transitions_to_ready() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let (task, mut lc) = park(
        &h,
        &dir,
        "run-reject-retry",
        vec![("confirm-migration", "x")],
    )
    .await;
    let attempt = h
        .store
        .list_attempts("run-reject-retry")
        .unwrap()
        .pop()
        .unwrap();
    let feedback = "The migration drops a column still read by the billing service.";
    enqueue_reject(
        h.store.as_ref(),
        "run-reject-retry",
        Some(feedback),
        Utc::now(),
    )
    .unwrap();

    let result = resolve(&h, &mut lc, &task, 1);
    assert!(result.applied);
    assert_eq!(result.reason, "rejected_retry");
    assert_eq!(lc.status, Status::Ready);
    assert_eq!(lc.retries, 1);
    assert_eq!(lc.awaiting_manual_ordinal, None);
    assert_eq!(lc.error, "");
    let attempts = h.store.list_attempts("run-reject-retry").unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, Some(Outcome::Succeeded));
    assert_eq!(attempts[0].ended_at, attempt.ended_at);
    let rows = manual_rows(&h, "run-reject-retry", attempt.number);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].passed);
    assert_eq!(rows[0].payload["summary"], feedback);
    let rejected = h
        .sink
        .of_kind("run-reject-retry", "harness.manual_rejected");
    assert_eq!(rejected.len(), 1);
    assert_eq!(
        serde_json::Value::Object(rejected[0].payload.clone()),
        json!({"grader_name": "confirm-migration", "awaiting_ordinal": 2, "feedback": feedback})
    );
    assert_eq!(
        h.sink
            .of_kind("run-reject-retry", "harness.retry_scheduled")
            .len(),
        1
    );
    let applied = h
        .sink
        .of_kind("run-reject-retry", "harness.control_command_applied");
    assert_eq!(applied[0].payload["kind"], "reject");
    let steering: Vec<_> = h
        .store
        .list_domain_events("run-reject-retry")
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            DomainEventPayload::CommandApplied(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(steering.len(), 1);
    assert_eq!(steering[0].command_kind, CONTROL_COMMAND_REJECT);
    assert_eq!(steering[0].command_payload["feedback"], feedback);
    assert!(
        h.store
            .claim_commands("run-reject-retry", Utc::now())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reject_with_retries_exhausted_reaches_failed() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let (task, mut lc) = park(
        &h,
        &dir,
        "run-reject-fail",
        vec![("confirm-migration", "x")],
    )
    .await;
    enqueue_reject(
        h.store.as_ref(),
        "run-reject-fail",
        Some("not good enough"),
        Utc::now(),
    )
    .unwrap();
    let result = resolve(&h, &mut lc, &task, 0);
    assert_eq!(result.reason, "rejected_failed");
    assert_eq!(lc.status, Status::Failed);
    assert_eq!(
        lc.error,
        "manual grader 'confirm-migration' rejected by operator"
    );
    assert!(
        h.sink
            .of_kind("run-reject-fail", "harness.retry_scheduled")
            .is_empty()
    );
    assert_eq!(
        h.store.list_attempts("run-reject-fail").unwrap()[0].outcome,
        Some(Outcome::Succeeded)
    );
    let rows = manual_rows(&h, "run-reject-fail", 1);
    assert_eq!(rows[0].payload["summary"], "not good enough");
}

#[tokio::test]
async fn reject_on_first_gate_skips_subsequent_gates() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let (task, mut lc) = park(
        &h,
        &dir,
        "run-reject-short-circuit",
        vec![("review-migration", "x"), ("review-rollout", "y")],
    )
    .await;
    enqueue_reject(
        h.store.as_ref(),
        "run-reject-short-circuit",
        Some("halt"),
        Utc::now(),
    )
    .unwrap();
    let result = resolve(&h, &mut lc, &task, 0);
    assert_eq!(result.reason, "rejected_failed");
    let rows = manual_rows(&h, "run-reject-short-circuit", 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ordinal, 2);
    assert_eq!(
        lc.error,
        "manual grader 'review-migration' rejected by operator"
    );
}

#[tokio::test]
async fn reject_with_absent_or_empty_feedback_records_the_substitution() {
    let dir = tempfile::tempdir().unwrap();
    for (run_id, feedback) in [
        ("run-reject-no-feedback", None),
        ("run-reject-empty-feedback", Some("")),
    ] {
        let h = Harness::new();
        let (task, mut lc) = park(&h, &dir, run_id, vec![("g", "x")]).await;
        enqueue_reject(h.store.as_ref(), run_id, feedback, Utc::now()).unwrap();
        let result = resolve(&h, &mut lc, &task, 0);
        assert!(result.applied);
        let rows = manual_rows(&h, run_id, 1);
        assert_eq!(rows[0].payload["summary"], "(no feedback provided)");
        let rejected = h.sink.of_kind(run_id, "harness.manual_rejected");
        assert_eq!(rejected[0].payload["feedback"], "(no feedback provided)");
    }
}

#[tokio::test]
async fn resolver_picks_the_oldest_approve_or_reject_and_ignores_other_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let (task, mut lc) = park(&h, &dir, "run-resolver-order", vec![("g", "x")]).await;
    crate::invoker_client::enqueue_say(
        h.store.as_ref(),
        "run-resolver-order",
        "orphan",
        Utc::now(),
    )
    .unwrap();
    enqueue_reject(
        h.store.as_ref(),
        "run-resolver-order",
        Some("first"),
        Utc::now(),
    )
    .unwrap();
    enqueue_approve(h.store.as_ref(), "run-resolver-order", Utc::now()).unwrap();
    let result = resolve(&h, &mut lc, &task, 0);
    assert_eq!(result.reason, "rejected_failed");
    let applied = h
        .sink
        .of_kind("run-resolver-order", "harness.control_command_applied");
    assert_eq!(applied[0].payload["kind"], CONTROL_COMMAND_REJECT);
    let _ = CONTROL_COMMAND_APPROVE;
}

fn not_applicable_ids(h: &Harness, run_id: &str) -> Vec<serde_json::Value> {
    h.sink
        .of_kind(run_id, crate::invoker_client::EVENT_CONTROL_NOT_APPLICABLE)
        .into_iter()
        .map(|r| r.payload["command_id"].clone())
        .collect()
}

#[tokio::test]
async fn an_approve_queued_before_the_park_does_not_approve_the_gate() {
    // The pre-park decision: an `approve` row carrying no gate binding, the
    // exact row an operator's `approve` enqueued while the run was still
    // VALIDATING (no watcher drains during command graders). Nobody has
    // seen this gate yet, so it must not approve it.
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-approve-before-park";
    let (task, mut lc) = park(&h, &dir, run_id, vec![("confirm", "Confirm.")]).await;
    let early = h
        .store
        .enqueue_command(
            run_id,
            CONTROL_COMMAND_APPROVE,
            JsonObject::new(),
            Utc::now(),
        )
        .unwrap();

    let result = resolve(&h, &mut lc, &task, 0);
    assert!(!result.applied, "{result:?}");
    assert_eq!(lc.status, Status::AwaitingApproval);
    assert_eq!(lc.awaiting_manual_ordinal, Some(2));
    assert!(manual_rows(&h, run_id, 1).is_empty());
    assert!(h.sink.of_kind(run_id, "harness.manual_approved").is_empty());
    assert_eq!(not_applicable_ids(&h, run_id), vec![json!(early.id)]);
    assert_eq!(
        h.store.load_lifecycle(run_id).unwrap().unwrap().status,
        Status::AwaitingApproval
    );
}

#[tokio::test]
async fn an_approve_bound_to_one_gate_does_not_approve_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-approve-stale-gate";
    let (task, mut lc) = park(
        &h,
        &dir,
        run_id,
        vec![("review-migration", "x"), ("review-rollout", "y")],
    )
    .await;
    enqueue_approve(h.store.as_ref(), run_id, Utc::now()).unwrap();
    assert_eq!(resolve(&h, &mut lc, &task, 0).reason, "approved_next_gate");
    assert_eq!(lc.awaiting_manual_ordinal, Some(3));

    // A second decision issued against the gate the operator saw (ordinal
    // 2, attempt 1) that only reaches the queue after that gate resolved:
    // the operator never saw `review-rollout`.
    let late = h
        .store
        .enqueue_command(
            run_id,
            CONTROL_COMMAND_APPROVE,
            json!({"awaiting_ordinal": 2, "attempt_number": 1})
                .as_object()
                .unwrap()
                .clone(),
            Utc::now(),
        )
        .unwrap();
    let result = resolve(&h, &mut lc, &task, 0);
    assert!(!result.applied, "{result:?}");
    assert_eq!(lc.status, Status::AwaitingApproval);
    assert_eq!(lc.awaiting_manual_ordinal, Some(3));
    let rows = manual_rows(&h, run_id, 1);
    assert_eq!(rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(), [2]);
    assert_eq!(not_applicable_ids(&h, run_id), vec![json!(late.id)]);

    // A decision bound to the gate now parked still applies.
    enqueue_approve(h.store.as_ref(), run_id, Utc::now()).unwrap();
    assert_eq!(resolve(&h, &mut lc, &task, 0).reason, "approved_done");
}

#[tokio::test]
async fn a_repeated_approve_pinned_to_a_resolved_gate_is_refused_not_applied_to_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-approve-pinned-gate";
    let (task, mut lc) = park(
        &h,
        &dir,
        run_id,
        vec![("review-migration", "x"), ("review-rollout", "y")],
    )
    .await;
    // The operator saw gate ordinal 2 on attempt 1 and pins the approve to it.
    let seen = json!({"awaiting_ordinal": 2, "attempt_number": 1})
        .as_object()
        .unwrap()
        .clone();
    crate::invoker_client::enqueue_gate_decision(
        h.store.as_ref(),
        run_id,
        CONTROL_COMMAND_APPROVE,
        seen.clone(),
        Utc::now(),
    )
    .unwrap();
    assert_eq!(resolve(&h, &mut lc, &task, 0).reason, "approved_next_gate");
    assert_eq!(lc.awaiting_manual_ordinal, Some(3));

    // The same intent re-issued (shell history, a retried script) after
    // `review-rollout` parked: it names the gate the operator saw, which is
    // no longer parked, so it is refused and nothing is queued.
    let err = crate::invoker_client::enqueue_gate_decision(
        h.store.as_ref(),
        run_id,
        CONTROL_COMMAND_APPROVE,
        seen,
        Utc::now(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("awaiting_ordinal=3"), "{err}");
    assert_eq!(resolve(&h, &mut lc, &task, 0).reason, "no_pending_command");
    assert_eq!(lc.status, Status::AwaitingApproval);
    assert_eq!(lc.awaiting_manual_ordinal, Some(3));
    assert_eq!(
        manual_rows(&h, run_id, 1)
            .iter()
            .map(|r| r.ordinal)
            .collect::<Vec<_>>(),
        [2]
    );
}

#[tokio::test]
async fn commands_the_resolver_does_not_apply_are_recorded_not_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let run_id = "run-resolver-leftovers";
    let (task, mut lc) = park(&h, &dir, run_id, vec![("g", "x")]).await;
    let say =
        crate::invoker_client::enqueue_say(h.store.as_ref(), run_id, "hello", Utc::now()).unwrap();
    let model = crate::invoker_client::enqueue_set_model(
        h.store.as_ref(),
        run_id,
        Some("opus"),
        Utc::now(),
    )
    .unwrap();
    let approve = enqueue_approve(h.store.as_ref(), run_id, Utc::now()).unwrap();
    let duplicate = enqueue_approve(h.store.as_ref(), run_id, Utc::now()).unwrap();

    let result = resolve(&h, &mut lc, &task, 0);
    assert_eq!(result.reason, "approved_done");
    assert_eq!(result.command_id, approve.id);
    let not_applicable = h
        .sink
        .of_kind(run_id, crate::invoker_client::EVENT_CONTROL_NOT_APPLICABLE);
    assert_eq!(
        not_applicable
            .iter()
            .map(|r| (r.payload["command_id"].clone(), r.payload["kind"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (json!(say.id), json!("say")),
            (json!(model.id), json!("set_model")),
            (json!(duplicate.id), json!("approve")),
        ]
    );
}

#[tokio::test]
async fn retry_after_manual_reject_carries_operator_feedback_into_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![
        ok_command(),
        rubric("r0", true),
        manual("confirm-migration", "Confirm the migration is safe."),
    ]);
    let mut lc = lifecycle("run-reject-retry-prompt");
    let judge = ScriptedJudge::new(vec![pass_verdict("ok"), pass_verdict("ok")]);
    let (invoke, calls) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let config = judge_config(&dir, judge, 1);

    let first = h.run(&task, &mut lc, &config, invoke.clone()).await;
    assert_eq!(first.lifecycle.status, Status::AwaitingApproval);
    let feedback = "The migration drops a column still read by the billing service. Gate it behind a feature flag first.";
    enqueue_reject(
        h.store.as_ref(),
        "run-reject-retry-prompt",
        Some(feedback),
        Utc::now(),
    )
    .unwrap();
    let resolved = resolve(&h, &mut lc, &task, config.max_retries);
    assert_eq!(resolved.reason, "rejected_retry");
    assert_eq!(lc.status, Status::Ready);

    let second = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(second.attempts.len(), 2);
    assert_eq!(second.lifecycle.status, Status::AwaitingApproval);
    assert_eq!(call_count(&calls), 2);
    let first_prompt = prompt_of(&calls, 0);
    let second_prompt = prompt_of(&calls, 1);
    assert!(!first_prompt.contains("# Reviewer feedback"));
    assert!(second_prompt.contains("# Reviewer feedback"));
    assert!(second_prompt.contains("## attempt #1"));
    assert!(second_prompt.contains(&format!(
        "- manual `confirm-migration` (operator): {feedback}"
    )));
    assert!(!second_prompt.contains("- rubric `r0`"));
}

// --- Landability gate ---------------------------------------------------------

fn gate_config(max_retries: u32, gate: crate::harness::LandabilityGate) -> HarnessConfig {
    HarnessConfig {
        max_retries,
        landability_gate: Some(gate),
        ..HarnessConfig::default()
    }
}

#[tokio::test]
async fn non_landable_verdict_blocks_done_and_retries() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-redrive");
    let (invoke, _) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let calls = Arc::new(std::sync::Mutex::new(0u32));
    let counter = calls.clone();
    let gate: crate::harness::LandabilityGate = Arc::new(move || {
        let mut n = counter.lock().unwrap();
        *n += 1;
        (*n == 1).then(|| "uncommitted tree".to_owned())
    });
    let outcome = h.run(&task, &mut lc, &gate_config(1, gate), invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.lifecycle.retries, 1);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::ValidationFailed));
    assert!(outcome.attempts[0].error.contains("uncommitted tree"));
    assert_eq!(outcome.attempts[1].outcome, Some(Outcome::Succeeded));
    assert_eq!(*calls.lock().unwrap(), 2);
    let blocked = h
        .sink
        .of_kind("run-redrive", "harness.landability_gate_blocked");
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].payload["reason"], "uncommitted tree");
}

#[tokio::test]
async fn never_landable_ends_failed_with_recorded_reason() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-failed");
    let (invoke, _) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let gate: crate::harness::LandabilityGate =
        Arc::new(|| Some("no commits on branch".to_owned()));
    let outcome = h.run(&task, &mut lc, &gate_config(1, gate), invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.lifecycle.retries, 1);
    assert_eq!(outcome.attempts.len(), 2);
    assert!(
        outcome
            .attempts
            .iter()
            .all(|a| a.outcome == Some(Outcome::ValidationFailed))
    );
    assert!(outcome.lifecycle.error.contains("no commits on branch"));
}

#[tokio::test]
async fn never_landable_zero_budget_fails_immediately() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-failed-0");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let gate: crate::harness::LandabilityGate = Arc::new(|| Some("empty diff".to_owned()));
    let outcome = h.run(&task, &mut lc, &gate_config(0, gate), invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].error, "change not landable: empty diff");
}

#[tokio::test]
async fn landable_verdict_and_no_gate_land_done() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-landable");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let gate: crate::harness::LandabilityGate = Arc::new(|| None);
    let outcome = h.run(&task, &mut lc, &gate_config(1, gate), invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.lifecycle.retries, 0);

    let mut lc = lifecycle("run-nogate");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let config = HarnessConfig {
        max_retries: 1,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 1);
}

#[tokio::test]
async fn gate_skipped_when_graders_fail() {
    let h = Harness::new();
    let task = task(vec![command("false", Some("fail"))]);
    let mut lc = lifecycle("run-gradersfail");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let calls = Arc::new(std::sync::Mutex::new(0u32));
    let counter = calls.clone();
    let gate: crate::harness::LandabilityGate = Arc::new(move || {
        *counter.lock().unwrap() += 1;
        None
    });
    let outcome = h.run(&task, &mut lc, &gate_config(0, gate), invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(*calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn gate_is_not_consulted_when_a_manual_gate_parks_first() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![ok_command(), manual("g", "x")]);
    let mut lc = lifecycle("run-gate-after-manual");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let calls = Arc::new(std::sync::Mutex::new(0u32));
    let counter = calls.clone();
    let gate: crate::harness::LandabilityGate = Arc::new(move || {
        *counter.lock().unwrap() += 1;
        None
    });
    let config = HarnessConfig {
        landability_gate: Some(gate),
        ..worktree_config(&dir)
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::AwaitingApproval);
    assert_eq!(*calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn store_is_the_harness_store_trait_object() {
    // The out-of-loop ops accept the same `Arc<dyn HarnessStore>` seam.
    let h = Harness::new();
    let store: Arc<dyn HarnessStore> = h.store.clone();
    assert!(store.load_lifecycle("nope").unwrap().is_none());
}
