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
* ``_events``, ``_sdk_messages``, and ``_grader_results`` are append-only
  ordered lists with monotonic ids; the contract surface exposes no
  update/delete entry point for grader_results.
* ``_run_sequence`` is the per-``run_id`` monotonic counter shared by
  ``append_event`` and ``save_sdk_messages`` so events and SDK messages
  form a single totally-ordered audit stream.
* ``_sessions`` mirrors ``claude_session_store`` with a monotonic ``seq``.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
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
from flywheel.lifecycle import Attempt, Lifecycle
from flywheel.notifier import RunNotifier
from flywheel.store_protocols import (
    AuditRecord,
    ClaudeSessionEntry,
    EventRecord,
    GraderResultRecord,
    GraderType,
    LifecycleAlreadyExistsError,
    LifecycleNotFoundError,
    OptimisticConcurrencyError,
    SdkMessageRecord,
)

# Page cap matches the sqlite/postgres ``read_audit_since`` cap; consumers
# drive cursor-based polling and a single page bounds the result set
# regardless of run length.
_AUDIT_PAGE_SIZE: int = 500


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
        blocked_requires_json=lc.blocked_requires_json,
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
        sequence=e.sequence,
        category=e.category,
    )


def _clone_sdk_message(m: SdkMessageRecord) -> SdkMessageRecord:
    return SdkMessageRecord(
        run_id=m.run_id,
        attempt_number=m.attempt_number,
        iteration_number=m.iteration_number,
        message_type=m.message_type,
        payload=dict(m.payload),
        ts=m.ts,
        sequence=m.sequence,
        id=m.id,
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

    def __init__(self, *, notifier: RunNotifier | None = None) -> None:
        # Default-on in-process reactivity: a consumer that shares this
        # store instance (the common single-host case) gets push wakeups;
        # separate instances fall back to the audit follower's poll.
        self.notifier: RunNotifier = notifier or RunNotifier()
        self._lifecycles: dict[str, Lifecycle] = {}
        self._attempts: dict[tuple[str, int], Attempt] = {}
        self._events: list[EventRecord] = []
        self._event_seq: int = 0
        self._grader_results: list[GraderResultRecord] = []
        self._grader_result_seq: int = 0
        self._sessions: list[ClaudeSessionEntry] = []
        self._session_seq: int = 0
        self._sdk_messages: list[SdkMessageRecord] = []
        self._sdk_message_seq: int = 0
        # Per-run monotonic sequence shared by append_event and
        # save_sdk_messages so the two write paths produce one strictly
        # ascending audit ordering per run_id.
        self._run_sequence: dict[str, int] = {}

    def _next_run_sequence(self, run_id: str) -> int:
        nxt = self._run_sequence.get(run_id, 0) + 1
        self._run_sequence[run_id] = nxt
        return nxt

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

    # --- DomainEventStore --------------------------------------------------

    def append_domain_event(
        self,
        event: DomainEvent,
        *,
        expected_version: int,
    ) -> Lifecycle:
        if isinstance(event, LifecycleInitialized):
            folded = apply(None, event)
            # Raises LifecycleAlreadyExistsError on a duplicate seed.
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
            self.update_lifecycle(folded, expected_version=expected_version)
            self._project_domain_event(event, folded)
        sequence = self._append_domain_event_row(event)
        self.notifier.notify(event.run_id, sequence)
        return folded

    def _append_domain_event_row(self, event: DomainEvent) -> int:
        self._event_seq += 1
        sequence = self._next_run_sequence(event.run_id)
        self._events.append(
            EventRecord(
                run_id=event.run_id,
                ts=event.ts,
                kind=event_kind(event),
                payload=event_payload(event),
                attempt_number=event.attempt_number,
                id=self._event_seq,
                sequence=sequence,
                category="domain",
            )
        )
        return sequence

    def _project_domain_event(
        self, event: DomainEvent, folded: Lifecycle
    ) -> None:
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
        rows = [
            e
            for e in self._events
            if e.run_id == run_id and e.category == "domain"
        ]
        rows.sort(key=lambda e: e.sequence if e.sequence is not None else 0)
        return [
            event_from_record(
                kind=e.kind,
                payload=dict(e.payload),
                run_id=e.run_id,
                ts=e.ts,
                attempt_number=e.attempt_number,
                sequence=e.sequence,
                id=e.id,
            )
            for e in rows
        ]

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
        sequence = self._next_run_sequence(event.run_id)
        record = EventRecord(
            run_id=event.run_id,
            ts=event.ts,
            kind=event.kind,
            payload=dict(event.payload),
            attempt_number=event.attempt_number,
            id=self._event_seq,
            sequence=sequence,
        )
        self._events.append(record)
        self.notifier.notify(event.run_id, sequence)
        return _clone_event(record)

    def list_events(self, run_id: str) -> list[EventRecord]:
        rows = [
            e
            for e in self._events
            if e.run_id == run_id and e.category == "telemetry"
        ]
        rows.sort(key=lambda e: (e.ts, e.id if e.id is not None else 0))
        return [_clone_event(e) for e in rows]

    # --- SdkMessageStore ---------------------------------------------------

    def save_sdk_messages(
        self,
        run_id: str,
        attempt_number: int,
        iteration_number: int,
        messages: Sequence[Mapping[str, Any]],
    ) -> list[SdkMessageRecord]:
        persisted: list[SdkMessageRecord] = []
        for msg in messages:
            self._sdk_message_seq += 1
            sequence = self._next_run_sequence(run_id)
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
                sequence=sequence,
                id=self._sdk_message_seq,
            )
            self._sdk_messages.append(record)
            persisted.append(_clone_sdk_message(record))
        if persisted and persisted[-1].sequence is not None:
            self.notifier.notify(run_id, persisted[-1].sequence)
        return persisted

    def list_sdk_messages(self, run_id: str) -> list[SdkMessageRecord]:
        rows = [m for m in self._sdk_messages if m.run_id == run_id]
        rows.sort(key=lambda m: m.sequence if m.sequence is not None else 0)
        return [_clone_sdk_message(m) for m in rows]

    # --- AuditStore --------------------------------------------------------

    def read_audit_since(
        self, run_id: str, cursor: int
    ) -> list[AuditRecord]:
        merged: list[AuditRecord] = []
        for e in self._events:
            if e.run_id != run_id or e.sequence is None:
                continue
            if e.category != "telemetry":
                continue
            if e.sequence > cursor:
                merged.append(_clone_event(e))
        for m in self._sdk_messages:
            if m.run_id != run_id or m.sequence is None:
                continue
            if m.sequence > cursor:
                merged.append(_clone_sdk_message(m))
        merged.sort(
            key=lambda r: r.sequence if r.sequence is not None else 0
        )
        return merged[:_AUDIT_PAGE_SIZE]

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
