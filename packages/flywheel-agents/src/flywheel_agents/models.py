"""Domain models: identity, probe results, run requests, and the fold output.

:class:`CompletedRun` is the supported product contract — one agent run
drained to completion and folded into structured signals. Its field set maps
one-to-one onto ``flywheel_core.invoker.InvocationSignals`` (see
``docs/agent-harness.md`` section 15.1) without importing anything from
flywheel: the bridge lives above this package.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path
from typing import Any

from flywheel_agents.capabilities import AgentCapabilities
from flywheel_agents.config import AgentConfiguration


@dataclass(frozen=True, slots=True, kw_only=True)
class AdapterCompatibility:
    minimum_version: str | None = None
    maximum_tested_version: str | None = None
    pinned_package_version: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentDescriptor:
    """Stable identity of one adapter. ``id`` is an open string, never an enum."""

    id: str
    display_name: str
    adapter_version: str
    vendor: str | None = None
    executable_names: tuple[str, ...] = ()
    capabilities: AgentCapabilities = field(default_factory=AgentCapabilities)
    compatibility: AdapterCompatibility = field(default_factory=AdapterCompatibility)


class AuthenticationKind(StrEnum):
    ACCOUNT_SESSION = "account-session"
    API_KEY = "api-key"
    MIXED = "mixed"
    INSTALLATION_ONLY = "installation-only"
    UNAUTHENTICATED = "unauthenticated"
    UNKNOWN = "unknown"


class AssuranceLevel(StrEnum):
    VERIFIED = "verified"
    STRONG_INDICATION = "strong-indication"
    BEST_EFFORT = "best-effort"
    UNKNOWN = "unknown"


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentProbeResult:
    """Installation and authentication evidence for one agent on this host.

    Never claims more than it can verify: ``authentication_assurance``
    qualifies ``authentication_kind``, and evidence strings must not contain
    secret values (names and paths only).
    """

    installed: bool
    executable_path: Path | None = None
    version: str | None = None
    authentication_kind: AuthenticationKind = AuthenticationKind.UNKNOWN
    authentication_assurance: AssuranceLevel = AssuranceLevel.UNKNOWN
    authentication_evidence: tuple[str, ...] = ()
    config_paths: tuple[Path, ...] = ()
    warnings: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True, kw_only=True)
class RunRequest:
    """One run: a prompt drained to completion in a working directory."""

    prompt: str
    working_directory: Path
    configuration: AgentConfiguration
    timeout_seconds: float | None = None


class StopReason(StrEnum):
    COMPLETED = "completed"
    MAX_TOKENS = "max-tokens"
    MAX_TURNS = "max-turns"
    CANCELLED = "cancelled"
    ERROR = "error"
    UNKNOWN = "unknown"


@dataclass(frozen=True, slots=True, kw_only=True)
class StopInfo:
    """Why the run stopped: normalized reason plus the vendor's own string.

    ``finished`` is True only when the agent emitted its terminal envelope
    (a stream-json ``result`` / SDK ``ResultMessage``); a crash or kill
    before the terminal envelope leaves it False.
    """

    reason: StopReason = StopReason.UNKNOWN
    native: str | None = None
    native_subtype: str | None = None
    is_error: bool = False
    pending_tool_use: bool = False
    finished: bool = False


@dataclass(frozen=True, slots=True, kw_only=True)
class ToolResult:
    tool_use_id: str
    is_error: bool | None = None
    content: object | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class ToolInteraction:
    """A tool call paired with its result (when one was observed)."""

    tool_use_id: str
    tool_name: str
    tool_input: Mapping[str, Any] = field(default_factory=dict)
    result: ToolResult | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class PermissionDenial:
    tool_name: str | None = None
    detail: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True, slots=True, kw_only=True)
class RateLimitInfo:
    resets_at_epoch: float | None = None
    detail: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentFault:
    """A vendor-specific fault recognized by the adapter's classifier.

    ``kind`` is a small open vocabulary: ``session_limit``, ``auth``,
    ``quota``, ``infra``.
    """

    kind: str
    message: str
    resets_at_epoch: float | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class FaultEvidence:
    """What a fault classifier may inspect. No live process access."""

    final_text: str
    stderr: str | None = None
    native_stop: str | None = None
    native_subtype: str | None = None
    is_error: bool = False


@dataclass(frozen=True, slots=True, kw_only=True)
class RunFailure:
    """Transport or process failure — distinct from agent output.

    Mirrors ``flywheel_core.invoker.InvocationFailure``: raw evidence, no
    classification. The caller decides what a failure means.
    """

    error_type: str
    message: str
    exit_code: int | None = None
    stderr: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentExit:
    returncode: int | None = None
    stderr_tail: str | None = None
    duration_seconds: float | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class CompletedRun:
    """One run folded to completion. The supported product contract."""

    final_text: str
    stop: StopInfo
    usage: Mapping[str, int] | None = None
    total_cost_usd: float | None = None
    num_turns: int | None = None
    native_session_id: str | None = None
    tool_interactions: tuple[ToolInteraction, ...] = ()
    permission_denials: tuple[PermissionDenial, ...] = ()
    rate_limit_events: tuple[RateLimitInfo, ...] = ()
    fault: AgentFault | None = None
    failure: RunFailure | None = None
    exit: AgentExit = field(default_factory=AgentExit)
    event_count: int = 0
