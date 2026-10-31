//! Autopilot emission is idempotent by task id across the whole tasks tree
//! (docs/autopilot.md: "re-running autopilot never duplicates in-flight
//! work"). An authored id that another phase already defines -- an
//! operator's in-flight task under `active/<phase>/`, or a completed task
//! under `archive/<phase>/` -- must not be written into `active/autopilot/`:
//! a second copy of a listed id isolates EVERY copy from the scheduling
//! graph (the operator's in-flight task included), and a copy of an
//! archived id is satisfied by the earlier task's DONE record and never
//! runs.

use std::path::Path;

use flywheel_core::task::{CommandGrader, Grader, Task};
use flywheel_orchestrator::autopilot::{
    AUTOPILOT_PHASE, EmittedTask, Finding, GraderSource, ScoreBreakdown, Tier, emit_emitted_task,
};
use flywheel_orchestrator::sources::{DirectoryWorkSource, WorkItem, WorkSource};
use flywheel_orchestrator::work_graph::WorkGraph;
use serde_json::json;
use tempfile::TempDir;

fn emitted(task_id: &str) -> EmittedTask {
    let task = Task::new(
        format!("Goal for {task_id}."),
        vec![Grader::Command(
            CommandGrader::new("cargo test --workspace").unwrap(),
        )],
    )
    .with_id(task_id);
    EmittedTask::new(
        Finding::new(format!("find-{task_id}"), Tier::TestCoverage, task_id),
        task,
        "cargo test --workspace",
        GraderSource::RepoCommand,
        "Cargo.toml",
    )
}

fn breakdown() -> ScoreBreakdown {
    ScoreBreakdown {
        tier: Tier::TestCoverage,
        tier_weight: 1,
        urgency: 1,
        importance: 1,
        blocks: 0,
        effort: 1,
        final_score: 10.0,
        preemptive: false,
    }
}

/// Write a hand-authored task file whose file name need not match its id.
fn write_task(dir: &Path, file_name: &str, task_id: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(file_name),
        serde_json::to_string_pretty(&json!({
            "id": task_id,
            "goal": format!("Operator goal for {task_id}."),
            "graders": [{"type": "command", "run": "true"}],
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn an_id_already_listed_in_another_active_phase_is_not_emitted() {
    let tmp = TempDir::new().unwrap();
    let tasks_dir = tmp.path().join("tasks");
    write_task(
        &tasks_dir.join("active").join("05-feature"),
        "01-add-retry.json",
        "add-retry",
    );

    let written = emit_emitted_task(
        &emitted("add-retry"),
        &breakdown(),
        &tasks_dir,
        AUTOPILOT_PHASE,
    )
    .unwrap();

    assert_eq!(written, None, "a duplicate id must not be emitted");
    assert!(
        !tasks_dir
            .join("active")
            .join(AUTOPILOT_PHASE)
            .join("add-retry.json")
            .exists()
    );
    // The operator's task stays schedulable: one listed copy, no exclusion.
    let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();
    let result = WorkGraph::build(items);
    assert!(
        result.excluded.is_empty(),
        "duplicate-id isolation: {:?}",
        result.excluded
    );
    let ids: Vec<&str> = result.graph.items().iter().map(WorkItem::id).collect();
    assert_eq!(ids, vec!["add-retry"]);
}

#[test]
fn an_id_already_archived_is_not_re_emitted() {
    let tmp = TempDir::new().unwrap();
    let tasks_dir = tmp.path().join("tasks");
    let archived = tasks_dir.join("archive").join("03-old");
    // Matched by the archived file's id field and by its file name (the
    // rule prerequisite resolution uses for `archive/*/<id>.json`).
    write_task(&archived, "07-parser-tests.json", "add-parser-tests");
    write_task(&archived, "lint-fix.json", "lint-fix");

    for id in ["add-parser-tests", "lint-fix"] {
        assert_eq!(
            emit_emitted_task(&emitted(id), &breakdown(), &tasks_dir, AUTOPILOT_PHASE).unwrap(),
            None,
            "{id} was already completed in an archived phase"
        );
    }
    assert!(
        DirectoryWorkSource::new(&tasks_dir)
            .list_work()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_fresh_id_is_still_emitted_beside_other_phases() {
    let tmp = TempDir::new().unwrap();
    let tasks_dir = tmp.path().join("tasks");
    write_task(
        &tasks_dir.join("active").join("05-feature"),
        "add-retry.json",
        "add-retry",
    );
    write_task(
        &tasks_dir.join("archive").join("03-old"),
        "lint-fix.json",
        "lint-fix",
    );

    let path = emit_emitted_task(
        &emitted("add-parser-tests"),
        &breakdown(),
        &tasks_dir,
        AUTOPILOT_PHASE,
    )
    .unwrap()
    .expect("a fresh id is emitted");
    assert_eq!(
        path,
        tasks_dir
            .join("active")
            .join(AUTOPILOT_PHASE)
            .join("add-parser-tests.json")
    );
    let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();
    let result = WorkGraph::build(items);
    assert!(result.excluded.is_empty());
    assert_eq!(result.graph.items().len(), 2);
}
