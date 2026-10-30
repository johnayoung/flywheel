//! Best-effort run telemetry and the ledger-mirroring store wrapper.
//!
//! [`RunTelemetry`] wraps a [`TelemetrySink`] with the harness's failure
//! policy (spec 00025 FR-3/FR-4/FR-7): telemetry loss is acceptable, so an
//! append failure never propagates. The first failure is recorded once on
//! stderr (plus a best-effort marker line in the sink itself); subsequent
//! failures for the run are silent. A `None` sink drops every record.
//!
//! [`MirroringStore`] wraps the harness store so every ledger append also
//! lands as a `domain.<kind>` mirror line in the run's telemetry stream,
//! store row first, so the run file renders a self-contained timeline.

use crate::event_serde::{event_kind, event_payload};
use crate::events::{DomainEvent, DomainEventKind};
use crate::invoker_client::Clock;
use crate::lifecycle::{Attempt, Lifecycle};
use crate::store::{
    AttemptStore, ControlCommandRecord, ControlCommandStore, DomainEventStore, GraderResultRecord,
    GraderResultStore, HarnessStore, JsonObject, LifecycleFilter, LifecycleStore, SpendSummary,
    SpendWindow, StoreError, TaskStore, TelemetryRecord, TelemetrySink,
};
use crate::task::Task;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Marker kind appended (best-effort) to the sink itself when its first
/// append fails.
pub const TELEMETRY_FAILED_KIND: &str = "harness.telemetry_sink_failed";

/// Prefix discriminating mirrored domain-event lines from harness telemetry
/// kinds and agent message types inside a run's telemetry stream.
pub const DOMAIN_MIRROR_KIND_PREFIX: &str = "domain.";

/// A shareable telemetry sink.
pub type SharedSink = Arc<dyn TelemetrySink + Send + Sync>;

/// Best-effort telemetry emitter for one run.
pub struct RunTelemetry {
    sink: Option<SharedSink>,
    run_id: String,
    clock: Clock,
    failure_reported: AtomicBool,
}

impl fmt::Debug for RunTelemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunTelemetry")
            .field("run_id", &self.run_id)
            .field("has_sink", &self.sink.is_some())
            .field(
                "failure_reported",
                &self.failure_reported.load(Ordering::SeqCst),
            )
            .finish()
    }
}

impl RunTelemetry {
    pub fn new(sink: Option<SharedSink>, run_id: impl Into<String>, clock: Clock) -> Self {
        Self {
            sink,
            run_id: run_id.into(),
            clock,
            failure_reported: AtomicBool::new(false),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// Append one harness telemetry record. Never fails.
    pub fn emit(
        &self,
        kind: &str,
        payload: JsonObject,
        attempt_number: Option<u32>,
        iteration_number: Option<u32>,
    ) {
        self.append(TelemetryRecord {
            run_id: self.run_id.clone(),
            ts: self.now(),
            kind: kind.to_owned(),
            payload,
            attempt_number,
            iteration_number,
        });
    }

    /// Append one agent message the instant it is observed. Never fails.
    ///
    /// `kind` is the message's own type discriminator (`message_type` for
    /// an SDK-shaped message, `type` for a normalized agent event); the
    /// payload is the verbatim message object.
    pub fn sdk_message(&self, message: &Value, attempt_number: u32, iteration_number: u32) {
        let payload = match message {
            Value::Object(map) => map.clone(),
            other => {
                let mut map = JsonObject::new();
                map.insert("message".into(), other.clone());
                map
            }
        };
        let kind = payload
            .get("message_type")
            .or_else(|| payload.get("type"))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        self.append(TelemetryRecord {
            run_id: self.run_id.clone(),
            ts: self.now(),
            kind,
            payload,
            attempt_number: Some(attempt_number),
            iteration_number: Some(iteration_number),
        });
    }

    /// Mirror one ledger event as a `domain.<kind>` line. Never fails.
    pub fn mirror_domain(&self, event: &DomainEvent) {
        let payload = match event_payload(&event.payload) {
            Ok(payload) => payload,
            Err(err) => {
                self.note_failure("EventSerdeError", &err.to_string());
                return;
            }
        };
        self.append(TelemetryRecord {
            run_id: self.run_id.clone(),
            ts: event.ts,
            kind: format!("{DOMAIN_MIRROR_KIND_PREFIX}{}", event_kind(event)),
            payload,
            attempt_number: event.attempt_number,
            iteration_number: None,
        });
    }

    fn append(&self, record: TelemetryRecord) {
        let Some(sink) = &self.sink else {
            return;
        };
        if let Err(err) = sink.append_telemetry(&record) {
            self.note_failure(store_error_type(&err), &err.to_string());
        }
    }

    /// FR-7: record the first sink failure once, then go silent.
    fn note_failure(&self, error_type: &str, message: &str) {
        if self.failure_reported.swap(true, Ordering::SeqCst) {
            return;
        }
        eprintln!(
            "flywheel: telemetry sink append failed for run {}: {error_type}: {message} -- \
             continuing; further telemetry for this run may be lost",
            self.run_id
        );
        let Some(sink) = &self.sink else {
            return;
        };
        let mut payload = JsonObject::new();
        payload.insert("error_type".into(), json!(error_type));
        payload.insert("message".into(), json!(message));
        let _ = sink.append_telemetry(&TelemetryRecord {
            run_id: self.run_id.clone(),
            ts: self.now(),
            kind: TELEMETRY_FAILED_KIND.to_owned(),
            payload,
            attempt_number: None,
            iteration_number: None,
        });
    }
}

/// A stable error-class label for a [`StoreError`], the Rust spelling of
/// Python's `type(exc).__name__` in crash / failure payloads.
pub fn store_error_type(err: &StoreError) -> &'static str {
    use crate::store::StoreConflictError;
    match err {
        StoreError::Conflict(StoreConflictError::OptimisticConcurrency { .. }) => {
            "OptimisticConcurrencyError"
        }
        StoreError::Conflict(StoreConflictError::LifecycleAlreadyExists { .. }) => {
            "LifecycleAlreadyExistsError"
        }
        StoreError::Conflict(StoreConflictError::LifecycleNotFound { .. }) => {
            "LifecycleNotFoundError"
        }
        StoreError::Schema(_) => "StoreSchemaError",
        StoreError::Replay(_) => "EventReplayError",
        StoreError::EventSerde(_) => "EventSerdeError",
        StoreError::Json(_) => "JsonError",
        StoreError::Io(_) => "IoError",
        StoreError::Backend(_) => "StoreBackendError",
    }
}

/// Wrap a [`HarnessStore`] so every ledger append also lands as a
/// `domain.*` mirror line in the run's telemetry stream.
///
/// `append_domain_event` is the harness's single authoritative state-write
/// seam; mirroring here keeps the store row first -- the mirror fires only
/// after the append returned -- and covers every transition. All other
/// store verbs delegate verbatim.
pub struct MirroringStore {
    wrapped: Arc<dyn HarnessStore>,
    telemetry: Arc<RunTelemetry>,
}

impl fmt::Debug for MirroringStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MirroringStore")
            .field("telemetry", &self.telemetry)
            .finish_non_exhaustive()
    }
}

impl MirroringStore {
    pub fn new(wrapped: Arc<dyn HarnessStore>, telemetry: Arc<RunTelemetry>) -> Self {
        Self { wrapped, telemetry }
    }

    pub fn telemetry(&self) -> &Arc<RunTelemetry> {
        &self.telemetry
    }

    pub fn inner(&self) -> &Arc<dyn HarnessStore> {
        &self.wrapped
    }
}

impl DomainEventStore for MirroringStore {
    fn append_domain_event(
        &self,
        event: &DomainEvent,
        expected_version: u64,
    ) -> Result<Lifecycle, StoreError> {
        let folded = self.wrapped.append_domain_event(event, expected_version)?;
        self.telemetry.mirror_domain(event);
        Ok(folded)
    }

    fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
        self.wrapped.list_domain_events(run_id)
    }

    fn latest_domain_event_of_kind(
        &self,
        run_id: &str,
        kind: DomainEventKind,
    ) -> Result<Option<DomainEvent>, StoreError> {
        self.wrapped.latest_domain_event_of_kind(run_id, kind)
    }
}

impl LifecycleStore for MirroringStore {
    fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
        self.wrapped.create_lifecycle(lifecycle)
    }

    fn update_lifecycle(
        &self,
        lifecycle: &Lifecycle,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        self.wrapped.update_lifecycle(lifecycle, expected_version)
    }

    fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
        self.wrapped.load_lifecycle(run_id)
    }

    fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
        self.wrapped.list_lifecycles(filter)
    }

    fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
        self.wrapped.summarize_spend(window)
    }
}

impl TaskStore for MirroringStore {
    fn save_task(&self, task: &Task, now: DateTime<Utc>) -> Result<String, StoreError> {
        self.wrapped.save_task(task, now)
    }

    fn load_task(
        &self,
        task_id: &str,
        content_hash: Option<&str>,
    ) -> Result<Option<Task>, StoreError> {
        self.wrapped.load_task(task_id, content_hash)
    }

    fn load_task_for_run(&self, run_id: &str) -> Result<Option<Task>, StoreError> {
        self.wrapped.load_task_for_run(run_id)
    }
}

impl AttemptStore for MirroringStore {
    fn save_attempt(
        &self,
        run_id: &str,
        attempt: &Attempt,
        expected_version: Option<u64>,
    ) -> Result<(), StoreError> {
        self.wrapped.save_attempt(run_id, attempt, expected_version)
    }

    fn load_attempt(&self, run_id: &str, number: u32) -> Result<Option<Attempt>, StoreError> {
        self.wrapped.load_attempt(run_id, number)
    }

    fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError> {
        self.wrapped.list_attempts(run_id)
    }
}

impl GraderResultStore for MirroringStore {
    fn append_grader_result(
        &self,
        result: &GraderResultRecord,
    ) -> Result<GraderResultRecord, StoreError> {
        self.wrapped.append_grader_result(result)
    }

    fn list_grader_results(
        &self,
        run_id: &str,
        attempt_number: u32,
    ) -> Result<Vec<GraderResultRecord>, StoreError> {
        self.wrapped.list_grader_results(run_id, attempt_number)
    }
}

impl ControlCommandStore for MirroringStore {
    fn enqueue_command(
        &self,
        run_id: &str,
        kind: &str,
        payload: JsonObject,
        now: DateTime<Utc>,
    ) -> Result<ControlCommandRecord, StoreError> {
        self.wrapped.enqueue_command(run_id, kind, payload, now)
    }

    fn claim_commands(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<ControlCommandRecord>, StoreError> {
        self.wrapped.claim_commands(run_id, now)
    }

    fn delete_command(&self, command_id: i64) -> Result<(), StoreError> {
        self.wrapped.delete_command(command_id)
    }
}
