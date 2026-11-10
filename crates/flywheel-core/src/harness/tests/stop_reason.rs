//! Stop reasons (spec 00085): every finalize path records exactly one
//! reason from the vocabulary, a path the vocabulary does not name records
//! `crash`, and the already-satisfied intent is a claim that never verifies
//! the task.

use super::support::*;
use crate::envelope::{EnvelopeResult, Intent};
use crate::events::replay;
use crate::grader_rubric::RubricJudgeError;
use crate::harness::{
    FinalizeStrandedOptions, HarnessConfig, HarnessError, InvokeFn, LandabilityGate,
    finalize_stranded_lifecycle,
};
use crate::invoker::{InvocationSignals, IterationResult};
use crate::invoker_client::FAILURE_OPERATOR_INTERRUPT;
use crate::lifecycle::{Attempt, Outcome, Status, StopReason};
use crate::store::{AttemptStore, DomainEventStore, GraderResultStore, LifecycleStore};
use crate::task::{Grader, RubricGrader, Task, TranscriptGrader};
use chrono::Utc;
use std::sync::Arc;

fn no_retries() -> HarnessConfig {
    HarnessConfig {
        max_retries: 0,
        ..HarnessConfig::default()
    }
}

fn only_attempt(attempts: &[Attempt]) -> &Attempt {
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    &attempts[0]
}

/// Run `task` once with `config` and return the single attempt's reason,
/// asserting the persisted attempt row agrees with the returned one.
async fn reason_of(
    run_id: &str,
    task: &Task,
    config: &HarnessConfig,
    invoke: InvokeFn,
) -> (Status, Attempt) {
    let h = Harness::new();
    let mut lc = lifecycle(run_id);
    let outcome = h.run(task, &mut lc, config, invoke).await;
    let attempt = only_attempt(&outcome.attempts).clone();
    let stored = h.store.list_attempts(run_id).unwrap();
    assert_eq!(only_attempt(&stored).stop_reason, attempt.stop_reason);
    (outcome.lifecycle.status, attempt)
}

fn rubric(name: &str) -> Grader {
    Grader::Rubric(RubricGrader {
        assertions: vec!["a".into()],
        rubric: None,
        name: Some(name.into()),
        judge_model: None,
        retry_on_fail: true,
    })
}

fn judge_config(dir: &tempfile::TempDir, judge: Arc<ScriptedJudge>) -> HarnessConfig {
    HarnessConfig {
        max_retries: 0,
        rubric_judge_invoke: Some(judge),
        ..worktree_config(dir)
    }
}

fn rate_limited_iteration() -> IterationResult {
    let mut result = iteration(EnvelopeResult::Missing, vec![]);
    result.signals = InvocationSignals {
        api_error_status: Some(429),
        ..signals()
    };
    result
}

#[tokio::test]
async fn stop_reason_success_records_verified() {
    let (status, attempt) = reason_of(
        "run-sr-verified",
        &task(vec![ok_command()]),
        &no_retries(),
        scripted_invoker(vec![verify_iteration()]).0,
    )
    .await;
    assert_eq!(status, Status::Done);
    assert_eq!(attempt.stop_reason, Some(StopReason::Verified));
}

#[tokio::test]
async fn stop_reason_failing_command_check_names_the_check() {
    let (_, attempt) = reason_of(
        "run-sr-checks",
        &task(vec![ok_command(), command("false", Some("unit"))]),
        &no_retries(),
        scripted_invoker(vec![verify_iteration()]).0,
    )
    .await;
    assert_eq!(attempt.outcome, Some(Outcome::ValidationFailed));
    assert_eq!(
        attempt.stop_reason,
        Some(StopReason::ChecksFailed {
            check: "unit".into()
        })
    );
}

#[tokio::test]
async fn stop_reason_failing_rubric_verdict_names_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let judge = ScriptedJudge::new(vec![Ok(rubric_wrap(
        r#"{"passed": false, "summary": "nope"}"#,
    ))]);
    let (_, attempt) = reason_of(
        "run-sr-rubric",
        &task(vec![ok_command(), rubric("style")]),
        &judge_config(&dir, judge),
        scripted_invoker(vec![verify_iteration()]).0,
    )
    .await;
    assert_eq!(
        attempt.stop_reason,
        Some(StopReason::ChecksFailed {
            check: "style".into()
        })
    );
}

#[tokio::test]
async fn stop_reason_judge_failure_names_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let crash = || Err(RubricJudgeError::new("style", "network down"));
    let judge = ScriptedJudge::new(vec![crash(), crash(), crash()]);
    let (_, attempt) = reason_of(
        "run-sr-judge",
        &task(vec![ok_command(), rubric("style")]),
        &judge_config(&dir, judge),
        scripted_invoker(vec![verify_iteration()]).0,
    )
    .await;
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    assert_eq!(
        attempt.stop_reason,
        Some(StopReason::JudgeFailed {
            check: "style".into()
        })
    );
}

#[tokio::test]
async fn stop_reason_abort_records_agent_gave_up() {
    let (status, attempt) = reason_of(
        "run-sr-abort",
        &task(vec![ok_command()]),
        &no_retries(),
        scripted_invoker(vec![iteration(
            valid_reason(Intent::Abort, "cannot proceed"),
            vec![assistant()],
        )])
        .0,
    )
    .await;
    assert_eq!(status, Status::Failed);
    assert_eq!(attempt.stop_reason, Some(StopReason::AgentGaveUp));
}

#[tokio::test]
async fn stop_reason_already_satisfied_is_a_claim_that_never_verifies() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-sr-already");
    let (invoke, calls) = scripted_invoker(vec![iteration(
        valid_reason(Intent::AlreadySatisfied, "goal holds on base"),
        vec![assistant()],
    )]);
    let config = HarnessConfig {
        max_retries: 3,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    let attempt = only_attempt(&outcome.attempts);
    assert_eq!(
        attempt.stop_reason,
        Some(StopReason::AgentClaimedAlreadyDone)
    );
    // The claim is recorded, never trusted: no DONE, no success, no grading.
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_ne!(attempt.outcome, Some(Outcome::Succeeded));
    assert!(
        attempt.error.contains("goal holds on base"),
        "{}",
        attempt.error
    );
    assert!(!outcome.lifecycle.timestamps.contains_key(&Status::Done));
    assert!(
        !outcome
            .lifecycle
            .timestamps
            .contains_key(&Status::Validating)
    );
    assert!(
        h.store
            .list_grader_results("run-sr-already", 1)
            .unwrap()
            .is_empty()
    );
    // One invocation, no retry loop on the claim.
    assert_eq!(call_count(&calls), 1);
    assert_eq!(outcome.lifecycle.retries, 0);
}

#[tokio::test]
async fn stop_reason_transient_exhaustion_and_session_limit_are_distinct() {
    let config = HarnessConfig {
        max_retries: 0,
        max_transient_retries: 1,
        transient_sleep: Some(Arc::new(|_| Box::pin(async {}))),
        ..HarnessConfig::default()
    };
    let (_, attempt) = reason_of(
        "run-sr-transient",
        &task(vec![ok_command()]),
        &config,
        scripted_invoker(vec![rate_limited_iteration(), rate_limited_iteration()]).0,
    )
    .await;
    assert_eq!(attempt.stop_reason, Some(StopReason::TransientExhausted));

    let h = Harness::new();
    let mut lc = lifecycle("run-sr-session");
    let mut result = iteration(EnvelopeResult::Missing, vec![]);
    result.signals = InvocationSignals {
        rate_limit_events: vec![
            serde_json::to_value(flywheel_agents::RateLimitInfo {
                resets_at_epoch: Some(utc(2026, 7, 9, 18, 0).timestamp() as f64),
                status: Some("rejected".to_owned()),
                limit_type: Some("five_hour".to_owned()),
                detail: serde_json::Map::new(),
            })
            .unwrap(),
        ],
        ..signals()
    };
    let outcome = h
        .run_with(
            &task(vec![ok_command()]),
            &mut lc,
            &config,
            scripted_invoker(vec![result]).0,
            h.options().with_now(fixed_clock(utc(2026, 7, 9, 12, 0))),
        )
        .await
        .unwrap();
    assert_eq!(
        only_attempt(&outcome.attempts).stop_reason,
        Some(StopReason::ProviderRateLimited)
    );
}

#[tokio::test]
async fn stop_reason_deadline_records_deadline_exceeded() {
    let config = HarnessConfig {
        max_retries: 0,
        deadlines: crate::deadline_config::DeadlineConfig {
            agent_iteration_seconds: Some(0.05),
            ..crate::deadline_config::DeadlineConfig::default()
        },
        ..HarnessConfig::default()
    };
    let never: InvokeFn = Arc::new(|_| Box::pin(std::future::pending::<IterationResult>()));
    let (_, attempt) = reason_of("run-sr-deadline", &task(vec![]), &config, never).await;
    assert_eq!(attempt.stop_reason, Some(StopReason::DeadlineExceeded));
}

#[tokio::test]
async fn stop_reason_hang_records_hang() {
    let h = Harness::new();
    let mut lc = lifecycle("run-sr-hang");
    let state = Arc::new(std::sync::Mutex::new(0.0f64));
    let reader = state.clone();
    let mclock: crate::grader_transcript::MonotonicClock =
        Arc::new(move || *reader.lock().unwrap());
    let stalling: InvokeFn = Arc::new(move |_| {
        let state = state.clone();
        Box::pin(async move {
            *state.lock().unwrap() = 1000.0;
            std::future::pending::<IterationResult>().await
        })
    });
    let config = HarnessConfig {
        max_retries: 0,
        loop_guard: crate::loop_guard::LoopGuardConfig {
            repeated_tool_failure_threshold: None,
            thrash_repeat_threshold: None,
            thrash_window: None,
            hang_timeout_seconds: Some(0.04),
        },
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(
            &task(vec![]),
            &mut lc,
            &config,
            stalling,
            h.options().with_monotonic(mclock),
        )
        .await
        .unwrap();
    assert_eq!(
        only_attempt(&outcome.attempts).stop_reason,
        Some(StopReason::Hang)
    );
}

#[tokio::test]
async fn stop_reason_protocol_failure_and_non_convergence() {
    let (_, attempt) = reason_of(
        "run-sr-protocol",
        &task(vec![ok_command()]),
        &no_retries(),
        scripted_invoker(vec![iteration(EnvelopeResult::Missing, vec![assistant()])]).0,
    )
    .await;
    assert_eq!(attempt.stop_reason, Some(StopReason::ProtocolFailure));

    let config = HarnessConfig {
        max_retries: 0,
        max_iterations_per_attempt: 1,
        ..HarnessConfig::default()
    };
    let (_, attempt) = reason_of(
        "run-sr-converge",
        &task(vec![ok_command()]),
        &config,
        scripted_invoker(vec![iteration(valid(Intent::Continue), vec![assistant()])]).0,
    )
    .await;
    assert_eq!(attempt.stop_reason, Some(StopReason::DidNotConverge));
}

#[tokio::test]
async fn stop_reason_transcript_budget_breach_records_budget_exceeded() {
    let grader = Grader::Transcript(TranscriptGrader {
        max_turns: Some(1),
        max_total_tokens: None,
        max_wall_seconds: None,
        name: Some("turns".into()),
    });
    let messages = vec![assistant(), assistant(), result_msg_with(3, None)];
    let (_, attempt) = reason_of(
        "run-sr-budget",
        &task(vec![ok_command(), grader]),
        &no_retries(),
        scripted_invoker(vec![iteration(valid(Intent::Verify), messages)]).0,
    )
    .await;
    assert_eq!(attempt.stop_reason, Some(StopReason::BudgetExceeded));
}

#[tokio::test]
async fn stop_reason_cost_ceiling_records_budget_exceeded() {
    let mut result = verify_iteration();
    result.signals.total_cost_usd = Some(1.0);
    let config = HarnessConfig {
        max_retries: 0,
        max_cost_usd: 0.5,
        ..HarnessConfig::default()
    };
    let (status, attempt) = reason_of(
        "run-sr-ceiling",
        &task(vec![ok_command()]),
        &config,
        scripted_invoker(vec![result]).0,
    )
    .await;
    assert_eq!(status, Status::Failed);
    assert_eq!(attempt.stop_reason, Some(StopReason::BudgetExceeded));
}

#[tokio::test]
async fn stop_reason_landability_gate_records_landing_blocked() {
    let gate: LandabilityGate = Arc::new(|| Some("uncommitted tree".to_owned()));
    let config = HarnessConfig {
        max_retries: 0,
        landability_gate: Some(gate),
        ..HarnessConfig::default()
    };
    let (_, attempt) = reason_of(
        "run-sr-landing",
        &task(vec![ok_command()]),
        &config,
        scripted_invoker(vec![verify_iteration()]).0,
    )
    .await;
    assert_eq!(
        attempt.stop_reason,
        Some(StopReason::LandingBlocked {
            reason: "uncommitted tree".into()
        })
    );
}

#[tokio::test]
async fn stop_reason_operator_interrupt_records_operator_interrupted() {
    let h = Harness::new();
    let mut lc = lifecycle("run-sr-interrupt");
    let mut result = iteration(EnvelopeResult::Missing, vec![assistant()]);
    result.failure = Some(failure(FAILURE_OPERATOR_INTERRUPT, "interrupted"));
    let err = h
        .run_with(
            &task(vec![]),
            &mut lc,
            &no_retries(),
            scripted_invoker(vec![result]).0,
            h.options(),
        )
        .await
        .expect_err("interrupted");
    assert!(matches!(err, HarnessError::Interrupted));
    let attempts = h.store.list_attempts("run-sr-interrupt").unwrap();
    assert_eq!(
        only_attempt(&attempts).stop_reason,
        Some(StopReason::OperatorInterrupted)
    );
}

#[tokio::test]
async fn stop_reason_signaled_grader_records_operator_interrupted() {
    let (status, attempt) = reason_of(
        "run-sr-signaled",
        &task(vec![command("kill -INT $$", Some("sig"))]),
        &no_retries(),
        scripted_invoker(vec![verify_iteration()]).0,
    )
    .await;
    assert_eq!(status, Status::Interrupted);
    assert_eq!(attempt.stop_reason, Some(StopReason::OperatorInterrupted));
}

#[test]
fn stop_reason_stranded_backstop_records_worker_interrupted() {
    let h = Harness::new();
    let run_id = "run-sr-strand";
    let now = Utc::now();
    let mut lc = lifecycle(run_id);
    lc.transition_to(Status::Ready, "", Some(now)).unwrap();
    lc.transition_to(Status::Running, "", Some(now)).unwrap();
    h.store.create_lifecycle(&lc).unwrap();
    h.store
        .save_attempt(run_id, &Attempt::new(1, now, run_id), None)
        .unwrap();
    assert!(
        finalize_stranded_lifecycle(h.store.clone(), run_id, FinalizeStrandedOptions::default())
            .unwrap()
    );
    let attempts = h.store.list_attempts(run_id).unwrap();
    assert_eq!(
        only_attempt(&attempts).stop_reason,
        Some(StopReason::WorkerInterrupted)
    );
}

#[tokio::test]
async fn stop_reason_unnamed_paths_record_crash() {
    // An agent CLI crash.
    let mut crashed = iteration(EnvelopeResult::Missing, vec![]);
    crashed.failure = Some(failure("ProcessError", "cli exited 137"));
    let (_, attempt) = reason_of(
        "run-sr-crash",
        &task(vec![ok_command()]),
        &no_retries(),
        scripted_invoker(vec![crashed]).0,
    )
    .await;
    assert_eq!(attempt.stop_reason, Some(StopReason::Crash));

    // No iteration was invoked at all.
    let config = HarnessConfig {
        max_retries: 0,
        max_iterations_per_attempt: 0,
        ..HarnessConfig::default()
    };
    let (_, attempt) = reason_of(
        "run-sr-zero",
        &task(vec![ok_command()]),
        &config,
        scripted_invoker(vec![]).0,
    )
    .await;
    assert_eq!(attempt.stop_reason, Some(StopReason::Crash));
}

/// Every finalize path leaves a reason on the attempt, and the reasons on
/// the persisted rows are exactly what the domain-event ledger replays to.
#[tokio::test]
async fn stop_reason_is_set_on_every_finalized_attempt_and_survives_replay() {
    let h = Harness::new();
    let cases: Vec<(&str, Task, HarnessConfig, Vec<IterationResult>)> = vec![
        (
            "run-sr-all-ok",
            task(vec![ok_command()]),
            no_retries(),
            vec![verify_iteration()],
        ),
        (
            "run-sr-all-retry",
            task(vec![fail_command()]),
            HarnessConfig {
                max_retries: 2,
                ..HarnessConfig::default()
            },
            vec![
                verify_iteration(),
                iteration(EnvelopeResult::Missing, vec![assistant()]),
                iteration(
                    valid_reason(Intent::AlreadySatisfied, "on base"),
                    vec![assistant()],
                ),
            ],
        ),
        (
            "run-sr-all-blocked",
            task(vec![command("true", Some("db"))]),
            no_retries(),
            vec![iteration(
                EnvelopeResult::Valid(crate::envelope::ValidEnvelope {
                    intent: Intent::Blocked,
                    reason: Some("needs db".into()),
                    requires: vec![crate::envelope::BlockedRequirement::CommandGrader {
                        name: "db".into(),
                    }],
                }),
                vec![assistant()],
            )],
        ),
    ];
    for (run_id, task, config, results) in cases {
        let mut lc = lifecycle(run_id);
        let outcome = h
            .run(&task, &mut lc, &config, scripted_invoker(results).0)
            .await;
        assert!(!outcome.attempts.is_empty());
        for attempt in &outcome.attempts {
            assert!(attempt.ended_at.is_some(), "{run_id}");
            assert!(attempt.stop_reason.is_some(), "{run_id}: {attempt:?}");
        }
        let events = h.store.list_domain_events(run_id).unwrap();
        let folded = replay(&events).unwrap();
        let stored = h.store.list_attempts(run_id).unwrap();
        let replayed: Vec<_> = folded.attempts.iter().map(|a| &a.stop_reason).collect();
        let persisted: Vec<_> = stored.iter().map(|a| &a.stop_reason).collect();
        assert_eq!(replayed, persisted, "{run_id}");
        let loaded = h.store.load_lifecycle(run_id).unwrap().unwrap();
        assert_eq!(loaded.attempts.len(), stored.len());
    }
    let retried = h.store.list_attempts("run-sr-all-retry").unwrap();
    let reasons: Vec<_> = retried.iter().map(|a| a.stop_reason.clone()).collect();
    assert_eq!(
        reasons,
        [
            // An unnamed grader is labelled by its type.
            Some(StopReason::ChecksFailed {
                check: "command".into()
            }),
            Some(StopReason::ProtocolFailure),
            Some(StopReason::AgentClaimedAlreadyDone),
        ]
    );
    // A blocked pause has no vocabulary name and records crash.
    let blocked = h.store.list_attempts("run-sr-all-blocked").unwrap();
    assert_eq!(only_attempt(&blocked).stop_reason, Some(StopReason::Crash));
}
