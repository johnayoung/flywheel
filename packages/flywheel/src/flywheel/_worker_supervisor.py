"""Subprocess supervisor for the engine-on-launch worker.

Bare ``flywheel`` (or ``fw``) opens the operator console; this module
is how the console keeps a git-worktree worker running underneath it
(the worker is :mod:`flywheel_worktree.worker`, launched as a
``flywheel worker`` subprocess). The supervisor owns three
responsibilities (and intentionally no more):

* **Liveness detection** -- a SQL read against the existing
  ``task_claims`` table. A row whose ``lease_expires_at`` is still in
  the future counts as a live worker; a stale lease is treated as no
  live worker (the existing lease-lapse semantics).
* **Spawn / supervise** -- when no live worker is detected, spawn a
  child process running the worker daemon and remember it as ours.
  The child's stdout/stderr are redirected to a supervisor log under
  ``.flywheel/logs/worker/`` next to the worker's own per-run logs.
* **Quit handoff** -- ``detach()`` drops our reference to the child
  (the worker keeps running, the console can exit); ``stop()`` sends
  SIGTERM and waits so the worker's existing graceful-shutdown path
  finalizes the in-flight lifecycle to ``interrupted``.

The supervisor is plain stdlib parent-child subprocess management:
no pidfiles, no daemon manager, no restart policy beyond an explicit
``start()`` call from the operator (``/worker start`` or
``--no-worker`` followed by manual start). It manages only workers
it spawned: an externally-started worker shows as ``DETACHED`` and is
not signaled. The child is placed in its own process group so a
Ctrl+C on the console terminal never reaches it silently --
``stop()`` is the only path that kills the worker.
"""

from __future__ import annotations

import enum
import os
import signal
import sqlite3
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import IO, Sequence


# How long ``stop()`` waits for the child to exit after SIGTERM before
# giving up and reporting failure. The worker's graceful shutdown
# finalizes the in-flight lifecycle inside ``run_task_object``; a few
# seconds of headroom covers the post-finalize cleanup. Tests override
# to a smaller value so the suite stays snappy.
DEFAULT_STOP_TIMEOUT_SECONDS: float = 10.0


class WorkerState(str, enum.Enum):
    """The five states the console's status bar renders.

    ``SUPERVISED`` -- this console owns a child it spawned and the
    child is alive (``poll()`` returns ``None``). Quit prompts the
    operator before exiting.

    ``DETACHED`` -- a live lease exists in ``task_claims`` but this
    console does not own the process. Either a previous ``fw`` session
    detached its child, or a worker was launched manually. The
    supervisor does not signal detached workers; ``/worker stop`` is
    a no-op with an inline notice.

    ``NONE`` -- no live lease, no supervised child. The status bar
    advertises ``/worker start``.

    ``DEAD`` -- the supervised child terminated unexpectedly mid-
    session. The status bar surfaces this so the operator can
    ``/worker start`` to respawn (the new worker's startup recovery
    sweep handles any stranded lifecycles, per existing worker
    semantics).

    ``ERROR`` -- the most recent ``start()`` call failed before the
    child could be launched (bad env, missing executable). The
    message field carries the underlying ``OSError`` detail so the
    operator can fix and retry via ``/worker start``.
    """

    SUPERVISED = "supervised"
    DETACHED = "detached"
    NONE = "none"
    DEAD = "dead"
    ERROR = "error"


@dataclass(frozen=True, kw_only=True)
class WorkerStatus:
    """Snapshot of supervisor state for the status bar.

    Immutable so the dashboard can stash one per tick without worrying
    about the supervisor mutating it from underneath. ``pid`` is set
    only for ``SUPERVISED`` / ``DEAD`` (the only states where this
    console knows the PID); ``message`` carries human-readable detail
    for ``DEAD`` (exit code) and ``ERROR`` (spawn failure) so the
    status bar can show it inline.
    """

    state: WorkerState
    pid: int | None = None
    message: str | None = None


def has_live_lease(db_path: Path, *, now: datetime | None = None) -> bool:
    """Whether any worker currently holds a live lease in ``task_claims``.

    Reads the orchestrator's ``task_claims`` table directly through a
    read-only SQLite connection -- the spec's "no new store surface,
    columns, or protocol methods" constraint is honoured by going
    through SQL rather than a new ``ClaimStore`` method. A lease whose
    ``lease_expires_at`` is in the past (lapsed lease) does NOT count
    as a live worker, matching the existing acquire-steal semantics.

    Returns ``False`` when the database file does not exist (a fresh
    project that has never been run) or the ``task_claims`` table is
    absent (the orchestrator schema has not been bootstrapped yet).
    Any other ``OperationalError`` re-raises so the caller can decide
    whether to surface it.
    """

    if not db_path.exists():
        return False
    moment = now if now is not None else datetime.now(timezone.utc)
    # Open read-only so a misconfigured supervisor cannot accidentally
    # write to the production store; URI form lets us pass ``mode=ro``.
    uri = f"file:{db_path}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    try:
        try:
            row = conn.execute(
                "SELECT 1 FROM task_claims WHERE lease_expires_at > ? LIMIT 1",
                (moment.isoformat(),),
            ).fetchone()
        except sqlite3.OperationalError as exc:
            # ``no such table: task_claims`` is the orchestrator schema
            # not having been bootstrapped on this DB yet; treat as
            # "no live worker" so the supervisor will spawn one (whose
            # startup will create the table). Re-raise other operational
            # errors so a corrupted DB does not silently look idle.
            if "no such table" in str(exc).lower():
                return False
            raise
        return row is not None
    finally:
        conn.close()


def build_default_spawn_argv(db_path: Path, *, tasks_dir: Path | None) -> list[str]:
    """Compose the ``python -m flywheel_worktree.worker ...`` argv.

    Used by the production ``WorkerSupervisor`` to launch the daemon
    in-band. Tests pass a substitute argv (a trivial sleep child) so
    the supervisor's ownership / quit-path behaviour is exercised
    without spawning the real worker. The args are the minimal pair
    the worker needs to land on the same store the console is
    watching; everything else (heartbeat, lease seconds, model)
    defaults to the worker's own argparse defaults.
    """

    argv: list[str] = [
        sys.executable,
        "-m",
        "flywheel_worktree.worker",
        "--db",
        str(db_path),
    ]
    if tasks_dir is not None:
        argv.extend(["--tasks-dir", str(tasks_dir)])
    return argv


class WorkerSupervisor:
    """Spawn, detach from, and stop one git-worktree worker child.

    Construction does not spawn anything; call :meth:`start` to spawn.
    ``status()`` is cheap (a ``Popen.poll`` plus a ``task_claims`` SQL
    read) and is what the dashboard calls each tick.

    Args:
        db_path: Path to the shared ``flywheel.sqlite`` store. Used
            both for liveness detection (read of ``task_claims``) and
            forwarded to the spawned worker as ``--db``.
        log_dir: Where the supervisor's per-spawn log file lands.
            Defaults to ``.flywheel/logs/worker/`` next to the
            worker's own per-run logs so a single ``ls`` shows the
            full forensics trail.
        spawn_argv: Optional override of the spawn command. Production
            callers leave this ``None`` and the default
            ``python -m flywheel_worktree.worker --db <path>`` argv is
            used; tests pass a trivial child (a ``time.sleep`` Python
            one-liner) so the supervisor's behaviour is exercised
            without launching the real worker.
        tasks_dir: Optional tasks directory; forwarded to the
            spawned worker as ``--tasks-dir`` so it watches the same
            tree the console resolves. Ignored when ``spawn_argv`` is
            provided (tests pin the argv directly).
    """

    def __init__(
        self,
        *,
        db_path: Path,
        log_dir: Path | None = None,
        spawn_argv: Sequence[str] | None = None,
        tasks_dir: Path | None = None,
    ) -> None:
        self._db_path = db_path
        self._log_dir = (
            log_dir if log_dir is not None else Path(".flywheel/logs/worker")
        )
        if spawn_argv is not None:
            self._spawn_argv: list[str] = list(spawn_argv)
        else:
            self._spawn_argv = build_default_spawn_argv(
                db_path, tasks_dir=tasks_dir
            )
        self._child: subprocess.Popen[bytes] | None = None
        self._log_handle: IO[bytes] | None = None
        self._last_error: str | None = None
        # Marks whether the most recent supervised child exited
        # unexpectedly mid-session. Cleared on the next successful
        # ``start()``; flips on when ``status()`` first observes a
        # non-running child it owned. The dashboard reads this through
        # ``status().state == DEAD`` so the death is visible until the
        # operator respawns.
        self._dead_pid: int | None = None
        self._dead_exit: int | None = None

    # ----- Public seams -----------------------------------------------------

    def status(self) -> WorkerStatus:
        """Compute the current :class:`WorkerStatus` for the status bar.

        Order is significant: a supervisor that already owns a live
        child reports ``SUPERVISED`` without touching the database;
        only when no child is owned does it consult ``task_claims``
        for the detached/none distinction.
        """

        if self._child is not None:
            rc = self._child.poll()
            if rc is None:
                return WorkerStatus(
                    state=WorkerState.SUPERVISED, pid=self._child.pid
                )
            # Child died on its own; reap the resource handles and
            # remember the death so subsequent ticks keep reporting
            # DEAD until the operator respawns.
            self._dead_pid = self._child.pid
            self._dead_exit = rc
            self._reap_child()

        if self._dead_pid is not None:
            return WorkerStatus(
                state=WorkerState.DEAD,
                pid=self._dead_pid,
                message=f"exit={self._dead_exit}",
            )
        if self._last_error is not None:
            return WorkerStatus(
                state=WorkerState.ERROR, message=self._last_error
            )
        if has_live_lease(self._db_path):
            return WorkerStatus(state=WorkerState.DETACHED)
        return WorkerStatus(state=WorkerState.NONE)

    def owns_supervised_child(self) -> bool:
        """Whether this supervisor has a live child it spawned.

        The quit prompt consults this: a console that does not own a
        live child exits silently (FR-3 -- detached/external workers
        never prompt). The check side-effects through ``status()``
        first so a child that just died is recognized as ``DEAD``
        rather than mis-reported as ``SUPERVISED``.
        """

        return self.status().state == WorkerState.SUPERVISED

    def start(self) -> WorkerStatus:
        """Spawn the worker if no live worker exists; idempotent otherwise.

        Returns the resulting :class:`WorkerStatus` so the caller can
        surface the outcome inline. The four observable branches:

        * Already ``SUPERVISED`` -- returns unchanged.
        * ``DETACHED`` (another worker holds a live lease) -- no
          spawn; returns ``DETACHED`` so the inline notice can name
          it.
        * Spawn succeeds -- ``SUPERVISED`` with the new ``pid``.
        * Spawn fails (``OSError`` from ``Popen``) -- ``ERROR`` with
          the exception's message in ``WorkerStatus.message`` so the
          status bar can show the operator what went wrong.

        The child is placed in its own session via
        ``start_new_session=True`` so a Ctrl+C on the console
        terminal does NOT propagate to it -- the spec's "never
        silently kill the child" requirement is met because only an
        explicit ``stop()`` (operator chose ``s`` at the quit prompt
        or typed ``/worker stop``) signals the worker.
        """

        current = self.status()
        if current.state == WorkerState.SUPERVISED:
            return current
        if current.state == WorkerState.DETACHED:
            return current

        # Clear any leftover DEAD / ERROR state from prior attempts so
        # the supervisor's snapshot matches reality post-spawn.
        self._dead_pid = None
        self._dead_exit = None
        self._last_error = None

        try:
            log_handle = self._open_log()
        except OSError as exc:
            self._last_error = f"cannot open worker log: {exc}"
            return WorkerStatus(
                state=WorkerState.ERROR, message=self._last_error
            )

        try:
            child = subprocess.Popen(
                self._spawn_argv,
                stdout=log_handle,
                stderr=subprocess.STDOUT,
                env=os.environ.copy(),
                # New session so SIGINT to the console terminal does
                # not reach the worker; ``stop()`` is the only path
                # that signals it.
                start_new_session=True,
                close_fds=True,
            )
        except (OSError, ValueError) as exc:
            log_handle.close()
            self._log_handle = None
            self._last_error = f"spawn failed: {exc}"
            return WorkerStatus(
                state=WorkerState.ERROR, message=self._last_error
            )

        self._child = child
        return WorkerStatus(state=WorkerState.SUPERVISED, pid=child.pid)

    def stop(
        self, *, timeout: float = DEFAULT_STOP_TIMEOUT_SECONDS
    ) -> bool:
        """Send SIGTERM to the supervised child and wait for it to exit.

        Returns ``True`` when the child exited within ``timeout``,
        ``False`` when there is no supervised child to stop or the
        wait timed out. The worker's existing graceful-shutdown path
        is what finalizes the in-flight lifecycle to ``interrupted``;
        the supervisor only signals + reaps.

        Idempotent: a second call after the child has exited returns
        ``False`` without raising.
        """

        if self._child is None:
            return False
        rc = self._child.poll()
        if rc is not None:
            self._dead_pid = self._child.pid
            self._dead_exit = rc
            self._reap_child()
            return False
        try:
            self._child.send_signal(signal.SIGTERM)
        except ProcessLookupError:
            # Child died between the poll and the signal; reap and
            # report not-stopped-by-us so the caller can decide.
            self._dead_pid = self._child.pid
            self._dead_exit = self._child.poll() or 0
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

        The child keeps running -- nothing is signaled. The log file
        handle we held is closed (the child has its own copy of the
        fd, so the file stays writable on the child's side). Calling
        ``status()`` afterwards reports ``DETACHED`` once the
        ``task_claims`` table sees the worker's first heartbeat,
        ``NONE`` until then.
        """

        self._child = None
        self._dead_pid = None
        self._dead_exit = None
        self._close_log()

    def close(self) -> None:
        """Drop resources without affecting the child.

        Same shape as ``detach`` but intended for the TUI's ``finally``
        block: idempotent, safe to call when no child was ever
        spawned. The supervised child (if any) keeps running so the
        SIGINT-on-the-console case takes the same detach-by-default
        path the spec requires.
        """

        self.detach()

    # ----- Internal helpers ------------------------------------------------

    def _open_log(self) -> IO[bytes]:
        """Open the per-spawn supervisor log file in append mode.

        Lands under ``log_dir`` (default ``.flywheel/logs/worker/``)
        so a manual ``ls`` reveals supervisor lifecycle alongside the
        worker's own per-run forensics files. Append mode + a
        timestamped filename means re-spawns never clobber prior
        logs.
        """

        self._log_dir.mkdir(parents=True, exist_ok=True)
        ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
        path = self._log_dir / f"supervisor-{ts}.log"
        handle = open(path, "ab", buffering=0)
        self._log_handle = handle
        return handle

    def _close_log(self) -> None:
        if self._log_handle is not None:
            try:
                self._log_handle.close()
            except OSError:
                pass
            self._log_handle = None

    def _reap_child(self) -> None:
        """Drop the ``Popen`` handle and close the log file.

        Called after we've observed the child has exited (either
        because we signalled it or it died on its own). Leaves
        ``_dead_pid`` / ``_dead_exit`` intact so ``status()`` can
        keep reporting ``DEAD`` until the operator respawns.
        """

        self._child = None
        self._close_log()


__all__ = [
    "DEFAULT_STOP_TIMEOUT_SECONDS",
    "WorkerState",
    "WorkerStatus",
    "WorkerSupervisor",
    "build_default_spawn_argv",
    "has_live_lease",
]
