"""Tests for the cross-task surface-overlap lint (infrared incident 2026-07-09).

Two active tasks that write the same file surface with no declared coordination
can be dispatched concurrently, both edit the shared file, and the loser's
rebase collides -- the P5 data-loss shape. These pin the surface derivation
(context.relevant + command-grader run paths, normalized), the pairwise flag,
and every exemption (shared conflict_keys, overlap_ok, prerequisite chain).
"""

from __future__ import annotations

import json
from pathlib import Path

from flywheel_core.task import CommandGrader, Context, Task
from flywheel_orchestrator._sources import _read_overlap_ok
from flywheel_orchestrator._surface_lint import (
    TaskSurface,
    build_surface,
    surface_overlap_defects,
    task_surface,
)


def _task(
    task_id: str,
    *,
    run: str = "true",
    relevant: tuple[str, ...] = (),
) -> Task:
    return Task(
        id=task_id,
        goal=f"Goal for {task_id}.",
        graders=[CommandGrader(run=run)],
        context=Context(relevant=list(relevant)),
    )


def _surface(
    task_id: str,
    paths: set[str],
    *,
    conflict_keys: set[str] | None = None,
    overlap_ok: set[str] | None = None,
    prerequisites: tuple[str, ...] = (),
) -> TaskSurface:
    return TaskSurface(
        task_id=task_id,
        paths=frozenset(paths),
        conflict_keys=frozenset(conflict_keys or set()),
        overlap_ok=frozenset(overlap_ok or set()),
        prerequisites=prerequisites,
    )


# --- surface derivation ------------------------------------------------------


def test_surface_includes_command_grader_run_paths() -> None:
    task = _task("t", run="uv run pytest tests/shared/golden_test.py")
    assert task_surface(task) == frozenset({"tests/shared/golden_test.py"})


def test_surface_includes_relevant_with_annotation_stripped() -> None:
    task = _task(
        "t",
        relevant=("tests/shared/golden_test.py -- writes the shared golden",),
    )
    assert task_surface(task) == frozenset({"tests/shared/golden_test.py"})


def test_surface_strips_pytest_node_ids_to_the_file_path() -> None:
    task = _task("t", run="pytest tests/a.py::TestX::test_y")
    assert task_surface(task) == frozenset({"tests/a.py"})


def test_surface_drops_urls_directories_and_manifests() -> None:
    task = _task(
        "t",
        run="uv run pytest tests/real.py",
        relevant=(
            "https://example.com/docs",  # URL -> nothing
            "docs/",  # bare directory -> nothing
            "packages/foo",  # extensionless -> nothing
            "pyproject.toml",  # manifest -> nothing
        ),
    )
    assert task_surface(task) == frozenset({"tests/real.py"})


def test_surface_of_bare_command_is_empty() -> None:
    assert task_surface(_task("t", run="true")) == frozenset()


# --- pairwise flagging -------------------------------------------------------


def test_disjoint_nonempty_conflict_keys_still_flag() -> None:
    # The incident shape: disjoint keys, shared surface, concurrent write.
    a = _surface("wsteth", {"tests/golden.py"}, conflict_keys={"internal-engine"})
    b = _surface(
        "balancer", {"tests/golden.py"}, conflict_keys={"harness-base"}
    )
    defects = surface_overlap_defects([a, b])
    assert len(defects) == 1
    detail = defects[0].detail
    assert "wsteth" in detail
    assert "balancer" in detail
    assert "tests/golden.py" in detail
    # Remediation options are all named.
    assert "conflict_keys" in detail
    assert "overlap_ok" in detail
    assert "prerequisites" in detail


def test_both_empty_conflict_keys_flag() -> None:
    a = _surface("a", {"tests/golden.py"})
    b = _surface("b", {"tests/golden.py"})
    assert len(surface_overlap_defects([a, b])) == 1


def test_shared_conflict_key_exempts_the_pair() -> None:
    a = _surface("a", {"tests/golden.py"}, conflict_keys={"harness", "x"})
    b = _surface("b", {"tests/golden.py"}, conflict_keys={"harness", "y"})
    assert surface_overlap_defects([a, b]) == []


def test_no_overlap_no_defect() -> None:
    a = _surface("a", {"tests/a.py"})
    b = _surface("b", {"tests/b.py"})
    assert surface_overlap_defects([a, b]) == []


def test_overlap_ok_exempts_only_the_listed_path() -> None:
    # Two overlapping paths; only one is allow-listed -> the other still flags.
    a = _surface(
        "a",
        {"tests/one.py", "tests/two.py"},
        overlap_ok={"tests/one.py"},
    )
    b = _surface("b", {"tests/one.py", "tests/two.py"})
    defects = surface_overlap_defects([a, b])
    assert len(defects) == 1
    assert "tests/two.py" in defects[0].detail
    assert "tests/one.py" not in defects[0].detail


def test_overlap_ok_on_either_task_exempts_the_whole_overlap() -> None:
    a = _surface("a", {"tests/one.py"})
    b = _surface("b", {"tests/one.py"}, overlap_ok={"tests/one.py"})
    assert surface_overlap_defects([a, b]) == []


def test_direct_prerequisite_exempts_the_pair() -> None:
    a = _surface("a", {"tests/g.py"}, prerequisites=("b",))
    b = _surface("b", {"tests/g.py"})
    assert surface_overlap_defects([a, b]) == []


def test_transitive_prerequisite_exempts_the_pair() -> None:
    # a -> mid -> b, all present: a reaches b, so the pair is a sequence.
    a = _surface("a", {"tests/g.py"}, prerequisites=("mid",))
    mid = _surface("mid", set(), prerequisites=("b",))
    b = _surface("b", {"tests/g.py"})
    assert surface_overlap_defects([a, mid, b]) == []


def test_prerequisite_through_absent_task_does_not_exempt() -> None:
    # a -> gone -> b, but 'gone' is not in the active listing: the edge to it is
    # dropped, so a does not reach b and the overlap still flags.
    a = _surface("a", {"tests/g.py"}, prerequisites=("gone",))
    b = _surface("b", {"tests/g.py"})
    assert len(surface_overlap_defects([a, b])) == 1


# --- build_surface reads the top-level orchestration keys --------------------


def test_build_surface_reads_conflict_keys_overlap_ok_prerequisites(
    tmp_path: Path,
) -> None:
    path = tmp_path / "ov.json"
    path.write_text(
        json.dumps(
            {
                "id": "ov",
                "goal": "G.",
                "graders": [
                    {"type": "command", "run": "pytest tests/golden.py"}
                ],
                "context": {"relevant": ["tests/other.py -- note"]},
                "conflict_keys": ["k1"],
                "overlap_ok": ["tests/golden.py"],
                "prerequisites": ["dep"],
            }
        )
    )
    from flywheel_core.loaders import load_task_file

    surface = build_surface(path, load_task_file(path))
    assert surface.task_id == "ov"
    assert surface.paths == frozenset({"tests/golden.py", "tests/other.py"})
    assert surface.conflict_keys == frozenset({"k1"})
    assert surface.overlap_ok == frozenset({"tests/golden.py"})
    assert surface.prerequisites == ("dep",)


def test_read_overlap_ok_tolerates_absent_and_malformed(tmp_path: Path) -> None:
    absent = tmp_path / "a.json"
    absent.write_text(json.dumps({"id": "a", "goal": "G.", "graders": []}))
    assert _read_overlap_ok(absent) == ()

    malformed = tmp_path / "b.json"
    malformed.write_text(
        json.dumps(
            {"id": "b", "goal": "G.", "graders": [], "overlap_ok": "nope"}
        )
    )
    assert _read_overlap_ok(malformed) == ()
