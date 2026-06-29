"""Tests for the strategy-supplied landability predicate (spec 00061, layer
predicate-seam).

:meth:`GitWorktreeSubmitter.is_landable` is a read-only inspection: given a
finished run it reports whether the sandbox holds a committed, non-empty change
against the branch's base, reusing the same ``git status --porcelain`` +
``git rev-list --count base..branch`` checks ``submit`` lands on. These tests
pin the three structural verdicts (committed diff -> landable; clean-but-empty
-> not landable; dirty tree -> not landable), prove the probe never mutates the
worktree/branch/base, and confirm the PR strategy inherits the same predicate.

The orchestrator wiring (the gate that acts on the verdict) is a later layer and
is out of scope here.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import LandabilityVerdict, SandboxRequest, SubmitRequest
from flywheel_worktree import worker
from flywheel_worktree.pr import GitPullRequestSubmitter


# --- git helpers (mirror test_worker.py) ------------------------------------


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
    _git(path, "config", "user.email", "worker-test@example.com")
    _git(path, "config", "user.name", "worker test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _rev(repo: Path, ref: str) -> str:
    return _git(repo, "rev-parse", ref)


def _commit(worktree: Path, filename: str, body: str, message: str) -> None:
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    _git(worktree, "commit", "-m", message)


def _submitter(repo: Path) -> worker.GitWorktreeSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
    )


def _pr_submitter(repo: Path) -> GitPullRequestSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return GitPullRequestSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        gh=lambda _argv: "",  # never invoked by is_landable
    )


def _task_file(repo: Path, phase: str, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": "true"}],
            }
        )
    )
    return tf


def _submit_req(tf: Path, task_id: str, sandbox: Path) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run="true")],
        ),
        run_id="run-1",
        status=Status.DONE,
        sandbox=sandbox,
    )


# --- verdicts ----------------------------------------------------------------


def test_committed_nonempty_diff_is_landable(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, "feature.txt", "x", "feat")

    verdict = s.is_landable(_submit_req(tf, "t1", wt))

    assert verdict == LandabilityVerdict(landable=True)


def test_clean_tree_zero_commits_is_not_landable(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    # Clean worktree, no commits beyond base -> empty diff.
    verdict = s.is_landable(_submit_req(tf, "t1", wt))

    assert verdict.landable is False
    assert "main" in verdict.reason  # names the base it found no commits beyond


def test_dirty_tree_is_not_landable(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    (wt / "dirty.txt").write_text("uncommitted")  # untracked, never committed

    verdict = s.is_landable(_submit_req(tf, "t1", wt))

    assert verdict.landable is False
    assert "uncommitted" in verdict.reason


def test_dirty_tree_takes_precedence_over_commits(tmp_path: Path) -> None:
    # A branch with a real commit but also an uncommitted edit is not landable:
    # the dirty-tree check fires first (the agent left work uncommitted).
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, "feature.txt", "x", "feat")
    (wt / "feature.txt").write_text("x-modified-but-not-committed")

    verdict = s.is_landable(_submit_req(tf, "t1", wt))

    assert verdict.landable is False
    assert "uncommitted" in verdict.reason


# --- read-only: the probe must not mutate ------------------------------------


def test_is_landable_does_not_mutate_worktree_branch_or_base(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    _commit(wt, "feature.txt", "x", "feat")
    base_before = _rev(repo, "main")
    branch_before = _rev(repo, "flywheel/01-phase/t1")

    verdict = s.is_landable(_submit_req(tf, "t1", wt))

    # Landable, but nothing landed/parked/cleaned: worktree, branch, and base
    # are byte-for-byte where they were. submit() owns the mutation.
    assert verdict.landable is True
    assert wt.exists()
    assert s._branch_exists("flywheel/01-phase/t1")
    assert _rev(repo, "main") == base_before
    assert _rev(repo, "flywheel/01-phase/t1") == branch_before
    assert (repo / "feature.txt").exists() is False  # never merged into base


# --- PR strategy inherits the same predicate ---------------------------------


def test_pr_strategy_inherits_predicate(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _pr_submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )

    empty = s.is_landable(_submit_req(tf, "t1", wt))
    assert empty.landable is False

    _commit(wt, "feature.txt", "x", "feat")
    landable = s.is_landable(_submit_req(tf, "t1", wt))
    assert landable == LandabilityVerdict(landable=True)
