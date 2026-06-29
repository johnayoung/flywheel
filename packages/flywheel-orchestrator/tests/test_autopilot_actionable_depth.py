"""Spec 00062: autopilot queue depth counts *actionable* work, not raw files.

A landed task's JSON lingers under ``active/<phase>/`` until its whole phase
archives (all-or-nothing, gated on every task DONE). Counting those terminal
files would pin depth at target and suppress refill -- and a single FAILED task
(now reachable via the spec-00061 landable-change gate) would never archive and
wedge intake forever. ``actionable_queue_depth`` excludes DONE/FAILED tasks so
depth tracks drivable work; the raw ``_directory_queue_depth`` does not.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator._autopilot import (
    _directory_queue_depth,
    actionable_queue_depth,
)


def _write_task(phase: Path, task_id: str) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    (phase / f"{task_id}.json").write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )


def _seed(store: SqliteStore, task_id: str, *terminal: Status) -> None:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    for status in terminal:
        if status is Status.DONE:
            lc.transition_to(Status.VALIDATING, now=now)
            lc.transition_to(Status.DONE, now=now)
        elif status is Status.FAILED:
            lc.transition_to(Status.FAILED, error="boom", now=now)
        else:
            lc.transition_to(status, now=now)
    store.create_lifecycle(lc)


def test_actionable_depth_excludes_terminal_tasks(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "autopilot"
    for tid in ("fresh", "done", "failed", "running", "interrupted"):
        _write_task(phase, tid)

    store = SqliteStore(":memory:")
    # "fresh" gets no lifecycle at all.
    _seed(store, "done", Status.DONE)
    _seed(store, "failed", Status.FAILED)
    _seed(store, "running")  # ends RUNNING (non-terminal)
    _seed(store, "interrupted", Status.INTERRUPTED)

    # Raw depth counts every file; actionable depth drops the two terminal ones.
    assert _directory_queue_depth(tmp_path) == 5
    assert actionable_queue_depth(tmp_path, store) == 3


def test_actionable_depth_drops_to_zero_when_all_terminal(tmp_path: Path) -> None:
    # The 1b wedge: a fully-terminal batch (one DONE, one FAILED) that has not
    # yet archived must report depth 0 so refill is not suppressed.
    phase = tmp_path / "active" / "autopilot"
    _write_task(phase, "landed")
    _write_task(phase, "dead")
    store = SqliteStore(":memory:")
    _seed(store, "landed", Status.DONE)
    _seed(store, "dead", Status.FAILED)

    assert _directory_queue_depth(tmp_path) == 2
    assert actionable_queue_depth(tmp_path, store) == 0


def test_actionable_depth_survives_store_errors(tmp_path: Path) -> None:
    # A store read that blows up must never crash a daemon cycle; the task is
    # counted as actionable (conservative -- never under-reports depth).
    phase = tmp_path / "active" / "autopilot"
    _write_task(phase, "a")
    _write_task(phase, "b")

    class _BrokenStore:
        def list_lifecycles(self, **_kwargs: object) -> list[object]:
            raise RuntimeError("database is locked")

    assert actionable_queue_depth(tmp_path, _BrokenStore()) == 2  # type: ignore[arg-type]
