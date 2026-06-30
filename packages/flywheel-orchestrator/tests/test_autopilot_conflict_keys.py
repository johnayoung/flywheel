"""Tests for autopilot-derived conflict keys (spec 00064 P2 / spec 00061 Gap 3).

Autopilot now stamps ``conflict_keys`` onto every emitted task, derived
deterministically from the files it contends for (``grader_target`` +
``creates_files``), so two tasks scoped to the same source file serialize at
claim time instead of racing -- the dedupe-swarm failure mode where ~25 tasks
all targeting ``crates/infrared-feed/src/tycho.rs`` ran concurrently overnight.

These pin the derivation (specific source files become keys; build manifests and
directories do not, so unrelated work is not over-serialized), the round-trip
through the directory work source into ``WorkItem.conflict_keys``, and the swarm
scenario (two tasks on the same file share a key).
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator._autopilot import (
    AUTOPILOT_PHASE,
    EmittedTask,
    Finding,
    ScoreBreakdown,
    Tier,
    _conflict_key_for_path,
    _conflict_keys_for,
    emit_emitted_task,
)
from flywheel_orchestrator._sources import DirectoryWorkSource


def _emitted(
    task_id: str,
    *,
    grader_target: str,
    creates_files: tuple[str, ...] = (),
) -> EmittedTask:
    return EmittedTask(
        finding=Finding(id=f"find-{task_id}", tier=Tier.TEST_COVERAGE, title=task_id),
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run="cargo test --workspace")],
        ),
        authoritative_grader="cargo test --workspace",
        grader_source="repo_command",
        grader_target=grader_target,
        creates_files=creates_files,
    )


def _breakdown() -> ScoreBreakdown:
    return ScoreBreakdown(
        tier=Tier.TEST_COVERAGE,
        tier_weight=1,
        urgency=1,
        importance=1,
        blocks=0,
        effort=1,
        final=10.0,
        preemptive=False,
    )


# --- path normalization ------------------------------------------------------


def test_specific_source_file_is_a_key() -> None:
    assert (
        _conflict_key_for_path("crates/infrared-feed/src/tycho.rs")
        == "crates/infrared-feed/src/tycho.rs"
    )


def test_manifest_and_lockfile_targets_are_not_keys() -> None:
    # Many distinct tasks legitimately share a build manifest; serializing on it
    # would collapse unrelated work into one lane.
    assert _conflict_key_for_path("Cargo.toml") is None
    assert _conflict_key_for_path("crates/infrared-feed/Cargo.toml") is None
    assert _conflict_key_for_path("Cargo.lock") is None
    assert _conflict_key_for_path("pyproject.toml") is None


def test_directory_like_targets_are_not_keys() -> None:
    assert _conflict_key_for_path("crates/") is None
    assert _conflict_key_for_path("crates") is None  # extensionless => coarse


def test_unusable_paths_are_not_keys() -> None:
    assert _conflict_key_for_path("") is None
    assert _conflict_key_for_path("   ") is None
    assert _conflict_key_for_path("/etc/passwd") is None  # absolute
    assert _conflict_key_for_path("../escape.rs") is None  # parent-escaping


# --- derivation from an emitted task -----------------------------------------


def test_keys_derive_from_grader_target() -> None:
    e = _emitted("t1", grader_target="crates/infrared-feed/src/tycho.rs")
    assert _conflict_keys_for(e) == ["crates/infrared-feed/src/tycho.rs"]


def test_keys_merge_grader_target_and_creates_files_sorted_deduped() -> None:
    e = _emitted(
        "t1",
        grader_target="crates/a/src/lib.rs",
        creates_files=("crates/b/src/new.rs", "crates/a/src/lib.rs", "Cargo.toml"),
    )
    # Sorted, de-duplicated, manifest dropped.
    assert _conflict_keys_for(e) == [
        "crates/a/src/lib.rs",
        "crates/b/src/new.rs",
    ]


def test_coarse_only_target_yields_no_keys() -> None:
    e = _emitted("t1", grader_target="Cargo.toml")
    assert _conflict_keys_for(e) == []


# --- round-trip into WorkItem.conflict_keys ----------------------------------


def test_emitted_file_carries_conflict_keys_into_work_item(
    tmp_path: Path,
) -> None:
    tasks_dir = tmp_path / "tasks"
    e = _emitted("t1", grader_target="crates/infrared-feed/src/tycho.rs")
    path = emit_emitted_task(e, _breakdown(), tasks_dir=tasks_dir)
    assert path is not None

    items = DirectoryWorkSource(tasks_dir).list_work()
    assert len(items) == 1
    assert items[0].conflict_keys == frozenset(
        {"crates/infrared-feed/src/tycho.rs"}
    )


def test_coarse_target_emits_no_conflict_keys_field(tmp_path: Path) -> None:
    tasks_dir = tmp_path / "tasks"
    e = _emitted("t1", grader_target="Cargo.toml")
    path = emit_emitted_task(e, _breakdown(), tasks_dir=tasks_dir)
    assert path is not None

    items = DirectoryWorkSource(tasks_dir).list_work()
    assert items[0].conflict_keys == frozenset()


def test_swarm_tasks_on_same_file_share_a_conflict_key(tmp_path: Path) -> None:
    # The regression: every dedupe-swarm task targeted the same source file.
    # They must now share a conflict key so claim-time enforcement serializes
    # them (only one in flight) instead of racing.
    tasks_dir = tmp_path / "tasks"
    target = "crates/infrared-feed/src/tycho.rs"
    for i in range(3):
        emit_emitted_task(
            _emitted(f"dedupe-{i}", grader_target=target),
            _breakdown(),
            tasks_dir=tasks_dir,
        )

    items = DirectoryWorkSource(tasks_dir).list_work()
    assert len(items) == 3
    keysets = {frozenset(item.conflict_keys) for item in items}
    assert keysets == {frozenset({target})}  # identical, overlapping key


def test_default_phase_is_autopilot(tmp_path: Path) -> None:
    # Sanity: emission lands under the autopilot phase the daemon drains.
    tasks_dir = tmp_path / "tasks"
    emit_emitted_task(
        _emitted("t1", grader_target="crates/a/src/lib.rs"),
        _breakdown(),
        tasks_dir=tasks_dir,
    )
    assert (tasks_dir / "active" / AUTOPILOT_PHASE / "t1.json").is_file()
