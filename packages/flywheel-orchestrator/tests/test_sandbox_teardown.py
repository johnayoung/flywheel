"""Held-out oracle for spec 00044 G1 — the ``SandboxHandle.teardown`` seam.

RED until G1 lands. A provider may return a ``SandboxHandle`` carrying a
``teardown`` callable (a container backend's stop/rm); ``orchestrate`` calls it
best-effort after ``submit`` and before releasing the lease. A bare ``Path``
(every worktree backend) has no teardown and nothing is called. A raising
teardown is contained — it never unwinds the worker or loses the run record.
Do not weaken assertions.
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
from flywheel_orchestrator import (
    SandboxHandle,
    SandboxRequest,
    SubmitRequest,
    orchestrate,
)


def _write_task(phase: Path, task_id: str, *, grader_run: str = "true") -> None:
    phase.mkdir(parents=True, exist_ok=True)
    (phase / f"{task_id}.json").write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": grader_run}],
            }
        )
    )


def _verify_result() -> IterationResult:
    return IterationResult(
        transcript="ok",
        messages=(
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
        ),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=InvocationSignals(
            stop_reason="end_turn",
            num_turns=1,
            total_cost_usd=0.0,
            result_is_error=False,
            result_subtype="success",
            api_error_status=None,
            session_id="sess",
        ),
        failure=None,
    )


def _always_verify():
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return _verify_result()

    return _invoke


def test_handle_teardown_called_after_submit(tmp_path: Path) -> None:
    _write_task(tmp_path / "tasks" / "active" / "01-phase", "alpha")
    events: list[str] = []

    def prepare(req: SandboxRequest) -> SandboxHandle:
        sandbox = tmp_path / "prepared" / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        return SandboxHandle(
            path=sandbox, teardown=lambda: events.append("teardown")
        )

    def submit(req: SubmitRequest) -> None:
        events.append("submit")

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
            submit=submit,
        )
    )

    assert [r.status for r in report.runs] == [Status.DONE]
    # teardown runs, and strictly after submit (landing settles first).
    assert events == ["submit", "teardown"]


def test_teardown_runs_even_on_failed_status(tmp_path: Path) -> None:
    _write_task(tmp_path / "tasks" / "active" / "01-phase", "boom", grader_run="false")
    torn_down: list[str] = []

    def prepare(req: SandboxRequest) -> SandboxHandle:
        sandbox = tmp_path / "prepared" / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        return SandboxHandle(
            path=sandbox, teardown=lambda: torn_down.append(req.task_id)
        )

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
            submit=lambda req: None,
        )
    )

    assert [r.status for r in report.runs] == [Status.FAILED]
    assert torn_down == ["boom"]  # disposed regardless of terminal status


def test_raising_teardown_is_contained(tmp_path: Path) -> None:
    _write_task(tmp_path / "tasks" / "active" / "01-phase", "alpha")

    def prepare(req: SandboxRequest) -> SandboxHandle:
        sandbox = tmp_path / "prepared" / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)

        def _boom() -> None:
            raise RuntimeError("docker rm failed")

        return SandboxHandle(path=sandbox, teardown=_boom)

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
            submit=lambda req: None,
        )
    )

    # The run is recorded DONE despite the teardown blowing up — the failure
    # is contained, never unwinding the worker or losing the record.
    assert [r.status for r in report.runs] == [Status.DONE]


def test_bare_path_provider_has_no_teardown(tmp_path: Path) -> None:
    # A provider returning a bare Path is adapted to a teardown-less handle;
    # the run completes normally with nothing to dispose.
    _write_task(tmp_path / "tasks" / "active" / "01-phase", "plain")

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
        )
    )

    assert [r.status for r in report.runs] == [Status.DONE]
