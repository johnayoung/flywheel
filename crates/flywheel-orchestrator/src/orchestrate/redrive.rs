//! The bounded re-drivers (spec 00069): parked landings, exhausted retries,
//! dangling prerequisites, no-progress units, and human gates. Each only
//! reads authoritative lifecycle state, re-attempts through a sanctioned
//! seam, and appends ledger rows; none forges a transition or an envelope.

use crate::claims::{
    ClaimStore, ClaimStoreError, REASON_ABORTED, REASON_AWAITING_APPROVAL, REASON_BUDGET_CEILING,
    REASON_NO_PROGRESS, REASON_PREREQUISITE_MISSING, REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
    STOP_DANGLING_PREREQUISITE, STOP_NO_PROGRESS, STOP_NO_PROGRESS_RESET, STOP_RETRIES_ESCALATED,
};
use crate::held_out_gate::{GateOptions, HeldOutGraderSource, LandingDecision, gate_landing};
use crate::sources::LogSink;
use crate::strategy::{
    LandabilityVerdict, SubmitOutcome, SubmitRequest, SubmitStrategy, panic_message, submit_guarded,
};
use crate::work_graph::GraphValidationIssue;
use flywheel_core::events::{
    DomainEvent, DomainEventPayload, LANDING_PARK_KINDS, LandingParked, LandingRedriven,
    PARK_KIND_HELD_OUT_GATE, REDRIVE_RESULT_LANDED, REDRIVE_RESULT_REPARKED, REDRIVE_RESULT_ROUTED,
};
use flywheel_core::harness::BUDGET_CEILING_ERROR_PREFIX;
use flywheel_core::invoker_client::Clock;
use flywheel_core::lifecycle::{Lifecycle, Outcome, Status};
use flywheel_core::store::HarnessStore;
use flywheel_core::task::Task;
use std::collections::BTreeSet;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;

use super::OrchestrateError;
use super::heartbeat::ClaimHeartbeat;
use super::sweep::emit;

/// How many automatic land re-attempts the landing re-driver makes for one
/// parked run before routing it to the human-review queue.
pub const DEFAULT_LANDING_REDRIVE_BOUND: u32 = 3;
/// How many sanctioned escalations the retry-escalation re-driver spends
/// before routing a task to the human-review queue: exactly one.
pub const DEFAULT_ESCALATION_BOUND: u32 = 1;
/// How many consecutive scheduling cycles a declared prerequisite may stay
/// missing before the referencing task is routed to the queue.
pub const DEFAULT_PREREQ_REDRIVE_BOUND: u32 = 3;
/// How many consecutive cycles a unit may make no observable progress
/// before it is backed off and routed to the queue.
pub const DEFAULT_NO_PROGRESS_BOUND: u32 = 3;

/// Ask the strategy's read-only landability predicate, containing a panic.
/// `None` (no bundled strategy) is always landable.
pub fn probe_landability(
    strategy: Option<&dyn SubmitStrategy>,
    request: &SubmitRequest,
) -> Result<LandabilityVerdict, String> {
    let Some(strategy) = strategy else {
        return Ok(LandabilityVerdict::landable());
    };
    catch_unwind(AssertUnwindSafe(|| strategy.is_landable(request)))
        .map_err(|payload| panic_message(payload.as_ref()))
}

// --- Landing re-drive ------------------------------------------------------------

/// What the landing re-driver did with one parked run in a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedriveResult {
    /// A re-attempt merged the branch; the strand cleared.
    Landed,
    /// The run exhausted its bound (or was already routed) and sits in the
    /// human-review queue; no further attempt is made.
    Queued,
    /// A re-attempt was made but the run neither landed nor hit the bound
    /// (a peer held the claim); left for a later pass.
    InProgress,
    /// No longer a landable parked change; neither re-driven nor queued.
    Skipped,
}

impl RedriveResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            RedriveResult::Landed => "landed",
            RedriveResult::Queued => "queued",
            RedriveResult::InProgress => "in-progress",
            RedriveResult::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedriveOutcome {
    pub run_id: String,
    pub task_id: String,
    pub result: RedriveResult,
    /// Sanctioned land re-attempts this call made for the run.
    pub attempts: u32,
    /// The last observed park cause (empty for `Landed`).
    pub park_kind: String,
}

/// Every `LandingParked` event on `run_id`, in ledger order.
pub fn landing_parks(
    control: &dyn HarnessStore,
    run_id: &str,
) -> Result<Vec<LandingParked>, OrchestrateError> {
    Ok(control
        .list_domain_events(run_id)?
        .into_iter()
        .filter_map(|event| match event.payload {
            DomainEventPayload::LandingParked(parked) => Some(parked),
            _ => None,
        })
        .collect())
}

/// True once this run's parked landing has been routed to the queue.
fn landing_already_queued(
    claims: &dyn ClaimStore,
    task_id: &str,
    run_id: &str,
) -> Result<bool, ClaimStoreError> {
    Ok(claims
        .list_subject_stop_events(task_id)?
        .iter()
        .any(|event| event.run_id == run_id && LANDING_PARK_KINDS.contains(&event.kind.as_str())))
}

/// Append a `LandingRedriven` record pairing one re-drive disposition with
/// the outcome witness it just produced. Best-effort: any failure is logged
/// and swallowed so the re-drive loop never unwinds on a recording failure.
fn record_landing_redrive(
    control: &dyn HarnessStore,
    run_id: &str,
    result: &str,
    park_kind: &str,
    task_id: &str,
    now: &Clock,
    log: Option<&LogSink>,
) {
    let recorded = control.load_lifecycle(run_id).and_then(|lifecycle| {
        let Some(lifecycle) = lifecycle else {
            return Ok(());
        };
        let event = DomainEvent::new(
            run_id,
            now(),
            DomainEventPayload::LandingRedriven(LandingRedriven {
                result: result.to_owned(),
                park_kind: park_kind.to_owned(),
            }),
        );
        control
            .append_domain_event(&event, lifecycle.version)
            .map(|_| ())
    });
    if let Err(err) = recorded {
        emit(
            log,
            format!("[orchestrate] {task_id}: failed to record landing-redrive event ({err})"),
        );
    }
}

/// Inputs shared by every call of [`redrive_parked_landings`].
pub struct LandingRedriveContext<'a> {
    pub control: &'a Arc<dyn HarnessStore>,
    pub claims: &'a Arc<dyn ClaimStore>,
    pub strategy: &'a dyn SubmitStrategy,
    pub worker_id: &'a str,
    pub bound: u32,
    pub held_out_source: Option<&'a dyn HeldOutGraderSource>,
    pub gate_options: &'a GateOptions,
    pub lease_seconds: f64,
    pub heartbeat_interval: f64,
    pub steal_grace_seconds: f64,
    pub now: &'a Clock,
    pub log: Option<&'a LogSink>,
}

/// Bounded re-drive of runs parked unlanded (spec 00069, criteria #3/#4).
///
/// For each request describing a DONE run: skips one that never parked,
/// short-circuits one already routed, confirms it is still a landable
/// parked change via the strategy's read-only probe, then re-attempts the
/// land up to `bound` times under a freshly acquired (heartbeat-renewed,
/// keyless) claim. Each re-attempt first clears a FRESH held-out landing
/// gate against the content it would land (spec 00074); a pass lets
/// `submit` run, a FAIL appends a fresh park. On the bound the run is routed
/// to the human-review queue with its park kind as the reason.
///
/// `never_parked` is a session-scoped cache of DONE runs observed with no
/// park: a landed run can never gain a park later, so re-reading its ledger
/// every pass is wasted store traffic.
pub async fn redrive_parked_landings(
    ctx: &LandingRedriveContext<'_>,
    requests: &[SubmitRequest],
    never_parked: &mut BTreeSet<String>,
) -> Result<Vec<RedriveOutcome>, OrchestrateError> {
    let mut outcomes = Vec::new();
    for request in requests {
        let run_id = request.run_id.clone();
        let task_id = request.task_id.clone();
        if never_parked.contains(&run_id) {
            continue;
        }
        let mut parks = landing_parks(ctx.control.as_ref(), &run_id)?;
        if parks.is_empty() {
            never_parked.insert(run_id);
            continue;
        }
        if landing_already_queued(ctx.claims.as_ref(), &task_id, &run_id)? {
            outcomes.push(RedriveOutcome {
                run_id,
                task_id,
                result: RedriveResult::Queued,
                attempts: 0,
                park_kind: parks
                    .last()
                    .map(|p| p.park_kind.clone())
                    .unwrap_or_default(),
            });
            continue;
        }
        let landable = match probe_landability(Some(ctx.strategy), request) {
            Ok(verdict) => verdict.landable,
            Err(message) => {
                emit(
                    ctx.log,
                    format!(
                        "[orchestrate] {task_id}: landability probe failed during re-drive (panic: {message}); skipping"
                    ),
                );
                false
            }
        };
        if !landable {
            outcomes.push(RedriveOutcome {
                run_id,
                task_id,
                result: RedriveResult::Skipped,
                attempts: 0,
                park_kind: parks
                    .last()
                    .map(|p| p.park_kind.clone())
                    .unwrap_or_default(),
            });
            continue;
        }
        let mut made = parks.len() as u32 - 1;
        let baseline = made;
        let mut attempts = 0u32;
        let mut landed = false;
        let mut no_op = false;
        // The cause of the latest re-attempt's park when its own
        // `LandingParked` write never reached the ledger (a swallowed witness
        // write, a logged-only gate park, a panic converted by
        // `submit_guarded`); `None` while the ledger's last park is current.
        let mut unrecorded: Option<(String, String)> = None;
        let mut unrecorded_count = 0u32;
        while made < ctx.bound {
            let Some(claim) = super::acquire_with_grace(
                ctx.claims.as_ref(),
                &task_id,
                ctx.worker_id,
                (ctx.now)(),
                ctx.lease_seconds,
                &BTreeSet::new(),
                ctx.steal_grace_seconds,
            )?
            else {
                break; // a peer holds the claim; retry on a later pass
            };
            let heartbeat = match ClaimHeartbeat::start(
                ctx.claims.clone(),
                claim.clone(),
                ctx.lease_seconds,
                ctx.heartbeat_interval,
                ctx.now.clone(),
            ) {
                Ok(heartbeat) => heartbeat,
                Err(err) => {
                    ctx.claims.release_claim(&claim, Some((ctx.now)()))?;
                    return Err(err.into());
                }
            };
            let mut log_line = |line: &str| emit(ctx.log, line);
            let decision = gate_landing(
                ctx.control.as_ref(),
                ctx.held_out_source,
                request,
                ctx.gate_options,
                || (ctx.now)(),
                &mut log_line,
            )
            .await;
            let submit_outcome = match &decision {
                LandingDecision::Land { .. } => submit_guarded(ctx.strategy, request),
                LandingDecision::Park { detail, .. } => {
                    SubmitOutcome::parked(PARK_KIND_HELD_OUT_GATE, detail.clone())
                }
            };
            let latest = heartbeat.stop();
            ctx.claims.release_claim(&latest, Some((ctx.now)()))?;
            attempts += 1;
            match submit_outcome {
                SubmitOutcome::Landed { .. } => {
                    landed = true;
                    record_landing_redrive(
                        ctx.control.as_ref(),
                        &run_id,
                        REDRIVE_RESULT_LANDED,
                        &parks
                            .last()
                            .map(|p| p.park_kind.clone())
                            .unwrap_or_default(),
                        &task_id,
                        ctx.now,
                        ctx.log,
                    );
                    break;
                }
                SubmitOutcome::NoOp { reason } => {
                    // Nothing landed and nothing parked: there is no witness
                    // to pair a redrive record with, so record nothing and
                    // leave the run for the probe to classify next pass.
                    emit(
                        ctx.log,
                        format!(
                            "[orchestrate] {task_id}: landing re-drive was a no-op ({reason}); nothing recorded"
                        ),
                    );
                    no_op = true;
                    break;
                }
                SubmitOutcome::Parked { park_kind, detail } => {
                    let recorded_before = parks.len();
                    parks = landing_parks(ctx.control.as_ref(), &run_id)?;
                    if parks.len() > recorded_before {
                        unrecorded = None;
                    } else {
                        emit(
                            ctx.log,
                            format!(
                                "[orchestrate] {task_id}: landing re-attempt parked ({park_kind}) but no landing-parked event was recorded; counting it against the re-drive bound"
                            ),
                        );
                        unrecorded_count += 1;
                        unrecorded = Some((park_kind, detail));
                    }
                    // Every re-attempt spends the bound whether or not its
                    // park reached the ledger: deriving `made` from the
                    // ledger alone would never advance past an unrecorded
                    // park and the loop would spin without sleeping.
                    made = (parks.len() as u32 - 1).max(baseline + attempts);
                    let kind = match &unrecorded {
                        Some((kind, _)) => kind.clone(),
                        None => parks
                            .last()
                            .map(|p| p.park_kind.clone())
                            .unwrap_or_default(),
                    };
                    record_landing_redrive(
                        ctx.control.as_ref(),
                        &run_id,
                        REDRIVE_RESULT_REPARKED,
                        &kind,
                        &task_id,
                        ctx.now,
                        ctx.log,
                    );
                }
            }
        }
        if landed {
            emit(
                ctx.log,
                format!(
                    "[orchestrate] {task_id}: re-drove parked landing to a merge after {attempts} re-attempt(s)"
                ),
            );
            outcomes.push(RedriveOutcome {
                run_id,
                task_id,
                result: RedriveResult::Landed,
                attempts,
                park_kind: String::new(),
            });
            continue;
        }
        let (park_kind, last_detail) = match unrecorded {
            Some(cause) => cause,
            None => parks
                .last()
                .map(|p| (p.park_kind.clone(), p.detail.clone()))
                .unwrap_or_default(),
        };
        if no_op {
            outcomes.push(RedriveOutcome {
                run_id,
                task_id,
                result: RedriveResult::Skipped,
                attempts,
                park_kind,
            });
            continue;
        }
        if made >= ctx.bound {
            let mut detail = format!(
                "landing re-drive exhausted {} re-attempt(s) without landing; last park cause {:?}: {}",
                ctx.bound, park_kind, last_detail
            );
            if unrecorded_count > 0 {
                detail = format!(
                    "{detail} ({unrecorded_count} re-attempt(s) parked without a recorded landing-parked event)"
                );
            }
            ctx.claims
                .record_human_review(&park_kind, &task_id, (ctx.now)(), &run_id, &detail)?;
            record_landing_redrive(
                ctx.control.as_ref(),
                &run_id,
                REDRIVE_RESULT_ROUTED,
                &park_kind,
                &task_id,
                ctx.now,
                ctx.log,
            );
            emit(
                ctx.log,
                format!(
                    "[orchestrate] {task_id}: parked landing routed to the human-review queue after {attempts} re-attempt(s) (reason {park_kind:?})"
                ),
            );
            outcomes.push(RedriveOutcome {
                run_id,
                task_id,
                result: RedriveResult::Queued,
                attempts,
                park_kind,
            });
            continue;
        }
        outcomes.push(RedriveOutcome {
            run_id,
            task_id,
            result: RedriveResult::InProgress,
            attempts,
            park_kind,
        });
    }
    Ok(outcomes)
}

// --- Retry escalation ---------------------------------------------------------------

/// The outcomes that mark a terminal FAILED lifecycle as reached by *retry
/// exhaustion* rather than an abort.
const RETRY_EXHAUSTION_OUTCOMES: [Outcome; 2] = [Outcome::ValidationFailed, Outcome::InternalError];

/// A candidate the retry-escalation re-driver may escalate.
#[derive(Debug, Clone, PartialEq)]
pub struct EscalationRequest {
    pub task_id: String,
    pub task: Task,
    pub run_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationResult {
    Escalated,
    Queued,
    InProgress,
}

impl EscalationResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            EscalationResult::Escalated => "escalated",
            EscalationResult::Queued => "queued",
            EscalationResult::InProgress => "in-progress",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationOutcome {
    pub run_id: String,
    pub task_id: String,
    pub result: EscalationResult,
    /// Sanctioned escalations spent on this task after the call.
    pub escalations: u32,
    /// The run minted by the escalation (empty unless `Escalated`).
    pub escalated_run_id: String,
}

/// True when `lifecycle` reached a terminal FAILED by retry exhaustion.
pub fn is_retry_exhausted(lifecycle: &Lifecycle, max_retries: u32) -> bool {
    if lifecycle.status != Status::Failed || lifecycle.retries < max_retries {
        return false;
    }
    lifecycle
        .attempts
        .last()
        .and_then(|a| a.outcome)
        .is_some_and(|outcome| RETRY_EXHAUSTION_OUTCOMES.contains(&outcome))
}

fn escalation_count(claims: &dyn ClaimStore, task_id: &str) -> Result<u32, ClaimStoreError> {
    Ok(claims
        .list_subject_stop_events(task_id)?
        .iter()
        .filter(|event| event.kind == STOP_RETRIES_ESCALATED)
        .count() as u32)
}

fn already_queued_after_escalation(
    claims: &dyn ClaimStore,
    task_id: &str,
) -> Result<bool, ClaimStoreError> {
    Ok(claims
        .list_subject_stop_events(task_id)?
        .iter()
        .any(|event| event.kind == REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION))
}

/// The sanctioned re-drive seam: given the candidate and the escalation
/// model, mint and drive a fresh run, returning its run id (`None` when it
/// could not run this pass).
pub type EscalationDriver = Arc<
    dyn Fn(
            EscalationRequest,
            Option<String>,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

/// Inputs shared by every call of [`redrive_exhausted_retries`].
pub struct EscalationContext<'a> {
    pub control: &'a dyn HarnessStore,
    pub claims: &'a dyn ClaimStore,
    pub worker_id: &'a str,
    pub drive: &'a EscalationDriver,
    pub escalation_model: Option<String>,
    pub max_retries: u32,
    pub bound: u32,
    pub lease_seconds: f64,
    pub steal_grace_seconds: f64,
    pub now: &'a Clock,
    pub log: Option<&'a LogSink>,
}

/// Bounded escalate-once-then-queue re-drive of exhausted retries (spec
/// 00069, criteria #5/#6; D-A/D-C). On the FIRST exhaustion escalates
/// exactly once through `drive` under a freshly acquired keyless claim and
/// records a `retries-escalated` marker; on the NEXT exhaustion routes the
/// task to the human-review queue with
/// `retries-exhausted-after-escalation` and never re-escalates.
pub async fn redrive_exhausted_retries(
    ctx: &EscalationContext<'_>,
    requests: &[EscalationRequest],
) -> Result<Vec<EscalationOutcome>, OrchestrateError> {
    let mut outcomes = Vec::new();
    for request in requests {
        let run_id = request.run_id.clone();
        let task_id = request.task_id.clone();
        let Some(lifecycle) = ctx.control.load_lifecycle(&run_id)? else {
            continue;
        };
        if !is_retry_exhausted(&lifecycle, ctx.max_retries) {
            continue;
        }
        if already_queued_after_escalation(ctx.claims, &task_id)? {
            outcomes.push(EscalationOutcome {
                run_id,
                task_id: task_id.clone(),
                result: EscalationResult::Queued,
                escalations: escalation_count(ctx.claims, &task_id)?,
                escalated_run_id: String::new(),
            });
            continue;
        }
        let escalations = escalation_count(ctx.claims, &task_id)?;
        if escalations >= ctx.bound {
            let detail = format!(
                "retry budget exhausted again on run {run_id:?} after the single sanctioned escalation (retries={}, max_retries={})",
                lifecycle.retries, ctx.max_retries
            );
            ctx.claims.record_human_review(
                REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
                &task_id,
                (ctx.now)(),
                &run_id,
                &detail,
            )?;
            emit(
                ctx.log,
                format!(
                    "[orchestrate] {task_id}: retries exhausted after escalation; routed to the human-review queue (reason {REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION:?})"
                ),
            );
            outcomes.push(EscalationOutcome {
                run_id,
                task_id,
                result: EscalationResult::Queued,
                escalations,
                escalated_run_id: String::new(),
            });
            continue;
        }
        let Some(claim) = super::acquire_with_grace(
            ctx.claims,
            &task_id,
            ctx.worker_id,
            (ctx.now)(),
            ctx.lease_seconds,
            &BTreeSet::new(),
            ctx.steal_grace_seconds,
        )?
        else {
            outcomes.push(EscalationOutcome {
                run_id,
                task_id,
                result: EscalationResult::InProgress,
                escalations,
                escalated_run_id: String::new(),
            });
            continue;
        };
        let escalated_run_id = (ctx.drive)(request.clone(), ctx.escalation_model.clone()).await;
        ctx.claims.release_claim(&claim, Some((ctx.now)()))?;
        let Some(escalated_run_id) = escalated_run_id.filter(|id| !id.is_empty()) else {
            outcomes.push(EscalationOutcome {
                run_id,
                task_id,
                result: EscalationResult::InProgress,
                escalations,
                escalated_run_id: String::new(),
            });
            continue;
        };
        let detail = format!(
            "escalated once after retry-budget exhaustion on run {run_id:?}; escalated run {escalated_run_id:?}"
        );
        ctx.claims
            .record_stop_event(STOP_RETRIES_ESCALATED, &task_id, &detail, (ctx.now)(), "")?;
        emit(
            ctx.log,
            format!(
                "[orchestrate] {task_id}: retries exhausted; escalated once to run {escalated_run_id:?}"
            ),
        );
        outcomes.push(EscalationOutcome {
            run_id,
            task_id,
            result: EscalationResult::Escalated,
            escalations: escalations + 1,
            escalated_run_id,
        });
    }
    Ok(outcomes)
}

// --- Dangling prerequisites ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrereqRedriveResult {
    Waiting,
    Queued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrereqRedriveOutcome {
    pub referencing_id: String,
    pub missing_id: String,
    pub result: PrereqRedriveResult,
    /// Consecutive cycles the prerequisite has been observed missing.
    pub cycles: u32,
}

/// The `repr(missing_id)` needle the witness detail carries so two distinct
/// dangling prerequisites on one task bound independently.
fn needle(missing_id: &str) -> String {
    format!("{missing_id:?}")
}

fn prereq_dangling_cycles(
    claims: &dyn ClaimStore,
    referencing_id: &str,
    missing_id: &str,
) -> Result<u32, ClaimStoreError> {
    let needle = needle(missing_id);
    Ok(claims
        .list_subject_stop_events(referencing_id)?
        .iter()
        .filter(|event| event.kind == STOP_DANGLING_PREREQUISITE && event.detail.contains(&needle))
        .count() as u32)
}

fn prereq_already_queued(
    claims: &dyn ClaimStore,
    referencing_id: &str,
    missing_id: &str,
) -> Result<bool, ClaimStoreError> {
    let needle = needle(missing_id);
    Ok(claims
        .list_subject_stop_events(referencing_id)?
        .iter()
        .any(|event| event.kind == REASON_PREREQUISITE_MISSING && event.detail.contains(&needle)))
}

/// Bounded re-drive of tasks blocked on a missing prerequisite (spec 00069,
/// criteria #7/#8/#13). Appends one `dangling-prerequisite` witness per
/// pass the edge stays missing and, on the bound, routes the referencing
/// task ONCE to the queue with `prerequisite-missing` naming the missing id.
/// Never claims or transitions anything.
pub fn redrive_missing_prerequisites(
    claims: &dyn ClaimStore,
    issues: &[GraphValidationIssue],
    bound: u32,
    now: &Clock,
    log: Option<&LogSink>,
) -> Result<Vec<PrereqRedriveOutcome>, OrchestrateError> {
    let mut outcomes = Vec::new();
    for issue in issues {
        let referencing_id = issue.referencing_id.clone();
        let missing_id = issue.missing_id.clone();
        if prereq_already_queued(claims, &referencing_id, &missing_id)? {
            outcomes.push(PrereqRedriveOutcome {
                cycles: prereq_dangling_cycles(claims, &referencing_id, &missing_id)?,
                referencing_id,
                missing_id,
                result: PrereqRedriveResult::Queued,
            });
            continue;
        }
        claims.record_stop_event(
            STOP_DANGLING_PREREQUISITE,
            &referencing_id,
            &format!(
                "prerequisite {missing_id:?} resolves to no work item; {referencing_id:?} stays out of the ready set"
            ),
            now(),
            "",
        )?;
        let cycles = prereq_dangling_cycles(claims, &referencing_id, &missing_id)?;
        if cycles >= bound {
            let detail = format!(
                "prerequisite {missing_id:?} stayed missing for {cycles} cycle(s) (bound {bound}); {referencing_id:?} was never dispatched against an unsatisfied prerequisite"
            );
            claims.record_human_review(
                REASON_PREREQUISITE_MISSING,
                &referencing_id,
                now(),
                "",
                &detail,
            )?;
            emit(
                log,
                format!(
                    "[orchestrate] {referencing_id}: prerequisite {missing_id:?} missing past {bound} cycle(s); routed to the human-review queue (reason {REASON_PREREQUISITE_MISSING:?})"
                ),
            );
            outcomes.push(PrereqRedriveOutcome {
                referencing_id,
                missing_id,
                result: PrereqRedriveResult::Queued,
                cycles,
            });
            continue;
        }
        outcomes.push(PrereqRedriveOutcome {
            referencing_id,
            missing_id,
            result: PrereqRedriveResult::Waiting,
            cycles,
        });
    }
    Ok(outcomes)
}

// --- No-progress back-off ----------------------------------------------------------

/// One unit's observed progress for a single scheduling cycle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoProgressObservation {
    pub unit_id: String,
    pub progressed: bool,
    pub detail: String,
    /// The review subject the queue entry carries; defaults to `unit_id`.
    pub task_id: String,
}

impl NoProgressObservation {
    pub fn new(unit_id: impl Into<String>, progressed: bool) -> Self {
        Self {
            unit_id: unit_id.into(),
            progressed,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoProgressResult {
    Progressed,
    Waiting,
    Queued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoProgressOutcome {
    pub unit_id: String,
    pub result: NoProgressResult,
    /// Length of the unit's current consecutive no-progress streak.
    pub cycles: u32,
}

/// Length of `unit_id`'s current consecutive no-progress streak: the
/// `no-progress-cycle` witnesses after its most recent `no-progress-reset`.
pub fn no_progress_streak(claims: &dyn ClaimStore, unit_id: &str) -> Result<u32, ClaimStoreError> {
    let mut streak = 0u32;
    for event in claims.list_subject_stop_events(unit_id)? {
        if event.kind == STOP_NO_PROGRESS_RESET {
            streak = 0;
        } else if event.kind == STOP_NO_PROGRESS {
            streak += 1;
        }
    }
    Ok(streak)
}

fn no_progress_already_queued(
    claims: &dyn ClaimStore,
    unit_id: &str,
) -> Result<bool, ClaimStoreError> {
    Ok(claims
        .list_subject_stop_events(unit_id)?
        .iter()
        .any(|event| event.kind == REASON_NO_PROGRESS))
}

/// Bounded no-progress back-off of units the loop cannot advance (spec
/// 00069, criteria #9/#13; D-C). Progress appends a reset marker (only when
/// a streak is outstanding); a fruitless cycle appends one witness and, on
/// the bound, routes the unit ONCE with `no-progress`.
pub fn redrive_no_progress(
    claims: &dyn ClaimStore,
    observations: &[NoProgressObservation],
    bound: u32,
    now: &Clock,
    log: Option<&LogSink>,
) -> Result<Vec<NoProgressOutcome>, OrchestrateError> {
    let mut outcomes = Vec::new();
    for observation in observations {
        let unit_id = observation.unit_id.clone();
        if no_progress_already_queued(claims, &unit_id)? {
            outcomes.push(NoProgressOutcome {
                cycles: no_progress_streak(claims, &unit_id)?,
                unit_id,
                result: NoProgressResult::Queued,
            });
            continue;
        }
        if observation.progressed {
            if no_progress_streak(claims, &unit_id)? > 0 {
                let detail = if observation.detail.is_empty() {
                    format!("unit {unit_id:?} made progress; streak reset")
                } else {
                    observation.detail.clone()
                };
                claims.record_stop_event(STOP_NO_PROGRESS_RESET, &unit_id, &detail, now(), "")?;
            }
            outcomes.push(NoProgressOutcome {
                unit_id,
                result: NoProgressResult::Progressed,
                cycles: 0,
            });
            continue;
        }
        let witness = if observation.detail.is_empty() {
            format!("unit {unit_id:?} made no observable progress this cycle")
        } else {
            observation.detail.clone()
        };
        claims.record_stop_event(STOP_NO_PROGRESS, &unit_id, &witness, now(), "")?;
        let cycles = no_progress_streak(claims, &unit_id)?;
        if cycles >= bound {
            let mut detail = format!(
                "unit {unit_id:?} made no observable progress for {cycles} consecutive cycle(s) (bound {bound}); backed off and routed to the human-review queue"
            );
            if !observation.detail.is_empty() {
                detail = format!("{detail}: {}", observation.detail);
            }
            let subject = if observation.task_id.is_empty() {
                unit_id.as_str()
            } else {
                observation.task_id.as_str()
            };
            claims.record_human_review(REASON_NO_PROGRESS, subject, now(), "", &detail)?;
            emit(
                log,
                format!(
                    "[orchestrate] {unit_id}: no progress for {bound} consecutive cycle(s); backed off and routed to the human-review queue (reason {REASON_NO_PROGRESS:?})"
                ),
            );
            outcomes.push(NoProgressOutcome {
                unit_id,
                result: NoProgressResult::Queued,
                cycles,
            });
            continue;
        }
        outcomes.push(NoProgressOutcome {
            unit_id,
            result: NoProgressResult::Waiting,
            cycles,
        });
    }
    Ok(outcomes)
}

// --- Human gates ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanGateRequest {
    pub task_id: String,
    pub run_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HumanGateResult {
    Queued,
    AlreadyQueued,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanGateOutcome {
    pub run_id: String,
    pub task_id: String,
    pub result: HumanGateResult,
    /// The machine-readable stop cause (empty for `Skipped`).
    pub reason: String,
}

/// The human-review reason for `lifecycle`'s intentional stop, or `None`:
/// `AWAITING_APPROVAL` -> `awaiting-approval`; a terminal FAILED whose final
/// attempt is `AGENT_ERROR` -> `budget-ceiling` when its error carries the
/// budget-ceiling prefix, else `abort`. Every other state (live, DONE, a
/// retry-exhausted FAILED) is not a human gate.
pub fn classify_human_gate(lifecycle: &Lifecycle) -> Option<&'static str> {
    if lifecycle.status == Status::AwaitingApproval {
        return Some(REASON_AWAITING_APPROVAL);
    }
    if lifecycle.status == Status::Failed
        && let Some(tail) = lifecycle.attempts.last()
        && tail.outcome == Some(Outcome::AgentError)
    {
        if tail.error.starts_with(BUDGET_CEILING_ERROR_PREFIX) {
            return Some(REASON_BUDGET_CEILING);
        }
        return Some(REASON_ABORTED);
    }
    None
}

fn human_gate_detail(lifecycle: &Lifecycle, task_id: &str, run_id: &str, reason: &str) -> String {
    if reason == REASON_AWAITING_APPROVAL {
        return format!(
            "human gate: {task_id:?} run {run_id:?} is awaiting manual approval; left parked for a human (status unchanged)"
        );
    }
    let error = lifecycle
        .attempts
        .last()
        .map(|a| a.error.clone())
        .unwrap_or_default();
    if reason == REASON_BUDGET_CEILING {
        return format!(
            "run {run_id:?} breached a per-run budget ceiling ({error}); routed for a human, never re-dispatched"
        );
    }
    format!(
        "run {run_id:?} ended via intent=abort ({error}); routed for a human, never re-dispatched"
    )
}

fn human_gate_already_queued(
    claims: &dyn ClaimStore,
    task_id: &str,
    run_id: &str,
    reason: &str,
) -> Result<bool, ClaimStoreError> {
    Ok(claims
        .list_subject_stop_events(task_id)?
        .iter()
        .any(|event| event.run_id == run_id && event.kind == reason))
}

/// Surface intentional human stops into the queue, never bypass them (spec
/// 00069, criteria #10/#11; D-E). Records ONE entry per run naming the
/// cause; never transitions a lifecycle. `routed` is a session-scoped cache
/// of `(task_id, run_id, reason)` triples already surfaced, so an unresolved
/// gate costs one ledger read per session rather than one per pass.
pub fn redrive_human_gates(
    control: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    requests: &[HumanGateRequest],
    now: &Clock,
    log: Option<&LogSink>,
    routed: &mut BTreeSet<(String, String, String)>,
) -> Result<Vec<HumanGateOutcome>, OrchestrateError> {
    let mut outcomes = Vec::new();
    for request in requests {
        let run_id = request.run_id.clone();
        let task_id = request.task_id.clone();
        let Some(lifecycle) = control.load_lifecycle(&run_id)? else {
            continue;
        };
        let Some(reason) = classify_human_gate(&lifecycle) else {
            outcomes.push(HumanGateOutcome {
                run_id,
                task_id,
                result: HumanGateResult::Skipped,
                reason: String::new(),
            });
            continue;
        };
        let key = (task_id.clone(), run_id.clone(), reason.to_owned());
        if routed.contains(&key) || human_gate_already_queued(claims, &task_id, &run_id, reason)? {
            routed.insert(key);
            outcomes.push(HumanGateOutcome {
                run_id,
                task_id,
                result: HumanGateResult::AlreadyQueued,
                reason: reason.to_owned(),
            });
            continue;
        }
        claims.record_human_review(
            reason,
            &task_id,
            now(),
            &run_id,
            &human_gate_detail(&lifecycle, &task_id, &run_id, reason),
        )?;
        routed.insert(key);
        emit(
            log,
            format!(
                "[orchestrate] {task_id}: intentional stop surfaced to the human-review queue (reason {reason:?}); status left unchanged"
            ),
        );
        outcomes.push(HumanGateOutcome {
            run_id,
            task_id,
            result: HumanGateResult::Queued,
            reason: reason.to_owned(),
        });
    }
    Ok(outcomes)
}
