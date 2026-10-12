//! One task execution under its lease: run (or resume), gate the land,
//! submit, report to the source, tear down the sandbox, release the lease.

use crate::claims::{ClaimStore, TaskClaim};
use crate::held_out_gate::{GateOptions, GateOutcome, HeldOutGraderSource, gate_landing};
use crate::sources::{GraderReceipt, LogSink, WorkItem, WorkReport, WorkSource};
use crate::strategy::{SandboxHandle, SubmitRequest, SubmitStrategy, submit_guarded};
use flywheel_core::harness::ShutdownSignal;
use flywheel_core::harness::{HarnessConfig, LandabilityGate, SharedSink};
use flywheel_core::invoker_client::{Clock, ProgressProbe};
use flywheel_core::lifecycle::{Lifecycle, Status};
use flywheel_core::store::{HarnessStore, StoreError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::heartbeat::ClaimHeartbeat;
use super::prereqs::latest_lifecycle_row;
use super::redrive::probe_landability;
use super::run_task::{AgentPrimitives, RunMode, RunTask, RunTaskError, RunTaskRequest};
use super::sweep::emit;
use super::{OrchestrateError, RunRecord};

/// The consumer's git-free progress-probe factory: given a run's sandbox it
/// returns the opaque closure the harness's checkpoint nudge consults.
pub type ProgressProbeFactory = Arc<dyn Fn(&Path) -> ProgressProbe + Send + Sync>;

/// What one drive settled to.
#[derive(Debug)]
pub(super) enum DriveDisposition {
    /// The run finished (or was contained as an in-band interrupt).
    Recorded(RunRecord),
    /// A peer took the claim mid-run; nothing recorded, re-evaluate.
    Relinquished,
    /// The worker itself is being torn down; stop draining.
    Shutdown,
}

/// The per-session inputs every drive shares.
pub(super) struct DriveContext {
    pub control: Arc<dyn HarnessStore>,
    pub claims: Arc<dyn ClaimStore>,
    pub run_task: Arc<dyn RunTask>,
    pub strategy: Arc<dyn SubmitStrategy>,
    /// `Some` only when the caller wired a strategy (arms the landability
    /// gate); `None` keeps the harness's DONE edge ungated.
    pub landability_probe: Option<Arc<dyn SubmitStrategy>>,
    pub source: Arc<dyn WorkSource>,
    pub held_out_source: Option<Arc<dyn HeldOutGraderSource>>,
    pub gate_options: GateOptions,
    pub progress_probe_factory: Option<ProgressProbeFactory>,
    pub base_config: HarnessConfig,
    pub agent: AgentPrimitives,
    pub invoke: Option<flywheel_core::agents_invoke::InvokeFn>,
    pub model: Option<String>,
    pub max_turns: u32,
    pub max_retries: u32,
    pub worker_id: String,
    pub lease_seconds: f64,
    pub heartbeat_interval: f64,
    pub session_pause_ceiling_seconds: f64,
    pub telemetry_root: Option<PathBuf>,
    pub sink: Option<SharedSink>,
    pub shutdown: Option<ShutdownSignal>,
    pub now: Clock,
    pub log: Option<LogSink>,
}

/// Why a drive did not produce a [`RunRecord`].
#[derive(Debug, thiserror::Error)]
pub(super) enum DriveError {
    #[error("claim on task {0:?} lost")]
    ClaimLost(String),
    #[error(transparent)]
    RunTask(RunTaskError),
    #[error(transparent)]
    Orchestrate(#[from] OrchestrateError),
}

impl From<StoreError> for DriveError {
    fn from(err: StoreError) -> Self {
        DriveError::Orchestrate(OrchestrateError::Store(err))
    }
}

impl From<crate::claims::ClaimStoreError> for DriveError {
    fn from(err: crate::claims::ClaimStoreError) -> Self {
        DriveError::Orchestrate(OrchestrateError::Claims(err))
    }
}

/// Holds the run's sandbox handle and heartbeat-renewed lease; `finish`
/// (or `Drop`, on cancellation) tears the sandbox down, stops renewing, and
/// releases the lease -- on EVERY exit path, so a container is never leaked
/// and a lease never wedges a task's conflict keys.
struct RunGuard {
    claims: Arc<dyn ClaimStore>,
    handle: SandboxHandle,
    heartbeat: Option<ClaimHeartbeat>,
    now: Clock,
    log: Option<LogSink>,
    task_id: String,
}

impl RunGuard {
    fn lost(&self) -> bool {
        self.heartbeat.as_ref().is_some_and(ClaimHeartbeat::lost)
    }

    fn settle(&mut self) {
        let Some(heartbeat) = self.heartbeat.take() else {
            return;
        };
        if let Err(err) = self.handle.run_teardown() {
            emit(
                self.log.as_ref(),
                format!(
                    "[orchestrate] {}: sandbox teardown failed ({err}); continuing",
                    self.task_id
                ),
            );
        }
        let latest: TaskClaim = heartbeat.stop();
        if let Err(err) = self.claims.release_claim(&latest, Some((self.now)())) {
            emit(
                self.log.as_ref(),
                format!(
                    "[orchestrate] {}: lease release failed ({err}); the lease lapses on its own",
                    self.task_id
                ),
            );
        }
    }

    fn finish(mut self) {
        self.settle();
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.settle();
    }
}

/// Project the final attempt's grader receipts for a work report. A run
/// that never reached grading yields an empty list.
pub fn final_grader_receipts(
    control: &dyn HarnessStore,
    run_id: &str,
) -> Result<Vec<GraderReceipt>, StoreError> {
    let attempts = control.list_attempts(run_id)?;
    let Some(last) = attempts.last() else {
        return Ok(Vec::new());
    };
    Ok(control
        .list_grader_results(run_id, last.number)?
        .into_iter()
        .map(|record| GraderReceipt {
            ordinal: record.ordinal,
            grader_type: record.grader_type,
            name: record.grader_name,
            passed: record.passed,
        })
        .collect())
}

fn submit_request_for(
    item: &WorkItem,
    run_id: &str,
    status: Status,
    sandbox: &Path,
) -> SubmitRequest {
    SubmitRequest::new(item.task.clone(), run_id, status, sandbox).with_item(item)
}

/// Run (or resume) one task while a heartbeat holds its lease, gate and
/// hand the terminal status to `submit`, report it to the work source, then
/// tear down and release.
pub(super) async fn drive_under_lease(
    ctx: &DriveContext,
    claim: TaskClaim,
    item: &WorkItem,
    handle: SandboxHandle,
    run_id: Option<&str>,
) -> Result<RunRecord, DriveError> {
    let task_id = item.task.id.clone();
    let sandbox = handle.path.clone();
    let mode = if run_id.is_some() {
        RunMode::Resume
    } else {
        RunMode::Fresh
    };

    // Landable-change gate (spec 00061): a git-free closure the harness
    // consults at the verify-passed VALIDATING -> DONE boundary. A probe
    // panic fails open (landable): a buggy predicate must never block.
    let landability_gate: Option<LandabilityGate> = ctx.landability_probe.as_ref().map(|probe| {
        let probe = Arc::clone(probe);
        let item = item.clone();
        let sandbox = sandbox.clone();
        let run_id = run_id.map(str::to_owned).unwrap_or_default();
        let log = ctx.log.clone();
        Arc::new(move || {
            let request = submit_request_for(&item, &run_id, Status::Done, &sandbox);
            match probe_landability(Some(probe.as_ref()), &request) {
                Ok(verdict) if verdict.landable => None,
                Ok(verdict) => Some(if verdict.reason.is_empty() {
                    "no landable change".to_owned()
                } else {
                    verdict.reason
                }),
                Err(message) => {
                    emit(
                        log.as_ref(),
                        format!(
                            "[orchestrate] {}: landability probe failed (panic: {message}); treating as landable",
                            item.task.id
                        ),
                    );
                    None
                }
            }
        }) as LandabilityGate
    });
    let mut config = ctx.base_config.clone();
    config.landability_gate = landability_gate;
    config.checkpoint_progress_probe = ctx
        .progress_probe_factory
        .as_ref()
        .map(|factory| factory(&sandbox));
    config.worktree = Some(sandbox.clone());

    let mut lifecycle = match run_id {
        Some(run_id) => Lifecycle::new(task_id.clone()).with_run_id(run_id),
        None => Lifecycle::new(task_id.clone()),
    };
    lifecycle.worker_id = ctx.worker_id.clone();
    lifecycle.source = item.source_ref.clone();

    let agent = AgentPrimitives {
        agent_env: handle.apply_env(&ctx.agent.agent_env),
        ..ctx.agent.clone()
    };
    let invoke = handle.wrap_invoke(ctx.invoke.clone());

    let heartbeat = ClaimHeartbeat::start(
        ctx.claims.clone(),
        claim,
        ctx.lease_seconds,
        ctx.heartbeat_interval,
        ctx.now.clone(),
    );
    let guard = RunGuard {
        claims: ctx.claims.clone(),
        handle,
        heartbeat: Some(heartbeat),
        now: ctx.now.clone(),
        log: ctx.log.clone(),
        task_id: task_id.clone(),
    };

    let request = RunTaskRequest {
        task: item.task.clone(),
        lifecycle,
        mode,
        store: ctx.control.clone(),
        sandbox: sandbox.clone(),
        agent,
        invoke,
        config,
        model: ctx.model.clone(),
        max_turns: ctx.max_turns,
        max_retries: ctx.max_retries,
        worker_id: ctx.worker_id.clone(),
        source_ref: item.source_ref.clone(),
        telemetry_root: ctx.telemetry_root.clone(),
        sink: ctx.sink.clone(),
        now: ctx.now.clone(),
        shutdown: ctx.shutdown.clone(),
        log: ctx.log.clone(),
    };
    let outcome = match ctx.run_task.run(request).await {
        Ok(outcome) => outcome,
        Err(err) => {
            guard.finish();
            return Err(DriveError::RunTask(err));
        }
    };

    // Session-limit-driven pool-wide claim pause: recorded before the
    // claim-loss check so the pool-wide fact survives a stolen lease.
    if let Some(reset_at) = outcome.session_limit_reset {
        let paused_until = ctx.claims.record_session_pause(
            reset_at,
            (ctx.now)(),
            ctx.session_pause_ceiling_seconds,
            &outcome.lifecycle.run_id,
        );
        match paused_until {
            Ok(Some(until)) => emit(
                ctx.log.as_ref(),
                format!(
                    "[orchestrate] {task_id}: session limit reached (reset {}); pausing pool-wide claiming until {}",
                    reset_at.to_rfc3339(),
                    until.to_rfc3339()
                ),
            ),
            Ok(None) => {}
            Err(err) => {
                guard.finish();
                return Err(err.into());
            }
        }
    }
    if guard.lost() {
        guard.finish();
        return Err(DriveError::ClaimLost(task_id));
    }

    let run_id = outcome.lifecycle.run_id.clone();
    let status = outcome.lifecycle.status;
    let receipts = match final_grader_receipts(ctx.control.as_ref(), &run_id) {
        Ok(receipts) => receipts,
        Err(err) => {
            emit(
                ctx.log.as_ref(),
                format!(
                    "[orchestrate] {task_id}: grader receipt projection failed ({err}); continuing"
                ),
            );
            Vec::new()
        }
    };

    // Execute-time held-out landing gate (spec 00050): while the lease is
    // held and BEFORE submit, grade the committed sandbox out of band. A
    // blocked gate suppresses submit; the verdict and park are recorded.
    let submit_request =
        submit_request_for(item, &run_id, status, &sandbox).with_receipts(receipts.clone());
    let mut log_line = |line: &str| emit(ctx.log.as_ref(), line);
    let decision = gate_landing(
        ctx.control.as_ref(),
        ctx.held_out_source.as_deref(),
        &submit_request,
        &ctx.gate_options,
        || (ctx.now)(),
        &mut log_line,
    )
    .await;
    let (gate, gate_reason): (Option<GateOutcome>, String) = match &decision {
        crate::held_out_gate::LandingDecision::Land { verdict } => (
            verdict.as_ref().map(|v| v.outcome),
            verdict
                .as_ref()
                .map(|v| v.reason.clone())
                .unwrap_or_default(),
        ),
        crate::held_out_gate::LandingDecision::Park { verdict, .. } => {
            (Some(verdict.outcome), verdict.reason.clone())
        }
    };
    if decision.lands() {
        submit_guarded(ctx.strategy.as_ref(), &submit_request);
    }
    let report = WorkReport {
        task_id: task_id.clone(),
        source_ref: item.source_ref.clone(),
        run_id: run_id.clone(),
        status,
        error: outcome.lifecycle.error.clone(),
        graders: receipts,
    };
    if let Err(err) = ctx.source.report(&report) {
        emit(
            ctx.log.as_ref(),
            format!(
                "[orchestrate] {task_id}: work-source report failed (WorkSourceError: {err}); continuing"
            ),
        );
    }
    guard.finish();
    Ok(RunRecord {
        task_id,
        run_id,
        status,
        mode,
        worker_id: ctx.worker_id.clone(),
        gate,
        gate_reason,
    })
}

/// Drive one task, containing a mid-run claim loss and an in-band interrupt
/// to that one task.
///
/// A peer stealing the lease (the worker stalled past the window, or clock
/// skew) can finalize the lifecycle out from under us; the losing run then
/// hits the optimistic-concurrency check or its heartbeat reports the loss.
/// Either is contained here as a re-evaluation. An in-band `interrupt`
/// control command surfaces as the harness's re-raised `Interrupted`; the
/// discriminator is two-sided: the worker itself must NOT be shutting down
/// (that propagates so the worker actually stops) AND the store must show
/// this task's lifecycle finalized `INTERRUPTED`.
pub(super) async fn drive_or_relinquish(
    ctx: &DriveContext,
    claim: TaskClaim,
    item: &WorkItem,
    handle: SandboxHandle,
    run_id: Option<&str>,
) -> Result<DriveDisposition, OrchestrateError> {
    let task_id = item.task.id.clone();
    match drive_under_lease(ctx, claim, item, handle, run_id).await {
        Ok(record) => Ok(DriveDisposition::Recorded(record)),
        Err(DriveError::ClaimLost(_)) => {
            emit(
                ctx.log.as_ref(),
                format!(
                    "[orchestrate] {task_id}: claim lost mid-run (ClaimLostError); a peer took over, re-evaluating"
                ),
            );
            Ok(DriveDisposition::Relinquished)
        }
        Err(DriveError::RunTask(err)) if err.is_optimistic_concurrency() => {
            emit(
                ctx.log.as_ref(),
                format!(
                    "[orchestrate] {task_id}: claim lost mid-run (OptimisticConcurrencyError); a peer took over, re-evaluating"
                ),
            );
            Ok(DriveDisposition::Relinquished)
        }
        Err(DriveError::RunTask(RunTaskError::Interrupted)) => {
            if ctx
                .shutdown
                .as_ref()
                .is_some_and(ShutdownSignal::is_triggered)
            {
                return Ok(DriveDisposition::Shutdown);
            }
            let latest = latest_lifecycle_row(ctx.control.as_ref(), &task_id)?;
            let Some(latest) = latest.filter(|row| row.status == Status::Interrupted) else {
                return Err(OrchestrateError::RunTask(RunTaskError::Interrupted));
            };
            emit(
                ctx.log.as_ref(),
                format!(
                    "[orchestrate] {task_id}: run {} interrupted in-band (control command); continuing with remaining tasks",
                    latest.run_id
                ),
            );
            Ok(DriveDisposition::Recorded(RunRecord {
                task_id,
                run_id: latest.run_id,
                status: Status::Interrupted,
                mode: if run_id.is_some() {
                    RunMode::Resume
                } else {
                    RunMode::Fresh
                },
                worker_id: ctx.worker_id.clone(),
                gate: None,
                gate_reason: String::new(),
            }))
        }
        Err(DriveError::RunTask(err)) => Err(OrchestrateError::RunTask(err)),
        Err(DriveError::Orchestrate(err)) => Err(err),
    }
}
