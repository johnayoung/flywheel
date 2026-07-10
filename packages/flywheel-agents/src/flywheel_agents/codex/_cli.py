"""Codex CLI transport: headless ``codex exec --json``.

Works under any :class:`~flywheel_agents.hosts.ExecutionHost`. The argv shape
mirrors the claude-code CLI transport: the prompt is piped on stdin (the
positional prompt argument is ``-``, which tells ``codex exec`` to read it
from stdin), dodging ARG_MAX.

Capability honesty: ``codex exec`` has no turn-cap flag, so
``AgentConfiguration.max_turns`` is ignored by :func:`build_cli_plan`; the
wall-clock ceiling on :class:`~flywheel_agents.models.RunRequest` is the
bound.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator, Mapping
from typing import Any

from flywheel_agents.adapter import AdapterServices, RunningAgent
from flywheel_agents.config import AuthenticationPolicy, PermissionPolicy
from flywheel_agents.errors import UnsupportedCapabilityError
from flywheel_agents.events import AgentEvent, EventType
from flywheel_agents.hosts import ProcessPlan, RunningProcess
from flywheel_agents.models import AgentExit, RunRequest, StopReason

_PROTOCOL = "jsonl"
_OPENAI_API_KEY = "OPENAI_API_KEY"

_ITEM_ENVELOPES = ("item.started", "item.updated", "item.completed")


def build_cli_plan(request: RunRequest) -> ProcessPlan:
    """Build the headless ``codex exec --json`` invocation for one run.

    ``cfg.max_turns`` is deliberately ignored: ``codex exec`` exposes no
    turn-cap flag, and inventing one would break the invocation. The runtime's
    ``RunRequest.timeout_seconds`` wall clock is the enforced ceiling.
    """
    cfg = request.configuration
    argv: list[str] = (
        list(cfg.command_override) if cfg.command_override else ["codex"]
    )
    argv.extend(["exec", "--json"])
    if cfg.model_id:
        argv.extend(["--model", cfg.model_id])
    if cfg.permission_policy is PermissionPolicy.AUTO:
        # Flywheel already isolates the run via worktree/container; AUTO maps
        # to codex's bypass flag, the exact analog of claude's
        # --dangerously-skip-permissions.
        argv.append("--dangerously-bypass-approvals-and-sandbox")
    elif cfg.permission_policy is PermissionPolicy.SUPERVISED:
        argv.extend(["--sandbox", "workspace-write"])
    elif cfg.permission_policy is PermissionPolicy.READ_ONLY:
        argv.extend(["--sandbox", "read-only"])
    elif cfg.permission_policy is PermissionPolicy.PLAN:
        raise UnsupportedCapabilityError(
            "codex has no plan mode; use read-only"
        )
    extra = cfg.adapter_options.get("extra_cli_args")
    if isinstance(extra, (list, tuple)):
        argv.extend(str(item) for item in extra)
    if cfg.adapter_options.get("skip_git_repo_check") is True:
        argv.append("--skip-git-repo-check")
    # The positional prompt "-" tells codex exec to read the prompt on stdin.
    argv.append("-")
    denied = (
        (_OPENAI_API_KEY,)
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


class CodexStreamNormalizer:
    """Stateful ``codex exec --json`` JSONL normalizer.

    Stateful because codex splits signals across envelopes:
    ``thread.started`` carries the session id, item envelopes repeat one item
    id across started/updated/completed (tool pairing and message dedupe key
    on it), and the terminal ``turn.completed``/``turn.failed`` envelopes
    carry usage. ``num_turns`` counts terminal turn envelopes seen
    (``turn.completed`` and ``turn.failed``); multiple terminal envelopes
    each emit a ``session.finished`` event and the fold keeps the last.
    """

    def __init__(self) -> None:
        self.session_id: str | None = None
        self._turns = 0
        self._emitted_messages: set[str] = set()
        self._started_tools: set[str] = set()

    def feed_line(self, line: str) -> tuple[str | None, tuple[AgentEvent, ...]]:
        """Normalize one stdout line. Returns ``(native_type, events)``.

        Non-JSON, malformed, deeply nested, and non-mapping lines become RAW
        events (never discarded). Envelopes that carry no normalizable signal
        (e.g. ``turn.started``, ``item.started`` for a message item) return
        an empty event tuple; the caller raw-records every envelope anyway.
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
        if native_type == "thread.started":
            sid = obj.get("thread_id")
            if isinstance(sid, str):
                self.session_id = sid
            return (
                AgentEvent(
                    type=EventType.SESSION_STARTED,
                    native_type=native_type,
                    payload={"session_id": self.session_id},
                ),
            )
        if native_type == "turn.started":
            return ()
        if native_type == "turn.completed":
            return self._turn_completed(obj)
        if native_type == "turn.failed":
            return self._turn_failed(obj)
        if native_type == "error":
            return (
                AgentEvent(
                    type=EventType.ERROR,
                    native_type=native_type,
                    payload={"message": obj.get("message")},
                ),
            )
        if native_type in _ITEM_ENVELOPES:
            return self._item(obj, native_type)
        return (_raw(dict(obj), native_type),)

    def _item(
        self, obj: Mapping[str, Any], native_type: str
    ) -> tuple[AgentEvent, ...]:
        item = obj.get("item")
        if not isinstance(item, Mapping):
            return (_raw(dict(obj), native_type),)
        # The item's type key drifted across codex releases: accept both
        # "type" and "item_type" ("type" wins when both are present).
        item_type = item.get("type")
        if not isinstance(item_type, str):
            item_type = item.get("item_type")
        if not isinstance(item_type, str):
            return (_raw(dict(obj), native_type),)
        raw_id = item.get("id")
        item_id = raw_id if isinstance(raw_id, str) else None
        completed = native_type == "item.completed"

        if item_type == "agent_message":
            if not completed:
                return ()
            if item_id is not None:
                if item_id in self._emitted_messages:
                    return ()  # repeated completed envelope: never re-emit
                self._emitted_messages.add(item_id)
            text = item.get("text")
            if not isinstance(text, str):
                return (_raw(dict(obj), native_type),)
            return (
                AgentEvent(
                    type=EventType.ASSISTANT_MESSAGE,
                    native_type=native_type,
                    payload={"text": text},
                ),
            )
        if item_type == "reasoning":
            if not completed:
                return ()
            return (
                AgentEvent(
                    type=EventType.THOUGHT,
                    native_type=native_type,
                    payload={"thinking": item.get("text")},
                ),
            )
        if item_type == "command_execution":
            if item_id is None:
                return (_raw(dict(obj), native_type),)
            events: list[AgentEvent] = []
            if item_id not in self._started_tools:
                self._started_tools.add(item_id)
                events.append(
                    AgentEvent(
                        type=EventType.TOOL_CALL_STARTED,
                        native_type=native_type,
                        payload={
                            "tool_use_id": item_id,
                            "tool_name": "command_execution",
                            "tool_input": {"command": item.get("command")},
                        },
                    )
                )
            if completed:
                exit_code = item.get("exit_code")
                is_error = (item.get("status") == "failed") or (
                    exit_code not in (0, None)
                )
                events.append(
                    AgentEvent(
                        type=EventType.TOOL_CALL_FINISHED,
                        native_type=native_type,
                        payload={
                            "tool_use_id": item_id,
                            "is_error": is_error,
                            "content": item.get("aggregated_output"),
                        },
                    )
                )
            return tuple(events)
        if item_type == "file_change":
            if not completed:
                return ()
            if item_id is None:
                return (_raw(dict(obj), native_type),)
            raw_changes = item.get("changes")
            changes = raw_changes if isinstance(raw_changes, list) else []
            is_error = item.get("status") == "failed"
            events = [
                AgentEvent(
                    type=EventType.TOOL_CALL_STARTED,
                    native_type=native_type,
                    payload={
                        "tool_use_id": item_id,
                        "tool_name": "file_change",
                        "tool_input": {"changes": changes},
                    },
                ),
                AgentEvent(
                    type=EventType.TOOL_CALL_FINISHED,
                    native_type=native_type,
                    payload={
                        "tool_use_id": item_id,
                        "is_error": is_error,
                        "content": None,
                    },
                ),
            ]
            for change in changes:
                if isinstance(change, Mapping):
                    events.append(
                        AgentEvent(
                            type=EventType.FILE_CHANGED,
                            native_type=native_type,
                            payload={
                                "path": change.get("path"),
                                "kind": change.get("kind"),
                            },
                        )
                    )
            return tuple(events)
        if item_type == "mcp_tool_call":
            if not completed:
                return ()
            if item_id is None:
                return (_raw(dict(obj), native_type),)
            tool_name = f"mcp:{item.get('server')}.{item.get('tool')}"
            return (
                AgentEvent(
                    type=EventType.TOOL_CALL_STARTED,
                    native_type=native_type,
                    payload={
                        "tool_use_id": item_id,
                        "tool_name": tool_name,
                        "tool_input": {},
                    },
                ),
                AgentEvent(
                    type=EventType.TOOL_CALL_FINISHED,
                    native_type=native_type,
                    payload={
                        "tool_use_id": item_id,
                        "is_error": item.get("status") == "failed",
                        "content": None,
                    },
                ),
            )
        if item_type == "web_search":
            if not completed:
                return ()
            if item_id is None:
                return (_raw(dict(obj), native_type),)
            return (
                AgentEvent(
                    type=EventType.TOOL_CALL_STARTED,
                    native_type=native_type,
                    payload={
                        "tool_use_id": item_id,
                        "tool_name": "web_search",
                        "tool_input": {"query": item.get("query")},
                    },
                ),
                AgentEvent(
                    type=EventType.TOOL_CALL_FINISHED,
                    native_type=native_type,
                    payload={
                        "tool_use_id": item_id,
                        "is_error": item.get("status") == "failed",
                        "content": None,
                    },
                ),
            )
        if item_type == "todo_list":
            # Emit on updated AND completed; the fold keeps the latest.
            if native_type in ("item.updated", "item.completed"):
                return (
                    AgentEvent(
                        type=EventType.PLAN_UPDATED,
                        native_type=native_type,
                        payload={"items": item.get("items")},
                    ),
                )
            return ()
        if item_type == "error":
            return (
                AgentEvent(
                    type=EventType.ERROR,
                    native_type=native_type,
                    payload={"message": item.get("message")},
                ),
            )
        return (_raw(dict(obj), native_type),)

    def _turn_completed(self, obj: Mapping[str, Any]) -> tuple[AgentEvent, ...]:
        self._turns += 1
        payload: dict[str, Any] = {
            "normalized_stop": StopReason.COMPLETED.value,
            "stop_reason": "turn.completed",
            "subtype": None,
            "is_error": False,
            "num_turns": self._turns,
            "total_cost_usd": None,
            "result_text": None,
            "session_id": self.session_id,
        }
        usage = _coerce_usage(obj.get("usage"))
        if usage is not None:
            payload["usage"] = usage
        return (
            AgentEvent(
                type=EventType.SESSION_FINISHED,
                native_type="turn.completed",
                payload=payload,
            ),
        )

    def _turn_failed(self, obj: Mapping[str, Any]) -> tuple[AgentEvent, ...]:
        self._turns += 1
        error = obj.get("error")
        message = error.get("message") if isinstance(error, Mapping) else None
        payload: dict[str, Any] = {
            "normalized_stop": StopReason.ERROR.value,
            "stop_reason": "turn.failed",
            "subtype": None,
            "is_error": True,
            "num_turns": self._turns,
            "total_cost_usd": None,
            "result_text": None,
            "session_id": self.session_id,
        }
        usage = _coerce_usage(obj.get("usage"))
        if usage is not None:
            payload["usage"] = usage
        return (
            AgentEvent(
                type=EventType.ERROR,
                native_type="turn.failed",
                payload={"message": message},
            ),
            AgentEvent(
                type=EventType.SESSION_FINISHED,
                native_type="turn.failed",
                payload=payload,
            ),
        )


def _coerce_usage(raw: object) -> dict[str, int] | None:
    """Project codex usage onto flywheel's four canonical counters.

    Returns ``None`` when the native usage mapping is absent (the payload
    then omits its ``usage`` key). ``cached_input_tokens`` maps to
    ``cache_read_input_tokens``; codex reports no cache-creation counter, so
    that key is always 0. Ints are coerced; bools are excluded.
    """
    if not isinstance(raw, Mapping):
        return None

    def _int(key: str) -> int:
        value = raw.get(key)
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return 0
        return int(value)

    return {
        "input_tokens": _int("input_tokens"),
        "output_tokens": _int("output_tokens"),
        "cache_read_input_tokens": _int("cached_input_tokens"),
        "cache_creation_input_tokens": 0,
    }


def _raw(data: Mapping[str, Any], native_type: str | None) -> AgentEvent:
    return AgentEvent(
        type=EventType.RAW, native_type=native_type, payload={"data": dict(data)}
    )


class CodexCliRunningAgent(RunningAgent):
    def __init__(self, process: RunningProcess, services: AdapterServices) -> None:
        self._process = process
        self._services = services
        self._normalizer = CodexStreamNormalizer()

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
    return CodexCliRunningAgent(process, services)
