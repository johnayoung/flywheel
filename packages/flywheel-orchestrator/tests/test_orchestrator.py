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

from flywheel_core import (
    FileExistsRequirement,
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    Status,
    ValidEnvelope,
)
from flywheel_orchestrator import (
    DirectoryWorkSource,
    SqliteClaimStore,
    orchestrate,
)
from flywheel_core.store_sqlite import SqliteStore


# --- fixtures / helpers -----------------------------------------------------


def _write_task(
    phase: Path,
    task_id: str,
    *,
    prerequisites: list[str] | None = None,
    conflict_keys: list[str] | None = None,
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
    if conflict_keys:
        payload["conflict_keys"] = conflict_keys
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


def test_statically_invalid_task_is_skipped_not_dispatched(
    tmp_path: Path,
) -> None:
    # Schedule-time validation gate (spec 00034): a task whose grader is an
    # unparseable shell command is statically invalid -- the orchestrator must
    # skip it (never dispatch its agent), while a valid peer in the same phase
    # still runs. The invalid task is absent from report.runs entirely
    # (skipped), not present-but-FAILED (dispatched then failed) -- that
    # discriminates the gate from a no-op. (The missing-path check is tabled.)
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "ok", grader_run="true")
    _write_task(
        phase,
        "broken",
        grader_run="uv run pytest 'unterminated",
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
            repo_root=tmp_path,
        )
    )

    ran = {r.task_id for r in report.runs}
    assert "ok" in ran, "a valid task must still dispatch"
    assert "broken" not in ran, (
        "a statically-invalid task must be skipped, never dispatched"
    )


def test_invalid_task_does_not_starve_a_valid_peer_repo_root_default(
    tmp_path: Path,
) -> None:
    # Without repo_root (library-caller default) the gate is disabled, so the
    # invalid task is dispatched and FAILS its grader exactly as before --
    # this pins the back-compat default and proves the gate is opt-in.
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(
        phase,
        "broken",
        grader_run="uv run pytest 'unterminated",
    )

    report = _orchestrate(tmp_path, _always_verify())

    ran = {r.task_id for r in report.runs}
    assert "broken" in ran, "with no repo_root the gate is off (back-compat)"


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


def test_bare_interrupted_task_resumes_on_same_run_id(tmp_path: Path) -> None:
    """A bare operator-interrupted lifecycle (INTERRUPTED with no structured
    block) must RESUME on its own run_id so run_task's entry-time
    INTERRUPTED -> READY normalization fires. Minting a fresh run_id instead
    drives a new lifecycle from scratch and orphans the paused one."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "paused")

    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    store = SqliteStore(db_path)
    try:
        now = datetime.now(timezone.utc)
        lc = Lifecycle(task_id="paused", run_id="run-paused")
        lc.transition_to(Status.READY, now=now)
        lc.transition_to(Status.RUNNING, now=now)
        lc.transition_to(Status.INTERRUPTED, now=now)  # no blocked_requires
        store.create_lifecycle(lc)
    finally:
        store.close()

    report = _orchestrate(tmp_path, _always_verify())

    # Resumed on the SAME run_id (mode "resume"), reaching DONE.
    paused = [r for r in report.runs if r.task_id == "paused"]
    assert len(paused) == 1
    assert paused[0].mode == "resume"
    assert paused[0].run_id == "run-paused"
    assert paused[0].status is Status.DONE

    # Exactly one lifecycle for the task: the paused one was resumed, not
    # orphaned alongside a freshly-minted run_id.
    store = SqliteStore(db_path)
    try:
        lifecycles = store.list_lifecycles(task_id="paused")
        assert [lc.run_id for lc in lifecycles] == ["run-paused"]
        assert lifecycles[0].status is Status.DONE
    finally:
        store.close()


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


def test_peer_conflict_key_claim_excludes_overlapping_task(
    tmp_path: Path,
) -> None:
    """A live keyed claim excludes a different task sharing a key.

    Discriminates the loop wiring: were the dispatch acquire keyless, the
    store would grant it (empty keys are never refused on overlap) and
    ``overlap`` would run despite the peer's live keyed claim.
    """
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "free")
    _write_task(phase, "overlap", conflict_keys=["src/hot.py"])
    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)

    # A peer worker holds a live keyed claim on some other task.
    holder = SqliteClaimStore(db_path)
    try:
        claim = holder.acquire_claim(
            "peer-task",
            "other-worker",
            now=datetime.now(timezone.utc),
            lease_seconds=3600,
            conflict_keys=frozenset({"src/hot.py"}),
        )
        assert claim is not None
    finally:
        holder.close()

    report = _orchestrate(tmp_path, _always_verify())
    # The keyless task runs; the overlapping one is skipped, not consumed.
    assert [r.task_id for r in report.runs] == ["free"]

    releaser = SqliteClaimStore(db_path)
    try:
        releaser.release_claim(claim)
    finally:
        releaser.close()
    report2 = _orchestrate(tmp_path, _always_verify())
    assert [r.task_id for r in report2.runs] == ["overlap"]
    assert report2.runs[0].status is Status.DONE


def test_conflicting_tasks_serialize_within_one_session(
    tmp_path: Path,
) -> None:
    """Two items sharing a key both finish in one session, one at a time.

    Each run's claim is released before the next fresh pass, so shared keys
    serialize the pair without deadlocking a single worker.
    """
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "first", conflict_keys=["src/hot.py"])
    _write_task(phase, "second", conflict_keys=["src/hot.py"])

    report = _orchestrate(tmp_path, _always_verify())
    assert sorted(r.task_id for r in report.runs) == ["first", "second"]
    assert all(r.status is Status.DONE for r in report.runs)


def test_peer_conflict_key_claim_defers_blocked_resume(
    tmp_path: Path,
) -> None:
    """The blocked-resume acquire carries keys too, not just fresh dispatch."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "gated", conflict_keys=["src/hot.py"])
    sentinel = tmp_path / "unblock-me"

    async def _invoke(request: InvocationRequest) -> IterationResult:
        if not sentinel.exists():
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
        return _verify_result()

    # Session 1: the task blocks on the missing sentinel.
    report = _orchestrate(tmp_path, _invoke)
    assert [r.status for r in report.runs] == [Status.INTERRUPTED]

    # The predicate now holds, but a peer holds a live overlapping claim.
    sentinel.write_text("ready")
    holder = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        claim = holder.acquire_claim(
            "peer-task",
            "other-worker",
            now=datetime.now(timezone.utc),
            lease_seconds=3600,
            conflict_keys=frozenset({"src/hot.py"}),
        )
        assert claim is not None
    finally:
        holder.close()
    report2 = _orchestrate(tmp_path, _invoke)
    assert report2.runs == ()

    # Released: the next session resumes the blocked run to DONE.
    releaser = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        releaser.release_claim(claim)
    finally:
        releaser.close()
    report3 = _orchestrate(tmp_path, _invoke)
    assert [(r.task_id, r.mode, r.status) for r in report3.runs] == [
        ("gated", "resume", Status.DONE)
    ]


def test_two_workers_run_each_task_exactly_once(tmp_path: Path) -> None:
    """Two workers contending for the same four tasks run each exactly once.

    Deterministic (not raced) regression for the fresh-selection double-claim.
    Worker B builds its per-pass snapshot while all four tasks are READY, then
    parks between snapshot and claim acquisition until worker A has driven every
    task to DONE and released each claim. ``release_claim`` deletes the claim
    row, so when B wakes it freely re-acquires each freed claim on its stale
    (all-READY) snapshot -- the exact TOCTOU that flaked under load's overlap.
    The post-claim terminal-state recheck must make B decline every already-DONE
    task rather than mint a second run, so together the workers cover the task
    set exactly once.

    The interleave is forced by a controlled clock (not a sleep-based race),
    mirroring ``test_fresh_selection_rechecks_terminal_state_under_claim``: B's
    clock parks it on its first ``acquire_claim`` until A signals it has drained
    and released the whole set.
    """
    import threading

    tasks_dir = tmp_path / "tasks"
    phase = tasks_dir / "active" / "01-phase"
    task_ids = ["t1", "t2", "t3", "t4"]
    for tid in task_ids:
        _write_task(phase, tid)
    db_path = tmp_path / "flywheel.sqlite"

    b_snapshot_taken = threading.Event()
    a_finished_all = threading.Event()

    class _SignalWhenDrainedSource:
        """Worker A's source. One task runs per pass, so for N tasks the
        (N+1)th ``list_work`` is the terminal no-progress pass -- fired only
        after every task is committed DONE and every claim released. That call
        signals B that the whole set is drained."""

        def __init__(self) -> None:
            self._inner = DirectoryWorkSource(tasks_dir)
            self._calls = 0

        def list_work(self):
            items = self._inner.list_work()
            self._calls += 1
            if self._calls == len(task_ids) + 1:
                a_finished_all.set()
            return items

        def report(self, report) -> None:  # type: ignore[no-untyped-def]
            self._inner.report(report)

    async def _invoke_a(request: InvocationRequest) -> IterationResult:
        # Do not finalize the first task until B has taken its (all-READY)
        # snapshot, so B is guaranteed to select off the stale view.
        b_snapshot_taken.wait(5)
        return _verify_result()

    async def _invoke_b(request: InvocationRequest) -> IterationResult:
        return _verify_result()

    # Worker B's clock parks it between snapshot and claim. The first call is
    # the per-pass graph snapshot, fired right AFTER B's states snapshot is
    # built -> signal and proceed. Every later call (the next is acquire_claim)
    # waits until A has drained and released the whole set, so B re-acquires each
    # freed (deleted) claim on its stale snapshot.
    t0 = datetime(2026, 1, 1, tzinfo=timezone.utc)

    def _clock_b() -> datetime:
        if not b_snapshot_taken.is_set():
            b_snapshot_taken.set()
            return t0
        a_finished_all.wait(5)
        return t0

    results: dict[str, object] = {}

    def _run(worker_id, source, invoke, clock=None) -> None:  # type: ignore[no-untyped-def]
        kwargs: dict[str, object] = dict(
            source=source,
            db_path=db_path,
            sandbox_root=tmp_path / "sb" / worker_id,
            invoke=invoke,
            worker_id=worker_id,
            max_retries=0,
            max_turns=4,
            lease_seconds=60,
            stream=io.StringIO(),
        )
        if clock is not None:
            kwargs["now"] = clock
        results[worker_id] = asyncio.run(orchestrate(**kwargs))  # type: ignore[arg-type]

    ta = threading.Thread(
        target=_run,
        args=("worker-a", _SignalWhenDrainedSource(), _invoke_a),
    )
    tb = threading.Thread(
        target=_run,
        args=("worker-b", DirectoryWorkSource(tasks_dir), _invoke_b),
        kwargs={"clock": _clock_b},
    )
    tb.start()
    ta.start()
    ta.join(30)
    tb.join(30)
    assert not ta.is_alive() and not tb.is_alive()

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


def test_fresh_selection_rechecks_terminal_state_under_claim(
    tmp_path: Path,
) -> None:
    """A peer that completes a task between this worker's snapshot read and
    its claim acquisition must NOT cause a re-run.

    Deterministic regression for the stale-snapshot TOCTOU in the
    fresh-selection path (the load-only flake of
    ``test_two_workers_run_each_task_exactly_once``): ``states`` is read once
    per scheduling pass, and ``release_claim`` deletes the claim row, so the
    claim layer keeps no memory that a task already finished. Worker B selects
    t1 off a snapshot that still shows it READY, then -- after worker A drives
    t1 to DONE and releases -- freely re-acquires the deleted claim. The fresh
    path must re-check the picked task's terminal state under the claim and
    decline, exactly as the resume paths already do.

    The interleaving is forced (not raced): B parks between building its
    snapshot and acquiring its claim until A has finished and released.
    """
    import threading

    tasks_dir = tmp_path / "tasks"
    phase = tasks_dir / "active" / "01-phase"
    _write_task(phase, "t1")
    db_path = tmp_path / "flywheel.sqlite"

    b_snapshot_taken = threading.Event()
    a_released = threading.Event()

    class _SignalOnSecondListSource:
        """Worker A's source: its second ``list_work`` (pass 2, after A has
        driven t1 to DONE and released the claim) signals that the claim is
        free and t1 is committed DONE."""

        def __init__(self) -> None:
            self._inner = DirectoryWorkSource(tasks_dir)
            self._calls = 0

        def list_work(self):
            items = self._inner.list_work()
            self._calls += 1
            if self._calls == 2:
                a_released.set()
            return items

        def report(self, report) -> None:  # type: ignore[no-untyped-def]
            self._inner.report(report)

    async def _invoke_a(request: InvocationRequest) -> IterationResult:
        # Do not finalize t1 until B has taken its (t1-is-READY) snapshot, so
        # B is guaranteed to select t1 off the stale view.
        b_snapshot_taken.wait(5)
        return _verify_result()

    async def _invoke_b(request: InvocationRequest) -> IterationResult:
        return _verify_result()

    # Worker B's clock parks it between snapshot and claim. The first call is
    # the per-pass graph snapshot, fired right AFTER B's states snapshot is
    # built -> signal and proceed. Every later call (the next is acquire_claim)
    # waits until A has released, so B acquires the freed (deleted) claim on a
    # stale snapshot.
    t0 = datetime(2026, 1, 1, tzinfo=timezone.utc)

    def _clock_b() -> datetime:
        if not b_snapshot_taken.is_set():
            b_snapshot_taken.set()
            return t0
        a_released.wait(5)
        return t0

    results: dict[str, object] = {}

    def _run(worker_id: str, source, invoke, clock=None) -> None:  # type: ignore[no-untyped-def]
        kwargs: dict[str, object] = dict(
            source=source,
            db_path=db_path,
            sandbox_root=tmp_path / "sb" / worker_id,
            invoke=invoke,
            worker_id=worker_id,
            max_retries=0,
            max_turns=4,
            lease_seconds=60,
            stream=io.StringIO(),
        )
        if clock is not None:
            kwargs["now"] = clock
        results[worker_id] = asyncio.run(orchestrate(**kwargs))  # type: ignore[arg-type]

    ta = threading.Thread(
        target=_run,
        args=("worker-a", _SignalOnSecondListSource(), _invoke_a),
    )
    tb = threading.Thread(
        target=_run,
        args=("worker-b", DirectoryWorkSource(tasks_dir), _invoke_b),
        kwargs={"clock": _clock_b},
    )
    tb.start()
    ta.start()
    ta.join(30)
    tb.join(30)
    assert not ta.is_alive() and not tb.is_alive()

    ran_a = {r.task_id for r in results["worker-a"].runs}  # type: ignore[attr-defined]
    ran_b = {r.task_id for r in results["worker-b"].runs}  # type: ignore[attr-defined]
    # t1 runs exactly once: A completes it; B observes the committed DONE under
    # its claim and declines to re-run it.
    assert ran_a == {"t1"}
    assert ran_b == set()


def test_claim_lost_mid_run_relinquishes_without_killing_worker(
    tmp_path: Path, monkeypatch
) -> None:
    # A peer stealing one task's lease mid-run surfaces as
    # OptimisticConcurrencyError out of _drive_under_lease. That must NOT
    # unwind out of orchestrate and abandon the worker's other tasks: the
    # losing task is relinquished (recorded as no run) and the loop carries
    # on to the next claimable task.
    import flywheel_orchestrator._orchestrate as orch
    from flywheel_core import OptimisticConcurrencyError

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


# --- per-pass graph snapshots (spec 00055) ---------------------------------


def test_each_scheduling_pass_records_a_graph_snapshot(tmp_path: Path) -> None:
    """Criterion #7: driving a single fresh task to its no-progress return
    leaves a non-empty snapshot stream whose latest snapshot's item set equals
    the source's work items with their (terminal) state."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "solo")

    report = _orchestrate(tmp_path, _always_verify())
    assert [r.task_id for r in report.runs] == ["solo"]
    assert report.runs[0].status is Status.DONE

    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        snapshots = claims.list_graph_snapshots()
        assert snapshots, "a driven run must leave at least one snapshot"
        latest = claims.latest_graph_snapshot()
        assert latest is not None
        items = claims.list_graph_snapshot_items(latest.id)
        # Item set equals the pass's work items, with their terminal state.
        assert {i.task_id for i in items} == {"solo"}
        solo = next(i for i in items if i.task_id == "solo")
        assert solo.state == "done"
        # A done task is absent from the ready set.
        assert solo.ready is False
        assert solo.claim_holder is None  # the lease was released
    finally:
        claims.close()


def test_successive_snapshots_track_the_graph_evolving(tmp_path: Path) -> None:
    """Criterion #8: across a two-task chain (B depends on A) an earlier
    snapshot shows A not-done with B not ready, while a later, distinct
    snapshot shows A done with B ready -- proving capture is per-pass, not
    cached or recorded once."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "a")
    _write_task(phase, "b", prerequisites=["a"])

    report = _orchestrate(tmp_path, _always_verify())
    assert [r.task_id for r in report.runs] == ["a", "b"]

    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        snapshots = claims.list_graph_snapshots()
        assert len(snapshots) >= 2, "per-pass capture must leave >1 snapshot"

        def by_task(snapshot_id: int) -> dict[str, object]:
            return {
                i.task_id: i
                for i in claims.list_graph_snapshot_items(snapshot_id)
            }

        early_id = next(
            (
                s.id
                for s in snapshots
                if (m := by_task(s.id))["a"].state != "done"  # type: ignore[attr-defined]
                and not m["b"].ready  # type: ignore[attr-defined]
            ),
            None,
        )
        late_id = next(
            (
                s.id
                for s in snapshots
                if (m := by_task(s.id))["a"].state == "done"  # type: ignore[attr-defined]
                and m["b"].ready  # type: ignore[attr-defined]
            ),
            None,
        )
        assert early_id is not None, (
            "an early pass must show A not-done and B not ready"
        )
        assert late_id is not None, (
            "a later pass must show A done and B ready"
        )
        # Distinct snapshots: a single cached/first-pass-only snapshot fails.
        assert early_id != late_id
    finally:
        claims.close()


def test_empty_source_still_records_a_terminal_snapshot(
    tmp_path: Path,
) -> None:
    """Criterion #11: an empty source makes no progress, but the terminal
    no-progress pass still records its (empty) cross-section."""
    (tmp_path / "tasks" / "active").mkdir(parents=True)

    report = _orchestrate(tmp_path, _always_verify())
    assert report.runs == ()

    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        snapshots = claims.list_graph_snapshots()
        assert len(snapshots) == 1
        assert snapshots[0].item_count == 0
        assert claims.list_graph_snapshot_items(snapshots[0].id) == []
    finally:
        claims.close()


def test_snapshot_captured_at_uses_the_injected_clock(tmp_path: Path) -> None:
    """The capture timestamp is the injected ``now``, never a wall clock, so
    driven-test assertions are deterministic."""
    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "solo")
    fixed = datetime(2026, 6, 25, 12, 0, 0, tzinfo=timezone.utc)

    asyncio.run(
        orchestrate(
            tasks_dir=tmp_path / "tasks",
            db_path=tmp_path / "flywheel.sqlite",
            sandbox_root=tmp_path / "sandboxes",
            invoke=_always_verify(),
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            now=lambda: fixed,
        )
    )

    claims = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        snapshots = claims.list_graph_snapshots()
        assert snapshots
        assert all(s.captured_at == fixed for s in snapshots)
    finally:
        claims.close()


def test_schema_mismatch_permanent_stop(tmp_path: Path) -> None:
    # A store whose schema_version row is wrong: reopening it raises
    # StoreSchemaError, which classify_fault buckets PERMANENT. The driver
    # reopens the store at the top of every pass, so the same error would raise
    # identically on cycles 1..5 -- burning ALL five breaker strikes (with a
    # backoff between each) if it were treated as a transient cycle failure. The
    # permanent-stop path must instead stop after exactly ONE cycle via a
    # distinct signal, never retry it, and never reach the transient give-up.
    import sqlite3

    from flywheel_core.store_protocols import (
        CURRENT_SCHEMA_VERSION,
        StoreSchemaError,
    )
    from flywheel_orchestrator._autopilot import AutopilotPassResult
    from flywheel_orchestrator._autopilot_run import (
        MAX_CONSECUTIVE_CYCLE_FAILURES,
        run_daemon_loop,
    )

    db_path = tmp_path / "flywheel.sqlite"
    # Materialize a valid store, then rewrite its schema_version to a value that
    # is neither CURRENT nor the one supported forward-migration source (11), so
    # a subsequent open is refused with StoreSchemaError rather than upgraded.
    SqliteStore(db_path).close()
    bad_version = CURRENT_SCHEMA_VERSION + 1000
    conn = sqlite3.connect(str(db_path))
    conn.execute(
        "UPDATE schema_version SET version = ? WHERE id = 1", (bad_version,)
    )
    conn.commit()
    conn.close()

    def run_cycle() -> AutopilotPassResult:
        # Mirrors the driver reopening the store at the top of each pass: the
        # store open raises StoreSchemaError before any work is listed, so this
        # never returns normally.
        _orchestrate(tmp_path, _always_verify())
        raise AssertionError("unreachable: store open must have raised")

    permanent: list[BaseException] = []
    strikes: list[int] = []
    gave_up: list[int] = []

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=lambda: False,
        sleep=lambda _seconds, _stop: None,
        on_cycle_failure=lambda exc, n: strikes.append(n),
        on_give_up=gave_up.append,
        on_permanent_stop=permanent.append,
        # Safety bound so a regression that keeps retrying cannot loop forever;
        # the assertion below proves the loop stopped at 1, well under this.
        max_cycles=MAX_CONSECUTIVE_CYCLE_FAILURES,
    )

    # Exactly ONE cycle -- not MAX_CONSECUTIVE_CYCLE_FAILURES strikes.
    assert cycles == 1
    # The permanent-stop signal fired once, carrying the schema mismatch.
    assert len(permanent) == 1
    assert isinstance(permanent[0], StoreSchemaError)
    # And it took the permanent path, NOT the transient strike / give-up path.
    assert strikes == []
    assert gave_up == []


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


def test_heartbeat_signals_lost_when_lease_stolen(tmp_path: Path) -> None:
    # H3: renewal against a lease a peer already stole must set lost(), so the
    # driving coroutine can stop before landing/reporting instead of only
    # discovering the loss via the lifecycle version CAS race.
    from datetime import timedelta

    from flywheel_orchestrator._orchestrate import _ClaimHeartbeat

    store = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        start = datetime.now(timezone.utc)
        claim = store.acquire_claim("t", "w", now=start, lease_seconds=10)
        assert claim is not None
        # A peer steals the now-lapsed lease: the row becomes w2/v2, so the
        # original token no longer matches and renewal raises ClaimLostError.
        stolen = store.acquire_claim(
            "t", "w2", now=start + timedelta(seconds=20), lease_seconds=10
        )
        assert stolen is not None and stolen.worker_id == "w2"

        heartbeat = _ClaimHeartbeat(
            claims=store,
            claim=claim,
            lease_seconds=10,
            interval=0.02,
            now=lambda: start + timedelta(seconds=21),
        ).start()
        try:
            deadline = time.monotonic() + 2.0
            while not heartbeat.lost() and time.monotonic() < deadline:
                time.sleep(0.01)
            assert heartbeat.lost()
        finally:
            heartbeat.stop()
    finally:
        store.close()


def test_mid_run_claim_loss_relinquishes_without_landing(
    tmp_path: Path, monkeypatch
) -> None:
    # H3: if the heartbeat observes the lease was stolen while the run was in
    # flight, _drive_under_lease must relinquish (raise ClaimLostError ->
    # contained to None by _drive_or_relinquish) rather than land/report a run
    # whose claim a peer now owns. Simulate the loss with a heartbeat whose
    # lost() is always True; the run finalizes but must not be recorded.
    import flywheel_orchestrator._orchestrate as orch

    class _LostHeartbeat:
        def __init__(self, *, claims, claim, lease_seconds, interval, now):  # type: ignore[no-untyped-def]
            self._claim = claim

        def start(self) -> "_LostHeartbeat":
            return self

        def lost(self) -> bool:
            return True

        def stop(self):  # type: ignore[no-untyped-def]
            return self._claim

    monkeypatch.setattr(orch, "_ClaimHeartbeat", _LostHeartbeat)

    phase = tmp_path / "tasks" / "active" / "01-phase"
    _write_task(phase, "solo")

    # orchestrate returns normally (the worker is not killed) and records no run
    # for the relinquished task.
    report = _orchestrate(tmp_path, _always_verify())
    assert report.runs == ()

    # The stale claim is still released on the way out (finally path).
    store = SqliteClaimStore(tmp_path / "flywheel.sqlite")
    try:
        assert store.load_claim("solo") is None
    finally:
        store.close()
