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

import logging
import re
import threading
from collections.abc import Mapping, Sequence
from datetime import datetime, timedelta, timezone
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

from flywheel.event_serde import event_from_record, event_kind, event_payload
from flywheel.events import (
    AttemptFinalized,
    AttemptStarted,
    DomainEvent,
    GraderEvaluated,
    LifecycleInitialized,
    apply,
)
from flywheel.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel.loaders import deserialize_task, serialize_task, task_digest
from flywheel.notifier import RunNotifier
from flywheel.task import Task
from flywheel.store_protocols import (
    CURRENT_SCHEMA_VERSION,
    AuditRecord,
    ClaimLostError,
    ControlCommandRecord,
    EventRecord,
    GraderResultRecord,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    OptimisticConcurrencyError,
    SdkMessageRecord,
    StoreSchemaError,
    TaskClaim,
)

_LOGGER = logging.getLogger(__name__)

# Page size for ``read_audit_since``; matches the SQLite and in-memory
# stores so the audit-stream contract behaves identically across backends.
_AUDIT_PAGE_SIZE: int = 500

# Postgres channel-name limit (NAMEDATALEN - 1). pg_notify rejects a
# longer channel, and LISTEN silently truncates it, so we refuse a schema
# whose derived channel would exceed this rather than risk a mismatch.
_MAX_CHANNEL_BYTES: int = 63

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

    # Listener thread cadence: how long each notifies() drain blocks before
    # re-checking the stop flag, and how long close() waits to join.
    _LISTEN_POLL_SECONDS: float = 1.0
    _LISTEN_JOIN_SECONDS: float = 2.0

    def __init__(
        self,
        dsn: str,
        *,
        pool_min: int = 1,
        pool_max: int = 10,
        schema: str = "public",
        notifier: RunNotifier | None = None,
        listen: bool = False,
    ) -> None:
        # In-process reactivity for same-instance consumers always works.
        # Cross-process push (the multi-host case) is opt-in via ``listen``:
        # a dedicated LISTEN connection bridges other instances' committed
        # writes into this same notifier. Writes always issue NOTIFY, so a
        # listening consumer on any instance is woken; with no listener the
        # NOTIFY is simply discarded by Postgres.
        self.notifier: RunNotifier = notifier or RunNotifier()
        self._schema: str = _validate_schema(schema)
        self._schema_ident = sql.Identifier(self._schema)
        self._dsn = dsn
        self._channel = self._derive_channel(self._schema)
        self._listen_stop = threading.Event()
        self._listen_ready = threading.Event()
        self._listen_thread: threading.Thread | None = None
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
            if listen:
                self._start_listener()
        except Exception:
            self._listen_stop.set()
            self._pool.close()
            raise

    @staticmethod
    def _derive_channel(schema: str) -> str:
        """Schema-qualified NOTIFY channel so deployments sharing a database
        under different schemas do not cross-talk."""
        channel = f"flywheel_run_{schema}"
        if len(channel.encode("utf-8")) > _MAX_CHANNEL_BYTES:
            raise ValueError(
                f"schema {schema!r} is too long: the derived NOTIFY channel "
                f"{channel!r} exceeds {_MAX_CHANNEL_BYTES} bytes"
            )
        return channel

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
                # Back-compat migration: cf45b58 added blocked_requires_json
                # to lifecycles without bumping schema_version. Existing
                # schema_version=2 databases created before that commit lack
                # the column; CREATE TABLE IF NOT EXISTS above no-ops on
                # them, so add the column here when missing.
                cur.execute(
                    """
                    SELECT 1
                    FROM information_schema.columns
                    WHERE table_schema = %s
                      AND table_name = 'lifecycles'
                      AND column_name = 'blocked_requires_json'
                    """,
                    (self._schema,),
                )
                if cur.fetchone() is None:
                    cur.execute(
                        "ALTER TABLE lifecycles "
                        "ADD COLUMN blocked_requires_json TEXT"
                    )
                # Additive, same back-compat path as blocked_requires_json:
                # events.category distinguishes domain (state-bearing) from
                # telemetry rows. The DEFAULT makes pre-existing rows
                # 'telemetry'. No schema_version bump.
                cur.execute(
                    """
                    SELECT 1
                    FROM information_schema.columns
                    WHERE table_schema = %s
                      AND table_name = 'events'
                      AND column_name = 'category'
                    """,
                    (self._schema,),
                )
                if cur.fetchone() is None:
                    cur.execute(
                        "ALTER TABLE events ADD COLUMN category TEXT "
                        "NOT NULL DEFAULT 'telemetry'"
                    )
                # Forward migration from schema_version 3 -> 4: the
                # ``control_commands`` table ships in schema_version 4.
                # CREATE TABLE IF NOT EXISTS above already materialized it
                # on the existing schema; the INSERT ... ON CONFLICT DO
                # NOTHING in the script no-ops against the pre-existing
                # v3 row, so bump the row explicitly when it is still at
                # 3. Guarded on the prior version so re-bootstrap of a v4
                # schema is a no-op and a future version still fails the
                # final pin below.
                cur.execute(
                    "UPDATE schema_version SET version = %s "
                    "WHERE id = 1 AND version = %s",
                    (4, 3),
                )
                # Forward migration from schema_version 4 -> 5: the
                # ``lifecycles.awaiting_manual_ordinal`` nullable column
                # ships in schema_version 5. The CREATE TABLE IF NOT EXISTS
                # in the script no-ops on an existing schema, so an existing
                # v4 schema needs an explicit ALTER to materialize the
                # column; the column is nullable so a v4 row reads NULL
                # (the post-migration sentinel for "not parked on a manual
                # gate"). Guarded on the prior version so re-bootstrap of
                # a v5 schema is a no-op.
                cur.execute(
                    """
                    SELECT 1
                    FROM information_schema.columns
                    WHERE table_schema = %s
                      AND table_name = 'lifecycles'
                      AND column_name = 'awaiting_manual_ordinal'
                    """,
                    (self._schema,),
                )
                if cur.fetchone() is None:
                    cur.execute(
                        "ALTER TABLE lifecycles "
                        "ADD COLUMN awaiting_manual_ordinal INTEGER"
                    )
                cur.execute(
                    "UPDATE schema_version SET version = %s "
                    "WHERE id = 1 AND version = %s",
                    (5, 4),
                )
                # Forward migration from schema_version 5 -> 6: the unused
                # claude_session_store table is dropped. The schema script no
                # longer creates it, so a fresh schema never has it, but an
                # existing v5 schema still carries the (empty) table — drop
                # it explicitly, then bump. DROP IF EXISTS is a no-op on a
                # fresh v6 schema; the bump is guarded on the prior version
                # so re-bootstrap of a v6 schema is a no-op.
                cur.execute("DROP TABLE IF EXISTS claude_session_store")
                cur.execute(
                    "UPDATE schema_version SET version = %s "
                    "WHERE id = 1 AND version = %s",
                    (CURRENT_SCHEMA_VERSION, 5),
                )
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

    def _notify_pg(self, cur: Any, run_id: str, sequence: int) -> None:
        """Queue a cross-process NOTIFY on the write's own transaction.

        Postgres holds the notification until the surrounding transaction
        commits, so a listener never observes a watermark for an aborted
        write. The payload is ``"<run_id>:<sequence>"``; the listener splits
        on the last colon.
        """
        cur.execute(
            "SELECT pg_notify(%s, %s)",
            (self._channel, f"{run_id}:{sequence}"),
        )

    def _start_listener(self) -> None:
        self._listen_thread = threading.Thread(
            target=self._run_listener,
            name=f"flywheel-pg-listener-{self._schema}",
            daemon=True,
        )
        self._listen_thread.start()

    def _run_listener(self) -> None:
        """Bridge committed cross-instance writes into the in-process
        notifier via a dedicated LISTEN connection.

        Runs on its own autocommit connection (LISTEN must be outside a
        transaction). Re-checks the stop flag every ``_LISTEN_POLL_SECONDS``
        between notification drains so ``close()`` cancels promptly. The
        bridge is best-effort: a connection failure is logged and the
        consumer degrades to its bounded poll, never hangs.
        """
        try:
            conn = psycopg.connect(self._dsn, autocommit=True)
        except Exception as exc:  # noqa: BLE001 - degrade to poll
            _LOGGER.warning(
                "flywheel listener (%s) failed to connect: %s",
                self._channel,
                exc,
            )
            self._listen_ready.set()
            return
        try:
            conn.execute(
                sql.SQL("LISTEN {}").format(sql.Identifier(self._channel))
            )
            # Signal readiness only after LISTEN is in effect, so a producer
            # that coordinates on this never issues a NOTIFY the listener
            # would miss.
            self._listen_ready.set()
            while not self._listen_stop.is_set():
                try:
                    for note in conn.notifies(
                        timeout=self._LISTEN_POLL_SECONDS
                    ):
                        self._handle_notify(note.payload)
                        if self._listen_stop.is_set():
                            break
                except Exception as exc:  # noqa: BLE001 - transient
                    if self._listen_stop.is_set():
                        break
                    _LOGGER.warning(
                        "flywheel listener (%s) error: %s",
                        self._channel,
                        exc,
                    )
                    self._listen_stop.wait(0.5)
        finally:
            try:
                conn.close()
            except Exception:  # noqa: BLE001 - best effort on shutdown
                pass

    def _handle_notify(self, payload: str) -> None:
        run_id, sep, seq_text = payload.rpartition(":")
        if not sep:
            return
        try:
            sequence = int(seq_text)
        except ValueError:
            return
        self.notifier.notify(run_id, sequence)

    def close(self) -> None:
        """Stop the listener (if any) and drain the pool. Idempotent."""
        self._listen_stop.set()
        thread = self._listen_thread
        if thread is not None and thread.is_alive():
            thread.join(timeout=self._LISTEN_JOIN_SECONDS)
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
                            worker_id, timestamps_json, updated_at,
                            blocked_requires_json, task_content_hash,
                            awaiting_manual_ordinal
                        ) VALUES (
                            %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s,
                            %s, %s, %s, %s
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
                            lifecycle.blocked_requires_json,
                            lifecycle.task_content_hash or None,
                            lifecycle.awaiting_manual_ordinal,
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
                        updated_at = %s,
                        blocked_requires_json = %s,
                        task_content_hash = %s,
                        awaiting_manual_ordinal = %s
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
                        lifecycle.blocked_requires_json,
                        lifecycle.task_content_hash or None,
                        lifecycle.awaiting_manual_ordinal,
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
                           timestamps_json, blocked_requires_json,
                           task_content_hash, awaiting_manual_ordinal
                    FROM lifecycles
                    WHERE run_id = %s
                    """,
                    (run_id,),
                )
                row = cur.fetchone()
        if row is None:
            return None
        awaiting_raw = row["awaiting_manual_ordinal"]
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
            blocked_requires_json=row["blocked_requires_json"],
            awaiting_manual_ordinal=(
                int(awaiting_raw) if awaiting_raw is not None else None
            ),
            task_content_hash=row["task_content_hash"] or "",
        )
        lc.attempts = self.list_attempts(run_id)
        return lc

    # --- TaskStore --------------------------------------------------------

    def save_task(self, task: Task, *, now: datetime) -> str:
        content_hash = task_digest(task)
        data = serialize_task(task)
        # ON CONFLICT DO NOTHING makes the (id, content_hash) write
        # idempotent: re-saving an unchanged task is a no-op and the
        # original created_at is preserved.
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO tasks (
                        id, content_hash, goal, graders_json,
                        prerequisites_json, tags_json, context_json,
                        created_at
                    ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
                    ON CONFLICT (id, content_hash) DO NOTHING
                    """,
                    (
                        task.id,
                        content_hash,
                        data["goal"],
                        Jsonb(data["graders"]),
                        Jsonb(data["prerequisites"]),
                        Jsonb(data["tags"]),
                        Jsonb(data["context"]),
                        now,
                    ),
                )
        return content_hash

    def load_task(
        self, task_id: str, content_hash: str | None = None
    ) -> Task | None:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                if content_hash is not None:
                    cur.execute(
                        "SELECT id, goal, graders_json, prerequisites_json, "
                        "tags_json, context_json FROM tasks "
                        "WHERE id = %s AND content_hash = %s",
                        (task_id, content_hash),
                    )
                else:
                    cur.execute(
                        "SELECT id, goal, graders_json, prerequisites_json, "
                        "tags_json, context_json FROM tasks "
                        "WHERE id = %s ORDER BY created_at DESC LIMIT 1",
                        (task_id,),
                    )
                row = cur.fetchone()
        if row is None:
            return None
        return _row_to_task(row)

    def load_task_for_run(self, run_id: str) -> Task | None:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    "SELECT task_id, task_content_hash FROM lifecycles "
                    "WHERE run_id = %s",
                    (run_id,),
                )
                row = cur.fetchone()
        if row is None:
            return None
        return self.load_task(
            row["task_id"], row["task_content_hash"] or None
        )

    # --- DomainEventStore -------------------------------------------------

    def append_domain_event(
        self,
        event: DomainEvent,
        *,
        expected_version: int,
    ) -> Lifecycle:
        # One pooled connection => one transaction. The context manager
        # commits on success and rolls back on any exception, so the
        # version check, projection writes, and event-row append are
        # atomic. SELECT ... FOR UPDATE locks the lifecycle row for the
        # transaction so a concurrent appender cannot interleave. Each
        # helper opens its own cursor on the shared connection; the lock
        # and transaction live on the connection, not the cursor.
        with self._pool.connection() as conn:
            if isinstance(event, LifecycleInitialized):
                folded = apply(None, event)
                try:
                    self._insert_lifecycle_conn(conn, folded)
                except psycopg.errors.UniqueViolation as exc:
                    raise LifecycleAlreadyExistsError(event.run_id) from exc
            else:
                current = self._load_lifecycle_for_update(conn, event.run_id)
                if current is None:
                    raise LifecycleNotFoundError(event.run_id)
                if current.version != expected_version:
                    raise OptimisticConcurrencyError(
                        event.run_id,
                        expected_version=expected_version,
                        actual_version=current.version,
                    )
                folded = apply(current, event)
                self._update_lifecycle_conn(conn, folded)
                self._project_domain_event_conn(conn, event, folded)
            sequence = self._insert_domain_event_row_conn(conn, event)
        # Signal only after the connection's transaction commits (on exit
        # of the `with` block) so a woken consumer never sees a watermark
        # for an uncommitted append.
        self.notifier.notify(event.run_id, sequence)
        return folded

    def _insert_lifecycle_conn(
        self, conn: Connection[Any], lc: Lifecycle
    ) -> None:
        with conn.cursor() as cur:
            cur.execute(
                """
                INSERT INTO lifecycles (
                    run_id, task_id, status, version, retries, error,
                    agent_output, session_id, artifacts_dir, worker_id,
                    timestamps_json, updated_at, blocked_requires_json,
                    task_content_hash, awaiting_manual_ordinal
                ) VALUES (
                    %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s,
                    %s
                )
                """,
                (
                    lc.run_id,
                    lc.task_id,
                    lc.status.value,
                    lc.version,
                    lc.retries,
                    lc.error or None,
                    lc.agent_output or None,
                    lc.session_id or None,
                    lc.artifacts_dir or None,
                    lc.worker_id or None,
                    Jsonb(_serialize_timestamps(lc.timestamps)),
                    _utcnow(),
                    lc.blocked_requires_json,
                    lc.task_content_hash or None,
                    lc.awaiting_manual_ordinal,
                ),
            )

    def _update_lifecycle_conn(
        self, conn: Connection[Any], lc: Lifecycle
    ) -> None:
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
                    updated_at = %s,
                    blocked_requires_json = %s,
                    task_content_hash = %s,
                    awaiting_manual_ordinal = %s
                WHERE run_id = %s
                """,
                (
                    lc.task_id,
                    lc.status.value,
                    lc.version,
                    lc.retries,
                    lc.error or None,
                    lc.agent_output or None,
                    lc.session_id or None,
                    lc.artifacts_dir or None,
                    lc.worker_id or None,
                    Jsonb(_serialize_timestamps(lc.timestamps)),
                    _utcnow(),
                    lc.blocked_requires_json,
                    lc.task_content_hash or None,
                    lc.awaiting_manual_ordinal,
                    lc.run_id,
                ),
            )

    def _load_lifecycle_for_update(
        self, conn: Connection[Any], run_id: str
    ) -> Lifecycle | None:
        with conn.cursor(row_factory=dict_row) as cur:
            cur.execute(
                """
                SELECT run_id, task_id, status, version, retries, error,
                       agent_output, session_id, artifacts_dir, worker_id,
                       timestamps_json, blocked_requires_json,
                       task_content_hash, awaiting_manual_ordinal
                FROM lifecycles
                WHERE run_id = %s
                FOR UPDATE
                """,
                (run_id,),
            )
            row = cur.fetchone()
            if row is None:
                return None
            awaiting_raw = row["awaiting_manual_ordinal"]
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
                blocked_requires_json=row["blocked_requires_json"],
                awaiting_manual_ordinal=(
                    int(awaiting_raw) if awaiting_raw is not None else None
                ),
                task_content_hash=row["task_content_hash"] or "",
            )
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
            lc.attempts = [_row_to_attempt(r) for r in cur.fetchall()]
        return lc

    def _project_domain_event_conn(
        self, conn: Connection[Any], event: DomainEvent, folded: Lifecycle
    ) -> None:
        if isinstance(event, (AttemptStarted, AttemptFinalized)):
            attempt = next(
                a for a in folded.attempts if a.number == event.number
            )
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
                        folded.run_id,
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
        elif isinstance(event, GraderEvaluated):
            assert event.attempt_number is not None
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO grader_results (
                        run_id, attempt_number, ordinal, grader_type,
                        grader_name, grader_spec_json, passed, duration_ms,
                        payload_json, ts
                    ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                    """,
                    (
                        event.run_id,
                        event.attempt_number,
                        event.ordinal,
                        event.grader_type,
                        event.grader_name,
                        Jsonb(dict(event.grader_spec)),
                        event.passed,
                        event.duration_ms,
                        Jsonb(dict(event.payload)),
                        event.ts,
                    ),
                )

    def _insert_domain_event_row_conn(
        self, conn: Connection[Any], event: DomainEvent
    ) -> int:
        with conn.cursor() as cur:
            sequence = self._next_run_sequence(cur, event.run_id)
            cur.execute(
                """
                INSERT INTO events (
                    run_id, attempt_number, ts, kind, payload_json, sequence,
                    category
                ) VALUES (%s, %s, %s, %s, %s, %s, 'domain')
                """,
                (
                    event.run_id,
                    event.attempt_number,
                    event.ts,
                    event_kind(event),
                    Jsonb(event_payload(event)),
                    sequence,
                ),
            )
            self._notify_pg(cur, event.run_id, sequence)
        return sequence

    def list_domain_events(self, run_id: str) -> list[DomainEvent]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT id, run_id, attempt_number, ts, kind,
                           payload_json, sequence
                    FROM events
                    WHERE run_id = %s AND category = 'domain'
                    ORDER BY sequence
                    """,
                    (run_id,),
                )
                rows = cur.fetchall()
        return [
            event_from_record(
                kind=r["kind"],
                payload=dict(r["payload_json"]),
                run_id=r["run_id"],
                ts=r["ts"],
                attempt_number=r["attempt_number"],
                sequence=int(r["sequence"]) if r["sequence"] is not None
                else None,
                id=int(r["id"]),
            )
            for r in rows
        ]

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
                self._notify_pg(cur, event.run_id, sequence)
        assert row is not None
        self.notifier.notify(event.run_id, sequence)
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
                           payload_json, sequence, category
                    FROM events
                    WHERE run_id = %s AND category = 'telemetry'
                    ORDER BY ts, id
                    """,
                    (run_id,),
                )
                rows = cur.fetchall()
        return [_row_to_event(r) for r in rows]

    # --- SdkMessageStore --------------------------------------------------

    def append_sdk_message(
        self, message: SdkMessageRecord
    ) -> SdkMessageRecord:
        payload = dict(message.payload)
        message_type = message.message_type or str(
            payload.get("message_type", payload.get("type", ""))
        )
        ts = message.ts
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                sequence = self._next_run_sequence(cur, message.run_id)
                cur.execute(
                    """
                    INSERT INTO sdk_messages (
                        run_id, attempt_number, iteration_number,
                        sequence, message_type, payload_json, ts
                    ) VALUES (%s, %s, %s, %s, %s, %s, %s)
                    RETURNING id
                    """,
                    (
                        message.run_id,
                        message.attempt_number,
                        message.iteration_number,
                        sequence,
                        message_type,
                        Jsonb(payload),
                        ts,
                    ),
                )
                row = cur.fetchone()
                self._notify_pg(cur, message.run_id, sequence)
        assert row is not None
        self.notifier.notify(message.run_id, sequence)
        return SdkMessageRecord(
            run_id=message.run_id,
            attempt_number=message.attempt_number,
            iteration_number=message.iteration_number,
            message_type=message_type,
            payload=payload,
            ts=ts,
            sequence=sequence,
            id=int(row[0]),
        )

    def save_sdk_messages(
        self,
        run_id: str,
        attempt_number: int,
        iteration_number: int,
        messages: Sequence[Mapping[str, Any]],
    ) -> list[SdkMessageRecord]:
        persisted: list[SdkMessageRecord] = []
        for msg in messages:
            payload = dict(msg)
            message_type = str(
                payload.get("message_type", payload.get("type", ""))
            )
            record = SdkMessageRecord(
                run_id=run_id,
                attempt_number=attempt_number,
                iteration_number=iteration_number,
                message_type=message_type,
                payload=payload,
                ts=datetime.now(timezone.utc),
            )
            persisted.append(self.append_sdk_message(record))
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
                           payload_json, sequence, category
                    FROM events
                    WHERE run_id = %s AND sequence > %s
                      AND category = 'telemetry'
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

    # --- ClaimStore -------------------------------------------------------

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim | None:
        lease_expires = now + timedelta(seconds=lease_seconds)
        # One atomic statement: insert when free, or steal/re-acquire on
        # conflict only when the existing lease has lapsed or is already
        # ours. A live claim held by another worker fails the DO UPDATE
        # WHERE, so RETURNING yields no row and we return None.
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO task_claims (
                        task_id, worker_id, claimed_at, lease_expires_at,
                        version
                    ) VALUES (%s, %s, %s, %s, 1)
                    ON CONFLICT (task_id) DO UPDATE SET
                        worker_id = EXCLUDED.worker_id,
                        claimed_at = EXCLUDED.claimed_at,
                        lease_expires_at = EXCLUDED.lease_expires_at,
                        version = task_claims.version + 1
                    WHERE task_claims.lease_expires_at <= EXCLUDED.claimed_at
                       OR task_claims.worker_id = EXCLUDED.worker_id
                    RETURNING version
                    """,
                    (task_id, worker_id, now, lease_expires),
                )
                row = cur.fetchone()
        if row is None:
            return None
        return TaskClaim(
            task_id=task_id,
            worker_id=worker_id,
            claimed_at=now,
            lease_expires_at=lease_expires,
            version=int(row[0]),
        )

    def renew_claim(
        self,
        claim: TaskClaim,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim:
        lease_expires = now + timedelta(seconds=lease_seconds)
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    UPDATE task_claims
                    SET lease_expires_at = %s, version = version + 1
                    WHERE task_id = %s AND version = %s AND worker_id = %s
                    RETURNING version
                    """,
                    (lease_expires, claim.task_id, claim.version,
                     claim.worker_id),
                )
                row = cur.fetchone()
        if row is None:
            raise ClaimLostError(claim.task_id)
        return TaskClaim(
            task_id=claim.task_id,
            worker_id=claim.worker_id,
            claimed_at=claim.claimed_at,
            lease_expires_at=lease_expires,
            version=int(row[0]),
        )

    def release_claim(self, claim: TaskClaim) -> None:
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "DELETE FROM task_claims "
                    "WHERE task_id = %s AND version = %s AND worker_id = %s",
                    (claim.task_id, claim.version, claim.worker_id),
                )

    def load_claim(self, task_id: str) -> TaskClaim | None:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT task_id, worker_id, claimed_at, lease_expires_at,
                           version
                    FROM task_claims WHERE task_id = %s
                    """,
                    (task_id,),
                )
                row = cur.fetchone()
        if row is None:
            return None
        return TaskClaim(
            task_id=row["task_id"],
            worker_id=row["worker_id"],
            claimed_at=row["claimed_at"],
            lease_expires_at=row["lease_expires_at"],
            version=int(row["version"]),
        )

    # --- ControlCommandStore ----------------------------------------------

    def enqueue_command(
        self,
        run_id: str,
        kind: str,
        payload: Mapping[str, Any],
        *,
        now: datetime,
    ) -> ControlCommandRecord:
        # Persist verbatim. ``claimed_at`` stays NULL so the row joins the
        # pending queue immediately; BIGSERIAL assigns ``id``, which is the
        # per-run enqueue-order key consumed by claim_commands.
        payload_copy = dict(payload)
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO control_commands (
                        run_id, kind, payload_json, enqueued_at, claimed_at
                    ) VALUES (%s, %s, %s, %s, NULL)
                    RETURNING id
                    """,
                    (run_id, kind, Jsonb(payload_copy), now),
                )
                row = cur.fetchone()
        assert row is not None
        return ControlCommandRecord(
            run_id=run_id,
            kind=kind,
            payload=payload_copy,
            enqueued_at=now,
            claimed_at=None,
            id=int(row[0]),
        )

    def claim_commands(
        self,
        run_id: str,
        *,
        now: datetime,
    ) -> list[ControlCommandRecord]:
        # Claim-once across concurrent workers: the inner SELECT pins each
        # pending row with FOR UPDATE SKIP LOCKED, so a second worker
        # running claim_commands at the same time sees no overlap with
        # this caller's slice. The outer UPDATE then flips claimed_at and
        # returns the row. Sorted ascending by id so callers consume
        # commands in enqueue order regardless of RETURNING's row order.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    UPDATE control_commands
                    SET claimed_at = %s
                    WHERE id IN (
                        SELECT id
                        FROM control_commands
                        WHERE run_id = %s AND claimed_at IS NULL
                        ORDER BY id
                        FOR UPDATE SKIP LOCKED
                    )
                    RETURNING id, run_id, kind, payload_json, enqueued_at,
                              claimed_at
                    """,
                    (now, run_id),
                )
                rows = cur.fetchall()
        records = [_row_to_control_command(r) for r in rows]
        records.sort(key=lambda r: r.id if r.id is not None else 0)
        return records


# --- Row -> dataclass converters --------------------------------------------


def _row_to_task(row: dict[str, Any]) -> Task:
    # JSONB columns arrive already parsed into Python objects under dict_row.
    return deserialize_task(
        {
            "id": row["id"],
            "goal": row["goal"],
            "graders": list(row["graders_json"]),
            "prerequisites": list(row["prerequisites_json"]),
            "tags": list(row["tags_json"]),
            "context": dict(row["context_json"]),
        }
    )


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
        category=row.get("category", "telemetry"),
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


def _row_to_control_command(row: dict[str, Any]) -> ControlCommandRecord:
    return ControlCommandRecord(
        run_id=row["run_id"],
        kind=row["kind"],
        payload=dict(row["payload_json"]),
        enqueued_at=row["enqueued_at"],
        claimed_at=row["claimed_at"],
        id=int(row["id"]),
    )


__all__ = ["PostgresStore"]
