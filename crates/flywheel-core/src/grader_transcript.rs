//! Transcript grader runner + hard-limit enforcement.
//!
//! A [`TranscriptGrader`] on a [`Task`] carries two responsibilities that
//! this module covers from a single source of truth:
//!
//! 1. **Hard limit enforcement during a run.** When `max_turns`,
//!    `max_total_tokens`, or `max_wall_seconds` is set on a transcript
//!    grader, the harness wraps the agent's message stream with
//!    [`enforce_transcript_limits`] and the stream ends as soon as the first
//!    limit is breached -- matching `docs/loop.md`'s requirement that
//!    `max_turns` halts the agent before the grader stage is reached.
//!
//! 2. **Validation-time grading.** [`run_transcript_graders`] evaluates
//!    every transcript grader against the observed totals and persists one
//!    row per grader to `grader_results` with the documented payload shape:
//!    `{"observed": {turns, total_tokens, wall_seconds}, "breached": "max_..."}`.
//!    The runner reads the same grader spec the hard-limit enforcer used, so
//!    the abort and the grading row agree on the breached field.
//!
//! Per `docs/task-schema.md` cost-order, transcript graders only run after
//! command graders pass -- [`run_transcript_graders`] takes an explicit
//! `command_passed` flag for the harness to wire that in.
//!
//! Messages are the opaque `serde_json::Value` shapes the invoker carries
//! (`IterationResult::messages`): the serialized SDK message with its
//! `message_type` discriminator (`"AssistantMessage"` / `"ResultMessage"`),
//! the agent CLI's stream-json form (`"type": "assistant"` / `"result"`,
//! usage nested under `message`), or the normalized `flywheel-agents` event
//! the agents-backed invoker records (`"type": "message.assistant"` /
//! `"tool.started"` / `"context.usage"` / `"session.finished"`, counters
//! nested under `payload`).

use crate::invoker::Usage;
use crate::store::{GraderResultRecord, GraderResultStore, JsonObject, StoreError};
use crate::task::{Grader, GraderType, Task, TranscriptGrader};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// The transcript limit a run breached. Serializes as the grader field name
/// (`max_turns`, `max_total_tokens`, `max_wall_seconds`), which is what the
/// persisted `breached` payload key carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreachedField {
    MaxTurns,
    MaxTotalTokens,
    MaxWallSeconds,
}

impl BreachedField {
    pub const fn as_str(self) -> &'static str {
        match self {
            BreachedField::MaxTurns => "max_turns",
            BreachedField::MaxTotalTokens => "max_total_tokens",
            BreachedField::MaxWallSeconds => "max_wall_seconds",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "max_turns" => Some(BreachedField::MaxTurns),
            "max_total_tokens" => Some(BreachedField::MaxTotalTokens),
            "max_wall_seconds" => Some(BreachedField::MaxWallSeconds),
            _ => None,
        }
    }
}

impl fmt::Display for BreachedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Canonical evaluation order. Both hard-limit enforcement and
/// validation-time grading evaluate fields in this order so multi-field
/// graders converge on the same "first breach" verdict regardless of which
/// path observed it first.
pub const LIMIT_ORDER: [BreachedField; 3] = [
    BreachedField::MaxTurns,
    BreachedField::MaxTotalTokens,
    BreachedField::MaxWallSeconds,
];

/// Usage keys that contribute to billable token totals. Every billed
/// category -- input, output, cache-creation, cache-read -- counts against
/// `max_total_tokens`.
pub const USAGE_TOKEN_KEYS: [&str; 4] = [
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
];

/// Sum the documented token fields from a JSON `usage` object.
///
/// Missing, non-object, or non-numeric fields contribute zero; a numeric
/// string is accepted (as Python's `int()` would). The result never goes
/// below zero.
pub fn total_tokens_from_usage(usage: Option<&Value>) -> u64 {
    let Some(Value::Object(usage)) = usage else {
        return 0;
    };
    let total: i64 = USAGE_TOKEN_KEYS
        .iter()
        .filter_map(|key| usage.get(*key))
        .filter_map(|value| match value {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        })
        .sum();
    u64::try_from(total).unwrap_or(0)
}

/// Sum the documented token fields from an invoker [`Usage`] breakdown.
pub fn total_tokens_from_usage_map(usage: &Usage) -> u64 {
    USAGE_TOKEN_KEYS
        .iter()
        .filter_map(|key| usage.get(*key))
        .sum()
}

/// One iteration's observed transcript totals.
///
/// The harness produces this from the iteration's signals plus a monotonic
/// wall clock; the grader runner consumes it. `wall_seconds` is monotonic so
/// a system clock change during a run cannot flip the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TranscriptObservation {
    pub turns: u64,
    pub total_tokens: u64,
    pub wall_seconds: f64,
}

/// A monotonic clock reading seconds, injectable so tests advance wall time
/// deterministically without sleeping.
pub type MonotonicClock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Process-wide monotonic seconds, the analogue of `time.monotonic()`.
pub fn monotonic_seconds() -> f64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn default_clock() -> MonotonicClock {
    Arc::new(monotonic_seconds)
}

/// How a recorded message counts toward the transcript totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageKind {
    /// One agent turn (an SDK `AssistantMessage`, a stream-json `assistant`
    /// line, or a normalized `message.assistant` / `tool.started` event).
    Assistant,
    /// The terminal envelope carrying cumulative totals.
    Result,
    /// A usage-only reading (`context.usage`): the claude-code normalizer
    /// reports each assistant line's token counters on this event rather
    /// than on the block events, so it adds tokens without adding a turn.
    Usage,
    Other,
}

pub(crate) fn message_kind(msg: &Value) -> MessageKind {
    match msg.get("message_type").and_then(Value::as_str) {
        Some("AssistantMessage") => return MessageKind::Assistant,
        Some("ResultMessage") => return MessageKind::Result,
        Some(_) => return MessageKind::Other,
        None => {}
    }
    match msg.get("type").and_then(Value::as_str) {
        Some("assistant" | "message.assistant" | "tool.started") => MessageKind::Assistant,
        Some("result" | "session.finished") => MessageKind::Result,
        Some("context.usage") => MessageKind::Usage,
        _ => MessageKind::Other,
    }
}

pub(crate) fn message_usage(msg: &Value) -> Option<&Value> {
    msg.get("usage")
        .filter(|u| u.is_object())
        .or_else(|| msg.get("message").and_then(|m| m.get("usage")))
        .or_else(|| msg.get("payload").and_then(|p| p.get("usage")))
        .filter(|u| u.is_object())
}

fn message_num_turns(msg: &Value) -> Option<u64> {
    msg.get("num_turns")
        .or_else(|| msg.get("payload").and_then(|p| p.get("num_turns")))
        .and_then(Value::as_u64)
}

/// Stateful accumulator that observes agent messages as they stream.
///
/// Used by [`enforce_transcript_limits`] to track running totals, and
/// reusable by the harness to compute the final [`TranscriptObservation`]
/// once the stream has been drained.
pub struct TranscriptCounter {
    pub turns: u64,
    pub total_tokens: u64,
    started_at_monotonic: f64,
    clock: MonotonicClock,
}

impl fmt::Debug for TranscriptCounter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptCounter")
            .field("turns", &self.turns)
            .field("total_tokens", &self.total_tokens)
            .field("started_at_monotonic", &self.started_at_monotonic)
            .finish_non_exhaustive()
    }
}

impl Default for TranscriptCounter {
    fn default() -> Self {
        Self::start()
    }
}

impl TranscriptCounter {
    /// Start counting now on the process monotonic clock.
    pub fn start() -> Self {
        Self::start_with_clock(default_clock())
    }

    /// Start counting now on an injected monotonic clock.
    pub fn start_with_clock(clock: MonotonicClock) -> Self {
        let started_at_monotonic = clock();
        Self {
            turns: 0,
            total_tokens: 0,
            started_at_monotonic,
            clock,
        }
    }

    pub fn started_at_monotonic(&self) -> f64 {
        self.started_at_monotonic
    }

    /// Fold one streamed message into the running totals.
    ///
    /// An assistant message counts one turn and adds its usage. A result
    /// message may report cumulative totals, so its usage and `num_turns`
    /// are taken as a maximum -- the count is never lost when per-message
    /// usage was absent but the final result reports the run's totals.
    pub fn observe(&mut self, msg: &Value) {
        match message_kind(msg) {
            MessageKind::Assistant => {
                self.turns += 1;
                self.total_tokens += total_tokens_from_usage(message_usage(msg));
            }
            MessageKind::Result => {
                if let Some(usage) = message_usage(msg) {
                    self.total_tokens = self.total_tokens.max(total_tokens_from_usage(Some(usage)));
                }
                if let Some(num_turns) = message_num_turns(msg) {
                    self.turns = self.turns.max(num_turns);
                }
            }
            MessageKind::Usage => {
                self.total_tokens += total_tokens_from_usage(message_usage(msg));
            }
            MessageKind::Other => {}
        }
    }

    pub fn snapshot(&self) -> TranscriptObservation {
        TranscriptObservation {
            turns: self.turns,
            total_tokens: self.total_tokens,
            wall_seconds: (self.clock)() - self.started_at_monotonic,
        }
    }
}

/// Return the first field of `grader` strictly exceeded by `observed`, or
/// `None` if every limit is respected.
///
/// "First" is canonical order ([`LIMIT_ORDER`]) so hard-limit aborts and
/// validation-time grading converge on the same breached field even when
/// multiple limits are set on one grader. Limits are inclusive: a run that
/// ends exactly at the limit is a pass; only strictly exceeding triggers a
/// breach.
pub fn first_breach(
    grader: &TranscriptGrader,
    observed: &TranscriptObservation,
) -> Option<BreachedField> {
    LIMIT_ORDER.into_iter().find(|field| match field {
        BreachedField::MaxTurns => grader.max_turns.is_some_and(|limit| observed.turns > limit),
        BreachedField::MaxTotalTokens => grader
            .max_total_tokens
            .is_some_and(|limit| observed.total_tokens > limit),
        BreachedField::MaxWallSeconds => grader
            .max_wall_seconds
            .is_some_and(|limit| observed.wall_seconds > limit.as_f64()),
    })
}

/// What [`enforce_transcript_limits`] saw by the time the stream ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TranscriptLimitOutcome {
    /// Totals observed over every forwarded message.
    pub observation: TranscriptObservation,
    /// The breach that ended the stream early, or `None` when the source
    /// was drained to completion.
    pub breached: Option<BreachedField>,
}

/// Forward messages from `next` into `sink` until any grader's first limit
/// is breached, then stop the stream cleanly.
///
/// Each forwarded message updates a [`TranscriptCounter`]; after every
/// forward the snapshot is compared against every grader's
/// [`first_breach`]. When the first breach is observed the wrapper returns
/// -- the consumer sees a clean end-of-stream and proceeds to envelope
/// parsing. The grader runner, given the same observed totals, will then
/// record the breach on the persisted row, so the abort and the row agree.
///
/// An empty `graders` slice makes this a transparent pass-through. `clock`
/// defaults to the process monotonic clock when `None`.
pub async fn enforce_transcript_limits<S, K>(
    mut next: S,
    mut sink: K,
    graders: &[TranscriptGrader],
    clock: Option<MonotonicClock>,
) -> TranscriptLimitOutcome
where
    S: AsyncFnMut() -> Option<Value>,
    K: FnMut(Value),
{
    let mut counter = TranscriptCounter::start_with_clock(clock.unwrap_or_else(default_clock));
    while let Some(msg) = next().await {
        counter.observe(&msg);
        sink(msg);
        if graders.is_empty() {
            continue;
        }
        let snapshot = counter.snapshot();
        if let Some(breached) = graders.iter().find_map(|g| first_breach(g, &snapshot)) {
            return TranscriptLimitOutcome {
                observation: snapshot,
                breached: Some(breached),
            };
        }
    }
    TranscriptLimitOutcome {
        observation: counter.snapshot(),
        breached: None,
    }
}

/// Snapshot the grader object as it appeared in the task at run time.
pub fn grader_spec_snapshot(grader: &TranscriptGrader) -> JsonObject {
    let mut spec = JsonObject::new();
    spec.insert("type".into(), json!("transcript"));
    if let Some(v) = grader.max_turns {
        spec.insert("max_turns".into(), json!(v));
    }
    if let Some(v) = grader.max_total_tokens {
        spec.insert("max_total_tokens".into(), json!(v));
    }
    if let Some(v) = grader.max_wall_seconds {
        spec.insert("max_wall_seconds".into(), Value::from(v));
    }
    if let Some(name) = &grader.name {
        spec.insert("name".into(), json!(name));
    }
    spec
}

/// Build the documented transcript payload:
/// `{"observed": {turns, total_tokens, wall_seconds}, "breached"?: "max_..."}`.
/// `breached` is omitted when the grader passed.
pub fn transcript_payload(
    observation: &TranscriptObservation,
    breached: Option<BreachedField>,
) -> JsonObject {
    let mut payload = JsonObject::new();
    payload.insert(
        "observed".into(),
        json!({
            "turns": observation.turns,
            "total_tokens": observation.total_tokens,
            "wall_seconds": observation.wall_seconds,
        }),
    );
    if let Some(field) = breached {
        payload.insert("breached".into(), json!(field.as_str()));
    }
    payload
}

/// Assemble (without persisting) the `grader_type="transcript"` record for
/// one grader at `ordinal`, grading `observation` against it.
pub fn build_transcript_result(
    grader: &TranscriptGrader,
    observation: &TranscriptObservation,
    run_id: &str,
    attempt_number: u32,
    ordinal: u32,
    ts: DateTime<Utc>,
) -> GraderResultRecord {
    let started = Instant::now();
    let breached = first_breach(grader, observation);
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    GraderResultRecord {
        run_id: run_id.to_owned(),
        attempt_number,
        ordinal,
        grader_type: GraderType::Transcript,
        grader_spec: grader_spec_snapshot(grader),
        passed: breached.is_none(),
        duration_ms,
        payload: transcript_payload(observation, breached),
        ts,
        grader_name: grader.name.clone(),
        id: None,
    }
}

/// Evaluate every [`TranscriptGrader`] on `task.graders` and persist one
/// `grader_results` row per execution in list order.
///
/// Cost-order is honored at two scales:
///
/// * Across types: when `command_passed` is `false` the runner returns an
///   empty list without persisting any row -- transcript graders are
///   skipped entirely when an earlier (cheaper) type failed.
/// * Within type: on the first failing transcript grader, later transcript
///   graders are not graded.
///
/// Non-transcript graders in `task.graders` are ignored without persisting
/// any row; `ordinal` is each grader's index in `task.graders`.
pub fn run_transcript_graders<S, F>(
    task: &Task,
    observation: &TranscriptObservation,
    store: &S,
    run_id: &str,
    attempt_number: u32,
    command_passed: bool,
    mut now: F,
) -> Result<Vec<GraderResultRecord>, StoreError>
where
    S: GraderResultStore + ?Sized,
    F: FnMut() -> DateTime<Utc>,
{
    if !command_passed {
        return Ok(Vec::new());
    }
    let mut persisted = Vec::new();
    for (index, grader) in task.graders.iter().enumerate() {
        let Grader::Transcript(grader) = grader else {
            continue;
        };
        let ordinal = u32::try_from(index).expect("grader list index fits in u32");
        let row =
            build_transcript_result(grader, observation, run_id, attempt_number, ordinal, now());
        let passed = row.passed;
        persisted.push(store.append_grader_result(&row)?);
        if !passed {
            break;
        }
    }
    Ok(persisted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{CommandGrader, ManualGrader, RubricGrader, Seconds};
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingStore {
        rows: Mutex<Vec<GraderResultRecord>>,
    }

    impl GraderResultStore for RecordingStore {
        fn append_grader_result(
            &self,
            result: &GraderResultRecord,
        ) -> Result<GraderResultRecord, StoreError> {
            let mut rows = self.rows.lock().unwrap();
            let mut stored = result.clone();
            stored.id = Some(rows.len() as i64 + 1);
            rows.push(stored.clone());
            Ok(stored)
        }

        fn list_grader_results(
            &self,
            run_id: &str,
            attempt_number: u32,
        ) -> Result<Vec<GraderResultRecord>, StoreError> {
            let mut rows: Vec<_> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.run_id == run_id && r.attempt_number == attempt_number)
                .cloned()
                .collect();
            rows.sort_by_key(|r| r.ordinal);
            Ok(rows)
        }
    }

    /// A monotonic clock advancing `step` per read, or manually.
    struct FakeClock {
        value: Mutex<f64>,
        step: f64,
    }

    impl FakeClock {
        fn new(start: f64, step: f64) -> Arc<Self> {
            Arc::new(Self {
                value: Mutex::new(start),
                step,
            })
        }

        fn advance(&self, delta: f64) {
            *self.value.lock().unwrap() += delta;
        }

        fn clock(self: &Arc<Self>) -> MonotonicClock {
            let this = Arc::clone(self);
            Arc::new(move || {
                let mut v = this.value.lock().unwrap();
                let current = *v;
                *v += this.step;
                current
            })
        }
    }

    fn assistant(usage: Option<Value>) -> Value {
        json!({
            "message_type": "AssistantMessage",
            "content": [{"text": "ok", "message_type": "TextBlock"}],
            "model": "claude-test",
            "stop_reason": "end_turn",
            "session_id": "sess-1",
            "usage": usage,
        })
    }

    fn result(num_turns: u64, usage: Option<Value>) -> Value {
        json!({
            "message_type": "ResultMessage",
            "subtype": "success",
            "duration_ms": 10,
            "is_error": false,
            "num_turns": num_turns,
            "session_id": "sess-1",
            "usage": usage,
        })
    }

    fn grader(
        max_turns: Option<u64>,
        max_total_tokens: Option<u64>,
        max_wall_seconds: Option<f64>,
    ) -> TranscriptGrader {
        TranscriptGrader {
            max_turns,
            max_total_tokens,
            max_wall_seconds: max_wall_seconds.map(Seconds::Float),
            name: None,
        }
    }

    fn obs(turns: u64, total_tokens: u64, wall_seconds: f64) -> TranscriptObservation {
        TranscriptObservation {
            turns,
            total_tokens,
            wall_seconds,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_780_000_000 + secs, 0).unwrap()
    }

    async fn drain(
        messages: Vec<Value>,
        graders: &[TranscriptGrader],
        clock: Option<MonotonicClock>,
    ) -> (Vec<Value>, TranscriptLimitOutcome) {
        let mut source = messages.into_iter();
        let mut out = Vec::new();
        let outcome =
            enforce_transcript_limits(async || source.next(), |m| out.push(m), graders, clock)
                .await;
        (out, outcome)
    }

    fn run(
        task: &Task,
        observation: &TranscriptObservation,
        store: &RecordingStore,
    ) -> Vec<GraderResultRecord> {
        run_transcript_graders(task, observation, store, "r1", 1, true, || at(0)).unwrap()
    }

    // --- total_tokens_from_usage ------------------------------------------

    #[test]
    fn sums_all_documented_token_fields() {
        let usage = json!({
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation_input_tokens": 5,
            "cache_read_input_tokens": 3,
        });
        assert_eq!(total_tokens_from_usage(Some(&usage)), 38);
    }

    #[test]
    fn treats_missing_fields_and_none_as_zero() {
        assert_eq!(
            total_tokens_from_usage(Some(&json!({"input_tokens": 7}))),
            7
        );
        assert_eq!(total_tokens_from_usage(Some(&json!({}))), 0);
        assert_eq!(total_tokens_from_usage(None), 0);
        assert_eq!(total_tokens_from_usage(Some(&Value::Null)), 0);
    }

    #[test]
    fn ignores_non_numeric_garbage_but_accepts_numeric_strings() {
        let usage =
            json!({"input_tokens": "junk", "output_tokens": 5, "cache_read_input_tokens": "4"});
        assert_eq!(total_tokens_from_usage(Some(&usage)), 9);
        assert_eq!(
            total_tokens_from_usage(Some(&json!({"input_tokens": 2.9}))),
            2
        );
        assert_eq!(
            total_tokens_from_usage(Some(&json!({"input_tokens": -50}))),
            0
        );
    }

    #[test]
    fn sums_invoker_usage_map() {
        let usage: Usage = [
            ("input_tokens", 10u64),
            ("output_tokens", 5),
            ("other", 100),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        assert_eq!(total_tokens_from_usage_map(&usage), 15);
    }

    // --- first_breach ------------------------------------------------------

    #[test]
    fn first_breach_none_when_within_every_limit() {
        let g = grader(Some(10), Some(1000), Some(60.0));
        assert_eq!(first_breach(&g, &obs(5, 500, 30.0)), None);
    }

    #[test]
    fn first_breach_each_field_alone() {
        assert_eq!(
            first_breach(&grader(Some(2), None, None), &obs(3, 0, 0.0)),
            Some(BreachedField::MaxTurns)
        );
        assert_eq!(
            first_breach(&grader(None, Some(100), None), &obs(0, 101, 0.0)),
            Some(BreachedField::MaxTotalTokens)
        );
        assert_eq!(
            first_breach(&grader(None, None, Some(2.5)), &obs(0, 0, 2.6)),
            Some(BreachedField::MaxWallSeconds)
        );
    }

    #[test]
    fn first_breach_uses_canonical_order_when_several_breach() {
        let g = grader(Some(1), Some(1), Some(1.0));
        assert_eq!(
            first_breach(&g, &obs(5, 500, 50.0)),
            Some(BreachedField::MaxTurns)
        );
        let g = grader(None, Some(1), Some(1.0));
        assert_eq!(
            first_breach(&g, &obs(5, 500, 50.0)),
            Some(BreachedField::MaxTotalTokens)
        );
    }

    #[test]
    fn first_breach_limits_are_inclusive() {
        let g = grader(Some(5), Some(100), Some(2.5));
        assert_eq!(first_breach(&g, &obs(5, 100, 2.5)), None);
        let g = TranscriptGrader {
            max_wall_seconds: Some(Seconds::Int(3)),
            ..Default::default()
        };
        assert_eq!(first_breach(&g, &obs(0, 0, 3.0)), None);
        assert_eq!(
            first_breach(&g, &obs(0, 0, 3.001)),
            Some(BreachedField::MaxWallSeconds)
        );
    }

    // --- TranscriptCounter -------------------------------------------------

    #[test]
    fn observe_increments_turns_per_assistant_message() {
        let mut counter = TranscriptCounter::start_with_clock(Arc::new(|| 0.0));
        for _ in 0..3 {
            counter.observe(&assistant(None));
        }
        counter.observe(&json!({"message_type": "SystemMessage"}));
        assert_eq!(counter.turns, 3);
        assert_eq!(counter.total_tokens, 0);
    }

    #[test]
    fn observe_sums_assistant_usage_token_fields() {
        let mut counter = TranscriptCounter::start_with_clock(Arc::new(|| 0.0));
        counter.observe(&assistant(Some(
            json!({"input_tokens": 10, "output_tokens": 5}),
        )));
        counter.observe(&assistant(Some(
            json!({"input_tokens": 20, "output_tokens": 8}),
        )));
        assert_eq!(counter.total_tokens, 43);
    }

    #[test]
    fn observe_takes_max_with_result_message_totals() {
        let mut counter = TranscriptCounter::start_with_clock(Arc::new(|| 0.0));
        counter.observe(&assistant(Some(
            json!({"input_tokens": 10, "output_tokens": 5}),
        )));
        counter.observe(&result(
            5,
            Some(json!({"input_tokens": 100, "output_tokens": 50})),
        ));
        assert_eq!(counter.total_tokens, 150);
        assert_eq!(counter.turns, 5);
        // A lower result total never shrinks what was already counted.
        counter.observe(&result(1, Some(json!({"input_tokens": 1}))));
        assert_eq!(counter.total_tokens, 150);
        assert_eq!(counter.turns, 5);
    }

    #[test]
    fn observe_understands_stream_json_shapes() {
        let mut counter = TranscriptCounter::start_with_clock(Arc::new(|| 0.0));
        counter.observe(&json!({
            "type": "assistant",
            "message": {"role": "assistant", "usage": {"input_tokens": 4, "output_tokens": 6}},
        }));
        counter.observe(&json!({"type": "result", "num_turns": 3, "usage": {"input_tokens": 40}}));
        assert_eq!(counter.turns, 3);
        assert_eq!(counter.total_tokens, 40);
    }

    #[test]
    fn observe_understands_normalized_agent_events() {
        // The agents-backed invoker records normalized flywheel-agents
        // events: text blocks and tool calls are turns, the claude-code
        // normalizer's context.usage carries the line's counters, and the
        // session.finished payload carries the cumulative totals.
        let mut counter = TranscriptCounter::start_with_clock(Arc::new(|| 0.0));
        counter.observe(&json!({"type": "message.assistant", "payload": {"text": "hi"}}));
        counter.observe(&json!({"type": "context.usage", "payload": {"usage": {"input_tokens": 5, "output_tokens": 1}}}));
        counter.observe(&json!({"type": "thought", "payload": {"thinking": "hm"}}));
        counter.observe(
            &json!({"type": "tool.started", "payload": {"tool_use_id": "t", "tool_name": "Bash"}}),
        );
        counter.observe(&json!({"type": "tool.finished", "payload": {"tool_use_id": "t"}}));
        assert_eq!(counter.turns, 2);
        assert_eq!(counter.total_tokens, 6);
        counter.observe(&json!({"type": "session.finished", "payload": {"num_turns": 7, "usage": {"input_tokens": 40, "output_tokens": 2}}}));
        assert_eq!(counter.turns, 7);
        assert_eq!(counter.total_tokens, 42);
    }
    #[test]
    fn snapshot_uses_monotonic_clock() {
        let clock = FakeClock::new(0.0, 0.0);
        let counter = TranscriptCounter::start_with_clock(clock.clock());
        clock.advance(2.5);
        let snap = counter.snapshot();
        assert!((snap.wall_seconds - 2.5).abs() < 1e-9);
        assert_eq!(counter.started_at_monotonic(), 0.0);
        assert!(format!("{counter:?}").starts_with("TranscriptCounter"));
    }

    #[test]
    fn default_counter_reads_real_monotonic_time() {
        let counter = TranscriptCounter::default();
        assert!(counter.snapshot().wall_seconds >= 0.0);
        assert!(monotonic_seconds() >= counter.started_at_monotonic());
    }

    // --- enforce_transcript_limits -----------------------------------------

    #[tokio::test]
    async fn passes_through_when_no_graders() {
        let messages = vec![assistant(None), assistant(None), result(2, None)];
        let (out, outcome) = drain(messages.clone(), &[], None).await;
        assert_eq!(out, messages);
        assert_eq!(outcome.breached, None);
        assert_eq!(outcome.observation.turns, 2);
    }

    #[tokio::test]
    async fn aborts_stream_when_max_turns_exceeded() {
        let messages: Vec<_> = (0..5).map(|_| assistant(None)).collect();
        let (out, outcome) = drain(messages, &[grader(Some(2), None, None)], None).await;
        // Turn 1 (1 not > 2), turn 2 (2 not > 2), turn 3 (3 > 2 -> abort).
        assert_eq!(out.len(), 3);
        assert_eq!(outcome.breached, Some(BreachedField::MaxTurns));
        assert_eq!(outcome.observation.turns, 3);
    }

    #[tokio::test]
    async fn aborts_stream_when_max_total_tokens_exceeded() {
        let messages: Vec<_> = (0..5)
            .map(|_| assistant(Some(json!({"input_tokens": 30, "output_tokens": 20}))))
            .collect();
        let (out, outcome) = drain(messages, &[grader(None, Some(120), None)], None).await;
        assert_eq!(out.len(), 3);
        assert_eq!(outcome.breached, Some(BreachedField::MaxTotalTokens));
        assert_eq!(outcome.observation.total_tokens, 150);
    }

    #[tokio::test]
    async fn aborts_stream_when_max_wall_seconds_exceeded() {
        let clock = FakeClock::new(0.0, 1.0);
        let messages: Vec<_> = (0..5).map(|_| assistant(None)).collect();
        let (out, outcome) = drain(
            messages,
            &[grader(None, None, Some(2.5))],
            Some(clock.clock()),
        )
        .await;
        assert_eq!(out.len(), 3);
        assert_eq!(outcome.breached, Some(BreachedField::MaxWallSeconds));
    }

    #[tokio::test]
    async fn no_abort_when_within_all_limits() {
        let messages = vec![assistant(None), assistant(None), result(2, None)];
        let (out, outcome) = drain(messages.clone(), &[grader(Some(10), None, None)], None).await;
        assert_eq!(out, messages);
        assert_eq!(outcome.breached, None);
    }

    #[tokio::test]
    async fn tightest_grader_among_many_wins() {
        let messages: Vec<_> = (0..5).map(|_| assistant(None)).collect();
        let graders = [grader(Some(100), None, None), grader(Some(1), None, None)];
        let (out, outcome) = drain(messages, &graders, None).await;
        assert_eq!(out.len(), 2);
        assert_eq!(outcome.breached, Some(BreachedField::MaxTurns));
    }

    // --- run_transcript_graders --------------------------------------------

    #[test]
    fn passing_grader_persists_observed_without_breached() {
        let store = RecordingStore::default();
        let t = Task::new(
            "g",
            vec![Grader::Transcript(grader(Some(10), Some(1000), Some(60.0)))],
        );
        let rows = run(&t, &obs(5, 500, 30.0), &store);
        let row = &rows[0];
        assert!(row.passed);
        assert_eq!(row.grader_type, GraderType::Transcript);
        assert_eq!(row.id, Some(1));
        assert_eq!(row.ts, at(0));
        assert_eq!(
            row.payload["observed"],
            json!({"turns": 5, "total_tokens": 500, "wall_seconds": 30.0})
        );
        assert!(!row.payload.contains_key("breached"));
        assert_eq!(row.payload.keys().collect::<Vec<_>>(), vec!["observed"]);
    }

    #[test]
    fn failing_grader_records_each_breached_field() {
        let cases = [
            (grader(Some(2), None, None), obs(5, 0, 0.0), "max_turns"),
            (
                grader(None, Some(100), None),
                obs(0, 200, 0.0),
                "max_total_tokens",
            ),
            (
                grader(None, None, Some(10.0)),
                obs(0, 0, 12.3),
                "max_wall_seconds",
            ),
        ];
        for (g, o, expected) in cases {
            let store = RecordingStore::default();
            let t = Task::new("g", vec![Grader::Transcript(g)]);
            let rows = run(&t, &o, &store);
            assert!(!rows[0].passed);
            assert_eq!(rows[0].payload["breached"], expected);
            assert_eq!(
                rows[0].payload.keys().collect::<Vec<_>>(),
                vec!["observed", "breached"]
            );
        }
    }

    #[test]
    fn skips_non_transcript_graders_and_preserves_ordinals() {
        let store = RecordingStore::default();
        let t = Task::new(
            "g",
            vec![
                Grader::Command(CommandGrader::new("true").unwrap()),
                Grader::Transcript(grader(Some(10), None, None)),
                Grader::Rubric(RubricGrader::new(vec!["x".into()]).unwrap()),
                Grader::Transcript(grader(None, Some(1000), None)),
                Grader::Manual(ManualGrader::new("ok").unwrap()),
            ],
        );
        let rows = run(&t, &obs(1, 1, 1.0), &store);
        assert_eq!(
            rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(rows.iter().all(|r| r.grader_type == GraderType::Transcript));
    }

    #[test]
    fn first_failing_transcript_grader_aborts_later_ones() {
        let store = RecordingStore::default();
        let tight = TranscriptGrader {
            name: Some("tight".into()),
            ..grader(Some(1), None, None)
        };
        let loose = TranscriptGrader {
            name: Some("loose".into()),
            ..grader(Some(100), None, None)
        };
        let t = Task::new(
            "g",
            vec![Grader::Transcript(tight), Grader::Transcript(loose)],
        );
        let rows = run(&t, &obs(5, 0, 0.0), &store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].grader_name.as_deref(), Some("tight"));
        assert!(!rows[0].passed);
    }

    #[test]
    fn command_failure_skips_transcript_graders_entirely() {
        let store = RecordingStore::default();
        let t = Task::new("g", vec![Grader::Transcript(grader(Some(10), None, None))]);
        let rows =
            run_transcript_graders(&t, &obs(1, 1, 1.0), &store, "r1", 1, false, || at(0)).unwrap();
        assert!(rows.is_empty());
        assert!(store.list_grader_results("r1", 1).unwrap().is_empty());
    }

    #[test]
    fn grader_spec_snapshots_input_verbatim_with_seconds_parity() {
        let g = TranscriptGrader {
            max_turns: Some(5),
            max_total_tokens: Some(1000),
            max_wall_seconds: Some(Seconds::Int(60)),
            name: Some("budget".into()),
        };
        assert_eq!(
            Value::Object(grader_spec_snapshot(&g)),
            json!({"type": "transcript", "max_turns": 5, "max_total_tokens": 1000, "max_wall_seconds": 60, "name": "budget"})
        );
        assert_eq!(
            Value::Object(grader_spec_snapshot(&grader(None, None, Some(2.5)))),
            json!({"type": "transcript", "max_wall_seconds": 2.5})
        );
    }

    #[test]
    fn each_row_keys_by_run_id_attempt_number_and_ordinal() {
        let store = RecordingStore::default();
        let t = Task::new(
            "g",
            vec![
                Grader::Transcript(grader(Some(10), None, None)),
                Grader::Command(CommandGrader::new("true").unwrap()),
                Grader::Transcript(grader(Some(10), None, None)),
            ],
        );
        run(&t, &obs(1, 1, 1.0), &store);
        let keys: Vec<_> = store
            .list_grader_results("r1", 1)
            .unwrap()
            .iter()
            .map(|r| (r.run_id.clone(), r.attempt_number, r.ordinal))
            .collect();
        assert_eq!(keys, vec![("r1".to_owned(), 1, 0), ("r1".to_owned(), 1, 2)]);
    }

    // --- Abort and row agree on the breached field -------------------------

    #[tokio::test]
    async fn abort_and_persisted_row_agree_for_every_field() {
        let cases: Vec<(TranscriptGrader, Vec<Value>, f64, BreachedField)> = vec![
            (
                grader(Some(2), None, None),
                (0..10).map(|_| assistant(None)).collect(),
                0.0,
                BreachedField::MaxTurns,
            ),
            (
                grader(None, Some(120), None),
                (0..10)
                    .map(|_| assistant(Some(json!({"input_tokens": 30, "output_tokens": 20}))))
                    .collect(),
                0.0,
                BreachedField::MaxTotalTokens,
            ),
            (
                grader(None, None, Some(2.5)),
                (0..10).map(|_| assistant(None)).collect(),
                1.0,
                BreachedField::MaxWallSeconds,
            ),
        ];
        for (g, source, step, expected) in cases {
            let clock = FakeClock::new(0.0, step);
            let shared = clock.clock();
            // The harness keeps its own counter over the forwarded messages,
            // sharing the enforcer's clock.
            let mut counter = TranscriptCounter::start_with_clock(Arc::clone(&shared));
            let mut it = source.into_iter();
            let outcome = enforce_transcript_limits(
                async || it.next(),
                |m| counter.observe(&m),
                std::slice::from_ref(&g),
                Some(shared),
            )
            .await;
            assert_eq!(outcome.breached, Some(expected));
            let observation = counter.snapshot();
            assert_eq!(first_breach(&g, &observation), Some(expected));

            let store = RecordingStore::default();
            let t = Task::new("g", vec![Grader::Transcript(g)]);
            let rows = run(&t, &observation, &store);
            assert!(!rows[0].passed);
            assert_eq!(rows[0].payload["breached"], expected.as_str());
        }
    }

    #[test]
    fn breached_field_round_trips() {
        for f in LIMIT_ORDER {
            assert_eq!(serde_json::to_value(f).unwrap(), json!(f.as_str()));
            assert_eq!(BreachedField::parse(f.as_str()), Some(f));
            assert_eq!(f.to_string(), f.as_str());
        }
        assert_eq!(BreachedField::parse("max_cost"), None);
    }
}
