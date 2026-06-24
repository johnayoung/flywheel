"""Held-out oracle for spec 00044 G2 — the plain-dict ``IterationResult.usage``
path.

RED until G2 lands. An invoker that produces no SDK ``Message`` objects (the
container backend driving the agent CLI in stream-json mode) carries the
iteration's token breakdown on ``IterationResult.usage``. The harness uses it
verbatim for the attempt rollup and the per-run token ceiling (00042); when
``usage`` is ``None`` (every SDK-backed invoker) the breakdown is derived from
``messages`` exactly as before. Do not weaken assertions.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
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


def _result(
    *,
    messages: tuple[object, ...],
    usage: dict[str, int] | None,
) -> IterationResult:
    return IterationResult(
        transcript="",
        messages=messages,  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=_signals(),
        failure=None,
        usage=usage,
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


PASS = CommandGrader(run="true")


def test_usage_dict_rolls_into_attempt_without_messages() -> None:
    store = InMemoryStore()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-usage-1")
    invoke = _scripted(
        [
            _result(
                messages=(),
                usage={
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "cache_creation_input_tokens": 5,
                    "cache_read_input_tokens": 3,
                },
            )
        ]
    )
    out = _run(run_task(task, lc, store, invoke=invoke, config=HarnessConfig()))
    assert out.lifecycle.status == Status.DONE
    # No SDK messages, yet the attempt's token counters reflect ``usage``.
    assert out.attempts[0].total_tokens == 128
    assert out.attempts[0].input_tokens == 100


def test_usage_dict_trips_token_ceiling() -> None:
    store, sink = InMemoryStore(), _Sink()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-usage-2")
    invoke = _scripted([_result(messages=(), usage={"input_tokens": 2000})])
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
    # The message-less usage feeds the 00042 token ceiling end to end.
    assert out.lifecycle.status == Status.FAILED
    breaches = [
        r
        for r in sink.records
        if r.kind == "harness.budget_ceiling_breached"
        and r.payload.get("ceiling") == "tokens"
    ]
    assert len(breaches) == 1


def test_messages_path_unchanged_when_usage_none() -> None:
    store = InMemoryStore()
    task = Task(goal="g", graders=[PASS])
    lc = Lifecycle(task_id="t", run_id="run-usage-3")
    messages = (
        AssistantMessage(
            content=[TextBlock(text="ok")],
            model="claude-test",
            stop_reason="end_turn",
            session_id="s",
            usage={"input_tokens": 42},
        ),
        ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=1,
            session_id="s",
            stop_reason="end_turn",
            total_cost_usd=0.0,
            usage=None,
        ),
    )
    invoke = _scripted([_result(messages=messages, usage=None)])
    out = _run(run_task(task, lc, store, invoke=invoke, config=HarnessConfig()))
    assert out.lifecycle.status == Status.DONE
    # usage=None → tokens derived from messages, exactly as before G2.
    assert out.attempts[0].input_tokens == 42
