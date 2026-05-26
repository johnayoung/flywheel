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

The TODO subsystems from ``docs/loop.md`` — thrash detection, hang
threshold defaults, context-recovery policy, fine-grained crash
classification, and ``blocked_implicit`` semantic similarity — are
**explicitly deferred** here. See :data:`_DEFERRED_LOOP_SUBSYSTEMS` for
the canonical list. The harness does not paper over them with stub
heuristics.
"""

from __future__ import annotations

import os
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Protocol, runtime_checkable

from claude_agent_sdk import AssistantMessage, Message, ResultMessage

from flywheel.envelope import (
    DuplicateEnvelope,
    EnvelopeResult,
    Intent,
    MalformedEnvelope,
    MissingEnvelope,
    TruncatedEnvelope,
    ValidEnvelope,
)
from flywheel.grader_command import run_command_graders
from flywheel.grader_transcript import (
    TranscriptObservation,
    first_breach,
    run_transcript_graders,
    total_tokens_from_usage,
)
from flywheel.invoker import IterationResult, invoke_iteration
from flywheel.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel.prompt import IterationInputs, build_iteration_prompt
from flywheel.store_protocols import (
    EventRecord,
    GraderResultRecord,
)
from flywheel.task import Task, TranscriptGrader


# Loop.md flags these subsystems as TODO. They are intentionally not
# implemented here so the rubric's "not silently faked" assertion holds.
_DEFERRED_LOOP_SUBSYSTEMS: tuple[str, ...] = (
    "thrash detection",
    "hang threshold defaults",
    "context-recovery policy",
    "fine-grained crash classification",
    "blocked_implicit semantic similarity",
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

    def save_attempt(self, run_id: str, attempt: Attempt) -> None: ...

    def list_attempts(self, run_id: str) -> list[Attempt]: ...

    def append_event(self, event: EventRecord) -> EventRecord: ...

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
    """

    prompt: str
    transcript_graders: tuple[TranscriptGrader, ...]
    attempt_number: int
    iteration_number: int


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
    """

    max_retries: int = 0
    max_iterations_per_attempt: int = 1
    artifacts_root: str | os.PathLike[str] | None = None
    agent_context: Mapping[str, str] = field(default_factory=dict)


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
    the SDK-backed default.
    """
    return await invoke_iteration(prompt=request.prompt)


def _transition(
    lifecycle: Lifecycle,
    target: Status,
    *,
    store: HarnessStore,
    error: str = "",
    now: Callable[[], datetime] | None = None,
) -> None:
    """Apply one lifecycle transition and persist it under optimistic
    concurrency.

    Centralized so every state change in the harness goes through the
    same code path. :meth:`Lifecycle.transition_to` increments
    ``lifecycle.version``; ``expected_version`` is the *prior* version,
    matching the store's contract.
    """
    expected_version = lifecycle.version
    clock = now or _utcnow
    lifecycle.transition_to(target, error=error, now=clock())
    store.update_lifecycle(lifecycle, expected_version=expected_version)


def _emit(
    store: HarnessStore,
    *,
    run_id: str,
    kind: str,
    payload: Mapping[str, Any],
    attempt_number: int | None = None,
    now: Callable[[], datetime] | None = None,
) -> None:
    clock = now or _utcnow
    store.append_event(
        EventRecord(
            run_id=run_id,
            ts=clock(),
            kind=kind,
            payload=dict(payload),
            attempt_number=attempt_number,
        )
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
    """

    config = config or HarnessConfig()
    invoker = invoke or _default_invoke
    clock = now or _utcnow
    mclock = monotonic or _default_monotonic

    # Ensure the lifecycle row exists. If the caller has previously
    # persisted it (e.g. resuming an interrupted run), trust the
    # in-memory copy they passed — the store's optimistic concurrency
    # check will catch any divergence on the first update.
    stored = store.load_lifecycle(lifecycle.run_id)
    if stored is None:
        store.create_lifecycle(lifecycle)

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
                _transition(lifecycle, Status.READY, store=store, now=clock)
                continue
            terminal_error = (
                lifecycle.error
                or f"retries exhausted ({lifecycle.retries}/{config.max_retries})"
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
    store.save_attempt(lifecycle.run_id, attempt)
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

    iteration_result, _iterations_run, wall_seconds = await _drive_iterations(
        task=task,
        lifecycle=lifecycle,
        store=store,
        config=config,
        invoker=invoker,
        clock=clock,
        mclock=mclock,
        attempt_number=attempt_number,
        transcript_graders=transcript_graders,
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
            reason = envelope.reason or "agent reported blocked intent"
            _finalize_attempt(
                store=store,
                lifecycle=lifecycle,
                attempt=attempt,
                outcome=Outcome.CANCELLED,
                error=reason,
                agent_output=iteration_result.transcript,
                clock=clock,
            )
            _emit(
                store,
                run_id=lifecycle.run_id,
                kind="harness.blocked",
                payload={"reason": reason},
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
) -> tuple[IterationResult | None, int, float]:
    """Run iterations until a non-``continue`` envelope, crash, or cap.

    Returns the last :class:`IterationResult` produced, the number of
    iterations that ran, and the wall-seconds elapsed across them. The
    wall-seconds value is what feeds the
    :class:`TranscriptObservation` so transcript graders see the same
    elapsed time regardless of which iteration sets the envelope.
    """
    iteration_result: IterationResult | None = None
    iteration_number = 0
    started_monotonic = mclock()
    wall_seconds = 0.0

    while iteration_number < config.max_iterations_per_attempt:
        iteration_number += 1

        prompt = build_iteration_prompt(
            task,
            lifecycle,
            IterationInputs(max_retries=config.max_retries),
        )
        request = InvocationRequest(
            prompt=prompt,
            transcript_graders=transcript_graders,
            attempt_number=attempt_number,
            iteration_number=iteration_number,
        )
        iteration_result = await invoker(request)
        wall_seconds = mclock() - started_monotonic

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
            },
            attempt_number=attempt_number,
            now=clock,
        )

        if iteration_result.failure is not None:
            break
        if (
            isinstance(iteration_result.envelope, ValidEnvelope)
            and iteration_result.envelope.intent == Intent.CONTINUE
        ):
            continue
        break

    return iteration_result, iteration_number, wall_seconds


async def _validate(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    attempt: Attempt,
    attempt_dir: Path | None,
    observation: TranscriptObservation,
    agent_output: str,
    clock: Callable[[], datetime],
) -> None:
    """Run command then transcript graders; transition validating -> done
    or validating -> failed_validation."""
    command_results = run_command_graders(
        task,
        store,
        run_id=lifecycle.run_id,
        attempt_number=attempt.number,
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

    if command_passed and transcript_passed:
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
    else:
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
    """Persist the Attempt's terminal fields and emit the finalization event."""
    attempt.ended_at = clock()
    attempt.outcome = outcome
    attempt.error = error
    if agent_output:
        attempt.agent_output = agent_output
    store.save_attempt(lifecycle.run_id, attempt)
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
        attempt.ended_at = clock()
        attempt.outcome = Outcome.INTERNAL_ERROR
        attempt.error = reason
        store.save_attempt(run_id, attempt)
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


__all__ = [
    "HarnessConfig",
    "HarnessOutcome",
    "HarnessStore",
    "InvocationRequest",
    "InvokeFunc",
    "finalize_stranded_lifecycle",
    "run_task",
]
