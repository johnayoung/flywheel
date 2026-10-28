//! In-memory implementation of every store trait in [`crate::store`].
//!
//! The test substrate: a single [`InMemoryStore`] backed by plain maps and
//! vectors behind one `Mutex`, so it is `Send + Sync` and usable as
//! `Arc<dyn HarnessStore>`. It is not durable and performs no file, SQLite,
//! or network IO; durable persistence lives in [`crate::store_sqlite`].
//!
//! Storage follows the table boundaries in
//! `crates/flywheel-core/schema/persistence-schema.sql`:
//!
//! * `lifecycles` is keyed by `run_id` and holds the row state with no
//!   attempts attached; `attempts` are populated on read by joining the
//!   attempts table.
//! * `attempts` is keyed by `(run_id, number)`.
//! * `events` is the per-run domain-event ledger (domain rows only;
//!   telemetry flows to a [`crate::store::TelemetrySink`], never the store)
//!   and `grader_results` is an append-only list with monotonic ids.
//! * tasks are content-addressed: a logical identity per `id` and immutable
//!   versions keyed `(id, content_hash)` where the hash is
//!   [`crate::loaders::task_digest`].
//!
//! Every mutation runs under the single lock, so a domain-event append
//! (version check, projection write, event row) is atomic with respect to
//! every other caller exactly as the SQLite store's `BEGIN IMMEDIATE`
//! transaction makes it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::events::{DomainEvent, DomainEventPayload, EventReplayError, apply};
use crate::lifecycle::{Attempt, Lifecycle};
use crate::loaders::{deserialize_task, serialize_task, task_digest};
use crate::notifier::RunNotifier;
use crate::store::{
    AttemptStore, ControlCommandRecord, ControlCommandStore, DomainEventStore, GraderResultRecord,
    GraderResultStore, JsonObject, LifecycleFilter, LifecycleStore, SpendSummary, SpendWindow,
    StoreError, TaskStore,
};
use crate::task::{GraderType, Task};

/// The definition keys a content-addressed task version stores, mirroring
/// the `task_versions` columns (`goal`, `graders_json`, `tags_json`,
/// `context_json`).
const DEFINITION_KEYS: [&str; 4] = ["goal", "graders", "tags", "context"];

/// One immutable, content-addressed task version.
struct TaskVersion {
    definition: Map<String, Value>,
    created_at: DateTime<Utc>,
    /// Insertion order, the tie-break for "latest" when two versions share
    /// a `created_at` (a fixed or injected clock).
    seq: u64,
}

#[derive(Default)]
struct Inner {
    /// Row state only: `attempts` is always empty here (see module docs).
    lifecycles: HashMap<String, Lifecycle>,
    /// Write-time ordering surrogate for `list_lifecycles`' `(updated_at
    /// DESC, run_id DESC)` contract. `updated_at` is not a `Lifecycle`
    /// field, so every create/update stamps a strictly monotonic write
    /// sequence; a later write sorts ahead of an earlier one exactly as the
    /// durable stores' server-set `updated_at` column does.
    lifecycle_write_seq: u64,
    lifecycle_updated: HashMap<String, u64>,
    /// `run_id -> number -> attempt`; the inner map keeps `number` order.
    attempts: HashMap<String, BTreeMap<u32, Attempt>>,
    /// `run_id -> events in ascending sequence`; each stored event carries
    /// its `sequence` and `id`.
    events: HashMap<String, Vec<DomainEvent>>,
    event_id_seq: i64,
    grader_results: Vec<GraderResultRecord>,
    grader_result_seq: i64,
    /// Logical task id -> first_seen_at: the FK anchor a run references.
    task_identities: HashMap<String, DateTime<Utc>>,
    /// `task_id -> content_hash -> version`.
    task_versions: HashMap<String, HashMap<String, TaskVersion>>,
    task_seq: u64,
    /// Kept in enqueue order with a monotonic `id`, so a claim surfaces rows
    /// in ascending id order without a separate index.
    control_commands: Vec<ControlCommandRecord>,
    control_command_seq: i64,
}

/// In-memory implementation of every store trait.
///
/// Records returned to callers are owned copies, so caller mutations never
/// reach the stored rows and vice versa. Construct with [`InMemoryStore::new`]
/// or [`InMemoryStore::with_notifier`] to share a [`RunNotifier`] with an
/// in-process consumer.
pub struct InMemoryStore {
    notifier: Arc<RunNotifier>,
    inner: Mutex<Inner>,
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for InMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("InMemoryStore")
            .field("lifecycles", &inner.lifecycles.len())
            .field(
                "events",
                &inner.events.values().map(Vec::len).sum::<usize>(),
            )
            .field("grader_results", &inner.grader_results.len())
            .field("control_commands", &inner.control_commands.len())
            .finish()
    }
}

impl InMemoryStore {
    /// An empty store with its own [`RunNotifier`].
    pub fn new() -> Self {
        Self::with_notifier(Arc::new(RunNotifier::new()))
    }

    /// An empty store that signals domain-event appends on `notifier`.
    ///
    /// Default-on in-process reactivity: a consumer sharing this store (the
    /// common single-host case) gets push wakeups; separate instances fall
    /// back to the audit follower's poll.
    pub fn with_notifier(notifier: Arc<RunNotifier>) -> Self {
        Self {
            notifier,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The notifier every `append_domain_event` signals after committing.
    pub fn notifier(&self) -> &Arc<RunNotifier> {
        &self.notifier
    }

    /// Lock the state, tolerating a poisoned lock: every mutation below is
    /// applied only after its checks pass, so a panicking caller cannot leave
    /// a half-written row behind.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// --- Shared helpers ---------------------------------------------------------------

/// The lifecycle as a row: attempts stripped, so the attempts table stays the
/// single source of truth.
fn lifecycle_row(lifecycle: &Lifecycle) -> Lifecycle {
    let mut row = lifecycle.clone();
    row.attempts.clear();
    row
}

/// Parse a persisted enum wire string (`GraderType`, ...) through serde so an
/// unknown value surfaces as the same [`StoreError::Json`] a corrupt JSON
/// column would.
pub(crate) fn parse_wire_enum<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, StoreError> {
    Ok(serde_json::from_value(Value::String(text.to_owned()))?)
}

/// The `grader_results` row a `GraderEvaluated` event projects.
pub(crate) fn grader_result_from_event(
    event: &DomainEvent,
    evaluated: &crate::events::GraderEvaluated,
) -> Result<GraderResultRecord, StoreError> {
    // The attempt coordinate rides on the event row, not the payload; a
    // grader receipt without one cannot be keyed and is a malformed event.
    let attempt_number = event.attempt_number.ok_or_else(|| {
        StoreError::Json(<serde_json::Error as serde::de::Error>::custom(
            "grader_evaluated event carries no attempt_number",
        ))
    })?;
    let grader_type: GraderType = parse_wire_enum(&evaluated.grader_type)?;
    Ok(GraderResultRecord {
        run_id: event.run_id.clone(),
        attempt_number,
        ordinal: evaluated.ordinal,
        grader_type,
        grader_spec: evaluated.grader_spec.clone(),
        passed: evaluated.passed,
        duration_ms: evaluated.duration_ms,
        payload: evaluated.payload.clone(),
        ts: event.ts,
        grader_name: evaluated.grader_name.clone(),
        id: None,
    })
}

/// The attempt a folded lifecycle holds for `number`; the fold already
/// guaranteed it exists, so its absence is a corrupt-ledger signal.
pub(crate) fn projected_attempt(folded: &Lifecycle, number: u32) -> Result<&Attempt, StoreError> {
    folded
        .attempts
        .iter()
        .find(|a| a.number == number)
        .ok_or(StoreError::Replay(EventReplayError::UnknownAttempt {
            number,
        }))
}

/// The hashed definition columns of a task (`goal`, `graders`, `tags`,
/// `context`) as JSON values, in the shape the `task_versions` row stores.
pub(crate) fn task_definition(task: &Task) -> Result<Map<String, Value>, StoreError> {
    let serialized: Map<String, Value> = serde_json::from_value(serialize_task(task))?;
    let mut definition = Map::new();
    for key in DEFINITION_KEYS {
        let value = serialized.get(key).cloned().unwrap_or(Value::Null);
        definition.insert(key.to_owned(), value);
    }
    Ok(definition)
}

/// Reassemble a task from its identity and stored definition columns.
pub(crate) fn assemble_task(
    task_id: &str,
    definition: &Map<String, Value>,
) -> Result<Task, StoreError> {
    let mut data = Map::new();
    data.insert("id".to_owned(), Value::String(task_id.to_owned()));
    for key in DEFINITION_KEYS {
        let value = definition.get(key).cloned().unwrap_or(Value::Null);
        data.insert(key.to_owned(), value);
    }
    deserialize_task(&Value::Object(data)).map_err(StoreError::backend)
}

// --- Inner (lock-held) operations ------------------------------------------------

impl Inner {
    fn stamp_updated(&mut self, run_id: &str) {
        self.lifecycle_write_seq += 1;
        self.lifecycle_updated
            .insert(run_id.to_owned(), self.lifecycle_write_seq);
    }

    fn create_lifecycle(
        &mut self,
        lifecycle: &Lifecycle,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if self.lifecycles.contains_key(&lifecycle.run_id) {
            return Err(StoreError::lifecycle_already_exists(&lifecycle.run_id));
        }
        // Auto-register the task identity so a run always references a
        // catalogued task (mirrors the lifecycles.task_id -> tasks(id) FK
        // the durable stores enforce). Idempotent: save_task usually did it.
        self.task_identities
            .entry(lifecycle.task_id.clone())
            .or_insert(now);
        self.lifecycles
            .insert(lifecycle.run_id.clone(), lifecycle_row(lifecycle));
        self.stamp_updated(&lifecycle.run_id);
        Ok(())
    }

    fn update_lifecycle(
        &mut self,
        lifecycle: &Lifecycle,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        let stored = self
            .lifecycles
            .get_mut(&lifecycle.run_id)
            .ok_or_else(|| StoreError::lifecycle_not_found(&lifecycle.run_id))?;
        if stored.version != expected_version {
            return Err(StoreError::optimistic_concurrency(
                &lifecycle.run_id,
                expected_version,
                stored.version,
            ));
        }
        *stored = lifecycle_row(lifecycle);
        self.stamp_updated(&lifecycle.run_id);
        Ok(())
    }

    fn load_lifecycle(&self, run_id: &str) -> Option<Lifecycle> {
        let mut lifecycle = self.lifecycles.get(run_id)?.clone();
        lifecycle.attempts = self.list_attempts(run_id);
        Some(lifecycle)
    }

    fn list_attempts(&self, run_id: &str) -> Vec<Attempt> {
        self.attempts
            .get(run_id)
            .map(|rows| rows.values().cloned().collect())
            .unwrap_or_default()
    }

    fn upsert_attempt(&mut self, run_id: &str, attempt: &Attempt) {
        self.attempts
            .entry(run_id.to_owned())
            .or_default()
            .insert(attempt.number, attempt.clone());
    }

    fn append_grader_result(&mut self, result: &GraderResultRecord) -> GraderResultRecord {
        self.grader_result_seq += 1;
        let mut stored = result.clone();
        stored.id = Some(self.grader_result_seq);
        self.grader_results.push(stored.clone());
        stored
    }

    /// `MAX(sequence) + 1` over the run's ledger: strictly monotonic per run
    /// without a shared cross-table counter. The per-run vector is kept in
    /// append order, so the last row holds the maximum.
    fn next_domain_sequence(&self, run_id: &str) -> u64 {
        self.events
            .get(run_id)
            .and_then(|rows| rows.last())
            .and_then(|last| last.sequence)
            .unwrap_or(0)
            + 1
    }

    fn append_event_row(&mut self, event: &DomainEvent) -> u64 {
        let sequence = self.next_domain_sequence(&event.run_id);
        self.event_id_seq += 1;
        let mut stored = event.clone();
        stored.sequence = Some(sequence);
        stored.id = Some(self.event_id_seq);
        self.events
            .entry(event.run_id.clone())
            .or_default()
            .push(stored);
        sequence
    }

    /// Maintain the attempts / grader_results read-model projections for a
    /// non-seed event, under the same lock as the lifecycle row write.
    fn project_domain_event(
        &mut self,
        event: &DomainEvent,
        folded: &Lifecycle,
    ) -> Result<(), StoreError> {
        match &event.payload {
            DomainEventPayload::AttemptStarted(a) => {
                let attempt = projected_attempt(folded, a.number)?.clone();
                self.upsert_attempt(&folded.run_id, &attempt);
            }
            DomainEventPayload::AttemptFinalized(f) => {
                let attempt = projected_attempt(folded, f.number)?.clone();
                self.upsert_attempt(&folded.run_id, &attempt);
            }
            DomainEventPayload::GraderEvaluated(g) => {
                let record = grader_result_from_event(event, g)?;
                self.append_grader_result(&record);
            }
            _ => {}
        }
        Ok(())
    }
}

// --- LifecycleStore -----------------------------------------------------------------

impl LifecycleStore for InMemoryStore {
    fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
        self.lock().create_lifecycle(lifecycle, Utc::now())
    }

    fn update_lifecycle(
        &self,
        lifecycle: &Lifecycle,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        self.lock().update_lifecycle(lifecycle, expected_version)
    }

    fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
        Ok(self.lock().load_lifecycle(run_id))
    }

    fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
        let inner = self.lock();
        let mut matched: Vec<(u64, &str)> = inner
            .lifecycles
            .values()
            .filter(|lc| filter.matches(lc))
            .map(|lc| {
                let stamp = inner
                    .lifecycle_updated
                    .get(&lc.run_id)
                    .copied()
                    .unwrap_or(0);
                (stamp, lc.run_id.as_str())
            })
            .collect();
        // (updated_at DESC, run_id DESC): newest write first, ties (none
        // under a strictly monotonic write seq) broken by greater run_id.
        matched.sort_by(|a, b| b.cmp(a));
        Ok(matched
            .into_iter()
            .filter_map(|(_, run_id)| inner.load_lifecycle(run_id))
            .collect())
    }

    fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
        let inner = self.lock();
        let mut summary = SpendSummary::default();
        for attempt in inner.attempts.values().flat_map(BTreeMap::values) {
            if window.includes(attempt.last_activity_at) {
                summary.add_attempt(attempt);
            }
        }
        Ok(summary)
    }
}

// --- TaskStore ------------------------------------------------------------------------

impl TaskStore for InMemoryStore {
    fn save_task(&self, task: &Task, now: DateTime<Utc>) -> Result<String, StoreError> {
        let content_hash = task_digest(task);
        let definition = task_definition(task)?;
        let mut inner = self.lock();
        inner.task_identities.entry(task.id.clone()).or_insert(now);
        let already_stored = inner
            .task_versions
            .get(&task.id)
            .is_some_and(|versions| versions.contains_key(&content_hash));
        if !already_stored {
            // Immutable and deduplicated: the first save of a definition
            // fixes its created_at; a re-save of the same hash is a no-op.
            inner.task_seq += 1;
            let version = TaskVersion {
                definition,
                created_at: now,
                seq: inner.task_seq,
            };
            inner
                .task_versions
                .entry(task.id.clone())
                .or_default()
                .insert(content_hash.clone(), version);
        }
        Ok(content_hash)
    }

    fn load_task(
        &self,
        task_id: &str,
        content_hash: Option<&str>,
    ) -> Result<Option<Task>, StoreError> {
        let inner = self.lock();
        let Some(versions) = inner.task_versions.get(task_id) else {
            return Ok(None);
        };
        let version = match content_hash {
            Some(hash) => versions.get(hash),
            // Most recent by created_at, breaking ties on insertion order.
            None => versions.values().max_by_key(|v| (v.created_at, v.seq)),
        };
        version
            .map(|v| assemble_task(task_id, &v.definition))
            .transpose()
    }

    fn load_task_for_run(&self, run_id: &str) -> Result<Option<Task>, StoreError> {
        let (task_id, content_hash) = {
            let inner = self.lock();
            let Some(stored) = inner.lifecycles.get(run_id) else {
                return Ok(None);
            };
            (stored.task_id.clone(), stored.task_content_hash.clone())
        };
        let pinned = (!content_hash.is_empty()).then_some(content_hash.as_str());
        self.load_task(&task_id, pinned)
    }
}

// --- DomainEventStore -------------------------------------------------------------

impl DomainEventStore for InMemoryStore {
    fn append_domain_event(
        &self,
        event: &DomainEvent,
        expected_version: u64,
    ) -> Result<Lifecycle, StoreError> {
        let (folded, sequence) = {
            let mut inner = self.lock();
            let folded = match &event.payload {
                DomainEventPayload::LifecycleInitialized(_) => {
                    let seeded = apply(None, event)?;
                    // Fails with LifecycleAlreadyExists on a duplicate seed;
                    // the seed's expected_version is ignored by contract.
                    inner.create_lifecycle(&seeded, event.ts)?;
                    seeded
                }
                _ => {
                    let current = inner
                        .load_lifecycle(&event.run_id)
                        .ok_or_else(|| StoreError::lifecycle_not_found(&event.run_id))?;
                    if current.version != expected_version {
                        return Err(StoreError::optimistic_concurrency(
                            &event.run_id,
                            expected_version,
                            current.version,
                        ));
                    }
                    let folded = apply(Some(&current), event)?;
                    inner.update_lifecycle(&folded, expected_version)?;
                    inner.project_domain_event(event, &folded)?;
                    folded
                }
            };
            let sequence = inner.append_event_row(event);
            (folded, sequence)
        };
        // Signal only after the lock is released so a woken consumer can
        // read the committed append without contending for it.
        self.notifier.notify(&event.run_id, sequence);
        Ok(folded)
    }

    fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
        Ok(self.lock().events.get(run_id).cloned().unwrap_or_default())
    }
}

// --- AttemptStore -----------------------------------------------------------------------

impl AttemptStore for InMemoryStore {
    fn save_attempt(
        &self,
        run_id: &str,
        attempt: &Attempt,
        expected_version: Option<u64>,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if let Some(expected) = expected_version {
            // Versioned write (the harness's iteration-boundary aggregate
            // rollup): verify the lifecycle's optimistic-concurrency key
            // before the upsert so a stale worker cannot clobber counters
            // after the run moved on.
            let stored = inner
                .lifecycles
                .get(run_id)
                .ok_or_else(|| StoreError::lifecycle_not_found(run_id))?;
            if stored.version != expected {
                return Err(StoreError::optimistic_concurrency(
                    run_id,
                    expected,
                    stored.version,
                ));
            }
        }
        inner.upsert_attempt(run_id, attempt);
        Ok(())
    }

    fn load_attempt(&self, run_id: &str, number: u32) -> Result<Option<Attempt>, StoreError> {
        Ok(self
            .lock()
            .attempts
            .get(run_id)
            .and_then(|rows| rows.get(&number))
            .cloned())
    }

    fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError> {
        Ok(self.lock().list_attempts(run_id))
    }
}

// --- GraderResultStore ----------------------------------------------------------------

impl GraderResultStore for InMemoryStore {
    fn append_grader_result(
        &self,
        result: &GraderResultRecord,
    ) -> Result<GraderResultRecord, StoreError> {
        Ok(self.lock().append_grader_result(result))
    }

    fn list_grader_results(
        &self,
        run_id: &str,
        attempt_number: u32,
    ) -> Result<Vec<GraderResultRecord>, StoreError> {
        let inner = self.lock();
        let mut rows: Vec<GraderResultRecord> = inner
            .grader_results
            .iter()
            .filter(|r| r.run_id == run_id && r.attempt_number == attempt_number)
            .cloned()
            .collect();
        rows.sort_by_key(|r| r.ordinal);
        Ok(rows)
    }
}

// --- ControlCommandStore ------------------------------------------------------------

impl ControlCommandStore for InMemoryStore {
    fn enqueue_command(
        &self,
        run_id: &str,
        kind: &str,
        payload: JsonObject,
        now: DateTime<Utc>,
    ) -> Result<ControlCommandRecord, StoreError> {
        let mut inner = self.lock();
        inner.control_command_seq += 1;
        let mut record = ControlCommandRecord::pending(run_id, kind, payload, now);
        record.id = Some(inner.control_command_seq);
        inner.control_commands.push(record.clone());
        Ok(record)
    }

    fn claim_commands(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<ControlCommandRecord>, StoreError> {
        // Claim-once: every still-pending row for run_id flips to
        // claimed_at = now in this single locked pass, so a second call
        // (or a concurrent caller) finds nothing left to take. The vector is
        // in enqueue order already, which is ascending id order.
        let mut inner = self.lock();
        let claimed: Vec<ControlCommandRecord> = inner
            .control_commands
            .iter_mut()
            .filter(|c| c.run_id == run_id && c.is_pending())
            .map(|c| {
                c.claimed_at = Some(now);
                c.clone()
            })
            .collect();
        Ok(claimed)
    }

    fn delete_command(&self, command_id: i64) -> Result<(), StoreError> {
        // Queue hygiene (spec 00025 FR-10); unknown ids are a no-op.
        self.lock()
            .control_commands
            .retain(|c| c.id != Some(command_id));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::Status;
    use crate::store::HarnessStore;
    use std::time::Duration;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_780_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn two_stores_have_independent_state() {
        let s1 = InMemoryStore::new();
        let s2 = InMemoryStore::new();
        s1.create_lifecycle(&Lifecycle::new("t").with_run_id("shared"))
            .unwrap();
        assert!(s1.load_lifecycle("shared").unwrap().is_some());
        assert!(s2.load_lifecycle("shared").unwrap().is_none());
    }

    #[test]
    fn blocked_requires_json_round_trips_through_the_row_copy() {
        let store = InMemoryStore::new();
        let payload = r#"[{"type": "env_var_set", "name": "EXAMPLE"}]"#;
        let mut lc = Lifecycle::new("t").with_run_id("r1");
        lc.blocked_requires_json = Some(payload.to_owned());
        store.create_lifecycle(&lc).unwrap();
        let loaded = store.load_lifecycle("r1").unwrap().unwrap();
        assert_eq!(loaded.blocked_requires_json.as_deref(), Some(payload));

        store
            .create_lifecycle(&Lifecycle::new("t").with_run_id("r2"))
            .unwrap();
        assert_eq!(
            store
                .load_lifecycle("r2")
                .unwrap()
                .unwrap()
                .blocked_requires_json,
            None
        );

        let mut cleared = loaded;
        cleared
            .transition_to(Status::Ready, "", Some(at(1)))
            .unwrap();
        cleared.blocked_requires_json = None;
        store.update_lifecycle(&cleared, 1).unwrap();
        assert_eq!(
            store
                .load_lifecycle("r1")
                .unwrap()
                .unwrap()
                .blocked_requires_json,
            None
        );
    }

    #[test]
    fn is_usable_as_shared_harness_store() {
        let shared: Arc<dyn HarnessStore> = Arc::new(InMemoryStore::new());
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                shared
                    .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
                    .unwrap();
            })
        };
        worker.join().unwrap();
        assert!(shared.load_lifecycle("r1").unwrap().is_some());
    }

    #[test]
    fn append_signals_the_shared_notifier_with_the_committed_sequence() {
        let notifier = Arc::new(RunNotifier::new());
        let store = InMemoryStore::with_notifier(Arc::clone(&notifier));
        let seed = DomainEvent::new(
            "r1",
            at(0),
            DomainEventPayload::LifecycleInitialized(crate::events::LifecycleInitialized {
                task_id: "t".into(),
                ..Default::default()
            }),
        );
        store.append_domain_event(&seed, 0).unwrap();
        assert_eq!(notifier.watermark("r1"), 1);
        assert_eq!(store.notifier().wait("r1", 0, Duration::from_millis(1)), 1);
        assert!(Arc::ptr_eq(store.notifier(), &notifier));
    }

    #[test]
    fn debug_output_summarizes_row_counts() {
        let store = InMemoryStore::new();
        assert!(format!("{store:?}").contains("lifecycles: 0"));
    }
}
