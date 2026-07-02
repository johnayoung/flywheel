"""Tests for the shared supervision policy core (spec 00070, policy-core layer).

Grades the windowed crash-loop budget in isolation -- the decide-respawn /
decide-exhausted behavior every supervisor and the worker pool will build on --
against an injected clock, so the window's decay is exercised deterministically
rather than against wall-clock. The supervisor-adoption cases at the bottom of
this module then pin the worker and autopilot supervisors' *use* of that policy
end-to-end -- real short-lived children, real respawns (a genuinely new pid, not
a relabeled DEAD string) -- while wiring into the worker pool remains separate,
dependent work.

Covers the held-out criteria that lower to this layer:

* #3 (``window_decays``) -- after the window elapses since the earliest counted
  death, the budget is treated as replenished, so a lifetime counter fails and
  only a genuinely windowed one passes.
* #7 (``budget_zero_no_respawn``) -- budget 0 never respawns, even on the first
  death (the unattended-base-branch safety override).

plus the off-by-one boundary of the budget, the window edge, and construction
validation.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from flywheel._autopilot_supervisor import AutopilotState, AutopilotSupervisor
from flywheel._worker_supervisor import WorkerState, WorkerSupervisor
from flywheel_orchestrator import (
    RespawnDecision,
    SupervisionBudget,
    SupervisionPolicy,
)


class _FakeClock:
    """A deterministic monotonic source the tests advance by hand.

    Stands in for ``time.monotonic`` so the window's decay is driven by explicit
    ``advance`` calls, never real elapsed time.
    """

    def __init__(self, start: float = 0.0) -> None:
        self.now = start

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def _policy(
    *, max_respawns: int, window_seconds: float, clock: _FakeClock
) -> SupervisionPolicy:
    budget = SupervisionBudget(
        max_respawns=max_respawns, window_seconds=window_seconds
    )
    return SupervisionPolicy(budget, clock=clock)


# ----- criterion #3: the window decays (a lifetime counter fails this) --------


def test_window_decays_replenishes_budget() -> None:
    """After the window fully elapses since the earliest death, the budget is
    replenished: a fresh death is respawned, not charged against the old burst.

    A non-decaying lifetime counter (today's ``MAX_POOL_RESTARTS_PER_SLOT``)
    would have already reported EXHAUSTED here and fails this test.
    """
    clock = _FakeClock()
    policy = _policy(max_respawns=3, window_seconds=100.0, clock=clock)

    # Spend the whole budget inside one window: 3 deaths, each respawned.
    for _ in range(3):
        clock.advance(1.0)
        assert policy.record_death() is RespawnDecision.RESPAWN
    assert policy.deaths_in_window == 3

    # Let the window fully elapse past every counted death, then die once more.
    clock.advance(100.0)
    assert policy.record_death() is RespawnDecision.RESPAWN
    # Only the fresh death remains in the window -- the burst has decayed out.
    assert policy.deaths_in_window == 1


def test_window_decays_only_prunes_deaths_past_the_edge() -> None:
    """Partial decay: a death still inside the window keeps counting while an
    older one that has crossed the edge is dropped, so the budget replenishes
    incrementally rather than all-or-nothing."""
    clock = _FakeClock()
    policy = _policy(max_respawns=2, window_seconds=10.0, clock=clock)

    policy.record_death()  # t=0
    clock.advance(6.0)
    policy.record_death()  # t=6
    assert policy.deaths_in_window == 2

    # Advance to t=11: the t=0 death is now 11s old (> window) and decays; the
    # t=6 death is 5s old and stays. The new death fits within budget.
    clock.advance(5.0)
    assert policy.record_death() is RespawnDecision.RESPAWN
    assert policy.deaths_in_window == 2  # t=6 and t=11, not t=0


# ----- criterion #7: budget 0 disables respawn entirely -----------------------


def test_budget_zero_no_respawn() -> None:
    """Budget 0 reports EXHAUSTED on the very first death and never respawns --
    the operator's explicit unattended-base-branch override."""
    clock = _FakeClock()
    policy = _policy(max_respawns=0, window_seconds=100.0, clock=clock)

    assert policy.budget.disabled is True
    assert policy.record_death() is RespawnDecision.EXHAUSTED

    # Still exhausted on subsequent deaths, even far apart in time -- disabled
    # means disabled, decay never re-enables it.
    clock.advance(1000.0)
    assert policy.record_death() is RespawnDecision.EXHAUSTED


# ----- the budget boundary must not be off-by-one -----------------------------


def test_nth_death_respawns_and_n_plus_one_exhausts() -> None:
    """With ``max_respawns == N`` inside one window, deaths 1..N are respawned
    and the (N+1)th is the first EXHAUSTED -- forecloses both a constant
    'always respawn' and a constant 'always exhausted' implementation."""
    clock = _FakeClock()
    policy = _policy(max_respawns=3, window_seconds=100.0, clock=clock)

    for i in range(3):
        clock.advance(1.0)
        assert policy.record_death() is RespawnDecision.RESPAWN, f"death {i + 1}"

    clock.advance(1.0)
    assert policy.record_death() is RespawnDecision.EXHAUSTED

    # And it stays exhausted while the window has not decayed.
    clock.advance(1.0)
    assert policy.record_death() is RespawnDecision.EXHAUSTED


def test_single_death_within_window_respawns() -> None:
    """A lone death inside the window is always respawned (budget >= 1)."""
    clock = _FakeClock()
    policy = _policy(max_respawns=1, window_seconds=30.0, clock=clock)
    assert policy.record_death() is RespawnDecision.RESPAWN


# ----- the window edge is deterministic (which side does it fall on?) ---------


def test_death_exactly_on_window_edge_has_decayed() -> None:
    """A prior death exactly ``window_seconds`` ago has decayed out (the far
    edge is open): it no longer counts, so a death on the edge is respawned."""
    clock = _FakeClock()
    policy = _policy(max_respawns=1, window_seconds=10.0, clock=clock)

    assert policy.record_death() is RespawnDecision.RESPAWN  # t=0
    clock.advance(10.0)  # exactly one window later
    # The t=0 death is on the far edge and decays, leaving only this one.
    assert policy.record_death() is RespawnDecision.RESPAWN
    assert policy.deaths_in_window == 1


def test_death_just_inside_window_edge_still_counts() -> None:
    """A prior death just under ``window_seconds`` ago still counts, so a second
    death inside the same window exhausts a budget of 1 -- the complement of the
    edge case above, pinning the boundary to one deterministic side."""
    clock = _FakeClock()
    policy = _policy(max_respawns=1, window_seconds=10.0, clock=clock)

    assert policy.record_death() is RespawnDecision.RESPAWN  # t=0
    clock.advance(9.999)  # just inside the window
    assert policy.record_death() is RespawnDecision.EXHAUSTED
    assert policy.deaths_in_window == 2


# ----- reset and construction validation --------------------------------------


def test_reset_clears_death_history() -> None:
    """``reset`` forgets prior deaths so a clean supervised restart begins with a
    full budget again."""
    clock = _FakeClock()
    policy = _policy(max_respawns=1, window_seconds=10.0, clock=clock)

    policy.record_death()
    policy.record_death()
    assert policy.deaths_in_window == 2

    policy.reset()
    assert policy.deaths_in_window == 0
    assert policy.record_death() is RespawnDecision.RESPAWN


def test_default_clock_is_monotonic_not_wallclock() -> None:
    """Constructed without an explicit clock, the policy still decides -- it
    defaults to a monotonic source rather than requiring a clock at every site
    and rather than reading wall-clock in the decision path."""
    policy = SupervisionPolicy(
        SupervisionBudget(max_respawns=1, window_seconds=10.0)
    )
    assert policy.record_death() is RespawnDecision.RESPAWN


@pytest.mark.parametrize("bad_respawns", [-1, -5])
def test_negative_budget_rejected(bad_respawns: int) -> None:
    with pytest.raises(ValueError):
        SupervisionBudget(max_respawns=bad_respawns, window_seconds=10.0)


@pytest.mark.parametrize("bad_window", [0.0, -1.0])
def test_non_positive_window_rejected(bad_window: float) -> None:
    with pytest.raises(ValueError):
        SupervisionBudget(max_respawns=1, window_seconds=bad_window)


# ----- supervisor adoption: the policy governs real respawns ------------------
#
# The policy layer above is exercised in isolation; these end-to-end cases pin
# the worker/autopilot supervisors' *use* of it. They spawn real short-lived
# children so a respawn is a genuinely new OS process (a new pid), never a
# relabel of the DEAD status string, and drive the shared windowed budget with
# the same injected clock so the window never decays under them. The two
# supervisors are asserted to enforce the *identical* policy semantics.


def _dying_child_argv(code: int = 2) -> list[str]:
    """A child that exits ``code`` immediately -- an instant crash loop."""
    return [sys.executable, "-c", f"import sys; sys.exit({code})"]


def _await_child_exit(
    sup: WorkerSupervisor | AutopilotSupervisor, *, timeout: float = 5.0
) -> None:
    """Block until the supervisor's current child exits, then return.

    White-box on purpose: reaching into the supervisor's own ``Popen`` handle
    makes the wait deterministic (no wall-clock polling loop). After this
    returns, the next ``status()`` poll observes the death and runs the
    crash-loop policy for that tick.
    """
    child = sup._child
    assert child is not None
    child.wait(timeout=timeout)


def test_worker_supervisor_respawn_within_window(tmp_path: Path) -> None:
    """A death inside budget makes ``status()`` launch a REAL new child.

    No operator ``start()`` is involved: the supervise tick (the status poll)
    observes the death, charges it against the windowed budget, and -- inside
    budget -- respawns. The new pid differs from the dead one, proving a genuine
    new process rather than a relabeled DEAD status string.
    """
    clock = _FakeClock()
    policy = _policy(max_respawns=2, window_seconds=1000.0, clock=clock)
    sup = WorkerSupervisor(
        db_path=tmp_path / "db.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(),
        policy=policy,
    )
    try:
        first = sup.start()
        assert first.state == WorkerState.SUPERVISED
        assert first.pid is not None
        _await_child_exit(sup)

        respawned = sup.status()
        assert respawned.state == WorkerState.SUPERVISED
        assert respawned.pid is not None
        assert respawned.pid != first.pid  # a new OS process, not a relabel
    finally:
        sup.stop(timeout=5.0)


def test_worker_supervisor_budget_exhausted(tmp_path: Path) -> None:
    """Past the budget the supervisor stops respawning and latches the distinct
    ``DEAD_AFTER_BUDGET`` state -- and stays there across further polls until an
    operator ``start()`` re-arms it."""
    clock = _FakeClock()
    policy = _policy(max_respawns=2, window_seconds=1000.0, clock=clock)
    sup = WorkerSupervisor(
        db_path=tmp_path / "db.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(2),
        policy=policy,
    )
    try:
        status = sup.start()
        assert status.state == WorkerState.SUPERVISED
        seen_pids = {status.pid}

        # Two in-budget deaths each respawn a genuinely new child.
        for _ in range(2):
            _await_child_exit(sup)
            status = sup.status()
            assert status.state == WorkerState.SUPERVISED
            seen_pids.add(status.pid)

        # The third death is past budget: no respawn, loud terminal state.
        _await_child_exit(sup)
        dead = sup.status()
        assert dead.state == WorkerState.DEAD_AFTER_BUDGET
        assert "exit=2" in (dead.message or "")
        assert len(seen_pids) == 3  # initial + 2 respawns, all distinct pids

        # Exhausted is sticky: a later poll does not sneak in another respawn.
        assert sup.status().state == WorkerState.DEAD_AFTER_BUDGET
    finally:
        sup.stop(timeout=5.0)


def test_worker_supervisor_budget_zero_no_respawn(tmp_path: Path) -> None:
    """A disabled (budget-0) policy reproduces today's plain DEAD exactly: the
    child is not respawned and the state is DEAD, never DEAD_AFTER_BUDGET."""
    clock = _FakeClock()
    policy = _policy(max_respawns=0, window_seconds=1000.0, clock=clock)
    sup = WorkerSupervisor(
        db_path=tmp_path / "db.sqlite",
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(2),
        policy=policy,
    )
    try:
        started = sup.start()
        assert started.state == WorkerState.SUPERVISED
        _await_child_exit(sup)

        dead = sup.status()
        assert dead.state == WorkerState.DEAD  # plain DEAD, not _AFTER_BUDGET
        assert dead.pid == started.pid  # the same child, not a respawn
        assert "exit=2" in (dead.message or "")
    finally:
        sup.stop(timeout=5.0)


# ----- the autopilot supervisor enforces the identical policy -----------------


def test_autopilot_supervisor_respawn_within_window(tmp_path: Path) -> None:
    """Autopilot mirrors the worker: an in-budget death auto-respawns a real new
    child from the status poll, with no operator ``start()``."""
    clock = _FakeClock()
    policy = _policy(max_respawns=2, window_seconds=1000.0, clock=clock)
    sup = AutopilotSupervisor(
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(),
        policy=policy,
    )
    try:
        first = sup.start()
        assert first.state == AutopilotState.SUPERVISED
        assert first.pid is not None
        _await_child_exit(sup)

        respawned = sup.status()
        assert respawned.state == AutopilotState.SUPERVISED
        assert respawned.pid is not None
        assert respawned.pid != first.pid
    finally:
        sup.stop(timeout=5.0)


def test_autopilot_supervisor_budget_exhausted(tmp_path: Path) -> None:
    """Autopilot past-budget latches ``DEAD_AFTER_BUDGET`` and stops respawning,
    identical to the worker."""
    clock = _FakeClock()
    policy = _policy(max_respawns=2, window_seconds=1000.0, clock=clock)
    sup = AutopilotSupervisor(
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(2),
        policy=policy,
    )
    try:
        status = sup.start()
        assert status.state == AutopilotState.SUPERVISED
        seen_pids = {status.pid}

        for _ in range(2):
            _await_child_exit(sup)
            status = sup.status()
            assert status.state == AutopilotState.SUPERVISED
            seen_pids.add(status.pid)

        _await_child_exit(sup)
        dead = sup.status()
        assert dead.state == AutopilotState.DEAD_AFTER_BUDGET
        assert "exit=2" in (dead.message or "")
        assert len(seen_pids) == 3

        assert sup.status().state == AutopilotState.DEAD_AFTER_BUDGET
    finally:
        sup.stop(timeout=5.0)


def test_autopilot_supervisor_budget_zero_no_respawn(tmp_path: Path) -> None:
    """A disabled (budget-0) autopilot policy reproduces plain DEAD."""
    clock = _FakeClock()
    policy = _policy(max_respawns=0, window_seconds=1000.0, clock=clock)
    sup = AutopilotSupervisor(
        log_dir=tmp_path / "logs",
        spawn_argv=_dying_child_argv(2),
        policy=policy,
    )
    try:
        started = sup.start()
        assert started.state == AutopilotState.SUPERVISED
        _await_child_exit(sup)

        dead = sup.status()
        assert dead.state == AutopilotState.DEAD
        assert dead.pid == started.pid
        assert "exit=2" in (dead.message or "")
    finally:
        sup.stop(timeout=5.0)
