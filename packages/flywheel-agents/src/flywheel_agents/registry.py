"""Adapter registry: first-party registration plus entry-point plugins.

Third-party packages register adapters via the ``flywheel_agents.adapters``
entry-point group. A failing plugin is recorded on ``load_failures`` and never
prevents the registry from loading.
"""

from __future__ import annotations

from importlib.metadata import entry_points

from flywheel_agents.adapter import AgentAdapter, PluginLoadFailure
from flywheel_agents.errors import UnknownAgentError

ENTRY_POINT_GROUP = "flywheel_agents.adapters"


class AdapterRegistry:
    def __init__(self) -> None:
        self._adapters: dict[str, AgentAdapter] = {}
        self.load_failures: list[PluginLoadFailure] = []

    def register(self, adapter: AgentAdapter) -> None:
        agent_id = adapter.descriptor.id
        if agent_id in self._adapters:
            raise ValueError(f"adapter already registered for {agent_id!r}")
        self._adapters[agent_id] = adapter

    def get(self, agent_id: str) -> AgentAdapter:
        try:
            return self._adapters[agent_id]
        except KeyError:
            known = ", ".join(sorted(self._adapters)) or "<none>"
            raise UnknownAgentError(
                f"no adapter registered for {agent_id!r} (registered: {known})"
            ) from None

    def ids(self) -> tuple[str, ...]:
        return tuple(sorted(self._adapters))

    def discover_plugins(self) -> None:
        for entry_point in entry_points(group=ENTRY_POINT_GROUP):
            try:
                factory = entry_point.load()
                adapter = factory()
                if not isinstance(adapter, AgentAdapter):
                    raise TypeError(
                        f"entry point {entry_point.name!r} produced "
                        f"{type(adapter).__name__}, not an AgentAdapter"
                    )
                self.register(adapter)
            except Exception as exc:  # a bad plugin must not break the runtime
                self.load_failures.append(
                    PluginLoadFailure(name=entry_point.name, error=str(exc))
                )


def default_registry() -> AdapterRegistry:
    """First-party adapters plus any installed plugins."""
    from flywheel_agents.claude_code import ClaudeCodeAdapter
    from flywheel_agents.codex import CodexAdapter

    registry = AdapterRegistry()
    registry.register(ClaudeCodeAdapter())
    registry.register(CodexAdapter())
    registry.discover_plugins()
    return registry
