"""Held-out acceptance test (spec 00027, criterion 2): the worker establishes a
DETERMINISTIC commit identity on every worktree it provisions.

BLIND test -- authored only from the contract, never from the implementation.

Criterion under test
---------------------
While the worker provisions worktrees with the identity it establishes, the
committer/author name and email it establishes are a single FIXED, DETERMINISTIC
value -- identical across two independently provisioned worktrees in the SAME
repo -- and NOT derived from a random/UUID/timestamp/per-run value.

Discriminating relation (metamorphic / equality)
-------------------------------------------------
Provision TWO distinct worktrees (task ids ``t1`` and ``t2``) from the SAME
``GitWorktreeSubmitter`` against the SAME repo, under a pinned scrubbed-identity
environment in which NO identity is otherwise resolvable. Make exactly one commit
in each worktree, also under that scrubbed env (so the only identity present is
the one the worker established). Read the identity quad
``git -C <wt> log -1 --format=%an|%ae|%cn|%ce`` from each and assert the two
strings are BYTE-IDENTICAL and NON-EMPTY.

This kills:
  * a per-worktree / per-run random or UUID identity (the two quads differ),
  * a per-provisioning timestamp identity (the two quads differ),
  * an empty/unset identity (the commit fails for "Author identity unknown",
    or, if forced, lands an empty name/email -> the non-empty check bites).

Only a fixed deterministic name+email, established identically on every
provisioned worktree, satisfies both halves of the relation.

Scrubbed-environment definition (binding; identical to criterion 1)
-------------------------------------------------------------------
A fresh ``os.environ``-derived env with ``HOME`` -> an empty temp dir,
``XDG_CONFIG_HOME`` -> an empty temp dir, ``GIT_CONFIG_NOSYSTEM="1"``. Any
inherited ``GIT_AUTHOR_*`` / ``GIT_COMMITTER_*`` (and the legacy ``EMAIL`` /
``GIT_CONFIG_GLOBAL`` / ``GIT_CONFIG_SYSTEM``) are stripped so they cannot leak
an ambient identity onto the under-test commit path. This env is used for BOTH
``prepare_sandbox`` provisioning AND both in-worktree commits. The test never
sets ``user.name`` / ``user.email`` in either worktree or in the repo config,
and never injects ``-c user.*`` or ``GIT_AUTHOR_*`` on the under-test commits.

Base-repo identity isolation
----------------------------
The base repo's initial commit needs *some* identity, but that identity must NOT
be resolvable on the path the under-test commits read. We therefore create the
base commit with env-injected ``GIT_AUTHOR_*`` / ``GIT_COMMITTER_*`` that we do
NOT pass to the under-test commits, and we never write ``user.name`` /
``user.email`` into the repo's config.

Scrubbing HOME/XDG/system config is necessary but not sufficient: git still
synthesises a fallback identity from the OS user's gecos name and a
``user@hostname`` email. That fallback is an ambient identity the worker did NOT
establish, so we suppress it with ``user.useConfigOnly=true`` -- a *suppression*
knob (git then refuses to guess and hard-fails a commit when no identity is
configured), NOT a ``user.name`` / ``user.email`` value. With it set, the only
identity any under-test commit can resolve is one the worker explicitly
established. The test asserts this isolation directly: before exercising the
worker, a commit on the clean base path must fail (no resolvable identity).
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

from flywheel_orchestrator import SandboxRequest
from flywheel_worktree import worker

PHASE = "01-phase"
QUAD_FORMAT = "%an|%ae|%cn|%ce"


# --- scrubbed environment ---------------------------------------------------


def _scrubbed_env(home: Path, xdg: Path) -> dict[str, str]:
    """A fresh os.environ-derived env with no resolvable git identity.

    HOME and XDG_CONFIG_HOME point at empty temp dirs (so no ~/.gitconfig or
    ~/.config/git/config is visible) and GIT_CONFIG_NOSYSTEM disables /etc.
    Any inherited author/committer identity is stripped so the only identity
    the under-test commit can resolve is the one the worker established.
    """
    env = dict(os.environ)
    for key in list(env):
        if key.startswith(("GIT_AUTHOR_", "GIT_COMMITTER_")):
            env.pop(key, None)
    for key in ("EMAIL", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"):
        env.pop(key, None)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(xdg)
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    return env


# --- git helpers ------------------------------------------------------------


def _git(
    cwd: Path,
    *args: str,
    env: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(cwd), *args],
        text=True,
        capture_output=True,
        check=check,
        env=env,
    )


def _init_base_repo(repo: Path, scrubbed: dict[str, str]) -> None:
    """Initialise a repo and create its first commit WITHOUT leaving any
    identity resolvable on the under-test commit path.

    The seed identity is injected purely via GIT_AUTHOR_*/GIT_COMMITTER_* env
    vars on this one commit; it is never written to the repo config and never
    passed to the under-test commits.
    """
    repo.mkdir(parents=True, exist_ok=True)
    _git(repo, "init", "-b", "main", env=scrubbed)
    _git(repo, "config", "commit.gpgsign", "false", env=scrubbed)
    # Suppress git's gecos/hostname identity fallback so the only resolvable
    # identity is one explicitly established (not git's user@hostname guess).
    # This is a suppression knob, not a user.name/user.email value.
    _git(repo, "config", "user.useConfigOnly", "true", env=scrubbed)
    (repo / "README.md").write_text("base\n")
    _git(repo, "add", "-A", env=scrubbed)
    seed = dict(scrubbed)
    seed.update(
        GIT_AUTHOR_NAME="base-seed",
        GIT_AUTHOR_EMAIL="base-seed@example.invalid",
        GIT_COMMITTER_NAME="base-seed",
        GIT_COMMITTER_EMAIL="base-seed@example.invalid",
    )
    _git(repo, "commit", "-m", "init", env=seed)

    # Guard: the repo must carry NO local user identity -- otherwise the
    # under-test commit could resolve identity from config rather than from
    # whatever the worker established, defeating the criterion.
    assert (
        _git(repo, "config", "--local", "--get", "user.name", check=False).stdout.strip()
        == ""
    ), "repo-local user.name leaked an ambient identity onto the under-test path"
    assert (
        _git(repo, "config", "--local", "--get", "user.email", check=False).stdout.strip()
        == ""
    ), "repo-local user.email leaked an ambient identity onto the under-test path"

    # Isolation proof: on this clean base, with no established identity, a
    # commit MUST fail. If it can succeed here, some ambient identity is
    # resolvable and the criterion cannot be discriminated -> under-specified
    # setup. This is the contract's "no identity resolvable on the path"
    # caveat, asserted directly.
    (repo / "README.md").write_text("base\nisolation-probe\n")
    _git(repo, "add", "-A", env=scrubbed)
    probe = _git(repo, "commit", "-m", "isolation probe", env=scrubbed, check=False)
    assert probe.returncode != 0, (
        "an identity was resolvable on the clean base path without the worker "
        "establishing one -- the test environment cannot isolate the "
        "worker-established identity"
    )
    # Roll the staged probe back so the repo is pristine for provisioning.
    _git(repo, "reset", "--hard", "HEAD", env=scrubbed)


def _submitter(repo: Path) -> "worker.GitWorktreeSubmitter":
    worktrees = repo / ".flywheel" / "worktrees"
    worktrees.mkdir(parents=True, exist_ok=True)  # worktrees_dir must exist
    return worker.GitWorktreeSubmitter(
        repo_root=repo,
        tasks_dir=repo / ".flywheel" / "tasks",
        worktrees_dir=worktrees,
        phase_base="main",
        lock_path=repo / ".flywheel" / ".merge.lock",
        log=lambda _m: None,
    )


def _task_file(repo: Path, task_id: str) -> Path:
    tf = repo / ".flywheel" / "tasks" / "active" / PHASE / f"{task_id}.json"
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


def _provision(
    submitter: "worker.GitWorktreeSubmitter",
    repo: Path,
    task_id: str,
    scrubbed: dict[str, str],
) -> Path:
    """Provision one worktree under the scrubbed env and return its path.

    ``prepare_sandbox`` runs under the same scrubbed identity environment so
    that whatever identity the worker establishes is established *without* an
    ambient identity to fall back on.
    """
    tf = _task_file(repo, task_id)
    prior_environ = dict(os.environ)
    os.environ.clear()
    os.environ.update(scrubbed)
    try:
        wt = submitter.prepare_sandbox(
            SandboxRequest(
                task_id=task_id,
                task_file=tf,
                run_id=None,
                mode="fresh",
            )
        )
    finally:
        os.environ.clear()
        os.environ.update(prior_environ)
    assert wt == repo / ".flywheel" / "worktrees" / task_id
    assert wt.is_dir()
    return wt


def _commit_in_worktree(wt: Path, task_id: str, scrubbed: dict[str, str]) -> None:
    """Make exactly one commit in the worktree under the scrubbed env.

    No identity is injected by the test (no ``-c user.*``, no GIT_AUTHOR_*),
    so the commit can only resolve identity from whatever the worker
    established on this worktree.
    """
    (wt / f"{task_id}.txt").write_text(f"work for {task_id}\n")
    _git(wt, "add", "-A", env=scrubbed)
    _git(wt, "commit", "-m", f"work {task_id}", env=scrubbed)


def _identity_quad(wt: Path, scrubbed: dict[str, str]) -> str:
    return _git(
        wt, "log", "-1", f"--format={QUAD_FORMAT}", env=scrubbed
    ).stdout.strip()


# --- the held-out test ------------------------------------------------------


def test_worker_establishes_deterministic_identity_across_worktrees(
    tmp_path: Path,
) -> None:
    home = tmp_path / "home"
    xdg = tmp_path / "xdg"
    home.mkdir()
    xdg.mkdir()
    scrubbed = _scrubbed_env(home, xdg)

    repo = tmp_path / "repo"
    _init_base_repo(repo, scrubbed)

    submitter = _submitter(repo)

    # Two distinct worktrees in the SAME repo, from the SAME submitter, both
    # provisioned under the scrubbed env.
    wt1 = _provision(submitter, repo, "t1", scrubbed)
    wt2 = _provision(submitter, repo, "t2", scrubbed)
    assert wt1 != wt2

    # One commit in each, relying only on the worker-established identity.
    _commit_in_worktree(wt1, "t1", scrubbed)
    _commit_in_worktree(wt2, "t2", scrubbed)

    quad1 = _identity_quad(wt1, scrubbed)
    quad2 = _identity_quad(wt2, scrubbed)

    # Non-empty: every one of author-name/author-email/committer-name/
    # committer-email must be present (kills an empty/unset identity).
    fields1 = quad1.split("|")
    assert len(fields1) == 4, f"unexpected identity quad shape: {quad1!r}"
    assert all(f.strip() for f in fields1), (
        f"worker established an empty identity field: {quad1!r}"
    )

    # Deterministic: byte-identical across the two independently provisioned
    # worktrees (kills a per-worktree/per-run random, UUID, or timestamp
    # identity, which would differ between the two).
    assert quad1 == quad2, (
        "worker-established commit identity differs across two worktrees in the "
        f"same repo -- it is not deterministic: {quad1!r} != {quad2!r}"
    )
