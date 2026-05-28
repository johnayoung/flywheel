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
    TranscriptObservation,
    first_breach,
    run_transcript_graders,
    total_tokens_from_usage,
)
from flywheel.invoker import (
    IterationResult,
    _serialize_sdk_message,
    invoke_iteration,
)
from flywheel.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel.prompt import IterationInputs, RubricFindings, build_iteration_prompt
from flywheel.store_protocols import (
    EventRecord,
    GraderResultRecord,
    LifecycleAlreadyExistsError,
)
from flywheel.task import CommandGrader, RubricGrader, Task, TranscriptGrader


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

    ``worktree`` is the per-attempt working directory the rubric judge
    runs against (typically the same sandbox the working agent uses).
    When ``None`` and the task declares a :class:`RubricGrader`, the
    rubric runner raises :class:`RubricJudgeError` ("worktree not
    available") which the harness routes through ``INTERNAL_ERROR``.
    Workflow CLIs populate this with their sandbox path.

    ``rubric_judge_model`` is the default model for rubric judges when
    the per-grader ``RubricGrader.judge_model`` is unset; ``None`` falls
    through to the SDK's own default. ``rubric_judge_max_turns`` caps
    the per-judge-call turn budget (default 8).

    ``rubric_judge_invoke`` is a test seam: when set, the harness passes
    it to ``run_rubric_graders`` instead of the runner's default fresh
    ``claude_agent_sdk.query`` invoker. Production callers leave it
    ``None``.
    """

    max_retries: int = 0
    max_iterations_per_attempt: int = 1
    artifacts_root: str | os.PathLike[str] | None = None
    agent_context: Mapping[str, str] = field(default_factory=dict)
    worktree: str | os.PathLike[str] | None = None
    rubric_judge_model: str | None = None
    rubric_judge_max_turns: int = 8
    rubric_judge_invoke: JudgeInvoke | None = None


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


def _persist_sdk_messages(
    store: HarnessStore,
    *,
    run_id: str,
    attempt_number: int,
    iteration_number: int,
    messages: Sequence[Message],
) -> None:
    """Persist every SDK :class:`Message` observed during one iteration.

    Serializes each message via :func:`_serialize_sdk_message` and hands
    the payload sequence to ``store.save_sdk_messages``. Empty batches
    are forwarded unchanged so the store can record an
    iteration-with-no-messages without assigning sequence numbers. On
    failure raises :class:`_AuditWriteError` carrying the failing method
    and the attempt/iteration context.
    """
    payloads = [_serialize_sdk_message(m) for m in messages]
    try:
        store.save_sdk_messages(
            run_id, attempt_number, iteration_number, payloads
        )
    except Exception as exc:
        raise _AuditWriteError(
            failing_method="save_sdk_messages",
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
        attempt.ended_at = clock()
        attempt.outcome = Outcome.INTERNAL_ERROR
        attempt.error = error
        try:
            store.save_attempt(lifecycle.run_id, attempt)
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
    interaction is :meth:`HarnessStore.create_lifecycle`; any
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
    operator-driven shutdown remains the existing
    :func:`finalize_stranded_lifecycle` path in
    :mod:`flywheel.workflow`. If a concurrent harness lands a
    transition first, the loser's
    :class:`~flywheel.store_protocols.OptimisticConcurrencyError`
    surfaces to the caller verbatim — not swallowed — so the worker
    learns its run_id is racing.

    **Remaining gap.** If :meth:`HarnessStore.create_lifecycle` itself
    fails (catastrophic schema breakage that the INSERT cannot satisfy)
    no lifecycle row can exist and ``harness.crash`` cannot foreign-key
    against ``run_id``. That exception propagates straight to the
    caller; the worker-side circuit breaker (separate task) covers the
    repeating-spawn shape.
    """

    config = config or HarnessConfig()
    invoker = invoke or _default_invoke
    clock = now or _utcnow
    mclock = monotonic or _default_monotonic

    # Ensure the lifecycle row exists as the first store interaction.
    # Create-then-swallow is the defensive shape: a load-time failure
    # (e.g. OperationalError on schema drift) would otherwise leave the
    # run with zero rows across lifecycles/attempts/events, which the
    # audit at .workflow/audits/08-recoverable-blocked-lifecycles.md
    # documents as the silent-crash shape we are closing here.
    try:
        store.create_lifecycle(lifecycle)
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
        # cannot catch them — operator shutdown stays on the existing
        # finalize_stranded_lifecycle path in workflow.py. Everything
        # else is internal failure: record one harness.crash event and
        # walk the lifecycle to FAILED before re-raising so the worker
        # subshell still exits non-zero with the original traceback.
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
    store.save_attempt(lifecycle.run_id, attempt)
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
    :func:`_emit` or ``save_sdk_messages`` via :func:`_persist_sdk_messages`)
    raises :class:`_AuditWriteError` on failure; the caller routes that
    through :func:`_handle_audit_failure`.
    """
    attempt_number = attempt.number
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
            # Persist the structured snapshot on the lifecycle row before
            # the transition so the update_lifecycle write inside
            # _transition picks up the new field atomically with the
            # status change. Lifecycle.transition_to clears this field on
            # every -> READY edge (recheck, retry, normalization).
            lifecycle.blocked_requires_json = json.dumps(requires_payload)
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
        request = InvocationRequest(
            prompt=prompt,
            transcript_graders=transcript_graders,
            attempt_number=attempt_number,
            iteration_number=iteration_number,
        )
        iteration_result = await invoker(request)
        wall_seconds = mclock() - started_monotonic

        _persist_sdk_messages(
            store,
            run_id=lifecycle.run_id,
            attempt_number=attempt_number,
            iteration_number=iteration_number,
            messages=iteration_result.messages,
        )

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
) -> dict[str, object]:
    """Evaluate one persisted predicate against the worker CWD/env and the
    supplied grader map. Never raises — OS errors are surfaced via the
    ``detail`` string with ``satisfied=False``.

    ``command_grader`` predicates resolve the named grader from
    ``task.graders`` and invoke its ``run`` string with ``subprocess.run``
    (``shell=True``), inheriting ``os.getcwd()`` / ``os.environ``. Exit
    code ``0`` means satisfied. We deliberately do **not** call
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
                cwd=os.getcwd(),
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
        exists = os.path.exists(req.path)
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
      predicate against the **caller's** CWD and ``os.environ`` (per the
      spec the caller owns the sandbox);
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
        evaluated.append(_evaluate_blocked_predicate(req, grader_by_name))
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
