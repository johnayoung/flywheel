"""Held-out acceptance test for criterion 00027: a CLEAN ``done`` run carrying
commits beyond base, on a fresh identity-less host, FF-merges into the base and
cleans up the worktree + branch.

Authored BLIND to the implementation, from the observable contract only.

This test composes three facts under the *pinned scrubbed environment* (HOME and
XDG_CONFIG_HOME pointing at empty temp dirs, ``GIT_CONFIG_NOSYSTEM=1``):

  1. The in-worktree commit, made WITHOUT setting any identity in the worktree or
     repo config, succeeds (exit 0) -- it can only succeed via the identity the
     worker established during ``prepare_sandbox``. (Criterion 1 is its own
     held-out test; here we *rely* on that identity.)
  2. ``submit`` with ``Status.DONE`` FF-merges the task branch so
     ``git -C <repo> rev-list --count main`` advances by exactly the number of
     commits made beyond base.
  3. The worktree directory is gone and the task branch ref is absent
     (``git show-ref --verify --quiet refs/heads/flywheel/<phase>/<task>`` exits
     non-zero).

The base repo's own commits are authored via env-injected GIT_AUTHOR_*/
GIT_COMMITTER_* that are NOT passed to the in-worktree commit, and no repo-local
``user.name``/``user.email`` is ever set -- so the only identity resolvable on the
path the under-test commit reads is whatever the worker established. That is what
makes this prove the scrubbed-host path rather than an ambient-config accident.

Lives outside the configured testpaths; collected only when its path is passed
explicitly::

    uv run pytest .flywheel/holdout/00027-worktree-commit-integrity/ \
        -k clean_commit_ff_merges
"""

from __future__ import annotations

import os
import subprocess
import tempfile
from pathlib import Path

from flywheel_core import CommandGrader, Status, Task
from flywheel_orchestrator import SandboxRequest, SubmitRequest
from flywheel_worktree import worker


# --- pinned scrubbed environment (binding; identical to criterion 1) ---------


def _scrubbed_env(tmp_path: Path) -> dict[str, str]:
    """A fresh ``os.environ``-derived env with HOME and XDG_CONFIG_HOME pointing
    at empty temp dirs and ``GIT_CONFIG_NOSYSTEM=1``. Any ambient identity
    variables inherited from the real environment are dropped so the *only*
    identity resolvable on the in-worktree commit path is the one the worker
    establishes during provisioning."""
    home = Path(tempfile.mkdtemp(dir=tmp_path, prefix="scrub-home-"))
    xdg = Path(tempfile.mkdtemp(dir=tmp_path, prefix="scrub-xdg-"))
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(xdg)
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    for key in (
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_DATE",
    ):
        env.pop(key, None)
    return env


# --- git helpers -------------------------------------------------------------


def _git(
    cwd: Path,
    *args: str,
    env: dict[str, str],
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        env=env,
        check=check,
    )


def _git_out(cwd: Path, *args: str, env: dict[str, str]) -> str:
    return _git(cwd, *args, env=env).stdout.strip()


def _init_repo(repo: Path, env: dict[str, str]) -> None:
    """Initialise the base repo. The initial commit is authored via
    env-injected GIT_AUTHOR_*/GIT_COMMITTER_* that we do NOT hand to the
    under-test commit; crucially we never write repo-local ``user.name`` /
    ``user.email``, so no ambient identity survives for the in-worktree commit
    to read."""
    repo.mkdir(parents=True, exist_ok=True)
    _git(repo, "init", "-b", "main", env=env)
    _git(repo, "config", "commit.gpgsign", "false", env=env)

    author_env = dict(env)
    author_env.update(
        GIT_AUTHOR_NAME="base setup",
        GIT_AUTHOR_EMAIL="base-setup@example.com",
        GIT_COMMITTER_NAME="base setup",
        GIT_COMMITTER_EMAIL="base-setup@example.com",
    )
    (repo / "README.md").write_text("base\n")
    _git(repo, "add", "-A", env=author_env)
    _git(repo, "commit", "-m", "init", env=author_env)

    # Guard the premise: no repo-local identity is resolvable. If this ever
    # fails, the test would be silently passing for the wrong reason.
    assert (
        _git(repo, "config", "--local", "--get", "user.email", env=env, check=False).returncode
        != 0
    )
    assert (
        _git(repo, "config", "--local", "--get", "user.name", env=env, check=False).returncode
        != 0
    )


def _task_file(repo: Path, phase: str, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / phase / f"{task_id}.json"
    tf.parent.mkdir(parents=True, exist_ok=True)
    tf.write_text(
        '{"id": "%s", "goal": "Goal for %s.", '
        '"graders": [{"type": "command", "run": "true"}]}' % (task_id, task_id)
    )
    return tf


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


# --- the discriminating test -------------------------------------------------


def test_clean_commit_ff_merges_on_scrubbed_host(tmp_path: Path) -> None:
    env = _scrubbed_env(tmp_path)
    repo = tmp_path / "repo"
    _init_repo(repo, env)

    s = _submitter(repo)
    tf = _task_file(repo, "01-phase", "t1")

    # Provision under the scrubbed environment. The worker establishes the
    # commit identity here (criterion 1); we rely on it below.
    env_before = dict(os.environ)
    try:
        os.environ.clear()
        os.environ.update(env)
        wt = s.prepare_sandbox(
            SandboxRequest(task_id="t1", task_file=tf, run_id=None, mode="fresh")
        )
        assert wt == repo / ".flywheel" / "worktrees" / "t1"

        # (1) Make TWO real commits inside the worktree via the
        # worker-established identity. We never set user.name/user.email here;
        # if the worker failed to establish identity, these commits abort
        # (git exits 128: "empty ident name"), and check=True surfaces it.
        commit_count = 0
        for i in range(2):
            (wt / f"feature-{i}.txt").write_text(f"content {i}\n")
            _git(wt, "add", "-A", env=env)
            done = _git(
                wt,
                "commit",
                "-m",
                f"feat {i}",
                env=env,
                check=False,
            )
            # The commit MUST succeed (exit 0) for the FF-merge to be
            # exercised -- this is the worker-established-identity path.
            assert done.returncode == 0, (
                "in-worktree commit failed on a scrubbed host -- the worker did "
                f"not establish a usable git identity: {done.stderr}"
            )
            commit_count += 1

        # N = commits made beyond base; base count BEFORE submit.
        base_count_before = int(
            _git_out(repo, "rev-list", "--count", "main", env=env)
        )

        # Submit a DONE run. The grader is trivially-passing ("true") so a
        # re-verify gate, if any, cannot block the merge for an unrelated
        # reason. submit() must not raise and returns None.
        result = s.submit(
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
    finally:
        os.environ.clear()
        os.environ.update(env_before)

    # (2) FF-merge advanced the base by EXACTLY the commit count.
    base_count_after = int(_git_out(repo, "rev-list", "--count", "main", env=env))
    assert base_count_after == base_count_before + commit_count

    # (3a) The worktree directory is gone.
    assert not wt.exists()

    # (3b) The task branch ref is absent (show-ref exits non-zero).
    show_ref = _git(
        repo,
        "show-ref",
        "--verify",
        "--quiet",
        "refs/heads/flywheel/01-phase/t1",
        env=env,
        check=False,
    )
    assert show_ref.returncode != 0
