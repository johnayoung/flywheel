"""In-memory implementation of every store Protocol from
``flywheel.store_protocols``.

This is the test substrate: a single ``InMemoryStore`` class backed by
plain dicts and lists. It is not durable, not thread-safe, and contains
no ``sqlite3``, file, or network IO — durable persistence lives in the
SQLite store (a sibling task).

Storage layout follows the table boundaries in
``flywheel/_schema/persistence-schema.sql``:

* ``_lifecycles`` is keyed by ``run_id`` and holds the row state with no
  attempts attached; ``attempts`` are populated on read by joining the
  attempts table.
* ``_attempts`` is keyed by ``(run_id, number)``.
* ``_events`` and ``_grader_results`` are append-only ordered lists with
  monotonic ids; the contract surface exposes no update/delete entry
  point for grader_results.
* ``_sessions`` mirrors ``claude_session_store`` with a monotonic ``seq``.
"""

from __future__ import annotations

from flywheel.lifecycle import Attempt, Lifecycle
from flywheel.store_protocols import (
    ClaudeSessionEntry,
    EventRecord,
    GraderResultRecord,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    OptimisticConcurrencyError,
)


def _clone_lifecycle_row(lc: Lifecycle) -> Lifecycle:
    """Defensive copy of the lifecycle row. ``attempts`` is intentionally
    left empty so the attempts table stays the single source of truth."""
    return Lifecycle(
        task_id=lc.task_id,
        run_id=lc.run_id,
        worker_id=lc.worker_id,
        status=lc.status,
        timestamps=dict(lc.timestamps),
        version=lc.version,
        retries=lc.retries,
        error=lc.error,
        agent_output=lc.agent_output,
        attempts=[],
        session_id=lc.session_id,
        artifacts_dir=lc.artifacts_dir,
    )


def _clone_attempt(a: Attempt) -> Attempt:
    return Attempt(
        number=a.number,
        started_at=a.started_at,
        run_id=a.run_id,
        ended_at=a.ended_at,
        outcome=a.outcome,
        agent_output=a.agent_output,
        error=a.error,
        agent_context=dict(a.agent_context),
    )


def _clone_event(e: EventRecord) -> EventRecord:
    return EventRecord(
        run_id=e.run_id,
        ts=e.ts,
        kind=e.kind,
        payload=dict(e.payload),
        attempt_number=e.attempt_number,
        id=e.id,
    )


def _clone_grader_result(r: GraderResultRecord) -> GraderResultRecord:
    return GraderResultRecord(
        run_id=r.run_id,
        attempt_number=r.attempt_number,
        ordinal=r.ordinal,
        grader_type=r.grader_type,
        grader_spec=dict(r.grader_spec),
        passed=r.passed,
        duration_ms=r.duration_ms,
        payload=dict(r.payload),
        ts=r.ts,
        grader_name=r.grader_name,
        id=r.id,
    )


def _clone_session_entry(e: ClaudeSessionEntry) -> ClaudeSessionEntry:
    return ClaudeSessionEntry(
        project_key=e.project_key,
        session_id=e.session_id,
        entry=e.entry,
        mtime=e.mtime,
        subpath=e.subpath,
        seq=e.seq,
    )


class InMemoryStore:
    """In-memory implementation of every store protocol.

    Each public method satisfies the corresponding Protocol in
    ``flywheel.store_protocols``. Records returned to callers are
    defensive copies so caller mutations do not corrupt stored rows; the
    same holds in reverse for record arguments.
    """

    def __init__(self) -> None:
        self._lifecycles: dict[str, Lifecycle] = {}
        self._attempts: dict[tuple[str, int], Attempt] = {}
        self._events: list[EventRecord] = []
        self._event_seq: int = 0
        self._grader_results: list[GraderResultRecord] = []
        self._grader_result_seq: int = 0
        self._sessions: list[ClaudeSessionEntry] = []
        self._session_seq: int = 0

    # --- LifecycleStore ----------------------------------------------------

    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        if lifecycle.run_id in self._lifecycles:
            raise LifecycleAlreadyExistsError(lifecycle.run_id)
        self._lifecycles[lifecycle.run_id] = _clone_lifecycle_row(lifecycle)

    def update_lifecycle(
        self,
        lifecycle: Lifecycle,
        *,
        expected_version: int,
    ) -> None:
        stored = self._lifecycles.get(lifecycle.run_id)
        if stored is None:
            raise LifecycleNotFoundError(lifecycle.run_id)
        if stored.version != expected_version:
            raise OptimisticConcurrencyError(
                lifecycle.run_id,
                expected_version=expected_version,
                actual_version=stored.version,
            )
        self._lifecycles[lifecycle.run_id] = _clone_lifecycle_row(lifecycle)

    def load_lifecycle(self, run_id: str) -> Lifecycle | None:
        stored = self._lifecycles.get(run_id)
        if stored is None:
            return None
        lc = _clone_lifecycle_row(stored)
        lc.attempts = self.list_attempts(run_id)
        return lc

    # --- AttemptStore ------------------------------------------------------

    def save_attempt(self, run_id: str, attempt: Attempt) -> None:
        self._attempts[(run_id, attempt.number)] = _clone_attempt(attempt)

    def load_attempt(self, run_id: str, number: int) -> Attempt | None:
        stored = self._attempts.get((run_id, number))
        if stored is None:
            return None
        return _clone_attempt(stored)

    def list_attempts(self, run_id: str) -> list[Attempt]:
        rows = [
            a for (rid, _n), a in self._attempts.items() if rid == run_id
        ]
        rows.sort(key=lambda a: a.number)
        return [_clone_attempt(a) for a in rows]

    # --- EventStore --------------------------------------------------------

    def append_event(self, event: EventRecord) -> EventRecord:
        self._event_seq += 1
        record = EventRecord(
            run_id=event.run_id,
            ts=event.ts,
            kind=event.kind,
            payload=dict(event.payload),
            attempt_number=event.attempt_number,
            id=self._event_seq,
        )
        self._events.append(record)
        return _clone_event(record)

    def list_events(self, run_id: str) -> list[EventRecord]:
        rows = [e for e in self._events if e.run_id == run_id]
        rows.sort(key=lambda e: (e.ts, e.id if e.id is not None else 0))
        return [_clone_event(e) for e in rows]

    # --- GraderResultStore -------------------------------------------------

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        self._grader_result_seq += 1
        record = GraderResultRecord(
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
            id=self._grader_result_seq,
        )
        self._grader_results.append(record)
        return _clone_grader_result(record)

    def list_grader_results(
        self,
        run_id: str,
        attempt_number: int,
    ) -> list[GraderResultRecord]:
        rows = [
            r
            for r in self._grader_results
            if r.run_id == run_id and r.attempt_number == attempt_number
        ]
        rows.sort(key=lambda r: r.ordinal)
        return [_clone_grader_result(r) for r in rows]

    # --- AgentSessionStore -------------------------------------------------

    def append_session_entry(
        self, entry: ClaudeSessionEntry
    ) -> ClaudeSessionEntry:
        self._session_seq += 1
        record = ClaudeSessionEntry(
            project_key=entry.project_key,
            session_id=entry.session_id,
            entry=entry.entry,
            mtime=entry.mtime,
            subpath=entry.subpath,
            seq=self._session_seq,
        )
        self._sessions.append(record)
        return _clone_session_entry(record)

    def list_session_entries(
        self,
        project_key: str,
        session_id: str,
        subpath: str = "",
    ) -> list[ClaudeSessionEntry]:
        rows = [
            e
            for e in self._sessions
            if e.project_key == project_key
            and e.session_id == session_id
            and e.subpath == subpath
        ]
        rows.sort(key=lambda e: e.seq if e.seq is not None else 0)
        return [_clone_session_entry(e) for e in rows]


__all__ = ["InMemoryStore"]
