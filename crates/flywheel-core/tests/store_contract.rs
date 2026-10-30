//! Shared contract suite for every store backend, plus interop with stores
//! written by the Python `SqliteStore` and `PostgresStore`.
//!
//! A case that lives here is a statement about the trait surface in
//! `flywheel_core::store`, not about a particular backend: the same cases
//! run against `InMemoryStore`, a file-backed `SqliteStore`, and a
//! `PostgresStore` (one fresh schema per case on a throwaway container)
//! through one generic `contract` driver. Backend-specific behaviour
//! (pragmas, triggers, schema migration, locked retry, LISTEN/NOTIFY) is
//! covered by each backend's own tests.
//!
//! The interop tests read frozen Python-written fixtures (a SQLite file and a
//! Postgres dump under `tests/fixtures/python_interop/`) through the Rust
//! store, and check that the Rust store writes the same schema and rows the
//! Python store wrote, for both durable backends. The Postgres cases skip,
//! with a message, only when Docker is unavailable.

#[path = "support/pg_container.rs"]
mod pg_container;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use flywheel_core::events::{
    AttemptFinalized, AttemptStarted, CommandApplied, DomainEvent, DomainEventKind,
    DomainEventPayload, GraderEvaluated, LandingParked, LifecycleInitialized, TransitionedTo,
    replay,
};
use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use flywheel_core::loaders::task_digest;
use flywheel_core::store::{
    GraderResultRecord, HarnessStore, JsonObject, LifecycleFilter, SpendSummary, SpendWindow,
    StoreConflictError, StoreError,
};
use flywheel_core::store_memory::InMemoryStore;
use flywheel_core::store_postgres::PostgresStore;
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_core::task::{CommandGrader, Context, Grader, GraderType, RubricGrader, Task};
use pg_container::Postgres;

// --- Fixtures ---------------------------------------------------------------------

/// `2026-05-28T12:00:<n>+00:00`, the Python contract suite's clock.
fn t(second: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_779_969_600 + second, 0).unwrap()
}

/// The lifecycle with the out-of-band attempt rollups (the versioned
/// `save_attempt` counters, which live only in the attempts table) reset, so
/// it can be compared against a pure replay of the ledger.
fn without_rolled_aggregates(lifecycle: &Lifecycle) -> Lifecycle {
    let mut stripped = lifecycle.clone();
    for attempt in &mut stripped.attempts {
        attempt.input_tokens = 0;
        attempt.output_tokens = 0;
        attempt.cache_creation_input_tokens = 0;
        attempt.cache_read_input_tokens = 0;
        attempt.iterations_completed = 0;
        attempt.turns = 0;
        attempt.total_cost_usd = 0.0;
        attempt.last_activity_at = None;
    }
    stripped
}

fn obj(value: Value) -> JsonObject {
    value.as_object().cloned().unwrap()
}

fn lifecycle(task_id: &str, run_id: &str) -> Lifecycle {
    Lifecycle::new(task_id).with_run_id(run_id)
}

fn command_task(task_id: &str, goal: &str) -> Task {
    Task::new(
        goal,
        vec![Grader::Command(CommandGrader::new("true").unwrap())],
    )
    .with_id(task_id)
}

fn ensure_lifecycle(store: &dyn HarnessStore, run_id: &str, task_id: &str) {
    if store.load_lifecycle(run_id).unwrap().is_none() {
        store.create_lifecycle(&lifecycle(task_id, run_id)).unwrap();
    }
}

fn ensure_attempt(store: &dyn HarnessStore, run_id: &str, number: u32) {
    ensure_lifecycle(store, run_id, "t");
    if store.load_attempt(run_id, number).unwrap().is_none() {
        store
            .save_attempt(run_id, &Attempt::new(number, t(0), run_id), None)
            .unwrap();
    }
}

fn receipt(
    run_id: &str,
    attempt_number: u32,
    ordinal: u32,
    grader_type: GraderType,
    passed: bool,
) -> GraderResultRecord {
    GraderResultRecord {
        run_id: run_id.into(),
        attempt_number,
        ordinal,
        grader_type,
        grader_spec: obj(json!({"type": grader_type.as_str(), "run": "true"})),
        passed,
        duration_ms: 1,
        payload: obj(json!({"exit_code": if passed { 0 } else { 1 }})),
        ts: t(0),
        grader_name: None,
        id: None,
    }
}

fn seed_event(run_id: &str) -> DomainEvent {
    DomainEvent::new(
        run_id,
        t(0),
        DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
            task_id: "t".into(),
            worker_id: "w".into(),
            artifacts_dir: "/artifacts".into(),
            ..Default::default()
        }),
    )
}

fn seed(store: &dyn HarnessStore, run_id: &str) -> Lifecycle {
    store.append_domain_event(&seed_event(run_id), 0).unwrap()
}

fn transition(run_id: &str, second: i64, target: Status) -> DomainEvent {
    DomainEvent::new(
        run_id,
        t(second),
        DomainEventPayload::TransitionedTo(TransitionedTo {
            target,
            error: String::new(),
        }),
    )
}

fn attempt_started(
    run_id: &str,
    second: i64,
    number: u32,
    context: &[(&str, &str)],
) -> DomainEvent {
    DomainEvent::new(
        run_id,
        t(second),
        DomainEventPayload::AttemptStarted(AttemptStarted {
            number,
            attempt_run_id: run_id.into(),
            started_at: t(second),
            agent_context: context
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }),
    )
    .with_attempt_number(number)
}

fn attempt_finalized(run_id: &str, second: i64, number: u32, output: &str) -> DomainEvent {
    DomainEvent::new(
        run_id,
        t(second),
        DomainEventPayload::AttemptFinalized(AttemptFinalized {
            number,
            outcome: Outcome::Succeeded,
            ended_at: t(second),
            agent_output: output.into(),
            error: String::new(),
        }),
    )
    .with_attempt_number(number)
}

fn grader_evaluated(run_id: &str, second: i64, attempt_number: u32) -> DomainEvent {
    DomainEvent::new(
        run_id,
        t(second),
        DomainEventPayload::GraderEvaluated(GraderEvaluated {
            ordinal: 0,
            grader_type: "command".into(),
            passed: true,
            duration_ms: 7,
            grader_name: Some("pytest".into()),
            grader_spec: obj(json!({"type": "command", "run": "uv run pytest"})),
            payload: obj(json!({"exit_code": 0})),
        }),
    )
    .with_attempt_number(attempt_number)
}

/// Append a full happy-path domain-event stream ending in DONE.
fn drive_to_done(store: &dyn HarnessStore, run_id: &str) {
    seed(store, run_id);
    store
        .append_domain_event(&transition(run_id, 1, Status::Ready), 1)
        .unwrap();
    store
        .append_domain_event(&transition(run_id, 2, Status::Running), 2)
        .unwrap();
    store
        .append_domain_event(
            &attempt_started(run_id, 3, 1, &[("model_id", "claude-opus-4-8")]),
            3,
        )
        .unwrap();
    store
        .append_domain_event(&transition(run_id, 4, Status::Validating), 4)
        .unwrap();
    store
        .append_domain_event(&grader_evaluated(run_id, 5, 1), 5)
        .unwrap();
    store
        .append_domain_event(&attempt_finalized(run_id, 6, 1, "done"), 6)
        .unwrap();
    store
        .append_domain_event(&transition(run_id, 7, Status::Done), 7)
        .unwrap();
}

fn kinds(events: &[DomainEvent]) -> Vec<DomainEventKind> {
    events.iter().map(DomainEvent::kind).collect()
}

fn sequences(events: &[DomainEvent]) -> Vec<Option<u64>> {
    events.iter().map(|e| e.sequence).collect()
}

fn conflict(err: &StoreError) -> &StoreConflictError {
    err.as_conflict()
        .unwrap_or_else(|| panic!("expected a conflict error, got {err}"))
}

// --- Contract cases -----------------------------------------------------------------

type Case = fn(&dyn HarnessStore);

fn load_missing_lifecycle_returns_none(store: &dyn HarnessStore) {
    assert!(store.load_lifecycle("nope").unwrap().is_none());
}

fn create_then_load_lifecycle_round_trips(store: &dyn HarnessStore) {
    let mut lc = lifecycle("t", "r1");
    lc.worker_id = "w1".into();
    lc.transition_to(Status::Ready, "", Some(t(1))).unwrap();
    store.create_lifecycle(&lc).unwrap();
    let loaded = store.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(loaded.task_id, "t");
    assert_eq!(loaded.run_id, "r1");
    assert_eq!(loaded.worker_id, "w1");
    assert_eq!(loaded.status, Status::Ready);
    assert_eq!(loaded.version, 2);
    assert!(loaded.attempts.is_empty());
    assert_eq!(loaded.timestamps.get(&Status::Ready), Some(&t(1)));
    assert_eq!(loaded, lc);
}

fn create_lifecycle_twice_is_already_exists(store: &dyn HarnessStore) {
    let lc = lifecycle("t", "r1");
    store.create_lifecycle(&lc).unwrap();
    let err = store.create_lifecycle(&lc).unwrap_err();
    assert_eq!(
        conflict(&err),
        &StoreConflictError::LifecycleAlreadyExists {
            run_id: "r1".into()
        }
    );
}

fn update_missing_lifecycle_is_not_found(store: &dyn HarnessStore) {
    let mut lc = lifecycle("t", "r1");
    lc.version = 2;
    let err = store.update_lifecycle(&lc, 1).unwrap_err();
    assert_eq!(
        conflict(&err),
        &StoreConflictError::LifecycleNotFound {
            run_id: "r1".into()
        }
    );
}

fn update_with_matching_version_succeeds(store: &dyn HarnessStore) {
    let mut lc = lifecycle("t", "r1");
    store.create_lifecycle(&lc).unwrap();
    lc.transition_to(Status::Ready, "", Some(t(1))).unwrap();
    store.update_lifecycle(&lc, 1).unwrap();
    let loaded = store.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(loaded.status, Status::Ready);
    assert_eq!(loaded.version, 2);
}

fn update_with_stale_version_conflicts_and_leaves_row_unchanged(store: &dyn HarnessStore) {
    let mut lc = lifecycle("t", "r1");
    store.create_lifecycle(&lc).unwrap();
    lc.transition_to(Status::Ready, "", Some(t(1))).unwrap();
    store.update_lifecycle(&lc, 1).unwrap();

    let mut stale = lifecycle("t", "r1");
    stale.status = Status::Ready;
    stale.version = 2;
    let err = store.update_lifecycle(&stale, 1).unwrap_err();
    assert_eq!(
        conflict(&err),
        &StoreConflictError::OptimisticConcurrency {
            run_id: "r1".into(),
            expected_version: 1,
            actual_version: 2,
        }
    );
    assert_eq!(store.load_lifecycle("r1").unwrap().unwrap().version, 2);
}

fn blocked_requires_json_round_trips(store: &dyn HarnessStore) {
    store
        .create_lifecycle(&lifecycle("t", "r-default"))
        .unwrap();
    assert_eq!(
        store
            .load_lifecycle("r-default")
            .unwrap()
            .unwrap()
            .blocked_requires_json,
        None
    );

    let payload = r#"[{"type": "command_grader", "name": "full-suite"}, {"type": "file_exists", "path": ".flywheel/lkg/.venv", "present": true}]"#;
    let mut lc = lifecycle("t", "r-set");
    lc.blocked_requires_json = Some(payload.into());
    store.create_lifecycle(&lc).unwrap();
    let mut loaded = store.load_lifecycle("r-set").unwrap().unwrap();
    assert_eq!(loaded.blocked_requires_json.as_deref(), Some(payload));

    loaded.transition_to(Status::Ready, "", Some(t(1))).unwrap();
    loaded.blocked_requires_json = None;
    store.update_lifecycle(&loaded, 1).unwrap();
    let mut reloaded = store.load_lifecycle("r-set").unwrap().unwrap();
    assert_eq!(reloaded.blocked_requires_json, None);
    assert_eq!(reloaded.version, 2);

    reloaded.blocked_requires_json = Some(r#"[{"type": "env_var_set", "name": "X"}]"#.into());
    reloaded.version = 3;
    store.update_lifecycle(&reloaded, 2).unwrap();
    assert_eq!(
        store
            .load_lifecycle("r-set")
            .unwrap()
            .unwrap()
            .blocked_requires_json
            .as_deref(),
        Some(r#"[{"type": "env_var_set", "name": "X"}]"#)
    );
}

fn awaiting_manual_ordinal_round_trips(store: &dyn HarnessStore) {
    store
        .create_lifecycle(&lifecycle("t", "r-default"))
        .unwrap();
    assert_eq!(
        store
            .load_lifecycle("r-default")
            .unwrap()
            .unwrap()
            .awaiting_manual_ordinal,
        None
    );
    let mut lc = lifecycle("t", "r-set");
    lc.awaiting_manual_ordinal = Some(3);
    store.create_lifecycle(&lc).unwrap();
    let mut loaded = store.load_lifecycle("r-set").unwrap().unwrap();
    assert_eq!(loaded.awaiting_manual_ordinal, Some(3));

    // An ordinal of 0 is a real value, not the NULL sentinel.
    let mut zero = lifecycle("t", "r-zero");
    zero.awaiting_manual_ordinal = Some(0);
    store.create_lifecycle(&zero).unwrap();
    assert_eq!(
        store
            .load_lifecycle("r-zero")
            .unwrap()
            .unwrap()
            .awaiting_manual_ordinal,
        Some(0)
    );

    loaded.version = 2;
    loaded.awaiting_manual_ordinal = Some(7);
    store.update_lifecycle(&loaded, 1).unwrap();
    let mut reloaded = store.load_lifecycle("r-set").unwrap().unwrap();
    assert_eq!(reloaded.awaiting_manual_ordinal, Some(7));
    reloaded.version = 3;
    reloaded.awaiting_manual_ordinal = None;
    store.update_lifecycle(&reloaded, 2).unwrap();
    assert_eq!(
        store
            .load_lifecycle("r-set")
            .unwrap()
            .unwrap()
            .awaiting_manual_ordinal,
        None
    );
}

fn awaiting_manual_ordinal_cleared_on_ready_done_failed_validation(store: &dyn HarnessStore) {
    // -> READY clears (via the FAILED_VALIDATION retry edge).
    let mut ready = lifecycle("t", "r-clear-ready");
    for (i, target) in [Status::Ready, Status::Running, Status::Validating]
        .into_iter()
        .enumerate()
    {
        ready.transition_to(target, "", Some(t(i as i64))).unwrap();
    }
    ready
        .transition_to(Status::FailedValidation, "grader failed", Some(t(4)))
        .unwrap();
    ready.awaiting_manual_ordinal = Some(2);
    store.create_lifecycle(&ready).unwrap();
    ready.transition_to(Status::Ready, "", Some(t(5))).unwrap();
    assert_eq!(ready.awaiting_manual_ordinal, None);
    store.update_lifecycle(&ready, 5).unwrap();
    let loaded = store.load_lifecycle("r-clear-ready").unwrap().unwrap();
    assert_eq!(loaded.awaiting_manual_ordinal, None);
    assert_eq!(loaded.retries, 1);

    // -> DONE and -> FAILED_VALIDATION clear (via AWAITING_APPROVAL).
    for (run_id, target, error) in [
        ("r-clear-done", Status::Done, ""),
        ("r-clear-fv", Status::FailedValidation, "reviewer rejected"),
    ] {
        let mut lc = lifecycle("t", run_id);
        for (i, step) in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::AwaitingApproval,
        ]
        .into_iter()
        .enumerate()
        {
            lc.transition_to(step, "", Some(t(i as i64))).unwrap();
        }
        lc.awaiting_manual_ordinal = Some(4);
        store.create_lifecycle(&lc).unwrap();
        lc.transition_to(target, error, Some(t(9))).unwrap();
        assert_eq!(lc.awaiting_manual_ordinal, None);
        store.update_lifecycle(&lc, 5).unwrap();
        let reloaded = store.load_lifecycle(run_id).unwrap().unwrap();
        assert_eq!(reloaded.status, target);
        assert_eq!(reloaded.awaiting_manual_ordinal, None);
    }
}

fn load_lifecycle_attaches_attempts_in_number_order(store: &dyn HarnessStore) {
    ensure_lifecycle(store, "r1", "t");
    for n in [3, 1, 2] {
        store
            .save_attempt("r1", &Attempt::new(n, t(0), "r1"), None)
            .unwrap();
    }
    let loaded = store.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(
        loaded.attempts.iter().map(|a| a.number).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

fn load_missing_attempt_returns_none(store: &dyn HarnessStore) {
    assert!(store.load_attempt("r1", 1).unwrap().is_none());
}

fn save_then_load_attempt_round_trips_with_agent_context(store: &dyn HarnessStore) {
    ensure_lifecycle(store, "r1", "t");
    let mut attempt = Attempt::new(1, t(0), "r1");
    attempt
        .agent_context
        .insert("model_id".into(), "claude-opus-4-7".into());
    store.save_attempt("r1", &attempt, None).unwrap();
    let loaded = store.load_attempt("r1", 1).unwrap().unwrap();
    assert_eq!(loaded, attempt);
    assert_eq!(
        loaded.agent_context.get("model_id").map(String::as_str),
        Some("claude-opus-4-7")
    );
}

fn save_attempt_is_an_upsert_keyed_by_run_and_number(store: &dyn HarnessStore) {
    ensure_lifecycle(store, "r1", "t");
    store
        .save_attempt("r1", &Attempt::new(1, t(0), "r1"), None)
        .unwrap();
    let mut finished = Attempt::new(1, t(0), "r1");
    finished.ended_at = Some(t(60));
    finished.outcome = Some(Outcome::Succeeded);
    store.save_attempt("r1", &finished, None).unwrap();
    let loaded = store.load_attempt("r1", 1).unwrap().unwrap();
    assert_eq!(loaded.ended_at, Some(t(60)));
    assert_eq!(loaded.outcome, Some(Outcome::Succeeded));
    let listed = store.list_attempts("r1").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].ended_at, Some(t(60)));
}

fn list_attempts_is_ordered_and_scoped_by_run(store: &dyn HarnessStore) {
    ensure_lifecycle(store, "r1", "t");
    ensure_lifecycle(store, "r2", "t");
    for n in [5, 2, 4, 1, 3] {
        store
            .save_attempt("r1", &Attempt::new(n, t(0), "r1"), None)
            .unwrap();
    }
    store
        .save_attempt("r2", &Attempt::new(1, t(0), "r2"), None)
        .unwrap();
    assert_eq!(
        store
            .list_attempts("r1")
            .unwrap()
            .iter()
            .map(|a| a.number)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        store
            .list_attempts("r2")
            .unwrap()
            .iter()
            .map(|a| a.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["r2"]
    );
    assert!(store.list_attempts("r-none").unwrap().is_empty());
}

fn attempt_aggregates_round_trip_and_default_to_zero(store: &dyn HarnessStore) {
    ensure_lifecycle(store, "r1", "t");
    let mut rolled = Attempt::new(1, t(0), "r1");
    rolled.input_tokens = 1000;
    rolled.output_tokens = 200;
    rolled.cache_creation_input_tokens = 30;
    rolled.cache_read_input_tokens = 4;
    rolled.iterations_completed = 3;
    rolled.turns = 9;
    rolled.total_cost_usd = 0.125;
    rolled.last_activity_at = Some(t(150));
    store.save_attempt("r1", &rolled, None).unwrap();
    store
        .save_attempt("r1", &Attempt::new(2, t(0), "r1"), None)
        .unwrap();
    let listed = store.list_attempts("r1").unwrap();
    for loaded in [
        store.load_attempt("r1", 1).unwrap().unwrap(),
        listed[0].clone(),
    ] {
        assert_eq!(loaded, rolled);
        assert_eq!(loaded.total_tokens(), 1234);
    }
    let bare = store.load_attempt("r1", 2).unwrap().unwrap();
    assert_eq!(bare.total_tokens(), 0);
    assert_eq!(bare.iterations_completed, 0);
    assert_eq!(bare.turns, 0);
    assert_eq!(bare.total_cost_usd, 0.0);
    assert_eq!(bare.last_activity_at, None);
}

fn summarize_spend_sums_across_runs_and_honors_window(store: &dyn HarnessStore) {
    assert_eq!(
        store.summarize_spend(&SpendWindow::UNBOUNDED).unwrap(),
        SpendSummary::default()
    );
    assert_eq!(
        store
            .summarize_spend(&SpendWindow {
                since: Some(t(0)),
                until: Some(t(86_400)),
            })
            .unwrap(),
        SpendSummary::default()
    );

    ensure_lifecycle(store, "ra", "ta");
    ensure_lifecycle(store, "rb", "tb");
    let early = t(0);
    let late = t(43_200);
    let mut a = Attempt::new(1, early, "ra");
    a.input_tokens = 100;
    a.output_tokens = 10;
    a.cache_creation_input_tokens = 2;
    a.cache_read_input_tokens = 1;
    a.total_cost_usd = 0.5;
    a.last_activity_at = Some(early);
    let mut b = Attempt::new(1, late, "rb");
    b.input_tokens = 300;
    b.output_tokens = 30;
    b.cache_creation_input_tokens = 6;
    b.cache_read_input_tokens = 3;
    b.total_cost_usd = 1.5;
    b.last_activity_at = Some(late);
    // No activity instant: counts toward the grand total only.
    let mut c = Attempt::new(2, late, "rb");
    c.input_tokens = 1000;
    store.save_attempt("ra", &a, None).unwrap();
    store.save_attempt("rb", &b, None).unwrap();
    store.save_attempt("rb", &c, None).unwrap();

    let grand = store.summarize_spend(&SpendWindow::UNBOUNDED).unwrap();
    assert_eq!(grand.input_tokens, 1400);
    assert_eq!(grand.output_tokens, 40);
    assert_eq!(grand.cache_creation_tokens, 8);
    assert_eq!(grand.cache_read_tokens, 4);
    assert!((grand.total_cost_usd - 2.0).abs() < 1e-9);

    // Half-open [early, late): includes the lower bound, excludes the upper.
    let windowed = store
        .summarize_spend(&SpendWindow {
            since: Some(early),
            until: Some(late),
        })
        .unwrap();
    assert_eq!(windowed.input_tokens, 100);
    assert!((windowed.total_cost_usd - 0.5).abs() < 1e-9);
    let empty = store
        .summarize_spend(&SpendWindow {
            since: Some(t(10_000_000)),
            until: Some(t(10_000_001)),
        })
        .unwrap();
    assert_eq!(empty, SpendSummary::default());
}

fn save_attempt_honors_expected_version(store: &dyn HarnessStore) {
    ensure_lifecycle(store, "r1", "t");
    let version = store.load_lifecycle("r1").unwrap().unwrap().version;
    let mut attempt = Attempt::new(1, t(0), "r1");
    attempt.input_tokens = 42;
    store.save_attempt("r1", &attempt, Some(version)).unwrap();
    assert_eq!(
        store.load_attempt("r1", 1).unwrap().unwrap().input_tokens,
        42
    );

    let stale = store
        .save_attempt("r1", &Attempt::new(2, t(0), "r1"), Some(999))
        .unwrap_err();
    assert!(matches!(
        conflict(&stale),
        StoreConflictError::OptimisticConcurrency {
            expected_version: 999,
            actual_version: 1,
            ..
        }
    ));
    assert!(store.load_attempt("r1", 2).unwrap().is_none());

    let missing = store
        .save_attempt("r-missing", &Attempt::new(1, t(0), "r-missing"), Some(1))
        .unwrap_err();
    assert_eq!(
        conflict(&missing),
        &StoreConflictError::LifecycleNotFound {
            run_id: "r-missing".into()
        }
    );
}

fn grader_results_append_with_monotonic_ids_and_list_in_ordinal_order(store: &dyn HarnessStore) {
    ensure_attempt(store, "r1", 1);
    ensure_attempt(store, "r1", 2);
    let first = store
        .append_grader_result(&receipt("r1", 1, 2, GraderType::Rubric, true))
        .unwrap();
    let second = store
        .append_grader_result(&receipt("r1", 1, 0, GraderType::Command, true))
        .unwrap();
    store
        .append_grader_result(&receipt("r1", 1, 1, GraderType::Transcript, false))
        .unwrap();
    // A different attempt must not bleed into the queried list.
    store
        .append_grader_result(&receipt("r1", 2, 0, GraderType::Command, true))
        .unwrap();
    assert!(second.id.unwrap() > first.id.unwrap());

    let listed = store.list_grader_results("r1", 1).unwrap();
    assert_eq!(
        listed.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(listed.iter().all(|r| r.attempt_number == 1));
    assert!(!listed[1].passed);
    assert_eq!(listed[1].payload, obj(json!({"exit_code": 1})));
    assert_eq!(listed[2].grader_type, GraderType::Rubric);
    assert!(store.list_grader_results("nope", 999).unwrap().is_empty());
}

fn grader_spec_and_payload_round_trip_exactly(store: &dyn HarnessStore) {
    ensure_attempt(store, "r1", 1);
    let spec = obj(json!({"type": "rubric", "assertions": ["a one", "b two"], "name": "x"}));
    let payload = obj(json!({
        "judge_model": "claude-opus-4-7",
        "per_assertion": [{"text": "a one", "verdict": "pass", "rationale": "ok"}],
        "usage": {"input_tokens": 12, "output_tokens": 34}
    }));
    let record = GraderResultRecord {
        run_id: "r1".into(),
        attempt_number: 1,
        ordinal: 0,
        grader_type: GraderType::Rubric,
        grader_spec: spec.clone(),
        passed: true,
        duration_ms: 42,
        payload: payload.clone(),
        ts: t(0),
        grader_name: Some("x".into()),
        id: None,
    };
    let appended = store.append_grader_result(&record).unwrap();
    assert!(appended.id.is_some());
    let listed = store.list_grader_results("r1", 1).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].grader_spec, spec);
    assert_eq!(listed[0].payload, payload);
    assert_eq!(listed[0].grader_name.as_deref(), Some("x"));
    assert_eq!(listed[0].id, appended.id);
}

fn domain_event_sequences_are_dense_monotonic_and_per_run(store: &dyn HarnessStore) {
    seed(store, "r1");
    seed(store, "r2");
    store
        .append_domain_event(&transition("r1", 1, Status::Ready), 1)
        .unwrap();
    store
        .append_domain_event(&transition("r1", 2, Status::Running), 2)
        .unwrap();
    let r1 = store.list_domain_events("r1").unwrap();
    assert_eq!(sequences(&r1), vec![Some(1), Some(2), Some(3)]);
    assert!(r1.iter().all(|e| e.id.is_some()));
    assert_eq!(
        sequences(&store.list_domain_events("r2").unwrap()),
        vec![Some(1)]
    );
    assert!(store.list_domain_events("ghost").unwrap().is_empty());
}

fn seed_event_creates_the_projection_row(store: &dyn HarnessStore) {
    let folded = seed(store, "r1");
    assert_eq!(folded.status, Status::Pending);
    assert_eq!(folded.version, 1);
    let loaded = store.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(loaded, folded);
    assert_eq!(loaded.task_id, "t");
    assert_eq!(loaded.worker_id, "w");
    assert_eq!(loaded.artifacts_dir, "/artifacts");
    // The seed auto-registers the task identity, so a direct catalogue read
    // of a never-saved task is still None (identity, not definition).
    assert!(store.load_task("t", None).unwrap().is_none());
}

fn duplicate_seed_is_already_exists(store: &dyn HarnessStore) {
    seed(store, "r1");
    let err = store.append_domain_event(&seed_event("r1"), 0).unwrap_err();
    assert_eq!(
        conflict(&err),
        &StoreConflictError::LifecycleAlreadyExists {
            run_id: "r1".into()
        }
    );
    assert_eq!(store.list_domain_events("r1").unwrap().len(), 1);
}

fn append_to_unknown_run_is_not_found(store: &dyn HarnessStore) {
    let err = store
        .append_domain_event(&transition("ghost", 1, Status::Ready), 1)
        .unwrap_err();
    assert_eq!(
        conflict(&err),
        &StoreConflictError::LifecycleNotFound {
            run_id: "ghost".into()
        }
    );
}

fn append_with_stale_version_conflicts(store: &dyn HarnessStore) {
    seed(store, "r1");
    store
        .append_domain_event(&transition("r1", 1, Status::Ready), 1)
        .unwrap();
    let err = store
        .append_domain_event(&transition("r1", 2, Status::Running), 1)
        .unwrap_err();
    assert_eq!(
        conflict(&err),
        &StoreConflictError::OptimisticConcurrency {
            run_id: "r1".into(),
            expected_version: 1,
            actual_version: 2,
        }
    );
    assert_eq!(store.list_domain_events("r1").unwrap().len(), 2);
}

fn illegal_transition_is_a_replay_error_and_writes_nothing(store: &dyn HarnessStore) {
    seed(store, "r1");
    let err = store
        .append_domain_event(&transition("r1", 1, Status::Done), 1)
        .unwrap_err();
    assert!(matches!(err, StoreError::Replay(_)), "{err}");
    assert_eq!(store.load_lifecycle("r1").unwrap().unwrap().version, 1);
    assert_eq!(store.list_domain_events("r1").unwrap().len(), 1);
}

fn happy_path_folds_to_done_with_projections(store: &dyn HarnessStore) {
    drive_to_done(store, "r1");
    let loaded = store.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(loaded.status, Status::Done);
    assert_eq!(loaded.version, 8);
    assert_eq!(loaded.agent_output, "done");
    assert_eq!(loaded.attempts.len(), 1);
    assert_eq!(loaded.attempts[0].outcome, Some(Outcome::Succeeded));
    assert_eq!(
        loaded.attempts[0].agent_context,
        BTreeMap::from([("model_id".to_owned(), "claude-opus-4-8".to_owned())])
    );
    let results = store.list_grader_results("r1", 1).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].passed);
    assert_eq!(results[0].grader_name.as_deref(), Some("pytest"));
    assert_eq!(results[0].grader_type, GraderType::Command);
    assert_eq!(results[0].ts, t(5));
    // The determinism oracle: folding the ledger reproduces the projection.
    let events = store.list_domain_events("r1").unwrap();
    assert_eq!(replay(&events).unwrap(), loaded);
    assert_eq!(
        kinds(&events),
        vec![
            DomainEventKind::LifecycleInitialized,
            DomainEventKind::TransitionedTo,
            DomainEventKind::TransitionedTo,
            DomainEventKind::AttemptStarted,
            DomainEventKind::TransitionedTo,
            DomainEventKind::GraderEvaluated,
            DomainEventKind::AttemptFinalized,
            DomainEventKind::TransitionedTo,
        ]
    );
}

fn attempt_finalized_preserves_boundary_rolled_aggregates(store: &dyn HarnessStore) {
    seed(store, "r1");
    store
        .append_domain_event(&transition("r1", 1, Status::Ready), 1)
        .unwrap();
    store
        .append_domain_event(&transition("r1", 2, Status::Running), 2)
        .unwrap();
    store
        .append_domain_event(&attempt_started("r1", 3, 1, &[]), 3)
        .unwrap();
    let mut rolled = store.load_attempt("r1", 1).unwrap().unwrap();
    rolled.input_tokens = 120;
    rolled.output_tokens = 30;
    rolled.cache_creation_input_tokens = 7;
    rolled.cache_read_input_tokens = 3;
    rolled.iterations_completed = 2;
    rolled.turns = 5;
    rolled.total_cost_usd = 0.5;
    rolled.last_activity_at = Some(t(4));
    store.save_attempt("r1", &rolled, Some(4)).unwrap();
    store
        .append_domain_event(&attempt_finalized("r1", 5, 1, "done"), 4)
        .unwrap();
    let loaded = store.load_attempt("r1", 1).unwrap().unwrap();
    assert_eq!(loaded.outcome, Some(Outcome::Succeeded));
    assert_eq!(loaded.ended_at, Some(t(5)));
    assert_eq!(loaded.total_tokens(), 160);
    assert_eq!(loaded.iterations_completed, 2);
    assert_eq!(loaded.turns, 5);
    assert_eq!(loaded.total_cost_usd, 0.5);
    assert_eq!(loaded.last_activity_at, Some(t(4)));
    let lc = store.load_lifecycle("r1").unwrap().unwrap();
    assert_eq!(lc.attempts[0].input_tokens, 120);
    assert_eq!(lc.attempts[0].outcome, Some(Outcome::Succeeded));
}

fn landing_parked_round_trips_on_the_ledger(store: &dyn HarnessStore) {
    drive_to_done(store, "r1");
    let before = store.load_lifecycle("r1").unwrap().unwrap().version;
    let parked = DomainEvent::new(
        "r1",
        t(8),
        DomainEventPayload::LandingParked(LandingParked {
            park_kind: "divergent-base".into(),
            detail: "base advanced; rebase conflicted".into(),
            ..Default::default()
        }),
    );
    let folded = store.append_domain_event(&parked, before).unwrap();
    assert_eq!(folded.status, Status::Done);
    assert_eq!(folded.version, before + 1);
    let events = store.list_domain_events("r1").unwrap();
    let parks: Vec<&LandingParked> = events
        .iter()
        .filter_map(|e| match &e.payload {
            DomainEventPayload::LandingParked(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(parks.len(), 1);
    assert_eq!(parks[0].park_kind, "divergent-base");
    assert_eq!(parks[0].detail, "base advanced; rebase conflicted");
    assert_eq!(replay(&events).unwrap(), folded);
}

fn command_applied_round_trips_through_the_ledger(store: &dyn HarnessStore) {
    seed(store, "r1");
    let applied = DomainEvent::new(
        "r1",
        t(1),
        DomainEventPayload::CommandApplied(CommandApplied {
            command_kind: "say".into(),
            command_payload: obj(json!({"text": "focus on graders"})),
            command_id: Some(42),
        }),
    )
    .with_attempt_number(1);
    store.append_domain_event(&applied, 1).unwrap();
    let events = store.list_domain_events("r1").unwrap();
    assert_eq!(
        kinds(&events),
        vec![
            DomainEventKind::LifecycleInitialized,
            DomainEventKind::CommandApplied
        ]
    );
    let DomainEventPayload::CommandApplied(steering) = &events[1].payload else {
        panic!("expected CommandApplied");
    };
    assert_eq!(steering.command_kind, "say");
    assert_eq!(
        steering.command_payload,
        obj(json!({"text": "focus on graders"}))
    );
    assert_eq!(steering.command_id, Some(42));
    assert_eq!(events[1].ts, t(1));
    assert_eq!(events[1].attempt_number, Some(1));
    let folded = replay(&events).unwrap();
    assert_eq!(folded.status, Status::Pending);
    assert_eq!(folded.version, 2);
}

fn save_task_returns_digest_and_is_idempotent(store: &dyn HarnessStore) {
    let task = command_task("t", "Do the thing.");
    let first = store.save_task(&task, t(0)).unwrap();
    let second = store.save_task(&task, t(5)).unwrap();
    assert_eq!(first, second);
    assert_eq!(first, task_digest(&task));
    assert_eq!(store.load_task("t", None).unwrap(), Some(task.clone()));
    assert_eq!(store.load_task("t", Some(&first)).unwrap(), Some(task));
    assert!(store.load_task("absent", None).unwrap().is_none());
    assert!(
        store
            .load_task("absent", Some("deadbeef"))
            .unwrap()
            .is_none()
    );
    assert!(store.load_task("t", Some("deadbeef")).unwrap().is_none());
}

fn load_task_latest_breaks_created_at_tie_by_insertion_order(store: &dyn HarnessStore) {
    let first = command_task("t", "First saved.");
    let second = command_task("t", "Second saved.");
    store.save_task(&first, t(0)).unwrap();
    store.save_task(&second, t(0)).unwrap();
    assert_eq!(store.load_task("t", None).unwrap(), Some(second));
}

fn save_task_edit_creates_new_version(store: &dyn HarnessStore) {
    let original = command_task("t", "Original goal.");
    let edited = command_task("t", "Edited goal.");
    let h_original = store.save_task(&original, t(0)).unwrap();
    let h_edited = store.save_task(&edited, t(10)).unwrap();
    assert_ne!(h_original, h_edited);
    assert_eq!(store.load_task("t", None).unwrap(), Some(edited.clone()));
    assert_eq!(
        store.load_task("t", Some(&h_original)).unwrap(),
        Some(original)
    );
    assert_eq!(store.load_task("t", Some(&h_edited)).unwrap(), Some(edited));
}

fn load_task_for_run_resolves_pinned_version(store: &dyn HarnessStore) {
    let original = command_task("t", "Original goal.");
    let edited = command_task("t", "Edited goal.");
    let h_original = store.save_task(&original, t(0)).unwrap();
    store.save_task(&edited, t(10)).unwrap();
    let pinned = DomainEvent::new(
        "run-pin",
        t(0),
        DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
            task_id: "t".into(),
            task_content_hash: h_original,
            ..Default::default()
        }),
    );
    store.append_domain_event(&pinned, 0).unwrap();
    assert_eq!(store.load_task_for_run("run-pin").unwrap(), Some(original));
    assert!(store.load_task_for_run("never-ran").unwrap().is_none());

    // An unpinned run (empty hash) resolves to the latest version.
    let unpinned = DomainEvent::new(
        "run-latest",
        t(1),
        DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
            task_id: "t".into(),
            ..Default::default()
        }),
    );
    store.append_domain_event(&unpinned, 0).unwrap();
    assert_eq!(store.load_task_for_run("run-latest").unwrap(), Some(edited));
}

fn tags_and_context_round_trip_and_fork_versions(store: &dyn HarnessStore) {
    let mut base = command_task("t", "Same goal.");
    base.tags = vec!["http".into(), "reliability".into()];
    base.context = Context {
        relevant: vec!["src/lib.rs".into()],
        references: vec!["docs/strategy.md".into()],
        constraints: vec!["no new deps".into()],
        non_goals: vec!["perf".into()],
        edge_cases: vec!["empty input".into()],
        notes: "Keep it small.".into(),
    };
    let mut retagged = base.clone();
    retagged.tags = vec!["b".into(), "c".into()];
    let h1 = store.save_task(&base, t(0)).unwrap();
    let h2 = store.save_task(&retagged, t(10)).unwrap();
    assert_ne!(h1, h2);
    let latest = store.load_task("t", None).unwrap().unwrap();
    assert_eq!(latest.tags, vec!["b", "c"]);
    assert_eq!(latest, retagged);
    let pinned = store.load_task("t", Some(&h1)).unwrap().unwrap();
    assert_eq!(pinned.tags, vec!["http", "reliability"]);
    assert_eq!(pinned, base);
}

fn rubric_grader_task_round_trips(store: &dyn HarnessStore) {
    let mut rubric = RubricGrader::new(vec!["It works".into(), "It is idiomatic".into()]).unwrap();
    rubric.name = Some("review".into());
    rubric.judge_model = Some("claude-opus-4-8".into());
    rubric.retry_on_fail = false;
    let mut command = CommandGrader::new("cargo test").unwrap();
    command.name = Some("tests".into());
    let task = Task::new(
        "Port the stores.",
        vec![Grader::Command(command), Grader::Rubric(rubric)],
    )
    .with_id("rubric-task");
    let digest = store.save_task(&task, t(0)).unwrap();
    let loaded = store
        .load_task("rubric-task", Some(&digest))
        .unwrap()
        .unwrap();
    assert_eq!(loaded, task);
    assert_eq!(task_digest(&loaded), digest);
}

fn control_commands_enqueue_claim_once_in_order_scoped_by_run(store: &dyn HarnessStore) {
    ensure_lifecycle(store, "r1", "t");
    ensure_lifecycle(store, "r2", "t");
    let first = store
        .enqueue_command("r1", "say", obj(json!({"text": "first"})), t(0))
        .unwrap();
    assert_eq!(first.run_id, "r1");
    assert_eq!(first.kind, "say");
    assert_eq!(first.payload, obj(json!({"text": "first"})));
    assert_eq!(first.enqueued_at, t(0));
    assert!(first.is_pending());
    let second = store
        .enqueue_command("r1", "interrupt", JsonObject::new(), t(1))
        .unwrap();
    let third = store
        .enqueue_command(
            "r1",
            "set_model",
            obj(json!({"target": "claude-opus-4-8"})),
            t(2),
        )
        .unwrap();
    let other = store
        .enqueue_command("r2", "say", obj(json!({"text": "to r2"})), t(2))
        .unwrap();
    assert!(second.id.unwrap() > first.id.unwrap());
    assert!(third.id.unwrap() > second.id.unwrap());
    assert!(other.id.unwrap() > third.id.unwrap());

    let claimed = store.claim_commands("r1", t(3)).unwrap();
    assert_eq!(
        claimed.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>(),
        vec!["say", "interrupt", "set_model"]
    );
    assert_eq!(
        claimed.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![first.id, second.id, third.id]
    );
    assert!(claimed.iter().all(|c| c.claimed_at == Some(t(3))));
    assert_eq!(
        claimed[2].payload,
        obj(json!({"target": "claude-opus-4-8"}))
    );
    assert!(store.claim_commands("r1", t(4)).unwrap().is_empty());

    let r2 = store.claim_commands("r2", t(4)).unwrap();
    assert_eq!(r2.len(), 1);
    assert_eq!(r2[0].payload, obj(json!({"text": "to r2"})));
    assert!(store.claim_commands("r-empty", t(4)).unwrap().is_empty());

    // A batch enqueued after the first claim is the only thing the next
    // claim returns.
    store
        .enqueue_command("r1", "approve", JsonObject::new(), t(5))
        .unwrap();
    let again = store.claim_commands("r1", t(6)).unwrap();
    assert_eq!(
        again.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>(),
        vec!["approve"]
    );
    assert_eq!(again[0].claimed_at, Some(t(6)));
}

fn delete_command_removes_the_row_and_unknown_ids_are_noops(store: &dyn HarnessStore) {
    ensure_lifecycle(store, "r1", "t");
    store
        .enqueue_command("r1", "say", obj(json!({"text": "one"})), t(0))
        .unwrap();
    store
        .enqueue_command("r1", "interrupt", JsonObject::new(), t(1))
        .unwrap();
    let claimed = store.claim_commands("r1", t(2)).unwrap();
    assert_eq!(claimed.len(), 2);
    store.delete_command(claimed[0].id.unwrap()).unwrap();
    assert!(store.claim_commands("r1", t(3)).unwrap().is_empty());
    store.delete_command(claimed[1].id.unwrap()).unwrap();
    store.delete_command(987_654).unwrap();
    // A pending row can be deleted too; it never surfaces afterwards.
    let pending = store
        .enqueue_command("r1", "say", JsonObject::new(), t(4))
        .unwrap();
    store.delete_command(pending.id.unwrap()).unwrap();
    assert!(store.claim_commands("r1", t(5)).unwrap().is_empty());
}

fn list_lifecycles_filters_and_orders_by_most_recent_write(store: &dyn HarnessStore) {
    assert!(
        store
            .list_lifecycles(&LifecycleFilter::default())
            .unwrap()
            .is_empty()
    );
    // Distinct write instants: the durable store orders on a wall-clock
    // updated_at with microsecond resolution.
    let pause = || std::thread::sleep(Duration::from_millis(2));
    let mut a = lifecycle("task-a", "r-a");
    a.transition_to(Status::Ready, "", Some(t(0))).unwrap();
    store.create_lifecycle(&a).unwrap();
    pause();
    store.create_lifecycle(&lifecycle("task-b", "r-b")).unwrap();
    pause();
    let mut c = lifecycle("task-a", "r-c");
    c.transition_to(Status::Ready, "", Some(t(0))).unwrap();
    store.create_lifecycle(&c).unwrap();
    store
        .save_attempt("r-c", &Attempt::new(1, t(0), "r-c"), None)
        .unwrap();
    pause();
    // Re-writing r-a makes it the most recent.
    a.transition_to(Status::Running, "", Some(t(1))).unwrap();
    store.update_lifecycle(&a, 2).unwrap();

    let run_ids = |filter: &LifecycleFilter| -> Vec<String> {
        store
            .list_lifecycles(filter)
            .unwrap()
            .into_iter()
            .map(|lc| lc.run_id)
            .collect()
    };
    assert_eq!(
        run_ids(&LifecycleFilter::default()),
        vec!["r-a", "r-c", "r-b"]
    );
    assert_eq!(
        run_ids(&LifecycleFilter::default().with_statuses([Status::Ready, Status::Pending])),
        vec!["r-c", "r-b"]
    );
    assert_eq!(
        run_ids(&LifecycleFilter::default().with_task_id("task-a")),
        vec!["r-a", "r-c"]
    );
    assert_eq!(
        run_ids(
            &LifecycleFilter::default()
                .with_statuses([Status::Ready])
                .with_task_id("task-a")
        ),
        vec!["r-c"]
    );
    assert!(run_ids(&LifecycleFilter::default().with_statuses([])).is_empty());
    assert!(run_ids(&LifecycleFilter::default().with_task_id("task-z")).is_empty());
    // A task-id set keeps the global order across the member tasks.
    assert_eq!(
        run_ids(&LifecycleFilter::default().with_task_ids(["task-b", "task-a", "task-z"])),
        vec!["r-a", "r-c", "r-b"]
    );
    assert_eq!(
        run_ids(&LifecycleFilter::default().with_task_ids(["task-b"])),
        vec!["r-b"]
    );
    assert_eq!(
        run_ids(
            &LifecycleFilter::default()
                .with_statuses([Status::Ready, Status::Pending])
                .with_task_ids(["task-a", "task-b"])
        ),
        vec!["r-c", "r-b"]
    );
    assert_eq!(
        run_ids(
            &LifecycleFilter::default()
                .with_task_id("task-a")
                .with_task_ids(["task-b"])
        ),
        Vec::<String>::new()
    );
    assert!(run_ids(&LifecycleFilter::default().with_task_ids(Vec::<String>::new())).is_empty());
    // The set form attaches attempts exactly like the single-task form.
    assert_eq!(
        store
            .list_lifecycles(&LifecycleFilter::default().with_task_ids(["task-a"]))
            .unwrap(),
        store
            .list_lifecycles(&LifecycleFilter::default().with_task_id("task-a"))
            .unwrap()
    );
    // Each element is the fully folded lifecycle, attempts attached.
    let listed = store
        .list_lifecycles(&LifecycleFilter::default().with_task_id("task-a"))
        .unwrap();
    assert_eq!(listed[1].attempts.len(), 1);
    assert_eq!(listed[1], store.load_lifecycle("r-c").unwrap().unwrap());
    assert!(listed[0].attempts.is_empty());
}

const CASES: &[(&str, Case)] = &[
    (
        "load_missing_lifecycle_returns_none",
        load_missing_lifecycle_returns_none,
    ),
    (
        "create_then_load_lifecycle_round_trips",
        create_then_load_lifecycle_round_trips,
    ),
    (
        "create_lifecycle_twice_is_already_exists",
        create_lifecycle_twice_is_already_exists,
    ),
    (
        "update_missing_lifecycle_is_not_found",
        update_missing_lifecycle_is_not_found,
    ),
    (
        "update_with_matching_version_succeeds",
        update_with_matching_version_succeeds,
    ),
    (
        "update_with_stale_version_conflicts_and_leaves_row_unchanged",
        update_with_stale_version_conflicts_and_leaves_row_unchanged,
    ),
    (
        "blocked_requires_json_round_trips",
        blocked_requires_json_round_trips,
    ),
    (
        "awaiting_manual_ordinal_round_trips",
        awaiting_manual_ordinal_round_trips,
    ),
    (
        "awaiting_manual_ordinal_cleared_on_ready_done_failed_validation",
        awaiting_manual_ordinal_cleared_on_ready_done_failed_validation,
    ),
    (
        "load_lifecycle_attaches_attempts_in_number_order",
        load_lifecycle_attaches_attempts_in_number_order,
    ),
    (
        "load_missing_attempt_returns_none",
        load_missing_attempt_returns_none,
    ),
    (
        "save_then_load_attempt_round_trips_with_agent_context",
        save_then_load_attempt_round_trips_with_agent_context,
    ),
    (
        "save_attempt_is_an_upsert_keyed_by_run_and_number",
        save_attempt_is_an_upsert_keyed_by_run_and_number,
    ),
    (
        "list_attempts_is_ordered_and_scoped_by_run",
        list_attempts_is_ordered_and_scoped_by_run,
    ),
    (
        "attempt_aggregates_round_trip_and_default_to_zero",
        attempt_aggregates_round_trip_and_default_to_zero,
    ),
    (
        "summarize_spend_sums_across_runs_and_honors_window",
        summarize_spend_sums_across_runs_and_honors_window,
    ),
    (
        "save_attempt_honors_expected_version",
        save_attempt_honors_expected_version,
    ),
    (
        "grader_results_append_with_monotonic_ids_and_list_in_ordinal_order",
        grader_results_append_with_monotonic_ids_and_list_in_ordinal_order,
    ),
    (
        "grader_spec_and_payload_round_trip_exactly",
        grader_spec_and_payload_round_trip_exactly,
    ),
    (
        "domain_event_sequences_are_dense_monotonic_and_per_run",
        domain_event_sequences_are_dense_monotonic_and_per_run,
    ),
    (
        "seed_event_creates_the_projection_row",
        seed_event_creates_the_projection_row,
    ),
    (
        "duplicate_seed_is_already_exists",
        duplicate_seed_is_already_exists,
    ),
    (
        "append_to_unknown_run_is_not_found",
        append_to_unknown_run_is_not_found,
    ),
    (
        "append_with_stale_version_conflicts",
        append_with_stale_version_conflicts,
    ),
    (
        "illegal_transition_is_a_replay_error_and_writes_nothing",
        illegal_transition_is_a_replay_error_and_writes_nothing,
    ),
    (
        "happy_path_folds_to_done_with_projections",
        happy_path_folds_to_done_with_projections,
    ),
    (
        "attempt_finalized_preserves_boundary_rolled_aggregates",
        attempt_finalized_preserves_boundary_rolled_aggregates,
    ),
    (
        "landing_parked_round_trips_on_the_ledger",
        landing_parked_round_trips_on_the_ledger,
    ),
    (
        "command_applied_round_trips_through_the_ledger",
        command_applied_round_trips_through_the_ledger,
    ),
    (
        "save_task_returns_digest_and_is_idempotent",
        save_task_returns_digest_and_is_idempotent,
    ),
    (
        "load_task_latest_breaks_created_at_tie_by_insertion_order",
        load_task_latest_breaks_created_at_tie_by_insertion_order,
    ),
    (
        "save_task_edit_creates_new_version",
        save_task_edit_creates_new_version,
    ),
    (
        "load_task_for_run_resolves_pinned_version",
        load_task_for_run_resolves_pinned_version,
    ),
    (
        "tags_and_context_round_trip_and_fork_versions",
        tags_and_context_round_trip_and_fork_versions,
    ),
    (
        "rubric_grader_task_round_trips",
        rubric_grader_task_round_trips,
    ),
    (
        "control_commands_enqueue_claim_once_in_order_scoped_by_run",
        control_commands_enqueue_claim_once_in_order_scoped_by_run,
    ),
    (
        "delete_command_removes_the_row_and_unknown_ids_are_noops",
        delete_command_removes_the_row_and_unknown_ids_are_noops,
    ),
    (
        "list_lifecycles_filters_and_orders_by_most_recent_write",
        list_lifecycles_filters_and_orders_by_most_recent_write,
    ),
];

/// Run every contract case against a fresh store from `make`, reporting
/// every failing case name rather than stopping at the first.
fn contract<S: HarnessStore>(make: impl Fn() -> S) {
    let mut failures = Vec::new();
    for (name, case) in CASES {
        let store = make();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(&store)));
        if let Err(panic) = outcome {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "non-string panic".to_owned());
            failures.push(format!("{name}: {message}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} contract case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn in_memory_store_satisfies_the_contract() {
    contract(InMemoryStore::new);
}

#[test]
fn sqlite_store_satisfies_the_contract() {
    let dir = tempfile::tempdir().unwrap();
    let counter = AtomicUsize::new(0);
    contract(|| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        SqliteStore::open(dir.path().join(format!("contract-{n}.db"))).unwrap()
    });
}

#[test]
fn sqlite_in_memory_store_satisfies_the_contract() {
    contract(|| SqliteStore::in_memory().unwrap());
}

#[test]
fn postgres_store_satisfies_the_contract() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let dsn = pg.dsn();
    contract(|| PostgresStore::connect(&dsn, &pg.fresh_schema("fw_contract")).unwrap());
}

#[test]
fn contract_cases_are_uniquely_named() {
    let mut names: Vec<&str> = CASES.iter().map(|(name, _)| *name).collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), total);
    assert!(total >= 38);
}

// --- Interop with Python-written stores (frozen golden fixtures) ---------------------
//
// Deployments that predate the Rust port hold SQLite files and Postgres
// schemas written by the Python `SqliteStore` / `PostgresStore`. The fixtures
// under `tests/fixtures/python_interop/` were written by those Python stores
// running the interop timeline that `seed_interop_with_rust` mirrors step for
// step, then frozen:
//
// - `store.sqlite`: the SQLite database file exactly as the Python store left it.
// - `store_postgres.sql`: `pg_dump --inserts` of the schema the Python store
//   bootstrapped and seeded (schema name `fw_golden_store`).

const INTEROP_RUN: &str = "run-interop";
const INTEROP_TASK: &str = "interop-task";

/// The task digest the Python store returned from `save_task` when it wrote
/// the fixtures.
const PYTHON_TASK_DIGEST: &str = "e3f7d2a2191dc72871eb3b7b6049e4617b9790d20dc5846dc7d54dbcc88e6ea3";

/// The schema name baked into `store_postgres.sql`.
const GOLDEN_PG_SCHEMA: &str = "fw_golden_store";

/// Columns the store stamps from the wall clock rather than the caller's
/// `now`, so they differ between any two seedings of the same timeline.
const WALL_CLOCK_COLUMNS: &[(&str, &str)] = &[("lifecycles", "updated_at")];

fn python_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/python_interop")
        .join(name)
}

/// A private copy of the Python-written SQLite fixture: opening a store
/// writes WAL/SHM side files and may touch the database, and the committed
/// fixture must stay byte-for-byte what Python wrote.
fn python_written_sqlite(dir: &Path) -> PathBuf {
    let db = dir.join("python-seeded.sqlite");
    std::fs::copy(python_fixture("store.sqlite"), &db).unwrap();
    db
}

/// Recreate the Python-written Postgres schema from its dump under `schema`.
fn load_python_written_postgres(pg: &Postgres, schema: &str) {
    let dump = std::fs::read_to_string(python_fixture("store_postgres.sql")).unwrap();
    pg.admin()
        .batch_execute(&dump.replace(GOLDEN_PG_SCHEMA, schema))
        .unwrap();
}

fn without_wall_clock_columns(table: &str, mut row: JsonObject) -> JsonObject {
    for (t, column) in WALL_CLOCK_COLUMNS {
        if *t == table {
            assert!(
                row.remove(*column).is_some_and(|v| !v.is_null()),
                "{table}.{column} is written on every save"
            );
        }
    }
    row
}

/// `value` serialized with every object's keys sorted, so two rows compare
/// equal whatever order their JSON documents were written in.
fn canonical_json(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.sort_by_key(|(key, _)| *key);
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(key, v)| (key.clone(), sorted(v)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

/// A SQLite cell as JSON; text holding a JSON object or array is decoded so
/// key order and whitespace (which differ between Python's `json.dumps` and
/// `serde_json`) do not count.
fn sqlite_cell(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(bytes) => {
            let text = std::str::from_utf8(bytes).unwrap();
            match serde_json::from_str::<Value>(text) {
                Ok(decoded @ (Value::Object(_) | Value::Array(_))) => decoded,
                _ => Value::String(text.to_owned()),
            }
        }
        ValueRef::Blob(bytes) => json!(bytes),
    }
}

/// Every schema object's DDL and every table's rows (order-insensitive),
/// keyed by object name.
fn sqlite_snapshot(path: &Path) -> BTreeMap<String, (Option<String>, Vec<String>)> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let objects: Vec<(String, String, Option<String>)> = conn
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut snapshot = BTreeMap::new();
    for (kind, name, sql) in objects {
        let mut rows = Vec::new();
        if kind == "table" {
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{name}\"")).unwrap();
            let columns: Vec<String> = stmt
                .column_names()
                .iter()
                .map(|c| (*c).to_owned())
                .collect();
            let mut cursor = stmt.query([]).unwrap();
            while let Some(row) = cursor.next().unwrap() {
                let mut object = JsonObject::new();
                for (i, column) in columns.iter().enumerate() {
                    object.insert(column.clone(), sqlite_cell(row.get_ref(i).unwrap()));
                }
                rows.push(canonical_json(&Value::Object(without_wall_clock_columns(
                    &name, object,
                ))));
            }
            rows.sort();
        }
        snapshot.insert(format!("{kind} {name}"), (sql, rows));
    }
    snapshot
}

/// Every table's column catalog and rows (order-insensitive) in `schema`.
fn postgres_snapshot(pg: &Postgres, schema: &str) -> BTreeMap<String, (Vec<String>, Vec<String>)> {
    let mut client = pg.admin();
    let tables: Vec<String> = client
        .query(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = $1 AND table_type = 'BASE TABLE' ORDER BY table_name",
            &[&schema],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut snapshot = BTreeMap::new();
    for table in tables {
        let columns: Vec<String> = client
            .query(
                "SELECT column_name::text, data_type::text, is_nullable::text \
                 FROM information_schema.columns \
                 WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
                &[&schema, &table],
            )
            .unwrap()
            .iter()
            .map(|r| {
                let (name, ty, nullable): (String, String, String) = (r.get(0), r.get(1), r.get(2));
                format!("{name} {ty} nullable={nullable}")
            })
            .collect();
        let mut rows: Vec<String> = client
            .query(
                &format!("SELECT row_to_json(t)::text FROM \"{schema}\".\"{table}\" t"),
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| {
                let row: Value = serde_json::from_str(r.get::<_, &str>(0)).unwrap();
                let Value::Object(row) = row else {
                    panic!("row_to_json returned a non-object")
                };
                canonical_json(&Value::Object(without_wall_clock_columns(&table, row)))
            })
            .collect();
        rows.sort();
        snapshot.insert(table, (columns, rows));
    }
    snapshot
}

/// The task the interop timeline saves, built with the Rust types.
fn interop_task() -> Task {
    let mut command = CommandGrader::new("cargo test").unwrap();
    command.name = Some("tests".into());
    let mut rubric = RubricGrader::new(vec!["It works".into(), "It is idiomatic".into()]).unwrap();
    rubric.name = Some("review".into());
    let mut task = Task::new(
        "Port the stores.",
        vec![Grader::Command(command), Grader::Rubric(rubric)],
    )
    .with_id(INTEROP_TASK);
    task.tags = vec!["rust".into(), "stores".into()];
    task.context = Context {
        relevant: vec!["crates/flywheel-core".into()],
        notes: "Keep parity.".into(),
        ..Default::default()
    };
    task
}

const INTEROP_TIMELINE_SECONDS: i64 = 12;

/// Seed `store` with the interop timeline using the Rust types: the same
/// calls, in the same order, the Python store made to write the fixtures.
fn seed_interop_with_rust(store: &dyn HarnessStore) -> String {
    let task = interop_task();
    let digest = store.save_task(&task, t(0)).unwrap();
    let seed = DomainEvent::new(
        INTEROP_RUN,
        t(0),
        DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
            task_id: INTEROP_TASK.into(),
            worker_id: "w-interop".into(),
            artifacts_dir: "/artifacts".into(),
            task_content_hash: digest.clone(),
            source: "owner/repo#42".into(),
        }),
    );
    store.append_domain_event(&seed, 0).unwrap();
    store
        .append_domain_event(&transition(INTEROP_RUN, 1, Status::Ready), 1)
        .unwrap();
    store
        .append_domain_event(&transition(INTEROP_RUN, 2, Status::Running), 2)
        .unwrap();
    store
        .append_domain_event(
            &attempt_started(
                INTEROP_RUN,
                3,
                1,
                &[
                    ("model_id", "claude-opus-4-8"),
                    ("base_commit_sha", "abc123"),
                ],
            ),
            3,
        )
        .unwrap();
    let mut rolled = store.load_attempt(INTEROP_RUN, 1).unwrap().unwrap();
    rolled.input_tokens = 1200;
    rolled.output_tokens = 300;
    rolled.cache_creation_input_tokens = 40;
    rolled.cache_read_input_tokens = 5;
    rolled.iterations_completed = 2;
    rolled.turns = 6;
    rolled.total_cost_usd = 0.75;
    rolled.last_activity_at = Some(t(4));
    store.save_attempt(INTEROP_RUN, &rolled, Some(4)).unwrap();
    store
        .append_domain_event(&transition(INTEROP_RUN, 5, Status::Validating), 4)
        .unwrap();
    let evaluated = DomainEvent::new(
        INTEROP_RUN,
        t(6),
        DomainEventPayload::GraderEvaluated(GraderEvaluated {
            ordinal: 0,
            grader_type: "command".into(),
            passed: true,
            duration_ms: 7,
            grader_name: Some("tests".into()),
            grader_spec: obj(json!({"type": "command", "run": "cargo test", "name": "tests"})),
            payload: obj(json!({"exit_code": 0, "stdout_tail": "ok"})),
        }),
    )
    .with_attempt_number(1);
    store.append_domain_event(&evaluated, 5).unwrap();
    store
        .append_domain_event(&attempt_finalized(INTEROP_RUN, 7, 1, "ported"), 6)
        .unwrap();
    store
        .append_domain_event(&transition(INTEROP_RUN, 8, Status::Done), 7)
        .unwrap();
    let parked = DomainEvent::new(
        INTEROP_RUN,
        t(9),
        DomainEventPayload::LandingParked(LandingParked {
            park_kind: "divergent-base".into(),
            detail: "base advanced".into(),
            ..Default::default()
        }),
    );
    store.append_domain_event(&parked, 8).unwrap();
    store
        .enqueue_command(INTEROP_RUN, "say", obj(json!({"text": "focus"})), t(10))
        .unwrap();
    let claimed = store.claim_commands(INTEROP_RUN, t(11)).unwrap();
    assert_eq!(claimed.len(), 1);
    store
        .enqueue_command(
            INTEROP_RUN,
            "interrupt",
            JsonObject::new(),
            t(INTEROP_TIMELINE_SECONDS),
        )
        .unwrap();
    digest
}

fn iso(second: i64) -> String {
    flywheel_core::event_serde::format_timestamp(&t(second))
}

#[test]
fn interop_clock_matches_the_python_fixture() {
    // `datetime(2026, 5, 28, 12, 0, n, tzinfo=timezone.utc)` is the clock the
    // Python store wrote the fixtures against.
    assert_eq!(iso(0), "2026-05-28T12:00:00+00:00");
    assert_eq!(iso(INTEROP_TIMELINE_SECONDS), "2026-05-28T12:00:12+00:00");
}

/// Everything the Rust store must read back from a Python-seeded timeline.
fn assert_rust_reads_python_seed(store: &dyn HarnessStore, digest: &str) {
    let task = interop_task();
    assert_eq!(
        digest,
        task_digest(&task),
        "digest parity across implementations"
    );
    let lc = store.load_lifecycle(INTEROP_RUN).unwrap().unwrap();
    assert_eq!(lc.status, Status::Done);
    assert_eq!(lc.version, 9);
    assert_eq!(lc.task_id, INTEROP_TASK);
    assert_eq!(lc.worker_id, "w-interop");
    assert_eq!(lc.artifacts_dir, "/artifacts");
    assert_eq!(lc.source, "owner/repo#42");
    assert_eq!(lc.task_content_hash, digest);
    assert_eq!(lc.agent_output, "ported");
    assert_eq!(
        lc.timestamps,
        BTreeMap::from([
            (Status::Ready, t(1)),
            (Status::Running, t(2)),
            (Status::Validating, t(5)),
            (Status::Done, t(8)),
        ])
    );
    assert_eq!(lc.attempts.len(), 1);
    let attempt = &lc.attempts[0];
    assert_eq!(attempt.number, 1);
    assert_eq!(attempt.run_id, INTEROP_RUN);
    assert_eq!(attempt.started_at, t(3));
    assert_eq!(attempt.ended_at, Some(t(7)));
    assert_eq!(attempt.outcome, Some(Outcome::Succeeded));
    assert_eq!(attempt.agent_output, "ported");
    assert_eq!(
        attempt.agent_context,
        BTreeMap::from([
            ("model_id".to_owned(), "claude-opus-4-8".to_owned()),
            ("base_commit_sha".to_owned(), "abc123".to_owned()),
        ])
    );
    assert_eq!(attempt.input_tokens, 1200);
    assert_eq!(attempt.output_tokens, 300);
    assert_eq!(attempt.cache_creation_input_tokens, 40);
    assert_eq!(attempt.cache_read_input_tokens, 5);
    assert_eq!(attempt.iterations_completed, 2);
    assert_eq!(attempt.turns, 6);
    assert_eq!(attempt.total_cost_usd, 0.75);
    assert_eq!(attempt.last_activity_at, Some(t(4)));

    let events = store.list_domain_events(INTEROP_RUN).unwrap();
    assert_eq!(
        kinds(&events),
        vec![
            DomainEventKind::LifecycleInitialized,
            DomainEventKind::TransitionedTo,
            DomainEventKind::TransitionedTo,
            DomainEventKind::AttemptStarted,
            DomainEventKind::TransitionedTo,
            DomainEventKind::GraderEvaluated,
            DomainEventKind::AttemptFinalized,
            DomainEventKind::TransitionedTo,
            DomainEventKind::LandingParked,
        ]
    );
    assert_eq!(sequences(&events), (1..=9).map(Some).collect::<Vec<_>>());
    assert_eq!(events[3].attempt_number, Some(1));
    // The python-written ledger folds, in Rust, to the python-written
    // projection (minus the boundary rollups that live only in attempts).
    assert_eq!(replay(&events).unwrap(), without_rolled_aggregates(&lc));

    let results = store.list_grader_results(INTEROP_RUN, 1).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].grader_type, GraderType::Command);
    assert_eq!(results[0].grader_name.as_deref(), Some("tests"));
    assert!(results[0].passed);
    assert_eq!(results[0].duration_ms, 7);
    assert_eq!(
        results[0].grader_spec,
        obj(json!({"type": "command", "run": "cargo test", "name": "tests"}))
    );
    assert_eq!(
        results[0].payload,
        obj(json!({"exit_code": 0, "stdout_tail": "ok"}))
    );
    assert_eq!(results[0].ts, t(6));

    assert_eq!(
        store.load_task_for_run(INTEROP_RUN).unwrap(),
        Some(task.clone())
    );
    assert_eq!(store.load_task(INTEROP_TASK, None).unwrap(), Some(task));

    let claimed = store.claim_commands(INTEROP_RUN, t(13)).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].kind, "interrupt");
    assert_eq!(claimed[0].id, Some(2));
    assert_eq!(claimed[0].enqueued_at, t(INTEROP_TIMELINE_SECONDS));
    assert_eq!(claimed[0].claimed_at, Some(t(13)));
    assert!(store.claim_commands(INTEROP_RUN, t(14)).unwrap().is_empty());

    let spend = store.summarize_spend(&SpendWindow::UNBOUNDED).unwrap();
    assert_eq!(spend.input_tokens, 1200);
    assert_eq!(spend.total_tokens(), 1545);
    assert_eq!(spend.total_cost_usd, 0.75);
    let listed = store
        .list_lifecycles(&LifecycleFilter::default().with_statuses([Status::Done]))
        .unwrap();
    assert_eq!(listed, vec![lc]);
}

#[test]
fn python_written_database_reads_back_through_the_rust_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(python_written_sqlite(dir.path())).unwrap();
    assert_rust_reads_python_seed(&store, PYTHON_TASK_DIGEST);
}

#[test]
fn rust_written_database_matches_the_python_written_golden() {
    // Rust seeding the same timeline must produce the same schema and the
    // same rows the Python store wrote, so a database holding rows from
    // both implementations is indistinguishable from either alone.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("rust-seeded.sqlite");
    let store = SqliteStore::open(&db).unwrap();
    assert_eq!(seed_interop_with_rust(&store), PYTHON_TASK_DIGEST);
    store.close().unwrap();

    let rust = sqlite_snapshot(&db);
    let python = sqlite_snapshot(&python_written_sqlite(dir.path()));
    assert_eq!(
        rust.keys().collect::<Vec<_>>(),
        python.keys().collect::<Vec<_>>(),
        "schema objects"
    );
    for (name, (sql, rows)) in &python {
        let (rust_sql, rust_rows) = &rust[name];
        assert_eq!(rust_sql, sql, "DDL of {name}");
        assert_eq!(rust_rows, rows, "rows of {name}");
    }
    assert_eq!(
        python["table events"].1.len(),
        9,
        "the golden carries the full ledger"
    );
}

#[test]
fn python_written_postgres_schema_reads_back_through_the_rust_store() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("fw_interop_py");
    load_python_written_postgres(&pg, &schema);
    // The Python store bootstrapped the schema; the Rust store opens it
    // unchanged (same schema_version, idempotent DDL) and reads every row.
    let store = PostgresStore::connect(&pg.dsn(), &schema).unwrap();
    assert_rust_reads_python_seed(&store, PYTHON_TASK_DIGEST);
}

#[test]
fn rust_written_postgres_schema_matches_the_python_written_golden() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let rust_schema = pg.fresh_schema("fw_interop_rs");
    let store = PostgresStore::connect(&pg.dsn(), &rust_schema).unwrap();
    assert_eq!(seed_interop_with_rust(&store), PYTHON_TASK_DIGEST);
    store.close().unwrap();
    let python_schema = pg.fresh_schema("fw_interop_py");
    load_python_written_postgres(&pg, &python_schema);

    let rust = postgres_snapshot(&pg, &rust_schema);
    let python = postgres_snapshot(&pg, &python_schema);
    assert_eq!(
        rust.keys().collect::<Vec<_>>(),
        python.keys().collect::<Vec<_>>(),
        "tables"
    );
    for (table, (columns, rows)) in &python {
        let (rust_columns, rust_rows) = &rust[table];
        assert_eq!(rust_columns, columns, "columns of {table}");
        assert_eq!(rust_rows, rows, "rows of {table}");
    }
    assert_eq!(
        python["events"].1.len(),
        9,
        "the golden carries the full ledger"
    );
}
