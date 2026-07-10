"""Tests for the flywheel-agents bridge (``flywheel_core.agents_invoke``).

The fold test drives the pure mapping; the end-to-end test runs the real
stack — AgentRuntime + LocalHost + the claude-code adapter — against a
scripted stream-json executable, then asserts the IterationResult the harness
would receive, including envelope extraction and on_message forwarding.
"""

from __future__ import annotations

import asyncio
import sys
import textwrap
from pathlib import Path

from flywheel_agents import (
    AgentExit,
    CompletedRun,
    RunFailure,
    StopInfo,
    StopReason,
    ToolInteraction,
    ToolResult,
)
from flywheel_core.agents_invoke import (
    completed_run_to_iteration_result,
    make_agents_invoke,
)
from flywheel_core.envelope import ValidEnvelope
from flywheel_core.harness import InvocationRequest


def _completed(**overrides: object) -> CompletedRun:
    base: dict[str, object] = {
        "final_text": "did the work",
        "stop": StopInfo(
            reason=StopReason.COMPLETED,
            native="end_turn",
            native_subtype="success",
            is_error=False,
            pending_tool_use=False,
            finished=True,
        ),
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "total_cost_usd": 0.25,
        "num_turns": 3,
        "native_session_id": "sess-42",
        "tool_interactions": (
            ToolInteraction(
                tool_use_id="t1",
                tool_name="Bash",
                tool_input={"command": "ls"},
                result=ToolResult(tool_use_id="t1", is_error=False, content="ok"),
            ),
        ),
        "exit": AgentExit(returncode=0),
    }
    base.update(overrides)
    return CompletedRun(**base)  # type: ignore[arg-type]


def test_fold_maps_signals_fields() -> None:
    result = completed_run_to_iteration_result(_completed())
    assert result.transcript == "did the work"
    assert result.messages == ()
    assert result.usage == {"input_tokens": 10, "output_tokens": 5}
    signals = result.signals
    assert signals.stop_reason == "end_turn"
    assert signals.result_subtype == "success"
    assert signals.result_is_error is False
    assert signals.num_turns == 3
    assert signals.total_cost_usd == 0.25
    assert signals.session_id == "sess-42"
    assert signals.pending_tool_use_at_stop is False
    assert len(signals.tool_interactions) == 1
    interaction = signals.tool_interactions[0]
    assert interaction.tool_name == "Bash"
    assert interaction.tool_input == {"command": "ls"}
    assert interaction.result is not None
    assert interaction.result.is_error is False
    assert signals.tool_result_blocks[0].tool_use_id == "t1"
    assert result.failure is None


def test_fold_unfinished_run_reports_none_result_error() -> None:
    result = completed_run_to_iteration_result(
        _completed(
            stop=StopInfo(reason=StopReason.UNKNOWN, finished=False),
            failure=RunFailure(
                error_type="agent_exit", message="agent exited 3", exit_code=3
            ),
        )
    )
    assert result.signals.result_is_error is None
    assert result.failure is not None
    assert result.failure.error_type == "agent_exit"
    assert result.failure.exit_code == 3


def test_fold_coerces_nonstandard_tool_result_content() -> None:
    result = completed_run_to_iteration_result(
        _completed(
            tool_interactions=(
                ToolInteraction(
                    tool_use_id="t2",
                    tool_name="Read",
                    tool_input={},
                    result=ToolResult(
                        tool_use_id="t2",
                        is_error=None,
                        content=[{"type": "text", "text": "x"}, "stray"],
                    ),
                ),
            )
        )
    )
    block = result.signals.tool_result_blocks[0]
    assert block.content == [{"type": "text", "text": "x"}]


_SCRIPT = textwrap.dedent(
    """
    import json
    import sys

    sys.stdin.read()
    def emit(obj):
        print(json.dumps(obj), flush=True)

    emit({"type": "system", "subtype": "init", "session_id": "sess-e2e"})
    envelope = (
        "done\\n<!-- LOOP_STATUS -->\\n"
        + json.dumps({"intent": "verify", "reason": "complete"})
        + "\\n<!-- /LOOP_STATUS -->"
    )
    emit(
        {
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": envelope}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 7, "output_tokens": 2},
            },
        }
    )
    emit(
        {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "num_turns": 1,
            "total_cost_usd": 0.01,
            "usage": {"input_tokens": 7, "output_tokens": 2},
        }
    )
    """
)


def test_make_agents_invoke_end_to_end(tmp_path: Path) -> None:
    script = tmp_path / "scripted_agent.py"
    script.write_text(_SCRIPT)
    invoke = make_agents_invoke(
        agent_id="claude-code",
        working_directory=tmp_path,
        command_override=(sys.executable, str(script)),
        timeout_seconds=30,
    )
    observed: list[object] = []
    request = InvocationRequest(
        prompt="do the thing",
        transcript_graders=(),
        attempt_number=1,
        iteration_number=1,
        on_message=observed.append,
    )
    async def _run():
        return await invoke(request)

    result = asyncio.run(_run())
    assert isinstance(result.envelope, ValidEnvelope)
    assert result.envelope.intent == "verify"
    assert result.signals.session_id == "sess-e2e"
    assert result.signals.result_subtype == "success"
    assert result.signals.stop_reason == "end_turn"
    assert result.usage == {
        "input_tokens": 7,
        "output_tokens": 2,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }
    assert result.failure is None
    # Normalized events reached the harness observer (watchdog liveness).
    assert observed, "on_message never called"


def test_make_agents_invoke_requires_flywheel_agents() -> None:
    # The module itself imports without the extra; only invocation requires
    # it. With the extra installed (dev env), construction must succeed.
    invoke = make_agents_invoke(
        agent_id="claude-code", working_directory=Path(".")
    )
    assert callable(invoke)
