"""Claude Code CLI transport: headless ``--print --output-format stream-json``.

Works under any :class:`~flywheel_agents.hosts.ExecutionHost` — a local
process group or ``docker exec`` against a bind-mounted worktree. The argv
shape is the battle-tested container-backend command (prompt piped on stdin to
dodge ARG_MAX), upgraded from shell-string to argv execution.

Normalization covers strictly more than the historical
``flywheel_container._stream`` fold: tool_use / tool_result / thinking blocks
and permission denials become first-class events, which is what turns the
loop's stuck / thrash / hang guards live on CLI-driven runs.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator, Mapping
from typing import Any

from flywheel_agents.adapter import AdapterServices, RunningAgent
from flywheel_agents.claude_code._common import coerce_usage, normalize_stop
from flywheel_agents.config import AuthenticationPolicy, PermissionPolicy
from flywheel_agents.errors import UnsupportedCapabilityError
from flywheel_agents.events import AgentEvent, EventType
from flywheel_agents.hosts import ProcessPlan, RunningProcess
from flywheel_agents.models import AgentExit, RunRequest

_PROTOCOL = "jsonl"
_ANTHROPIC_API_KEY = "ANTHROPIC_API_KEY"


def build_cli_plan(request: RunRequest) -> ProcessPlan:
    """Build the headless stream-json invocation for one run."""
    cfg = request.configuration
    argv: list[str] = (
        list(cfg.command_override) if cfg.command_override else ["claude"]
    )
    argv.extend(["--print", "--verbose", "--output-format", "stream-json"])
    if cfg.model_id:
        argv.extend(["--model", cfg.model_id])
    if cfg.max_turns is not None:
        argv.extend(["--max-turns", str(cfg.max_turns)])
    if cfg.permission_policy is PermissionPolicy.AUTO:
        # Headless --print has no TTY to approve tool use; AUTO maps to the
        # CLI's bypass flag, exactly as the container backend always ran.
        argv.append("--dangerously-skip-permissions")
    elif cfg.permission_policy is PermissionPolicy.PLAN:
        argv.extend(["--permission-mode", "plan"])
    elif cfg.permission_policy is PermissionPolicy.READ_ONLY:
        raise UnsupportedCapabilityError(
            "claude-code has no read-only permission mode; use PLAN or AUTO"
        )
    if cfg.adapter_options.get("suppress_coauthor", True):
        # Same inline settings layer the flywheel host SDK path always
        # injects; the historical container command lacked it (a known
        # inconsistency this transport closes). Opt out via adapter_options.
        argv.extend(["--settings", '{"includeCoAuthoredBy": false}'])
    extra = cfg.adapter_options.get("extra_cli_args")
    if isinstance(extra, (list, tuple)):
        argv.extend(str(item) for item in extra)
    argv.extend(["-p", "-"])
    denied = (
        (_ANTHROPIC_API_KEY,)
        if cfg.authentication_policy is AuthenticationPolicy.ACCOUNT_ONLY
        else ()
    )
    return ProcessPlan(
        argv=tuple(argv),
        cwd=request.working_directory,
        environment=dict(cfg.environment),
        stdin_payload=request.prompt,
        denied_environment=denied,
    )


class ClaudeStreamNormalizer:
    """Stateful stream-json line normalizer.

    Stateful because the CLI splits signals across envelopes: assistant
    messages carry ``stop_reason``/``usage`` while the terminal ``result``
    envelope carries turns/cost/subtype — the ``session.finished`` payload
    reunites them.
    """

    def __init__(self) -> None:
        self.session_id: str | None = None
        self._last_stop_reason: str | None = None

    def feed_line(self, line: str) -> tuple[str | None, tuple[AgentEvent, ...]]:
        """Normalize one stdout line. Returns ``(native_type, events)``.

        Non-JSON and malformed lines become RAW events (never discarded).
        """
        stripped = line.strip()
        if not stripped:
            return None, ()
        if not stripped.startswith("{"):
            return None, (_raw({"line": stripped}, None),)
        try:
            obj = json.loads(stripped)
        except (ValueError, RecursionError):
            return None, (_raw({"line": stripped}, None),)
        if not isinstance(obj, Mapping):
            return None, (_raw({"line": stripped}, None),)
        kind = obj.get("type")
        native_type = kind if isinstance(kind, str) else None
        return native_type, self._normalize(obj, native_type)

    def _normalize(
        self, obj: Mapping[str, Any], native_type: str | None
    ) -> tuple[AgentEvent, ...]:
        if native_type == "system" and obj.get("subtype") == "init":
            sid = obj.get("session_id")
            if isinstance(sid, str):
                self.session_id = sid
            return (
                AgentEvent(
                    type=EventType.SESSION_STARTED,
                    native_type="system",
                    payload={
                        "session_id": self.session_id,
                        "model": obj.get("model"),
                    },
                ),
            )
        if native_type == "assistant":
            return self._assistant(obj)
        if native_type == "user":
            return self._user(obj)
        if native_type == "result":
            return self._result(obj)
        return (_raw(dict(obj), native_type),)

    def _assistant(self, obj: Mapping[str, Any]) -> tuple[AgentEvent, ...]:
        message = obj.get("message")
        if not isinstance(message, Mapping):
            return (_raw(dict(obj), "assistant"),)
        events: list[AgentEvent] = []
        stop = message.get("stop_reason")
        if isinstance(stop, str):
            self._last_stop_reason = stop
        content = message.get("content")
        if isinstance(content, list):
            for block in content:
                if not isinstance(block, Mapping):
                    continue
                block_type = block.get("type")
                if block_type == "text" and isinstance(block.get("text"), str):
                    events.append(
                        AgentEvent(
                            type=EventType.ASSISTANT_MESSAGE,
                            native_type="assistant",
                            payload={"text": block["text"]},
                        )
                    )
                elif block_type == "thinking":
                    events.append(
                        AgentEvent(
                            type=EventType.THOUGHT,
                            native_type="assistant",
                            payload={"thinking": block.get("thinking")},
                        )
                    )
                elif block_type == "tool_use":
                    tool_use_id = block.get("id")
                    tool_name = block.get("name")
                    if isinstance(tool_use_id, str) and isinstance(tool_name, str):
                        tool_input = block.get("input")
                        events.append(
                            AgentEvent(
                                type=EventType.TOOL_CALL_STARTED,
                                native_type="assistant",
                                payload={
                                    "tool_use_id": tool_use_id,
                                    "tool_name": tool_name,
                                    "tool_input": tool_input
                                    if isinstance(tool_input, Mapping)
                                    else {},
                                },
                            )
                        )
        usage = coerce_usage(message.get("usage"))
        if usage:
            events.append(
                AgentEvent(
                    type=EventType.CONTEXT_USAGE,
                    native_type="assistant",
                    payload={"usage": usage},
                )
            )
        if not events:
            return (_raw(dict(obj), "assistant"),)
        return tuple(events)

    def _user(self, obj: Mapping[str, Any]) -> tuple[AgentEvent, ...]:
        message = obj.get("message")
        if not isinstance(message, Mapping):
            return (_raw(dict(obj), "user"),)
        events: list[AgentEvent] = []
        content = message.get("content")
        if isinstance(content, list):
            for block in content:
                if (
                    isinstance(block, Mapping)
                    and block.get("type") == "tool_result"
                    and isinstance(block.get("tool_use_id"), str)
                ):
                    is_error = block.get("is_error")
                    events.append(
                        AgentEvent(
                            type=EventType.TOOL_CALL_FINISHED,
                            native_type="user",
                            payload={
                                "tool_use_id": block["tool_use_id"],
                                "is_error": is_error
                                if isinstance(is_error, bool)
                                else None,
                                "content": block.get("content"),
                            },
                        )
                    )
        if not events:
            return (_raw(dict(obj), "user"),)
        return tuple(events)

    def _result(self, obj: Mapping[str, Any]) -> tuple[AgentEvent, ...]:
        events: list[AgentEvent] = []
        denials = obj.get("permission_denials")
        if isinstance(denials, list):
            for denial in denials:
                if isinstance(denial, Mapping):
                    tool_name = denial.get("tool_name")
                    events.append(
                        AgentEvent(
                            type=EventType.PERMISSION_DENIED,
                            native_type="result",
                            payload={
                                "tool_name": tool_name
                                if isinstance(tool_name, str)
                                else None,
                                "denial": dict(denial),
                            },
                        )
                    )
        subtype = obj.get("subtype")
        subtype_str = subtype if isinstance(subtype, str) else None
        is_error = obj.get("is_error") is True
        num_turns = obj.get("num_turns")
        cost = obj.get("total_cost_usd")
        result_text = obj.get("result")
        payload: dict[str, Any] = {
            "normalized_stop": normalize_stop(
                subtype=subtype_str,
                native_stop=self._last_stop_reason,
                is_error=is_error,
            ).value,
            "stop_reason": self._last_stop_reason,
            "subtype": subtype_str,
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
        usage = coerce_usage(obj.get("usage"))
        if usage:
            payload["usage"] = usage
        events.append(
            AgentEvent(
                type=EventType.SESSION_FINISHED,
                native_type="result",
                payload=payload,
            )
        )
        return tuple(events)


def _raw(data: Mapping[str, Any], native_type: str | None) -> AgentEvent:
    return AgentEvent(
        type=EventType.RAW, native_type=native_type, payload={"data": dict(data)}
    )


class CliRunningAgent(RunningAgent):
    def __init__(self, process: RunningProcess, services: AdapterServices) -> None:
        self._process = process
        self._services = services
        self._normalizer = ClaudeStreamNormalizer()

    @property
    def native_session_id(self) -> str | None:
        return self._normalizer.session_id

    async def events(self) -> AsyncIterator[AgentEvent]:
        async for line in self._process.stdout_lines():
            stripped = line.strip()
            if not stripped:
                continue
            native_type, events = self._normalizer.feed_line(stripped)
            self._services.emit_raw(
                protocol=_PROTOCOL,
                stream="stdout",
                data=stripped,
                native_type=native_type,
            )
            for event in events:
                yield event

    async def cancel(self) -> None:
        await self._process.kill()

    async def wait(self) -> AgentExit:
        return await self._process.wait()


async def start_cli_agent(
    request: RunRequest, services: AdapterServices
) -> RunningAgent:
    plan = build_cli_plan(request)
    process = await services.host.spawn(plan)
    return CliRunningAgent(process, services)
