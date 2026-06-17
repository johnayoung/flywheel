"""Postgres :class:`ClaimStore`. Behind the ``postgres`` extra.

Mirrors ``flywheel_core.store_postgres`` operationally — a pooled, schema-isolated
connection — but owns only ``task_claims`` and its own
``orchestrator_schema_version`` sentinel, so it can share a database (under the
same or a different schema) with flywheel's Postgres store without either
touching the other's tables.
"""

from __future__ import annotations

import re
from datetime import datetime, timedelta
from typing import Any

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

from flywheel_orchestrator._claims import (
    CURRENT_ORCH_SCHEMA_VERSION,
    ClaimLostError,
    OrchestratorSchemaError,
    TaskClaim,
)

_SCHEMA_NAME_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def _validate_schema(schema: str) -> str:
    if not isinstance(schema, str) or not _SCHEMA_NAME_RE.match(schema):
        raise ValueError(
            f"invalid schema name {schema!r}: must match "
            f"[A-Za-z_][A-Za-z0-9_]*"
        )
    return schema


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
                    "INSERT INTO orchestrator_schema_version (id, version) "
                    "VALUES (1, %s) ON CONFLICT (id) DO NOTHING",
                    (CURRENT_ORCH_SCHEMA_VERSION,),
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

    def close(self) -> None:
        self._pool.close()


__all__ = ["PostgresClaimStore"]
