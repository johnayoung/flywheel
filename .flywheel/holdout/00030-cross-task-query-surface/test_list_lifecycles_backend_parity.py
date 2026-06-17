"""Held-out acceptance test: cross-backend parity of ``list_lifecycles``.

This test defends spec 00030's cross-task query surface against the audit's
**M1 lesson** -- a method that is *advertised* on every backend but *non-functional*
(or divergent) on Postgres. It seeds an identical set of mixed-status, mixed-attempt
lifecycles into each available backend, applies the same filter(s), and asserts that
the three backends agree on:

  1. the SET of ``run_id`` values returned for a filter (membership), AND
  2. the fully-folded ``Lifecycle`` objects per ``run_id`` (field-for-field dataclass
     equality, including populated ``attempts``).

The robust comparison is order-free: build ``{lc.run_id: lc for lc in result}`` on each
backend and compare the dicts. ``Lifecycle`` has no ``updated_at`` field, so this
equality does not depend on the server-set timestamp that drives row ordering -- which
is precisely why this test deliberately does NOT assert result ORDER.

Memory-vs-SQLite parity is asserted unconditionally. The Postgres leg is *skipped*
(never failed) when ``postgres_dsn`` is ``None``, mirroring ``test_store_contract.py``.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

import pytest

from flywheel_core import (
    Attempt,
    InMemoryStore,
    Lifecycle,
    SqliteStore,
    Status,
)

_BASE = datetime(2024, 1, 1, tzinfo=timezone.utc)

# A fixed task id we will also filter on. A second task id ensures the
# task_id filter actually discriminates (it must exclude the other task).
_TASK_A = "task-alpha"
_TASK_B = "task-beta"

# The seed: a mixed-status, mixed-attempt population. Each row is
# (run_id, task_id, status, attempt_numbers). We deliberately span several
# statuses (including ones whose names differ only by case/underscore so a
# case-sensitive or name-mangling Postgres path diverges), give some rows
# multiple attempts and some none, and split rows across two task ids.
_SEED: tuple[tuple[str, str, Status, tuple[int, ...]], ...] = (
    ("r-run-1", _TASK_A, Status.RUNNING, (1, 2)),
    ("r-run-2", _TASK_B, Status.RUNNING, ()),
    ("r-ready", _TASK_A, Status.READY, (1,)),
    ("r-done", _TASK_A, Status.DONE, (1, 2, 3)),
    ("r-failed", _TASK_B, Status.FAILED, (1,)),
    ("r-await", _TASK_A, Status.AWAITING_APPROVAL, ()),
    ("r-fv", _TASK_B, Status.FAILED_VALIDATION, (1,)),
    ("r-pending", _TASK_A, Status.PENDING, ()),
)


def _seed_backend(store: object) -> None:
    """Seed the SAME population into ``store`` through the public write API.

    No raw SQL: create each lifecycle, then save each attempt. The status is
    set on the ``Lifecycle`` at construction time so the row lands in the
    intended status for the filter assertions.
    """
    for run_id, task_id, status, attempt_numbers in _SEED:
        store.create_lifecycle(  # type: ignore[attr-defined]
            Lifecycle(task_id=task_id, run_id=run_id, status=status)
        )
        for number in attempt_numbers:
            store.save_attempt(  # type: ignore[attr-defined]
                run_id,
                Attempt(number=number, started_at=_BASE, run_id=run_id),
            )


def _as_dict(result: list[Lifecycle]) -> dict[str, Lifecycle]:
    """Order-free view keyed by run_id; compares membership AND every folded field."""
    return {lc.run_id: lc for lc in result}


# The filters under test. Each is a kwargs dict passed straight to
# ``list_lifecycles``. We cover: a single-status filter, a task_id filter, and
# the no-filter (everything) case. The no-filter case forces every seeded row
# to fold identically across backends.
_FILTERS: tuple[tuple[str, dict[str, object]], ...] = (
    ("status_running", {"statuses": {Status.RUNNING}}),
    ("status_multi", {"statuses": {Status.DONE, Status.FAILED}}),
    ("task_alpha", {"task_id": _TASK_A}),
    ("no_filter", {}),
)


def _assert_parity(left: object, right: object, left_name: str, right_name: str) -> None:
    """Assert two backends agree on membership + fold for every filter."""
    for filter_name, kwargs in _FILTERS:
        left_result = _as_dict(left.list_lifecycles(**kwargs))  # type: ignore[attr-defined]
        right_result = _as_dict(right.list_lifecycles(**kwargs))  # type: ignore[attr-defined]

        # Membership parity: the SET of run_ids must match.
        assert set(left_result) == set(right_result), (
            f"filter {filter_name!r}: run_id membership diverges between "
            f"{left_name} ({sorted(left_result)}) and "
            f"{right_name} ({sorted(right_result)})"
        )

        # Fold parity: every folded Lifecycle (incl. attempts) must be equal.
        # Comparing the full dicts catches both a divergent set AND any
        # field-for-field divergence (e.g. dropped attempts) in one assertion.
        assert left_result == right_result, (
            f"filter {filter_name!r}: folded Lifecycle objects diverge between "
            f"{left_name} and {right_name}"
        )


def test_list_lifecycles_backend_parity(
    tmp_path: Path,
    postgres_dsn: str | None,
) -> None:
    """In-memory, SQLite, and Postgres must return identical results.

    Memory-vs-SQLite parity is unconditional. The Postgres comparison is
    skipped (not failed) when no database is reachable.
    """
    mem = InMemoryStore()
    sql = SqliteStore(tmp_path / "parity.db")

    pg: object | None = None
    if postgres_dsn is not None:
        from flywheel_core import PostgresStore

        pg = PostgresStore(
            postgres_dsn,
            schema=f"flywheel_test_{uuid4().hex[:12]}",
            pool_min=1,
            pool_max=4,
        )

    try:
        _seed_backend(mem)
        _seed_backend(sql)
        if pg is not None:
            _seed_backend(pg)

        # Unconditional: memory and sqlite must agree on every filter.
        _assert_parity(mem, sql, "memory", "sqlite")

        # Postgres leg: skip (do not fail) when unavailable; otherwise it must
        # agree with both other backends. This is the M1 discriminator -- a
        # Postgres path that raises or diverges fails here, never silently.
        if pg is None:
            pytest.skip("Postgres backend skipped: no database reachable")
        _assert_parity(mem, pg, "memory", "postgres")
        _assert_parity(sql, pg, "sqlite", "postgres")
    finally:
        sql.close()
        if pg is not None:
            pg.close()  # type: ignore[attr-defined]
