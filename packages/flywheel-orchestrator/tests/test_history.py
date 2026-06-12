"""Tests for the completed-run history read path (``_history``) and the
``history`` / ``show`` CLI verbs."""

from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from flywheel_core.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.store_protocols import GraderResultRecord
from flywheel_orchestrator._history import (
    build_task_phase_index,
    collect_history_rows,
    collect_run_detail,
    phase_from_source,
    resolve_run_id,
)
from flywheel_orchestrator._workflow import main as orch_main

_T0 = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)


def _seed_terminal(
    store: SqliteStore,
    task_id: str,
    *,
    run_id: str,
    terminal: Status = Status.DONE,
    source: str = "",
    at: datetime = _T0,
    error: str = "boom",
) -> Lifecycle:
    lc = Lifecycle(task_id=task_id, run_id=run_id, source=source)
    lc.transition_to(Status.READY, now=at)
    lc.transition_to(Status.RUNNING, now=at + timedelta(seconds=1))
    if terminal == Status.FAILED:
        # FAILED is reachable from RUNNING, not VALIDATING.
        lc.transition_to(
            Status.FAILED, error=error, now=at + timedelta(minutes=5)
        )
    else:
        lc.transition_to(Status.VALIDATING, now=at + timedelta(seconds=2))
        if terminal == Status.DONE:
            lc.transition_to(Status.DONE, now=at + timedelta(minutes=5))
        else:
            lc.transition_to(
                terminal, error=error, now=at + timedelta(minutes=5)
            )
    store.create_lifecycle(lc)
    return lc


def _seed_attempt(
    store: SqliteStore,
    run_id: str,
    *,
    number: int = 1,
    tokens: int = 1000,
    cost: float = 0.5,
    turns: int = 7,
) -> None:
    store.save_attempt(
        run_id,
        Attempt(
            number=number,
            started_at=_T0,
            run_id=run_id,
            ended_at=_T0 + timedelta(minutes=4),
            outcome=Outcome.SUCCEEDED,
            input_tokens=tokens,
            iterations_completed=2,
            turns=turns,
            total_cost_usd=cost,
        ),
    )


# --- phase_from_source -------------------------------------------------------


def test_phase_from_source_derives_directory_phase() -> None:
    assert (
        phase_from_source(".flywheel/tasks/active/30-history/t1.json")
        == "30-history"
    )
    assert (
        phase_from_source(".flywheel/tasks/archive/05-audit/t2.json")
        == "05-audit"
    )


def test_phase_from_source_rejects_non_phase_shapes() -> None:
    assert phase_from_source(None) is None
    assert phase_from_source("") is None
    assert phase_from_source("owner/repo#123") is None
    assert phase_from_source("t1.json") is None
    assert phase_from_source(".flywheel/tasks/active/t1.json") is None


# --- collect_history_rows ----------------------------------------------------


def test_history_lists_only_terminal_runs() -> None:
    store = SqliteStore(":memory:")
    _seed_terminal(store, "t-done", run_id="run-1")
    _seed_terminal(
        store, "t-failed", run_id="run-2", terminal=Status.FAILED
    )
    live = Lifecycle(task_id="t-live", run_id="run-3")
    live.transition_to(Status.READY, now=_T0)
    live.transition_to(Status.RUNNING, now=_T0)
    store.create_lifecycle(live)

    rows = collect_history_rows(store)
    assert {r.task_id for r in rows} == {"t-done", "t-failed"}


def test_history_groups_retried_task_into_one_row() -> None:
    store = SqliteStore(":memory:")
    _seed_terminal(
        store,
        "t1",
        run_id="run-old",
        terminal=Status.FAILED,
        at=_T0,
    )
    _seed_terminal(
        store,
        "t1",
        run_id="run-new",
        at=_T0 + timedelta(hours=1),
    )

    rows = collect_history_rows(store)
    assert len(rows) == 1
    row = rows[0]
    assert row.latest.run_id == "run-new"
    assert row.latest.status == Status.DONE
    assert [r.run_id for r in row.prior_runs] == ["run-old"]


def test_history_orders_most_recently_finished_first() -> None:
    store = SqliteStore(":memory:")
    _seed_terminal(store, "t-early", run_id="run-a", at=_T0)
    _seed_terminal(
        store, "t-late", run_id="run-b", at=_T0 + timedelta(hours=2)
    )

    rows = collect_history_rows(store)
    assert [r.task_id for r in rows] == ["t-late", "t-early"]


def test_history_derives_phase_and_honors_fallback() -> None:
    store = SqliteStore(":memory:")
    _seed_terminal(
        store,
        "t-sourced",
        run_id="run-s",
        source=".flywheel/tasks/active/30-history/t-sourced.json",
    )
    _seed_terminal(store, "t-legacy", run_id="run-l")

    rows = collect_history_rows(
        store, fallback_phases={"t-legacy": "07-readme-onboarding"}
    )
    by_task = {r.task_id: r for r in rows}
    assert by_task["t-sourced"].phase == "30-history"
    assert by_task["t-legacy"].phase == "07-readme-onboarding"


def test_history_filters_by_status_phase_and_limit() -> None:
    store = SqliteStore(":memory:")
    _seed_terminal(
        store,
        "t1",
        run_id="run-1",
        source=".flywheel/tasks/active/30-a/t1.json",
        at=_T0 + timedelta(hours=2),
    )
    _seed_terminal(
        store,
        "t2",
        run_id="run-2",
        terminal=Status.FAILED,
        source=".flywheel/tasks/active/31-b/t2.json",
        at=_T0 + timedelta(hours=1),
    )

    failed_only = collect_history_rows(store, statuses=(Status.FAILED,))
    assert [r.task_id for r in failed_only] == ["t2"]

    phase_only = collect_history_rows(store, phase="30-a")
    assert [r.task_id for r in phase_only] == ["t1"]

    capped = collect_history_rows(store, limit=1)
    assert [r.task_id for r in capped] == ["t1"]


def test_history_rolls_up_attempt_totals() -> None:
    store = SqliteStore(":memory:")
    _seed_terminal(store, "t1", run_id="run-1")
    _seed_attempt(store, "run-1", number=1, tokens=1000, cost=0.5, turns=7)
    _seed_attempt(store, "run-1", number=2, tokens=400, cost=0.25, turns=3)

    (row,) = collect_history_rows(store)
    assert row.latest.attempts == 2
    assert row.latest.tokens_total == 1400
    assert row.latest.cost_usd_total == 0.75
    assert row.latest.turns_total == 10
    assert row.latest.finished_at is not None
    assert row.latest.started_at is not None
    assert row.latest.started_at < row.latest.finished_at


# --- build_task_phase_index --------------------------------------------------


def test_phase_index_scans_active_and_archive(tmp_path: Path) -> None:
    tasks_dir = tmp_path / "tasks"
    archived = tasks_dir / "archive" / "05-old" / "t-archived.json"
    archived.parent.mkdir(parents=True)
    archived.write_text(json.dumps({"id": "t-archived", "goal": "g"}))
    active = tasks_dir / "active" / "30-new" / "t-active.json"
    active.parent.mkdir(parents=True)
    active.write_text(json.dumps({"id": "t-active", "goal": "g"}))
    # A re-issued task id present in both buckets: active wins.
    reissued = tasks_dir / "active" / "31-redo" / "t-archived.json"
    reissued.parent.mkdir(parents=True)
    reissued.write_text(json.dumps({"id": "t-archived", "goal": "g"}))
    # Garbage is skipped, not fatal.
    bad = tasks_dir / "archive" / "05-old" / "broken.json"
    bad.write_text("{not json")

    index = build_task_phase_index(tasks_dir)
    assert index == {"t-archived": "31-redo", "t-active": "30-new"}


# --- resolve_run_id / collect_run_detail --------------------------------------


def test_resolve_run_id_accepts_run_or_task_id() -> None:
    store = SqliteStore(":memory:")
    _seed_terminal(store, "t1", run_id="run-old", at=_T0)
    _seed_terminal(
        store, "t1", run_id="run-new", at=_T0 + timedelta(hours=1)
    )

    assert resolve_run_id(store, "run-old") == "run-old"
    assert resolve_run_id(store, "t1") == "run-new"
    assert resolve_run_id(store, "nope") is None


def test_run_detail_carries_attempts_graders_and_related_runs() -> None:
    store = SqliteStore(":memory:")
    _seed_terminal(
        store, "t1", run_id="run-old", terminal=Status.FAILED, at=_T0
    )
    lc = _seed_terminal(
        store,
        "t1",
        run_id="run-new",
        source=".flywheel/tasks/active/30-a/t1.json",
        at=_T0 + timedelta(hours=1),
    )
    lc.agent_output = "All done."
    store.update_lifecycle(lc, expected_version=lc.version)
    _seed_attempt(store, "run-new", number=1)
    store.append_grader_result(
        GraderResultRecord(
            run_id="run-new",
            attempt_number=1,
            ordinal=0,
            grader_type="command",
            grader_spec={"type": "command", "run": "true"},
            passed=True,
            duration_ms=12,
            payload={"exit_code": 0},
            ts=_T0,
            grader_name="smoke",
        )
    )

    detail = collect_run_detail(store, "run-new")
    assert detail is not None
    assert detail.run.status == Status.DONE
    assert detail.phase == "30-a"
    assert detail.agent_output == "All done."
    assert [a.number for a in detail.attempts] == [1]
    assert detail.attempts[0].outcome == "succeeded"
    assert [g.grader_name for g in detail.grader_results] == ["smoke"]
    assert [r.run_id for r in detail.related_runs] == ["run-old"]
    assert collect_run_detail(store, "run-missing") is None


# --- CLI verbs -----------------------------------------------------------------


def _seeded_db(tmp_path: Path) -> Path:
    db = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db)
    _seed_terminal(
        store,
        "t1",
        run_id="run-1",
        source=str(tmp_path / "tasks/active/30-a/t1.json"),
    )
    _seed_attempt(store, "run-1")
    store.close()
    return db


def test_cmd_history_text_output(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = _seeded_db(tmp_path)
    rc = orch_main(
        ["history", "--db", str(db), "--tasks-dir", str(tmp_path / "tasks")]
    )
    out = capsys.readouterr().out
    assert rc == 0
    assert "30-a/t1" in out
    assert "done" in out
    assert "runs=1" in out


def test_cmd_history_json_output(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = _seeded_db(tmp_path)
    rc = orch_main(
        [
            "history",
            "--db",
            str(db),
            "--tasks-dir",
            str(tmp_path / "tasks"),
            "--json",
        ]
    )
    out = capsys.readouterr().out
    assert rc == 0
    payload = json.loads(out)
    assert payload[0]["task_id"] == "t1"
    assert payload[0]["phase"] == "30-a"
    assert payload[0]["latest"]["status"] == "done"
    assert payload[0]["latest"]["tokens_total"] == 1000


def test_cmd_history_empty_store(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = tmp_path / "flywheel.sqlite"
    SqliteStore(db).close()
    rc = orch_main(
        ["history", "--db", str(db), "--tasks-dir", str(tmp_path / "tasks")]
    )
    assert rc == 0
    assert "(no finished runs)" in capsys.readouterr().out


def test_cmd_show_by_task_id(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = _seeded_db(tmp_path)
    rc = orch_main(
        ["show", "t1", "--db", str(db), "--tasks-dir", str(tmp_path / "tasks")]
    )
    out = capsys.readouterr().out
    assert rc == 0
    assert "task     : t1" in out
    assert "run      : run-1" in out
    assert "status   : done" in out
    assert "phase    : 30-a" in out


def test_cmd_show_unknown_id_exits_nonzero(
    tmp_path: Path, capsys: "pytest.CaptureFixture[str]"
) -> None:
    db = _seeded_db(tmp_path)
    rc = orch_main(
        [
            "show",
            "nope",
            "--db",
            str(db),
            "--tasks-dir",
            str(tmp_path / "tasks"),
        ]
    )
    assert rc == 1
    assert "no run or task with that id" in capsys.readouterr().out
