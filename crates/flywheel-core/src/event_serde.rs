//! Serialization between domain events and their persisted row shape.
//!
//! `crate::events` is pure: it knows nothing about how an event is stored.
//! This module is the bridge. It projects a [`DomainEventPayload`] into the
//! JSON text held by the `events.payload_json` column and reconstructs the
//! typed event from a stored row. The common columns (`run_id`, `ts`,
//! `attempt_number`, `sequence`, `id`) live on the row, not in the payload;
//! the `kind` discriminator is its own column and is never written into the
//! payload text, so rows are byte-compatible with the Python implementation.
//!
//! Timestamps are written as RFC 3339 UTC in exactly the form Python's
//! `datetime.isoformat()` emits for an aware UTC value
//! (`2026-01-01T00:00:00+00:00`, microseconds only when non-zero) so rows
//! written by either implementation sort and compare identically. Reads
//! accept every form either side has ever written: any RFC 3339 offset or
//! `Z`, any fractional precision, and a naive `2026-01-01T00:00:00` (treated
//! as UTC).

use chrono::{DateTime, NaiveDateTime, Timelike, Utc};
use serde::{Deserialize, Deserializer, Serializer};
use serde_json::{Map, Value};
use std::fmt;

use crate::events::{DomainEvent, DomainEventKind, DomainEventPayload};

/// The JSON key the tagged payload enum uses for its discriminator. It is
/// stripped from the persisted payload text and re-injected on read from the
/// row's `kind` column.
const KIND_TAG: &str = "kind";

// --- Timestamps -------------------------------------------------------------

/// A persisted timestamp string that no accepted form could parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot parse timestamp {0:?}")]
pub struct TimestampParseError(pub String);

/// Format a UTC instant as the store's canonical timestamp text.
///
/// Byte-identical to Python's `datetime.isoformat()` for an aware UTC value:
/// `YYYY-MM-DDTHH:MM:SS+00:00`, with a six-digit fractional part only when the
/// microseconds are non-zero. Sub-microsecond precision is truncated so a
/// round trip through either implementation is stable.
pub fn format_timestamp(ts: &DateTime<Utc>) -> String {
    let micros = ts.nanosecond() / 1_000;
    if micros == 0 {
        ts.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        format!("{}.{micros:06}+00:00", ts.format("%Y-%m-%dT%H:%M:%S"))
    }
}

/// Parse a persisted timestamp into a UTC instant.
///
/// Accepts RFC 3339 with any offset or `Z` and any fractional precision
/// (chrono's and Python's aware `isoformat()` output), and a naive
/// `YYYY-MM-DDTHH:MM:SS[.ffffff]` which is interpreted as UTC (Python's naive
/// `isoformat()` output). A space separator is accepted in place of `T`.
pub fn parse_timestamp(text: &str) -> Result<DateTime<Utc>, TimestampParseError> {
    let trimmed = text.trim();
    if let Ok(aware) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(aware.with_timezone(&Utc));
    }
    // RFC 3339 requires a `T` (or `t`) separator; Python accepts any single
    // character, in practice a space. Normalize and retry before the naive
    // forms.
    let normalized = normalize_separator(trimmed);
    if let Ok(aware) = DateTime::parse_from_rfc3339(&normalized) {
        return Ok(aware.with_timezone(&Utc));
    }
    for pattern in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, pattern) {
            return Ok(naive.and_utc());
        }
    }
    Err(TimestampParseError(text.to_owned()))
}

fn normalize_separator(text: &str) -> String {
    // The date part is exactly ten bytes (`YYYY-MM-DD`); whatever follows it
    // is the separator. Only a space or lowercase `t` needs normalizing.
    match text.as_bytes().get(10) {
        Some(b' ') | Some(b't') => {
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..10]);
            out.push('T');
            out.push_str(&text[11..]);
            out
        }
        _ => text.to_owned(),
    }
}

/// `#[serde(with = "crate::event_serde::timestamp")]` for a required
/// `DateTime<Utc>` field: writes [`format_timestamp`], reads
/// [`parse_timestamp`].
pub mod timestamp {
    use super::*;

    pub fn serialize<S: Serializer>(ts: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_timestamp(ts))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let text = String::deserialize(d)?;
        parse_timestamp(&text).map_err(serde::de::Error::custom)
    }
}

/// `#[serde(with = "crate::event_serde::optional_timestamp")]` for an
/// `Option<DateTime<Utc>>` field: `null` round-trips as `None`.
pub mod optional_timestamp {
    use super::*;

    pub fn serialize<S: Serializer>(ts: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match ts {
            Some(ts) => s.serialize_some(&format_timestamp(ts)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(text) => parse_timestamp(&text)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

// --- Event <-> row --------------------------------------------------------------

/// Raised when a stored row cannot be turned back into a typed event, or an
/// event cannot be projected into row text.
#[derive(Debug, thiserror::Error)]
pub enum EventSerdeError {
    /// The row's `kind` column is not a member of the closed domain-event
    /// set: a corrupt or forward-incompatible row, surfaced loudly rather
    /// than silently dropped.
    #[error("unknown domain event kind {0:?}")]
    UnknownKind(String),
    /// The payload text is not a JSON object.
    #[error("domain event payload must be a JSON object, got {0}")]
    PayloadNotObject(&'static str),
    #[error("domain event payload: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Timestamp(#[from] TimestampParseError),
}

/// The `events` table row shape, as persisted: timestamps are text, the
/// payload is JSON text, and the common columns sit beside it. A concrete
/// store maps its columns onto this struct in both directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub id: Option<i64>,
    pub run_id: String,
    pub attempt_number: Option<u32>,
    pub ts: String,
    pub kind: String,
    pub payload_json: String,
    pub sequence: Option<u64>,
}

/// The stable wire discriminator for `event` (the `events.kind` column).
pub fn event_kind(event: &DomainEvent) -> &'static str {
    event.payload.kind().as_str()
}

/// Project the event-specific fields into a JSON object.
///
/// Excludes the common columns and the `kind` discriminator, which the store
/// persists as dedicated columns. Fields the Python implementation omits when
/// at their default (a park's `receipts`, a land's `rung`, the agent-session
/// usage pair) are omitted here too, so a row written by either side is
/// byte-identical for the same event.
pub fn event_payload(payload: &DomainEventPayload) -> Result<Map<String, Value>, EventSerdeError> {
    match serde_json::to_value(payload)? {
        Value::Object(mut map) => {
            map.shift_remove(KIND_TAG);
            Ok(map)
        }
        other => Err(EventSerdeError::PayloadNotObject(json_type_name(&other))),
    }
}

/// [`event_payload`] rendered as the `payload_json` column text.
pub fn event_payload_json(payload: &DomainEventPayload) -> Result<String, EventSerdeError> {
    Ok(serde_json::to_string(&event_payload(payload)?)?)
}

/// Reconstruct a typed payload from the `kind` column and a parsed payload
/// object. Missing optional fields take the same defaults the Python reader
/// applies.
pub fn payload_from_value(
    kind: &str,
    payload: Map<String, Value>,
) -> Result<DomainEventPayload, EventSerdeError> {
    let kind: DomainEventKind = kind
        .parse()
        .map_err(|_| EventSerdeError::UnknownKind(kind.to_owned()))?;
    let mut tagged = payload;
    tagged.insert(KIND_TAG.to_owned(), Value::String(kind.as_str().to_owned()));
    Ok(serde_json::from_value(Value::Object(tagged))?)
}

/// Reconstruct a typed payload from the `kind` and `payload_json` columns.
pub fn payload_from_json(
    kind: &str,
    payload_json: &str,
) -> Result<DomainEventPayload, EventSerdeError> {
    match serde_json::from_str::<Value>(payload_json)? {
        Value::Object(map) => payload_from_value(kind, map),
        other => Err(EventSerdeError::PayloadNotObject(json_type_name(&other))),
    }
}

/// Project a typed event into its persisted row shape. `id` and `sequence`
/// are carried through as-is; the store assigns them on append.
pub fn event_to_row(event: &DomainEvent) -> Result<EventRow, EventSerdeError> {
    Ok(EventRow {
        id: event.id,
        run_id: event.run_id.clone(),
        attempt_number: event.attempt_number,
        ts: format_timestamp(&event.ts),
        kind: event_kind(event).to_owned(),
        payload_json: event_payload_json(&event.payload)?,
        sequence: event.sequence,
    })
}

/// Reconstruct a typed [`DomainEvent`] from a stored row.
///
/// Fails loudly on an unrecognized `kind`, a malformed payload, or an
/// unparseable timestamp so a corrupt or forward-incompatible row never
/// silently drops.
pub fn event_from_row(row: &EventRow) -> Result<DomainEvent, EventSerdeError> {
    Ok(DomainEvent {
        run_id: row.run_id.clone(),
        ts: parse_timestamp(&row.ts)?,
        attempt_number: row.attempt_number,
        sequence: row.sequence,
        id: row.id,
        payload: payload_from_json(&row.kind, &row.payload_json)?,
    })
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

impl fmt::Display for EventRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} seq={} kind={} ts={}",
            self.run_id,
            self.sequence
                .map_or_else(|| "-".to_owned(), |s| s.to_string()),
            self.kind,
            self.ts
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        AttemptFinalized, AttemptStarted, CommandApplied, GateGraderReceipt, GraderEvaluated,
        HeldOutGateEvaluated, Landed, LandingParked, LandingRedriven, LandingRouted,
        LifecycleInitialized, REDRIVE_RESULT_LANDED, REDRIVE_RESULT_REPARKED,
        REDRIVE_RESULT_ROUTED, RetryScheduled, TransitionedTo,
    };
    use crate::lifecycle::{Outcome, Status};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_780_000_000 + secs, 0).unwrap()
    }

    fn round_trip(payload: DomainEventPayload) -> DomainEvent {
        let event = DomainEvent::new("run-9", at(5), payload);
        let row = event_to_row(&event).unwrap();
        let restored = event_from_row(&row).unwrap();
        assert_eq!(restored, event);
        restored
    }

    #[test]
    fn format_matches_python_isoformat() {
        let ts = DateTime::from_timestamp(1_780_000_000, 0).unwrap();
        assert_eq!(format_timestamp(&ts), "2026-05-28T20:26:40+00:00");
        let with_micros = DateTime::from_timestamp(1_780_000_000, 123_456_789).unwrap();
        assert_eq!(
            format_timestamp(&with_micros),
            "2026-05-28T20:26:40.123456+00:00"
        );
    }

    #[test]
    fn parse_accepts_every_known_form() {
        let expected = DateTime::from_timestamp(1_780_000_000, 0).unwrap();
        for text in [
            "2026-05-28T20:26:40+00:00",
            "2026-05-28T20:26:40Z",
            "2026-05-28T20:26:40",
            "2026-05-28 20:26:40",
            "2026-05-28T20:26:40.000000+00:00",
            "2026-05-28T22:26:40+02:00",
            "  2026-05-28T20:26:40Z\n",
        ] {
            assert_eq!(parse_timestamp(text).unwrap(), expected, "{text}");
        }
        let micros = DateTime::from_timestamp(1_780_000_000, 123_456_000).unwrap();
        assert_eq!(
            parse_timestamp("2026-05-28T20:26:40.123456+00:00").unwrap(),
            micros
        );
        assert_eq!(
            parse_timestamp("2026-05-28T20:26:40.123456").unwrap(),
            micros
        );
        let nanos = DateTime::from_timestamp(1_780_000_000, 123_456_789).unwrap();
        assert_eq!(
            parse_timestamp("2026-05-28T20:26:40.123456789Z").unwrap(),
            nanos
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        for text in ["", "yesterday", "2026-13-01T00:00:00Z", "2026-05-28T"] {
            assert_eq!(
                parse_timestamp(text).unwrap_err(),
                TimestampParseError(text.to_owned()),
                "{text}"
            );
        }
    }

    #[test]
    fn format_then_parse_is_identity_at_microsecond_precision() {
        let ts = DateTime::from_timestamp(1_780_000_000, 999_999_999).unwrap();
        let truncated = DateTime::from_timestamp(1_780_000_000, 999_999_000).unwrap();
        assert_eq!(parse_timestamp(&format_timestamp(&ts)).unwrap(), truncated);
    }

    #[test]
    fn serde_with_modules_round_trip_including_null() {
        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Holder {
            #[serde(with = "timestamp")]
            ts: DateTime<Utc>,
            #[serde(with = "optional_timestamp")]
            maybe: Option<DateTime<Utc>>,
        }
        let holder = Holder {
            ts: at(0),
            maybe: None,
        };
        let text = serde_json::to_string(&holder).unwrap();
        assert_eq!(text, r#"{"ts":"2026-05-28T20:26:40+00:00","maybe":null}"#);
        assert_eq!(serde_json::from_str::<Holder>(&text).unwrap(), holder);
        let python: Holder = serde_json::from_str(
            r#"{"ts":"2026-05-28T20:26:40","maybe":"2026-05-28T20:26:41+00:00"}"#,
        )
        .unwrap();
        assert_eq!(
            python,
            Holder {
                ts: at(0),
                maybe: Some(at(1))
            }
        );
    }

    #[test]
    fn kind_is_never_written_into_the_payload() {
        let payload = event_payload(&DomainEventPayload::Unblocked).unwrap();
        assert!(payload.is_empty());
        let text = event_payload_json(&DomainEventPayload::TransitionedTo(TransitionedTo {
            target: Status::Ready,
            error: String::new(),
        }))
        .unwrap();
        assert_eq!(text, r#"{"target":"ready","error":""}"#);
    }

    #[test]
    fn landing_parked_round_trips_to_the_two_key_payload() {
        let event = round_trip(DomainEventPayload::LandingParked(LandingParked {
            park_kind: "divergent-base".into(),
            detail: "cannot fast-forward landing-base".into(),
            ..LandingParked::default()
        }));
        assert_eq!(event_kind(&event), "landing_parked");
        let payload = event_payload(&event.payload).unwrap();
        assert_eq!(
            Value::Object(payload),
            json!({"park_kind": "divergent-base", "detail": "cannot fast-forward landing-base"})
        );
    }

    #[test]
    fn landing_parked_with_receipts_and_session_usage_emits_the_extra_keys() {
        let event = round_trip(DomainEventPayload::LandingParked(LandingParked {
            park_kind: "merge-conflict".into(),
            detail: "resolver exhausted".into(),
            receipts: vec![GateGraderReceipt {
                grader_name: Some("check".into()),
                passed: false,
                output_excerpt: "boom\n".into(),
            }],
            agent_turns: Some(12),
            agent_wall_seconds: Some(90.5),
        }));
        assert_eq!(
            Value::Object(event_payload(&event.payload).unwrap()),
            json!({
                "park_kind": "merge-conflict",
                "detail": "resolver exhausted",
                "receipts": [{"grader_name": "check", "passed": false, "output_excerpt": "boom\n"}],
                "agent_turns": 12,
                "agent_wall_seconds": 90.5
            })
        );
    }

    #[test]
    fn landed_round_trips_for_merge_and_pr_references() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let merge = round_trip(DomainEventPayload::Landed(Landed {
            strategy: "merge".into(),
            landed_ref: sha.into(),
            ..Landed::default()
        }));
        assert_eq!(event_kind(&merge), "landed");
        assert_eq!(
            Value::Object(event_payload(&merge.payload).unwrap()),
            json!({"strategy": "merge", "landed_ref": sha})
        );
        let pr = round_trip(DomainEventPayload::Landed(Landed {
            strategy: "pr".into(),
            landed_ref: "https://example.test/pr/7".into(),
            rung: "rebase".into(),
            agent_turns: Some(3),
            agent_wall_seconds: Some(1.25),
        }));
        assert_eq!(
            Value::Object(event_payload(&pr.payload).unwrap()),
            json!({
                "strategy": "pr",
                "landed_ref": "https://example.test/pr/7",
                "rung": "rebase",
                "agent_turns": 3,
                "agent_wall_seconds": 1.25
            })
        );
    }

    #[test]
    fn held_out_gate_evaluated_round_trips_with_and_without_receipts() {
        let reason = "held-out gate FAILED: 2 of 2 grader(s) ran; [oracle-b] exited exit_code=1";
        let failed = round_trip(DomainEventPayload::HeldOutGateEvaluated(
            HeldOutGateEvaluated {
                outcome: "fail".into(),
                reason: reason.into(),
                receipts: vec![
                    GateGraderReceipt {
                        grader_name: Some("oracle-a".into()),
                        passed: true,
                        output_excerpt: "all good\n".into(),
                    },
                    GateGraderReceipt {
                        grader_name: None,
                        passed: false,
                        output_excerpt: "assertion failed\n".into(),
                    },
                ],
            },
        ));
        assert_eq!(event_kind(&failed), "held_out_gate_evaluated");
        assert_eq!(
            Value::Object(event_payload(&failed.payload).unwrap()),
            json!({
                "outcome": "fail",
                "reason": reason,
                "receipts": [
                    {"grader_name": "oracle-a", "passed": true, "output_excerpt": "all good\n"},
                    {"grader_name": null, "passed": false, "output_excerpt": "assertion failed\n"}
                ]
            })
        );
        let no_gate = round_trip(DomainEventPayload::HeldOutGateEvaluated(
            HeldOutGateEvaluated {
                outcome: "no_gate".into(),
                reason: "no held-out graders registered for this task".into(),
                receipts: Vec::new(),
            },
        ));
        assert_eq!(
            Value::Object(event_payload(&no_gate.payload).unwrap()),
            json!({
                "outcome": "no_gate",
                "reason": "no held-out graders registered for this task",
                "receipts": []
            })
        );
    }

    #[test]
    fn landing_redriven_round_trips_every_result_and_defaults_park_kind() {
        for result in [
            REDRIVE_RESULT_LANDED,
            REDRIVE_RESULT_REPARKED,
            REDRIVE_RESULT_ROUTED,
        ] {
            let event = round_trip(DomainEventPayload::LandingRedriven(LandingRedriven {
                result: result.into(),
                park_kind: "divergent-base".into(),
            }));
            assert_eq!(event_kind(&event), "landing_redriven");
            assert_eq!(
                Value::Object(event_payload(&event.payload).unwrap()),
                json!({"result": result, "park_kind": "divergent-base"})
            );
        }
        let restored = payload_from_json("landing_redriven", r#"{"result": "routed"}"#).unwrap();
        assert_eq!(
            restored,
            DomainEventPayload::LandingRedriven(LandingRedriven {
                result: REDRIVE_RESULT_ROUTED.into(),
                park_kind: String::new(),
            })
        );
    }

    #[test]
    fn landing_routed_round_trips_and_defaults_per_file_tiers() {
        let event = round_trip(DomainEventPayload::LandingRouted(LandingRouted {
            winning_tier: 1,
            strategy: "phase".into(),
            per_file_tiers: BTreeMap::from([
                ("src/x.py".to_owned(), 1),
                ("docs/a.md".to_owned(), 0),
            ]),
        }));
        assert_eq!(event_kind(&event), "landing_routed");
        assert_eq!(
            Value::Object(event_payload(&event.payload).unwrap()),
            json!({
                "winning_tier": 1,
                "strategy": "phase",
                "per_file_tiers": {"docs/a.md": 0, "src/x.py": 1}
            })
        );
        let restored = payload_from_json(
            "landing_routed",
            r#"{"winning_tier": 0, "strategy": "merge"}"#,
        )
        .unwrap();
        assert_eq!(
            restored,
            DomainEventPayload::LandingRouted(LandingRouted {
                winning_tier: 0,
                strategy: "merge".into(),
                per_file_tiers: BTreeMap::new(),
            })
        );
    }

    #[test]
    fn every_kind_round_trips_through_a_row() {
        let payloads = vec![
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: "task-1".into(),
                worker_id: "w".into(),
                artifacts_dir: "/a".into(),
                task_content_hash: "h".into(),
                source: "tasks/t.md".into(),
            }),
            DomainEventPayload::TransitionedTo(TransitionedTo {
                target: Status::FailedValidation,
                error: "grader failed".into(),
            }),
            DomainEventPayload::Blocked(crate::events::Blocked {
                requires_json: "[]".into(),
            }),
            DomainEventPayload::Unblocked,
            DomainEventPayload::AwaitingApproval(crate::events::AwaitingApproval {
                awaiting_ordinal: 2,
            }),
            DomainEventPayload::RetryScheduled(RetryScheduled {
                retries_used: 0,
                max_retries: 2,
            }),
            DomainEventPayload::AttemptStarted(AttemptStarted {
                number: 1,
                attempt_run_id: "run-9".into(),
                started_at: at(3),
                agent_context: BTreeMap::from([("model".to_owned(), "claude-opus-4-8".to_owned())]),
            }),
            DomainEventPayload::AttemptFinalized(AttemptFinalized {
                number: 1,
                outcome: Outcome::Succeeded,
                ended_at: at(6),
                agent_output: "all done".into(),
                error: String::new(),
            }),
            DomainEventPayload::SessionRecorded(crate::events::SessionRecorded {
                session_id: "sess-42".into(),
            }),
            DomainEventPayload::GraderEvaluated(GraderEvaluated {
                ordinal: 0,
                grader_type: "command".into(),
                passed: true,
                duration_ms: 12,
                grader_name: Some("pytest".into()),
                grader_spec: json!({"run": "uv run pytest"}).as_object().unwrap().clone(),
                payload: json!({"exit_code": 0}).as_object().unwrap().clone(),
            }),
            DomainEventPayload::CommandApplied(CommandApplied {
                command_kind: "say".into(),
                command_payload: json!({"text": "focus"}).as_object().unwrap().clone(),
                command_id: Some(7),
            }),
        ];
        for payload in payloads {
            let kind = payload.kind();
            let restored = round_trip(payload);
            assert_eq!(restored.payload.kind(), kind);
        }
    }

    #[test]
    fn python_written_rows_are_readable() {
        // Payloads exactly as the Python store serializes them, including
        // the isoformat timestamps inside attempt events and a null name.
        let started = EventRow {
            id: Some(4),
            run_id: "run-1".into(),
            attempt_number: Some(1),
            ts: "2026-05-28T12:00:03+00:00".into(),
            kind: "attempt_started".into(),
            payload_json: r#"{"number": 1, "attempt_run_id": "run-1", "started_at": "2026-05-28T12:00:03.250000+00:00", "agent_context": {"model": "claude-opus-4-8"}}"#.into(),
            sequence: Some(4),
        };
        let event = event_from_row(&started).unwrap();
        assert_eq!(event.id, Some(4));
        assert_eq!(event.sequence, Some(4));
        assert_eq!(event.attempt_number, Some(1));
        match &event.payload {
            DomainEventPayload::AttemptStarted(a) => {
                assert_eq!(a.number, 1);
                assert_eq!(a.started_at.timestamp_subsec_micros(), 250_000);
                assert_eq!(a.agent_context["model"], "claude-opus-4-8");
            }
            other => panic!("unexpected payload {other:?}"),
        }
        let graded = payload_from_json(
            "grader_evaluated",
            r#"{"ordinal": 0, "grader_type": "command", "passed": true, "duration_ms": 12, "grader_name": null, "grader_spec": {}, "payload": {}}"#,
        )
        .unwrap();
        match graded {
            DomainEventPayload::GraderEvaluated(g) => assert_eq!(g.grader_name, None),
            other => panic!("unexpected payload {other:?}"),
        }
        let minimal = payload_from_json("lifecycle_initialized", r#"{"task_id": "t"}"#).unwrap();
        assert_eq!(
            minimal,
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: "t".into(),
                ..LifecycleInitialized::default()
            })
        );
    }

    #[test]
    fn unknown_kind_and_malformed_payload_fail_loudly() {
        assert!(matches!(
            payload_from_json("not_a_kind", "{}").unwrap_err(),
            EventSerdeError::UnknownKind(k) if k == "not_a_kind"
        ));
        assert!(matches!(
            payload_from_json("unblocked", "[]").unwrap_err(),
            EventSerdeError::PayloadNotObject("array")
        ));
        assert!(matches!(
            payload_from_json("blocked", "{}").unwrap_err(),
            EventSerdeError::Json(_)
        ));
        assert!(matches!(
            payload_from_json("blocked", "{not json").unwrap_err(),
            EventSerdeError::Json(_)
        ));
        let bad_ts = EventRow {
            id: None,
            run_id: "r".into(),
            attempt_number: None,
            ts: "soon".into(),
            kind: "unblocked".into(),
            payload_json: "{}".into(),
            sequence: None,
        };
        assert!(matches!(
            event_from_row(&bad_ts).unwrap_err(),
            EventSerdeError::Timestamp(_)
        ));
    }

    #[test]
    fn whole_event_json_carries_kind_and_columns() {
        let event =
            DomainEvent::new("run-1", at(0), DomainEventPayload::Unblocked).with_attempt_number(2);
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            json!({
                "run_id": "run-1",
                "ts": "2026-05-28T20:26:40+00:00",
                "attempt_number": 2,
                "sequence": null,
                "id": null,
                "kind": "unblocked"
            })
        );
        assert_eq!(serde_json::from_value::<DomainEvent>(value).unwrap(), event);
    }
}
