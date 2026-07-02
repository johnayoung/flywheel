"""Headless supervised autopilot: run the daemon under the shared policy.

``flywheel autopilot`` (:mod:`._autopilot_run`) is the neverending daemon with
no supervisor -- a crash simply ends it. This module is the headless supervised
entrypoint (spec 00070, criterion #5): it runs that same daemon as a child under
the SAME shared crash-loop budget (:class:`SupervisionPolicy`) and the SAME
liveness-record adoption (:func:`read_live_activity`) the console
``AutopilotSupervisor`` enforces, so a crashed headless daemon is restarted
inside its window and an already-live daemon is adopted rather than duplicated.

It is a RUN path, not a cold-start default: nothing here auto-starts autopilot
where it was manual-start-only (the unattended-base-branch safety default is
unchanged). Invoke it explicitly:

    python -m flywheel_orchestrator._autopilot_supervised_run

The spawned child is the neverending daemon (NO ``--once``), placed in its own
session so a Ctrl+C on the launching terminal never races it; a SIGTERM/SIGINT
to this supervisor is forwarded to the child's whole group so the daemon shuts
down gracefully and no agent/MCP grandchild is orphaned on stop.
"""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import IO

from flywheel_orchestrator._autopilot_activity import (
    AutopilotActivity,
    read_live_activity,
)
from flywheel_orchestrator._autopilot_run import (
    SupervisedChild,
    SupervisedOutcome,
    _repo_root,
    make_logger,
    run_supervised,
)
from flywheel_orchestrator._supervision_policy import (
    RespawnDecision,
    SupervisionBudget,
    SupervisionPolicy,
)

# The same default crash-loop budget the console supervisors enforce
# (``flywheel._tui._SUPERVISION_BUDGET``): a handful of respawns inside a rolling
# window so a transient death self-heals while a persistent boot-loop is
# contained -- for autopilot, that caps unattended writes to the base branch.
DEFAULT_MAX_RESPAWNS = 5
DEFAULT_WINDOW_SECONDS = 300.0

# How long a graceful SIGTERM is given before escalating the child group to
# SIGKILL, mirroring the console supervisor's stop window so a daemon blocked
# mid-cycle is still brought down rather than orphaned.
DEFAULT_STOP_TIMEOUT_SECONDS = 10.0


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python -m flywheel_orchestrator._autopilot_supervised_run",
        description=(
            "Run the autopilot daemon headless under the shared crash-loop "
            "budget: restart a daemon that dies inside its window, adopt an "
            "already-live daemon via its liveness record, and stop respawning "
            "once the budget is exhausted."
        ),
    )
    parser.add_argument("--tasks-dir", default=None)
    parser.add_argument("--model", default=None)
    parser.add_argument(
        "--activity-file",
        default=None,
        help=(
            "Path to the daemon's activity/liveness record (defaults to "
            "<repo>/.flywheel/logs/autopilot/activity.json)."
        ),
    )
    parser.add_argument(
        "--max-respawns",
        type=int,
        default=DEFAULT_MAX_RESPAWNS,
        help="Deaths tolerated inside the rolling window before giving up.",
    )
    parser.add_argument(
        "--window-seconds",
        type=float,
        default=DEFAULT_WINDOW_SECONDS,
        help="Rolling crash-loop window in seconds.",
    )
    return parser


def _build_daemon_argv(
    *, tasks_dir: str | None, model: str | None, activity_file: Path
) -> list[str]:
    """Compose the ``python -m ..._autopilot_run`` argv for the daemon child.

    Carries NO ``--once`` so the child is the neverending loop, and pins the
    ``--activity-file`` so the daemon and this supervisor agree on the one
    liveness record (the same contract the console supervisor uses).
    """
    argv: list[str] = [
        sys.executable,
        "-m",
        "flywheel_orchestrator._autopilot_run",
        "--activity-file",
        str(activity_file),
    ]
    if tasks_dir is not None:
        argv.extend(["--tasks-dir", tasks_dir])
    if model is not None:
        argv.extend(["--model", model])
    return argv


def _signal_group(pid: int, sig: int) -> None:
    """Signal the child's whole process group, ignoring an already-gone group.

    The daemon is its own session leader (``start_new_session=True``), so its
    pgid equals its pid and the signal reaches every agent/MCP descendant it
    spawned -- signaling only the pid would orphan those grandchildren.
    """
    try:
        pgid = os.getpgid(pid)
    except ProcessLookupError:
        return
    try:
        os.killpg(pgid, sig)
    except ProcessLookupError:
        return


def _close(handle: IO[bytes]) -> None:
    try:
        handle.close()
    except OSError:
        pass


def _open_log(log_dir: Path) -> IO[bytes]:
    log_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
    return open(log_dir / f"autopilot-supervised-{ts}.log", "ab", buffering=0)


def _spawn_daemon(
    daemon_argv: Sequence[str], *, log_dir: Path, stop_timeout: float
) -> SupervisedChild:
    """Spawn one daemon child and wrap it as an orphan-safe SupervisedChild.

    The child is placed in its own session so a Ctrl+C on the launching terminal
    never reaches it and only an explicit stop signals it; its output lands in a
    fresh per-spawn log. ``wait`` blocks on the child; ``terminate`` brings the
    whole group down (SIGTERM, then SIGKILL after ``stop_timeout``) so a daemon
    blocked mid-cycle is never left orphaned.
    """
    log_handle = _open_log(log_dir)
    proc = subprocess.Popen(
        list(daemon_argv),
        stdout=log_handle,
        stderr=subprocess.STDOUT,
        env=os.environ.copy(),
        start_new_session=True,
        close_fds=True,
    )

    def wait() -> int:
        try:
            return proc.wait()
        finally:
            _close(log_handle)

    def terminate() -> None:
        if proc.poll() is None:
            _signal_group(proc.pid, signal.SIGTERM)
            try:
                proc.wait(timeout=stop_timeout)
            except subprocess.TimeoutExpired:
                _signal_group(proc.pid, signal.SIGKILL)
                try:
                    proc.wait(timeout=stop_timeout)
                except subprocess.TimeoutExpired:
                    pass
        _close(log_handle)

    return SupervisedChild(pid=proc.pid, wait=wait, terminate=terminate)


def main(argv: Sequence[str] | None = None) -> int:
    """Run the autopilot daemon headless under the shared supervision policy.

    Returns 0 on a clean stop or an adoption, 1 when the daemon exhausted the
    crash-loop budget (the loud DEAD-after-budget terminal state), and 2 on a
    bad budget argument.
    """
    args = _build_parser().parse_args(argv)
    log = make_logger("[autopilot-supervised]")

    if args.max_respawns < 0:
        print(
            "autopilot-supervised: --max-respawns must be >= 0",
            file=sys.stderr,
        )
        return 2
    if args.window_seconds <= 0:
        print(
            "autopilot-supervised: --window-seconds must be > 0",
            file=sys.stderr,
        )
        return 2

    repo_root = _repo_root()
    activity_path = (
        Path(args.activity_file)
        if args.activity_file
        else repo_root / ".flywheel" / "logs" / "autopilot" / "activity.json"
    )
    log_dir = activity_path.parent
    daemon_argv = _build_daemon_argv(
        tasks_dir=args.tasks_dir, model=args.model, activity_file=activity_path
    )
    policy = SupervisionPolicy(
        SupervisionBudget(
            max_respawns=args.max_respawns, window_seconds=args.window_seconds
        )
    )

    stop: dict[str, bool] = {"requested": False}
    current: dict[str, SupervisedChild | None] = {"child": None}

    def _forward(signum: int, _frame: object) -> None:
        stop["requested"] = True
        child = current["child"]
        if child is not None:
            child.terminate()
        log(f"stop signal {signum}; shutting the daemon down and exiting")

    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, _forward)

    def spawn() -> SupervisedChild:
        child = _spawn_daemon(
            daemon_argv,
            log_dir=log_dir,
            stop_timeout=DEFAULT_STOP_TIMEOUT_SECONDS,
        )
        current["child"] = child
        return child

    def on_spawn(child: SupervisedChild) -> None:
        log(f"spawned autopilot daemon pid={child.pid}")

    def on_death(exit_code: int, decision: RespawnDecision) -> None:
        log(f"daemon exited (exit={exit_code}); decision={decision.value}")

    def on_adopt(record: AutopilotActivity) -> None:
        log(f"adopted live autopilot daemon pid={record.pid}; spawning nothing")

    log(
        f"headless supervised autopilot repo={repo_root} "
        f"budget={args.max_respawns}/{args.window_seconds}s pid={os.getpid()}"
    )
    result = run_supervised(
        spawn=spawn,
        policy=policy,
        should_stop=lambda: stop["requested"],
        read_liveness=lambda: read_live_activity(activity_path),
        on_spawn=on_spawn,
        on_death=on_death,
        on_adopt=on_adopt,
    )

    if result.outcome is SupervisedOutcome.DEAD_AFTER_BUDGET:
        log(
            f"daemon exhausted the crash-loop budget after {result.deaths} "
            "death(s) in the window; stopped respawning (dead_after_budget)."
        )
        return 1
    if result.outcome is SupervisedOutcome.ADOPTED:
        log(f"exiting; adopted daemon pid={result.adopted_pid} keeps running.")
        return 0
    log(f"stopped after {result.spawn_count} spawn(s).")
    return 0


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())
