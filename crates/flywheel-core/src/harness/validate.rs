//! The `validating` seat: command -> transcript -> rubric graders in cost
//! order, then the manual gate, the landability gate, and `DONE`.

use super::state::{AttemptError, Run, object, wait_shutdown};
use super::{HarnessConfig, HarnessError};
use crate::deadline_config::DeadlineClass;
use crate::events::{AwaitingApproval, DomainEventPayload};
use crate::grader_command::{
    CommandGraderOptions, is_operator_signaled, is_spawn_failed, payload_signal,
    run_command_graders,
};
use crate::grader_manual::{ManualGate, next_pending_manual_gate};
use crate::grader_rubric::{
    AgentsJudgeInvoke, JudgeInvoke, RubricGraderError, RubricRun, route_rubric_failure,
    run_rubric_graders,
};
use crate::grader_transcript::{TranscriptObservation, run_transcript_graders};
use crate::lifecycle::{Attempt, Outcome, Status};
use crate::store::GraderResultRecord;
use crate::task::Task;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn all_passed(results: &[GraderResultRecord]) -> bool {
    results.iter().all(|r| r.passed)
}

fn grader_failure_error(results: &[GraderResultRecord]) -> String {
    results
        .iter()
        .find(|r| !r.passed)
        .map(|r| {
            let name = r
                .grader_name
                .clone()
                .unwrap_or_else(|| r.grader_type.as_str().to_owned());
            format!("{} grader {name:?} failed", r.grader_type.as_str())
        })
        .unwrap_or_else(|| "grader failure".to_owned())
}

/// Finalize `attempt` with `outcome` / `error` and transition `target`.
fn finalize_and_transition(
    run: &Run,
    attempt: &mut Attempt,
    outcome: Outcome,
    error: &str,
    agent_output: &str,
    target: Status,
) -> Result<(), AttemptError> {
    run.finalize_attempt(attempt, outcome, error, agent_output)?;
    run.transition(target, error)?;
    Ok(())
}

/// Run command, transcript, then rubric graders and route the outcome.
///
/// - All pass -> manual gate park when the task declares one, else the
///   landability gate, else `DONE`.
/// - Command / transcript fail -> `FAILED_VALIDATION`.
/// - Operator-signal-killed command grader -> `INTERRUPTED`
///   (`harness.crash` / `grader_signaled`).
/// - Command grader whose subprocess failed to start -> `INTERNAL_ERROR`
///   (`harness.crash` / `grader_spawn_failure`).
/// - Rubric fail with `retry_on_fail` -> `FAILED_VALIDATION`, else
///   `INTERRUPTED`.
/// - Judge infrastructure failure -> `INTERNAL_ERROR` (`harness.crash` /
///   `rubric_judge_error`).
pub(super) async fn validate(
    task: &Task,
    run: &Run,
    config: &HarnessConfig,
    attempt: &mut Attempt,
    attempt_dir: Option<&Path>,
    observation: &TranscriptObservation,
    agent_output: &str,
) -> Result<(), AttemptError> {
    let attempt_number = attempt.number;
    let options = CommandGraderOptions {
        cwd: config.worktree.clone(),
        env: config.grader_env.clone(),
        per_grader_timeout: config
            .deadlines
            .duration_for_class(DeadlineClass::CommandGrader),
        artifacts_dir: attempt_dir.map(Path::to_path_buf),
        ..CommandGraderOptions::default()
    };
    let clock = run.clock.clone();
    let command_results = run_command_graders(
        task,
        run.store.as_ref(),
        &run.run_id,
        attempt_number,
        &options,
        move || clock(),
    )
    .await
    .map_err(HarnessError::CommandGrader)?;
    let command_passed = all_passed(&command_results);

    let clock = run.clock.clone();
    let transcript_results = run_transcript_graders(
        task,
        observation,
        run.store.as_ref(),
        &run.run_id,
        attempt_number,
        command_passed,
        move || clock(),
    )?;
    let transcript_passed = all_passed(&transcript_results);

    // Rubric graders only run when command + transcript both passed.
    let mut rubric_results: Vec<GraderResultRecord> = Vec::new();
    let mut rubric_passed = true;
    if command_passed && transcript_passed {
        let judge_max_turns = task
            .budgets
            .rubric_judge_max_turns
            .and_then(|t| u32::try_from(t).ok())
            .unwrap_or(config.rubric_judge_max_turns);
        let judge: Arc<dyn JudgeInvoke> = match &config.rubric_judge_invoke {
            Some(seam) => seam.clone(),
            None => Arc::new(AgentsJudgeInvoke::new(
                &config.agent_id,
                config.rubric_judge_model.as_deref(),
                judge_max_turns,
            )),
        };
        let telemetry = run.telemetry.clone();
        let mut rubric_run = RubricRun::new(run.run_id.clone(), attempt_number, agent_output)
            .with_command_passed(command_passed)
            .with_transcript_passed(transcript_passed)
            .with_judge_model(config.rubric_judge_model.as_deref())
            .with_judge_ceiling(
                super::iterate::override_ceiling(
                    task.budgets.rubric_judge_seconds.map(|s| s.as_f64()),
                    config.deadlines.for_class(DeadlineClass::RubricJudge),
                )
                .map(Duration::from_secs_f64),
            )
            .with_judge_retries(config.rubric_judge_retries)
            .with_on_judge_retry(Arc::new(move |grader_name, judge_attempt, reason| {
                // A judge-infra failure absorbed by the in-place retry
                // envelope: the judge alone re-runs, the attempt survives.
                telemetry.emit(
                    "harness.rubric_judge_retry",
                    object(json!({
                        "grader_name": grader_name,
                        "judge_attempt": judge_attempt,
                        "reason": reason,
                    })),
                    Some(attempt_number),
                    None,
                );
            }))
            .with_clock(run.clock.clone());
        if let Some(worktree) = &config.worktree {
            rubric_run = rubric_run.with_worktree(worktree.clone());
        }
        let judged = async {
            run_rubric_graders(task, run.store.as_ref(), judge.as_ref(), &rubric_run).await
        };
        let judged = tokio::select! {
            biased;
            outcome = judged => outcome,
            _ = wait_shutdown(run.shutdown.as_ref()) => return Err(AttemptError::Interrupted),
        };
        match judged {
            Ok(results) => rubric_results = results,
            Err(RubricGraderError::Store(err)) => return Err(err.into()),
            Err(RubricGraderError::Judge(exc)) => {
                let error = format!("rubric judge failed: {}: {}", exc.grader_name, exc.reason);
                run.emit(
                    "harness.crash",
                    object(json!({
                        "classification": "rubric_judge_error",
                        "grader_name": exc.grader_name,
                        "reason": exc.reason,
                        "message": error,
                    })),
                    Some(attempt_number),
                );
                return finalize_and_transition(
                    run,
                    attempt,
                    Outcome::InternalError,
                    &error,
                    agent_output,
                    Status::InternalError,
                );
            }
        }
        for record in &rubric_results {
            emit_rubric_events(run, attempt_number, record);
        }
        rubric_passed = all_passed(&rubric_results);
    }

    if command_passed && transcript_passed && rubric_passed {
        // A declared ManualGrader: the attempt passed every automated grader
        // but still owes a human decision -- finalize SUCCEEDED and park.
        if let Some(first_gate) = next_pending_manual_gate(task, None) {
            run.finalize_attempt(attempt, Outcome::Succeeded, "", agent_output)?;
            enter_manual_gate(run, attempt, &first_gate, attempt_dir)?;
            return Ok(());
        }
        // Landability gate (spec 00061): consulted first on the would-be
        // DONE path; a reason means the change must not land as a success.
        let landability_reason = config
            .landability_gate
            .as_ref()
            .and_then(|gate| gate())
            .filter(|reason| !reason.is_empty());
        if let Some(reason) = landability_reason {
            let error = format!("change not landable: {reason}");
            run.emit(
                "harness.landability_gate_blocked",
                object(json!({"reason": reason, "message": error})),
                Some(attempt_number),
            );
            return finalize_and_transition(
                run,
                attempt,
                Outcome::ValidationFailed,
                &error,
                agent_output,
                Status::FailedValidation,
            );
        }
        run.finalize_attempt(attempt, Outcome::Succeeded, "", agent_output)?;
        run.transition(Status::Done, "")?;
        return Ok(());
    }

    if !command_passed {
        if let Some(signaled) = command_results.iter().find(|r| is_operator_signaled(r)) {
            let grader_label = signaled
                .grader_name
                .clone()
                .unwrap_or_else(|| signaled.grader_type.as_str().to_owned());
            let signal_no = payload_signal(&signaled.payload).unwrap_or(0);
            let error = format!(
                "operator interrupted command grader {grader_label:?} (signal {signal_no})"
            );
            run.emit(
                "harness.crash",
                object(json!({
                    "classification": "grader_signaled",
                    "signal": signal_no,
                    "grader_name": signaled.grader_name,
                    "grader_ordinal": signaled.ordinal,
                    "message": error,
                })),
                Some(attempt_number),
            );
            run.finalize_attempt(attempt, Outcome::InternalError, &error, agent_output)?;
            run.transition(Status::Interrupted, "")?;
            return Ok(());
        }
        if let Some(spawn_failed) = command_results.iter().find(|r| is_spawn_failed(r)) {
            let grader_label = spawn_failed
                .grader_name
                .clone()
                .unwrap_or_else(|| spawn_failed.grader_type.as_str().to_owned());
            let spawn_error = spawn_failed
                .payload
                .get("spawn_error")
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let error = format!("command grader {grader_label:?} failed to start: {spawn_error}");
            run.emit(
                "harness.crash",
                object(json!({
                    "classification": "grader_spawn_failure",
                    "grader_name": spawn_failed.grader_name,
                    "grader_ordinal": spawn_failed.ordinal,
                    "message": error,
                })),
                Some(attempt_number),
            );
            return finalize_and_transition(
                run,
                attempt,
                Outcome::InternalError,
                &error,
                agent_output,
                Status::InternalError,
            );
        }
        let error = grader_failure_error(&command_results);
        return finalize_and_transition(
            run,
            attempt,
            Outcome::ValidationFailed,
            &error,
            agent_output,
            Status::FailedValidation,
        );
    }

    if !transcript_passed {
        let error = grader_failure_error(&transcript_results);
        return finalize_and_transition(
            run,
            attempt,
            Outcome::ValidationFailed,
            &error,
            agent_output,
            Status::FailedValidation,
        );
    }

    // Rubric failure: the first failing rubric record's grader controls the
    // retry-on-fail policy.
    let (error, target) = match route_rubric_failure(task, &rubric_results) {
        Some(routing) => (routing.error, routing.status),
        None => (
            grader_failure_error(&rubric_results),
            Status::FailedValidation,
        ),
    };
    run.finalize_attempt(attempt, Outcome::ValidationFailed, &error, agent_output)?;
    run.transition(target, &error)?;
    Ok(())
}

/// Park a lifecycle on a manual-grader gate after automated graders pass:
/// append `AwaitingApproval`, transition `VALIDATING -> AWAITING_APPROVAL`,
/// emit `harness.awaiting_approval`.
pub(super) fn enter_manual_gate(
    run: &Run,
    attempt: &Attempt,
    gate: &ManualGate,
    attempt_dir: Option<&Path>,
) -> Result<(), AttemptError> {
    run.append_payload(
        run.now(),
        Some(attempt.number),
        DomainEventPayload::AwaitingApproval(AwaitingApproval {
            awaiting_ordinal: gate.ordinal,
        }),
    )?;
    run.transition(Status::AwaitingApproval, "")?;
    let artifacts_dir = attempt_dir
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    run.emit(
        "harness.awaiting_approval",
        awaiting_approval_payload(gate, &run.run_id, attempt.number, &artifacts_dir),
        Some(attempt.number),
    );
    Ok(())
}

pub(super) fn awaiting_approval_payload(
    gate: &ManualGate,
    run_id: &str,
    attempt_number: u32,
    artifacts_dir: &str,
) -> crate::store::JsonObject {
    object(json!({
        "instructions": gate.instruction,
        "awaiting_ordinal": gate.ordinal,
        "grader_name": gate.grader_name,
        "run_id": run_id,
        "attempt_number": attempt_number,
        "artifacts_dir": artifacts_dir,
    }))
}

/// Emit `harness.rubric_invoked` / `harness.rubric_verdict` (and
/// `harness.rubric_unknown` when applicable) for one persisted record.
fn emit_rubric_events(run: &Run, attempt_number: u32, record: &GraderResultRecord) {
    let summary = record
        .payload
        .get("summary")
        .cloned()
        .unwrap_or(Value::Null);
    let unknown = record
        .payload
        .get("unknown")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let judge_model = record
        .payload
        .get("judge_model")
        .cloned()
        .unwrap_or(Value::Null);
    run.emit(
        "harness.rubric_invoked",
        object(json!({
            "grader_name": record.grader_name,
            "judge_model": judge_model,
            "attempt_number": record.attempt_number,
        })),
        Some(attempt_number),
    );
    run.emit(
        "harness.rubric_verdict",
        object(json!({
            "grader_name": record.grader_name,
            "passed": record.passed,
            "summary": summary,
            "unknown": unknown,
        })),
        Some(attempt_number),
    );
    if unknown {
        run.emit(
            "harness.rubric_unknown",
            object(json!({"grader_name": record.grader_name, "summary": summary})),
            Some(attempt_number),
        );
    }
}
