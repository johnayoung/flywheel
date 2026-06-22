"""Held-out oracle for spec 00039 (cost budget ceiling, increment D of 00036).

RED until ``sandbox-budget-ceilings`` lands. The harness enforces a PER-RUN
cumulative cost ceiling (``HarnessConfig.max_cost_usd``): once the run's total
``total_cost_usd`` across all attempts reaches the ceiling, the run terminates
``Status.FAILED`` (non-retryable, mirroring the ABORT path), emitting a
``harness.budget_ceiling_breached`` event (``payload["ceiling"] == "cost_usd"``)
so audit distinguishes a budget kill from an agent error. A zero ceiling (the
``fast`` default) is unenforced. Do not weaken or delete assertions.

Self-contained (own minimal harness-test helpers) so it stands alone as the
grading surface.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from typing import Any

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


def _signals(cost: float) -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=cost,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="s",
    )


def _iter(intent: Intent, cost: float) -> IterationResult:
    return IterationResult(
        transcript="",
        messages=(),
        envelope=ValidEnvelope(intent=intent),
        signals=_signals(cost),
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


def _run(coro: Any) -> HarnessOutcome:
    return asyncio.run(coro)


def _breaches(sink: _Sink, run_id: str) -> list[TelemetryRecord]:
    return [
        r
        for r in sink.records
        if r.run_id == run_id and r.kind == "harness.budget_ceiling_breached"
    ]


PASS = CommandGrader(run="true")
FAIL = CommandGrader(run="false")


def test_cost_breach_is_terminal_and_non_retryable() -> None:
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-cost-1")
    invoke = _scripted([_iter(Intent.VERIFY, 0.10)])  # 0.10 >= 0.05 ceiling
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            config=HarnessConfig(max_retries=1, max_cost_usd=0.05),
        )
    )
    assert out.lifecycle.status == Status.FAILED  # terminal, not failed_validation
    assert len(out.attempts) == 1  # breach pre-empts the grade and the retry
    breaches = _breaches(sink, "run-cost-1")
    assert len(breaches) == 1
    assert breaches[0].payload["ceiling"] == "cost_usd"


def test_cost_accumulates_across_attempts() -> None:
    # Per-run proof: attempt 1 (0.03, under ceiling) fails validation and
    # retries; attempt 2 (0.03) pushes the RUN cumulative to 0.06 >= 0.05 and
    # breaches. A per-attempt impl resets each attempt and never breaches here.
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[FAIL])
    lc = Lifecycle(task_id="t", run_id="run-cost-2")
    invoke = _scripted([_iter(Intent.VERIFY, 0.03), _iter(Intent.VERIFY, 0.03)])
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            config=HarnessConfig(max_retries=1, max_cost_usd=0.05),
        )
    )
    assert out.lifecycle.status == Status.FAILED
    assert len(out.attempts) == 2  # retried, then breached in attempt 2
    breaches = _breaches(sink, "run-cost-2")
    assert len(breaches) == 1
    assert breaches[0].payload["ceiling"] == "cost_usd"


def test_under_ceiling_completes_done() -> None:
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-cost-3")
    invoke = _scripted([_iter(Intent.VERIFY, 0.01)])
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            config=HarnessConfig(max_retries=1, max_cost_usd=1.0),
        )
    )
    assert out.lifecycle.status == Status.DONE
    assert _breaches(sink, "run-cost-3") == []


def test_zero_ceiling_is_unenforced_backcompat() -> None:
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-cost-4")
    invoke = _scripted([_iter(Intent.VERIFY, 100.0)])  # huge cost, ceiling off
    out = _run(
        run_task(
            task,
            lc,
            store,
            sink=sink,
            invoke=invoke,
            config=HarnessConfig(max_retries=1, max_cost_usd=0.0),
        )
    )
    assert out.lifecycle.status == Status.DONE
    assert _breaches(sink, "run-cost-4") == []
