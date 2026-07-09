"""The reactive approval sweep's empty-sweep mark EXPIRES within a session.

Regression pin for the overnight-2026-07-08 P1 root cause (b): an
``approve`` / ``reject`` enqueued *after* a session's approval sweep found no
pending command for a parked run must be consumed by that same still-running
session once the sweep mark expires -- not skipped until the session ends.

The scenario is exercised end-to-end inside a *single* ``orchestrate`` call:

1. A gated task (higher priority) is driven to ``AWAITING_APPROVAL``.
2. The reactive sweep (section 1b) passes it empty and marks it with a TTL.
3. A second task keeps the session alive for another pass; its stubbed agent
   enqueues the ``approve`` (only now, strictly after the empty sweep) and
   advances the injected clock past the mark's TTL.
4. A later pass of the *same* call re-sweeps the now-expired mark, finds the
   pending ``approve``, resolves it in place, and the lifecycle reaches
   ``DONE`` before ``orchestrate`` returns.

Time is controlled exclusively through ``orchestrate``'s injected ``now``
clock -- there are no real sleeps. Against the pre-fix session-permanent
``attempted_approve`` set the mark never expires, step 4 skips the parked run,
and the final assertion (``DONE``) fails.
"""

from __future__ import annotations

import asyncio
import io
import json
from datetime import datetime, timedelta, timezone
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
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import orchestrate
from flywheel_orchestrator._orchestrate import APPROVAL_SWEEP_MARK_TTL_SECONDS


# --- helpers ---------------------------------------------------------------


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


def _write_gated_task(phase: Path, task_id: str, *, priority: int) -> None:
    """A task whose command grader passes then parks on a single manual gate,
    so a verifying agent drives the lifecycle straight to
    ``AWAITING_APPROVAL``."""
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "priority": priority,
        "graders": [
            {"type": "command", "run": "true"},
            {
                "type": "manual",
                "instruction": "Confirm the rollout.",
                "name": "operator-confirm",
            },
        ],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def _write_plain_task(phase: Path, task_id: str, *, priority: int) -> None:
    """A task with a single always-passing command grader (drives to DONE)."""
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "priority": priority,
        "graders": [{"type": "command", "run": "true"}],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


# --- test ------------------------------------------------------------------


def test_empty_approval_mark_expires_and_is_reswept_same_session(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    # ``gated`` (priority 1) is selected before ``keepalive`` (priority 0), so
    # it parks AWAITING_APPROVAL and is swept-empty on an EARLIER pass than the
    # one that drives the session-keepalive task.
    _write_gated_task(phase, "gated", priority=1)
    _write_plain_task(phase, "keepalive", priority=0)

    db_path = tmp_path / "flywheel.sqlite"

    # A fake clock the test advances by hand -- the only time source in play.
    now_state = {"t": datetime(2026, 1, 1, tzinfo=timezone.utc)}

    def _clock() -> datetime:
        return now_state["t"]

    # Enqueue the approve exactly once, and only after ``gated`` is observed
    # parked in AWAITING_APPROVAL. The gated task's OWN drive sees its
    # lifecycle still RUNNING (not parked), so the enqueue fires on a later
    # invoke -- the keepalive task's -- which the loop reaches only AFTER
    # section 1b has swept the parked run empty and marked it. Advancing the
    # clock past the mark's TTL in the same step lets the next pass's sweep
    # treat the mark as expired and re-resolve.
    state = {"approve_enqueued": False}

    async def _invoke(request: InvocationRequest) -> IterationResult:
        if not state["approve_enqueued"]:
            store = SqliteStore(db_path)
            try:
                parked = store.list_lifecycles(
                    statuses=[Status.AWAITING_APPROVAL], task_id="gated"
                )
                if parked:
                    store.enqueue_command(
                        parked[0].run_id, "approve", {}, now=_clock()
                    )
                    now_state["t"] += timedelta(
                        seconds=APPROVAL_SWEEP_MARK_TTL_SECONDS + 5
                    )
                    state["approve_enqueued"] = True
            finally:
                store.close()
        return _verify_result()

    report = asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=db_path,
            sandbox_root=tmp_path / "sandboxes",
            invoke=_invoke,
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            now=_clock,
        )
    )

    # Guard against a vacuous pass: the approve really was enqueued mid-session.
    assert state["approve_enqueued"] is True

    # ``gated`` was driven once (to AWAITING_APPROVAL); the in-place resolve
    # advances the lifecycle without minting a second RunRecord.
    gated_runs = [r for r in report.runs if r.task_id == "gated"]
    assert len(gated_runs) == 1
    assert gated_runs[0].status is Status.AWAITING_APPROVAL
    run_id = gated_runs[0].run_id

    # The single orchestrate call consumed the mid-session approve: the mark
    # expired, the run was re-swept, and the lifecycle reached DONE.
    store = SqliteStore(db_path)
    try:
        final = store.load_lifecycle(run_id)
        assert final is not None
        assert final.status is Status.DONE
        # The -> DONE edge clears the awaiting ordinal.
        assert final.awaiting_manual_ordinal is None
    finally:
        store.close()
