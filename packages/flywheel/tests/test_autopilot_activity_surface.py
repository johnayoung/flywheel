"""Tests for the console-side autopilot activity surface.

The supervisor folds the daemon's live activity snapshot into AutopilotStatus
(guarding against a stale file by pid), and the status line renders it as a
human-readable per-cycle summary with a ticking countdown. These cover the read
path, the pid guard, the spawn-argv wiring, and the renderer.
"""

from __future__ import annotations

import contextlib
import os
import signal
import sys

from flywheel._autopilot_supervisor import (
    AutopilotState,
    AutopilotStatus,
    AutopilotSupervisor,
    build_autopilot_spawn_argv,
)
from flywheel._dashboard import (
    _format_autopilot_activity,
    _format_autopilot_status,
)
from flywheel_orchestrator._autopilot_activity import (
    PHASE_IDLE,
    PHASE_RUNNING,
    AutopilotActivity,
    EmittedSummary,
    write_activity,
)


def _sleep_argv(seconds: int = 60) -> list[str]:
    return [sys.executable, "-c", f"import time; time.sleep({seconds})"]


def _activity(pid: int, **over: object) -> AutopilotActivity:
    base: dict[str, object] = dict(
        pid=pid,
        phase=PHASE_IDLE,
        cycle_index=3,
        updated_at=1000.0,
        interval_seconds=300.0,
        next_cycle_at=1130.0,
        last_emitted=2,
        last_dropped=1,
        last_reason="emitted 2, dropped 1",
        last_relevant_tiers=(1,),
        last_emitted_tasks=(EmittedSummary(task_id="fix-a", tier=1),),
    )
    base.update(over)
    return AutopilotActivity(**base)  # type: ignore[arg-type]


# --- spawn argv carries the activity-file path ------------------------------


def test_spawn_argv_includes_activity_file(tmp_path) -> None:
    path = tmp_path / "activity.json"
    argv = build_autopilot_spawn_argv(activity_file=path)
    assert "--activity-file" in argv
    assert str(path) in argv


def test_default_supervisor_argv_wires_the_activity_file(tmp_path) -> None:
    sup = AutopilotSupervisor(log_dir=tmp_path)
    assert "--activity-file" in sup._spawn_argv
    assert str(tmp_path / "activity.json") in sup._spawn_argv


# --- status() folds in the owned daemon's activity --------------------------


def test_status_includes_activity_when_pid_matches(tmp_path) -> None:
    path = tmp_path / "activity.json"
    sup = AutopilotSupervisor(spawn_argv=_sleep_argv(), activity_path=path)
    try:
        started = sup.start()
        assert started.pid is not None
        write_activity(path, _activity(started.pid))
        status = sup.status()
        assert status.state == AutopilotState.SUPERVISED
        assert status.activity is not None
        assert status.activity.cycle_index == 3
        assert status.activity.last_emitted == 2
    finally:
        sup.stop(timeout=5.0)


def test_status_ignores_stale_activity_from_other_pid(tmp_path) -> None:
    path = tmp_path / "activity.json"
    sup = AutopilotSupervisor(spawn_argv=_sleep_argv(), activity_path=path)
    try:
        started = sup.start()
        assert started.pid is not None
        # A snapshot left by a *different* (previous) daemon must be ignored.
        write_activity(path, _activity(started.pid + 1))
        status = sup.status()
        assert status.state == AutopilotState.SUPERVISED
        assert status.activity is None
    finally:
        sup.stop(timeout=5.0)


def test_status_without_activity_file_is_plain_supervised(tmp_path) -> None:
    path = tmp_path / "activity.json"
    sup = AutopilotSupervisor(spawn_argv=_sleep_argv(), activity_path=path)
    try:
        status = sup.start()
        assert status.state == AutopilotState.SUPERVISED
        # No file written yet -> degrade to plain liveness.
        assert sup.status().activity is None
    finally:
        sup.stop(timeout=5.0)


def test_dead_child_drops_activity(tmp_path) -> None:
    path = tmp_path / "activity.json"
    sup = AutopilotSupervisor(spawn_argv=_sleep_argv(), activity_path=path)
    started = sup.start()
    assert started.pid is not None
    write_activity(path, _activity(started.pid))
    pid = started.pid
    os.kill(pid, signal.SIGKILL)
    with contextlib.suppress(ChildProcessError):
        os.waitpid(pid, 0)
    # Once dead, the surface shows DEAD and carries no live activity.
    status = sup.status()
    assert status.state == AutopilotState.DEAD
    assert status.activity is None


# --- the renderer -----------------------------------------------------------


def test_format_status_appends_activity_summary() -> None:
    status = AutopilotStatus(
        state=AutopilotState.SUPERVISED, pid=123, activity=_activity(123)
    )
    rendered = _format_autopilot_status(status, now=1000.0).plain
    assert "autopilot: supervised pid=123" in rendered
    assert "idle, next in" in rendered
    assert "last: 2 emitted, 1 dropped" in rendered


def test_format_activity_idle_counts_down_against_now() -> None:
    activity = _activity(1, phase=PHASE_IDLE, next_cycle_at=1130.0)
    # 130s remaining -> 2m10s.
    text = _format_autopilot_activity(activity, now=1000.0)
    assert "idle, next in 2m10s" in text


def test_format_activity_running_shows_cycle_and_carries_last() -> None:
    activity = _activity(1, phase=PHASE_RUNNING, cycle_index=4, next_cycle_at=None)
    text = _format_autopilot_activity(activity, now=1000.0)
    assert "cycle 4 running" in text
    # cycle_index > 1 while running -> the previous cycle's summary still shows.
    assert "last: 2 emitted, 1 dropped" in text


def test_format_activity_first_running_cycle_has_no_last_summary() -> None:
    activity = _activity(
        1,
        phase=PHASE_RUNNING,
        cycle_index=1,
        next_cycle_at=None,
        last_emitted=0,
        last_dropped=0,
    )
    text = _format_autopilot_activity(activity, now=1000.0)
    assert "cycle 1 running" in text
    assert "last:" not in text


def test_format_status_plain_supervised_without_activity() -> None:
    status = AutopilotStatus(state=AutopilotState.SUPERVISED, pid=9)
    rendered = _format_autopilot_status(status, now=1000.0).plain
    assert rendered == "autopilot: supervised pid=9"
