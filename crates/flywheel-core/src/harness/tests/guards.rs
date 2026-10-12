//! Loop guards (STUCK / THRASH), the hang watchdog, the wall-clock
//! iteration deadline, and the checkpoint-nudge threading.

use super::support::*;
use crate::deadline_config::DeadlineConfig;
use crate::envelope::Intent;
use crate::harness::{HarnessConfig, InvokeFn};
use crate::invoker::IterationResult;
use crate::lifecycle::{Outcome, Status};
use crate::loop_guard::LoopGuardConfig;
use crate::store::{GraderResultStore, LifecycleStore};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn guard(stuck: Option<u32>, thrash: Option<u32>, window: Option<u32>) -> LoopGuardConfig {
    LoopGuardConfig {
        repeated_tool_failure_threshold: stuck,
        thrash_repeat_threshold: thrash,
        thrash_window: window,
        hang_timeout_seconds: None,
    }
}

fn continue_with_tools(tools: Vec<crate::invoker::ToolInteraction>) -> IterationResult {
    let mut result = iteration(valid(Intent::Continue), vec![assistant()]);
    result.signals = signals_with_tools(tools);
    result
}

fn failing_block(n: usize) -> Vec<crate::invoker::ToolInteraction> {
    (0..n).map(|i| tool_call(&format!("t{i}"), true)).collect()
}

// --- STUCK ----------------------------------------------------------------

#[tokio::test]
async fn three_consecutive_failing_calls_route_to_interrupted() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-loop-stuck");
    let (invoke, _) = scripted_invoker(vec![continue_with_tools(failing_block(3))]);
    let config = HarnessConfig {
        max_retries: 1,
        loop_guard: guard(Some(3), None, None),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.lifecycle.retries, 0);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Cancelled));
    assert!(
        h.store
            .list_grader_results("run-loop-stuck", 1)
            .unwrap()
            .is_empty()
    );
    let stuck = h.sink.of_kind("run-loop-stuck", "harness.stuck");
    assert_eq!(stuck.len(), 1);
    let payload = &stuck[0].payload;
    assert_eq!(payload["tool_name"], "Bash");
    let digest = payload["input_digest"].as_str().unwrap();
    assert_eq!(digest.len(), 64);
    assert_eq!(
        payload["requires"],
        json!([{"type": "tool_loop_block", "tool_name": "Bash", "input_digest": digest}])
    );
    assert_eq!(
        outcome.lifecycle.blocked_requires_json.as_deref(),
        Some(format!(r#"[{{"type": "tool_loop_block", "tool_name": "Bash", "input_digest": "{digest}"}}]"#).as_str())
    );
    // An operator transition to READY clears the snapshot.
    let mut reloaded = h.store.load_lifecycle("run-loop-stuck").unwrap().unwrap();
    reloaded.transition_to(Status::Ready, "", None).unwrap();
    assert_eq!(reloaded.blocked_requires_json, None);
}

#[tokio::test]
async fn stuck_trips_on_final_iteration_and_preempts_cap() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-stuck-on-cap");
    let (invoke, _) = scripted_invoker(vec![
        continue_with_tools(vec![tool_call("g1", false)]),
        continue_with_tools(failing_block(3)),
    ]);
    let config = HarnessConfig {
        max_retries: 2,
        max_iterations_per_attempt: 2,
        loop_guard: guard(Some(3), None, None),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Cancelled));
    let kinds = h.sink.kinds("run-stuck-on-cap");
    assert!(kinds.contains(&"harness.stuck".to_string()));
    assert!(!kinds.contains(&"harness.protocol_failure".to_string()));
}

// --- THRASH ---------------------------------------------------------------

#[tokio::test]
async fn thrash_routes_to_ready_when_retries_remain() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-thrash-retry");
    let tools: Vec<_> = (0..4).map(|i| tool_call(&format!("t{i}"), false)).collect();
    let (invoke, _) = scripted_invoker(vec![continue_with_tools(tools), verify_iteration()]);
    let config = HarnessConfig {
        max_retries: 1,
        loop_guard: guard(None, Some(4), Some(12)),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.lifecycle.retries, 1);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::AgentError));
    let kinds = h.sink.kinds("run-thrash-retry");
    assert!(kinds.contains(&"harness.thrash_detected".to_string()));
    assert!(kinds.contains(&"harness.retry_scheduled".to_string()));
    let thrash = h
        .sink
        .of_kind("run-thrash-retry", "harness.thrash_detected");
    assert_eq!(thrash.len(), 1);
    assert_eq!(thrash[0].payload["tool_name"], "Bash");
    assert_eq!(
        thrash[0].payload["input_digest"].as_str().unwrap().len(),
        64
    );
    // THRASH walks through VALIDATING so the FAILED_VALIDATION edge is legal.
    assert!(
        outcome
            .lifecycle
            .timestamps
            .contains_key(&Status::Validating)
    );
}

#[tokio::test]
async fn thrash_routes_to_failed_when_no_retries_remain() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-thrash-final");
    let tools: Vec<_> = (0..4).map(|i| tool_call(&format!("t{i}"), false)).collect();
    let (invoke, _) = scripted_invoker(vec![continue_with_tools(tools)]);
    let config = HarnessConfig {
        loop_guard: guard(None, Some(4), Some(12)),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::AgentError));
    let kinds = h.sink.kinds("run-thrash-final");
    assert!(kinds.contains(&"harness.thrash_detected".to_string()));
    assert!(!kinds.contains(&"harness.retry_scheduled".to_string()));
}

#[tokio::test]
async fn failing_repeats_satisfy_both_but_route_to_interrupted() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-both");
    let (invoke, _) = scripted_invoker(vec![continue_with_tools(failing_block(4))]);
    let config = HarnessConfig {
        max_retries: 1,
        loop_guard: guard(Some(3), Some(4), Some(12)),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Cancelled));
    let kinds = h.sink.kinds("run-both");
    assert!(kinds.contains(&"harness.stuck".to_string()));
    assert!(!kinds.contains(&"harness.thrash_detected".to_string()));
}

#[tokio::test]
async fn thrashing_stream_runs_to_cap_when_detectors_disabled() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-loop-off");
    let tools: Vec<_> = (0..4).map(|i| tool_call(&format!("t{i}"), false)).collect();
    let (invoke, _) = scripted_invoker(vec![continue_with_tools(tools)]);
    let config = HarnessConfig {
        loop_guard: guard(None, None, None),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::AgentError));
    let kinds = h.sink.kinds("run-loop-off");
    assert!(!kinds.contains(&"harness.stuck".to_string()));
    assert!(!kinds.contains(&"harness.thrash_detected".to_string()));
}

#[tokio::test]
async fn loop_guard_counters_do_not_bleed_across_attempts() {
    // Two failing calls in attempt 1 (below threshold), two more in attempt
    // 2: a fresh guard per attempt never reaches three.
    let h = Harness::new();
    let task = task(vec![fail_command()]);
    let mut lc = lifecycle("run-guard-fresh");
    let mut first = iteration(valid(Intent::Verify), vec![assistant()]);
    first.signals = signals_with_tools(failing_block(2));
    let mut second = iteration(valid(Intent::Verify), vec![assistant()]);
    second.signals = signals_with_tools(failing_block(2));
    let (invoke, _) = scripted_invoker(vec![first, second]);
    let config = HarnessConfig {
        max_retries: 1,
        loop_guard: guard(Some(3), None, None),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert!(
        !h.sink
            .kinds("run-guard-fresh")
            .contains(&"harness.stuck".to_string())
    );
}

// --- Hang watchdog --------------------------------------------------------

fn fake_mclock() -> (crate::grader_transcript::MonotonicClock, Arc<Mutex<f64>>) {
    let state = Arc::new(Mutex::new(0.0f64));
    let reader = state.clone();
    (Arc::new(move || *reader.lock().unwrap()), state)
}

fn hang_config(hang_timeout: f64) -> HarnessConfig {
    HarnessConfig {
        loop_guard: LoopGuardConfig {
            hang_timeout_seconds: Some(hang_timeout),
            ..guard(None, None, None)
        },
        ..HarnessConfig::default()
    }
}

/// An invoker that stalls forever once entered, advancing the fake clock
/// past the threshold so the next watchdog tick trips.
fn stalling_invoker(state: Arc<Mutex<f64>>) -> InvokeFn {
    Arc::new(move |_request| {
        let state = state.clone();
        Box::pin(async move {
            *state.lock().unwrap() = 1000.0;
            std::future::pending::<IterationResult>().await
        })
    })
}

#[tokio::test]
async fn stalled_stream_routes_to_internal_error() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-hang-stall");
    let (mclock, state) = fake_mclock();
    let hang_timeout = 0.04;
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &hang_config(hang_timeout),
            stalling_invoker(state),
            h.options().with_monotonic(mclock),
        )
        .await
        .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    assert!(attempt.error.contains("hang watchdog"));
    let hang = h.sink.of_kind("run-hang-stall", "harness.hang_detected");
    assert_eq!(hang.len(), 1);
    assert_eq!(hang[0].payload["iteration"], 1);
    assert_eq!(hang[0].payload["hang_timeout_seconds"], hang_timeout);
    assert!(hang[0].payload["silence_seconds"].as_f64().unwrap() >= hang_timeout);
    assert_eq!(hang[0].attempt_number, Some(1));
    assert!(
        h.sink
            .of_kind("run-hang-stall", "harness.interrupted")
            .is_empty()
    );
    assert!(outcome.lifecycle.error.contains("hang watchdog"));
}

#[tokio::test]
async fn heartbeat_under_threshold_runs_to_completion() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("ok"))]);
    let mut lc = lifecycle("run-hang-heartbeat");
    let (mclock, state) = fake_mclock();
    let hang_timeout = 0.1;
    let invoke: InvokeFn = Arc::new(move |request| {
        let state = state.clone();
        Box::pin(async move {
            let msgs = vec![
                assistant_with("m1", None),
                assistant_with("m2", None),
                assistant_with("m3", None),
                assistant_with("m4", None),
                assistant(),
                result_msg(),
            ];
            for msg in &msgs {
                tokio::time::sleep(Duration::from_millis(5)).await;
                *state.lock().unwrap() += hang_timeout * 0.5;
                if let Some(on_message) = &request.observers.on_message {
                    on_message(msg);
                }
            }
            iteration(valid(Intent::Verify), msgs)
        })
    });
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &hang_config(hang_timeout),
            invoke,
            h.options().with_monotonic(mclock),
        )
        .await
        .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let kinds = h.sink.kinds("run-hang-heartbeat");
    assert!(!kinds.contains(&"harness.hang_detected".to_string()));
    assert!(!kinds.contains(&"harness.interrupted".to_string()));
}

#[tokio::test]
async fn steady_rate_limit_stream_never_trips_watchdog() {
    let h = Harness::new();
    let task = task(vec![command("true", Some("ok"))]);
    let mut lc = lifecycle("run-hang-ratelimit");
    let (mclock, state) = fake_mclock();
    let hang_timeout = 0.1;
    let rate_events: Vec<_> = (0..6)
        .map(|i| json!({"message_type": "RateLimitEvent", "uuid": format!("evt-{i}"), "rate_limit_info": {"status": "allowed_warning"}}))
        .collect();
    let invoke: InvokeFn = Arc::new(move |request| {
        let state = state.clone();
        let rate_events = rate_events.clone();
        Box::pin(async move {
            let mut all = rate_events.clone();
            all.push(assistant());
            all.push(result_msg());
            for msg in &all {
                tokio::time::sleep(Duration::from_millis(5)).await;
                *state.lock().unwrap() += hang_timeout * 0.5;
                if let Some(on_message) = &request.observers.on_message {
                    on_message(msg);
                }
            }
            let mut result = iteration(valid(Intent::Verify), all);
            result.signals.rate_limit_events = rate_events;
            result
        })
    });
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &hang_config(hang_timeout),
            invoke,
            h.options().with_monotonic(mclock),
        )
        .await
        .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let completed = h
        .sink
        .of_kind("run-hang-ratelimit", "harness.iteration_completed");
    assert_eq!(completed[0].payload["rate_limited"], true);
    assert!(
        h.sink
            .of_kind("run-hang-ratelimit", "harness.hang_detected")
            .is_empty()
    );
}

#[tokio::test]
async fn operator_cancel_with_watchdog_disabled_is_unchanged() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-cancel-watchdog-off");
    let shutdown = crate::harness::ShutdownSignal::new();
    let trigger = shutdown.clone();
    let invoke: InvokeFn = Arc::new(move |_request| {
        let trigger = trigger.clone();
        Box::pin(async move {
            trigger.trigger();
            std::future::pending::<IterationResult>().await
        })
    });
    let config = HarnessConfig::default();
    assert_eq!(config.loop_guard.hang_timeout_seconds, None);
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
    assert!(matches!(err, crate::harness::HarnessError::Interrupted));
    let reloaded = h
        .store
        .load_lifecycle("run-cancel-watchdog-off")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, Status::Interrupted);
    assert_eq!(reloaded.retries, 0);
    assert_eq!(
        h.sink
            .of_kind("run-cancel-watchdog-off", "harness.interrupted")
            .len(),
        1
    );
    assert!(
        h.sink
            .of_kind("run-cancel-watchdog-off", "harness.hang_detected")
            .is_empty()
    );
}

#[tokio::test]
async fn hang_detected_lands_in_telemetry_stream_in_order() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-audit-hang");
    let (mclock, state) = fake_mclock();
    h.run_with(
        &task,
        &mut lc,
        &hang_config(0.04),
        stalling_invoker(state),
        h.options().with_monotonic(mclock),
    )
    .await
    .unwrap();
    assert_eq!(
        h.sink.kinds("run-audit-hang"),
        [
            "harness.attempt_started",
            "harness.hang_detected",
            "harness.attempt_finalized"
        ]
    );
}

// --- Wall-clock agent-iteration deadline ---------------------------------

fn never_returning() -> InvokeFn {
    Arc::new(|_request| Box::pin(std::future::pending::<IterationResult>()))
}

fn deadline_config(ceiling: Option<f64>) -> HarnessConfig {
    HarnessConfig {
        deadlines: DeadlineConfig {
            agent_iteration_seconds: ceiling,
            ..DeadlineConfig::default()
        },
        ..HarnessConfig::default()
    }
}

#[tokio::test]
async fn agent_deadline_cancels_never_returning_invoker() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-agent-deadline");
    let outcome = h
        .run(
            &task,
            &mut lc,
            &deadline_config(Some(0.05)),
            never_returning(),
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    assert!(attempt.error.contains("deadline"));
    let events = h
        .sink
        .of_kind("run-agent-deadline", "harness.deadline_exceeded");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["deadline_class"], "agent_iteration");
    assert_eq!(events[0].payload["ceiling_seconds"], 0.05);
    assert_eq!(events[0].payload["iteration"], 1);
    assert_eq!(events[0].attempt_number, Some(1));
    assert!(events[0].payload["elapsed_seconds"].as_f64().unwrap() >= 0.05);
    let kinds = h.sink.kinds("run-agent-deadline");
    assert!(!kinds.contains(&"harness.interrupted".to_string()));
    assert!(!kinds.contains(&"harness.hang_detected".to_string()));
}

#[tokio::test]
async fn agent_deadline_streaming_invoker_is_cut_off() {
    // The bound is wall-clock, not silence: a steadily streaming invoker
    // that the hang watchdog would never trip on is still cancelled.
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-agent-deadline-stream");
    let invoke: InvokeFn = Arc::new(|request| {
        Box::pin(async move {
            loop {
                if let Some(on_message) = &request.observers.on_message {
                    on_message(&assistant_with("still working", None));
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    });
    let config = HarnessConfig {
        loop_guard: LoopGuardConfig {
            hang_timeout_seconds: Some(10.0),
            ..guard(None, None, None)
        },
        ..deadline_config(Some(0.05))
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    assert!(outcome.attempts[0].error.contains("deadline"));
    assert!(
        !h.sink
            .of_kind("run-agent-deadline-stream", "harness.deadline_exceeded")
            .is_empty()
    );
    assert!(
        h.sink
            .of_kind("run-agent-deadline-stream", "harness.hang_detected")
            .is_empty()
    );
}

#[tokio::test]
async fn agent_deadline_normal_completion_not_tripped() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-agent-deadline-ok");
    let config = HarnessConfig::default();
    assert!(config.deadlines.agent_iteration_seconds.is_some());
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert!(
        h.sink
            .of_kind("run-agent-deadline-ok", "harness.deadline_exceeded")
            .is_empty()
    );
}

#[tokio::test]
async fn task_budget_overrides_the_agent_iteration_ceiling() {
    // A per-task `agent_iteration_seconds` beats the policy ceiling; `0`
    // opts the task out entirely (threads a None ceiling to the request).
    let h = Harness::new();
    let mut task = task(vec![]);
    task.budgets.agent_iteration_seconds = Some(crate::task::Seconds::Float(0.05));
    let mut lc = lifecycle("run-task-budget-ceiling");
    let outcome = h
        .run(
            &task,
            &mut lc,
            &deadline_config(Some(3600.0)),
            never_returning(),
        )
        .await;
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    let events = h
        .sink
        .of_kind("run-task-budget-ceiling", "harness.deadline_exceeded");
    assert_eq!(events[0].payload["ceiling_seconds"], 0.05);

    task.budgets.agent_iteration_seconds = Some(crate::task::Seconds::Int(0));
    let mut lc = lifecycle("run-task-budget-optout");
    let (invoke, calls) = scripted_invoker(vec![verify_iteration()]);
    h.run(&task, &mut lc, &deadline_config(Some(3600.0)), invoke)
        .await;
    assert_eq!(
        calls.lock().unwrap()[0].agent_iteration_ceiling_seconds,
        None
    );
}

// --- Checkpoint nudge threading ------------------------------------------

#[tokio::test]
async fn harness_threads_nudge_fields_onto_request() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-nudge-thread");
    let probe: crate::invoker_client::ProgressProbe = Arc::new(|| Some(json!("tok")));
    let config = HarnessConfig {
        checkpoint_nudge_seconds: 123.0,
        checkpoint_progress_probe: Some(probe.clone()),
        ..deadline_config(Some(777.0))
    };
    let (invoke, calls) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let requests = calls.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].checkpoint_nudge_seconds, Some(123.0));
    assert_eq!(requests[0].agent_iteration_ceiling_seconds, Some(777.0));
    let threaded = requests[0]
        .observers
        .checkpoint_progress_probe
        .as_ref()
        .unwrap();
    assert!(Arc::ptr_eq(threaded, &probe));
    assert!(requests[0].observers.on_message.is_some());
    assert!(requests[0].observers.context_observer.is_some());
    assert!(requests[0].observers.recovery_interrupt.is_some());
    assert!(requests[0].observers.on_command_applied.is_some());
    assert_eq!(requests[0].resume_session_id, None);
}

#[tokio::test]
async fn harness_threads_defaults_dormant_and_none_ceiling_when_opted_out() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-nudge-default");
    let (invoke, calls) = scripted_invoker(vec![verify_iteration()]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    {
        let requests = calls.lock().unwrap();
        assert!(requests[0].observers.checkpoint_progress_probe.is_none());
        assert_eq!(requests[0].checkpoint_nudge_seconds, Some(300.0));
        assert_eq!(requests[0].agent_iteration_ceiling_seconds, Some(3600.0));
    }
    let mut lc = lifecycle("run-nudge-optout");
    let probe: crate::invoker_client::ProgressProbe = Arc::new(|| Some(json!("tok")));
    let config = HarnessConfig {
        checkpoint_progress_probe: Some(probe),
        ..deadline_config(None)
    };
    let (invoke, calls) = scripted_invoker(vec![verify_iteration()]);
    h.run(&task, &mut lc, &config, invoke).await;
    let requests = calls.lock().unwrap();
    assert_eq!(requests[0].agent_iteration_ceiling_seconds, None);
    assert!(requests[0].observers.checkpoint_progress_probe.is_some());
}

#[tokio::test]
async fn nudge_config_does_not_move_the_deadline() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-nudge-deadline");
    let probe: crate::invoker_client::ProgressProbe = Arc::new(|| Some(json!("tok")));
    let config = HarnessConfig {
        checkpoint_nudge_seconds: 0.01,
        checkpoint_progress_probe: Some(probe),
        ..deadline_config(Some(0.05))
    };
    let outcome = h.run(&task, &mut lc, &config, never_returning()).await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert!(outcome.attempts[0].error.contains("deadline"));
    let events = h
        .sink
        .of_kind("run-nudge-deadline", "harness.deadline_exceeded");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["ceiling_seconds"], 0.05);
}
