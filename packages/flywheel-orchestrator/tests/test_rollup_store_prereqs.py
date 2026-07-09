"""Rollup prerequisite resolution consults the store for unlisted ids.

``status --rollup`` classifies a never-started dependent as ``blocked_by_prereq``
only when a prerequisite is genuinely not done. The store is the authoritative
record of completion (``docs/data-taxonomy.md``): a prerequisite whose defining
task left the active listing -- e.g. its phase archived, moving the task JSON out
of ``active/`` -- is still *satisfied* when its lifecycle reached ``DONE``, even
though no listed row provides it.

These tests pin that contract for the rollup: an unlisted store-DONE
prerequisite unblocks its dependent (which reports ``not_started``, exactly as if
the prerequisite were listed and DONE), while an unlisted prerequisite with only
a non-DONE lifecycle -- or none at all -- keeps the dependent
``blocked_by_prereq``. The rollup resolves satisfaction through the same
store-DONE predicate the scheduler uses
(:func:`satisfied_prerequisites_from_store`), which reads the store only for ids
absent from the classified row set, so a fully-listed graph adds no store read.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator._rollup import (
    RollupStatus,
    Rollup,
    TaskRollup,
    build_rollup,
)
from flywheel_orchestrator._sources import WorkItem
from flywheel_orchestrator._workflow import status_rows_for_items

_NOW = datetime(2026, 7, 9, tzinfo=timezone.utc)


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


def _seed_done(store: SqliteStore, task_id: str) -> None:
    """A DONE lifecycle for an *unlisted* prerequisite -- e.g. one whose phase
    archived, so its task JSON is gone but its completion record remains."""
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-ok")
    lc.transition_to(Status.READY, now=_NOW)
    lc.transition_to(Status.RUNNING, now=_NOW)
    lc.transition_to(Status.VALIDATING, now=_NOW)
    lc.transition_to(Status.DONE, now=_NOW)
    store.create_lifecycle(lc)


def _seed_failed(store: SqliteStore, task_id: str) -> None:
    """A FAILED lifecycle -- a non-DONE record that must NOT satisfy."""
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}-fail")
    lc.transition_to(Status.READY, now=_NOW)
    lc.transition_to(Status.RUNNING, now=_NOW)
    lc.transition_to(Status.FAILED, error="boom", now=_NOW)
    store.create_lifecycle(lc)


def _rollup_for(store: SqliteStore, items: list[WorkItem]):
    return build_rollup(status_rows_for_items(items, store), store)


def _dependent(rollup: Rollup) -> TaskRollup:
    return next(
        t for p in rollup.phases for t in p.tasks if t.task_id == "dependent"
    )


def test_unlisted_store_done_prereq_unblocks_dependent() -> None:
    """A prerequisite absent from the listing but carrying a DONE lifecycle is
    satisfied off the store, so the never-started dependent reports
    ``not_started`` -- not ``blocked_by_prereq``."""
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "A")  # unlisted: no _item("A") is passed
        rollup = _rollup_for(store, [_item("dependent", prerequisites=("A",))])
    finally:
        store.close()
    dependent = _dependent(rollup)
    assert dependent.status is RollupStatus.NOT_STARTED
    assert dependent.unsatisfied_prerequisites == ()


def test_unlisted_prereq_only_failed_keeps_dependent_blocked() -> None:
    """The discrimination screen: an unlisted prerequisite whose only lifecycle
    is FAILED does not satisfy, so the dependent stays ``blocked_by_prereq`` with
    that id in ``unsatisfied_prerequisites``. Only a DONE lifecycle satisfies --
    the rollup borrows the scheduler's store-DONE predicate, not a looser one."""
    store = SqliteStore(":memory:")
    try:
        _seed_failed(store, "A")  # unlisted, FAILED -- never satisfies
        rollup = _rollup_for(store, [_item("dependent", prerequisites=("A",))])
    finally:
        store.close()
    dependent = _dependent(rollup)
    assert dependent.status is RollupStatus.BLOCKED_BY_PREREQ
    assert dependent.unsatisfied_prerequisites == ("A",)
    assert "A" in dependent.detail


def test_unlisted_prereq_with_no_lifecycle_keeps_dependent_blocked() -> None:
    """An unlisted prerequisite with no lifecycle at all is unsatisfied, so the
    dependent is ``blocked_by_prereq`` -- the store consult finds nothing to
    resolve and the classification is unchanged from the missing-id case."""
    store = SqliteStore(":memory:")
    try:
        rollup = _rollup_for(store, [_item("dependent", prerequisites=("A",))])
    finally:
        store.close()
    dependent = _dependent(rollup)
    assert dependent.status is RollupStatus.BLOCKED_BY_PREREQ
    assert dependent.unsatisfied_prerequisites == ("A",)


def test_mixed_unlisted_prereqs_list_only_the_missing_id() -> None:
    """Two unlisted prerequisites -- one store-DONE, one with no lifecycle -- and
    the dependent reports ``blocked_by_prereq`` naming ONLY the genuinely missing
    id in both ``unsatisfied_prerequisites`` and the waiting-on detail."""
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "A")  # satisfied off the store
        # "B" has no lifecycle at all -- the one genuinely missing prerequisite.
        rollup = _rollup_for(
            store, [_item("dependent", prerequisites=("A", "B"))]
        )
    finally:
        store.close()
    dependent = _dependent(rollup)
    assert dependent.status is RollupStatus.BLOCKED_BY_PREREQ
    assert dependent.unsatisfied_prerequisites == ("B",)
    assert dependent.detail == "waiting on: B"


def test_all_unlisted_prereqs_store_done_reports_not_started() -> None:
    """A dependent whose every prerequisite is store-DONE (all unlisted) reports
    ``not_started``, exactly as if the prerequisites were listed and DONE."""
    store = SqliteStore(":memory:")
    try:
        _seed_done(store, "A")
        _seed_done(store, "B")
        rollup = _rollup_for(
            store, [_item("dependent", prerequisites=("A", "B"))]
        )
    finally:
        store.close()
    dependent = _dependent(rollup)
    assert dependent.status is RollupStatus.NOT_STARTED
    assert dependent.unsatisfied_prerequisites == ()
