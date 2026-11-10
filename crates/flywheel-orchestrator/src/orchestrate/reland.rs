//! The operator-initiated one-shot re-land (spec 00086, D-1/D-2/D-3): one
//! landing attempt for a task's parked latest DONE run, through exactly the
//! held-out gate and `submit_guarded` path the landing re-driver uses, under
//! a freshly acquired heartbeat-renewed claim. It never re-arms the
//! re-driver's bound, and it clears the task's stop only when it lands.

use crate::claims::{ClaimStore, RESOLUTION_ATTRIBUTION_RELAND, STOP_RESOLVED};
use crate::held_out_gate::{GateOptions, HeldOutGraderSource};
use crate::sources::{LogSink, WorkItem};
use crate::strategy::{LandingCandidate, SubmitOutcome, SubmitRequest, SubmitStrategy};
use flywheel_core::events::{
    DomainEventKind, DomainEventPayload, REDRIVE_INITIATOR_OPERATOR, REDRIVE_RESULT_LANDED,
    REDRIVE_RESULT_REPARKED,
};
use flywheel_core::invoker_client::Clock;
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{HarnessStore, LifecycleFilter};
use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use super::OrchestrateError;
use super::redrive::{
    LandingAttempt, LandingPath, land_once, record_landing_redrive, redrive_record,
    resolved_since_latest_park,
};
use super::sweep::emit;

/// The caller-side workspace seam of a re-land: the strategy-specific
/// checks and provisioning the orchestrator cannot do itself (the CLI verb
/// wires the git implementation).
pub trait RelandWorkspace: Send + Sync {
    /// Read-only check run before anything is claimed or recorded. An `Err`
    /// refuses the re-land with that reason (e.g. the run's task branch no
    /// longer exists).
    fn preflight(&self, request: &SubmitRequest) -> Result<(), String>;

    /// Under the task's claim, before the gate: make `request.sandbox` hold
    /// the task branch's current head (re-materializing a pruned worktree,
    /// D-7). An `Err` aborts the attempt with nothing recorded.
    fn materialize(&self, request: &SubmitRequest) -> Result<(), String>;
}

/// Everything one [`reland`] call needs.
pub struct RelandContext<'a> {
    pub control: &'a Arc<dyn HarnessStore>,
    pub claims: &'a Arc<dyn ClaimStore>,
    pub strategy: &'a dyn SubmitStrategy,
    /// Builds the scratch tree the held-out gate grades: the task branch
    /// head rebased onto the current base.
    pub candidate: &'a dyn LandingCandidate,
    /// `None` uses `request.sandbox` as it stands.
    pub workspace: Option<&'a dyn RelandWorkspace>,
    pub worker_id: &'a str,
    pub held_out_source: Option<&'a dyn HeldOutGraderSource>,
    pub gate_options: &'a GateOptions,
    pub lease_seconds: f64,
    pub heartbeat_interval: f64,
    pub steal_grace_seconds: f64,
    pub now: &'a Clock,
    pub log: Option<&'a LogSink>,
}

impl RelandContext<'_> {
    fn landing_path(&self) -> LandingPath<'_> {
        LandingPath {
            control: self.control,
            claims: self.claims,
            strategy: self.strategy,
            held_out_source: self.held_out_source,
            gate_options: self.gate_options,
            lease_seconds: self.lease_seconds,
            heartbeat_interval: self.heartbeat_interval,
            now: self.now,
            log: self.log,
        }
    }
}

/// Why a re-land refused before attempting: nothing was claimed and nothing
/// was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelandRefusal {
    /// The task has no DONE run.
    NoDoneRun { task_id: String },
    /// The latest DONE run never parked its landing.
    NeverParked { run_id: String },
    /// The latest DONE run already landed after its latest park.
    AlreadyLanded { run_id: String },
    /// An attributed `stop-resolved` is at least as recent as the latest
    /// park (abandoned, or confirmed landed by the archive probe).
    ResolvedSinceLatestPark { run_id: String },
    /// Another worker holds the task's claim.
    ClaimHeld { run_id: String, holder: String },
    /// The workspace seam refused or could not provision the attempt (no
    /// gate ran).
    Workspace { run_id: String, reason: String },
}

impl fmt::Display for RelandRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelandRefusal::NoDoneRun { task_id } => {
                write!(f, "{task_id} has no DONE run to re-land")
            }
            RelandRefusal::NeverParked { run_id } => {
                write!(
                    f,
                    "run {run_id} never parked its landing; nothing to re-land"
                )
            }
            RelandRefusal::AlreadyLanded { run_id } => {
                write!(f, "run {run_id} already landed after its latest park")
            }
            RelandRefusal::ResolvedSinceLatestPark { run_id } => write!(
                f,
                "run {run_id}'s strand was resolved after its latest park; not re-landing"
            ),
            RelandRefusal::ClaimHeld { run_id, holder } => write!(
                f,
                "the task claim for run {run_id} is held by worker {holder}"
            ),
            RelandRefusal::Workspace { run_id, reason } => {
                write!(f, "cannot re-land run {run_id}: {reason}")
            }
        }
    }
}

/// What one [`reland`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelandOutcome {
    /// The attempt landed; `landing_redriven{landed, operator}` and a
    /// `reland`-attributed `stop-resolved` were appended.
    Landed {
        run_id: String,
        strategy: String,
        landed_ref: String,
    },
    /// The attempt parked (the gate's or `submit`'s park witness plus
    /// `landing_redriven{re-parked, operator}`); the run stays routed.
    Reparked {
        run_id: String,
        park_kind: String,
        detail: String,
    },
    /// `submit` neither landed nor parked; nothing further recorded.
    NoOp { run_id: String, reason: String },
    /// The lease was lost before the irreversible step; nothing submitted.
    LeaseLost { run_id: String },
    /// Refused before attempting; nothing claimed or recorded.
    Refused(RelandRefusal),
}

impl RelandOutcome {
    pub fn is_landed(&self) -> bool {
        matches!(self, RelandOutcome::Landed { .. })
    }
}

/// The re-landable DONE run of `task_id`, or why there is none. Read-only.
fn relandable_run(
    control: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    task_id: &str,
) -> Result<Result<(String, String), RelandRefusal>, OrchestrateError> {
    let filter = LifecycleFilter::default()
        .with_task_id(task_id)
        .with_statuses([Status::Done]);
    let Some(done) = control.list_lifecycles(&filter)?.into_iter().next() else {
        return Ok(Err(RelandRefusal::NoDoneRun {
            task_id: task_id.to_owned(),
        }));
    };
    let run_id = done.run_id;
    let Some(park_event) =
        control.latest_domain_event_of_kind(&run_id, DomainEventKind::LandingParked)?
    else {
        return Ok(Err(RelandRefusal::NeverParked { run_id }));
    };
    let park_kind = match &park_event.payload {
        DomainEventPayload::LandingParked(parked) => parked.park_kind.clone(),
        _ => String::new(),
    };
    let landed_after = control
        .latest_domain_event_of_kind(&run_id, DomainEventKind::Landed)?
        .is_some_and(|landed| landed.ts >= park_event.ts);
    if landed_after {
        return Ok(Err(RelandRefusal::AlreadyLanded { run_id }));
    }
    if resolved_since_latest_park(control, claims, task_id, &run_id)? {
        return Ok(Err(RelandRefusal::ResolvedSinceLatestPark { run_id }));
    }
    Ok(Ok((run_id, park_kind)))
}

/// One operator-initiated landing attempt for `item`'s latest DONE run,
/// landing from `sandbox` (the run's task-branch checkout).
///
/// Refuses (nothing claimed, nothing recorded) when the task has no DONE
/// run, the run never parked, it already landed after its latest park, an
/// attributed resolution postdates its latest park, the workspace preflight
/// refuses, or another worker holds the claim. Otherwise acquires the claim
/// and makes exactly one attempt through the re-driver's own landing path,
/// with the held-out gate grading the candidate tree (the branch head
/// rebased onto the current base). On landing it appends
/// `landing_redriven{landed, operator}` after the `Landed` witness, then a
/// `reland`-attributed `stop-resolved` for the task; on a park it appends
/// `landing_redriven{re-parked, operator}` beside the park witness and no
/// resolution. The re-driver's bound and routing are never touched.
pub async fn reland(
    ctx: &RelandContext<'_>,
    item: &WorkItem,
    sandbox: impl Into<PathBuf>,
) -> Result<RelandOutcome, OrchestrateError> {
    let task_id = item.task.id.clone();
    let (run_id, cleared_park_kind) =
        match relandable_run(ctx.control.as_ref(), ctx.claims.as_ref(), &task_id)? {
            Ok(found) => found,
            Err(refusal) => return Ok(RelandOutcome::Refused(refusal)),
        };
    let request = SubmitRequest::new(item.task.clone(), run_id.clone(), Status::Done, sandbox)
        .with_item(item);
    if let Some(workspace) = ctx.workspace
        && let Err(reason) = workspace.preflight(&request)
    {
        return Ok(RelandOutcome::Refused(RelandRefusal::Workspace {
            run_id,
            reason,
        }));
    }
    let now = (ctx.now)();
    if let Some(existing) = ctx.claims.load_claim(&task_id)?
        && existing.worker_id != ctx.worker_id
        && existing.lease_expires_at > now
    {
        return Ok(RelandOutcome::Refused(RelandRefusal::ClaimHeld {
            run_id,
            holder: existing.worker_id,
        }));
    }
    let Some(claim) = super::acquire_with_grace(
        ctx.claims.as_ref(),
        &task_id,
        ctx.worker_id,
        now,
        ctx.lease_seconds,
        &BTreeSet::new(),
        ctx.steal_grace_seconds,
    )?
    else {
        let holder = ctx
            .claims
            .load_claim(&task_id)?
            .map(|claim| claim.worker_id)
            .unwrap_or_default();
        return Ok(RelandOutcome::Refused(RelandRefusal::ClaimHeld {
            run_id,
            holder,
        }));
    };
    emit(
        ctx.log,
        format!(
            "[reland] {task_id}: re-landing run {run_id} under worker {}",
            ctx.worker_id
        ),
    );
    let prepare = || -> Result<_, String> {
        if let Some(workspace) = ctx.workspace {
            workspace.materialize(&request)?;
        }
        // With no held-out source the gate never grades a tree; building a
        // scratch candidate would be wasted work.
        if ctx.held_out_source.is_none() {
            return Ok(None);
        }
        ctx.candidate.candidate_tree(&request).map(Some)
    };
    let outcome = match land_once(&ctx.landing_path(), claim, &request, prepare).await? {
        LandingAttempt::Submitted(outcome) => outcome,
        LandingAttempt::LeaseLost => {
            emit(
                ctx.log,
                format!(
                    "[reland] {task_id}: lost the task lease before landing; nothing submitted"
                ),
            );
            return Ok(RelandOutcome::LeaseLost { run_id });
        }
        LandingAttempt::Unprepared(reason) => {
            return Ok(RelandOutcome::Refused(RelandRefusal::Workspace {
                run_id,
                reason,
            }));
        }
    };
    match outcome {
        SubmitOutcome::Landed {
            strategy,
            landed_ref,
        } => {
            record_landing_redrive(
                ctx.control.as_ref(),
                &run_id,
                redrive_record(
                    REDRIVE_RESULT_LANDED,
                    &cleared_park_kind,
                    REDRIVE_INITIATOR_OPERATOR,
                ),
                &task_id,
                ctx.now,
                ctx.log,
            );
            ctx.claims.record_stop_event(
                STOP_RESOLVED,
                &task_id,
                &format!("operator re-land landed run {run_id} as {landed_ref}"),
                (ctx.now)(),
                RESOLUTION_ATTRIBUTION_RELAND,
            )?;
            emit(
                ctx.log,
                format!("[reland] {task_id}: landed run {run_id} as {landed_ref}"),
            );
            Ok(RelandOutcome::Landed {
                run_id,
                strategy,
                landed_ref,
            })
        }
        SubmitOutcome::Parked { park_kind, detail } => {
            record_landing_redrive(
                ctx.control.as_ref(),
                &run_id,
                redrive_record(
                    REDRIVE_RESULT_REPARKED,
                    &park_kind,
                    REDRIVE_INITIATOR_OPERATOR,
                ),
                &task_id,
                ctx.now,
                ctx.log,
            );
            emit(
                ctx.log,
                format!("[reland] {task_id}: re-land parked ({park_kind}): {detail}"),
            );
            Ok(RelandOutcome::Reparked {
                run_id,
                park_kind,
                detail,
            })
        }
        SubmitOutcome::NoOp { reason } => {
            emit(
                ctx.log,
                format!("[reland] {task_id}: re-land was a no-op ({reason}); nothing recorded"),
            );
            Ok(RelandOutcome::NoOp { run_id, reason })
        }
    }
}
