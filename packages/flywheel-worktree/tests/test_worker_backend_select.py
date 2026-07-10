"""Held-out oracle for spec 00045 SC-4 — worker backend selection.

RED until worker-backend-select lands. maybe_wrap_for_backend returns the
worktree submitter unchanged for backend="worktree" and a ContainerSubmitStrategy
wrapping it for backend="container", resolving image/model/auth from policy. Do
not weaken.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

import pytest

from flywheel_container import ContainerSubmitStrategy
from flywheel_orchestrator import SandboxRequest, SubmitRequest
from flywheel_orchestrator._policy import load_policy
from flywheel_worktree.worker import maybe_wrap_for_backend

OAUTH_ENV = "CLAUDE_CODE_OAUTH_TOKEN"


def _policy(tmp_path: Path, body: str):
    p = tmp_path / "flywheel.toml"
    p.write_text(
        '[source]\nkind = "directory"\n' + textwrap.dedent(body),
        encoding="utf-8",
    )
    return load_policy(p)


class _FakeSubmitter:
    """Stand-in for GitWorktreeSubmitter (maybe_wrap only passes it through)."""

    def prepare_sandbox(self, request: SandboxRequest) -> Path:
        return Path("/tmp/wt")

    def submit(self, request: SubmitRequest) -> None:  # pragma: no cover
        pass


def _log(_msg: str) -> None:
    pass


def test_worktree_backend_returns_submitter_unchanged(tmp_path: Path) -> None:
    submitter = _FakeSubmitter()
    pol = _policy(tmp_path, "")  # default backend = worktree
    out = maybe_wrap_for_backend(
        submitter, pol, model="m", env={}, log=_log  # type: ignore[arg-type]
    )
    assert out is submitter


def test_container_backend_wraps_with_resolved_config(tmp_path: Path) -> None:
    submitter = _FakeSubmitter()
    pol = _policy(
        tmp_path,
        """
        [sandbox]
        backend = "container"
        [sandbox.container]
        image = "flywheel-agent:latest"
        model = "claude-sonnet-4-6"
        auth = "none"
        """,
    )
    out = maybe_wrap_for_backend(
        submitter, pol, model="ignored", env={}, log=_log  # type: ignore[arg-type]
    )
    assert isinstance(out, ContainerSubmitStrategy)
    assert out._model == "claude-sonnet-4-6"  # type: ignore[attr-defined]


def test_container_falls_back_to_worker_model(tmp_path: Path) -> None:
    pol = _policy(
        tmp_path,
        """
        [sandbox]
        backend = "container"
        [sandbox.container]
        image = "img"
        auth = "none"
        """,
    )
    out = maybe_wrap_for_backend(
        _FakeSubmitter(), pol, model="claude-from-worker", env={},  # type: ignore[arg-type]
        log=_log,
    )
    assert out._model == "claude-from-worker"  # type: ignore[attr-defined]


def test_container_without_any_model_raises(tmp_path: Path) -> None:
    pol = _policy(
        tmp_path,
        """
        [sandbox]
        backend = "container"
        [sandbox.container]
        image = "img"
        auth = "none"
        """,
    )
    with pytest.raises(RuntimeError, match="needs an explicit model"):
        maybe_wrap_for_backend(
            _FakeSubmitter(), pol, model=None, env={}, log=_log  # type: ignore[arg-type]
        )


def test_container_oauth_reads_token_from_env(tmp_path: Path) -> None:
    pol = _policy(
        tmp_path,
        """
        [sandbox]
        backend = "container"
        [sandbox.container]
        image = "img"
        model = "m"
        auth = "oauth"
        """,
    )
    out = maybe_wrap_for_backend(
        _FakeSubmitter(), pol, model="m", env={OAUTH_ENV: "tok"},  # type: ignore[arg-type]
        log=_log,
    )
    assert isinstance(out, ContainerSubmitStrategy)


def test_container_oauth_missing_token_errors(tmp_path: Path) -> None:
    pol = _policy(
        tmp_path,
        """
        [sandbox]
        backend = "container"
        [sandbox.container]
        image = "img"
        model = "m"
        auth = "oauth"
        """,
    )
    with pytest.raises(ValueError, match=OAUTH_ENV):
        maybe_wrap_for_backend(
            _FakeSubmitter(), pol, model="m", env={}, log=_log  # type: ignore[arg-type]
        )
