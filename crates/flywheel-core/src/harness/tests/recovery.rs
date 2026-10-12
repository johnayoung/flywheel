//! Context recovery (boundary and mid-turn), the mid-turn threshold
//! observe tiers, and the steering ledger.

use super::support::*;
use crate::envelope::Intent;
use crate::events::{DomainEventPayload, replay};
use crate::harness::{HarnessConfig, InvokeFn};
use crate::invoker::IterationResult;
use crate::invoker_client::{FAILURE_HARNESS_RECOVERY, enqueue_say};
use crate::lifecycle::{Outcome, Status};
use crate::loop_guard::LoopGuardConfig;
use crate::store::{ControlCommandStore, DomainEventStore};
use chrono::Utc;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn recovery_config(
    dir: &tempfile::TempDir,
    summarizer: Arc<ScriptedSummarizer>,
    max: u32,
) -> HarnessConfig {
    HarnessConfig {
        context_window_tokens: Some(100),
        context_recovery_trigger_ratio: 0.9,
        max_context_recoveries: max,
        recovery_summarizer_invoke: Some(summarizer),
        ..worktree_config(dir)
    }
}

fn continue_at(input_tokens: u64, transcript: &str) -> IterationResult {
    let mut result = iteration(
        valid(Intent::Continue),
        vec![
            assistant_usage(usage_dict(input_tokens)),
            result_msg_with(1, Some(usage_dict(input_tokens))),
        ],
    );
    result.transcript = transcript.into();
    result
}

fn verify_at(input_tokens: u64) -> IterationResult {
    let mut result = iteration(
        valid_reason(Intent::Verify, "done"),
        vec![
            assistant_usage(usage_dict(input_tokens)),
            result_msg_with(1, Some(usage_dict(input_tokens))),
        ],
    );
    result.transcript = wrap(r#"{"intent": "verify", "reason": "done"}"#);
    result
}

fn verify_done() -> IterationResult {
    let mut result = iteration(
        valid_reason(Intent::Verify, "done"),
        vec![assistant(), result_msg()],
    );
    result.transcript = wrap(r#"{"intent": "verify", "reason": "done"}"#);
    result
}

fn index_of(kinds: &[String], kind: &str, nth: usize) -> usize {
    kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| *k == kind)
        .map(|(i, _)| i)
        .nth(nth)
        .unwrap()
}

// --- Boundary recovery (spec 00018) -----------------------------------------

#[tokio::test]
async fn over_ratio_continue_triggers_recovery_and_fresh_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-recover-happy");
    let handoff = handoff(
        "probed handler X",
        "implement handler Y",
        "use approach Z",
        "open handler.py and patch dispatch",
    );
    let (invoke, calls) = scripted_invoker(vec![
        continue_at(95, "iteration-one transcript"),
        verify_done(),
    ]);
    let summarizer = ScriptedSummarizer::new(handoff.clone());
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer.clone(), 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Recovered));
    assert_eq!(outcome.attempts[0].agent_output, "iteration-one transcript");
    assert_eq!(outcome.attempts[1].outcome, Some(Outcome::Succeeded));
    assert_eq!(outcome.lifecycle.retries, 0);
    let recoveries = h
        .sink
        .of_kind("run-recover-happy", "harness.context_recovery");
    assert_eq!(recoveries.len(), 1);
    let payload = &recoveries[0].payload;
    assert_eq!(payload["occupancy_tokens"], 95);
    assert_eq!(payload["context_window_tokens"], 100);
    assert_eq!(payload["context_recovery_trigger_ratio"], 0.9);
    assert_eq!(payload["recoveries_used"], 1);
    assert_eq!(payload["recoveries_remaining"], 0);
    assert_eq!(payload["attempt_number"], 1);
    assert_eq!(payload["iteration"], 1);
    assert_eq!(payload["trigger"], "boundary");
    assert_eq!(
        payload["summary_digest"]["work_done_length"],
        handoff.work_done.len()
    );
    assert_eq!(
        payload["summary_digest"]["suggested_next_step_length"],
        handoff.suggested_next_step.len()
    );
    // The recovery event precedes the recovery attempt's start.
    let kinds = h.sink.kinds("run-recover-happy");
    let recovery_idx = index_of(&kinds, "harness.context_recovery", 0);
    let started_idx = index_of(&kinds, "harness.attempt_started", 1);
    assert!(recovery_idx < started_idx);
    // Legal edge walk: running -> interrupted -> ready, then attempt 2.
    assert!(
        outcome
            .lifecycle
            .timestamps
            .contains_key(&Status::Interrupted)
    );
    let recovery_prompt = prompt_of(&calls, 1);
    assert!(recovery_prompt.contains("# Recovery handoff"));
    assert!(recovery_prompt.contains(&handoff.work_done));
    assert!(recovery_prompt.contains(&handoff.suggested_next_step));
    assert!(!prompt_of(&calls, 0).contains("# Recovery handoff"));
    assert_eq!(summarizer.call_count(), 1);
    let (summarizer_prompt, worktree) = summarizer.calls.lock().unwrap()[0].clone();
    assert_eq!(worktree, dir.path());
    assert!(summarizer_prompt.contains("iteration-one transcript"));
    assert!(summarizer_prompt.contains(&task.goal));
    // The recovery attempt resumes with attempt 2 / iteration 1.
    let requests = calls.lock().unwrap();
    assert_eq!(requests[1].attempt_number, 2);
    assert_eq!(requests[1].iteration_number, 1);
    // Ledger replay still folds to the stored lifecycle.
    let folded = replay(&h.store.list_domain_events("run-recover-happy").unwrap()).unwrap();
    assert_eq!(folded.status, Status::Done);
    assert_eq!(folded.version, outcome.lifecycle.version);
}

#[tokio::test]
async fn below_ratio_does_not_recover() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-recover-below");
    let (invoke, _) = scripted_invoker(vec![verify_at(10)]);
    let summarizer = ScriptedSummarizer::new(handoff("", "", "", ""));
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer.clone(), 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 1);
    assert!(
        h.sink
            .of_kind("run-recover-below", "harness.context_recovery")
            .is_empty()
    );
    assert_eq!(summarizer.call_count(), 0);
}

#[tokio::test]
async fn disabled_when_capacity_none() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-recover-disabled");
    let (invoke, _) = scripted_invoker(vec![verify_at(999_999_999)]);
    let config = HarnessConfig::default();
    assert!(config.context_window_tokens.is_none());
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert!(
        h.sink
            .of_kind("run-recover-disabled", "harness.context_recovery")
            .is_empty()
    );
}

#[tokio::test]
async fn recovery_budget_caps_at_max() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-recover-budget");
    let (invoke, _) = scripted_invoker(vec![continue_at(95, "t1"), continue_at(95, "t2")]);
    let summarizer = ScriptedSummarizer::new(handoff("a", "b", "c", "d"));
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer.clone(), 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Recovered));
    assert_eq!(outcome.attempts[1].outcome, Some(Outcome::AgentError));
    let recoveries = h
        .sink
        .of_kind("run-recover-budget", "harness.context_recovery");
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0].payload["trigger"], "boundary");
    assert_eq!(summarizer.call_count(), 1);
}

#[tokio::test]
async fn loop_guard_verdict_preempts_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-recover-precedence-stuck");
    let mut result = continue_at(95, "stuck-and-over-ratio");
    result.signals = signals_with_tools(vec![
        tool_call("t1", true),
        tool_call("t2", true),
        tool_call("t3", true),
    ]);
    let (invoke, _) = scripted_invoker(vec![result]);
    let summarizer = ScriptedSummarizer::new(handoff("", "", "", ""));
    let config = HarnessConfig {
        loop_guard: LoopGuardConfig {
            repeated_tool_failure_threshold: Some(3),
            thrash_repeat_threshold: None,
            thrash_window: None,
            hang_timeout_seconds: None,
        },
        ..recovery_config(&dir, summarizer.clone(), 1)
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.lifecycle.status, Status::Interrupted);
    assert!(
        h.sink
            .of_kind("run-recover-precedence-stuck", "harness.context_recovery")
            .is_empty()
    );
    assert_eq!(summarizer.call_count(), 0);
}

#[tokio::test]
async fn completion_claim_preempts_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-recover-precedence-verify");
    let (invoke, _) = scripted_invoker(vec![verify_at(95)]);
    let summarizer = ScriptedSummarizer::new(handoff("", "", "", ""));
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer.clone(), 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert!(
        h.sink
            .of_kind("run-recover-precedence-verify", "harness.context_recovery")
            .is_empty()
    );
    assert_eq!(summarizer.call_count(), 0);
}

#[tokio::test]
async fn summarizer_failure_routes_to_internal_error() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-recover-summ-fail");
    let (invoke, _) = scripted_invoker(vec![continue_at(95, "over-ratio-and-broken-summarizer")]);
    let summarizer = ScriptedSummarizer::failing("summarizer crashed");
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer, 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    assert_eq!(
        outcome.attempts[0].error,
        "recovery summarizer failed: summarizer crashed"
    );
    assert!(
        h.sink
            .of_kind("run-recover-summ-fail", "harness.context_recovery")
            .is_empty()
    );
    let crashes: Vec<_> = h
        .sink
        .of_kind("run-recover-summ-fail", "harness.crash")
        .into_iter()
        .filter(|e| e.payload["classification"] == "recovery_summarizer_error")
        .collect();
    assert_eq!(crashes.len(), 1);
    assert_eq!(crashes[0].payload["trigger"], "boundary");
}

#[tokio::test]
async fn missing_worktree_fails_the_summarizer_not_the_run_silently() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-recover-no-worktree");
    let (invoke, _) = scripted_invoker(vec![continue_at(95, "t")]);
    let summarizer = ScriptedSummarizer::new(handoff("a", "b", "c", "d"));
    let config = HarnessConfig {
        context_window_tokens: Some(100),
        recovery_summarizer_invoke: Some(summarizer.clone()),
        ..HarnessConfig::default()
    };
    let outcome = h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    assert!(outcome.attempts[0].error.contains("worktree not available"));
    assert_eq!(summarizer.call_count(), 0);
}

// --- Mid-turn observe (spec 00019 FR-1..FR-3) ---------------------------------

fn crossings(h: &Harness, run_id: &str) -> Vec<crate::store::TelemetryRecord> {
    h.sink.of_kind(run_id, "harness.context_threshold_crossed")
}

fn tiers(records: &[crate::store::TelemetryRecord]) -> Vec<f64> {
    records
        .iter()
        .map(|c| c.payload["tier"].as_f64().unwrap())
        .collect()
}

#[tokio::test]
async fn three_tiers_fire_in_order_from_streamed_usage() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-observe-tiers");
    let mut result = iteration(
        valid_reason(Intent::Verify, "done"),
        vec![
            assistant_usage(usage_dict(40)),
            assistant_usage(usage_dict(60)),
            assistant_usage(usage_dict(80)),
            assistant_usage(usage_dict(95)),
            result_msg_with(1, Some(usage_dict(95))),
        ],
    );
    result.transcript = wrap(r#"{"intent": "verify", "reason": "done"}"#);
    let (invoke, _) = scripted_invoker(vec![result]);
    let config = HarnessConfig {
        context_window_tokens: Some(100),
        ..HarnessConfig::default()
    };
    h.run(&task, &mut lc, &config, invoke).await;
    let events = h.sink.events("run-observe-tiers");
    let crossed = crossings(&h, "run-observe-tiers");
    assert_eq!(tiers(&crossed), [0.5, 0.75, 0.9]);
    let kinds: Vec<String> = events.iter().map(|e| e.kind.clone()).collect();
    let done_idx = index_of(&kinds, "harness.iteration_completed", 0);
    let last_crossing = kinds
        .iter()
        .rposition(|k| k == "harness.context_threshold_crossed")
        .unwrap();
    assert!(last_crossing < done_idx);
    for c in &crossed {
        assert_eq!(c.payload["iteration"], 1);
        assert_eq!(c.payload["capacity_tokens"], 100);
        assert_eq!(c.payload["capacity_source"], "operator");
        assert_eq!(c.iteration_number, Some(1));
    }
    assert_eq!(crossed[0].payload["occupancy_tokens"], 60);
    assert_eq!(crossed[1].payload["occupancy_tokens"], 80);
    assert_eq!(crossed[2].payload["occupancy_tokens"], 95);
    assert_eq!(crossed[0].payload["percentage"], 60.0);
    assert_eq!(crossed[2].payload["percentage"], 95.0);
}

#[tokio::test]
async fn no_capacity_no_events_even_with_huge_usage() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-observe-disabled");
    let (invoke, _) = scripted_invoker(vec![verify_at(999_999_999)]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    assert!(crossings(&h, "run-observe-disabled").is_empty());
    assert!(
        h.sink
            .of_kind("run-observe-disabled", "harness.context_recovery")
            .is_empty()
    );
}

#[tokio::test]
async fn re_cross_in_same_iteration_does_not_duplicate_event() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-observe-recross");
    let mut result = iteration(
        valid_reason(Intent::Verify, "done"),
        vec![
            assistant_usage(usage_dict(60)),
            assistant_usage(usage_dict(40)),
            assistant_usage(usage_dict(60)),
            result_msg_with(1, Some(usage_dict(60))),
        ],
    );
    result.transcript = wrap(r#"{"intent": "verify", "reason": "done"}"#);
    let (invoke, _) = scripted_invoker(vec![result]);
    let config = HarnessConfig {
        context_window_tokens: Some(100),
        ..HarnessConfig::default()
    };
    h.run(&task, &mut lc, &config, invoke).await;
    assert_eq!(tiers(&crossings(&h, "run-observe-recross")), [0.5]);
}

#[tokio::test]
async fn one_message_jumps_all_three_tiers_emits_three_events() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-observe-jump");
    let (invoke, _) = scripted_invoker(vec![verify_at(95)]);
    let config = HarnessConfig {
        context_window_tokens: Some(100),
        ..HarnessConfig::default()
    };
    h.run(&task, &mut lc, &config, invoke).await;
    let crossed = crossings(&h, "run-observe-jump");
    assert_eq!(tiers(&crossed), [0.5, 0.75, 0.9]);
    for c in &crossed {
        assert_eq!(c.payload["occupancy_tokens"], 95);
        assert_eq!(c.payload["percentage"], 95.0);
    }
}

#[tokio::test]
async fn emitted_tiers_reset_per_iteration() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-observe-reset");
    let (invoke, _) = scripted_invoker(vec![continue_at(95, "iteration-one"), verify_at(95)]);
    let config = HarnessConfig {
        context_window_tokens: Some(100),
        max_iterations_per_attempt: 2,
        max_context_recoveries: 0,
        ..HarnessConfig::default()
    };
    h.run(&task, &mut lc, &config, invoke).await;
    let crossed = crossings(&h, "run-observe-reset");
    assert_eq!(crossed.len(), 6);
    let by_iter = |n: u64| -> Vec<f64> {
        crossed
            .iter()
            .filter(|c| c.payload["iteration"] == n)
            .map(|c| c.payload["tier"].as_f64().unwrap())
            .collect()
    };
    assert_eq!(by_iter(1), [0.5, 0.75, 0.9]);
    assert_eq!(by_iter(2), [0.5, 0.75, 0.9]);
}

/// An invoker that pumps SDK context readings through `context_observer`
/// before the iteration's messages.
fn invoker_with_readings(result: IterationResult, readings: Vec<Value>) -> InvokeFn {
    Arc::new(move |request| {
        let result = result.clone();
        let readings = readings.clone();
        Box::pin(async move {
            if let Some(observer) = &request.observers.context_observer {
                for reading in &readings {
                    observer(reading);
                }
            }
            if let Some(on_message) = &request.observers.on_message {
                for msg in &result.messages {
                    on_message(msg);
                }
            }
            result
        })
    })
}

fn sdk_reading(total: u64, max: u64) -> Value {
    json!({"totalTokens": total, "maxTokens": max, "rawMaxTokens": max, "percentage": 0.0, "model": "claude-test"})
}

#[tokio::test]
async fn sdk_reading_wins_over_estimate_and_marks_source_sdk() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-observe-sdk-wins");
    let invoke = invoker_with_readings(verify_at(5), vec![sdk_reading(80, 100)]);
    let config = HarnessConfig {
        context_window_tokens: Some(1000),
        ..HarnessConfig::default()
    };
    h.run(&task, &mut lc, &config, invoke).await;
    let crossed = crossings(&h, "run-observe-sdk-wins");
    assert_eq!(tiers(&crossed), [0.5, 0.75]);
    for c in &crossed {
        assert_eq!(c.payload["capacity_source"], "sdk");
        assert_eq!(c.payload["capacity_tokens"], 100);
        assert_eq!(c.payload["occupancy_tokens"], 80);
        assert_eq!(c.payload["percentage"], 80.0);
    }
}

#[tokio::test]
async fn sdk_reading_alone_provides_capacity_without_operator_knob() {
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-observe-sdk-only");
    let invoke = invoker_with_readings(verify_done(), vec![sdk_reading(60, 100)]);
    h.run(&task, &mut lc, &HarnessConfig::default(), invoke)
        .await;
    let crossed = crossings(&h, "run-observe-sdk-only");
    assert_eq!(tiers(&crossed), [0.5]);
    assert_eq!(crossed[0].payload["capacity_source"], "sdk");
    assert_eq!(crossed[0].payload["occupancy_tokens"], 60);
}

// --- Mid-turn act (spec 00019 FR-4..FR-7) -------------------------------------

/// An invoker honoring the recovery signal mid-stream: on the first call
/// it pumps `pre_messages` one at a time and, once the harness arms the
/// signal, returns the FAILURE_HARNESS_RECOVERY result the agents invoker
/// produces for a harness-initiated interrupt. Later calls return
/// `after_resume`.
fn midturn_invoker(pre_messages: Vec<Value>, after_resume: IterationResult) -> (InvokeFn, Calls) {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let invoke: InvokeFn = Arc::new(move |request| {
        let pre_messages = pre_messages.clone();
        let after_resume = after_resume.clone();
        let recorded = recorded.clone();
        Box::pin(async move {
            recorded.lock().unwrap().push(request.clone());
            let first = recorded.lock().unwrap().len() == 1;
            if first {
                if let Some(on_message) = &request.observers.on_message {
                    for msg in &pre_messages {
                        on_message(msg);
                        if request
                            .observers
                            .recovery_interrupt
                            .as_ref()
                            .is_some_and(|s| s.is_requested())
                        {
                            let mut result =
                                iteration(crate::envelope::EnvelopeResult::Missing, vec![]);
                            result.failure = Some(failure(
                                FAILURE_HARNESS_RECOVERY,
                                "run interrupted for a harness-initiated mid-turn recovery",
                            ));
                            return result;
                        }
                    }
                }
                return after_resume;
            }
            if let Some(on_message) = &request.observers.on_message {
                for msg in &after_resume.messages {
                    on_message(msg);
                }
            }
            after_resume
        })
    });
    (invoke, calls)
}

#[tokio::test]
async fn over_ratio_midturn_triggers_recovery_with_trigger_marker() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-midturn-happy");
    let handoff = handoff(
        "probed handler X mid-turn",
        "finish handler Y",
        "approach Z",
        "patch handler.py",
    );
    let (invoke, calls) = midturn_invoker(
        vec![
            assistant_with("early-work ", Some(usage_dict(40))),
            assistant_with("late-work-before-interrupt", Some(usage_dict(95))),
        ],
        verify_done(),
    );
    let summarizer = ScriptedSummarizer::new(handoff.clone());
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer.clone(), 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Recovered));
    assert_eq!(
        outcome.attempts[0].agent_output,
        "early-work late-work-before-interrupt"
    );
    assert_eq!(outcome.attempts[1].outcome, Some(Outcome::Succeeded));
    assert_eq!(outcome.lifecycle.retries, 0);
    let recoveries = h
        .sink
        .of_kind("run-midturn-happy", "harness.context_recovery");
    assert_eq!(recoveries.len(), 1);
    let payload = &recoveries[0].payload;
    assert_eq!(payload["trigger"], "mid_turn");
    assert_eq!(payload["occupancy_tokens"], 95);
    assert_eq!(payload["context_window_tokens"], 100);
    assert_eq!(payload["recoveries_used"], 1);
    assert_eq!(payload["recoveries_remaining"], 0);
    assert_eq!(payload["attempt_number"], 1);
    assert_eq!(payload["iteration"], 1);
    assert_eq!(
        payload["summary_digest"]["work_done_length"],
        handoff.work_done.len()
    );
    let kinds = h.sink.kinds("run-midturn-happy");
    assert!(
        index_of(&kinds, "harness.context_recovery", 0)
            < index_of(&kinds, "harness.attempt_started", 1)
    );
    // The interrupted iteration never emits iteration_completed; only the
    // recovery attempt's verify iteration does.
    let completed = h
        .sink
        .of_kind("run-midturn-happy", "harness.iteration_completed");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].attempt_number, Some(2));
    let recovery_prompt = prompt_of(&calls, 1);
    assert!(recovery_prompt.contains("# Recovery handoff"));
    assert!(recovery_prompt.contains(&handoff.work_done));
    assert!(!prompt_of(&calls, 0).contains("# Recovery handoff"));
    assert_eq!(summarizer.call_count(), 1);
    // The summarizer saw the streamed work-so-far, not a returned transcript.
    let (summarizer_prompt, _) = summarizer.calls.lock().unwrap()[0].clone();
    assert!(summarizer_prompt.contains("early-work "));
    assert!(summarizer_prompt.contains("late-work-before-interrupt"));
    let requests = calls.lock().unwrap();
    assert_eq!(requests[1].attempt_number, 2);
    assert_eq!(requests[1].iteration_number, 1);
}

#[tokio::test]
async fn observe_event_ordered_before_recovery_event() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-midturn-ordering");
    let (invoke, _) = midturn_invoker(
        vec![assistant_with("big-jump", Some(usage_dict(95)))],
        verify_done(),
    );
    let summarizer = ScriptedSummarizer::new(handoff("x", "y", "z", "next"));
    h.run(
        &task,
        &mut lc,
        &recovery_config(&dir, summarizer, 1),
        invoke,
    )
    .await;
    let events = h.sink.events("run-midturn-ordering");
    let observe_90 = events
        .iter()
        .position(|e| e.kind == "harness.context_threshold_crossed" && e.payload["tier"] == 0.9)
        .unwrap();
    let recovery = events
        .iter()
        .position(|e| e.kind == "harness.context_recovery")
        .unwrap();
    assert!(observe_90 < recovery);
}

#[tokio::test]
async fn budget_exhausted_skips_midturn_act() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-midturn-exhausted");
    let (invoke, _) = midturn_invoker(
        vec![assistant_with("late-work", Some(usage_dict(95)))],
        verify_done(),
    );
    let summarizer = ScriptedSummarizer::new(handoff("", "", "", ""));
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer.clone(), 0),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Succeeded));
    assert!(
        h.sink
            .of_kind("run-midturn-exhausted", "harness.context_recovery")
            .is_empty()
    );
    let mut observed = tiers(&crossings(&h, "run-midturn-exhausted"));
    observed.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(observed, [0.5, 0.75, 0.9]);
    assert_eq!(summarizer.call_count(), 0);
}

#[tokio::test]
async fn shared_budget_blocks_followup_boundary_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-midturn-shared-budget");
    let (invoke, _) = midturn_invoker(
        vec![assistant_with("late-work", Some(usage_dict(95)))],
        continue_at(95, "over-ratio-second"),
    );
    let summarizer = ScriptedSummarizer::new(handoff("midturn-done", "r", "d", "n"));
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer.clone(), 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Recovered));
    assert_eq!(outcome.attempts[1].outcome, Some(Outcome::AgentError));
    let recoveries = h
        .sink
        .of_kind("run-midturn-shared-budget", "harness.context_recovery");
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0].payload["trigger"], "mid_turn");
    assert_eq!(summarizer.call_count(), 1);
}

#[tokio::test]
async fn midturn_summarizer_failure_routes_to_internal_error() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-midturn-summ-fail");
    let (invoke, _) = midturn_invoker(
        vec![assistant_with("late-work", Some(usage_dict(95)))],
        verify_done(),
    );
    let summarizer = ScriptedSummarizer::failing("midturn summarizer crashed");
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer, 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
    assert!(
        h.sink
            .of_kind("run-midturn-summ-fail", "harness.context_recovery")
            .is_empty()
    );
    let crashes: Vec<_> = h
        .sink
        .of_kind("run-midturn-summ-fail", "harness.crash")
        .into_iter()
        .filter(|e| e.payload["classification"] == "recovery_summarizer_error")
        .collect();
    assert_eq!(crashes.len(), 1);
    assert_eq!(crashes[0].payload["trigger"], "mid_turn");
}

#[tokio::test]
async fn plain_path_invoker_degrades_to_observe_only() {
    // The plain scripted invoker never polls the recovery signal, so the
    // boundary check covers the over-ratio tail.
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![]);
    let mut lc = lifecycle("run-midturn-plain-degrade");
    let (invoke, _) = scripted_invoker(vec![continue_at(95, "over-ratio-plain"), verify_done()]);
    let summarizer = ScriptedSummarizer::new(handoff("boundary-done", "", "", "next"));
    let outcome = h
        .run(
            &task,
            &mut lc,
            &recovery_config(&dir, summarizer, 1),
            invoke,
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].outcome, Some(Outcome::Recovered));
    let recoveries = h
        .sink
        .of_kind("run-midturn-plain-degrade", "harness.context_recovery");
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0].payload["trigger"], "boundary");
}

// --- Steering ledger (spec 00025 FR-10) ----------------------------------------

/// An invoker standing in for the live watcher: claims the run's pending
/// commands and feeds each through the harness's `on_command_applied` seam.
fn steering_invoker(store: Arc<crate::store_memory::InMemoryStore>, run_id: &str) -> InvokeFn {
    let run_id = run_id.to_owned();
    Arc::new(move |request| {
        let store = store.clone();
        let run_id = run_id.clone();
        Box::pin(async move {
            let claimed = store.claim_commands(&run_id, Utc::now()).unwrap();
            let on_applied = request
                .observers
                .on_command_applied
                .as_ref()
                .expect("on_command_applied is threaded");
            for cmd in &claimed {
                on_applied(cmd, &crate::invoker_client::command_applied_event(cmd));
            }
            let result = verify_iteration();
            if let Some(on_message) = &request.observers.on_message {
                for msg in &result.messages {
                    on_message(msg);
                }
            }
            result
        })
    })
}

fn steering_events(
    store: &crate::store_memory::InMemoryStore,
    run_id: &str,
) -> Vec<crate::events::CommandApplied> {
    store
        .list_domain_events(run_id)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            DomainEventPayload::CommandApplied(c) => Some(c),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn applied_command_lands_in_ledger_and_clears_queue() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-steer");
    enqueue_say(
        h.store.as_ref(),
        "run-steer",
        "focus on graders",
        Utc::now(),
    )
    .unwrap();
    let outcome = h
        .run(
            &task,
            &mut lc,
            &HarnessConfig::default(),
            steering_invoker(h.store.clone(), "run-steer"),
        )
        .await;
    assert_eq!(outcome.lifecycle.status, Status::Done);
    let steering = steering_events(&h.store, "run-steer");
    assert_eq!(steering.len(), 1);
    assert_eq!(steering[0].command_kind, "say");
    assert_eq!(steering[0].command_payload["text"], "focus on graders");
    assert!(steering[0].command_id.is_some());
    // The applied queue row was deleted after the event committed.
    assert!(
        h.store
            .claim_commands("run-steer", Utc::now())
            .unwrap()
            .is_empty()
    );
    let mirrors = h.sink.domain_mirrors("run-steer");
    assert_eq!(
        mirrors
            .iter()
            .filter(|m| m.kind == "domain.command_applied")
            .count(),
        1
    );
    let folded = replay(&h.store.list_domain_events("run-steer").unwrap()).unwrap();
    assert_eq!(folded.status, outcome.lifecycle.status);
    assert_eq!(folded.version, outcome.lifecycle.version);
}

#[tokio::test]
async fn applied_command_ledgered_with_hang_watchdog_enabled() {
    let h = Harness::new();
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-steer-wd");
    enqueue_say(
        h.store.as_ref(),
        "run-steer-wd",
        "focus on graders",
        Utc::now(),
    )
    .unwrap();
    let config = HarnessConfig {
        loop_guard: LoopGuardConfig {
            repeated_tool_failure_threshold: None,
            thrash_repeat_threshold: None,
            thrash_window: None,
            hang_timeout_seconds: Some(0.1),
        },
        ..HarnessConfig::default()
    };
    let outcome = h
        .run_with(
            &task,
            &mut lc,
            &config,
            steering_invoker(h.store.clone(), "run-steer-wd"),
            h.options().with_monotonic(Arc::new(|| 0.0)),
        )
        .await
        .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert_eq!(steering_events(&h.store, "run-steer-wd").len(), 1);
    assert!(
        h.store
            .claim_commands("run-steer-wd", Utc::now())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn append_failure_retains_queue_row() {
    let sink = ListSink::new();
    let failing = FailingStore::on_event(crate::events::DomainEventKind::CommandApplied);
    let task = task(vec![ok_command()]);
    let mut lc = lifecycle("run-steer-fail");
    failing
        .inner
        .enqueue_command(
            "run-steer-fail",
            "say",
            json!({"text": "steer"}).as_object().unwrap().clone(),
            Utc::now(),
        )
        .unwrap();
    let store_for_invoker = failing.clone();
    let invoke: InvokeFn = Arc::new(move |request| {
        let store = store_for_invoker.clone();
        Box::pin(async move {
            let claimed = store
                .inner
                .claim_commands("run-steer-fail", Utc::now())
                .unwrap();
            let on_applied = request.observers.on_command_applied.as_ref().unwrap();
            for cmd in &claimed {
                on_applied(cmd, &crate::invoker_client::command_applied_event(cmd));
            }
            verify_iteration()
        })
    });
    let store: Arc<dyn crate::store::HarnessStore> = failing.clone();
    let outcome = crate::harness::run_task(
        &task,
        &mut lc,
        store,
        &HarnessConfig::default(),
        invoke,
        crate::harness::RunOptions::default().with_sink(sink),
    )
    .await
    .unwrap();
    assert_eq!(outcome.lifecycle.status, Status::Done);
    assert!(steering_events(&failing.inner, "run-steer-fail").is_empty());
    // The claimed row is retained as the visible trace (claimed, not deleted).
    assert!(
        failing
            .inner
            .claim_commands("run-steer-fail", Utc::now())
            .unwrap()
            .is_empty()
    );
    let all = failing.inner.list_domain_events("run-steer-fail").unwrap();
    assert!(
        all.iter()
            .all(|e| !matches!(e.payload, DomainEventPayload::CommandApplied(_)))
    );
}
