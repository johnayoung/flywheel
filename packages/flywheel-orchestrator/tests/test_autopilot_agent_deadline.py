"""Wall-clock deadline on the autopilot discovery/authoring agent calls.

Spec 00066 criterion #6: when the autopilot discovery or authoring agent call is
issued with no operator override, the agent stream runs under a finite
wall-clock ceiling, so a stalled SDK stream is cancelled and surfaces a timeout
outcome rather than parking the daemon, and the resolved default ceiling is
non-null.

These tests inject an invoke-iteration whose underlying stream never yields a
terminal message (the gaming move ``max_turns`` -- a turn budget -- cannot
catch) and assert the call is cancelled with a distinguishable timeout outcome
in bounded wall time. A tiny ceiling keeps the real wall cost trivial; the bound
is real elapsed event-loop time, not a fake-clock comparison.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable
from math import isfinite
from pathlib import Path
from typing import TypeVar

from flywheel_core.deadline import DeadlineExceeded
from flywheel_core.deadline_config import (
    DeadlineClass,
    DeadlineConfig,
    resolve_deadlines,
)
from flywheel_core.envelope import parse_envelope
from flywheel_core.invoker import InvocationSignals, IterationResult

from flywheel_orchestrator._autopilot import (
    Finding,
    Tier,
    author_findings,
    build_repo_invoker,
    build_single_session_runner,
    run_single_session_discovery,
)

_T = TypeVar("_T")


async def _await(awaitable: Awaitable[_T]) -> _T:
    """Adapt a bare ``Awaitable`` (the invoker/runner seam type) for ``asyncio.run``."""
    return await awaitable


# A small ceiling: the deadline must fire well within the test's wall budget.
_TINY_CEILING = 0.05
# A generous upper bound on how long a 0.05s-ceiling call may take to be cut
# off; far below any real agent call, far above the ceiling + scheduling slack.
_WALL_BUDGET = 5.0


def _completed_iteration(transcript: str) -> IterationResult:
    """A drained iteration result, as a normally-terminating invoke would yield."""
    return IterationResult(
        transcript=transcript,
        messages=(),
        envelope=parse_envelope(transcript),
        signals=InvocationSignals(
            stop_reason="end_turn",
            num_turns=1,
            total_cost_usd=None,
            result_is_error=False,
            result_subtype="success",
            api_error_status=None,
            session_id="sess",
        ),
    )


async def _never_returning_invoke(**_kwargs: object) -> IterationResult:
    """An invoke-iteration whose stream never yields a terminal message."""
    await asyncio.Future()
    raise RuntimeError("unreachable")  # pragma: no cover


async def _streaming_forever_invoke(**_kwargs: object) -> IterationResult:
    """An invoke-iteration that streams forever without ever terminating.

    Models the gaming move criterion #6 forecloses: a stream that keeps
    producing (never idle) but never spends a terminal "turn", so a turn budget
    and an idle/silence watchdog both miss it. Only a wall-clock bound cuts it.
    """
    while True:
        await asyncio.sleep(0.005)


def _finding() -> Finding:
    return Finding(
        id="f1",
        tier=Tier.TEST_COVERAGE,
        title="add coverage for the parser",
        detail="the parser module has no tests",
        evidence=("src/parser.py:1",),
        urgency=3,
        importance=5,
        effort=2,
    )


class TestAutopilotAgentDeadline:
    """Criterion #6: the autopilot agent call is wall-clock bounded by default."""

    def test_autopilot_agent_deadline_default_ceiling_is_non_null(self) -> None:
        # Default-on: with no operator override the resolved autopilot agent
        # ceiling is finite and non-null -- the guard against faking
        # "default-on" by requiring an operator to set it.
        for config in (DeadlineConfig(), resolve_deadlines()):
            ceiling = config.for_class(DeadlineClass.AUTOPILOT_AGENT)
            assert ceiling is not None
            assert ceiling > 0
            assert isfinite(ceiling)

    def test_autopilot_agent_deadline_cancels_authoring_stall(self) -> None:
        # The authoring agent call is bounded: a never-returning invoke is
        # cancelled and the cycle records a distinguishable timeout drop rather
        # than parking. The wall cost stays tiny.
        invoker = build_repo_invoker(
            Path("/repo"),
            deadline_seconds=_TINY_CEILING,
            invoke=_never_returning_invoke,
        )
        started = time.monotonic()
        result = asyncio.run(
            author_findings([_finding()], repo_root=Path("/repo"), invoker=invoker)
        )
        elapsed = time.monotonic() - started

        assert elapsed < _WALL_BUDGET
        assert not result.emitted
        assert len(result.dropped) == 1
        # The drop reason names the timeout outcome (DeadlineExceeded), so the
        # stall is distinguishable from an ordinary authoring failure.
        assert "DeadlineExceeded" in result.dropped[0].reason

    def test_autopilot_agent_deadline_cancels_discovery_stall(self) -> None:
        # The discovery session agent call is bounded: a never-returning invoke
        # is cancelled with DeadlineExceeded in bounded wall time, so the
        # single-session discovery cycle cannot wedge the daemon.
        runner = build_single_session_runner(
            Path("/repo"),
            deadline_seconds=_TINY_CEILING,
            invoke=_never_returning_invoke,
        )
        started = time.monotonic()
        raised = False
        try:
            asyncio.run(
                run_single_session_discovery(
                    repo_root=Path("/repo"), session_runner=runner
                )
            )
        except DeadlineExceeded as exc:
            raised = True
            assert exc.ceiling_seconds == _TINY_CEILING
        elapsed = time.monotonic() - started

        assert raised, "discovery stall must surface a DeadlineExceeded timeout"
        assert elapsed < _WALL_BUDGET

    def test_autopilot_agent_deadline_cuts_off_streaming_stream(self) -> None:
        # D-2: the bound is wall-clock, not silence/turns. A stream that
        # produces forever without ever terminating is still cut off once the
        # ceiling passes -- the turn-budget gaming move does not escape it.
        runner = build_single_session_runner(
            Path("/repo"),
            deadline_seconds=_TINY_CEILING,
            invoke=_streaming_forever_invoke,
        )
        started = time.monotonic()
        raised = False
        try:
            asyncio.run(_await(runner("prompt")))
        except DeadlineExceeded:
            raised = True
        elapsed = time.monotonic() - started

        assert raised, "a forever-streaming agent call must be cut off"
        assert elapsed < _WALL_BUDGET

    def test_autopilot_agent_deadline_normal_completion_not_tripped(self) -> None:
        # No false positive: under the finite default ceiling an agent call that
        # returns promptly completes normally -- the deadline wrap is
        # transparent when the invoke terminates in time.
        async def _completing_invoke(**_kwargs: object) -> IterationResult:
            return _completed_iteration("all done")

        runner = build_single_session_runner(
            Path("/repo"), invoke=_completing_invoke
        )
        result = asyncio.run(_await(runner("prompt")))
        assert result.transcript == "all done"

        invoker = build_repo_invoker(Path("/repo"), invoke=_completing_invoke)
        transcript = asyncio.run(_await(invoker("prompt")))
        assert transcript == "all done"
