"""Contract tests for the single human-review queue surface (spec 00069).

The work re-driver's foundation layer: one durable, queryable queue that lists
every unit a routing layer could not auto-recover, each carrying its task/run
identity and a machine-readable ``reason``. The queue is NOT a new persistence
silo -- it is a routed read over the existing append-only
``orchestrator_stop_events`` ledger, so these cases prove the read across
backends, the reason vocabulary is enforced (a stable token, never free text),
pre-run stop rows stay out of the queue, and no new table is introduced.

Parametrized over the in-memory and SQLite backends (mirroring the stop-event
ledger's own backend scope); a dedicated parity case proves SQLite and Postgres
read the queue back identically when a database is reachable.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING
from uuid import uuid4

import pytest

from flywheel_core.events import PARK_KIND_HELD_OUT_GATE
from flywheel_orchestrator import (
    HUMAN_REVIEW_QUEUE_REASONS,
    REASON_ABORTED,
    REASON_AWAITING_APPROVAL,
    REASON_BUDGET_CEILING,
    REASON_NO_PROGRESS,
    REASON_PREREQUISITE_MISSING,
    REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
    HumanReviewQueueEntry,
    InMemoryClaimStore,
    SqliteClaimStore,
)
from flywheel_orchestrator._claims import (
    STOP_DANGLING_PREREQUISITE,
    STOP_NO_OP_CYCLE,
    STOP_PREPARE_SKIP,
)

if TYPE_CHECKING:
    from flywheel_orchestrator import PostgresClaimStore

# The queue read lives on the in-memory and SQLite backends (Postgres parity is
# proved by a dedicated case below), mirroring the stop-event ledger's scope.
_QUEUE_BACKENDS = ("memory", "sqlite")


def _t(second: int) -> datetime:
    return datetime(2026, 5, 28, 12, 0, second, tzinfo=timezone.utc)


@pytest.fixture(params=_QUEUE_BACKENDS, ids=_QUEUE_BACKENDS)
def queue_store(
    request: pytest.FixtureRequest,
    tmp_path: Path,
) -> Iterator[InMemoryClaimStore | SqliteClaimStore]:
    if request.param == "memory":
        instance: InMemoryClaimStore | SqliteClaimStore = InMemoryClaimStore()
    else:
        instance = SqliteClaimStore(tmp_path / "queue.db")
    try:
        yield instance
    finally:
        instance.close()


def test_empty_queue_returns_empty_list(
    queue_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Edge case: an empty queue is an empty list, never an error.
    assert queue_store.list_human_review_queue() == []


def test_records_and_lists_all_routed_reason_kinds(
    queue_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Every routing layer writes the same shape; the one read returns the full
    # queue across all kinds in insertion order. Cover a run-keyed reason, the
    # task-keyed reasons, and a landing-park reason (folded in from
    # LANDING_PARK_KINDS) so the read spans the whole routed vocabulary.
    queue_store.record_human_review(
        reason=REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
        task_id="task-a",
        run_id="run-a1",
        detail="4/4 attempts failed after escalation",
        occurred_at=_t(0),
    )
    queue_store.record_human_review(
        reason=REASON_PREREQUISITE_MISSING,
        task_id="task-b",
        detail="prerequisite 'task-x' never lands",
        occurred_at=_t(1),
    )
    queue_store.record_human_review(
        reason=REASON_NO_PROGRESS,
        task_id="task-c",
        detail="no file changed across 3 attempts",
        occurred_at=_t(2),
    )
    queue_store.record_human_review(
        reason=REASON_AWAITING_APPROVAL,
        task_id="task-d",
        run_id="run-d1",
        detail="human gate: awaiting approval",
        occurred_at=_t(3),
    )
    queue_store.record_human_review(
        reason=REASON_ABORTED,
        task_id="task-e",
        run_id="run-e1",
        detail="operator aborted",
        occurred_at=_t(4),
    )
    queue_store.record_human_review(
        reason=REASON_BUDGET_CEILING,
        task_id="task-f",
        run_id="run-f1",
        detail="token budget ceiling hit",
        occurred_at=_t(5),
    )
    queue_store.record_human_review(
        reason=PARK_KIND_HELD_OUT_GATE,
        task_id="task-g",
        run_id="run-g1",
        detail="held-out gate refused the landing",
        occurred_at=_t(6),
    )

    queue = queue_store.list_human_review_queue()
    assert all(isinstance(entry, HumanReviewQueueEntry) for entry in queue)
    # Full queue, insertion order, every routed kind present exactly once.
    assert [(e.task_id, e.reason) for e in queue] == [
        ("task-a", REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION),
        ("task-b", REASON_PREREQUISITE_MISSING),
        ("task-c", REASON_NO_PROGRESS),
        ("task-d", REASON_AWAITING_APPROVAL),
        ("task-e", REASON_ABORTED),
        ("task-f", REASON_BUDGET_CEILING),
        ("task-g", PARK_KIND_HELD_OUT_GATE),
    ]
    # Ordered by underlying ledger row id (insertion order).
    assert [e.id for e in queue] == sorted(e.id for e in queue)
    # Every entry carries its machine-readable reason and human detail.
    first = queue[0]
    assert first.reason in HUMAN_REVIEW_QUEUE_REASONS
    assert first.detail == "4/4 attempts failed after escalation"
    assert first.occurred_at == _t(0)


def test_task_and_run_identity_carried(
    queue_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # A run-keyed stop carries both task and run identity; a task-keyed stop
    # (a missing prerequisite has no run yet) carries the task and an empty run.
    queue_store.record_human_review(
        reason=REASON_ABORTED,
        task_id="task-a",
        run_id="run-a1",
        detail="operator aborted",
        occurred_at=_t(0),
    )
    queue_store.record_human_review(
        reason=REASON_PREREQUISITE_MISSING,
        task_id="task-b",
        detail="prerequisite 'task-x' never lands",
        occurred_at=_t(1),
    )
    by_task = {e.task_id: e for e in queue_store.list_human_review_queue()}
    assert by_task["task-a"].run_id == "run-a1"
    assert by_task["task-b"].run_id == ""


def test_reason_must_be_machine_readable_token(
    queue_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # A reason must be a stable token from the routed vocabulary, never a
    # free-text detail string; an unknown reason is rejected and writes nothing.
    with pytest.raises(ValueError):
        queue_store.record_human_review(
            reason="the agent gave up after a while",
            task_id="task-a",
            detail="free text masquerading as a reason",
            occurred_at=_t(0),
        )
    assert queue_store.list_human_review_queue() == []


def test_pre_run_stop_kinds_excluded_from_queue(
    queue_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Pre-run stop rows share the ledger but are NOT queue entries: their kinds
    # are disjoint from the human-review reason vocabulary, so the queue read
    # projects only the routed rows.
    queue_store.record_stop_event(
        kind=STOP_DANGLING_PREREQUISITE,
        subject="task-a",
        detail="prerequisite 'missing' resolves to no work item",
        occurred_at=_t(0),
    )
    queue_store.record_stop_event(
        kind=STOP_NO_OP_CYCLE,
        subject="queue-dir",
        detail="idle (observed queue depth 0, target 5)",
        occurred_at=_t(1),
    )
    claim = queue_store.acquire_claim(
        "task-b", "worker-1", now=_t(2), lease_seconds=30
    )
    assert claim is not None
    queue_store.record_prepare_skip(claim, detail="boom", now=_t(3))

    # None of the pre-run kinds belong to the review vocabulary.
    for kind in (
        STOP_DANGLING_PREREQUISITE,
        STOP_NO_OP_CYCLE,
        STOP_PREPARE_SKIP,
    ):
        assert kind not in HUMAN_REVIEW_QUEUE_REASONS
    # The stop-event ledger holds the pre-run rows...
    assert len(queue_store.list_stop_events()) == 3
    # ...but the human-review queue sees none of them.
    assert queue_store.list_human_review_queue() == []

    # A routed row now appears in the queue while the pre-run rows stay out.
    queue_store.record_human_review(
        reason=REASON_NO_PROGRESS,
        task_id="task-b",
        detail="no file changed across 3 attempts",
        occurred_at=_t(4),
    )
    queue = queue_store.list_human_review_queue()
    assert [(e.task_id, e.reason) for e in queue] == [
        ("task-b", REASON_NO_PROGRESS)
    ]


def test_queue_introduces_no_new_silo_table(tmp_path: Path) -> None:
    # Criterion #12: the queue is a routed read over the existing stop-event
    # ledger, not a new persistence silo. After routing a unit, the routed row
    # lives in ``orchestrator_stop_events`` and no review-queue table exists.
    db_path = tmp_path / "no_silo.db"
    store = SqliteClaimStore(db_path)
    try:
        store.record_human_review(
            reason=REASON_ABORTED,
            task_id="task-a",
            run_id="run-a1",
            detail="operator aborted",
            occurred_at=_t(0),
        )
    finally:
        store.close()

    conn = sqlite3.connect(db_path)
    try:
        tables = {
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            )
        }
        # No dedicated queue silo was created under any plausible name.
        for forbidden in (
            "human_review_queue",
            "review_queue",
            "redriver_queue",
            "human_review",
        ):
            assert forbidden not in tables
        # The routed row lives on the existing stop-event ledger.
        assert "orchestrator_stop_events" in tables
        (count,) = conn.execute(
            "SELECT COUNT(*) FROM orchestrator_stop_events "
            "WHERE kind = ? AND subject = ? AND run_id = ?",
            (REASON_ABORTED, "task-a", "run-a1"),
        ).fetchone()
        assert count == 1
    finally:
        conn.close()


def test_queue_never_dedupes_recurrences(
    queue_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # The same unit routed three times is three queue entries -- recurrence is
    # the signal, so a queue that collapses them must fail.
    for _ in range(3):
        queue_store.record_human_review(
            reason=REASON_NO_PROGRESS,
            task_id="task-a",
            detail="no file changed across 3 attempts",
            occurred_at=_t(0),
        )
    queue = queue_store.list_human_review_queue()
    assert len(queue) == 3
    assert len({e.id for e in queue}) == 3  # distinct, monotonic ids


def test_sqlite_postgres_queue_parity(
    tmp_path: Path,
    postgres_dsn: str | None,
) -> None:
    # The queue reads back identically across the durable backends: same
    # reasons, task/run identity, details, and insertion order (ids dropped).
    if postgres_dsn is None:
        pytest.skip("Postgres backend skipped: no database reachable")
    from flywheel_orchestrator import PostgresClaimStore

    sqlite_store = SqliteClaimStore(tmp_path / "queue_parity.db")
    pg_store = PostgresClaimStore(
        postgres_dsn,
        schema=f"flywheel_claims_test_{uuid4().hex[:12]}",
        pool_min=1,
        pool_max=4,
    )

    def _drive(store: SqliteClaimStore | PostgresClaimStore) -> None:
        store.record_human_review(
            reason=REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
            task_id="task-a",
            run_id="run-a1",
            detail="4/4 attempts failed after escalation",
            occurred_at=_t(0),
        )
        store.record_human_review(
            reason=REASON_PREREQUISITE_MISSING,
            task_id="task-b",
            detail="prerequisite 'task-x' never lands",
            occurred_at=_t(1),
        )
        store.record_human_review(
            reason=PARK_KIND_HELD_OUT_GATE,
            task_id="task-c",
            run_id="run-c1",
            detail="held-out gate refused the landing",
            occurred_at=_t(2),
        )

    def _tuples(rows: list[HumanReviewQueueEntry]) -> list:
        return [
            (e.task_id, e.run_id, e.reason, e.detail, e.occurred_at)
            for e in rows
        ]

    try:
        _drive(sqlite_store)
        _drive(pg_store)
        assert _tuples(
            sqlite_store.list_human_review_queue()
        ) == _tuples(pg_store.list_human_review_queue())
    finally:
        sqlite_store.close()
        pg_store.close()
