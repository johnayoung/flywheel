//! The `status` verb: one line per active task plus the stranded surface
//! (parked landings, pre-run stops, store-backed landing strands), the
//! `--json` projection, and the `--rollup` evidence-derived view.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use flywheel_core::events::{
    DomainEventKind, DomainEventPayload, LANDING_STRAND_KINDS, LandingParked,
};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{HarnessStore, LifecycleFilter, StoreError};
use flywheel_core::task::Grader;
use serde_json::{Map, Value, json};

use crate::claims::{
    ClaimStore, OrchestratorStopEventRecord, RESOLUTION_ATTRIBUTIONS, STOP_RESOLVED,
};
use crate::rollup::{build_rollup, render_rollup_text, rollup_to_json};
use crate::sources::WorkItem;

use super::args::StatusArgs;
use super::select::{StatusRow, reachability_held_prerequisites, snapshots, status_rows};
use super::{WorkflowError, py_json_dumps_indent2, python_path_str};

/// A `LandingParked` event with its ledger timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct Park {
    pub ts: DateTime<Utc>,
    pub park_kind: String,
    pub detail: String,
}

impl Park {
    fn from_event(ts: DateTime<Utc>, parked: &LandingParked) -> Self {
        Self {
            ts,
            park_kind: parked.park_kind.clone(),
            detail: parked.detail.clone(),
        }
    }
}

/// Decode `blocked_requires_json` into its predicate objects; `None` when
/// absent or not a JSON list (non-object entries are dropped).
pub fn parse_blocked_requires(payload: Option<&str>) -> Option<Vec<Map<String, Value>>> {
    let payload = payload?;
    let data: Value = serde_json::from_str(payload).ok()?;
    let Value::Array(entries) = data else {
        return None;
    };
    Some(
        entries
            .into_iter()
            .filter_map(|entry| match entry {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .collect(),
    )
}

/// The persisted predicate's primary identifier (`name` for
/// `command_grader` / `env_var_set`, `path` for `file_exists`), `?` when
/// absent or of an unknown type.
fn predicate_identifier(predicate: &Map<String, Value>) -> String {
    let key = match predicate.get("type").and_then(Value::as_str) {
        Some("command_grader") | Some("env_var_set") => "name",
        Some("file_exists") => "path",
        _ => return "?".to_owned(),
    };
    match predicate.get(key).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => value.to_owned(),
        _ => "?".to_owned(),
    }
}

fn type_label(predicate: &Map<String, Value>) -> String {
    match predicate.get("type").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => t.to_owned(),
        _ => "?".to_owned(),
    }
}

/// Render the persisted requires list as `type=identifier, ...`.
pub fn format_blocked_on(predicates: &[Map<String, Value>]) -> String {
    predicates
        .iter()
        .map(|p| format!("{}={}", type_label(p), predicate_identifier(p)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The pending manual gate's instruction for a row parked on
/// AWAITING_APPROVAL whose ordinal addresses a manual grader.
pub fn awaiting_instruction_for_row(row: &StatusRow) -> Option<&str> {
    if row.status.latest_status != Some(Status::AwaitingApproval) {
        return None;
    }
    let ordinal = row.status.awaiting_manual_ordinal? as usize;
    match row.item.task.graders.get(ordinal) {
        Some(Grader::Manual(grader)) => Some(grader.instruction.as_str()),
        _ => None,
    }
}

/// The most recent `LandingParked` on `run_id`, or `None`.
fn landing_park_for_run(
    store: &dyn HarnessStore,
    run_id: &str,
) -> Result<Option<Park>, StoreError> {
    Ok(store
        .latest_domain_event_of_kind(run_id, DomainEventKind::LandingParked)?
        .and_then(|event| match &event.payload {
            DomainEventPayload::LandingParked(parked) => Some(Park::from_event(event.ts, parked)),
            _ => None,
        }))
}

/// The landing strand `task_id` still surfaces on: the park on its latest
/// DONE run when that park is a strand kind and no attributed resolution
/// marker postdates it -- the same rule `status` applies, so `resolve` can
/// clear exactly what `status` shows.
pub(crate) fn unresolved_landing_strand(
    store: &dyn HarnessStore,
    stop_events: &[OrchestratorStopEventRecord],
    task_id: &str,
) -> Result<Option<Park>, StoreError> {
    let filter = LifecycleFilter::default()
        .with_task_id(task_id)
        .with_statuses([Status::Done]);
    let Some(latest_done) = store.list_lifecycles(&filter)?.into_iter().next() else {
        return Ok(None);
    };
    let Some(park) = landing_park_for_run(store, &latest_done.run_id)? else {
        return Ok(None);
    };
    if !LANDING_STRAND_KINDS.contains(&park.park_kind.as_str()) {
        return Ok(None);
    }
    let (_, resolved_at) = fold_stop_events(stop_events);
    Ok(resolved_at
        .get(task_id)
        .is_none_or(|resolution| park.ts > *resolution)
        .then_some(park))
}

/// The pre-run stop ledger folded two ways in one pass: the latest
/// unresolved stop per subject (a `stop-resolved` row of ANY attribution
/// clears the subject; only a later stop re-surfaces it), and the latest
/// *attributed* resolution instant per subject (the only markers that clear
/// a landing strand).
fn fold_stop_events(
    events: &[OrchestratorStopEventRecord],
) -> (
    BTreeMap<String, OrchestratorStopEventRecord>,
    BTreeMap<String, DateTime<Utc>>,
) {
    let mut latest: BTreeMap<String, OrchestratorStopEventRecord> = BTreeMap::new();
    let mut resolved_at: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    for event in events {
        if event.kind == STOP_RESOLVED {
            latest.remove(&event.subject);
            if RESOLUTION_ATTRIBUTIONS.contains(&event.attribution.as_str()) {
                resolved_at.insert(event.subject.clone(), event.occurred_at);
            }
            continue;
        }
        latest.insert(event.subject.clone(), event.clone());
    }
    (latest, resolved_at)
}

/// Persisted worker ids of the in-flight runs (RUNNING / VALIDATING /
/// AWAITING_APPROVAL), keyed by run id; an unset id is `None`.
fn live_worker_ids(
    store: &dyn HarnessStore,
) -> Result<BTreeMap<String, Option<String>>, StoreError> {
    let filter = LifecycleFilter::default().with_statuses([
        Status::Running,
        Status::Validating,
        Status::AwaitingApproval,
    ]);
    Ok(store
        .list_lifecycles(&filter)?
        .into_iter()
        .map(|lc| {
            let worker = (!lc.worker_id.is_empty()).then_some(lc.worker_id);
            (lc.run_id, worker)
        })
        .collect())
}

fn stop_json(stop: &OrchestratorStopEventRecord) -> Value {
    json!({"kind": stop.kind, "detail": stop.detail})
}

fn park_json(park: &Park) -> Value {
    json!({"park_kind": park.park_kind, "detail": park.detail})
}

fn detail_suffix(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(" -- {detail}")
    }
}

/// The phase label a row renders under: the task file's directory, else the
/// source ref, else `external`.
fn phase_label(row: &StatusRow) -> String {
    let dir_name = row
        .task_file()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty());
    if let Some(name) = dir_name {
        return name;
    }
    if !row.item.source_ref.is_empty() {
        return row.item.source_ref.clone();
    }
    "external".to_owned()
}

fn cmd_status_rollup(
    args: &StatusArgs,
    rows: &[StatusRow],
    store: &dyn HarnessStore,
    repo_root: Option<&Path>,
    true_base: Option<&str>,
    out: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let plain = snapshots(rows);
    let holds = reachability_held_prerequisites(&plain, repo_root, true_base);
    let rollup = build_rollup(&plain, store, &holds)?;
    if args.json {
        writeln!(out, "{}", py_json_dumps_indent2(&rollup_to_json(&rollup)))?;
    } else {
        writeln!(out, "{}", render_rollup_text(&rollup))?;
    }
    Ok(0)
}

/// Print the state of every active task (text or `--json`), unioned with
/// the stranded surface: a parked DONE run renders `stranded:`, a pre-run
/// stop keyed to a task renders `stopped:`, and subjects with no active row
/// (source-level stops, strands whose task file left the listing) render as
/// their own `[no active row]` entries. An attributed resolution marker
/// clears a strand; a park appended after it is a fresh recurrence.
pub fn cmd_status(
    args: &StatusArgs,
    items: &[WorkItem],
    store: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    repo_root: Option<&Path>,
    true_base: Option<&str>,
    out: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let rows = status_rows(items, store)?;
    if args.rollup {
        return cmd_status_rollup(args, &rows, store, repo_root, true_base, out);
    }
    let worker_ids = live_worker_ids(store)?;

    // One ledger read per DONE run, shared by the per-row annotation and
    // the store-backed strand surface (the Python reads each run twice).
    let mut park_by_run: BTreeMap<String, Option<Park>> = BTreeMap::new();
    let mut park_for = |run_id: &str| -> Result<Option<Park>, StoreError> {
        if let Some(cached) = park_by_run.get(run_id) {
            return Ok(cached.clone());
        }
        let park = landing_park_for_run(store, run_id)?;
        park_by_run.insert(run_id.to_owned(), park.clone());
        Ok(park)
    };
    let mut parked_landings: BTreeMap<String, Park> = BTreeMap::new();
    for row in &rows {
        if let (Some(run_id), Some(Status::Done)) =
            (&row.status.latest_run_id, row.status.latest_status)
            && let Some(park) = park_for(run_id)?
        {
            parked_landings.insert(run_id.clone(), park);
        }
    }
    // The latest DONE run per task across the whole store, so a strand
    // outlives the active listing (spec 00077, criterion 6).
    let mut landing_strands: BTreeMap<String, Park> = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let done = store.list_lifecycles(&LifecycleFilter::default().with_statuses([Status::Done]))?;
    for lifecycle in done {
        if !seen.insert(lifecycle.task_id.clone()) {
            continue;
        }
        if let Some(park) = park_for(&lifecycle.run_id)?
            && LANDING_STRAND_KINDS.contains(&park.park_kind.as_str())
        {
            landing_strands.insert(lifecycle.task_id.clone(), park);
        }
    }
    let (stops_by_subject, resolved_at) = fold_stop_events(&claims.list_stop_events()?);
    let listed_ids: BTreeSet<&str> = rows.iter().map(StatusRow::task_id).collect();
    let rowless_strands: BTreeMap<&String, &Park> = landing_strands
        .iter()
        .filter(|(subject, park)| {
            !listed_ids.contains(subject.as_str())
                && resolved_at
                    .get(subject.as_str())
                    .is_none_or(|resolution| park.ts > *resolution)
        })
        .collect();
    for row in &rows {
        let Some(run_id) = &row.status.latest_run_id else {
            continue;
        };
        let Some(park) = parked_landings.get(run_id) else {
            continue;
        };
        if let Some(resolution) = resolved_at.get(row.task_id())
            && park.ts <= *resolution
        {
            parked_landings.remove(run_id);
        }
    }

    if args.json {
        let mut entries: Vec<Value> = Vec::new();
        for row in &rows {
            let mut entry = Map::new();
            entry.insert("task_id".into(), json!(row.task_id()));
            entry.insert(
                "task_file".into(),
                json!(
                    row.task_file()
                        .map_or_else(|| ".".to_owned(), python_path_str)
                ),
            );
            entry.insert("source_ref".into(), json!(row.item.source_ref));
            entry.insert("state".into(), json!(row.state().as_str()));
            entry.insert("latest_run_id".into(), json!(row.status.latest_run_id));
            entry.insert(
                "latest_status".into(),
                json!(row.status.latest_status.map(Status::as_str)),
            );
            entry.insert("latest_error".into(), json!(row.status.latest_error));
            entry.insert("prerequisites".into(), json!(row.item.prerequisites));
            if let Some(Some(worker_id)) = row
                .status
                .latest_run_id
                .as_deref()
                .and_then(|run_id| worker_ids.get(run_id))
            {
                entry.insert("worker_id".into(), json!(worker_id));
            }
            if let Some(parsed) = parse_blocked_requires(row.status.blocked_requires.as_deref()) {
                entry.insert(
                    "blocked_requires".into(),
                    Value::Array(parsed.into_iter().map(Value::Object).collect()),
                );
            }
            if let Some(instruction) = awaiting_instruction_for_row(row) {
                entry.insert(
                    "awaiting_on".into(),
                    json!({"ordinal": row.status.awaiting_manual_ordinal, "instruction": instruction}),
                );
            }
            if let Some(park) = row
                .status
                .latest_run_id
                .as_deref()
                .and_then(|run_id| parked_landings.get(run_id))
            {
                entry.insert("stranded".into(), park_json(park));
            }
            if let Some(stop) = stops_by_subject.get(row.task_id()) {
                entry.insert("stopped".into(), stop_json(stop));
            }
            entries.push(Value::Object(entry));
        }
        for (subject, stop) in &stops_by_subject {
            if listed_ids.contains(subject.as_str()) {
                continue;
            }
            entries.push(json!({"subject": subject, "stopped": stop_json(stop)}));
        }
        for (subject, park) in &rowless_strands {
            entries.push(json!({"subject": subject, "stranded": park_json(park)}));
        }
        writeln!(out, "{}", py_json_dumps_indent2(&Value::Array(entries)))?;
        return Ok(0);
    }

    if rows.is_empty() && stops_by_subject.is_empty() && rowless_strands.is_empty() {
        writeln!(out, "(no active tasks)")?;
        return Ok(0);
    }
    let width = rows
        .iter()
        .map(|row| row.task_id().chars().count())
        .max()
        .unwrap_or(0);
    for row in &rows {
        let suffix = if row.status.latest_error.is_empty() {
            String::new()
        } else {
            format!("  -- {}", row.status.latest_error)
        };
        writeln!(
            out,
            "  {}/{:<width$}  {:<17}{suffix}",
            phase_label(row),
            row.task_id(),
            row.state().as_str(),
        )?;
        if row.status.latest_status == Some(Status::Interrupted)
            && let Some(parsed) = parse_blocked_requires(row.status.blocked_requires.as_deref())
            && !parsed.is_empty()
        {
            writeln!(out, "    blocked_on: {}", format_blocked_on(&parsed))?;
        }
        if let Some(instruction) = awaiting_instruction_for_row(row) {
            writeln!(out, "    awaiting_on: {instruction}")?;
        }
        if let Some(park) = row
            .status
            .latest_run_id
            .as_deref()
            .and_then(|run_id| parked_landings.get(run_id))
        {
            writeln!(
                out,
                "    stranded: {}{}",
                park.park_kind,
                detail_suffix(&park.detail)
            )?;
        }
        if let Some(stop) = stops_by_subject.get(row.task_id()) {
            writeln!(
                out,
                "    stopped: {}{}",
                stop.kind,
                detail_suffix(&stop.detail)
            )?;
        }
    }
    for (subject, stop) in &stops_by_subject {
        if listed_ids.contains(subject.as_str()) {
            continue;
        }
        writeln!(
            out,
            "  {subject} [no active row]  stopped: {}{}",
            stop.kind,
            detail_suffix(&stop.detail)
        )?;
    }
    for (subject, park) in &rowless_strands {
        writeln!(
            out,
            "  {subject} [no active row]  stranded: {}{}",
            park.park_kind,
            detail_suffix(&park.detail)
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::claims::{
        InMemoryClaimStore, RESOLUTION_ATTRIBUTION_OPERATOR, RESOLUTION_ATTRIBUTION_PROBE,
        STOP_DANGLING_PREREQUISITE, STOP_NO_OP_CYCLE, STOP_PREPARE_SKIP, STOP_SOURCE_TRUNCATION,
        STOP_ZERO_GRADER_DROP,
    };
    use crate::sources::{DirectoryWorkSource, WorkSource};
    use chrono::Duration;
    use flywheel_core::events::{
        DomainEvent, PARK_KIND_DIVERGENT_BASE, PARK_KIND_HELD_OUT_GATE, PARK_KIND_PROTECTED_PATHS,
        PARK_KIND_PUSH_FAILED, PARK_KIND_STANDING_VERIFY, PARK_KIND_SUBMIT_ERROR,
    };
    use flywheel_core::lifecycle::{Attempt, Lifecycle};
    use flywheel_core::store::{AttemptStore, DomainEventStore, LifecycleStore, TaskStore};
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::{CommandGrader, ManualGrader, Task};
    use tempfile::TempDir;

    fn park(store: &InMemoryStore, run_id: &str, kind: &str, detail: &str, ts: DateTime<Utc>) {
        let loaded = store.load_lifecycle(run_id).unwrap().unwrap();
        let event = DomainEvent::new(
            run_id,
            ts,
            DomainEventPayload::LandingParked(LandingParked {
                park_kind: kind.into(),
                detail: detail.into(),
                ..LandingParked::default()
            }),
        );
        store.append_domain_event(&event, loaded.version).unwrap();
    }

    fn stop(claims: &InMemoryClaimStore, kind: &str, subject: &str, detail: &str) {
        claims
            .record_stop_event(kind, subject, detail, t0(), "")
            .unwrap();
    }

    fn resolution(
        claims: &InMemoryClaimStore,
        subject: &str,
        attribution: &str,
        at: DateTime<Utc>,
    ) {
        claims
            .record_stop_event(STOP_RESOLVED, subject, "resolved", at, attribution)
            .unwrap();
    }

    fn run_status(
        tmp: &TempDir,
        store: &InMemoryStore,
        claims: &InMemoryClaimStore,
        json: bool,
        rollup: bool,
    ) -> String {
        let items = DirectoryWorkSource::new(tmp.path()).list_work().unwrap();
        let args = StatusArgs {
            json,
            rollup,
            ..StatusArgs::default()
        };
        let mut out = Vec::new();
        let code = cmd_status(&args, &items, store, claims, None, None, &mut out).unwrap();
        assert_eq!(code, 0);
        String::from_utf8(out).unwrap()
    }

    fn status_json(
        tmp: &TempDir,
        store: &InMemoryStore,
        claims: &InMemoryClaimStore,
    ) -> Vec<Value> {
        serde_json::from_str(&run_status(tmp, store, claims, true, false)).unwrap()
    }

    /// The seeded scenario the Python fixture rendered (see the embedded
    /// expectations below): seven listed tasks across two phases plus
    /// rowless stops and a rowless strand.
    fn scenario() -> (TempDir, InMemoryStore, InMemoryClaimStore) {
        let tmp = TempDir::new().unwrap();
        let ph1 = tmp.path().join("active").join("01-phase");
        let ph2 = tmp.path().join("active").join("02-phase");
        write_task(&ph1.join("a.json"), "a", &[]);
        write_task(&ph1.join("b.json"), "b", &[]);
        write_task_json(
            &ph1.join("c.json"),
            &json!({"id": "c", "goal": "Goal for c.", "graders": [{"type": "command", "run": "true", "name": "full-suite"}]}),
        );
        write_task_json(
            &ph1.join("d.json"),
            &json!({"id": "d", "goal": "Goal for d.", "graders": [{"type": "command", "run": "true"}, {"type": "manual", "instruction": "Confirm the migration is safe.", "name": "confirm"}]}),
        );
        write_task(&ph1.join("e.json"), "e", &[]);
        write_task(&ph1.join("f-long-task-id.json"), "f-long-task-id", &["a"]);
        write_task(&ph2.join("g.json"), "g", &[]);

        let store = InMemoryStore::new();
        let b = walk(
            "b",
            "run-b",
            &[
                Status::Ready,
                Status::Running,
                Status::Validating,
                Status::Done,
            ],
            "",
        );
        store.create_lifecycle(&b).unwrap();
        park(
            &store,
            "run-b",
            "standing-verify",
            "b failed the standing build invariant",
            t0() + Duration::minutes(1),
        );
        let mut c = walk(
            "c",
            "run-c",
            &[Status::Ready, Status::Running, Status::Interrupted],
            "",
        );
        c.blocked_requires_json = Some(
            json!([
                {"type": "command_grader", "name": "full-suite"},
                {"type": "file_exists", "path": ".flywheel/lkg/.venv", "present": true},
                {"type": "env_var_set", "name": "READY"},
                {"type": "weird"}
            ])
            .to_string(),
        );
        store.create_lifecycle(&c).unwrap();
        let mut task = Task::new(
            "Goal for d.",
            vec![
                Grader::Command(CommandGrader::new("true").unwrap()),
                Grader::Manual(ManualGrader::new("Confirm the migration is safe.").unwrap()),
            ],
        );
        task.id = "d".into();
        let hash = store.save_task(&task, t0()).unwrap();
        let mut d = Lifecycle::new("d").with_run_id("run-d");
        d.task_content_hash = hash;
        for s in [Status::Ready, Status::Running, Status::Validating] {
            d.transition_to(s, "", Some(t0())).unwrap();
        }
        d.awaiting_manual_ordinal = Some(1);
        d.transition_to(Status::AwaitingApproval, "", Some(t0()))
            .unwrap();
        store.create_lifecycle(&d).unwrap();
        let mut attempt = Attempt::new(1, t0(), "run-d");
        attempt.iterations_completed = 2;
        attempt.input_tokens = 5;
        attempt.output_tokens = 5;
        attempt.turns = 1;
        attempt.total_cost_usd = 0.5;
        attempt.last_activity_at = Some(t0() + Duration::seconds(30));
        store.save_attempt("run-d", &attempt, None).unwrap();
        let e = walk(
            "e",
            "run-e",
            &[Status::Ready, Status::Running, Status::Failed],
            "boom: it broke",
        );
        store.create_lifecycle(&e).unwrap();
        let g = walk(
            "g",
            "run-g",
            &[Status::Ready, Status::Running, Status::InternalError],
            "infra blew up",
        );
        store.create_lifecycle(&g).unwrap();
        let zz = walk(
            "zz",
            "run-zz",
            &[
                Status::Ready,
                Status::Running,
                Status::Validating,
                Status::Done,
            ],
            "",
        );
        store.create_lifecycle(&zz).unwrap();
        park(
            &store,
            "run-zz",
            "divergent-base",
            "cannot fast-forward",
            t0() + Duration::minutes(1),
        );
        let mut r1 = walk(
            "running-one",
            "run-r1",
            &[Status::Ready, Status::Running],
            "",
        );
        r1.worker_id = "worker-7".into();
        store.create_lifecycle(&r1).unwrap();

        let claims = InMemoryClaimStore::new();
        stop(&claims, STOP_PREPARE_SKIP, "e", "skip cause");
        stop(&claims, STOP_NO_OP_CYCLE, "my-source", "");
        stop(&claims, STOP_DANGLING_PREREQUISITE, "gone", "d1");
        stop(&claims, STOP_RESOLVED, "gone", "archived");
        stop(&claims, STOP_DANGLING_PREREQUISITE, "gone2", "d2");
        (tmp, store, claims)
    }

    const EXPECTED_TEXT: &str = "  01-phase/a               fresh            
  01-phase/b               done             
    stranded: standing-verify -- b failed the standing build invariant
  01-phase/c               interrupted      
    blocked_on: command_grader=full-suite, file_exists=.flywheel/lkg/.venv, env_var_set=READY, weird=?
  01-phase/d               awaiting_approval
    awaiting_on: Confirm the migration is safe.
  01-phase/e               retryable          -- boom: it broke
    stopped: prepare-skip -- skip cause
  01-phase/f-long-task-id  fresh            
  02-phase/g               retryable          -- infra blew up
  gone2 [no active row]  stopped: dangling-prerequisite -- d2
  my-source [no active row]  stopped: no-op-cycle
  zz [no active row]  stranded: divergent-base -- cannot fast-forward
";

    const EXPECTED_JSON: &str = r#"[
  {
    "task_id": "a",
    "task_file": "<TMP>/active/01-phase/a.json",
    "source_ref": "<TMP>/active/01-phase/a.json",
    "state": "fresh",
    "latest_run_id": null,
    "latest_status": null,
    "latest_error": "",
    "prerequisites": []
  },
  {
    "task_id": "b",
    "task_file": "<TMP>/active/01-phase/b.json",
    "source_ref": "<TMP>/active/01-phase/b.json",
    "state": "done",
    "latest_run_id": "run-b",
    "latest_status": "done",
    "latest_error": "",
    "prerequisites": [],
    "stranded": {
      "park_kind": "standing-verify",
      "detail": "b failed the standing build invariant"
    }
  },
  {
    "task_id": "c",
    "task_file": "<TMP>/active/01-phase/c.json",
    "source_ref": "<TMP>/active/01-phase/c.json",
    "state": "interrupted",
    "latest_run_id": "run-c",
    "latest_status": "interrupted",
    "latest_error": "",
    "prerequisites": [],
    "blocked_requires": [
      {
        "type": "command_grader",
        "name": "full-suite"
      },
      {
        "type": "file_exists",
        "path": ".flywheel/lkg/.venv",
        "present": true
      },
      {
        "type": "env_var_set",
        "name": "READY"
      },
      {
        "type": "weird"
      }
    ]
  },
  {
    "task_id": "d",
    "task_file": "<TMP>/active/01-phase/d.json",
    "source_ref": "<TMP>/active/01-phase/d.json",
    "state": "awaiting_approval",
    "latest_run_id": "run-d",
    "latest_status": "awaiting_approval",
    "latest_error": "",
    "prerequisites": [],
    "awaiting_on": {
      "ordinal": 1,
      "instruction": "Confirm the migration is safe."
    }
  },
  {
    "task_id": "e",
    "task_file": "<TMP>/active/01-phase/e.json",
    "source_ref": "<TMP>/active/01-phase/e.json",
    "state": "retryable",
    "latest_run_id": "run-e",
    "latest_status": "failed",
    "latest_error": "boom: it broke",
    "prerequisites": [],
    "stopped": {
      "kind": "prepare-skip",
      "detail": "skip cause"
    }
  },
  {
    "task_id": "f-long-task-id",
    "task_file": "<TMP>/active/01-phase/f-long-task-id.json",
    "source_ref": "<TMP>/active/01-phase/f-long-task-id.json",
    "state": "fresh",
    "latest_run_id": null,
    "latest_status": null,
    "latest_error": "",
    "prerequisites": [
      "a"
    ]
  },
  {
    "task_id": "g",
    "task_file": "<TMP>/active/02-phase/g.json",
    "source_ref": "<TMP>/active/02-phase/g.json",
    "state": "retryable",
    "latest_run_id": "run-g",
    "latest_status": "internal_error",
    "latest_error": "infra blew up",
    "prerequisites": []
  },
  {
    "subject": "gone2",
    "stopped": {
      "kind": "dangling-prerequisite",
      "detail": "d2"
    }
  },
  {
    "subject": "my-source",
    "stopped": {
      "kind": "no-op-cycle",
      "detail": ""
    }
  },
  {
    "subject": "zz",
    "stranded": {
      "park_kind": "divergent-base",
      "detail": "cannot fast-forward"
    }
  }
]
"#;

    const EXPECTED_ROLLUP: &str =
        "rollup -- status is derived from grader evidence, not self-reported

  01-phase  (0/6 verified)
    not_started        a             
    accepted           b               done with no graders -- agent claim, unverified
    blocked            c               interrupted
    blocked            d               awaiting operator approval
    failed             e               boom: it broke
    blocked_by_prereq  f-long-task-id  waiting on: a

  02-phase  (0/1 verified)
    failed             g               infra blew up

  totals: accepted 1  blocked 2  failed 2  blocked_by_prereq 1  not_started 1
";

    #[test]
    fn text_surface_is_byte_identical_to_python() {
        let (tmp, store, claims) = scenario();
        assert_eq!(
            run_status(&tmp, &store, &claims, false, false),
            EXPECTED_TEXT
        );
    }

    #[test]
    fn json_surface_is_byte_identical_to_python() {
        let (tmp, store, claims) = scenario();
        let expected = EXPECTED_JSON.replace("<TMP>", &tmp.path().display().to_string());
        assert_eq!(run_status(&tmp, &store, &claims, true, false), expected);
    }

    #[test]
    fn rollup_surfaces_are_byte_identical_to_python() {
        let (tmp, store, claims) = scenario();
        assert_eq!(
            run_status(&tmp, &store, &claims, false, true),
            EXPECTED_ROLLUP
        );
        let json_text = run_status(&tmp, &store, &claims, true, true);
        assert!(json_text.starts_with("{\n  \"totals\": {\n    \"not_started\": 1,\n    \"accepted\": 1,\n    \"blocked\": 2,\n    \"failed\": 2,\n    \"blocked_by_prereq\": 1\n  },\n  \"phases\": [\n    {\n      \"name\": \"01-phase\",\n      \"verified\": 0,\n      \"task_count\": 6,\n"));
        assert!(json_text.ends_with(
            "          \"detail\": \"infra blew up\"\n        }\n      ]\n    }\n  ]\n}\n"
        ));
    }

    #[test]
    fn empty_listing_renders_the_placeholders() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("active")).unwrap();
        let store = InMemoryStore::new();
        let claims = InMemoryClaimStore::new();
        assert_eq!(
            run_status(&tmp, &store, &claims, false, false),
            "(no active tasks)\n"
        );
        assert_eq!(run_status(&tmp, &store, &claims, true, false), "[]\n");
        assert_eq!(
            run_status(&tmp, &store, &claims, false, true),
            "rollup -- status is derived from grader evidence, not self-reported\n(no tasks)\n"
        );
        assert_eq!(
            run_status(&tmp, &store, &claims, true, true),
            "{\n  \"totals\": {},\n  \"phases\": []\n}\n"
        );
    }

    #[test]
    fn resolve_clears_a_ledger_only_landing_strand_until_it_recurs() {
        use super::super::args::ResolveArgs;
        use super::super::ops::cmd_resolve;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("active")).unwrap();
        let store = InMemoryStore::new();
        let claims = InMemoryClaimStore::new();
        // A parked DONE run whose task left the listing and that never
        // wrote a stop row: status surfaces it, so resolve must clear it.
        let lc = seed_done(&store, "zz");
        park(
            &store,
            &lc.run_id,
            PARK_KIND_STANDING_VERIFY,
            "red gate",
            t0(),
        );
        assert!(
            run_status(&tmp, &store, &claims, false, false)
                .contains("zz [no active row]  stranded")
        );
        let resolve = |reason: &str, now: DateTime<Utc>| {
            let args = ResolveArgs {
                task_id: "zz".into(),
                reason: reason.into(),
                policy: None,
                db: None,
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cmd_resolve(&args, &store, &claims, now, &mut out, &mut err).unwrap();
            (
                code,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        };
        let later = t0() + chrono::Duration::seconds(10);
        assert_eq!(
            resolve("superseded by main", later),
            (
                0,
                "resolved zz: abandoned strand (superseded 'standing-verify')\n".into(),
                String::new()
            )
        );
        assert_eq!(
            run_status(&tmp, &store, &claims, false, false),
            "(no active tasks)\n"
        );
        let (code, _, err) = resolve("again", later);
        assert_eq!(code, 1);
        assert!(err.contains("(already resolved)"), "{err}");
        // A park after the marker is a fresh recurrence: surfaced and
        // resolvable again.
        park(
            &store,
            &lc.run_id,
            PARK_KIND_STANDING_VERIFY,
            "red again",
            later + chrono::Duration::seconds(10),
        );
        assert!(
            run_status(&tmp, &store, &claims, false, false)
                .contains("stranded: standing-verify -- red again")
        );
        assert_eq!(resolve("again", later + chrono::Duration::seconds(20)).0, 0);
        assert_eq!(
            run_status(&tmp, &store, &claims, false, false),
            "(no active tasks)\n"
        );
    }

    #[test]
    fn worker_id_is_surfaced_only_for_live_runs_with_an_id() {
        let tmp = TempDir::new().unwrap();
        write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
        write_task(&tmp.path().join("active/01/b.json"), "b", &[]);
        let store = InMemoryStore::new();
        let mut a = walk("a", "run-a", &[Status::Ready, Status::Running], "");
        a.worker_id = "worker-99".into();
        store.create_lifecycle(&a).unwrap();
        seed_running(&store, "b");
        let claims = InMemoryClaimStore::new();
        let payload = status_json(&tmp, &store, &claims);
        assert_eq!(payload[0]["worker_id"], json!("worker-99"));
        assert!(payload[1].get("worker_id").is_none());
    }

    #[test]
    fn every_park_kind_surfaces_on_its_row_and_clean_done_does_not() {
        for kind in [
            PARK_KIND_HELD_OUT_GATE,
            PARK_KIND_PROTECTED_PATHS,
            PARK_KIND_PUSH_FAILED,
            PARK_KIND_SUBMIT_ERROR,
        ] {
            let tmp = TempDir::new().unwrap();
            write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
            let store = InMemoryStore::new();
            let lc = seed_done(&store, "a");
            park(&store, &lc.run_id, kind, "blocked here", t0());
            let claims = InMemoryClaimStore::new();
            let text = run_status(&tmp, &store, &claims, false, false);
            let line = text.lines().find(|l| l.contains("stranded:")).unwrap();
            assert!(line.contains(kind) && line.contains("blocked here"));
            let payload = status_json(&tmp, &store, &claims);
            assert_eq!(
                payload[0]["stranded"],
                json!({"park_kind": kind, "detail": "blocked here"})
            );
        }
        let tmp = TempDir::new().unwrap();
        write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
        let store = InMemoryStore::new();
        seed_done(&store, "a");
        let claims = InMemoryClaimStore::new();
        assert!(
            status_json(&tmp, &store, &claims)[0]
                .get("stranded")
                .is_none()
        );
        assert!(!run_status(&tmp, &store, &claims, false, false).contains("stranded:"));
    }

    #[test]
    fn stops_surface_per_task_or_as_their_own_unit_and_clear_on_resolution() {
        for kind in [STOP_DANGLING_PREREQUISITE, STOP_PREPARE_SKIP] {
            let tmp = TempDir::new().unwrap();
            write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
            let store = InMemoryStore::new();
            let claims = InMemoryClaimStore::new();
            for _ in 0..3 {
                stop(&claims, kind, "a", "stop cause");
            }
            let text = run_status(&tmp, &store, &claims, false, false);
            let line = text.lines().find(|l| l.contains("stopped:")).unwrap();
            assert!(line.contains(kind) && line.contains("stop cause"));
            let payload = status_json(&tmp, &store, &claims);
            assert_eq!(
                payload[0]["stopped"],
                json!({"kind": kind, "detail": "stop cause"})
            );
            assert_eq!(payload.len(), 1);
        }
        for kind in [
            STOP_NO_OP_CYCLE,
            STOP_SOURCE_TRUNCATION,
            STOP_ZERO_GRADER_DROP,
        ] {
            let tmp = TempDir::new().unwrap();
            write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
            let store = InMemoryStore::new();
            let claims = InMemoryClaimStore::new();
            stop(&claims, kind, "my-source", "source cause");
            let text = run_status(&tmp, &store, &claims, false, false);
            assert_eq!(
                text.lines().last().unwrap(),
                format!("  my-source [no active row]  stopped: {kind} -- source cause")
            );
            let payload = status_json(&tmp, &store, &claims);
            assert_eq!(
                payload[1],
                json!({"subject": "my-source", "stopped": {"kind": kind, "detail": "source cause"}})
            );
            assert!(payload[0].get("stopped").is_none());
        }
        let tmp = TempDir::new().unwrap();
        write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
        let store = InMemoryStore::new();
        let claims = InMemoryClaimStore::new();
        stop(&claims, STOP_PREPARE_SKIP, "a", "stop cause");
        stop(&claims, STOP_RESOLVED, "a", "serviced");
        stop(&claims, STOP_DANGLING_PREREQUISITE, "gone", "d1");
        stop(&claims, STOP_RESOLVED, "gone", "archived");
        let payload = status_json(&tmp, &store, &claims);
        assert_eq!(payload.len(), 1);
        assert!(payload[0].get("stopped").is_none());
        let text = run_status(&tmp, &store, &claims, false, false);
        assert!(!text.contains("stopped:") && !text.contains("gone"));
        stop(&claims, STOP_PREPARE_SKIP, "a", "recurred");
        let payload = status_json(&tmp, &store, &claims);
        assert_eq!(
            payload[0]["stopped"],
            json!({"kind": STOP_PREPARE_SKIP, "detail": "recurred"})
        );
    }

    #[test]
    fn landing_strands_outlive_the_listing_and_clear_only_on_attributed_resolution() {
        let strand = |with_file: bool, kind: &str| {
            let tmp = TempDir::new().unwrap();
            std::fs::create_dir_all(tmp.path().join("active/01")).unwrap();
            if with_file {
                write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
            }
            let store = InMemoryStore::new();
            let lc = walk(
                "a",
                "run-a",
                &[
                    Status::Ready,
                    Status::Running,
                    Status::Validating,
                    Status::Done,
                ],
                "",
            );
            store.create_lifecycle(&lc).unwrap();
            park(&store, "run-a", kind, "cannot fast-forward", t0());
            (tmp, store, InMemoryClaimStore::new())
        };
        let (tmp, store, claims) = strand(false, PARK_KIND_DIVERGENT_BASE);
        let payload = status_json(&tmp, &store, &claims);
        assert_eq!(
            payload,
            vec![
                json!({"subject": "a", "stranded": {"park_kind": PARK_KIND_DIVERGENT_BASE, "detail": "cannot fast-forward"}})
            ]
        );
        assert_eq!(
            run_status(&tmp, &store, &claims, false, false),
            "  a [no active row]  stranded: divergent-base -- cannot fast-forward\n"
        );
        // Attributed resolutions clear it; a plain archival marker does not.
        for attribution in [
            RESOLUTION_ATTRIBUTION_PROBE,
            RESOLUTION_ATTRIBUTION_OPERATOR,
        ] {
            let (tmp, store, claims) = strand(false, PARK_KIND_STANDING_VERIFY);
            resolution(&claims, "a", attribution, t0() + Duration::hours(1));
            assert!(status_json(&tmp, &store, &claims).is_empty());
            assert_eq!(
                run_status(&tmp, &store, &claims, false, false),
                "(no active tasks)\n"
            );
        }
        let (tmp, store, claims) = strand(false, PARK_KIND_DIVERGENT_BASE);
        resolution(&claims, "a", "", t0() + Duration::hours(1));
        assert_eq!(status_json(&tmp, &store, &claims).len(), 1);
        // A park after the resolution is a fresh recurrence.
        resolution(
            &claims,
            "a",
            RESOLUTION_ATTRIBUTION_OPERATOR,
            t0() + Duration::hours(1),
        );
        assert!(status_json(&tmp, &store, &claims).is_empty());
        park(
            &store,
            "run-a",
            PARK_KIND_DIVERGENT_BASE,
            "re-parked after resolution",
            t0() + Duration::hours(2),
        );
        let payload = status_json(&tmp, &store, &claims);
        assert_eq!(
            payload[0]["stranded"]["detail"],
            json!("re-parked after resolution")
        );
        // Non-strand parks keep the active-listing-only behavior.
        let (tmp, store, claims) = strand(false, PARK_KIND_HELD_OUT_GATE);
        assert!(status_json(&tmp, &store, &claims).is_empty());
        // A listed strand renders on its row, never duplicated, and honors
        // resolutions on the row too.
        let (tmp, store, claims) = strand(true, PARK_KIND_DIVERGENT_BASE);
        let payload = status_json(&tmp, &store, &claims);
        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0]["task_id"], json!("a"));
        assert_eq!(
            payload[0]["stranded"]["park_kind"],
            json!(PARK_KIND_DIVERGENT_BASE)
        );
        resolution(
            &claims,
            "a",
            RESOLUTION_ATTRIBUTION_OPERATOR,
            t0() + Duration::minutes(1),
        );
        let text = run_status(&tmp, &store, &claims, false, false);
        assert!(text.contains("01/a") && !text.contains("stranded:"));
        assert!(
            status_json(&tmp, &store, &claims)[0]
                .get("stranded")
                .is_none()
        );
        park(
            &store,
            "run-a",
            PARK_KIND_STANDING_VERIFY,
            "fresh recurrence",
            t0() + Duration::minutes(2),
        );
        assert!(
            run_status(&tmp, &store, &claims, false, false).contains("stranded: standing-verify")
        );
    }

    #[test]
    fn blocked_on_renders_only_for_blocked_interrupted_rows() {
        let tmp = TempDir::new().unwrap();
        write_task(&tmp.path().join("active/01/task-s.json"), "task-s", &[]);
        write_task(&tmp.path().join("active/01/task-i.json"), "task-i", &[]);
        let store = InMemoryStore::new();
        seed_blocked(
            &store,
            "task-s",
            &json!([{"type": "command_grader", "name": "full-suite"}, {"type": "file_exists", "path": ".flywheel/lkg/.venv", "present": true}]),
            None,
        );
        seed_interrupted(&store, "task-i");
        let claims = InMemoryClaimStore::new();
        let text = run_status(&tmp, &store, &claims, false, false);
        assert!(text.contains(
            "    blocked_on: command_grader=full-suite, file_exists=.flywheel/lkg/.venv\n"
        ));
        assert_eq!(text.matches("blocked_on:").count(), 1);
        let payload = status_json(&tmp, &store, &claims);
        assert!(payload[0].get("blocked_requires").is_none());
        assert_eq!(payload[1]["blocked_requires"].as_array().unwrap().len(), 2);
        assert_eq!(
            format_blocked_on(
                &parse_blocked_requires(Some(
                    r#"[{"type":"file_exists"},{"type":"","name":"x"},5]"#
                ))
                .unwrap()
            ),
            "file_exists=?, ?=?"
        );
        assert!(parse_blocked_requires(Some("{}")).is_none());
        assert!(parse_blocked_requires(Some("nope")).is_none());
    }
}
