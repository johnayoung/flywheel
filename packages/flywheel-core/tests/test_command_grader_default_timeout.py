"""Spec 00066 criterion #4: the harness threads the default-on COMMAND_GRADER
wall-clock ceiling from config into ``run_command_graders``.

A grader command that hangs must be SIGKILLed and recorded with
``payload['termination'] == 'timeout'`` under the *default* config -- not block
validation forever. These tests assert through the harness's default call path
(``run_task`` -> ``_validate`` -> ``run_command_graders``), not the runner
parameter in isolation. The runner's own timeout mechanics are locked by
``test_grader_command.py`` and are not re-tested here.
"""

from __future__ import annotations

import asyncio
import sys
import time
from collections.abc import Awaitable, Callable
from typing import Any

import flywheel_core.harness as harness_module
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
from flywheel_core.deadline_config import (
    DEFAULT_COMMAND_GRADER_SECONDS,
    DeadlineClass,
    resolve_deadlines,
)
from flywheel_core.envelope import Intent, ValidEnvelope

import pytest


# --- Minimal harness driver ------------------------------------------------


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess-1",
    )


def _verify_invoker() -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    """A one-shot invoker that immediately emits a ``verify`` envelope.

    Returning ``verify`` drives the harness straight into ``_validate`` so the
    command graders run under the real default call path.
    """

    async def _invoke(request: InvocationRequest) -> IterationResult:
        return IterationResult(
            transcript="",
            messages=(),
            envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
            signals=_signals(),
        )

    return _invoke


def _run(coro: Awaitable[HarnessOutcome]) -> HarnessOutcome:
    return asyncio.run(coro)  # type: ignore[arg-type]


def _hanging_task(run_id: str) -> tuple[Task, Lifecycle]:
    # A grader that blocks far longer than any test timeout: only the
    # harness-applied wall-clock ceiling can end it.
    task = Task(
        goal="g",
        graders=[
            CommandGrader(
                run=f"{sys.executable} -c 'import time; time.sleep(3600)'",
                name="hangs",
            )
        ],
    )
    return task, Lifecycle(task_id="t1", run_id=run_id)


# --- Tests -----------------------------------------------------------------


def test_default_config_passes_finite_command_grader_ceiling(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Under the default config the harness hands the runner the finite,
    non-null COMMAND_GRADER ceiling -- the default call path is bounded,
    never the unbounded ``per_grader_timeout_seconds=None`` wait."""

    captured: dict[str, Any] = {}
    real = getattr(harness_module, "run_command_graders")

    def _spy(*args: Any, **kwargs: Any) -> Any:
        captured["per_grader_timeout_seconds"] = kwargs.get(
            "per_grader_timeout_seconds", "ABSENT"
        )
        return real(*args, **kwargs)

    monkeypatch.setattr(harness_module, "run_command_graders", _spy)

    store = InMemoryStore()
    task = Task(
        goal="g",
        graders=[CommandGrader(run=f"{sys.executable} -c 'pass'", name="fast")],
    )
    lifecycle = Lifecycle(task_id="t1", run_id="run-default-ceiling")

    outcome = _run(
        run_task(
            task,
            lifecycle,
            store,
            config=HarnessConfig(),
            invoke=_verify_invoker(),
        )
    )

    assert captured["per_grader_timeout_seconds"] == DEFAULT_COMMAND_GRADER_SECONDS
    # A fast grader under the default ceiling still passes: no false-positive
    # timeout from threading the bound through.
    assert outcome.lifecycle.status == Status.DONE
    rows = store.list_grader_results(lifecycle.run_id, 1)
    assert [r.passed for r in rows] == [True]
    assert rows[0].payload["termination"] == "exited"


def test_hanging_grader_is_killed_and_recorded_timeout() -> None:
    """A grader whose ``run`` blocks is SIGKILLed and recorded with
    ``termination == 'timeout'`` in bounded wall time -- exercised through the
    harness, with a small operator ceiling so the test stays fast."""

    store = InMemoryStore()
    task, lifecycle = _hanging_task("run-hang-killed")
    config = HarnessConfig(
        deadlines=resolve_deadlines({DeadlineClass.COMMAND_GRADER: 0.5})
    )

    start = time.monotonic()
    outcome = _run(
        run_task(
            task, lifecycle, store, config=config, invoke=_verify_invoker()
        )
    )
    elapsed = time.monotonic() - start

    # Bounded: the 3600s sleep must not have run to completion.
    assert elapsed < 60.0
    assert outcome.lifecycle.status != Status.DONE
    rows = store.list_grader_results(lifecycle.run_id, 1)
    assert len(rows) == 1
    assert rows[0].passed is False
    assert rows[0].payload["termination"] == "timeout"


def test_operator_optout_passes_unbounded_through_harness(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An operator opt-out (COMMAND_GRADER override of ``0`` -> unbounded)
    is honored: the harness passes ``per_grader_timeout_seconds=None`` to the
    runner, restoring the unbounded wait."""

    captured: dict[str, Any] = {}
    real = getattr(harness_module, "run_command_graders")

    def _spy(*args: Any, **kwargs: Any) -> Any:
        captured["per_grader_timeout_seconds"] = kwargs.get(
            "per_grader_timeout_seconds", "ABSENT"
        )
        return real(*args, **kwargs)

    monkeypatch.setattr(harness_module, "run_command_graders", _spy)

    store = InMemoryStore()
    task = Task(
        goal="g",
        graders=[CommandGrader(run=f"{sys.executable} -c 'pass'", name="fast")],
    )
    lifecycle = Lifecycle(task_id="t1", run_id="run-optout")
    config = HarnessConfig(
        deadlines=resolve_deadlines({DeadlineClass.COMMAND_GRADER: 0})
    )

    _run(
        run_task(
            task, lifecycle, store, config=config, invoke=_verify_invoker()
        )
    )

    assert captured["per_grader_timeout_seconds"] is None
