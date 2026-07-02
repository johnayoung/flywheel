"""Tests for autopilot console activation (spec 00058, autopilot-activation).

Grades acceptance criterion #10 and decision D-6: the console start action
spawns the neverending autopilot daemon as a detached supervised child the
supervisor reports live; the stop action signals it; the start dispatch targets
the autopilot *daemon* entry (no ``--once``), not a single pass or the worker;
and autopilot is an independent supervised child from the worker.

Mirrors the worker-supervisor test harness: a trivial sleeping child stands in
for the real daemon so spawn / detach / stop are exercised without launching it.
"""

from __future__ import annotations

import contextlib
import os
import signal
import sys
import time
from pathlib import Path

from flywheel._autopilot_supervisor import (
    AutopilotState,
    AutopilotSupervisor,
    build_autopilot_spawn_argv,
)
from flywheel._dashboard import DashboardApp
from flywheel_orchestrator._autopilot_activity import (
    PHASE_IDLE,
    AutopilotActivity,
    write_activity,
)


def _sleep_argv(seconds: int = 60) -> list[str]:
    return [sys.executable, "-c", f"import time; time.sleep({seconds})"]


def _process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def _wait_for_exit(pid: int, *, timeout: float = 5.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not _process_alive(pid):
            return True
        time.sleep(0.05)
    return False


# --- the spawn argv targets the neverending daemon, not a single pass -------


def test_spawn_argv_targets_the_daemon_not_once() -> None:
    argv = build_autopilot_spawn_argv()
    assert argv[1:3] == ["-m", "flywheel_orchestrator._autopilot_run"]
    # The defends-against for criterion #10: the console must launch the
    # neverending daemon, never a one-shot --once pass.
    assert "--once" not in argv


def test_spawn_argv_forwards_tasks_dir_and_model(tmp_path) -> None:
    argv = build_autopilot_spawn_argv(tasks_dir=tmp_path, model="claude-x")
    assert "--tasks-dir" in argv and str(tmp_path) in argv
    assert "--model" in argv and "claude-x" in argv
    assert "--once" not in argv


# --- spawn / detach / stop --------------------------------------------------


def test_start_spawns_a_live_supervised_child(tmp_path) -> None:
    sup = AutopilotSupervisor(log_dir=tmp_path, spawn_argv=_sleep_argv())
    try:
        status = sup.start()
        assert status.state == AutopilotState.SUPERVISED
        assert status.pid is not None
        assert _process_alive(status.pid)
        # The supervisor reports it live.
        assert sup.status().state == AutopilotState.SUPERVISED
        assert sup.owns_supervised_child()
    finally:
        sup.stop(timeout=5.0)


def test_idempotent_start_does_not_spawn_a_second_daemon(tmp_path) -> None:
    sup = AutopilotSupervisor(log_dir=tmp_path, spawn_argv=_sleep_argv())
    try:
        first = sup.start()
        second = sup.start()
        assert first.pid == second.pid  # same child; no second spawn
    finally:
        sup.stop(timeout=5.0)


def test_detached_child_survives_console_exit(tmp_path) -> None:
    sup = AutopilotSupervisor(log_dir=tmp_path, spawn_argv=_sleep_argv())
    status = sup.start()
    pid = status.pid
    assert pid is not None
    # Detach: the console can exit while autopilot keeps running.
    sup.detach()
    assert sup.status().state == AutopilotState.NONE
    assert _process_alive(pid)  # still running after detach
    # Clean up the orphan ourselves (the test, unlike a real console, stays the
    # child's OS parent, so we must reap it to avoid a lingering zombie).
    os.kill(pid, signal.SIGKILL)
    with contextlib.suppress(ChildProcessError):
        os.waitpid(pid, 0)


def test_stop_signals_the_child_to_exit(tmp_path) -> None:
    sup = AutopilotSupervisor(log_dir=tmp_path, spawn_argv=_sleep_argv())
    status = sup.start()
    pid = status.pid
    assert pid is not None
    assert sup.stop(timeout=5.0) is True
    assert _wait_for_exit(pid)
    assert sup.status().state == AutopilotState.DEAD


def test_stop_with_no_child_is_a_clean_noop(tmp_path) -> None:
    sup = AutopilotSupervisor(log_dir=tmp_path, spawn_argv=_sleep_argv())
    assert sup.stop() is False  # nothing to stop, no error


# --- cross-process adoption via the liveness record -------------------------


def _write_record(path: Path, *, pid: int, expires_at: float) -> None:
    """Publish a liveness record at ``path`` with an explicit freshness deadline.

    Uses a wall-clock-relative ``expires_at`` so the supervisor's default
    ``read_live_activity(now=time.time())`` reads it live (future) or stale
    (past) deterministically without injecting a clock.
    """
    write_activity(
        path,
        AutopilotActivity(
            pid=pid,
            phase=PHASE_IDLE,
            cycle_index=3,
            updated_at=time.time(),
            interval_seconds=300.0,
            next_cycle_at=expires_at,
            expires_at=expires_at,
        ),
    )


def test_status_adopts_a_live_foreign_record_as_detached(tmp_path) -> None:
    """A live record from a daemon this console never spawned reads DETACHED."""
    activity_path = tmp_path / "activity.json"
    _write_record(activity_path, pid=9999, expires_at=time.time() + 3600)
    sup = AutopilotSupervisor(
        log_dir=tmp_path, activity_path=activity_path, spawn_argv=_sleep_argv()
    )
    status = sup.status()
    assert status.state == AutopilotState.DETACHED
    assert status.pid == 9999  # the adopted daemon's pid, not one we own
    assert status.activity is not None and status.activity.pid == 9999
    assert not sup.owns_supervised_child()


def test_start_adopts_a_live_record_and_spawns_nothing(tmp_path) -> None:
    """start() on a live foreign record adopts it -- no duplicate daemon."""
    activity_path = tmp_path / "activity.json"
    _write_record(activity_path, pid=9999, expires_at=time.time() + 3600)
    sentinel = tmp_path / "spawned.marker"
    spawn_argv = [
        sys.executable,
        "-c",
        f"open({str(sentinel)!r}, 'w').close()",
    ]
    sup = AutopilotSupervisor(
        log_dir=tmp_path, activity_path=activity_path, spawn_argv=spawn_argv
    )
    status = sup.start()
    assert status.state == AutopilotState.DETACHED
    # The adopt branch returns before _spawn_child, so the sentinel child that a
    # spawn would have created never runs.
    assert not sentinel.exists()


def test_stale_record_does_not_block_a_respawn(tmp_path) -> None:
    """A dead daemon's lapsed record must not stop start() from spawning."""
    activity_path = tmp_path / "activity.json"
    _write_record(activity_path, pid=9999, expires_at=time.time() - 3600)
    sup = AutopilotSupervisor(
        log_dir=tmp_path, activity_path=activity_path, spawn_argv=_sleep_argv()
    )
    try:
        status = sup.start()
        # Stale record -> not-live -> a real child is spawned (not adopted).
        assert status.state == AutopilotState.SUPERVISED
        assert status.pid is not None and _process_alive(status.pid)
    finally:
        sup.stop(timeout=5.0)


def test_missing_record_reads_none_not_detached(tmp_path) -> None:
    """With no owned child and no record, status is NONE (nothing to adopt)."""
    sup = AutopilotSupervisor(
        log_dir=tmp_path,
        activity_path=tmp_path / "activity.json",
        spawn_argv=_sleep_argv(),
    )
    assert sup.status().state == AutopilotState.NONE


def test_owned_death_takes_precedence_over_a_live_foreign_record(tmp_path) -> None:
    """A just-died owned child reads DEAD even when a live foreign record exists.

    Mirrors the worker ordering: local death state is resolved before the
    cross-process record, so a peer daemon's record never masks the death of the
    child this console owned (the activity-pid distinction in effect).
    """
    activity_path = tmp_path / "activity.json"
    sup = AutopilotSupervisor(
        log_dir=tmp_path, activity_path=activity_path, spawn_argv=_sleep_argv()
    )
    status = sup.start()
    pid = status.pid
    assert pid is not None
    assert sup.stop(timeout=5.0) is True
    assert _wait_for_exit(pid)
    # A different, live daemon publishes its record after our child died.
    _write_record(activity_path, pid=pid + 1, expires_at=time.time() + 3600)
    assert sup.status().state == AutopilotState.DEAD


# --- console action dispatch (the operator's /autopilot start|stop) ---------


def _app_with_stub_supervisor():
    """A DashboardApp wired to a scripted autopilot supervisor."""
    calls: dict[str, int] = {"start": 0, "stop": 0}
    state = {"status": AutopilotState.NONE, "pid": None}

    from flywheel._autopilot_supervisor import AutopilotStatus

    def status() -> AutopilotStatus:
        return AutopilotStatus(state=state["status"], pid=state["pid"])

    def start() -> AutopilotStatus:
        calls["start"] += 1
        state["status"] = AutopilotState.SUPERVISED
        state["pid"] = 4242
        return status()

    def stop() -> bool:
        calls["stop"] += 1
        state["status"] = AutopilotState.DEAD
        return True

    app = DashboardApp(
        poll=lambda: None,  # type: ignore[arg-type,return-value]
        autopilot_status=status,
        autopilot_start=start,
        autopilot_stop=stop,
    )
    return app, calls


def test_console_start_action_spawns_autopilot() -> None:
    app, calls = _app_with_stub_supervisor()
    notice = app.handle_autopilot_slash("start")
    assert calls["start"] == 1
    assert "supervised" in notice
    assert "pid=4242" in notice


def test_console_stop_action_signals_autopilot() -> None:
    app, calls = _app_with_stub_supervisor()
    app.handle_autopilot_slash("start")
    notice = app.handle_autopilot_slash("stop")
    assert calls["stop"] == 1
    assert "terminated gracefully" in notice


def test_console_stop_with_no_live_autopilot_is_a_clean_notice() -> None:
    app, _ = _app_with_stub_supervisor()
    notice = app.handle_autopilot_slash("stop")
    assert "no supervised autopilot to stop" in notice


def test_autopilot_not_wired_degrades_to_a_notice() -> None:
    app = DashboardApp(poll=lambda: None)  # type: ignore[arg-type,return-value]
    assert "not wired" in app.handle_autopilot_slash("start")


def test_worker_and_autopilot_are_independent_children() -> None:
    """Stopping autopilot does not touch the worker seam, and vice versa."""
    worker_calls = {"stop": 0}
    ap_calls = {"stop": 0}

    from flywheel._autopilot_supervisor import AutopilotStatus
    from flywheel._worker_supervisor import WorkerState, WorkerStatus

    def worker_status() -> WorkerStatus:
        return WorkerStatus(state=WorkerState.SUPERVISED, pid=1)

    def worker_stop() -> bool:
        worker_calls["stop"] += 1
        return True

    def ap_status() -> AutopilotStatus:
        return AutopilotStatus(state=AutopilotState.SUPERVISED, pid=2)

    def ap_stop() -> bool:
        ap_calls["stop"] += 1
        return True

    app = DashboardApp(
        poll=lambda: None,  # type: ignore[arg-type,return-value]
        worker_status=worker_status,
        worker_stop=worker_stop,
        autopilot_status=ap_status,
        autopilot_stop=ap_stop,
    )
    app.handle_autopilot_slash("stop")
    assert ap_calls["stop"] == 1
    assert worker_calls["stop"] == 0  # the worker was untouched

    app.handle_worker_slash("stop")
    assert worker_calls["stop"] == 1
    assert ap_calls["stop"] == 1  # autopilot was not stopped again
