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

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Literal, Protocol, Union, runtime_checkable

from flywheel.events import DomainEvent
from flywheel.lifecycle import Attempt, Lifecycle
from flywheel.task import Task


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


class ClaimLostError(StoreConflictError):
    """Raised when ``renew_claim`` finds the claim is no longer the caller's.

    Either the lease lapsed and another worker stole the task, or the claim
    was released, so the version/worker no longer match. The caller must
    stop acting on the task — another worker now owns it.
    """

    def __init__(self, task_id: str) -> None:
        super().__init__(f"claim on task {task_id!r} lost")
        self.task_id = task_id


# --- Schema-version mismatch signal ----------------------------------------


# Bumped whenever the persistence schema gains a backwards-incompatible
# change. Stores compare their on-disk row against this constant and
# raise :class:`StoreSchemaError` when it does not match.
CURRENT_SCHEMA_VERSION: int = 5


class StoreSchemaError(Exception):
    """Raised when a concrete store opens a database whose
    ``schema_version`` row does not match
    :data:`CURRENT_SCHEMA_VERSION`.

    The message is fixed-prefix ("store must be re-created") and the
    instance carries both the observed and expected versions so callers
    can distinguish a legacy database from generic operational errors.
    """

    def __init__(
        self,
        *,
        observed_version: int | None,
        expected_version: int,
    ) -> None:
        super().__init__(
            "store must be re-created: "
            f"observed schema_version={observed_version!r}, "
            f"expected {expected_version}"
        )
        self.observed_version = observed_version
        self.expected_version = expected_version


# --- Record types -----------------------------------------------------------


GraderType = Literal["command", "transcript", "rubric", "manual"]


@dataclass(kw_only=True)
class EventRecord:
    """A single timeline event emitted by the harness.

    Mirrors the ``events`` table in ``flywheel/_schema/persistence-schema.sql``. ``id``
    is assigned by the store on append; callers leave it ``None``.

    ``sequence`` is the per-run monotonic counter shared with
    :class:`SdkMessageRecord` so events and SDK messages form a single
    totally-ordered audit stream. Stores assign ``sequence`` on
    ``append_event``; callers leave it ``None``.

    ``category`` discriminates a ``'domain'`` event (a state-bearing
    member of the event-sourced log, written via
    :meth:`DomainEventStore.append_domain_event`) from a ``'telemetry'``
    event (pure observability, written via :meth:`EventStore.append_event`).
    Both share the ``events`` table and the per-run ``sequence`` ordering;
    only domain events are folded into lifecycle state.
    """

    run_id: str
    ts: datetime
    kind: str
    payload: Mapping[str, Any] = field(default_factory=dict)
    attempt_number: int | None = None
    id: int | None = None
    sequence: int | None = None
    category: str = "telemetry"


@dataclass(kw_only=True)
class SdkMessageRecord:
    """A single SDK message captured from the agent transport.

    Mirrors the ``sdk_messages`` table in
    ``flywheel/_schema/persistence-schema.sql``. ``id`` is assigned by
    the store on insert. ``sequence`` is the per-run monotonic counter
    shared with :class:`EventRecord` so audit consumers see a single
    totally-ordered stream regardless of which write path produced a
    record; the store assigns it atomically with persistence and
    callers leave it ``None``.

    ``payload`` is an opaque JSON-compatible mapping (no typed coupling
    to ``claude-agent-sdk`` dataclasses): stores persist it verbatim,
    the same way :attr:`EventRecord.payload` is handled.
    """

    run_id: str
    attempt_number: int
    iteration_number: int
    message_type: str
    payload: Mapping[str, Any]
    ts: datetime
    sequence: int | None = None
    id: int | None = None


# Typed union returned by :meth:`AuditStore.read_audit_since`. Consumers
# discriminate on ``isinstance``; both arms expose the shared per-run
# ``sequence`` field that defines the audit ordering.
AuditRecord = Union[EventRecord, SdkMessageRecord]


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


@dataclass(frozen=True, kw_only=True)
class TaskClaim:
    """A worker's lease on a task, mirroring one ``task_claims`` row.

    Immutable snapshot: ``acquire_claim`` / ``renew_claim`` return a fresh
    instance with the bumped ``version`` and extended ``lease_expires_at``.
    ``version`` and ``worker_id`` together are the optimistic-concurrency
    key for renew/release — a stale token (wrong version, or a different
    worker stole the task) is rejected.
    """

    task_id: str
    worker_id: str
    claimed_at: datetime
    lease_expires_at: datetime
    version: int


@dataclass(kw_only=True)
class ControlCommandRecord:
    """One row in the ``control_commands`` table.

    Mirrors the ``control_commands`` table in
    ``flywheel/_schema/persistence-schema.sql``. A control command is an
    operator-issued intervention against a live run (``interrupt``, ``say``,
    ``set_model``, ...) routed through the store so producers and the
    in-process watcher coordinate across the worker-daemon boundary.

    ``payload`` is opaque per-kind execution detail (e.g. the operator
    message for ``say``, the target identifier for ``set_model``); the
    protocol persists it verbatim the same way :attr:`EventRecord.payload`
    is handled.

    ``id`` is assigned by the store on ``enqueue_command`` and is the
    canonical enqueue-order key — ``claim_commands`` returns pending rows
    ascending by ``id``. ``claimed_at`` is ``None`` while pending and
    populated with the claim moment by ``claim_commands``; the column
    drives claim-once semantics so a single command applies at most one
    time across watcher restarts and concurrent workers.
    """

    run_id: str
    kind: str
    payload: Mapping[str, Any]
    enqueued_at: datetime
    claimed_at: datetime | None = None
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
class TaskStore(Protocol):
    """Persistence contract for ``Task`` definitions, content-addressed.

    A task is stored under the composite key ``(id, content_hash)`` where
    ``content_hash`` is :func:`flywheel.loaders.task_digest` over the task's
    definition (everything except ``id``). This makes storage immutable and
    deduplicated: re-saving an unchanged task is a no-op, while editing it
    produces a new version row. A run pins the exact version it executed via
    ``Lifecycle.task_content_hash``, so historical truth survives later edits
    — the same guarantee ``grader_results`` provides for graders, extended to
    the whole task.

    * ``save_task`` is idempotent. It serializes the task, computes its
      digest, and inserts a row keyed by ``(task.id, digest)`` only if absent;
      it returns the digest. ``created_at`` is set from the injected ``now``
      on first insert and never mutated thereafter.
    * ``load_task`` returns the exact version when ``content_hash`` is given,
      else the most recently created version for ``task_id``; ``None`` when no
      matching row exists.
    * ``load_task_for_run`` resolves the run's lifecycle ``task_id`` +
      ``task_content_hash`` to the precise task that run executed; ``None``
      when the run or its pinned task is unknown.
    """

    def save_task(self, task: Task, *, now: datetime) -> str: ...

    def load_task(
        self, task_id: str, content_hash: str | None = None
    ) -> Task | None: ...

    def load_task_for_run(self, run_id: str) -> Task | None: ...


@runtime_checkable
class DomainEventStore(Protocol):
    """Event-sourced write contract for the lifecycle.

    Lifecycle state is the fold of an ordered domain-event log. Appending
    a domain event is the single authoritative write: it advances the log,
    updates the ``lifecycles`` projection (and the ``attempts`` /
    ``grader_results`` projections it implies) atomically, and returns the
    folded :class:`Lifecycle`. There is no separate "mutate row then emit
    event" step, so state and timeline cannot diverge.

    Concurrency: ``expected_version`` is the optimistic-concurrency
    compare-and-swap key — the caller's view of the current domain-event
    offset (``Lifecycle.version``). A mismatch raises
    ``OptimisticConcurrencyError``. The seed event
    (:class:`flywheel.events.LifecycleInitialized`) creates the projection
    row and raises ``LifecycleAlreadyExistsError`` if it already exists;
    its ``expected_version`` is ignored. Every other event raises
    ``LifecycleNotFoundError`` when no row exists for the event's
    ``run_id``.

    ``list_domain_events`` returns the run's domain events in ascending
    ``sequence`` order, suitable for ``flywheel.events.replay``.
    """

    def append_domain_event(
        self,
        event: DomainEvent,
        *,
        expected_version: int,
    ) -> Lifecycle: ...

    def list_domain_events(self, run_id: str) -> list[DomainEvent]: ...


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
    assigns both ``id`` and the per-run monotonic ``sequence`` value (the
    same counter feeds :meth:`SdkMessageStore.append_sdk_message` so events
    and SDK messages share one strict ordering) and returns the persisted
    record. ``list_events`` returns events for a ``run_id`` in ``(ts, id)``
    order, matching the ``idx_events_run`` ordering implied by the schema.
    """

    def append_event(self, event: EventRecord) -> EventRecord: ...

    def list_events(self, run_id: str) -> list[EventRecord]: ...


@runtime_checkable
class SdkMessageStore(Protocol):
    """Persistence contract for captured agent SDK messages.

    Append-only. ``append_sdk_message`` is the live, per-message write
    path: it accepts one :class:`SdkMessageRecord`, allocates one tick
    from the per-run monotonic ``sequence`` counter (the same counter
    that feeds :meth:`EventStore.append_event`, so interleaved event and
    SDK-message writes against the same ``run_id`` produce strictly
    ascending sequence numbers across both record types), inserts the
    row, and returns the persisted record with ``id`` and ``sequence``
    populated. ``save_sdk_messages`` is retained for backward
    compatibility — it persists a per-iteration batch and is now a thin
    loop over ``append_sdk_message``; the harness no longer calls it.
    ``list_sdk_messages`` returns every persisted record for ``run_id``
    in ascending ``sequence`` order.
    """

    def append_sdk_message(
        self, message: SdkMessageRecord
    ) -> SdkMessageRecord: ...

    def save_sdk_messages(
        self,
        run_id: str,
        attempt_number: int,
        iteration_number: int,
        messages: Sequence[Mapping[str, Any]],
    ) -> list[SdkMessageRecord]: ...

    def list_sdk_messages(self, run_id: str) -> list[SdkMessageRecord]: ...


@runtime_checkable
class AuditStore(Protocol):
    """Unified read contract over the per-run audit stream.

    The audit stream merges :class:`EventRecord` and
    :class:`SdkMessageRecord` rows for one ``run_id`` into a single
    totally-ordered sequence keyed by the shared per-run ``sequence``
    counter. ``read_audit_since(run_id, cursor)`` returns every record
    with ``sequence > cursor`` in ascending ``sequence`` order, capped
    at an internal page size so callers can drive cursor-based polling
    without blowing the result set on a long-lived run.
    """

    def read_audit_since(
        self, run_id: str, cursor: int
    ) -> list[AuditRecord]: ...


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
class ClaimStore(Protocol):
    """Per-task lease contract for multi-worker mutual exclusion.

    At most one live claim exists per ``task_id``. A worker acquires it
    before running the task and releases it on completion; the lease's
    expiry lets another worker reclaim a task whose worker crashed.

    * ``acquire_claim`` returns a :class:`TaskClaim` when the task is free,
      the existing lease has expired (the new claim *steals* it), or the
      caller already holds it (idempotent re-acquire). It returns ``None``
      when a *live* lease is held by a different worker. The check-and-write
      is atomic so two workers racing for the same task cannot both win.
    * ``renew_claim`` extends the lease, bumping ``version``; it raises
      :class:`ClaimLostError` when the caller's token no longer matches the
      stored row (stolen after expiry, or released).
    * ``release_claim`` drops the claim when the token still matches; a
      no-op if it was already stolen or released.
    * ``load_claim`` returns the current claim for a task, or ``None``.

    ``now`` is injected (not read from a clock) so lease expiry is
    deterministic and testable; ``lease_seconds`` sets the new
    ``lease_expires_at = now + lease_seconds``.
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


@runtime_checkable
class ControlCommandStore(Protocol):
    """Persistence contract for operator-issued control commands.

    A control command is a store-routed intervention against a live run:
    ``enqueue_command`` persists a pending row, ``claim_commands`` returns
    and atomically marks every pending row for one run exactly once, in
    enqueue order. The producer (a CLI subcommand) and the consuming
    in-process watcher communicate only through the store so steering
    works across the worker-daemon boundary.

    * ``enqueue_command(run_id, kind, payload, *, now)`` writes a fresh
      row with ``claimed_at = None`` and ``enqueued_at = now``, assigns
      the store-side ``id`` (the enqueue-order key), and returns the
      persisted record. The command is persisted unconditionally — a
      command targeting a run that is not currently in-flight is kept
      pending rather than silently dropped. Concrete stores that enforce
      a foreign key to ``lifecycles`` require the run to be known; if
      enforcement is missing on a particular backend the protocol still
      accepts unknown runs and they remain pending.
    * ``claim_commands(run_id, *, now)`` is the claim-once primitive: it
      atomically selects every pending row for ``run_id``, sets
      ``claimed_at = now`` on each, and returns the claimed rows in
      ascending ``id`` order. A second call (or a concurrent worker's
      call) returns nothing for the same rows — the claim never
      double-applies. An empty pending queue returns an empty list with
      no error.
    """

    def enqueue_command(
        self,
        run_id: str,
        kind: str,
        payload: Mapping[str, Any],
        *,
        now: datetime,
    ) -> ControlCommandRecord: ...

    def claim_commands(
        self,
        run_id: str,
        *,
        now: datetime,
    ) -> list[ControlCommandRecord]: ...


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
    "AuditRecord",
    "AuditStore",
    "CURRENT_SCHEMA_VERSION",
    "ClaimLostError",
    "ClaimStore",
    "ClaudeSessionEntry",
    "ControlCommandRecord",
    "ControlCommandStore",
    "DomainEventStore",
    "EventRecord",
    "EventStore",
    "GraderResultRecord",
    "GraderResultStore",
    "GraderType",
    "LifecycleAlreadyExistsError",
    "LifecycleNotFoundError",
    "LifecycleStore",
    "OptimisticConcurrencyError",
    "SdkMessageRecord",
    "SdkMessageStore",
    "StoreConflictError",
    "StoreSchemaError",
    "TaskClaim",
    "TaskStore",
]
