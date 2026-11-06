//! Autopilot decisions as audit-ledger rows.
//!
//! Every autopilot pass records one [`KIND_CYCLE`] row (its outcome, tier
//! verdicts, counts, and -- for a failed pass -- the error) plus one
//! [`KIND_DECISION`] row per finding it emitted or dropped (with the
//! reason). An approve / deny of a gate autopilot's work waits on records
//! [`KIND_APPROVE`] / [`KIND_DENY`]. Rows go to the core store's
//! append-only `audit_log` ([`LedgerStore`]); the daemon's activity file
//! stays the liveness record only.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use flywheel_core::store::{JsonObject, LedgerEntry, LedgerStore, StoreError};

use crate::autopilot::AutopilotPassResult;
use crate::autopilot_activity::{
    CycleSummary, DISCOVERY_NOT_CONSULTED, DroppedSummary, EmittedSummary,
};

/// One pass: outcome, tier verdicts, counts.
pub const KIND_CYCLE: &str = "autopilot.cycle";
/// One finding emitted as a task or dropped, with the reason.
pub const KIND_DECISION: &str = "autopilot.decision";
/// An autopilot gate approved.
pub const KIND_APPROVE: &str = "autopilot.approve";
/// An autopilot gate denied.
pub const KIND_DENY: &str = "autopilot.deny";
/// The prefix every autopilot ledger kind shares.
pub const KIND_PREFIX: &str = "autopilot.";

/// How the pass was driven (`--once`, or a daemon cycle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassMode {
    Once,
    Daemon,
}

impl PassMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Daemon => "daemon",
        }
    }
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// The `autopilot.cycle` payload of a completed pass.
pub fn cycle_payload(cycle: i64, mode: PassMode, result: &AutopilotPassResult) -> JsonObject {
    let tiers = |tiers: &[crate::autopilot::Tier]| -> Vec<u8> {
        tiers.iter().map(|t| u8::from(*t)).collect()
    };
    object(json!({
        "cycle": cycle,
        "mode": mode.as_str(),
        "outcome": "ok",
        "reason": result.reason,
        "emitted": result.emitted_count(),
        "dropped": result.dropped.len(),
        "queue_depth_before": result.queue_depth_before,
        "target_depth": result.target_depth,
        "landing": result.landing,
        "relevant_tiers": tiers(&result.relevant_tiers),
        "not_relevant_tiers": tiers(&result.not_relevant_tiers),
        "prefiltered": result.prefiltered.iter().map(|s| json!({
            "tier": u8::from(s.tier),
            "reason": s.reason,
        })).collect::<Vec<_>>(),
        "discovery_cache": result.discovery_cache.to_string(),
        "emitted_tasks": result.emitted.iter().map(|e| json!({
            "task_id": e.task.id,
            "tier": u8::from(e.finding.tier),
        })).collect::<Vec<_>>(),
        "dropped_findings": result.dropped.iter().map(|d| json!({
            "finding_id": d.finding.id,
            "tier": u8::from(d.finding.tier),
            "title": d.finding.title,
            "reason": d.reason,
        })).collect::<Vec<_>>(),
        "already_authored": result.already_authored,
        "notes": result.notes,
    }))
}

/// Every row one completed pass records: the cycle row, then one decision
/// row per emitted task and per dropped finding.
pub fn pass_entries(
    cycle: i64,
    mode: PassMode,
    result: &AutopilotPassResult,
    at: DateTime<Utc>,
) -> Vec<LedgerEntry> {
    let mut entries = vec![LedgerEntry::new(
        KIND_CYCLE,
        at,
        cycle_payload(cycle, mode, result),
    )];
    for emitted in &result.emitted {
        entries.push(LedgerEntry::new(
            KIND_DECISION,
            at,
            object(json!({
                "cycle": cycle,
                "decision": "emitted",
                "finding_id": emitted.finding.id,
                "tier": u8::from(emitted.finding.tier),
                "title": emitted.finding.title,
                "task_id": emitted.task.id,
                "reason": format!(
                    "compiled to task {} with grader {} ({})",
                    emitted.task.id, emitted.authoritative_grader, emitted.grader_target
                ),
            })),
        ));
    }
    for dropped in &result.dropped {
        entries.push(LedgerEntry::new(
            KIND_DECISION,
            at,
            object(json!({
                "cycle": cycle,
                "decision": "dropped",
                "finding_id": dropped.finding.id,
                "tier": u8::from(dropped.finding.tier),
                "title": dropped.finding.title,
                "reason": dropped.reason,
            })),
        ));
    }
    entries
}

/// The `autopilot.cycle` row of a pass that failed.
pub fn failed_cycle_entry(
    cycle: i64,
    mode: PassMode,
    error: &str,
    error_type: &str,
    at: DateTime<Utc>,
) -> LedgerEntry {
    LedgerEntry::new(
        KIND_CYCLE,
        at,
        object(json!({
            "cycle": cycle,
            "mode": mode.as_str(),
            "outcome": "error",
            "error": error,
            "error_type": error_type,
            "emitted": 0,
            "dropped": 0,
        })),
    )
}

/// Append a completed pass's rows; returns how many were written.
pub fn record_pass(
    store: &dyn LedgerStore,
    cycle: i64,
    mode: PassMode,
    result: &AutopilotPassResult,
    at: DateTime<Utc>,
) -> Result<usize, StoreError> {
    let entries = pass_entries(cycle, mode, result, at);
    for entry in &entries {
        store.append_ledger_entry(entry)?;
    }
    Ok(entries.len())
}

/// Append a failed pass's cycle row.
pub fn record_failed_pass(
    store: &dyn LedgerStore,
    cycle: i64,
    mode: PassMode,
    error: &str,
    error_type: &str,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    store
        .append_ledger_entry(&failed_cycle_entry(cycle, mode, error, error_type, at))
        .map(|_| ())
}

/// The [`CycleSummary`] an `autopilot.cycle` row describes (the shape the
/// activity record and the hub's view share), or `None` for any other
/// kind. A failed pass reads as a cycle that decided nothing, its reason
/// naming the error.
pub fn cycle_summary(entry: &LedgerEntry) -> Option<CycleSummary> {
    if entry.kind != KIND_CYCLE {
        return None;
    }
    let p = &entry.payload;
    let int = |key: &str| p.get(key).and_then(Value::as_i64).unwrap_or(0);
    let text = |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or("").to_owned();
    let tier = |value: Option<&Value>| {
        value
            .and_then(Value::as_u64)
            .and_then(|t| u8::try_from(t).ok())
            .unwrap_or(0)
    };
    let list = |key: &str| {
        p.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let tiers = |key: &str| list(key).iter().map(|t| tier(Some(t))).collect::<Vec<u8>>();
    let failed = p.get("outcome").and_then(Value::as_str) == Some("error");
    let reason = if failed {
        format!(
            "cycle failed ({}): {}",
            text(p.get("error_type")),
            text(p.get("error"))
        )
    } else {
        text(p.get("reason"))
    };
    Some(CycleSummary {
        cycle: int("cycle"),
        at: entry.at.timestamp_micros() as f64 / 1_000_000.0,
        emitted: int("emitted"),
        dropped: int("dropped"),
        reason,
        relevant_tiers: tiers("relevant_tiers"),
        emitted_tasks: list("emitted_tasks")
            .iter()
            .map(|e| EmittedSummary {
                task_id: text(e.get("task_id")),
                tier: tier(e.get("tier")),
            })
            .collect(),
        prefiltered_tiers: list("prefiltered")
            .iter()
            .map(|s| tier(s.get("tier")))
            .collect(),
        discovery_cache: if failed {
            DISCOVERY_NOT_CONSULTED.to_owned()
        } else {
            text(p.get("discovery_cache"))
        },
        dropped_findings: list("dropped_findings")
            .iter()
            .map(|d| {
                DroppedSummary::new(
                    &text(d.get("finding_id")),
                    tier(d.get("tier")),
                    &text(d.get("title")),
                    &text(d.get("reason")),
                )
            })
            .collect(),
    })
}

/// One gate decision on autopilot's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDecisionRecord<'a> {
    pub approve: bool,
    pub run_id: &'a str,
    pub task_id: &'a str,
    pub gate: u32,
    pub attempt: u32,
    pub command_id: i64,
    /// Who decided (`hub`, `cli`, ...).
    pub actor: &'a str,
    pub with_feedback: bool,
}

/// The `autopilot.approve` / `autopilot.deny` row of `decision` (the
/// feedback text itself is not copied: it rides the control command).
pub fn gate_decision_entry(decision: &GateDecisionRecord<'_>, at: DateTime<Utc>) -> LedgerEntry {
    let kind = if decision.approve {
        KIND_APPROVE
    } else {
        KIND_DENY
    };
    LedgerEntry::new(
        kind,
        at,
        object(json!({
            "task_id": decision.task_id,
            "gate": decision.gate,
            "attempt": decision.attempt,
            "command_id": decision.command_id,
            "actor": decision.actor,
            "with_feedback": decision.with_feedback,
        })),
    )
    .with_run_id(decision.run_id)
}

/// Append the row of one gate decision.
pub fn record_gate_decision(
    store: &dyn LedgerStore,
    decision: &GateDecisionRecord<'_>,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    store
        .append_ledger_entry(&gate_decision_entry(decision, at))
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::{DroppedFinding, EmittedTask, Finding, GraderSource, Tier};
    use flywheel_core::store::LedgerFilter;
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::Task;

    fn result() -> AutopilotPassResult {
        let finding = Finding::new("f-1", Tier::try_from(5).unwrap(), "fix the parser");
        let task: Task = serde_json::from_value(json!({
            "id": "autopilot-fix-parser",
            "goal": "fix it",
            "graders": [{"type": "command", "run": "true"}],
        }))
        .unwrap();
        AutopilotPassResult {
            emitted_paths: vec!["a.json".into()],
            emitted: vec![EmittedTask::new(
                finding,
                task,
                "cargo test parser",
                GraderSource::RepoCommand,
                "src/parser.rs",
            )],
            dropped: vec![DroppedFinding {
                finding: Finding::new("f-2", Tier::try_from(7).unwrap(), "vague idea"),
                reason: "no checkable grader".into(),
            }],
            relevant_tiers: vec![Tier::try_from(5).unwrap()],
            reason: "emitted 1".into(),
            ..AutopilotPassResult::default()
        }
    }

    #[test]
    fn a_pass_records_its_cycle_and_one_row_per_finding() {
        let store = InMemoryStore::new();
        let at = Utc::now();
        let written = record_pass(&store, 3, PassMode::Daemon, &result(), at).unwrap();
        assert_eq!(written, 3);
        let rows = store
            .list_ledger_entries(&LedgerFilter::kind_prefix(KIND_PREFIX))
            .unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, [KIND_CYCLE, KIND_DECISION, KIND_DECISION]);
        assert_eq!(rows[0].payload["emitted"], 1);
        assert_eq!(rows[0].payload["dropped"], 1);
        assert_eq!(rows[0].payload["relevant_tiers"], json!([5]));
        assert_eq!(rows[0].payload["cycle"], 3);
        assert_eq!(rows[1].payload["decision"], "emitted");
        assert_eq!(rows[1].payload["task_id"], "autopilot-fix-parser");
        assert_eq!(rows[2].payload["decision"], "dropped");
        assert_eq!(rows[2].payload["reason"], "no checkable grader");
    }

    #[test]
    fn a_cycle_row_reads_back_as_the_summary_the_views_share() {
        let at = Utc::now();
        let entries = pass_entries(4, PassMode::Daemon, &result(), at);
        let summary = cycle_summary(&entries[0]).unwrap();
        assert_eq!(summary.cycle, 4);
        assert_eq!((summary.emitted, summary.dropped), (1, 1));
        assert_eq!(summary.relevant_tiers, vec![5]);
        assert_eq!(summary.emitted_tasks[0].task_id, "autopilot-fix-parser");
        assert_eq!(summary.dropped_findings[0].reason, "no checkable grader");
        assert_eq!(summary.reason, "emitted 1");
        assert!(summary.decided());
        assert!(cycle_summary(&entries[1]).is_none());

        let failed = failed_cycle_entry(5, PassMode::Daemon, "boom", "CycleError", at);
        let summary = cycle_summary(&failed).unwrap();
        assert_eq!(summary.reason, "cycle failed (CycleError): boom");
        assert!(!summary.decided());
    }

    #[test]
    fn a_failed_pass_and_a_gate_decision_are_rows_too() {
        let store = InMemoryStore::new();
        let at = Utc::now();
        record_failed_pass(&store, 1, PassMode::Once, "boom", "CycleError", at).unwrap();
        record_gate_decision(
            &store,
            &GateDecisionRecord {
                approve: false,
                run_id: "run-1",
                task_id: "t",
                gate: 0,
                attempt: 2,
                command_id: 9,
                actor: "hub",
                with_feedback: true,
            },
            at,
        )
        .unwrap();
        let rows = store.list_ledger_entries(&LedgerFilter::default()).unwrap();
        assert_eq!(rows[0].payload["outcome"], "error");
        assert_eq!(rows[0].payload["error"], "boom");
        assert_eq!(rows[1].kind, KIND_DENY);
        assert_eq!(rows[1].run_id.as_deref(), Some("run-1"));
        assert_eq!(rows[1].payload["attempt"], 2);
    }
}
