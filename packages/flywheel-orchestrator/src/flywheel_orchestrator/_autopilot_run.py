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
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

from collections.abc import Callable

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


def main(argv: Sequence[str] | None = None) -> int:
    """Run ``flywheel autopilot`` -- a single pass under ``--once``.

    The neverending daemon form (bare ``flywheel autopilot``) is added by
    autopilot-daemon; this task wires the single pass and the verb.
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

    log(
        f"started repo={repo_root} tasks={tasks_dir} target_depth={target_depth} "
        f"landing={landing}"
    )
    result = run_single_pass(
        repo_root=repo_root,
        tasks_dir=tasks_dir,
        target_depth=target_depth,
        landing=landing,
        weights=weights,
        model=model,
    )
    log(result.reason)
    for path in result.emitted_paths:
        log(f"emitted {path}")
    for drop in result.dropped:
        log(f"dropped {drop.finding.id}: {drop.reason}")
    return 0


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())
