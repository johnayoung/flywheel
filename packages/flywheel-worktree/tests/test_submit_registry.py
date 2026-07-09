"""Tests for the submit (landing) strategy selection seam.

The submit-strategy registry (:data:`flywheel_worktree._submit_registry.
SUBMIT_STRATEGIES`) is the seam an operator uses to pick a landing policy by
name (``merge`` vs ``pr``) and the worker dispatches through. These tests
prove the behaviour the worker relies on:

* the two built-ins resolve to the matching builders, and an unknown name
  raises the branded :class:`UnknownPluginError` listing the valid choices;
* each builder returns a protocol-conformant submitter wired with the kwargs
  it was handed (the merge builder tolerating ``policy=None``, the PR builder
  reading remote/base from a required policy and logging the landing target);
* the two builders share one keyword signature so the registry dispatches by
  name with no per-strategy branch;
* resolving ``pr`` is what breaks the worker<->pr import cycle: ``worker``
  does not drag ``pr`` in at import time, and ``resolve("pr")`` lazily imports
  it — proven in a fresh interpreter so a pre-imported ``pr`` cannot mask it.
"""

from __future__ import annotations

import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

from flywheel_core._registry import UnknownPluginError
from flywheel_orchestrator import SubmitStrategy, WorkPolicy
from flywheel_worktree import pr, worker
from flywheel_worktree._submit_registry import SUBMIT_STRATEGIES


# --- fixtures / helpers ------------------------------------------------------


def _kwargs(tmp_path: Path, log: worker.Logger) -> dict[str, object]:
    """The shared keyword arguments both builders accept (uniform signature).

    Returns a fresh dict each call so a test can mutate it without bleeding
    into another, and uses ``tmp_path`` for every filesystem location so no
    real repo is touched (the builders construct, they do not run git).
    """
    return {
        "repo_root": tmp_path / "repo",
        "tasks_dir": tmp_path / "tasks",
        "worktrees_dir": tmp_path / "worktrees",
        "phase_base": "main",
        "lock_path": tmp_path / ".merge.lock",
        "log": log,
        "protected_paths": ("conftest.py", ".github/**"),
        "setup_command": "echo setup",
    }


def _pr_policy(
    *, remote: str = "upstream", pr_base: str | None = "release"
) -> WorkPolicy:
    """A minimal policy that selects the PR strategy with explicit targets."""
    return WorkPolicy(
        source_kind="directory",
        submit_strategy="pr",
        submit_remote=remote,
        submit_pr_base=pr_base,
    )


# --- registry shape ----------------------------------------------------------


def test_names_are_merge_pr_then_phase() -> None:
    assert SUBMIT_STRATEGIES.names() == ("merge", "pr", "phase")


def test_resolve_merge_returns_merge_builder() -> None:
    assert SUBMIT_STRATEGIES.resolve("merge") is worker.build_merge_submitter


def test_resolve_pr_returns_pr_builder() -> None:
    assert SUBMIT_STRATEGIES.resolve("pr") is pr.build_pr_submitter


def test_resolve_phase_returns_phase_builder() -> None:
    assert SUBMIT_STRATEGIES.resolve("phase") is worker.build_phase_submitter


def test_resolve_unknown_raises_listing_choices() -> None:
    with pytest.raises(UnknownPluginError) as excinfo:
        SUBMIT_STRATEGIES.resolve("nope")
    message = str(excinfo.value)
    assert "'merge'" in message
    assert "'pr'" in message
    assert "nope" in message


# --- merge builder -----------------------------------------------------------


def test_merge_builder_returns_plain_worktree_submitter(tmp_path: Path) -> None:
    kwargs = _kwargs(tmp_path, lambda _m: None)
    submitter = worker.build_merge_submitter(None, **kwargs)  # type: ignore[arg-type]

    # The merge backend is the base submitter, never the PR subclass.
    assert isinstance(submitter, worker.GitWorktreeSubmitter)
    assert not isinstance(submitter, pr.GitPullRequestSubmitter)


def test_merge_builder_works_with_policy_none(tmp_path: Path) -> None:
    # The merge landing reads nothing from policy; policy=None must build.
    submitter = worker.build_merge_submitter(
        None, **_kwargs(tmp_path, lambda _m: None)  # type: ignore[arg-type]
    )
    assert isinstance(submitter, worker.GitWorktreeSubmitter)


def test_merge_builder_stores_passed_kwargs(tmp_path: Path) -> None:
    kwargs = _kwargs(tmp_path, lambda _m: None)
    submitter = worker.build_merge_submitter(None, **kwargs)  # type: ignore[arg-type]

    assert submitter.repo_root == kwargs["repo_root"]
    assert submitter.tasks_dir == kwargs["tasks_dir"]
    assert submitter.worktrees_dir == kwargs["worktrees_dir"]
    assert submitter.phase_base == kwargs["phase_base"]
    assert submitter.lock_path == kwargs["lock_path"]
    assert submitter.protected_paths == kwargs["protected_paths"]
    assert submitter.setup_command == kwargs["setup_command"]


# --- pr builder --------------------------------------------------------------


def test_pr_builder_returns_pr_submitter_with_remote_and_base(
    tmp_path: Path,
) -> None:
    policy = _pr_policy(remote="upstream", pr_base="release")
    submitter = pr.build_pr_submitter(
        policy, **_kwargs(tmp_path, lambda _m: None)  # type: ignore[arg-type]
    )

    assert isinstance(submitter, pr.GitPullRequestSubmitter)
    assert submitter.remote == "upstream"
    assert submitter.pr_base == "release"


def test_pr_builder_falls_back_to_phase_base_for_pr_base(
    tmp_path: Path,
) -> None:
    # No explicit pr_base in policy -> the PR base is the worker's phase base.
    policy = _pr_policy(remote="origin", pr_base=None)
    kwargs = _kwargs(tmp_path, lambda _m: None)
    submitter = pr.build_pr_submitter(policy, **kwargs)  # type: ignore[arg-type]

    assert submitter.pr_base == kwargs["phase_base"]


def test_pr_builder_logs_landing_target(tmp_path: Path) -> None:
    captured: list[str] = []
    policy = _pr_policy(remote="origin", pr_base="main")
    pr.build_pr_submitter(
        policy, **_kwargs(tmp_path, captured.append)  # type: ignore[arg-type]
    )

    assert captured, "PR builder must emit a landing log line"
    joined = " ".join(captured).lower()
    assert "pr" in joined
    assert "landing" in joined


def test_pr_builder_requires_policy(tmp_path: Path) -> None:
    # Selecting "pr" without a policy is a programming error: the builder
    # asserts a policy is present rather than silently building defaults.
    with pytest.raises(AssertionError):
        pr.build_pr_submitter(
            None, **_kwargs(tmp_path, lambda _m: None)  # type: ignore[arg-type]
        )


# --- phase builder -----------------------------------------------------------


def test_phase_builder_returns_phase_submitter(tmp_path: Path) -> None:
    policy = WorkPolicy(source_kind="directory", submit_strategy="phase")
    submitter = worker.build_phase_submitter(
        policy, **_kwargs(tmp_path, lambda _m: None)  # type: ignore[arg-type]
    )

    # The phase backend is a GitWorktreeSubmitter subclass (it inherits the
    # merge strategy's whole verify ladder), never the PR subclass.
    assert isinstance(submitter, worker.PhaseBranchSubmitter)
    assert isinstance(submitter, worker.GitWorktreeSubmitter)
    assert not isinstance(submitter, pr.GitPullRequestSubmitter)


def test_phase_builder_tolerates_policy_none(tmp_path: Path) -> None:
    # Like the merge builder, the phase landing reads no required policy field,
    # so policy=None must still build (mirrors build_merge_submitter).
    submitter = worker.build_phase_submitter(
        None, **_kwargs(tmp_path, lambda _m: None)  # type: ignore[arg-type]
    )
    assert isinstance(submitter, worker.PhaseBranchSubmitter)


def test_phase_builder_logs_landing_target(tmp_path: Path) -> None:
    captured: list[str] = []
    worker.build_phase_submitter(
        None, **_kwargs(tmp_path, captured.append)  # type: ignore[arg-type]
    )

    assert captured, "phase builder must emit a landing log line"
    joined = " ".join(captured).lower()
    assert "phase" in joined
    assert "landing" in joined


# --- protocol conformance ----------------------------------------------------


def test_both_builders_yield_submit_strategy_conformant_objects(
    tmp_path: Path,
) -> None:
    merge_submitter = worker.build_merge_submitter(
        None, **_kwargs(tmp_path, lambda _m: None)  # type: ignore[arg-type]
    )
    pr_submitter = pr.build_pr_submitter(
        _pr_policy(), **_kwargs(tmp_path, lambda _m: None)  # type: ignore[arg-type]
    )
    phase_submitter = worker.build_phase_submitter(
        None, **_kwargs(tmp_path, lambda _m: None)  # type: ignore[arg-type]
    )

    # runtime_checkable: the registry yields objects that satisfy the seam's
    # Protocol, so orchestrate can take any of them whole.
    assert isinstance(merge_submitter, SubmitStrategy)
    assert isinstance(pr_submitter, SubmitStrategy)
    assert isinstance(phase_submitter, SubmitStrategy)


# --- uniform signature -------------------------------------------------------


def test_builders_share_one_keyword_signature(tmp_path: Path) -> None:
    # The same kwargs dict drives both builders unchanged — the uniform
    # signature the registry relies on to dispatch by name with no branch.
    shared = _kwargs(tmp_path, lambda _m: None)

    merge_submitter = worker.build_merge_submitter(None, **shared)  # type: ignore[arg-type]
    pr_submitter = pr.build_pr_submitter(_pr_policy(), **shared)  # type: ignore[arg-type]

    assert isinstance(merge_submitter, worker.GitWorktreeSubmitter)
    assert isinstance(pr_submitter, pr.GitPullRequestSubmitter)


# --- lazy import / broken cycle ----------------------------------------------


def test_worker_import_does_not_drag_in_pr_and_resolve_is_lazy() -> None:
    """The registry dissolves the historical worker<->pr import cycle.

    Run in a fresh interpreter so a pre-imported ``flywheel_worktree.pr``
    (the dev process has it) cannot mask the contract:

    1. importing ``worker`` must NOT import ``pr`` (no top-level cycle);
    2. resolving ``"pr"`` lazily imports ``pr`` — afterwards it IS in
       ``sys.modules`` and is the genuine ``build_pr_submitter``.
    """
    program = textwrap.dedent(
        """
        import sys

        import flywheel_worktree.worker  # noqa: F401
        assert "flywheel_worktree.pr" not in sys.modules, (
            "worker dragged pr in at import time"
        )

        from flywheel_worktree._submit_registry import SUBMIT_STRATEGIES
        builder = SUBMIT_STRATEGIES.resolve("pr")
        assert "flywheel_worktree.pr" in sys.modules, (
            "resolve('pr') did not import pr"
        )

        import flywheel_worktree.pr as pr
        assert builder is pr.build_pr_submitter

        print("OK")
        """
    )
    result = subprocess.run(
        [sys.executable, "-c", program],
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip().endswith("OK"), result.stdout
