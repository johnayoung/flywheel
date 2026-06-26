"""Tests for the autopilot daemon's live activity snapshot.

The daemon runs detached from the console, so it writes an AutopilotActivity
snapshot each cycle that the console-side supervisor reads to show what the
daemon is doing right now. These cover the on-disk round-trip, the degrade-to-
None reads (missing / malformed / wrong schema), and the per-cycle snapshot
sequence the daemon emits (starting -> running -> idle, carrying the previous
cycle's summary into the next running snapshot).
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator._autopilot import (
    AutopilotPassResult,
    DroppedFinding,
    EmittedTask,
    Finding,
    Tier,
)
from flywheel_orchestrator._autopilot_activity import (
    ACTIVITY_SCHEMA_VERSION,
    PHASE_IDLE,
    PHASE_RUNNING,
    PHASE_STARTING,
    AutopilotActivity,
    EmittedSummary,
    read_activity,
    write_activity,
)
from flywheel_orchestrator._autopilot_run import _ActivityRecorder, run_daemon_loop


def _emitted_task(task_id: str, tier: Tier) -> EmittedTask:
    return EmittedTask(
        finding=Finding(id=f"find-{task_id}", tier=tier, title=task_id),
        task=Task(goal="cover it", graders=[CommandGrader(run="pytest")], id=task_id),
        authoritative_grader="pytest",
        grader_source="repo_command",
        grader_target="tests/x.py",
    )


def _result(
    *,
    emitted: tuple[EmittedTask, ...] = (),
    dropped: tuple[DroppedFinding, ...] = (),
    relevant: tuple[Tier, ...] = (),
    reason: str = "",
) -> AutopilotPassResult:
    return AutopilotPassResult(
        emitted_paths=tuple(Path(f"{e.task.id}.json") for e in emitted),
        emitted=emitted,
        dropped=dropped,
        relevant_tiers=relevant,
        reason=reason,
    )


# --- on-disk round-trip -----------------------------------------------------


def test_write_read_round_trip_preserves_all_fields(tmp_path: Path) -> None:
    path = tmp_path / "activity.json"
    activity = AutopilotActivity(
        pid=4321,
        phase=PHASE_IDLE,
        cycle_index=7,
        updated_at=1000.0,
        interval_seconds=300.0,
        next_cycle_at=1300.0,
        last_emitted=2,
        last_dropped=1,
        last_reason="emitted 2, dropped 1",
        last_relevant_tiers=(1, 3),
        last_emitted_tasks=(
            EmittedSummary(task_id="fix-a", tier=1),
            EmittedSummary(task_id="fix-b", tier=3),
        ),
    )
    write_activity(path, activity)
    back = read_activity(path)
    assert back == activity


def test_write_creates_missing_parent_dirs(tmp_path: Path) -> None:
    path = tmp_path / "deep" / "nested" / "activity.json"
    write_activity(
        path,
        AutopilotActivity(
            pid=1,
            phase=PHASE_STARTING,
            cycle_index=0,
            updated_at=0.0,
            interval_seconds=300.0,
        ),
    )
    assert read_activity(path) is not None


def test_write_overwrites_prior_snapshot(tmp_path: Path) -> None:
    path = tmp_path / "activity.json"
    write_activity(
        path,
        AutopilotActivity(
            pid=1,
            phase=PHASE_RUNNING,
            cycle_index=1,
            updated_at=0.0,
            interval_seconds=300.0,
        ),
    )
    write_activity(
        path,
        AutopilotActivity(
            pid=1,
            phase=PHASE_IDLE,
            cycle_index=1,
            updated_at=0.0,
            interval_seconds=300.0,
        ),
    )
    back = read_activity(path)
    assert back is not None and back.phase == PHASE_IDLE


# --- degrade-to-None reads --------------------------------------------------


def test_read_missing_file_is_none(tmp_path: Path) -> None:
    assert read_activity(tmp_path / "nope.json") is None


def test_read_none_path_is_none() -> None:
    assert read_activity(None) is None


def test_read_malformed_json_is_none(tmp_path: Path) -> None:
    path = tmp_path / "activity.json"
    path.write_text("{not json", encoding="utf-8")
    assert read_activity(path) is None


def test_read_wrong_schema_version_is_none(tmp_path: Path) -> None:
    path = tmp_path / "activity.json"
    path.write_text(
        f'{{"schema_version": {ACTIVITY_SCHEMA_VERSION + 99}, "pid": 1}}',
        encoding="utf-8",
    )
    assert read_activity(path) is None


# --- the recorder's per-cycle snapshot sequence -----------------------------


def test_recorder_starting_then_running_then_idle(tmp_path: Path) -> None:
    path = tmp_path / "activity.json"
    clock = iter([10.0, 20.0, 30.0])
    recorder = _ActivityRecorder(
        path=path, interval_seconds=300.0, pid=999, clock=lambda: next(clock)
    )

    recorder.starting()
    starting = read_activity(path)
    assert starting is not None
    assert starting.phase == PHASE_STARTING
    assert starting.pid == 999
    assert starting.cycle_index == 0

    recorder.before_cycle()
    running = read_activity(path)
    assert running is not None
    assert running.phase == PHASE_RUNNING
    assert running.cycle_index == 1
    # No cycle has completed yet: no last-cycle summary.
    assert running.last_emitted == 0

    result = _result(
        emitted=(_emitted_task("fix-a", Tier.PRODUCTION_DOWN),),
        dropped=(
            DroppedFinding(
                finding=Finding(id="d1", tier=Tier.BROKEN_BUILD, title="x"),
                reason="ungradeable",
            ),
        ),
        relevant=(Tier.PRODUCTION_DOWN,),
        reason="emitted 1, dropped 1",
    )
    recorder.on_cycle(result)
    idle = read_activity(path)
    assert idle is not None
    assert idle.phase == PHASE_IDLE
    assert idle.last_emitted == 1
    assert idle.last_dropped == 1
    assert idle.last_reason == "emitted 1, dropped 1"
    assert idle.last_relevant_tiers == (int(Tier.PRODUCTION_DOWN),)
    assert idle.last_emitted_tasks == (
        EmittedSummary(task_id="fix-a", tier=int(Tier.PRODUCTION_DOWN)),
    )
    # next_cycle_at = clock-at-on_cycle (30.0) + interval (300).
    assert idle.next_cycle_at == 330.0


def test_running_snapshot_carries_previous_cycle_summary(tmp_path: Path) -> None:
    path = tmp_path / "activity.json"
    recorder = _ActivityRecorder(
        path=path, interval_seconds=60.0, pid=1, clock=lambda: 0.0
    )
    recorder.before_cycle()  # cycle 1 running
    recorder.on_cycle(
        _result(
            emitted=(_emitted_task("fix-a", Tier.PRODUCTION_DOWN),),
            reason="cycle 1",
        )
    )
    recorder.before_cycle()  # cycle 2 running -- must still show cycle 1's result
    running = read_activity(path)
    assert running is not None
    assert running.phase == PHASE_RUNNING
    assert running.cycle_index == 2
    assert running.last_emitted == 1
    assert running.last_reason == "cycle 1"


def test_recorder_drives_through_daemon_loop(tmp_path: Path) -> None:
    """The recorder's hooks wire into run_daemon_loop the way main wires them."""
    path = tmp_path / "activity.json"
    recorder = _ActivityRecorder(
        path=path, interval_seconds=42.0, pid=7, clock=lambda: 100.0
    )
    results = iter(
        [
            _result(reason="idle cycle"),
            _result(
                emitted=(_emitted_task("fix-z", Tier.IMMINENT_SEVERE_RISK),),
                reason="emitted 1",
            ),
        ]
    )
    stop = {"n": 0}

    def should_stop() -> bool:
        # Stop after two cycles have recorded.
        return stop["n"] >= 2

    def run_cycle() -> AutopilotPassResult:
        stop["n"] += 1
        return next(results)

    cycles = run_daemon_loop(
        run_cycle=run_cycle,
        interval_seconds=42.0,
        should_stop=should_stop,
        sleep=lambda _s, _stop: None,
        on_cycle=recorder.on_cycle,
        before_cycle=recorder.before_cycle,
        max_cycles=2,
    )
    assert cycles == 2
    final = read_activity(path)
    assert final is not None
    assert final.phase == PHASE_IDLE
    assert final.cycle_index == 2
    assert final.last_emitted == 1
    assert final.last_reason == "emitted 1"
    assert final.next_cycle_at == 142.0
