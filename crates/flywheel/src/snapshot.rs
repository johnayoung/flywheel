//! Pure data shape + collector for one dashboard frame.
//!
//! The console and the `--json` snapshot mode render the exact same
//! [`DashboardSnapshot`] so the two surfaces stay field-for-field parallel:
//! one row per active run (RUNNING / VALIDATING / AWAITING_APPROVAL) sourced
//! from [`collect_live_rows`], plus a summary header whose task-state counts
//! mirror `flywheel status` against the same store.
//!
//! [`collect_live_rows`] is the live read model (the Python
//! `flywheel_orchestrator.collect_live_rows`): every field is computed from
//! relational rows only -- `lifecycles` plus the per-attempt aggregate
//! columns the harness maintains at iteration boundaries -- never from a
//! telemetry scan. Output is sorted by `(task_id, run_id)` so two
//! consecutive snapshots over the same store are stable.

use chrono::{DateTime, Utc};
use flywheel_core::invoker_client::GateBinding;
use flywheel_core::lifecycle::{Lifecycle, Status};
use flywheel_core::store::{LifecycleFilter, LifecycleStore, StoreError, TaskStore};
use flywheel_core::task::Grader;
use flywheel_core::telemetry_file::{JsonStyle, python_json_dumps};
use flywheel_orchestrator::rollup::status_rows_for_items;
use flywheel_orchestrator::sources::{WorkItem, WorkReport, WorkSource, WorkSourceError};
use serde_json::{Map, Value, json};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// The lifecycle statuses the live surface shows: in-flight runs plus runs
/// parked on a manual gate (no live worker, but owed operator attention).
pub const ACTIVE_STATUSES: [Status; 3] = [
    Status::Running,
    Status::Validating,
    Status::AwaitingApproval,
];

/// Per-in-flight-run snapshot used by the live surfaces.
///
/// `attempt` / `iteration` form the lifecycle-position breadcrumb;
/// `last_kind` + `last_detail` describe the run's latest recorded position.
/// `tokens_total` / `cost_usd_total` / `turns_total` sum the per-attempt
/// rolled-up counters; `iterations_completed` sums the per-attempt iteration
/// counts. `last_ts` is the latest attempt's `last_activity_at` rollup
/// (falling back to its `started_at` before the first completed iteration);
/// `started_at` is the earliest lifecycle transition timestamp. `worker_id`
/// is `None` when the column is unset -- never a sentinel string.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRunRow {
    pub run_id: String,
    pub task_id: String,
    pub status: Status,
    pub attempt: Option<u32>,
    pub iteration: Option<u64>,
    pub last_kind: String,
    pub last_detail: String,
    pub last_ts: Option<DateTime<Utc>>,
    pub tokens_total: u64,
    pub cost_usd_total: f64,
    pub turns_total: u64,
    pub iterations_completed: u64,
    pub awaiting_instruction: Option<String>,
    /// The parked gate instance (ordinal + attempt) the instruction belongs
    /// to; a console decision on this row is pinned to it.
    pub awaiting_gate: Option<GateBinding>,
    pub started_at: Option<DateTime<Utc>>,
    pub worker_id: Option<String>,
}

/// Look up the pending manual gate's instruction for a parked run.
///
/// Returns `None` unless the lifecycle is at AWAITING_APPROVAL with a
/// recorded ordinal that addresses a manual grader in the task definition
/// pinned to the run. A pointer that does not resolve (forward-compat,
/// data skew, archived task) is absorbed silently rather than failing the
/// live view; a store read failure propagates.
pub fn resolve_awaiting_instruction<S: TaskStore + ?Sized>(
    store: &S,
    run_id: &str,
    status: Status,
    awaiting_ordinal: Option<u32>,
) -> Result<Option<String>, StoreError> {
    if status != Status::AwaitingApproval {
        return Ok(None);
    }
    let Some(ordinal) = awaiting_ordinal else {
        return Ok(None);
    };
    let Some(task) = store.load_task_for_run(run_id)? else {
        return Ok(None);
    };
    Ok(match task.graders.get(ordinal as usize) {
        Some(Grader::Manual(grader)) => Some(grader.instruction.clone()),
        _ => None,
    })
}

/// Snapshot every in-flight run from relational rows only.
///
/// Reads `lifecycles` for status in [`ACTIVE_STATUSES`] and folds each
/// row's attempts (already populated on the listed lifecycle -- one query
/// per frame, not one per run). Output is sorted by `(task_id, run_id)`.
pub fn collect_live_rows<S: LifecycleStore + TaskStore + ?Sized>(
    store: &S,
) -> Result<Vec<LiveRunRow>, StoreError> {
    let filter = LifecycleFilter::default().with_statuses(ACTIVE_STATUSES);
    let mut lifecycles = store.list_lifecycles(&filter)?;
    lifecycles.sort_by(|a, b| (&a.task_id, &a.run_id).cmp(&(&b.task_id, &b.run_id)));
    lifecycles
        .iter()
        .map(|lc| live_row_from_lifecycle(store, lc))
        .collect()
}

fn live_row_from_lifecycle<S: TaskStore + ?Sized>(
    store: &S,
    lc: &Lifecycle,
) -> Result<LiveRunRow, StoreError> {
    let mut tokens = 0u64;
    let mut cost = 0.0f64;
    let mut turns = 0u64;
    let mut iters_completed = 0u64;
    for attempt in &lc.attempts {
        tokens += attempt.total_tokens();
        cost += attempt.total_cost_usd;
        turns += attempt.turns;
        iters_completed += attempt.iterations_completed;
    }
    let mut attempt_number = None;
    let mut iteration = None;
    let mut last_kind = "(none)".to_owned();
    let mut last_detail = "(no activity yet)".to_owned();
    let mut last_ts = None;
    if let Some(latest) = lc.attempts.last() {
        attempt_number = Some(latest.number);
        if latest.iterations_completed > 0 {
            iteration = Some(latest.iterations_completed);
            last_kind = "ITERATION".to_owned();
            last_detail = format!("iteration {} completed", latest.iterations_completed);
            last_ts = latest.last_activity_at.or(Some(latest.started_at));
        } else {
            last_kind = "ATTEMPT".to_owned();
            last_detail = format!("attempt {} started", latest.number);
            last_ts = Some(latest.started_at);
        }
    }
    let started_at = lc.timestamps.values().min().copied();
    let worker_id = (!lc.worker_id.is_empty()).then(|| lc.worker_id.clone());
    let awaiting_instruction =
        resolve_awaiting_instruction(store, &lc.run_id, lc.status, lc.awaiting_manual_ordinal)?;
    let awaiting_gate = GateBinding::of_parked(lc);
    Ok(LiveRunRow {
        run_id: lc.run_id.clone(),
        task_id: lc.task_id.clone(),
        status: lc.status,
        attempt: attempt_number,
        iteration,
        last_kind,
        last_detail,
        last_ts,
        tokens_total: tokens,
        cost_usd_total: cost,
        turns_total: turns,
        iterations_completed: iters_completed,
        awaiting_instruction,
        awaiting_gate,
        started_at,
        worker_id,
    })
}

/// One in-flight run's render-ready fields.
///
/// Mirrors [`LiveRunRow`] minus the live timestamps (collapsed against the
/// snapshot's `now`) so the dashboard and `--json` surfaces consume the same
/// flat shape. `age_seconds` is how long the run has existed (since its
/// earliest lifecycle transition) and grows monotonically; `idle_seconds`
/// is how long since the run's last recorded activity and is the staleness
/// signal.
#[derive(Debug, Clone, PartialEq)]
pub struct RowSnapshot {
    pub run_id: String,
    pub task_id: String,
    pub status: String,
    pub attempt: Option<u32>,
    pub iteration: Option<u64>,
    pub age_seconds: Option<i64>,
    pub idle_seconds: Option<i64>,
    pub tokens: u64,
    pub cost_usd: f64,
    pub turns: u64,
    pub iterations_completed: u64,
    pub last_kind: String,
    pub last_detail: String,
    pub awaiting_instruction: Option<String>,
    pub awaiting_gate: Option<GateBinding>,
}

/// Summary header fields aggregated across the snapshot.
///
/// `task_counts` is keyed by the `TaskState` value (`fresh` /
/// `in_progress` / `retryable` / `interrupted` / `awaiting_approval` /
/// `done`) in first-seen order and matches what `flywheel status` would
/// emit against the same store and work source. A missing key means zero.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryData {
    pub active_workers: usize,
    pub task_counts: Vec<(String, u64)>,
    pub tokens_total: u64,
    pub cost_usd_total: f64,
    pub runtime_seconds: i64,
}

impl SummaryData {
    /// An all-zero summary (the frame rendered before the first poll).
    pub fn empty() -> Self {
        Self {
            active_workers: 0,
            task_counts: Vec::new(),
            tokens_total: 0,
            cost_usd_total: 0.0,
            runtime_seconds: 0,
        }
    }

    /// The count for one task-state key; zero when absent.
    pub fn count(&self, key: &str) -> u64 {
        self.task_counts
            .iter()
            .find(|(k, _)| k == key)
            .map_or(0, |(_, n)| *n)
    }
}

/// One full dashboard frame: header summary plus active-run rows, sorted by
/// `(task_id, run_id)`.
#[derive(Debug, Clone, PartialEq)]
pub struct DashboardSnapshot {
    pub summary: SummaryData,
    pub rows: Vec<RowSnapshot>,
}

impl DashboardSnapshot {
    /// The frame with no rows and an all-zero summary.
    pub fn empty() -> Self {
        Self {
            summary: SummaryData::empty(),
            rows: Vec::new(),
        }
    }
}

/// Whole seconds from `since` to `now`, clamped at zero (clock skew between
/// the store and the host reads as 0s rather than a misleading negative).
fn elapsed_seconds(now: DateTime<Utc>, since: DateTime<Utc>) -> i64 {
    (now - since).num_seconds().max(0)
}

fn row_snapshot(row: &LiveRunRow, now: DateTime<Utc>) -> RowSnapshot {
    RowSnapshot {
        run_id: row.run_id.clone(),
        task_id: row.task_id.clone(),
        status: row.status.as_str().to_owned(),
        attempt: row.attempt,
        iteration: row.iteration,
        age_seconds: row.started_at.map(|t| elapsed_seconds(now, t)),
        idle_seconds: row.last_ts.map(|t| elapsed_seconds(now, t)),
        tokens: row.tokens_total,
        cost_usd: row.cost_usd_total,
        turns: row.turns_total,
        iterations_completed: row.iterations_completed,
        last_kind: row.last_kind.clone(),
        last_detail: row.last_detail.clone(),
        awaiting_instruction: row.awaiting_instruction.clone(),
        awaiting_gate: row.awaiting_gate,
    }
}

/// Snapshot the store for one dashboard frame.
///
/// `work_source` is consulted only for the summary header's task-state
/// counts (matching `flywheel status`); a missing or failing source
/// degrades to an empty counts map so the live-rows half still renders. A
/// store read failure propagates. `started_at` and `now` are threaded
/// explicitly so callers (the polling loop, tests) own time.
pub fn build_snapshot<S: LifecycleStore + TaskStore + ?Sized>(
    store: &S,
    work_source: Option<&dyn WorkSource>,
    now: DateTime<Utc>,
    started_at: DateTime<Utc>,
) -> Result<DashboardSnapshot, StoreError> {
    let live = collect_live_rows(store)?;
    let rows: Vec<RowSnapshot> = live.iter().map(|row| row_snapshot(row, now)).collect();
    let mut counts: Vec<(String, u64)> = Vec::new();
    if let Some(source) = work_source
        && let Ok(items) = source.list_work()
    {
        for status_row in status_rows_for_items(&items, store)? {
            let key = status_row.state.as_str();
            match counts.iter_mut().find(|(k, _)| k == key) {
                Some((_, n)) => *n += 1,
                None => counts.push((key.to_owned(), 1)),
            }
        }
    }
    let summary = SummaryData {
        active_workers: live.len(),
        task_counts: counts,
        tokens_total: live.iter().map(|r| r.tokens_total).sum(),
        // Fold from +0.0: `f64`'s `Sum` starts at -0.0, so an empty sum
        // would render as `-0.0`.
        cost_usd_total: live.iter().fold(0.0, |total, r| total + r.cost_usd_total),
        runtime_seconds: elapsed_seconds(now, started_at),
    };
    Ok(DashboardSnapshot { summary, rows })
}

/// Render a snapshot as plain JSON data with stable field names: scripts
/// piping `flywheel --json` rely on the same keys the console uses.
pub fn snapshot_to_json(snapshot: &DashboardSnapshot) -> Value {
    let mut counts = Map::new();
    for (key, n) in &snapshot.summary.task_counts {
        counts.insert(key.clone(), Value::from(*n));
    }
    let rows: Vec<Value> = snapshot
        .rows
        .iter()
        .map(|r| {
            json!({
                "run_id": r.run_id,
                "task_id": r.task_id,
                "status": r.status,
                "attempt": r.attempt,
                "iteration": r.iteration,
                "age_seconds": r.age_seconds,
                "idle_seconds": r.idle_seconds,
                "tokens": r.tokens,
                "cost_usd": r.cost_usd,
                "turns": r.turns,
                "iterations_completed": r.iterations_completed,
                "last_kind": r.last_kind,
                "last_detail": r.last_detail,
                "awaiting_instruction": r.awaiting_instruction,
            })
        })
        .collect();
    // Python's `sum()` over no live rows is the integer 0; over any rows it
    // is a float. Mirror that so `--json` prints `0`, never `0.0` / `-0.0`.
    let cost_usd_total = if snapshot.rows.is_empty() && snapshot.summary.cost_usd_total == 0.0 {
        Value::from(0)
    } else {
        Value::from(snapshot.summary.cost_usd_total)
    };
    json!({
        "summary": {
            "active_workers": snapshot.summary.active_workers,
            "task_counts": Value::Object(counts),
            "tokens_total": snapshot.summary.tokens_total,
            "cost_usd_total": cost_usd_total,
            "runtime_seconds": snapshot.summary.runtime_seconds,
        },
        "rows": rows,
    })
}

/// Serialize `value` exactly as Python's `json.dumps(value, indent=2)`
/// would: two-space indentation, `", "`-free item separators (one member
/// per line), `": "` key separators, empty containers as `{}` / `[]`,
/// scalars and keys in Python's own escaping and float `repr`.
pub fn python_json_dumps_indented(value: &Value) -> String {
    let mut out = String::new();
    write_indented(value, 0, &mut out);
    out
}

fn write_indented(value: &Value, depth: usize, out: &mut String) {
    let style = JsonStyle::PYTHON_DEFAULT;
    match value {
        Value::Object(map) if !map.is_empty() => {
            out.push_str("{\n");
            let last = map.len() - 1;
            for (i, (key, item)) in map.iter().enumerate() {
                push_indent(depth + 1, out);
                out.push_str(&python_json_dumps(&Value::String(key.clone()), &style));
                out.push_str(": ");
                write_indented(item, depth + 1, out);
                if i != last {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(depth, out);
            out.push('}');
        }
        Value::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            let last = items.len() - 1;
            for (i, item) in items.iter().enumerate() {
                push_indent(depth + 1, out);
                write_indented(item, depth + 1, out);
                if i != last {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(depth, out);
            out.push(']');
        }
        other => out.push_str(&python_json_dumps(other, &style)),
    }
}

fn push_indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// The `--json` payload: [`snapshot_to_json`] rendered as Python's
/// `json.dumps(..., indent=2)` would print it.
pub fn render_snapshot_json(snapshot: &DashboardSnapshot) -> String {
    python_json_dumps_indented(&snapshot_to_json(snapshot))
}

// --- Cached work-source listing ---------------------------------------------

/// A content fingerprint over a tasks directory's `active/` tree.
///
/// Hashes every entry's path, modification time, and size two levels deep
/// (`active/<phase>/<task>.json`); any add / remove / edit changes the
/// value. Reading directory metadata is far cheaper than re-parsing and
/// re-validating every task file, which is what a raw `list_work` costs on
/// every poll tick. A missing tree hashes to a stable sentinel.
pub fn directory_fingerprint(tasks_dir: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    let active = tasks_dir.join("active");
    hash_entry(&active, &mut hasher);
    if let Ok(phases) = std::fs::read_dir(&active) {
        let mut phase_paths: Vec<_> = phases.flatten().map(|e| e.path()).collect();
        phase_paths.sort();
        for phase in phase_paths {
            hash_entry(&phase, &mut hasher);
            if let Ok(files) = std::fs::read_dir(&phase) {
                let mut file_paths: Vec<_> = files.flatten().map(|e| e.path()).collect();
                file_paths.sort();
                for file in file_paths {
                    hash_entry(&file, &mut hasher);
                }
            }
        }
    }
    hasher.finish()
}

fn hash_entry(path: &Path, hasher: &mut DefaultHasher) {
    path.hash(hasher);
    match std::fs::metadata(path) {
        Ok(meta) => {
            meta.len().hash(hasher);
            if let Ok(modified) = meta.modified()
                && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                since.as_nanos().hash(hasher);
            }
        }
        Err(_) => 0u8.hash(hasher),
    }
}

/// A work source whose listing is re-read only when a caller-supplied
/// fingerprint changes.
///
/// The dashboard polls once a second; listing a directory source parses
/// and validates every task file each time. Wrapping the source here keeps
/// the per-tick cost at one metadata walk while an unchanged tree is served
/// from the last listing. A listing error is never cached: the next call
/// retries. `report` passes straight through.
pub struct CachedWorkSource {
    inner: Box<dyn WorkSource>,
    fingerprint: Box<dyn Fn() -> u64 + Send + Sync>,
    cache: Mutex<Option<(u64, Vec<WorkItem>)>>,
    last_cached: AtomicBool,
}

impl CachedWorkSource {
    pub fn new(
        inner: Box<dyn WorkSource>,
        fingerprint: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            fingerprint: Box::new(fingerprint),
            cache: Mutex::new(None),
            last_cached: AtomicBool::new(false),
        }
    }

    /// A cached directory source keyed on [`directory_fingerprint`].
    pub fn for_directory(
        inner: Box<dyn WorkSource>,
        tasks_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        let tasks_dir = tasks_dir.into();
        Self::new(inner, move || directory_fingerprint(&tasks_dir))
    }

    /// Whether the last `list_work` call was served from the cache.
    pub fn last_was_cached(&self) -> bool {
        self.last_cached.load(Ordering::SeqCst)
    }

    fn set_last_cached(&self, cached: bool) {
        self.last_cached.store(cached, Ordering::SeqCst);
    }
}

impl WorkSource for CachedWorkSource {
    fn source_kind(&self) -> &str {
        self.inner.source_kind()
    }

    fn source_name(&self) -> String {
        self.inner.source_name()
    }

    fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError> {
        let current = (self.fingerprint)();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((seen, items)) = cache.as_ref()
            && *seen == current
        {
            self.set_last_cached(true);
            return Ok(items.clone());
        }
        let items = self.inner.list_work()?;
        *cache = Some((current, items.clone()));
        self.set_last_cached(false);
        Ok(items)
    }

    fn report(&self, report: &WorkReport) -> Result<(), WorkSourceError> {
        self.inner.report(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use flywheel_core::lifecycle::Attempt;
    use flywheel_core::store::AttemptStore;
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::{ManualGrader, Task};
    use flywheel_orchestrator::sources::DirectoryWorkSource;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    fn at(minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 10, 12, minute, second)
            .unwrap()
    }

    fn seed_running(store: &InMemoryStore, task_id: &str, now: DateTime<Utc>) -> Lifecycle {
        let mut lc = Lifecycle::new(task_id);
        lc.run_id = format!("run-{task_id}-running");
        lc.transition_to(Status::Ready, "", Some(now)).unwrap();
        lc.transition_to(Status::Running, "", Some(now)).unwrap();
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    fn seed_done(store: &InMemoryStore, task_id: &str, now: DateTime<Utc>) -> Lifecycle {
        let mut lc = Lifecycle::new(task_id);
        lc.run_id = format!("run-{task_id}-done");
        for status in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::Done,
        ] {
            lc.transition_to(status, "", Some(now)).unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    fn write_task(path: &Path, task_id: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!(
                "{{\"id\": \"{task_id}\", \"goal\": \"Goal for {task_id}.\", \
                 \"graders\": [{{\"type\": \"command\", \"run\": \"true\"}}]}}"
            ),
        )
        .unwrap();
    }

    #[test]
    fn empty_store_returns_empty_rows() {
        let store = InMemoryStore::new();
        let now = at(0, 0);
        let snap = build_snapshot(&store, None, now, now).unwrap();
        assert!(snap.rows.is_empty());
        assert_eq!(snap.summary.active_workers, 0);
        assert_eq!(snap.summary.tokens_total, 0);
        assert_eq!(snap.summary.cost_usd_total, 0.0);
        assert!(snap.summary.cost_usd_total.is_sign_positive());
        assert!(snap.summary.task_counts.is_empty());
        let rendered = render_snapshot_json(&snap);
        assert!(rendered.contains("\"cost_usd_total\": 0,"), "{rendered}");
        assert!(!rendered.contains("-0.0"), "{rendered}");
    }

    #[test]
    fn running_rows_carry_attempt_rollups() {
        let store = InMemoryStore::new();
        let now = at(0, 0);
        let running = seed_running(&store, "alpha", now);
        let mut attempt = Attempt::new(1, now, running.run_id.clone());
        attempt.input_tokens = 100;
        attempt.iterations_completed = 1;
        attempt.turns = 3;
        attempt.total_cost_usd = 0.25;
        attempt.last_activity_at = Some(now);
        store.save_attempt(&running.run_id, &attempt, None).unwrap();
        let snap = build_snapshot(&store, None, now, now).unwrap();
        assert_eq!(snap.rows.len(), 1);
        let row = &snap.rows[0];
        assert_eq!(row.task_id, "alpha");
        assert_eq!(row.status, "running");
        assert_eq!(row.tokens, 100);
        assert_eq!(row.cost_usd, 0.25);
        assert_eq!(row.turns, 3);
        assert_eq!(row.iterations_completed, 1);
        assert_eq!(row.attempt, Some(1));
        assert_eq!(row.iteration, Some(1));
        assert_eq!(row.last_kind, "ITERATION");
        assert_eq!(row.last_detail, "iteration 1 completed");
        assert_eq!(snap.summary.active_workers, 1);
        assert_eq!(snap.summary.tokens_total, 100);
        assert_eq!(snap.summary.cost_usd_total, 0.25);
    }

    #[test]
    fn run_without_attempts_reads_as_no_activity() {
        let store = InMemoryStore::new();
        let now = at(0, 0);
        seed_running(&store, "quiet", now);
        let rows = collect_live_rows(&store).unwrap();
        assert_eq!(rows[0].attempt, None);
        assert_eq!(rows[0].iteration, None);
        assert_eq!(rows[0].last_kind, "(none)");
        assert_eq!(rows[0].last_detail, "(no activity yet)");
        assert_eq!(rows[0].last_ts, None);
        assert_eq!(rows[0].started_at, Some(now));
        assert_eq!(rows[0].worker_id, None);
    }

    #[test]
    fn attempt_without_iterations_reads_as_attempt_started() {
        let store = InMemoryStore::new();
        let now = at(0, 0);
        let lc = seed_running(&store, "fresh", now);
        store
            .save_attempt(&lc.run_id, &Attempt::new(2, now, lc.run_id.clone()), None)
            .unwrap();
        let rows = collect_live_rows(&store).unwrap();
        assert_eq!(rows[0].attempt, Some(2));
        assert_eq!(rows[0].iteration, None);
        assert_eq!(rows[0].last_kind, "ATTEMPT");
        assert_eq!(rows[0].last_detail, "attempt 2 started");
        assert_eq!(rows[0].last_ts, Some(now));
    }

    #[test]
    fn rows_sort_by_task_id_then_run_id_regardless_of_write_order() {
        let store = InMemoryStore::new();
        let now = at(0, 0);
        for (task, run) in [
            ("zeta", "run-2"),
            ("alpha", "run-9"),
            ("alpha", "run-1"),
            ("mid", "run-5"),
        ] {
            let mut lc = Lifecycle::new(task);
            lc.run_id = run.to_owned();
            lc.transition_to(Status::Ready, "", Some(now)).unwrap();
            lc.transition_to(Status::Running, "", Some(now)).unwrap();
            store.create_lifecycle(&lc).unwrap();
        }
        seed_done(&store, "alpha", now);
        let rows = collect_live_rows(&store).unwrap();
        let order: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.task_id.as_str(), r.run_id.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("alpha", "run-1"),
                ("alpha", "run-9"),
                ("mid", "run-5"),
                ("zeta", "run-2")
            ]
        );
    }

    #[test]
    fn task_counts_match_status_states() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        write_task(&tasks.join("active/01/fresh.json"), "fresh-task");
        write_task(&tasks.join("active/01/done.json"), "done-task");
        write_task(&tasks.join("active/01/running.json"), "running-task");
        let store = InMemoryStore::new();
        let now = at(0, 0);
        seed_done(&store, "done-task", now);
        seed_running(&store, "running-task", now);
        let source = DirectoryWorkSource::new(&tasks);
        let snap = build_snapshot(&store, Some(&source), now, now).unwrap();
        assert_eq!(snap.summary.count("fresh"), 1);
        assert_eq!(snap.summary.count("done"), 1);
        assert_eq!(snap.summary.count("in_progress"), 1);
        assert_eq!(snap.summary.count("retryable"), 0);
    }

    #[test]
    fn failing_work_source_degrades_to_empty_counts() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemoryStore::new();
        let now = at(0, 0);
        seed_running(&store, "alpha", now);
        // A tasks root with an unreadable entry: active/ is a file.
        let tasks = dir.path().join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(tasks.join("active"), "not a directory").unwrap();
        let source = DirectoryWorkSource::new(&tasks);
        let snap = build_snapshot(&store, Some(&source), now, now).unwrap();
        assert_eq!(snap.rows.len(), 1);
        assert!(snap.summary.task_counts.is_empty());
    }

    #[test]
    fn age_clamps_clock_skew_to_zero() {
        let store = InMemoryStore::new();
        let now = at(0, 0);
        let future = at(5, 0);
        let mut lc = Lifecycle::new("skewed");
        lc.run_id = "run-skewed-running".to_owned();
        lc.transition_to(Status::Ready, "", Some(future)).unwrap();
        lc.transition_to(Status::Running, "", Some(future)).unwrap();
        store.create_lifecycle(&lc).unwrap();
        store
            .save_attempt(
                &lc.run_id,
                &Attempt::new(1, future, lc.run_id.clone()),
                None,
            )
            .unwrap();
        let snap = build_snapshot(&store, None, now, now).unwrap();
        assert_eq!(snap.rows[0].age_seconds, Some(0));
        assert_eq!(snap.rows[0].idle_seconds, Some(0));
    }

    #[test]
    fn age_is_run_age_not_last_event_age() {
        let store = InMemoryStore::new();
        let start = at(0, 0);
        let activity = at(1, 40);
        let now = at(2, 0);
        let mut lc = Lifecycle::new("aging");
        lc.run_id = "run-aging".to_owned();
        lc.transition_to(Status::Ready, "", Some(start)).unwrap();
        lc.transition_to(Status::Running, "", Some(start)).unwrap();
        store.create_lifecycle(&lc).unwrap();
        let mut attempt = Attempt::new(1, start, lc.run_id.clone());
        attempt.iterations_completed = 1;
        attempt.last_activity_at = Some(activity);
        store.save_attempt(&lc.run_id, &attempt, None).unwrap();
        let snap = build_snapshot(&store, None, now, now).unwrap();
        assert_eq!(snap.rows[0].age_seconds, Some(120));
        assert_eq!(snap.rows[0].idle_seconds, Some(20));
    }

    #[test]
    fn awaiting_approval_resolves_manual_gate_instruction() {
        let store = InMemoryStore::new();
        let now = at(0, 0);
        let task = Task::new(
            "Ship the migration.",
            vec![Grader::Manual(
                ManualGrader::new("Confirm the migration before merging.").unwrap(),
            )],
        );
        let digest = store.save_task(&task, now).unwrap();
        let mut lc = Lifecycle::new(task.id.clone());
        lc.run_id = "run-gated".to_owned();
        lc.task_content_hash = digest;
        for status in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::AwaitingApproval,
        ] {
            lc.transition_to(status, "", Some(now)).unwrap();
        }
        lc.awaiting_manual_ordinal = Some(0);
        store.create_lifecycle(&lc).unwrap();
        let rows = collect_live_rows(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::AwaitingApproval);
        assert_eq!(
            rows[0].awaiting_instruction.as_deref(),
            Some("Confirm the migration before merging.")
        );
        // A stale ordinal degrades to None rather than failing the frame.
        assert_eq!(
            resolve_awaiting_instruction(&store, "run-gated", Status::AwaitingApproval, Some(7))
                .unwrap(),
            None
        );
        assert_eq!(
            resolve_awaiting_instruction(&store, "run-gated", Status::Running, Some(0)).unwrap(),
            None
        );
        assert_eq!(
            resolve_awaiting_instruction(&store, "run-gated", Status::AwaitingApproval, None)
                .unwrap(),
            None
        );
    }

    #[test]
    fn json_emits_full_schema_in_python_indent_format() {
        let store = InMemoryStore::new();
        let now = at(0, 0);
        seed_running(&store, "beta", now);
        let snap = build_snapshot(&store, None, now, now).unwrap();
        let encoded = snapshot_to_json(&snap);
        let obj = encoded.as_object().unwrap();
        assert_eq!(obj.keys().collect::<Vec<_>>(), vec!["summary", "rows"]);
        let summary = obj["summary"].as_object().unwrap();
        assert_eq!(
            summary.keys().collect::<Vec<_>>(),
            vec![
                "active_workers",
                "task_counts",
                "tokens_total",
                "cost_usd_total",
                "runtime_seconds"
            ]
        );
        let row = encoded["rows"][0].as_object().unwrap();
        assert_eq!(row["task_id"], "beta");
        assert_eq!(
            row.keys().collect::<Vec<_>>(),
            vec![
                "run_id",
                "task_id",
                "status",
                "attempt",
                "iteration",
                "age_seconds",
                "idle_seconds",
                "tokens",
                "cost_usd",
                "turns",
                "iterations_completed",
                "last_kind",
                "last_detail",
                "awaiting_instruction"
            ]
        );
        let rendered = render_snapshot_json(&snap);
        let expected = "{\n  \"summary\": {\n    \"active_workers\": 1,\n    \"task_counts\": {},\n    \
                        \"tokens_total\": 0,\n    \"cost_usd_total\": 0.0,\n    \"runtime_seconds\": 0\n  },\n  \
                        \"rows\": [\n    {\n      \"run_id\": \"run-beta-running\",\n      \"task_id\": \"beta\",\n      \
                        \"status\": \"running\",\n      \"attempt\": null,\n      \"iteration\": null,\n      \
                        \"age_seconds\": 0,\n      \"idle_seconds\": null,\n      \"tokens\": 0,\n      \
                        \"cost_usd\": 0.0,\n      \"turns\": 0,\n      \"iterations_completed\": 0,\n      \
                        \"last_kind\": \"(none)\",\n      \"last_detail\": \"(no activity yet)\",\n      \
                        \"awaiting_instruction\": null\n    }\n  ]\n}";
        assert_eq!(rendered, expected);
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn empty_snapshot_json_uses_empty_containers() {
        let rendered = render_snapshot_json(&DashboardSnapshot::empty());
        assert_eq!(
            rendered,
            "{\n  \"summary\": {\n    \"active_workers\": 0,\n    \"task_counts\": {},\n    \
             \"tokens_total\": 0,\n    \"cost_usd_total\": 0,\n    \"runtime_seconds\": 0\n  },\n  \
             \"rows\": []\n}"
        );
    }

    #[test]
    fn task_counts_keep_first_seen_order_in_json() {
        let snap = DashboardSnapshot {
            summary: SummaryData {
                active_workers: 0,
                task_counts: vec![("done".to_owned(), 2), ("fresh".to_owned(), 1)],
                tokens_total: 0,
                cost_usd_total: 1.5,
                runtime_seconds: 3,
            },
            rows: Vec::new(),
        };
        let rendered = render_snapshot_json(&snap);
        assert!(
            rendered.contains("\"task_counts\": {\n      \"done\": 2,\n      \"fresh\": 1\n    }")
        );
        assert!(rendered.contains("\"cost_usd_total\": 1.5"));
    }

    #[test]
    fn runtime_seconds_measures_from_started_at() {
        let store = InMemoryStore::new();
        let start = at(0, 0);
        let now = at(5, 30);
        let snap = build_snapshot(&store, None, now, start).unwrap();
        assert_eq!(snap.summary.runtime_seconds, 5 * 60 + 30);
    }

    /// Byte-identical parity against the Python `build_snapshot` over a
    /// store the Python seeded: set `FLYWHEEL_PARITY_DB` (sqlite path),
    /// `FLYWHEEL_PARITY_NOW` / `FLYWHEEL_PARITY_STARTED` (RFC 3339),
    /// optionally `FLYWHEEL_PARITY_TASKS` (tasks dir), and
    /// `FLYWHEEL_PARITY_EXPECTED` (the Python `--json` text). Skipped when
    /// the variables are unset so the suite needs no Python.
    #[test]
    fn python_parity_over_seeded_sqlite_store() {
        let Ok(db) = std::env::var("FLYWHEEL_PARITY_DB") else {
            return;
        };
        let expected =
            std::fs::read_to_string(std::env::var("FLYWHEEL_PARITY_EXPECTED").unwrap()).unwrap();
        let now: DateTime<Utc> = std::env::var("FLYWHEEL_PARITY_NOW")
            .unwrap()
            .parse()
            .unwrap();
        let started: DateTime<Utc> = std::env::var("FLYWHEEL_PARITY_STARTED")
            .unwrap()
            .parse()
            .unwrap();
        let store = flywheel_core::store_sqlite::SqliteStore::open(&db).unwrap();
        let source = std::env::var("FLYWHEEL_PARITY_TASKS")
            .ok()
            .map(DirectoryWorkSource::new);
        let snap = build_snapshot(
            &store,
            source.as_ref().map(|s| s as &dyn WorkSource),
            now,
            started,
        )
        .unwrap();
        assert_eq!(render_snapshot_json(&snap), expected.trim_end());
    }

    #[test]
    fn cached_work_source_relists_only_when_fingerprint_changes() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        write_task(&tasks.join("active/01/a.json"), "a");
        let stamp = Arc::new(AtomicU64::new(1));
        let probe = Arc::clone(&stamp);
        let cached = CachedWorkSource::new(Box::new(DirectoryWorkSource::new(&tasks)), move || {
            probe.load(Ordering::SeqCst)
        });
        assert_eq!(cached.list_work().unwrap().len(), 1);
        assert!(!cached.last_was_cached());
        write_task(&tasks.join("active/01/b.json"), "b");
        // Same fingerprint: served from the cache, the new file unseen.
        assert_eq!(cached.list_work().unwrap().len(), 1);
        assert!(cached.last_was_cached());
        stamp.store(2, Ordering::SeqCst);
        assert_eq!(cached.list_work().unwrap().len(), 2);
        assert!(!cached.last_was_cached());
        assert_eq!(cached.source_kind(), "directory");
    }

    #[test]
    fn directory_fingerprint_tracks_adds_and_edits() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        let missing = directory_fingerprint(&tasks);
        write_task(&tasks.join("active/01/a.json"), "a");
        let one = directory_fingerprint(&tasks);
        assert_ne!(missing, one);
        assert_eq!(one, directory_fingerprint(&tasks));
        write_task(&tasks.join("active/01/b.json"), "b");
        let two = directory_fingerprint(&tasks);
        assert_ne!(one, two);
        std::fs::write(
            tasks.join("active/01/b.json"),
            "{\"id\": \"b\", \"goal\": \"longer goal text\"}",
        )
        .unwrap();
        assert_ne!(two, directory_fingerprint(&tasks));
        let cached =
            CachedWorkSource::for_directory(Box::new(DirectoryWorkSource::new(&tasks)), &tasks);
        assert_eq!(
            cached.source_name(),
            DirectoryWorkSource::new(&tasks).source_name()
        );
    }
}
