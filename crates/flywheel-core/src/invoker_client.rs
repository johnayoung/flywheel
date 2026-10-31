//! The in-run control plane: operator steering applied to a live agent.
//!
//! A CLI producer enqueues a control command into the store's
//! `control_commands` channel; the [`ControlCommandWatcher`] running inside
//! the worker claims it on its next tick (claim-once, enqueue-ordered) and
//! dispatches it to the live run's [`AgentControl`] (`interrupt` /
//! `set_model` / `say`). Each successful application emits a
//! `harness.control_command_applied` audit event and hands the
//! [`CommandApplied`] ledger payload to the caller; a failed dispatch emits
//! `harness.control_command_failed` and is best-effort -- the run continues
//! so a faulty command can never abort the iteration.
//!
//! A store-triggered `interrupt` mirrors a SIGINT/SIGTERM cancellation: after
//! the agent acknowledges the interrupt the watcher cancels the run, and the
//! agents-backed invoker surfaces the stop as an [`InvocationFailure`] whose
//! `error_type` is [`FAILURE_OPERATOR_INTERRUPT`] so the harness routes the
//! lifecycle through its interrupt handling exactly the way operator-driven
//! shutdown does. A harness-initiated mid-turn recovery ([`RecoverySignal`])
//! surfaces as [`FAILURE_HARNESS_RECOVERY`] instead, keeping the three
//! cancellation channels (operator interrupt, recovery, external cancel)
//! distinguishable.
//!
//! The watcher also hosts the two other mid-turn seams: the context-usage
//! observer (one reading per poll from the agent's own context accounting)
//! and the checkpoint nudge (a single harness-initiated "commit your work
//! now" injection when the iteration nears its wall-clock ceiling without
//! new progress).
//!
//! In the Python implementation this module wrapped the vendor SDK client;
//! the Rust runtime has no SDK and every agent runs through
//! `flywheel-agents`, so the client wrapper collapses onto [`AgentControl`]
//! and the per-iteration drive lives in [`crate::agents_invoke`].
//!
//! [`InvocationFailure`]: crate::invoker::InvocationFailure

use crate::events::CommandApplied;
use crate::lifecycle::{Lifecycle, Status};
use crate::store::{
    ControlCommandRecord, ControlCommandStore, JsonObject, LifecycleStore, StoreError,
};
use chrono::{DateTime, Utc};
use flywheel_agents::runtime::CancelHandle;
use flywheel_agents::{AgentControl, AgentError};
use serde_json::{Value, json};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// One-shot re-prompt sent on the SAME session when an iteration ends
/// cleanly (`end_turn`) but its closing `LOOP_STATUS` envelope was missing
/// or truncated -- the fingerprint of an auto-compaction dropping the
/// protocol instruction on a context-heavy iteration. Asking only for the
/// envelope recovers the completed work instead of discarding the whole
/// attempt. Bounded to a single attempt; only a VALID re-emitted envelope is
/// adopted.
pub const ENVELOPE_SALVAGE_PROMPT: &str = "Your previous response ended without the required LOOP_STATUS iteration \
envelope. Do not take any further action or make any further edits. \
Emit exactly one iteration envelope now -- the fenced `<!-- LOOP_STATUS \
-->` block defined in your instructions -- describing the state you have \
already reached, and output nothing else.";

/// One-shot instruction injected on the live session when the iteration's
/// remaining wall time to the resolved `AGENT_ITERATION` ceiling drops to
/// the configured threshold AND the progress probe reports no new committed
/// progress since the iteration began. Harness-initiated: it fabricates no
/// operator control-command row and does not move the deadline.
pub const CHECKPOINT_NUDGE_PROMPT: &str = "You are close to this iteration's wall-clock time limit and no new commit \
has landed yet. Stop what you are doing and commit your work-in-progress \
NOW as one or more incremental checkpoint commits on the current task \
branch, so no progress is lost if the iteration is cut off. Commit \
whatever is on disk even if it is incomplete; you may keep working \
afterward if time still allows.";

/// Default poll tick when the store has no pending row: short enough that an
/// operator's command lands with a bounded delay, long enough that idle
/// iterations do not hammer the store.
pub const DEFAULT_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Control-command verbs. `interrupt` / `set_model` / `say` each map to one
/// dispatch on the live agent; `approve` / `reject` are out-of-band verbs
/// resolving a parked `awaiting_approval` lifecycle and are left for that
/// resolver (recorded as not-applicable by the live watcher).
pub const CONTROL_COMMAND_INTERRUPT: &str = "interrupt";
pub const CONTROL_COMMAND_SET_MODEL: &str = "set_model";
pub const CONTROL_COMMAND_SAY: &str = "say";
pub const CONTROL_COMMAND_APPROVE: &str = "approve";
pub const CONTROL_COMMAND_REJECT: &str = "reject";

/// Audit-event kinds emitted by the watcher.
pub const EVENT_CONTROL_APPLIED: &str = "harness.control_command_applied";
pub const EVENT_CONTROL_FAILED: &str = "harness.control_command_failed";
pub const EVENT_CONTROL_CLAIM_FAILED: &str = "harness.control_command_claim_failed";
pub const EVENT_CONTROL_NOT_APPLICABLE: &str = "harness.control_command_not_applicable";
/// Emitted the single time the checkpoint nudge fires on an in-flight
/// invocation; distinct from the control-command kinds because the nudge is
/// not an operator command row.
pub const EVENT_CHECKPOINT_NUDGE: &str = "harness.checkpoint_nudge";

/// `InvocationFailure::error_type` for a run stopped by an operator
/// `interrupt` control command (the store-triggered twin of SIGINT/SIGTERM).
pub const FAILURE_OPERATOR_INTERRUPT: &str = "operator_interrupt";
/// `InvocationFailure::error_type` for a run stopped by a harness-initiated
/// mid-turn recovery request (the Python `HarnessRecoveryRequested`).
pub const FAILURE_HARNESS_RECOVERY: &str = "harness_recovery_requested";

/// Audit emitter: `(event kind, payload)`. Best-effort; a panicking emitter
/// is contained and never breaks the drain.
pub type AuditEmit = Arc<dyn Fn(&str, &JsonObject) + Send + Sync>;
/// Injectable wall clock for enqueue/claim timestamps and the nudge.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
/// Receives the live agent's context-usage reading once per watcher poll.
pub type ContextUsageObserver = Arc<dyn Fn(&Value) + Send + Sync>;
/// Git-free progress probe: an opaque token; two equal tokens mean "no new
/// progress". `None` means the reading failed (the nudge is skipped or
/// disarmed, never fired on absent information).
pub type ProgressProbe = Arc<dyn Fn() -> Option<Value> + Send + Sync>;
/// Steering-ledger seam: invoked once per successfully dispatched command
/// with the claimed row and the [`CommandApplied`] fact to append.
pub type CommandAppliedObserver = Arc<dyn Fn(&ControlCommandRecord, &CommandApplied) + Send + Sync>;

/// The harness's mid-turn recovery request: set it and the watcher
/// interrupts the live agent, cancels the run, and the invoker surfaces
/// [`FAILURE_HARNESS_RECOVERY`].
#[derive(Debug, Clone, Default)]
pub struct RecoverySignal(Arc<AtomicBool>);

impl RecoverySignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a mid-turn recovery. Idempotent.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn utc_now_clock() -> Clock {
    Arc::new(Utc::now)
}

/// Run `f`, containing a panic. The control-plane callbacks are best-effort
/// by contract: a faulty observer must never abort the run or mask the
/// dispatch outcome.
fn guarded(f: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(f));
}

// --- Payload coercion ---------------------------------------------------------

/// A control command whose payload cannot be relayed to the agent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlCommandError {
    #[error("control command 'say' payload requires non-empty 'text' (str)")]
    SayText,
    #[error("control command 'set_model' payload 'model' must be str or null")]
    SetModelModel,
    #[error("control command 'reject' payload 'feedback' must be str if provided")]
    RejectFeedback,
    #[error("unknown control command kind: {0:?}")]
    UnknownKind(String),
}

impl ControlCommandError {
    /// Stable audit code.
    pub const fn error_type(&self) -> &'static str {
        match self {
            ControlCommandError::UnknownKind(_) => "unknown_command_kind",
            _ => "invalid_payload",
        }
    }
}

/// Coerce a `say` payload to its operator-message text.
pub fn payload_text(payload: &JsonObject) -> Result<&str, ControlCommandError> {
    match payload.get("text").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => Ok(text),
        _ => Err(ControlCommandError::SayText),
    }
}

/// Coerce a `set_model` payload's `model` field. `None` is permitted (the
/// agent restores its default model); any other non-string value is
/// rejected before the dispatch.
pub fn payload_model(payload: &JsonObject) -> Result<Option<&str>, ControlCommandError> {
    match payload.get("model") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(model)) => Ok(Some(model.as_str())),
        Some(_) => Err(ControlCommandError::SetModelModel),
    }
}

/// Coerce a `reject` payload's optional `feedback` field (absent means no
/// feedback was provided).
pub fn payload_feedback(payload: &JsonObject) -> Result<Option<&str>, ControlCommandError> {
    match payload.get("feedback") {
        None => Ok(None),
        Some(Value::String(feedback)) => Ok(Some(feedback.as_str())),
        Some(_) => Err(ControlCommandError::RejectFeedback),
    }
}

/// How one claimed command was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// The verb reached the live agent.
    Applied,
    /// The verb has no live-session semantics (`approve` / `reject`); the
    /// row is left for the out-of-band resolver.
    NotApplicable,
}

/// Why a dispatch failed: a payload the watcher refused to relay, or the
/// agent rejecting / not answering the control request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Payload(#[from] ControlCommandError),
    #[error(transparent)]
    Agent(#[from] AgentError),
}

impl ApplyError {
    pub const fn error_type(&self) -> &'static str {
        match self {
            ApplyError::Payload(err) => err.error_type(),
            ApplyError::Agent(err) => err.code(),
        }
    }
}

/// Dispatch one claimed control command against the live agent.
pub async fn apply_command(
    control: &dyn AgentControl,
    command: &ControlCommandRecord,
) -> Result<Dispatch, ApplyError> {
    match command.kind.as_str() {
        CONTROL_COMMAND_INTERRUPT => {
            control.interrupt().await?;
            Ok(Dispatch::Applied)
        }
        CONTROL_COMMAND_SET_MODEL => {
            let model = payload_model(&command.payload)?.map(str::to_owned);
            control.set_model(model).await?;
            Ok(Dispatch::Applied)
        }
        CONTROL_COMMAND_SAY => {
            let text = payload_text(&command.payload)?.to_owned();
            control.inject_message(text).await?;
            Ok(Dispatch::Applied)
        }
        CONTROL_COMMAND_APPROVE => Ok(Dispatch::NotApplicable),
        CONTROL_COMMAND_REJECT => {
            // Validate eagerly so a malformed enqueue lands as a failed event
            // in the live watcher's audit stream rather than reaching the
            // resolver as a poisoned row.
            payload_feedback(&command.payload)?;
            Ok(Dispatch::NotApplicable)
        }
        other => Err(ControlCommandError::UnknownKind(other.to_owned()).into()),
    }
}

/// The ledger fact for an applied command: verb, operator-supplied fields,
/// and queue-row provenance.
pub fn command_applied_event(command: &ControlCommandRecord) -> CommandApplied {
    CommandApplied {
        command_kind: command.kind.clone(),
        command_payload: command.payload.clone(),
        command_id: command.id,
    }
}

// --- Enqueue helpers -----------------------------------------------------------

/// Enqueue an operator `say` (inject one user turn into the live session).
pub fn enqueue_say<S: ControlCommandStore + ?Sized>(
    store: &S,
    run_id: &str,
    text: &str,
    now: DateTime<Utc>,
) -> Result<ControlCommandRecord, StoreError> {
    let mut payload = JsonObject::new();
    payload.insert("text".into(), json!(text));
    store.enqueue_command(run_id, CONTROL_COMMAND_SAY, payload, now)
}

/// Enqueue an operator `interrupt`.
pub fn enqueue_interrupt<S: ControlCommandStore + ?Sized>(
    store: &S,
    run_id: &str,
    now: DateTime<Utc>,
) -> Result<ControlCommandRecord, StoreError> {
    store.enqueue_command(run_id, CONTROL_COMMAND_INTERRUPT, JsonObject::new(), now)
}

/// Enqueue a `set_model`; `None` asks the agent to restore its default.
pub fn enqueue_set_model<S: ControlCommandStore + ?Sized>(
    store: &S,
    run_id: &str,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Result<ControlCommandRecord, StoreError> {
    let mut payload = JsonObject::new();
    payload.insert("model".into(), json!(model));
    store.enqueue_command(run_id, CONTROL_COMMAND_SET_MODEL, payload, now)
}

/// Payload key binding an `approve` / `reject` to the manual gate it was
/// issued against: the lifecycle's `awaiting_manual_ordinal` at enqueue time.
pub const GATE_BINDING_ORDINAL: &str = "awaiting_ordinal";
/// Payload key binding an `approve` / `reject` to the attempt whose park it
/// was issued against, so a decision on one attempt's gate never resolves a
/// later attempt's re-park of the same gate.
pub const GATE_BINDING_ATTEMPT: &str = "attempt_number";

/// One parked manual-gate instance: which gate, on which attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateBinding {
    pub awaiting_ordinal: u32,
    pub attempt_number: u32,
}

impl GateBinding {
    /// The gate instance `lifecycle` is parked on, or `None` when it is not
    /// parked at `awaiting_approval` with a pending gate and an attempt.
    #[must_use]
    pub fn of_parked(lifecycle: &Lifecycle) -> Option<Self> {
        if lifecycle.status != Status::AwaitingApproval {
            return None;
        }
        Some(Self {
            awaiting_ordinal: lifecycle.awaiting_manual_ordinal?,
            attempt_number: lifecycle.attempts.iter().map(|a| a.number).max()?,
        })
    }

    /// The binding a queued command carries, or `None` when it carries none
    /// (or a malformed one).
    #[must_use]
    pub fn of_command(command: &ControlCommandRecord) -> Option<Self> {
        let field = |key: &str| {
            command
                .payload
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        Some(Self {
            awaiting_ordinal: field(GATE_BINDING_ORDINAL)?,
            attempt_number: field(GATE_BINDING_ATTEMPT)?,
        })
    }

    fn stamp(self, payload: &mut JsonObject) {
        payload.insert(GATE_BINDING_ORDINAL.into(), json!(self.awaiting_ordinal));
        payload.insert(GATE_BINDING_ATTEMPT.into(), json!(self.attempt_number));
    }
}

/// Why an `approve` / `reject` was not enqueued.
#[derive(Debug, thiserror::Error)]
pub enum GateDecisionError {
    #[error("run {0:?} is unknown to this store; no command enqueued")]
    UnknownRun(String),
    #[error(
        "run {run_id:?} has no parked manual gate to decide (status={status}); \
         approve/reject apply only to a gate parked at awaiting_approval, nothing enqueued"
    )]
    NotParked { run_id: String, status: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Enqueue an `approve` / `reject` bound to the gate `run_id` is parked on
/// right now.
///
/// A decision binds to the gate instance the operator is looking at: the
/// payload is stamped with the lifecycle's pending `awaiting_ordinal` and
/// current `attempt_number`, and the resolver applies it only to that same
/// park. A run not parked at a gate (still running or validating, already
/// resolved) refuses with [`GateDecisionError::NotParked`] and enqueues
/// nothing, so a decision can never queue ahead of a gate nobody has seen.
///
/// # Errors
///
/// [`GateDecisionError::UnknownRun`] when the store has no lifecycle for
/// `run_id`, [`GateDecisionError::NotParked`] when it is not parked at a
/// manual gate, and [`GateDecisionError::Store`] when loading the lifecycle
/// or enqueueing the command fails.
pub fn enqueue_gate_decision<S: LifecycleStore + ControlCommandStore + ?Sized>(
    store: &S,
    run_id: &str,
    kind: &str,
    mut payload: JsonObject,
    now: DateTime<Utc>,
) -> Result<(ControlCommandRecord, GateBinding), GateDecisionError> {
    let lifecycle = store
        .load_lifecycle(run_id)?
        .ok_or_else(|| GateDecisionError::UnknownRun(run_id.to_owned()))?;
    let binding =
        GateBinding::of_parked(&lifecycle).ok_or_else(|| GateDecisionError::NotParked {
            run_id: run_id.to_owned(),
            status: lifecycle.status.as_str().to_owned(),
        })?;
    binding.stamp(&mut payload);
    Ok((store.enqueue_command(run_id, kind, payload, now)?, binding))
}

/// Enqueue an `approve` bound to the gate the lifecycle is parked on.
pub fn enqueue_approve<S: LifecycleStore + ControlCommandStore + ?Sized>(
    store: &S,
    run_id: &str,
    now: DateTime<Utc>,
) -> Result<ControlCommandRecord, GateDecisionError> {
    enqueue_gate_decision(
        store,
        run_id,
        CONTROL_COMMAND_APPROVE,
        JsonObject::new(),
        now,
    )
    .map(|(record, _)| record)
}

/// Enqueue a `reject` with optional operator feedback, bound to the gate the
/// lifecycle is parked on.
pub fn enqueue_reject<S: LifecycleStore + ControlCommandStore + ?Sized>(
    store: &S,
    run_id: &str,
    feedback: Option<&str>,
    now: DateTime<Utc>,
) -> Result<ControlCommandRecord, GateDecisionError> {
    let mut payload = JsonObject::new();
    if let Some(feedback) = feedback {
        payload.insert("feedback".into(), json!(feedback));
    }
    enqueue_gate_decision(store, run_id, CONTROL_COMMAND_REJECT, payload, now)
        .map(|(record, _)| record)
}

// --- Checkpoint nudge ------------------------------------------------------------

/// The armed checkpoint-nudge state for one in-flight invocation.
///
/// Armed only when all three inputs are present (a progress probe, a positive
/// threshold, a bounded ceiling) and the baseline probe reading succeeded.
/// Fires at most once: the first poll where the remaining wall time to the
/// ceiling is `<= threshold` AND the probe token still equals the baseline.
pub struct CheckpointNudge {
    probe: ProgressProbe,
    threshold_seconds: f64,
    ceiling_seconds: f64,
    baseline: Value,
    started_at: DateTime<Utc>,
    fired: bool,
}

impl fmt::Debug for CheckpointNudge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckpointNudge")
            .field("threshold_seconds", &self.threshold_seconds)
            .field("ceiling_seconds", &self.ceiling_seconds)
            .field("baseline", &self.baseline)
            .field("started_at", &self.started_at)
            .field("fired", &self.fired)
            .finish_non_exhaustive()
    }
}

impl CheckpointNudge {
    /// Arm the nudge, capturing the iteration-start baseline token now.
    /// Returns `None` (dormant) when any input is absent, the threshold is
    /// non-positive, or the baseline probe reading fails.
    pub fn arm(
        probe: Option<ProgressProbe>,
        threshold_seconds: Option<f64>,
        ceiling_seconds: Option<f64>,
        started_at: DateTime<Utc>,
    ) -> Option<Self> {
        let probe = probe?;
        let threshold_seconds = threshold_seconds.filter(|t| *t > 0.0)?;
        let ceiling_seconds = ceiling_seconds?;
        let baseline = probe_guarded(&probe)?;
        Some(Self {
            probe,
            threshold_seconds,
            ceiling_seconds,
            baseline,
            started_at,
            fired: false,
        })
    }

    pub fn fired(&self) -> bool {
        self.fired
    }

    /// Seconds left to the ceiling at `now`.
    pub fn remaining_seconds(&self, now: DateTime<Utc>) -> f64 {
        let elapsed = (now - self.started_at).as_seconds_f64();
        self.ceiling_seconds - elapsed
    }

    /// Decide whether to fire on this poll: `Some(remaining)` when the
    /// threshold is crossed and the probe reports no new progress. Consumes
    /// the one-shot before the caller dispatches, so a failed injection can
    /// never re-fire on a later poll.
    pub fn should_fire(&mut self, now: DateTime<Utc>) -> Option<f64> {
        if self.fired {
            return None;
        }
        let remaining = self.remaining_seconds(now);
        if remaining > self.threshold_seconds {
            return None;
        }
        let token = probe_guarded(&self.probe)?;
        if token != self.baseline {
            return None;
        }
        self.fired = true;
        Some(remaining)
    }

    fn audit_payload(&self, remaining: f64) -> JsonObject {
        let mut payload = JsonObject::new();
        payload.insert("ceiling_seconds".into(), json!(self.ceiling_seconds));
        payload.insert("threshold_seconds".into(), json!(self.threshold_seconds));
        payload.insert("remaining_seconds".into(), json!(remaining));
        payload
    }
}

/// Read the probe once, containing a panic as a failed reading.
fn probe_guarded(probe: &ProgressProbe) -> Option<Value> {
    catch_unwind(AssertUnwindSafe(|| probe())).ok().flatten()
}

// --- The watcher -----------------------------------------------------------------

/// Where the watcher claims commands from: the store plus the run they are
/// addressed to, with the audit/clock/poll knobs the producer side shares.
#[derive(Clone)]
pub struct ControlPlane {
    pub store: Arc<dyn ControlCommandStore + Send + Sync>,
    pub run_id: String,
    pub poll_interval: Duration,
    pub audit_emit: Option<AuditEmit>,
    pub clock: Clock,
}

impl fmt::Debug for ControlPlane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlPlane")
            .field("run_id", &self.run_id)
            .field("poll_interval", &self.poll_interval)
            .field("audit_emit", &self.audit_emit.is_some())
            .finish_non_exhaustive()
    }
}

impl ControlPlane {
    pub fn new(
        store: Arc<dyn ControlCommandStore + Send + Sync>,
        run_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            run_id: run_id.into(),
            poll_interval: DEFAULT_CONTROL_POLL_INTERVAL,
            audit_emit: None,
            clock: utc_now_clock(),
        }
    }

    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    pub fn with_audit_emit(mut self, audit_emit: AuditEmit) -> Self {
        self.audit_emit = Some(audit_emit);
        self
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

/// What one drain tick did.
#[derive(Debug, Default)]
pub struct DrainReport {
    /// Commands dispatched to the agent, in enqueue order.
    pub applied: Vec<ControlCommandRecord>,
    /// Commands whose dispatch failed (the run continues).
    pub failed: Vec<(ControlCommandRecord, ApplyError)>,
    /// Commands with no live-session semantics (`approve` / `reject`).
    pub not_applicable: Vec<ControlCommandRecord>,
    /// The `interrupt` that was dispatched on this tick, if any: the caller
    /// cancels the run on the same tick.
    pub interrupt: Option<ControlCommandRecord>,
    /// The store's claim call itself failed; nothing was claimed and the
    /// watcher retries on the next tick.
    pub claim_failed: bool,
}

/// How the watcher loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherStop {
    /// The run finished (or the caller stopped the watcher) without a
    /// watcher-initiated cancellation.
    Finished,
    /// An operator `interrupt` was dispatched and the run cancelled.
    OperatorInterrupt { command_id: Option<i64> },
    /// The harness requested a mid-turn recovery; the agent was interrupted
    /// (best-effort) and the run cancelled.
    RecoveryRequested,
}

/// Claims pending control commands for one run and applies them live.
///
/// Built per invocation from the run's [`ControlPlane`] (optional: without
/// one the watcher only serves the observer seams) and the iteration's
/// observers. Drive it with [`ControlCommandWatcher::run`], or call
/// [`ControlCommandWatcher::drain_pending`] for one tick.
pub struct ControlCommandWatcher {
    store: Option<Arc<dyn ControlCommandStore + Send + Sync>>,
    run_id: String,
    poll_interval: Duration,
    audit_emit: Option<AuditEmit>,
    clock: Clock,
    on_applied: Option<CommandAppliedObserver>,
    context_observer: Option<ContextUsageObserver>,
    recovery_signal: Option<RecoverySignal>,
    nudge: Option<CheckpointNudge>,
}

impl fmt::Debug for ControlCommandWatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlCommandWatcher")
            .field("run_id", &self.run_id)
            .field("has_store", &self.store.is_some())
            .field("poll_interval", &self.poll_interval)
            .field("nudge", &self.nudge)
            .finish_non_exhaustive()
    }
}

impl Default for ControlCommandWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlCommandWatcher {
    /// A watcher with no command source: it polls nothing and serves only
    /// the observer seams until configured further.
    pub fn new() -> Self {
        Self {
            store: None,
            run_id: String::new(),
            poll_interval: DEFAULT_CONTROL_POLL_INTERVAL,
            audit_emit: None,
            clock: utc_now_clock(),
            on_applied: None,
            context_observer: None,
            recovery_signal: None,
            nudge: None,
        }
    }

    /// Take the store, run id, poll interval, audit emitter, and clock from
    /// a [`ControlPlane`].
    pub fn with_control_plane(mut self, plane: &ControlPlane) -> Self {
        self.store = Some(plane.store.clone());
        self.run_id = plane.run_id.clone();
        self.poll_interval = plane.poll_interval;
        self.audit_emit = plane.audit_emit.clone();
        self.clock = plane.clock.clone();
        self
    }

    pub fn with_commands(
        mut self,
        store: Arc<dyn ControlCommandStore + Send + Sync>,
        run_id: impl Into<String>,
    ) -> Self {
        self.store = Some(store);
        self.run_id = run_id.into();
        self
    }

    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    pub fn with_audit_emit(mut self, audit_emit: AuditEmit) -> Self {
        self.audit_emit = Some(audit_emit);
        self
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_on_applied(mut self, on_applied: CommandAppliedObserver) -> Self {
        self.on_applied = Some(on_applied);
        self
    }

    pub fn with_context_observer(mut self, observer: ContextUsageObserver) -> Self {
        self.context_observer = Some(observer);
        self
    }

    pub fn with_recovery_signal(mut self, signal: RecoverySignal) -> Self {
        self.recovery_signal = Some(signal);
        self
    }

    pub fn with_checkpoint_nudge(mut self, nudge: Option<CheckpointNudge>) -> Self {
        self.nudge = nudge;
        self
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn nudge(&self) -> Option<&CheckpointNudge> {
        self.nudge.as_ref()
    }

    fn emit(&self, kind: &str, payload: JsonObject) {
        if let Some(emit) = &self.audit_emit {
            guarded(|| emit(kind, &payload));
        }
    }

    fn command_payload(command: &ControlCommandRecord) -> JsonObject {
        let mut payload = JsonObject::new();
        payload.insert("command_id".into(), json!(command.id));
        payload.insert("kind".into(), json!(command.kind));
        payload.insert("payload".into(), Value::Object(command.payload.clone()));
        payload
    }

    /// Claim every pending row for the run and apply each in order.
    ///
    /// The claim-once primitive is shared across watcher restarts and
    /// concurrent workers, so a second tick (or a concurrent peer) returns
    /// nothing for already-claimed rows: a command is never double-applied.
    /// Each apply is independent -- a failure on one row records a failed
    /// event and the loop continues with the next. A failed dispatch never
    /// reaches `on_applied`: the row stays claimed as the visible trace and
    /// no ledger fact is recorded for an application that did not happen.
    pub async fn drain_pending(&self, control: &dyn AgentControl) -> DrainReport {
        let mut report = DrainReport::default();
        let Some(store) = &self.store else {
            return report;
        };
        let pending = match store.claim_commands(&self.run_id, (self.clock)()) {
            Ok(pending) => pending,
            Err(err) => {
                let mut payload = JsonObject::new();
                payload.insert("error_type".into(), json!(audit_error_type(&err)));
                payload.insert("message".into(), json!(err.to_string()));
                self.emit(EVENT_CONTROL_CLAIM_FAILED, payload);
                report.claim_failed = true;
                return report;
            }
        };
        for command in pending {
            let dispatched = match apply_command(control, &command).await {
                Ok(dispatch) => dispatch,
                Err(err) => {
                    let mut payload = Self::command_payload(&command);
                    payload.insert("error_type".into(), json!(err.error_type()));
                    payload.insert("message".into(), json!(err.to_string()));
                    self.emit(EVENT_CONTROL_FAILED, payload);
                    report.failed.push((command, err));
                    continue;
                }
            };
            let kind = match dispatched {
                Dispatch::Applied => EVENT_CONTROL_APPLIED,
                Dispatch::NotApplicable => EVENT_CONTROL_NOT_APPLICABLE,
            };
            self.emit(kind, Self::command_payload(&command));
            match dispatched {
                Dispatch::Applied => {
                    // Steering ledger: record the applied command before a
                    // possible interrupt cancellation, so the fact lands even
                    // when this tick stops the run.
                    if let Some(on_applied) = &self.on_applied {
                        let fact = command_applied_event(&command);
                        guarded(|| on_applied(&command, &fact));
                    }
                    if command.kind == CONTROL_COMMAND_INTERRUPT && report.interrupt.is_none() {
                        report.interrupt = Some(command.clone());
                    }
                    report.applied.push(command);
                }
                Dispatch::NotApplicable => report.not_applicable.push(command),
            }
        }
        report
    }

    /// One poll of the observer seams: context usage, recovery request,
    /// checkpoint nudge. Returns `true` when a recovery was requested (the
    /// agent has been interrupted best-effort; the caller cancels the run).
    async fn observe(&mut self, control: &dyn AgentControl) -> bool {
        if let Some(observer) = &self.context_observer
            && let Ok(reading) = control.context_usage().await
        {
            guarded(|| observer(&reading));
        }
        if self
            .recovery_signal
            .as_ref()
            .is_some_and(RecoverySignal::is_requested)
        {
            // Best-effort: a failed interrupt dispatch must not block the
            // recovery -- the harness still owns the recovery decision.
            let _ = control.interrupt().await;
            return true;
        }
        if let Some(nudge) = &mut self.nudge
            && let Some(remaining) = nudge.should_fire((self.clock)())
            && control
                .inject_message(CHECKPOINT_NUDGE_PROMPT.to_owned())
                .await
                .is_ok()
        {
            let payload = nudge.audit_payload(remaining);
            self.emit(EVENT_CHECKPOINT_NUDGE, payload);
        }
        false
    }

    /// Poll until `stop` flips to `true` or a watcher-initiated cancellation.
    ///
    /// Drain-then-wait: a command already pending when the watcher starts
    /// fires on the first tick. A dispatched `interrupt` or a requested
    /// recovery cancels the run through `cancel` and ends the loop.
    pub async fn run(
        mut self,
        control: Arc<dyn AgentControl>,
        cancel: CancelHandle,
        mut stop: watch::Receiver<bool>,
    ) -> WatcherStop {
        loop {
            if *stop.borrow() {
                return WatcherStop::Finished;
            }
            let report = self.drain_pending(control.as_ref()).await;
            if let Some(command) = report.interrupt {
                cancel.cancel();
                return WatcherStop::OperatorInterrupt {
                    command_id: command.id,
                };
            }
            if self.observe(control.as_ref()).await {
                cancel.cancel();
                return WatcherStop::RecoveryRequested;
            }
            tokio::select! {
                _ = tokio::time::sleep(self.poll_interval) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return WatcherStop::Finished;
                    }
                }
            }
        }
    }
}

fn audit_error_type(err: &StoreError) -> &'static str {
    if err.is_schema_mismatch() {
        "store_schema_error"
    } else if err.as_conflict().is_some() {
        "store_conflict_error"
    } else {
        "store_error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_memory::InMemoryStore;
    use flywheel_agents::BoxFuture;
    use flywheel_agents::PermissionPolicy;
    use std::sync::Mutex;

    /// Scripted live-agent control surface: records every dispatch and can
    /// be told to reject one verb.
    #[derive(Default)]
    struct ScriptedControl {
        injected: Mutex<Vec<String>>,
        models: Mutex<Vec<Option<String>>>,
        interrupts: Mutex<u32>,
        usage_reads: Mutex<u32>,
        fail_set_model: bool,
        fail_interrupt: bool,
        fail_inject: bool,
        usage: Option<Value>,
    }

    impl ScriptedControl {
        fn injected(&self) -> Vec<String> {
            self.injected.lock().unwrap().clone()
        }

        fn models(&self) -> Vec<Option<String>> {
            self.models.lock().unwrap().clone()
        }

        fn interrupts(&self) -> u32 {
            *self.interrupts.lock().unwrap()
        }
    }

    impl AgentControl for ScriptedControl {
        fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>> {
            Box::pin(async move {
                *self.interrupts.lock().unwrap() += 1;
                if self.fail_interrupt {
                    return Err(AgentError::ControlRequest("interrupt rejected".into()));
                }
                Ok(())
            })
        }

        fn set_model(&self, model: Option<String>) -> BoxFuture<'_, Result<(), AgentError>> {
            Box::pin(async move {
                if self.fail_set_model {
                    return Err(AgentError::ControlRequest("set_model rejected".into()));
                }
                self.models.lock().unwrap().push(model);
                Ok(())
            })
        }

        fn set_permission_policy(
            &self,
            _policy: PermissionPolicy,
        ) -> BoxFuture<'_, Result<(), AgentError>> {
            Box::pin(async { Ok(()) })
        }

        fn inject_message(&self, text: String) -> BoxFuture<'_, Result<(), AgentError>> {
            Box::pin(async move {
                if self.fail_inject {
                    return Err(AgentError::ControlRequest("inject rejected".into()));
                }
                self.injected.lock().unwrap().push(text);
                Ok(())
            })
        }

        fn context_usage(&self) -> BoxFuture<'_, Result<Value, AgentError>> {
            Box::pin(async move {
                *self.usage_reads.lock().unwrap() += 1;
                self.usage
                    .clone()
                    .ok_or_else(|| AgentError::ControlRequest("no usage".into()))
            })
        }
    }

    /// A store whose claim call always fails.
    struct FailingClaimStore;

    impl ControlCommandStore for FailingClaimStore {
        fn enqueue_command(
            &self,
            _run_id: &str,
            _kind: &str,
            _payload: JsonObject,
            _now: DateTime<Utc>,
        ) -> Result<ControlCommandRecord, StoreError> {
            Err(StoreError::backend(std::io::Error::other("db locked")))
        }

        fn claim_commands(
            &self,
            _run_id: &str,
            _now: DateTime<Utc>,
        ) -> Result<Vec<ControlCommandRecord>, StoreError> {
            Err(StoreError::backend(std::io::Error::other("db locked")))
        }

        fn delete_command(&self, _command_id: i64) -> Result<(), StoreError> {
            Ok(())
        }
    }

    type AuditLog = Arc<Mutex<Vec<(String, JsonObject)>>>;

    fn audit() -> (AuditLog, AuditEmit) {
        let log: AuditLog = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let emit: AuditEmit = Arc::new(move |kind: &str, payload: &JsonObject| {
            sink.lock()
                .unwrap()
                .push((kind.to_owned(), payload.clone()));
        });
        (log, emit)
    }

    fn kinds(log: &AuditLog) -> Vec<String> {
        log.lock().unwrap().iter().map(|(k, _)| k.clone()).collect()
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn watcher(store: &Arc<InMemoryStore>, emit: AuditEmit) -> ControlCommandWatcher {
        let store: Arc<dyn ControlCommandStore + Send + Sync> = store.clone();
        ControlCommandWatcher::new()
            .with_commands(store, "run-1")
            .with_audit_emit(emit)
            .with_poll_interval(Duration::from_millis(5))
    }

    // --- payload coercion ----------------------------------------------------

    #[test]
    fn payload_coercions_match_the_wire_contract() {
        let say = json!({"text": "focus"}).as_object().unwrap().clone();
        assert_eq!(payload_text(&say).unwrap(), "focus");
        let empty = json!({"text": ""}).as_object().unwrap().clone();
        assert_eq!(payload_text(&empty), Err(ControlCommandError::SayText));
        assert_eq!(
            payload_text(&JsonObject::new()),
            Err(ControlCommandError::SayText)
        );

        assert_eq!(payload_model(&JsonObject::new()).unwrap(), None);
        let null = json!({"model": null}).as_object().unwrap().clone();
        assert_eq!(payload_model(&null).unwrap(), None);
        let named = json!({"model": "opus"}).as_object().unwrap().clone();
        assert_eq!(payload_model(&named).unwrap(), Some("opus"));
        let bad = json!({"model": 7}).as_object().unwrap().clone();
        assert_eq!(payload_model(&bad), Err(ControlCommandError::SetModelModel));

        assert_eq!(payload_feedback(&JsonObject::new()).unwrap(), None);
        let fb = json!({"feedback": "needs tests"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(payload_feedback(&fb).unwrap(), Some("needs tests"));
        let bad_fb = json!({"feedback": ["x"]}).as_object().unwrap().clone();
        assert_eq!(
            payload_feedback(&bad_fb),
            Err(ControlCommandError::RejectFeedback)
        );
        assert_eq!(
            ControlCommandError::UnknownKind("zap".into()).error_type(),
            "unknown_command_kind"
        );
        assert_eq!(ControlCommandError::SayText.error_type(), "invalid_payload");
    }

    #[test]
    fn verb_constants_match_wire_values() {
        assert_eq!(CONTROL_COMMAND_INTERRUPT, "interrupt");
        assert_eq!(CONTROL_COMMAND_SET_MODEL, "set_model");
        assert_eq!(CONTROL_COMMAND_SAY, "say");
        assert_eq!(CONTROL_COMMAND_APPROVE, "approve");
        assert_eq!(CONTROL_COMMAND_REJECT, "reject");
    }

    /// Seed `run_id` parked the way the harness parks one: at
    /// `awaiting_approval` on gate ordinal 2 after a succeeded attempt 1.
    fn seed_parked(store: &InMemoryStore, run_id: &str) {
        use crate::lifecycle::{Attempt, Outcome};
        use crate::store::AttemptStore;
        let mut lc = Lifecycle::new("t").with_run_id(run_id);
        for status in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::AwaitingApproval,
        ] {
            lc.transition_to(status, "", Some(now())).unwrap();
        }
        lc.awaiting_manual_ordinal = Some(2);
        store.create_lifecycle(&lc).unwrap();
        let mut attempt = Attempt::new(1, now(), run_id);
        attempt.outcome = Some(Outcome::Succeeded);
        store.save_attempt(run_id, &attempt, None).unwrap();
    }

    #[test]
    fn gate_decisions_refuse_a_run_with_no_parked_gate() {
        let store = InMemoryStore::new();
        assert!(matches!(
            enqueue_approve(&store, "run-x", now()),
            Err(GateDecisionError::UnknownRun(_))
        ));
        let mut lc = Lifecycle::new("t").with_run_id("run-v");
        for status in [Status::Ready, Status::Running, Status::Validating] {
            lc.transition_to(status, "", Some(now())).unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        match enqueue_reject(&store, "run-v", Some("no"), now()) {
            Err(GateDecisionError::NotParked { status, .. }) => assert_eq!(status, "validating"),
            other => panic!("expected NotParked, got {other:?}"),
        }
        assert!(store.claim_commands("run-v", now()).unwrap().is_empty());
        assert!(store.claim_commands("run-x", now()).unwrap().is_empty());
    }

    #[test]
    fn enqueue_helpers_persist_the_documented_payloads() {
        let store = InMemoryStore::new();
        seed_parked(&store, "run-1");
        let bound = json!({"awaiting_ordinal": 2, "attempt_number": 1});
        let say = enqueue_say(&store, "run-1", "focus", now()).unwrap();
        assert_eq!(say.kind, "say");
        assert_eq!(say.payload.get("text"), Some(&json!("focus")));
        let interrupt = enqueue_interrupt(&store, "run-1", now()).unwrap();
        assert!(interrupt.payload.is_empty());
        let model = enqueue_set_model(&store, "run-1", Some("opus"), now()).unwrap();
        assert_eq!(model.payload.get("model"), Some(&json!("opus")));
        let reset = enqueue_set_model(&store, "run-1", None, now()).unwrap();
        assert_eq!(reset.payload.get("model"), Some(&Value::Null));
        let approve = enqueue_approve(&store, "run-1", now()).unwrap();
        assert_eq!(Value::Object(approve.payload), bound);
        let reject = enqueue_reject(&store, "run-1", Some("nope"), now()).unwrap();
        assert_eq!(
            Value::Object(reject.payload),
            json!({"feedback": "nope", "awaiting_ordinal": 2, "attempt_number": 1})
        );
        let bare_reject = enqueue_reject(&store, "run-1", None, now()).unwrap();
        assert_eq!(Value::Object(bare_reject.payload), bound);
        assert_eq!(store.claim_commands("run-1", now()).unwrap().len(), 7);
    }

    #[test]
    fn command_applied_event_snapshots_the_row() {
        let mut record = ControlCommandRecord::pending(
            "run-1",
            "say",
            json!({"text": "x"}).as_object().unwrap().clone(),
            now(),
        );
        record.id = Some(9);
        let fact = command_applied_event(&record);
        assert_eq!(fact.command_kind, "say");
        assert_eq!(fact.command_payload.get("text"), Some(&json!("x")));
        assert_eq!(fact.command_id, Some(9));
    }

    // --- drain_pending -------------------------------------------------------

    #[tokio::test]
    async fn no_pending_commands_is_silent() {
        let store = Arc::new(InMemoryStore::new());
        let (log, emit) = audit();
        let control = ScriptedControl::default();
        let report = watcher(&store, emit).drain_pending(&control).await;
        assert!(report.applied.is_empty());
        assert!(report.interrupt.is_none());
        assert!(!report.claim_failed);
        assert!(kinds(&log).is_empty());
    }

    #[tokio::test]
    async fn say_injects_and_emits_applied_with_ledger_fact() {
        let store = Arc::new(InMemoryStore::new());
        enqueue_say(
            store.as_ref(),
            "run-1",
            "please address rubric finding",
            now(),
        )
        .unwrap();
        let (log, emit) = audit();
        let facts: Arc<Mutex<Vec<CommandApplied>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = facts.clone();
        let control = ScriptedControl::default();
        let report = watcher(&store, emit)
            .with_on_applied(Arc::new(move |_record, fact| {
                sink.lock().unwrap().push(fact.clone());
            }))
            .drain_pending(&control)
            .await;
        assert_eq!(control.injected(), vec!["please address rubric finding"]);
        assert_eq!(report.applied.len(), 1);
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, EVENT_CONTROL_APPLIED);
        assert_eq!(log[0].1["kind"], "say");
        assert_eq!(log[0].1["payload"]["text"], "please address rubric finding");
        assert_eq!(log[0].1["command_id"], 1);
        let facts = facts.lock().unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].command_kind, "say");
        assert_eq!(facts[0].command_id, Some(1));
    }

    #[tokio::test]
    async fn set_model_applies_and_emits_event() {
        let store = Arc::new(InMemoryStore::new());
        enqueue_set_model(store.as_ref(), "run-1", Some("claude-opus"), now()).unwrap();
        enqueue_set_model(store.as_ref(), "run-1", None, now()).unwrap();
        let (log, emit) = audit();
        let control = ScriptedControl::default();
        watcher(&store, emit).drain_pending(&control).await;
        assert_eq!(control.models(), vec![Some("claude-opus".to_owned()), None]);
        assert_eq!(
            kinds(&log),
            vec![EVENT_CONTROL_APPLIED, EVENT_CONTROL_APPLIED]
        );
    }

    #[tokio::test]
    async fn interrupt_is_reported_after_the_ledger_fact() {
        let store = Arc::new(InMemoryStore::new());
        enqueue_interrupt(store.as_ref(), "run-1", now()).unwrap();
        let (log, emit) = audit();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let control = ScriptedControl::default();
        let report = watcher(&store, emit)
            .with_on_applied(Arc::new(move |record, _fact| {
                sink.lock().unwrap().push(record.kind.clone());
            }))
            .drain_pending(&control)
            .await;
        assert_eq!(control.interrupts(), 1);
        assert_eq!(report.interrupt.unwrap().kind, "interrupt");
        assert_eq!(kinds(&log), vec![EVENT_CONTROL_APPLIED]);
        assert_eq!(seen.lock().unwrap().as_slice(), ["interrupt"]);
    }

    #[tokio::test]
    async fn command_is_not_double_applied_across_ticks() {
        let store = Arc::new(InMemoryStore::new());
        enqueue_say(store.as_ref(), "run-1", "once", now()).unwrap();
        let (_, emit) = audit();
        let control = ScriptedControl::default();
        let w = watcher(&store, emit);
        w.drain_pending(&control).await;
        w.drain_pending(&control).await;
        w.drain_pending(&control).await;
        assert_eq!(control.injected(), vec!["once"]);
    }

    #[tokio::test]
    async fn failed_dispatch_records_failed_event_and_continues() {
        let store = Arc::new(InMemoryStore::new());
        enqueue_set_model(store.as_ref(), "run-1", Some("x"), now()).unwrap();
        enqueue_say(store.as_ref(), "run-1", "after failure", now()).unwrap();
        let (log, emit) = audit();
        let facts = Arc::new(Mutex::new(0u32));
        let sink = facts.clone();
        let control = ScriptedControl {
            fail_set_model: true,
            ..ScriptedControl::default()
        };
        let report = watcher(&store, emit)
            .with_on_applied(Arc::new(move |_r, _f| {
                *sink.lock().unwrap() += 1;
            }))
            .drain_pending(&control)
            .await;
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.applied.len(), 1);
        assert_eq!(control.injected(), vec!["after failure"]);
        let log = log.lock().unwrap();
        assert_eq!(log[0].0, EVENT_CONTROL_FAILED);
        assert_eq!(log[0].1["kind"], "set_model");
        assert_eq!(log[0].1["error_type"], "control_request_failed");
        assert_eq!(log[0].1["message"], "set_model rejected");
        assert_eq!(log[1].0, EVENT_CONTROL_APPLIED);
        // The failed dispatch never reached the steering ledger.
        assert_eq!(*facts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn bad_say_payload_and_unknown_kind_record_failed_events() {
        let store = Arc::new(InMemoryStore::new());
        store
            .enqueue_command(
                "run-1",
                CONTROL_COMMAND_SAY,
                json!({"text": 42}).as_object().unwrap().clone(),
                now(),
            )
            .unwrap();
        store
            .enqueue_command("run-1", "teleport", JsonObject::new(), now())
            .unwrap();
        let (log, emit) = audit();
        let control = ScriptedControl::default();
        let report = watcher(&store, emit).drain_pending(&control).await;
        assert!(control.injected().is_empty());
        assert_eq!(report.failed.len(), 2);
        let log = log.lock().unwrap();
        assert_eq!(log[0].0, EVENT_CONTROL_FAILED);
        assert_eq!(log[0].1["error_type"], "invalid_payload");
        assert_eq!(log[1].0, EVENT_CONTROL_FAILED);
        assert_eq!(log[1].1["error_type"], "unknown_command_kind");
        assert_eq!(log[1].1["kind"], "teleport");
    }

    #[tokio::test]
    async fn command_for_other_run_is_left_alone() {
        let store = Arc::new(InMemoryStore::new());
        enqueue_say(store.as_ref(), "run-2", "not mine", now()).unwrap();
        let (log, emit) = audit();
        let control = ScriptedControl::default();
        watcher(&store, emit).drain_pending(&control).await;
        assert!(control.injected().is_empty());
        assert!(kinds(&log).is_empty());
        assert_eq!(store.claim_commands("run-2", now()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn claim_failure_records_event_and_keeps_going() {
        let (log, emit) = audit();
        let store: Arc<dyn ControlCommandStore + Send + Sync> = Arc::new(FailingClaimStore);
        let control = ScriptedControl::default();
        let w = ControlCommandWatcher::new()
            .with_commands(store, "run-1")
            .with_audit_emit(emit);
        let report = w.drain_pending(&control).await;
        assert!(report.claim_failed);
        let log = log.lock().unwrap();
        assert_eq!(log[0].0, EVENT_CONTROL_CLAIM_FAILED);
        assert_eq!(log[0].1["error_type"], "store_error");
        assert!(log[0].1["message"].as_str().unwrap().contains("db locked"));
    }

    #[tokio::test]
    async fn commands_dispatch_in_enqueue_order() {
        let store = Arc::new(InMemoryStore::new());
        enqueue_say(store.as_ref(), "run-1", "first", now()).unwrap();
        enqueue_set_model(store.as_ref(), "run-1", Some("m"), now()).unwrap();
        enqueue_say(store.as_ref(), "run-1", "second", now()).unwrap();
        let (log, emit) = audit();
        let control = ScriptedControl::default();
        watcher(&store, emit).drain_pending(&control).await;
        assert_eq!(control.injected(), vec!["first", "second"]);
        let ids: Vec<i64> = log
            .lock()
            .unwrap()
            .iter()
            .map(|(_, p)| p["command_id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn approve_and_reject_are_not_dispatched() {
        let store = Arc::new(InMemoryStore::new());
        seed_parked(&store, "run-1");
        enqueue_approve(store.as_ref(), "run-1", now()).unwrap();
        enqueue_reject(store.as_ref(), "run-1", None, now()).unwrap();
        enqueue_reject(store.as_ref(), "run-1", Some("missing tests"), now()).unwrap();
        store
            .enqueue_command(
                "run-1",
                CONTROL_COMMAND_REJECT,
                json!({"feedback": 1}).as_object().unwrap().clone(),
                now(),
            )
            .unwrap();
        let (log, emit) = audit();
        let facts = Arc::new(Mutex::new(0u32));
        let sink = facts.clone();
        let control = ScriptedControl::default();
        let report = watcher(&store, emit)
            .with_on_applied(Arc::new(move |_r, _f| {
                *sink.lock().unwrap() += 1;
            }))
            .drain_pending(&control)
            .await;
        assert!(control.injected().is_empty());
        assert_eq!(control.interrupts(), 0);
        assert_eq!(report.not_applicable.len(), 3);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            kinds(&log),
            vec![
                EVENT_CONTROL_NOT_APPLICABLE,
                EVENT_CONTROL_NOT_APPLICABLE,
                EVENT_CONTROL_NOT_APPLICABLE,
                EVENT_CONTROL_FAILED
            ]
        );
        assert_eq!(*facts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn panicking_callbacks_never_break_the_drain() {
        let store = Arc::new(InMemoryStore::new());
        enqueue_say(store.as_ref(), "run-1", "a", now()).unwrap();
        enqueue_say(store.as_ref(), "run-1", "b", now()).unwrap();
        let control = ScriptedControl::default();
        let report = watcher(
            &store,
            Arc::new(|_k: &str, _p: &JsonObject| panic!("audit sink exploded")),
        )
        .with_on_applied(Arc::new(|_r, _f| panic!("ledger exploded")))
        .drain_pending(&control)
        .await;
        assert_eq!(report.applied.len(), 2);
        assert_eq!(control.injected(), vec!["a", "b"]);
    }

    // --- run loop --------------------------------------------------------------

    #[tokio::test]
    async fn run_loop_finishes_when_stopped() {
        let store = Arc::new(InMemoryStore::new());
        let (log, emit) = audit();
        let control: Arc<dyn AgentControl> = Arc::new(ScriptedControl::default());
        let cancel = CancelHandle::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(watcher(&store, emit).run(control, cancel.clone(), stop_rx));
        tokio::time::sleep(Duration::from_millis(20)).await;
        stop_tx.send(true).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, WatcherStop::Finished);
        assert!(!cancel.is_cancelled());
        assert!(kinds(&log).is_empty());
    }

    #[tokio::test]
    async fn run_loop_cancels_on_operator_interrupt() {
        let store = Arc::new(InMemoryStore::new());
        let (log, emit) = audit();
        let scripted = Arc::new(ScriptedControl::default());
        let control: Arc<dyn AgentControl> = scripted.clone();
        let cancel = CancelHandle::new();
        let (_stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(watcher(&store, emit).run(control, cancel.clone(), stop_rx));
        tokio::time::sleep(Duration::from_millis(10)).await;
        enqueue_interrupt(store.as_ref(), "run-1", now()).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome,
            WatcherStop::OperatorInterrupt {
                command_id: Some(1)
            }
        );
        assert!(cancel.is_cancelled());
        assert_eq!(scripted.interrupts(), 1);
        assert_eq!(kinds(&log), vec![EVENT_CONTROL_APPLIED]);
    }

    #[tokio::test]
    async fn run_loop_observes_context_usage_each_poll() {
        let store = Arc::new(InMemoryStore::new());
        let (_, emit) = audit();
        let control: Arc<dyn AgentControl> = Arc::new(ScriptedControl {
            usage: Some(json!({"used_percentage": 42})),
            ..ScriptedControl::default()
        });
        let readings: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = readings.clone();
        let cancel = CancelHandle::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(
            watcher(&store, emit)
                .with_context_observer(Arc::new(move |reading| {
                    sink.lock().unwrap().push(reading.clone());
                }))
                .run(control, cancel, stop_rx),
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
        stop_tx.send(true).unwrap();
        task.await.unwrap();
        let readings = readings.lock().unwrap();
        assert!(readings.len() >= 2, "{}", readings.len());
        assert_eq!(readings[0]["used_percentage"], 42);
    }

    #[tokio::test]
    async fn context_usage_errors_and_panicking_observers_are_swallowed() {
        let store = Arc::new(InMemoryStore::new());
        let (_, emit) = audit();
        let control: Arc<dyn AgentControl> = Arc::new(ScriptedControl::default());
        let cancel = CancelHandle::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(
            watcher(&store, emit)
                .with_context_observer(Arc::new(|_reading| panic!("observer exploded")))
                .run(control, cancel, stop_rx),
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        stop_tx.send(true).unwrap();
        assert_eq!(task.await.unwrap(), WatcherStop::Finished);
    }

    #[tokio::test]
    async fn recovery_signal_interrupts_and_cancels() {
        for fail_interrupt in [false, true] {
            let store = Arc::new(InMemoryStore::new());
            let (_, emit) = audit();
            let scripted = Arc::new(ScriptedControl {
                fail_interrupt,
                ..ScriptedControl::default()
            });
            let control: Arc<dyn AgentControl> = scripted.clone();
            let signal = RecoverySignal::new();
            let cancel = CancelHandle::new();
            let (_stop_tx, stop_rx) = watch::channel(false);
            let task = tokio::spawn(
                watcher(&store, emit)
                    .with_recovery_signal(signal.clone())
                    .run(control, cancel.clone(), stop_rx),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            signal.request();
            let outcome = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(outcome, WatcherStop::RecoveryRequested);
            assert!(cancel.is_cancelled());
            assert_eq!(scripted.interrupts(), 1);
        }
    }

    // --- checkpoint nudge -----------------------------------------------------

    fn fixed_clock(seconds: Arc<Mutex<i64>>) -> Clock {
        Arc::new(move || {
            DateTime::<Utc>::from_timestamp(1_700_000_000 + *seconds.lock().unwrap(), 0).unwrap()
        })
    }

    fn probe_returning(token: Value) -> ProgressProbe {
        Arc::new(move || Some(token.clone()))
    }

    #[test]
    fn nudge_is_dormant_without_probe_threshold_or_ceiling() {
        let probe = probe_returning(json!("base"));
        let at = Utc::now();
        assert!(CheckpointNudge::arm(None, Some(60.0), Some(600.0), at).is_none());
        assert!(CheckpointNudge::arm(Some(probe.clone()), None, Some(600.0), at).is_none());
        assert!(CheckpointNudge::arm(Some(probe.clone()), Some(0.0), Some(600.0), at).is_none());
        assert!(CheckpointNudge::arm(Some(probe.clone()), Some(60.0), None, at).is_none());
        let failing: ProgressProbe = Arc::new(|| None);
        assert!(CheckpointNudge::arm(Some(failing), Some(60.0), Some(600.0), at).is_none());
        let panicking: ProgressProbe = Arc::new(|| panic!("probe exploded"));
        assert!(CheckpointNudge::arm(Some(panicking), Some(60.0), Some(600.0), at).is_none());
        assert!(CheckpointNudge::arm(Some(probe), Some(60.0), Some(600.0), at).is_some());
    }

    #[test]
    fn nudge_fires_once_when_threshold_crossed_without_progress() {
        let seconds = Arc::new(Mutex::new(0));
        let clock = fixed_clock(seconds.clone());
        let mut nudge = CheckpointNudge::arm(
            Some(probe_returning(json!("base"))),
            Some(60.0),
            Some(600.0),
            clock(),
        )
        .unwrap();
        assert_eq!(nudge.should_fire(clock()), None);
        *seconds.lock().unwrap() = 500;
        assert_eq!(nudge.should_fire(clock()), None);
        *seconds.lock().unwrap() = 545;
        let remaining = nudge.should_fire(clock()).unwrap();
        assert!((remaining - 55.0).abs() < 1e-9);
        assert!(nudge.fired());
        *seconds.lock().unwrap() = 590;
        assert_eq!(nudge.should_fire(clock()), None);
    }

    #[test]
    fn nudge_skipped_when_progress_reported_or_probe_fails_at_check() {
        let seconds = Arc::new(Mutex::new(0));
        let clock = fixed_clock(seconds.clone());
        let calls = Arc::new(Mutex::new(0u32));
        let counter = calls.clone();
        let probe: ProgressProbe = Arc::new(move || {
            let mut n = counter.lock().unwrap();
            *n += 1;
            match *n {
                1 => Some(json!(1)),
                2 => Some(json!(2)),
                3 => None,
                _ => Some(json!(1)),
            }
        });
        let mut nudge =
            CheckpointNudge::arm(Some(probe), Some(60.0), Some(600.0), clock()).unwrap();
        *seconds.lock().unwrap() = 590;
        // Progress made (token changed): skip, stays armed.
        assert_eq!(nudge.should_fire(clock()), None);
        assert!(!nudge.fired());
        // Probe failed: skip, stays armed.
        assert_eq!(nudge.should_fire(clock()), None);
        assert!(!nudge.fired());
        // Back to the baseline token: fires.
        assert!(nudge.should_fire(clock()).is_some());
    }

    #[tokio::test]
    async fn watcher_injects_nudge_prompt_and_emits_event() {
        let store = Arc::new(InMemoryStore::new());
        let (log, emit) = audit();
        let scripted = Arc::new(ScriptedControl::default());
        let control: Arc<dyn AgentControl> = scripted.clone();
        let seconds = Arc::new(Mutex::new(0));
        let clock = fixed_clock(seconds.clone());
        let nudge = CheckpointNudge::arm(
            Some(probe_returning(json!("same"))),
            Some(60.0),
            Some(100.0),
            clock(),
        );
        let cancel = CancelHandle::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(
            watcher(&store, emit)
                .with_clock(clock)
                .with_checkpoint_nudge(nudge)
                .run(control, cancel, stop_rx),
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(scripted.injected().is_empty());
        *seconds.lock().unwrap() = 50;
        tokio::time::sleep(Duration::from_millis(30)).await;
        stop_tx.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(scripted.injected(), vec![CHECKPOINT_NUDGE_PROMPT]);
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, EVENT_CHECKPOINT_NUDGE);
        assert_eq!(log[0].1["ceiling_seconds"], 100.0);
        assert_eq!(log[0].1["threshold_seconds"], 60.0);
        assert_eq!(log[0].1["remaining_seconds"], 50.0);
    }

    #[tokio::test]
    async fn failed_nudge_injection_emits_nothing_and_never_refires() {
        let store = Arc::new(InMemoryStore::new());
        let (log, emit) = audit();
        let control: Arc<dyn AgentControl> = Arc::new(ScriptedControl {
            fail_inject: true,
            ..ScriptedControl::default()
        });
        let clock: Clock = Arc::new(|| DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap());
        let nudge = CheckpointNudge::arm(
            Some(probe_returning(json!(0))),
            Some(60.0),
            Some(10.0),
            clock(),
        );
        let cancel = CancelHandle::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(
            watcher(&store, emit)
                .with_clock(clock)
                .with_checkpoint_nudge(nudge)
                .run(control, cancel, stop_rx),
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        stop_tx.send(true).unwrap();
        task.await.unwrap();
        assert!(kinds(&log).is_empty());
    }

    #[test]
    fn control_plane_builder_threads_knobs_into_watcher() {
        let store: Arc<dyn ControlCommandStore + Send + Sync> = Arc::new(InMemoryStore::new());
        let (_, emit) = audit();
        let plane = ControlPlane::new(store, "run-9")
            .with_poll_interval(Duration::from_millis(7))
            .with_audit_emit(emit);
        let w = ControlCommandWatcher::new().with_control_plane(&plane);
        assert_eq!(w.run_id(), "run-9");
        assert_eq!(w.poll_interval, Duration::from_millis(7));
        assert!(w.audit_emit.is_some());
        assert!(w.store.is_some());
        assert!(format!("{plane:?}").contains("run-9"));
    }

    #[test]
    fn recovery_signal_is_shared_and_idempotent() {
        let signal = RecoverySignal::new();
        let twin = signal.clone();
        assert!(!signal.is_requested());
        twin.request();
        twin.request();
        assert!(signal.is_requested());
    }
}
