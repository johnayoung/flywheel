"""SQLite implementation of every store Protocol from
``flywheel_core.store_protocols``.

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
from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from datetime import datetime, timezone
from importlib.resources.abc import Traversable
from importlib.resources import files
from pathlib import Path
from typing import Any, cast

from flywheel_core.event_serde import event_from_record, event_kind, event_payload
from flywheel_core.events import (
    AttemptFinalized,
    AttemptStarted,
    DomainEvent,
    GraderEvaluated,
    LifecycleInitialized,
    apply,
)
from flywheel_core.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel_core.loaders import deserialize_task, serialize_task, task_digest
from flywheel_core.notifier import RunNotifier
from flywheel_core.task import Task
from flywheel_core.store_protocols import (
    CURRENT_SCHEMA_VERSION,
    ControlCommandRecord,
    GraderResultRecord,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    OptimisticConcurrencyError,
    StoreSchemaError,
)

# Bundled as package data so the DDL travels with the install — no
# reliance on a repo-relative ``docs/`` path that breaks under wheel
# installs or the LKG snapshot.
_SCHEMA_PATH: Traversable = (
    files("flywheel_core") / "_schema" / "persistence-schema.sql"
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
        # Version pin: a database whose schema_version row does not match is
        # refused with a "must be re-created" error. There is no in-place
        # migration — the schema is pre-MVP, so an older on-disk shape is
        # rejected rather than upgraded. The schema_version table has a CHECK
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

    def _next_domain_sequence(self, run_id: str) -> int:
        """Allocate the next per-run domain-event sequence number.

        ``MAX(sequence) + 1`` over the run's existing event rows. Always
        called inside the append transaction (``BEGIN IMMEDIATE``
        serializes writers), so per-run ordering stays strictly monotonic
        without the retired shared cross-table counter; the
        ``UNIQUE (run_id, sequence)`` constraint backstops any writer
        that bypasses the transaction.
        """
        row = self._connection.execute(
            "SELECT COALESCE(MAX(sequence), 0) + 1 AS next_seq "
            "FROM events WHERE run_id = ?",
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
        # Auto-register the task identity so lifecycles.task_id -> tasks(id)
        # always holds: a run can never reference a task the catalog has
        # never heard of. Idempotent — save_task (the normal pre-seed path)
        # has usually created it already; this covers callers that seed a
        # lifecycle directly.
        self._ensure_task_identity(lifecycle.task_id, _utcnow_iso())
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

    def _ensure_task_identity(self, task_id: str, now_iso: str) -> None:
        """Create the logical task identity row if absent (idempotent).

        INSERT OR IGNORE preserves the original first_seen_at; the FK anchor
        exists after this call so versions, tags, prerequisites, and
        lifecycles can reference it.
        """
        self._connection.execute(
            "INSERT OR IGNORE INTO tasks (id, first_seen_at, updated_at) "
            "VALUES (?, ?, ?)",
            (task_id, now_iso, now_iso),
        )

    def save_task(self, task: Task, *, now: datetime) -> str:
        content_hash = task_digest(task)
        data = serialize_task(task)
        now_iso = _iso(now)
        # One transaction so the identity row and the version row land
        # together. The version row is immutable (INSERT OR IGNORE keeps the
        # original created_at); the whole definition — goal, graders, tags,
        # context — is part of the content hash.
        with self._transaction():
            self._ensure_task_identity(task.id, now_iso)
            self._connection.execute(
                "UPDATE tasks SET updated_at = ? WHERE id = ?",
                (now_iso, task.id),
            )
            self._connection.execute(
                """
                INSERT OR IGNORE INTO task_versions (
                    task_id, content_hash, goal, graders_json, tags_json,
                    context_json, created_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    task.id,
                    content_hash,
                    data["goal"],
                    json.dumps(data["graders"]),
                    json.dumps(data["tags"]),
                    json.dumps(data["context"]),
                    now_iso,
                ),
            )
        return content_hash

    def load_task(
        self, task_id: str, content_hash: str | None = None
    ) -> Task | None:
        if content_hash is not None:
            row = self._connection.execute(
                "SELECT goal, graders_json, tags_json, context_json "
                "FROM task_versions WHERE task_id = ? AND content_hash = ?",
                (task_id, content_hash),
            ).fetchone()
        else:
            row = self._connection.execute(
                "SELECT goal, graders_json, tags_json, context_json "
                "FROM task_versions WHERE task_id = ? "
                "ORDER BY created_at DESC, rowid DESC LIMIT 1",
                (task_id,),
            ).fetchone()
        if row is None:
            return None
        return deserialize_task(
            {
                "id": task_id,
                "goal": row["goal"],
                "graders": json.loads(row["graders_json"]),
                "tags": json.loads(row["tags_json"]),
                "context": json.loads(row["context_json"]),
            }
        )

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
        sequence = self._next_domain_sequence(event.run_id)
        self._connection.execute(
            """
            INSERT INTO events (
                run_id, attempt_number, ts, kind, payload_json, sequence
            ) VALUES (?, ?, ?, ?, ?, ?)
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
            WHERE run_id = ?
            ORDER BY sequence
            """,
            (run_id,),
        ).fetchall()
        return [_row_to_domain_event(r) for r in rows]

    # --- AttemptStore -----------------------------------------------------

    def save_attempt(
        self,
        run_id: str,
        attempt: Attempt,
        *,
        expected_version: int | None = None,
    ) -> None:
        if expected_version is None:
            # Unconditional upsert: either a direct caller's write or the
            # domain-event projection, which already runs inside the
            # version-checked append transaction.
            self._upsert_attempt(run_id, attempt)
            return
        # Versioned write (the harness's iteration-boundary aggregate
        # rollup): verify the lifecycle's optimistic-concurrency key in
        # the same transaction as the upsert so a stale worker cannot
        # clobber counters after the run moved on.
        with self._transaction():
            row = self._connection.execute(
                "SELECT version FROM lifecycles WHERE run_id = ?",
                (run_id,),
            ).fetchone()
            if row is None:
                raise LifecycleNotFoundError(run_id)
            actual = int(row["version"])
            if actual != expected_version:
                raise OptimisticConcurrencyError(
                    run_id,
                    expected_version=expected_version,
                    actual_version=actual,
                )
            self._upsert_attempt(run_id, attempt)

    def _upsert_attempt(self, run_id: str, attempt: Attempt) -> None:
        # INSERT OR REPLACE upserts on the (run_id, number) PK so the
        # harness can record both the start and the finalization of an
        # attempt without separate verbs.
        self._connection.execute(
            """
            INSERT OR REPLACE INTO attempts (
                run_id, number, attempt_run_id, started_at, ended_at,
                outcome, agent_output, error, agent_context_json,
                input_tokens, output_tokens, cache_creation_input_tokens,
                cache_read_input_tokens, iterations_completed, turns,
                total_cost_usd, last_activity_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                attempt.input_tokens,
                attempt.output_tokens,
                attempt.cache_creation_input_tokens,
                attempt.cache_read_input_tokens,
                attempt.iterations_completed,
                attempt.turns,
                attempt.total_cost_usd,
                (
                    _iso(attempt.last_activity_at)
                    if attempt.last_activity_at
                    else None
                ),
            ),
        )

    def load_attempt(self, run_id: str, number: int) -> Attempt | None:
        row = self._connection.execute(
            """
            SELECT number, attempt_run_id, started_at, ended_at, outcome,
                   agent_output, error, agent_context_json, input_tokens,
                   output_tokens, cache_creation_input_tokens,
                   cache_read_input_tokens, iterations_completed, turns,
                   total_cost_usd, last_activity_at
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
                   agent_output, error, agent_context_json, input_tokens,
                   output_tokens, cache_creation_input_tokens,
                   cache_read_input_tokens, iterations_completed, turns,
                   total_cost_usd, last_activity_at
            FROM attempts
            WHERE run_id = ?
            ORDER BY number
            """,
            (run_id,),
        ).fetchall()
        return [_row_to_attempt(r) for r in rows]

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
        # RETURNING yields rows in arbitrary order;
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

    def delete_command(self, command_id: int) -> None:
        # Queue hygiene (spec 00025 FR-10): the consumer deletes an
        # applied row only after the steering domain event committed.
        # Unknown ids are a no-op by contract.
        self._connection.execute(
            "DELETE FROM control_commands WHERE id = ?",
            (command_id,),
        )


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
        input_tokens=int(row["input_tokens"] or 0),
        output_tokens=int(row["output_tokens"] or 0),
        cache_creation_input_tokens=int(
            row["cache_creation_input_tokens"] or 0
        ),
        cache_read_input_tokens=int(row["cache_read_input_tokens"] or 0),
        iterations_completed=int(row["iterations_completed"] or 0),
        turns=int(row["turns"] or 0),
        total_cost_usd=float(row["total_cost_usd"] or 0.0),
        last_activity_at=_parse_iso(row["last_activity_at"]),
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
