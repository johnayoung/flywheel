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
from datetime import datetime, timezone
from pathlib import Path
from typing import TextIO

from flywheel_core.faults import FaultClass, classify_fault
from flywheel_orchestrator._autopilot import (
    DEFAULT_WEIGHTS,
    AutopilotPassResult,
    ScoreWeights,
    actionable_queue_depth,
    run_refill_pass,
)
from flywheel_orchestrator._claims import SqliteClaimStore
from flywheel_orchestrator._orchestrate import (
    DEFAULT_NO_PROGRESS_BOUND,
    NoProgressObservation,
    NoProgressOutcome,
    redrive_no_progress,
)
from flywheel_orchestrator._store_factory import build_store
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
    queue_depth: Callable[[Path], int] | None = None,
) -> AutopilotPassResult:
    """Drive exactly one refill pass with the real SDK-backed invokers.

    ``queue_depth`` measures actionable work for the fill-to-target decision;
    ``None`` falls back to the directory adapter's raw active-file count (spec
    00062: the daemon passes a store-backed counter that excludes terminal
    tasks so a finished-but-unarchived task file never suppresses refill).
    """
    return asyncio.run(
        run_refill_pass(
            tasks_dir=tasks_dir,
            repo_root=repo_root,
            target_depth=target_depth,
            weights=weights,
            landing=landing,
            model=model,
            queue_depth=queue_depth,
        )
    )


def _build_queue_depth(
    policy: WorkPolicy | None,
    repo_root: Path,
    log: Callable[[str], None],
) -> Callable[[Path], int] | None:
    """Open a store-backed actionable-depth counter, or ``None`` on failure.

    Spec 00062: resolves the policy db path (default
    ``.flywheel/flywheel.sqlite`` under the repo, matching the worker), opens
    the store once, and returns a counter that excludes terminal tasks. A build
    failure (missing/locked db, missing backend extra) degrades to the raw
    active-file count -- today's behavior -- rather than crashing the daemon.
    """
    raw_db = (
        policy.db_path
        if policy is not None and policy.db_path is not None
        else Path(".flywheel") / "flywheel.sqlite"
    )
    db_path = raw_db if raw_db.is_absolute() else repo_root / raw_db
    try:
        store = build_store(policy, db_path=db_path, environ=os.environ)
    except Exception as exc:  # noqa: BLE001 - depth backing is best-effort
        log(
            f"queue-depth store unavailable ({type(exc).__name__}: {exc}); "
            "falling back to raw active-file count"
        )
        return None
    return lambda tasks_dir: actionable_queue_depth(tasks_dir, store)


# --- No-progress back-off around the refill pass (spec 00069, #9/#13) --------


def _cycle_made_progress(result: AutopilotPassResult) -> bool:
    """Did this refill cycle make observable progress on the repo unit?

    Progress is a task authored this cycle (``emitted_count > 0``) OR a queue
    already at/above target -- healthy backpressure, not a dead-end, exactly as
    the existing no-op ledger treats it. Only a below-target cycle that did
    discovery work yet authored nothing is a genuine no-progress cycle. This
    observes the pass result; it never changes discovery or authoring semantics.
    """
    if result.emitted_count > 0:
        return True
    return result.queue_depth_before >= result.target_depth


def apply_no_progress_backoff(
    result: AutopilotPassResult,
    *,
    claims: SqliteClaimStore,
    unit_id: str,
    bound: int = DEFAULT_NO_PROGRESS_BOUND,
    now: Callable[[], datetime] | None = None,
    stream: TextIO | None = None,
) -> NoProgressOutcome:
    """Feed one autopilot cycle to the bounded no-progress back-off re-driver.

    The autopilot repo IS the "unit": a cycle that authored nothing while below
    target made no headway. After ``bound`` such consecutive cycles the re-driver
    backs the repo off -- routing it to the single human-review queue with the
    machine-readable ``no-progress`` reason -- and the daemon stops re-attempting
    it. A cycle that makes progress resets the streak, so a repo that ever
    authors is never backed off. Returns the re-driver's outcome; a ``"queued"``
    result is the daemon's cue to stop.
    """
    clock = now if now is not None else (lambda: datetime.now(timezone.utc))
    (outcome,) = redrive_no_progress(
        claims,
        observations=[
            NoProgressObservation(
                unit_id=unit_id,
                progressed=_cycle_made_progress(result),
                detail=result.reason,
            )
        ],
        bound=bound,
        now=clock,
        stream=stream,
    )
    return outcome


def _open_backoff_claims(
    policy: WorkPolicy | None,
    repo_root: Path,
    log: Callable[[str], None],
) -> SqliteClaimStore | None:
    """Open the claim store the no-progress back-off records witnesses on.

    Resolves the same db path the worker/queue-depth counter use (default
    ``.flywheel/flywheel.sqlite`` under the repo). A build failure (missing
    backend, locked db) degrades to no back-off rather than crashing the daemon
    -- the loop simply keeps running, as it does today.
    """
    raw_db = (
        policy.db_path
        if policy is not None and policy.db_path is not None
        else Path(".flywheel") / "flywheel.sqlite"
    )
    db_path = raw_db if raw_db.is_absolute() else repo_root / raw_db
    try:
        return SqliteClaimStore(db_path)
    except Exception as exc:  # noqa: BLE001 - back-off is best-effort
        log(
            f"no-progress back-off store unavailable "
            f"({type(exc).__name__}: {exc}); back-off disabled"
        )
        return None


# --- The neverending daemon loop (autopilot-daemon) -------------------------

# Consecutive whole-cycle failures (a refill pass raising unexpectedly) before
# the daemon gives up so an operator can inspect, rather than hot-looping.
# Mirrors the worker daemon's cross-cycle backstop
# (``flywheel_worktree.worker.MAX_CONSECUTIVE_CYCLE_FAILURES`` /
# ``CYCLE_FAILURE_BACKOFF_SECONDS``): a single raising cycle is counted and
# backed off so the loop runs further cycles; a subsequent success resets the
# count; on the bounded count the loop stops by surfacing a give-up signal the
# caller turns into a non-zero exit (never a silent exit).
MAX_CONSECUTIVE_CYCLE_FAILURES = 5
CYCLE_FAILURE_BACKOFF_SECONDS = 10


def run_daemon_loop(
    *,
    run_cycle: Callable[[], AutopilotPassResult],
    interval_seconds: float,
    should_stop: Callable[[], bool],
    sleep: Callable[[float, Callable[[], bool]], None],
    on_cycle: Callable[[AutopilotPassResult], None] | None = None,
    before_cycle: Callable[[], None] | None = None,
    on_cycle_failure: Callable[[BaseException, int], None] | None = None,
    on_give_up: Callable[[int], None] | None = None,
    on_permanent_stop: Callable[[BaseException], None] | None = None,
    max_cycles: int | None = None,
) -> int:
    """Run refill passes on an interval until an explicit stop signal.

    The testable core of ``flywheel autopilot`` (no ``--once``): each cycle
    runs one ``run_cycle`` pass, then waits ``interval_seconds`` before the
    next. It MUST NOT terminate on an idle (nothing-actionable) cycle -- an
    empty pass writes nothing and the loop continues (D-5). The loop exits only
    when ``should_stop()`` is true (an injected stop event in tests; a
    SIGTERM/SIGINT flag in production), or when the circuit breaker gives up
    (below), and at no other time.

    Circuit breaker (mirrors the worker daemon): a ``run_cycle`` that raises is
    contained -- the exception is counted, ``on_cycle_failure`` is notified, and
    the loop backs off ``CYCLE_FAILURE_BACKOFF_SECONDS`` (via the injected,
    interruptible ``sleep``) before running a further cycle. A subsequent
    successful cycle resets the consecutive-failure count. After
    ``MAX_CONSECUTIVE_CYCLE_FAILURES`` consecutive failures the loop stops and
    calls ``on_give_up`` so the caller can surface a visible non-zero signal --
    never a silent exit. ``KeyboardInterrupt``/``asyncio.CancelledError`` are
    not counted: they stop the loop immediately.

    Permanent stop (distinct from the transient breaker): a raised fault that
    ``flywheel_core.faults.classify_fault`` buckets PERMANENT -- e.g. a
    ``StoreSchemaError``/``OrchestratorSchemaError`` from reopening a store
    whose ``schema_version`` row is wrong -- can never succeed on retry. It is
    NOT counted as a transient strike (it would otherwise burn all five and
    back off between each); instead the loop counts exactly this one cycle,
    calls ``on_permanent_stop`` (a signal distinct from ``on_cycle_failure`` /
    ``on_give_up`` that a caller/grader can assert on), and stops immediately.

    Every collaborator is injected so the loop runs with no real wall-clock
    waits and no live model: ``run_cycle`` is the (scripted) pass, ``sleep``
    the interruptible wait, ``should_stop`` the stop signal. ``before_cycle``
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
            # Permanent faults (a schema-version mismatch reopening the store
            # every cycle) can never succeed on retry, so they must not burn
            # the transient-strike budget: classify PERMANENT, count this one
            # cycle, surface the distinct permanent-stop signal, and stop --
            # never one strike per cycle up to MAX_CONSECUTIVE_CYCLE_FAILURES.
            if classify_fault(exc) is FaultClass.PERMANENT:
                cycles += 1
                if on_permanent_stop is not None:
                    on_permanent_stop(exc)
                break
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
    queue_depth = _build_queue_depth(policy, repo_root, log)

    def run_cycle() -> AutopilotPassResult:
        return run_single_pass(
            repo_root=repo_root,
            tasks_dir=tasks_dir,
            target_depth=target_depth,
            landing=landing,
            weights=weights,
            model=model,
            queue_depth=queue_depth,
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

    # Bounded no-progress back-off (spec 00069, #9/#13): a repo that keeps
    # authoring nothing while below target is backed off after a finite number of
    # fruitless cycles -- routed to the human-review queue and no longer
    # re-attempted -- so a never-progressing repo cannot burn agent cost forever.
    backoff_claims = _open_backoff_claims(policy, repo_root, log)
    backoff_unit = str(tasks_dir)

    def on_cycle(result: AutopilotPassResult) -> None:
        _log_result(log, result)
        recorder.on_cycle(result)
        if backoff_claims is not None:
            outcome = apply_no_progress_backoff(
                result, claims=backoff_claims, unit_id=backoff_unit
            )
            if outcome.result == "queued":
                shutdown["requested"] = True
                log(
                    f"no progress for {outcome.cycles} consecutive cycle(s); "
                    "backed off and routed to the human-review queue "
                    "(reason 'no-progress'); stopping."
                )
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

    try:
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
    finally:
        if backoff_claims is not None:
            backoff_claims.close()
    log(f"daemon stopped after {cycles} cycle(s)")
    return 1 if gave_up["requested"] else 0


if __name__ == "__main__":  # pragma: no cover - module entry stub
    raise SystemExit(main())
