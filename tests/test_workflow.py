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
from flywheel.store_protocols import EventRecord
from flywheel.store_sqlite import SqliteStore
from flywheel.workflow import (
    TaskState,
    archive_completed_phases,
    build_status_rows,
    collect_live_rows,
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


def _seed_interrupted(store: SqliteStore, task_id: str) -> Lifecycle:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-interrupted")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.INTERRUPTED, now=now)
    store.create_lifecycle(lc)
    return lc


def _seed_blocked(
    store: SqliteStore,
    task_id: str,
    requires_payload: list[dict[str, object]],
    *,
    run_id: str | None = None,
) -> Lifecycle:
    """Persist an INTERRUPTED lifecycle whose ``blocked_requires_json``
    captures ``requires_payload`` — the recheck-eligible shape.

    Mirrors what the harness's ``Intent.BLOCKED`` branch writes: status
    INTERRUPTED + a non-null persisted requires snapshot. Used by the
    recheck-blocked CLI tests so the scan filter sees them and the
    primitive has predicates to evaluate.
    """
    rid = run_id or f"run-{task_id}-blocked"
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=rid)
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.INTERRUPTED, now=now)
    lc.blocked_requires_json = json.dumps(requires_payload)
    store.create_lifecycle(lc)
    return lc


def _write_blocked_task(path: Path, task_id: str, grader_name: str) -> Path:
    """Task file with a single command grader the recheck primitive can
    resolve when a ``command_grader`` predicate references ``grader_name``.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [
            {"type": "command", "run": "true", "name": grader_name}
        ],
    }
    path.write_text(json.dumps(payload))
    return path


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


def test_build_status_rows_classifies_fresh_done_failed_running_interrupted(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "fresh.json", "fresh")
    _write_task(phase / "done.json", "done")
    _write_task(phase / "failed.json", "failed")
    _write_task(phase / "running.json", "running")
    _write_task(phase / "interrupted.json", "interrupted")

    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "done")
        _seed_failed(store, "failed")
        _seed_running(store, "running")
        _seed_interrupted(store, "interrupted")

        rows = build_status_rows(tmp_path, store)
        state_by_id = {row.task.id: row.state for row in rows}
        assert state_by_id == {
            "fresh": TaskState.FRESH,
            "done": TaskState.DONE,
            "failed": TaskState.RETRYABLE,
            "running": TaskState.IN_PROGRESS,
            "interrupted": TaskState.INTERRUPTED,
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


def test_select_next_resumes_interrupted_task(tmp_path: Path) -> None:
    """Interrupted tasks are retry-eligible without operator unblock.

    The harness normalizes INTERRUPTED -> READY at entry (see
    docs/task-lifecycle.md), and the worker reconciles stranded
    lifecycles to INTERRUPTED on startup. The selector must agree:
    interrupted tasks block the phase otherwise."""
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "paused.json", "paused")

    store = SqliteStore(":memory:")
    try:
        _seed_interrupted(store, "paused")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "paused"
        assert pick.state == TaskState.INTERRUPTED
    finally:
        store.close()


def test_select_next_unblocks_downstream_after_interrupted(
    tmp_path: Path,
) -> None:
    """Interrupted root of a dependency chain must not freeze the phase.

    Regression guard for the symptom that triggered this change: the worker
    reconciles a SIGTERM'd task to INTERRUPTED, and every downstream task
    that lists it as a prerequisite would stall forever if INTERRUPTED were
    treated as ineligible. The selector picks the interrupted root first;
    downstream only unblocks once that root reaches DONE."""
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "root.json", "root")
    _write_task(phase / "leaf.json", "leaf", prerequisites=["root"])

    store = SqliteStore(":memory:")
    try:
        _seed_interrupted(store, "root")
        rows = build_status_rows(tmp_path, store)
        pick = select_next_task(rows)
        assert pick is not None and pick.task.id == "root"
        assert pick.state == TaskState.INTERRUPTED
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


# ---------- Live progress snapshot ----------


def test_live_skips_runs_that_are_not_in_flight(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_done(store, "done-task")
        _seed_failed(store, "failed-task")
        _seed_interrupted(store, "interrupted-task")
        assert collect_live_rows(store) == []
    finally:
        store.close()


def test_live_reports_latest_sdk_message_when_newer(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-a")
        ts0 = datetime.now(timezone.utc)
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=ts0,
                kind="harness.attempt_started",
                payload={},
                attempt_number=1,
            )
        )
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=1,
            iteration_number=2,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Edit",
                            "input": {"file_path": "README.md"},
                        }
                    ],
                }
            ],
        )
        rows = collect_live_rows(store)
        assert len(rows) == 1
        row = rows[0]
        assert row.task_id == "task-a"
        assert row.status == Status.RUNNING
        assert row.iteration == 2
        assert row.last_kind == "ASSISTANT"
        assert "Edit" in row.last_detail
        assert "README.md" in row.last_detail
    finally:
        store.close()


def test_live_falls_back_to_event_when_no_sdk_messages(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-b")
        store.append_event(
            EventRecord(
                run_id=running.run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.iteration_completed",
                payload={},
                attempt_number=1,
            )
        )
        rows = collect_live_rows(store)
        assert len(rows) == 1
        assert rows[0].last_kind == "EVENT"
        assert rows[0].last_detail == "harness.iteration_completed"
        assert rows[0].iteration is None
    finally:
        store.close()


def test_live_marks_runs_with_no_activity(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running(store, "task-c")
        rows = collect_live_rows(store)
        assert len(rows) == 1
        assert rows[0].last_kind == "(none)"
        assert rows[0].last_ts is None
    finally:
        store.close()


def test_live_summarizes_user_tool_result(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running = _seed_running(store, "task-d")
        body = "x" * 1234
        store.save_sdk_messages(
            run_id=running.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "UserMessage",
                    "content": [
                        {"tool_use_id": "toolu_x", "content": body}
                    ],
                }
            ],
        )
        rows = collect_live_rows(store)
        assert rows[0].last_kind == "USER"
        assert rows[0].last_detail == f"tool_result({len(body)}B)"
    finally:
        store.close()


def test_main_live_prints_one_line_per_running_task(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        running_a = _seed_running(store, "task-a")
        running_b = _seed_running(store, "task-b")
        store.save_sdk_messages(
            run_id=running_a.run_id,
            attempt_number=1,
            iteration_number=1,
            messages=[
                {
                    "message_type": "AssistantMessage",
                    "content": [
                        {
                            "type": "tool_use",
                            "name": "Read",
                            "input": {"file_path": "x.py"},
                        }
                    ],
                }
            ],
        )
        store.append_event(
            EventRecord(
                run_id=running_b.run_id,
                ts=datetime.now(timezone.utc),
                kind="harness.attempt_started",
                payload={},
                attempt_number=1,
            )
        )
    finally:
        store.close()
    rc = main(["live", "--db", str(db)])
    assert rc == 0
    out = capsys.readouterr().out
    lines = [ln for ln in out.splitlines() if ln.strip()]
    assert len(lines) == 2
    assert any("task-a" in ln and "ASSISTANT" in ln and "Read" in ln for ln in lines)
    assert any("task-b" in ln and "EVENT" in ln for ln in lines)


def test_main_live_empty_prints_placeholder(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    db = tmp_path / "db.sqlite"
    rc = main(["live", "--db", str(db)])
    assert rc == 0
    assert capsys.readouterr().out.strip() == "(no in-flight runs)"


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


# ---------- recheck-blocked CLI ----------


def test_main_recheck_blocked_empty_store_prints_placeholder(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """No blocked lifecycles -> the scan exits cleanly with a clear empty
    state line, never raises. Covers both 'no lifecycles at all' and
    'lifecycles exist but none with blocked_requires_json'."""
    db = tmp_path / "db.sqlite"
    SqliteStore(db).close()
    rc = main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    assert rc == 0
    assert capsys.readouterr().out.strip() == "(no blocked lifecycles)"


def test_main_recheck_blocked_all_satisfied_transitions_and_prints_unblocked(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """All predicates satisfied -> recheck applies the transition and the
    CLI line announces the unblock. The lifecycle is now READY and
    blocked_requires_json was cleared by the harness."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-a.json", "task-a", "full-suite")
    db = tmp_path / "db.sqlite"

    store = SqliteStore(db)
    try:
        seeded = _seed_blocked(
            store,
            "task-a",
            [
                {"type": "file_exists", "path": "ready.flag", "present": True}
            ],
        )
    finally:
        store.close()

    # file_exists predicate evaluates against the worker CWD. Make the
    # path real *in* that CWD so the predicate satisfies.
    (tmp_path / "ready.flag").write_text("ok")
    monkeypatch.chdir(tmp_path)

    rc = main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    out_lines = [
        ln for ln in capsys.readouterr().out.splitlines() if ln.strip()
    ]
    assert rc == 0
    assert out_lines == [f"{seeded.run_id}: unblocked"]

    store = SqliteStore(db)
    try:
        reloaded = store.load_lifecycle(seeded.run_id)
    finally:
        store.close()
    assert reloaded is not None
    assert reloaded.status == Status.READY
    assert reloaded.blocked_requires_json is None


def test_main_recheck_blocked_partially_satisfied_reports_still_blocked(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """One satisfied + one unsatisfied predicate -> no transition; CLI
    line names only the misses, not every predicate."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-p.json", "task-p", "full-suite")
    db = tmp_path / "db.sqlite"

    monkeypatch.delenv("RECHECK_CLI_VAR", raising=False)
    monkeypatch.chdir(tmp_path)

    store = SqliteStore(db)
    try:
        seeded = _seed_blocked(
            store,
            "task-p",
            [
                {
                    "type": "file_exists",
                    "path": "ignored",
                    "present": False,
                },
                {"type": "env_var_set", "name": "RECHECK_CLI_VAR"},
            ],
        )
    finally:
        store.close()

    rc = main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    out = capsys.readouterr().out.strip()
    assert rc == 0
    assert out.startswith(f"{seeded.run_id}: still blocked (")
    assert "env_var_set=RECHECK_CLI_VAR" in out
    # The satisfied predicate must not appear in the "still blocked" list.
    assert "file_exists=ignored" not in out

    store = SqliteStore(db)
    try:
        reloaded = store.load_lifecycle(seeded.run_id)
    finally:
        store.close()
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED
    assert reloaded.blocked_requires_json is not None


def test_main_recheck_blocked_run_id_targets_one_lifecycle(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """--run-id processes the named lifecycle only; siblings stay
    interrupted with their persisted requires intact."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-a.json", "task-a", "full-suite")
    _write_blocked_task(phase / "task-b.json", "task-b", "full-suite")
    db = tmp_path / "db.sqlite"

    (tmp_path / "ready.flag").write_text("ok")
    monkeypatch.chdir(tmp_path)

    store = SqliteStore(db)
    try:
        targeted = _seed_blocked(
            store,
            "task-a",
            [
                {"type": "file_exists", "path": "ready.flag", "present": True}
            ],
            run_id="run-target",
        )
        other = _seed_blocked(
            store,
            "task-b",
            [
                {"type": "file_exists", "path": "ready.flag", "present": True}
            ],
            run_id="run-other",
        )
    finally:
        store.close()

    rc = main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
            "--run-id",
            "run-target",
        ]
    )
    out_lines = [
        ln for ln in capsys.readouterr().out.splitlines() if ln.strip()
    ]
    assert rc == 0
    assert out_lines == [f"{targeted.run_id}: unblocked"]

    store = SqliteStore(db)
    try:
        reloaded_target = store.load_lifecycle(targeted.run_id)
        reloaded_other = store.load_lifecycle(other.run_id)
    finally:
        store.close()
    assert reloaded_target is not None
    assert reloaded_target.status == Status.READY
    assert reloaded_other is not None
    assert reloaded_other.status == Status.INTERRUPTED
    assert reloaded_other.blocked_requires_json is not None


def test_main_recheck_blocked_dry_run_reports_without_transitioning(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """--dry-run on an all-satisfied lifecycle prints 'would unblock',
    leaves status INTERRUPTED, leaves blocked_requires_json intact, and
    emits harness.recheck_attempted but never harness.unblocked."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-d.json", "task-d", "full-suite")
    db = tmp_path / "db.sqlite"

    (tmp_path / "ready.flag").write_text("ok")
    monkeypatch.chdir(tmp_path)

    store = SqliteStore(db)
    try:
        seeded = _seed_blocked(
            store,
            "task-d",
            [
                {"type": "file_exists", "path": "ready.flag", "present": True}
            ],
        )
    finally:
        store.close()

    rc = main(
        [
            "recheck-blocked",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
            "--dry-run",
        ]
    )
    out_lines = [
        ln for ln in capsys.readouterr().out.splitlines() if ln.strip()
    ]
    assert rc == 0
    assert out_lines == [f"{seeded.run_id}: would unblock"]

    store = SqliteStore(db)
    try:
        reloaded = store.load_lifecycle(seeded.run_id)
        events = [e.kind for e in store.list_events(seeded.run_id)]
    finally:
        store.close()
    assert reloaded is not None
    assert reloaded.status == Status.INTERRUPTED
    assert reloaded.blocked_requires_json is not None
    assert "harness.recheck_attempted" in events
    assert "harness.unblocked" not in events


# ---------- status: blocked_on surface ----------


def test_main_status_text_includes_blocked_on_for_blocked_interrupted_row(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Interrupted row with blocked_requires_json -> text output carries
    a `blocked_on:` summary listing predicate type=identifier pairs."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "task-s.json", "task-s", "full-suite")
    db = tmp_path / "db.sqlite"

    store = SqliteStore(db)
    try:
        _seed_blocked(
            store,
            "task-s",
            [
                {"type": "command_grader", "name": "full-suite"},
                {
                    "type": "file_exists",
                    "path": ".workflow/lkg/.venv",
                    "present": True,
                },
            ],
        )
    finally:
        store.close()

    rc = main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    assert "blocked_on:" in out
    assert "command_grader=full-suite" in out
    assert "file_exists=.workflow/lkg/.venv" in out


def test_main_status_text_omits_blocked_on_for_sigint_interrupted_row(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """SIGINT-paused lifecycle (INTERRUPTED, blocked_requires_json IS
    NULL) renders cleanly without a blocked_on line."""
    phase = tmp_path / "active" / "01-phase"
    _write_task(phase / "task-i.json", "task-i")
    db = tmp_path / "db.sqlite"

    store = SqliteStore(db)
    try:
        _seed_interrupted(store, "task-i")
    finally:
        store.close()

    rc = main(
        [
            "status",
            "--tasks-dir",
            str(tmp_path),
            "--db",
            str(db),
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    assert "interrupted" in out
    assert "blocked_on:" not in out


def test_main_status_json_includes_parsed_blocked_requires_when_present(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """JSON mode emits the parsed list (list of dicts) on blocked rows
    and OMITS the key entirely on rows without a snapshot — null is not a
    valid sentinel, the key must be absent."""
    phase = tmp_path / "active" / "01-phase"
    _write_blocked_task(phase / "blocked.json", "blocked", "full-suite")
    _write_task(phase / "fresh.json", "fresh")
    db = tmp_path / "db.sqlite"

    persisted_requires: list[dict[str, object]] = [
        {"type": "command_grader", "name": "full-suite"},
        {"type": "env_var_set", "name": "READY"},
    ]
    store = SqliteStore(db)
    try:
        _seed_blocked(store, "blocked", persisted_requires)
    finally:
        store.close()

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
    by_id = {row["task_id"]: row for row in payload}

    assert "blocked_requires" in by_id["blocked"]
    assert by_id["blocked"]["blocked_requires"] == persisted_requires
    assert "blocked_requires" not in by_id["fresh"]


# ---------- run_task_file entry-time crash recording ----------


def test_run_task_file_records_crash_for_invoke_runtime_error(
    tmp_path: Path,
) -> None:
    """A stub invoke that raises mid-call must leave a non-empty
    lifecycles row and at least one harness.crash event in the
    SqliteStore. workflow.py's run_task_file re-raises the original
    exception so the worker subshell sees a non-zero exit.

    Backs the audit at
    ``.workflow/audits/08-recoverable-blocked-lifecycles.md`` finding
    "Crashes before create_lifecycle are invisible to every loop
    subsystem except the worker log": the new harness ordering must
    write the lifecycle row first so this exact failure shape is now
    visible in the DB.
    """
    import asyncio

    from flywheel.harness import InvocationRequest
    from flywheel.invoker import IterationResult
    from flywheel.workflow import run_task_file

    task_file = tmp_path / "task.json"
    _write_task(task_file, "probe")
    db = tmp_path / "db.sqlite"
    sandbox = tmp_path / "sandbox"

    async def _raising_invoke(_request: InvocationRequest) -> IterationResult:
        raise RuntimeError("workflow stub blew up")

    with pytest.raises(RuntimeError, match="workflow stub blew up"):
        asyncio.run(
            run_task_file(
                task_file,
                db_path=db,
                sandbox=sandbox,
                invoke=_raising_invoke,
            )
        )

    # Re-open the store to verify the DB recorded the crash before the
    # exception propagated to the caller.
    store = SqliteStore(db)
    try:
        conn = store._connection  # noqa: SLF001 — inspecting raw rows
        lifecycle_rows = conn.execute(
            "SELECT run_id, status FROM lifecycles WHERE task_id = ?",
            ("probe",),
        ).fetchall()
        assert len(lifecycle_rows) == 1
        row = lifecycle_rows[0]
        run_id = row["run_id"]
        # Terminal status: the entry-crash recorder walks the lifecycle
        # to FAILED so subsequent observers see the run is over.
        assert row["status"] == Status.FAILED.value
        # The harness.crash event is the audit-visible record of the
        # failure mode.
        crash_count = conn.execute(
            "SELECT COUNT(*) AS n FROM events "
            "WHERE run_id = ? AND kind = 'harness.crash'",
            (run_id,),
        ).fetchone()["n"]
        assert crash_count >= 1
    finally:
        store.close()
