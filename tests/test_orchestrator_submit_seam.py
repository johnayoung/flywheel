"""Tests for the ``prepare_sandbox`` / ``submit`` seam on ``orchestrate``.

The seam lets a consumer inject a working directory per run and act on the
terminal status — without any consumer (e.g. git) code entering flywheel.
These tests drive real lifecycles through a file-backed store with the agent
stubbed, asserting the consumer callbacks see the right requests and that the
provider's returned path is the one used for both the run and the blocked
recheck. The plain-dir default (no callbacks) is verified to be unchanged.
"""

from __future__ import annotations

import asyncio
import io
import json
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel import (
    FileExistsRequirement,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    SandboxRequest,
    Status,
    SubmitRequest,
    ValidEnvelope,
    orchestrate,
)


# --- fixtures / helpers -----------------------------------------------------


def _write_task(
    phase: Path,
    task_id: str,
    *,
    prerequisites: list[str] | None = None,
    grader_run: str = "true",
) -> None:
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": grader_run}],
    }
    if prerequisites:
        payload["prerequisites"] = prerequisites
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


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


def _verify_result() -> IterationResult:
    return IterationResult(
        transcript="ok",
        messages=_messages(),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=Intent.VERIFY),
        signals=_signals(),
        failure=None,
    )


def _always_verify():
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return _verify_result()

    return _invoke


# --- the prepared path is the one actually used -----------------------------


def test_prepared_path_used_for_run_and_passed_to_submit(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    # Grader passes only if `marker` exists in the run's cwd. The provider
    # writes that marker into the dir it returns, so a DONE proves the run
    # graded in the provided dir, not the default sandbox_root/<task-id>.
    _write_task(phase, "alpha", grader_run="test -f marker")

    prepared_root = tmp_path / "prepared"
    prepare_calls: list[SandboxRequest] = []
    submit_calls: list[SubmitRequest] = []

    def prepare(req: SandboxRequest) -> Path:
        prepare_calls.append(req)
        sandbox = prepared_root / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        (sandbox / "marker").write_text("ok")
        return sandbox

    def submit(req: SubmitRequest) -> None:
        submit_calls.append(req)

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
    run = report.runs[0]

    assert len(prepare_calls) == 1
    assert prepare_calls[0].task_id == "alpha"
    assert prepare_calls[0].mode == "fresh"
    assert prepare_calls[0].run_id is None
    assert prepare_calls[0].task_file.name == "alpha.json"

    assert len(submit_calls) == 1
    assert submit_calls[0].task_id == "alpha"
    assert submit_calls[0].status is Status.DONE
    assert submit_calls[0].run_id == run.run_id
    assert submit_calls[0].sandbox == prepared_root / "alpha"
    # The default sandbox dir was never used for this task.
    assert not (tmp_path / "sandboxes" / "alpha").exists()


def test_submit_receives_failed_status_for_parking(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "boom", grader_run="false")

    prepared = tmp_path / "prepared" / "boom"
    submit_calls: list[SubmitRequest] = []

    def prepare(req: SandboxRequest) -> Path:
        prepared.mkdir(parents=True, exist_ok=True)
        return prepared

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
        )
    )

    assert [r.status for r in report.runs] == [Status.FAILED]
    assert len(submit_calls) == 1
    assert submit_calls[0].status is Status.FAILED
    assert submit_calls[0].sandbox == prepared


# --- the blocked recheck evaluates in the prepared cwd ----------------------


def test_recheck_evaluates_in_prepared_sandbox(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "gated")

    prepared = tmp_path / "prepared" / "gated"
    # The provider always ensures `marker` exists in the dir it returns. The
    # task blocks on file_exists(marker, relative). If recheck used the default
    # sandbox (no marker) it would stay blocked; resuming proves the recheck
    # cwd is the provided dir.
    def prepare(req: SandboxRequest) -> Path:
        prepared.mkdir(parents=True, exist_ok=True)
        (prepared / "marker").write_text("ok")
        return prepared

    calls = {"n": 0}

    async def _invoke(request: InvocationRequest) -> IterationResult:
        calls["n"] += 1
        if calls["n"] == 1:
            return IterationResult(
                transcript="blocked",
                messages=_messages(),  # type: ignore[arg-type]
                envelope=ValidEnvelope(
                    intent=Intent.BLOCKED,
                    reason="waiting on artifact",
                    requires=(
                        FileExistsRequirement(path="marker", present=True),
                    ),
                ),
                signals=_signals(),
                failure=None,
            )
        return _verify_result()

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_invoke,
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            prepare_sandbox=prepare,
        )
    )

    gated = [r for r in report.runs if r.task_id == "gated"]
    assert [r.mode for r in gated] == ["fresh", "resume"]
    assert [r.status for r in gated] == [Status.INTERRUPTED, Status.DONE]
    # prepare was asked for a "resume" sandbox too.
    assert any(c for c in gated if c.mode == "resume")


# --- robustness: a failing provider skips its task, not the worker ----------


def test_prepare_failure_skips_task_and_keeps_draining(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "bad")
    _write_task(phase, "good")

    good_dir = tmp_path / "prepared" / "good"

    def prepare(req: SandboxRequest) -> Path:
        if req.task_id == "bad":
            raise RuntimeError("cannot provision worktree")
        good_dir.mkdir(parents=True, exist_ok=True)
        return good_dir

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

    ran = {r.task_id for r in report.runs}
    assert "good" in ran
    assert "bad" not in ran
    assert all(r.status is Status.DONE for r in report.runs)


# --- defaults: no callbacks reproduces the plain-dir behavior ----------------


def test_default_sandbox_used_when_no_callbacks(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "plain", grader_run="true")

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
        )
    )

    assert [r.status for r in report.runs] == [Status.DONE]
    # The default provider runs each task in sandbox_root/<task-id>.
    assert (tmp_path / "sandboxes" / "plain").is_dir()
