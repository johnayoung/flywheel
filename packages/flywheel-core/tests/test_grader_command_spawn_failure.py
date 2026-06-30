"""Spawn-failure classification for command graders.

A command grader whose subprocess never starts (``Popen`` raised
``OSError`` -- a bad cwd, an exec failure, resource exhaustion) is an
infrastructure failure, not the code under test asserting false. It must be
recorded with a distinct ``termination`` discriminator and routed by the
harness to the retryable ``INTERNAL_ERROR`` class -- distinct from a grader
that ran and exited non-zero, which stays ``FAILED_VALIDATION``.

The two classes must not merge: a spawn failure is retryable infra; a
ran-and-failed grader is a validation failure that the holdout suite pins.
"""

from __future__ import annotations

import asyncio
import sys
from collections.abc import Awaitable, Callable
from datetime import datetime, timezone

from claude_agent_sdk import (
    AssistantMessage,
    Message,
    ResultMessage,
    TextBlock,
)

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
    Status,
    Task,
    run_command_graders,
    run_task,
)
from flywheel_core.envelope import Intent, ValidEnvelope
from flywheel_core.store_protocols import TelemetryRecord

# A directory guaranteed not to exist. ``Popen(cwd=...)`` raises
# ``FileNotFoundError`` (an ``OSError`` subclass) when it cannot chdir into
# this path, which is exactly the spawn-failure infra fault under test.
_NONEXISTENT_CWD = "/no/such/dir/flywheel-spawn-failure-fixture"


# --- grader_command unit level --------------------------------------------


def _attempt_run_id(store: InMemoryStore, run_id: str = "r1") -> None:
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


def test_spawn_failure_records_distinct_termination() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = Task(goal="g", graders=[CommandGrader(run="true", name="boom")])

    results = run_command_graders(
        task,
        store,
        run_id="r1",
        attempt_number=1,
        cwd=_NONEXISTENT_CWD,
    )

    assert len(results) == 1
    row = results[0]
    assert row.passed is False
    assert row.grader_type == "command"
    assert row.grader_name == "boom"
    # The distinguisher: a spawn failure is its own termination class, not an
    # ``exited`` non-zero. Collapsing it into ``exited`` would let the harness
    # misclassify it as a validation failure.
    assert row.payload["termination"] == "spawn_failure"
    assert row.payload["exit_code"] is None
    assert row.payload["spawn_error"]


def test_spawn_failure_aborts_later_command_graders() -> None:
    store = InMemoryStore()
    _attempt_run_id(store)
    task = Task(
        goal="g",
        graders=[
            CommandGrader(run="true", name="first"),
            CommandGrader(run="echo never", name="skipped"),
        ],
    )

    results = run_command_graders(
        task,
        store,
        run_id="r1",
        attempt_number=1,
        cwd=_NONEXISTENT_CWD,
    )

    # The first grader fails to spawn; later command graders are skipped, just
    # like any other first-failure abort.
    assert [r.grader_name for r in results] == ["first"]
    assert results[0].payload["termination"] == "spawn_failure"


# --- harness classification level -----------------------------------------


def _make_signals(**overrides: object) -> InvocationSignals:
    defaults: dict[str, object] = {
        "stop_reason": "end_turn",
        "num_turns": 1,
        "total_cost_usd": 0.01,
        "result_is_error": False,
        "result_subtype": "success",
        "api_error_status": None,
        "session_id": "sess-1",
    }
    defaults.update(overrides)
    return InvocationSignals(**defaults)  # type: ignore[arg-type]


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


def _verify_invoker() -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    messages: tuple[Message, ...] = (_assistant(), _result_msg())

    async def _invoker(request: InvocationRequest) -> IterationResult:
        result = IterationResult(
            transcript="",
            messages=messages,
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_make_signals(),
            failure=None,
        )
        if request.on_message is not None:
            for msg in messages:
                try:
                    request.on_message(msg)
                except Exception:  # noqa: BLE001 - mirror invoker contract
                    pass
        return result

    return _invoker


class _ListSink:
    def __init__(self) -> None:
        self.records: list[TelemetryRecord] = []

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self.records.append(record)

    def events(self, run_id: str) -> list[TelemetryRecord]:
        return [
            r
            for r in self.records
            if r.run_id == run_id and r.kind.startswith("harness.")
        ]


def _run(coro: Awaitable[HarnessOutcome]) -> HarnessOutcome:
    return asyncio.run(coro)  # type: ignore[arg-type]


def test_spawn_failure_routes_to_internal_error() -> None:
    store = InMemoryStore()
    sink = _ListSink()
    task = Task(goal="g", graders=[CommandGrader(run="true", name="boom")])
    lifecycle = Lifecycle(task_id="t1", run_id="run-spawn-failure")
    # Driving the grader subprocess into a nonexistent cwd makes Popen raise,
    # exercising the real spawn-failure guard end-to-end.
    config = HarnessConfig(worktree=_NONEXISTENT_CWD, max_retries=1)

    outcome = _run(
        run_task(
            task,
            lifecycle,
            store,
            sink=sink,
            config=config,
            invoke=_verify_invoker(),
        )
    )

    attempt = outcome.attempts[0]
    # The infra class, not a validation failure: outcome is INTERNAL_ERROR.
    assert attempt.outcome == Outcome.INTERNAL_ERROR
    # INTERNAL_ERROR is a retry source, so with a budget of 1 the lifecycle
    # retries once (consuming the budget) before reaching terminal FAILED.
    # The key property is that it never collapses into FAILED_VALIDATION and
    # is not a terminal crash.
    assert lifecycle.retries == 1
    assert outcome.lifecycle.status == Status.FAILED
    # The audit-visible distinguisher from a real grader failure.
    crash = [e for e in sink.events(lifecycle.run_id) if e.kind == "harness.crash"]
    assert len(crash) == 2
    assert all(
        e.payload["classification"] == "grader_spawn_failure" for e in crash
    )


def test_ran_and_failed_grader_stays_validation_failed() -> None:
    """A grader that DOES start and exits non-zero must still be a validation
    failure -- the two classes cannot merge. This pins the negative half of
    the distinction alongside the holdout suite."""
    store = InMemoryStore()
    sink = _ListSink()
    task = Task(
        goal="g",
        graders=[
            CommandGrader(
                run=f"{sys.executable} -c 'import sys; sys.exit(7)'",
                name="ran-and-failed",
            )
        ],
    )
    lifecycle = Lifecycle(task_id="t1", run_id="run-ran-and-failed")
    config = HarnessConfig(max_retries=0)

    outcome = _run(
        run_task(
            task,
            lifecycle,
            store,
            sink=sink,
            config=config,
            invoke=_verify_invoker(),
        )
    )

    attempt = outcome.attempts[0]
    assert attempt.outcome == Outcome.VALIDATION_FAILED
    assert outcome.lifecycle.status == Status.FAILED
    # No spawn-failure crash event was emitted for a grader that ran.
    crash = [e for e in sink.events(lifecycle.run_id) if e.kind == "harness.crash"]
    assert crash == []
    rows = store.list_grader_results(lifecycle.run_id, 1)
    assert rows[0].payload["termination"] == "exited"
    assert rows[0].payload["exit_code"] == 7
