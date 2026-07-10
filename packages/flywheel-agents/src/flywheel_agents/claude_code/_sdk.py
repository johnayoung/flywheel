"""Claude Code SDK transport: the official ``claude-agent-sdk`` client.

Local-only — the SDK owns its subprocess, so there is no execution host and
no exit code to report. This module is imported lazily by the adapter
(``adapter_options={"transport": "sdk"}``); the top-level SDK import below is
the optional-extra boundary the adapter catches as ``ModuleNotFoundError``.

Normalization is payload-compatible with the CLI transport (``_cli``): both
transports feed :class:`flywheel_agents.fold.EventFolder` the same payload
keys, so a run folds identically regardless of how it was driven.
"""

from __future__ import annotations

import dataclasses
import os
from collections.abc import AsyncIterator, Mapping
from typing import Any

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    ClaudeSDKClient,
    RateLimitEvent,
    ResultMessage,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
)

from flywheel_agents.adapter import AdapterServices, RunningAgent
from flywheel_agents.claude_code._common import coerce_usage, normalize_stop
from flywheel_agents.config import AuthenticationPolicy, PermissionPolicy
from flywheel_agents.errors import (
    AuthenticationPolicyError,
    UnsupportedCapabilityError,
)
from flywheel_agents.events import AgentEvent, EventType
from flywheel_agents.models import AgentExit, RunRequest

_PROTOCOL = "sdk"
_STREAM = "sdk"
_ANTHROPIC_API_KEY = "ANTHROPIC_API_KEY"

# Same inline settings layer flywheel_core.workflow._NO_COAUTHOR_SETTINGS and
# the CLI transport inject: suppress the AI co-author git trailer at source.
_NO_COAUTHOR_SETTINGS = '{"includeCoAuthoredBy": false}'

_PERMISSION_MODES: dict[PermissionPolicy, str] = {
    PermissionPolicy.AUTO: "bypassPermissions",
    PermissionPolicy.SUPERVISED: "default",
    PermissionPolicy.PLAN: "plan",
}


def build_sdk_options(request: RunRequest) -> ClaudeAgentOptions:
    """Construct the ``ClaudeAgentOptions`` for one run.

    Mirrors ``flywheel_core.workflow.build_agent_options`` semantics, sourced
    from :class:`~flywheel_agents.config.AgentConfiguration`: fields are
    omitted at their SDK sentinel when unset. The one always-on-by-default
    exception is ``settings`` (co-author suppression); opt out via
    ``adapter_options={"suppress_coauthor": False}``.
    """
    cfg = request.configuration
    mode = _PERMISSION_MODES.get(cfg.permission_policy)
    if mode is None:
        raise UnsupportedCapabilityError(
            "claude-code has no read-only permission mode; use PLAN or AUTO"
        )
    if (
        cfg.authentication_policy is AuthenticationPolicy.ACCOUNT_ONLY
        and os.environ.get(_ANTHROPIC_API_KEY)
    ):
        # The SDK subprocess inherits os.environ, so the key cannot be
        # stripped through options — mirror the container backend's guard
        # (flywheel_container._auth.ClaudeAuth.resolve) and refuse to start.
        raise AuthenticationPolicyError(
            f"authentication policy is account-only but {_ANTHROPIC_API_KEY} "
            "is set in the host environment; the SDK subprocess inherits it "
            "and the CLI prefers the key over the subscription session. "
            f"Unset {_ANTHROPIC_API_KEY} to run account-only."
        )
    workdir = str(request.working_directory)
    kwargs: dict[str, Any] = dict(
        cwd=workdir,
        add_dirs=[workdir],
        permission_mode=mode,
    )
    if cfg.model_id:
        kwargs["model"] = cfg.model_id
    if cfg.max_turns is not None:
        kwargs["max_turns"] = cfg.max_turns
    if cfg.adapter_options.get("suppress_coauthor", True):
        kwargs["settings"] = _NO_COAUTHOR_SETTINGS
    skills = cfg.adapter_options.get("skills", "all")
    if skills:
        kwargs["skills"] = list(skills) if isinstance(skills, (list, tuple)) else skills
    allowed_tools = cfg.adapter_options.get("allowed_tools")
    if isinstance(allowed_tools, (list, tuple)) and allowed_tools:
        kwargs["allowed_tools"] = list(allowed_tools)
    disallowed_tools = cfg.adapter_options.get("disallowed_tools")
    if isinstance(disallowed_tools, (list, tuple)) and disallowed_tools:
        kwargs["disallowed_tools"] = list(disallowed_tools)
    setting_sources = cfg.adapter_options.get("setting_sources")
    if isinstance(setting_sources, (list, tuple)) and setting_sources:
        kwargs["setting_sources"] = list(setting_sources)
    mcp_servers = cfg.adapter_options.get("mcp_servers")
    if mcp_servers:
        if isinstance(mcp_servers, Mapping):
            kwargs["mcp_servers"] = dict(mcp_servers)
        elif isinstance(mcp_servers, (list, tuple)):
            kwargs["mcp_servers"] = list(mcp_servers)
        else:
            kwargs["mcp_servers"] = mcp_servers
    if cfg.adapter_options.get("mcp_strict"):
        kwargs["strict_mcp_config"] = True
    sandbox_exec = cfg.adapter_options.get("sandbox_exec")
    if isinstance(sandbox_exec, Mapping) and sandbox_exec:
        kwargs["sandbox"] = dict(sandbox_exec)
    if cfg.environment:
        kwargs["env"] = dict(cfg.environment)
    return ClaudeAgentOptions(**kwargs)


def _to_jsonable(value: Any) -> Any:
    """Recursively project ``value`` onto JSON-compatible primitives.

    Ports ``flywheel_core.invoker._to_jsonable``: dataclasses via their
    fields, mappings/sequences recursively, then ``vars()``, then ``repr``.
    """
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {
            f.name: _to_jsonable(getattr(value, f.name))
            for f in dataclasses.fields(value)
        }
    if isinstance(value, Mapping):
        return {str(k): _to_jsonable(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_to_jsonable(v) for v in value]
    try:
        return {str(k): _to_jsonable(v) for k, v in vars(value).items()}
    except TypeError:
        return repr(value)


def _project(value: object) -> object:
    """Total JSON-safe projection of one SDK object. Never raises."""
    try:
        return _to_jsonable(value)
    except Exception:  # noqa: BLE001 - the raw record must always land.
        return repr(value)


def _raw(msg: object) -> AgentEvent:
    return AgentEvent(
        type=EventType.RAW,
        native_type=type(msg).__name__,
        payload={"data": _project(msg)},
    )


class SdkMessageNormalizer:
    """Stateful SDK-message normalizer, payload-compatible with ``_cli``.

    Stateful because the SDK splits signals across messages exactly like the
    stream-json protocol underneath it: assistant messages carry
    ``stop_reason``/``usage`` while the terminal :class:`ResultMessage`
    carries turns/cost/subtype — the ``session.finished`` payload reunites
    them.
    """

    def __init__(self) -> None:
        self.session_id: str | None = None
        self._last_stop_reason: str | None = None

    def normalize(self, msg: object) -> tuple[AgentEvent, ...]:
        """Map one SDK message to normalized events. Unknowns become RAW."""
        if isinstance(msg, AssistantMessage):
            return self._assistant(msg)
        if isinstance(msg, UserMessage):
            return self._user(msg)
        if isinstance(msg, ResultMessage):
            return self._result(msg)
        if isinstance(msg, RateLimitEvent):
            return self._rate_limit(msg)
        return (_raw(msg),)

    def _assistant(self, msg: AssistantMessage) -> tuple[AgentEvent, ...]:
        if isinstance(msg.session_id, str) and msg.session_id:
            self.session_id = msg.session_id
        if isinstance(msg.stop_reason, str):
            self._last_stop_reason = msg.stop_reason
        events: list[AgentEvent] = []
        for block in msg.content:
            if isinstance(block, TextBlock):
                events.append(
                    AgentEvent(
                        type=EventType.ASSISTANT_MESSAGE,
                        native_type="AssistantMessage",
                        payload={"text": block.text},
                    )
                )
            elif isinstance(block, ThinkingBlock):
                events.append(
                    AgentEvent(
                        type=EventType.THOUGHT,
                        native_type="AssistantMessage",
                        payload={"thinking": block.thinking},
                    )
                )
            elif isinstance(block, ToolUseBlock):
                events.append(
                    AgentEvent(
                        type=EventType.TOOL_CALL_STARTED,
                        native_type="AssistantMessage",
                        payload={
                            "tool_use_id": block.id,
                            "tool_name": block.name,
                            "tool_input": block.input
                            if isinstance(block.input, Mapping)
                            else {},
                        },
                    )
                )
        usage = coerce_usage(msg.usage)
        if usage:
            events.append(
                AgentEvent(
                    type=EventType.CONTEXT_USAGE,
                    native_type="AssistantMessage",
                    payload={"usage": usage},
                )
            )
        if not events:
            return (_raw(msg),)
        return tuple(events)

    def _user(self, msg: UserMessage) -> tuple[AgentEvent, ...]:
        events: list[AgentEvent] = []
        if isinstance(msg.content, list):
            for block in msg.content:
                if isinstance(block, ToolResultBlock):
                    events.append(
                        AgentEvent(
                            type=EventType.TOOL_CALL_FINISHED,
                            native_type="UserMessage",
                            payload={
                                "tool_use_id": block.tool_use_id,
                                "is_error": block.is_error
                                if isinstance(block.is_error, bool)
                                else None,
                                "content": block.content,
                            },
                        )
                    )
        if not events:
            return (_raw(msg),)
        return tuple(events)

    def _result(self, msg: ResultMessage) -> tuple[AgentEvent, ...]:
        if isinstance(msg.session_id, str) and msg.session_id:
            self.session_id = msg.session_id
        events: list[AgentEvent] = []
        for denial in msg.permission_denials or []:
            if isinstance(denial, Mapping):
                tool_name = denial.get("tool_name")
                detail: object = {str(k): _project(v) for k, v in denial.items()}
            else:
                tool_name = getattr(denial, "tool_name", None)
                detail = _project(denial)
            events.append(
                AgentEvent(
                    type=EventType.PERMISSION_DENIED,
                    native_type="ResultMessage",
                    payload={
                        "tool_name": tool_name
                        if isinstance(tool_name, str)
                        else None,
                        "denial": detail,
                    },
                )
            )
        native_stop = self._last_stop_reason or (
            msg.stop_reason if isinstance(msg.stop_reason, str) else None
        )
        subtype = msg.subtype if isinstance(msg.subtype, str) else None
        is_error = msg.is_error is True
        num_turns = msg.num_turns
        cost = msg.total_cost_usd
        result_text = msg.result
        payload: dict[str, Any] = {
            "normalized_stop": normalize_stop(
                subtype=subtype,
                native_stop=native_stop,
                is_error=is_error,
            ).value,
            "stop_reason": native_stop,
            "subtype": subtype,
            "is_error": is_error,
            "num_turns": num_turns
            if isinstance(num_turns, int) and not isinstance(num_turns, bool)
            else None,
            "total_cost_usd": cost
            if isinstance(cost, (int, float)) and not isinstance(cost, bool)
            else None,
            "result_text": result_text if isinstance(result_text, str) else None,
            "session_id": self.session_id,
        }
        usage = coerce_usage(msg.usage)
        if usage:
            payload["usage"] = usage
        events.append(
            AgentEvent(
                type=EventType.SESSION_FINISHED,
                native_type="ResultMessage",
                payload=payload,
            )
        )
        return tuple(events)

    def _rate_limit(self, msg: RateLimitEvent) -> tuple[AgentEvent, ...]:
        if isinstance(msg.session_id, str) and msg.session_id:
            self.session_id = msg.session_id
        info = msg.rate_limit_info
        resets_at = getattr(info, "resets_at", None)
        return (
            AgentEvent(
                type=EventType.RATE_LIMITED,
                native_type="RateLimitEvent",
                payload={
                    "resets_at_epoch": float(resets_at)
                    if isinstance(resets_at, (int, float))
                    and not isinstance(resets_at, bool)
                    else None,
                    "detail": _project(info),
                },
            ),
        )


class SdkRunningAgent(RunningAgent):
    """A live SDK-driven run. The SDK owns its subprocess.

    SDK exceptions raised while draining ``events()`` propagate — the runtime
    catches them and folds a structured ``RunFailure``.
    """

    def __init__(self, client: ClaudeSDKClient, services: AdapterServices) -> None:
        self._client = client
        self._services = services
        self._normalizer = SdkMessageNormalizer()
        self._closed = False

    @property
    def native_session_id(self) -> str | None:
        return self._normalizer.session_id

    async def events(self) -> AsyncIterator[AgentEvent]:
        async for msg in self._client.receive_response():
            self._services.emit_raw(
                protocol=_PROTOCOL,
                stream=_STREAM,
                data=_project(msg),
                native_type=type(msg).__name__,
            )
            for event in self._normalizer.normalize(msg):
                yield event

    async def cancel(self) -> None:
        try:
            await self._client.interrupt()
        except Exception:  # noqa: BLE001 - best-effort, mirrors the CLI kill.
            pass
        await self._close()

    async def wait(self) -> AgentExit:
        # The SDK owns its subprocess: there is no exit code to report, only
        # the guarantee that the client connection is closed.
        await self._close()
        return AgentExit(returncode=None)

    async def _close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            await self._client.disconnect()
        except Exception:  # noqa: BLE001 - a failed teardown must not mask
            # the already-folded run outcome.
            pass


async def start_sdk_agent(
    request: RunRequest, services: AdapterServices
) -> RunningAgent:
    """Connect a ``ClaudeSDKClient`` and send the prompt for one run."""
    options = build_sdk_options(request)
    client = ClaudeSDKClient(options)
    await client.connect()
    try:
        await client.query(request.prompt)
    except BaseException:
        # Never leak a connected client when the send fails.
        try:
            await client.disconnect()
        except Exception:  # noqa: BLE001 - the original error wins.
            pass
        raise
    return SdkRunningAgent(client, services)
