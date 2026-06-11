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
from typing import Any, Literal, Protocol, Union, runtime_checkable

from flywheel_core.events import DomainEvent
from flywheel_core.lifecycle import Attempt, Lifecycle
from flywheel_core.task import Task


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


# --- Schema-version mismatch signal ----------------------------------------


# Bumped whenever the persistence schema gains a backwards-incompatible
# change. Stores compare their on-disk row against this constant and
# raise :class:`StoreSchemaError` when it does not match.
CURRENT_SCHEMA_VERSION: int = 11


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
    """A harness telemetry event as read back from a run's stream.

    Since spec 00025 this is a read-model shape, not a table mirror:
    telemetry lives in the per-run JSONL file behind
    :class:`TelemetrySink`, and the file reader
    (:mod:`flywheel_core.audit`) reconstructs ``EventRecord`` instances
    from ``harness.*`` lines. ``sequence`` is assigned by the reader
    (line count within the file) and defines the per-run ordering
    consumers see; ``id`` is unused on the file path and stays ``None``.
    """

    run_id: str
    ts: datetime
    kind: str
    payload: Mapping[str, Any] = field(default_factory=dict)
    attempt_number: int | None = None
    id: int | None = None
    sequence: int | None = None


@dataclass(kw_only=True)
class SdkMessageRecord:
    """A single SDK message as read back from a run's stream.

    Since spec 00025 this is a read-model shape, not a table mirror:
    the verbatim SDK message stream lives in the per-run JSONL file
    behind :class:`TelemetrySink`, and the file reader reconstructs
    ``SdkMessageRecord`` instances from message lines. ``sequence`` is
    assigned by the reader (line count within the file); ``id`` is
    unused on the file path and stays ``None``.

    ``payload`` is an opaque JSON-compatible mapping (no typed coupling
    to ``claude-agent-sdk`` dataclasses), persisted and read back
    verbatim the same way :attr:`EventRecord.payload` is handled.
    """

    run_id: str
    attempt_number: int
    iteration_number: int
    message_type: str
    payload: Mapping[str, Any]
    ts: datetime
    sequence: int | None = None
    id: int | None = None


# Typed union yielded by the observability readers
# (``flywheel_core.audit``). Consumers discriminate on ``isinstance``;
# both arms expose the per-run ``sequence`` field that defines the
# stream ordering.
AuditRecord = Union[EventRecord, SdkMessageRecord]


@dataclass(kw_only=True)
class TelemetryRecord:
    """One line in a run's observability stream.

    The unit of :class:`TelemetrySink` appends. Unlike the store record
    types above it mirrors no table: telemetry lives outside the
    database, in per-run destinations owned by a concrete sink (see
    ``docs/data-taxonomy.md``). There is no store-assigned ``id`` or
    ``sequence`` — emission order at the sink is the canonical
    observability ordering for the run.

    ``kind`` discriminates the record (an SDK message type, a harness
    telemetry event kind, or a mirrored domain-event kind).
    ``attempt_number``/``iteration_number`` locate the record within the
    run; either may be ``None`` for records emitted outside an attempt
    or iteration. ``payload`` is an opaque JSON-compatible mapping
    persisted verbatim, the same way :attr:`EventRecord.payload` is
    handled.
    """

    run_id: str
    ts: datetime
    kind: str
    payload: Mapping[str, Any] = field(default_factory=dict)
    attempt_number: int | None = None
    iteration_number: int | None = None


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

    A task is stored across a logical identity and immutable, content-
    addressed *versions*:

    * The logical task (its stable ``id``) is the identity every other task
      reference foreign-keys, so a run can never point at a task the store has
      never heard of.
    * A *version* is keyed ``(id, content_hash)`` where ``content_hash`` is
      :func:`flywheel_core.loaders.task_digest` over the definition — goal,
      graders, tags, and context. Versions are immutable and deduplicated:
      re-saving an unchanged definition is a no-op, while editing any hashed
      field produces a new version. A run pins the exact version it executed
      via ``Lifecycle.task_content_hash``, so historical truth survives later
      edits — the same guarantee ``grader_results`` gives for graders,
      extended to the whole definition.

    Flywheel owns a single task's lifecycle; the dependency DAG between tasks
    (``prerequisites``) is an orchestration-layer concern and is not persisted
    here.

    * ``save_task`` is idempotent. It registers the task identity and inserts
      the version row keyed by ``(task.id, digest)`` only if absent; it
      returns the digest. A version's ``created_at`` is set from the injected
      ``now`` on first insert and never mutated thereafter.
    * ``load_task`` returns the exact version when ``content_hash`` is given,
      else the most recently created version for ``task_id``; ``None`` when no
      version row exists.
    * ``load_task_for_run`` resolves the run's lifecycle ``task_id`` +
      ``task_content_hash`` to the precise version that run executed; ``None``
      when the run or its pinned version is unknown.
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
    (:class:`flywheel_core.events.LifecycleInitialized`) creates the projection
    row and raises ``LifecycleAlreadyExistsError`` if it already exists;
    its ``expected_version`` is ignored. Every other event raises
    ``LifecycleNotFoundError`` when no row exists for the event's
    ``run_id``.

    ``list_domain_events`` returns the run's domain events in ascending
    ``sequence`` order, suitable for ``flywheel_core.events.replay``.
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

    ``expected_version`` opts the write into the lifecycle's optimistic-
    concurrency check: when supplied, the store verifies — atomically with
    the upsert — that the stored ``Lifecycle.version`` for ``run_id``
    still equals it, raising ``OptimisticConcurrencyError`` on mismatch
    and ``LifecycleNotFoundError`` when no lifecycle row exists. The
    harness uses this for the iteration-boundary aggregate rollup so a
    stale worker cannot clobber counters after the run moved on. When
    ``None`` (the default) the write is unconditional, matching the
    projection path that already runs inside a version-checked
    ``append_domain_event`` transaction.

    ``list_attempts`` returns attempts in ascending ``number`` order
    regardless of insertion order; ``load_attempt`` returns ``None`` for
    missing rows.
    """

    def save_attempt(
        self,
        run_id: str,
        attempt: Attempt,
        *,
        expected_version: int | None = None,
    ) -> None: ...

    def load_attempt(self, run_id: str, number: int) -> Attempt | None: ...

    def list_attempts(self, run_id: str) -> list[Attempt]: ...


@runtime_checkable
class TelemetrySink(Protocol):
    """Pluggable destination for a run's telemetry stream.

    Telemetry (SDK messages, harness telemetry events, mirrored domain
    events) flows to a sink, not to the relational store: loss is
    acceptable, ordering is defined by emission order at the sink, and
    durability semantics belong entirely to the implementation. The MVP
    implementation is :class:`flywheel_core.telemetry_file.FileTelemetrySink`
    (one append-only JSONL file per run); nothing in core imports a
    concrete sink.

    ``append_telemetry`` is the single verb: it accepts one
    :class:`TelemetryRecord` and persists it to the destination keyed by
    ``record.run_id``. Implementations must keep concurrent runs
    disjoint (records for different ``run_id`` values never interleave
    into one destination) and must not reorder or rewrite previously
    appended records.
    """

    def append_telemetry(self, record: TelemetryRecord) -> None: ...


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
    * ``delete_command(command_id)`` removes one row by its store-side
      ``id`` — queue hygiene, not retention (spec 00025 FR-10): the
      consumer deletes an applied row only after the steering domain
      event recording the application has committed to the ledger, so
      the operator fact outlives the queue row. Deleting an unknown id
      is a no-op.
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

    def delete_command(self, command_id: int) -> None: ...


__all__ = [
    "AttemptStore",
    "AuditRecord",
    "CURRENT_SCHEMA_VERSION",
    "ControlCommandRecord",
    "ControlCommandStore",
    "DomainEventStore",
    "EventRecord",
    "GraderResultRecord",
    "GraderResultStore",
    "GraderType",
    "LifecycleAlreadyExistsError",
    "LifecycleNotFoundError",
    "LifecycleStore",
    "OptimisticConcurrencyError",
    "SdkMessageRecord",
    "StoreConflictError",
    "StoreSchemaError",
    "TaskStore",
    "TelemetryRecord",
    "TelemetrySink",
]
