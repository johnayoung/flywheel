"""The codex adapter: identity, probe, transport routing, faults.

One transport: ``cli`` — headless ``codex exec --json`` under any execution
host. There is no SDK transport for codex; requesting any other transport
fails explicitly rather than approximating.

Capability notes: the normalizer emits PLAN_UPDATED events from ``todo_list``
items regardless of declaration — :class:`AgentCapabilities` has no
plan-event flag to declare. ``max_turns`` is ignored by the CLI plan (codex
exec has no turn-cap flag); the run's wall-clock ceiling is the bound.
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
from flywheel_agents.codex._cli import start_cli_agent
from flywheel_agents.codex._probe import probe_codex
from flywheel_agents.errors import UnsupportedCapabilityError
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
    id="codex",
    display_name="Codex",
    adapter_version="0.1.0",
    vendor="OpenAI",
    executable_names=("codex",),
    capabilities=_CAPABILITIES,
)


class CodexAdapter(AgentAdapter):
    @property
    def descriptor(self) -> AgentDescriptor:
        return _DESCRIPTOR

    async def probe(self) -> AgentProbeResult:
        return await probe_codex()

    async def discover_options(self, context: DiscoveryContext) -> AgentOptions:
        # No native discovery surface is wired yet; model_id passes through
        # verbatim (the codex CLI validates it), so an empty list is honest.
        return AgentOptions(
            warnings=(
                "model discovery not implemented; model_id passes through "
                "to the codex CLI unvalidated",
            )
        )

    async def start(
        self, request: RunRequest, services: AdapterServices
    ) -> RunningAgent:
        transport = request.configuration.adapter_options.get("transport", "cli")
        if transport in ("cli", None):
            return await start_cli_agent(request, services)
        raise UnsupportedCapabilityError(
            f"unknown codex transport {transport!r} (codex supports only cli)"
        )

    def classify_fault(self, evidence: FaultEvidence) -> AgentFault | None:
        """No vetted codex fault strings yet — deliberately returns None.

        Recognizing vendor refusal text requires observed evidence; guessing
        strings would misclassify runs. Add patterns here only once real
        codex fault transcripts are in hand.
        """
        return None
