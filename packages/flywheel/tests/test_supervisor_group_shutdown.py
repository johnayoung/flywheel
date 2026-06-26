"""Regression tests: stop() takes down the whole process group, never orphans.

The autopilot/worker daemons are session leaders that spawn agent subprocesses
(and those spawn MCP children). A SIGTERM to the daemon pid alone leaves those
descendants orphaned, and a daemon blocked mid-cycle cannot honor SIGTERM
before the stop timeout -- which is exactly how a console /exit left a live
daemon + agent running. stop() must therefore signal the group and escalate to
SIGKILL, so it always returns with nothing left alive.
"""

from __future__ import annotations

import contextlib
import os
import signal
import sys
import time
from pathlib import Path

from flywheel._autopilot_supervisor import AutopilotState, AutopilotSupervisor
from flywheel._worker_supervisor import WorkerState, WorkerSupervisor


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _ignore_term_with_child_argv(pidfile: Path) -> list[str]:
    """A child that ignores SIGTERM and spawns a grandchild that does too.

    The grandchild's pid is written to ``pidfile`` so the test can assert the
    group SIGKILL reached it. Both ignore SIGTERM, so only the escalation to
    SIGKILL (which cannot be caught) can end them -- a plain group-SIGTERM would
    not be enough, making this a true escalation test.
    """
    grandchild = (
        "import signal,time;"
        "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
        "time.sleep(300)"
    )
    script = (
        "import signal, subprocess, sys, time\n"
        "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
        f"g = subprocess.Popen([sys.executable, '-c', {grandchild!r}])\n"
        f"open({str(pidfile)!r}, 'w').write(str(g.pid))\n"
        "time.sleep(300)\n"
    )
    return [sys.executable, "-c", script]


def _read_pid(pidfile: Path, *, timeout: float = 5.0) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if pidfile.exists():
            text = pidfile.read_text().strip()
            if text:
                return int(text)
        time.sleep(0.02)
    raise AssertionError("grandchild pid file never appeared")


def _reap_group(pid: int) -> None:
    with contextlib.suppress(ProcessLookupError, OSError):
        os.killpg(os.getpgid(pid), signal.SIGKILL)


def test_autopilot_stop_force_kills_sigterm_ignoring_group(tmp_path) -> None:
    pidfile = tmp_path / "grandchild.pid"
    sup = AutopilotSupervisor(
        log_dir=tmp_path, spawn_argv=_ignore_term_with_child_argv(pidfile)
    )
    started = sup.start()
    assert started.pid is not None
    child_pid = started.pid
    grandchild_pid = _read_pid(pidfile)
    try:
        assert _alive(child_pid) and _alive(grandchild_pid)
        # SIGTERM is ignored, so this must escalate to SIGKILL within the window.
        stopped = sup.stop(timeout=0.5)
        assert stopped is True
        assert not _alive(child_pid)
        assert not _alive(grandchild_pid)  # the group-kill reached the grandchild
        assert sup.status().state == AutopilotState.DEAD
    finally:
        _reap_group(child_pid)
        with contextlib.suppress(ProcessLookupError, OSError):
            os.kill(grandchild_pid, signal.SIGKILL)


def test_worker_stop_force_kills_sigterm_ignoring_group(tmp_path) -> None:
    pidfile = tmp_path / "grandchild.pid"
    sup = WorkerSupervisor(
        db_path=tmp_path / "flywheel.sqlite",
        log_dir=tmp_path,
        spawn_argv=_ignore_term_with_child_argv(pidfile),
    )
    started = sup.start()
    assert started.pid is not None
    child_pid = started.pid
    grandchild_pid = _read_pid(pidfile)
    try:
        assert _alive(child_pid) and _alive(grandchild_pid)
        stopped = sup.stop(timeout=0.5)
        assert stopped is True
        assert not _alive(child_pid)
        assert not _alive(grandchild_pid)
        assert sup.status().state == WorkerState.DEAD
    finally:
        _reap_group(child_pid)
        with contextlib.suppress(ProcessLookupError, OSError):
            os.kill(grandchild_pid, signal.SIGKILL)
