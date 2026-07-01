"""The bounded retry-escalation re-driver (spec 00069, criteria #5/#6/#13).

A task that spends its entire retry budget without grading green is a strand the
loop must clear, not leave in a silent terminal ``FAILED``. Decision D-A: on the
FIRST retry-budget exhaustion the re-driver escalates exactly ONCE -- a
stronger-model / re-decompose re-drive under the existing per-run budget
ceilings -- and on a SECOND exhaustion (the escalated run also spent its budget)
routes the task to the single human-review queue with
``retries-exhausted-after-escalation`` rather than terminating silently.

These cases drive REAL lifecycles to genuine retry exhaustion through a
file-backed SQLite store: the agent is stubbed by a scripted ``invoke`` that
always emits a ``verify`` envelope, and a failing command grader (``run: false``)
forces the harness's own retry walk to spend the budget and land the run in a
terminal ``FAILED`` reached by validation failure. Nothing about the lifecycle
state is forged -- the re-driver only reads authoritative state, requests a fresh
run through the sanctioned ``drive`` seam, and appends ledger rows.

The oracle (audit 00069, Oracle 2) properties pinned here:

* P1 -- the first exhaustion escalates EXACTLY once (one sanctioned re-drive, one
  boundedness marker), never re-escalating on every exhaustion.
* P4 -- the first exhaustion is NOT routed to the queue (never a silent terminal
  FAILED, never an immediate human hand-off before the one stronger attempt).
* P2 -- the second exhaustion ends the task queued, not a bare terminal FAILED.
* P3 -- that routing carries the machine-readable
  ``retries-exhausted-after-escalation`` reason token.

Plus the boundedness guards: a non-exhausted run and an abort (``AGENT_ERROR``,
not a budget exhaustion) are never escalated, and a task already routed
post-escalation is never re-escalated or re-queued.
"""

from __future__ import annotations

import asyncio
from datetime import datetime, timezone
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    CommandGrader,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Status,
    Task,
    ValidEnvelope,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.workflow import run_task_object
from flywheel_orchestrator import (
    REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION,
    EscalationRequest,
    SqliteClaimStore,
    redrive_exhausted_retries,
)
from flywheel_orchestrator._claims import STOP_RETRIES_ESCALATED

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- scripted-agent helpers (mirror test_orchestrator.py) -------------------


def _frozen(at: datetime):
    def _now() -> datetime:
        return at

    return _now


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


def _result(intent: Intent) -> IterationResult:
    return IterationResult(
        transcript="ok",
        messages=_messages(),  # type: ignore[arg-type]
        envelope=ValidEnvelope(intent=intent),
        signals=_signals(),
        failure=None,
    )


def _always(intent: Intent):
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return _result(intent)

    return _invoke


def _failing_task(task_id: str = "t1") -> Task:
    # A verify envelope drives validation; a ``false`` command grader always
    # fails it, so the harness's retry walk spends the whole budget and lands
    # the run in a terminal FAILED reached by validation failure.
    return Task(
        id=task_id,
        goal=f"Goal for {task_id}.",
        graders=[CommandGrader(run="false")],
    )


def _drive_run(
    task: Task,
    *,
    db: Path,
    sandbox: Path,
    run_id: str,
    intent: Intent = Intent.VERIFY,
    max_retries: int = 1,
) -> None:
    asyncio.run(
        run_task_object(
            task,
            db_path=db,
            sandbox=sandbox,
            run_id=run_id,
            max_retries=max_retries,
            max_turns=4,
            invoke=_always(intent),
        )
    )


def _escalation_markers(claims: SqliteClaimStore, task_id: str) -> list:
    return [
        e
        for e in claims.list_subject_stop_events(task_id)
        if e.kind == STOP_RETRIES_ESCALATED
    ]


# --- P1 + P4: first exhaustion escalates exactly once, not queued ------------


def test_first_exhaustion_escalates_once_and_is_not_queued(
    tmp_path: Path,
) -> None:
    """On the first retry-budget exhaustion the re-driver escalates EXACTLY once
    (one sanctioned re-drive, one boundedness marker) and does NOT route the task
    to the human-review queue (P1 + P4; D-A)."""
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        task = _failing_task("t1")
        # Drive the original run to genuine retry exhaustion.
        _drive_run(task, db=db, sandbox=tmp_path / "sb-1", run_id="run-1")
        original = control.load_lifecycle("run-1")
        assert original is not None
        assert original.status is Status.FAILED
        assert original.retries == 1

        captured: dict[str, object] = {}

        async def drive(
            request: EscalationRequest, model: str | None
        ) -> str | None:
            captured["model"] = model
            captured["task_id"] = request.task.id
            new_run = "run-esc-1"
            await run_task_object(
                request.task,
                db_path=db,
                sandbox=tmp_path / f"sb-{new_run}",
                run_id=new_run,
                max_retries=1,
                max_turns=4,
                invoke=_always(Intent.VERIFY),
            )
            return new_run

        req = EscalationRequest(task_id="t1", task=task, run_id="run-1")
        outcomes = asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[req],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )

        # Escalated once, naming the fresh run; the escalation config was used.
        assert [o.result for o in outcomes] == ["escalated"]
        assert outcomes[0].task_id == "t1"
        assert outcomes[0].escalated_run_id == "run-esc-1"
        assert outcomes[0].escalations == 1
        assert captured["model"] == "opus"
        assert captured["task_id"] == "t1"
        # P1: exactly one sanctioned escalation marker.
        assert len(_escalation_markers(claims, "t1")) == 1
        # P4: the first exhaustion is never routed to the queue.
        assert claims.list_human_review_queue() == []
    finally:
        control.close()
        claims.close()


# --- P2 + P3: the second exhaustion routes to the queue ----------------------


def test_second_exhaustion_routes_to_queue_with_reason(tmp_path: Path) -> None:
    """After the single sanctioned escalation, a second retry-budget exhaustion
    routes the task to the human-review queue with the machine-readable
    ``retries-exhausted-after-escalation`` reason -- not a silent terminal FAILED
    (P2 + P3; criterion #6)."""
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        task = _failing_task("t1")
        # Original run exhausts, then the escalated run exhausts too.
        _drive_run(task, db=db, sandbox=tmp_path / "sb-1", run_id="run-1")

        async def drive(
            request: EscalationRequest, model: str | None
        ) -> str | None:
            new_run = "run-esc-1"
            await run_task_object(
                request.task,
                db_path=db,
                sandbox=tmp_path / f"sb-{new_run}",
                run_id=new_run,
                max_retries=1,
                max_turns=4,
                invoke=_always(Intent.VERIFY),
            )
            return new_run

        # First pass: escalate on the original run's exhaustion.
        first = asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[EscalationRequest(task_id="t1", task=task, run_id="run-1")],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )
        assert [o.result for o in first] == ["escalated"]
        escalated_lc = control.load_lifecycle("run-esc-1")
        assert escalated_lc is not None
        assert escalated_lc.status is Status.FAILED

        # Second pass: the escalated run also exhausted -> route to the queue.
        second = asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[
                    EscalationRequest(task_id="t1", task=task, run_id="run-esc-1")
                ],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )

        assert [o.result for o in second] == ["queued"]
        # P2 + P3: routed once, keyed to the escalated run, with the reason token.
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_RETRIES_EXHAUSTED_AFTER_ESCALATION
        assert queue[0].task_id == "t1"
        assert queue[0].run_id == "run-esc-1"
        # Bounded: still exactly one escalation, no second re-drive.
        assert len(_escalation_markers(claims, "t1")) == 1
    finally:
        control.close()
        claims.close()


# --- boundedness: a queued task is never re-escalated or re-queued -----------


def test_already_queued_task_makes_no_further_attempt(tmp_path: Path) -> None:
    """A task already routed to the queue after its escalation short-circuits on
    the terminal guard: no re-escalation, no second queue entry, no re-drive
    (criterion #6)."""
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        task = _failing_task("t1")
        _drive_run(task, db=db, sandbox=tmp_path / "sb-1", run_id="run-1")

        drive_calls: list[str] = []

        async def drive(
            request: EscalationRequest, model: str | None
        ) -> str | None:
            drive_calls.append(request.run_id)
            new_run = "run-esc-1"
            await run_task_object(
                request.task,
                db_path=db,
                sandbox=tmp_path / f"sb-{new_run}",
                run_id=new_run,
                max_retries=1,
                max_turns=4,
                invoke=_always(Intent.VERIFY),
            )
            return new_run

        def _pass(run_id: str):
            return asyncio.run(
                redrive_exhausted_retries(
                    control,
                    claims,
                    "worker-a",
                    requests=[
                        EscalationRequest(task_id="t1", task=task, run_id=run_id)
                    ],
                    drive=drive,
                    escalation_model="opus",
                    max_retries=1,
                    lease_seconds=3600,
                    now=_frozen(_BASE),
                )
            )

        _pass("run-1")  # escalate
        _pass("run-esc-1")  # route to queue
        assert len(claims.list_human_review_queue()) == 1
        assert drive_calls == ["run-1"]  # escalated exactly once

        # Third pass over the already-queued task: no further attempt.
        third = _pass("run-esc-1")
        assert [o.result for o in third] == ["queued"]
        assert third[0].escalated_run_id == ""
        # No second re-drive, no second queue entry.
        assert drive_calls == ["run-1"]
        assert len(claims.list_human_review_queue()) == 1
        assert len(_escalation_markers(claims, "t1")) == 1
    finally:
        control.close()
        claims.close()


# --- boundedness: a non-exhausted run is never escalated ---------------------


def test_non_exhausted_run_is_not_escalated(tmp_path: Path) -> None:
    """A run that graded green (``DONE``) never spent its retry budget, so the
    re-driver drops it: no escalation, no queue entry, no outcome."""
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        task = Task(
            id="t1",
            goal="Goal for t1.",
            graders=[CommandGrader(run="true")],
        )
        _drive_run(task, db=db, sandbox=tmp_path / "sb-1", run_id="run-1")
        done = control.load_lifecycle("run-1")
        assert done is not None
        assert done.status is Status.DONE

        async def drive(
            request: EscalationRequest, model: str | None
        ) -> str | None:
            raise AssertionError("a non-exhausted run must never be escalated")

        outcomes = asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[EscalationRequest(task_id="t1", task=task, run_id="run-1")],
                drive=drive,
                escalation_model="opus",
                max_retries=1,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )
        assert outcomes == ()
        assert claims.list_human_review_queue() == []
        assert _escalation_markers(claims, "t1") == []
    finally:
        control.close()
        claims.close()


# --- boundedness: an abort is not a retry exhaustion and is not escalated -----


def test_aborted_run_is_not_escalated(tmp_path: Path) -> None:
    """An ``intent=abort`` run reaches terminal FAILED with an ``AGENT_ERROR``
    outcome, not a spent retry budget -- an abort is surfaced to the queue by the
    D-E routing layer, never escalated by this re-driver."""
    db = tmp_path / "flywheel.sqlite"
    control = SqliteStore(db)
    claims = SqliteClaimStore(db)
    try:
        task = _failing_task("t1")
        # An abort reaches FAILED directly without consuming a retry.
        _drive_run(
            task,
            db=db,
            sandbox=tmp_path / "sb-1",
            run_id="run-1",
            intent=Intent.ABORT,
            max_retries=0,
        )
        aborted = control.load_lifecycle("run-1")
        assert aborted is not None
        assert aborted.status is Status.FAILED

        async def drive(
            request: EscalationRequest, model: str | None
        ) -> str | None:
            raise AssertionError("an aborted run must never be escalated")

        outcomes = asyncio.run(
            redrive_exhausted_retries(
                control,
                claims,
                "worker-a",
                requests=[EscalationRequest(task_id="t1", task=task, run_id="run-1")],
                drive=drive,
                escalation_model="opus",
                max_retries=0,
                lease_seconds=3600,
                now=_frozen(_BASE),
            )
        )
        assert outcomes == ()
        assert claims.list_human_review_queue() == []
        assert _escalation_markers(claims, "t1") == []
    finally:
        control.close()
        claims.close()
