"""The acceptance-criterion test (design doc section 18).

An application changes ``agent_id`` (plus the fake executable standing in for
the vendor CLI) and nothing else: the orchestration helper below builds the
configuration and request identically for every agent and contains no
per-agent branching. Both first-party adapters must complete a run through
it. flywheel_core is deliberately not imported here — the LOOP_STATUS
envelope is asserted as round-tripped text, not parsed.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

from flywheel_agents import (
    AgentConfiguration,
    AgentRuntime,
    PermissionPolicy,
    RunRequest,
    StopReason,
)

FAKE_AGENT = Path(__file__).parent / "fake_agent.py"
FAKE_CODEX = Path(__file__).parent / "fake_codex.py"


def _orchestrate(
    agent_id: str,
    command_override: tuple[str, ...],
    environment: dict[str, str],
    working_directory: Path,
) -> tuple[str, StopReason, bool, bool]:
    """Agent-agnostic orchestration: no branching on agent_id anywhere."""
    configuration = AgentConfiguration(
        agent_id=agent_id,
        permission_policy=PermissionPolicy.AUTO,
        command_override=command_override,
        environment=environment,
    )
    request = RunRequest(
        prompt="do the task",
        working_directory=working_directory,
        configuration=configuration,
    )
    run = asyncio.run(AgentRuntime().run(request))
    return (
        run.final_text,
        run.stop.reason,
        run.usage is not None,
        run.native_session_id is not None,
    )


def test_agent_swap_changes_only_agent_id(tmp_path: Path) -> None:
    results = {
        "claude-code": _orchestrate(
            "claude-code",
            (sys.executable, str(FAKE_AGENT)),
            {"FAKE_AGENT_SCENARIO": "happy"},
            tmp_path,
        ),
        "codex": _orchestrate(
            "codex",
            (sys.executable, str(FAKE_CODEX)),
            {"FAKE_CODEX_SCENARIO": "happy"},
            tmp_path,
        ),
    }
    for agent_id, (final_text, stop, has_usage, has_session) in results.items():
        assert stop is StopReason.COMPLETED, agent_id
        assert final_text, agent_id
        assert has_usage, agent_id
        assert has_session, agent_id


def test_final_text_round_trips_through_both_adapters(tmp_path: Path) -> None:
    # codex: the envelope scenario's LOOP_STATUS block survives the fold
    # verbatim (parsing it is flywheel_core's job, not this package's).
    codex_text, codex_stop, _, _ = _orchestrate(
        "codex",
        (sys.executable, str(FAKE_CODEX)),
        {"FAKE_CODEX_SCENARIO": "envelope"},
        tmp_path,
    )
    assert codex_stop is StopReason.COMPLETED
    assert "<!-- LOOP_STATUS -->" in codex_text
    assert "<!-- /LOOP_STATUS -->" in codex_text
    assert '"intent": "verify"' in codex_text
    # claude: the happy scenario carries no envelope; assert the assistant
    # text round-trips exactly instead.
    claude_text, claude_stop, _, _ = _orchestrate(
        "claude-code",
        (sys.executable, str(FAKE_AGENT)),
        {"FAKE_AGENT_SCENARIO": "happy"},
        tmp_path,
    )
    assert claude_stop is StopReason.COMPLETED
    assert claude_text == "Hello, world."
