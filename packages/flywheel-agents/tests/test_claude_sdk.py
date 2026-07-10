"""Tests for the claude-agent-sdk transport.

No live client, no network, no claude subprocess: option construction is
pure, and normalization is exercised by feeding real ``claude_agent_sdk``
dataclass instances through :class:`SdkMessageNormalizer`. The parity test
folds the same logical exchange through both transports' normalizers and
asserts the :class:`CompletedRun` outputs agree.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any, cast

import pytest
from claude_agent_sdk import (
    AssistantMessage,
    ClaudeSDKClient,
    HookEventMessage,
    ProcessError,
    RateLimitEvent,
    RateLimitInfo,
    ResultMessage,
    SystemMessage,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
)

from flywheel_agents.adapter import AdapterServices
from flywheel_agents.claude_code import ClaudeStreamNormalizer
from flywheel_agents.claude_code._sdk import (
    SdkMessageNormalizer,
    SdkRunningAgent,
    build_sdk_options,
)
from flywheel_agents.config import (
    AgentConfiguration,
    AuthenticationPolicy,
    PermissionPolicy,
)
from flywheel_agents.errors import (
    AuthenticationPolicyError,
    UnsupportedCapabilityError,
)
from flywheel_agents.events import AgentEvent, EventType
from flywheel_agents.fold import EventFolder
from flywheel_agents.hosts import LocalHost
from flywheel_agents.models import AgentExit, CompletedRun, RunRequest, StopReason

_API_KEY = "ANTHROPIC_API_KEY"


def _request(**config: Any) -> RunRequest:
    kwargs: dict[str, Any] = {"agent_id": "claude-code"}
    kwargs.update(config)
    return RunRequest(
        prompt="do the task",
        working_directory=Path("/tmp/sandbox"),
        configuration=AgentConfiguration(**kwargs),
    )


# --- build_sdk_options -------------------------------------------------------


def test_cwd_and_add_dirs_from_working_directory() -> None:
    options = build_sdk_options(_request())
    assert options.cwd == "/tmp/sandbox"
    assert options.add_dirs == ["/tmp/sandbox"]


def test_permission_auto_maps_bypass() -> None:
    options = build_sdk_options(
        _request(permission_policy=PermissionPolicy.AUTO)
    )
    assert options.permission_mode == "bypassPermissions"


def test_permission_supervised_maps_default() -> None:
    options = build_sdk_options(
        _request(permission_policy=PermissionPolicy.SUPERVISED)
    )
    assert options.permission_mode == "default"


def test_permission_plan_maps_plan() -> None:
    options = build_sdk_options(
        _request(permission_policy=PermissionPolicy.PLAN)
    )
    assert options.permission_mode == "plan"


def test_permission_read_only_raises() -> None:
    with pytest.raises(UnsupportedCapabilityError):
        build_sdk_options(
            _request(permission_policy=PermissionPolicy.READ_ONLY)
        )


def test_model_and_max_turns_passthrough() -> None:
    options = build_sdk_options(
        _request(model_id="claude-opus-4", max_turns=7)
    )
    assert options.model == "claude-opus-4"
    assert options.max_turns == 7


def test_model_and_max_turns_omitted_when_unset() -> None:
    options = build_sdk_options(_request())
    assert options.model is None
    assert options.max_turns is None


def test_suppress_coauthor_default_settings() -> None:
    options = build_sdk_options(_request())
    assert options.settings == '{"includeCoAuthoredBy": false}'


def test_suppress_coauthor_opt_out() -> None:
    options = build_sdk_options(
        _request(adapter_options={"suppress_coauthor": False})
    )
    assert options.settings is None


def test_skills_defaults_to_all() -> None:
    options = build_sdk_options(_request())
    assert options.skills == "all"


def test_skills_explicit_list_passthrough() -> None:
    options = build_sdk_options(
        _request(adapter_options={"skills": ("alpha", "beta")})
    )
    assert options.skills == ["alpha", "beta"]


def test_tool_lists_passthrough() -> None:
    options = build_sdk_options(
        _request(
            adapter_options={
                "allowed_tools": ("Read", "Bash"),
                "disallowed_tools": ["WebSearch"],
            }
        )
    )
    assert options.allowed_tools == ["Read", "Bash"]
    assert options.disallowed_tools == ["WebSearch"]


def test_tool_lists_omitted_by_default() -> None:
    options = build_sdk_options(_request())
    assert options.allowed_tools == []
    assert options.disallowed_tools == []


def test_setting_sources_passthrough() -> None:
    options = build_sdk_options(
        _request(adapter_options={"setting_sources": ("user", "project")})
    )
    assert options.setting_sources == ["user", "project"]
    assert build_sdk_options(_request()).setting_sources is None


def test_mcp_servers_and_strict_passthrough() -> None:
    servers = {"serena": {"type": "stdio", "command": "serena"}}
    options = build_sdk_options(
        _request(adapter_options={"mcp_servers": servers, "mcp_strict": True})
    )
    assert options.mcp_servers == servers
    assert options.strict_mcp_config is True


def test_mcp_strict_defaults_false() -> None:
    options = build_sdk_options(_request())
    assert options.strict_mcp_config is False
    assert options.mcp_servers == {}


def test_sandbox_exec_mapping_passthrough() -> None:
    sandbox = {"enabled": True, "autoAllowBashIfSandboxed": True}
    options = build_sdk_options(
        _request(adapter_options={"sandbox_exec": sandbox})
    )
    assert options.sandbox == sandbox
    assert build_sdk_options(_request()).sandbox is None


def test_environment_passthrough() -> None:
    options = build_sdk_options(_request(environment={"FOO": "bar"}))
    assert options.env == {"FOO": "bar"}
    assert build_sdk_options(_request()).env == {}


def test_account_only_raises_when_api_key_set(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(_API_KEY, "sk-test-secret")
    with pytest.raises(AuthenticationPolicyError) as excinfo:
        build_sdk_options(
            _request(authentication_policy=AuthenticationPolicy.ACCOUNT_ONLY)
        )
    assert _API_KEY in str(excinfo.value)
    assert "sk-test-secret" not in str(excinfo.value)


def test_account_only_passes_without_api_key(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv(_API_KEY, raising=False)
    options = build_sdk_options(
        _request(authentication_policy=AuthenticationPolicy.ACCOUNT_ONLY)
    )
    assert options.permission_mode == "default"


def test_account_only_ignores_empty_api_key(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv(_API_KEY, "")
    build_sdk_options(
        _request(authentication_policy=AuthenticationPolicy.ACCOUNT_ONLY)
    )


# --- message normalization ---------------------------------------------------


def _assistant_turn_one() -> AssistantMessage:
    return AssistantMessage(
        content=[
            TextBlock(text="Let me check."),
            ThinkingBlock(thinking="planning the check", signature="sig-1"),
            ToolUseBlock(id="tu-1", name="Bash", input={"command": "ls"}),
        ],
        model="claude-opus-4",
        usage={"input_tokens": 10, "output_tokens": 5},
        session_id="sess-1",
    )


def _tool_result_turn() -> UserMessage:
    return UserMessage(
        content=[
            ToolResultBlock(tool_use_id="tu-1", content="ok", is_error=False)
        ]
    )


def _assistant_turn_two() -> AssistantMessage:
    return AssistantMessage(
        content=[TextBlock(text=" Done.")],
        model="claude-opus-4",
        stop_reason="end_turn",
        usage={"input_tokens": 20, "output_tokens": 9},
        session_id="sess-1",
    )


def _result_message(**overrides: Any) -> ResultMessage:
    kwargs: dict[str, Any] = dict(
        subtype="success",
        duration_ms=1200,
        duration_api_ms=900,
        is_error=False,
        num_turns=2,
        session_id="sess-1",
        total_cost_usd=0.01,
        usage={"input_tokens": 20, "output_tokens": 9},
        result="Let me check. Done.",
    )
    kwargs.update(overrides)
    return ResultMessage(**kwargs)


def test_assistant_message_blocks_normalize() -> None:
    normalizer = SdkMessageNormalizer()
    events = normalizer.normalize(_assistant_turn_one())
    assert [e.type for e in events] == [
        EventType.ASSISTANT_MESSAGE,
        EventType.THOUGHT,
        EventType.TOOL_CALL_STARTED,
        EventType.CONTEXT_USAGE,
    ]
    assert events[0].payload == {"text": "Let me check."}
    assert events[1].payload == {"thinking": "planning the check"}
    assert events[2].payload == {
        "tool_use_id": "tu-1",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
    }
    assert events[3].payload == {
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        }
    }
    assert normalizer.session_id == "sess-1"


def test_user_message_tool_result_normalizes() -> None:
    events = SdkMessageNormalizer().normalize(_tool_result_turn())
    assert [e.type for e in events] == [EventType.TOOL_CALL_FINISHED]
    assert events[0].payload == {
        "tool_use_id": "tu-1",
        "is_error": False,
        "content": "ok",
    }


def test_result_message_normalizes_finish_and_denials() -> None:
    normalizer = SdkMessageNormalizer()
    normalizer.normalize(_assistant_turn_two())  # sets last stop_reason
    denial = {
        "tool_name": "Bash",
        "tool_use_id": "tu-9",
        "tool_input": {"command": "rm -rf /"},
    }
    events = normalizer.normalize(_result_message(permission_denials=[denial]))
    assert [e.type for e in events] == [
        EventType.PERMISSION_DENIED,
        EventType.SESSION_FINISHED,
    ]
    assert events[0].payload["tool_name"] == "Bash"
    assert events[0].payload["denial"] == denial
    finished = events[1].payload
    assert finished == {
        "normalized_stop": StopReason.COMPLETED.value,
        "stop_reason": "end_turn",
        "subtype": "success",
        "is_error": False,
        "num_turns": 2,
        "total_cost_usd": 0.01,
        "result_text": "Let me check. Done.",
        "session_id": "sess-1",
        "usage": {
            "input_tokens": 20,
            "output_tokens": 9,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        },
    }


def test_result_message_falls_back_to_own_stop_reason() -> None:
    events = SdkMessageNormalizer().normalize(
        _result_message(subtype="unrecognized", stop_reason="end_turn")
    )
    finished = events[-1].payload
    assert finished["stop_reason"] == "end_turn"
    assert finished["normalized_stop"] == StopReason.COMPLETED.value


def test_result_message_max_turns_subtype() -> None:
    events = SdkMessageNormalizer().normalize(
        _result_message(subtype="error_max_turns", is_error=True)
    )
    finished = events[-1].payload
    assert finished["normalized_stop"] == StopReason.MAX_TURNS.value
    assert finished["is_error"] is True


def test_rate_limit_event_normalizes() -> None:
    normalizer = SdkMessageNormalizer()
    event = normalizer.normalize(
        RateLimitEvent(
            rate_limit_info=RateLimitInfo(
                status="allowed_warning", resets_at=1751990400
            ),
            uuid="u-1",
            session_id="sess-1",
        )
    )[0]
    assert event.type is EventType.RATE_LIMITED
    assert event.payload["resets_at_epoch"] == 1751990400.0
    detail = event.payload["detail"]
    assert isinstance(detail, dict)
    assert detail["status"] == "allowed_warning"
    assert normalizer.session_id == "sess-1"


def test_rate_limit_event_without_reset_instant() -> None:
    event = SdkMessageNormalizer().normalize(
        RateLimitEvent(
            rate_limit_info=RateLimitInfo(status="allowed"),
            uuid="u-2",
            session_id="sess-1",
        )
    )[0]
    assert event.payload["resets_at_epoch"] is None


def test_unrecognized_messages_become_raw() -> None:
    normalizer = SdkMessageNormalizer()
    system = normalizer.normalize(
        SystemMessage(subtype="init", data={"session_id": "sess-1"})
    )
    assert [e.type for e in system] == [EventType.RAW]
    assert system[0].native_type == "SystemMessage"
    assert system[0].payload["data"] == {
        "subtype": "init",
        "data": {"session_id": "sess-1"},
    }
    hook = normalizer.normalize(
        HookEventMessage(
            subtype="hook", data={"x": 1}, hook_event_name="PostToolUse"
        )
    )
    assert [e.type for e in hook] == [EventType.RAW]
    assert hook[0].native_type == "HookEventMessage"


def _sdk_exchange() -> list[object]:
    return [
        _assistant_turn_one(),
        _tool_result_turn(),
        _assistant_turn_two(),
        _result_message(),
    ]


def _fold(events: list[AgentEvent]) -> CompletedRun:
    folder = EventFolder()
    for event in events:
        folder.feed(event)
    return folder.completed(exit=AgentExit(returncode=None))


def test_sdk_events_fold_to_completed_run() -> None:
    normalizer = SdkMessageNormalizer()
    events: list[AgentEvent] = []
    for msg in _sdk_exchange():
        events.extend(normalizer.normalize(msg))
    run = _fold(events)
    assert run.final_text == "Let me check. Done."
    assert run.stop.reason is StopReason.COMPLETED
    assert run.stop.native == "end_turn"
    assert run.stop.native_subtype == "success"
    assert run.stop.is_error is False
    assert run.stop.pending_tool_use is False
    assert run.usage == {
        "input_tokens": 20,
        "output_tokens": 9,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    }
    assert run.num_turns == 2
    assert run.total_cost_usd == 0.01
    assert run.native_session_id == "sess-1"
    assert len(run.tool_interactions) == 1
    interaction = run.tool_interactions[0]
    assert interaction.tool_use_id == "tu-1"
    assert interaction.tool_name == "Bash"
    assert interaction.tool_input == {"command": "ls"}
    assert interaction.result is not None
    assert interaction.result.is_error is False
    assert interaction.result.content == "ok"


# --- transport parity --------------------------------------------------------


def _cli_exchange_lines() -> list[dict[str, Any]]:
    return [
        {
            "type": "system",
            "subtype": "init",
            "session_id": "sess-1",
            "model": "claude-opus-4",
        },
        {
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "Let me check."},
                    {
                        "type": "thinking",
                        "thinking": "planning the check",
                    },
                    {
                        "type": "tool_use",
                        "id": "tu-1",
                        "name": "Bash",
                        "input": {"command": "ls"},
                    },
                ],
                "usage": {"input_tokens": 10, "output_tokens": 5},
            },
        },
        {
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu-1",
                        "is_error": False,
                        "content": "ok",
                    }
                ]
            },
        },
        {
            "type": "assistant",
            "message": {
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": " Done."}],
                "usage": {"input_tokens": 20, "output_tokens": 9},
            },
        },
        {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "num_turns": 2,
            "total_cost_usd": 0.01,
            "result": "Let me check. Done.",
            "usage": {"input_tokens": 20, "output_tokens": 9},
            "session_id": "sess-1",
        },
    ]


def test_sdk_and_cli_transports_fold_identically() -> None:
    sdk_normalizer = SdkMessageNormalizer()
    sdk_events: list[AgentEvent] = []
    for msg in _sdk_exchange():
        sdk_events.extend(sdk_normalizer.normalize(msg))
    sdk_run = _fold(sdk_events)

    cli_normalizer = ClaudeStreamNormalizer()
    cli_events: list[AgentEvent] = []
    for line in _cli_exchange_lines():
        _, events = cli_normalizer.feed_line(json.dumps(line))
        cli_events.extend(events)
    cli_run = _fold(cli_events)

    assert sdk_run.final_text == cli_run.final_text
    assert sdk_run.stop.reason is cli_run.stop.reason is StopReason.COMPLETED
    assert sdk_run.usage == cli_run.usage
    assert sdk_run.num_turns == cli_run.num_turns

    def _pairs(run: CompletedRun) -> list[tuple[str, str, bool | None]]:
        return [
            (
                i.tool_use_id,
                i.tool_name,
                i.result.is_error if i.result is not None else None,
            )
            for i in run.tool_interactions
        ]

    assert _pairs(sdk_run) == _pairs(cli_run) == [("tu-1", "Bash", False)]


# --- SdkRunningAgent lifecycle (fake client, no subprocess) -------------------


class _FakeClient:
    def __init__(
        self,
        messages: list[object],
        *,
        stream_error: Exception | None = None,
    ) -> None:
        self._messages = messages
        self._stream_error = stream_error
        self.disconnects = 0
        self.interrupts = 0
        self.interrupt_error: Exception | None = None

    async def receive_response(self) -> Any:
        for msg in self._messages:
            yield msg
        if self._stream_error is not None:
            raise self._stream_error

    async def interrupt(self) -> None:
        self.interrupts += 1
        if self.interrupt_error is not None:
            raise self.interrupt_error

    async def disconnect(self) -> None:
        self.disconnects += 1


def _agent(
    fake: _FakeClient, journal: list[tuple[str, Any]]
) -> SdkRunningAgent:
    def emit_raw(
        *,
        protocol: str,
        stream: str,
        data: object,
        native_type: str | None = None,
    ) -> None:
        journal.append(("raw", (protocol, stream, native_type)))

    services = AdapterServices(host=LocalHost(), emit_raw=emit_raw)
    return SdkRunningAgent(cast(ClaudeSDKClient, fake), services)


def test_running_agent_emits_raw_before_normalized() -> None:
    journal: list[tuple[str, Any]] = []
    fake = _FakeClient([_assistant_turn_two(), _result_message()])

    async def scenario() -> AgentExit:
        agent = _agent(fake, journal)
        async for event in agent.events():
            journal.append(("event", event.type))
        assert agent.native_session_id == "sess-1"
        first = await agent.wait()
        second = await agent.wait()
        assert second.returncode is None
        return first

    exit_info = asyncio.run(scenario())
    assert exit_info.returncode is None
    assert journal == [
        ("raw", ("sdk", "sdk", "AssistantMessage")),
        ("event", EventType.ASSISTANT_MESSAGE),
        ("event", EventType.CONTEXT_USAGE),
        ("raw", ("sdk", "sdk", "ResultMessage")),
        ("event", EventType.SESSION_FINISHED),
    ]
    # Double-close guard: two wait() calls, one disconnect.
    assert fake.disconnects == 1


def test_running_agent_cancel_swallows_interrupt_error() -> None:
    journal: list[tuple[str, Any]] = []
    fake = _FakeClient([])
    fake.interrupt_error = RuntimeError("session already closed")

    async def scenario() -> AgentExit:
        agent = _agent(fake, journal)
        await agent.cancel()
        return await agent.wait()

    exit_info = asyncio.run(scenario())
    assert exit_info.returncode is None
    assert fake.interrupts == 1
    assert fake.disconnects == 1


def test_running_agent_propagates_stream_errors() -> None:
    journal: list[tuple[str, Any]] = []
    fake = _FakeClient(
        [_assistant_turn_two()],
        stream_error=ProcessError("boom", exit_code=3),
    )

    async def scenario() -> None:
        agent = _agent(fake, journal)
        async for _ in agent.events():
            pass

    with pytest.raises(ProcessError):
        asyncio.run(scenario())
    # The message before the failure was still normalized and recorded.
    assert ("raw", ("sdk", "sdk", "AssistantMessage")) in journal
