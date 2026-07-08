"""First-open bootstrap must be concurrency-safe on both Postgres stores.

A fresh schema opened by many workers at once has to provision exactly once
without deadlocking. Both ``PostgresStore`` and ``PostgresClaimStore`` take a
transaction-scoped advisory lock as the first bootstrap statement so the
``CREATE SCHEMA`` / ``CREATE TABLE IF NOT EXISTS`` sequence is serialized.

This test proves the guarantee where it can only be observed server-side: the
database's own cumulative deadlock counter (``pg_stat_database.deadlocks``) must
not move across a burst of concurrent fresh-schema opens, and no opener may
raise. Asserting the counter -- not merely the absence of a raised
``DeadlockDetected`` -- defeats catch-and-retry masking, since the server bumps
the counter even when a client swallows the deadlock and retries.

Skips when no Postgres is reachable (Docker / testcontainers / the ``postgres``
extra unavailable).
"""

from __future__ import annotations

import threading
from collections.abc import Callable
from typing import Any
from uuid import uuid4


# At least eight simultaneous fresh-schema opens per store kind, per the task.
_CONCURRENT_OPENS = 8


def _deadlock_count(dsn: str) -> int:
    """Cumulative server-side deadlock counter for the connected database."""
    import psycopg

    with psycopg.connect(dsn) as conn:
        with conn.cursor() as cur:
            cur.execute(
                "SELECT deadlocks FROM pg_stat_database "
                "WHERE datname = current_database()"
            )
            row = cur.fetchone()
    assert row is not None
    return int(row[0])


def _hammer_opens(
    open_one: Callable[[], Any], count: int
) -> tuple[list[Any], list[BaseException]]:
    """Open ``count`` stores against one schema as simultaneously as a barrier
    allows, returning the opened stores and every exception raised."""
    barrier = threading.Barrier(count)
    guard = threading.Lock()
    stores: list[Any] = []
    errors: list[BaseException] = []

    def _worker() -> None:
        try:
            barrier.wait()
            store = open_one()
        except BaseException as exc:  # noqa: BLE001 - surfaced via ``errors``
            with guard:
                errors.append(exc)
            return
        with guard:
            stores.append(store)

    threads = [threading.Thread(target=_worker) for _ in range(count)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=60)
    still_running = [thread for thread in threads if thread.is_alive()]
    assert not still_running, (
        f"{len(still_running)} opener(s) still running after 60s -- "
        "bootstrap serialization is stuck"
    )
    return stores, errors


def _assert_no_deadlock_burst(
    dsn: str, open_one: Callable[[], Any]
) -> None:
    before = _deadlock_count(dsn)
    stores, errors = _hammer_opens(open_one, _CONCURRENT_OPENS)
    try:
        assert not errors, f"concurrent fresh-schema opens raised: {errors!r}"
        assert len(stores) == _CONCURRENT_OPENS, (
            f"expected {_CONCURRENT_OPENS} opened stores, got {len(stores)}"
        )
        after = _deadlock_count(dsn)
        assert after == before, (
            f"server deadlock counter moved {before} -> {after} across "
            f"{_CONCURRENT_OPENS} concurrent fresh-schema opens"
        )
    finally:
        for store in stores:
            store.close()


def test_postgres_run_store_concurrent_fresh_bootstrap(
    require_postgres: str,
) -> None:
    dsn = require_postgres
    from flywheel_core import PostgresStore

    schema = f"flywheel_boot_{uuid4().hex[:12]}"
    _assert_no_deadlock_burst(
        dsn,
        lambda: PostgresStore(dsn, schema=schema, pool_min=1, pool_max=2),
    )


def test_postgres_claim_store_concurrent_fresh_bootstrap(
    require_postgres: str,
) -> None:
    dsn = require_postgres
    from flywheel_orchestrator import PostgresClaimStore

    schema = f"flywheel_claims_boot_{uuid4().hex[:12]}"
    _assert_no_deadlock_burst(
        dsn,
        lambda: PostgresClaimStore(dsn, schema=schema, pool_min=1, pool_max=2),
    )
