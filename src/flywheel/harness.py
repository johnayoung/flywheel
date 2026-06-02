"""Harness — single-task orchestration loop.

Wires :mod:`flywheel.invoker`, :mod:`flywheel.envelope`,
:mod:`flywheel.prompt`, :mod:`flywheel.grader_command`, and
:mod:`flywheel.grader_transcript` into one cohesive driver. Given a
:class:`~flywheel.task.Task` and a :class:`~flywheel.lifecycle.Lifecycle`,
:func:`run_task` carries the lifecycle to a terminal status (``done`` /
``failed``) or pauses it (``interrupted``) using a single owned state
machine.

The harness is the **sole owner of lifecycle transitions** for the
flywheel package: no other module mutates ``Lifecycle.status`` during a
run. Graders persist receipts; the invoker captures SDK signals; the
envelope parser classifies the iteration outcome — but only this module
calls :meth:`Lifecycle.transition_to`.

Per ``docs/loop.md``'s detection map, this module implements concrete
dispatches for:

* envelope ``intent`` (``verify``, ``blocked``, ``continue``, ``abort``)
* budget breaches recorded by the transcript runner (``failed_validation``)
* malformed / missing / duplicate / truncated envelopes (Protocol-failure
  outcomes — never silently coerced to ``continue``)
* invoker crashes (recorded coarsely; refined classification remains
  ``TODO`` per the spec)
* ``rate_limited`` events (observed and surfaced via emitted events;
  classification stays transient per the spec)

Each ``harness.iteration_completed`` event also carries the iteration's
raw context-pressure signals: a ``usage`` breakdown (input / output /
cache-creation / cache-read tokens plus the summed ``total_tokens``), the
SDK-reported ``total_cost_usd``, and ``num_turns``. Token fields are
per-iteration deltas — consumers cumulate by summing the audit stream;
the harness keeps no running counter. ``total_cost_usd`` and ``num_turns``
are emitted verbatim from :class:`InvocationSignals` and are
session-cumulative as the SDK reports them. Utilization% and the 50 / 75
/ 90 threshold-crossing signals listed in ``docs/vision.md`` remain
future work (no window-capacity source today).

The mechanical cases the safety-net work landed — repeated-failure
``STUCK`` and tuple-repetition ``THRASH`` via :mod:`flywheel.loop_guard`,
plus the in-harness hang watchdog mechanism — already run; see
:func:`_drive_iterations` for the wiring. What genuinely remains TODO
from ``docs/loop.md`` is **explicitly deferred** here: thrash sub-problems
(b) net-diff and (c) input-novelty, the still-ungrounded
``hang_timeout_seconds`` default value (the mechanism ships, the
threshold is operator-supplied until research grounds one), the
context-recovery policy, fine-grained crash classification, and
``blocked_implicit`` "same question re-asked" semantic detection. See
:data:`_DEFERRED_LOOP_SUBSYSTEMS` for the canonical list. The harness
does not paper over deferred items with stub heuristics.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Protocol, runtime_checkable

from claude_agent_sdk import AssistantMessage, Message, ResultMessage

from flywheel.envelope import (
    BlockedRequirement,
    CommandGraderRequirement,
    DuplicateEnvelope,
    EnvVarSetRequirement,
    EnvelopeResult,
    FileExistsRequirement,
    Intent,
    MalformedEnvelope,
    MissingEnvelope,
    TruncatedEnvelope,
    ValidEnvelope,
)
from flywheel.grader_command import run_command_graders
from flywheel.grader_rubric import (
    JudgeInvoke,
    RubricJudgeError,
    run_rubric_graders,
)
from flywheel.grader_transcript import (
    _USAGE_TOKEN_KEYS,
    TranscriptObservation,
    first_breach,
    run_transcript_graders,
    total_tokens_from_usage,
)
from flywheel.events import (
    AttemptFinalized,
    AttemptStarted,
    Blocked,
    DomainEvent,
    LifecycleInitialized,
    TransitionedTo,
)
from flywheel.invoker import (
    IterationResult,
    _serialize_sdk_message,
    invoke_iteration,
)
from flywheel.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel.loop_guard import (
    LoopGuard,
    LoopGuardConfig,
    LoopGuardVerdict,
    LoopGuardVerdictKind,
)
from flywheel.prompt import IterationInputs, RubricFindings, build_iteration_prompt
from flywheel.store_protocols import (
    EventRecord,
    GraderResultRecord,
    LifecycleAlreadyExistsError,
    SdkMessageRecord,
)
from flywheel.loaders import task_digest
from flywheel.task import CommandGrader, RubricGrader, Task, TranscriptGrader


# Loop.md flags these subsystems as still-TODO after the safety-net work
# landed. They are intentionally not implemented here so the rubric's
# "not silently faked" assertion holds. The mechanical detectors that
# did ship (repeated-failure STUCK, tuple-repetition THRASH, the hang
# watchdog mechanism) live in flywheel.loop_guard and _drive_iterations
# and are deliberately NOT in this list.
_DEFERRED_LOOP_SUBSYSTEMS: tuple[str, ...] = (
    "thrash net-diff detection (sub-problem b)",
    "thrash input-novelty score (sub-problem c)",
    "hang threshold default value (mechanism shipped, value ungrounded)",
    "context-recovery policy",
    "fine-grained crash classification",
    "blocked_implicit same-question-re-asked detection",
)


@runtime_checkable
class HarnessStore(Protocol):
    """Composite store contract the harness requires.

    Satisfied by :class:`flywheel.store_memory.InMemoryStore` and
    :class:`flywheel.store_sqlite.SqliteStore`. The harness operates only
    against this Protocol, not against a concrete backend — see the
    roadmap-10 constraint on backend-agnosticism.
    """

    def create_lifecycle(self, lifecycle: Lifecycle) -> None: ...

    def update_lifecycle(
        self,
        lifecycle: Lifecycle,
        *,
        expected_version: int,
    ) -> None: ...

    def load_lifecycle(self, run_id: str) -> Lifecycle | None: ...

    def save_task(self, task: Task, *, now: datetime) -> str: ...

    def append_domain_event(
        self,
        event: DomainEvent,
        *,
        expected_version: int,
    ) -> Lifecycle: ...

    def list_domain_events(self, run_id: str) -> list[DomainEvent]: ...

    def save_attempt(self, run_id: str, attempt: Attempt) -> None: ...

    def list_attempts(self, run_id: str) -> list[Attempt]: ...

    def append_event(self, event: EventRecord) -> EventRecord: ...

    def append_sdk_message(
        self, message: SdkMessageRecord
    ) -> SdkMessageRecord: ...

    def save_sdk_messages(
        self,
        run_id: str,
        attempt_number: int,
        iteration_number: int,
        messages: Sequence[Mapping[str, Any]],
    ) -> list[Any]: ...

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord: ...

    def list_grader_results(
        self,
        run_id: str,
        attempt_number: int,
    ) -> list[GraderResultRecord]: ...


@dataclass(frozen=True, kw_only=True)
class InvocationRequest:
    """Arguments handed to the harness's invoke callable.

    Decoupled from :func:`flywheel.invoker.invoke_iteration` so the
    harness can hand the transcript graders (which a production invoker
    wraps via :func:`enforce_transcript_limits`) plus the attempt /
    iteration context to a substitutable transport, without each
    invoker having to re-derive that context from the transcript text.

    ``on_message`` is the harness's per-message persistence observer.
    Invokers must call it once for every SDK :class:`Message` they
    observe, the instant it arrives, before returning the
    :class:`IterationResult` — the SDK-backed default invoker does this
    by forwarding it to :func:`invoke_iteration`, and test invokers
    that construct an :class:`IterationResult` directly must call it
    explicitly over ``IterationResult.messages``. The observer is
    expected to swallow its own exceptions (the harness captures the
    first error into a per-iteration sentinel and re-raises after the
    invoker returns), so invokers do not need to wrap it.
    """

    prompt: str
    transcript_graders: tuple[TranscriptGrader, ...]
    attempt_number: int
    iteration_number: int
    on_message: Callable[[Message], None] | None = None


InvokeFunc = Callable[[InvocationRequest], Awaitable[IterationResult]]


@dataclass(frozen=True, kw_only=True)
class HarnessConfig:
    """Per-run harness knobs.

    ``max_retries`` is the retry budget consumed by
    :meth:`Lifecycle.is_retry_eligible` — the harness delegates the rule,
    it does not re-derive it.

    ``max_iterations_per_attempt`` caps the inner ``intent=continue``
    loop within one ``Attempt``. The default of 1 means a single
    invocation per Attempt; raise it to allow multi-turn agents to
    iterate within one attempt before validation. When the cap is
    reached without a terminal envelope the Attempt finalizes as an
    agent error (no silent coercion to ``verify``).

    ``artifacts_root``, when set, is the base directory under which the
    harness creates per-attempt subdirectories. Each Attempt gets a
    deterministic ``attempt-NNN`` subdir, surfaced to grader runners
    that write logs (e.g. the command grader's ``stdout_path`` /
    ``stderr_path``). If left ``None``, the lifecycle's
    ``artifacts_dir`` is used; if that is also empty, command graders
    do not write per-attempt artifact files (their payloads still
    record stdout/stderr tails).

    ``agent_context`` is the immutable snapshot persisted onto every
    ``Attempt.agent_context`` per ``docs/task-lifecycle.md`` — model id,
    model version, agent-SDK version, prompt-template hash. The harness
    does not mutate or extend the supplied mapping; later analysis can
    distinguish model swaps from regressions because every Attempt
    carries exactly the context that produced it.

    ``worktree`` is the per-attempt sandbox the working agent uses. It is
    the working directory for both the rubric judge and the ``command``
    graders, so deterministic checks grade the tree the agent actually
    edited rather than the harness's ambient CWD. When ``None`` and the
    task declares a :class:`RubricGrader`, the rubric runner raises
    :class:`RubricJudgeError` ("worktree not available") which the harness
    routes through ``INTERNAL_ERROR``; when ``None``, command graders fall
    back to the harness process CWD (the bash worker chdirs into the
    sandbox, so that path stays correct). Workflow CLIs populate this with
    their sandbox path.

    ``rubric_judge_model`` is the default model for rubric judges when
    the per-grader ``RubricGrader.judge_model`` is unset; ``None`` falls
    through to the SDK's own default. ``rubric_judge_max_turns`` caps
    the per-judge-call turn budget (default 8).

    ``rubric_judge_invoke`` is a test seam: when set, the harness passes
    it to ``run_rubric_graders`` instead of the runner's default fresh
    ``claude_agent_sdk.query`` invoker. Production callers leave it
    ``None``.

    ``loop_guard`` carries the thresholds for the repeated-failure (STUCK)
    and identical-tuple-repeat (THRASH) detectors in
    :mod:`flywheel.loop_guard`. A fresh :class:`LoopGuard` is constructed
    per attempt from this config; each iteration's
    ``signals.tool_interactions`` is fed into it in arrival order. Each
    threshold disables independently via ``None`` / ``0``; default values
    keep the detectors on without tripping the existing harness suite.
    """

    max_retries: int = 0
    max_iterations_per_attempt: int = 1
    artifacts_root: str | os.PathLike[str] | None = None
    agent_context: Mapping[str, str] = field(default_factory=dict)
    worktree: str | os.PathLike[str] | None = None
    rubric_judge_model: str | None = None
    rubric_judge_max_turns: int = 8
    rubric_judge_invoke: JudgeInvoke | None = None
    loop_guard: LoopGuardConfig = field(default_factory=LoopGuardConfig)


@dataclass(frozen=True, kw_only=True)
class HarnessOutcome:
    """Final return value of :func:`run_task`.

    ``lifecycle`` is the mutated lifecycle in its terminal-or-paused
    state. ``attempts`` is a read-only snapshot of every Attempt
    persisted during the run, in ``number`` order; callers can also
    reload it from ``store.list_attempts`` and the two agree.
    """

    lifecycle: Lifecycle
    attempts: tuple[Attempt, ...]


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _default_monotonic() -> float:
    return time.monotonic()


async def _default_invoke(request: InvocationRequest) -> IterationResult:
    """Production invoke seam: delegate to :func:`invoke_iteration`.

    Hard-cap enforcement via :func:`enforce_transcript_limits` is the
    invoker's job, not the harness's. This default keeps a clean test
    seam: tests pass a stub invoke, production passes ``None`` and gets
    the SDK-backed default. ``request.on_message`` is forwarded to the
    SDK invoker so the harness's per-message persistence observer fires
    as messages arrive.
    """
    return await invoke_iteration(
        prompt=request.prompt, on_message=request.on_message
    )


def _append(
    lifecycle: Lifecycle,
    event: DomainEvent,
    *,
    store: HarnessStore,
) -> None:
    """Append one domain event and re-sync the in-memory lifecycle.

    This is the single authoritative state-write seam. The store folds
    ``event`` onto the persisted projection, advances the lifecycles /
    attempts projections, and appends the event row in one atomic unit;
    the returned :class:`Lifecycle` is the new source-of-truth state.
    The caller's in-memory ``lifecycle`` is reconciled from it (including
    the version, which is the optimistic-concurrency key for the next
    append) so a subsequent ``_append`` presents the right
    ``expected_version``.

    State and timeline can no longer diverge: there is no separate
    "mutate row, then emit event" pair — the append *is* the transition.
    """
    folded = store.append_domain_event(
        event, expected_version=lifecycle.version
    )
    lifecycle.replace_from(folded)
    lifecycle.attempts = list(folded.attempts)


def _transition(
    lifecycle: Lifecycle,
    target: Status,
    *,
    store: HarnessStore,
    error: str = "",
    now: Callable[[], datetime] | None = None,
) -> None:
    """Apply one lifecycle transition by appending a ``TransitionedTo``
    domain event.

    Centralized so every status change in the harness goes through the
    same event-sourced path. The signature is unchanged from the
    pre-event-sourcing version so every call site is untouched; the body
    now appends a domain event (which folds the legal-edge, retry-counter,
    and blocked-snapshot-clear rules) instead of mutating the row and
    issuing a separate ``update_lifecycle``.
    """
    clock = now or _utcnow
    _append(
        lifecycle,
        TransitionedTo(
            run_id=lifecycle.run_id,
            ts=clock(),
            target=target,
            error=error,
        ),
        store=store,
    )


class _AuditWriteError(Exception):
    """Sentinel exception raised when an audit-stream write fails.

    Carries the structured detail used to populate the final
    ``harness.audit_write_failed`` event and the
    :class:`Attempt`/`:class:`Lifecycle` finalization. The harness catches
    this exception at the :func:`_run_attempt` boundary; everywhere else
    it propagates so the caller (e.g. the outer ``run_task`` retry-arm
    or :func:`finalize_stranded_lifecycle`) can surface the failure.
    """

    def __init__(
        self,
        *,
        failing_method: str,
        inner: BaseException,
        attempt_number: int | None,
        iteration_number: int | None = None,
    ) -> None:
        super().__init__(
            f"audit write failed via {failing_method}: "
            f"{type(inner).__name__}: {inner}"
        )
        self.failing_method = failing_method
        self.inner = inner
        self.attempt_number = attempt_number
        self.iteration_number = iteration_number


class _HangDetected(Exception):
    """Sentinel raised when the hang watchdog cancels an invocation.

    Carries the context needed for the FR-3 finalization shape
    (``Outcome.INTERNAL_ERROR`` / ``running -> internal_error``). The
    ``harness.hang_detected`` audit event is emitted inside
    :func:`_invoke_with_watchdog` before this is raised; the
    :func:`_run_attempt` boundary catches this BEFORE
    :exc:`asyncio.CancelledError` so a watchdog-induced cancellation never
    reaches the operator-interrupt path (:func:`_handle_interrupt`).
    See ``.workflow/specs/00015-FEATURE-loop-safety-net.md`` FR-4.
    """

    def __init__(
        self,
        *,
        attempt_number: int,
        iteration_number: int,
        timeout_seconds: float,
        silence_seconds: float,
    ) -> None:
        super().__init__(
            f"hang watchdog: no SDK message for "
            f"{silence_seconds:.3f}s (threshold "
            f"{timeout_seconds:.3f}s)"
        )
        self.attempt_number = attempt_number
        self.iteration_number = iteration_number
        self.timeout_seconds = timeout_seconds
        self.silence_seconds = silence_seconds


def _emit(
    store: HarnessStore,
    *,
    run_id: str,
    kind: str,
    payload: Mapping[str, Any],
    attempt_number: int | None = None,
    now: Callable[[], datetime] | None = None,
    best_effort: bool = False,
) -> None:
    """Persist one :class:`EventRecord`.

    On any store-side exception, wraps the underlying error in
    :class:`_AuditWriteError` and re-raises so the harness can route
    the failure through the strict-audit policy (FR-5). When
    ``best_effort=True`` the exception is swallowed instead — used for
    the final ``harness.audit_write_failed`` emit so the failure path
    cannot itself loop.
    """
    clock = now or _utcnow
    try:
        store.append_event(
            EventRecord(
                run_id=run_id,
                ts=clock(),
                kind=kind,
                payload=dict(payload),
                attempt_number=attempt_number,
            )
        )
    except Exception as exc:
        if best_effort:
            return
        raise _AuditWriteError(
            failing_method="append_event",
            inner=exc,
            attempt_number=attempt_number,
        ) from exc


def _persist_sdk_message(
    store: HarnessStore,
    *,
    run_id: str,
    attempt_number: int,
    iteration_number: int,
    message: Message,
    now: Callable[[], datetime] | None = None,
) -> None:
    """Persist one SDK :class:`Message` the instant it is observed.

    Serializes ``message`` via :func:`_serialize_sdk_message`, builds a
    :class:`SdkMessageRecord`, and hands it to
    ``store.append_sdk_message`` — which allocates one tick from the
    per-run audit sequence counter, inserts the row, and notifies any
    listeners. On store failure raises :class:`_AuditWriteError` carrying
    the failing method and the attempt / iteration context so the
    harness can route the failure through the strict-audit policy
    (FR-5). The per-iteration batch write that
    :func:`save_sdk_messages` used to do is gone — sdk_messages rows
    are themselves the live progress signal.
    """
    clock = now or _utcnow
    payload = _serialize_sdk_message(message)
    message_type = str(payload.get("message_type", payload.get("type", "")))
    record = SdkMessageRecord(
        run_id=run_id,
        attempt_number=attempt_number,
        iteration_number=iteration_number,
        message_type=message_type,
        payload=payload,
        ts=clock(),
    )
    try:
        store.append_sdk_message(record)
    except Exception as exc:
        raise _AuditWriteError(
            failing_method="append_sdk_message",
            inner=exc,
            attempt_number=attempt_number,
            iteration_number=iteration_number,
        ) from exc


def _handle_audit_failure(
    exc: _AuditWriteError,
    *,
    store: HarnessStore,
    lifecycle: Lifecycle,
    attempt: Attempt | None,
    clock: Callable[[], datetime],
) -> None:
    """Best-effort finalization after a strict-audit write failure.

    Emits ``harness.audit_write_failed`` (best-effort — swallows nested
    failures so the audit path cannot itself loop), finalizes the open
    attempt as :attr:`Outcome.INTERNAL_ERROR`, and transitions the
    lifecycle to :attr:`Status.INTERNAL_ERROR`. The outer retry policy
    then decides whether the run continues or terminates.
    """
    error = (
        f"audit write failed: {type(exc.inner).__name__}: {exc.inner}"
    )
    _emit(
        store,
        run_id=lifecycle.run_id,
        kind="harness.audit_write_failed",
        payload={
            "failing_method": exc.failing_method,
            "error_type": type(exc.inner).__name__,
            "message": str(exc.inner),
            "attempt_number": exc.attempt_number,
            "iteration_number": exc.iteration_number,
        },
        attempt_number=exc.attempt_number,
        now=clock,
        best_effort=True,
    )
    if attempt is not None and attempt.ended_at is None:
        ended_at = clock()
        attempt.ended_at = ended_at
        attempt.outcome = Outcome.INTERNAL_ERROR
        attempt.error = error
        try:
            _append(
                lifecycle,
                AttemptFinalized(
                    run_id=lifecycle.run_id,
                    ts=ended_at,
                    attempt_number=attempt.number,
                    number=attempt.number,
                    outcome=Outcome.INTERNAL_ERROR,
                    ended_at=ended_at,
                    agent_output=attempt.agent_output,
                    error=error,
                ),
                store=store,
            )
        except Exception:
            pass
    try:
        _transition(
            lifecycle,
            Status.INTERNAL_ERROR,
            store=store,
            error=error,
            now=clock,
        )
    except Exception:
        pass


# Statuses from which the operator-interrupt finalizer can land cleanly on
# INTERRUPTED via a single transition. RUNNING covers cancellation mid-stream
# (invoker iterator); VALIDATING covers cancellation between invoke and
# graders or inside the rubric judge's await. Other statuses (READY between
# attempts, terminal, or already INTERRUPTED) leave the finalizer a no-op so
# a second signal during shutdown cannot corrupt the prior finalization.
_INTERRUPTIBLE_STATUSES: frozenset[Status] = frozenset(
    {Status.RUNNING, Status.VALIDATING}
)


def _handle_interrupt(
    *,
    store: HarnessStore,
    lifecycle: Lifecycle,
    attempt: Attempt | None,
    clock: Callable[[], datetime],
) -> None:
    """In-band finalization for an operator-driven SIGINT/SIGTERM.

    Mirrors :func:`_handle_audit_failure`: closes the open attempt as
    :attr:`Outcome.INTERNAL_ERROR`, emits a ``harness.interrupted``
    telemetry event, and transitions the lifecycle to
    :attr:`Status.INTERRUPTED`. INTERRUPTED is not a retry-source state, so
    the retry budget is preserved and the next worker start can resume the
    run through ``ready``.

    Idempotent: when the lifecycle is not in :data:`_INTERRUPTIBLE_STATUSES`
    (already finalized, between attempts, or terminal) the function is a
    no-op. This makes a second signal during shutdown safe — the cancel is
    scheduled at the next await point, but the synchronous finalization has
    already completed by then, so re-entering this helper just exits early.

    Best-effort emit / append: a store-side failure inside the audit emit or
    the AttemptFinalized append is swallowed (the audit path itself must not
    loop, mirroring :func:`_handle_audit_failure`). The status transition
    remains the source of truth.
    """
    if lifecycle.status not in _INTERRUPTIBLE_STATUSES:
        return
    reason = "operator interrupted mid-attempt"
    from_status = lifecycle.status.value
    try:
        _emit(
            store,
            run_id=lifecycle.run_id,
            kind="harness.interrupted",
            payload={
                "classification": "worker_interrupted",
                "from_status": from_status,
                "message": reason,
            },
            attempt_number=attempt.number if attempt is not None else None,
            now=clock,
            best_effort=True,
        )
    except _AuditWriteError:
        # best_effort=True swallows store errors already; this except is a
        # belt-and-braces guard so the interrupt finalizer can never itself
        # raise back into the caller.
        pass
    if attempt is not None and attempt.ended_at is None:
        ended_at = clock()
        attempt.ended_at = ended_at
        attempt.outcome = Outcome.INTERNAL_ERROR
        attempt.error = reason
        try:
            _append(
                lifecycle,
                AttemptFinalized(
                    run_id=lifecycle.run_id,
                    ts=ended_at,
                    attempt_number=attempt.number,
                    number=attempt.number,
                    outcome=Outcome.INTERNAL_ERROR,
                    ended_at=ended_at,
                    agent_output=attempt.agent_output,
                    error=reason,
                ),
                store=store,
            )
        except Exception:
            pass
        # Mirror finalize_stranded_lifecycle's telemetry shape so the
        # in-band path and the out-of-band recovery sweep produce the
        # same audit-stream signature for an operator-interrupted attempt.
        try:
            _emit(
                store,
                run_id=lifecycle.run_id,
                kind="harness.attempt_finalized",
                payload={
                    "number": attempt.number,
                    "outcome": Outcome.INTERNAL_ERROR.value,
                    "error": reason,
                },
                attempt_number=attempt.number,
                now=clock,
                best_effort=True,
            )
        except _AuditWriteError:
            pass
    try:
        _transition(
            lifecycle,
            Status.INTERRUPTED,
            store=store,
            now=clock,
        )
    except Exception:
        pass


def _handle_hang_detected(
    exc: _HangDetected,
    *,
    store: HarnessStore,
    lifecycle: Lifecycle,
    attempt: Attempt,
    clock: Callable[[], datetime],
) -> None:
    """Finalize an attempt whose invocation the hang watchdog cancelled.

    Mirrors the crash path: closes the open attempt as
    :attr:`Outcome.INTERNAL_ERROR` and transitions the lifecycle to
    :attr:`Status.INTERNAL_ERROR` (the infrastructure class, per FR-3 of
    ``.workflow/specs/00015-FEATURE-loop-safety-net.md``).

    The ``harness.hang_detected`` audit event was emitted inside
    :func:`_invoke_with_watchdog` before the cancel — this helper does not
    re-emit it. Idempotent against ``attempt.ended_at is not None`` so a
    near-simultaneous normal completion that already finalized the attempt
    cannot be double-written.
    """
    error = str(exc)
    if attempt.ended_at is None:
        _finalize_attempt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.INTERNAL_ERROR,
            error=error,
            clock=clock,
        )
    if lifecycle.status != Status.INTERNAL_ERROR:
        _transition(
            lifecycle,
            Status.INTERNAL_ERROR,
            store=store,
            error=error,
            now=clock,
        )


def _ensure_attempt_dir(
    config: HarnessConfig,
    lifecycle: Lifecycle,
    attempt_number: int,
) -> Path | None:
    base: str | os.PathLike[str] | None
    if config.artifacts_root is not None:
        base = config.artifacts_root
    elif lifecycle.artifacts_dir:
        base = lifecycle.artifacts_dir
    else:
        return None
    path = Path(base) / f"attempt-{attempt_number:03d}"
    path.mkdir(parents=True, exist_ok=True)
    return path


def _build_usage_breakdown(messages: Sequence[Message]) -> dict[str, int]:
    """Aggregate the iteration's per-field token usage breakdown.

    Mirrors :func:`_build_observation`'s ``total_tokens`` algorithm
    field-by-field so the emitted ``harness.iteration_completed`` payload's
    ``total_tokens`` equals ``observation.total_tokens`` for the same
    iteration (FR-3 of the context-pressure-telemetry spec).

    Semantics, per field:

    - Sum the value across every :class:`AssistantMessage` whose ``usage``
      dict carries the key.
    - When a :class:`ResultMessage` reports a *larger* aggregate (sum of all
      four fields) than the running breakdown, the ResultMessage's
      breakdown wins — the SDK sometimes leaves AssistantMessage usage
      empty and only reports totals at the end. This mirrors the
      ``max(running, total_tokens_from_usage(rm.usage))`` reconciliation in
      :func:`_build_observation`.
    """
    breakdown: dict[str, int] = {k: 0 for k in _USAGE_TOKEN_KEYS}
    for msg in messages:
        if isinstance(msg, AssistantMessage) and msg.usage:
            for key in _USAGE_TOKEN_KEYS:
                value = msg.usage.get(key)
                if value is None:
                    continue
                try:
                    breakdown[key] += int(value)
                except (TypeError, ValueError):
                    continue
        elif isinstance(msg, ResultMessage) and msg.usage:
            rm_breakdown: dict[str, int] = {k: 0 for k in _USAGE_TOKEN_KEYS}
            for key in _USAGE_TOKEN_KEYS:
                value = msg.usage.get(key)
                if value is None:
                    continue
                try:
                    rm_breakdown[key] += int(value)
                except (TypeError, ValueError):
                    continue
            if sum(rm_breakdown.values()) > sum(breakdown.values()):
                breakdown = rm_breakdown
    return breakdown


def _build_observation(
    messages: Sequence[Message],
    *,
    wall_seconds: float,
) -> TranscriptObservation:
    """Aggregate the iteration's ``messages`` into a
    :class:`TranscriptObservation`.

    Mirrors the running totals computed by
    :class:`flywheel.grader_transcript.TranscriptCounter` — when the
    invoker has already drained the stream, the harness recomputes the
    same totals from the recorded messages so the validation-time
    grader and the hard-limit enforcer converge on identical numbers.
    """
    turns = 0
    total_tokens = 0
    for msg in messages:
        if isinstance(msg, AssistantMessage):
            turns += 1
            total_tokens += total_tokens_from_usage(msg.usage)
        elif isinstance(msg, ResultMessage):
            if msg.usage:
                total_tokens = max(
                    total_tokens, total_tokens_from_usage(msg.usage)
                )
            if msg.num_turns is not None:
                turns = max(turns, msg.num_turns)
    return TranscriptObservation(
        turns=turns,
        total_tokens=total_tokens,
        wall_seconds=wall_seconds,
    )


def _envelope_protocol_error(envelope: EnvelopeResult) -> str:
    """Render a protocol-failure envelope as a short error string."""
    if isinstance(envelope, MissingEnvelope):
        return "protocol failure: missing iteration envelope"
    if isinstance(envelope, TruncatedEnvelope):
        return f"protocol failure: truncated envelope ({envelope.detail})"
    if isinstance(envelope, DuplicateEnvelope):
        return (
            f"protocol failure: duplicate envelope (count={envelope.count})"
        )
    if isinstance(envelope, MalformedEnvelope):
        return f"protocol failure: malformed envelope ({envelope.reason})"
    return "protocol failure: unrecognized envelope"


def _envelope_payload(envelope: EnvelopeResult) -> dict[str, Any]:
    """Event payload describing the envelope kind without leaking SDK types."""
    if isinstance(envelope, ValidEnvelope):
        return {
            "kind": "valid",
            "intent": envelope.intent.value,
            "reason": envelope.reason,
        }
    if isinstance(envelope, MissingEnvelope):
        return {"kind": "missing"}
    if isinstance(envelope, TruncatedEnvelope):
        return {"kind": "truncated", "detail": envelope.detail}
    if isinstance(envelope, DuplicateEnvelope):
        return {"kind": "duplicate", "count": envelope.count}
    if isinstance(envelope, MalformedEnvelope):
        return {
            "kind": "malformed",
            "reason": envelope.reason,
            "offending": envelope.offending,
        }
    return {"kind": "unknown"}


def _all_passed(results: Sequence[GraderResultRecord]) -> bool:
    return all(r.passed for r in results)


def _first_breach_across_graders(
    graders: Sequence[TranscriptGrader],
    observation: TranscriptObservation,
) -> str | None:
    for grader in graders:
        breach = first_breach(grader, observation)
        if breach is not None:
            return breach
    return None


def _grader_failure_error(
    results: Sequence[GraderResultRecord],
) -> str:
    for result in results:
        if not result.passed:
            name = result.grader_name or result.grader_type
            return f"{result.grader_type} grader {name!r} failed"
    return "grader failure"


def _collect_prior_rubric_findings(
    store: HarnessStore,
    run_id: str,
    current_attempt_number: int,
) -> tuple[RubricFindings, ...]:
    """Build ``IterationInputs.prior_rubric_findings`` from prior attempts.

    Walks attempts backwards from ``current_attempt_number - 1`` to find
    the most recent attempt that has failing rubric records. Returns the
    failing rubric rows from that attempt in ordinal order. Returns an
    empty tuple when no prior attempt carried any failing rubric record
    (including the first attempt of a lifecycle).

    A rubric record's ``passed`` field is the lifecycle-level pass/fail
    bit: the rubric runner stores ``passed=True`` for both
    well-formed-pass verdicts and ``unknown=True`` verdicts, so
    filtering on ``passed=False`` naturally excludes unknown verdicts
    from the next attempt's feedback section.
    """
    if current_attempt_number <= 1:
        return ()
    for prior in range(current_attempt_number - 1, 0, -1):
        rows = store.list_grader_results(run_id, prior)
        failing = [
            r
            for r in rows
            if r.grader_type == "rubric" and not r.passed
        ]
        if not failing:
            continue
        findings: list[RubricFindings] = []
        for row in sorted(failing, key=lambda r: r.ordinal):
            summary_raw = row.payload.get("summary")
            summary = summary_raw if isinstance(summary_raw, str) else ""
            findings.append(
                RubricFindings(
                    grader_name=row.grader_name or "<unnamed>",
                    attempt_number=row.attempt_number,
                    summary=summary,
                )
            )
        return tuple(findings)
    return ()


# SIGINT (Ctrl+C from operator) and SIGTERM (typical process shutdown)
# indicate the grader subprocess was killed externally, not that the
# code under test asserted false. The audit pattern these signals fit
# is "operator interrupted", so the harness must not record them as a
# validation failure that consumes the retry budget.
_OPERATOR_SIGNAL_NUMBERS: frozenset[int] = frozenset({2, 15})


def _signal_killed_grader(
    results: Sequence[GraderResultRecord],
) -> GraderResultRecord | None:
    """Return the first failing command grader killed by SIGINT/SIGTERM.

    Distinguishes operator-induced interruption (which must route to
    ``INTERRUPTED`` so the retry budget is preserved) from a grader that
    legitimately exited non-zero (``VALIDATION_FAILED``).
    """
    for result in results:
        if result.passed:
            continue
        if result.grader_type != "command":
            continue
        payload = result.payload
        if payload.get("termination") != "signal":
            continue
        signal_no = payload.get("signal")
        if isinstance(signal_no, int) and signal_no in _OPERATOR_SIGNAL_NUMBERS:
            return result
    return None


def _serialize_requires(
    requires: Sequence[BlockedRequirement],
) -> list[dict[str, Any]]:
    """Render ``requires`` as a stable, JSON-ready list of dicts.

    Order is preserved. The shape mirrors the parser input shape (each
    predicate as ``{type, ...fields}``) so a round trip through the
    persisted ``blocked_requires_json`` column reconstructs identical
    :class:`BlockedRequirement` instances. This is the single chokepoint
    for the serialization shape so the harness, the event payload, and
    recheck-time parsing cannot drift.
    """
    out: list[dict[str, Any]] = []
    for req in requires:
        if isinstance(req, CommandGraderRequirement):
            out.append({"type": "command_grader", "name": req.name})
        elif isinstance(req, FileExistsRequirement):
            out.append(
                {
                    "type": "file_exists",
                    "path": req.path,
                    "present": req.present,
                }
            )
        elif isinstance(req, EnvVarSetRequirement):
            out.append({"type": "env_var_set", "name": req.name})
    return out


def _validate_blocked_requires_against_task(
    requires: Sequence[BlockedRequirement],
    task: Task,
) -> str | None:
    """Return an error string when any predicate is unresolvable against
    ``task``; otherwise ``None``.

    Per spec FR-3, the envelope parser cannot validate ``command_grader``
    predicates in isolation (it has no :class:`Task`). The harness owns
    that check: a predicate that names a grader missing from
    ``task.graders`` or present but of the wrong grader type is a
    protocol failure.
    """
    grader_by_name: dict[str, Any] = {}
    for grader in task.graders:
        name = getattr(grader, "name", None)
        if isinstance(name, str) and name:
            grader_by_name[name] = grader
    for req in requires:
        if isinstance(req, CommandGraderRequirement):
            grader = grader_by_name.get(req.name)
            if grader is None:
                return (
                    f"command_grader predicate references unknown grader "
                    f"name {req.name!r}"
                )
            if not isinstance(grader, CommandGrader):
                grader_type = getattr(grader, "type", type(grader).__name__)
                return (
                    f"command_grader predicate {req.name!r} resolves to a "
                    f"{grader_type!r} grader, not 'command'"
                )
    return None


def _parse_blocked_requires_json(
    payload: str,
) -> tuple[BlockedRequirement, ...] | str:
    """Round-trip the persisted ``blocked_requires_json`` back into a
    typed predicate tuple, or return an error string.

    Reuses :class:`BlockedRequirement` dataclasses so the type contract is
    identical to the parse-time shape. Defers structural validation to a
    minimal inline pass (the persisted payload was emitted by the harness
    itself, so divergence implies data corruption rather than untrusted
    input).
    """
    try:
        data = json.loads(payload)
    except json.JSONDecodeError as exc:
        return f"blocked_requires_json is not valid JSON: {exc.msg}"
    if not isinstance(data, list):
        return (
            f"blocked_requires_json must decode to a list, "
            f"got {type(data).__name__}"
        )
    parsed: list[BlockedRequirement] = []
    for index, entry in enumerate(data):
        if not isinstance(entry, dict):
            return (
                f"blocked_requires_json[{index}] is not a JSON object "
                f"(got {type(entry).__name__})"
            )
        raw_type = entry.get("type")
        if raw_type == "command_grader":
            name = entry.get("name")
            if not isinstance(name, str) or not name:
                return (
                    f"blocked_requires_json[{index}] command_grader 'name' "
                    f"must be a non-empty string"
                )
            parsed.append(CommandGraderRequirement(name=name))
        elif raw_type == "file_exists":
            path = entry.get("path")
            if not isinstance(path, str) or not path:
                return (
                    f"blocked_requires_json[{index}] file_exists 'path' "
                    f"must be a non-empty string"
                )
            present = entry.get("present", True)
            if not isinstance(present, bool):
                return (
                    f"blocked_requires_json[{index}] file_exists 'present' "
                    f"must be a bool"
                )
            parsed.append(FileExistsRequirement(path=path, present=present))
        elif raw_type == "env_var_set":
            name = entry.get("name")
            if not isinstance(name, str) or not name:
                return (
                    f"blocked_requires_json[{index}] env_var_set 'name' "
                    f"must be a non-empty string"
                )
            parsed.append(EnvVarSetRequirement(name=name))
        else:
            return (
                f"blocked_requires_json[{index}] has unknown 'type' "
                f"{raw_type!r}"
            )
    return tuple(parsed)


# Statuses that require an ``error`` argument on transition_to per
# flywheel.lifecycle._REQUIRES_ERROR. Mirrored here so the harness's
# entry-crash recorder picks the right argument shape without reaching
# into the lifecycle module's private surface.
_TRANSITION_REQUIRES_ERROR: frozenset[Status] = frozenset(
    {Status.FAILED, Status.FAILED_VALIDATION, Status.INTERNAL_ERROR}
)


# Per-status walk from an entry-crash detection point to Status.FAILED.
# The state machine in :class:`Lifecycle.transition_to` only permits
# the listed edges; we pre-compute the sequence so the recorder applies
# the minimum number of legal transitions. Statuses absent from this
# map (DONE, FAILED, INTERRUPTED) are already terminal or quiescent --
# no walk is required and none is attempted.
_ENTRY_CRASH_PATH_TO_FAILED: dict[Status, tuple[Status, ...]] = {
    Status.PENDING: (Status.READY, Status.RUNNING, Status.FAILED),
    Status.READY: (Status.RUNNING, Status.FAILED),
    Status.RUNNING: (Status.FAILED,),
    Status.VALIDATING: (Status.INTERNAL_ERROR, Status.FAILED),
    Status.FAILED_VALIDATION: (Status.FAILED,),
    Status.INTERNAL_ERROR: (Status.FAILED,),
}


def _record_entry_crash(
    store: HarnessStore,
    lifecycle: Lifecycle,
    exception: BaseException,
    *,
    clock: Callable[[], datetime],
) -> None:
    """Emit one ``harness.crash`` event and walk to :attr:`Status.FAILED`.

    Used by :func:`run_task`'s top-level handler when an uncaught
    :class:`Exception` escapes the main loop after the lifecycle row
    has been persisted. Three secondary-failure shapes are explicitly
    handled:

    * An audit-write failure inside :func:`_emit` is swallowed via
      ``best_effort=True`` so the recorder cannot itself loop and mask
      the original exception (the caller re-raises after this returns).
    * A transition failure (e.g.
      :class:`~flywheel.store_protocols.OptimisticConcurrencyError`
      from a concurrent harness racing on the same ``run_id``)
      propagates: the loser must learn its lifecycle is owned by
      another worker.
    * A lifecycle already in a terminal or quiescent state
      (:attr:`Status.DONE`, :attr:`Status.FAILED`,
      :attr:`Status.INTERRUPTED`) is left untouched -- no walk is
      attempted because there is no legal edge that lands on FAILED
      from those states and re-emitting the crash event after the run
      is already closed would be misleading.

    Payload keys match the existing ``harness.crash`` convention so no
    new event schema is introduced: ``classification='entry_error'``,
    ``exception_type``, and ``message``.
    """
    error = (
        f"harness entry crash: {type(exception).__name__}: {exception}"
    )
    _emit(
        store,
        run_id=lifecycle.run_id,
        kind="harness.crash",
        payload={
            "classification": "entry_error",
            "exception_type": type(exception).__name__,
            "message": str(exception),
        },
        now=clock,
        best_effort=True,
    )

    path = _ENTRY_CRASH_PATH_TO_FAILED.get(lifecycle.status, ())
    for target in path:
        target_error = (
            error if target in _TRANSITION_REQUIRES_ERROR else ""
        )
        _transition(
            lifecycle,
            target,
            store=store,
            error=target_error,
            now=clock,
        )


async def run_task(
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    *,
    config: HarnessConfig | None = None,
    invoke: InvokeFunc | None = None,
    now: Callable[[], datetime] | None = None,
    monotonic: Callable[[], float] | None = None,
) -> HarnessOutcome:
    """Drive ``task`` against ``lifecycle`` to a terminal or paused status.

    The harness owns every transition. The flow per ``docs/loop.md``:

    1. Bring the lifecycle to ``ready`` (from ``pending`` or
       ``interrupted``).
    2. While not terminal:

       a. ``ready -> running``; start a new ``Attempt`` with the
          configured ``agent_context``.
       b. Build the iteration prompt; invoke the agent; on
          ``intent=continue`` loop within the same Attempt up to
          ``config.max_iterations_per_attempt``.
       c. Classify the terminal iteration:

          * crash -> ``running -> internal_error`` (``INTERNAL_ERROR``
            outcome; retry-eligible under the same budget as
            ``failed_validation``).
          * ``intent=blocked`` -> ``running -> interrupted``
            (``CANCELLED`` outcome; no retry consumed).
          * ``intent=abort`` -> ``running -> failed`` (``AGENT_ERROR``).
          * everything else (verify, breach, protocol failure, continue
            past cap) -> ``running -> validating``, then either
            ``validating -> done`` on full grader pass or
            ``validating -> failed_validation`` with a typed error.
       d. On ``failed_validation`` or ``internal_error``, consult
          :meth:`Lifecycle.is_retry_eligible`: transition to ``ready``
          when retries remain (emitting ``harness.retry_scheduled``),
          otherwise transition to ``failed`` with the inherited error.
    3. Return the lifecycle and the full Attempt history.

    ``invoke`` defaults to :func:`invoke_iteration` (via
    :func:`_default_invoke`); tests inject a stub callable returning
    pre-built :class:`IterationResult` instances.

    **Entry ordering and resume reconciliation.** The first store
    interaction appends a
    :class:`~flywheel.events.LifecycleInitialized` domain event, which
    creates the lifecycle projection as its side effect; any
    :class:`LifecycleAlreadyExistsError` (the resume case) is swallowed
    so we fall through to a follow-up
    :meth:`HarnessStore.load_lifecycle`. The persisted row wins: on
    resume the harness reconciles the caller's in-memory lifecycle
    fields (``status``, ``version``, ``retries``, ``error``,
    ``timestamps``, ``blocked_requires_json``, ``agent_output``,
    ``session_id``, ``artifacts_dir``, ``worker_id``) from the loaded
    row before any normalization runs. This guarantees the
    optimistic-concurrency check on the first ``update_lifecycle``
    sees the canonical ``expected_version`` even when the caller hands
    in a stale ``Lifecycle``.

    **Entry-time crash recording.** Once the lifecycle row exists, the
    main loop body runs under a top-level ``except Exception`` handler.
    Any uncaught exception emits a single ``harness.crash`` event with
    ``classification='entry_error'`` (best-effort — audit-write failure
    here is swallowed so it cannot mask the original exception) and
    walks the lifecycle to :attr:`Status.FAILED` via whichever
    transition path the state machine allows from the current status.
    FAILED is chosen over INTERRUPTED because an unhandled Python
    exception is not recoverable by the run itself — surfacing the
    failure as terminal matches the semantic and lets the worker exit
    non-zero with the original traceback intact (we re-raise after
    recording). :exc:`asyncio.CancelledError` and
    :exc:`KeyboardInterrupt` are deliberately *not* caught here:
    operator-driven shutdown is handled in-band one level down, at the
    :func:`_run_attempt` boundary, which routes the cancellation
    through :func:`_handle_interrupt` (closes the open attempt as
    ``INTERNAL_ERROR``, emits ``harness.interrupted``, transitions to
    :attr:`Status.INTERRUPTED`) before re-raising so the worker stops
    cleanly. :func:`finalize_stranded_lifecycle` remains the backstop
    for SIGKILL/OOM/reboot, where no in-process handler runs at all. If
    a concurrent harness lands a transition first, the loser's
    :class:`~flywheel.store_protocols.OptimisticConcurrencyError`
    surfaces to the caller verbatim — not swallowed — so the worker
    learns its run_id is racing.

    **Pre-lifecycle crashes.** Under event sourcing the projection row is
    created by the very first append (the ``LifecycleInitialized`` event),
    so there is no window in which events exist before the row — the
    silent-crash shape documented in
    ``.workflow/audits/08-recoverable-blocked-lifecycles.md`` is closed by
    construction. If that first append itself fails (catastrophic store
    breakage), the exception propagates straight to the caller with no
    partial state to reconcile; the worker-side circuit breaker covers the
    repeating-spawn shape.
    """

    config = config or HarnessConfig()
    invoker = invoke or _default_invoke
    clock = now or _utcnow
    mclock = monotonic or _default_monotonic

    # Seed the lifecycle by appending the first domain event. Under event
    # sourcing the lifecycle row *is* the projection of this event, so the
    # log and the row come into existence together — there is no window in
    # which events could exist before the row (the silent pre-lifecycle
    # crash shape that .workflow/audits/08-recoverable-blocked-lifecycles.md
    # documents). Append-then-swallow is the defensive resume shape.
    # Persist the task definition before seeding the lifecycle so every run
    # records the exact task it executed (end-to-end traceability). save_task
    # is content-addressed and idempotent, so resumes and retries re-saving
    # the same task are no-ops. The returned digest pins this run to its task
    # version via LifecycleInitialized -> Lifecycle.task_content_hash.
    task_content_hash = store.save_task(task, now=clock())
    # Reflect the pinned version on the in-memory lifecycle too. replace_from
    # treats task_content_hash as identity-shaping and won't copy it from the
    # persisted row on resume, so set it here from the digest we just computed.
    lifecycle.task_content_hash = task_content_hash
    try:
        _append(
            lifecycle,
            LifecycleInitialized(
                run_id=lifecycle.run_id,
                ts=clock(),
                task_id=lifecycle.task_id,
                worker_id=lifecycle.worker_id,
                artifacts_dir=lifecycle.artifacts_dir,
                task_content_hash=task_content_hash,
            ),
            store=store,
        )
    except LifecycleAlreadyExistsError:
        # Resume case: the row was persisted by a prior run_task call
        # (or by the workflow CLI's resume path). Fall through to the
        # follow-up load_lifecycle so the canonical state replaces any
        # stale fields the caller may be holding.
        pass

    # Reconcile from the persisted row so optimistic concurrency on
    # the first update_lifecycle sees the right expected_version. The
    # caller's Lifecycle is mutated in place so the HarnessOutcome
    # they receive at the end reflects the same identity.
    stored = store.load_lifecycle(lifecycle.run_id)
    if stored is not None:
        lifecycle.replace_from(stored)

    try:
        # Entry-time normalization: bring resumable states to `ready`.
        if lifecycle.status == Status.PENDING:
            _transition(lifecycle, Status.READY, store=store, now=clock)
        elif lifecycle.status == Status.INTERRUPTED:
            _transition(lifecycle, Status.READY, store=store, now=clock)

        while True:
            if lifecycle.status in (Status.DONE, Status.FAILED):
                break
            if lifecycle.status == Status.INTERRUPTED:
                # Paused this run; caller resumes by invoking run_task again.
                break

            if lifecycle.status == Status.READY:
                await _run_attempt(
                    task=task,
                    lifecycle=lifecycle,
                    store=store,
                    config=config,
                    invoker=invoker,
                    clock=clock,
                    mclock=mclock,
                )
                continue

            if lifecycle.status in (
                Status.FAILED_VALIDATION,
                Status.INTERNAL_ERROR,
            ):
                if lifecycle.is_retry_eligible(config.max_retries):
                    try:
                        _emit(
                            store,
                            run_id=lifecycle.run_id,
                            kind="harness.retry_scheduled",
                            payload={
                                "retries_used": lifecycle.retries,
                                "max_retries": config.max_retries,
                            },
                            now=clock,
                        )
                    except _AuditWriteError as exc:
                        # No active attempt to finalize between retries;
                        # emit the audit-failure event best-effort and
                        # terminate the run as FAILED with the audit
                        # error.
                        audit_error = (
                            f"audit write failed: "
                            f"{type(exc.inner).__name__}: {exc.inner}"
                        )
                        _emit(
                            store,
                            run_id=lifecycle.run_id,
                            kind="harness.audit_write_failed",
                            payload={
                                "failing_method": exc.failing_method,
                                "error_type": type(exc.inner).__name__,
                                "message": str(exc.inner),
                                "attempt_number": exc.attempt_number,
                                "iteration_number": exc.iteration_number,
                            },
                            now=clock,
                            best_effort=True,
                        )
                        try:
                            _transition(
                                lifecycle,
                                Status.FAILED,
                                store=store,
                                error=audit_error,
                                now=clock,
                            )
                        except Exception:
                            pass
                        break
                    _transition(
                        lifecycle, Status.READY, store=store, now=clock
                    )
                    continue
                terminal_error = (
                    lifecycle.error
                    or f"retries exhausted ({lifecycle.retries}/"
                    f"{config.max_retries})"
                )
                _transition(
                    lifecycle,
                    Status.FAILED,
                    store=store,
                    error=terminal_error,
                    now=clock,
                )
                continue

            # Defensive: any other state in the loop is a bug.
            raise RuntimeError(
                f"harness encountered unexpected lifecycle status: "
                f"{lifecycle.status.value}"
            )
    except Exception as exc:
        # asyncio.CancelledError and KeyboardInterrupt inherit from
        # BaseException (Python 3.8+), so this except deliberately
        # cannot catch them -- operator shutdown is handled in-band at
        # the _run_attempt boundary via _handle_interrupt (which finalizes
        # the open attempt to INTERRUPTED and re-raises the cancellation),
        # with finalize_stranded_lifecycle remaining as the
        # SIGKILL/OOM/reboot backstop. Everything else is internal
        # failure: record one harness.crash event and walk the lifecycle
        # to FAILED before re-raising so the worker subshell still exits
        # non-zero with the original traceback.
        _record_entry_crash(
            store, lifecycle, exc, clock=clock
        )
        raise

    attempts = tuple(store.list_attempts(lifecycle.run_id))
    return HarnessOutcome(lifecycle=lifecycle, attempts=attempts)


async def _run_attempt(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    config: HarnessConfig,
    invoker: InvokeFunc,
    clock: Callable[[], datetime],
    mclock: Callable[[], float],
) -> None:
    """Run one Attempt: invoke -> envelope -> intent -> graders -> finalize."""
    _transition(lifecycle, Status.RUNNING, store=store, now=clock)

    attempt_number = len(store.list_attempts(lifecycle.run_id)) + 1
    started_at = clock()
    attempt = Attempt(
        number=attempt_number,
        started_at=started_at,
        run_id=lifecycle.run_id,
        agent_context=dict(config.agent_context),
    )
    _append(
        lifecycle,
        AttemptStarted(
            run_id=lifecycle.run_id,
            ts=started_at,
            attempt_number=attempt_number,
            number=attempt_number,
            attempt_run_id=lifecycle.run_id,
            started_at=started_at,
            agent_context=dict(config.agent_context),
        ),
        store=store,
    )
    try:
        _emit(
            store,
            run_id=lifecycle.run_id,
            kind="harness.attempt_started",
            payload={
                "number": attempt_number,
                "agent_context": dict(config.agent_context),
            },
            attempt_number=attempt_number,
            now=clock,
        )

        attempt_dir = _ensure_attempt_dir(config, lifecycle, attempt_number)
        transcript_graders: tuple[TranscriptGrader, ...] = tuple(
            g for g in task.graders if isinstance(g, TranscriptGrader)
        )

        await _run_attempt_body(
            task=task,
            lifecycle=lifecycle,
            store=store,
            config=config,
            invoker=invoker,
            clock=clock,
            mclock=mclock,
            attempt=attempt,
            attempt_dir=attempt_dir,
            transcript_graders=transcript_graders,
        )
    except _AuditWriteError as exc:
        _handle_audit_failure(
            exc,
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            clock=clock,
        )
    except _HangDetected as exc:
        # Hang watchdog tripped: route to the FR-3 internal_error path
        # (mirrors the crash transition) rather than the operator-interrupt
        # path. The disambiguation gate is FR-4: only a watchdog-induced
        # CancelledError surfaces as _HangDetected; an unrelated cancel
        # leaves the flag clear and falls through to _handle_interrupt
        # below. The harness.hang_detected audit event was already emitted
        # inside _invoke_with_watchdog before the cancel.
        _handle_hang_detected(
            exc,
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            clock=clock,
        )
    except (asyncio.CancelledError, KeyboardInterrupt):
        # Operator-driven shutdown surfaced as cancellation. Close the
        # in-flight attempt to INTERRUPTED in-band so the audit stream
        # carries the exogenous stop and no lifecycle is left wedged in
        # RUNNING/VALIDATING. We then re-raise so run_task_object's
        # outer handler (and orchestrate above it) propagate the
        # cancellation and the worker actually stops -- the finalize
        # itself is purely additional bookkeeping, not a replacement
        # for the shutdown signal. recover_stranded_lifecycles remains
        # the SIGKILL/OOM/reboot backstop.
        _handle_interrupt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            clock=clock,
        )
        raise


async def _run_attempt_body(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    config: HarnessConfig,
    invoker: InvokeFunc,
    clock: Callable[[], datetime],
    mclock: Callable[[], float],
    attempt: Attempt,
    attempt_dir: Path | None,
    transcript_graders: tuple[TranscriptGrader, ...],
) -> None:
    """Inner body of :func:`_run_attempt`, split out so the audit-write
    sentinel exception can be caught at one boundary.

    Every store-write inside this function (either ``append_event`` via
    :func:`_emit` or ``append_sdk_message`` via the per-message observer
    built in :func:`_drive_iterations`) raises :class:`_AuditWriteError`
    on failure; the caller routes that through
    :func:`_handle_audit_failure`.
    """
    attempt_number = attempt.number
    # Fresh LoopGuard per attempt: each attempt is a new agent context,
    # so repeated-failure / thrash counters must not bleed across.
    loop_guard = LoopGuard(config.loop_guard)
    (
        iteration_result,
        _iterations_run,
        wall_seconds,
        loop_guard_verdict,
    ) = await _drive_iterations(
        task=task,
        lifecycle=lifecycle,
        store=store,
        config=config,
        invoker=invoker,
        clock=clock,
        mclock=mclock,
        attempt_number=attempt_number,
        transcript_graders=transcript_graders,
        loop_guard=loop_guard,
    )

    if iteration_result is None:
        # No invocation happened (cap <= 0). Treat as a protocol-class
        # agent error so the retry policy can still kick in.
        error = "no iterations were invoked"
        _transition(lifecycle, Status.VALIDATING, store=store, now=clock)
        _finalize_attempt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.AGENT_ERROR,
            error=error,
            clock=clock,
        )
        _transition(
            lifecycle,
            Status.FAILED_VALIDATION,
            store=store,
            error=error,
            now=clock,
        )
        return

    lifecycle.agent_output = iteration_result.transcript

    # Crash takes priority and goes straight to FAILED — refined
    # classification is deferred (see _DEFERRED_LOOP_SUBSYSTEMS).
    if iteration_result.failure is not None:
        failure = iteration_result.failure
        crash_error = f"crashed: {failure.error_type}: {failure.message}"
        _emit(
            store,
            run_id=lifecycle.run_id,
            kind="harness.crash",
            payload={
                "error_type": failure.error_type,
                "message": failure.message,
                "exit_code": failure.exit_code,
                "stderr": failure.stderr,
                "classification": "deferred",
            },
            attempt_number=attempt_number,
            now=clock,
        )
        _finalize_attempt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.INTERNAL_ERROR,
            error=crash_error,
            agent_output=iteration_result.transcript,
            clock=clock,
        )
        _transition(
            lifecycle,
            Status.INTERNAL_ERROR,
            store=store,
            error=crash_error,
            now=clock,
        )
        return

    # LoopGuard verdicts preempt envelope interpretation. A STUCK verdict
    # routes through the explicit-blocked shape (Outcome.CANCELLED,
    # Blocked domain event, running -> interrupted) so the retry budget is
    # preserved and an operator transition to READY clears the
    # blocked_requires_json snapshot. A THRASH verdict routes through the
    # cap-reached agent-error shape (Outcome.AGENT_ERROR,
    # running -> validating -> failed_validation) so the existing retry arm
    # applies ``is_retry_eligible``.
    if loop_guard_verdict is not None:
        if loop_guard_verdict.kind is LoopGuardVerdictKind.STUCK:
            _handle_loop_guard_stuck(
                store=store,
                lifecycle=lifecycle,
                attempt=attempt,
                attempt_number=attempt_number,
                verdict=loop_guard_verdict,
                transcript=iteration_result.transcript,
                clock=clock,
            )
            return
        if loop_guard_verdict.kind is LoopGuardVerdictKind.THRASH:
            _handle_loop_guard_thrash(
                store=store,
                lifecycle=lifecycle,
                attempt=attempt,
                attempt_number=attempt_number,
                verdict=loop_guard_verdict,
                transcript=iteration_result.transcript,
                clock=clock,
            )
            return

    envelope = iteration_result.envelope

    # ABORT and BLOCKED leave RUNNING directly without entering
    # VALIDATING — they are not validation outcomes, they are
    # agent-reported terminal / pause signals.
    if isinstance(envelope, ValidEnvelope):
        if envelope.intent == Intent.ABORT:
            reason = envelope.reason or "agent reported abort intent"
            _finalize_attempt(
                store=store,
                lifecycle=lifecycle,
                attempt=attempt,
                outcome=Outcome.AGENT_ERROR,
                error=reason,
                agent_output=iteration_result.transcript,
                clock=clock,
            )
            _transition(
                lifecycle,
                Status.FAILED,
                store=store,
                error=reason,
                now=clock,
            )
            return

        if envelope.intent == Intent.BLOCKED:
            # Task-aware validation: the envelope parser cannot see
            # ``task.graders``, so unresolvable command_grader predicates
            # surface here. Per spec FR-3 they are protocol failures, not
            # benign blocks — route through the same shape the parser-side
            # protocol_failure path uses (no harness.blocked emit).
            requires_error = _validate_blocked_requires_against_task(
                envelope.requires, task
            )
            if requires_error is not None:
                protocol_error = (
                    f"protocol failure: invalid blocked requires: "
                    f"{requires_error}"
                )
                _transition(
                    lifecycle, Status.VALIDATING, store=store, now=clock
                )
                _emit(
                    store,
                    run_id=lifecycle.run_id,
                    kind="harness.protocol_failure",
                    payload={
                        "kind": "invalid_blocked_requires",
                        "reason": requires_error,
                        "intent": envelope.intent.value,
                    },
                    attempt_number=attempt_number,
                    now=clock,
                )
                _finalize_attempt(
                    store=store,
                    lifecycle=lifecycle,
                    attempt=attempt,
                    outcome=Outcome.AGENT_ERROR,
                    error=protocol_error,
                    agent_output=iteration_result.transcript,
                    clock=clock,
                )
                _transition(
                    lifecycle,
                    Status.FAILED_VALIDATION,
                    store=store,
                    error=protocol_error,
                    now=clock,
                )
                return

            reason = envelope.reason or "agent reported blocked intent"
            requires_payload = _serialize_requires(envelope.requires)
            _finalize_attempt(
                store=store,
                lifecycle=lifecycle,
                attempt=attempt,
                outcome=Outcome.CANCELLED,
                error=reason,
                agent_output=iteration_result.transcript,
                clock=clock,
            )
            # Persist the structured snapshot as a Blocked domain event
            # before the INTERRUPTED transition so the projection carries
            # blocked_requires_json when the status change lands. The
            # reducer clears this field on every -> READY edge (recheck,
            # retry, normalization), centralized in Lifecycle.
            _append(
                lifecycle,
                Blocked(
                    run_id=lifecycle.run_id,
                    ts=clock(),
                    attempt_number=attempt_number,
                    requires_json=json.dumps(requires_payload),
                ),
                store=store,
            )
            _emit(
                store,
                run_id=lifecycle.run_id,
                kind="harness.blocked",
                payload={
                    "reason": reason,
                    "requires": requires_payload,
                },
                attempt_number=attempt_number,
                now=clock,
            )
            _transition(
                lifecycle,
                Status.INTERRUPTED,
                store=store,
                now=clock,
            )
            return

    # All remaining classifications (verify, breach, protocol failure,
    # continue past cap) route through VALIDATING because that is the
    # only legal predecessor of FAILED_VALIDATION / DONE per the state
    # machine.
    _transition(lifecycle, Status.VALIDATING, store=store, now=clock)

    observation = _build_observation(
        iteration_result.messages,
        wall_seconds=wall_seconds,
    )
    breach = _first_breach_across_graders(transcript_graders, observation)

    if breach is not None:
        _emit(
            store,
            run_id=lifecycle.run_id,
            kind="harness.budget_exceeded",
            payload={
                "breached": breach,
                "observed": {
                    "turns": observation.turns,
                    "total_tokens": observation.total_tokens,
                    "wall_seconds": observation.wall_seconds,
                },
            },
            attempt_number=attempt_number,
            now=clock,
        )
        run_transcript_graders(
            task,
            observation,
            store,
            run_id=lifecycle.run_id,
            attempt_number=attempt_number,
            command_passed=True,
            now=clock,
        )
        error = f"budget exceeded: {breach}"
        _finalize_attempt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.VALIDATION_FAILED,
            error=error,
            agent_output=iteration_result.transcript,
            clock=clock,
        )
        _transition(
            lifecycle,
            Status.FAILED_VALIDATION,
            store=store,
            error=error,
            now=clock,
        )
        return

    if isinstance(envelope, ValidEnvelope) and envelope.intent == Intent.VERIFY:
        await _validate(
            task=task,
            lifecycle=lifecycle,
            store=store,
            config=config,
            attempt=attempt,
            attempt_dir=attempt_dir,
            observation=observation,
            agent_output=iteration_result.transcript,
            clock=clock,
        )
        return

    if isinstance(envelope, ValidEnvelope) and envelope.intent == Intent.CONTINUE:
        cap = config.max_iterations_per_attempt
        error = (
            f"agent did not converge after {cap} iteration"
            f"{'s' if cap != 1 else ''}"
        )
        _finalize_attempt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.AGENT_ERROR,
            error=error,
            agent_output=iteration_result.transcript,
            clock=clock,
        )
        _transition(
            lifecycle,
            Status.FAILED_VALIDATION,
            store=store,
            error=error,
            now=clock,
        )
        return

    # Protocol failure: malformed / missing / duplicate / truncated.
    protocol_error = _envelope_protocol_error(envelope)
    _emit(
        store,
        run_id=lifecycle.run_id,
        kind="harness.protocol_failure",
        payload=_envelope_payload(envelope),
        attempt_number=attempt_number,
        now=clock,
    )
    _finalize_attempt(
        store=store,
        lifecycle=lifecycle,
        attempt=attempt,
        outcome=Outcome.AGENT_ERROR,
        error=protocol_error,
        agent_output=iteration_result.transcript,
        clock=clock,
    )
    _transition(
        lifecycle,
        Status.FAILED_VALIDATION,
        store=store,
        error=protocol_error,
        now=clock,
    )


def _loop_guard_requires_payload(
    verdict: LoopGuardVerdict,
) -> list[dict[str, Any]]:
    """Render the STUCK verdict as a structured ``requires_json`` payload.

    The harness records a :class:`Blocked` domain event for the
    repeated-failure path so the existing recoverable-blocked recovery
    flow (operator transition to READY clears ``blocked_requires_json``)
    applies unchanged. The synthesized list shape mirrors the parser
    input contract (each entry is ``{type, ...fields}``) and names the
    offending tool plus the input digest so an operator can identify
    which call kept failing.
    """
    return [
        {
            "type": "tool_loop_block",
            "tool_name": verdict.tool_name,
            "input_digest": verdict.input_digest,
        }
    ]


def _handle_loop_guard_stuck(
    *,
    store: HarnessStore,
    lifecycle: Lifecycle,
    attempt: Attempt,
    attempt_number: int,
    verdict: LoopGuardVerdict,
    transcript: str,
    clock: Callable[[], datetime],
) -> None:
    """Route a STUCK verdict through the recoverable-blocked path.

    Mirrors the explicit ``intent=blocked`` flow: finalize the attempt as
    :attr:`Outcome.CANCELLED`, append a :class:`Blocked` domain event with
    a synthesized ``requires_json`` describing the failing tool, emit a
    ``harness.stuck`` audit event, and transition ``running -> interrupted``.
    The retry budget is **not** consumed — INTERRUPTED is not a
    retry-source state, so :meth:`Lifecycle.is_retry_eligible` is
    unchanged and an operator transition to READY clears
    ``blocked_requires_json``.
    """
    reason = verdict.reason
    requires_payload = _loop_guard_requires_payload(verdict)
    _finalize_attempt(
        store=store,
        lifecycle=lifecycle,
        attempt=attempt,
        outcome=Outcome.CANCELLED,
        error=reason,
        agent_output=transcript,
        clock=clock,
    )
    _append(
        lifecycle,
        Blocked(
            run_id=lifecycle.run_id,
            ts=clock(),
            attempt_number=attempt_number,
            requires_json=json.dumps(requires_payload),
        ),
        store=store,
    )
    _emit(
        store,
        run_id=lifecycle.run_id,
        kind="harness.stuck",
        payload={
            "reason": reason,
            "tool_name": verdict.tool_name,
            "input_digest": verdict.input_digest,
            "requires": requires_payload,
        },
        attempt_number=attempt_number,
        now=clock,
    )
    _transition(
        lifecycle,
        Status.INTERRUPTED,
        store=store,
        now=clock,
    )


def _handle_loop_guard_thrash(
    *,
    store: HarnessStore,
    lifecycle: Lifecycle,
    attempt: Attempt,
    attempt_number: int,
    verdict: LoopGuardVerdict,
    transcript: str,
    clock: Callable[[], datetime],
) -> None:
    """Route a THRASH verdict through the cap-reached agent-error path.

    Mirrors the ``intent=continue`` past-cap flow: emit a
    ``harness.thrash_detected`` audit event, transition through VALIDATING
    so the FAILED_VALIDATION edge is legal, finalize the attempt as
    :attr:`Outcome.AGENT_ERROR`, and transition into FAILED_VALIDATION.
    The outer retry arm then applies :meth:`Lifecycle.is_retry_eligible`:
    READY when retries remain (with ``harness.retry_scheduled``), else
    FAILED.
    """
    reason = verdict.reason
    _emit(
        store,
        run_id=lifecycle.run_id,
        kind="harness.thrash_detected",
        payload={
            "reason": reason,
            "tool_name": verdict.tool_name,
            "input_digest": verdict.input_digest,
        },
        attempt_number=attempt_number,
        now=clock,
    )
    _transition(lifecycle, Status.VALIDATING, store=store, now=clock)
    _finalize_attempt(
        store=store,
        lifecycle=lifecycle,
        attempt=attempt,
        outcome=Outcome.AGENT_ERROR,
        error=reason,
        agent_output=transcript,
        clock=clock,
    )
    _transition(
        lifecycle,
        Status.FAILED_VALIDATION,
        store=store,
        error=reason,
        now=clock,
    )


# Default tick floor for the hang watchdog (seconds). The tick is
# ``min(hang_timeout / 4, 0.5)`` clamped to this floor so the watchdog
# adds no measurable overhead for long thresholds while still resolving
# subsecond timeouts quickly in tests. When the watchdog is disabled
# (``hang_timeout_seconds is None`` or ``<= 0``) the early return in
# :func:`_drive_iterations` skips this code path entirely.
_HANG_WATCHDOG_MIN_TICK_SECONDS: float = 0.01
_HANG_WATCHDOG_MAX_TICK_SECONDS: float = 0.5


async def _invoke_with_watchdog(
    *,
    invoker: InvokeFunc,
    request: InvocationRequest,
    hang_timeout: float,
    mclock: Callable[[], float],
    store: HarnessStore,
    run_id: str,
    attempt_number: int,
    iteration_number: int,
    clock: Callable[[], datetime],
) -> IterationResult:
    """Race ``invoker(request)`` against a hang watchdog.

    FR-3: when the invocation produces no SDK message for longer than
    ``hang_timeout`` seconds, the watchdog cancels the invocation task,
    emits ``harness.hang_detected``, and raises :class:`_HangDetected` so
    the :func:`_run_attempt` boundary routes the attempt to the FR-3
    ``internal_error`` path.

    The watchdog reads its heartbeat from ``request.on_message``: the
    caller (``_drive_iterations``) wraps its persistence observer with a
    closure that updates a ``last_activity`` slot on every SDK message,
    so any message — including a :class:`RateLimitEvent` or a
    :class:`ThinkingBlock`-bearing :class:`AssistantMessage` — counts as
    liveness. This reuses the seam already wired for per-message
    persistence so the watchdog adds no second subscription.

    FR-4: only a watchdog-induced cancel raises :class:`_HangDetected`. A
    cancel that arrives from outside (operator SIGINT/SIGTERM, parent
    task cancellation) leaves ``hang_tripped`` ``False`` and the
    :exc:`asyncio.CancelledError` propagates unchanged so the existing
    operator-interrupt path runs as today.

    Race handling: when the invocation result is already produced as the
    cancel races in, ``await invocation_task`` returns the result and we
    honor it; ``hang_tripped`` is consulted only when the await actually
    raised :exc:`asyncio.CancelledError`. This means a near-simultaneous
    normal completion finalizes exactly once.
    """
    last_activity = mclock()
    hang_tripped = False

    base_on_message = request.on_message

    def watchdog_on_message(msg: Message) -> None:
        nonlocal last_activity
        last_activity = mclock()
        if base_on_message is not None:
            base_on_message(msg)

    watched_request = InvocationRequest(
        prompt=request.prompt,
        transcript_graders=request.transcript_graders,
        attempt_number=request.attempt_number,
        iteration_number=request.iteration_number,
        on_message=watchdog_on_message,
    )

    # Wrap the invoker call in a coroutine so asyncio.create_task gets a
    # Coroutine[Any, Any, IterationResult] (InvokeFunc returns Awaitable
    # by contract).
    async def _drive_invocation() -> IterationResult:
        return await invoker(watched_request)

    invocation_task: asyncio.Task[IterationResult] = asyncio.create_task(
        _drive_invocation()
    )

    tick = max(
        _HANG_WATCHDOG_MIN_TICK_SECONDS,
        min(hang_timeout / 4.0, _HANG_WATCHDOG_MAX_TICK_SECONDS),
    )

    async def watchdog() -> None:
        nonlocal hang_tripped
        while True:
            try:
                await asyncio.sleep(tick)
            except asyncio.CancelledError:
                return
            if invocation_task.done():
                return
            if mclock() - last_activity > hang_timeout:
                hang_tripped = True
                invocation_task.cancel()
                return

    watchdog_task = asyncio.create_task(watchdog())

    try:
        try:
            return await invocation_task
        except asyncio.CancelledError:
            if hang_tripped:
                silence = mclock() - last_activity
                _emit(
                    store,
                    run_id=run_id,
                    kind="harness.hang_detected",
                    payload={
                        "iteration": iteration_number,
                        "hang_timeout_seconds": hang_timeout,
                        "silence_seconds": silence,
                    },
                    attempt_number=attempt_number,
                    now=clock,
                )
                raise _HangDetected(
                    attempt_number=attempt_number,
                    iteration_number=iteration_number,
                    timeout_seconds=hang_timeout,
                    silence_seconds=silence,
                )
            raise
    finally:
        if not watchdog_task.done():
            watchdog_task.cancel()
            try:
                await watchdog_task
            except BaseException:
                # Drain the watchdog. We never need its result and a
                # cancellation propagating from this finally would mask
                # the outgoing exception (hang, operator interrupt, or
                # successful return). Suppressing it preserves the
                # caller-visible control flow.
                pass


async def _drive_iterations(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    config: HarnessConfig,
    invoker: InvokeFunc,
    clock: Callable[[], datetime],
    mclock: Callable[[], float],
    attempt_number: int,
    transcript_graders: tuple[TranscriptGrader, ...],
    loop_guard: LoopGuard,
) -> tuple[IterationResult | None, int, float, LoopGuardVerdict | None]:
    """Run iterations until a non-``continue`` envelope, crash, or cap.

    Returns the last :class:`IterationResult` produced, the number of
    iterations that ran, the wall-seconds elapsed across them, and any
    :class:`LoopGuardVerdict` that tripped the safety net (FR-1 STUCK /
    FR-2 THRASH). The verdict is non-``None`` only when ``loop_guard``
    observed a repeating-tool pattern in the iteration's
    ``signals.tool_interactions``; the caller then routes the attempt
    through the matching transition. The wall-seconds value is what feeds
    the :class:`TranscriptObservation` so transcript graders see the same
    elapsed time regardless of which iteration sets the envelope.
    """
    iteration_result: IterationResult | None = None
    iteration_number = 0
    started_monotonic = mclock()
    wall_seconds = 0.0
    loop_guard_verdict: LoopGuardVerdict | None = None

    prior_rubric_findings = _collect_prior_rubric_findings(
        store, lifecycle.run_id, attempt_number
    )

    while iteration_number < config.max_iterations_per_attempt:
        iteration_number += 1

        prompt = build_iteration_prompt(
            task,
            lifecycle,
            IterationInputs(
                max_retries=config.max_retries,
                prior_rubric_findings=prior_rubric_findings,
            ),
        )

        # Per-iteration sentinel: a failing per-message store write
        # captures into this slot rather than propagating out through the
        # invoker's on_message wrapper (which swallows exceptions so a
        # faulty live renderer cannot break the agent run). The harness
        # re-raises after the invoker returns so the strict-audit policy
        # (FR-5) still routes failures through INTERNAL_ERROR.
        captured_iteration = iteration_number
        first_audit_error: _AuditWriteError | None = None

        def _on_message(msg: Message) -> None:
            nonlocal first_audit_error
            try:
                _persist_sdk_message(
                    store,
                    run_id=lifecycle.run_id,
                    attempt_number=attempt_number,
                    iteration_number=captured_iteration,
                    message=msg,
                    now=clock,
                )
            except _AuditWriteError as exc:
                if first_audit_error is None:
                    first_audit_error = exc

        request = InvocationRequest(
            prompt=prompt,
            transcript_graders=transcript_graders,
            attempt_number=attempt_number,
            iteration_number=iteration_number,
            on_message=_on_message,
        )
        # Hang watchdog gate (FR-3 / FR-5): when the threshold is unset or
        # non-positive the watchdog never starts and the call path is
        # exactly as today -- no asyncio.Task is created and no second
        # subscription is registered, so the disabled-default adds no
        # measurable overhead. When set, _invoke_with_watchdog wraps
        # _on_message so any SDK message resets the timer and a watchdog
        # cancel routes through _HangDetected to the FR-3 internal_error
        # path rather than _handle_interrupt (FR-4).
        hang_timeout = config.loop_guard.hang_timeout_seconds
        if hang_timeout is None or hang_timeout <= 0:
            iteration_result = await invoker(request)
        else:
            iteration_result = await _invoke_with_watchdog(
                invoker=invoker,
                request=request,
                hang_timeout=hang_timeout,
                mclock=mclock,
                store=store,
                run_id=lifecycle.run_id,
                attempt_number=attempt_number,
                iteration_number=iteration_number,
                clock=clock,
            )
        wall_seconds = mclock() - started_monotonic

        if first_audit_error is not None:
            raise first_audit_error

        # Context-pressure telemetry: token fields are per-iteration deltas
        # — consumers cumulate by summing the audit stream; the harness
        # holds no running counter. ``total_cost_usd`` and ``num_turns`` are
        # forwarded verbatim from the SDK's ResultMessage and are
        # session-cumulative as the SDK reports them; do NOT delta them.
        usage_breakdown = _build_usage_breakdown(iteration_result.messages)
        usage_payload: dict[str, Any] = dict(usage_breakdown)
        usage_payload["total_tokens"] = total_tokens_from_usage(usage_breakdown)
        _emit(
            store,
            run_id=lifecycle.run_id,
            kind="harness.iteration_completed",
            payload={
                "iteration": iteration_number,
                "envelope": _envelope_payload(iteration_result.envelope),
                "failure": (
                    {
                        "error_type": iteration_result.failure.error_type,
                        "message": iteration_result.failure.message,
                    }
                    if iteration_result.failure is not None
                    else None
                ),
                "stop_reason": iteration_result.signals.stop_reason,
                "rate_limited": len(
                    iteration_result.signals.rate_limit_events
                )
                > 0,
                "usage": usage_payload,
                "total_cost_usd": iteration_result.signals.total_cost_usd,
                "num_turns": iteration_result.signals.num_turns,
            },
            attempt_number=attempt_number,
            now=clock,
        )

        if iteration_result.failure is not None:
            break

        # Feed the iteration's tool tuples to the per-attempt LoopGuard
        # before deciding whether to continue. A STUCK / THRASH verdict
        # preempts the cap-reached and even the final-iteration outcomes
        # so the safety-net transition fires regardless of where the
        # repeating pattern lands. ``observe`` is pure and never silently
        # swallows errors -- if it raises we let it propagate to
        # ``_run_attempt``'s crash path.
        loop_guard_verdict = loop_guard.observe(
            iteration_result.signals.tool_interactions
        )
        if loop_guard_verdict is not None:
            break

        if (
            isinstance(iteration_result.envelope, ValidEnvelope)
            and iteration_result.envelope.intent == Intent.CONTINUE
        ):
            continue
        break

    return iteration_result, iteration_number, wall_seconds, loop_guard_verdict


async def _validate(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    config: HarnessConfig,
    attempt: Attempt,
    attempt_dir: Path | None,
    observation: TranscriptObservation,
    agent_output: str,
    clock: Callable[[], datetime],
) -> None:
    """Run command, transcript, then rubric graders.

    Transitions:
    - All pass -> ``DONE``.
    - Command/transcript fail -> ``FAILED_VALIDATION``.
    - Operator-signal-killed command grader -> ``INTERRUPTED``.
    - Rubric fail with ``retry_on_fail=True`` -> ``FAILED_VALIDATION``.
    - Rubric fail with ``retry_on_fail=False`` -> ``INTERRUPTED``.
    - Judge infra failure (``RubricJudgeError``) ->
      ``INTERNAL_ERROR`` outcome + ``INTERRUPTED`` lifecycle status
      via a ``harness.crash`` event whose ``classification`` is
      ``rubric_judge_error``.
    """
    command_results = run_command_graders(
        task,
        store,
        run_id=lifecycle.run_id,
        attempt_number=attempt.number,
        cwd=config.worktree,
        artifacts_dir=attempt_dir,
        now=clock,
    )
    command_passed = _all_passed(command_results)

    transcript_results = run_transcript_graders(
        task,
        observation,
        store,
        run_id=lifecycle.run_id,
        attempt_number=attempt.number,
        command_passed=command_passed,
        now=clock,
    )
    transcript_passed = _all_passed(transcript_results)

    # Rubric graders only run when command + transcript both passed
    # (cost-order short-circuit). The runner itself enforces this
    # invariant; the harness honors it so no rubric events emit on
    # earlier failures.
    rubric_results: list[GraderResultRecord] = []
    rubric_passed = True
    if command_passed and transcript_passed:
        try:
            rubric_results = await run_rubric_graders(
                task,
                store,
                run_id=lifecycle.run_id,
                attempt_number=attempt.number,
                transcript=agent_output,
                worktree=config.worktree,
                command_passed=command_passed,
                transcript_passed=transcript_passed,
                judge_invoke=config.rubric_judge_invoke,
                judge_model=config.rubric_judge_model,
                judge_max_turns=config.rubric_judge_max_turns,
                now=clock,
            )
        except RubricJudgeError as exc:
            error = (
                f"rubric judge failed: {exc.grader_name}: {exc.reason}"
            )
            _emit(
                store,
                run_id=lifecycle.run_id,
                kind="harness.crash",
                payload={
                    "classification": "rubric_judge_error",
                    "grader_name": exc.grader_name,
                    "reason": exc.reason,
                    "message": error,
                },
                attempt_number=attempt.number,
                now=clock,
            )
            _finalize_attempt(
                store=store,
                lifecycle=lifecycle,
                attempt=attempt,
                outcome=Outcome.INTERNAL_ERROR,
                error=error,
                agent_output=agent_output,
                clock=clock,
            )
            _transition(
                lifecycle,
                Status.INTERNAL_ERROR,
                store=store,
                error=error,
                now=clock,
            )
            return

        for record in rubric_results:
            _emit_rubric_events(
                store,
                run_id=lifecycle.run_id,
                attempt_number=attempt.number,
                record=record,
                clock=clock,
            )
        rubric_passed = all(r.passed for r in rubric_results)

    if command_passed and transcript_passed and rubric_passed:
        _finalize_attempt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.SUCCEEDED,
            error="",
            agent_output=agent_output,
            clock=clock,
        )
        _transition(lifecycle, Status.DONE, store=store, now=clock)
        return

    if not command_passed:
        signaled = _signal_killed_grader(command_results)
        if signaled is not None:
            grader_label = signaled.grader_name or signaled.grader_type
            signal_no = int(signaled.payload.get("signal", 0))
            error = (
                f"operator interrupted command grader {grader_label!r} "
                f"(signal {signal_no})"
            )
            _emit(
                store,
                run_id=lifecycle.run_id,
                kind="harness.crash",
                payload={
                    "classification": "grader_signaled",
                    "signal": signal_no,
                    "grader_name": signaled.grader_name,
                    "grader_ordinal": signaled.ordinal,
                    "message": error,
                },
                attempt_number=attempt.number,
                now=clock,
            )
            _finalize_attempt(
                store=store,
                lifecycle=lifecycle,
                attempt=attempt,
                outcome=Outcome.INTERNAL_ERROR,
                error=error,
                agent_output=agent_output,
                clock=clock,
            )
            _transition(
                lifecycle,
                Status.INTERRUPTED,
                store=store,
                now=clock,
            )
            return
        error = _grader_failure_error(command_results)
        _finalize_attempt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.VALIDATION_FAILED,
            error=error,
            agent_output=agent_output,
            clock=clock,
        )
        _transition(
            lifecycle,
            Status.FAILED_VALIDATION,
            store=store,
            error=error,
            now=clock,
        )
        return

    if not transcript_passed:
        error = _grader_failure_error(transcript_results)
        _finalize_attempt(
            store=store,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.VALIDATION_FAILED,
            error=error,
            agent_output=agent_output,
            clock=clock,
        )
        _transition(
            lifecycle,
            Status.FAILED_VALIDATION,
            store=store,
            error=error,
            now=clock,
        )
        return

    # Rubric failure: the first failing rubric record's grader controls
    # the retry-on-fail policy.
    failed_record = next(r for r in rubric_results if not r.passed)
    grader = task.graders[failed_record.ordinal]
    assert isinstance(grader, RubricGrader), (
        "rubric record ordinal must point to a RubricGrader"
    )
    grader_label = failed_record.grader_name or "rubric"
    error = f"rubric grader {grader_label!r} failed"
    _finalize_attempt(
        store=store,
        lifecycle=lifecycle,
        attempt=attempt,
        outcome=Outcome.VALIDATION_FAILED,
        error=error,
        agent_output=agent_output,
        clock=clock,
    )
    if grader.retry_on_fail:
        _transition(
            lifecycle,
            Status.FAILED_VALIDATION,
            store=store,
            error=error,
            now=clock,
        )
    else:
        _transition(
            lifecycle,
            Status.INTERRUPTED,
            store=store,
            error=error,
            now=clock,
        )


def _emit_rubric_events(
    store: HarnessStore,
    *,
    run_id: str,
    attempt_number: int,
    record: GraderResultRecord,
    clock: Callable[[], datetime],
) -> None:
    """Emit harness.rubric_invoked / harness.rubric_verdict (and, when
    applicable, harness.rubric_unknown) for one persisted rubric record.

    Derived from the record so the events and the durable receipt agree
    by construction. ``rubric_invoked`` precedes ``rubric_verdict`` to
    preserve start-then-end ordering in the audit stream.
    """
    payload = record.payload
    summary = payload.get("summary")
    unknown = bool(payload.get("unknown", False))
    judge_model = payload.get("judge_model")
    _emit(
        store,
        run_id=run_id,
        kind="harness.rubric_invoked",
        payload={
            "grader_name": record.grader_name,
            "judge_model": judge_model,
            "attempt_number": record.attempt_number,
        },
        attempt_number=attempt_number,
        now=clock,
    )
    _emit(
        store,
        run_id=run_id,
        kind="harness.rubric_verdict",
        payload={
            "grader_name": record.grader_name,
            "passed": record.passed,
            "summary": summary,
            "unknown": unknown,
        },
        attempt_number=attempt_number,
        now=clock,
    )
    if unknown:
        _emit(
            store,
            run_id=run_id,
            kind="harness.rubric_unknown",
            payload={
                "grader_name": record.grader_name,
                "summary": summary,
            },
            attempt_number=attempt_number,
            now=clock,
        )


def _finalize_attempt(
    *,
    store: HarnessStore,
    lifecycle: Lifecycle,
    attempt: Attempt,
    outcome: Outcome,
    error: str,
    clock: Callable[[], datetime],
    agent_output: str = "",
) -> None:
    """Persist the Attempt's terminal fields and emit the finalization event.

    The terminal write is an ``AttemptFinalized`` domain event: it closes
    the attempt projection and folds the lifecycle's ``agent_output`` in
    one atomic append. The in-memory ``attempt`` object is kept in sync for
    any local reads, but the event is the source of truth.
    """
    ended_at = clock()
    attempt.ended_at = ended_at
    attempt.outcome = outcome
    attempt.error = error
    if agent_output:
        attempt.agent_output = agent_output
    _append(
        lifecycle,
        AttemptFinalized(
            run_id=lifecycle.run_id,
            ts=ended_at,
            attempt_number=attempt.number,
            number=attempt.number,
            outcome=outcome,
            ended_at=ended_at,
            agent_output=agent_output,
            error=error,
        ),
        store=store,
    )
    _emit(
        store,
        run_id=lifecycle.run_id,
        kind="harness.attempt_finalized",
        payload={
            "number": attempt.number,
            "outcome": outcome.value,
            "error": error,
        },
        attempt_number=attempt.number,
        now=clock,
    )


def finalize_stranded_lifecycle(
    store: HarnessStore,
    run_id: str,
    *,
    reason: str = "worker interrupted before finalization",
    classification: str = "worker_interrupted",
    now: Callable[[], datetime] | None = None,
) -> bool:
    """Drain a lifecycle stranded in ``RUNNING`` or ``VALIDATING`` to
    ``INTERRUPTED``.

    Used when the worker process is killed mid-attempt (SIGINT, SIGTERM,
    machine reboot) and the in-flight attempt never reached
    :func:`_finalize_attempt`. Closes any open attempt as
    :attr:`Outcome.INTERNAL_ERROR`, emits a single ``harness.crash``
    event, and transitions the lifecycle to :attr:`Status.INTERRUPTED`
    so the retry budget is preserved and the next worker start can pick
    up a fresh lifecycle.

    No-op (returns ``False``) when the lifecycle is missing or already
    in a status the harness considers final or quiescent.
    """
    clock = now or _utcnow
    lifecycle = store.load_lifecycle(run_id)
    if lifecycle is None:
        return False
    if lifecycle.status not in (Status.RUNNING, Status.VALIDATING):
        return False

    attempts = store.list_attempts(run_id)
    open_attempts = [a for a in attempts if a.ended_at is None]
    last_attempt_number = (
        open_attempts[-1].number
        if open_attempts
        else (attempts[-1].number if attempts else None)
    )
    for attempt in open_attempts:
        ended_at = clock()
        attempt.ended_at = ended_at
        attempt.outcome = Outcome.INTERNAL_ERROR
        attempt.error = reason
        _append(
            lifecycle,
            AttemptFinalized(
                run_id=run_id,
                ts=ended_at,
                attempt_number=attempt.number,
                number=attempt.number,
                outcome=Outcome.INTERNAL_ERROR,
                ended_at=ended_at,
                agent_output=attempt.agent_output,
                error=reason,
            ),
            store=store,
        )
        _emit(
            store,
            run_id=run_id,
            kind="harness.attempt_finalized",
            payload={
                "number": attempt.number,
                "outcome": Outcome.INTERNAL_ERROR.value,
                "error": reason,
            },
            attempt_number=attempt.number,
            now=clock,
        )

    _emit(
        store,
        run_id=run_id,
        kind="harness.crash",
        payload={
            "classification": classification,
            "message": reason,
            "from_status": lifecycle.status.value,
        },
        attempt_number=last_attempt_number,
        now=clock,
    )
    _transition(lifecycle, Status.INTERRUPTED, store=store, now=clock)
    return True


@dataclass(frozen=True, kw_only=True)
class RecheckOutcome:
    """Return value of :func:`recheck_blocked_lifecycle`.

    ``applied`` is ``True`` only when a transition ``INTERRUPTED -> READY``
    actually landed on this call. ``reason`` is a short stable token
    (``"not_blocked"``, ``"dry_run"``, ``"unsatisfied"``, ``"unblocked"``,
    or ``"parse_error: ..."``) for programmatic consumers. ``per_predicate``
    mirrors the ``harness.recheck_attempted`` payload entries — one dict
    per persisted predicate with ``type``, ``identifier``, ``satisfied``,
    and ``detail``.
    """

    applied: bool
    reason: str
    per_predicate: tuple[dict[str, object], ...]


def _evaluate_blocked_predicate(
    req: BlockedRequirement,
    grader_by_name: Mapping[str, Any],
    *,
    cwd: str | os.PathLike[str] | None = None,
) -> dict[str, object]:
    """Evaluate one persisted predicate against ``cwd``/env and the
    supplied grader map. Never raises — OS errors are surfaced via the
    ``detail`` string with ``satisfied=False``.

    ``command_grader`` predicates resolve the named grader from
    ``task.graders`` and invoke its ``run`` string with ``subprocess.run``
    (``shell=True``), inheriting ``os.environ``. The working directory is
    ``cwd`` when supplied (the task's sandbox; the caller owns it) and
    ``os.getcwd()`` otherwise. ``file_exists`` paths are resolved relative
    to the same ``cwd`` when relative. Exit code ``0`` means satisfied. We
    deliberately do **not** call
    :func:`flywheel.grader_command.run_command_graders` because that path
    persists a ``grader_results`` row, and per spec FR-5 recheck is a
    control-plane operation whose audit surface is the event payload.
    """
    if isinstance(req, CommandGraderRequirement):
        grader = grader_by_name.get(req.name)
        if grader is None or not isinstance(grader, CommandGrader):
            return {
                "type": "command_grader",
                "identifier": req.name,
                "satisfied": False,
                "detail": "grader not found",
            }
        try:
            proc = subprocess.run(
                grader.run,
                shell=True,
                cwd=os.fspath(cwd) if cwd is not None else os.getcwd(),
                env=os.environ.copy(),
                capture_output=True,
                check=False,
            )
        except OSError as exc:
            return {
                "type": "command_grader",
                "identifier": req.name,
                "satisfied": False,
                "detail": f"subprocess failed to start: {exc}",
            }
        satisfied = proc.returncode == 0
        return {
            "type": "command_grader",
            "identifier": req.name,
            "satisfied": satisfied,
            "detail": f"exit_code={proc.returncode}",
        }
    if isinstance(req, FileExistsRequirement):
        probe_path = req.path
        if cwd is not None and not os.path.isabs(probe_path):
            probe_path = os.path.join(os.fspath(cwd), probe_path)
        exists = os.path.exists(probe_path)
        satisfied = exists if req.present else not exists
        observed = "present" if exists else "absent"
        expected = "present" if req.present else "absent"
        detail = (
            observed if satisfied else f"observed={observed}, expected={expected}"
        )
        return {
            "type": "file_exists",
            "identifier": req.path,
            "satisfied": satisfied,
            "detail": detail,
        }
    # env_var_set: non-empty value required.
    value = os.environ.get(req.name)
    satisfied = bool(value)
    detail = "set" if satisfied else "unset_or_empty"
    return {
        "type": "env_var_set",
        "identifier": req.name,
        "satisfied": satisfied,
        "detail": detail,
    }


def recheck_blocked_lifecycle(
    store: HarnessStore,
    run_id: str,
    task: Task,
    *,
    dry_run: bool = False,
    cwd: str | os.PathLike[str] | None = None,
    now: Callable[[], datetime] | None = None,
) -> RecheckOutcome:
    """Re-evaluate a blocked lifecycle's persisted ``requires`` predicates
    and, when all are satisfied and ``dry_run`` is ``False``, transition
    ``INTERRUPTED -> READY``.

    Per spec FR-5:

    * loads the lifecycle. If status is not :attr:`Status.INTERRUPTED` or
      ``blocked_requires_json`` is ``None``, returns a no-event no-op
      ``RecheckOutcome(applied=False, reason="not_blocked")``;
    * parses the persisted ``requires`` snapshot, evaluating each
      predicate against ``cwd`` (the task's sandbox; the caller owns it)
      and ``os.environ``, falling back to ``os.getcwd()`` when ``cwd`` is
      ``None``;
    * emits ``harness.recheck_attempted`` recording per-predicate detail,
      the aggregate ``all_satisfied`` bit, and the ``dry_run`` flag;
    * if ``dry_run`` is ``True``, never transitions and never emits
      ``harness.unblocked``;
    * if all predicates satisfied, transitions ``INTERRUPTED -> READY``
      (which clears ``blocked_requires_json`` via the centralized clearer
      in :meth:`Lifecycle.transition_to`) and emits ``harness.unblocked``.

    ``command_grader`` predicates run their ``run`` string via
    :func:`subprocess.run` rather than reusing
    :func:`flywheel.grader_command.run_command_graders`: the latter
    persists a ``grader_results`` row, but recheck's audit surface is the
    event payload only (FR-5 Out of Scope).

    Concurrent callers race on ``lifecycle.version`` exactly as the rest
    of the harness does; the loser surfaces
    :class:`flywheel.store_protocols.OptimisticConcurrencyError` to its
    caller — this function does not swallow it.
    """
    clock = now or _utcnow
    lifecycle = store.load_lifecycle(run_id)
    if lifecycle is None:
        return RecheckOutcome(
            applied=False, reason="not_blocked", per_predicate=()
        )
    if lifecycle.status != Status.INTERRUPTED:
        return RecheckOutcome(
            applied=False, reason="not_blocked", per_predicate=()
        )
    if lifecycle.blocked_requires_json is None:
        return RecheckOutcome(
            applied=False, reason="not_blocked", per_predicate=()
        )

    parsed = _parse_blocked_requires_json(lifecycle.blocked_requires_json)
    if isinstance(parsed, str):
        per_predicate: tuple[dict[str, object], ...] = (
            {
                "type": "parse_error",
                "identifier": None,
                "satisfied": False,
                "detail": parsed,
            },
        )
        _emit(
            store,
            run_id=run_id,
            kind="harness.recheck_attempted",
            payload={
                "per_predicate": [dict(p) for p in per_predicate],
                "all_satisfied": False,
                "dry_run": dry_run,
            },
            now=clock,
        )
        return RecheckOutcome(
            applied=False,
            reason=f"parse_error: {parsed}",
            per_predicate=per_predicate,
        )

    grader_by_name: dict[str, Any] = {}
    for grader in task.graders:
        gname = getattr(grader, "name", None)
        if isinstance(gname, str) and gname:
            grader_by_name[gname] = grader

    evaluated: list[dict[str, object]] = []
    for req in parsed:
        evaluated.append(
            _evaluate_blocked_predicate(req, grader_by_name, cwd=cwd)
        )
    all_satisfied = all(bool(p["satisfied"]) for p in evaluated)

    _emit(
        store,
        run_id=run_id,
        kind="harness.recheck_attempted",
        payload={
            "per_predicate": [dict(p) for p in evaluated],
            "all_satisfied": all_satisfied,
            "dry_run": dry_run,
        },
        now=clock,
    )

    per_predicate_out: tuple[dict[str, object], ...] = tuple(evaluated)

    if dry_run:
        return RecheckOutcome(
            applied=False,
            reason="dry_run",
            per_predicate=per_predicate_out,
        )

    if not all_satisfied:
        return RecheckOutcome(
            applied=False,
            reason="unsatisfied",
            per_predicate=per_predicate_out,
        )

    _transition(lifecycle, Status.READY, store=store, now=clock)
    _emit(
        store,
        run_id=run_id,
        kind="harness.unblocked",
        payload={
            "from_status": Status.INTERRUPTED.value,
            "to_status": Status.READY.value,
        },
        now=clock,
    )
    return RecheckOutcome(
        applied=True,
        reason="unblocked",
        per_predicate=per_predicate_out,
    )


__all__ = [
    "HarnessConfig",
    "HarnessOutcome",
    "HarnessStore",
    "InvocationRequest",
    "InvokeFunc",
    "RecheckOutcome",
    "finalize_stranded_lifecycle",
    "recheck_blocked_lifecycle",
    "run_task",
]
