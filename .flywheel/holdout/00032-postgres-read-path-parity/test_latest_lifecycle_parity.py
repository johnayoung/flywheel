"""Held-out parity test for the most-recent-lifecycle-for-task read path.

Pins a CROSS-backend equivalence: seeded with the identical persisted state
(two runs of one task with a defined recency order), the by-task-id latest-
lifecycle selection -- exposed through ``resolve_run_id`` in its task-id form --
must resolve to the SAME run id on a ``PostgresStore`` as it does on a
``SqliteStore``. The Postgres result is compared against the SQLite-computed
reference (NOT a baked literal); the SQLite reference is itself anchored to the
most-recently-updated run so the equivalence cannot pass on two matching-wrong
answers. The literal-run-id branch is pinned to cross identically too.

The seed gives the two runs an unambiguous recency order (one hour apart) and
chooses run ids so that returning the first-inserted row, or sorting by run id
alone, would yield the WRONG run -- so a no-op or a wrong-ordering backend read
is discriminated, not merely a None-returning one.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from uuid import uuid4

import pytest

from flywheel_core import Lifecycle, PostgresStore, SqliteStore, Status
from flywheel_orchestrator._history import resolve_run_id

_T0 = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)

# Recency winner is the LATER run. Run ids are chosen so the winner is neither
# the first-inserted row (run-old goes in first) nor the lexicographically
# largest run id ("run-old" > "run-new"); only selecting by update RECENCY
# yields "run-new".
_OLD_RUN = "run-old"
_NEW_RUN = "run-new"
_TASK_ID = "t1"


def _seed_two_runs(store: SqliteStore | PostgresStore) -> None:
    """Write the identical state on whichever backend ``store`` is.

    ``run-old`` is transitioned (and inserted) first at ``_T0``; ``run-new`` is
    transitioned an hour later at ``_T0 + 1h``. The update time advances on each
    transition, so ``run-new`` is the most-recently-updated lifecycle of task
    ``t1``. The same calls run in the same order with the same timestamps on
    both backends.
    """
    early = Lifecycle(task_id=_TASK_ID, run_id=_OLD_RUN)
    early.transition_to(Status.READY, now=_T0)
    early.transition_to(Status.RUNNING, now=_T0)
    store.create_lifecycle(early)

    late = Lifecycle(task_id=_TASK_ID, run_id=_NEW_RUN)
    late.transition_to(Status.READY, now=_T0 + timedelta(hours=1))
    late.transition_to(Status.RUNNING, now=_T0 + timedelta(hours=1))
    store.create_lifecycle(late)


def _sqlite_reference(tmp_path) -> tuple[str | None, str | None, str | None]:
    """Compute (task-id form, run-old literal, run-new literal) on a fresh
    SqliteStore seeded with the identical state. This is the cross-backend
    reference the Postgres arm must match -- a computed value, never a literal.
    """
    store = SqliteStore(tmp_path / "ref.sqlite")
    try:
        _seed_two_runs(store)
        return (
            resolve_run_id(store, _TASK_ID),
            resolve_run_id(store, _OLD_RUN),
            resolve_run_id(store, _NEW_RUN),
        )
    finally:
        store.close()


@pytest.mark.parametrize("backend", ["sqlite", "postgres"])
def test_latest_lifecycle_resolve_run_id_parity(
    backend: str, tmp_path, postgres_dsn: str | None
) -> None:
    # The SQLite-computed reference. Anchor it to the recency winner so the
    # equivalence below cannot be satisfied by two backends agreeing on a wrong
    # answer: if the SQLite read itself returned run-old (first row) or sorted
    # by run id, these anchors fail and the cross-backend claim never gets the
    # chance to pass on a matched mistake.
    ref_by_task, ref_by_old, ref_by_new = _sqlite_reference(tmp_path)
    assert ref_by_task == _NEW_RUN  # most-recently-updated, not first/lex.
    assert ref_by_old == _OLD_RUN
    assert ref_by_new == _NEW_RUN

    if backend == "sqlite":
        store: SqliteStore | PostgresStore = SqliteStore(
            tmp_path / "flywheel.sqlite"
        )
    else:
        if postgres_dsn is None:
            pytest.skip("Postgres backend not reachable")
        store = PostgresStore(
            postgres_dsn,
            schema=f"flywheel_test_{uuid4().hex[:12]}",
            pool_min=1,
            pool_max=4,
        )

    try:
        _seed_two_runs(store)
        # task-id form: the most-recently-updated run wins.
        by_task = resolve_run_id(store, _TASK_ID)
        # literal-run-id form: the argument is returned as-is when it names a run.
        by_old_run = resolve_run_id(store, _OLD_RUN)
        by_new_run = resolve_run_id(store, _NEW_RUN)
    finally:
        store.close()

    # CROSS-backend equivalence: this backend's reads equal the SQLite reference
    # computed from the identical persisted state. On the Postgres arm this is
    # the parity claim the criterion names -- and because the reference is
    # pinned to the recency winner above, equality also means "correct".
    assert by_task == ref_by_task
    assert by_old_run == ref_by_old
    assert by_new_run == ref_by_new
