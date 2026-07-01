"""The human-gate routing re-driver (spec 00069, criteria #10/#11; D-E).

Three lifecycle stops are *intentional* -- a human's decision or a deliberate
ceiling -- and must be ROUTED to the single human-review queue, NEVER bypassed:

* ``AWAITING_APPROVAL`` -- parked on a manual gate. Auto-approving or
  auto-rejecting it (or "resolving" it by re-driving onward) is THE most
  dangerous failure in this phase: it silently defeats the human's authority.
* ``intent=abort`` -- a deliberate agent/operator stop -> terminal ``FAILED``.
* a per-run budget-ceiling breach -> terminal ``FAILED``.

The re-driver surfaces each into the queue ONCE with a machine-readable reason
naming its cause (``awaiting-approval`` / ``abort`` / ``budget-ceiling``) and
leaves the lifecycle's status byte-identical -- it never approves, rejects,
re-drives, or otherwise transitions an intentional stop. These cases drive the
real re-driver over genuine harness-produced lifecycles (a manual gate parks
``AWAITING_APPROVAL``; ``intent=abort`` and an over-ceiling cost produce the two
terminal ``FAILED`` shapes) so the classification and the never-transition
invariant are proven against the actual state the harness writes, not a forgery.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from datetime import datetime, timezone
from pathlib import Path

from flywheel_core import (
    CommandGrader,
    HarnessConfig,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    ManualGrader,
    SqliteStore,
    Status,
    Task,
    run_task,
)
from flywheel_core.envelope import Intent, ValidEnvelope
from flywheel_core.lifecycle import Outcome

from flywheel_orchestrator import (
    REASON_ABORTED,
    REASON_AWAITING_APPROVAL,
    REASON_BUDGET_CEILING,
    HumanGateRequest,
    SqliteClaimStore,
    redrive_human_gates,
)

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)

PASS = CommandGrader(run="true")
FAIL = CommandGrader(run="false")
GATE = ManualGrader(instruction="Confirm the rollout.", name="operator-confirm")


# --- harness helpers --------------------------------------------------------


def _frozen(at: datetime) -> Callable[[], datetime]:
    def _now() -> datetime:
        return at

    return _now


def _signals(cost: float) -> InvocationSignals:
    return InvocationSignals(
        stop_reason="end_turn",
        num_turns=1,
        total_cost_usd=cost,
        result_is_error=False,
        result_subtype="success",
        api_error_status=None,
        session_id="s",
    )


def _iter(intent: Intent, cost: float = 0.0) -> IterationResult:
    return IterationResult(
        transcript="",
        messages=(),
        envelope=ValidEnvelope(intent=intent),
        signals=_signals(cost),
        failure=None,
    )


def _scripted(
    results: list[IterationResult],
) -> Callable[[InvocationRequest], Awaitable[IterationResult]]:
    async def _invoke(request: InvocationRequest) -> IterationResult:
        return results.pop(0)

    return _invoke


def _drive(
    store: SqliteStore,
    *,
    task_id: str,
    run_id: str,
    task: Task,
    results: list[IterationResult],
    config: HarnessConfig | None = None,
) -> str:
    """Drive one real run to a terminal/parked status, persisting to ``store``."""
    lifecycle = Lifecycle(task_id=task_id, run_id=run_id)
    asyncio.run(
        run_task(
            task,
            lifecycle,
            store,
            invoke=_scripted(results),
            config=config or HarnessConfig(max_retries=1),
        )
    )
    return run_id


def _make_awaiting(store: SqliteStore, *, task_id: str, run_id: str) -> str:
    # A passing command grader then a manual gate parks the run AWAITING_APPROVAL.
    return _drive(
        store,
        task_id=task_id,
        run_id=run_id,
        task=Task(goal="g", graders=[PASS, GATE]),
        results=[_iter(Intent.VERIFY, 0.01)],
    )


def _make_abort(store: SqliteStore, *, task_id: str, run_id: str) -> str:
    return _drive(
        store,
        task_id=task_id,
        run_id=run_id,
        task=Task(goal="g", graders=[PASS]),
        results=[_iter(Intent.ABORT)],
    )


def _make_budget(store: SqliteStore, *, task_id: str, run_id: str) -> str:
    # 0.10 cost >= 0.05 ceiling breaches before grading -> terminal FAILED.
    return _drive(
        store,
        task_id=task_id,
        run_id=run_id,
        task=Task(goal="g", graders=[PASS]),
        results=[_iter(Intent.VERIFY, 0.10)],
        config=HarnessConfig(max_retries=1, max_cost_usd=0.05),
    )


def _make_done(store: SqliteStore, *, task_id: str, run_id: str) -> str:
    return _drive(
        store,
        task_id=task_id,
        run_id=run_id,
        task=Task(goal="g", graders=[PASS]),
        results=[_iter(Intent.VERIFY, 0.01)],
    )


def _make_retry_exhausted(
    store: SqliteStore, *, task_id: str, run_id: str
) -> str:
    # A failing command grader spends the whole retry budget -> terminal FAILED
    # whose final attempt is VALIDATION_FAILED (the escalation re-driver's
    # territory), NOT an intentional human/budget stop.
    return _drive(
        store,
        task_id=task_id,
        run_id=run_id,
        task=Task(goal="g", graders=[FAIL]),
        results=[_iter(Intent.VERIFY), _iter(Intent.VERIFY)],
        config=HarnessConfig(max_retries=1),
    )


def _status(store: SqliteStore, run_id: str) -> Status:
    lifecycle = store.load_lifecycle(run_id)
    assert lifecycle is not None
    return lifecycle.status


def _stores(tmp_path: Path) -> tuple[SqliteStore, SqliteClaimStore]:
    return (
        SqliteStore(tmp_path / "control.db"),
        SqliteClaimStore(tmp_path / "claims.db"),
    )


# --- #10: AWAITING_APPROVAL is surfaced but NEVER transitioned --------------


def test_awaiting_approval_surfaced_and_status_never_changes(
    tmp_path: Path,
) -> None:
    """A parked manual gate is surfaced ONCE with ``awaiting-approval`` and its
    status stays exactly AWAITING_APPROVAL across many re-drive cycles -- no
    auto-approve, no auto-reject, no auto-redrive (criterion #10; D-E)."""
    control, claims = _stores(tmp_path)
    try:
        run_id = _make_awaiting(control, task_id="gated", run_id="run-gate")
        assert _status(control, run_id) == Status.AWAITING_APPROVAL
        before = control.load_lifecycle(run_id)
        assert before is not None
        pinned_ordinal = before.awaiting_manual_ordinal

        results = []
        for _ in range(4):  # pump many re-drive/sweep cycles
            (outcome,) = redrive_human_gates(
                control,
                claims,
                requests=[HumanGateRequest(task_id="gated", run_id=run_id)],
                now=_frozen(_BASE),
            )
            results.append(outcome.result)
            # The status is byte-identical after every cycle -- the human still
            # owns the gate.
            assert _status(control, run_id) == Status.AWAITING_APPROVAL

        # Surfaced exactly once; every later cycle is a no-op terminal report.
        assert results == ["queued", "already-queued", "already-queued", "already-queued"]

        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_AWAITING_APPROVAL
        assert queue[0].task_id == "gated"
        assert queue[0].run_id == run_id

        # The gate ordinal (the resolver's pin) is untouched: no approve/reject
        # command was applied.
        after = control.load_lifecycle(run_id)
        assert after is not None
        assert after.awaiting_manual_ordinal == pinned_ordinal
    finally:
        control.close()
        claims.close()


# --- #11: abort / budget are routed naming the cause, never re-dispatched ----


def test_abort_routed_named_and_left_failed(tmp_path: Path) -> None:
    """An ``intent=abort`` run is routed with reason ``abort`` and its terminal
    FAILED is left untouched -- never re-driven as a transient failure."""
    control, claims = _stores(tmp_path)
    try:
        run_id = _make_abort(control, task_id="aborted", run_id="run-abort")
        assert _status(control, run_id) == Status.FAILED

        (outcome,) = redrive_human_gates(
            control,
            claims,
            requests=[HumanGateRequest(task_id="aborted", run_id=run_id)],
            now=_frozen(_BASE),
        )
        assert outcome.result == "queued"
        assert outcome.reason == REASON_ABORTED
        # Status unchanged: the re-driver never transitions an intentional stop.
        assert _status(control, run_id) == Status.FAILED

        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_ABORTED
        assert queue[0].task_id == "aborted"
        assert queue[0].run_id == run_id
    finally:
        control.close()
        claims.close()


def test_budget_ceiling_routed_named_and_left_failed(tmp_path: Path) -> None:
    """A budget-ceiling breach is routed with reason ``budget-ceiling`` (not
    ``abort``, though both reach FAILED/AGENT_ERROR) and left FAILED."""
    control, claims = _stores(tmp_path)
    try:
        run_id = _make_budget(control, task_id="broke", run_id="run-budget")
        loaded = control.load_lifecycle(run_id)
        assert loaded is not None
        assert loaded.status == Status.FAILED
        assert loaded.attempts[-1].outcome is Outcome.AGENT_ERROR

        (outcome,) = redrive_human_gates(
            control,
            claims,
            requests=[HumanGateRequest(task_id="broke", run_id=run_id)],
            now=_frozen(_BASE),
        )
        assert outcome.result == "queued"
        assert outcome.reason == REASON_BUDGET_CEILING
        assert _status(control, run_id) == Status.FAILED

        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_BUDGET_CEILING
        assert queue[0].run_id == run_id
    finally:
        control.close()
        claims.close()


def test_abort_and_budget_are_distinguished(tmp_path: Path) -> None:
    """Abort and budget breach share the terminal FAILED/AGENT_ERROR shape but
    are routed with distinct machine-readable reasons naming each cause."""
    control, claims = _stores(tmp_path)
    try:
        abort_id = _make_abort(control, task_id="a", run_id="run-a")
        budget_id = _make_budget(control, task_id="b", run_id="run-b")

        outcomes = redrive_human_gates(
            control,
            claims,
            requests=[
                HumanGateRequest(task_id="a", run_id=abort_id),
                HumanGateRequest(task_id="b", run_id=budget_id),
            ],
            now=_frozen(_BASE),
        )
        assert [o.reason for o in outcomes] == [
            REASON_ABORTED,
            REASON_BUDGET_CEILING,
        ]
        by_task = {e.task_id: e.reason for e in claims.list_human_review_queue()}
        assert by_task == {"a": REASON_ABORTED, "b": REASON_BUDGET_CEILING}
    finally:
        control.close()
        claims.close()


# --- classification: non-gates are never surfaced ---------------------------


def test_done_and_retry_exhausted_are_not_human_gates(tmp_path: Path) -> None:
    """A DONE run and a retry-exhausted FAILED (final attempt VALIDATION_FAILED,
    the escalation re-driver's job) are NOT human gates: they are skipped and
    never surfaced, so the two failure families never cross-route."""
    control, claims = _stores(tmp_path)
    try:
        done_id = _make_done(control, task_id="done", run_id="run-done")
        exhausted_id = _make_retry_exhausted(
            control, task_id="exhausted", run_id="run-exhausted"
        )
        assert _status(control, done_id) == Status.DONE
        exhausted = control.load_lifecycle(exhausted_id)
        assert exhausted is not None
        assert exhausted.status == Status.FAILED
        assert exhausted.attempts[-1].outcome is Outcome.VALIDATION_FAILED

        outcomes = redrive_human_gates(
            control,
            claims,
            requests=[
                HumanGateRequest(task_id="done", run_id=done_id),
                HumanGateRequest(task_id="exhausted", run_id=exhausted_id),
            ],
            now=_frozen(_BASE),
        )
        assert [o.result for o in outcomes] == ["skipped", "skipped"]
        assert claims.list_human_review_queue() == []
    finally:
        control.close()
        claims.close()


# --- boundedness: exactly one queue entry per run ---------------------------


def test_each_stop_surfaces_exactly_once_across_many_passes(
    tmp_path: Path,
) -> None:
    """Every intentional stop costs exactly one queue entry no matter how many
    passes observe it -- recurrence never re-queues an unresolved gate."""
    control, claims = _stores(tmp_path)
    try:
        gate_id = _make_awaiting(control, task_id="g", run_id="run-g")
        abort_id = _make_abort(control, task_id="a", run_id="run-a")
        budget_id = _make_budget(control, task_id="b", run_id="run-b")
        requests = [
            HumanGateRequest(task_id="g", run_id=gate_id),
            HumanGateRequest(task_id="a", run_id=abort_id),
            HumanGateRequest(task_id="b", run_id=budget_id),
        ]
        for _ in range(5):
            redrive_human_gates(
                control, claims, requests=requests, now=_frozen(_BASE)
            )
        queue = claims.list_human_review_queue()
        assert len(queue) == 3
        assert {e.reason for e in queue} == {
            REASON_AWAITING_APPROVAL,
            REASON_ABORTED,
            REASON_BUDGET_CEILING,
        }
    finally:
        control.close()
        claims.close()
