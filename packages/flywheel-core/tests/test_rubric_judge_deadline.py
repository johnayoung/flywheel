"""Wall-clock deadline on the rubric judge invocation (spec 00066 #3).

The rubric judge stream sits outside ``_drive_iterations`` and the harness's
inter-message silence watchdog. Before this bound a judge whose ``async for``
never terminated -- even one steadily yielding output -- stalled VALIDATING
forever. These tests prove the judge invocation now runs under a finite
wall-clock ceiling: a never-ending judge stream is cancelled in bounded wall
time and surfaced as a :class:`RubricJudgeError` (the judge-infra
classification, distinguishable from a normal verdict), while a normally
returning judge still produces its verdict (no false-positive timeout).

Two layers are exercised:

1. :func:`run_rubric_graders` directly with an injected ``judge_invoke`` whose
   ``async for`` never ends (mirrors the substitution seam in
   ``test_grader_rubric.py`` without touching that suite).
2. The harness ``_validate`` -> rubric path under default-on config, proving
   the harness actually threads the resolved ``RUBRIC_JUDGE`` ceiling through
   to the runner (the gap criterion #3 forecloses).
"""

from __future__ import annotations

import asyncio
import sys
import time
from collections.abc import AsyncIterator, Awaitable, Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest
from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Attempt,
    CommandGrader,
    HarnessConfig,
    HarnessOutcome,
    InMemoryStore,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    Outcome,
    RubricGrader,
    Status,
    Task,
    run_task,
)
from flywheel_core.deadline_config import (
    DEFAULT_RUBRIC_JUDGE_SECONDS,
    DeadlineClass,
    DeadlineConfig,
)
from flywheel_core.envelope import Intent, ValidEnvelope
from flywheel_core.grader_rubric import (
    CLOSING_FENCE,
    OPENING_FENCE,
    RubricJudgeError,
    run_rubric_graders,
)

# A tiny ceiling keeps the real wall cost of each timeout test in the
# millisecond range; the bound is real elapsed event-loop time (the primitive
# reads ``loop.time()``), not a fake clock. A generous upper bound on observed
# wall time still proves "bounded" without flaking under load.
_TINY_CEILING = 0.05
_WALL_BUDGET = 10.0


# --- Helpers ---------------------------------------------------------------


def _wrap(payload: str) -> str:
    return f"{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}"


def _bootstrap(store: InMemoryStore, run_id: str = "r1") -> None:
    if store.load_lifecycle(run_id) is None:
        store.create_lifecycle(Lifecycle(task_id="t", run_id=run_id))
    if store.load_attempt(run_id, 1) is None:
        store.save_attempt(
            run_id,
            Attempt(
                number=1,
                started_at=datetime(2024, 1, 1, tzinfo=timezone.utc),
                run_id=run_id,
            ),
        )


async def _never_ending_judge(
    prompt: str, grader: RubricGrader, worktree: Path | str
) -> str:
    """A judge whose ``async for`` never terminates, yet keeps yielding.

    This is the adversary criterion #3 names: a steadily-streaming judge that
    an idle/silence watchdog would never trip. Only a wall-clock bound can cut
    it off.
    """

    async def _forever() -> AsyncIterator[str]:
        while True:
            yield "still thinking"
            await asyncio.sleep(0.001)

    chunks: list[str] = []
    async for chunk in _forever():
        chunks.append(chunk)
    return "".join(chunks)  # pragma: no cover - never reached


async def _instant_pass_judge(
    prompt: str, grader: RubricGrader, worktree: Path | str
) -> str:
    return _wrap('{"passed": true, "summary": "looks right"}')


# --- run_rubric_graders: direct deadline behavior --------------------------


class TestRubricJudgeDeadlineRunner:
    def test_rubric_judge_deadline_cancels_never_ending_stream(self) -> None:
        # A judge whose async-for never ends is cancelled by the wall-clock
        # ceiling and surfaced as a RubricJudgeError (judge-infra failure),
        # in bounded wall time -- not stalled forever.
        store = InMemoryStore()
        _bootstrap(store)
        task = Task(
            goal="g",
            graders=[RubricGrader(assertions=["a"], name="r-timeout")],
        )

        started = time.monotonic()
        with pytest.raises(RubricJudgeError) as excinfo:
            asyncio.run(
                run_rubric_graders(
                    task,
                    store,
                    run_id="r1",
                    attempt_number=1,
                    transcript="agent did stuff",
                    worktree="/tmp/wt",
                    command_passed=True,
                    transcript_passed=True,
                    judge_invoke=_never_ending_judge,
                    judge_ceiling_seconds=_TINY_CEILING,
                )
            )
        elapsed = time.monotonic() - started

        # Bounded wall time: roughly ceiling + epsilon, well under the budget.
        assert elapsed < _WALL_BUDGET
        # Distinguishable as a deadline timeout, attributed to the grader.
        assert excinfo.value.grader_name == "r-timeout"
        assert "deadline" in excinfo.value.reason
        # No grader row is persisted for a judge-infra failure.
        assert store.list_grader_results("r1", 1) == []

    def test_rubric_judge_deadline_normal_verdict_not_tripped(self) -> None:
        # No false positive: a judge that returns promptly still produces its
        # verdict even though a (generous) ceiling is in force.
        store = InMemoryStore()
        _bootstrap(store)
        task = Task(
            goal="g",
            graders=[RubricGrader(assertions=["a"], name="r-ok")],
        )

        results = asyncio.run(
            run_rubric_graders(
                task,
                store,
                run_id="r1",
                attempt_number=1,
                transcript="agent did stuff",
                worktree="/tmp/wt",
                command_passed=True,
                transcript_passed=True,
                judge_invoke=_instant_pass_judge,
                judge_ceiling_seconds=DEFAULT_RUBRIC_JUDGE_SECONDS,
            )
        )

        assert len(results) == 1
        assert results[0].passed is True
        assert results[0].payload["summary"] == "looks right"

    def test_rubric_judge_deadline_none_ceiling_is_unbounded_optout(
        self,
    ) -> None:
        # The unbounded opt-out (operator override resolving to None) leaves
        # the invocation unwrapped -- a prompt judge still returns its verdict.
        store = InMemoryStore()
        _bootstrap(store)
        task = Task(
            goal="g",
            graders=[RubricGrader(assertions=["a"], name="r-unbounded")],
        )

        results = asyncio.run(
            run_rubric_graders(
                task,
                store,
                run_id="r1",
                attempt_number=1,
                transcript="agent did stuff",
                worktree="/tmp/wt",
                command_passed=True,
                transcript_passed=True,
                judge_invoke=_instant_pass_judge,
                judge_ceiling_seconds=None,
            )
        )

        assert len(results) == 1
        assert results[0].passed is True

    def test_rubric_judge_deadline_default_ceiling_is_non_null(self) -> None:
        # Default-on: a default-constructed HarnessConfig resolves a finite,
        # non-null RUBRIC_JUDGE ceiling -- the guard against wiring the bound
        # but defaulting it off so it only fires under an override.
        from math import isfinite

        ceiling = HarnessConfig().deadlines.for_class(
            DeadlineClass.RUBRIC_JUDGE
        )
        assert ceiling is not None
        assert ceiling > 0
        assert isfinite(ceiling)


# --- harness _validate -> rubric path --------------------------------------


def _assistant(text: str = "ok") -> AssistantMessage:
    return AssistantMessage(
        content=[TextBlock(text=text)],
        model="claude-test",
        stop_reason="end_turn",
        session_id="sess-1",
        usage=None,
    )


def _result_msg() -> ResultMessage:
    return ResultMessage(
        subtype="success",
        duration_ms=10,
        duration_api_ms=8,
        is_error=False,
        num_turns=1,
        session_id="sess-1",
        stop_reason="end_turn",
        total_cost_usd=0.01,
        usage=None,
    )


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.01,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess-1",
    )


def _verify_iteration() -> IterationResult:
    return IterationResult(
        transcript="",
        messages=(_assistant(), _result_msg()),
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=_signals(),
        failure=None,
    )


def _single_verify_invoker() -> Callable[
    [InvocationRequest], Awaitable[IterationResult]
]:
    results = [_verify_iteration()]

    async def _invoker(request: InvocationRequest) -> IterationResult:
        result = results.pop(0)
        if request.on_message is not None:
            for msg in result.messages:
                request.on_message(msg)
        return result

    return _invoker


def _run(coro: Any) -> HarnessOutcome:
    return asyncio.run(coro)


class TestRubricJudgeDeadlineHarness:
    @pytest.fixture(autouse=True)
    def _worktree(self, tmp_path: Path) -> None:
        self._wt = str(tmp_path)

    def test_rubric_judge_deadline_via_harness_surfaces_judge_infra_timeout(
        self,
    ) -> None:
        # Criterion #3: the harness runs the rubric judge under a finite,
        # default-on ceiling. With a never-ending judge stream the call is
        # cancelled in bounded wall time and routed to the judge-infra
        # INTERNAL_ERROR containment path (classification rubric_judge_error),
        # NOT left stalling VALIDATING. A small override on the RUBRIC_JUDGE
        # class only keeps the wall cost tiny; the other classes keep their
        # finite defaults.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="ok",
                ),
                RubricGrader(assertions=["a"], name="r-judge"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-deadline")
        config = HarnessConfig(
            max_retries=0,
            worktree=self._wt,
            rubric_judge_invoke=_never_ending_judge,
            deadlines=DeadlineConfig(rubric_judge_seconds=_TINY_CEILING),
        )

        started = time.monotonic()
        outcome = _run(
            run_task(
                task,
                lifecycle,
                store,
                sink=sink,
                config=config,
                invoke=_single_verify_invoker(),
            )
        )
        elapsed = time.monotonic() - started

        assert elapsed < _WALL_BUDGET
        # INTERNAL_ERROR with max_retries=0 exhausts immediately -> FAILED.
        assert outcome.lifecycle.status == Status.FAILED
        attempt = outcome.attempts[0]
        assert attempt.outcome == Outcome.INTERNAL_ERROR
        assert "rubric judge failed" in attempt.error
        assert "deadline" in attempt.error

        crash_events = [
            e
            for e in sink.events(lifecycle.run_id)
            if e.kind == "harness.crash"
        ]
        assert len(crash_events) == 1
        assert (
            crash_events[0].payload["classification"] == "rubric_judge_error"
        )
        assert crash_events[0].payload["grader_name"] == "r-judge"
        assert "deadline" in crash_events[0].payload["reason"]

    def test_rubric_judge_deadline_via_harness_normal_verdict_reaches_done(
        self,
    ) -> None:
        # No false positive through the harness: under the DEFAULT config
        # (finite, non-null ceiling) a prompt judge still reaches DONE.
        store = InMemoryStore()
        sink = _ListSink()
        task = Task(
            goal="g",
            graders=[
                CommandGrader(
                    run=f"{sys.executable} -c 'raise SystemExit(0)'",
                    name="ok",
                ),
                RubricGrader(assertions=["a"], name="r-judge"),
            ],
        )
        lifecycle = Lifecycle(task_id="t1", run_id="run-rubric-deadline-ok")
        config = HarnessConfig(
            worktree=self._wt,
            rubric_judge_invoke=_instant_pass_judge,
        )
        assert config.deadlines.rubric_judge_seconds is not None

        outcome = _run(
            run_task(
                task,
                lifecycle,
                store,
                sink=sink,
                config=config,
                invoke=_single_verify_invoker(),
            )
        )

        assert outcome.lifecycle.status == Status.DONE


class _ListSink:
    """Minimal in-memory TelemetrySink capturing harness.* records."""

    def __init__(self) -> None:
        self.records: list[Any] = []

    def append_telemetry(self, record: Any) -> None:
        self.records.append(record)

    def events(self, run_id: str) -> list[Any]:
        return [
            r
            for r in self.records
            if r.run_id == run_id and r.kind.startswith("harness.")
        ]
