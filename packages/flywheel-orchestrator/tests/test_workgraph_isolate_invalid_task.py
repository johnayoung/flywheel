"""Containment floor (spec 00065): structural corruption isolates the
offending task(s) with a recorded reason instead of poisoning the pass.

A scheduling pass built from a task set carrying one structurally-invalid
task -- a duplicate id, a self-dependency, or a cycle member -- must exclude
that task with a recorded reason naming the offender AND still build a graph
in which the remaining valid, independent tasks stay selectable. Both halves
are asserted in every case: a silent drop (no recorded reason) fails, and
dropping the whole pass (the valid peer vanishing) fails.
"""

from __future__ import annotations

from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator import (
    ExcludedTask,
    WorkGraph,
    WorkItem,
)


def _item(task_id: str, *, prerequisites: tuple[str, ...] = ()) -> WorkItem:
    """A minimal, valid WorkItem with an explicit id and optional edges."""
    return WorkItem(
        task=Task(
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run="true")],
            id=task_id,
        ),
        source_ref=f"/tasks/{task_id}.json",
        prerequisites=prerequisites,
    )


def _fresh_states(graph: WorkGraph) -> dict[str, str]:
    """Map every surviving item id to an eligible (fresh) state."""
    return {item.task.id: "fresh" for item in graph.items}


def _selectable_ids(graph: WorkGraph) -> set[str]:
    return {item.task.id for item in graph.ready_set(_fresh_states(graph))}


# -- duplicate id -----------------------------------------------------------


def test_duplicate_id_is_isolated_and_valid_peer_stays_selectable() -> None:
    # Two items share the id "dup" (ambiguous -- neither is authoritative);
    # "solo" is a valid independent peer in the same pass.
    result = WorkGraph.build(
        [_item("dup"), _item("dup"), _item("solo")]
    )
    graph = result.graph

    # Half 1: the valid, independent peer survives and is selectable.
    assert "solo" in _selectable_ids(graph)
    assert "solo" in {item.task.id for item in graph.items}

    # Half 2: the duplicate is excluded with a recorded reason naming it.
    excluded_by_id = {e.task_id: e for e in result.excluded}
    assert "dup" in excluded_by_id
    assert "dup" in excluded_by_id["dup"].reason
    # The offender is gone from the graph entirely (not merely de-selected).
    assert "dup" not in {item.task.id for item in graph.items}
    assert "dup" not in _selectable_ids(graph)


# -- self-dependency --------------------------------------------------------


def test_self_dependency_is_isolated_and_valid_peer_stays_selectable() -> None:
    result = WorkGraph.build(
        [_item("selfdep", prerequisites=("selfdep",)), _item("solo")]
    )
    graph = result.graph

    assert "solo" in _selectable_ids(graph)

    excluded_by_id = {e.task_id: e for e in result.excluded}
    assert "selfdep" in excluded_by_id
    assert "selfdep" in excluded_by_id["selfdep"].reason
    assert "selfdep" not in {item.task.id for item in graph.items}
    assert "selfdep" not in _selectable_ids(graph)


# -- cycle ------------------------------------------------------------------


def test_cycle_members_are_isolated_and_valid_peer_stays_selectable() -> None:
    # alpha depends on beta, beta depends on alpha -- a 2-node cycle. "solo"
    # is the valid independent peer.
    result = WorkGraph.build(
        [
            _item("alpha", prerequisites=("beta",)),
            _item("beta", prerequisites=("alpha",)),
            _item("solo"),
        ]
    )
    graph = result.graph

    assert "solo" in _selectable_ids(graph)

    excluded_by_id = {e.task_id: e for e in result.excluded}
    # Every participating member is isolated...
    assert {"alpha", "beta"} <= set(excluded_by_id)
    # ...and each member's reason names the whole cycle (recoverable from any
    # of its ids).
    for member in ("alpha", "beta"):
        assert "alpha" in excluded_by_id[member].reason
        assert "beta" in excluded_by_id[member].reason
    assert {"alpha", "beta"}.isdisjoint(
        {item.task.id for item in graph.items}
    )
    assert {"alpha", "beta"}.isdisjoint(_selectable_ids(graph))


# -- a sound pass records nothing -------------------------------------------


def test_structurally_sound_pass_excludes_nothing() -> None:
    # A clean chain (leaf depends on base) plus an independent task records no
    # exclusions, and both heads are present.
    result = WorkGraph.build(
        [_item("base"), _item("leaf", prerequisites=("base",)), _item("solo")]
    )
    assert result.excluded == ()
    assert {item.task.id for item in result.graph.items} == {
        "base",
        "leaf",
        "solo",
    }


# -- the exclusion record is on the graph too -------------------------------


def test_excluded_is_recorded_on_graph_and_result_alike() -> None:
    graph = WorkGraph([_item("dup"), _item("dup"), _item("solo")])
    assert isinstance(graph.excluded, tuple)
    assert all(isinstance(e, ExcludedTask) for e in graph.excluded)
    assert {e.task_id for e in graph.excluded} == {"dup"}
    # build() surfaces the identical record set.
    assert graph.validation.excluded == graph.excluded


# -- all three defects in one pass, one valid survivor ----------------------


def test_all_three_defects_isolated_in_a_single_pass() -> None:
    result = WorkGraph.build(
        [
            _item("dup"),
            _item("dup"),
            _item("selfdep", prerequisites=("selfdep",)),
            _item("alpha", prerequisites=("beta",)),
            _item("beta", prerequisites=("alpha",)),
            _item("solo"),
        ]
    )
    graph = result.graph

    # The lone valid, independent task survives and is selectable -- dropping
    # the whole pass would fail here.
    assert _selectable_ids(graph) == {"solo"}

    # Every offender is recorded with a reason naming it -- a silent drop
    # would fail here.
    excluded_by_id = {e.task_id: e for e in result.excluded}
    assert set(excluded_by_id) == {"dup", "selfdep", "alpha", "beta"}
    for task_id, exclusion in excluded_by_id.items():
        assert task_id in exclusion.reason


# -- a survivor depending on an isolated task stays ineligible ---------------


def test_survivor_depending_on_isolated_task_is_not_selectable() -> None:
    # "dependent" depends on "selfdep", which is isolated. The dependent
    # survives in the graph but, with a now-dangling prerequisite, never
    # enters the ready set -- exactly the missing-prerequisite posture.
    result = WorkGraph.build(
        [
            _item("selfdep", prerequisites=("selfdep",)),
            _item("dependent", prerequisites=("selfdep",)),
            _item("solo"),
        ]
    )
    graph = result.graph

    assert "selfdep" in {e.task_id for e in result.excluded}
    # The dependent is kept (a recorded missing-prereq issue), not selectable.
    assert "dependent" in {item.task.id for item in graph.items}
    assert "dependent" not in _selectable_ids(graph)
    assert any(
        issue.referencing_id == "dependent" and issue.missing_id == "selfdep"
        for issue in result.issues
    )
    # The truly-independent task is unaffected.
    assert "solo" in _selectable_ids(graph)
