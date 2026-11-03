//! The hub's verified fan-outs grade with flywheel-core's command-grader
//! runner: this module implements `flywheel_hub::fanout::GraderRunner`
//! over `flywheel_core::grader_command::run_command_graders`, and `fw hub
//! serve` injects it (the hub crate sits below the loop's crates and never
//! depends on them).
//!
//! Same semantics as a task's command graders: in list order, each `/bin/sh
//! -c <run>` in the candidate's worktree in its own process group, pass =
//! exit 0, the whole group SIGKILLed at the per-grader timeout, the first
//! grader that does not pass ends the run, stdout/stderr kept as bounded
//! tails. The receipts go to an in-memory store (the hub keeps them in its
//! own state); a cancelled grading drops the runner's future, which kills
//! the grader's process group.

use chrono::Utc;
use flywheel_core::grader_command::{
    CommandGraderOptions, Termination, payload_signal, payload_termination, run_command_graders,
};
use flywheel_core::store::GraderResultRecord;
use flywheel_core::store_memory::InMemoryStore;
use flywheel_core::task::{CommandGrader, Grader, Task};
use flywheel_hub::fanout::{
    Cancel, GradeRequest, GraderRun, GraderRunner, GraderStatus, RunnerError,
};
use std::time::Duration;

/// How often a running grading looks at its cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// flywheel-core's command-grader runner, for the hub.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoreGraderRunner;

fn text(payload: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn flag(payload: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    payload
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// One core receipt as the hub's grader run.
fn to_run(record: &GraderResultRecord, run: &str) -> GraderRun {
    let payload = &record.payload;
    let exit_code = payload
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .and_then(|c| i32::try_from(c).ok());
    let termination = payload_termination(payload);
    let status = if record.passed {
        GraderStatus::Passed
    } else {
        match termination {
            Termination::Exited => GraderStatus::Failed,
            Termination::Timeout => GraderStatus::Timeout,
            Termination::Signal => GraderStatus::Signal,
            Termination::SpawnFailure => GraderStatus::SpawnFailure,
        }
    };
    let detail = match termination {
        Termination::Timeout => payload
            .get("timeout_seconds")
            .and_then(serde_json::Value::as_f64)
            .map(|secs| format!("limit {secs:.0}s")),
        Termination::SpawnFailure => Some(text(payload, "spawn_error")).filter(|e| !e.is_empty()),
        Termination::Exited | Termination::Signal => None,
    };
    GraderRun {
        name: record.grader_name.clone(),
        run: run.to_owned(),
        status,
        exit_code: exit_code.filter(|c| *c >= 0),
        signal: payload_signal(payload),
        duration_ms: record.duration_ms,
        stdout: text(payload, "stdout_tail"),
        stderr: text(payload, "stderr_tail"),
        stdout_truncated: flag(payload, "stdout_truncated"),
        stderr_truncated: flag(payload, "stderr_truncated"),
        detail,
        // The engine marks held-out graders from the fan-out's own list.
        held_out: false,
    }
}

async fn cancelled(cancel: &Cancel) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

impl GraderRunner for CoreGraderRunner {
    fn name(&self) -> String {
        "flywheel-core command graders".to_owned()
    }

    fn run(
        &self,
        request: &GradeRequest<'_>,
        cancel: &Cancel,
    ) -> Result<Vec<GraderRun>, RunnerError> {
        let graders: Vec<Grader> = request
            .graders
            .iter()
            .map(|g| {
                Grader::Command(CommandGrader {
                    run: g.run.clone(),
                    name: g.name.clone(),
                })
            })
            .collect();
        let task = Task::new("hub fan-out grading", graders);
        let store = InMemoryStore::new();
        let options = CommandGraderOptions {
            cwd: Some(request.dir.to_path_buf()),
            env: None,
            per_grader_timeout: Some(request.timeout),
            stdout_tail_bytes: request.tail_bytes,
            stderr_tail_bytes: request.tail_bytes,
            artifacts_dir: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| RunnerError::Failed(format!("cannot start the grader runtime: {e}")))?;
        let outcome = runtime.block_on(async {
            tokio::select! {
                records = run_command_graders(&task, &store, "hub-fanout", 1, &options, Utc::now) => Some(records),
                () = cancelled(cancel) => None,
            }
        });
        let records = match outcome {
            None => return Err(RunnerError::Cancelled),
            Some(Ok(records)) => records,
            Some(Err(e)) => return Err(RunnerError::Failed(e.to_string())),
        };
        Ok(records
            .iter()
            .map(|record| {
                let index = usize::try_from(record.ordinal).unwrap_or(usize::MAX);
                let run = request
                    .graders
                    .get(index)
                    .map(|g| g.run.as_str())
                    .unwrap_or_default();
                to_run(record, run)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_hub::fanout::GraderSpec;
    use std::time::Instant;

    fn spec(run: &str, name: Option<&str>) -> GraderSpec {
        GraderSpec {
            run: run.to_owned(),
            name: name.map(str::to_owned),
            held_out: false,
        }
    }

    fn grade(dir: &std::path::Path, graders: &[GraderSpec], timeout: Duration) -> Vec<GraderRun> {
        CoreGraderRunner
            .run(
                &GradeRequest {
                    dir,
                    graders,
                    timeout,
                    tail_bytes: 4096,
                },
                &Cancel::new(),
            )
            .unwrap()
    }

    #[test]
    fn graders_run_in_the_worktree_and_stop_at_the_first_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("answer.txt"), "41\n").unwrap();
        let graders = [
            spec("test -f answer.txt && echo found", Some("exists")),
            spec(
                "a=$(cat answer.txt); echo \"got $a\"; echo oops >&2; test \"$a\" = 42",
                Some("answer"),
            ),
            spec("echo never", None),
        ];
        let runs = grade(dir.path(), &graders, Duration::from_secs(30));
        assert_eq!(runs.len(), 2, "the run ends at the first failure: {runs:?}");
        assert_eq!(runs[0].status, GraderStatus::Passed);
        assert_eq!(runs[0].stdout.trim(), "found");
        assert_eq!(runs[0].name.as_deref(), Some("exists"));
        assert_eq!(runs[1].status, GraderStatus::Failed);
        assert_eq!(runs[1].exit_code, Some(1));
        assert_eq!(runs[1].stdout.trim(), "got 41");
        assert_eq!(runs[1].stderr.trim(), "oops");
        assert_eq!(runs[1].run, graders[1].run);

        std::fs::write(dir.path().join("answer.txt"), "42\n").unwrap();
        let runs = grade(dir.path(), &graders, Duration::from_secs(30));
        assert!(runs.iter().all(GraderRun::passed), "{runs:?}");
        assert_eq!(runs.len(), 3);
    }

    #[test]
    fn a_grader_past_its_time_limit_is_killed_and_fails() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let runs = grade(
            dir.path(),
            &[spec("sleep 30; echo late", None)],
            Duration::from_millis(300),
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(runs[0].status, GraderStatus::Timeout);
        assert!(
            runs[0].outcome().contains("timed out"),
            "{}",
            runs[0].outcome()
        );
        assert!(!runs[0].stdout.contains("late"));
    }

    #[test]
    fn a_missing_directory_is_a_spawn_failure_and_cancel_stops_a_run() {
        let runs = grade(
            std::path::Path::new("/nonexistent/fanout/worktree"),
            &[spec("true", None)],
            Duration::from_secs(5),
        );
        assert_eq!(runs[0].status, GraderStatus::SpawnFailure);
        assert!(runs[0].detail.is_some());

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-running");
        let cancel = Cancel::new();
        let flag = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.cancel();
        });
        let started = Instant::now();
        let graders = [spec(&format!("sleep 3; touch {}", marker.display()), None)];
        let result = CoreGraderRunner.run(
            &GradeRequest {
                dir: dir.path(),
                graders: &graders,
                timeout: Duration::from_secs(60),
                tail_bytes: 1024,
            },
            &cancel,
        );
        canceller.join().unwrap();
        assert_eq!(result.unwrap_err(), RunnerError::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !marker.exists(),
            "a cancelled grader's process group is killed"
        );
    }
}
