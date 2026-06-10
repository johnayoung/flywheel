"""Postgres-specific tests for ``flywheel.store_postgres.PostgresStore``.

The shared protocol contract lives in ``test_store_contract.py`` and
runs against every backend (including Postgres when Docker is up).
Tests here pin properties that only make sense for the Postgres
backend: the import-time guard for the ``flywheel[postgres]`` extra,
JSONB column typing, raw-SQL append-only enforcement, FK enforcement,
pool sizing, schema-name isolation across two stores against the same
DB, optimistic-concurrency conflicts via two pool connections, and
``close()`` releasing pool resources.

Every test that touches a real Postgres skips with a clear reason when
Docker is unavailable.
"""

from __future__ import annotations

import importlib
import sys
import threading
import time
from collections.abc import Iterator
from datetime import datetime, timezone
from typing import Any
from uuid import uuid4

import pytest

from flywheel import (
    CURRENT_SCHEMA_VERSION,
    Attempt,
    EventRecord,
    GraderResultRecord,
    Lifecycle,
    LifecycleInitialized,
    OptimisticConcurrencyError,
    Status,
    StoreSchemaError,
)


# ---------------------------------------------------------------------------
# Import-guard test runs without psycopg by monkeypatching sys.modules.
# Everything else is gated on Docker via the shared helper from the
# contract suite.
# ---------------------------------------------------------------------------


def test_import_guard_names_the_postgres_extra(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``flywheel.store_postgres`` must raise ``ImportError`` naming the
    ``postgres`` extra when ``psycopg`` is unavailable."""
    # Force ``import psycopg`` to fail; clear the cached store_postgres
    # module so the import-time guard re-runs.
    monkeypatch.setitem(sys.modules, "psycopg", None)
    monkeypatch.delitem(sys.modules, "flywheel.store_postgres", raising=False)
    with pytest.raises(ImportError) as exc_info:
        importlib.import_module("flywheel.store_postgres")
    msg = str(exc_info.value)
    assert "postgres" in msg
    assert "flywheel[postgres]" in msg


# ---------------------------------------------------------------------------
# Postgres-backed fixtures. The DSN is sourced from the shared container
# helper in test_store_contract; we deliberately reach into it so the
# same container backs every test in the test session.
# ---------------------------------------------------------------------------


from conftest import _get_postgres_dsn  # noqa: E402


def _dsn_or_skip() -> str:
    dsn = _get_postgres_dsn()
    if dsn is None:
        pytest.skip("Postgres tests skipped: Docker/testcontainers unavailable")
    return dsn


@pytest.fixture
def fresh_schema() -> str:
    """A unique, identifier-safe schema name per test."""
    return f"flywheel_test_{uuid4().hex[:12]}"


@pytest.fixture
def pg_store(fresh_schema: str) -> Iterator[Any]:
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    store = PostgresStore(dsn, schema=fresh_schema, pool_min=1, pool_max=4)
    try:
        yield store
    finally:
        store.close()


# ---------------------------------------------------------------------------
# Schema-name validation (no SQL injection through caller input).
# ---------------------------------------------------------------------------


def test_invalid_schema_name_is_rejected() -> None:
    """Schema names must pass identifier validation before any SQL runs.

    Skips when Docker is unavailable -- the validation runs in the
    constructor, but we want a real DSN so the construction sequence is
    exercised end-to-end."""
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    for bad in ("public; drop table x", "1invalid", "bad-name", ""):
        with pytest.raises(ValueError):
            PostgresStore(dsn, schema=bad)


# ---------------------------------------------------------------------------
# Bootstrap: column types, idempotency, trigger installation.
# ---------------------------------------------------------------------------


def _query(store: Any, sql_text: str, params: tuple[Any, ...] = ()) -> list[Any]:
    with store._pool.connection() as conn:
        with conn.cursor() as cur:
            cur.execute(sql_text, params)
            return cur.fetchall()


def test_jsonb_columns_use_jsonb_type(
    pg_store: Any, fresh_schema: str
) -> None:
    """JSONB columns must materialise as actual JSONB, not TEXT/JSON.

    Inspects ``information_schema.columns`` for every ``*_json`` column
    defined in ``docs/persistence-schema-postgres.sql``."""
    rows = _query(
        pg_store,
        """
        SELECT table_name, column_name, data_type
        FROM information_schema.columns
        WHERE table_schema = %s
          AND column_name LIKE %s
        ORDER BY table_name, column_name
        """,
        (fresh_schema, "%_json"),
    )
    by_pair = {(t, c): dt for (t, c, dt) in rows}
    expected = {
        ("attempts", "agent_context_json"),
        ("events", "payload_json"),
        ("grader_results", "grader_spec_json"),
        ("grader_results", "payload_json"),
        ("lifecycles", "timestamps_json"),
    }
    assert set(by_pair) >= expected
    for key in expected:
        assert by_pair[key] == "jsonb", (
            f"{key} is {by_pair[key]!r}, expected jsonb"
        )


def test_bootstrap_is_idempotent(fresh_schema: str) -> None:
    """A second construction against the same DB+schema must succeed."""
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    s1 = PostgresStore(dsn, schema=fresh_schema, pool_min=1, pool_max=2)
    try:
        # Insert a row to make sure no DDL re-run truncates anything.
        s1.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))
    finally:
        s1.close()

    s2 = PostgresStore(dsn, schema=fresh_schema, pool_min=1, pool_max=2)
    try:
        loaded = s2.load_lifecycle("r1")
        assert loaded is not None
        assert loaded.task_id == "t"
    finally:
        s2.close()


def test_grader_result_triggers_are_installed(
    pg_store: Any, fresh_schema: str
) -> None:
    rows = _query(
        pg_store,
        """
        SELECT tgname
        FROM pg_trigger
        WHERE tgrelid = (
            SELECT oid FROM pg_class
            WHERE relname = 'grader_results'
              AND relnamespace = (
                  SELECT oid FROM pg_namespace WHERE nspname = %s
              )
        )
          AND NOT tgisinternal
        ORDER BY tgname
        """,
        (fresh_schema,),
    )
    names = {r[0] for r in rows}
    assert {"grader_results_no_update", "grader_results_no_delete"} <= names


# ---------------------------------------------------------------------------
# Append-only enforcement at the raw-SQL level.
# ---------------------------------------------------------------------------


def _seed_grader_result(store: Any) -> None:
    store.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))
    store.save_attempt(
        "r1",
        Attempt(
            number=1,
            started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
            run_id="r1",
        ),
    )
    store.append_grader_result(
        GraderResultRecord(
            run_id="r1",
            attempt_number=1,
            ordinal=0,
            grader_type="command",
            grader_spec={"type": "command", "run": "true"},
            passed=True,
            duration_ms=1,
            payload={"exit_code": 0},
            ts=datetime(2024, 1, 1, tzinfo=timezone.utc),
        )
    )


def test_raw_update_on_grader_results_is_rejected(pg_store: Any) -> None:
    # The schema script tags the trigger's RAISE with
    # ``ERRCODE = 'check_violation'``, which psycopg maps to
    # ``psycopg.errors.CheckViolation``. We catch the broader
    # ``DatabaseError`` base so the test pins the behavioral contract
    # (the trigger fires and the message names append-only) without
    # over-fitting to one psycopg subclass.
    import psycopg

    _seed_grader_result(pg_store)
    with pytest.raises(psycopg.errors.DatabaseError) as exc:
        with pg_store._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute("UPDATE grader_results SET passed = false")
    assert "append-only" in str(exc.value)


def test_raw_delete_on_grader_results_is_rejected(pg_store: Any) -> None:
    import psycopg

    _seed_grader_result(pg_store)
    with pytest.raises(psycopg.errors.DatabaseError) as exc:
        with pg_store._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute("DELETE FROM grader_results")
    assert "append-only" in str(exc.value)
    # Row is still present after the rejected delete.
    assert len(pg_store.list_grader_results("r1", 1)) == 1


# ---------------------------------------------------------------------------
# Foreign-key enforcement.
# ---------------------------------------------------------------------------


def test_foreign_key_rejects_orphan_event(pg_store: Any) -> None:
    """Inserting an event for a missing lifecycle must raise FK violation."""
    import psycopg

    with pytest.raises(psycopg.errors.ForeignKeyViolation):
        with pg_store._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO events
                        (run_id, ts, kind, payload_json, sequence)
                    VALUES (%s, %s, %s, %s::jsonb, %s)
                    """,
                    ("nope", datetime.now(timezone.utc), "x", "{}", 1),
                )


def test_foreign_key_rejects_orphan_grader_result(pg_store: Any) -> None:
    import psycopg

    pg_store.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))
    with pytest.raises(psycopg.errors.ForeignKeyViolation):
        with pg_store._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO grader_results
                        (run_id, attempt_number, ordinal, grader_type,
                         grader_spec_json, passed, duration_ms,
                         payload_json, ts)
                    VALUES (%s, %s, %s, %s, %s::jsonb, %s, %s, %s::jsonb, %s)
                    """,
                    ("r1", 999, 0, "command", "{}", True, 1, "{}",
                     datetime.now(timezone.utc)),
                )


# ---------------------------------------------------------------------------
# Pool sizing + parallel reads.
# ---------------------------------------------------------------------------


def test_pool_supports_parallel_reads(fresh_schema: str) -> None:
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    store = PostgresStore(dsn, schema=fresh_schema, pool_min=2, pool_max=4)
    try:
        store.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))

        results: list[bool] = []
        errors: list[BaseException] = []

        def worker() -> None:
            try:
                lc = store.load_lifecycle("r1")
                results.append(lc is not None)
            except BaseException as exc:  # pragma: no cover - defensive
                errors.append(exc)

        threads = [threading.Thread(target=worker) for _ in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=10)

        assert not errors
        assert results == [True] * 8
        # The pool is at least pool_min connections; under load it
        # may grow up to pool_max.
        assert store._pool.max_size == 4
        assert store._pool.min_size == 2
    finally:
        store.close()


# ---------------------------------------------------------------------------
# Schema-name isolation: two stores against the same DB don't collide.
# ---------------------------------------------------------------------------


def test_schema_isolation_between_two_stores() -> None:
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    schema_a = f"flywheel_iso_a_{uuid4().hex[:8]}"
    schema_b = f"flywheel_iso_b_{uuid4().hex[:8]}"
    a = PostgresStore(dsn, schema=schema_a, pool_min=1, pool_max=2)
    b = PostgresStore(dsn, schema=schema_b, pool_min=1, pool_max=2)
    try:
        a.create_lifecycle(Lifecycle(task_id="a-task", run_id="r1"))
        b.create_lifecycle(Lifecycle(task_id="b-task", run_id="r1"))

        loaded_a = a.load_lifecycle("r1")
        loaded_b = b.load_lifecycle("r1")
        assert loaded_a is not None
        assert loaded_b is not None
        assert loaded_a.task_id == "a-task"
        assert loaded_b.task_id == "b-task"
    finally:
        b.close()
        a.close()


# ---------------------------------------------------------------------------
# Optimistic concurrency at the SQL level via two pool connections.
# ---------------------------------------------------------------------------


def test_optimistic_concurrency_conflict_via_two_pool_connections(
    pg_store: Any,
) -> None:
    pg_store.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))

    # Two snapshots at version=1; first writer succeeds, second must
    # see OptimisticConcurrencyError.
    snap1 = pg_store.load_lifecycle("r1")
    snap2 = pg_store.load_lifecycle("r1")
    assert snap1 is not None and snap2 is not None

    snap1.transition_to(Status.READY)
    pg_store.update_lifecycle(snap1, expected_version=1)

    # snap2 still thinks version == 1. Make a fresh transition so the
    # version bump is non-trivial.
    snap2.transition_to(Status.READY)
    with pytest.raises(OptimisticConcurrencyError) as exc:
        pg_store.update_lifecycle(snap2, expected_version=1)
    assert exc.value.expected_version == 1
    assert exc.value.actual_version == 2


# ---------------------------------------------------------------------------
# close() releases pool resources.
# ---------------------------------------------------------------------------


def test_close_releases_pool_resources(fresh_schema: str) -> None:
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    store = PostgresStore(dsn, schema=fresh_schema, pool_min=1, pool_max=2)
    store.create_lifecycle(Lifecycle(task_id="t", run_id="r1"))
    store.close()

    # Give the pool a moment to settle then verify operations no longer
    # work on the closed pool.
    time.sleep(0.05)
    from psycopg_pool import PoolClosed

    with pytest.raises(PoolClosed):
        store.load_lifecycle("r1")


# ---------------------------------------------------------------------------
# TIMESTAMPTZ round-trip: tz-aware in, tz-aware out (UTC).
# ---------------------------------------------------------------------------


def test_blocked_requires_json_round_trips_through_postgres(
    pg_store: Any,
) -> None:
    """The hand-written PostgresStore INSERT/UPDATE/SELECT statements
    must name ``blocked_requires_json`` so the column round-trips. NULL
    must survive as ``None`` (not coerced to empty string) and updating
    a non-None value back to NULL must work."""
    payload = (
        '[{"type": "command_grader", "name": "full-suite"}, '
        '{"type": "file_exists", "path": ".flywheel/lkg/.venv", '
        '"present": true}]'
    )

    # 1. Persist with the column set.
    lc = Lifecycle(
        task_id="t",
        run_id="r-set",
        blocked_requires_json=payload,
    )
    pg_store.create_lifecycle(lc)
    loaded = pg_store.load_lifecycle("r-set")
    assert loaded is not None
    assert loaded.blocked_requires_json == payload

    # The raw stored TEXT column is the same string, byte-for-byte.
    rows = _query(
        pg_store,
        "SELECT blocked_requires_json FROM lifecycles WHERE run_id = %s",
        ("r-set",),
    )
    assert rows[0][0] == payload

    # 2. Persist a row that leaves the column unset; NULL becomes None.
    pg_store.create_lifecycle(Lifecycle(task_id="t", run_id="r-null"))
    loaded_null = pg_store.load_lifecycle("r-null")
    assert loaded_null is not None
    assert loaded_null.blocked_requires_json is None
    rows_null = _query(
        pg_store,
        "SELECT blocked_requires_json FROM lifecycles WHERE run_id = %s",
        ("r-null",),
    )
    assert rows_null[0][0] is None

    # 3. Update can clear the column back to NULL.
    loaded.transition_to(Status.READY)
    loaded.blocked_requires_json = None
    pg_store.update_lifecycle(loaded, expected_version=1)
    cleared = pg_store.load_lifecycle("r-set")
    assert cleared is not None
    assert cleared.blocked_requires_json is None
    rows_cleared = _query(
        pg_store,
        "SELECT blocked_requires_json FROM lifecycles WHERE run_id = %s",
        ("r-set",),
    )
    assert rows_cleared[0][0] is None


def test_timestamptz_round_trips_as_aware_utc(pg_store: Any) -> None:
    lc = Lifecycle(task_id="t", run_id="r1")
    lc.transition_to(Status.READY)
    pg_store.create_lifecycle(lc)

    loaded = pg_store.load_lifecycle("r1")
    assert loaded is not None
    ts = loaded.timestamps[Status.READY]
    assert ts.tzinfo is not None
    assert ts.utcoffset() == timezone.utc.utcoffset(ts)


# ---------------------------------------------------------------------------
# Schema-version pin: refuse pre-feature stores.
# ---------------------------------------------------------------------------


def test_opening_postgres_with_mismatched_schema_version_raises(
    fresh_schema: str,
) -> None:
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    store = PostgresStore(dsn, schema=fresh_schema, pool_min=1, pool_max=2)
    store.close()

    import psycopg
    from psycopg import sql

    schema_ident = sql.Identifier(fresh_schema)
    with psycopg.connect(dsn) as conn:
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute(
                sql.SQL("SET search_path TO {}, public").format(schema_ident)
            )
            cur.execute(
                "UPDATE schema_version SET version = %s WHERE id = 1",
                (CURRENT_SCHEMA_VERSION + 99,),
            )

    with pytest.raises(StoreSchemaError) as exc:
        PostgresStore(dsn, schema=fresh_schema, pool_min=1, pool_max=2)
    assert "store must be re-created" in str(exc.value)
    assert exc.value.observed_version == CURRENT_SCHEMA_VERSION + 99
    assert exc.value.expected_version == CURRENT_SCHEMA_VERSION


def test_fresh_postgres_records_current_schema_version(
    pg_store: Any,
) -> None:
    rows = _query(
        pg_store, "SELECT version FROM schema_version WHERE id = 1"
    )
    assert len(rows) == 1
    assert int(rows[0][0]) == CURRENT_SCHEMA_VERSION



# ---------------------------------------------------------------------------
# P3: cross-process LISTEN/NOTIFY bridges committed writes into a consumer's
# in-process notifier, so a follower on a *different* store instance gets
# push wakeups instead of relying solely on poll.
# ---------------------------------------------------------------------------


def test_listen_notify_bridges_cross_instance_writes(
    fresh_schema: str,
) -> None:
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    _ts = datetime(2026, 5, 28, 12, 0, 0, tzinfo=timezone.utc)
    # Writer does not listen; consumer listens on the same schema/channel.
    writer = PostgresStore(dsn, schema=fresh_schema, pool_min=1, pool_max=2)
    consumer = PostgresStore(
        dsn, schema=fresh_schema, pool_min=1, pool_max=2, listen=True
    )
    try:
        # Wait until LISTEN is actually in effect so the NOTIFY below cannot
        # be issued before the consumer is subscribed.
        assert consumer._listen_ready.wait(5.0)

        run_id = "run-pg-listen"
        writer.append_domain_event(
            LifecycleInitialized(run_id=run_id, ts=_ts, task_id="t"),
            expected_version=0,
        )
        # The seed alone should already wake the consumer's notifier via the
        # cross-instance bridge; a telemetry event exercises append_event too.
        writer.append_event(
            EventRecord(run_id=run_id, ts=_ts, kind="harness.x")
        )

        # The consumer never wrote anything, yet its notifier watermark is
        # advanced by the bridge translating the writer's committed NOTIFYs.
        watermark = consumer.notifier.wait(run_id, after=0, timeout=5.0)
        assert watermark >= 1
    finally:
        consumer.close()
        writer.close()


def test_close_stops_the_listener_thread(fresh_schema: str) -> None:
    dsn = _dsn_or_skip()
    from flywheel import PostgresStore

    store = PostgresStore(
        dsn, schema=fresh_schema, pool_min=1, pool_max=2, listen=True
    )
    assert store._listen_ready.wait(5.0)
    thread = store._listen_thread
    assert thread is not None and thread.is_alive()
    store.close()
    assert not thread.is_alive()
