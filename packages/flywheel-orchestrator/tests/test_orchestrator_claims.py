"""Contract tests for the orchestrator's per-task lease store.

Parametrized over every :class:`flywheel_orchestrator.ClaimStore` backend so
in-memory, SQLite, and Postgres claim stores prove identical lease semantics
(acquire / steal-on-expiry / renew / release). The Postgres backend skips when
no database is reachable, reusing the same container helper as the core store
contract suite.
"""

from __future__ import annotations

from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

import pytest

from flywheel_orchestrator import (
    ClaimLostError,
    ClaimStore,
    InMemoryClaimStore,
    SqliteClaimStore,
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


def test_store_satisfies_claim_store_protocol(store: object) -> None:
    assert isinstance(store, ClaimStore)


def test_acquire_claim_on_free_task_succeeds(store: object) -> None:
    assert isinstance(store, ClaimStore)
    claim = store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    assert claim.task_id == "task-a"
    assert claim.worker_id == "worker-1"
    assert claim.version == 1
    assert claim.lease_expires_at == _t(30)
    loaded = store.load_claim("task-a")
    assert loaded is not None and loaded.worker_id == "worker-1"


def test_acquire_claim_held_by_live_other_worker_returns_none(
    store: object,
) -> None:
    assert isinstance(store, ClaimStore)
    first = store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert first is not None
    # Still well within the lease window: a different worker cannot claim.
    second = store.acquire_claim(
        "task-a", "worker-2", now=_t(10), lease_seconds=30
    )
    assert second is None
    # The original claim is untouched.
    loaded = store.load_claim("task-a")
    assert loaded is not None and loaded.worker_id == "worker-1"


def test_acquire_claim_reacquires_own_live_claim(store: object) -> None:
    assert isinstance(store, ClaimStore)
    first = store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert first is not None
    again = store.acquire_claim(
        "task-a", "worker-1", now=_t(5), lease_seconds=30
    )
    assert again is not None
    assert again.worker_id == "worker-1"
    assert again.version == 2
    assert again.lease_expires_at == _t(35)


def test_acquire_claim_steals_expired_lease(store: object) -> None:
    assert isinstance(store, ClaimStore)
    first = store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert first is not None
    # now is past the lease end -> a different worker reclaims it.
    stolen = store.acquire_claim(
        "task-a", "worker-2", now=_t(31), lease_seconds=30
    )
    assert stolen is not None
    assert stolen.worker_id == "worker-2"
    assert stolen.version == 2


def test_renew_extends_lease_and_bumps_version(store: object) -> None:
    assert isinstance(store, ClaimStore)
    claim = store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    renewed = store.renew_claim(claim, now=_t(20), lease_seconds=30)
    assert renewed.version == 2
    assert renewed.lease_expires_at == _t(50)
    # A different worker still cannot claim while the renewed lease is live.
    assert (
        store.acquire_claim(
            "task-a", "worker-2", now=_t(40), lease_seconds=30
        )
        is None
    )


def test_renew_after_steal_raises_claim_lost(store: object) -> None:
    assert isinstance(store, ClaimStore)
    claim = store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    # worker-2 steals after expiry, bumping the version.
    stolen = store.acquire_claim(
        "task-a", "worker-2", now=_t(31), lease_seconds=30
    )
    assert stolen is not None
    # worker-1's stale token no longer matches -> it learns it lost.
    with pytest.raises(ClaimLostError) as exc_info:
        store.renew_claim(claim, now=_t(35), lease_seconds=30)
    assert exc_info.value.task_id == "task-a"


def test_release_frees_the_task_for_another_worker(store: object) -> None:
    assert isinstance(store, ClaimStore)
    claim = store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    store.release_claim(claim)
    assert store.load_claim("task-a") is None
    # Even within the original lease window, the task is now free.
    reclaimed = store.acquire_claim(
        "task-a", "worker-2", now=_t(5), lease_seconds=30
    )
    assert reclaimed is not None
    assert reclaimed.worker_id == "worker-2"


def test_release_with_stale_token_is_noop(store: object) -> None:
    assert isinstance(store, ClaimStore)
    claim = store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    stolen = store.acquire_claim(
        "task-a", "worker-2", now=_t(31), lease_seconds=30
    )
    assert stolen is not None
    # worker-1 releasing its stale token must not drop worker-2's claim.
    store.release_claim(claim)
    loaded = store.load_claim("task-a")
    assert loaded is not None and loaded.worker_id == "worker-2"


def test_claims_are_independent_per_task(store: object) -> None:
    assert isinstance(store, ClaimStore)
    a = store.acquire_claim("task-a", "worker-1", now=_t(0), lease_seconds=30)
    b = store.acquire_claim("task-b", "worker-2", now=_t(0), lease_seconds=30)
    assert a is not None and b is not None
    assert store.load_claim("task-a").worker_id == "worker-1"  # type: ignore[union-attr]
    assert store.load_claim("task-b").worker_id == "worker-2"  # type: ignore[union-attr]


def test_load_missing_claim_returns_none(store: object) -> None:
    assert isinstance(store, ClaimStore)
    assert store.load_claim("nope") is None


def test_list_claims_empty_store_is_empty_list(store: object) -> None:
    assert isinstance(store, ClaimStore)
    assert store.list_claims() == []


def test_list_claims_enumerates_held_and_drops_released(
    store: object,
) -> None:
    """list_claims returns one TaskClaim per currently-held claim, each
    carrying its owning worker; a released claim is absent and its still-
    held sibling remains."""
    assert isinstance(store, ClaimStore)
    a = store.acquire_claim("task-a", "worker-1", now=_t(0), lease_seconds=30)
    b = store.acquire_claim("task-b", "worker-2", now=_t(0), lease_seconds=30)
    assert a is not None and b is not None
    held = {(c.task_id, c.worker_id) for c in store.list_claims()}
    assert held == {("task-a", "worker-1"), ("task-b", "worker-2")}
    # Release one: only the still-held claim's pair survives; the freed
    # worker is no longer reported.
    store.release_claim(a)
    remaining = {(c.task_id, c.worker_id) for c in store.list_claims()}
    assert remaining == {("task-b", "worker-2")}
