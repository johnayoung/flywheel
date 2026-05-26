"""Persistence protocol contracts for flywheel.

Pure type module: defines the wire-level shape every concrete store must
satisfy (in-memory, SQLite, future backends). Keying mirrors
`flywheel/_schema/persistence-schema.sql` column-level definitions exactly so concrete
stores converge on the same surface.

This module deliberately imports no IO, persistence, SQLite, JSON, file,
stream, or network APIs. Serialization is the concrete store's concern;
the protocol traffics in typed dataclasses and ``Mapping`` payloads.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Literal, Protocol, runtime_checkable

from flywheel.lifecycle import Attempt, Lifecycle


# --- Typed conflict signals -------------------------------------------------


class StoreConflictError(Exception):
    """Base class for typed conflict signals surfaced by a store contract."""


class OptimisticConcurrencyError(StoreConflictError):
    """Raised when a lifecycle write's ``expected_version`` does not match
    the stored ``Lifecycle.version``.

    Carries enough structured detail (``run_id``, ``expected_version``,
    ``actual_version``) that callers can decide between reload-and-retry,
    abort, or merge without re-parsing the message.
    """

    def __init__(
        self,
        run_id: str,
        *,
        expected_version: int,
        actual_version: int,
    ) -> None:
        super().__init__(
            f"lifecycle {run_id!r}: expected version {expected_version}, "
            f"stored version {actual_version}"
        )
        self.run_id = run_id
        self.expected_version = expected_version
        self.actual_version = actual_version


class LifecycleAlreadyExistsError(StoreConflictError):
    """Raised when ``create_lifecycle`` is called with a ``run_id`` already
    present in the store."""

    def __init__(self, run_id: str) -> None:
        super().__init__(f"lifecycle {run_id!r} already exists")
        self.run_id = run_id


class LifecycleNotFoundError(StoreConflictError):
    """Raised when ``update_lifecycle`` is called for a ``run_id`` that has
    no stored row."""

    def __init__(self, run_id: str) -> None:
        super().__init__(f"lifecycle {run_id!r} not found")
        self.run_id = run_id


# --- Record types -----------------------------------------------------------


GraderType = Literal["command", "transcript", "rubric", "manual"]


@dataclass(kw_only=True)
class EventRecord:
    """A single timeline event emitted by the harness.

    Mirrors the ``events`` table in ``flywheel/_schema/persistence-schema.sql``. ``id``
    is assigned by the store on append; callers leave it ``None``.
    """

    run_id: str
    ts: datetime
    kind: str
    payload: Mapping[str, Any] = field(default_factory=dict)
    attempt_number: int | None = None
    id: int | None = None


@dataclass(kw_only=True)
class GraderResultRecord:
    """A single grader execution receipt; append-only by contract.

    Mirrors the ``grader_results`` table in ``flywheel/_schema/persistence-schema.sql``.
    ``grader_spec`` snapshots the exact grader object as it appeared in the
    task at run time so historical truth survives later edits.
    ``payload`` carries per-type execution detail (shape defined by
    grader_type per the schema header).
    """

    run_id: str
    attempt_number: int
    ordinal: int
    grader_type: GraderType
    grader_spec: Mapping[str, Any]
    passed: bool
    duration_ms: int
    payload: Mapping[str, Any]
    ts: datetime
    grader_name: str | None = None
    id: int | None = None


@dataclass(kw_only=True)
class ClaudeSessionEntry:
    """One row in the ``claude_session_store`` table.

    Append-only stream keyed by ``(project_key, session_id, subpath, seq)``.
    ``entry`` is an opaque payload; the protocol does not parse it. The
    empty string is the subpath sentinel for the main transcript.
    """

    project_key: str
    session_id: str
    entry: str
    mtime: int
    subpath: str = ""
    seq: int | None = None


# --- Store protocols --------------------------------------------------------


@runtime_checkable
class LifecycleStore(Protocol):
    """Persistence contract for ``Lifecycle`` rows, keyed by ``run_id``.

    Writes are split into ``create`` and ``update`` so optimistic
    concurrency is enforced on every mutation:

    * ``create_lifecycle`` raises ``LifecycleAlreadyExistsError`` if the
      ``run_id`` already exists.
    * ``update_lifecycle`` raises ``OptimisticConcurrencyError`` if the
      stored ``version`` does not match ``expected_version``, and
      ``LifecycleNotFoundError`` if no row exists for ``run_id``.

    ``load_lifecycle`` returns a typed ``Lifecycle`` instance with its
    ``attempts`` field populated in ascending ``number`` order, or ``None``
    when the ``run_id`` is unknown.
    """

    def create_lifecycle(self, lifecycle: Lifecycle) -> None: ...

    def update_lifecycle(
        self,
        lifecycle: Lifecycle,
        *,
        expected_version: int,
    ) -> None: ...

    def load_lifecycle(self, run_id: str) -> Lifecycle | None: ...


@runtime_checkable
class AttemptStore(Protocol):
    """Persistence contract for ``Attempt`` records, keyed by
    ``(run_id, number)``.

    ``save_attempt`` is upsert-style: it inserts a new attempt or replaces
    an existing row keyed by ``(run_id, attempt.number)``. The harness uses
    this to record both the start (``started_at`` set, ``ended_at``/
    ``outcome`` unset) and the finalization (``ended_at`` and ``outcome``
    set) of a single attempt without separate verbs.

    ``list_attempts`` returns attempts in ascending ``number`` order
    regardless of insertion order; ``load_attempt`` returns ``None`` for
    missing rows.
    """

    def save_attempt(self, run_id: str, attempt: Attempt) -> None: ...

    def load_attempt(self, run_id: str, number: int) -> Attempt | None: ...

    def list_attempts(self, run_id: str) -> list[Attempt]: ...


@runtime_checkable
class EventStore(Protocol):
    """Persistence contract for harness-emitted events.

    Append-only chronological log keyed by an autoincrement ``id`` and
    foreign-keyed to ``lifecycles(run_id)`` per the schema. ``append_event``
    assigns the ``id`` and returns the persisted record. ``list_events``
    returns events for a ``run_id`` in ``(ts, id)`` order, matching the
    ``idx_events_run`` ordering implied by the schema.
    """

    def append_event(self, event: EventRecord) -> EventRecord: ...

    def list_events(self, run_id: str) -> list[EventRecord]: ...


@runtime_checkable
class GraderResultStore(Protocol):
    """Persistence contract for ``grader_results``. Append-only by contract.

    The protocol intentionally exposes no update or delete entry point:
    corrections go in new rows or compensating events, never in-place edits.
    Records are keyed by ``(run_id, attempt_number, ordinal)`` and
    ``list_grader_results`` returns them in that order.
    """

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord: ...

    def list_grader_results(
        self,
        run_id: str,
        attempt_number: int,
    ) -> list[GraderResultRecord]: ...


@runtime_checkable
class AgentSessionStore(Protocol):
    """Persistence contract for the ``claude_session_store`` transcript log.

    Append-only, keyed by ``(project_key, session_id, subpath, seq)``.
    ``append_session_entry`` assigns the ``seq`` and returns the persisted
    record. ``list_session_entries`` returns entries for one
    ``(project_key, session_id, subpath)`` tuple in ascending ``seq``
    order; the empty-string ``subpath`` selects the main transcript.
    """

    def append_session_entry(
        self, entry: ClaudeSessionEntry
    ) -> ClaudeSessionEntry: ...

    def list_session_entries(
        self,
        project_key: str,
        session_id: str,
        subpath: str = "",
    ) -> list[ClaudeSessionEntry]: ...


__all__ = [
    "AgentSessionStore",
    "AttemptStore",
    "ClaudeSessionEntry",
    "EventRecord",
    "EventStore",
    "GraderResultRecord",
    "GraderResultStore",
    "GraderType",
    "LifecycleAlreadyExistsError",
    "LifecycleNotFoundError",
    "LifecycleStore",
    "OptimisticConcurrencyError",
    "StoreConflictError",
]
