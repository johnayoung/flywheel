"""Behavior tests for injected-store single-task runs (spec 00075, D-2/D-3).

:func:`flywheel_core.workflow.run_task_object` accepts an optional
caller-supplied ``store``. When one is injected the complete run record --
task version, lifecycle, every attempt, the run's domain events, and grader
results -- must land in that store, and no :class:`SqliteStore` may be opened
(so no sqlite file is created at ``db_path``). A scripted ``invoke`` returns a
canned ``verify`` iteration so the run reaches ``DONE`` without spawning an
agent, exactly like the harness contract tests.
"""

from __future__ import annotations

import asyncio
import io
import sys
from collections.abc import Awaitable, Callable
from pathlib import Path

from claude_agent_sdk import (
    AssistantMessage,
    ResultMessage,
    TextBlock,
)

from flywheel_core import (
    CommandGrader,
    InMemoryStore,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    Task,
)
from flywheel_core.envelope import (
    CLOSING_FENCE,
    Intent,
    OPENING_FENCE,
    ValidEnvelope,
)
from flywheel_core.store_protocols import TelemetryRecord
from flywheel_core.workflow import run_task_object


class _ListSink:
    """In-memory TelemetrySink so the run touches no telemetry file either."""

    def __init__(self) -> None:
        self.records: list[TelemetryRecord] = []

    def append_telemetry(self, record: TelemetryRecord) -> None:
        self.records.append(record)


def _verify_invoke() -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    """A one-shot invoker that returns a ``verify`` envelope iteration.

    Honors the per-message observer contract (calls ``request.on_message``
    for each SDK message) exactly as the real invoker does, so the harness's
    persistence path fires identically in the test.
    """
    transcript = (
        f"{OPENING_FENCE}\n"
        '{"intent": "verify", "reason": "done"}'
        f"\n{CLOSING_FENCE}"
    )
    assistant = AssistantMessage(
        content=[TextBlock(text="ok")],
        model="claude-test",
        stop_reason="end_turn",
        session_id="sess-1",
        usage=None,
    )
    result_msg = ResultMessage(
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
    result = IterationResult(
        transcript=transcript,
        messages=(assistant, result_msg),
        envelope=ValidEnvelope(intent=Intent.VERIFY, reason="done"),
        signals=InvocationSignals(
            stop_reason="end_turn",
            num_turns=1,
            total_cost_usd=0.01,
            result_is_error=False,
            result_subtype="success",
            api_error_status=None,
            session_id="sess-1",
        ),
        failure=None,
    )

    async def _invoke(request: InvocationRequest) -> IterationResult:
        if request.on_message is not None:
            for msg in result.messages:
                try:
                    request.on_message(msg)
                except Exception:  # noqa: BLE001 - mirror the production
                    # invoker, which swallows observer exceptions.
                    pass
        return result

    return _invoke


def test_injected_store_holds_complete_run_record(tmp_path: Path) -> None:
    store = InMemoryStore()
    sink = _ListSink()
    # A db path that does not exist yet, under a directory that does not
    # exist yet: nothing may materialize it when a store is injected.
    db_path = tmp_path / "nested" / "flywheel.sqlite"
    sandbox = tmp_path / "sandbox"
    task = Task(
        goal="produce a green run",
        graders=[
            CommandGrader(
                run=f"{sys.executable} -c 'import sys; sys.exit(0)'",
                name="ok",
            )
        ],
    )

    outcome = asyncio.run(
        run_task_object(
            task,
            db_path=db_path,
            sandbox=sandbox,
            invoke=_verify_invoke(),
            store=store,
            sink=sink,
            stream=io.StringIO(),
        )
    )
    run_id = outcome.lifecycle.run_id

    # Edge: no sqlite database is created. An implementation that opens both
    # stores (or falls back to SqliteStore(db_path)) materializes this file
    # and fails here.
    assert not db_path.exists()
    assert not db_path.parent.exists()

    # Lifecycle reached DONE and is queryable through the injected store,
    # with its attempts folded on -- not just a terminal in-memory object.
    assert outcome.lifecycle.status == Status.DONE
    loaded = store.load_lifecycle(run_id)
    assert loaded is not None
    assert loaded.status == Status.DONE
    assert loaded.attempts, "attempts must be persisted through the store"

    # Task version persisted through the injected store (the exact version
    # this run pinned resolves back to the task definition).
    persisted_task = store.load_task_for_run(run_id)
    assert persisted_task is not None
    assert persisted_task.goal == task.goal

    # Every attempt landed via the AttemptStore surface.
    attempts = store.list_attempts(run_id)
    assert len(attempts) >= 1

    # Edge: the run's domain events -- not only a terminal lifecycle row --
    # landed in the injected store. A terminal-summary-only write leaves this
    # empty and fails.
    events = store.list_domain_events(run_id)
    assert events, "run events must land in the injected store"

    # Edge: grader results landed via append_grader_result, not only in the
    # final lifecycle summary.
    grader_rows = store.list_grader_results(run_id, 1)
    assert [r.grader_type for r in grader_rows] == ["command"]
    assert all(r.passed for r in grader_rows)


def test_injected_store_receives_run_events_stream(tmp_path: Path) -> None:
    """The injected store -- not the telemetry sink -- owns the event log.

    Separately pins that the domain-event stream for the run is readable
    back from the injected store in order, so a caller holding only the
    store can reconstruct the lifecycle (spec 00075 criterion 1's "events").
    """
    store = InMemoryStore()
    task = Task(goal="unverified but recorded", graders=[])

    outcome = asyncio.run(
        run_task_object(
            task,
            db_path=tmp_path / "db" / "flywheel.sqlite",
            sandbox=tmp_path / "sandbox",
            invoke=_verify_invoke(),
            store=store,
            sink=_ListSink(),
            stream=io.StringIO(),
        )
    )
    run_id = outcome.lifecycle.run_id

    events = store.list_domain_events(run_id)
    # The seed initializes the lifecycle and at least one attempt is recorded,
    # so the event log has more than a single row.
    assert len(events) >= 2
    kinds = [type(e).__name__ for e in events]
    assert kinds[0] == "LifecycleInitialized"
    assert not (tmp_path / "db" / "flywheel.sqlite").exists()
