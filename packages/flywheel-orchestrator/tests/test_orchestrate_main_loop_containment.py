"""Main-driver containment of a raising work source and a raising gate.

Two failure modes that previously unwound the whole ``orchestrate`` session
are contained at the main driver:

* A work source whose ``list_work()`` raises must degrade the pass (the worker
  quiesces and returns its report) instead of crashing, mirroring the posture
  the reconciler and ``sync_work_source`` already take. The guard is a generic
  ``Exception`` -- a narrowed subtype that let an unrelated raise escape would
  fail ``test_raising_source_does_not_crash_the_driver``.
* A held-out gate *evaluation* that raises an unexpected exception (orthogonal
  to the engine's own fail-closed verdict, which it returns rather than raises)
  must be contained AND must FAIL CLOSED at the call site: ``submit`` is not
  invoked, so no merge/PR lands off an unevaluated gate. A contain-then-land
  implementation would fail ``test_raising_gate_evaluation_fails_closed``.
"""

from __future__ import annotations

import asyncio
import io
import json
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    ValidEnvelope,
)
from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator import (
    OrchestratorReport,
    SandboxRequest,
    SubmitRequest,
    WorkReport,
    orchestrate,
)


# --- shared agent stub ------------------------------------------------------


def _signals() -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=0.0,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="sess",
    )


def _messages() -> tuple[object, ...]:
    return (
        AssistantMessage(
            content=[TextBlock(text="done")],
            model="claude-test",
            stop_reason="end_turn",
            session_id="sess",
            usage=None,
        ),
        ResultMessage(
            subtype="success",
            duration_ms=1,
            duration_api_ms=1,
            is_error=False,
            num_turns=1,
            session_id="sess",
            stop_reason="end_turn",
            total_cost_usd=0.0,
            usage=None,
        ),
    )


def _always_verify():
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return IterationResult(
            transcript="ok",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(intent=Intent.VERIFY),
            signals=_signals(),
            failure=None,
        )

    return _invoke


def _write_task(phase: Path, task_id: str) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    payload = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": "true"}],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


# --- #1: a raising work source degrades the pass, never crashes -------------


class _SourceBoom(Exception):
    """A generic source failure unrelated to ``WorkSourceError``.

    Raised by the fake source so a guard narrowed to a specific subtype
    (e.g. ``WorkSourceError``) would let it escape and fail the test.
    """


class _RaisingWorkSource:
    """A work source whose every ``list_work()`` blows up.

    A raised listing must never reach the scheduler; the driver contains it
    and quiesces. ``report`` records calls so the test can assert in-flight
    reporting was never touched.
    """

    def __init__(self) -> None:
        self.list_calls = 0
        self.reports: list[WorkReport] = []

    def list_work(self):
        self.list_calls += 1
        raise _SourceBoom("work-source listing exploded")

    def report(self, report: WorkReport) -> None:
        self.reports.append(report)


def test_raising_source_does_not_crash_the_driver(tmp_path: Path) -> None:
    source = _RaisingWorkSource()
    submit_calls: list[SubmitRequest] = []

    def prepare(req: SandboxRequest) -> Path:
        sandbox = tmp_path / "prepared" / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        return sandbox

    # The driver must return its report rather than propagating _SourceBoom.
    report = asyncio.run(
        orchestrate(
            source=source,
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            prepare_sandbox=prepare,
            submit=submit_calls.append,
        )
    )

    assert isinstance(report, OrchestratorReport)
    # The pass degraded: nothing was driven, nothing landed, and the raise
    # never escaped to unwind the worker. In-flight state is untouched (no
    # task was claimed, no run recorded, no outbound report fired).
    assert report.runs == ()
    assert submit_calls == []
    assert source.reports == []
    # list_work was actually exercised (the containment guarded a real raise).
    assert source.list_calls >= 1


# --- #2: a raising gate evaluation fails closed (landing suppressed) --------


class _RaisingHeldOutSource:
    """A held-out grader source whose lookup raises an *unexpected* error.

    This is orthogonal to the engine's fail-closed path: ``HeldOutGraderError``
    is converted to a FAIL verdict inside ``evaluate_held_out_gate``. A bare
    ``RuntimeError`` instead propagates out of the gate evaluation entirely,
    exercising the call-site containment. Failing closed means landing is
    suppressed -- ``submit`` must not be invoked.
    """

    def graders_for(self, task_id: str) -> list[CommandGrader] | None:
        raise RuntimeError(f"gate source exploded for {task_id}")

    def standing_graders_for(self, task: Task) -> list[CommandGrader]:
        return []


def test_raising_gate_evaluation_fails_closed(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")

    submit_calls: list[SubmitRequest] = []

    def prepare(req: SandboxRequest) -> Path:
        sandbox = tmp_path / "prepared" / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        return sandbox

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            prepare_sandbox=prepare,
            submit=submit_calls.append,
            held_out_source=_RaisingHeldOutSource(),
        )
    )

    # The pass settled (orchestrate returned) despite the gate raising.
    assert isinstance(report, OrchestratorReport)
    # The lifecycle still reached DONE -- a gate evaluation error is a landing
    # decision, not a re-judgement of the verified attempt.
    assert [r.status for r in report.runs] == [Status.DONE]
    # FAIL CLOSED: the errored gate suppressed the land. A contain-then-land
    # implementation would have invoked submit here and failed this assertion.
    assert submit_calls == []
    # The prepared sandbox is retained (parked) for forensics, never merged.
    assert (tmp_path / "prepared" / "alpha").is_dir()
