//! The `live` verb: one line per in-flight run, computed from relational
//! rows only (lifecycles + attempts; spec 00025 FR-6, no telemetry scan).

use std::io::Write;

use chrono::{DateTime, Utc};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{AttemptStore, LifecycleFilter, LifecycleStore, StoreError, TaskStore};
use flywheel_core::task::Grader;

use crate::history::short;

use super::WorkflowError;
use super::args::LiveArgs;

/// A run is marked `STALE` once idle longer than this.
pub const LIVE_STALE_AFTER_SECONDS: i64 = 90;
/// The last-detail column is clipped to this many characters.
pub const LIVE_DETAIL_MAX_WIDTH: usize = 120;

/// Per-in-flight-run snapshot used by `live` reporting.
///
/// `attempt` / `iteration` form the lifecycle-position breadcrumb;
/// `last_kind` + `last_detail` describe the run's latest recorded position.
/// The totals sum the per-attempt rolled-up counters the harness writes at
/// iteration boundaries; zero iterations completed means "no totals yet".
/// `last_ts` is the latest attempt's `last_activity_at` (falling back to
/// its `started_at` before the first completed iteration); `started_at` is
/// the earliest lifecycle transition stamp. `awaiting_instruction` carries
/// the pending manual gate's instruction for an AWAITING_APPROVAL run.
/// `worker_id` is the persisted worker id, `None` when unset (never a
/// sentinel).
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
    pub started_at: Option<DateTime<Utc>>,
    pub worker_id: Option<String>,
}

/// The pending manual gate's instruction for a parked run, resolved from
/// the task definition pinned to the run; `None` unless the lifecycle is
/// AWAITING_APPROVAL with an ordinal addressing a manual grader (a pointer
/// that does not resolve is absorbed silently).
fn resolve_awaiting_instruction<S: TaskStore + ?Sized>(
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

/// Snapshot every in-flight run (RUNNING / VALIDATING / AWAITING_APPROVAL)
/// from relational rows only, sorted by task id (ties by run id).
pub fn collect_live_rows<S>(store: &S) -> Result<Vec<LiveRunRow>, StoreError>
where
    S: LifecycleStore + AttemptStore + TaskStore + ?Sized,
{
    let filter = LifecycleFilter::default().with_statuses([
        Status::Running,
        Status::Validating,
        Status::AwaitingApproval,
    ]);
    let mut lifecycles = store.list_lifecycles(&filter)?;
    lifecycles.sort_by(|a, b| (&a.task_id, &a.run_id).cmp(&(&b.task_id, &b.run_id)));
    let mut rows = Vec::with_capacity(lifecycles.len());
    for lc in lifecycles {
        let attempts = store.list_attempts(&lc.run_id)?;
        let mut tokens = 0u64;
        let mut cost = 0.0f64;
        let mut turns = 0u64;
        let mut iters_completed = 0u64;
        for a in &attempts {
            tokens += a.total_tokens();
            cost += a.total_cost_usd;
            turns += a.turns;
            iters_completed += a.iterations_completed;
        }
        let mut iteration = None;
        let mut attempt = None;
        let mut last_kind = "(none)".to_owned();
        let mut last_detail = "(no activity yet)".to_owned();
        let mut last_ts = None;
        if let Some(latest) = attempts.last() {
            attempt = Some(latest.number);
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
        rows.push(LiveRunRow {
            run_id: lc.run_id,
            task_id: lc.task_id,
            status: lc.status,
            attempt,
            iteration,
            last_kind,
            last_detail,
            last_ts,
            tokens_total: tokens,
            cost_usd_total: cost,
            turns_total: turns,
            iterations_completed: iters_completed,
            awaiting_instruction,
            started_at,
            worker_id,
        });
    }
    Ok(rows)
}

fn format_breadcrumb(row: &LiveRunRow) -> String {
    let attempt = row
        .attempt
        .map_or_else(|| "attempt=?".to_owned(), |a| format!("attempt={a}"));
    let iter = row
        .iteration
        .map_or_else(|| "iter=?".to_owned(), |i| format!("iter={i}"));
    format!("{attempt} {iter}")
}

fn format_totals(row: &LiveRunRow) -> String {
    if row.iterations_completed == 0 {
        return "tokens=0 cost=-- turns=0".to_owned();
    }
    format!(
        "tokens={} cost=${:.4} turns={}",
        row.tokens_total, row.cost_usd_total, row.turns_total
    )
}

/// Whole seconds between two instants, clamped at zero.
fn clamped_seconds(then: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    let micros = (now - then).num_microseconds().unwrap_or(i64::MAX);
    // Python truncates toward zero; the clamp makes the sign irrelevant.
    (micros / 1_000_000).max(0)
}

/// Render one `live` line (plus the indented `awaiting_on:` follow-up for a
/// parked run).
pub fn format_live_line(row: &LiveRunRow, now: DateTime<Utc>) -> String {
    let age = row.started_at.map_or_else(
        || "\u{2014}".to_owned(),
        |s| format!("{}s", clamped_seconds(s, now)),
    );
    let (idle, stale) = match row.last_ts {
        None => ("\u{2014}".to_owned(), ""),
        Some(ts) => {
            let idle_s = clamped_seconds(ts, now);
            (
                format!("{idle_s}s"),
                if idle_s > LIVE_STALE_AFTER_SECONDS {
                    "  STALE"
                } else {
                    ""
                },
            )
        }
    };
    let detail = short(&row.last_detail, LIVE_DETAIL_MAX_WIDTH);
    let head = format!(
        "{}  {}  {}  {}  age={age}  idle={idle}  {}  {detail}{stale}",
        row.task_id,
        row.status.as_str(),
        format_breadcrumb(row),
        format_totals(row),
        row.last_kind,
    );
    match &row.awaiting_instruction {
        Some(instruction) => format!("{head}\n    awaiting_on: {instruction}"),
        None => head,
    }
}

fn snapshot<S>(store: &S, out: &mut dyn Write) -> Result<(), WorkflowError>
where
    S: LifecycleStore + AttemptStore + TaskStore + ?Sized,
{
    let rows = collect_live_rows(store)?;
    let now = Utc::now();
    if rows.is_empty() {
        writeln!(out, "(no in-flight runs)")?;
        return Ok(());
    }
    for row in &rows {
        writeln!(out, "{}", format_live_line(row, now))?;
    }
    Ok(())
}

/// Print one snapshot (exit 0), or with `--watch N` clear the screen and
/// refresh every N seconds until the process is interrupted.
pub fn cmd_live<S>(args: &LiveArgs, store: &S, out: &mut dyn Write) -> Result<i32, WorkflowError>
where
    S: LifecycleStore + AttemptStore + TaskStore + ?Sized,
{
    let interval = args.watch.max(0);
    if interval <= 0 {
        snapshot(store, out)?;
        return Ok(0);
    }
    loop {
        // ANSI clear+home; falls back to scroll on dumb terminals.
        out.write_all(b"\x1b[2J\x1b[H")?;
        writeln!(
            out,
            "flywheel live  (refresh {interval}s, Ctrl-C to exit)\n"
        )?;
        snapshot(store, out)?;
        out.flush()?;
        std::thread::sleep(std::time::Duration::from_secs(interval as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use chrono::Duration;
    use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome};
    use flywheel_core::store::{AttemptStore, LifecycleStore, TaskStore};
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::{CommandGrader, ManualGrader, Task};

    #[allow(clippy::too_many_arguments)]
    fn save_attempt(
        store: &InMemoryStore,
        lc: &Lifecycle,
        number: u32,
        started_at: DateTime<Utc>,
        iterations_completed: u64,
        tokens: (u64, u64, u64, u64),
        turns: u64,
        cost: f64,
        last_activity_at: Option<DateTime<Utc>>,
        ended: Option<(DateTime<Utc>, Outcome)>,
    ) {
        let mut a = Attempt::new(number, started_at, &lc.run_id);
        a.iterations_completed = iterations_completed;
        a.input_tokens = tokens.0;
        a.output_tokens = tokens.1;
        a.cache_creation_input_tokens = tokens.2;
        a.cache_read_input_tokens = tokens.3;
        a.turns = turns;
        a.total_cost_usd = cost;
        a.last_activity_at = last_activity_at;
        if let Some((ended_at, outcome)) = ended {
            a.ended_at = Some(ended_at);
            a.outcome = Some(outcome);
        }
        store.save_attempt(&lc.run_id, &a, None).unwrap();
    }

    fn seed_awaiting_with_task(
        store: &InMemoryStore,
        task_id: &str,
        instruction: &str,
    ) -> Lifecycle {
        let mut task = Task::new(
            format!("Goal for {task_id}."),
            vec![
                Grader::Command(CommandGrader::new("true").unwrap()),
                Grader::Manual(ManualGrader::new(instruction).unwrap()),
            ],
        );
        task.id = task_id.to_owned();
        let hash = store.save_task(&task, t0()).unwrap();
        let mut lc = Lifecycle::new(task_id).with_run_id(format!("run-{task_id}-awaiting"));
        lc.task_content_hash = hash;
        for s in [Status::Ready, Status::Running, Status::Validating] {
            lc.transition_to(s, "", Some(t0())).unwrap();
        }
        lc.awaiting_manual_ordinal = Some(1);
        lc.transition_to(Status::AwaitingApproval, "", Some(t0()))
            .unwrap();
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    #[test]
    fn skips_runs_that_are_not_in_flight() {
        let store = InMemoryStore::new();
        seed_done(&store, "done-task");
        seed_failed(&store, "failed-task");
        seed_interrupted(&store, "interrupted-task");
        assert!(collect_live_rows(&store).unwrap().is_empty());
    }

    #[test]
    fn reports_latest_attempt_position_and_totals() {
        let store = InMemoryStore::new();
        let running = seed_running(&store, "task-a");
        let now = t0() + Duration::seconds(10);
        save_attempt(
            &store,
            &running,
            1,
            t0(),
            2,
            (100, 150, 50, 50),
            5,
            0.06,
            Some(now),
            Some((now, Outcome::ValidationFailed)),
        );
        save_attempt(
            &store,
            &running,
            2,
            t0(),
            1,
            (25, 25, 0, 0),
            1,
            0.005,
            Some(now),
            None,
        );
        let rows = collect_live_rows(&store).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.task_id, "task-a");
        assert_eq!(row.status, Status::Running);
        assert_eq!(row.attempt, Some(2));
        assert_eq!(row.iteration, Some(1));
        assert_eq!(row.last_kind, "ITERATION");
        assert_eq!(row.last_detail, "iteration 1 completed");
        assert_eq!(row.last_ts, Some(now));
        assert_eq!(row.iterations_completed, 3);
        assert_eq!(row.tokens_total, 400);
        assert_eq!(row.turns_total, 6);
        assert!((row.cost_usd_total - 0.065).abs() < 1e-9);
        assert_eq!(row.started_at, Some(t0()));
        assert_eq!(row.worker_id, None);
    }

    #[test]
    fn attempt_started_without_iteration_and_no_activity_shapes() {
        let store = InMemoryStore::new();
        let running = seed_running(&store, "task-b");
        let started = t0() + Duration::seconds(5);
        save_attempt(
            &store,
            &running,
            1,
            started,
            0,
            (0, 0, 0, 0),
            0,
            0.0,
            None,
            None,
        );
        seed_running(&store, "task-c");
        let rows = collect_live_rows(&store).unwrap();
        assert_eq!(rows[0].attempt, Some(1));
        assert_eq!(rows[0].iteration, None);
        assert_eq!(rows[0].last_kind, "ATTEMPT");
        assert_eq!(rows[0].last_detail, "attempt 1 started");
        assert_eq!(rows[0].last_ts, Some(started));
        assert_eq!(rows[1].last_kind, "(none)");
        assert_eq!(rows[1].last_ts, None);
        assert_eq!(rows[1].attempt, None);
        let line = format_live_line(&rows[0], t0() + Duration::seconds(10));
        assert!(line.contains("tokens=0 cost=-- turns=0"));
        assert!(line.contains("attempt=1 iter=?"));
        let line = format_live_line(&rows[1], t0());
        assert!(line.contains("attempt=? iter=?"));
    }

    #[test]
    fn worker_id_is_verbatim_or_none_and_rows_sort_by_task_id() {
        let store = InMemoryStore::new();
        let mut lc = walk(
            "task-w",
            "run-task-w-running",
            &[Status::Ready, Status::Running],
            "",
        );
        lc.worker_id = "worker-99".into();
        store.create_lifecycle(&lc).unwrap();
        seed_running(&store, "task-zeta");
        seed_running(&store, "task-alpha");
        let rows = collect_live_rows(&store).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.task_id.as_str()).collect();
        assert_eq!(ids, ["task-alpha", "task-w", "task-zeta"]);
        assert_eq!(rows[1].worker_id.as_deref(), Some("worker-99"));
        assert_eq!(rows[0].worker_id, None);
    }

    #[test]
    fn awaiting_approval_rows_carry_the_instruction_when_the_task_resolves() {
        let store = InMemoryStore::new();
        seed_awaiting_with_task(&store, "parked", "Confirm the migration is safe.");
        let mut orphan = walk(
            "orphan",
            "run-orphan-awaiting",
            &[Status::Ready, Status::Running, Status::Validating],
            "",
        );
        orphan.awaiting_manual_ordinal = Some(1);
        orphan
            .transition_to(Status::AwaitingApproval, "", Some(t0()))
            .unwrap();
        store.create_lifecycle(&orphan).unwrap();
        let rows = collect_live_rows(&store).unwrap();
        assert_eq!(rows[0].task_id, "orphan");
        assert_eq!(rows[0].awaiting_instruction, None);
        assert_eq!(rows[1].status, Status::AwaitingApproval);
        assert_eq!(
            rows[1].awaiting_instruction.as_deref(),
            Some("Confirm the migration is safe.")
        );
    }

    #[test]
    fn live_lines_are_byte_identical_to_python() {
        let now = t0() + Duration::seconds(200);
        let base = LiveRunRow {
            run_id: "r".into(),
            task_id: "task-a".into(),
            status: Status::Running,
            attempt: Some(3),
            iteration: Some(5),
            last_kind: "ITERATION".into(),
            last_detail: "iteration 5 completed".into(),
            last_ts: Some(t0() + Duration::seconds(150)),
            tokens_total: 1234,
            cost_usd_total: 0.0567,
            turns_total: 4,
            iterations_completed: 5,
            awaiting_instruction: None,
            started_at: Some(t0()),
            worker_id: None,
        };
        assert_eq!(
            format_live_line(&base, now),
            "task-a  running  attempt=3 iter=5  tokens=1234 cost=$0.0567 turns=4  age=200s  idle=50s  ITERATION  iteration 5 completed"
        );
        let b = LiveRunRow {
            task_id: "task-b".into(),
            status: Status::Validating,
            attempt: Some(1),
            iteration: None,
            last_kind: "ATTEMPT".into(),
            last_detail: "attempt 1 started".into(),
            last_ts: Some(t0()),
            tokens_total: 0,
            cost_usd_total: 0.0,
            turns_total: 0,
            iterations_completed: 0,
            started_at: Some(t0() + Duration::seconds(300)),
            worker_id: Some("w".into()),
            ..base.clone()
        };
        assert_eq!(
            format_live_line(&b, now),
            "task-b  validating  attempt=1 iter=?  tokens=0 cost=-- turns=0  age=0s  idle=200s  ATTEMPT  attempt 1 started  STALE"
        );
        let c = LiveRunRow {
            task_id: "task-c".into(),
            attempt: None,
            iteration: None,
            last_kind: "(none)".into(),
            last_detail: "(no activity yet)".into(),
            last_ts: None,
            tokens_total: 0,
            cost_usd_total: 0.0,
            turns_total: 0,
            iterations_completed: 0,
            started_at: None,
            ..base.clone()
        };
        assert_eq!(
            format_live_line(&c, now),
            "task-c  running  attempt=? iter=?  tokens=0 cost=-- turns=0  age=\u{2014}  idle=\u{2014}  (none)  (no activity yet)"
        );
        let parked = LiveRunRow {
            task_id: "parked".into(),
            status: Status::AwaitingApproval,
            attempt: Some(1),
            iteration: Some(2),
            last_detail: "x".repeat(130),
            last_ts: Some(t0()),
            tokens_total: 10,
            cost_usd_total: 1.23456,
            turns_total: 1,
            iterations_completed: 2,
            awaiting_instruction: Some("Confirm the migration is safe.".into()),
            ..base
        };
        assert_eq!(
            format_live_line(&parked, now),
            format!(
                "parked  awaiting_approval  attempt=1 iter=2  tokens=10 cost=$1.2346 turns=1  age=200s  idle=200s  ITERATION  {}\u{2026}  STALE\n    awaiting_on: Confirm the migration is safe.",
                "x".repeat(119)
            )
        );
    }

    #[test]
    fn cmd_live_prints_placeholder_or_one_line_per_run() {
        let store = InMemoryStore::new();
        let args = LiveArgs::default();
        let mut out = Vec::new();
        assert_eq!(cmd_live(&args, &store, &mut out).unwrap(), 0);
        assert_eq!(String::from_utf8(out).unwrap(), "(no in-flight runs)\n");

        let a = seed_running(&store, "task-a");
        save_attempt(
            &store,
            &a,
            1,
            t0(),
            1,
            (0, 0, 0, 0),
            0,
            0.0,
            Some(Utc::now()),
            None,
        );
        seed_running(&store, "task-b");
        seed_awaiting_with_task(&store, "parked", "Confirm the migration is safe.");
        let mut out = Vec::new();
        assert_eq!(cmd_live(&args, &store, &mut out).unwrap(), 0);
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("parked  awaiting_approval"));
        assert_eq!(
            lines[1].trim(),
            "awaiting_on: Confirm the migration is safe."
        );
        assert!(
            lines[2].contains("task-a")
                && lines[2].contains("ITERATION")
                && lines[2].contains("iteration 1 completed")
        );
        assert!(lines[3].contains("task-b") && lines[3].contains("(none)"));
    }
}
