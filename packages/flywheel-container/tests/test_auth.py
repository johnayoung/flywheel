"""Held-out oracle for the ClaudeAuth abstraction (spec 00044).

RED until ClaudeAuth lands. Subscription-first auth for the container backend:
oauth_token -> CLAUDE_CODE_OAUTH_TOKEN env; session -> bind-mount ~/.claude +
HOME; api_key -> ANTHROPIC_API_KEY. A subscription mode refuses to coexist with
an ANTHROPIC_API_KEY in the container env (which would override it). Do not
weaken assertions.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from flywheel_container import (
    API_KEY_ENV,
    ClaudeAuth,
    ClaudeCliAgent,
    ContainerRuntime,
    ContainerSubmitStrategy,
    DEFAULT_AGENT_HOME,
    OAUTH_TOKEN_ENV,
)
from flywheel_orchestrator import SandboxRequest


# --- pure auth modes --------------------------------------------------------


def test_oauth_token_sets_env_no_mounts() -> None:
    env, mounts = ClaudeAuth.oauth_token("tok-123").resolve({})
    assert env == {OAUTH_TOKEN_ENV: "tok-123"}
    assert mounts == ()


def test_session_mounts_credentials_and_sets_home() -> None:
    env, mounts = ClaudeAuth.session().resolve({})
    assert env == {"HOME": DEFAULT_AGENT_HOME}
    assert len(mounts) == 1
    assert mounts[0].host_path == os.path.expanduser("~/.claude")
    assert mounts[0].sandbox_path == f"{DEFAULT_AGENT_HOME}/.claude"


def test_session_honors_custom_claude_dir(tmp_path: Path) -> None:
    _, mounts = ClaudeAuth.session(str(tmp_path / "creds")).resolve({})
    assert mounts[0].host_path == str(tmp_path / "creds")


def test_api_key_sets_env() -> None:
    env, mounts = ClaudeAuth.api_key("sk-ant-x").resolve({})
    assert env == {API_KEY_ENV: "sk-ant-x"}
    assert mounts == ()


def test_empty_token_or_key_rejected() -> None:
    with pytest.raises(ValueError, match="non-empty token"):
        ClaudeAuth.oauth_token("")
    with pytest.raises(ValueError, match="non-empty key"):
        ClaudeAuth.api_key("")


def test_subscription_rejects_api_key_in_env() -> None:
    # The CLI prefers ANTHROPIC_API_KEY over the subscription token, so the two
    # together would silently ignore the session — refuse it.
    with pytest.raises(ValueError, match=API_KEY_ENV):
        ClaudeAuth.oauth_token("tok").resolve({API_KEY_ENV: "sk-ant-x"})
    with pytest.raises(ValueError, match=API_KEY_ENV):
        ClaudeAuth.session().resolve({API_KEY_ENV: "sk-ant-x"})


def test_api_key_mode_coexists_with_api_key_env() -> None:
    # Not a subscription, so no conflict.
    env, _ = ClaudeAuth.api_key("sk-ant-x").resolve({API_KEY_ENV: "sk-ant-x"})
    assert env == {API_KEY_ENV: "sk-ant-x"}


# --- strategy wiring (fake runtime, no docker) ------------------------------


class _Recorder:
    def __init__(self) -> None:
        self.start_kwargs: dict = {}

    def start(self, name, image, **kwargs):
        self.start_kwargs = kwargs
        return name

    def runtime(self) -> ContainerRuntime:
        return ContainerRuntime(
            start=self.start,
            exec_command=lambda *a, **k: None,
            remove=lambda name: None,
            register_cleanup=lambda name: (lambda: None),
            ensure_internal_network=lambda name: None,
        )


class _Inner:
    def prepare_sandbox(self, request: SandboxRequest):
        return Path("/tmp/wt")

    def submit(self, request) -> None:  # pragma: no cover
        pass


def _req() -> SandboxRequest:
    return SandboxRequest(task_id="t", task_file=Path(), run_id=None, mode="fresh")


def test_strategy_oauth_auth_reaches_container_start_env() -> None:
    rec = _Recorder()
    ContainerSubmitStrategy(
        _Inner(),
        image="img",
        agent=ClaudeCliAgent(model="m"),
        auth=ClaudeAuth.oauth_token("tok-xyz"),
        container_uid=1000,
        container_gid=1000,
        preflight=False,
        runtime=rec.runtime(),
    ).prepare_sandbox(_req())
    assert rec.start_kwargs["env"][OAUTH_TOKEN_ENV] == "tok-xyz"
    assert API_KEY_ENV not in rec.start_kwargs["env"]


def test_strategy_session_auth_adds_credentials_mount() -> None:
    rec = _Recorder()
    ContainerSubmitStrategy(
        _Inner(),
        image="img",
        agent=ClaudeCliAgent(model="m"),
        auth=ClaudeAuth.session(),
        container_uid=1000,
        container_gid=1000,
        preflight=False,
        runtime=rec.runtime(),
    ).prepare_sandbox(_req())
    mounts = rec.start_kwargs["mounts"]
    # the worktree bind mount + the ~/.claude credentials mount
    sandbox_paths = {m.sandbox_path for m in mounts}
    assert f"{DEFAULT_AGENT_HOME}/.claude" in sandbox_paths
    assert rec.start_kwargs["env"]["HOME"] == DEFAULT_AGENT_HOME


def test_strategy_rejects_subscription_auth_with_api_key_env() -> None:
    with pytest.raises(ValueError, match=API_KEY_ENV):
        ContainerSubmitStrategy(
            _Inner(),
            image="img",
            agent=ClaudeCliAgent(model="m"),
            env={API_KEY_ENV: "sk-ant-x"},
            auth=ClaudeAuth.oauth_token("tok"),
            preflight=False,
            runtime=_Recorder().runtime(),
        )
