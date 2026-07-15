"""``flywheel triage`` entry: one triage pass and (daemon) the polling loop.

Mirrors ``flywheel autopilot``: a neverending label-polling daemon by default
that triages the intake board on an interval, and ``--once`` for a single pass.
The triage pass itself (list -> author -> grade -> label with fail-first
receipts) lives in :mod:`flywheel_orchestrator._triage`; this module is the
thin CLI/runtime shell over it -- argument parsing, policy resolution, signal
handling -- mirroring how ``flywheel autopilot`` shells over the refill pass.

Load discipline (spec 00082): the policy is resolved and validated BEFORE the
repo root or any GitHub seam is touched, so a malformed ``[triage]`` policy
exits 2 having issued no ``gh`` call and no git command -- the write-free
guarantee an idle or failing pass must honor. An idle pass (empty backlog)
writes nothing and the daemon simply schedules the next cycle; it never
terminates on an idle cycle.
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

from flywheel_orchestrator._github import GhRunner, _default_runner
from flywheel_orchestrator._policy import PolicyError
from flywheel_orchestrator._triage import (
    TriagePass,
    TriagePassResult,
    build_grader_executor,
    build_triage_authoring_invoker,
    resolve_base_sha,
)
from flywheel_orchestrator._workflow import load_effective_policy

# Consecutive whole-cycle failures (a triage pass raising unexpectedly) before
# the daemon gives up so an operator can inspect, rather than hot-looping.
# Mirrors the autopilot daemon's cross-cycle backstop: a single raising cycle
# is counted and backed off so the loop runs further cycles; a subsequent
# success resets the count; on the bounded count the loop stops by surfacing a
# give-up signal the caller turns into a non-zero exit (never a silent exit).
MAX_CONSECUTIVE_CYCLE_FAILURES = 5
CYCLE_FAILURE_BACKOFF_SECONDS = 10


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
        prog="flywheel triage",
        description=(
            "Triage the intake board: promote well-specified issues to the "
            "ready label and route under-specified ones to needs-detail, each "
            "with a fail-first receipt. Runs as a neverending label-polling "
            "daemon by default; --once runs a single triage pass and exits."
        ),
    )
    parser.add_argument("--model", default=None)
    parser.add_argument(
        "--interval",
        type=float,
        default=None,
        help="Seconds between daemon cycles (overrides [triage] config).",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Run a single triage pass and exit (no daemon loop).",
    )
    return parser


def run_single_pass(
    *,
    repo_root: Path,
    repo: str,
    intake_label: str,
    ready_label: str,
    needs_detail_label: str,
    per_pass_cap: int | None,
    model: str | None,
    runner: GhRunner,
    log: Callable[[str], None] | None = None,
) -> TriagePassResult:
    """Drive exactly one triage pass with the real SDK-backed seams.

    Resolves the base commit SHA the receipt binds to, builds the production
    authoring invoker (a repo-rooted claude session behind the lazy
    ``flywheel_core._sdk`` boundary) and grader executor, and runs the pass with
    the given ``gh`` ``runner``. ``per_pass_cap`` is the policy's
    ``[triage] max_per_pass`` wired verbatim into the engine's ``per_pass_cap``
    so a large board is drained in bounded batches.
    """
    base_sha = resolve_base_sha(repo_root)
    invoker = build_triage_authoring_invoker(repo_root, model=model)
    executor = build_grader_executor(repo_root)
    triage = TriagePass(
        repo=repo,
        intake_label=intake_label,
        ready_label=ready_label,
        needs_detail_label=needs_detail_label,
        base_sha=base_sha,
        invoker=invoker,
        executor=executor,
        runner=runner,
        per_pass_cap=per_pass_cap,
        log=log,
    )
    return asyncio.run(triage.run())


# --- The neverending daemon loop (mirrors the autopilot daemon) --------------


def run_daemon_loop(
    *,
    run_cycle: Callable[[], TriagePassResult],
    interval_seconds: float,
    should_stop: Callable[[], bool],
    sleep: Callable[[float, Callable[[], bool]], None],
    on_cycle: Callable[[TriagePassResult], None] | None = None,
    before_cycle: Callable[[], None] | None = None,
    on_cycle_failure: Callable[[BaseException, int], None] | None = None,
    on_give_up: Callable[[int], None] | None = None,
    max_cycles: int | None = None,
) -> int:
    """Run triage passes on an interval until an explicit stop signal.

    The testable core of ``flywheel triage`` (no ``--once``): each cycle runs
    one ``run_cycle`` pass, then waits ``interval_seconds`` before the next. It
    MUST NOT terminate on an idle (nothing-to-triage) cycle -- an empty pass
    writes nothing and the loop continues. The loop exits only when
    ``should_stop()`` is true (an injected stop event in tests; a SIGTERM/SIGINT
    flag in production), or when the circuit breaker gives up, and at no other
    time.

    Circuit breaker (mirrors the autopilot daemon): a ``run_cycle`` that raises
    is contained -- the exception is counted, ``on_cycle_failure`` is notified,
    and the loop backs off ``CYCLE_FAILURE_BACKOFF_SECONDS`` (via the injected,
    interruptible ``sleep``) before running a further cycle. A subsequent
    successful cycle resets the consecutive-failure count. After
    ``MAX_CONSECUTIVE_CYCLE_FAILURES`` consecutive failures the loop stops and
    calls ``on_give_up`` so the caller can surface a visible non-zero signal --
    never a silent exit. ``KeyboardInterrupt``/``asyncio.CancelledError`` are
    not counted: they stop the loop immediately.

    Every collaborator is injected so the loop runs with no real wall-clock
    waits and no live model: ``run_cycle`` is the (scripted) pass, ``sleep`` the
    interruptible wait, ``should_stop`` the stop signal. ``before_cycle``
    re-arms signal handlers each iteration (``asyncio.run`` inside a real pass
    reclaims them). ``max_cycles`` is a test-only safety bound; production
    leaves it ``None`` (truly neverending).

    Returns the number of cycles attempted (successful and failed).
    """
    cycles = 0
    consecutive_failures = 0
    while not should_stop():
        if before_cycle is not None:
            before_cycle()
        try:
            result = run_cycle()
        except (KeyboardInterrupt, asyncio.CancelledError):
            break
        except Exception as exc:  # noqa: BLE001 - one bad cycle must not crash the daemon
            consecutive_failures += 1
            cycles += 1
            if on_cycle_failure is not None:
                on_cycle_failure(exc, consecutive_failures)
            if consecutive_failures >= MAX_CONSECUTIVE_CYCLE_FAILURES:
                if on_give_up is not None:
                    on_give_up(consecutive_failures)
                break
            if max_cycles is not None and cycles >= max_cycles:
                break
            if should_stop():
                break
            sleep(CYCLE_FAILURE_BACKOFF_SECONDS, should_stop)
            continue
        consecutive_failures = 0
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
    -- mirroring the autopilot daemon.
    """
    for sig in (signal.SIGTERM, signal.SIGINT):
        with contextlib.suppress(ValueError):
            signal.signal(sig, handler)


def _interruptible_sleep(
    seconds: float, should_stop: Callable[[], bool]
) -> None:
    """Sleep up to ``seconds``, waking immediately when ``should_stop`` flips."""
    whole = int(max(seconds, 0))
    for _ in range(whole):
        if should_stop():
            return
        time.sleep(1)


def _log_result(
    log: Callable[[str], None], result: TriagePassResult
) -> None:
    log(
        f"pass complete: {len(result.ready)} ready, "
        f"{len(result.needs_detail)} needs-detail, "
        f"{result.deferred} deferred"
    )
    for outcome in result.outcomes:
        log(f"#{outcome.number}: {outcome.decision}")


def main(argv: Sequence[str] | None = None) -> int:
    """Run ``flywheel triage``: a neverending label-polling daemon, or one pass.

    Bare ``flywheel triage`` runs the neverending polling loop on the configured
    interval (idling, not exiting, on an empty cycle), stopping only on
    SIGTERM/SIGINT. ``--once`` runs exactly one triage pass and exits 0 (the
    testable unit and a manual escape hatch). Mirrors ``flywheel autopilot``
    (daemon by default, ``--once`` for a single pass).

    The policy is resolved and validated first: a malformed ``[triage]`` value
    prints to stderr and returns 2 having built no engine, resolved no repo
    root, and issued no ``gh`` call.
    """
    args = _build_parser().parse_args(argv)
    log = make_logger("[triage]")

    # Resolve and validate the policy BEFORE touching git or the gh seam: a
    # malformed [triage] value must exit 2 having issued no GitHub write and no
    # git command (spec 00082, the write-free guarantee).
    try:
        policy = load_effective_policy()
    except PolicyError as exc:
        print(f"flywheel triage: policy error: {exc}", file=sys.stderr)
        return 2

    if (
        policy is None
        or policy.source_kind != "github"
        or not policy.github_repo
    ):
        print(
            'flywheel triage: requires [source] kind = "github" with a repo '
            "(the intake board triage reads and writes)",
            file=sys.stderr,
        )
        return 2

    repo = policy.github_repo
    intake_label = policy.triage_intake_label
    ready_label = policy.triage_ready_label
    needs_detail_label = policy.triage_needs_detail_label
    per_pass_cap = policy.triage_max_per_pass
    model = args.model or policy.model
    runner = _default_runner

    repo_root = _repo_root()

    def run_cycle() -> TriagePassResult:
        return run_single_pass(
            repo_root=repo_root,
            repo=repo,
            intake_label=intake_label,
            ready_label=ready_label,
            needs_detail_label=needs_detail_label,
            per_pass_cap=per_pass_cap,
            model=model,
            runner=runner,
            log=log,
        )

    if args.once:
        log(
            f"single pass repo={repo} intake={intake_label} "
            f"ready={ready_label} needs_detail={needs_detail_label}"
        )
        _log_result(log, run_cycle())
        return 0

    interval = (
        args.interval
        if args.interval is not None
        else policy.triage_interval_seconds
    )
    log(
        f"daemon started repo={repo} intake={intake_label} "
        f"ready={ready_label} needs_detail={needs_detail_label} "
        f"interval={interval}s pid={os.getpid()}"
    )

    shutdown = {"requested": False}

    def _flag(signum: int, _frame: object) -> None:
        shutdown["requested"] = True
        log(f"stop signal {signum}; exiting after the current cycle")

    def should_stop() -> bool:
        return shutdown["requested"]

    def before_cycle() -> None:
        _arm_signals(_flag)

    def on_cycle(result: TriagePassResult) -> None:
        _log_result(log, result)
        _arm_signals(_flag)

    gave_up = {"requested": False}

    def on_cycle_failure(exc: BaseException, consecutive: int) -> None:
        log(
            f"cycle failed ({type(exc).__name__}: {exc}) "
            f"[{consecutive}/{MAX_CONSECUTIVE_CYCLE_FAILURES}]"
        )
        _arm_signals(_flag)

    def on_give_up(consecutive: int) -> None:
        gave_up["requested"] = True
        log(
            f"too many consecutive cycle failures ({consecutive}); "
            "exiting non-zero for operator inspection."
        )

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=interval,
        should_stop=should_stop,
        sleep=_interruptible_sleep,
        on_cycle=on_cycle,
        before_cycle=before_cycle,
        on_cycle_failure=on_cycle_failure,
        on_give_up=on_give_up,
    )
    log(f"daemon stopped after {cycles} cycle(s)")
    return 1 if gave_up["requested"] else 0


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())
