"""SQLite implementation of every store Protocol from
``flywheel.store_protocols``.

Durable persistence backend. Bootstraps schema by executing
``docs/persistence-schema.sql`` verbatim (including the
``PRAGMA journal_mode = WAL`` and ``PRAGMA foreign_keys = ON``
directives). The DDL is not re-derived inline — the canonical SQL file
is the single source of truth.

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
from datetime import datetime, timezone
from pathlib import Path
from typing import cast

from flywheel.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel.store_protocols import (
    ClaudeSessionEntry,
    EventRecord,
    GraderResultRecord,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    OptimisticConcurrencyError,
)

# Resolved at import time so the schema file is found via the editable
# package layout (``src/flywheel`` -> repo root contains ``docs/``).
_SCHEMA_PATH: Path = (
    Path(__file__).resolve().parents[2] / "docs" / "persistence-schema.sql"
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
        # The canonical schema script applies the pragmas (WAL +
        # foreign_keys) and creates every table. Executed verbatim so
        # the DDL is not re-derived here.
        conn.executescript(_read_schema_sql())
        # foreign_keys is per-connection; reaffirm it so a later schema
        # edit cannot silently drop the per-connection guarantee.
        conn.execute("PRAGMA foreign_keys = ON;")
        # Append-only triggers on grader_results.
        conn.executescript(_APPEND_ONLY_TRIGGERS)

    def close(self) -> None:
        self._connection.close()

    # --- LifecycleStore ---------------------------------------------------

    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        try:
            self._connection.execute(
                """
                INSERT INTO lifecycles (
                    run_id, task_id, status, version, retries, error,
                    agent_output, implementation_notes, session_id,
                    artifacts_dir, worker_id, timestamps_json, updated_at
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
                    None,
                    lifecycle.session_id,
                    lifecycle.artifacts_dir,
                    lifecycle.worker_id,
                    _serialize_timestamps(lifecycle.timestamps),
                    _utcnow_iso(),
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
                updated_at = ?
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
                   timestamps_json
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
        cursor = self._connection.execute(
            """
            INSERT INTO events (run_id, attempt_number, ts, kind, payload_json)
            VALUES (?, ?, ?, ?, ?)
            """,
            (
                event.run_id,
                event.attempt_number,
                _iso(event.ts),
                event.kind,
                json.dumps(dict(event.payload)),
            ),
        )
        return EventRecord(
            run_id=event.run_id,
            ts=event.ts,
            kind=event.kind,
            payload=dict(event.payload),
            attempt_number=event.attempt_number,
            id=cursor.lastrowid,
        )

    def list_events(self, run_id: str) -> list[EventRecord]:
        rows = self._connection.execute(
            """
            SELECT id, run_id, attempt_number, ts, kind, payload_json
            FROM events
            WHERE run_id = ?
            ORDER BY ts, id
            """,
            (run_id,),
        ).fetchall()
        return [_row_to_event(r) for r in rows]

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
    return EventRecord(
        run_id=row["run_id"],
        ts=ts,
        kind=row["kind"],
        payload=json.loads(row["payload_json"]),
        attempt_number=row["attempt_number"],
        id=int(row["id"]),
    )


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
