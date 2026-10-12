//! One agent iteration's structured result, and the envelope-salvage
//! helpers over it.
//!
//! This is the SDK-independent half of the Python `flywheel_core.invoker`
//! module: the data shapes every SDK signal enumerated in `docs/loop.md`'s
//! state-detection map is projected into, plus the pure helpers the harness
//! uses to recover a lost closing envelope. `invoke_iteration` (the function
//! that actually drives an agent turn) and the SDK message serializer are
//! deliberately **not** here: in the Rust implementation the harness drives
//! agents through the `flywheel-agents` crate (adapters, normalized events,
//! execution hosts), and that agents-backed path is what produces an
//! [`IterationResult`]. Opaque agent payloads (raw messages, rate-limit and
//! hook events, permission denials) are carried as `serde_json::Value`.
//!
//! Envelope extraction delegates entirely to [`crate::envelope`].

use crate::envelope::EnvelopeResult;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::ops::Add;

/// The agent stop reason that marks a clean end of turn.
pub const END_TURN_STOP_REASON: &str = "end_turn";

/// Content of a tool result block: plain text or a list of content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<Map<String, Value>>),
}

/// One tool result block observed in the stream.
///
/// Surfaced raw because the `blocked_implicit` counter in `docs/loop.md`
/// keys on `is_error == true` repeats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultObservation {
    pub tool_use_id: String,
    #[serde(default)]
    pub is_error: Option<bool>,
    #[serde(default)]
    pub content: Option<ToolResultContent>,
}

/// A tool-use block paired with its matching result block.
///
/// The harness keys `blocked_implicit` on `(tool_name, sha256(input))` and
/// feeds thrash detection the same `(tool, input)` tuple -- both need
/// name + input + outcome together, which this struct surfaces directly.
/// `result` is `None` when no result block arrived (a pending call).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInteraction {
    pub tool_use_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Map<String, Value>,
    #[serde(default)]
    pub result: Option<ToolResultObservation>,
}

impl ToolInteraction {
    /// True when a matching result block was observed and it flagged an
    /// error. A pending call (no result) is not a failure.
    pub fn is_failure(&self) -> bool {
        self.result
            .as_ref()
            .is_some_and(|r| r.is_error == Some(true))
    }
}

/// Iterator / subprocess failure captured without classification.
///
/// The harness -- not the invoker -- decides whether a given failure is
/// infrastructure, task, or protocol. This carries the raw evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationFailure {
    pub error_type: String,
    pub message: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stderr: Option<String>,
}

/// Structured projection of one iteration's agent signals.
///
/// Every surface listed in the `docs/loop.md` detection map is reachable
/// from here: stop reason, tool interactions and raw result blocks, the
/// result message's error flag / subtype / API status, turn and cost
/// counters, rate-limit and hook events, permission denials, and whether a
/// tool call was still pending when the stream stopped.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InvocationSignals {
    pub stop_reason: Option<String>,
    pub num_turns: Option<u64>,
    pub total_cost_usd: Option<f64>,
    pub result_is_error: Option<bool>,
    pub result_subtype: Option<String>,
    pub api_error_status: Option<u16>,
    pub session_id: Option<String>,
    pub permission_denials: Vec<Value>,
    pub rate_limit_events: Vec<Value>,
    pub tool_interactions: Vec<ToolInteraction>,
    pub tool_result_blocks: Vec<ToolResultObservation>,
    pub hook_events: Vec<Value>,
    pub pending_tool_use_at_stop: bool,
}

/// Token breakdown keyed by counter name (`input_tokens`, `output_tokens`,
/// `cache_creation_input_tokens`, `cache_read_input_tokens`).
pub type Usage = BTreeMap<String, u64>;

/// One iteration's transcript, envelope verdict, and structured signals.
///
/// `failure` is set when the agent transport raised -- non-zero exit,
/// stream error, cancellation surfaced as an error. Even on failure the
/// other fields reflect whatever was observed before it, including an
/// envelope verdict (typically `Missing` or `Truncated`).
///
/// `usage` is the iteration's token breakdown for invokers that produce no
/// per-message usage (a container backend driving the agent CLI in
/// stream-json mode). When `None` the harness derives the breakdown from
/// `messages`; when set, the harness uses it verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IterationResult {
    pub transcript: String,
    #[serde(default)]
    pub messages: Vec<Value>,
    pub envelope: EnvelopeResult,
    #[serde(default)]
    pub signals: InvocationSignals,
    #[serde(default)]
    pub failure: Option<InvocationFailure>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// True when an iteration finished cleanly but lost its envelope tail.
///
/// Only `missing` (no envelope at all -- the signature of an auto-compaction
/// dropping the protocol instruction after a long iteration) and
/// `truncated` (an opening fence with no close -- the final message was cut
/// off) are salvageable: both are a lost tail on otherwise-complete work,
/// recoverable by asking the still-live session to re-emit just the
/// envelope. `malformed` and `duplicate` are structural agent errors (the
/// agent emitted something wrong, not nothing) and are deliberately not
/// salvaged. A crash, a mid-tool-call stop, or any non-`end_turn` stop
/// reason routes to its existing handling unchanged.
pub fn iteration_envelope_is_salvageable(result: &IterationResult) -> bool {
    if result.failure.is_some() {
        return false;
    }
    if !matches!(
        result.envelope,
        EnvelopeResult::Missing | EnvelopeResult::Truncated { .. }
    ) {
        return false;
    }
    let signals = &result.signals;
    if signals.pending_tool_use_at_stop {
        return false;
    }
    signals.stop_reason.as_deref() == Some(END_TURN_STOP_REASON)
}

/// Add two optional numeric signals, treating a missing side as absent.
///
/// Returns `None` only when both are `None` (nothing observed on either
/// invocation); otherwise the present values sum (a missing side is zero).
pub fn sum_optional<T: Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => Some(a + b),
    }
}

/// Sum two optional usage mappings key-wise.
///
/// `None` on both sides stays `None` (the harness derives usage from the
/// merged `messages` instead). When either side carries an explicit
/// mapping, the per-key totals add so the salvage turn's tokens are not
/// lost.
pub fn merge_usage(a: Option<&Usage>, b: Option<&Usage>) -> Option<Usage> {
    if a.is_none() && b.is_none() {
        return None;
    }
    let mut merged = a.cloned().unwrap_or_default();
    for (key, value) in b.into_iter().flatten() {
        let slot = merged.entry(key.clone()).or_insert(0);
        *slot = slot.saturating_add(*value);
    }
    Some(merged)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.is_empty())
}

/// Fold a successful envelope-salvage re-invocation into the base result.
///
/// The salvage re-prompt continues the SAME live session, so its messages,
/// turns, and usage are a genuine continuation of `base`. The merged result
/// carries the salvaged envelope verbatim (not re-parsed from the
/// concatenated transcript, so a leftover truncated fence in `base` cannot
/// turn the merged text into a spurious duplicate/malformed verdict); both
/// transcripts joined by a newline; both message lists concatenated; summed
/// cost/turns; the salvage's terminal stop signals; and the work-describing
/// signals (tool interactions, rate-limit / hook events, permission
/// denials) unioned base-then-salvage so nothing the base observed is
/// dropped.
pub fn merge_salvaged_iteration(
    base: IterationResult,
    salvage: IterationResult,
) -> IterationResult {
    let IterationResult {
        transcript: base_transcript,
        mut messages,
        envelope: _,
        mut signals,
        failure: _,
        usage: base_usage,
    } = base;
    let IterationResult {
        transcript: salvage_transcript,
        messages: salvage_messages,
        envelope,
        signals: salv,
        failure,
        usage: salvage_usage,
    } = salvage;

    signals.stop_reason = non_empty(salv.stop_reason).or(signals.stop_reason.take());
    signals.num_turns = sum_optional(signals.num_turns, salv.num_turns);
    signals.total_cost_usd = sum_optional(signals.total_cost_usd, salv.total_cost_usd);
    signals.result_is_error = salv.result_is_error;
    signals.result_subtype = salv.result_subtype;
    signals.api_error_status = salv.api_error_status.or(signals.api_error_status);
    signals.session_id = non_empty(salv.session_id).or(signals.session_id.take());
    signals.permission_denials.extend(salv.permission_denials);
    signals.rate_limit_events.extend(salv.rate_limit_events);
    signals.tool_interactions.extend(salv.tool_interactions);
    signals.tool_result_blocks.extend(salv.tool_result_blocks);
    signals.hook_events.extend(salv.hook_events);
    signals.pending_tool_use_at_stop = salv.pending_tool_use_at_stop;

    messages.extend(salvage_messages);

    IterationResult {
        transcript: format!("{base_transcript}\n{salvage_transcript}"),
        messages,
        envelope,
        signals,
        failure,
        usage: merge_usage(base_usage.as_ref(), salvage_usage.as_ref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{CLOSING_FENCE, OPENING_FENCE, parse_envelope};
    use serde_json::json;

    fn build(transcript: &str, stop_reason: Option<&str>) -> IterationResult {
        IterationResult {
            transcript: transcript.to_string(),
            messages: vec![json!({"message_type": "AssistantMessage", "text": transcript})],
            envelope: parse_envelope(transcript),
            signals: InvocationSignals {
                stop_reason: stop_reason.map(str::to_string),
                ..InvocationSignals::default()
            },
            failure: None,
            usage: None,
        }
    }

    fn wrapped(payload: &str) -> String {
        format!("{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}")
    }

    #[test]
    fn missing_after_clean_end_is_salvageable() {
        let result = build("did work, no envelope", Some("end_turn"));
        assert_eq!(result.envelope, EnvelopeResult::Missing);
        assert!(iteration_envelope_is_salvageable(&result));
    }

    #[test]
    fn truncated_after_clean_end_is_salvageable() {
        let result = build(&format!("{OPENING_FENCE}\n{{\"intent\""), Some("end_turn"));
        assert!(matches!(result.envelope, EnvelopeResult::Truncated { .. }));
        assert!(iteration_envelope_is_salvageable(&result));
    }

    #[test]
    fn valid_malformed_and_duplicate_are_not_salvageable() {
        let valid = build(&wrapped(r#"{"intent": "verify"}"#), Some("end_turn"));
        assert!(valid.envelope.as_valid().is_some());
        assert!(!iteration_envelope_is_salvageable(&valid));
        let malformed = build(&wrapped("not json"), Some("end_turn"));
        assert!(matches!(
            malformed.envelope,
            EnvelopeResult::Malformed { .. }
        ));
        assert!(!iteration_envelope_is_salvageable(&malformed));
        let duplicate = build(
            &format!("{OPENING_FENCE}{OPENING_FENCE}{CLOSING_FENCE}"),
            Some("end_turn"),
        );
        assert!(matches!(
            duplicate.envelope,
            EnvelopeResult::Duplicate { .. }
        ));
        assert!(!iteration_envelope_is_salvageable(&duplicate));
    }

    #[test]
    fn missing_without_end_turn_is_not_salvageable() {
        assert!(!iteration_envelope_is_salvageable(&build(
            "cut off",
            Some("max_tokens")
        )));
        assert!(!iteration_envelope_is_salvageable(&build("cut off", None)));
    }

    #[test]
    fn pending_tool_use_or_failure_is_not_salvageable() {
        let mut result = build("no envelope", Some("end_turn"));
        result.signals.pending_tool_use_at_stop = true;
        assert!(!iteration_envelope_is_salvageable(&result));

        let mut result = build("no envelope", Some("end_turn"));
        result.failure = Some(InvocationFailure {
            error_type: "ProcessError".into(),
            message: "exit 1".into(),
            exit_code: Some(1),
            stderr: None,
        });
        assert!(!iteration_envelope_is_salvageable(&result));
    }

    #[test]
    fn merge_adopts_salvage_envelope_and_folds_signals() {
        let mut base = build("the work", Some("end_turn"));
        base.signals.num_turns = Some(40);
        base.signals.total_cost_usd = Some(3.50);
        base.signals.session_id = Some("sess-1".into());
        base.signals.api_error_status = Some(529);
        base.signals.tool_interactions.push(ToolInteraction {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            tool_input: Map::new(),
            result: None,
        });
        base.signals.pending_tool_use_at_stop = true;
        base.usage = Some(Usage::from([
            ("input_tokens".to_string(), 100),
            ("output_tokens".to_string(), 5),
        ]));

        let mut salvage = build(&wrapped(r#"{"intent": "verify"}"#), Some("end_turn"));
        salvage.signals.num_turns = Some(1);
        salvage.signals.total_cost_usd = Some(0.05);
        salvage.signals.session_id = Some(String::new());
        salvage.signals.result_subtype = Some("success".into());
        salvage
            .signals
            .rate_limit_events
            .push(json!({"kind": "rate_limit"}));
        salvage.usage = Some(Usage::from([("output_tokens".to_string(), 7)]));
        let expected_envelope = salvage.envelope.clone();
        let base_messages = base.messages.clone();
        let salvage_messages = salvage.messages.clone();

        let merged = merge_salvaged_iteration(base, salvage);
        assert_eq!(merged.envelope, expected_envelope);
        assert!(merged.envelope.as_valid().is_some());
        assert_eq!(
            merged.transcript,
            format!("the work\n{}", wrapped(r#"{"intent": "verify"}"#))
        );
        assert_eq!(merged.messages, [base_messages, salvage_messages].concat());
        assert!((merged.signals.total_cost_usd.unwrap() - 3.55).abs() < 1e-9);
        assert_eq!(merged.signals.num_turns, Some(41));
        // An empty salvage session id falls back to the base's.
        assert_eq!(merged.signals.session_id.as_deref(), Some("sess-1"));
        assert_eq!(merged.signals.api_error_status, Some(529));
        assert_eq!(merged.signals.result_subtype.as_deref(), Some("success"));
        assert_eq!(merged.signals.tool_interactions.len(), 1);
        assert_eq!(merged.signals.rate_limit_events.len(), 1);
        assert!(!merged.signals.pending_tool_use_at_stop);
        assert_eq!(merged.failure, None);
        assert_eq!(
            merged.usage,
            Some(Usage::from([
                ("input_tokens".to_string(), 100),
                ("output_tokens".to_string(), 12)
            ]))
        );
    }

    #[test]
    fn sum_optional_treats_missing_side_as_absent() {
        assert_eq!(sum_optional::<u64>(None, None), None);
        assert_eq!(sum_optional(Some(2u64), None), Some(2));
        assert_eq!(sum_optional(None, Some(3u64)), Some(3));
        assert_eq!(sum_optional(Some(2u64), Some(3)), Some(5));
        assert_eq!(sum_optional(Some(1.5f64), Some(0.25)), Some(1.75));
    }

    #[test]
    fn merge_usage_sums_keywise_and_keeps_none_when_both_absent() {
        assert_eq!(merge_usage(None, None), None);
        let a = Usage::from([("input_tokens".to_string(), 1)]);
        assert_eq!(merge_usage(Some(&a), None), Some(a.clone()));
        assert_eq!(merge_usage(None, Some(&a)), Some(a.clone()));
        let b = Usage::from([
            ("input_tokens".to_string(), 2),
            ("cache_read_input_tokens".to_string(), 9),
        ]);
        assert_eq!(
            merge_usage(Some(&a), Some(&b)),
            Some(Usage::from([
                ("input_tokens".to_string(), 3),
                ("cache_read_input_tokens".to_string(), 9)
            ]))
        );
        let max = Usage::from([("input_tokens".to_string(), u64::MAX)]);
        assert_eq!(
            merge_usage(Some(&max), Some(&a)).unwrap()["input_tokens"],
            u64::MAX
        );
    }

    #[test]
    fn tool_interaction_failure_requires_an_error_result() {
        let mut interaction = ToolInteraction {
            tool_use_id: "t".into(),
            tool_name: "Bash".into(),
            tool_input: Map::new(),
            result: None,
        };
        assert!(!interaction.is_failure());
        interaction.result = Some(ToolResultObservation {
            tool_use_id: "t".into(),
            is_error: None,
            content: None,
        });
        assert!(!interaction.is_failure());
        interaction.result = Some(ToolResultObservation {
            tool_use_id: "t".into(),
            is_error: Some(true),
            content: Some(ToolResultContent::Text("boom".into())),
        });
        assert!(interaction.is_failure());
    }

    #[test]
    fn iteration_result_round_trips_through_json() {
        let mut result = build(&wrapped(r#"{"intent": "continue"}"#), Some("end_turn"));
        result.signals.tool_interactions.push(ToolInteraction {
            tool_use_id: "t1".into(),
            tool_name: "Read".into(),
            tool_input: json!({"path": "a.rs"}).as_object().unwrap().clone(),
            result: Some(ToolResultObservation {
                tool_use_id: "t1".into(),
                is_error: Some(false),
                content: Some(ToolResultContent::Blocks(vec![
                    json!({"type": "text", "text": "fn main() {}"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ])),
            }),
        });
        result.usage = Some(Usage::from([("input_tokens".to_string(), 3)]));
        let json = serde_json::to_string(&result).unwrap();
        let back: IterationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, result);
        // Minimal payloads deserialize with defaults for the optional fields.
        let minimal: IterationResult =
            serde_json::from_value(json!({"transcript": "", "envelope": {"kind": "missing"}}))
                .unwrap();
        assert_eq!(minimal.signals, InvocationSignals::default());
        assert!(minimal.messages.is_empty());
    }
}
