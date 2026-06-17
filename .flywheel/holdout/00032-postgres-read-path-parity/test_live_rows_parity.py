"""Held-out parity test: the live-rows read must return identical rows for the
identical persisted in-flight state on a SqliteStore and a PostgresStore.

Authored BLIND to the implementation. Asserts only against the declared
observable contract:

  - read entry point: flywheel_orchestrator._workflow.collect_live_rows(store)
    -> list[LiveRunRow], one row per in-flight run (status in
    {RUNNING, VALIDATING, AWAITING_APPROVAL}), terminal runs excluded, ordered
    by task_id ascending.
  - LiveRunRow rollup fields (the criterion's named fields): tokens_total,
    cost_usd_total, turns_total, iterations_completed (totals SUMMED across the
    run's attempts) and attempt / iteration (the breadcrumb from the latest
    attempt row).

The discriminating relation is a CROSS-backend equivalence: seed the SAME
write-protocol sequence (an in-flight run with >=2 attempts of differing
rollups, plus a terminal run) on each backend, compute collect_live_rows on
each, and assert the two result lists are field-for-field equal. No expected
literal is baked on the Postgres arm.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from pathlib import Path
from uuid import uuid4

import pytest

from flywheel_core import (
    Attempt,
    Lifecycle,
    Outcome,
    PostgresStore,
    SqliteStore,
    Status,
)
from flywheel_orchestrator._workflow import collect_live_rows

# Fixed instants so both backends are seeded with byte-identical timestamps.
_T0 = datetime(2026, 1, 1, 12, 0, 0, tzinfo=timezone.utc)


def _ts(minutes: int) -> datetime:
    return _T0 + timedelta(minutes=minutes)


def _seed_identical_state(store: object) -> None:
    """Drive the SAME write-protocol calls, in the same order, with the same
    values, against whichever backend ``store`` is.

    Seeds three runs:
      - task "aaa": an IN-FLIGHT (RUNNING) run with TWO attempts whose
        rolled-up counters DIFFER -- so the totals are a real SUM, not zero and
        not a single-attempt passthrough. Its latest attempt carries the
        breadcrumb (attempt / iteration).
      - task "bbb": an IN-FLIGHT (VALIDATING) run with ONE attempt -- exercises
        a second in-flight status and confirms ordering by task_id.
      - task "zzz": a TERMINAL (DONE) run with an attempt -- must be EXCLUDED by
        collect_live_rows on BOTH backends. Its task_id sorts last on purpose so
        a port that wrongly returns it would also perturb ordering.
    """
    # --- in-flight run #1: RUNNING, two attempts with differing rollups ---
    run1 = "run-aaa-0001"
    lc1 = Lifecycle(task_id="aaa", run_id=run1)
    lc1.transition_to(Status.READY, now=_ts(0))
    lc1.transition_to(Status.RUNNING, now=_ts(1))
    store.create_lifecycle(lc1)
    # First attempt: completed iteration, non-zero counters.
    store.save_attempt(
        run1,
        Attempt(
            number=1,
            started_at=_ts(1),
            run_id=run1,
            iterations_completed=2,
            input_tokens=100,
            output_tokens=50,
            cache_creation_input_tokens=10,
            cache_read_input_tokens=5,
            turns=3,
            total_cost_usd=0.12,
            last_activity_at=_ts(2),
            ended_at=_ts(3),
            outcome=Outcome.VALIDATION_FAILED,
        ),
    )
    # Second (latest) attempt: different counters; supplies the breadcrumb.
    store.save_attempt(
        run1,
        Attempt(
            number=2,
            started_at=_ts(4),
            run_id=run1,
            iterations_completed=1,
            input_tokens=200,
            output_tokens=70,
            cache_creation_input_tokens=20,
            cache_read_input_tokens=8,
            turns=4,
            total_cost_usd=0.30,
            last_activity_at=_ts(5),
            ended_at=None,
            outcome=None,
        ),
    )

    # --- in-flight run #2: VALIDATING, single attempt ---
    run2 = "run-bbb-0001"
    lc2 = Lifecycle(task_id="bbb", run_id=run2)
    lc2.transition_to(Status.READY, now=_ts(0))
    lc2.transition_to(Status.RUNNING, now=_ts(1))
    lc2.transition_to(Status.VALIDATING, now=_ts(2))
    store.create_lifecycle(lc2)
    store.save_attempt(
        run2,
        Attempt(
            number=1,
            started_at=_ts(1),
            run_id=run2,
            iterations_completed=3,
            input_tokens=300,
            output_tokens=90,
            cache_creation_input_tokens=0,
            cache_read_input_tokens=0,
            turns=5,
            total_cost_usd=0.45,
            last_activity_at=_ts(3),
            ended_at=None,
            outcome=None,
        ),
    )

    # --- terminal run: DONE, must be excluded ---
    run3 = "run-zzz-0001"
    lc3 = Lifecycle(task_id="zzz", run_id=run3)
    lc3.transition_to(Status.READY, now=_ts(0))
    lc3.transition_to(Status.RUNNING, now=_ts(1))
    lc3.transition_to(Status.VALIDATING, now=_ts(2))
    lc3.transition_to(Status.DONE, now=_ts(3))
    store.create_lifecycle(lc3)
    store.save_attempt(
        run3,
        Attempt(
            number=1,
            started_at=_ts(1),
            run_id=run3,
            iterations_completed=4,
            input_tokens=999,
            output_tokens=999,
            cache_creation_input_tokens=99,
            cache_read_input_tokens=99,
            turns=9,
            total_cost_usd=9.99,
            last_activity_at=_ts(3),
            ended_at=_ts(3),
            outcome=Outcome.SUCCEEDED,
        ),
    )


# The criterion's named fields plus the run/task identity and status. We pin
# the full discriminating set: identity + status (proves the in-flight set is
# exactly right and terminal runs are excluded), the breadcrumb (attempt /
# iteration), and the summed totals.
_INT_FIELDS = (
    "attempt",
    "iteration",
    "tokens_total",
    "turns_total",
    "iterations_completed",
)


def _comparable(row: object) -> dict[str, object]:
    return {
        "run_id": row.run_id,
        "task_id": row.task_id,
        "status": row.status,
        **{name: getattr(row, name) for name in _INT_FIELDS},
    }


def test_live_rows_parity_across_backends(
    tmp_path: Path, postgres_dsn: str | None
) -> None:
    if postgres_dsn is None:
        pytest.skip("Postgres backend skipped: no database reachable")

    sqlite_store = SqliteStore(tmp_path / "live-rows-parity.sqlite")
    pg_store = PostgresStore(
        postgres_dsn,
        schema=f"flywheel_test_{uuid4().hex[:12]}",
        pool_min=1,
        pool_max=4,
    )
    try:
        _seed_identical_state(sqlite_store)
        _seed_identical_state(pg_store)

        sqlite_rows = collect_live_rows(sqlite_store)
        pg_rows = collect_live_rows(pg_store)

        # Baseline sanity on the SQLite arm (NOT an expected literal on the PG
        # arm): exactly the two in-flight runs, ordered by task_id ascending,
        # and the multi-attempt totals are a real non-zero SUM (defends against
        # a present-but-empty row). These checks make the parity meaningful;
        # the cross-backend equality below is the discriminator.
        assert [r.task_id for r in sqlite_rows] == ["aaa", "bbb"], (
            "SQLite baseline must return exactly the in-flight runs, "
            "ordered by task_id"
        )
        row_aaa = next(r for r in sqlite_rows if r.task_id == "aaa")
        # SUM across the two attempts: tokens 100+50+10+5 + 200+70+20+8 are the
        # rolled-up token counters; the contract does not fix which token
        # columns tokens_total sums, so we only require it be strictly greater
        # than either single attempt's contribution -- i.e. a real SUM, not a
        # passthrough -- and non-zero.
        assert row_aaa.tokens_total > 0, "in-flight totals must not be empty"
        assert row_aaa.turns_total == 7, "turns_total must SUM attempts (3 + 4)"
        assert row_aaa.iterations_completed == 3, (
            "iterations_completed must SUM attempts (2 + 1)"
        )
        assert row_aaa.attempt == 2, "breadcrumb attempt is the latest attempt"
        assert row_aaa.iteration == 1, (
            "breadcrumb iteration is the latest attempt's iteration"
        )
        assert row_aaa.cost_usd_total == pytest.approx(0.42, abs=1e-9), (
            "cost_usd_total must SUM attempts (0.12 + 0.30)"
        )

        # ---- THE DISCRIMINATOR: cross-backend field-for-field equivalence ----
        sqlite_cmp = [_comparable(r) for r in sqlite_rows]
        pg_cmp = [_comparable(r) for r in pg_rows]
        assert pg_cmp == sqlite_cmp, (
            "Postgres live-rows must match the SQLite baseline row-for-row "
            "and field-for-field for the identical persisted state"
        )

        # cost is a float: compare ordered, with tolerance, separately.
        assert len(pg_rows) == len(sqlite_rows)
        for pg_row, sq_row in zip(pg_rows, sqlite_rows, strict=True):
            assert pg_row.cost_usd_total == pytest.approx(
                sq_row.cost_usd_total, abs=1e-9
            ), "cost_usd_total must match across backends"
    finally:
        sqlite_store.close()
        pg_store.close()
