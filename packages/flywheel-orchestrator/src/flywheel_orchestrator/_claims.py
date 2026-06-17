"""Per-task lease store for multi-worker mutual exclusion.

The orchestrator's *own* persistence, independent of flywheel's core store. A
claim is transient coordination state — a worker holds a lease while running a
task and releases it on completion — not audit history, so it lives here rather
than in the flywheel-core schema. The store owns only the ``task_claims`` table
(and its own ``orchestrator_schema_version`` sentinel) and can be pointed at
the same backend (one SQLite file, one Postgres) as the flywheel store; each
layer manages its own tables and never references the other's.

``task_id`` is a bare string with no foreign key by design: a claim is taken
before the task definition is recorded and deleted on completion.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import Protocol, runtime_checkable

# Bump if the orchestrator's persisted schema gains a backwards-incompatible
# change. Versioned independently of flywheel-core's schema_version so the two
# can share one backend without colliding.
CURRENT_ORCH_SCHEMA_VERSION: int = 1


class ClaimLostError(Exception):
    """Raised when ``renew_claim`` finds the claim is no longer the caller's.

    Either the lease lapsed and another worker stole the task, or the claim was
    released, so the version/worker no longer match. The caller must stop
    acting on the task — another worker now owns it.
    """

    def __init__(self, task_id: str) -> None:
        super().__init__(f"claim on task {task_id!r} lost")
        self.task_id = task_id


@dataclass(frozen=True, kw_only=True)
class TaskClaim:
    """A worker's lease on a task, mirroring one ``task_claims`` row.

    Immutable snapshot: ``acquire_claim`` / ``renew_claim`` return a fresh
    instance with the bumped ``version`` and extended ``lease_expires_at``.
    ``version`` and ``worker_id`` together are the optimistic-concurrency key
    for renew/release — a stale token (wrong version, or a different worker
    stole the task) is rejected.
    """

    task_id: str
    worker_id: str
    claimed_at: datetime
    lease_expires_at: datetime
    version: int


@runtime_checkable
class ClaimStore(Protocol):
    """Per-task lease contract for multi-worker mutual exclusion.

    At most one live claim exists per ``task_id``. A worker acquires it before
    running the task and releases it on completion; the lease's expiry lets
    another worker reclaim a task whose worker crashed.

    * ``acquire_claim`` returns a :class:`TaskClaim` when the task is free, the
      existing lease has expired (the new claim *steals* it), or the caller
      already holds it (idempotent re-acquire). It returns ``None`` when a
      *live* lease is held by a different worker. The check-and-write is atomic.
    * ``renew_claim`` extends the lease, bumping ``version``; it raises
      :class:`ClaimLostError` when the caller's token no longer matches.
    * ``release_claim`` drops the claim when the token still matches; a no-op
      if it was already stolen or released.
    * ``load_claim`` returns the current claim for a task, or ``None``.
    * ``list_claims`` enumerates every currently-held claim — one
      :class:`TaskClaim` per held row — so an operator surface can see who
      holds what without knowing task ids up front. Released claims are
      absent; expiry is not filtered (an expired-but-not-yet-stolen lease
      still appears, consistent with ``load_claim``).

    ``now`` is injected (not read from a clock) so lease expiry is
    deterministic and testable.
    """

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim | None: ...

    def renew_claim(
        self,
        claim: TaskClaim,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim: ...

    def release_claim(self, claim: TaskClaim) -> None: ...

    def load_claim(self, task_id: str) -> TaskClaim | None: ...

    def list_claims(self) -> list[TaskClaim]: ...


def _iso(ts: datetime) -> str:
    return ts.isoformat()


def _parse_iso(value: str) -> datetime:
    return datetime.fromisoformat(value)


class InMemoryClaimStore:
    """In-memory :class:`ClaimStore`. Not durable; the test substrate."""

    def __init__(self) -> None:
        self._claims: dict[str, TaskClaim] = {}

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim | None:
        existing = self._claims.get(task_id)
        free = (
            existing is None
            or existing.lease_expires_at <= now
            or existing.worker_id == worker_id
        )
        if not free:
            return None
        version = existing.version + 1 if existing is not None else 1
        claim = TaskClaim(
            task_id=task_id,
            worker_id=worker_id,
            claimed_at=now,
            lease_expires_at=now + timedelta(seconds=lease_seconds),
            version=version,
        )
        self._claims[task_id] = claim
        return claim

    def renew_claim(
        self,
        claim: TaskClaim,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim:
        existing = self._claims.get(claim.task_id)
        if (
            existing is None
            or existing.version != claim.version
            or existing.worker_id != claim.worker_id
        ):
            raise ClaimLostError(claim.task_id)
        renewed = TaskClaim(
            task_id=claim.task_id,
            worker_id=claim.worker_id,
            claimed_at=existing.claimed_at,
            lease_expires_at=now + timedelta(seconds=lease_seconds),
            version=existing.version + 1,
        )
        self._claims[claim.task_id] = renewed
        return renewed

    def release_claim(self, claim: TaskClaim) -> None:
        existing = self._claims.get(claim.task_id)
        if (
            existing is not None
            and existing.version == claim.version
            and existing.worker_id == claim.worker_id
        ):
            del self._claims[claim.task_id]

    def load_claim(self, task_id: str) -> TaskClaim | None:
        return self._claims.get(task_id)

    def list_claims(self) -> list[TaskClaim]:
        # Live held rows only; released claims were removed from the dict.
        # Expiry is not filtered, matching load_claim. TaskClaim is frozen,
        # so the stored instances are safe to return directly.
        return list(self._claims.values())

    def close(self) -> None:  # parity with the durable stores
        pass


_SCHEMA_SQL = """
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS task_claims (
  task_id          TEXT PRIMARY KEY,
  worker_id        TEXT NOT NULL,
  claimed_at       DATETIME NOT NULL,
  lease_expires_at DATETIME NOT NULL,
  version          INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS orchestrator_schema_version (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);
"""


class OrchestratorSchemaError(Exception):
    """Raised when the orchestrator's on-disk schema version mismatches."""

    def __init__(self, *, observed: int | None, expected: int) -> None:
        super().__init__(
            "orchestrator store must be re-created: "
            f"observed orchestrator_schema_version={observed!r}, "
            f"expected {expected}"
        )
        self.observed = observed
        self.expected = expected


class SqliteClaimStore:
    """SQLite :class:`ClaimStore`. Owns ``task_claims`` and its own schema
    sentinel; safe to point at the same file as ``flywheel.SqliteStore`` (each
    touches only its own tables)."""

    def __init__(self, path: str | Path = ":memory:") -> None:
        self._connection = sqlite3.connect(
            str(path), check_same_thread=False, isolation_level=None
        )
        self._connection.row_factory = sqlite3.Row
        self._bootstrap()

    def _bootstrap(self) -> None:
        conn = self._connection
        conn.executescript(_SCHEMA_SQL)
        conn.execute("PRAGMA foreign_keys = ON;")
        conn.execute("PRAGMA busy_timeout = 5000;")
        conn.execute(
            "INSERT OR IGNORE INTO orchestrator_schema_version (id, version) "
            "VALUES (1, ?)",
            (CURRENT_ORCH_SCHEMA_VERSION,),
        )
        row = conn.execute(
            "SELECT version FROM orchestrator_schema_version WHERE id = 1"
        ).fetchone()
        observed = int(row["version"]) if row is not None else None
        if observed != CURRENT_ORCH_SCHEMA_VERSION:
            raise OrchestratorSchemaError(
                observed=observed, expected=CURRENT_ORCH_SCHEMA_VERSION
            )

    @contextmanager
    def _transaction(self) -> Iterator[None]:
        self._connection.execute("BEGIN IMMEDIATE")
        try:
            yield
        except BaseException:
            self._connection.execute("ROLLBACK")
            raise
        else:
            self._connection.execute("COMMIT")

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
    ) -> TaskClaim | None:
        lease_expires = now + timedelta(seconds=lease_seconds)
        with self._transaction():
            row = self._connection.execute(
                "SELECT worker_id, lease_expires_at, version "
                "FROM task_claims WHERE task_id = ?",
                (task_id,),
            ).fetchone()
            if row is None:
                self._connection.execute(
                    "INSERT INTO task_claims (task_id, worker_id, "
                    "claimed_at, lease_expires_at, version) "
                    "VALUES (?, ?, ?, ?, 1)",
                    (task_id, worker_id, _iso(now), _iso(lease_expires)),
                )
                return TaskClaim(
                    task_id=task_id,
                    worker_id=worker_id,
                    claimed_at=now,
                    lease_expires_at=lease_expires,
                    version=1,
                )
            existing_expires = _parse_iso(row["lease_expires_at"])
            if existing_expires > now and row["worker_id"] != worker_id:
                return None
            new_version = int(row["version"]) + 1
            self._connection.execute(
                "UPDATE task_claims SET worker_id = ?, claimed_at = ?, "
                "lease_expires_at = ?, version = ? WHERE task_id = ?",
                (
                    worker_id,
                    _iso(now),
                    _iso(lease_expires),
                    new_version,
                    task_id,
                ),
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
        new_version = claim.version + 1
        cursor = self._connection.execute(
            "UPDATE task_claims SET lease_expires_at = ?, version = ? "
            "WHERE task_id = ? AND version = ? AND worker_id = ?",
            (
                _iso(lease_expires),
                new_version,
                claim.task_id,
                claim.version,
                claim.worker_id,
            ),
        )
        if cursor.rowcount != 1:
            raise ClaimLostError(claim.task_id)
        return TaskClaim(
            task_id=claim.task_id,
            worker_id=claim.worker_id,
            claimed_at=claim.claimed_at,
            lease_expires_at=lease_expires,
            version=new_version,
        )

    def release_claim(self, claim: TaskClaim) -> None:
        self._connection.execute(
            "DELETE FROM task_claims "
            "WHERE task_id = ? AND version = ? AND worker_id = ?",
            (claim.task_id, claim.version, claim.worker_id),
        )

    def load_claim(self, task_id: str) -> TaskClaim | None:
        row = self._connection.execute(
            "SELECT task_id, worker_id, claimed_at, lease_expires_at, "
            "version FROM task_claims WHERE task_id = ?",
            (task_id,),
        ).fetchone()
        if row is None:
            return None
        return TaskClaim(
            task_id=row["task_id"],
            worker_id=row["worker_id"],
            claimed_at=_parse_iso(row["claimed_at"]),
            lease_expires_at=_parse_iso(row["lease_expires_at"]),
            version=int(row["version"]),
        )

    def list_claims(self) -> list[TaskClaim]:
        # Every held row, reusing load_claim's column projection and
        # _parse_iso. Released claims are deleted rows, so absent; expiry is
        # not filtered (an expired-but-not-yet-stolen lease still appears).
        rows = self._connection.execute(
            "SELECT task_id, worker_id, claimed_at, lease_expires_at, "
            "version FROM task_claims"
        ).fetchall()
        return [
            TaskClaim(
                task_id=row["task_id"],
                worker_id=row["worker_id"],
                claimed_at=_parse_iso(row["claimed_at"]),
                lease_expires_at=_parse_iso(row["lease_expires_at"]),
                version=int(row["version"]),
            )
            for row in rows
        ]

    def close(self) -> None:
        self._connection.close()


__all__ = [
    "CURRENT_ORCH_SCHEMA_VERSION",
    "ClaimLostError",
    "ClaimStore",
    "InMemoryClaimStore",
    "SqliteClaimStore",
    "OrchestratorSchemaError",
    "TaskClaim",
]
