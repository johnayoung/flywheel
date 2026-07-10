"""Bridge from the ``flywheel-agents`` runtime to the harness invoke seam.

The multi-agent counterpart of :func:`flywheel_core.workflow._make_claude_code_invoke`:
:func:`make_agents_invoke` builds an :data:`~flywheel_core.harness.InvokeFunc`
backed by :class:`flywheel_agents.AgentRuntime`, and
:func:`completed_run_to_iteration_result` is the pure fold from a
:class:`flywheel_agents.CompletedRun` onto :class:`flywheel_core.IterationResult`
(field mapping specified in ``docs/agent-harness.md`` section 15.1).

``flywheel-agents`` is an optional extra, mirroring the ``_sdk`` boundary:
this module imports without it; only calling :func:`make_agents_invoke`
requires it. Signature takes plain primitives so orchestrating packages never
import agents types.

v1 parity notes (documented limitations, matching the container invoker):
- ``InvocationRequest.transcript_graders`` are not enforced mid-run; they
  still run at grade time.
- ``rate_limit_events`` is left empty (the signals field is typed with SDK
  event objects); session-limit refusals still surface via the transcript
  regexes in :mod:`flywheel_core.faults` and via ``CompletedRun.fault``.
- Every normalized event is forwarded to ``InvocationRequest.on_message`` —
  the hang watchdog resets on each one and the telemetry sink records it.
  Raw envelopes are not double-recorded; enable a dedicated sink for those.
"""

from __future__ import annotations

from collections.abc import Callable, Mapping
from pathlib import Path
from typing import TYPE_CHECKING, Any, cast

from flywheel_core.envelope import parse_envelope
from flywheel_core.harness import InvocationRequest, InvokeFunc
from flywheel_core.invoker import (
    InvocationFailure,
    InvocationSignals,
    IterationResult,
    ToolInteraction,
    ToolResultObservation,
)

if TYPE_CHECKING:
    from flywheel_agents import AgentRuntime, CompletedRun, EventSink, ExecutionHost


class MissingAgentsRuntimeError(RuntimeError):
    """Raised when the flywheel-agents extra is required but not installed."""


def _require_agents() -> Any:
    try:
        import flywheel_agents
    except ModuleNotFoundError as exc:
        raise MissingAgentsRuntimeError(
            "the multi-agent path requires the flywheel-agents package: "
            "install flywheel-core[agents]"
        ) from exc
    return flywheel_agents


def _coerce_content(content: object) -> str | list[dict[str, Any]] | None:
    if content is None or isinstance(content, str):
        return content
    if isinstance(content, list):
        return [dict(item) for item in content if isinstance(item, Mapping)]
    return str(content)


def completed_run_to_iteration_result(completed: CompletedRun) -> IterationResult:
    """Pure fold: CompletedRun -> IterationResult (section 15.1 mapping)."""
    results: dict[str, ToolResultObservation] = {}
    interactions: list[ToolInteraction] = []
    for interaction in completed.tool_interactions:
        observation = None
        if interaction.result is not None:
            observation = ToolResultObservation(
                tool_use_id=interaction.result.tool_use_id,
                is_error=interaction.result.is_error,
                content=_coerce_content(interaction.result.content),
            )
            results[observation.tool_use_id] = observation
        interactions.append(
            ToolInteraction(
                tool_use_id=interaction.tool_use_id,
                tool_name=interaction.tool_name,
                tool_input=dict(interaction.tool_input),
                result=observation,
            )
        )
    finished = completed.stop.finished
    signals = InvocationSignals(
        stop_reason=completed.stop.native,
        num_turns=completed.num_turns,
        total_cost_usd=completed.total_cost_usd,
        result_is_error=completed.stop.is_error if finished else None,
        result_subtype=completed.stop.native_subtype,
        api_error_status=None,
        session_id=completed.native_session_id,
        permission_denials=tuple(completed.permission_denials),
        tool_interactions=tuple(interactions),
        tool_result_blocks=tuple(results.values()),
        pending_tool_use_at_stop=completed.stop.pending_tool_use,
    )
    failure = (
        InvocationFailure(
            error_type=completed.failure.error_type,
            message=completed.failure.message,
            exit_code=completed.failure.exit_code,
            stderr=completed.failure.stderr,
        )
        if completed.failure is not None
        else None
    )
    return IterationResult(
        transcript=completed.final_text,
        messages=(),
        envelope=parse_envelope(completed.final_text),
        signals=signals,
        failure=failure,
        usage=completed.usage,
    )


class _OnMessageSink:
    """Forwards normalized events to the harness per-message observer.

    Each forwarded event resets the harness hang watchdog and lands in the
    run telemetry (serialized by ``_serialize_sdk_message``'s total
    projection), giving CLI-driven runs the liveness the SDK path has.
    """

    def __init__(self, on_message: Callable[[Any], None] | None) -> None:
        self._on_message = on_message

    def on_raw(self, event: object) -> None:
        return None

    def on_event(self, event: object) -> None:
        if self._on_message is not None:
            self._on_message(event)


def make_agents_invoke(
    *,
    agent_id: str,
    working_directory: Path,
    model: str | None = None,
    permission_policy: str = "auto",
    authentication_policy: str = "account-preferred",
    max_turns: int | None = None,
    environment: Mapping[str, str] | None = None,
    adapter_options: Mapping[str, object] | None = None,
    command_override: tuple[str, ...] | None = None,
    timeout_seconds: float | None = None,
    host: ExecutionHost | None = None,
    runtime: AgentRuntime | None = None,
) -> InvokeFunc:
    """Build an InvokeFunc backed by the flywheel-agents runtime.

    Plain-primitive signature by design (the ``build_agent_options``
    convention): callers select the agent and policy with strings; agents
    types never leak upward. Raises :class:`MissingAgentsRuntimeError` when
    flywheel-agents is not installed.
    """
    agents = _require_agents()
    runtime_obj = runtime if runtime is not None else agents.AgentRuntime()
    configuration = agents.AgentConfiguration(
        agent_id=agent_id,
        model_id=model,
        permission_policy=agents.PermissionPolicy(permission_policy),
        authentication_policy=agents.AuthenticationPolicy(authentication_policy),
        max_turns=max_turns,
        command_override=command_override,
        environment=dict(environment) if environment else {},
        adapter_options=dict(adapter_options) if adapter_options else {},
    )

    async def _invoke(request: InvocationRequest) -> IterationResult:
        run_request = agents.RunRequest(
            prompt=request.prompt,
            working_directory=working_directory,
            configuration=configuration,
            timeout_seconds=timeout_seconds,
        )
        sink = cast("EventSink", _OnMessageSink(request.on_message))
        completed = await runtime_obj.run(run_request, sink=sink, host=host)
        return completed_run_to_iteration_result(completed)

    return _invoke


__all__ = [
    "MissingAgentsRuntimeError",
    "completed_run_to_iteration_result",
    "make_agents_invoke",
]
