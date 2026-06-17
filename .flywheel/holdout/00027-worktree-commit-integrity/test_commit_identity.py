"""Held-out acceptance test for worktree commit-identity provisioning.

Criterion: when a worktree is provisioned by the worker's prepare path on a host
with NO global/system git identity resolvable, a plain `git commit` of a working-tree
change INSIDE that worktree must exit 0 and produce one new commit.

This file lives outside the configured testpaths; collect it explicitly:
    uv run pytest .flywheel/holdout/00027-worktree-commit-integrity/ -k commit_identity
"""

import json
import os
import subprocess
from pathlib import Path

from flywheel_orchestrator import SandboxRequest
from flywheel_worktree import worker


# Identity-bearing env keys that, if inherited from the dev's ambient environment,
# would let a commit succeed WITHOUT the worker having established any identity on
# the worktree -- defeating the discrimination. The criterion binds on HOME /
# XDG_CONFIG_HOME / GIT_CONFIG_NOSYSTEM, but its stated intent ("NO global and NO
# system git identity is resolvable on either path") requires these env-var identity
# sources to be absent too, so the only identity the under-test commit can see is the
# one provisioning put on the worktree.
_IDENTITY_ENV_KEYS = (
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_DATE",
    "EMAIL",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_COUNT",
)


def _scrubbed_env(home: Path, xdg: Path) -> dict[str, str]:
    """The pinned scrubbed environment, used for BOTH provisioning and the commit.

    Derived from os.environ with HOME -> empty dir, XDG_CONFIG_HOME -> empty dir,
    GIT_CONFIG_NOSYSTEM=1, and every ambient identity source removed so no
    global/system identity is resolvable on either path.
    """
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(xdg)
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    for key in _IDENTITY_ENV_KEYS:
        env.pop(key, None)
    return env


def _git(cwd: Path, *args: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    """Run git without check=True so callers can inspect .returncode."""
    return subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        env=env,
    )


def _init_base_repo(repo: Path) -> None:
    """Build a base repo whose initial commit uses env-injected identity that is
    NOT passed to the behavior-under-test commit, so no identity is left resolvable
    on the scrubbed path the under-test commit reads.
    """
    repo.mkdir(parents=True, exist_ok=True)
    init = _git(repo, "init", "-b", "main")
    assert init.returncode == 0, init.stderr

    base_env = dict(os.environ)
    base_env.update(
        GIT_AUTHOR_NAME="Base Setup",
        GIT_AUTHOR_EMAIL="base-setup@example.invalid",
        GIT_COMMITTER_NAME="Base Setup",
        GIT_COMMITTER_EMAIL="base-setup@example.invalid",
    )
    (repo / "README.md").write_text("base\n")
    add = _git(repo, "add", "-A")
    assert add.returncode == 0, add.stderr
    commit = _git(repo, "commit", "-m", "initial commit on main", env=base_env)
    assert commit.returncode == 0, commit.stderr


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


def test_commit_identity_survives_scrubbed_environment(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    home = tmp_path / "empty_home"
    xdg = tmp_path / "empty_xdg"
    home.mkdir()
    xdg.mkdir()

    _init_base_repo(repo)

    scrubbed = _scrubbed_env(home, xdg)

    # Provision the worktree via the worker's prepare path under the SAME scrubbed
    # environment, by pointing the current process's environment at it for the call.
    # The submitter shells out to git; with the scrub in force here, no ambient
    # identity is resolvable during provisioning either.
    saved = {k: os.environ.get(k) for k in set(scrubbed) | set(os.environ)}
    try:
        os.environ.clear()
        os.environ.update(scrubbed)
        submitter = _submitter(repo)
        task_file = _task_file(repo, phase="01-phase", task_id="t1")
        worktree = submitter.prepare_sandbox(
            SandboxRequest(task_id="t1", task_file=task_file, run_id=None, mode="fresh")
        )
    finally:
        os.environ.clear()
        for key, value in saved.items():
            if value is not None:
                os.environ[key] = value

    assert isinstance(worktree, Path)
    assert worktree.is_dir(), f"worktree dir not provisioned: {worktree}"
    assert worktree == repo / ".flywheel" / "worktrees" / "t1"

    # Branch naming contract sanity check.
    branch = _git(worktree, "rev-parse", "--abbrev-ref", "HEAD")
    assert branch.returncode == 0, branch.stderr
    assert branch.stdout.strip() == "flywheel/01-phase/t1"

    # Count commits BEFORE the behavior-under-test commit (under scrubbed env).
    before = _git(worktree, "rev-list", "--count", "HEAD", env=scrubbed)
    assert before.returncode == 0, before.stderr
    before_count = int(before.stdout.strip())

    # Behavior under test: a working-tree change committed with a plain `git commit`,
    # run INSIDE the worktree under the SAME scrubbed environment. No identity is
    # resolvable except whatever provisioning established on the worktree.
    (worktree / "change.txt").write_text("a working-tree change\n")
    add = _git(worktree, "add", "-A", env=scrubbed)
    assert add.returncode == 0, add.stderr

    commit = _git(worktree, "commit", "-m", "x", env=scrubbed)
    assert commit.returncode == 0, (
        "git commit inside the provisioned worktree must exit 0 under the scrubbed "
        f"environment; got {commit.returncode}.\nstderr:\n{commit.stderr}"
    )

    after = _git(worktree, "rev-list", "--count", "HEAD", env=scrubbed)
    assert after.returncode == 0, after.stderr
    after_count = int(after.stdout.strip())

    assert after_count == before_count + 1, (
        f"expected HEAD commit count to increase by exactly 1 "
        f"(before={before_count}, after={after_count})"
    )
