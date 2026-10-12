//! One attempt: invoke -> envelope -> intent -> graders -> finalize, plus
//! the loop-guard, context-recovery, and budget-ceiling routings.

use super::iterate::{
    Driven, RecoveryTrigger, build_observation, drive_iterations, envelope_payload,
    envelope_protocol_error, transcript_graders,
};
use super::requires::{requires_json, serialize_requires, validate_blocked_requires_against_task};
use super::state::{AttemptError, Run, object};
use super::validate::validate;
use super::{
    BUDGET_CEILING_ERROR_PREFIX, HarnessConfig, HarnessError, InvokeFn, RecoveryState,
    SessionLimitState,
};
use crate::envelope::{EnvelopeResult, Intent};
use crate::events::{AttemptStarted, Blocked, DomainEventPayload};
use crate::grader_transcript::first_breach;
use crate::grader_transcript::run_transcript_graders;
use crate::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use crate::loop_guard::{LoopGuard, LoopGuardVerdict, LoopGuardVerdictKind};
use crate::prompt::RecoveryHandoff;
use crate::recovery_summarizer::{
    AgentsSummarizerInvoke, SummarizerInvoke, run_recovery_summarizer,
};
use crate::store::AttemptStore;
use crate::task::{Task, python_float_repr};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Create (if needed) and return the per-attempt artifact directory:
/// `artifacts_root` (or the lifecycle's `artifacts_dir`) joined with
/// `attempt-NNN`; `None` when neither base is configured.
fn ensure_attempt_dir(
    config: &HarnessConfig,
    lifecycle: &Lifecycle,
    attempt_number: u32,
) -> Result<Option<PathBuf>, std::io::Error> {
    let base = match &config.artifacts_root {
        Some(root) => root.clone(),
        None if !lifecycle.artifacts_dir.is_empty() => PathBuf::from(&lifecycle.artifacts_dir),
        None => return Ok(None),
    };
    let path = base.join(format!("attempt-{attempt_number:03}"));
    std::fs::create_dir_all(&path)?;
    Ok(Some(path))
}

/// Run one attempt and route every exit of its body.
pub(super) async fn run_attempt(
    task: &Task,
    run: &Run,
    config: &HarnessConfig,
    invoker: &InvokeFn,
    recovery_state: &mut RecoveryState,
    session_state: &mut SessionLimitState,
) -> Result<(), HarnessError> {
    run.transition(Status::Running, "")?;

    let attempt_number = u32::try_from(run.store.list_attempts(&run.run_id)?.len())
        .expect("attempt count fits in u32")
        + 1;
    let started_at = run.now();
    let mut attempt = Attempt::new(attempt_number, started_at, run.run_id.clone());
    attempt.agent_context = config.agent_context.clone();
    run.append_payload(
        started_at,
        Some(attempt_number),
        DomainEventPayload::AttemptStarted(AttemptStarted {
            number: attempt_number,
            attempt_run_id: run.run_id.clone(),
            started_at,
            agent_context: config.agent_context.clone(),
        }),
    )?;
    run.emit(
        "harness.attempt_started",
        object(json!({
            "number": attempt_number,
            "agent_context": config.agent_context,
        })),
        Some(attempt_number),
    );

    let outcome = run_attempt_body(
        task,
        run,
        config,
        invoker,
        &mut attempt,
        recovery_state,
        session_state,
    )
    .await;
    match outcome {
        Ok(()) => Ok(()),
        // Hang watchdog / wall-clock deadline: the timeout-classified
        // containment path (INTERNAL_ERROR), never the operator-interrupt
        // path. The audit event was emitted before the cancel.
        Err(err @ (AttemptError::Hang { .. } | AttemptError::Deadline { .. })) => {
            run.handle_timeout_class(&mut attempt, &err.error_text())?;
            Ok(())
        }
        // Operator-driven shutdown: close the in-flight attempt to
        // INTERRUPTED in-band so no lifecycle is left wedged, then surface
        // the interruption so the worker actually stops.
        Err(AttemptError::Interrupted) => {
            run.handle_interrupt(Some(&mut attempt));
            Err(HarnessError::Interrupted)
        }
        // Any other failure escaping the body is an entry-class crash:
        // finalize the open attempt here (run_task's handler holds no
        // reference to it) so a terminal FAILED lifecycle never strands an
        // attempt with ended_at unset, then propagate unchanged.
        Err(AttemptError::Failed(err)) => {
            if attempt.ended_at.is_none() {
                let error = format!("attempt crashed: {}: {err}", err.error_type());
                let _ = run.finalize_attempt(&mut attempt, Outcome::InternalError, &error, "");
            }
            Err(err)
        }
    }
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

#[allow(clippy::too_many_arguments)]
async fn run_attempt_body(
    task: &Task,
    run: &Run,
    config: &HarnessConfig,
    invoker: &InvokeFn,
    attempt: &mut Attempt,
    recovery_state: &mut RecoveryState,
    session_state: &mut SessionLimitState,
) -> Result<(), AttemptError> {
    let attempt_number = attempt.number;
    let attempt_dir =
        ensure_attempt_dir(config, &run.snapshot(), attempt_number).map_err(HarnessError::Io)?;
    let graders = transcript_graders(task);
    // Fresh LoopGuard per attempt: repeated-failure / thrash counters must
    // not bleed across attempts.
    let mut loop_guard = LoopGuard::new(config.loop_guard.clone());
    let Driven {
        result,
        iterations_run: _,
        wall_seconds,
        loop_guard_verdict,
        recovery_trigger,
        transient_exhausted,
        session_limit_reset,
    } = drive_iterations(
        run,
        task,
        config,
        invoker,
        attempt,
        &graders,
        &mut loop_guard,
        recovery_state,
    )
    .await?;

    // Mid-turn recovery: the iteration was interrupted in flight and never
    // produced a result; the streamed work-so-far is the attempt's output.
    let result = match (result, &recovery_trigger) {
        (None, Some(trigger)) => {
            run.with_lifecycle(|lc| lc.agent_output = trigger.transcript_tail.clone());
            return handle_context_recovery(task, run, config, attempt, trigger, recovery_state)
                .await;
        }
        (None, None) => {
            // No invocation happened (cap <= 0): a protocol-class agent
            // error so the retry policy can still kick in.
            let error = "no iterations were invoked";
            run.transition(Status::Validating, "")?;
            return finalize_and_transition(
                run,
                attempt,
                Outcome::AgentError,
                error,
                "",
                Status::FailedValidation,
            );
        }
        (Some(result), _) => result,
    };

    run.with_lifecycle(|lc| lc.agent_output = result.transcript.clone());

    // Transient rate-limit exhaustion / session-limit fast-abort: an
    // infrastructure fault routed to the retryable INTERNAL_ERROR class.
    if transient_exhausted {
        let error = match session_limit_reset {
            Some(reset) => {
                session_state.reset_at = Some(reset);
                format!(
                    "session-limit refusal; reset at {}",
                    crate::event_serde::format_timestamp(&reset)
                )
            }
            None => format!(
                "transient rate-limit retries exhausted after {} invocations",
                config.max_transient_retries + 1
            ),
        };
        return finalize_and_transition(
            run,
            attempt,
            Outcome::InternalError,
            &error,
            &result.transcript,
            Status::InternalError,
        );
    }

    // Crash takes priority; refined classification is deferred.
    if let Some(failure) = &result.failure {
        let crash_error = format!("crashed: {}: {}", failure.error_type, failure.message);
        run.emit(
            "harness.crash",
            object(json!({
                "error_type": failure.error_type,
                "message": failure.message,
                "exit_code": failure.exit_code,
                "stderr": failure.stderr,
                "classification": "deferred",
            })),
            Some(attempt_number),
        );
        return finalize_and_transition(
            run,
            attempt,
            Outcome::InternalError,
            &crash_error,
            &result.transcript,
            Status::InternalError,
        );
    }

    // Loop-guard verdicts preempt envelope interpretation.
    if let Some(verdict) = loop_guard_verdict {
        return match verdict.kind {
            LoopGuardVerdictKind::Stuck => {
                handle_loop_guard_stuck(run, attempt, &verdict, &result.transcript)
            }
            LoopGuardVerdictKind::Thrash => {
                handle_loop_guard_thrash(run, attempt, &verdict, &result.transcript)
            }
        };
    }

    // Boundary context recovery (spec 00018 FR-3).
    if let Some(trigger) = &recovery_trigger {
        return handle_context_recovery(task, run, config, attempt, trigger, recovery_state).await;
    }

    // Per-run budget ceilings: checked after the rollup, before grading;
    // the first breach ends the run terminal FAILED.
    if config.max_cost_usd > 0.0 || config.max_tokens > 0 || config.wall_clock_seconds > 0 {
        let prior: Vec<Attempt> = run
            .store
            .list_attempts(&run.run_id)?
            .into_iter()
            .filter(|a| a.number != attempt.number)
            .collect();
        if config.max_cost_usd > 0.0 {
            let run_total_cost: f64 =
                prior.iter().map(|a| a.total_cost_usd).sum::<f64>() + attempt.total_cost_usd;
            if run_total_cost >= config.max_cost_usd {
                return finalize_budget_breach(
                    run,
                    attempt,
                    "cost_usd",
                    json!(config.max_cost_usd),
                    json!(run_total_cost),
                    &result.transcript,
                );
            }
        }
        if config.max_tokens > 0 {
            let run_total_tokens: u64 =
                prior.iter().map(Attempt::total_tokens).sum::<u64>() + attempt.total_tokens();
            if run_total_tokens >= config.max_tokens {
                return finalize_budget_breach(
                    run,
                    attempt,
                    "tokens",
                    json!(config.max_tokens),
                    json!(run_total_tokens),
                    &result.transcript,
                );
            }
        }
        if config.wall_clock_seconds > 0 {
            let run_started = prior
                .iter()
                .map(|a| a.started_at)
                .chain(std::iter::once(attempt.started_at))
                .min()
                .expect("at least the current attempt");
            let elapsed_seconds = (run.now() - run_started).as_seconds_f64();
            if elapsed_seconds >= config.wall_clock_seconds as f64 {
                return finalize_budget_breach(
                    run,
                    attempt,
                    "wall_clock_seconds",
                    json!(config.wall_clock_seconds),
                    json!(elapsed_seconds),
                    &result.transcript,
                );
            }
        }
    }

    // ABORT and BLOCKED leave RUNNING directly without entering VALIDATING:
    // agent-reported terminal / pause signals, not validation outcomes.
    if let EnvelopeResult::Valid(envelope) = &result.envelope {
        if envelope.intent == Intent::Abort {
            let reason = envelope
                .reason
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "agent reported abort intent".to_owned());
            return finalize_and_transition(
                run,
                attempt,
                Outcome::AgentError,
                &reason,
                &result.transcript,
                Status::Failed,
            );
        }
        if envelope.intent == Intent::Blocked {
            // Task-aware validation: an unresolvable command_grader
            // predicate is a protocol failure, not a benign block.
            if let Some(requires_error) =
                validate_blocked_requires_against_task(&envelope.requires, task)
            {
                let protocol_error =
                    format!("protocol failure: invalid blocked requires: {requires_error}");
                run.transition(Status::Validating, "")?;
                run.emit(
                    "harness.protocol_failure",
                    object(json!({
                        "kind": "invalid_blocked_requires",
                        "reason": requires_error,
                        "intent": envelope.intent.as_str(),
                    })),
                    Some(attempt_number),
                );
                return finalize_and_transition(
                    run,
                    attempt,
                    Outcome::AgentError,
                    &protocol_error,
                    &result.transcript,
                    Status::FailedValidation,
                );
            }
            let reason = envelope
                .reason
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "agent reported blocked intent".to_owned());
            let requires_payload = serialize_requires(&envelope.requires);
            run.finalize_attempt(attempt, Outcome::Cancelled, &reason, &result.transcript)?;
            // Persist the structured snapshot before the INTERRUPTED
            // transition so the projection carries it when the status
            // change lands; the reducer clears it on every -> READY edge.
            run.append_payload(
                run.now(),
                Some(attempt_number),
                DomainEventPayload::Blocked(Blocked {
                    requires_json: requires_json(&requires_payload),
                }),
            )?;
            run.emit(
                "harness.blocked",
                object(json!({"reason": reason, "requires": requires_payload})),
                Some(attempt_number),
            );
            run.transition(Status::Interrupted, "")?;
            return Ok(());
        }
    }

    // Everything else (verify, breach, protocol failure, continue past cap)
    // routes through VALIDATING, the only legal predecessor of
    // FAILED_VALIDATION / DONE.
    run.transition(Status::Validating, "")?;

    let observation = build_observation(&result.messages, wall_seconds);
    let breach = graders.iter().find_map(|g| first_breach(g, &observation));
    if let Some(breach) = breach {
        run.emit(
            "harness.budget_exceeded",
            object(json!({
                "breached": breach.as_str(),
                "observed": {
                    "turns": observation.turns,
                    "total_tokens": observation.total_tokens,
                    "wall_seconds": observation.wall_seconds,
                },
            })),
            Some(attempt_number),
        );
        let clock = run.clock.clone();
        run_transcript_graders(
            task,
            &observation,
            run.store.as_ref(),
            &run.run_id,
            attempt_number,
            true,
            move || clock(),
        )?;
        let error = format!("budget exceeded: {}", breach.as_str());
        return finalize_and_transition(
            run,
            attempt,
            Outcome::ValidationFailed,
            &error,
            &result.transcript,
            Status::FailedValidation,
        );
    }

    match &result.envelope {
        EnvelopeResult::Valid(envelope) if envelope.intent == Intent::Verify => {
            validate(
                task,
                run,
                config,
                attempt,
                attempt_dir.as_deref(),
                &observation,
                &result.transcript,
            )
            .await
        }
        EnvelopeResult::Valid(envelope) if envelope.intent == Intent::Continue => {
            let cap = config.max_iterations_per_attempt;
            let error = format!(
                "agent did not converge after {cap} iteration{}",
                if cap != 1 { "s" } else { "" }
            );
            finalize_and_transition(
                run,
                attempt,
                Outcome::AgentError,
                &error,
                &result.transcript,
                Status::FailedValidation,
            )
        }
        envelope => {
            // Protocol failure: malformed / missing / duplicate / truncated.
            let protocol_error = envelope_protocol_error(envelope);
            run.emit(
                "harness.protocol_failure",
                envelope_payload(envelope),
                Some(attempt_number),
            );
            finalize_and_transition(
                run,
                attempt,
                Outcome::AgentError,
                &protocol_error,
                &result.transcript,
                Status::FailedValidation,
            )
        }
    }
}

/// The STUCK verdict rendered as a structured `requires_json` payload so the
/// recoverable-blocked recovery flow applies unchanged.
fn loop_guard_requires_payload(verdict: &LoopGuardVerdict) -> Vec<Value> {
    vec![json!({
        "type": "tool_loop_block",
        "tool_name": verdict.tool_name,
        "input_digest": verdict.input_digest,
    })]
}

/// Route a STUCK verdict through the recoverable-blocked path: finalize
/// `CANCELLED`, append a `Blocked` snapshot naming the failing tool, emit
/// `harness.stuck`, transition `running -> interrupted`. The retry budget
/// is not consumed.
fn handle_loop_guard_stuck(
    run: &Run,
    attempt: &mut Attempt,
    verdict: &LoopGuardVerdict,
    transcript: &str,
) -> Result<(), AttemptError> {
    let requires_payload = loop_guard_requires_payload(verdict);
    run.finalize_attempt(attempt, Outcome::Cancelled, &verdict.reason, transcript)?;
    run.append_payload(
        run.now(),
        Some(attempt.number),
        DomainEventPayload::Blocked(Blocked {
            requires_json: requires_json(&requires_payload),
        }),
    )?;
    run.emit(
        "harness.stuck",
        object(json!({
            "reason": verdict.reason,
            "tool_name": verdict.tool_name,
            "input_digest": verdict.input_digest,
            "requires": requires_payload,
        })),
        Some(attempt.number),
    );
    run.transition(Status::Interrupted, "")?;
    Ok(())
}

/// Route a THRASH verdict through the cap-reached agent-error path: emit
/// `harness.thrash_detected`, transition through VALIDATING, finalize
/// `AGENT_ERROR`, transition into FAILED_VALIDATION for the retry arm.
fn handle_loop_guard_thrash(
    run: &Run,
    attempt: &mut Attempt,
    verdict: &LoopGuardVerdict,
    transcript: &str,
) -> Result<(), AttemptError> {
    run.emit(
        "harness.thrash_detected",
        object(json!({
            "reason": verdict.reason,
            "tool_name": verdict.tool_name,
            "input_digest": verdict.input_digest,
        })),
        Some(attempt.number),
    );
    run.transition(Status::Validating, "")?;
    finalize_and_transition(
        run,
        attempt,
        Outcome::AgentError,
        &verdict.reason,
        transcript,
        Status::FailedValidation,
    )
}

/// The audit-event digest of a recovery handoff: per-field lengths only.
fn handoff_digest(handoff: &RecoveryHandoff) -> Value {
    json!({
        "work_done_length": handoff.work_done.chars().count(),
        "work_remaining_length": handoff.work_remaining.chars().count(),
        "key_decisions_length": handoff.key_decisions.chars().count(),
        "suggested_next_step_length": handoff.suggested_next_step.chars().count(),
    })
}

/// Concatenate every prior attempt's final transcript for the summarizer's
/// `cumulative_diff` argument.
fn cumulative_prior_outputs(run: &Run) -> Result<String, AttemptError> {
    let prior: Vec<String> = run
        .store
        .list_attempts(&run.run_id)?
        .into_iter()
        .filter(|a| !a.agent_output.is_empty())
        .map(|a| a.agent_output)
        .collect();
    Ok(prior.join("\n\n---\n\n"))
}

/// The summarize-restart action (spec 00018 FR-3 / spec 00019 FR-4).
///
/// Invoke the summarizer; on failure route through INTERNAL_ERROR (recovery
/// never silently restarts with an empty handoff). On success emit
/// `harness.context_recovery` BEFORE the next attempt's start, finalize this
/// attempt `RECOVERED`, and walk `running -> interrupted -> ready` so the
/// outer loop schedules the recovery attempt with the handoff threaded into
/// its first prompt. The retry counter is untouched.
async fn handle_context_recovery(
    task: &Task,
    run: &Run,
    config: &HarnessConfig,
    attempt: &mut Attempt,
    trigger: &RecoveryTrigger,
    recovery_state: &mut RecoveryState,
) -> Result<(), AttemptError> {
    let cumulative_diff = cumulative_prior_outputs(run)?;
    let summarizer: Arc<dyn SummarizerInvoke> = match &config.recovery_summarizer_invoke {
        Some(seam) => seam.clone(),
        None => Arc::new(AgentsSummarizerInvoke::new(
            &config.agent_id,
            None,
            config.recovery_summarizer_max_turns,
        )),
    };
    let handoff = match run_recovery_summarizer(
        task,
        &trigger.transcript_tail,
        &cumulative_diff,
        config.worktree.as_deref(),
        summarizer.as_ref(),
    )
    .await
    {
        Ok(handoff) => handoff,
        Err(exc) => {
            let error = format!("recovery summarizer failed: {}", exc.reason);
            run.emit(
                "harness.crash",
                object(json!({
                    "classification": "recovery_summarizer_error",
                    "reason": exc.reason,
                    "message": error,
                    "trigger": trigger.trigger,
                })),
                Some(attempt.number),
            );
            return finalize_and_transition(
                run,
                attempt,
                Outcome::InternalError,
                &error,
                &trigger.transcript_tail,
                Status::InternalError,
            );
        }
    };

    // Reserve the budget slot before the audit event so the figures reflect
    // the count after this recovery; boundary and mid-turn share the slot.
    let recoveries_used = recovery_state
        .recoveries_used
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    let recoveries_remaining = config
        .max_context_recoveries
        .saturating_sub(recoveries_used);
    run.emit(
        "harness.context_recovery",
        object(json!({
            "iteration": trigger.iteration_number,
            "attempt_number": attempt.number,
            "occupancy_tokens": trigger.occupancy_tokens,
            "context_window_tokens": config.context_window_tokens,
            "context_recovery_trigger_ratio": config.context_recovery_trigger_ratio,
            "recoveries_used": recoveries_used,
            "recoveries_remaining": recoveries_remaining,
            "summary_digest": handoff_digest(&handoff),
            "trigger": trigger.trigger,
        })),
        Some(attempt.number),
    );
    run.finalize_attempt(attempt, Outcome::Recovered, "", &trigger.transcript_tail)?;
    // Walk the legal edge set back to READY without consuming a validation
    // retry (RUNNING has no direct edge to READY).
    run.transition(Status::Interrupted, "")?;
    run.transition(Status::Ready, "")?;
    recovery_state.pending_handoff = Some(handoff);
    Ok(())
}

/// Render a JSON number the way Python's `str()` would.
fn python_number(value: &Value) -> String {
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                python_float_repr(n.as_f64().unwrap_or(0.0))
            }
        }
        other => other.to_string(),
    }
}

/// End a run that breached a per-run budget ceiling (specs 00039/00042):
/// finalize `AGENT_ERROR` (the ABORT shape), emit
/// `harness.budget_ceiling_breached`, transition RUNNING -> FAILED.
fn finalize_budget_breach(
    run: &Run,
    attempt: &mut Attempt,
    ceiling: &str,
    limit: Value,
    observed: Value,
    transcript: &str,
) -> Result<(), AttemptError> {
    let breach_error = format!(
        "{BUDGET_CEILING_ERROR_PREFIX} {ceiling} {} >= limit {}",
        python_number(&observed),
        python_number(&limit)
    );
    run.finalize_attempt(attempt, Outcome::AgentError, &breach_error, transcript)?;
    run.emit(
        "harness.budget_ceiling_breached",
        object(json!({"ceiling": ceiling, "limit": limit, "observed": observed})),
        Some(attempt.number),
    );
    run.transition(Status::Failed, &breach_error)?;
    Ok(())
}
