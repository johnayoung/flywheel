"""Subprocess supervisor for the operator-started autopilot daemon.

The console keeps a second supervised child running underneath it, beside the
git-worktree worker: the autopilot intake daemon
(:mod:`flywheel_orchestrator._autopilot_run`, launched as a ``flywheel
autopilot`` subprocess with NO ``--once`` so it is the neverending loop). This
module mirrors :class:`flywheel._worker_supervisor.WorkerSupervisor`'s
spawn / detach / stop pattern (decision D-6) -- it is deliberately the same
shape so the console gains an autopilot status the way it shows worker status.

The one difference from the worker supervisor: autopilot writes no
``task_claims`` lease, so liveness is determined purely from the child this
supervisor spawned (``SUPERVISED`` / ``DEAD`` / ``NONE`` / ``ERROR``), with no
cross-process ``DETACHED`` detection. A detached autopilot daemon keeps running
after the console exits (``start_new_session=True``); the next console simply
does not adopt it.
"""

from __future__ import annotations

import enum
import os
import signal
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import IO, Sequence

from flywheel._worker_supervisor import (
    format_dead_message,
    read_supervised_death_reason,
)

# Same SIGTERM wait window as the worker supervisor: the autopilot daemon's
# graceful-shutdown path exits promptly on signal (it idles between cycles), so
# a few seconds of headroom is ample. Tests override to stay snappy.
DEFAULT_STOP_TIMEOUT_SECONDS: float = 10.0


class AutopilotState(str, enum.Enum):
    """The states the console's status surface renders for autopilot.

    ``SUPERVISED`` -- this console owns a live autopilot child it spawned.
    ``NONE`` -- no supervised child; the status surface advertises
    ``/autopilot start``. ``DEAD`` -- the supervised child exited unexpectedly
    mid-session. ``ERROR`` -- the most recent ``start()`` failed before the
    child could launch.

    There is intentionally no ``DETACHED`` state: autopilot writes no lease, so
    a daemon another console launched (or that this console detached) is not
    detectable here -- it simply keeps running.
    """

    SUPERVISED = "supervised"
    NONE = "none"
    DEAD = "dead"
    ERROR = "error"


@dataclass(frozen=True, kw_only=True)
class AutopilotStatus:
    """Snapshot of supervisor state for the console's status surface."""

    state: AutopilotState
    pid: int | None = None
    message: str | None = None


def build_autopilot_spawn_argv(
    *, tasks_dir: Path | None = None, model: str | None = None
) -> list[str]:
    """Compose the ``python -m flywheel_orchestrator._autopilot_run ...`` argv.

    The spawned process is the neverending daemon: the argv carries NO
    ``--once`` flag (the defends-against for criterion #10 -- the console must
    launch the continuous daemon, not a single pass). ``tasks_dir`` / ``model``
    are forwarded as ``--tasks-dir`` / ``--model`` when set, mirroring the
    worker supervisor's default argv.
    """
    argv: list[str] = [
        sys.executable,
        "-m",
        "flywheel_orchestrator._autopilot_run",
    ]
    if tasks_dir is not None:
        argv.extend(["--tasks-dir", str(tasks_dir)])
    if model is not None:
        argv.extend(["--model", model])
    return argv


class AutopilotSupervisor:
    """Spawn, detach from, and stop one autopilot daemon child.

    Construction does not spawn anything; call :meth:`start`. ``status()`` is
    cheap (a single ``Popen.poll``). The supervisor manages only the child it
    spawned and is independent of the worker supervisor -- starting or stopping
    autopilot never touches the worker, and vice versa.
    """

    def __init__(
        self,
        *,
        log_dir: Path | None = None,
        spawn_argv: Sequence[str] | None = None,
        tasks_dir: Path | None = None,
        model: str | None = None,
    ) -> None:
        self._log_dir = (
            log_dir if log_dir is not None else Path(".flywheel/logs/autopilot")
        )
        if spawn_argv is not None:
            self._spawn_argv: list[str] = list(spawn_argv)
        else:
            self._spawn_argv = build_autopilot_spawn_argv(
                tasks_dir=tasks_dir, model=model
            )
        self._child: subprocess.Popen[bytes] | None = None
        self._log_handle: IO[bytes] | None = None
        self._log_path: Path | None = None
        self._last_error: str | None = None
        self._dead_pid: int | None = None
        self._dead_exit: int | None = None
        self._dead_reason: str | None = None

    # ----- Public seams -----------------------------------------------------

    def status(self) -> AutopilotStatus:
        """Compute the current :class:`AutopilotStatus`.

        A live owned child reports ``SUPERVISED``; a child that exited on its
        own flips to ``DEAD`` until the operator respawns; otherwise ``NONE``
        (or ``ERROR`` after a failed spawn).
        """
        if self._child is not None:
            rc = self._child.poll()
            if rc is None:
                return AutopilotStatus(
                    state=AutopilotState.SUPERVISED, pid=self._child.pid
                )
            self._dead_pid = self._child.pid
            self._dead_exit = rc
            self._dead_reason = read_supervised_death_reason(self._log_path)
            self._reap_child()

        if self._dead_pid is not None:
            return AutopilotStatus(
                state=AutopilotState.DEAD,
                pid=self._dead_pid,
                message=format_dead_message(self._dead_exit, self._dead_reason),
            )
        if self._last_error is not None:
            return AutopilotStatus(
                state=AutopilotState.ERROR, message=self._last_error
            )
        return AutopilotStatus(state=AutopilotState.NONE)

    def owns_supervised_child(self) -> bool:
        """Whether this supervisor has a live child it spawned."""
        return self.status().state == AutopilotState.SUPERVISED

    def start(self) -> AutopilotStatus:
        """Spawn the autopilot daemon if none is owned; idempotent otherwise.

        An already-``SUPERVISED`` supervisor returns unchanged without spawning
        a second daemon (idempotent start). The child is placed in its own
        session (``start_new_session=True``) so a Ctrl+C on the console
        terminal never reaches it and it survives the console's exit -- only an
        explicit :meth:`stop` signals it.
        """
        current = self.status()
        if current.state == AutopilotState.SUPERVISED:
            return current

        self._dead_pid = None
        self._dead_exit = None
        self._dead_reason = None
        self._last_error = None

        try:
            log_handle = self._open_log()
        except OSError as exc:
            self._last_error = f"cannot open autopilot log: {exc}"
            return AutopilotStatus(
                state=AutopilotState.ERROR, message=self._last_error
            )

        try:
            child = subprocess.Popen(
                self._spawn_argv,
                stdout=log_handle,
                stderr=subprocess.STDOUT,
                env=os.environ.copy(),
                start_new_session=True,
                close_fds=True,
            )
        except (OSError, ValueError) as exc:
            log_handle.close()
            self._log_handle = None
            self._last_error = f"spawn failed: {exc}"
            return AutopilotStatus(
                state=AutopilotState.ERROR, message=self._last_error
            )

        self._child = child
        return AutopilotStatus(state=AutopilotState.SUPERVISED, pid=child.pid)

    def stop(self, *, timeout: float = DEFAULT_STOP_TIMEOUT_SECONDS) -> bool:
        """Send SIGTERM to the supervised child and wait for it to exit.

        Returns ``True`` when the child exited within ``timeout``; ``False``
        when there is no supervised child or the wait timed out. Idempotent.
        """
        if self._child is None:
            return False
        rc = self._child.poll()
        if rc is not None:
            self._dead_pid = self._child.pid
            self._dead_exit = rc
            self._dead_reason = read_supervised_death_reason(self._log_path)
            self._reap_child()
            return False
        try:
            self._child.send_signal(signal.SIGTERM)
        except ProcessLookupError:
            self._dead_pid = self._child.pid
            self._dead_exit = self._child.poll() or 0
            self._dead_reason = read_supervised_death_reason(self._log_path)
            self._reap_child()
            return False
        try:
            exit_code = self._child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return False
        self._dead_pid = self._child.pid
        self._dead_exit = exit_code
        self._reap_child()
        return True

    def detach(self) -> None:
        """Forget the supervised child so the console can exit cleanly.

        The child keeps running -- nothing is signaled (the detached daemon
        survives console exit). The log handle we held is closed; the child has
        its own copy of the fd.
        """
        self._child = None
        self._dead_pid = None
        self._dead_exit = None
        self._dead_reason = None
        self._close_log()

    def close(self) -> None:
        """Drop resources without affecting the child (detach-by-default)."""
        self.detach()

    # ----- Internal helpers ------------------------------------------------

    def _open_log(self) -> IO[bytes]:
        self._log_dir.mkdir(parents=True, exist_ok=True)
        ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
        path = self._log_dir / f"autopilot-supervisor-{ts}.log"
        handle = open(path, "ab", buffering=0)
        self._log_handle = handle
        self._log_path = path
        return handle

    def _close_log(self) -> None:
        if self._log_handle is not None:
            try:
                self._log_handle.close()
            except OSError:
                pass
            self._log_handle = None

    def _reap_child(self) -> None:
        self._child = None
        self._close_log()


__all__ = [
    "DEFAULT_STOP_TIMEOUT_SECONDS",
    "AutopilotState",
    "AutopilotStatus",
    "AutopilotSupervisor",
    "build_autopilot_spawn_argv",
]
