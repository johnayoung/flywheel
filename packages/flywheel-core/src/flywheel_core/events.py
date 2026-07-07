"""Domain events: the event-sourced source of truth for a lifecycle.

A :class:`Lifecycle` is the *fold* of a closed, ordered sequence of domain
events. This module defines that closed set plus the pure reducer
(:func:`apply` / :func:`replay`) that derives lifecycle state from it.

Two invariants make this safe to depend on:

- **Closed taxonomy.** Only the event types defined here may change lifecycle
  state. Pure-observability telemetry (``harness.iteration_completed`` and
  friends) lives outside this module and is never folded into state.
- **Purity.** Like :mod:`flywheel_core.lifecycle`, this module performs no
  json/pathlib/io. Serialization of an event to/from a store row is the
  concrete store's responsibility; here events are plain dataclasses.

``version`` semantics: it is the *domain-event offset*. Every domain event
advances ``version`` by exactly one, so for a folded lifecycle
``version == number of domain events folded``. This doubles as the
optimistic-concurrency compare-and-swap key when the store conditionally
appends the next event.
"""

from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field, replace
from datetime import datetime
from enum import Enum
from typing import ClassVar

from flywheel_core.lifecycle import Attempt, Lifecycle, Outcome, Status


class DomainEventKind(str, Enum):
    """Discriminator for the closed domain-event set.

    The value is the stable wire identifier a store persists alongside the
    serialized event payload.
    """

    LIFECYCLE_INITIALIZED = "lifecycle_initialized"
    TRANSITIONED_TO = "transitioned_to"
    BLOCKED = "blocked"
    UNBLOCKED = "unblocked"
    AWAITING_APPROVAL = "awaiting_approval"
    RETRY_SCHEDULED = "retry_scheduled"
    ATTEMPT_STARTED = "attempt_started"
    ATTEMPT_FINALIZED = "attempt_finalized"
    SESSION_RECORDED = "session_recorded"
    GRADER_EVALUATED = "grader_evaluated"
    COMMAND_APPLIED = "command_applied"
    LANDING_PARKED = "landing_parked"
    HELD_OUT_GATE_EVALUATED = "held_out_gate_evaluated"


@dataclass(frozen=True, kw_only=True)
class _DomainEventBase:
    """Fields shared by every domain event.

    ``sequence`` and ``id`` are assigned by the store on append and are
    ``None`` for in-memory events that have not been persisted yet. They are
    never read by the reducer; ordering during a fold is the caller's
    responsibility (the store returns events in ``sequence`` order).
    """

    run_id: str
    ts: datetime
    attempt_number: int | None = None
    sequence: int | None = None
    id: int | None = None


@dataclass(frozen=True, kw_only=True)
class LifecycleInitialized(_DomainEventBase):
    """Seeds the lifecycle. Must be the first event in any stream.

    Replaces the old ``create_lifecycle`` write: appending this event is what
    brings a lifecycle row into existence, so there is no window in which
    events exist before the lifecycle does.
    """

    KIND: ClassVar[DomainEventKind] = DomainEventKind.LIFECYCLE_INITIALIZED

    task_id: str
    worker_id: str = ""
    artifacts_dir: str = ""
    task_content_hash: str = ""
    source: str = ""


@dataclass(frozen=True, kw_only=True)
class TransitionedTo(_DomainEventBase):
    """A state-machine move. Carries the status and, for failure states, the
    error string. The reducer derives the retry-counter increment and the
    READY-clears-``blocked_requires_json`` rule from the edge."""

    KIND: ClassVar[DomainEventKind] = DomainEventKind.TRANSITIONED_TO

    target: Status
    error: str = ""


@dataclass(frozen=True, kw_only=True)
class Blocked(_DomainEventBase):
    """Records the machine-readable blocked-requires snapshot. Always paired
    with a subsequent ``TransitionedTo(INTERRUPTED)``."""

    KIND: ClassVar[DomainEventKind] = DomainEventKind.BLOCKED

    requires_json: str


@dataclass(frozen=True, kw_only=True)
class Unblocked(_DomainEventBase):
    """Audit witness that a blocked lifecycle's requirements were satisfied.
    The actual snapshot clear happens via the paired ``TransitionedTo(READY)``,
    so this event's fold is the identity (it still advances ``version``)."""

    KIND: ClassVar[DomainEventKind] = DomainEventKind.UNBLOCKED


@dataclass(frozen=True, kw_only=True)
class AwaitingApproval(_DomainEventBase):
    """Records which manual-grader gate a lifecycle is parking on.

    Always paired with a subsequent ``TransitionedTo(AWAITING_APPROVAL)``
    (or, on re-park to a later gate, a re-emit while the lifecycle is
    already in ``AWAITING_APPROVAL``). The reducer folds ``awaiting_ordinal``
    onto the lifecycle's ``awaiting_manual_ordinal`` field; the centralized
    clear on ``-> READY`` / ``-> DONE`` / ``-> FAILED_VALIDATION`` later
    nulls it again.
    """

    KIND: ClassVar[DomainEventKind] = DomainEventKind.AWAITING_APPROVAL

    awaiting_ordinal: int


@dataclass(frozen=True, kw_only=True)
class RetryScheduled(_DomainEventBase):
    """Audit witness of the harness's decision to retry. The retry-counter
    bump is applied by the paired ``TransitionedTo(READY)`` from a retry-source
    status, so this event's fold is the identity (it still advances
    ``version``)."""

    KIND: ClassVar[DomainEventKind] = DomainEventKind.RETRY_SCHEDULED

    retries_used: int
    max_retries: int


@dataclass(frozen=True, kw_only=True)
class AttemptStarted(_DomainEventBase):
    """Opens a new :class:`Attempt` in the lifecycle's attempts projection."""

    KIND: ClassVar[DomainEventKind] = DomainEventKind.ATTEMPT_STARTED

    number: int
    attempt_run_id: str
    started_at: datetime
    agent_context: Mapping[str, str] = field(default_factory=dict)


@dataclass(frozen=True, kw_only=True)
class AttemptFinalized(_DomainEventBase):
    """Closes the matching :class:`Attempt` and records the lifecycle's last
    agent output."""

    KIND: ClassVar[DomainEventKind] = DomainEventKind.ATTEMPT_FINALIZED

    number: int
    outcome: Outcome
    ended_at: datetime
    agent_output: str = ""
    error: str = ""


@dataclass(frozen=True, kw_only=True)
class SessionRecorded(_DomainEventBase):
    """Records the agent session id for resumption."""

    KIND: ClassVar[DomainEventKind] = DomainEventKind.SESSION_RECORDED

    session_id: str


@dataclass(frozen=True, kw_only=True)
class GraderEvaluated(_DomainEventBase):
    """A grader receipt. Projects a ``grader_results`` row; its fold onto the
    :class:`Lifecycle` dataclass is the identity (it still advances
    ``version``)."""

    KIND: ClassVar[DomainEventKind] = DomainEventKind.GRADER_EVALUATED

    ordinal: int
    grader_type: str
    passed: bool
    duration_ms: int
    grader_name: str | None = None
    grader_spec: Mapping[str, object] = field(default_factory=dict)
    payload: Mapping[str, object] = field(default_factory=dict)


@dataclass(frozen=True, kw_only=True)
class CommandApplied(_DomainEventBase):
    """Records an operator control command applied against the run.

    A ledger fact (spec 00025 FR-10): the operator intervention survives
    even after the applied ``control_commands`` queue row is deleted.
    ``ts`` is the applied-at moment; ``command_kind`` / ``command_payload``
    snapshot the verb and the operator-supplied fields; ``command_id``
    keeps the queue-row provenance. Its fold onto the :class:`Lifecycle`
    dataclass is the identity (it still advances ``version``).
    """

    KIND: ClassVar[DomainEventKind] = DomainEventKind.COMMAND_APPLIED

    command_kind: str
    command_payload: Mapping[str, object] = field(default_factory=dict)
    command_id: int | None = None


@dataclass(frozen=True, kw_only=True)
class LandingParked(_DomainEventBase):
    """Records that a DONE run's branch could not be landed at submit time and
    its worktree was parked for forensics.

    An audit-witness event (like :class:`Unblocked` / :class:`RetryScheduled`):
    landing happens after the run already finalized ``DONE``, the harness owns
    lifecycle transitions, and ``DONE`` is terminal — so this event's fold is
    the identity (it advances ``version`` only and performs no state change).
    The run stays ``Status.DONE``.

    ``park_kind`` discriminates the cause on one shared surface (see
    :data:`LANDING_PARK_KINDS`): ``"uncommitted-work"`` (the agent reached DONE
    with an uncommitted tree, spec 00027), ``"divergent-base"`` (the configured
    base could not fast-forward even after rebase + re-verify, spec 00026),
    ``"standing-verify"`` (the ``[submit] verify`` standing build invariant
    failed against the tree about to land, spec 00064), ``"held-out-gate"`` (the
    execute-time held-out landing gate blocked the land, spec 00050),
    ``"protected-paths"`` (the branch touched a protected path so the merge/PR
    was refused), ``"push-failed"`` (the PR strategy could not push the branch or
    open the pull request), or ``"submit-error"`` (the submit step raised and the
    exception was swallowed, leaving the worktree parked). ``detail`` is a
    human-readable reason, queryable via ``list_domain_events(run_id)``.
    """

    KIND: ClassVar[DomainEventKind] = DomainEventKind.LANDING_PARKED

    park_kind: str
    detail: str = ""


#: Per-grader output excerpt bound carried on a :class:`GateGraderReceipt`.
#: The excerpt is a *tail* of the grader's captured output, capped at this many
#: bytes and stored raw; redaction is a render-time concern, never applied at
#: persist time (spec 00073, D-2). Mirrors the command-runner's stream tail
#: bound so a receipt shows the same final content an operator would see.
GATE_EXCERPT_MAX_BYTES: int = 8192


@dataclass(frozen=True)
class GateGraderReceipt:
    """One executed held-out gate grader's receipt on a verdict record.

    Carries the diagnosable minimum for a single grader the held-out landing
    gate ran: its ``grader_name`` (``None`` for an unnamed grader), its
    ``passed`` outcome, and a bounded ``output_excerpt`` -- a raw tail of the
    grader's captured output capped at :data:`GATE_EXCERPT_MAX_BYTES` bytes
    (spec 00073, criteria 1/11). The excerpt retains the *final* content when
    the grader emitted more than the bound, so the reason a gate blocked is
    visible from the store alone. Stored raw: redaction is applied at render
    time, not persist time (D-2).
    """

    grader_name: str | None = None
    passed: bool = False
    output_excerpt: str = ""


@dataclass(frozen=True, kw_only=True)
class HeldOutGateEvaluated(_DomainEventBase):
    """Records a held-out landing-gate evaluation's verdict on the run's ledger.

    Emitted for *every* gate evaluation -- pass, fail, or no-gate (spec 00073,
    D-1) -- so a gate-decided park is diagnosable from the store alone rather
    than only from the in-process ``RunRecord``. An audit-witness event like
    :class:`LandingParked`: the gate runs after the run already finalized
    ``DONE`` and the harness owns lifecycle transitions, so this event's fold is
    the identity (it advances ``version`` only and performs no state change).

    ``outcome`` is the terminal gate outcome's stable wire value
    (``"no_gate"`` / ``"pass"`` / ``"fail"``) -- a plain string because the gate
    engine's ``GateOutcome`` enum lives in the orchestrator, downstream of this
    pure module. ``reason`` mirrors the verdict's operator-readable summary.
    ``receipts`` carries one :class:`GateGraderReceipt` per executed held-out
    grader in execution order: populated for ``pass``/``fail`` verdicts, empty
    for ``no_gate`` and for a load-time fail-closed where no grader ran. A
    ``no_gate`` record with empty receipts is still distinguishable from *no
    record at all* -- the evaluation happened and found nothing to gate.
    """

    KIND: ClassVar[DomainEventKind] = DomainEventKind.HELD_OUT_GATE_EVALUATED

    outcome: str
    reason: str = ""
    receipts: tuple[GateGraderReceipt, ...] = ()


# Landing-park cause vocabulary. Every site that suppresses a land -- the merge
# and PR protected-path refusals, a failed push / PR open, a swallowed submit
# error, and the held-out landing gate -- names its cause with one of these
# stable kebab-case spellings, kept identical across the worker, PR, and
# orchestrate emit sites so a single status reader recognizes them all. The
# first three predate this set; the rest were added so every land-suppression
# path leaves a durable witness.
PARK_KIND_UNCOMMITTED_WORK = "uncommitted-work"
PARK_KIND_DIVERGENT_BASE = "divergent-base"
PARK_KIND_STANDING_VERIFY = "standing-verify"
PARK_KIND_HELD_OUT_GATE = "held-out-gate"
PARK_KIND_PROTECTED_PATHS = "protected-paths"
PARK_KIND_PUSH_FAILED = "push-failed"
PARK_KIND_SUBMIT_ERROR = "submit-error"

LANDING_PARK_KINDS: frozenset[str] = frozenset(
    {
        PARK_KIND_UNCOMMITTED_WORK,
        PARK_KIND_DIVERGENT_BASE,
        PARK_KIND_STANDING_VERIFY,
        PARK_KIND_HELD_OUT_GATE,
        PARK_KIND_PROTECTED_PATHS,
        PARK_KIND_PUSH_FAILED,
        PARK_KIND_SUBMIT_ERROR,
    }
)


DomainEvent = (
    LifecycleInitialized
    | TransitionedTo
    | Blocked
    | Unblocked
    | AwaitingApproval
    | RetryScheduled
    | AttemptStarted
    | AttemptFinalized
    | SessionRecorded
    | GraderEvaluated
    | CommandApplied
    | LandingParked
    | HeldOutGateEvaluated
)


class EventReplayError(ValueError):
    """Raised when an event stream cannot be folded into a coherent state.

    Surfaced loudly rather than coerced: an empty stream, a stream that does
    not begin with :class:`LifecycleInitialized`, a duplicate initialization,
    or a finalize for an attempt that was never started all indicate a corrupt
    log, not a recoverable condition. An illegal state-machine edge raises
    :class:`flywheel_core.lifecycle.LifecycleTransitionError` (from
    ``apply_transition``) for the same reason.
    """


def _clone(state: Lifecycle) -> Lifecycle:
    """Return an independent copy so :func:`apply` never mutates its input."""
    return Lifecycle(
        task_id=state.task_id,
        run_id=state.run_id,
        worker_id=state.worker_id,
        status=state.status,
        timestamps=dict(state.timestamps),
        version=state.version,
        retries=state.retries,
        error=state.error,
        agent_output=state.agent_output,
        attempts=[
            replace(a, agent_context=dict(a.agent_context))
            for a in state.attempts
        ],
        session_id=state.session_id,
        artifacts_dir=state.artifacts_dir,
        blocked_requires_json=state.blocked_requires_json,
        awaiting_manual_ordinal=state.awaiting_manual_ordinal,
        task_content_hash=state.task_content_hash,
        source=state.source,
    )


def _finalize_attempt(state: Lifecycle, event: AttemptFinalized) -> None:
    for attempt in state.attempts:
        if attempt.number == event.number:
            attempt.ended_at = event.ended_at
            attempt.outcome = event.outcome
            attempt.agent_output = event.agent_output
            attempt.error = event.error
            state.agent_output = event.agent_output
            return
    raise EventReplayError(
        f"AttemptFinalized for unknown attempt number {event.number}"
    )


def apply(state: Lifecycle | None, event: DomainEvent) -> Lifecycle:
    """Fold one domain event onto ``state``, returning a new lifecycle.

    ``state`` is ``None`` only for the leading :class:`LifecycleInitialized`;
    every other event requires an existing state. The returned lifecycle is a
    fresh object — the input is never mutated — so a fold is referentially
    transparent and repeatable.
    """
    if isinstance(event, LifecycleInitialized):
        if state is not None:
            raise EventReplayError(
                "LifecycleInitialized must be the first and only seed event"
            )
        return Lifecycle(
            task_id=event.task_id,
            run_id=event.run_id,
            worker_id=event.worker_id,
            artifacts_dir=event.artifacts_dir,
            task_content_hash=event.task_content_hash,
            source=event.source,
            status=Status.PENDING,
            version=1,
        )

    if state is None:
        raise EventReplayError(
            "event stream must begin with LifecycleInitialized"
        )

    new = _clone(state)
    new.version += 1

    if isinstance(event, TransitionedTo):
        new.apply_transition(event.target, error=event.error, now=event.ts)
    elif isinstance(event, Blocked):
        new.blocked_requires_json = event.requires_json
    elif isinstance(event, AwaitingApproval):
        new.awaiting_manual_ordinal = event.awaiting_ordinal
    elif isinstance(event, AttemptStarted):
        new.attempts.append(
            Attempt(
                number=event.number,
                started_at=event.started_at,
                run_id=event.attempt_run_id,
                agent_context=dict(event.agent_context),
            )
        )
    elif isinstance(event, AttemptFinalized):
        _finalize_attempt(new, event)
    elif isinstance(event, SessionRecorded):
        new.session_id = event.session_id
    elif isinstance(
        event,
        (
            Unblocked,
            RetryScheduled,
            GraderEvaluated,
            CommandApplied,
            LandingParked,
            HeldOutGateEvaluated,
        ),
    ):
        # Identity fold: these carry audit intent or project a separate table.
        # They still advance version as members of the domain-event sequence.
        pass
    else:  # pragma: no cover - exhaustive over the closed union
        raise EventReplayError(
            f"unknown domain event: {type(event).__name__}"
        )

    return new


def replay(events: Iterable[DomainEvent]) -> Lifecycle:
    """Fold an ordered domain-event sequence into a :class:`Lifecycle`.

    The sequence must begin with :class:`LifecycleInitialized`. Raises
    :class:`EventReplayError` for an empty stream and surfaces any
    state-machine illegality from the underlying transitions.
    """
    state: Lifecycle | None = None
    for event in events:
        state = apply(state, event)
    if state is None:
        raise EventReplayError("cannot replay an empty event stream")
    return state
