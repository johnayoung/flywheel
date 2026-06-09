"""Tests for the cross-task orchestrator (P4).

Drives real lifecycles end-to-end through a file-backed SQLite store, with
the agent stubbed by a fake ``invoke``. Command graders run real
subprocesses (``true``/``false``) so DONE/FAILED outcomes are authentic.
"""

from __future__ import annotations

import asyncio
import io
import json
import time
from datetime import datetime, timezone
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel import (
    FileExistsRequirement,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    Status,
    ValidEnvelope,
)
from flywheel_orchestrator import SqliteClaimStore, orchestrate
from flywheel.store_sqlite import SqliteStore


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


def _orchestrate(tmp_path: Path, invoke):
    return asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=invoke,
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
        )
    )


# --- prerequisite promotion / gating ---------------------------------------


def test_prerequisite_promotion_runs_dependent_after_dependency(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "base")
    _write_task(phase, "leaf", prerequisites=["base"])

    report = _orchestrate(tmp_path, _always_verify())

    order = [r.task_id for r in report.runs]
    assert order == ["base", "leaf"]
    assert all(r.status is Status.DONE for r in report.runs)
    assert all(r.mode == "fresh" for r in report.runs)


def test_task_with_undone_prerequisite_never_runs(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    # 'leaf' depends on 'gate', and 'gate' always fails its grader, so it
    # never reaches DONE -- 'leaf' must therefore never run.
    _write_task(phase, "gate", grader_run="false")
    _write_task(phase, "leaf", prerequisites=["gate"])

    report = _orchestrate(tmp_path, _always_verify())

    ran = {r.task_id for r in report.runs}
    assert "gate" in ran
    assert "leaf" not in ran
    gate = next(r for r in report.runs if r.task_id == "gate")
    assert gate.status is Status.FAILED


def test_task_with_dangling_prerequisite_never_runs(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "leaf", prerequisites=["does-not-exist"])

    report = _orchestrate(tmp_path, _always_verify())

    assert report.runs == ()


# --- termination ------------------------------------------------------------


def test_perpetually_failing_task_runs_once_and_stops(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "flaky", grader_run="false")

    report = _orchestrate(tmp_path, _always_verify())

    # One fresh run per task per session -> exactly one run, no infinite loop.
    flaky_runs = [r for r in report.runs if r.task_id == "flaky"]
    assert len(flaky_runs) == 1
    assert flaky_runs[0].status is Status.FAILED


# --- reactive unblock + resume ---------------------------------------------


def test_blocked_then_unblocked_resumes_on_same_run_id(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "gated")
    sentinel = tmp_path / "unblock-me"

    calls = {"n": 0}

    async def _invoke(request: InvocationRequest) -> IterationResult:
        calls["n"] += 1
        if calls["n"] == 1:
            # First (fresh) run: report blocked on a file that this run
            # "produces", and create it so the recheck will succeed.
            sentinel.write_text("ready")
            return IterationResult(
                transcript="blocked",
                messages=_messages(),  # type: ignore[arg-type]
                envelope=ValidEnvelope(
                    intent=Intent.BLOCKED,
                    reason="waiting on artifact",
                    requires=(
                        FileExistsRequirement(
                            path=str(sentinel), present=True
                        ),
                    ),
                ),
                signals=_signals(),
                failure=None,
            )
        # Resume run: predicate now satisfied, proceed to verification.
        return _verify_result()

    report = _orchestrate(tmp_path, _invoke)

    # Two runs of the same task: a fresh one that blocked, then a resume on
    # the SAME run_id that completed.
    gated = [r for r in report.runs if r.task_id == "gated"]
    assert [r.mode for r in gated] == ["fresh", "resume"]
    assert gated[0].run_id == gated[1].run_id
    assert gated[0].status is Status.INTERRUPTED
    assert gated[1].status is Status.DONE

    # The store agrees: the (single) lifecycle for the task is DONE.
    store = SqliteStore(tmp_path / "flywheel.sqlite")
    try:
        final = store.load_lifecycle(gated[1].run_id)
        assert final is not None
        assert final.status is Status.DONE
    finally:
        store.close()


def test_still_blocked_task_is_not_resumed_or_rerun(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "stuck")
    missing = tmp_path / "never-created"

    async def _invoke(request: InvocationRequest) -> IterationResult:
        return IterationResult(
            transcript="blocked",
            messages=_messages(),  # type: ignore[arg-type]
            envelope=ValidEnvelope(
                intent=Intent.BLOCKED,
                reason="waiting forever",
                requires=(
                    FileExistsRequirement(path=str(missing), present=True),
                ),
            ),
            signals=_signals(),
            failure=None,
        )

    report = _orchestrate(tmp_path, _invoke)

    # The predicate never holds, so the task runs once (fresh -> blocked) and
    # is never resumed or wastefully re-run.
    stuck = [r for r in report.runs if r.task_id == "stuck"]
    assert len(stuck) == 1
    assert stuck[0].mode == "fresh"
    assert stuck[0].status is Status.INTERRUPTED


# --- stranded recovery at entry --------------------------------------------


def test_stranded_lifecycle_is_recovered_then_task_runs(
    tmp_path: Path,
) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "resumable")

    # Pre-seed a lifecycle stranded mid-attempt (worker died in RUNNING).
    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db_path)
    try:
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="resumable", run_id="run-stranded")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        store.create_lifecycle(lc)
    finally:
        store.close()

    report = _orchestrate(tmp_path, _always_verify())

    # Entry recovery finalized the stranded run...
    assert "run-stranded" in report.recovered
    # ...and the task then ran fresh to completion.
    resumable = [r for r in report.runs if r.task_id == "resumable"]
    assert len(resumable) == 1
    assert resumable[0].status is Status.DONE


# --- multi-worker coordination (P5) ----------------------------------------


def test_held_claim_makes_orchestrator_skip_the_task(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "owned")
    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)

    # A peer worker holds a live lease on the only task.
    holder = SqliteClaimStore(db_path)
    try:
        claim = holder.acquire_claim(
            "owned",
            "other-worker",
            now=datetime.now(timezone.utc),
            lease_seconds=3600,
        )
        assert claim is not None
    finally:
        holder.close()  # the claim row persists in the file

    report = _orchestrate(tmp_path, _always_verify())
    # The task is claimed by a live peer, so this worker runs nothing.
    assert report.runs == ()

    # Once the lease is released, a fresh orchestrate run picks it up.
    releaser = SqliteClaimStore(db_path)
    try:
        releaser.release_claim(claim)
    finally:
        releaser.close()
    report2 = _orchestrate(tmp_path, _always_verify())
    assert [r.task_id for r in report2.runs] == ["owned"]
    assert report2.runs[0].status is Status.DONE


def test_claim_is_released_after_a_run(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "solo")
    report = _orchestrate(tmp_path, _always_verify())
    assert [r.task_id for r in report.runs] == ["solo"]

    store = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        assert store.load_claim("solo") is None
    finally:
        store.close()


def test_two_workers_run_each_task_exactly_once(tmp_path: Path) -> None:
    phase = tmp_path / "tasks" / "active" / "01-phase"
    task_ids = ["t1", "t2", "t3", "t4"]
    for tid in task_ids:
        _write_task(phase, tid)
    db_path = tmp_path / "flywheel.sqlite"

    async def _slow_verify_invoke(request: InvocationRequest):
        # A short delay so the two workers overlap and actually contend for
        # claims rather than serializing by accident.
        await asyncio.sleep(0.02)
        return _verify_result()

    results: dict[str, object] = {}

    def _worker(name: str) -> None:
        results[name] = asyncio.run(
            orchestrate(
                tasks_dir=tmp_path / "tasks",
                db_path=db_path,
                sandbox_root=tmp_path / "sb" / name,
                invoke=_slow_verify_invoke,
                worker_id=name,
                max_retries=0,
                max_turns=4,
                lease_seconds=60,
                stream=io.StringIO(),
            )
        )

    import threading

    threads = [
        threading.Thread(target=_worker, args=(name,))
        for name in ("worker-a", "worker-b")
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(30)
    assert all(not t.is_alive() for t in threads)

    report_a = results["worker-a"]
    report_b = results["worker-b"]
    ran_a = {r.task_id for r in report_a.runs}  # type: ignore[attr-defined]
    ran_b = {r.task_id for r in report_b.runs}  # type: ignore[attr-defined]

    # No task was run by both workers, and together they covered every task.
    assert ran_a.isdisjoint(ran_b)
    assert ran_a | ran_b == set(task_ids)
    all_runs = list(report_a.runs) + list(report_b.runs)  # type: ignore[attr-defined]
    assert len(all_runs) == len(task_ids)
    assert all(r.status is Status.DONE for r in all_runs)


def test_claim_lost_mid_run_relinquishes_without_killing_worker(
    tmp_path: Path, monkeypatch
) -> None:
    # A peer stealing one task's lease mid-run surfaces as
    # OptimisticConcurrencyError out of _drive_under_lease. That must NOT
    # unwind out of orchestrate and abandon the worker's other tasks: the
    # losing task is relinquished (recorded as no run) and the loop carries
    # on to the next claimable task.
    import flywheel_orchestrator._orchestrate as orch
    from flywheel import OptimisticConcurrencyError

    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "a-loser")
    _write_task(phase, "b-winner")

    async def _stub_drive(control, claims, claim, row, *, stream, **kwargs):
        task_id = row.task.id
        if task_id == "a-loser":
            raise OptimisticConcurrencyError(
                f"run-{task_id}", expected_version=1, actual_version=2
            )
        return orch.RunRecord(
            task_id=task_id,
            run_id=f"run-{task_id}",
            status=Status.DONE,
            mode="fresh",
            worker_id="w",
        )

    monkeypatch.setattr(orch, "_drive_under_lease", _stub_drive)

    # Must return normally rather than propagating the stolen-claim error.
    report = _orchestrate(tmp_path, _always_verify())

    ran = {r.task_id for r in report.runs}
    assert "a-loser" not in ran  # relinquished, no run recorded
    assert "b-winner" in ran  # peer-takeover of one task did not abort the rest
    assert all(r.status is Status.DONE for r in report.runs)


# --- reactive resolve of AWAITING_APPROVAL gates ---------------------------


def _write_task_with_manual_gate(
    phase: Path,
    task_id: str,
    *,
    gate_name: str = "operator-confirm",
    gate_instruction: str = "Confirm the rollout.",
) -> None:
    """Write a task whose validation passes the command grader and
    parks on a single manual gate, so a verifying agent drives the
    lifecycle straight to ``AWAITING_APPROVAL`` (spec 00016 FR-4)."""
    phase.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "id": task_id,
        "goal": f"Goal for {task_id}.",
        "graders": [
            {"type": "command", "run": "true"},
            {
                "type": "manual",
                "instruction": gate_instruction,
                "name": gate_name,
            },
        ],
    }
    (phase / f"{task_id}.json").write_text(json.dumps(payload))


def test_awaiting_approval_pending_approve_resolves_on_next_tick(
    tmp_path: Path,
) -> None:
    """Spec 00016 FR-9 acceptance (a): a pending ``approve`` is applied
    on the next reactive tick and the lifecycle advances. The first
    session drives the task to ``AWAITING_APPROVAL``; the operator
    enqueues ``approve`` out-of-band; the second session's reactive
    sweep resolves it in place (no follow-on drive) and the lifecycle
    reaches ``DONE``."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task_with_manual_gate(phase, "gated")

    first = _orchestrate(tmp_path, _always_verify())
    gated_first = [r for r in first.runs if r.task_id == "gated"]
    assert len(gated_first) == 1
    assert gated_first[0].status is Status.AWAITING_APPROVAL
    run_id = gated_first[0].run_id

    # The operator enqueues an approve out-of-band against the parked run.
    db_path = tmp_path / "flywheel.sqlite"
    store = SqliteStore(db_path)
    try:
        store.enqueue_command(
            run_id,
            "approve",
            {},
            now=datetime.now(timezone.utc),
        )
    finally:
        store.close()

    # Second session: the reactive resolve pass claims the pending
    # approve, writes the manual receipt, and transitions the
    # lifecycle to DONE. No new RunRecord is created — resolution is
    # in-place, not a fresh drive.
    second = _orchestrate(tmp_path, _always_verify())
    assert second.runs == ()

    store = SqliteStore(db_path)
    try:
        final = store.load_lifecycle(run_id)
        assert final is not None
        assert final.status is Status.DONE
        # The -> DONE edge centralizes the awaiting-ordinal clear.
        assert final.awaiting_manual_ordinal is None
    finally:
        store.close()


def test_awaiting_approval_with_no_pending_command_stays_parked(
    tmp_path: Path,
) -> None:
    """Spec 00016 FR-9 acceptance (b): with no pending command, the
    reactive resolve pass is a no-op and the lifecycle remains
    ``AWAITING_APPROVAL``. Also exercises the
    ``finalize_stranded_lifecycle`` exemption — a parked gate is not
    recovered as stranded on entry."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task_with_manual_gate(phase, "gated")

    first = _orchestrate(tmp_path, _always_verify())
    gated_first = [r for r in first.runs if r.task_id == "gated"]
    assert len(gated_first) == 1
    assert gated_first[0].status is Status.AWAITING_APPROVAL
    run_id = gated_first[0].run_id

    # No approve / reject is enqueued. The reactive sweep finds no
    # pending command, marks the run attempted-this-session, and the
    # lifecycle stays parked.
    second = _orchestrate(tmp_path, _always_verify())
    assert second.runs == ()
    # AWAITING_APPROVAL is exempt from the stranded-recovery backstop.
    assert second.recovered == ()

    store = SqliteStore(tmp_path / "flywheel.sqlite")
    try:
        loaded = store.load_lifecycle(run_id)
        assert loaded is not None
        assert loaded.status is Status.AWAITING_APPROVAL
        # The persisted ordinal still points at the parked gate.
        assert loaded.awaiting_manual_ordinal is not None
    finally:
        store.close()


def test_heartbeat_renews_the_lease(tmp_path: Path) -> None:
    from flywheel_orchestrator._orchestrate import _ClaimHeartbeat

    store = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        start = datetime.now(timezone.utc)
        claim = store.acquire_claim("t", "w", now=start, lease_seconds=10)
        assert claim is not None
        heartbeat = _ClaimHeartbeat(
            claims=store,
            claim=claim,
            lease_seconds=10,
            interval=0.02,
            now=lambda: datetime.now(timezone.utc),
        ).start()
        time.sleep(0.12)  # ~6 renewal ticks
        latest = heartbeat.stop()
        assert latest.version > claim.version
        reloaded = store.load_claim("t")
        assert reloaded is not None
        assert reloaded.version >= 2
    finally:
        store.close()
