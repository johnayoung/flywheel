//! The per-run shared state and the single authoritative state-write seam.
//!
//! [`Run`] bundles the mirroring store, the run telemetry, the shared
//! in-memory lifecycle, the clocks, and the shutdown signal. Every status
//! change in the harness goes through [`Run::append`] / [`Run::transition`]:
//! the store folds the event onto the persisted projection, advances the
//! lifecycle / attempts projections, and appends the event row in one
//! atomic unit; the returned lifecycle is reconciled back into the shared
//! in-memory copy (including `version`, the optimistic-concurrency key for
//! the next append). State and timeline cannot diverge: the append *is*
//! the transition.
//!
//! The lifecycle is shared behind a mutex because the steering-ledger
//! observer appends `CommandApplied` facts from the invoker's watcher task
//! while the harness is parked awaiting the iteration -- the Rust spelling
//! of the Python single-event-loop interleaving.

use super::HarnessError;
use super::telemetry::{MirroringStore, RunTelemetry, store_error_type};
use crate::events::{AttemptFinalized, DomainEvent, DomainEventPayload, TransitionedTo};
use crate::grader_transcript::MonotonicClock;
use crate::invoker_client::Clock;
use crate::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use crate::store::{DomainEventStore, JsonObject, StoreError};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::fmt;
use std::future::pending;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

/// The operator shutdown signal (SIGINT / SIGTERM): the Rust spelling of the
/// `asyncio.CancelledError` a signal-cancelled task surfaces in the Python
/// harness.
///
/// Trigger it from a signal handler (see
/// [`ShutdownSignal::install_os_handlers`]) or any supervisor; the harness
/// observes it at the attempt boundary (the in-flight agent invocation and
/// the rubric-judge await are the cancellation points), finalizes the open
/// attempt `INTERNAL_ERROR`, emits `harness.interrupted`, transitions the
/// lifecycle to `INTERRUPTED`, and returns [`HarnessError::Interrupted`] so
/// the worker stops cleanly.
#[derive(Clone, Debug)]
pub struct ShutdownSignal {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl ShutdownSignal {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// Request shutdown. Idempotent.
    pub fn trigger(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_triggered(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolve once the signal has been triggered; never resolves otherwise.
    pub async fn triggered(&self) {
        let mut rx = self.rx.clone();
        if rx.wait_for(|triggered| *triggered).await.is_err() {
            pending::<()>().await;
        }
    }

    /// Trigger this signal on the process's SIGINT or SIGTERM (unix only;
    /// a no-op elsewhere). Spawns a background listener on the current
    /// tokio runtime.
    pub fn install_os_handlers(&self) {
        #[cfg(unix)]
        {
            let signal = self.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal as unix_signal};
                let mut term = match unix_signal(SignalKind::terminate()) {
                    Ok(term) => term,
                    Err(_) => return,
                };
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                signal.trigger();
            });
        }
    }
}

/// Resolve when `signal` fires; pending forever when there is no signal.
pub(super) async fn wait_shutdown(signal: Option<&ShutdownSignal>) {
    match signal {
        Some(signal) => signal.triggered().await,
        None => pending::<()>().await,
    }
}

/// Why an attempt body stopped before finalizing normally.
#[derive(Debug)]
pub(super) enum AttemptError {
    /// The hang watchdog cancelled the invocation.
    Hang {
        timeout_seconds: f64,
        silence_seconds: f64,
    },
    /// The wall-clock agent-iteration deadline cancelled the invocation.
    Deadline {
        ceiling_seconds: f64,
        elapsed_seconds: f64,
    },
    /// Operator-driven shutdown (SIGINT/SIGTERM or an operator `interrupt`
    /// command) surfaced mid-attempt.
    Interrupted,
    /// An infrastructure failure (store, grader runner, filesystem).
    Failed(HarnessError),
}

impl AttemptError {
    pub(super) fn error_text(&self) -> String {
        match self {
            AttemptError::Hang {
                timeout_seconds,
                silence_seconds,
                ..
            } => format!(
                "hang watchdog: no SDK message for {silence_seconds:.3}s (threshold {timeout_seconds:.3}s)"
            ),
            AttemptError::Deadline {
                ceiling_seconds,
                elapsed_seconds,
                ..
            } => format!(
                "agent-iteration deadline: invocation exceeded wall-clock ceiling of {ceiling_seconds:.3}s after {elapsed_seconds:.3}s"
            ),
            AttemptError::Interrupted => "operator interrupted mid-attempt".to_owned(),
            AttemptError::Failed(err) => err.to_string(),
        }
    }
}

impl From<StoreError> for AttemptError {
    fn from(err: StoreError) -> Self {
        AttemptError::Failed(HarnessError::Store(err))
    }
}

impl From<HarnessError> for AttemptError {
    fn from(err: HarnessError) -> Self {
        AttemptError::Failed(err)
    }
}

/// Statuses from which the operator-interrupt finalizer can land cleanly on
/// INTERRUPTED via a single transition.
const INTERRUPTIBLE_STATUSES: [Status; 2] = [Status::Running, Status::Validating];

/// The shared per-run context.
pub(super) struct Run {
    pub store: Arc<MirroringStore>,
    pub telemetry: Arc<RunTelemetry>,
    pub lifecycle: Arc<Mutex<Lifecycle>>,
    pub run_id: String,
    pub clock: Clock,
    pub mclock: MonotonicClock,
    pub shutdown: Option<ShutdownSignal>,
}

impl fmt::Debug for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Run")
            .field("run_id", &self.run_id)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl Run {
    pub fn new(
        store: Arc<MirroringStore>,
        telemetry: Arc<RunTelemetry>,
        lifecycle: Lifecycle,
        clock: Clock,
        mclock: MonotonicClock,
        shutdown: Option<ShutdownSignal>,
    ) -> Self {
        let run_id = lifecycle.run_id.clone();
        Self {
            store,
            telemetry,
            lifecycle: Arc::new(Mutex::new(lifecycle)),
            run_id,
            clock,
            mclock,
            shutdown,
        }
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    pub fn monotonic(&self) -> f64 {
        (self.mclock)()
    }

    fn lock(&self) -> MutexGuard<'_, Lifecycle> {
        self.lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn snapshot(&self) -> Lifecycle {
        self.lock().clone()
    }

    pub fn status(&self) -> Status {
        self.lock().status
    }

    pub fn with_lifecycle<R>(&self, f: impl FnOnce(&mut Lifecycle) -> R) -> R {
        f(&mut self.lock())
    }

    pub fn emit(&self, kind: &str, payload: JsonObject, attempt_number: Option<u32>) {
        self.telemetry.emit(kind, payload, attempt_number, None);
    }

    /// Append one domain event and re-sync the shared lifecycle.
    pub fn append(&self, event: DomainEvent) -> Result<(), StoreError> {
        let mut lifecycle = self.lock();
        let folded = self.store.append_domain_event(&event, lifecycle.version)?;
        lifecycle.replace_from(&folded);
        lifecycle.attempts = folded.attempts;
        Ok(())
    }

    pub fn append_payload(
        &self,
        ts: DateTime<Utc>,
        attempt_number: Option<u32>,
        payload: DomainEventPayload,
    ) -> Result<(), StoreError> {
        let mut event = DomainEvent::new(self.run_id.clone(), ts, payload);
        event.attempt_number = attempt_number;
        self.append(event)
    }

    /// Apply one lifecycle transition by appending a `TransitionedTo` event.
    pub fn transition(&self, target: Status, error: &str) -> Result<(), StoreError> {
        self.append_payload(
            self.now(),
            None,
            DomainEventPayload::TransitionedTo(TransitionedTo {
                target,
                error: error.to_owned(),
            }),
        )
    }

    /// Persist the attempt's terminal fields and emit the finalization event.
    pub fn finalize_attempt(
        &self,
        attempt: &mut Attempt,
        outcome: Outcome,
        error: &str,
        agent_output: &str,
    ) -> Result<(), StoreError> {
        let ended_at = self.now();
        attempt.ended_at = Some(ended_at);
        attempt.outcome = Some(outcome);
        attempt.error = error.to_owned();
        if !agent_output.is_empty() {
            attempt.agent_output = agent_output.to_owned();
        }
        self.append_payload(
            ended_at,
            Some(attempt.number),
            DomainEventPayload::AttemptFinalized(AttemptFinalized {
                number: attempt.number,
                outcome,
                ended_at,
                agent_output: agent_output.to_owned(),
                error: error.to_owned(),
            }),
        )?;
        self.emit_attempt_finalized(attempt.number, outcome, error);
        Ok(())
    }

    pub fn emit_attempt_finalized(&self, number: u32, outcome: Outcome, error: &str) {
        self.emit(
            "harness.attempt_finalized",
            object(json!({
                "number": number,
                "outcome": outcome.as_str(),
                "error": error,
            })),
            Some(number),
        );
    }

    /// Finalize an attempt whose invocation the hang watchdog or the
    /// wall-clock deadline cancelled: close the open attempt
    /// `INTERNAL_ERROR` and transition `running -> internal_error`. The
    /// `harness.hang_detected` / `harness.deadline_exceeded` audit event
    /// was emitted before the cancel. Idempotent against an already
    /// finalized attempt.
    pub fn handle_timeout_class(
        &self,
        attempt: &mut Attempt,
        error: &str,
    ) -> Result<(), StoreError> {
        if attempt.ended_at.is_none() {
            self.finalize_attempt(attempt, Outcome::InternalError, error, "")?;
        }
        if self.status() != Status::InternalError {
            self.transition(Status::InternalError, error)?;
        }
        Ok(())
    }

    /// In-band finalization for an operator-driven SIGINT/SIGTERM.
    ///
    /// Closes the open attempt as `INTERNAL_ERROR`, emits
    /// `harness.interrupted`, and transitions the lifecycle to
    /// `INTERRUPTED` (not a retry-source state, so the retry budget is
    /// preserved). Idempotent: a lifecycle outside RUNNING / VALIDATING is
    /// left untouched so a second signal during shutdown is safe. Never
    /// fails: a store fault during the attempt finalization is swallowed
    /// and one during the transition is surfaced as a `harness.crash`
    /// event (`classification='interrupt_transition_failed'`) so the row
    /// left in RUNNING / VALIDATING is observable for the stranded sweep.
    pub fn handle_interrupt(&self, attempt: Option<&mut Attempt>) {
        let from_status = self.status();
        if !INTERRUPTIBLE_STATUSES.contains(&from_status) {
            return;
        }
        let reason = "operator interrupted mid-attempt";
        let attempt_number = attempt.as_ref().map(|a| a.number);
        self.emit(
            "harness.interrupted",
            object(json!({
                "classification": "worker_interrupted",
                "from_status": from_status.as_str(),
                "message": reason,
            })),
            attempt_number,
        );
        if let Some(attempt) = attempt
            && attempt.ended_at.is_none()
        {
            let ended_at = self.now();
            attempt.ended_at = Some(ended_at);
            attempt.outcome = Some(Outcome::InternalError);
            attempt.error = reason.to_owned();
            let _ = self.append_payload(
                ended_at,
                Some(attempt.number),
                DomainEventPayload::AttemptFinalized(AttemptFinalized {
                    number: attempt.number,
                    outcome: Outcome::InternalError,
                    ended_at,
                    agent_output: attempt.agent_output.clone(),
                    error: reason.to_owned(),
                }),
            );
            // Mirror finalize_stranded_lifecycle's telemetry shape so the
            // in-band path and the out-of-band sweep share one signature.
            self.emit_attempt_finalized(attempt.number, Outcome::InternalError, reason);
        }
        if let Err(err) = self.transition(Status::Interrupted, "") {
            self.emit(
                "harness.crash",
                object(json!({
                    "classification": "interrupt_transition_failed",
                    "exception_type": store_error_type(&err),
                    "message": err.to_string(),
                })),
                attempt_number,
            );
        }
    }
}

/// Coerce a `json!` object literal into the payload map type.
pub(super) fn object(value: serde_json::Value) -> JsonObject {
    match value {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = JsonObject::new();
            map.insert("value".into(), other);
            map
        }
    }
}
