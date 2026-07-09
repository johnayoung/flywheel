"""Behavior: a DONE run whose branch can neither fast-forward nor cleanly
rebase onto the advanced base is recovered unattended through the
merge-fallback landing rung (spec 00076, D-1/D-2/D-3), or parked with its
branch+worktree preserved under a queryable park kind.

Every scenario runs real git against a tmp repo. The base advance is shaped so
the branch's rebase genuinely conflicts, exercising the merge-fallback leaf of
``GitWorktreeSubmitter._submit``. The store is a recording ledger capturing the
domain events the submitter appends, so each criterion is asserted against the
witness that actually lands on the run's ledger -- never a value the recorder
invents.

The "rebase conflicts but the merge is clean" shape (criteria 1, 2, 6) is a
branch that edits a line and then reverts it: the net branch tree leaves the
line untouched, so a 3-way merge of an advanced base takes the base's edit
cleanly, while a commit-by-commit rebase replays the intermediate edit and
collides with the base. The "both conflict" shape (criterion 3) drops the
revert, so the surviving edit collides on both paths.

Criterion 1: after a merge-fallback land, ``git merge-base --is-ancestor
<original-branch-head> <base>`` exits 0 and the base ref advanced -- a landed
witness with no ancestor relation fails the test. Criterion 2: a task grader
that passes on the branch tree but fails on the merged tree leaves the base ref
byte-identical and records no ``Landed``. Criterion 3: a merge conflict aborts
cleanly -- the branch ref still resolves, its worktree exists with no
in-progress merge state, and a ``merge-conflict`` park is on the ledger.
Criterion 6: the ``Landed`` record names the ``merge-fallback`` rung. Criterion
7: a failing re-verify appends a fresh ``LandingParked`` witness (which the
re-driver counts toward its bound) and preserves the branch.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_core.events import (
    LANDING_PARK_KINDS,
    LANDING_STRATEGY_MERGE,
    PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_MERGE_CONFLICT,
    RUNG_MERGE_FALLBACK,
    DomainEvent,
    Landed,
    LandingParked,
)
from flywheel_orchestrator import SandboxRequest, SubmitRequest

from flywheel_worktree import worker


# --- store stub ---------------------------------------------------------------


class _RecordingLedger:
    """Minimal LandingLedger stub capturing every appended domain event, enough
    to assert which landing/park witness a merge-fallback pass recorded."""

    def __init__(self) -> None:
        self.events: list[DomainEvent] = []

    class _Lifecycle:
        version = 0

    def load_lifecycle(self, run_id: str) -> "_RecordingLedger._Lifecycle":
        return self._Lifecycle()

    def append_domain_event(
        self, event: DomainEvent, *, expected_version: int
    ) -> "_RecordingLedger._Lifecycle":
        self.events.append(event)
        return self._Lifecycle()

    def landed(self) -> list[Landed]:
        return [e for e in self.events if isinstance(e, Landed)]

    def parked(self) -> list[LandingParked]:
        return [e for e in self.events if isinstance(e, LandingParked)]


# --- git / fixture helpers ----------------------------------------------------


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "landing-test@example.com")
    _git(path, "config", "user.name", "landing test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    # The line the branch and the advanced base contend over.
    (path / "file.txt").write_text("line1\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _is_ancestor(repo: Path, ancestor: str, rev: str) -> bool:
    """``git merge-base --is-ancestor`` -- exit 0 iff ``ancestor`` is reachable
    from ``rev`` (so the branch's commits became part of the landed base)."""
    return (
        subprocess.run(
            ["git", "-C", str(repo), "merge-base", "--is-ancestor",
             ancestor, rev],
            capture_output=True,
        ).returncode
        == 0
    )


def _write_commit(cwd: Path, files: dict[str, str], message: str) -> None:
    for name, body in files.items():
        (cwd / name).write_text(body)
    _git(cwd, "add", "-A")
    _git(cwd, "commit", "-m", message)


def _merge_bearing_branch(wt: Path) -> str:
    """Edit ``file.txt`` then revert it: the branch's net tree leaves the line
    untouched (so an advanced base merges cleanly) while a rebase replays the
    intermediate edit and conflicts. Returns the branch tip sha."""
    _write_commit(wt, {"file.txt": "line1-branch\n"}, "diverge file.txt")
    _write_commit(wt, {"file.txt": "line1\n"}, "revert file.txt")
    return _rev(wt, "HEAD")


def _task_file(repo: Path, phase: str, task_id: str, run: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": run}],
            }
        )
    )
    return tf


def _sandbox_req(tf: Path, task_id: str) -> SandboxRequest:
    return SandboxRequest(
        task_id=task_id, task_file=tf, run_id=None, mode="fresh"
    )


def _submit_req(
    tf: Path, task_id: str, sandbox: Path, run: str
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run=run)],
        ),
        run_id="run-1",
        status=Status.DONE,
        sandbox=sandbox,
    )


def _merge_submitter(
    repo: Path, ledger: _RecordingLedger, **kwargs: object
) -> worker.GitWorktreeSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        store=ledger,  # type: ignore[arg-type]
        **kwargs,  # type: ignore[arg-type]
    )


# --- criterion 1 + 6: the merge-fallback land ---------------------------------


def test_merge_fallback_lands_rebase_conflicting_branch(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _merge_submitter(repo, ledger)
    tf = _task_file(repo, "01-phase", "t1", run="true")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    branch_head = _merge_bearing_branch(wt)
    # Advance the base over the same line the branch touched: the branch's
    # rebase replays its intermediate edit and conflicts, so the clean-FF and
    # rebase rungs both fail and submit falls through to merge-fallback.
    _write_commit(repo, {"file.txt": "line1-main\n"}, "base advances")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, run="true"))

    # Criterion 6: exactly one Landed witness, naming the merge-fallback rung.
    landed = ledger.landed()
    assert len(landed) == 1
    assert landed[0].strategy == LANDING_STRATEGY_MERGE
    assert landed[0].rung == RUNG_MERGE_FALLBACK
    # Criterion 1: the base ref advanced and the branch's tip became an
    # ancestor of it -- the commits are reachable from the landed base, not
    # merely marked landed in the ledger.
    assert _rev(repo, "main") != base_before
    assert _is_ancestor(repo, branch_head, "main")
    # The recorded reference is the advanced base head observed via git.
    assert landed[0].landed_ref == _rev(repo, "main")
    # A land records no park.
    assert ledger.parked() == []


def test_merge_fallback_lands_when_base_not_checked_out(tmp_path: Path) -> None:
    # Must-not-regress: the separate-base (out-of-tree) layout lands via the
    # merge-fallback rung too. The base "main" is not the operator's checked-out
    # branch, so the fast-forward advances its ref out-of-tree.
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _merge_submitter(repo, ledger)
    tf = _task_file(repo, "01-phase", "t1", run="true")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    branch_head = _merge_bearing_branch(wt)
    _write_commit(repo, {"file.txt": "line1-main\n"}, "base advances")
    # Move the operator's HEAD off "main" so the landing base is not checked out.
    _git(repo, "checkout", "-b", "operator")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, run="true"))

    landed = ledger.landed()
    assert len(landed) == 1
    assert landed[0].rung == RUNG_MERGE_FALLBACK
    assert _rev(repo, "main") != base_before  # base advanced out-of-tree
    assert _is_ancestor(repo, branch_head, "main")


# --- criterion 2 + 7: a grader that fails on the merged tree parks ------------


def test_merge_fallback_grader_fail_on_merged_tree_parks(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _merge_submitter(repo, ledger)
    # A grader that passes on the branch tree alone (no poison.txt) but fails on
    # the merged tree, which the advanced base poisons. If receipts were
    # recycled from the original attempt rather than run against the candidate
    # tree, this grader would pass and the branch would land.
    tf = _task_file(repo, "01-phase", "t1", run="test ! -f poison.txt")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _merge_bearing_branch(wt)
    # The base advance both forces the rebase conflict (file.txt) and poisons
    # the merged tree (poison.txt), so the merge is clean but re-verify fails.
    _write_commit(
        repo,
        {"file.txt": "line1-main\n", "poison.txt": "boom\n"},
        "base advances and poisons",
    )
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, run="test ! -f poison.txt"))

    # Criterion 2: the base ref is byte-identical before and after, and no
    # Landed record exists -- unverified merged content did not land.
    assert _rev(repo, "main") == base_before
    assert ledger.landed() == []
    # Criterion 7: a fresh LandingParked witness is appended (the re-driver
    # counts these toward its bound), under a queryable park kind.
    parked = ledger.parked()
    assert len(parked) == 1
    assert parked[0].park_kind == PARK_KIND_DIVERGENT_BASE
    assert parked[0].park_kind in LANDING_PARK_KINDS
    # The park carries the deciding grader's receipt, run against the candidate
    # tree -- a failing check, not a recycled pass.
    assert parked[0].receipts
    assert any(not r.passed for r in parked[0].receipts)
    # The branch+worktree survive for the recovery re-drive (D-3).
    assert s._branch_exists("flywheel/01-phase/t1")
    assert wt.is_dir()


# --- criterion 3: a merge conflict aborts cleanly and parks -------------------


def test_merge_fallback_conflict_parks_and_preserves_worktree(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    ledger = _RecordingLedger()
    s = _merge_submitter(repo, ledger)
    tf = _task_file(repo, "01-phase", "t1", run="true")

    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    # A single un-reverted edit: the branch's surviving change to file.txt
    # collides with the base's change on both the rebase and the merge.
    _write_commit(wt, {"file.txt": "line1-branch\n"}, "edit file.txt")
    branch_head = _rev(wt, "HEAD")
    _write_commit(repo, {"file.txt": "line1-main\n"}, "base advances")
    base_before = _rev(repo, "main")

    s.submit(_submit_req(tf, "t1", wt, run="true"))

    # Criterion 3: nothing landed and the base ref is untouched.
    assert ledger.landed() == []
    assert _rev(repo, "main") == base_before
    # The park is on the ledger under the queryable merge-conflict kind.
    parked = ledger.parked()
    assert len(parked) == 1
    assert parked[0].park_kind == PARK_KIND_MERGE_CONFLICT
    assert parked[0].park_kind in LANDING_PARK_KINDS
    # The branch ref still resolves and its worktree survives.
    assert _rev(repo, "flywheel/01-phase/t1") == branch_head
    assert wt.is_dir()
    # The merge aborted cleanly: no in-progress merge state, no unmerged paths.
    assert (
        subprocess.run(
            ["git", "-C", str(wt), "rev-parse", "-q", "--verify", "MERGE_HEAD"],
            capture_output=True,
        ).returncode
        != 0
    )
    assert _git(wt, "status", "--porcelain") == ""
