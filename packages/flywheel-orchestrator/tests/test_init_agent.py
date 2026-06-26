"""Tests for ``flywheel init --agent`` (agent-driven onboarding).

The agent path mirrors the established autopilot agent seam: an injectable
:data:`~flywheel_orchestrator._autopilot.AutopilotInvoker` coroutine drives the
proposal, so every test scripts a canned proposal and runs fully offline -- no
live model, and the SDK stays behind the lazy ``flywheel_core._sdk`` boundary.
The internal entry point :func:`run_agent_init` is exercised directly with the
scripted invoker; a CLI-level test monkeypatches the same seam.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

from flywheel_orchestrator import load_policy
from flywheel_orchestrator._init_agent import (
    InitAgentError,
    parse_init_proposal,
    run_agent_init,
)

_CANNED_PROPOSAL = {
    "source_kind": "directory",
    "default_graders": [
        "cargo test --workspace",
        "cargo clippy --workspace -- -D warnings",
    ],
    "sandbox_setup": "cargo fetch",
    "autopilot": {"target_depth": 3, "interval_seconds": 600},
    "gitignore_additions": ["/target"],
    "notes": "Rust Cargo workspace; ~50 tests",
}


def _scripted_invoker(payload: dict[str, object]):
    """Build an injectable invoker returning ``payload`` as a fenced block."""

    async def _invoke(_prompt: str) -> str:
        return (
            "Here is my proposal.\n\n```json\n"
            + json.dumps(payload)
            + "\n```\n"
        )

    return _invoke


def _git_init_attached(path: Path) -> None:
    run = lambda *args: subprocess.run(  # noqa: E731
        ["git", *args], cwd=path, check=True, capture_output=True
    )
    run("init", "-b", "main")
    run("config", "user.email", "test@flywheel.invalid")
    run("config", "user.name", "Flywheel Test")
    run("commit", "--allow-empty", "-m", "root")


@pytest.fixture
def repo(tmp_path: Path, monkeypatch) -> Path:
    _git_init_attached(tmp_path)
    monkeypatch.chdir(tmp_path)
    return tmp_path


# --- proposal -> loadable policy ---------------------------------------------


def test_agent_init_writes_loadable_policy_reflecting_proposal(
    tmp_path: Path,
) -> None:
    """The scripted proposal renders a flywheel.toml that load_policy accepts
    and whose values reflect the proposal verbatim."""
    result = run_agent_init(
        repo_root=tmp_path,
        invoker=_scripted_invoker(_CANNED_PROPOSAL),
    )

    policy_path = tmp_path / "flywheel.toml"
    assert policy_path.is_file()

    policy = load_policy(policy_path)
    assert policy.source_kind == "directory"
    assert policy.autopilot_target_depth == 3
    assert policy.autopilot_interval_seconds == 600
    assert policy.sandbox_setup == "cargo fetch"
    grader_runs = [getattr(g, "run", None) for g in policy.default_graders]
    assert grader_runs == [
        "cargo test --workspace",
        "cargo clippy --workspace -- -D warnings",
    ]
    # The validated policy is also returned on the result.
    assert result.policy.autopilot_target_depth == 3


def test_agent_init_appends_gitignore_without_duplicating(
    tmp_path: Path,
) -> None:
    """gitignore_additions are appended once; a second run is a no-op for an
    already-present line."""
    first = run_agent_init(
        repo_root=tmp_path,
        invoker=_scripted_invoker(_CANNED_PROPOSAL),
    )
    assert first.gitignore_added == ("/target",)
    gitignore = tmp_path / ".gitignore"
    assert gitignore.read_text(encoding="utf-8").count("/target") == 1

    second = run_agent_init(
        repo_root=tmp_path,
        invoker=_scripted_invoker(_CANNED_PROPOSAL),
    )
    assert second.gitignore_added == ()
    assert gitignore.read_text(encoding="utf-8").count("/target") == 1


def test_agent_init_preserves_existing_gitignore_lines(tmp_path: Path) -> None:
    """A pre-existing .gitignore keeps its lines; only new ones are appended."""
    (tmp_path / ".gitignore").write_text("/build\n", encoding="utf-8")
    run_agent_init(
        repo_root=tmp_path,
        invoker=_scripted_invoker(_CANNED_PROPOSAL),
    )
    text = (tmp_path / ".gitignore").read_text(encoding="utf-8")
    assert "/build" in text
    assert "/target" in text


# --- failure modes write nothing ---------------------------------------------


def test_agent_init_unparseable_response_raises_and_writes_nothing(
    tmp_path: Path,
) -> None:
    """A response with no JSON object raises InitAgentError and leaves no
    policy (and no .gitignore) behind."""

    async def _no_json(_prompt: str) -> str:
        return "I could not figure out the toolchain. Sorry, no JSON here."

    with pytest.raises(InitAgentError):
        run_agent_init(repo_root=tmp_path, invoker=_no_json)

    assert not (tmp_path / "flywheel.toml").exists()
    assert not (tmp_path / ".gitignore").exists()


def test_agent_init_invalid_render_raises_and_writes_nothing(
    tmp_path: Path, monkeypatch,
) -> None:
    """When the rendered policy fails load_policy validation, init raises
    InitAgentError and leaves no flywheel.toml behind (the in-memory
    validation gate runs before the real write)."""
    import flywheel_orchestrator._init_agent as init_agent

    # Force an invalid render so the validation gate -- not the parser -- is
    # what fails. ``[store] backend`` must be sqlite/postgres; "bogus" is
    # rejected by load_policy.
    monkeypatch.setattr(
        init_agent,
        "render_agent_policy",
        lambda *a, **k: '[source]\nkind = "directory"\n[store]\nbackend = "bogus"\n',
    )

    with pytest.raises(InitAgentError):
        run_agent_init(
            repo_root=tmp_path,
            invoker=_scripted_invoker(_CANNED_PROPOSAL),
        )

    assert not (tmp_path / "flywheel.toml").exists()
    # The scratch validation file is cleaned up too.
    assert not any(p.name.endswith(".init-agent.tmp") for p in tmp_path.iterdir())


# --- the module stays SDK-lazy -----------------------------------------------


def test_init_agent_module_does_not_import_sdk_at_load() -> None:
    """Importing the init-agent module must not pull in claude_agent_sdk -- the
    SDK stays behind the lazy flywheel_core._sdk boundary."""
    code = (
        "import sys\n"
        "import flywheel_orchestrator._init_agent  # noqa: F401\n"
        "assert 'claude_agent_sdk' not in sys.modules, "
        "'init-agent import pulled in the SDK eagerly'\n"
    )
    completed = subprocess.run(
        [sys.executable, "-c", code], capture_output=True, text=True
    )
    assert completed.returncode == 0, completed.stderr


# --- proposal parsing defensiveness ------------------------------------------


def test_parse_proposal_defaults_on_missing_fields() -> None:
    """A minimal fenced object still parses; absent fields take defaults."""
    proposal = parse_init_proposal("```json\n{}\n```")
    assert proposal.source_kind == "directory"
    assert proposal.default_graders == ()
    assert proposal.sandbox_setup is None
    assert proposal.target_depth is None


# --- CLI wiring (monkeypatched seam) -----------------------------------------


def test_cli_agent_flag_routes_through_seam(repo: Path, monkeypatch, capsys) -> None:
    """`flywheel init --agent` builds the scaffold and routes the policy write
    through the agent seam, printing the proposal before writing."""
    import flywheel_orchestrator._init_agent as init_agent
    from flywheel_orchestrator._workflow import main

    monkeypatch.setattr(init_agent, "build_repo_invoker", lambda *a, **k: _scripted_invoker(_CANNED_PROPOSAL))

    assert main(["init", "--agent"]) == 0

    policy = load_policy(repo / "flywheel.toml")
    assert policy.autopilot_target_depth == 3
    assert policy.sandbox_setup == "cargo fetch"
    # The scaffold still ran.
    assert (repo / ".flywheel" / "tasks" / "active" / ".gitkeep").is_file()
    out = capsys.readouterr().out
    assert "Agent proposal:" in out
    assert "Rust Cargo workspace" in out
