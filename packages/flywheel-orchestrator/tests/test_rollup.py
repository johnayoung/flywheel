"""Tests for the evidence-derived status rollup (``status --rollup``).

The rollup is a pure projection: every node's status is computed from
lifecycle state and grader receipts, never asserted. These tests pin the
load-bearing distinction the surface exists to draw -- ``verified`` (done
with green graders) is not ``accepted`` (done with no graders) -- plus
phase grouping, prerequisite blocking, and the JSON shape.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

from flywheel_core.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel_core.store_protocols import GraderResultRecord
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator._rollup import (
    RollupStatus,
    build_rollup,
    render_rollup_text,
    rollup_to_json,
)
from flywheel_orchestrator._sources import WorkItem
from flywheel_orchestrator._workflow import status_rows_for_items

_NOW = datetime(2026, 6, 19, tzinfo=timezone.utc)


def _task(task_id: str) -> Task:
    return Task(id=task_id, goal=f"Goal {task_id}", graders=[CommandGrader(run="true")])


def _item(
    task_id: str,
    *,
    phase: str = "phase-x",
    prerequisites: tuple[str, ...] = (),
) -> WorkItem:
    """A file-backed work item so the rollup derives ``phase`` from the path."""
    return WorkItem(
        task=_task(task_id),
        prerequisites=prerequisites,
        source_ref=f"{phase}/{task_id}.json",
        local_path=Path(".flywheel/tasks/active") / phase / f"{task_id}.json",
    )


def _seed_done(store: SqliteStore, task_id: str) -> str:
    run_id = f"run-{task_id}-ok"
    lc = Lifecycle(task_id=task_id, run_id=run_id)
    lc.transition_to(Status.READY, now=_NOW)
    lc.transition_to(Status.RUNNING, now=_NOW)
    lc.transition_to(Status.VALIDATING, now=_NOW)
    lc.transition_to(Status.DONE, now=_NOW)
    store.create_lifecycle(lc)
    return run_id


def _seed_verified(store: SqliteStore, task_id: str, *, passed: bool = True) -> None:
    """A DONE run with a recorded grader receipt -- the verifiable shape."""
    run_id = _seed_done(store, task_id)
    store.save_attempt(
        run_id,
        Attempt(
            number=1,
            started_at=_NOW,
            run_id=run_id,
            ended_at=_NOW,
            outcome=Outcome.SUCCEEDED,
        ),
    )
    store.append_grader_result(
        GraderResultRecord(
            run_id=run_id,
            attempt_number=1,
            ordinal=0,
            grader_type="command",
            grader_spec={"type": "command", "run": "true"},
            passed=passed,
            duration_ms=1,
            payload={"run": "true", "exit_code": 0 if passed else 1},
            ts=_NOW,
        )
    )


def _seed_running(store: SqliteStore, task_id: str) -> None:
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-running")
    lc.transition_to(Status.READY, now=_NOW)
    lc.transition_to(Status.RUNNING, now=_NOW)
    store.create_lifecycle(lc)


def _seed_failed(store: SqliteStore, task_id: str) -> None:
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-fail")
    lc.transition_to(Status.READY, now=_NOW)
    lc.transition_to(Status.RUNNING, now=_NOW)
    lc.transition_to(Status.FAILED, error="boom", now=_NOW)
    store.create_lifecycle(lc)


def _rollup_for(store: SqliteStore, items: list[WorkItem]):
    return build_rollup(status_rows_for_items(items, store), store)


def test_done_with_graders_is_verified_not_accepted() -> None:
    store = SqliteStore(":memory:")
    try:
        _seed_verified(store, "graded")
        rollup = _rollup_for(store, [_item("graded")])
    finally:
        store.close()
    task = rollup.phases[0].tasks[0]
    assert task.status is RollupStatus.VERIFIED
    assert (task.passed_graders, task.total_graders) == (1, 1)


def test_done_without_graders_is_accepted_not_verified() -> None:
    """A DONE run with no grader receipts is the agent's own claim. The
    rollup must not let it masquerade as verified."""
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "ungraded")
        rollup = _rollup_for(store, [_item("ungraded")])
    finally:
        store.close()
    task = rollup.phases[0].tasks[0]
    assert task.status is RollupStatus.ACCEPTED
    assert (task.passed_graders, task.total_graders) == (0, 0)


def test_running_failed_and_fresh_classification() -> None:
    store = SqliteStore(":memory:")
    try:
        _seed_running(store, "active")
        _seed_failed(store, "broken")
        # "fresh" has no lifecycle and no prerequisites -> not_started.
        rollup = _rollup_for(
            store,
            [_item("active"), _item("broken"), _item("fresh")],
        )
    finally:
        store.close()
    by_id = {t.task_id: t.status for p in rollup.phases for t in p.tasks}
    assert by_id["active"] is RollupStatus.IN_PROGRESS
    assert by_id["broken"] is RollupStatus.FAILED
    assert by_id["fresh"] is RollupStatus.NOT_STARTED


def test_unsatisfied_prerequisite_blocks_a_fresh_task() -> None:
    store = SqliteStore(":memory:")
    try:
        _seed_failed(store, "dep")  # prerequisite never reached done
        rollup = _rollup_for(
            store,
            [_item("dep"), _item("dependent", prerequisites=("dep",))],
        )
    finally:
        store.close()
    dependent = next(
        t for p in rollup.phases for t in p.tasks if t.task_id == "dependent"
    )
    assert dependent.status is RollupStatus.BLOCKED_BY_PREREQ
    assert dependent.unsatisfied_prerequisites == ("dep",)


def test_satisfied_prerequisite_leaves_dependent_ready() -> None:
    store = SqliteStore(":memory:")
    try:
        _seed_verified(store, "dep")  # prerequisite reached done, verified
        rollup = _rollup_for(
            store,
            [_item("dep"), _item("dependent", prerequisites=("dep",))],
        )
    finally:
        store.close()
    dependent = next(
        t for p in rollup.phases for t in p.tasks if t.task_id == "dependent"
    )
    assert dependent.status is RollupStatus.NOT_STARTED
    assert dependent.unsatisfied_prerequisites == ()


def test_phase_grouping_and_totals() -> None:
    store = SqliteStore(":memory:")
    try:
        _seed_verified(store, "a")
        _seed_running(store, "b")
        rollup = _rollup_for(
            store,
            [
                _item("a", phase="phase-1"),
                _item("b", phase="phase-1"),
                _item("c", phase="phase-2"),
            ],
        )
    finally:
        store.close()
    phases = {p.name: p for p in rollup.phases}
    assert set(phases) == {"phase-1", "phase-2"}
    assert phases["phase-1"].verified == 1  # a verified, b in progress
    assert rollup.totals == {
        RollupStatus.VERIFIED.value: 1,
        RollupStatus.IN_PROGRESS.value: 1,
        RollupStatus.NOT_STARTED.value: 1,
    }


def test_json_shape_is_evidence_first() -> None:
    store = SqliteStore(":memory:")
    try:
        _seed_verified(store, "a")
        rollup = _rollup_for(store, [_item("a", phase="phase-1")])
    finally:
        store.close()
    payload = rollup_to_json(rollup)
    assert payload["totals"] == {RollupStatus.VERIFIED.value: 1}
    phase = payload["phases"][0]
    assert phase["name"] == "phase-1"
    assert phase["verified"] == 1
    task = phase["tasks"][0]
    assert task == {
        "task_id": "a",
        "status": "verified",
        "graders": {"passed": 1, "total": 1},
        "prerequisites": [],
        "unsatisfied_prerequisites": [],
        "detail": "",
    }


def test_render_text_is_emoji_free_and_labels_evidence() -> None:
    store = SqliteStore(":memory:")
    try:
        _seed_verified(store, "a")
        _seed_done(store, "b")  # accepted: done, no graders
        rollup = _rollup_for(
            store, [_item("a", phase="p"), _item("b", phase="p")]
        )
    finally:
        store.close()
    text = render_rollup_text(rollup)
    assert "derived from grader evidence" in text
    assert "graders 1/1 passed" in text
    assert "agent claim, unverified" in text
    assert text.isascii()


def test_empty_rollup_renders_cleanly() -> None:
    store = SqliteStore(":memory:")
    try:
        rollup = _rollup_for(store, [])
    finally:
        store.close()
    assert rollup.phases == ()
    assert rollup.totals == {}
    assert "(no tasks)" in render_rollup_text(rollup)
