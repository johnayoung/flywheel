"""Postgres implementation of every store Protocol from
``flywheel.store_protocols``.

Durable, network-addressable persistence backend. Sits behind the
optional ``flywheel[postgres]`` extra: importing this module without
``psycopg`` / ``psycopg_pool`` available raises a clear ``ImportError``
that names the extra to install.

The store owns an internal ``psycopg_pool.ConnectionPool`` sized by
constructor arguments so multiple workers can share durable run state
without serializing on a single connection. ``close()`` drains the
pool.

Schema isolation lets multiple flywheel deployments share one database
under separate Postgres schemas. The schema name is validated against
an identifier-safe regex and applied per-connection via ``SET
search_path TO <schema>, public`` in a pool ``configure`` callback;
unqualified DDL in ``flywheel/_schema/persistence-schema-postgres.sql`` therefore
creates tables under the requested schema without f-stringing caller
input into the SQL.

Optimistic concurrency on ``lifecycles.version`` is enforced via a
``WHERE version = %s`` clause on every update; a zero-row-affected
result raises ``OptimisticConcurrencyError`` after a follow-up
existence check disambiguates ``LifecycleNotFoundError`` --
matches the SqliteStore contract.

``grader_results`` stays append-only at the database layer: the schema
file installs ``BEFORE UPDATE`` / ``BEFORE DELETE`` triggers that raise
on any mutation, defending even against callers who bypass the protocol
surface. The trigger's exception (``psycopg.errors.CheckViolation``,
mapped from the ``ERRCODE = 'check_violation'`` clause on the trigger's
``RAISE EXCEPTION``) bubbles up unchanged.
"""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from importlib.resources.abc import Traversable
from importlib.resources import files
from typing import Any, cast

try:
    import psycopg
    import psycopg_pool
    from psycopg import Connection, sql
    from psycopg.rows import dict_row
    from psycopg.types.json import Jsonb
except ImportError as exc:  # pragma: no cover - exercised via monkeypatch
    raise ImportError(
        "flywheel.store_postgres requires the 'postgres' extra; "
        "install with: uv add 'flywheel[postgres]'"
    ) from exc

from flywheel.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel.store_protocols import (
    CURRENT_SCHEMA_VERSION,
    AuditRecord,
    ClaudeSessionEntry,
    EventRecord,
    GraderResultRecord,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    OptimisticConcurrencyError,
    SdkMessageRecord,
    StoreSchemaError,
)

# Page size for ``read_audit_since``; matches the SQLite and in-memory
# stores so the audit-stream contract behaves identically across backends.
_AUDIT_PAGE_SIZE: int = 500

# Bundled as package data so the DDL travels with the install — no
# reliance on a repo-relative ``docs/`` path that breaks under wheel
# installs or the LKG snapshot.
_SCHEMA_PATH: Traversable = (
    files("flywheel") / "_schema" / "persistence-schema-postgres.sql"
)

# Identifier-safe schema name: ASCII letter/underscore start, then
# letter/digit/underscore. Anything else is rejected before substitution
# into DDL so caller input cannot be concatenated into SQL.
_SCHEMA_NAME_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def _validate_schema(schema: str) -> str:
    if not isinstance(schema, str) or not _SCHEMA_NAME_RE.match(schema):
        raise ValueError(
            f"invalid schema name {schema!r}: must match "
            f"[A-Za-z_][A-Za-z0-9_]*"
        )
    return schema


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _serialize_timestamps(ts: dict[Status, datetime]) -> dict[str, str]:
    return {k.value: v.isoformat() for k, v in ts.items()}


def _deserialize_timestamps(blob: Any) -> dict[Status, datetime]:
    out: dict[Status, datetime] = {}
    for k, v in dict(blob).items():
        out[Status(k)] = datetime.fromisoformat(v)
    return out


def _read_schema_sql() -> bytes:
    """Read the canonical Postgres schema script.

    Returned as ``bytes`` so it can be passed to ``cursor.execute`` without
    tripping psycopg's ``LiteralString``-only static type for ``str``
    queries -- the DDL is loaded from disk at runtime and is therefore
    not a literal.
    """
    return _SCHEMA_PATH.read_bytes()


class PostgresStore:
    """Postgres implementation of every store protocol.

    Parameters
    ----------
    dsn:
        libpq-style connection string. Credentials are the caller's
        concern; the store never logs the DSN.
    pool_min, pool_max:
        Lower and upper bound on the internal
        ``psycopg_pool.ConnectionPool``. Defaults match the spec.
    schema:
        Postgres schema name under which every table, index, and
        trigger is created. Validated against an identifier-safe regex
        so it cannot be used as an SQL-injection vector. Two stores
        against the same DSN with different schema names are fully
        isolated.

    The constructor opens the pool, validates connectivity by probing
    the DSN (raises ``psycopg.OperationalError`` unchanged on unreachable
    server / auth failure), ensures the schema exists, and bootstraps
    the schema by executing ``flywheel/_schema/persistence-schema-postgres.sql``.
    Bootstrap is idempotent against an already-initialised database.
    """

    def __init__(
        self,
        dsn: str,
        *,
        pool_min: int = 1,
        pool_max: int = 10,
        schema: str = "public",
    ) -> None:
        self._schema: str = _validate_schema(schema)
        self._schema_ident = sql.Identifier(self._schema)
        # Eagerly validate connectivity. psycopg.OperationalError bubbles
        # up unchanged from this call -- the message names the host/port
        # so operators see auth/network failures directly.
        probe = psycopg.connect(dsn)
        probe.close()
        self._pool: psycopg_pool.ConnectionPool[Connection[Any]] = (
            psycopg_pool.ConnectionPool(
                dsn,
                min_size=pool_min,
                max_size=pool_max,
                open=False,
                configure=self._configure_connection,
            )
        )
        try:
            self._pool.open(wait=True)
            self._bootstrap()
        except Exception:
            self._pool.close()
            raise

    # --- pool lifecycle ---------------------------------------------------

    def _configure_connection(self, conn: Connection[Any]) -> None:
        """Per-connection configuration. Pins the search_path to the
        store's schema so unqualified DDL/DML resolves under it without
        embedding the schema name in every SQL statement."""
        with conn.cursor() as cur:
            cur.execute(
                sql.SQL("SET search_path TO {}, public").format(
                    self._schema_ident
                )
            )
        conn.commit()

    def _bootstrap(self) -> None:
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    sql.SQL("CREATE SCHEMA IF NOT EXISTS {}").format(
                        self._schema_ident
                    )
                )
                # Re-pin search_path after CREATE SCHEMA in case the
                # schema didn't exist at connection-configure time.
                cur.execute(
                    sql.SQL("SET search_path TO {}, public").format(
                        self._schema_ident
                    )
                )
                # Pre-feature detection: a legacy schema has an ``events``
                # table without a ``sequence`` column. The new schema
                # script's ``CREATE TABLE IF NOT EXISTS`` would no-op and
                # silently leave the legacy shape in place, so we check
                # explicitly before bootstrap and refuse with a clear
                # StoreSchemaError.
                cur.execute(
                    """
                    SELECT 1
                    FROM information_schema.tables
                    WHERE table_schema = %s AND table_name = 'events'
                    """,
                    (self._schema,),
                )
                events_exists = cur.fetchone() is not None
                if events_exists:
                    cur.execute(
                        """
                        SELECT 1
                        FROM information_schema.columns
                        WHERE table_schema = %s
                          AND table_name = 'events'
                          AND column_name = 'sequence'
                        """,
                        (self._schema,),
                    )
                    if cur.fetchone() is None:
                        raise StoreSchemaError(
                            observed_version=None,
                            expected_version=CURRENT_SCHEMA_VERSION,
                        )
                cur.execute(_read_schema_sql())
                cur.execute(
                    "SELECT version FROM schema_version WHERE id = 1"
                )
                row = cur.fetchone()
        observed = int(row[0]) if row is not None else None
        if observed != CURRENT_SCHEMA_VERSION:
            raise StoreSchemaError(
                observed_version=observed,
                expected_version=CURRENT_SCHEMA_VERSION,
            )

    def _next_run_sequence(
        self, cur: Any, run_id: str
    ) -> int:
        """Allocate the next per-run audit sequence number atomically.

        Issued through ``cur`` so the increment and the row it gates
        share one transaction; the ``ON CONFLICT … RETURNING`` clause
        returns the freshly-stored value in the same statement.
        """
        cur.execute(
            """
            INSERT INTO run_sequence (run_id, next_seq) VALUES (%s, 1)
            ON CONFLICT (run_id) DO UPDATE
                SET next_seq = run_sequence.next_seq + 1
            RETURNING next_seq
            """,
            (run_id,),
        )
        row = cur.fetchone()
        assert row is not None
        return int(row[0])

    def close(self) -> None:
        """Drain and close the internal pool. Idempotent."""
        self._pool.close()

    # --- LifecycleStore ---------------------------------------------------

    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        try:
            with self._pool.connection() as conn:
                with conn.cursor() as cur:
                    cur.execute(
                        """
                        INSERT INTO lifecycles (
                            run_id, task_id, status, version, retries,
                            error, agent_output, session_id, artifacts_dir,
                            worker_id, timestamps_json, updated_at
                        ) VALUES (
                            %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s,
                            %s
                        )
                        """,
                        (
                            lifecycle.run_id,
                            lifecycle.task_id,
                            lifecycle.status.value,
                            lifecycle.version,
                            lifecycle.retries,
                            lifecycle.error or None,
                            lifecycle.agent_output or None,
                            lifecycle.session_id or None,
                            lifecycle.artifacts_dir or None,
                            lifecycle.worker_id or None,
                            Jsonb(_serialize_timestamps(lifecycle.timestamps)),
                            _utcnow(),
                        ),
                    )
        except psycopg.errors.UniqueViolation as exc:
            raise LifecycleAlreadyExistsError(lifecycle.run_id) from exc

    def update_lifecycle(
        self,
        lifecycle: Lifecycle,
        *,
        expected_version: int,
    ) -> None:
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    UPDATE lifecycles SET
                        task_id = %s,
                        status = %s,
                        version = %s,
                        retries = %s,
                        error = %s,
                        agent_output = %s,
                        session_id = %s,
                        artifacts_dir = %s,
                        worker_id = %s,
                        timestamps_json = %s,
                        updated_at = %s
                    WHERE run_id = %s AND version = %s
                    """,
                    (
                        lifecycle.task_id,
                        lifecycle.status.value,
                        lifecycle.version,
                        lifecycle.retries,
                        lifecycle.error or None,
                        lifecycle.agent_output or None,
                        lifecycle.session_id or None,
                        lifecycle.artifacts_dir or None,
                        lifecycle.worker_id or None,
                        Jsonb(_serialize_timestamps(lifecycle.timestamps)),
                        _utcnow(),
                        lifecycle.run_id,
                        expected_version,
                    ),
                )
                if cur.rowcount == 1:
                    return
                # rowcount == 0 -- disambiguate not-found vs version conflict.
                cur.execute(
                    "SELECT version FROM lifecycles WHERE run_id = %s",
                    (lifecycle.run_id,),
                )
                row = cur.fetchone()
        if row is None:
            raise LifecycleNotFoundError(lifecycle.run_id)
        raise OptimisticConcurrencyError(
            lifecycle.run_id,
            expected_version=expected_version,
            actual_version=int(row[0]),
        )

    def load_lifecycle(self, run_id: str) -> Lifecycle | None:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT run_id, task_id, status, version, retries, error,
                           agent_output, session_id, artifacts_dir, worker_id,
                           timestamps_json
                    FROM lifecycles
                    WHERE run_id = %s
                    """,
                    (run_id,),
                )
                row = cur.fetchone()
        if row is None:
            return None
        lc = Lifecycle(
            task_id=row["task_id"],
            run_id=row["run_id"],
            worker_id=row["worker_id"] or "",
            status=Status(row["status"]),
            timestamps=_deserialize_timestamps(row["timestamps_json"]),
            version=int(row["version"]),
            retries=int(row["retries"]),
            error=row["error"] or "",
            agent_output=row["agent_output"] or "",
            session_id=row["session_id"] or "",
            artifacts_dir=row["artifacts_dir"] or "",
        )
        lc.attempts = self.list_attempts(run_id)
        return lc

    # --- AttemptStore -----------------------------------------------------

    def save_attempt(self, run_id: str, attempt: Attempt) -> None:
        # Upsert on (run_id, number) primary key so callers can both
        # record the start and later finalize an attempt with one verb.
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO attempts (
                        run_id, number, attempt_run_id, started_at, ended_at,
                        outcome, agent_output, error, agent_context_json
                    ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)
                    ON CONFLICT (run_id, number) DO UPDATE SET
                        attempt_run_id = EXCLUDED.attempt_run_id,
                        started_at = EXCLUDED.started_at,
                        ended_at = EXCLUDED.ended_at,
                        outcome = EXCLUDED.outcome,
                        agent_output = EXCLUDED.agent_output,
                        error = EXCLUDED.error,
                        agent_context_json = EXCLUDED.agent_context_json
                    """,
                    (
                        run_id,
                        attempt.number,
                        attempt.run_id or None,
                        attempt.started_at,
                        attempt.ended_at,
                        attempt.outcome.value if attempt.outcome else None,
                        attempt.agent_output or None,
                        attempt.error or None,
                        Jsonb(dict(attempt.agent_context)),
                    ),
                )

    def load_attempt(self, run_id: str, number: int) -> Attempt | None:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT number, attempt_run_id, started_at, ended_at,
                           outcome, agent_output, error, agent_context_json
                    FROM attempts
                    WHERE run_id = %s AND number = %s
                    """,
                    (run_id, number),
                )
                row = cur.fetchone()
        if row is None:
            return None
        return _row_to_attempt(row)

    def list_attempts(self, run_id: str) -> list[Attempt]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT number, attempt_run_id, started_at, ended_at,
                           outcome, agent_output, error, agent_context_json
                    FROM attempts
                    WHERE run_id = %s
                    ORDER BY number
                    """,
                    (run_id,),
                )
                rows = cur.fetchall()
        return [_row_to_attempt(r) for r in rows]

    # --- EventStore -------------------------------------------------------

    def append_event(self, event: EventRecord) -> EventRecord:
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                sequence = self._next_run_sequence(cur, event.run_id)
                cur.execute(
                    """
                    INSERT INTO events (
                        run_id, attempt_number, ts, kind, payload_json,
                        sequence
                    ) VALUES (%s, %s, %s, %s, %s, %s)
                    RETURNING id
                    """,
                    (
                        event.run_id,
                        event.attempt_number,
                        event.ts,
                        event.kind,
                        Jsonb(dict(event.payload)),
                        sequence,
                    ),
                )
                row = cur.fetchone()
        assert row is not None
        return EventRecord(
            run_id=event.run_id,
            ts=event.ts,
            kind=event.kind,
            payload=dict(event.payload),
            attempt_number=event.attempt_number,
            id=int(row[0]),
            sequence=sequence,
        )

    def list_events(self, run_id: str) -> list[EventRecord]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT id, run_id, attempt_number, ts, kind,
                           payload_json, sequence
                    FROM events
                    WHERE run_id = %s
                    ORDER BY ts, id
                    """,
                    (run_id,),
                )
                rows = cur.fetchall()
        return [_row_to_event(r) for r in rows]

    # --- SdkMessageStore --------------------------------------------------

    def save_sdk_messages(
        self,
        run_id: str,
        attempt_number: int,
        iteration_number: int,
        messages: Sequence[Mapping[str, Any]],
    ) -> list[SdkMessageRecord]:
        persisted: list[SdkMessageRecord] = []
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                for msg in messages:
                    payload = dict(msg)
                    message_type = str(payload.get("type", ""))
                    ts = datetime.now(timezone.utc)
                    sequence = self._next_run_sequence(cur, run_id)
                    cur.execute(
                        """
                        INSERT INTO sdk_messages (
                            run_id, attempt_number, iteration_number,
                            sequence, message_type, payload_json, ts
                        ) VALUES (%s, %s, %s, %s, %s, %s, %s)
                        RETURNING id
                        """,
                        (
                            run_id,
                            attempt_number,
                            iteration_number,
                            sequence,
                            message_type,
                            Jsonb(payload),
                            ts,
                        ),
                    )
                    row = cur.fetchone()
                    assert row is not None
                    persisted.append(
                        SdkMessageRecord(
                            run_id=run_id,
                            attempt_number=attempt_number,
                            iteration_number=iteration_number,
                            message_type=message_type,
                            payload=payload,
                            ts=ts,
                            sequence=sequence,
                            id=int(row[0]),
                        )
                    )
        return persisted

    def list_sdk_messages(self, run_id: str) -> list[SdkMessageRecord]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT id, run_id, attempt_number, iteration_number,
                           sequence, message_type, payload_json, ts
                    FROM sdk_messages
                    WHERE run_id = %s
                    ORDER BY sequence
                    """,
                    (run_id,),
                )
                rows = cur.fetchall()
        return [_row_to_sdk_message(r) for r in rows]

    # --- AuditStore -------------------------------------------------------

    def read_audit_since(
        self, run_id: str, cursor: int
    ) -> list[AuditRecord]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT id, run_id, attempt_number, ts, kind,
                           payload_json, sequence
                    FROM events
                    WHERE run_id = %s AND sequence > %s
                    ORDER BY sequence
                    LIMIT %s
                    """,
                    (run_id, cursor, _AUDIT_PAGE_SIZE),
                )
                event_rows = cur.fetchall()
                cur.execute(
                    """
                    SELECT id, run_id, attempt_number, iteration_number,
                           sequence, message_type, payload_json, ts
                    FROM sdk_messages
                    WHERE run_id = %s AND sequence > %s
                    ORDER BY sequence
                    LIMIT %s
                    """,
                    (run_id, cursor, _AUDIT_PAGE_SIZE),
                )
                sdk_rows = cur.fetchall()
        merged: list[AuditRecord] = [_row_to_event(r) for r in event_rows]
        merged.extend(_row_to_sdk_message(r) for r in sdk_rows)
        merged.sort(
            key=lambda r: r.sequence if r.sequence is not None else 0
        )
        return merged[:_AUDIT_PAGE_SIZE]

    # --- GraderResultStore ------------------------------------------------

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO grader_results (
                        run_id, attempt_number, ordinal, grader_type,
                        grader_name, grader_spec_json, passed, duration_ms,
                        payload_json, ts
                    ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                    RETURNING id
                    """,
                    (
                        result.run_id,
                        result.attempt_number,
                        result.ordinal,
                        result.grader_type,
                        result.grader_name,
                        Jsonb(dict(result.grader_spec)),
                        result.passed,
                        result.duration_ms,
                        Jsonb(dict(result.payload)),
                        result.ts,
                    ),
                )
                row = cur.fetchone()
        assert row is not None
        return GraderResultRecord(
            run_id=result.run_id,
            attempt_number=result.attempt_number,
            ordinal=result.ordinal,
            grader_type=result.grader_type,
            grader_spec=dict(result.grader_spec),
            passed=result.passed,
            duration_ms=result.duration_ms,
            payload=dict(result.payload),
            ts=result.ts,
            grader_name=result.grader_name,
            id=int(row[0]),
        )

    def list_grader_results(
        self,
        run_id: str,
        attempt_number: int,
    ) -> list[GraderResultRecord]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT id, run_id, attempt_number, ordinal, grader_type,
                           grader_name, grader_spec_json, passed, duration_ms,
                           payload_json, ts
                    FROM grader_results
                    WHERE run_id = %s AND attempt_number = %s
                    ORDER BY ordinal
                    """,
                    (run_id, attempt_number),
                )
                rows = cur.fetchall()
        return [_row_to_grader_result(r) for r in rows]

    # --- AgentSessionStore ------------------------------------------------

    def append_session_entry(
        self, entry: ClaudeSessionEntry
    ) -> ClaudeSessionEntry:
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO claude_session_store
                        (project_key, session_id, subpath, entry, mtime)
                    VALUES (%s, %s, %s, %s, %s)
                    RETURNING seq
                    """,
                    (
                        entry.project_key,
                        entry.session_id,
                        entry.subpath,
                        entry.entry,
                        entry.mtime,
                    ),
                )
                row = cur.fetchone()
        assert row is not None
        return ClaudeSessionEntry(
            project_key=entry.project_key,
            session_id=entry.session_id,
            entry=entry.entry,
            mtime=entry.mtime,
            subpath=entry.subpath,
            seq=int(row[0]),
        )

    def list_session_entries(
        self,
        project_key: str,
        session_id: str,
        subpath: str = "",
    ) -> list[ClaudeSessionEntry]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT seq, project_key, session_id, subpath, entry, mtime
                    FROM claude_session_store
                    WHERE project_key = %s
                      AND session_id = %s
                      AND subpath = %s
                    ORDER BY seq
                    """,
                    (project_key, session_id, subpath),
                )
                rows = cur.fetchall()
        return [_row_to_session_entry(r) for r in rows]


# --- Row -> dataclass converters --------------------------------------------


def _row_to_attempt(row: dict[str, Any]) -> Attempt:
    outcome_value = row["outcome"]
    ctx_raw = row["agent_context_json"] or {}
    ctx = {str(k): str(v) for k, v in dict(ctx_raw).items()}
    return Attempt(
        number=int(row["number"]),
        started_at=row["started_at"],
        run_id=row["attempt_run_id"] or "",
        ended_at=row["ended_at"],
        outcome=Outcome(outcome_value) if outcome_value else None,
        agent_output=row["agent_output"] or "",
        error=row["error"] or "",
        agent_context=ctx,
    )


def _row_to_event(row: dict[str, Any]) -> EventRecord:
    sequence_raw = row.get("sequence")
    return EventRecord(
        run_id=row["run_id"],
        ts=row["ts"],
        kind=row["kind"],
        payload=dict(row["payload_json"]),
        attempt_number=row["attempt_number"],
        id=int(row["id"]),
        sequence=int(sequence_raw) if sequence_raw is not None else None,
    )


def _row_to_sdk_message(row: dict[str, Any]) -> SdkMessageRecord:
    return SdkMessageRecord(
        run_id=row["run_id"],
        attempt_number=int(row["attempt_number"]),
        iteration_number=int(row["iteration_number"]),
        message_type=row["message_type"],
        payload=dict(row["payload_json"]),
        ts=row["ts"],
        sequence=int(row["sequence"]),
        id=int(row["id"]),
    )


def _row_to_grader_result(row: dict[str, Any]) -> GraderResultRecord:
    return GraderResultRecord(
        run_id=row["run_id"],
        attempt_number=int(row["attempt_number"]),
        ordinal=int(row["ordinal"]),
        grader_type=cast(GraderType, row["grader_type"]),
        grader_spec=dict(row["grader_spec_json"]),
        passed=bool(row["passed"]),
        duration_ms=int(row["duration_ms"]),
        payload=dict(row["payload_json"]),
        ts=row["ts"],
        grader_name=row["grader_name"],
        id=int(row["id"]),
    )


def _row_to_session_entry(row: dict[str, Any]) -> ClaudeSessionEntry:
    return ClaudeSessionEntry(
        project_key=row["project_key"],
        session_id=row["session_id"],
        entry=row["entry"],
        mtime=int(row["mtime"]),
        subpath=row["subpath"],
        seq=int(row["seq"]),
    )


__all__ = ["PostgresStore"]
