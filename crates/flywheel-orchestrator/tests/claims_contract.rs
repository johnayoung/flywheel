//! Contract suite for the orchestrator's claim store, run against both the
//! in-memory and the SQLite backends (a port of the meaningful cases in the
//! Python `test_orchestrator_claims.py`), plus SQLite-only concurrency,
//! migration, and Rust/Python interoperability cases.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
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
}

fn backends() -> Vec<Backend> {
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteClaimStore::open(dir.path().join("claims.db")).unwrap();
    vec![
        Backend {
            name: "memory",
            store: Box::new(InMemoryClaimStore::new()),
            _dir: None,
        },
        Backend {
            name: "sqlite",
            store: Box::new(sqlite),
            _dir: Some(dir),
        },
    ]
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

// --- Rust <-> Python interoperability ---------------------------------------------------------

/// The repository root (the uv workspace) from the crate's manifest dir.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Python script driving the Python `SqliteClaimStore` against a db path:
/// `seed` writes a fixed transcript, `dump` prints the store's view as JSON
/// in the same shape [`rust_view`] produces.
const PY_INTEROP: &str = r#"
import json, sys
from datetime import datetime, timezone
from flywheel_core.task import Task, CommandGrader
from flywheel_orchestrator._claims import SqliteClaimStore, GraphSnapshotItem
from flywheel_orchestrator._sources import WorkItem

def t(second, micro=0):
    return datetime(2026, 5, 28, 12, 0, second, micro, tzinfo=timezone.utc)

def iso(ts):
    return ts.isoformat() if ts is not None else None

mode, path = sys.argv[1], sys.argv[2]
store = SqliteClaimStore(path)

if mode == "seed":
    a = store.acquire_claim("task-a", "py-worker", now=t(0, 123456), lease_seconds=30,
                            conflict_keys=frozenset({"resource-x", "resource-y"}))
    b = store.acquire_claim("task-b", "py-worker", now=t(1), lease_seconds=30)
    a = store.renew_claim(a, now=t(5), lease_seconds=30)
    store.release_claim(b, now=t(6))
    c = store.acquire_claim("task-c", "py-other", now=t(0), lease_seconds=10)
    store.acquire_claim("task-c", "py-worker", now=t(11), lease_seconds=30)
    d = store.acquire_claim("task-d", "py-dead", now=t(0), lease_seconds=5)
    store.sweep_expired_claims(now=t(12))
    store.record_graph_snapshot([
        GraphSnapshotItem(task_id="task-a", source_kind="directory", source_ref="/tasks/a.json",
                          source_url=None, source_version="hash-a", priority=5,
                          required_capabilities=frozenset({"gpu", "linux"}),
                          conflict_keys=frozenset({"resource-x", "resource-y"}),
                          state="running", ready=True, claim_holder="py-worker",
                          resolved_prerequisites=frozenset()),
        GraphSnapshotItem(task_id="task-b", state="blocked", ready=False,
                          resolved_prerequisites=frozenset({"task-a", "task-z"})),
    ], captured_at=t(13))
    store.record_stop_event(kind="dangling-prerequisite", subject="task-b",
                            detail="needs task-z", occurred_at=t(14))
    store.record_human_review(reason="awaiting-approval", task_id="task-a",
                              occurred_at=t(15), run_id="run-1", detail="manual gate")
    store.record_session_pause(reset_at=t(50), now=t(16), ceiling_seconds=20, run_id="run-1")
    task = Task(goal="seed goal", graders=[CommandGrader(run="true")], id="task-w")
    store.upsert_work_item(WorkItem(task=task, source_ref="/tasks/w.json",
                                    source_kind="directory", source_version="v1",
                                    priority=3, required_capabilities=frozenset({"linux"}),
                                    conflict_keys=frozenset({"resource-x"})), now=t(17))
    store.replace_work_item_dependencies("task-w", ["task-a", "task-b", "task-a"], now=t(17))
    sync = store.record_source_sync_start("directory", "/tasks", now=t(18))
    store.record_source_sync_finish(sync, status="ok", observed_count=1, now=t(19))
    store.close()
    sys.exit(0)

view = {
    "claims": [
        {"task_id": c.task_id, "worker_id": c.worker_id, "claimed_at": iso(c.claimed_at),
         "lease_expires_at": iso(c.lease_expires_at), "version": c.version}
        for c in sorted(store.list_claims(), key=lambda c: c.task_id)
    ],
    "events": [
        {"id": e.id, "task_id": e.task_id, "worker_id": e.worker_id, "event_type": e.event_type,
         "version": e.version, "lease_expires_at": iso(e.lease_expires_at),
         "occurred_at": iso(e.occurred_at)}
        for e in store.list_events()
    ],
    "snapshots": [
        {"id": s.id, "captured_at": iso(s.captured_at), "item_count": s.item_count,
         "last_event_id": s.last_event_id,
         "items": [
             {"task_id": i.task_id, "source_kind": i.source_kind, "source_ref": i.source_ref,
              "source_url": i.source_url, "source_version": i.source_version,
              "priority": i.priority, "required_capabilities": sorted(i.required_capabilities),
              "conflict_keys": sorted(i.conflict_keys), "state": i.state, "ready": i.ready,
              "claim_holder": i.claim_holder,
              "resolved_prerequisites": sorted(i.resolved_prerequisites)}
             for i in store.list_graph_snapshot_items(s.id)
         ]}
        for s in store.list_graph_snapshots()
    ],
    "stop_events": [
        {"id": e.id, "kind": e.kind, "subject": e.subject, "detail": e.detail,
         "occurred_at": iso(e.occurred_at), "run_id": e.run_id, "attribution": e.attribution}
        for e in store.list_stop_events()
    ],
    "review_queue": [
        {"id": e.id, "task_id": e.task_id, "run_id": e.run_id, "reason": e.reason,
         "detail": e.detail, "occurred_at": iso(e.occurred_at)}
        for e in store.list_human_review_queue()
    ],
    "pauses": [
        {"id": p.id, "run_id": p.run_id, "reset_at": iso(p.reset_at),
         "pause_until": iso(p.pause_until), "recorded_at": iso(p.recorded_at)}
        for p in store.list_session_pauses()
    ],
    "pause_until_at_20": iso(store.session_pause_until(now=t(20))),
    "work_items": [
        {"task_id": w.task_id, "source_kind": w.source_kind, "source_ref": w.source_ref,
         "source_url": w.source_url, "source_version": w.source_version,
         "task_content_hash": w.task_content_hash, "priority": w.priority,
         "required_capabilities_json": w.required_capabilities_json,
         "conflict_keys_json": w.conflict_keys_json, "first_seen_at": iso(w.first_seen_at),
         "last_seen_at": iso(w.last_seen_at), "disappeared_at": iso(w.disappeared_at),
         "metadata_json": w.metadata_json}
        for w in sorted(store.list_work_items(), key=lambda w: w.task_id)
    ],
    "dependencies": [list(edge) for edge in store.list_work_item_dependencies()],
    "source_syncs": [
        {"id": s.id, "source_kind": s.source_kind, "source_name": s.source_name,
         "started_at": iso(s.started_at), "finished_at": iso(s.finished_at),
         "status": s.status, "observed_count": s.observed_count, "error": s.error,
         "metadata_json": s.metadata_json}
        for s in store.list_source_syncs()
    ],
}
store.close()
print(json.dumps(view, sort_keys=True))
"#;

fn opt_ts(ts: Option<DateTime<Utc>>) -> Value {
    ts.map_or(Value::Null, |ts| Value::String(format_timestamp(&ts)))
}

/// The Rust store's view of a database, in the JSON shape the Python
/// `dump` mode prints.
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

enum Python {
    Ran(String),
    Missing(String),
}

/// Run the interop script through `uv run python` inside the workspace.
/// `Missing` when `uv` is not on PATH; a failing script is a test failure.
fn run_python(mode: &str, db: &Path, script_dir: &Path) -> Python {
    let script = script_dir.join("interop.py");
    std::fs::write(&script, PY_INTEROP).unwrap();
    let output = Command::new("uv")
        .args(["run", "--quiet", "python"])
        .arg(&script)
        .arg(mode)
        .arg(db)
        .current_dir(repo_root())
        .env_remove("VIRTUAL_ENV")
        .output();
    match output {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Python::Missing("`uv` is not on PATH".to_owned())
        }
        Err(err) => panic!("could not spawn uv: {err}"),
        Ok(output) => {
            assert!(
                output.status.success(),
                "python interop `{mode}` failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Python::Ran(String::from_utf8(output.stdout).unwrap())
        }
    }
}

fn python_view(db: &Path, script_dir: &Path) -> Option<Value> {
    match run_python("dump", db, script_dir) {
        Python::Missing(reason) => {
            eprintln!("SKIPPED interop: {reason}");
            None
        }
        Python::Ran(stdout) => Some(serde_json::from_str(stdout.trim()).unwrap()),
    }
}

#[test]
fn interop_python_seeds_rust_reads_and_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("py-seeded.db");
    match run_python("seed", &db, dir.path()) {
        Python::Missing(reason) => {
            eprintln!("SKIPPED interop: {reason}");
            return;
        }
        Python::Ran(_) => {}
    }
    let store = SqliteClaimStore::open(&db).unwrap();

    // Rust reads back exactly what Python wrote.
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

    // The two implementations agree on every row.
    let py = python_view(&db, dir.path()).unwrap();
    assert_eq!(rust_view(&store), py);

    // Rust honours Python's live leases and conflict keys, then steals after
    // expiry; Python sees the Rust-written transition.
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
    let py_after = python_view(&db, dir.path()).unwrap();
    let last = py_after["events"].as_array().unwrap().last().unwrap();
    assert_eq!(last["event_type"], "stolen");
    assert_eq!(last["worker_id"], "rust-worker");
    assert_eq!(last["lease_expires_at"], "2026-05-28T12:01:05+00:00");
    assert_eq!(rust_view(&store), py_after);
}

#[test]
fn interop_rust_seeds_python_reads() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("rust-seeded.db");
    let store = SqliteClaimStore::open(&db).unwrap();
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

    let Some(py) = python_view(&db, dir.path()) else {
        return;
    };
    assert_eq!(rust_view(&store), py);
    // Spot-check the Python-side decoding of Rust-written text.
    assert_eq!(
        py["claims"][0]["claimed_at"],
        "2026-05-28T12:00:00.123456+00:00"
    );
    assert_eq!(py["claims"][0]["version"], 2);
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
