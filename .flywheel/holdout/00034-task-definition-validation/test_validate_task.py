"""Held-out acceptance test (spec 00034): static task-definition validation.

criterion 1: a command grader run referencing a repo-relative path that does not
  exist -> validate_task returns a defect naming the task id and the missing path.
criterion 2: an empty or unparseable command grader run -> a defect.
criterion 3: a task whose graders all parse and whose referenced paths all exist
  -> no defects.

validate_task is STATIC: it never executes a grader. Authored blind from the
declared contract (signature validate_task(task, *, repo_root) -> list of defects,
each carrying .task_id and .detail; exported from flywheel_core). Outside the four
pytest testpaths; collected explicitly by the grader (-k validate_task).
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core import CommandGrader, Task, validate_task


def _task(task_id: str, run: str) -> Task:
    return Task(id=task_id, goal="g.", graders=[CommandGrader(run=run)])


def _details(defects: list) -> str:
    return " || ".join(getattr(d, "detail", str(d)) for d in defects)


def test_validate_task_flags_missing_path(tmp_path: Path) -> None:
    # repo_root has NO .flywheel/holdout/missing-dir.
    task = _task("t-missing", "uv run pytest .flywheel/holdout/missing-dir/ -q")
    defects = validate_task(task, repo_root=tmp_path)

    assert defects, "a grader referencing a non-existent path must be flagged"
    assert any(getattr(d, "task_id", None) == "t-missing" for d in defects)
    blob = _details(defects)
    assert ".flywheel/holdout/missing-dir" in blob, (
        f"the defect must name the missing path; got {blob!r}"
    )


def test_validate_task_flags_unparseable_and_empty_run(tmp_path: Path) -> None:
    # An unterminated shell construct (`bash -n` rejects it).
    bad = validate_task(_task("t-parse", "for f in"), repo_root=tmp_path)
    assert any(getattr(d, "task_id", None) == "t-parse" for d in bad), (
        "an unparseable command grader run must be flagged"
    )
    # An empty run. The schema guard forbids constructing an empty ``run`` at
    # build time (test_command_grader_rejects_empty_run is a locked invariant),
    # so the empty value is set past ``__post_init__`` to exercise
    # validate_task's static empty-run catch -- the defensive layer for a
    # definition that still reaches the validator.
    empty_grader = CommandGrader(run="placeholder")
    empty_grader.run = ""
    empty = validate_task(
        Task(id="t-empty", goal="g.", graders=[empty_grader]),
        repo_root=tmp_path,
    )
    assert any(getattr(d, "task_id", None) == "t-empty" for d in empty), (
        "an empty command grader run must be flagged"
    )


def test_validate_task_accepts_valid_graders(tmp_path: Path) -> None:
    # Create the path the grader references so it exists under repo_root.
    real = tmp_path / ".flywheel" / "holdout" / "real-dir"
    real.mkdir(parents=True)
    task = _task("t-ok", "uv run pytest .flywheel/holdout/real-dir/ -q")
    defects = validate_task(task, repo_root=tmp_path)
    assert defects == [], (
        f"a task whose graders parse and whose paths exist must have no defects; "
        f"got {_details(defects)!r}"
    )


def test_validate_task_does_not_flag_flags_or_urls(tmp_path: Path) -> None:
    # A run with only flags + a tool + a URL has no repo-relative path token, so
    # the conservative heuristic must not false-positive (defends D-2).
    task = _task("t-url", "curl -sSL https://example.invalid/x.json --fail")
    assert validate_task(task, repo_root=tmp_path) == []
