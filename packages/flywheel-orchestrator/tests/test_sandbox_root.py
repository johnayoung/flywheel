"""Tests for :func:`resolve_sandbox_root` -- the ``[paths] sandbox_root``
resolution seam.

The knob accepts a literal path (relative anchors at the repo root, never
the process cwd; absolute is verbatim) or an out-of-tree token: ``@cache``
(XDG cache dir keyed by repo identity) or ``@sibling``
(``<repo-parent>/<repo>.worktrees``). Unset keeps the built-in nested
default. Tokens must fail fast as :class:`PolicyError` -- a worker should
refuse at startup, not mid-task.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

from flywheel_orchestrator import PolicyError, resolve_sandbox_root


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


def _git_repo(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-q", "-b", "main")
    return path


# --- literal paths -----------------------------------------------------------


def test_unset_keeps_nested_default(tmp_path: Path) -> None:
    resolved = resolve_sandbox_root(None, repo_root=tmp_path)
    assert resolved == (tmp_path / ".flywheel" / "worktrees").resolve()


def test_relative_path_anchors_at_repo_root_not_cwd(
    tmp_path: Path, monkeypatch
) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    monkeypatch.chdir(elsewhere)
    resolved = resolve_sandbox_root("scratch/worktrees", repo_root=repo)
    assert resolved == (repo / "scratch" / "worktrees").resolve()


def test_absolute_path_is_verbatim(tmp_path: Path) -> None:
    target = tmp_path / "abs" / "worktrees"
    resolved = resolve_sandbox_root(target, repo_root=tmp_path / "repo")
    assert resolved == target


def test_unknown_token_is_a_policy_error(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="unknown token '@elsewhere'"):
        resolve_sandbox_root("@elsewhere", repo_root=tmp_path)


# --- @sibling ----------------------------------------------------------------


def test_sibling_resolves_next_to_the_repo(tmp_path: Path) -> None:
    repo = tmp_path / "myrepo"
    repo.mkdir()
    resolved = resolve_sandbox_root("@sibling", repo_root=repo)
    assert resolved == tmp_path / "myrepo.worktrees"


@pytest.mark.skipif(
    os.geteuid() == 0, reason="root bypasses filesystem write checks"
)
def test_sibling_refuses_read_only_parent(tmp_path: Path) -> None:
    parent = tmp_path / "mount"
    repo = parent / "repo"
    repo.mkdir(parents=True)
    parent.chmod(0o555)
    try:
        with pytest.raises(PolicyError, match="not writable"):
            resolve_sandbox_root("@sibling", repo_root=repo)
    finally:
        parent.chmod(0o755)


# --- @cache ------------------------------------------------------------------


def test_cache_lands_under_xdg_cache_home(tmp_path: Path, monkeypatch) -> None:
    xdg = tmp_path / "xdg"
    monkeypatch.setenv("XDG_CACHE_HOME", str(xdg))
    repo = _git_repo(tmp_path / "myrepo")
    resolved = resolve_sandbox_root("@cache", repo_root=repo)
    assert resolved.parts[: len(xdg.parts)] == xdg.parts
    assert resolved.parent.parent == xdg / "flywheel"
    assert resolved.name == "worktrees"
    key = resolved.parent.name
    prefix, _, digest = key.rpartition("-")
    assert prefix == "myrepo"
    assert len(digest) == 12 and all(c in "0123456789abcdef" for c in digest)


def test_cache_is_stable_and_keyed_per_repo(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "xdg"))
    repo_a = _git_repo(tmp_path / "alpha")
    repo_b = _git_repo(tmp_path / "beta")
    assert resolve_sandbox_root("@cache", repo_root=repo_a) == (
        resolve_sandbox_root("@cache", repo_root=repo_a)
    )
    assert resolve_sandbox_root("@cache", repo_root=repo_a) != (
        resolve_sandbox_root("@cache", repo_root=repo_b)
    )


def test_cache_linked_worktree_shares_the_main_repo_key(
    tmp_path: Path, monkeypatch
) -> None:
    """A checkout created by ``git worktree add`` must key to its main
    repo's cache dir (same git common dir), not to a fresh one."""
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "xdg"))
    repo = _git_repo(tmp_path / "mainrepo")
    _git(repo, "commit", "--allow-empty", "-m", "root")
    linked = tmp_path / "linked"
    _git(repo, "worktree", "add", "-b", "t", str(linked), "main")
    main_key = resolve_sandbox_root("@cache", repo_root=repo).parent.name
    linked_key = resolve_sandbox_root("@cache", repo_root=linked).parent.name
    assert main_key.split("-")[-1] == linked_key.split("-")[-1]


def test_cache_falls_back_to_home_cache_without_xdg(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.delenv("XDG_CACHE_HOME", raising=False)
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    resolved = resolve_sandbox_root("@cache", repo_root=tmp_path / "r")
    expected_base = home / ".cache"
    assert resolved.parts[: len(expected_base.parts)] == expected_base.parts
