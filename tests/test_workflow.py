"""Tests for the ``flywheel.workflow`` CLI module.

Covers the pure logic — task discovery, status classification, eligibility,
archive — against an in-memory SQLite store and temp task dirs. The ``run``
subcommand's real-agent path is intentionally not exercised end-to-end here;
``run_task_file``'s seam is the ``invoke`` callable, which lower-level
harness tests already cover.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path

import pytest

from flywheel.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel.store_sqlite import SqliteStore
from flywheel.workflow import (
    TaskState,
    archive_completed_phases,
    build_status_rows,
    iter_active_phase_dirs,
    iter_active_task_files,
    main,
    recover_stranded_lifecycles,
    select_next_task,
)


def _write_task(
    path: Path,
    task_id: str,
    *,
    prerequisites: list[str] | None = None,
) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": "true"}],
    }
    if prerequisites:
        payload["prerequisites"] = prerequisites
    path.write_text(json.dumps(payload))
    return path


def _seed_done(store: SqliteStore, task_id: str) -> Lifecycle:
    """Persist a single Lifecycle for ``task_id`` ending in DONE."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-ok")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.DONE, now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_failed(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-fail")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.FAILED, error="boom", now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_running(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-running")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)
    return lc


# ---------- Filesystem walking ----------


def test_iter_active_phase_dirs_orders_by_filename(tmp_path: Path) -> None:
    (tmp_path / "active" / "02-second").mkdir(parents=True)
    (tmp_path / "active" / "01-first").mkdir(parents=True)
    (tmp_path / "active" / "10-tenth").mkdir(parents=True)
    (tmp_path / "active" / ".hidden").mkdir(parents=True)

    dirs = list(iter_active_phase_dirs(tmp_path))
    names = [d.name for d in dirs]
    assert names == ["01-first", "02-second", "10-tenth"]


def test_iter_active_phase_dirs_handles_missing_root(tmp_path: Path) -> None:
    assert list(iter_active_phase_dirs(tmp_path)) == []


def test_iter_active_task_files_skips_underscore_and_hidden(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "active" / "01-only"
    _write_task(phase / "a.json", "a")
    _write_task(phase / "b.json", "b")
    (phase / "_phase.json").write_text("{}")
    (phase / ".secret.json").write_text("{}")
    (phase / "notes.md").write_text("text")

    files = [p.name for p in iter_active_task_files(tmp_path)]
    assert files == ["a.json", "b.json"]


# ---------- Status classification ----------


def test_build_status_rows_classifies_fresh_done_failed_running(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "fresh.json", "fresh")
    _write_task(phase / "done.json", "done")
    _write_task(phase / "failed.json", "failed")
    _write_task(phase / "running.json", "running")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "done")
        _seed_failed(store, "failed")
        _seed_running(store, "running")

        rows = build_status_rows(tmp_path, store)
        state_by_id = {row.task.id: row.state for row in rows}
        assert state_by_id == {
            "fresh": TaskState.FRESH,
            "done": TaskState.DONE,
            "failed": TaskState.RETRYABLE,
            "running": TaskState.IN_PROGRESS,
        }
    finally:
        store.close()


def test_build_status_rows_treats_later_done_as_done(tmp_path: Path) -> None:
    """A task with an earlier failure + later done should classify DONE."""
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "retried.json", "retried")
    store = SqliteStore(":memory:")
    try:
        _seed_failed(store, "retried")
        _seed_done(store, "retried")
        rows = build_status_rows(tmp_path, store)
        assert rows[0].state == TaskState.DONE
    finally:
        store.close()


# ---------- Eligibility / next-task selection ----------


def test_select_next_picks_first_fresh_in_walk_order(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "a.json", "a")
    _write_task(phase / "b.json", "b")

    store = SqliteStore(":memory:")
    try:
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None
        assert pick.task.id == "a"
    finally:
        store.close()


def test_select_next_respects_prerequisites(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "first.json", "first")
    _write_task(phase / "second.json", "second", prerequisites=["first"])

    store = SqliteStore(":memory:")
    try:
        # 'first' is the only eligible task.
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "first"

        # After 'first' is done, 'second' becomes eligible.
        _seed_done(store, "first")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "second"
    finally:
        store.close()


def test_select_next_skips_in_progress_and_done(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "a.json", "a")
    _write_task(phase / "b.json", "b")
    _write_task(phase / "c.json", "c")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "a")
        _seed_running(store, "b")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "c"
    finally:
        store.close()


def test_select_next_retries_failed_task(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "broken.json", "broken")

    store = SqliteStore(":memory:")
    try:
        _seed_failed(store, "broken")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "broken"
        assert pick.state == TaskState.RETRYABLE
    finally:
        store.close()


def test_select_next_returns_none_when_prereq_missing(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "only.json", "only", prerequisites=["ghost"])
    store = SqliteStore(":memory:")
    try:
        rows = build_status_rows(tmp_path, store)
        assert select_next_task(rows) is None
    finally:
        store.close()


def test_select_next_returns_none_when_all_done(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "a.json", "a")
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "a")
        rows = build_status_rows(tmp_path, store)
        assert select_next_task(rows) is None
    finally:
        store.close()


def test_select_next_spans_phases(tmp_path: Path) -> None:
    _write_task(tmp_path / "active" / "01-first" / "a.json", "a")
    _write_task(tmp_path / "active" / "02-second" / "b.json", "b")
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "a")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "b"
    finally:
        store.close()


# ---------- Archive ----------


def test_archive_moves_only_fully_done_phases(tmp_path: Path) -> None:
    done_phase = tmp_path / "active" / "01-done"
    mixed_phase = tmp_path / "active" / "02-mixed"
    _write_task(done_phase / "a.json", "done-a")
    _write_task(done_phase / "b.json", "done-b")
    _write_task(mixed_phase / "c.json", "mixed-c")
    _write_task(mixed_phase / "d.json", "mixed-d")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "done-a")
        _seed_done(store, "done-b")
        _seed_done(store, "mixed-c")
        # 'mixed-d' is left fresh — phase should not archive.
        moved = archive_completed_phases(tmp_path, store)
    finally:
        store.close()

    assert [d.name for d in moved] == ["01-done"]
    assert not done_phase.exists()
    assert (tmp_path / "archive" / "01-done").is_dir()
    assert mixed_phase.is_dir()


def test_archive_is_idempotent(tmp_path: Path) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "a.json", "a")
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "a")
        first = archive_completed_phases(tmp_path, store)
        second = archive_completed_phases(tmp_path, store)
    finally:
        store.close()
    assert len(first) == 1
    assert second == []


# ---------- CLI integration ----------


def test_main_next_prints_path_and_exits_zero(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    phase = tmp_path / "active" / "01-phase"
    written = _write_task(phase / "a.json", "a")
    db = tmp_path / "db.sqlite"
    rc = main(
        [
            "next",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    out = capsys.readouterr().out.strip()
    assert Path(out) == written


def test_main_next_returns_one_when_no_tasks(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    rc = main(["next", "--tasks-dir", str(tmp_path), "--db", str(db)])
    assert rc == 1
    assert capsys.readouterr().out == ""


def test_main_is_done_reflects_store(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    task_file = _write_task(tmp_path / "active" / "01" / "a.json", "a")
    db = tmp_path / "db.sqlite"
    # Before any lifecycle, exit non-zero.
    rc = main(["is-done", str(task_file), "--db", str(db)])
    assert rc == 1
    # After a DONE lifecycle, exit zero.
    store = SqliteStore(db)
    try:
        _seed_done(store, "a")
    finally:
        store.close()
    rc = main(["is-done", str(task_file), "--db", str(db)])
    assert rc == 0


def test_main_status_json_emits_machine_readable(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    phase = tmp_path / "active" / "01"
    _write_task(phase / "a.json", "a")
    db = tmp_path / "db.sqlite"
    rc = main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
            "--json",
        ]
    )
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert len(payload) == 1
    assert payload[0]["task_id"] == "a"
    assert payload[0]["state"] == "fresh"


# ---------- Stranded-lifecycle recovery ----------


def _seed_validating(store: SqliteStore, task_id: str) -> Lifecycle:
    """Persist a lifecycle wedged in VALIDATING with an open attempt."""
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-validating")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    store.create_lifecycle(lc)
    store.save_attempt(
        lc.run_id,
        Attempt(number=1, started_at=now, run_id=lc.run_id),
    )
    return lc


def test_recover_finalizes_running_lifecycle(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-a")
        # A task already done in the store must be left untouched.
        done = _seed_done(store, "task-b")
        finalized = recover_stranded_lifecycles(store)
        assert finalized == [running.run_id]
        reloaded_running = store.load_lifecycle(running.run_id)
        assert reloaded_running is not None
        assert reloaded_running.status == Status.INTERRUPTED
        reloaded_done = store.load_lifecycle(done.run_id)
        assert reloaded_done is not None
        assert reloaded_done.status == Status.DONE
    finally:
        store.close()


def test_recover_finalizes_validating_and_closes_open_attempt(
    tmp_path: Path,
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        validating = _seed_validating(store, "task-v")
        finalized = recover_stranded_lifecycles(store)
        assert finalized == [validating.run_id]
        reloaded = store.load_lifecycle(validating.run_id)
        assert reloaded is not None
        assert reloaded.status == Status.INTERRUPTED
        attempts = store.list_attempts(validating.run_id)
        assert len(attempts) == 1
        assert attempts[0].ended_at is not None
        assert attempts[0].outcome == Outcome.INTERNAL_ERROR
        events = store.list_events(validating.run_id)
        kinds = [e.kind for e in events]
        assert "harness.crash" in kinds
        crash = next(e for e in events if e.kind == "harness.crash")
        assert crash.payload["classification"] == "worker_interrupted"
    finally:
        store.close()


def test_recover_filters_by_task_id(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        a = _seed_running(store, "task-a")
        b = _seed_running(store, "task-b")
        finalized = recover_stranded_lifecycles(store, task_id="task-a")
        assert finalized == [a.run_id]
        # task-b's stranded lifecycle stays stranded until its own
        # worker-start sweep runs.
        reloaded_b = store.load_lifecycle(b.run_id)
        assert reloaded_b is not None
        assert reloaded_b.status == Status.RUNNING
    finally:
        store.close()


def test_recover_does_not_consume_retry_budget(tmp_path: Path) -> None:
    """INTERRUPTED is not a retry-source state — retries must stay put."""
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        lc = _seed_validating(store, "task-r")
        assert lc.retries == 0
        recover_stranded_lifecycles(store)
        reloaded = store.load_lifecycle(lc.run_id)
        assert reloaded is not None
        assert reloaded.retries == 0
    finally:
        store.close()


def test_main_recover_prints_run_ids(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        seeded = _seed_running(store, "task-x")
    finally:
        store.close()
    rc = main(["recover", "--db", str(db)])
    assert rc == 0
    out = capsys.readouterr().out.strip().splitlines()
    assert out == [seeded.run_id]


def test_main_recover_empty_prints_placeholder(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    # Touch the store so it exists with no lifecycles.
    SqliteStore(db).close()
    rc = main(["recover", "--db", str(db)])
    assert rc == 0
    assert capsys.readouterr().out.strip() == "(no stranded lifecycles)"
