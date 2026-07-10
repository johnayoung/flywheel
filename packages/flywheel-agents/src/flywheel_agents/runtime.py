"""The runtime: orchestrates one run over any adapter.

``run()`` is the supported product contract — start the agent, stamp and
fan out every event (raw and normalized share one monotonic sequence), fold
to a :class:`CompletedRun`, classify vendor faults, and always terminate the
process tree. Cancellation of ``run()`` cancels the agent.

Hang policing is deliberately not here: flywheel's harness owns watchdogs and
resets them on every event the sink observes. ``RunRequest.timeout_seconds``
is a hard wall-clock ceiling, the analog of the container backend's
``exec_timeout``.
"""

from __future__ import annotations

import asyncio
from dataclasses import replace
from itertools import count

from flywheel_agents.adapter import (
    AdapterServices,
    AgentAdapter,
    AgentOptions,
    DiscoveryContext,
)
from flywheel_agents.events import (
    AgentEvent,
    EventSink,
    NullSink,
    RawAgentEvent,
)
from flywheel_agents.fold import EventFolder
from flywheel_agents.hosts import ExecutionHost, LocalHost
from flywheel_agents.models import (
    AgentProbeResult,
    CompletedRun,
    FaultEvidence,
    RunFailure,
    RunRequest,
)
from flywheel_agents.registry import AdapterRegistry, default_registry


class _RunRecorder:
    """Stamps one shared monotonic sequence across raw and normalized events."""

    def __init__(self, sink: EventSink, adapter_id: str) -> None:
        self._sink = sink
        self._adapter_id = adapter_id
        self._sequence = count(1)

    def emit_raw(
        self,
        *,
        protocol: str,
        stream: str,
        data: object,
        native_type: str | None = None,
    ) -> None:
        self._sink.on_raw(
            RawAgentEvent(
                adapter_id=self._adapter_id,
                protocol=protocol,
                stream=stream,
                data=data,
                native_type=native_type,
                sequence=next(self._sequence),
            )
        )

    def stamp(self, event: AgentEvent) -> AgentEvent:
        stamped = replace(
            event, sequence=next(self._sequence), source=self._adapter_id
        )
        self._sink.on_event(stamped)
        return stamped


class AgentRuntime:
    def __init__(
        self,
        *,
        registry: AdapterRegistry | None = None,
        host: ExecutionHost | None = None,
    ) -> None:
        self._registry = registry if registry is not None else default_registry()
        self._host: ExecutionHost = host if host is not None else LocalHost()

    @property
    def registry(self) -> AdapterRegistry:
        return self._registry

    def adapter(self, agent_id: str) -> AgentAdapter:
        return self._registry.get(agent_id)

    async def probe(self, agent_id: str) -> AgentProbeResult:
        return await self._registry.get(agent_id).probe()

    async def discover_options(
        self, agent_id: str, context: DiscoveryContext | None = None
    ) -> AgentOptions:
        return await self._registry.get(agent_id).discover_options(
            context if context is not None else DiscoveryContext()
        )

    async def run(
        self,
        request: RunRequest,
        *,
        sink: EventSink | None = None,
        host: ExecutionHost | None = None,
    ) -> CompletedRun:
        adapter = self._registry.get(request.configuration.agent_id)
        recorder = _RunRecorder(
            sink if sink is not None else NullSink(), adapter.descriptor.id
        )
        services = AdapterServices(
            host=host if host is not None else self._host,
            emit_raw=recorder.emit_raw,
        )
        agent = await adapter.start(request, services)
        folder = EventFolder()
        failure: RunFailure | None = None
        timed_out = False

        async def _drain() -> None:
            async for event in agent.events():
                folder.feed(recorder.stamp(event))

        try:
            if request.timeout_seconds is not None:
                async with asyncio.timeout(request.timeout_seconds):
                    await _drain()
            else:
                await _drain()
        except TimeoutError:
            timed_out = True
            await agent.cancel()
        except asyncio.CancelledError:
            await agent.cancel()
            await agent.wait()
            raise
        except Exception as exc:
            # Transport/SDK failures fold into a structured RunFailure —
            # raw evidence, no classification (mirrors InvocationFailure).
            failure = RunFailure(error_type=type(exc).__name__, message=str(exc))
            await agent.cancel()

        exit_info = await agent.wait()
        if timed_out:
            failure = RunFailure(
                error_type="timeout",
                message=f"run exceeded {request.timeout_seconds}s wall clock",
                exit_code=exit_info.returncode,
                stderr=exit_info.stderr_tail,
            )
        elif failure is None and exit_info.returncode not in (0, None):
            failure = RunFailure(
                error_type="agent_exit",
                message=f"agent exited {exit_info.returncode}",
                exit_code=exit_info.returncode,
                stderr=exit_info.stderr_tail,
            )

        interim = folder.completed(exit=exit_info, failure=failure)
        fault = adapter.classify_fault(
            FaultEvidence(
                final_text=interim.final_text,
                stderr=exit_info.stderr_tail,
                native_stop=interim.stop.native,
                native_subtype=interim.stop.native_subtype,
                is_error=interim.stop.is_error,
            )
        )
        if fault is None:
            return interim
        return replace(interim, fault=fault)
