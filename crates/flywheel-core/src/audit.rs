//! The per-run observability stream: reader, live follower, and CLI surface.
//!
//! Since spec 00025 the durable telemetry destination is the per-run JSONL
//! file written by [`crate::telemetry_file::FileTelemetrySink`] at
//! `<logs_root>/runs/<run_id>.jsonl`, not the store. This module is the
//! canonical reader for that stream. It tails the file (the cursor is a byte
//! offset plus a line count) and reconstructs the [`AuditRecord`] shapes
//! consumers receive, so the record contract, the [`Redactor`], and every
//! renderer are unchanged by the destination move.
//!
//! Three consumers are served:
//!
//! * [`stream`]: a lazy iterator. Replay (`follow = false`) drains every
//!   complete line currently in the file and stops; a missing file reads as
//!   empty. Live tail (`follow = true`) drains, then polls the file until the
//!   lifecycle reaches a terminal status. The store is the terminal oracle
//!   (`load_lifecycle` decides when the run is over; the file decides what
//!   happened). The follow loop drains once more *after* observing the
//!   terminal status so the final write is never dropped, and a partial
//!   trailing line (crash mid-write) is withheld until complete or discarded
//!   on that final pass. After the file is exhausted the run's landing-stage
//!   ledger records (held-out gate verdict, landing-parked witness, landed
//!   record) are projected from the store at the tail, numbered strictly
//!   after the last file record.
//! * [`subscribe`] / [`attach_logger`]: a background thread that follows the
//!   stream and hands each record to an [`EventHandler`] / [`AuditLogger`],
//!   with per-record error isolation. The returned handle stops and joins
//!   the thread.
//! * [`run_audit`]: the `flywheel audit` verb as a library function. It
//!   renders exactly what the Python CLI renders (text and NDJSON modes,
//!   redaction on by default with the stderr notice, `--dry-run` coverage
//!   report, `--follow`) against an already-open store and a caller-supplied
//!   `Write`; the product shell owns argument parsing and store opening.
//!
//! Line classification mirrors the merged stream's surface: SDK message
//! lines become [`SdkMessageRecord`], `harness.*` telemetry lines become
//! [`EventRecord`], and `domain.*` ledger mirrors are skipped (ledger state
//! was never part of the audit stream's record surface; consumers read it
//! from the store). A malformed but complete line is skipped rather than
//! raised so one corrupt write cannot wedge a live tail. Skipped lines still
//! advance the cursor and consume a sequence number, so sequences are stable
//! regardless of which reader assigned them.
//!
//! Every read path here is line-at-a-time over a buffered handle: the
//! reader never materializes a whole run file, so a multi-gigabyte log tails
//! in constant memory.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{Map, Value};

use crate::event_serde::{event_payload, format_timestamp, parse_timestamp};
use crate::events::DomainEventKind;
use crate::lifecycle::Status;
use crate::loaders::canonical_json;
use crate::redaction::{EnvValueRedactor, Redactor, compose, default_policy, strict_policy};
use crate::store::{
    AuditRecord, DomainEventStore, EventRecord, JsonObject, LifecycleStore, SdkMessageRecord,
    StoreError,
};
use crate::telemetry_file::{DEFAULT_LOGS_ROOT, JsonStyle, RUNS_DIRNAME, python_json_dumps};

// --- Errors ---------------------------------------------------------------------

/// Every failure the audit readers and CLI can surface. Skipped lines are
/// not errors; only the store and the filesystem can fail a read.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("audit io: {0}")]
    Io(#[from] io::Error),
}

// --- File reader ----------------------------------------------------------------

/// Kind prefix the in-run mirror stamps onto a ledger event it writes into
/// the run file. Lines with this prefix are skipped by the reader; the same
/// prefix labels the landing-stage records projected from the store so the
/// renderers see one shape regardless of the source.
pub const DOMAIN_MIRROR_PREFIX: &str = "domain.";

/// Kind prefix of harness telemetry events; such lines become
/// [`EventRecord`]s, every other kind becomes an [`SdkMessageRecord`].
pub const TELEMETRY_EVENT_PREFIX: &str = "harness.";

/// Read position inside a run's telemetry JSONL file.
///
/// `offset` is the byte position of the first unconsumed byte; `line` is
/// the count of complete lines consumed so far (the sequence source for
/// reconstructed records). Immutable so callers can hold the previous
/// cursor for retry / comparison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FileCursor {
    pub offset: u64,
    pub line: u64,
}

/// Return the JSONL path for `run_id` under `logs_root`.
///
/// Mirrors [`crate::telemetry_file::FileTelemetrySink::path_for`] so writer
/// and readers agree on the layout without sharing an instance. Performs no
/// validation: a reader that is handed an unsafe `run_id` simply finds no
/// file.
pub fn run_file_path(logs_root: &Path, run_id: &str) -> PathBuf {
    logs_root.join(RUNS_DIRNAME).join(format!("{run_id}.jsonl"))
}

/// Line-at-a-time tailer over one run file.
///
/// Holds the file open across reads so a follower does not reopen the file
/// on every poll, reads one line per call so memory is bounded by the
/// longest line rather than the file, and withholds a partial trailing line
/// until the writer completes it (or discards it when told the file is
/// final). A file that does not exist yet reads as empty; the reader keeps
/// retrying the open on later calls.
#[derive(Debug)]
pub struct RunFileReader {
    path: PathBuf,
    cursor: FileCursor,
    file: Option<BufReader<File>>,
    pending: Vec<u8>,
}

impl RunFileReader {
    /// Position a reader at `cursor` inside `path`.
    pub fn new(path: impl Into<PathBuf>, cursor: FileCursor) -> Self {
        Self {
            path: path.into(),
            cursor,
            file: None,
            pending: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The position after the last complete line consumed.
    pub fn cursor(&self) -> FileCursor {
        self.cursor
    }

    /// `Ok(false)` when the file does not exist (yet).
    fn ensure_open(&mut self) -> io::Result<bool> {
        if self.file.is_some() {
            return Ok(true);
        }
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        file.seek(SeekFrom::Start(self.cursor.offset))?;
        self.file = Some(BufReader::new(file));
        Ok(true)
    }

    /// Read the next complete line (without its newline) past the cursor.
    ///
    /// Returns `None` at end of file, when the file is missing, or when only
    /// a partial line remains. With `eof_final` a trailing partial line is
    /// discarded (the cursor advances past it) instead of being withheld.
    pub fn next_line(&mut self, eof_final: bool) -> io::Result<Option<Vec<u8>>> {
        if !self.ensure_open()? {
            return Ok(None);
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(None);
        };
        file.read_until(b'\n', &mut self.pending)?;
        if self.pending.last() == Some(&b'\n') {
            let mut line = std::mem::take(&mut self.pending);
            self.cursor.offset += line.len() as u64;
            self.cursor.line += 1;
            line.pop();
            return Ok(Some(line));
        }
        if eof_final && !self.pending.is_empty() {
            // Crash mid-write: the trailing bytes will never be completed.
            // Discard them so a re-read of a finished run is idempotent.
            self.cursor.offset += self.pending.len() as u64;
            self.pending.clear();
        }
        Ok(None)
    }

    /// Read lines until one reconstructs into a record (skipping domain
    /// mirrors and malformed lines) or the file is exhausted.
    pub fn next_record(&mut self, eof_final: bool) -> io::Result<Option<AuditRecord>> {
        while let Some(line) = self.next_line(eof_final)? {
            if let Some(record) = record_from_line(&line, self.cursor.line) {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }
}

/// Read every complete line past `cursor` and reconstruct records.
///
/// Returns the parsed records (skipped lines still advance the cursor) and
/// the new cursor. A missing file returns `([], cursor)`. See the module
/// docs for partial-line and classification rules. One page per call; use
/// [`RunFileReader`] directly to read incrementally without materializing a
/// page.
pub fn read_records_since(
    path: &Path,
    cursor: FileCursor,
    eof_final: bool,
) -> io::Result<(Vec<AuditRecord>, FileCursor)> {
    let mut reader = RunFileReader::new(path, cursor);
    let mut records = Vec::new();
    while let Some(record) = reader.next_record(eof_final)? {
        records.push(record);
    }
    Ok((records, reader.cursor()))
}

/// Reconstruct one [`AuditRecord`] from a JSONL line.
///
/// Returns `None` for domain mirrors and for malformed lines (both advance
/// the cursor at the call site; neither reaches consumers).
pub fn record_from_line(raw: &[u8], sequence: u64) -> Option<AuditRecord> {
    let Value::Object(obj) = serde_json::from_slice::<Value>(raw).ok()? else {
        return None;
    };
    let kind = obj.get("kind").and_then(Value::as_str)?;
    if kind.is_empty() || kind.starts_with(DOMAIN_MIRROR_PREFIX) {
        return None;
    }
    let run_id = obj
        .get("run_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let payload: JsonObject = match obj.get("payload") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    let attempt_number = obj.get("attempt_number").and_then(coordinate);
    let iteration_number = obj.get("iteration_number").and_then(coordinate);
    let ts = parse_ts(obj.get("ts"));
    if kind.starts_with(TELEMETRY_EVENT_PREFIX) {
        return Some(AuditRecord::Event(EventRecord {
            run_id,
            ts,
            kind: kind.to_owned(),
            payload,
            attempt_number,
            id: None,
            sequence: Some(sequence),
        }));
    }
    Some(AuditRecord::SdkMessage(SdkMessageRecord {
        run_id,
        attempt_number: attempt_number.unwrap_or(0),
        iteration_number: iteration_number.unwrap_or(0),
        message_type: kind.to_owned(),
        payload,
        ts,
        sequence: Some(sequence),
        id: None,
    }))
}

/// An integer coordinate (`attempt_number` / `iteration_number`); booleans
/// and non-integers are not coordinates.
fn coordinate(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|n| u32::try_from(n).ok())
}

/// Parse the line's ISO-8601 timestamp, defensively.
///
/// A missing or malformed `ts` falls back to the epoch rather than raising;
/// the timestamp is presentation metadata, ordering comes from the file
/// position.
fn parse_ts(value: Option<&Value>) -> DateTime<Utc> {
    value
        .and_then(Value::as_str)
        .and_then(|text| parse_timestamp(text).ok())
        .unwrap_or(DateTime::UNIX_EPOCH)
}

// --- Landing-stage tail ---------------------------------------------------------

/// Terminal lifecycle statuses for the follow loop's exit predicate. The
/// follow loop stops when the lifecycle is in one of these and a subsequent
/// page yielded no new records.
pub const TERMINAL_STATUSES: [Status; 4] = [
    Status::Done,
    Status::Failed,
    Status::InternalError,
    Status::Interrupted,
];

/// Landing-stage ledger kinds the stream projects at the tail, after every
/// file record (i.e. after attempt finalization). These decisions are
/// appended to the store *after* the run finalized, so the harness's in-run
/// mirror never wrote them to the run file; they live only in the store.
pub const LANDING_STAGE_KINDS: [DomainEventKind; 3] = [
    DomainEventKind::HeldOutGateEvaluated,
    DomainEventKind::LandingParked,
    DomainEventKind::Landed,
];

/// Project the run's landing-stage ledger events as tail records.
///
/// Reads the authoritative domain-event ledger and reconstructs each
/// landing-stage event as an [`EventRecord`] carrying the same
/// `domain.<kind>` label and payload projection the in-run mirror uses.
/// Records keep ledger order and are numbered strictly after
/// `start_sequence` (the last file record's sequence) so the stream's
/// ascending-sequence contract holds across the file/ledger seam.
///
/// Best-effort and read-only: a ledger read or projection that fails yields
/// no tail records (the ledger is authoritative and the stream is a
/// disposable projection, so a lookup failure must never break the live
/// file tail). Returns an empty vector when the run has no landing
/// decision, which keeps a non-landing run identical to the pre-projection
/// stream.
pub fn landing_stage_records<S: DomainEventStore + ?Sized>(
    store: &S,
    run_id: &str,
    start_sequence: u64,
) -> Vec<EventRecord> {
    let Ok(events) = store.list_domain_events(run_id) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    let mut sequence = start_sequence;
    for event in &events {
        let kind = event.kind();
        if !LANDING_STAGE_KINDS.contains(&kind) {
            continue;
        }
        let Ok(payload) = event_payload(&event.payload) else {
            return Vec::new();
        };
        sequence += 1;
        records.push(EventRecord {
            run_id: run_id.to_owned(),
            ts: event.ts,
            kind: format!("{DOMAIN_MIRROR_PREFIX}{}", kind.as_str()),
            payload,
            attempt_number: event.attempt_number,
            id: None,
            sequence: Some(sequence),
        });
    }
    records
}

// --- Stop flag ------------------------------------------------------------------

/// A cooperative, clonable cancellation signal with an interruptible wait.
///
/// Used by subscriptions to stop their background thread promptly and by
/// the CLI to turn an operator interrupt into a clean exit; a follower
/// sleeping between polls wakes as soon as the flag is set.
#[derive(Debug, Clone, Default)]
pub struct StopFlag {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl StopFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self) {
        let (lock, cvar) = &*self.inner;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cvar.notify_all();
    }

    pub fn is_set(&self) -> bool {
        *self.inner.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Block for up to `timeout` or until the flag is set; returns whether
    /// the flag is set.
    pub fn wait(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &*self.inner;
        let mut set = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + timeout;
        while !*set {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            set = cvar
                .wait_timeout(set, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *set
    }
}

// --- stream ---------------------------------------------------------------------

/// Default cadence of the follow loop and of `--poll-interval`.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Options for [`stream`] and the subscription constructors.
#[derive(Clone)]
pub struct StreamOptions {
    /// Logs root holding `runs/<run_id>.jsonl`; `None` is
    /// [`DEFAULT_LOGS_ROOT`].
    pub logs_root: Option<PathBuf>,
    /// Tail the file until the lifecycle is terminal. Subscriptions always
    /// follow and ignore this field.
    pub follow: bool,
    pub poll_interval: Duration,
    /// Applied to every record at the single yield seam, after the cursor
    /// math consumed the line; `None` is verbatim.
    pub redactor: Option<Arc<dyn Redactor>>,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            logs_root: None,
            follow: false,
            poll_interval: DEFAULT_POLL_INTERVAL,
            redactor: None,
        }
    }
}

impl std::fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamOptions")
            .field("logs_root", &self.logs_root)
            .field("follow", &self.follow)
            .field("poll_interval", &self.poll_interval)
            .field("redactor", &self.redactor.is_some())
            .finish()
    }
}

fn resolve_logs_root(logs_root: Option<&Path>) -> PathBuf {
    logs_root.map_or_else(|| PathBuf::from(DEFAULT_LOGS_ROOT), Path::to_path_buf)
}

/// Yield the run's observability records in file order.
///
/// Records come from `<logs_root>/runs/<run_id>.jsonl`; `store` supplies
/// only the lifecycle row for the unknown-run guard and the terminal-status
/// exit, plus the landing-stage ledger for the tail. See the module docs
/// for replay versus follow semantics. The iterator is lazy: nothing is
/// read until it is advanced, and a page is never materialized.
///
/// Items are `Err` only for store or filesystem failures; the iterator
/// finishes after yielding an error.
pub fn stream<'a, S>(run_id: &str, store: &'a S, options: &StreamOptions) -> AuditStream<'a, S>
where
    S: LifecycleStore + DomainEventStore + ?Sized,
{
    AuditStream::new(run_id, store, options, true)
}

/// The quiescence window the follow loop waits out after observing a
/// terminal status before its final pass. Independent of the poll interval
/// so a long poll never delays the exit of an already-finished run. A writer that records its last line just after the terminal
/// transition -- or is descheduled between the two under load -- lands that
/// line inside the window instead of after the stream has exited.
pub const FOLLOW_SETTLE_WINDOW: Duration = Duration::from_millis(250);

/// The hard ceiling on the post-terminal settle: a writer that keeps
/// appending after the terminal status cannot hold the follow open past
/// this, so the stream always ends.
pub const FOLLOW_SETTLE_CAP: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Drain,
    FollowCheck,
    /// A non-terminal poll: read every complete line, then re-check status.
    FollowPage,
    /// Terminal observed: keep draining complete lines; on an empty read,
    /// wait one quiescence window (`idle_waited` records that) and read
    /// again, so a write racing the terminal transition is not dropped.
    Settle {
        idle_waited: bool,
    },
    /// The quiescent final pass: drain what remains and discard a partial
    /// trailing line (a crash mid-write that will never complete).
    FinalPage,
    Tail,
    Finished,
}

/// The lazy iterator returned by [`stream`].
pub struct AuditStream<'a, S: ?Sized> {
    run_id: String,
    store: &'a S,
    reader: RunFileReader,
    follow: bool,
    poll_interval: Duration,
    redactor: Option<Arc<dyn Redactor>>,
    stop: Option<StopFlag>,
    include_landing_tail: bool,
    last_sequence: u64,
    phase: Phase,
    settle_started: Option<Instant>,
    tail: std::vec::IntoIter<EventRecord>,
}

impl<'a, S> AuditStream<'a, S>
where
    S: LifecycleStore + DomainEventStore + ?Sized,
{
    fn new(
        run_id: &str,
        store: &'a S,
        options: &StreamOptions,
        include_landing_tail: bool,
    ) -> Self {
        let path = run_file_path(&resolve_logs_root(options.logs_root.as_deref()), run_id);
        Self {
            run_id: run_id.to_owned(),
            store,
            reader: RunFileReader::new(path, FileCursor::default()),
            follow: options.follow,
            poll_interval: options.poll_interval,
            redactor: options.redactor.clone(),
            stop: None,
            include_landing_tail,
            last_sequence: 0,
            phase: Phase::Drain,
            settle_started: None,
            tail: Vec::new().into_iter(),
        }
    }

    /// Make the follow loop cooperatively cancellable: once `stop` is set
    /// the iterator stops polling (its sleep is interrupted) and finishes.
    pub fn with_stop(mut self, stop: StopFlag) -> Self {
        self.stop = Some(stop);
        self
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The reader's position after the last complete line consumed.
    pub fn cursor(&self) -> FileCursor {
        self.reader.cursor()
    }

    fn stopped(&self) -> bool {
        self.stop.as_ref().is_some_and(StopFlag::is_set)
    }

    fn emit(&mut self, record: AuditRecord) -> AuditRecord {
        if let Some(sequence) = record.sequence()
            && sequence > self.last_sequence
        {
            self.last_sequence = sequence;
        }
        match &self.redactor {
            Some(redactor) => redactor.redact(&record),
            None => record,
        }
    }

    fn enter_tail(&mut self) {
        self.tail = if self.include_landing_tail {
            landing_stage_records(self.store, &self.run_id, self.last_sequence).into_iter()
        } else {
            Vec::new().into_iter()
        };
        self.phase = Phase::Tail;
    }

    fn fail(&mut self, error: AuditError) -> Option<Result<AuditRecord, AuditError>> {
        self.phase = Phase::Finished;
        Some(Err(error))
    }

    fn sleep(&self) {
        self.sleep_for(self.poll_interval);
    }

    fn sleep_for(&self, duration: Duration) {
        match &self.stop {
            Some(stop) => {
                stop.wait(duration);
            }
            None => thread::sleep(duration),
        }
    }

    /// Whether one more quiescence wait fits under [`FOLLOW_SETTLE_CAP`].
    fn may_settle(&mut self) -> bool {
        let started = *self.settle_started.get_or_insert_with(Instant::now);
        !self.stopped() && started.elapsed() < FOLLOW_SETTLE_CAP
    }
}

impl<S> Iterator for AuditStream<'_, S>
where
    S: LifecycleStore + DomainEventStore + ?Sized,
{
    type Item = Result<AuditRecord, AuditError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.phase {
                Phase::Drain => match self.reader.next_record(false) {
                    Err(err) => return self.fail(err.into()),
                    Ok(Some(record)) => return Some(Ok(self.emit(record))),
                    Ok(None) if !self.follow => self.enter_tail(),
                    Ok(None) => match self.store.load_lifecycle(&self.run_id) {
                        Err(err) => return self.fail(err.into()),
                        // Spec error-handling table: an unknown run_id
                        // yields nothing and returns even under follow.
                        // Without this guard the follow loop would spin
                        // forever waiting on a lifecycle that will never
                        // exist.
                        Ok(None) => self.phase = Phase::Finished,
                        Ok(Some(_)) => self.phase = Phase::FollowCheck,
                    },
                },
                Phase::FollowCheck => {
                    if self.stopped() {
                        self.enter_tail();
                        continue;
                    }
                    let status = match self.store.load_lifecycle(&self.run_id) {
                        Err(err) => return self.fail(err.into()),
                        Ok(lifecycle) => lifecycle.map(|lc| lc.status),
                    };
                    let Some(status) = status else {
                        self.enter_tail();
                        continue;
                    };
                    if TERMINAL_STATUSES.contains(&status) {
                        self.phase = Phase::Settle { idle_waited: false };
                    } else {
                        self.sleep();
                        self.phase = Phase::FollowPage;
                    }
                }
                Phase::FollowPage => match self.reader.next_record(false) {
                    Err(err) => return self.fail(err.into()),
                    Ok(Some(record)) => return Some(Ok(self.emit(record))),
                    Ok(None) => self.phase = Phase::FollowCheck,
                },
                // A terminal lifecycle does not end the follow on the first
                // empty read: the line written alongside (or just after) the
                // terminal transition may not have landed yet. Drain until a
                // read comes back empty after a full quiescence window, then
                // run the final pass.
                Phase::Settle { idle_waited } => match self.reader.next_record(false) {
                    Err(err) => return self.fail(err.into()),
                    Ok(Some(record)) => {
                        self.phase = Phase::Settle { idle_waited: false };
                        return Some(Ok(self.emit(record)));
                    }
                    Ok(None) if !idle_waited && self.may_settle() => {
                        self.sleep_for(FOLLOW_SETTLE_WINDOW);
                        self.phase = Phase::Settle { idle_waited: true };
                    }
                    Ok(None) => self.phase = Phase::FinalPage,
                },
                Phase::FinalPage => match self.reader.next_record(true) {
                    Err(err) => return self.fail(err.into()),
                    Ok(Some(record)) => return Some(Ok(self.emit(record))),
                    Ok(None) => self.enter_tail(),
                },
                Phase::Tail => match self.tail.next() {
                    Some(event) => return Some(Ok(self.emit(AuditRecord::Event(event)))),
                    None => self.phase = Phase::Finished,
                },
                Phase::Finished => return None,
            }
        }
    }
}

// --- Subscriptions ----------------------------------------------------------------

/// The error a subscriber reports for one record.
pub type HandlerError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A read-only subscriber to the per-run observability stream.
///
/// Handlers receive each [`AuditRecord`] in file order. They observe only
/// committed lines and are handed no store or lifecycle handle, so a
/// subscriber cannot mutate authoritative state; the harness remains the
/// sole owner of transitions. A handler that fails (or panics) is isolated:
/// [`on_error`](Self::on_error) is reported and the dispatcher continues to
/// the next record, so a faulty subscriber never breaks the follow loop.
pub trait EventHandler: Send {
    fn on_record(&mut self, record: &AuditRecord) -> Result<(), HandlerError>;

    /// Called when [`on_record`](Self::on_record) or the subscription's
    /// redactor failed for `record`. The default writes a one-line warning
    /// to stderr.
    fn on_error(&mut self, run_id: &str, record: &AuditRecord, error: &HandlerError) {
        eprintln!(
            "audit subscriber for run={run_id} raised on seq={}: {error}",
            record.sequence().unwrap_or(0)
        );
    }

    /// Called when the underlying stream failed (a store or filesystem
    /// error); the subscription ends afterwards. The default writes a
    /// one-line warning to stderr.
    fn on_stream_error(&mut self, run_id: &str, error: &AuditError) {
        eprintln!("audit subscription for run={run_id} failed: {error}");
    }
}

impl<F> EventHandler for F
where
    F: FnMut(&AuditRecord) -> Result<(), HandlerError> + Send,
{
    fn on_record(&mut self, record: &AuditRecord) -> Result<(), HandlerError> {
        self(record)
    }
}

/// A background follower that dispatches each record to a handler.
///
/// Spawns one thread that drains the run file's backlog, then follows the
/// live stream. Each record is handed to the handler in file order. The
/// thread exits on its own when the lifecycle reaches a terminal status
/// (and the final drain is empty) or when the `run_id` is unknown;
/// [`unsubscribe`](Self::unsubscribe) stops it early and joins with a short
/// timeout so a slow handler cannot block the caller indefinitely. Dropping
/// the subscription signals the thread to stop without joining.
#[derive(Debug)]
pub struct Subscription {
    run_id: String,
    stop: StopFlag,
    thread: Option<JoinHandle<()>>,
}

impl Subscription {
    pub const JOIN_TIMEOUT: Duration = Duration::from_secs(1);

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Stop the background dispatcher and join its thread.
    ///
    /// Idempotent: calling it after the thread has exited is a no-op. The
    /// join waits at most [`JOIN_TIMEOUT`](Self::JOIN_TIMEOUT); the stop flag
    /// also interrupts the poll sleep so exit is prompt regardless of the
    /// poll interval.
    pub fn unsubscribe(&mut self) {
        self.stop.set();
        let Some(handle) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + Self::JOIN_TIMEOUT;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            self.thread = Some(handle);
        }
    }

    /// Whether the background thread is still running.
    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.stop.set();
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> HandlerError {
    let message = if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "handler panicked".to_owned()
    };
    HandlerError::from(message)
}

fn dispatch<H: EventHandler + ?Sized>(
    handler: &mut H,
    run_id: &str,
    redactor: Option<&Arc<dyn Redactor>>,
    record: &AuditRecord,
) {
    let outcome = catch_unwind(AssertUnwindSafe(|| match redactor {
        Some(redactor) => handler.on_record(&redactor.redact(record)),
        None => handler.on_record(record),
    }));
    let error = match outcome {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error,
        Err(panic) => panic_message(panic),
    };
    // Never let a failing error reporter take the dispatcher down.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        handler.on_error(run_id, record, &error)
    }));
}

/// Subscribe `handler` to `run_id`'s observability stream.
///
/// The subscriber runs on its own thread, observes records in file order,
/// and is isolated: a failing handler is reported and the stream continues.
/// When `options.redactor` is set, every record passes through it before
/// the handler sees it, inside the same per-record isolation. Returns a
/// [`Subscription`] whose `unsubscribe()` stops and joins the thread.
///
/// Plugins register here without touching the harness and cannot corrupt
/// lifecycle state: they only read committed lines.
pub fn subscribe<S, H>(
    handler: H,
    run_id: &str,
    store: Arc<S>,
    options: &StreamOptions,
) -> Subscription
where
    S: LifecycleStore + DomainEventStore + Send + Sync + ?Sized + 'static,
    H: EventHandler + 'static,
{
    let stop = StopFlag::new();
    let thread_stop = stop.clone();
    let thread_run_id = run_id.to_owned();
    // The redactor is applied inside the per-record isolation below, not at
    // the stream's yield seam, so a failing redactor is reported like a
    // failing handler and the stream continues.
    let redactor = options.redactor.clone();
    let options = StreamOptions {
        follow: true,
        redactor: None,
        ..options.clone()
    };
    let mut handler = handler;
    let thread = thread::Builder::new()
        .name(format!("flywheel-subscription-{run_id}"))
        .spawn(move || {
            let stream = AuditStream::new(&thread_run_id, &*store, &options, false)
                .with_stop(thread_stop.clone());
            for item in stream {
                match item {
                    Ok(record) => {
                        dispatch(&mut handler, &thread_run_id, redactor.as_ref(), &record)
                    }
                    Err(error) => {
                        let _ = catch_unwind(AssertUnwindSafe(|| {
                            handler.on_stream_error(&thread_run_id, &error)
                        }));
                        return;
                    }
                }
                if thread_stop.is_set() {
                    return;
                }
            }
        })
        .expect("spawn audit subscription thread");
    Subscription {
        run_id: run_id.to_owned(),
        stop,
        thread: Some(thread),
    }
}

/// One emission of [`attach_logger`]: a short human-readable `message` of
/// the form `audit run=<id> seq=<n> kind=<k>` plus the structured record.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditLogEntry<'a> {
    pub run_id: &'a str,
    pub sequence: u64,
    /// `event:<kind>` or `sdk:<message_type>`.
    pub kind: String,
    pub message: String,
    pub record: &'a AuditRecord,
}

impl AuditLogEntry<'_> {
    /// The record as a plain JSON object (the structured `extra` field a
    /// JSON log sink would route).
    pub fn record_as_json(&self) -> Value {
        match self.record {
            AuditRecord::Event(event) => serde_json::to_value(event),
            AuditRecord::SdkMessage(message) => serde_json::to_value(message),
        }
        .unwrap_or(Value::Null)
    }
}

/// Destination of [`attach_logger`]: receives one entry per audit record.
pub trait AuditLogger: Send {
    fn log(&mut self, entry: &AuditLogEntry<'_>);
}

impl<F> AuditLogger for F
where
    F: FnMut(&AuditLogEntry<'_>) + Send,
{
    fn log(&mut self, entry: &AuditLogEntry<'_>) {
        self(entry)
    }
}

struct LoggerHandler<L> {
    run_id: String,
    logger: L,
}

impl<L: AuditLogger> EventHandler for LoggerHandler<L> {
    fn on_record(&mut self, record: &AuditRecord) -> Result<(), HandlerError> {
        let kind = record_kind_label(record);
        let sequence = record.sequence().unwrap_or(0);
        let entry = AuditLogEntry {
            run_id: &self.run_id,
            sequence,
            message: format!("audit run={} seq={sequence} kind={kind}", self.run_id),
            kind,
            record,
        };
        self.logger.log(&entry);
        Ok(())
    }
}

/// Handle returned by [`attach_logger`]: a [`Subscription`] whose
/// [`detach`](Self::detach) stops emission and joins the thread.
#[derive(Debug)]
pub struct AuditLoggerHandle {
    subscription: Subscription,
}

impl AuditLoggerHandle {
    /// Stop the background emitter and join the thread (alias for
    /// [`Subscription::unsubscribe`]). Idempotent.
    pub fn detach(&mut self) {
        self.subscription.unsubscribe();
    }

    pub fn is_alive(&self) -> bool {
        self.subscription.is_alive()
    }

    pub fn run_id(&self) -> &str {
        self.subscription.run_id()
    }
}

/// Emit every observability record for `run_id` through `logger`.
///
/// A convenience subscriber: spawns a thread that follows the run's stream
/// and calls `logger.log(entry)` for each record. There are no global side
/// effects: calling `attach_logger` twice returns distinct handles, each
/// owning its own thread. Equivalent to [`subscribe`] with a logging
/// handler.
pub fn attach_logger<S, L>(
    logger: L,
    run_id: &str,
    store: Arc<S>,
    options: &StreamOptions,
) -> AuditLoggerHandle
where
    S: LifecycleStore + DomainEventStore + Send + Sync + ?Sized + 'static,
    L: AuditLogger + 'static,
{
    let handler = LoggerHandler {
        run_id: run_id.to_owned(),
        logger,
    };
    AuditLoggerHandle {
        subscription: subscribe(handler, run_id, store, options),
    }
}

/// The `kind=<k>` label shared by the logger emitter and the CLI's text
/// mode: `event:<kind>` for events, `sdk:<message_type>` for SDK messages.
pub fn record_kind_label(record: &AuditRecord) -> String {
    match record {
        AuditRecord::Event(event) => format!("event:{}", event.kind),
        AuditRecord::SdkMessage(message) => format!("sdk:{}", message.message_type),
    }
}

// --- CLI surface ----------------------------------------------------------------

/// Default store path used when neither `--db` nor `$FLYWHEEL_DB` is set.
/// Matches the spec's `./flywheel.db` fallback: the audit CLI is a debugging
/// tool that may run outside a `.flywheel/` layout.
pub const DEFAULT_DB_PATH: &str = "flywheel.db";

/// Total preview budget for the text mode. Fixed (not terminal-width
/// derived) so output is reproducible across pipes and tests.
pub const PREVIEW_MAX_CHARS: usize = 80;

/// Truncation hint appended when the preview body would exceed
/// [`PREVIEW_MAX_CHARS`]. The hint occupies part of the budget so the final
/// line never exceeds it.
pub const TRUNCATION_HINT: &str = "... (use --json for full payload)";

/// Environment variables whose ambient values the CLI seeds into an
/// [`EnvValueRedactor`] when building a named built-in policy. The redactor
/// silently drops absent or too-short values, so this is safe to extend.
pub const CLI_ENV_SEEDS: [&str; 1] = ["ANTHROPIC_API_KEY"];

/// Names of the built-in policies addressable via `--redact-policy`.
pub const BUILTIN_POLICY_NAMES: [&str; 2] = ["default", "strict"];

/// A registered custom `--redact-policy` constructor: receives the
/// `--redact-salt` bytes (if any) and returns the policy, or a message that
/// is reported verbatim as the resolution failure.
pub type PolicyFactory =
    Arc<dyn Fn(Option<&[u8]>) -> Result<Box<dyn Redactor>, String> + Send + Sync>;

/// The `flywheel audit` verb's options, mirroring the Python CLI flags
/// one-for-one. The product shell parses the command line into this.
#[derive(Clone)]
pub struct AuditArgs {
    /// The run whose audit stream to read.
    pub run_id: String,
    /// `--json`: NDJSON, one object per record, instead of the text view.
    pub json: bool,
    /// `--follow`: tail until the lifecycle reaches a terminal status.
    pub follow: bool,
    /// `--poll-interval`: cadence of the follow loop.
    pub poll_interval: Duration,
    /// `--redact`: explicit opt-in (already the default).
    pub redact: bool,
    /// `--redact-policy`: `default`, `strict`, or a name registered in
    /// [`custom_policies`](Self::custom_policies). `None` is `default`.
    pub redact_policy: Option<String>,
    /// `--raw`: verbatim output; disables redaction and its notice.
    pub raw: bool,
    /// `--dry-run`: coverage report only; no payload content is printed.
    pub dry_run: bool,
    /// `--redact-salt`: stable `[REDACTED:label:digest]` tokens.
    pub redact_salt: Option<String>,
    /// Custom policies addressable by name through `--redact-policy`.
    pub custom_policies: Vec<(String, PolicyFactory)>,
    /// Set by the shell's interrupt handler (Ctrl-C under `--follow`):
    /// streaming stops promptly and the verb exits 130.
    pub interrupt: Option<StopFlag>,
}

impl AuditArgs {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            json: false,
            follow: false,
            poll_interval: DEFAULT_POLL_INTERVAL,
            redact: false,
            redact_policy: None,
            raw: false,
            dry_run: false,
            redact_salt: None,
            custom_policies: Vec::new(),
            interrupt: None,
        }
    }
}

impl std::fmt::Debug for AuditArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditArgs")
            .field("run_id", &self.run_id)
            .field("json", &self.json)
            .field("follow", &self.follow)
            .field("poll_interval", &self.poll_interval)
            .field("redact", &self.redact)
            .field("redact_policy", &self.redact_policy)
            .field("raw", &self.raw)
            .field("dry_run", &self.dry_run)
            .field("redact_salt", &self.redact_salt)
            .field(
                "custom_policies",
                &self
                    .custom_policies
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("interrupt", &self.interrupt.is_some())
            .finish()
    }
}

/// Resolve the `--db` argument with `$FLYWHEEL_DB` fallback.
///
/// Precedence: explicit `--db` > `$FLYWHEEL_DB` > [`DEFAULT_DB_PATH`]. An
/// empty value is treated as unset so a stray `FLYWHEEL_DB=` does not
/// silently shadow the default.
pub fn resolve_db_path(arg: Option<&str>) -> PathBuf {
    if let Some(arg) = arg.filter(|a| !a.is_empty()) {
        return PathBuf::from(arg);
    }
    match std::env::var("FLYWHEEL_DB") {
        Ok(env) if !env.is_empty() => PathBuf::from(env),
        _ => PathBuf::from(DEFAULT_DB_PATH),
    }
}

/// The logs root the CLI uses when `--logs-root` is absent: the `logs`
/// directory next to the database.
pub fn logs_root_for_db(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf)
        .join("logs")
}

/// Return a human-readable error for any disallowed flag combination.
///
/// `--raw` is the only mutually-exclusive flag: combining it with any
/// redaction-shaping flag is operator confusion. All other combinations are
/// valid.
pub fn validate_flag_combinations(args: &AuditArgs) -> Option<String> {
    if !args.raw {
        return None;
    }
    let mut conflicting = Vec::new();
    if args.redact {
        conflicting.push("--redact");
    }
    if args.redact_policy.is_some() {
        conflicting.push("--redact-policy");
    }
    if args.dry_run {
        conflicting.push("--dry-run");
    }
    if args.redact_salt.is_some() {
        conflicting.push("--redact-salt");
    }
    if conflicting.is_empty() {
        return None;
    }
    Some(format!(
        "--raw cannot be combined with {} (redaction is either fully off or fully on)",
        conflicting.join(", ")
    ))
}

/// `--redact-policy` resolution failed; reported before any record streams.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PolicyError(pub String);

/// `repr(text)` for a Python string, for operator-facing messages that
/// mirror the Python CLI.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Build one of the built-in named policies, composed with an
/// [`EnvValueRedactor`] seeded from the ambient [`CLI_ENV_SEEDS`] so the
/// operator's own API keys are redacted even when they appear verbatim in
/// agent output (the most common leak vector).
fn build_named_policy(name: &str, salt: Option<&[u8]>) -> Option<Box<dyn Redactor>> {
    let base: Box<dyn Redactor> = match name {
        "default" => Box::new(default_policy(salt)),
        "strict" => Box::new(strict_policy(salt)),
        _ => return None,
    };
    let env_redactor = EnvValueRedactor::new(&CLI_ENV_SEEDS, salt)
        .expect("CLI_ENV_SEEDS are non-empty variable names");
    Some(Box::new(compose(vec![base, Box::new(env_redactor)])))
}

/// Resolve the requested `--redact-policy` into `(redactor, display label)`.
///
/// `None` is `default`; `default` / `strict` are the built-ins; any other
/// name must be registered in `args.custom_policies`, whose factory
/// receives the salt and owns its own configuration. Resolution failures
/// are [`PolicyError`]s so [`run_audit`] can translate them into a single
/// exit-2 message before any record is streamed.
pub fn resolve_policy(args: &AuditArgs) -> Result<(Box<dyn Redactor>, String), PolicyError> {
    let salt_bytes: Option<Vec<u8>> = args.redact_salt.as_ref().map(|s| s.as_bytes().to_vec());
    let salt = salt_bytes.as_deref();
    let name = args.redact_policy.as_deref().unwrap_or("default");
    if let Some(redactor) = build_named_policy(name, salt) {
        return Ok((redactor, name.to_owned()));
    }
    if let Some((_, factory)) = args.custom_policies.iter().find(|(n, _)| n == name) {
        return match factory(salt) {
            Ok(redactor) => Ok((redactor, name.to_owned())),
            Err(message) => Err(PolicyError(format!(
                "--redact-policy {} factory raised: {message}",
                py_repr(name)
            ))),
        };
    }
    let mut builtin: Vec<&str> = BUILTIN_POLICY_NAMES.to_vec();
    builtin.sort_unstable();
    let mut custom: Vec<&str> = args
        .custom_policies
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    custom.sort_unstable();
    custom.dedup();
    let listed: Vec<String> = builtin.into_iter().chain(custom).map(py_repr).collect();
    if name.contains(':') {
        // The Python CLI imported `module.sub:factory` at run time; a
        // compiled binary has no importable modules, so say so instead of
        // reporting the dotted path as a mistyped built-in name.
        return Err(PolicyError(format!(
            "--redact-policy {}: dotted-path policy factories ('module.sub:factory') are not \
             supported; expected one of [{}]",
            py_repr(name),
            listed.join(", ")
        )));
    }
    Err(PolicyError(format!(
        "unknown built-in policy {}; expected one of [{}] or a registered custom policy",
        py_repr(name),
        listed.join(", ")
    )))
}

fn record_attempt(record: &AuditRecord) -> String {
    match record {
        AuditRecord::Event(event) => event
            .attempt_number
            .map_or_else(|| "-".to_owned(), |n| n.to_string()),
        AuditRecord::SdkMessage(message) => message.attempt_number.to_string(),
    }
}

fn record_iter(record: &AuditRecord) -> String {
    match record {
        AuditRecord::Event(_) => "-".to_owned(),
        AuditRecord::SdkMessage(message) => message.iteration_number.to_string(),
    }
}

fn record_payload(record: &AuditRecord) -> &JsonObject {
    match record {
        AuditRecord::Event(event) => &event.payload,
        AuditRecord::SdkMessage(message) => &message.payload,
    }
}

/// A single-line truncated payload preview.
///
/// The payload is rendered as compact JSON (sorted keys for determinism),
/// embedded newlines flattened to spaces. If the result is longer than
/// [`PREVIEW_MAX_CHARS`] the tail is replaced by [`TRUNCATION_HINT`] so the
/// total stays within budget.
pub fn payload_preview(record: &AuditRecord) -> String {
    let body =
        canonical_json(&Value::Object(record_payload(record).clone())).replace(['\n', '\r'], " ");
    if body.chars().count() <= PREVIEW_MAX_CHARS {
        return body;
    }
    let keep = PREVIEW_MAX_CHARS.saturating_sub(TRUNCATION_HINT.chars().count());
    let mut out: String = body.chars().take(keep).collect();
    out.push_str(TRUNCATION_HINT);
    out
}

/// Format one record as a text-mode line:
/// `<iso-ts> seq=<n> attempt=<a> iter=<i> kind=<k> | <preview>`.
/// Sequence renders as `?` if absent.
pub fn render_default(record: &AuditRecord) -> String {
    let sequence = record
        .sequence()
        .map_or_else(|| "?".to_owned(), |n| n.to_string());
    format!(
        "{} seq={sequence} attempt={} iter={} kind={} | {}",
        format_timestamp(&record.ts()),
        record_attempt(record),
        record_iter(record),
        record_kind_label(record),
        payload_preview(record)
    )
}

/// Format one record as an NDJSON object.
///
/// The schema is fixed so downstream consumers can rely on the field set
/// without sniffing the record type: `iteration_number` is `null` for
/// events, `kind_or_message_type` carries the per-type discriminator
/// without a prefix.
pub fn render_json(record: &AuditRecord) -> String {
    let mut obj = Map::with_capacity(7);
    obj.insert(
        "ts".to_owned(),
        Value::String(format_timestamp(&record.ts())),
    );
    obj.insert(
        "sequence".to_owned(),
        record.sequence().map_or(Value::Null, Value::from),
    );
    obj.insert(
        "run_id".to_owned(),
        Value::String(record.run_id().to_owned()),
    );
    let (attempt, iteration, discriminator) = match record {
        AuditRecord::Event(event) => (
            event.attempt_number.map_or(Value::Null, Value::from),
            Value::Null,
            event.kind.clone(),
        ),
        AuditRecord::SdkMessage(message) => (
            Value::from(message.attempt_number),
            Value::from(message.iteration_number),
            message.message_type.clone(),
        ),
    };
    obj.insert("attempt_number".to_owned(), attempt);
    obj.insert("iteration_number".to_owned(), iteration);
    obj.insert(
        "kind_or_message_type".to_owned(),
        Value::String(discriminator),
    );
    obj.insert(
        "payload".to_owned(),
        Value::Object(record_payload(record).clone()),
    );
    python_json_dumps(&Value::Object(obj), &JsonStyle::SORTED)
}

/// Strip the trailing `:<digest>` suffix from a token label when salt is in
/// use. Labels may legitimately contain colons (`tool:Read`), so only the
/// last segment is stripped and only when it is exactly
/// [`crate::redaction::DIGEST_HEX_LEN`] lowercase hex chars.
fn strip_digest(label: &str, salt_in_use: bool) -> &str {
    if !salt_in_use {
        return label;
    }
    let Some((head, tail)) = label.rsplit_once(':') else {
        return label;
    };
    if tail.len() != crate::redaction::DIGEST_HEX_LEN
        || !tail
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        return label;
    }
    head
}

/// Count every `[REDACTED:label[:digest]]` token in `value` by stripped
/// label; returns the total.
fn count_tokens(
    value: &Value,
    token_re: &Regex,
    label_counts: &mut HashMap<String, usize>,
    salt_in_use: bool,
) -> usize {
    match value {
        Value::String(text) => {
            let mut total = 0;
            for captures in token_re.captures_iter(text) {
                let label = strip_digest(&captures[1], salt_in_use);
                *label_counts.entry(label.to_owned()).or_insert(0) += 1;
                total += 1;
            }
            total
        }
        Value::Object(map) => map
            .values()
            .map(|item| count_tokens(item, token_re, label_counts, salt_in_use))
            .sum(),
        Value::Array(items) => items
            .iter()
            .map(|item| count_tokens(item, token_re, label_counts, salt_in_use))
            .sum(),
        _ => 0,
    }
}

/// Regex used by `--dry-run` to discover `[REDACTED:<label>[:<digest>]]`
/// tokens inside the redacted payload tree.
fn redaction_token_regex() -> Regex {
    Regex::new(r"\[REDACTED:([^\[\]]+)\]").expect("static token regex")
}

/// Aggregate `--dry-run` coverage counters.
#[derive(Debug, Default)]
struct DryRunReport {
    records_scanned: usize,
    records_redacted: usize,
    total_redactions: usize,
    label_counts: HashMap<String, usize>,
}

impl DryRunReport {
    /// Print the coverage report. Intentionally emits no payload content:
    /// only aggregate counts and per-label hit totals, sorted by `(count
    /// desc, label asc)` so output is byte-deterministic.
    fn write(&self, out: &mut dyn Write, policy_label: &str) -> io::Result<()> {
        writeln!(out, "dry-run coverage report")?;
        writeln!(out, "policy: {policy_label}")?;
        writeln!(out, "records scanned: {}", self.records_scanned)?;
        writeln!(out, "records redacted: {}", self.records_redacted)?;
        writeln!(out, "total redactions: {}", self.total_redactions)?;
        if self.label_counts.is_empty() {
            return writeln!(out, "hits by label: (none)");
        }
        writeln!(out, "hits by label:")?;
        let mut labels: Vec<(&String, &usize)> = self.label_counts.iter().collect();
        labels.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (label, count) in labels {
            writeln!(out, "  {label}: {count}")?;
        }
        Ok(())
    }
}

/// The redact-by-default stderr notice: the policy line plus the
/// best-effort caveat so the operator can never mistake "redacted" for
/// "safe".
fn emit_redaction_notice(err: &mut dyn Write, policy_label: &str) -> io::Result<()> {
    writeln!(
        err,
        "redaction: {policy_label} policy applied (use --raw for verbatim)"
    )?;
    writeln!(
        err,
        "note: redaction is best-effort; unmatched secrets may pass through; \
         the store remains the unredacted source of truth"
    )
}

/// Exit code reported when the operator interrupted a stream.
pub const EXIT_INTERRUPTED: i32 = 130;

/// Run the `flywheel audit` verb against an already-open store.
///
/// Writes exactly what the Python CLI writes to `out` / `err` and returns
/// its exit code:
///
/// * `2` with an `error: ...` line for a disallowed flag combination or an
///   unresolvable `--redact-policy` (both decided before any record
///   streams).
/// * `0` with `no records for run_id <id>` on `err` for an unknown run; no
///   redaction notice is emitted on this path.
/// * `0` after streaming; redaction is on by default and announces itself
///   on `err` (suppressed by `--raw`). `--dry-run` prints the coverage
///   report instead of records.
/// * [`EXIT_INTERRUPTED`] when `args.interrupt` was set mid-stream.
///
/// The caller resolves the store path (see [`resolve_db_path`]) and the
/// logs root (see [`logs_root_for_db`]); a missing store file is the
/// caller's error to report, since opening it would silently bootstrap an
/// empty database. Store and filesystem failures are returned as `Err`.
pub fn run_audit<S>(
    args: &AuditArgs,
    store: &S,
    logs_root: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, AuditError>
where
    S: LifecycleStore + DomainEventStore + ?Sized,
{
    if let Some(message) = validate_flag_combinations(args) {
        writeln!(err, "error: {message}")?;
        return Ok(2);
    }
    // Resolve the policy *before* touching the store so a malformed policy
    // fails fast with a clean exit, not midway through a stream. `--raw`
    // skips this entirely and streams verbatim.
    let (redactor, policy_label): (Option<Box<dyn Redactor>>, String) = if args.raw {
        (None, "raw".to_owned())
    } else {
        match resolve_policy(args) {
            Ok((redactor, label)) => (Some(redactor), label),
            Err(PolicyError(message)) => {
                writeln!(err, "error: {message}")?;
                return Ok(2);
            }
        }
    };
    // Spec error-handling table: unknown run_id yields nothing and exits 0
    // with a clear stderr message. Checked *before* draining so the message
    // fires whether or not any records exist (an orphaned file without a
    // lifecycle row is a store-integrity bug, not a user-facing error).
    if store.load_lifecycle(&args.run_id)?.is_none() {
        writeln!(err, "no records for run_id {}", args.run_id)?;
        return Ok(0);
    }
    if redactor.is_some() {
        emit_redaction_notice(err, &policy_label)?;
    }
    let options = StreamOptions {
        logs_root: Some(logs_root.to_path_buf()),
        follow: args.follow,
        poll_interval: args.poll_interval,
        redactor: None,
    };
    let mut records = stream(&args.run_id, store, &options);
    if let Some(interrupt) = &args.interrupt {
        records = records.with_stop(interrupt.clone());
    }
    let interrupted = |args: &AuditArgs| args.interrupt.as_ref().is_some_and(StopFlag::is_set);

    if args.dry_run {
        // Dry-run uses the redactor but suppresses payload output entirely.
        // The redactor is present because --dry-run cannot combine with
        // --raw (validate_flag_combinations); fall back to verbatim counting
        // (which finds no tokens) rather than failing if it ever were.
        let token_re = redaction_token_regex();
        let salt_in_use = args.redact_salt.is_some();
        let mut report = DryRunReport::default();
        for item in records {
            let record = item?;
            report.records_scanned += 1;
            let redacted = match &redactor {
                Some(redactor) => redactor.redact(&record),
                None => record,
            };
            let hits = count_tokens(
                &Value::Object(record_payload(&redacted).clone()),
                &token_re,
                &mut report.label_counts,
                salt_in_use,
            );
            if hits > 0 {
                report.records_redacted += 1;
                report.total_redactions += hits;
            }
            if interrupted(args) {
                return Ok(EXIT_INTERRUPTED);
            }
        }
        report.write(out, &policy_label)?;
        return Ok(0);
    }

    let render = if args.json {
        render_json
    } else {
        render_default
    };
    for item in records {
        let record = item?;
        let visible = match &redactor {
            Some(redactor) => redactor.redact(&record),
            None => record,
        };
        writeln!(out, "{}", render(&visible))?;
        out.flush()?;
        if interrupted(args) {
            return Ok(EXIT_INTERRUPTED);
        }
    }
    if interrupted(args) {
        return Ok(EXIT_INTERRUPTED);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{DomainEvent, DomainEventPayload, HeldOutGateEvaluated, Landed};
    use crate::lifecycle::{Attempt, Lifecycle};
    use crate::redaction::PatternRedactor;
    use crate::store::{
        LifecycleFilter, SpendSummary, SpendWindow, TelemetryRecord, TelemetrySink,
    };
    use crate::telemetry_file::FileTelemetrySink;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- A minimal real store: just the two sub-traits the readers use -----

    #[derive(Default)]
    struct TestStore {
        lifecycles: Mutex<HashMap<String, Lifecycle>>,
        events: Mutex<Vec<DomainEvent>>,
        fail_reads: Mutex<bool>,
    }

    impl TestStore {
        fn with_running(run_id: &str) -> Arc<Self> {
            let store = Self::default();
            let mut lifecycle = Lifecycle::new("t");
            lifecycle.run_id = run_id.to_owned();
            lifecycle.status = Status::Running;
            store
                .lifecycles
                .lock()
                .unwrap()
                .insert(run_id.to_owned(), lifecycle);
            Arc::new(store)
        }

        fn set_status(&self, run_id: &str, status: Status) {
            let mut rows = self.lifecycles.lock().unwrap();
            rows.get_mut(run_id).expect("known run").status = status;
        }

        fn push_event(&self, event: DomainEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl LifecycleStore for TestStore {
        fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
            let mut rows = self.lifecycles.lock().unwrap();
            if rows.contains_key(&lifecycle.run_id) {
                return Err(StoreError::lifecycle_already_exists(&lifecycle.run_id));
            }
            rows.insert(lifecycle.run_id.clone(), lifecycle.clone());
            Ok(())
        }
        fn update_lifecycle(
            &self,
            lifecycle: &Lifecycle,
            expected_version: u64,
        ) -> Result<(), StoreError> {
            let mut rows = self.lifecycles.lock().unwrap();
            let row = rows
                .get_mut(&lifecycle.run_id)
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
            if *self.fail_reads.lock().unwrap() {
                return Err(StoreError::backend(io::Error::other("db locked")));
            }
            Ok(self.lifecycles.lock().unwrap().get(run_id).cloned())
        }
        fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
            let mut out: Vec<Lifecycle> = self
                .lifecycles
                .lock()
                .unwrap()
                .values()
                .filter(|l| filter.matches(l))
                .cloned()
                .collect();
            out.sort_by(|a, b| b.run_id.cmp(&a.run_id));
            Ok(out)
        }
        fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
            let mut summary = SpendSummary::default();
            for lifecycle in self.lifecycles.lock().unwrap().values() {
                for attempt in &lifecycle.attempts {
                    if window.includes(attempt.last_activity_at) {
                        summary.add_attempt(attempt);
                    }
                }
            }
            Ok(summary)
        }
    }

    impl DomainEventStore for TestStore {
        fn append_domain_event(
            &self,
            event: &DomainEvent,
            _expected_version: u64,
        ) -> Result<Lifecycle, StoreError> {
            let current = self.load_lifecycle(&event.run_id)?;
            let folded = crate::events::apply(current.as_ref(), event)?;
            self.lifecycles
                .lock()
                .unwrap()
                .insert(event.run_id.clone(), folded.clone());
            self.push_event(event.clone());
            Ok(folded)
        }
        fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
            if *self.fail_reads.lock().unwrap() {
                return Err(StoreError::backend(io::Error::other("db locked")));
            }
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

    // --- Fixtures -----------------------------------------------------------

    fn ts(text: &str) -> DateTime<Utc> {
        parse_timestamp(text).unwrap()
    }

    fn telemetry(
        run_id: &str,
        kind: &str,
        payload: Value,
        attempt: Option<u32>,
        iteration: Option<u32>,
    ) -> TelemetryRecord {
        let mut record = TelemetryRecord::new(run_id, ts("2026-06-11T12:00:00+00:00"), kind);
        record.payload = payload.as_object().unwrap().clone();
        record.attempt_number = attempt;
        record.iteration_number = iteration;
        record
    }

    fn append_event(sink: &FileTelemetrySink, run_id: &str, kind: &str) {
        sink.append_telemetry(&telemetry(run_id, kind, json!({"k": kind}), None, None))
            .unwrap();
    }

    fn append_sdk(sink: &FileTelemetrySink, run_id: &str, payload: Value) {
        let kind = payload["message_type"]
            .as_str()
            .unwrap_or("assistant")
            .to_owned();
        sink.append_telemetry(&telemetry(run_id, &kind, payload, Some(1), Some(1)))
            .unwrap();
    }

    fn kinds(records: &[AuditRecord]) -> Vec<String> {
        records
            .iter()
            .map(|r| match r {
                AuditRecord::Event(e) => e.kind.clone(),
                AuditRecord::SdkMessage(m) => format!("sdk:{}", m.message_type),
            })
            .collect()
    }

    fn collect<S: LifecycleStore + DomainEventStore + ?Sized>(
        run_id: &str,
        store: &S,
        options: &StreamOptions,
    ) -> Vec<AuditRecord> {
        stream(run_id, store, options)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn opts(logs_root: &Path) -> StreamOptions {
        StreamOptions {
            logs_root: Some(logs_root.to_path_buf()),
            ..StreamOptions::default()
        }
    }

    fn follow_opts(logs_root: &Path, poll_ms: u64) -> StreamOptions {
        StreamOptions {
            logs_root: Some(logs_root.to_path_buf()),
            follow: true,
            poll_interval: Duration::from_millis(poll_ms),
            redactor: None,
        }
    }

    /// The `run-cli` fixture the Python CLI reference outputs were captured
    /// against: three sink records, a mirrored domain line, a line with no
    /// coordinates, a hand-written line, a malformed line, and a partial
    /// trailing line.
    const SECRET: &str = "sk-ant-abcdef0123456789ABCDEF";
    const ENV_SECRET: &str = "unusual-test-shaped-key-value-1234";

    fn write_cli_fixture(logs_root: &Path, run_id: &str, trailing: bool) {
        let sink = FileTelemetrySink::new(logs_root);
        sink.append_telemetry(&telemetry(
            run_id,
            "harness.iteration_start",
            json!({"attempt": 1, "iteration": 1}),
            Some(1),
            None,
        ))
        .unwrap();
        let mut assistant = telemetry(
            run_id,
            "assistant",
            json!({
                "message_type": "assistant",
                "text": format!("key is {SECRET} \u{e9}"),
                "tool_input": {"command": format!("echo {SECRET}")},
                "content": [{"type": "tool_use", "name": "Read", "input": {"file_path": "/home/me/.env"}}],
                "big": "X".repeat(200),
            }),
            Some(1),
            Some(1),
        );
        assistant.ts = ts("2026-06-11T12:00:01.123456+00:00");
        sink.append_telemetry(&assistant).unwrap();
        sink.append_telemetry(&telemetry(
            run_id,
            "domain.transitioned_to",
            json!({"target": "validating"}),
            Some(1),
            None,
        ))
        .unwrap();
        sink.append_telemetry(&telemetry(
            run_id,
            "harness.iteration_end",
            json!({"attempt": 1, "iteration": 1, "cost": 0.0123}),
            Some(1),
            None,
        ))
        .unwrap();
        sink.append_telemetry(&telemetry(
            run_id,
            "result",
            json!({"message_type": "result", "ok": true}),
            None,
            None,
        ))
        .unwrap();
        drop(sink);
        if trailing {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(run_file_path(logs_root, run_id))
                .unwrap();
            write!(
                file,
                "{{\"kind\": \"assistant\", \"run_id\": \"{run_id}\", \"ts\": \"2026-06-11T12:00:00+00:00\", \"payload\": {{\"text\": \"env {ENV_SECRET} here\"}}, \"attempt_number\": 1, \"iteration_number\": 2}}\nnot json\n{{\"kind\": \"harness.partial\", \"run_id\": \"{run_id}\""
            )
            .unwrap();
        }
    }

    /// Captured from `python -m flywheel_core.audit` against the fixture
    /// above (default text mode, redaction on, ANTHROPIC_API_KEY seeded).
    const PY_TEXT: &str = "2026-06-11T12:00:00+00:00 seq=1 attempt=1 iter=- kind=event:harness.iteration_start | {\"attempt\":1,\"iteration\":1}\n2026-06-11T12:00:01.123456+00:00 seq=2 attempt=1 iter=1 kind=sdk:assistant | {\"big\":\"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX... (use --json for full payload)\n2026-06-11T12:00:00+00:00 seq=4 attempt=1 iter=- kind=event:harness.iteration_end | {\"attempt\":1,\"cost\":0.0123,\"iteration\":1}\n2026-06-11T12:00:00+00:00 seq=5 attempt=0 iter=0 kind=sdk:result | {\"message_type\":\"result\",\"ok\":true}\n2026-06-11T12:00:00+00:00 seq=6 attempt=1 iter=2 kind=sdk:assistant | {\"text\":\"env [REDACTED:ANTHROPIC_API_KEY] here\"}\n";
    const PY_NOTICE: &str = "redaction: default policy applied (use --raw for verbatim)\nnote: redaction is best-effort; unmatched secrets may pass through; the store remains the unredacted source of truth\n";
    const PY_JSON_LINE_2_STRICT: &str = "{\"attempt_number\": 1, \"iteration_number\": 1, \"kind_or_message_type\": \"assistant\", \"payload\": {\"big\": \"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX\", \"content\": [{\"input\": \"[REDACTED:tool:Read]\", \"name\": \"Read\", \"type\": \"tool_use\"}], \"message_type\": \"assistant\", \"text\": \"key is [REDACTED:anthropic_key] \\u00e9\", \"tool_input\": {\"command\": \"echo [REDACTED:anthropic_key]\"}}, \"run_id\": \"run-cli\", \"sequence\": 2, \"ts\": \"2026-06-11T12:00:01.123456+00:00\"}";
    const PY_JSON_LINE_1: &str = "{\"attempt_number\": 1, \"iteration_number\": null, \"kind_or_message_type\": \"harness.iteration_start\", \"payload\": {\"attempt\": 1, \"iteration\": 1}, \"run_id\": \"run-cli\", \"sequence\": 1, \"ts\": \"2026-06-11T12:00:00+00:00\"}";
    const PY_JSON_LINE_5: &str = "{\"attempt_number\": 0, \"iteration_number\": 0, \"kind_or_message_type\": \"result\", \"payload\": {\"message_type\": \"result\", \"ok\": true}, \"run_id\": \"run-cli\", \"sequence\": 5, \"ts\": \"2026-06-11T12:00:00+00:00\"}";
    const PY_DRY_RUN_STRICT_SALTED: &str = "dry-run coverage report\npolicy: strict\nrecords scanned: 5\nrecords redacted: 2\ntotal redactions: 4\nhits by label:\n  anthropic_key: 2\n  ANTHROPIC_API_KEY: 1\n  tool:Read: 1\n";

    /// `run_audit` with the CLI's `ANTHROPIC_API_KEY` seed supplied through
    /// a registered policy instead of the process environment, so the test
    /// is hermetic. The policy is the exact composition `build_named_policy`
    /// produces when the variable is set.
    fn seeded_policy(name: &'static str) -> PolicyFactory {
        Arc::new(move |salt: Option<&[u8]>| {
            let base: Box<dyn Redactor> = match name {
                "default" => Box::new(default_policy(salt)),
                _ => Box::new(strict_policy(salt)),
            };
            let env = EnvValueRedactor::with_lookup(
                &CLI_ENV_SEEDS,
                &crate::redaction::EnvValueOptions {
                    salt: salt.map(<[u8]>::to_vec),
                    ..Default::default()
                },
                |_| Some(ENV_SECRET.to_owned()),
            )
            .unwrap();
            Ok(Box::new(compose(vec![base, Box::new(env)])) as Box<dyn Redactor>)
        })
    }

    fn cli(args: &AuditArgs, store: &TestStore, logs_root: &Path) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_audit(args, store, logs_root, &mut out, &mut err).unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn seeded_args(run_id: &str, policy: &'static str) -> AuditArgs {
        let mut args = AuditArgs::new(run_id);
        args.redact_policy = Some(format!("seeded-{policy}"));
        args.custom_policies = vec![(format!("seeded-{policy}"), seeded_policy(policy))];
        args
    }

    // --- File reader --------------------------------------------------------

    #[test]
    fn reader_withholds_partial_trailing_line_until_completed() {
        let dir = tempfile::tempdir().unwrap();
        let path = run_file_path(dir.path(), "r1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"kind\": \"harness.one\", \"run_id\": \"r1\", \"ts\": \"2026-06-11T00:00:00+00:00\", \"payload\": {}}\n{\"kind\": \"harness.two\", \"run_id\": \"r1\"",
        )
        .unwrap();
        let (records, cursor) = read_records_since(&path, FileCursor::default(), false).unwrap();
        assert_eq!(kinds(&records), vec!["harness.one"]);
        assert_eq!(cursor.line, 1);
        let (again, cursor2) = read_records_since(&path, cursor, false).unwrap();
        assert!(again.is_empty());
        assert_eq!(cursor2, cursor);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b", \"ts\": \"2026-06-11T00:00:01+00:00\", \"payload\": {}}\n")
            .unwrap();
        let (completed, cursor3) = read_records_since(&path, cursor2, false).unwrap();
        assert_eq!(kinds(&completed), vec!["harness.two"]);
        assert_eq!(cursor3.line, 2);
        assert_eq!(cursor3.offset, std::fs::metadata(&path).unwrap().len());
        assert_eq!(completed[0].sequence(), Some(2));
        assert_eq!(completed[0].ts(), ts("2026-06-11T00:00:01+00:00"));
    }

    #[test]
    fn reader_discards_partial_line_on_eof_final() {
        let dir = tempfile::tempdir().unwrap();
        let path = run_file_path(dir.path(), "r1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"kind\": \"harness.one\", \"run_id\": \"r1\", \"payload\": {}}\n{\"kind\": \"harness.tru").unwrap();
        let (records, cursor) = read_records_since(&path, FileCursor::default(), true).unwrap();
        assert_eq!(kinds(&records), vec!["harness.one"]);
        assert_eq!(cursor.offset, std::fs::metadata(&path).unwrap().len());
        assert_eq!(cursor.line, 1);
        let (again, cursor2) = read_records_since(&path, cursor, true).unwrap();
        assert!(again.is_empty());
        assert_eq!(cursor2, cursor);
        // Missing ts falls back to the epoch.
        assert_eq!(records[0].ts(), DateTime::UNIX_EPOCH);
    }

    #[test]
    fn reader_keeps_handle_across_reads_and_tolerates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = run_file_path(dir.path(), "r1");
        let mut reader = RunFileReader::new(&path, FileCursor::default());
        assert_eq!(reader.next_record(false).unwrap(), None);
        assert_eq!(reader.cursor(), FileCursor::default());
        assert_eq!(reader.path(), path.as_path());
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.a");
        assert_eq!(
            kinds(&[reader.next_record(false).unwrap().unwrap()]),
            vec!["harness.a"]
        );
        assert_eq!(reader.next_record(false).unwrap(), None);
        append_event(&sink, "r1", "harness.b");
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "tool_use", "name": "Read"}),
        );
        assert_eq!(
            kinds(&[reader.next_record(false).unwrap().unwrap()]),
            vec!["harness.b"]
        );
        assert_eq!(
            kinds(&[reader.next_record(false).unwrap().unwrap()]),
            vec!["sdk:tool_use"]
        );
        assert_eq!(reader.cursor().line, 3);
        let (resumed, _) =
            read_records_since(&path, FileCursor { offset: 0, line: 0 }, false).unwrap();
        assert_eq!(resumed.len(), 3);
    }

    #[test]
    fn record_from_line_classifies_and_skips() {
        let event = record_from_line(br#"{"kind": "harness.x", "run_id": "r", "ts": "2026-01-01T00:00:00+00:00", "attempt_number": 3, "iteration_number": 9, "payload": {"a": 1}}"#, 4).unwrap();
        assert_eq!(
            event,
            AuditRecord::Event(EventRecord {
                run_id: "r".into(),
                ts: ts("2026-01-01T00:00:00+00:00"),
                kind: "harness.x".into(),
                payload: json!({"a": 1}).as_object().unwrap().clone(),
                attempt_number: Some(3),
                id: None,
                sequence: Some(4),
            })
        );
        let sdk = record_from_line(br#"{"kind": "assistant", "payload": "not an object", "attempt_number": true, "iteration_number": -1, "ts": "garbage"}"#, 5).unwrap();
        assert_eq!(
            sdk,
            AuditRecord::SdkMessage(SdkMessageRecord {
                run_id: String::new(),
                attempt_number: 0,
                iteration_number: 0,
                message_type: "assistant".into(),
                payload: Map::new(),
                ts: DateTime::UNIX_EPOCH,
                sequence: Some(5),
                id: None,
            })
        );
        assert_eq!(
            record_from_line(br#"{"kind": "domain.landed", "payload": {}}"#, 1),
            None
        );
        assert_eq!(record_from_line(br#"{"kind": "", "payload": {}}"#, 1), None);
        assert_eq!(record_from_line(br#"{"payload": {}}"#, 1), None);
        assert_eq!(record_from_line(br#"[1, 2]"#, 1), None);
        assert_eq!(record_from_line(b"not json", 1), None);
        assert_eq!(record_from_line(b"\xff\xfe", 1), None);
    }

    // --- stream: replay -----------------------------------------------------

    #[test]
    fn stream_drains_mixed_records_in_sequence_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.iteration_start");
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "assistant", "text": "hi"}),
        );
        append_event(&sink, "r1", "harness.envelope_observed");
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "tool_use", "name": "Read"}),
        );
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "tool_result", "content": "ok"}),
        );
        append_event(&sink, "r1", "harness.iteration_end");

        let collected = collect("r1", &*store, &opts(dir.path()));
        assert_eq!(collected.len(), 6);
        let sequences: Vec<Option<u64>> = collected.iter().map(AuditRecord::sequence).collect();
        assert_eq!(sequences, (1..=6).map(Some).collect::<Vec<_>>());
        let variants: Vec<bool> = collected
            .iter()
            .map(|r| matches!(r, AuditRecord::Event(_)))
            .collect();
        assert_eq!(variants, vec![true, false, true, false, false, true]);
    }

    #[test]
    fn stream_unknown_run_and_missing_file_yield_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let empty = TestStore::default();
        assert!(collect("does-not-exist", &empty, &opts(dir.path())).is_empty());
        assert!(collect("nope", &empty, &follow_opts(dir.path(), 5000)).is_empty());
        let store = TestStore::with_running("r1");
        assert!(collect("r1", &*store, &opts(dir.path())).is_empty());
        let mut lazy = stream("r1", &*store, &opts(dir.path()));
        assert!(lazy.next().is_none());
        assert_eq!(lazy.run_id(), "r1");
        assert_eq!(lazy.cursor(), FileCursor::default());
    }

    #[test]
    fn stream_applies_redactor_at_yield_seam_and_is_lazy() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "assistant", "text": "sk-ant-AAA"}),
        );
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "assistant", "text": "sk-ant-BBB"}),
        );
        let options = StreamOptions {
            redactor: Some(Arc::new(
                PatternRedactor::new(&[("anthropic_key", r"sk-ant-[A-Z]+")], None).unwrap(),
            )),
            ..opts(dir.path())
        };
        let mut lazy = stream("r1", &*store, &options);
        let first = lazy.next().unwrap().unwrap();
        assert_eq!(first.sequence(), Some(1));
        assert_eq!(lazy.cursor().line, 1);
        let AuditRecord::SdkMessage(message) = first else {
            panic!("expected sdk message");
        };
        assert_eq!(message.payload["text"], "[REDACTED:anthropic_key]");
        assert_eq!(lazy.next().unwrap().unwrap().sequence(), Some(2));
        assert!(lazy.next().is_none());
        assert!(lazy.next().is_none());
    }

    #[test]
    fn stream_projects_landing_stage_ledger_at_tail() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.iteration_start");
        append_sdk(&sink, "r1", json!({"message_type": "assistant"}));
        store.push_event(DomainEvent::new(
            "r1",
            ts("2026-06-11T13:00:00+00:00"),
            DomainEventPayload::TransitionedTo(crate::events::TransitionedTo {
                target: Status::Done,
                error: String::new(),
            }),
        ));
        store.push_event(
            DomainEvent::new(
                "r1",
                ts("2026-06-11T13:00:01+00:00"),
                DomainEventPayload::HeldOutGateEvaluated(HeldOutGateEvaluated {
                    outcome: "pass".into(),
                    reason: "all green".into(),
                    receipts: vec![],
                }),
            )
            .with_attempt_number(1),
        );
        store.push_event(DomainEvent::new(
            "r1",
            ts("2026-06-11T13:00:02+00:00"),
            DomainEventPayload::Landed(Landed {
                strategy: "merge".into(),
                landed_ref: "abc123".into(),
                ..Landed::default()
            }),
        ));
        let collected = collect("r1", &*store, &opts(dir.path()));
        assert_eq!(
            kinds(&collected),
            vec![
                "harness.iteration_start",
                "sdk:assistant",
                "domain.held_out_gate_evaluated",
                "domain.landed",
            ]
        );
        let sequences: Vec<Option<u64>> = collected.iter().map(AuditRecord::sequence).collect();
        assert_eq!(sequences, vec![Some(1), Some(2), Some(3), Some(4)]);
        let AuditRecord::Event(gate) = &collected[2] else {
            panic!("expected event");
        };
        assert_eq!(gate.attempt_number, Some(1));
        assert_eq!(gate.ts, ts("2026-06-11T13:00:01+00:00"));
        assert_eq!(
            Value::Object(gate.payload.clone()),
            json!({"outcome": "pass", "reason": "all green", "receipts": []})
        );
        let AuditRecord::Event(landed) = &collected[3] else {
            panic!("expected event");
        };
        assert_eq!(
            Value::Object(landed.payload.clone()),
            json!({"strategy": "merge", "landed_ref": "abc123"})
        );
        assert_eq!(landed.id, None);

        // A ledger read failure yields no tail and does not break the stream.
        *store.fail_reads.lock().unwrap() = true;
        let tail = landing_stage_records(&*store, "r1", 2);
        assert!(tail.is_empty());
        *store.fail_reads.lock().unwrap() = false;
        assert!(landing_stage_records(&*store, "other", 0).is_empty());
    }

    #[test]
    fn stream_surfaces_store_failures_then_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        *store.fail_reads.lock().unwrap() = true;
        let mut lazy = stream("r1", &*store, &follow_opts(dir.path(), 1));
        let err = lazy.next().unwrap().unwrap_err();
        assert!(matches!(err, AuditError::Store(_)));
        assert_eq!(err.to_string(), "store backend: db locked");
        assert!(lazy.next().is_none());
    }

    // --- stream: follow -----------------------------------------------------

    #[test]
    fn stream_follow_observes_concurrent_writes_and_exits_on_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let logs_root = dir.path().to_path_buf();
        let consumer = {
            let store = Arc::clone(&store);
            thread::spawn(move || collect("r1", &*store, &follow_opts(&logs_root, 10)))
        };
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.iteration_start");
        thread::sleep(Duration::from_millis(40));
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "assistant", "text": "ok"}),
        );
        thread::sleep(Duration::from_millis(40));
        append_event(&sink, "r1", "harness.iteration_end");
        thread::sleep(Duration::from_millis(40));
        store.set_status("r1", Status::Done);
        let collected = consumer.join().unwrap();
        let sequences: Vec<Option<u64>> = collected.iter().map(AuditRecord::sequence).collect();
        assert_eq!(sequences, vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn stream_follow_against_terminal_run_drains_without_sleeping() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.iteration_start");
        append_event(&sink, "r1", "harness.iteration_end");
        store.set_status("r1", Status::Failed);
        let start = Instant::now();
        let collected = collect("r1", &*store, &follow_opts(dir.path(), 5000));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn stream_follow_does_not_drop_final_write_after_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.first");
        let logs_root = dir.path().to_path_buf();
        let consumer = {
            let store = Arc::clone(&store);
            thread::spawn(move || collect("r1", &*store, &follow_opts(&logs_root, 50)))
        };
        thread::sleep(Duration::from_millis(100));
        store.set_status("r1", Status::Interrupted);
        append_event(&sink, "r1", "harness.last");
        // A partial trailing line is discarded on the final pass.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(run_file_path(dir.path(), "r1"))
            .unwrap();
        file.write_all(b"{\"kind\": \"harness.tru").unwrap();
        let collected = consumer.join().unwrap();
        assert_eq!(kinds(&collected), vec!["harness.first", "harness.last"]);
    }

    #[test]
    fn stream_follow_keeps_a_write_landing_just_after_terminal() {
        // The writer is descheduled between the terminal transition and its
        // last append (simulated by a delay shorter than the settle window):
        // the follow must still deliver that append.
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.first");
        let logs_root = dir.path().to_path_buf();
        let consumer = {
            let store = Arc::clone(&store);
            thread::spawn(move || collect("r1", &*store, &follow_opts(&logs_root, 10)))
        };
        thread::sleep(Duration::from_millis(50));
        store.set_status("r1", Status::Done);
        thread::sleep(FOLLOW_SETTLE_WINDOW / 2);
        append_event(&sink, "r1", "harness.late");
        let collected = consumer.join().unwrap();
        assert_eq!(kinds(&collected), vec!["harness.first", "harness.late"]);
    }

    #[test]
    fn stream_follow_waits_for_file_to_appear() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let logs_root = dir.path().to_path_buf();
        let consumer = {
            let store = Arc::clone(&store);
            thread::spawn(move || collect("r1", &*store, &follow_opts(&logs_root, 10)))
        };
        thread::sleep(Duration::from_millis(60));
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.appeared");
        drop(sink);
        thread::sleep(Duration::from_millis(60));
        store.set_status("r1", Status::InternalError);
        let collected = consumer.join().unwrap();
        assert_eq!(kinds(&collected), vec!["harness.appeared"]);
    }

    #[test]
    fn stream_follow_stops_promptly_on_stop_flag() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let stop = StopFlag::new();
        let logs_root = dir.path().to_path_buf();
        let consumer = {
            let store = Arc::clone(&store);
            let stop = stop.clone();
            thread::spawn(move || {
                stream("r1", &*store, &follow_opts(&logs_root, 5000))
                    .with_stop(stop)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            })
        };
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        assert!(!stop.is_set());
        stop.set();
        assert!(consumer.join().unwrap().is_empty());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(stop.is_set());
        assert!(stop.wait(Duration::from_millis(1)));
        assert!(!StopFlag::new().wait(Duration::from_millis(1)));
    }

    // --- subscribe / attach_logger ------------------------------------------

    fn wait_until(deadline: Duration, condition: impl Fn() -> bool) {
        let start = Instant::now();
        while !condition() && start.elapsed() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn attach_logger_emits_one_entry_per_record_with_label_and_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.iteration_start");
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "assistant", "text": "ok"}),
        );
        append_event(&sink, "r1", "harness.iteration_end");
        type Entries = Arc<Mutex<Vec<(String, u64, String, Value)>>>;
        let entries: Entries = Arc::default();
        let sink_entries = Arc::clone(&entries);
        let mut handle = attach_logger(
            move |entry: &AuditLogEntry<'_>| {
                sink_entries.lock().unwrap().push((
                    entry.message.clone(),
                    entry.sequence,
                    entry.kind.clone(),
                    entry.record_as_json(),
                ));
            },
            "r1",
            Arc::clone(&store),
            &follow_opts(dir.path(), 10),
        );
        wait_until(Duration::from_secs(2), || {
            entries.lock().unwrap().len() >= 3
        });
        assert!(handle.is_alive());
        assert_eq!(handle.run_id(), "r1");
        handle.detach();
        assert!(!handle.is_alive());
        let entries = entries.lock().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().map(|e| e.1).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            entries[0].0,
            "audit run=r1 seq=1 kind=event:harness.iteration_start"
        );
        assert_eq!(entries[1].0, "audit run=r1 seq=2 kind=sdk:assistant");
        assert_eq!(entries[1].2, "sdk:assistant");
        assert_eq!(entries[1].3["payload"]["text"], "ok");
        assert_eq!(entries[1].3["sequence"], 2);
        assert_eq!(entries[0].3["kind"], "harness.iteration_start");
    }

    #[test]
    fn attach_logger_twice_returns_distinct_handles() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let counter = Arc::new(AtomicUsize::new(0));
        let make = |counter: Arc<AtomicUsize>| {
            move |_: &AuditLogEntry<'_>| {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        };
        let mut h1 = attach_logger(
            make(Arc::clone(&counter)),
            "r1",
            Arc::clone(&store),
            &follow_opts(dir.path(), 20),
        );
        let mut h2 = attach_logger(
            make(Arc::clone(&counter)),
            "r1",
            Arc::clone(&store),
            &follow_opts(dir.path(), 20),
        );
        assert!(h1.is_alive() && h2.is_alive());
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "r1", "harness.x");
        wait_until(Duration::from_secs(2), || {
            counter.load(Ordering::SeqCst) >= 2
        });
        h1.detach();
        h2.detach();
        h1.detach();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn detach_stops_emission_and_joins_even_mid_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        for i in 0..50 {
            append_event(&sink, "r1", &format!("harness.e{i}"));
        }
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let mut handle = attach_logger(
            move |_: &AuditLogEntry<'_>| {
                counter.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(2));
            },
            "r1",
            Arc::clone(&store),
            &follow_opts(dir.path(), 10),
        );
        wait_until(Duration::from_secs(2), || seen.load(Ordering::SeqCst) >= 1);
        let start = Instant::now();
        handle.detach();
        assert!(start.elapsed() < Duration::from_millis(1500));
        assert!(!handle.is_alive());
        let before = seen.load(Ordering::SeqCst);
        append_event(&sink, "r1", "harness.late");
        thread::sleep(Duration::from_millis(60));
        assert_eq!(seen.load(Ordering::SeqCst), before);
        assert!(before < 50);
    }

    #[test]
    fn subscription_isolates_failing_and_panicking_handlers() {
        struct Flaky {
            seen: Arc<Mutex<Vec<u64>>>,
            errors: Arc<Mutex<Vec<(u64, String)>>>,
        }
        impl EventHandler for Flaky {
            fn on_record(&mut self, record: &AuditRecord) -> Result<(), HandlerError> {
                let seq = record.sequence().unwrap_or(0);
                self.seen.lock().unwrap().push(seq);
                match seq {
                    2 => Err("boom".into()),
                    3 => panic!("kaboom {seq}"),
                    _ => Ok(()),
                }
            }
            fn on_error(&mut self, run_id: &str, record: &AuditRecord, error: &HandlerError) {
                assert_eq!(run_id, "r1");
                self.errors
                    .lock()
                    .unwrap()
                    .push((record.sequence().unwrap_or(0), error.to_string()));
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        for i in 0..4 {
            append_event(&sink, "r1", &format!("harness.e{i}"));
        }
        let seen: Arc<Mutex<Vec<u64>>> = Arc::default();
        let errors: Arc<Mutex<Vec<(u64, String)>>> = Arc::default();
        let handler = Flaky {
            seen: Arc::clone(&seen),
            errors: Arc::clone(&errors),
        };
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut subscription = subscribe(
            handler,
            "r1",
            Arc::clone(&store),
            &follow_opts(dir.path(), 10),
        );
        wait_until(Duration::from_secs(2), || seen.lock().unwrap().len() >= 4);
        store.set_status("r1", Status::Done);
        wait_until(Duration::from_secs(2), || !subscription.is_alive());
        std::panic::set_hook(previous_hook);
        assert!(!subscription.is_alive());
        subscription.unsubscribe();
        assert_eq!(subscription.run_id(), "r1");
        assert_eq!(*seen.lock().unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(
            *errors.lock().unwrap(),
            vec![(2, "boom".to_owned()), (3, "kaboom 3".to_owned())]
        );
    }

    #[test]
    fn subscription_applies_redactor_and_reports_stream_errors() {
        struct Recorder {
            texts: Arc<Mutex<Vec<String>>>,
            stream_errors: Arc<Mutex<Vec<String>>>,
        }
        impl EventHandler for Recorder {
            fn on_record(&mut self, record: &AuditRecord) -> Result<(), HandlerError> {
                if let AuditRecord::SdkMessage(message) = record {
                    self.texts
                        .lock()
                        .unwrap()
                        .push(message.payload["text"].as_str().unwrap_or("").to_owned());
                }
                Ok(())
            }
            fn on_stream_error(&mut self, _run_id: &str, error: &AuditError) {
                self.stream_errors.lock().unwrap().push(error.to_string());
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("r1");
        let sink = FileTelemetrySink::new(dir.path());
        append_sdk(
            &sink,
            "r1",
            json!({"message_type": "assistant", "text": "sk-ant-AAA"}),
        );
        let texts: Arc<Mutex<Vec<String>>> = Arc::default();
        let stream_errors: Arc<Mutex<Vec<String>>> = Arc::default();
        let options = StreamOptions {
            redactor: Some(Arc::new(
                PatternRedactor::new(&[("anthropic_key", r"sk-ant-[A-Z]+")], None).unwrap(),
            )),
            ..follow_opts(dir.path(), 10)
        };
        let subscription = subscribe(
            Recorder {
                texts: Arc::clone(&texts),
                stream_errors: Arc::clone(&stream_errors),
            },
            "r1",
            Arc::clone(&store),
            &options,
        );
        wait_until(Duration::from_secs(2), || !texts.lock().unwrap().is_empty());
        assert_eq!(*texts.lock().unwrap(), vec!["[REDACTED:anthropic_key]"]);
        *store.fail_reads.lock().unwrap() = true;
        wait_until(Duration::from_secs(2), || !subscription.is_alive());
        assert_eq!(
            *stream_errors.lock().unwrap(),
            vec!["store backend: db locked"]
        );
        // Dropping a live subscription signals stop without blocking.
        *store.fail_reads.lock().unwrap() = false;
        let live = subscribe(
            |_: &AuditRecord| Ok(()),
            "r1",
            Arc::clone(&store),
            &follow_opts(dir.path(), 5000),
        );
        let flag = live.stop.clone();
        drop(live);
        assert!(flag.is_set());
    }

    // --- CLI: byte parity with the Python CLI --------------------------------

    #[test]
    fn cli_text_mode_matches_python_output_with_redaction_notice() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("run-cli");
        write_cli_fixture(dir.path(), "run-cli", true);
        let args = seeded_args("run-cli", "default");
        let (code, out, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 0);
        assert_eq!(out, PY_TEXT);
        assert_eq!(
            err,
            PY_NOTICE.replace("default policy", "seeded-default policy")
        );
    }

    #[test]
    fn cli_json_mode_matches_python_output_for_default_strict_and_salt() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("run-cli");
        write_cli_fixture(dir.path(), "run-cli", true);

        let mut args = seeded_args("run-cli", "strict");
        args.json = true;
        let (code, out, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], PY_JSON_LINE_1);
        assert_eq!(lines[1], PY_JSON_LINE_2_STRICT);
        assert_eq!(lines[3], PY_JSON_LINE_5);
        assert_eq!(
            lines[4],
            "{\"attempt_number\": 1, \"iteration_number\": 2, \"kind_or_message_type\": \"assistant\", \"payload\": {\"text\": \"env [REDACTED:ANTHROPIC_API_KEY] here\"}, \"run_id\": \"run-cli\", \"sequence\": 6, \"ts\": \"2026-06-11T12:00:00+00:00\"}"
        );
        assert_eq!(
            err,
            PY_NOTICE.replace("default policy", "seeded-strict policy")
        );

        let mut args = seeded_args("run-cli", "default");
        args.json = true;
        let (_, out, _) = cli(&args, &store, dir.path());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[1],
            PY_JSON_LINE_2_STRICT.replace(
                "\"[REDACTED:tool:Read]\"",
                "{\"file_path\": \"/home/me/.env\"}"
            )
        );
        assert!(!out.contains(SECRET));
        assert!(!out.contains(ENV_SECRET));

        let mut args = seeded_args("run-cli", "default");
        args.json = true;
        args.redact_salt = Some("test-salt".to_owned());
        let (_, out, _) = cli(&args, &store, dir.path());
        assert!(out.contains("\"text\": \"key is [REDACTED:anthropic_key:f5f92d03] \\u00e9\""));
        assert!(out.contains("\"command\": \"echo [REDACTED:anthropic_key:f5f92d03]\""));
        assert!(out.contains("env [REDACTED:ANTHROPIC_API_KEY:0b67e6b5] here"));
    }

    #[test]
    fn cli_raw_is_verbatim_without_notice_and_follow_drains_terminal_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("run-cli");
        write_cli_fixture(dir.path(), "run-cli", true);
        let mut args = AuditArgs::new("run-cli");
        args.raw = true;
        let (code, out, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 0);
        assert_eq!(err, "");
        assert_eq!(
            out,
            PY_TEXT.replace("[REDACTED:ANTHROPIC_API_KEY]", ENV_SECRET)
        );
        args.json = true;
        let (_, out, _) = cli(&args, &store, dir.path());
        assert!(out.contains(&format!("\"text\": \"key is {SECRET} \\u00e9\"")));
        assert!(!out.contains("[REDACTED:"));

        store.set_status("run-cli", Status::Done);
        let mut args = seeded_args("run-cli", "default");
        args.follow = true;
        args.poll_interval = Duration::from_secs(5);
        let start = Instant::now();
        let (code, out, _) = cli(&args, &store, dir.path());
        assert_eq!(code, 0);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(out, PY_TEXT);
    }

    #[test]
    fn cli_dry_run_matches_python_coverage_report() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("run-cli");
        write_cli_fixture(dir.path(), "run-cli", true);
        let mut args = seeded_args("run-cli", "strict");
        args.dry_run = true;
        args.redact_salt = Some("s".to_owned());
        let (code, out, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 0);
        assert_eq!(
            out,
            PY_DRY_RUN_STRICT_SALTED.replace("policy: strict", "policy: seeded-strict")
        );
        assert!(err.starts_with("redaction: seeded-strict policy applied"));
        assert!(!out.contains(SECRET));

        let mut args = seeded_args("run-cli", "default");
        args.dry_run = true;
        let (_, out, _) = cli(&args, &store, dir.path());
        assert_eq!(
            out,
            "dry-run coverage report\npolicy: seeded-default\nrecords scanned: 5\nrecords redacted: 2\ntotal redactions: 3\nhits by label:\n  anthropic_key: 2\n  ANTHROPIC_API_KEY: 1\n"
        );

        let store = TestStore::with_running("run-benign");
        let sink = FileTelemetrySink::new(dir.path());
        append_event(&sink, "run-benign", "harness.iteration_start");
        append_sdk(
            &sink,
            "run-benign",
            json!({"message_type": "assistant", "text": "hello world"}),
        );
        let mut args = AuditArgs::new("run-benign");
        args.dry_run = true;
        let (code, out, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "dry-run coverage report\npolicy: default\nrecords scanned: 2\nrecords redacted: 0\ntotal redactions: 0\nhits by label: (none)\n"
        );
        assert_eq!(err, PY_NOTICE);
    }

    #[test]
    fn cli_unknown_run_prints_empty_state_without_notice() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::default();
        let (code, out, err) = cli(&AuditArgs::new("missing-run"), &store, dir.path());
        assert_eq!(code, 0);
        assert_eq!(out, "");
        assert_eq!(err, "no records for run_id missing-run\n");
    }

    #[test]
    fn cli_rejects_raw_combined_with_redaction_flags() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("run-cli");
        let mut args = AuditArgs::new("run-cli");
        args.raw = true;
        args.dry_run = true;
        args.redact_salt = Some("x".to_owned());
        let (code, out, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 2);
        assert_eq!(out, "");
        assert_eq!(
            err,
            "error: --raw cannot be combined with --dry-run, --redact-salt (redaction is either fully off or fully on)\n"
        );
        let mut args = AuditArgs::new("run-cli");
        args.raw = true;
        args.redact = true;
        args.redact_policy = Some("strict".to_owned());
        assert_eq!(
            validate_flag_combinations(&args).unwrap(),
            "--raw cannot be combined with --redact, --redact-policy (redaction is either fully off or fully on)"
        );
        args.raw = false;
        assert_eq!(validate_flag_combinations(&args), None);
    }

    #[test]
    fn cli_policy_resolution_fails_fast_before_streaming() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("run-cli");
        write_cli_fixture(dir.path(), "run-cli", false);
        let mut args = AuditArgs::new("run-cli");
        args.redact_policy = Some("nope".to_owned());
        let (code, out, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 2);
        assert_eq!(out, "");
        assert_eq!(
            err,
            "error: unknown built-in policy 'nope'; expected one of ['default', 'strict'] or a registered custom policy\n"
        );

        let failing: PolicyFactory = Arc::new(|_| Err("TypeError: missing salt".to_owned()));
        args.redact_policy = Some("it's:broken".to_owned());
        args.custom_policies = vec![("it's:broken".to_owned(), failing)];
        let (code, _, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 2);
        assert_eq!(
            err,
            "error: --redact-policy \"it's:broken\" factory raised: TypeError: missing salt\n"
        );
        args.redact_policy = Some("other".to_owned());
        let (code, _, err) = cli(&args, &store, dir.path());
        assert_eq!(code, 2);
        assert_eq!(
            err,
            "error: unknown built-in policy 'other'; expected one of ['default', 'strict', \"it's:broken\"] or a registered custom policy\n"
        );

        // A Python dotted-path factory is rejected with the supported names.
        let mut dotted = AuditArgs::new("run-cli");
        dotted.redact_policy = Some("mypkg.redact:build".to_owned());
        let (code, out, err) = cli(&dotted, &store, dir.path());
        assert_eq!(code, 2);
        assert_eq!(out, "");
        assert_eq!(
            err,
            "error: --redact-policy 'mypkg.redact:build': dotted-path policy factories \
             ('module.sub:factory') are not supported; expected one of ['default', 'strict']\n"
        );

        // Built-in names resolve even when custom policies are registered.
        let mut args = AuditArgs::new("run-cli");
        args.redact_policy = Some("strict".to_owned());
        let (redactor, label) = resolve_policy(&args).unwrap();
        assert_eq!(label, "strict");
        let out = redactor.redact(&AuditRecord::SdkMessage(SdkMessageRecord {
            run_id: "r".into(),
            attempt_number: 1,
            iteration_number: 1,
            message_type: "assistant".into(),
            payload: json!({"content": [{"name": "Bash", "input": "cat .env"}]})
                .as_object()
                .unwrap()
                .clone(),
            ts: DateTime::UNIX_EPOCH,
            sequence: None,
            id: None,
        }));
        assert_eq!(
            record_payload(&out)["content"][0]["input"],
            "[REDACTED:tool:Bash]"
        );
        assert!(format!("{args:?}").contains("redact_policy: Some(\"strict\")"));
    }

    #[test]
    fn cli_interrupt_flag_exits_130_under_follow() {
        let dir = tempfile::tempdir().unwrap();
        let store = TestStore::with_running("run-cli");
        write_cli_fixture(dir.path(), "run-cli", false);
        let mut args = AuditArgs::new("run-cli");
        args.raw = true;
        args.follow = true;
        args.poll_interval = Duration::from_secs(5);
        let interrupt = StopFlag::new();
        args.interrupt = Some(interrupt.clone());
        let logs_root = dir.path().to_path_buf();
        let runner = thread::spawn(move || {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = run_audit(&args, &*store, &logs_root, &mut out, &mut err).unwrap();
            (code, String::from_utf8(out).unwrap())
        });
        thread::sleep(Duration::from_millis(100));
        interrupt.set();
        let (code, out) = runner.join().unwrap();
        assert_eq!(code, EXIT_INTERRUPTED);
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn cli_path_helpers_and_preview_budget() {
        assert_eq!(resolve_db_path(Some("x.db")), PathBuf::from("x.db"));
        assert_eq!(
            logs_root_for_db(Path::new("/tmp/store/flywheel.sqlite")),
            PathBuf::from("/tmp/store/logs")
        );
        assert_eq!(
            logs_root_for_db(Path::new("flywheel.db")),
            PathBuf::from("logs")
        );
        let db = resolve_db_path(None);
        assert!(
            db == Path::new(DEFAULT_DB_PATH)
                || db == Path::new(&std::env::var("FLYWHEEL_DB").unwrap_or_default())
        );

        let huge = AuditRecord::SdkMessage(SdkMessageRecord {
            run_id: "r".into(),
            attempt_number: 1,
            iteration_number: 1,
            message_type: "assistant".into(),
            payload: json!({"content": "X".repeat(4096), "n": "\n\r"})
                .as_object()
                .unwrap()
                .clone(),
            ts: DateTime::UNIX_EPOCH,
            sequence: None,
            id: None,
        });
        let preview = payload_preview(&huge);
        assert!(preview.ends_with(TRUNCATION_HINT));
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS);
        assert_eq!(
            render_default(&huge),
            format!(
                "1970-01-01T00:00:00+00:00 seq=? attempt=1 iter=1 kind=sdk:assistant | {preview}"
            )
        );
        let json_line = render_json(&huge);
        assert!(json_line.contains(&"X".repeat(4096)));
        assert!(json_line.starts_with("{\"attempt_number\": 1, \"iteration_number\": 1, \"kind_or_message_type\": \"assistant\", \"payload\": {\"content\": "));
        assert!(json_line.ends_with(", \"n\": \"\\n\\r\"}, \"run_id\": \"r\", \"sequence\": null, \"ts\": \"1970-01-01T00:00:00+00:00\"}"));

        assert_eq!(strip_digest("tool:Read:0b67e6b5", true), "tool:Read");
        assert_eq!(
            strip_digest("tool:Read:0b67e6b5", false),
            "tool:Read:0b67e6b5"
        );
        assert_eq!(strip_digest("tool:Read", true), "tool:Read");
        assert_eq!(strip_digest("x:ABCDEF01", true), "x:ABCDEF01");
        assert_eq!(py_repr("plain"), "'plain'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c\\d\n"), "'a\\'b\"c\\\\d\\n'");

        let mut lifecycle = Lifecycle::new("t");
        lifecycle.attempts.push(Attempt::new(
            1,
            DateTime::UNIX_EPOCH,
            lifecycle.run_id.clone(),
        ));
        let store = TestStore::default();
        store.create_lifecycle(&lifecycle).unwrap();
        assert_eq!(
            store
                .list_lifecycles(&LifecycleFilter::default())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store.summarize_spend(&SpendWindow::UNBOUNDED).unwrap(),
            SpendSummary::default()
        );
    }
}
