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

import json
import sqlite3
from collections.abc import Iterable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Protocol, runtime_checkable

from flywheel_core.loaders import task_digest

if TYPE_CHECKING:
    from flywheel_orchestrator._sources import WorkItem

# Bump if the orchestrator's persisted schema gains a backwards-incompatible
# change. Versioned independently of flywheel-core's schema_version so the two
# can share one backend without colliding.
#
# v2 adds the additive WorkGraph persistence tables (``work_items``,
# ``work_item_dependencies``, and ``source_syncs``); the 1 -> 2 bump is a pure
# forward migration (``CREATE TABLE IF NOT EXISTS`` on open), so a pre-existing
# v1 store keeps its ``task_claims`` rows. All three tables are part of the same
# v2 DDL block, so a store opened at v2 by an earlier build that predates the
# ``source_syncs`` table gains it on the next open without a further bump.
#
# v3 adds the additive ``task_claims.conflict_keys_json`` column (spec 00049,
# D-4/D-5): each live claim records its item's conflict keys so ``acquire_claim``
# can refuse an item overlapping a different live claim. The bump is additive --
# pre-existing v1/v2 stores gain the column via ``ALTER TABLE ADD COLUMN`` (the
# column defaults to ``'[]'``, so every surviving claim row keeps its data) and
# converge their sentinel forward; no drop-and-recreate, no hard mismatch.
CURRENT_ORCH_SCHEMA_VERSION: int = 3


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


@dataclass(frozen=True, kw_only=True)
class WorkItemRecord:
    """A persisted ``work_items`` row read back from the orchestrator store.

    Immutable snapshot of one observed work item's catalog entry.
    ``first_seen_at`` is stamped once on the first observation and never
    moves; ``last_seen_at`` advances on every observation; ``disappeared_at``
    is set when a *successful* sync no longer observes the item and is cleared
    (back to ``None``) the moment it is observed again.

    ``priority`` / ``required_capabilities_json`` / ``conflict_keys_json``
    carry the item's scheduling metadata (spec 00049): an integer priority
    and two canonical (sorted) JSON-array string sets, written from the
    matching ``WorkItem`` fields. ``metadata_json`` remains a forward-compat
    column carried at its default (``'{}'``); nothing populates it yet. The
    ``*_json`` fields are canonical JSON strings on both backends.
    """

    task_id: str
    source_kind: str | None
    source_ref: str | None
    source_url: str | None
    source_version: str | None
    task_content_hash: str | None
    priority: int
    required_capabilities_json: str
    conflict_keys_json: str
    first_seen_at: datetime
    last_seen_at: datetime
    disappeared_at: datetime | None
    metadata_json: str


@dataclass(frozen=True, kw_only=True)
class SourceSyncRecord:
    """A persisted ``source_syncs`` row read back from the orchestrator store.

    Immutable snapshot of one sync run over a :class:`WorkSource`. A row is
    written at the start of a pass (``status='running'``, ``finished_at`` NULL)
    and finished when the pass settles: ``status='ok'`` with ``observed_count``
    equal to the number of items the pass observed, or ``status='error'`` with
    a non-empty ``error`` when ``list_work()`` failed. ``source_name`` is the
    source's locus (D-4): the ``tasks_dir`` path for a directory source, the
    ``owner/repo`` for a GitHub source. ``metadata_json`` is a canonical JSON
    string carried at its default (``'{}'``); nothing populates it this spec.
    """

    id: int
    source_kind: str
    source_name: str
    started_at: datetime
    finished_at: datetime | None
    status: str
    observed_count: int
    error: str | None
    metadata_json: str


@runtime_checkable
class ClaimStore(Protocol):
    """Per-task lease contract for multi-worker mutual exclusion.

    At most one live claim exists per ``task_id``. A worker acquires it before
    running the task and releases it on completion; the lease's expiry lets
    another worker reclaim a task whose worker crashed.

    * ``acquire_claim`` returns a :class:`TaskClaim` when the task is free, the
      existing lease has expired (the new claim *steals* it), or the caller
      already holds it (idempotent re-acquire). It returns ``None`` when a
      *live* lease is held by a different worker, or when the item's
      ``conflict_keys`` overlap those of a *different* live claim (so two
      conflicting items never hold concurrent claims; spec 00049 D-3/D-4). The
      refusal clears once the conflicting claim is released or its lease lapses.
      ``conflict_keys`` defaults to empty, in which case acquisition is never
      refused on a conflict basis. The check-and-write is atomic.
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
        conflict_keys: frozenset[str] = frozenset(),
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


def encode_str_set(values: frozenset[str]) -> str:
    """Canonical JSON-array encoding of a string set (order-insensitive).

    Sorted so the persisted ``*_json`` value is deterministic regardless of
    set iteration order; the read-back decodes back to the same set. Shared by
    both backends so SQLite TEXT and Postgres JSONB store identical content.
    """
    return json.dumps(sorted(values))


def decode_str_set(value: str) -> frozenset[str]:
    """Inverse of :func:`encode_str_set` -- a canonical JSON array to a set.

    Shared by both backends so the conflict-key overlap check reads identical
    content whether the column was stored as SQLite TEXT or Postgres JSONB.
    """
    return frozenset(json.loads(value))


def _parse_iso(value: str) -> datetime:
    return datetime.fromisoformat(value)


def _row_to_work_item_record(row: sqlite3.Row) -> WorkItemRecord:
    disappeared = row["disappeared_at"]
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
        first_seen_at=_parse_iso(row["first_seen_at"]),
        last_seen_at=_parse_iso(row["last_seen_at"]),
        disappeared_at=(
            _parse_iso(disappeared) if disappeared is not None else None
        ),
        metadata_json=row["metadata_json"],
    )


def _row_to_source_sync_record(row: sqlite3.Row) -> SourceSyncRecord:
    finished = row["finished_at"]
    return SourceSyncRecord(
        id=int(row["id"]),
        source_kind=row["source_kind"],
        source_name=row["source_name"],
        started_at=_parse_iso(row["started_at"]),
        finished_at=_parse_iso(finished) if finished is not None else None,
        status=row["status"],
        observed_count=int(row["observed_count"]),
        error=row["error"],
        metadata_json=row["metadata_json"],
    )


class InMemoryClaimStore:
    """In-memory :class:`ClaimStore`. Not durable; the test substrate."""

    def __init__(self) -> None:
        self._claims: dict[str, TaskClaim] = {}
        self._conflict_keys: dict[str, frozenset[str]] = {}

    def acquire_claim(
        self,
        task_id: str,
        worker_id: str,
        *,
        now: datetime,
        lease_seconds: float,
        conflict_keys: frozenset[str] = frozenset(),
    ) -> TaskClaim | None:
        existing = self._claims.get(task_id)
        free = (
            existing is None
            or existing.lease_expires_at <= now
            or existing.worker_id == worker_id
        )
        if not free:
            return None
        incoming = frozenset(conflict_keys)
        if incoming and self._has_conflicting_live_claim(
            task_id, incoming, now=now
        ):
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
        self._conflict_keys[task_id] = incoming
        return claim

    def _has_conflicting_live_claim(
        self,
        task_id: str,
        incoming: frozenset[str],
        *,
        now: datetime,
    ) -> bool:
        # A *different* live claim (another task whose lease has not lapsed)
        # whose conflict keys overlap the incoming set blocks the acquire.
        for other_id, other_claim in self._claims.items():
            if other_id == task_id:
                continue
            if other_claim.lease_expires_at <= now:
                continue
            if self._conflict_keys.get(other_id, frozenset()) & incoming:
                return True
        return False

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
            self._conflict_keys.pop(claim.task_id, None)

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
  task_id            TEXT PRIMARY KEY,
  worker_id          TEXT NOT NULL,
  claimed_at         DATETIME NOT NULL,
  lease_expires_at   DATETIME NOT NULL,
  version            INTEGER NOT NULL,
  conflict_keys_json TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS orchestrator_schema_version (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS work_items (
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

CREATE TABLE IF NOT EXISTS work_item_dependencies (
  task_id              TEXT NOT NULL,
  prerequisite_task_id TEXT NOT NULL,
  created_at           TEXT NOT NULL,
  PRIMARY KEY (task_id, prerequisite_task_id)
);

CREATE INDEX IF NOT EXISTS idx_work_item_dependencies_prerequisite
  ON work_item_dependencies (prerequisite_task_id);

CREATE TABLE IF NOT EXISTS source_syncs (
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
        # Additive v3 migration: a pre-existing v1/v2 store has a task_claims
        # table that predates the conflict-keys column. CREATE TABLE IF NOT
        # EXISTS leaves that older table untouched, so add the column in place
        # (defaulting to '[]', preserving every existing claim row). New stores
        # already have it from the CREATE TABLE above, so the ALTER is skipped.
        columns = {
            str(r["name"])
            for r in conn.execute("PRAGMA table_info(task_claims)").fetchall()
        }
        if "conflict_keys_json" not in columns:
            conn.execute(
                "ALTER TABLE task_claims "
                "ADD COLUMN conflict_keys_json TEXT NOT NULL DEFAULT '[]'"
            )
        conn.execute(
            "INSERT OR IGNORE INTO orchestrator_schema_version (id, version) "
            "VALUES (1, ?)",
            (CURRENT_ORCH_SCHEMA_VERSION,),
        )
        row = conn.execute(
            "SELECT version FROM orchestrator_schema_version WHERE id = 1"
        ).fetchone()
        observed = int(row["version"]) if row is not None else None
        # Additive forward migration v1/v2 -> v3: the WorkGraph tables (v2) and
        # the conflict-keys column (v3) were already materialized above, so a
        # pre-existing store keeps its task_claims/work_items rows intact.
        # Converge the sentinel forward rather than refusing the store; a
        # newer-than-current version still trips the mismatch guard below.
        if observed is not None and observed < CURRENT_ORCH_SCHEMA_VERSION:
            conn.execute(
                "UPDATE orchestrator_schema_version SET version = ? "
                "WHERE id = 1",
                (CURRENT_ORCH_SCHEMA_VERSION,),
            )
            observed = CURRENT_ORCH_SCHEMA_VERSION
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
        conflict_keys: frozenset[str] = frozenset(),
    ) -> TaskClaim | None:
        lease_expires = now + timedelta(seconds=lease_seconds)
        incoming = frozenset(conflict_keys)
        keys_json = encode_str_set(incoming)
        with self._transaction():
            row = self._connection.execute(
                "SELECT worker_id, lease_expires_at, version "
                "FROM task_claims WHERE task_id = ?",
                (task_id,),
            ).fetchone()
            if (
                row is not None
                and _parse_iso(row["lease_expires_at"]) > now
                and row["worker_id"] != worker_id
            ):
                return None
            # Refuse on conflict-key overlap with a *different* live claim. The
            # task's own row (re-acquire / expiry-steal of the same task_id) is
            # excluded, and lapsed claims do not block -- so the refusal clears
            # once the conflicting claim is released or its lease expires.
            if incoming and self._has_conflicting_live_claim(
                task_id, incoming, now=now
            ):
                return None
            if row is None:
                self._connection.execute(
                    "INSERT INTO task_claims (task_id, worker_id, "
                    "claimed_at, lease_expires_at, version, "
                    "conflict_keys_json) VALUES (?, ?, ?, ?, 1, ?)",
                    (
                        task_id,
                        worker_id,
                        _iso(now),
                        _iso(lease_expires),
                        keys_json,
                    ),
                )
                return TaskClaim(
                    task_id=task_id,
                    worker_id=worker_id,
                    claimed_at=now,
                    lease_expires_at=lease_expires,
                    version=1,
                )
            new_version = int(row["version"]) + 1
            self._connection.execute(
                "UPDATE task_claims SET worker_id = ?, claimed_at = ?, "
                "lease_expires_at = ?, version = ?, conflict_keys_json = ? "
                "WHERE task_id = ?",
                (
                    worker_id,
                    _iso(now),
                    _iso(lease_expires),
                    new_version,
                    keys_json,
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

    def _has_conflicting_live_claim(
        self,
        task_id: str,
        incoming: frozenset[str],
        *,
        now: datetime,
    ) -> bool:
        # A *different* live claim (another task_id whose lease has not lapsed)
        # whose stored conflict keys overlap the incoming set blocks acquire.
        rows = self._connection.execute(
            "SELECT conflict_keys_json FROM task_claims "
            "WHERE task_id != ? AND lease_expires_at > ?",
            (task_id, _iso(now)),
        ).fetchall()
        for row in rows:
            if decode_str_set(row["conflict_keys_json"]) & incoming:
                return True
        return False

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
        with self._transaction():
            self._connection.execute(
                "INSERT INTO work_items ("
                "  task_id, source_kind, source_ref, source_url, "
                "  source_version, task_content_hash, priority, "
                "  required_capabilities_json, conflict_keys_json, "
                "  first_seen_at, last_seen_at, disappeared_at"
                ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL) "
                "ON CONFLICT(task_id) DO UPDATE SET "
                "  source_kind = excluded.source_kind, "
                "  source_ref = excluded.source_ref, "
                "  source_url = excluded.source_url, "
                "  source_version = excluded.source_version, "
                "  task_content_hash = excluded.task_content_hash, "
                "  priority = excluded.priority, "
                "  required_capabilities_json = "
                "    excluded.required_capabilities_json, "
                "  conflict_keys_json = excluded.conflict_keys_json, "
                "  last_seen_at = excluded.last_seen_at, "
                "  disappeared_at = NULL",
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
                    _iso(now),
                    _iso(now),
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
        with self._transaction():
            self._connection.execute(
                "DELETE FROM work_item_dependencies WHERE task_id = ?",
                (task_id,),
            )
            for prerequisite in dict.fromkeys(prerequisite_task_ids):
                self._connection.execute(
                    "INSERT INTO work_item_dependencies "
                    "(task_id, prerequisite_task_id, created_at) "
                    "VALUES (?, ?, ?)",
                    (task_id, prerequisite, _iso(now)),
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
        with self._transaction():
            if observed:
                placeholders = ",".join("?" for _ in observed)
                self._connection.execute(
                    "UPDATE work_items SET disappeared_at = ? "
                    "WHERE disappeared_at IS NULL "
                    f"AND task_id NOT IN ({placeholders})",
                    (_iso(now), *observed),
                )
            else:
                self._connection.execute(
                    "UPDATE work_items SET disappeared_at = ? "
                    "WHERE disappeared_at IS NULL",
                    (_iso(now),),
                )

    def load_work_item(self, task_id: str) -> WorkItemRecord | None:
        row = self._connection.execute(
            "SELECT task_id, source_kind, source_ref, source_url, "
            "source_version, task_content_hash, priority, "
            "required_capabilities_json, conflict_keys_json, first_seen_at, "
            "last_seen_at, disappeared_at, metadata_json "
            "FROM work_items WHERE task_id = ?",
            (task_id,),
        ).fetchone()
        if row is None:
            return None
        return _row_to_work_item_record(row)

    def list_work_items(self) -> list[WorkItemRecord]:
        rows = self._connection.execute(
            "SELECT task_id, source_kind, source_ref, source_url, "
            "source_version, task_content_hash, priority, "
            "required_capabilities_json, conflict_keys_json, first_seen_at, "
            "last_seen_at, disappeared_at, metadata_json "
            "FROM work_items"
        ).fetchall()
        return [_row_to_work_item_record(row) for row in rows]

    def load_work_item_dependencies(self, task_id: str) -> list[str]:
        rows = self._connection.execute(
            "SELECT prerequisite_task_id FROM work_item_dependencies "
            "WHERE task_id = ? ORDER BY prerequisite_task_id",
            (task_id,),
        ).fetchall()
        return [row["prerequisite_task_id"] for row in rows]

    def list_work_item_dependencies(self) -> list[tuple[str, str]]:
        rows = self._connection.execute(
            "SELECT task_id, prerequisite_task_id FROM work_item_dependencies "
            "ORDER BY task_id, prerequisite_task_id"
        ).fetchall()
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
        with self._transaction():
            cursor = self._connection.execute(
                "INSERT INTO source_syncs ("
                "  source_kind, source_name, started_at, status, "
                "  observed_count"
                ") VALUES (?, ?, ?, 'running', 0)",
                (source_kind, source_name, _iso(now)),
            )
            row_id = cursor.lastrowid
            assert row_id is not None  # AUTOINCREMENT INSERT always sets it
            return int(row_id)

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
        with self._transaction():
            self._connection.execute(
                "UPDATE source_syncs SET status = ?, observed_count = ?, "
                "error = ?, finished_at = ? WHERE id = ?",
                (status, observed_count, error, _iso(now), sync_id),
            )

    def load_source_sync(self, sync_id: int) -> SourceSyncRecord | None:
        row = self._connection.execute(
            "SELECT id, source_kind, source_name, started_at, finished_at, "
            "status, observed_count, error, metadata_json "
            "FROM source_syncs WHERE id = ?",
            (sync_id,),
        ).fetchone()
        if row is None:
            return None
        return _row_to_source_sync_record(row)

    def list_source_syncs(self) -> list[SourceSyncRecord]:
        rows = self._connection.execute(
            "SELECT id, source_kind, source_name, started_at, finished_at, "
            "status, observed_count, error, metadata_json "
            "FROM source_syncs ORDER BY id"
        ).fetchall()
        return [_row_to_source_sync_record(row) for row in rows]

    def close(self) -> None:
        self._connection.close()


__all__ = [
    "CURRENT_ORCH_SCHEMA_VERSION",
    "ClaimLostError",
    "ClaimStore",
    "InMemoryClaimStore",
    "SqliteClaimStore",
    "OrchestratorSchemaError",
    "SourceSyncRecord",
    "TaskClaim",
    "WorkItemRecord",
]
