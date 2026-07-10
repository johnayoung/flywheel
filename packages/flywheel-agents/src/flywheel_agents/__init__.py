"""flywheel-agents: the multi-agent execution layer.

One consistent way to run coding-agent harnesses. Adapters translate vendor
protocols into normalized events; the runtime folds each run into a
:class:`CompletedRun`. Stdlib-only at runtime; vendor SDKs are optional
extras. Design doc: ``docs/agent-harness.md``.
"""

from flywheel_agents.adapter import (
    AdapterServices,
    AgentAdapter,
    AgentModeOption,
    AgentOptions,
    DiscoveryContext,
    ModelOption,
    PluginLoadFailure,
    RawEmitter,
    ReasoningOption,
    RunningAgent,
)
from flywheel_agents.capabilities import AgentCapabilities
from flywheel_agents.config import (
    AgentConfiguration,
    AuthenticationPolicy,
    PermissionPolicy,
)
from flywheel_agents.errors import (
    AgentAuthenticationError,
    AgentHarnessError,
    AgentNotInstalledError,
    AgentProcessExitedError,
    AgentProtocolError,
    AgentStartupError,
    ApprovalTimeoutError,
    AuthenticationPolicyError,
    UnknownAgentError,
    UnsupportedCapabilityError,
)
from flywheel_agents.events import (
    AgentEvent,
    EventSink,
    EventType,
    MemorySink,
    NullSink,
    RawAgentEvent,
)
from flywheel_agents.fold import EventFolder
from flywheel_agents.hosts import (
    DockerExecHost,
    ExecutionHost,
    LocalHost,
    ProcessPlan,
    RunningProcess,
)
from flywheel_agents.models import (
    AdapterCompatibility,
    AgentDescriptor,
    AgentExit,
    AgentFault,
    AgentProbeResult,
    AssuranceLevel,
    AuthenticationKind,
    CompletedRun,
    FaultEvidence,
    PermissionDenial,
    RateLimitInfo,
    RunFailure,
    RunRequest,
    StopInfo,
    StopReason,
    ToolInteraction,
    ToolResult,
)
from flywheel_agents.registry import AdapterRegistry, default_registry
from flywheel_agents.runtime import AgentRuntime

__all__ = [
    "AdapterCompatibility",
    "AdapterRegistry",
    "AdapterServices",
    "AgentAdapter",
    "AgentAuthenticationError",
    "AgentCapabilities",
    "AgentConfiguration",
    "AgentDescriptor",
    "AgentEvent",
    "AgentExit",
    "AgentFault",
    "AgentHarnessError",
    "AgentModeOption",
    "AgentNotInstalledError",
    "AgentOptions",
    "AgentProbeResult",
    "AgentProcessExitedError",
    "AgentProtocolError",
    "AgentRuntime",
    "AgentStartupError",
    "ApprovalTimeoutError",
    "AssuranceLevel",
    "AuthenticationKind",
    "AuthenticationPolicy",
    "AuthenticationPolicyError",
    "CompletedRun",
    "DiscoveryContext",
    "DockerExecHost",
    "EventFolder",
    "EventSink",
    "EventType",
    "ExecutionHost",
    "FaultEvidence",
    "LocalHost",
    "MemorySink",
    "ModelOption",
    "NullSink",
    "PermissionDenial",
    "PermissionPolicy",
    "PluginLoadFailure",
    "ProcessPlan",
    "RateLimitInfo",
    "RawAgentEvent",
    "RawEmitter",
    "ReasoningOption",
    "RunFailure",
    "RunRequest",
    "RunningAgent",
    "RunningProcess",
    "StopInfo",
    "StopReason",
    "ToolInteraction",
    "ToolResult",
    "UnknownAgentError",
    "UnsupportedCapabilityError",
    "default_registry",
]
