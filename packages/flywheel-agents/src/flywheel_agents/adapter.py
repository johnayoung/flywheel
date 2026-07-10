"""The adapter contract: lifecycle and capability translation per agent.

Adapters translate; the runtime orchestrates. An adapter may internally use an
execution host + transport (CLI paths) or an official SDK that owns its own
subprocess (declared local-only). The runtime sees only :class:`RunningAgent`.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from collections.abc import AsyncIterator
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from flywheel_agents.events import AgentEvent
from flywheel_agents.hosts import ExecutionHost
from flywheel_agents.models import (
    AgentDescriptor,
    AgentExit,
    AgentFault,
    AgentProbeResult,
    FaultEvidence,
    RunRequest,
)


@dataclass(frozen=True, slots=True, kw_only=True)
class DiscoveryContext:
    working_directory: Path | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class ReasoningOption:
    id: str
    display_name: str


@dataclass(frozen=True, slots=True, kw_only=True)
class ModelOption:
    id: str
    display_name: str
    reasoning_options: tuple[ReasoningOption, ...] = ()


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentModeOption:
    id: str
    display_name: str
    description: str | None = None
    is_default: bool = False


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentOptions:
    """Discovered options. Static fallback lists must say so in ``warnings``."""

    models: tuple[ModelOption, ...] = ()
    modes: tuple[AgentModeOption, ...] = ()
    warnings: tuple[str, ...] = ()


class RawEmitter(Protocol):
    """Runtime-provided raw-event recorder.

    Adapters call it once per native envelope, *before* yielding the
    normalized events derived from it, so raw and normalized share one
    monotonic sequence in arrival order.
    """

    def __call__(
        self,
        *,
        protocol: str,
        stream: str,
        data: object,
        native_type: str | None = None,
    ) -> None: ...


@dataclass(frozen=True, slots=True, kw_only=True)
class AdapterServices:
    """What the runtime lends an adapter for one run."""

    host: ExecutionHost
    emit_raw: RawEmitter


class RunningAgent(ABC):
    """A live agent run. ``events()`` is single-consumer."""

    @property
    @abstractmethod
    def native_session_id(self) -> str | None: ...

    @abstractmethod
    def events(self) -> AsyncIterator[AgentEvent]: ...

    @abstractmethod
    async def cancel(self) -> None: ...

    @abstractmethod
    async def wait(self) -> AgentExit: ...


class AgentAdapter(ABC):
    @property
    @abstractmethod
    def descriptor(self) -> AgentDescriptor: ...

    @abstractmethod
    async def probe(self) -> AgentProbeResult: ...

    @abstractmethod
    async def discover_options(self, context: DiscoveryContext) -> AgentOptions: ...

    @abstractmethod
    async def start(
        self, request: RunRequest, services: AdapterServices
    ) -> RunningAgent: ...

    def classify_fault(self, evidence: FaultEvidence) -> AgentFault | None:
        """Recognize vendor-specific fault strings. Default: no opinion."""
        return None


@dataclass(frozen=True, slots=True, kw_only=True)
class PluginLoadFailure:
    """A third-party adapter that failed to load. Never fatal to the runtime."""

    name: str
    error: str


__all__ = [
    "AdapterServices",
    "AgentAdapter",
    "AgentModeOption",
    "AgentOptions",
    "DiscoveryContext",
    "ModelOption",
    "PluginLoadFailure",
    "RawEmitter",
    "ReasoningOption",
    "RunningAgent",
]
