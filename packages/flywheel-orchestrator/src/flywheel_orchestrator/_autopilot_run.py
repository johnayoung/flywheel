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

    shutdown = {"requested": False}

    def _flag(signum: int, _frame: object) -> None:
        shutdown["requested"] = True
        log(f"stop signal {signum}; exiting after the current cycle")

    def should_stop() -> bool:
        return shutdown["requested"]

    def before_cycle() -> None:
        _arm_signals(_flag)

    def on_cycle(result: AutopilotPassResult) -> None:
        _log_result(log, result)
        before_cycle()

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
