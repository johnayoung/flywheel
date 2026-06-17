import json
import subprocess
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import SandboxRequest, SubmitRequest
from flywheel_worktree import worker


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


def _submitter_for_base(repo: Path, phase_base: str) -> "worker.GitWorktreeSubmitter":
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base=phase_base,
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
    )


def _task_file(repo: Path, phase: str, task_id: str, *, grader: str = "true") -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        json.dumps(
            {
                "id": task_id,
                "goal": f"Goal for {task_id}.",
                "graders": [{"type": "command", "run": grader}],
            }
        )
    )
    return tf


def _sandbox_req(tf: Path, task_id: str, mode: str = "fresh") -> SandboxRequest:
    return SandboxRequest(
        task_id=task_id,
        task_file=tf,
        run_id=None if mode == "fresh" else "run-1",
        mode=mode,  # type: ignore[arg-type]
    )


def _submit_req(tf, task_id, sandbox, status, *, grader="true") -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run=grader)],
        ),
        run_id="run-1",
        status=status,
        sandbox=sandbox,
    )


def test_landing_does_not_touch_operator_worktree(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)

    # Configured landing base is a branch the operator does NOT have checked out.
    _git(repo, "branch", "landing-base", "main")
    # Operator works on a THIRD branch, distinct from the base.
    _git(repo, "checkout", "-b", "op-work")

    # Dirty a TRACKED sentinel file in the operator's repo_root working tree.
    sentinel = repo / "README.md"
    sentinel.write_text("operator uncommitted sentinel edit\n")

    base_before = _rev(repo, "landing-base")

    # Provision the sandbox and play the agent: one task commit ahead of base.
    # (Sandbox + task-file scaffolding live under repo_root/.flywheel; provision
    # BEFORE snapshotting the operator state so the landing -- not the test's own
    # scaffolding -- is the only thing that could change the operator checkout.)
    s = _submitter_for_base(repo, phase_base="landing-base")
    tf = _task_file(repo, "01-phase", "t1")
    wt = s.prepare_sandbox(_sandbox_req(tf, "t1"))
    _commit(wt, "feature.txt", "task work\n", "feat: task commit")
    task_commit = _rev(wt, "HEAD")

    # Capture the operator checkout state immediately BEFORE the landing -- the
    # `submit` call is the only operation under test.
    head_before = _rev(repo, "HEAD")
    porcelain_before = _git(repo, "status", "--porcelain")
    sentinel_bytes_before = sentinel.read_bytes()
    # The sentinel edit must actually be visible to git (precondition for the test).
    assert "README.md" in porcelain_before

    # Full landing of a DONE branch targeting `landing-base`. Must not raise.
    s.submit(_submit_req(tf, "t1", wt, Status.DONE))

    # Half 1: the operator checkout is byte-for-byte unchanged.
    assert _rev(repo, "HEAD") == head_before
    assert _git(repo, "status", "--porcelain") == porcelain_before
    assert sentinel.read_bytes() == sentinel_bytes_before
    # HEAD still points at the operator's branch, not the base.
    assert _git(repo, "rev-parse", "--abbrev-ref", "HEAD") == "op-work"

    # Half 2: the configured (non-checked-out) base ref advanced to include the
    # task commit.
    base_after = _rev(repo, "landing-base")
    assert base_after != base_before
    # The task commit is reachable from the advanced base.
    ancestor = subprocess.run(
        ["git", "-C", str(repo), "merge-base", "--is-ancestor", task_commit, "landing-base"],
        capture_output=True,
    )
    assert ancestor.returncode == 0
