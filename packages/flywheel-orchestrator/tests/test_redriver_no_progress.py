"""The bounded no-progress back-off re-driver (spec 00069, criteria #9/#13; D-C).

A "unit" the loop keeps re-attempting with nothing to show for it -- a phase
whose verify never passes, an autopilot repo that never authors a task -- is
otherwise driven every cycle forever, burning agent cost without progress. This
re-driver bounds that dead-end:

* #9 -- after a fixed bound of consecutive cycles that make NO observable
  progress on a unit, the unit is backed off (absent from the active set the next
  cycle) and routed ONCE to the single human-review queue with the
  machine-readable ``no-progress`` reason, instead of being re-attempted.
* #13 -- the path is bounded: exactly ``bound`` no-progress witnesses then
  exactly one queue entry; a never-progressing unit produces neither an infinite
  re-attempt spin nor a growing pile of queue entries.
* the reset edge case -- a unit that makes observable progress resets its
  no-progress streak and is never backed off; progress mid-streak restarts the
  count.

The direct-unit cases drive ``redrive_no_progress`` with a frozen clock over an
``SqliteClaimStore``; the integration cases drive the real autopilot back-off
seam (``apply_no_progress_backoff``) over genuine ``AutopilotPassResult`` values
so the progress signal (a task authored, a queue at target) is real. Nothing
about lifecycle state is forged -- the re-driver only appends ledger rows
(criterion #14).
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

from flywheel_orchestrator import (
    DEFAULT_NO_PROGRESS_BOUND,
    REASON_NO_PROGRESS,
    NoProgressObservation,
    SqliteClaimStore,
    redrive_no_progress,
)
from flywheel_orchestrator._autopilot import AutopilotPassResult
from flywheel_orchestrator._autopilot_run import apply_no_progress_backoff
from flywheel_orchestrator._claims import STOP_NO_PROGRESS, STOP_NO_PROGRESS_RESET

_BASE = datetime(2026, 7, 1, 12, 0, 0, tzinfo=timezone.utc)


# --- helpers ----------------------------------------------------------------


def _frozen(at: datetime):
    def _now() -> datetime:
        return at

    return _now


def _obs(unit_id: str, *, progressed: bool) -> NoProgressObservation:
    return NoProgressObservation(unit_id=unit_id, progressed=progressed)


def _witnesses(claims: SqliteClaimStore, unit_id: str) -> list:
    return [
        e
        for e in claims.list_subject_stop_events(unit_id)
        if e.kind == STOP_NO_PROGRESS
    ]


def _resets(claims: SqliteClaimStore, unit_id: str) -> list:
    return [
        e
        for e in claims.list_subject_stop_events(unit_id)
        if e.kind == STOP_NO_PROGRESS_RESET
    ]


def _drive(
    claims: SqliteClaimStore, unit_id: str, *, progressed: bool, bound: int
):
    outcomes = redrive_no_progress(
        claims,
        observations=[_obs(unit_id, progressed=progressed)],
        bound=bound,
        now=_frozen(_BASE),
    )
    assert len(outcomes) == 1
    return outcomes[0]


# --- #9/#13: bounded back-off (direct unit) ---------------------------------


def test_below_bound_waits_and_never_queues(tmp_path: Path) -> None:
    """While a unit makes no progress but the bound is not yet reached, the
    re-driver records a witness and waits -- it never backs the unit off and
    never queues (criterion #9, the still-trying state)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        for cycle in range(2):
            outcome = _drive(claims, "U", progressed=False, bound=3)
            assert outcome.result == "waiting"
            assert outcome.unit_id == "U"
            assert outcome.cycles == cycle + 1
        # Two fruitless cycles under bound 3: two witnesses, no queue entry.
        assert len(_witnesses(claims, "U")) == 2
        assert claims.list_human_review_queue() == []
    finally:
        claims.close()


def test_no_progress_past_bound_backs_off_and_queues_once(
    tmp_path: Path,
) -> None:
    """When a unit makes no progress through the bound, it is backed off and
    routed to the human-review queue with the machine-readable ``no-progress``
    reason naming the unit (criterion #9)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        results = [
            _drive(claims, "U", progressed=False, bound=3).result
            for _ in range(3)
        ]
        assert results == ["waiting", "waiting", "queued"]

        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        entry = queue[0]
        assert entry.reason == REASON_NO_PROGRESS
        assert entry.task_id == "U"
        # A machine-readable reason plus a detail naming the backed-off unit.
        assert "'U'" in entry.detail
    finally:
        claims.close()


def test_bounded_exactly_one_queue_entry_no_infinite_spin(
    tmp_path: Path,
) -> None:
    """A unit that never progresses costs exactly ``bound`` witnesses and exactly
    one queue entry -- the terminal guard stops further witnessing and re-queuing,
    so there is no infinite re-attempt spin and no re-queue every cycle (criteria
    #9/#13)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        bound = 2
        # Pump well past the bound.
        for _ in range(6):
            _drive(claims, "U", progressed=False, bound=bound)
        # Exactly ``bound`` witnesses -- no further witnessing after back-off.
        assert len(_witnesses(claims, "U")) == bound
        # Exactly one queue entry -- never re-queued.
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_NO_PROGRESS
        # An already-backed-off unit reports its terminal state, not a fresh
        # route -- and even a progress observation does not un-queue it.
        terminal = _drive(claims, "U", progressed=False, bound=bound)
        assert terminal.result == "queued"
        assert _drive(claims, "U", progressed=True, bound=bound).result == (
            "queued"
        )
        assert len(claims.list_human_review_queue()) == 1
        assert len(_witnesses(claims, "U")) == bound
    finally:
        claims.close()


def test_progress_resets_the_streak_and_never_backs_off(
    tmp_path: Path,
) -> None:
    """A unit that makes progress resets its no-progress streak and is never
    backed off -- even across many more no-progress cycles than the bound, as
    long as a progress cycle keeps interrupting the run before it reaches the
    bound (the reset edge case)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        bound = 3
        # Repeatedly: two fruitless cycles, then one that makes progress. The
        # streak never reaches 3, so the unit is never backed off.
        for _ in range(4):
            assert _drive(claims, "U", progressed=False, bound=bound).result == (
                "waiting"
            )
            assert _drive(claims, "U", progressed=False, bound=bound).result == (
                "waiting"
            )
            progressed = _drive(claims, "U", progressed=True, bound=bound)
            assert progressed.result == "progressed"
            assert progressed.cycles == 0
        assert claims.list_human_review_queue() == []
        # Each progress cycle after a live streak left a reset delimiter.
        assert len(_resets(claims, "U")) == 4
    finally:
        claims.close()


def test_progress_delimits_a_fresh_streak_before_the_bound(
    tmp_path: Path,
) -> None:
    """Progress mid-streak restarts the count from zero: two fruitless cycles,
    a progress cycle, then two more fruitless cycles is a streak of two -- below
    bound 3 -- so the unit still is not backed off even though four fruitless
    cycles have now occurred in total."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        bound = 3
        _drive(claims, "U", progressed=False, bound=bound)
        _drive(claims, "U", progressed=False, bound=bound)
        _drive(claims, "U", progressed=True, bound=bound)
        first = _drive(claims, "U", progressed=False, bound=bound)
        second = _drive(claims, "U", progressed=False, bound=bound)
        assert first.cycles == 1
        assert second.cycles == 2
        assert second.result == "waiting"
        assert claims.list_human_review_queue() == []
        # The third fruitless cycle of the fresh streak crosses the bound.
        third = _drive(claims, "U", progressed=False, bound=bound)
        assert third.result == "queued"
        assert len(claims.list_human_review_queue()) == 1
    finally:
        claims.close()


def test_perpetually_progressing_unit_never_witnesses_or_queues(
    tmp_path: Path,
) -> None:
    """A unit that always makes progress records no witnesses, no reset markers
    (there is never a live streak to reset), and is never queued -- the ledger
    does not grow for a healthy unit."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        for _ in range(5):
            outcome = _drive(claims, "U", progressed=True, bound=3)
            assert outcome.result == "progressed"
            assert outcome.cycles == 0
        assert claims.list_subject_stop_events("U") == []
        assert claims.list_human_review_queue() == []
    finally:
        claims.close()


def test_distinct_units_are_bounded_independently(tmp_path: Path) -> None:
    """Two distinct units count their streaks and back off on their own ledgers:
    a stuck unit is queued while a healthy one, observed the same passes, is
    never touched."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        bound = 2
        for _ in range(bound):
            outcomes = redrive_no_progress(
                claims,
                observations=[
                    _obs("stuck", progressed=False),
                    _obs("healthy", progressed=True),
                ],
                bound=bound,
                now=_frozen(_BASE),
            )
            assert [o.unit_id for o in outcomes] == ["stuck", "healthy"]
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].task_id == "stuck"
        assert queue[0].reason == REASON_NO_PROGRESS
        assert _witnesses(claims, "healthy") == []
    finally:
        claims.close()


def test_default_bound_is_finite(tmp_path: Path) -> None:
    """The default bound is a small finite integer, so a never-progressing unit
    reaches a human under the default policy rather than spinning forever
    (criterion #13)."""
    assert isinstance(DEFAULT_NO_PROGRESS_BOUND, int)
    assert DEFAULT_NO_PROGRESS_BOUND >= 1
    claims = SqliteClaimStore(tmp_path / "c.db")
    try:
        for _ in range(DEFAULT_NO_PROGRESS_BOUND + 3):
            redrive_no_progress(
                claims,
                observations=[_obs("U", progressed=False)],
                now=_frozen(_BASE),
            )
        assert len(_witnesses(claims, "U")) == DEFAULT_NO_PROGRESS_BOUND
        assert len(claims.list_human_review_queue()) == 1
    finally:
        claims.close()


# --- integration: the real autopilot back-off seam --------------------------


def _no_op_pass() -> AutopilotPassResult:
    # Below target and authored nothing this cycle: a genuine no-progress cycle.
    return AutopilotPassResult(
        queue_depth_before=0,
        target_depth=5,
        reason="no actionable findings this cycle; idling without emitting",
    )


def _at_target_pass() -> AutopilotPassResult:
    # Queue already at target: healthy backpressure, not a dead-end -- progress.
    return AutopilotPassResult(
        queue_depth_before=5,
        target_depth=5,
        reason="queue depth 5 already at or above target 5; nothing emitted",
    )


def _productive_pass() -> AutopilotPassResult:
    # A task authored this cycle: progress.
    return AutopilotPassResult(
        emitted_paths=(Path("emitted/task-1.json"),),
        queue_depth_before=0,
        target_depth=5,
        reason="authored 1 task toward target 5",
    )


def test_autopilot_repo_backed_off_after_bound_of_idle_cycles(
    tmp_path: Path,
) -> None:
    """The real autopilot back-off seam: a repo that authors nothing while below
    target for ``bound`` consecutive cycles is backed off and routed to the
    human-review queue with ``no-progress`` (criterion #9, the autopilot unit)."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    unit = str(tmp_path / "tasks")
    try:
        results = [
            apply_no_progress_backoff(
                _no_op_pass(),
                claims=claims,
                unit_id=unit,
                bound=3,
                now=_frozen(_BASE),
            ).result
            for _ in range(3)
        ]
        assert results == ["waiting", "waiting", "queued"]
        queue = claims.list_human_review_queue()
        assert len(queue) == 1
        assert queue[0].reason == REASON_NO_PROGRESS
        assert queue[0].task_id == unit
    finally:
        claims.close()


def test_autopilot_at_target_cycle_counts_as_progress(tmp_path: Path) -> None:
    """A queue-at-target cycle is healthy backpressure, not a dead-end: it counts
    as progress, so a repo idling at target is never backed off no matter how many
    such cycles pass."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    unit = str(tmp_path / "tasks")
    try:
        for _ in range(6):
            outcome = apply_no_progress_backoff(
                _at_target_pass(),
                claims=claims,
                unit_id=unit,
                bound=3,
                now=_frozen(_BASE),
            )
            assert outcome.result == "progressed"
        assert claims.list_human_review_queue() == []
        assert claims.list_subject_stop_events(unit) == []
    finally:
        claims.close()


def test_autopilot_productive_cycle_resets_the_streak(tmp_path: Path) -> None:
    """A cycle that authors a task resets the streak, so a repo that keeps
    producing work is never backed off even with idle cycles interleaved."""
    claims = SqliteClaimStore(tmp_path / "c.db")
    unit = str(tmp_path / "tasks")
    try:
        bound = 3
        for _ in range(4):
            apply_no_progress_backoff(
                _no_op_pass(),
                claims=claims,
                unit_id=unit,
                bound=bound,
                now=_frozen(_BASE),
            )
            apply_no_progress_backoff(
                _no_op_pass(),
                claims=claims,
                unit_id=unit,
                bound=bound,
                now=_frozen(_BASE),
            )
            reset = apply_no_progress_backoff(
                _productive_pass(),
                claims=claims,
                unit_id=unit,
                bound=bound,
                now=_frozen(_BASE),
            )
            assert reset.result == "progressed"
        assert claims.list_human_review_queue() == []
    finally:
        claims.close()
