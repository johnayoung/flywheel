//! Postgres implementation of every store trait in [`crate::store`].
//!
//! The durable, network-addressable backend. Bootstraps the schema by
//! executing the canonical `persistence-schema-postgres.sql` verbatim
//! (embedded at compile time from its canonical path, so the DDL is never
//! re-derived here). Schemas written by the Python `PostgresStore` open
//! unchanged, and every row this store writes reads back through the Python
//! store: `*_json` columns are JSONB, timestamps are `TIMESTAMPTZ`, `status`
//! / `outcome` / `grader_type` are their snake_case wire strings, and
//! identity-style empty strings are persisted as `NULL`.
//!
//! Schema isolation lets multiple flywheel deployments share one database
//! under separate Postgres schemas. The schema name is validated against an
//! identifier-safe pattern ([`validate_schema`]) and pinned on the session
//! via `SET search_path TO <schema>, public`, so the unqualified DDL creates
//! every table under the requested schema without splicing caller input into
//! SQL.
//!
//! Bootstrap takes a transaction-scoped advisory lock
//! ([`BOOTSTRAP_LOCK_KEY`]) as its first statement so concurrent first opens
//! of one fresh schema serialize instead of deadlocking on the shared
//! catalogs; every later opener then sees a provisioned schema and the
//! `IF NOT EXISTS` script is a clean no-op. The one supported forward
//! migration (v11 -> v12: the nullable `lifecycles.source` column) is applied
//! in place; any other `schema_version` mismatch is refused with
//! [`StoreSchemaError`].
//!
//! Optimistic concurrency on `lifecycles.version` is a single
//! `UPDATE ... WHERE run_id = $1 AND version = $2` checked by rows affected;
//! a zero-row result is disambiguated into
//! [`StoreConflictError::LifecycleNotFound`] or
//! [`StoreConflictError::OptimisticConcurrency`] by a follow-up read. The
//! event append path locks the lifecycle row with `SELECT ... FOR UPDATE` so
//! the version check, projection writes, `MAX(sequence) + 1` allocation and
//! event-row insert are one serialized transaction per run.
//!
//! `grader_results` is append-only: the trait surface exposes no update or
//! delete verb, and the schema's `BEFORE UPDATE` / `BEFORE DELETE` triggers
//! reject any raw mutation with SQLSTATE `check_violation`.
//!
//! Cross-process reactivity: every committed append issues `pg_notify` on a
//! schema-derived channel ([`derive_channel`]) with payload
//! `"<run_id>:<sequence>"`, so a Python or Rust listener on any host is woken.
//! Opting into `listen` starts a dedicated `LISTEN` connection that bridges
//! other instances' committed writes into this store's [`RunNotifier`]; the
//! bridge is best-effort and a consumer degrades to its bounded poll when it
//! cannot connect.
//!
//! Thread-safety: the single connection lives behind a `Mutex`, so the store
//! is `Send + Sync` and shareable as `Arc<dyn HarnessStore>`; every public
//! verb is one lock acquisition.
//!
//! [`StoreConflictError::LifecycleNotFound`]: crate::store::StoreConflictError::LifecycleNotFound
//! [`StoreConflictError::OptimisticConcurrency`]: crate::store::StoreConflictError::OptimisticConcurrency

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use postgres::error::SqlState;
use postgres::fallible_iterator::FallibleIterator;
use postgres::types::{FromSql, ToSql};
use postgres::{Client, GenericClient, Row};
use serde_json::{Map, Value};

use crate::event_serde::{
    EventSerdeError, event_kind, event_payload, format_timestamp, parse_timestamp,
    payload_from_value,
};
use crate::events::{DomainEvent, DomainEventPayload, apply};
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

/// The canonical Postgres DDL, embedded verbatim from its single source of
/// truth.
pub const SCHEMA_SQL: &str = include_str!("../schema/persistence-schema-postgres.sql");

/// Indexes for the store's own access patterns that the canonical DDL does
/// not declare. Postgres does not index foreign-key columns automatically, so
/// `list_lifecycles(task_id = ...)` and the `(updated_at DESC, run_id DESC)`
/// ordering every listing uses would otherwise sequential-scan `lifecycles`.
/// Purely additive and idempotent; a schema bootstrapped by the Python store
/// gains them on first open here and loses nothing.
const SUPPLEMENTARY_INDEXES: &str = "
CREATE INDEX IF NOT EXISTS idx_lifecycles_task_id ON lifecycles(task_id);
CREATE INDEX IF NOT EXISTS idx_lifecycles_updated_at ON lifecycles(updated_at DESC, run_id DESC);
";

/// Serializes concurrent first-open bootstraps against a fresh schema.
///
/// Two openers racing the `CREATE SCHEMA` / `CREATE TABLE IF NOT EXISTS`
/// sequence otherwise deadlock on the shared system catalogs (bumping the
/// server's deadlock counter) or trip a `pg_namespace` unique violation.
/// Taken as the first statement of the bootstrap transaction and released at
/// its commit. The same key value the Python store and the orchestrator's
/// claim store take, so every store initializing one schema concurrently
/// serializes too. `0x666C7962` == `"flyb"`.
pub const BOOTSTRAP_LOCK_KEY: i64 = 0x666C_7962;

/// Postgres channel-name limit (`NAMEDATALEN - 1`). `pg_notify` rejects a
/// longer channel and `LISTEN` silently truncates it, so a schema whose
/// derived channel would exceed this is refused rather than risking a
/// mismatch.
pub const MAX_CHANNEL_BYTES: usize = 63;

/// The schema used when none is configured.
pub const DEFAULT_SCHEMA: &str = "public";

/// Listener cadence: how long each notification drain blocks before
/// re-checking the stop flag.
pub const LISTEN_POLL: Duration = Duration::from_secs(1);

/// Back-off after a transient listener error before the next drain.
const LISTEN_RETRY_DELAY: Duration = Duration::from_millis(500);

// --- Argument validation ----------------------------------------------------------------

/// A schema name that is not identifier-safe.
///
/// Raised before any SQL runs so caller input can never be concatenated into
/// a statement.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid schema name '{0}': must match [A-Za-z_][A-Za-z0-9_]*")]
pub struct InvalidSchemaName(pub String);

/// Validate a Postgres schema name: an ASCII letter or underscore, then
/// letters, digits, or underscores. Anything else is rejected.
pub fn validate_schema(schema: &str) -> Result<&str, InvalidSchemaName> {
    let mut chars = schema.chars();
    let valid_start = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid_start && valid_rest {
        Ok(schema)
    } else {
        Err(InvalidSchemaName(schema.to_owned()))
    }
}

/// The schema's derived NOTIFY channel would exceed [`MAX_CHANNEL_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "schema '{schema}' is too long: the derived NOTIFY channel '{channel}' exceeds {MAX_CHANNEL_BYTES} bytes"
)]
pub struct ChannelTooLong {
    pub schema: String,
    pub channel: String,
}

/// The schema-qualified NOTIFY channel (`flywheel_run_<schema>`), so
/// deployments sharing a database under different schemas do not cross-talk.
pub fn derive_channel(schema: &str) -> Result<String, ChannelTooLong> {
    let channel = format!("flywheel_run_{schema}");
    if channel.len() > MAX_CHANNEL_BYTES {
        return Err(ChannelTooLong {
            schema: schema.to_owned(),
            channel,
        });
    }
    Ok(channel)
}

// --- Error helpers ----------------------------------------------------------------------

/// A driver failure, carrying a human-readable message up front.
///
/// `postgres::Error`'s own `Display` is a fixed category ("db error", "error
/// connecting to server"); the server's message and the OS-level cause live
/// in its source chain. Operators need those, so this wrapper renders the
/// chain into `message` while keeping the original error as the source for
/// callers that want to inspect it.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PostgresError {
    message: String,
    #[source]
    source: postgres::Error,
}

impl PostgresError {
    pub fn new(source: postgres::Error) -> Self {
        Self {
            message: describe_error(&source),
            source,
        }
    }

    /// The SQLSTATE, when the failure came from the server.
    pub fn code(&self) -> Option<&SqlState> {
        self.source.code()
    }

    pub fn inner(&self) -> &postgres::Error {
        &self.source
    }
}

/// Render a driver error as one line: the server's message (and SQLSTATE)
/// for a database error, otherwise the category plus its full cause chain.
pub fn describe_error(err: &postgres::Error) -> String {
    if let Some(db) = err.as_db_error() {
        return format!("{} (SQLSTATE {})", db.message(), db.code().code());
    }
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn db(err: postgres::Error) -> StoreError {
    StoreError::backend(PostgresError::new(err))
}

fn is_unique_violation(err: &postgres::Error) -> bool {
    err.code() == Some(&SqlState::UNIQUE_VIOLATION)
}

/// A value that does not fit the column's integer width.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("value {value} does not fit the {column} {width} column")]
pub struct IntegerOutOfRange {
    pub column: &'static str,
    pub width: &'static str,
    pub value: i128,
}

fn int4<T: Copy + Into<i128> + TryInto<i32>>(
    value: T,
    column: &'static str,
) -> Result<i32, StoreError> {
    value.try_into().map_err(|_| {
        StoreError::backend(IntegerOutOfRange {
            column,
            width: "INTEGER",
            value: value.into(),
        })
    })
}

fn int8<T: Copy + Into<i128> + TryInto<i64>>(
    value: T,
    column: &'static str,
) -> Result<i64, StoreError> {
    value.try_into().map_err(|_| {
        StoreError::backend(IntegerOutOfRange {
            column,
            width: "BIGINT",
            value: value.into(),
        })
    })
}

fn opt_int4(value: Option<u32>, column: &'static str) -> Result<Option<i32>, StoreError> {
    value.map(|v| int4(v, column)).transpose()
}

fn unsigned<T: Copy + Into<i128>, U: TryFrom<T>>(
    value: T,
    column: &'static str,
) -> Result<U, StoreError> {
    U::try_from(value).map_err(|_| {
        StoreError::backend(IntegerOutOfRange {
            column,
            width: "unsigned",
            value: value.into(),
        })
    })
}

fn col<'a, T: FromSql<'a>>(row: &'a Row, idx: usize) -> Result<T, StoreError> {
    row.try_get(idx).map_err(db)
}

fn ts(text: &str) -> Result<DateTime<Utc>, StoreError> {
    parse_timestamp(text).map_err(|e| StoreError::EventSerde(EventSerdeError::Timestamp(e)))
}

/// NULL for an empty string, matching the Python store's `value or None`.
fn null_if_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn json_object(value: Value) -> Result<JsonObject, StoreError> {
    match value {
        Value::Object(map) => Ok(map),
        other => Err(StoreError::Json(
            <serde_json::Error as serde::de::Error>::custom(format!(
                "expected a JSON object column, got {other}"
            )),
        )),
    }
}

// --- Options / construction -------------------------------------------------------------

/// Construction options for [`PostgresStore`].
#[derive(Clone)]
pub struct PostgresStoreOptions {
    /// Postgres schema every table, index, and trigger lives under.
    pub schema: String,
    /// Start the cross-process `LISTEN` bridge.
    pub listen: bool,
    /// `None` gives the store its own [`RunNotifier`].
    pub notifier: Option<Arc<RunNotifier>>,
}

impl Default for PostgresStoreOptions {
    fn default() -> Self {
        Self {
            schema: DEFAULT_SCHEMA.to_owned(),
            listen: false,
            notifier: None,
        }
    }
}

impl fmt::Debug for PostgresStoreOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresStoreOptions")
            .field("schema", &self.schema)
            .field("listen", &self.listen)
            .field("notifier", &self.notifier.is_some())
            .finish()
    }
}

/// Where the `LISTEN` bridge stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerState {
    /// The bridge thread has not yet issued `LISTEN`.
    Starting,
    /// `LISTEN` is in effect: a NOTIFY issued from now on is observed.
    Listening,
    /// The bridge could not connect or subscribe; consumers fall back to
    /// their bounded poll. Carries the sanitized driver message.
    Failed(String),
}

struct Listener {
    stop: Arc<AtomicBool>,
    state: Arc<(Mutex<ListenerState>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl Listener {
    fn set_state(state: &(Mutex<ListenerState>, Condvar), value: ListenerState) {
        let (lock, condvar) = state;
        *lock.lock().unwrap_or_else(|p| p.into_inner()) = value;
        condvar.notify_all();
    }

    fn wait(&self, timeout: Duration) -> ListenerState {
        let (lock, condvar) = &*self.state;
        let deadline = Instant::now() + timeout;
        let mut guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        while *guard == ListenerState::Starting {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            guard = condvar
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        guard.clone()
    }

    fn is_alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            // The loop re-checks the stop flag every LISTEN_POLL, so the
            // join is bounded; a panicked bridge thread has nothing to add.
            let _ = handle.join();
        }
    }
}

/// Postgres implementation of every store trait.
///
/// Each instance owns one connection in autocommit mode (each statement is
/// its own transaction unless a verb opens an explicit transaction).
pub struct PostgresStore {
    client: Mutex<Client>,
    /// Kept to heal a closed connection; never logged or echoed.
    dsn: String,
    schema: String,
    channel: String,
    notifier: Arc<RunNotifier>,
    listener: Option<Listener>,
}

impl fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresStore")
            .field("schema", &self.schema)
            .field("channel", &self.channel)
            .field("listening", &self.listener.is_some())
            .finish_non_exhaustive()
    }
}

impl PostgresStore {
    /// Connect to `dsn` (a libpq-style URL or `key=value` string), pin
    /// `schema`, and bootstrap it. The DSN is never logged or echoed.
    ///
    /// Fails before any network traffic when `schema` is not identifier-safe
    /// or its derived NOTIFY channel is too long; connection and
    /// authentication failures surface unchanged as
    /// [`StoreError::Backend`] wrapping a [`PostgresError`].
    pub fn connect(dsn: &str, schema: &str) -> Result<Self, StoreError> {
        Self::connect_with(
            dsn,
            PostgresStoreOptions {
                schema: schema.to_owned(),
                ..Default::default()
            },
        )
    }

    pub fn connect_with(dsn: &str, options: PostgresStoreOptions) -> Result<Self, StoreError> {
        let schema = validate_schema(&options.schema)
            .map_err(StoreError::backend)?
            .to_owned();
        let channel = derive_channel(&schema).map_err(StoreError::backend)?;
        let notifier = options
            .notifier
            .clone()
            .unwrap_or_else(|| Arc::new(RunNotifier::new()));
        let (config, tls) = crate::pg_tls::configure(dsn).map_err(StoreError::backend)?;
        let mut client = config.connect(tls).map_err(db)?;
        // Pin the search_path for the session so unqualified DDL/DML
        // resolves under the store's schema without embedding the name in
        // every statement (the Python store's per-connection configure).
        client
            .batch_execute(&search_path_sql(&schema))
            .map_err(db)?;
        bootstrap(&mut client, &schema)?;
        let listener = options
            .listen
            .then(|| start_listener(dsn, &channel, &notifier));
        Ok(Self {
            client: Mutex::new(client),
            dsn: dsn.to_owned(),
            schema,
            channel,
            notifier,
            listener,
        })
    }

    /// The schema every table lives under.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// The NOTIFY channel this store signals on (and listens to).
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// The notifier every `append_domain_event` signals after committing.
    pub fn notifier(&self) -> &Arc<RunNotifier> {
        &self.notifier
    }

    /// Block until the `LISTEN` bridge is in effect (or has failed), at most
    /// `timeout`. `None` when the store was opened without `listen`.
    pub fn wait_listener(&self, timeout: Duration) -> Option<ListenerState> {
        self.listener.as_ref().map(|l| l.wait(timeout))
    }

    /// Whether the bridge thread is still running.
    pub fn listener_alive(&self) -> bool {
        self.listener.as_ref().is_some_and(Listener::is_alive)
    }

    /// Run `f` against the underlying connection under the store's lock.
    ///
    /// The escape hatch for consumers that share the database (the
    /// orchestrator's own tables) and for inspection; the store's own
    /// invariants are not re-checked on whatever `f` does.
    pub fn with_client<T>(&self, f: impl FnOnce(&mut Client) -> T) -> T {
        let mut client = self.raw_lock();
        // Best-effort heal; a still-dead connection surfaces through `f`.
        let _ = crate::pg_tls::ensure_open(&mut client, &self.dsn, &search_path_sql(&self.schema));
        f(&mut client)
    }

    /// Stop the listener (if any) and close the connection.
    pub fn close(mut self) -> Result<(), StoreError> {
        self.listener.take();
        let client = self
            .client
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        client.close().map_err(db)
    }

    fn raw_lock(&self) -> MutexGuard<'_, Client> {
        self.client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The connection under the store's lock, reconnected first when the
    /// previous one was observed closed.
    fn lock(&self) -> Result<MutexGuard<'_, Client>, StoreError> {
        let mut client = self.raw_lock();
        crate::pg_tls::ensure_open(&mut client, &self.dsn, &search_path_sql(&self.schema))
            .map_err(StoreError::backend)?;
        Ok(client)
    }
}

fn search_path_sql(schema: &str) -> String {
    format!("SET search_path TO \"{schema}\", public")
}

// --- Bootstrap ----------------------------------------------------------------------------

fn bootstrap(client: &mut Client, schema: &str) -> Result<(), StoreError> {
    let mut tx = client.transaction().map_err(db)?;
    // Serialize the whole bootstrap against every other opener so the
    // CREATE SCHEMA / CREATE TABLE IF NOT EXISTS sequence cannot race on a
    // fresh schema. Transaction-scoped: released at the commit below.
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&BOOTSTRAP_LOCK_KEY])
        .map_err(db)?;
    tx.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{schema}\";\n{}",
        search_path_sql(schema)
    ))
    .map_err(db)?;
    tx.batch_execute(SCHEMA_SQL).map_err(db)?;
    // Additive, version-neutral convergence: a task_versions table created
    // before insertion_seq existed gains it here. Guarded by a catalog read
    // rather than ADD COLUMN IF NOT EXISTS, which takes an ACCESS EXCLUSIVE
    // lock on the table even when it then no-ops.
    let has_insertion_seq: bool = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1::text AND table_name = 'task_versions' \
             AND column_name = 'insertion_seq')",
            &[&schema],
        )
        .map_err(db)?
        .get(0);
    if !has_insertion_seq {
        tx.batch_execute(
            "ALTER TABLE task_versions ADD COLUMN insertion_seq BIGINT GENERATED ALWAYS AS IDENTITY",
        )
        .map_err(db)?;
    }
    tx.batch_execute(SUPPLEMENTARY_INDEXES).map_err(db)?;
    // Version pin: the one supported forward migration (v11 -> v12: add the
    // nullable lifecycles.source column) is applied in place first; it is
    // purely additive, so existing history survives. Any other mismatch is
    // refused rather than upgraded.
    let observed: Option<i32> = tx
        .query_opt("SELECT version FROM schema_version WHERE id = 1", &[])
        .map_err(db)?
        .map(|row| row.get(0));
    let mut observed = observed.and_then(|v| u32::try_from(v).ok());
    if observed == Some(11) {
        tx.batch_execute(
            "ALTER TABLE lifecycles ADD COLUMN IF NOT EXISTS source TEXT;
             UPDATE schema_version SET version = 12 WHERE id = 1;",
        )
        .map_err(db)?;
        observed = Some(12);
    }
    tx.commit().map_err(db)?;
    if observed != Some(CURRENT_SCHEMA_VERSION) {
        return Err(StoreSchemaError::new(observed).into());
    }
    Ok(())
}

// --- LISTEN bridge --------------------------------------------------------------------------

fn start_listener(dsn: &str, channel: &str, notifier: &Arc<RunNotifier>) -> Listener {
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new((Mutex::new(ListenerState::Starting), Condvar::new()));
    let thread_stop = Arc::clone(&stop);
    let thread_state = Arc::clone(&state);
    let dsn = dsn.to_owned();
    let channel = channel.to_owned();
    let notifier = Arc::clone(notifier);
    let handle = std::thread::Builder::new()
        .name(format!("flywheel-pg-listener-{channel}"))
        .spawn(move || run_listener(&dsn, &channel, &notifier, &thread_stop, &thread_state))
        .ok();
    if handle.is_none() {
        Listener::set_state(
            &state,
            ListenerState::Failed("could not spawn the listener thread".to_owned()),
        );
    }
    Listener {
        stop,
        state,
        handle,
    }
}

/// Bridge committed cross-instance writes into the in-process notifier via
/// a dedicated `LISTEN` connection.
///
/// Runs on its own autocommit connection (`LISTEN` must be outside a
/// transaction). Re-checks the stop flag every [`LISTEN_POLL`] between
/// notification drains so dropping the store cancels promptly. Best-effort:
/// a failure to establish the first subscription is recorded in the
/// listener state and the consumer degrades to its bounded poll, never
/// hangs. Once listening, a dropped connection is re-established (and
/// `LISTEN` re-issued) every [`LISTEN_RETRY_DELAY`] until it succeeds or
/// the store is dropped; notifications sent while disconnected are covered
/// by the consumers' bounded poll.
fn run_listener(
    dsn: &str,
    channel: &str,
    notifier: &RunNotifier,
    stop: &AtomicBool,
    state: &(Mutex<ListenerState>, Condvar),
) {
    let mut client = match subscribe(dsn, channel) {
        Ok(client) => client,
        Err(reason) => {
            Listener::set_state(state, ListenerState::Failed(reason));
            return;
        }
    };
    // Signal readiness only after LISTEN is in effect, so a producer that
    // coordinates on this never issues a NOTIFY the listener would miss.
    Listener::set_state(state, ListenerState::Listening);
    while !stop.load(Ordering::SeqCst) {
        if client.is_closed() {
            match subscribe(dsn, channel) {
                Ok(fresh) => client = fresh,
                Err(_) => {
                    std::thread::sleep(LISTEN_RETRY_DELAY);
                    continue;
                }
            }
        }
        let mut notifications = client.notifications();
        let mut drain = notifications.timeout_iter(LISTEN_POLL);
        loop {
            match drain.next() {
                Ok(Some(note)) => {
                    handle_notify(notifier, note.payload());
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(LISTEN_RETRY_DELAY);
                    break;
                }
            }
        }
    }
}

/// One connection with `LISTEN` on `channel` in effect, or the sanitized
/// reason it could not be established.
fn subscribe(dsn: &str, channel: &str) -> Result<Client, String> {
    let mut client = crate::pg_tls::connect(dsn).map_err(|err| match err {
        crate::pg_tls::ConnectError::Postgres(err) => describe_error(&err),
        crate::pg_tls::ConnectError::Tls(err) => err.to_string(),
    })?;
    client
        .batch_execute(&format!("LISTEN \"{channel}\""))
        .map_err(|err| describe_error(&err))?;
    Ok(client)
}

/// Translate a `"<run_id>:<sequence>"` payload into a notifier signal; a
/// malformed payload is ignored.
fn handle_notify(notifier: &RunNotifier, payload: &str) {
    let Some((run_id, sequence)) = payload.rsplit_once(':') else {
        return;
    };
    if let Ok(sequence) = sequence.parse::<u64>() {
        notifier.notify(run_id, sequence);
    }
}

// --- lifecycles ---------------------------------------------------------------------------

const LIFECYCLE_COLUMNS: &str = "l.run_id, l.task_id, l.status, l.version, l.retries, l.error, \
     l.agent_output, l.session_id, l.artifacts_dir, l.worker_id, l.timestamps_json, \
     l.blocked_requires_json, l.task_content_hash, l.awaiting_manual_ordinal, l.source";
const LIFECYCLE_COLUMN_COUNT: usize = 15;

const ATTEMPT_COLUMNS: &str = "a.number, a.attempt_run_id, a.started_at, a.ended_at, a.outcome, \
     a.agent_output, a.error, a.agent_context_json, a.input_tokens, a.output_tokens, \
     a.cache_creation_input_tokens, a.cache_read_input_tokens, a.iterations_completed, \
     a.turns, a.total_cost_usd, a.last_activity_at";

/// The `lifecycles` row as persisted, before any decoding.
struct RawLifecycle {
    run_id: String,
    task_id: String,
    status: String,
    version: i32,
    retries: i32,
    error: Option<String>,
    agent_output: Option<String>,
    session_id: Option<String>,
    artifacts_dir: Option<String>,
    worker_id: Option<String>,
    timestamps_json: Value,
    blocked_requires_json: Option<String>,
    task_content_hash: Option<String>,
    awaiting_manual_ordinal: Option<i32>,
    source: Option<String>,
}

impl RawLifecycle {
    fn read(row: &Row) -> Result<Self, StoreError> {
        Ok(Self {
            run_id: col(row, 0)?,
            task_id: col(row, 1)?,
            status: col(row, 2)?,
            version: col(row, 3)?,
            retries: col(row, 4)?,
            error: col(row, 5)?,
            agent_output: col(row, 6)?,
            session_id: col(row, 7)?,
            artifacts_dir: col(row, 8)?,
            worker_id: col(row, 9)?,
            timestamps_json: col(row, 10)?,
            blocked_requires_json: col(row, 11)?,
            task_content_hash: col(row, 12)?,
            awaiting_manual_ordinal: col(row, 13)?,
            source: col(row, 14)?,
        })
    }

    /// The lifecycle row with `attempts` empty; the caller attaches them.
    fn decode(self) -> Result<Lifecycle, StoreError> {
        let mut lifecycle = Lifecycle::new(self.task_id).with_run_id(self.run_id);
        lifecycle.worker_id = self.worker_id.unwrap_or_default();
        lifecycle.status = parse_wire_enum(&self.status)?;
        lifecycle.timestamps = deserialize_timestamps(self.timestamps_json)?;
        lifecycle.version = unsigned(self.version, "lifecycles.version")?;
        lifecycle.retries = unsigned(self.retries, "lifecycles.retries")?;
        lifecycle.error = self.error.unwrap_or_default();
        lifecycle.agent_output = self.agent_output.unwrap_or_default();
        lifecycle.session_id = self.session_id.unwrap_or_default();
        lifecycle.artifacts_dir = self.artifacts_dir.unwrap_or_default();
        lifecycle.blocked_requires_json = self.blocked_requires_json;
        lifecycle.awaiting_manual_ordinal = self
            .awaiting_manual_ordinal
            .map(|v| unsigned(v, "lifecycles.awaiting_manual_ordinal"))
            .transpose()?;
        lifecycle.task_content_hash = self.task_content_hash.unwrap_or_default();
        lifecycle.source = self.source.unwrap_or_default();
        Ok(lifecycle)
    }
}

fn serialize_timestamps(timestamps: &BTreeMap<Status, DateTime<Utc>>) -> Value {
    let map: Map<String, Value> = timestamps
        .iter()
        .map(|(status, at)| {
            (
                status.as_str().to_owned(),
                Value::String(format_timestamp(at)),
            )
        })
        .collect();
    Value::Object(map)
}

fn deserialize_timestamps(value: Value) -> Result<BTreeMap<Status, DateTime<Utc>>, StoreError> {
    let mut out = BTreeMap::new();
    for (key, value) in json_object(value)? {
        let status: Status = parse_wire_enum(&key)?;
        let text: String = serde_json::from_value(value)?;
        out.insert(status, ts(&text)?);
    }
    Ok(out)
}

/// Create the logical task identity row if absent (idempotent). The FK
/// anchor for task_versions and lifecycles; `ON CONFLICT DO NOTHING`
/// preserves the original `first_seen_at`.
fn ensure_task_identity<C: GenericClient>(
    c: &mut C,
    task_id: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    c.execute(
        "INSERT INTO tasks (id, first_seen_at, updated_at) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO NOTHING",
        &[&task_id, &now, &now],
    )
    .map_err(db)?;
    Ok(())
}

/// The lifecycle's columns as bound parameters, in the shared order the
/// insert and update statements use.
struct LifecycleParams {
    status: &'static str,
    version: i32,
    retries: i32,
    timestamps_json: Value,
    updated_at: DateTime<Utc>,
    awaiting_manual_ordinal: Option<i32>,
}

impl LifecycleParams {
    fn new(lifecycle: &Lifecycle) -> Result<Self, StoreError> {
        Ok(Self {
            status: lifecycle.status.as_str(),
            version: int4(lifecycle.version, "lifecycles.version")?,
            retries: int4(lifecycle.retries, "lifecycles.retries")?,
            timestamps_json: serialize_timestamps(&lifecycle.timestamps),
            updated_at: Utc::now(),
            awaiting_manual_ordinal: opt_int4(
                lifecycle.awaiting_manual_ordinal,
                "lifecycles.awaiting_manual_ordinal",
            )?,
        })
    }
}

fn insert_lifecycle<C: GenericClient>(c: &mut C, lifecycle: &Lifecycle) -> Result<(), StoreError> {
    // Auto-register the task identity so lifecycles.task_id -> tasks(id)
    // always holds, covering callers that seed a lifecycle without a prior
    // save_task.
    let now = Utc::now();
    ensure_task_identity(c, &lifecycle.task_id, now)?;
    let p = LifecycleParams::new(lifecycle)?;
    let result = c.execute(
        "INSERT INTO lifecycles (
            run_id, task_id, status, version, retries, error,
            agent_output, session_id, artifacts_dir, worker_id,
            timestamps_json, updated_at, blocked_requires_json,
            task_content_hash, awaiting_manual_ordinal, source
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        &[
            &lifecycle.run_id,
            &lifecycle.task_id,
            &p.status,
            &p.version,
            &p.retries,
            &null_if_empty(&lifecycle.error),
            &null_if_empty(&lifecycle.agent_output),
            &null_if_empty(&lifecycle.session_id),
            &null_if_empty(&lifecycle.artifacts_dir),
            &null_if_empty(&lifecycle.worker_id),
            &p.timestamps_json,
            &p.updated_at,
            &lifecycle.blocked_requires_json,
            &null_if_empty(&lifecycle.task_content_hash),
            &p.awaiting_manual_ordinal,
            &null_if_empty(&lifecycle.source),
        ],
    );
    match result {
        Ok(_) => Ok(()),
        Err(err) if is_unique_violation(&err) => {
            Err(StoreError::lifecycle_already_exists(&lifecycle.run_id))
        }
        Err(err) => Err(db(err)),
    }
}

fn update_lifecycle_row<C: GenericClient>(
    c: &mut C,
    lifecycle: &Lifecycle,
    expected_version: u64,
) -> Result<(), StoreError> {
    let p = LifecycleParams::new(lifecycle)?;
    let expected = int4(expected_version, "lifecycles.version")?;
    let affected = c
        .execute(
            "UPDATE lifecycles SET
                task_id = $1, status = $2, version = $3, retries = $4, error = $5,
                agent_output = $6, session_id = $7, artifacts_dir = $8, worker_id = $9,
                timestamps_json = $10, updated_at = $11, blocked_requires_json = $12,
                task_content_hash = $13, awaiting_manual_ordinal = $14, source = $15
            WHERE run_id = $16 AND version = $17",
            &[
                &lifecycle.task_id,
                &p.status,
                &p.version,
                &p.retries,
                &null_if_empty(&lifecycle.error),
                &null_if_empty(&lifecycle.agent_output),
                &null_if_empty(&lifecycle.session_id),
                &null_if_empty(&lifecycle.artifacts_dir),
                &null_if_empty(&lifecycle.worker_id),
                &p.timestamps_json,
                &p.updated_at,
                &lifecycle.blocked_requires_json,
                &null_if_empty(&lifecycle.task_content_hash),
                &p.awaiting_manual_ordinal,
                &null_if_empty(&lifecycle.source),
                &lifecycle.run_id,
                &expected,
            ],
        )
        .map_err(db)?;
    if affected == 1 {
        return Ok(());
    }
    // Zero rows: disambiguate not-found from a version conflict.
    match stored_version(c, &lifecycle.run_id, false)? {
        None => Err(StoreError::lifecycle_not_found(&lifecycle.run_id)),
        Some(actual) => Err(StoreError::optimistic_concurrency(
            &lifecycle.run_id,
            expected_version,
            actual,
        )),
    }
}

/// The stored `lifecycles.version`, optionally locking the row for the
/// surrounding transaction.
fn stored_version<C: GenericClient>(
    c: &mut C,
    run_id: &str,
    for_update: bool,
) -> Result<Option<u64>, StoreError> {
    let sql = if for_update {
        "SELECT version FROM lifecycles WHERE run_id = $1 FOR UPDATE"
    } else {
        "SELECT version FROM lifecycles WHERE run_id = $1"
    };
    let row = c.query_opt(sql, &[&run_id]).map_err(db)?;
    row.map(|row| {
        let version: i32 = col(&row, 0)?;
        unsigned(version, "lifecycles.version")
    })
    .transpose()
}

/// Decode `LIFECYCLE_COLUMNS` + `ATTEMPT_COLUMNS` rows from a
/// `lifecycles LEFT JOIN attempts` query into folded lifecycles, in row
/// order. Rows for one run are consecutive (the queries order by run first,
/// then attempt number); a run with no attempts yields one row whose attempt
/// columns are all NULL.
fn fold_joined_rows(rows: &[Row]) -> Result<Vec<Lifecycle>, StoreError> {
    let mut lifecycles: Vec<Lifecycle> = Vec::new();
    for row in rows {
        let run_id: String = col(row, 0)?;
        if lifecycles.last().is_none_or(|lc| lc.run_id != run_id) {
            lifecycles.push(RawLifecycle::read(row)?.decode()?);
        }
        if let Some(raw) = RawAttempt::read_offset(row, LIFECYCLE_COLUMN_COUNT)? {
            let attempt = raw.decode()?;
            if let Some(current) = lifecycles.last_mut() {
                current.attempts.push(attempt);
            }
        }
    }
    Ok(lifecycles)
}

/// One round trip: the lifecycle row and every attempt of the run, read
/// under a single statement snapshot. `for_update` locks the lifecycle row
/// (`FOR UPDATE OF l`) for the surrounding append transaction.
fn load_lifecycle_row<C: GenericClient>(
    c: &mut C,
    run_id: &str,
    for_update: bool,
) -> Result<Option<Lifecycle>, StoreError> {
    let lock = if for_update { " FOR UPDATE OF l" } else { "" };
    let rows = c
        .query(
            &format!(
                "SELECT {LIFECYCLE_COLUMNS}, {ATTEMPT_COLUMNS} FROM lifecycles l \
                 LEFT JOIN attempts a ON a.run_id = l.run_id \
                 WHERE l.run_id = $1 ORDER BY a.number{lock}"
            ),
            &[&run_id],
        )
        .map_err(db)?;
    Ok(fold_joined_rows(&rows)?.into_iter().next())
}

/// The SQL filter for `list_lifecycles`; `None` when the filter can match
/// nothing (an empty status set).
fn filter_clause(filter: &LifecycleFilter) -> Option<(String, Vec<Box<dyn ToSql + Sync>>)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn ToSql + Sync>> = Vec::new();
    if let Some(statuses) = &filter.statuses {
        if statuses.is_empty() {
            return None;
        }
        let values: Vec<String> = statuses.iter().map(|s| s.as_str().to_owned()).collect();
        params.push(Box::new(values));
        clauses.push(format!("l.status = ANY(${})", params.len()));
    }
    if let Some(task_id) = &filter.task_id {
        params.push(Box::new(task_id.clone()));
        clauses.push(format!("l.task_id = ${}", params.len()));
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    Some((where_clause, params))
}

fn list_lifecycle_rows<C: GenericClient>(
    c: &mut C,
    filter: &LifecycleFilter,
) -> Result<Vec<Lifecycle>, StoreError> {
    let Some((where_clause, params)) = filter_clause(filter) else {
        return Ok(Vec::new());
    };
    let param_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p.as_ref()).collect();
    // One query regardless of row count: every matched lifecycle with its
    // attempts joined in, grouped in memory. No per-row follow-up reads.
    let rows = c
        .query(
            &format!(
                "SELECT {LIFECYCLE_COLUMNS}, {ATTEMPT_COLUMNS} FROM lifecycles l \
                 LEFT JOIN attempts a ON a.run_id = l.run_id {where_clause} \
                 ORDER BY l.updated_at DESC, l.run_id DESC, a.number"
            ),
            param_refs.as_slice(),
        )
        .map_err(db)?;
    fold_joined_rows(&rows)
}

fn summarize_spend_rows<C: GenericClient>(
    c: &mut C,
    window: &SpendWindow,
) -> Result<SpendSummary, StoreError> {
    // SUM over every attempt's four token columns + cost. The window is
    // half-open [since, until) on last_activity_at, compared natively as
    // TIMESTAMPTZ; a bounded window also requires last_activity_at IS NOT
    // NULL so an attempt with an unknown activity instant counts toward the
    // unbounded grand total but never lands inside a bounded window. SUM of
    // BIGINT is NUMERIC in Postgres, hence the casts; COALESCE keeps an
    // empty result at all-zero totals rather than NULL.
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn ToSql + Sync>> = Vec::new();
    if let Some(since) = window.since {
        params.push(Box::new(since));
        clauses.push(format!(
            "last_activity_at IS NOT NULL AND last_activity_at >= ${}",
            params.len()
        ));
    }
    if let Some(until) = window.until {
        params.push(Box::new(until));
        clauses.push(format!(
            "last_activity_at IS NOT NULL AND last_activity_at < ${}",
            params.len()
        ));
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let param_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p.as_ref()).collect();
    let row = c
        .query_one(
            &format!(
                "SELECT COALESCE(SUM(input_tokens), 0)::bigint,
                        COALESCE(SUM(output_tokens), 0)::bigint,
                        COALESCE(SUM(cache_creation_input_tokens), 0)::bigint,
                        COALESCE(SUM(cache_read_input_tokens), 0)::bigint,
                        COALESCE(SUM(total_cost_usd), 0.0)::double precision
                 FROM attempts {where_clause}"
            ),
            param_refs.as_slice(),
        )
        .map_err(db)?;
    Ok(SpendSummary {
        input_tokens: unsigned(col::<i64>(&row, 0)?, "attempts.input_tokens")?,
        output_tokens: unsigned(col::<i64>(&row, 1)?, "attempts.output_tokens")?,
        cache_creation_tokens: unsigned(
            col::<i64>(&row, 2)?,
            "attempts.cache_creation_input_tokens",
        )?,
        cache_read_tokens: unsigned(col::<i64>(&row, 3)?, "attempts.cache_read_input_tokens")?,
        total_cost_usd: col(&row, 4)?,
    })
}

// --- attempts -------------------------------------------------------------------------------

struct RawAttempt {
    number: i32,
    attempt_run_id: Option<String>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    outcome: Option<String>,
    agent_output: Option<String>,
    error: Option<String>,
    agent_context_json: Option<Value>,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
    iterations_completed: i32,
    turns: i32,
    total_cost_usd: f64,
    last_activity_at: Option<DateTime<Utc>>,
}

impl RawAttempt {
    fn read(row: &Row) -> Result<Self, StoreError> {
        Self::read_offset(row, 0)?.ok_or_else(|| {
            StoreError::backend(IntegerOutOfRange {
                column: "attempts.number",
                width: "INTEGER",
                value: 0,
            })
        })
    }

    /// `None` when the attempt columns are the NULL side of an outer join.
    fn read_offset(row: &Row, at: usize) -> Result<Option<Self>, StoreError> {
        let Some(number) = col::<Option<i32>>(row, at)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            number,
            attempt_run_id: col(row, at + 1)?,
            started_at: col(row, at + 2)?,
            ended_at: col(row, at + 3)?,
            outcome: col(row, at + 4)?,
            agent_output: col(row, at + 5)?,
            error: col(row, at + 6)?,
            agent_context_json: col(row, at + 7)?,
            input_tokens: col(row, at + 8)?,
            output_tokens: col(row, at + 9)?,
            cache_creation_input_tokens: col(row, at + 10)?,
            cache_read_input_tokens: col(row, at + 11)?,
            iterations_completed: col(row, at + 12)?,
            turns: col(row, at + 13)?,
            total_cost_usd: col(row, at + 14)?,
            last_activity_at: col(row, at + 15)?,
        }))
    }

    fn decode(self) -> Result<Attempt, StoreError> {
        let mut attempt = Attempt::new(
            unsigned(self.number, "attempts.number")?,
            self.started_at,
            self.attempt_run_id.unwrap_or_default(),
        );
        attempt.ended_at = self.ended_at;
        attempt.outcome = self
            .outcome
            .as_deref()
            .filter(|o| !o.is_empty())
            .map(parse_wire_enum::<Outcome>)
            .transpose()?;
        attempt.agent_output = self.agent_output.unwrap_or_default();
        attempt.error = self.error.unwrap_or_default();
        attempt.agent_context = match self.agent_context_json {
            Some(value) if !value.is_null() => serde_json::from_value(value)?,
            _ => BTreeMap::new(),
        };
        attempt.input_tokens = unsigned(self.input_tokens, "attempts.input_tokens")?;
        attempt.output_tokens = unsigned(self.output_tokens, "attempts.output_tokens")?;
        attempt.cache_creation_input_tokens = unsigned(
            self.cache_creation_input_tokens,
            "attempts.cache_creation_input_tokens",
        )?;
        attempt.cache_read_input_tokens = unsigned(
            self.cache_read_input_tokens,
            "attempts.cache_read_input_tokens",
        )?;
        attempt.iterations_completed =
            unsigned(self.iterations_completed, "attempts.iterations_completed")?;
        attempt.turns = unsigned(self.turns, "attempts.turns")?;
        attempt.total_cost_usd = self.total_cost_usd;
        attempt.last_activity_at = self.last_activity_at;
        Ok(attempt)
    }
}

fn upsert_attempt<C: GenericClient>(
    c: &mut C,
    run_id: &str,
    attempt: &Attempt,
) -> Result<(), StoreError> {
    // A true upsert on the (run_id, number) primary key: the existing row is
    // updated in place rather than deleted and re-inserted, so dependent
    // grader_results rows and the id are untouched. The harness records
    // both the start and the finalization of an attempt through this verb.
    let number = int4(attempt.number, "attempts.number")?;
    let outcome = attempt.outcome.map(Outcome::as_str);
    let agent_context = serde_json::to_value(&attempt.agent_context)?;
    let input_tokens = int8(attempt.input_tokens, "attempts.input_tokens")?;
    let output_tokens = int8(attempt.output_tokens, "attempts.output_tokens")?;
    let cache_creation = int8(
        attempt.cache_creation_input_tokens,
        "attempts.cache_creation_input_tokens",
    )?;
    let cache_read = int8(
        attempt.cache_read_input_tokens,
        "attempts.cache_read_input_tokens",
    )?;
    let iterations = int4(
        attempt.iterations_completed,
        "attempts.iterations_completed",
    )?;
    let turns = int4(attempt.turns, "attempts.turns")?;
    c.execute(
        "INSERT INTO attempts (
            run_id, number, attempt_run_id, started_at, ended_at, outcome,
            agent_output, error, agent_context_json, input_tokens, output_tokens,
            cache_creation_input_tokens, cache_read_input_tokens,
            iterations_completed, turns, total_cost_usd, last_activity_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
        ON CONFLICT (run_id, number) DO UPDATE SET
            attempt_run_id = EXCLUDED.attempt_run_id,
            started_at = EXCLUDED.started_at,
            ended_at = EXCLUDED.ended_at,
            outcome = EXCLUDED.outcome,
            agent_output = EXCLUDED.agent_output,
            error = EXCLUDED.error,
            agent_context_json = EXCLUDED.agent_context_json,
            input_tokens = EXCLUDED.input_tokens,
            output_tokens = EXCLUDED.output_tokens,
            cache_creation_input_tokens = EXCLUDED.cache_creation_input_tokens,
            cache_read_input_tokens = EXCLUDED.cache_read_input_tokens,
            iterations_completed = EXCLUDED.iterations_completed,
            turns = EXCLUDED.turns,
            total_cost_usd = EXCLUDED.total_cost_usd,
            last_activity_at = EXCLUDED.last_activity_at",
        &[
            &run_id,
            &number,
            &null_if_empty(&attempt.run_id),
            &attempt.started_at,
            &attempt.ended_at,
            &outcome,
            &null_if_empty(&attempt.agent_output),
            &null_if_empty(&attempt.error),
            &agent_context,
            &input_tokens,
            &output_tokens,
            &cache_creation,
            &cache_read,
            &iterations,
            &turns,
            &attempt.total_cost_usd,
            &attempt.last_activity_at,
        ],
    )
    .map_err(db)?;
    Ok(())
}

fn list_attempt_rows<C: GenericClient>(
    c: &mut C,
    run_id: &str,
) -> Result<Vec<Attempt>, StoreError> {
    let rows = c
        .query(
            &format!(
                "SELECT {ATTEMPT_COLUMNS} FROM attempts a WHERE a.run_id = $1 ORDER BY a.number"
            ),
            &[&run_id],
        )
        .map_err(db)?;
    rows.iter()
        .map(|row| RawAttempt::read(row)?.decode())
        .collect()
}

fn load_attempt_row<C: GenericClient>(
    c: &mut C,
    run_id: &str,
    number: u32,
) -> Result<Option<Attempt>, StoreError> {
    let number = int4(number, "attempts.number")?;
    let row = c
        .query_opt(
            &format!(
                "SELECT {ATTEMPT_COLUMNS} FROM attempts a WHERE a.run_id = $1 AND a.number = $2"
            ),
            &[&run_id, &number],
        )
        .map_err(db)?;
    row.map(|row| RawAttempt::read(&row)?.decode()).transpose()
}

// --- events -----------------------------------------------------------------------------------

/// `MAX(sequence) + 1` over the run's rows. Always called inside the append
/// transaction, where the `FOR UPDATE` lock on the lifecycle row serializes
/// appenders, so per-run ordering stays strictly monotonic; the
/// `UNIQUE (run_id, sequence)` constraint backstops any writer that bypasses
/// the lock.
fn next_domain_sequence<C: GenericClient>(c: &mut C, run_id: &str) -> Result<i64, StoreError> {
    let row = c
        .query_one(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE run_id = $1",
            &[&run_id],
        )
        .map_err(db)?;
    col(&row, 0)
}

/// Append the event row and queue the cross-process NOTIFY on the same
/// transaction: Postgres holds the notification until commit, so a listener
/// never observes a watermark for an aborted write.
fn insert_domain_event<C: GenericClient>(
    c: &mut C,
    event: &DomainEvent,
    channel: &str,
) -> Result<u64, StoreError> {
    let sequence = next_domain_sequence(c, &event.run_id)?;
    let attempt_number = opt_int4(event.attempt_number, "events.attempt_number")?;
    let kind = event_kind(event);
    let payload = Value::Object(event_payload(&event.payload)?);
    c.execute(
        "INSERT INTO events (run_id, attempt_number, ts, kind, payload_json, sequence)
         VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            &event.run_id,
            &attempt_number,
            &event.ts,
            &kind,
            &payload,
            &sequence,
        ],
    )
    .map_err(db)?;
    let notify_payload = format!("{}:{sequence}", event.run_id);
    c.execute("SELECT pg_notify($1, $2)", &[&channel, &notify_payload])
        .map_err(db)?;
    unsigned(sequence, "events.sequence")
}

fn list_domain_event_rows<C: GenericClient>(
    c: &mut C,
    run_id: &str,
) -> Result<Vec<DomainEvent>, StoreError> {
    let rows = c
        .query(
            "SELECT id, run_id, attempt_number, ts, kind, payload_json, sequence
             FROM events WHERE run_id = $1 ORDER BY sequence",
            &[&run_id],
        )
        .map_err(db)?;
    rows.iter()
        .map(|row| {
            let attempt_number: Option<i32> = col(row, 2)?;
            let kind: String = col(row, 4)?;
            let payload = json_object(col(row, 5)?)?;
            let sequence: i64 = col(row, 6)?;
            Ok(DomainEvent {
                id: Some(col(row, 0)?),
                run_id: col(row, 1)?,
                attempt_number: attempt_number
                    .map(|n| unsigned(n, "events.attempt_number"))
                    .transpose()?,
                ts: col(row, 3)?,
                sequence: Some(unsigned(sequence, "events.sequence")?),
                payload: payload_from_value(&kind, payload)?,
            })
        })
        .collect()
}

/// Maintain the attempts / grader_results read-model projections inside the
/// append transaction so they commit atomically with the event row and the
/// lifecycle row.
fn project_domain_event<C: GenericClient>(
    c: &mut C,
    event: &DomainEvent,
    folded: &Lifecycle,
) -> Result<(), StoreError> {
    match &event.payload {
        DomainEventPayload::AttemptStarted(a) => {
            upsert_attempt(c, &folded.run_id, projected_attempt(folded, a.number)?)
        }
        DomainEventPayload::AttemptFinalized(f) => {
            upsert_attempt(c, &folded.run_id, projected_attempt(folded, f.number)?)
        }
        DomainEventPayload::GraderEvaluated(g) => {
            insert_grader_result(c, &grader_result_from_event(event, g)?).map(|_| ())
        }
        _ => Ok(()),
    }
}

// --- grader_results -----------------------------------------------------------------------

fn insert_grader_result<C: GenericClient>(
    c: &mut C,
    result: &GraderResultRecord,
) -> Result<GraderResultRecord, StoreError> {
    let attempt_number = int4(result.attempt_number, "grader_results.attempt_number")?;
    let ordinal = int4(result.ordinal, "grader_results.ordinal")?;
    let duration_ms = int4(result.duration_ms, "grader_results.duration_ms")?;
    let grader_spec = Value::Object(result.grader_spec.clone());
    let payload = Value::Object(result.payload.clone());
    let row = c
        .query_one(
            "INSERT INTO grader_results (
                run_id, attempt_number, ordinal, grader_type, grader_name,
                grader_spec_json, passed, duration_ms, payload_json, ts
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            RETURNING id",
            &[
                &result.run_id,
                &attempt_number,
                &ordinal,
                &result.grader_type.as_str(),
                &result.grader_name,
                &grader_spec,
                &result.passed,
                &duration_ms,
                &payload,
                &result.ts,
            ],
        )
        .map_err(db)?;
    let mut stored = result.clone();
    stored.id = Some(col(&row, 0)?);
    Ok(stored)
}

fn list_grader_result_rows<C: GenericClient>(
    c: &mut C,
    run_id: &str,
    attempt_number: u32,
) -> Result<Vec<GraderResultRecord>, StoreError> {
    let attempt_number = int4(attempt_number, "grader_results.attempt_number")?;
    let rows = c
        .query(
            "SELECT id, run_id, attempt_number, ordinal, grader_type, grader_name,
                    grader_spec_json, passed, duration_ms, payload_json, ts
             FROM grader_results WHERE run_id = $1 AND attempt_number = $2
             ORDER BY ordinal",
            &[&run_id, &attempt_number],
        )
        .map_err(db)?;
    rows.iter()
        .map(|row| {
            let grader_type: String = col(row, 4)?;
            Ok(GraderResultRecord {
                run_id: col(row, 1)?,
                attempt_number: unsigned(col::<i32>(row, 2)?, "grader_results.attempt_number")?,
                ordinal: unsigned(col::<i32>(row, 3)?, "grader_results.ordinal")?,
                grader_type: parse_wire_enum::<GraderType>(&grader_type)?,
                grader_spec: json_object(col(row, 6)?)?,
                passed: col(row, 7)?,
                duration_ms: unsigned(col::<i32>(row, 8)?, "grader_results.duration_ms")?,
                payload: json_object(col(row, 9)?)?,
                ts: col(row, 10)?,
                grader_name: col(row, 5)?,
                id: Some(col(row, 0)?),
            })
        })
        .collect()
}

// --- control_commands ---------------------------------------------------------------------

fn control_command_from_row(row: &Row) -> Result<ControlCommandRecord, StoreError> {
    Ok(ControlCommandRecord {
        run_id: col(row, 1)?,
        kind: col(row, 2)?,
        payload: json_object(col(row, 3)?)?,
        enqueued_at: col(row, 4)?,
        claimed_at: col(row, 5)?,
        id: Some(col(row, 0)?),
    })
}

// --- Trait implementations ------------------------------------------------------------------

impl LifecycleStore for PostgresStore {
    fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
        let mut client = self.lock()?;
        let mut tx = client.transaction().map_err(db)?;
        insert_lifecycle(&mut tx, lifecycle)?;
        tx.commit().map_err(db)
    }

    fn update_lifecycle(
        &self,
        lifecycle: &Lifecycle,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        let mut client = self.lock()?;
        update_lifecycle_row(&mut *client, lifecycle, expected_version)
    }

    fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
        let mut client = self.lock()?;
        load_lifecycle_row(&mut *client, run_id, false)
    }

    fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
        let mut client = self.lock()?;
        list_lifecycle_rows(&mut *client, filter)
    }

    fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
        let mut client = self.lock()?;
        summarize_spend_rows(&mut *client, window)
    }
}

impl TaskStore for PostgresStore {
    fn save_task(&self, task: &Task, now: DateTime<Utc>) -> Result<String, StoreError> {
        let content_hash = task_digest(task);
        let definition = task_definition(task)?;
        let goal = definition
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let column = |key: &str| definition.get(key).cloned().unwrap_or(Value::Null);
        let (graders_json, tags_json, context_json) =
            (column("graders"), column("tags"), column("context"));
        let mut client = self.lock()?;
        // One transaction so the identity row and the version row land
        // together. The version row is immutable (ON CONFLICT DO NOTHING
        // keeps the original created_at); the whole definition is part of
        // the content hash.
        let mut tx = client.transaction().map_err(db)?;
        ensure_task_identity(&mut tx, &task.id, now)?;
        tx.execute(
            "UPDATE tasks SET updated_at = $1 WHERE id = $2",
            &[&now, &task.id],
        )
        .map_err(db)?;
        tx.execute(
            "INSERT INTO task_versions (
                task_id, content_hash, goal, graders_json, tags_json, context_json, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (task_id, content_hash) DO NOTHING",
            &[
                &task.id,
                &content_hash,
                &goal,
                &graders_json,
                &tags_json,
                &context_json,
                &now,
            ],
        )
        .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(content_hash)
    }

    fn load_task(
        &self,
        task_id: &str,
        content_hash: Option<&str>,
    ) -> Result<Option<Task>, StoreError> {
        let mut client = self.lock()?;
        load_task_row(&mut *client, task_id, content_hash)
    }

    fn load_task_for_run(&self, run_id: &str) -> Result<Option<Task>, StoreError> {
        let mut client = self.lock()?;
        let pinned = client
            .query_opt(
                "SELECT task_id, task_content_hash FROM lifecycles WHERE run_id = $1",
                &[&run_id],
            )
            .map_err(db)?;
        let Some(row) = pinned else {
            return Ok(None);
        };
        let task_id: String = col(&row, 0)?;
        let content_hash: Option<String> = col(&row, 1)?;
        let content_hash = content_hash.filter(|h| !h.is_empty());
        load_task_row(&mut *client, &task_id, content_hash.as_deref())
    }
}

fn load_task_row<C: GenericClient>(
    c: &mut C,
    task_id: &str,
    content_hash: Option<&str>,
) -> Result<Option<Task>, StoreError> {
    let row = match content_hash {
        Some(hash) => c.query_opt(
            "SELECT goal, graders_json, tags_json, context_json
             FROM task_versions WHERE task_id = $1 AND content_hash = $2",
            &[&task_id, &hash],
        ),
        None => c.query_opt(
            "SELECT goal, graders_json, tags_json, context_json
             FROM task_versions WHERE task_id = $1
             ORDER BY created_at DESC, insertion_seq DESC LIMIT 1",
            &[&task_id],
        ),
    }
    .map_err(db)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut definition = Map::new();
    definition.insert("goal".to_owned(), Value::String(col(&row, 0)?));
    definition.insert("graders".to_owned(), col(&row, 1)?);
    definition.insert("tags".to_owned(), col(&row, 2)?);
    definition.insert("context".to_owned(), col(&row, 3)?);
    assemble_task(task_id, &definition).map(Some)
}

impl DomainEventStore for PostgresStore {
    fn append_domain_event(
        &self,
        event: &DomainEvent,
        expected_version: u64,
    ) -> Result<Lifecycle, StoreError> {
        let (folded, sequence) = {
            let mut client = self.lock()?;
            // One transaction: the version check, projection writes and
            // event-row append are atomic, and SELECT ... FOR UPDATE on the
            // lifecycle row keeps a concurrent appender from interleaving.
            // A failure anywhere rolls the whole unit back when the
            // transaction drops.
            let mut tx = client.transaction().map_err(db)?;
            let folded = match &event.payload {
                DomainEventPayload::LifecycleInitialized(_) => {
                    let seeded = apply(None, event)?;
                    insert_lifecycle(&mut tx, &seeded)?;
                    seeded
                }
                _ => {
                    let current = load_lifecycle_row(&mut tx, &event.run_id, true)?
                        .ok_or_else(|| StoreError::lifecycle_not_found(&event.run_id))?;
                    if current.version != expected_version {
                        return Err(StoreError::optimistic_concurrency(
                            &event.run_id,
                            expected_version,
                            current.version,
                        ));
                    }
                    let folded = apply(Some(&current), event)?;
                    update_lifecycle_row(&mut tx, &folded, expected_version)?;
                    project_domain_event(&mut tx, event, &folded)?;
                    folded
                }
            };
            let sequence = insert_domain_event(&mut tx, event, &self.channel)?;
            tx.commit().map_err(db)?;
            (folded, sequence)
        };
        // Signal only after the transaction commits so a woken consumer
        // never observes a watermark for an uncommitted (or rolled-back)
        // append.
        self.notifier.notify(&event.run_id, sequence);
        Ok(folded)
    }

    fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
        let mut client = self.lock()?;
        list_domain_event_rows(&mut *client, run_id)
    }
}

impl AttemptStore for PostgresStore {
    fn save_attempt(
        &self,
        run_id: &str,
        attempt: &Attempt,
        expected_version: Option<u64>,
    ) -> Result<(), StoreError> {
        let mut client = self.lock()?;
        let Some(expected) = expected_version else {
            // Unconditional upsert: a direct caller's write, or the
            // domain-event projection (already inside the version-checked
            // append transaction).
            return upsert_attempt(&mut *client, run_id, attempt);
        };
        // Versioned write (the harness's iteration-boundary aggregate
        // rollup): verify the lifecycle's optimistic-concurrency key under
        // FOR UPDATE in the same transaction as the upsert so a stale worker
        // cannot clobber counters after the run moved on.
        let mut tx = client.transaction().map_err(db)?;
        let actual = stored_version(&mut tx, run_id, true)?
            .ok_or_else(|| StoreError::lifecycle_not_found(run_id))?;
        if actual != expected {
            return Err(StoreError::optimistic_concurrency(run_id, expected, actual));
        }
        upsert_attempt(&mut tx, run_id, attempt)?;
        tx.commit().map_err(db)
    }

    fn load_attempt(&self, run_id: &str, number: u32) -> Result<Option<Attempt>, StoreError> {
        let mut client = self.lock()?;
        load_attempt_row(&mut *client, run_id, number)
    }

    fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError> {
        let mut client = self.lock()?;
        list_attempt_rows(&mut *client, run_id)
    }
}

impl GraderResultStore for PostgresStore {
    fn append_grader_result(
        &self,
        result: &GraderResultRecord,
    ) -> Result<GraderResultRecord, StoreError> {
        let mut client = self.lock()?;
        insert_grader_result(&mut *client, result)
    }

    fn list_grader_results(
        &self,
        run_id: &str,
        attempt_number: u32,
    ) -> Result<Vec<GraderResultRecord>, StoreError> {
        let mut client = self.lock()?;
        list_grader_result_rows(&mut *client, run_id, attempt_number)
    }
}

impl ControlCommandStore for PostgresStore {
    fn enqueue_command(
        &self,
        run_id: &str,
        kind: &str,
        payload: JsonObject,
        now: DateTime<Utc>,
    ) -> Result<ControlCommandRecord, StoreError> {
        // Persist verbatim. claimed_at stays NULL so the row joins the
        // pending queue immediately; BIGSERIAL assigns id, the per-run
        // enqueue-order key claim_commands consumes.
        let payload_json = Value::Object(payload.clone());
        let id: i64 = {
            let mut client = self.lock()?;
            let row = client
                .query_one(
                    "INSERT INTO control_commands (run_id, kind, payload_json, enqueued_at, claimed_at)
                     VALUES ($1, $2, $3, $4, NULL) RETURNING id",
                    &[&run_id, &kind, &payload_json, &now],
                )
                .map_err(db)?;
            col(&row, 0)?
        };
        let mut record = ControlCommandRecord::pending(run_id, kind, payload, now);
        record.id = Some(id);
        Ok(record)
    }

    fn claim_commands(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<ControlCommandRecord>, StoreError> {
        // Claim-once across concurrent workers: the inner SELECT pins each
        // pending row with FOR UPDATE SKIP LOCKED, so a second worker running
        // claim_commands at the same time sees no overlap with this caller's
        // slice. The outer UPDATE then flips claimed_at and returns the row.
        // RETURNING yields rows in arbitrary order; sort by id afterwards to
        // honor the enqueue-order contract.
        let rows = {
            let mut client = self.lock()?;
            client
                .query(
                    "UPDATE control_commands SET claimed_at = $1
                     WHERE id IN (
                         SELECT id FROM control_commands
                         WHERE run_id = $2 AND claimed_at IS NULL
                         ORDER BY id
                         FOR UPDATE SKIP LOCKED
                     )
                     RETURNING id, run_id, kind, payload_json, enqueued_at, claimed_at",
                    &[&now, &run_id],
                )
                .map_err(db)?
        };
        let mut records = rows
            .iter()
            .map(control_command_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        records.sort_by_key(|r| r.id);
        Ok(records)
    }

    fn delete_command(&self, command_id: i64) -> Result<(), StoreError> {
        // Queue hygiene (spec 00025 FR-10): the consumer deletes an applied
        // row only after the steering domain event committed. Unknown ids
        // are a no-op by contract.
        let mut client = self.lock()?;
        client
            .execute("DELETE FROM control_commands WHERE id = $1", &[&command_id])
            .map_err(db)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_embedded_from_the_canonical_file() {
        assert!(SCHEMA_SQL.contains("timestamps_json         JSONB NOT NULL"));
        assert!(SCHEMA_SQL.contains("INSERT INTO schema_version (id, version) VALUES (1, 12)"));
        assert!(SCHEMA_SQL.contains("grader_results_reject_mutation"));
        assert!(SCHEMA_SQL.contains("ERRCODE = 'check_violation'"));
    }

    #[test]
    fn schema_names_are_identifier_safe_or_rejected() {
        for ok in ["public", "flywheel", "_x", "Fw_01", "a"] {
            assert_eq!(validate_schema(ok), Ok(ok));
        }
        for bad in [
            "public; drop table x",
            "1invalid",
            "bad-name",
            "",
            "sch ema",
            "\u{e9}",
        ] {
            let err = validate_schema(bad).unwrap_err();
            assert_eq!(err, InvalidSchemaName(bad.to_owned()));
            assert_eq!(
                err.to_string(),
                format!("invalid schema name '{bad}': must match [A-Za-z_][A-Za-z0-9_]*")
            );
        }
    }

    #[test]
    fn channel_is_schema_qualified_and_length_capped() {
        assert_eq!(derive_channel("public").unwrap(), "flywheel_run_public");
        let longest_ok = "s".repeat(MAX_CHANNEL_BYTES - "flywheel_run_".len());
        assert!(derive_channel(&longest_ok).is_ok());
        let too_long = "s".repeat(MAX_CHANNEL_BYTES - "flywheel_run_".len() + 1);
        let err = derive_channel(&too_long).unwrap_err();
        assert_eq!(err.schema, too_long);
        assert!(err.to_string().contains("exceeds 63 bytes"));
    }

    #[test]
    fn invalid_schema_fails_before_any_connection_attempt() {
        // An unroutable DSN: a connection attempt would surface a driver
        // error, so a schema error proves validation ran first.
        let err =
            PostgresStore::connect("postgresql://u:p@127.0.0.1:1/db", "bad-name").unwrap_err();
        let StoreError::Backend(source) = &err else {
            panic!("expected a backend error, got {err}");
        };
        assert!(
            source.downcast_ref::<InvalidSchemaName>().is_some(),
            "{err}"
        );
        let too_long = "s".repeat(80);
        let err = PostgresStore::connect("postgresql://u:p@127.0.0.1:1/db", &too_long).unwrap_err();
        let StoreError::Backend(source) = &err else {
            panic!("expected a backend error, got {err}");
        };
        assert!(source.downcast_ref::<ChannelTooLong>().is_some(), "{err}");
    }

    #[test]
    fn unreachable_server_fails_loudly_without_leaking_the_password() {
        let err = PostgresStore::connect(
            "postgresql://flywheel:hunter2@127.0.0.1:1/flywheel?connect_timeout=1",
            "public",
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, StoreError::Backend(_)), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("error connecting to server"), "{text}");
    }

    #[test]
    fn notify_payloads_split_on_the_last_colon() {
        let notifier = RunNotifier::new();
        handle_notify(&notifier, "run:with:colons:7");
        assert_eq!(notifier.watermark("run:with:colons"), 7);
        handle_notify(&notifier, "no-separator");
        handle_notify(&notifier, "run-x:not-a-number");
        assert_eq!(notifier.watermark("run-x"), 0);
    }

    #[test]
    fn timestamps_round_trip_through_json() {
        let at = DateTime::from_timestamp(1_780_000_000, 250_000_000).unwrap();
        let mut timestamps = BTreeMap::new();
        timestamps.insert(Status::Ready, at);
        timestamps.insert(Status::Done, at);
        let value = serialize_timestamps(&timestamps);
        assert_eq!(value["ready"], "2026-05-28T20:26:40.250000+00:00");
        assert_eq!(deserialize_timestamps(value).unwrap(), timestamps);
        assert!(deserialize_timestamps(Value::Array(vec![])).is_err());
    }

    #[test]
    fn integer_columns_reject_out_of_range_values() {
        assert_eq!(int4(7u64, "x").unwrap(), 7);
        let err = int4(u64::from(u32::MAX), "lifecycles.version").unwrap_err();
        assert!(
            err.to_string().contains("lifecycles.version INTEGER"),
            "{err}"
        );
        assert_eq!(int8(u64::MAX / 4, "y").unwrap(), (u64::MAX / 4) as i64);
        assert!(int8(u64::MAX, "y").is_err());
        assert!(unsigned::<i32, u32>(-1, "z").is_err());
        assert_eq!(unsigned::<i64, u64>(5, "z").unwrap(), 5);
    }

    #[test]
    fn filter_clause_uses_array_membership_and_positional_params() {
        let (where_clause, params) = filter_clause(
            &LifecycleFilter::default()
                .with_statuses([Status::Ready])
                .with_task_id("t"),
        )
        .unwrap();
        assert_eq!(where_clause, "WHERE l.status = ANY($1) AND l.task_id = $2");
        assert_eq!(params.len(), 2);
        assert!(filter_clause(&LifecycleFilter::default().with_statuses([])).is_none());
        let (empty, none) = filter_clause(&LifecycleFilter::default()).unwrap();
        assert_eq!(empty, "");
        assert!(none.is_empty());
    }
}
