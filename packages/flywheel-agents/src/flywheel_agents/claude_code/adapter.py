"""The claude-code adapter: identity, probe, transport routing, faults.

Two transports, one normalization:

- ``cli`` (default) — headless stream-json under any execution host; the
  container-safe path.
- ``sdk`` — the official ``claude-agent-sdk`` client; local-only (the SDK owns
  its subprocess) and requires the ``flywheel-agents[claude]`` extra.

Select via ``adapter_options={"transport": "sdk"}``. The declared capability
set is the CLI transport's (the default and the floor); the SDK transport adds
signal fidelity but no capability the loop cannot degrade without.
"""

from __future__ import annotations

from flywheel_agents.adapter import (
    AdapterServices,
    AgentAdapter,
    AgentOptions,
    DiscoveryContext,
    RunningAgent,
)
from flywheel_agents.capabilities import AgentCapabilities
from flywheel_agents.claude_code._cli import start_cli_agent
from flywheel_agents.claude_code._faults import classify_claude_fault
from flywheel_agents.claude_code._probe import probe_claude_code
from flywheel_agents.errors import AgentNotInstalledError, UnsupportedCapabilityError
from flywheel_agents.models import (
    AgentDescriptor,
    AgentFault,
    AgentProbeResult,
    FaultEvidence,
    RunRequest,
)

_CAPABILITIES = AgentCapabilities(
    cancellation=True,
    model_selection=True,
    structured_tool_calls=True,
    tool_result_errors=True,
    thought_events=True,
    account_login_detection=True,
    api_key_detection=True,
)

_DESCRIPTOR = AgentDescriptor(
    id="claude-code",
    display_name="Claude Code",
    adapter_version="0.1.0",
    vendor="Anthropic",
    executable_names=("claude",),
    capabilities=_CAPABILITIES,
)


class ClaudeCodeAdapter(AgentAdapter):
    @property
    def descriptor(self) -> AgentDescriptor:
        return _DESCRIPTOR

    async def probe(self) -> AgentProbeResult:
        return await probe_claude_code()

    async def discover_options(self, context: DiscoveryContext) -> AgentOptions:
        # No native discovery surface is wired yet; model_id passes through
        # verbatim (the CLI/SDK validates it), so an empty list is honest.
        return AgentOptions(
            warnings=(
                "model discovery not implemented; model_id passes through "
                "to the claude CLI/SDK unvalidated",
            )
        )

    async def start(
        self, request: RunRequest, services: AdapterServices
    ) -> RunningAgent:
        transport = request.configuration.adapter_options.get("transport", "cli")
        if transport == "cli":
            return await start_cli_agent(request, services)
        if transport == "sdk":
            try:
                from flywheel_agents.claude_code._sdk import start_sdk_agent
            except ModuleNotFoundError as exc:
                raise AgentNotInstalledError(
                    "the sdk transport requires the claude-agent-sdk extra: "
                    "install flywheel-agents[claude]"
                ) from exc
            return await start_sdk_agent(request, services)
        raise UnsupportedCapabilityError(
            f"unknown claude-code transport {transport!r} (expected cli or sdk)"
        )

    def classify_fault(self, evidence: FaultEvidence) -> AgentFault | None:
        return classify_claude_fault(evidence)
