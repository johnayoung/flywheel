//! SQLite implementation of every store trait in [`crate::store`].
//!
//! The durable backend. Bootstraps the schema by executing the canonical
//! `persistence-schema.sql` verbatim (embedded at compile time from its
//! canonical path, so the DDL is never re-derived here), including its
//! `PRAGMA journal_mode = WAL` and `PRAGMA foreign_keys = ON` directives.
//! Databases written by the Python `SqliteStore` open unchanged, and every
//! row this store writes reads back through the Python store: timestamps
//! use [`crate::event_serde::format_timestamp`], JSON columns use the same
//! key names, `status` / `outcome` / `grader_type` are their snake_case
//! wire strings, and `passed` is `0` / `1`.
//!
//! Per-connection invariants enforced on every open:
//!
//! * `PRAGMA busy_timeout` is set first, so the schema script (this
//!   connection's first contended write) waits for a competing writer
//!   instead of failing instantly.
//! * `PRAGMA foreign_keys = ON` is re-asserted after the schema script so a
//!   later schema-file edit cannot silently drop the guarantee.
//! * WAL mode is sticky at the database level for file-backed databases and
//!   falls back to `memory` for `:memory:` databases per SQLite semantics.
//!
//! Optimistic concurrency on `lifecycles.version` is a single
//! `UPDATE ... WHERE run_id = ? AND version = ?` checked by rows affected; a
//! zero-row result is disambiguated into
//! [`StoreConflictError::LifecycleNotFound`] or
//! [`StoreConflictError::OptimisticConcurrency`] by a follow-up read.
//!
//! `grader_results` is append-only: the trait surface exposes no update or
//! delete verb, and `BEFORE UPDATE` / `BEFORE DELETE` triggers reject any raw
//! mutation so the table matches the schema header even when a caller
//! bypasses the store.
//!
//! Thread-safety: the single connection lives behind a `Mutex`, so the store
//! is `Send + Sync` and shareable as `Arc<dyn HarnessStore>`; every public
//! verb is one lock acquisition. A write that stays locked past
//! `busy_timeout` (another process holding the file) is retried with a
//! capped exponential backoff before the still-locked error surfaces.
//!
//! [`StoreConflictError::LifecycleNotFound`]: crate::store::StoreConflictError::LifecycleNotFound
//! [`StoreConflictError::OptimisticConcurrency`]: crate::store::StoreConflictError::OptimisticConcurrency

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::types::ToSql;
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::{Map, Value};

use crate::event_serde::{
    EventRow, EventSerdeError, event_from_row, event_to_row, format_timestamp, parse_timestamp,
};
use crate::events::{DomainEvent, DomainEventKind, DomainEventPayload, apply};
use crate::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use crate::loaders::task_digest;
use crate::notifier::RunNotifier;
use crate::store::{
    AttemptStore, CURRENT_SCHEMA_VERSION, ControlCommandRecord, ControlCommandStore,
    DomainEventStore, GraderResultRecord, GraderResultStore, JsonObject, LifecycleFilter,
    LifecycleStore, SpendSummary, SpendWindow, StoreError, StoreSchemaError, TaskStore,
};
use crate::store_memory::{
    assemble_task, grader_result_from_event, parse_wire_enum, projected_attempt, task_definition,
};
use crate::task::{GraderType, Task};

/// The canonical DDL, embedded verbatim from its single source of truth.
pub const SCHEMA_SQL: &str = include_str!("../schema/persistence-schema.sql");

/// Append-only enforcement on `grader_results`. Applied post-bootstrap so
/// the canonical schema file is unchanged; `IF NOT EXISTS` makes the
/// triggers idempotent across reopens of the same file.
const APPEND_ONLY_TRIGGERS: &str = "
CREATE TRIGGER IF NOT EXISTS grader_results_no_update
BEFORE UPDATE ON grader_results
BEGIN
    SELECT RAISE(ABORT, 'grader_results is append-only');
END;

CREATE TRIGGER IF NOT EXISTS grader_results_no_delete
BEFORE DELETE ON grader_results
BEGIN
    SELECT RAISE(ABORT, 'grader_results is append-only');
END;
";

/// Default per-connection wait before SQLite reports `database is locked`.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
/// Bounded retry budget for the locked-past-`busy_timeout` path (the same
/// budget convention as the harness's transient retries).
pub const DEFAULT_MAX_TRANSIENT_RETRIES: u32 = 6;
/// Capped exponential backoff defaults shared with the Python fault policy:
/// `base * factor^attempt`, held at `cap`.
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(500);
pub const DEFAULT_BACKOFF_FACTOR: f64 = 2.0;
pub const DEFAULT_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// An injectable sleep for the transient-retry backoff (tests capture the
/// waits deterministically instead of blocking on a real clock).
pub type SleepFn = Arc<dyn Fn(Duration) + Send + Sync>;

/// Construction options for [`SqliteStore`].
#[derive(Clone)]
pub struct SqliteStoreOptions {
    /// Per-connection `PRAGMA busy_timeout`.
    pub busy_timeout: Duration,
    /// Retries after a write stays locked past `busy_timeout`.
    pub max_transient_retries: u32,
    pub backoff_base: Duration,
    pub backoff_factor: f64,
    pub backoff_cap: Duration,
    /// `None` sleeps on the real clock.
    pub transient_sleep: Option<SleepFn>,
    /// `None` gives the store its own [`RunNotifier`].
    pub notifier: Option<Arc<RunNotifier>>,
}

impl Default for SqliteStoreOptions {
    fn default() -> Self {
        Self {
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            max_transient_retries: DEFAULT_MAX_TRANSIENT_RETRIES,
            backoff_base: DEFAULT_BACKOFF_BASE,
            backoff_factor: DEFAULT_BACKOFF_FACTOR,
            backoff_cap: DEFAULT_BACKOFF_CAP,
            transient_sleep: None,
            notifier: None,
        }
    }
}

impl fmt::Debug for SqliteStoreOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteStoreOptions")
            .field("busy_timeout", &self.busy_timeout)
            .field("max_transient_retries", &self.max_transient_retries)
            .field("backoff_base", &self.backoff_base)
            .field("backoff_factor", &self.backoff_factor)
            .field("backoff_cap", &self.backoff_cap)
            .field("transient_sleep", &self.transient_sleep.is_some())
            .field("notifier", &self.notifier.is_some())
            .finish()
    }
}

impl SqliteStoreOptions {
    /// The capped wait for 0-based retry `attempt`.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        // An exponent past i32::MAX saturates (the result is capped below);
        // a wrapping cast would turn it negative and collapse the delay.
        let exponent = i32::try_from(attempt).unwrap_or(i32::MAX);
        let scaled = self.backoff_base.as_secs_f64() * self.backoff_factor.powi(exponent);
        let capped = scaled.min(self.backoff_cap.as_secs_f64()).max(0.0);
        Duration::from_secs_f64(capped)
    }
}

/// SQLite implementation of every store trait.
///
/// Each instance owns one connection in autocommit mode (each statement is
/// its own transaction unless a verb opens an explicit `BEGIN IMMEDIATE`).
/// WAL mode gives concurrent readers visibility while a writer is active.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    notifier: Arc<RunNotifier>,
    options: SqliteStoreOptions,
}

impl fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteStore")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Open (creating if absent) and bootstrap the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(path, SqliteStoreOptions::default())
    }

    pub fn open_with(
        path: impl AsRef<Path>,
        options: SqliteStoreOptions,
    ) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(StoreError::backend)?;
        Self::from_connection(conn, options)
    }

    /// An ephemeral in-process database.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::in_memory_with(SqliteStoreOptions::default())
    }

    pub fn in_memory_with(options: SqliteStoreOptions) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(StoreError::backend)?;
        Self::from_connection(conn, options)
    }

    fn from_connection(conn: Connection, options: SqliteStoreOptions) -> Result<Self, StoreError> {
        let notifier = options
            .notifier
            .clone()
            .unwrap_or_else(|| Arc::new(RunNotifier::new()));
        let store = Self {
            conn: Mutex::new(conn),
            notifier,
            options,
        };
        // Bootstrap under the same transient-retry envelope as every other
        // write: N workers opening one fresh file race the schema DDL, and a
        // loser that stays locked past busy_timeout must back off and re-run
        // (the script is idempotent) rather than fail the open.
        let busy_timeout = store.options.busy_timeout;
        store.with_retry(|conn| bootstrap(conn, busy_timeout))?;
        Ok(store)
    }

    /// The notifier every `append_domain_event` signals after committing.
    pub fn notifier(&self) -> &Arc<RunNotifier> {
        &self.notifier
    }

    pub fn options(&self) -> &SqliteStoreOptions {
        &self.options
    }

    /// Run `f` against the underlying connection under the store's lock.
    ///
    /// The escape hatch for consumers that share the database file (the
    /// orchestrator's own tables) and for inspection; the store's own
    /// invariants are not re-checked on whatever `f` does.
    pub fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let conn = self.lock();
        f(&conn)
    }

    /// Close the connection, surfacing any failure to flush.
    pub fn close(self) -> Result<(), StoreError> {
        let conn = self
            .conn
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.close().map_err(|(_, err)| StoreError::backend(err))
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `op`, retrying a write that stayed locked past `busy_timeout`.
    ///
    /// Every store verb is one atomic unit (an autocommit statement, or an
    /// explicit immediate transaction that rolls back on error), so a
    /// locked error is raised before any row commits and re-running
    /// double-applies nothing. Non-transient failures (schema mismatch,
    /// version conflict, corrupt row) surface immediately. On budget
    /// exhaustion the last locked error propagates, never a fabricated
    /// success.
    fn with_retry<T>(
        &self,
        mut op: impl FnMut(&mut Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut conn = self.lock();
        let mut attempt = 0u32;
        loop {
            match op(&mut conn) {
                Ok(value) => return Ok(value),
                Err(err) if is_locked(&err) && attempt < self.options.max_transient_retries => {
                    let delay = self.options.delay_for(attempt);
                    match &self.options.transient_sleep {
                        Some(sleep) => sleep(delay),
                        None => std::thread::sleep(delay),
                    }
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

// --- Bootstrap ------------------------------------------------------------------------

fn bootstrap(conn: &mut Connection, busy_timeout: Duration) -> Result<(), StoreError> {
    conn.busy_timeout(busy_timeout).map_err(db)?;
    conn.execute_batch(SCHEMA_SQL).map_err(db)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(db)?;
    conn.execute_batch(APPEND_ONLY_TRIGGERS).map_err(db)?;
    // Version pin: the schema_version table has CHECK (id = 1) so there is
    // at most one row. The one supported forward migration (v11 -> v12: add
    // the nullable lifecycles.source column) is applied in place first; it
    // is purely additive, so existing history survives. Any other mismatch
    // is refused rather than upgraded.
    let observed: Option<i64> = conn
        .query_row(
            "SELECT version FROM schema_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(db)?;
    let mut observed = observed.and_then(|v| u32::try_from(v).ok());
    if observed == Some(11) {
        conn.execute_batch(
            "ALTER TABLE lifecycles ADD COLUMN source TEXT;
             UPDATE schema_version SET version = 12 WHERE id = 1;",
        )
        .map_err(db)?;
        observed = Some(12);
    }
    if observed != Some(CURRENT_SCHEMA_VERSION) {
        return Err(StoreSchemaError::new(observed).into());
    }
    Ok(())
}

// --- Error helpers --------------------------------------------------------------------

fn db(err: rusqlite::Error) -> StoreError {
    StoreError::backend(err)
}

/// Whether `err` is SQLite reporting the file still locked after
/// `busy_timeout` elapsed (the transient class the retry wrapper covers).
fn is_locked(err: &StoreError) -> bool {
    let StoreError::Backend(source) = err else {
        return false;
    };
    matches!(
        source.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(ffi, _))
            if matches!(ffi.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Whether an insert failed on the `lifecycles.run_id` primary key.
fn is_duplicate_run(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(ffi, message) => {
            ffi.code == ErrorCode::ConstraintViolation
                && message
                    .as_deref()
                    .is_some_and(|m| m.contains("lifecycles.run_id") || m.contains("PRIMARY KEY"))
        }
        _ => false,
    }
}

fn ts(text: &str) -> Result<DateTime<Utc>, StoreError> {
    parse_timestamp(text).map_err(|e| StoreError::EventSerde(EventSerdeError::Timestamp(e)))
}

fn opt_ts(text: Option<&str>) -> Result<Option<DateTime<Utc>>, StoreError> {
    text.map(ts).transpose()
}

fn fmt_opt_ts(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(|t| format_timestamp(&t))
}

fn json_object(text: &str) -> Result<JsonObject, StoreError> {
    Ok(serde_json::from_str(text)?)
}

/// NULL for an empty identity-style string (`task_content_hash`, `source`),
/// matching the Python store's `value or None`.
fn null_if_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

// --- lifecycles -------------------------------------------------------------------------

const LIFECYCLE_COLUMNS: &str = "run_id, task_id, status, version, retries, error, agent_output, \
     session_id, artifacts_dir, worker_id, timestamps_json, blocked_requires_json, \
     task_content_hash, awaiting_manual_ordinal, source";

/// The `lifecycles` row as persisted, before any decoding.
struct RawLifecycle {
    run_id: String,
    task_id: String,
    status: String,
    version: u64,
    retries: u32,
    error: Option<String>,
    agent_output: Option<String>,
    session_id: Option<String>,
    artifacts_dir: Option<String>,
    worker_id: Option<String>,
    timestamps_json: String,
    blocked_requires_json: Option<String>,
    task_content_hash: Option<String>,
    awaiting_manual_ordinal: Option<u32>,
    source: Option<String>,
}

impl RawLifecycle {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            run_id: row.get(0)?,
            task_id: row.get(1)?,
            status: row.get(2)?,
            version: row.get(3)?,
            retries: row.get(4)?,
            error: row.get(5)?,
            agent_output: row.get(6)?,
            session_id: row.get(7)?,
            artifacts_dir: row.get(8)?,
            worker_id: row.get(9)?,
            timestamps_json: row.get(10)?,
            blocked_requires_json: row.get(11)?,
            task_content_hash: row.get(12)?,
            awaiting_manual_ordinal: row.get(13)?,
            source: row.get(14)?,
        })
    }

    /// The lifecycle row with `attempts` empty; the caller attaches them.
    fn decode(self) -> Result<Lifecycle, StoreError> {
        let mut lifecycle = Lifecycle::new(self.task_id).with_run_id(self.run_id);
        lifecycle.worker_id = self.worker_id.unwrap_or_default();
        lifecycle.status = parse_wire_enum(&self.status)?;
        lifecycle.timestamps = deserialize_timestamps(&self.timestamps_json)?;
        lifecycle.version = self.version;
        lifecycle.retries = self.retries;
        lifecycle.error = self.error.unwrap_or_default();
        lifecycle.agent_output = self.agent_output.unwrap_or_default();
        lifecycle.session_id = self.session_id.unwrap_or_default();
        lifecycle.artifacts_dir = self.artifacts_dir.unwrap_or_default();
        lifecycle.blocked_requires_json = self.blocked_requires_json;
        lifecycle.awaiting_manual_ordinal = self.awaiting_manual_ordinal;
        lifecycle.task_content_hash = self.task_content_hash.unwrap_or_default();
        lifecycle.source = self.source.unwrap_or_default();
        Ok(lifecycle)
    }
}

fn serialize_timestamps(timestamps: &std::collections::BTreeMap<Status, DateTime<Utc>>) -> String {
    let map: Map<String, Value> = timestamps
        .iter()
        .map(|(status, at)| {
            (
                status.as_str().to_owned(),
                Value::String(format_timestamp(at)),
            )
        })
        .collect();
    Value::Object(map).to_string()
}

fn deserialize_timestamps(
    text: &str,
) -> Result<std::collections::BTreeMap<Status, DateTime<Utc>>, StoreError> {
    let raw: Map<String, Value> = serde_json::from_str(text)?;
    let mut out = std::collections::BTreeMap::new();
    for (key, value) in raw {
        let status: Status = parse_wire_enum(&key)?;
        let text: String = serde_json::from_value(value)?;
        out.insert(status, ts(&text)?);
    }
    Ok(out)
}

fn ensure_task_identity(conn: &Connection, task_id: &str, now_iso: &str) -> Result<(), StoreError> {
    // INSERT OR IGNORE preserves the original first_seen_at; the FK anchor
    // exists after this call so versions and lifecycles can reference it.
    conn.execute(
        "INSERT OR IGNORE INTO tasks (id, first_seen_at, updated_at) VALUES (?1, ?2, ?3)",
        params![task_id, now_iso, now_iso],
    )
    .map_err(db)?;
    Ok(())
}

fn insert_lifecycle(conn: &Connection, lifecycle: &Lifecycle) -> Result<(), StoreError> {
    // Auto-register the task identity so lifecycles.task_id -> tasks(id)
    // always holds, covering callers that seed a lifecycle without a prior
    // save_task.
    let now_iso = format_timestamp(&Utc::now());
    ensure_task_identity(conn, &lifecycle.task_id, &now_iso)?;
    let result = conn.execute(
        "INSERT INTO lifecycles (
            run_id, task_id, status, version, retries, error,
            agent_output, session_id, artifacts_dir, worker_id,
            timestamps_json, updated_at, blocked_requires_json,
            task_content_hash, awaiting_manual_ordinal, source
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            lifecycle.run_id,
            lifecycle.task_id,
            lifecycle.status.as_str(),
            lifecycle.version,
            lifecycle.retries,
            lifecycle.error,
            lifecycle.agent_output,
            lifecycle.session_id,
            lifecycle.artifacts_dir,
            lifecycle.worker_id,
            serialize_timestamps(&lifecycle.timestamps),
            now_iso,
            lifecycle.blocked_requires_json,
            null_if_empty(&lifecycle.task_content_hash),
            lifecycle.awaiting_manual_ordinal,
            null_if_empty(&lifecycle.source),
        ],
    );
    match result {
        Ok(_) => Ok(()),
        Err(err) if is_duplicate_run(&err) => {
            Err(StoreError::lifecycle_already_exists(&lifecycle.run_id))
        }
        Err(err) => Err(db(err)),
    }
}

fn update_lifecycle_row(
    conn: &Connection,
    lifecycle: &Lifecycle,
    expected_version: u64,
) -> Result<(), StoreError> {
    let affected = conn
        .execute(
            "UPDATE lifecycles SET
                task_id = ?1, status = ?2, version = ?3, retries = ?4, error = ?5,
                agent_output = ?6, session_id = ?7, artifacts_dir = ?8, worker_id = ?9,
                timestamps_json = ?10, updated_at = ?11, blocked_requires_json = ?12,
                task_content_hash = ?13, awaiting_manual_ordinal = ?14, source = ?15
            WHERE run_id = ?16 AND version = ?17",
            params![
                lifecycle.task_id,
                lifecycle.status.as_str(),
                lifecycle.version,
                lifecycle.retries,
                lifecycle.error,
                lifecycle.agent_output,
                lifecycle.session_id,
                lifecycle.artifacts_dir,
                lifecycle.worker_id,
                serialize_timestamps(&lifecycle.timestamps),
                format_timestamp(&Utc::now()),
                lifecycle.blocked_requires_json,
                null_if_empty(&lifecycle.task_content_hash),
                lifecycle.awaiting_manual_ordinal,
                null_if_empty(&lifecycle.source),
                lifecycle.run_id,
                expected_version,
            ],
        )
        .map_err(db)?;
    if affected == 1 {
        return Ok(());
    }
    // Zero rows: disambiguate not-found from a version conflict.
    match stored_version(conn, &lifecycle.run_id)? {
        None => Err(StoreError::lifecycle_not_found(&lifecycle.run_id)),
        Some(actual) => Err(StoreError::optimistic_concurrency(
            &lifecycle.run_id,
            expected_version,
            actual,
        )),
    }
}

fn stored_version(conn: &Connection, run_id: &str) -> Result<Option<u64>, StoreError> {
    conn.query_row(
        "SELECT version FROM lifecycles WHERE run_id = ?1",
        params![run_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(db)
}

fn load_lifecycle_row(conn: &Connection, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
    let raw = conn
        .query_row(
            &format!("SELECT {LIFECYCLE_COLUMNS} FROM lifecycles WHERE run_id = ?1"),
            params![run_id],
            RawLifecycle::read,
        )
        .optional()
        .map_err(db)?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut lifecycle = raw.decode()?;
    lifecycle.attempts = list_attempt_rows(conn, run_id)?;
    Ok(Some(lifecycle))
}

/// The SQL filter for `list_lifecycles`; `None` when the filter can match
/// nothing (an empty status set).
fn filter_clause(filter: &LifecycleFilter) -> Option<(String, Vec<Box<dyn ToSql>>)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(statuses) = &filter.statuses {
        if statuses.is_empty() {
            return None;
        }
        let placeholders: Vec<String> = (0..statuses.len())
            .map(|i| format!("?{}", params.len() + i + 1))
            .collect();
        clauses.push(format!("l.status IN ({})", placeholders.join(", ")));
        for status in statuses {
            params.push(Box::new(status.as_str()));
        }
    }
    if let Some(task_id) = &filter.task_id {
        clauses.push(format!("l.task_id = ?{}", params.len() + 1));
        params.push(Box::new(task_id.clone()));
    }
    if let Some(task_ids) = &filter.task_ids {
        if task_ids.is_empty() {
            return None;
        }
        // One JSON-array parameter rather than one placeholder per id, so the
        // set size is not bounded by SQLITE_MAX_VARIABLE_NUMBER.
        clauses.push(format!(
            "l.task_id IN (SELECT value FROM json_each(?{}))",
            params.len() + 1
        ));
        params.push(Box::new(Value::from(task_ids.clone()).to_string()));
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    Some((where_clause, params))
}

fn list_lifecycle_rows(
    conn: &Connection,
    filter: &LifecycleFilter,
) -> Result<Vec<Lifecycle>, StoreError> {
    let Some((where_clause, params)) = filter_clause(filter) else {
        return Ok(Vec::new());
    };
    let param_refs: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
    // Two queries regardless of row count: the ordered lifecycle rows, then
    // every attempt of a matched run in one pass (joined on the same
    // filter), grouped in memory. No per-row follow-up reads.
    let mut lifecycles: Vec<Lifecycle> = {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {LIFECYCLE_COLUMNS} FROM lifecycles l {where_clause} \
                 ORDER BY l.updated_at DESC, l.run_id DESC"
            ))
            .map_err(db)?;
        let raws = stmt
            .query_map(param_refs.as_slice(), RawLifecycle::read)
            .map_err(db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db)?;
        raws.into_iter()
            .map(RawLifecycle::decode)
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut attempts_by_run: HashMap<String, Vec<Attempt>> = HashMap::new();
    {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT a.run_id, {ATTEMPT_COLUMNS} FROM attempts a \
                 JOIN lifecycles l ON l.run_id = a.run_id {where_clause} \
                 ORDER BY a.run_id, a.number"
            ))
            .map_err(db)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let run_id: String = row.get(0)?;
                Ok((run_id, RawAttempt::read_offset(row, 1)?))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db)?;
        for (run_id, raw) in rows {
            attempts_by_run
                .entry(run_id)
                .or_default()
                .push(raw.decode()?);
        }
    }
    for lifecycle in &mut lifecycles {
        if let Some(attempts) = attempts_by_run.remove(&lifecycle.run_id) {
            lifecycle.attempts = attempts;
        }
    }
    Ok(lifecycles)
}

fn summarize_spend_rows(
    conn: &Connection,
    window: &SpendWindow,
) -> Result<SpendSummary, StoreError> {
    if window.is_unbounded() {
        // The grand total is a single aggregate query.
        return conn
            .query_row(
                "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cache_creation_input_tokens), 0),
                        COALESCE(SUM(cache_read_input_tokens), 0),
                        COALESCE(SUM(total_cost_usd), 0.0)
                 FROM attempts",
                [],
                |row| {
                    Ok(SpendSummary {
                        input_tokens: row.get(0)?,
                        output_tokens: row.get(1)?,
                        cache_creation_tokens: row.get(2)?,
                        cache_read_tokens: row.get(3)?,
                        total_cost_usd: row.get(4)?,
                    })
                },
            )
            .map_err(db);
    }
    // A bounded window applies the half-open predicate against parsed
    // instants, never a raw text compare, so rows written with any
    // timestamp offset place identically across backends. Rows with no
    // activity instant are excluded from every bounded window by contract
    // and are filtered out in SQL.
    let mut stmt = conn
        .prepare(
            "SELECT input_tokens, output_tokens, cache_creation_input_tokens,
                    cache_read_input_tokens, total_cost_usd, last_activity_at
             FROM attempts WHERE last_activity_at IS NOT NULL",
        )
        .map_err(db)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(db)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db)?;
    let mut summary = SpendSummary::default();
    for (input, output, cache_creation, cache_read, cost, activity) in rows {
        if !window.includes(Some(ts(&activity)?)) {
            continue;
        }
        summary.input_tokens += input;
        summary.output_tokens += output;
        summary.cache_creation_tokens += cache_creation;
        summary.cache_read_tokens += cache_read;
        summary.total_cost_usd += cost;
    }
    Ok(summary)
}

// --- attempts -----------------------------------------------------------------------------

const ATTEMPT_COLUMNS: &str = "a.number, a.attempt_run_id, a.started_at, a.ended_at, a.outcome, \
     a.agent_output, a.error, a.agent_context_json, a.input_tokens, a.output_tokens, \
     a.cache_creation_input_tokens, a.cache_read_input_tokens, a.iterations_completed, \
     a.turns, a.total_cost_usd, a.last_activity_at";

struct RawAttempt {
    number: u32,
    attempt_run_id: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    outcome: Option<String>,
    agent_output: Option<String>,
    error: Option<String>,
    agent_context_json: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    iterations_completed: u64,
    turns: u64,
    total_cost_usd: f64,
    last_activity_at: Option<String>,
}

impl RawAttempt {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Self::read_offset(row, 0)
    }

    fn read_offset(row: &Row<'_>, at: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            number: row.get(at)?,
            attempt_run_id: row.get(at + 1)?,
            started_at: row.get(at + 2)?,
            ended_at: row.get(at + 3)?,
            outcome: row.get(at + 4)?,
            agent_output: row.get(at + 5)?,
            error: row.get(at + 6)?,
            agent_context_json: row.get(at + 7)?,
            input_tokens: row.get(at + 8)?,
            output_tokens: row.get(at + 9)?,
            cache_creation_input_tokens: row.get(at + 10)?,
            cache_read_input_tokens: row.get(at + 11)?,
            iterations_completed: row.get(at + 12)?,
            turns: row.get(at + 13)?,
            total_cost_usd: row.get(at + 14)?,
            last_activity_at: row.get(at + 15)?,
        })
    }

    fn decode(self) -> Result<Attempt, StoreError> {
        let mut attempt = Attempt::new(
            self.number,
            ts(&self.started_at)?,
            self.attempt_run_id.unwrap_or_default(),
        );
        attempt.ended_at = opt_ts(self.ended_at.as_deref())?;
        attempt.outcome = self
            .outcome
            .as_deref()
            .filter(|o| !o.is_empty())
            .map(parse_wire_enum::<Outcome>)
            .transpose()?;
        attempt.agent_output = self.agent_output.unwrap_or_default();
        attempt.error = self.error.unwrap_or_default();
        attempt.agent_context = match self.agent_context_json.as_deref() {
            Some(text) if !text.is_empty() => serde_json::from_str(text)?,
            _ => Default::default(),
        };
        attempt.input_tokens = self.input_tokens;
        attempt.output_tokens = self.output_tokens;
        attempt.cache_creation_input_tokens = self.cache_creation_input_tokens;
        attempt.cache_read_input_tokens = self.cache_read_input_tokens;
        attempt.iterations_completed = self.iterations_completed;
        attempt.turns = self.turns;
        attempt.total_cost_usd = self.total_cost_usd;
        attempt.last_activity_at = opt_ts(self.last_activity_at.as_deref())?;
        Ok(attempt)
    }
}

fn upsert_attempt(conn: &Connection, run_id: &str, attempt: &Attempt) -> Result<(), StoreError> {
    // A true upsert on the (run_id, number) primary key: the existing row is
    // updated in place rather than deleted and re-inserted, so dependent
    // grader_results rows and the rowid are untouched. The harness records
    // both the start and the finalization of an attempt through this verb.
    conn.execute(
        "INSERT INTO attempts (
            run_id, number, attempt_run_id, started_at, ended_at, outcome,
            agent_output, error, agent_context_json, input_tokens, output_tokens,
            cache_creation_input_tokens, cache_read_input_tokens,
            iterations_completed, turns, total_cost_usd, last_activity_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
        ON CONFLICT(run_id, number) DO UPDATE SET
            attempt_run_id = excluded.attempt_run_id,
            started_at = excluded.started_at,
            ended_at = excluded.ended_at,
            outcome = excluded.outcome,
            agent_output = excluded.agent_output,
            error = excluded.error,
            agent_context_json = excluded.agent_context_json,
            input_tokens = excluded.input_tokens,
            output_tokens = excluded.output_tokens,
            cache_creation_input_tokens = excluded.cache_creation_input_tokens,
            cache_read_input_tokens = excluded.cache_read_input_tokens,
            iterations_completed = excluded.iterations_completed,
            turns = excluded.turns,
            total_cost_usd = excluded.total_cost_usd,
            last_activity_at = excluded.last_activity_at",
        params![
            run_id,
            attempt.number,
            attempt.run_id,
            format_timestamp(&attempt.started_at),
            fmt_opt_ts(attempt.ended_at),
            attempt.outcome.map(Outcome::as_str),
            attempt.agent_output,
            attempt.error,
            serde_json::to_string(&attempt.agent_context)?,
            attempt.input_tokens,
            attempt.output_tokens,
            attempt.cache_creation_input_tokens,
            attempt.cache_read_input_tokens,
            attempt.iterations_completed,
            attempt.turns,
            attempt.total_cost_usd,
            fmt_opt_ts(attempt.last_activity_at),
        ],
    )
    .map_err(db)?;
    Ok(())
}

fn list_attempt_rows(conn: &Connection, run_id: &str) -> Result<Vec<Attempt>, StoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM attempts a WHERE a.run_id = ?1 ORDER BY a.number"
        ))
        .map_err(db)?;
    let raws = stmt
        .query_map(params![run_id], RawAttempt::read)
        .map_err(db)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db)?;
    raws.into_iter().map(RawAttempt::decode).collect()
}

fn load_attempt_row(
    conn: &Connection,
    run_id: &str,
    number: u32,
) -> Result<Option<Attempt>, StoreError> {
    conn.query_row(
        &format!("SELECT {ATTEMPT_COLUMNS} FROM attempts a WHERE a.run_id = ?1 AND a.number = ?2"),
        params![run_id, number],
        RawAttempt::read,
    )
    .optional()
    .map_err(db)?
    .map(RawAttempt::decode)
    .transpose()
}

// --- events ---------------------------------------------------------------------------------

/// `MAX(sequence) + 1` over the run's rows. Always called inside the append
/// transaction (`BEGIN IMMEDIATE` serializes writers) so per-run ordering
/// stays strictly monotonic; the `UNIQUE (run_id, sequence)` constraint
/// backstops any writer that bypasses it.
fn next_domain_sequence(conn: &Connection, run_id: &str) -> Result<u64, StoreError> {
    conn.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE run_id = ?1",
        params![run_id],
        |row| row.get(0),
    )
    .map_err(db)
}

fn insert_domain_event(conn: &Connection, event: &DomainEvent) -> Result<u64, StoreError> {
    let sequence = next_domain_sequence(conn, &event.run_id)?;
    let row = event_to_row(event)?;
    conn.execute(
        "INSERT INTO events (run_id, attempt_number, ts, kind, payload_json, sequence)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            row.run_id,
            row.attempt_number,
            row.ts,
            row.kind,
            row.payload_json,
            sequence
        ],
    )
    .map_err(db)?;
    Ok(sequence)
}

const EVENT_COLUMNS: &str = "id, run_id, attempt_number, ts, kind, payload_json, sequence";

fn read_event_row(row: &Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        attempt_number: row.get(2)?,
        ts: row.get(3)?,
        kind: row.get(4)?,
        payload_json: row.get(5)?,
        sequence: row.get(6)?,
    })
}

fn list_domain_event_rows(conn: &Connection, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE run_id = ?1 ORDER BY sequence"
        ))
        .map_err(db)?;
    let rows = stmt
        .query_map(params![run_id], read_event_row)
        .map_err(db)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db)?;
    rows.iter()
        .map(|row| event_from_row(row).map_err(StoreError::from))
        .collect()
}

/// The run's highest-sequence `kind` row, walking the `(run_id, sequence)`
/// unique index backwards and decoding only the matching row.
fn latest_domain_event_row_of_kind(
    conn: &Connection,
    run_id: &str,
    kind: DomainEventKind,
) -> Result<Option<DomainEvent>, StoreError> {
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE run_id = ?1 AND kind = ?2 \
             ORDER BY sequence DESC LIMIT 1"
        ))
        .map_err(db)?;
    let row = stmt
        .query_row(params![run_id, kind.as_str()], read_event_row)
        .optional()
        .map_err(db)?;
    row.as_ref()
        .map(|row| event_from_row(row).map_err(StoreError::from))
        .transpose()
}

/// Maintain the attempts / grader_results read-model projections inside the
/// append transaction so they commit atomically with the event row and the
/// lifecycle row.
fn project_domain_event(
    conn: &Connection,
    event: &DomainEvent,
    folded: &Lifecycle,
) -> Result<(), StoreError> {
    match &event.payload {
        DomainEventPayload::AttemptStarted(a) => {
            upsert_attempt(conn, &folded.run_id, projected_attempt(folded, a.number)?)
        }
        DomainEventPayload::AttemptFinalized(f) => {
            upsert_attempt(conn, &folded.run_id, projected_attempt(folded, f.number)?)
        }
        DomainEventPayload::GraderEvaluated(g) => {
            insert_grader_result(conn, &grader_result_from_event(event, g)?).map(|_| ())
        }
        _ => Ok(()),
    }
}

// --- grader_results -------------------------------------------------------------------

struct RawGraderResult {
    id: i64,
    run_id: String,
    attempt_number: u32,
    ordinal: u32,
    grader_type: String,
    grader_name: Option<String>,
    grader_spec_json: String,
    passed: i64,
    duration_ms: u64,
    payload_json: String,
    ts: String,
}

impl RawGraderResult {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            run_id: row.get(1)?,
            attempt_number: row.get(2)?,
            ordinal: row.get(3)?,
            grader_type: row.get(4)?,
            grader_name: row.get(5)?,
            grader_spec_json: row.get(6)?,
            passed: row.get(7)?,
            duration_ms: row.get(8)?,
            payload_json: row.get(9)?,
            ts: row.get(10)?,
        })
    }

    fn decode(self) -> Result<GraderResultRecord, StoreError> {
        Ok(GraderResultRecord {
            run_id: self.run_id,
            attempt_number: self.attempt_number,
            ordinal: self.ordinal,
            grader_type: parse_wire_enum::<GraderType>(&self.grader_type)?,
            grader_spec: json_object(&self.grader_spec_json)?,
            passed: self.passed != 0,
            duration_ms: self.duration_ms,
            payload: json_object(&self.payload_json)?,
            ts: ts(&self.ts)?,
            grader_name: self.grader_name,
            id: Some(self.id),
        })
    }
}

fn insert_grader_result(
    conn: &Connection,
    result: &GraderResultRecord,
) -> Result<GraderResultRecord, StoreError> {
    conn.execute(
        "INSERT INTO grader_results (
            run_id, attempt_number, ordinal, grader_type, grader_name,
            grader_spec_json, passed, duration_ms, payload_json, ts
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            result.run_id,
            result.attempt_number,
            result.ordinal,
            result.grader_type.as_str(),
            result.grader_name,
            Value::Object(result.grader_spec.clone()).to_string(),
            i64::from(result.passed),
            result.duration_ms,
            Value::Object(result.payload.clone()).to_string(),
            format_timestamp(&result.ts),
        ],
    )
    .map_err(db)?;
    let mut stored = result.clone();
    stored.id = Some(conn.last_insert_rowid());
    Ok(stored)
}

fn list_grader_result_rows(
    conn: &Connection,
    run_id: &str,
    attempt_number: u32,
) -> Result<Vec<GraderResultRecord>, StoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, run_id, attempt_number, ordinal, grader_type, grader_name,
                    grader_spec_json, passed, duration_ms, payload_json, ts
             FROM grader_results WHERE run_id = ?1 AND attempt_number = ?2
             ORDER BY ordinal",
        )
        .map_err(db)?;
    let raws = stmt
        .query_map(params![run_id, attempt_number], RawGraderResult::read)
        .map_err(db)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db)?;
    raws.into_iter().map(RawGraderResult::decode).collect()
}

// --- control_commands ------------------------------------------------------------------

struct RawControlCommand {
    id: i64,
    run_id: String,
    kind: String,
    payload_json: String,
    enqueued_at: String,
    claimed_at: Option<String>,
}

impl RawControlCommand {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            run_id: row.get(1)?,
            kind: row.get(2)?,
            payload_json: row.get(3)?,
            enqueued_at: row.get(4)?,
            claimed_at: row.get(5)?,
        })
    }

    fn decode(self) -> Result<ControlCommandRecord, StoreError> {
        Ok(ControlCommandRecord {
            run_id: self.run_id,
            kind: self.kind,
            payload: json_object(&self.payload_json)?,
            enqueued_at: ts(&self.enqueued_at)?,
            claimed_at: opt_ts(self.claimed_at.as_deref())?,
            id: Some(self.id),
        })
    }
}

// --- Trait implementations ----------------------------------------------------------------

impl LifecycleStore for SqliteStore {
    fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
        self.with_retry(|conn| insert_lifecycle(conn, lifecycle))
    }

    fn update_lifecycle(
        &self,
        lifecycle: &Lifecycle,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        self.with_retry(|conn| update_lifecycle_row(conn, lifecycle, expected_version))
    }

    fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
        self.with_retry(|conn| load_lifecycle_row(conn, run_id))
    }

    fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
        self.with_retry(|conn| list_lifecycle_rows(conn, filter))
    }

    fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
        self.with_retry(|conn| summarize_spend_rows(conn, window))
    }
}

impl TaskStore for SqliteStore {
    fn save_task(&self, task: &Task, now: DateTime<Utc>) -> Result<String, StoreError> {
        let content_hash = task_digest(task);
        let definition = task_definition(task)?;
        let now_iso = format_timestamp(&now);
        let goal = definition
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let column = |key: &str| {
            definition
                .get(key)
                .cloned()
                .unwrap_or(Value::Null)
                .to_string()
        };
        let (graders_json, tags_json, context_json) =
            (column("graders"), column("tags"), column("context"));
        self.with_retry(|conn| {
            // One transaction so the identity row and the version row land
            // together. The version row is immutable (INSERT OR IGNORE keeps
            // the original created_at); the whole definition is part of the
            // content hash.
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db)?;
            ensure_task_identity(&tx, &task.id, &now_iso)?;
            tx.execute(
                "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
                params![now_iso, task.id],
            )
            .map_err(db)?;
            tx.execute(
                "INSERT OR IGNORE INTO task_versions (
                    task_id, content_hash, goal, graders_json, tags_json, context_json, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    task.id,
                    content_hash,
                    goal,
                    graders_json,
                    tags_json,
                    context_json,
                    now_iso
                ],
            )
            .map_err(db)?;
            tx.commit().map_err(db)?;
            Ok(content_hash.clone())
        })
    }

    fn load_task(
        &self,
        task_id: &str,
        content_hash: Option<&str>,
    ) -> Result<Option<Task>, StoreError> {
        self.with_retry(|conn| load_task_row(conn, task_id, content_hash))
    }

    fn load_task_for_run(&self, run_id: &str) -> Result<Option<Task>, StoreError> {
        self.with_retry(|conn| {
            let pinned: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT task_id, task_content_hash FROM lifecycles WHERE run_id = ?1",
                    params![run_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(db)?;
            let Some((task_id, content_hash)) = pinned else {
                return Ok(None);
            };
            let content_hash = content_hash.filter(|h| !h.is_empty());
            load_task_row(conn, &task_id, content_hash.as_deref())
        })
    }
}

fn load_task_row(
    conn: &Connection,
    task_id: &str,
    content_hash: Option<&str>,
) -> Result<Option<Task>, StoreError> {
    let read = |row: &Row<'_>| -> rusqlite::Result<(String, String, String, String)> {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    };
    let row = match content_hash {
        Some(hash) => conn
            .query_row(
                "SELECT goal, graders_json, tags_json, context_json
                 FROM task_versions WHERE task_id = ?1 AND content_hash = ?2",
                params![task_id, hash],
                read,
            )
            .optional(),
        None => conn
            .query_row(
                "SELECT goal, graders_json, tags_json, context_json
                 FROM task_versions WHERE task_id = ?1
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![task_id],
                read,
            )
            .optional(),
    }
    .map_err(db)?;
    let Some((goal, graders_json, tags_json, context_json)) = row else {
        return Ok(None);
    };
    let mut definition = Map::new();
    definition.insert("goal".to_owned(), Value::String(goal));
    definition.insert("graders".to_owned(), serde_json::from_str(&graders_json)?);
    definition.insert("tags".to_owned(), serde_json::from_str(&tags_json)?);
    definition.insert("context".to_owned(), serde_json::from_str(&context_json)?);
    assemble_task(task_id, &definition).map(Some)
}

impl DomainEventStore for SqliteStore {
    fn append_domain_event(
        &self,
        event: &DomainEvent,
        expected_version: u64,
    ) -> Result<Lifecycle, StoreError> {
        let (folded, sequence) = self.with_retry(|conn| {
            // BEGIN IMMEDIATE acquires the write lock up front so a
            // concurrent appender cannot interleave between the version
            // check and the write; a failure anywhere rolls the whole unit
            // back when the transaction drops.
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db)?;
            let folded = match &event.payload {
                DomainEventPayload::LifecycleInitialized(_) => {
                    let seeded = apply(None, event)?;
                    insert_lifecycle(&tx, &seeded)?;
                    seeded
                }
                _ => {
                    let current = load_lifecycle_row(&tx, &event.run_id)?
                        .ok_or_else(|| StoreError::lifecycle_not_found(&event.run_id))?;
                    if current.version != expected_version {
                        return Err(StoreError::optimistic_concurrency(
                            &event.run_id,
                            expected_version,
                            current.version,
                        ));
                    }
                    let folded = apply(Some(&current), event)?;
                    update_lifecycle_row(&tx, &folded, expected_version)?;
                    project_domain_event(&tx, event, &folded)?;
                    folded
                }
            };
            let sequence = insert_domain_event(&tx, event)?;
            tx.commit().map_err(db)?;
            Ok((folded, sequence))
        })?;
        // Signal only after the transaction commits so a woken consumer
        // never observes a watermark for an uncommitted (or rolled-back)
        // append.
        self.notifier.notify(&event.run_id, sequence);
        Ok(folded)
    }

    fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
        self.with_retry(|conn| list_domain_event_rows(conn, run_id))
    }

    fn latest_domain_event_of_kind(
        &self,
        run_id: &str,
        kind: DomainEventKind,
    ) -> Result<Option<DomainEvent>, StoreError> {
        self.with_retry(|conn| latest_domain_event_row_of_kind(conn, run_id, kind))
    }
}

impl AttemptStore for SqliteStore {
    fn save_attempt(
        &self,
        run_id: &str,
        attempt: &Attempt,
        expected_version: Option<u64>,
    ) -> Result<(), StoreError> {
        self.with_retry(|conn| {
            let Some(expected) = expected_version else {
                // Unconditional upsert: a direct caller's write, or the
                // domain-event projection (already inside the
                // version-checked append transaction).
                return upsert_attempt(conn, run_id, attempt);
            };
            // Versioned write (the harness's iteration-boundary aggregate
            // rollup): verify the lifecycle's optimistic-concurrency key in
            // the same transaction as the upsert so a stale worker cannot
            // clobber counters after the run moved on.
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db)?;
            let actual = stored_version(&tx, run_id)?
                .ok_or_else(|| StoreError::lifecycle_not_found(run_id))?;
            if actual != expected {
                return Err(StoreError::optimistic_concurrency(run_id, expected, actual));
            }
            upsert_attempt(&tx, run_id, attempt)?;
            tx.commit().map_err(db)
        })
    }

    fn load_attempt(&self, run_id: &str, number: u32) -> Result<Option<Attempt>, StoreError> {
        self.with_retry(|conn| load_attempt_row(conn, run_id, number))
    }

    fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError> {
        self.with_retry(|conn| list_attempt_rows(conn, run_id))
    }
}

impl GraderResultStore for SqliteStore {
    fn append_grader_result(
        &self,
        result: &GraderResultRecord,
    ) -> Result<GraderResultRecord, StoreError> {
        self.with_retry(|conn| insert_grader_result(conn, result))
    }

    fn list_grader_results(
        &self,
        run_id: &str,
        attempt_number: u32,
    ) -> Result<Vec<GraderResultRecord>, StoreError> {
        self.with_retry(|conn| list_grader_result_rows(conn, run_id, attempt_number))
    }
}

impl ControlCommandStore for SqliteStore {
    fn enqueue_command(
        &self,
        run_id: &str,
        kind: &str,
        payload: JsonObject,
        now: DateTime<Utc>,
    ) -> Result<ControlCommandRecord, StoreError> {
        // Persist verbatim. claimed_at is left NULL so the row joins the
        // pending queue immediately; id is assigned by AUTOINCREMENT and is
        // the enqueue-order key claim_commands consumes.
        let payload_json = Value::Object(payload.clone()).to_string();
        let enqueued_at = format_timestamp(&now);
        let id = self.with_retry(|conn| {
            conn.execute(
                "INSERT INTO control_commands (run_id, kind, payload_json, enqueued_at, claimed_at)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                params![run_id, kind, payload_json, enqueued_at],
            )
            .map_err(db)?;
            Ok(conn.last_insert_rowid())
        })?;
        let mut record = ControlCommandRecord::pending(run_id, kind, payload, now);
        record.id = Some(id);
        Ok(record)
    }

    fn claim_commands(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<ControlCommandRecord>, StoreError> {
        // Claim-once is one atomic UPDATE: every still-pending row for the
        // run flips to claimed_at = now in a single statement, so a racing
        // claim (another worker, a watcher restart) finds no rows to take.
        // RETURNING yields rows in arbitrary order; sort by id afterwards to
        // honor the enqueue-order contract.
        let claimed_at = format_timestamp(&now);
        let mut records = self.with_retry(|conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db)?;
            let raws = {
                let mut stmt = tx
                    .prepare(
                        "UPDATE control_commands SET claimed_at = ?1
                         WHERE run_id = ?2 AND claimed_at IS NULL
                         RETURNING id, run_id, kind, payload_json, enqueued_at, claimed_at",
                    )
                    .map_err(db)?;
                stmt.query_map(params![claimed_at, run_id], RawControlCommand::read)
                    .map_err(db)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db)?
            };
            tx.commit().map_err(db)?;
            raws.into_iter()
                .map(RawControlCommand::decode)
                .collect::<Result<Vec<_>, _>>()
        })?;
        records.sort_by_key(|r| r.id);
        Ok(records)
    }

    fn delete_command(&self, command_id: i64) -> Result<(), StoreError> {
        // Queue hygiene (spec 00025 FR-10): the consumer deletes an applied
        // row only after the steering domain event committed. Unknown ids
        // are a no-op by contract.
        self.with_retry(|conn| {
            conn.execute(
                "DELETE FROM control_commands WHERE id = ?1",
                params![command_id],
            )
            .map_err(db)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::LifecycleInitialized;
    use crate::store::{HarnessStore, StoreConflictError};
    use std::collections::BTreeSet;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_780_000_000 + secs, 0).unwrap()
    }

    fn table_names(store: &SqliteStore) -> BTreeSet<String> {
        store.with_connection(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' \
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        })
    }

    #[test]
    fn schema_is_embedded_from_the_canonical_file() {
        assert!(SCHEMA_SQL.contains("PRAGMA journal_mode = WAL"));
        assert!(SCHEMA_SQL.contains("PRAGMA foreign_keys = ON"));
        assert!(
            SCHEMA_SQL
                .contains("INSERT OR IGNORE INTO schema_version (id, version) VALUES (1, 12)")
        );
    }

    #[test]
    fn bootstrap_creates_every_schema_table() {
        let store = SqliteStore::in_memory().unwrap();
        let expected: BTreeSet<String> = [
            "tasks",
            "task_versions",
            "lifecycles",
            "attempts",
            "events",
            "grader_results",
            "schema_version",
            "control_commands",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(table_names(&store), expected);
    }

    #[test]
    fn bootstrap_is_idempotent_across_reopens_and_installs_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.db");
        SqliteStore::open(&path).unwrap().close().unwrap();
        let store = SqliteStore::open(&path).unwrap();
        let triggers: BTreeSet<String> = store.with_connection(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'trigger' \
                     AND tbl_name = 'grader_results'",
                )
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        });
        assert_eq!(
            triggers,
            ["grader_results_no_update", "grader_results_no_delete"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn pragmas_apply_on_every_connection_and_wal_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.db");
        let s1 = SqliteStore::open(&path).unwrap();
        let s2 = SqliteStore::open(&path).unwrap();
        for store in [&s1, &s2] {
            let (fk, mode): (i64, String) = store.with_connection(|conn| {
                (
                    conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                        .unwrap(),
                    conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                        .unwrap(),
                )
            });
            assert_eq!(fk, 1);
            assert_eq!(mode, "wal");
        }
        s2.close().unwrap();
        s1.close().unwrap();
        let raw = Connection::open(&path).unwrap();
        let mode: String = raw
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn foreign_keys_reject_orphan_event_insert() {
        let store = SqliteStore::in_memory().unwrap();
        let err = store.with_connection(|conn| {
            conn.execute(
                "INSERT INTO events (run_id, ts, kind, payload_json, sequence) \
                 VALUES ('missing-run', '2024-01-01T00:00:00+00:00', 'x', '{}', 1)",
                [],
            )
            .unwrap_err()
        });
        assert!(matches!(
            err,
            rusqlite::Error::SqliteFailure(ffi, _) if ffi.code == ErrorCode::ConstraintViolation
        ));
    }

    #[test]
    fn grader_results_are_append_only_at_the_db_level() {
        let store = SqliteStore::in_memory().unwrap();
        store
            .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
            .unwrap();
        store
            .save_attempt("r1", &Attempt::new(1, at(0), "r1"), None)
            .unwrap();
        let receipt = GraderResultRecord {
            run_id: "r1".into(),
            attempt_number: 1,
            ordinal: 0,
            grader_type: GraderType::Command,
            grader_spec: json_object(r#"{"type": "command", "run": "true"}"#).unwrap(),
            passed: true,
            duration_ms: 1,
            payload: json_object(r#"{"exit_code": 0}"#).unwrap(),
            ts: at(0),
            grader_name: None,
            id: None,
        };
        store.append_grader_result(&receipt).unwrap();
        for sql in [
            "UPDATE grader_results SET passed = 0 WHERE id = 1",
            "DELETE FROM grader_results WHERE id = 1",
        ] {
            let err = store.with_connection(|conn| conn.execute(sql, []).unwrap_err());
            assert!(err.to_string().contains("append-only"), "{sql}: {err}");
        }
        assert_eq!(store.list_grader_results("r1", 1).unwrap().len(), 1);
    }

    #[test]
    fn lifecycle_state_survives_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("durable.db");
        let s1 = SqliteStore::open(&path).unwrap();
        let mut lc = Lifecycle::new("t").with_run_id("r1");
        lc.transition_to(Status::Ready, "", Some(at(1))).unwrap();
        lc.blocked_requires_json = Some(r#"[{"type": "file_exists", "path": "/tmp/x"}]"#.into());
        s1.create_lifecycle(&lc).unwrap();
        s1.close().unwrap();
        let s2 = SqliteStore::open(&path).unwrap();
        let loaded = s2.load_lifecycle("r1").unwrap().unwrap();
        assert_eq!(loaded.status, Status::Ready);
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.blocked_requires_json, lc.blocked_requires_json);
        assert_eq!(loaded.timestamps.get(&Status::Ready), Some(&at(1)));
    }

    #[test]
    fn optimistic_concurrency_conflict_across_two_open_stores() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrency.db");
        let s1 = SqliteStore::open(&path).unwrap();
        let s2 = SqliteStore::open(&path).unwrap();
        s1.create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
            .unwrap();
        let mut loaded = s1.load_lifecycle("r1").unwrap().unwrap();
        loaded
            .transition_to(Status::Ready, "", Some(at(1)))
            .unwrap();
        s1.update_lifecycle(&loaded, 1).unwrap();

        let mut stale = Lifecycle::new("t").with_run_id("r1");
        stale.status = Status::Ready;
        stale.version = 2;
        let err = s2.update_lifecycle(&stale, 1).unwrap_err();
        assert_eq!(
            err.as_conflict(),
            Some(&StoreConflictError::OptimisticConcurrency {
                run_id: "r1".into(),
                expected_version: 1,
                actual_version: 2,
            })
        );
        assert_eq!(s2.load_lifecycle("r1").unwrap().unwrap().version, 2);
    }

    #[test]
    fn mismatched_schema_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mismatch.db");
        SqliteStore::open(&path).unwrap().close().unwrap();
        let raw = Connection::open(&path).unwrap();
        raw.execute(
            "UPDATE schema_version SET version = ?1 WHERE id = 1",
            params![CURRENT_SCHEMA_VERSION + 99],
        )
        .unwrap();
        raw.close().unwrap();
        let err = SqliteStore::open(&path).unwrap_err();
        assert!(err.is_schema_mismatch());
        assert!(err.to_string().contains("store must be re-created"));
        let StoreError::Schema(schema) = err else {
            panic!("expected schema error");
        };
        assert_eq!(schema.observed_version, Some(CURRENT_SCHEMA_VERSION + 99));
        assert_eq!(schema.expected_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn fresh_store_records_current_schema_version() {
        let store = SqliteStore::in_memory().unwrap();
        let version: i64 = store.with_connection(|conn| {
            conn.query_row("SELECT version FROM schema_version WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap()
        });
        assert_eq!(version, i64::from(CURRENT_SCHEMA_VERSION));
    }

    fn downgrade_to_v11(path: &Path) {
        let raw = Connection::open(path).unwrap();
        raw.execute_batch(
            "ALTER TABLE lifecycles DROP COLUMN source;
             UPDATE schema_version SET version = 11 WHERE id = 1;",
        )
        .unwrap();
        raw.close().unwrap();
    }

    #[test]
    fn v11_database_is_forward_migrated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v11.db");
        let store = SqliteStore::open(&path).unwrap();
        store
            .create_lifecycle(&Lifecycle::new("t1").with_run_id("run-old"))
            .unwrap();
        store.close().unwrap();
        downgrade_to_v11(&path);

        let migrated = SqliteStore::open(&path).unwrap();
        let version: i64 = migrated.with_connection(|conn| {
            conn.query_row("SELECT version FROM schema_version WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap()
        });
        assert_eq!(version, i64::from(CURRENT_SCHEMA_VERSION));
        let old = migrated.load_lifecycle("run-old").unwrap().unwrap();
        assert_eq!(old.source, "");
        let mut fresh = Lifecycle::new("t1").with_run_id("run-new");
        fresh.source = ".flywheel/tasks/active/30-history/t1.json".into();
        migrated.create_lifecycle(&fresh).unwrap();
        assert_eq!(
            migrated.load_lifecycle("run-new").unwrap().unwrap().source,
            fresh.source
        );
        migrated.close().unwrap();
        // Reopening after the migration must not re-add the column.
        SqliteStore::open(&path).unwrap().close().unwrap();
    }

    #[test]
    fn source_persists_as_null_when_empty_and_round_trips_when_set() {
        let store = SqliteStore::in_memory().unwrap();
        let mut with_source = Lifecycle::new("t1").with_run_id("run-a");
        with_source.source = "owner/repo#7".into();
        store.create_lifecycle(&with_source).unwrap();
        store
            .create_lifecycle(&Lifecycle::new("t1").with_run_id("run-b"))
            .unwrap();
        assert_eq!(
            store.load_lifecycle("run-a").unwrap().unwrap().source,
            "owner/repo#7"
        );
        let raw: Option<String> = store.with_connection(|conn| {
            conn.query_row(
                "SELECT source FROM lifecycles WHERE run_id = 'run-b'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(raw, None);
        assert_eq!(store.load_lifecycle("run-b").unwrap().unwrap().source, "");
    }

    #[test]
    fn seed_event_carries_source_through_fold_and_replay() {
        let store = SqliteStore::in_memory().unwrap();
        let seed = DomainEvent::new(
            "run-s",
            at(0),
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: "t1".into(),
                source: ".flywheel/tasks/active/30-history/t1.json".into(),
                ..Default::default()
            }),
        );
        let folded = store.append_domain_event(&seed, 0).unwrap();
        assert_eq!(folded.source, ".flywheel/tasks/active/30-history/t1.json");
        let loaded = store.load_lifecycle("run-s").unwrap().unwrap();
        assert_eq!(loaded.source, folded.source);
        let replayed = crate::events::replay(&store.list_domain_events("run-s").unwrap()).unwrap();
        assert_eq!(replayed.source, folded.source);
        assert_eq!(store.notifier().watermark("run-s"), 1);
    }

    fn fast_backoff() -> SqliteStoreOptions {
        SqliteStoreOptions {
            busy_timeout: Duration::from_millis(25),
            backoff_base: Duration::from_millis(1),
            backoff_factor: 1.0,
            backoff_cap: Duration::from_millis(1),
            ..Default::default()
        }
    }

    #[test]
    fn locked_retry_returns_the_real_result_once_the_lock_clears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked-retry.db");
        let holder: Arc<Mutex<Option<Connection>>> = Arc::new(Mutex::new(None));
        let release = {
            let holder = Arc::clone(&holder);
            Arc::new(move |_delay: Duration| {
                // The first backoff releases the competing writer, so the
                // next retry acquires the write lock and completes for real.
                if let Some(blocker) = holder.lock().unwrap().take() {
                    blocker.execute_batch("ROLLBACK").unwrap();
                    blocker.close().unwrap();
                }
            }) as SleepFn
        };
        // Construct before the lock is taken so the bootstrap is uncontended.
        let store = SqliteStore::open_with(
            &path,
            SqliteStoreOptions {
                max_transient_retries: 5,
                transient_sleep: Some(release),
                ..fast_backoff()
            },
        )
        .unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        *holder.lock().unwrap() = Some(blocker);

        let task = Task::new(
            "survive a held write lock",
            vec![crate::task::Grader::Command(
                crate::task::CommandGrader::new("true").unwrap(),
            )],
        )
        .with_id("t-locked");
        let digest = store.save_task(&task, at(0)).unwrap();
        assert_eq!(digest, task_digest(&task));
        assert_eq!(store.load_task("t-locked", None).unwrap().unwrap(), task);
        assert!(
            holder.lock().unwrap().is_none(),
            "the backoff released the blocker"
        );
    }

    #[test]
    fn locked_past_the_budget_surfaces_the_locked_error_not_a_fake_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked-classified.db");
        let waits = Arc::new(Mutex::new(Vec::new()));
        let record = {
            let waits = Arc::clone(&waits);
            Arc::new(move |delay: Duration| waits.lock().unwrap().push(delay)) as SleepFn
        };
        let store = SqliteStore::open_with(
            &path,
            SqliteStoreOptions {
                max_transient_retries: 2,
                transient_sleep: Some(record),
                ..fast_backoff()
            },
        )
        .unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let err = store
            .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
            .unwrap_err();
        assert!(is_locked(&err), "{err}");
        assert!(err.to_string().to_lowercase().contains("locked"));
        assert_eq!(waits.lock().unwrap().len(), 2);
        // WAL lets this read see the last committed snapshot: nothing leaked.
        assert!(store.load_lifecycle("r1").unwrap().is_none());
        blocker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn backoff_schedule_is_capped_exponential() {
        let options = SqliteStoreOptions::default();
        assert_eq!(options.delay_for(0), Duration::from_millis(500));
        assert_eq!(options.delay_for(1), Duration::from_secs(1));
        assert_eq!(options.delay_for(3), Duration::from_secs(4));
        assert_eq!(options.delay_for(30), DEFAULT_BACKOFF_CAP);
        assert!(format!("{options:?}").contains("busy_timeout"));
    }

    #[test]
    fn backoff_past_i32_exponent_range_stays_at_the_cap() {
        let options = SqliteStoreOptions::default();
        let past_i32 = u32::try_from(i32::MAX).unwrap() + 1;
        assert_eq!(options.delay_for(past_i32), DEFAULT_BACKOFF_CAP);
        assert_eq!(options.delay_for(u32::MAX), DEFAULT_BACKOFF_CAP);
    }

    #[test]
    fn is_usable_as_shared_harness_store_across_threads() {
        let shared: Arc<dyn HarnessStore> = Arc::new(SqliteStore::in_memory().unwrap());
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    shared
                        .create_lifecycle(&Lifecycle::new("t").with_run_id(format!("r{i}")))
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            shared
                .list_lifecycles(&LifecycleFilter::default())
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn list_breaks_updated_at_ties_by_descending_run_id() {
        let store = SqliteStore::in_memory().unwrap();
        for run_id in ["r-a", "r-c", "r-b"] {
            store
                .create_lifecycle(&Lifecycle::new("t").with_run_id(run_id))
                .unwrap();
        }
        // Writes inside one microsecond share an updated_at; the documented
        // order then falls back to the greater run_id first.
        store.with_connection(|conn| {
            conn.execute(
                "UPDATE lifecycles SET updated_at = '2026-05-28T12:00:00.000000+00:00'",
                [],
            )
            .unwrap();
        });
        let listed: Vec<String> = store
            .list_lifecycles(&LifecycleFilter::default())
            .unwrap()
            .into_iter()
            .map(|lc| lc.run_id)
            .collect();
        assert_eq!(listed, vec!["r-c", "r-b", "r-a"]);
    }

    #[test]
    fn resave_bumps_task_updated_at_and_keeps_first_seen_at() {
        let store = SqliteStore::in_memory().unwrap();
        let task = Task::new(
            "Goal.",
            vec![crate::task::Grader::Command(
                crate::task::CommandGrader::new("true").unwrap(),
            )],
        )
        .with_id("t");
        store.save_task(&task, at(0)).unwrap();
        store.save_task(&task, at(30)).unwrap();
        let (first_seen, updated): (String, String) = store.with_connection(|conn| {
            conn.query_row(
                "SELECT first_seen_at, updated_at FROM tasks WHERE id = 't'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        });
        assert_eq!(first_seen, format_timestamp(&at(0)));
        assert_eq!(updated, format_timestamp(&at(30)));
    }
}
