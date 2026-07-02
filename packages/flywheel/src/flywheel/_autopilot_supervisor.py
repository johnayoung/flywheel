"""Subprocess supervisor for the operator-started autopilot daemon.

The console keeps a second supervised child running underneath it, beside the
git-worktree worker: the autopilot intake daemon
(:mod:`flywheel_orchestrator._autopilot_run`, launched as a ``flywheel
autopilot`` subprocess with NO ``--once`` so it is the neverending loop). This
module mirrors :class:`flywheel._worker_supervisor.WorkerSupervisor`'s
spawn / detach / stop pattern (decision D-6) -- it is deliberately the same
shape so the console gains an autopilot status the way it shows worker status.

The one difference from the worker supervisor: autopilot writes no
``task_claims`` lease. Instead, cross-process liveness rides on the daemon's
activity snapshot, which doubles as a **liveness record** -- it carries the
daemon ``pid`` plus an ``expires_at`` freshness deadline the daemon pushes
forward every cycle (see :mod:`flywheel_orchestrator._autopilot_activity`). A
second supervisor reads that record (``read_live_activity``) and, finding a live
daemon it does not own, adopts it -- reporting ``DETACHED`` and spawning nothing,
exactly as the worker supervisor treats a live foreign ``task_claims`` lease. A
*stale* record (``expires_at`` at/before now) reads as not-live by the identical
rule the worker uses for a lapsed lease, so a dead daemon's leftover record does
not block a respawn. A detached daemon keeps running after the console exits
(``start_new_session=True``); the next console adopts it via its record.
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
from flywheel_orchestrator._autopilot_activity import (
    AutopilotActivity,
    read_activity,
    read_live_activity,
)
from flywheel_orchestrator._supervision_policy import (
    RespawnDecision,
    SupervisionPolicy,
)

# Same SIGTERM wait window as the worker supervisor: the autopilot daemon's
# graceful-shutdown path exits promptly on signal (it idles between cycles), so
# a few seconds of headroom is ample. Tests override to stay snappy.
DEFAULT_STOP_TIMEOUT_SECONDS: float = 10.0


class AutopilotState(str, enum.Enum):
    """The states the console's status surface renders for autopilot.

    ``SUPERVISED`` -- this console owns a live autopilot child it spawned.
    ``DETACHED`` -- a live daemon this console does NOT own holds a fresh
    liveness record (its ``expires_at`` is still in the future). Either a
    previous console detached its child or another console launched it; this
    console adopts it -- it spawns nothing and does not signal it (``stop()`` is
    a no-op with an inline notice), mirroring how the worker supervisor treats a
    live foreign ``task_claims`` lease. ``NONE`` -- no supervised child and no
    live record; the status surface advertises ``/autopilot start``. ``DEAD`` --
    the supervised child exited unexpectedly mid-session and was NOT respawned
    (no crash-loop policy, or a disabled budget-0 policy). ``DEAD_AFTER_BUDGET``
    -- the child kept dying and exhausted the shared windowed crash-loop budget,
    so the supervisor stopped auto-respawning and latched this distinct,
    queryable terminal state (the unattended-base-branch safety interlock --
    spec 00070); an operator ``/autopilot start`` re-arms a fresh budget window.
    ``ERROR`` -- the most recent ``start()`` failed before the child could
    launch.

    A record whose freshness deadline is at/before now (a lapsed record) is
    treated as no live daemon -- the identical staleness rule the worker applies
    to a lapsed lease -- so a dead daemon's leftover record never latches
    ``DETACHED`` nor blocks a respawn.
    """

    SUPERVISED = "supervised"
    DETACHED = "detached"
    NONE = "none"
    DEAD = "dead"
    DEAD_AFTER_BUDGET = "dead_after_budget"
    ERROR = "error"


@dataclass(frozen=True, kw_only=True)
class AutopilotStatus:
    """Snapshot of supervisor state for the console's status surface.

    ``activity`` is the live per-cycle snapshot the daemon writes (which cycle,
    last cycle's emitted/dropped counts, time-to-next-cycle). For
    ``state == SUPERVISED`` it is populated only when the snapshot's pid matches
    the owned child (a stale file from a previous daemon is ignored). For
    ``state == DETACHED`` it is the adopted foreign daemon's live record, and
    ``pid`` is that daemon's pid -- so the console can name which daemon it
    adopted rather than duplicating it.
    """

    state: AutopilotState
    pid: int | None = None
    message: str | None = None
    activity: AutopilotActivity | None = None


def build_autopilot_spawn_argv(
    *,
    tasks_dir: Path | None = None,
    model: str | None = None,
    activity_file: Path | None = None,
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
    if activity_file is not None:
        argv.extend(["--activity-file", str(activity_file)])
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
        activity_path: Path | None = None,
        policy: SupervisionPolicy | None = None,
    ) -> None:
        self._policy = policy
        self._log_dir = (
            log_dir if log_dir is not None else Path(".flywheel/logs/autopilot")
        )
        # The activity snapshot the daemon writes and ``status()`` reads. The
        # supervisor owns the path and hands it to the child via ``--activity-
        # file`` so both ends agree without the daemon guessing.
        self._activity_path = (
            activity_path
            if activity_path is not None
            else self._log_dir / "activity.json"
        )
        if spawn_argv is not None:
            self._spawn_argv: list[str] = list(spawn_argv)
        else:
            self._spawn_argv = build_autopilot_spawn_argv(
                tasks_dir=tasks_dir,
                model=model,
                activity_file=self._activity_path,
            )
        self._child: subprocess.Popen[bytes] | None = None
        self._log_handle: IO[bytes] | None = None
        self._log_path: Path | None = None
        self._last_error: str | None = None
        self._dead_pid: int | None = None
        self._dead_exit: int | None = None
        self._dead_reason: str | None = None
        # Latched ``True`` once a policy-governed child exhausts the crash-loop
        # budget: the supervisor stops respawning and reports
        # ``DEAD_AFTER_BUDGET`` until an operator ``start()`` re-arms it.
        self._dead_after_budget: bool = False

    # ----- Public seams -----------------------------------------------------

    def status(self) -> AutopilotStatus:
        """Compute the current :class:`AutopilotStatus`.

        A live owned child reports ``SUPERVISED``; a child that exited on its
        own flips to ``DEAD`` until the operator respawns. With no owned child,
        a live liveness record from a daemon this console does not own reports
        ``DETACHED`` (adopt, spawn nothing); otherwise ``NONE`` (or ``ERROR``
        after a failed spawn).

        Order mirrors the worker supervisor: an owned/dead/errored child is
        resolved from local state first, and only then is the cross-process
        record consulted -- so a just-died owned child reads ``DEAD`` (its own
        lapsing record never masks the death as ``DETACHED``).
        """
        if self._child is not None:
            rc = self._child.poll()
            if rc is None:
                return AutopilotStatus(
                    state=AutopilotState.SUPERVISED,
                    pid=self._child.pid,
                    activity=self._read_owned_activity(self._child.pid),
                )
            # Child died on its own; capture *why* before reaping, then let the
            # crash-loop policy decide whether this tick auto-respawns it or
            # latches the exhausted terminal state.
            dead_pid = self._child.pid
            dead_exit = rc
            dead_reason = read_supervised_death_reason(self._log_path)
            self._reap_child()
            respawned = self._respawn_or_retire(dead_pid, dead_exit, dead_reason)
            if respawned is not None:
                return respawned

        if self._dead_after_budget:
            return AutopilotStatus(
                state=AutopilotState.DEAD_AFTER_BUDGET,
                pid=self._dead_pid,
                message=format_dead_message(self._dead_exit, self._dead_reason),
            )
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
        live = read_live_activity(self._activity_path)
        if live is not None:
            # A fresh record from a daemon this console does not own: adopt it
            # (report DETACHED, carrying the foreign pid + record) rather than
            # spawning a duplicate. A stale record read back as None and falls
            # through to NONE -- the dead-daemon respawn path.
            return AutopilotStatus(
                state=AutopilotState.DETACHED, pid=live.pid, activity=live
            )
        return AutopilotStatus(state=AutopilotState.NONE)

    def owns_supervised_child(self) -> bool:
        """Whether this supervisor has a live child it spawned."""
        return self.status().state == AutopilotState.SUPERVISED

    def start(self) -> AutopilotStatus:
        """Spawn the autopilot daemon if none is owned; idempotent otherwise.

        An already-``SUPERVISED`` supervisor returns unchanged without spawning
        a second daemon (idempotent start). A ``DETACHED`` supervisor -- a live
        daemon it does not own already holds a fresh liveness record -- likewise
        returns unchanged without spawning, adopting that daemon instead of
        duplicating it (the same no-spawn branch the worker takes on a live
        foreign lease). The child is placed in its own session
        (``start_new_session=True``) so a Ctrl+C on the console terminal never
        reaches it and it survives the console's exit -- only an explicit
        :meth:`stop` signals it.
        """
        current = self.status()
        if current.state == AutopilotState.SUPERVISED:
            return current
        if current.state == AutopilotState.DETACHED:
            return current

        self._dead_pid = None
        self._dead_exit = None
        self._dead_reason = None
        self._dead_after_budget = False
        self._last_error = None

        # A DEAD / ERROR status short-circuits status() *before* its liveness
        # read, so a live record (a peer daemon's, or this console's own
        # just-detached child) would otherwise be missed and we would spawn a
        # duplicate against a store that already has a live daemon. Now that the
        # DEAD/ERROR flags are cleared, re-evaluate: a live record surfaces as
        # DETACHED and we adopt it rather than spawn.
        rechecked = self.status()
        if rechecked.state == AutopilotState.DETACHED:
            return rechecked

        # An operator start() re-arms the crash-loop budget: forget the prior
        # window's deaths so a manual restart begins with a full budget again
        # (after DEAD_AFTER_BUDGET, this is what lets the operator retry).
        if self._policy is not None:
            self._policy.reset()

        return self._spawn_child()

    def stop(self, *, timeout: float = DEFAULT_STOP_TIMEOUT_SECONDS) -> bool:
        """Stop the supervised daemon and everything it spawned.

        Signals the child's whole process *group*, not just its pid: the daemon
        is its own session leader (``start_new_session=True``), so the agent
        subprocesses it spawns (and their MCP children) share its group and must
        come down with it -- signaling only the daemon pid leaves those agents
        orphaned and still editing the repo. A graceful SIGTERM goes first; if
        the group is still alive after ``timeout`` -- a daemon blocked mid-cycle
        cannot honor the stop flag until its in-flight agent call returns -- it
        is escalated to SIGKILL so the console never exits leaving an orphan.

        Returns ``True`` when a running daemon was stopped (gracefully or
        force-killed); ``False`` when there was no supervised child. Idempotent.
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

        if not self._signal_group(signal.SIGTERM):
            # The child vanished between the poll and the signal; nothing to do.
            self._dead_pid = self._child.pid
            self._dead_exit = self._child.poll() or 0
            self._dead_reason = read_supervised_death_reason(self._log_path)
            self._reap_child()
            return True

        try:
            exit_code = self._child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            # Blocked mid-cycle: force the whole group down rather than orphan
            # it, then reap so no zombie is left behind.
            self._signal_group(signal.SIGKILL)
            try:
                exit_code = self._child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                exit_code = None
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
        self._dead_after_budget = False
        self._close_log()

    def close(self) -> None:
        """Drop resources without affecting the child (detach-by-default)."""
        self.detach()

    # ----- Internal helpers ------------------------------------------------

    def _respawn_or_retire(
        self,
        dead_pid: int,
        dead_exit: int | None,
        dead_reason: str | None,
    ) -> AutopilotStatus | None:
        """Decide what to do about a daemon child that died on its own.

        With no policy, or a disabled (budget-0) policy, this reproduces the
        pre-respawn behavior exactly: record the death and fall through to a
        plain ``DEAD`` status, never respawning (the operator's unattended-
        base-branch safety override). With an active policy it charges the
        death against the shared windowed crash-loop budget -- a death inside
        budget launches a real new child (returning ``SUPERVISED`` with the new
        pid, no operator ``start()``); a death past budget stops respawning and
        latches the distinct ``DEAD_AFTER_BUDGET`` terminal state.

        Returns the ``SUPERVISED`` status when a respawn was launched;
        otherwise ``None`` so ``status()`` falls through to its DEAD /
        DEAD_AFTER_BUDGET tail.
        """
        if self._policy is None or self._policy.budget.disabled:
            self._dead_pid = dead_pid
            self._dead_exit = dead_exit
            self._dead_reason = dead_reason
            return None
        if self._policy.record_death() is RespawnDecision.RESPAWN:
            # Inside budget: launch a real new child. Clear the prior death
            # markers first so a successful respawn presents as clean
            # SUPERVISED rather than trailing a stale DEAD reason.
            self._dead_pid = None
            self._dead_exit = None
            self._dead_reason = None
            self._dead_after_budget = False
            self._last_error = None
            spawned = self._spawn_child()
            if spawned.state == AutopilotState.SUPERVISED:
                return spawned
            # The respawn's own spawn failed -> ERROR is already latched in
            # _last_error; fall through so status() surfaces it.
            return None
        # Budget exhausted: stop respawning and latch the loud terminal state.
        self._dead_pid = dead_pid
        self._dead_exit = dead_exit
        self._dead_reason = dead_reason
        self._dead_after_budget = True
        return None

    def _spawn_child(self) -> AutopilotStatus:
        """Spawn one autopilot daemon child, redirecting output to a fresh log.

        The single spawn seam shared by the operator ``start()`` and the
        automatic in-budget respawn, so both take the identical orphan-safe
        path: the child is placed in its own session
        (``start_new_session=True``) so a Ctrl+C on the console terminal never
        reaches it and it survives console exit (only ``stop()`` signals it).
        Returns ``SUPERVISED`` with the new pid on success, or ``ERROR`` (with
        the cause latched in ``_last_error``) when the log cannot be opened or
        ``Popen`` raises.
        """
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

    def _signal_group(self, sig: int) -> bool:
        """Signal the child's process group; ``False`` if it no longer exists.

        The daemon is a session leader, so its pgid equals its pid and the
        signal reaches every descendant it spawned. Returns ``False`` when the
        process (or its group) is already gone, so the caller can treat a
        vanished child as already-stopped.
        """
        child = self._child
        if child is None:
            return False
        try:
            pgid = os.getpgid(child.pid)
        except ProcessLookupError:
            return False
        try:
            os.killpg(pgid, sig)
        except ProcessLookupError:
            return False
        return True

    def _read_owned_activity(self, child_pid: int) -> AutopilotActivity | None:
        """Read the activity snapshot iff it belongs to the owned child.

        Guards against a stale ``activity.json`` left by a previous daemon: a
        snapshot whose ``pid`` does not match the child this supervisor spawned
        is treated as absent, so the status surface shows plain liveness until
        the live daemon records its first cycle.
        """
        activity = read_activity(self._activity_path)
        if activity is None or activity.pid != child_pid:
            return None
        return activity

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
