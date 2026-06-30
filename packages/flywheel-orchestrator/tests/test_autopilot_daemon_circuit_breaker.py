"""Circuit-breaker coverage for the autopilot daemon loop.

The daemon's ``run_daemon_loop`` wraps each per-cycle ``run_cycle`` in a
consecutive-failure breaker mirroring the worker daemon
(:mod:`flywheel_worktree.worker`): a raising cycle is counted and backed off so
the loop runs a further cycle, a subsequent success resets the count, and on a
bounded consecutive-failure count the loop stops by surfacing a visible give-up
signal (turned into a non-zero exit) rather than exiting silently.

Every collaborator is injected, so these run with no wall-clock waits and no
live model.
"""

from __future__ import annotations

from flywheel_orchestrator._autopilot import AutopilotPassResult
from flywheel_orchestrator._autopilot_run import (
    CYCLE_FAILURE_BACKOFF_SECONDS,
    MAX_CONSECUTIVE_CYCLE_FAILURES,
    run_daemon_loop,
)


def test_one_failing_cycle_is_contained_then_a_further_cycle_runs() -> None:
    """A single raising cycle is counted, backed off, and the loop runs on.

    Cycle 1 raises; the breaker contains it and the loop runs a *further*
    cycle that succeeds. The success resets the consecutive count, so the loop
    never gives up and returns normally. Catching-then-breaking (giving up on a
    single failure) must fail this test.
    """
    calls = {"n": 0}
    gave_up: list[int] = []
    failures: list[tuple[str, int]] = []

    def run_cycle() -> AutopilotPassResult:
        calls["n"] += 1
        if calls["n"] == 1:
            raise RuntimeError("transient cycle failure")
        return AutopilotPassResult(reason="recovered")

    stop_state = {"stop": False}

    def should_stop() -> bool:
        return stop_state["stop"]

    def fake_sleep(seconds: float, cb) -> None:
        # Stop after the first *successful* cycle so the loop ends cleanly,
        # never spinning on the injected sleep.
        if calls["n"] >= 2:
            stop_state["stop"] = True

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=should_stop,
        sleep=fake_sleep,
        on_cycle_failure=lambda exc, n: failures.append((type(exc).__name__, n)),
        on_give_up=lambda n: gave_up.append(n),
        max_cycles=50,
    )

    # A cycle ran strictly AFTER the failing one.
    assert calls["n"] >= 2
    # The breaker never gave up: a success followed the single failure.
    assert gave_up == []
    # Exactly one failure was observed and counted as the first consecutive one.
    assert failures == [("RuntimeError", 1)]
    assert cycles == 2


def test_every_cycle_failing_stops_bounded_and_surfaces_give_up() -> None:
    """All cycles raise: the loop terminates at the bounded count and gives up.

    The loop must not retry unboundedly. After exactly
    ``MAX_CONSECUTIVE_CYCLE_FAILURES`` failures it stops and surfaces a give-up
    signal carrying the consecutive-failure count. An unbounded retry, or a
    give-up that never fires, must fail this test.
    """
    calls = {"n": 0}
    gave_up: list[int] = []
    backoffs: list[float] = []

    def run_cycle() -> AutopilotPassResult:
        calls["n"] += 1
        raise RuntimeError("always fails")

    def fake_sleep(seconds: float, cb) -> None:
        backoffs.append(seconds)

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=lambda: False,
        sleep=fake_sleep,
        on_give_up=lambda n: gave_up.append(n),
        # Generous bound: the breaker, not max_cycles, must stop the loop.
        max_cycles=10_000,
    )

    # Bounded: exactly the breaker's threshold of cycles were attempted.
    assert calls["n"] == MAX_CONSECUTIVE_CYCLE_FAILURES
    assert cycles == MAX_CONSECUTIVE_CYCLE_FAILURES
    # Give-up fired once, reporting the consecutive-failure count.
    assert gave_up == [MAX_CONSECUTIVE_CYCLE_FAILURES]
    # Backoff happened between failed cycles, never after the final give-up.
    assert backoffs == [CYCLE_FAILURE_BACKOFF_SECONDS] * (
        MAX_CONSECUTIVE_CYCLE_FAILURES - 1
    )


def test_main_returns_non_zero_when_daemon_gives_up(monkeypatch) -> None:
    """``main`` surfaces a give-up as a non-zero exit, never a silent exit 0.

    A supervisor must be able to tell a crash-loop give-up from a clean stop.
    """
    import flywheel_orchestrator._autopilot_run as mod

    monkeypatch.setattr(mod, "_repo_root", lambda: __import__("pathlib").Path("/"))
    monkeypatch.setattr(mod, "load_effective_policy", lambda: None)
    monkeypatch.setattr(mod, "_build_queue_depth", lambda *a, **k: None)

    def boom() -> AutopilotPassResult:
        raise RuntimeError("cycle blew up")

    monkeypatch.setattr(mod, "run_single_pass", lambda **kwargs: boom())

    # No wall-clock waits: the breaker's backoff resolves instantly.
    monkeypatch.setattr(mod, "_interruptible_sleep", lambda seconds, cb: None)

    exit_code = mod.main(["--interval", "0", "--activity-file", "/dev/null"])
    assert exit_code == 1


def test_consecutive_count_resets_across_an_intervening_success() -> None:
    """Failures separated by a success never trip the breaker.

    fail, success, fail, success, ... must not accumulate toward the give-up
    bound -- the consecutive count resets on every successful cycle.
    """
    calls = {"n": 0}
    gave_up: list[int] = []

    def run_cycle() -> AutopilotPassResult:
        calls["n"] += 1
        if calls["n"] % 2 == 1:
            raise RuntimeError("odd cycle fails")
        return AutopilotPassResult(reason="even ok")

    stop_state = {"stop": False}

    def fake_sleep(seconds: float, cb) -> None:
        # Run well past the give-up threshold worth of alternating cycles.
        if calls["n"] >= 2 * MAX_CONSECUTIVE_CYCLE_FAILURES + 2:
            stop_state["stop"] = True

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=lambda: stop_state["stop"],
        sleep=fake_sleep,
        on_give_up=lambda n: gave_up.append(n),
        max_cycles=10_000,
    )

    assert gave_up == []
    assert cycles >= 2 * MAX_CONSECUTIVE_CYCLE_FAILURES
