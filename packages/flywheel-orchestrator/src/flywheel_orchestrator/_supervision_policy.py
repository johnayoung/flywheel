"""Windowed crash-loop budget: the shared supervision policy core (spec 00070).

Every long-running flywheel daemon -- the autopilot intake loop, a git-worktree
worker, a worker-pool slot -- can die. This module owns the one shared decision
those three supervisors make on a death: *respawn it, or declare it exhausted?*
It answers with a rolling-window crash-loop budget rather than a lifetime
counter, which is the whole point: a transient blip inside a healthy fleet
recovers, but a persistent boot-loop is contained before it can crash-loop
(for the autopilot worker, that means unbounded unattended writes to the
operator's base branch -- so the budget is a safety interlock, not a nicety).

Two objects, deliberately split:

* :class:`SupervisionBudget` -- the *parameter shape* (the shared invariant):
  ``max_respawns`` deaths tolerated inside a rolling ``window_seconds``. This is
  the surface the autopilot supervisor, the worker supervisor, and the worker
  pool all construct from config, so all three enforce one policy. A
  ``max_respawns`` of 0 disables respawn entirely -- the operator's
  unattended-base-branch override (``budget.disabled`` reads ``True``).
* :class:`SupervisionPolicy` -- the stateful decider. It records death instants
  against an *injected* clock (a ``now`` callable defaulting to
  :func:`time.monotonic`, never wall-clock in the decision path so tests drive
  time deterministically) and returns a :class:`RespawnDecision` per death.

Windowing rule (deterministic at the edge): a prior death is still counted
while it is *strictly newer* than ``now - window_seconds``; a death exactly on
the far edge (``now - t == window_seconds``) has decayed out and no longer
counts. So after the window has fully elapsed since the earliest counted death,
the budget is treated as replenished -- a fresh death after a long healthy
interval is respawned rather than charged against an old burst.

Boundary of the budget itself (must not be off-by-one): with
``max_respawns == N``, the Nth death inside the window is still a ``RESPAWN``;
the (N+1)th is the first ``EXHAUSTED``. Budget 0 makes the very first death
``EXHAUSTED``.

This is pure counting -- it knows nothing of tasks, leases, subprocesses, or how
a supervisor acts on the decision. Wiring it into the supervisors and the pool
is separate, dependent work.
"""

from __future__ import annotations

import enum
import time
from collections import deque
from dataclasses import dataclass
from typing import Callable


class RespawnDecision(enum.Enum):
    """What a supervisor should do about a death it just observed.

    ``RESPAWN`` -- the death is inside budget; launch a fresh child.
    ``EXHAUSTED`` -- the crash-loop budget is spent for the current window;
    stop respawning and surface the loud, queryable DEAD-after-budget state.
    """

    RESPAWN = "respawn"
    EXHAUSTED = "exhausted"


@dataclass(frozen=True, kw_only=True)
class SupervisionBudget:
    """The shared crash-loop budget parameter shape.

    Tolerate at most ``max_respawns`` deaths within any rolling window of
    ``window_seconds`` before declaring the daemon exhausted. This is the single
    surface the autopilot supervisor, worker supervisor, and worker pool all
    construct, so the three sites enforce one policy rather than three.

    ``max_respawns == 0`` is the explicit disable: the very first death reports
    ``EXHAUSTED`` and nothing is ever respawned (the unattended-base-branch
    safety override). ``window_seconds`` must be positive even when disabled --
    it is nominal in that case since no death is ever tolerated.
    """

    max_respawns: int
    window_seconds: float

    def __post_init__(self) -> None:
        if self.max_respawns < 0:
            raise ValueError(
                f"max_respawns must be >= 0, got {self.max_respawns}"
            )
        if self.window_seconds <= 0:
            raise ValueError(
                f"window_seconds must be > 0, got {self.window_seconds}"
            )

    @property
    def disabled(self) -> bool:
        """Whether respawn is turned off entirely (budget 0)."""
        return self.max_respawns == 0


class SupervisionPolicy:
    """Decide respawn-vs-exhausted for a daemon's deaths under a windowed budget.

    Holds the death history for one supervised entity and an injected clock.
    Call :meth:`record_death` once per observed death; it returns the
    :class:`RespawnDecision` the supervisor acts on. The clock is injected (a
    ``now`` callable, default :func:`time.monotonic`) so the window's decay is
    driven deterministically in tests -- the decision path never reads
    wall-clock directly.

    One policy instance tracks one entity (one daemon, or one pool slot); a
    pool constructs one per slot so slots decay independently.
    """

    def __init__(
        self,
        budget: SupervisionBudget,
        *,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._budget = budget
        self._clock = clock
        # Death instants inside the current window, oldest first. Pruned from
        # the left on each death as the window slides forward.
        self._deaths: deque[float] = deque()

    @property
    def budget(self) -> SupervisionBudget:
        """The budget this policy enforces."""
        return self._budget

    @property
    def deaths_in_window(self) -> int:
        """Deaths currently counted in the window (as of the last record).

        Reflects state after the most recent :meth:`record_death`; it does not
        re-read the clock, so a long idle gap since the last death is not
        decayed until the next death arrives.
        """
        return len(self._deaths)

    def record_death(self) -> RespawnDecision:
        """Record one death at the current clock instant and decide.

        Decays any prior deaths that have fallen out of the rolling window
        (strictly older than ``now - window_seconds``), counts this death, and
        returns ``RESPAWN`` while the count is within ``max_respawns`` or
        ``EXHAUSTED`` once it exceeds it. Budget 0 exhausts on the first death.
        """
        now = self._clock()
        cutoff = now - self._budget.window_seconds
        # A death exactly on the far edge (t == cutoff) has decayed out; only
        # deaths strictly newer than the cutoff remain counted.
        while self._deaths and self._deaths[0] <= cutoff:
            self._deaths.popleft()
        self._deaths.append(now)
        if len(self._deaths) <= self._budget.max_respawns:
            return RespawnDecision.RESPAWN
        return RespawnDecision.EXHAUSTED

    def reset(self) -> None:
        """Forget all recorded deaths (a clean supervised restart)."""
        self._deaths.clear()


__all__ = [
    "RespawnDecision",
    "SupervisionBudget",
    "SupervisionPolicy",
]
