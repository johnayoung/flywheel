//! Contract suite for the orchestrator's claim store, run against the
//! in-memory, SQLite, and Postgres backends (a port of the meaningful cases
//! in the Python `test_orchestrator_claims.py`), plus SQLite-only
//! concurrency and migration cases and, for both durable backends, interop
//! cases against frozen stores and views written by the Python claim store.
//!
//! The Postgres backend joins every shared case through the throwaway
//! container in `flywheel-core/tests/support/pg_container.rs` and drops out,
//! with a message, when Docker is unavailable; its own concurrency and
//! migration cases live in `claims_postgres.rs`.

#[path = "../../flywheel-core/tests/support/pg_container.rs"]
mod pg_container;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::Connection;
use serde_json::{Value, json};

use flywheel_core::event_serde::format_timestamp;
use flywheel_orchestrator::claims::{
    CURRENT_ORCH_SCHEMA_VERSION, ClaimStore, ClaimStoreError, EVENT_ACQUIRED, EVENT_EXPIRED,
    EVENT_RELEASED, EVENT_RENEWED, EVENT_STOLEN, GraphSnapshotItem, InMemoryClaimStore,
    REASON_AWAITING_APPROVAL, REASON_PREREQUISITE_MISSING, STOP_DANGLING_PREREQUISITE,
    STOP_NO_OP_CYCLE, STOP_PREPARE_SKIP, SqliteClaimStore, WorkItemObservation, conflict_keys,
};
use flywheel_orchestrator::claims_postgres::PostgresClaimStore;
use pg_container::Postgres;

fn t(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 28, 12, 0, second).unwrap()
}

fn none() -> BTreeSet<String> {
    BTreeSet::new()
}

struct Backend {
    name: &'static str,
    store: Box<dyn ClaimStore>,
    _dir: Option<tempfile::TempDir>,
    /// Keeps the shared container alive for the Postgres backend's lifetime.
    _pg: Option<Postgres>,
}

/// Every backend the shared cases run against: memory and sqlite always,
/// and a Postgres store on a fresh schema whenever the container is up.
fn backends() -> Vec<Backend> {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteClaimStore::open(dir.path().join("claims.db")).unwrap();
    let mut all = vec![
        Backend {
            name: "memory",
            store: Box::new(InMemoryClaimStore::new()),
            _dir: None,
            _pg: None,
        },
        Backend {
            name: "sqlite",
            store: Box::new(sqlite),
            _dir: Some(dir),
            _pg: None,
        },
    ];
    if let Some(pg) = Postgres::acquire() {
        let schema = pg.fresh_schema("fw_claims");
        let store = PostgresClaimStore::connect(&pg.dsn(), &schema).unwrap();
        all.push(Backend {
            name: "postgres",
            store: Box::new(store),
            _dir: None,
            _pg: Some(pg),
        });
    }
    all
}

/// Run `case` once per backend, labelling any panic with the backend name.
fn each(case: impl Fn(&dyn ClaimStore)) {
    for backend in backends() {
        let name = backend.name;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            case(backend.store.as_ref())
        }));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "non-string panic".to_owned());
            panic!("[{name}] {message}");
        }
    }
}

fn types(store: &dyn ClaimStore, task_id: &str) -> Vec<String> {
    store
        .list_task_events(task_id)
        .unwrap()
        .into_iter()
        .map(|e| e.event_type)
        .collect()
}

// --- Lease semantics ----------------------------------------------------------------

#[test]
fn acquire_claim_on_free_task_succeeds() {
    each(|store| {
        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .expect("free task is claimable");
        assert_eq!(claim.task_id, "task-a");
        assert_eq!(claim.worker_id, "worker-1");
        assert_eq!(claim.claimed_at, t(0));
        assert_eq!(claim.lease_expires_at, t(30));
        assert_eq!(claim.version, 1);
        assert_eq!(store.load_claim("task-a").unwrap(), Some(claim));
    });
}

#[test]
fn acquire_claim_held_by_live_other_worker_returns_none() {
    each(|store| {
        let first = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap();
        assert!(first.is_some());
        let refused = store
            .acquire_claim("task-a", "worker-2", t(10), 30.0, &none())
            .unwrap();
        assert!(refused.is_none());
        // Exactly at expiry (<= now) the lease is lapsed and stealable.
        let at_expiry = store
            .acquire_claim("task-a", "worker-2", t(30), 30.0, &none())
            .unwrap();
        assert!(at_expiry.is_some());
        assert_eq!(
            store.load_claim("task-a").unwrap().unwrap().worker_id,
            "worker-2"
        );
    });
}

#[test]
fn acquire_claim_reacquires_own_live_claim_with_version_bump() {
    each(|store| {
        let first = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let again = store
            .acquire_claim("task-a", "worker-1", t(10), 30.0, &none())
            .unwrap()
            .expect("same worker re-acquires idempotently");
        assert_eq!(again.version, first.version + 1);
        assert_eq!(again.lease_expires_at, t(40));
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED, EVENT_ACQUIRED]);
    });
}

#[test]
fn acquire_claim_steals_expired_lease_and_records_stolen() {
    each(|store| {
        let a = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let b = store
            .acquire_claim("task-a", "worker-2", t(31), 30.0, &none())
            .unwrap()
            .expect("lapsed lease is stolen");
        assert_eq!(b.worker_id, "worker-2");
        assert_eq!(b.version, a.version + 1);
        assert_eq!(b.claimed_at, t(31));
        let timeline = store.list_task_events("task-a").unwrap();
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED, EVENT_STOLEN]);
        assert_eq!(timeline[1].worker_id, "worker-2");
        assert_eq!(timeline[1].version, b.version);
        assert_eq!(timeline[1].lease_expires_at, b.lease_expires_at);
        assert_eq!(timeline[1].occurred_at, t(31));
    });
}

#[test]
fn renew_extends_lease_bumps_version_and_records_renewed() {
    each(|store| {
        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let renewed = store.renew_claim(&claim, t(20), 30.0).unwrap();
        assert_eq!(renewed.version, 2);
        assert_eq!(renewed.lease_expires_at, t(50));
        assert_eq!(renewed.claimed_at, t(0));
        assert_eq!(store.load_claim("task-a").unwrap(), Some(renewed.clone()));
        let timeline = store.list_task_events("task-a").unwrap();
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED, EVENT_RENEWED]);
        assert_eq!(timeline[1].version, 2);
        assert_eq!(timeline[1].lease_expires_at, t(50));
        assert_eq!(timeline[1].occurred_at, t(20));
        // The old token is stale now.
        let stale = store.renew_claim(&claim, t(25), 30.0);
        assert!(matches!(stale, Err(ClaimStoreError::ClaimLost(_))));
    });
}

#[test]
fn renew_after_steal_fails_with_claim_lost_and_records_no_event() {
    each(|store| {
        let a = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        store
            .acquire_claim("task-a", "worker-2", t(31), 30.0, &none())
            .unwrap()
            .unwrap();
        let err = store.renew_claim(&a, t(32), 30.0).err().unwrap();
        match err {
            ClaimStoreError::ClaimLost(lost) => assert_eq!(lost.task_id, "task-a"),
            other => panic!("expected ClaimLost, got {other:?}"),
        }
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED, EVENT_STOLEN]);
        assert_eq!(
            store.load_claim("task-a").unwrap().unwrap().worker_id,
            "worker-2"
        );
    });
}

#[test]
fn release_frees_the_task_and_records_released() {
    each(|store| {
        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        store.release_claim(&claim, Some(t(5))).unwrap();
        assert_eq!(store.load_claim("task-a").unwrap(), None);
        let other = store
            .acquire_claim("task-a", "worker-2", t(6), 30.0, &none())
            .unwrap();
        assert!(other.is_some());
        let timeline = store.list_task_events("task-a").unwrap();
        assert_eq!(
            types(store, "task-a"),
            vec![EVENT_ACQUIRED, EVENT_RELEASED, EVENT_ACQUIRED]
        );
        assert_eq!(timeline[1].occurred_at, t(5));
        assert_eq!(timeline[1].version, claim.version);
        assert_eq!(timeline[1].lease_expires_at, claim.lease_expires_at);
    });
}

#[test]
fn release_without_clock_stamps_the_lease_expiry() {
    each(|store| {
        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        store.release_claim(&claim, None).unwrap();
        let timeline = store.list_task_events("task-a").unwrap();
        assert_eq!(timeline[1].occurred_at, t(30));
    });
}

#[test]
fn release_with_stale_token_is_a_noop_with_no_event() {
    each(|store| {
        let a = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let b = store
            .acquire_claim("task-a", "worker-2", t(31), 30.0, &none())
            .unwrap()
            .unwrap();
        store.release_claim(&a, Some(t(32))).unwrap();
        assert_eq!(store.load_claim("task-a").unwrap(), Some(b.clone()));
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED, EVENT_STOLEN]);
        // A double release is also silent.
        store.release_claim(&b, Some(t(33))).unwrap();
        store.release_claim(&b, Some(t(34))).unwrap();
        assert_eq!(
            types(store, "task-a"),
            vec![EVENT_ACQUIRED, EVENT_STOLEN, EVENT_RELEASED]
        );
    });
}

#[test]
fn claims_are_independent_per_task() {
    each(|store| {
        store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let b = store
            .acquire_claim("task-b", "worker-2", t(0), 30.0, &none())
            .unwrap();
        assert!(b.is_some());
        assert_eq!(
            store.load_claim("task-a").unwrap().unwrap().worker_id,
            "worker-1"
        );
        assert_eq!(
            store.load_claim("task-b").unwrap().unwrap().worker_id,
            "worker-2"
        );
    });
}

#[test]
fn load_missing_claim_returns_none_and_empty_store_lists_nothing() {
    each(|store| {
        assert_eq!(store.load_claim("nope").unwrap(), None);
        assert!(store.list_claims().unwrap().is_empty());
        assert!(store.list_events().unwrap().is_empty());
        assert!(store.list_task_events("nope").unwrap().is_empty());
    });
}

#[test]
fn list_claims_enumerates_held_rows_and_drops_released() {
    each(|store| {
        let a = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let b = store
            .acquire_claim("task-b", "worker-2", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let listed = store.list_claims().unwrap();
        assert_eq!(listed, vec![a.clone(), b.clone()]);
        store.release_claim(&a, Some(t(1))).unwrap();
        assert_eq!(store.list_claims().unwrap(), vec![b.clone()]);
        // Expiry is not filtered: a lapsed-but-unstolen lease still lists.
        let listed_late = store.list_claims().unwrap();
        assert_eq!(listed_late, vec![b]);
    });
}

// --- Conflict keys --------------------------------------------------------------------

#[test]
fn conflict_key_overlap_with_a_different_live_claim_is_refused_without_event() {
    each(|store| {
        let held = store
            .acquire_claim(
                "task-a",
                "worker-1",
                t(0),
                30.0,
                &conflict_keys(["resource-x"]),
            )
            .unwrap();
        assert!(held.is_some());
        let refused = store
            .acquire_claim(
                "task-b",
                "worker-2",
                t(5),
                30.0,
                &conflict_keys(["resource-x", "resource-y"]),
            )
            .unwrap();
        assert!(refused.is_none());
        assert!(store.list_task_events("task-b").unwrap().is_empty());
        assert_eq!(store.load_claim("task-b").unwrap(), None);
        // Disjoint keys are never refused.
        let disjoint = store
            .acquire_claim(
                "task-c",
                "worker-2",
                t(5),
                30.0,
                &conflict_keys(["resource-z"]),
            )
            .unwrap();
        assert!(disjoint.is_some());
        // Empty keys are never refused on a conflict basis.
        let empty = store
            .acquire_claim("task-d", "worker-3", t(5), 30.0, &none())
            .unwrap();
        assert!(empty.is_some());
    });
}

#[test]
fn conflict_refusal_clears_on_release() {
    each(|store| {
        let held = store
            .acquire_claim(
                "task-a",
                "worker-1",
                t(0),
                30.0,
                &conflict_keys(["resource-x"]),
            )
            .unwrap()
            .unwrap();
        assert!(
            store
                .acquire_claim(
                    "task-b",
                    "worker-2",
                    t(5),
                    30.0,
                    &conflict_keys(["resource-x"])
                )
                .unwrap()
                .is_none()
        );
        store.release_claim(&held, Some(t(6))).unwrap();
        let after = store
            .acquire_claim(
                "task-b",
                "worker-2",
                t(7),
                30.0,
                &conflict_keys(["resource-x"]),
            )
            .unwrap();
        assert!(after.is_some());
        assert_eq!(types(store, "task-b"), vec![EVENT_ACQUIRED]);
    });
}

#[test]
fn conflict_refusal_clears_once_the_other_lease_lapses() {
    each(|store| {
        store
            .acquire_claim(
                "task-a",
                "worker-1",
                t(0),
                30.0,
                &conflict_keys(["resource-x"]),
            )
            .unwrap()
            .unwrap();
        assert!(
            store
                .acquire_claim(
                    "task-b",
                    "worker-2",
                    t(29),
                    30.0,
                    &conflict_keys(["resource-x"])
                )
                .unwrap()
                .is_none()
        );
        // The lapsed claim row still exists (not swept) but no longer blocks.
        let after = store
            .acquire_claim(
                "task-b",
                "worker-2",
                t(30),
                30.0,
                &conflict_keys(["resource-x"]),
            )
            .unwrap();
        assert!(after.is_some());
        assert!(store.load_claim("task-a").unwrap().is_some());
    });
}

#[test]
fn conflict_check_excludes_the_tasks_own_row() {
    each(|store| {
        let own = store
            .acquire_claim(
                "task-a",
                "worker-1",
                t(0),
                30.0,
                &conflict_keys(["resource-x"]),
            )
            .unwrap()
            .unwrap();
        let again = store
            .acquire_claim(
                "task-a",
                "worker-1",
                t(1),
                30.0,
                &conflict_keys(["resource-x"]),
            )
            .unwrap();
        assert!(again.is_some(), "own live row must not self-conflict");
        assert_eq!(again.unwrap().version, own.version + 1);
        // A different worker stealing the lapsed same-task row is likewise
        // not blocked by that row's own keys.
        let stolen = store
            .acquire_claim(
                "task-a",
                "worker-2",
                t(40),
                30.0,
                &conflict_keys(["resource-x"]),
            )
            .unwrap();
        assert!(stolen.is_some());
    });
}

// --- Liveness sweep ---------------------------------------------------------------

#[test]
fn sweep_reaps_only_lapsed_claims_and_records_one_expired_each() {
    each(|store| {
        let a = store
            .acquire_claim("task-a", "worker-dead", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let b = store
            .acquire_claim("task-b", "worker-dead", t(5), 30.0, &none())
            .unwrap()
            .unwrap();
        let live = store
            .acquire_claim("task-c", "worker-live", t(20), 30.0, &none())
            .unwrap()
            .unwrap();
        // Nothing has lapsed yet.
        assert!(store.sweep_expired_claims(t(29)).unwrap().is_empty());
        assert_eq!(store.list_claims().unwrap().len(), 3);
        // At :35 both of the dead worker's leases (30, 35) have lapsed; the
        // live one (50) has not.
        let reaped = store.sweep_expired_claims(t(35)).unwrap();
        assert_eq!(reaped, vec!["task-a", "task-b"]);
        assert_eq!(store.list_claims().unwrap(), vec![live.clone()]);
        for (claim, task) in [(&a, "task-a"), (&b, "task-b")] {
            let timeline = store.list_task_events(task).unwrap();
            assert_eq!(types(store, task), vec![EVENT_ACQUIRED, EVENT_EXPIRED]);
            let expired = &timeline[1];
            assert_eq!(expired.worker_id, "worker-dead");
            assert_eq!(expired.version, claim.version);
            assert_eq!(expired.lease_expires_at, claim.lease_expires_at);
            assert_eq!(expired.occurred_at, t(35));
        }
        assert_eq!(types(store, "task-c"), vec![EVENT_ACQUIRED]);
        // Reaped tasks are immediately re-acquirable; the live one is not.
        assert!(
            store
                .acquire_claim("task-a", "worker-new", t(36), 30.0, &none())
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .acquire_claim("task-c", "worker-new", t(36), 30.0, &none())
                .unwrap()
                .is_none()
        );
        // A second sweep at the same instant finds nothing: no duplicate rows.
        assert!(store.sweep_expired_claims(t(35)).unwrap().is_empty());
        let _ = live;
    });
}

#[test]
fn sweep_frees_conflict_keys_held_by_lapsed_claims() {
    each(|store| {
        store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &conflict_keys(["k"]))
            .unwrap()
            .unwrap();
        assert_eq!(store.sweep_expired_claims(t(30)).unwrap(), vec!["task-a"]);
        let after = store
            .acquire_claim("task-b", "worker-2", t(30), 30.0, &conflict_keys(["k"]))
            .unwrap();
        assert!(after.is_some());
    });
}

// --- Ledger ---------------------------------------------------------------------------

#[test]
fn ledger_is_append_only_in_id_order_with_one_row_per_committed_transition() {
    each(|store| {
        let a = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let b = store
            .acquire_claim("task-b", "worker-2", t(1), 30.0, &none())
            .unwrap()
            .unwrap();
        let a = store.renew_claim(&a, t(2), 30.0).unwrap();
        // Refused / no-op operations: none of these write a row.
        assert!(
            store
                .acquire_claim("task-a", "worker-3", t(3), 30.0, &none())
                .unwrap()
                .is_none()
        );
        assert!(store.renew_claim(&b.clone(), t(3), 30.0).is_ok());
        let stale_b = b;
        assert!(store.renew_claim(&stale_b, t(4), 30.0).is_err());
        store.release_claim(&stale_b, Some(t(4))).unwrap();
        assert!(store.sweep_expired_claims(t(4)).unwrap().is_empty());
        store.release_claim(&a, Some(t(5))).unwrap();
        let events = store.list_events().unwrap();
        let ids: Vec<i64> = events.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
        let shape: Vec<(&str, &str)> = events
            .iter()
            .map(|e| (e.task_id.as_str(), e.event_type.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("task-a", EVENT_ACQUIRED),
                ("task-b", EVENT_ACQUIRED),
                ("task-a", EVENT_RENEWED),
                ("task-b", EVENT_RENEWED),
                ("task-a", EVENT_RELEASED),
            ]
        );
        // Per-task timelines are the global stream filtered, in the same order.
        let a_ids: Vec<i64> = store
            .list_task_events("task-a")
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(a_ids, vec![1, 3, 5]);
        let b_ids: Vec<i64> = store
            .list_task_events("task-b")
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(b_ids, vec![2, 4]);
    });
}

// --- Stop events, prepare skips, human review ------------------------------------------

#[test]
fn stop_events_append_to_global_and_subject_streams_without_dedupe() {
    each(|store| {
        store
            .record_stop_event(
                STOP_DANGLING_PREREQUISITE,
                "task-a",
                "needs task-z",
                t(0),
                "",
            )
            .unwrap();
        store
            .record_stop_event(STOP_NO_OP_CYCLE, "/repo", "no candidates", t(1), "")
            .unwrap();
        store
            .record_stop_event(
                STOP_DANGLING_PREREQUISITE,
                "task-a",
                "needs task-z",
                t(2),
                "",
            )
            .unwrap();
        let all = store.list_stop_events().unwrap();
        assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(all[1].kind, STOP_NO_OP_CYCLE);
        assert_eq!(all[1].subject, "/repo");
        assert_eq!(all[1].run_id, "");
        assert_eq!(all[1].attribution, "");
        let for_a = store.list_subject_stop_events("task-a").unwrap();
        assert_eq!(for_a.len(), 2, "recurrence is the signal: no dedupe");
        assert_eq!(for_a[0].occurred_at, t(0));
        assert_eq!(for_a[1].occurred_at, t(2));
        assert!(store.list_subject_stop_events("task-q").unwrap().is_empty());
    });
}

#[test]
fn stop_event_attribution_is_persisted_as_its_own_field() {
    each(|store| {
        store
            .record_stop_event("stop-resolved", "task-a", "landed per probe", t(0), "probe")
            .unwrap();
        let row = &store.list_subject_stop_events("task-a").unwrap()[0];
        assert_eq!(row.attribution, "probe");
        assert_eq!(row.kind, "stop-resolved");
    });
}

#[test]
fn record_prepare_skip_releases_the_claim_and_records_the_stop_together() {
    each(|store| {
        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        store
            .record_prepare_skip(&claim, "docker daemon unreachable", t(3))
            .unwrap();
        assert_eq!(store.load_claim("task-a").unwrap(), None);
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED, EVENT_RELEASED]);
        let stops = store.list_subject_stop_events("task-a").unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].kind, STOP_PREPARE_SKIP);
        assert_eq!(stops[0].detail, "docker daemon unreachable");
        assert_eq!(stops[0].occurred_at, t(3));
        // A stale token still records the stop, just no release event.
        store.record_prepare_skip(&claim, "again", t(4)).unwrap();
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED, EVENT_RELEASED]);
        assert_eq!(store.list_subject_stop_events("task-a").unwrap().len(), 2);
    });
}

/// `stored` is `written` verbatim, or (Postgres, whose TEXT cannot hold
/// U+0000) with every NUL replaced by U+FFFD -- never refused.
fn assert_nul_preserved_or_replaced(stored: &str, written: &str) {
    assert!(
        stored == written || stored == written.replace('\0', "\u{fffd}"),
        "{stored:?} is neither {written:?} nor its NUL-replaced form"
    );
}

#[test]
fn nul_bytes_in_free_form_stop_and_sync_text_never_refuse_the_write() {
    // Setup-hook stderr tails and grader excerpts are lossily decoded
    // process output that keeps NULs; they reach these columns verbatim.
    each(|store| {
        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let skip = "PrepareError: sandbox setup failed (exit 1): bin\0ary";
        store.record_prepare_skip(&claim, skip, t(1)).unwrap();
        assert_eq!(
            store.load_claim("task-a").unwrap(),
            None,
            "the release commits with the stop row"
        );
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED, EVENT_RELEASED]);
        store
            .record_stop_event(STOP_NO_OP_CYCLE, "/repo", "\0", t(2), "")
            .unwrap();
        store
            .record_human_review(
                REASON_PREREQUISITE_MISSING,
                "task-b",
                t(3),
                "run-1",
                "tail: x\0y",
            )
            .unwrap();
        let stops = store.list_stop_events().unwrap();
        assert_eq!(stops.len(), 3);
        assert_eq!(stops[0].kind, STOP_PREPARE_SKIP);
        assert_nul_preserved_or_replaced(&stops[0].detail, skip);
        assert_nul_preserved_or_replaced(&stops[1].detail, "\0");
        let queue = store.list_human_review_queue().unwrap();
        assert_eq!(queue.len(), 1);
        assert_nul_preserved_or_replaced(&queue[0].detail, "tail: x\0y");

        let sync = store
            .record_source_sync_start("github", "owner/repo", t(4))
            .unwrap();
        store
            .record_source_sync_finish(sync, "error", 0, Some("gh: \0"), t(5))
            .unwrap();
        let row = store.load_source_sync(sync).unwrap().unwrap();
        assert_eq!(row.status, "error");
        assert_nul_preserved_or_replaced(row.error.as_deref().unwrap(), "gh: \0");
    });
}

#[test]
fn human_review_queue_projects_only_review_reasons() {
    each(|store| {
        store
            .record_stop_event(STOP_DANGLING_PREREQUISITE, "task-a", "pre-run", t(0), "")
            .unwrap();
        store
            .record_human_review(
                REASON_PREREQUISITE_MISSING,
                "task-a",
                t(1),
                "",
                "task-z gone",
            )
            .unwrap();
        store
            .record_human_review(REASON_AWAITING_APPROVAL, "task-b", t(2), "run-7", "")
            .unwrap();
        store
            .record_human_review("uncommitted-work", "task-c", t(3), "run-8", "dirty tree")
            .unwrap();
        let queue = store.list_human_review_queue().unwrap();
        assert_eq!(queue.len(), 3, "the pre-run stop is not a queue entry");
        assert_eq!(queue[0].task_id, "task-a");
        assert_eq!(queue[0].reason, REASON_PREREQUISITE_MISSING);
        assert_eq!(queue[0].run_id, "");
        assert_eq!(queue[0].detail, "task-z gone");
        assert_eq!(queue[1].run_id, "run-7");
        assert_eq!(queue[2].reason, "uncommitted-work");
        assert_eq!(queue[2].occurred_at, t(3));
        // Queue ids are the underlying ledger ids.
        assert_eq!(
            queue.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(store.list_stop_events().unwrap().len(), 4);
    });
}

#[test]
fn human_review_with_free_text_reason_is_refused_and_writes_nothing() {
    each(|store| {
        let err = store
            .record_human_review("the build was flaky", "task-a", t(0), "", "")
            .err()
            .unwrap();
        assert!(
            matches!(err, ClaimStoreError::UnknownReviewReason { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("the build was flaky"));
        assert!(store.list_stop_events().unwrap().is_empty());
        assert!(store.list_human_review_queue().unwrap().is_empty());
    });
}

// --- Session pauses ---------------------------------------------------------------------

#[test]
fn session_pause_records_clamped_horizon_and_lifts_on_its_own() {
    each(|store| {
        assert_eq!(store.session_pause_until(t(0)).unwrap(), None);
        // Within the ceiling: the reset itself.
        assert_eq!(
            store
                .record_session_pause(t(20), t(0), 60.0, "run-1")
                .unwrap(),
            Some(t(20))
        );
        // Far-future reset clamps to now + ceiling.
        assert_eq!(
            store
                .record_session_pause(t(50), t(0), 30.0, "run-2")
                .unwrap(),
            Some(t(30))
        );
        // Past reset and disabled ceiling record nothing.
        assert_eq!(
            store.record_session_pause(t(0), t(5), 30.0, "").unwrap(),
            None
        );
        assert_eq!(
            store.record_session_pause(t(50), t(5), 0.0, "").unwrap(),
            None
        );
        let rows = store.list_session_pauses().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[0].run_id, "run-1");
        assert_eq!(rows[0].reset_at, t(20));
        assert_eq!(rows[0].pause_until, t(20));
        assert_eq!(rows[0].recorded_at, t(0));
        assert_eq!(rows[1].pause_until, t(30));
        // High-water over rows still in the future.
        assert_eq!(store.session_pause_until(t(10)).unwrap(), Some(t(30)));
        assert_eq!(store.session_pause_until(t(25)).unwrap(), Some(t(30)));
        // Lifted once the horizon passes (strictly after now).
        assert_eq!(store.session_pause_until(t(30)).unwrap(), None);
    });
}

#[test]
fn session_pause_recorded_twice_does_not_accumulate() {
    each(|store| {
        store.record_session_pause(t(50), t(0), 30.0, "r").unwrap();
        store.record_session_pause(t(50), t(0), 30.0, "r").unwrap();
        assert_eq!(store.list_session_pauses().unwrap().len(), 2);
        assert_eq!(store.session_pause_until(t(1)).unwrap(), Some(t(30)));
    });
}

// --- Graph snapshots -----------------------------------------------------------------------

fn item_a() -> GraphSnapshotItem {
    GraphSnapshotItem {
        task_id: "task-a".to_owned(),
        source_kind: Some("directory".to_owned()),
        source_ref: Some("/tasks/a.json".to_owned()),
        source_url: Some("/tasks/a.json".to_owned()),
        source_version: Some("hash-a".to_owned()),
        priority: 5,
        required_capabilities: conflict_keys(["gpu", "linux"]),
        conflict_keys: conflict_keys(["resource-x"]),
        state: "pending".to_owned(),
        ready: true,
        claim_holder: Some("worker-1".to_owned()),
        resolved_prerequisites: BTreeSet::new(),
    }
}

fn item_b() -> GraphSnapshotItem {
    GraphSnapshotItem {
        task_id: "task-b".to_owned(),
        source_kind: Some("github_issue".to_owned()),
        source_ref: Some("owner/repo#7".to_owned()),
        source_url: Some("https://example/7".to_owned()),
        source_version: Some("hash-b".to_owned()),
        priority: 0,
        required_capabilities: BTreeSet::new(),
        conflict_keys: BTreeSet::new(),
        state: "blocked".to_owned(),
        ready: false,
        claim_holder: None,
        resolved_prerequisites: conflict_keys(["task-z", "task-a"]),
    }
}

#[test]
fn snapshot_round_trips_full_per_item_state_in_task_id_order() {
    each(|store| {
        let record = store
            .record_graph_snapshot(&[item_b(), item_a()], t(0))
            .unwrap();
        assert_eq!(record.id, 1);
        assert_eq!(record.item_count, 2);
        assert_eq!(record.captured_at, t(0));
        assert_eq!(record.last_event_id, 0);
        let items = store.list_graph_snapshot_items(record.id).unwrap();
        assert_eq!(items, vec![item_a(), item_b()]);
        assert!(items[0].ready);
        assert!(!items[1].ready);
        assert_eq!(items[1].claim_holder, None);
        assert_eq!(items[1].state, "blocked");
    });
}

#[test]
fn snapshot_item_count_equals_rows_and_input_size_including_empty() {
    each(|store| {
        for (n, size) in [0usize, 1, 3].into_iter().enumerate() {
            let items: Vec<GraphSnapshotItem> = (0..size)
                .map(|i| GraphSnapshotItem::new(format!("t{n}-{i}"), "pending", false))
                .collect();
            let record = store.record_graph_snapshot(&items, t(n as u32)).unwrap();
            assert_eq!(record.item_count, size as i64);
            assert_eq!(
                store.list_graph_snapshot_items(record.id).unwrap().len(),
                size
            );
        }
        assert_eq!(store.list_graph_snapshots().unwrap().len(), 3);
        assert!(store.list_graph_snapshot_items(99).unwrap().is_empty());
    });
}

#[test]
fn snapshot_cursor_tracks_the_event_high_water_mark() {
    each(|store| {
        let first = store.record_graph_snapshot(&[], t(0)).unwrap();
        assert_eq!(first.last_event_id, 0);
        let claim = store
            .acquire_claim("task-a", "worker-1", t(1), 30.0, &none())
            .unwrap()
            .unwrap();
        let claim = store.renew_claim(&claim, t(5), 30.0).unwrap();
        let latest = store.list_events().unwrap().last().unwrap().id;
        let second = store.record_graph_snapshot(&[], t(6)).unwrap();
        assert_eq!(second.last_event_id, latest);
        assert!(second.last_event_id > first.last_event_id);
        store.release_claim(&claim, Some(t(7))).unwrap();
        let third = store.record_graph_snapshot(&[], t(8)).unwrap();
        assert_eq!(
            third.last_event_id,
            store.list_events().unwrap().last().unwrap().id
        );
        assert!(third.last_event_id > second.last_event_id);
        // The stored header agrees with the returned one.
        assert_eq!(store.latest_graph_snapshot().unwrap(), Some(third));
    });
}

#[test]
fn snapshot_stream_is_append_only_and_latest_is_the_last() {
    each(|store| {
        assert_eq!(store.latest_graph_snapshot().unwrap(), None);
        let one = store.record_graph_snapshot(&[item_a()], t(0)).unwrap();
        let two = store.record_graph_snapshot(&[item_b()], t(1)).unwrap();
        assert_eq!(one.id, 1);
        assert_eq!(two.id, 2);
        assert_eq!(
            store.list_graph_snapshots().unwrap(),
            vec![one.clone(), two.clone()]
        );
        assert_eq!(store.latest_graph_snapshot().unwrap(), Some(two));
        // Recording a later snapshot never rewrites an earlier one's items.
        assert_eq!(
            store.list_graph_snapshot_items(one.id).unwrap(),
            vec![item_a()]
        );
    });
}

#[test]
fn snapshot_with_duplicate_task_is_refused_atomically() {
    each(|store| {
        let before = store.list_graph_snapshots().unwrap().len();
        let mut dup = item_a();
        dup.state = "running".to_owned();
        let err = store
            .record_graph_snapshot(&[item_a(), item_b(), dup], t(0))
            .err()
            .unwrap();
        assert!(
            matches!(err, ClaimStoreError::DuplicateSnapshotItem { .. }),
            "{err}"
        );
        assert_eq!(store.list_graph_snapshots().unwrap().len(), before);
        assert_eq!(store.latest_graph_snapshot().unwrap(), None);
        assert!(store.list_graph_snapshot_items(1).unwrap().is_empty());
        // The next successful snapshot gets id 1: nothing was consumed.
        let ok = store.record_graph_snapshot(&[item_a()], t(1)).unwrap();
        assert_eq!(ok.id, 1);
    });
}

// --- Work-item catalog and source syncs -----------------------------------------------------

fn observation(task_id: &str, keys: &[&str]) -> WorkItemObservation {
    WorkItemObservation {
        task_id: task_id.to_owned(),
        source_kind: Some("directory".to_owned()),
        source_ref: Some(format!("/tasks/{task_id}.json")),
        source_url: None,
        source_version: Some("v1".to_owned()),
        task_content_hash: format!("hash-{task_id}"),
        priority: 2,
        required_capabilities: conflict_keys(["linux"]),
        conflict_keys: conflict_keys(keys.iter().copied()),
    }
}

#[test]
fn work_item_upsert_pins_first_seen_advances_last_seen_and_clears_disappeared() {
    each(|store| {
        assert_eq!(store.load_work_item("task-a").unwrap(), None);
        store
            .upsert_work_item(&observation("task-a", &["k2", "k1"]), t(0))
            .unwrap();
        let first = store.load_work_item("task-a").unwrap().unwrap();
        assert_eq!(first.first_seen_at, t(0));
        assert_eq!(first.last_seen_at, t(0));
        assert_eq!(first.disappeared_at, None);
        assert_eq!(first.priority, 2);
        assert_eq!(first.task_content_hash.as_deref(), Some("hash-task-a"));
        assert_eq!(first.conflict_keys_json, "[\"k1\", \"k2\"]");
        assert_eq!(first.required_capabilities_json, "[\"linux\"]");
        assert_eq!(first.metadata_json, "{}");
        assert_eq!(first.source_ref.as_deref(), Some("/tasks/task-a.json"));

        store.mark_work_items_disappeared(&[], t(5)).unwrap();
        assert_eq!(
            store
                .load_work_item("task-a")
                .unwrap()
                .unwrap()
                .disappeared_at,
            Some(t(5))
        );
        // Already-marked items keep their original stamp.
        store.mark_work_items_disappeared(&[], t(6)).unwrap();
        assert_eq!(
            store
                .load_work_item("task-a")
                .unwrap()
                .unwrap()
                .disappeared_at,
            Some(t(5))
        );

        let mut changed = observation("task-a", &["k3"]);
        changed.priority = 9;
        changed.task_content_hash = "hash-2".to_owned();
        store.upsert_work_item(&changed, t(10)).unwrap();
        let again = store.load_work_item("task-a").unwrap().unwrap();
        assert_eq!(again.first_seen_at, t(0));
        assert_eq!(again.last_seen_at, t(10));
        assert_eq!(again.disappeared_at, None);
        assert_eq!(again.priority, 9);
        assert_eq!(again.task_content_hash.as_deref(), Some("hash-2"));
        assert_eq!(again.conflict_keys_json, "[\"k3\"]");
    });
}

#[test]
fn mark_disappeared_spares_observed_items() {
    each(|store| {
        store
            .upsert_work_item(&observation("task-a", &[]), t(0))
            .unwrap();
        store
            .upsert_work_item(&observation("task-b", &[]), t(0))
            .unwrap();
        store
            .mark_work_items_disappeared(&["task-b".to_owned()], t(1))
            .unwrap();
        let items = store.list_work_items().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].task_id, "task-a");
        assert_eq!(items[0].disappeared_at, Some(t(1)));
        assert_eq!(items[1].task_id, "task-b");
        assert_eq!(items[1].disappeared_at, None);
    });
}

#[test]
fn dependencies_replace_wholesale_and_collapse_duplicates() {
    each(|store| {
        store
            .replace_work_item_dependencies(
                "task-c",
                &[
                    "task-b".to_owned(),
                    "task-a".to_owned(),
                    "task-b".to_owned(),
                ],
                t(0),
            )
            .unwrap();
        assert_eq!(
            store.load_work_item_dependencies("task-c").unwrap(),
            vec!["task-a", "task-b"]
        );
        store
            .replace_work_item_dependencies("task-d", &["task-c".to_owned()], t(0))
            .unwrap();
        store
            .replace_work_item_dependencies("task-c", &["task-z".to_owned()], t(1))
            .unwrap();
        assert_eq!(
            store.list_work_item_dependencies().unwrap(),
            vec![
                ("task-c".to_owned(), "task-z".to_owned()),
                ("task-d".to_owned(), "task-c".to_owned()),
            ]
        );
        store
            .replace_work_item_dependencies("task-c", &[], t(2))
            .unwrap();
        assert!(
            store
                .load_work_item_dependencies("task-c")
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .load_work_item_dependencies("unknown")
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn source_syncs_open_running_and_settle_ok_or_error() {
    each(|store| {
        let ok_id = store
            .record_source_sync_start("directory", "/tasks", t(0))
            .unwrap();
        let err_id = store
            .record_source_sync_start("github", "owner/repo", t(1))
            .unwrap();
        assert_eq!((ok_id, err_id), (1, 2));
        let running = store.load_source_sync(ok_id).unwrap().unwrap();
        assert_eq!(running.status, "running");
        assert_eq!(running.finished_at, None);
        assert_eq!(running.observed_count, 0);
        assert_eq!(running.error, None);
        assert_eq!(running.metadata_json, "{}");
        store
            .record_source_sync_finish(ok_id, "ok", 4, None, t(2))
            .unwrap();
        store
            .record_source_sync_finish(err_id, "error", 0, Some("rate limited"), t(3))
            .unwrap();
        let all = store.list_source_syncs().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].status, "ok");
        assert_eq!(all[0].observed_count, 4);
        assert_eq!(all[0].finished_at, Some(t(2)));
        assert_eq!(all[1].status, "error");
        assert_eq!(all[1].error.as_deref(), Some("rate limited"));
        assert_eq!(all[1].started_at, t(1));
        assert_eq!(store.load_source_sync(42).unwrap(), None);
    });
}

// --- The (version, worker_id) token, re-acquired keys, lease arithmetic ----------------------

/// Versions restart at 1 once a row is released or reaped, so the version
/// alone does not identify a lease: a slow worker's stale token can carry the
/// same version as the claim another worker now holds. Only the worker half
/// of the token tells them apart; renew and release must both honour it or
/// the stale holder silently takes over (or frees) the new holder's lease.
#[test]
fn a_recycled_version_never_lets_a_stale_holder_renew_or_release_the_new_claim() {
    each(|store| {
        let stale = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        assert_eq!(store.sweep_expired_claims(t(30)).unwrap(), vec!["task-a"]);
        let current = store
            .acquire_claim("task-a", "worker-2", t(31), 30.0, &none())
            .unwrap()
            .unwrap();
        assert_eq!(current.version, stale.version, "row was recreated at v1");

        let err = store.renew_claim(&stale, t(32), 30.0).err().unwrap();
        assert!(err.is_claim_lost(), "stale renew must be lost, got {err:?}");
        store.release_claim(&stale, Some(t(33))).unwrap();

        assert_eq!(store.load_claim("task-a").unwrap(), Some(current));
        assert_eq!(
            types(store, "task-a"),
            vec![EVENT_ACQUIRED, EVENT_EXPIRED, EVENT_ACQUIRED]
        );
        store
            .record_prepare_skip(&stale, "stale holder dead-end", t(34))
            .unwrap();
        assert_eq!(
            store.load_claim("task-a").unwrap().unwrap().worker_id,
            "worker-2"
        );
    });
}

/// A steal or re-acquire replaces the row's conflict keys with the incoming
/// set: the old keys stop blocking and the new ones start.
#[test]
fn reacquiring_a_task_replaces_its_conflict_keys() {
    each(|store| {
        store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &conflict_keys(["old"]))
            .unwrap()
            .unwrap();
        store
            .acquire_claim("task-a", "worker-2", t(31), 30.0, &conflict_keys(["new"]))
            .unwrap()
            .expect("lapsed lease is stolen");
        let freed = store
            .acquire_claim("task-b", "worker-3", t(32), 30.0, &conflict_keys(["old"]))
            .unwrap();
        assert!(freed.is_some(), "the stolen row no longer holds 'old'");
        let blocked = store
            .acquire_claim("task-c", "worker-4", t(32), 30.0, &conflict_keys(["new"]))
            .unwrap();
        assert!(blocked.is_none(), "the stolen row now holds 'new'");

        // A same-worker re-acquire replaces them too.
        store
            .acquire_claim("task-a", "worker-2", t(33), 30.0, &conflict_keys(["newer"]))
            .unwrap()
            .unwrap();
        let unblocked = store
            .acquire_claim("task-c", "worker-4", t(34), 30.0, &conflict_keys(["new"]))
            .unwrap();
        assert!(unblocked.is_some(), "'new' was replaced by 'newer'");
    });
}

/// Lease seconds convert to whole microseconds by rounding, not truncation:
/// `1.005 * 1e6` is `1004999.999...` in binary floating point.
#[test]
fn fractional_lease_seconds_round_to_the_nearest_microsecond() {
    each(|store| {
        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 1.005, &none())
            .unwrap()
            .unwrap();
        assert_eq!(
            claim.lease_expires_at,
            t(0) + Duration::microseconds(1_005_000)
        );
        assert_eq!(
            store
                .load_claim("task-a")
                .unwrap()
                .unwrap()
                .lease_expires_at,
            claim.lease_expires_at
        );
        let renewed = store.renew_claim(&claim, t(1), 1.005).unwrap();
        assert_eq!(
            renewed.lease_expires_at,
            t(1) + Duration::microseconds(1_005_000)
        );
    });
}

/// A NaN lease is refused before anything is written, on acquire and renew.
#[test]
fn a_nan_lease_is_refused_without_writing_anything() {
    each(|store| {
        let err = store
            .acquire_claim("task-a", "worker-1", t(0), f64::NAN, &none())
            .expect_err("NaN lease refused");
        assert!(
            matches!(err, ClaimStoreError::LeaseOutOfRange { .. }),
            "{err:?}"
        );
        assert_eq!(store.load_claim("task-a").unwrap(), None);
        assert!(store.list_events().unwrap().is_empty());

        let claim = store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .unwrap();
        let err = store.renew_claim(&claim, t(5), f64::NAN).err().unwrap();
        assert!(
            matches!(err, ClaimStoreError::LeaseOutOfRange { .. }),
            "{err:?}"
        );
        assert_eq!(store.load_claim("task-a").unwrap(), Some(claim));
        assert_eq!(types(store, "task-a"), vec![EVENT_ACQUIRED]);
    });
}

// --- SQLite-only: concurrency, persistence, migration ----------------------------------------

#[test]
fn concurrent_acquire_on_one_file_admits_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.db");
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let handles: Vec<_> = (0..workers)
        .map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                // Each worker bootstraps its own connection concurrently, then
                // races the acquire from the barrier.
                let store = SqliteClaimStore::open(&path).unwrap();
                barrier.wait();
                let won = store
                    .acquire_claim("task-hot", &format!("worker-{i}"), t(0), 30.0, &none())
                    .unwrap()
                    .is_some();
                let keyed = store
                    .acquire_claim(
                        &format!("task-{i}"),
                        &format!("worker-{i}"),
                        t(0),
                        30.0,
                        &conflict_keys(["shared-resource"]),
                    )
                    .unwrap()
                    .is_some();
                (won, keyed)
            })
        })
        .collect();
    let results: Vec<(bool, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|(won, _)| *won).count(), 1);
    assert_eq!(results.iter().filter(|(_, keyed)| *keyed).count(), 1);
    let store = SqliteClaimStore::open(&path).unwrap();
    assert_eq!(store.list_task_events("task-hot").unwrap().len(), 1);
    assert_eq!(store.list_events().unwrap().len(), 2);
    assert_eq!(store.list_claims().unwrap().len(), 2);
}

/// A transition whose ledger write fails rolls back its claim write too: the
/// row never outlives the failed transition. A trigger in the same file makes
/// the ledger insert abort after the claim row was written.
#[test]
fn a_failed_ledger_write_rolls_back_the_claim_it_journals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.db");
    let store = SqliteClaimStore::open(&path).unwrap();
    let held = store
        .acquire_claim("task-held", "worker-1", t(0), 30.0, &none())
        .unwrap()
        .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_ledger BEFORE INSERT ON orchestrator_events \
             BEGIN SELECT RAISE(ABORT, 'ledger refused'); END;",
        )
        .unwrap();

    let err = store
        .acquire_claim("task-a", "worker-1", t(1), 30.0, &none())
        .expect_err("ledger failure surfaces");
    assert!(err.to_string().contains("ledger refused"), "{err}");
    assert_eq!(store.load_claim("task-a").unwrap(), None);

    assert!(store.renew_claim(&held, t(2), 30.0).is_err());
    assert_eq!(store.load_claim("task-held").unwrap(), Some(held.clone()));

    assert!(store.release_claim(&held, Some(t(3))).is_err());
    assert!(store.sweep_expired_claims(t(59)).is_err());
    assert_eq!(store.load_claim("task-held").unwrap(), Some(held));
    assert_eq!(store.list_events().unwrap().len(), 1);
}

#[test]
fn sqlite_state_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("durable.db");
    {
        let store = SqliteClaimStore::open(&path).unwrap();
        store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &conflict_keys(["k"]))
            .unwrap()
            .unwrap();
        store
            .record_session_pause(t(50), t(0), 30.0, "run-1")
            .unwrap();
        store.record_graph_snapshot(&[item_a()], t(1)).unwrap();
    }
    let store = SqliteClaimStore::open(&path).unwrap();
    assert_eq!(
        store.load_claim("task-a").unwrap().unwrap().worker_id,
        "worker-1"
    );
    assert_eq!(store.session_pause_until(t(10)).unwrap(), Some(t(30)));
    assert_eq!(store.list_graph_snapshot_items(1).unwrap(), vec![item_a()]);
    assert!(
        store
            .acquire_claim("task-b", "worker-2", t(5), 30.0, &conflict_keys(["k"]))
            .unwrap()
            .is_none()
    );
}

fn build_v3_store(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE task_claims (
           task_id            TEXT PRIMARY KEY,
           worker_id          TEXT NOT NULL,
           claimed_at         DATETIME NOT NULL,
           lease_expires_at   DATETIME NOT NULL,
           version            INTEGER NOT NULL,
           conflict_keys_json TEXT NOT NULL DEFAULT '[]'
         );
         CREATE TABLE orchestrator_schema_version (
           id      INTEGER PRIMARY KEY CHECK (id = 1),
           version INTEGER NOT NULL
         );
         CREATE TABLE work_items (
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
         INSERT INTO task_claims (task_id, worker_id, claimed_at, lease_expires_at, version, \
           conflict_keys_json) VALUES ('task-a', 'worker-1', '2026-05-28T12:00:00+00:00', \
           '2026-05-28T12:00:30+00:00', 1, '[]');
         INSERT INTO work_items (task_id, first_seen_at, last_seen_at) VALUES ('task-a', \
           '2026-05-28T12:00:00+00:00', '2026-05-28T12:00:00+00:00');
         INSERT INTO orchestrator_schema_version (id, version) VALUES (1, 3);",
    )
    .unwrap();
}

fn build_v6_stop_table_without_later_columns(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE orchestrator_stop_events (
           id          INTEGER PRIMARY KEY AUTOINCREMENT,
           kind        TEXT NOT NULL,
           subject     TEXT NOT NULL,
           detail      TEXT NOT NULL,
           occurred_at TEXT NOT NULL
         );
         INSERT INTO orchestrator_stop_events (kind, subject, detail, occurred_at) VALUES \
           ('no-op-cycle', '/repo', 'nothing', '2026-05-28T12:00:00+00:00');",
    )
    .unwrap();
}

#[test]
fn v3_store_migrates_forward_keeping_rows_with_empty_ledgers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.db");
    build_v3_store(&path);
    {
        let conn = Connection::open(&path).unwrap();
        build_v6_stop_table_without_later_columns(&conn);
    }
    let store = SqliteClaimStore::open(&path).unwrap();
    let claim = store.load_claim("task-a").unwrap().unwrap();
    assert_eq!(claim.worker_id, "worker-1");
    assert_eq!(claim.lease_expires_at, t(30));
    assert!(store.load_work_item("task-a").unwrap().is_some());
    assert!(store.list_events().unwrap().is_empty());
    assert!(store.list_graph_snapshots().unwrap().is_empty());
    assert!(store.list_session_pauses().unwrap().is_empty());
    // The pre-v7/v9 stop row survives with the added columns at their defaults.
    let stops = store.list_stop_events().unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].run_id, "");
    assert_eq!(stops[0].attribution, "");
    // The migrated store is fully functional and its sentinel converged.
    store
        .record_human_review(REASON_AWAITING_APPROVAL, "task-a", t(1), "run-1", "")
        .unwrap();
    assert_eq!(store.list_human_review_queue().unwrap().len(), 1);
    drop(store);
    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row(
            "SELECT version FROM orchestrator_schema_version WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, i64::from(CURRENT_ORCH_SCHEMA_VERSION));
}

// --- Interop with Python-written claim stores (frozen golden fixtures) ------------------------
//
// Deployments that predate the Rust port hold claim stores written by the
// Python `SqliteClaimStore` / `PostgresClaimStore`. The fixtures under
// `tests/fixtures/python_interop/` were produced by those Python stores and
// frozen:
//
// - `claims.sqlite`: the database file the Python store left after running
//   the seed transcript that `seed_with_rust` mirrors (workers `py-*`).
// - `claims_postgres.sql`: `pg_dump --inserts` of the same transcript written
//   by the Python Postgres store (schema name `fw_golden_claims`).
// - `python_view_of_python_seed.json`: the Python store's view (the shape
//   `rust_view` produces) of that seed, identical for both backends.
// - `python_view_after_rust_steal.json`: the Python store's view after the
//   Rust store stole `task-a` from the Python-seeded database.
// - `python_view_of_rust_seed.json`: the Python store's view of a database
//   the Rust store seeded with `seed_with_rust`.

/// The schema name baked into `claims_postgres.sql`.
const GOLDEN_PG_SCHEMA: &str = "fw_golden_claims";

fn python_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/python_interop")
        .join(name)
}

fn python_golden_view(name: &str) -> Value {
    let text = std::fs::read_to_string(python_fixture(name)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn opt_ts(ts: Option<DateTime<Utc>>) -> Value {
    ts.map_or(Value::Null, |ts| Value::String(format_timestamp(&ts)))
}

/// The Rust store's view of a database, in the JSON shape of the frozen
/// Python views.
fn rust_view(store: &dyn ClaimStore) -> Value {
    let claims: Vec<Value> = store
        .list_claims()
        .unwrap()
        .iter()
        .map(|c| {
            json!({
                "task_id": c.task_id, "worker_id": c.worker_id,
                "claimed_at": format_timestamp(&c.claimed_at),
                "lease_expires_at": format_timestamp(&c.lease_expires_at),
                "version": c.version,
            })
        })
        .collect();
    let events: Vec<Value> = store
        .list_events()
        .unwrap()
        .iter()
        .map(|e| {
            json!({
                "id": e.id, "task_id": e.task_id, "worker_id": e.worker_id,
                "event_type": e.event_type, "version": e.version,
                "lease_expires_at": format_timestamp(&e.lease_expires_at),
                "occurred_at": format_timestamp(&e.occurred_at),
            })
        })
        .collect();
    let snapshots: Vec<Value> = store
        .list_graph_snapshots()
        .unwrap()
        .iter()
        .map(|s| {
            let items: Vec<Value> = store
                .list_graph_snapshot_items(s.id)
                .unwrap()
                .iter()
                .map(|i| {
                    json!({
                        "task_id": i.task_id, "source_kind": i.source_kind,
                        "source_ref": i.source_ref, "source_url": i.source_url,
                        "source_version": i.source_version, "priority": i.priority,
                        "required_capabilities": i.required_capabilities,
                        "conflict_keys": i.conflict_keys, "state": i.state,
                        "ready": i.ready, "claim_holder": i.claim_holder,
                        "resolved_prerequisites": i.resolved_prerequisites,
                    })
                })
                .collect();
            json!({
                "id": s.id, "captured_at": format_timestamp(&s.captured_at),
                "item_count": s.item_count, "last_event_id": s.last_event_id,
                "items": items,
            })
        })
        .collect();
    let stop_events: Vec<Value> = store
        .list_stop_events()
        .unwrap()
        .iter()
        .map(|e| {
            json!({
                "id": e.id, "kind": e.kind, "subject": e.subject, "detail": e.detail,
                "occurred_at": format_timestamp(&e.occurred_at), "run_id": e.run_id,
                "attribution": e.attribution,
            })
        })
        .collect();
    let review_queue: Vec<Value> = store
        .list_human_review_queue()
        .unwrap()
        .iter()
        .map(|e| {
            json!({
                "id": e.id, "task_id": e.task_id, "run_id": e.run_id, "reason": e.reason,
                "detail": e.detail, "occurred_at": format_timestamp(&e.occurred_at),
            })
        })
        .collect();
    let pauses: Vec<Value> = store
        .list_session_pauses()
        .unwrap()
        .iter()
        .map(|p| {
            json!({
                "id": p.id, "run_id": p.run_id, "reset_at": format_timestamp(&p.reset_at),
                "pause_until": format_timestamp(&p.pause_until),
                "recorded_at": format_timestamp(&p.recorded_at),
            })
        })
        .collect();
    let work_items: Vec<Value> = store
        .list_work_items()
        .unwrap()
        .iter()
        .map(|w| {
            json!({
                "task_id": w.task_id, "source_kind": w.source_kind, "source_ref": w.source_ref,
                "source_url": w.source_url, "source_version": w.source_version,
                "task_content_hash": w.task_content_hash, "priority": w.priority,
                "required_capabilities_json": w.required_capabilities_json,
                "conflict_keys_json": w.conflict_keys_json,
                "first_seen_at": format_timestamp(&w.first_seen_at),
                "last_seen_at": format_timestamp(&w.last_seen_at),
                "disappeared_at": opt_ts(w.disappeared_at),
                "metadata_json": w.metadata_json,
            })
        })
        .collect();
    let dependencies: Vec<Value> = store
        .list_work_item_dependencies()
        .unwrap()
        .iter()
        .map(|(a, b)| json!([a, b]))
        .collect();
    let source_syncs: Vec<Value> = store
        .list_source_syncs()
        .unwrap()
        .iter()
        .map(|s| {
            json!({
                "id": s.id, "source_kind": s.source_kind, "source_name": s.source_name,
                "started_at": format_timestamp(&s.started_at),
                "finished_at": opt_ts(s.finished_at), "status": s.status,
                "observed_count": s.observed_count, "error": s.error,
                "metadata_json": s.metadata_json,
            })
        })
        .collect();
    json!({
        "claims": claims,
        "events": events,
        "snapshots": snapshots,
        "stop_events": stop_events,
        "review_queue": review_queue,
        "pauses": pauses,
        "pause_until_at_20": opt_ts(store.session_pause_until(t(20)).unwrap()),
        "work_items": work_items,
        "dependencies": dependencies,
        "source_syncs": source_syncs,
    })
}

/// A private copy of the Python-written SQLite fixture, so the committed
/// file stays byte-for-byte what Python wrote.
fn python_written_sqlite(dir: &Path) -> PathBuf {
    let db = dir.join("py-seeded.db");
    std::fs::copy(python_fixture("claims.sqlite"), &db).unwrap();
    db
}

/// Recreate the Python-written Postgres claim schema from its dump under
/// `schema`.
fn load_python_written_postgres(pg: &Postgres, schema: &str) {
    let dump = std::fs::read_to_string(python_fixture("claims_postgres.sql")).unwrap();
    pg.admin()
        .batch_execute(&dump.replace(GOLDEN_PG_SCHEMA, schema))
        .unwrap();
}

/// Rust reads a Python-seeded store exactly as Python did, coordinates
/// against its live leases, and the result of its steal is what Python
/// read back from the same database.
fn assert_rust_coordinates_on_python_seed(store: &dyn ClaimStore) {
    assert_rust_reads_python_seed(store);
    assert_eq!(
        rust_view(store),
        python_golden_view("python_view_of_python_seed.json")
    );
    assert_rust_steals_after_python_lease_lapses(store);
    assert_python_saw_rust_steal(
        store,
        &python_golden_view("python_view_after_rust_steal.json"),
    );
}

#[test]
fn interop_python_seeds_rust_reads_and_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteClaimStore::open(python_written_sqlite(dir.path())).unwrap();
    assert_rust_coordinates_on_python_seed(&store);
}

/// Rust reads back exactly what the Python `seed` transcript wrote.
fn assert_rust_reads_python_seed(store: &dyn ClaimStore) {
    let a = store.load_claim("task-a").unwrap().unwrap();
    assert_eq!(a.worker_id, "py-worker");
    assert_eq!(a.version, 2);
    assert_eq!(a.claimed_at, t(0) + Duration::microseconds(123_456));
    assert_eq!(a.lease_expires_at, t(35));
    assert_eq!(store.load_claim("task-b").unwrap(), None);
    assert_eq!(
        store.load_claim("task-c").unwrap().unwrap().worker_id,
        "py-worker"
    );
    assert_eq!(store.load_claim("task-d").unwrap(), None, "swept by Python");
    let event_types: Vec<String> = store
        .list_events()
        .unwrap()
        .into_iter()
        .map(|e| format!("{}:{}", e.task_id, e.event_type))
        .collect();
    assert_eq!(
        event_types,
        vec![
            "task-a:acquired",
            "task-b:acquired",
            "task-a:renewed",
            "task-b:released",
            "task-c:acquired",
            "task-c:stolen",
            "task-d:acquired",
            "task-d:expired",
        ]
    );
    let snapshot = store.latest_graph_snapshot().unwrap().unwrap();
    assert_eq!(snapshot.item_count, 2);
    assert_eq!(snapshot.last_event_id, 8);
    let items = store.list_graph_snapshot_items(snapshot.id).unwrap();
    assert_eq!(
        items[0].required_capabilities,
        conflict_keys(["gpu", "linux"])
    );
    assert_eq!(items[0].claim_holder.as_deref(), Some("py-worker"));
    assert_eq!(
        items[1].resolved_prerequisites,
        conflict_keys(["task-a", "task-z"])
    );
    assert!(!items[1].ready);
    assert_eq!(store.list_human_review_queue().unwrap()[0].run_id, "run-1");
    assert_eq!(store.session_pause_until(t(20)).unwrap(), Some(t(36)));
    let w = store.load_work_item("task-w").unwrap().unwrap();
    assert_eq!(w.conflict_keys_json, "[\"resource-x\"]");
    assert_eq!(w.priority, 3);
    assert_eq!(
        store.load_work_item_dependencies("task-w").unwrap(),
        vec!["task-a", "task-b"]
    );
    assert_eq!(store.load_source_sync(1).unwrap().unwrap().status, "ok");
}

/// Rust honours Python's live leases and conflict keys, then steals after
/// expiry.
fn assert_rust_steals_after_python_lease_lapses(store: &dyn ClaimStore) {
    assert!(
        store
            .acquire_claim("task-a", "rust-worker", t(20), 30.0, &none())
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .acquire_claim(
                "task-e",
                "rust-worker",
                t(20),
                30.0,
                &conflict_keys(["resource-y"])
            )
            .unwrap()
            .is_none()
    );
    let stolen = store
        .acquire_claim("task-a", "rust-worker", t(35), 30.0, &none())
        .unwrap()
        .expect("lease lapsed at :35");
    assert_eq!(stolen.version, 3);
}

/// Python saw the Rust-written steal as the ledger's last row, and the Rust
/// view after the steal is the Python view.
fn assert_python_saw_rust_steal(store: &dyn ClaimStore, py_after: &Value) {
    let last = py_after["events"].as_array().unwrap().last().unwrap();
    assert_eq!(last["event_type"], "stolen");
    assert_eq!(last["worker_id"], "rust-worker");
    assert_eq!(last["lease_expires_at"], "2026-05-28T12:01:05+00:00");
    assert_eq!(&rust_view(store), py_after);
}

#[test]
fn interop_rust_seeds_python_reads() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteClaimStore::open(dir.path().join("rust-seeded.db")).unwrap();
    seed_with_rust(&store);
    assert_python_read_rust_seed(&store, &python_golden_view("python_view_of_rust_seed.json"));
}

/// Seed `store` with the Rust twin of the transcript the Python store ran to
/// write the fixtures.
fn seed_with_rust(store: &dyn ClaimStore) {
    let a = store
        .acquire_claim(
            "task-a",
            "rust-worker",
            t(0) + Duration::microseconds(123_456),
            30.0,
            &conflict_keys(["resource-x", "resource-y"]),
        )
        .unwrap()
        .unwrap();
    let b = store
        .acquire_claim("task-b", "rust-worker", t(1), 30.0, &none())
        .unwrap()
        .unwrap();
    store.renew_claim(&a, t(5), 30.0).unwrap();
    store.release_claim(&b, Some(t(6))).unwrap();
    store
        .acquire_claim("task-c", "rust-other", t(0), 10.0, &none())
        .unwrap()
        .unwrap();
    store
        .acquire_claim("task-c", "rust-worker", t(11), 30.0, &none())
        .unwrap()
        .unwrap();
    store
        .acquire_claim("task-d", "rust-dead", t(0), 5.0, &none())
        .unwrap()
        .unwrap();
    assert_eq!(store.sweep_expired_claims(t(12)).unwrap(), vec!["task-d"]);
    let mut running = item_a();
    running.state = "running".to_owned();
    running.source_url = None;
    running.claim_holder = Some("rust-worker".to_owned());
    running.conflict_keys = conflict_keys(["resource-x", "resource-y"]);
    store
        .record_graph_snapshot(&[running, item_b()], t(13))
        .unwrap();
    store
        .record_stop_event(
            STOP_DANGLING_PREREQUISITE,
            "task-b",
            "needs task-z",
            t(14),
            "",
        )
        .unwrap();
    store
        .record_human_review(
            REASON_AWAITING_APPROVAL,
            "task-a",
            t(15),
            "run-1",
            "manual gate",
        )
        .unwrap();
    store
        .record_session_pause(t(50), t(16), 20.0, "run-1")
        .unwrap();
    store
        .upsert_work_item(
            &WorkItemObservation {
                task_id: "task-w".to_owned(),
                source_kind: Some("directory".to_owned()),
                source_ref: Some("/tasks/w.json".to_owned()),
                source_url: None,
                source_version: Some("v1".to_owned()),
                task_content_hash: "hash-w".to_owned(),
                priority: 3,
                required_capabilities: conflict_keys(["linux"]),
                conflict_keys: conflict_keys(["resource-x"]),
            },
            t(17),
        )
        .unwrap();
    store
        .replace_work_item_dependencies(
            "task-w",
            &["task-a".to_owned(), "task-b".to_owned()],
            t(17),
        )
        .unwrap();
    let sync = store
        .record_source_sync_start("directory", "/tasks", t(18))
        .unwrap();
    store
        .record_source_sync_finish(sync, "ok", 1, None, t(19))
        .unwrap();
}

/// The frozen Python view of a Rust-seeded store equals the Rust view, and
/// shows the Rust-written values decoded the way Python decoded them.
fn assert_python_read_rust_seed(store: &dyn ClaimStore, py: &Value) {
    assert_eq!(&rust_view(store), py);
    assert_eq!(
        py["claims"][0]["claimed_at"],
        "2026-05-28T12:00:00.123456+00:00"
    );
    assert_eq!(py["claims"][0]["version"], 2);
    assert_eq!(py["claims"][0]["worker_id"], "rust-worker");
    assert_eq!(py["events"].as_array().unwrap().len(), 8);
    assert_eq!(py["events"][5]["event_type"], "stolen");
    assert_eq!(py["snapshots"][0]["last_event_id"], 8);
    assert_eq!(
        py["snapshots"][0]["items"][0]["conflict_keys"],
        json!(["resource-x", "resource-y"])
    );
    assert_eq!(py["pause_until_at_20"], "2026-05-28T12:00:36+00:00");
    assert_eq!(
        py["work_items"][0]["conflict_keys_json"],
        "[\"resource-x\"]"
    );
    assert_eq!(py["review_queue"][0]["reason"], "awaiting-approval");
}

#[test]
fn interop_postgres_python_seeds_rust_reads_and_coordinates() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_interop_py");
    load_python_written_postgres(&pg, &schema);
    // The Python store bootstrapped the schema; the Rust store opens it
    // unchanged (same sentinel, idempotent DDL) and reads every row.
    let store = PostgresClaimStore::connect(&pg.dsn(), &schema).unwrap();
    assert_rust_coordinates_on_python_seed(&store);
}

#[test]
fn interop_postgres_rust_seeds_python_reads() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_interop_rs");
    let store = PostgresClaimStore::connect(&pg.dsn(), &schema).unwrap();
    seed_with_rust(&store);
    assert_python_read_rust_seed(&store, &python_golden_view("python_view_of_rust_seed.json"));
}
