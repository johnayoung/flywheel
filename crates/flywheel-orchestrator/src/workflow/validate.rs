//! The `validate` verb: static, pre-dispatch checks over the active listing
//! (spec 00034 + the surface-overlap lint).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use flywheel_core::validation::{TaskDefect, validate_task};

use crate::sources::load_active_tasks;
use crate::surface_lint::{build_surface, surface_overlap_defects};

use super::{WorkflowError, repo_root_for_tasks_dir};

/// Statically validate every active task under `tasks_dir`.
///
/// Per task, each command grader must shell-parse; pairwise, two tasks'
/// derived file surfaces must not overlap without a shared `conflict_keys`
/// entry, an `overlap_ok` marker, or a prerequisite chain. Exits 1 naming
/// each offending task and its defects (the count goes to stderr); exits 0
/// with `All N active task(s) valid.` otherwise. Never runs a grader, reads
/// the store, or tests whether a derived path exists.
pub fn cmd_validate(
    tasks_dir: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let repo_root = repo_root_for_tasks_dir(tasks_dir);
    let loaded = load_active_tasks(tasks_dir)?;
    let mut invalid: BTreeMap<String, Vec<TaskDefect>> = BTreeMap::new();
    for (_, task) in &loaded {
        let defects = validate_task(task, &repo_root);
        if !defects.is_empty() {
            invalid.entry(task.id.clone()).or_default().extend(defects);
        }
    }
    let surfaces: Vec<_> = loaded
        .iter()
        .map(|(path, task)| build_surface(path, task))
        .collect();
    for defect in surface_overlap_defects(&surfaces) {
        invalid
            .entry(defect.task_id.clone())
            .or_default()
            .push(defect);
    }
    let valid_count = loaded.len() - invalid.len();
    if invalid.is_empty() {
        writeln!(out, "All {valid_count} active task(s) valid.")?;
        return Ok(0);
    }
    for (task_id, defects) in &invalid {
        writeln!(out, "{task_id}: invalid task definition")?;
        for defect in defects {
            writeln!(out, "  - {}", defect.detail)?;
        }
    }
    writeln!(
        err,
        "{} invalid task(s); {valid_count} valid.",
        invalid.len()
    )?;
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn run(tmp: &TempDir) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cmd_validate(tmp.path(), &mut out, &mut err).unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn valid_listing_counts_tasks() {
        let tmp = TempDir::new().unwrap();
        write_task(&tmp.path().join("active/01/a.json"), "a", &[]);
        write_task(&tmp.path().join("active/02/b.json"), "b", &["a"]);
        assert_eq!(
            run(&tmp),
            (0, "All 2 active task(s) valid.\n".into(), String::new())
        );
    }

    #[test]
    fn defects_are_named_per_task_in_sorted_order() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("01");
        write_task_json(
            &phase.join("bad.json"),
            &json!({"id": "bad", "goal": "g", "graders": [{"type": "command", "run": "echo \"unterminated"}]}),
        );
        write_task_json(
            &phase.join("x.json"),
            &json!({"id": "x", "goal": "g", "graders": [{"type": "command", "run": "cat src/shared.py"}], "context": {"relevant": ["src/shared.py"]}}),
        );
        write_task_json(
            &phase.join("y.json"),
            &json!({"id": "y", "goal": "g", "graders": [{"type": "command", "run": "true"}], "context": {"relevant": ["src/shared.py"]}}),
        );
        write_task(&phase.join("ok.json"), "ok", &[]);
        let (code, out, err) = run(&tmp);
        assert_eq!(code, 1);
        assert_eq!(
            out,
            "bad: invalid task definition\n  - command grader 'run' does not parse as a shell command: 'echo \"unterminated'\nx: invalid task definition\n  - surface overlap between tasks 'x' and 'y' on: src/shared.py. Resolve by declaring a shared conflict_keys entry, listing each path in one task's overlap_ok, or chaining the tasks via prerequisites.\n"
        );
        assert_eq!(err, "2 invalid task(s); 2 valid.\n");
    }
}
