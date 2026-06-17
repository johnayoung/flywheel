"""Held-out acceptance test (spec 00026, criterion 3): a detached-HEAD operator
checkout with a configured base starts and lands without aborting.

When the operator checkout is in detached-HEAD state and ``[submit] base`` is
configured, base resolution must NOT abort on the checkout state; the worker
lands the task onto the configured base, and the operator's HEAD stays detached
at its original SHA.

Authored blind from the contract (D-1). Drives the named base-resolution
entrypoint with the resolved policy (not ``_phase_base``). Discriminators: a
no-crash fix that resolves to the literal ``"HEAD"`` or the detached SHA fails
the "base ref advanced to include the task commit" assertion.

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
        run_id="run-detached-1",
        status=Status.DONE,
        sandbox=sandbox,
    )


def test_detached_head_lands_on_configured_base(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)
    # A configured base branch the operator does not have checked out.
    _git(repo, "branch", "landing-base", "main")
    # Detach the operator checkout.
    _git(repo, "checkout", "--detach")
    detached_sha_before = _git(repo, "rev-parse", "HEAD")
    assert _git(repo, "rev-parse", "--abbrev-ref", "HEAD") == "HEAD"  # detached
    base_before = _git(repo, "rev-parse", "landing-base")

    policy = WorkPolicy(source_kind="directory", submit_base="landing-base")

    # (a) Base resolution completes without aborting on the detached checkout
    #     and yields the configured base (no SystemExit, no literal "HEAD").
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

    # (b) The configured base advanced to include the task commit.
    assert _git(repo, "rev-parse", "landing-base") != base_before
    ancestor = subprocess.run(
        ["git", "-C", str(repo), "merge-base", "--is-ancestor", task_commit, "landing-base"],
        capture_output=True,
    )
    assert ancestor.returncode == 0

    # (c) HEAD is still detached at its original SHA — the operator checkout was
    #     never moved onto the base.
    assert _git(repo, "rev-parse", "--abbrev-ref", "HEAD") == "HEAD"
    assert _git(repo, "rev-parse", "HEAD") == detached_sha_before
