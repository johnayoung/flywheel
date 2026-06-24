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
    from psycopg import Connection, sql
    from psycopg.rows import dict_row
except ImportError as exc:  # pragma: no cover - exercised via env
    raise ImportError(
        "flywheel_orchestrator.store_postgres requires the 'postgres' extra; "
        "install psycopg / psycopg_pool"
    ) from exc

from flywheel_core.loaders import task_digest

from flywheel_orchestrator._claims import (
    CURRENT_ORCH_SCHEMA_VERSION,
    ClaimLostError,
    OrchestratorSchemaError,
    SourceSyncRecord,
    TaskClaim,
    WorkItemRecord,
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

    def __init__(
        self,
        dsn: str,
        *,
        pool_min: int = 1,
        pool_max: int = 10,
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
                      task_id          TEXT PRIMARY KEY,
                      worker_id        TEXT NOT NULL,
                      claimed_at       TIMESTAMPTZ NOT NULL,
                      lease_expires_at TIMESTAMPTZ NOT NULL,
                      version          INTEGER NOT NULL
                    )
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
                cur.execute(
                    "INSERT INTO orchestrator_schema_version (id, version) "
                    "VALUES (1, %s) ON CONFLICT (id) DO NOTHING",
                    (CURRENT_ORCH_SCHEMA_VERSION,),
                )
                # Additive forward migration v1 -> v2: the new WorkGraph
                # tables were just created above, so a pre-existing v1 store
                # gains them with its task_claims rows intact. Converge the
                # sentinel rather than refusing the store.
                cur.execute(
                    "UPDATE orchestrator_schema_version SET version = 2 "
                    "WHERE id = 1 AND version = 1"
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

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim | None:
        lease_expires = now + timedelta(seconds=lease_seconds)
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

    # -- WorkGraph persistence (schema v2) ---------------------------------

    def upsert_work_item(self, item: WorkItem, *, now: datetime) -> None:
        """Insert or refresh the ``work_items`` row for an observed item.

        ``first_seen_at`` is set only on the initial insert; ``last_seen_at``
        is set to ``now`` on every observation and any prior
        ``disappeared_at`` is cleared. ``task_content_hash`` is
        ``task_digest(item.task)`` (D-1). The forward-compat columns are left
        at their column defaults.
        """
        with self._pool.connection() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO work_items (
                        task_id, source_kind, source_ref, source_url,
                        source_version, task_content_hash, first_seen_at,
                        last_seen_at, disappeared_at
                    ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, NULL)
                    ON CONFLICT (task_id) DO UPDATE SET
                        source_kind = EXCLUDED.source_kind,
                        source_ref = EXCLUDED.source_ref,
                        source_url = EXCLUDED.source_url,
                        source_version = EXCLUDED.source_version,
                        task_content_hash = EXCLUDED.task_content_hash,
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
