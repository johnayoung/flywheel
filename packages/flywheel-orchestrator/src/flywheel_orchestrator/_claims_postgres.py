"""Postgres :class:`ClaimStore`. Behind the ``postgres`` extra.

Mirrors ``flywheel_core.store_postgres`` operationally — a pooled, schema-isolated
connection — but owns only ``task_claims`` and its own
``orchestrator_schema_version`` sentinel, so it can share a database (under the
same or a different schema) with flywheel's Postgres store without either
touching the other's tables.
"""

from __future__ import annotations

import re
from collections.abc import Iterable
from datetime import datetime, timedelta
from typing import TYPE_CHECKING, Any

try:
    import psycopg
    import psycopg_pool
    from psycopg import Connection, Cursor, sql
    from psycopg.rows import dict_row
except ImportError as exc:  # pragma: no cover - exercised via env
    raise ImportError(
        "flywheel_orchestrator.store_postgres requires the 'postgres' extra; "
        "install psycopg / psycopg_pool"
    ) from exc

from flywheel_core.loaders import task_digest

from flywheel_orchestrator._claims import (
    CURRENT_ORCH_SCHEMA_VERSION,
    HUMAN_REVIEW_QUEUE_REASONS,
    EVENT_ACQUIRED,
    EVENT_EXPIRED,
    EVENT_RELEASED,
    EVENT_RENEWED,
    EVENT_STOLEN,
    ClaimLostError,
    STOP_PREPARE_SKIP,
    GraphSnapshotItem,
    GraphSnapshotRecord,
    HumanReviewQueueEntry,
    OrchestratorEventRecord,
    OrchestratorSchemaError,
    OrchestratorStopEventRecord,
    SourceSyncRecord,
    TaskClaim,
    WorkItemRecord,
    _require_review_reason,
    decode_str_set,
    encode_str_set,
)

if TYPE_CHECKING:
    from flywheel_orchestrator._sources import WorkItem

_SCHEMA_NAME_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def _validate_schema(schema: str) -> str:
    if not isinstance(schema, str) or not _SCHEMA_NAME_RE.match(schema):
        raise ValueError(
            f"invalid schema name {schema!r}: must match "
            f"[A-Za-z_][A-Za-z0-9_]*"
        )
    return schema


# JSONB columns are cast to text so the read-back ``*_json`` fields are
# canonical JSON strings, matching the SQLite backend's TEXT storage.
_WORK_ITEM_SELECT = """
    SELECT task_id, source_kind, source_ref, source_url, source_version,
           task_content_hash, priority,
           required_capabilities_json::text AS required_capabilities_json,
           conflict_keys_json::text AS conflict_keys_json,
           first_seen_at, last_seen_at, disappeared_at,
           metadata_json::text AS metadata_json
    FROM work_items
"""


_SOURCE_SYNC_SELECT = """
    SELECT id, source_kind, source_name, started_at, finished_at, status,
           observed_count, error, metadata_json::text AS metadata_json
    FROM source_syncs
"""


_ORCH_EVENT_SELECT = """
    SELECT id, task_id, worker_id, event_type, version,
           lease_expires_at, occurred_at
    FROM orchestrator_events
"""


_ORCH_STOP_EVENT_SELECT = """
    SELECT id, kind, subject, detail, occurred_at, run_id
    FROM orchestrator_stop_events
"""


_GRAPH_SNAPSHOT_SELECT = """
    SELECT id, captured_at, item_count, last_event_id
    FROM graph_snapshots
"""


# JSONB set columns are cast to text so the read-back ``*_json`` content decodes
# through the same ``decode_str_set`` the SQLite TEXT columns use -- identical
# required_capabilities / conflict_keys / resolved-prerequisites sets across
# backends regardless of storage representation.
_GRAPH_SNAPSHOT_ITEM_SELECT = """
    SELECT task_id, source_kind, source_ref, source_url, source_version,
           priority,
           required_capabilities_json::text AS required_capabilities_json,
           conflict_keys_json::text AS conflict_keys_json,
           state, ready, claim_holder,
           resolved_prerequisites_json::text AS resolved_prerequisites_json
    FROM graph_snapshot_items
"""


def _row_to_graph_snapshot_record(row: dict[str, Any]) -> GraphSnapshotRecord:
    return GraphSnapshotRecord(
        id=int(row["id"]),
        captured_at=row["captured_at"],
        item_count=int(row["item_count"]),
        last_event_id=int(row["last_event_id"]),
    )


def _row_to_graph_snapshot_item(row: dict[str, Any]) -> GraphSnapshotItem:
    return GraphSnapshotItem(
        task_id=row["task_id"],
        source_kind=row["source_kind"],
        source_ref=row["source_ref"],
        source_url=row["source_url"],
        source_version=row["source_version"],
        priority=int(row["priority"]),
        required_capabilities=decode_str_set(row["required_capabilities_json"]),
        conflict_keys=decode_str_set(row["conflict_keys_json"]),
        state=row["state"],
        ready=bool(row["ready"]),
        claim_holder=row["claim_holder"],
        resolved_prerequisites=decode_str_set(
            row["resolved_prerequisites_json"]
        ),
    )


def _row_to_orchestrator_event_record(
    row: dict[str, Any],
) -> OrchestratorEventRecord:
    return OrchestratorEventRecord(
        id=int(row["id"]),
        task_id=row["task_id"],
        worker_id=row["worker_id"],
        event_type=row["event_type"],
        version=int(row["version"]),
        lease_expires_at=row["lease_expires_at"],
        occurred_at=row["occurred_at"],
    )


def _row_to_orchestrator_stop_event_record(
    row: dict[str, Any],
) -> OrchestratorStopEventRecord:
    return OrchestratorStopEventRecord(
        id=int(row["id"]),
        kind=row["kind"],
        subject=row["subject"],
        detail=row["detail"],
        occurred_at=row["occurred_at"],
        run_id=row["run_id"],
    )


def _row_to_human_review_entry(row: dict[str, Any]) -> HumanReviewQueueEntry:
    return HumanReviewQueueEntry(
        id=int(row["id"]),
        task_id=row["subject"],
        run_id=row["run_id"],
        reason=row["kind"],
        detail=row["detail"],
        occurred_at=row["occurred_at"],
    )


def _row_to_source_sync_record(row: dict[str, Any]) -> SourceSyncRecord:
    return SourceSyncRecord(
        id=int(row["id"]),
        source_kind=row["source_kind"],
        source_name=row["source_name"],
        started_at=row["started_at"],
        finished_at=row["finished_at"],
        status=row["status"],
        observed_count=int(row["observed_count"]),
        error=row["error"],
        metadata_json=row["metadata_json"],
    )


def _row_to_work_item_record(row: dict[str, Any]) -> WorkItemRecord:
    return WorkItemRecord(
        task_id=row["task_id"],
        source_kind=row["source_kind"],
        source_ref=row["source_ref"],
        source_url=row["source_url"],
        source_version=row["source_version"],
        task_content_hash=row["task_content_hash"],
        priority=int(row["priority"]),
        required_capabilities_json=row["required_capabilities_json"],
        conflict_keys_json=row["conflict_keys_json"],
        first_seen_at=row["first_seen_at"],
        last_seen_at=row["last_seen_at"],
        disappeared_at=row["disappeared_at"],
        metadata_json=row["metadata_json"],
    )


class PostgresClaimStore:
    """Postgres-backed per-task lease store."""

    # Default bounded pool-acquisition wait (seconds). Mirrors
    # ``PostgresStore``: an explicit bound so pool exhaustion surfaces as a
    # bounded, TRANSIENT-classified ``PoolTimeout`` instead of an indefinite
    # hang.
    _DEFAULT_POOL_TIMEOUT_SECONDS: float = 30.0

    def __init__(
        self,
        dsn: str,
        *,
        pool_min: int = 1,
        pool_max: int = 10,
        pool_timeout: float = _DEFAULT_POOL_TIMEOUT_SECONDS,
        schema: str = "public",
    ) -> None:
        self._schema = _validate_schema(schema)
        self._schema_ident = sql.Identifier(self._schema)
        probe = psycopg.connect(dsn)
        probe.close()
        self._pool: psycopg_pool.ConnectionPool[Connection[Any]] = (
            psycopg_pool.ConnectionPool(
                dsn,
                min_size=pool_min,
                max_size=pool_max,
                timeout=pool_timeout,
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

    def _configure_connection(self, conn: Connection[Any]) -> None:
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
                cur.execute(
                    sql.SQL("SET search_path TO {}, public").format(
                        self._schema_ident
                    )
                )
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS task_claims (
                      task_id            TEXT PRIMARY KEY,
                      worker_id          TEXT NOT NULL,
                      claimed_at         TIMESTAMPTZ NOT NULL,
                      lease_expires_at   TIMESTAMPTZ NOT NULL,
                      version            INTEGER NOT NULL,
                      conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb
                    )
                    """
                )
                # Additive v3 migration: a pre-existing v1/v2 store has a
                # task_claims table predating the conflict-keys column. ADD
                # COLUMN IF NOT EXISTS adds it in place (default '[]', so every
                # existing claim row survives); new stores already have it.
                cur.execute(
                    """
                    ALTER TABLE task_claims ADD COLUMN IF NOT EXISTS
                      conflict_keys_json JSONB NOT NULL DEFAULT '[]'::jsonb
                    """
                )
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS orchestrator_schema_version (
                      id      INTEGER PRIMARY KEY CHECK (id = 1),
                      version INTEGER NOT NULL
                    )
                    """
                )
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS work_items (
                      task_id                    TEXT PRIMARY KEY,
                      source_kind                TEXT,
                      source_ref                 TEXT,
                      source_url                 TEXT,
                      source_version             TEXT,
                      task_content_hash          TEXT,
                      priority                   INTEGER NOT NULL DEFAULT 0,
                      required_capabilities_json JSONB NOT NULL
                                                 DEFAULT '[]'::jsonb,
                      conflict_keys_json         JSONB NOT NULL
                                                 DEFAULT '[]'::jsonb,
                      first_seen_at              TIMESTAMPTZ NOT NULL,
                      last_seen_at               TIMESTAMPTZ NOT NULL,
                      disappeared_at             TIMESTAMPTZ,
                      metadata_json              JSONB NOT NULL
                                                 DEFAULT '{}'::jsonb
                    )
                    """
                )
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS work_item_dependencies (
                      task_id              TEXT NOT NULL,
                      prerequisite_task_id TEXT NOT NULL,
                      created_at           TIMESTAMPTZ NOT NULL,
                      PRIMARY KEY (task_id, prerequisite_task_id)
                    )
                    """
                )
                cur.execute(
                    """
                    CREATE INDEX IF NOT EXISTS
                      idx_work_item_dependencies_prerequisite
                      ON work_item_dependencies (prerequisite_task_id)
                    """
                )
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS source_syncs (
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
                # Additive v4 migration: the append-only ledger. CREATE TABLE
                # IF NOT EXISTS materializes it on open, so a pre-existing
                # v1/v2/v3 store keeps every task_claims / work_items row and
                # simply gains an empty ledger -- no drop-and-recreate.
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS orchestrator_events (
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
                cur.execute(
                    """
                    CREATE INDEX IF NOT EXISTS idx_orchestrator_events_task
                      ON orchestrator_events (task_id, id)
                    """
                )
                # Additive v5 migration: the append-only WorkGraph snapshot
                # record (header + per-item rows). CREATE TABLE IF NOT EXISTS
                # materializes both tables on open, so a pre-existing
                # v1/v2/v3/v4 store keeps every task_claims / work_items /
                # source_syncs / orchestrator_events row and simply gains an
                # empty snapshot stream -- no drop-and-recreate.
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS graph_snapshots (
                      id            BIGSERIAL PRIMARY KEY,
                      captured_at   TIMESTAMPTZ NOT NULL,
                      item_count    INTEGER NOT NULL,
                      last_event_id BIGINT NOT NULL
                    )
                    """
                )
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS graph_snapshot_items (
                      snapshot_id                 BIGINT NOT NULL,
                      task_id                     TEXT NOT NULL,
                      source_kind                 TEXT,
                      source_ref                  TEXT,
                      source_url                  TEXT,
                      source_version              TEXT,
                      priority                    INTEGER NOT NULL DEFAULT 0,
                      required_capabilities_json  JSONB NOT NULL
                                                  DEFAULT '[]'::jsonb,
                      conflict_keys_json          JSONB NOT NULL
                                                  DEFAULT '[]'::jsonb,
                      state                       TEXT NOT NULL,
                      ready                       BOOLEAN NOT NULL,
                      claim_holder                TEXT,
                      resolved_prerequisites_json JSONB NOT NULL
                                                  DEFAULT '[]'::jsonb,
                      PRIMARY KEY (snapshot_id, task_id)
                    )
                    """
                )
                cur.execute(
                    """
                    CREATE INDEX IF NOT EXISTS
                      idx_graph_snapshot_items_snapshot
                      ON graph_snapshot_items (snapshot_id)
                    """
                )
                # Additive v6 migration: the append-only orchestrator
                # stop-event ledger (the closed pre-run dead-end taxonomy).
                # CREATE TABLE IF NOT EXISTS materializes it on open, so a
                # pre-existing v1..v5 store keeps every prior row and simply
                # gains an empty stop-event stream -- no drop-and-recreate.
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS orchestrator_stop_events (
                      id          BIGSERIAL PRIMARY KEY,
                      kind        TEXT NOT NULL,
                      subject     TEXT NOT NULL,
                      detail      TEXT NOT NULL,
                      occurred_at TIMESTAMPTZ NOT NULL,
                      run_id      TEXT NOT NULL DEFAULT ''
                    )
                    """
                )
                # Additive v7 migration: a pre-existing v6 store has an
                # orchestrator_stop_events table predating the run_id column.
                # ADD COLUMN IF NOT EXISTS adds it in place (default '', so
                # every existing stop row survives); new stores already have it.
                cur.execute(
                    """
                    ALTER TABLE orchestrator_stop_events ADD COLUMN IF NOT
                      EXISTS run_id TEXT NOT NULL DEFAULT ''
                    """
                )
                cur.execute(
                    """
                    CREATE INDEX IF NOT EXISTS
                      idx_orchestrator_stop_events_subject
                      ON orchestrator_stop_events (subject, id)
                    """
                )
                cur.execute(
                    "INSERT INTO orchestrator_schema_version (id, version) "
                    "VALUES (1, %s) ON CONFLICT (id) DO NOTHING",
                    (CURRENT_ORCH_SCHEMA_VERSION,),
                )
                # Additive forward migration v1..v6 -> v7: the WorkGraph
                # tables (v2), the conflict-keys column (v3), the
                # orchestrator_events ledger (v4), the graph_snapshots /
                # graph_snapshot_items tables (v5), the
                # orchestrator_stop_events ledger (v6), and its run_id column
                # (v7) were just materialized above, so a pre-existing store
                # keeps its task_claims / work_items / source_syncs /
                # orchestrator_events / stop rows intact and simply gains the
                # run_id column. Converge any older sentinel forward rather
                # than refusing the store; a newer-than-current version still
                # trips the mismatch guard below.
                cur.execute(
                    "UPDATE orchestrator_schema_version SET version = %s "
                    "WHERE id = 1 AND version < %s",
                    (CURRENT_ORCH_SCHEMA_VERSION, CURRENT_ORCH_SCHEMA_VERSION),
                )
                cur.execute(
                    "SELECT version FROM orchestrator_schema_version "
                    "WHERE id = 1"
                )
                row = cur.fetchone()
        observed = int(row[0]) if row is not None else None
        if observed != CURRENT_ORCH_SCHEMA_VERSION:
            raise OrchestratorSchemaError(
                observed=observed, expected=CURRENT_ORCH_SCHEMA_VERSION
            )

    def _append_event(
        self,
        cur: Cursor[Any],
        *,
        task_id: str,
        worker_id: str,
        event_type: str,
        version: int,
        lease_expires_at: datetime,
        occurred_at: datetime,
    ) -> None:
        # Append one ledger row using the caller's cursor, so the event commits
        # in the same transaction as the task_claims mutation it describes
        # (D-1): a rolled-back transition takes its event with it.
        cur.execute(
            """
            INSERT INTO orchestrator_events (
                task_id, worker_id, event_type, version,
                lease_expires_at, occurred_at
            ) VALUES (%s, %s, %s, %s, %s, %s)
            """,
            (
                task_id,
                worker_id,
                event_type,
                version,
                lease_expires_at,
                occurred_at,
            ),
        )

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
        conflict_keys: frozenset[str] = frozenset(),
    ) -> TaskClaim | None:
        lease_expires = now + timedelta(seconds=lease_seconds)
        incoming = frozenset(conflict_keys)
        keys_json = encode_str_set(incoming)
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                # Refuse on conflict-key overlap with a *different* live claim.
                # Run in the same transaction as the upsert; the task's own row
                # is excluded and lapsed claims do not block, so the refusal
                # clears once the conflicting claim is released or expires.
                if incoming:
                    cur.execute(
                        """
                        SELECT conflict_keys_json::text
                        FROM task_claims
                        WHERE task_id <> %s AND lease_expires_at > %s
                        """,
                        (task_id, now),
                    )
                    for other in cur.fetchall():
                        if decode_str_set(other[0]) & incoming:
                            return None
                # ``prev`` reads the row's pre-statement worker (statement
                # snapshot, so it never sees the upsert's own write); the upsert
                # keeps the exact ON CONFLICT optimistic-concurrency semantics.
                # No row back means the WHERE refused the update (a live,
                # different-worker lease) -> return None, write no event.
                cur.execute(
                    """
                    WITH prev AS (
                        SELECT worker_id AS prev_worker
                        FROM task_claims
                        WHERE task_id = %s
                    ),
                    upserted AS (
                        INSERT INTO task_claims (
                            task_id, worker_id, claimed_at, lease_expires_at,
                            version, conflict_keys_json
                        ) VALUES (%s, %s, %s, %s, 1, %s::jsonb)
                        ON CONFLICT (task_id) DO UPDATE SET
                            worker_id = EXCLUDED.worker_id,
                            claimed_at = EXCLUDED.claimed_at,
                            lease_expires_at = EXCLUDED.lease_expires_at,
                            version = task_claims.version + 1,
                            conflict_keys_json = EXCLUDED.conflict_keys_json
                        WHERE task_claims.lease_expires_at
                                  <= EXCLUDED.claimed_at
                           OR task_claims.worker_id = EXCLUDED.worker_id
                        RETURNING version
                    )
                    SELECT upserted.version, prev.prev_worker
                    FROM upserted LEFT JOIN prev ON true
                    """,
                    (
                        task_id,
                        task_id,
                        worker_id,
                        now,
                        lease_expires,
                        keys_json,
                    ),
                )
                row = cur.fetchone()
                if row is None:
                    return None
                new_version = int(row[0])
                prev_worker = row[1]
                # A fresh insert (no prior row) or a same-worker re-acquire is
                # ``acquired``; reclaiming a *different* worker's lapsed lease is
                # ``stolen`` (D-2) -- the live-different-worker case already
                # returned None above.
                if prev_worker is None or prev_worker == worker_id:
                    event_type = EVENT_ACQUIRED
                else:
                    event_type = EVENT_STOLEN
                self._append_event(
                    cur,
                    task_id=task_id,
                    worker_id=worker_id,
                    event_type=event_type,
                    version=new_version,
                    lease_expires_at=lease_expires,
                    occurred_at=now,
                )
        return TaskClaim(
            task_id=task_id,
            worker_id=worker_id,
            claimed_at=now,
            lease_expires_at=lease_expires,
            version=new_version,
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
                # The UPDATE and its ``renewed`` event commit together: a stale
                # token updates no row, so the raise rolls the transaction back
                # (no event is written; criterion #5).
                if row is None:
                    raise ClaimLostError(claim.task_id)
                new_version = int(row[0])
                self._append_event(
                    cur,
                    task_id=claim.task_id,
                    worker_id=claim.worker_id,
                    event_type=EVENT_RENEWED,
                    version=new_version,
                    lease_expires_at=lease_expires,
                    occurred_at=now,
                )
        return TaskClaim(
            task_id=claim.task_id,
            worker_id=claim.worker_id,
            claimed_at=claim.claimed_at,
            lease_expires_at=lease_expires,
            version=new_version,
        )

    def release_claim(
        self, claim: TaskClaim, *, now: datetime | None = None
    ) -> None:
        # The DELETE and its ``released`` event commit together. A stale /
        # already-stolen token deletes no row, so no event is written
        # (criterion #7). ``now`` is the injected occurred-at; absent a
        # caller-supplied clock it falls back to the released lease's expiry.
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "DELETE FROM task_claims "
                    "WHERE task_id = %s AND version = %s AND worker_id = %s "
                    "RETURNING task_id",
                    (claim.task_id, claim.version, claim.worker_id),
                )
                row = cur.fetchone()
                if row is not None:
                    self._append_event(
                        cur,
                        task_id=claim.task_id,
                        worker_id=claim.worker_id,
                        event_type=EVENT_RELEASED,
                        version=claim.version,
                        lease_expires_at=claim.lease_expires_at,
                        occurred_at=(
                            now if now is not None
                            else claim.lease_expires_at
                        ),
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

    def list_claims(self) -> list[TaskClaim]:
        # Every held row, same column projection as load_claim. Released
        # claims are deleted rows so absent; expiry is not filtered.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT task_id, worker_id, claimed_at, lease_expires_at,
                           version
                    FROM task_claims
                    """
                )
                rows = cur.fetchall()
        return [
            TaskClaim(
                task_id=row["task_id"],
                worker_id=row["worker_id"],
                claimed_at=row["claimed_at"],
                lease_expires_at=row["lease_expires_at"],
                version=int(row["version"]),
            )
            for row in rows
        ]

    def sweep_expired_claims(self, *, now: datetime) -> list[str]:
        # Batch-delete every lapsed row (lease_expires_at <= now) in one
        # statement, returning the freed task ids. TIMESTAMPTZ comparison is
        # temporal (not lexical), matching acquire's "lease_expires_at > now
        # means live" test. Released rows leave list_claims and are
        # immediately re-acquirable; still-valid claims are untouched. Rows are
        # ordered by task_id so the freed ids and the ``expired`` events emit
        # in the same deterministic order as the SQLite backend.
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "DELETE FROM task_claims WHERE lease_expires_at <= %s "
                    "RETURNING task_id, worker_id, version, lease_expires_at",
                    (now,),
                )
                rows = sorted(cur.fetchall(), key=lambda r: r[0])
                # One ``expired`` event per reaped claim, carrying that claim's
                # holder/version (D-2); still-valid rows were never selected.
                for row in rows:
                    self._append_event(
                        cur,
                        task_id=row[0],
                        worker_id=row[1],
                        event_type=EVENT_EXPIRED,
                        version=int(row[2]),
                        lease_expires_at=row[3],
                        occurred_at=now,
                    )
        return [row[0] for row in rows]

    def list_events(self) -> list[OrchestratorEventRecord]:
        # Global stream: every recorded event in id (insertion) order.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(_ORCH_EVENT_SELECT + " ORDER BY id")
                rows = cur.fetchall()
        return [_row_to_orchestrator_event_record(row) for row in rows]

    def list_task_events(self, task_id: str) -> list[OrchestratorEventRecord]:
        # Per-task timeline: one task's events in id order.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    _ORCH_EVENT_SELECT + " WHERE task_id = %s ORDER BY id",
                    (task_id,),
                )
                rows = cur.fetchall()
        return [_row_to_orchestrator_event_record(row) for row in rows]

    # -- orchestrator stop-event ledger (schema v6) ------------------------

    def _insert_stop_event(
        self,
        cur: Cursor[Any],
        *,
        kind: str,
        subject: str,
        detail: str,
        occurred_at: datetime,
        run_id: str = "",
    ) -> None:
        # Append one stop row using the caller's cursor, so a prepare-skip stop
        # commits atomically with the claim release it accompanies (D-3). Never
        # dedupes -- recurrence is the signal.
        cur.execute(
            """
            INSERT INTO orchestrator_stop_events
                (kind, subject, detail, occurred_at, run_id)
            VALUES (%s, %s, %s, %s, %s)
            """,
            (kind, subject, detail, occurred_at, run_id),
        )

    def record_stop_event(
        self,
        *,
        kind: str,
        subject: str,
        detail: str,
        occurred_at: datetime,
    ) -> None:
        # Audit-witness only: append one row naming the stop and its cause.
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                self._insert_stop_event(
                    cur,
                    kind=kind,
                    subject=subject,
                    detail=detail,
                    occurred_at=occurred_at,
                )

    def record_prepare_skip(
        self,
        claim: TaskClaim,
        *,
        detail: str,
        now: datetime,
    ) -> None:
        # A sandbox prepare/preflight failure: the claim release (with its
        # ``released`` event when the token still owns a row) and the
        # ``prepare-skip`` stop row commit in one transaction (D-3). The stop
        # row is written unconditionally -- the dead-end happened regardless of
        # whether the release found a matching row to delete.
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "DELETE FROM task_claims "
                    "WHERE task_id = %s AND version = %s AND worker_id = %s "
                    "RETURNING task_id",
                    (claim.task_id, claim.version, claim.worker_id),
                )
                row = cur.fetchone()
                if row is not None:
                    self._append_event(
                        cur,
                        task_id=claim.task_id,
                        worker_id=claim.worker_id,
                        event_type=EVENT_RELEASED,
                        version=claim.version,
                        lease_expires_at=claim.lease_expires_at,
                        occurred_at=now,
                    )
                self._insert_stop_event(
                    cur,
                    kind=STOP_PREPARE_SKIP,
                    subject=claim.task_id,
                    detail=detail,
                    occurred_at=now,
                )

    def list_stop_events(self) -> list[OrchestratorStopEventRecord]:
        # Global stream: every recorded stop in id (insertion) order.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(_ORCH_STOP_EVENT_SELECT + " ORDER BY id")
                rows = cur.fetchall()
        return [_row_to_orchestrator_stop_event_record(row) for row in rows]

    def list_subject_stop_events(
        self, subject: str
    ) -> list[OrchestratorStopEventRecord]:
        # Per-subject timeline: one subject's stops in id order.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    _ORCH_STOP_EVENT_SELECT
                    + " WHERE subject = %s ORDER BY id",
                    (subject,),
                )
                rows = cur.fetchall()
        return [_row_to_orchestrator_stop_event_record(row) for row in rows]

    def record_human_review(
        self,
        *,
        reason: str,
        task_id: str,
        occurred_at: datetime,
        run_id: str = "",
        detail: str = "",
    ) -> None:
        # Route one unit into the single human-review queue by appending a stop
        # row whose ``kind`` is the machine-readable ``reason`` (no new silo).
        # The reason MUST be a stable token from HUMAN_REVIEW_QUEUE_REASONS,
        # never free text. Append-only, never deduped.
        _require_review_reason(reason)
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                self._insert_stop_event(
                    cur,
                    kind=reason,
                    subject=task_id,
                    detail=detail,
                    occurred_at=occurred_at,
                    run_id=run_id,
                )

    def list_human_review_queue(self) -> list[HumanReviewQueueEntry]:
        # The single queue read: every routed unit across all kinds, in id
        # (insertion) order. Filters the shared ledger to rows whose kind is a
        # review reason (disjoint from the pre-run stop kinds). Empty -> [].
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    _ORCH_STOP_EVENT_SELECT
                    + " WHERE kind = ANY(%s) ORDER BY id",
                    (sorted(HUMAN_REVIEW_QUEUE_REASONS),),
                )
                rows = cur.fetchall()
        return [_row_to_human_review_entry(row) for row in rows]

    # -- WorkGraph snapshots (schema v5) -----------------------------------

    def record_graph_snapshot(
        self,
        items: Iterable[GraphSnapshotItem],
        *,
        captured_at: datetime,
    ) -> GraphSnapshotRecord:
        """Record one WorkGraph snapshot atomically (spec 00055, D-3).

        The header row and every item row commit in a single transaction, so a
        reader never sees a snapshot whose item rows are a subset of what it
        captured (criterion #3). ``last_event_id`` is stamped by the store as
        the live ``orchestrator_events`` max id read *inside* this transaction
        (0 when the ledger is empty, D-2), never caller-supplied, so it cannot
        drift from the ledger. An empty ``items`` still records a valid snapshot
        with item count 0 (criterion #11). Append-only: a fresh snapshot id per
        call, never overwriting an earlier one.
        """
        materialized = list(items)
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "SELECT COALESCE(MAX(id), 0) FROM orchestrator_events"
                )
                hwm_row = cur.fetchone()
                assert hwm_row is not None  # COALESCE aggregate always rows
                last_event_id = int(hwm_row[0])
                cur.execute(
                    """
                    INSERT INTO graph_snapshots
                        (captured_at, item_count, last_event_id)
                    VALUES (%s, %s, %s)
                    RETURNING id
                    """,
                    (captured_at, len(materialized), last_event_id),
                )
                id_row = cur.fetchone()
                assert id_row is not None  # RETURNING id always yields a row
                snapshot_id = int(id_row[0])
                for item in materialized:
                    cur.execute(
                        """
                        INSERT INTO graph_snapshot_items (
                            snapshot_id, task_id, source_kind, source_ref,
                            source_url, source_version, priority,
                            required_capabilities_json, conflict_keys_json,
                            state, ready, claim_holder,
                            resolved_prerequisites_json
                        ) VALUES (
                            %s, %s, %s, %s, %s, %s, %s,
                            %s::jsonb, %s::jsonb, %s, %s, %s, %s::jsonb
                        )
                        """,
                        (
                            snapshot_id,
                            item.task_id,
                            item.source_kind,
                            item.source_ref,
                            item.source_url,
                            item.source_version,
                            item.priority,
                            encode_str_set(item.required_capabilities),
                            encode_str_set(item.conflict_keys),
                            item.state,
                            item.ready,
                            item.claim_holder,
                            encode_str_set(item.resolved_prerequisites),
                        ),
                    )
        return GraphSnapshotRecord(
            id=snapshot_id,
            captured_at=captured_at,
            item_count=len(materialized),
            last_event_id=last_event_id,
        )

    def list_graph_snapshots(self) -> list[GraphSnapshotRecord]:
        # Snapshot stream: every recorded header in id (insertion) order.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(_GRAPH_SNAPSHOT_SELECT + " ORDER BY id")
                rows = cur.fetchall()
        return [_row_to_graph_snapshot_record(row) for row in rows]

    def list_graph_snapshot_items(
        self, snapshot_id: int
    ) -> list[GraphSnapshotItem]:
        # One snapshot's item rows in task_id order. Unknown id -> empty list.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    _GRAPH_SNAPSHOT_ITEM_SELECT
                    + " WHERE snapshot_id = %s ORDER BY task_id",
                    (snapshot_id,),
                )
                rows = cur.fetchall()
        return [_row_to_graph_snapshot_item(row) for row in rows]

    def latest_graph_snapshot(self) -> GraphSnapshotRecord | None:
        # Most recently recorded snapshot header; None on an empty store.
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    _GRAPH_SNAPSHOT_SELECT + " ORDER BY id DESC LIMIT 1"
                )
                row = cur.fetchone()
        return _row_to_graph_snapshot_record(row) if row is not None else None

    # -- WorkGraph persistence (schema v2) ---------------------------------

    def upsert_work_item(self, item: WorkItem, *, now: datetime) -> None:
        """Insert or refresh the ``work_items`` row for an observed item.

        ``first_seen_at`` is set only on the initial insert; ``last_seen_at``
        is set to ``now`` on every observation and any prior
        ``disappeared_at`` is cleared. ``task_content_hash`` is
        ``task_digest(item.task)`` (D-1). ``priority`` /
        ``required_capabilities_json`` / ``conflict_keys_json`` are written
        from the item's scheduling metadata (spec 00049); ``metadata_json``
        is left at its column default.
        """
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO work_items (
                        task_id, source_kind, source_ref, source_url,
                        source_version, task_content_hash, priority,
                        required_capabilities_json, conflict_keys_json,
                        first_seen_at, last_seen_at, disappeared_at
                    ) VALUES (
                        %s, %s, %s, %s, %s, %s, %s,
                        %s::jsonb, %s::jsonb, %s, %s, NULL
                    )
                    ON CONFLICT (task_id) DO UPDATE SET
                        source_kind = EXCLUDED.source_kind,
                        source_ref = EXCLUDED.source_ref,
                        source_url = EXCLUDED.source_url,
                        source_version = EXCLUDED.source_version,
                        task_content_hash = EXCLUDED.task_content_hash,
                        priority = EXCLUDED.priority,
                        required_capabilities_json =
                            EXCLUDED.required_capabilities_json,
                        conflict_keys_json = EXCLUDED.conflict_keys_json,
                        last_seen_at = EXCLUDED.last_seen_at,
                        disappeared_at = NULL
                    """,
                    (
                        item.task.id,
                        item.source_kind,
                        item.source_ref,
                        item.source_url,
                        item.source_version,
                        task_digest(item.task),
                        item.priority,
                        encode_str_set(item.required_capabilities),
                        encode_str_set(item.conflict_keys),
                        now,
                        now,
                    ),
                )

    def replace_work_item_dependencies(
        self,
        task_id: str,
        prerequisite_task_ids: Iterable[str],
        *,
        now: datetime,
    ) -> None:
        """Replace the dependency edge set for ``task_id`` with the given
        prerequisites (the current-graph edges); duplicates collapse."""
        prerequisites = list(dict.fromkeys(prerequisite_task_ids))
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "DELETE FROM work_item_dependencies WHERE task_id = %s",
                    (task_id,),
                )
                for prerequisite in prerequisites:
                    cur.execute(
                        """
                        INSERT INTO work_item_dependencies
                            (task_id, prerequisite_task_id, created_at)
                        VALUES (%s, %s, %s)
                        """,
                        (task_id, prerequisite, now),
                    )

    def mark_work_items_disappeared(
        self,
        observed_task_ids: Iterable[str],
        *,
        now: datetime,
    ) -> None:
        """Stamp ``disappeared_at`` on previously-seen items absent from the
        current observed set, without deleting any row. Items already marked
        disappeared keep their original timestamp."""
        observed = list(dict.fromkeys(observed_task_ids))
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                if observed:
                    cur.execute(
                        """
                        UPDATE work_items SET disappeared_at = %s
                        WHERE disappeared_at IS NULL
                          AND task_id <> ALL(%s)
                        """,
                        (now, observed),
                    )
                else:
                    cur.execute(
                        """
                        UPDATE work_items SET disappeared_at = %s
                        WHERE disappeared_at IS NULL
                        """,
                        (now,),
                    )

    def load_work_item(self, task_id: str) -> WorkItemRecord | None:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(_WORK_ITEM_SELECT + " WHERE task_id = %s", (task_id,))
                row = cur.fetchone()
        if row is None:
            return None
        return _row_to_work_item_record(row)

    def list_work_items(self) -> list[WorkItemRecord]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(_WORK_ITEM_SELECT)
                rows = cur.fetchall()
        return [_row_to_work_item_record(row) for row in rows]

    def load_work_item_dependencies(self, task_id: str) -> list[str]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT prerequisite_task_id FROM work_item_dependencies
                    WHERE task_id = %s ORDER BY prerequisite_task_id
                    """,
                    (task_id,),
                )
                rows = cur.fetchall()
        return [row["prerequisite_task_id"] for row in rows]

    def list_work_item_dependencies(self) -> list[tuple[str, str]]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    """
                    SELECT task_id, prerequisite_task_id
                    FROM work_item_dependencies
                    ORDER BY task_id, prerequisite_task_id
                    """
                )
                rows = cur.fetchall()
        return [(row["task_id"], row["prerequisite_task_id"]) for row in rows]

    # -- source-sync recording (schema v2) ---------------------------------

    def record_source_sync_start(
        self,
        source_kind: str,
        source_name: str,
        *,
        now: datetime,
    ) -> int:
        """Open a ``source_syncs`` row for a pass and return its id.

        The row starts ``status='running'`` with ``finished_at`` NULL; the
        returned id is handed to :meth:`record_source_sync_finish` to settle
        the row once the pass succeeds or fails.
        """
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO source_syncs (
                        source_kind, source_name, started_at, status,
                        observed_count
                    ) VALUES (%s, %s, %s, 'running', 0)
                    RETURNING id
                    """,
                    (source_kind, source_name, now),
                )
                row = cur.fetchone()
        assert row is not None  # RETURNING id always yields a row
        return int(row[0])

    def record_source_sync_finish(
        self,
        sync_id: int,
        *,
        status: str,
        observed_count: int = 0,
        error: str | None = None,
        now: datetime,
    ) -> None:
        """Settle the ``source_syncs`` row ``sync_id``.

        ``status='ok'`` carries ``observed_count`` (the number of items the
        pass observed); ``status='error'`` carries a non-empty ``error``.
        ``finished_at`` is stamped to ``now`` either way. Recording a finish
        does NOT touch ``work_items`` — the failed-pass-marks-nothing posture
        (D-3) lives in the caller, which simply skips the mark-disappeared
        step on the error path.
        """
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    UPDATE source_syncs
                    SET status = %s, observed_count = %s, error = %s,
                        finished_at = %s
                    WHERE id = %s
                    """,
                    (status, observed_count, error, now, sync_id),
                )

    def load_source_sync(self, sync_id: int) -> SourceSyncRecord | None:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(
                    _SOURCE_SYNC_SELECT + " WHERE id = %s", (sync_id,)
                )
                row = cur.fetchone()
        if row is None:
            return None
        return _row_to_source_sync_record(row)

    def list_source_syncs(self) -> list[SourceSyncRecord]:
        with self._pool.connection() as conn:
            with conn.cursor(row_factory=dict_row) as cur:
                cur.execute(_SOURCE_SYNC_SELECT + " ORDER BY id")
                rows = cur.fetchall()
        return [_row_to_source_sync_record(row) for row in rows]

    def close(self) -> None:
        self._pool.close()


__all__ = ["PostgresClaimStore"]
