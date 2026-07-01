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
from typing import TYPE_CHECKING
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
    GraphSnapshotItem,
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


# --- stop-event ledger (schema v6) ------------------------------------------


def test_record_stop_event_appends_to_global_and_subject_streams(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    event_store.record_stop_event(
        kind=STOP_DANGLING_PREREQUISITE,
        subject="task-a",
        detail="prerequisite 'missing' resolves to no work item",
        occurred_at=_t(0),
    )
    event_store.record_stop_event(
        kind=STOP_NO_OP_CYCLE,
        subject="queue-dir",
        detail="idle (observed queue depth 0, target 5)",
        occurred_at=_t(1),
    )
    rows = event_store.list_stop_events()
    assert [(r.kind, r.subject) for r in rows] == [
        (STOP_DANGLING_PREREQUISITE, "task-a"),
        (STOP_NO_OP_CYCLE, "queue-dir"),
    ]
    # Strictly increasing insertion ids, id-ordered.
    assert [r.id for r in rows] == sorted(r.id for r in rows)
    # Per-subject timeline filters to one subject.
    only_a = event_store.list_subject_stop_events("task-a")
    assert [r.kind for r in only_a] == [STOP_DANGLING_PREREQUISITE]
    assert only_a[0].detail.startswith("prerequisite 'missing'")
    assert only_a[0].occurred_at == _t(0)


def test_stop_events_are_never_deduped(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # An identical stop recorded on three passes is three rows -- recurrence is
    # the signal, so a ledger that collapses them must fail.
    for _ in range(3):
        event_store.record_stop_event(
            kind=STOP_NO_OP_CYCLE,
            subject="queue-dir",
            detail="idle (observed queue depth 0, target 5)",
            occurred_at=_t(0),
        )
    rows = event_store.list_subject_stop_events("queue-dir")
    assert len(rows) == 3
    assert len({r.id for r in rows}) == 3  # distinct, monotonic ids


def test_record_prepare_skip_releases_claim_and_records_stop(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    claim = event_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    event_store.record_prepare_skip(
        claim, detail="RuntimeError: sandbox boom", now=_t(5)
    )
    # Same call released the claim (a ``released`` event) AND wrote the stop.
    assert event_store.load_claim("task-a") is None
    assert _types(event_store.list_task_events("task-a")) == [
        EVENT_ACQUIRED,
        EVENT_RELEASED,
    ]
    stops = event_store.list_subject_stop_events("task-a")
    assert [r.kind for r in stops] == [STOP_PREPARE_SKIP]
    assert stops[0].detail == "RuntimeError: sandbox boom"
    assert stops[0].occurred_at == _t(5)


def test_recurring_prepare_skip_records_one_row_per_occurrence(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # A prepare-preflight failure recurring across three cycles is three stop
    # rows (the claim is re-acquired each cycle), never deduped.
    for cycle in range(3):
        claim = event_store.acquire_claim(
            "task-a", "worker-1", now=_t(cycle * 10), lease_seconds=5
        )
        assert claim is not None
        event_store.record_prepare_skip(
            claim, detail=f"cycle {cycle}", now=_t(cycle * 10 + 1)
        )
    stops = event_store.list_subject_stop_events("task-a")
    assert [r.detail for r in stops] == ["cycle 0", "cycle 1", "cycle 2"]


def test_stop_ledger_exposes_no_mutating_surface(
    event_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    for forbidden in (
        "update_stop_event",
        "delete_stop_event",
        "edit_stop_event",
        "remove_stop_event",
        "clear_stop_events",
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


# -- WorkGraph snapshots (spec 00055) -------------------------------------
#
# The snapshot record lives on the in-memory and SQLite backends this slice
# (Layer A); Postgres is a dependent task, so these cases parametrize over
# memory + sqlite only, mirroring the lease/event cases above.
_SNAPSHOT_BACKENDS = ("memory", "sqlite")


@pytest.fixture(params=_SNAPSHOT_BACKENDS, ids=_SNAPSHOT_BACKENDS)
def snapshot_store(
    request: pytest.FixtureRequest,
    tmp_path: Path,
) -> Iterator[InMemoryClaimStore | SqliteClaimStore]:
    if request.param == "memory":
        instance: InMemoryClaimStore | SqliteClaimStore = InMemoryClaimStore()
    else:
        instance = SqliteClaimStore(tmp_path / "snapshots.db")
    try:
        yield instance
    finally:
        instance.close()


def test_snapshot_round_trips_full_per_item_state(
    snapshot_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #1: every field of every item round-trips -- mixed provenance,
    # priorities, one ready and one blocked item, a held and an unheld item,
    # and an item with a non-empty resolved-prerequisites set.
    item_a = GraphSnapshotItem(
        task_id="task-a",
        source_kind="directory",
        source_ref="/tasks/a.json",
        source_url="/tasks/a.json",
        source_version="hash-a",
        priority=5,
        required_capabilities=frozenset({"gpu", "linux"}),
        conflict_keys=frozenset({"resource-x"}),
        state="pending",
        ready=True,
        claim_holder="worker-1",
        resolved_prerequisites=frozenset(),
    )
    item_b = GraphSnapshotItem(
        task_id="task-b",
        source_kind="github_issue",
        source_ref="owner/repo#7",
        source_url="https://example/7",
        source_version="hash-b",
        priority=0,
        required_capabilities=frozenset(),
        conflict_keys=frozenset(),
        state="blocked",
        ready=False,
        claim_holder=None,
        resolved_prerequisites=frozenset({"task-a", "task-z"}),
    )
    record = snapshot_store.record_graph_snapshot(
        [item_a, item_b], captured_at=_t(0)
    )
    assert record.item_count == 2
    items = {
        i.task_id: i
        for i in snapshot_store.list_graph_snapshot_items(record.id)
    }
    assert items["task-a"] == item_a
    assert items["task-b"] == item_b


def test_snapshot_readiness_holder_state_match_passed_values(
    snapshot_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #2: the recorded ready flag, holder, and state equal the
    # per-item values passed -- a constant (all-ready/all-unheld/fixed-state)
    # that misrepresents the graph must fail.
    ready_held = GraphSnapshotItem(
        task_id="x",
        state="running",
        ready=True,
        claim_holder="worker-A",
    )
    blocked_unheld = GraphSnapshotItem(
        task_id="y",
        state="pending",
        ready=False,
        claim_holder=None,
    )
    record = snapshot_store.record_graph_snapshot(
        [ready_held, blocked_unheld], captured_at=_t(1)
    )
    items = {
        i.task_id: i
        for i in snapshot_store.list_graph_snapshot_items(record.id)
    }
    assert items["x"].ready is True
    assert items["x"].claim_holder == "worker-A"
    assert items["x"].state == "running"
    assert items["y"].ready is False
    assert items["y"].claim_holder is None
    assert items["y"].state == "pending"
    # The two items disagree on every captured dimension, so a constant would
    # be wrong for at least one of them.
    assert items["x"].ready != items["y"].ready
    assert items["x"].claim_holder != items["y"].claim_holder
    assert items["x"].state != items["y"].state


def test_snapshot_item_count_equals_rows_and_input_size(
    snapshot_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #3: the header's declared item count equals the number of item
    # rows read back and the size of the input graph -- across sizes including
    # an empty graph.
    sizes = [0, 1, 3]
    records = []
    for n, size in enumerate(sizes):
        items = [
            GraphSnapshotItem(task_id=f"t{n}-{i}", state="pending", ready=False)
            for i in range(size)
        ]
        records.append(snapshot_store.record_graph_snapshot(items, captured_at=_t(n)))
    for size, record in zip(sizes, records):
        rows = snapshot_store.list_graph_snapshot_items(record.id)
        assert record.item_count == size
        assert len(rows) == size


def test_snapshot_cursor_tracks_event_high_water_mark(
    snapshot_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #4: cursor is 0 on a fresh ledger, equals the latest event id
    # after transitions, and advances strictly as more events land.
    first = snapshot_store.record_graph_snapshot([], captured_at=_t(0))
    assert first.last_event_id == 0
    claim = snapshot_store.acquire_claim(
        "task-a", "worker-1", now=_t(1), lease_seconds=30
    )
    assert claim is not None
    claim = snapshot_store.renew_claim(claim, now=_t(5), lease_seconds=30)
    latest_event_id = snapshot_store.list_events()[-1].id
    second = snapshot_store.record_graph_snapshot([], captured_at=_t(6))
    assert second.last_event_id == latest_event_id
    assert second.last_event_id > first.last_event_id
    snapshot_store.release_claim(claim, now=_t(7))
    third = snapshot_store.record_graph_snapshot([], captured_at=_t(8))
    assert third.last_event_id == snapshot_store.list_events()[-1].id
    assert third.last_event_id > second.last_event_id


def test_snapshot_record_is_append_only(
    snapshot_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #5: successive records yield distinct, accumulating snapshots
    # in insertion order; re-recording never replaces an earlier one; the store
    # exposes no snapshot-updating or -deleting method.
    r1 = snapshot_store.record_graph_snapshot(
        [GraphSnapshotItem(task_id="a", state="pending", ready=False)],
        captured_at=_t(0),
    )
    r2 = snapshot_store.record_graph_snapshot([], captured_at=_t(1))
    r3 = snapshot_store.record_graph_snapshot(
        [GraphSnapshotItem(task_id="b", state="done", ready=True)],
        captured_at=_t(2),
    )
    stream = snapshot_store.list_graph_snapshots()
    assert [s.id for s in stream] == [r1.id, r2.id, r3.id]
    assert len(set(s.id for s in stream)) == 3
    assert stream[0].id < stream[1].id < stream[2].id
    # The first snapshot's single item row is unchanged by later records.
    first_items = snapshot_store.list_graph_snapshot_items(r1.id)
    assert [i.task_id for i in first_items] == ["a"]
    for forbidden in (
        "update_graph_snapshot",
        "delete_graph_snapshot",
        "edit_graph_snapshot",
        "remove_graph_snapshot",
        "clear_graph_snapshots",
    ):
        assert not hasattr(snapshot_store, forbidden)


def test_snapshot_read_api_stream_items_latest(
    snapshot_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #6: empty store -> empty stream + null latest; after records,
    # the stream returns headers in id order, an item accessor returns one
    # snapshot's rows, and latest returns the most recent.
    assert snapshot_store.list_graph_snapshots() == []
    assert snapshot_store.latest_graph_snapshot() is None
    r1 = snapshot_store.record_graph_snapshot(
        [GraphSnapshotItem(task_id="a", state="pending", ready=False)],
        captured_at=_t(0),
    )
    r2 = snapshot_store.record_graph_snapshot(
        [GraphSnapshotItem(task_id="b", state="running", ready=True)],
        captured_at=_t(1),
    )
    assert [s.id for s in snapshot_store.list_graph_snapshots()] == [r1.id, r2.id]
    assert [
        i.task_id for i in snapshot_store.list_graph_snapshot_items(r1.id)
    ] == ["a"]
    latest = snapshot_store.latest_graph_snapshot()
    assert latest is not None and latest.id == r2.id
    assert latest.item_count == 1


def test_snapshot_of_empty_graph_is_valid(
    snapshot_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Criterion #11: an empty graph still records a valid snapshot -- one stream
    # entry, item count 0, empty item rows, the current cursor.
    claim = snapshot_store.acquire_claim(
        "task-a", "worker-1", now=_t(0), lease_seconds=30
    )
    assert claim is not None
    record = snapshot_store.record_graph_snapshot([], captured_at=_t(1))
    stream = snapshot_store.list_graph_snapshots()
    assert len(stream) == 1
    assert record.item_count == 0
    assert snapshot_store.list_graph_snapshot_items(record.id) == []
    assert record.last_event_id == snapshot_store.list_events()[-1].id


def test_snapshot_prerequisites_set_round_trips_exactly(
    snapshot_store: InMemoryClaimStore | SqliteClaimStore,
) -> None:
    # Edge case: a non-empty resolved-prerequisites set round-trips exactly,
    # mirroring the conflict_keys_json encode/decode parity.
    prereqs = frozenset({"p-3", "p-1", "p-2"})
    item = GraphSnapshotItem(
        task_id="t",
        state="blocked",
        ready=False,
        resolved_prerequisites=prereqs,
    )
    record = snapshot_store.record_graph_snapshot([item], captured_at=_t(0))
    (read_back,) = snapshot_store.list_graph_snapshot_items(record.id)
    assert read_back.resolved_prerequisites == prereqs


def _build_v4_sqlite_store(path: Path) -> None:
    """Materialize a schema-v4 orchestrator store (pre-snapshot) with rows.

    Mirrors the v4 DDL (task_claims + conflict_keys_json, work_items,
    source_syncs, orchestrator_events, the sentinel) so the v5 build's additive
    migration can be exercised against a real pre-existing store carrying
    claim, work-item, source-sync, and event rows.
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
            CREATE TABLE source_syncs (
              id             INTEGER PRIMARY KEY AUTOINCREMENT,
              source_kind    TEXT NOT NULL,
              source_name    TEXT NOT NULL,
              started_at     TEXT NOT NULL,
              finished_at    TEXT,
              status         TEXT NOT NULL,
              observed_count INTEGER NOT NULL DEFAULT 0,
              error          TEXT,
              metadata_json  TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE orchestrator_events (
              id               INTEGER PRIMARY KEY AUTOINCREMENT,
              task_id          TEXT NOT NULL,
              worker_id        TEXT NOT NULL,
              event_type       TEXT NOT NULL,
              version          INTEGER NOT NULL,
              lease_expires_at TEXT NOT NULL,
              occurred_at      TEXT NOT NULL
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
            "INSERT INTO source_syncs (source_kind, source_name, started_at, "
            "status, observed_count) VALUES ('directory', '/tasks', ?, 'ok', 1)",
            (_t(0).isoformat(),),
        )
        conn.execute(
            "INSERT INTO orchestrator_events (task_id, worker_id, event_type, "
            "version, lease_expires_at, occurred_at) "
            "VALUES ('task-a', 'worker-1', 'acquired', 1, ?, ?)",
            (_t(30).isoformat(), _t(0).isoformat()),
        )
        conn.execute(
            "INSERT INTO orchestrator_schema_version (id, version) "
            "VALUES (1, 4)"
        )
    finally:
        conn.close()


def test_v4_store_opens_under_v5_with_rows_and_empty_snapshots(
    tmp_path: Path,
) -> None:
    # Criterion #10: a pre-existing v4 store opens under the v5 build without
    # error, keeps its task_claims/work_items/source_syncs/orchestrator_events
    # rows, and exposes an empty snapshot stream -- the additive,
    # non-destructive bump.
    path = tmp_path / "v4.db"
    _build_v4_sqlite_store(path)
    store = SqliteClaimStore(path)
    try:
        claim = store.load_claim("task-a")
        assert claim is not None and claim.worker_id == "worker-1"
        assert store.load_work_item("task-a") is not None
        assert len(store.list_source_syncs()) == 1
        assert len(store.list_events()) == 1
        assert store.list_graph_snapshots() == []
        assert store.latest_graph_snapshot() is None
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


def test_sqlite_postgres_stop_event_ledger_parity(
    tmp_path: Path,
    postgres_dsn: str | None,
) -> None:
    # The stop-event ledger reads back identically across the durable backends:
    # same kind spellings, subjects, details, and insertion order (ids dropped).
    if postgres_dsn is None:
        pytest.skip("Postgres backend skipped: no database reachable")
    from flywheel_orchestrator import PostgresClaimStore

    sqlite_store = SqliteClaimStore(tmp_path / "stop_parity.db")
    pg_store = PostgresClaimStore(
        postgres_dsn,
        schema=f"flywheel_claims_test_{uuid4().hex[:12]}",
        pool_min=1,
        pool_max=4,
    )

    def _drive_stops(store: SqliteClaimStore | PostgresClaimStore) -> None:
        store.record_stop_event(
            kind=STOP_DANGLING_PREREQUISITE,
            subject="task-a",
            detail="prerequisite 'missing' resolves to no work item",
            occurred_at=_t(0),
        )
        claim = store.acquire_claim(
            "task-a", "worker-1", now=_t(1), lease_seconds=30
        )
        assert claim is not None
        store.record_prepare_skip(claim, detail="boom", now=_t(2))
        # Recurrence is preserved: the same no-op stop twice is two rows.
        for _ in range(2):
            store.record_stop_event(
                kind=STOP_NO_OP_CYCLE,
                subject="queue-dir",
                detail="idle (observed queue depth 0, target 5)",
                occurred_at=_t(3),
            )

    def _stop_tuples(rows: list) -> list:
        return [(r.kind, r.subject, r.detail, r.occurred_at) for r in rows]

    try:
        _drive_stops(sqlite_store)
        _drive_stops(pg_store)
        assert _stop_tuples(sqlite_store.list_stop_events()) == _stop_tuples(
            pg_store.list_stop_events()
        )
        for subject in ("task-a", "queue-dir"):
            assert _stop_tuples(
                sqlite_store.list_subject_stop_events(subject)
            ) == _stop_tuples(pg_store.list_subject_stop_events(subject))
        # queue-dir carries two identical rows: the ledger never dedupes.
        assert len(pg_store.list_subject_stop_events("queue-dir")) == 2
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


# -- cross-backend snapshot parity + Postgres migration (spec 00055) --------


def _drive_snapshot_sequence(
    store: SqliteClaimStore | PostgresClaimStore,
) -> None:
    """Record one identical snapshot sequence against any durable backend.

    Exercises every dimension the parity holdout must cover: a snapshot taken
    at cursor 0 (empty ledger), claim events that advance the
    ``orchestrator_events`` high-water mark, a snapshot past that mark holding a
    mixed-provenance ready+held item and a blocked unheld item with a non-empty
    resolved-prerequisites set, and an empty-graph snapshot.
    """
    # Snapshot #1: cursor 0, before any claim event lands.
    store.record_graph_snapshot(
        [GraphSnapshotItem(task_id="seed", state="pending", ready=False)],
        captured_at=_t(0),
    )
    # Append claim events so the ledger high-water mark advances past 0.
    a = store.acquire_claim("task-a", "worker-1", now=_t(1), lease_seconds=30)
    assert a is not None
    a = store.renew_claim(a, now=_t(5), lease_seconds=30)
    b = store.acquire_claim("task-b", "worker-3", now=_t(2), lease_seconds=30)
    assert b is not None
    # Snapshot #2: past the high-water mark, mixed provenance + ready/blocked.
    item_a = GraphSnapshotItem(
        task_id="task-a",
        source_kind="directory",
        source_ref="/tasks/a.json",
        source_url="/tasks/a.json",
        source_version="hash-a",
        priority=5,
        required_capabilities=frozenset({"gpu", "linux"}),
        conflict_keys=frozenset({"resource-x"}),
        state="running",
        ready=True,
        claim_holder="worker-1",
        resolved_prerequisites=frozenset(),
    )
    item_b = GraphSnapshotItem(
        task_id="task-b",
        source_kind="github_issue",
        source_ref="owner/repo#7",
        source_url="https://example/7",
        source_version="hash-b",
        priority=0,
        required_capabilities=frozenset(),
        conflict_keys=frozenset(),
        state="blocked",
        ready=False,
        claim_holder=None,
        resolved_prerequisites=frozenset({"task-a", "task-z"}),
    )
    store.record_graph_snapshot([item_a, item_b], captured_at=_t(6))
    # Snapshot #3: an empty graph still records a valid snapshot.
    store.record_graph_snapshot([], captured_at=_t(8))


def _snapshot_header_tuples(records: list) -> list[tuple]:
    # Drop the backend-assigned snapshot id; compare recorded content + order.
    return [(r.captured_at, r.item_count, r.last_event_id) for r in records]


def test_sqlite_postgres_graph_snapshot_parity(
    tmp_path: Path,
    postgres_dsn: str | None,
) -> None:
    # Criterion #9: the same record_graph_snapshot input sequence yields equal
    # snapshot streams and item rows across backends -- same item field
    # spellings, states, holders, captured-at, cursors, and insertion order --
    # after dropping the backend-assigned snapshot ids. This is the composition
    # holdout over the shared snapshot-record invariant.
    if postgres_dsn is None:
        pytest.skip("Postgres backend skipped: no database reachable")
    from flywheel_orchestrator import PostgresClaimStore

    sqlite_store = SqliteClaimStore(tmp_path / "snap_parity.db")
    pg_store = PostgresClaimStore(
        postgres_dsn,
        schema=f"flywheel_claims_test_{uuid4().hex[:12]}",
        pool_min=1,
        pool_max=4,
    )
    try:
        _drive_snapshot_sequence(sqlite_store)
        _drive_snapshot_sequence(pg_store)
        sqlite_stream = sqlite_store.list_graph_snapshots()
        pg_stream = pg_store.list_graph_snapshots()
        # Header streams equal modulo the backend-assigned ids.
        assert _snapshot_header_tuples(
            sqlite_stream
        ) == _snapshot_header_tuples(pg_stream)
        # Cursor coverage: the first snapshot is at cursor 0 and a later one is
        # stamped at the live ledger high-water mark -- a missing/zero cursor on
        # one backend must fail.
        cursors = [r.last_event_id for r in sqlite_stream]
        assert cursors[0] == 0
        assert max(cursors) == sqlite_store.list_events()[-1].id
        assert max(cursors) == pg_store.list_events()[-1].id
        # Item rows equal per snapshot (read-back GraphSnapshotItems are id-free
        # so compare directly), in the same task_id order.
        for s_rec, p_rec in zip(sqlite_stream, pg_stream):
            assert sqlite_store.list_graph_snapshot_items(
                s_rec.id
            ) == pg_store.list_graph_snapshot_items(p_rec.id)
        # Latest equals modulo id.
        s_latest = sqlite_store.latest_graph_snapshot()
        p_latest = pg_store.latest_graph_snapshot()
        assert s_latest is not None and p_latest is not None
        assert (
            s_latest.captured_at,
            s_latest.item_count,
            s_latest.last_event_id,
        ) == (
            p_latest.captured_at,
            p_latest.item_count,
            p_latest.last_event_id,
        )
    finally:
        sqlite_store.close()
        pg_store.close()


def _build_v4_postgres_store(dsn: str, schema: str) -> None:
    """Materialize a schema-v4 Postgres orchestrator store (pre-snapshot).

    Mirrors the v4 DDL (task_claims + conflict_keys_json, the sentinel,
    work_items, source_syncs, orchestrator_events) in a fresh schema and seeds
    claim, work-item, source-sync, and event rows, so the v5 build's additive
    migration can be exercised against a real pre-existing Postgres store -- no
    graph_snapshots / graph_snapshot_items tables present.
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
            """
            CREATE TABLE source_syncs (
              id             BIGSERIAL PRIMARY KEY,
              source_kind    TEXT NOT NULL,
              source_name    TEXT NOT NULL,
              started_at     TIMESTAMPTZ NOT NULL,
              finished_at    TIMESTAMPTZ,
              status         TEXT NOT NULL,
              observed_count INTEGER NOT NULL DEFAULT 0,
              error          TEXT,
              metadata_json  JSONB NOT NULL DEFAULT '{}'::jsonb
            )
            """
        )
        conn.execute(
            """
            CREATE TABLE orchestrator_events (
              id               BIGSERIAL PRIMARY KEY,
              task_id          TEXT NOT NULL,
              worker_id        TEXT NOT NULL,
              event_type       TEXT NOT NULL,
              version          INTEGER NOT NULL,
              lease_expires_at TIMESTAMPTZ NOT NULL,
              occurred_at      TIMESTAMPTZ NOT NULL
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
            "INSERT INTO source_syncs (source_kind, source_name, started_at, "
            "status, observed_count) VALUES ('directory', '/tasks', %s, "
            "'ok', 1)",
            (_t(0),),
        )
        conn.execute(
            "INSERT INTO orchestrator_events (task_id, worker_id, event_type, "
            "version, lease_expires_at, occurred_at) "
            "VALUES ('task-a', 'worker-1', 'acquired', 1, %s, %s)",
            (_t(30), _t(0)),
        )
        conn.execute(
            "INSERT INTO orchestrator_schema_version (id, version) "
            "VALUES (1, 4)"
        )
        conn.commit()
    finally:
        conn.close()


def test_v4_postgres_store_opens_under_v5_with_rows_and_empty_snapshots(
    postgres_dsn: str | None,
) -> None:
    # Criterion #10: a pre-existing v4 Postgres store opens under the v5 build
    # without OrchestratorSchemaError, keeps its task_claims / work_items /
    # source_syncs / orchestrator_events rows, and exposes an empty snapshot
    # stream -- the additive, non-destructive bump.
    if postgres_dsn is None:
        pytest.skip("Postgres backend skipped: no database reachable")
    from flywheel_orchestrator import PostgresClaimStore

    schema = f"flywheel_claims_test_{uuid4().hex[:12]}"
    _build_v4_postgres_store(postgres_dsn, schema)
    store = PostgresClaimStore(
        postgres_dsn, schema=schema, pool_min=1, pool_max=4
    )
    try:
        claim = store.load_claim("task-a")
        assert claim is not None and claim.worker_id == "worker-1"
        assert store.load_work_item("task-a") is not None
        assert len(store.list_source_syncs()) == 1
        assert len(store.list_events()) == 1
        assert store.list_graph_snapshots() == []
        assert store.latest_graph_snapshot() is None
    finally:
        store.close()
