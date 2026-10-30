//! `MirroringStore` delegation contract. Every harness run writes through
//! this wrapper, so each non-append verb must reach the wrapped store with
//! its arguments intact (writes land there; reads and typed errors come back
//! unchanged) and must never touch the telemetry stream. A ledger append
//! mirrors exactly one `domain.*` line, and only once the store accepted it.

use super::support::{ListSink, fail_command, fixed_clock, ok_command, utc};
use crate::events::{
    DomainEvent, DomainEventKind, DomainEventPayload, LifecycleInitialized, TransitionedTo,
};
use crate::harness::telemetry::{MirroringStore, RunTelemetry};
use crate::lifecycle::{Attempt, Lifecycle, Status};
use crate::store::{
    AttemptStore, ControlCommandStore, DomainEventStore, GraderResultRecord, GraderResultStore,
    HarnessStore, JsonObject, LifecycleFilter, LifecycleStore, SpendWindow, StoreConflictError,
    TaskStore,
};
use crate::store_memory::InMemoryStore;
use crate::task::{GraderType, Task};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::sync::Arc;

const MIRRORED_RUN: &str = "run-mirrored";
const PLAIN_RUN: &str = "run-plain";

fn wrapped() -> (Arc<InMemoryStore>, MirroringStore, Arc<ListSink>) {
    let inner = Arc::new(InMemoryStore::new());
    let sink = ListSink::new();
    let telemetry = Arc::new(RunTelemetry::new(
        Some(sink.clone()),
        MIRRORED_RUN,
        fixed_clock(at(0)),
    ));
    let as_harness: Arc<dyn HarnessStore> = inner.clone();
    (inner, MirroringStore::new(as_harness, telemetry), sink)
}

fn at(minute: u32) -> DateTime<Utc> {
    utc(2026, 1, 1, 0, minute)
}

fn object(value: &serde_json::Value) -> JsonObject {
    value.as_object().cloned().unwrap()
}

fn optimistic(run_id: &str, expected: u64, actual: u64) -> StoreConflictError {
    StoreConflictError::OptimisticConcurrency {
        run_id: run_id.into(),
        expected_version: expected,
        actual_version: actual,
    }
}

fn seed_event() -> DomainEvent {
    DomainEvent::new(
        MIRRORED_RUN,
        at(1),
        DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
            task_id: "mirror-task".into(),
            worker_id: "w".into(),
            artifacts_dir: "/artifacts".into(),
            ..Default::default()
        }),
    )
}

fn transition(minute: u32, target: Status) -> DomainEvent {
    DomainEvent::new(
        MIRRORED_RUN,
        at(minute),
        DomainEventPayload::TransitionedTo(TransitionedTo {
            target,
            error: String::new(),
        }),
    )
}

#[test]
fn ledger_appends_mirror_once_and_only_after_the_store_accepts_them() {
    let (inner, store, sink) = wrapped();

    let seeded = store.append_domain_event(&seed_event(), 0).unwrap();
    let ready = store
        .append_domain_event(&transition(2, Status::Ready), seeded.version)
        .unwrap();
    assert_eq!(ready.status, Status::Ready);
    assert_eq!(
        inner.load_lifecycle(MIRRORED_RUN).unwrap(),
        Some(ready.clone())
    );

    // A stale append is rejected with the wrapped store's typed conflict
    // and leaves no mirror line behind.
    let stale = store
        .append_domain_event(&transition(3, Status::Running), seeded.version)
        .unwrap_err();
    assert_eq!(
        stale.as_conflict(),
        Some(&optimistic(MIRRORED_RUN, seeded.version, ready.version))
    );

    let mirrors: Vec<String> = sink
        .domain_mirrors(MIRRORED_RUN)
        .into_iter()
        .map(|r| r.kind)
        .collect();
    assert_eq!(
        mirrors,
        ["domain.lifecycle_initialized", "domain.transitioned_to"]
    );

    let events = store.list_domain_events(MIRRORED_RUN).unwrap();
    assert_eq!(events, inner.list_domain_events(MIRRORED_RUN).unwrap());
    assert_eq!(events.len(), 2);
    assert_eq!(
        store
            .latest_domain_event_of_kind(MIRRORED_RUN, DomainEventKind::TransitionedTo)
            .unwrap()
            .map(|e| e.ts),
        Some(at(2))
    );
    assert_eq!(
        store
            .latest_domain_event_of_kind(MIRRORED_RUN, DomainEventKind::Landed)
            .unwrap(),
        None
    );
}

#[test]
fn task_and_lifecycle_verbs_delegate_verbatim_without_telemetry() {
    let (inner, store, sink) = wrapped();

    // Tasks: two versions of one id; a pinned read must honour the pin.
    let v1 = Task::new("first goal", vec![ok_command()]).with_id("tsk");
    let v2 = Task::new("second goal", vec![ok_command(), fail_command()]).with_id("tsk");
    let h1 = store.save_task(&v1, at(1)).unwrap();
    let h2 = store.save_task(&v2, at(2)).unwrap();
    assert_ne!(h1, h2);
    assert_eq!(inner.load_task("tsk", Some(&h1)).unwrap(), Some(v1.clone()));
    assert_eq!(store.load_task("tsk", Some(&h1)).unwrap(), Some(v1.clone()));
    assert_eq!(store.load_task("tsk", None).unwrap(), Some(v2.clone()));
    assert_eq!(store.load_task("missing", None).unwrap(), None);

    // Lifecycles: created through the wrapper, pinned to the older task.
    let mut plain = Lifecycle::new("tsk").with_run_id(PLAIN_RUN);
    plain.task_content_hash = h1.clone();
    store.create_lifecycle(&plain).unwrap();
    let other = Lifecycle::new("tsk").with_run_id("run-other");
    store.create_lifecycle(&other).unwrap();
    assert_eq!(
        store
            .create_lifecycle(&plain)
            .unwrap_err()
            .as_conflict()
            .cloned(),
        Some(StoreConflictError::LifecycleAlreadyExists {
            run_id: PLAIN_RUN.into()
        })
    );
    assert_eq!(store.load_task_for_run(PLAIN_RUN).unwrap(), Some(v1));
    assert_eq!(store.load_task_for_run("run-other").unwrap(), Some(v2));
    assert_eq!(store.load_task_for_run("run-missing").unwrap(), None);

    let loaded = store.load_lifecycle(PLAIN_RUN).unwrap().unwrap();
    assert_eq!(
        Some(loaded.clone()),
        inner.load_lifecycle(PLAIN_RUN).unwrap()
    );
    let prev = loaded.version;
    let mut advanced = loaded;
    advanced
        .transition_to(Status::Ready, "", Some(at(3)))
        .unwrap();
    advanced.version = prev + 1;
    store.update_lifecycle(&advanced, prev).unwrap();
    assert_eq!(
        inner.load_lifecycle(PLAIN_RUN).unwrap().unwrap().status,
        Status::Ready
    );
    // A stale writer (expected `prev`, carrying a newer row) is refused.
    let mut stale = advanced.clone();
    stale.version = prev + 2;
    assert_eq!(
        store
            .update_lifecycle(&stale, prev)
            .unwrap_err()
            .as_conflict()
            .cloned(),
        Some(optimistic(PLAIN_RUN, prev, prev + 1))
    );
    let ready_filter = LifecycleFilter::default().with_statuses([Status::Ready]);
    let ready: Vec<String> = store
        .list_lifecycles(&ready_filter)
        .unwrap()
        .into_iter()
        .map(|lc| lc.run_id)
        .collect();
    assert_eq!(ready, [PLAIN_RUN]);
    assert_eq!(
        store.list_lifecycles(&LifecycleFilter::default()).unwrap(),
        inner.list_lifecycles(&LifecycleFilter::default()).unwrap()
    );
    assert!(sink.all().is_empty(), "non-append verbs emitted telemetry");
}

/// A wrapper holding one plain (non-ledger) lifecycle at `PLAIN_RUN`.
fn wrapped_with_plain_run() -> (Arc<InMemoryStore>, MirroringStore, Arc<ListSink>, u64) {
    let (inner, store, sink) = wrapped();
    let plain = Lifecycle::new("tsk").with_run_id(PLAIN_RUN);
    store.create_lifecycle(&plain).unwrap();
    let version = inner.load_lifecycle(PLAIN_RUN).unwrap().unwrap().version;
    (inner, store, sink, version)
}

#[test]
fn attempt_and_spend_verbs_delegate_verbatim_without_telemetry() {
    let (inner, store, sink, version) = wrapped_with_plain_run();

    // The versioned write keeps its optimistic-concurrency key.
    let mut first = Attempt::new(1, at(4), PLAIN_RUN);
    first.input_tokens = 100;
    first.total_cost_usd = 0.25;
    assert_eq!(
        store
            .save_attempt(PLAIN_RUN, &first, Some(version + 1))
            .unwrap_err()
            .as_conflict()
            .cloned(),
        Some(optimistic(PLAIN_RUN, version + 1, version))
    );
    assert_eq!(inner.list_attempts(PLAIN_RUN).unwrap(), []);
    store
        .save_attempt(PLAIN_RUN, &first, Some(version))
        .unwrap();
    let mut second = Attempt::new(2, at(5), PLAIN_RUN);
    second.output_tokens = 7;
    store.save_attempt(PLAIN_RUN, &second, None).unwrap();
    assert_eq!(
        store.load_attempt(PLAIN_RUN, 2).unwrap(),
        Some(second.clone())
    );
    assert_eq!(store.load_attempt(PLAIN_RUN, 3).unwrap(), None);
    assert_eq!(
        store.list_attempts(PLAIN_RUN).unwrap(),
        [first.clone(), second]
    );
    let spend = store.summarize_spend(&SpendWindow::UNBOUNDED).unwrap();
    assert_eq!(
        spend,
        inner.summarize_spend(&SpendWindow::UNBOUNDED).unwrap()
    );
    assert_eq!((spend.input_tokens, spend.output_tokens), (100, 7));
    assert!((spend.total_cost_usd - 0.25).abs() < f64::EPSILON);
    assert!(sink.all().is_empty(), "non-append verbs emitted telemetry");
}

#[test]
fn grader_and_control_command_verbs_delegate_verbatim_without_telemetry() {
    let (inner, store, sink, _) = wrapped_with_plain_run();

    // Grader receipts: the store assigns the id; reads are per attempt.
    let receipt = |attempt_number: u32, passed: bool| GraderResultRecord {
        run_id: PLAIN_RUN.into(),
        attempt_number,
        ordinal: 0,
        grader_type: GraderType::Command,
        grader_spec: object(&json!({"type": "command", "run": "true"})),
        passed,
        duration_ms: 3,
        payload: object(&json!({"exit_code": i32::from(!passed)})),
        ts: at(6),
        grader_name: None,
        id: None,
    };
    let stored = store.append_grader_result(&receipt(1, false)).unwrap();
    assert!(stored.id.is_some());
    store.append_grader_result(&receipt(2, true)).unwrap();
    assert_eq!(store.list_grader_results(PLAIN_RUN, 1).unwrap(), [stored]);
    assert_eq!(
        store.list_grader_results(PLAIN_RUN, 2).unwrap(),
        inner.list_grader_results(PLAIN_RUN, 2).unwrap()
    );

    // Control commands: enqueue, delete, claim-once.
    let say = store
        .enqueue_command(PLAIN_RUN, "say", object(&json!({"message": "hi"})), at(7))
        .unwrap();
    let doomed = store
        .enqueue_command(PLAIN_RUN, "interrupt", JsonObject::new(), at(7))
        .unwrap();
    store
        .create_lifecycle(&Lifecycle::new("tsk").with_run_id("run-other"))
        .unwrap();
    store
        .enqueue_command("run-other", "interrupt", JsonObject::new(), at(7))
        .unwrap();
    store.delete_command(doomed.id.unwrap()).unwrap();
    let claimed = store.claim_commands(PLAIN_RUN, at(8)).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, say.id);
    assert_eq!(claimed[0].payload, say.payload);
    assert_eq!(claimed[0].claimed_at, Some(at(8)));
    assert_eq!(store.claim_commands(PLAIN_RUN, at(9)).unwrap(), []);
    let pending = inner.claim_commands("run-other", at(10)).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, "interrupt");

    assert!(sink.all().is_empty(), "non-append verbs emitted telemetry");
}
