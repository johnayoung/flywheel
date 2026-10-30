//! Criterion benches for flywheel-orchestrator hot paths.
//!
//! * `dag/*` -- prerequisite-DAG task selection over a layered graph of
//!   `DAG_TASKS` items: building the validated `WorkGraph` (cycle and
//!   dangling-edge checks), the scheduler's `ready_set` query, classifying
//!   every item against a seeded SQLite lifecycle store (`status_rows`, run
//!   each scheduling pass), and `select_next_task` over the classified rows.
//! * `claims_sqlite/*` -- claim acquisition against a real temp-file
//!   `SqliteClaimStore` already holding `HELD_CLAIMS` live peer claims with
//!   conflict keys: a winning acquire+release round trip and a refused
//!   acquire on a peer-held task.
//! * `work_source/*` -- `DirectoryWorkSource::list_work` (every scheduling
//!   pass's first step) over `SOURCE_PHASES` active phase directories of
//!   `SOURCE_TASKS_PER_PHASE` task files each.
//!
//! Sample sizes and measurement windows are tuned so the whole file runs in
//! well under a minute; the improvement-loop scorecard reads the medians.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::hint::black_box;
use std::time::Duration;

use chrono::{DateTime, Utc};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

use flywheel_core::lifecycle::{Lifecycle, Status};
use flywheel_core::store::{LifecycleStore, TaskStore};
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_core::task::{CommandGrader, Grader, Task};
use flywheel_orchestrator::claims::{ClaimStore, SqliteClaimStore};
use flywheel_orchestrator::sources::{DirectoryWorkSource, WorkItem, WorkSource};
use flywheel_orchestrator::work_graph::{ReadySetQuery, WorkGraph};
use flywheel_orchestrator::workflow::select::{SelectOptions, select_next_task, status_rows};

/// Tasks in the benchmark DAG.
const DAG_TASKS: usize = 500;
/// Tasks per layer; each task depends on up to three tasks one layer up.
const LAYER_WIDTH: usize = 50;
/// Leading layers whose tasks are already DONE in the store.
const DONE_LAYERS: usize = 3;
/// Live peer claims present while measuring claim acquisition.
const HELD_CLAIMS: usize = 200;
/// Active phase directories listed by the work-source bench.
const SOURCE_PHASES: usize = 5;
/// Task files per active phase directory in the work-source bench.
const SOURCE_TASKS_PER_PHASE: usize = 100;

fn t(second: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_779_969_600 + second, 0).expect("in-range timestamp")
}

fn task_id(index: usize) -> String {
    format!("task-{index:04}")
}

/// A layered DAG: task `i` in layer `L > 0` depends on up to three tasks of
/// layer `L - 1`; priorities vary so the stable sort does real work.
fn dag_items() -> Vec<WorkItem> {
    (0..DAG_TASKS)
        .map(|index| {
            let layer = index / LAYER_WIDTH;
            let slot = index % LAYER_WIDTH;
            let id = task_id(index);
            let task = Task::new(
                format!("Implement {id}."),
                vec![Grader::Command(
                    CommandGrader::new("cargo test --workspace").expect("valid grader"),
                )],
            )
            .with_id(&id);
            let prerequisites: Vec<String> = if layer == 0 {
                Vec::new()
            } else {
                let base = (layer - 1) * LAYER_WIDTH;
                [slot, (slot + 7) % LAYER_WIDTH, (slot + 19) % LAYER_WIDTH]
                    .iter()
                    .map(|s| task_id(base + s))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            };
            WorkItem::new(task, format!("tasks/{id}.json"))
                .with_prerequisites(prerequisites)
                .with_priority(i64::try_from(index % 5).expect("small priority"))
        })
        .collect()
}

fn is_done(index: usize) -> bool {
    index / LAYER_WIDTH < DONE_LAYERS
}

/// Scheduler state values (`fresh` / `done`) keyed by task id.
fn state_values(items: &[WorkItem]) -> HashMap<String, &'static str> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let state = if is_done(index) { "done" } else { "fresh" };
            (item.task.id.clone(), state)
        })
        .collect()
}

/// A temp-file lifecycle store holding one DONE run per done-layer task.
fn seeded_lifecycle_store(items: &[WorkItem]) -> (TempDir, SqliteStore) {
    let dir = TempDir::new().expect("tempdir");
    let store = SqliteStore::open(dir.path().join("flywheel.db")).expect("open sqlite store");
    for (index, item) in items.iter().enumerate() {
        store.save_task(&item.task, t(0)).expect("save task");
        if !is_done(index) {
            continue;
        }
        let mut lc = Lifecycle::new(&item.task.id).with_run_id(format!("run-{index:05}"));
        store.create_lifecycle(&lc).expect("create lifecycle");
        let offset = i64::try_from(index).expect("small index") * 10;
        for (target, step) in [
            (Status::Ready, 1),
            (Status::Running, 2),
            (Status::Validating, 3),
            (Status::Done, 4),
        ] {
            let prior = lc.version;
            lc.transition_to(target, "", Some(t(offset + step)))
                .expect("legal transition");
            store
                .update_lifecycle(&lc, prior)
                .expect("update lifecycle");
        }
    }
    (dir, store)
}

fn bench_dag(c: &mut Criterion) {
    let items = dag_items();
    let states = state_values(&items);
    let graph = WorkGraph::build(items.clone()).graph;
    let query = ReadySetQuery::new();
    let ready = graph.ready_set(&states, &query);
    assert_eq!(
        ready.len(),
        LAYER_WIDTH,
        "exactly the first fresh layer is ready"
    );

    let (_dir, store) = seeded_lifecycle_store(&items);
    let rows = status_rows(&items, &store).expect("classify rows");
    let options = SelectOptions::default();
    assert!(select_next_task(&rows, &options).is_some());

    let mut group = c.benchmark_group("dag");
    group
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));
    group.bench_function("work_graph_build_500", |b| {
        b.iter_batched(
            || items.clone(),
            |batch| WorkGraph::build(black_box(batch)),
            BatchSize::LargeInput,
        );
    });
    group.bench_function("ready_set_500", |b| {
        b.iter(|| graph.ready_set(black_box(&states), black_box(&query)).len());
    });
    group.bench_function("status_rows_sqlite_500", |b| {
        b.iter(|| status_rows(black_box(&items), &store).expect("classify rows"));
    });
    group.bench_function("select_next_task_500", |b| {
        b.iter(|| select_next_task(black_box(&rows), black_box(&options)).map(|r| r.task_id()));
    });
    group.finish();
}

fn bench_claims(c: &mut Criterion) {
    let dir = TempDir::new().expect("tempdir");
    let claims = SqliteClaimStore::open(dir.path().join("claims.db")).expect("open claim store");
    let lease = 1.0e9;
    for index in 0..HELD_CLAIMS {
        let keys: BTreeSet<String> = [format!("crate-{index}")].into_iter().collect();
        let held = claims
            .acquire_claim(&task_id(index), "w-peer", t(0), lease, &keys)
            .expect("seed acquire");
        assert!(held.is_some(), "seed claims never conflict");
    }

    let mut group = c.benchmark_group("claims_sqlite");
    group
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));

    let free_keys: BTreeSet<String> = ["crate-free".to_owned()].into_iter().collect();
    group.bench_function("acquire_release", |b| {
        b.iter(|| {
            let claim = claims
                .acquire_claim(black_box("task-bench"), "w-bench", t(10), lease, &free_keys)
                .expect("acquire")
                .expect("free task is claimable");
            claims
                .release_claim(&claim, Some(t(11)))
                .expect("release claim");
        });
    });

    let peer_keys: BTreeSet<String> = ["crate-0".to_owned()].into_iter().collect();
    group.bench_function("acquire_refused_held", |b| {
        b.iter(|| {
            let refused = claims
                .acquire_claim(black_box(&task_id(0)), "w-bench", t(10), lease, &peer_keys)
                .expect("acquire");
            assert!(refused.is_none());
        });
    });
    group.finish();
}

fn bench_work_source(c: &mut Criterion) {
    let dir = TempDir::new().expect("tempdir");
    let tasks_dir = dir.path().join(".flywheel").join("tasks");
    for phase in 0..SOURCE_PHASES {
        let phase_dir = tasks_dir.join("active").join(format!("{phase:02}-phase"));
        fs::create_dir_all(&phase_dir).expect("create phase dir");
        for index in 0..SOURCE_TASKS_PER_PHASE {
            let id = format!("p{phase:02}-{}", task_id(index));
            let body = serde_json::json!({
                "id": id,
                "goal": format!("Implement {id}."),
                "graders": [{"type": "command", "run": "cargo test --workspace"}],
                "priority": index % 3,
                "conflict_keys": [format!("crate-{}", index % 7)],
            });
            fs::write(phase_dir.join(format!("{id}.json")), body.to_string())
                .expect("write task file");
        }
    }
    let source = DirectoryWorkSource::new(&tasks_dir);
    let listed = source.list_work().expect("list work");
    assert_eq!(listed.len(), SOURCE_PHASES * SOURCE_TASKS_PER_PHASE);
    assert_eq!(listed[0].phase.as_deref(), Some("00-phase"));

    let mut group = c.benchmark_group("work_source");
    group
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));
    group.bench_function("directory_list_work_500", |b| {
        b.iter(|| black_box(&source).list_work().expect("list work").len());
    });
    group.finish();
}

criterion_group!(benches, bench_dag, bench_claims, bench_work_source);
criterion_main!(benches);
