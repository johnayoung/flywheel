//! Port of the Python `flywheel_orchestrator._claims` module.
//!
//! Per-task lease store for multi-worker mutual exclusion -- the
//! orchestrator's *own* persistence, independent of flywheel-core's store. A
//! claim is transient coordination state (a worker holds a lease while running
//! a task and releases it on completion), not audit history, so it lives here
//! rather than in the core schema. The store owns only its own tables (plus
//! its own `orchestrator_schema_version` sentinel) and can be pointed at the
//! same SQLite file as the core store; each layer manages its own tables and
//! never references the other's.
//!
//! `task_id` is a bare string with no foreign key by design: a claim is taken
//! before the task definition is recorded and deleted on completion.
//!
//! Every timestamp column is written with
//! [`flywheel_core::event_serde::format_timestamp`] and read with
//! [`flywheel_core::event_serde::parse_timestamp`], which reproduce Python's
//! aware-UTC `datetime.isoformat()` byte for byte, so one SQLite file can be
//! driven by Rust and Python workers at once and both agree on lease expiry.
//! The DDL below is the Python module's inline schema verbatim (plus one
//! additive index), so a store bootstrapped by either side opens cleanly in
//! the other.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Duration, Timelike, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use flywheel_core::event_serde::{TimestampParseError, format_timestamp, parse_timestamp};
use flywheel_core::events::{
    PARK_KIND_DIVERGENT_BASE, PARK_KIND_HELD_OUT_GATE, PARK_KIND_MERGE_CONFLICT,
    PARK_KIND_PROTECTED_PATHS, PARK_KIND_PUSH_FAILED, PARK_KIND_STANDING_VERIFY,
    PARK_KIND_SUBMIT_ERROR, PARK_KIND_UNCOMMITTED_WORK,
};
use flywheel_core::loaders::{canonical_json, task_digest};
use flywheel_core::task::Task;

// --- Schema version -------------------------------------------------------------

/// The orchestrator's persisted schema version. Versioned independently of
/// flywheel-core's `schema_version` so the two can share one backend without
/// colliding.
///
/// History (every bump is a pure additive forward migration -- `CREATE TABLE
/// IF NOT EXISTS` plus `ALTER TABLE ADD COLUMN`, never drop-and-recreate):
///
/// * v1 `task_claims` + `orchestrator_schema_version`
/// * v2 `work_items`, `work_item_dependencies`, `source_syncs`
/// * v3 `task_claims.conflict_keys_json`
/// * v4 `orchestrator_events` ledger
/// * v5 `graph_snapshots` + `graph_snapshot_items`
/// * v6 `orchestrator_stop_events` ledger
/// * v7 `orchestrator_stop_events.run_id`
/// * v8 `orchestrator_session_pauses` ledger
/// * v9 `orchestrator_stop_events.attribution`
///
/// This matches the Python module's sentinel so a store bootstrapped by
/// either implementation opens in the other without tripping the
/// newer-than-code guard.
pub const CURRENT_ORCH_SCHEMA_VERSION: u32 = 9;

// --- Claim-event vocabulary --------------------------------------------------------

/// Fresh claim or same-worker re-acquire.
pub const EVENT_ACQUIRED: &str = "acquired";
/// Reclaim over a *different* worker's lapsed lease (deliberately distinct
/// from `acquired` so a steal is observable).
pub const EVENT_STOLEN: &str = "stolen";
/// Lease extended.
pub const EVENT_RENEWED: &str = "renewed";
/// Claim explicitly dropped.
pub const EVENT_RELEASED: &str = "released";
/// Claim reaped by the liveness sweep.
pub const EVENT_EXPIRED: &str = "expired";

/// The five event types the ledger records, one per committed `task_claims`
/// insert/update/delete. There is no `lost` type: a rejected renew commits no
/// state change.
pub const ORCHESTRATOR_EVENT_TYPES: [&str; 5] = [
    EVENT_ACQUIRED,
    EVENT_STOLEN,
    EVENT_RENEWED,
    EVENT_RELEASED,
    EVENT_EXPIRED,
];

// --- Stop-event vocabulary ---------------------------------------------------------

/// A task whose declared prerequisite resolves to no work item this pass.
pub const STOP_DANGLING_PREREQUISITE: &str = "dangling-prerequisite";
/// An autopilot refill cycle that emitted zero tasks.
pub const STOP_NO_OP_CYCLE: &str = "no-op-cycle";
/// A cycle in which a unit made no observable progress.
pub const STOP_NO_PROGRESS: &str = "no-progress-cycle";
/// A sandbox prepare/preflight failure that released the claim without
/// minting a run.
pub const STOP_PREPARE_SKIP: &str = "prepare-skip";
/// A work-source listing that filled its one-page cap.
pub const STOP_SOURCE_TRUNCATION: &str = "source-truncation";
/// A candidate item a source dropped because it resolved to no graders.
pub const STOP_ZERO_GRADER_DROP: &str = "zero-grader-drop";

/// The closed taxonomy of pre-run dead-ends that produced no run id.
pub const ORCHESTRATOR_STOP_EVENT_KINDS: [&str; 6] = [
    STOP_DANGLING_PREREQUISITE,
    STOP_NO_OP_CYCLE,
    STOP_NO_PROGRESS,
    STOP_PREPARE_SKIP,
    STOP_SOURCE_TRUNCATION,
    STOP_ZERO_GRADER_DROP,
];

/// The no-progress back-off re-driver's streak delimiter: internal
/// bookkeeping, neither a pre-run stop nor a queue reason.
pub const STOP_NO_PROGRESS_RESET: &str = "no-progress-reset";
/// The retry-escalation re-driver's boundedness marker (one per task).
pub const STOP_RETRIES_ESCALATED: &str = "retries-escalated";
/// The stop-surface supersession marker appended when a dead-end is serviced.
pub const STOP_RESOLVED: &str = "stop-resolved";
/// The archive sweep's fail-closed marker when landing can be proven neither
/// way.
pub const STOP_INDETERMINATE_LANDING: &str = "indeterminate-landing";

/// A `stop-resolved` marker cleared by the archive sweep's landability probe.
pub const RESOLUTION_ATTRIBUTION_PROBE: &str = "probe";
/// A `stop-resolved` marker cleared by a deliberate operator decision.
pub const RESOLUTION_ATTRIBUTION_OPERATOR: &str = "operator";

/// The resolution-marker attribution vocabulary.
pub const RESOLUTION_ATTRIBUTIONS: [&str; 2] = [
    RESOLUTION_ATTRIBUTION_PROBE,
    RESOLUTION_ATTRIBUTION_OPERATOR,
];

// --- Human-review queue vocabulary ---------------------------------------------------

pub const REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION: &str = "retries-exhausted-after-escalation";
pub const REASON_PREREQUISITE_MISSING: &str = "prerequisite-missing";
pub const REASON_NO_PROGRESS: &str = "no-progress";
pub const REASON_AWAITING_APPROVAL: &str = "awaiting-approval";
pub const REASON_ABORTED: &str = "abort";
pub const REASON_BUDGET_CEILING: &str = "budget-ceiling";

/// The machine-readable reasons a routing layer may queue a unit for human
/// review with: the six routing tokens plus every landing park kind (a parked
/// landing is routed with its park cause as the reason). Disjoint from
/// [`ORCHESTRATOR_STOP_EVENT_KINDS`] so both streams share one table and a
/// reader tells them apart by membership alone.
pub const HUMAN_REVIEW_QUEUE_REASONS: [&str; 14] = [
    REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
    REASON_PREREQUISITE_MISSING,
    REASON_NO_PROGRESS,
    REASON_AWAITING_APPROVAL,
    REASON_ABORTED,
    REASON_BUDGET_CEILING,
    PARK_KIND_UNCOMMITTED_WORK,
    PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_STANDING_VERIFY,
    PARK_KIND_HELD_OUT_GATE,
    PARK_KIND_PROTECTED_PATHS,
    PARK_KIND_PUSH_FAILED,
    PARK_KIND_SUBMIT_ERROR,
    PARK_KIND_MERGE_CONFLICT,
];

/// Whether `reason` is a member of [`HUMAN_REVIEW_QUEUE_REASONS`].
pub fn is_human_review_reason(reason: &str) -> bool {
    HUMAN_REVIEW_QUEUE_REASONS.contains(&reason)
}

fn require_review_reason(reason: &str) -> Result<(), ClaimStoreError> {
    if is_human_review_reason(reason) {
        Ok(())
    } else {
        let mut known: Vec<&str> = HUMAN_REVIEW_QUEUE_REASONS.to_vec();
        known.sort_unstable();
        Err(ClaimStoreError::UnknownReviewReason {
            reason: reason.to_owned(),
            known: known.into_iter().map(str::to_owned).collect(),
        })
    }
}

// --- Errors -----------------------------------------------------------------------

/// `renew_claim` found the claim is no longer the caller's: the lease lapsed
/// and another worker stole the task, or the claim was released, so the
/// `(version, worker_id)` token no longer matches. The caller must stop
/// acting on the task.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("claim on task {task_id:?} lost")]
pub struct ClaimLostError {
    pub task_id: String,
}

/// The orchestrator's on-disk schema version is newer than this build (or the
/// sentinel is unreadable). Older stores are migrated forward in place and
/// never trip this.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "orchestrator store must be re-created: observed orchestrator_schema_version={}, expected {expected}",
    observed_display(.observed)
)]
pub struct OrchestratorSchemaError {
    /// `None` when the sentinel row is missing entirely.
    pub observed: Option<u32>,
    pub expected: u32,
}

fn observed_display(observed: &Option<u32>) -> String {
    match observed {
        Some(v) => v.to_string(),
        None => "None".to_owned(),
    }
}

/// Every failure a claim store can surface.
#[derive(Debug, thiserror::Error)]
pub enum ClaimStoreError {
    #[error(transparent)]
    ClaimLost(#[from] ClaimLostError),
    #[error(transparent)]
    Schema(#[from] OrchestratorSchemaError),
    /// A human-review routing carried a reason outside
    /// [`HUMAN_REVIEW_QUEUE_REASONS`].
    #[error("unknown human-review reason {reason:?}; must be one of {known:?}")]
    UnknownReviewReason { reason: String, known: Vec<String> },
    /// A snapshot named the same `task_id` twice; a snapshot is a
    /// cross-section keyed by task, so the whole write is refused and nothing
    /// is recorded.
    #[error("graph snapshot names task {task_id:?} more than once")]
    DuplicateSnapshotItem { task_id: String },
    /// A lease of `lease_seconds` from `now` lands outside the representable
    /// timestamp range (or is not a number); nothing is recorded.
    #[error(
        "lease of {lease_seconds} seconds from {now} is outside the representable timestamp range"
    )]
    LeaseOutOfRange {
        lease_seconds: f64,
        now: DateTime<Utc>,
    },
    /// A persisted timestamp column could not be parsed.
    #[error(transparent)]
    Timestamp(#[from] TimestampParseError),
    /// A persisted `*_json` column could not be decoded.
    #[error("claim store json: {0}")]
    Json(#[from] serde_json::Error),
    /// The store's connection mutex was poisoned by a panicking holder.
    #[error("claim store lock poisoned")]
    Poisoned,
    /// A backend-specific failure (database driver, lock, corruption).
    #[error("claim store backend: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl From<rusqlite::Error> for ClaimStoreError {
    fn from(err: rusqlite::Error) -> Self {
        ClaimStoreError::Backend(Box::new(err))
    }
}

impl ClaimStoreError {
    pub fn is_claim_lost(&self) -> bool {
        matches!(self, ClaimStoreError::ClaimLost(_))
    }

    pub fn is_schema_mismatch(&self) -> bool {
        matches!(self, ClaimStoreError::Schema(_))
    }
}

// --- Records ----------------------------------------------------------------------

/// A worker's lease on a task, mirroring one `task_claims` row.
///
/// Immutable snapshot: `acquire_claim` / `renew_claim` return a fresh value
/// with the bumped `version` and extended `lease_expires_at`. `version` and
/// `worker_id` together are the optimistic-concurrency token for
/// renew/release -- a stale token (wrong version, or a different worker stole
/// the task) is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskClaim {
    pub task_id: String,
    pub worker_id: String,
    pub claimed_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
    pub version: i64,
}

/// A persisted `work_items` row.
///
/// `first_seen_at` is stamped once on the first observation and never moves;
/// `last_seen_at` advances on every observation; `disappeared_at` is set when
/// a *successful* sync no longer observes the item and cleared the moment it
/// is observed again. The `*_json` fields are canonical JSON strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemRecord {
    pub task_id: String,
    pub source_kind: Option<String>,
    pub source_ref: Option<String>,
    pub source_url: Option<String>,
    pub source_version: Option<String>,
    pub task_content_hash: Option<String>,
    pub priority: i64,
    pub required_capabilities_json: String,
    pub conflict_keys_json: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub disappeared_at: Option<DateTime<Utc>>,
    pub metadata_json: String,
}

/// One observed work item, as handed to [`ClaimStore::upsert_work_item`].
///
/// The catalog write-side shape: the Python store takes a `WorkItem` and
/// derives `task_content_hash` with `task_digest`; this crate's sources
/// module is independent of the store, so the observation carries the digest
/// itself ([`WorkItemObservation::for_task`] computes it).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkItemObservation {
    pub task_id: String,
    pub source_kind: Option<String>,
    pub source_ref: Option<String>,
    pub source_url: Option<String>,
    pub source_version: Option<String>,
    pub task_content_hash: String,
    pub priority: i64,
    pub required_capabilities: BTreeSet<String>,
    pub conflict_keys: BTreeSet<String>,
}

impl WorkItemObservation {
    /// An observation keyed by `task.id` with `task_content_hash =
    /// task_digest(task)` and every provenance / scheduling field at its
    /// default; set the rest with struct update syntax.
    pub fn for_task(task: &Task) -> Self {
        Self {
            task_id: task.id.clone(),
            task_content_hash: task_digest(task),
            ..Self::default()
        }
    }
}

/// A persisted `source_syncs` row: one sync pass over a work source. Written
/// at pass start (`status = "running"`, `finished_at` NULL) and settled when
/// the pass completes (`"ok"` with `observed_count`, or `"error"` with a
/// non-empty `error`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSyncRecord {
    pub id: i64,
    pub source_kind: String,
    pub source_name: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub observed_count: i64,
    pub error: Option<String>,
    pub metadata_json: String,
}

/// One immutable `orchestrator_events` row -- a committed claim-lease
/// transition. Written in the same transaction as the `task_claims` change it
/// describes, never updated or deleted. `id` is the monotonic insertion id so
/// the global stream and a per-task timeline both read back in `id` order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorEventRecord {
    pub id: i64,
    pub task_id: String,
    pub worker_id: String,
    pub event_type: String,
    pub version: i64,
    pub lease_expires_at: DateTime<Utc>,
    pub occurred_at: DateTime<Utc>,
}

/// Alias matching the assignment's naming; the record type is
/// [`OrchestratorEventRecord`].
pub type OrchestratorEvent = OrchestratorEventRecord;

/// One immutable `orchestrator_stop_events` row -- a pre-run dead-end, a
/// human-review routing, or a bookkeeping marker. Never deduped: recurrence
/// is the signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorStopEventRecord {
    pub id: i64,
    pub kind: String,
    pub subject: String,
    pub detail: String,
    pub occurred_at: DateTime<Utc>,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub attribution: String,
}

/// One immutable `orchestrator_session_pauses` row: an observed
/// session-limited run whose derived reset was still in the future when seen.
/// `pause_until` is the clamped horizon (`min(reset_at, recorded_at +
/// ceiling)`) drivers gate on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPauseRecord {
    pub id: i64,
    pub run_id: String,
    pub reset_at: DateTime<Utc>,
    pub pause_until: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
}

/// One entry in the single visible human-review queue: a projection of an
/// `orchestrator_stop_events` row whose `kind` is a member of
/// [`HUMAN_REVIEW_QUEUE_REASONS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanReviewQueueEntry {
    pub id: i64,
    pub task_id: String,
    pub run_id: String,
    pub reason: String,
    pub detail: String,
    pub occurred_at: DateTime<Utc>,
}

/// One captured work item inside a WorkGraph snapshot: the full materialized
/// state of a single item as a scheduling pass saw it. Used both as the input
/// to [`ClaimStore::record_graph_snapshot`] and as the read-back row, so a
/// recorded item round-trips to an equal value.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GraphSnapshotItem {
    pub task_id: String,
    pub source_kind: Option<String>,
    pub source_ref: Option<String>,
    pub source_url: Option<String>,
    pub source_version: Option<String>,
    pub priority: i64,
    pub required_capabilities: BTreeSet<String>,
    pub conflict_keys: BTreeSet<String>,
    pub state: String,
    pub ready: bool,
    pub claim_holder: Option<String>,
    pub resolved_prerequisites: BTreeSet<String>,
}

impl GraphSnapshotItem {
    /// An item with the three required fields set and everything else at its
    /// default (no provenance, priority 0, empty sets, unheld).
    pub fn new(task_id: impl Into<String>, state: impl Into<String>, ready: bool) -> Self {
        Self {
            task_id: task_id.into(),
            state: state.into(),
            ready,
            ..Self::default()
        }
    }
}

/// One `graph_snapshots` header row. `item_count` equals the number of item
/// rows; `last_event_id` is the `orchestrator_events` high-water mark read
/// inside the snapshot's own write transaction (0 on an empty ledger).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSnapshotRecord {
    pub id: i64,
    pub captured_at: DateTime<Utc>,
    pub item_count: i64,
    pub last_event_id: i64,
}

// --- Encoding helpers -----------------------------------------------------------------

/// Canonical JSON-array encoding of a string set, byte-identical to the
/// Python `json.dumps(sorted(values))` (`", "` separator, ASCII-escaped).
pub fn encode_str_set(values: &BTreeSet<String>) -> String {
    let mut out = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&canonical_json(&serde_json::Value::String(value.clone())));
    }
    out.push(']');
    out
}

/// Inverse of [`encode_str_set`].
pub fn decode_str_set(value: &str) -> Result<BTreeSet<String>, serde_json::Error> {
    serde_json::from_str(value)
}

/// Truncate to microsecond precision -- the precision every durable backend
/// (and Python's `datetime`) carries -- so the in-memory store returns the
/// same instants the durable stores read back.
fn to_micros(ts: DateTime<Utc>) -> DateTime<Utc> {
    let nanos = ts.nanosecond();
    let trimmed = nanos - nanos % 1_000;
    ts.with_nanosecond(trimmed).unwrap_or(ts)
}

fn lease_duration(lease_seconds: f64) -> Duration {
    let micros = (lease_seconds * 1_000_000.0).round();
    Duration::microseconds(micros as i64)
}

/// `now + lease_seconds` truncated to micros, or `None` when the seconds are
/// NaN or the sum leaves the representable timestamp range. `lease_duration`
/// saturates out-of-range values to `i64` micros (~292k years), which is
/// already past chrono's range, so the checked add catches every overflow.
pub(crate) fn lease_expiry(now: DateTime<Utc>, lease_seconds: f64) -> Option<DateTime<Utc>> {
    if lease_seconds.is_nan() {
        return None;
    }
    now.checked_add_signed(lease_duration(lease_seconds))
        .map(to_micros)
}

/// [`lease_expiry`] as a store error, so an out-of-range lease is refused
/// before anything is written instead of panicking on `DateTime` overflow.
pub(crate) fn checked_lease_expiry(
    now: DateTime<Utc>,
    lease_seconds: f64,
) -> Result<DateTime<Utc>, ClaimStoreError> {
    lease_expiry(now, lease_seconds).ok_or(ClaimStoreError::LeaseOutOfRange { lease_seconds, now })
}

fn disjoint(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    a.is_disjoint(b)
}

fn sets_from_iter<I, S>(values: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    values.into_iter().map(Into::into).collect()
}

/// Build a conflict-key set from any iterable of strings.
pub fn conflict_keys<I, S>(values: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    sets_from_iter(values)
}

// --- The contract --------------------------------------------------------------------

/// Per-task lease contract for multi-worker mutual exclusion, plus the
/// orchestrator's append-only ledgers and the WorkGraph catalog.
///
/// At most one live claim exists per `task_id`. `now` is injected everywhere
/// (never read from a clock) so expiry, pause horizons, and ledger timestamps
/// are deterministic and testable. Implementations take `&self` so a store
/// can be shared as `Arc<dyn ClaimStore>`.
pub trait ClaimStore: Send + Sync {
    /// Take the lease on `task_id` for `worker_id`.
    ///
    /// Returns `Some` when the task is free, the existing lease has expired
    /// (the new claim *steals* it), or the caller already holds it (idempotent
    /// re-acquire). Returns `None` when a *live* lease is held by a different
    /// worker, or when `conflict_keys` overlap those of a *different* live
    /// claim; the refusal clears once that claim is released or lapses. The
    /// check-and-write is atomic; a refusal writes no ledger row.
    fn acquire_claim(
        &self,
        task_id: &str,
        worker_id: &str,
        now: DateTime<Utc>,
        lease_seconds: f64,
        conflict_keys: &BTreeSet<String>,
    ) -> Result<Option<TaskClaim>, ClaimStoreError>;

    /// Extend the lease and bump `version`. Fails with
    /// [`ClaimStoreError::ClaimLost`] when the `(version, worker_id)` token
    /// no longer matches; a failed renew writes no ledger row.
    fn renew_claim(
        &self,
        claim: &TaskClaim,
        now: DateTime<Utc>,
        lease_seconds: f64,
    ) -> Result<TaskClaim, ClaimStoreError>;

    /// Drop the claim when the token still matches; a no-op (and no ledger
    /// row) if it was already stolen or released. `now` is the ledger's
    /// `occurred_at`; absent a clock it falls back to the lease's expiry.
    fn release_claim(
        &self,
        claim: &TaskClaim,
        now: Option<DateTime<Utc>>,
    ) -> Result<(), ClaimStoreError>;

    /// The current claim row for a task (expiry is not filtered).
    fn load_claim(&self, task_id: &str) -> Result<Option<TaskClaim>, ClaimStoreError>;

    /// Every held row in `task_id` order; released claims are absent, expiry
    /// is not filtered.
    fn list_claims(&self) -> Result<Vec<TaskClaim>, ClaimStoreError>;

    /// Batch-release every claim whose lease lapsed at `now`
    /// (`lease_expires_at <= now`), one `expired` ledger row per reaped
    /// claim, returning the freed task ids in `task_id` order.
    fn sweep_expired_claims(&self, now: DateTime<Utc>) -> Result<Vec<String>, ClaimStoreError>;

    /// The global claim-event stream in id order.
    fn list_events(&self) -> Result<Vec<OrchestratorEventRecord>, ClaimStoreError>;

    /// One task's claim-event timeline in id order.
    fn list_task_events(
        &self,
        task_id: &str,
    ) -> Result<Vec<OrchestratorEventRecord>, ClaimStoreError>;

    /// Append one stop row naming a pre-run dead-end (or a bookkeeping
    /// marker) and its cause. Never dedupes.
    fn record_stop_event(
        &self,
        kind: &str,
        subject: &str,
        detail: &str,
        occurred_at: DateTime<Utc>,
        attribution: &str,
    ) -> Result<(), ClaimStoreError>;

    /// A sandbox prepare/preflight failure: release the claim (with its
    /// `released` event when the token still owns a row) and append a
    /// `prepare-skip` stop row, in one transaction. The stop row is written
    /// unconditionally.
    fn record_prepare_skip(
        &self,
        claim: &TaskClaim,
        detail: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError>;

    /// The global stop-event stream in id order.
    fn list_stop_events(&self) -> Result<Vec<OrchestratorStopEventRecord>, ClaimStoreError>;

    /// One subject's stop-event timeline in id order.
    fn list_subject_stop_events(
        &self,
        subject: &str,
    ) -> Result<Vec<OrchestratorStopEventRecord>, ClaimStoreError>;

    /// Record a pool-wide claim pause for a structural session-limit reset.
    /// Appends one row with `pause_until = min(reset_at, now + ceiling)` and
    /// returns that horizon; records nothing and returns `None` when
    /// `ceiling_seconds <= 0` or the clamped horizon is not after `now`.
    fn record_session_pause(
        &self,
        reset_at: DateTime<Utc>,
        now: DateTime<Utc>,
        ceiling_seconds: f64,
        run_id: &str,
    ) -> Result<Option<DateTime<Utc>>, ClaimStoreError>;

    /// The high-water `pause_until` over rows still in the future at `now`,
    /// or `None` when none remain.
    fn session_pause_until(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, ClaimStoreError>;

    /// Every recorded pause row in id order.
    fn list_session_pauses(&self) -> Result<Vec<SessionPauseRecord>, ClaimStoreError>;

    /// Route one unit into the human-review queue by appending a stop row
    /// whose `kind` is the machine-readable `reason` (which must be a member
    /// of [`HUMAN_REVIEW_QUEUE_REASONS`]).
    fn record_human_review(
        &self,
        reason: &str,
        task_id: &str,
        occurred_at: DateTime<Utc>,
        run_id: &str,
        detail: &str,
    ) -> Result<(), ClaimStoreError>;

    /// Every routed unit across all reasons, in id order.
    fn list_human_review_queue(&self) -> Result<Vec<HumanReviewQueueEntry>, ClaimStoreError>;

    /// Record one WorkGraph snapshot atomically: the header and every item row
    /// commit in one transaction, and `last_event_id` is the live
    /// `orchestrator_events` max id read inside that transaction (0 when the
    /// ledger is empty). An empty `items` records a valid snapshot with item
    /// count 0. Append-only.
    fn record_graph_snapshot(
        &self,
        items: &[GraphSnapshotItem],
        captured_at: DateTime<Utc>,
    ) -> Result<GraphSnapshotRecord, ClaimStoreError>;

    /// Every snapshot header in id order.
    fn list_graph_snapshots(&self) -> Result<Vec<GraphSnapshotRecord>, ClaimStoreError>;

    /// One snapshot's item rows in `task_id` order; an unknown id is empty.
    fn list_graph_snapshot_items(
        &self,
        snapshot_id: i64,
    ) -> Result<Vec<GraphSnapshotItem>, ClaimStoreError>;

    /// The most recently recorded snapshot header, or `None`.
    fn latest_graph_snapshot(&self) -> Result<Option<GraphSnapshotRecord>, ClaimStoreError>;

    /// Insert or refresh the `work_items` row for an observed item.
    /// `first_seen_at` is set only on the initial insert; `last_seen_at` is
    /// set to `now` on every observation and `disappeared_at` is cleared.
    fn upsert_work_item(
        &self,
        item: &WorkItemObservation,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError>;

    /// Replace the dependency edge set for `task_id` with the given
    /// prerequisites; duplicates collapse.
    fn replace_work_item_dependencies(
        &self,
        task_id: &str,
        prerequisite_task_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError>;

    /// Stamp `disappeared_at = now` on previously-seen items absent from
    /// `observed_task_ids`, without deleting any row; items already marked
    /// keep their original stamp.
    fn mark_work_items_disappeared(
        &self,
        observed_task_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError>;

    fn load_work_item(&self, task_id: &str) -> Result<Option<WorkItemRecord>, ClaimStoreError>;

    /// Every catalog row in `task_id` order.
    fn list_work_items(&self) -> Result<Vec<WorkItemRecord>, ClaimStoreError>;

    /// One item's prerequisite ids, sorted.
    fn load_work_item_dependencies(&self, task_id: &str) -> Result<Vec<String>, ClaimStoreError>;

    /// Every `(task_id, prerequisite_task_id)` edge, sorted.
    fn list_work_item_dependencies(&self) -> Result<Vec<(String, String)>, ClaimStoreError>;

    /// Open a `source_syncs` row for a pass (`status = "running"`) and return
    /// its id.
    fn record_source_sync_start(
        &self,
        source_kind: &str,
        source_name: &str,
        now: DateTime<Utc>,
    ) -> Result<i64, ClaimStoreError>;

    /// Settle the `source_syncs` row `sync_id`: `status`, `observed_count`,
    /// `error`, and `finished_at = now`.
    fn record_source_sync_finish(
        &self,
        sync_id: i64,
        status: &str,
        observed_count: i64,
        error: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError>;

    fn load_source_sync(&self, sync_id: i64) -> Result<Option<SourceSyncRecord>, ClaimStoreError>;

    /// Every sync row in id order.
    fn list_source_syncs(&self) -> Result<Vec<SourceSyncRecord>, ClaimStoreError>;
}

// --- In-memory store ---------------------------------------------------------------------

#[derive(Debug, Default)]
struct InMemoryState {
    claims: BTreeMap<String, TaskClaim>,
    claim_conflict_keys: BTreeMap<String, BTreeSet<String>>,
    events: Vec<OrchestratorEventRecord>,
    stop_events: Vec<OrchestratorStopEventRecord>,
    session_pauses: Vec<SessionPauseRecord>,
    snapshots: Vec<GraphSnapshotRecord>,
    snapshot_items: BTreeMap<i64, Vec<GraphSnapshotItem>>,
    work_items: BTreeMap<String, WorkItemRecord>,
    dependencies: BTreeSet<(String, String)>,
    source_syncs: Vec<SourceSyncRecord>,
}

impl InMemoryState {
    fn append_event(
        &mut self,
        task_id: &str,
        worker_id: &str,
        event_type: &str,
        version: i64,
        lease_expires_at: DateTime<Utc>,
        occurred_at: DateTime<Utc>,
    ) {
        let id = self.events.len() as i64 + 1;
        self.events.push(OrchestratorEventRecord {
            id,
            task_id: task_id.to_owned(),
            worker_id: worker_id.to_owned(),
            event_type: event_type.to_owned(),
            version,
            lease_expires_at: to_micros(lease_expires_at),
            occurred_at: to_micros(occurred_at),
        });
    }

    fn append_stop_event(
        &mut self,
        kind: &str,
        subject: &str,
        detail: &str,
        occurred_at: DateTime<Utc>,
        run_id: &str,
        attribution: &str,
    ) {
        let id = self.stop_events.len() as i64 + 1;
        self.stop_events.push(OrchestratorStopEventRecord {
            id,
            kind: kind.to_owned(),
            subject: subject.to_owned(),
            detail: detail.to_owned(),
            occurred_at: to_micros(occurred_at),
            run_id: run_id.to_owned(),
            attribution: attribution.to_owned(),
        });
    }

    fn has_conflicting_live_claim(
        &self,
        task_id: &str,
        incoming: &BTreeSet<String>,
        now: DateTime<Utc>,
    ) -> bool {
        self.claims.iter().any(|(other_id, other)| {
            other_id != task_id
                && other.lease_expires_at > now
                && self
                    .claim_conflict_keys
                    .get(other_id)
                    .is_some_and(|keys| !disjoint(keys, incoming))
        })
    }

    /// Delete the row when the token matches, returning whether it did.
    fn delete_matching(&mut self, claim: &TaskClaim) -> bool {
        let matches = self.claims.get(&claim.task_id).is_some_and(|existing| {
            existing.version == claim.version && existing.worker_id == claim.worker_id
        });
        if matches {
            self.claims.remove(&claim.task_id);
            self.claim_conflict_keys.remove(&claim.task_id);
        }
        matches
    }
}

/// In-memory [`ClaimStore`]. Not durable; the test substrate. Mirrors the
/// durable backends bit for bit: microsecond timestamps, canonical sets, the
/// same ledger and catalog semantics.
#[derive(Debug, Default)]
pub struct InMemoryClaimStore {
    state: Mutex<InMemoryState>,
}

impl InMemoryClaimStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, InMemoryState>, ClaimStoreError> {
        self.state.lock().map_err(|_| ClaimStoreError::Poisoned)
    }
}

impl ClaimStore for InMemoryClaimStore {
    fn acquire_claim(
        &self,
        task_id: &str,
        worker_id: &str,
        now: DateTime<Utc>,
        lease_seconds: f64,
        conflict_keys: &BTreeSet<String>,
    ) -> Result<Option<TaskClaim>, ClaimStoreError> {
        let now = to_micros(now);
        let lease_expires_at = checked_lease_expiry(now, lease_seconds)?;
        let mut state = self.lock()?;
        let existing = state.claims.get(task_id).cloned();
        let free = existing
            .as_ref()
            .is_none_or(|e| e.lease_expires_at <= now || e.worker_id == worker_id);
        if !free {
            return Ok(None);
        }
        if !conflict_keys.is_empty()
            && state.has_conflicting_live_claim(task_id, conflict_keys, now)
        {
            return Ok(None);
        }
        let version = existing.as_ref().map_or(1, |e| e.version + 1);
        let event_type = match &existing {
            Some(e) if e.worker_id != worker_id => EVENT_STOLEN,
            _ => EVENT_ACQUIRED,
        };
        let claim = TaskClaim {
            task_id: task_id.to_owned(),
            worker_id: worker_id.to_owned(),
            claimed_at: now,
            lease_expires_at,
            version,
        };
        state.claims.insert(task_id.to_owned(), claim.clone());
        state
            .claim_conflict_keys
            .insert(task_id.to_owned(), conflict_keys.clone());
        state.append_event(
            task_id,
            worker_id,
            event_type,
            version,
            claim.lease_expires_at,
            now,
        );
        Ok(Some(claim))
    }

    fn renew_claim(
        &self,
        claim: &TaskClaim,
        now: DateTime<Utc>,
        lease_seconds: f64,
    ) -> Result<TaskClaim, ClaimStoreError> {
        let now = to_micros(now);
        let lease_expires_at = checked_lease_expiry(now, lease_seconds)?;
        let mut state = self.lock()?;
        let existing = state.claims.get(&claim.task_id).cloned();
        let Some(existing) =
            existing.filter(|e| e.version == claim.version && e.worker_id == claim.worker_id)
        else {
            return Err(ClaimLostError {
                task_id: claim.task_id.clone(),
            }
            .into());
        };
        let renewed = TaskClaim {
            task_id: claim.task_id.clone(),
            worker_id: claim.worker_id.clone(),
            claimed_at: existing.claimed_at,
            lease_expires_at,
            version: existing.version + 1,
        };
        state.claims.insert(claim.task_id.clone(), renewed.clone());
        state.append_event(
            &renewed.task_id,
            &renewed.worker_id,
            EVENT_RENEWED,
            renewed.version,
            renewed.lease_expires_at,
            now,
        );
        Ok(renewed)
    }

    fn release_claim(
        &self,
        claim: &TaskClaim,
        now: Option<DateTime<Utc>>,
    ) -> Result<(), ClaimStoreError> {
        let mut state = self.lock()?;
        if state.delete_matching(claim) {
            state.append_event(
                &claim.task_id,
                &claim.worker_id,
                EVENT_RELEASED,
                claim.version,
                claim.lease_expires_at,
                now.unwrap_or(claim.lease_expires_at),
            );
        }
        Ok(())
    }

    fn load_claim(&self, task_id: &str) -> Result<Option<TaskClaim>, ClaimStoreError> {
        Ok(self.lock()?.claims.get(task_id).cloned())
    }

    fn list_claims(&self) -> Result<Vec<TaskClaim>, ClaimStoreError> {
        Ok(self.lock()?.claims.values().cloned().collect())
    }

    fn sweep_expired_claims(&self, now: DateTime<Utc>) -> Result<Vec<String>, ClaimStoreError> {
        let now = to_micros(now);
        let mut state = self.lock()?;
        let doomed: Vec<TaskClaim> = state
            .claims
            .values()
            .filter(|c| c.lease_expires_at <= now)
            .cloned()
            .collect();
        for claim in &doomed {
            state.claims.remove(&claim.task_id);
            state.claim_conflict_keys.remove(&claim.task_id);
            state.append_event(
                &claim.task_id,
                &claim.worker_id,
                EVENT_EXPIRED,
                claim.version,
                claim.lease_expires_at,
                now,
            );
        }
        Ok(doomed.into_iter().map(|c| c.task_id).collect())
    }

    fn list_events(&self) -> Result<Vec<OrchestratorEventRecord>, ClaimStoreError> {
        Ok(self.lock()?.events.clone())
    }

    fn list_task_events(
        &self,
        task_id: &str,
    ) -> Result<Vec<OrchestratorEventRecord>, ClaimStoreError> {
        Ok(self
            .lock()?
            .events
            .iter()
            .filter(|e| e.task_id == task_id)
            .cloned()
            .collect())
    }

    fn record_stop_event(
        &self,
        kind: &str,
        subject: &str,
        detail: &str,
        occurred_at: DateTime<Utc>,
        attribution: &str,
    ) -> Result<(), ClaimStoreError> {
        self.lock()?
            .append_stop_event(kind, subject, detail, occurred_at, "", attribution);
        Ok(())
    }

    fn record_prepare_skip(
        &self,
        claim: &TaskClaim,
        detail: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let mut state = self.lock()?;
        if state.delete_matching(claim) {
            state.append_event(
                &claim.task_id,
                &claim.worker_id,
                EVENT_RELEASED,
                claim.version,
                claim.lease_expires_at,
                now,
            );
        }
        state.append_stop_event(STOP_PREPARE_SKIP, &claim.task_id, detail, now, "", "");
        Ok(())
    }

    fn list_stop_events(&self) -> Result<Vec<OrchestratorStopEventRecord>, ClaimStoreError> {
        Ok(self.lock()?.stop_events.clone())
    }

    fn list_subject_stop_events(
        &self,
        subject: &str,
    ) -> Result<Vec<OrchestratorStopEventRecord>, ClaimStoreError> {
        Ok(self
            .lock()?
            .stop_events
            .iter()
            .filter(|e| e.subject == subject)
            .cloned()
            .collect())
    }

    fn record_session_pause(
        &self,
        reset_at: DateTime<Utc>,
        now: DateTime<Utc>,
        ceiling_seconds: f64,
        run_id: &str,
    ) -> Result<Option<DateTime<Utc>>, ClaimStoreError> {
        let Some(pause_until) = clamp_pause(reset_at, now, ceiling_seconds) else {
            return Ok(None);
        };
        let mut state = self.lock()?;
        let id = state.session_pauses.len() as i64 + 1;
        state.session_pauses.push(SessionPauseRecord {
            id,
            run_id: run_id.to_owned(),
            reset_at: to_micros(reset_at),
            pause_until,
            recorded_at: to_micros(now),
        });
        Ok(Some(pause_until))
    }

    fn session_pause_until(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, ClaimStoreError> {
        let now = to_micros(now);
        Ok(self
            .lock()?
            .session_pauses
            .iter()
            .map(|p| p.pause_until)
            .filter(|&p| p > now)
            .max())
    }

    fn list_session_pauses(&self) -> Result<Vec<SessionPauseRecord>, ClaimStoreError> {
        Ok(self.lock()?.session_pauses.clone())
    }

    fn record_human_review(
        &self,
        reason: &str,
        task_id: &str,
        occurred_at: DateTime<Utc>,
        run_id: &str,
        detail: &str,
    ) -> Result<(), ClaimStoreError> {
        require_review_reason(reason)?;
        self.lock()?
            .append_stop_event(reason, task_id, detail, occurred_at, run_id, "");
        Ok(())
    }

    fn list_human_review_queue(&self) -> Result<Vec<HumanReviewQueueEntry>, ClaimStoreError> {
        Ok(self
            .lock()?
            .stop_events
            .iter()
            .filter(|e| is_human_review_reason(&e.kind))
            .map(|e| HumanReviewQueueEntry {
                id: e.id,
                task_id: e.subject.clone(),
                run_id: e.run_id.clone(),
                reason: e.kind.clone(),
                detail: e.detail.clone(),
                occurred_at: e.occurred_at,
            })
            .collect())
    }

    fn record_graph_snapshot(
        &self,
        items: &[GraphSnapshotItem],
        captured_at: DateTime<Utc>,
    ) -> Result<GraphSnapshotRecord, ClaimStoreError> {
        require_unique_snapshot_items(items)?;
        let mut state = self.lock()?;
        let last_event_id = state.events.last().map_or(0, |e| e.id);
        let id = state.snapshots.len() as i64 + 1;
        let record = GraphSnapshotRecord {
            id,
            captured_at: to_micros(captured_at),
            item_count: items.len() as i64,
            last_event_id,
        };
        state.snapshots.push(record.clone());
        let mut rows: Vec<GraphSnapshotItem> = items.to_vec();
        rows.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        state.snapshot_items.insert(id, rows);
        Ok(record)
    }

    fn list_graph_snapshots(&self) -> Result<Vec<GraphSnapshotRecord>, ClaimStoreError> {
        Ok(self.lock()?.snapshots.clone())
    }

    fn list_graph_snapshot_items(
        &self,
        snapshot_id: i64,
    ) -> Result<Vec<GraphSnapshotItem>, ClaimStoreError> {
        Ok(self
            .lock()?
            .snapshot_items
            .get(&snapshot_id)
            .cloned()
            .unwrap_or_default())
    }

    fn latest_graph_snapshot(&self) -> Result<Option<GraphSnapshotRecord>, ClaimStoreError> {
        Ok(self.lock()?.snapshots.last().cloned())
    }

    fn upsert_work_item(
        &self,
        item: &WorkItemObservation,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let now = to_micros(now);
        let mut state = self.lock()?;
        let first_seen_at = state
            .work_items
            .get(&item.task_id)
            .map_or(now, |existing| existing.first_seen_at);
        let metadata_json = state.work_items.get(&item.task_id).map_or_else(
            || "{}".to_owned(),
            |existing| existing.metadata_json.clone(),
        );
        state.work_items.insert(
            item.task_id.clone(),
            WorkItemRecord {
                task_id: item.task_id.clone(),
                source_kind: item.source_kind.clone(),
                source_ref: item.source_ref.clone(),
                source_url: item.source_url.clone(),
                source_version: item.source_version.clone(),
                task_content_hash: Some(item.task_content_hash.clone()),
                priority: item.priority,
                required_capabilities_json: encode_str_set(&item.required_capabilities),
                conflict_keys_json: encode_str_set(&item.conflict_keys),
                first_seen_at,
                last_seen_at: now,
                disappeared_at: None,
                metadata_json,
            },
        );
        Ok(())
    }

    fn replace_work_item_dependencies(
        &self,
        task_id: &str,
        prerequisite_task_ids: &[String],
        _now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let mut state = self.lock()?;
        state.dependencies.retain(|(t, _)| t != task_id);
        for prerequisite in prerequisite_task_ids {
            state
                .dependencies
                .insert((task_id.to_owned(), prerequisite.clone()));
        }
        Ok(())
    }

    fn mark_work_items_disappeared(
        &self,
        observed_task_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let now = to_micros(now);
        let observed: BTreeSet<&String> = observed_task_ids.iter().collect();
        let mut state = self.lock()?;
        for (task_id, record) in state.work_items.iter_mut() {
            if record.disappeared_at.is_none() && !observed.contains(task_id) {
                record.disappeared_at = Some(now);
            }
        }
        Ok(())
    }

    fn load_work_item(&self, task_id: &str) -> Result<Option<WorkItemRecord>, ClaimStoreError> {
        Ok(self.lock()?.work_items.get(task_id).cloned())
    }

    fn list_work_items(&self) -> Result<Vec<WorkItemRecord>, ClaimStoreError> {
        Ok(self.lock()?.work_items.values().cloned().collect())
    }

    fn load_work_item_dependencies(&self, task_id: &str) -> Result<Vec<String>, ClaimStoreError> {
        Ok(self
            .lock()?
            .dependencies
            .iter()
            .filter(|(t, _)| t == task_id)
            .map(|(_, p)| p.clone())
            .collect())
    }

    fn list_work_item_dependencies(&self) -> Result<Vec<(String, String)>, ClaimStoreError> {
        Ok(self.lock()?.dependencies.iter().cloned().collect())
    }

    fn record_source_sync_start(
        &self,
        source_kind: &str,
        source_name: &str,
        now: DateTime<Utc>,
    ) -> Result<i64, ClaimStoreError> {
        let mut state = self.lock()?;
        let id = state.source_syncs.len() as i64 + 1;
        state.source_syncs.push(SourceSyncRecord {
            id,
            source_kind: source_kind.to_owned(),
            source_name: source_name.to_owned(),
            started_at: to_micros(now),
            finished_at: None,
            status: "running".to_owned(),
            observed_count: 0,
            error: None,
            metadata_json: "{}".to_owned(),
        });
        Ok(id)
    }

    fn record_source_sync_finish(
        &self,
        sync_id: i64,
        status: &str,
        observed_count: i64,
        error: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let mut state = self.lock()?;
        if let Some(record) = state.source_syncs.iter_mut().find(|s| s.id == sync_id) {
            record.status = status.to_owned();
            record.observed_count = observed_count;
            record.error = error.map(str::to_owned);
            record.finished_at = Some(to_micros(now));
        }
        Ok(())
    }

    fn load_source_sync(&self, sync_id: i64) -> Result<Option<SourceSyncRecord>, ClaimStoreError> {
        Ok(self
            .lock()?
            .source_syncs
            .iter()
            .find(|s| s.id == sync_id)
            .cloned())
    }

    fn list_source_syncs(&self) -> Result<Vec<SourceSyncRecord>, ClaimStoreError> {
        Ok(self.lock()?.source_syncs.clone())
    }
}

/// Refuse a snapshot that names one task twice (both backends, before any
/// row is written, so a refused snapshot leaves no header behind).
fn require_unique_snapshot_items(items: &[GraphSnapshotItem]) -> Result<(), ClaimStoreError> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for item in items {
        if !seen.insert(&item.task_id) {
            return Err(ClaimStoreError::DuplicateSnapshotItem {
                task_id: item.task_id.clone(),
            });
        }
    }
    Ok(())
}

/// `min(reset_at, now + ceiling)` when pausing is enabled and the horizon is
/// strictly after `now`; `None` otherwise. Shared by both backends so the
/// unbounded-pause failure mode is unreachable on either.
fn clamp_pause(
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
    ceiling_seconds: f64,
) -> Option<DateTime<Utc>> {
    // NaN is disabled too, matching the old saturating behavior.
    if ceiling_seconds.is_nan() || ceiling_seconds <= 0.0 {
        return None;
    }
    let reset_at = to_micros(reset_at);
    let now = to_micros(now);
    // A horizon past the timestamp range cannot bind; the reset wins.
    let pause_until = lease_expiry(now, ceiling_seconds).map_or(reset_at, |c| reset_at.min(c));
    (pause_until > now).then_some(pause_until)
}

// --- SQLite store -----------------------------------------------------------------------

/// The orchestrator DDL, verbatim from the Python module so one file serves
/// both implementations. Every statement is idempotent (`IF NOT EXISTS`);
/// the column-level additive migrations run after it in [`bootstrap`].
///
/// One addition over the Python DDL: `idx_orchestrator_stop_events_kind`,
/// which backs the human-review queue read (`WHERE kind IN (...)`) on a
/// ledger that otherwise grows without bound. It is purely additive -- the
/// Python store opens the file unchanged.
const SCHEMA_SQL: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS task_claims (
  task_id            TEXT PRIMARY KEY,
  worker_id          TEXT NOT NULL,
  claimed_at         DATETIME NOT NULL,
  lease_expires_at   DATETIME NOT NULL,
  version            INTEGER NOT NULL,
  conflict_keys_json TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS orchestrator_schema_version (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS work_items (
  task_id                    TEXT PRIMARY KEY,
  source_kind                TEXT,
  source_ref                 TEXT,
  source_url                 TEXT,
  source_version             TEXT,
  task_content_hash          TEXT,
  priority                   INTEGER NOT NULL DEFAULT 0,
  required_capabilities_json TEXT NOT NULL DEFAULT '[]',
  conflict_keys_json         TEXT NOT NULL DEFAULT '[]',
  first_seen_at              TEXT NOT NULL,
  last_seen_at               TEXT NOT NULL,
  disappeared_at             TEXT,
  metadata_json              TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS work_item_dependencies (
  task_id              TEXT NOT NULL,
  prerequisite_task_id TEXT NOT NULL,
  created_at           TEXT NOT NULL,
  PRIMARY KEY (task_id, prerequisite_task_id)
);

CREATE INDEX IF NOT EXISTS idx_work_item_dependencies_prerequisite
  ON work_item_dependencies (prerequisite_task_id);

CREATE TABLE IF NOT EXISTS source_syncs (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  source_kind    TEXT NOT NULL,
  source_name    TEXT NOT NULL,
  started_at     TEXT NOT NULL,
  finished_at    TEXT,
  status         TEXT NOT NULL,
  observed_count INTEGER NOT NULL DEFAULT 0,
  error          TEXT,
  metadata_json  TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS orchestrator_events (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id          TEXT NOT NULL,
  worker_id        TEXT NOT NULL,
  event_type       TEXT NOT NULL,
  version          INTEGER NOT NULL,
  lease_expires_at TEXT NOT NULL,
  occurred_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_events_task
  ON orchestrator_events (task_id, id);

CREATE TABLE IF NOT EXISTS orchestrator_stop_events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  kind        TEXT NOT NULL,
  subject     TEXT NOT NULL,
  detail      TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  run_id      TEXT NOT NULL DEFAULT '',
  attribution TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_subject
  ON orchestrator_stop_events (subject, id);

CREATE TABLE IF NOT EXISTS graph_snapshots (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  captured_at   TEXT NOT NULL,
  item_count    INTEGER NOT NULL,
  last_event_id INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS graph_snapshot_items (
  snapshot_id                 INTEGER NOT NULL,
  task_id                     TEXT NOT NULL,
  source_kind                 TEXT,
  source_ref                  TEXT,
  source_url                  TEXT,
  source_version              TEXT,
  priority                    INTEGER NOT NULL DEFAULT 0,
  required_capabilities_json  TEXT NOT NULL DEFAULT '[]',
  conflict_keys_json          TEXT NOT NULL DEFAULT '[]',
  state                       TEXT NOT NULL,
  ready                       INTEGER NOT NULL,
  claim_holder                TEXT,
  resolved_prerequisites_json TEXT NOT NULL DEFAULT '[]',
  PRIMARY KEY (snapshot_id, task_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_snapshot_items_snapshot
  ON graph_snapshot_items (snapshot_id);

CREATE TABLE IF NOT EXISTS orchestrator_session_pauses (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id      TEXT NOT NULL DEFAULT '',
  reset_at    TEXT NOT NULL,
  pause_until TEXT NOT NULL,
  recorded_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_session_pauses_pause_until
  ON orchestrator_session_pauses (pause_until);
";

/// The index this port adds over the Python DDL (see [`SCHEMA_SQL`]). Kept
/// as a separate statement so the shared block above stays byte-identical to
/// the Python module's.
const EXTRA_INDEX_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_kind
  ON orchestrator_stop_events (kind, id);
";

/// SQLite [`ClaimStore`]. Owns the orchestrator tables and their schema
/// sentinel; safe to point at the same file as the core `SqliteStore` (each
/// touches only its own tables).
///
/// One connection behind a mutex; every mutation runs in a `BEGIN IMMEDIATE`
/// transaction so concurrent instances on one file (several workers, or Rust
/// and Python side by side) serialize their check-and-write through SQLite's
/// own lock with a 5s busy timeout.
#[derive(Debug)]
pub struct SqliteClaimStore {
    conn: Mutex<Connection>,
}

impl SqliteClaimStore {
    /// Open (and bootstrap or migrate) the store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ClaimStoreError> {
        let conn = Connection::open(path.as_ref())?;
        with_locked_retry(|| bootstrap(&conn))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// A private in-memory database (the `":memory:"` path).
    pub fn in_memory() -> Result<Self, ClaimStoreError> {
        let conn = Connection::open_in_memory()?;
        bootstrap(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ClaimStoreError> {
        self.conn.lock().map_err(|_| ClaimStoreError::Poisoned)
    }

    /// Run `body` inside a `BEGIN IMMEDIATE` transaction: committed on `Ok`,
    /// rolled back on `Err` (so a ledger row never outlives a failed
    /// transition).
    fn write<T>(
        &self,
        mut body: impl FnMut(&Transaction<'_>) -> Result<T, ClaimStoreError>,
    ) -> Result<T, ClaimStoreError> {
        let mut conn = self.lock()?;
        with_locked_retry(|| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let value = body(&tx)?;
            tx.commit()?;
            Ok(value)
        })
    }
}

/// Transient-lock retries past `busy_timeout`, mirroring the core SQLite
/// store: `SQLITE_BUSY`/`SQLITE_LOCKED` after the busy handler gave up is
/// retried on the shared backoff schedule. Every claim-store verb is one
/// atomic unit (an immediate transaction that rolls back on error), so a
/// locked error surfaces before any row commits and re-running
/// double-applies nothing. On budget exhaustion the last locked error
/// propagates, never a fabricated success.
const MAX_TRANSIENT_RETRIES: u32 = 6;

fn is_locked(err: &ClaimStoreError) -> bool {
    let ClaimStoreError::Backend(source) = err else {
        return false;
    };
    source
        .downcast_ref::<rusqlite::Error>()
        .is_some_and(flywheel_core::faults::is_sqlite_locked)
}

fn with_locked_retry<T>(
    mut op: impl FnMut() -> Result<T, ClaimStoreError>,
) -> Result<T, ClaimStoreError> {
    let mut attempt = 0u32;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) if is_locked(&err) && attempt < MAX_TRANSIENT_RETRIES => {
                let delay = flywheel_core::faults::DEFAULT_BACKOFF.delay_for(attempt);
                std::thread::sleep(std::time::Duration::from_secs_f64(delay));
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn table_columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect()
}

/// Materialize the schema and converge an older store forward.
fn bootstrap(conn: &Connection) -> Result<(), ClaimStoreError> {
    // busy_timeout FIRST: the schema script is this connection's first
    // contended write, and N workers bootstrapping one fresh store at once
    // would otherwise hand every loser an instant "database is locked".
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.execute_batch(SCHEMA_SQL)?;
    conn.execute_batch(EXTRA_INDEX_SQL)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    // Additive v3 migration: a v1/v2 task_claims table predates the
    // conflict-keys column; add it in place.
    if !table_columns(conn, "task_claims")?.contains("conflict_keys_json") {
        conn.execute_batch(
            "ALTER TABLE task_claims ADD COLUMN conflict_keys_json TEXT NOT NULL DEFAULT '[]'",
        )?;
    }
    // Additive v7 / v9 migrations: a v6..v8 stop-events table predates the
    // run_id / attribution columns.
    let stop_columns = table_columns(conn, "orchestrator_stop_events")?;
    if !stop_columns.contains("run_id") {
        conn.execute_batch(
            "ALTER TABLE orchestrator_stop_events ADD COLUMN run_id TEXT NOT NULL DEFAULT ''",
        )?;
    }
    if !stop_columns.contains("attribution") {
        conn.execute_batch(
            "ALTER TABLE orchestrator_stop_events ADD COLUMN attribution TEXT NOT NULL DEFAULT ''",
        )?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO orchestrator_schema_version (id, version) VALUES (1, ?1)",
        params![CURRENT_ORCH_SCHEMA_VERSION],
    )?;
    let observed: Option<i64> = conn
        .query_row(
            "SELECT version FROM orchestrator_schema_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let observed = observed.and_then(|v| u32::try_from(v).ok());
    // Every table/column above was materialized additively, so an older store
    // keeps its rows and simply converges its sentinel forward. A store newer
    // than this build (or an unreadable sentinel) is refused.
    match observed {
        Some(v) if v < CURRENT_ORCH_SCHEMA_VERSION => {
            conn.execute(
                "UPDATE orchestrator_schema_version SET version = ?1 WHERE id = 1",
                params![CURRENT_ORCH_SCHEMA_VERSION],
            )?;
            Ok(())
        }
        Some(v) if v == CURRENT_ORCH_SCHEMA_VERSION => Ok(()),
        other => Err(OrchestratorSchemaError {
            observed: other,
            expected: CURRENT_ORCH_SCHEMA_VERSION,
        }
        .into()),
    }
}

fn append_event(
    tx: &Transaction<'_>,
    task_id: &str,
    worker_id: &str,
    event_type: &str,
    version: i64,
    lease_expires_at: DateTime<Utc>,
    occurred_at: DateTime<Utc>,
) -> Result<(), ClaimStoreError> {
    tx.execute(
        "INSERT INTO orchestrator_events (task_id, worker_id, event_type, version, \
         lease_expires_at, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            task_id,
            worker_id,
            event_type,
            version,
            format_timestamp(&lease_expires_at),
            format_timestamp(&occurred_at),
        ],
    )?;
    Ok(())
}

fn insert_stop_event(
    tx: &Transaction<'_>,
    kind: &str,
    subject: &str,
    detail: &str,
    occurred_at: DateTime<Utc>,
    run_id: &str,
    attribution: &str,
) -> Result<(), ClaimStoreError> {
    tx.execute(
        "INSERT INTO orchestrator_stop_events (kind, subject, detail, occurred_at, run_id, \
         attribution) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            kind,
            subject,
            detail,
            format_timestamp(&occurred_at),
            run_id,
            attribution
        ],
    )?;
    Ok(())
}

/// Delete the claim row when the token matches, returning whether it did.
fn delete_matching(tx: &Transaction<'_>, claim: &TaskClaim) -> Result<bool, ClaimStoreError> {
    let deleted = tx.execute(
        "DELETE FROM task_claims WHERE task_id = ?1 AND version = ?2 AND worker_id = ?3",
        params![claim.task_id, claim.version, claim.worker_id],
    )?;
    Ok(deleted == 1)
}

/// Whether a *different* live claim (another `task_id` whose lease has not
/// lapsed at `now`) carries conflict keys overlapping `incoming`.
///
/// Expiry is compared on parsed instants rather than on the ISO text in SQL,
/// so a naive or differently-offset timestamp written by another process can
/// never make a lapsed lease block (or a live lease pass) on string order.
fn has_conflicting_live_claim(
    tx: &Transaction<'_>,
    task_id: &str,
    incoming: &BTreeSet<String>,
    now: DateTime<Utc>,
) -> Result<bool, ClaimStoreError> {
    let mut stmt = tx.prepare(
        "SELECT lease_expires_at, conflict_keys_json FROM task_claims WHERE task_id != ?1",
    )?;
    let rows = stmt.query_map(params![task_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (expires, keys_json) = row?;
        if parse_timestamp(&expires)? <= now {
            continue;
        }
        if !disjoint(&decode_str_set(&keys_json)?, incoming) {
            return Ok(true);
        }
    }
    Ok(false)
}

const CLAIM_COLUMNS: &str = "task_id, worker_id, claimed_at, lease_expires_at, version";

fn row_to_claim(row: &rusqlite::Row<'_>) -> Result<TaskClaim, ClaimStoreError> {
    Ok(TaskClaim {
        task_id: row.get(0)?,
        worker_id: row.get(1)?,
        claimed_at: parse_timestamp(&row.get::<_, String>(2)?)?,
        lease_expires_at: parse_timestamp(&row.get::<_, String>(3)?)?,
        version: row.get(4)?,
    })
}

const EVENT_COLUMNS: &str =
    "id, task_id, worker_id, event_type, version, lease_expires_at, occurred_at";

fn row_to_event(row: &rusqlite::Row<'_>) -> Result<OrchestratorEventRecord, ClaimStoreError> {
    Ok(OrchestratorEventRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        worker_id: row.get(2)?,
        event_type: row.get(3)?,
        version: row.get(4)?,
        lease_expires_at: parse_timestamp(&row.get::<_, String>(5)?)?,
        occurred_at: parse_timestamp(&row.get::<_, String>(6)?)?,
    })
}

const STOP_COLUMNS: &str = "id, kind, subject, detail, occurred_at, run_id, attribution";

fn row_to_stop_event(
    row: &rusqlite::Row<'_>,
) -> Result<OrchestratorStopEventRecord, ClaimStoreError> {
    Ok(OrchestratorStopEventRecord {
        id: row.get(0)?,
        kind: row.get(1)?,
        subject: row.get(2)?,
        detail: row.get(3)?,
        occurred_at: parse_timestamp(&row.get::<_, String>(4)?)?,
        run_id: row.get(5)?,
        attribution: row.get(6)?,
    })
}

const SNAPSHOT_COLUMNS: &str = "id, captured_at, item_count, last_event_id";

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> Result<GraphSnapshotRecord, ClaimStoreError> {
    Ok(GraphSnapshotRecord {
        id: row.get(0)?,
        captured_at: parse_timestamp(&row.get::<_, String>(1)?)?,
        item_count: row.get(2)?,
        last_event_id: row.get(3)?,
    })
}

const SNAPSHOT_ITEM_COLUMNS: &str = "task_id, source_kind, source_ref, source_url, \
     source_version, priority, required_capabilities_json, conflict_keys_json, state, ready, \
     claim_holder, resolved_prerequisites_json";

fn row_to_snapshot_item(row: &rusqlite::Row<'_>) -> Result<GraphSnapshotItem, ClaimStoreError> {
    Ok(GraphSnapshotItem {
        task_id: row.get(0)?,
        source_kind: row.get(1)?,
        source_ref: row.get(2)?,
        source_url: row.get(3)?,
        source_version: row.get(4)?,
        priority: row.get(5)?,
        required_capabilities: decode_str_set(&row.get::<_, String>(6)?)?,
        conflict_keys: decode_str_set(&row.get::<_, String>(7)?)?,
        state: row.get(8)?,
        ready: row.get::<_, i64>(9)? != 0,
        claim_holder: row.get(10)?,
        resolved_prerequisites: decode_str_set(&row.get::<_, String>(11)?)?,
    })
}

const WORK_ITEM_COLUMNS: &str = "task_id, source_kind, source_ref, source_url, source_version, \
     task_content_hash, priority, required_capabilities_json, conflict_keys_json, first_seen_at, \
     last_seen_at, disappeared_at, metadata_json";

fn row_to_work_item(row: &rusqlite::Row<'_>) -> Result<WorkItemRecord, ClaimStoreError> {
    let disappeared: Option<String> = row.get(11)?;
    Ok(WorkItemRecord {
        task_id: row.get(0)?,
        source_kind: row.get(1)?,
        source_ref: row.get(2)?,
        source_url: row.get(3)?,
        source_version: row.get(4)?,
        task_content_hash: row.get(5)?,
        priority: row.get(6)?,
        required_capabilities_json: row.get(7)?,
        conflict_keys_json: row.get(8)?,
        first_seen_at: parse_timestamp(&row.get::<_, String>(9)?)?,
        last_seen_at: parse_timestamp(&row.get::<_, String>(10)?)?,
        disappeared_at: disappeared.as_deref().map(parse_timestamp).transpose()?,
        metadata_json: row.get(12)?,
    })
}

const SOURCE_SYNC_COLUMNS: &str = "id, source_kind, source_name, started_at, finished_at, \
     status, observed_count, error, metadata_json";

fn row_to_source_sync(row: &rusqlite::Row<'_>) -> Result<SourceSyncRecord, ClaimStoreError> {
    let finished: Option<String> = row.get(4)?;
    Ok(SourceSyncRecord {
        id: row.get(0)?,
        source_kind: row.get(1)?,
        source_name: row.get(2)?,
        started_at: parse_timestamp(&row.get::<_, String>(3)?)?,
        finished_at: finished.as_deref().map(parse_timestamp).transpose()?,
        status: row.get(5)?,
        observed_count: row.get(6)?,
        error: row.get(7)?,
        metadata_json: row.get(8)?,
    })
}

fn row_to_pause(row: &rusqlite::Row<'_>) -> Result<SessionPauseRecord, ClaimStoreError> {
    Ok(SessionPauseRecord {
        id: row.get(0)?,
        run_id: row.get(1)?,
        reset_at: parse_timestamp(&row.get::<_, String>(2)?)?,
        pause_until: parse_timestamp(&row.get::<_, String>(3)?)?,
        recorded_at: parse_timestamp(&row.get::<_, String>(4)?)?,
    })
}

/// Collect every row of a prepared query through a fallible mapper.
fn query_all<T>(
    conn: &Connection,
    sql: &str,
    bind: impl rusqlite::Params,
    map: fn(&rusqlite::Row<'_>) -> Result<T, ClaimStoreError>,
) -> Result<Vec<T>, ClaimStoreError> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(bind)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(map(row)?);
    }
    Ok(out)
}

fn query_one<T>(
    conn: &Connection,
    sql: &str,
    bind: impl rusqlite::Params,
    map: fn(&rusqlite::Row<'_>) -> Result<T, ClaimStoreError>,
) -> Result<Option<T>, ClaimStoreError> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(bind)?;
    match rows.next()? {
        Some(row) => Ok(Some(map(row)?)),
        None => Ok(None),
    }
}

impl ClaimStore for SqliteClaimStore {
    fn acquire_claim(
        &self,
        task_id: &str,
        worker_id: &str,
        now: DateTime<Utc>,
        lease_seconds: f64,
        conflict_keys: &BTreeSet<String>,
    ) -> Result<Option<TaskClaim>, ClaimStoreError> {
        let now = to_micros(now);
        let lease_expires = checked_lease_expiry(now, lease_seconds)?;
        let keys_json = encode_str_set(conflict_keys);
        self.write(|tx| {
            let existing: Option<(String, String, i64)> = tx
                .query_row(
                    "SELECT worker_id, lease_expires_at, version FROM task_claims \
                     WHERE task_id = ?1",
                    params![task_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((holder, expires, _)) = &existing
                && parse_timestamp(expires)? > now
                && holder != worker_id
            {
                return Ok(None);
            }
            if !conflict_keys.is_empty()
                && has_conflicting_live_claim(tx, task_id, conflict_keys, now)?
            {
                return Ok(None);
            }
            let (version, event_type) = match &existing {
                None => (1, EVENT_ACQUIRED),
                Some((holder, _, version)) if holder == worker_id => (version + 1, EVENT_ACQUIRED),
                // A surviving different-worker row is necessarily lapsed: the
                // live-different-worker case returned above.
                Some((_, _, version)) => (version + 1, EVENT_STOLEN),
            };
            if existing.is_none() {
                tx.execute(
                    "INSERT INTO task_claims (task_id, worker_id, claimed_at, lease_expires_at, \
                     version, conflict_keys_json) VALUES (?1, ?2, ?3, ?4, 1, ?5)",
                    params![
                        task_id,
                        worker_id,
                        format_timestamp(&now),
                        format_timestamp(&lease_expires),
                        keys_json,
                    ],
                )?;
            } else {
                tx.execute(
                    "UPDATE task_claims SET worker_id = ?1, claimed_at = ?2, \
                     lease_expires_at = ?3, version = ?4, conflict_keys_json = ?5 \
                     WHERE task_id = ?6",
                    params![
                        worker_id,
                        format_timestamp(&now),
                        format_timestamp(&lease_expires),
                        version,
                        keys_json,
                        task_id,
                    ],
                )?;
            }
            append_event(
                tx,
                task_id,
                worker_id,
                event_type,
                version,
                lease_expires,
                now,
            )?;
            Ok(Some(TaskClaim {
                task_id: task_id.to_owned(),
                worker_id: worker_id.to_owned(),
                claimed_at: now,
                lease_expires_at: lease_expires,
                version,
            }))
        })
    }

    fn renew_claim(
        &self,
        claim: &TaskClaim,
        now: DateTime<Utc>,
        lease_seconds: f64,
    ) -> Result<TaskClaim, ClaimStoreError> {
        let now = to_micros(now);
        let lease_expires = checked_lease_expiry(now, lease_seconds)?;
        let new_version = claim.version + 1;
        // The UPDATE and its `renewed` event commit together: a stale token
        // updates no row, the error rolls the transaction back, no event.
        self.write(|tx| {
            let updated = tx.execute(
                "UPDATE task_claims SET lease_expires_at = ?1, version = ?2 \
                 WHERE task_id = ?3 AND version = ?4 AND worker_id = ?5",
                params![
                    format_timestamp(&lease_expires),
                    new_version,
                    claim.task_id,
                    claim.version,
                    claim.worker_id,
                ],
            )?;
            if updated != 1 {
                return Err(ClaimLostError {
                    task_id: claim.task_id.clone(),
                }
                .into());
            }
            append_event(
                tx,
                &claim.task_id,
                &claim.worker_id,
                EVENT_RENEWED,
                new_version,
                lease_expires,
                now,
            )?;
            Ok(TaskClaim {
                task_id: claim.task_id.clone(),
                worker_id: claim.worker_id.clone(),
                claimed_at: claim.claimed_at,
                lease_expires_at: lease_expires,
                version: new_version,
            })
        })
    }

    fn release_claim(
        &self,
        claim: &TaskClaim,
        now: Option<DateTime<Utc>>,
    ) -> Result<(), ClaimStoreError> {
        self.write(|tx| {
            if delete_matching(tx, claim)? {
                append_event(
                    tx,
                    &claim.task_id,
                    &claim.worker_id,
                    EVENT_RELEASED,
                    claim.version,
                    claim.lease_expires_at,
                    now.unwrap_or(claim.lease_expires_at),
                )?;
            }
            Ok(())
        })
    }

    fn load_claim(&self, task_id: &str) -> Result<Option<TaskClaim>, ClaimStoreError> {
        let conn = self.lock()?;
        query_one(
            &conn,
            &format!("SELECT {CLAIM_COLUMNS} FROM task_claims WHERE task_id = ?1"),
            params![task_id],
            row_to_claim,
        )
    }

    fn list_claims(&self) -> Result<Vec<TaskClaim>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {CLAIM_COLUMNS} FROM task_claims ORDER BY task_id"),
            [],
            row_to_claim,
        )
    }

    fn sweep_expired_claims(&self, now: DateTime<Utc>) -> Result<Vec<String>, ClaimStoreError> {
        let now = to_micros(now);
        // Select every row, decide lapse on the parsed instant (never on ISO
        // string order), and delete the lapsed ones by key inside the same
        // IMMEDIATE transaction -- one `expired` event per reaped claim.
        self.write(|tx| {
            let mut stmt = tx.prepare(&format!(
                "SELECT {CLAIM_COLUMNS} FROM task_claims ORDER BY task_id"
            ))?;
            let mut rows = stmt.query([])?;
            let mut doomed = Vec::new();
            while let Some(row) = rows.next()? {
                let claim = row_to_claim(row)?;
                if claim.lease_expires_at <= now {
                    doomed.push(claim);
                }
            }
            drop(rows);
            drop(stmt);
            for claim in &doomed {
                tx.execute(
                    "DELETE FROM task_claims WHERE task_id = ?1",
                    params![claim.task_id],
                )?;
                append_event(
                    tx,
                    &claim.task_id,
                    &claim.worker_id,
                    EVENT_EXPIRED,
                    claim.version,
                    claim.lease_expires_at,
                    now,
                )?;
            }
            Ok(doomed.into_iter().map(|c| c.task_id).collect())
        })
    }

    fn list_events(&self) -> Result<Vec<OrchestratorEventRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {EVENT_COLUMNS} FROM orchestrator_events ORDER BY id"),
            [],
            row_to_event,
        )
    }

    fn list_task_events(
        &self,
        task_id: &str,
    ) -> Result<Vec<OrchestratorEventRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!(
                "SELECT {EVENT_COLUMNS} FROM orchestrator_events WHERE task_id = ?1 ORDER BY id"
            ),
            params![task_id],
            row_to_event,
        )
    }

    fn record_stop_event(
        &self,
        kind: &str,
        subject: &str,
        detail: &str,
        occurred_at: DateTime<Utc>,
        attribution: &str,
    ) -> Result<(), ClaimStoreError> {
        self.write(|tx| insert_stop_event(tx, kind, subject, detail, occurred_at, "", attribution))
    }

    fn record_prepare_skip(
        &self,
        claim: &TaskClaim,
        detail: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        self.write(|tx| {
            if delete_matching(tx, claim)? {
                append_event(
                    tx,
                    &claim.task_id,
                    &claim.worker_id,
                    EVENT_RELEASED,
                    claim.version,
                    claim.lease_expires_at,
                    now,
                )?;
            }
            insert_stop_event(tx, STOP_PREPARE_SKIP, &claim.task_id, detail, now, "", "")
        })
    }

    fn list_stop_events(&self) -> Result<Vec<OrchestratorStopEventRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {STOP_COLUMNS} FROM orchestrator_stop_events ORDER BY id"),
            [],
            row_to_stop_event,
        )
    }

    fn list_subject_stop_events(
        &self,
        subject: &str,
    ) -> Result<Vec<OrchestratorStopEventRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!(
                "SELECT {STOP_COLUMNS} FROM orchestrator_stop_events WHERE subject = ?1 \
                 ORDER BY id"
            ),
            params![subject],
            row_to_stop_event,
        )
    }

    fn record_session_pause(
        &self,
        reset_at: DateTime<Utc>,
        now: DateTime<Utc>,
        ceiling_seconds: f64,
        run_id: &str,
    ) -> Result<Option<DateTime<Utc>>, ClaimStoreError> {
        let Some(pause_until) = clamp_pause(reset_at, now, ceiling_seconds) else {
            return Ok(None);
        };
        self.write(|tx| {
            tx.execute(
                "INSERT INTO orchestrator_session_pauses (run_id, reset_at, pause_until, \
                 recorded_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    run_id,
                    format_timestamp(&reset_at),
                    format_timestamp(&pause_until),
                    format_timestamp(&now),
                ],
            )?;
            Ok(())
        })?;
        Ok(Some(pause_until))
    }

    fn session_pause_until(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, ClaimStoreError> {
        let now = to_micros(now);
        // Index-friendly prefilter on the date prefix (every persisted format
        // starts `YYYY-MM-DD`; a day of slack covers any offset), then the
        // real comparison on parsed instants.
        let floor = format_timestamp(&(now - Duration::days(1)));
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT pause_until FROM orchestrator_session_pauses WHERE pause_until >= ?1",
        )?;
        let mut rows = stmt.query(params![&floor[..10]])?;
        let mut horizon: Option<DateTime<Utc>> = None;
        while let Some(row) = rows.next()? {
            let candidate = parse_timestamp(&row.get::<_, String>(0)?)?;
            if candidate > now && horizon.is_none_or(|h| candidate > h) {
                horizon = Some(candidate);
            }
        }
        Ok(horizon)
    }

    fn list_session_pauses(&self) -> Result<Vec<SessionPauseRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            "SELECT id, run_id, reset_at, pause_until, recorded_at \
             FROM orchestrator_session_pauses ORDER BY id",
            [],
            row_to_pause,
        )
    }

    fn record_human_review(
        &self,
        reason: &str,
        task_id: &str,
        occurred_at: DateTime<Utc>,
        run_id: &str,
        detail: &str,
    ) -> Result<(), ClaimStoreError> {
        require_review_reason(reason)?;
        self.write(|tx| insert_stop_event(tx, reason, task_id, detail, occurred_at, run_id, ""))
    }

    fn list_human_review_queue(&self) -> Result<Vec<HumanReviewQueueEntry>, ClaimStoreError> {
        let placeholders = (1..=HUMAN_REVIEW_QUEUE_REASONS.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut reasons: Vec<&str> = HUMAN_REVIEW_QUEUE_REASONS.to_vec();
        reasons.sort_unstable();
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT id, kind, subject, detail, occurred_at, run_id \
             FROM orchestrator_stop_events WHERE kind IN ({placeholders}) ORDER BY id"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(reasons.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(HumanReviewQueueEntry {
                id: row.get(0)?,
                reason: row.get(1)?,
                task_id: row.get(2)?,
                detail: row.get(3)?,
                occurred_at: parse_timestamp(&row.get::<_, String>(4)?)?,
                run_id: row.get(5)?,
            });
        }
        Ok(out)
    }

    fn record_graph_snapshot(
        &self,
        items: &[GraphSnapshotItem],
        captured_at: DateTime<Utc>,
    ) -> Result<GraphSnapshotRecord, ClaimStoreError> {
        require_unique_snapshot_items(items)?;
        let captured_at = to_micros(captured_at);
        self.write(|tx| {
            let last_event_id: i64 = tx.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM orchestrator_events",
                [],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO graph_snapshots (captured_at, item_count, last_event_id) \
                 VALUES (?1, ?2, ?3)",
                params![
                    format_timestamp(&captured_at),
                    items.len() as i64,
                    last_event_id
                ],
            )?;
            let snapshot_id = tx.last_insert_rowid();
            let mut insert = tx.prepare(
                "INSERT INTO graph_snapshot_items (snapshot_id, task_id, source_kind, \
                 source_ref, source_url, source_version, priority, required_capabilities_json, \
                 conflict_keys_json, state, ready, claim_holder, resolved_prerequisites_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for item in items {
                insert.execute(params![
                    snapshot_id,
                    item.task_id,
                    item.source_kind,
                    item.source_ref,
                    item.source_url,
                    item.source_version,
                    item.priority,
                    encode_str_set(&item.required_capabilities),
                    encode_str_set(&item.conflict_keys),
                    item.state,
                    i64::from(item.ready),
                    item.claim_holder,
                    encode_str_set(&item.resolved_prerequisites),
                ])?;
            }
            Ok(GraphSnapshotRecord {
                id: snapshot_id,
                captured_at,
                item_count: items.len() as i64,
                last_event_id,
            })
        })
    }

    fn list_graph_snapshots(&self) -> Result<Vec<GraphSnapshotRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {SNAPSHOT_COLUMNS} FROM graph_snapshots ORDER BY id"),
            [],
            row_to_snapshot,
        )
    }

    fn list_graph_snapshot_items(
        &self,
        snapshot_id: i64,
    ) -> Result<Vec<GraphSnapshotItem>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!(
                "SELECT {SNAPSHOT_ITEM_COLUMNS} FROM graph_snapshot_items \
                 WHERE snapshot_id = ?1 ORDER BY task_id"
            ),
            params![snapshot_id],
            row_to_snapshot_item,
        )
    }

    fn latest_graph_snapshot(&self) -> Result<Option<GraphSnapshotRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_one(
            &conn,
            &format!("SELECT {SNAPSHOT_COLUMNS} FROM graph_snapshots ORDER BY id DESC LIMIT 1"),
            [],
            row_to_snapshot,
        )
    }

    fn upsert_work_item(
        &self,
        item: &WorkItemObservation,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let now_text = format_timestamp(&now);
        self.write(|tx| {
            tx.execute(
                "INSERT INTO work_items (task_id, source_kind, source_ref, source_url, \
                 source_version, task_content_hash, priority, required_capabilities_json, \
                 conflict_keys_json, first_seen_at, last_seen_at, disappeared_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL) \
                 ON CONFLICT(task_id) DO UPDATE SET \
                 source_kind = excluded.source_kind, source_ref = excluded.source_ref, \
                 source_url = excluded.source_url, source_version = excluded.source_version, \
                 task_content_hash = excluded.task_content_hash, priority = excluded.priority, \
                 required_capabilities_json = excluded.required_capabilities_json, \
                 conflict_keys_json = excluded.conflict_keys_json, \
                 last_seen_at = excluded.last_seen_at, disappeared_at = NULL",
                params![
                    item.task_id,
                    item.source_kind,
                    item.source_ref,
                    item.source_url,
                    item.source_version,
                    item.task_content_hash,
                    item.priority,
                    encode_str_set(&item.required_capabilities),
                    encode_str_set(&item.conflict_keys),
                    now_text,
                    now_text,
                ],
            )?;
            Ok(())
        })
    }

    fn replace_work_item_dependencies(
        &self,
        task_id: &str,
        prerequisite_task_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let now_text = format_timestamp(&now);
        let unique: BTreeSet<&String> = prerequisite_task_ids.iter().collect();
        self.write(|tx| {
            tx.execute(
                "DELETE FROM work_item_dependencies WHERE task_id = ?1",
                params![task_id],
            )?;
            let mut insert = tx.prepare(
                "INSERT INTO work_item_dependencies (task_id, prerequisite_task_id, created_at) \
                 VALUES (?1, ?2, ?3)",
            )?;
            for prerequisite in &unique {
                insert.execute(params![task_id, prerequisite, now_text])?;
            }
            Ok(())
        })
    }

    fn mark_work_items_disappeared(
        &self,
        observed_task_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let now_text = format_timestamp(&now);
        let observed: BTreeSet<&String> = observed_task_ids.iter().collect();
        self.write(|tx| {
            if observed.is_empty() {
                tx.execute(
                    "UPDATE work_items SET disappeared_at = ?1 WHERE disappeared_at IS NULL",
                    params![now_text],
                )?;
                return Ok(());
            }
            let placeholders = (2..observed.len() + 2)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&now_text];
            bind.extend(observed.iter().map(|s| *s as &dyn rusqlite::ToSql));
            tx.execute(
                &format!(
                    "UPDATE work_items SET disappeared_at = ?1 WHERE disappeared_at IS NULL \
                     AND task_id NOT IN ({placeholders})"
                ),
                bind.as_slice(),
            )?;
            Ok(())
        })
    }

    fn load_work_item(&self, task_id: &str) -> Result<Option<WorkItemRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_one(
            &conn,
            &format!("SELECT {WORK_ITEM_COLUMNS} FROM work_items WHERE task_id = ?1"),
            params![task_id],
            row_to_work_item,
        )
    }

    fn list_work_items(&self) -> Result<Vec<WorkItemRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {WORK_ITEM_COLUMNS} FROM work_items ORDER BY task_id"),
            [],
            row_to_work_item,
        )
    }

    fn load_work_item_dependencies(&self, task_id: &str) -> Result<Vec<String>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            "SELECT prerequisite_task_id FROM work_item_dependencies WHERE task_id = ?1 \
             ORDER BY prerequisite_task_id",
            params![task_id],
            |row| Ok(row.get(0)?),
        )
    }

    fn list_work_item_dependencies(&self) -> Result<Vec<(String, String)>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            "SELECT task_id, prerequisite_task_id FROM work_item_dependencies \
             ORDER BY task_id, prerequisite_task_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    fn record_source_sync_start(
        &self,
        source_kind: &str,
        source_name: &str,
        now: DateTime<Utc>,
    ) -> Result<i64, ClaimStoreError> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO source_syncs (source_kind, source_name, started_at, status, \
                 observed_count) VALUES (?1, ?2, ?3, 'running', 0)",
                params![source_kind, source_name, format_timestamp(&now)],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    fn record_source_sync_finish(
        &self,
        sync_id: i64,
        status: &str,
        observed_count: i64,
        error: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        self.write(|tx| {
            tx.execute(
                "UPDATE source_syncs SET status = ?1, observed_count = ?2, error = ?3, \
                 finished_at = ?4 WHERE id = ?5",
                params![
                    status,
                    observed_count,
                    error,
                    format_timestamp(&now),
                    sync_id
                ],
            )?;
            Ok(())
        })
    }

    fn load_source_sync(&self, sync_id: i64) -> Result<Option<SourceSyncRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_one(
            &conn,
            &format!("SELECT {SOURCE_SYNC_COLUMNS} FROM source_syncs WHERE id = ?1"),
            params![sync_id],
            row_to_source_sync,
        )
    }

    fn list_source_syncs(&self) -> Result<Vec<SourceSyncRecord>, ClaimStoreError> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {SOURCE_SYNC_COLUMNS} FROM source_syncs ORDER BY id"),
            [],
            row_to_source_sync,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use flywheel_core::events::LANDING_PARK_KINDS;

    fn t(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 28, 12, 0, second).unwrap()
    }

    #[test]
    fn schema_version_matches_python_sentinel() {
        assert_eq!(CURRENT_ORCH_SCHEMA_VERSION, 9);
    }

    #[test]
    fn human_review_reasons_are_the_six_tokens_plus_every_park_kind() {
        let reasons: BTreeSet<&str> = HUMAN_REVIEW_QUEUE_REASONS.into_iter().collect();
        assert_eq!(reasons.len(), HUMAN_REVIEW_QUEUE_REASONS.len());
        for kind in LANDING_PARK_KINDS {
            assert!(reasons.contains(kind), "park kind {kind} missing");
        }
        for kind in ORCHESTRATOR_STOP_EVENT_KINDS {
            assert!(
                !reasons.contains(kind),
                "stop kind {kind} leaked into reasons"
            );
        }
        assert_eq!(reasons.len(), 6 + LANDING_PARK_KINDS.len());
    }

    #[test]
    fn event_types_are_five_distinct_tokens() {
        let unique: BTreeSet<&str> = ORCHESTRATOR_EVENT_TYPES.into_iter().collect();
        assert_eq!(unique.len(), 5);
    }

    #[test]
    fn encode_str_set_matches_python_json_dumps_sorted() {
        let keys = conflict_keys(["linux", "gpu", "caf\u{e9}"]);
        assert_eq!(
            encode_str_set(&keys),
            "[\"caf\\u00e9\", \"gpu\", \"linux\"]"
        );
        assert_eq!(encode_str_set(&BTreeSet::new()), "[]");
        assert_eq!(
            decode_str_set("[\"b\", \"a\"]").unwrap(),
            conflict_keys(["a", "b"])
        );
    }

    #[test]
    fn timestamps_round_trip_through_python_isoformat_text() {
        let ts = t(5);
        assert_eq!(format_timestamp(&ts), "2026-05-28T12:00:05+00:00");
        assert_eq!(parse_timestamp("2026-05-28T12:00:05+00:00").unwrap(), ts);
        let with_micros = ts + Duration::microseconds(123_456);
        assert_eq!(
            format_timestamp(&with_micros),
            "2026-05-28T12:00:05.123456+00:00"
        );
    }

    #[test]
    fn lease_duration_handles_fractional_seconds() {
        assert_eq!(lease_duration(0.5), Duration::milliseconds(500));
        assert_eq!(lease_duration(30.0), Duration::seconds(30));
    }

    #[test]
    fn clamp_pause_with_an_unrepresentable_ceiling_falls_back_to_reset_at() {
        // A ceiling whose horizon is past the timestamp range cannot bind, so
        // the reported reset wins; it must never overflow `now + ceiling`.
        assert_eq!(clamp_pause(t(50), t(0), f64::INFINITY), Some(t(50)));
        assert_eq!(clamp_pause(t(50), t(0), 1e30), Some(t(50)));
        assert_eq!(clamp_pause(t(50), t(0), f64::NAN), None);
        assert_eq!(clamp_pause(t(50), t(0), -5.0), None);
    }

    #[test]
    fn unrepresentable_lease_is_refused_without_recording_a_claim() {
        let mem = InMemoryClaimStore::new();
        let sqlite = SqliteClaimStore::in_memory().unwrap();
        let stores: [&dyn ClaimStore; 2] = [&mem, &sqlite];
        for store in stores {
            for lease in [1e30, f64::INFINITY, f64::NAN] {
                let err = store
                    .acquire_claim("task-a", "worker-1", t(0), lease, &BTreeSet::new())
                    .unwrap_err();
                assert!(
                    matches!(err, ClaimStoreError::LeaseOutOfRange { .. }),
                    "{err}"
                );
            }
            assert!(store.load_claim("task-a").unwrap().is_none());
            let claim = store
                .acquire_claim("task-a", "worker-1", t(0), 30.0, &BTreeSet::new())
                .unwrap()
                .unwrap();
            let err = store.renew_claim(&claim, t(1), 1e30).unwrap_err();
            assert!(
                matches!(err, ClaimStoreError::LeaseOutOfRange { .. }),
                "{err}"
            );
            assert_eq!(store.load_claim("task-a").unwrap().unwrap(), claim);
        }
    }

    #[test]
    fn to_micros_truncates_nanoseconds() {
        let ts = t(0) + Duration::nanoseconds(1_234_567);
        assert_eq!(to_micros(ts), t(0) + Duration::microseconds(1_234));
    }

    #[test]
    fn clamp_pause_honours_ceiling_and_past_reset() {
        assert_eq!(clamp_pause(t(50), t(0), 30.0), Some(t(30)));
        assert_eq!(clamp_pause(t(10), t(0), 30.0), Some(t(10)));
        assert_eq!(clamp_pause(t(0), t(5), 30.0), None);
        assert_eq!(clamp_pause(t(50), t(0), 0.0), None);
    }

    #[test]
    fn schema_error_message_matches_python_shape() {
        let err = OrchestratorSchemaError {
            observed: Some(12),
            expected: CURRENT_ORCH_SCHEMA_VERSION,
        };
        assert_eq!(
            err.to_string(),
            "orchestrator store must be re-created: observed orchestrator_schema_version=12, \
             expected 9"
        );
        let missing = OrchestratorSchemaError {
            observed: None,
            expected: 9,
        };
        assert!(missing.to_string().contains("version=None"));
    }

    #[test]
    fn sqlite_store_refuses_a_newer_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE orchestrator_schema_version (id INTEGER PRIMARY KEY CHECK (id = 1), \
                 version INTEGER NOT NULL); \
                 INSERT INTO orchestrator_schema_version (id, version) VALUES (1, 99);",
            )
            .unwrap();
        }
        let err = SqliteClaimStore::open(&path).unwrap_err();
        assert!(err.is_schema_mismatch(), "{err}");
        match err {
            ClaimStoreError::Schema(schema) => {
                assert_eq!(schema.observed, Some(99));
                assert_eq!(schema.expected, CURRENT_ORCH_SCHEMA_VERSION);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sqlite_store_coexists_with_foreign_tables_in_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE lifecycles (run_id TEXT PRIMARY KEY); \
                 CREATE TABLE schema_version (id INTEGER PRIMARY KEY, version INTEGER NOT NULL); \
                 INSERT INTO schema_version VALUES (1, 12); \
                 INSERT INTO lifecycles VALUES ('run-1');",
            )
            .unwrap();
        }
        let store = SqliteClaimStore::open(&path).unwrap();
        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &BTreeSet::new())
            .unwrap();
        assert!(claim.is_some());
        drop(store);
        let conn = Connection::open(&path).unwrap();
        let core_version: i64 = conn
            .query_row("SELECT version FROM schema_version WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(core_version, 12);
        let runs: i64 = conn
            .query_row("SELECT COUNT(*) FROM lifecycles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runs, 1);
        let orch_version: i64 = conn
            .query_row(
                "SELECT version FROM orchestrator_schema_version WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orch_version, i64::from(CURRENT_ORCH_SCHEMA_VERSION));
    }

    #[test]
    fn sqlite_lease_text_is_python_isoformat() {
        let store = SqliteClaimStore::in_memory().unwrap();
        store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &conflict_keys(["k"]))
            .unwrap()
            .unwrap();
        let conn = store.lock().unwrap();
        let (claimed, expires, keys): (String, String, String) = conn
            .query_row(
                "SELECT claimed_at, lease_expires_at, conflict_keys_json FROM task_claims",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(claimed, "2026-05-28T12:00:00+00:00");
        assert_eq!(expires, "2026-05-28T12:00:30+00:00");
        assert_eq!(keys, "[\"k\"]");
    }

    #[test]
    fn sqlite_expiry_is_decided_on_instants_not_string_order() {
        // A naive timestamp (Python's naive isoformat) sorts lexically BEFORE
        // an aware one at the same instant and AFTER a shorter aware one at
        // an earlier second; both orderings must be decided chronologically.
        let store = SqliteClaimStore::in_memory().unwrap();
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "INSERT INTO task_claims (task_id, worker_id, claimed_at, lease_expires_at, \
                 version, conflict_keys_json) VALUES ('held', 'w1', '2026-05-28T12:00:00', \
                 '2026-05-28T12:00:30', 1, '[\"k\"]')",
                [],
            )
            .unwrap();
        }
        // Live at :10 -> conflict refusal.
        assert!(
            store
                .acquire_claim("other", "w2", t(10), 30.0, &conflict_keys(["k"]))
                .unwrap()
                .is_none()
        );
        // Not yet lapsed at :10 -> sweep leaves it.
        assert!(store.sweep_expired_claims(t(10)).unwrap().is_empty());
        // Lapsed at :30 (<= now) -> sweep reaps it.
        assert_eq!(store.sweep_expired_claims(t(30)).unwrap(), vec!["held"]);
    }

    #[test]
    fn work_item_observation_for_task_carries_the_digest() {
        let task = Task::new("do the thing", Vec::new());
        let observation = WorkItemObservation::for_task(&task);
        assert_eq!(observation.task_id, task.id);
        assert_eq!(observation.task_content_hash, task_digest(&task));
        assert_eq!(observation.priority, 0);
    }
}
