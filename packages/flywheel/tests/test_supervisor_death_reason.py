"""A dead supervised child surfaces *why* it died, not just its exit code.

When the console auto-spawns the worker (or autopilot) and the child exits on
startup — e.g. a ``flywheel worker: policy error: ...`` from a misconfigured
``[submit] base`` — the operator should see the reason inline in the status
bar, instead of a bare ``dead (exit=2)`` that forces a log dig.
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

from flywheel._autopilot_supervisor import AutopilotState, AutopilotSupervisor
from flywheel._worker_supervisor import (
    WorkerState,
    WorkerSupervisor,
    format_dead_message,
    read_supervised_death_reason,
)


_POLICY_ERROR = (
    "flywheel worker: policy error: configured landing base 'main' is the "
    "operator's currently-checked-out branch"
)


def _dying_child_argv(message: str, code: int = 2) -> list[str]:
    """A child that writes ``message`` to stderr and exits ``code``."""
    return [
        sys.executable,
        "-c",
        f"import sys; sys.stderr.write({message!r} + '\\n'); sys.exit({code})",
    ]


def _wait_until_dead(pid: int, *, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            __import__("os").kill(pid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.02)


# --- the helpers ------------------------------------------------------------


def test_read_reason_returns_last_meaningful_line(tmp_path: Path) -> None:
    log = tmp_path / "s.log"
    log.write_text(
        "<frozen runpy>:128: RuntimeWarning: 'x' found in sys.modules ...\n"
        f"{_POLICY_ERROR}\n"
    )
    reason = read_supervised_death_reason(log)
    assert reason == _POLICY_ERROR  # the runpy warning is skipped as noise


def test_read_reason_caps_length(tmp_path: Path) -> None:
    log = tmp_path / "s.log"
    log.write_text("x" * 5000 + "\n")
    reason = read_supervised_death_reason(log)
    assert reason is not None and len(reason) <= 240 and reason.endswith("…")


def test_read_reason_none_when_missing_or_empty(tmp_path: Path) -> None:
    assert read_supervised_death_reason(None) is None
    assert read_supervised_death_reason(tmp_path / "nope.log") is None
    empty = tmp_path / "empty.log"
    empty.write_text("")
    assert read_supervised_death_reason(empty) is None


def test_format_dead_message_includes_reason_when_present() -> None:
    assert format_dead_message(2, None) == "exit=2"
    assert format_dead_message(2, "boom") == "exit=2: boom"


# --- worker supervisor end-to-end -------------------------------------------


def test_worker_dead_status_surfaces_the_reason(tmp_path: Path) -> None:
    sup = WorkerSupervisor(
        db_path=tmp_path / "db.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(_POLICY_ERROR),
    )
    status = sup.start()
    assert status.pid is not None
    _wait_until_dead(status.pid)
    dead = sup.status()
    assert dead.state == WorkerState.DEAD
    assert dead.message is not None
    assert "exit=2" in dead.message
    assert "policy error" in dead.message  # the actual cause, surfaced inline


def test_worker_reason_clears_on_respawn(tmp_path: Path) -> None:
    sup = WorkerSupervisor(
        db_path=tmp_path / "db.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(_POLICY_ERROR),
    )
    pid = sup.start().pid
    assert pid is not None
    _wait_until_dead(pid)
    assert "policy error" in (sup.status().message or "")
    # A successful respawn (sleeping child) clears the stale reason.
    sup._spawn_argv = [sys.executable, "-c", "import time; time.sleep(30)"]
    try:
        respawned = sup.start()
        assert respawned.state == WorkerState.SUPERVISED
    finally:
        sup.stop(timeout=5.0)


# --- autopilot supervisor (same behavior) -----------------------------------


def test_autopilot_dead_status_surfaces_the_reason(tmp_path: Path) -> None:
    msg = "flywheel autopilot: policy error: [autopilot] target_depth must be positive"
    sup = AutopilotSupervisor(
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(msg, code=2),
    )
    status = sup.start()
    assert status.pid is not None
    _wait_until_dead(status.pid)
    dead = sup.status()
    assert dead.state == AutopilotState.DEAD
    assert dead.message is not None
    assert "exit=2" in dead.message
    assert "policy error" in dead.message
