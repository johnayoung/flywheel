//! The inner iteration loop: prompt -> invoke (under the hang watchdog, the
//! wall-clock deadline, and the shutdown signal) -> rollup -> loop guards
//! -> context-recovery trigger, plus the transient rate-limit re-invocation
//! and the session-limit fast-abort.

use super::state::{AttemptError, Run, object, wait_shutdown};
use super::telemetry::RunTelemetry;
use super::{HarnessConfig, InvokeFn, RecoveryState};
use crate::agents_invoke::{InvocationObservers, InvocationRequest};
use crate::deadline::run_with_deadline;
use crate::deadline_config::DeadlineClass;
use crate::envelope::{EnvelopeResult, Intent};
use crate::event_serde::format_timestamp;
use crate::faults::{FaultClass, classify_status, derive_session_limit_reset_utc};
use crate::grader_transcript::{
    MessageKind, TranscriptCounter, TranscriptObservation, USAGE_TOKEN_KEYS, message_kind,
    message_usage, total_tokens_from_usage_map,
};
use crate::invoker::{InvocationSignals, IterationResult, Usage};
use crate::invoker_client::{FAILURE_HARNESS_RECOVERY, FAILURE_OPERATOR_INTERRUPT, RecoverySignal};
use crate::lifecycle::Attempt;
use crate::loop_guard::{LoopGuard, LoopGuardVerdict};
use crate::prompt::{IterationInputs, build_iteration_prompt};
use crate::store::{AttemptStore, JsonObject};
use crate::task::{Grader, Task, TranscriptGrader};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Input-side context-occupancy keys (spec 00018 FR-1): the resent
/// conversation plus the cached prefix; `output_tokens` is excluded.
pub const OCCUPANCY_USAGE_KEYS: [&str; 3] = [
    "input_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
];

/// Mid-turn observe tiers (spec 00019 FR-3).
pub const CONTEXT_OBSERVE_TIERS: [f64; 3] = [0.5, 0.75, 0.9];

pub const CAPACITY_SOURCE_SDK: &str = "sdk";
pub const CAPACITY_SOURCE_OPERATOR: &str = "operator";
pub const RECOVERY_TRIGGER_BOUNDARY: &str = "boundary";
pub const RECOVERY_TRIGGER_MID_TURN: &str = "mid_turn";

/// Hang watchdog tick bounds (seconds): `min(hang_timeout / 4, 0.5)`
/// clamped to the floor.
pub const HANG_WATCHDOG_MIN_TICK_SECONDS: f64 = 0.01;
pub const HANG_WATCHDOG_MAX_TICK_SECONDS: f64 = 0.5;

/// The iteration crossed the context-recovery threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecoveryTrigger {
    pub occupancy_tokens: u64,
    pub transcript_tail: String,
    pub iteration_number: u32,
    pub trigger: &'static str,
}

/// What [`drive_iterations`] produced.
pub(super) struct Driven {
    pub result: Option<IterationResult>,
    pub iterations_run: u32,
    pub wall_seconds: f64,
    pub loop_guard_verdict: Option<LoopGuardVerdict>,
    pub recovery_trigger: Option<RecoveryTrigger>,
    pub transient_exhausted: bool,
    pub session_limit_reset: Option<DateTime<Utc>>,
}

/// A usage field as Python's `int(value)` would read it: integers, floats
/// (truncated), and numeric strings; anything else is absent.
fn usage_field(usage: &Value, key: &str) -> Option<i64> {
    match usage.get(key)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Sum the input-side token fields of an iteration's usage payload.
///
/// The latest iteration's input-side tokens *are* the current context sent
/// to the model; summing per-iteration deltas would double-count the
/// re-sent conversation. Missing fields contribute zero.
pub fn occupancy_from_usage(usage: &Usage) -> u64 {
    OCCUPANCY_USAGE_KEYS
        .iter()
        .filter_map(|key| usage.get(*key))
        .sum()
}

fn occupancy_from_value(usage: &Value) -> u64 {
    OCCUPANCY_USAGE_KEYS
        .iter()
        .filter_map(|key| usage_field(usage, key))
        .filter(|v| *v > 0)
        .map(|v| v as u64)
        .sum()
}

fn breakdown_from_value(usage: &Value) -> Usage {
    let mut out = Usage::new();
    for key in USAGE_TOKEN_KEYS {
        let value = usage_field(usage, key).unwrap_or(0).max(0) as u64;
        out.insert(key.to_owned(), value);
    }
    out
}

/// Aggregate the iteration's per-field token usage breakdown from its
/// recorded messages.
///
/// Mirrors [`build_observation`]'s `total_tokens` algorithm field by field
/// so the emitted `harness.iteration_completed` payload's `total_tokens`
/// equals `observation.total_tokens` for the same iteration: assistant
/// turns (and usage-only readings) add their fields; when a result message
/// reports a *larger* aggregate than the running breakdown, its breakdown
/// wins.
pub fn build_usage_breakdown(messages: &[Value]) -> Usage {
    let mut breakdown = Usage::new();
    for key in USAGE_TOKEN_KEYS {
        breakdown.insert(key.to_owned(), 0);
    }
    for msg in messages {
        let Some(usage) = message_usage(msg) else {
            continue;
        };
        match message_kind(msg) {
            MessageKind::Assistant | MessageKind::Usage => {
                for key in USAGE_TOKEN_KEYS {
                    let add = usage_field(usage, key).unwrap_or(0).max(0) as u64;
                    *breakdown.entry(key.to_owned()).or_insert(0) += add;
                }
            }
            MessageKind::Result => {
                let candidate = breakdown_from_value(usage);
                if total_tokens_from_usage_map(&candidate) > total_tokens_from_usage_map(&breakdown)
                {
                    breakdown = candidate;
                }
            }
            MessageKind::Other => {}
        }
    }
    breakdown
}

/// Aggregate the iteration's messages into a [`TranscriptObservation`],
/// recomputing the same totals the live [`TranscriptCounter`] tracked so the
/// validation-time grader and the hard-limit enforcer converge.
pub fn build_observation(messages: &[Value], wall_seconds: f64) -> TranscriptObservation {
    let mut counter = TranscriptCounter::start_with_clock(Arc::new(|| 0.0));
    for msg in messages {
        counter.observe(msg);
    }
    TranscriptObservation {
        turns: counter.turns,
        total_tokens: counter.total_tokens,
        wall_seconds,
    }
}

/// Text chunks of an assistant message: SDK-shaped `content[].text`
/// blocks, a normalized event's `payload.text`, or a top-level `text`.
fn assistant_text_chunks(msg: &Value) -> Vec<String> {
    let mut chunks = Vec::new();
    let content = msg
        .get("content")
        .or_else(|| msg.get("message").and_then(|m| m.get("content")));
    if let Some(Value::Array(blocks)) = content {
        for block in blocks {
            let is_text = block
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|t| t == "text");
            if is_text && let Some(text) = block.get("text").and_then(Value::as_str) {
                chunks.push(text.to_owned());
            }
        }
    }
    if let Some(text) = msg
        .get("payload")
        .and_then(|p| p.get("text"))
        .and_then(Value::as_str)
    {
        chunks.push(text.to_owned());
    } else if let Some(text) = msg.get("text").and_then(Value::as_str)
        && content.is_none()
    {
        chunks.push(text.to_owned());
    }
    chunks
}

/// Event payload describing the envelope kind.
pub(super) fn envelope_payload(envelope: &EnvelopeResult) -> JsonObject {
    object(match envelope {
        EnvelopeResult::Valid(v) => json!({
            "kind": "valid",
            "intent": v.intent.as_str(),
            "reason": v.reason,
        }),
        EnvelopeResult::Missing => json!({"kind": "missing"}),
        EnvelopeResult::Truncated { detail } => json!({"kind": "truncated", "detail": detail}),
        EnvelopeResult::Duplicate { count } => json!({"kind": "duplicate", "count": count}),
        EnvelopeResult::Malformed { reason, offending } => json!({
            "kind": "malformed",
            "reason": reason,
            "offending": offending,
        }),
    })
}

/// Render a protocol-failure envelope as a short error string.
pub(super) fn envelope_protocol_error(envelope: &EnvelopeResult) -> String {
    match envelope {
        EnvelopeResult::Missing => "protocol failure: missing iteration envelope".to_owned(),
        EnvelopeResult::Truncated { detail } => {
            format!("protocol failure: truncated envelope ({detail})")
        }
        EnvelopeResult::Duplicate { count } => {
            format!("protocol failure: duplicate envelope (count={count})")
        }
        EnvelopeResult::Malformed { reason, .. } => {
            format!("protocol failure: malformed envelope ({reason})")
        }
        EnvelopeResult::Valid(_) => "protocol failure: unrecognized envelope".to_owned(),
    }
}

/// A short reason when `signals` mark a transient rate limit: a
/// TRANSIENT-classified `api_error_status` or a `rejected` rate-limit
/// event. `None` when no transient signal is present.
fn transient_rate_limit_reason(signals: &InvocationSignals) -> Option<String> {
    if let Some(status) = signals.api_error_status
        && classify_status(status) == Some(FaultClass::Transient)
    {
        return Some(format!("api_error_status={status}"));
    }
    for event in &signals.rate_limit_events {
        let info = event.get("rate_limit_info")?;
        if info.get("status").and_then(Value::as_str) == Some("rejected") {
            let rl_type = info
                .get("rate_limit_type")
                .and_then(Value::as_str)
                .unwrap_or("None");
            return Some(format!("rate_limit_rejected:{rl_type}"));
        }
    }
    None
}

/// The transient reason when `result` is a rate-limited iteration. A valid
/// envelope of any intent is never transient (the agent produced actionable
/// output).
fn iteration_is_transient_rate_limit(result: &IterationResult) -> Option<String> {
    if result.envelope.as_valid().is_some() {
        return None;
    }
    transient_rate_limit_reason(&result.signals)
}

/// Per-task budget precedence over a policy-resolved deadline class:
/// `None` inherits, `0` is the explicit unbounded opt-out, a positive value
/// replaces the ceiling.
pub(super) fn override_ceiling(task_override: Option<f64>, resolved: Option<f64>) -> Option<f64> {
    match task_override {
        None => resolved,
        Some(value) if value > 0.0 => Some(value),
        Some(_) => None,
    }
}

/// Mid-turn occupancy state for one iteration (spec 00019).
#[derive(Default)]
struct IterationWatch {
    emitted_tiers: Vec<f64>,
    latest_sdk_reading: Option<Value>,
    accumulated_estimate: u64,
    midturn_armed: bool,
    midturn_occupancy: u64,
    partial_transcript: String,
}

/// The threshold checker shared by the per-message observer and the SDK
/// context-usage observer.
struct ThresholdChecker {
    watch: Mutex<IterationWatch>,
    telemetry: Arc<RunTelemetry>,
    context_window_tokens: Option<u64>,
    trigger_ratio: f64,
    max_recoveries: u32,
    recoveries_used: Arc<std::sync::atomic::AtomicU32>,
    signal: RecoverySignal,
    attempt_number: u32,
    iteration_number: u32,
}

impl ThresholdChecker {
    fn lock(&self) -> std::sync::MutexGuard<'_, IterationWatch> {
        self.watch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Emit `harness.context_threshold_crossed` for newly-crossed tiers and
    /// arm mid-turn recovery when the act ratio is crossed with budget
    /// remaining.
    ///
    /// Capacity: the SDK reading's `maxTokens` wins when positive, else the
    /// operator knob; neither leaves the iteration inert. Occupancy: the
    /// SDK reading's `totalTokens` wins, else the accumulated per-message
    /// estimate.
    fn check(&self) {
        let mut watch = self.lock();
        let mut capacity: Option<u64> = None;
        let mut source = "";
        let mut occupancy = watch.accumulated_estimate;
        if let Some(reading) = &watch.latest_sdk_reading {
            if let Some(max) = reading.get("maxTokens").and_then(Value::as_u64)
                && max > 0
            {
                capacity = Some(max);
                source = CAPACITY_SOURCE_SDK;
            }
            if let Some(total) = reading.get("totalTokens").and_then(Value::as_u64) {
                occupancy = total;
            }
        }
        if capacity.is_none() {
            match self.context_window_tokens {
                Some(knob) => {
                    capacity = Some(knob);
                    source = CAPACITY_SOURCE_OPERATOR;
                }
                None => return,
            }
        }
        let Some(capacity) = capacity.filter(|c| *c > 0) else {
            return;
        };
        let ratio = occupancy as f64 / capacity as f64;
        for tier in CONTEXT_OBSERVE_TIERS {
            if watch.emitted_tiers.contains(&tier) || ratio < tier {
                continue;
            }
            watch.emitted_tiers.push(tier);
            self.telemetry.emit(
                "harness.context_threshold_crossed",
                object(json!({
                    "iteration": self.iteration_number,
                    "tier": tier,
                    "occupancy_tokens": occupancy,
                    "capacity_tokens": capacity,
                    "percentage": ratio * 100.0,
                    "capacity_source": source,
                })),
                Some(self.attempt_number),
                Some(self.iteration_number),
            );
        }
        if !watch.midturn_armed
            && ratio >= self.trigger_ratio
            && self.recoveries_used.load(Ordering::SeqCst) < self.max_recoveries
        {
            watch.midturn_armed = true;
            watch.midturn_occupancy = occupancy;
            self.signal.request();
        }
    }

    fn on_message(&self, msg: &Value) {
        self.telemetry
            .sdk_message(msg, self.attempt_number, self.iteration_number);
        let kind = message_kind(msg);
        let mut touched = false;
        {
            let mut watch = self.lock();
            if matches!(kind, MessageKind::Assistant) {
                for chunk in assistant_text_chunks(msg) {
                    watch.partial_transcript.push_str(&chunk);
                }
            }
            if matches!(kind, MessageKind::Assistant | MessageKind::Usage)
                && let Some(usage) = message_usage(msg)
            {
                watch.accumulated_estimate = occupancy_from_value(usage);
                touched = true;
            }
        }
        if touched {
            self.check();
        }
    }

    fn on_context_reading(&self, reading: &Value) {
        self.lock().latest_sdk_reading = Some(reading.clone());
        self.check();
    }
}

/// Race `invoker(request)` against the inter-message hang watchdog.
///
/// Any observed message resets the timer (the watchdog wraps the
/// per-message observer, so a rate-limit notice or a thinking block counts
/// as liveness). On silence past `hang_timeout` the invocation is dropped
/// (cancelled), `harness.hang_detected` is emitted, and
/// [`AttemptError::Hang`] returned. A completed invocation always wins the
/// race.
async fn invoke_with_watchdog(
    run: &Run,
    invoker: &InvokeFn,
    mut request: InvocationRequest,
    hang_timeout: f64,
    attempt_number: u32,
    iteration_number: u32,
) -> Result<IterationResult, AttemptError> {
    let last_activity = Arc::new(Mutex::new(run.monotonic()));
    let base = request.observers.on_message.take();
    let mclock = run.mclock.clone();
    let heartbeat = last_activity.clone();
    request.observers.on_message = Some(Arc::new(move |msg: &Value| {
        if let Ok(mut slot) = heartbeat.lock() {
            *slot = mclock();
        }
        if let Some(base) = &base {
            base(msg);
        }
    }));
    let tick = (hang_timeout / 4.0).clamp(
        HANG_WATCHDOG_MIN_TICK_SECONDS,
        HANG_WATCHDOG_MAX_TICK_SECONDS,
    );
    let mut invocation = invoker(request);
    loop {
        tokio::select! {
            biased;
            result = &mut invocation => return Ok(result),
            _ = tokio::time::sleep(Duration::from_secs_f64(tick)) => {
                let last = last_activity.lock().map(|s| *s).unwrap_or(0.0);
                let silence = run.monotonic() - last;
                if silence > hang_timeout {
                    drop(invocation);
                    run.telemetry.emit(
                        "harness.hang_detected",
                        object(json!({
                            "iteration": iteration_number,
                            "hang_timeout_seconds": hang_timeout,
                            "silence_seconds": silence,
                        })),
                        Some(attempt_number),
                        None,
                    );
                    return Err(AttemptError::Hang {
                        timeout_seconds: hang_timeout,
                        silence_seconds: silence,
                    });
                }
            }
        }
    }
}

/// One bounded invocation: the hang watchdog (when enabled) inside the
/// wall-clock agent-iteration deadline (when bounded), the whole thing
/// raced against the operator shutdown signal.
async fn run_invocation(
    run: &Run,
    invoker: &InvokeFn,
    request: InvocationRequest,
    hang_timeout: Option<f64>,
    agent_ceiling: Option<f64>,
    attempt_number: u32,
    iteration_number: u32,
) -> Result<IterationResult, AttemptError> {
    if run
        .shutdown
        .as_ref()
        .is_some_and(super::ShutdownSignal::is_triggered)
    {
        return Err(AttemptError::Interrupted);
    }
    let guarded = async {
        match hang_timeout {
            Some(timeout) if timeout > 0.0 => {
                invoke_with_watchdog(
                    run,
                    invoker,
                    request,
                    timeout,
                    attempt_number,
                    iteration_number,
                )
                .await
            }
            _ => Ok(invoker(request).await),
        }
    };
    let bounded = async {
        match agent_ceiling {
            None => guarded.await,
            Some(ceiling) => {
                match run_with_deadline(guarded, Duration::from_secs_f64(ceiling)).await {
                    Ok(outcome) => outcome,
                    Err(exceeded) => {
                        run.telemetry.emit(
                            "harness.deadline_exceeded",
                            object(json!({
                                "iteration": iteration_number,
                                "deadline_class": DeadlineClass::AgentIteration.as_str(),
                                "ceiling_seconds": ceiling,
                                "elapsed_seconds": exceeded.elapsed_seconds(),
                            })),
                            Some(attempt_number),
                            None,
                        );
                        Err(AttemptError::Deadline {
                            ceiling_seconds: ceiling,
                            elapsed_seconds: exceeded.elapsed_seconds(),
                        })
                    }
                }
            }
        }
    };
    tokio::select! {
        biased;
        outcome = bounded => outcome,
        _ = wait_shutdown(run.shutdown.as_ref()) => Err(AttemptError::Interrupted),
    }
}

/// Build `IterationInputs.prior_rubric_findings` from the most recent prior
/// attempt carrying failing rubric records (ordinal order).
fn collect_prior_findings(
    run: &Run,
    current_attempt_number: u32,
    grader_type: crate::task::GraderType,
) -> Result<Vec<(String, u32, String, u32)>, AttemptError> {
    use crate::store::GraderResultStore;
    if current_attempt_number <= 1 {
        return Ok(Vec::new());
    }
    for prior in (1..current_attempt_number).rev() {
        let rows = run.store.list_grader_results(&run.run_id, prior)?;
        let mut failing: Vec<_> = rows
            .into_iter()
            .filter(|r| r.grader_type == grader_type && !r.passed)
            .collect();
        if failing.is_empty() {
            continue;
        }
        failing.sort_by_key(|r| r.ordinal);
        return Ok(failing
            .into_iter()
            .map(|row| {
                let summary = row
                    .payload
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                (
                    row.grader_name
                        .clone()
                        .unwrap_or_else(|| "<unnamed>".to_owned()),
                    row.attempt_number,
                    summary,
                    row.ordinal,
                )
            })
            .collect());
    }
    Ok(Vec::new())
}

/// Run iterations until a non-`continue` envelope, a crash, a loop-guard
/// verdict, a context-recovery trigger, or the cap.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_iterations(
    run: &Run,
    task: &Task,
    config: &HarnessConfig,
    invoker: &InvokeFn,
    attempt: &mut Attempt,
    transcript_graders: &[TranscriptGrader],
    loop_guard: &mut LoopGuard,
    recovery_state: &mut RecoveryState,
) -> Result<Driven, AttemptError> {
    use crate::prompt::{ManualFinding, RubricFindings};
    use crate::task::GraderType;

    let attempt_number = attempt.number;
    let mut driven = Driven {
        result: None,
        iterations_run: 0,
        wall_seconds: 0.0,
        loop_guard_verdict: None,
        recovery_trigger: None,
        transient_exhausted: false,
        session_limit_reset: None,
    };
    let started_monotonic = run.monotonic();

    let prior_rubric_findings: Vec<RubricFindings> =
        collect_prior_findings(run, attempt_number, GraderType::Rubric)?
            .into_iter()
            .map(|(name, number, summary, ordinal)| {
                RubricFindings::new(name, number, summary).with_ordinal(ordinal)
            })
            .collect();
    let prior_manual_findings: Vec<ManualFinding> =
        collect_prior_findings(run, attempt_number, GraderType::Manual)?
            .into_iter()
            .map(|(name, number, summary, ordinal)| {
                ManualFinding::new(name, number, summary).with_ordinal(ordinal)
            })
            .collect();
    // Consume any pending handoff exactly once: the recovery attempt gets
    // it on its first iteration prompt only.
    let pending_handoff = recovery_state.pending_handoff.take();

    let steering_run = Arc::new(SteeringRun {
        store: run.store.clone(),
        lifecycle: run.lifecycle.clone(),
        run_id: run.run_id.clone(),
        clock: run.clock.clone(),
    });

    let mut iteration_number: u32 = 0;
    while iteration_number < config.max_iterations_per_attempt {
        iteration_number += 1;
        driven.iterations_run = iteration_number;

        let prompt = build_iteration_prompt(
            task,
            &run.snapshot(),
            &IterationInputs {
                max_retries: config.max_retries,
                prior_rubric_findings: prior_rubric_findings.clone(),
                prior_manual_findings: prior_manual_findings.clone(),
                recovery_handoff: if iteration_number == 1 {
                    pending_handoff.clone()
                } else {
                    None
                },
            },
        );

        let recovery_signal = RecoverySignal::new();
        let checker = Arc::new(ThresholdChecker {
            watch: Mutex::new(IterationWatch::default()),
            telemetry: run.telemetry.clone(),
            context_window_tokens: config.context_window_tokens,
            trigger_ratio: config.context_recovery_trigger_ratio,
            max_recoveries: config.max_context_recoveries,
            recoveries_used: recovery_state.recoveries_used.clone(),
            signal: recovery_signal.clone(),
            attempt_number,
            iteration_number,
        });
        let on_message = {
            let checker = checker.clone();
            Arc::new(move |msg: &Value| checker.on_message(msg))
        };
        let context_observer = {
            let checker = checker.clone();
            Arc::new(move |reading: &Value| checker.on_context_reading(reading))
        };
        let on_command_applied = {
            let steering = steering_run.clone();
            Arc::new(
                move |command: &crate::store::ControlCommandRecord,
                      fact: &crate::events::CommandApplied| {
                    steering.ledger(command, fact, Some(attempt_number));
                },
            )
        };

        let agent_ceiling = override_ceiling(
            task.budgets.agent_iteration_seconds.map(|s| s.as_f64()),
            config.deadlines.for_class(DeadlineClass::AgentIteration),
        );
        let request = InvocationRequest {
            prompt,
            transcript_graders: transcript_graders.to_vec(),
            attempt_number,
            iteration_number,
            checkpoint_nudge_seconds: Some(config.checkpoint_nudge_seconds),
            agent_iteration_ceiling_seconds: agent_ceiling,
            resume_session_id: None,
            observers: InvocationObservers {
                on_message: Some(on_message),
                context_observer: Some(context_observer),
                recovery_interrupt: Some(recovery_signal.clone()),
                on_command_applied: Some(on_command_applied),
                checkpoint_progress_probe: config.checkpoint_progress_probe.clone(),
            },
        };
        let hang_timeout = config.loop_guard.hang_timeout_seconds;

        // Transient rate-limit retry: re-invoke the SAME iteration on the
        // separate bounded budget, backing off between tries.
        let mut transient_retries: u32 = 0;
        let mut iteration_result: Option<IterationResult> = None;
        let mut recovery_requested = false;
        loop {
            let result = run_invocation(
                run,
                invoker,
                request.clone(),
                hang_timeout,
                agent_ceiling,
                attempt_number,
                iteration_number,
            )
            .await?;
            if let Some(failure) = &result.failure {
                if failure.error_type == FAILURE_HARNESS_RECOVERY {
                    // Mid-turn recovery (spec 00019 FR-4): the invoker
                    // honored the armed recovery signal. Capture the
                    // work-so-far and the occupancy that armed it.
                    let watch = checker.lock();
                    driven.recovery_trigger = Some(RecoveryTrigger {
                        occupancy_tokens: watch.midturn_occupancy,
                        transcript_tail: watch.partial_transcript.clone(),
                        iteration_number,
                        trigger: RECOVERY_TRIGGER_MID_TURN,
                    });
                    driven.wall_seconds = run.monotonic() - started_monotonic;
                    recovery_requested = true;
                    break;
                }
                if failure.error_type == FAILURE_OPERATOR_INTERRUPT {
                    // The store-routed operator interrupt is the twin of
                    // SIGINT/SIGTERM: route through the interrupt path.
                    return Err(AttemptError::Interrupted);
                }
            }

            let transient_reason = iteration_is_transient_rate_limit(&result);
            // Session-limit fast-abort: a refusal whose reset is derivable
            // AND still in the future is a hard block until that instant.
            let session_now = run.now();
            let derived = if result.envelope.as_valid().is_some() {
                None
            } else {
                derive_session_limit_reset_utc(
                    &result.signals.rate_limit_events,
                    &result.transcript,
                    session_now,
                )
            };
            if let Some(derived) = derived
                && derived.reset_at > session_now
            {
                driven.session_limit_reset = Some(derived.reset_at);
                driven.transient_exhausted = true;
                run.telemetry.emit(
                    "harness.session_limit",
                    object(json!({
                        "iteration": iteration_number,
                        "reason": transient_reason.clone().unwrap_or_else(|| "session_limit_refusal".to_owned()),
                        "reset_at": format_timestamp(&derived.reset_at),
                        "source": derived.source.as_str(),
                        "classification": FaultClass::Transient.as_str(),
                    })),
                    Some(attempt_number),
                    Some(iteration_number),
                );
                iteration_result = Some(result);
                break;
            }
            let Some(transient_reason) = transient_reason else {
                iteration_result = Some(result);
                break;
            };
            if transient_retries >= config.max_transient_retries {
                driven.transient_exhausted = true;
                run.telemetry.emit(
                    "harness.transient_exhausted",
                    object(json!({
                        "iteration": iteration_number,
                        "reason": transient_reason,
                        "invocations": transient_retries + 1,
                        "max_transient_retries": config.max_transient_retries,
                        "classification": FaultClass::Transient.as_str(),
                    })),
                    Some(attempt_number),
                    Some(iteration_number),
                );
                iteration_result = Some(result);
                break;
            }
            let delay = config.transient_backoff.delay_for(transient_retries);
            transient_retries += 1;
            run.telemetry.emit(
                "harness.transient_retry",
                object(json!({
                    "iteration": iteration_number,
                    "reason": transient_reason,
                    "retry": transient_retries,
                    "max_transient_retries": config.max_transient_retries,
                    "delay_seconds": delay,
                    "classification": FaultClass::Transient.as_str(),
                })),
                Some(attempt_number),
                Some(iteration_number),
            );
            match &config.transient_sleep {
                Some(sleep) => sleep(delay).await,
                None => tokio::time::sleep(Duration::from_secs_f64(delay)).await,
            }
        }

        if recovery_requested {
            driven.result = None;
            break;
        }
        let result = iteration_result.expect("every non-recovery exit assigned a result");
        driven.wall_seconds = run.monotonic() - started_monotonic;

        // Context-pressure telemetry: token fields are per-iteration deltas;
        // cost / turns are forwarded verbatim (session-cumulative).
        let usage_breakdown = match &result.usage {
            Some(usage) => {
                let mut out = Usage::new();
                for key in USAGE_TOKEN_KEYS {
                    out.insert(key.to_owned(), usage.get(key).copied().unwrap_or(0));
                }
                out
            }
            None => build_usage_breakdown(&result.messages),
        };
        let mut usage_payload = JsonObject::new();
        for key in USAGE_TOKEN_KEYS {
            usage_payload.insert(
                key.to_owned(),
                json!(usage_breakdown.get(key).copied().unwrap_or(0)),
            );
        }
        usage_payload.insert(
            "total_tokens".into(),
            json!(total_tokens_from_usage_map(&usage_breakdown)),
        );
        run.telemetry.emit(
            "harness.iteration_completed",
            object(json!({
                "iteration": iteration_number,
                "envelope": envelope_payload(&result.envelope),
                "failure": result.failure.as_ref().map(|f| json!({
                    "error_type": f.error_type,
                    "message": f.message,
                })),
                "stop_reason": result.signals.stop_reason,
                "rate_limited": !result.signals.rate_limit_events.is_empty(),
                "usage": usage_payload,
                "total_cost_usd": result.signals.total_cost_usd,
                "num_turns": result.signals.num_turns,
            })),
            Some(attempt_number),
            None,
        );

        // Iteration-boundary aggregate rollup (spec 00025 FR-6) through the
        // versioned save_attempt path; a mismatch means another writer moved
        // the run on and this worker must not clobber it.
        let get = |key: &str| usage_breakdown.get(key).copied().unwrap_or(0);
        attempt.input_tokens += get("input_tokens");
        attempt.output_tokens += get("output_tokens");
        attempt.cache_creation_input_tokens += get("cache_creation_input_tokens");
        attempt.cache_read_input_tokens += get("cache_read_input_tokens");
        attempt.iterations_completed = u64::from(iteration_number);
        if let Some(turns) = result.signals.num_turns {
            attempt.turns += turns;
        }
        if let Some(cost) = result.signals.total_cost_usd {
            attempt.total_cost_usd += cost;
        }
        attempt.last_activity_at = Some(run.now());
        let version = run.with_lifecycle(|lc| lc.version);
        run.store
            .save_attempt(&run.run_id, attempt, Some(version))?;

        if result.failure.is_some() {
            driven.result = Some(result);
            break;
        }

        // Loop guards preempt the cap-reached and even the final-iteration
        // outcomes.
        driven.loop_guard_verdict = loop_guard.observe(&result.signals.tool_interactions);
        if driven.loop_guard_verdict.is_some() {
            driven.result = Some(result);
            break;
        }

        // Boundary context-recovery trigger (spec 00018 FR-1), after the
        // loop-guard check so loop-guard precedence holds. Only a validly
        // continuing iteration recovers.
        let is_continue = matches!(
            &result.envelope,
            EnvelopeResult::Valid(v) if v.intent == Intent::Continue
        );
        if let Some(capacity) = config.context_window_tokens
            && recovery_state.recoveries_used.load(Ordering::SeqCst) < config.max_context_recoveries
            && is_continue
        {
            let occupancy = occupancy_from_usage(&usage_breakdown);
            let threshold = capacity as f64 * config.context_recovery_trigger_ratio;
            if occupancy as f64 >= threshold {
                driven.recovery_trigger = Some(RecoveryTrigger {
                    occupancy_tokens: occupancy,
                    transcript_tail: result.transcript.clone(),
                    iteration_number,
                    trigger: RECOVERY_TRIGGER_BOUNDARY,
                });
                driven.result = Some(result);
                break;
            }
        }

        driven.result = Some(result);
        if !is_continue {
            break;
        }
    }
    Ok(driven)
}

/// The slice of the run the steering-ledger observer needs: it appends the
/// `CommandApplied` fact against the shared lifecycle version from the
/// invoker's watcher task.
pub(super) struct SteeringRun {
    pub store: Arc<super::MirroringStore>,
    pub lifecycle: Arc<Mutex<crate::lifecycle::Lifecycle>>,
    pub run_id: String,
    pub clock: crate::invoker_client::Clock,
}

impl SteeringRun {
    /// Append the `CommandApplied` ledger fact, then delete the queue row.
    ///
    /// The application already happened, so an append failure retains the
    /// claimed queue row as the visible trace and surfaces on stderr rather
    /// than failing the caller; the delete is best-effort hygiene.
    pub fn ledger(
        &self,
        command: &crate::store::ControlCommandRecord,
        fact: &crate::events::CommandApplied,
        attempt_number: Option<u32>,
    ) {
        use crate::events::{DomainEvent, DomainEventPayload};
        use crate::store::{ControlCommandStore, DomainEventStore};
        let mut event = DomainEvent::new(
            self.run_id.clone(),
            (self.clock)(),
            DomainEventPayload::CommandApplied(fact.clone()),
        );
        event.attempt_number = attempt_number;
        {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match self.store.append_domain_event(&event, lifecycle.version) {
                Ok(folded) => {
                    lifecycle.replace_from(&folded);
                    lifecycle.attempts = folded.attempts;
                }
                Err(err) => {
                    eprintln!(
                        "flywheel: steering ledger append failed for command {:?} ({}): {}: {err}",
                        command.id,
                        command.kind,
                        super::store_error_type(&err)
                    );
                    return;
                }
            }
        }
        let Some(id) = command.id else {
            return;
        };
        if let Err(err) = self.store.delete_command(id) {
            eprintln!(
                "flywheel: applied control command row {id:?} could not be deleted: {}: {err}",
                super::store_error_type(&err)
            );
        }
    }
}

/// Non-command graders are skipped; the transcript graders of `task` in
/// list order.
pub(super) fn transcript_graders(task: &Task) -> Vec<TranscriptGrader> {
    task.graders
        .iter()
        .filter_map(|g| match g {
            Grader::Transcript(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}
