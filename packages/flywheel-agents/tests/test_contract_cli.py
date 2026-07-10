"""Adapter contract suite (design doc section 17) through the real stack.

``AgentRuntime`` + ``LocalHost`` + the claude-code CLI adapter drive the
deterministic fake vendor CLI (``tests/fake_agent.py``) end to end: real
subprocess, real stream-json parsing, real fold. No vendor CLI required.
"""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path

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
)

FAKE_AGENT = Path(__file__).parent / "fake_agent.py"

# Mirrors fake_agent.py's happy-path constants.
RESULT_USAGE = {
    "input_tokens": 100,
    "output_tokens": 25,
    "cache_creation_input_tokens": 7,
    "cache_read_input_tokens": 3,
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
    env = {"FAKE_AGENT_SCENARIO": scenario}
    if environment:
        env.update(environment)
    configuration = AgentConfiguration(
        agent_id="claude-code",
        permission_policy=PermissionPolicy.AUTO,
        authentication_policy=authentication_policy,
        command_override=(sys.executable, str(FAKE_AGENT)),
        environment=env,
    )
    request = RunRequest(
        prompt="exercise the fake agent",
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
    assert started[0].payload["session_id"] == "sess-1"
    assert run.final_text == "Hello, world."
    assert run.stop.reason is StopReason.COMPLETED
    assert run.stop.native_subtype == "success"
    assert run.stop.is_error is False
    assert run.stop.pending_tool_use is False
    assert run.usage == RESULT_USAGE  # the result envelope's usage wins
    assert run.num_turns == 3
    assert run.total_cost_usd == 0.0125
    assert run.native_session_id == "sess-1"
    assert run.failure is None
    assert run.fault is None
    assert run.exit.returncode == 0
    assert len(run.tool_interactions) == 1
    interaction = run.tool_interactions[0]
    assert interaction.tool_use_id == "tool-1"
    assert interaction.tool_name == "Bash"
    assert interaction.tool_input == {"command": "echo hi"}
    assert interaction.result is not None
    assert interaction.result.is_error is False


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
    assert all(e.source == "claude-code" for e in sink.events)
    assert all(e.adapter_id == "claude-code" for e in sink.raw)


def test_tool_error_reported_on_interaction(tmp_path: Path) -> None:
    run, _ = run_scenario("tool_error", tmp_path)
    assert run.failure is None
    assert len(run.tool_interactions) == 1
    interaction = run.tool_interactions[0]
    assert interaction.tool_use_id == "tool-err"
    assert interaction.result is not None
    assert interaction.result.is_error is True


def test_malformed_lines_become_raw_and_run_completes(tmp_path: Path) -> None:
    run, sink = run_scenario("malformed", tmp_path)
    assert run.failure is None
    assert run.stop.reason is StopReason.COMPLETED
    assert run.final_text == "before garbage after garbage"
    raw_events = [e for e in sink.events if e.type is EventType.RAW]
    assert len(raw_events) >= 3
    assert any(e.native_type == "mystery" for e in raw_events)


def test_crash_without_result_is_agent_exit(tmp_path: Path) -> None:
    run, _ = run_scenario("crash", tmp_path)
    assert run.failure is not None
    assert run.failure.error_type == "agent_exit"
    assert run.failure.exit_code == 3
    captured = (run.failure.stderr or "") + (run.exit.stderr_tail or "")
    assert "fatal explosion" in captured
    assert run.stop.reason is StopReason.UNKNOWN
    assert run.exit.returncode == 3


def test_hang_bounded_by_timeout(tmp_path: Path) -> None:
    started = time.monotonic()
    run, _ = run_scenario("hang", tmp_path, timeout_seconds=2.0)
    elapsed = time.monotonic() - started
    assert elapsed < 15.0
    assert run.failure is not None
    assert run.failure.error_type == "timeout"
    assert run.stop.reason is StopReason.UNKNOWN


def test_stderr_tail_captured(tmp_path: Path) -> None:
    run, _ = run_scenario("stderr", tmp_path)
    assert run.failure is None
    assert run.exit.stderr_tail is not None
    assert "FAKE-AGENT-STDERR-MARKER" in run.exit.stderr_tail


def test_working_directory_respected(tmp_path: Path) -> None:
    run, _ = run_scenario("cwd", tmp_path)
    assert run.failure is None
    assert run.final_text
    assert Path(run.final_text).resolve() == tmp_path.resolve()


def test_environment_marker_passes_through(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-test")
    run, _ = run_scenario(
        "env", tmp_path, environment={"TEST_MARKER": "marker-123"}
    )
    assert run.failure is None
    reported = json.loads(run.final_text)
    assert reported == {"marker": "marker-123", "api_key": "sk-test"}


def test_account_only_strips_api_key(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-test")
    run, _ = run_scenario(
        "env",
        tmp_path,
        environment={"TEST_MARKER": "marker-123"},
        authentication_policy=AuthenticationPolicy.ACCOUNT_ONLY,
    )
    assert run.failure is None
    reported = json.loads(run.final_text)
    assert reported == {"marker": "marker-123", "api_key": None}


def test_usage_limit_classified_as_session_limit_fault(tmp_path: Path) -> None:
    run, _ = run_scenario("usage_limit", tmp_path)
    assert run.fault is not None
    assert run.fault.kind == "session_limit"
    assert run.fault.resets_at_epoch == 1751990400.0
    assert run.stop.reason is StopReason.ERROR


def test_no_result_clean_exit_is_unknown_stop(tmp_path: Path) -> None:
    run, _ = run_scenario("no_result", tmp_path)
    assert run.stop.reason is StopReason.UNKNOWN
    assert run.stop.pending_tool_use is False
    assert run.failure is None
    assert run.final_text == "partial work"
