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
use crate::landing_queue::{LandingQueue, LandingState, LandingView};
use crate::orchestrate::unlanded_prerequisite_holds;
use crate::rollup::{build_rollup_landed, render_rollup_text, rollup_to_json};
use crate::sources::WorkItem;

use super::args::StatusArgs;
use super::pauses::{active_pauses, session_pause_line};
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

/// The `landing` object of a queued task's `--json` entry:
/// `{state, position, queued_at, batch: null|{id, index, size}}`.
fn landing_json(view: &LandingView) -> Value {
    json!({
        "state": view.state.as_str(),
        "position": view.position,
        "queued_at": view.queued_at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        "batch": view.batch.as_ref().map(|batch| json!({
            "id": batch.id,
            "index": batch.index,
            "size": batch.size,
        })),
    })
}

/// The text form's `landing:` value: `queued #<position>` while waiting,
/// `batch <id> <index>/<size>` while its batch is under the standing verify.
fn landing_text(view: &LandingView) -> String {
    match (view.state, &view.batch) {
        (LandingState::Verifying, Some(batch)) => {
            format!("batch {} {}/{}", batch.id, batch.index, batch.size)
        }
        _ => format!("queued #{}", view.position),
    }
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

/// Every not-done listed task held behind unlanded prerequisites, keyed by
/// the dependent, each with those prerequisite ids in prerequisite order --
/// the landed-only predicate selection applies (spec 00086, D-4).
pub(crate) fn waiting_on_landing(
    rows: &[StatusRow],
    store: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    now: DateTime<Utc>,
) -> Result<BTreeMap<String, Vec<String>>, WorkflowError> {
    Ok(
        unlanded_prerequisite_holds(store, claims, &snapshots(rows), now)?
            .into_iter()
            .map(|(dependent, holds)| {
                (
                    dependent,
                    holds.into_iter().map(|hold| hold.task_id).collect(),
                )
            })
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
fn cmd_status_rollup(
    args: &StatusArgs,
    rows: &[StatusRow],
    store: &dyn HarnessStore,
    landing_holds: &BTreeMap<String, Vec<String>>,
    repo_root: Option<&Path>,
    true_base: Option<&str>,
    out: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let plain = snapshots(rows);
    let holds = reachability_held_prerequisites(&plain, repo_root, true_base);
    let rollup = build_rollup_landed(&plain, store, &holds, landing_holds)?;
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
/// clears a strand; a park appended after it is a fresh recurrence. The
/// text form leads with one `session-pause:` line per active pause; the
/// `--json` array is unchanged. A task with a live entry in
/// `landing_queue` (spec 00089) carries `landing: queued #<position>` /
/// `landing: batch <id> <index>/<size>` and the `--json` `landing` object;
/// every other task carries neither.
#[allow(clippy::too_many_arguments)]
pub fn cmd_status(
    args: &StatusArgs,
    items: &[WorkItem],
    store: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    landing_queue: Option<&LandingQueue>,
    repo_root: Option<&Path>,
    true_base: Option<&str>,
    out: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let rows = status_rows(items, store)?;
    let now = Utc::now();
    let landing_holds = waiting_on_landing(&rows, store, claims, now)?;
    if args.rollup {
        return cmd_status_rollup(
            args,
            &rows,
            store,
            &landing_holds,
            repo_root,
            true_base,
            out,
        );
    }
    let worker_ids = live_worker_ids(store)?;
    let landing: BTreeMap<String, LandingView> = match landing_queue {
        Some(queue) => queue
            .landing_views(claims, now)
            .map_err(|err| WorkflowError::Message(format!("reading the landing queue: {err}")))?,
        None => BTreeMap::new(),
    };

    // One ledger read per DONE run, shared by the per-row annotation and
    // the store-backed strand surface.
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
            if let Some(unlanded) = landing_holds.get(row.task_id()) {
                entry.insert("unlanded_prerequisites".into(), json!(unlanded));
            }
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
            if let Some(view) = landing.get(row.task_id()) {
                entry.insert("landing".into(), landing_json(view));
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

    // Account-scoped session pauses lead the listing: one line per pause
    // still holding workers back; expired and cleared ones print nothing.
    for pause in active_pauses(claims, Utc::now())? {
        writeln!(out, "{}", session_pause_line(&pause))?;
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
        if let Some(unlanded) = landing_holds.get(row.task_id()) {
            writeln!(out, "    waiting_on_landing: {}", unlanded.join(", "))?;
        }
        if let Some(view) = landing.get(row.task_id()) {
            writeln!(out, "    landing: {}", landing_text(view))?;
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
    use super::super::pauses::{PauseSelector, clear_session_pauses, format_pause_time};
    use super::super::test_support::*;
    use super::*;
    use crate::claims::{
        DEFAULT_PAUSE_ACCOUNT, InMemoryClaimStore, PAUSE_CLEAR_VIA_CLI,
        RESOLUTION_ATTRIBUTION_OPERATOR, RESOLUTION_ATTRIBUTION_PROBE,
        RESOLUTION_ATTRIBUTION_RELAND, STOP_DANGLING_PREREQUISITE, STOP_NO_OP_CYCLE,
        STOP_PREPARE_SKIP, STOP_SOURCE_TRUNCATION, STOP_ZERO_GRADER_DROP, SqliteClaimStore,
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
        let code = cmd_status(&args, &items, store, claims, None, None, None, &mut out).unwrap();
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

    /// The seeded scenario the goldens render (see the embedded
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
    fn text_surface_matches_golden() {
        let (tmp, store, claims) = scenario();
        assert_eq!(
            run_status(&tmp, &store, &claims, false, false),
            EXPECTED_TEXT
        );
    }

    #[test]
    fn json_surface_matches_golden() {
        let (tmp, store, claims) = scenario();
        let expected = EXPECTED_JSON.replace("<TMP>", &tmp.path().display().to_string());
        assert_eq!(run_status(&tmp, &store, &claims, true, false), expected);
    }

    #[test]
    fn rollup_surfaces_match_golden() {
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
    fn reland_resolution_clears_the_routed_stop_and_the_strand_like_the_others() {
        let tmp = TempDir::new().unwrap();
        write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
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
        park(
            &store,
            "run-a",
            PARK_KIND_STANDING_VERIFY,
            "verify failed",
            t0(),
        );
        let claims = InMemoryClaimStore::new();
        claims
            .record_human_review(
                PARK_KIND_STANDING_VERIFY,
                "a",
                t0(),
                "run-a",
                "landing re-drive exhausted",
            )
            .unwrap();
        // A re-land that parks appends no resolution: stop and strand stay.
        park(
            &store,
            "run-a",
            PARK_KIND_STANDING_VERIFY,
            "re-land parked",
            t0() + Duration::minutes(1),
        );
        let entry = &status_json(&tmp, &store, &claims)[0];
        assert_eq!(entry["stopped"]["kind"], json!(PARK_KIND_STANDING_VERIFY));
        assert_eq!(entry["stranded"]["detail"], json!("re-land parked"));
        // A landed re-land's `reland` resolution clears both.
        resolution(
            &claims,
            "a",
            RESOLUTION_ATTRIBUTION_RELAND,
            t0() + Duration::minutes(2),
        );
        let entry = &status_json(&tmp, &store, &claims)[0];
        assert!(entry.get("stopped").is_none(), "{entry}");
        assert!(entry.get("stranded").is_none(), "{entry}");
        let text = run_status(&tmp, &store, &claims, false, false);
        assert!(!text.contains("stopped:") && !text.contains("stranded:"));
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

    // --- landing queue (spec 00089) ---------------------------------------

    /// `fw status` over a listing of DONE tasks `a`, `b`, `c` with the
    /// landing queue at `queue`.
    fn landing_status(
        tmp: &TempDir,
        store: &InMemoryStore,
        claims: &InMemoryClaimStore,
        queue: &LandingQueue,
        json: bool,
    ) -> String {
        let items = DirectoryWorkSource::new(tmp.path().join("tasks"))
            .list_work()
            .unwrap();
        let args = StatusArgs {
            json,
            ..StatusArgs::default()
        };
        let mut out = Vec::new();
        let code = cmd_status(
            &args,
            &items,
            store,
            claims,
            Some(queue),
            None,
            None,
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 0);
        String::from_utf8(out).unwrap()
    }

    fn landing_scenario() -> (TempDir, InMemoryStore, InMemoryClaimStore, LandingQueue) {
        let tmp = TempDir::new().unwrap();
        let ph = tmp.path().join("tasks").join("active").join("01-phase");
        let store = InMemoryStore::new();
        for task in ["a", "b", "c"] {
            write_task(&ph.join(format!("{task}.json")), task, &[]);
            seed_done(&store, task);
        }
        let queue = LandingQueue::at(tmp.path().join("queue"));
        (tmp, store, InMemoryClaimStore::new(), queue)
    }

    /// Hold `task_id`'s claim as `worker` with a lease well past now.
    fn hold_claim(claims: &InMemoryClaimStore, task_id: &str, worker: &str) {
        claims
            .acquire_claim(
                task_id,
                worker,
                Utc::now(),
                3_600.0,
                &crate::claims::conflict_keys::<_, String>([]),
            )
            .unwrap()
            .expect("claim acquired");
    }

    /// Enqueue `task_id`'s DONE run as owned by `owner`, `secs` after `t0()`.
    fn enqueue(
        queue: &LandingQueue,
        task_id: &str,
        owner: &str,
        secs: i64,
    ) -> crate::landing_queue::QueueEntry {
        let task = Task::new(
            format!("land {task_id}"),
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id(task_id);
        let request = crate::strategy::SubmitRequest::new(
            task,
            format!("run-{task_id}-ok"),
            Status::Done,
            format!("/wt/{task_id}"),
        );
        queue
            .enqueue(&request, owner, t0() + Duration::seconds(secs))
            .unwrap()
    }

    fn entry_for(payload: &[Value], task_id: &str) -> Value {
        payload
            .iter()
            .find(|e| e["task_id"] == json!(task_id))
            .cloned()
            .unwrap()
    }

    #[test]
    fn landing_status_gives_each_queued_task_its_fifo_position() {
        let (tmp, store, claims, queue) = landing_scenario();
        hold_claim(&claims, "a", "w1");
        hold_claim(&claims, "b", "w2");
        // b queued first: it is #1 even though a sorts first by id.
        enqueue(&queue, "a", "w1", 20);
        enqueue(&queue, "b", "w2", 10);

        let text = landing_status(&tmp, &store, &claims, &queue, false);
        assert!(
            text.contains("  01-phase/a  done             \n    landing: queued #2\n"),
            "{text}"
        );
        assert!(
            text.contains("  01-phase/b  done             \n    landing: queued #1\n"),
            "{text}"
        );
        assert!(
            text.contains("  01-phase/c  done             \n")
                && !text.contains("c  done             \n    landing:"),
            "{text}"
        );
        assert_eq!(text.matches("landing:").count(), 2, "{text}");

        let payload: Vec<Value> =
            serde_json::from_str(&landing_status(&tmp, &store, &claims, &queue, true)).unwrap();
        assert_eq!(
            entry_for(&payload, "a")["landing"],
            json!({
                "state": "queued",
                "position": 2,
                "queued_at": "2026-07-08T12:00:20Z",
                "batch": null,
            })
        );
        assert_eq!(
            entry_for(&payload, "b")["landing"],
            json!({
                "state": "queued",
                "position": 1,
                "queued_at": "2026-07-08T12:00:10Z",
                "batch": null,
            })
        );
        // Every pre-existing key is still carried.
        let a = entry_for(&payload, "a");
        for key in [
            "task_id",
            "task_file",
            "source_ref",
            "state",
            "latest_run_id",
            "latest_status",
            "latest_error",
            "prerequisites",
        ] {
            assert!(a.get(key).is_some(), "missing {key}: {a}");
        }
    }

    #[test]
    fn landing_status_shows_batch_id_index_and_size_while_verifying() {
        let (tmp, store, claims, queue) = landing_scenario();
        hold_claim(&claims, "a", "w1");
        hold_claim(&claims, "b", "w2");
        hold_claim(&claims, "c", "w3");
        let a = enqueue(&queue, "a", "w1", 10);
        let b = enqueue(&queue, "b", "w2", 20);
        enqueue(&queue, "c", "w3", 30);
        let batch = |index| flywheel_core::events::LandedBatch {
            id: "batch-7".to_owned(),
            index,
            size: 2,
        };
        queue.mark_verifying(&a, batch(1)).unwrap();
        queue.mark_verifying(&b, batch(2)).unwrap();

        let text = landing_status(&tmp, &store, &claims, &queue, false);
        assert!(
            text.contains("01-phase/a  done             \n    landing: batch batch-7 1/2\n"),
            "{text}"
        );
        assert!(
            text.contains("01-phase/b  done             \n    landing: batch batch-7 2/2\n"),
            "{text}"
        );
        assert!(
            text.contains("01-phase/c  done             \n    landing: queued #3\n"),
            "{text}"
        );

        let payload: Vec<Value> =
            serde_json::from_str(&landing_status(&tmp, &store, &claims, &queue, true)).unwrap();
        assert_eq!(
            entry_for(&payload, "b")["landing"],
            json!({
                "state": "verifying",
                "position": 2,
                "queued_at": "2026-07-08T12:00:20Z",
                "batch": {"id": "batch-7", "index": 2, "size": 2},
            })
        );
        assert_eq!(
            entry_for(&payload, "c")["landing"]["state"],
            json!("queued")
        );
        assert_eq!(entry_for(&payload, "c")["landing"]["position"], json!(3));
    }

    #[test]
    fn landing_status_is_omitted_when_not_queued() {
        let (tmp, store, claims, queue) = landing_scenario();
        // An empty (absent) queue: every DONE task carries no landing.
        let text = landing_status(&tmp, &store, &claims, &queue, false);
        assert!(!text.contains("landing:"), "{text}");
        let payload: Vec<Value> =
            serde_json::from_str(&landing_status(&tmp, &store, &claims, &queue, true)).unwrap();
        assert!(
            payload.iter().all(|e| e.get("landing").is_none()),
            "{payload:?}"
        );

        // Only the queued one gains it; the landed ones without an entry don't.
        hold_claim(&claims, "b", "w2");
        enqueue(&queue, "b", "w2", 10);
        let payload: Vec<Value> =
            serde_json::from_str(&landing_status(&tmp, &store, &claims, &queue, true)).unwrap();
        assert!(entry_for(&payload, "a").get("landing").is_none());
        assert!(entry_for(&payload, "c").get("landing").is_none());
        assert_eq!(entry_for(&payload, "b")["landing"]["position"], json!(1));
    }

    #[test]
    fn landing_status_drops_a_dead_owner_entry() {
        let (tmp, store, claims, queue) = landing_scenario();
        // a's owner holds no claim; c's claim is held by another worker.
        enqueue(&queue, "a", "w-dead", 10);
        hold_claim(&claims, "c", "w-other");
        enqueue(&queue, "c", "w3", 15);
        hold_claim(&claims, "b", "w2");
        enqueue(&queue, "b", "w2", 20);

        let text = landing_status(&tmp, &store, &claims, &queue, false);
        assert_eq!(text.matches("landing:").count(), 1, "{text}");
        assert!(
            text.contains("01-phase/b  done             \n    landing: queued #1\n"),
            "{text}"
        );
        let payload: Vec<Value> =
            serde_json::from_str(&landing_status(&tmp, &store, &claims, &queue, true)).unwrap();
        assert!(entry_for(&payload, "a").get("landing").is_none());
        assert!(entry_for(&payload, "c").get("landing").is_none());
        assert_eq!(entry_for(&payload, "b")["landing"]["position"], json!(1));
    }

    /// Two parked DONE prerequisites (`p2` listed before `p1` in `dep`'s
    /// prerequisites), one landed-clean DONE prerequisite `q` with its own
    /// dependent `free`.
    fn waiting_on_landing_scenario() -> (TempDir, InMemoryStore, InMemoryClaimStore) {
        let tmp = TempDir::new().unwrap();
        let ph = tmp.path().join("active").join("01-phase");
        write_task(&ph.join("p1.json"), "p1", &[]);
        write_task(&ph.join("p2.json"), "p2", &[]);
        write_task(&ph.join("q.json"), "q", &[]);
        write_task(&ph.join("dep.json"), "dep", &["p2", "q", "p1"]);
        write_task(&ph.join("free.json"), "free", &["q"]);
        let store = InMemoryStore::new();
        for (task, kind) in [
            ("p1", PARK_KIND_HELD_OUT_GATE),
            ("p2", PARK_KIND_STANDING_VERIFY),
        ] {
            let lc = seed_done(&store, task);
            park(&store, &lc.run_id, kind, "parked", t0());
        }
        seed_done(&store, "q");
        // A held-out-gate park is routed to the human-review queue, which
        // records its stop row (the strand `resolve` abandons).
        let claims = InMemoryClaimStore::new();
        claims
            .record_human_review(
                PARK_KIND_HELD_OUT_GATE,
                "p1",
                t0(),
                "run-p1-ok",
                "landing re-drive exhausted",
            )
            .unwrap();
        (tmp, store, claims)
    }

    fn rollup_task(payload: &Value, task_id: &str) -> Value {
        payload["phases"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|phase| phase["tasks"].as_array().unwrap().iter())
            .find(|task| task["task_id"] == json!(task_id))
            .cloned()
            .unwrap()
    }

    #[test]
    fn waiting_on_landing_names_every_unlanded_prerequisite_in_prerequisite_order() {
        let (tmp, store, claims) = waiting_on_landing_scenario();
        let text = run_status(&tmp, &store, &claims, false, false);
        assert!(
            text.contains("  01-phase/dep   fresh            \n    waiting_on_landing: p2, p1\n"),
            "{text}"
        );
        assert_eq!(text.matches("waiting_on_landing:").count(), 1, "{text}");

        let payload = status_json(&tmp, &store, &claims);
        let entry = |id: &str| {
            payload
                .iter()
                .find(|e| e["task_id"] == json!(id))
                .cloned()
                .unwrap()
        };
        assert_eq!(entry("dep")["unlanded_prerequisites"], json!(["p2", "p1"]));
        for id in ["free", "q", "p1", "p2"] {
            assert!(entry(id).get("unlanded_prerequisites").is_none(), "{id}");
        }

        let rollup: Value =
            serde_json::from_str(&run_status(&tmp, &store, &claims, true, true)).unwrap();
        let dep = rollup_task(&rollup, "dep");
        assert_eq!(dep["status"], json!("blocked_by_prereq"));
        assert_eq!(dep["unsatisfied_prerequisites"], json!(["p2", "p1"]));
        assert_eq!(dep["detail"], json!("waiting on landing: p2, p1"));
        let free = rollup_task(&rollup, "free");
        assert_eq!(free["status"], json!("not_started"));
        assert_eq!(free["unsatisfied_prerequisites"], json!([]));
        let rollup_text = run_status(&tmp, &store, &claims, false, true);
        assert!(
            rollup_text.contains("waiting on landing: p2, p1"),
            "{rollup_text}"
        );
    }

    #[test]
    fn waiting_on_landing_joins_incomplete_prerequisites_in_the_rollup() {
        let (tmp, store, claims) = waiting_on_landing_scenario();
        let ph = tmp.path().join("active").join("01-phase");
        write_task(&ph.join("todo.json"), "todo", &[]);
        write_task(&ph.join("mixed.json"), "mixed", &["todo", "p1"]);
        let rollup: Value =
            serde_json::from_str(&run_status(&tmp, &store, &claims, true, true)).unwrap();
        let mixed = rollup_task(&rollup, "mixed");
        assert_eq!(mixed["status"], json!("blocked_by_prereq"));
        assert_eq!(mixed["unsatisfied_prerequisites"], json!(["todo", "p1"]));
        assert_eq!(
            mixed["detail"],
            json!("waiting on: todo; waiting on landing: p1")
        );
    }

    #[test]
    fn waiting_on_landing_covers_a_landing_still_in_flight_and_clears_once_landed() {
        let tmp = TempDir::new().unwrap();
        let ph = tmp.path().join("active").join("01-phase");
        write_task(&ph.join("p.json"), "p", &[]);
        write_task(&ph.join("dep.json"), "dep", &["p"]);
        let store = InMemoryStore::new();
        seed_done(&store, "p");
        let claims = InMemoryClaimStore::new();
        // DONE, never parked, claim still held by a live worker: mid-landing.
        let claim = claims
            .acquire_claim("p", "worker-1", Utc::now(), 3600.0, &BTreeSet::new())
            .unwrap()
            .unwrap();
        let text = run_status(&tmp, &store, &claims, false, false);
        assert!(text.contains("    waiting_on_landing: p\n"), "{text}");
        let payload = status_json(&tmp, &store, &claims);
        let dep = payload
            .iter()
            .find(|e| e["task_id"] == json!("dep"))
            .unwrap();
        assert_eq!(dep["unlanded_prerequisites"], json!(["p"]));

        claims.release_claim(&claim, None).unwrap();
        let text = run_status(&tmp, &store, &claims, false, false);
        assert!(!text.contains("waiting_on_landing"), "{text}");
        let payload = status_json(&tmp, &store, &claims);
        assert!(
            payload
                .iter()
                .all(|e| e.get("unlanded_prerequisites").is_none())
        );
    }

    #[test]
    fn waiting_on_landing_clears_on_resolve_which_names_the_released_dependents() {
        use super::super::args::ResolveArgs;
        use super::super::ops::{cmd_resolve, cmd_resolve_releasing};
        let (tmp, store, claims) = waiting_on_landing_scenario();
        let ph = tmp.path().join("active").join("01-phase");
        write_task(&ph.join("dep-b.json"), "dep-b", &["p1"]);
        let items = DirectoryWorkSource::new(tmp.path()).list_work().unwrap();
        let resolve = |task: &str, now: DateTime<Utc>| {
            let args = ResolveArgs {
                task_id: task.into(),
                reason: "landed by hand".into(),
                db: None,
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code =
                cmd_resolve_releasing(&args, &items, &store, &claims, now, &mut out, &mut err)
                    .unwrap();
            (code, String::from_utf8(out).unwrap())
        };

        // p2 resolved: dep is still held on p1, so nothing is released.
        let (code, out) = resolve("p2", t0() + Duration::minutes(1));
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "resolved p2: abandoned strand (superseded 'standing-verify')\n"
        );
        let payload = status_json(&tmp, &store, &claims);
        let dep = payload
            .iter()
            .find(|e| e["task_id"] == json!("dep"))
            .unwrap();
        assert_eq!(dep["unlanded_prerequisites"], json!(["p1"]));

        // p1 resolved: both of its waiting dependents are freed, in status listing order.
        let (code, out) = resolve("p1", t0() + Duration::minutes(2));
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "resolved p1: abandoned strand (superseded 'held-out-gate')\nreleases: dep-b, dep\n"
        );
        let text = run_status(&tmp, &store, &claims, false, false);
        assert!(!text.contains("waiting_on_landing"), "{text}");

        // A refused resolve prints nothing on stdout, like the plain verb.
        let (code, out) = resolve("p1", t0() + Duration::minutes(3));
        assert_eq!((code, out.as_str()), (1, ""));

        // A resolve releasing nobody is byte-identical to the plain verb.
        let lone = seed_done(&store, "lone");
        park(&store, &lone.run_id, PARK_KIND_STANDING_VERIFY, "x", t0());
        let (_, wrapped) = resolve("lone", t0() + Duration::minutes(4));
        let other = seed_done(&store, "other");
        park(&store, &other.run_id, PARK_KIND_STANDING_VERIFY, "x", t0());
        let args = ResolveArgs {
            task_id: "other".into(),
            reason: "landed by hand".into(),
            db: None,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        cmd_resolve(&args, &store, &claims, t0(), &mut out, &mut err).unwrap();
        assert_eq!(
            wrapped.replace("lone", "other"),
            String::from_utf8(out).unwrap()
        );
    }

    /// `fw status` (text or `--json`) over `claims` with an empty listing.
    fn pause_status(claims: &dyn ClaimStore, json: bool) -> String {
        let args = StatusArgs {
            json,
            ..StatusArgs::default()
        };
        let mut out = Vec::new();
        let code = cmd_status(
            &args,
            &[],
            &InMemoryStore::new(),
            claims,
            None,
            None,
            None,
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 0);
        String::from_utf8(out).unwrap()
    }

    /// Record an active pause on `account` for `run_id`, `hours` out.
    fn active_pause(claims: &dyn ClaimStore, account: &str, run_id: &str, hours: i64) -> String {
        let now = Utc::now();
        let until = claims
            .record_session_pause(now + Duration::hours(hours), now, 86_400.0, run_id, account)
            .unwrap()
            .unwrap();
        format_pause_time(until)
    }

    /// A SQLite claim store plus a pre-account (`NULL`) row for `run_id`,
    /// written the way a pre-v11 store reads back.
    fn store_with_legacy_pause(dir: &TempDir, run_id: &str) -> (SqliteClaimStore, String) {
        let db = dir.path().join("claims.sqlite");
        let claims = SqliteClaimStore::open(&db).unwrap();
        let until = active_pause(&claims, "legacy", run_id, 3);
        let conn = rusqlite::Connection::open(&db).unwrap();
        let changed = conn
            .execute(
                "UPDATE orchestrator_session_pauses SET account = NULL WHERE run_id = ?1",
                [run_id],
            )
            .unwrap();
        assert_eq!(changed, 1);
        (claims, until)
    }

    fn expired_pause(claims: &dyn ClaimStore, account: &str, run_id: &str) {
        let then = Utc::now() - Duration::hours(3);
        claims
            .record_session_pause(then + Duration::hours(1), then, 86_400.0, run_id, account)
            .unwrap()
            .unwrap();
    }

    fn cleared_pause(claims: &dyn ClaimStore, account: &str, run_id: &str) {
        active_pause(claims, account, run_id, 4);
        let id = active_pauses(claims, Utc::now())
            .unwrap()
            .into_iter()
            .find(|entry| entry.run_id == run_id)
            .unwrap()
            .id;
        let cleared = clear_session_pauses(
            claims,
            &PauseSelector::Ids(vec![id]),
            Utc::now(),
            PAUSE_CLEAR_VIA_CLI,
            "operator cleared it",
        )
        .unwrap();
        assert_eq!(cleared, vec![id]);
    }

    #[test]
    fn pause_status_line_active_on_account_a() {
        let claims = InMemoryClaimStore::new();
        let until = active_pause(&claims, "A", "run-a", 2);
        assert_eq!(
            pause_status(&claims, false),
            format!("session-pause: @A until {until} (run run-a)\n(no active tasks)\n")
        );
    }

    #[test]
    fn pause_status_line_legacy_null_reads_all_accounts() {
        let dir = TempDir::new().unwrap();
        let (claims, until) = store_with_legacy_pause(&dir, "run-legacy");
        assert_eq!(
            pause_status(&claims, false),
            format!(
                "session-pause: all accounts until {until} (run run-legacy)\n(no active tasks)\n"
            )
        );
    }

    #[test]
    fn pause_status_line_default_account_reads_cli_login() {
        let claims = InMemoryClaimStore::new();
        let until = active_pause(&claims, DEFAULT_PAUSE_ACCOUNT, "run-d", 2);
        assert_eq!(
            pause_status(&claims, false),
            format!("session-pause: cli login until {until} (run run-d)\n(no active tasks)\n")
        );
    }

    #[test]
    fn pause_status_line_expired_prints_nothing() {
        let claims = InMemoryClaimStore::new();
        expired_pause(&claims, "A", "run-expired");
        assert_eq!(pause_status(&claims, false), "(no active tasks)\n");
    }

    #[test]
    fn pause_status_line_cleared_prints_nothing() {
        let claims = InMemoryClaimStore::new();
        cleared_pause(&claims, "A", "run-cleared");
        assert_eq!(pause_status(&claims, false), "(no active tasks)\n");
    }

    #[test]
    fn pause_status_line_exactly_one_per_active_pause_and_json_unchanged() {
        let dir = TempDir::new().unwrap();
        let (claims, legacy_until) = store_with_legacy_pause(&dir, "run-legacy");
        expired_pause(&claims, "A", "run-a-old");
        cleared_pause(&claims, "A", "run-a-cleared");
        let a_until = active_pause(&claims, "A", "run-a", 2);
        assert_eq!(
            pause_status(&claims, false),
            format!(
                "session-pause: @A until {a_until} (run run-a)\n\
                 session-pause: all accounts until {legacy_until} (run run-legacy)\n\
                 (no active tasks)\n"
            )
        );
        assert_eq!(pause_status(&claims, true), "[]\n");
    }

    #[test]
    fn pause_status_line_leads_the_task_listing() {
        let (tmp, store, claims) = scenario();
        let until = active_pause(&claims, "A", "run-a", 2);
        assert_eq!(
            run_status(&tmp, &store, &claims, false, false),
            format!("session-pause: @A until {until} (run run-a)\n{EXPECTED_TEXT}")
        );
        let expected = EXPECTED_JSON.replace("<TMP>", &tmp.path().display().to_string());
        assert_eq!(run_status(&tmp, &store, &claims, true, false), expected);
    }
}
