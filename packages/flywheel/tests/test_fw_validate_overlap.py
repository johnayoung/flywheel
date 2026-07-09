"""End-to-end tests for the surface-overlap lint on the ``validate`` verb.

Drives :func:`flywheel.main` through the router seam the console script takes,
mirroring ``test_fw_validate.py``. The lint flags two active tasks whose derived
file surfaces overlap with no shared ``conflict_keys`` entry, no ``overlap_ok``
allow marker, and no prerequisite chain -- the infrared 2026-07-09 shape.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from flywheel import main


def _write(phase: Path, task_id: str, body: dict[str, Any]) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    (phase / f"{task_id}.json").write_text(
        json.dumps({"id": task_id, "goal": f"Goal for {task_id}.", **body})
    )


def _tasks_dir(repo_root: Path) -> Path:
    return repo_root / ".flywheel" / "tasks"


def _grader(run: str) -> dict[str, Any]:
    return {"graders": [{"type": "command", "run": run}]}


def test_overlapping_surfaces_exit_nonzero_and_name_both(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    tasks_dir = _tasks_dir(tmp_path)
    phase = tasks_dir / "active" / "01-phase"
    # ov-a's authoritative grader writes the shared golden; ov-b lists it as a
    # relevant surface. Disjoint (empty) conflict keys, no overlap_ok, no chain.
    _write(phase, "ov-a", _grader("uv run pytest tests/shared/golden_test.py"))
    _write(
        phase,
        "ov-b",
        {
            **_grader("true"),
            "context": {
                "relevant": ["tests/shared/golden_test.py -- writes the golden"]
            },
        },
    )

    rc = main(["validate", "--tasks-dir", str(tasks_dir)])

    out = capsys.readouterr().out
    assert rc != 0, "an unguarded surface overlap must make the verb exit non-zero"
    assert "ov-a" in out
    assert "ov-b" in out
    assert "tests/shared/golden_test.py" in out


def test_shared_conflict_key_is_not_flagged(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    tasks_dir = _tasks_dir(tmp_path)
    phase = tasks_dir / "active" / "01-phase"
    _write(
        phase,
        "ov-a",
        {
            **_grader("uv run pytest tests/shared/golden_test.py"),
            "conflict_keys": ["golden-harness"],
        },
    )
    _write(
        phase,
        "ov-b",
        {
            **_grader("true"),
            "conflict_keys": ["golden-harness", "other"],
            "context": {
                "relevant": ["tests/shared/golden_test.py -- writes the golden"]
            },
        },
    )

    rc = main(["validate", "--tasks-dir", str(tasks_dir)])

    assert rc == 0, "a shared conflict key serializes the pair -> no defect"


def test_overlap_ok_exempts_the_listed_path(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    tasks_dir = _tasks_dir(tmp_path)
    phase = tasks_dir / "active" / "01-phase"
    _write(
        phase,
        "ov-a",
        {
            **_grader("uv run pytest tests/shared/golden_test.py"),
            "overlap_ok": ["tests/shared/golden_test.py"],
        },
    )
    _write(
        phase,
        "ov-b",
        {
            **_grader("true"),
            "context": {
                "relevant": ["tests/shared/golden_test.py -- writes the golden"]
            },
        },
    )

    rc = main(["validate", "--tasks-dir", str(tasks_dir)])

    assert rc == 0, "an overlap_ok marker on the shared path exempts the pair"


def test_prerequisite_chain_is_not_flagged(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    tasks_dir = _tasks_dir(tmp_path)
    phase = tasks_dir / "active" / "01-phase"
    _write(
        phase,
        "ov-a",
        {
            **_grader("uv run pytest tests/shared/golden_test.py"),
            "prerequisites": ["ov-b"],
        },
    )
    _write(
        phase,
        "ov-b",
        {
            **_grader("true"),
            "context": {
                "relevant": ["tests/shared/golden_test.py -- writes the golden"]
            },
        },
    )

    rc = main(["validate", "--tasks-dir", str(tasks_dir)])

    assert rc == 0, "a prerequisite chain runs the pair in sequence -> no defect"
