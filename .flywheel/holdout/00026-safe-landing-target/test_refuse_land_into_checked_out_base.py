"""Held-out acceptance test (spec 00026, criterion 2): the worker refuses to
land into the operator's currently-checked-out branch.

When the configured base (``[submit] base`` -> ``WorkPolicy.submit_base``) equals
the branch the operator has checked out in ``repo_root``, base resolution must
refuse rather than advance that branch: it raises a ``PolicyError``-class error
at startup (before any landing) and the branch's SHA is unchanged. The refusal
is read from git ref state and the raised error, never from stderr.

Authored blind from the contract (D-2). Discriminators: a name-equality check
that still runs ``git merge`` in ``repo_root`` would advance the checked-out
branch's SHA (caught); proceeding with only a stderr warning fails the raise.

Outside the four pytest testpaths; collected explicitly by the grader.
"""

import subprocess
from pathlib import Path

import pytest

from flywheel_orchestrator import PolicyError, WorkPolicy
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


def test_refuse_land_into_checked_out_base(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    _init_repo(repo)

    # The configured landing base is the operator's currently-checked-out branch.
    assert _git(repo, "rev-parse", "--abbrev-ref", "HEAD") == "main"
    policy = WorkPolicy(source_kind="directory", submit_base="main")

    head_before = _git(repo, "rev-parse", "HEAD")
    main_before = _git(repo, "rev-parse", "main")
    porcelain_before = _git(repo, "status", "--porcelain")

    # Resolution refuses before any landing — a PolicyError-class raise.
    with pytest.raises(PolicyError):
        worker.resolve_landing_base(repo, policy)

    # The checked-out branch did not advance, and the operator tree is unchanged.
    assert _git(repo, "rev-parse", "main") == main_before
    assert _git(repo, "rev-parse", "HEAD") == head_before
    assert _git(repo, "status", "--porcelain") == porcelain_before
