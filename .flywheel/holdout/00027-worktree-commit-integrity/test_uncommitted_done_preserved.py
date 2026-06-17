"""Held-out acceptance test for 00027-worktree-commit-integrity.

CRITERION: when a `done` run is submitted with a dirty worktree (an uncommitted
modified or untracked file present at submit time), the task branch and worktree
BOTH still exist after `submit` returns.

Discriminating assertion (which the visible suite's park test does NOT make):
the task branch ref still exists, proven via
`git -C <repo> show-ref --verify --quiet refs/heads/flywheel/<phase>/<task-id>`
exiting 0. This forecloses a fix that "surfaces" the dirty-DONE by logging while
still running `_cleanup`/`branch -D`, which would discard recoverable work.

Authored BLIND to the implementation, from the contract only.
"""

import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import SandboxRequest, SubmitRequest
from flywheel_worktree import worker


def _git(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    ).stdout.strip()


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "worker-test@example.com")
    _git(path, "config", "user.name", "worker test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _submitter(repo: Path) -> "worker.GitWorktreeSubmitter":
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


def test_submit_uncommitted_done_preserved(tmp_path: Path) -> None:
    """A dirty-tree DONE submit preserves BOTH the worktree and the branch ref.

    The task file lives at tasks/active/01-phase/t1.json, so the phase is
    "01-phase" and the branch is "flywheel/01-phase/t1".
    """
    repo = tmp_path / "repo"
    _init_repo(repo)
    submitter = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    wt = submitter.prepare_sandbox(
        SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
    )
    assert wt == repo / ".flywheel" / "worktrees" / "t1"

    # Make the tree DIRTY: an untracked, uncommitted file at submit time.
    (wt / "dirty.txt").write_text("uncommitted")

    # submit() is the landing seam: it records its own outcome and swallows
    # errors, so it MUST NOT raise and returns None.
    result = submitter.submit(
        SubmitRequest(
            task_id="t1",
            task_file=tf,
            task=Task(
                id="t1",
                goal="Goal for t1.",
                graders=[CommandGrader(run="true")],
            ),
            run_id="run-1",
            status=Status.DONE,
            sandbox=wt,
        )
    )
    assert result is None

    # Post-condition: a DIRTY tree at DONE preserves BOTH the worktree directory
    # and the task branch ref (not cleaned up, not branch -D'd).
    assert wt.exists()

    # Discriminating assertion: the branch ref still exists. show-ref --verify
    # --quiet exits 0 iff refs/heads/flywheel/01-phase/t1 resolves. Run WITHOUT
    # check=True and read the return code.
    show_ref = subprocess.run(
        [
            "git",
            "-C",
            str(repo),
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/flywheel/01-phase/t1",
        ],
        text=True,
        capture_output=True,
    )
    assert show_ref.returncode == 0
