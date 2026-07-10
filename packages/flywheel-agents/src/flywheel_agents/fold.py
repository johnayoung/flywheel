"""Fold a normalized event stream into a :class:`CompletedRun`.

This is the single generic replacement for the two divergent folds that exist
in flywheel today (the SDK-message dispatch in ``flywheel_core.invoker`` and
the stream-json dict dispatch in ``flywheel_container._stream``). Adapters own
vendor semantics by shaping event payloads; the folder only aggregates.

Payload keys the folder reads:

- ``session.started``: ``session_id``
- ``message.assistant``: ``text``, ``usage`` (turn-cumulative token mapping)
- ``context.usage``: ``usage`` (same shape; lets adapters report usage on
  assistant messages that carry no text block, e.g. tool-use-only turns)
- ``tool.started``: ``tool_use_id``, ``tool_name``, ``tool_input``
- ``tool.finished``: ``tool_use_id``, ``is_error``, ``content``
- ``permission.denied``: ``tool_name`` (rest kept as detail)
- ``rate.limited``: ``resets_at_epoch`` (rest kept as detail)
- ``session.finished``: ``normalized_stop`` (a :class:`StopReason` value,
  adapter-computed), ``stop_reason`` (native), ``subtype``, ``is_error``,
  ``num_turns``, ``total_cost_usd``, ``usage`` (authoritative when present),
  ``result_text`` (transcript fallback), ``session_id``
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from flywheel_agents.events import AgentEvent, EventType
from flywheel_agents.models import (
    AgentExit,
    AgentFault,
    CompletedRun,
    PermissionDenial,
    RateLimitInfo,
    RunFailure,
    StopInfo,
    StopReason,
    ToolInteraction,
    ToolResult,
)


def _usage_from(payload: Mapping[str, Any], key: str = "usage") -> dict[str, int]:
    raw = payload.get(key)
    if not isinstance(raw, Mapping):
        return {}
    out: dict[str, int] = {}
    for k, v in raw.items():
        if isinstance(v, bool):
            continue
        if isinstance(k, str) and isinstance(v, (int, float)):
            out[k] = int(v)
    return out


class EventFolder:
    """Accumulates one run's events. Single-threaded; feed in arrival order."""

    def __init__(self) -> None:
        self._texts: list[str] = []
        self._session_id: str | None = None
        self._tool_order: list[str] = []
        self._tool_calls: dict[str, tuple[str, Mapping[str, Any]]] = {}
        self._tool_results: dict[str, ToolResult] = {}
        self._denials: list[PermissionDenial] = []
        self._rate_limits: list[RateLimitInfo] = []
        self._assistant_usage: dict[str, int] = {}
        self._finished: Mapping[str, Any] | None = None
        self._event_count = 0

    def feed(self, event: AgentEvent) -> None:
        self._event_count += 1
        payload = event.payload
        if event.type is EventType.SESSION_STARTED:
            sid = payload.get("session_id")
            if isinstance(sid, str):
                self._session_id = sid
        elif event.type is EventType.ASSISTANT_MESSAGE:
            text = payload.get("text")
            if isinstance(text, str):
                self._texts.append(text)
            usage = _usage_from(payload)
            if usage:
                self._assistant_usage = usage
        elif event.type is EventType.CONTEXT_USAGE:
            usage = _usage_from(payload)
            if usage:
                self._assistant_usage = usage
        elif event.type is EventType.TOOL_CALL_STARTED:
            tool_use_id = payload.get("tool_use_id")
            tool_name = payload.get("tool_name")
            if isinstance(tool_use_id, str) and isinstance(tool_name, str):
                tool_input = payload.get("tool_input")
                self._tool_order.append(tool_use_id)
                self._tool_calls[tool_use_id] = (
                    tool_name,
                    tool_input if isinstance(tool_input, Mapping) else {},
                )
        elif event.type is EventType.TOOL_CALL_FINISHED:
            tool_use_id = payload.get("tool_use_id")
            if isinstance(tool_use_id, str):
                is_error = payload.get("is_error")
                self._tool_results[tool_use_id] = ToolResult(
                    tool_use_id=tool_use_id,
                    is_error=is_error if isinstance(is_error, bool) else None,
                    content=payload.get("content"),
                )
        elif event.type is EventType.PERMISSION_DENIED:
            tool_name = payload.get("tool_name")
            self._denials.append(
                PermissionDenial(
                    tool_name=tool_name if isinstance(tool_name, str) else None,
                    detail=dict(payload),
                )
            )
        elif event.type is EventType.RATE_LIMITED:
            resets = payload.get("resets_at_epoch")
            self._rate_limits.append(
                RateLimitInfo(
                    resets_at_epoch=float(resets)
                    if isinstance(resets, (int, float)) and not isinstance(resets, bool)
                    else None,
                    detail=dict(payload),
                )
            )
        elif event.type is EventType.SESSION_FINISHED:
            self._finished = dict(payload)
            sid = payload.get("session_id")
            if isinstance(sid, str):
                self._session_id = sid

    @property
    def session_id(self) -> str | None:
        return self._session_id

    def _final_text(self) -> str:
        if self._texts:
            return "".join(self._texts)
        if self._finished is not None:
            result_text = self._finished.get("result_text")
            if isinstance(result_text, str):
                return result_text
        return ""

    def _stop(self) -> StopInfo:
        pending = any(uid not in self._tool_results for uid in self._tool_order)
        if self._finished is None:
            return StopInfo(reason=StopReason.UNKNOWN, pending_tool_use=pending)
        normalized = self._finished.get("normalized_stop")
        try:
            reason = StopReason(normalized) if isinstance(normalized, str) else None
        except ValueError:
            reason = None
        native = self._finished.get("stop_reason")
        subtype = self._finished.get("subtype")
        is_error = self._finished.get("is_error") is True
        if reason is None:
            reason = StopReason.ERROR if is_error else StopReason.UNKNOWN
        return StopInfo(
            reason=reason,
            native=native if isinstance(native, str) else None,
            native_subtype=subtype if isinstance(subtype, str) else None,
            is_error=is_error,
            pending_tool_use=pending,
            finished=True,
        )

    def completed(
        self,
        *,
        exit: AgentExit,
        failure: RunFailure | None = None,
        fault: AgentFault | None = None,
    ) -> CompletedRun:
        finished = self._finished or {}
        usage = _usage_from(finished) or self._assistant_usage
        num_turns = finished.get("num_turns")
        cost = finished.get("total_cost_usd")
        interactions = tuple(
            ToolInteraction(
                tool_use_id=uid,
                tool_name=self._tool_calls[uid][0],
                tool_input=self._tool_calls[uid][1],
                result=self._tool_results.get(uid),
            )
            for uid in self._tool_order
        )
        return CompletedRun(
            final_text=self._final_text(),
            stop=self._stop(),
            usage=usage or None,
            total_cost_usd=float(cost)
            if isinstance(cost, (int, float)) and not isinstance(cost, bool)
            else None,
            num_turns=num_turns
            if isinstance(num_turns, int) and not isinstance(num_turns, bool)
            else None,
            native_session_id=self._session_id,
            tool_interactions=interactions,
            permission_denials=tuple(self._denials),
            rate_limit_events=tuple(self._rate_limits),
            fault=fault,
            failure=failure,
            exit=exit,
            event_count=self._event_count,
        )
