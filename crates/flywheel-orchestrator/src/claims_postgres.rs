//! Port of the Python `flywheel_orchestrator._claims_postgres` module.
//!
//! The Postgres [`ClaimStore`]: the durable, network-addressable backend for
//! the orchestrator's leases, ledgers, and WorkGraph catalog. Mirrors
//! [`flywheel_core::store_postgres::PostgresStore`] operationally -- one
//! connection behind a mutex, a validated schema pinned on the session
//! `search_path`, an advisory-locked bootstrap -- but owns only the
//! orchestrator tables and its own `orchestrator_schema_version` sentinel, so
//! it can share a database (under the same or a different schema) with the
//! core store without either touching the other's tables.
//!
//! The DDL is the Python module's verbatim: timestamps are `TIMESTAMPTZ`
//! (compared temporally, never as text), `*_json` columns are `JSONB`, ids
//! are `BIGSERIAL`. A schema bootstrapped by either implementation opens in
//! the other, and every row either side writes reads back through the other:
//! mixed Rust and Python workers can share one database and agree on lease
//! expiry, ledger order, and catalog contents.
//!
//! Concurrency model: `acquire_claim` takes a transaction-scoped advisory
//! lock ([`CLAIM_ACQUIRE_LOCK_KEY`], the Postgres analog of SQLite's `BEGIN
//! IMMEDIATE`) so the conflict-key check and the `ON CONFLICT` upsert are one
//! serialized check-and-write across every connection on the store; the
//! upsert's `WHERE` is the optimistic-concurrency guard (a live lease held by
//! a different worker updates no row and returns `None`). Every ledger row
//! commits in the same transaction as the `task_claims` change it describes.
//!
//! Departures from the Python store, all interop-preserving (storage and
//! semantics are unchanged; only the Rust side's queries differ):
//!
//! * conflict-key overlap is decided in SQL with the JSONB `?|` operator
//!   instead of fetching every live row's keys and intersecting client-side;
//! * `sweep_expired_claims` reaps and journals in one statement (a
//!   data-modifying CTE) instead of one `INSERT` round trip per lapsed claim;
//! * snapshot items and dependency edges are bulk-inserted through `unnest`
//!   instead of one `INSERT` per row;
//! * `list_claims` and `list_work_items` are ordered by `task_id`, as the
//!   trait documents (the Python store emits heap order);
//! * `*_json` set columns read back as [`encode_str_set`] canonical text
//!   (ASCII-escaped, as the SQLite and in-memory stores return) rather than
//!   the server's JSONB rendering;
//! * the additive column migrations are guarded by a catalog read instead of
//!   `ADD COLUMN IF NOT EXISTS`, which takes an `ACCESS EXCLUSIVE` lock on
//!   the table even when it then no-ops;
//! * `idx_orchestrator_stop_events_kind` backs the human-review queue read,
//!   matching the index the SQLite port adds.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Duration, Timelike, Utc};
use postgres::types::{FromSql, ToSql};
use postgres::{Client, GenericClient, NoTls, Row, Transaction};
use serde_json::Value;

use flywheel_core::store::HarnessStore;
use flywheel_core::store_postgres::{
    BOOTSTRAP_LOCK_KEY, DEFAULT_SCHEMA, IntegerOutOfRange, PostgresError, PostgresStore,
    validate_schema,
};

use crate::claims::{
    CURRENT_ORCH_SCHEMA_VERSION, ClaimLostError, ClaimStore, ClaimStoreError, EVENT_ACQUIRED,
    EVENT_EXPIRED, EVENT_RELEASED, EVENT_RENEWED, EVENT_STOLEN, GraphSnapshotItem,
    GraphSnapshotRecord, HUMAN_REVIEW_QUEUE_REASONS, HumanReviewQueueEntry,
    OrchestratorEventRecord, OrchestratorSchemaError, OrchestratorStopEventRecord,
    STOP_PREPARE_SKIP, SessionPauseRecord, SourceSyncRecord, TaskClaim, WorkItemObservation,
    WorkItemRecord, decode_str_set, encode_str_set, is_human_review_reason,
};
use crate::store_factory::{StoreConfigError, resolve_postgres_dsn};
use crate::store_registry::{
    BackendSpec, BuildRequest, ClaimStoreBuilder, CoreStoreBuilder, RegistryError, StoreRegistries,
};

/// The registry name both families select the Postgres backend by.
pub const POSTGRES_BACKEND: &str = "postgres";

/// The optional extra the Python distribution gates this backend behind;
/// informational on the Rust side (the driver is compiled in).
pub const POSTGRES_EXTRA: &str = "postgres";

/// Fixed key serializing every `acquire_claim` transaction on a database.
///
/// Under the default `READ COMMITTED` isolation the conflict-key check and
/// the row upsert are otherwise a check-then-act race: two different
/// `task_id`s with overlapping conflict keys could each pass the pre-check
/// before either commits. Taken unconditionally (an empty-key claim must not
/// slip between a keyed claim's check and its upsert) and released at
/// commit or rollback. Distinct from [`BOOTSTRAP_LOCK_KEY`] so runtime
/// claims never wait on a bootstrap. `0x666C7977` == `"flyw"`; the same
/// value the Python store takes, so Rust and Python workers serialize
/// against each other too.
pub const CLAIM_ACQUIRE_LOCK_KEY: i64 = 0x666C_7977;

/// The orchestrator DDL, verbatim from the Python module (every statement
/// idempotent). The column-level additive migrations -- v3
/// `task_claims.conflict_keys_json`, v7 `orchestrator_stop_events.run_id`,
/// v9 `orchestrator_stop_events.attribution` -- run after it in
/// [`bootstrap`], guarded by a catalog read, so a v1..v8 schema converges in
/// place keeping every row.
pub const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS task_claims (
  task_id            TEXT PRIMARY KEY,
  worker_id          TEXT NOT NULL,
  claimed_at         TIMESTAMPTZ NOT NULL,
  lease_expires_at   TIMESTAMPTZ NOT NULL,
  version            INTEGER NOT NULL,
  conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb
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
  required_capabilities_json JSONB NOT NULL DEFAULT '[]'::jsonb,
  conflict_keys_json         JSONB NOT NULL DEFAULT '[]'::jsonb,
  first_seen_at              TIMESTAMPTZ NOT NULL,
  last_seen_at               TIMESTAMPTZ NOT NULL,
  disappeared_at             TIMESTAMPTZ,
  metadata_json              JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS work_item_dependencies (
  task_id              TEXT NOT NULL,
  prerequisite_task_id TEXT NOT NULL,
  created_at           TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (task_id, prerequisite_task_id)
);

CREATE INDEX IF NOT EXISTS idx_work_item_dependencies_prerequisite
  ON work_item_dependencies (prerequisite_task_id);

CREATE TABLE IF NOT EXISTS source_syncs (
  id             BIGSERIAL PRIMARY KEY,
  source_kind    TEXT NOT NULL,
  source_name    TEXT NOT NULL,
  started_at     TIMESTAMPTZ NOT NULL,
  finished_at    TIMESTAMPTZ,
  status         TEXT NOT NULL,
  observed_count INTEGER NOT NULL DEFAULT 0,
  error          TEXT,
  metadata_json  JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS orchestrator_events (
  id               BIGSERIAL PRIMARY KEY,
  task_id          TEXT NOT NULL,
  worker_id        TEXT NOT NULL,
  event_type       TEXT NOT NULL,
  version          INTEGER NOT NULL,
  lease_expires_at TIMESTAMPTZ NOT NULL,
  occurred_at      TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_events_task
  ON orchestrator_events (task_id, id);

CREATE TABLE IF NOT EXISTS graph_snapshots (
  id            BIGSERIAL PRIMARY KEY,
  captured_at   TIMESTAMPTZ NOT NULL,
  item_count    INTEGER NOT NULL,
  last_event_id BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS graph_snapshot_items (
  snapshot_id                 BIGINT NOT NULL,
  task_id                     TEXT NOT NULL,
  source_kind                 TEXT,
  source_ref                  TEXT,
  source_url                  TEXT,
  source_version              TEXT,
  priority                    INTEGER NOT NULL DEFAULT 0,
  required_capabilities_json  JSONB NOT NULL DEFAULT '[]'::jsonb,
  conflict_keys_json          JSONB NOT NULL DEFAULT '[]'::jsonb,
  state                       TEXT NOT NULL,
  ready                       BOOLEAN NOT NULL,
  claim_holder                TEXT,
  resolved_prerequisites_json JSONB NOT NULL DEFAULT '[]'::jsonb,
  PRIMARY KEY (snapshot_id, task_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_snapshot_items_snapshot
  ON graph_snapshot_items (snapshot_id);

CREATE TABLE IF NOT EXISTS orchestrator_stop_events (
  id          BIGSERIAL PRIMARY KEY,
  kind        TEXT NOT NULL,
  subject     TEXT NOT NULL,
  detail      TEXT NOT NULL,
  occurred_at TIMESTAMPTZ NOT NULL,
  run_id      TEXT NOT NULL DEFAULT '',
  attribution TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_subject
  ON orchestrator_stop_events (subject, id);

CREATE TABLE IF NOT EXISTS orchestrator_session_pauses (
  id          BIGSERIAL PRIMARY KEY,
  run_id      TEXT NOT NULL DEFAULT '',
  reset_at    TIMESTAMPTZ NOT NULL,
  pause_until TIMESTAMPTZ NOT NULL,
  recorded_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_session_pauses_pause_until
  ON orchestrator_session_pauses (pause_until);
";

/// The index this port adds over the Python DDL: it backs the human-review
/// queue read (`WHERE kind = ANY(...) ORDER BY id`) on a ledger that
/// otherwise grows without bound. Purely additive; a schema bootstrapped by
/// the Python store gains it on first open here and loses nothing.
const SUPPLEMENTARY_INDEXES: &str = "
CREATE INDEX IF NOT EXISTS idx_orchestrator_stop_events_kind
  ON orchestrator_stop_events (kind, id);
";

/// The additive column migrations, keyed by `(table, column)`: applied only
/// when the catalog says the column is absent.
const COLUMN_MIGRATIONS: [(&str, &str, &str); 3] = [
    (
        "task_claims",
        "conflict_keys_json",
        "ALTER TABLE task_claims ADD COLUMN conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb",
    ),
    (
        "orchestrator_stop_events",
        "run_id",
        "ALTER TABLE orchestrator_stop_events ADD COLUMN run_id TEXT NOT NULL DEFAULT ''",
    ),
    (
        "orchestrator_stop_events",
        "attribution",
        "ALTER TABLE orchestrator_stop_events ADD COLUMN attribution TEXT NOT NULL DEFAULT ''",
    ),
];

// --- Error and value helpers ------------------------------------------------------------

fn db(err: postgres::Error) -> ClaimStoreError {
    ClaimStoreError::Backend(Box::new(PostgresError::new(err)))
}

fn col<'a, T: FromSql<'a>>(row: &'a Row, idx: usize) -> Result<T, ClaimStoreError> {
    row.try_get(idx).map_err(db)
}

/// A persisted `INTEGER` column read as the trait's `i64`.
fn col_int4(row: &Row, idx: usize) -> Result<i64, ClaimStoreError> {
    col::<i32>(row, idx).map(i64::from)
}

/// Narrow a trait `i64` to an `INTEGER` column, refusing values that do not
/// fit instead of letting the server truncate or reject them opaquely.
fn int4(value: i64, column: &'static str) -> Result<i32, ClaimStoreError> {
    i32::try_from(value).map_err(|_| {
        ClaimStoreError::Backend(Box::new(IntegerOutOfRange {
            column,
            width: "INTEGER",
            value: i128::from(value),
        }))
    })
}

/// Truncate to microsecond precision -- what `TIMESTAMPTZ` carries -- so a
/// value handed back by a verb equals the one a later read returns.
fn to_micros(ts: DateTime<Utc>) -> DateTime<Utc> {
    let nanos = ts.nanosecond();
    let trimmed = nanos - nanos % 1_000;
    ts.with_nanosecond(trimmed).unwrap_or(ts)
}

fn lease_duration(lease_seconds: f64) -> Duration {
    let micros = (lease_seconds * 1_000_000.0).round();
    Duration::microseconds(micros as i64)
}

/// `min(reset_at, now + ceiling)` when pausing is enabled and the horizon is
/// strictly after `now`; `None` otherwise (the same clamp every backend
/// applies, so the unbounded-pause failure mode is unreachable here too).
fn clamp_pause(
    reset_at: DateTime<Utc>,
    now: DateTime<Utc>,
    ceiling_seconds: f64,
) -> Option<DateTime<Utc>> {
    if ceiling_seconds <= 0.0 {
        return None;
    }
    let reset_at = to_micros(reset_at);
    let now = to_micros(now);
    let pause_until = reset_at.min(to_micros(now + lease_duration(ceiling_seconds)));
    (pause_until > now).then_some(pause_until)
}

fn require_review_reason(reason: &str) -> Result<(), ClaimStoreError> {
    if is_human_review_reason(reason) {
        return Ok(());
    }
    let mut known: Vec<&str> = HUMAN_REVIEW_QUEUE_REASONS.to_vec();
    known.sort_unstable();
    Err(ClaimStoreError::UnknownReviewReason {
        reason: reason.to_owned(),
        known: known.into_iter().map(str::to_owned).collect(),
    })
}

/// Refuse a snapshot that names one task twice before any row is written,
/// so a refused snapshot leaves no header behind.
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

/// A string set as the JSONB array value the column stores (sorted, so the
/// server's rendering and [`encode_str_set`] agree on element order).
fn set_value(values: &BTreeSet<String>) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

/// Re-canonicalize a JSONB set rendered by the server into the
/// [`encode_str_set`] text every other backend returns.
fn canonical_set_text(rendered: &str) -> Result<String, ClaimStoreError> {
    Ok(encode_str_set(&decode_str_set(rendered)?))
}

fn search_path_sql(schema: &str) -> String {
    format!("SET search_path TO \"{schema}\", public")
}

// --- Row mappers -------------------------------------------------------------------------

const CLAIM_COLUMNS: &str = "task_id, worker_id, claimed_at, lease_expires_at, version";

fn row_to_claim(row: &Row) -> Result<TaskClaim, ClaimStoreError> {
    Ok(TaskClaim {
        task_id: col(row, 0)?,
        worker_id: col(row, 1)?,
        claimed_at: col(row, 2)?,
        lease_expires_at: col(row, 3)?,
        version: col_int4(row, 4)?,
    })
}

const EVENT_COLUMNS: &str =
    "id, task_id, worker_id, event_type, version, lease_expires_at, occurred_at";

fn row_to_event(row: &Row) -> Result<OrchestratorEventRecord, ClaimStoreError> {
    Ok(OrchestratorEventRecord {
        id: col(row, 0)?,
        task_id: col(row, 1)?,
        worker_id: col(row, 2)?,
        event_type: col(row, 3)?,
        version: col_int4(row, 4)?,
        lease_expires_at: col(row, 5)?,
        occurred_at: col(row, 6)?,
    })
}

const STOP_COLUMNS: &str = "id, kind, subject, detail, occurred_at, run_id, attribution";

fn row_to_stop_event(row: &Row) -> Result<OrchestratorStopEventRecord, ClaimStoreError> {
    Ok(OrchestratorStopEventRecord {
        id: col(row, 0)?,
        kind: col(row, 1)?,
        subject: col(row, 2)?,
        detail: col(row, 3)?,
        occurred_at: col(row, 4)?,
        run_id: col(row, 5)?,
        attribution: col(row, 6)?,
    })
}

fn row_to_review_entry(row: &Row) -> Result<HumanReviewQueueEntry, ClaimStoreError> {
    Ok(HumanReviewQueueEntry {
        id: col(row, 0)?,
        reason: col(row, 1)?,
        task_id: col(row, 2)?,
        detail: col(row, 3)?,
        occurred_at: col(row, 4)?,
        run_id: col(row, 5)?,
    })
}

const PAUSE_COLUMNS: &str = "id, run_id, reset_at, pause_until, recorded_at";

fn row_to_pause(row: &Row) -> Result<SessionPauseRecord, ClaimStoreError> {
    Ok(SessionPauseRecord {
        id: col(row, 0)?,
        run_id: col(row, 1)?,
        reset_at: col(row, 2)?,
        pause_until: col(row, 3)?,
        recorded_at: col(row, 4)?,
    })
}

const SNAPSHOT_COLUMNS: &str = "id, captured_at, item_count, last_event_id";

fn row_to_snapshot(row: &Row) -> Result<GraphSnapshotRecord, ClaimStoreError> {
    Ok(GraphSnapshotRecord {
        id: col(row, 0)?,
        captured_at: col(row, 1)?,
        item_count: col_int4(row, 2)?,
        last_event_id: col(row, 3)?,
    })
}

/// JSONB set columns are cast to text so the read-back decodes through the
/// same [`decode_str_set`] the SQLite TEXT columns use.
const SNAPSHOT_ITEM_COLUMNS: &str = "task_id, source_kind, source_ref, source_url, \
     source_version, priority, required_capabilities_json::text, conflict_keys_json::text, \
     state, ready, claim_holder, resolved_prerequisites_json::text";

fn row_to_snapshot_item(row: &Row) -> Result<GraphSnapshotItem, ClaimStoreError> {
    Ok(GraphSnapshotItem {
        task_id: col(row, 0)?,
        source_kind: col(row, 1)?,
        source_ref: col(row, 2)?,
        source_url: col(row, 3)?,
        source_version: col(row, 4)?,
        priority: col_int4(row, 5)?,
        required_capabilities: decode_str_set(&col::<String>(row, 6)?)?,
        conflict_keys: decode_str_set(&col::<String>(row, 7)?)?,
        state: col(row, 8)?,
        ready: col(row, 9)?,
        claim_holder: col(row, 10)?,
        resolved_prerequisites: decode_str_set(&col::<String>(row, 11)?)?,
    })
}

const WORK_ITEM_COLUMNS: &str = "task_id, source_kind, source_ref, source_url, source_version, \
     task_content_hash, priority, required_capabilities_json::text, conflict_keys_json::text, \
     first_seen_at, last_seen_at, disappeared_at, metadata_json::text";

fn row_to_work_item(row: &Row) -> Result<WorkItemRecord, ClaimStoreError> {
    Ok(WorkItemRecord {
        task_id: col(row, 0)?,
        source_kind: col(row, 1)?,
        source_ref: col(row, 2)?,
        source_url: col(row, 3)?,
        source_version: col(row, 4)?,
        task_content_hash: col(row, 5)?,
        priority: col_int4(row, 6)?,
        required_capabilities_json: canonical_set_text(&col::<String>(row, 7)?)?,
        conflict_keys_json: canonical_set_text(&col::<String>(row, 8)?)?,
        first_seen_at: col(row, 9)?,
        last_seen_at: col(row, 10)?,
        disappeared_at: col(row, 11)?,
        metadata_json: col(row, 12)?,
    })
}

const SOURCE_SYNC_COLUMNS: &str = "id, source_kind, source_name, started_at, finished_at, \
     status, observed_count, error, metadata_json::text";

fn row_to_source_sync(row: &Row) -> Result<SourceSyncRecord, ClaimStoreError> {
    Ok(SourceSyncRecord {
        id: col(row, 0)?,
        source_kind: col(row, 1)?,
        source_name: col(row, 2)?,
        started_at: col(row, 3)?,
        finished_at: col(row, 4)?,
        status: col(row, 5)?,
        observed_count: col_int4(row, 6)?,
        error: col(row, 7)?,
        metadata_json: col(row, 8)?,
    })
}

fn query_all<C: GenericClient, T>(
    c: &mut C,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    map: fn(&Row) -> Result<T, ClaimStoreError>,
) -> Result<Vec<T>, ClaimStoreError> {
    c.query(sql, params).map_err(db)?.iter().map(map).collect()
}

fn query_one<C: GenericClient, T>(
    c: &mut C,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    map: fn(&Row) -> Result<T, ClaimStoreError>,
) -> Result<Option<T>, ClaimStoreError> {
    c.query_opt(sql, params)
        .map_err(db)?
        .as_ref()
        .map(map)
        .transpose()
}

// --- Ledger writers (shared by every transition) --------------------------------------------

/// Append one ledger row on the caller's transaction, so the event commits
/// with the `task_claims` mutation it describes and a rolled-back transition
/// takes its event with it.
fn append_event<C: GenericClient>(
    c: &mut C,
    task_id: &str,
    worker_id: &str,
    event_type: &str,
    version: i64,
    lease_expires_at: DateTime<Utc>,
    occurred_at: DateTime<Utc>,
) -> Result<(), ClaimStoreError> {
    let version = int4(version, "orchestrator_events.version")?;
    c.execute(
        "INSERT INTO orchestrator_events (task_id, worker_id, event_type, version, \
         lease_expires_at, occurred_at) VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            &task_id,
            &worker_id,
            &event_type,
            &version,
            &lease_expires_at,
            &occurred_at,
        ],
    )
    .map_err(db)?;
    Ok(())
}

/// Append one stop row on the caller's transaction. Never dedupes:
/// recurrence is the signal.
fn insert_stop_event<C: GenericClient>(
    c: &mut C,
    kind: &str,
    subject: &str,
    detail: &str,
    occurred_at: DateTime<Utc>,
    run_id: &str,
    attribution: &str,
) -> Result<(), ClaimStoreError> {
    c.execute(
        "INSERT INTO orchestrator_stop_events (kind, subject, detail, occurred_at, run_id, \
         attribution) VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            &kind,
            &subject,
            &detail,
            &to_micros(occurred_at),
            &run_id,
            &attribution,
        ],
    )
    .map_err(db)?;
    Ok(())
}

/// Delete the claim row when the `(version, worker_id)` token still owns
/// it, returning whether it did.
fn delete_matching<C: GenericClient>(
    c: &mut C,
    claim: &TaskClaim,
) -> Result<bool, ClaimStoreError> {
    let deleted = c
        .execute(
            "DELETE FROM task_claims WHERE task_id = $1 AND version = $2::bigint \
             AND worker_id = $3",
            &[&claim.task_id, &claim.version, &claim.worker_id],
        )
        .map_err(db)?;
    Ok(deleted == 1)
}

// --- Bootstrap ----------------------------------------------------------------------------

/// Materialize the schema and converge an older store forward, all under
/// the bootstrap advisory lock in one transaction.
fn bootstrap(client: &mut Client, schema: &str) -> Result<(), ClaimStoreError> {
    let mut tx = client.transaction().map_err(db)?;
    // Serialize the whole bootstrap against every other opener (of this
    // store or the core store) so the CREATE SCHEMA / CREATE TABLE IF NOT
    // EXISTS sequence cannot race on a fresh schema: two racing openers
    // otherwise deadlock on the shared catalogs or trip a pg_namespace
    // unique violation. Released at the commit below.
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&BOOTSTRAP_LOCK_KEY])
        .map_err(db)?;
    tx.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{schema}\";\n{}",
        search_path_sql(schema)
    ))
    .map_err(db)?;
    tx.batch_execute(SCHEMA_SQL).map_err(db)?;
    // Additive column migrations (v3 / v7 / v9), applied only when the
    // catalog says the column is missing: a v1..v8 schema gains the column
    // in place with its default, so every existing row survives.
    let present: BTreeSet<(String, String)> = tx
        .query(
            "SELECT table_name, column_name FROM information_schema.columns \
             WHERE table_schema = $1::text \
             AND table_name IN ('task_claims', 'orchestrator_stop_events')",
            &[&schema],
        )
        .map_err(db)?
        .iter()
        .map(|row| Ok((col(row, 0)?, col(row, 1)?)))
        .collect::<Result<_, ClaimStoreError>>()?;
    for (table, column, ddl) in COLUMN_MIGRATIONS {
        if !present.contains(&(table.to_owned(), column.to_owned())) {
            tx.batch_execute(ddl).map_err(db)?;
        }
    }
    tx.batch_execute(SUPPLEMENTARY_INDEXES).map_err(db)?;
    let current = int4(
        i64::from(CURRENT_ORCH_SCHEMA_VERSION),
        "orchestrator_schema_version.version",
    )?;
    tx.execute(
        "INSERT INTO orchestrator_schema_version (id, version) VALUES (1, $1) \
         ON CONFLICT (id) DO NOTHING",
        &[&current],
    )
    .map_err(db)?;
    // Every table and column above was materialized additively, so an older
    // store keeps its rows and simply converges its sentinel forward. A
    // newer-than-this-build sentinel is left alone and refused below.
    tx.execute(
        "UPDATE orchestrator_schema_version SET version = $1 WHERE id = 1 AND version < $1",
        &[&current],
    )
    .map_err(db)?;
    let observed: Option<i32> = tx
        .query_opt(
            "SELECT version FROM orchestrator_schema_version WHERE id = 1",
            &[],
        )
        .map_err(db)?
        .map(|row| col(&row, 0))
        .transpose()?;
    tx.commit().map_err(db)?;
    let observed = observed.and_then(|v| u32::try_from(v).ok());
    if observed != Some(CURRENT_ORCH_SCHEMA_VERSION) {
        return Err(OrchestratorSchemaError {
            observed,
            expected: CURRENT_ORCH_SCHEMA_VERSION,
        }
        .into());
    }
    Ok(())
}

// --- The store ----------------------------------------------------------------------------

/// Postgres [`ClaimStore`]: one connection behind a mutex, every verb one
/// lock acquisition, every mutation one transaction.
///
/// Safe to point at the schema the core `PostgresStore` uses (each owns only
/// its own tables and sentinel) or at a schema of its own. Several instances
/// -- in one process, across processes, or mixed with Python workers --
/// coordinate through the database itself: the acquire advisory lock, row
/// locks, and the `ON CONFLICT` guard, never through anything in-process.
pub struct PostgresClaimStore {
    client: Mutex<Client>,
    schema: String,
}

impl fmt::Debug for PostgresClaimStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresClaimStore")
            .field("schema", &self.schema)
            .finish_non_exhaustive()
    }
}

impl PostgresClaimStore {
    /// Connect to `dsn` (a libpq-style URL or `key=value` string), pin
    /// `schema` on the session, and bootstrap (or migrate) it. The DSN is
    /// never logged or echoed.
    ///
    /// Fails before any network traffic when `schema` is not
    /// identifier-safe; connection and authentication failures surface as
    /// [`ClaimStoreError::Backend`] wrapping a [`PostgresError`]; a schema
    /// whose sentinel is newer than this build is refused with
    /// [`ClaimStoreError::Schema`].
    pub fn connect(dsn: &str, schema: &str) -> Result<Self, ClaimStoreError> {
        let schema = validate_schema(schema)
            .map_err(|err| ClaimStoreError::Backend(Box::new(err)))?
            .to_owned();
        let mut client = Client::connect(dsn, NoTls).map_err(db)?;
        client
            .batch_execute(&search_path_sql(&schema))
            .map_err(db)?;
        bootstrap(&mut client, &schema)?;
        Ok(Self {
            client: Mutex::new(client),
            schema,
        })
    }

    /// The schema every table lives under.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Run `f` against the underlying connection under the store's lock:
    /// the escape hatch for inspection and for consumers sharing the
    /// database. The store's invariants are not re-checked on what `f` does.
    pub fn with_client<T>(&self, f: impl FnOnce(&mut Client) -> T) -> T {
        let mut client = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut client)
    }

    /// Close the connection.
    pub fn close(self) -> Result<(), ClaimStoreError> {
        let client = self
            .client
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        client.close().map_err(db)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Client>, ClaimStoreError> {
        self.client.lock().map_err(|_| ClaimStoreError::Poisoned)
    }

    /// Run `body` in one transaction: committed on `Ok`, rolled back on
    /// `Err` (so a ledger row never outlives a failed transition).
    fn write<T>(
        &self,
        body: impl FnOnce(&mut Transaction<'_>) -> Result<T, ClaimStoreError>,
    ) -> Result<T, ClaimStoreError> {
        let mut client = self.lock()?;
        let mut tx = client.transaction().map_err(db)?;
        let value = body(&mut tx)?;
        tx.commit().map_err(db)?;
        Ok(value)
    }

    fn read<T>(
        &self,
        body: impl FnOnce(&mut Client) -> Result<T, ClaimStoreError>,
    ) -> Result<T, ClaimStoreError> {
        let mut client = self.lock()?;
        body(&mut client)
    }
}

impl ClaimStore for PostgresClaimStore {
    fn acquire_claim(
        &self,
        task_id: &str,
        worker_id: &str,
        now: DateTime<Utc>,
        lease_seconds: f64,
        conflict_keys: &BTreeSet<String>,
    ) -> Result<Option<TaskClaim>, ClaimStoreError> {
        let now = to_micros(now);
        let lease_expires = to_micros(now + lease_duration(lease_seconds));
        let keys_value = set_value(conflict_keys);
        let keys: Vec<&str> = conflict_keys.iter().map(String::as_str).collect();
        self.write(|tx| {
            // Serialize the check-then-upsert against every other acquire on
            // this database (Rust or Python); released at commit/rollback.
            tx.execute(
                "SELECT pg_advisory_xact_lock($1)",
                &[&CLAIM_ACQUIRE_LOCK_KEY],
            )
            .map_err(db)?;
            // Refuse on conflict-key overlap with a *different* live claim:
            // the task's own row is excluded and lapsed claims do not block,
            // so the refusal clears once the conflicting claim is released
            // or expires. `?|` is "any of these strings is an element".
            if !keys.is_empty() {
                let blocked: bool = tx
                    .query_one(
                        "SELECT EXISTS (SELECT 1 FROM task_claims WHERE task_id <> $1 \
                         AND lease_expires_at > $2 AND conflict_keys_json ?| $3::text[])",
                        &[&task_id, &now, &keys],
                    )
                    .map_err(db)?
                    .try_get(0)
                    .map_err(db)?;
                if blocked {
                    return Ok(None);
                }
            }
            // `prev` reads the row's pre-statement holder (statement
            // snapshot: it never sees the upsert's own write); the upsert's
            // WHERE is the optimistic-concurrency guard. No row back means
            // a live, different-worker lease refused the update.
            let row = tx
                .query_opt(
                    "WITH prev AS (\
                         SELECT worker_id AS prev_worker FROM task_claims WHERE task_id = $1\
                     ), upserted AS (\
                         INSERT INTO task_claims (task_id, worker_id, claimed_at, \
                             lease_expires_at, version, conflict_keys_json) \
                         VALUES ($1, $2, $3, $4, 1, $5::jsonb) \
                         ON CONFLICT (task_id) DO UPDATE SET \
                             worker_id = EXCLUDED.worker_id, \
                             claimed_at = EXCLUDED.claimed_at, \
                             lease_expires_at = EXCLUDED.lease_expires_at, \
                             version = task_claims.version + 1, \
                             conflict_keys_json = EXCLUDED.conflict_keys_json \
                         WHERE task_claims.lease_expires_at <= EXCLUDED.claimed_at \
                            OR task_claims.worker_id = EXCLUDED.worker_id \
                         RETURNING version\
                     ) \
                     SELECT upserted.version, prev.prev_worker \
                     FROM upserted LEFT JOIN prev ON true",
                    &[&task_id, &worker_id, &now, &lease_expires, &keys_value],
                )
                .map_err(db)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let version = col_int4(&row, 0)?;
            let prev_worker: Option<String> = col(&row, 1)?;
            // A fresh insert or a same-worker re-acquire is `acquired`;
            // reclaiming a *different* worker's lapsed lease is `stolen` (the
            // live-different-worker case returned None above).
            let event_type = match prev_worker.as_deref() {
                None => EVENT_ACQUIRED,
                Some(prev) if prev == worker_id => EVENT_ACQUIRED,
                Some(_) => EVENT_STOLEN,
            };
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
        let lease_expires = to_micros(now + lease_duration(lease_seconds));
        // The UPDATE and its `renewed` event commit together: a stale token
        // updates no row, the error rolls the transaction back, no event.
        self.write(|tx| {
            let row = tx
                .query_opt(
                    "UPDATE task_claims SET lease_expires_at = $1, version = version + 1 \
                     WHERE task_id = $2 AND version = $3::bigint AND worker_id = $4 \
                     RETURNING version",
                    &[
                        &lease_expires,
                        &claim.task_id,
                        &claim.version,
                        &claim.worker_id,
                    ],
                )
                .map_err(db)?;
            let Some(row) = row else {
                return Err(ClaimLostError {
                    task_id: claim.task_id.clone(),
                }
                .into());
            };
            let version = col_int4(&row, 0)?;
            append_event(
                tx,
                &claim.task_id,
                &claim.worker_id,
                EVENT_RENEWED,
                version,
                lease_expires,
                now,
            )?;
            Ok(TaskClaim {
                task_id: claim.task_id.clone(),
                worker_id: claim.worker_id.clone(),
                claimed_at: claim.claimed_at,
                lease_expires_at: lease_expires,
                version,
            })
        })
    }

    fn release_claim(
        &self,
        claim: &TaskClaim,
        now: Option<DateTime<Utc>>,
    ) -> Result<(), ClaimStoreError> {
        let occurred_at = to_micros(now.unwrap_or(claim.lease_expires_at));
        self.write(|tx| {
            if delete_matching(tx, claim)? {
                append_event(
                    tx,
                    &claim.task_id,
                    &claim.worker_id,
                    EVENT_RELEASED,
                    claim.version,
                    to_micros(claim.lease_expires_at),
                    occurred_at,
                )?;
            }
            Ok(())
        })
    }

    fn load_claim(&self, task_id: &str) -> Result<Option<TaskClaim>, ClaimStoreError> {
        self.read(|c| {
            query_one(
                c,
                &format!("SELECT {CLAIM_COLUMNS} FROM task_claims WHERE task_id = $1"),
                &[&task_id],
                row_to_claim,
            )
        })
    }

    fn list_claims(&self) -> Result<Vec<TaskClaim>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!("SELECT {CLAIM_COLUMNS} FROM task_claims ORDER BY task_id"),
                &[],
                row_to_claim,
            )
        })
    }

    fn sweep_expired_claims(&self, now: DateTime<Utc>) -> Result<Vec<String>, ClaimStoreError> {
        let now = to_micros(now);
        // One statement: delete every lapsed row (TIMESTAMPTZ comparison,
        // temporal not lexical) and journal one `expired` event per reaped
        // claim carrying that claim's holder / version, in task_id order so
        // the freed ids and the events emit as the SQLite backend does.
        self.write(|tx| {
            let rows = tx
                .query(
                    "WITH reaped AS (\
                         DELETE FROM task_claims WHERE lease_expires_at <= $1 \
                         RETURNING task_id, worker_id, version, lease_expires_at\
                     ), journaled AS (\
                         INSERT INTO orchestrator_events (task_id, worker_id, event_type, \
                             version, lease_expires_at, occurred_at) \
                         SELECT task_id, worker_id, $2::text, version, lease_expires_at, $1 \
                         FROM reaped ORDER BY task_id \
                         RETURNING task_id\
                     ) \
                     SELECT task_id FROM journaled ORDER BY task_id",
                    &[&now, &EVENT_EXPIRED],
                )
                .map_err(db)?;
            rows.iter().map(|row| col(row, 0)).collect()
        })
    }

    fn list_events(&self) -> Result<Vec<OrchestratorEventRecord>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!("SELECT {EVENT_COLUMNS} FROM orchestrator_events ORDER BY id"),
                &[],
                row_to_event,
            )
        })
    }

    fn list_task_events(
        &self,
        task_id: &str,
    ) -> Result<Vec<OrchestratorEventRecord>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!(
                    "SELECT {EVENT_COLUMNS} FROM orchestrator_events WHERE task_id = $1 \
                     ORDER BY id"
                ),
                &[&task_id],
                row_to_event,
            )
        })
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
        let now = to_micros(now);
        // The release (with its `released` event when the token still owns a
        // row) and the `prepare-skip` stop row commit in one transaction; the
        // stop row is written unconditionally -- the dead-end happened
        // whether or not the release found a row to delete.
        self.write(|tx| {
            if delete_matching(tx, claim)? {
                append_event(
                    tx,
                    &claim.task_id,
                    &claim.worker_id,
                    EVENT_RELEASED,
                    claim.version,
                    to_micros(claim.lease_expires_at),
                    now,
                )?;
            }
            insert_stop_event(tx, STOP_PREPARE_SKIP, &claim.task_id, detail, now, "", "")
        })
    }

    fn list_stop_events(&self) -> Result<Vec<OrchestratorStopEventRecord>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!("SELECT {STOP_COLUMNS} FROM orchestrator_stop_events ORDER BY id"),
                &[],
                row_to_stop_event,
            )
        })
    }

    fn list_subject_stop_events(
        &self,
        subject: &str,
    ) -> Result<Vec<OrchestratorStopEventRecord>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!(
                    "SELECT {STOP_COLUMNS} FROM orchestrator_stop_events WHERE subject = $1 \
                     ORDER BY id"
                ),
                &[&subject],
                row_to_stop_event,
            )
        })
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
        let reset_at = to_micros(reset_at);
        let now = to_micros(now);
        self.write(|tx| {
            tx.execute(
                "INSERT INTO orchestrator_session_pauses (run_id, reset_at, pause_until, \
                 recorded_at) VALUES ($1, $2, $3, $4)",
                &[&run_id, &reset_at, &pause_until, &now],
            )
            .map_err(db)?;
            Ok(())
        })?;
        Ok(Some(pause_until))
    }

    fn session_pause_until(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, ClaimStoreError> {
        let now = to_micros(now);
        // High-water horizon over rows still in the future; each row was
        // individually clamped so the max never exceeds the latest
        // min(reset, recorded_at + ceiling). Index-backed on pause_until.
        self.read(|c| {
            let row = c
                .query_one(
                    "SELECT MAX(pause_until) FROM orchestrator_session_pauses \
                     WHERE pause_until > $1",
                    &[&now],
                )
                .map_err(db)?;
            col(&row, 0)
        })
    }

    fn list_session_pauses(&self) -> Result<Vec<SessionPauseRecord>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!("SELECT {PAUSE_COLUMNS} FROM orchestrator_session_pauses ORDER BY id"),
                &[],
                row_to_pause,
            )
        })
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
        let mut reasons: Vec<&str> = HUMAN_REVIEW_QUEUE_REASONS.to_vec();
        reasons.sort_unstable();
        self.read(|c| {
            query_all(
                c,
                "SELECT id, kind, subject, detail, occurred_at, run_id \
                 FROM orchestrator_stop_events WHERE kind = ANY($1::text[]) ORDER BY id",
                &[&reasons],
                row_to_review_entry,
            )
        })
    }

    fn record_graph_snapshot(
        &self,
        items: &[GraphSnapshotItem],
        captured_at: DateTime<Utc>,
    ) -> Result<GraphSnapshotRecord, ClaimStoreError> {
        require_unique_snapshot_items(items)?;
        let captured_at = to_micros(captured_at);
        let item_count = int4(items.len() as i64, "graph_snapshots.item_count")?;
        let priorities = items
            .iter()
            .map(|item| int4(item.priority, "graph_snapshot_items.priority"))
            .collect::<Result<Vec<i32>, _>>()?;
        let task_ids: Vec<&str> = items.iter().map(|i| i.task_id.as_str()).collect();
        let source_kinds: Vec<Option<&str>> =
            items.iter().map(|i| i.source_kind.as_deref()).collect();
        let source_refs: Vec<Option<&str>> =
            items.iter().map(|i| i.source_ref.as_deref()).collect();
        let source_urls: Vec<Option<&str>> =
            items.iter().map(|i| i.source_url.as_deref()).collect();
        let source_versions: Vec<Option<&str>> =
            items.iter().map(|i| i.source_version.as_deref()).collect();
        let capabilities: Vec<String> = items
            .iter()
            .map(|i| encode_str_set(&i.required_capabilities))
            .collect();
        let conflict_keys: Vec<String> = items
            .iter()
            .map(|i| encode_str_set(&i.conflict_keys))
            .collect();
        let states: Vec<&str> = items.iter().map(|i| i.state.as_str()).collect();
        let readiness: Vec<bool> = items.iter().map(|i| i.ready).collect();
        let holders: Vec<Option<&str>> = items.iter().map(|i| i.claim_holder.as_deref()).collect();
        let resolved: Vec<String> = items
            .iter()
            .map(|i| encode_str_set(&i.resolved_prerequisites))
            .collect();
        // Header and every item row commit in one transaction, and
        // `last_event_id` is the live ledger high-water mark read inside
        // that transaction (0 on an empty ledger), never caller-supplied.
        self.write(|tx| {
            let last_event_id: i64 = tx
                .query_one("SELECT COALESCE(MAX(id), 0) FROM orchestrator_events", &[])
                .map_err(db)?
                .try_get(0)
                .map_err(db)?;
            let snapshot_id: i64 = tx
                .query_one(
                    "INSERT INTO graph_snapshots (captured_at, item_count, last_event_id) \
                     VALUES ($1, $2, $3) RETURNING id",
                    &[&captured_at, &item_count, &last_event_id],
                )
                .map_err(db)?
                .try_get(0)
                .map_err(db)?;
            tx.execute(
                "INSERT INTO graph_snapshot_items (snapshot_id, task_id, source_kind, \
                     source_ref, source_url, source_version, priority, \
                     required_capabilities_json, conflict_keys_json, state, ready, \
                     claim_holder, resolved_prerequisites_json) \
                 SELECT $1::bigint, u.task_id, u.source_kind, u.source_ref, u.source_url, \
                     u.source_version, u.priority, u.required_capabilities::jsonb, \
                     u.conflict_keys::jsonb, u.state, u.ready, u.claim_holder, \
                     u.resolved_prerequisites::jsonb \
                 FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], $6::text[], \
                     $7::int4[], $8::text[], $9::text[], $10::text[], $11::bool[], \
                     $12::text[], $13::text[]) \
                 AS u(task_id, source_kind, source_ref, source_url, source_version, \
                     priority, required_capabilities, conflict_keys, state, ready, \
                     claim_holder, resolved_prerequisites)",
                &[
                    &snapshot_id,
                    &task_ids,
                    &source_kinds,
                    &source_refs,
                    &source_urls,
                    &source_versions,
                    &priorities,
                    &capabilities,
                    &conflict_keys,
                    &states,
                    &readiness,
                    &holders,
                    &resolved,
                ],
            )
            .map_err(db)?;
            Ok(GraphSnapshotRecord {
                id: snapshot_id,
                captured_at,
                item_count: i64::from(item_count),
                last_event_id,
            })
        })
    }

    fn list_graph_snapshots(&self) -> Result<Vec<GraphSnapshotRecord>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!("SELECT {SNAPSHOT_COLUMNS} FROM graph_snapshots ORDER BY id"),
                &[],
                row_to_snapshot,
            )
        })
    }

    fn list_graph_snapshot_items(
        &self,
        snapshot_id: i64,
    ) -> Result<Vec<GraphSnapshotItem>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!(
                    "SELECT {SNAPSHOT_ITEM_COLUMNS} FROM graph_snapshot_items \
                     WHERE snapshot_id = $1 ORDER BY task_id"
                ),
                &[&snapshot_id],
                row_to_snapshot_item,
            )
        })
    }

    fn latest_graph_snapshot(&self) -> Result<Option<GraphSnapshotRecord>, ClaimStoreError> {
        self.read(|c| {
            query_one(
                c,
                &format!("SELECT {SNAPSHOT_COLUMNS} FROM graph_snapshots ORDER BY id DESC LIMIT 1"),
                &[],
                row_to_snapshot,
            )
        })
    }

    fn upsert_work_item(
        &self,
        item: &WorkItemObservation,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let now = to_micros(now);
        let priority = int4(item.priority, "work_items.priority")?;
        let capabilities = set_value(&item.required_capabilities);
        let conflict_keys = set_value(&item.conflict_keys);
        self.write(|tx| {
            tx.execute(
                "INSERT INTO work_items (task_id, source_kind, source_ref, source_url, \
                     source_version, task_content_hash, priority, required_capabilities_json, \
                     conflict_keys_json, first_seen_at, last_seen_at, disappeared_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9::jsonb, $10, $10, NULL) \
                 ON CONFLICT (task_id) DO UPDATE SET \
                     source_kind = EXCLUDED.source_kind, source_ref = EXCLUDED.source_ref, \
                     source_url = EXCLUDED.source_url, source_version = EXCLUDED.source_version, \
                     task_content_hash = EXCLUDED.task_content_hash, \
                     priority = EXCLUDED.priority, \
                     required_capabilities_json = EXCLUDED.required_capabilities_json, \
                     conflict_keys_json = EXCLUDED.conflict_keys_json, \
                     last_seen_at = EXCLUDED.last_seen_at, disappeared_at = NULL",
                &[
                    &item.task_id,
                    &item.source_kind,
                    &item.source_ref,
                    &item.source_url,
                    &item.source_version,
                    &item.task_content_hash,
                    &priority,
                    &capabilities,
                    &conflict_keys,
                    &now,
                ],
            )
            .map_err(db)?;
            Ok(())
        })
    }

    fn replace_work_item_dependencies(
        &self,
        task_id: &str,
        prerequisite_task_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let now = to_micros(now);
        let unique: Vec<&str> = prerequisite_task_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.write(|tx| {
            tx.execute(
                "DELETE FROM work_item_dependencies WHERE task_id = $1",
                &[&task_id],
            )
            .map_err(db)?;
            tx.execute(
                "INSERT INTO work_item_dependencies (task_id, prerequisite_task_id, created_at) \
                 SELECT $1, p, $3 FROM unnest($2::text[]) AS p",
                &[&task_id, &unique, &now],
            )
            .map_err(db)?;
            Ok(())
        })
    }

    fn mark_work_items_disappeared(
        &self,
        observed_task_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), ClaimStoreError> {
        let now = to_micros(now);
        let observed: Vec<&str> = observed_task_ids.iter().map(String::as_str).collect();
        self.write(|tx| {
            tx.execute(
                "UPDATE work_items SET disappeared_at = $1 \
                 WHERE disappeared_at IS NULL AND task_id <> ALL($2::text[])",
                &[&now, &observed],
            )
            .map_err(db)?;
            Ok(())
        })
    }

    fn load_work_item(&self, task_id: &str) -> Result<Option<WorkItemRecord>, ClaimStoreError> {
        self.read(|c| {
            query_one(
                c,
                &format!("SELECT {WORK_ITEM_COLUMNS} FROM work_items WHERE task_id = $1"),
                &[&task_id],
                row_to_work_item,
            )
        })
    }

    fn list_work_items(&self) -> Result<Vec<WorkItemRecord>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!("SELECT {WORK_ITEM_COLUMNS} FROM work_items ORDER BY task_id"),
                &[],
                row_to_work_item,
            )
        })
    }

    fn load_work_item_dependencies(&self, task_id: &str) -> Result<Vec<String>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                "SELECT prerequisite_task_id FROM work_item_dependencies WHERE task_id = $1 \
                 ORDER BY prerequisite_task_id",
                &[&task_id],
                |row| col(row, 0),
            )
        })
    }

    fn list_work_item_dependencies(&self) -> Result<Vec<(String, String)>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                "SELECT task_id, prerequisite_task_id FROM work_item_dependencies \
                 ORDER BY task_id, prerequisite_task_id",
                &[],
                |row| Ok((col(row, 0)?, col(row, 1)?)),
            )
        })
    }

    fn record_source_sync_start(
        &self,
        source_kind: &str,
        source_name: &str,
        now: DateTime<Utc>,
    ) -> Result<i64, ClaimStoreError> {
        let now = to_micros(now);
        self.write(|tx| {
            let row = tx
                .query_one(
                    "INSERT INTO source_syncs (source_kind, source_name, started_at, status, \
                     observed_count) VALUES ($1, $2, $3, 'running', 0) RETURNING id",
                    &[&source_kind, &source_name, &now],
                )
                .map_err(db)?;
            col(&row, 0)
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
        let now = to_micros(now);
        let observed_count = int4(observed_count, "source_syncs.observed_count")?;
        self.write(|tx| {
            tx.execute(
                "UPDATE source_syncs SET status = $1, observed_count = $2, error = $3, \
                 finished_at = $4 WHERE id = $5",
                &[&status, &observed_count, &error, &now, &sync_id],
            )
            .map_err(db)?;
            Ok(())
        })
    }

    fn load_source_sync(&self, sync_id: i64) -> Result<Option<SourceSyncRecord>, ClaimStoreError> {
        self.read(|c| {
            query_one(
                c,
                &format!("SELECT {SOURCE_SYNC_COLUMNS} FROM source_syncs WHERE id = $1"),
                &[&sync_id],
                row_to_source_sync,
            )
        })
    }

    fn list_source_syncs(&self) -> Result<Vec<SourceSyncRecord>, ClaimStoreError> {
        self.read(|c| {
            query_all(
                c,
                &format!("SELECT {SOURCE_SYNC_COLUMNS} FROM source_syncs ORDER BY id"),
                &[],
                row_to_source_sync,
            )
        })
    }
}

// --- Registry wiring -------------------------------------------------------------------------

/// The schema a build request selects: `[store] schema` when set, else
/// Postgres's own default.
fn requested_schema<'a>(request: &'a BuildRequest<'_>) -> &'a str {
    request.config.schema.as_deref().unwrap_or(DEFAULT_SCHEMA)
}

/// The DSN a build request resolves to, or the fail-fast missing-DSN error
/// (which names the variables, never a value).
fn requested_dsn(request: &BuildRequest<'_>) -> Result<String, StoreConfigError> {
    resolve_postgres_dsn(request.environ).ok_or_else(|| StoreConfigError::MissingDsn {
        backend: POSTGRES_BACKEND.to_owned(),
    })
}

/// Build the Postgres claim store (the registry's `postgres` target): the
/// DSN comes from the environment (`FLYWHEEL_PG_DSN`, then `DATABASE_URL`),
/// the schema from `[store] schema`. An unreachable database surfaces as the
/// store's own connection error -- never a silent sqlite fallback -- and
/// `db_path` is untouched.
pub fn build_postgres_claim_store(
    request: &BuildRequest<'_>,
) -> Result<Arc<dyn ClaimStore>, StoreConfigError> {
    let dsn = requested_dsn(request)?;
    let store = PostgresClaimStore::connect(&dsn, requested_schema(request))?;
    Ok(Arc::new(store))
}

/// Build the Postgres core store (the core registry's `postgres` target)
/// from the same DSN and schema the claim store uses, so one `[store]`
/// section routes both families onto one database.
pub fn build_postgres_core_store(
    request: &BuildRequest<'_>,
) -> Result<Arc<dyn HarnessStore>, StoreConfigError> {
    let dsn = requested_dsn(request)?;
    let store = PostgresStore::connect(&dsn, requested_schema(request))?;
    Ok(Arc::new(store))
}

fn postgres_spec<B>(build: B) -> BackendSpec<B> {
    BackendSpec::new(
        POSTGRES_BACKEND,
        "durable Postgres backend",
        Some(POSTGRES_EXTRA),
        build,
    )
}

/// Register `postgres` in the claim-store family; refuses a duplicate.
pub fn register_postgres_claim_backend(
    registries: &mut StoreRegistries,
) -> Result<(), RegistryError> {
    let builder: ClaimStoreBuilder = Arc::new(build_postgres_claim_store);
    registries.claims.register(postgres_spec(builder))
}

/// Register `postgres` in the core-store family; refuses a duplicate.
pub fn register_postgres_core_backend(
    registries: &mut StoreRegistries,
) -> Result<(), RegistryError> {
    let builder: CoreStoreBuilder = Arc::new(build_postgres_core_store);
    registries.core.register(postgres_spec(builder))
}

/// Register `postgres` in both families, so a postgres deployment routes
/// runs and claims onto one database and no coordination state leaks to
/// sqlite. Either registration refusing (already present) fails the whole
/// call, leaving whatever was registered first in place.
pub fn register_postgres_backends(registries: &mut StoreRegistries) -> Result<(), RegistryError> {
    register_postgres_core_backend(registries)?;
    register_postgres_claim_backend(registries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_factory::{
        PG_DSN_ENV, PG_DSN_FALLBACK_ENV, build_claim_store, build_store, preflight_store,
    };
    use crate::store_registry::{Environ, SqliteCoreOpener, StoreConfig};
    use chrono::TimeZone;
    use flywheel_core::store::StoreError;
    use std::path::Path;

    fn t(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 28, 12, 0, second).unwrap()
    }

    fn registries() -> StoreRegistries {
        let opener: SqliteCoreOpener = Arc::new(|_path: &Path| {
            Err(StoreError::backend(std::io::Error::other(
                "core sqlite opener unused in these tests",
            )))
        });
        StoreRegistries::with_sqlite(opener)
    }

    fn postgres_config(schema: Option<&str>) -> StoreConfig {
        StoreConfig {
            backend: POSTGRES_BACKEND.to_owned(),
            db_path: "/tmp/never-created.db".into(),
            schema: schema.map(str::to_owned),
        }
    }

    #[test]
    fn lock_keys_are_distinct_and_the_python_values() {
        assert_eq!(CLAIM_ACQUIRE_LOCK_KEY, 0x666C_7977);
        assert_eq!(BOOTSTRAP_LOCK_KEY, 0x666C_7962);
        assert_ne!(CLAIM_ACQUIRE_LOCK_KEY, BOOTSTRAP_LOCK_KEY);
        assert!(i32::try_from(CLAIM_ACQUIRE_LOCK_KEY).is_ok());
    }

    #[test]
    fn ddl_declares_every_table_with_temporal_and_jsonb_columns() {
        for table in [
            "task_claims",
            "orchestrator_schema_version",
            "work_items",
            "work_item_dependencies",
            "source_syncs",
            "orchestrator_events",
            "graph_snapshots",
            "graph_snapshot_items",
            "orchestrator_stop_events",
            "orchestrator_session_pauses",
        ] {
            assert!(
                SCHEMA_SQL.contains(&format!("CREATE TABLE IF NOT EXISTS {table} (")),
                "{table} missing"
            );
        }
        assert!(SCHEMA_SQL.contains("lease_expires_at   TIMESTAMPTZ NOT NULL"));
        assert!(SCHEMA_SQL.contains("conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb"));
        assert!(!SCHEMA_SQL.contains("DATETIME"));
        assert!(!SCHEMA_SQL.contains("AUTOINCREMENT"));
        for (table, column, ddl) in COLUMN_MIGRATIONS {
            assert!(ddl.starts_with(&format!("ALTER TABLE {table} ADD COLUMN {column} ")));
        }
    }

    #[test]
    fn invalid_schema_fails_before_any_connection_attempt() {
        let err = PostgresClaimStore::connect("postgresql://nobody@203.0.113.1:1/x", "bad-name")
            .unwrap_err();
        assert!(matches!(err, ClaimStoreError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("invalid schema name"));
        assert!(!err.to_string().contains("203.0.113.1"));
    }

    #[test]
    fn unreachable_server_fails_loudly_without_leaking_the_password() {
        let err = PostgresClaimStore::connect(
            "postgresql://user:hunter2@127.0.0.1:1/postgres?connect_timeout=2",
            "fw",
        )
        .unwrap_err();
        assert!(matches!(err, ClaimStoreError::Backend(_)), "{err:?}");
        assert!(!err.to_string().contains("hunter2"));
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn registration_adds_postgres_to_both_families_once() {
        let mut registries = registries();
        register_postgres_backends(&mut registries).unwrap();
        assert_eq!(registries.core.names(), vec!["postgres", "sqlite"]);
        assert_eq!(registries.claims.names(), vec!["postgres", "sqlite"]);
        let spec = registries.claims.resolve(POSTGRES_BACKEND).unwrap();
        assert_eq!(spec.extra.as_deref(), Some(POSTGRES_EXTRA));
        assert_eq!(spec.summary, "durable Postgres backend");
        let again = register_postgres_claim_backend(&mut registries).unwrap_err();
        assert_eq!(
            again,
            RegistryError::Duplicate {
                family: "claim store".to_owned(),
                name: POSTGRES_BACKEND.to_owned()
            }
        );
        assert!(register_postgres_core_backend(&mut registries).is_err());
    }

    #[test]
    fn builders_fail_fast_without_a_dsn_and_name_both_variables() {
        let mut registries = registries();
        register_postgres_backends(&mut registries).unwrap();
        let config = postgres_config(Some("fw"));
        for result in [
            build_claim_store(&registries, &config, &Environ::new()).map(|_| ()),
            build_store(&registries, &config, &Environ::new()).map(|_| ()),
            preflight_store(&registries, &config, &Environ::new()),
        ] {
            let err = result.unwrap_err();
            assert!(matches!(err, StoreConfigError::MissingDsn { .. }), "{err}");
            let message = err.to_string();
            assert!(message.contains(PG_DSN_ENV) && message.contains(PG_DSN_FALLBACK_ENV));
        }
        assert!(!Path::new("/tmp/never-created.db").exists());
    }

    #[test]
    fn builders_surface_connection_failures_without_the_dsn_value() {
        let mut registries = registries();
        register_postgres_backends(&mut registries).unwrap();
        let secret = "postgresql://user:hunter2@127.0.0.1:1/postgres?connect_timeout=2";
        let environ: Environ = [(PG_DSN_FALLBACK_ENV.to_owned(), secret.to_owned())]
            .into_iter()
            .collect();
        let config = postgres_config(None);
        let claims = build_claim_store(&registries, &config, &environ)
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(claims, StoreConfigError::Claims(_)), "{claims}");
        assert!(!claims.to_string().contains("hunter2"));
        let core = build_store(&registries, &config, &environ)
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(core, StoreConfigError::Core(_)), "{core}");
        assert!(!core.to_string().contains("hunter2"));
        let preflight = preflight_store(&registries, &config, &environ).unwrap_err();
        assert!(
            matches!(preflight, StoreConfigError::Unreachable { .. }),
            "{preflight}"
        );
        assert!(!preflight.to_string().contains("hunter2"));
    }

    #[test]
    fn requested_schema_defaults_to_public() {
        let environ = Environ::new();
        let with = postgres_config(Some("fw_ci"));
        let without = postgres_config(None);
        assert_eq!(
            requested_schema(&BuildRequest {
                config: &with,
                environ: &environ
            }),
            "fw_ci"
        );
        assert_eq!(
            requested_schema(&BuildRequest {
                config: &without,
                environ: &environ
            }),
            "public"
        );
    }

    #[test]
    fn integer_columns_reject_out_of_range_values() {
        let err = int4(i64::from(i32::MAX) + 1, "work_items.priority").unwrap_err();
        assert!(err.to_string().contains("work_items.priority"));
        assert_eq!(int4(-7, "x").unwrap(), -7);
    }

    #[test]
    fn helpers_match_the_shared_backend_semantics() {
        assert_eq!(lease_duration(0.5), Duration::milliseconds(500));
        assert_eq!(
            to_micros(t(0) + Duration::nanoseconds(1_234_567)),
            t(0) + Duration::microseconds(1_234)
        );
        assert_eq!(clamp_pause(t(50), t(0), 30.0), Some(t(30)));
        assert_eq!(clamp_pause(t(10), t(0), 30.0), Some(t(10)));
        assert_eq!(clamp_pause(t(0), t(5), 30.0), None);
        assert_eq!(clamp_pause(t(50), t(0), 0.0), None);
        assert!(require_review_reason("the build was flaky").is_err());
        assert!(require_review_reason("awaiting-approval").is_ok());
        let dup = [
            GraphSnapshotItem::new("a", "pending", true),
            GraphSnapshotItem::new("a", "running", true),
        ];
        assert!(matches!(
            require_unique_snapshot_items(&dup).unwrap_err(),
            ClaimStoreError::DuplicateSnapshotItem { .. }
        ));
    }

    #[test]
    fn set_columns_round_trip_through_the_servers_jsonb_rendering() {
        let keys: BTreeSet<String> = ["linux", "caf\u{e9}"].map(str::to_owned).into();
        assert_eq!(set_value(&keys), serde_json::json!(["caf\u{e9}", "linux"]));
        // What `jsonb::text` renders (UTF-8, not ASCII-escaped) comes back as
        // the canonical encoding every other backend returns.
        assert_eq!(
            canonical_set_text("[\"caf\u{e9}\", \"linux\"]").unwrap(),
            "[\"caf\\u00e9\", \"linux\"]"
        );
        assert_eq!(canonical_set_text("[]").unwrap(), "[]");
    }
}
