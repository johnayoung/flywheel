"""Repo-owned attempt budgets reach the harness at the worker's seam.

:func:`flywheel_core.workflow.run_task_object` is the single-task seam the
worker drives. Two ``flywheel.toml``-owned attempt budgets ride it onto the
harness config:

* ``deadlines`` -> :attr:`HarnessConfig.deadlines` (spec 00066 wall-clock
  ceilings). Proven *behaviorally*: a tiny ``agent_iteration`` ceiling plus an
  ``invoke`` that never returns cancels the run in bounded wall time and
  surfaces the timeout-classified ``INTERNAL_ERROR`` containment path.
* ``rubric_judge_max_turns`` -> :attr:`HarnessConfig.rubric_judge_max_turns`
  (the per-judge turn budget). Proven by capturing the ``HarnessConfig`` the
  seam constructs.

Each knob forwards *only when supplied*: a caller that passes neither builds a
byte-identical config -- the harness keeps its finite default-on ceilings and
32 judge turns. That default-preserving path is pinned too so a future forward
that leaks a non-``None`` sentinel is caught.
"""

from __future__ import annotations

import asyncio
import io
import time
from pathlib import Path

from flywheel_core import (
    HarnessConfig,
    HarnessOutcome,
    InMemoryStore,
    InvocationRequest,
    IterationResult,
    Lifecycle,
    Outcome,
    Status,
    Task,
)
from flywheel_core.deadline_config import DeadlineConfig
from flywheel_core.store_protocols import TelemetryRecord
from flywheel_core.workflow import run_task_object


class _ListSink:
    """In-memory TelemetrySink so a run touches no telemetry file."""

    def __init__(self) -> None:
        self.records: list[TelemetryRecord] = []

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self.records.append(record)


async def _never_returns(request: InvocationRequest) -> IterationResult:
    """An invoker that hangs forever, so only the deadline can end it."""
    await asyncio.Future()
    raise RuntimeError("unreachable")  # pragma: no cover


def test_deadlines_override_cancels_never_returning_invoke(
    tmp_path: Path,
) -> None:
    # A tiny agent-iteration ceiling threaded through the seam onto
    # HarnessConfig.deadlines cancels an invoker that never returns, in real
    # (bounded) wall time. This is the end-to-end behavior proof: the knob is
    # not merely stored, it governs the run.
    store = InMemoryStore()
    task = Task(goal="hang forever", graders=[])

    started = time.monotonic()
    outcome = asyncio.run(
        run_task_object(
            task,
            db_path=tmp_path / "db" / "flywheel.sqlite",
            sandbox=tmp_path / "sandbox",
            max_retries=0,
            deadlines=DeadlineConfig(agent_iteration_seconds=0.05),
            invoke=_never_returns,
            store=store,
            sink=_ListSink(),
            stream=io.StringIO(),
        )
    )
    elapsed = time.monotonic() - started

    # Bounded wall time: the 0.05s ceiling fires; a broken forward (knob
    # dropped -> the harness's finite default of 3600s) would hang far past
    # this generous bound.
    assert elapsed < 10.0

    # Timeout-classified containment: max_retries=0 -> lifecycle FAILED with a
    # single INTERNAL_ERROR attempt whose error names the deadline.
    assert outcome.lifecycle.status == Status.FAILED
    assert len(outcome.attempts) == 1
    attempt = outcome.attempts[0]
    assert attempt.outcome == Outcome.INTERNAL_ERROR
    assert attempt.error is not None
    assert "deadline" in attempt.error


def test_rubric_judge_max_turns_forwarded_to_harness_config(
    tmp_path: Path, monkeypatch
) -> None:
    # The turn budget has no cheap behavioral trigger at this seam (it governs
    # a rubric judge call), so capture the HarnessConfig the seam constructs
    # and assert both budgets landed on it. Stubbing run_task keeps the test
    # transport-free and fast.
    captured: dict[str, HarnessConfig] = {}

    async def _capture(
        task: Task,
        lifecycle: Lifecycle,
        backend: object,
        *,
        config: HarnessConfig,
        invoke: object,
        sink: object,
    ) -> HarnessOutcome:
        captured["config"] = config
        return HarnessOutcome(lifecycle=lifecycle, attempts=())

    monkeypatch.setattr("flywheel_core.workflow.run_task", _capture)

    asyncio.run(
        run_task_object(
            Task(goal="g", graders=[]),
            db_path=tmp_path / "db" / "flywheel.sqlite",
            sandbox=tmp_path / "sandbox",
            deadlines=DeadlineConfig(rubric_judge_seconds=123.0),
            rubric_judge_max_turns=8,
            invoke=_never_returns,
            store=InMemoryStore(),
            sink=_ListSink(),
            stream=io.StringIO(),
        )
    )

    config = captured["config"]
    assert config.rubric_judge_max_turns == 8
    assert config.deadlines == DeadlineConfig(rubric_judge_seconds=123.0)


def test_budgets_default_preserved_when_knobs_absent(
    tmp_path: Path, monkeypatch
) -> None:
    # A caller that passes neither knob (every existing caller) must build a
    # byte-identical HarnessConfig: the harness's own finite default-on
    # deadlines and 32 judge turns, never a leaked None/sentinel override.
    captured: dict[str, HarnessConfig] = {}

    async def _capture(
        task: Task,
        lifecycle: Lifecycle,
        backend: object,
        *,
        config: HarnessConfig,
        invoke: object,
        sink: object,
    ) -> HarnessOutcome:
        captured["config"] = config
        return HarnessOutcome(lifecycle=lifecycle, attempts=())

    monkeypatch.setattr("flywheel_core.workflow.run_task", _capture)

    asyncio.run(
        run_task_object(
            Task(goal="g", graders=[]),
            db_path=tmp_path / "db" / "flywheel.sqlite",
            sandbox=tmp_path / "sandbox",
            invoke=_never_returns,
            store=InMemoryStore(),
            sink=_ListSink(),
            stream=io.StringIO(),
        )
    )

    config = captured["config"]
    assert config.rubric_judge_max_turns == 32
    assert config.deadlines == DeadlineConfig()


def test_task_budget_overrides_the_iteration_ceiling_per_task(
    tmp_path: Path,
) -> None:
    """Per-task budgets beat the class ceiling: a task declaring a tiny
    ``budgets.agent_iteration_seconds`` is cancelled by ITS ceiling even
    though the resolved [deadlines] class ceiling is the finite default
    (3600s). The heavyweight-tail inverse (a task RAISING its ceiling above
    the class value) rides the same _override_ceiling precedence, unit-pinned
    below."""
    from flywheel_core import TaskBudgets

    store = InMemoryStore()
    task = Task(
        goal="hang forever",
        graders=[],
        budgets=TaskBudgets(agent_iteration_seconds=0.05),
    )

    started = time.monotonic()
    outcome = asyncio.run(
        run_task_object(
            task,
            db_path=tmp_path / "db" / "flywheel.sqlite",
            sandbox=tmp_path / "sandbox",
            max_retries=0,
            invoke=_never_returns,
            store=store,
            sink=_ListSink(),
            stream=io.StringIO(),
        )
    )
    elapsed = time.monotonic() - started

    assert elapsed < 10.0, "the task's own ceiling must govern the run"
    assert outcome.lifecycle.status == Status.FAILED
    assert outcome.attempts[0].outcome == Outcome.INTERNAL_ERROR
    assert "deadline" in (outcome.attempts[0].error or "")


def test_override_ceiling_precedence() -> None:
    """None inherits; 0 is the per-task unbounded opt-out; positive replaces
    -- in BOTH directions (a heavyweight task may raise the ceiling)."""
    from flywheel_core.harness import _override_ceiling

    assert _override_ceiling(None, 600.0) == 600.0
    assert _override_ceiling(None, None) is None
    assert _override_ceiling(0, 600.0) is None
    assert _override_ceiling(120.0, 600.0) == 120.0
    assert _override_ceiling(7200.0, 3600.0) == 7200.0
