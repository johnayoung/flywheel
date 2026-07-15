"""The ``flywheel triage`` daemon loop (spec 00082, triage-verb-daemon).

The neverending label-polling loop mirrors the autopilot daemon: it runs one
triage pass per cycle, waits the interval, and MUST NOT terminate on an idle
(nothing-to-triage) cycle -- it stops only on an explicit stop signal or the
circuit breaker giving up. Every collaborator is injected so the loop runs with
no wall-clock waits and no live model or ``gh``.
"""

from __future__ import annotations

from flywheel_orchestrator._triage import TriagePassResult
from flywheel_orchestrator._triage_run import (
    MAX_CONSECUTIVE_CYCLE_FAILURES,
    run_daemon_loop,
)


def _idle() -> TriagePassResult:
    """A pass over a settled board: no outcomes, nothing written."""
    return TriagePassResult()


def _noop_sleep(_seconds: float, _should_stop: object) -> None:
    """Injected sleep: no real wall-clock wait in tests."""


def test_idle_cycle_does_not_stop_the_daemon() -> None:
    """An idle pass writes nothing and the loop schedules the next cycle."""
    calls = {"n": 0}

    def run_cycle() -> TriagePassResult:
        calls["n"] += 1
        return _idle()

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=lambda: False,
        sleep=_noop_sleep,
        max_cycles=3,
    )

    # The loop ran three back-to-back idle cycles without terminating; only the
    # test-only max_cycles bound stopped it, never the empty result.
    assert cycles == 3
    assert calls["n"] == 3


def test_loops_until_the_stop_signal() -> None:
    """The daemon keeps cycling until ``should_stop`` flips true."""
    calls = {"n": 0}
    stop_after = 4

    def run_cycle() -> TriagePassResult:
        calls["n"] += 1
        return _idle()

    def should_stop() -> bool:
        # Flip after the fourth cycle has run: the loop re-checks should_stop
        # right after on_cycle, so it exits without a fifth pass.
        return calls["n"] >= stop_after

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=should_stop,
        sleep=_noop_sleep,
    )

    assert cycles == stop_after
    assert calls["n"] == stop_after


def test_stop_before_the_first_cycle_runs_nothing() -> None:
    """A stop signal already set at entry runs zero cycles."""
    calls = {"n": 0}

    def run_cycle() -> TriagePassResult:  # pragma: no cover - must not run
        calls["n"] += 1
        return _idle()

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=lambda: True,
        sleep=_noop_sleep,
    )

    assert cycles == 0
    assert calls["n"] == 0


def test_before_cycle_runs_each_iteration() -> None:
    """``before_cycle`` (signal re-arm in production) fires every cycle."""
    armed = {"n": 0}

    def before_cycle() -> None:
        armed["n"] += 1

    run_daemon_loop(
        run_cycle=_idle,
        interval_seconds=0.0,
        should_stop=lambda: False,
        sleep=_noop_sleep,
        before_cycle=before_cycle,
        max_cycles=2,
    )

    assert armed["n"] == 2


def test_raising_cycle_backs_off_then_gives_up() -> None:
    """A persistently raising pass is bounded by the circuit breaker.

    Each raising cycle is counted and backed off (via the injected sleep) so
    the loop runs further cycles; after ``MAX_CONSECUTIVE_CYCLE_FAILURES`` it
    stops and surfaces the give-up signal rather than hot-looping or exiting
    silently.
    """
    failures: list[int] = []
    gave_up: list[int] = []
    backoffs = {"n": 0}

    def run_cycle() -> TriagePassResult:
        raise RuntimeError("gh unreachable")

    def on_cycle_failure(_exc: BaseException, consecutive: int) -> None:
        failures.append(consecutive)

    def on_give_up(consecutive: int) -> None:
        gave_up.append(consecutive)

    def counting_sleep(_seconds: float, _should_stop: object) -> None:
        backoffs["n"] += 1

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=lambda: False,
        sleep=counting_sleep,
        on_cycle_failure=on_cycle_failure,
        on_give_up=on_give_up,
    )

    assert cycles == MAX_CONSECUTIVE_CYCLE_FAILURES
    assert failures == list(range(1, MAX_CONSECUTIVE_CYCLE_FAILURES + 1))
    assert gave_up == [MAX_CONSECUTIVE_CYCLE_FAILURES]
    # Backoff between strikes, but not after the final give-up strike.
    assert backoffs["n"] == MAX_CONSECUTIVE_CYCLE_FAILURES - 1


def test_a_success_resets_the_failure_streak() -> None:
    """One good cycle clears the consecutive-failure count."""
    outcomes = iter([RuntimeError("blip"), None, RuntimeError("blip")])
    failures: list[int] = []

    def run_cycle() -> TriagePassResult:
        item = next(outcomes)
        if isinstance(item, BaseException):
            raise item
        return _idle()

    def on_cycle_failure(_exc: BaseException, consecutive: int) -> None:
        failures.append(consecutive)

    run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=0.0,
        should_stop=lambda: False,
        sleep=_noop_sleep,
        on_cycle_failure=on_cycle_failure,
        max_cycles=3,
    )

    # Fail (1), succeed (reset), fail (1 again) -- never 2.
    assert failures == [1, 1]
