//! Session telemetry: everything each agent session does, recorded and
//! folded into what it is doing now.
//!
//! Every tool call (start, finish, duration, outcome, paired by the
//! agent's call id), turn, subagent, declared plan, wait on a person,
//! context-window reading, token and cost total, and rate-limit reading is
//! one [`Event`] in an append-only log ([`store`]). A reducer
//! ([`reduce`]) folds each session's events into its state: the fleet
//! board's row ([`Snapshot`]) and the work tree behind it.
//!
//! Capture is local to the hub that owns the session ([`claude`]: Claude
//! Code's hooks over http and its status line; [`codex`]: the rollout the
//! hub already tails). A hub can also take other hubs' events
//! ([`Telemetry::ingest`], `POST /api/telemetry/batch`) and send its own
//! upstream ([`forward`]): many hosts report to one collector in batches,
//! each event named by `(host, session, seq)` so a repeat is ignored. The
//! collector runs the same reducer, so the board there is the board here.
//!
//! Audience: unlike the activity journal, this store keeps a scrubbed
//! one-line summary of what each call acts on ([`summary`]). It is never
//! fed to plugins, digests, or share links; it is read with the hub's own
//! credential, and by a collector this hub was pointed at.
//!
//! Writes are batched: [`Telemetry::record`] updates the in-memory state
//! and queues the event; a writer thread commits a batch every
//! [`FLUSH_EVERY`] in one transaction and then announces the sessions that
//! changed (at most once each per flush, however many events arrived).

pub mod ask;
pub mod claude;
pub mod codex;
pub mod event;
pub mod forward;
pub mod insights;
pub mod lanes;
pub mod log;
pub mod mcp;
pub mod reduce;
pub mod shape;
pub mod station;
pub mod store;
pub mod summary;
#[cfg(test)]
mod tests;
pub mod turns;

pub use event::{
    Draft, Event, HoldKind, ItemStatus, OUTSIDE_ROLLUP, Outcome, PlanStep, SessionLinks,
    WaitReason, What,
};
pub use reduce::{Detail, SessionState, Snapshot};
pub use store::{BatchRollup, FleetPage, FleetQuery, StoreError};

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;
use store::Store;

/// How often queued events are committed (and changed sessions announced).
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// Events and sessions with nothing newer than this are forgotten.
pub const RETENTION_DAYS: i64 = 30;
/// How often old rows are pruned.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// A session with no event for this long leaves memory (its row stays; it
/// is read back when its next event arrives).
const EVICT_AFTER_MS: i64 = 6 * 60 * 60 * 1000;
/// Most events one batch from another hub may carry.
pub const BATCH_MAX_EVENTS: usize = 5000;
/// The session the log files a hub event under when it is about no
/// session (a gate decision, a digest): never on the fleet board.
pub const HUB_SCOPE: &str = "hub";

type Key = (String, String);
type Announce = dyn Fn(Snapshot) + Send + Sync;
type Moved = dyn Fn(&str) + Send + Sync;

/// Unix milliseconds now.
/// Most turns one read folds, and most events it reads for them.
pub const TURNS_MAX: usize = 60;
const TURN_EVENTS_MAX: usize = 20_000;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Unix milliseconds of an RFC 3339 time (a transcript line's).
pub fn ms_of(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|t| t.timestamp_millis())
}

#[derive(Default)]
struct Pending {
    events: Vec<Event>,
    dirty: HashSet<Key>,
}

struct Inner {
    host: String,
    live: Mutex<HashMap<Key, SessionState>>,
    pending: Mutex<Pending>,
    /// The writer's connection.
    db: Mutex<Store>,
    /// A second connection, for reads that must not wait on a commit.
    read: Mutex<Store>,
    announce: OnceLock<Box<Announce>>,
    /// Told of each own session whose status an event just moved.
    moved: OnceLock<Box<Moved>>,
    /// Serializes commits and evictions (taken before `live`).
    gate: Mutex<()>,
    stop: AtomicBool,
    wake: (Mutex<()>, Condvar),
    /// The log's last committed row (see [`log`]): waiters on the log
    /// wake when it moves.
    head: tokio::sync::watch::Sender<i64>,
}

/// The hub's telemetry: the store, the live states, and the writer.
pub struct Telemetry {
    inner: Arc<Inner>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Telemetry {
    /// Open the store in `state_dir` for the hub `host` and start the
    /// writer.
    pub fn open(state_dir: &Path, host: &str) -> Result<Self, StoreError> {
        let path = state_dir.join(store::DB_FILE);
        let db = Store::open(&path)?;
        let read = Store::open(&path)?;
        let head = tokio::sync::watch::Sender::new(read.head()?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let inner = Arc::new(Inner {
            host: host.to_owned(),
            live: Mutex::default(),
            pending: Mutex::default(),
            db: Mutex::new(db),
            read: Mutex::new(read),
            announce: OnceLock::new(),
            moved: OnceLock::new(),
            gate: Mutex::new(()),
            stop: AtomicBool::new(false),
            wake: (Mutex::new(()), Condvar::new()),
            head,
        });
        let writer = {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name("fw-hub-telemetry".to_owned())
                .spawn(move || inner.run())
                .ok()
        };
        Ok(Self {
            inner,
            writer: Mutex::new(writer),
        })
    }

    /// This hub's id: the `host` of the events it observes.
    pub fn host(&self) -> &str {
        &self.inner.host
    }

    /// Call `announce` with each changed session's snapshot after a flush.
    pub fn on_change(&self, announce: impl Fn(Snapshot) + Send + Sync + 'static) {
        let _ = self.inner.announce.set(Box::new(announce));
    }

    /// Call `moved` with an own session's id as soon as an event recorded
    /// for it moves its status (not at the next flush): whatever reads the
    /// live state from then on (a status push) already sees the new one,
    /// so a change and its undoing between two flushes are both told.
    pub fn on_move(&self, moved: impl Fn(&str) + Send + Sync + 'static) {
        let _ = self.inner.moved.set(Box::new(moved));
    }

    /// Record what this hub observed in its own session `session`. Gauge
    /// readings that say nothing new are dropped.
    pub fn record(&self, session: &str, drafts: Vec<Draft>) {
        if drafts.is_empty() {
            return;
        }
        let key = (self.inner.host.clone(), session.to_owned());
        let mut live = lock(&self.inner.live);
        let state = self.inner.state_in(&mut live, &key);
        let mut events = Vec::with_capacity(drafts.len());
        for draft in drafts {
            if state.unchanged(&draft.what) {
                continue;
            }
            let mut event = Event {
                moved: None,
                host: key.0.clone(),
                session: key.1.clone(),
                seq: state.seq + 1,
                ts: draft.ts,
                agent: draft.agent,
                work: None,
                item: None,
                what: draft.what,
            };
            apply_noting_moves(state, &mut event);
            events.push(event);
        }
        if events.is_empty() {
            return;
        }
        let moved = events.iter().any(|e| e.moved.is_some());
        {
            let mut pending = lock(&self.inner.pending);
            pending.events.extend(events);
            pending.dirty.insert(key);
        }
        drop(live);
        if moved && let Some(tell) = self.inner.moved.get() {
            tell(session);
        }
    }

    /// Record what a log this hub tails says (a Codex rollout's lines,
    /// each with its own time): lines older than the session's last event
    /// were read before (the log was read again from its start) and are
    /// skipped.
    pub fn record_log(&self, session: &str, drafts: Vec<Draft>) {
        if drafts.is_empty() {
            return;
        }
        let last = {
            let key = (self.inner.host.clone(), session.to_owned());
            let mut live = lock(&self.inner.live);
            self.inner.state_in(&mut live, &key).last_ts
        };
        self.record(
            session,
            drafts.into_iter().filter(|d| d.ts >= last).collect(),
        );
    }

    /// Take a batch of events other hubs observed (a collector's side).
    /// Events for a session arrive in order; one at or before the
    /// session's last is a repeat and is skipped. Returns how many were
    /// new. An event claiming this hub as its host is refused.
    pub fn ingest(&self, events: Vec<Event>) -> Result<usize, String> {
        if events.len() > BATCH_MAX_EVENTS {
            return Err(format!("a batch carries at most {BATCH_MAX_EVENTS} events"));
        }
        if let Some(bad) = events.iter().find(|e| !valid_origin(e, &self.inner.host)) {
            return Err(format!(
                "event {} of session {:?} names host {:?}: a batch carries another hub's events, \
                 each with its 32-hex hub id",
                bad.seq, bad.session, bad.host
            ));
        }
        let mut fresh = Vec::new();
        let mut dirty = HashSet::new();
        {
            let mut live = lock(&self.inner.live);
            for mut event in events {
                let key = (event.host.clone(), event.session.clone());
                let state = self.inner.state_in(&mut live, &key);
                if event.seq <= state.seq {
                    continue;
                }
                apply_noting_moves(state, &mut event);
                dirty.insert(key);
                fresh.push(event);
            }
        }
        let new = fresh.len();
        if new > 0 {
            let mut pending = lock(&self.inner.pending);
            pending.events.extend(fresh);
            pending.dirty.extend(dirty);
        }
        Ok(new)
    }

    /// A session in full (`host`: `None` for this hub's own).
    pub fn detail(&self, host: Option<&str>, session: &str) -> Option<Detail> {
        let key = (
            host.unwrap_or(&self.inner.host).to_owned(),
            session.to_owned(),
        );
        if let Some(state) = lock(&self.inner.live).get(&key) {
            return Some(state.detail());
        }
        let saved = lock(&self.inner.read).state(&key.0, &key.1);
        match saved {
            Ok(state) => state.map(|state| state.detail()),
            Err(e) => {
                eprintln!("fw hub: {e}");
                None
            }
        }
    }

    /// [`SessionState::refinement`] of each of `sessions` (this hub's own)
    /// that has reported anything: one out of memory (a hub just
    /// upgraded or restarted) is read back.
    pub fn refinements(&self, sessions: &[&str]) -> HashMap<String, (&'static str, i64, bool)> {
        let mut live = lock(&self.inner.live);
        let mut out = HashMap::new();
        for session in sessions {
            let key = (self.inner.host.clone(), (*session).to_owned());
            if !live.contains_key(&key) {
                let saved = lock(&self.inner.read).state(&key.0, &key.1);
                match saved {
                    Ok(Some(state)) => {
                        live.insert(key.clone(), state);
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        eprintln!("fw hub: {e}");
                        continue;
                    }
                }
            }
            if let Some(state) = live.get(&key) {
                out.insert((*session).to_owned(), state.refinement());
            }
        }
        out
    }

    /// This hub's own session on the board, if it has reported anything.
    pub fn snapshot(&self, session: &str) -> Option<Snapshot> {
        let key = (self.inner.host.clone(), session.to_owned());
        lock(&self.inner.live).get(&key).map(SessionState::snapshot)
    }

    /// A page of a session's events (see [`Store::events`]).
    pub fn events(
        &self,
        host: Option<&str>,
        session: &str,
        after: u64,
        limit: usize,
        newest: bool,
    ) -> Result<Vec<Event>, StoreError> {
        self.flush();
        lock(&self.inner.read).events(
            host.unwrap_or(&self.inner.host),
            session,
            after,
            limit,
            newest,
        )
    }

    /// The newest `limit` turns of a session, each folded (see
    /// [`turns`]); `None`: the session has reported nothing.
    pub fn turns(
        &self,
        host: Option<&str>,
        session: &str,
        limit: usize,
    ) -> Result<Option<Vec<turns::Turn>>, StoreError> {
        let Some(detail) = self.detail(host, session) else {
            return Ok(None);
        };
        let limit = limit.clamp(1, TURNS_MAX);
        let mut starts: Vec<i64> = detail
            .work
            .iter()
            .filter(|w| w.source == reduce::WorkSource::Turn)
            .map(|w| w.started)
            .collect();
        starts.sort_unstable();
        let from = starts
            .len()
            .checked_sub(limit)
            .and_then(|at| starts.get(at))
            .copied()
            .unwrap_or(0);
        self.flush();
        let events = lock(&self.inner.read).events_from(
            host.unwrap_or(&self.inner.host),
            session,
            from,
            TURN_EVENTS_MAX,
        )?;
        let complete = events.first().is_some_and(|e| e.seq == 1);
        Ok(Some(turns::fold(&detail.work, &events, limit, complete)))
    }

    /// Where time and money went (see [`insights`]).
    pub fn insights(
        &self,
        since: i64,
        host: Option<&str>,
        project: Option<&str>,
    ) -> Result<insights::Insights, StoreError> {
        self.flush();
        lock(&self.inner.read).insights(since, host, project)
    }

    /// A page of the fleet board.
    pub fn fleet(&self, query: &FleetQuery) -> Result<FleetPage, StoreError> {
        self.flush();
        lock(&self.inner.read).fleet(query)
    }

    /// This hub's events after the forwarder's cursor `name`, oldest
    /// first, with the cursor each would move it to.
    pub fn outbox(&self, name: &str, limit: usize) -> Result<Vec<(i64, Event)>, StoreError> {
        self.flush();
        let read = lock(&self.inner.read);
        let after = read.cursor(name)?;
        read.outbox(&self.inner.host, after, limit)
    }

    /// One read of the event log (see [`log`]).
    pub fn log(&self, query: &log::LogQuery) -> Result<log::LogPage, StoreError> {
        self.flush();
        lock(&self.inner.read).log(query)
    }

    /// The log's epoch (what its cursors carry).
    pub fn log_epoch(&self) -> Result<u32, StoreError> {
        lock(&self.inner.read).epoch()
    }

    /// The cursor at the log's end now.
    pub fn log_head(&self) -> Result<log::Cursor, StoreError> {
        Ok(log::Cursor {
            epoch: self.log_epoch()?,
            at: *self.inner.head.borrow(),
        })
    }

    /// A session's labels as the store keeps them.
    pub fn session_labels(&self, host: &str, session: &str) -> crate::labels::Labels {
        lock(&self.inner.read)
            .labels(host, session)
            .unwrap_or_default()
    }

    /// Wakes when the log's end moves (a commit added rows).
    pub fn log_moves(&self) -> tokio::sync::watch::Receiver<i64> {
        self.inner.head.subscribe()
    }

    /// This hub's sessions with log rows after `after` (`None`: more than
    /// `max` rows since), and whether the session list itself changed.
    pub fn sessions_since(
        &self,
        after: i64,
        max: usize,
    ) -> Result<Option<(Vec<String>, bool)>, StoreError> {
        self.flush();
        lock(&self.inner.read).sessions_since(&self.inner.host, after, max)
    }

    /// Move the forwarder's cursor `name` (its batch was accepted).
    pub fn advance(&self, name: &str, at: i64) -> Result<(), StoreError> {
        lock(&self.inner.db).set_cursor(name, at)
    }

    /// Commit everything queued now.
    pub fn flush(&self) {
        self.inner.flush();
    }

    /// Flush and stop the writer.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.wake.1.notify_all();
        if let Some(writer) = lock(&self.writer).take() {
            let _ = writer.join();
        }
        self.inner.flush();
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Fold `event` into `state`, noting on it the status it moved the
/// session to (the log's state filter): both the hub that saw it and a
/// collector derive the same.
fn apply_noting_moves(state: &mut SessionState, event: &mut Event) {
    let before = state.status().0;
    let observed = state.observed();
    state.apply(event);
    let after = state.status().0;
    if state.observed() && (after != before || !observed) {
        event.moved = Some(after.to_owned());
    }
}

/// A forwarded event names another hub by its id.
fn valid_origin(event: &Event, own: &str) -> bool {
    event.host != own
        && event.host.len() == 32
        && event.host.bytes().all(|b| b.is_ascii_hexdigit())
        && !event.session.is_empty()
        && event.session.len() <= 128
        && event.seq > 0
}

impl Inner {
    /// The live state for `key`: in memory, else as saved, else new.
    fn state_in<'a>(
        &self,
        live: &'a mut HashMap<Key, SessionState>,
        key: &Key,
    ) -> &'a mut SessionState {
        // A session not in memory has nothing queued: it left memory only
        // after a commit saved it whole (see `evict`), so its row is
        // current. Read on the readers' connection: recording never waits
        // on the writer's commit.
        live.entry(key.clone()).or_insert_with(|| {
            let read = lock(&self.read);
            read.state(&key.0, &key.1)
                .ok()
                .flatten()
                .unwrap_or_else(|| {
                    // Never saved (a session only the hub recorded): its
                    // numbering and labels go on from what is kept.
                    let mut state = SessionState::new(&key.0, &key.1);
                    state.seq = read.max_seq(&key.0, &key.1).unwrap_or(0);
                    state.restore_labels(read.labels(&key.0, &key.1).unwrap_or_default());
                    state
                })
        })
    }

    fn run(&self) {
        let mut pruned: Option<std::time::Instant> = None;
        while !self.stop.load(Ordering::SeqCst) {
            {
                let guard = lock(&self.wake.0);
                let _ = self
                    .wake
                    .1
                    .wait_timeout(guard, FLUSH_EVERY)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            self.flush();
            if pruned.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
                pruned = Some(std::time::Instant::now());
                let before = now_ms() - RETENTION_DAYS * 24 * 60 * 60 * 1000;
                if let Err(e) = lock(&self.db).prune(before) {
                    eprintln!("fw hub: {e}");
                }
                self.evict(now_ms() - EVICT_AFTER_MS);
            }
        }
    }

    /// Commit the queue, then announce what changed. The states are
    /// copied under the `live` lock and written outside it, so recording
    /// never waits on a commit.
    fn flush(&self) {
        let changed = {
            let _gate = lock(&self.gate);
            self.commit()
        };
        if let Some(announce) = self.announce.get() {
            for snapshot in changed {
                announce(snapshot);
            }
        }
    }

    /// One commit; the caller holds `gate`.
    fn commit(&self) -> Vec<Snapshot> {
        let (pending, states) = {
            let live = lock(&self.live);
            let pending = std::mem::take(&mut *lock(&self.pending));
            let states: Vec<SessionState> = pending
                .dirty
                .iter()
                .filter_map(|key| live.get(key).cloned())
                .collect();
            (pending, states)
        };
        if pending.events.is_empty() && states.is_empty() {
            return Vec::new();
        }
        {
            let mut db = lock(&self.db);
            if let Err(e) = db.write(&pending.events, &states) {
                eprintln!("fw hub: {e}");
            }
            match db.head() {
                Ok(head) => {
                    self.head
                        .send_if_modified(|at| std::mem::replace(at, head) != head);
                }
                Err(e) => eprintln!("fw hub: {e}"),
            }
        }
        states
            .iter()
            .filter(|s| s.observed())
            .map(SessionState::snapshot)
            .collect()
    }

    /// Drop from memory the sessions with nothing since `before`. Under
    /// `gate`: no commit is between copying a state and saving it, so a
    /// dropped session's row is whole when it is read back.
    fn evict(&self, before: i64) {
        let _gate = lock(&self.gate);
        let _ = self.commit();
        lock(&self.live).retain(|_, state| state.last_ts >= before);
    }
}
