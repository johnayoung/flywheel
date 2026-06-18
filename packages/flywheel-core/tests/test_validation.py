"""Unit tests for static task-definition validation (spec 00034).

The held-out oracle proved the core criteria (missing path, empty/unparseable
run, all-valid). These package-level tests lock the conservative path heuristic
in :func:`flywheel_core.validation._missing_path_tokens` -- specifically that a
``shlex``-split token carrying unparsed shell syntax (a command substitution,
variable expansion, redirection, or pipe) is NOT existence-checked as a path,
the false positive that flagged the real ``phase-verify-gate`` grader.
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core import CommandGrader, Task, validate_task


def _task(run: str, task_id: str = "t") -> Task:
    return Task(id=task_id, goal="g.", graders=[CommandGrader(run=run)])


def _details(defects: list[object]) -> str:
    return " || ".join(getattr(d, "detail", str(d)) for d in defects)


def test_command_substitution_token_is_not_flagged_as_a_path(
    tmp_path: Path,
) -> None:
    # The real phase-verify-gate grader: a quoted "$(...)" whose embedded
    # pkg/dir paths must NOT be stat-ed -- the value is only known at run time.
    run = (
        "test \"$(uv run pytest packages/flywheel-worktree "
        "packages/flywheel-orchestrator --collect-only -q 2>/dev/null "
        "| grep -c '::')\" -ge 430"
    )
    assert validate_task(_task(run), repo_root=tmp_path) == [], (
        f"a command-substitution token must not be path-checked; "
        f"got {_details(validate_task(_task(run), repo_root=tmp_path))!r}"
    )


def test_variable_expansion_and_redirection_tokens_are_not_flagged(
    tmp_path: Path,
) -> None:
    # A ${VAR}/path expansion and an attached redirection (2>path/...) each
    # carry a shell-dynamic char inside the shlex token, so neither is
    # existence-checked. (A space-separated redirect target, e.g. "> a/b", is a
    # distinct clean token indistinguishable from a path arg and out of scope.)
    assert validate_task(_task("cat ${OUT_DIR}/report.txt"), repo_root=tmp_path) == []
    assert validate_task(_task("uv run x 2>logs/err.txt"), repo_root=tmp_path) == []


def test_a_genuine_missing_path_alongside_a_substitution_still_flags(
    tmp_path: Path,
) -> None:
    # The skip is per-token: a real missing literal path is still caught even
    # when another token is a substitution, so the fix narrows, not blunts.
    run = "test -d does/not/exist && echo \"$(date +%s)/x\""
    defects = validate_task(_task(run, "t-mix"), repo_root=tmp_path)
    blob = _details(defects)
    assert "does/not/exist" in blob, f"real missing path must still flag; got {blob!r}"
    assert "date" not in blob, f"the substitution must not be flagged; got {blob!r}"


def test_existing_literal_path_checks_are_unchanged(tmp_path: Path) -> None:
    # Regression guard: a plain missing literal path still flags, and an
    # existing one passes -- the new skip does not disturb the base behavior.
    missing = validate_task(_task("uv run pytest pkg/missing/ -q"), repo_root=tmp_path)
    assert "pkg/missing" in _details(missing)

    (tmp_path / "pkg" / "real").mkdir(parents=True)
    assert validate_task(_task("uv run pytest pkg/real/ -q"), repo_root=tmp_path) == []
