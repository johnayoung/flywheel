//! Cross-task orchestrator -- the consumer-side scheduler (port of the
//! Python `flywheel_orchestrator._orchestrate`).
//!
//! `docs/strategy.md` is explicit that *strategy lives in the consumer, not
//! the loop*: the harness owns a single task's lifecycle; deciding **which**
//! task runs next is this layer. It only *reads* authoritative lifecycle
//! state and *decides what to run next*; it never transitions a lifecycle
//! itself and holds no harness privilege.
//!
//! Scheduling: a fresh/retryable task runs only once every prerequisite has
//! a DONE lifecycle (or is DONE in the authoritative store); a
//! blocked-interrupted lifecycle is re-evaluated via `recheck_blocked_lifecycle`
//! and resumed on its own run id; a parked approval gate is resolved in
//! place. Several orchestrators may share one store: a worker acquires a
//! task's lease before running it, a heartbeat renews it while the run is in
//! flight, and a task held by a live lease is skipped. Termination is
//! guaranteed by two per-session guards -- at most one fresh run per task id
//! and at most one resume per run id.
//!
//! Authoritative specs: `docs/orchestration.md`, `docs/strategy.md`,
//! `docs/held-out-gate.md`, `docs/work-sources.md`, `docs/task-lifecycle.md`.

mod drive;
mod heartbeat;
mod prereqs;
mod primitives;
mod redrive;
mod run_task;
mod sweep;
#[cfg(test)]
mod tests;

pub use drive::{ProgressProbeFactory, final_grader_receipts};
pub use heartbeat::ClaimHeartbeat;
pub use prereqs::{
    DEFAULT_MAX_RETRIES, DEFAULT_MAX_TURNS, LatestLifecycleRow, STRANDED_STATUSES,
    latest_lifecycle_row, reachability_held_prerequisites, stranded_run_ids,
};
pub use primitives::{
    AttemptBudgetPrimitives, SandboxLimitPrimitives, attempt_budget_primitives,
    checkpoint_nudge_seconds, harness_config_from_policy, sandbox_agent_primitives,
    sandbox_limit_primitives,
};
pub use redrive::{
    DEFAULT_ESCALATION_BOUND, DEFAULT_LANDING_REDRIVE_BOUND, DEFAULT_NO_PROGRESS_BOUND,
    DEFAULT_PREREQ_REDRIVE_BOUND, EscalationContext, EscalationDriver, EscalationOutcome,
    EscalationRequest, EscalationResult, HumanGateOutcome, HumanGateRequest, HumanGateResult,
    LandingRedriveContext, NoProgressObservation, NoProgressOutcome, NoProgressResult,
    PrereqRedriveOutcome, PrereqRedriveResult, RedriveOutcome, RedriveResult, classify_human_gate,
    is_retry_exhausted, landing_parks, no_progress_streak, probe_landability,
    redrive_exhausted_retries, redrive_human_gates, redrive_missing_prerequisites,
    redrive_no_progress, redrive_parked_landings,
};
pub use run_task::{
    AgentPrimitives, RunMode, RunTask, RunTaskError, RunTaskFuture, RunTaskRequest, SharedRunTask,
};
pub use sweep::{
    assemble_graph_snapshot_items, reconcile_live_runs, recover_claimable_stranded,
    sweep_expired_leases, sync_work_source,
};

use crate::claims::{ClaimStore, ClaimStoreError, TaskClaim};
use crate::held_out_gate::{GateOptions, GateOutcome, HeldOutGraderSource, gate_landing};
use crate::policy::WorkPolicy;
use crate::rollup::{
    TaskState, TaskStatusRow, satisfied_prerequisites_from_store, status_rows_for_items, task_state,
};
use crate::sources::{LogSink, WorkItem, WorkSource};
use crate::strategy::{
    PlainDirectoryStrategy, SandboxHandle, SandboxRequest, SubmitRequest, SubmitStrategy,
    prepare_sandbox_guarded, submit_guarded,
};
use crate::work_graph::{ReadySetQuery, WorkGraph};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use drive::{DriveContext, DriveDisposition, drive_or_relinquish};
use flywheel_core::agents_invoke::InvokeFn;
use flywheel_core::deadline_config::DeadlineClass;
use flywheel_core::harness::{
    RecheckOptions, ResolveApprovalOptions, SharedSink, ShutdownSignal, recheck_blocked_lifecycle,
    resolve_manual_approval,
};
use flywheel_core::invoker_client::Clock;
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{HarnessStore, StoreConflictError, StoreError};
use flywheel_core::telemetry_file::FileTelemetrySink;
use flywheel_core::validation::validate_task;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use sweep::emit;

// --- Constants ---------------------------------------------------------------------

/// Default lease window. Generous so a normal run finishes well within it;
/// the heartbeat renews at roughly a third of it, so a crashed worker's
/// lease lapses within about one window of its death.
///
/// Lease expiry is compared against a wall clock and the `now` each worker
/// injects is its own host clock -- there is no shared or monotonic time
/// source across hosts. `lease_seconds` must comfortably exceed (max
/// cross-host clock skew + the longest a healthy worker can go between
/// renewals); see [`OrchestrateRequest::steal_grace_seconds`] for the
/// loop-side margin that raises the tolerated skew to a full window.
pub const DEFAULT_LEASE_SECONDS: f64 = 300.0;

/// How often the source reconciler (the steering bridge) re-lists the work
/// source while runs are in flight.
pub const DEFAULT_RECONCILE_SECONDS: f64 = 15.0;

/// How often the in-loop expired-lease sweep runs while the loop drives
/// (spec 00069).
pub const DEFAULT_SWEEP_SECONDS: f64 = 15.0;

/// Default pool-wide session-pause ceiling (`[worker]
/// session_pause_ceiling_seconds`, 6h); `0` disables pausing.
pub const DEFAULT_SESSION_PAUSE_CEILING_SECONDS: f64 =
    crate::policy::DEFAULT_SESSION_PAUSE_CEILING_SECONDS;

/// Default `[worker] concurrency`.
pub const DEFAULT_WORKER_CONCURRENCY: i64 = crate::policy::DEFAULT_WORKER_CONCURRENCY;

/// How long the reactive approval sweep suppresses a re-resolve of a parked
/// AWAITING_APPROVAL run whose sweep found NO pending command. The mark
/// EXPIRES (measured against the injected clock) so an approve / reject
/// enqueued mid-session is consumed by the same still-running session once
/// the deadline passes; it re-arms so an idle gate never tight-loops.
pub const APPROVAL_SWEEP_MARK_TTL_SECONDS: f64 = 30.0;

// --- Report ------------------------------------------------------------------------

/// One task execution the orchestrator drove, in launch order.
///
/// `gate` is the held-out landing gate's verdict: `None` when the gate did
/// not run, `NoGate`/`Pass` when the task landed, `Fail` when the gate
/// blocked the land -- so only a gate-blocked land carries `status == Done`
/// with `gate == Some(Fail)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub task_id: String,
    pub run_id: String,
    pub status: Status,
    pub mode: RunMode,
    pub worker_id: String,
    pub gate: Option<GateOutcome>,
    #[serde(default)]
    pub gate_reason: String,
}

/// Outcome of an [`orchestrate`] session: `recovered` lists run ids
/// finalized from a stranded state at entry; `runs` lists every task
/// execution this worker launched, in order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OrchestratorReport {
    pub worker_id: String,
    pub recovered: Vec<String>,
    pub runs: Vec<RunRecord>,
}

/// How the per-pass graph snapshot is recorded.
///
/// `EveryPass` is the spec's contract (exactly one snapshot per scheduling
/// pass, including the terminal and empty-source passes). `SkipUnchanged`
/// is the daemon's opt-in bound: a pass whose cross-section is identical to
/// the store's latest snapshot records nothing, so a long-lived daemon
/// idling over an unchanged queue does not accumulate duplicate snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphSnapshotPolicy {
    #[default]
    EveryPass,
    SkipUnchanged,
}

/// Every failure [`orchestrate`] can surface.
#[derive(Debug, thiserror::Error)]
pub enum OrchestrateError {
    /// The worker was shut down (SIGINT/SIGTERM or the injected signal);
    /// every claim was released and `report` carries the runs so far.
    #[error("orchestrate interrupted after {} run(s)", report.runs.len())]
    Interrupted { report: OrchestratorReport },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Claims(#[from] ClaimStoreError),
    #[error("run task: {0}")]
    RunTask(RunTaskError),
    #[error("orchestrate config: {0}")]
    Config(String),
    #[error("orchestrate io: {0}")]
    Io(#[from] std::io::Error),
}

// --- Request -----------------------------------------------------------------------

/// Everything one [`orchestrate`] session needs.
///
/// Stores are injected (the product daemon opens them through the store
/// factory); `run_task` is the [`RunTask`] seam the daemon binds to the
/// core workflow. `strategy` is the consumer landing seam; `None` runs each
/// task in `sandbox_root/<task-id>` and lands nothing, exactly the plain
/// library default. Every other knob defaults as the Python library call
/// did (`new`).
#[derive(Clone)]
pub struct OrchestrateRequest {
    pub source: Arc<dyn WorkSource>,
    pub control: Arc<dyn HarnessStore>,
    pub claims: Arc<dyn ClaimStore>,
    pub run_task: Arc<dyn RunTask>,
    pub sandbox_root: PathBuf,
    pub policy: Option<WorkPolicy>,
    pub invoke: Option<InvokeFn>,
    pub model: Option<String>,
    pub max_turns: u32,
    pub max_retries: u32,
    pub worker_id: Option<String>,
    pub lease_seconds: f64,
    /// Loop-side margin before stealing a *different* worker's lapsed lease:
    /// a row that lapsed less than this many seconds ago is treated as held.
    /// Raises the tolerated cross-host clock skew from two thirds of a window
    /// to a full window at the cost of that much extra reclaim latency for a
    /// genuinely dead worker. `None` resolves to the heartbeat interval
    /// (`lease_seconds / 3`); `Some(0.0)` disables it (the Python behavior).
    pub steal_grace_seconds: Option<f64>,
    pub log: Option<LogSink>,
    pub now: Option<Clock>,
    pub strategy: Option<Arc<dyn SubmitStrategy>>,
    pub reconcile_seconds: Option<f64>,
    pub sweep_seconds: Option<f64>,
    pub landing_redrive_bound: Option<u32>,
    pub prereq_redrive_bound: u32,
    pub session_pause_ceiling_seconds: f64,
    pub repo_root: Option<PathBuf>,
    pub held_out_source: Option<Arc<dyn HeldOutGraderSource>>,
    pub progress_probe_factory: Option<ProgressProbeFactory>,
    /// Logs root for control-plane telemetry (the per-run JSONL files the
    /// harness writes); `None` records no telemetry unless `sink` is set.
    pub telemetry_root: Option<PathBuf>,
    pub sink: Option<SharedSink>,
    pub shutdown: Option<ShutdownSignal>,
    pub snapshot_policy: GraphSnapshotPolicy,
    /// The environment `[sandbox.env]` resolves against; the process
    /// environment when `None`.
    pub environ: Option<BTreeMap<String, String>>,
}

impl fmt::Debug for OrchestrateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrchestrateRequest")
            .field("sandbox_root", &self.sandbox_root)
            .field("worker_id", &self.worker_id)
            .field("lease_seconds", &self.lease_seconds)
            .field("max_turns", &self.max_turns)
            .field("max_retries", &self.max_retries)
            .field("reconcile_seconds", &self.reconcile_seconds)
            .field("sweep_seconds", &self.sweep_seconds)
            .field("landing_redrive_bound", &self.landing_redrive_bound)
            .field("prereq_redrive_bound", &self.prereq_redrive_bound)
            .field(
                "session_pause_ceiling_seconds",
                &self.session_pause_ceiling_seconds,
            )
            .field("snapshot_policy", &self.snapshot_policy)
            .finish_non_exhaustive()
    }
}

impl OrchestrateRequest {
    /// A request with the Python library defaults for every optional knob.
    pub fn new(
        source: Arc<dyn WorkSource>,
        control: Arc<dyn HarnessStore>,
        claims: Arc<dyn ClaimStore>,
        run_task: Arc<dyn RunTask>,
        sandbox_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            source,
            control,
            claims,
            run_task,
            sandbox_root: sandbox_root.into(),
            policy: None,
            invoke: None,
            model: None,
            max_turns: DEFAULT_MAX_TURNS,
            max_retries: DEFAULT_MAX_RETRIES,
            worker_id: None,
            lease_seconds: DEFAULT_LEASE_SECONDS,
            steal_grace_seconds: None,
            log: None,
            now: None,
            strategy: None,
            reconcile_seconds: None,
            sweep_seconds: None,
            landing_redrive_bound: None,
            prereq_redrive_bound: DEFAULT_PREREQ_REDRIVE_BOUND,
            session_pause_ceiling_seconds: 0.0,
            repo_root: None,
            held_out_source: None,
            progress_probe_factory: None,
            telemetry_root: None,
            sink: None,
            shutdown: None,
            snapshot_policy: GraphSnapshotPolicy::EveryPass,
            environ: None,
        }
    }
}

// --- Claim helpers ------------------------------------------------------------------

/// `acquire_claim` with the loop-side steal grace: a different worker's
/// lease that lapsed less than `steal_grace_seconds` ago is treated as
/// still held (`None`) rather than stolen. The same worker's own row and a
/// row lapsed past the grace go straight to the store's atomic acquire.
pub(crate) fn acquire_with_grace(
    claims: &dyn ClaimStore,
    task_id: &str,
    worker_id: &str,
    now: DateTime<Utc>,
    lease_seconds: f64,
    conflict_keys: &BTreeSet<String>,
    steal_grace_seconds: f64,
) -> Result<Option<TaskClaim>, ClaimStoreError> {
    if steal_grace_seconds > 0.0
        && let Some(existing) = claims.load_claim(task_id)?
        && existing.worker_id != worker_id
        && existing.lease_expires_at <= now
    {
        let grace = ChronoDuration::milliseconds((steal_grace_seconds * 1000.0) as i64);
        if now < existing.lease_expires_at + grace {
            return Ok(None);
        }
    }
    claims.acquire_claim(task_id, worker_id, now, lease_seconds, conflict_keys)
}

/// A lifecycle parked on a structured block (vs a bare SIGINT pause): only
/// these route through reactive recheck/resume and are kept out of fresh
/// selection.
fn is_blocked_interrupted(row: &TaskStatusRow) -> bool {
    row.latest_status == Some(Status::Interrupted) && row.blocked_requires.is_some()
}

fn is_awaiting_approval(row: &TaskStatusRow) -> bool {
    row.latest_status == Some(Status::AwaitingApproval)
}

fn state_values(states: &HashMap<String, TaskState>) -> HashMap<String, &'static str> {
    states
        .iter()
        .map(|(id, state)| (id.clone(), state.as_str()))
        .collect()
}

fn is_optimistic_conflict(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::Conflict(StoreConflictError::OptimisticConcurrency { .. })
    )
}

/// Abort a background tokio task when the session ends on any path.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// --- The loop ----------------------------------------------------------------------

/// Drive every eligible task from the work source to quiescence.
///
/// Returns once this worker can make no further progress: no blocked
/// lifecycle it can claim unblocks, no parked approval it can claim
/// resolves, no parked landing it re-drives lands, and no fresh task is both
/// eligible and claimable. Safe to run as several concurrent workers against
/// one store -- per-task leases keep them from double-running a task.
///
/// See [`OrchestrateRequest`] for every knob; the Python `orchestrate(...)`
/// keyword set maps onto it one-to-one, with the stores and the runner
/// injected instead of opened from `db_path`.
pub async fn orchestrate(
    request: OrchestrateRequest,
) -> Result<OrchestratorReport, OrchestrateError> {
    if request.lease_seconds <= 0.0 || !request.lease_seconds.is_finite() {
        return Err(OrchestrateError::Config(format!(
            "lease_seconds must be a positive number, got {}",
            request.lease_seconds
        )));
    }
    let clock: Clock = request.now.clone().unwrap_or_else(|| Arc::new(Utc::now));
    let wid = request
        .worker_id
        .clone()
        .unwrap_or_else(|| format!("worker-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    let log = request.log.clone();
    let policy = request.policy.as_ref();
    let worker_capabilities: BTreeSet<String> = policy
        .map(|p| p.execution_capabilities.clone())
        .unwrap_or_default();
    let heartbeat_interval = (request.lease_seconds / 3.0).max(0.001);
    let steal_grace = request
        .steal_grace_seconds
        .unwrap_or(heartbeat_interval)
        .max(0.0);
    let environ: BTreeMap<String, String> = request
        .environ
        .clone()
        .unwrap_or_else(|| std::env::vars().collect());
    let agent = sandbox_agent_primitives(policy, &environ);
    let base_config =
        harness_config_from_policy(policy, request.max_retries, agent.grader_env.clone());
    let gate_options = GateOptions {
        env: agent.grader_env.clone(),
        per_grader_timeout: base_config
            .deadlines
            .duration_for_class(DeadlineClass::CommandGrader),
    };

    let has_strategy = request.strategy.is_some();
    let strategy: Arc<dyn SubmitStrategy> = match request.strategy.clone() {
        Some(strategy) => strategy,
        None => Arc::new(PlainDirectoryStrategy::new(request.sandbox_root.clone())),
    };
    let landability_probe = request.strategy.clone();

    std::fs::create_dir_all(&request.sandbox_root)?;
    let sink: Option<SharedSink> = match (&request.sink, &request.telemetry_root) {
        (Some(sink), _) => Some(sink.clone()),
        (None, Some(root)) => Some(Arc::new(FileTelemetrySink::new(root)) as SharedSink),
        (None, None) => None,
    };

    let control = request.control.clone();
    let claims = request.claims.clone();
    let source = request.source.clone();
    let shutdown = request.shutdown.clone();

    let ctx = DriveContext {
        control: control.clone(),
        claims: claims.clone(),
        run_task: request.run_task.clone(),
        strategy: strategy.clone(),
        landability_probe,
        source: source.clone(),
        held_out_source: request.held_out_source.clone(),
        gate_options: gate_options.clone(),
        progress_probe_factory: request.progress_probe_factory.clone(),
        base_config,
        agent,
        invoke: request.invoke.clone(),
        model: request.model.clone(),
        max_turns: request.max_turns,
        max_retries: request.max_retries,
        worker_id: wid.clone(),
        lease_seconds: request.lease_seconds,
        heartbeat_interval,
        session_pause_ceiling_seconds: request.session_pause_ceiling_seconds,
        telemetry_root: request.telemetry_root.clone(),
        sink: sink.clone(),
        shutdown: shutdown.clone(),
        now: clock.clone(),
        log: log.clone(),
    };

    // Entry-time backstop: finalize stranded lifecycles no live worker owns.
    let recovered = recover_claimable_stranded(
        &control,
        claims.as_ref(),
        &wid,
        request.lease_seconds,
        steal_grace,
        &clock,
        sink.clone(),
    )?;

    let _reconciler = request
        .reconcile_seconds
        .filter(|s| *s > 0.0)
        .map(|interval| {
            AbortOnDrop(tokio::spawn(sweep::source_reconcile_loop(
                source.clone(),
                control.clone(),
                interval,
                clock.clone(),
                log.clone(),
            )))
        });
    // The in-loop sweeper claims under a DISTINCT worker id: acquire_claim
    // lets the same worker re-take its own live lease, so a sweeper sharing
    // `wid` would reclaim a task this worker is actively running and wrongly
    // finalize it mid-run.
    let _sweeper = request.sweep_seconds.filter(|s| *s > 0.0).map(|interval| {
        AbortOnDrop(tokio::spawn(sweep::lease_sweep_loop(
            control.clone(),
            claims.clone(),
            format!("{wid}-sweeper"),
            request.lease_seconds,
            steal_grace,
            interval,
            clock.clone(),
            sink.clone(),
            log.clone(),
        )))
    });

    let mut runs: Vec<RunRecord> = Vec::new();
    let mut attempted_fresh: HashSet<String> = HashSet::new();
    let mut attempted_resume: HashSet<String> = HashSet::new();
    let mut attempted_approve: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut never_parked: BTreeSet<String> = BTreeSet::new();
    let mut routed_gates: BTreeSet<(String, String, String)> = BTreeSet::new();
    // Base for the phase-prerequisite reachability hold: the configured
    // submit base, else HEAD; `None` without a repo_root (no git probe).
    let scheduling_true_base: Option<String> = request.repo_root.as_ref().map(|_| {
        policy
            .and_then(|p| p.submit_base.clone())
            .unwrap_or_else(|| "HEAD".to_owned())
    });
    let report = |wid: &str, recovered: &[String], runs: &[RunRecord]| OrchestratorReport {
        worker_id: wid.to_owned(),
        recovered: recovered.to_vec(),
        runs: runs.to_vec(),
    };

    loop {
        if shutdown.as_ref().is_some_and(ShutdownSignal::is_triggered) {
            return Err(OrchestrateError::Interrupted {
                report: report(&wid, &recovered, &runs),
            });
        }
        // Source-listing containment: a failing list_work() degrades this
        // pass and ends the session gracefully, never crashing the driver.
        let items: Vec<WorkItem> = match source.list_work() {
            Ok(items) => items,
            Err(err) => {
                emit(
                    log.as_ref(),
                    format!(
                        "[orchestrate] work-source listing failed (WorkSourceError: {err}); no work this pass, ending the session without touching in-flight state"
                    ),
                );
                break;
            }
        };
        let rows = status_rows_for_items(&items, control.as_ref())?;
        // Build (and structurally validate) the WorkGraph from the same
        // items the rows derive from. A structural defect (duplicate id,
        // self-dependency, cycle) isolates the offending task(s) -- logged
        // here, never dispatched -- while every valid, independent task
        // stays schedulable; a missing prerequisite is a recorded issue.
        let validation = WorkGraph::build(items.iter().cloned());
        for excluded in &validation.excluded {
            emit(
                log.as_ref(),
                format!(
                    "[orchestrate] {}: isolated from this pass's graph ({}); skipping",
                    excluded.task_id, excluded.reason
                ),
            );
        }
        let graph = &validation.graph;
        let satisfied_prereqs = satisfied_prerequisites_from_store(&rows, control.as_ref())?;
        let reachability_held: BTreeSet<String> = reachability_held_prerequisites(
            &rows,
            request.repo_root.as_deref(),
            scheduling_true_base.as_deref(),
        )
        .into_keys()
        .collect();
        let dangling: Vec<_> = validation
            .issues
            .iter()
            .filter(|issue| !satisfied_prereqs.contains(&issue.missing_id))
            .cloned()
            .collect();
        redrive_missing_prerequisites(
            claims.as_ref(),
            &dangling,
            request.prereq_redrive_bound,
            &clock,
            log.as_ref(),
        )?;
        let gate_requests: Vec<HumanGateRequest> = rows
            .iter()
            .filter(|r| {
                matches!(
                    r.latest_status,
                    Some(Status::AwaitingApproval) | Some(Status::Failed)
                )
            })
            .filter_map(|r| {
                r.latest_run_id.as_ref().map(|run_id| HumanGateRequest {
                    task_id: r.task_id.clone(),
                    run_id: run_id.clone(),
                })
            })
            .collect();
        redrive_human_gates(
            control.as_ref(),
            claims.as_ref(),
            &gate_requests,
            &clock,
            log.as_ref(),
            &mut routed_gates,
        )?;

        let mut states: HashMap<String, TaskState> =
            rows.iter().map(|r| (r.task_id.clone(), r.state)).collect();
        let item_by_id: HashMap<String, &WorkItem> =
            items.iter().map(|i| (i.task.id.clone(), i)).collect();
        let row_by_id: HashMap<String, &TaskStatusRow> =
            rows.iter().map(|r| (r.task_id.clone(), r)).collect();
        let blocked_ids: HashSet<String> = rows
            .iter()
            .filter(|r| is_blocked_interrupted(r))
            .map(|r| r.task_id.clone())
            .collect();

        // Graph snapshot (spec 00055): one immutable cross-section at the
        // top of the pass, after graph/states/readiness are known and
        // BEFORE any dispatch. A pure read-and-record side channel.
        // Assembled from the graph's surviving items: an isolated task
        // (duplicate id, self-dependency, cycle) is not part of the graph
        // this pass built, and a snapshot is keyed by task id.
        let snapshot_items = assemble_graph_snapshot_items(
            graph.items(),
            graph,
            &states,
            &worker_capabilities,
            claims.as_ref(),
            &satisfied_prereqs,
        )?;
        let record_snapshot = match request.snapshot_policy {
            GraphSnapshotPolicy::EveryPass => true,
            GraphSnapshotPolicy::SkipUnchanged => {
                let mut sorted = snapshot_items.clone();
                sorted.sort_by(|a, b| a.task_id.cmp(&b.task_id));
                match claims.latest_graph_snapshot()? {
                    Some(latest) => claims.list_graph_snapshot_items(latest.id)? != sorted,
                    None => true,
                }
            }
        };
        if record_snapshot {
            claims.record_graph_snapshot(&snapshot_items, clock())?;
        }

        // Session-limit-driven pool-wide claim pause: while the horizon is
        // in the future every driver declines fresh and resume claims;
        // bookkeeping is deliberately not gated.
        let mut claim_pause_until: Option<DateTime<Utc>> = None;
        if request.session_pause_ceiling_seconds > 0.0 {
            claim_pause_until = claims.session_pause_until(clock())?;
            if let Some(until) = claim_pause_until {
                emit(
                    log.as_ref(),
                    format!(
                        "[orchestrate] session-limit pause active until {}; declining fresh and resume claims this pass",
                        until.to_rfc3339()
                    ),
                );
            }
        }

        // 1. Reactive unblock + resume, claim-gated. The first that unblocks
        //    is resumed on its own run_id; then restart so the changed state
        //    is re-read.
        let mut progressed = false;
        for row in &rows {
            if claim_pause_until.is_some() {
                break;
            }
            if !is_blocked_interrupted(row) {
                continue;
            }
            let Some(run_id) = row.latest_run_id.clone() else {
                continue;
            };
            if attempted_resume.contains(&run_id) {
                continue;
            }
            let item = item_by_id[&row.task_id];
            let Some(claim) = acquire_with_grace(
                claims.as_ref(),
                &row.task_id,
                &wid,
                clock(),
                request.lease_seconds,
                &item.conflict_keys,
                steal_grace,
            )?
            else {
                continue; // another worker owns this task right now
            };
            // Provision the sandbox before recheck so the blocked predicates
            // are evaluated in the same (consumer-prepared) tree the resumed
            // run will use. A failing provider skips this task for the
            // session; the claim release and the prepare-skip witness commit
            // together.
            let handle = match prepare_sandbox_guarded(
                strategy.as_ref(),
                &SandboxRequest::resume(item, run_id.clone()),
            ) {
                Ok(handle) => handle,
                Err(err) => {
                    attempted_resume.insert(run_id.clone());
                    emit(
                        log.as_ref(),
                        format!(
                            "[orchestrate] {}: prepare failed (PrepareError: {err}); skipping",
                            row.task_id
                        ),
                    );
                    claims.record_prepare_skip(&claim, &format!("PrepareError: {err}"), clock())?;
                    continue;
                }
            };
            let recheck = recheck_blocked_lifecycle(
                control.clone(),
                &run_id,
                &item.task,
                RecheckOptions {
                    cwd: Some(handle.path.clone()),
                    sink: sink.clone(),
                    now: Some(clock.clone()),
                    ..RecheckOptions::default()
                },
            );
            let applied = match recheck {
                Ok(outcome) => outcome.applied,
                Err(err) if is_optimistic_conflict(&err) => false, // a peer got there first
                Err(err) => {
                    release_after_prepare(&claims, &claim, &handle, &clock, log.as_ref());
                    return Err(err.into());
                }
            };
            if !applied {
                // Still blocked (or lost the race): dispose the sandbox
                // provisioned for the recheck and let go of the claim.
                release_after_prepare(&claims, &claim, &handle, &clock, log.as_ref());
                continue;
            }
            attempted_resume.insert(run_id.clone());
            match drive_or_relinquish(&ctx, claim, item, handle, Some(&run_id)).await? {
                DriveDisposition::Recorded(record) => runs.push(record),
                DriveDisposition::Relinquished => {}
                DriveDisposition::Shutdown => {
                    return Err(OrchestrateError::Interrupted {
                        report: report(&wid, &recovered, &runs),
                    });
                }
            }
            progressed = true;
            break;
        }
        if progressed {
            continue;
        }

        // 1b. Reactive resolve of AWAITING_APPROVAL gates, claim-gated. An
        //     `approved_done` lands the parked branch inline through the
        //     same held-out-gate + submit ladder a first-attempt DONE lands
        //     through; with no pending command the run is marked with an
        //     expiring deadline so the no-op resolve is not re-run every
        //     pass.
        for row in &rows {
            if !is_awaiting_approval(row) {
                continue;
            }
            let Some(run_id) = row.latest_run_id.clone() else {
                continue;
            };
            if let Some(marked_until) = attempted_approve.get(&run_id)
                && clock() < *marked_until
            {
                continue;
            }
            let item = item_by_id[&row.task_id];
            let Some(claim) = acquire_with_grace(
                claims.as_ref(),
                &row.task_id,
                &wid,
                clock(),
                request.lease_seconds,
                &BTreeSet::new(),
                steal_grace,
            )?
            else {
                continue;
            };
            let mark_ttl =
                ChronoDuration::milliseconds((APPROVAL_SWEEP_MARK_TTL_SECONDS * 1000.0) as i64);
            let Some(mut lifecycle) = control.load_lifecycle(&run_id)? else {
                attempted_approve.insert(run_id.clone(), clock() + mark_ttl);
                claims.release_claim(&claim, Some(clock()))?;
                continue;
            };
            let resolved = resolve_manual_approval(
                &mut lifecycle,
                control.clone(),
                &item.task,
                ResolveApprovalOptions {
                    max_retries: request.max_retries,
                    sink: sink.clone(),
                    now: Some(clock.clone()),
                },
            );
            let outcome = match resolved {
                Ok(outcome) => outcome,
                Err(err) if is_optimistic_conflict(&err) => {
                    claims.release_claim(&claim, Some(clock()))?;
                    continue; // another worker resolved it first
                }
                Err(err) => {
                    claims.release_claim(&claim, Some(clock()))?;
                    return Err(err.into());
                }
            };
            if !outcome.applied {
                attempted_approve.insert(run_id.clone(), clock() + mark_ttl);
                claims.release_claim(&claim, Some(clock()))?;
                continue;
            }
            if outcome.reason == "approved_done" && has_strategy {
                // Land the parked branch under the claim we already hold,
                // heartbeat-renewed so a long rebase/re-verify cannot outlive
                // the lease, through the standard gate + submit ladder.
                let heartbeat = ClaimHeartbeat::start(
                    claims.clone(),
                    claim,
                    request.lease_seconds,
                    heartbeat_interval,
                    clock.clone(),
                );
                let land_request = SubmitRequest::new(
                    item.task.clone(),
                    run_id.clone(),
                    Status::Done,
                    request.sandbox_root.join(&row.task_id),
                )
                .with_item(item);
                let mut log_line = |line: &str| emit(log.as_ref(), line);
                let decision = gate_landing(
                    control.as_ref(),
                    request.held_out_source.as_deref(),
                    &land_request,
                    &gate_options,
                    || clock(),
                    &mut log_line,
                )
                .await;
                if decision.lands() {
                    submit_guarded(strategy.as_ref(), &land_request);
                }
                let latest = heartbeat.stop();
                claims.release_claim(&latest, Some(clock()))?;
            } else {
                claims.release_claim(&claim, Some(clock()))?;
            }
            progressed = true;
            break;
        }
        if progressed {
            continue;
        }

        // 1c. Bounded re-drive of parked landings (spec 00069), opt-in and
        //     strategy-gated. A re-attempt that lands advanced the base and
        //     may promote dependents, so state is re-read on the next pass.
        if let Some(bound) = request.landing_redrive_bound.filter(|b| *b > 0)
            && has_strategy
        {
            let redrive_requests: Vec<SubmitRequest> = rows
                .iter()
                .filter(|r| r.latest_status == Some(Status::Done))
                .filter_map(|r| {
                    let run_id = r.latest_run_id.clone()?;
                    let item = item_by_id[&r.task_id];
                    Some(
                        SubmitRequest::new(
                            item.task.clone(),
                            run_id,
                            Status::Done,
                            request.sandbox_root.join(&r.task_id),
                        )
                        .with_item(item),
                    )
                })
                .collect();
            let redrive_ctx = LandingRedriveContext {
                control: &control,
                claims: &claims,
                strategy: strategy.as_ref(),
                worker_id: &wid,
                bound,
                held_out_source: request.held_out_source.as_deref(),
                gate_options: &gate_options,
                lease_seconds: request.lease_seconds,
                heartbeat_interval,
                steal_grace_seconds: steal_grace,
                now: &clock,
                log: log.as_ref(),
            };
            let outcomes =
                redrive_parked_landings(&redrive_ctx, &redrive_requests, &mut never_parked).await?;
            if outcomes.iter().any(|o| o.result == RedriveResult::Landed) {
                continue;
            }
        }

        // 2. Fresh selection over the prerequisite graph. Exclude
        //    still-blocked lifecycles and tasks already run this session
        //    from candidacy while keeping the full row set so a DONE
        //    dependency still promotes its dependents. Skip -- but do not
        //    consume -- tasks a peer worker currently holds.
        let mut held: HashSet<String> = HashSet::new();
        let mut ran_fresh = false;
        loop {
            if claim_pause_until.is_some() {
                break;
            }
            if shutdown.as_ref().is_some_and(ShutdownSignal::is_triggered) {
                return Err(OrchestrateError::Interrupted {
                    report: report(&wid, &recovered, &runs),
                });
            }
            let exclude: HashSet<String> = attempted_fresh
                .iter()
                .chain(blocked_ids.iter())
                .chain(held.iter())
                .chain(reachability_held.iter())
                .cloned()
                .collect();
            let query = ReadySetQuery {
                excluded: exclude.clone(),
                worker_capabilities: worker_capabilities.clone(),
                satisfied_prerequisites: satisfied_prereqs.iter().cloned().collect(),
            };
            // Highest-priority-first over the validated graph; taking the
            // first preserves the deterministic selection.
            let pick_id = graph
                .ready_set(&state_values(&states), &query)
                .first()
                .map(|item| item.task.id.clone());
            let Some(pick_id) = pick_id else {
                break;
            };
            let pick = row_by_id[&pick_id];
            let item = item_by_id[&pick_id];
            // Schedule-time static-validation gate (spec 00034): a task
            // whose graders are statically broken is skipped for the session.
            if let Some(repo_root) = &request.repo_root {
                let defects = validate_task(&item.task, repo_root);
                if !defects.is_empty() {
                    attempted_fresh.insert(pick_id.clone());
                    for defect in defects {
                        emit(
                            log.as_ref(),
                            format!(
                                "[orchestrate] {pick_id}: invalid task definition ({}); skipping",
                                defect.detail
                            ),
                        );
                    }
                    continue;
                }
            }
            let Some(claim) = acquire_with_grace(
                claims.as_ref(),
                &pick_id,
                &wid,
                clock(),
                request.lease_seconds,
                &item.conflict_keys,
                steal_grace,
            )?
            else {
                held.insert(pick_id);
                continue;
            };
            // Post-claim terminal-state recheck (closes the stale-snapshot
            // TOCTOU): `states` was read at the top of this pass; a peer may
            // have driven this task to DONE and released its claim before we
            // acquired ours. Refresh just the picked task's state under our
            // claim and re-test readiness with the exact predicate the
            // scheduler used; if a peer finished it, drop the claim and
            // exclude it rather than minting a second run.
            let refreshed = task_state(control.as_ref(), &pick_id)?;
            states.insert(pick_id.clone(), refreshed.state);
            let still_ready = graph
                .ready_set(&state_values(&states), &query)
                .iter()
                .any(|i| i.task.id == pick_id);
            if !still_ready {
                claims.release_claim(&claim, Some(clock()))?;
                attempted_fresh.insert(pick_id);
                continue;
            }
            attempted_fresh.insert(pick_id.clone());
            // A bare-interrupted lifecycle must RESUME on its own run_id so
            // the entry-time INTERRUPTED -> READY normalization fires and the
            // paused work continues.
            let resume_run_id = if refreshed.state == TaskState::Interrupted {
                refreshed
                    .latest_run_id
                    .clone()
                    .or_else(|| pick.latest_run_id.clone())
            } else {
                None
            };
            let sandbox_request = match &resume_run_id {
                Some(run_id) => SandboxRequest::resume(item, run_id.clone()),
                None => SandboxRequest::fresh(item),
            };
            let handle = match prepare_sandbox_guarded(strategy.as_ref(), &sandbox_request) {
                Ok(handle) => handle,
                Err(err) => {
                    claims.record_prepare_skip(&claim, &format!("PrepareError: {err}"), clock())?;
                    emit(
                        log.as_ref(),
                        format!(
                            "[orchestrate] {pick_id}: prepare failed (PrepareError: {err}); skipping"
                        ),
                    );
                    continue;
                }
            };
            match drive_or_relinquish(&ctx, claim, item, handle, resume_run_id.as_deref()).await? {
                DriveDisposition::Recorded(record) => runs.push(record),
                DriveDisposition::Relinquished => {}
                DriveDisposition::Shutdown => {
                    return Err(OrchestrateError::Interrupted {
                        report: report(&wid, &recovered, &runs),
                    });
                }
            }
            ran_fresh = true;
            break;
        }
        if ran_fresh {
            continue;
        }

        // Nothing to resume and nothing fresh we could claim: done.
        break;
    }

    Ok(report(&wid, &recovered, &runs))
}

/// Dispose a sandbox provisioned for a recheck that did not resume, then
/// release the claim. The teardown is best-effort and never unwinds.
fn release_after_prepare(
    claims: &Arc<dyn ClaimStore>,
    claim: &TaskClaim,
    handle: &SandboxHandle,
    clock: &Clock,
    log: Option<&LogSink>,
) {
    if let Err(err) = handle.run_teardown() {
        emit(
            log,
            format!(
                "[orchestrate] {}: sandbox teardown failed ({err}); continuing",
                claim.task_id
            ),
        );
    }
    if let Err(err) = claims.release_claim(claim, Some(clock())) {
        emit(
            log,
            format!(
                "[orchestrate] {}: lease release failed ({err}); the lease lapses on its own",
                claim.task_id
            ),
        );
    }
}
