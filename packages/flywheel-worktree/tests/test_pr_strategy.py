"""Tests for the PR landing strategy ``flywheel_worktree.pr``.

Real git against a tmp repo with a local bare remote (pushes are real);
the ``gh`` CLI is replaced by a recording fake through the runner seam.
"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Sequence
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import (
    GraderReceipt,
    SandboxRequest,
    SubmitRequest,
    SubmitStrategy,
)

from flywheel_worktree.pr import GitPullRequestSubmitter, render_pr_body


# --- git / fixture helpers ----------------------------------------------------


def _git(cwd: Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo_with_remote(path: Path) -> Path:
    """Init a work repo plus a bare ``origin`` next to it."""
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-b", "main")
    _git(path, "config", "user.email", "pr-test@example.com")
    _git(path, "config", "user.name", "pr test")
    _git(path, "config", "commit.gpgsign", "false")
    (path / "README.md").write_text("base\n")
    _git(path, "add", "-A")
    _git(path, "commit", "-m", "init")
    remote = path.parent / "origin.git"
    subprocess.run(
        ["git", "init", "--bare", str(remote)],
        capture_output=True,
        check=True,
    )
    _git(path, "remote", "add", "origin", str(remote))
    return remote


class _FakeGh:
    """Recording ``gh`` runner: ``pr list`` returns a canned URL (or
    nothing), ``pr create`` returns the new PR URL."""

    def __init__(self, existing_url: str = "") -> None:
        self.existing_url = existing_url
        self.calls: list[list[str]] = []

    def __call__(self, argv: Sequence[str]) -> str:
        argv = list(argv)
        self.calls.append(argv)
        if argv[:2] == ["pr", "list"]:
            return self.existing_url
        if argv[:2] == ["pr", "create"]:
            return "https://example.test/pr/7\n"
        return ""

    def commands(self) -> list[str]:
        return [" ".join(c[:2]) for c in self.calls]


def _submitter(
    repo: Path, gh: _FakeGh, **kwargs: object
) -> GitPullRequestSubmitter:
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)
    return GitPullRequestSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
        gh=gh,
        **kwargs,  # type: ignore[arg-type]
    )


def _task_file(repo: Path, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / "01-phase" / f"{task_id}.json"
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


def _commit(worktree: Path, filename: str, body: str, message: str) -> None:
    (worktree / filename).write_text(body)
    _git(worktree, "add", "-A")
    _git(worktree, "commit", "-m", message)


def _req(
    tf: Path, task_id: str, sandbox: Path, status: Status
) -> SubmitRequest:
    return SubmitRequest(
        task_id=task_id,
        task_file=tf,
        task=Task(
            id=task_id,
            goal=f"Goal for {task_id}.",
            graders=[CommandGrader(run="true")],
        ),
        run_id="run-1",
        status=status,
        sandbox=sandbox,
        receipts=(
            GraderReceipt(
                ordinal=0, grader_type="command", name="tests", passed=True
            ),
        ),
    )


def _remote_branch_exists(remote: Path, branch: str) -> bool:
    proc = subprocess.run(
        [
            "git",
            "-C",
            str(remote),
            "show-ref",
            "--verify",
            "--quiet",
            f"refs/heads/{branch}",
        ],
        capture_output=True,
        check=False,
    )
    return proc.returncode == 0


# --- landing ------------------------------------------------------------------


def test_done_pushes_branch_and_opens_pr(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    gh = _FakeGh()
    s = _submitter(repo, gh)
    tf = _task_file(repo, "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(
            task_id="t1", task_file=tf, run_id=None, mode="fresh"
        )
    )
    _commit(wt, "feature.txt", "x", "feat")
    base_before = _git(repo, "rev-parse", "main")

    s.submit(_req(tf, "t1", wt, Status.DONE))

    # Branch landed on the remote, PR opened, nothing merged locally.
    assert _remote_branch_exists(remote, "flywheel/01-phase/t1")
    assert gh.commands() == ["pr list", "pr create"]
    assert _git(repo, "rev-parse", "main") == base_before
    assert not (repo / "feature.txt").exists()
    # Local worktree + branch cleaned: the remote + PR hold the work now.
    assert not wt.exists()
    assert not s._branch_exists("flywheel/01-phase/t1")

    create = next(c for c in gh.calls if c[:2] == ["pr", "create"])
    body = create[create.index("--body") + 1]
    assert "Goal for t1." in body
    assert "run-1" in body
    assert "| 0 | command | tests | pass |" in body
    title = create[create.index("--title") + 1]
    assert title.startswith("t1:")
    base = create[create.index("--base") + 1]
    assert base == "main"


def test_done_with_open_pr_refreshes_body(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    gh = _FakeGh(existing_url="https://example.test/pr/3\n")
    s = _submitter(repo, gh)
    tf = _task_file(repo, "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(
            task_id="t1", task_file=tf, run_id=None, mode="fresh"
        )
    )
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_req(tf, "t1", wt, Status.DONE))

    # Re-landing a branch with an open PR edits it; never a duplicate.
    assert gh.commands() == ["pr list", "pr edit"]
    edit = gh.calls[1]
    assert edit[2] == "https://example.test/pr/3"


def test_failed_parks_without_push(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    gh = _FakeGh()
    s = _submitter(repo, gh)
    tf = _task_file(repo, "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(
            task_id="t1", task_file=tf, run_id=None, mode="fresh"
        )
    )
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_req(tf, "t1", wt, Status.FAILED))

    # Identical park semantics to the merge strategy: forensics stay local.
    assert wt.exists()
    assert not _remote_branch_exists(remote, "flywheel/01-phase/t1")
    assert gh.calls == []


def test_protected_path_refuses_pr(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    remote = _init_repo_with_remote(repo)
    gh = _FakeGh()
    s = _submitter(repo, gh, protected_paths=["conftest.py"])
    tf = _task_file(repo, "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(
            task_id="t1", task_file=tf, run_id=None, mode="fresh"
        )
    )
    _commit(wt, "conftest.py", "tampered", "edit conftest")

    s.submit(_req(tf, "t1", wt, Status.DONE))

    # Defense in depth: even with a human reviewing, tampered verification
    # surface never even reaches the PR stage.
    assert wt.exists()
    assert not _remote_branch_exists(remote, "flywheel/01-phase/t1")
    assert gh.calls == []


def test_push_failure_parks(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    gh = _FakeGh()
    # A remote that does not exist makes the real push fail.
    s = _submitter(repo, gh, remote="nonexistent")
    tf = _task_file(repo, "t1")

    wt = s.prepare_sandbox(
        SandboxRequest(
            task_id="t1", task_file=tf, run_id=None, mode="fresh"
        )
    )
    _commit(wt, "feature.txt", "x", "feat")

    s.submit(_req(tf, "t1", wt, Status.DONE))

    assert wt.exists()
    assert gh.calls == []


def test_pr_submitter_is_a_submit_strategy(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo_with_remote(repo)
    assert isinstance(_submitter(repo, _FakeGh()), SubmitStrategy)


# --- body rendering -----------------------------------------------------------


def test_render_pr_body_without_receipts(tmp_path: Path) -> None:
    req = SubmitRequest(
        task_id="t1",
        task_file=tmp_path / "t1.json",
        task=Task(id="t1", goal="G.", graders=[CommandGrader(run="true")]),
        run_id="run-9",
        status=Status.DONE,
        sandbox=tmp_path,
    )
    body = render_pr_body(req)
    assert "receipt projection unavailable" in body
    assert "agent claims are never authoritative" in body
