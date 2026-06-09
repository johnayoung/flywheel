"""Tests for the steering bridge (source reconciler).

The rule under test: an in-flight run whose item is no longer listed by
its work source gets exactly one ``interrupt`` control command, enqueued
through the spec-00013 store-routed path. The apply side (the invoker's
watcher driving the lifecycle to INTERRUPTED) is the already-tested 00013
machinery; these tests own the bridge's enqueue side and its failure
posture (a listing failure never interrupts anything).

The end-to-end tests drive the real ``orchestrate`` loop against a real
SQLite store with a scripted ``invoke`` that mutates the source mid-run
and waits for the bridge to react.
"""

from __future__ import annotations

import asyncio
import io
import sqlite3
import time
from datetime import datetime, timezone
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    Status,
    ValidEnvelope,
)
from flywheel.store_sqlite import SqliteStore
from flywheel.task import CommandGrader, Task
from flywheel_orchestrator import (
    WorkItem,
    WorkReport,
    orchestrate,
    reconcile_live_runs,
)

# --- fixtures / helpers -----------------------------------------------------


def _item(task_id: str) -> WorkItem:
    return WorkItem(
        task=Task(
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run="true")],
            id=task_id,
        ),
        source_ref=f"mem://{task_id}",
    )


class _MutableSource:
    """In-memory WorkSource whose item list tests mutate mid-run."""

    def __init__(self, items: list[WorkItem]) -> None:
        self.items = items
        self.failing = False
        self.reports: list[WorkReport] = []

    def list_work(self) -> list[WorkItem]:
        if self.failing:
            raise RuntimeError("tracker unreachable")
        return list(self.items)

    def report(self, report: WorkReport) -> None:
        self.reports.append(report)


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _verify_result() -> IterationResult:
    return IterationResult(
        transcript="ok",
        messages=(
            AssistantMessage(
                content=[TextBlock(text="done")],
                model="claude-test",
                stop_reason="end_turn",
                session_id="sess",
                usage=None,
            ),
            ResultMessage(
                subtype="success",
                duration_ms=1,
                duration_api_ms=1,
                is_error=False,
                num_turns=1,
                session_id="sess",
                stop_reason="end_turn",
                total_cost_usd=0.0,
                usage=None,
            ),
        ),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=_signals(),
        failure=None,
    )


def _interrupt_rows(db_path: Path) -> list[tuple[str, str]]:
    """``(run_id, kind)`` of every interrupt command row in the store."""
    conn = sqlite3.connect(db_path)
    try:
        rows = conn.execute(
            "SELECT run_id, kind FROM control_commands "
            "WHERE kind = 'interrupt' ORDER BY id"
        ).fetchall()
        return [(r[0], r[1]) for r in rows]
    finally:
        conn.close()


def _seed_running_lifecycle(
    store: SqliteStore, task_id: str, run_id: str
) -> None:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=run_id)
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    store.create_lifecycle(lc)


# --- reconcile_live_runs (the bridge's core) --------------------------------


def test_vanished_item_gets_one_interrupt(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running_lifecycle(store, "wanted", "run-wanted")
        _seed_running_lifecycle(store, "dropped", "run-dropped")
        already: set[str] = set()

        signaled = reconcile_live_runs(
            store,
            frozenset({"wanted"}),
            already_signaled=already,
            now=datetime.now(timezone.utc),
        )

        assert signaled == ("run-dropped",)
        assert _interrupt_rows(db) == [("run-dropped", "interrupt")]

        # Second tick: the run is still RUNNING (watcher has not acted yet)
        # but the dedup set prevents a duplicate command.
        signaled = reconcile_live_runs(
            store,
            frozenset({"wanted"}),
            already_signaled=already,
            now=datetime.now(timezone.utc),
        )
        assert signaled == ()
        assert len(_interrupt_rows(db)) == 1
    finally:
        store.close()


def test_listed_items_are_never_interrupted(tmp_path: Path) -> None:
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        _seed_running_lifecycle(store, "wanted", "run-wanted")
        signaled = reconcile_live_runs(
            store,
            frozenset({"wanted"}),
            already_signaled=set(),
            now=datetime.now(timezone.utc),
        )
        assert signaled == ()
        assert _interrupt_rows(db) == []
    finally:
        store.close()


def test_awaiting_approval_is_out_of_scope(tmp_path: Path) -> None:
    # A parked manual gate has no live session to interrupt; its
    # disposition belongs to approve/reject, not the bridge.
    db = tmp_path / "db.sqlite"
    store = SqliteStore(db)
    try:
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="gated", run_id="run-gated")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        lc.transition_to(Status.VALIDATING, now=now)
        lc.transition_to(Status.AWAITING_APPROVAL, now=now)
        store.create_lifecycle(lc)

        signaled = reconcile_live_runs(
            store,
            frozenset(),
            already_signaled=set(),
            now=datetime.now(timezone.utc),
        )
        assert signaled == ()
        assert _interrupt_rows(db) == []
    finally:
        store.close()


# --- end-to-end through orchestrate -----------------------------------------


def _run_orchestrate(source, tmp_path: Path, invoke):
    return asyncio.run(
        orchestrate(
            source=source,
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=invoke,
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            reconcile_seconds=0.05,
        )
    )


def test_mid_run_vanish_enqueues_interrupt_end_to_end(tmp_path: Path) -> None:
    """The item vanishes while its run is in flight; the bridge reacts.

    The scripted invoke drops the item from the source, then waits until
    the reconciler's interrupt row lands (or times out), then keeps the
    run alive for several more ticks to prove the dedup holds before
    finishing. Only the agent's text is scripted: the loop, store, and
    command enqueue path are all real.
    """
    db = tmp_path / "flywheel.sqlite"
    source = _MutableSource([_item("vanishing")])

    async def _invoke(request: InvocationRequest) -> IterationResult:
        source.items.clear()  # operator "deleted the ticket" mid-run
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline and not _interrupt_rows(db):
            await asyncio.sleep(0.02)
        assert _interrupt_rows(db), "bridge never enqueued the interrupt"
        # Stay in flight across several more reconcile ticks: the dedup
        # set must keep this at exactly one command.
        await asyncio.sleep(0.25)
        return _verify_result()

    report = _run_orchestrate(source, tmp_path, _invoke)

    rows = _interrupt_rows(db)
    assert len(rows) == 1
    (run_id, kind) = rows[0]
    assert kind == "interrupt"
    assert run_id == report.runs[0].run_id


def test_listing_failure_never_interrupts(tmp_path: Path) -> None:
    """A tracker hiccup mid-run must not be read as 'all work vanished'."""
    db = tmp_path / "flywheel.sqlite"
    source = _MutableSource([_item("steady")])

    async def _invoke(request: InvocationRequest) -> IterationResult:
        source.failing = True  # every reconcile tick now raises
        await asyncio.sleep(0.25)  # several ticks' worth of failures
        source.failing = False  # restore before the main loop re-lists
        return _verify_result()

    report = _run_orchestrate(source, tmp_path, _invoke)

    assert _interrupt_rows(db) == []
    assert report.runs[0].status is Status.DONE


def test_bridge_disabled_by_default(tmp_path: Path) -> None:
    """Library callers see no reconciler unless they opt in."""
    db = tmp_path / "flywheel.sqlite"
    source = _MutableSource([_item("vanishing")])

    async def _invoke(request: InvocationRequest) -> IterationResult:
        source.items.clear()
        await asyncio.sleep(0.2)  # would be ~4 ticks if a bridge ran
        return _verify_result()

    report = asyncio.run(
        orchestrate(
            source=source,
            db_path=db,
            sandbox_root=tmp_path / "sandboxes",
            invoke=_invoke,
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
        )
    )

    assert _interrupt_rows(db) == []
    assert report.runs[0].status is Status.DONE
