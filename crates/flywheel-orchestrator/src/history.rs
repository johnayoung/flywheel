//! Completed-run history: the read path over terminal lifecycles.
//!
//! The live surfaces (`flywheel live`, the fw console) deliberately show
//! only in-flight runs; once a run reaches a terminal status it leaves those
//! views, and `flywheel archive` then moves the task files out of
//! `active/`. Nothing is lost -- the store keeps every lifecycle, attempt,
//! event, and grader receipt -- but without this module there is no way to
//! *list* that history. [`collect_history_rows`] is that listing, and
//! [`collect_run_detail`] is the drill-in for one run.
//!
//! Grouping: a task retried across runs has several lifecycle rows; the
//! history surface shows one row per task (the most recently updated
//! terminal run) with the older runs carried as `prior_runs` so a
//! failed-then-succeeded task reads as one line, not noise.
//!
//! Phase attribution: every run seeded through the orchestrator records the
//! work item's `source_ref` on the lifecycle (`lifecycles.source`, schema
//! v12). For the directory work source that is the task-file path, and
//! [`phase_from_source`] derives the phase directory name from it. Runs
//! recorded before v12 have no source; callers may supply a
//! `fallback_phases` map (see [`build_task_phase_index`], which scans
//! `active/` and `archive/`) so pre-migration history still groups.
//!
//! The `history` / `show` verb renderers (text and JSON) live here too, so
//! the orchestrator owns the whole read surface end to end.

use chrono::{DateTime, Utc};
use flywheel_core::event_serde::{event_kind, event_payload, format_timestamp};
use flywheel_core::events::{DomainEvent, DomainEventKind, DomainEventPayload};
use flywheel_core::lifecycle::{Attempt, Lifecycle, Status};
use flywheel_core::store::{
    DomainEventStore, GraderResultRecord, GraderResultStore, LifecycleFilter, LifecycleStore,
    StoreError, TaskStore,
};
use flywheel_core::task::Task;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// A run is history once it can no longer move on its own. INTERRUPTED and
/// AWAITING_APPROVAL are parked-but-live (an operator owes an action) and
/// stay on the live surfaces instead.
pub const TERMINAL_STATUSES: [Status; 3] = [Status::Done, Status::Failed, Status::FailedValidation];

/// One terminal run's rolled-up summary.
///
/// Totals sum the per-attempt aggregate columns the harness maintains at
/// iteration boundaries -- the same relational rollup the live surface
/// reads, so live and history agree on a run's numbers. `finished_at` is
/// the terminal transition's timestamp; `started_at` is the earliest
/// recorded signal -- the folded transition stamps and the first attempt's
/// start. (Per-status stamps are overwritten when a retry re-enters a
/// status, so on a retried run only the attempt record still carries the
/// true start.)
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRun {
    pub run_id: String,
    pub task_id: String,
    pub status: Status,
    pub source: String,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub retries: u32,
    pub error: String,
    pub attempts: usize,
    pub tokens_total: u64,
    pub cost_usd_total: f64,
    pub turns_total: u64,
}

/// One task's history line: the latest terminal run plus its priors.
///
/// `phase` is the derived grouping label (`None` when the source is absent
/// or not phase-shaped). `prior_runs` are older terminal runs of the same
/// task, most recent first.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRow {
    pub task_id: String,
    pub phase: Option<String>,
    pub latest: HistoryRun,
    pub prior_runs: Vec<HistoryRun>,
}

/// One attempt's render-ready rollup for the `show` surface.
///
/// `grader_results` are that attempt's own receipts in ordinal order (empty
/// when the attempt was finalized before grading -- e.g. a protocol
/// failure). Keying each receipt set to its own attempt is what lets the
/// run detail carry every attempt's verdicts, not only the last.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptSummary {
    pub number: u32,
    pub outcome: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub iterations: u64,
    pub turns: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    pub error: String,
    pub grader_results: Vec<GraderResultRecord>,
}

/// Everything `show` renders for one run.
///
/// `task` is the exact definition the run pinned (content-addressed);
/// `None` when the pin cannot resolve. `grader_results` are *every*
/// attempt's receipts, ordered by attempt then ordinal (each record carries
/// its own `attempt_number`); the per-attempt split lives on each
/// [`AttemptSummary::grader_results`] so a retried run shows the verdicts
/// of the discarded attempt too. `related_runs` are the task's other runs
/// (any status), most recent first. `decisions` are the landing-stage
/// decision records from the domain-event ledger (spec 00073): gate
/// verdicts, parks, landings, redrives, in ascending-sequence order --
/// sourced from the store, never the telemetry file, so they survive JSONL
/// deletion.
#[derive(Debug, Clone, PartialEq)]
pub struct RunDetail {
    pub run: HistoryRun,
    pub phase: Option<String>,
    pub task: Option<Task>,
    pub agent_output: String,
    pub attempts: Vec<AttemptSummary>,
    pub grader_results: Vec<GraderResultRecord>,
    pub related_runs: Vec<HistoryRun>,
    pub decisions: Vec<DomainEvent>,
}

/// Derive the phase directory name from a recorded source label.
///
/// Only path-shaped sources from the directory layout
/// (`.../active/<phase>/<task>.json` or the post-archive
/// `.../archive/<phase>/<task>.json`) yield a phase: the file's parent
/// directory name. Tracker refs (`owner/repo#123`) and absent sources
/// return `None` -- the caller renders those ungrouped.
pub fn phase_from_source(source: Option<&str>) -> Option<String> {
    let source = source.filter(|s| !s.is_empty())?;
    let path = Path::new(source);
    if path.extension().is_none_or(|ext| ext != "json") {
        return None;
    }
    let parent = path.parent()?.file_name()?.to_string_lossy().into_owned();
    if matches!(parent.as_str(), "" | "active" | "archive" | "tasks") {
        return None;
    }
    Some(parent)
}

fn sorted_dir_entries(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = read.flatten().map(|e| e.path()).collect();
    entries.sort();
    entries
}

fn file_name_str(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Map task id -> phase by scanning `active/` and `archive/`.
///
/// The fallback for runs recorded before `lifecycles.source` existed
/// (schema v12): their lifecycle rows carry no provenance, but for the
/// directory layout the task files themselves still sit in (or were
/// archived into) a phase directory. Archive is scanned first so an active
/// re-issue of the same task id wins. Unreadable or id-less files are
/// skipped -- the index is best-effort by design.
pub fn build_task_phase_index(tasks_dir: impl AsRef<Path>) -> BTreeMap<String, String> {
    let mut index = BTreeMap::new();
    for bucket in ["archive", "active"] {
        let root = tasks_dir.as_ref().join(bucket);
        if !root.is_dir() {
            continue;
        }
        for phase_dir in sorted_dir_entries(&root) {
            let phase_name = file_name_str(&phase_dir);
            if !phase_dir.is_dir() || phase_name.starts_with('.') {
                continue;
            }
            for entry in sorted_dir_entries(&phase_dir) {
                let name = file_name_str(&entry);
                if !entry.is_file()
                    || entry.extension().is_none_or(|ext| ext != "json")
                    || name.starts_with('_')
                    || name.starts_with('.')
                {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&entry) else {
                    continue;
                };
                let Ok(data) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(task_id) = data.get("id").and_then(Value::as_str)
                    && !task_id.is_empty()
                {
                    index.insert(task_id.to_owned(), phase_name.clone());
                }
            }
        }
    }
    index
}

/// Roll one folded lifecycle into its [`HistoryRun`].
///
/// The attempts are the ones the store folded onto the lifecycle
/// (`list_lifecycles` / `load_lifecycle` populate them in ascending `number`
/// order), so no second per-run read is needed.
fn history_run_from_lifecycle(lifecycle: &Lifecycle) -> HistoryRun {
    let attempts: &[Attempt] = &lifecycle.attempts;
    // The terminal transition's timestamp is the honest finish instant. A
    // terminal run always records its terminal status' stamp; an unstamped
    // row degrades finished_at to None.
    let finished = lifecycle.timestamps.get(&lifecycle.status).copied();
    // The folded per-status stamps are overwritten when a retry re-enters
    // a status (READY/RUNNING), so min(stamps) alone reports the *latest*
    // attempt's start on a retried run. Attempt 1's start is the honest
    // run start; take the earliest signal available.
    let started = lifecycle
        .timestamps
        .values()
        .copied()
        .chain(attempts.first().map(|a| a.started_at))
        .min();
    HistoryRun {
        run_id: lifecycle.run_id.clone(),
        task_id: lifecycle.task_id.clone(),
        status: lifecycle.status,
        source: lifecycle.source.clone(),
        started_at: started,
        finished_at: finished,
        retries: lifecycle.retries,
        error: lifecycle.error.clone(),
        attempts: attempts.len(),
        tokens_total: attempts.iter().map(Attempt::total_tokens).sum(),
        cost_usd_total: attempts.iter().fold(0.0, |acc, a| acc + a.total_cost_usd),
        turns_total: attempts.iter().map(|a| a.turns).sum(),
    }
}

/// The filters for [`collect_history_rows`].
///
/// `statuses` narrows which terminal states qualify (default
/// [`TERMINAL_STATUSES`]); `phase` keeps only rows whose derived (or
/// fallback) phase matches; `limit` caps the number of *task rows* returned
/// (0 = no cap). `fallback_phases` supplies task-id -> phase attribution for
/// pre-v12 runs whose lifecycle carries no source (see
/// [`build_task_phase_index`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    pub statuses: Vec<Status>,
    pub phase: Option<String>,
    pub limit: usize,
    pub fallback_phases: BTreeMap<String, String>,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            statuses: TERMINAL_STATUSES.to_vec(),
            phase: None,
            limit: 0,
            fallback_phases: BTreeMap::new(),
        }
    }
}

impl HistoryQuery {
    pub fn with_statuses(mut self, statuses: impl IntoIterator<Item = Status>) -> Self {
        self.statuses = statuses.into_iter().collect();
        self
    }

    pub fn with_phase(mut self, phase: impl Into<String>) -> Self {
        self.phase = Some(phase.into());
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_fallback_phases(mut self, fallback: BTreeMap<String, String>) -> Self {
        self.fallback_phases = fallback;
        self
    }
}

fn derive_phase(
    source: &str,
    task_id: &str,
    fallback: &BTreeMap<String, String>,
) -> Option<String> {
    phase_from_source(Some(source)).or_else(|| fallback.get(task_id).cloned())
}

/// List finished work, one row per task, most recently finished first.
pub fn collect_history_rows<S: LifecycleStore + ?Sized>(
    store: &S,
    query: &HistoryQuery,
) -> Result<Vec<HistoryRow>, StoreError> {
    // list_lifecycles returns rows in (updated_at DESC, run_id DESC) order
    // across every backend, so the first row seen for a task is its latest
    // run and the rest are its priors, most recent first.
    let filter = LifecycleFilter::default().with_statuses(query.statuses.iter().copied());
    let mut rows: Vec<HistoryRow> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for lifecycle in store.list_lifecycles(&filter)? {
        let run = history_run_from_lifecycle(&lifecycle);
        match index.get(&run.task_id) {
            Some(&i) => rows[i].prior_runs.push(run),
            None => {
                let phase = derive_phase(&run.source, &run.task_id, &query.fallback_phases);
                index.insert(run.task_id.clone(), rows.len());
                rows.push(HistoryRow {
                    task_id: run.task_id.clone(),
                    phase,
                    latest: run,
                    prior_runs: Vec::new(),
                });
            }
        }
    }
    if let Some(phase) = &query.phase {
        rows.retain(|row| row.phase.as_deref() == Some(phase.as_str()));
    }
    // Most recently *finished* first; the terminal timestamp is the honest
    // sort key and rows without one sink to the bottom. The sort is stable
    // so ties keep the store's order.
    rows.sort_by(|a, b| b.latest.finished_at.cmp(&a.latest.finished_at));
    if query.limit > 0 {
        rows.truncate(query.limit);
    }
    Ok(rows)
}

/// Resolve a `show` argument to a run id.
///
/// A literal run id wins; otherwise the argument is treated as a task id
/// and the task's most recently updated lifecycle (any status) is chosen.
/// `None` when neither resolves.
pub fn resolve_run_id<S: LifecycleStore + ?Sized>(
    store: &S,
    run_or_task_id: &str,
) -> Result<Option<String>, StoreError> {
    if store.load_lifecycle(run_or_task_id)?.is_some() {
        return Ok(Some(run_or_task_id.to_owned()));
    }
    let filter = LifecycleFilter::default().with_task_id(run_or_task_id);
    Ok(store
        .list_lifecycles(&filter)?
        .into_iter()
        .next()
        .map(|lc| lc.run_id))
}

/// The landing-stage domain events the run-detail surface promotes to
/// first-class "decision" records: the held-out gate verdict, the land-park
/// witness, the positive landing, and the re-drive disposition. Every other
/// domain event is already surfaced through the lifecycle/attempts/grader
/// projections, so the decision list stays scoped to the trust decisions
/// the loop took at land time.
fn is_landing_decision(event: &DomainEvent) -> bool {
    matches!(
        event.kind(),
        DomainEventKind::HeldOutGateEvaluated
            | DomainEventKind::LandingParked
            | DomainEventKind::Landed
            | DomainEventKind::LandingRedriven
    )
}

/// Read the run's landing-stage decision records from the ledger.
///
/// Best-effort and read-only, mirroring the audit stream's ledger
/// projection: a ledger read that fails yields no decisions -- the ledger is
/// authoritative and this list is a disposable projection, so a lookup
/// failure must never break the run-detail surface.
fn landing_decisions<S: DomainEventStore + ?Sized>(store: &S, run_id: &str) -> Vec<DomainEvent> {
    store
        .list_domain_events(run_id)
        .map(|events| events.into_iter().filter(is_landing_decision).collect())
        .unwrap_or_default()
}

/// Assemble the full `show` payload for one run, or `None`.
///
/// Unlike [`collect_history_rows`] this accepts any status -- an operator
/// may inspect a parked or in-flight run too; the surface renders whatever
/// the row says.
pub fn collect_run_detail<S>(
    store: &S,
    run_id: &str,
    fallback_phases: &BTreeMap<String, String>,
) -> Result<Option<RunDetail>, StoreError>
where
    S: LifecycleStore + GraderResultStore + TaskStore + DomainEventStore + ?Sized,
{
    let Some(lifecycle) = store.load_lifecycle(run_id)? else {
        return Ok(None);
    };
    let run = history_run_from_lifecycle(&lifecycle);
    let phase = derive_phase(&run.source, &run.task_id, fallback_phases);

    let mut attempts = Vec::with_capacity(lifecycle.attempts.len());
    for a in &lifecycle.attempts {
        attempts.push(AttemptSummary {
            number: a.number,
            outcome: a
                .outcome
                .map_or_else(|| "(open)".to_owned(), |o| o.to_string()),
            started_at: Some(a.started_at),
            ended_at: a.ended_at,
            iterations: a.iterations_completed,
            turns: a.turns,
            tokens: a.total_tokens(),
            cost_usd: a.total_cost_usd,
            error: a.error.clone(),
            grader_results: store.list_grader_results(run_id, a.number)?,
        });
    }
    // Every attempt's receipts, not just the last one's. Each record
    // already carries its own attempt_number, so the flat concatenation
    // stays keyed by attempt.
    let grader_results: Vec<GraderResultRecord> = attempts
        .iter()
        .flat_map(|a| a.grader_results.iter().cloned())
        .collect();

    let related: Vec<HistoryRun> = store
        .list_lifecycles(&LifecycleFilter::default().with_task_id(&run.task_id))?
        .iter()
        .filter(|other| other.run_id != run_id)
        .map(history_run_from_lifecycle)
        .collect();

    Ok(Some(RunDetail {
        run,
        phase,
        task: store.load_task_for_run(run_id)?,
        agent_output: lifecycle.agent_output.clone(),
        attempts,
        grader_results,
        related_runs: related,
        decisions: landing_decisions(store, run_id),
    }))
}

// --- Renderers --------------------------------------------------------------------

/// `YYYY-MM-DD HH:MM`, or an em dash for an absent stamp.
pub fn format_history_ts(ts: Option<DateTime<Utc>>) -> String {
    match ts {
        Some(ts) => ts.format("%Y-%m-%d %H:%M").to_string(),
        None => "\u{2014}".to_owned(),
    }
}

/// Collapse newlines and clip to `limit` characters with a trailing
/// ellipsis (the operator-surface one-liner).
pub fn short(value: &str, limit: usize) -> String {
    let text: String = value
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if text.chars().count() <= limit {
        return text;
    }
    let keep = limit.saturating_sub(1).max(1);
    let mut clipped: String = text.chars().take(keep).collect();
    clipped.push('\u{2026}');
    clipped
}

fn iso(ts: Option<DateTime<Utc>>) -> Value {
    ts.map_or(Value::Null, |ts| Value::String(format_timestamp(&ts)))
}

/// One run as the `history --json` / `show --json` consumers see it.
pub fn history_run_to_json(run: &HistoryRun) -> Value {
    json!({
        "run_id": run.run_id,
        "task_id": run.task_id,
        "status": run.status.as_str(),
        "source": if run.source.is_empty() { Value::Null } else { Value::String(run.source.clone()) },
        "started_at": iso(run.started_at),
        "finished_at": iso(run.finished_at),
        "retries": run.retries,
        "error": run.error,
        "attempts": run.attempts,
        "tokens_total": run.tokens_total,
        "cost_usd_total": run.cost_usd_total,
        "turns_total": run.turns_total,
    })
}

fn history_label(row: &HistoryRow) -> String {
    match &row.phase {
        Some(phase) => format!("{phase}/{}", row.task_id),
        None => row.task_id.clone(),
    }
}

fn format_history_line(row: &HistoryRow, width: usize) -> String {
    let run = &row.latest;
    let runs_total = 1 + row.prior_runs.len();
    let suffix = if run.error.is_empty() {
        String::new()
    } else {
        format!("  -- {}", run.error)
    };
    format!(
        "  {:<width$}  {:<17} {}  runs={runs_total}  tokens={}  cost=${:.4}{suffix}",
        history_label(row),
        run.status.as_str(),
        format_history_ts(run.finished_at),
        run.tokens_total,
        run.cost_usd_total,
    )
}

/// The `history` verb's text listing: one aligned line per task row, or
/// `(no finished runs)`.
pub fn render_history_text(rows: &[HistoryRow]) -> String {
    if rows.is_empty() {
        return "(no finished runs)".to_owned();
    }
    let width = rows
        .iter()
        .map(|row| history_label(row).chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| format_history_line(row, width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `history --json` payload.
pub fn history_rows_to_json(rows: &[HistoryRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "task_id": row.task_id,
                    "phase": row.phase,
                    "latest": history_run_to_json(&row.latest),
                    "prior_runs": row.prior_runs.iter().map(history_run_to_json).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

fn grader_result_to_json(g: &GraderResultRecord) -> Value {
    json!({
        "attempt_number": g.attempt_number,
        "ordinal": g.ordinal,
        "grader_type": g.grader_type.as_str(),
        "grader_name": g.grader_name,
        "passed": g.passed,
        "duration_ms": g.duration_ms,
    })
}

/// One landing-stage decision event as a JSON object: a `kind`
/// discriminator, the ledger coordinates, and the event-specific payload
/// flattened beside them (the gate `outcome`/`receipts`, a park's
/// `park_kind`/`detail`, a landing's `strategy`/`landed_ref`, a redrive's
/// `result`). Rendered verbatim from the store.
pub fn decision_to_json(event: &DomainEvent) -> Value {
    let mut map = Map::new();
    map.insert(
        "kind".to_owned(),
        Value::String(event_kind(event).to_owned()),
    );
    map.insert("ts".to_owned(), Value::String(format_timestamp(&event.ts)));
    map.insert("attempt_number".to_owned(), json!(event.attempt_number));
    map.insert("sequence".to_owned(), json!(event.sequence));
    if let Ok(payload) = event_payload(&event.payload) {
        map.extend(payload);
    }
    Value::Object(map)
}

/// Format one decision event as human-readable text lines. The first line
/// is a `<kind>` summary; a gate verdict adds one indented line per
/// executed grader carrying its output excerpt.
fn format_decision_lines(event: &DomainEvent) -> Vec<String> {
    match &event.payload {
        DomainEventPayload::HeldOutGateEvaluated(gate) => {
            let mut summary = format!("  gate      {}", gate.outcome);
            if !gate.reason.is_empty() {
                summary.push_str(&format!("  {}", short(&gate.reason, 80)));
            }
            let mut lines = vec![summary];
            for receipt in &gate.receipts {
                let verdict = if receipt.passed { "pass" } else { "FAIL" };
                let name = receipt
                    .grader_name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .unwrap_or("(unnamed)");
                lines.push(format!(
                    "      {verdict}  {name}  {}",
                    short(&receipt.output_excerpt, 120)
                ));
            }
            lines
        }
        DomainEventPayload::LandingParked(parked) => {
            let mut line = format!("  parked    {}", parked.park_kind);
            if !parked.detail.is_empty() {
                line.push_str(&format!("  {}", short(&parked.detail, 80)));
            }
            vec![line]
        }
        DomainEventPayload::Landed(landed) => {
            vec![format!(
                "  landed    {}  {}",
                landed.strategy, landed.landed_ref
            )]
        }
        DomainEventPayload::LandingRedriven(redriven) => {
            let mut line = format!("  redriven  {}", redriven.result);
            if !redriven.park_kind.is_empty() {
                line.push_str(&format!("  (was {})", redriven.park_kind));
            }
            vec![line]
        }
        other => vec![format!("  {}", other.kind().as_str())],
    }
}

/// The `show` verb's text report for one run.
pub fn render_run_detail_text(detail: &RunDetail) -> String {
    let run = &detail.run;
    let mut lines = vec![
        format!("task     : {}", run.task_id),
        format!(
            "phase    : {}",
            detail.phase.as_deref().unwrap_or("\u{2014}")
        ),
        format!("run      : {}", run.run_id),
        format!("status   : {}", run.status.as_str()),
    ];
    if !run.source.is_empty() {
        lines.push(format!("source   : {}", run.source));
    }
    lines.push(format!("started  : {}", format_history_ts(run.started_at)));
    lines.push(format!("finished : {}", format_history_ts(run.finished_at)));
    lines.push(format!("retries  : {}", run.retries));
    lines.push(format!(
        "totals   : tokens={} cost=${:.4} turns={}",
        run.tokens_total, run.cost_usd_total, run.turns_total
    ));
    if !run.error.is_empty() {
        lines.push(format!("error    : {}", run.error));
    }
    if let Some(task) = &detail.task {
        lines.push(format!("goal     : {}", task.goal));
    }
    if !detail.attempts.is_empty() {
        lines.push("attempts :".to_owned());
        for a in &detail.attempts {
            let mut line = format!(
                "  {}  {:<18} iter={} turns={} tokens={} cost=${:.4}",
                a.number, a.outcome, a.iterations, a.turns, a.tokens, a.cost_usd
            );
            if !a.error.is_empty() {
                line.push_str(&format!("  -- {}", short(&a.error, 80)));
            }
            lines.push(line);
            for g in &a.grader_results {
                let verdict = if g.passed { "pass" } else { "FAIL" };
                let grader_type = g.grader_type.as_str();
                let name = g
                    .grader_name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .unwrap_or(grader_type);
                lines.push(format!(
                    "      {verdict}  {grader_type:<10} {name}  ({} ms)",
                    g.duration_ms
                ));
            }
        }
    }
    if !detail.decisions.is_empty() {
        lines.push("decisions:".to_owned());
        for event in &detail.decisions {
            lines.extend(format_decision_lines(event));
        }
    }
    if !detail.agent_output.is_empty() {
        lines.push("agent output:".to_owned());
        lines.push(detail.agent_output.clone());
    }
    if !detail.related_runs.is_empty() {
        lines.push("related runs:".to_owned());
        for r in &detail.related_runs {
            lines.push(format!(
                "  {}  {:<17} {}",
                r.run_id,
                r.status.as_str(),
                format_history_ts(r.finished_at)
            ));
        }
    }
    lines.join("\n")
}

/// The `show --json` payload.
pub fn run_detail_to_json(detail: &RunDetail) -> Value {
    json!({
        "run": history_run_to_json(&detail.run),
        "phase": detail.phase,
        "goal": detail.task.as_ref().map(|t| t.goal.clone()),
        "agent_output": detail.agent_output,
        "attempts": detail.attempts.iter().map(|a| json!({
            "number": a.number,
            "outcome": a.outcome,
            "started_at": iso(a.started_at),
            "ended_at": iso(a.ended_at),
            "iterations": a.iterations,
            "turns": a.turns,
            "tokens": a.tokens,
            "cost_usd": a.cost_usd,
            "error": a.error,
            "grader_results": a.grader_results.iter().map(grader_result_to_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "grader_results": detail.grader_results.iter().map(grader_result_to_json).collect::<Vec<_>>(),
        "related_runs": detail.related_runs.iter().map(history_run_to_json).collect::<Vec<_>>(),
        "decisions": detail.decisions.iter().map(decision_to_json).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use flywheel_core::events::{
        GateGraderReceipt, HeldOutGateEvaluated, Landed, LandingParked, LandingRedriven,
        LandingRouted,
    };
    use flywheel_core::lifecycle::Outcome;
    use flywheel_core::store::{AttemptStore, DomainEventStore};
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::GraderType;
    use tempfile::TempDir;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn seed_terminal(
        store: &InMemoryStore,
        task_id: &str,
        run_id: &str,
        terminal: Status,
        source: &str,
        at: DateTime<Utc>,
        error: &str,
    ) -> Lifecycle {
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        lc.source = source.to_owned();
        lc.transition_to(Status::Ready, "", Some(at)).unwrap();
        lc.transition_to(Status::Running, "", Some(at + Duration::seconds(1)))
            .unwrap();
        if terminal == Status::Failed {
            lc.transition_to(Status::Failed, error, Some(at + Duration::minutes(5)))
                .unwrap();
        } else {
            lc.transition_to(Status::Validating, "", Some(at + Duration::seconds(2)))
                .unwrap();
            let err = if terminal == Status::Done { "" } else { error };
            lc.transition_to(terminal, err, Some(at + Duration::minutes(5)))
                .unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    fn attempt(run_id: &str, number: u32, tokens: u64, cost: f64, turns: u64) -> Attempt {
        let mut a = Attempt::new(number, t0(), run_id);
        a.ended_at = Some(t0() + Duration::minutes(4));
        a.outcome = Some(Outcome::Succeeded);
        a.input_tokens = tokens;
        a.iterations_completed = 2;
        a.turns = turns;
        a.total_cost_usd = cost;
        a
    }

    fn grader(
        run_id: &str,
        number: u32,
        ordinal: u32,
        passed: bool,
        name: Option<&str>,
    ) -> GraderResultRecord {
        GraderResultRecord {
            run_id: run_id.to_owned(),
            attempt_number: number,
            ordinal,
            grader_type: GraderType::Command,
            grader_spec: Map::new(),
            passed,
            duration_ms: 1,
            payload: Map::new(),
            ts: t0(),
            grader_name: name.map(str::to_owned),
            id: None,
        }
    }

    fn seed_live(store: &InMemoryStore, task_id: &str, run_id: &str) {
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        lc.transition_to(Status::Ready, "", Some(t0())).unwrap();
        lc.transition_to(Status::Running, "", Some(t0())).unwrap();
        store.create_lifecycle(&lc).unwrap();
    }

    // --- phase_from_source --------------------------------------------------

    #[test]
    fn phase_from_source_derives_directory_phase() {
        assert_eq!(
            phase_from_source(Some(".flywheel/tasks/active/30-history/t1.json")).as_deref(),
            Some("30-history")
        );
        assert_eq!(
            phase_from_source(Some(".flywheel/tasks/archive/05-audit/t2.json")).as_deref(),
            Some("05-audit")
        );
    }

    #[test]
    fn phase_from_source_rejects_non_phase_shapes() {
        assert_eq!(phase_from_source(None), None);
        assert_eq!(phase_from_source(Some("")), None);
        assert_eq!(phase_from_source(Some("owner/repo#123")), None);
        assert_eq!(phase_from_source(Some("t1.json")), None);
        assert_eq!(
            phase_from_source(Some(".flywheel/tasks/active/t1.json")),
            None
        );
        assert_eq!(phase_from_source(Some("tasks/t1.json")), None);
    }

    // --- collect_history_rows -----------------------------------------------

    #[test]
    fn history_lists_only_terminal_runs() {
        let store = InMemoryStore::new();
        seed_terminal(&store, "t-done", "run-1", Status::Done, "", t0(), "boom");
        seed_terminal(
            &store,
            "t-failed",
            "run-2",
            Status::Failed,
            "",
            t0(),
            "boom",
        );
        seed_live(&store, "t-live", "run-3");
        let rows = collect_history_rows(&store, &HistoryQuery::default()).unwrap();
        let mut ids: Vec<&str> = rows.iter().map(|r| r.task_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["t-done", "t-failed"]);
    }

    #[test]
    fn history_groups_retried_task_into_one_row() {
        let store = InMemoryStore::new();
        seed_terminal(&store, "t1", "run-old", Status::Failed, "", t0(), "boom");
        seed_terminal(
            &store,
            "t1",
            "run-new",
            Status::Done,
            "",
            t0() + Duration::hours(1),
            "",
        );
        let rows = collect_history_rows(&store, &HistoryQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].latest.run_id, "run-new");
        assert_eq!(rows[0].latest.status, Status::Done);
        let priors: Vec<&str> = rows[0]
            .prior_runs
            .iter()
            .map(|r| r.run_id.as_str())
            .collect();
        assert_eq!(priors, vec!["run-old"]);
    }

    #[test]
    fn history_orders_most_recently_finished_first() {
        let store = InMemoryStore::new();
        seed_terminal(&store, "t-early", "run-a", Status::Done, "", t0(), "");
        seed_terminal(
            &store,
            "t-late",
            "run-b",
            Status::Done,
            "",
            t0() + Duration::hours(2),
            "",
        );
        let rows = collect_history_rows(&store, &HistoryQuery::default()).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.task_id.as_str()).collect();
        assert_eq!(ids, vec!["t-late", "t-early"]);
    }

    #[test]
    fn history_derives_phase_and_honors_fallback() {
        let store = InMemoryStore::new();
        seed_terminal(
            &store,
            "t-sourced",
            "run-s",
            Status::Done,
            ".flywheel/tasks/active/30-history/t-sourced.json",
            t0(),
            "",
        );
        seed_terminal(&store, "t-legacy", "run-l", Status::Done, "", t0(), "");
        let query = HistoryQuery::default().with_fallback_phases(BTreeMap::from([(
            "t-legacy".to_owned(),
            "07-readme-onboarding".to_owned(),
        )]));
        let rows = collect_history_rows(&store, &query).unwrap();
        let by_task: BTreeMap<&str, Option<&str>> = rows
            .iter()
            .map(|r| (r.task_id.as_str(), r.phase.as_deref()))
            .collect();
        assert_eq!(by_task["t-sourced"], Some("30-history"));
        assert_eq!(by_task["t-legacy"], Some("07-readme-onboarding"));
    }

    #[test]
    fn history_filters_by_status_phase_and_limit() {
        let store = InMemoryStore::new();
        seed_terminal(
            &store,
            "t1",
            "run-1",
            Status::Done,
            ".flywheel/tasks/active/30-a/t1.json",
            t0() + Duration::hours(2),
            "",
        );
        seed_terminal(
            &store,
            "t2",
            "run-2",
            Status::Failed,
            ".flywheel/tasks/active/31-b/t2.json",
            t0() + Duration::hours(1),
            "boom",
        );
        let ids = |rows: &[HistoryRow]| -> Vec<String> {
            rows.iter().map(|r| r.task_id.clone()).collect()
        };
        let failed_only = collect_history_rows(
            &store,
            &HistoryQuery::default().with_statuses([Status::Failed]),
        )
        .unwrap();
        assert_eq!(ids(&failed_only), vec!["t2"]);
        let phase_only =
            collect_history_rows(&store, &HistoryQuery::default().with_phase("30-a")).unwrap();
        assert_eq!(ids(&phase_only), vec!["t1"]);
        let capped = collect_history_rows(&store, &HistoryQuery::default().with_limit(1)).unwrap();
        assert_eq!(ids(&capped), vec!["t1"]);
    }

    #[test]
    fn history_rolls_up_attempt_totals() {
        let store = InMemoryStore::new();
        seed_terminal(&store, "t1", "run-1", Status::Done, "", t0(), "");
        store
            .save_attempt("run-1", &attempt("run-1", 1, 1000, 0.5, 7), None)
            .unwrap();
        store
            .save_attempt("run-1", &attempt("run-1", 2, 400, 0.25, 3), None)
            .unwrap();
        let rows = collect_history_rows(&store, &HistoryQuery::default()).unwrap();
        let row = &rows[0];
        assert_eq!(row.latest.attempts, 2);
        assert_eq!(row.latest.tokens_total, 1400);
        assert_eq!(row.latest.cost_usd_total, 0.75);
        assert_eq!(row.latest.turns_total, 10);
        assert!(row.latest.started_at.unwrap() < row.latest.finished_at.unwrap());
    }

    #[test]
    fn history_started_at_survives_retry_stamp_overwrite() {
        let store = InMemoryStore::new();
        let mut lc = Lifecycle::new("t-retry").with_run_id("run-retry");
        let stamps = [
            (Status::Ready, "", 0),
            (Status::Running, "", 1),
            (Status::Validating, "", 18 * 60),
            (Status::FailedValidation, "protocol failure", 19 * 60),
            (Status::Ready, "", 20 * 60),
            (Status::Running, "", 21 * 60),
            (Status::Validating, "", 29 * 60),
            (Status::Done, "", 30 * 60),
        ];
        for (status, error, secs) in stamps {
            lc.transition_to(status, error, Some(t0() + Duration::seconds(secs)))
                .unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        let mut a1 = Attempt::new(1, t0() + Duration::seconds(1), "run-retry");
        a1.ended_at = Some(t0() + Duration::minutes(19));
        a1.outcome = Some(Outcome::AgentError);
        let mut a2 = Attempt::new(2, t0() + Duration::minutes(21), "run-retry");
        a2.ended_at = Some(t0() + Duration::minutes(30));
        a2.outcome = Some(Outcome::Succeeded);
        store.save_attempt("run-retry", &a1, None).unwrap();
        store.save_attempt("run-retry", &a2, None).unwrap();

        let rows = collect_history_rows(&store, &HistoryQuery::default()).unwrap();
        assert_eq!(rows[0].latest.started_at, Some(t0() + Duration::seconds(1)));
        assert_eq!(
            rows[0].latest.finished_at,
            Some(t0() + Duration::minutes(30))
        );
        assert_eq!(rows[0].latest.retries, 1);
    }

    // --- build_task_phase_index ---------------------------------------------

    #[test]
    fn phase_index_scans_active_and_archive() {
        let dir = TempDir::new().unwrap();
        let tasks_dir = dir.path().join("tasks");
        let write = |rel: &str, body: &str| {
            let path = tasks_dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write(
            "archive/05-old/t-archived.json",
            r#"{"id": "t-archived", "goal": "g"}"#,
        );
        write(
            "active/30-new/t-active.json",
            r#"{"id": "t-active", "goal": "g"}"#,
        );
        // A re-issued task id present in both buckets: active wins.
        write(
            "active/31-redo/t-archived.json",
            r#"{"id": "t-archived", "goal": "g"}"#,
        );
        // Garbage, hidden, and underscore-prefixed files are skipped.
        write("archive/05-old/broken.json", "{not json");
        write("archive/05-old/_notes.json", r#"{"id": "hidden"}"#);
        write("active/.hidden/x.json", r#"{"id": "hidden2"}"#);
        write("active/30-new/no-id.json", r#"{"goal": "g"}"#);
        let index = build_task_phase_index(&tasks_dir);
        assert_eq!(
            index,
            BTreeMap::from([
                ("t-archived".to_owned(), "31-redo".to_owned()),
                ("t-active".to_owned(), "30-new".to_owned()),
            ])
        );
        assert!(build_task_phase_index(dir.path().join("missing")).is_empty());
    }

    // --- resolve_run_id / collect_run_detail --------------------------------

    #[test]
    fn resolve_run_id_accepts_run_or_task_id() {
        let store = InMemoryStore::new();
        seed_terminal(&store, "t1", "run-old", Status::Done, "", t0(), "");
        seed_terminal(
            &store,
            "t1",
            "run-new",
            Status::Done,
            "",
            t0() + Duration::hours(1),
            "",
        );
        assert_eq!(
            resolve_run_id(&store, "run-old").unwrap().as_deref(),
            Some("run-old")
        );
        assert_eq!(
            resolve_run_id(&store, "t1").unwrap().as_deref(),
            Some("run-new")
        );
        assert_eq!(resolve_run_id(&store, "nope").unwrap(), None);
    }

    #[test]
    fn run_detail_carries_attempts_graders_and_related_runs() {
        let store = InMemoryStore::new();
        seed_terminal(&store, "t1", "run-old", Status::Failed, "", t0(), "boom");
        let mut lc = seed_terminal(
            &store,
            "t1",
            "run-new",
            Status::Done,
            ".flywheel/tasks/active/30-a/t1.json",
            t0() + Duration::hours(1),
            "",
        );
        let version = lc.version;
        lc.agent_output = "All done.".to_owned();
        lc.version += 1;
        store.update_lifecycle(&lc, version).unwrap();
        store
            .save_attempt("run-new", &attempt("run-new", 1, 1000, 0.5, 7), None)
            .unwrap();
        store
            .append_grader_result(&grader("run-new", 1, 0, true, Some("smoke")))
            .unwrap();

        let detail = collect_run_detail(&store, "run-new", &BTreeMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(detail.run.status, Status::Done);
        assert_eq!(detail.phase.as_deref(), Some("30-a"));
        assert_eq!(detail.agent_output, "All done.");
        assert_eq!(detail.attempts.len(), 1);
        assert_eq!(detail.attempts[0].outcome, "succeeded");
        let names: Vec<Option<&str>> = detail
            .grader_results
            .iter()
            .map(|g| g.grader_name.as_deref())
            .collect();
        assert_eq!(names, vec![Some("smoke")]);
        let related: Vec<&str> = detail
            .related_runs
            .iter()
            .map(|r| r.run_id.as_str())
            .collect();
        assert_eq!(related, vec!["run-old"]);
        assert!(detail.task.is_none());
        assert!(detail.decisions.is_empty());
        assert!(
            collect_run_detail(&store, "run-missing", &BTreeMap::new())
                .unwrap()
                .is_none()
        );
    }

    /// A run that failed verification on attempt 1 and passed on attempt 2,
    /// with landing decisions appended to its ledger. Mirrors the Python
    /// fixture the embedded renderer expectations were generated from.
    fn seed_two_attempt_run(store: &InMemoryStore, run_id: &str, task_id: &str, source: &str) {
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        lc.source = source.to_owned();
        let base = t0() + Duration::hours(1);
        let stamps = [
            (Status::Ready, "", 0),
            (Status::Running, "", 1),
            (Status::Validating, "", 5 * 60),
            (Status::FailedValidation, "graders failed", 6 * 60),
            (Status::Ready, "", 10 * 60),
            (Status::Running, "", 11 * 60),
            (Status::Validating, "", 15 * 60),
            (Status::Done, "", 16 * 60),
        ];
        for (status, error, secs) in stamps {
            lc.transition_to(status, error, Some(base + Duration::seconds(secs)))
                .unwrap();
        }
        lc.agent_output = "All done.\nSecond line.".to_owned();
        store.create_lifecycle(&lc).unwrap();

        let mut a1 = Attempt::new(1, base + Duration::seconds(1), run_id);
        a1.ended_at = Some(base + Duration::minutes(6));
        a1.outcome = Some(Outcome::ValidationFailed);
        a1.input_tokens = 1000;
        a1.iterations_completed = 1;
        a1.turns = 5;
        a1.total_cost_usd = 0.5;
        a1.error = "grader alpha failed\nwith a very long explanation that should be \
                    truncated by the short helper at eighty chars"
            .to_owned();
        let mut a2 = Attempt::new(2, base + Duration::minutes(11), run_id);
        a2.ended_at = Some(base + Duration::minutes(16));
        a2.outcome = Some(Outcome::Succeeded);
        a2.input_tokens = 400;
        a2.output_tokens = 50;
        a2.iterations_completed = 1;
        a2.turns = 3;
        a2.total_cost_usd = 0.25;
        store.save_attempt(run_id, &a1, None).unwrap();
        store.save_attempt(run_id, &a2, None).unwrap();
        store
            .append_grader_result(&grader(run_id, 1, 0, false, Some("alpha")))
            .unwrap();
        store
            .append_grader_result(&grader(run_id, 2, 0, true, Some("alpha")))
            .unwrap();
        store
            .append_grader_result(&grader(run_id, 2, 1, true, None))
            .unwrap();

        let ts = base + Duration::minutes(17);
        let decisions = [
            DomainEventPayload::HeldOutGateEvaluated(HeldOutGateEvaluated {
                outcome: "pass".to_owned(),
                reason: "all held-out graders passed".to_owned(),
                receipts: vec![
                    GateGraderReceipt {
                        grader_name: Some("oracle".to_owned()),
                        passed: true,
                        output_excerpt: "3 passed\nin 0.1s".to_owned(),
                    },
                    GateGraderReceipt {
                        grader_name: None,
                        passed: false,
                        output_excerpt: String::new(),
                    },
                ],
            }),
            DomainEventPayload::LandingParked(LandingParked {
                park_kind: "divergent-base".to_owned(),
                detail: "rebase conflict in src/x.rs".to_owned(),
                ..LandingParked::default()
            }),
            DomainEventPayload::LandingRedriven(LandingRedriven {
                result: "landed".to_owned(),
                park_kind: "divergent-base".to_owned(),
            }),
            DomainEventPayload::Landed(Landed {
                strategy: "merge".to_owned(),
                landed_ref: "abc123".to_owned(),
                ..Landed::default()
            }),
            // Routing is not a landing decision; the surface must drop it.
            DomainEventPayload::LandingRouted(LandingRouted {
                winning_tier: 1,
                strategy: "merge".to_owned(),
                per_file_tiers: BTreeMap::new(),
            }),
        ];
        let mut version = store.load_lifecycle(run_id).unwrap().unwrap().version;
        for (i, payload) in decisions.into_iter().enumerate() {
            let event = DomainEvent::new(run_id, ts + Duration::seconds(i as i64), payload);
            version = store.append_domain_event(&event, version).unwrap().version;
        }
    }

    fn fixture_store() -> InMemoryStore {
        let store = InMemoryStore::new();
        seed_terminal(&store, "t1", "run-old", Status::Failed, "", t0(), "boom");
        seed_two_attempt_run(&store, "run-x", "t1", ".flywheel/tasks/active/30-a/t1.json");
        seed_terminal(
            &store,
            "t2",
            "run-2",
            Status::Failed,
            ".flywheel/tasks/active/31-b/t2.json",
            t0() + Duration::hours(2),
            "boom\nsecond",
        );
        seed_terminal(
            &store,
            "t3",
            "run-3",
            Status::FailedValidation,
            "",
            t0() - Duration::hours(1),
            "bad",
        );
        seed_live(&store, "t-live", "run-live");
        let mut open = Attempt::new(1, t0() + Duration::hours(2), "run-2");
        open.input_tokens = 7;
        open.total_cost_usd = 0.0001;
        store.save_attempt("run-2", &open, None).unwrap();
        store
    }

    fn fixture_rows() -> Vec<HistoryRow> {
        let query = HistoryQuery::default()
            .with_fallback_phases(BTreeMap::from([("t3".to_owned(), "07-legacy".to_owned())]));
        collect_history_rows(&fixture_store(), &query).unwrap()
    }

    #[test]
    fn run_detail_keys_grader_verdicts_by_attempt_and_spans_totals() {
        let store = InMemoryStore::new();
        seed_two_attempt_run(&store, "run-x", "t1", "");
        let detail = collect_run_detail(&store, "run-x", &BTreeMap::new())
            .unwrap()
            .unwrap();
        let verdicts = |n: usize| -> Vec<(Option<&str>, bool)> {
            detail.attempts[n]
                .grader_results
                .iter()
                .map(|g| (g.grader_name.as_deref(), g.passed))
                .collect()
        };
        assert_eq!(verdicts(0), vec![(Some("alpha"), false)]);
        assert_eq!(verdicts(1), vec![(Some("alpha"), true), (None, true)]);
        assert!(
            detail.attempts[0]
                .grader_results
                .iter()
                .all(|g| g.attempt_number == 1)
        );
        assert!(
            detail.attempts[1]
                .grader_results
                .iter()
                .all(|g| g.attempt_number == 2)
        );
        assert_eq!(detail.grader_results.len(), 3);
        assert_eq!(detail.run.tokens_total, 1450);
        assert!(detail.run.started_at.unwrap() <= t0() + Duration::hours(1) + Duration::seconds(1));
        let kinds: Vec<&str> = detail.decisions.iter().map(event_kind).collect();
        assert_eq!(
            kinds,
            vec![
                "held_out_gate_evaluated",
                "landing_parked",
                "landing_redriven",
                "landed"
            ]
        );
    }

    // --- renderers: byte-identical to the Python CLI --------------------------

    #[test]
    fn history_text_matches_python_byte_for_byte() {
        let expected = "  31-b/t2       failed            2026-06-01 14:05  runs=1  tokens=7  cost=$0.0001  -- boom\nsecond\n  30-a/t1       done              2026-06-01 13:16  runs=2  tokens=1450  cost=$0.7500\n  07-legacy/t3  failed_validation 2026-06-01 11:05  runs=1  tokens=0  cost=$0.0000  -- bad";
        assert_eq!(render_history_text(&fixture_rows()), expected);
        assert_eq!(render_history_text(&[]), "(no finished runs)");
    }

    #[test]
    fn history_json_matches_python_byte_for_byte() {
        let expected = r#"[
  {
    "task_id": "t2",
    "phase": "31-b",
    "latest": {
      "run_id": "run-2",
      "task_id": "t2",
      "status": "failed",
      "source": ".flywheel/tasks/active/31-b/t2.json",
      "started_at": "2026-06-01T14:00:00+00:00",
      "finished_at": "2026-06-01T14:05:00+00:00",
      "retries": 0,
      "error": "boom\nsecond",
      "attempts": 1,
      "tokens_total": 7,
      "cost_usd_total": 0.0001,
      "turns_total": 0
    },
    "prior_runs": []
  },
  {
    "task_id": "t1",
    "phase": "30-a",
    "latest": {
      "run_id": "run-x",
      "task_id": "t1",
      "status": "done",
      "source": ".flywheel/tasks/active/30-a/t1.json",
      "started_at": "2026-06-01T13:00:01+00:00",
      "finished_at": "2026-06-01T13:16:00+00:00",
      "retries": 1,
      "error": "",
      "attempts": 2,
      "tokens_total": 1450,
      "cost_usd_total": 0.75,
      "turns_total": 8
    },
    "prior_runs": [
      {
        "run_id": "run-old",
        "task_id": "t1",
        "status": "failed",
        "source": null,
        "started_at": "2026-06-01T12:00:00+00:00",
        "finished_at": "2026-06-01T12:05:00+00:00",
        "retries": 0,
        "error": "boom",
        "attempts": 0,
        "tokens_total": 0,
        "cost_usd_total": 0.0,
        "turns_total": 0
      }
    ]
  },
  {
    "task_id": "t3",
    "phase": "07-legacy",
    "latest": {
      "run_id": "run-3",
      "task_id": "t3",
      "status": "failed_validation",
      "source": null,
      "started_at": "2026-06-01T11:00:00+00:00",
      "finished_at": "2026-06-01T11:05:00+00:00",
      "retries": 0,
      "error": "bad",
      "attempts": 0,
      "tokens_total": 0,
      "cost_usd_total": 0.0,
      "turns_total": 0
    },
    "prior_runs": []
  }
]"#;
        let json = serde_json::to_string_pretty(&history_rows_to_json(&fixture_rows())).unwrap();
        assert_eq!(json, expected);
    }

    #[test]
    fn show_text_matches_python_byte_for_byte() {
        let expected = "task     : t1
phase    : 30-a
run      : run-x
status   : done
source   : .flywheel/tasks/active/30-a/t1.json
started  : 2026-06-01 13:00
finished : 2026-06-01 13:16
retries  : 1
totals   : tokens=1450 cost=$0.7500 turns=8
attempts :
  1  validation_failed  iter=1 turns=5 tokens=1000 cost=$0.5000  -- grader alpha failed with a very long explanation that should be truncated by th\u{2026}
      FAIL  command    alpha  (1 ms)
  2  succeeded          iter=1 turns=3 tokens=450 cost=$0.2500
      pass  command    alpha  (1 ms)
      pass  command    command  (1 ms)
decisions:
  gate      pass  all held-out graders passed
      pass  oracle  3 passed in 0.1s
      FAIL  (unnamed)  \n  parked    divergent-base  rebase conflict in src/x.rs
  redriven  landed  (was divergent-base)
  landed    merge  abc123
agent output:
All done.
Second line.
related runs:
  run-old  failed            2026-06-01 12:05";
        let store = fixture_store();
        let detail = collect_run_detail(&store, "run-x", &BTreeMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(render_run_detail_text(&detail), expected);

        let expected_open = "task     : t2
phase    : 31-b
run      : run-2
status   : failed
source   : .flywheel/tasks/active/31-b/t2.json
started  : 2026-06-01 14:00
finished : 2026-06-01 14:05
retries  : 0
totals   : tokens=7 cost=$0.0001 turns=0
error    : boom
second
attempts :
  1  (open)             iter=0 turns=0 tokens=7 cost=$0.0001";
        let detail = collect_run_detail(&store, "run-2", &BTreeMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(render_run_detail_text(&detail), expected_open);
    }

    #[test]
    fn show_json_matches_python_byte_for_byte() {
        let expected = r#"{
  "run": {
    "run_id": "run-x",
    "task_id": "t1",
    "status": "done",
    "source": ".flywheel/tasks/active/30-a/t1.json",
    "started_at": "2026-06-01T13:00:01+00:00",
    "finished_at": "2026-06-01T13:16:00+00:00",
    "retries": 1,
    "error": "",
    "attempts": 2,
    "tokens_total": 1450,
    "cost_usd_total": 0.75,
    "turns_total": 8
  },
  "phase": "30-a",
  "goal": null,
  "agent_output": "All done.\nSecond line.",
  "attempts": [
    {
      "number": 1,
      "outcome": "validation_failed",
      "started_at": "2026-06-01T13:00:01+00:00",
      "ended_at": "2026-06-01T13:06:00+00:00",
      "iterations": 1,
      "turns": 5,
      "tokens": 1000,
      "cost_usd": 0.5,
      "error": "grader alpha failed\nwith a very long explanation that should be truncated by the short helper at eighty chars",
      "grader_results": [
        {
          "attempt_number": 1,
          "ordinal": 0,
          "grader_type": "command",
          "grader_name": "alpha",
          "passed": false,
          "duration_ms": 1
        }
      ]
    },
    {
      "number": 2,
      "outcome": "succeeded",
      "started_at": "2026-06-01T13:11:00+00:00",
      "ended_at": "2026-06-01T13:16:00+00:00",
      "iterations": 1,
      "turns": 3,
      "tokens": 450,
      "cost_usd": 0.25,
      "error": "",
      "grader_results": [
        {
          "attempt_number": 2,
          "ordinal": 0,
          "grader_type": "command",
          "grader_name": "alpha",
          "passed": true,
          "duration_ms": 1
        },
        {
          "attempt_number": 2,
          "ordinal": 1,
          "grader_type": "command",
          "grader_name": null,
          "passed": true,
          "duration_ms": 1
        }
      ]
    }
  ],
  "grader_results": [
    {
      "attempt_number": 1,
      "ordinal": 0,
      "grader_type": "command",
      "grader_name": "alpha",
      "passed": false,
      "duration_ms": 1
    },
    {
      "attempt_number": 2,
      "ordinal": 0,
      "grader_type": "command",
      "grader_name": "alpha",
      "passed": true,
      "duration_ms": 1
    },
    {
      "attempt_number": 2,
      "ordinal": 1,
      "grader_type": "command",
      "grader_name": null,
      "passed": true,
      "duration_ms": 1
    }
  ],
  "related_runs": [
    {
      "run_id": "run-old",
      "task_id": "t1",
      "status": "failed",
      "source": null,
      "started_at": "2026-06-01T12:00:00+00:00",
      "finished_at": "2026-06-01T12:05:00+00:00",
      "retries": 0,
      "error": "boom",
      "attempts": 0,
      "tokens_total": 0,
      "cost_usd_total": 0.0,
      "turns_total": 0
    }
  ],
  "decisions": [
    {
      "kind": "held_out_gate_evaluated",
      "ts": "2026-06-01T13:17:00+00:00",
      "attempt_number": null,
      "sequence": 1,
      "outcome": "pass",
      "reason": "all held-out graders passed",
      "receipts": [
        {
          "grader_name": "oracle",
          "passed": true,
          "output_excerpt": "3 passed\nin 0.1s"
        },
        {
          "grader_name": null,
          "passed": false,
          "output_excerpt": ""
        }
      ]
    },
    {
      "kind": "landing_parked",
      "ts": "2026-06-01T13:17:01+00:00",
      "attempt_number": null,
      "sequence": 2,
      "park_kind": "divergent-base",
      "detail": "rebase conflict in src/x.rs"
    },
    {
      "kind": "landing_redriven",
      "ts": "2026-06-01T13:17:02+00:00",
      "attempt_number": null,
      "sequence": 3,
      "result": "landed",
      "park_kind": "divergent-base"
    },
    {
      "kind": "landed",
      "ts": "2026-06-01T13:17:03+00:00",
      "attempt_number": null,
      "sequence": 4,
      "strategy": "merge",
      "landed_ref": "abc123"
    }
  ]
}"#;
        let store = fixture_store();
        let detail = collect_run_detail(&store, "run-x", &BTreeMap::new())
            .unwrap()
            .unwrap();
        let json = serde_json::to_string_pretty(&run_detail_to_json(&detail)).unwrap();
        assert_eq!(json, expected);

        let expected_open = r#"{
  "run": {
    "run_id": "run-2",
    "task_id": "t2",
    "status": "failed",
    "source": ".flywheel/tasks/active/31-b/t2.json",
    "started_at": "2026-06-01T14:00:00+00:00",
    "finished_at": "2026-06-01T14:05:00+00:00",
    "retries": 0,
    "error": "boom\nsecond",
    "attempts": 1,
    "tokens_total": 7,
    "cost_usd_total": 0.0001,
    "turns_total": 0
  },
  "phase": "31-b",
  "goal": null,
  "agent_output": "",
  "attempts": [
    {
      "number": 1,
      "outcome": "(open)",
      "started_at": "2026-06-01T14:00:00+00:00",
      "ended_at": null,
      "iterations": 0,
      "turns": 0,
      "tokens": 7,
      "cost_usd": 0.0001,
      "error": "",
      "grader_results": []
    }
  ],
  "grader_results": [],
  "related_runs": [],
  "decisions": []
}"#;
        let detail = collect_run_detail(&store, "run-2", &BTreeMap::new())
            .unwrap()
            .unwrap();
        let json = serde_json::to_string_pretty(&run_detail_to_json(&detail)).unwrap();
        assert_eq!(json, expected_open);
    }

    #[test]
    fn short_collapses_newlines_and_clips_by_characters() {
        assert_eq!(short("a\nb\rc", 60), "a b c");
        assert_eq!(short("abcdef", 6), "abcdef");
        assert_eq!(short("abcdefg", 6), "abcde\u{2026}");
        assert_eq!(short("\u{e9}\u{e9}\u{e9}", 2), "\u{e9}\u{2026}");
        assert_eq!(short("xyz", 1), "x\u{2026}");
        assert_eq!(short("xyz", 0), "x\u{2026}");
    }

    #[test]
    fn format_history_ts_uses_em_dash_for_absent() {
        assert_eq!(format_history_ts(None), "\u{2014}");
        assert_eq!(format_history_ts(Some(t0())), "2026-06-01 12:00");
    }
}
