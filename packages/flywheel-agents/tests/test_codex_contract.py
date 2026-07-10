"""Codex adapter contract suite (design doc section 17) through the real stack.

``AgentRuntime`` + ``LocalHost`` + the codex CLI adapter drive the
deterministic fake codex CLI (``tests/fake_codex.py``) end to end: real
subprocess, real ``codex exec --json`` parsing, real fold. No vendor CLI
required. Also unit-tests ``build_cli_plan``'s flag mapping.
"""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path
from typing import Any

import pytest

from flywheel_agents import (
    AgentConfiguration,
    AgentRuntime,
    AuthenticationPolicy,
    CompletedRun,
    EventType,
    MemorySink,
    PermissionPolicy,
    RunRequest,
    StopReason,
    UnsupportedCapabilityError,
)
from flywheel_agents.codex import build_cli_plan

FAKE_CODEX = Path(__file__).parent / "fake_codex.py"

# Mirrors fake_codex.py's happy-path usage after cached_input_tokens mapping.
FOLDED_USAGE = {
    "input_tokens": 11,
    "output_tokens": 3,
    "cache_read_input_tokens": 4,
    "cache_creation_input_tokens": 0,
}


def run_scenario(
    scenario: str,
    working_directory: Path,
    *,
    environment: dict[str, str] | None = None,
    timeout_seconds: float | None = None,
    authentication_policy: AuthenticationPolicy = (
        AuthenticationPolicy.ACCOUNT_PREFERRED
    ),
) -> tuple[CompletedRun, MemorySink]:
    env = {"FAKE_CODEX_SCENARIO": scenario}
    if environment:
        env.update(environment)
    configuration = AgentConfiguration(
        agent_id="codex",
        permission_policy=PermissionPolicy.AUTO,
        authentication_policy=authentication_policy,
        command_override=(sys.executable, str(FAKE_CODEX)),
        environment=env,
    )
    request = RunRequest(
        prompt="exercise the fake codex",
        working_directory=working_directory,
        configuration=configuration,
        timeout_seconds=timeout_seconds,
    )
    sink = MemorySink()
    run = asyncio.run(AgentRuntime().run(request, sink=sink))
    return run, sink


def test_happy_path_fold(tmp_path: Path) -> None:
    run, sink = run_scenario("happy", tmp_path)
    started = [e for e in sink.events if e.type is EventType.SESSION_STARTED]
    assert len(started) == 1
    assert started[0].payload["session_id"] == "codex-1"
    assert run.final_text == "Hello from codex. All done."
    assert run.stop.reason is StopReason.COMPLETED
    assert run.stop.native == "turn.completed"
    assert run.stop.is_error is False
    assert run.stop.pending_tool_use is False
    assert run.usage == FOLDED_USAGE
    assert run.num_turns == 1
    assert run.total_cost_usd is None
    assert run.native_session_id == "codex-1"
    assert run.failure is None
    assert run.fault is None
    assert run.exit.returncode == 0
    assert len(run.tool_interactions) == 2
    command = run.tool_interactions[0]
    assert command.tool_use_id == "cmd-1"
    assert command.tool_name == "command_execution"
    assert command.tool_input == {"command": "echo hi"}
    assert command.result is not None
    assert command.result.is_error is False
    assert command.result.content == "hi\n"
    file_change = run.tool_interactions[1]
    assert file_change.tool_use_id == "fc-1"
    assert file_change.tool_name == "file_change"
    assert file_change.result is not None
    assert file_change.result.is_error is False
    changed = [e for e in sink.events if e.type is EventType.FILE_CHANGED]
    assert len(changed) == 1
    assert changed[0].payload == {"path": "src/app.py", "kind": "update"}
    plans = [e for e in sink.events if e.type is EventType.PLAN_UPDATED]
    assert len(plans) == 1


def test_happy_path_sequences_and_sources(tmp_path: Path) -> None:
    run, sink = run_scenario("happy", tmp_path)
    assert run.failure is None
    assert sink.raw
    assert sink.events
    raw_sequences = [e.sequence for e in sink.raw]
    event_sequences = [e.sequence for e in sink.events]
    assert all(b > a for a, b in zip(raw_sequences, raw_sequences[1:]))
    assert all(b > a for a, b in zip(event_sequences, event_sequences[1:]))
    merged = sorted(raw_sequences + event_sequences)
    assert merged == list(range(1, len(merged) + 1))
    assert all(e.source == "codex" for e in sink.events)
    assert all(e.adapter_id == "codex" for e in sink.raw)


def test_tool_error_reported_on_interaction(tmp_path: Path) -> None:
    run, _ = run_scenario("tool_error", tmp_path)
    assert run.failure is None
    assert len(run.tool_interactions) == 1
    interaction = run.tool_interactions[0]
    assert interaction.tool_use_id == "cmd-err"
    assert interaction.result is not None
    assert interaction.result.is_error is True


def test_turn_failed_is_error_stop_without_failure(tmp_path: Path) -> None:
    run, sink = run_scenario("turn_failed", tmp_path)
    assert run.stop.reason is StopReason.ERROR
    assert run.stop.is_error is True
    assert run.stop.native == "turn.failed"
    assert run.failure is None  # the fake exits 0: agent output, not transport
    assert run.exit.returncode == 0
    errors = [e for e in sink.events if e.type is EventType.ERROR]
    assert any(
        e.payload.get("message") == "model stream disconnected" for e in errors
    )


def test_malformed_lines_become_raw_and_run_completes(tmp_path: Path) -> None:
    run, sink = run_scenario("malformed", tmp_path)
    assert run.failure is None
    assert run.stop.reason is StopReason.COMPLETED
    assert run.final_text == "before garbage after garbage"
    raw_events = [e for e in sink.events if e.type is EventType.RAW]
    assert len(raw_events) >= 3
    assert any(e.native_type == "mystery" for e in raw_events)
    assert any(e.native_type == "item.completed" for e in raw_events)


def test_crash_without_turn_completed_is_agent_exit(tmp_path: Path) -> None:
    run, _ = run_scenario("crash", tmp_path)
    assert run.failure is not None
    assert run.failure.error_type == "agent_exit"
    assert run.failure.exit_code == 3
    captured = (run.failure.stderr or "") + (run.exit.stderr_tail or "")
    assert "fatal explosion" in captured
    assert run.stop.reason is StopReason.UNKNOWN
    assert run.exit.returncode == 3


def test_working_directory_respected(tmp_path: Path) -> None:
    run, _ = run_scenario("cwd", tmp_path)
    assert run.failure is None
    assert run.final_text
    assert Path(run.final_text).resolve() == tmp_path.resolve()


def test_environment_marker_passes_through(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("OPENAI_API_KEY", "sk-test")
    run, _ = run_scenario(
        "env", tmp_path, environment={"TEST_MARKER": "marker-123"}
    )
    assert run.failure is None
    reported = json.loads(run.final_text)
    assert reported == {"marker": "marker-123", "api_key": "sk-test"}


def test_account_only_strips_api_key(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("OPENAI_API_KEY", "sk-test")
    run, _ = run_scenario(
        "env",
        tmp_path,
        environment={"TEST_MARKER": "marker-123"},
        authentication_policy=AuthenticationPolicy.ACCOUNT_ONLY,
    )
    assert run.failure is None
    reported = json.loads(run.final_text)
    assert reported == {"marker": "marker-123", "api_key": None}


def test_unknown_transport_rejected(tmp_path: Path) -> None:
    configuration = AgentConfiguration(
        agent_id="codex",
        command_override=(sys.executable, str(FAKE_CODEX)),
        adapter_options={"transport": "sdk"},
    )
    request = RunRequest(
        prompt="never spawns",
        working_directory=tmp_path,
        configuration=configuration,
    )
    with pytest.raises(UnsupportedCapabilityError):
        asyncio.run(AgentRuntime().run(request))


def test_codex_registered_in_default_registry() -> None:
    ids = AgentRuntime().registry.ids()
    assert "codex" in ids
    assert "claude-code" in ids


# --- build_cli_plan flag mapping -------------------------------------------


def _request(**overrides: Any) -> RunRequest:
    configuration = AgentConfiguration(agent_id="codex", **overrides)
    return RunRequest(
        prompt="the prompt",
        working_directory=Path("/tmp/workdir"),
        configuration=configuration,
    )


def test_plan_defaults_supervised_sandbox_and_stdin_prompt() -> None:
    plan = build_cli_plan(_request())
    assert plan.argv[:3] == ("codex", "exec", "--json")
    assert ("--sandbox", "workspace-write") == plan.argv[3:5]
    assert plan.argv[-1] == "-"
    assert plan.stdin_payload == "the prompt"
    assert plan.cwd == Path("/tmp/workdir")
    assert plan.denied_environment == ()
    assert "--max-turns" not in plan.argv


def test_plan_auto_maps_to_bypass_flag() -> None:
    plan = build_cli_plan(
        _request(permission_policy=PermissionPolicy.AUTO)
    )
    assert "--dangerously-bypass-approvals-and-sandbox" in plan.argv
    assert "--sandbox" not in plan.argv


def test_plan_read_only_maps_to_read_only_sandbox() -> None:
    plan = build_cli_plan(
        _request(permission_policy=PermissionPolicy.READ_ONLY)
    )
    index = plan.argv.index("--sandbox")
    assert plan.argv[index + 1] == "read-only"


def test_plan_plan_mode_raises() -> None:
    with pytest.raises(UnsupportedCapabilityError):
        build_cli_plan(_request(permission_policy=PermissionPolicy.PLAN))


def test_plan_model_flag() -> None:
    plan = build_cli_plan(_request(model_id="gpt-5-codex"))
    index = plan.argv.index("--model")
    assert plan.argv[index + 1] == "gpt-5-codex"


def test_plan_max_turns_ignored() -> None:
    plan = build_cli_plan(_request(max_turns=5))
    assert "--max-turns" not in plan.argv
    assert "5" not in plan.argv


def test_plan_extra_cli_args_appended_before_prompt() -> None:
    plan = build_cli_plan(
        _request(adapter_options={"extra_cli_args": ["--profile", "ci"]})
    )
    assert plan.argv[-3:] == ("--profile", "ci", "-")


def test_plan_skip_git_repo_check_flag() -> None:
    plan = build_cli_plan(
        _request(adapter_options={"skip_git_repo_check": True})
    )
    assert "--skip-git-repo-check" in plan.argv
    plan = build_cli_plan(_request())
    assert "--skip-git-repo-check" not in plan.argv


def test_plan_account_only_denies_openai_api_key() -> None:
    plan = build_cli_plan(
        _request(authentication_policy=AuthenticationPolicy.ACCOUNT_ONLY)
    )
    assert plan.denied_environment == ("OPENAI_API_KEY",)


def test_plan_command_override_replaces_executable() -> None:
    plan = build_cli_plan(
        _request(command_override=(sys.executable, str(FAKE_CODEX)))
    )
    assert plan.argv[:2] == (sys.executable, str(FAKE_CODEX))
    assert plan.argv[2:4] == ("exec", "--json")
