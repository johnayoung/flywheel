"""SQLite implementation of every store Protocol from
``flywheel.store_protocols``.

Durable persistence backend. Bootstraps schema by executing
``flywheel/_schema/persistence-schema.sql`` verbatim (including the
``PRAGMA journal_mode = WAL`` and ``PRAGMA foreign_keys = ON``
directives). The DDL is not re-derived inline — the canonical SQL file
is the single source of truth and travels with the package.

Per-connection invariants enforced on every open:

* ``PRAGMA foreign_keys = ON`` — off-by-default in SQLite, must be set
  on every new connection; re-asserted after schema bootstrap so a
  later schema-file edit cannot silently drop the guarantee.
* ``PRAGMA journal_mode = WAL`` — set by the schema script. WAL mode is
  sticky at the database level (it survives connection close on
  file-backed databases) and falls back to ``memory`` for ``:memory:``
  databases per SQLite semantics.

Optimistic concurrency on ``lifecycles.version`` is enforced via a
``WHERE version = :expected`` clause on every update; a zero-row-
affected result raises ``OptimisticConcurrencyError`` after a follow-up
existence check disambiguates ``LifecycleNotFoundError``.

``grader_results`` is append-only. The protocol surface exposes no
update or delete entry point, and DB-level ``BEFORE UPDATE`` /
``BEFORE DELETE`` triggers reject any raw mutation attempt on the
table — the implementation matches the schema header comment even if
a caller bypasses the protocol surface.
"""

from __future__ import annotations

import json
import sqlite3
from collections.abc import Iterator, Mapping, Sequence
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from importlib.resources.abc import Traversable
from importlib.resources import files
from pathlib import Path
from typing import Any, cast

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
    ClaudeSessionEntry,
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

# Page size for ``read_audit_since``: a cursor-based reader can keep
# polling, but a single call's result set is bounded.
_AUDIT_PAGE_SIZE: int = 500

# Bundled as package data so the DDL travels with the install — no
# reliance on a repo-relative ``docs/`` path that breaks under wheel
# installs or the LKG snapshot.
_SCHEMA_PATH: Traversable = (
    files("flywheel") / "_schema" / "persistence-schema.sql"
)

# Append-only enforcement on grader_results. Applied post-bootstrap so
# the canonical schema file is unchanged. ``IF NOT EXISTS`` makes the
# triggers idempotent across reopens of the same database file.
_APPEND_ONLY_TRIGGERS: str = """
CREATE TRIGGER IF NOT EXISTS grader_results_no_update
BEFORE UPDATE ON grader_results
BEGIN
    SELECT RAISE(ABORT, 'grader_results is append-only');
END;

CREATE TRIGGER IF NOT EXISTS grader_results_no_delete
BEFORE DELETE ON grader_results
BEGIN
    SELECT RAISE(ABORT, 'grader_results is append-only');
END;
"""


def _utcnow_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _iso(ts: datetime) -> str:
    return ts.isoformat()


def _parse_iso(value: str | None) -> datetime | None:
    if value is None:
        return None
    return datetime.fromisoformat(value)


def _serialize_timestamps(ts: dict[Status, datetime]) -> str:
    return json.dumps({k.value: _iso(v) for k, v in ts.items()})


def _deserialize_timestamps(blob: str) -> dict[Status, datetime]:
    raw = json.loads(blob)
    out: dict[Status, datetime] = {}
    for k, v in raw.items():
        parsed = _parse_iso(v)
        assert parsed is not None  # timestamps_json values are never null
        out[Status(k)] = parsed
    return out


def _read_schema_sql() -> str:
    return _SCHEMA_PATH.read_text(encoding="utf-8")


class SqliteStore:
    """SQLite implementation of every store protocol.

    Each instance owns a single ``sqlite3.Connection`` in autocommit
    mode (``isolation_level=None``) so each statement is its own
    transaction. WAL mode gives concurrent readers visibility while a
    writer is active.

    Parameters
    ----------
    path:
        Filesystem location of the database file, or ``":memory:"``
        (default) for an ephemeral in-process database. The schema is
        bootstrapped on construction; opening an existing file picks
        up the existing schema unchanged (all DDL uses
        ``IF NOT EXISTS``).
    """

    def __init__(
        self,
        path: str | Path = ":memory:",
        *,
        notifier: RunNotifier | None = None,
    ) -> None:
        # Default-on in-process reactivity: a consumer sharing this store
        # instance gets push wakeups; separate instances (e.g. another
        # process on the same file) fall back to the audit follower's poll.
        self.notifier: RunNotifier = notifier or RunNotifier()
        self._connection = sqlite3.connect(
            str(path),
            check_same_thread=False,
            isolation_level=None,
        )
        self._connection.row_factory = sqlite3.Row
        self._bootstrap()

    def _bootstrap(self) -> None:
        conn = self._connection
        # Pre-feature detection: a legacy database has an ``events`` table
        # without a ``sequence`` column. Re-running the new schema script
        # against it would silently no-op the ``CREATE TABLE IF NOT EXISTS``
        # and leave the legacy shape in place, so we check explicitly
        # before bootstrap and refuse with a clear error.
        if _has_table(conn, "events") and not _events_has_sequence(conn):
            raise StoreSchemaError(
                observed_version=None,
                expected_version=CURRENT_SCHEMA_VERSION,
            )
        # The canonical schema script applies the pragmas (WAL +
        # foreign_keys) and creates every table. Executed verbatim so
        # the DDL is not re-derived here.
        conn.executescript(_read_schema_sql())
        # foreign_keys is per-connection; reaffirm it so a later schema
        # edit cannot silently drop the per-connection guarantee.
        conn.execute("PRAGMA foreign_keys = ON;")
        # Multi-worker contention: when two workers write the same file
        # (each its own connection, e.g. competing for task claims), make a
        # blocked writer wait for the lock instead of erroring immediately
        # with "database is locked". Per-connection, so set on every open.
        conn.execute("PRAGMA busy_timeout = 5000;")
        # Append-only triggers on grader_results.
        conn.executescript(_APPEND_ONLY_TRIGGERS)
        # Back-compat migration: cf45b58 added blocked_requires_json to
        # lifecycles without bumping schema_version (per its commit msg,
        # nullable-add is the spec's back-compat path). Existing v2 DBs
        # created before that commit lack the column; CREATE TABLE IF NOT
        # EXISTS above no-ops on them, so add the column here when missing.
        if not _lifecycles_has_blocked_requires_json(conn):
            conn.execute(
                "ALTER TABLE lifecycles ADD COLUMN blocked_requires_json TEXT"
            )
        # Additive, same back-compat path as blocked_requires_json: the
        # events.category column distinguishes domain (state-bearing) from
        # telemetry rows. Existing v2 databases lack it; the DEFAULT means
        # every pre-existing row is correctly 'telemetry'. No version bump.
        if not _events_has_category(conn):
            conn.execute(
                "ALTER TABLE events ADD COLUMN category TEXT NOT NULL "
                "DEFAULT 'telemetry'"
            )
        # Forward migration from schema_version 3 -> 4: the
        # ``control_commands`` table (and its pending-row index) ships in
        # schema_version 4. CREATE TABLE IF NOT EXISTS above already
        # materialized it on the existing database; INSERT OR IGNORE in the
        # schema script no-ops against the pre-existing v3 row, so explicitly
        # bump the row when it is still at 3. The bump is guarded on the
        # current version so re-bootstrap of a v4 store is a no-op and a
        # mismatched future version still fails the final pin below.
        conn.execute(
            "UPDATE schema_version SET version = ? "
            "WHERE id = 1 AND version = ?",
            (4, 3),
        )
        # Forward migration from schema_version 4 -> 5: the
        # ``lifecycles.awaiting_manual_ordinal`` nullable column ships in
        # schema_version 5. CREATE TABLE IF NOT EXISTS in the schema script
        # only creates the table when absent, so an existing v4 database
        # needs an explicit ALTER to materialize the column; the column is
        # nullable so a v4 row reads NULL (the post-migration sentinel for
        # "not parked on a manual gate"). The version bump is guarded on
        # the prior version so re-bootstrap of a v5 store is a no-op.
        if not _lifecycles_has_awaiting_manual_ordinal(conn):
            conn.execute(
                "ALTER TABLE lifecycles "
                "ADD COLUMN awaiting_manual_ordinal INTEGER"
            )
        conn.execute(
            "UPDATE schema_version SET version = ? "
            "WHERE id = 1 AND version = ?",
            (CURRENT_SCHEMA_VERSION, 4),
        )
        # Final version pin: any row mismatch is fatal regardless of how
        # the database got here. The schema_version table has a CHECK
        # (id = 1) so there is at most one row to read.
        row = conn.execute(
            "SELECT version FROM schema_version WHERE id = 1"
        ).fetchone()
        observed = int(row["version"]) if row is not None else None
        if observed != CURRENT_SCHEMA_VERSION:
            raise StoreSchemaError(
                observed_version=observed,
                expected_version=CURRENT_SCHEMA_VERSION,
            )

    def _next_run_sequence(self, run_id: str) -> int:
        """Allocate the next per-run audit sequence number.

        Atomic with respect to other writers on the same connection because
        SQLite in autocommit mode serializes statements; the
        ``ON CONFLICT … RETURNING`` clause yields the newly-stored value
        in the same statement so events and SDK messages cannot race past
        each other on the same run_id.
        """
        row = self._connection.execute(
            """
            INSERT INTO run_sequence (run_id, next_seq) VALUES (?, 1)
            ON CONFLICT(run_id) DO UPDATE
                SET next_seq = next_seq + 1
            RETURNING next_seq
            """,
            (run_id,),
        ).fetchone()
        assert row is not None
        return int(row["next_seq"])

    @contextmanager
    def _transaction(self) -> Iterator[None]:
        """Run a block as one atomic unit under ``BEGIN IMMEDIATE``.

        The connection is in autocommit mode, so domain-event appends —
        which must check the version, write the projection, and append the
        event row indivisibly — issue an explicit immediate transaction.
        BEGIN IMMEDIATE acquires the write lock up front so a concurrent
        appender cannot interleave between the version check and the write
        (the SQLite analog of Postgres ``SELECT ... FOR UPDATE``).
        """
        self._connection.execute("BEGIN IMMEDIATE")
        try:
            yield
        except BaseException:
            self._connection.execute("ROLLBACK")
            raise
        else:
            self._connection.execute("COMMIT")

    def close(self) -> None:
        self._connection.close()

    # --- LifecycleStore ---------------------------------------------------

    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        try:
            self._connection.execute(
                """
                INSERT INTO lifecycles (
                    run_id, task_id, status, version, retries, error,
                    agent_output, session_id, artifacts_dir, worker_id,
                    timestamps_json, updated_at, blocked_requires_json,
                    task_content_hash, awaiting_manual_ordinal
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    lifecycle.run_id,
                    lifecycle.task_id,
                    lifecycle.status.value,
                    lifecycle.version,
                    lifecycle.retries,
                    lifecycle.error,
                    lifecycle.agent_output,
                    lifecycle.session_id,
                    lifecycle.artifacts_dir,
                    lifecycle.worker_id,
                    _serialize_timestamps(lifecycle.timestamps),
                    _utcnow_iso(),
                    lifecycle.blocked_requires_json,
                    lifecycle.task_content_hash or None,
                    lifecycle.awaiting_manual_ordinal,
                ),
            )
        except sqlite3.IntegrityError as exc:
            if "lifecycles.run_id" in str(exc) or "PRIMARY KEY" in str(exc):
                raise LifecycleAlreadyExistsError(lifecycle.run_id) from exc
            raise

    def update_lifecycle(
        self,
        lifecycle: Lifecycle,
        *,
        expected_version: int,
    ) -> None:
        cursor = self._connection.execute(
            """
            UPDATE lifecycles SET
                task_id = ?,
                status = ?,
                version = ?,
                retries = ?,
                error = ?,
                agent_output = ?,
                session_id = ?,
                artifacts_dir = ?,
                worker_id = ?,
                timestamps_json = ?,
                updated_at = ?,
                blocked_requires_json = ?,
                task_content_hash = ?,
                awaiting_manual_ordinal = ?
            WHERE run_id = ? AND version = ?
            """,
            (
                lifecycle.task_id,
                lifecycle.status.value,
                lifecycle.version,
                lifecycle.retries,
                lifecycle.error,
                lifecycle.agent_output,
                lifecycle.session_id,
                lifecycle.artifacts_dir,
                lifecycle.worker_id,
                _serialize_timestamps(lifecycle.timestamps),
                _utcnow_iso(),
                lifecycle.blocked_requires_json,
                lifecycle.task_content_hash or None,
                lifecycle.awaiting_manual_ordinal,
                lifecycle.run_id,
                expected_version,
            ),
        )
        if cursor.rowcount == 1:
            return
        # rowcount == 0 — disambiguate not-found vs version conflict.
        existing = self._connection.execute(
            "SELECT version FROM lifecycles WHERE run_id = ?",
            (lifecycle.run_id,),
        ).fetchone()
        if existing is None:
            raise LifecycleNotFoundError(lifecycle.run_id)
        raise OptimisticConcurrencyError(
            lifecycle.run_id,
            expected_version=expected_version,
            actual_version=int(existing["version"]),
        )

    def load_lifecycle(self, run_id: str) -> Lifecycle | None:
        row = self._connection.execute(
            """
            SELECT run_id, task_id, status, version, retries, error,
                   agent_output, session_id, artifacts_dir, worker_id,
                   timestamps_json, blocked_requires_json, task_content_hash,
                   awaiting_manual_ordinal
            FROM lifecycles
            WHERE run_id = ?
            """,
            (run_id,),
        ).fetchone()
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
        # INSERT OR IGNORE makes the (id, content_hash) write idempotent:
        # re-saving an unchanged task is a no-op and created_at is preserved.
        self._connection.execute(
            """
            INSERT OR IGNORE INTO tasks (
                id, content_hash, goal, graders_json, prerequisites_json,
                tags_json, context_json, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                task.id,
                content_hash,
                data["goal"],
                json.dumps(data["graders"]),
                json.dumps(data["prerequisites"]),
                json.dumps(data["tags"]),
                json.dumps(data["context"]),
                _iso(now),
            ),
        )
        return content_hash

    def load_task(
        self, task_id: str, content_hash: str | None = None
    ) -> Task | None:
        if content_hash is not None:
            row = self._connection.execute(
                "SELECT id, goal, graders_json, prerequisites_json, "
                "tags_json, context_json FROM tasks "
                "WHERE id = ? AND content_hash = ?",
                (task_id, content_hash),
            ).fetchone()
        else:
            row = self._connection.execute(
                "SELECT id, goal, graders_json, prerequisites_json, "
                "tags_json, context_json FROM tasks "
                "WHERE id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
                (task_id,),
            ).fetchone()
        if row is None:
            return None
        return _row_to_task(row)

    def load_task_for_run(self, run_id: str) -> Task | None:
        row = self._connection.execute(
            "SELECT task_id, task_content_hash FROM lifecycles "
            "WHERE run_id = ?",
            (run_id,),
        ).fetchone()
        if row is None:
            return None
        return self.load_task(row["task_id"], row["task_content_hash"] or None)

    # --- DomainEventStore -------------------------------------------------

    def append_domain_event(
        self,
        event: DomainEvent,
        *,
        expected_version: int,
    ) -> Lifecycle:
        with self._transaction():
            if isinstance(event, LifecycleInitialized):
                folded = apply(None, event)
                # create_lifecycle raises LifecycleAlreadyExistsError on a
                # duplicate seed; the transaction rolls back around it.
                self.create_lifecycle(folded)
            else:
                current = self.load_lifecycle(event.run_id)
                if current is None:
                    raise LifecycleNotFoundError(event.run_id)
                if current.version != expected_version:
                    raise OptimisticConcurrencyError(
                        event.run_id,
                        expected_version=expected_version,
                        actual_version=current.version,
                    )
                folded = apply(current, event)
                self.update_lifecycle(
                    folded, expected_version=expected_version
                )
                self._project_domain_event(event, folded)
            sequence = self._insert_domain_event_row(event)
        # Signal only after the transaction commits so a woken consumer
        # never observes a watermark for an uncommitted (or rolled-back)
        # append.
        self.notifier.notify(event.run_id, sequence)
        return folded

    def _insert_domain_event_row(self, event: DomainEvent) -> int:
        sequence = self._next_run_sequence(event.run_id)
        self._connection.execute(
            """
            INSERT INTO events (
                run_id, attempt_number, ts, kind, payload_json, sequence,
                category
            ) VALUES (?, ?, ?, ?, ?, ?, 'domain')
            """,
            (
                event.run_id,
                event.attempt_number,
                _iso(event.ts),
                event_kind(event),
                json.dumps(event_payload(event)),
                sequence,
            ),
        )
        return sequence

    def _project_domain_event(
        self, event: DomainEvent, folded: Lifecycle
    ) -> None:
        """Maintain the attempts / grader_results read-model projections.

        Called inside the append transaction so the projections commit
        atomically with the event row and the lifecycle row.
        """
        if isinstance(event, (AttemptStarted, AttemptFinalized)):
            attempt = next(
                a for a in folded.attempts if a.number == event.number
            )
            self.save_attempt(folded.run_id, attempt)
        elif isinstance(event, GraderEvaluated):
            assert event.attempt_number is not None
            self.append_grader_result(
                GraderResultRecord(
                    run_id=event.run_id,
                    attempt_number=event.attempt_number,
                    ordinal=event.ordinal,
                    grader_type=cast(GraderType, event.grader_type),
                    grader_spec=dict(event.grader_spec),
                    passed=event.passed,
                    duration_ms=event.duration_ms,
                    payload=dict(event.payload),
                    ts=event.ts,
                    grader_name=event.grader_name,
                )
            )

    def list_domain_events(self, run_id: str) -> list[DomainEvent]:
        rows = self._connection.execute(
            """
            SELECT id, run_id, attempt_number, ts, kind, payload_json,
                   sequence
            FROM events
            WHERE run_id = ? AND category = 'domain'
            ORDER BY sequence
            """,
            (run_id,),
        ).fetchall()
        return [_row_to_domain_event(r) for r in rows]

    # --- AttemptStore -----------------------------------------------------

    def save_attempt(self, run_id: str, attempt: Attempt) -> None:
        # INSERT OR REPLACE upserts on the (run_id, number) PK so the
        # harness can record both the start and the finalization of an
        # attempt without separate verbs.
        self._connection.execute(
            """
            INSERT OR REPLACE INTO attempts (
                run_id, number, attempt_run_id, started_at, ended_at,
                outcome, agent_output, error, agent_context_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                run_id,
                attempt.number,
                attempt.run_id,
                _iso(attempt.started_at),
                _iso(attempt.ended_at) if attempt.ended_at else None,
                attempt.outcome.value if attempt.outcome else None,
                attempt.agent_output,
                attempt.error,
                json.dumps(dict(attempt.agent_context)),
            ),
        )

    def load_attempt(self, run_id: str, number: int) -> Attempt | None:
        row = self._connection.execute(
            """
            SELECT number, attempt_run_id, started_at, ended_at, outcome,
                   agent_output, error, agent_context_json
            FROM attempts
            WHERE run_id = ? AND number = ?
            """,
            (run_id, number),
        ).fetchone()
        if row is None:
            return None
        return _row_to_attempt(row)

    def list_attempts(self, run_id: str) -> list[Attempt]:
        rows = self._connection.execute(
            """
            SELECT number, attempt_run_id, started_at, ended_at, outcome,
                   agent_output, error, agent_context_json
            FROM attempts
            WHERE run_id = ?
            ORDER BY number
            """,
            (run_id,),
        ).fetchall()
        return [_row_to_attempt(r) for r in rows]

    # --- EventStore -------------------------------------------------------

    def append_event(self, event: EventRecord) -> EventRecord:
        sequence = self._next_run_sequence(event.run_id)
        cursor = self._connection.execute(
            """
            INSERT INTO events (
                run_id, attempt_number, ts, kind, payload_json, sequence
            ) VALUES (?, ?, ?, ?, ?, ?)
            """,
            (
                event.run_id,
                event.attempt_number,
                _iso(event.ts),
                event.kind,
                json.dumps(dict(event.payload)),
                sequence,
            ),
        )
        self.notifier.notify(event.run_id, sequence)
        return EventRecord(
            run_id=event.run_id,
            ts=event.ts,
            kind=event.kind,
            payload=dict(event.payload),
            attempt_number=event.attempt_number,
            id=cursor.lastrowid,
            sequence=sequence,
        )

    def list_events(self, run_id: str) -> list[EventRecord]:
        rows = self._connection.execute(
            """
            SELECT id, run_id, attempt_number, ts, kind, payload_json,
                   sequence, category
            FROM events
            WHERE run_id = ? AND category = 'telemetry'
            ORDER BY ts, id
            """,
            (run_id,),
        ).fetchall()
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
        sequence = self._next_run_sequence(message.run_id)
        cursor = self._connection.execute(
            """
            INSERT INTO sdk_messages (
                run_id, attempt_number, iteration_number, sequence,
                message_type, payload_json, ts
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            """,
            (
                message.run_id,
                message.attempt_number,
                message.iteration_number,
                sequence,
                message_type,
                json.dumps(payload),
                _iso(ts),
            ),
        )
        self.notifier.notify(message.run_id, sequence)
        return SdkMessageRecord(
            run_id=message.run_id,
            attempt_number=message.attempt_number,
            iteration_number=message.iteration_number,
            message_type=message_type,
            payload=payload,
            ts=ts,
            sequence=sequence,
            id=cursor.lastrowid,
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
        rows = self._connection.execute(
            """
            SELECT id, run_id, attempt_number, iteration_number, sequence,
                   message_type, payload_json, ts
            FROM sdk_messages
            WHERE run_id = ?
            ORDER BY sequence
            """,
            (run_id,),
        ).fetchall()
        return [_row_to_sdk_message(r) for r in rows]

    # --- AuditStore -------------------------------------------------------

    def read_audit_since(
        self, run_id: str, cursor: int
    ) -> list[AuditRecord]:
        event_rows = self._connection.execute(
            """
            SELECT id, run_id, attempt_number, ts, kind, payload_json,
                   sequence, category
            FROM events
            WHERE run_id = ? AND sequence > ? AND category = 'telemetry'
            ORDER BY sequence
            LIMIT ?
            """,
            (run_id, cursor, _AUDIT_PAGE_SIZE),
        ).fetchall()
        sdk_rows = self._connection.execute(
            """
            SELECT id, run_id, attempt_number, iteration_number, sequence,
                   message_type, payload_json, ts
            FROM sdk_messages
            WHERE run_id = ? AND sequence > ?
            ORDER BY sequence
            LIMIT ?
            """,
            (run_id, cursor, _AUDIT_PAGE_SIZE),
        ).fetchall()
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
        cursor = self._connection.execute(
            """
            INSERT INTO grader_results (
                run_id, attempt_number, ordinal, grader_type, grader_name,
                grader_spec_json, passed, duration_ms, payload_json, ts
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                result.run_id,
                result.attempt_number,
                result.ordinal,
                result.grader_type,
                result.grader_name,
                json.dumps(dict(result.grader_spec)),
                1 if result.passed else 0,
                result.duration_ms,
                json.dumps(dict(result.payload)),
                _iso(result.ts),
            ),
        )
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
            id=cursor.lastrowid,
        )

    def list_grader_results(
        self,
        run_id: str,
        attempt_number: int,
    ) -> list[GraderResultRecord]:
        rows = self._connection.execute(
            """
            SELECT id, run_id, attempt_number, ordinal, grader_type,
                   grader_name, grader_spec_json, passed, duration_ms,
                   payload_json, ts
            FROM grader_results
            WHERE run_id = ? AND attempt_number = ?
            ORDER BY ordinal
            """,
            (run_id, attempt_number),
        ).fetchall()
        return [_row_to_grader_result(r) for r in rows]

    # --- AgentSessionStore ------------------------------------------------

    def append_session_entry(
        self, entry: ClaudeSessionEntry
    ) -> ClaudeSessionEntry:
        cursor = self._connection.execute(
            """
            INSERT INTO claude_session_store
                (project_key, session_id, subpath, entry, mtime)
            VALUES (?, ?, ?, ?, ?)
            """,
            (
                entry.project_key,
                entry.session_id,
                entry.subpath,
                entry.entry,
                entry.mtime,
            ),
        )
        return ClaudeSessionEntry(
            project_key=entry.project_key,
            session_id=entry.session_id,
            entry=entry.entry,
            mtime=entry.mtime,
            subpath=entry.subpath,
            seq=cursor.lastrowid,
        )

    def list_session_entries(
        self,
        project_key: str,
        session_id: str,
        subpath: str = "",
    ) -> list[ClaudeSessionEntry]:
        rows = self._connection.execute(
            """
            SELECT seq, project_key, session_id, subpath, entry, mtime
            FROM claude_session_store
            WHERE project_key = ? AND session_id = ? AND subpath = ?
            ORDER BY seq
            """,
            (project_key, session_id, subpath),
        ).fetchall()
        return [_row_to_session_entry(r) for r in rows]

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
        # BEGIN IMMEDIATE so the read-then-write is atomic: two workers
        # racing the same task cannot both observe it free.
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
            assert existing_expires is not None
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
        claimed = _parse_iso(row["claimed_at"])
        expires = _parse_iso(row["lease_expires_at"])
        assert claimed is not None and expires is not None
        return TaskClaim(
            task_id=row["task_id"],
            worker_id=row["worker_id"],
            claimed_at=claimed,
            lease_expires_at=expires,
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
        # Persist verbatim. ``claimed_at`` is left NULL so the row joins the
        # pending queue immediately; ``id`` is assigned by AUTOINCREMENT and
        # becomes the per-run enqueue-order key consumed by claim_commands.
        payload_copy = dict(payload)
        cursor = self._connection.execute(
            """
            INSERT INTO control_commands (
                run_id, kind, payload_json, enqueued_at, claimed_at
            ) VALUES (?, ?, ?, ?, NULL)
            """,
            (run_id, kind, json.dumps(payload_copy), _iso(now)),
        )
        return ControlCommandRecord(
            run_id=run_id,
            kind=kind,
            payload=payload_copy,
            enqueued_at=now,
            claimed_at=None,
            id=cursor.lastrowid,
        )

    def claim_commands(
        self,
        run_id: str,
        *,
        now: datetime,
    ) -> list[ControlCommandRecord]:
        # Claim-once is one atomic UPDATE: every still-pending row for the
        # run is flipped to claimed_at = now in a single statement, so a
        # racing claim_commands call (different worker, watcher restart)
        # sees no rows to take. ``BEGIN IMMEDIATE`` forces the writer lock
        # up front, which matches the optimistic-concurrency pattern used
        # by acquire_claim above. RETURNING yields rows in arbitrary order;
        # we sort by id afterwards to match the enqueue-order contract.
        with self._transaction():
            rows = self._connection.execute(
                """
                UPDATE control_commands
                SET claimed_at = ?
                WHERE run_id = ? AND claimed_at IS NULL
                RETURNING id, run_id, kind, payload_json, enqueued_at,
                          claimed_at
                """,
                (_iso(now), run_id),
            ).fetchall()
        records = [_row_to_control_command(r) for r in rows]
        records.sort(key=lambda r: r.id if r.id is not None else 0)
        return records


# --- Row -> dataclass converters --------------------------------------------


def _row_to_attempt(row: sqlite3.Row) -> Attempt:
    outcome_value = row["outcome"]
    started = _parse_iso(row["started_at"])
    assert started is not None  # NOT NULL in schema
    return Attempt(
        number=int(row["number"]),
        started_at=started,
        run_id=row["attempt_run_id"] or "",
        ended_at=_parse_iso(row["ended_at"]),
        outcome=Outcome(outcome_value) if outcome_value else None,
        agent_output=row["agent_output"] or "",
        error=row["error"] or "",
        agent_context=json.loads(row["agent_context_json"] or "{}"),
    )


def _row_to_task(row: sqlite3.Row) -> Task:
    return deserialize_task(
        {
            "id": row["id"],
            "goal": row["goal"],
            "graders": json.loads(row["graders_json"]),
            "prerequisites": json.loads(row["prerequisites_json"]),
            "tags": json.loads(row["tags_json"]),
            "context": json.loads(row["context_json"]),
        }
    )


def _row_to_event(row: sqlite3.Row) -> EventRecord:
    ts = _parse_iso(row["ts"])
    assert ts is not None
    sequence_raw = row["sequence"]
    return EventRecord(
        run_id=row["run_id"],
        ts=ts,
        kind=row["kind"],
        payload=json.loads(row["payload_json"]),
        attempt_number=row["attempt_number"],
        id=int(row["id"]),
        sequence=int(sequence_raw) if sequence_raw is not None else None,
        category=row["category"],
    )


def _row_to_domain_event(row: sqlite3.Row) -> DomainEvent:
    ts = _parse_iso(row["ts"])
    assert ts is not None
    sequence_raw = row["sequence"]
    return event_from_record(
        kind=row["kind"],
        payload=json.loads(row["payload_json"]),
        run_id=row["run_id"],
        ts=ts,
        attempt_number=row["attempt_number"],
        sequence=int(sequence_raw) if sequence_raw is not None else None,
        id=int(row["id"]),
    )


def _row_to_sdk_message(row: sqlite3.Row) -> SdkMessageRecord:
    ts = _parse_iso(row["ts"])
    assert ts is not None
    return SdkMessageRecord(
        run_id=row["run_id"],
        attempt_number=int(row["attempt_number"]),
        iteration_number=int(row["iteration_number"]),
        message_type=row["message_type"],
        payload=json.loads(row["payload_json"]),
        ts=ts,
        sequence=int(row["sequence"]),
        id=int(row["id"]),
    )


def _has_table(conn: sqlite3.Connection, table: str) -> bool:
    return (
        conn.execute(
            "SELECT name FROM sqlite_master "
            "WHERE type='table' AND name=?",
            (table,),
        ).fetchone()
        is not None
    )


def _events_has_sequence(conn: sqlite3.Connection) -> bool:
    rows = conn.execute("PRAGMA table_info(events)").fetchall()
    return any(r["name"] == "sequence" for r in rows)


def _events_has_category(conn: sqlite3.Connection) -> bool:
    rows = conn.execute("PRAGMA table_info(events)").fetchall()
    return any(r["name"] == "category" for r in rows)


def _lifecycles_has_blocked_requires_json(conn: sqlite3.Connection) -> bool:
    rows = conn.execute("PRAGMA table_info(lifecycles)").fetchall()
    return any(r["name"] == "blocked_requires_json" for r in rows)


def _lifecycles_has_awaiting_manual_ordinal(conn: sqlite3.Connection) -> bool:
    rows = conn.execute("PRAGMA table_info(lifecycles)").fetchall()
    return any(r["name"] == "awaiting_manual_ordinal" for r in rows)


def _row_to_grader_result(row: sqlite3.Row) -> GraderResultRecord:
    ts = _parse_iso(row["ts"])
    assert ts is not None
    return GraderResultRecord(
        run_id=row["run_id"],
        attempt_number=int(row["attempt_number"]),
        ordinal=int(row["ordinal"]),
        grader_type=cast(GraderType, row["grader_type"]),
        grader_spec=json.loads(row["grader_spec_json"]),
        passed=bool(row["passed"]),
        duration_ms=int(row["duration_ms"]),
        payload=json.loads(row["payload_json"]),
        ts=ts,
        grader_name=row["grader_name"],
        id=int(row["id"]),
    )


def _row_to_session_entry(row: sqlite3.Row) -> ClaudeSessionEntry:
    return ClaudeSessionEntry(
        project_key=row["project_key"],
        session_id=row["session_id"],
        entry=row["entry"],
        mtime=int(row["mtime"]),
        subpath=row["subpath"],
        seq=int(row["seq"]),
    )


def _row_to_control_command(row: sqlite3.Row) -> ControlCommandRecord:
    enqueued = _parse_iso(row["enqueued_at"])
    assert enqueued is not None  # NOT NULL in schema
    return ControlCommandRecord(
        run_id=row["run_id"],
        kind=row["kind"],
        payload=json.loads(row["payload_json"]),
        enqueued_at=enqueued,
        claimed_at=_parse_iso(row["claimed_at"]),
        id=int(row["id"]),
    )


__all__ = ["SqliteStore"]
