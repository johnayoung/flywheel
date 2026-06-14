"""Transcript grader runner + hard-limit enforcement.

A :class:`flywheel_core.task.TranscriptGrader` on a :class:`flywheel_core.task.Task`
carries two responsibilities that this module covers from a single source
of truth:

1. **Hard limit enforcement during a run.** When ``max_turns``,
   ``max_total_tokens``, or ``max_wall_seconds`` is set on a transcript
   grader, the harness wraps the agent's message stream with
   :func:`enforce_transcript_limits` and the stream ends as soon as the
   first limit is breached — matching ``docs/loop.md``'s requirement that
   ``max_turns`` halts the agent before the grader stage is reached.

2. **Validation-time grading.** :func:`run_transcript_graders` evaluates
   every transcript grader against the observed totals and persists one
   row per grader to ``grader_results`` with the documented payload
   shape: ``{"observed": {...}, "breached": "max_..."}``. The runner
   reads the same grader spec the hard-limit enforcer used, so the abort
   and the grading row agree on the breached field.

Per ``docs/task-schema.md`` cost-order, transcript graders only run
after command graders pass — :func:`run_transcript_graders` accepts an
explicit ``command_passed`` flag for the harness to wire that in.

The runner has no opinion about retry policy or lifecycle transitions;
those belong to the harness (``roadmap-10``).
"""

from __future__ import annotations

import time
from collections.abc import (
    AsyncIterable,
    AsyncIterator,
    Callable,
    Mapping,
    Sequence,
)
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any, Literal

if TYPE_CHECKING:
    from claude_agent_sdk import Message

from flywheel_core.store_protocols import GraderResultRecord, GraderResultStore
from flywheel_core.task import Task, TranscriptGrader

BreachedField = Literal["max_turns", "max_total_tokens", "max_wall_seconds"]

# Canonical order — both hard-limit enforcement and validation-time grading
# evaluate fields in this order so multi-field graders converge on the same
# "first breach" verdict regardless of which path observed it first.
_LIMIT_ORDER: tuple[BreachedField, ...] = (
    "max_turns",
    "max_total_tokens",
    "max_wall_seconds",
)

# Anthropic API usage dict keys that contribute to billable token totals.
_USAGE_TOKEN_KEYS: tuple[str, ...] = (
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
)


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


def total_tokens_from_usage(usage: Mapping[str, Any] | None) -> int:
    """Sum the documented token fields from an SDK ``usage`` dict.

    Counts every billed category — input, output, cache-creation, and
    cache-read — since each one is paid for and therefore counts against
    ``max_total_tokens``. Missing or non-numeric fields contribute zero.
    """
    if not usage:
        return 0
    total = 0
    for key in _USAGE_TOKEN_KEYS:
        value = usage.get(key)
        if value is None:
            continue
        try:
            total += int(value)
        except (TypeError, ValueError):
            continue
    return total


@dataclass(frozen=True, kw_only=True)
class TranscriptObservation:
    """One iteration's observed transcript totals.

    The harness produces this from the iteration's SDK signals + a
    monotonic wall clock; the grader runner consumes it. ``wall_seconds``
    is monotonic so a system clock change during a run cannot flip the
    verdict.
    """

    turns: int
    total_tokens: int
    wall_seconds: float


@dataclass
class TranscriptCounter:
    """Stateful accumulator that observes SDK messages as they stream.

    Used by :func:`enforce_transcript_limits` to track running totals,
    and reusable by the harness to compute the final
    :class:`TranscriptObservation` once the stream has been drained.

    The clock is injected as a callable so tests can advance wall time
    deterministically without sleeping; production callers should let
    :meth:`start` default to :func:`time.monotonic`.
    """

    turns: int = 0
    total_tokens: int = 0
    started_at_monotonic: float = 0.0
    _monotonic: Callable[[], float] = field(
        default=time.monotonic, repr=False, compare=False
    )

    @classmethod
    def start(
        cls,
        *,
        monotonic: Callable[[], float] | None = None,
    ) -> TranscriptCounter:
        clock = monotonic or time.monotonic
        return cls(started_at_monotonic=clock(), _monotonic=clock)

    def observe(self, msg: Message) -> None:
        from flywheel_core._sdk import AssistantMessage, ResultMessage

        if isinstance(msg, AssistantMessage):
            self.turns += 1
            self.total_tokens += total_tokens_from_usage(msg.usage)
        elif isinstance(msg, ResultMessage):
            # ResultMessage may report cumulative totals — take the max
            # so we never lose count if AssistantMessage usage was absent
            # but the final ResultMessage reports the run's totals.
            if msg.usage:
                self.total_tokens = max(
                    self.total_tokens, total_tokens_from_usage(msg.usage)
                )
            if msg.num_turns is not None:
                self.turns = max(self.turns, msg.num_turns)

    def snapshot(self) -> TranscriptObservation:
        return TranscriptObservation(
            turns=self.turns,
            total_tokens=self.total_tokens,
            wall_seconds=self._monotonic() - self.started_at_monotonic,
        )


def first_breach(
    grader: TranscriptGrader,
    observed: TranscriptObservation,
) -> BreachedField | None:
    """Return the first field of ``grader`` strictly exceeded by
    ``observed``, or ``None`` if every limit is respected.

    "First" is canonical order — ``max_turns`` → ``max_total_tokens`` →
    ``max_wall_seconds`` — so hard-limit aborts and validation-time
    grading converge on the same breached field even when multiple
    limits are set on one grader. Limits are inclusive: a run that ends
    exactly at the limit is a pass; only strictly exceeding triggers a
    breach.
    """
    observed_values: dict[BreachedField, float] = {
        "max_turns": observed.turns,
        "max_total_tokens": observed.total_tokens,
        "max_wall_seconds": observed.wall_seconds,
    }
    for name in _LIMIT_ORDER:
        limit = getattr(grader, name)
        if limit is None:
            continue
        if observed_values[name] > limit:
            return name
    return None


async def enforce_transcript_limits(
    source: AsyncIterable[Message],
    *,
    graders: Sequence[TranscriptGrader],
    monotonic: Callable[[], float] | None = None,
) -> AsyncIterator[Message]:
    """Yield messages from ``source`` until any grader's first limit is
    breached, then stop the stream cleanly.

    Each yielded message updates a :class:`TranscriptCounter`; after
    every yield the snapshot is compared against every grader's
    :func:`first_breach`. When the first breach is observed the wrapper
    returns — the consumer (typically
    :func:`flywheel_core.invoker.invoke_iteration`) sees a clean end-of-stream
    and proceeds to envelope parsing. The grader runner, given the same
    observed totals, will then record the breach on the persisted
    ``grader_results`` row, so the abort and the row agree.

    An empty ``graders`` sequence makes this a transparent pass-through.
    """
    if not graders:
        async for msg in source:
            yield msg
        return

    counter = TranscriptCounter.start(monotonic=monotonic)
    async for msg in source:
        counter.observe(msg)
        yield msg
        snapshot = counter.snapshot()
        for grader in graders:
            if first_breach(grader, snapshot) is not None:
                return


def _grader_spec_snapshot(grader: TranscriptGrader) -> dict[str, Any]:
    """Snapshot the grader object as it appeared in the task at run time.

    Persisted into ``grader_spec_json`` so later edits to the task
    definition do not rewrite historical truth — mirroring the
    convention used by the command grader runner.
    """
    spec: dict[str, Any] = {"type": "transcript"}
    if grader.max_turns is not None:
        spec["max_turns"] = grader.max_turns
    if grader.max_total_tokens is not None:
        spec["max_total_tokens"] = grader.max_total_tokens
    if grader.max_wall_seconds is not None:
        spec["max_wall_seconds"] = grader.max_wall_seconds
    if grader.name is not None:
        spec["name"] = grader.name
    return spec


def run_transcript_graders(
    task: Task,
    observation: TranscriptObservation,
    store: GraderResultStore,
    *,
    run_id: str,
    attempt_number: int,
    command_passed: bool = True,
    now: Callable[[], datetime] | None = None,
) -> list[GraderResultRecord]:
    """Evaluate every ``TranscriptGrader`` on ``task.graders`` and persist
    one ``grader_results`` row per execution in list order.

    For each transcript grader:

    1. Compute the first breached field (canonical order) given
       ``observation``.
    2. Build a payload matching the documented shape:
       ``{"observed": {"turns", "total_tokens", "wall_seconds"},
       "breached": "max_..."}``. ``breached`` is omitted when the grader
       passes.
    3. Append one :class:`GraderResultRecord` to ``store`` whose
       ``grader_spec`` snapshots the grader verbatim and whose
       ``ordinal`` is the grader's index in ``task.graders`` (so other
       grader types preserve numbering).

    Cost-order is honored at two scales:

    * Across types: when ``command_passed`` is ``False`` the runner
      returns ``[]`` without persisting any row — transcript graders are
      skipped entirely when an earlier (cheaper) type failed.
    * Within type: on the first failing transcript grader, later
      transcript graders are not graded.

    Non-transcript graders in ``task.graders`` are ignored without
    persisting any row.
    """
    if not command_passed:
        return []

    clock = now or _utcnow
    persisted: list[GraderResultRecord] = []
    aborted = False

    for ordinal, grader in enumerate(task.graders):
        if not isinstance(grader, TranscriptGrader):
            continue
        if aborted:
            break

        ts_start = clock()
        start_ns = time.monotonic_ns()

        breached = first_breach(grader, observation)
        passed = breached is None

        end_ns = time.monotonic_ns()
        duration_ms = (end_ns - start_ns) // 1_000_000

        payload: dict[str, Any] = {
            "observed": {
                "turns": observation.turns,
                "total_tokens": observation.total_tokens,
                "wall_seconds": observation.wall_seconds,
            }
        }
        if breached is not None:
            payload["breached"] = breached

        record = GraderResultRecord(
            run_id=run_id,
            attempt_number=attempt_number,
            ordinal=ordinal,
            grader_type="transcript",
            grader_spec=_grader_spec_snapshot(grader),
            grader_name=grader.name,
            passed=passed,
            duration_ms=int(duration_ms),
            payload=payload,
            ts=ts_start,
        )
        persisted.append(store.append_grader_result(record))

        if not passed:
            aborted = True

    return persisted


__all__ = [
    "BreachedField",
    "TranscriptCounter",
    "TranscriptObservation",
    "enforce_transcript_limits",
    "first_breach",
    "run_transcript_graders",
    "total_tokens_from_usage",
]
