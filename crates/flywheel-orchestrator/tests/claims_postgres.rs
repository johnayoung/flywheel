//! Postgres-specific behaviour of
//! `flywheel_orchestrator::claims_postgres::PostgresClaimStore`.
//!
//! The shared trait contract (and the Rust/Python interop) lives in
//! `claims_contract.rs`, where the Postgres backend joins every case. The
//! cases here pin what only makes sense for this backend: contention across
//! separate connections (ports of the Python `test_postgres_claim_contention`
//! and the advisory-lock cases in `test_orchestrator_claims`), the v3 -> v9
//! in-place migration and the newer-sentinel refusal, concurrent first-open
//! bootstrap safety (`pg_stat_database.deadlocks` must not move), schema
//! isolation and coexistence with the core store, TIMESTAMPTZ / JSONB column
//! typing with temporal expiry, and the factory routing.
//!
//! Every case runs against the throwaway container from
//! `flywheel-core/tests/support/pg_container.rs` and skips, with a message,
//! when Docker is unavailable.

#[path = "../../flywheel-core/tests/support/pg_container.rs"]
mod pg_container;

use std::collections::BTreeSet;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use postgres::Client;

use flywheel_core::lifecycle::Lifecycle;
use flywheel_core::store::{CURRENT_SCHEMA_VERSION, LifecycleStore};
use flywheel_core::store_postgres::PostgresStore;
use flywheel_orchestrator::claims::{
    CURRENT_ORCH_SCHEMA_VERSION, ClaimStore, ClaimStoreError, EVENT_ACQUIRED, EVENT_EXPIRED,
    REASON_AWAITING_APPROVAL, SqliteClaimStore, WorkItemObservation, conflict_keys,
};
use flywheel_orchestrator::claims_postgres::{
    CLAIM_ACQUIRE_LOCK_KEY, POSTGRES_BACKEND, PostgresClaimStore, register_postgres_backends,
    register_postgres_claim_backend,
};
use flywheel_orchestrator::store_factory::{
    PG_DSN_ENV, StoreConfigError, build_claim_store, open_stores, preflight_store,
};
use flywheel_orchestrator::store_registry::{
    Environ, SqliteCoreOpener, StoreConfig, StoreRegistries,
};
use pg_container::Postgres;

fn t(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 28, 12, 0, second).unwrap()
}

fn none() -> BTreeSet<String> {
    BTreeSet::new()
}

fn open(pg: &Postgres, schema: &str) -> PostgresClaimStore {
    PostgresClaimStore::connect(&pg.dsn(), schema).unwrap()
}

fn scalar<T>(client: &mut Client, sql: &str) -> T
where
    T: for<'a> postgres::types::FromSql<'a>,
{
    client.query_one(sql, &[]).unwrap().get(0)
}

// --- Contention across connections ---------------------------------------------------------

#[test]
fn concurrent_acquire_across_connections_admits_exactly_one_winner() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_race");
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let handles: Vec<_> = (0..workers)
        .map(|i| {
            let dsn = pg.dsn();
            let schema = schema.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                // Each worker opens (and bootstraps) its own connection
                // concurrently, then races the acquire from the barrier.
                let store = PostgresClaimStore::connect(&dsn, &schema).unwrap();
                barrier.wait();
                let won = store
                    .acquire_claim("task-hot", &format!("worker-{i}"), t(0), 30.0, &none())
                    .unwrap()
                    .is_some();
                // Distinct tasks sharing one conflict key: the advisory lock
                // serializes the check-then-upsert so only one passes.
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
    let store = open(&pg, &schema);
    assert_eq!(store.list_task_events("task-hot").unwrap().len(), 1);
    assert_eq!(store.list_events().unwrap().len(), 2);
    assert_eq!(store.list_claims().unwrap().len(), 2);
}

#[test]
fn two_claimants_racing_one_task_yield_one_winner_every_time() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_pair");
    let store_a = Arc::new(open(&pg, &schema));
    let store_b = Arc::new(open(&pg, &schema));
    let task_ids: Vec<String> = (0..6).map(|i| format!("t{i}")).collect();
    for task_id in &task_ids {
        let gate = Arc::new(Barrier::new(2));
        let race = |store: Arc<PostgresClaimStore>, worker: &'static str| {
            let gate = Arc::clone(&gate);
            let task_id = task_id.clone();
            thread::spawn(move || {
                gate.wait();
                store
                    .acquire_claim(&task_id, worker, t(0), 3600.0, &none())
                    .unwrap()
            })
        };
        let a = race(Arc::clone(&store_a), "worker-a");
        let b = race(Arc::clone(&store_b), "worker-b");
        let outcomes = [a.join().unwrap(), b.join().unwrap()];
        let winners = outcomes.iter().filter(|o| o.is_some()).count();
        assert_eq!(winners, 1, "{task_id}: {winners} winners");
    }
    assert_eq!(store_a.list_claims().unwrap().len(), task_ids.len());
}

#[test]
fn two_claimants_through_the_factory_split_every_task_exactly_once() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_baton");
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("flywheel.sqlite");
    // Production construction point: a postgres config routes both
    // instances onto postgres and creates no sqlite file.
    let mut registries = sqlite_registries();
    register_postgres_claim_backend(&mut registries).unwrap();
    let environ: Environ = [(PG_DSN_ENV.to_owned(), pg.dsn())].into_iter().collect();
    let config = StoreConfig {
        backend: POSTGRES_BACKEND.to_owned(),
        db_path: db_path.clone(),
        schema: Some(schema.clone()),
    };
    let store_a = build_claim_store(&registries, &config, &environ).unwrap();
    let store_b = build_claim_store(&registries, &config, &environ).unwrap();
    assert!(!db_path.exists());

    // Alternating first mover per task: the first mover must win and the
    // peer, contending the just-granted live lease, must be refused.
    let task_ids: Vec<String> = (0..6).map(|i| format!("t{i}")).collect();
    let mut won_a = BTreeSet::new();
    let mut won_b = BTreeSet::new();
    for (round, task_id) in task_ids.iter().enumerate() {
        let (first, first_name, peer, peer_name) = if round % 2 == 0 {
            (&store_a, "worker-a", &store_b, "worker-b")
        } else {
            (&store_b, "worker-b", &store_a, "worker-a")
        };
        let granted = first
            .acquire_claim(task_id, first_name, t(0), 3600.0, &none())
            .unwrap();
        let refused = peer
            .acquire_claim(task_id, peer_name, t(0), 3600.0, &none())
            .unwrap();
        let granted = granted.unwrap_or_else(|| panic!("{task_id}: the first mover was refused"));
        assert!(
            refused.is_none(),
            "{task_id}: the peer double-claimed a live lease"
        );
        if granted.worker_id == "worker-a" {
            won_a.insert(task_id.clone());
        } else {
            won_b.insert(task_id.clone());
        }
    }
    assert!(!won_a.is_empty() && !won_b.is_empty());
    assert!(won_a.is_disjoint(&won_b));
    let all: BTreeSet<String> = won_a.union(&won_b).cloned().collect();
    assert_eq!(all, task_ids.iter().cloned().collect());
    assert_eq!(store_a.list_claims().unwrap().len(), task_ids.len());
    assert!(!db_path.exists());
}

#[test]
fn acquire_blocks_while_another_session_holds_the_acquire_lock() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_lock");
    let store = Arc::new(open(&pg, &schema));
    // Hold the shared acquire lock in a live transaction on a separate
    // session; the store's acquire path must block on it.
    let mut holder = pg.admin();
    let mut held = holder.transaction().unwrap();
    held.execute(
        "SELECT pg_advisory_xact_lock($1)",
        &[&CLAIM_ACQUIRE_LOCK_KEY],
    )
    .unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = {
        let store = Arc::clone(&store);
        thread::spawn(move || {
            let claim = store.acquire_claim("task-a", "worker-1", t(0), 30.0, &none());
            sender.send(claim).unwrap();
        })
    };
    assert!(
        matches!(
            receiver.recv_timeout(Duration::from_millis(1500)),
            Err(RecvTimeoutError::Timeout)
        ),
        "acquire_claim returned while the advisory lock was held; the acquire path is not \
         serialized on Postgres"
    );
    // Release the lock; acquire must now complete and succeed.
    held.commit().unwrap();
    let claim = receiver
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap()
        .expect("free task is claimable once the lock lifts");
    assert_eq!(claim.worker_id, "worker-1");
    worker.join().unwrap();
}

#[test]
fn sweep_journals_reaped_claims_in_task_id_order_in_one_statement() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let store = open(&pg, &pg.fresh_schema("fw_sweep"));
    for (task, at) in [("task-c", 0), ("task-a", 1), ("task-b", 2)] {
        store
            .acquire_claim(task, "worker-dead", t(at), 5.0, &none())
            .unwrap()
            .unwrap();
    }
    store
        .acquire_claim("task-live", "worker-live", t(0), 3600.0, &none())
        .unwrap()
        .unwrap();
    assert_eq!(
        store.sweep_expired_claims(t(10)).unwrap(),
        vec!["task-a", "task-b", "task-c"]
    );
    let expired: Vec<(i64, String)> = store
        .list_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == EVENT_EXPIRED)
        .map(|e| (e.id, e.task_id))
        .collect();
    assert_eq!(expired.len(), 3);
    let ids: Vec<i64> = expired.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![5, 6, 7]);
    let order: Vec<&str> = expired.iter().map(|(_, task)| task.as_str()).collect();
    assert_eq!(order, vec!["task-a", "task-b", "task-c"]);
    assert_eq!(
        store
            .list_claims()
            .unwrap()
            .iter()
            .map(|c| c.task_id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-live"]
    );
}

// --- Schema: migration, sentinel guard, bootstrap concurrency ------------------------------

/// Materialize a schema-v3 orchestrator store (pre-ledger) plus a v6
/// stop-events table that predates the `run_id` / `attribution` columns,
/// seeded with rows -- the Python `_build_v3_postgres_store` fixture.
fn build_v3_schema(pg: &Postgres, schema: &str) {
    let mut client = pg.admin();
    client
        .batch_execute(&format!(
            "CREATE SCHEMA \"{schema}\"; SET search_path TO \"{schema}\", public;
             CREATE TABLE task_claims (
               task_id            TEXT PRIMARY KEY,
               worker_id          TEXT NOT NULL,
               claimed_at         TIMESTAMPTZ NOT NULL,
               lease_expires_at   TIMESTAMPTZ NOT NULL,
               version            INTEGER NOT NULL,
               conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb
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
               required_capabilities_json JSONB NOT NULL DEFAULT '[]'::jsonb,
               conflict_keys_json         JSONB NOT NULL DEFAULT '[]'::jsonb,
               first_seen_at              TIMESTAMPTZ NOT NULL,
               last_seen_at               TIMESTAMPTZ NOT NULL,
               disappeared_at             TIMESTAMPTZ,
               metadata_json              JSONB NOT NULL DEFAULT '{{}}'::jsonb
             );
             CREATE TABLE orchestrator_stop_events (
               id          BIGSERIAL PRIMARY KEY,
               kind        TEXT NOT NULL,
               subject     TEXT NOT NULL,
               detail      TEXT NOT NULL,
               occurred_at TIMESTAMPTZ NOT NULL
             );
             INSERT INTO task_claims (task_id, worker_id, claimed_at, lease_expires_at, version,
               conflict_keys_json) VALUES ('task-a', 'worker-1', '2026-05-28T12:00:00+00:00',
               '2026-05-28T12:00:30+00:00', 1, '[]'::jsonb);
             INSERT INTO work_items (task_id, first_seen_at, last_seen_at) VALUES ('task-a',
               '2026-05-28T12:00:00+00:00', '2026-05-28T12:00:00+00:00');
             INSERT INTO orchestrator_stop_events (kind, subject, detail, occurred_at) VALUES
               ('no-op-cycle', '/repo', 'nothing', '2026-05-28T12:00:00+00:00');
             INSERT INTO orchestrator_schema_version (id, version) VALUES (1, 3);"
        ))
        .unwrap();
}

#[test]
fn v3_schema_migrates_forward_keeping_rows_with_empty_ledgers() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_v3");
    build_v3_schema(&pg, &schema);
    let store = open(&pg, &schema);
    let claim = store.load_claim("task-a").unwrap().unwrap();
    assert_eq!(claim.worker_id, "worker-1");
    assert_eq!(claim.lease_expires_at, t(30));
    assert!(store.load_work_item("task-a").unwrap().is_some());
    assert!(store.list_events().unwrap().is_empty());
    assert!(store.list_graph_snapshots().unwrap().is_empty());
    assert!(store.list_session_pauses().unwrap().is_empty());
    // The pre-v7/v9 stop row survives with the added columns at their
    // defaults.
    let stops = store.list_stop_events().unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].run_id, "");
    assert_eq!(stops[0].attribution, "");
    // The migrated store is fully functional and its sentinel converged.
    store
        .record_human_review(REASON_AWAITING_APPROVAL, "task-a", t(1), "run-1", "")
        .unwrap();
    assert_eq!(store.list_human_review_queue().unwrap().len(), 1);
    let mut admin = pg.admin_in(&schema);
    let version: i32 = scalar(
        &mut admin,
        "SELECT version FROM orchestrator_schema_version WHERE id = 1",
    );
    assert_eq!(version, CURRENT_ORCH_SCHEMA_VERSION as i32);
    // A second open is a clean no-op on the converged schema.
    drop(store);
    let reopened = open(&pg, &schema);
    assert_eq!(reopened.list_stop_events().unwrap().len(), 2);
}

#[test]
fn newer_sentinel_is_refused_without_being_rewritten() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_future");
    pg.admin()
        .batch_execute(&format!(
            "CREATE SCHEMA \"{schema}\"; SET search_path TO \"{schema}\", public;
             CREATE TABLE orchestrator_schema_version (
               id INTEGER PRIMARY KEY CHECK (id = 1), version INTEGER NOT NULL);
             INSERT INTO orchestrator_schema_version (id, version) VALUES (1, 99);"
        ))
        .unwrap();
    let err = PostgresClaimStore::connect(&pg.dsn(), &schema).unwrap_err();
    assert!(err.is_schema_mismatch(), "{err}");
    match err {
        ClaimStoreError::Schema(schema_err) => {
            assert_eq!(schema_err.observed, Some(99));
            assert_eq!(schema_err.expected, CURRENT_ORCH_SCHEMA_VERSION);
        }
        other => panic!("unexpected {other:?}"),
    }
    let mut admin = pg.admin_in(&schema);
    let version: i32 = scalar(
        &mut admin,
        "SELECT version FROM orchestrator_schema_version WHERE id = 1",
    );
    assert_eq!(version, 99, "a newer sentinel is never downgraded");
}

#[test]
fn concurrent_fresh_schema_opens_provision_once_without_deadlocking() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_boot");
    let mut admin = pg.admin();
    let deadlocks_sql = "SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()";
    let before: i64 = scalar(&mut admin, deadlocks_sql);
    let openers = 8;
    let barrier = Arc::new(Barrier::new(openers));
    let handles: Vec<_> = (0..openers)
        .map(|_| {
            let dsn = pg.dsn();
            let schema = schema.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                PostgresClaimStore::connect(&dsn, &schema).map(|_| ())
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    // The server's cumulative counter moves even when a client swallows a
    // deadlock and retries, so asserting it defeats catch-and-retry masking.
    // pg_stat views lag a little behind commit; settle before reading.
    thread::sleep(Duration::from_millis(500));
    let after: i64 = scalar(&mut admin, deadlocks_sql);
    assert_eq!(after, before, "concurrent bootstrap deadlocked");
    let store = open(&pg, &schema);
    assert!(
        store
            .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
            .unwrap()
            .is_some()
    );
}

// --- Isolation, coexistence, column typing ---------------------------------------------------

#[test]
fn schemas_are_isolated_and_the_claim_store_coexists_with_the_core_store() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema_a = pg.fresh_schema("fw_iso_a");
    let schema_b = pg.fresh_schema("fw_iso_b");
    let core = PostgresStore::connect(&pg.dsn(), &schema_a).unwrap();
    core.create_lifecycle(&Lifecycle::new("task-a").with_run_id("run-1"))
        .unwrap();
    let claims_a = open(&pg, &schema_a);
    let claims_b = open(&pg, &schema_b);
    claims_a
        .acquire_claim("task-a", "worker-1", t(0), 30.0, &conflict_keys(["k"]))
        .unwrap()
        .unwrap();
    // The other schema sees neither the claim nor its conflict key.
    assert_eq!(claims_b.load_claim("task-a").unwrap(), None);
    assert!(
        claims_b
            .acquire_claim("task-b", "worker-2", t(1), 30.0, &conflict_keys(["k"]))
            .unwrap()
            .is_some()
    );
    // The core store's rows and sentinel are untouched by the claim store's
    // bootstrap in the same schema, and vice versa.
    assert!(core.load_lifecycle("run-1").unwrap().is_some());
    let mut admin = pg.admin_in(&schema_a);
    let core_version: i32 = scalar(
        &mut admin,
        "SELECT version FROM schema_version WHERE id = 1",
    );
    assert_eq!(core_version, CURRENT_SCHEMA_VERSION as i32);
    let orch_version: i32 = scalar(
        &mut admin,
        "SELECT version FROM orchestrator_schema_version WHERE id = 1",
    );
    assert_eq!(orch_version, CURRENT_ORCH_SCHEMA_VERSION as i32);
    assert_eq!(claims_a.schema(), schema_a);
}

#[test]
fn columns_are_timestamptz_and_jsonb_and_the_queue_index_exists() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_types");
    let _store = open(&pg, &schema);
    let mut admin = pg.admin();
    let typed: Vec<(String, String, String)> = admin
        .query(
            "SELECT table_name, column_name, data_type FROM information_schema.columns \
             WHERE table_schema = $1 AND (column_name LIKE '%_at' OR column_name LIKE '%_json' \
             OR column_name = 'pause_until' OR column_name = 'ready') \
             ORDER BY table_name, column_name",
            &[&schema],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert!(!typed.is_empty());
    for (table, column, data_type) in &typed {
        let expected = if column.ends_with("_json") {
            "jsonb"
        } else if column == "ready" {
            "boolean"
        } else {
            "timestamp with time zone"
        };
        assert_eq!(data_type, expected, "{table}.{column}");
    }
    let indexes: Vec<String> = admin
        .query(
            "SELECT indexname FROM pg_indexes WHERE schemaname = $1 ORDER BY indexname",
            &[&schema],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    for expected in [
        "idx_orchestrator_events_task",
        "idx_orchestrator_stop_events_subject",
        "idx_orchestrator_stop_events_kind",
        "idx_orchestrator_session_pauses_pause_until",
        "idx_graph_snapshot_items_snapshot",
        "idx_work_item_dependencies_prerequisite",
    ] {
        assert!(indexes.iter().any(|i| i == expected), "{expected} missing");
    }
}

#[test]
fn expiry_is_decided_temporally_across_offsets() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_temporal");
    let store = open(&pg, &schema);
    // A row written with a +02:00 offset by another client: 14:00:30+02 is
    // 12:00:30Z, so it must block a conflicting acquire at :10 and be reaped
    // at :30 -- decisions no text comparison of the literals would reach.
    pg.admin_in(&schema)
        .batch_execute(
            "INSERT INTO task_claims (task_id, worker_id, claimed_at, lease_expires_at, \
             version, conflict_keys_json) VALUES ('held', 'w1', '2026-05-28 14:00:00+02', \
             '2026-05-28 14:00:30+02', 1, '[\"k\"]'::jsonb)",
        )
        .unwrap();
    assert_eq!(
        store.load_claim("held").unwrap().unwrap().lease_expires_at,
        t(30)
    );
    assert!(
        store
            .acquire_claim("other", "w2", t(10), 30.0, &conflict_keys(["k"]))
            .unwrap()
            .is_none()
    );
    assert!(store.sweep_expired_claims(t(10)).unwrap().is_empty());
    assert_eq!(store.sweep_expired_claims(t(30)).unwrap(), vec!["held"]);
    assert_eq!(
        store
            .list_task_events("held")
            .unwrap()
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![EVENT_EXPIRED]
    );
}

#[test]
fn non_ascii_set_columns_read_back_as_the_canonical_encoding() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let store = open(&pg, &pg.fresh_schema("fw_utf8"));
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteClaimStore::open(dir.path().join("claims.db")).unwrap();
    let observation = WorkItemObservation {
        task_id: "task-a".to_owned(),
        task_content_hash: "hash".to_owned(),
        required_capabilities: conflict_keys(["caf\u{e9}", "linux"]),
        conflict_keys: conflict_keys(["r\u{e9}sum\u{e9}"]),
        ..WorkItemObservation::default()
    };
    for backend in [&store as &dyn ClaimStore, &sqlite] {
        backend.upsert_work_item(&observation, t(0)).unwrap();
    }
    let from_pg = store.load_work_item("task-a").unwrap().unwrap();
    let from_sqlite = sqlite.load_work_item("task-a").unwrap().unwrap();
    assert_eq!(
        from_pg.required_capabilities_json,
        "[\"caf\\u00e9\", \"linux\"]"
    );
    assert_eq!(
        from_pg.required_capabilities_json,
        from_sqlite.required_capabilities_json
    );
    assert_eq!(from_pg.conflict_keys_json, from_sqlite.conflict_keys_json);
    // The live conflict check decodes the same key on the way back in.
    store
        .acquire_claim(
            "task-a",
            "w1",
            t(0),
            30.0,
            &conflict_keys(["r\u{e9}sum\u{e9}"]),
        )
        .unwrap()
        .unwrap();
    assert!(
        store
            .acquire_claim(
                "task-b",
                "w2",
                t(1),
                30.0,
                &conflict_keys(["r\u{e9}sum\u{e9}"])
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .list_task_events("task-a")
            .unwrap()
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![EVENT_ACQUIRED]
    );
}

// --- Factory routing ---------------------------------------------------------------------------

fn sqlite_registries() -> StoreRegistries {
    let opener: SqliteCoreOpener = Arc::new(|path: &std::path::Path| {
        flywheel_core::store_sqlite::SqliteStore::open(path)
            .map(|store| Arc::new(store) as Arc<dyn flywheel_core::store::HarnessStore>)
    });
    StoreRegistries::with_sqlite(opener)
}

#[test]
fn open_stores_routes_both_families_onto_one_postgres_schema() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_bundle");
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("never.sqlite");
    let mut registries = sqlite_registries();
    register_postgres_backends(&mut registries).unwrap();
    let environ: Environ = [(PG_DSN_ENV.to_owned(), pg.dsn())].into_iter().collect();
    let config = StoreConfig {
        backend: POSTGRES_BACKEND.to_owned(),
        db_path: db_path.clone(),
        schema: Some(schema.clone()),
    };
    preflight_store(&registries, &config, &environ).unwrap();
    let bundle = open_stores(&registries, &config, &environ).unwrap();
    bundle
        .core
        .create_lifecycle(&Lifecycle::new("task-a").with_run_id("run-1"))
        .unwrap();
    let claim = bundle
        .claims
        .acquire_claim("task-a", "worker-1", t(0), 30.0, &none())
        .unwrap()
        .unwrap();
    assert_eq!(claim.version, 1);
    // Both landed in the one schema; nothing touched sqlite.
    let mut admin = pg.admin_in(&schema);
    let runs: i64 = scalar(&mut admin, "SELECT COUNT(*) FROM lifecycles");
    let claims: i64 = scalar(&mut admin, "SELECT COUNT(*) FROM task_claims");
    assert_eq!((runs, claims), (1, 1));
    assert!(!db_path.exists());
    // Without a DSN the same config fails fast, naming the variables only.
    let err = preflight_store(&registries, &config, &Environ::new()).unwrap_err();
    assert!(matches!(err, StoreConfigError::MissingDsn { .. }), "{err}");
}
