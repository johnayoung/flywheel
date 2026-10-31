//! Core run scenarios: success, retry policy, protocol failures, blocked /
//! abort, crash, transcript budget, the continue loop, entry normalization,
//! rate limits, session limits, iteration telemetry, the attempt rollup,
//! operator interruption, stranded finalization, entry crashes, the
//! telemetry stream, and config defaults.

use super::support::*;
use crate::envelope::{BlockedRequirement, EnvelopeResult, Intent, ValidEnvelope};
use crate::events::{DomainEventKind, replay};
use crate::harness::state::Run;
use crate::harness::telemetry::{MirroringStore, RunTelemetry};
use crate::harness::{
    DEFERRED_LOOP_SUBSYSTEMS, FinalizeStrandedOptions, HarnessConfig, HarnessError, RunOptions,
    ShutdownSignal, build_observation, build_usage_breakdown, finalize_stranded_lifecycle,
    run_task,
};
use crate::invoker::{InvocationSignals, IterationResult};
use crate::invoker_client::FAILURE_OPERATOR_INTERRUPT;
use crate::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use crate::loaders::task_digest;
use crate::store::{
    AttemptStore, DomainEventStore, GraderResultStore, HarnessStore, LifecycleStore, TaskStore,
};
use crate::store_memory::InMemoryStore;
use crate::task::{Grader, Task, TranscriptGrader};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn transcript_grader(max_turns: Option<u64>, max_total_tokens: Option<u64>, name: &str) -> Grader {
    Grader::Transcript(TranscriptGrader {
        max_turns,
        max_total_tokens,
        max_wall_seconds: None,
        name: Some(name.into()),
    })
}

// --- Successful run -------------------------------------------------------

#[tokio::test]
async fn verify_then_all_graders_pass_reaches_done() {
    let h = Harness::new();
    let task = task(vec![
        ok_command(),
        transcript_grader(Some(10), None, "caps"),
    ]);
    let mut lc = lifecycle("run-success");
    let mut result = verify_iteration();
    result.transcript = wrap(r#"{"intent": "verify", "reason": "all done"}"#);
    let (invoke, _) = scripted_invoker(vec![result]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Succeeded));
    assert!(outcome.attempts[0].ended_at.is_some());
    let rows = h.store.list_grader_results("run-success", 1).unwrap();
    let types: Vec<_> = rows.iter().map(|r| r.grader_type.as_str()).collect();
    assert_eq!(types, ["command", "transcript"]);
    assert!(rows.iter().all(|r| r.passed));
    // The caller's handle reflects the same terminal state.
    assert_eq!(lc.status, Status::Done);
}

#[tokio::test]
async fn agent_context_persisted_on_attempt() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-ctx");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let ctx = [
        ("model_id", "claude-opus-4-7"),
        ("model_version", "2026-01-01"),
        ("agent_sdk_version", "0.1.0"),
        ("prompt_template_hash", "abc123"),
    ];
    let config = HarnessConfig {
        agent_context: ctx
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.attempts[0].agent_context, config.agent_context);
    let reloaded = h.store.list_attempts("run-ctx").unwrap();
    assert_eq!(reloaded[0].agent_context["model_id"], "claude-opus-4-7");
}

#[tokio::test]
async fn command_grader_runs_in_worktree() {
    // A relative-path check passes only if the grader's CWD is the sandbox.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sentinel.flag"), "ok").unwrap();
    let h = Harness::new();
    let task = task(vec![command(
        "test -f sentinel.flag",
        Some("relative-check"),
    )]);
    let mut lc = lifecycle("run-wt-cwd");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h.run(&task, &mut lc, &worktree_config(&dir), invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
}

#[tokio::test]
async fn per_attempt_artifact_directory_created() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![command("echo hi", Some("echo"))]);
    let mut lc = lifecycle("run-art");
    let config = HarnessConfig {
        artifacts_root: Some(dir.path().to_path_buf()),
        ..HarnessConfig::default()
    };
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    h.run(&task, &mut lc, &config, invoke).await;
    let attempt_dir = dir.path().join("attempt-001");
    assert!(attempt_dir.is_dir());
    let produced: Vec<String> = std::fs::read_dir(&attempt_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(produced.iter().any(|n| n.starts_with("grader-000")));
}

// --- Retry policy ---------------------------------------------------------

#[tokio::test]
async fn validation_failure_retries_when_budget_remains() {
    // Grader passes only on the second invocation (marker file).
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![command(
        "if [ -f marker ]; then exit 0; else touch marker; exit 1; fi",
        None,
    )]);
    let mut lc = lifecycle("run-retry");
    let (invoke, _) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let config = HarnessConfig {
        max_retries: 1,
        ..worktree_config(&dir)
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.lifecycle.retries, 1);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::ValidationFailed));
    assert_eq!(outcome.attempts[1].outcome, Some(Outcome::Succeeded));
    assert!(
        h.sink
            .kinds("run-retry")
            .contains(&"harness.retry_scheduled".to_string())
    );
}

#[tokio::test]
async fn validation_failure_terminates_when_retries_exhausted() {
    let h = Harness::new();
    let task = task(vec![fail_command()]);
    let mut lc = lifecycle("run-exhaust");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.lifecycle.retries, 0);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::ValidationFailed));
    assert_eq!(
        outcome.attempts[0].error,
        "command grader \"command\" failed"
    );
    assert!(
        !h.sink
            .kinds("run-exhaust")
            .contains(&"harness.retry_scheduled".to_string())
    );
}

// --- Envelope protocol failures -------------------------------------------

#[tokio::test]
async fn protocol_failure_routes_to_failed_validation() {
    let cases: Vec<(EnvelopeResult, &str)> = vec![
        (EnvelopeResult::Missing, "missing"),
        (
            EnvelopeResult::Truncated {
                detail: "opening only".into(),
            },
            "truncated",
        ),
        (EnvelopeResult::Duplicate { count: 2 }, "duplicate"),
        (
            EnvelopeResult::Malformed {
                reason: "bad json".into(),
                offending: None,
            },
            "malformed",
        ),
    ];
    for (envelope, expected) in cases {
        let h = Harness::new();
        let task = task(vec![ok_command()]);
        let run_id = format!("run-proto-{expected}");
        let mut lc = lifecycle(&run_id);
        let (invoke, _) =
            scripted_invoker(vec![iteration(envelope, vec![assistant(), result_msg()])]);
        let outcome = h
            .run(&task, &mut lc, &HarnessConfig::default(), invoke)
            .await;
        assert_eq!(outcome.lifecycle.status, Status::Failed);
        let attempt = &outcome.attempts[0];
        assert_eq!(attempt.outcome, Some(Outcome::AgentError));
        assert!(attempt.error.contains(expected), "{}", attempt.error);
        // No command grader rows: validation did not progress.
        assert!(h.store.list_grader_results(&run_id, 1).unwrap().is_empty());
        let proto = h.sink.of_kind(&run_id, "harness.protocol_failure");
        assert_eq!(proto.len(), 1);
        assert_eq!(proto[0].payload["kind"], expected);
    }
}

#[tokio::test]
async fn protocol_failure_does_not_silently_coerce_to_continue() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-no-coerce");
    let (invoke, calls) = scripted_invoker(vec![iteration(EnvelopeResult::Missing, vec![])]);
    let config = HarnessConfig {
        max_iterations_per_attempt: 5,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(call_count(&calls), 1);
}

// --- Blocked / abort ------------------------------------------------------

#[tokio::test]
async fn blocked_transitions_to_interrupted_and_preserves_retries() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("full-suite"))]);
    let mut lc = lifecycle("run-blocked");
    let envelope = EnvelopeResult::Valid(ValidEnvelope {
        intent: Intent::Blocked,
        reason: Some("need API key".into()),
        requires: vec![
            BlockedRequirement::CommandGrader {
                name: "full-suite".into(),
            },
            BlockedRequirement::EnvVarSet {
                name: "MISSING_KEY".into(),
            },
        ],
    });
    let (invoke, _) = scripted_invoker(vec![iteration(envelope, vec![assistant()])]);
    let config = HarnessConfig {
        max_retries: 5,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.lifecycle.retries, 0);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::Cancelled));
    assert!(attempt.error.contains("need API key"));
    assert!(
        h.store
            .list_grader_results("run-blocked", 1)
            .unwrap()
            .is_empty()
    );
    let expected_requires = json!([
        {"type": "command_grader", "name": "full-suite"},
        {"type": "env_var_set", "name": "MISSING_KEY"},
    ]);
    // Python json.dumps spacing on the persisted column.
    assert_eq!(
        outcome.lifecycle.blocked_requires_json.as_deref(),
        Some(
            r#"[{"type": "command_grader", "name": "full-suite"}, {"type": "env_var_set", "name": "MISSING_KEY"}]"#
        )
    );
    let blocked = h.sink.of_kind("run-blocked", "harness.blocked");
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].payload["reason"], "need API key");
    assert_eq!(blocked[0].payload["requires"], expected_requires);
}

#[tokio::test]
async fn blocked_with_unknown_command_grader_routes_through_protocol_failure() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("other"))]);
    let mut lc = lifecycle("run-bad-blocked");
    let envelope = EnvelopeResult::Valid(ValidEnvelope {
        intent: Intent::Blocked,
        reason: Some("need help".into()),
        requires: vec![BlockedRequirement::CommandGrader {
            name: "not-on-task".into(),
        }],
    });
    let (invoke, _) = scripted_invoker(vec![iteration(envelope, vec![assistant()])]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::AgentError));
    assert!(attempt.error.contains("invalid blocked requires"));
    assert_eq!(outcome.lifecycle.blocked_requires_json, None);
    let kinds = h.sink.kinds("run-bad-blocked");
    assert!(!kinds.contains(&"harness.blocked".to_string()));
    assert!(kinds.contains(&"harness.protocol_failure".to_string()));
}

#[tokio::test]
async fn abort_transitions_to_failed() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-abort");
    let (invoke, _) = scripted_invoker(vec![iteration(
        valid_reason(Intent::Abort, "cannot proceed"),
        vec![assistant()],
    )]);
    let config = HarnessConfig {
        max_retries: 5,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert!(outcome.lifecycle.error.contains("cannot proceed"));
    assert_eq!(outcome.lifecycle.retries, 0);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::AgentError));
    // Abort never enters VALIDATING.
    assert!(
        !outcome
            .lifecycle
            .timestamps
            .contains_key(&Status::Validating)
    );
}

// --- Crash ----------------------------------------------------------------

#[tokio::test]
async fn invoker_failure_transitions_to_failed() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-crash");
    let mut result = iteration(EnvelopeResult::Missing, vec![]);
    result.failure = Some(crate::invoker::InvocationFailure {
        error_type: "ProcessError".into(),
        message: "cli exited 137".into(),
        exit_code: Some(137),
        stderr: None,
    });
    let (invoke, _) = scripted_invoker(vec![result]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    assert!(attempt.error.contains("ProcessError"));
    let crashes = h.sink.of_kind("run-crash", "harness.crash");
    assert_eq!(crashes.len(), 1);
    assert_eq!(crashes[0].payload["classification"], "deferred");
    assert_eq!(crashes[0].payload["exit_code"], 137);
}

// --- Transcript budget breach ---------------------------------------------

#[tokio::test]
async fn max_turns_breach_routes_to_failed_validation() {
    let h = Harness::new();
    let task = task(vec![
        ok_command(),
        transcript_grader(Some(2), None, "turns-cap"),
    ]);
    let mut lc = lifecycle("run-budget");
    let mut messages: Vec<Value> = (0..5).map(|_| assistant()).collect();
    messages.push(result_msg_with(5, None));
    let (invoke, _) = scripted_invoker(vec![iteration(EnvelopeResult::Missing, messages)]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::ValidationFailed));
    assert!(attempt.error.contains("max_turns"));
    let rows = h.store.list_grader_results("run-budget", 1).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].grader_type.as_str(), "transcript");
    assert!(!rows[0].passed);
    assert_eq!(rows[0].payload["breached"], "max_turns");
}

#[tokio::test]
async fn budget_event_emitted_with_observed_totals() {
    let h = Harness::new();
    let task = task(vec![
        ok_command(),
        transcript_grader(None, Some(10), "tok-cap"),
    ]);
    let mut lc = lifecycle("run-budget-evt");
    let messages = vec![
        assistant_usage(json!({"input_tokens": 30, "output_tokens": 20})),
        result_msg_with(1, None),
    ];
    let (invoke, _) = scripted_invoker(vec![iteration(valid(Intent::Verify), messages)]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let events = h.sink.of_kind("run-budget-evt", "harness.budget_exceeded");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["breached"], "max_total_tokens");
    assert_eq!(events[0].payload["observed"]["total_tokens"], 50);
}

// --- Continue loop --------------------------------------------------------

#[tokio::test]
async fn continue_loops_within_cap_then_verifies() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-continue");
    let (invoke, calls) = scripted_invoker(vec![
        iteration(valid(Intent::Continue), vec![assistant()]),
        iteration(valid(Intent::Continue), vec![assistant()]),
        iteration(
            valid(Intent::Verify),
            vec![assistant(), result_msg_with(3, None)],
        ),
    ]);
    let config = HarnessConfig {
        max_iterations_per_attempt: 3,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(call_count(&calls), 3);
    assert_eq!(outcome.attempts.len(), 1);
    let requests = calls.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.iteration_number)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(requests.iter().all(|r| r.attempt_number == 1));
}

#[tokio::test]
async fn continue_past_cap_fails_validation_without_grading() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-continue-cap");
    let (invoke, _) = scripted_invoker(vec![
        iteration(valid(Intent::Continue), vec![assistant()]),
        iteration(valid(Intent::Continue), vec![assistant()]),
    ]);
    let config = HarnessConfig {
        max_iterations_per_attempt: 2,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::AgentError));
    assert_eq!(attempt.error, "agent did not converge after 2 iterations");
    assert!(
        h.store
            .list_grader_results("run-continue-cap", 1)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn zero_iteration_cap_is_a_protocol_class_agent_error() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-cap-zero");
    let (invoke, calls) = scripted_invoker(vec![]);
    let config = HarnessConfig {
        max_iterations_per_attempt: 0,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(call_count(&calls), 0);
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::AgentError));
    assert_eq!(outcome.attempts[0].error, "no iterations were invoked");
}

// --- Entry normalization --------------------------------------------------

#[tokio::test]
async fn pending_lifecycle_is_brought_to_ready() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-pending");
    assert_eq!(lc.status, Status::Pending);
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    for status in [
        Status::Ready,
        Status::Running,
        Status::Validating,
        Status::Done,
    ] {
        assert!(
            outcome.lifecycle.timestamps.contains_key(&status),
            "{status}"
        );
    }
}

#[tokio::test]
async fn resume_from_interrupted_runs_a_new_attempt() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("ok"))]);
    let mut lc = lifecycle("run-resume");
    let blocked = EnvelopeResult::Valid(ValidEnvelope {
        intent: Intent::Blocked,
        reason: Some("need help".into()),
        requires: vec![BlockedRequirement::CommandGrader { name: "ok".into() }],
    });
    let (first, _) = scripted_invoker(vec![iteration(blocked, vec![assistant()])]);
    h.run(&task, &mut lc, &HarnessConfig::default(), first)
        .await;
    assert_eq!(lc.status, Status::Interrupted);
    assert!(lc.blocked_requires_json.is_some());

    let (second, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), second)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    // The resume drain (INTERRUPTED -> READY) clears the snapshot.
    assert_eq!(outcome.lifecycle.blocked_requires_json, None);
    assert_eq!(outcome.attempts.len(), 2);
}

#[tokio::test]
async fn stale_caller_lifecycle_is_reconciled_from_the_persisted_row() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("ok"))]);
    let mut lc = lifecycle("run-resume-clean");
    let blocked = EnvelopeResult::Valid(ValidEnvelope {
        intent: Intent::Blocked,
        reason: Some("paused".into()),
        requires: vec![BlockedRequirement::CommandGrader { name: "ok".into() }],
    });
    let (first, _) = scripted_invoker(vec![iteration(blocked, vec![assistant()])]);
    h.run(&task, &mut lc, &HarnessConfig::default(), first)
        .await;
    let persisted = h.store.load_lifecycle("run-resume-clean").unwrap().unwrap();
    assert!(persisted.version > 1);

    // A stale in-memory lifecycle for the same run_id (PENDING, version 1).
    let mut stale = lifecycle("run-resume-clean");
    let (second, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut stale, &HarnessConfig::default(), second)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert!(
        h.sink
            .of_kind("run-resume-clean", "harness.crash")
            .is_empty()
    );
}

// --- Rate limits ----------------------------------------------------------

/// A rate-limit event as the agents bridge carries it: a serialized
/// `flywheel_agents::RateLimitInfo`.
fn rate_limit_event(status: &str, resets_at: i64) -> Value {
    #[allow(clippy::cast_precision_loss)]
    let resets_at_epoch = Some(resets_at as f64);
    serde_json::to_value(flywheel_agents::RateLimitInfo {
        resets_at_epoch,
        status: Some(status.to_owned()),
        limit_type: Some("five_hour".to_owned()),
        detail: serde_json::Map::new(),
    })
    .unwrap()
}

fn rejected_event(resets_at: i64) -> Value {
    rate_limit_event("rejected", resets_at)
}

fn warning_event() -> Value {
    rate_limit_event("allowed_warning", 1_700_000_000)
}

fn rate_limited_iteration(api_error_status: Option<u16>, rejected: bool) -> IterationResult {
    let mut result = iteration(EnvelopeResult::Missing, vec![]);
    result.signals = InvocationSignals {
        api_error_status,
        rate_limit_events: if rejected {
            vec![rejected_event(1_700_000_000)]
        } else {
            vec![]
        },
        ..signals()
    };
    result
}

fn no_sleep() -> crate::harness::SleepFn {
    Arc::new(|_| Box::pin(async {}))
}

fn recording_sleep() -> (crate::harness::SleepFn, Arc<Mutex<Vec<f64>>>) {
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded = delays.clone();
    let sleep: crate::harness::SleepFn = Arc::new(move |delay| {
        recorded.lock().unwrap().push(delay);
        Box::pin(async {})
    });
    (sleep, delays)
}

#[tokio::test]
async fn rate_limit_event_surfaced_without_classification() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-rate");
    let mut result = verify_iteration();
    result.signals.rate_limit_events = vec![warning_event()];
    let (invoke, _) = scripted_invoker(vec![result]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let completed = h.sink.of_kind("run-rate", "harness.iteration_completed");
    assert_eq!(completed[0].payload["rate_limited"], true);
}

#[tokio::test]
async fn transient_rate_limit_retries_on_the_separate_budget() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("ok"))]);
    let mut lc = lifecycle("run-rl-retry");
    let (invoke, calls) = scripted_invoker(vec![
        rate_limited_iteration(Some(429), false),
        rate_limited_iteration(None, true),
        verify_iteration(),
    ]);
    let (sleep, delays) = recording_sleep();
    let config = HarnessConfig {
        max_retries: 0,
        max_transient_retries: 3,
        transient_sleep: Some(sleep),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(call_count(&calls), 3);
    let delays = delays.lock().unwrap().clone();
    assert_eq!(delays.len(), 2);
    assert!(delays.windows(2).all(|w| w[1] >= w[0]));
    assert!(
        delays
            .iter()
            .all(|d| *d <= config.transient_backoff.cap_seconds())
    );
    let kinds = h.sink.kinds("run-rl-retry");
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "harness.transient_retry")
            .count(),
        2
    );
    assert!(!kinds.contains(&"harness.retry_scheduled".to_string()));
    let retries = h.sink.of_kind("run-rl-retry", "harness.transient_retry");
    assert_eq!(retries[0].payload["reason"], "api_error_status=429");
    assert_eq!(
        retries[1].payload["reason"],
        "rate_limit_rejected:five_hour"
    );
}

#[tokio::test]
async fn transient_rate_limit_exhausted_is_bounded_and_transient_classified() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("ok"))]);
    let mut lc = lifecycle("run-rl-exhausted");
    let budget = 2;
    let (invoke, calls) = scripted_invoker(
        (0..=budget)
            .map(|_| rate_limited_iteration(Some(429), false))
            .collect(),
    );
    let config = HarnessConfig {
        max_retries: 0,
        max_transient_retries: budget,
        transient_sleep: Some(no_sleep()),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(call_count(&calls), (budget + 1) as usize);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    assert_eq!(
        outcome.attempts[0].error,
        "transient rate-limit retries exhausted after 3 invocations"
    );
    let exhausted = h
        .sink
        .of_kind("run-rl-exhausted", "harness.transient_exhausted");
    assert_eq!(exhausted.len(), 1);
    assert_eq!(exhausted[0].payload["classification"], "transient");
    assert_eq!(exhausted[0].payload["invocations"], budget + 1);
}

// --- Session-limit fast-abort ---------------------------------------------

fn session_now() -> chrono::DateTime<Utc> {
    utc(2026, 7, 9, 12, 0)
}

fn future_reset() -> chrono::DateTime<Utc> {
    utc(2026, 7, 9, 18, 0)
}

fn passing_task() -> Task {
    task(vec![command("true", Some("ok"))])
}

#[tokio::test]
async fn session_limit_rejected_event_future_reset_fast_aborts() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-session-reject");
    let mut result = iteration(EnvelopeResult::Missing, vec![]);
    result.transcript = "rate limited".into();
    result.signals = InvocationSignals {
        api_error_status: None,
        rate_limit_events: vec![rejected_event(future_reset().timestamp())],
        ..signals()
    };
    let (invoke, calls) = scripted_invoker(vec![result]);
    let (sleep, delays) = recording_sleep();
    let config = HarnessConfig {
        max_transient_retries: 3,
        transient_sleep: Some(sleep),
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &config,
            invoke,
            h.options().with_now(fixed_clock(session_now())),
        )
        .await
        .unwrap();
    assert_eq!(call_count(&calls), 1);
    assert!(delays.lock().unwrap().is_empty());
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.session_limit_reset, Some(future_reset()));
    let kinds = h.sink.kinds("run-session-reject");
    assert!(!kinds.contains(&"harness.transient_retry".to_string()));
    assert!(!kinds.contains(&"harness.transient_exhausted".to_string()));
    let session = h
        .sink
        .of_kind("run-session-reject", "harness.session_limit");
    assert_eq!(session.len(), 1);
    assert_eq!(session[0].payload["reset_at"], "2026-07-09T18:00:00+00:00");
    assert_eq!(session[0].payload["source"], "rate_limit_event");
    assert_eq!(session[0].payload["classification"], "transient");
    assert_eq!(
        session[0].payload["reason"],
        "rate_limit_rejected:five_hour"
    );
}

/// One Claude Code session-limit iteration, driven through the real
/// claude-code stream normalizer, the agents fold, and the bridge fold
/// (`completed_run_to_iteration_result`) -- the exact path a live
/// invocation takes. The native lines are the ones the CLI emitted in a
/// recorded run (`run-928e21003adf49fe9be85ff2fbf88e3e`, 2026-07-09): a
/// `rejected` five-hour `rate_limit_event` resetting at 1783620000
/// (2026-07-09T18:00:00Z), the synthetic refusal text, and the error result.
/// `api_error_status` is omitted from the result line when `with_429` is
/// false, leaving the rate-limit event as the only transient signal.
fn recorded_claude_session_limit_iteration(with_429: bool, refusal: &str) -> IterationResult {
    use flywheel_agents::claude_code::ClaudeStreamNormalizer;
    use flywheel_agents::{AgentExit, EventFolder};
    let session = "31a4b1ab-1689-4b19-9aec-6020b5820b76";
    let mut result_line = json!({
        "type": "result", "subtype": "success", "duration_ms": 834, "duration_api_ms": 0,
        "is_error": true, "num_turns": 1, "session_id": session, "stop_reason": "stop_sequence",
        "total_cost_usd": 0, "result": refusal, "permission_denials": [],
        "uuid": "6f5ce867-141b-4ba9-97f3-ab875ea9139f",
    });
    if with_429 {
        result_line["api_error_status"] = json!(429);
    }
    let lines = [
        json!({"type": "system", "subtype": "init", "session_id": session}),
        json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected", "resetsAt": 1_783_620_000i64, "rateLimitType": "five_hour",
                "overageStatus": "rejected", "overageDisabledReason": "org_level_disabled_until",
                "isUsingOverage": false,
            },
            "uuid": "e9a4f1fb-2a40-410c-a878-64562f409996", "session_id": session,
        }),
        json!({
            "type": "assistant", "error": "rate_limit", "session_id": session,
            "message": {"model": "<synthetic>", "content": [{"type": "text", "text": refusal}]},
        }),
        result_line,
    ];
    let mut normalizer = ClaudeStreamNormalizer::new();
    let mut folder = EventFolder::new();
    let mut messages = Vec::new();
    for line in &lines {
        for event in normalizer.feed_line(&line.to_string()).events {
            folder.feed(&event);
            messages.push(crate::agents_invoke::event_to_message(&event));
        }
    }
    let completed = folder.completed(
        AgentExit {
            returncode: Some(1),
            stderr_tail: None,
            duration_seconds: Some(0.8),
        },
        None,
        None,
    );
    crate::agents_invoke::completed_run_to_iteration_result(&completed, messages)
}

#[tokio::test]
async fn session_limit_fast_abort_uses_a_recorded_claude_rejected_event() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-session-recorded");
    let result = recorded_claude_session_limit_iteration(
        true,
        "You've hit your session limit \u{b7} resets 2pm (America/New_York)",
    );
    let (invoke, calls) = scripted_invoker(vec![result]);
    let config = HarnessConfig {
        max_transient_retries: 3,
        transient_sleep: Some(no_sleep()),
        ..HarnessConfig::default()
    };
    // The recorded refusal arrived at 16:42:29Z; 2pm America/New_York that
    // day is 18:00Z, exactly the event's resetsAt.
    let now = utc(2026, 7, 9, 16, 42);
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &config,
            invoke,
            h.options().with_now(fixed_clock(now)),
        )
        .await
        .unwrap();
    assert_eq!(call_count(&calls), 1);
    assert_eq!(outcome.session_limit_reset, Some(utc(2026, 7, 9, 18, 0)));
    let session = h
        .sink
        .of_kind("run-session-recorded", "harness.session_limit");
    assert_eq!(session.len(), 1);
    assert_eq!(session[0].payload["source"], "rate_limit_event");
    assert_eq!(session[0].payload["reset_at"], "2026-07-09T18:00:00+00:00");
}

#[tokio::test]
async fn recorded_claude_rejected_event_alone_drives_the_transient_retry() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-rl-recorded");
    // No api_error_status and no parseable refusal text: the rejected
    // rate-limit event is the only transient signal. The clock is past its
    // reset, so the session-limit fast-abort stands down and the transient
    // retry budget applies.
    let (invoke, calls) = scripted_invoker(vec![
        recorded_claude_session_limit_iteration(false, "rate limited"),
        verify_iteration(),
    ]);
    let (sleep, delays) = recording_sleep();
    let config = HarnessConfig {
        max_retries: 0,
        max_transient_retries: 3,
        transient_sleep: Some(sleep),
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &config,
            invoke,
            h.options().with_now(fixed_clock(utc(2026, 7, 9, 19, 0))),
        )
        .await
        .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(call_count(&calls), 2);
    assert_eq!(delays.lock().unwrap().len(), 1);
    let retries = h.sink.of_kind("run-rl-recorded", "harness.transient_retry");
    assert_eq!(retries.len(), 1);
    assert_eq!(
        retries[0].payload["reason"],
        "rate_limit_rejected:five_hour"
    );
}

#[tokio::test]
async fn session_limit_pipe_epoch_transcript_fast_aborts() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-session-pipe");
    let transcript = format!(
        "Claude AI usage limit reached|{}",
        future_reset().timestamp()
    );
    let mut result = iteration(EnvelopeResult::Missing, vec![]);
    result.transcript = transcript.clone();
    result.signals = InvocationSignals {
        api_error_status: Some(429),
        ..signals()
    };
    let (invoke, calls) = scripted_invoker(vec![result]);
    let config = HarnessConfig {
        max_transient_retries: 3,
        transient_sleep: Some(no_sleep()),
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &config,
            invoke,
            h.options().with_now(fixed_clock(session_now())),
        )
        .await
        .unwrap();
    assert_eq!(call_count(&calls), 1);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    assert_eq!(outcome.session_limit_reset, Some(future_reset()));
    assert_eq!(outcome.lifecycle.agent_output, transcript);
    assert_eq!(
        outcome.attempts[0].error,
        "session-limit refusal; reset at 2026-07-09T18:00:00+00:00"
    );
    let session = h.sink.of_kind("run-session-pipe", "harness.session_limit");
    assert_eq!(session[0].payload["source"], "refusal_pipe_epoch");
}

#[tokio::test]
async fn session_limit_human_clock_time_fast_aborts() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-session-clock");
    let mut result = iteration(EnvelopeResult::Missing, vec![]);
    result.transcript = "Claude AI usage limit reached, resets 6pm".into();
    result.signals = InvocationSignals {
        api_error_status: Some(429),
        ..signals()
    };
    let (invoke, calls) = scripted_invoker(vec![result]);
    let config = HarnessConfig {
        max_transient_retries: 3,
        transient_sleep: Some(no_sleep()),
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &config,
            invoke,
            h.options().with_now(fixed_clock(utc(2026, 7, 9, 17, 0))),
        )
        .await
        .unwrap();
    assert_eq!(call_count(&calls), 1);
    assert_eq!(outcome.session_limit_reset, Some(utc(2026, 7, 9, 18, 0)));
    let session = h.sink.of_kind("run-session-clock", "harness.session_limit");
    assert_eq!(session[0].payload["source"], "refusal_clock_time");
}

#[tokio::test]
async fn session_limit_past_reset_falls_through_to_transient_retry() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-session-past");
    let mut past = iteration(EnvelopeResult::Missing, vec![]);
    past.signals = InvocationSignals {
        rate_limit_events: vec![rejected_event(1_700_000_000)],
        ..signals()
    };
    let (invoke, calls) = scripted_invoker(vec![past, verify_iteration()]);
    let (sleep, delays) = recording_sleep();
    let config = HarnessConfig {
        max_transient_retries: 3,
        transient_sleep: Some(sleep),
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &config,
            invoke,
            h.options().with_now(fixed_clock(session_now())),
        )
        .await
        .unwrap();
    assert_eq!(call_count(&calls), 2);
    assert_eq!(delays.lock().unwrap().len(), 1);
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.session_limit_reset, None);
    let kinds = h.sink.kinds("run-session-past");
    assert!(!kinds.contains(&"harness.session_limit".to_string()));
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "harness.transient_retry")
            .count(),
        1
    );
}

#[tokio::test]
async fn session_limit_valid_envelope_with_future_reset_not_aborted() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-session-valid");
    let mut result = verify_iteration();
    result.signals.rate_limit_events = vec![rejected_event(future_reset().timestamp())];
    let (invoke, calls) = scripted_invoker(vec![result]);
    let config = HarnessConfig {
        max_transient_retries: 3,
        transient_sleep: Some(no_sleep()),
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &config,
            invoke,
            h.options().with_now(fixed_clock(session_now())),
        )
        .await
        .unwrap();
    assert_eq!(call_count(&calls), 1);
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.session_limit_reset, None);
    assert!(
        !h.sink
            .kinds("run-session-valid")
            .contains(&"harness.session_limit".to_string())
    );
}

// --- Iteration-completed telemetry ---------------------------------------

fn completed_payloads(h: &Harness, run_id: &str) -> Vec<serde_json::Map<String, Value>> {
    h.sink
        .of_kind(run_id, "harness.iteration_completed")
        .into_iter()
        .map(|r| r.payload)
        .collect()
}

#[tokio::test]
async fn usage_breakdown_attached_to_payload() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-usage-fr1");
    let usage = json!({
        "input_tokens": 12,
        "output_tokens": 7,
        "cache_creation_input_tokens": 3,
        "cache_read_input_tokens": 100,
    });
    let (invoke, _) = scripted_invoker(vec![iteration(
        valid(Intent::Verify),
        vec![assistant_usage(usage), result_msg()],
    )]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let payloads = completed_payloads(&h, "run-usage-fr1");
    assert_eq!(payloads.len(), 1);
    assert_eq!(
        payloads[0]["usage"],
        json!({
            "input_tokens": 12,
            "output_tokens": 7,
            "cache_creation_input_tokens": 3,
            "cache_read_input_tokens": 100,
            "total_tokens": 122,
        })
    );
    // Python key order on the usage object.
    let keys: Vec<_> = payloads[0]["usage"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
            "total_tokens"
        ]
    );
}

#[tokio::test]
async fn cost_and_turns_surfaced_from_signals_or_null() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-usage-fr2");
    let mut with = verify_iteration();
    with.signals.num_turns = Some(4);
    with.signals.total_cost_usd = Some(0.42);
    let (invoke, _) = scripted_invoker(vec![with]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let payload = &completed_payloads(&h, "run-usage-fr2")[0];
    assert_eq!(payload["num_turns"], 4);
    assert_eq!(payload["total_cost_usd"], 0.42);

    let mut lc2 = lifecycle("run-usage-fr2-null");
    let mut without = verify_iteration();
    without.signals.num_turns = None;
    without.signals.total_cost_usd = None;
    let (invoke, _) = scripted_invoker(vec![without]);
    h.run(&task, &mut lc2, &HarnessConfig::default(), invoke)
        .await;
    let payload = &completed_payloads(&h, "run-usage-fr2-null")[0];
    assert_eq!(payload["num_turns"], Value::Null);
    assert_eq!(payload["total_cost_usd"], Value::Null);
    assert_eq!(payload["usage"]["total_tokens"], 0);
}

#[tokio::test]
async fn total_tokens_matches_observation() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-usage-fr3");
    let messages = vec![
        assistant_usage(json!({"input_tokens": 30, "output_tokens": 20})),
        assistant_usage(json!({"input_tokens": 11, "output_tokens": 4})),
        result_msg_with(2, None),
    ];
    let (invoke, _) = scripted_invoker(vec![iteration(valid(Intent::Verify), messages.clone())]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let payload = &completed_payloads(&h, "run-usage-fr3")[0];
    let observation = build_observation(&messages, 0.0);
    assert_eq!(payload["usage"]["total_tokens"], observation.total_tokens);
    assert_eq!(observation.total_tokens, 65);
    assert_eq!(observation.turns, 2);
}

#[tokio::test]
async fn two_iterations_each_carry_own_usage_not_running_sum() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-usage-fr4");
    let (invoke, _) = scripted_invoker(vec![
        iteration(
            valid(Intent::Continue),
            vec![assistant_usage(
                json!({"input_tokens": 10, "output_tokens": 5}),
            )],
        ),
        iteration(
            valid(Intent::Verify),
            vec![
                assistant_usage(json!({"input_tokens": 7, "output_tokens": 3})),
                result_msg_with(2, None),
            ],
        ),
    ]);
    let config = HarnessConfig {
        max_iterations_per_attempt: 2,
        ..HarnessConfig::default()
    };
    h.run(&task, &mut lc, &config, invoke).await;
    let payloads = completed_payloads(&h, "run-usage-fr4");
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0]["usage"]["total_tokens"], 15);
    assert_eq!(payloads[1]["usage"]["input_tokens"], 7);
    assert_eq!(payloads[1]["usage"]["total_tokens"], 10);
}

#[tokio::test]
async fn failed_iteration_still_carries_breakdown() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-usage-fail");
    let mut result = iteration(
        EnvelopeResult::Missing,
        vec![assistant_usage(
            json!({"input_tokens": 9, "output_tokens": 4}),
        )],
    );
    result.failure = Some(failure("ProcessError", "agent crashed"));
    let (invoke, _) = scripted_invoker(vec![result]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let payload = &completed_payloads(&h, "run-usage-fail")[0];
    assert_eq!(payload["failure"]["error_type"], "ProcessError");
    assert_eq!(payload["usage"]["total_tokens"], 13);
}

#[tokio::test]
async fn explicit_usage_map_wins_over_messages() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-usage-explicit");
    let mut result = iteration(
        valid(Intent::Verify),
        vec![assistant_usage(json!({"input_tokens": 9}))],
    );
    result.usage = Some(crate::invoker::Usage::from([
        ("input_tokens".to_string(), 100),
        ("output_tokens".to_string(), 1),
    ]));
    let (invoke, _) = scripted_invoker(vec![result]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let payload = &completed_payloads(&h, "run-usage-explicit")[0];
    assert_eq!(payload["usage"]["input_tokens"], 100);
    assert_eq!(payload["usage"]["total_tokens"], 101);
}

#[test]
fn build_usage_breakdown_total_matches_build_observation() {
    let messages = vec![
        assistant_usage(json!({"input_tokens": 12, "output_tokens": 8})),
        assistant_usage(json!({"input_tokens": 5, "cache_read_input_tokens": 100})),
        result_msg_with(2, None),
    ];
    let breakdown = build_usage_breakdown(&messages);
    let observation = build_observation(&messages, 0.0);
    assert_eq!(breakdown.values().sum::<u64>(), observation.total_tokens);
}

#[test]
fn result_message_breakdown_wins_when_larger() {
    let messages = vec![
        assistant_usage(json!({"input_tokens": 5, "output_tokens": 5})),
        result_msg_with(
            1,
            Some(json!({"input_tokens": 100, "output_tokens": 50, "cache_read_input_tokens": 10})),
        ),
    ];
    let breakdown = build_usage_breakdown(&messages);
    assert_eq!(breakdown["input_tokens"], 100);
    assert_eq!(breakdown["output_tokens"], 50);
    assert_eq!(breakdown["cache_creation_input_tokens"], 0);
    assert_eq!(breakdown["cache_read_input_tokens"], 10);
}

#[test]
fn normalized_agent_events_feed_the_breakdown() {
    // The claude-code normalizer reports usage on context.usage events.
    let messages = vec![
        json!({"type": "message.assistant", "payload": {"text": "hi"}}),
        json!({"type": "context.usage", "payload": {"usage": {"input_tokens": 7, "output_tokens": 2}}}),
    ];
    let breakdown = build_usage_breakdown(&messages);
    assert_eq!(breakdown["input_tokens"], 7);
    assert_eq!(build_observation(&messages, 0.0).total_tokens, 9);
}

// --- Attempt aggregate rollup ---------------------------------------------

#[tokio::test]
async fn attempt_row_carries_cumulative_aggregates_after_run() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-agg-roll");
    let mut first = iteration(
        valid(Intent::Continue),
        vec![
            assistant_usage(
                json!({"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 2}),
            ),
            result_msg_with(3, None),
        ],
    );
    first.signals.num_turns = Some(3);
    first.signals.total_cost_usd = Some(0.10);
    let mut second = iteration(
        valid(Intent::Verify),
        vec![
            assistant_usage(
                json!({"input_tokens": 7, "output_tokens": 3, "cache_read_input_tokens": 50}),
            ),
            result_msg_with(2, None),
        ],
    );
    second.signals.num_turns = Some(2);
    second.signals.total_cost_usd = Some(0.05);
    let (invoke, _) = scripted_invoker(vec![first, second]);
    let config = HarnessConfig {
        max_iterations_per_attempt: 2,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let attempt = h.store.load_attempt("run-agg-roll", 1).unwrap().unwrap();
    assert_eq!(attempt.input_tokens, 17);
    assert_eq!(attempt.output_tokens, 8);
    assert_eq!(attempt.cache_creation_input_tokens, 2);
    assert_eq!(attempt.cache_read_input_tokens, 50);
    assert_eq!(attempt.total_tokens(), 77);
    assert_eq!(attempt.iterations_completed, 2);
    assert_eq!(attempt.turns, 5);
    assert!((attempt.total_cost_usd - 0.15).abs() < 1e-9);
    assert!(attempt.last_activity_at.is_some());
    // Finalization preserved the rollups alongside the outcome.
    assert_eq!(attempt.outcome, Some(Outcome::Succeeded));
    assert_eq!(outcome.attempts[0].total_tokens(), 77);
}

#[tokio::test]
async fn none_turns_and_cost_roll_up_as_zero() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-agg-none");
    let mut result = verify_iteration();
    result.signals.num_turns = None;
    result.signals.total_cost_usd = None;
    let (invoke, _) = scripted_invoker(vec![result]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let attempt = h.store.load_attempt("run-agg-none", 1).unwrap().unwrap();
    assert_eq!(attempt.total_tokens(), 0);
    assert_eq!(attempt.iterations_completed, 1);
    assert_eq!(attempt.turns, 0);
    assert_eq!(attempt.total_cost_usd, 0.0);
    assert!(attempt.last_activity_at.is_some());
}

// --- Operator interruption: signal-killed command graders ----------------

fn signal_killed_task(signal: &str) -> Task {
    task(vec![command(
        &format!("kill -{signal} $$"),
        Some("signaled"),
    )])
}

#[tokio::test]
async fn sigint_killed_command_grader_routes_to_interrupted() {
    let h = Harness::new();
    let task = signal_killed_task("INT");
    let mut lc = lifecycle("run-sigint-grader");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let config = HarnessConfig {
        max_retries: 1,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.lifecycle.retries, 0);
    assert_eq!(outcome.attempts.len(), 1);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    assert!(attempt.error.contains("operator interrupted"));
    assert!(attempt.error.contains("signal 2"));
    let crash = h.sink.of_kind("run-sigint-grader", "harness.crash");
    assert_eq!(crash.len(), 1);
    assert_eq!(crash[0].payload["classification"], "grader_signaled");
    assert_eq!(crash[0].payload["signal"], 2);
}

#[tokio::test]
async fn sigterm_killed_command_grader_routes_to_interrupted() {
    let h = Harness::new();
    let task = signal_killed_task("TERM");
    let mut lc = lifecycle("run-sigterm-grader");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    let crash = h.sink.of_kind("run-sigterm-grader", "harness.crash");
    assert_eq!(crash[0].payload["signal"], 15);
}

#[tokio::test]
async fn non_operator_signal_still_routes_to_failed_validation() {
    let h = Harness::new();
    let task = signal_killed_task("SEGV");
    let mut lc = lifecycle("run-sigsegv-grader");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::ValidationFailed));
}

// --- Operator shutdown mid-iteration -------------------------------------

/// An invoker that pumps `pre_messages`, triggers the shutdown signal, then
/// stalls: the harness must observe the signal at its cancellation point.
fn cancelling_invoker(
    shutdown: ShutdownSignal,
    pre_messages: Vec<Value>,
) -> crate::harness::InvokeFn {
    Arc::new(move |request| {
        let shutdown = shutdown.clone();
        let pre_messages = pre_messages.clone();
        Box::pin(async move {
            if let Some(on_message) = &request.observers.on_message {
                for msg in &pre_messages {
                    on_message(msg);
                }
            }
            shutdown.trigger();
            std::future::pending::<IterationResult>().await
        })
    })
}

async fn run_interrupted(
    h: &Harness,
    task: &Task,
    lc: &mut Lifecycle,
    pre_messages: Vec<Value>,
) -> HarnessError {
    let shutdown = ShutdownSignal::new();
    let invoke = cancelling_invoker(shutdown.clone(), pre_messages);
    h.run_with(
        task,
        lc,
        &HarnessConfig::default(),
        invoke,
        h.options().with_shutdown(shutdown),
    )
    .await
    .expect_err("shutdown surfaces as an error")
}

#[tokio::test]
async fn cancelled_mid_stream_finalizes_interrupted() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("ok"))]);
    let mut lc = lifecycle("run-interrupt-mid");
    let err = run_interrupted(&h, &task, &mut lc, vec![assistant()]).await;
    assert!(matches!(err, HarnessError::Interrupted));
    let reloaded = h
        .store
        .load_lifecycle("run-interrupt-mid")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, Status::Interrupted);
    assert_eq!(reloaded.retries, 0);
    assert_eq!(lc.status, Status::Interrupted);
    // The attempt is closed INTERNAL_ERROR, never stranded.
    let attempts = h.store.list_attempts("run-interrupt-mid").unwrap();
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].ended_at.is_some());
    assert_eq!(attempts[0].outcome, Some(Outcome::InternalError));
    assert!(attempts[0].error.contains("operator interrupted"));
    // Pre-interrupt messages were persisted immediately.
    let rows = h.sink.messages("run-interrupt-mid");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "AssistantMessage");
    // The observability event records the exogenous stop.
    let interrupted = h.sink.of_kind("run-interrupt-mid", "harness.interrupted");
    assert_eq!(interrupted.len(), 1);
    assert_eq!(
        interrupted[0].payload["classification"],
        "worker_interrupted"
    );
    assert_eq!(interrupted[0].payload["from_status"], "running");
    assert_eq!(interrupted[0].attempt_number, Some(1));
    let kinds = h.sink.kinds("run-interrupt-mid");
    assert!(kinds.contains(&"harness.attempt_started".to_string()));
    assert!(kinds.contains(&"harness.attempt_finalized".to_string()));
    assert!(!kinds.contains(&"harness.crash".to_string()));
    // The in-band finalizer left nothing for the stranded backstop.
    let swept = finalize_stranded_lifecycle(
        h.store.clone(),
        "run-interrupt-mid",
        FinalizeStrandedOptions::default(),
    )
    .unwrap();
    assert!(!swept);
}

#[tokio::test]
async fn interrupted_lifecycle_is_resumable() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-interrupt-resume");
    run_interrupted(&h, &task, &mut lc, vec![assistant()]).await;
    assert_eq!(h.sink.messages("run-interrupt-resume").len(), 1);

    // Resume on the same run_id with a stale in-memory copy.
    let mut resume = lifecycle("run-interrupt-resume");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut resume, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let attempts = h.store.list_attempts("run-interrupt-resume").unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].outcome, Some(Outcome::InternalError));
    assert_eq!(attempts[1].outcome, Some(Outcome::Succeeded));
    assert!(h.sink.messages("run-interrupt-resume").len() >= 2);
}

#[tokio::test]
async fn operator_interrupt_command_routes_through_the_interrupt_path() {
    // The store-routed `interrupt` surfaces as FAILURE_OPERATOR_INTERRUPT on
    // the iteration result: the twin of SIGINT/SIGTERM.
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-interrupt-command");
    let mut result = iteration(EnvelopeResult::Missing, vec![assistant()]);
    result.failure = Some(failure(
        FAILURE_OPERATOR_INTERRUPT,
        "run interrupted by operator",
    ));
    let (invoke, _) = scripted_invoker(vec![result]);
    let err = h
        .run_with(
            &task,
            &mut lc,
            &HarnessConfig::default(),
            invoke,
            h.options(),
        )
        .await
        .expect_err("interrupted");
    assert!(matches!(err, HarnessError::Interrupted));
    assert_eq!(lc.status, Status::Interrupted);
    assert_eq!(
        h.sink
            .of_kind("run-interrupt-command", "harness.interrupted")
            .len(),
        1
    );
}

#[tokio::test]
async fn handle_interrupt_is_idempotent() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-interrupt-idemp");
    run_interrupted(&h, &task, &mut lc, vec![]).await;
    let first_events = h.sink.events("run-interrupt-idemp").len();
    let first_version = h
        .store
        .load_lifecycle("run-interrupt-idemp")
        .unwrap()
        .unwrap()
        .version;

    let telemetry = Arc::new(RunTelemetry::new(
        Some(h.sink.clone()),
        "run-interrupt-idemp",
        Arc::new(Utc::now),
    ));
    let store: Arc<dyn HarnessStore> = h.store.clone();
    let reloaded = h
        .store
        .load_lifecycle("run-interrupt-idemp")
        .unwrap()
        .unwrap();
    let run = Run::new(
        Arc::new(MirroringStore::new(store, telemetry.clone())),
        telemetry,
        reloaded,
        Arc::new(Utc::now),
        Arc::new(|| 0.0),
        None,
    );
    run.handle_interrupt(None);
    assert_eq!(h.sink.events("run-interrupt-idemp").len(), first_events);
    assert_eq!(
        h.store
            .load_lifecycle("run-interrupt-idemp")
            .unwrap()
            .unwrap()
            .version,
        first_version
    );
    assert_eq!(
        h.sink
            .of_kind("run-interrupt-idemp", "harness.interrupted")
            .len(),
        1
    );
}

#[tokio::test]
async fn interrupt_transition_failure_is_observable() {
    let sink = ListSink::new();
    let failing = FailingStore::on_event(DomainEventKind::TransitionedTo);
    let now = Utc::now();
    let mut lc = lifecycle("run-interrupt-transition-fail");
    lc.transition_to(Status::Ready, "", Some(now)).unwrap();
    lc.transition_to(Status::Running, "", Some(now)).unwrap();
    let telemetry = Arc::new(RunTelemetry::new(
        Some(sink.clone()),
        "run-interrupt-transition-fail",
        fixed_clock(now),
    ));
    let store: Arc<dyn HarnessStore> = failing;
    let run = Run::new(
        Arc::new(MirroringStore::new(store, telemetry.clone())),
        telemetry,
        lc,
        fixed_clock(now),
        Arc::new(|| 0.0),
        None,
    );
    run.handle_interrupt(None);
    let crash = sink.of_kind("run-interrupt-transition-fail", "harness.crash");
    assert_eq!(crash.len(), 1);
    assert_eq!(
        crash[0].payload["classification"],
        "interrupt_transition_failed"
    );
    assert_eq!(crash[0].payload["exception_type"], "StoreBackendError");
    assert!(
        crash[0].payload["message"]
            .as_str()
            .unwrap()
            .contains("simulated ledger append failure")
    );
    assert_eq!(run.status(), Status::Running);
}

#[tokio::test]
async fn interrupt_during_validating_finalizes_interrupted() {
    // The rubric judge's await is the cancellation point inside validating.
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let shutdown = ShutdownSignal::new();
    let trigger = shutdown.clone();
    let judge: Arc<dyn crate::grader_rubric::JudgeInvoke> = Arc::new(
        move |_prompt: String, _grader: crate::task::RubricGrader, _wt: std::path::PathBuf| {
            let trigger = trigger.clone();
            async move {
                trigger.trigger();
                std::future::pending::<
                    Result<
                        crate::grader_rubric::JudgeResponse,
                        crate::grader_rubric::JudgeInvokeError,
                    >,
                >()
                .await
            }
        },
    );
    let task = task(vec![
        ok_command(),
        Grader::Rubric(crate::task::RubricGrader::new(vec!["a".into()]).unwrap()),
    ]);
    let mut lc = lifecycle("run-interrupt-validating");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let config = HarnessConfig {
        rubric_judge_invoke: Some(judge),
        ..worktree_config(&dir)
    };
    let err = h
        .run_with(
            &task,
            &mut lc,
            &config,
            invoke,
            h.options().with_shutdown(shutdown),
        )
        .await
        .expect_err("interrupted");
    assert!(matches!(err, HarnessError::Interrupted));
    let reloaded = h
        .store
        .load_lifecycle("run-interrupt-validating")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, Status::Interrupted);
    let attempt = &h.store.list_attempts("run-interrupt-validating").unwrap()[0];
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    let interrupted = h
        .sink
        .of_kind("run-interrupt-validating", "harness.interrupted");
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].payload["from_status"], "validating");
}

// --- Stranded finalization --------------------------------------------------

fn seed_open(store: &InMemoryStore, run_id: &str, status: Status) -> Lifecycle {
    let now = Utc::now();
    let mut lc = lifecycle(run_id);
    lc.transition_to(Status::Ready, "", Some(now)).unwrap();
    lc.transition_to(Status::Running, "", Some(now)).unwrap();
    if status == Status::Validating {
        lc.transition_to(Status::Validating, "", Some(now)).unwrap();
    }
    store.create_lifecycle(&lc).unwrap();
    store
        .save_attempt(run_id, &Attempt::new(1, now, run_id), None)
        .unwrap();
    lc
}

#[test]
fn finalize_running_transitions_to_interrupted() {
    let h = Harness::new();
    let lc = seed_open(&h.store, "run-strand-r", Status::Running);
    let ok = finalize_stranded_lifecycle(
        h.store.clone(),
        &lc.run_id,
        FinalizeStrandedOptions::default().with_sink(h.sink.clone()),
    )
    .unwrap();
    assert!(ok);
    let reloaded = h.store.load_lifecycle("run-strand-r").unwrap().unwrap();
    assert_eq!(reloaded.status, Status::Interrupted);
    let attempts = h.store.list_attempts("run-strand-r").unwrap();
    assert!(attempts[0].ended_at.is_some());
    assert_eq!(attempts[0].outcome, Some(Outcome::InternalError));
    assert_eq!(attempts[0].error, "worker interrupted before finalization");
    let crash = h.sink.of_kind("run-strand-r", "harness.crash");
    assert_eq!(crash.len(), 1);
    assert_eq!(crash[0].payload["classification"], "worker_interrupted");
    assert_eq!(crash[0].payload["from_status"], "running");
    assert_eq!(crash[0].attempt_number, Some(1));
    // Emission order: attempt_finalized before crash; mirrors follow the rows.
    assert_eq!(
        h.sink.kinds("run-strand-r"),
        ["harness.attempt_finalized", "harness.crash"]
    );
    let mirrors: Vec<_> = h
        .sink
        .domain_mirrors("run-strand-r")
        .into_iter()
        .map(|m| m.kind)
        .collect();
    assert_eq!(
        mirrors,
        ["domain.attempt_finalized", "domain.transitioned_to"]
    );
}

#[test]
fn finalize_validating_transitions_to_interrupted() {
    let h = Harness::new();
    let lc = seed_open(&h.store, "run-strand-v", Status::Validating);
    let ok = finalize_stranded_lifecycle(
        h.store.clone(),
        &lc.run_id,
        FinalizeStrandedOptions::default().with_sink(h.sink.clone()),
    )
    .unwrap();
    assert!(ok);
    assert_eq!(
        h.store
            .load_lifecycle("run-strand-v")
            .unwrap()
            .unwrap()
            .status,
        Status::Interrupted
    );
}

#[test]
fn finalize_done_or_missing_lifecycle_is_noop() {
    let h = Harness::new();
    let now = Utc::now();
    let mut lc = lifecycle("run-strand-done");
    for s in [
        Status::Ready,
        Status::Running,
        Status::Validating,
        Status::Done,
    ] {
        lc.transition_to(s, "", Some(now)).unwrap();
    }
    h.store.create_lifecycle(&lc).unwrap();
    let ok = finalize_stranded_lifecycle(
        h.store.clone(),
        "run-strand-done",
        FinalizeStrandedOptions::default().with_sink(h.sink.clone()),
    )
    .unwrap();
    assert!(!ok);
    assert_eq!(
        h.store
            .load_lifecycle("run-strand-done")
            .unwrap()
            .unwrap()
            .status,
        Status::Done
    );
    assert!(
        !finalize_stranded_lifecycle(
            h.store.clone(),
            "run-does-not-exist",
            FinalizeStrandedOptions::default()
        )
        .unwrap()
    );
}

#[test]
fn finalize_awaiting_approval_lifecycle_is_noop() {
    let h = Harness::new();
    let now = Utc::now();
    let mut lc = lifecycle("run-strand-awaiting");
    for s in [
        Status::Ready,
        Status::Running,
        Status::Validating,
        Status::AwaitingApproval,
    ] {
        lc.transition_to(s, "", Some(now)).unwrap();
    }
    lc.awaiting_manual_ordinal = Some(1);
    h.store.create_lifecycle(&lc).unwrap();
    let mut attempt = Attempt::new(1, now, "run-strand-awaiting");
    attempt.ended_at = Some(now);
    attempt.outcome = Some(Outcome::Succeeded);
    h.store
        .save_attempt("run-strand-awaiting", &attempt, None)
        .unwrap();
    let ok = finalize_stranded_lifecycle(
        h.store.clone(),
        "run-strand-awaiting",
        FinalizeStrandedOptions::default().with_sink(h.sink.clone()),
    )
    .unwrap();
    assert!(!ok);
    let reloaded = h
        .store
        .load_lifecycle("run-strand-awaiting")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, Status::AwaitingApproval);
    assert_eq!(reloaded.awaiting_manual_ordinal, Some(1));
    let attempts = h.store.list_attempts("run-strand-awaiting").unwrap();
    assert_eq!(attempts[0].outcome, Some(Outcome::Succeeded));
    assert_eq!(attempts[0].ended_at, Some(now));
    assert!(
        h.sink
            .of_kind("run-strand-awaiting", "harness.crash")
            .is_empty()
    );
}

// --- Entry-time crash recording ------------------------------------------

#[tokio::test]
async fn store_failure_mid_attempt_writes_crash_event_and_walks_to_failed() {
    let sink = ListSink::new();
    let failing = FailingStore::on_save_attempt();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-entry-crash");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let store: Arc<dyn HarnessStore> = failing.clone();
    let err = run_task(
        &task,
        &mut lc,
        store,
        &HarnessConfig::default(),
        invoke,
        RunOptions::default().with_sink(sink.clone()),
    )
    .await
    .expect_err("the store failure propagates");
    assert!(matches!(err, HarnessError::Store(_)), "{err:?}");
    assert!(err.to_string().contains("simulated rollup write failure"));
    // The lifecycle row exists and walked RUNNING -> FAILED.
    let reloaded = failing
        .inner
        .load_lifecycle("run-entry-crash")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, Status::Failed);
    assert!(reloaded.error.contains("StoreBackendError"));
    assert!(reloaded.error.contains("simulated rollup write failure"));
    assert_eq!(lc.status, Status::Failed);
    // Exactly one harness.crash with the entry_error classification.
    let crashes = sink.of_kind("run-entry-crash", "harness.crash");
    assert_eq!(crashes.len(), 1);
    assert_eq!(crashes[0].payload["classification"], "entry_error");
    assert_eq!(crashes[0].payload["exception_type"], "StoreBackendError");
    assert!(
        sink.kinds("run-entry-crash")
            .contains(&"harness.attempt_started".to_string())
    );
    // The open attempt was finalized, never stranded under a terminal row.
    let attempts = failing.inner.list_attempts("run-entry-crash").unwrap();
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].ended_at.is_some());
    assert_eq!(attempts[0].outcome, Some(Outcome::InternalError));
    assert!(
        attempts[0]
            .error
            .starts_with("attempt crashed: StoreBackendError:")
    );
}

#[tokio::test]
async fn domain_append_failure_stays_fatal() {
    let sink = ListSink::new();
    let failing = FailingStore::on_event(DomainEventKind::AttemptFinalized);
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-ledger-fail");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let store: Arc<dyn HarnessStore> = failing;
    let err = run_task(
        &task,
        &mut lc,
        store,
        &HarnessConfig::default(),
        invoke,
        RunOptions::default().with_sink(sink),
    )
    .await
    .expect_err("ledger failure is fatal");
    assert!(err.to_string().contains("simulated ledger append failure"));
}

#[tokio::test]
async fn invalid_config_is_rejected_at_entry() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-bad-config");
    let (invoke, _) = scripted_invoker(vec![]);
    for ratio in [0.0, -0.1, 1.0001, 2.0] {
        let config = HarnessConfig {
            context_recovery_trigger_ratio: ratio,
            ..HarnessConfig::default()
        };
        let err = h
            .run_with(&task, &mut lc, &config, invoke.clone(), h.options())
            .await
            .expect_err("rejected");
        assert!(err.to_string().contains("trigger_ratio"), "{err}");
    }
    let config = HarnessConfig {
        context_window_tokens: Some(0),
        ..HarnessConfig::default()
    };
    let err = h
        .run_with(&task, &mut lc, &config, invoke, h.options())
        .await
        .expect_err("rejected");
    assert!(err.to_string().contains("context_window_tokens"));
    assert!(
        HarnessConfig {
            context_recovery_trigger_ratio: 1.0,
            ..HarnessConfig::default()
        }
        .validate()
        .is_ok()
    );
    assert!(
        HarnessConfig {
            context_window_tokens: Some(200_000),
            ..HarnessConfig::default()
        }
        .validate()
        .is_ok()
    );
}

// --- SDK message persistence and stream ordering -------------------------

#[tokio::test]
async fn each_iteration_persists_its_message_batch() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-sdk-persist");
    let (invoke, _) = scripted_invoker(vec![
        iteration(
            valid(Intent::Continue),
            vec![assistant_with("iter-1-a", None)],
        ),
        iteration(
            valid(Intent::Verify),
            vec![
                assistant_with("iter-2-a", None),
                assistant_with("iter-2-b", None),
                result_msg_with(2, None),
            ],
        ),
    ]);
    let config = HarnessConfig {
        max_iterations_per_attempt: 2,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let listed = h.sink.messages("run-sdk-persist");
    let kinds: Vec<_> = listed.iter().map(|m| m.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "AssistantMessage",
            "AssistantMessage",
            "AssistantMessage",
            "ResultMessage"
        ]
    );
    assert_eq!(
        listed
            .iter()
            .filter(|m| m.iteration_number == Some(1))
            .count(),
        1
    );
    assert_eq!(
        listed
            .iter()
            .filter(|m| m.iteration_number == Some(2))
            .count(),
        3
    );
    assert!(listed.iter().all(|m| m.attempt_number == Some(1)));
}

#[tokio::test]
async fn zero_message_iteration_still_emits_iteration_completed() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-sdk-empty");
    let (invoke, _) = scripted_invoker(vec![iteration(valid(Intent::Verify), vec![])]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert!(h.sink.messages("run-sdk-empty").is_empty());
    assert_eq!(
        h.sink
            .of_kind("run-sdk-empty", "harness.iteration_completed")
            .len(),
        1
    );
}

#[tokio::test]
async fn run_stream_interleaves_messages_telemetry_and_domain_mirrors() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-interleaved");
    let messages = vec![
        assistant_with("a", None),
        assistant_with("b", None),
        result_msg_with(2, None),
    ];
    let (invoke, _) = scripted_invoker(vec![iteration(valid(Intent::Verify), messages.clone())]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let records = h.sink.all();
    let kinds: Vec<&str> = records.iter().map(|r| r.kind.as_str()).collect();
    let completed_idx = kinds
        .iter()
        .position(|k| *k == "harness.iteration_completed")
        .unwrap();
    let message_indices: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| **k == "AssistantMessage" || **k == "ResultMessage")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(message_indices.len(), messages.len());
    assert!(message_indices.iter().all(|i| *i < completed_idx));
    // Every ledger append is mirrored, store row first; the kinds agree.
    let rows = h.store.list_domain_events("run-interleaved").unwrap();
    let mirrors: Vec<String> = h
        .sink
        .domain_mirrors("run-interleaved")
        .into_iter()
        .map(|m| m.kind)
        .collect();
    let expected: Vec<String> = rows
        .iter()
        .map(|e| format!("domain.{}", e.kind().as_str()))
        .collect();
    assert_eq!(mirrors, expected);
    // The full telemetry sequence of a happy path.
    assert_eq!(
        h.sink.kinds("run-interleaved"),
        [
            "harness.attempt_started",
            "harness.iteration_completed",
            "harness.attempt_finalized",
        ]
    );
    assert_eq!(
        expected,
        [
            "domain.lifecycle_initialized",
            "domain.transitioned_to",
            "domain.transitioned_to",
            "domain.attempt_started",
            "domain.transitioned_to",
            "domain.attempt_finalized",
            "domain.transitioned_to",
        ]
    );
}

#[tokio::test]
async fn harness_domain_log_replays_to_the_stored_lifecycle() {
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-oracle");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let mut loaded = h.store.load_lifecycle("run-oracle").unwrap().unwrap();
    let events = h.store.list_domain_events("run-oracle").unwrap();
    let mut folded = replay(&events).unwrap();
    strip_attempt_aggregates(&mut loaded);
    strip_attempt_aggregates(&mut folded);
    assert_eq!(loaded, folded);
    assert_eq!(loaded.version, events.len() as u64);
    // The returned lifecycle agrees with the persisted row too.
    let mut returned = outcome.lifecycle.clone();
    strip_attempt_aggregates(&mut returned);
    assert_eq!(returned, loaded);
}

// --- Telemetry failure semantics -----------------------------------------

#[tokio::test]
async fn sink_failure_never_aborts_the_run() {
    let store = Arc::new(InMemoryStore::new());
    let sink = RaisingSink::new(None);
    let task = passing_task();
    let mut lc = lifecycle("run-sink-fail");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = run_task(
        &task,
        &mut lc,
        store.clone(),
        &HarnessConfig::default(),
        invoke,
        RunOptions::default().with_sink(sink.clone()),
    )
    .await
    .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Succeeded));
    assert!(sink.attempts() > 1);
    assert_eq!(
        store.list_grader_results("run-sink-fail", 1).unwrap().len(),
        1
    );
}

#[tokio::test]
async fn first_failure_drops_marker_line_when_sink_recovers() {
    let store = Arc::new(InMemoryStore::new());
    let sink = RaisingSink::new(Some(1));
    let task = passing_task();
    let mut lc = lifecycle("run-sink-marker");
    let (invoke, _) = scripted_invoker(vec![iteration(valid(Intent::Verify), vec![assistant()])]);
    let outcome = run_task(
        &task,
        &mut lc,
        store,
        &HarnessConfig::default(),
        invoke,
        RunOptions::default().with_sink(sink.clone()),
    )
    .await
    .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let records = sink.records.lock().unwrap();
    let markers: Vec<_> = records
        .iter()
        .filter(|r| r.kind == "harness.telemetry_sink_failed")
        .collect();
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].payload["error_type"], "StoreBackendError");
}

// --- Deferred subsystems and defaults -----------------------------------------

#[test]
fn deferred_subsystems_are_listed_and_not_silently_implemented() {
    let expected = [
        "thrash net-diff detection (sub-problem b)",
        "thrash input-novelty score (sub-problem c)",
        "hang threshold default value (mechanism shipped, value ungrounded)",
        "fine-grained crash classification",
        "blocked_implicit same-question-re-asked detection",
    ];
    assert_eq!(DEFERRED_LOOP_SUBSYSTEMS, expected);
}

#[test]
fn harness_config_defaults_match_python() {
    let cfg = HarnessConfig::default();
    assert_eq!(cfg.max_retries, 0);
    assert_eq!(cfg.max_cost_usd, 0.0);
    assert_eq!(cfg.max_tokens, 0);
    assert_eq!(cfg.wall_clock_seconds, 0);
    assert_eq!(cfg.max_iterations_per_attempt, 1);
    assert_eq!(cfg.max_transient_retries, 6);
    assert!(cfg.rubric_judge_model.is_none());
    assert_eq!(cfg.rubric_judge_max_turns, 32);
    assert_eq!(cfg.rubric_judge_retries, 2);
    assert!(cfg.worktree.is_none());
    assert!(cfg.rubric_judge_invoke.is_none());
    assert!(cfg.context_window_tokens.is_none());
    assert_eq!(cfg.context_recovery_trigger_ratio, 0.9);
    assert_eq!(cfg.max_context_recoveries, 1);
    assert!(cfg.recovery_summarizer_invoke.is_none());
    assert!(cfg.landability_gate.is_none());
    assert_eq!(cfg.checkpoint_nudge_seconds, 300.0);
    assert!(cfg.checkpoint_progress_probe.is_none());
    assert_eq!(cfg.loop_guard.repeated_tool_failure_threshold, Some(3));
    assert_eq!(cfg.loop_guard.thrash_repeat_threshold, Some(4));
    assert_eq!(cfg.loop_guard.thrash_window, Some(12));
    assert_eq!(cfg.loop_guard.hang_timeout_seconds, None);
    let ceiling = cfg.deadlines.agent_iteration_seconds.unwrap();
    assert!(ceiling > 0.0 && ceiling.is_finite());
}

// --- Task persistence -------------------------------------------------------

#[tokio::test]
async fn run_persists_task_and_pins_content_hash() {
    let h = Harness::new();
    let task = Task::new("g", vec![]).with_id("persist-me");
    let mut lc = Lifecycle::new("persist-me").with_run_id("run-persist");
    let mut result = verify_iteration();
    result.transcript = wrap(r#"{"intent": "verify", "reason": "done"}"#);
    let (invoke, _) = scripted_invoker(vec![result]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let digest = task_digest(&task);
    assert_eq!(outcome.lifecycle.task_content_hash, digest);
    assert_eq!(
        h.store.load_task("persist-me", Some(&digest)).unwrap(),
        Some(task.clone())
    );
    assert_eq!(
        h.store.load_task_for_run("run-persist").unwrap(),
        Some(task)
    );
}

#[tokio::test]
async fn wall_clock_elapsed_uses_the_injected_clock() {
    // The wall-clock budget reads the injected clock, not the OS clock.
    let h = Harness::new();
    let task = passing_task();
    let mut lc = lifecycle("run-clock-inject");
    let base = utc(2026, 1, 1, 0, 0);
    let counter = Arc::new(Mutex::new(0i64));
    let clock: crate::invoker_client::Clock = Arc::new(move || {
        let mut n = counter.lock().unwrap();
        *n += 1;
        base + ChronoDuration::seconds(100 * *n)
    });
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let config = HarnessConfig {
        max_retries: 1,
        wall_clock_seconds: 1,
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(&task, &mut lc, &config, invoke, h.options().with_now(clock))
        .await
        .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert!(
        outcome.attempts[0]
            .error
            .starts_with("budget ceiling breached: wall_clock_seconds")
    );
}

// --- Attempt routing pins (mutation survey, iteration 53) -----------------

#[tokio::test]
async fn abort_with_an_empty_reason_fails_with_the_default_reason() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-abort-empty");
    let (invoke, _) = scripted_invoker(vec![iteration(
        valid_reason(Intent::Abort, ""),
        vec![assistant()],
    )]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.lifecycle.error, "agent reported abort intent");
    assert_eq!(outcome.attempts[0].error, "agent reported abort intent");
}

#[tokio::test]
async fn blocked_with_an_empty_reason_records_the_default_reason() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-blocked-empty");
    let envelope = EnvelopeResult::Valid(ValidEnvelope {
        intent: Intent::Blocked,
        reason: Some(String::new()),
        requires: vec![BlockedRequirement::EnvVarSet {
            name: "MISSING_KEY".into(),
        }],
    });
    let (invoke, _) = scripted_invoker(vec![iteration(envelope, vec![assistant()])]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.attempts[0].error, "agent reported blocked intent");
    let blocked = h.sink.of_kind("run-blocked-empty", "harness.blocked");
    assert_eq!(
        blocked[0].payload["reason"],
        "agent reported blocked intent"
    );
}

#[tokio::test]
async fn invalid_blocked_requires_is_a_retryable_validation_failure() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("other"))]);
    let mut lc = lifecycle("run-bad-blocked-retry");
    let envelope = EnvelopeResult::Valid(ValidEnvelope {
        intent: Intent::Blocked,
        reason: Some("need help".into()),
        requires: vec![BlockedRequirement::CommandGrader {
            name: "not-on-task".into(),
        }],
    });
    let (invoke, _) = scripted_invoker(vec![
        iteration(envelope, vec![assistant()]),
        verify_iteration(),
    ]);
    let config = HarnessConfig {
        max_retries: 1,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.lifecycle.retries, 1);
    let timestamps = &outcome.lifecycle.timestamps;
    assert!(timestamps.contains_key(&Status::FailedValidation));
    assert!(!timestamps.contains_key(&Status::InternalError));
}

#[tokio::test]
async fn a_crash_wins_over_a_valid_verify_envelope() {
    // The agent's verify claim is untrusted: a crashed invocation never
    // reaches the graders, whatever envelope it carried.
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-crash-verify");
    let mut result = verify_iteration();
    result.failure = Some(failure("ProcessError", "cli exited 137"));
    let (invoke, _) = scripted_invoker(vec![result]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    let timestamps = &outcome.lifecycle.timestamps;
    assert!(timestamps.contains_key(&Status::InternalError));
    assert!(!timestamps.contains_key(&Status::Validating));
    assert!(
        h.store
            .list_grader_results("run-crash-verify", 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(h.sink.of_kind("run-crash-verify", "harness.crash").len(), 1);
}

#[tokio::test]
async fn transient_exhaustion_walks_through_the_retryable_internal_error() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-rl-internal");
    let (invoke, _) = scripted_invoker(vec![rate_limited_iteration(Some(429), false)]);
    let config = HarnessConfig {
        max_retries: 0,
        max_transient_retries: 0,
        transient_sleep: Some(no_sleep()),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert!(
        outcome
            .lifecycle
            .timestamps
            .contains_key(&Status::InternalError)
    );
}

#[tokio::test]
async fn zero_iteration_cap_walks_through_failed_validation() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-cap-zero-walk");
    let (invoke, _) = scripted_invoker(vec![]);
    let config = HarnessConfig {
        max_iterations_per_attempt: 0,
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    let timestamps = &outcome.lifecycle.timestamps;
    assert!(timestamps.contains_key(&Status::FailedValidation));
    assert!(!timestamps.contains_key(&Status::InternalError));
}

#[tokio::test]
async fn one_failing_command_grader_fails_the_attempt() {
    let h = Harness::new();
    let task = task(vec![
        command("true", Some("a")),
        command("false", Some("b")),
        command("true", Some("c")),
    ]);
    let mut lc = lifecycle("run-one-fails");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert!(!outcome.lifecycle.timestamps.contains_key(&Status::Done));
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::ValidationFailed));
    assert_eq!(outcome.attempts[0].error, "command grader \"b\" failed");
}
