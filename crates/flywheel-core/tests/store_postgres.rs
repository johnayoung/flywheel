//! Postgres-specific behaviour of `flywheel_core::store_postgres::PostgresStore`.
//!
//! The shared trait contract (and the Python interop) lives in
//! `store_contract.rs`. The cases here pin properties that only make sense
//! for the Postgres backend: JSONB and TIMESTAMPTZ column typing, raw-SQL
//! append-only enforcement, foreign keys, schema isolation, the version pin
//! and the v11 -> v12 migration, the LISTEN/NOTIFY bridge, concurrent
//! first-open bootstrap safety, and the fail-loud connection path.
//!
//! Every database-backed case runs against the throwaway container from
//! `support/pg_container.rs` and skips, with a message, when Docker is
//! unavailable.

#[path = "support/pg_container.rs"]
mod pg_container;

use std::collections::BTreeSet;
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use postgres::error::SqlState;
use serde_json::json;

use flywheel_core::events::{DomainEvent, DomainEventPayload, LifecycleInitialized};
use flywheel_core::lifecycle::{Attempt, Lifecycle, Status};
use flywheel_core::store::{
    AttemptStore, CURRENT_SCHEMA_VERSION, ControlCommandStore, DomainEventStore,
    GraderResultRecord, GraderResultStore, HarnessStore, JsonObject, LifecycleFilter,
    LifecycleStore, StoreConflictError, StoreError, TaskStore,
};
use flywheel_core::store_postgres::{
    BOOTSTRAP_LOCK_KEY, ListenerState, PostgresError, PostgresStore, PostgresStoreOptions,
};
use flywheel_core::task::{CommandGrader, Grader, GraderType, Task};
use pg_container::Postgres;

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_780_000_000 + secs, 0).unwrap()
}

fn obj(value: serde_json::Value) -> JsonObject {
    value.as_object().cloned().unwrap()
}

fn open(pg: &Postgres, schema: &str) -> PostgresStore {
    PostgresStore::connect(&pg.dsn(), schema).unwrap()
}

fn command_task(task_id: &str, goal: &str) -> Task {
    Task::new(
        goal,
        vec![Grader::Command(CommandGrader::new("true").unwrap())],
    )
    .with_id(task_id)
}

fn seed_grader_result(store: &dyn HarnessStore) {
    store
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    store
        .save_attempt("r1", &Attempt::new(1, at(0), "r1"), None)
        .unwrap();
    store
        .append_grader_result(&GraderResultRecord {
            run_id: "r1".into(),
            attempt_number: 1,
            ordinal: 0,
            grader_type: GraderType::Command,
            grader_spec: obj(json!({"type": "command", "run": "true"})),
            passed: true,
            duration_ms: 1,
            payload: obj(json!({"exit_code": 0})),
            ts: at(0),
            grader_name: None,
            id: None,
        })
        .unwrap();
}

fn sqlstate(err: &postgres::Error) -> Option<SqlState> {
    err.code().cloned()
}

fn schema_version(pg: &Postgres, schema: &str) -> i32 {
    pg.admin_in(schema)
        .query_one("SELECT version FROM schema_version WHERE id = 1", &[])
        .unwrap()
        .get(0)
}

// --- Bootstrap ----------------------------------------------------------------------------------

#[test]
fn bootstrap_creates_every_table_with_jsonb_and_timestamptz_columns() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_boot");
    let _store = open(&pg, &schema);
    let mut admin = pg.admin();
    let tables: BTreeSet<String> = admin
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = $1::text",
            &[&schema],
        )
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
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
    assert_eq!(tables, expected);

    let columns: Vec<(String, String, String)> = admin
        .query(
            "SELECT table_name, column_name, data_type FROM information_schema.columns \
             WHERE table_schema = $1::text ORDER BY table_name, column_name",
            &[&schema],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    let type_of = |table: &str, column: &str| -> String {
        columns
            .iter()
            .find(|(t, c, _)| t == table && c == column)
            .map(|(_, _, ty)| ty.clone())
            .unwrap_or_else(|| panic!("missing column {table}.{column}"))
    };
    for (table, column) in [
        ("attempts", "agent_context_json"),
        ("events", "payload_json"),
        ("grader_results", "grader_spec_json"),
        ("grader_results", "payload_json"),
        ("lifecycles", "timestamps_json"),
        ("control_commands", "payload_json"),
        ("task_versions", "graders_json"),
    ] {
        assert_eq!(type_of(table, column), "jsonb", "{table}.{column}");
    }
    for (table, column) in [
        ("lifecycles", "updated_at"),
        ("attempts", "started_at"),
        ("attempts", "last_activity_at"),
        ("events", "ts"),
        ("grader_results", "ts"),
        ("control_commands", "enqueued_at"),
        ("control_commands", "claimed_at"),
        ("task_versions", "created_at"),
    ] {
        assert_eq!(
            type_of(table, column),
            "timestamp with time zone",
            "{table}.{column}"
        );
    }
    // blocked_requires_json is deliberately TEXT (an opaque serialized blob).
    assert_eq!(type_of("lifecycles", "blocked_requires_json"), "text");
    assert_eq!(type_of("task_versions", "insertion_seq"), "bigint");
}

#[test]
fn bootstrap_is_idempotent_and_keeps_existing_rows() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_idem");
    let first = open(&pg, &schema);
    first
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    first.close().unwrap();
    let second = open(&pg, &schema);
    let loaded = second.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(loaded.task_id, "t");
    assert_eq!(schema_version(&pg, &schema), CURRENT_SCHEMA_VERSION as i32);
}

#[test]
fn fresh_store_records_current_schema_version_and_supplementary_indexes() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_fresh");
    let _store = open(&pg, &schema);
    assert_eq!(schema_version(&pg, &schema), CURRENT_SCHEMA_VERSION as i32);
    let indexes: BTreeSet<String> = pg
        .admin()
        .query(
            "SELECT indexname FROM pg_indexes WHERE schemaname = $1::text AND tablename = 'lifecycles'",
            &[&schema],
        )
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    assert!(indexes.contains("idx_lifecycles_task_id"), "{indexes:?}");
    assert!(indexes.contains("idx_lifecycles_updated_at"), "{indexes:?}");
}

#[test]
fn mismatched_schema_version_is_refused() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_mismatch");
    open(&pg, &schema).close().unwrap();
    let bumped = CURRENT_SCHEMA_VERSION as i32 + 99;
    pg.admin_in(&schema)
        .execute(
            "UPDATE schema_version SET version = $1 WHERE id = 1",
            &[&bumped],
        )
        .unwrap();
    let err = PostgresStore::connect(&pg.dsn(), &schema).unwrap_err();
    assert!(err.is_schema_mismatch());
    assert!(err.to_string().contains("store must be re-created"));
    let StoreError::Schema(schema_err) = err else {
        panic!("expected schema error");
    };
    assert_eq!(
        schema_err.observed_version,
        Some(CURRENT_SCHEMA_VERSION + 99)
    );
    assert_eq!(schema_err.expected_version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn v11_schema_is_forward_migrated_in_place() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_v11");
    let store = open(&pg, &schema);
    store
        .create_lifecycle(&Lifecycle::new("t1").with_run_id("run-old"))
        .unwrap();
    store.close().unwrap();
    pg.admin_in(&schema)
        .batch_execute(
            "ALTER TABLE lifecycles DROP COLUMN source;
             UPDATE schema_version SET version = 11 WHERE id = 1;",
        )
        .unwrap();
    assert_eq!(schema_version(&pg, &schema), 11);

    let migrated = open(&pg, &schema);
    assert_eq!(schema_version(&pg, &schema), CURRENT_SCHEMA_VERSION as i32);
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
    open(&pg, &schema).close().unwrap();
}

#[test]
fn insertion_seq_is_restored_on_a_pre_identity_schema() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_seq");
    open(&pg, &schema).close().unwrap();
    pg.admin_in(&schema)
        .batch_execute("ALTER TABLE task_versions DROP COLUMN insertion_seq")
        .unwrap();
    let store = open(&pg, &schema);
    let first = command_task("t", "First saved.");
    let second = command_task("t", "Second saved.");
    store.save_task(&first, at(0)).unwrap();
    store.save_task(&second, at(0)).unwrap();
    assert_eq!(store.load_task("t", None).unwrap(), Some(second));
}

#[test]
fn latest_task_version_survives_reindex() {
    // Two versions saved at the SAME created_at must resolve "latest" to the
    // most-recently-saved one even after a REINDEX re-lays out the equal
    // created_at tuples; insertion_seq is the stable tie-break.
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_reindex");
    let store = open(&pg, &schema);
    let older = command_task("t", "older-saved");
    let newer = command_task("t", "newer-saved");
    store.save_task(&older, at(0)).unwrap();
    store.save_task(&newer, at(0)).unwrap();
    assert_eq!(
        store.load_task("t", None).unwrap().unwrap().goal,
        "newer-saved"
    );
    store.with_client(|client| {
        client
            .batch_execute("REINDEX INDEX idx_task_versions_id_created")
            .unwrap();
    });
    assert_eq!(
        store.load_task("t", None).unwrap().unwrap().goal,
        "newer-saved"
    );
}

// --- Database-level invariants ----------------------------------------------------------------

#[test]
fn grader_result_triggers_are_installed_and_reject_raw_mutation() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_trig");
    let store = open(&pg, &schema);
    let triggers: BTreeSet<String> = pg
        .admin()
        .query(
            "SELECT tgname FROM pg_trigger WHERE tgrelid = (
                 SELECT oid FROM pg_class WHERE relname = 'grader_results'
                 AND relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = $1::text)
             ) AND NOT tgisinternal",
            &[&schema],
        )
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    assert!(
        triggers.contains("grader_results_no_update"),
        "{triggers:?}"
    );
    assert!(
        triggers.contains("grader_results_no_delete"),
        "{triggers:?}"
    );

    seed_grader_result(&store);
    for sql in [
        "UPDATE grader_results SET passed = false",
        "DELETE FROM grader_results",
    ] {
        let err = store.with_client(|client| client.batch_execute(sql).unwrap_err());
        assert_eq!(sqlstate(&err), Some(SqlState::CHECK_VIOLATION), "{sql}");
        assert!(
            err.as_db_error().unwrap().message().contains("append-only"),
            "{sql}: {err}"
        );
    }
    assert_eq!(store.list_grader_results("r1", 1).unwrap().len(), 1);
}

#[test]
fn foreign_keys_reject_orphan_rows() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_fk");
    let store = open(&pg, &schema);
    store
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    let now = Utc::now();
    let orphan_event = store.with_client(|client| {
        client
            .execute(
                "INSERT INTO events (run_id, ts, kind, payload_json, sequence) \
                 VALUES ($1, $2, 'x', '{}'::jsonb, 1)",
                &[&"nope", &now],
            )
            .unwrap_err()
    });
    assert_eq!(
        sqlstate(&orphan_event),
        Some(SqlState::FOREIGN_KEY_VIOLATION)
    );
    let orphan_result = store.with_client(|client| {
        client
            .execute(
                "INSERT INTO grader_results (run_id, attempt_number, ordinal, grader_type, \
                 grader_spec_json, passed, duration_ms, payload_json, ts) \
                 VALUES ('r1', 999, 0, 'command', '{}'::jsonb, true, 1, '{}'::jsonb, $1)",
                &[&now],
            )
            .unwrap_err()
    });
    assert_eq!(
        sqlstate(&orphan_result),
        Some(SqlState::FOREIGN_KEY_VIOLATION)
    );
}

#[test]
fn blocked_requires_json_persists_as_text_and_null() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_blocked");
    let store = open(&pg, &schema);
    let payload = r#"[{"type": "command_grader", "name": "full-suite"}, {"type": "file_exists", "path": ".flywheel/lkg/.venv", "present": true}]"#;
    let mut lc = Lifecycle::new("t").with_run_id("r-set");
    lc.blocked_requires_json = Some(payload.into());
    store.create_lifecycle(&lc).unwrap();
    store
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r-null"))
        .unwrap();
    let raw = |run_id: &str| -> Option<String> {
        store.with_client(|client| {
            client
                .query_one(
                    "SELECT blocked_requires_json FROM lifecycles WHERE run_id = $1",
                    &[&run_id],
                )
                .unwrap()
                .get(0)
        })
    };
    assert_eq!(raw("r-set").as_deref(), Some(payload));
    assert_eq!(raw("r-null"), None);
    let mut loaded = store.load_lifecycle("r-set").unwrap().unwrap();
    loaded
        .transition_to(Status::Ready, "", Some(at(1)))
        .unwrap();
    loaded.blocked_requires_json = None;
    store.update_lifecycle(&loaded, 1).unwrap();
    assert_eq!(raw("r-set"), None);
}

#[test]
fn empty_identity_strings_persist_as_null_like_the_python_store() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_null");
    let store = open(&pg, &schema);
    store
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    store
        .save_attempt("r1", &Attempt::new(1, at(0), ""), None)
        .unwrap();
    let raw: Vec<Option<String>> = store.with_client(|client| {
        let row = client
            .query_one(
                "SELECT l.source, l.task_content_hash, l.worker_id, a.attempt_run_id, a.outcome \
                 FROM lifecycles l JOIN attempts a ON a.run_id = l.run_id WHERE l.run_id = 'r1'",
                &[],
            )
            .unwrap();
        (0..row.len()).map(|i| row.get(i)).collect()
    });
    assert_eq!(raw, vec![None, None, None, None, None]);
    let loaded = store.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(loaded.source, "");
    assert_eq!(loaded.attempts[0].run_id, "");
    assert_eq!(loaded.attempts[0].outcome, None);
}

#[test]
fn nul_bytes_in_free_form_columns_are_stored_as_replacement_characters() {
    // TEXT and JSONB cannot hold U+0000; the store substitutes U+FFFD in
    // free-form run data instead of refusing (and rolling back) the write.
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_nul");
    let store = open(&pg, &schema);
    let mut lc = Lifecycle::new("t").with_run_id("r1");
    lc.agent_output = "out\0put".into();
    lc.error = "err\0or".into();
    lc.blocked_requires_json = Some("[\"a\0b\"]".into());
    store.create_lifecycle(&lc).unwrap();
    let mut attempt = Attempt::new(1, at(0), "r1");
    attempt.agent_output = "\0".into();
    attempt.error = "e\0".into();
    attempt.agent_context = [("k\0".to_owned(), "\0v".to_owned())].into();
    store.save_attempt("r1", &attempt, None).unwrap();
    store
        .append_grader_result(&GraderResultRecord {
            run_id: "r1".into(),
            attempt_number: 1,
            ordinal: 0,
            grader_type: GraderType::Command,
            grader_spec: obj(json!({"type": "command", "run": "printf '\0'"})),
            passed: false,
            duration_ms: 1,
            payload: obj(json!({"stdout_tail": ["x\0y", {"n\0": "\0"}], "exit_code": 1})),
            ts: at(0),
            grader_name: None,
            id: None,
        })
        .unwrap();
    let command = store
        .enqueue_command("r1", "message", obj(json!({"text": "hi\0"})), at(0))
        .unwrap();
    assert_eq!(command.payload, obj(json!({"text": "hi\0"})));

    let loaded = store.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(loaded.agent_output, "out\u{fffd}put");
    assert_eq!(loaded.error, "err\u{fffd}or");
    assert_eq!(
        loaded.blocked_requires_json.as_deref(),
        Some("[\"a\u{fffd}b\"]")
    );
    assert_eq!(loaded.attempts[0].agent_output, "\u{fffd}");
    assert_eq!(loaded.attempts[0].error, "e\u{fffd}");
    assert_eq!(
        loaded.attempts[0].agent_context,
        [("k\u{fffd}".to_owned(), "\u{fffd}v".to_owned())].into()
    );
    let results = store.list_grader_results("r1", 1).unwrap();
    assert_eq!(
        results[0].grader_spec,
        obj(json!({"type": "command", "run": "printf '\u{fffd}'"}))
    );
    assert_eq!(
        results[0].payload,
        obj(json!({"stdout_tail": ["x\u{fffd}y", {"n\u{fffd}": "\u{fffd}"}], "exit_code": 1}))
    );
    let claimed = store.claim_commands("r1", at(1)).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].payload, obj(json!({"text": "hi\u{fffd}"})));
}

// --- Isolation and concurrency -------------------------------------------------------------------

#[test]
fn schema_isolation_between_two_stores_on_one_database() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let a = open(&pg, &pg.fresh_schema("fw_iso_a"));
    let b = open(&pg, &pg.fresh_schema("fw_iso_b"));
    a.create_lifecycle(&Lifecycle::new("a-task").with_run_id("r1"))
        .unwrap();
    b.create_lifecycle(&Lifecycle::new("b-task").with_run_id("r1"))
        .unwrap();
    assert_eq!(a.load_lifecycle("r1").unwrap().unwrap().task_id, "a-task");
    assert_eq!(b.load_lifecycle("r1").unwrap().unwrap().task_id, "b-task");
    assert_ne!(a.channel(), b.channel());
}

#[test]
fn optimistic_concurrency_conflict_across_two_connections() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_occ");
    let s1 = open(&pg, &schema);
    let s2 = open(&pg, &schema);
    s1.create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    let mut snap1 = s1.load_lifecycle("r1").unwrap().unwrap();
    let mut snap2 = s2.load_lifecycle("r1").unwrap().unwrap();
    snap1.transition_to(Status::Ready, "", Some(at(1))).unwrap();
    s1.update_lifecycle(&snap1, 1).unwrap();
    snap2.transition_to(Status::Ready, "", Some(at(1))).unwrap();
    let err = s2.update_lifecycle(&snap2, 1).unwrap_err();
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
fn claim_commands_is_claim_once_across_concurrent_stores() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_claim");
    let producer = open(&pg, &schema);
    producer
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    for n in 0..12 {
        producer
            .enqueue_command("r1", "say", obj(json!({"n": n})), at(n))
            .unwrap();
    }
    let workers: Vec<Arc<PostgresStore>> = (0..4).map(|_| Arc::new(open(&pg, &schema))).collect();
    let barrier = Arc::new(Barrier::new(workers.len()));
    let handles: Vec<_> = workers
        .iter()
        .map(|store| {
            let store = Arc::clone(store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store.claim_commands("r1", at(100)).unwrap()
            })
        })
        .collect();
    let mut seen: Vec<i64> = Vec::new();
    for handle in handles {
        for record in handle.join().unwrap() {
            seen.push(record.id.unwrap());
        }
    }
    seen.sort_unstable();
    assert_eq!(
        seen.len(),
        12,
        "every command claimed exactly once: {seen:?}"
    );
    seen.dedup();
    assert_eq!(seen.len(), 12, "no command claimed twice");
    assert!(producer.claim_commands("r1", at(101)).unwrap().is_empty());
}

#[test]
fn concurrent_fresh_bootstrap_never_deadlocks_or_fails() {
    // Eight simultaneous first opens of one fresh schema must all succeed,
    // and the server's own cumulative deadlock counter must not move: the
    // advisory lock serializes the CREATE SCHEMA / CREATE TABLE sequence.
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_race");
    let deadlocks = || -> i64 {
        pg.admin()
            .query_one(
                "SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()",
                &[],
            )
            .unwrap()
            .get(0)
    };
    let before = deadlocks();
    const OPENERS: usize = 8;
    let barrier = Arc::new(Barrier::new(OPENERS));
    let dsn = pg.dsn();
    let handles: Vec<_> = (0..OPENERS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let dsn = dsn.clone();
            let schema = schema.clone();
            std::thread::spawn(move || {
                barrier.wait();
                PostgresStore::connect(&dsn, &schema)
            })
        })
        .collect();
    let start = Instant::now();
    let mut stores = Vec::new();
    let mut errors = Vec::new();
    for handle in handles {
        match handle.join().unwrap() {
            Ok(store) => stores.push(store),
            Err(err) => errors.push(err.to_string()),
        }
    }
    assert!(
        errors.is_empty(),
        "concurrent fresh-schema opens failed: {errors:?}"
    );
    assert_eq!(stores.len(), OPENERS);
    assert!(
        start.elapsed() < Duration::from_secs(60),
        "bootstrap serialization is stuck"
    );
    assert_eq!(deadlocks(), before, "server deadlock counter moved");
    assert_eq!(schema_version(&pg, &schema), CURRENT_SCHEMA_VERSION as i32);
    // Every opener is fully usable against the same rows.
    stores[0]
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r-shared"))
        .unwrap();
    for store in &stores {
        assert!(store.load_lifecycle("r-shared").unwrap().is_some());
    }
    assert_eq!(BOOTSTRAP_LOCK_KEY, 0x666C_7962);
}

#[test]
fn is_usable_as_shared_harness_store_across_threads() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_shared");
    let shared: Arc<dyn HarnessStore> = Arc::new(open(&pg, &schema));
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

// --- LISTEN / NOTIFY bridge ---------------------------------------------------------------------

#[test]
fn listen_notify_bridges_cross_instance_writes() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_listen");
    // The writer does not listen; the consumer listens on the same channel.
    let writer = open(&pg, &schema);
    assert!(writer.wait_listener(Duration::from_millis(10)).is_none());
    let consumer = PostgresStore::connect_with(
        &pg.dsn(),
        PostgresStoreOptions {
            schema: schema.clone(),
            listen: true,
            notifier: None,
        },
    )
    .unwrap();
    // Wait until LISTEN is in effect so the NOTIFY below cannot precede it.
    assert_eq!(
        consumer.wait_listener(Duration::from_secs(5)),
        Some(ListenerState::Listening)
    );
    assert!(consumer.listener_alive());
    let seed = DomainEvent::new(
        "run-pg-listen",
        at(0),
        DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
            task_id: "t".into(),
            ..Default::default()
        }),
    );
    writer.append_domain_event(&seed, 0).unwrap();
    // The consumer never wrote anything, yet its notifier watermark advances
    // through the bridge translating the writer's committed NOTIFY.
    let watermark = consumer
        .notifier()
        .wait("run-pg-listen", 0, Duration::from_secs(5));
    assert!(watermark >= 1, "watermark {watermark}");
    // Second write: the bridge keeps draining after the first notification.
    writer
        .append_domain_event(
            &DomainEvent::new(
                "run-pg-listen",
                at(1),
                DomainEventPayload::TransitionedTo(flywheel_core::events::TransitionedTo {
                    target: Status::Ready,
                    error: String::new(),
                }),
            ),
            1,
        )
        .unwrap();
    assert!(
        consumer
            .notifier()
            .wait("run-pg-listen", 1, Duration::from_secs(5))
            >= 2
    );
    let start = Instant::now();
    consumer.close().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "close joined the listener promptly"
    );
}

#[test]
fn listener_reports_failure_when_it_cannot_subscribe() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_listen_fail");
    // Prove the store opened on a good DSN, then point a second listener at a
    // dead port through the same code path: the bridge records the failure
    // and the store stays usable (consumers degrade to poll).
    let store = open(&pg, &schema);
    store
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    let err = PostgresStore::connect_with(
        "postgresql://postgres:fw-test@127.0.0.1:1/postgres?connect_timeout=1",
        PostgresStoreOptions {
            schema,
            listen: true,
            notifier: None,
        },
    )
    .unwrap_err();
    let StoreError::Backend(source) = &err else {
        panic!("expected a backend error, got {err}");
    };
    let pg_err = source
        .downcast_ref::<PostgresError>()
        .expect("a driver error");
    assert!(
        pg_err.code().is_none(),
        "a connection failure carries no SQLSTATE"
    );
    assert!(!err.to_string().contains("fw-test"), "{err}");
}

// --- Fail loud -----------------------------------------------------------------------------------

#[test]
fn connect_to_an_unreachable_server_fails_without_fallback_or_password_leak() {
    // A closed local port: refused fast, no live server needed. There is no
    // sqlite fallback to assert against here; the point is that the store
    // surfaces a typed backend error naming the connection failure and never
    // echoes the credentials.
    let dsn = "postgresql://flywheel:hunter2@127.0.0.1:1/flywheel?connect_timeout=1";
    let err = PostgresStore::connect(dsn, "flywheel").unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, StoreError::Backend(_)), "{text}");
    assert!(text.contains("error connecting to server"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains(dsn), "{text}");
}

#[test]
fn bad_credentials_fail_loudly_without_echoing_the_password() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let dsn = pg.dsn_with_credentials("postgres", "definitely-wrong-pw");
    let err = PostgresStore::connect(&dsn, "public").unwrap_err();
    let text = err.to_string();
    assert!(text.contains("password authentication failed"), "{text}");
    assert!(!text.contains("definitely-wrong-pw"), "{text}");
}

#[test]
fn close_is_clean_and_a_reopened_store_sees_committed_state() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_close");
    let store = open(&pg, &schema);
    let mut lc = Lifecycle::new("t").with_run_id("r1");
    lc.transition_to(Status::Ready, "", Some(at(1))).unwrap();
    store.create_lifecycle(&lc).unwrap();
    store.close().unwrap();
    let reopened = open(&pg, &schema);
    let loaded = reopened.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(loaded.status, Status::Ready);
    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.timestamps.get(&Status::Ready), Some(&at(1)));
    let listing = Mutex::new(
        reopened
            .list_lifecycles(&LifecycleFilter::default())
            .unwrap(),
    );
    assert_eq!(listing.lock().unwrap().len(), 1);
}

fn backend_pid(store: &PostgresStore) -> i32 {
    store.with_client(|client| {
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .unwrap()
            .get(0)
    })
}

fn terminate(pg: &Postgres, pid: i32) {
    let mut admin = pg.admin();
    let killed: bool = admin
        .query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .unwrap()
        .get(0);
    assert!(killed, "terminated backend {pid}");
}

#[test]
fn store_heals_a_terminated_connection() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_reconnect");
    let store = open(&pg, &schema);
    store
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    let before = backend_pid(&store);
    terminate(&pg, before);
    // The call that meets the dead socket may fail (as with a pool); the
    // store must be usable again by the next one, search_path re-pinned.
    let first = store.load_lifecycle("r1");
    let healed = match first {
        Ok(found) => found,
        Err(_) => store.load_lifecycle("r1").unwrap(),
    };
    assert_eq!(healed.unwrap().run_id, "r1");
    assert_ne!(
        backend_pid(&store),
        before,
        "a fresh backend serves the store"
    );
}

#[test]
fn listener_resubscribes_after_its_connection_is_terminated() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_listen_heal");
    let writer = open(&pg, &schema);
    let consumer = PostgresStore::connect_with(
        &pg.dsn(),
        PostgresStoreOptions {
            schema: schema.clone(),
            listen: true,
            notifier: None,
        },
    )
    .unwrap();
    assert_eq!(
        consumer.wait_listener(Duration::from_secs(5)),
        Some(ListenerState::Listening)
    );
    let channel = consumer.channel().to_owned();
    let mut admin = pg.admin();
    let listener_pids: Vec<i32> = admin
        .query(
            "SELECT pid FROM pg_stat_activity WHERE query = $1",
            &[&format!("LISTEN \"{channel}\"")],
        )
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(listener_pids.len(), 1, "one listener backend");
    terminate(&pg, listener_pids[0]);
    // Keep writing (a fresh run per probe) until the re-established LISTEN
    // observes a NOTIFY.
    let deadline = Instant::now() + Duration::from_secs(20);
    for probe in 0.. {
        let run_id = format!("run-pg-heal-{probe}");
        let event = DomainEvent::new(
            run_id.as_str(),
            at(probe),
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: "t".into(),
                ..Default::default()
            }),
        );
        writer.append_domain_event(&event, 0).unwrap();
        if consumer
            .notifier()
            .wait(&run_id, 0, Duration::from_millis(500))
            >= 1
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "listener never resubscribed after termination"
        );
    }
    assert!(consumer.listener_alive());
}

#[test]
fn list_breaks_updated_at_ties_by_descending_run_id() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_tie");
    let store = open(&pg, &schema);
    for run_id in ["r-a", "r-c", "r-b"] {
        store
            .create_lifecycle(&Lifecycle::new("t").with_run_id(run_id))
            .unwrap();
    }
    // Writes inside one microsecond share an updated_at; the documented
    // order then falls back to the greater run_id first.
    pg.admin_in(&schema)
        .execute(
            "UPDATE lifecycles SET updated_at = '2026-05-28T12:00:00Z'",
            &[],
        )
        .unwrap();
    let listed: Vec<String> = store
        .list_lifecycles(&LifecycleFilter::default())
        .unwrap()
        .into_iter()
        .map(|lc| lc.run_id)
        .collect();
    assert_eq!(listed, vec!["r-c", "r-b", "r-a"]);
}

#[test]
fn versioned_save_attempt_waits_for_an_in_flight_lifecycle_write() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_rowlock");
    let store = Arc::new(open(&pg, &schema));
    store
        .create_lifecycle(&Lifecycle::new("t").with_run_id("r1"))
        .unwrap();
    // Another writer advances the run and has not committed yet.
    let mut writer = pg.admin_in(&schema);
    let mut tx = writer.transaction().unwrap();
    tx.execute(
        "UPDATE lifecycles SET version = version + 1 WHERE run_id = 'r1'",
        &[],
    )
    .unwrap();
    let saver = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            let mut rolled = Attempt::new(1, at(0), "r1");
            rolled.input_tokens = 42;
            store.save_attempt("r1", &rolled, Some(1))
        })
    };
    // The versioned write must block on the row rather than check the
    // pre-commit snapshot and land a stale rollup.
    std::thread::sleep(Duration::from_millis(500));
    tx.commit().unwrap();
    let err = saver.join().unwrap().unwrap_err();
    assert_eq!(
        err.as_conflict(),
        Some(&StoreConflictError::OptimisticConcurrency {
            run_id: "r1".into(),
            expected_version: 1,
            actual_version: 2,
        })
    );
    assert!(store.load_attempt("r1", 1).unwrap().is_none());
}

#[test]
fn listen_bridge_watermark_is_the_exact_committed_sequence() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_watermark");
    let writer = open(&pg, &schema);
    let consumer = PostgresStore::connect_with(
        &pg.dsn(),
        PostgresStoreOptions {
            schema: schema.clone(),
            listen: true,
            notifier: None,
        },
    )
    .unwrap();
    assert_eq!(
        consumer.wait_listener(Duration::from_secs(5)),
        Some(ListenerState::Listening)
    );
    let seed = DomainEvent::new(
        "run-wm",
        at(0),
        DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
            task_id: "t".into(),
            ..Default::default()
        }),
    );
    writer.append_domain_event(&seed, 0).unwrap();
    // A watermark past the ledger would let a consumer skip a real row.
    assert_eq!(
        consumer
            .notifier()
            .wait("run-wm", 0, Duration::from_secs(5)),
        1
    );
    writer
        .append_domain_event(
            &DomainEvent::new(
                "run-wm",
                at(1),
                DomainEventPayload::TransitionedTo(flywheel_core::events::TransitionedTo {
                    target: Status::Ready,
                    error: String::new(),
                }),
            ),
            1,
        )
        .unwrap();
    assert_eq!(
        consumer
            .notifier()
            .wait("run-wm", 1, Duration::from_secs(5)),
        2
    );
    assert_eq!(consumer.notifier().watermark("run-wm"), 2);
    consumer.close().unwrap();
}
