"""Held-out cross-backend parity test for the history listing read.

Criterion (sharpened by spec 00032 D-6): the history listing read computed
against a PostgresStore (a) enumerates EVERY terminal run the store holds and
(b) returns the same HistoryRow/HistoryRun field values (run id, task id,
status, attempt count, token total, cost total, turn total) that the *identical*
persisted state returns from a SqliteStore. The enumeration of terminal runs is
served by the backend-agnostic SI-3 seam list_lifecycles(statuses=TERMINAL)
(the (DONE, FAILED, FAILED_VALIDATION) set already named TERMINAL_STATUSES in
_history.py), NOT by a SQLite-only store._connection lifecycle SELECT — a
left-behind raw SELECT raises AttributeError on Postgres rather than
enumerating, so the Postgres arm cannot produce the complete terminal set.

This is a metamorphic / cross-backend equivalence test: it seeds the SAME
write-protocol sequence (save_task -> create_lifecycle -> save_attempt x2 ->
append_grader_result) for TWO terminal tasks PLUS one non-terminal lifecycle on
a SqliteStore and a PostgresStore, computes collect_history_rows on each, and
asserts:
  (i)  the set of returned run ids equals EXACTLY the seeded terminal run ids on
       BOTH backends (the non-terminal run is absent from each), and
  (ii) the two results are field-for-field equal on the named HistoryRun fields.
No expected value is baked on the Postgres arm: both reads are computed at test
time from the same seed and compared to each other.

Run: uv run pytest -k "history and parity"
Skips (does not fail) when the Postgres test container is unreachable.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any
from uuid import uuid4

import pytest

from flywheel_core import (
    Attempt,
    GraderResultRecord,
    Lifecycle,
    Outcome,
    PostgresStore,
    SqliteStore,
    Status,
    Task,
)
from flywheel_orchestrator._history import collect_history_rows

# The named parity surface from the criterion: run id, task id, status, attempt
# count, token total, cost total, turn total. We assert on EXACTLY these fields,
# pulled off HistoryRow.latest (a HistoryRun).
_PARITY_FIELDS = (
    "run_id",
    "task_id",
    "status",
    "attempts",
    "tokens_total",
    "cost_usd_total",
    "turns_total",
)

# A fixed epoch so both backends are seeded with byte-identical timestamps; the
# read read-back must coincide regardless of backend timestamp handling.
_T0 = datetime(2026, 1, 2, 3, 4, 5, tzinfo=timezone.utc)


def _seed_terminal(
    store: Any, task_id: str, run_id: str, *, tokens_a: int, tokens_b: int
) -> None:
    """Drive the WRITE protocol to persist one terminal, multi-attempt run with
    a grader receipt. Identical calls/values/timestamps on every backend.

    The per-attempt token/cost/turn values DIFFER between the two attempts so
    the run-level totals are non-trivial sums (not all-equal, not zero), which
    makes an empty/constant/zeroing read diverge from the real one. The two
    seeded tasks use different per-attempt token counts so their rolled-up
    totals differ, foreclosing a read that returns one task's row for both.
    """
    # save_task first so the lifecycle's task_id has a row to hang off of.
    task = Task(goal="parity probe", graders=[], id=task_id)
    store.save_task(task, now=_T0)

    # Build a terminal lifecycle: PENDING -> READY -> RUNNING -> VALIDATING -> DONE.
    lc = Lifecycle(task_id=task_id, run_id=run_id, source="contract-seed")
    lc.transition_to(Status.READY, now=_T0 + timedelta(seconds=1))
    lc.transition_to(Status.RUNNING, now=_T0 + timedelta(seconds=2))
    lc.transition_to(Status.VALIDATING, now=_T0 + timedelta(seconds=3))
    lc.transition_to(Status.DONE, now=_T0 + timedelta(seconds=4))
    store.create_lifecycle(lc)

    # Two attempts with deliberately different aggregate values so the totals
    # are sums and not coincidentally equal across the two attempts.
    store.save_attempt(
        run_id,
        Attempt(
            number=1,
            run_id=run_id,
            started_at=_T0 + timedelta(seconds=2),
            ended_at=_T0 + timedelta(seconds=2, milliseconds=500),
            outcome=Outcome.SUCCEEDED,
            input_tokens=tokens_a,
            iterations_completed=1,
            turns=3,
            total_cost_usd=0.25,
        ),
    )
    store.save_attempt(
        run_id,
        Attempt(
            number=2,
            run_id=run_id,
            started_at=_T0 + timedelta(seconds=3),
            ended_at=_T0 + timedelta(seconds=3, milliseconds=500),
            outcome=Outcome.SUCCEEDED,
            input_tokens=tokens_b,
            iterations_completed=1,
            turns=5,
            total_cost_usd=0.75,
        ),
    )

    # At least one grader receipt so the read traverses the grader-result table
    # on both backends.
    store.append_grader_result(
        GraderResultRecord(
            run_id=run_id,
            attempt_number=2,
            ordinal=0,
            grader_type="command",
            grader_spec={"type": "command", "run": "true"},
            passed=True,
            duration_ms=1,
            payload={"exit_code": 0},
            ts=_T0 + timedelta(seconds=3, milliseconds=600),
            grader_name="probe",
        )
    )


def _seed_non_terminal(store: Any, task_id: str, run_id: str) -> None:
    """Persist one IN-FLIGHT (running) lifecycle through the write protocol.

    The history listing must NOT enumerate this run: collect_history_rows
    enumerates terminal statuses (DONE/FAILED/FAILED_VALIDATION) through
    list_lifecycles(statuses=TERMINAL_STATUSES). Seeding a non-terminal run on
    both backends lets the test assert the returned run-id set is EXACTLY the
    terminal set — a backend that drops the status filter (and leaks this run)
    or silently special-cases an incomplete enumeration diverges.
    """
    task = Task(goal="in-flight probe", graders=[], id=task_id)
    store.save_task(task, now=_T0)
    lc = Lifecycle(task_id=task_id, run_id=run_id, source="contract-seed")
    lc.transition_to(Status.READY, now=_T0 + timedelta(seconds=1))
    lc.transition_to(Status.RUNNING, now=_T0 + timedelta(seconds=2))
    store.create_lifecycle(lc)
    store.save_attempt(
        run_id,
        Attempt(
            number=1,
            run_id=run_id,
            started_at=_T0 + timedelta(seconds=2),
            outcome=Outcome.RECOVERED,
            input_tokens=10,
            iterations_completed=0,
            turns=1,
            total_cost_usd=0.01,
        ),
    )


def _run_id_set(rows: list[Any]) -> set[str]:
    """The complete set of run ids the history listing enumerates: every
    latest run plus every prior run across every returned task row."""
    ids: set[str] = set()
    for row in rows:
        ids.add(row.latest.run_id)
        for prior in row.prior_runs:
            ids.add(prior.run_id)
    return ids


def _latest_field_tuple(rows: list[Any], task_id: str) -> tuple[Any, ...]:
    """Locate the HistoryRow for task_id and project its latest HistoryRun onto
    the named parity fields. Failing to find the row is a real failure (an empty
    or task-dropping read), not a skip."""
    matches = [r for r in rows if r.task_id == task_id]
    assert matches, (
        f"history listing returned no row for seeded task {task_id!r}; "
        f"got task_ids={[r.task_id for r in rows]!r}"
    )
    assert len(matches) == 1, (
        f"expected exactly one HistoryRow per task id; got {len(matches)} "
        f"for {task_id!r}"
    )
    run = matches[0].latest
    return tuple(getattr(run, name) for name in _PARITY_FIELDS)


def test_history_listing_parity_across_backends(
    tmp_path: Path, postgres_dsn: str | None
) -> None:
    if postgres_dsn is None:
        pytest.skip("Postgres backend skipped: no database reachable")

    # Identical seed identifiers on both backends so the read keys line up.
    # Two terminal tasks plus one non-terminal run: the terminal set is what the
    # history listing must enumerate; the non-terminal run must be absent.
    task_a = f"task-a-{uuid4().hex[:12]}"
    run_a = f"run-a-{uuid4().hex[:12]}"
    task_b = f"task-b-{uuid4().hex[:12]}"
    run_b = f"run-b-{uuid4().hex[:12]}"
    task_live = f"task-live-{uuid4().hex[:12]}"
    run_live = f"run-live-{uuid4().hex[:12]}"
    terminal_run_ids = {run_a, run_b}

    sqlite_store = SqliteStore(tmp_path / "contract.db")
    pg_store = PostgresStore(
        postgres_dsn,
        schema=f"flywheel_test_{uuid4().hex[:12]}",
        pool_min=1,
        pool_max=4,
    )
    try:
        for store in (sqlite_store, pg_store):
            _seed_terminal(store, task_a, run_a, tokens_a=100, tokens_b=400)
            _seed_terminal(store, task_b, run_b, tokens_a=70, tokens_b=130)
            _seed_non_terminal(store, task_live, run_live)

        sqlite_rows = collect_history_rows(sqlite_store)
        pg_rows = collect_history_rows(pg_store)

        # (i) ENUMERATION: the set of returned run ids must equal EXACTLY the
        # seeded terminal run ids on BOTH backends; the non-terminal run is
        # absent from each. A left-behind SQLite-only store._connection SELECT
        # would AttributeError on Postgres (no complete set); a dropped status
        # filter would leak run_live; an incomplete enumeration would miss a
        # terminal run. The Postgres arm computing the full, exact terminal set
        # proves the enumeration crossed backends through list_lifecycles.
        sqlite_ids = _run_id_set(sqlite_rows)
        pg_ids = _run_id_set(pg_rows)
        assert sqlite_ids == terminal_run_ids, (
            "SQLite history enumeration is not the exact terminal set: "
            f"got {sqlite_ids!r} expected {terminal_run_ids!r} "
            f"(non-terminal {run_live!r} must be absent)"
        )
        assert pg_ids == terminal_run_ids, (
            "Postgres history enumeration is not the exact terminal set: "
            f"got {pg_ids!r} expected {terminal_run_ids!r} "
            f"(non-terminal {run_live!r} must be absent; a SQLite-only "
            "store._connection SELECT cannot enumerate on Postgres)"
        )
        assert pg_ids == sqlite_ids, (
            "history enumeration diverged across backends: "
            f"sqlite={sqlite_ids!r} postgres={pg_ids!r}"
        )

        # (ii) FIELD PARITY: each terminal task's latest HistoryRun must agree
        # field-for-field across backends on the named parity surface. No
        # literal expected value is asserted on the Postgres arm — both reads
        # are independently computed from the same seed.
        for task_id in (task_a, task_b):
            sqlite_tuple = _latest_field_tuple(sqlite_rows, task_id)
            pg_tuple = _latest_field_tuple(pg_rows, task_id)
            assert pg_tuple == sqlite_tuple, (
                "history listing diverged across backends on parity fields "
                f"{_PARITY_FIELDS!r} for {task_id!r}: "
                f"sqlite={sqlite_tuple!r} postgres={pg_tuple!r}"
            )
    finally:
        sqlite_store.close()
        pg_store.close()
