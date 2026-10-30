//! Criterion benches for flywheel-worktree hot paths.
//!
//! * `archive_sweep_sqlite/*` -- the worker's phase archive sweep
//!   (`archive_completed_phases`, run after every worker pass) over one
//!   active phase of `PHASE_TASKS` task files against a seeded temp-file
//!   `SQLite` lifecycle store. Every task but the last is DONE, so each sweep
//!   classifies the whole phase and then leaves it active: the steady state
//!   of a nearly finished phase.
//!
//! Sample sizes and measurement windows are tuned so the whole file runs in
//! well under a minute; the improvement-loop scorecard reads the medians.

use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

use flywheel_core::lifecycle::{Lifecycle, Status};
use flywheel_core::store::LifecycleStore;
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_worktree::phase::{ArchiveOptions, archive_completed_phases};

/// Task files in the benchmark phase.
const PHASE_TASKS: usize = 100;

fn t(second: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_779_969_600 + second, 0).expect("in-range timestamp")
}

fn task_id(index: usize) -> String {
    format!("task-{index:04}")
}

fn write_task_file(phase_dir: &Path, id: &str) {
    let body = serde_json::json!({
        "id": id,
        "goal": format!("Implement {id}."),
        "graders": [{"type": "command", "run": "cargo test --workspace"}],
    });
    fs::write(phase_dir.join(format!("{id}.json")), body.to_string()).expect("write task file");
}

/// Walk a fresh lifecycle for `id` through `path`, persisting every step.
fn seed_lifecycle(store: &SqliteStore, id: &str, run_id: &str, offset: i64, path: &[Status]) {
    let mut lc = Lifecycle::new(id).with_run_id(run_id);
    store.create_lifecycle(&lc).expect("create lifecycle");
    for (step, target) in (1..).zip(path) {
        let prior = lc.version;
        lc.transition_to(*target, "", Some(t(offset + step)))
            .expect("legal transition");
        store
            .update_lifecycle(&lc, prior)
            .expect("update lifecycle");
    }
}

fn bench_archive_sweep(c: &mut Criterion) {
    let dir = TempDir::new().expect("tempdir");
    let tasks_dir = dir.path().join("tasks");
    let phase_dir = tasks_dir.join("active").join("01-phase");
    fs::create_dir_all(&phase_dir).expect("create phase dir");
    let store = SqliteStore::open(dir.path().join("flywheel.db")).expect("open sqlite store");
    let done = [
        Status::Ready,
        Status::Running,
        Status::Validating,
        Status::Done,
    ];
    let running = [Status::Ready, Status::Running];
    for index in 0..PHASE_TASKS {
        let id = task_id(index);
        write_task_file(&phase_dir, &id);
        let offset = i64::try_from(index).expect("small index") * 10;
        let path: &[Status] = if index + 1 == PHASE_TASKS {
            &running
        } else {
            &done
        };
        seed_lifecycle(&store, &id, &format!("run-{index:05}"), offset, path);
    }

    let options = ArchiveOptions::default();
    let moved = archive_completed_phases(&tasks_dir, &store, &options).expect("sweep");
    assert!(
        moved.is_empty(),
        "the phase with a running task stays active"
    );

    let mut group = c.benchmark_group("archive_sweep_sqlite");
    group
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));
    group.bench_function("nearly_done_phase_100", |b| {
        b.iter(|| {
            archive_completed_phases(black_box(&tasks_dir), &store, &options)
                .expect("sweep")
                .len()
        });
    });
    group.finish();
}

criterion_group!(benches, bench_archive_sweep);
criterion_main!(benches);
