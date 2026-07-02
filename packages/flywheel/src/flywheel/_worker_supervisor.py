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
from urllib.parse import quote

from flywheel_orchestrator._supervision_policy import (
    RespawnDecision,
    SupervisionPolicy,
)


# How long ``stop()`` waits for the child to exit after SIGTERM before
# giving up and reporting failure. The worker's graceful shutdown
# finalizes the in-flight lifecycle inside ``run_task_object``; a few
# seconds of headroom covers the post-finalize cleanup. Tests override
# to a smaller value so the suite stays snappy.
DEFAULT_STOP_TIMEOUT_SECONDS: float = 10.0


class WorkerState(str, enum.Enum):
    """The states the console's status bar renders.

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
    session and was NOT respawned (no crash-loop policy, or a disabled
    budget-0 policy). The status bar surfaces this so the operator can
    ``/worker start`` to respawn (the new worker's startup recovery
    sweep handles any stranded lifecycles, per existing worker
    semantics).

    ``DEAD_AFTER_BUDGET`` -- the supervised child kept dying and
    exhausted the shared windowed crash-loop budget: the supervisor
    stopped auto-respawning and latched this distinct, queryable
    terminal state (the safety interlock -- see spec 00070). Unlike
    ``DEAD`` it means "we tried and gave up", not "died once"; an
    operator ``/worker start`` re-arms a fresh budget window.

    ``ERROR`` -- the most recent ``start()`` call failed before the
    child could be launched (bad env, missing executable). The
    message field carries the underlying ``OSError`` detail so the
    operator can fix and retry via ``/worker start``.
    """

    SUPERVISED = "supervised"
    DETACHED = "detached"
    NONE = "none"
    DEAD = "dead"
    DEAD_AFTER_BUDGET = "dead_after_budget"
    ERROR = "error"


@dataclass(frozen=True, kw_only=True)
class WorkerStatus:
    """Snapshot of supervisor state for the status bar.

    Immutable so the dashboard can stash one per tick without worrying
    about the supervisor mutating it from underneath. ``pid`` is set
    only for ``SUPERVISED`` / ``DEAD`` / ``DEAD_AFTER_BUDGET`` (the
    states where this console knows the PID -- the last dead child's
    for the two terminal ones); ``message`` carries human-readable
    detail for ``DEAD`` / ``DEAD_AFTER_BUDGET`` (exit code + captured
    reason) and ``ERROR`` (spawn failure) so the status bar can show
    it inline.
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
    # Percent-encode the path so a ``?`` or ``#`` in it (e.g. a worktree dir
    # named for a branch) is not parsed as the URI's query/fragment delimiter,
    # which would silently truncate the path and open a different, table-less
    # database -- making a live lease read as absent. ``quote`` keeps ``/``
    # safe so the path structure is preserved.
    uri = f"file:{quote(str(db_path))}?mode=ro"
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


def build_default_spawn_argv(
    db_path: Path, *, tasks_dir: Path | None, model: str | None = None
) -> list[str]:
    """Compose the ``python -m flywheel_worktree.worker ...`` argv.

    Used by the production ``WorkerSupervisor`` to launch the daemon
    in-band. Tests pass a substitute argv (a trivial sleep child) so
    the supervisor's ownership / quit-path behaviour is exercised
    without spawning the real worker. The args are the minimal pair
    the worker needs to land on the same store the console is
    watching; everything else (heartbeat, lease seconds) defaults to
    the worker's own argparse defaults.

    ``model`` is the agent model id the console resolved via the
    ``--model`` CLI flag / ``flywheel.toml`` ``[agent] model`` policy.
    When set, it is appended as ``--model <value>`` so the spawned
    worker pins every task to that model; when ``None``, the flag is
    omitted entirely and the worker's own ``--model`` default (also
    ``None``) lets the SDK fall through to the Claude Code default.
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
    if model is not None:
        argv.extend(["--model", model])
    return argv


_DEATH_REASON_MAX_LEN = 240


def read_supervised_death_reason(log_path: Path | None) -> str | None:
    """Extract a human-readable death reason from a dead child's log file.

    A supervised child's stdout+stderr are redirected to ``log_path``; on an
    unexpected exit the most informative line is typically the last non-blank
    line — e.g. ``flywheel worker: policy error: ...`` (a config problem the
    operator can fix) or the final ``SomeError: ...`` of a traceback. Returns
    that line (capped), skipping the ``RuntimeWarning`` the ``python -m``
    launcher emits, or ``None`` when the log is empty/unreadable. Best-effort:
    never raises — a missing reason simply degrades to the bare exit code.
    """
    if log_path is None:
        return None
    try:
        text = log_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    lines = [ln.strip() for ln in text.splitlines() if ln.strip()]
    meaningful = [
        ln
        for ln in lines
        if "RuntimeWarning" not in ln and "found in sys.modules" not in ln
    ]
    candidates = meaningful or lines
    if not candidates:
        return None
    reason = candidates[-1]
    if len(reason) > _DEATH_REASON_MAX_LEN:
        reason = reason[: _DEATH_REASON_MAX_LEN - 1] + "…"
    return reason


def format_dead_message(exit_code: int | None, reason: str | None) -> str:
    """Compose the ``DEAD`` status message: the exit code plus, when known,
    the captured failure reason so the console explains *why* the child died
    instead of only that it did."""
    base = f"exit={exit_code}"
    return f"{base}: {reason}" if reason else base


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
        model: Optional agent model id the console resolved
            (``--model`` CLI flag > ``flywheel.toml`` ``[agent] model``
            > ``None``). Forwarded to the spawned worker as
            ``--model <value>`` when set, omitted entirely otherwise so
            the SDK keeps falling through to the Claude Code default.
            Ignored when ``spawn_argv`` is provided.
        policy: Optional shared crash-loop supervision policy (spec
            00070). When ``None`` (the default) the supervisor keeps its
            pre-respawn behavior exactly -- an unexpected death is
            reported ``DEAD`` and never auto-respawned. When supplied,
            a death inside the policy's windowed budget is auto-respawned
            (a real new child, inside ``status()`` with no operator
            ``start()``), and a death past the budget latches the
            distinct ``DEAD_AFTER_BUDGET`` state. A disabled (budget-0)
            policy behaves exactly like ``None``: the operator's
            unattended-base-branch safety override.
    """

    def __init__(
        self,
        *,
        db_path: Path,
        log_dir: Path | None = None,
        spawn_argv: Sequence[str] | None = None,
        tasks_dir: Path | None = None,
        model: str | None = None,
        policy: SupervisionPolicy | None = None,
    ) -> None:
        self._db_path = db_path
        self._policy = policy
        self._log_dir = (
            log_dir if log_dir is not None else Path(".flywheel/logs/worker")
        )
        if spawn_argv is not None:
            self._spawn_argv: list[str] = list(spawn_argv)
        else:
            self._spawn_argv = build_default_spawn_argv(
                db_path, tasks_dir=tasks_dir, model=model
            )
        self._child: subprocess.Popen[bytes] | None = None
        self._log_handle: IO[bytes] | None = None
        # Path of the current child's redirected log; kept after the handle is
        # closed so a death can be explained by reading the log's tail.
        self._log_path: Path | None = None
        self._last_error: str | None = None
        # Marks whether the most recent supervised child exited
        # unexpectedly mid-session. Cleared on the next successful
        # ``start()``; flips on when ``status()`` first observes a
        # non-running child it owned. The dashboard reads this through
        # ``status().state == DEAD`` so the death is visible until the
        # operator respawns. ``_dead_reason`` is the captured "why" (the log's
        # last meaningful line) surfaced alongside the exit code.
        self._dead_pid: int | None = None
        self._dead_exit: int | None = None
        self._dead_reason: str | None = None
        # Latched ``True`` once a policy-governed child exhausts the
        # crash-loop budget: the supervisor stops respawning and reports
        # ``DEAD_AFTER_BUDGET`` until an operator ``start()`` re-arms it.
        self._dead_after_budget: bool = False

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
            # Child died on its own; capture *why* (the log tail) before
            # reaping, then let the crash-loop policy decide whether this
            # tick auto-respawns it or latches the exhausted terminal state.
            dead_pid = self._child.pid
            dead_exit = rc
            dead_reason = read_supervised_death_reason(self._log_path)
            self._reap_child()
            respawned = self._respawn_or_retire(dead_pid, dead_exit, dead_reason)
            if respawned is not None:
                return respawned

        if self._dead_after_budget:
            return WorkerStatus(
                state=WorkerState.DEAD_AFTER_BUDGET,
                pid=self._dead_pid,
                message=format_dead_message(self._dead_exit, self._dead_reason),
            )
        if self._dead_pid is not None:
            return WorkerStatus(
                state=WorkerState.DEAD,
                pid=self._dead_pid,
                message=format_dead_message(self._dead_exit, self._dead_reason),
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

        # Clear any leftover DEAD / DEAD_AFTER_BUDGET / ERROR state from
        # prior attempts so the supervisor's snapshot matches reality
        # post-spawn.
        self._dead_pid = None
        self._dead_exit = None
        self._dead_reason = None
        self._dead_after_budget = False
        self._last_error = None

        # A DEAD / ERROR status short-circuits status() *before* its
        # has_live_lease() check, so a live lease (the just-died child's own
        # not-yet-expired lease, or a peer worker's) would otherwise be missed
        # and we would spawn a duplicate against a store that already has a
        # live worker. Now that the DEAD/ERROR flags are cleared, re-evaluate:
        # a live lease surfaces as DETACHED and we spawn nothing.
        rechecked = self.status()
        if rechecked.state == WorkerState.DETACHED:
            return rechecked

        # An operator start() re-arms the crash-loop budget: forget the prior
        # window's deaths so a manual restart begins with a full budget again
        # (after DEAD_AFTER_BUDGET, this is what lets the operator retry).
        if self._policy is not None:
            self._policy.reset()

        return self._spawn_child()

    def stop(
        self, *, timeout: float = DEFAULT_STOP_TIMEOUT_SECONDS
    ) -> bool:
        """Stop the supervised worker and everything it spawned.

        Signals the child's whole process *group*, not just its pid: the worker
        is its own session leader (``start_new_session=True``), so the agent
        subprocesses it spawns share its group and must come down with it --
        signaling only the worker pid leaves those agents orphaned. A graceful
        SIGTERM goes first (giving the worker's shutdown path its ``timeout``
        window to finalize the in-flight lifecycle to ``interrupted``); if the
        group is still alive after ``timeout`` -- a worker blocked mid-agent-
        call cannot honor the signal until that call returns -- it is escalated
        to SIGKILL so the console never exits leaving an orphan. A force-killed
        worker's lease simply lapses and another worker reclaims the task.

        Returns ``True`` when a running worker was stopped (gracefully or
        force-killed); ``False`` when there is no supervised child. Idempotent.
        """

        if self._child is None:
            return False
        rc = self._child.poll()
        if rc is not None:
            # Already exited before we signaled it (an unexpected death):
            # capture why so the DEAD status can explain it.
            self._dead_pid = self._child.pid
            self._dead_exit = rc
            self._dead_reason = read_supervised_death_reason(self._log_path)
            self._reap_child()
            return False

        if not self._signal_group(signal.SIGTERM):
            # Child died between the poll and the signal; reap and report it
            # stopped (there is nothing left running).
            self._dead_pid = self._child.pid
            self._dead_exit = self._child.poll() or 0
            self._dead_reason = read_supervised_death_reason(self._log_path)
            self._reap_child()
            return True

        try:
            exit_code = self._child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            # Blocked mid-agent-call: force the whole group down rather than
            # orphan it, then reap so no zombie is left behind.
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
        self._dead_reason = None
        self._dead_after_budget = False
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

    def _respawn_or_retire(
        self,
        dead_pid: int,
        dead_exit: int | None,
        dead_reason: str | None,
    ) -> WorkerStatus | None:
        """Decide what to do about a child that died on its own.

        With no policy, or a disabled (budget-0) policy, this reproduces the
        pre-respawn behavior exactly: record the death and fall through to a
        plain ``DEAD`` status, never respawning (the operator's unattended-
        base-branch safety override). With an active policy it charges the
        death against the shared windowed crash-loop budget -- a death inside
        budget launches a real new child (returning ``SUPERVISED`` with the new
        pid, no operator ``start()``); a death past budget stops respawning and
        latches the distinct ``DEAD_AFTER_BUDGET`` terminal state.

        Returns the ``SUPERVISED`` status when a respawn was launched (so
        ``status()`` returns it directly); otherwise ``None`` so ``status()``
        falls through to its DEAD / DEAD_AFTER_BUDGET tail.
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
            if spawned.state == WorkerState.SUPERVISED:
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

    def _spawn_child(self) -> WorkerStatus:
        """Spawn one worker child, redirecting its output to a fresh log.

        The single spawn seam shared by the operator ``start()`` and the
        automatic in-budget respawn, so both take the identical orphan-safe
        path: the child is placed in its own session
        (``start_new_session=True``) so a Ctrl+C on the console terminal never
        reaches it (only ``stop()`` signals it), and its stdout/stderr land in
        a per-spawn supervisor log. Returns ``SUPERVISED`` with the new pid on
        success, or ``ERROR`` (with the cause latched in ``_last_error``) when
        the log cannot be opened or ``Popen`` raises.
        """
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

    def _signal_group(self, sig: int) -> bool:
        """Signal the child's process group; ``False`` if it no longer exists.

        The worker is a session leader, so its pgid equals its pid and the
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
    "format_dead_message",
    "has_live_lease",
    "read_supervised_death_reason",
]
