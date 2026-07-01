"""In-loop expired-lease sweep (spec 00069, the lease-sweep layer).

Proves the two lease-sweep criteria with a fully injected clock so lease lapse
is deterministic (no wall-clock waits for expiry):

* #1 -- while the loop drives, a bounded-cadence sweep finalizes a stranded
  lifecycle through the sanctioned ``finalize_stranded_lifecycle`` path and
  releases its lapsed claim, so the task returns to an eligible (INTERRUPTED)
  state WITHOUT any worker re-selecting that exact task.
* #2 -- a still-live (non-lapsed) claim and its lifecycle are byte-identical
  before and after a sweep cycle; a task a live owner is actively running is
  never swept or reclaimed.

The direct-unit tests drive ``sweep_expired_leases`` / ``_lease_sweep_loop``
with a frozen clock; the integration test drives the real ``orchestrate`` loop
and advances an injected clock from inside the agent invoke, so the strand is
finalized by the in-loop sweeper -- not the entry-time backstop.
"""

from __future__ import annotations

import asyncio
import io
import json
import sqlite3
import time
from contextlib import suppress
from datetime import datetime, timedelta, timezone
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ResultMessage, TextBlock

from flywheel_core import (
    Intent,
    InvocationRequest,
    InvocationSignals,
    IterationResult,
    Lifecycle,
    Status,
    ValidEnvelope,
)
from flywheel_core.store_sqlite import SqliteStore
from flywheel_orchestrator import SqliteClaimStore, orchestrate
from flywheel_orchestrator._orchestrate import (
    _lease_sweep_loop,
    sweep_expired_leases,
)

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- fixtures / helpers -----------------------------------------------------


def _frozen(at: datetime):
    def _now() -> datetime:
        return at

    return _now


def _seed_stranded_run(
    store: SqliteStore, task_id: str, run_id: str, *, at: datetime
) -> None:
    """Create a lifecycle stranded mid-attempt in RUNNING (worker died)."""
    lc = Lifecycle(task_id=task_id, run_id=run_id)
    lc.transition_to(Status.READY, now=at)
    lc.transition_to(Status.RUNNING, now=at)
    store.create_lifecycle(lc)


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


# --- criterion #1: in-loop sweep finalizes a stranded lifecycle -------------


def test_sweep_finalizes_claimable_stranded_lifecycle(tmp_path: Path) -> None:
    """A stranded RUNNING lifecycle whose lease has lapsed is finalized to
    INTERRUPTED via the sanctioned path and its claim is released, so the task
    is eligible again (criterion #1)."""
    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    control = SqliteStore(db_path)
    claims = SqliteClaimStore(db_path)
    try:
        _seed_stranded_run(control, "ghost", "run-ghost", at=_BASE)
        # A dead worker's lease, already lapsed by sweep time.
        claim = claims.acquire_claim(
            "ghost", "dead-worker", now=_BASE, lease_seconds=10
        )
        assert claim is not None

        sweep_at = _BASE + timedelta(seconds=100)
        recovered, released = sweep_expired_leases(
            control,
            claims,
            "worker-a-sweeper",
            lease_seconds=3600,
            now=_frozen(sweep_at),
        )

        assert recovered == ("run-ghost",)
        assert released == ()
        # Finalized through finalize_stranded_lifecycle, not a direct write.
        lc = control.load_lifecycle("run-ghost")
        assert lc is not None
        assert lc.status is Status.INTERRUPTED
        # The lease was released, so the task holds no claim and is selectable.
        assert claims.load_claim("ghost") is None
    finally:
        control.close()
        claims.close()


def test_sweep_reaps_orphan_lapsed_lease(tmp_path: Path) -> None:
    """A lapsed lease with no stranded lifecycle behind it (a dead worker's
    leaked claim) is reaped by the batch sweep so its conflict keys are freed."""
    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    control = SqliteStore(db_path)
    claims = SqliteClaimStore(db_path)
    try:
        claim = claims.acquire_claim(
            "orphan", "dead-worker", now=_BASE, lease_seconds=10
        )
        assert claim is not None

        sweep_at = _BASE + timedelta(seconds=100)
        recovered, released = sweep_expired_leases(
            control,
            claims,
            "worker-a-sweeper",
            lease_seconds=3600,
            now=_frozen(sweep_at),
        )

        # No stranded lifecycle to finalize, but the lapsed claim is reaped.
        assert recovered == ()
        assert released == ("orphan",)
        assert claims.load_claim("orphan") is None
    finally:
        control.close()
        claims.close()


# --- criterion #2: a live claim + lifecycle are untouched -------------------


def test_sweep_leaves_live_claim_and_lifecycle_byte_identical(
    tmp_path: Path,
) -> None:
    """A task a live owner is actively running -- a future lease plus a RUNNING
    lifecycle -- is neither reclaimed nor finalized. The claim row and the
    lifecycle status are byte-identical before and after the sweep, even though
    the sweeper's id shares the owner's base id (criterion #2)."""
    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    control = SqliteStore(db_path)
    claims = SqliteClaimStore(db_path)
    try:
        _seed_stranded_run(control, "owned", "run-live", at=_BASE)
        # The primary owner holds a live lease that outlives the sweep.
        held = claims.acquire_claim(
            "owned", "worker-a", now=_BASE, lease_seconds=3600
        )
        assert held is not None
        claim_before = claims.load_claim("owned")
        status_before = control.load_lifecycle("run-live")
        assert status_before is not None

        # The production sweeper claims under "<wid>-sweeper", i.e. a different
        # worker id than the live owner "worker-a". Its lease is still in the
        # future, so it must look like a peer's live claim and be skipped.
        sweep_at = _BASE + timedelta(seconds=100)
        recovered, released = sweep_expired_leases(
            control,
            claims,
            "worker-a-sweeper",
            lease_seconds=3600,
            now=_frozen(sweep_at),
        )

        assert recovered == ()
        assert released == ()
        # Byte-identical: TaskClaim is a frozen dataclass, so equality is a
        # field-for-field snapshot comparison.
        assert claims.load_claim("owned") == claim_before
        lc_after = control.load_lifecycle("run-live")
        assert lc_after is not None
        assert lc_after.status is Status.RUNNING
        assert lc_after.status is status_before.status
    finally:
        control.close()
        claims.close()


# --- the async sweep loop ticks on its cadence ------------------------------


def test_lease_sweep_loop_finalizes_on_cadence(tmp_path: Path) -> None:
    """The background loop mirrors the reconciler: it ticks the sweep on a
    bounded interval and finalizes a lapsed strand without external prompting,
    and exits cleanly on cancellation."""
    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    control = SqliteStore(db_path)
    claims = SqliteClaimStore(db_path)
    try:
        _seed_stranded_run(control, "ghost", "run-ghost", at=_BASE)
        claim = claims.acquire_claim(
            "ghost", "dead-worker", now=_BASE, lease_seconds=10
        )
        assert claim is not None
        sweep_at = _BASE + timedelta(seconds=100)

        async def _drive() -> Status | None:
            task = asyncio.create_task(
                _lease_sweep_loop(
                    control=control,
                    claims=claims,
                    worker_id="worker-a-sweeper",
                    lease_seconds=3600,
                    interval=0.01,
                    now=_frozen(sweep_at),
                    sink=None,
                    stream=None,
                )
            )
            try:
                deadline = time.monotonic() + 5.0
                while True:
                    await asyncio.sleep(0.01)
                    lc = control.load_lifecycle("run-ghost")
                    if lc is not None and lc.status is Status.INTERRUPTED:
                        return lc.status
                    if time.monotonic() > deadline:
                        return None if lc is None else lc.status
            finally:
                task.cancel()
                with suppress(asyncio.CancelledError):
                    await task

        final = asyncio.run(_drive())
        assert final is Status.INTERRUPTED
        assert claims.load_claim("ghost") is None
    finally:
        control.close()
        claims.close()


# --- integration: the in-loop sweeper (not entry recovery) frees the task ---


def test_orchestrate_in_loop_sweep_finalizes_strand_mid_drive(
    tmp_path: Path,
) -> None:
    """End-to-end through ``orchestrate``: a strand whose lease is still LIVE at
    entry is skipped by the entry-time backstop, then finalized by the in-loop
    sweeper once the injected clock advances past the lease -- proving the
    sweep is wired into the steady-state loop and is additive to entry recovery.
    """
    db_path = tmp_path / "flywheel.sqlite"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    tasks_dir = tmp_path / "tasks"
    phase = tasks_dir / "active" / "01-phase"
    phase.mkdir(parents=True, exist_ok=True)
    (phase / "real.json").write_text(
        json.dumps(
            {
                "id": "real",
                "goal": "Goal for real.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )

    # Seed a strand whose lease is LIVE at orchestrate entry (base + 60s), held
    # by a peer worker. The entry-time _recover_claimable_stranded runs at the
    # base clock and cannot claim it (live lease, different worker), so it is
    # left for the in-loop sweep.
    seed = SqliteStore(db_path)
    seed_claims = SqliteClaimStore(db_path)
    try:
        _seed_stranded_run(seed, "ghost", "run-ghost", at=_BASE)
        held = seed_claims.acquire_claim(
            "ghost", "peer-worker", now=_BASE, lease_seconds=60
        )
        assert held is not None
    finally:
        seed.close()
        seed_claims.close()

    # An injected clock the agent invoke advances mid-drive. Once it passes the
    # ghost lease (base + 60s), the in-loop sweeper reclaims and finalizes it.
    holder = {"t": _BASE}

    def _clock() -> datetime:
        return holder["t"]

    def _ghost_status() -> Status | None:
        # A short-lived reader connection; tolerate a transient WAL lock while
        # the sweeper/heartbeat write, and retry on the next poll.
        try:
            reader = SqliteStore(db_path)
        except sqlite3.OperationalError:
            return None
        try:
            lc = reader.load_lifecycle("run-ghost")
            return lc.status if lc is not None else None
        except sqlite3.OperationalError:
            return None
        finally:
            reader.close()

    async def _invoke(request: InvocationRequest) -> IterationResult:
        # Advance the clock past the ghost lease so the in-loop sweep can lapse
        # it, then hold the "real" drive open until the sweeper has finalized
        # the ghost strand -- keeping the loop alive across a sweep tick.
        holder["t"] = _BASE + timedelta(seconds=3000)
        deadline = time.monotonic() + 5.0
        while _ghost_status() is not Status.INTERRUPTED:
            if time.monotonic() > deadline:
                raise AssertionError(
                    "in-loop sweep did not finalize the ghost strand"
                )
            await asyncio.sleep(0.01)
        return _verify_result()

    report = asyncio.run(
        orchestrate(
            tasks_dir=tasks_dir,
            db_path=db_path,
            sandbox_root=tmp_path / "sandboxes",
            invoke=_invoke,
            max_retries=0,
            max_turns=4,
            stream=io.StringIO(),
            now=_clock,
            worker_id="worker-a",
            # A long lease keeps the real task's own claim live even after the
            # clock jumps; a tight cadence fires the sweep during the drive.
            lease_seconds=3600,
            sweep_seconds=0.01,
        )
    )

    # The entry-time backstop did NOT recover the ghost (its lease was live at
    # entry): the in-loop sweep is what finalized it.
    assert "run-ghost" not in report.recovered
    # The real task ran to completion, its own live claim untouched by the sweep.
    assert [r.task_id for r in report.runs] == ["real"]
    assert report.runs[0].status is Status.DONE

    after = SqliteStore(db_path)
    after_claims = SqliteClaimStore(db_path)
    try:
        ghost_lc = after.load_lifecycle("run-ghost")
        assert ghost_lc is not None
        assert ghost_lc.status is Status.INTERRUPTED
        # The lapsed lease was released, so the ghost task is eligible again.
        assert after_claims.load_claim("ghost") is None
    finally:
        after.close()
        after_claims.close()
