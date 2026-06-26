"""``flywheel autopilot`` entry: the single refill pass and (daemon) loop.

autopilot-loop builds the ``--once`` single-pass verb here; autopilot-daemon
wraps the same pass in a neverending loop on an interval. The composed pass
(discovery -> score -> author -> emit) lives in
:mod:`flywheel_orchestrator._autopilot`; this module is the thin CLI/runtime
shell over it -- argument parsing, policy resolution, logging -- mirroring how
``flywheel worker`` shells over the worktree daemon.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import os
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Sequence
from pathlib import Path

from flywheel_orchestrator._autopilot import (
    DEFAULT_WEIGHTS,
    AutopilotPassResult,
    ScoreWeights,
    run_refill_pass,
)
from flywheel_orchestrator._autopilot_activity import (
    PHASE_IDLE,
    PHASE_RUNNING,
    PHASE_STARTING,
    AutopilotActivity,
    EmittedSummary,
    write_activity,
)
from flywheel_orchestrator._policy import PolicyError, WorkPolicy
from flywheel_orchestrator._workflow import (
    DEFAULT_TASKS_DIR,
    load_effective_policy,
)


def make_logger(prefix: str) -> Callable[[str], None]:
    def log(message: str) -> None:
        print(f"{prefix} {message}", file=sys.stderr, flush=True)

    return log


def _repo_root() -> Path:
    proc = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise SystemExit("ERROR: not inside a git repository.")
    return Path(proc.stdout.strip())


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="flywheel autopilot",
        description=(
            "Keep the work queue full with verifiable, tier-prioritized tasks. "
            "Runs as a neverending daemon by default; --once runs a single "
            "refill pass and exits."
        ),
    )
    parser.add_argument("--tasks-dir", default=None)
    parser.add_argument("--model", default=None)
    parser.add_argument(
        "--target-depth",
        type=int,
        default=None,
        help="Fill the queue up to this depth (overrides [autopilot] config).",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=None,
        help="Seconds between daemon cycles (overrides [autopilot] config).",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Run a single refill pass and exit (no daemon loop).",
    )
    parser.add_argument(
        "--activity-file",
        default=None,
        help=(
            "Path to write the live activity snapshot the console reads "
            "(defaults to <repo>/.flywheel/logs/autopilot/activity.json)."
        ),
    )
    return parser


def _resolve_runtime(
    args: argparse.Namespace, policy: WorkPolicy | None
) -> tuple[Path, int, str, ScoreWeights]:
    """Resolve (tasks_dir, target_depth, landing, weights) from flags + policy."""
    if args.tasks_dir:
        tasks_dir = Path(args.tasks_dir)
    elif policy is not None and policy.tasks_dir is not None:
        tasks_dir = policy.tasks_dir
    else:
        tasks_dir = DEFAULT_TASKS_DIR
    target_depth = (
        args.target_depth
        if args.target_depth is not None
        else (policy.autopilot_target_depth if policy is not None else 5)
    )
    landing = policy.autopilot_landing if policy is not None else "merge"
    weights = (
        policy.autopilot_weights
        if policy is not None and policy.autopilot_weights is not None
        else DEFAULT_WEIGHTS
    )
    return tasks_dir, target_depth, landing, weights


def run_single_pass(
    *,
    repo_root: Path,
    tasks_dir: Path,
    target_depth: int,
    landing: str,
    weights: ScoreWeights,
    model: str | None,
) -> AutopilotPassResult:
    """Drive exactly one refill pass with the real SDK-backed invokers."""
    return asyncio.run(
        run_refill_pass(
            tasks_dir=tasks_dir,
            repo_root=repo_root,
            target_depth=target_depth,
            weights=weights,
            landing=landing,
            model=model,
        )
    )


# --- The neverending daemon loop (autopilot-daemon) -------------------------


def run_daemon_loop(
    *,
    run_cycle: Callable[[], AutopilotPassResult],
    interval_seconds: float,
    should_stop: Callable[[], bool],
    sleep: Callable[[float, Callable[[], bool]], None],
    on_cycle: Callable[[AutopilotPassResult], None] | None = None,
    before_cycle: Callable[[], None] | None = None,
    max_cycles: int | None = None,
) -> int:
    """Run refill passes on an interval until an explicit stop signal.

    The testable core of ``flywheel autopilot`` (no ``--once``): each cycle
    runs one ``run_cycle`` pass, then waits ``interval_seconds`` before the
    next. It MUST NOT terminate on an idle (nothing-actionable) cycle -- an
    empty pass writes nothing and the loop continues (D-5). The loop exits only
    when ``should_stop()`` is true (an injected stop event in tests; a
    SIGTERM/SIGINT flag in production), and at no other time.

    Every collaborator is injected so the loop runs with no real wall-clock
    waits and no live model: ``run_cycle`` is the (scripted) pass, ``sleep``
    the interruptible wait, ``should_stop`` the stop signal. ``before_cycle``
    re-arms signal handlers each iteration (``asyncio.run`` inside a real pass
    reclaims them). ``max_cycles`` is a test-only safety bound; production
    leaves it ``None`` (truly neverending).

    Returns the number of cycles run.
    """
    cycles = 0
    while not should_stop():
        if before_cycle is not None:
            before_cycle()
        result = run_cycle()
        cycles += 1
        if on_cycle is not None:
            on_cycle(result)
        if max_cycles is not None and cycles >= max_cycles:
            break
        if should_stop():
            break
        sleep(interval_seconds, should_stop)
    return cycles


def _arm_signals(handler: Callable[[int, object], None]) -> None:
    """Install the shutdown-flag handler for SIGTERM/SIGINT.

    Re-armed each cycle because ``asyncio.run`` (inside a pass) takes these
    signals over for the run and restores their default disposition afterward
    -- mirroring the worker daemon.
    """
    for sig in (signal.SIGTERM, signal.SIGINT):
        with contextlib.suppress(ValueError):
            signal.signal(sig, handler)


def _interruptible_sleep(seconds: float, should_stop: Callable[[], bool]) -> None:
    """Sleep up to ``seconds``, waking immediately when ``should_stop`` flips."""
    whole = int(max(seconds, 0))
    for _ in range(whole):
        if should_stop():
            return
        time.sleep(1)


def _log_result(log: Callable[[str], None], result: AutopilotPassResult) -> None:
    log(result.reason)
    for path in result.emitted_paths:
        log(f"emitted {path}")
    for drop in result.dropped:
        log(f"dropped {drop.finding.id}: {drop.reason}")


class _ActivityRecorder:
    """Writes the daemon's per-cycle activity snapshots for the console.

    Extracted from :func:`main` so the snapshot sequence is testable by driving
    :func:`run_daemon_loop` with a scripted ``run_cycle`` and no live model:
    ``starting`` before the first cycle, ``running`` at each cycle start
    (carrying the *previous* cycle's summary so the console keeps showing "last:
    N emitted" while the next cycle runs), and ``idle`` with ``next_cycle_at``
    after each cycle. ``clock`` is injected so ``next_cycle_at`` is deterministic
    in tests. Snapshot I/O is best-effort -- a write failure never crashes a
    cycle.
    """

    def __init__(
        self,
        *,
        path: Path,
        interval_seconds: float,
        pid: int | None = None,
        clock: Callable[[], float] = time.time,
    ) -> None:
        self._path = path
        self._interval = interval_seconds
        self._pid = pid if pid is not None else os.getpid()
        self._clock = clock
        self._cycle_index = 0
        self._last_result: AutopilotPassResult | None = None

    def _write(
        self, phase: str, *, now: float, next_cycle_at: float | None = None
    ) -> None:
        result = self._last_result
        activity = AutopilotActivity(
            pid=self._pid,
            phase=phase,
            cycle_index=self._cycle_index,
            updated_at=now,
            interval_seconds=self._interval,
            next_cycle_at=next_cycle_at,
            last_emitted=0 if result is None else result.emitted_count,
            last_dropped=0 if result is None else len(result.dropped),
            last_reason="" if result is None else result.reason,
            last_relevant_tiers=(
                ()
                if result is None
                else tuple(int(t) for t in result.relevant_tiers)
            ),
            last_emitted_tasks=(
                ()
                if result is None
                else tuple(
                    EmittedSummary(task_id=e.task.id, tier=int(e.finding.tier))
                    for e in result.emitted
                )
            ),
        )
        try:
            write_activity(self._path, activity)
        except OSError:
            pass

    def starting(self) -> None:
        self._write(PHASE_STARTING, now=self._clock())

    def before_cycle(self) -> None:
        self._cycle_index += 1
        self._write(PHASE_RUNNING, now=self._clock())

    def on_cycle(self, result: AutopilotPassResult) -> None:
        self._last_result = result
        now = self._clock()
        self._write(PHASE_IDLE, now=now, next_cycle_at=now + self._interval)


def main(argv: Sequence[str] | None = None) -> int:
    """Run ``flywheel autopilot``: a neverending daemon, or one pass.

    Bare ``flywheel autopilot`` runs the neverending refill loop on the
    configured interval (idling, not exiting, on an empty cycle, D-5),
    stopping only on SIGTERM/SIGINT. ``--once`` runs exactly one refill pass
    and exits 0 (the testable unit and a manual escape hatch). Mirrors
    ``flywheel worker`` (daemon by default, ``--once`` for a single drain).
    """
    args = _build_parser().parse_args(argv)
    repo_root = _repo_root()
    log = make_logger("[autopilot]")

    try:
        policy = load_effective_policy()
    except PolicyError as exc:
        print(f"flywheel autopilot: policy error: {exc}", file=sys.stderr)
        return 2

    tasks_dir, target_depth, landing, weights = _resolve_runtime(args, policy)
    model = args.model or (policy.model if policy is not None else None)

    def run_cycle() -> AutopilotPassResult:
        return run_single_pass(
            repo_root=repo_root,
            tasks_dir=tasks_dir,
            target_depth=target_depth,
            landing=landing,
            weights=weights,
            model=model,
        )

    if args.once:
        log(
            f"single pass repo={repo_root} tasks={tasks_dir} "
            f"target_depth={target_depth} landing={landing}"
        )
        _log_result(log, run_cycle())
        return 0

    interval = (
        args.interval
        if args.interval is not None
        else (
            policy.autopilot_interval_seconds
            if policy is not None
            else 300.0
        )
    )
    log(
        f"daemon started repo={repo_root} tasks={tasks_dir} "
        f"target_depth={target_depth} landing={landing} interval={interval}s "
        f"pid={os.getpid()}"
    )

    activity_path = (
        Path(args.activity_file)
        if args.activity_file
        else repo_root / ".flywheel" / "logs" / "autopilot" / "activity.json"
    )
    recorder = _ActivityRecorder(path=activity_path, interval_seconds=interval)
    recorder.starting()

    shutdown = {"requested": False}

    def _flag(signum: int, _frame: object) -> None:
        shutdown["requested"] = True
        log(f"stop signal {signum}; exiting after the current cycle")

    def should_stop() -> bool:
        return shutdown["requested"]

    def before_cycle() -> None:
        _arm_signals(_flag)
        recorder.before_cycle()

    def on_cycle(result: AutopilotPassResult) -> None:
        _log_result(log, result)
        recorder.on_cycle(result)
        _arm_signals(_flag)

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=interval,
        should_stop=should_stop,
        sleep=_interruptible_sleep,
        on_cycle=on_cycle,
        before_cycle=before_cycle,
    )
    log(f"daemon stopped after {cycles} cycle(s)")
    return 0


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())
