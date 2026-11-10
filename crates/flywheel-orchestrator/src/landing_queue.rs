//! The machine-local landing queue (spec 00089, D-1/D-5/D-7).
//!
//! A merge-strategy landing enters a FIFO queue of files beside the
//! project's merge lock ([`queue_dir_beside`] of
//! [`crate::home::ProjectPaths::merge_lock`], i.e.
//! [`crate::home::ProjectPaths::landing_queue`]). It is scoped exactly like
//! the `flock` it sits beside: one queue per machine, never a store table.
//!
//! * An **entry** (`<queued_at>-<run>.entry.json`) is written when the
//!   owning worker reaches the landing (run DONE, held-out gate
//!   `pass`/`no_gate`, lease re-confirmed). It carries the task, run, owner
//!   worker, `queued_at`, and the full [`SubmitRequest`] so whichever worker
//!   holds the merge lock can land it.
//! * A **settlement** (`<run>.settled.json`) is written by the lock holder
//!   once the entry's landing settled (landed, parked, or no-op); the owner
//!   takes it and returns that exact [`SubmitOutcome`] (D-6).
//! * **Liveness** is the one predicate [`owner_is_live`]: the entry's task
//!   claim is held by its owner with an unexpired lease. A dead entry is
//!   dropped, never landed (D-7), and the same predicate filters what
//!   readers report, so a SIGKILLed owner's entry vanishes within one lease
//!   window.
//!
//! The owner keeps its claim, lease, and conflict keys from the held-out
//! gate through the queue wait to the settlement (D-9): nothing here
//! touches the claim.

use crate::claims::{ClaimStore, ClaimStoreError};
use crate::strategy::{SubmitOutcome, SubmitRequest};
use chrono::{DateTime, Utc};
use flywheel_core::events::LandedBatch;
use flywheel_core::invoker_client::Clock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The queue directory's name, beside the project's `.merge.lock`.
pub const LANDING_QUEUE_DIR: &str = ".landing-queue";

const ENTRY_SUFFIX: &str = ".entry.json";
const SETTLED_SUFFIX: &str = ".settled.json";

/// The queue directory beside `lock_path` (the merge lock). For a project
/// this equals [`crate::home::ProjectPaths::landing_queue`].
pub fn queue_dir_beside(lock_path: &Path) -> PathBuf {
    lock_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        .join(LANDING_QUEUE_DIR)
}

/// A fresh, never-reused batch id (a lone landing gets its own).
pub fn fresh_batch_id() -> String {
    format!("batch-{}", uuid::Uuid::new_v4().simple())
}

// --- errors -------------------------------------------------------------------

/// A queue file could not be read, written, or decoded.
#[derive(Debug, thiserror::Error)]
pub enum LandingQueueError {
    #[error("landing queue I/O at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot encode landing queue record {}: {source}", path.display())]
    Encode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Claims(#[from] ClaimStoreError),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> LandingQueueError + '_ {
    move |source| LandingQueueError::Io {
        path: path.to_path_buf(),
        source,
    }
}

// --- the ticket ----------------------------------------------------------------

/// What a landing needs to enter the queue: the owning worker and the claim
/// store + clock the liveness predicate reads.
///
/// Only the orchestrator's normal landing (`DriveContext`) stamps one on its
/// [`SubmitRequest`]; `fw reland`, the landing re-driver, and every other
/// caller leave it unset and land directly under the merge lock.
#[derive(Clone)]
pub struct LandingTicket {
    pub owner: String,
    pub claims: Arc<dyn ClaimStore>,
    pub now: Clock,
}

impl LandingTicket {
    pub fn new(owner: impl Into<String>, claims: Arc<dyn ClaimStore>, now: Clock) -> Self {
        Self {
            owner: owner.into(),
            claims,
            now,
        }
    }

    /// [`owner_is_live`] for `entry` at this ticket's clock.
    pub fn entry_is_live(&self, entry: &QueueEntry) -> Result<bool, ClaimStoreError> {
        owner_is_live(
            self.claims.as_ref(),
            &entry.task_id,
            &entry.owner,
            (self.now)(),
        )
    }
}

impl fmt::Debug for LandingTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LandingTicket")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

/// Tickets compare by owner: the store and clock handles are not data.
impl PartialEq for LandingTicket {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner
    }
}

/// The one liveness predicate: `task_id`'s claim is held by `owner` with a
/// lease that has not lapsed at `now` (a lease expiring at `now` is lapsed,
/// matching the claim sweep's `lease_expires_at <= now`).
pub fn owner_is_live(
    claims: &dyn ClaimStore,
    task_id: &str,
    owner: &str,
    now: DateTime<Utc>,
) -> Result<bool, ClaimStoreError> {
    Ok(claims
        .load_claim(task_id)?
        .is_some_and(|claim| claim.worker_id == owner && claim.lease_expires_at > now))
}

// --- records -------------------------------------------------------------------

/// One queued landing.
///
/// `batch` is unset while the entry waits and set by the merge-lock holder
/// the moment it starts landing the entry: it is exactly the batch recorded
/// on the entry's `Landed` witness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub task_id: String,
    pub run_id: String,
    pub owner: String,
    pub queued_at: DateTime<Utc>,
    #[serde(default)]
    pub batch: Option<LandedBatch>,
    pub request: SubmitRequest,
}

impl QueueEntry {
    fn file_name(&self) -> String {
        let nanos = self.queued_at.timestamp_nanos_opt().unwrap_or_default();
        format!("{nanos:020}-{}{ENTRY_SUFFIX}", file_safe(&self.run_id))
    }

    /// Whether the lock holder is landing this entry right now.
    pub fn is_verifying(&self) -> bool {
        self.batch.is_some()
    }
}

/// The settled outcome of one entry, left for its owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settlement {
    pub task_id: String,
    pub run_id: String,
    pub owner: String,
    pub outcome: SubmitOutcome,
}

fn file_safe(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// --- the queue -----------------------------------------------------------------

/// The queue directory and its file operations. Every mutation is a
/// temp-file write + rename, so a reader never sees a torn record. Callers
/// serialize mutations other than [`LandingQueue::enqueue`] under the merge
/// lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandingQueue {
    dir: PathBuf,
}

impl LandingQueue {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The queue beside the merge lock at `lock_path`.
    pub fn beside_lock(lock_path: &Path) -> Self {
        Self::at(queue_dir_beside(lock_path))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> Result<(), LandingQueueError> {
        fs::create_dir_all(&self.dir).map_err(io_err(&self.dir))?;
        let path = self.dir.join(name);
        let body =
            serde_json::to_vec_pretty(value).map_err(|source| LandingQueueError::Encode {
                path: path.clone(),
                source,
            })?;
        let tmp = self
            .dir
            .join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut file = fs::File::create(&tmp).map_err(io_err(&tmp))?;
        file.write_all(&body).map_err(io_err(&tmp))?;
        file.sync_all().map_err(io_err(&tmp))?;
        drop(file);
        fs::rename(&tmp, &path).map_err(io_err(&path))
    }

    fn remove(&self, name: &str) -> Result<(), LandingQueueError> {
        let path = self.dir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(LandingQueueError::Io { path, source: err }),
        }
    }

    fn list(&self, suffix: &str) -> Result<Vec<PathBuf>, LandingQueueError> {
        let read = match fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(io_err(&self.dir)(err)),
        };
        let mut paths = Vec::new();
        for item in read {
            let path = item.map_err(io_err(&self.dir))?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with('.') && name.ends_with(suffix) {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    /// Append `request` to the queue as owned by `owner`, stamped
    /// `queued_at`. The request's ticket is not persisted.
    pub fn enqueue(
        &self,
        request: &SubmitRequest,
        owner: &str,
        queued_at: DateTime<Utc>,
    ) -> Result<QueueEntry, LandingQueueError> {
        let mut stored = request.clone();
        stored.landing_ticket = None;
        let entry = QueueEntry {
            task_id: request.task_id.clone(),
            run_id: request.run_id.clone(),
            owner: owner.to_owned(),
            queued_at,
            batch: None,
            request: stored,
        };
        // A re-enqueue of the same run (a retried submit) replaces its entry.
        self.remove_run_entries(&entry.run_id)?;
        self.write_json(&entry.file_name(), &entry)?;
        Ok(entry)
    }

    fn remove_run_entries(&self, run_id: &str) -> Result<(), LandingQueueError> {
        for entry in self.raw_entries()? {
            if entry.run_id == run_id {
                self.remove(&entry.file_name())?;
            }
        }
        Ok(())
    }

    /// Every decodable entry, live or not. An entry file that cannot be
    /// read or decoded (a foreign or torn file) is skipped.
    fn raw_entries(&self) -> Result<Vec<QueueEntry>, LandingQueueError> {
        let mut entries: Vec<QueueEntry> = self
            .list(ENTRY_SUFFIX)?
            .into_iter()
            .filter_map(|path| fs::read(&path).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect();
        entries.sort_by(|a, b| {
            a.queued_at
                .cmp(&b.queued_at)
                .then_with(|| a.run_id.cmp(&b.run_id))
        });
        Ok(entries)
    }

    /// Every entry in FIFO (`queued_at`) order, liveness unfiltered: the
    /// drain order the merge-lock holder walks.
    pub fn entries(&self) -> Result<Vec<QueueEntry>, LandingQueueError> {
        self.raw_entries()
    }

    /// The entries whose owner is live at `now`, in FIFO order: what every
    /// reader reports.
    pub fn live_entries(
        &self,
        claims: &dyn ClaimStore,
        now: DateTime<Utc>,
    ) -> Result<Vec<QueueEntry>, LandingQueueError> {
        let mut live = Vec::new();
        for entry in self.raw_entries()? {
            if owner_is_live(claims, &entry.task_id, &entry.owner, now)? {
                live.push(entry);
            }
        }
        Ok(live)
    }

    /// Whether `entry` (by run) is still queued.
    pub fn contains(&self, entry: &QueueEntry) -> Result<bool, LandingQueueError> {
        Ok(self.dir.join(entry.file_name()).is_file())
    }

    /// Mark `entry` as being landed through `batch` (state `verifying`).
    pub fn mark_verifying(
        &self,
        entry: &QueueEntry,
        batch: LandedBatch,
    ) -> Result<QueueEntry, LandingQueueError> {
        let marked = QueueEntry {
            batch: Some(batch),
            ..entry.clone()
        };
        self.write_json(&marked.file_name(), &marked)?;
        Ok(marked)
    }

    /// Drop `entry` unlanded (its owner is not live).
    pub fn drop_entry(&self, entry: &QueueEntry) -> Result<(), LandingQueueError> {
        self.remove(&entry.file_name())
    }

    /// Record `entry`'s settled `outcome` for its owner, then remove the
    /// entry: the settlement exists before the entry vanishes.
    pub fn settle(
        &self,
        entry: &QueueEntry,
        outcome: &SubmitOutcome,
    ) -> Result<(), LandingQueueError> {
        let settlement = Settlement {
            task_id: entry.task_id.clone(),
            run_id: entry.run_id.clone(),
            owner: entry.owner.clone(),
            outcome: outcome.clone(),
        };
        self.write_json(&settlement_name(&entry.run_id), &settlement)?;
        self.remove(&entry.file_name())
    }

    /// Take (read and remove) the settlement recorded for `run_id`.
    pub fn take_settlement(&self, run_id: &str) -> Result<Option<Settlement>, LandingQueueError> {
        let name = settlement_name(run_id);
        let path = self.dir.join(&name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(LandingQueueError::Io { path, source: err }),
        };
        self.remove(&name)?;
        Ok(serde_json::from_slice(&bytes).ok())
    }

    /// Remove every settlement whose owner is no longer live (it died after
    /// another worker landed its entry), so they never accumulate.
    pub fn prune_settlements(
        &self,
        claims: &dyn ClaimStore,
        now: DateTime<Utc>,
    ) -> Result<usize, LandingQueueError> {
        let mut pruned = 0;
        for path in self.list(SETTLED_SUFFIX)? {
            let Some(settlement) = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Settlement>(&bytes).ok())
            else {
                continue;
            };
            if !owner_is_live(claims, &settlement.task_id, &settlement.owner, now)? {
                self.remove(&settlement_name(&settlement.run_id))?;
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    /// The landing view of every live entry, keyed by task id.
    pub fn landing_views(
        &self,
        claims: &dyn ClaimStore,
        now: DateTime<Utc>,
    ) -> Result<BTreeMap<String, LandingView>, LandingQueueError> {
        Ok(landing_views(&self.live_entries(claims, now)?))
    }
}

fn settlement_name(run_id: &str) -> String {
    format!("{}{SETTLED_SUFFIX}", file_safe(run_id))
}

// --- the landing view ------------------------------------------------------------

/// Where a queued task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandingState {
    /// Waiting for the merge-lock holder.
    Queued,
    /// Being landed (its batch is under the standing verify).
    Verifying,
}

impl LandingState {
    pub const fn as_str(self) -> &'static str {
        match self {
            LandingState::Queued => "queued",
            LandingState::Verifying => "verifying",
        }
    }
}

/// The `landing` object every surface reports for a queued task:
/// `{state, position, queued_at, batch: null|{id, index, size}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingView {
    pub state: LandingState,
    pub position: u32,
    pub queued_at: DateTime<Utc>,
    pub batch: Option<LandedBatch>,
}

/// Positions over `entries` (already liveness-filtered, any order): 1-based,
/// entries being landed first, then the waiting ones in FIFO order of
/// `queued_at`. Keyed by task id.
pub fn landing_views(entries: &[QueueEntry]) -> BTreeMap<String, LandingView> {
    let mut ordered: Vec<&QueueEntry> = entries.iter().collect();
    ordered.sort_by(|a, b| {
        b.is_verifying()
            .cmp(&a.is_verifying())
            .then_with(|| a.queued_at.cmp(&b.queued_at))
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
    ordered
        .into_iter()
        .zip(1u32..)
        .map(|(entry, position)| {
            let state = if entry.is_verifying() {
                LandingState::Verifying
            } else {
                LandingState::Queued
            };
            (
                entry.task_id.clone(),
                LandingView {
                    state,
                    position,
                    queued_at: entry.queued_at,
                    batch: entry.batch.clone(),
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{InMemoryClaimStore, conflict_keys};
    use crate::home::ProjectPaths;
    use flywheel_core::lifecycle::Status;
    use flywheel_core::task::{CommandGrader, Grader, Task};
    use serde_json::json;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }

    fn request(task_id: &str, run_id: &str) -> SubmitRequest {
        let task = Task::new(
            format!("land {task_id}"),
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id(task_id);
        SubmitRequest::new(task, run_id, Status::Done, format!("/wt/{task_id}"))
    }

    fn claim(store: &InMemoryClaimStore, task_id: &str, worker: &str, now: DateTime<Utc>) {
        store
            .acquire_claim(task_id, worker, now, 60.0, &conflict_keys::<_, String>([]))
            .unwrap()
            .expect("claim acquired");
    }

    #[test]
    fn landing_queue_dir_sits_beside_the_project_merge_lock() {
        let paths = ProjectPaths::at("/home/u/.flywheel/projects/p1");
        assert_eq!(queue_dir_beside(&paths.merge_lock()), paths.landing_queue());
        assert_eq!(
            LandingQueue::beside_lock(&paths.merge_lock()).dir(),
            paths.landing_queue()
        );
    }

    #[test]
    fn landing_queue_entries_are_fifo_by_queued_at_whoever_wrote_them() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = LandingQueue::at(tmp.path().join("q"));
        assert!(queue.entries().unwrap().is_empty(), "absent dir is empty");
        // Written out of order by two owners.
        queue.enqueue(&request("c", "run-c"), "w2", at(30)).unwrap();
        queue.enqueue(&request("a", "run-a"), "w1", at(10)).unwrap();
        queue.enqueue(&request("b", "run-b"), "w2", at(20)).unwrap();
        let order: Vec<(String, String)> = queue
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| (e.task_id, e.owner))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a".to_owned(), "w1".to_owned()),
                ("b".to_owned(), "w2".to_owned()),
                ("c".to_owned(), "w2".to_owned()),
            ]
        );
        let first = &queue.entries().unwrap()[0];
        assert_eq!(first.run_id, "run-a");
        assert_eq!(first.queued_at, at(10));
        assert_eq!(first.request, request("a", "run-a"));
        assert_eq!(first.batch, None);
    }

    #[test]
    fn landing_queue_reenqueue_of_a_run_replaces_its_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = LandingQueue::at(tmp.path());
        queue.enqueue(&request("a", "run-a"), "w1", at(10)).unwrap();
        queue.enqueue(&request("a", "run-a"), "w1", at(40)).unwrap();
        let entries = queue.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].queued_at, at(40));
    }

    #[test]
    fn landing_queue_positions_count_verifying_entries_first() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = LandingQueue::at(tmp.path());
        let a = queue.enqueue(&request("a", "run-a"), "w1", at(10)).unwrap();
        queue.enqueue(&request("b", "run-b"), "w2", at(20)).unwrap();
        let c = queue.enqueue(&request("c", "run-c"), "w3", at(30)).unwrap();
        let mut entries = queue.entries().unwrap();
        let views = landing_views(&entries);
        assert_eq!(views["a"].position, 1);
        assert_eq!(views["b"].position, 2);
        assert_eq!(views["c"].position, 3);
        assert!(views.values().all(|v| v.state == LandingState::Queued));
        assert!(views.values().all(|v| v.batch.is_none()));

        // c is being landed (e.g. a and b's owners are mid-recovery): it
        // counts first, the waiting ones keep their FIFO order behind it.
        let batch = LandedBatch::single("batch-x");
        let marked = queue.mark_verifying(&c, batch.clone()).unwrap();
        assert!(queue.contains(&marked).unwrap());
        entries = queue.entries().unwrap();
        let views = landing_views(&entries);
        assert_eq!(views["c"].position, 1);
        assert_eq!(views["c"].state, LandingState::Verifying);
        assert_eq!(views["c"].batch, Some(batch));
        assert_eq!(views["a"].position, 2);
        assert_eq!(views["b"].position, 3);
        assert_eq!(views["a"].queued_at, a.queued_at);

        let encoded = serde_json::to_value(&views["a"]).unwrap();
        assert_eq!(
            encoded,
            json!({
                "state": "queued",
                "position": 2,
                "queued_at": serde_json::to_value(at(10)).unwrap(),
                "batch": null,
            })
        );
        assert_eq!(
            serde_json::to_value(&views["c"]).unwrap()["batch"],
            json!({"id": "batch-x", "index": 1, "size": 1})
        );
        assert_eq!(views["c"].state.as_str(), "verifying");
    }

    #[test]
    fn landing_queue_liveness_requires_the_owner_holding_an_unexpired_lease() {
        let claims = InMemoryClaimStore::new();
        // No claim at all.
        assert!(!owner_is_live(&claims, "a", "w1", at(0)).unwrap());
        claim(&claims, "a", "w1", at(0));
        assert!(owner_is_live(&claims, "a", "w1", at(1)).unwrap());
        // Held by someone else (a stolen lease) is not live for w1.
        assert!(!owner_is_live(&claims, "a", "w2", at(1)).unwrap());
        // Lapsed at exactly the expiry instant, and after.
        assert!(!owner_is_live(&claims, "a", "w1", at(60)).unwrap());
        assert!(!owner_is_live(&claims, "a", "w1", at(61)).unwrap());
    }

    #[test]
    fn landing_queue_readers_filter_entries_by_owner_liveness() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = LandingQueue::at(tmp.path());
        let claims = InMemoryClaimStore::new();
        claim(&claims, "a", "w1", at(0));
        claim(&claims, "b", "w2", at(0));
        queue.enqueue(&request("a", "run-a"), "w1", at(1)).unwrap();
        queue.enqueue(&request("b", "run-b"), "w2", at(2)).unwrap();
        // Owner w9 holds nothing (its worker was killed and swept).
        queue.enqueue(&request("z", "run-z"), "w9", at(0)).unwrap();

        let views = queue.landing_views(&claims, at(5)).unwrap();
        assert_eq!(views.keys().cloned().collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(views["a"].position, 1, "the dead entry takes no position");
        assert_eq!(views["b"].position, 2);
        // The dead entry is still on disk (only the drainer drops it) but
        // past every lease window nothing is reported.
        assert_eq!(queue.entries().unwrap().len(), 3);
        assert!(queue.landing_views(&claims, at(120)).unwrap().is_empty());

        let ticket = LandingTicket::new(
            "w1",
            Arc::new(InMemoryClaimStore::new()),
            Arc::new(|| at(5)),
        );
        assert!(!ticket.entry_is_live(&queue.entries().unwrap()[1]).unwrap());
    }

    #[test]
    fn landing_queue_settlement_is_left_for_the_owner_and_taken_once() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = LandingQueue::at(tmp.path());
        let entry = queue.enqueue(&request("a", "run-a"), "w1", at(1)).unwrap();
        let outcome = SubmitOutcome::landed("merge", "abc123");
        queue.settle(&entry, &outcome).unwrap();
        assert!(
            !queue.contains(&entry).unwrap(),
            "settled entry left the queue"
        );
        assert!(queue.entries().unwrap().is_empty());
        let settlement = queue.take_settlement("run-a").unwrap().unwrap();
        assert_eq!(settlement.outcome, outcome);
        assert_eq!(settlement.owner, "w1");
        assert_eq!(queue.take_settlement("run-a").unwrap(), None);
    }

    #[test]
    fn landing_queue_prunes_settlements_of_dead_owners_only() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = LandingQueue::at(tmp.path());
        let claims = InMemoryClaimStore::new();
        claim(&claims, "a", "w1", at(0));
        let a = queue.enqueue(&request("a", "run-a"), "w1", at(1)).unwrap();
        let z = queue.enqueue(&request("z", "run-z"), "w9", at(2)).unwrap();
        queue.settle(&a, &SubmitOutcome::no_op("x")).unwrap();
        queue.settle(&z, &SubmitOutcome::no_op("y")).unwrap();
        assert_eq!(queue.prune_settlements(&claims, at(5)).unwrap(), 1);
        assert!(queue.take_settlement("run-z").unwrap().is_none());
        assert!(queue.take_settlement("run-a").unwrap().is_some());
    }

    #[test]
    fn landing_queue_drop_removes_an_entry_unsettled() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = LandingQueue::at(tmp.path());
        let entry = queue.enqueue(&request("a", "run-a"), "w1", at(1)).unwrap();
        queue.drop_entry(&entry).unwrap();
        assert!(!queue.contains(&entry).unwrap());
        assert!(queue.take_settlement("run-a").unwrap().is_none());
        // Dropping twice is idempotent.
        queue.drop_entry(&entry).unwrap();
    }

    #[test]
    fn landing_queue_batch_ids_are_fresh() {
        let a = fresh_batch_id();
        let b = fresh_batch_id();
        assert_ne!(a, b);
        assert!(a.starts_with("batch-"));
    }
}
