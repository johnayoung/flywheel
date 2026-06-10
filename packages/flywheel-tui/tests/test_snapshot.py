"""Tests for the pure :mod:`flywheel_tui._snapshot` collector.

These exercise the snapshot shape against a real SqliteStore so the
``--json`` and Textual surfaces share the same data path the
orchestrator's ``collect_live_rows`` already covers.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

from flywheel.lifecycle import Lifecycle, Status
from flywheel.store_protocols import EventRecord
from flywheel.store_sqlite import SqliteStore
from flywheel_orchestrator import DirectoryWorkSource

from flywheel_tui._snapshot import build_snapshot, snapshot_to_dict


def _seed_running(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-running")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_done(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-done")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.DONE, now=now)
    store.create_lifecycle(lc)
    return lc


def _write_task(path: Path, task_id: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = (
        '{"id": "%s", "goal": "Goal for %s.", '
        '"graders": [{"type": "command", "run": "true"}]}'
    ) % (task_id, task_id)
    path.write_text(payload)
    return path


def test_build_snapshot_empty_store_returns_empty_rows(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    now = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
    store = SqliteStore(db)
    try:
        snap = build_snapshot(store, now=now, started_at=now)
    finally:
        store.close()
    assert snap.rows == ()
    assert snap.summary.active_workers == 0
    assert snap.summary.tokens_total == 0
    assert snap.summary.cost_usd_total == 0.0


def test_build_snapshot_includes_running_rows(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    now = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "alpha")
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=now,
                kind="harness.iteration_completed",
                payload={
                    "iteration": 1,
                    "usage": {"total_tokens": 100},
                    "total_cost_usd": 0.25,
                    "num_turns": 3,
                },
                attempt_number=1,
            )
        )
        snap = build_snapshot(store, now=now, started_at=now)
    finally:
        store.close()
    assert len(snap.rows) == 1
    row = snap.rows[0]
    assert row.task_id == "alpha"
    assert row.status == "running"
    assert row.tokens == 100
    assert row.cost_usd == 0.25
    assert row.turns == 3
    assert row.iterations_completed == 1
    assert snap.summary.active_workers == 1
    assert snap.summary.tokens_total == 100
    assert snap.summary.cost_usd_total == 0.25


def test_build_snapshot_task_counts_match_status_states(tmp_path: Path) -> None:
    """Summary counts mirror what flywheel-orchestrate status would emit
    aggregated by ``TaskState`` for the same store + work source."""
    db = tmp_path / "db.sqlite"
    tasks = tmp_path / "tasks"
    _write_task(tasks / "active" / "01" / "fresh.json", "fresh-task")
    _write_task(tasks / "active" / "01" / "done.json", "done-task")
    _write_task(tasks / "active" / "01" / "running.json", "running-task")
    store = SqliteStore(db)
    try:
        _seed_done(store, "done-task")
        _seed_running(store, "running-task")
        now = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
        snap = build_snapshot(
            store,
            work_source=DirectoryWorkSource(tasks),
            now=now,
            started_at=now,
        )
    finally:
        store.close()
    counts = snap.summary.task_counts
    assert counts.get("fresh") == 1
    assert counts.get("done") == 1
    assert counts.get("in_progress") == 1


def test_build_snapshot_age_clamps_clock_skew_to_zero(tmp_path: Path) -> None:
    """A last_ts strictly in the future (SQLite/host skew) reads as 0s
    rather than a negative age — mirrors ``_format_live_line``."""
    db = tmp_path / "db.sqlite"
    now = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
    future = now.replace(minute=5)
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "skewed")
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=future,
                kind="harness.attempt_started",
                payload={},
                attempt_number=1,
            )
        )
        snap = build_snapshot(store, now=now, started_at=now)
    finally:
        store.close()
    assert len(snap.rows) == 1
    assert snap.rows[0].age_seconds == 0


def test_snapshot_to_dict_emits_full_schema(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    now = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
    store = SqliteStore(db)
    try:
        _seed_running(store, "beta")
        snap = build_snapshot(store, now=now, started_at=now)
    finally:
        store.close()
    encoded = snapshot_to_dict(snap)
    assert set(encoded.keys()) == {"summary", "rows"}
    summary = encoded["summary"]
    assert set(summary.keys()) == {
        "active_workers",
        "task_counts",
        "tokens_total",
        "cost_usd_total",
        "runtime_seconds",
    }
    assert encoded["rows"][0]["task_id"] == "beta"
    # Stable field names: scripts pipe this; the keys must not drift.
    assert set(encoded["rows"][0].keys()) == {
        "run_id",
        "task_id",
        "status",
        "attempt",
        "iteration",
        "age_seconds",
        "tokens",
        "cost_usd",
        "turns",
        "iterations_completed",
        "last_kind",
        "last_detail",
        "awaiting_instruction",
    }


def test_build_snapshot_runtime_seconds_against_started_at(
    tmp_path: Path,
) -> None:
    db = tmp_path / "db.sqlite"
    start = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
    now = start.replace(minute=5, second=30)
    store = SqliteStore(db)
    try:
        snap = build_snapshot(store, now=now, started_at=start)
    finally:
        store.close()
    assert snap.summary.runtime_seconds == 5 * 60 + 30
