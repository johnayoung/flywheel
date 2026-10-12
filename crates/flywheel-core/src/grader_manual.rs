//! Manual grader gate-selection and receipt assembly.
//!
//! Pure module that supports the human-approval gate
//! (`.flywheel/specs/00016-FEATURE-manual-grader-approval-gate.md`). The
//! harness/resolver layer drives IO; this module only:
//!
//! * Selects the next pending [`ManualGrader`] on a [`Task`] as a small
//!   [`ManualGate`] snapshot preserving the grader's index in
//!   `task.graders` as its ordinal.
//! * Assembles a [`GraderResultRecord`] for an approve or reject decision
//!   so the resolver can append it via the store contract.
//!
//! Manual graders do not run a subprocess and have no live attempt clock of
//! their own (the attempt is already finalized `SUCCEEDED` by the time the
//! gate is reached), so `duration_ms` is always `0` on manual receipts.
//!
//! The receipt payload is the documented manual shape from the schema
//! header -- `{operator_id, decision, notes, decided_at}` -- plus the
//! gate's `instruction` and a `summary` alias of `notes`, so rows written
//! here read back through the same `summary` key the Python harness and
//! prompt renderer consume (`ManualFinding`).

use crate::event_serde::format_timestamp;
use crate::store::{GraderResultRecord, JsonObject};
use crate::task::{Grader, GraderType, Task};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

/// The feedback text substituted when a reject carries no operator notes.
pub const NO_FEEDBACK_PLACEHOLDER: &str = "(no feedback provided)";

/// The notes text recorded for an approve that carries no operator notes.
pub const APPROVED_NOTES: &str = "approved";

/// A pending manual-approval gate snapshot.
///
/// `ordinal` is the gate's index in `task.graders` (preserved across
/// non-manual neighbors so audits can reconstruct the original list from
/// receipts alone). `instruction` is the operator-facing instruction text
/// from the underlying [`ManualGrader`]. `grader_name` is the grader's
/// optional human label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualGate {
    pub ordinal: u32,
    pub instruction: String,
    #[serde(default)]
    pub grader_name: Option<String>,
}

/// An operator's verdict on a manual gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualDecision {
    Approve,
    Reject,
}

impl ManualDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            ManualDecision::Approve => "approve",
            ManualDecision::Reject => "reject",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(ManualDecision::Approve),
            "reject" => Some(ManualDecision::Reject),
            _ => None,
        }
    }

    /// The `passed` flag a receipt with this decision carries.
    pub const fn passed(self) -> bool {
        matches!(self, ManualDecision::Approve)
    }
}

impl fmt::Display for ManualDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Return the first manual gate in `task.graders` strictly after
/// `after_ordinal`.
///
/// When `after_ordinal` is `None` the search returns the first manual gate
/// at any index. Non-manual graders are skipped while their indices stay
/// aligned to `task.graders` -- the returned [`ManualGate::ordinal`] is the
/// grader's literal position in the list, not the count of manual graders
/// preceding it. Returns `None` when no manual gate remains.
pub fn next_pending_manual_gate(task: &Task, after_ordinal: Option<u32>) -> Option<ManualGate> {
    task.graders
        .iter()
        .enumerate()
        .filter_map(|(index, grader)| match grader {
            Grader::Manual(manual) => Some((index, manual)),
            _ => None,
        })
        .map(|(index, manual)| {
            (
                u32::try_from(index).expect("grader list index fits in u32"),
                manual,
            )
        })
        .find(|(ordinal, _)| after_ordinal.is_none_or(|after| *ordinal > after))
        .map(|(ordinal, manual)| ManualGate {
            ordinal,
            instruction: manual.instruction.clone(),
            grader_name: manual.name.clone(),
        })
}

/// Snapshot the gate's grader as it appeared in the task at gate time.
pub fn grader_spec_snapshot(gate: &ManualGate) -> JsonObject {
    let mut spec = JsonObject::new();
    spec.insert("type".into(), json!("manual"));
    spec.insert("instruction".into(), json!(gate.instruction));
    if let Some(name) = &gate.grader_name {
        spec.insert("name".into(), json!(name));
    }
    spec
}

/// Build the documented manual payload
/// `{operator_id, decision, notes, decided_at}` plus `instruction` and the
/// `summary` alias of `notes`.
pub fn manual_payload(
    gate: &ManualGate,
    decision: ManualDecision,
    operator_id: &str,
    notes: &str,
    decided_at: DateTime<Utc>,
) -> JsonObject {
    let mut payload = JsonObject::new();
    payload.insert("operator_id".into(), json!(operator_id));
    payload.insert("decision".into(), json!(decision.as_str()));
    payload.insert("notes".into(), json!(notes));
    payload.insert("decided_at".into(), json!(format_timestamp(&decided_at)));
    payload.insert("instruction".into(), json!(gate.instruction));
    payload.insert("summary".into(), json!(notes));
    payload
}

/// Assemble a `grader_type="manual"` [`GraderResultRecord`].
///
/// The record is built but not persisted; the caller (the harness /
/// resolver) appends it via `GraderResultStore::append_grader_result`.
/// `passed` follows the decision (approve = `true`, reject = `false`).
/// `notes` is recorded verbatim -- use [`build_approve_result`] /
/// [`build_reject_result`] for the documented empty-notes substitutions.
/// `decided_at` is injected so the receipt's timestamp matches the
/// lifecycle event's timestamp deterministically; it is both the record
/// `ts` and the payload's `decided_at`.
pub fn build_manual_result(
    gate: &ManualGate,
    run_id: &str,
    attempt_number: u32,
    decision: ManualDecision,
    operator_id: &str,
    notes: &str,
    decided_at: DateTime<Utc>,
) -> GraderResultRecord {
    GraderResultRecord {
        run_id: run_id.to_owned(),
        attempt_number,
        ordinal: gate.ordinal,
        grader_type: GraderType::Manual,
        grader_spec: grader_spec_snapshot(gate),
        passed: decision.passed(),
        duration_ms: 0,
        payload: manual_payload(gate, decision, operator_id, notes, decided_at),
        ts: decided_at,
        grader_name: gate.grader_name.clone(),
        id: None,
    }
}

/// An operator approve receipt. Blank `notes` are recorded as
/// [`APPROVED_NOTES`].
pub fn build_approve_result(
    gate: &ManualGate,
    run_id: &str,
    attempt_number: u32,
    operator_id: &str,
    notes: &str,
    decided_at: DateTime<Utc>,
) -> GraderResultRecord {
    let notes = if notes.trim().is_empty() {
        APPROVED_NOTES
    } else {
        notes
    };
    build_manual_result(
        gate,
        run_id,
        attempt_number,
        ManualDecision::Approve,
        operator_id,
        notes,
        decided_at,
    )
}

/// An operator reject receipt. Blank `notes` are recorded as
/// [`NO_FEEDBACK_PLACEHOLDER`], the documented substitution the resolver
/// applies when the reject payload omits feedback.
pub fn build_reject_result(
    gate: &ManualGate,
    run_id: &str,
    attempt_number: u32,
    operator_id: &str,
    notes: &str,
    decided_at: DateTime<Utc>,
) -> GraderResultRecord {
    let notes = if notes.trim().is_empty() {
        NO_FEEDBACK_PLACEHOLDER
    } else {
        notes
    };
    build_manual_result(
        gate,
        run_id,
        attempt_number,
        ManualDecision::Reject,
        operator_id,
        notes,
        decided_at,
    )
}

/// The operator feedback carried by a persisted manual receipt: `notes`,
/// falling back to the `summary` key of rows written by the Python
/// implementation. `None` when neither is a string.
pub fn receipt_notes(payload: &JsonObject) -> Option<&str> {
    payload
        .get("notes")
        .or_else(|| payload.get("summary"))
        .and_then(Value::as_str)
}

/// The decision recorded on a persisted manual receipt. Falls back to the
/// record's `passed` flag for rows without the `decision` key.
pub fn receipt_decision(record: &GraderResultRecord) -> ManualDecision {
    record
        .payload
        .get("decision")
        .and_then(Value::as_str)
        .and_then(ManualDecision::parse)
        .unwrap_or(if record.passed {
            ManualDecision::Approve
        } else {
            ManualDecision::Reject
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{CommandGrader, ManualGrader, RubricGrader};

    fn manual(instruction: &str, name: Option<&str>) -> Grader {
        Grader::Manual(ManualGrader {
            instruction: instruction.into(),
            name: name.map(str::to_owned),
        })
    }

    fn rubric() -> Grader {
        Grader::Rubric(RubricGrader::new(vec!["x".into()]).unwrap())
    }

    fn command() -> Grader {
        Grader::Command(CommandGrader::new("true").unwrap())
    }

    fn gate(ordinal: u32, name: Option<&str>) -> ManualGate {
        ManualGate {
            ordinal,
            instruction: "confirm migration is safe".into(),
            grader_name: name.map(str::to_owned),
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_714_564_800 + secs, 0).unwrap()
    }

    // --- next_pending_manual_gate ------------------------------------------

    #[test]
    fn first_gate_when_after_ordinal_is_none() {
        let task = Task::new(
            "g",
            vec![
                manual("confirm A", Some("gate-a")),
                manual("confirm B", None),
            ],
        );
        assert_eq!(
            next_pending_manual_gate(&task, None),
            Some(ManualGate {
                ordinal: 0,
                instruction: "confirm A".into(),
                grader_name: Some("gate-a".into()),
            })
        );
    }

    #[test]
    fn next_gate_strictly_after_given_ordinal() {
        let task = Task::new(
            "g",
            vec![
                manual("a", Some("gate-a")),
                manual("b", Some("gate-b")),
                manual("c", Some("gate-c")),
            ],
        );
        let g = next_pending_manual_gate(&task, Some(0)).unwrap();
        assert_eq!((g.ordinal, g.grader_name.as_deref()), (1, Some("gate-b")));
        let g = next_pending_manual_gate(&task, Some(1)).unwrap();
        assert_eq!((g.ordinal, g.grader_name.as_deref()), (2, Some("gate-c")));
        assert_eq!(next_pending_manual_gate(&task, Some(2)), None);
    }

    #[test]
    fn none_when_no_gate_remains() {
        let task = Task::new("g", vec![manual("a", None), rubric()]);
        assert_eq!(next_pending_manual_gate(&task, Some(1)), None);
        let no_manual = Task::new("g", vec![command(), rubric()]);
        assert_eq!(next_pending_manual_gate(&no_manual, None), None);
        assert_eq!(next_pending_manual_gate(&no_manual, Some(0)), None);
        let empty = Task::new("g", vec![]);
        assert_eq!(next_pending_manual_gate(&empty, None), None);
        assert_eq!(next_pending_manual_gate(&empty, Some(42)), None);
    }

    #[test]
    fn skips_non_manual_graders_while_preserving_ordinals() {
        let task = Task::new(
            "g",
            vec![
                command(),
                rubric(),
                command(),
                manual("confirm A", Some("gate-a")),
                rubric(),
                manual("confirm B", Some("gate-b")),
            ],
        );
        let first = next_pending_manual_gate(&task, None).unwrap();
        assert_eq!(first.ordinal, 3);
        assert_eq!(first.grader_name.as_deref(), Some("gate-a"));
        assert_eq!(first.instruction, "confirm A");
        let second = next_pending_manual_gate(&task, Some(first.ordinal)).unwrap();
        assert_eq!(second.ordinal, 5);
        assert_eq!(second.grader_name.as_deref(), Some("gate-b"));
        assert_eq!(second.instruction, "confirm B");
        assert_eq!(next_pending_manual_gate(&task, Some(second.ordinal)), None);
    }

    #[test]
    fn manual_gate_without_name_carries_none() {
        let task = Task::new("g", vec![manual("confirm", None)]);
        assert_eq!(
            next_pending_manual_gate(&task, None),
            Some(ManualGate {
                ordinal: 0,
                instruction: "confirm".into(),
                grader_name: None,
            })
        );
    }

    // --- build_manual_result -----------------------------------------------

    #[test]
    fn approve_receipt_has_documented_payload_and_spec() {
        let g = gate(2, Some("confirm-migration"));
        let decided = ts(0);
        let record = build_approve_result(&g, "run-1", 1, "alice", "looks good", decided);
        assert_eq!(record.run_id, "run-1");
        assert_eq!(record.attempt_number, 1);
        assert_eq!(record.ordinal, 2);
        assert_eq!(record.grader_type, GraderType::Manual);
        assert_eq!(record.grader_name.as_deref(), Some("confirm-migration"));
        assert!(record.passed);
        assert_eq!(record.duration_ms, 0);
        assert_eq!(record.ts, decided);
        assert_eq!(record.id, None);
        assert_eq!(
            Value::Object(record.grader_spec.clone()),
            json!({"type": "manual", "instruction": "confirm migration is safe", "name": "confirm-migration"})
        );
        assert_eq!(
            Value::Object(record.payload.clone()),
            json!({
                "operator_id": "alice",
                "decision": "approve",
                "notes": "looks good",
                "decided_at": "2024-05-01T12:00:00+00:00",
                "instruction": "confirm migration is safe",
                "summary": "looks good",
            })
        );
        assert_eq!(receipt_notes(&record.payload), Some("looks good"));
        assert_eq!(receipt_decision(&record), ManualDecision::Approve);
    }

    #[test]
    fn approve_without_notes_records_approved() {
        let record = build_approve_result(&gate(0, None), "run-1", 1, "alice", "  ", ts(0));
        assert_eq!(record.payload["notes"], APPROVED_NOTES);
        assert_eq!(record.payload["summary"], APPROVED_NOTES);
    }

    #[test]
    fn reject_receipt_carries_feedback_verbatim() {
        let g = gate(4, Some("confirm-migration"));
        let feedback = "The migration drops a column still read by the billing service. \
                        Gate it behind a feature flag first.";
        let record = build_reject_result(&g, "run-2", 3, "bob", feedback, ts(77_400));
        assert!(!record.passed);
        assert_eq!(record.grader_type, GraderType::Manual);
        assert_eq!(record.attempt_number, 3);
        assert_eq!(record.ordinal, 4);
        assert_eq!(record.duration_ms, 0);
        assert_eq!(record.ts, ts(77_400));
        assert_eq!(record.payload["decision"], "reject");
        assert_eq!(record.payload["operator_id"], "bob");
        assert_eq!(record.payload["notes"], feedback);
        assert_eq!(record.payload["summary"], feedback);
        assert_eq!(record.payload["instruction"], "confirm migration is safe");
        assert_eq!(record.payload["decided_at"], "2024-05-02T09:30:00+00:00");
        assert_eq!(receipt_decision(&record), ManualDecision::Reject);
    }

    #[test]
    fn reject_without_feedback_substitutes_placeholder() {
        let record = build_reject_result(&gate(0, None), "run-1", 1, "bob", "", ts(0));
        assert_eq!(record.payload["notes"], NO_FEEDBACK_PLACEHOLDER);
        assert_eq!(
            receipt_notes(&record.payload),
            Some(NO_FEEDBACK_PLACEHOLDER)
        );
    }

    #[test]
    fn build_manual_result_keeps_notes_verbatim() {
        let record = build_manual_result(
            &gate(0, None),
            "run-1",
            1,
            ManualDecision::Reject,
            "bob",
            "",
            ts(0),
        );
        assert_eq!(record.payload["notes"], "");
        assert!(!record.passed);
    }

    #[test]
    fn unnamed_gate_omits_name_from_spec() {
        let g = ManualGate {
            ordinal: 0,
            instruction: "confirm".into(),
            grader_name: None,
        };
        let record = build_approve_result(&g, "run-3", 1, "alice", "ok", ts(0));
        assert_eq!(
            Value::Object(record.grader_spec.clone()),
            json!({"type": "manual", "instruction": "confirm"})
        );
        assert_eq!(record.grader_name, None);
    }

    #[test]
    fn receipt_readers_understand_python_written_rows() {
        let mut record = build_approve_result(&gate(0, None), "run-1", 1, "alice", "ok", ts(0));
        record.payload = json!({"instruction": "confirm", "summary": "legacy feedback"})
            .as_object()
            .unwrap()
            .clone();
        record.passed = false;
        assert_eq!(receipt_notes(&record.payload), Some("legacy feedback"));
        assert_eq!(receipt_decision(&record), ManualDecision::Reject);
        assert_eq!(receipt_notes(&JsonObject::new()), None);
    }

    #[test]
    fn decision_round_trips() {
        for d in [ManualDecision::Approve, ManualDecision::Reject] {
            assert_eq!(serde_json::to_value(d).unwrap(), json!(d.as_str()));
            assert_eq!(ManualDecision::parse(d.as_str()), Some(d));
            assert_eq!(d.to_string(), d.as_str());
        }
        assert_eq!(ManualDecision::parse("maybe"), None);
        assert!(ManualDecision::Approve.passed());
        assert!(!ManualDecision::Reject.passed());
    }
}
