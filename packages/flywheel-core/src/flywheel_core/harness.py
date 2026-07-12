"""Harness — single-task orchestration loop.

Wires :mod:`flywheel_core.invoker`, :mod:`flywheel_core.envelope`,
:mod:`flywheel_core.prompt`, :mod:`flywheel_core.grader_command`, and
:mod:`flywheel_core.grader_transcript` into one cohesive driver. Given a
:class:`~flywheel_core.task.Task` and a :class:`~flywheel_core.lifecycle.Lifecycle`,
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
``STUCK`` and tuple-repetition ``THRASH`` via :mod:`flywheel_core.loop_guard`,
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
import sys
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol, runtime_checkable

if TYPE_CHECKING:
    from claude_agent_sdk import (
        ContextUsageResponse,
        Message,
    )

from flywheel_core.deadline import DeadlineExceeded, run_with_deadline
from flywheel_core.deadline_config import DeadlineClass, DeadlineConfig
from flywheel_core.envelope import (
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
from flywheel_core.grader_command import run_command_graders
from flywheel_core.invoker_client import (
    CONTROL_COMMAND_APPROVE,
    CONTROL_COMMAND_REJECT,
    HarnessRecoveryRequested,
)
from flywheel_core.grader_rubric import (
    JudgeInvoke,
    RubricJudgeError,
    run_rubric_graders,
)
from flywheel_core.recovery_summarizer import (
    RecoverySummarizerError,
    SummarizerInvoke,
    run_recovery_summarizer,
)
from flywheel_core.grader_transcript import (
    _USAGE_TOKEN_KEYS,
    TranscriptObservation,
    first_breach,
    run_transcript_graders,
    total_tokens_from_usage,
)
from flywheel_core.event_serde import event_kind, event_payload
from flywheel_core.faults import (
    BackoffPolicy,
    FaultClass,
    classify_fault,
    derive_session_limit_reset,
)
from flywheel_core.events import (
    AttemptFinalized,
    AttemptStarted,
    AwaitingApproval,
    Blocked,
    CommandApplied,
    DomainEvent,
    LifecycleInitialized,
    TransitionedTo,
)
from flywheel_core.invoker import (
    InvocationSignals,
    IterationResult,
    _serialize_sdk_message,
    invoke_iteration,
)
from flywheel_core.lifecycle import Attempt, Lifecycle, Outcome, Status
from flywheel_core.loop_guard import (
    LoopGuard,
    LoopGuardConfig,
    LoopGuardVerdict,
    LoopGuardVerdictKind,
)
from flywheel_core.prompt import (
    IterationInputs,
    ManualFinding,
    RecoveryHandoff,
    RubricFindings,
    build_iteration_prompt,
)
from flywheel_core.store_protocols import (
    ControlCommandRecord,
    GraderResultRecord,
    LifecycleAlreadyExistsError,
    TelemetryRecord,
    TelemetrySink,
)
from flywheel_core.grader_manual import (
    ManualGate,
    build_manual_result,
    next_pending_manual_gate,
)
from flywheel_core.task import (
    CommandGrader,
    ManualGrader,
    RubricGrader,
    Task,
    TranscriptGrader,
)


# Loop.md flags these subsystems as still-TODO after the safety-net work
# landed. They are intentionally not implemented here so the rubric's
# "not silently faked" assertion holds. The mechanical detectors that
# did ship (repeated-failure STUCK, tuple-repetition THRASH, the hang
# watchdog mechanism) live in flywheel_core.loop_guard and _drive_iterations
# and are deliberately NOT in this list.
_DEFERRED_LOOP_SUBSYSTEMS: tuple[str, ...] = (
    "thrash net-diff detection (sub-problem b)",
    "thrash input-novelty score (sub-problem c)",
    "hang threshold default value (mechanism shipped, value ungrounded)",
    "fine-grained crash classification",
    "blocked_implicit same-question-re-asked detection",
)


@runtime_checkable
class HarnessStore(Protocol):
    """Composite store contract the harness requires.

    Satisfied by :class:`flywheel_core.store_memory.InMemoryStore` and
    :class:`flywheel_core.store_sqlite.SqliteStore`. The harness operates only
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

    def save_attempt(
        self,
        run_id: str,
        attempt: Attempt,
        *,
        expected_version: int | None = None,
    ) -> None: ...

    def list_attempts(self, run_id: str) -> list[Attempt]: ...

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord: ...

    def list_grader_results(
        self,
        run_id: str,
        attempt_number: int,
    ) -> list[GraderResultRecord]: ...

    def claim_commands(
        self,
        run_id: str,
        *,
        now: datetime,
    ) -> list[ControlCommandRecord]: ...

    def delete_command(self, command_id: int) -> None: ...


@dataclass(frozen=True, kw_only=True)
class InvocationRequest:
    """Arguments handed to the harness's invoke callable.

    Decoupled from :func:`flywheel_core.invoker.invoke_iteration` so the
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

    ``context_observer`` is the harness's mid-turn occupancy seam
    (spec 00019). When supplied, an invoker backed by a live
    :class:`claude_agent_sdk.ClaudeSDKClient` reads
    :meth:`ClaudeSDKClient.get_context_usage` once per watcher poll and
    hands the resulting :class:`ContextUsageResponse` to the observer so
    the harness can drive its 50 / 75 / 90 percent threshold checks off
    the exact SDK reading. Invokers that have no live client (the plain
    ``query`` path, scripted test invokers) leave it un-called; the
    harness's accumulated :attr:`AssistantMessage.usage` estimate is the
    always-available fallback. The observer is expected to swallow its
    own exceptions, mirroring the ``on_message`` contract.

    ``recovery_interrupt_event`` is the harness's mid-turn act seam
    (spec 00019 FR-4). When supplied, the harness sets the event from
    its threshold-checking closure the instant input-side occupancy
    crosses ``context_recovery_trigger_ratio`` with recovery budget
    remaining; an invoker backed by a live
    :class:`claude_agent_sdk.ClaudeSDKClient` polls the event in its
    watcher loop, dispatches :meth:`ClaudeSDKClient.interrupt`, cancels
    the iteration, and translates the resulting cancel into
    :class:`flywheel_core.invoker_client.HarnessRecoveryRequested` so the
    harness can route the attempt into the summarize-restart action.
    Invokers without a live client (the plain ``query`` path, scripted
    test invokers) leave the event un-polled; mid-turn act on those
    paths degrades to observe-only (FR-4 plain-path degradation), and
    the spec 00018 boundary recovery still covers the iteration's tail.

    ``on_command_applied`` is the harness's steering-ledger seam (spec
    00025 FR-10). When supplied, an invoker that applies an operator
    control command against the live session calls it once per
    successfully dispatched command, after the dispatch returns. The
    harness's callback appends the :class:`CommandApplied` domain event
    to the ledger and deletes the applied queue row once the event
    commits; it swallows its own exceptions (an append failure retains
    the row and surfaces on stderr), so invokers do not need to wrap it.
    Invokers without a control plane leave it un-called.

    ``checkpoint_nudge_seconds`` / ``agent_iteration_ceiling_seconds`` /
    ``checkpoint_progress_probe`` are the checkpoint-nudge seam. The
    harness threads its ``HarnessConfig.checkpoint_nudge_seconds`` knob,
    the resolved ``DeadlineClass.AGENT_ITERATION`` ceiling, and its
    optional git-free ``HarnessConfig.checkpoint_progress_probe`` closure
    to an invoker backed by a live
    :class:`claude_agent_sdk.ClaudeSDKClient`. That invoker fires a single
    checkpoint-commit instruction on the live session -- the same
    ``client.query`` surface an operator ``say`` uses -- when the
    remaining wall time to the ceiling drops to the threshold AND the
    probe reports no new progress. A ``None`` probe (the default) leaves
    the nudge dormant; an unbounded ceiling (opted-out AGENT_ITERATION)
    or a non-positive threshold also disables it. Invokers without a live
    client (the plain ``query`` path, scripted test invokers) ignore
    these fields.
    """

    prompt: str
    transcript_graders: tuple[TranscriptGrader, ...]
    attempt_number: int
    iteration_number: int
    on_message: Callable[[Message], None] | None = None
    context_observer: Callable[[ContextUsageResponse], None] | None = None
    recovery_interrupt_event: asyncio.Event | None = None
    on_command_applied: Callable[[ControlCommandRecord], None] | None = None
    checkpoint_nudge_seconds: float | None = None
    agent_iteration_ceiling_seconds: float | None = None
    checkpoint_progress_probe: Callable[[], object] | None = None


InvokeFunc = Callable[[InvocationRequest], Awaitable[IterationResult]]


@dataclass(frozen=True, kw_only=True)
class HarnessConfig:
    """Per-run harness knobs.

    ``max_retries`` is the retry budget consumed by
    :meth:`Lifecycle.is_retry_eligible` — the harness delegates the rule,
    it does not re-derive it.

    ``max_cost_usd`` is the PER-RUN cumulative cost ceiling in USD (spec
    00039). The harness sums ``total_cost_usd`` across *all* attempts of
    the run and, after the per-iteration rollup but before grading, ends
    the run ``Status.FAILED`` (terminal, non-retryable — mirroring the
    ABORT path) once the run total reaches the ceiling, emitting a
    ``harness.budget_ceiling_breached`` event so audit tells a budget kill
    from an agent error. The default ``0.0`` disables the ceiling
    (byte-identical to today's behavior and the ``fast`` default).

    ``max_tokens`` and ``wall_clock_seconds`` are the token and wall-clock
    companions of ``max_cost_usd`` (spec 00042, completing increment D of
    00036). Both share its exact semantics — PER-RUN cumulative, checked
    after the rollup but before grading, terminal ``Status.FAILED`` and
    non-retryable on breach, emitting the same
    ``harness.budget_ceiling_breached`` event (``payload["ceiling"]`` is
    ``"tokens"`` / ``"wall_clock_seconds"`` respectively). ``max_tokens``
    sums ``Attempt.total_tokens`` across all attempts; ``wall_clock_seconds``
    measures elapsed wall time from the run's earliest attempt
    ``started_at`` to ``clock()``. Each defaults to ``0`` (unenforced =
    today = ``fast``); independent of one another and of ``max_cost_usd``.

    ``max_iterations_per_attempt`` caps the inner ``intent=continue``
    loop within one ``Attempt``. The default of 1 means a single
    invocation per Attempt; raise it to allow multi-turn agents to
    iterate within one attempt before validation. When the cap is
    reached without a terminal envelope the Attempt finalizes as an
    agent error (no silent coercion to ``verify``).

    ``max_transient_retries`` is the SEPARATE, bounded budget for
    re-invoking a single iteration whose result is a transient
    infrastructure fault — a 429 / overload (5xx) ``api_error_status`` or a
    ``rejected`` ``RateLimitEvent`` — that yielded no usable completion. It
    is tracked independently of ``max_retries`` (the validation retry
    budget): a rate-limited iteration is re-invoked in place up to this many
    times before the iteration is finalized, so a run with ``max_retries=0``
    still rides out transient rate limits. The loop is bounded by this
    operator-supplied budget, never by an agent-supplied value, so it can
    never spin forever. ``transient_backoff`` is the shared
    :class:`flywheel_core.faults.BackoffPolicy` schedule waited between those
    re-invocations (the same capped-exponential helper every retry site
    reuses); ``transient_sleep`` is the awaitable sleep seam (default
    :func:`asyncio.sleep`) tests inject to capture the waits without
    blocking. A genuine non-transient missing/malformed envelope (no
    ``api_error_status``, no ``rejected`` rate-limit event) is unaffected and
    still consumes the validation budget as before.

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

    ``grader_env`` is the full environment ``command`` graders run with —
    the subprocess ``env`` REPLACES the inherited environment, so this is the
    complete mapping (ambient env plus any ``[sandbox.env]`` overrides),
    resolved by the orchestrator. ``None`` (the default) makes graders inherit
    the harness process environment exactly as before, so a run with no
    ``[sandbox.env]`` configured is byte-identical. Set, it lets a command
    grader see the same environment the agent built under (e.g. a shared Rust
    ``CARGO_TARGET_DIR`` / ``RUSTC_WRAPPER``).

    ``rubric_judge_model`` is the default model for rubric judges when
    the per-grader ``RubricGrader.judge_model`` is unset; ``None`` falls
    through to the SDK's own default. ``rubric_judge_max_turns`` caps
    the per-judge-call turn budget (default 32; judges get the full tool
    surface and multi-assertion rubrics routinely need more than 8 turns
    to verify against the worktree before emitting a verdict); a task's
    ``budgets.rubric_judge_max_turns`` overrides it for that task alone.
    ``rubric_judge_retries`` bounds the in-place judge-infra retry envelope
    (default 2): a judge dying on its own turn/time budget or transport
    re-runs alone instead of burning the task attempt and re-driving the
    whole implementation; ``0`` restores the old fail-immediately behavior.

    ``rubric_judge_invoke`` is a test seam: when set, the harness passes
    it to ``run_rubric_graders`` instead of the runner's default fresh
    ``claude_agent_sdk.query`` invoker. Production callers leave it
    ``None``.

    ``loop_guard`` carries the thresholds for the repeated-failure (STUCK)
    and identical-tuple-repeat (THRASH) detectors in
    :mod:`flywheel_core.loop_guard`. A fresh :class:`LoopGuard` is constructed
    per attempt from this config; each iteration's
    ``signals.tool_interactions`` is fed into it in arrival order. Each
    threshold disables independently via ``None`` / ``0``; default values
    keep the detectors on without tripping the existing harness suite.

    ``deadlines`` carries the default-on, operator-overridable wall-clock
    ceilings for the external-call classes (spec 00066). The harness reads
    ``deadlines.for_class(DeadlineClass.AGENT_ITERATION)`` to bound every
    working-agent iteration: when it resolves to a positive float the
    invocation is wrapped in :func:`flywheel_core.deadline.run_with_deadline`
    so an invoker that never returns (even one still streaming) is cancelled
    once the ceiling passes and routed to the timeout-classified
    ``INTERNAL_ERROR`` path; the default is finite and non-null (default-on).
    The bound is total wall-clock elapsed, additive to and independent of the
    inter-message silence watchdog (``loop_guard.hang_timeout_seconds``) — both
    run. An operator opts the class out (unbounded) with a ``0`` override,
    which resolves to ``None`` and restores the bare await path.

    ``context_window_tokens`` is the operator-supplied agent context-window
    capacity used by the context-recovery policy (spec 00018). When
    ``None`` (the default) the recovery policy is disabled and the harness
    behaves exactly as today; the SDK exposes no capacity source so the
    value is operator-supplied, mirroring the hang-watchdog pattern. When
    set, the harness compares the iteration's input-side occupancy against
    this capacity and recovers above
    ``context_recovery_trigger_ratio``.

    ``context_recovery_trigger_ratio`` is the occupancy fraction at which
    recovery fires (default ``0.9``). Must satisfy ``0 < ratio <= 1``;
    out-of-range values are rejected at construction rather than silently
    clamped (spec 00018 Error Handling).

    ``max_context_recoveries`` is the recovery budget per ``run_task``
    call (default ``1``), independent of ``max_retries``. Recovery is
    skipped once the budget is exhausted and the iteration loop follows
    its normal termination path.

    ``recovery_summarizer_invoke`` is a test seam mirroring
    ``rubric_judge_invoke``: when set, the harness passes it to the
    recovery summarizer instead of the runner's default fresh
    ``claude_agent_sdk.query`` invoker. Production callers leave it
    ``None``.

    ``landability_gate`` is a git-free opaque consumer hook (spec 00061)
    consulted exactly at the verify-passed ``VALIDATING -> DONE`` boundary,
    after graders pass and before the run lands as a success. It is a
    zero-argument callable returning ``None`` when the finished change is
    landable (the run proceeds to ``DONE`` byte-identically to today) or a
    non-empty *reason* string when it is not (an empty or uncommitted change
    under a git landing strategy). On a reason, the harness does NOT land the
    run as ``DONE``: it finalizes the attempt ``VALIDATION_FAILED`` and
    transitions ``FAILED_VALIDATION`` so the existing ``max_retries`` machinery
    re-drives the task against the same base, ending terminal ``FAILED`` (never
    ``DONE``) with the reason recorded once the budget is exhausted. ``None``
    (the default) disables the gate, byte-identical to today. Core stays
    git-unaware: the orchestrator supplies the closure that calls the
    strategy's landability predicate, the harness only consults the opaque
    callback.

    ``checkpoint_nudge_seconds`` is the remaining-wall-time threshold (seconds,
    default ``300.0``) at which the harness injects a single checkpoint-commit
    instruction onto the live agent session. The nudge fires when the remaining
    wall time to the resolved ``DeadlineClass.AGENT_ITERATION`` ceiling drops to
    this threshold AND ``checkpoint_progress_probe`` reports no new progress; it
    is dispatched through the same live ``ClaudeSDKClient.query`` surface an
    operator ``say`` uses and emits a distinct ``harness.checkpoint_nudge``
    event. The nudge does NOT move the deadline -- the iteration is still
    cancelled at the original ceiling. A value ``<= 0`` disables the nudge, as
    does an unbounded (opted-out) AGENT_ITERATION ceiling.

    ``checkpoint_progress_probe`` is the git-free, OPTIONAL closure the nudge
    consults for progress. It returns an opaque token; the invoker captures a
    baseline at iteration start and an equal token at nudge-check time means "no
    new progress since the iteration began". ``None`` (the default) leaves the
    nudge fully dormant so a bare core run is byte-for-byte unchanged. Core
    gains zero git awareness: the concrete git probe is supplied from above.
    """

    max_retries: int = 0
    max_cost_usd: float = 0.0
    max_tokens: int = 0
    wall_clock_seconds: int = 0
    max_iterations_per_attempt: int = 1
    max_transient_retries: int = 6
    transient_backoff: BackoffPolicy = field(default_factory=BackoffPolicy)
    transient_sleep: Callable[[float], Awaitable[None]] = asyncio.sleep
    artifacts_root: str | os.PathLike[str] | None = None
    agent_context: Mapping[str, str] = field(default_factory=dict)
    worktree: str | os.PathLike[str] | None = None
    grader_env: Mapping[str, str] | None = None
    rubric_judge_model: str | None = None
    rubric_judge_max_turns: int = 32
    rubric_judge_retries: int = 2
    rubric_judge_invoke: JudgeInvoke | None = None
    loop_guard: LoopGuardConfig = field(default_factory=LoopGuardConfig)
    deadlines: DeadlineConfig = field(default_factory=DeadlineConfig)
    context_window_tokens: int | None = None
    context_recovery_trigger_ratio: float = 0.9
    max_context_recoveries: int = 1
    recovery_summarizer_invoke: SummarizerInvoke | None = None
    landability_gate: Callable[[], str | None] | None = None
    checkpoint_nudge_seconds: float = 300.0
    checkpoint_progress_probe: Callable[[], object] | None = None

    def __post_init__(self) -> None:
        # Reject out-of-range ratio: must be in (0, 1]. Spec 00018
        # Error Handling requires construction-time rejection rather
        # than silent clamping.
        ratio = self.context_recovery_trigger_ratio
        if not isinstance(ratio, (int, float)) or isinstance(ratio, bool):
            raise ValueError(
                "context_recovery_trigger_ratio must be a number in (0, 1], "
                f"got {ratio!r}"
            )
        if ratio <= 0 or ratio > 1:
            raise ValueError(
                "context_recovery_trigger_ratio must be in (0, 1], "
                f"got {ratio!r}"
            )
        # Reject non-positive capacity (None is the disabled sentinel).
        capacity = self.context_window_tokens
        if capacity is not None:
            if (
                not isinstance(capacity, int)
                or isinstance(capacity, bool)
                or capacity <= 0
            ):
                raise ValueError(
                    "context_window_tokens must be a positive int or None, "
                    f"got {capacity!r}"
                )


@dataclass(frozen=True, kw_only=True)
class HarnessOutcome:
    """Final return value of :func:`run_task`.

    ``lifecycle`` is the mutated lifecycle in its terminal-or-paused
    state. ``attempts`` is a read-only snapshot of every Attempt
    persisted during the run, in ``number`` order; callers can also
    reload it from ``store.list_attempts`` and the two agree.

    ``session_limit_reset`` carries the derived session-limit reset instant
    (aware UTC) when the run hit a session-limit refusal whose reset time was
    derivable and still in the future -- the fast-abort surfaces it here
    structurally so a consumer (e.g. a claim-loop pool pause) never has to
    regex it back out of a lifecycle error string. ``None`` on every run that
    did not fast-abort on a future session-limit reset.
    """

    lifecycle: Lifecycle
    attempts: tuple[Attempt, ...]
    session_limit_reset: datetime | None = None


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


# Input-side context-occupancy keys: the model's resent conversation plus
# the cached prefix it reads. Mirrors the spec 00018 FR-1 occupancy
# definition (input + cache-read + cache-creation tokens) -- output_tokens
# are NOT input-side and are excluded.
_OCCUPANCY_USAGE_KEYS: tuple[str, ...] = (
    "input_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
)


# Mid-turn observe tiers (spec 00019 FR-3). Fixed 50 / 75 / 90 percent
# fractions; ``_drive_iterations`` emits ``harness.context_threshold_crossed``
# at the first mid-turn point occupancy reaches each tier, at most once
# per tier per iteration. The act ratio (``context_recovery_trigger_ratio``,
# default ``0.9``) stays separate -- these are observe-only legibility
# markers ahead of the act point.
_CONTEXT_OBSERVE_TIERS: tuple[float, ...] = (0.5, 0.75, 0.9)


# Capacity source labels surfaced in the ``harness.context_threshold_crossed``
# payload. ``sdk`` means the capacity came from
# :meth:`ClaudeSDKClient.get_context_usage`'s ``maxTokens``; ``operator``
# means it came from ``HarnessConfig.context_window_tokens``. The two are
# the only sources spec 00019 admits.
_CAPACITY_SOURCE_SDK: str = "sdk"
_CAPACITY_SOURCE_OPERATOR: str = "operator"


def _occupancy_from_usage(usage_payload: Mapping[str, Any]) -> int:
    """Sum the input-side token fields from an iteration's usage payload.

    The latest iteration's input-side tokens *are* the current context
    sent to the model; summing per-iteration deltas would double-count
    the re-sent conversation (spec 00018 Decisions Log). A missing or
    non-integer field contributes zero so a sparse payload (no usage
    data) yields zero and stays below any positive threshold -- the
    "no usage data" case in the spec Error Handling table.
    """
    total = 0
    for key in _OCCUPANCY_USAGE_KEYS:
        value = usage_payload.get(key)
        if isinstance(value, int) and not isinstance(value, bool):
            total += value
    return total


# Recovery trigger markers surfaced in the ``harness.context_recovery``
# audit payload (spec 00019 FR-6). ``boundary`` is the spec 00018
# iteration-tail crossing; ``mid_turn`` is the spec 00019 in-flight
# crossing that interrupts the iteration via
# :class:`flywheel_core.invoker_client.HarnessRecoveryRequested`. Operators
# read this field to attribute a recovery to the path that produced it.
_RECOVERY_TRIGGER_BOUNDARY: str = "boundary"
_RECOVERY_TRIGGER_MID_TURN: str = "mid_turn"


@dataclass(frozen=True, kw_only=True)
class _RecoveryTrigger:
    """Sentinel raised by :func:`_drive_iterations` when an iteration
    crosses the context-recovery threshold.

    Carries the occupancy, the transcript text the summarizer needs, the
    iteration number for the audit payload, and the ``trigger`` marker
    distinguishing the boundary crossing (spec 00018) from the in-flight
    mid-turn crossing (spec 00019). Pure data; only flywheel_core.harness
    builds and consumes it.
    """

    occupancy_tokens: int
    transcript_tail: str
    iteration_number: int
    trigger: str


@dataclass
class _RecoveryState:
    """Mutable per-``run_task`` recovery accounting.

    ``recoveries_used`` is the in-process counter that enforces FR-4's
    budget cap; the spec deliberately scopes it to the single
    ``run_task`` call (cross-process persistence is deferred).
    ``pending_handoff`` is the summary the harness must thread into the
    next attempt's :class:`IterationInputs`; once that attempt's
    :func:`_drive_iterations` consumes it the field returns to ``None``
    so subsequent attempts on the same run do not re-render the same
    section.
    """

    recoveries_used: int = 0
    pending_handoff: RecoveryHandoff | None = None


@dataclass
class _SessionLimitState:
    """Mutable per-``run_task`` holder for a derived session-limit reset.

    The session-limit fast-abort finalizes an attempt deep in
    :func:`_run_attempt_body`, but the derived reset must survive up to the
    :class:`HarnessOutcome` that :func:`run_task` builds after the retry
    loop. ``reset_at`` holds the most recently derived future reset (aware
    UTC); it stays ``None`` on any run that never fast-aborted on a future
    session-limit reset.
    """

    reset_at: datetime | None = None


class _HangDetected(Exception):
    """Sentinel raised when the hang watchdog cancels an invocation.

    Carries the context needed for the FR-3 finalization shape
    (``Outcome.INTERNAL_ERROR`` / ``running -> internal_error``). The
    ``harness.hang_detected`` audit event is emitted inside
    :func:`_invoke_with_watchdog` before this is raised; the
    :func:`_run_attempt` boundary catches this BEFORE
    :exc:`asyncio.CancelledError` so a watchdog-induced cancellation never
    reaches the operator-interrupt path (:func:`_handle_interrupt`).
    See ``.flywheel/specs/00015-FEATURE-loop-safety-net.md`` FR-4.
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


class _IterationDeadlineExceeded(Exception):
    """Sentinel raised when the wall-clock deadline cancels an iteration.

    The wall-clock agent-iteration deadline (spec 00066 criterion #2, D-2)
    is additive to the inter-message silence watchdog: it fires on total
    elapsed time since the invocation started, even while the invoker is
    still streaming, so a steadily-producing-but-never-terminating agent is
    cut off where the silence watchdog would not catch it. The
    :class:`flywheel_core.deadline.DeadlineExceeded` raised by
    :func:`flywheel_core.deadline.run_with_deadline` is translated into this
    harness-local sentinel so the :func:`_run_attempt` boundary can route it
    to the timeout-classified containment path (``Outcome.INTERNAL_ERROR`` /
    ``running -> internal_error``) — the same retryable infrastructure class
    the hang watchdog uses — distinct from an operator-driven cancellation.
    The ``harness.deadline_exceeded`` audit event is emitted inside
    :func:`_drive_iterations` before this is raised.
    """

    def __init__(
        self,
        *,
        attempt_number: int,
        iteration_number: int,
        ceiling_seconds: float,
        elapsed_seconds: float | None,
    ) -> None:
        elapsed_detail = (
            "" if elapsed_seconds is None else f" after {elapsed_seconds:.3f}s"
        )
        super().__init__(
            f"agent-iteration deadline: invocation exceeded wall-clock "
            f"ceiling of {ceiling_seconds:.3f}s{elapsed_detail}"
        )
        self.attempt_number = attempt_number
        self.iteration_number = iteration_number
        self.ceiling_seconds = ceiling_seconds
        self.elapsed_seconds = elapsed_seconds


# Marker kind appended (best-effort) to the sink itself when its first
# append fails, per spec 00025 FR-7 ("stderr + a marker line attempt in
# the sink when possible"). A sink that recovered mid-run will carry the
# marker; a permanently broken one simply drops it.
_TELEMETRY_FAILED_KIND: str = "harness.telemetry_sink_failed"

# Prefix discriminating mirrored domain-event lines from harness
# telemetry kinds and SDK message types inside a run's telemetry stream.
# The store row remains authoritative for state; the mirror line exists
# so the run file renders a self-contained timeline (spec 00025 FR-4).
_DOMAIN_MIRROR_KIND_PREFIX: str = "domain."


class _RunTelemetry:
    """Best-effort telemetry emitter for one run (spec 00025 FR-3/FR-4/FR-7).

    Wraps a :class:`flywheel_core.store_protocols.TelemetrySink` with the
    harness's failure policy: telemetry loss is acceptable, so an append
    failure never raises. The first failure is recorded once on stderr
    (plus a best-effort marker line in the sink itself); subsequent
    failures for the run are silent. A ``None`` sink drops every record —
    callers that want durable telemetry hand in a concrete sink (the
    workflow CLI wires :class:`flywheel_core.telemetry_file.FileTelemetrySink`).
    """

    def __init__(
        self,
        sink: TelemetrySink | None,
        *,
        run_id: str,
        clock: Callable[[], datetime],
    ) -> None:
        self._sink = sink
        self._run_id = run_id
        self._clock = clock
        self._failure_reported = False

    def emit(
        self,
        *,
        kind: str,
        payload: Mapping[str, Any],
        attempt_number: int | None = None,
        iteration_number: int | None = None,
    ) -> None:
        """Append one harness telemetry record. Never raises."""
        self._append(
            TelemetryRecord(
                run_id=self._run_id,
                ts=self._clock(),
                kind=kind,
                payload=dict(payload),
                attempt_number=attempt_number,
                iteration_number=iteration_number,
            )
        )

    def sdk_message(
        self,
        message: Message,
        *,
        attempt_number: int,
        iteration_number: int,
    ) -> None:
        """Append one SDK message the instant it is observed. Never raises.

        ``kind`` is the SDK class name (``AssistantMessage``, ...); the
        payload is the verbatim :func:`_serialize_sdk_message` dict, the
        same shape the store used to persist, so downstream renderers
        keep working off one serialization.
        """
        payload = _serialize_sdk_message(message)
        kind = str(payload.get("message_type", payload.get("type", "")))
        self._append(
            TelemetryRecord(
                run_id=self._run_id,
                ts=self._clock(),
                kind=kind,
                payload=payload,
                attempt_number=attempt_number,
                iteration_number=iteration_number,
            )
        )

    def mirror_domain(self, event: DomainEvent) -> None:
        """Mirror one ledger event as a ``domain.<kind>`` line. Never raises.

        Called after the authoritative store append succeeds, so the file
        timeline interleaves state changes with messages and telemetry in
        emission order. The line is disposable; the row is the truth.
        """
        self._append(
            TelemetryRecord(
                run_id=self._run_id,
                ts=event.ts,
                kind=f"{_DOMAIN_MIRROR_KIND_PREFIX}{event_kind(event)}",
                payload=event_payload(event),
                attempt_number=event.attempt_number,
            )
        )

    def _append(self, record: TelemetryRecord) -> None:
        if self._sink is None:
            return
        try:
            self._sink.append_telemetry(record)
        except Exception as exc:
            self._note_failure(exc)

    def _note_failure(self, exc: Exception) -> None:
        """FR-7: record the first sink failure once, then go silent."""
        if self._failure_reported:
            return
        self._failure_reported = True
        print(
            f"flywheel: telemetry sink append failed for run "
            f"{self._run_id}: {type(exc).__name__}: {exc} — continuing; "
            f"further telemetry for this run may be lost",
            file=sys.stderr,
            flush=True,
        )
        if self._sink is None:
            return
        try:
            self._sink.append_telemetry(
                TelemetryRecord(
                    run_id=self._run_id,
                    ts=self._clock(),
                    kind=_TELEMETRY_FAILED_KIND,
                    payload={
                        "error_type": type(exc).__name__,
                        "message": str(exc),
                    },
                )
            )
        except Exception:
            pass


class _MirroringStore:
    """Wrap a :class:`HarnessStore` so every ledger append also lands as a
    ``domain.*`` mirror line in the run's telemetry stream (FR-4).

    ``append_domain_event`` is the harness's single authoritative
    state-write seam (see :func:`_append`); mirroring here keeps the
    store row first — the mirror fires only after the append returned —
    and covers every transition without threading the telemetry session
    through each call site. All other store verbs delegate verbatim.
    """

    def __init__(
        self, wrapped: HarnessStore, telemetry: _RunTelemetry
    ) -> None:
        self._wrapped = wrapped
        self._telemetry = telemetry

    def append_domain_event(
        self, event: DomainEvent, *, expected_version: int
    ) -> Lifecycle:
        folded = self._wrapped.append_domain_event(
            event, expected_version=expected_version
        )
        self._telemetry.mirror_domain(event)
        return folded

    def create_lifecycle(self, lifecycle: Lifecycle) -> None:
        self._wrapped.create_lifecycle(lifecycle)

    def update_lifecycle(
        self, lifecycle: Lifecycle, *, expected_version: int
    ) -> None:
        self._wrapped.update_lifecycle(
            lifecycle, expected_version=expected_version
        )

    def load_lifecycle(self, run_id: str) -> Lifecycle | None:
        return self._wrapped.load_lifecycle(run_id)

    def save_task(self, task: Task, *, now: datetime) -> str:
        return self._wrapped.save_task(task, now=now)

    def list_domain_events(self, run_id: str) -> list[DomainEvent]:
        return self._wrapped.list_domain_events(run_id)

    def save_attempt(
        self,
        run_id: str,
        attempt: Attempt,
        *,
        expected_version: int | None = None,
    ) -> None:
        self._wrapped.save_attempt(
            run_id, attempt, expected_version=expected_version
        )

    def list_attempts(self, run_id: str) -> list[Attempt]:
        return self._wrapped.list_attempts(run_id)

    def append_grader_result(
        self, result: GraderResultRecord
    ) -> GraderResultRecord:
        return self._wrapped.append_grader_result(result)

    def list_grader_results(
        self, run_id: str, attempt_number: int
    ) -> list[GraderResultRecord]:
        return self._wrapped.list_grader_results(run_id, attempt_number)

    def claim_commands(
        self, run_id: str, *, now: datetime
    ) -> list[ControlCommandRecord]:
        return self._wrapped.claim_commands(run_id, now=now)

    def delete_command(self, command_id: int) -> None:
        self._wrapped.delete_command(command_id)


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
    telemetry: _RunTelemetry,
    lifecycle: Lifecycle,
    attempt: Attempt | None,
    clock: Callable[[], datetime],
) -> None:
    """In-band finalization for an operator-driven SIGINT/SIGTERM.

    Closes the open attempt as :attr:`Outcome.INTERNAL_ERROR`, emits a
    ``harness.interrupted`` telemetry event, and transitions the
    lifecycle to :attr:`Status.INTERRUPTED`. INTERRUPTED is not a
    retry-source state, so the retry budget is preserved and the next
    worker start can resume the run through ``ready``.

    Idempotent: when the lifecycle is not in :data:`_INTERRUPTIBLE_STATUSES`
    (already finalized, between attempts, or terminal) the function is a
    no-op. This makes a second signal during shutdown safe — the cancel is
    scheduled at the next await point, but the synchronous finalization has
    already completed by then, so re-entering this helper just exits early.

    Best-effort append: a store-side failure inside the AttemptFinalized
    append is swallowed (the interrupt finalizer must not raise back into
    the shutdown path). The status transition remains the source of truth.

    If the INTERRUPTED transition itself fails (a transient store fault
    during shutdown), it too must not raise back into the shutdown path,
    but the failure is not swallowed silently: a ``harness.crash`` event
    (``classification='interrupt_transition_failed'``) is emitted so the
    row left stuck at RUNNING/VALIDATING is observable. The stranded-
    recovery sweep repairs the row on the next worker start.
    """
    if lifecycle.status not in _INTERRUPTIBLE_STATUSES:
        return
    reason = "operator interrupted mid-attempt"
    from_status = lifecycle.status.value
    telemetry.emit(
        kind="harness.interrupted",
        payload={
            "classification": "worker_interrupted",
            "from_status": from_status,
            "message": reason,
        },
        attempt_number=attempt.number if attempt is not None else None,
    )
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
        telemetry.emit(
            kind="harness.attempt_finalized",
            payload={
                "number": attempt.number,
                "outcome": Outcome.INTERNAL_ERROR.value,
                "error": reason,
            },
            attempt_number=attempt.number,
        )
    try:
        _transition(
            lifecycle,
            Status.INTERRUPTED,
            store=store,
            now=clock,
        )
    except Exception as exc:  # noqa: BLE001 - must not raise into shutdown
        # The INTERRUPTED write failed (transient store fault during
        # shutdown). We must not raise back into the shutdown path, but the
        # failure has to be observable: the row is left in RUNNING/VALIDATING
        # for the stranded-recovery sweep to repair on the next worker start.
        # Mirror the harness.crash payload shape so no new event schema is
        # introduced.
        telemetry.emit(
            kind="harness.crash",
            payload={
                "classification": "interrupt_transition_failed",
                "exception_type": type(exc).__name__,
                "message": str(exc),
            },
            attempt_number=attempt.number if attempt is not None else None,
        )


def _handle_hang_detected(
    exc: _HangDetected,
    *,
    store: HarnessStore,
    telemetry: _RunTelemetry,
    lifecycle: Lifecycle,
    attempt: Attempt,
    clock: Callable[[], datetime],
) -> None:
    """Finalize an attempt whose invocation the hang watchdog cancelled.

    Mirrors the crash path: closes the open attempt as
    :attr:`Outcome.INTERNAL_ERROR` and transitions the lifecycle to
    :attr:`Status.INTERNAL_ERROR` (the infrastructure class, per FR-3 of
    ``.flywheel/specs/00015-FEATURE-loop-safety-net.md``).

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
            telemetry=telemetry,
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


def _handle_iteration_deadline(
    exc: _IterationDeadlineExceeded,
    *,
    store: HarnessStore,
    telemetry: _RunTelemetry,
    lifecycle: Lifecycle,
    attempt: Attempt,
    clock: Callable[[], datetime],
) -> None:
    """Finalize an attempt whose invocation the wall-clock deadline cancelled.

    Mirrors :func:`_handle_hang_detected`: closes the open attempt as
    :attr:`Outcome.INTERNAL_ERROR` and transitions the lifecycle to
    :attr:`Status.INTERNAL_ERROR` — the retryable infrastructure class, the
    timeout-classified containment outcome (spec 00066 criterion #2, D-4).
    The ``harness.deadline_exceeded`` audit event was emitted inside
    :func:`_drive_iterations` before the cancel — this helper does not
    re-emit it. Idempotent against ``attempt.ended_at is not None`` so a
    near-simultaneous normal completion that already finalized the attempt
    cannot be double-written.
    """
    error = str(exc)
    if attempt.ended_at is None:
        _finalize_attempt(
            store=store,
            telemetry=telemetry,
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
    from flywheel_core._sdk import AssistantMessage, ResultMessage

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
    :class:`flywheel_core.grader_transcript.TranscriptCounter` — when the
    invoker has already drained the stream, the harness recomputes the
    same totals from the recorded messages so the validation-time
    grader and the hard-limit enforcer converge on identical numbers.
    """
    from flywheel_core._sdk import AssistantMessage, ResultMessage

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


def _transient_rate_limit_reason(signals: InvocationSignals) -> str | None:
    """Return a short reason when ``signals`` mark a transient rate limit.

    Consumes the signals the invoker already collected — never re-derives
    them from raw messages. A 429 / overload / transient-5xx
    ``api_error_status`` is bucketed through the shared
    :func:`flywheel_core.faults.classify_fault` classifier (the one TRANSIENT
    taxonomy every retry site reuses); a ``rejected``
    :class:`claude_agent_sdk.RateLimitEvent` is a hard rate-limit block in
    its own right. Attributes are read via ``getattr`` so this module never
    imports the optional SDK types. Returns ``None`` when no transient
    rate-limit signal is present.
    """
    status = signals.api_error_status
    if status is not None and classify_fault(status) is FaultClass.TRANSIENT:
        return f"api_error_status={status}"
    for event in signals.rate_limit_events:
        info = getattr(event, "rate_limit_info", None)
        if getattr(info, "status", None) == "rejected":
            rl_type = getattr(info, "rate_limit_type", None)
            return f"rate_limit_rejected:{rl_type}"
    return None


def _iteration_is_transient_rate_limit(result: IterationResult) -> str | None:
    """Return the transient reason when ``result`` is a rate-limited iteration.

    A rate limit yields no usable completion, so a valid envelope of any
    intent is never treated as transient (the agent produced actionable
    output — a mere ``allowed_warning`` rate-limit notice alongside a valid
    ``verify`` must not trigger a re-invocation). Otherwise the presence of a
    transient signal on the iteration's :class:`InvocationSignals` decides.
    """
    if isinstance(result.envelope, ValidEnvelope):
        return None
    return _transient_rate_limit_reason(result.signals)


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
                    ordinal=row.ordinal,
                )
            )
        return tuple(findings)
    return ()


def _collect_prior_manual_findings(
    store: HarnessStore,
    run_id: str,
    current_attempt_number: int,
) -> tuple[ManualFinding, ...]:
    """Build ``IterationInputs.prior_manual_findings`` from prior attempts.

    Sibling of :func:`_collect_prior_rubric_findings` for the manual
    grader receipts spec 00016 introduces. Walks attempts backwards from
    ``current_attempt_number - 1`` to find the most recent attempt that
    has failing manual records (operator rejections, per FR-6) and
    returns those rows in ordinal order so the renderer's
    ``(attempt_number, ordinal)`` ordering interleaves them with rubric
    findings from the same attempt.

    Manual receipts are keyed to the ``SUCCEEDED`` attempt that passed
    every automated grader (the rejection is a lifecycle-level gate, not
    an agent failure — see spec 00016's ``SUCCEEDED`` semantics NFR), so
    the same backwards walk that drives the rubric collector lands on
    the rejected-but-succeeded attempt and surfaces its operator
    feedback into the retry's prompt.

    Returns an empty tuple when no prior attempt carried any failing
    manual record (including the first attempt of a lifecycle, when the
    rubric collector returns empty for the same reason).
    """
    if current_attempt_number <= 1:
        return ()
    for prior in range(current_attempt_number - 1, 0, -1):
        rows = store.list_grader_results(run_id, prior)
        failing = [
            r
            for r in rows
            if r.grader_type == "manual" and not r.passed
        ]
        if not failing:
            continue
        findings: list[ManualFinding] = []
        for row in sorted(failing, key=lambda r: r.ordinal):
            summary_raw = row.payload.get("summary")
            summary = summary_raw if isinstance(summary_raw, str) else ""
            findings.append(
                ManualFinding(
                    grader_name=row.grader_name or "<unnamed>",
                    attempt_number=row.attempt_number,
                    summary=summary,
                    ordinal=row.ordinal,
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


def _spawn_failed_grader(
    results: Sequence[GraderResultRecord],
) -> GraderResultRecord | None:
    """Return the first failing command grader whose subprocess never
    started.

    A ``spawn_failure`` termination (the ``Popen`` raised ``OSError``) is an
    infrastructure failure, not the code under test asserting false. It must
    route to the retryable ``INTERNAL_ERROR`` class — distinct from a grader
    that ran and exited non-zero (``VALIDATION_FAILED``).
    """
    for result in results:
        if result.passed:
            continue
        if result.grader_type != "command":
            continue
        if result.payload.get("termination") == "spawn_failure":
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
# flywheel_core.lifecycle._REQUIRES_ERROR. Mirrored here so the harness's
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
    telemetry: _RunTelemetry,
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

    * The crash telemetry emit is best-effort by construction
      (:class:`_RunTelemetry` never raises) so the recorder cannot
      itself loop and mask the original exception (the caller re-raises
      after this returns).
    * A transition failure (e.g.
      :class:`~flywheel_core.store_protocols.OptimisticConcurrencyError`
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
    telemetry.emit(
        kind="harness.crash",
        payload={
            "classification": "entry_error",
            "exception_type": type(exception).__name__,
            "message": str(exception),
        },
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
    sink: TelemetrySink | None = None,
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

    ``sink`` is the run's telemetry destination (spec 00025): every SDK
    message, every ``harness.*`` telemetry event, and a ``domain.*``
    mirror of every ledger append stream to it in emission order. Sink
    appends are best-effort — a failure is recorded once on stderr and
    the run continues (FR-7) — while ledger and lifecycle writes keep
    their strict failure semantics. ``None`` (the default) drops
    telemetry; production callers wire a
    :class:`flywheel_core.telemetry_file.FileTelemetrySink`.

    **Entry ordering and resume reconciliation.** The first store
    interaction appends a
    :class:`~flywheel_core.events.LifecycleInitialized` domain event, which
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
    :class:`~flywheel_core.store_protocols.OptimisticConcurrencyError`
    surfaces to the caller verbatim — not swallowed — so the worker
    learns its run_id is racing.

    **Pre-lifecycle crashes.** Under event sourcing the projection row is
    created by the very first append (the ``LifecycleInitialized`` event),
    so there is no window in which events exist before the row — the
    silent-crash shape documented in
    ``.flywheel/audits/08-recoverable-blocked-lifecycles.md`` is closed by
    construction. If that first append itself fails (catastrophic store
    breakage), the exception propagates straight to the caller with no
    partial state to reconcile; the worker-side circuit breaker covers the
    repeating-spawn shape.
    """

    config = config or HarnessConfig()
    invoker = invoke or _default_invoke
    clock = now or _utcnow
    mclock = monotonic or _default_monotonic
    # In-process recovery accounting (spec 00018 FR-4). Scoped to this
    # ``run_task`` call so the budget is independent of ``max_retries``
    # and resets on a fresh process; carries the most recently produced
    # handoff so the next ``_run_attempt`` renders ``# Recovery handoff``
    # on its first iteration prompt.
    recovery_state = _RecoveryState()
    # Cross-attempt holder for a derived session-limit reset (spec: session
    # limit classification). A future-reset fast-abort finalizes an attempt
    # inside _run_attempt_body; this carries the derived reset up to the
    # HarnessOutcome built below.
    session_state = _SessionLimitState()

    # Telemetry session for the run (spec 00025): SDK messages and
    # harness telemetry stream to the sink; the store keeps only ledger
    # and OLTP writes. The _MirroringStore wrap makes every ledger
    # append also land as a domain.* line in the same stream, store row
    # first, so the run file is a self-contained timeline. A None sink
    # drops telemetry — production callers wire a FileTelemetrySink.
    telemetry = _RunTelemetry(sink, run_id=lifecycle.run_id, clock=clock)
    store = _MirroringStore(store, telemetry)

    # Seed the lifecycle by appending the first domain event. Under event
    # sourcing the lifecycle row *is* the projection of this event, so the
    # log and the row come into existence together — there is no window in
    # which events could exist before the row (the silent pre-lifecycle
    # crash shape that .flywheel/audits/08-recoverable-blocked-lifecycles.md
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
                source=lifecycle.source,
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
            if lifecycle.status == Status.AWAITING_APPROVAL:
                # Parked on a manual-grader gate; the attempt was finalized
                # SUCCEEDED at gate entry. The resolver (spec 00016) applies
                # the operator's approve/reject decision out-of-band on the
                # orchestrator's reactive sweep, so this run_task call
                # returns here exactly like the INTERRUPTED park does.
                break

            if lifecycle.status == Status.READY:
                await _run_attempt(
                    task=task,
                    lifecycle=lifecycle,
                    store=store,
                    telemetry=telemetry,
                    config=config,
                    invoker=invoker,
                    clock=clock,
                    mclock=mclock,
                    recovery_state=recovery_state,
                    session_state=session_state,
                )
                continue

            if lifecycle.status in (
                Status.FAILED_VALIDATION,
                Status.INTERNAL_ERROR,
            ):
                if lifecycle.is_retry_eligible(config.max_retries):
                    telemetry.emit(
                        kind="harness.retry_scheduled",
                        payload={
                            "retries_used": lifecycle.retries,
                            "max_retries": config.max_retries,
                        },
                    )
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
            store, telemetry, lifecycle, exc, clock=clock
        )
        raise

    attempts = tuple(store.list_attempts(lifecycle.run_id))
    return HarnessOutcome(
        lifecycle=lifecycle,
        attempts=attempts,
        session_limit_reset=session_state.reset_at,
    )


async def _run_attempt(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    telemetry: _RunTelemetry,
    config: HarnessConfig,
    invoker: InvokeFunc,
    clock: Callable[[], datetime],
    mclock: Callable[[], float],
    recovery_state: _RecoveryState,
    session_state: _SessionLimitState,
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
        telemetry.emit(
            kind="harness.attempt_started",
            payload={
                "number": attempt_number,
                "agent_context": dict(config.agent_context),
            },
            attempt_number=attempt_number,
        )

        attempt_dir = _ensure_attempt_dir(config, lifecycle, attempt_number)
        transcript_graders: tuple[TranscriptGrader, ...] = tuple(
            g for g in task.graders if isinstance(g, TranscriptGrader)
        )

        await _run_attempt_body(
            task=task,
            lifecycle=lifecycle,
            store=store,
            telemetry=telemetry,
            config=config,
            invoker=invoker,
            clock=clock,
            mclock=mclock,
            attempt=attempt,
            attempt_dir=attempt_dir,
            transcript_graders=transcript_graders,
            recovery_state=recovery_state,
            session_state=session_state,
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
            telemetry=telemetry,
            lifecycle=lifecycle,
            attempt=attempt,
            clock=clock,
        )
    except _IterationDeadlineExceeded as exc:
        # Wall-clock agent-iteration deadline fired (spec 00066 criterion
        # #2): the invocation exceeded its finite ceiling -- even while it
        # was still streaming -- so run_with_deadline cancelled it. Route to
        # the same timeout-classified internal_error containment path the
        # hang watchdog uses; distinct from the operator-interrupt path
        # because run_with_deadline translated the DeadlineExceeded into this
        # harness-local sentinel rather than letting a bare CancelledError
        # propagate. The harness.deadline_exceeded audit event was already
        # emitted inside _drive_iterations before the cancel.
        _handle_iteration_deadline(
            exc,
            store=store,
            telemetry=telemetry,
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
            telemetry=telemetry,
            lifecycle=lifecycle,
            attempt=attempt,
            clock=clock,
        )
        raise
    except Exception as exc:
        # Any other exception escaping the attempt body (e.g. an OSError from
        # run_command_graders during validation, or a store write failure) is
        # an entry-class crash. run_task's top-level handler emits one
        # harness.crash and walks the lifecycle to FAILED, but it holds no
        # reference to this attempt -- finalize the open attempt here (as the
        # interrupt and hang paths do) so a terminal FAILED lifecycle never
        # strands an attempt with ended_at=None, which finalize_stranded_
        # lifecycle (RUNNING/VALIDATING only) would never repair. Re-raise
        # unchanged so the crash still propagates and the worker exits non-zero.
        if attempt.ended_at is None:
            try:
                _finalize_attempt(
                    store=store,
                    telemetry=telemetry,
                    lifecycle=lifecycle,
                    attempt=attempt,
                    outcome=Outcome.INTERNAL_ERROR,
                    error=f"attempt crashed: {type(exc).__name__}: {exc}",
                    clock=clock,
                )
            except Exception:
                # A secondary failure (often the same store error that caused
                # the crash) must not mask the original; the stranded-recovery
                # sweep remains the backstop.
                pass
        raise


async def _run_attempt_body(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    telemetry: _RunTelemetry,
    config: HarnessConfig,
    invoker: InvokeFunc,
    clock: Callable[[], datetime],
    mclock: Callable[[], float],
    attempt: Attempt,
    attempt_dir: Path | None,
    transcript_graders: tuple[TranscriptGrader, ...],
    recovery_state: _RecoveryState,
    session_state: _SessionLimitState,
) -> None:
    """Inner body of :func:`_run_attempt`, split out so the hang /
    interrupt sentinel exceptions can be caught at one boundary.

    Telemetry writes inside this function (``telemetry.emit`` and the
    per-message observer built in :func:`_drive_iterations`) are
    best-effort and never raise (spec 00025 FR-7); only ledger and OLTP
    store writes can fail, and those propagate to the caller unchanged.
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
        recovery_trigger,
        transient_exhausted,
        session_limit_reset,
    ) = await _drive_iterations(
        task=task,
        lifecycle=lifecycle,
        store=store,
        telemetry=telemetry,
        config=config,
        invoker=invoker,
        clock=clock,
        mclock=mclock,
        attempt=attempt,
        transcript_graders=transcript_graders,
        loop_guard=loop_guard,
        recovery_state=recovery_state,
    )

    # Mid-turn recovery (spec 00019 FR-4): _drive_iterations interrupts
    # the in-flight iteration via the recovery_interrupt_event seam,
    # catches HarnessRecoveryRequested, and returns a recovery_trigger
    # with no iteration_result attached. Route to the summarize-restart
    # action before the iteration_result-None check below, which is the
    # cap<=0 no-iterations-ran path. The mid-turn path uses the
    # transcript_tail accumulated from streamed AssistantMessage text
    # blocks as both the summarizer input and the attempt's
    # ``agent_output`` (the iteration never produced a full transcript).
    if iteration_result is None and recovery_trigger is not None:
        lifecycle.agent_output = recovery_trigger.transcript_tail
        await _handle_context_recovery(
            task=task,
            lifecycle=lifecycle,
            store=store,
            telemetry=telemetry,
            config=config,
            attempt=attempt,
            attempt_number=attempt_number,
            recovery_trigger=recovery_trigger,
            recovery_state=recovery_state,
            clock=clock,
        )
        return

    if iteration_result is None:
        # No invocation happened (cap <= 0). Treat as a protocol-class
        # agent error so the retry policy can still kick in.
        error = "no iterations were invoked"
        _transition(lifecycle, Status.VALIDATING, store=store, now=clock)
        _finalize_attempt(
            store=store,
            telemetry=telemetry,
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

    # Transient rate-limit exhaustion / session-limit fast-abort: the
    # iteration returned a rate-limit fault that will not clear by re-invoking
    # now. Either the separate transient budget was spent (exhaustion) or the
    # refusal named a future reset that made retrying pointless (fast-abort,
    # exactly one invocation). Both are infrastructure faults, not agent
    # protocol errors, so they route to the retryable, TRANSIENT-classified
    # INTERNAL_ERROR class (the same class the hang watchdog and store faults
    # use) rather than the AGENT_ERROR / FAILED_VALIDATION path a genuine
    # missing envelope takes. The per-retry / exhaustion / session-limit
    # telemetry already emitted inside _drive_iterations; this finalizes the
    # attempt and transitions. A derived future reset is surfaced
    # structurally on the run's HarnessOutcome via ``session_state`` -- never
    # as something a consumer must regex back out of ``error``.
    if transient_exhausted:
        if session_limit_reset is not None:
            session_state.reset_at = session_limit_reset
            error = (
                "session-limit refusal; reset at "
                f"{session_limit_reset.isoformat()}"
            )
        else:
            error = (
                "transient rate-limit retries exhausted after "
                f"{config.max_transient_retries + 1} invocations"
            )
        _finalize_attempt(
            store=store,
            telemetry=telemetry,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.INTERNAL_ERROR,
            error=error,
            agent_output=iteration_result.transcript,
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

    # Crash takes priority and goes straight to FAILED — refined
    # classification is deferred (see _DEFERRED_LOOP_SUBSYSTEMS).
    if iteration_result.failure is not None:
        failure = iteration_result.failure
        crash_error = f"crashed: {failure.error_type}: {failure.message}"
        telemetry.emit(
            kind="harness.crash",
            payload={
                "error_type": failure.error_type,
                "message": failure.message,
                "exit_code": failure.exit_code,
                "stderr": failure.stderr,
                "classification": "deferred",
            },
            attempt_number=attempt_number,
        )
        _finalize_attempt(
            store=store,
            telemetry=telemetry,
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
                telemetry=telemetry,
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
                telemetry=telemetry,
                lifecycle=lifecycle,
                attempt=attempt,
                attempt_number=attempt_number,
                verdict=loop_guard_verdict,
                transcript=iteration_result.transcript,
                clock=clock,
            )
            return

    # Context-recovery action (spec 00018 FR-3). The trigger fires only
    # for a validly-continuing iteration (intent=continue) with budget
    # remaining and no loop-guard verdict -- the precedence checks above
    # have already exited. The summarize-restart action finalizes this
    # attempt with Outcome.RECOVERED, emits harness.context_recovery
    # before scheduling the recovery attempt, and transitions
    # running -> interrupted -> ready so the outer ``run_task`` loop
    # picks up a fresh attempt with the handoff threaded into its
    # IterationInputs.
    if recovery_trigger is not None:
        await _handle_context_recovery(
            task=task,
            lifecycle=lifecycle,
            store=store,
            telemetry=telemetry,
            config=config,
            attempt=attempt,
            attempt_number=attempt_number,
            recovery_trigger=recovery_trigger,
            recovery_state=recovery_state,
            clock=clock,
        )
        return

    # Per-run budget ceilings (cost: spec 00039; tokens + wall-clock: spec
    # 00042, completing increment D of 00036). All three share one shape:
    # PER-RUN cumulative, checked after the per-iteration rollup but BEFORE
    # grading so a breach pre-empts the grade (a run that blew its budget
    # does not get to pass), terminal Status.FAILED and non-retryable on
    # breach. The current attempt may already appear in list_attempts via
    # its in-loop save_attempt, so it is excluded from the prior set and
    # re-added from the in-memory object to avoid double counting. A zero
    # ceiling (the fast default) is unenforced. Cost is checked first, then
    # tokens, then wall-clock; the first breach ends the run.
    if config.max_cost_usd > 0 or config.max_tokens > 0 or config.wall_clock_seconds > 0:
        prior = [
            a
            for a in store.list_attempts(lifecycle.run_id)
            if a.number != attempt.number
        ]
        if config.max_cost_usd > 0:
            run_total_cost = sum(a.total_cost_usd for a in prior) + attempt.total_cost_usd
            if run_total_cost >= config.max_cost_usd:
                _finalize_budget_breach(
                    store=store,
                    telemetry=telemetry,
                    lifecycle=lifecycle,
                    attempt=attempt,
                    attempt_number=attempt_number,
                    ceiling="cost_usd",
                    limit=config.max_cost_usd,
                    observed=run_total_cost,
                    transcript=iteration_result.transcript,
                    clock=clock,
                )
                return
        if config.max_tokens > 0:
            run_total_tokens = sum(a.total_tokens for a in prior) + attempt.total_tokens
            if run_total_tokens >= config.max_tokens:
                _finalize_budget_breach(
                    store=store,
                    telemetry=telemetry,
                    lifecycle=lifecycle,
                    attempt=attempt,
                    attempt_number=attempt_number,
                    ceiling="tokens",
                    limit=config.max_tokens,
                    observed=run_total_tokens,
                    transcript=iteration_result.transcript,
                    clock=clock,
                )
                return
        if config.wall_clock_seconds > 0:
            run_started = min(
                [a.started_at for a in prior] + [attempt.started_at]
            )
            elapsed_seconds = (clock() - run_started).total_seconds()
            if elapsed_seconds >= config.wall_clock_seconds:
                _finalize_budget_breach(
                    store=store,
                    telemetry=telemetry,
                    lifecycle=lifecycle,
                    attempt=attempt,
                    attempt_number=attempt_number,
                    ceiling="wall_clock_seconds",
                    limit=config.wall_clock_seconds,
                    observed=elapsed_seconds,
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
                telemetry=telemetry,
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
                telemetry.emit(
                    kind="harness.protocol_failure",
                    payload={
                        "kind": "invalid_blocked_requires",
                        "reason": requires_error,
                        "intent": envelope.intent.value,
                    },
                    attempt_number=attempt_number,
                )
                _finalize_attempt(
                    store=store,
                    telemetry=telemetry,
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
                telemetry=telemetry,
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
            telemetry.emit(
                kind="harness.blocked",
                payload={
                    "reason": reason,
                    "requires": requires_payload,
                },
                attempt_number=attempt_number,
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
        telemetry.emit(
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
            telemetry=telemetry,
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
            telemetry=telemetry,
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
            telemetry=telemetry,
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
    telemetry.emit(
        kind="harness.protocol_failure",
        payload=_envelope_payload(envelope),
        attempt_number=attempt_number,
    )
    _finalize_attempt(
        store=store,
        telemetry=telemetry,
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
    telemetry: _RunTelemetry,
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
        telemetry=telemetry,
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
    telemetry.emit(
        kind="harness.stuck",
        payload={
            "reason": reason,
            "tool_name": verdict.tool_name,
            "input_digest": verdict.input_digest,
            "requires": requires_payload,
        },
        attempt_number=attempt_number,
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
    telemetry: _RunTelemetry,
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
    telemetry.emit(
        kind="harness.thrash_detected",
        payload={
            "reason": reason,
            "tool_name": verdict.tool_name,
            "input_digest": verdict.input_digest,
        },
        attempt_number=attempt_number,
    )
    _transition(lifecycle, Status.VALIDATING, store=store, now=clock)
    _finalize_attempt(
        store=store,
        telemetry=telemetry,
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


def _handoff_digest(handoff: RecoveryHandoff) -> dict[str, int]:
    """Build the audit-event digest of a recovery handoff.

    Records only the per-field character lengths -- enough to confirm
    the structured handoff was non-empty without copying its full
    contents into the audit event a second time (the handoff renders in
    the next attempt's prompt, which already lives in the store via the
    persisted SDK messages).
    """
    return {
        "work_done_length": len(handoff.work_done),
        "work_remaining_length": len(handoff.work_remaining),
        "key_decisions_length": len(handoff.key_decisions),
        "suggested_next_step_length": len(handoff.suggested_next_step),
    }


def _cumulative_prior_outputs(
    store: HarnessStore, run_id: str
) -> str:
    """Concatenate every prior attempt's final transcript for the
    summarizer's ``cumulative_diff`` argument.

    The summarizer needs the work-so-far context; prior attempts'
    ``agent_output`` fields are the most-canonical work-so-far record
    the store carries. The current (about-to-be-finalized-RECOVERED)
    attempt's transcript is supplied separately as ``transcript_tail``.
    """
    prior = [
        a.agent_output
        for a in store.list_attempts(run_id)
        if a.agent_output
    ]
    return "\n\n---\n\n".join(prior)


async def _handle_context_recovery(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    telemetry: _RunTelemetry,
    config: HarnessConfig,
    attempt: Attempt,
    attempt_number: int,
    recovery_trigger: _RecoveryTrigger,
    recovery_state: _RecoveryState,
    clock: Callable[[], datetime],
) -> None:
    """Execute the summarize-restart action (spec 00018 FR-3 / spec 00019 FR-4).

    Drives the production-grade recovery sequence:

    1. Invoke the summarizer (via the config seam in tests, fresh SDK
       query in production) to produce a structured handoff.
    2. On summarizer failure, route through ``INTERNAL_ERROR`` -- mirrors
       the RubricJudgeError handling so the retry policy decides whether
       another attempt runs. Recovery does NOT silently restart with an
       empty handoff (spec Error Handling table).
    3. On success, emit ``harness.context_recovery`` BEFORE the next
       AttemptStarted (FR-5 / FR-6), finalize this attempt with
       ``Outcome.RECOVERED``, and walk the lifecycle
       ``running -> interrupted -> ready`` so the outer ``run_task``
       loop schedules the recovery attempt. The retry counter is not
       touched -- the recovery budget is independent of ``max_retries``
       (FR-4).

    The recovery payload carries ``trigger=recovery_trigger.trigger``
    so operators can tell a boundary recovery (spec 00018 -- iteration
    finished, occupancy at its tail crossed the ratio) apart from a
    mid-turn recovery (spec 00019 -- the iteration was interrupted in
    flight because the live SDK reading crossed the ratio). The shared
    ``recoveries_used`` counter decrements identically in either case
    so a single ``max_context_recoveries`` budget covers both paths
    (spec 00019 FR-5).
    """
    cumulative_diff = _cumulative_prior_outputs(store, lifecycle.run_id)
    worktree: str = (
        str(config.worktree) if config.worktree is not None else ""
    )
    try:
        handoff = await run_recovery_summarizer(
            task,
            transcript_tail=recovery_trigger.transcript_tail,
            cumulative_diff=cumulative_diff,
            worktree=worktree,
            summarizer_invoke=config.recovery_summarizer_invoke,
        )
    except RecoverySummarizerError as exc:
        # Mirrors RubricJudgeError routing -- the summarizer is the
        # same class of infrastructure as the rubric judge (a separate
        # LLM call distinct from the working agent), so an
        # invoke-raise / parse-failure is classified the same way.
        # Spec 00019 Error Handling: mid-turn and boundary recovery
        # share this routing so a failed summarizer never restarts
        # with an empty handoff regardless of which path triggered.
        error = f"recovery summarizer failed: {exc.reason}"
        telemetry.emit(
            kind="harness.crash",
            payload={
                "classification": "recovery_summarizer_error",
                "reason": exc.reason,
                "message": error,
                "trigger": recovery_trigger.trigger,
            },
            attempt_number=attempt_number,
        )
        _finalize_attempt(
            store=store,
            telemetry=telemetry,
            lifecycle=lifecycle,
            attempt=attempt,
            outcome=Outcome.INTERNAL_ERROR,
            error=error,
            agent_output=recovery_trigger.transcript_tail,
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

    # Reserve the budget slot before the audit event so the
    # ``recoveries_used`` / ``recoveries_remaining`` figures in the
    # event reflect the count *after* this recovery (the count an
    # operator sees when reading the audit stream matches the count
    # the in-process counter holds afterwards). Spec 00019 FR-5: the
    # boundary and mid-turn paths decrement the SAME counter so a run
    # cannot recover twice when ``max_context_recoveries=1`` no matter
    # which path produced each crossing.
    recovery_state.recoveries_used += 1
    recoveries_used = recovery_state.recoveries_used
    recoveries_remaining = max(
        0, config.max_context_recoveries - recoveries_used
    )

    # FR-5 / FR-6 ordering: the recovery audit event precedes the
    # recovery attempt's AttemptStarted. We emit it before finalizing
    # the prior attempt for the same reason -- the attempt_finalized
    # event is the cleanest "prior attempt closed" marker, but the
    # spec frames context_recovery as the recovery signal itself, so
    # emit it first and the finalize / transitions follow.
    telemetry.emit(
        kind="harness.context_recovery",
        payload={
            "iteration": recovery_trigger.iteration_number,
            "attempt_number": attempt_number,
            "occupancy_tokens": recovery_trigger.occupancy_tokens,
            "context_window_tokens": config.context_window_tokens,
            "context_recovery_trigger_ratio": (
                config.context_recovery_trigger_ratio
            ),
            "recoveries_used": recoveries_used,
            "recoveries_remaining": recoveries_remaining,
            "summary_digest": _handoff_digest(handoff),
            "trigger": recovery_trigger.trigger,
        },
        attempt_number=attempt_number,
    )
    _finalize_attempt(
        store=store,
        telemetry=telemetry,
        lifecycle=lifecycle,
        attempt=attempt,
        outcome=Outcome.RECOVERED,
        error="",
        agent_output=recovery_trigger.transcript_tail,
        clock=clock,
    )
    # Park the lifecycle in INTERRUPTED briefly to walk the legal edge
    # set back to READY without consuming a validation retry (RUNNING
    # has no direct edge to READY). The reducer clears
    # ``blocked_requires_json`` on the -> READY edge; we never set it
    # for recovery, so the clear is a harmless no-op.
    _transition(lifecycle, Status.INTERRUPTED, store=store, now=clock)
    _transition(lifecycle, Status.READY, store=store, now=clock)
    # Thread the handoff into the next attempt; _drive_iterations
    # consumes and clears this slot before the next iteration prompt
    # builds.
    recovery_state.pending_handoff = handoff


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
    telemetry: _RunTelemetry,
    attempt_number: int,
    iteration_number: int,
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
        context_observer=request.context_observer,
        recovery_interrupt_event=request.recovery_interrupt_event,
        on_command_applied=request.on_command_applied,
        checkpoint_nudge_seconds=request.checkpoint_nudge_seconds,
        agent_iteration_ceiling_seconds=(
            request.agent_iteration_ceiling_seconds
        ),
        checkpoint_progress_probe=request.checkpoint_progress_probe,
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
                telemetry.emit(
                    kind="harness.hang_detected",
                    payload={
                        "iteration": iteration_number,
                        "hang_timeout_seconds": hang_timeout,
                        "silence_seconds": silence,
                    },
                    attempt_number=attempt_number,
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


def _override_ceiling(
    task_override: float | None, resolved_default: float | None
) -> float | None:
    """Per-task budget precedence over a policy-resolved deadline class.

    ``None`` inherits the resolved class ceiling; ``0`` is the task's
    explicit unbounded opt-out (mirroring the ``[deadlines]`` semantics);
    any positive value replaces the ceiling for this task only.
    """
    if task_override is None:
        return resolved_default
    return float(task_override) if task_override > 0 else None


async def _drive_iterations(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    telemetry: _RunTelemetry,
    config: HarnessConfig,
    invoker: InvokeFunc,
    clock: Callable[[], datetime],
    mclock: Callable[[], float],
    attempt: Attempt,
    transcript_graders: tuple[TranscriptGrader, ...],
    loop_guard: LoopGuard,
    recovery_state: _RecoveryState,
) -> tuple[
    IterationResult | None,
    int,
    float,
    LoopGuardVerdict | None,
    _RecoveryTrigger | None,
    bool,
    datetime | None,
]:
    """Run iterations until a non-``continue`` envelope, crash, or cap.

    Returns the last :class:`IterationResult` produced, the number of
    iterations that ran, the wall-seconds elapsed across them, any
    :class:`LoopGuardVerdict` that tripped the safety net (FR-1 STUCK /
    FR-2 THRASH), and any :class:`_RecoveryTrigger` produced when an
    iteration crosses the context-recovery threshold (spec 00018 FR-1).
    The verdict is non-``None`` only when ``loop_guard`` observed a
    repeating-tool pattern; the recovery trigger is non-``None`` only
    when a validly-continuing iteration's input-side occupancy crosses
    the configured ratio with recovery budget remaining. At most one of
    ``loop_guard_verdict`` / ``recovery_trigger`` is set per
    ``_drive_iterations`` call -- loop-guard precedence (FR-6) ensures a
    verdict short-circuits the recovery check. The wall-seconds value is
    what feeds the :class:`TranscriptObservation` so transcript graders
    see the same elapsed time regardless of which iteration sets the
    envelope.

    The ``transient_exhausted`` tuple element is ``True`` when a rate-limited
    iteration was re-invoked ``config.max_transient_retries`` times (on the
    separate transient budget, backing off between tries) and still returned a
    transient fault, OR when the session-limit fast-abort fired (see below).
    Either way the caller finalizes the attempt as a TRANSIENT-classified
    ``INTERNAL_ERROR`` rather than an agent protocol error. It is ``False`` on
    every other path.

    The final tuple element is ``session_limit_reset``: an aware-UTC
    :class:`~datetime.datetime` when a rate-limit refusal named a reset that is
    derivable AND still in the future -- the fast-abort. In that case the
    iteration is NOT re-invoked on the transient budget (exactly one
    invocation happened); ``transient_exhausted`` is set so the same
    INTERNAL_ERROR route finalizes the attempt, and a ``harness.session_limit``
    event carries the reset and its derivation source. It is ``None`` when no
    future reset was derivable (a ``None`` or already-past reset preserves the
    existing transient-retry behavior exactly).
    """
    iteration_result: IterationResult | None = None
    iteration_number = 0
    started_monotonic = mclock()
    wall_seconds = 0.0
    loop_guard_verdict: LoopGuardVerdict | None = None
    recovery_trigger: _RecoveryTrigger | None = None
    transient_exhausted = False
    session_limit_reset: datetime | None = None
    attempt_number = attempt.number

    def _record_steering(command: ControlCommandRecord) -> None:
        """Ledger an applied control command, then delete its queue row.

        Spec 00025 FR-10: invoked by the live invoker's watcher after a
        command was successfully dispatched against the SDK session. The
        watcher coroutine interleaves with this loop on one event loop
        (the harness is parked awaiting the iteration task while it
        runs), and ``_append`` reconciles ``lifecycle`` in place, so the
        version key stays consistent for the harness's next append. The
        failure semantics live in :func:`_ledger_steering` — never
        raises back into the watcher.
        """
        _ledger_steering(
            lifecycle,
            store,
            command,
            attempt_number=attempt_number,
            clock=clock,
        )

    prior_rubric_findings = _collect_prior_rubric_findings(
        store, lifecycle.run_id, attempt_number
    )
    prior_manual_findings = _collect_prior_manual_findings(
        store, lifecycle.run_id, attempt_number
    )
    # Consume any pending handoff exactly once -- the recovery attempt
    # gets it on its first iteration prompt and subsequent attempts on
    # the same run do not re-render the same section. Recovery on a
    # later attempt overwrites this slot before the next call.
    pending_handoff = recovery_state.pending_handoff
    recovery_state.pending_handoff = None

    while iteration_number < config.max_iterations_per_attempt:
        iteration_number += 1

        prompt = build_iteration_prompt(
            task,
            lifecycle,
            IterationInputs(
                max_retries=config.max_retries,
                prior_rubric_findings=prior_rubric_findings,
                prior_manual_findings=prior_manual_findings,
                recovery_handoff=(
                    pending_handoff if iteration_number == 1 else None
                ),
            ),
        )

        captured_iteration = iteration_number

        # Mid-turn occupancy state (spec 00019 FR-1 / FR-3 / FR-4). All
        # slots are per-iteration: ``emitted_tiers`` enforces "at most
        # once per tier", ``latest_sdk_reading`` holds the most recent
        # ``ClaudeSDKClient.get_context_usage`` payload (when the
        # invoker pumps one through ``context_observer``),
        # ``accumulated_estimate`` is the always-available fallback
        # derived from streamed ``AssistantMessage.usage``,
        # ``midturn_armed`` records whether the act-ratio crossing
        # already armed the recovery event so the event is set at most
        # once per iteration, ``midturn_occupancy`` captures the
        # occupancy at the moment of arming so the audit payload
        # reports the exact reading that triggered the interrupt, and
        # ``partial_transcript_chunks`` accumulates streamed
        # AssistantMessage TextBlock content so a mid-turn-interrupted
        # iteration can hand the summarizer the work-so-far the agent
        # produced before the interrupt landed (the iteration itself
        # never returns a full transcript on the mid-turn path). Lists
        # wrap the mutable single-cell state so the nested closures can
        # rebind without ``nonlocal`` chains.
        emitted_tiers: list[float] = []
        latest_sdk_reading: list[ContextUsageResponse | None] = [None]
        accumulated_estimate: list[int] = [0]
        midturn_armed: list[bool] = [False]
        midturn_occupancy: list[int] = [0]
        partial_transcript_chunks: list[str] = []
        # Per-iteration recovery interrupt event (spec 00019 FR-4). The
        # event is wired into the invoker via ``InvocationRequest`` so a
        # live :class:`ClaudeSDKClient`-backed invoker can poll it from
        # its watcher loop and translate a set event into a
        # :class:`HarnessRecoveryRequested` cancel. Test invokers and
        # the plain ``query`` path do not poll the event, so mid-turn
        # *act* simply does not fire on those paths (spec FR-4
        # plain-path degradation) and the boundary recovery check
        # below still catches an over-ratio tail. Always-allocated so
        # the request's field type stays non-optional at the call
        # site; the event is consulted only when ``_check_context_thresholds``
        # decides to arm it.
        recovery_interrupt_event = asyncio.Event()

        def _check_context_thresholds() -> None:
            """Emit ``harness.context_threshold_crossed`` for newly-crossed
            tiers and arm mid-turn recovery when the act ratio is crossed.

            Hybrid capacity: the SDK reading's ``maxTokens`` wins when a
            reading is present and carries a positive value; otherwise
            the operator-supplied ``HarnessConfig.context_window_tokens``
            is the fallback. When neither yields a capacity the
            iteration is left fully inert (FR-2 off-by-default) -- no
            ratio is computed, no event fires, and no interrupt arms.

            Hybrid occupancy: the SDK reading's ``totalTokens`` wins
            when a reading is present (the live client knows the exact
            on-server context), with the accumulated
            ``AssistantMessage.usage`` estimate as the fallback when
            either no reading is available or its ``totalTokens`` is
            absent / non-integer.

            Telemetry emits are best-effort (spec 00025 FR-7): a faulty
            sink is recorded once on stderr by :class:`_RunTelemetry`
            and never interrupts the iteration.

            Mid-turn act (FR-4 / FR-5 / spec edge case "Crossing 90%
            observe and the act ratio on the same message"): after
            emitting observe events, if the same ratio also crosses
            ``context_recovery_trigger_ratio`` AND recovery budget
            remains AND the event was not already armed this iteration,
            set the recovery event so the invoker's watcher
            (when present) dispatches ``ClaudeSDKClient.interrupt`` and
            raises :class:`HarnessRecoveryRequested`. Arming after the
            tier emission keeps the observe event ordered before the
            recovery event in the audit stream (FR-6 ordering).
            Setting an already-set event is a no-op; the ``midturn_armed``
            flag guards against re-arming on a re-cross / oscillation
            and prevents double-firing for the same crossing.
            """
            reading = latest_sdk_reading[0]
            capacity: int | None = None
            source: str | None = None
            occupancy: int = accumulated_estimate[0]
            if reading is not None:
                sdk_capacity = reading.get("maxTokens")
                if (
                    isinstance(sdk_capacity, int)
                    and not isinstance(sdk_capacity, bool)
                    and sdk_capacity > 0
                ):
                    capacity = sdk_capacity
                    source = _CAPACITY_SOURCE_SDK
                sdk_occupancy = reading.get("totalTokens")
                if (
                    isinstance(sdk_occupancy, int)
                    and not isinstance(sdk_occupancy, bool)
                    and sdk_occupancy >= 0
                ):
                    occupancy = sdk_occupancy
            if capacity is None:
                if config.context_window_tokens is not None:
                    capacity = config.context_window_tokens
                    source = _CAPACITY_SOURCE_OPERATOR
                else:
                    # FR-2 off-by-default: no capacity from either
                    # source, so no ratio and no event.
                    return
            assert source is not None  # for type-checkers; set with capacity.
            if capacity <= 0:
                return
            ratio = occupancy / capacity
            for tier in _CONTEXT_OBSERVE_TIERS:
                if tier in emitted_tiers:
                    continue
                if ratio < tier:
                    continue
                emitted_tiers.append(tier)
                telemetry.emit(
                    kind="harness.context_threshold_crossed",
                    payload={
                        "iteration": captured_iteration,
                        "tier": tier,
                        "occupancy_tokens": occupancy,
                        "capacity_tokens": capacity,
                        "percentage": ratio * 100.0,
                        "capacity_source": source,
                    },
                    attempt_number=attempt_number,
                    iteration_number=captured_iteration,
                )
            # Mid-turn act gate (FR-4 / FR-5). Budget check uses the
            # same ``recoveries_used`` / ``max_context_recoveries``
            # counter the boundary recovery consumes, so the mid-turn
            # path cannot exceed the shared budget. When budget is
            # already exhausted the event is not armed: observe events
            # still fire (above) and the iteration runs to its natural
            # end (spec edge case).
            if (
                not midturn_armed[0]
                and ratio >= config.context_recovery_trigger_ratio
                and recovery_state.recoveries_used
                < config.max_context_recoveries
            ):
                midturn_armed[0] = True
                midturn_occupancy[0] = occupancy
                recovery_interrupt_event.set()

        def _on_message(msg: Message) -> None:
            from flywheel_core._sdk import AssistantMessage, TextBlock

            telemetry.sdk_message(
                msg,
                attempt_number=attempt_number,
                iteration_number=captured_iteration,
            )
            # Accumulate the work-so-far transcript from streamed
            # AssistantMessage text blocks (FR-4). The iteration's
            # full ``IterationResult.transcript`` covers the boundary
            # path; the mid-turn path interrupts the invoker before
            # ``invoke_iteration`` returns, so the summarizer would
            # otherwise see no work-so-far at all. Mirrors the
            # transcript-building algorithm in
            # :func:`flywheel_core.invoker.invoke_iteration`.
            if isinstance(msg, AssistantMessage):
                for block in msg.content:
                    if isinstance(block, TextBlock):
                        partial_transcript_chunks.append(block.text)
            # FR-1: update the fallback estimate from this streamed
            # message's usage. Each ``AssistantMessage.usage`` carries
            # the input-side context for THAT turn (input + cache_read +
            # cache_creation, per ``_occupancy_from_usage``), not a
            # delta -- the latest value supersedes prior estimates so
            # the running occupancy tracks the most recent model call
            # rather than double-counting the resent conversation
            # (mirrors the spec 00018 occupancy semantic).
            if isinstance(msg, AssistantMessage) and msg.usage is not None:
                accumulated_estimate[0] = _occupancy_from_usage(msg.usage)
                _check_context_thresholds()

        def _context_observer(reading: ContextUsageResponse) -> None:
            """Adopt the watcher's SDK reading and re-check thresholds.

            Invoked from ``invoke_iteration_with_client``'s watcher
            coroutine once per poll. Storing the reading wins the
            occupancy / capacity check against the
            ``AssistantMessage.usage`` estimate (FR-1 preference), so
            a tier first reached on the SDK path fires off the exact
            reading rather than the accumulated estimate.
            """
            latest_sdk_reading[0] = reading
            _check_context_thresholds()

        # Resolve the AGENT_ITERATION wall-clock ceiling once here so it feeds
        # BOTH the checkpoint-nudge seam on the request below AND the
        # ``run_with_deadline`` wrapper further down. Reading it via
        # ``deadlines.for_class`` keeps operator ``[deadlines]`` overrides
        # (phase 14) flowing through; ``None`` is the opted-out unbounded
        # case. The task's own ``budgets.agent_iteration_seconds`` takes
        # precedence over the class ceiling -- the heavyweight tail (a legit
        # golden-record run longer than the repo-wide default) declares its
        # budget per task instead of the operator unbounding every task.
        agent_ceiling = _override_ceiling(
            task.budgets.agent_iteration_seconds,
            config.deadlines.for_class(DeadlineClass.AGENT_ITERATION),
        )
        request = InvocationRequest(
            prompt=prompt,
            transcript_graders=transcript_graders,
            attempt_number=attempt_number,
            iteration_number=iteration_number,
            on_message=_on_message,
            context_observer=_context_observer,
            recovery_interrupt_event=recovery_interrupt_event,
            on_command_applied=_record_steering,
            # Checkpoint-nudge seam: the invoker gates on the probe being
            # present, the threshold positive, and the ceiling bounded, so
            # passing the raw values leaves the nudge dormant under the
            # default (``checkpoint_progress_probe is None``).
            checkpoint_nudge_seconds=config.checkpoint_nudge_seconds,
            agent_iteration_ceiling_seconds=agent_ceiling,
            checkpoint_progress_probe=config.checkpoint_progress_probe,
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

        async def _run_invocation() -> IterationResult:
            if hang_timeout is None or hang_timeout <= 0:
                return await invoker(request)
            return await _invoke_with_watchdog(
                invoker=invoker,
                request=request,
                hang_timeout=hang_timeout,
                mclock=mclock,
                telemetry=telemetry,
                attempt_number=attempt_number,
                iteration_number=iteration_number,
            )

        # Wall-clock agent-iteration deadline (spec 00066 criterion #2, D-2):
        # the resolved ceiling is finite and non-null by default (default-on),
        # so the bare unbounded ``await invoker(request)`` path is NOT
        # reachable under the default config. The bound is total elapsed wall
        # time since the invocation started -- additive to, not a replacement
        # for, the inter-message silence watchdog above (both run): an invoker
        # that never returns, even one steadily streaming output the silence
        # watchdog would never trip on, is cancelled once the ceiling passes.
        # ``run_with_deadline`` raises ``DeadlineExceeded`` on timeout; we
        # translate it into the harness-local ``_IterationDeadlineExceeded``
        # sentinel so the ``_run_attempt`` boundary routes it to the
        # timeout-classified internal_error containment path (mirroring the
        # hang watchdog), distinct from an operator-driven cancellation. An
        # operator opts out per class with a ``0`` override (resolves to
        # ``None``), restoring the unbounded await. ``agent_ceiling`` is
        # resolved once above (it also feeds the checkpoint-nudge seam).
        # Transient rate-limit retry: a 429 / overload / ``rejected``
        # rate-limit iteration is an infrastructure fault that produced no
        # usable completion. Rather than spend the validation retry budget on
        # it, the harness re-invokes the SAME iteration on the SEPARATE,
        # bounded ``config.max_transient_retries`` budget, backing off between
        # tries with the shared BackoffPolicy schedule. The loop is bounded by
        # that operator-supplied budget, never by an agent-supplied value, so
        # it cannot spin forever. On exhaustion ``transient_exhausted`` is set
        # and the last (still rate-limited) result falls through so the caller
        # finalizes the attempt as a TRANSIENT-classified INTERNAL_ERROR.
        transient_retries = 0
        recovery_requested = False
        while True:
            try:
                if agent_ceiling is None:
                    iteration_result = await _run_invocation()
                else:
                    try:
                        iteration_result = await run_with_deadline(
                            _run_invocation(), agent_ceiling
                        )
                    except DeadlineExceeded as exc:
                        telemetry.emit(
                            kind="harness.deadline_exceeded",
                            payload={
                                "iteration": iteration_number,
                                "deadline_class": (
                                    DeadlineClass.AGENT_ITERATION.value
                                ),
                                "ceiling_seconds": agent_ceiling,
                                "elapsed_seconds": exc.elapsed_seconds,
                            },
                            attempt_number=attempt_number,
                        )
                        raise _IterationDeadlineExceeded(
                            attempt_number=attempt_number,
                            iteration_number=iteration_number,
                            ceiling_seconds=agent_ceiling,
                            elapsed_seconds=exc.elapsed_seconds,
                        ) from exc
            except HarnessRecoveryRequested:
                # Spec 00019 FR-4 / FR-7: the live-client invoker raised
                # the distinguishable mid-turn recovery signal in response
                # to ``recovery_interrupt_event`` being set above. Capture
                # the work-so-far transcript and the occupancy that armed
                # the interrupt into a mid_turn ``_RecoveryTrigger`` and
                # break the iteration loop -- _run_attempt_body routes
                # this through :func:`_handle_context_recovery` which
                # finalizes the attempt RECOVERED and emits
                # ``harness.context_recovery`` with ``trigger="mid_turn"``.
                recovery_trigger = _RecoveryTrigger(
                    occupancy_tokens=midturn_occupancy[0],
                    transcript_tail="".join(partial_transcript_chunks),
                    iteration_number=iteration_number,
                    trigger=_RECOVERY_TRIGGER_MID_TURN,
                )
                iteration_result = None
                # Update wall_seconds for parity with the normal-completion
                # path even though the mid-turn route does not invoke
                # transcript graders -- keeps the return shape uniform.
                wall_seconds = mclock() - started_monotonic
                recovery_requested = True
                break

            transient_reason = _iteration_is_transient_rate_limit(
                iteration_result
            )
            # Session-limit fast-abort: a rate-limit refusal whose reset time
            # is derivable AND still in the future. The reset comes from a
            # rejected rate-limit event or a parseable refusal in the
            # transcript; either way it is a hard block until that instant, so
            # re-invoking on the transient budget would only burn invocations.
            # Finalize after THIS one invocation through the same TRANSIENT
            # INTERNAL_ERROR route, carrying the reset up structurally. A
            # ValidEnvelope is never session-limited (the agent produced
            # actionable output), mirroring _iteration_is_transient_rate_limit's
            # guard. A ``None`` reset OR one at-or-before now falls through to
            # today's transient-retry behavior unchanged. Checked BEFORE the
            # ``transient_reason is None`` break so a pure refusal-text block
            # with no structural rate-limit signal still fast-aborts.
            session_now = clock()
            derived = (
                None
                if isinstance(iteration_result.envelope, ValidEnvelope)
                else derive_session_limit_reset(
                    rate_limit_events=iteration_result.signals.rate_limit_events,
                    transcript=iteration_result.transcript,
                    now=session_now,
                )
            )
            if derived is not None and derived.reset_at > session_now:
                session_limit_reset = derived.reset_at
                transient_exhausted = True
                telemetry.emit(
                    kind="harness.session_limit",
                    payload={
                        "iteration": iteration_number,
                        "reason": transient_reason or "session_limit_refusal",
                        "reset_at": derived.reset_at.isoformat(),
                        "source": derived.source,
                        "classification": FaultClass.TRANSIENT.value,
                    },
                    attempt_number=attempt_number,
                    iteration_number=iteration_number,
                )
                break
            if transient_reason is None:
                break
            if transient_retries >= config.max_transient_retries:
                # Budget exhausted: hand the rate-limited result back marked
                # transient so the caller routes it to INTERNAL_ERROR, not the
                # agent-protocol AGENT_ERROR path.
                transient_exhausted = True
                telemetry.emit(
                    kind="harness.transient_exhausted",
                    payload={
                        "iteration": iteration_number,
                        "reason": transient_reason,
                        "invocations": transient_retries + 1,
                        "max_transient_retries": config.max_transient_retries,
                        "classification": FaultClass.TRANSIENT.value,
                    },
                    attempt_number=attempt_number,
                    iteration_number=iteration_number,
                )
                break
            delay = config.transient_backoff.delay_for(transient_retries)
            transient_retries += 1
            telemetry.emit(
                kind="harness.transient_retry",
                payload={
                    "iteration": iteration_number,
                    "reason": transient_reason,
                    "retry": transient_retries,
                    "max_transient_retries": config.max_transient_retries,
                    "delay_seconds": delay,
                    "classification": FaultClass.TRANSIENT.value,
                },
                attempt_number=attempt_number,
                iteration_number=iteration_number,
            )
            await config.transient_sleep(delay)

        if recovery_requested:
            break
        # Only the recovery path leaves iteration_result None (and it broke
        # the outer loop above); every other exit of the transient loop
        # assigned an IterationResult.
        assert iteration_result is not None
        wall_seconds = mclock() - started_monotonic

        # Context-pressure telemetry: token fields are per-iteration deltas
        # — consumers cumulate by summing the audit stream; the harness
        # holds no running counter. ``total_cost_usd`` and ``num_turns`` are
        # forwarded verbatim from the SDK's ResultMessage and are
        # session-cumulative as the SDK reports them; do NOT delta them.
        # Token breakdown: prefer an explicit plain-dict ``usage`` (a
        # message-less invoker, e.g. the container backend driving the agent
        # CLI in stream-json mode — spec 00044) and otherwise derive it from
        # the SDK messages exactly as before. Both feed the same rollup, the
        # context-pressure telemetry, and the per-run token ceiling (00042).
        if iteration_result.usage is not None:
            usage_breakdown = {
                key: int(iteration_result.usage.get(key, 0))
                for key in _USAGE_TOKEN_KEYS
            }
        else:
            usage_breakdown = _build_usage_breakdown(iteration_result.messages)
        usage_payload: dict[str, Any] = dict(usage_breakdown)
        usage_payload["total_tokens"] = total_tokens_from_usage(usage_breakdown)
        telemetry.emit(
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
        )

        # Iteration-boundary aggregate rollup (spec 00025 FR-6): fold this
        # iteration's deltas into the attempt row's cumulative counters and
        # persist through the versioned save_attempt path. Token fields are
        # per-iteration deltas; turns / total_cost_usd are SDK
        # session-cumulative readings summed at the boundary (same
        # overcount policy as the telemetry stream). The lifecycle version
        # is stable across the iteration loop (only telemetry was written
        # since the AttemptStarted append), so a mismatch means another
        # writer moved the run on and this worker must not clobber it. A
        # failed rollup is a relational-write failure and propagates to
        # the crash path, the same severity as the domain-event writes.
        attempt.input_tokens += usage_breakdown.get("input_tokens", 0)
        attempt.output_tokens += usage_breakdown.get("output_tokens", 0)
        attempt.cache_creation_input_tokens += usage_breakdown.get(
            "cache_creation_input_tokens", 0
        )
        attempt.cache_read_input_tokens += usage_breakdown.get(
            "cache_read_input_tokens", 0
        )
        attempt.iterations_completed = iteration_number
        num_turns = iteration_result.signals.num_turns
        if num_turns is not None:
            attempt.turns += num_turns
        cost = iteration_result.signals.total_cost_usd
        if cost is not None:
            attempt.total_cost_usd += cost
        attempt.last_activity_at = clock()
        store.save_attempt(
            lifecycle.run_id, attempt, expected_version=lifecycle.version
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

        # Context-recovery trigger (spec 00018 FR-1). Evaluated AFTER
        # the loop-guard verdict check so loop-guard precedence (FR-6)
        # is enforced: a STUCK / THRASH verdict halts the run without
        # recovery. The trigger fires only for a validly-continuing
        # iteration (intent=continue) with recovery budget remaining
        # whose input-side occupancy crosses the operator-supplied
        # ratio. A completion claim (intent=verify / abort / blocked)
        # does NOT recover -- those iterations break to the caller's
        # classification arm below.
        if (
            config.context_window_tokens is not None
            and recovery_state.recoveries_used
            < config.max_context_recoveries
            and isinstance(iteration_result.envelope, ValidEnvelope)
            and iteration_result.envelope.intent == Intent.CONTINUE
        ):
            occupancy = _occupancy_from_usage(usage_payload)
            threshold = (
                config.context_window_tokens
                * config.context_recovery_trigger_ratio
            )
            if occupancy >= threshold:
                recovery_trigger = _RecoveryTrigger(
                    occupancy_tokens=occupancy,
                    transcript_tail=iteration_result.transcript,
                    iteration_number=iteration_number,
                    trigger=_RECOVERY_TRIGGER_BOUNDARY,
                )
                break

        if (
            isinstance(iteration_result.envelope, ValidEnvelope)
            and iteration_result.envelope.intent == Intent.CONTINUE
        ):
            continue
        break

    return (
        iteration_result,
        iteration_number,
        wall_seconds,
        loop_guard_verdict,
        recovery_trigger,
        transient_exhausted,
        session_limit_reset,
    )


async def _validate(
    *,
    task: Task,
    lifecycle: Lifecycle,
    store: HarnessStore,
    telemetry: _RunTelemetry,
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
    - Command grader whose subprocess failed to start ->
      ``INTERNAL_ERROR`` outcome + ``INTERNAL_ERROR`` lifecycle status
      (the retryable infra class) via a ``harness.crash`` event whose
      ``classification`` is ``grader_spawn_failure``.
    - Rubric fail with ``retry_on_fail=True`` -> ``FAILED_VALIDATION``.
    - Rubric fail with ``retry_on_fail=False`` -> ``INTERRUPTED``.
    - Judge infra failure (``RubricJudgeError``) ->
      ``INTERNAL_ERROR`` outcome + ``INTERRUPTED`` lifecycle status
      via a ``harness.crash`` event whose ``classification`` is
      ``rubric_judge_error``.
    """
    # Wall-clock per-grader deadline (spec 00066 criterion #4): resolve the
    # default-on COMMAND_GRADER ceiling from config and pass it through so a
    # grader command that hangs is SIGKILLed and recorded with
    # ``payload['termination'] == 'timeout'`` instead of blocking validation
    # forever. ``for_class`` returns ``None`` only when an operator opts the
    # class out (``0``/unbounded override), which restores the unbounded wait.
    command_grader_ceiling = config.deadlines.for_class(
        DeadlineClass.COMMAND_GRADER
    )
    command_results = run_command_graders(
        task,
        store,
        run_id=lifecycle.run_id,
        attempt_number=attempt.number,
        cwd=config.worktree,
        env=config.grader_env,
        per_grader_timeout_seconds=command_grader_ceiling,
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

        def _on_judge_retry(
            grader_name: str, judge_attempt: int, reason: str
        ) -> None:
            # A judge-infra failure absorbed by the in-place retry envelope:
            # the judge alone re-runs, the implementation attempt survives.
            # Emitted per absorbed failure so a flaky judge is diagnosable
            # from telemetry without ever reaching the INTERNAL_ERROR path.
            telemetry.emit(
                kind="harness.rubric_judge_retry",
                payload={
                    "grader_name": grader_name,
                    "judge_attempt": judge_attempt,
                    "reason": reason,
                },
                attempt_number=attempt.number,
            )

        try:
            rubric_results = await run_rubric_graders(
                task,
                store,
                run_id=lifecycle.run_id,
                attempt_number=attempt.number,
                transcript=agent_output,
                worktree=(
                    str(config.worktree)
                    if config.worktree is not None
                    else None
                ),
                command_passed=command_passed,
                transcript_passed=transcript_passed,
                judge_invoke=config.rubric_judge_invoke,
                judge_model=config.rubric_judge_model,
                # Per-task budget overrides (the heavyweight tail) beat the
                # config/policy values; None inherits, 0 seconds = unbounded.
                judge_max_turns=(
                    task.budgets.rubric_judge_max_turns
                    if task.budgets.rubric_judge_max_turns is not None
                    else config.rubric_judge_max_turns
                ),
                judge_ceiling_seconds=_override_ceiling(
                    task.budgets.rubric_judge_seconds,
                    config.deadlines.for_class(DeadlineClass.RUBRIC_JUDGE),
                ),
                judge_retries=config.rubric_judge_retries,
                on_judge_retry=_on_judge_retry,
                now=clock,
            )
        except RubricJudgeError as exc:
            error = (
                f"rubric judge failed: {exc.grader_name}: {exc.reason}"
            )
            telemetry.emit(
                kind="harness.crash",
                payload={
                    "classification": "rubric_judge_error",
                    "grader_name": exc.grader_name,
                    "reason": exc.reason,
                    "message": error,
                },
                attempt_number=attempt.number,
            )
            _finalize_attempt(
                store=store,
                telemetry=telemetry,
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
                telemetry,
                attempt_number=attempt.number,
                record=record,
            )
        rubric_passed = all(r.passed for r in rubric_results)

    if command_passed and transcript_passed and rubric_passed:
        # When the task declares any ManualGrader, the attempt has passed
        # every automated grader but still owes a human decision. Finalize
        # the attempt SUCCEEDED (the agent's clock measures the agent, not
        # the unbounded human wait — see the SUCCEEDED-semantics NFR in
        # spec 00016) and park the lifecycle on the first manual gate
        # instead of promoting straight to DONE. The byte-identical
        # ``-> DONE`` path is preserved below for the zero-manual-gate
        # case.
        first_gate = next_pending_manual_gate(task, after_ordinal=None)
        if first_gate is not None:
            _finalize_attempt(
                store=store,
                telemetry=telemetry,
                lifecycle=lifecycle,
                attempt=attempt,
                outcome=Outcome.SUCCEEDED,
                error="",
                agent_output=agent_output,
                clock=clock,
            )
            _enter_manual_gate(
                store=store,
                telemetry=telemetry,
                lifecycle=lifecycle,
                attempt=attempt,
                gate=first_gate,
                attempt_dir=attempt_dir,
                clock=clock,
            )
            return
        # Landable-change gate (spec 00061): the run has passed every
        # automated grader and would land as DONE. Consult the optional
        # git-free consumer hook FIRST. A non-empty reason means the change
        # is not landable (empty/uncommitted under a git landing strategy);
        # do not land it as a success — finalize VALIDATION_FAILED and route
        # to FAILED_VALIDATION so the existing max_retries machinery re-drives
        # the task against the same base, ending terminal FAILED (never DONE)
        # once the budget is exhausted. ``None``/unset is byte-identical to
        # the historical direct ``-> DONE`` path below.
        landability_reason = (
            config.landability_gate() if config.landability_gate else None
        )
        if landability_reason:
            error = f"change not landable: {landability_reason}"
            telemetry.emit(
                kind="harness.landability_gate_blocked",
                payload={"reason": landability_reason, "message": error},
                attempt_number=attempt.number,
            )
            _finalize_attempt(
                store=store,
                telemetry=telemetry,
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
        _finalize_attempt(
            store=store,
            telemetry=telemetry,
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
            telemetry.emit(
                kind="harness.crash",
                payload={
                    "classification": "grader_signaled",
                    "signal": signal_no,
                    "grader_name": signaled.grader_name,
                    "grader_ordinal": signaled.ordinal,
                    "message": error,
                },
                attempt_number=attempt.number,
            )
            _finalize_attempt(
                store=store,
                telemetry=telemetry,
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
        spawn_failed = _spawn_failed_grader(command_results)
        if spawn_failed is not None:
            grader_label = spawn_failed.grader_name or spawn_failed.grader_type
            spawn_error = str(spawn_failed.payload.get("spawn_error", ""))
            error = (
                f"command grader {grader_label!r} failed to start: "
                f"{spawn_error}"
            )
            telemetry.emit(
                kind="harness.crash",
                payload={
                    "classification": "grader_spawn_failure",
                    "grader_name": spawn_failed.grader_name,
                    "grader_ordinal": spawn_failed.ordinal,
                    "message": error,
                },
                attempt_number=attempt.number,
            )
            _finalize_attempt(
                store=store,
                telemetry=telemetry,
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
        error = _grader_failure_error(command_results)
        _finalize_attempt(
            store=store,
            telemetry=telemetry,
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
            telemetry=telemetry,
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
        telemetry=telemetry,
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


def _enter_manual_gate(
    *,
    store: HarnessStore,
    telemetry: _RunTelemetry,
    lifecycle: Lifecycle,
    attempt: Attempt,
    gate: ManualGate,
    attempt_dir: Path | None,
    clock: Callable[[], datetime],
) -> None:
    """Park a lifecycle on a manual-grader gate after automated graders pass.

    Persists the awaiting gate ordinal via an ``AwaitingApproval`` domain
    event (so the column survives event-replay parity) and transitions
    ``VALIDATING -> AWAITING_APPROVAL``. Then emits the audit-stream
    ``harness.awaiting_approval`` event so operators (and the live surface)
    learn a decision is owed. The attempt is expected to have been
    finalized ``SUCCEEDED`` immediately before this call; the human wait
    is a lifecycle-level gate that does not extend attempt duration.
    """
    _append(
        lifecycle,
        AwaitingApproval(
            run_id=lifecycle.run_id,
            ts=clock(),
            attempt_number=attempt.number,
            awaiting_ordinal=gate.ordinal,
        ),
        store=store,
    )
    _transition(
        lifecycle,
        Status.AWAITING_APPROVAL,
        store=store,
        now=clock,
    )
    artifacts_dir = str(attempt_dir) if attempt_dir is not None else ""
    telemetry.emit(
        kind="harness.awaiting_approval",
        payload={
            "instructions": gate.instruction,
            "awaiting_ordinal": gate.ordinal,
            "grader_name": gate.grader_name,
            "run_id": lifecycle.run_id,
            "attempt_number": attempt.number,
            "artifacts_dir": artifacts_dir,
        },
        attempt_number=attempt.number,
    )


def _emit_rubric_events(
    telemetry: _RunTelemetry,
    *,
    attempt_number: int,
    record: GraderResultRecord,
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
    telemetry.emit(
        kind="harness.rubric_invoked",
        payload={
            "grader_name": record.grader_name,
            "judge_model": judge_model,
            "attempt_number": record.attempt_number,
        },
        attempt_number=attempt_number,
    )
    telemetry.emit(
        kind="harness.rubric_verdict",
        payload={
            "grader_name": record.grader_name,
            "passed": record.passed,
            "summary": summary,
            "unknown": unknown,
        },
        attempt_number=attempt_number,
    )
    if unknown:
        telemetry.emit(
            kind="harness.rubric_unknown",
            payload={
                "grader_name": record.grader_name,
                "summary": summary,
            },
            attempt_number=attempt_number,
        )


# The stable prefix every per-run budget-ceiling breach error carries (specs
# 00039/00042). A budget kill and an ``intent=abort`` both reach a terminal
# ``Status.FAILED`` with an ``Outcome.AGENT_ERROR`` final attempt, so this prefix
# on the attempt's ``error`` is the durable, control-store-readable discriminator
# the work re-driver (spec 00069) uses to route a breach to the human-review
# queue with ``budget-ceiling`` rather than ``abort``. Keep it in lockstep with
# ``_finalize_budget_breach`` below.
BUDGET_CEILING_ERROR_PREFIX: str = "budget ceiling breached:"


def _finalize_budget_breach(
    *,
    store: HarnessStore,
    telemetry: _RunTelemetry,
    lifecycle: Lifecycle,
    attempt: Attempt,
    attempt_number: int,
    ceiling: str,
    limit: float,
    observed: float,
    transcript: str,
    clock: Callable[[], datetime],
) -> None:
    """End a run that breached a per-run budget ceiling (specs 00039/00042).

    Shared by the cost, token, and wall-clock guards: finalize the current
    attempt ``Outcome.AGENT_ERROR`` (reusing the ABORT shape), emit the
    distinct ``harness.budget_ceiling_breached`` event so audit tells a
    budget kill from an agent error, then transition RUNNING -> FAILED
    directly (terminal, non-retryable). ``ceiling`` names the dimension
    (``cost_usd`` / ``tokens`` / ``wall_clock_seconds``).
    """
    breach_error = (
        f"{BUDGET_CEILING_ERROR_PREFIX} {ceiling} {observed} >= limit {limit}"
    )
    _finalize_attempt(
        store=store,
        telemetry=telemetry,
        lifecycle=lifecycle,
        attempt=attempt,
        outcome=Outcome.AGENT_ERROR,
        error=breach_error,
        agent_output=transcript,
        clock=clock,
    )
    telemetry.emit(
        kind="harness.budget_ceiling_breached",
        payload={"ceiling": ceiling, "limit": limit, "observed": observed},
        attempt_number=attempt_number,
    )
    _transition(
        lifecycle,
        Status.FAILED,
        store=store,
        error=breach_error,
        now=clock,
    )


def _finalize_attempt(
    *,
    store: HarnessStore,
    telemetry: _RunTelemetry,
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
    telemetry.emit(
        kind="harness.attempt_finalized",
        payload={
            "number": attempt.number,
            "outcome": outcome.value,
            "error": error,
        },
        attempt_number=attempt.number,
    )


def finalize_stranded_lifecycle(
    store: HarnessStore,
    run_id: str,
    *,
    reason: str = "worker interrupted before finalization",
    classification: str = "worker_interrupted",
    sink: TelemetrySink | None = None,
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
    :attr:`Status.AWAITING_APPROVAL` is one such quiescent status (spec
    00016 FR-9): the attempt was finalized ``SUCCEEDED`` at gate entry
    per FR-4, so the open-attempt strand rule is unaffected; only the
    parked status needs exempting so the manual gate survives worker
    restart untouched.
    """
    clock = now or _utcnow
    telemetry = _RunTelemetry(sink, run_id=run_id, clock=clock)
    store = _MirroringStore(store, telemetry)
    lifecycle = store.load_lifecycle(run_id)
    if lifecycle is None:
        return False
    # AWAITING_APPROVAL is a durable park, not a stranded mid-attempt;
    # explicitly absent from the stranded set (alongside terminal /
    # interrupted / retry-source statuses) so a parked manual gate
    # survives a worker restart untouched.
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
        telemetry.emit(
            kind="harness.attempt_finalized",
            payload={
                "number": attempt.number,
                "outcome": Outcome.INTERNAL_ERROR.value,
                "error": reason,
            },
            attempt_number=attempt.number,
        )

    telemetry.emit(
        kind="harness.crash",
        payload={
            "classification": classification,
            "message": reason,
            "from_status": lifecycle.status.value,
        },
        attempt_number=last_attempt_number,
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
    :func:`flywheel_core.grader_command.run_command_graders` because that path
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
    sink: TelemetrySink | None = None,
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
    :func:`flywheel_core.grader_command.run_command_graders`: the latter
    persists a ``grader_results`` row, but recheck's audit surface is the
    event payload only (FR-5 Out of Scope).

    Concurrent callers race on ``lifecycle.version`` exactly as the rest
    of the harness does; the loser surfaces
    :class:`flywheel_core.store_protocols.OptimisticConcurrencyError` to its
    caller — this function does not swallow it.
    """
    clock = now or _utcnow
    telemetry = _RunTelemetry(sink, run_id=run_id, clock=clock)
    store = _MirroringStore(store, telemetry)
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
        telemetry.emit(
            kind="harness.recheck_attempted",
            payload={
                "per_predicate": [dict(p) for p in per_predicate],
                "all_satisfied": False,
                "dry_run": dry_run,
            },
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

    telemetry.emit(
        kind="harness.recheck_attempted",
        payload={
            "per_predicate": [dict(p) for p in evaluated],
            "all_satisfied": all_satisfied,
            "dry_run": dry_run,
        },
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
    telemetry.emit(
        kind="harness.unblocked",
        payload={
            "from_status": Status.INTERRUPTED.value,
            "to_status": Status.READY.value,
        },
    )
    return RecheckOutcome(
        applied=True,
        reason="unblocked",
        per_predicate=per_predicate_out,
    )


@dataclass(frozen=True, kw_only=True)
class ResolveApprovalOutcome:
    """Return value of :func:`resolve_manual_approval`.

    ``applied`` is ``True`` only when an ``approve`` / ``reject`` command
    was claimed for the parked gate and an effect landed on the
    lifecycle. ``reason`` is a short stable token for programmatic
    consumers:

    * ``"not_awaiting"`` -- lifecycle missing, not in
      :attr:`Status.AWAITING_APPROVAL`, or missing an
      ``awaiting_manual_ordinal``.
    * ``"missing_gate"`` -- defensive: the persisted ordinal does not
      resolve to a :class:`ManualGrader` on ``task.graders`` (task drift
      between the parked run and the resolver call).
    * ``"no_attempt"`` -- defensive: an ``AWAITING_APPROVAL`` lifecycle
      with no finalized attempt; the resolver cannot key a manual
      receipt without one.
    * ``"no_pending_command"`` -- the claim batch carried no
      ``approve`` / ``reject`` row; the lifecycle stays parked.
    * ``"approved_done"`` -- approve landed on the last gate;
      lifecycle reached :attr:`Status.DONE`.
    * ``"approved_next_gate"`` -- approve landed on a non-final gate;
      lifecycle re-parked on the next gate's ordinal.
    * ``"rejected_retry"`` -- reject landed with retries remaining;
      lifecycle transitioned ``AWAITING_APPROVAL -> FAILED_VALIDATION
      -> READY`` (consuming one retry on the second edge).
    * ``"rejected_failed"`` -- reject landed with retries exhausted;
      lifecycle reached :attr:`Status.FAILED` retaining the rejection
      error.

    ``command_id`` is the id of the applied control command when
    ``applied`` is ``True``; ``None`` otherwise.
    """

    applied: bool
    reason: str
    command_id: int | None = None


def _reject_feedback_text(payload: Mapping[str, Any]) -> str:
    """Coerce a ``reject`` payload's optional ``feedback`` field for the
    manual receipt summary.

    Producer-side validation already constrains ``feedback`` to a string
    when present (see ``flywheel_core.invoker_client._payload_feedback``), so
    the resolver only needs to substitute the documented placeholder for
    the absent / empty cases per the spec error-handling table.
    """
    raw = payload.get("feedback")
    if isinstance(raw, str) and raw:
        return raw
    return "(no feedback provided)"


def _emit_control_applied(
    telemetry: _RunTelemetry,
    command: ControlCommandRecord,
    *,
    attempt_number: int | None,
) -> None:
    """Record the resolver's claim via the existing
    ``harness.control_command_applied`` event so the audit stream
    attributes the operator decision to the same telemetry shape the
    live in-session watcher uses (``EVENT_CONTROL_APPLIED`` in
    :mod:`flywheel_core.invoker_client`).
    """
    telemetry.emit(
        kind="harness.control_command_applied",
        payload={
            "command_id": command.id,
            "kind": command.kind,
            "payload": dict(command.payload),
        },
        attempt_number=attempt_number,
    )


def _ledger_steering(
    lifecycle: Lifecycle,
    store: HarnessStore,
    command: ControlCommandRecord,
    *,
    attempt_number: int | None,
    clock: Callable[[], datetime],
) -> None:
    """Append the :class:`CommandApplied` ledger fact, then delete the row.

    The resolver-side twin of ``_drive_iterations``'s ``_record_steering``
    (spec 00025 FR-10): the application already happened, so an append
    failure retains the claimed queue row as the visible trace and
    surfaces on stderr rather than raising back into the caller.
    """
    try:
        _append(
            lifecycle,
            CommandApplied(
                run_id=lifecycle.run_id,
                ts=clock(),
                attempt_number=attempt_number,
                command_kind=command.kind,
                command_payload=dict(command.payload),
                command_id=command.id,
            ),
            store=store,
        )
    except Exception as exc:  # noqa: BLE001 - application already landed.
        print(
            "flywheel: steering ledger append failed for command "
            f"{command.id!r} ({command.kind}): {type(exc).__name__}: {exc}",
            file=sys.stderr,
        )
        return
    if command.id is None:
        return
    try:
        store.delete_command(command.id)
    except Exception as exc:  # noqa: BLE001 - hygiene is best-effort.
        print(
            "flywheel: applied control command row "
            f"{command.id!r} could not be deleted: "
            f"{type(exc).__name__}: {exc}",
            file=sys.stderr,
        )


def resolve_manual_approval(
    lifecycle: Lifecycle,
    store: HarnessStore,
    task: Task,
    *,
    max_retries: int = 0,
    sink: TelemetrySink | None = None,
    now: Callable[[], datetime] | None = None,
) -> ResolveApprovalOutcome:
    """Apply the oldest pending approve/reject command to a parked gate.

    Structural sibling of :func:`recheck_blocked_lifecycle`: out-of-band
    resolution of a parked, non-running lifecycle, driven by the
    orchestrator's reactive sweep. The caller hands in the lifecycle
    snapshot it loaded for the sweep tick; this function mutates that
    snapshot in place (status, version, ``awaiting_manual_ordinal``,
    retry counter) so the caller's view stays aligned with the
    persisted projection across the resolver's appends.

    Per spec FR-5 and FR-6:

    * approve: writes a ``passed=True`` manual
      :class:`GraderResultRecord` (keyed to the already-finalized
      ``SUCCEEDED`` attempt) for the gate at
      ``lifecycle.awaiting_manual_ordinal`` via
      :func:`flywheel_core.grader_manual.build_manual_result`; selects the
      next manual gate via
      :func:`flywheel_core.grader_manual.next_pending_manual_gate`; if one
      exists, re-parks (appends an :class:`AwaitingApproval` domain
      event with the new ordinal and emits a fresh
      ``harness.awaiting_approval`` so operators learn another decision
      is owed); if none remain, transitions
      ``AWAITING_APPROVAL -> DONE`` (the attempt was finalized at gate
      entry per FR-4 -- there is no second attempt-finalize). Emits
      ``harness.manual_approved`` with ``{grader_name, awaiting_ordinal}``.
    * reject: writes a ``passed=False`` manual
      :class:`GraderResultRecord` whose summary is the command's
      ``feedback`` payload (or the documented ``"(no feedback provided)"``
      placeholder for the absent / empty cases); transitions
      ``AWAITING_APPROVAL -> FAILED_VALIDATION`` with
      ``error = "manual grader '<name>' rejected by operator"`` and **no**
      attempt re-finalize (the attempt's ``SUCCEEDED`` outcome accurately
      records that the agent passed every automated grader; the rejection
      is captured by the manual receipt). The resolver then drives the
      same retry arm :func:`run_task` uses: when
      :meth:`Lifecycle.is_retry_eligible` is ``True`` it emits
      ``harness.retry_scheduled`` and transitions
      ``FAILED_VALIDATION -> READY`` (consuming one retry on that edge);
      otherwise it transitions ``-> FAILED`` retaining the rejection
      error. A reject short-circuits any later gates (a fresh attempt on
      retry will re-evaluate them from the top). Emits
      ``harness.manual_rejected`` with
      ``{grader_name, awaiting_ordinal, feedback}``.

    Claim semantics: the resolver calls
    :meth:`ControlCommandStore.claim_commands` and selects the first
    ``approve`` / ``reject`` row in enqueue order. The single-claim
    primitive (``idx_control_commands_pending``) ensures two workers
    cannot double-apply the same command. When the claim batch carries
    no ``approve`` / ``reject`` row, the lifecycle stays parked and the
    caller learns via ``reason="no_pending_command"``. The applied
    command is recorded via the existing
    ``harness.control_command_applied`` event so the resolver's audit
    surface matches the live in-session watcher's.

    Concurrent callers race on ``lifecycle.version`` exactly as the
    rest of the harness does; the loser surfaces
    :class:`flywheel_core.store_protocols.OptimisticConcurrencyError` to its
    caller -- this function does not swallow it.
    """
    clock = now or _utcnow
    telemetry = _RunTelemetry(sink, run_id=lifecycle.run_id, clock=clock)
    store = _MirroringStore(store, telemetry)

    if lifecycle.status != Status.AWAITING_APPROVAL:
        return ResolveApprovalOutcome(applied=False, reason="not_awaiting")
    ordinal = lifecycle.awaiting_manual_ordinal
    if ordinal is None:
        return ResolveApprovalOutcome(applied=False, reason="not_awaiting")
    if ordinal < 0 or ordinal >= len(task.graders):
        return ResolveApprovalOutcome(applied=False, reason="missing_gate")
    grader = task.graders[ordinal]
    if not isinstance(grader, ManualGrader):
        return ResolveApprovalOutcome(applied=False, reason="missing_gate")

    gate = ManualGate(
        ordinal=ordinal,
        instruction=grader.instruction,
        grader_name=grader.name,
    )

    # Claim every pending row for the run. The single-claim primitive
    # (idx_control_commands_pending) prevents two workers from grabbing
    # the same row; per the spec's "claim the oldest approve/reject" we
    # apply the first approve/reject in id order and leave any other
    # rows in the claim batch to the resolver's audit surface (the
    # in-session watcher owns interrupt/say/set_model; against an
    # AWAITING_APPROVAL lifecycle those rows are orphans either way
    # because no live session is running).
    claimed = store.claim_commands(lifecycle.run_id, now=clock())

    target: ControlCommandRecord | None = None
    for cmd in claimed:
        if cmd.kind in (CONTROL_COMMAND_APPROVE, CONTROL_COMMAND_REJECT):
            target = cmd
            break

    if target is None:
        return ResolveApprovalOutcome(
            applied=False, reason="no_pending_command"
        )

    # The manual receipt is keyed to the attempt already finalized
    # SUCCEEDED at gate entry. With no attempt at all the parked state
    # is structurally impossible (FR-4 finalizes before transitioning);
    # bail defensively so the parked state survives for diagnosis.
    attempts = store.list_attempts(lifecycle.run_id)
    if not attempts:
        return ResolveApprovalOutcome(applied=False, reason="no_attempt")
    attempt_number = attempts[-1].number

    if target.kind == CONTROL_COMMAND_APPROVE:
        store.append_grader_result(
            build_manual_result(
                gate,
                run_id=lifecycle.run_id,
                attempt_number=attempt_number,
                passed=True,
                summary="approved",
                now=clock(),
            )
        )

        next_gate = next_pending_manual_gate(task, after_ordinal=ordinal)
        if next_gate is not None:
            _append(
                lifecycle,
                AwaitingApproval(
                    run_id=lifecycle.run_id,
                    ts=clock(),
                    attempt_number=attempt_number,
                    awaiting_ordinal=next_gate.ordinal,
                ),
                store=store,
            )
            artifacts_dir = lifecycle.artifacts_dir or ""
            telemetry.emit(
                kind="harness.awaiting_approval",
                payload={
                    "instructions": next_gate.instruction,
                    "awaiting_ordinal": next_gate.ordinal,
                    "grader_name": next_gate.grader_name,
                    "run_id": lifecycle.run_id,
                    "attempt_number": attempt_number,
                    "artifacts_dir": artifacts_dir,
                },
                attempt_number=attempt_number,
            )
            reason = "approved_next_gate"
        else:
            _transition(lifecycle, Status.DONE, store=store, now=clock)
            reason = "approved_done"

        telemetry.emit(
            kind="harness.manual_approved",
            payload={
                "grader_name": gate.grader_name,
                "awaiting_ordinal": ordinal,
            },
            attempt_number=attempt_number,
        )
        _emit_control_applied(
            telemetry, target, attempt_number=attempt_number
        )
        _ledger_steering(
            lifecycle,
            store,
            target,
            attempt_number=attempt_number,
            clock=clock,
        )
        return ResolveApprovalOutcome(
            applied=True, reason=reason, command_id=target.id
        )

    # Reject path.
    feedback_text = _reject_feedback_text(target.payload)
    store.append_grader_result(
        build_manual_result(
            gate,
            run_id=lifecycle.run_id,
            attempt_number=attempt_number,
            passed=False,
            summary=feedback_text,
            now=clock(),
        )
    )

    grader_label = gate.grader_name or "manual"
    error = f"manual grader '{grader_label}' rejected by operator"

    _transition(
        lifecycle,
        Status.FAILED_VALIDATION,
        store=store,
        error=error,
        now=clock,
    )
    telemetry.emit(
        kind="harness.manual_rejected",
        payload={
            "grader_name": gate.grader_name,
            "awaiting_ordinal": ordinal,
            "feedback": feedback_text,
        },
        attempt_number=attempt_number,
    )
    _emit_control_applied(
        telemetry, target, attempt_number=attempt_number
    )
    _ledger_steering(
        lifecycle,
        store,
        target,
        attempt_number=attempt_number,
        clock=clock,
    )

    # Reuse the same retry arm run_task drives so a reject reaches the
    # same FAILED_VALIDATION -> READY (consuming budget) / -> FAILED
    # (exhausted) decision regardless of which entry point lands the
    # parked-lifecycle resolution. The retry-counter increment lives on
    # the FAILED_VALIDATION -> READY edge in Lifecycle.apply_transition.
    if lifecycle.is_retry_eligible(max_retries):
        telemetry.emit(
            kind="harness.retry_scheduled",
            payload={
                "retries_used": lifecycle.retries,
                "max_retries": max_retries,
            },
            attempt_number=attempt_number,
        )
        _transition(lifecycle, Status.READY, store=store, now=clock)
        return ResolveApprovalOutcome(
            applied=True, reason="rejected_retry", command_id=target.id
        )

    _transition(
        lifecycle,
        Status.FAILED,
        store=store,
        error=error,
        now=clock,
    )
    return ResolveApprovalOutcome(
        applied=True, reason="rejected_failed", command_id=target.id
    )


__all__ = [
    "HarnessConfig",
    "HarnessOutcome",
    "HarnessStore",
    "InvocationRequest",
    "InvokeFunc",
    "RecheckOutcome",
    "ResolveApprovalOutcome",
    "finalize_stranded_lifecycle",
    "recheck_blocked_lifecycle",
    "resolve_manual_approval",
    "run_task",
]
