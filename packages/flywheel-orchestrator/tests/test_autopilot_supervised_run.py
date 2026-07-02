"""Liveness-record adoption for the supervised autopilot daemon.

A running autopilot daemon publishes a liveness record (its pid plus an
``expires_at`` freshness deadline) every time it writes its activity snapshot.
A second supervisor reads that record to decide whether a live daemon already
exists: a fresh record is *adopted* (the reader spawns no duplicate) while a
stale record -- one whose freshness deadline is at/before now -- reads as
not-live, exactly like a lapsed worker ``task_claims`` lease, so a dead daemon
is respawned instead of adopted forever.

These grade the publish + read/adopt path at the orchestrator level (the daemon
writes the record; ``read_live_activity`` reads it). The console-side
``AutopilotSupervisor.DETACHED`` wiring that consumes this is covered in the
flywheel package's ``test_autopilot_supervisor``.
"""

from __future__ import annotations

from pathlib import Path

from flywheel_orchestrator._autopilot import AutopilotPassResult
from flywheel_orchestrator._autopilot_activity import (
    PHASE_IDLE,
    AutopilotActivity,
    is_record_live,
    read_activity,
    read_live_activity,
    write_activity,
)
from flywheel_orchestrator._autopilot_run import (
    LIVENESS_GRACE_SECONDS,
    SupervisedChild,
    SupervisedOutcome,
    _ActivityRecorder,
    run_daemon_loop,
    run_supervised,
)
from flywheel_orchestrator._supervision_policy import (
    SupervisionBudget,
    SupervisionPolicy,
)

_INTERVAL = 300.0
_DAEMON_PID = 4242


def _publish_via_recorder(
    path: Path, *, pid: int = _DAEMON_PID, at: float
) -> AutopilotActivity:
    """Publish one liveness record the way the daemon does at startup.

    Drives the real ``_ActivityRecorder`` so the record under test is the one
    the daemon actually writes -- not a hand-built fixture -- pinning the clock
    so ``expires_at`` is deterministic.
    """
    recorder = _ActivityRecorder(
        path=path, interval_seconds=_INTERVAL, pid=pid, clock=lambda: at
    )
    recorder.starting()
    published = read_activity(path)
    assert published is not None
    return published


# --- the freshness deadline the daemon stamps -------------------------------


def test_liveness_adoption_record_stamps_a_future_freshness_deadline(
    tmp_path: Path,
) -> None:
    """The published record carries ``expires_at = now + interval + grace``.

    This is the freshness field a second supervisor reads; documenting it here
    pins the one-interval-plus-grace window that keeps a healthy daemon (which
    only rewrites the record each cycle) reading live between writes.
    """
    path = tmp_path / "activity.json"
    published = _publish_via_recorder(path, at=1000.0)
    assert published.expires_at == 1000.0 + _INTERVAL + LIVENESS_GRACE_SECONDS


# --- adopt a live record, don't adopt a corpse ------------------------------


def test_liveness_adoption_reads_a_fresh_record_from_a_live_daemon(
    tmp_path: Path,
) -> None:
    """A record read before its deadline reads live -> the reader adopts it."""
    path = tmp_path / "activity.json"
    published = _publish_via_recorder(path, at=1000.0)

    # now well before the freshness deadline: the daemon is live -> adopt.
    adopted = read_live_activity(path, now=1200.0)
    assert adopted is not None
    assert adopted == published
    # The record carries the daemon pid so the reader can name whom it adopted.
    assert adopted.pid == _DAEMON_PID


def test_liveness_adoption_stale_record_reads_not_live(tmp_path: Path) -> None:
    """A record read past its deadline reads not-live -> respawn, don't adopt.

    The "don't adopt a corpse forever" case: a dead daemon's leftover record
    must not keep a fresh supervisor from spawning a replacement.
    """
    path = tmp_path / "activity.json"
    _publish_via_recorder(path, at=1000.0)

    # now past the freshness deadline (1000 + 300 + 60 = 1360): not-live.
    assert read_live_activity(path, now=5000.0) is None


def test_liveness_adoption_boundary_matches_worker_lease_strictness(
    tmp_path: Path,
) -> None:
    """At exactly ``expires_at`` the record is not-live (strict ``> now``).

    Mirrors ``has_live_lease``'s ``lease_expires_at > now``: the deadline
    instant itself counts as lapsed, so the autopilot record and the worker
    lease share one staleness boundary.
    """
    path = tmp_path / "activity.json"
    published = _publish_via_recorder(path, at=1000.0)
    deadline = published.expires_at
    assert deadline is not None

    assert read_live_activity(path, now=deadline - 0.001) is not None
    assert read_live_activity(path, now=deadline) is None
    assert read_live_activity(path, now=deadline + 0.001) is None


def test_liveness_adoption_missing_record_reads_not_live(tmp_path: Path) -> None:
    """No record at all (fresh project, daemon never ran) reads not-live.

    The same "no lease, no live worker" default the worker takes for an absent
    ``task_claims`` row.
    """
    assert read_live_activity(tmp_path / "never-written.json", now=1000.0) is None
    assert read_live_activity(None, now=1000.0) is None


def test_liveness_adoption_record_without_a_deadline_reads_not_live(
    tmp_path: Path,
) -> None:
    """A record missing ``expires_at`` (an older writer) reads not-live.

    Liveness rides on the freshness deadline alone; without one there is no
    positive proof of life, so the reader must not adopt.
    """
    path = tmp_path / "activity.json"
    write_activity(
        path,
        AutopilotActivity(
            pid=_DAEMON_PID,
            phase=PHASE_IDLE,
            cycle_index=1,
            updated_at=1000.0,
            interval_seconds=_INTERVAL,
            expires_at=None,
        ),
    )
    back = read_activity(path)
    assert back is not None and back.expires_at is None
    assert read_live_activity(path, now=1000.0) is None


def test_liveness_adoption_pid_distinguishes_a_foreign_daemon(
    tmp_path: Path,
) -> None:
    """A live record's pid lets a reader tell a foreign daemon from its own.

    Mirrors the activity-pid guard: a supervisor owning child pid ``owner_pid``
    can read the record and see it belongs to a *different* daemon, so it is
    adopted as a peer rather than mistaken for the owned child.
    """
    path = tmp_path / "activity.json"
    foreign = _publish_via_recorder(path, pid=9999, at=1000.0)
    live = read_live_activity(path, now=1200.0)
    assert live is not None
    owner_pid = 4242
    assert live.pid != owner_pid
    assert foreign.pid == 9999


# --- across a real daemon run: publish while alive, lapse once dead ----------


def test_liveness_adoption_across_a_daemon_run_publishes_then_lapses(
    tmp_path: Path,
) -> None:
    """The record published during a daemon run reads live, then lapses.

    Drives the actual ``run_daemon_loop`` with the recorder hooks the way
    ``main`` wires them: while the daemon runs, a peer supervisor reading the
    record adopts it; once the daemon is gone and its last record's deadline
    passes, the same read reads not-live so the peer respawns.
    """
    path = tmp_path / "activity.json"
    recorder = _ActivityRecorder(
        path=path, interval_seconds=_INTERVAL, pid=_DAEMON_PID, clock=lambda: 100.0
    )

    def run_cycle() -> AutopilotPassResult:
        return AutopilotPassResult(reason="idle cycle")

    stop = {"n": 0}

    def should_stop() -> bool:
        return stop["n"] >= 1

    def before_cycle() -> None:
        stop["n"] += 1
        recorder.before_cycle()

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=_INTERVAL,
        should_stop=should_stop,
        sleep=lambda _s, _stop: None,
        on_cycle=recorder.on_cycle,
        before_cycle=before_cycle,
        max_cycles=1,
    )
    assert cycles == 1

    # The last write was the idle snapshot at clock=100.0.
    published = read_activity(path)
    assert published is not None
    assert published.phase == PHASE_IDLE
    deadline = published.expires_at
    assert deadline is not None
    assert deadline == 100.0 + _INTERVAL + LIVENESS_GRACE_SECONDS

    # A peer supervisor reading mid-window adopts the (now-exited) daemon's
    # record while it is still fresh...
    assert read_live_activity(path, now=deadline - 1.0) is not None
    # ...and once the deadline lapses, the same read reads not-live -> respawn.
    assert read_live_activity(path, now=deadline + 1.0) is None
    assert is_record_live(published, now=deadline + 1.0) is False


# --- headless supervised run: restart on death, adopt a live daemon ----------


def _budget(max_respawns: int) -> SupervisionBudget:
    return SupervisionBudget(max_respawns=max_respawns, window_seconds=300.0)


def test_headless_respawn_restarts_a_daemon_that_dies_once() -> None:
    """A daemon that dies once inside budget is restarted; a further child runs.

    Criterion #5: drive the headless supervised entrypoint with an injected
    spawn whose first child crashes and second child runs; assert a second child
    is spawned (the restart) under the shared policy, with injected spawn +
    clock -- no real subprocess. Proves an observed restart, not merely that an
    entrypoint exists.
    """
    clock = {"t": 0.0}
    policy = SupervisionPolicy(_budget(5), clock=lambda: clock["t"])

    stop = {"flag": False}
    spawned: list[int] = []

    def spawn() -> SupervisedChild:
        idx = len(spawned)
        pid = 1000 + idx

        def wait() -> int:
            if idx == 0:
                # The first daemon crashes inside the window.
                clock["t"] += 1.0
                return 1
            # The restarted daemon is now the live one; end the run the way a
            # forwarded SIGTERM would -- a graceful stop, not a crash.
            stop["flag"] = True
            return 0

        spawned.append(pid)
        return SupervisedChild(pid=pid, wait=wait)

    result = run_supervised(
        spawn=spawn,
        policy=policy,
        should_stop=lambda: stop["flag"],
        read_liveness=lambda: None,
        max_spawns=5,
    )

    assert spawned == [1000, 1001]  # restarted exactly once
    assert result.spawn_count == 2
    assert result.deaths == 1  # only the crash counts, not the graceful stop
    assert result.outcome is SupervisedOutcome.STOPPED
    assert result.last_pid == 1001


def test_headless_adopts_live_record_and_spawns_nothing() -> None:
    """A live liveness record at startup is adopted; no daemon is spawned.

    Criterion #6: point the headless entrypoint at a live record from a daemon
    it does not own; assert it adopts (spawn count == 0) rather than launching a
    duplicate.
    """
    spawned: list[int] = []

    def spawn() -> SupervisedChild:  # pragma: no cover - must not run
        spawned.append(1)
        raise AssertionError("must not spawn when a live daemon is adopted")

    live = AutopilotActivity(
        pid=9999,
        phase=PHASE_IDLE,
        cycle_index=2,
        updated_at=1000.0,
        interval_seconds=_INTERVAL,
        expires_at=1000.0 + _INTERVAL,
    )

    result = run_supervised(
        spawn=spawn,
        policy=SupervisionPolicy(_budget(5)),
        should_stop=lambda: False,
        read_liveness=lambda: live,
        max_spawns=5,
    )

    assert result.outcome is SupervisedOutcome.ADOPTED
    assert result.spawn_count == 0  # spawn count == 0 (the criterion #6 assert)
    assert result.adopted_pid == 9999
    assert spawned == []  # spawn was never called


def test_headless_stale_record_does_not_block_a_spawn() -> None:
    """A dead daemon's lapsed record reads not-live -> the entrypoint spawns.

    Mirrors the worker lease-expiry semantics: a corpse's leftover record must
    not keep the headless entrypoint from launching a replacement.
    """
    stop = {"flag": False}
    spawned: list[int] = []

    def spawn() -> SupervisedChild:
        pid = 3000 + len(spawned)

        def wait() -> int:
            stop["flag"] = True  # run one child, then stop as a graceful shutdown
            return 0

        spawned.append(pid)
        return SupervisedChild(pid=pid, wait=wait)

    result = run_supervised(
        spawn=spawn,
        policy=SupervisionPolicy(_budget(5)),
        should_stop=lambda: stop["flag"],
        # A stale record reads back as not-live (None): nothing to adopt.
        read_liveness=lambda: None,
        max_spawns=5,
    )

    assert spawned == [3000]  # a real child was spawned, not adopted
    assert result.outcome is SupervisedOutcome.STOPPED
    assert result.spawn_count == 1


def test_headless_budget_exhausted_stops_respawning() -> None:
    """A daemon that keeps crashing past budget latches DEAD_AFTER_BUDGET.

    The headless safety interlock (task edge case; the headless side of
    criterion #2): rather than crash-looping unattended into the base branch,
    the entrypoint stops respawning once the shared windowed budget is spent and
    returns the loud terminal state instead of looping forever.
    """
    clock = {"t": 0.0}
    policy = SupervisionPolicy(_budget(2), clock=lambda: clock["t"])
    spawned: list[int] = []

    def spawn() -> SupervisedChild:
        pid = 2000 + len(spawned)

        def wait() -> int:
            clock["t"] += 1.0  # deaths land inside the same window
            return 1  # every child crashes immediately

        spawned.append(pid)
        return SupervisedChild(pid=pid, wait=wait)

    result = run_supervised(
        spawn=spawn,
        policy=policy,
        should_stop=lambda: False,
        read_liveness=lambda: None,
        max_spawns=10,
    )

    # budget max_respawns=2: deaths 1 and 2 respawn, the 3rd exhausts.
    assert result.outcome is SupervisedOutcome.DEAD_AFTER_BUDGET
    assert result.deaths == 3
    assert result.spawn_count == 3
    assert spawned == [2000, 2001, 2002]


def test_headless_stop_before_first_spawn_runs_nothing() -> None:
    """A stop signal set before startup spawns no daemon (mirrors the daemon)."""

    def spawn() -> SupervisedChild:  # pragma: no cover - must not run
        raise AssertionError("no spawn when stop is set up front")

    result = run_supervised(
        spawn=spawn,
        policy=SupervisionPolicy(_budget(5)),
        should_stop=lambda: True,
        read_liveness=lambda: None,
    )
    assert result.outcome is SupervisedOutcome.STOPPED
    assert result.spawn_count == 0


def test_headless_terminates_a_live_child_on_stop_orphan_free() -> None:
    """On a forwarded stop the still-live child is terminated, leaving no orphan.

    The orphan-free-exit guarantee: the wrapper's ``finally`` brings the last
    spawned child down rather than returning while it is still running.
    """
    stop = {"flag": False}
    terminated: list[int] = []

    def spawn() -> SupervisedChild:
        pid = 4242

        def wait() -> int:
            # The child is still "alive" when the operator asks the wrapper to
            # stop; wait returns only because the stop was forwarded to it.
            stop["flag"] = True
            return 0

        def terminate() -> None:
            terminated.append(pid)

        return SupervisedChild(pid=pid, wait=wait, terminate=terminate)

    result = run_supervised(
        spawn=spawn,
        policy=SupervisionPolicy(_budget(5)),
        should_stop=lambda: stop["flag"],
        read_liveness=lambda: None,
        max_spawns=5,
    )

    assert result.outcome is SupervisedOutcome.STOPPED
    assert terminated == [4242]  # the wrapper cleaned the child up on the way out
