"""Normalized and raw event models plus the sink seam.

The normalization guarantee: every native event becomes either a known
normalized :class:`AgentEvent` or an ``EventType.RAW`` event — never silently
discarded. Raw envelopes are delivered to the sink *before* the normalized
events derived from them.

This package persists nothing. Durable storage belongs to the caller via
:class:`EventSink`; flywheel bridges to its own telemetry sink.

``sequence`` is runtime-assigned: one monotonic counter per run covering raw
and normalized events together. Adapters leave the default ``0``; the runtime
re-stamps every event it forwards.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from typing import Any, Protocol


class EventType(StrEnum):
    SESSION_STARTED = "session.started"
    SESSION_FINISHED = "session.finished"

    ASSISTANT_MESSAGE = "message.assistant"
    ASSISTANT_DELTA = "message.assistant.delta"
    USER_MESSAGE = "message.user"
    THOUGHT = "thought"

    TOOL_CALL_STARTED = "tool.started"
    TOOL_CALL_UPDATED = "tool.updated"
    TOOL_CALL_FINISHED = "tool.finished"

    FILE_CHANGED = "file.changed"
    COMMAND_STARTED = "command.started"
    COMMAND_FINISHED = "command.finished"

    PLAN_UPDATED = "plan.updated"

    APPROVAL_REQUESTED = "approval.requested"
    APPROVAL_RESOLVED = "approval.resolved"
    PERMISSION_DENIED = "permission.denied"

    CONTEXT_USAGE = "context.usage"
    RATE_LIMITED = "rate.limited"
    MODE_CHANGED = "mode.changed"

    WARNING = "warning"
    ERROR = "error"
    RAW = "raw"


def utc_now() -> datetime:
    return datetime.now(UTC)


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentEvent:
    """One normalized event.

    ``payload`` keys are event-type specific and documented on the adapter
    normalizers. ``native_type`` preserves the vendor's own event name so a
    consumer can always trace an event back to its raw envelope.
    """

    type: EventType
    payload: Mapping[str, Any] = field(default_factory=dict)
    source: str = ""
    native_type: str | None = None
    sequence: int = 0
    timestamp: datetime = field(default_factory=utc_now)


@dataclass(frozen=True, slots=True, kw_only=True)
class RawAgentEvent:
    """One native envelope, retained verbatim before normalization."""

    adapter_id: str
    protocol: str
    stream: str
    data: object
    native_type: str | None = None
    sequence: int = 0
    timestamp: datetime = field(default_factory=utc_now)


class EventSink(Protocol):
    """Caller-supplied event receiver. Implementations must not raise."""

    def on_raw(self, event: RawAgentEvent) -> None: ...

    def on_event(self, event: AgentEvent) -> None: ...


class NullSink:
    """Discards everything. The default when no sink is supplied."""

    def on_raw(self, event: RawAgentEvent) -> None:
        return None

    def on_event(self, event: AgentEvent) -> None:
        return None


class MemorySink:
    """Accumulates events in memory. Intended for tests and fixtures."""

    def __init__(self) -> None:
        self.raw: list[RawAgentEvent] = []
        self.events: list[AgentEvent] = []

    def on_raw(self, event: RawAgentEvent) -> None:
        self.raw.append(event)

    def on_event(self, event: AgentEvent) -> None:
        self.events.append(event)
