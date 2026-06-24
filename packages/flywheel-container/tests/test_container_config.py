"""Held-out oracle for spec 00045 SC-2/SC-3 — the policy->strategy bridge.

RED until container-config-bridge lands. resolve_auth maps an auth mode + env to
a ClaudeAuth (reading tokens by name); build_container_strategy wraps an inner
strategy into a ContainerSubmitStrategy from primitives. Do not weaken.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from flywheel_container import (
    API_KEY_ENV,
    ClaudeAuth,
    ContainerSubmitStrategy,
    OAUTH_TOKEN_ENV,
    build_container_strategy,
    resolve_auth,
)
from flywheel_orchestrator import SandboxRequest, SubmitRequest


# --- resolve_auth -----------------------------------------------------------


def test_oauth_reads_default_env() -> None:
    auth = resolve_auth("oauth", env={OAUTH_TOKEN_ENV: "tok"})
    assert isinstance(auth, ClaudeAuth)
    env, _ = auth.resolve({})
    assert env == {OAUTH_TOKEN_ENV: "tok"}


def test_api_key_reads_default_env() -> None:
    auth = resolve_auth("api_key", env={API_KEY_ENV: "sk-ant"})
    env, _ = auth.resolve({})  # type: ignore[union-attr]
    assert env == {API_KEY_ENV: "sk-ant"}


def test_custom_token_env_name() -> None:
    auth = resolve_auth("oauth", env={"MY_TOKEN": "t"}, token_env="MY_TOKEN")
    env, _ = auth.resolve({})  # type: ignore[union-attr]
    assert env == {OAUTH_TOKEN_ENV: "t"}


def test_missing_token_env_is_clear_error() -> None:
    with pytest.raises(ValueError, match=OAUTH_TOKEN_ENV):
        resolve_auth("oauth", env={})


def test_session_needs_no_env() -> None:
    auth = resolve_auth("session", env={})
    _, mounts = auth.resolve({})  # type: ignore[union-attr]
    assert len(mounts) == 1


def test_none_injects_nothing() -> None:
    assert resolve_auth("none", env={}) is None


def test_unknown_mode_rejected() -> None:
    with pytest.raises(ValueError, match="unknown container auth mode"):
        resolve_auth("magic", env={})


# --- build_container_strategy ----------------------------------------------


class _Inner:
    def prepare_sandbox(self, request: SandboxRequest) -> Path:
        return Path("/tmp/wt")

    def submit(self, request: SubmitRequest) -> None:  # pragma: no cover
        pass


def test_build_returns_container_strategy_wrapping_inner() -> None:
    strategy = build_container_strategy(
        _Inner(),
        image="img",
        model="claude-sonnet-4-6",
        exec_timeout=900,
        network_policy="deny",
        auth=resolve_auth("oauth", env={OAUTH_TOKEN_ENV: "tok"}),
    )
    assert isinstance(strategy, ContainerSubmitStrategy)


def test_exec_timeout_zero_is_unbounded() -> None:
    # 0 maps to None (unbounded) on the strategy.
    strategy = build_container_strategy(
        _Inner(), image="img", model="m", exec_timeout=0
    )
    assert strategy._exec_timeout is None  # type: ignore[attr-defined]
