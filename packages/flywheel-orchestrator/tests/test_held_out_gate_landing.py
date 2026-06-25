"""Landing integration for the held-out gate (spec 00050, #1/#2/#6/#7/#8).

The engine (``test_held_out_gate.py``) computes the verdict; these tests drive a
real lifecycle through ``orchestrate`` with the agent stubbed and assert the
*landing* consequences: a passing/absent gate lands exactly as today (``submit``
invoked), a failing gate blocks the land (``submit`` never invoked, the run
parked), and the three end-states -- landed-ok, agent-failed, gate-failed -- are
distinguishable on the recorded ``RunRecord``.
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
    FilesystemHeldOutGraderSource,
    GateOutcome,
    SandboxRequest,
    SubmitRequest,
    orchestrate,
)


# --- helpers ----------------------------------------------------------------


def _write_task(phase: Path, task_id: str, *, grader_run: str = "true") -> None:
    phase.mkdir(parents=True, exist_ok=True)
    payload = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [{"type": "command", "run": grader_run}],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _register_held_out(root: Path, task_id: str, entries: object) -> None:
    root.mkdir(parents=True, exist_ok=True)
    (root / f"{task_id}.json").write_text(json.dumps(entries))


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


def _drive(
    tmp_path: Path,
    *,
    held_out_root: Path | None,
    submit_calls: list[SubmitRequest],
):
    def prepare(req: SandboxRequest) -> Path:
        sandbox = tmp_path / "prepared" / req.task_id
        sandbox.mkdir(parents=True, exist_ok=True)
        return sandbox

    source = (
        FilesystemHeldOutGraderSource(root=held_out_root)
        if held_out_root is not None
        else None
    )
    return asyncio.run(
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
            held_out_source=source,
        )
    )


# --- #1: a passing gate lands exactly like a no-gate task -------------------


def test_passing_gate_lands_like_no_gate(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")
    held_out = tmp_path / "held_out"
    _register_held_out(
        held_out, "alpha", [{"type": "command", "run": "true", "name": "ok"}]
    )

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)

    assert [r.status for r in report.runs] == [Status.DONE]
    assert report.runs[0].gate is GateOutcome.PASS
    # Landed exactly as today: submit invoked with the DONE landing status.
    assert [c.status for c in submit_calls] == [Status.DONE]
    assert submit_calls[0].task_id == "alpha"


# --- #7: no registration => no gate, lands unchanged ------------------------


def test_no_registration_lands_unchanged(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")
    # Source wired, but the task has no held-out file registered.
    held_out = tmp_path / "held_out"
    held_out.mkdir()

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)

    assert [r.status for r in report.runs] == [Status.DONE]
    assert report.runs[0].gate is GateOutcome.NO_GATE
    assert [c.status for c in submit_calls] == [Status.DONE]


def test_no_source_lands_unchanged(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=None, submit_calls=submit_calls)

    assert [r.status for r in report.runs] == [Status.DONE]
    # The gate never ran: no marker recorded.
    assert report.runs[0].gate is None
    assert [c.status for c in submit_calls] == [Status.DONE]


# --- #2 / #8: a failing gate blocks the land, submit never invoked ----------


def test_failing_gate_blocks_land(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    # The agent's own (visible) grader passes -> the run reports DONE, but the
    # held-out grader exits non-zero, so the gate must block the land (#4).
    _write_task(phase, "alpha", grader_run="true")
    held_out = tmp_path / "held_out"
    _register_held_out(
        held_out,
        "alpha",
        [{"type": "command", "run": "exit 1", "name": "gate-check"}],
    )

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)

    # The lifecycle is still DONE (the gate is a landing decision, not a
    # re-judgement of the attempt), but nothing landed.
    assert [r.status for r in report.runs] == [Status.DONE]
    assert report.runs[0].gate is GateOutcome.FAIL
    assert "gate-check" in report.runs[0].gate_reason
    # submit was never invoked: no merge/PR landing effect.
    assert submit_calls == []
    # The agent's prepared sandbox is retained (parked) for forensics.
    assert (tmp_path / "prepared" / "alpha").is_dir()


def test_unrunnable_gate_fails_closed_and_blocks(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "alpha")
    held_out = tmp_path / "held_out"
    _register_held_out(
        held_out,
        "alpha",
        [{"type": "command", "run": "./not-a-real-binary-xyz", "name": "x"}],
    )

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)

    assert report.runs[0].gate is GateOutcome.FAIL
    assert submit_calls == []


# --- #6: landed-ok, agent-failed, gate-failed are distinguishable -----------


def test_three_outcomes_are_distinguishable(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "land-ok", grader_run="true")
    _write_task(phase, "agent-fail", grader_run="false")
    _write_task(phase, "gate-fail", grader_run="true")

    held_out = tmp_path / "held_out"
    _register_held_out(
        held_out, "land-ok", [{"type": "command", "run": "true", "name": "ok"}]
    )
    _register_held_out(
        held_out,
        "gate-fail",
        [{"type": "command", "run": "exit 1", "name": "blocks"}],
    )
    # "agent-fail" has no held-out registration.

    submit_calls: list[SubmitRequest] = []
    report = _drive(tmp_path, held_out_root=held_out, submit_calls=submit_calls)

    by_id = {r.task_id: r for r in report.runs}
    landed = by_id["land-ok"]
    agent_failed = by_id["agent-fail"]
    gate_failed = by_id["gate-fail"]

    # Each end-state carries a marker pair distinct from the other two.
    assert (landed.status, landed.gate) == (Status.DONE, GateOutcome.PASS)
    assert (agent_failed.status, agent_failed.gate) == (Status.FAILED, None)
    assert (gate_failed.status, gate_failed.gate) == (
        Status.DONE,
        GateOutcome.FAIL,
    )

    # The three markers are pairwise distinct.
    markers = {
        (landed.status, landed.gate),
        (agent_failed.status, agent_failed.gate),
        (gate_failed.status, gate_failed.gate),
    }
    assert len(markers) == 3

    # Only the gate-failed run was withheld from submit; the other two reached
    # it (landed-ok to merge, agent-failed to park).
    submitted = {c.task_id for c in submit_calls}
    assert submitted == {"land-ok", "agent-fail"}
