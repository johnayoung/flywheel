//! Persistence contracts for flywheel.
//!
//! Pure type module: defines the wire-level shape every concrete store must
//! satisfy (in-memory, SQLite, future backends). Keying mirrors
//! `crates/flywheel-core/schema/persistence-schema.sql`
//! column-level definitions exactly so concrete stores converge on the same
//! surface.
//!
//! This module deliberately touches no IO, SQLite, file, stream, or network
//! API. Serialization is the concrete store's concern; the traits traffic in
//! typed records and JSON object payloads. Every method is synchronous:
//! SQLite is fast, and the harness calls the store directly from async code.
//! Implementations take `&self` so a store can be shared (`Arc<dyn
//! HarnessStore>`); interior mutability is the implementation's business.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;

use crate::event_serde::EventSerdeError;
use crate::events::{DomainEvent, DomainEventKind, EventReplayError};
use crate::lifecycle::{Attempt, Lifecycle, Status};
use crate::task::{GraderType, Task};

// --- Schema version -------------------------------------------------------------

/// Bumped whenever the persistence schema gains a backwards-incompatible
/// change. Stores compare their on-disk `schema_version` row against this
/// constant and fail with [`StoreSchemaError`] when it does not match (v11
/// databases are forward-migrated in place to v12 by the store bootstraps;
/// the change is purely additive: the nullable `lifecycles.source` column).
pub const CURRENT_SCHEMA_VERSION: u32 = 12;

// --- Typed conflict signals -----------------------------------------------------

/// Typed conflict signals surfaced by a store contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreConflictError {
    /// A lifecycle write's `expected_version` does not match the stored
    /// `Lifecycle.version`. Carries enough structured detail that callers
    /// can decide between reload-and-retry, abort, or merge without
    /// re-parsing the message.
    #[error(
        "lifecycle {run_id:?}: expected version {expected_version}, stored version {actual_version}"
    )]
    OptimisticConcurrency {
        run_id: String,
        expected_version: u64,
        actual_version: u64,
    },
    /// `create_lifecycle` (or the seed domain event) was called with a
    /// `run_id` already present in the store.
    #[error("lifecycle {run_id:?} already exists")]
    LifecycleAlreadyExists { run_id: String },
    /// A versioned write named a `run_id` that has no stored row.
    #[error("lifecycle {run_id:?} not found")]
    LifecycleNotFound { run_id: String },
}

impl StoreConflictError {
    pub fn run_id(&self) -> &str {
        match self {
            StoreConflictError::OptimisticConcurrency { run_id, .. }
            | StoreConflictError::LifecycleAlreadyExists { run_id }
            | StoreConflictError::LifecycleNotFound { run_id } => run_id,
        }
    }
}

// --- Schema-version mismatch signal ---------------------------------------------

/// A concrete store opened a database whose `schema_version` row does not
/// match [`CURRENT_SCHEMA_VERSION`].
///
/// The message is fixed-prefix (`store must be re-created`) and the value
/// carries both the observed and expected versions so callers can
/// distinguish a legacy database from generic operational errors. Retrying
/// a mismatch cannot reconcile an incompatible on-disk schema; the store
/// must be re-created, so the fault classifier buckets it as PERMANENT via
/// [`StoreError::is_schema_mismatch`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "store must be re-created: observed schema_version={}, expected {expected_version}",
    observed_display(.observed_version)
)]
pub struct StoreSchemaError {
    /// `None` when the sentinel row is missing entirely.
    pub observed_version: Option<u32>,
    pub expected_version: u32,
}

fn observed_display(observed: &Option<u32>) -> String {
    match observed {
        Some(v) => v.to_string(),
        None => "None".to_owned(),
    }
}

impl StoreSchemaError {
    pub fn new(observed_version: Option<u32>) -> Self {
        Self {
            observed_version,
            expected_version: CURRENT_SCHEMA_VERSION,
        }
    }
}

/// Every failure a store contract can surface.
///
/// Conflict and schema signals are typed so callers branch on them; anything
/// backend-specific (a database driver error, a filesystem failure) rides in
/// [`StoreError::Backend`] / [`StoreError::Io`] with its source preserved.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Conflict(#[from] StoreConflictError),
    #[error(transparent)]
    Schema(#[from] StoreSchemaError),
    /// The stored event log could not be folded (corrupt ledger).
    #[error(transparent)]
    Replay(#[from] EventReplayError),
    /// A stored event row could not be decoded, or an event could not be
    /// encoded for storage.
    #[error(transparent)]
    EventSerde(#[from] EventSerdeError),
    /// A JSON payload column (grader spec/payload, control-command payload,
    /// lifecycle timestamps) failed to encode or decode.
    #[error("store json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("store io: {0}")]
    Io(#[from] std::io::Error),
    /// A backend-specific failure (database driver, lock, corruption).
    #[error("store backend: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl StoreError {
    /// Wrap a backend-specific error, preserving it as the source.
    pub fn backend(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        StoreError::Backend(Box::new(err))
    }

    pub fn optimistic_concurrency(
        run_id: impl Into<String>,
        expected_version: u64,
        actual_version: u64,
    ) -> Self {
        StoreConflictError::OptimisticConcurrency {
            run_id: run_id.into(),
            expected_version,
            actual_version,
        }
        .into()
    }

    pub fn lifecycle_already_exists(run_id: impl Into<String>) -> Self {
        StoreConflictError::LifecycleAlreadyExists {
            run_id: run_id.into(),
        }
        .into()
    }

    pub fn lifecycle_not_found(run_id: impl Into<String>) -> Self {
        StoreConflictError::LifecycleNotFound {
            run_id: run_id.into(),
        }
        .into()
    }

    pub fn as_conflict(&self) -> Option<&StoreConflictError> {
        match self {
            StoreError::Conflict(c) => Some(c),
            _ => None,
        }
    }

    /// The single check a fault classifier needs to bucket any
    /// schema-version mismatch as PERMANENT.
    pub fn is_schema_mismatch(&self) -> bool {
        matches!(self, StoreError::Schema(_))
    }
}

// --- Record types ---------------------------------------------------------------

/// A JSON object payload persisted verbatim.
pub type JsonObject = Map<String, Value>;

/// A harness telemetry event as read back from a run's stream.
///
/// Since spec 00025 this is a read-model shape, not a table mirror:
/// telemetry lives in the per-run JSONL file behind [`TelemetrySink`], and
/// the file reader (`crate::audit`) reconstructs `EventRecord` values from
/// `harness.*` lines. `sequence` is assigned by the reader (line count
/// within the file) and defines the per-run ordering consumers see; `id` is
/// unused on the file path and stays `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub run_id: String,
    #[serde(with = "crate::event_serde::timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: String,
    #[serde(default)]
    pub payload: JsonObject,
    #[serde(default)]
    pub attempt_number: Option<u32>,
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub sequence: Option<u64>,
}

impl EventRecord {
    pub fn new(run_id: impl Into<String>, ts: DateTime<Utc>, kind: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            ts,
            kind: kind.into(),
            payload: JsonObject::new(),
            attempt_number: None,
            id: None,
            sequence: None,
        }
    }
}

/// A single SDK message as read back from a run's stream.
///
/// Since spec 00025 this is a read-model shape, not a table mirror: the
/// verbatim SDK message stream lives in the per-run JSONL file behind
/// [`TelemetrySink`], and the file reader reconstructs `SdkMessageRecord`
/// values from message lines. `sequence` is assigned by the reader (line
/// count within the file); `id` is unused on the file path and stays
/// `None`. `payload` is an opaque JSON object (no typed coupling to any
/// agent SDK), persisted and read back verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdkMessageRecord {
    pub run_id: String,
    pub attempt_number: u32,
    pub iteration_number: u32,
    pub message_type: String,
    pub payload: JsonObject,
    #[serde(with = "crate::event_serde::timestamp")]
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub sequence: Option<u64>,
    #[serde(default)]
    pub id: Option<i64>,
}

/// Typed union yielded by the observability readers (`crate::audit`). Both
/// arms expose the per-run `sequence` that defines the stream ordering.
#[derive(Debug, Clone, PartialEq)]
pub enum AuditRecord {
    Event(EventRecord),
    SdkMessage(SdkMessageRecord),
}

impl AuditRecord {
    pub fn run_id(&self) -> &str {
        match self {
            AuditRecord::Event(e) => &e.run_id,
            AuditRecord::SdkMessage(m) => &m.run_id,
        }
    }

    pub fn ts(&self) -> DateTime<Utc> {
        match self {
            AuditRecord::Event(e) => e.ts,
            AuditRecord::SdkMessage(m) => m.ts,
        }
    }

    pub fn sequence(&self) -> Option<u64> {
        match self {
            AuditRecord::Event(e) => e.sequence,
            AuditRecord::SdkMessage(m) => m.sequence,
        }
    }
}

/// One line in a run's observability stream: the unit of [`TelemetrySink`]
/// appends.
///
/// Mirrors no table: telemetry lives outside the database, in per-run
/// destinations owned by a concrete sink (see `docs/data-taxonomy.md`).
/// There is no store-assigned `id` or `sequence`; emission order at the
/// sink is the canonical observability ordering for the run.
///
/// `kind` discriminates the record (an SDK message type, a harness
/// telemetry event kind, or a mirrored domain-event kind).
/// `attempt_number` / `iteration_number` locate the record within the run;
/// either may be `None` for records emitted outside an attempt or
/// iteration. `payload` is an opaque JSON object persisted verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryRecord {
    pub run_id: String,
    #[serde(with = "crate::event_serde::timestamp")]
    pub ts: DateTime<Utc>,
    pub kind: String,
    #[serde(default)]
    pub payload: JsonObject,
    #[serde(default)]
    pub attempt_number: Option<u32>,
    #[serde(default)]
    pub iteration_number: Option<u32>,
}

impl TelemetryRecord {
    pub fn new(run_id: impl Into<String>, ts: DateTime<Utc>, kind: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            ts,
            kind: kind.into(),
            payload: JsonObject::new(),
            attempt_number: None,
            iteration_number: None,
        }
    }
}

/// A single grader execution receipt; append-only by contract.
///
/// Mirrors the `grader_results` table. `grader_spec` snapshots the exact
/// grader object as it appeared in the task at run time so historical truth
/// survives later edits. `payload` carries per-type execution detail (shape
/// defined by `grader_type` per the schema header). `id` is assigned by the
/// store on append.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraderResultRecord {
    pub run_id: String,
    pub attempt_number: u32,
    pub ordinal: u32,
    pub grader_type: GraderType,
    pub grader_spec: JsonObject,
    pub passed: bool,
    pub duration_ms: u64,
    pub payload: JsonObject,
    #[serde(with = "crate::event_serde::timestamp")]
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub grader_name: Option<String>,
    #[serde(default)]
    pub id: Option<i64>,
}

/// One row in the `control_commands` table.
///
/// A control command is an operator-issued intervention against a live run
/// (`interrupt`, `say`, `set_model`, `approve`, `reject`, ...) routed
/// through the store so producers and the in-process watcher coordinate
/// across the worker-daemon boundary. `payload` is opaque per-kind detail
/// (the operator message for `say`, the target identifier for
/// `set_model`), persisted verbatim.
///
/// `id` is assigned by the store on `enqueue_command` and is the canonical
/// enqueue-order key: `claim_commands` returns pending rows ascending by
/// `id`. `claimed_at` is `None` while pending and populated with the claim
/// moment by `claim_commands`; the column drives claim-once semantics so a
/// single command applies at most one time across watcher restarts and
/// concurrent workers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlCommandRecord {
    pub run_id: String,
    pub kind: String,
    pub payload: JsonObject,
    #[serde(with = "crate::event_serde::timestamp")]
    pub enqueued_at: DateTime<Utc>,
    #[serde(default, with = "crate::event_serde::optional_timestamp")]
    pub claimed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub id: Option<i64>,
}

impl ControlCommandRecord {
    /// A freshly enqueued, unclaimed, store-id-less command.
    pub fn pending(
        run_id: impl Into<String>,
        kind: impl Into<String>,
        payload: JsonObject,
        enqueued_at: DateTime<Utc>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            kind: kind.into(),
            payload,
            enqueued_at,
            claimed_at: None,
            id: None,
        }
    }

    pub fn is_pending(&self) -> bool {
        self.claimed_at.is_none()
    }
}

/// A queryable witness that a disk/inode preflight declined an authoritative
/// store write because free space or free inodes fell below threshold.
///
/// Pure data. It is produced by the worker-layer preflight (which owns the
/// filesystem probe; this module performs no IO) and surfaced on the
/// worker's status/telemetry surface so an operator can see the decline as
/// first-class state instead of a half-written store row or a swallowed
/// `ENOSPC` crash. The record exists precisely because the crashing write
/// was *not* attempted: it is emitted before that write, never after
/// catching its failure.
///
/// `below_bytes` / `below_inodes` say which dimension tripped (either, or
/// both); at least one is always `true` on a record that exists. The
/// observed and configured values are captured verbatim so the decision is
/// auditable. `run_id` locates the declined unit when the preflight guards
/// a run-scoped write; it is `None` for writes not tied to a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DegradedSpaceRecord {
    pub path: String,
    pub free_bytes: u64,
    pub free_inodes: u64,
    pub min_free_bytes: u64,
    pub min_free_inodes: u64,
    pub below_bytes: bool,
    pub below_inodes: bool,
    #[serde(with = "crate::event_serde::timestamp")]
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub run_id: Option<String>,
}

// --- Aggregate read shapes ------------------------------------------------------

/// Cross-run token and cost totals returned by
/// [`LifecycleStore::summarize_spend`].
///
/// The four token fields mirror the `attempts` aggregate columns summed
/// across every attempt of every run; `total_cost_usd` is the matching
/// `SUM(total_cost_usd)`. An empty store (or an empty window) yields
/// all-zero totals, never an absent value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SpendSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_cost_usd: f64,
}

impl SpendSummary {
    /// Add one attempt's aggregate counters to the running total.
    pub fn add_attempt(&mut self, attempt: &Attempt) {
        self.input_tokens += attempt.input_tokens;
        self.output_tokens += attempt.output_tokens;
        self.cache_creation_tokens += attempt.cache_creation_input_tokens;
        self.cache_read_tokens += attempt.cache_read_input_tokens;
        self.total_cost_usd += attempt.total_cost_usd;
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_creation_tokens + self.cache_read_tokens
    }
}

/// The half-open activity window `[since, until)` a spend summary is bounded
/// by. Both bounds are optional; the default is unbounded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpendWindow {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

impl SpendWindow {
    pub const UNBOUNDED: SpendWindow = SpendWindow {
        since: None,
        until: None,
    };

    pub fn is_unbounded(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }

    /// The single predicate every backend applies to an attempt's
    /// `last_activity_at`: an attempt exactly at `since` is included, one
    /// exactly at `until` is excluded, and an attempt with no activity
    /// instant contributes to the unbounded grand total but is excluded from
    /// any bounded window (its instant is unknown, so it cannot be placed
    /// inside one).
    pub fn includes(&self, last_activity_at: Option<DateTime<Utc>>) -> bool {
        if self.is_unbounded() {
            return true;
        }
        let Some(activity) = last_activity_at else {
            return false;
        };
        if self.since.is_some_and(|since| activity < since) {
            return false;
        }
        if self.until.is_some_and(|until| activity >= until) {
            return false;
        }
        true
    }
}

/// The optional filters for [`LifecycleStore::list_lifecycles`]. All
/// compose with AND; the default applies no filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleFilter {
    /// `None` applies no status filter; `Some(set)` keeps rows whose status
    /// is a member (an empty set matches nothing).
    pub statuses: Option<Vec<Status>>,
    pub task_id: Option<String>,
    /// `None` applies no task-set filter; `Some(set)` keeps rows whose task
    /// id is a member (an empty set matches nothing). Lets a caller classify
    /// many tasks with one read instead of one read per task.
    pub task_ids: Option<Vec<String>>,
}

impl LifecycleFilter {
    #[must_use]
    pub fn with_statuses(mut self, statuses: impl IntoIterator<Item = Status>) -> Self {
        self.statuses = Some(statuses.into_iter().collect());
        self
    }

    #[must_use]
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    #[must_use]
    pub fn with_task_ids<I, T>(mut self, task_ids: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        self.task_ids = Some(task_ids.into_iter().map(Into::into).collect());
        self
    }

    /// Whether a lifecycle satisfies both filters; backends that scan rather
    /// than query apply exactly this.
    pub fn matches(&self, lifecycle: &Lifecycle) -> bool {
        if let Some(statuses) = &self.statuses
            && !statuses.contains(&lifecycle.status)
        {
            return false;
        }
        if let Some(task_id) = &self.task_id
            && task_id != &lifecycle.task_id
        {
            return false;
        }
        if let Some(task_ids) = &self.task_ids
            && !task_ids.contains(&lifecycle.task_id)
        {
            return false;
        }
        true
    }
}

// --- Store traits ----------------------------------------------------------------

/// Persistence contract for [`Lifecycle`] rows, keyed by `run_id`.
///
/// Writes are split into `create` and `update` so optimistic concurrency is
/// enforced on every mutation:
///
/// * [`create_lifecycle`](Self::create_lifecycle) fails with
///   [`StoreConflictError::LifecycleAlreadyExists`] if the `run_id` already
///   exists.
/// * [`update_lifecycle`](Self::update_lifecycle) fails with
///   [`StoreConflictError::OptimisticConcurrency`] if the stored `version`
///   does not match `expected_version`, and
///   [`StoreConflictError::LifecycleNotFound`] if no row exists for
///   `run_id`. The write persists the lifecycle's own `version` (the caller
///   has already advanced it); the check is against the row's prior value.
///
/// [`load_lifecycle`](Self::load_lifecycle) returns a typed [`Lifecycle`]
/// with its `attempts` populated in ascending `number` order, or `None`
/// when the `run_id` is unknown.
pub trait LifecycleStore {
    fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError>;

    fn update_lifecycle(
        &self,
        lifecycle: &Lifecycle,
        expected_version: u64,
    ) -> Result<(), StoreError>;

    fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError>;

    /// List stored lifecycles, optionally filtered, fully folded.
    ///
    /// The cross-task read: holding only the store, a caller can enumerate
    /// every lifecycle and filter by status and/or `task_id` without a
    /// `WorkSource` or on-disk task files. The two filters compose with AND
    /// (see [`LifecycleFilter::matches`]). A filter matching no row returns
    /// an empty vector, never an error.
    ///
    /// Each element is the same fully-folded [`Lifecycle`]
    /// [`load_lifecycle`](Self::load_lifecycle) returns for that `run_id`
    /// (`attempts` populated in ascending `number` order), not a stub or
    /// column projection.
    ///
    /// Results are ordered `(updated_at DESC, run_id DESC)` where
    /// `updated_at` is the store-set write timestamp (not a [`Lifecycle`]
    /// field): the most-recently-written row first, ties broken by greater
    /// `run_id`. The order is deterministic (identical calls return an
    /// identical `run_id` sequence) and identical across every backend.
    fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError>;

    /// Sum token and cost spend across every attempt of every run.
    ///
    /// The cross-run analogue of the per-run rollup: the grand total of the
    /// four `attempts` token columns plus `total_cost_usd` as a
    /// [`SpendSummary`]. `window` bounds a half-open `[since, until)` on
    /// each attempt's `last_activity_at` with exactly the predicate
    /// [`SpendWindow::includes`] defines, so all backends return the same
    /// totals for the same data. An empty store, or a window containing no
    /// attempt, returns all-zero totals, never the grand total in place of
    /// an empty window.
    fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError>;
}

/// Persistence contract for [`Task`] definitions, content-addressed.
///
/// A task is stored across a logical identity and immutable,
/// content-addressed *versions*:
///
/// * The logical task (its stable `id`) is the identity every other task
///   reference foreign-keys, so a run can never point at a task the store
///   has never heard of.
/// * A *version* is keyed `(id, content_hash)` where `content_hash` is the
///   task digest (`crate::loaders`) over the definition: goal, graders,
///   tags, and context. Versions are immutable and deduplicated: re-saving
///   an unchanged definition is a no-op, while editing any hashed field
///   produces a new version. A run pins the exact version it executed via
///   `Lifecycle.task_content_hash`, so historical truth survives later
///   edits.
///
/// Flywheel owns a single task's lifecycle; the dependency DAG between tasks
/// (`prerequisites`) is an orchestration-layer concern and is not persisted
/// here.
pub trait TaskStore {
    /// Idempotent. Registers the task identity and inserts the version row
    /// keyed by `(task.id, digest)` only if absent; returns the digest. A
    /// version's `created_at` is set from `now` on first insert and never
    /// mutated thereafter.
    fn save_task(&self, task: &Task, now: DateTime<Utc>) -> Result<String, StoreError>;

    /// The exact version when `content_hash` is given, else the most
    /// recently created version for `task_id`; `None` when no version row
    /// exists.
    fn load_task(
        &self,
        task_id: &str,
        content_hash: Option<&str>,
    ) -> Result<Option<Task>, StoreError>;

    /// Resolve the run's lifecycle `task_id` + `task_content_hash` to the
    /// precise version that run executed; `None` when the run or its pinned
    /// version is unknown.
    fn load_task_for_run(&self, run_id: &str) -> Result<Option<Task>, StoreError>;
}

/// Event-sourced write contract for the lifecycle.
///
/// Lifecycle state is the fold of an ordered domain-event log. Appending a
/// domain event is the single authoritative write: it advances the log,
/// updates the `lifecycles` projection (and the `attempts` /
/// `grader_results` projections it implies) atomically, and returns the
/// folded [`Lifecycle`]. There is no separate "mutate row then emit event"
/// step, so state and timeline cannot diverge.
///
/// Concurrency: `expected_version` is the optimistic-concurrency
/// compare-and-swap key, the caller's view of the current domain-event
/// offset (`Lifecycle.version`). A mismatch fails with
/// [`StoreConflictError::OptimisticConcurrency`]. The seed event
/// (`LifecycleInitialized`) creates the projection row and fails with
/// [`StoreConflictError::LifecycleAlreadyExists`] if it already exists; its
/// `expected_version` is ignored. Every other event fails with
/// [`StoreConflictError::LifecycleNotFound`] when no row exists for the
/// event's `run_id`.
///
/// `sequence` is allocated per run and strictly monotonic (`MAX + 1` inside
/// the append transaction); `(run_id, sequence)` is unique.
pub trait DomainEventStore {
    fn append_domain_event(
        &self,
        event: &DomainEvent,
        expected_version: u64,
    ) -> Result<Lifecycle, StoreError>;

    /// The run's domain events in ascending `sequence` order, suitable for
    /// `crate::events::replay`. An unknown run yields an empty vector.
    fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError>;

    /// The run's highest-`sequence` event of `kind`, or `None` (also for an
    /// unknown run). Equivalent to the last `kind` event of
    /// [`list_domain_events`](Self::list_domain_events); durable backends
    /// override it with an indexed read that decodes only the one row, so
    /// per-run probes ("is this run parked?") stay cheap on large ledgers.
    ///
    /// # Errors
    ///
    /// The backend's read failure, or a stored row that does not decode.
    fn latest_domain_event_of_kind(
        &self,
        run_id: &str,
        kind: DomainEventKind,
    ) -> Result<Option<DomainEvent>, StoreError> {
        Ok(self
            .list_domain_events(run_id)?
            .into_iter()
            .rev()
            .find(|event| event.kind() == kind))
    }
}

/// Persistence contract for [`Attempt`] records, keyed by `(run_id, number)`.
///
/// [`save_attempt`](Self::save_attempt) is upsert-style: it inserts a new
/// attempt or replaces an existing row keyed by `(run_id, attempt.number)`.
/// The harness uses this to record both the start (`started_at` set,
/// `ended_at` / `outcome` unset) and the finalization (`ended_at` and
/// `outcome` set) of a single attempt without separate verbs.
///
/// `expected_version` opts the write into the lifecycle's
/// optimistic-concurrency check: when supplied, the store verifies,
/// atomically with the upsert, that the stored `Lifecycle.version` for
/// `run_id` still equals it, failing with
/// [`StoreConflictError::OptimisticConcurrency`] on mismatch and
/// [`StoreConflictError::LifecycleNotFound`] when no lifecycle row exists.
/// The harness uses this for the iteration-boundary aggregate rollup so a
/// stale worker cannot clobber counters after the run moved on. When `None`
/// the write is unconditional, matching the projection path that already
/// runs inside a version-checked `append_domain_event` transaction.
///
/// [`list_attempts`](Self::list_attempts) returns attempts in ascending
/// `number` order regardless of insertion order;
/// [`load_attempt`](Self::load_attempt) returns `None` for missing rows.
pub trait AttemptStore {
    fn save_attempt(
        &self,
        run_id: &str,
        attempt: &Attempt,
        expected_version: Option<u64>,
    ) -> Result<(), StoreError>;

    fn load_attempt(&self, run_id: &str, number: u32) -> Result<Option<Attempt>, StoreError>;

    fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError>;
}

/// Pluggable destination for a run's telemetry stream.
///
/// Telemetry (SDK messages, harness telemetry events, mirrored domain
/// events) flows to a sink, not to the relational store: loss is
/// acceptable, ordering is defined by emission order at the sink, and
/// durability semantics belong entirely to the implementation. The shipped
/// implementation is `crate::telemetry_file` (one append-only JSONL file per
/// run); nothing in core depends on a concrete sink.
///
/// [`append_telemetry`](Self::append_telemetry) is the single verb: it
/// accepts one [`TelemetryRecord`] and persists it to the destination keyed
/// by `record.run_id`. Implementations must keep concurrent runs disjoint
/// (records for different `run_id` values never interleave into one
/// destination) and must not reorder or rewrite previously appended
/// records.
pub trait TelemetrySink {
    fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError>;
}

/// Persistence contract for `grader_results`. Append-only by contract.
///
/// The trait intentionally exposes no update or delete entry point:
/// corrections go in new rows or compensating events, never in-place edits.
/// Records are keyed by `(run_id, attempt_number, ordinal)` and
/// [`list_grader_results`](Self::list_grader_results) returns them in that
/// order.
pub trait GraderResultStore {
    /// Persist one receipt and return it with the store-assigned `id` set.
    fn append_grader_result(
        &self,
        result: &GraderResultRecord,
    ) -> Result<GraderResultRecord, StoreError>;

    fn list_grader_results(
        &self,
        run_id: &str,
        attempt_number: u32,
    ) -> Result<Vec<GraderResultRecord>, StoreError>;
}

/// Persistence contract for operator-issued control commands.
///
/// A control command is a store-routed intervention against a live run:
/// [`enqueue_command`](Self::enqueue_command) persists a pending row,
/// [`claim_commands`](Self::claim_commands) returns and atomically marks
/// every pending row for one run exactly once, in enqueue order. The
/// producer (a CLI subcommand) and the consuming in-process watcher
/// communicate only through the store so steering works across the
/// worker-daemon boundary.
///
/// * `enqueue_command` writes a fresh row with `claimed_at = None` and
///   `enqueued_at = now`, assigns the store-side `id` (the enqueue-order
///   key), and returns the persisted record. The command is persisted
///   unconditionally: a command targeting a run that is not currently
///   in-flight is kept pending rather than silently dropped. Concrete stores
///   that enforce a foreign key to `lifecycles` require the run to be known;
///   where enforcement is missing the contract still accepts unknown runs
///   and they remain pending.
/// * `claim_commands` is the claim-once primitive: it atomically selects
///   every pending row for `run_id`, sets `claimed_at = now` on each, and
///   returns the claimed rows in ascending `id` order. A second call (or a
///   concurrent worker's call) returns nothing for the same rows; the claim
///   never double-applies. An empty pending queue returns an empty vector
///   with no error.
/// * `delete_command` removes one row by its store-side `id`: queue
///   hygiene, not retention (spec 00025 FR-10). The consumer deletes an
///   applied row only after the `CommandApplied` domain event recording the
///   application has committed to the ledger, so the operator fact outlives
///   the queue row. Deleting an unknown id is a no-op.
pub trait ControlCommandStore {
    fn enqueue_command(
        &self,
        run_id: &str,
        kind: &str,
        payload: JsonObject,
        now: DateTime<Utc>,
    ) -> Result<ControlCommandRecord, StoreError>;

    fn claim_commands(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<ControlCommandRecord>, StoreError>;

    fn delete_command(&self, command_id: i64) -> Result<(), StoreError>;
}

/// Composite store contract the harness requires.
///
/// Satisfied automatically by any `Send + Sync` type implementing every
/// part (the in-memory and SQLite stores), so the harness operates against
/// `dyn HarnessStore` alone and never against a concrete backend. Telemetry
/// is deliberately not part of it: the sink is a separate seam (see
/// [`TelemetrySink`]) because telemetry never lives in the relational
/// store.
pub trait HarnessStore:
    LifecycleStore
    + TaskStore
    + DomainEventStore
    + AttemptStore
    + GraderResultStore
    + ControlCommandStore
    + Send
    + Sync
{
}

impl<T> HarnessStore for T where
    T: LifecycleStore
        + TaskStore
        + DomainEventStore
        + AttemptStore
        + GraderResultStore
        + ControlCommandStore
        + Send
        + Sync
        + ?Sized
{
}

// --- Forwarding through smart pointers --------------------------------------------
//
// `Box<S>` and `Arc<S>` forward every trait so a wrapped or shared store can be
// handed to anything that takes `impl HarnessStore` / `&dyn HarnessStore`.

macro_rules! forward_store_traits {
    ($($ptr:ident),* $(,)?) => {$(
        impl<S: LifecycleStore + ?Sized> LifecycleStore for $ptr<S> {
            fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
                (**self).create_lifecycle(lifecycle)
            }
            fn update_lifecycle(&self, lifecycle: &Lifecycle, expected_version: u64) -> Result<(), StoreError> {
                (**self).update_lifecycle(lifecycle, expected_version)
            }
            fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
                (**self).load_lifecycle(run_id)
            }
            fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
                (**self).list_lifecycles(filter)
            }
            fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
                (**self).summarize_spend(window)
            }
        }

        impl<S: TaskStore + ?Sized> TaskStore for $ptr<S> {
            fn save_task(&self, task: &Task, now: DateTime<Utc>) -> Result<String, StoreError> {
                (**self).save_task(task, now)
            }
            fn load_task(&self, task_id: &str, content_hash: Option<&str>) -> Result<Option<Task>, StoreError> {
                (**self).load_task(task_id, content_hash)
            }
            fn load_task_for_run(&self, run_id: &str) -> Result<Option<Task>, StoreError> {
                (**self).load_task_for_run(run_id)
            }
        }

        impl<S: DomainEventStore + ?Sized> DomainEventStore for $ptr<S> {
            fn append_domain_event(&self, event: &DomainEvent, expected_version: u64) -> Result<Lifecycle, StoreError> {
                (**self).append_domain_event(event, expected_version)
            }
            fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
                (**self).list_domain_events(run_id)
            }
            fn latest_domain_event_of_kind(&self, run_id: &str, kind: DomainEventKind) -> Result<Option<DomainEvent>, StoreError> {
                (**self).latest_domain_event_of_kind(run_id, kind)
            }
        }

        impl<S: AttemptStore + ?Sized> AttemptStore for $ptr<S> {
            fn save_attempt(&self, run_id: &str, attempt: &Attempt, expected_version: Option<u64>) -> Result<(), StoreError> {
                (**self).save_attempt(run_id, attempt, expected_version)
            }
            fn load_attempt(&self, run_id: &str, number: u32) -> Result<Option<Attempt>, StoreError> {
                (**self).load_attempt(run_id, number)
            }
            fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError> {
                (**self).list_attempts(run_id)
            }
        }

        impl<S: TelemetrySink + ?Sized> TelemetrySink for $ptr<S> {
            fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError> {
                (**self).append_telemetry(record)
            }
        }

        impl<S: GraderResultStore + ?Sized> GraderResultStore for $ptr<S> {
            fn append_grader_result(&self, result: &GraderResultRecord) -> Result<GraderResultRecord, StoreError> {
                (**self).append_grader_result(result)
            }
            fn list_grader_results(&self, run_id: &str, attempt_number: u32) -> Result<Vec<GraderResultRecord>, StoreError> {
                (**self).list_grader_results(run_id, attempt_number)
            }
        }

        impl<S: ControlCommandStore + ?Sized> ControlCommandStore for $ptr<S> {
            fn enqueue_command(
                &self,
                run_id: &str,
                kind: &str,
                payload: JsonObject,
                now: DateTime<Utc>,
            ) -> Result<ControlCommandRecord, StoreError> {
                (**self).enqueue_command(run_id, kind, payload, now)
            }
            fn claim_commands(&self, run_id: &str, now: DateTime<Utc>) -> Result<Vec<ControlCommandRecord>, StoreError> {
                (**self).claim_commands(run_id, now)
            }
            fn delete_command(&self, command_id: i64) -> Result<(), StoreError> {
                (**self).delete_command(command_id)
            }
        }
    )*};
}

forward_store_traits!(Box, Arc);

impl fmt::Display for SpendSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "in={} out={} cache_create={} cache_read={} cost_usd={:.4}",
            self.input_tokens,
            self.output_tokens,
            self.cache_creation_tokens,
            self.cache_read_tokens,
            self.total_cost_usd
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{DomainEventPayload, LifecycleInitialized, replay};
    use serde_json::json;
    use std::sync::Mutex;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_780_000_000 + secs, 0).unwrap()
    }

    /// A complete, real in-memory implementation of every store part: it
    /// proves the traits are satisfiable together, that the blanket
    /// `HarnessStore` impl kicks in, and that `dyn HarnessStore` is usable
    /// through `Box` / `Arc`.
    #[derive(Default)]
    struct MemoryStore {
        lifecycles: Mutex<Vec<Lifecycle>>,
        tasks: Mutex<Vec<(String, Task)>>,
        events: Mutex<Vec<DomainEvent>>,
        graders: Mutex<Vec<GraderResultRecord>>,
        commands: Mutex<Vec<ControlCommandRecord>>,
        telemetry: Mutex<Vec<TelemetryRecord>>,
    }

    impl LifecycleStore for MemoryStore {
        fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
            let mut rows = self.lifecycles.lock().unwrap();
            if rows.iter().any(|r| r.run_id == lifecycle.run_id) {
                return Err(StoreError::lifecycle_already_exists(&lifecycle.run_id));
            }
            rows.push(lifecycle.clone());
            Ok(())
        }
        fn update_lifecycle(
            &self,
            lifecycle: &Lifecycle,
            expected_version: u64,
        ) -> Result<(), StoreError> {
            let mut rows = self.lifecycles.lock().unwrap();
            let row = rows
                .iter_mut()
                .find(|r| r.run_id == lifecycle.run_id)
                .ok_or_else(|| StoreError::lifecycle_not_found(&lifecycle.run_id))?;
            if row.version != expected_version {
                return Err(StoreError::optimistic_concurrency(
                    &lifecycle.run_id,
                    expected_version,
                    row.version,
                ));
            }
            *row = lifecycle.clone();
            Ok(())
        }
        fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
            Ok(self
                .lifecycles
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.run_id == run_id)
                .cloned())
        }
        fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
            let mut out: Vec<Lifecycle> = self
                .lifecycles
                .lock()
                .unwrap()
                .iter()
                .filter(|l| filter.matches(l))
                .cloned()
                .collect();
            out.sort_by(|a, b| b.run_id.cmp(&a.run_id));
            Ok(out)
        }
        fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
            let mut summary = SpendSummary::default();
            for lifecycle in self.lifecycles.lock().unwrap().iter() {
                for attempt in &lifecycle.attempts {
                    if window.includes(attempt.last_activity_at) {
                        summary.add_attempt(attempt);
                    }
                }
            }
            Ok(summary)
        }
    }

    impl TaskStore for MemoryStore {
        fn save_task(&self, task: &Task, _now: DateTime<Utc>) -> Result<String, StoreError> {
            let digest = format!("digest-{}", task.id);
            let mut rows = self.tasks.lock().unwrap();
            if !rows.iter().any(|(d, t)| d == &digest && t.id == task.id) {
                rows.push((digest.clone(), task.clone()));
            }
            Ok(digest)
        }
        fn load_task(
            &self,
            task_id: &str,
            content_hash: Option<&str>,
        ) -> Result<Option<Task>, StoreError> {
            Ok(self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(d, t)| t.id == task_id && content_hash.is_none_or(|h| h == d))
                .map(|(_, t)| t.clone()))
        }
        fn load_task_for_run(&self, run_id: &str) -> Result<Option<Task>, StoreError> {
            match self.load_lifecycle(run_id)? {
                Some(lc) => self.load_task(&lc.task_id, Some(&lc.task_content_hash)),
                None => Ok(None),
            }
        }
    }

    impl DomainEventStore for MemoryStore {
        fn append_domain_event(
            &self,
            event: &DomainEvent,
            expected_version: u64,
        ) -> Result<Lifecycle, StoreError> {
            let current = self.load_lifecycle(&event.run_id)?;
            let folded = match (&event.payload, current) {
                (DomainEventPayload::LifecycleInitialized(_), Some(_)) => {
                    return Err(StoreError::lifecycle_already_exists(&event.run_id));
                }
                (DomainEventPayload::LifecycleInitialized(_), None) => {
                    let seeded = crate::events::apply(None, event)?;
                    self.create_lifecycle(&seeded)?;
                    seeded
                }
                (_, None) => return Err(StoreError::lifecycle_not_found(&event.run_id)),
                (_, Some(current)) => {
                    if current.version != expected_version {
                        return Err(StoreError::optimistic_concurrency(
                            &event.run_id,
                            expected_version,
                            current.version,
                        ));
                    }
                    let folded = crate::events::apply(Some(&current), event)?;
                    self.update_lifecycle(&folded, expected_version)?;
                    folded
                }
            };
            let mut log = self.events.lock().unwrap();
            let sequence = log.iter().filter(|e| e.run_id == event.run_id).count() as u64 + 1;
            let mut stored = event.clone();
            stored.sequence = Some(sequence);
            stored.id = Some(log.len() as i64 + 1);
            log.push(stored);
            Ok(folded)
        }
        fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.run_id == run_id)
                .cloned()
                .collect())
        }
    }

    impl AttemptStore for MemoryStore {
        fn save_attempt(
            &self,
            run_id: &str,
            attempt: &Attempt,
            expected_version: Option<u64>,
        ) -> Result<(), StoreError> {
            let mut rows = self.lifecycles.lock().unwrap();
            let row = rows
                .iter_mut()
                .find(|r| r.run_id == run_id)
                .ok_or_else(|| StoreError::lifecycle_not_found(run_id))?;
            if let Some(expected) = expected_version
                && row.version != expected
            {
                return Err(StoreError::optimistic_concurrency(
                    run_id,
                    expected,
                    row.version,
                ));
            }
            match row.attempts.iter_mut().find(|a| a.number == attempt.number) {
                Some(existing) => *existing = attempt.clone(),
                None => row.attempts.push(attempt.clone()),
            }
            row.attempts.sort_by_key(|a| a.number);
            Ok(())
        }
        fn load_attempt(&self, run_id: &str, number: u32) -> Result<Option<Attempt>, StoreError> {
            Ok(self
                .list_attempts(run_id)?
                .into_iter()
                .find(|a| a.number == number))
        }
        fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError> {
            Ok(self
                .load_lifecycle(run_id)?
                .map(|l| l.attempts)
                .unwrap_or_default())
        }
    }

    impl TelemetrySink for MemoryStore {
        fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError> {
            self.telemetry.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    impl GraderResultStore for MemoryStore {
        fn append_grader_result(
            &self,
            result: &GraderResultRecord,
        ) -> Result<GraderResultRecord, StoreError> {
            let mut rows = self.graders.lock().unwrap();
            let mut stored = result.clone();
            stored.id = Some(rows.len() as i64 + 1);
            rows.push(stored.clone());
            Ok(stored)
        }
        fn list_grader_results(
            &self,
            run_id: &str,
            attempt_number: u32,
        ) -> Result<Vec<GraderResultRecord>, StoreError> {
            let mut out: Vec<GraderResultRecord> = self
                .graders
                .lock()
                .unwrap()
                .iter()
                .filter(|g| g.run_id == run_id && g.attempt_number == attempt_number)
                .cloned()
                .collect();
            out.sort_by_key(|g| g.ordinal);
            Ok(out)
        }
    }

    impl ControlCommandStore for MemoryStore {
        fn enqueue_command(
            &self,
            run_id: &str,
            kind: &str,
            payload: JsonObject,
            now: DateTime<Utc>,
        ) -> Result<ControlCommandRecord, StoreError> {
            let mut rows = self.commands.lock().unwrap();
            let mut record = ControlCommandRecord::pending(run_id, kind, payload, now);
            record.id = Some(rows.iter().filter_map(|c| c.id).max().unwrap_or(0) + 1);
            rows.push(record.clone());
            Ok(record)
        }
        fn claim_commands(
            &self,
            run_id: &str,
            now: DateTime<Utc>,
        ) -> Result<Vec<ControlCommandRecord>, StoreError> {
            let mut rows = self.commands.lock().unwrap();
            let mut claimed = Vec::new();
            for row in rows
                .iter_mut()
                .filter(|c| c.run_id == run_id && c.is_pending())
            {
                row.claimed_at = Some(now);
                claimed.push(row.clone());
            }
            claimed.sort_by_key(|c| c.id);
            Ok(claimed)
        }
        fn delete_command(&self, command_id: i64) -> Result<(), StoreError> {
            self.commands
                .lock()
                .unwrap()
                .retain(|c| c.id != Some(command_id));
            Ok(())
        }
    }

    fn seed_event(run_id: &str, task_id: &str) -> DomainEvent {
        DomainEvent::new(
            run_id,
            at(0),
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: task_id.into(),
                ..LifecycleInitialized::default()
            }),
        )
    }

    fn transition(run_id: &str, secs: i64, target: Status) -> DomainEvent {
        DomainEvent::new(
            run_id,
            at(secs),
            DomainEventPayload::TransitionedTo(crate::events::TransitionedTo {
                target,
                error: String::new(),
            }),
        )
    }

    fn uses_dyn(store: &dyn HarnessStore) -> Result<Lifecycle, StoreError> {
        let seeded = store.append_domain_event(&seed_event("run-1", "task-1"), 0)?;
        store.append_domain_event(&transition("run-1", 1, Status::Ready), seeded.version)
    }

    #[test]
    fn blanket_harness_store_works_through_box_and_arc() {
        let boxed: Box<dyn HarnessStore> = Box::new(MemoryStore::default());
        let ready = uses_dyn(boxed.as_ref()).unwrap();
        assert_eq!(ready.status, Status::Ready);
        assert_eq!(ready.version, 2);
        let events = boxed.list_domain_events("run-1").unwrap();
        assert_eq!(
            events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![Some(1), Some(2)]
        );
        assert_eq!(replay(&events).unwrap(), ready);

        let shared: Arc<dyn HarnessStore> = Arc::new(MemoryStore::default());
        let via_arc = uses_dyn(&shared).unwrap();
        assert_eq!(via_arc.status, Status::Ready);
        // The forwarding impls make the pointer itself a HarnessStore.
        fn takes_impl(store: impl HarnessStore) -> Result<Option<Lifecycle>, StoreError> {
            store.load_lifecycle("run-1")
        }
        assert!(takes_impl(shared.clone()).unwrap().is_some());
        assert!(takes_impl(boxed).unwrap().is_some());
    }

    #[test]
    fn conflict_signals_carry_structured_detail() {
        let store = MemoryStore::default();
        let seeded = store
            .append_domain_event(&seed_event("run-1", "task-1"), 0)
            .unwrap();
        let dup = store
            .append_domain_event(&seed_event("run-1", "task-1"), 0)
            .unwrap_err();
        assert_eq!(
            dup.as_conflict(),
            Some(&StoreConflictError::LifecycleAlreadyExists {
                run_id: "run-1".into()
            })
        );
        assert_eq!(dup.to_string(), "lifecycle \"run-1\" already exists");

        let stale = store
            .append_domain_event(&transition("run-1", 1, Status::Ready), seeded.version + 5)
            .unwrap_err();
        assert_eq!(
            stale.as_conflict(),
            Some(&StoreConflictError::OptimisticConcurrency {
                run_id: "run-1".into(),
                expected_version: 6,
                actual_version: 1,
            })
        );
        assert_eq!(
            stale.to_string(),
            "lifecycle \"run-1\": expected version 6, stored version 1"
        );
        assert_eq!(stale.as_conflict().unwrap().run_id(), "run-1");

        let missing = store
            .append_domain_event(&transition("run-9", 1, Status::Ready), 1)
            .unwrap_err();
        assert_eq!(
            missing.as_conflict(),
            Some(&StoreConflictError::LifecycleNotFound {
                run_id: "run-9".into()
            })
        );
        assert!(!missing.is_schema_mismatch());

        let illegal = store
            .append_domain_event(&transition("run-1", 1, Status::Done), seeded.version)
            .unwrap_err();
        assert!(matches!(illegal, StoreError::Replay(_)));
    }

    #[test]
    fn schema_error_message_and_fields() {
        let err = StoreSchemaError::new(Some(1));
        assert_eq!(err.observed_version, Some(1));
        assert_eq!(err.expected_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            err.to_string(),
            "store must be re-created: observed schema_version=1, expected 12"
        );
        let unknown = StoreSchemaError::new(None);
        assert_eq!(
            unknown.to_string(),
            "store must be re-created: observed schema_version=None, expected 12"
        );
        let wrapped: StoreError = unknown.into();
        assert!(wrapped.is_schema_mismatch());
        assert_eq!(CURRENT_SCHEMA_VERSION, 12);
    }

    #[test]
    fn backend_errors_keep_their_source() {
        let err = StoreError::backend(std::io::Error::other("disk on fire"));
        assert_eq!(err.to_string(), "store backend: disk on fire");
        assert!(std::error::Error::source(&err).is_some());
        let io: StoreError = std::io::Error::other("enospc").into();
        assert!(matches!(io, StoreError::Io(_)));
    }

    #[test]
    fn spend_window_predicate_is_half_open_and_excludes_unknown_activity() {
        let window = SpendWindow {
            since: Some(at(10)),
            until: Some(at(20)),
        };
        assert!(window.includes(Some(at(10))));
        assert!(window.includes(Some(at(19))));
        assert!(!window.includes(Some(at(20))));
        assert!(!window.includes(Some(at(9))));
        assert!(!window.includes(None));
        assert!(SpendWindow::UNBOUNDED.includes(None));
        assert!(
            SpendWindow {
                since: Some(at(10)),
                until: None
            }
            .includes(Some(at(10)))
        );
        assert!(
            !SpendWindow {
                since: None,
                until: Some(at(10))
            }
            .includes(None)
        );
    }

    #[test]
    fn summarize_spend_sums_attempts_and_respects_window() {
        let store = MemoryStore::default();
        store
            .append_domain_event(&seed_event("run-1", "task-1"), 0)
            .unwrap();
        let mut early = Attempt::new(1, at(0), "run-1");
        early.input_tokens = 10;
        early.output_tokens = 5;
        early.cache_creation_input_tokens = 2;
        early.cache_read_input_tokens = 1;
        early.total_cost_usd = 0.5;
        early.last_activity_at = Some(at(5));
        let mut unknown = Attempt::new(2, at(0), "run-1");
        unknown.input_tokens = 100;
        unknown.total_cost_usd = 1.0;
        store.save_attempt("run-1", &early, Some(1)).unwrap();
        store.save_attempt("run-1", &unknown, None).unwrap();

        let grand = store.summarize_spend(&SpendWindow::UNBOUNDED).unwrap();
        assert_eq!(grand.input_tokens, 110);
        assert_eq!(grand.total_tokens(), 118);
        assert!((grand.total_cost_usd - 1.5).abs() < f64::EPSILON);

        let windowed = store
            .summarize_spend(&SpendWindow {
                since: Some(at(0)),
                until: Some(at(6)),
            })
            .unwrap();
        assert_eq!(windowed.input_tokens, 10);
        assert_eq!(windowed.cache_creation_tokens, 2);
        let empty = store
            .summarize_spend(&SpendWindow {
                since: Some(at(100)),
                until: None,
            })
            .unwrap();
        assert_eq!(empty, SpendSummary::default());

        let stale = store.save_attempt("run-1", &early, Some(7)).unwrap_err();
        assert!(matches!(
            stale.as_conflict(),
            Some(StoreConflictError::OptimisticConcurrency {
                expected_version: 7,
                actual_version: 1,
                ..
            })
        ));
    }

    #[test]
    fn lifecycle_filter_composes_with_and() {
        let mut lc = Lifecycle::new("task-1");
        lc.status = Status::Ready;
        assert!(LifecycleFilter::default().matches(&lc));
        assert!(
            LifecycleFilter::default()
                .with_statuses([Status::Ready, Status::Running])
                .matches(&lc)
        );
        assert!(
            !LifecycleFilter::default()
                .with_statuses([Status::Running])
                .matches(&lc)
        );
        assert!(!LifecycleFilter::default().with_statuses([]).matches(&lc));
        assert!(
            LifecycleFilter::default()
                .with_task_id("task-1")
                .matches(&lc)
        );
        assert!(
            !LifecycleFilter::default()
                .with_task_id("task-2")
                .matches(&lc)
        );
        assert!(
            !LifecycleFilter::default()
                .with_statuses([Status::Ready])
                .with_task_id("task-2")
                .matches(&lc)
        );
    }

    #[test]
    fn control_commands_claim_once_in_enqueue_order() {
        let store = MemoryStore::default();
        let first = store
            .enqueue_command(
                "run-1",
                "say",
                json!({"text": "focus"}).as_object().unwrap().clone(),
                at(1),
            )
            .unwrap();
        let second = store
            .enqueue_command("run-1", "interrupt", JsonObject::new(), at(2))
            .unwrap();
        assert_eq!(first.id, Some(1));
        assert_eq!(second.id, Some(2));
        assert!(first.is_pending());
        assert_eq!(first.enqueued_at, at(1));

        let claimed = store.claim_commands("run-1", at(3)).unwrap();
        assert_eq!(
            claimed.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![Some(1), Some(2)]
        );
        assert!(claimed.iter().all(|c| c.claimed_at == Some(at(3))));
        assert!(store.claim_commands("run-1", at(4)).unwrap().is_empty());
        assert!(store.claim_commands("run-other", at(4)).unwrap().is_empty());

        store.delete_command(1).unwrap();
        store.delete_command(99).unwrap();
        assert_eq!(store.commands.lock().unwrap().len(), 1);
    }

    #[test]
    fn grader_results_append_and_list_in_ordinal_order() {
        let store = MemoryStore::default();
        let receipt = |ordinal: u32| GraderResultRecord {
            run_id: "run-1".into(),
            attempt_number: 1,
            ordinal,
            grader_type: GraderType::Command,
            grader_spec: json!({"type": "command", "run": "true"})
                .as_object()
                .unwrap()
                .clone(),
            passed: true,
            duration_ms: 12,
            payload: json!({"exit_code": 0}).as_object().unwrap().clone(),
            ts: at(0),
            grader_name: None,
            id: None,
        };
        let second = store.append_grader_result(&receipt(1)).unwrap();
        let first = store.append_grader_result(&receipt(0)).unwrap();
        assert_eq!(second.id, Some(1));
        assert_eq!(first.id, Some(2));
        let listed = store.list_grader_results("run-1", 1).unwrap();
        assert_eq!(
            listed.iter().map(|g| g.ordinal).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(store.list_grader_results("run-1", 2).unwrap().is_empty());
    }

    #[test]
    fn records_serialize_with_canonical_timestamps() {
        let mut record = TelemetryRecord::new("run-1", at(0), "message_turn");
        record.attempt_number = Some(1);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(
            json,
            json!({
                "run_id": "run-1",
                "ts": "2026-05-28T20:26:40+00:00",
                "kind": "message_turn",
                "payload": {},
                "attempt_number": 1,
                "iteration_number": null
            })
        );
        let back: TelemetryRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, record);
        let minimal: EventRecord = serde_json::from_str(
            r#"{"run_id": "r", "ts": "2024-01-01T00:00:00", "kind": "started"}"#,
        )
        .unwrap();
        assert_eq!(
            minimal,
            EventRecord::new(
                "r",
                DateTime::from_timestamp(1_704_067_200, 0).unwrap(),
                "started"
            )
        );

        let command = ControlCommandRecord::pending("run-1", "say", JsonObject::new(), at(0));
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["claimed_at"], Value::Null);
        assert_eq!(json["id"], Value::Null);
        let back: ControlCommandRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, command);

        let sdk = SdkMessageRecord {
            run_id: "r".into(),
            attempt_number: 1,
            iteration_number: 2,
            message_type: "assistant".into(),
            payload: json!({"raw": "x"}).as_object().unwrap().clone(),
            ts: at(0),
            sequence: None,
            id: None,
        };
        let audit = AuditRecord::SdkMessage(sdk.clone());
        assert_eq!(audit.sequence(), None);
        assert_eq!(audit.run_id(), "r");
        assert_eq!(audit.ts(), at(0));
        let degraded = DegradedSpaceRecord {
            path: "/tmp".into(),
            free_bytes: 1,
            free_inodes: 2,
            min_free_bytes: 3,
            min_free_inodes: 4,
            below_bytes: true,
            below_inodes: true,
            ts: at(0),
            run_id: None,
        };
        let back: DegradedSpaceRecord =
            serde_json::from_value(serde_json::to_value(&degraded).unwrap()).unwrap();
        assert_eq!(back, degraded);
    }
}
