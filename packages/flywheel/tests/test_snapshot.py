"""Tests for the pure :mod:`flywheel._snapshot` collector.

These exercise the snapshot shape against a real SqliteStore so the
``--json`` and Textual surfaces share the same data path the
orchestrator's ``collect_live_rows`` already covers.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

from flywheel_core.lifecycle import Attempt, Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import DirectoryWorkSource

from flywheel._snapshot import build_snapshot, snapshot_to_dict


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
        store.save_attempt(
            running.run_id,
            Attempt(
                number=1,
                started_at=now,
                run_id=running.run_id,
                input_tokens=100,
                iterations_completed=1,
                turns=3,
                total_cost_usd=0.25,
                last_activity_at=now,
            ),
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
    """Summary counts mirror what ``flywheel status`` would emit
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
    """Timestamps strictly in the future (SQLite/host skew) read as 0s
    rather than a negative age/idle — mirrors ``_format_live_line``."""
    db = tmp_path / "db.sqlite"
    now = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
    future = now.replace(minute=5)
    store = SqliteStore(db)
    try:
        lc = Lifecycle(task_id="skewed", run_id="run-skewed-running")
        lc.transition_to(Status.READY, now=future)
        lc.transition_to(Status.RUNNING, now=future)
        store.create_lifecycle(lc)
        store.save_attempt(
            lc.run_id,
            Attempt(number=1, started_at=future, run_id=lc.run_id),
        )
        snap = build_snapshot(store, now=now, started_at=now)
    finally:
        store.close()
    assert len(snap.rows) == 1
    assert snap.rows[0].age_seconds == 0
    assert snap.rows[0].idle_seconds == 0


def test_build_snapshot_age_is_run_age_not_last_event_age(
    tmp_path: Path,
) -> None:
    """``age_seconds`` measures from the run's first lifecycle
    transition and keeps growing as fresh activity lands; the per-
    activity reset lives in ``idle_seconds``. Regression: the dashboard
    age column used to bounce back to 0 on every new event."""
    db = tmp_path / "db.sqlite"
    start = datetime(2026, 6, 10, 12, 0, tzinfo=timezone.utc)
    activity_ts = start.replace(minute=1, second=40)  # 100s in
    now = start.replace(minute=2)  # 120s in
    store = SqliteStore(db)
    try:
        lc = Lifecycle(task_id="aging", run_id="run-aging")
        lc.transition_to(Status.READY, now=start)
        lc.transition_to(Status.RUNNING, now=start)
        store.create_lifecycle(lc)
        store.save_attempt(
            lc.run_id,
            Attempt(
                number=1,
                started_at=start,
                run_id=lc.run_id,
                iterations_completed=1,
                last_activity_at=activity_ts,
            ),
        )
        snap = build_snapshot(store, now=now, started_at=now)
    finally:
        store.close()
    assert len(snap.rows) == 1
    assert snap.rows[0].age_seconds == 120
    assert snap.rows[0].idle_seconds == 20


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
        "idle_seconds",
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
