"""Unit tests for static task-definition validation (spec 00034).

The validator flags a ``command`` grader the harness could not even run — an
empty or unparseable shell command. The missing-path check ("check #3") is
TABLED (see ``flywheel_core.validation``'s module docstring): it ran
pre-execution and could not tell an input path from an output path a task
creates, so it silently stranded legitimate "generate file X and verify it"
tasks. These tests lock the surviving checks and guard that a missing path no
longer blocks a task.
"""

from __future__ import annotations

from collections.abc import Sequence
from pathlib import Path

from flywheel_core import CommandGrader, Task, validate_task


def _task(run: str, task_id: str = "t") -> Task:
    return Task(id=task_id, goal="g.", graders=[CommandGrader(run=run)])


def _details(defects: Sequence[object]) -> str:
    return " || ".join(getattr(d, "detail", str(d)) for d in defects)


# --- surviving checks: empty + unparseable ----------------------------------


def test_whitespace_only_run_is_a_defect(tmp_path: Path) -> None:
    # An empty "" run is rejected at CommandGrader construction; a
    # whitespace-only run constructs and reaches validate_task's empty check.
    defects = validate_task(_task("   \t  "), repo_root=tmp_path)
    assert len(defects) == 1
    assert "empty" in defects[0].detail


def test_unparseable_run_is_a_defect(tmp_path: Path) -> None:
    # An unterminated single quote does not parse under ``bash -n``.
    defects = validate_task(_task("echo 'unterminated"), repo_root=tmp_path)
    assert len(defects) == 1
    assert "does not parse" in defects[0].detail


def test_valid_run_has_no_defects(tmp_path: Path) -> None:
    assert validate_task(
        _task("uv run pytest && cargo test --workspace"), repo_root=tmp_path
    ) == []


def test_multiple_graders_report_per_grader(tmp_path: Path) -> None:
    task = Task(
        id="multi",
        goal="g.",
        graders=[CommandGrader(run="cargo test"), CommandGrader(run="   ")],
    )
    defects = validate_task(task, repo_root=tmp_path)
    assert len(defects) == 1
    assert defects[0].task_id == "multi"
    assert "empty" in defects[0].detail


# --- tabled: a missing path no longer blocks --------------------------------


def test_missing_input_path_no_longer_flags(tmp_path: Path) -> None:
    # Previously a defect (check #3); now it runs and would fail honestly at
    # most one cycle rather than being statically stranded.
    assert validate_task(
        _task("uv run pytest pkg/missing/ -q"), repo_root=tmp_path
    ) == []


def test_creation_task_postcondition_grader_is_allowed(tmp_path: Path) -> None:
    # The real case this unblocks: a task that CREATES a file and verifies it.
    # The grader references a path absent at validation time (pre-execution);
    # it must not be rejected.
    run = (
        "test -f .github/workflows/ci.yml && "
        "python3 -c \"import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))\""
    )
    assert validate_task(_task(run), repo_root=tmp_path) == []
