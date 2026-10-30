//! `validating`-seat routing for a command grader whose subprocess never
//! started: a harness crash (`INTERNAL_ERROR`), never a verdict on the
//! agent's work (`FAILED_VALIDATION`).

use super::support::*;
use crate::events::DomainEventPayload;
use crate::harness::HarnessConfig;
use crate::lifecycle::{Outcome, Status};
use crate::store::{DomainEventStore, GraderResultStore, LifecycleStore};
use crate::task::{Grader, GraderType, TranscriptGrader};
use std::path::PathBuf;

/// A worktree path that does not exist: every command grader's spawn fails
/// on the missing working directory before the child process starts.
fn missing_worktree(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("worktree-removed-before-grading")
}

fn spawn_failure_config(dir: &tempfile::TempDir, max_retries: u32) -> HarnessConfig {
    HarnessConfig {
        worktree: Some(missing_worktree(dir)),
        max_retries,
        ..HarnessConfig::default()
    }
}

/// A transcript grader no scripted iteration can breach, placed ahead of a
/// command grader so the command grader's ordinal is not zero.
fn lenient_transcript() -> Grader {
    Grader::Transcript(TranscriptGrader {
        max_turns: Some(1_000),
        name: Some("lenient".into()),
        ..TranscriptGrader::default()
    })
}

#[tokio::test]
async fn spawn_failed_command_grader_is_an_internal_error_not_a_validation_failure() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![
        lenient_transcript(),
        command("true", Some("build")),
        command("true", Some("never-run")),
    ]);
    let mut lc = lifecycle("run-spawn-fail");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &spawn_failure_config(&dir, 0), invoke)
        .await;

    // Retry budget 0: INTERNAL_ERROR walks straight on to FAILED.
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.attempts.len(), 1);
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    assert!(
        attempt
            .error
            .starts_with("command grader \"build\" failed to start: "),
        "unexpected attempt error: {:?}",
        attempt.error
    );

    // The spawn error text recorded on the grader row is carried verbatim.
    let rows = h.store.list_grader_results("run-spawn-fail", 1).unwrap();
    assert_eq!(rows.len(), 1, "the first spawn failure stops the suite");
    let row = &rows[0];
    assert_eq!(row.grader_type, GraderType::Command);
    assert_eq!(row.ordinal, 1);
    let spawn_error = row.payload["spawn_error"].as_str().unwrap();
    assert!(!spawn_error.is_empty());
    assert_eq!(
        attempt.error,
        format!("command grader \"build\" failed to start: {spawn_error}")
    );

    // One harness.crash carrying the spawn-failure classification and the
    // failing grader's identity; never a signal or judge classification.
    let crashes = h.sink.of_kind("run-spawn-fail", "harness.crash");
    assert_eq!(crashes.len(), 1);
    let crash = &crashes[0].payload;
    assert_eq!(crash["classification"], "grader_spawn_failure");
    assert_eq!(crash["grader_name"], "build");
    assert_eq!(crash["grader_ordinal"], 1);
    assert_eq!(crash["message"], attempt.error.as_str());
    assert_eq!(crashes[0].attempt_number, Some(1));

    // The ledger walks VALIDATING -> INTERNAL_ERROR -> FAILED, never
    // through FAILED_VALIDATION.
    let targets: Vec<Status> = h
        .store
        .list_domain_events("run-spawn-fail")
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.payload {
            DomainEventPayload::TransitionedTo(t) => Some(t.target),
            _ => None,
        })
        .collect();
    assert_eq!(
        targets[targets.len() - 3..],
        [Status::Validating, Status::InternalError, Status::Failed],
        "transitions: {targets:?}"
    );
    assert!(!targets.contains(&Status::FailedValidation));
    let reloaded = h.store.load_lifecycle("run-spawn-fail").unwrap().unwrap();
    assert_eq!(reloaded.status, Status::Failed);
}

#[tokio::test]
async fn spawn_failure_of_an_unnamed_grader_is_labelled_by_its_type() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![command("true", None)]);
    let mut lc = lifecycle("run-spawn-unnamed");
    let (invoke, _) = scripted_invoker(vec![verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &spawn_failure_config(&dir, 0), invoke)
        .await;
    let attempt = &outcome.attempts[0];
    assert_eq!(attempt.outcome, Some(Outcome::InternalError));
    assert!(
        attempt
            .error
            .starts_with("command grader \"command\" failed to start: "),
        "unexpected attempt error: {:?}",
        attempt.error
    );
    let crashes = h.sink.of_kind("run-spawn-unnamed", "harness.crash");
    assert_eq!(crashes.len(), 1);
    assert_eq!(crashes[0].payload["grader_name"], serde_json::Value::Null);
    assert_eq!(crashes[0].payload["grader_ordinal"], 0);
}

#[tokio::test]
async fn spawn_failure_consumes_a_retry_like_any_internal_error() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let task = task(vec![command("true", Some("build"))]);
    let mut lc = lifecycle("run-spawn-retry");
    let (invoke, calls) = scripted_invoker(vec![verify_iteration(), verify_iteration()]);
    let outcome = h
        .run(&task, &mut lc, &spawn_failure_config(&dir, 1), invoke)
        .await;
    assert_eq!(
        calls.lock().unwrap().len(),
        2,
        "the retry re-invokes the agent"
    );
    assert_eq!(outcome.lifecycle.status, Status::Failed);
    assert_eq!(outcome.lifecycle.retries, 1);
    assert_eq!(outcome.attempts.len(), 2);
    for attempt in &outcome.attempts {
        assert_eq!(attempt.outcome, Some(Outcome::InternalError));
        assert!(attempt.error.contains("failed to start"));
    }
    let scheduled = h.sink.of_kind("run-spawn-retry", "harness.retry_scheduled");
    assert_eq!(scheduled.len(), 1);
    assert_eq!(h.sink.of_kind("run-spawn-retry", "harness.crash").len(), 2);
}
