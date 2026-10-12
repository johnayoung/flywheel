//! Static task-definition validation (spec 00034).
//!
//! A grader is the spine of a task; an un-runnable grader is a silent hole
//! in the verification surface that masquerades as "not done yet." This
//! module flags a `command` grader the harness could not even *run* -- an
//! empty or unparseable shell command -- *before* a worker burns a cycle on
//! it.
//!
//! The check is STATIC (D-1): it never executes a grader. Shell
//! parseability is checked with `bash -n` (parse only, no run). It lives
//! here, not in the purity-enforced `crate::task`, because the shell check
//! spawns a process -- IO belongs where IO is allowed.
//!
//! It deliberately does NOT judge a grader that parses and runs but is
//! semantically wrong for its tool (e.g. a bad CLI flag): that is
//! undecidable statically and is named out of scope by the spec.
//!
//! TABLED -- the missing-path check (spec 00034 "check #3"). An earlier
//! version also flagged a `command` grader referencing a repo-relative path
//! that did not exist under `repo_root`. It ran *pre-execution* and so could
//! not tell an INPUT path (`pytest tests/typo.py` -- a real defect) from an
//! OUTPUT path the task is about to create (`test -f ci.yml` for a
//! "generate ci.yml and verify it" task -- a legitimate postcondition). The
//! asymmetry favors not blocking: a false positive silently strands a valid
//! task forever, while a genuinely un-runnable path-grader merely wastes one
//! self-correcting cycle. `repo_root` is retained on [`validate_task`] for a
//! future revival that discriminates input from output.

use crate::task::{Grader, Task};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, Stdio};

/// A single static defect found in a task's definition.
///
/// `task_id` names the owning task; `detail` is a human-readable
/// description naming the specific problem (the empty or unparseable
/// command). The verb and the orchestrator render this one shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskDefect {
    pub task_id: String,
    pub detail: String,
}

/// Return the static defects in `task`'s graders (empty if none).
///
/// Only `command` graders gain these checks; transcript/rubric/manual
/// graders are validated solely by their existing schema rules. For each
/// command grader, in order:
///
/// * an empty (or whitespace-only) `run` is a defect;
/// * a `run` that `bash -n` cannot parse is a defect;
/// * a host where `bash` cannot be spawned at all is reported as a defect
///   too, since such a grader could never run there either.
///
/// Never executes a grader (D-1). `repo_root` is currently unused -- it is
/// retained for the tabled missing-path check (see the module docs) so a
/// future revival re-wires without churning callers.
pub fn validate_task(task: &Task, repo_root: &Path) -> Vec<TaskDefect> {
    let _ = repo_root;
    let mut defects = Vec::new();
    for grader in &task.graders {
        let Grader::Command(grader) = grader else {
            continue;
        };
        let run = grader.run.as_str();
        if run.trim().is_empty() {
            defects.push(TaskDefect {
                task_id: task.id.clone(),
                detail: "command grader 'run' is empty".to_owned(),
            });
            continue;
        }
        match parses_as_shell(run) {
            Ok(true) => {}
            Ok(false) => defects.push(TaskDefect {
                task_id: task.id.clone(),
                detail: format!("command grader 'run' does not parse as a shell command: {run:?}"),
            }),
            Err(err) => defects.push(TaskDefect {
                task_id: task.id.clone(),
                detail: format!(
                    "command grader 'run' could not be checked: failed to spawn `bash -n`: {err}"
                ),
            }),
        }
    }
    defects
}

/// `Ok(true)` when `bash -n` accepts `run` (parse, never run); `Err` only
/// when `bash` itself could not be spawned.
fn parses_as_shell(run: &str) -> std::io::Result<bool> {
    let status = Command::new("bash")
        .args(["-n", "-c", run])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::CommandGrader;

    fn task(run: &str, id: &str) -> Task {
        Task {
            id: id.to_owned(),
            goal: "g.".to_owned(),
            graders: vec![Grader::Command(CommandGrader {
                run: run.to_owned(),
                name: None,
            })],
            tags: Vec::new(),
            context: Default::default(),
            budgets: Default::default(),
        }
    }

    #[test]
    fn whitespace_only_run_is_a_defect() {
        let defects = validate_task(&task("   \t  ", "t"), Path::new("."));
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].task_id, "t");
        assert!(defects[0].detail.contains("empty"));
    }

    #[test]
    fn unparseable_run_is_a_defect() {
        let defects = validate_task(&task("echo 'unterminated", "t"), Path::new("."));
        assert_eq!(defects.len(), 1);
        assert!(
            defects[0].detail.contains("does not parse"),
            "{}",
            defects[0].detail
        );
        assert!(defects[0].detail.contains("echo 'unterminated"));
    }

    #[test]
    fn valid_run_has_no_defects() {
        assert!(
            validate_task(
                &task("uv run pytest && cargo test --workspace", "t"),
                Path::new(".")
            )
            .is_empty()
        );
    }

    #[test]
    fn multiple_graders_report_per_grader() {
        let mut t = task("cargo test", "multi");
        t.graders.push(Grader::Command(CommandGrader {
            run: "   ".to_owned(),
            name: None,
        }));
        t.graders
            .push(Grader::Transcript(crate::task::TranscriptGrader {
                max_turns: Some(1),
                ..Default::default()
            }));
        let defects = validate_task(&t, Path::new("."));
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].task_id, "multi");
        assert!(defects[0].detail.contains("empty"));
    }

    #[test]
    fn missing_input_path_no_longer_flags() {
        assert!(
            validate_task(
                &task("uv run pytest pkg/missing/ -q", "t"),
                Path::new("/nonexistent")
            )
            .is_empty()
        );
    }

    #[test]
    fn creation_task_postcondition_grader_is_allowed() {
        let run = "test -f .github/workflows/ci.yml && \
                   python3 -c \"import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))\"";
        assert!(validate_task(&task(run, "t"), Path::new(".")).is_empty());
    }

    #[test]
    fn never_executes_the_grader() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let run = format!("touch {}", marker.display());
        assert!(validate_task(&task(&run, "t"), dir.path()).is_empty());
        assert!(!marker.exists(), "validate_task must parse, never run");
    }
}
