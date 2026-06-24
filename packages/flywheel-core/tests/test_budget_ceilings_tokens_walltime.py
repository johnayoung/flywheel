"""Held-out oracle for spec 00042 (token + wall-clock ceilings, finishing
increment D of 00036).

RED until ``sandbox-budget-tokens-walltime`` lands. The harness enforces PER-RUN
cumulative ``max_tokens`` and ``wall_clock_seconds`` ceilings with the same
shape as the shipped cost ceiling (00039): once the run's cumulative tokens (sum
of ``Attempt.total_tokens``) or elapsed wall time reaches the ceiling, the run
terminates ``Status.FAILED`` (non-retryable, pre-empting the grade), emitting a
``harness.budget_ceiling_breached`` event (``payload["ceiling"]`` of ``"tokens"``
/ ``"wall_clock_seconds"``). A zero ceiling (the ``fast`` default) is unenforced.
Do not weaken or delete assertions.

Self-contained: builds its own SDK messages (for the token rollup) and a
controllable clock (for wall time), so it stands alone as the grading surface.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from datetime import datetime, timedelta, timezone
from typing import Any

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    CommandGrader,
    HarnessConfig,
    HarnessOutcome,
    InMemoryStore,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    Status,
    Task,
    run_task,
)
from flywheel_core.envelope import Intent, ValidEnvelope
from flywheel_core.store_protocols import TelemetryRecord


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="s",
    )


def _iter(intent: Intent, *, input_tokens: int) -> IterationResult:
    """An iteration whose AssistantMessage usage drives ``Attempt.total_tokens``."""
    assistant = AssistantMessage(
        content=[TextBlock(text="ok")],
        model="claude-test",
        stop_reason="end_turn",
        session_id="s",
        usage={"input_tokens": input_tokens},
    )
    result = ResultMessage(
        subtype="success",
        duration_ms=10,
        duration_api_ms=8,
        is_error=False,
        num_turns=1,
        session_id="s",
        stop_reason="end_turn",
        total_cost_usd=0.0,
        usage=None,
    )
    return IterationResult(
        transcript="",
        messages=(assistant, result),
        envelope=ValidEnvelope(intent=intent),
        signals=_signals(),
        failure=None,
    )


def _scripted(
    results: list[IterationResult],
) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return results.pop(0)

    return _invoke


class _Sink:
    def __init__(self) -> None:
        self.records: list[TelemetryRecord] = []

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self.records.append(record)


def _advancing_clock(step_seconds: float) -> Callable[[], datetime]:
    """A clock that advances ``step_seconds`` on every call.

    The attempt's ``started_at`` is the first reading; the budget check later
    in the same iteration reads a strictly later time, so elapsed >= one step.
    """
    base = datetime(2026, 1, 1, tzinfo=timezone.utc)
    state = {"n": 0}

    def _now() -> datetime:
        state["n"] += 1
        return base + timedelta(seconds=step_seconds * state["n"])

    return _now


def _run(coro: Any) -> HarnessOutcome:
    return asyncio.run(coro)


def _breaches(sink: _Sink, run_id: str, ceiling: str) -> list[TelemetryRecord]:
    return [
        r
        for r in sink.records
        if r.run_id == run_id
        and r.kind == "harness.budget_ceiling_breached"
        and r.payload.get("ceiling") == ceiling
    ]


PASS = CommandGrader(run="true")
FAIL = CommandGrader(run="false")


def test_token_breach_is_terminal_and_non_retryable() -> None:
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-tok-1")
    invoke = _scripted([_iter(Intent.VERIFY, input_tokens=2000)])  # >= 1500
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            config=HarnessConfig(max_retries=1, max_tokens=1500),
        )
    )
    assert out.lifecycle.status == Status.FAILED
    assert len(out.attempts) == 1  # breach pre-empts the grade and the retry
    assert len(_breaches(sink, "run-tok-1", "tokens")) == 1


def test_tokens_accumulate_across_attempts() -> None:
    # Per-run proof: attempt 1 (1000 tokens, under 1500) fails validation and
    # retries; attempt 2 (1000) pushes the RUN cumulative to 2000 >= 1500 and
    # breaches. A per-attempt impl resets each attempt and never breaches.
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[FAIL])
    lc = Lifecycle(task_id="t", run_id="run-tok-2")
    invoke = _scripted(
        [_iter(Intent.VERIFY, input_tokens=1000), _iter(Intent.VERIFY, input_tokens=1000)]
    )
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            config=HarnessConfig(max_retries=1, max_tokens=1500),
        )
    )
    assert out.lifecycle.status == Status.FAILED
    assert len(out.attempts) == 2  # retried, then breached in attempt 2
    assert len(_breaches(sink, "run-tok-2", "tokens")) == 1


def test_wall_clock_breach_is_terminal_and_non_retryable() -> None:
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-wc-1")
    invoke = _scripted([_iter(Intent.VERIFY, input_tokens=1)])
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            config=HarnessConfig(max_retries=1, wall_clock_seconds=1),
            now=_advancing_clock(step_seconds=100.0),  # elapsed >> 1s ceiling
        )
    )
    assert out.lifecycle.status == Status.FAILED
    assert len(out.attempts) == 1
    assert len(_breaches(sink, "run-wc-1", "wall_clock_seconds")) == 1


def test_under_ceilings_completes_done() -> None:
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-under")
    invoke = _scripted([_iter(Intent.VERIFY, input_tokens=10)])
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            config=HarnessConfig(
                max_retries=1, max_tokens=1_000_000, wall_clock_seconds=86_400
            ),
        )
    )
    assert out.lifecycle.status == Status.DONE
    assert _breaches(sink, "run-under", "tokens") == []
    assert _breaches(sink, "run-under", "wall_clock_seconds") == []


def test_zero_ceilings_are_unenforced_backcompat() -> None:
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-zero")
    invoke = _scripted([_iter(Intent.VERIFY, input_tokens=10_000_000)])
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            # Huge usage + an advancing clock, but both ceilings off.
            config=HarnessConfig(max_retries=1, max_tokens=0, wall_clock_seconds=0),
            now=_advancing_clock(step_seconds=100.0),
        )
    )
    assert out.lifecycle.status == Status.DONE
    assert _breaches(sink, "run-zero", "tokens") == []
    assert _breaches(sink, "run-zero", "wall_clock_seconds") == []
