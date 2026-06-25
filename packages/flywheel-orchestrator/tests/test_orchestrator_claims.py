"""Contract tests for the orchestrator's per-task lease store.

Parametrized over every :class:`flywheel_orchestrator.ClaimStore` backend so
in-memory, SQLite, and Postgres claim stores prove identical lease semantics
(acquire / steal-on-expiry / renew / release). The Postgres backend skips when
no database is reachable, reusing the same container helper as the core store
contract suite.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

import pytest

from flywheel_orchestrator import (
    EVENT_ACQUIRED,
    EVENT_EXPIRED,
    EVENT_RELEASED,
    EVENT_RENEWED,
    EVENT_STOLEN,
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


# -- orchestrator_events ledger (spec 00054) ------------------------------
#
# The ledger lives on the in-memory and SQLite backends this slice (Layer A);
# Postgres is a dependent task, so these cases parametrize over memory + sqlite
# only, mirroring the lease cases above.
_EVENT_BACKENDS = ("memory", "sqlite")


@pytest.fixture(params=_EVENT_BACKENDS, ids=_EVENT_BACKENDS)
def event_store(
    request: pytest.FixtureRequest,
    tmp_path: Path,
) -> Iterator[InMemoryClaimStore | SqliteClaimStore]:
    if request.param == "memory":
        instance: InMemoryClaimStore | SqliteClaimStore = InMemoryClaimStore()
    else:
        instance = SqliteClaimStore(tmp_path / "events.db")
    try:
        yield instance
    finally:
        instance.close()


def _types(events: list) -> list[str]:
    return [event.event_type for event in events]


def test_acquire_records_one_acquired_event(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #1: a fresh acquire writes exactly one ``acquired`` event
    # carrying the worker and the post-acquire version.
    claim = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    timeline = event_store.list_task_events("task-a")
    assert len(timeline) == 1
    event = timeline[0]
    assert event.event_type == EVENT_ACQUIRED
    assert event.worker_id == "worker-1"
    assert event.version == claim.version
    assert event.lease_expires_at == claim.lease_expires_at
    assert event.occurred_at == _t(0)


def test_refused_acquire_live_other_worker_records_no_event(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #2: a refused acquire (live lease held by another worker)
    # returns None and adds no event.
    first = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert first is not None
    refused = event_store.acquire_claim(
        "task-a", "worker-2", now=_t(10), lease_seconds=30
    )
    assert refused is None
    assert _types(event_store.list_task_events("task-a")) == [EVENT_ACQUIRED]


def test_refused_acquire_conflict_key_records_no_event(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #2: a conflict-key refusal also adds no event.
    held = event_store.acquire_claim(
        "task-a",
        "worker-1",
        now=_t(0),
        lease_seconds=30,
        conflict_keys=frozenset({"resource-x"}),
    )
    assert held is not None
    refused = event_store.acquire_claim(
        "task-b",
        "worker-2",
        now=_t(5),
        lease_seconds=30,
        conflict_keys=frozenset({"resource-x"}),
    )
    assert refused is None
    assert event_store.list_task_events("task-b") == []


def test_steal_records_distinct_stolen_event(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #3: stealing a different worker's lapsed lease writes a
    # ``stolen`` event distinct from ``acquired``, carrying the new holder.
    a = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert a is not None
    b = event_store.acquire_claim(
        "task-a", "worker-2", now=_t(31), lease_seconds=30
    )
    assert b is not None
    timeline = event_store.list_task_events("task-a")
    assert _types(timeline) == [EVENT_ACQUIRED, EVENT_STOLEN]
    steal = timeline[1]
    assert steal.event_type != EVENT_ACQUIRED
    assert steal.worker_id == "worker-2"
    assert steal.version == b.version


def test_same_worker_reacquire_records_acquired_not_stolen(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # D-2: a same-worker re-acquire is ``acquired``, never ``stolen``.
    first = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert first is not None
    again = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(5), lease_seconds=30
    )
    assert again is not None
    assert _types(event_store.list_task_events("task-a")) == [
        EVENT_ACQUIRED,
        EVENT_ACQUIRED,
    ]


def test_renew_records_renewed_event_with_version_and_expiry(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #4: a renew writes a ``renewed`` event carrying the post-renew
    # version and lease expiry.
    claim = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    renewed = event_store.renew_claim(claim, now=_t(20), lease_seconds=30)
    timeline = event_store.list_task_events("task-a")
    assert _types(timeline) == [EVENT_ACQUIRED, EVENT_RENEWED]
    event = timeline[1]
    assert event.version == renewed.version
    assert event.lease_expires_at == renewed.lease_expires_at


def test_failed_renew_records_no_event(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #5: a renew that raises ClaimLostError commits no state change
    # and writes no event.
    claim = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    stolen = event_store.acquire_claim(
        "task-a", "worker-2", now=_t(31), lease_seconds=30
    )
    assert stolen is not None
    with pytest.raises(ClaimLostError):
        event_store.renew_claim(claim, now=_t(35), lease_seconds=30)
    assert _types(event_store.list_task_events("task-a")) == [
        EVENT_ACQUIRED,
        EVENT_STOLEN,
    ]


def test_release_records_released_event(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #6: releasing the matching live claim writes a ``released``
    # event for that worker.
    claim = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    event_store.release_claim(claim, now=_t(10))
    timeline = event_store.list_task_events("task-a")
    assert _types(timeline) == [EVENT_ACQUIRED, EVENT_RELEASED]
    assert timeline[-1].worker_id == "worker-1"
    assert timeline[-1].occurred_at == _t(10)


def test_noop_release_records_no_event(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #7: a release with a stale/already-stolen token deletes no row
    # and writes no ``released`` event.
    claim = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    stolen = event_store.acquire_claim(
        "task-a", "worker-2", now=_t(31), lease_seconds=30
    )
    assert stolen is not None
    event_store.release_claim(claim, now=_t(35))
    assert _types(event_store.list_task_events("task-a")) == [
        EVENT_ACQUIRED,
        EVENT_STOLEN,
    ]


def test_sweep_records_one_expired_per_reaped_claim(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #8: a sweep writes exactly one ``expired`` event per reaped
    # claim (carrying its holder) and none for a still-valid claim.
    a = event_store.acquire_claim(
        "task-1", "worker-a", now=_t(0), lease_seconds=30
    )
    b = event_store.acquire_claim(
        "task-2", "worker-b", now=_t(0), lease_seconds=30
    )
    c = event_store.acquire_claim(
        "task-3", "worker-c", now=_t(0), lease_seconds=120
    )
    assert a is not None and b is not None and c is not None
    reaped = event_store.sweep_expired_claims(now=_t(45))
    assert set(reaped) == {"task-1", "task-2"}
    expired = [
        event
        for event in event_store.list_events()
        if event.event_type == EVENT_EXPIRED
    ]
    assert {(e.task_id, e.worker_id) for e in expired} == {
        ("task-1", "worker-a"),
        ("task-2", "worker-b"),
    }
    assert event_store.list_task_events("task-3") == [
        event
        for event in event_store.list_events()
        if event.task_id == "task-3"
    ]
    assert _types(event_store.list_task_events("task-3")) == [EVENT_ACQUIRED]


def test_ledger_is_append_only_in_id_order(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #9: a committed sequence on one task yields one event per
    # change in id order, never collapsed; re-acquiring after release appends.
    claim = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    claim = event_store.renew_claim(claim, now=_t(10), lease_seconds=30)
    claim = event_store.renew_claim(claim, now=_t(20), lease_seconds=30)
    event_store.release_claim(claim, now=_t(25))
    assert _types(event_store.list_task_events("task-a")) == [
        EVENT_ACQUIRED,
        EVENT_RENEWED,
        EVENT_RENEWED,
        EVENT_RELEASED,
    ]
    reacquired = event_store.acquire_claim(
        "task-a", "worker-2", now=_t(30), lease_seconds=30
    )
    assert reacquired is not None
    timeline = event_store.list_task_events("task-a")
    assert _types(timeline) == [
        EVENT_ACQUIRED,
        EVENT_RENEWED,
        EVENT_RENEWED,
        EVENT_RELEASED,
        EVENT_ACQUIRED,
    ]
    # Strictly increasing insertion ids, never deduped or overwritten.
    ids = [event.id for event in timeline]
    assert ids == sorted(ids)
    assert len(set(ids)) == len(ids)


def test_read_api_global_stream_and_per_task_timeline(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #12: the global stream returns every event in id order; the
    # per-task accessor returns only that task's events in id order; the store
    # exposes no event-mutating/deleting method.
    a = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    b = event_store.acquire_claim(
        "task-b", "worker-2", now=_t(1), lease_seconds=30
    )
    assert a is not None and b is not None
    event_store.renew_claim(a, now=_t(5), lease_seconds=30)
    global_stream = event_store.list_events()
    assert [(e.task_id, e.event_type) for e in global_stream] == [
        ("task-a", EVENT_ACQUIRED),
        ("task-b", EVENT_ACQUIRED),
        ("task-a", EVENT_RENEWED),
    ]
    assert [e.id for e in global_stream] == sorted(e.id for e in global_stream)
    assert _types(event_store.list_task_events("task-a")) == [
        EVENT_ACQUIRED,
        EVENT_RENEWED,
    ]
    # No event-mutating or event-deleting surface on the store.
    for forbidden in (
        "update_event",
        "delete_event",
        "edit_event",
        "remove_event",
        "clear_events",
    ):
        assert not hasattr(event_store, forbidden)


def _build_v3_sqlite_store(path: Path) -> None:
    """Materialize a schema-v3 orchestrator store (pre-ledger) with rows.

    Mirrors the v3 DDL (task_claims + conflict_keys_json, work_items, the
    sentinel) so the v4 build's additive migration can be exercised against a
    real pre-existing store carrying claim and work-item rows.
    """
    conn = sqlite3.connect(str(path), isolation_level=None)
    try:
        conn.executescript(
            """
            CREATE TABLE task_claims (
              task_id            TEXT PRIMARY KEY,
              worker_id          TEXT NOT NULL,
              claimed_at         DATETIME NOT NULL,
              lease_expires_at   DATETIME NOT NULL,
              version            INTEGER NOT NULL,
              conflict_keys_json TEXT NOT NULL DEFAULT '[]'
            );
            CREATE TABLE orchestrator_schema_version (
              id      INTEGER PRIMARY KEY CHECK (id = 1),
              version INTEGER NOT NULL
            );
            CREATE TABLE work_items (
              task_id                    TEXT PRIMARY KEY,
              source_kind                TEXT,
              source_ref                 TEXT,
              source_url                 TEXT,
              source_version             TEXT,
              task_content_hash          TEXT,
              priority                   INTEGER NOT NULL DEFAULT 0,
              required_capabilities_json TEXT NOT NULL DEFAULT '[]',
              conflict_keys_json         TEXT NOT NULL DEFAULT '[]',
              first_seen_at              TEXT NOT NULL,
              last_seen_at               TEXT NOT NULL,
              disappeared_at             TEXT,
              metadata_json              TEXT NOT NULL DEFAULT '{}'
            );
            """
        )
        conn.execute(
            "INSERT INTO task_claims (task_id, worker_id, claimed_at, "
            "lease_expires_at, version, conflict_keys_json) "
            "VALUES ('task-a', 'worker-1', ?, ?, 1, '[]')",
            (_t(0).isoformat(), _t(30).isoformat()),
        )
        conn.execute(
            "INSERT INTO work_items (task_id, first_seen_at, last_seen_at) "
            "VALUES ('task-a', ?, ?)",
            (_t(0).isoformat(), _t(0).isoformat()),
        )
        conn.execute(
            "INSERT INTO orchestrator_schema_version (id, version) "
            "VALUES (1, 3)"
        )
    finally:
        conn.close()


def test_v3_store_opens_under_v4_with_rows_and_empty_ledger(
    tmp_path: Path,
) -> None:
    # Criterion #11: a pre-existing v3 store opens under the v4 build without
    # error, keeps its task_claims and work_items rows, and exposes an empty
    # ledger -- the additive, non-destructive bump.
    path = tmp_path / "v3.db"
    _build_v3_sqlite_store(path)
    store = SqliteClaimStore(path)
    try:
        claim = store.load_claim("task-a")
        assert claim is not None and claim.worker_id == "worker-1"
        assert store.load_work_item("task-a") is not None
        assert store.list_events() == []
    finally:
        store.close()


# -- cross-backend parity + Postgres migration (spec 00054, #10/#11) -------


def _drive_lease_sequence(store: object) -> None:
    """Drive one identical acquire/renew/steal/sweep/release sequence.

    Touches every event type: ``acquired`` (task-a fresh, task-b fresh),
    ``renewed`` (task-a), ``stolen`` (task-a reclaimed after its lease lapses),
    ``expired`` (task-b reaped by the sweep), ``released`` (task-a dropped).
    """
    assert isinstance(store, ClaimStore)
    a = store.acquire_claim("task-a", "worker-1", now=_t(0), lease_seconds=20)
    assert a is not None
    a = store.renew_claim(a, now=_t(5), lease_seconds=20)
    b = store.acquire_claim("task-b", "worker-3", now=_t(2), lease_seconds=10)
    assert b is not None
    stolen = store.acquire_claim(
        "task-a", "worker-2", now=_t(26), lease_seconds=30
    )
    assert stolen is not None
    reaped = store.sweep_expired_claims(now=_t(30))
    assert reaped == ["task-b"]
    store.release_claim(stolen, now=_t(40))  # type: ignore[call-arg]


def _event_tuples(events: list) -> list[tuple]:
    # Drop the backend-assigned row id; compare the recorded content + order.
    return [
        (
            e.task_id,
            e.worker_id,
            e.event_type,
            e.version,
            e.lease_expires_at,
            e.occurred_at,
        )
        for e in events
    ]


def test_sqlite_postgres_event_ledger_parity(
    tmp_path: Path,
    postgres_dsn: str | None,
) -> None:
    # Criterion #10: the same transition sequence yields equal event lists
    # across backends -- same type spellings, worker ids, versions, lease /
    # occurred timestamps, and insertion order -- after dropping row ids.
    if postgres_dsn is None:
        pytest.skip("Postgres backend skipped: no database reachable")
    from flywheel_orchestrator import PostgresClaimStore

    sqlite_store = SqliteClaimStore(tmp_path / "parity.db")
    pg_store = PostgresClaimStore(
        postgres_dsn,
        schema=f"flywheel_claims_test_{uuid4().hex[:12]}",
        pool_min=1,
        pool_max=4,
    )
    try:
        _drive_lease_sequence(sqlite_store)
        _drive_lease_sequence(pg_store)
        sqlite_global = _event_tuples(sqlite_store.list_events())
        assert sqlite_global == _event_tuples(pg_store.list_events())
        # The five event types are all exercised, so the parity holds over the
        # whole taxonomy rather than a subset.
        assert {row[2] for row in sqlite_global} == {
            EVENT_ACQUIRED,
            EVENT_RENEWED,
            EVENT_STOLEN,
            EVENT_EXPIRED,
            EVENT_RELEASED,
        }
        for task_id in ("task-a", "task-b"):
            assert _event_tuples(
                sqlite_store.list_task_events(task_id)
            ) == _event_tuples(pg_store.list_task_events(task_id))
    finally:
        sqlite_store.close()
        pg_store.close()


def _build_v3_postgres_store(dsn: str, schema: str) -> None:
    """Materialize a schema-v3 Postgres orchestrator store (pre-ledger).

    Mirrors the v3 DDL (task_claims + conflict_keys_json, the sentinel,
    work_items) in a fresh schema and seeds claim + work-item rows, so the v4
    build's additive migration can be exercised against a real pre-existing
    Postgres store -- no orchestrator_events table present.
    """
    import psycopg
    from psycopg import sql

    conn = psycopg.connect(dsn)
    try:
        conn.execute(
            sql.SQL("CREATE SCHEMA IF NOT EXISTS {}").format(
                sql.Identifier(schema)
            )
        )
        conn.execute(
            sql.SQL("SET search_path TO {}, public").format(
                sql.Identifier(schema)
            )
        )
        conn.execute(
            """
            CREATE TABLE task_claims (
              task_id            TEXT PRIMARY KEY,
              worker_id          TEXT NOT NULL,
              claimed_at         TIMESTAMPTZ NOT NULL,
              lease_expires_at   TIMESTAMPTZ NOT NULL,
              version            INTEGER NOT NULL,
              conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb
            )
            """
        )
        conn.execute(
            """
            CREATE TABLE orchestrator_schema_version (
              id      INTEGER PRIMARY KEY CHECK (id = 1),
              version INTEGER NOT NULL
            )
            """
        )
        conn.execute(
            """
            CREATE TABLE work_items (
              task_id           TEXT PRIMARY KEY,
              source_kind       TEXT,
              source_ref        TEXT,
              source_url        TEXT,
              source_version    TEXT,
              task_content_hash TEXT,
              priority          INTEGER NOT NULL DEFAULT 0,
              required_capabilities_json JSONB NOT NULL DEFAULT '[]'::jsonb,
              conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb,
              first_seen_at     TIMESTAMPTZ NOT NULL,
              last_seen_at      TIMESTAMPTZ NOT NULL,
              disappeared_at    TIMESTAMPTZ,
              metadata_json     JSONB NOT NULL DEFAULT '{}'::jsonb
            )
            """
        )
        conn.execute(
            "INSERT INTO task_claims (task_id, worker_id, claimed_at, "
            "lease_expires_at, version, conflict_keys_json) "
            "VALUES ('task-a', 'worker-1', %s, %s, 1, '[]'::jsonb)",
            (_t(0), _t(30)),
        )
        conn.execute(
            "INSERT INTO work_items (task_id, first_seen_at, last_seen_at) "
            "VALUES ('task-a', %s, %s)",
            (_t(0), _t(0)),
        )
        conn.execute(
            "INSERT INTO orchestrator_schema_version (id, version) "
            "VALUES (1, 3)"
        )
        conn.commit()
    finally:
        conn.close()


def test_v3_postgres_store_opens_under_v4_with_rows_and_empty_ledger(
    postgres_dsn: str | None,
) -> None:
    # Criterion #11: a pre-existing v3 Postgres store opens under the v4 build
    # without OrchestratorSchemaError, keeps its task_claims and work_items
    # rows, and exposes an empty ledger -- the additive, non-destructive bump.
    if postgres_dsn is None:
        pytest.skip("Postgres backend skipped: no database reachable")
    from flywheel_orchestrator import PostgresClaimStore

    schema = f"flywheel_claims_test_{uuid4().hex[:12]}"
    _build_v3_postgres_store(postgres_dsn, schema)
    store = PostgresClaimStore(
        postgres_dsn, schema=schema, pool_min=1, pool_max=4
    )
    try:
        claim = store.load_claim("task-a")
        assert claim is not None and claim.worker_id == "worker-1"
        assert store.load_work_item("task-a") is not None
        assert store.list_events() == []
    finally:
        store.close()
