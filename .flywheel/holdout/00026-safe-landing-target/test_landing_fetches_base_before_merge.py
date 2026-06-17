"""Held-out acceptance test (spec 00026, criterion 4): the worker fetches the
configured base fresh before landing.

When the base's remote has a commit C absent from the worker's local base ref,
base resolution must fetch the base fresh so the landing targets the up-to-date
base: the post-landing local base ref has BOTH C and the task commit as
ancestors.

Authored blind from the contract (D-3). C is created in a separate remote clone
and is uncomputable from the worker's own inputs, so it can only become an
ancestor of the landed base if a fresh fetch actually fed the landing.

Outside the four pytest testpaths; collected explicitly by the grader.
"""

import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import SandboxRequest, SubmitRequest, WorkPolicy
from flywheel_worktree import worker


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(cwd), *args], text=True, capture_output=True, check=True
    ).stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "op@example.invalid")
    _git(path, "config", "user.name", "op")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _is_ancestor(repo: Path, commit: str, ref: str) -> bool:
    return (
        subprocess.run(
            ["git", "-C", str(repo), "merge-base", "--is-ancestor", commit, ref],
            capture_output=True,
        ).returncode
        == 0
    )


def _task_file(repo: Path, phase: str, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {"id": task_id, "goal": "g.", "graders": [{"type": "command", "run": "true"}]}
        )
    )
    return tf


def _submit_req(tf: Path, task_id: str, sandbox: Path) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(id=task_id, goal="g.", graders=[CommandGrader(run="true")]),
        run_id="run-fetch-1",
        status=Status.DONE,
        sandbox=sandbox,
    )


def test_landing_fetches_base_before_merge(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    # A landing base the operator does not have checked out (operator stays on
    # main).
    _git(repo, "branch", "landing-base", "main")

    # A separate "origin" clone whose landing-base has a commit C the worker's
    # local landing-base ref does not yet have.
    origin = tmp_path / "origin"
    subprocess.run(
        ["git", "clone", str(repo), str(origin)], check=True, capture_output=True
    )
    _git(origin, "config", "user.email", "remote@example.invalid")
    _git(origin, "config", "user.name", "remote")
    _git(origin, "checkout", "landing-base")
    (origin / "remote_only.txt").write_text("commit C only on the remote base\n")
    _git(origin, "add", "-A")
    _git(origin, "commit", "-m", "C: remote-only base commit")
    commit_c = _git(origin, "rev-parse", "HEAD")

    _git(repo, "remote", "add", "origin", str(origin))
    # Precondition: C is NOT yet in the worker's local base ref.
    assert not _is_ancestor(repo, commit_c, "landing-base")

    policy = WorkPolicy(source_kind="directory", submit_base="landing-base")

    # Resolution fetches the base fresh -> the local base ref now has C.
    base = worker.resolve_landing_base(repo, policy)
    assert base == "landing-base"

    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    s = worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base=base,
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
    )
    tf = _task_file(repo, "01-phase", "t1")
    wt = s.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    (wt / "feature.txt").write_text("task work\n")
    _git(wt, "add", "-A")
    _git(wt, "commit", "-m", "feat: task commit")
    task_commit = _git(wt, "rev-parse", "HEAD")

    s.submit(_submit_req(tf, "t1", wt))

    # The landed base ref contains BOTH the remote-only commit C and the task
    # commit — only true if the fresh fetch actually fed the landing.
    assert _is_ancestor(repo, commit_c, "landing-base"), (
        "remote-only commit C is not an ancestor of the landed base: the base "
        "was not fetched fresh before landing"
    )
    assert _is_ancestor(repo, task_commit, "landing-base"), (
        "task commit is not an ancestor of the landed base"
    )
