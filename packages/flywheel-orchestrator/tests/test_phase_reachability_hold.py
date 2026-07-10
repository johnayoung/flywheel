"""Phase-prerequisite reachability hold (spec 00079, criterion 7, D-4).

Under the ``phase`` submit strategy each phase lands its DONE tasks onto its own
integration branch ``flywheel/phase/<phase>`` and phases are independent in v1
(no PR stacking). A dependent in a later phase therefore branches from the true
base, which does *not* contain a sibling phase's work until that phase's PR
merges. So a prerequisite reaching DONE is not sufficient across unmerged
phases: the dependent must stay unclaimed until the prerequisite's landed work
is *reachable* (git ancestry) from the base the dependent would branch from,
becoming claimable on the first pass after reachability holds -- with no
operator action, and never parked/failed.

These build real git repos so the ancestry probe has commits to walk, and gate
on the existence of ``flywheel/phase/<phase>`` -- never created under merge/pr,
so those repos schedule byte-identically. The hold is a scheduling verdict and
a visible status surface: the withheld dependent reads ``blocked_by_prereq``
naming the blocking phase, never silent starvation.
"""

from __future__ import annotations

import subprocess
from datetime import datetime, timezone
from pathlib import Path

from flywheel_core.lifecycle import Lifecycle, Status
from flywheel_core.store_sqlite import SqliteStore
from flywheel_core.task import CommandGrader, Task
from flywheel_orchestrator._rollup import RollupStatus, build_rollup
from flywheel_orchestrator._sources import WorkItem
from flywheel_orchestrator._work_graph import WorkGraph
from flywheel_orchestrator._workflow import (
    PrerequisiteReachabilityHold,
    reachability_held_prerequisites,
    select_next_task,
    status_rows_for_items,
)

# ---------- git helpers ----------


def _git(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        capture_output=True,
        text=True,
    )


def _git_init_repo(repo: Path) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    _git(repo, "init", "-b", "main")
    _git(repo, "config", "user.email", "reachability-test@example.com")
    _git(repo, "config", "user.name", "reachability test")
    _git(repo, "config", "commit.gpgsign", "false")
    (repo / "README.md").write_text("base\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", "init")


def _git_head(repo: Path) -> str:
    return _git(repo, "rev-parse", "HEAD").stdout.strip()


def _phase_branch(phase: str) -> str:
    return f"flywheel/phase/{phase}"


def _build_phase_branch(repo: Path, phase: str, filename: str) -> str:
    """Create ``flywheel/phase/<phase>`` off main with one commit; return tip.

    Leaves ``main`` checked out. The branch tip is NOT an ancestor of main --
    the open-PR shape a dependent's true base cannot yet reach.
    """
    branch = _phase_branch(phase)
    base = _git_head(repo)
    _git(repo, "checkout", "-b", branch, base)
    (repo / filename).write_text(f"{phase} content\n")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-m", f"feat: {phase} work")
    tip = _git_head(repo)
    _git(repo, "checkout", "main")
    return tip


def _merge_phase(repo: Path, phase: str) -> None:
    """Merge the phase branch into main as a merge commit (the PR merges).

    A merge-commit merge keeps the phase tip reachable from main, so the
    sibling's landed work becomes an ancestor of the true base.
    """
    _git(repo, "merge", "--no-ff", "-m", f"Merge {phase}", _phase_branch(phase))


# ---------- store / task helpers ----------


def _task(task_id: str) -> Task:
    return Task(id=task_id, goal=f"Goal {task_id}", graders=[CommandGrader(run="true")])


def _item(
    repo: Path,
    phase: str,
    task_id: str,
    *,
    prerequisites: tuple[str, ...] = (),
) -> WorkItem:
    """A file-backed work item so the row derives its phase from the path."""
    return WorkItem(
        task=_task(task_id),
        prerequisites=prerequisites,
        source_ref=f"{phase}/{task_id}.json",
        local_path=repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json",
    )


def _seed_done(store: SqliteStore, task_id: str) -> None:
    now = datetime.now(timezone.utc)
    lc = Lifecycle(task_id=task_id, run_id=f"run-{task_id}")
    lc.transition_to(Status.READY, now=now)
    lc.transition_to(Status.RUNNING, now=now)
    lc.transition_to(Status.VALIDATING, now=now)
    lc.transition_to(Status.DONE, now=now)
    store.create_lifecycle(lc)


# ---------- tests ----------


def test_unmerged_sibling_phase_holds_dependent_naming_phase(
    tmp_path: Path,
) -> None:
    """A DONE prerequisite whose phase's landed work is not reachable from the
    true base holds the dependent, naming the blocking phase. DONE-in-store
    alone would have selected it -- the ancestry check is what withholds it."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    _build_phase_branch(repo, "10-alpha", "alpha.txt")  # dep's work, unmerged

    items = [
        _item(repo, "10-alpha", "dep"),
        _item(repo, "20-beta", "dependent", prerequisites=("dep",)),
    ]
    store = SqliteStore(tmp_path / "db.sqlite")
    try:
        _seed_done(store, "dep")  # prerequisite reached DONE...
        rows = status_rows_for_items(items, store)
        # ...yet its landed work sits on the unmerged phase branch.
        holds = reachability_held_prerequisites(
            rows, repo_root=repo, true_base="main"
        )
        # Without the hold a DONE prerequisite satisfies selection outright.
        naive_pick = select_next_task(rows)
        # With it, the dependent is excluded from candidacy this pass.
        guarded_pick = select_next_task(rows, exclude_ids=frozenset(holds))
    finally:
        store.close()

    assert "dependent" in holds
    hold = holds["dependent"]
    assert isinstance(hold, PrerequisiteReachabilityHold)
    assert hold.blocking_phase == "10-alpha"
    assert hold.held_by == ("dep",)
    assert naive_pick is not None and naive_pick.task.id == "dependent"
    assert guarded_pick is None, (
        "the held dependent must not be offered while its prerequisite's "
        "phase is unmerged"
    )


def test_dependent_becomes_claimable_once_blocking_phase_merges(
    tmp_path: Path,
) -> None:
    """The hold is transient: once the blocking phase merges (its tip becomes an
    ancestor of the true base) the very next pass re-offers the dependent with
    no operator action -- never parked, never failed."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    _build_phase_branch(repo, "10-alpha", "alpha.txt")

    items = [
        _item(repo, "10-alpha", "dep"),
        _item(repo, "20-beta", "dependent", prerequisites=("dep",)),
    ]
    store = SqliteStore(tmp_path / "db.sqlite")
    try:
        _seed_done(store, "dep")
        rows = status_rows_for_items(items, store)
        held_before = reachability_held_prerequisites(
            rows, repo_root=repo, true_base="main"
        )
        assert "dependent" in held_before

        _merge_phase(repo, "10-alpha")  # the phase PR merges

        held_after = reachability_held_prerequisites(
            rows, repo_root=repo, true_base="main"
        )
        pick = select_next_task(rows, exclude_ids=frozenset(held_after))
    finally:
        store.close()

    assert held_after == {}, "reachability holds; the hold must clear"
    assert pick is not None and pick.task.id == "dependent", (
        "the dependent must become claimable the first pass after merge"
    )


def test_rollup_surfaces_reachability_hold_as_blocked_by_prereq(
    tmp_path: Path,
) -> None:
    """The status rollup renders the held dependent as blocked_by_prereq naming
    the blocking phase -- the hold is visible, never silent starvation."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    _build_phase_branch(repo, "10-alpha", "alpha.txt")

    items = [
        _item(repo, "10-alpha", "dep"),
        _item(repo, "20-beta", "dependent", prerequisites=("dep",)),
    ]
    store = SqliteStore(tmp_path / "db.sqlite")
    try:
        _seed_done(store, "dep")
        rows = status_rows_for_items(items, store)
        rollup = build_rollup(rows, store, repo_root=repo, true_base="main")
    finally:
        store.close()

    dependent = next(
        t for p in rollup.phases for t in p.tasks if t.task_id == "dependent"
    )
    assert dependent.status is RollupStatus.BLOCKED_BY_PREREQ
    assert dependent.unsatisfied_prerequisites == ("dep",)
    assert "10-alpha" in dependent.detail
    assert "not reachable" in dependent.detail


def test_same_phase_prerequisite_chain_is_unaffected(tmp_path: Path) -> None:
    """A same-phase prerequisite shares one integration branch, so the hold
    never applies -- even while a sibling phase branch is present and unmerged
    (which proves the machinery is armed, not merely no-op)."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    _build_phase_branch(repo, "10-alpha", "alpha.txt")  # sibling, unmerged

    items = [
        _item(repo, "10-alpha", "dep"),
        # Same-phase chain in beta: second depends on first.
        _item(repo, "20-beta", "first"),
        _item(repo, "20-beta", "second", prerequisites=("first",)),
        # Cross-phase dependent on the unmerged alpha: this one IS held.
        _item(repo, "20-beta", "crossdep", prerequisites=("dep",)),
    ]
    store = SqliteStore(tmp_path / "db.sqlite")
    try:
        _seed_done(store, "dep")
        _seed_done(store, "first")
        rows = status_rows_for_items(items, store)
        holds = reachability_held_prerequisites(
            rows, repo_root=repo, true_base="main"
        )
        pick = select_next_task(rows, exclude_ids=frozenset(holds))
    finally:
        store.close()

    assert "second" not in holds, "a same-phase prerequisite must never hold"
    assert "crossdep" in holds, (
        "the machinery must be armed -- the cross-phase dependent is held"
    )
    assert pick is not None and pick.task.id == "second", (
        "the same-phase dependent stays freely claimable"
    )


def test_merge_pr_repo_without_phase_branch_is_byte_identical(
    tmp_path: Path,
) -> None:
    """merge/pr repos create no ``flywheel/phase/*`` branch, so the hold never
    arms: a cross-phase DONE prerequisite satisfies selection exactly as before
    (a single shared base makes reachability trivially true)."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)  # no phase branch is ever built

    items = [
        _item(repo, "10-alpha", "dep"),
        _item(repo, "20-beta", "dependent", prerequisites=("dep",)),
    ]
    store = SqliteStore(tmp_path / "db.sqlite")
    try:
        _seed_done(store, "dep")
        rows = status_rows_for_items(items, store)
        holds = reachability_held_prerequisites(
            rows, repo_root=repo, true_base="main"
        )
        pick = select_next_task(rows, exclude_ids=frozenset(holds))
    finally:
        store.close()

    assert holds == {}, "no phase branch must leave the hold a no-op"
    assert pick is not None and pick.task.id == "dependent"


def test_reachability_hold_keys_on_real_ancestry(tmp_path: Path) -> None:
    """Differential anti-cheat: identical two-phase setup, flipped only by
    whether the sibling phase merged. The merged repo holds nothing (dependent
    claimable); the open-PR repo holds the dependent. A hold that treated DONE
    as satisfied without the ancestry check could not produce both outcomes."""
    def items_for(repo: Path) -> list[WorkItem]:
        return [
            _item(repo, "10-alpha", "dep"),
            _item(repo, "20-beta", "dependent", prerequisites=("dep",)),
        ]

    repo_open = tmp_path / "open"
    _git_init_repo(repo_open)
    _build_phase_branch(repo_open, "10-alpha", "alpha.txt")

    repo_merged = tmp_path / "merged"
    _git_init_repo(repo_merged)
    _build_phase_branch(repo_merged, "10-alpha", "alpha.txt")
    _merge_phase(repo_merged, "10-alpha")

    store_open = SqliteStore(repo_open / "db.sqlite")
    store_merged = SqliteStore(repo_merged / "db.sqlite")
    try:
        _seed_done(store_open, "dep")
        _seed_done(store_merged, "dep")
        holds_open = reachability_held_prerequisites(
            status_rows_for_items(items_for(repo_open), store_open),
            repo_root=repo_open,
            true_base="main",
        )
        holds_merged = reachability_held_prerequisites(
            status_rows_for_items(items_for(repo_merged), store_merged),
            repo_root=repo_merged,
            true_base="main",
        )
    finally:
        store_open.close()
        store_merged.close()

    assert "dependent" in holds_open
    assert holds_merged == {}


def test_ready_set_withholds_then_releases_across_merge(tmp_path: Path) -> None:
    """The orchestrate loop's exact mechanism: hold ids feed the fresh-selection
    ``excluded`` set fed to ``WorkGraph.ready_set``. The dependent is out of the
    ready set while the sibling phase is unmerged, and re-enters it once merged
    -- no lifecycle mutation, so the task is never consumed."""
    repo = tmp_path / "repo"
    _git_init_repo(repo)
    _build_phase_branch(repo, "10-alpha", "alpha.txt")

    items = [
        _item(repo, "10-alpha", "dep"),
        _item(repo, "20-beta", "dependent", prerequisites=("dep",)),
    ]
    graph = WorkGraph.build(items).graph
    store = SqliteStore(tmp_path / "db.sqlite")
    try:
        _seed_done(store, "dep")
        rows = status_rows_for_items(items, store)
        states = {r.task.id: r.state for r in rows}

        held = reachability_held_prerequisites(
            rows, repo_root=repo, true_base="main"
        )
        ready_before = graph.ready_set(states, excluded=frozenset(held))
        assert "dependent" not in {i.task.id for i in ready_before}

        _merge_phase(repo, "10-alpha")

        held_after = reachability_held_prerequisites(
            rows, repo_root=repo, true_base="main"
        )
        ready_after = graph.ready_set(states, excluded=frozenset(held_after))
    finally:
        store.close()

    assert "dependent" in {i.task.id for i in ready_after}
