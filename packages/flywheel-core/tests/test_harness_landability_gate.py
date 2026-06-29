"""Harness-level tests for the landable-change gate (spec 00061).

The orchestrator owns the gate but core must stay git-unaware: it consults
an opaque, zero-argument :attr:`HarnessConfig.landability_gate` callback at the
verify-passed ``VALIDATING -> DONE`` boundary. ``None`` means landable (proceed
to ``DONE`` byte-identically); a non-empty *reason* string means the finished
change is not landable, so the harness must NOT land it as a success -- it
routes to ``FAILED_VALIDATION`` so the existing ``max_retries`` machinery
re-drives the run against the same base, ending terminal ``FAILED`` with the
reason recorded once the budget is exhausted (never an infinite loop, never a
``DONE`` for an unlandable change).
"""

from __future__ import annotations

import asyncio
import sys
from collections.abc import Awaitable, Callable, Coroutine
from typing import Any

from claude_agent_sdk import (
    AssistantMessage,
    ResultMessage,
    TextBlock,
)

from flywheel_core import (
    CommandGrader,
    HarnessConfig,
    HarnessOutcome,
    InMemoryStore,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    Outcome,
    Status,
    Task,
    run_task,
)
from flywheel_core.envelope import Intent, ValidEnvelope


# --- Helpers (mirrors of test_harness.py, kept self-contained) ------------


def _make_signals(**overrides: Any) -> InvocationSignals:
    defaults: dict[str, Any] = {
        "stop_reason": "end_turn",
        "num_turns": 1,
        "total_cost_usd": 0.01,
        "result_is_error": False,
        "result_subtype": "success",
        "api_error_status": None,
        "session_id": "sess-1",
    }
    defaults.update(overrides)
    return InvocationSignals(**defaults)


def _assistant() -> AssistantMessage:
    return AssistantMessage(
        content=[TextBlock(text="ok")],
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


def _iteration() -> IterationResult:
    return IterationResult(
        transcript="",
        messages=(_assistant(), _result_msg()),
        envelope=ValidEnvelope(intent=Intent.VERIFY, reason="all done"),
        signals=_make_signals(),
        failure=None,
    )


def _scripted_invoker(
    count: int,
) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    """An invoker yielding ``count`` identical verify iterations in order."""
    results = [_iteration() for _ in range(count)]
    calls: list[InvocationRequest] = []

    async def _invoker(request: InvocationRequest) -> IterationResult:
        calls.append(request)
        result = results.pop(0)
        if request.on_message is not None:
            for msg in result.messages:
                request.on_message(msg)
        return result

    _invoker.calls = calls  # type: ignore[attr-defined]
    return _invoker


def _run(coro: Coroutine[Any, Any, HarnessOutcome]) -> HarnessOutcome:
    return asyncio.run(coro)


def _passing_task() -> Task:
    return Task(
        goal="g",
        graders=[
            CommandGrader(
                run=f"{sys.executable} -c 'import sys; sys.exit(0)'",
                name="ok",
            )
        ],
    )


# --- Tests ----------------------------------------------------------------


def test_non_landable_verdict_blocks_done_and_retries() -> None:
    """Criterion #1/#2: graders pass but the change is not landable, so the
    first attempt does NOT land as DONE -- it routes to FAILED_VALIDATION and
    the retry budget re-drives it. The second attempt (now landable) lands."""
    store = InMemoryStore()
    task = _passing_task()
    lifecycle = Lifecycle(task_id="t1", run_id="run-redrive")
    invoke = _scripted_invoker(2)

    calls = {"n": 0}

    def gate() -> str | None:
        calls["n"] += 1
        # Not landable on the first finished run; landable on the second.
        return "uncommitted tree" if calls["n"] == 1 else None

    config = HarnessConfig(max_retries=1, landability_gate=gate)

    outcome = _run(
        run_task(task, lifecycle, store, config=config, invoke=invoke)
    )

    assert outcome.lifecycle.status == Status.DONE
    assert outcome.lifecycle.retries == 1
    assert len(outcome.attempts) == 2
    assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED
    assert "uncommitted tree" in outcome.attempts[0].error
    assert outcome.attempts[1].outcome == Outcome.SUCCEEDED
    assert calls["n"] == 2


def test_never_landable_ends_failed_with_recorded_reason() -> None:
    """Criterion #4: a run whose change is never landable exhausts the retry
    budget and ends terminal FAILED (never DONE) with the reason recorded --
    never an infinite loop."""
    store = InMemoryStore()
    task = _passing_task()
    lifecycle = Lifecycle(task_id="t1", run_id="run-failed")
    invoke = _scripted_invoker(2)

    def gate() -> str | None:
        return "no commits on branch"

    config = HarnessConfig(max_retries=1, landability_gate=gate)

    outcome = _run(
        run_task(task, lifecycle, store, config=config, invoke=invoke)
    )

    assert outcome.lifecycle.status == Status.FAILED
    assert outcome.lifecycle.retries == 1
    assert len(outcome.attempts) == 2
    assert all(a.outcome == Outcome.VALIDATION_FAILED for a in outcome.attempts)
    assert "no commits on branch" in outcome.lifecycle.error


def test_never_landable_zero_budget_fails_immediately() -> None:
    """Criterion #4 with no retry budget: the single non-landable run is
    terminal FAILED, reason recorded, exactly one attempt."""
    store = InMemoryStore()
    task = _passing_task()
    lifecycle = Lifecycle(task_id="t1", run_id="run-failed-0")
    invoke = _scripted_invoker(1)

    config = HarnessConfig(
        max_retries=0, landability_gate=lambda: "empty diff"
    )

    outcome = _run(
        run_task(task, lifecycle, store, config=config, invoke=invoke)
    )

    assert outcome.lifecycle.status == Status.FAILED
    assert len(outcome.attempts) == 1
    assert outcome.attempts[0].outcome == Outcome.VALIDATION_FAILED
    assert "empty diff" in outcome.lifecycle.error


def test_landable_verdict_lands_done() -> None:
    """Criterion #5: a landable verdict (gate returns None) lands DONE on the
    first attempt -- the post-run path is unchanged."""
    store = InMemoryStore()
    task = _passing_task()
    lifecycle = Lifecycle(task_id="t1", run_id="run-landable")
    invoke = _scripted_invoker(1)

    config = HarnessConfig(max_retries=1, landability_gate=lambda: None)

    outcome = _run(
        run_task(task, lifecycle, store, config=config, invoke=invoke)
    )

    assert outcome.lifecycle.status == Status.DONE
    assert outcome.lifecycle.retries == 0
    assert len(outcome.attempts) == 1
    assert outcome.attempts[0].outcome == Outcome.SUCCEEDED


def test_no_gate_configured_is_byte_identical() -> None:
    """Criterion #3: with no gate (the default ``None``) a verify-passed run
    lands DONE on its first attempt exactly as before -- no spurious retry."""
    store = InMemoryStore()
    task = _passing_task()
    lifecycle = Lifecycle(task_id="t1", run_id="run-nogate")
    invoke = _scripted_invoker(1)

    config = HarnessConfig(max_retries=1)
    assert config.landability_gate is None

    outcome = _run(
        run_task(task, lifecycle, store, config=config, invoke=invoke)
    )

    assert outcome.lifecycle.status == Status.DONE
    assert outcome.lifecycle.retries == 0
    assert len(outcome.attempts) == 1
    assert outcome.attempts[0].outcome == Outcome.SUCCEEDED


def test_gate_skipped_when_graders_fail() -> None:
    """The gate is consulted only on the would-be-DONE path: a grader failure
    routes to FAILED_VALIDATION via its own path and never calls the gate."""
    store = InMemoryStore()
    task = Task(
        goal="g",
        graders=[
            CommandGrader(
                run=f"{sys.executable} -c 'raise SystemExit(1)'",
                name="fail",
            )
        ],
    )
    lifecycle = Lifecycle(task_id="t1", run_id="run-gradersfail")
    invoke = _scripted_invoker(1)

    calls = {"n": 0}

    def gate() -> str | None:
        calls["n"] += 1
        return None

    config = HarnessConfig(max_retries=0, landability_gate=gate)

    outcome = _run(
        run_task(task, lifecycle, store, config=config, invoke=invoke)
    )

    assert outcome.lifecycle.status == Status.FAILED
    assert calls["n"] == 0  # gate never consulted when graders fail
