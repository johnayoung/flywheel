"""Held-out acceptance test: ``ClaimStore.list_claims`` enumerates every live claim.

Parametrized over every :class:`flywheel_orchestrator.ClaimStore` backend
(in-memory, SQLite, Postgres) so all three prove identical enumeration
semantics. The Postgres backend skips -- never fails -- when no database is
reachable, reusing the root conftest ``postgres_dsn`` fixture exactly like the
committed claims contract suite.

Discriminator: with two or more live claims owned by distinct workers, the
enumeration returns one entry per held claim, each carrying its OWN owning
worker_id alongside its task_id. The assertion is set-equality over the
(task_id, worker_id) pairs the test itself acquired -- so it fails an impl that
returns only one claim, returns task_ids with the worker stripped/blanked,
scrambles workers across rows, or returns a count instead of the pairs.
"""

from __future__ import annotations

from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

import pytest

from flywheel_orchestrator import (
    ClaimStore,
    InMemoryClaimStore,
    SqliteClaimStore,
    TaskClaim,
)

# The Postgres container is provided session-scoped by the root conftest
# (``postgres_dsn``); a None DSN skips the postgres cases.
_STORE_BACKENDS = ("memory", "sqlite", "postgres")


def _t(second: int) -> datetime:
    return datetime(2026, 5, 28, 12, 0, second, tzinfo=timezone.utc)


@pytest.fixture(params=_STORE_BACKENDS, ids=_STORE_BACKENDS)
def store(
    request: pytest.FixtureRequest,
    tmp_path: Path,
    postgres_dsn: str | None,
) -> Iterator[object]:
    param = request.param
    if param == "memory":
        instance: object = InMemoryClaimStore()
    elif param == "sqlite":
        instance = SqliteClaimStore(tmp_path / "claims.db")
    else:
        if postgres_dsn is None:
            pytest.skip("Postgres backend skipped: no database reachable")
        from flywheel_orchestrator import PostgresClaimStore

        instance = PostgresClaimStore(
            postgres_dsn,
            schema=f"flywheel_claims_test_{uuid4().hex[:12]}",
            pool_min=1,
            pool_max=4,
        )
    try:
        yield instance
    finally:
        close = getattr(instance, "close", None)
        if callable(close):
            close()


def test_list_claims_returns_every_held_claim_with_its_owning_worker(
    store: object,
) -> None:
    assert isinstance(store, ClaimStore)

    # Acquire three distinct (task_id, worker_id) pairs. Each task_id is
    # independent (at most one live claim per task_id), so these are three
    # concurrently-held claims owned by three distinct workers. lease_seconds=30
    # against now=_t(0) keeps every lease live at enumeration time _t(0).
    expected_pairs = {
        ("task-a", "worker-1"),
        ("task-b", "worker-2"),
        ("task-c", "worker-3"),
    }
    for task_id, worker_id in expected_pairs:
        acquired = store.acquire_claim(
            task_id, worker_id, now=_t(0), lease_seconds=30
        )
        # Distinct task_ids are always free here, so each acquire must succeed.
        assert acquired is not None
        assert isinstance(acquired, TaskClaim)

    listed = store.list_claims()

    # One TaskClaim per held claim -- a real list of the dataclass, not a count.
    assert isinstance(listed, list)
    for entry in listed:
        assert isinstance(entry, TaskClaim)

    # The discriminator: set-equality over the (task_id, worker_id) PAIRS, so
    # each claim must carry its own worker, not just a bag of task_ids and not
    # just one row. Order-independent and count-independent (the set collapses
    # any accidental duplicates -- but pairing a task to the wrong worker, or
    # dropping a row, or blanking the worker, all break equality).
    listed_pairs = {(c.task_id, c.worker_id) for c in listed}
    assert listed_pairs == expected_pairs
