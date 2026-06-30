"""Tests for the ``fw validate`` / ``flywheel validate`` verb (spec 00034).

Criterion 4 (visible): over an active task set, the verb exits non-zero and
names each invalid task when any is statically invalid; exit 0 when all are
valid. Drives :func:`flywheel.main` directly so the verb is exercised through
the router seam the console script takes.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from flywheel import main


def _write_task(phase: Path, task_id: str, run: str) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    (phase / f"{task_id}.json").write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": run}],
            }
        )
    )


def _tasks_dir(repo_root: Path) -> Path:
    # Tasks live under <repo_root>/.flywheel/tasks so the verb resolves the
    # repo root (where grader path tokens are rooted) from the layout.
    return repo_root / ".flywheel" / "tasks"


def test_validate_mixed_corpus_exits_nonzero_and_names_invalid(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    tasks_dir = _tasks_dir(tmp_path)
    phase = tasks_dir / "active" / "01-phase"
    _write_task(phase, "valid-one", "uv run pytest pkg/ -q")
    # An unparseable command (unterminated quote) is still statically invalid;
    # the missing-path check is tabled, so a missing path no longer counts.
    _write_task(phase, "broken-one", "uv run pytest 'unterminated")

    rc = main(["validate", "--tasks-dir", str(tasks_dir)])

    out = capsys.readouterr().out
    assert rc != 0, "a statically-invalid task must make the verb exit non-zero"
    assert "broken-one" in out, "the invalid task must be named in the output"
    assert "does not parse" in out, "the unparseable command must be reported"
    # The valid task is not reported as invalid.
    assert "valid-one: invalid" not in out


def test_validate_all_valid_exits_zero(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    tasks_dir = _tasks_dir(tmp_path)
    phase = tasks_dir / "active" / "01-phase"
    _write_task(phase, "ok-a", "true")
    _write_task(phase, "ok-b", "uv run pytest -q")

    rc = main(["validate", "--tasks-dir", str(tasks_dir)])

    assert rc == 0, "an all-valid active set must exit 0"
    assert "invalid task definition" not in capsys.readouterr().out


def test_validate_empty_active_set_exits_zero(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    tasks_dir = _tasks_dir(tmp_path)
    (tasks_dir / "active").mkdir(parents=True)

    rc = main(["validate", "--tasks-dir", str(tasks_dir)])

    assert rc == 0, "an empty active set has nothing invalid -> exit 0"
