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
from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from importlib.resources.abc import Traversable
from importlib.resources import files
from pathlib import Path
from typing import Any, cast

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

    def __init__(self, path: str | Path = ":memory:") -> None:
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
                    timestamps_json, updated_at, blocked_requires_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                blocked_requires_json = ?
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
                   timestamps_json, blocked_requires_json
            FROM lifecycles
            WHERE run_id = ?
            """,
            (run_id,),
        ).fetchone()
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
            blocked_requires_json=row["blocked_requires_json"],
        )
        lc.attempts = self.list_attempts(run_id)
        return lc

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
                   sequence
            FROM events
            WHERE run_id = ?
            ORDER BY ts, id
            """,
            (run_id,),
        ).fetchall()
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
        for msg in messages:
            payload = dict(msg)
            message_type = str(
                payload.get("message_type", payload.get("type", ""))
            )
            ts = datetime.now(timezone.utc)
            sequence = self._next_run_sequence(run_id)
            cursor = self._connection.execute(
                """
                INSERT INTO sdk_messages (
                    run_id, attempt_number, iteration_number, sequence,
                    message_type, payload_json, ts
                ) VALUES (?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    run_id,
                    attempt_number,
                    iteration_number,
                    sequence,
                    message_type,
                    json.dumps(payload),
                    _iso(ts),
                ),
            )
            persisted.append(
                SdkMessageRecord(
                    run_id=run_id,
                    attempt_number=attempt_number,
                    iteration_number=iteration_number,
                    message_type=message_type,
                    payload=payload,
                    ts=ts,
                    sequence=sequence,
                    id=cursor.lastrowid,
                )
            )
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
                   sequence
            FROM events
            WHERE run_id = ? AND sequence > ?
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


def _lifecycles_has_blocked_requires_json(conn: sqlite3.Connection) -> bool:
    rows = conn.execute("PRAGMA table_info(lifecycles)").fetchall()
    return any(r["name"] == "blocked_requires_json" for r in rows)


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


__all__ = ["SqliteStore"]
