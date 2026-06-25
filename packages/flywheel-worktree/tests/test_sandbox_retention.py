"""Held-out oracle for spec 00041 (sandbox retention, increment E of 00036).

RED until ``sandbox-retention`` lands. ``GitWorktreeSubmitter`` gains
``on_done`` (``destroy`` | ``preserve``) and ``on_failure`` (``park`` |
``destroy``) controlling submit-time teardown; the defaults (``destroy``/
``park``) reproduce today's behavior. Drives a real git repo + worktree (the
test plays the agent), mirroring test_worker.py. Do not weaken or delete
assertions.
"""

from __future__ import annotations

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
    _git(path, "config", "user.email", "t@example.com")
    _git(path, "config", "user.name", "t")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")


def _submitter(
    repo: Path, *, on_done: str = "destroy", on_failure: str = "park"
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
        on_done=on_done,
        on_failure=on_failure,
    )


def _task_file(repo: Path, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / "_root" / f"{task_id}.json"
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


def _submit_req(tf: Path, task_id: str, sandbox: Path, status: Status) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(id=task_id, goal="g", graders=[CommandGrader(run="true")]),
        run_id="run-1",
        status=status,
        sandbox=sandbox,
    )


def _provision_with_work(
    s: worker.GitWorktreeSubmitter, tf: Path, task_id: str, filename: str
) -> Path:
    wt = s.prepare_sandbox(
        SandboxRequest(task_id=task_id, task_file=tf, run_id=None, mode="fresh")
    )
    (wt / filename).write_text("x\n")
    _git(wt, "add", "-A")
    _git(wt, "commit", "-m", "work")
    return wt


def test_on_done_preserve_keeps_worktree_after_merge(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo, on_done="preserve")
    tf = _task_file(repo, "t1")
    wt = _provision_with_work(s, tf, "t1", "feature.txt")
    s.submit(_submit_req(tf, "t1", wt, Status.DONE))
    assert (repo / "feature.txt").exists()  # the work still merged
    assert wt.exists()  # ... but the worktree is preserved for inspection


def test_on_done_destroy_is_the_default(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)  # default on_done="destroy"
    tf = _task_file(repo, "t1")
    wt = _provision_with_work(s, tf, "t1", "feature.txt")
    s.submit(_submit_req(tf, "t1", wt, Status.DONE))
    assert (repo / "feature.txt").exists()
    assert not wt.exists()  # today's behavior: cleaned on success


def test_on_failure_destroy_removes_worktree(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo, on_failure="destroy")
    tf = _task_file(repo, "t1")
    wt = _provision_with_work(s, tf, "t1", "feature.txt")
    s.submit(_submit_req(tf, "t1", wt, Status.FAILED))
    assert not wt.exists()  # ephemeral: failed worktree destroyed, no forensics


def test_on_failure_park_is_the_default(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    s = _submitter(repo)  # default on_failure="park"
    tf = _task_file(repo, "t1")
    wt = _provision_with_work(s, tf, "t1", "feature.txt")
    s.submit(_submit_req(tf, "t1", wt, Status.FAILED))
    assert wt.exists()  # today's behavior: parked for forensics
