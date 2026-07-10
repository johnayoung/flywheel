"""AdapterRegistry and AgentRuntime plumbing tests.

The run path itself is exercised by the contract suite; this file covers
registration, lookup errors, plugin isolation, and option discovery.
"""

from __future__ import annotations

import asyncio

import pytest

import flywheel_agents.registry as registry_module
from flywheel_agents import (
    AdapterRegistry,
    AgentRuntime,
    UnknownAgentError,
    default_registry,
)
from flywheel_agents.claude_code import ClaudeCodeAdapter


def test_get_unknown_id_names_registered_ids() -> None:
    registry = AdapterRegistry()
    registry.register(ClaudeCodeAdapter())
    with pytest.raises(UnknownAgentError) as excinfo:
        registry.get("codex")
    message = str(excinfo.value)
    assert "codex" in message
    assert "claude-code" in message


def test_get_on_empty_registry_says_none() -> None:
    with pytest.raises(UnknownAgentError) as excinfo:
        AdapterRegistry().get("anything")
    assert "<none>" in str(excinfo.value)


def test_duplicate_register_raises_value_error() -> None:
    registry = AdapterRegistry()
    registry.register(ClaudeCodeAdapter())
    with pytest.raises(ValueError):
        registry.register(ClaudeCodeAdapter())


def test_default_registry_contains_claude_code() -> None:
    assert "claude-code" in default_registry().ids()


def test_runtime_adapter_returns_registered_adapter() -> None:
    adapter = AgentRuntime().adapter("claude-code")
    assert isinstance(adapter, ClaudeCodeAdapter)
    assert adapter.descriptor.id == "claude-code"


def test_discover_options_returns_honest_warning() -> None:
    options = asyncio.run(AgentRuntime().discover_options("claude-code"))
    assert options.warnings
    assert options.models == ()


class _ExplodingEntryPoint:
    """Test double for an installed plugin whose import explodes."""

    name = "exploding-plugin"

    def load(self) -> object:
        raise RuntimeError("plugin import exploded")


def test_failing_plugin_is_recorded_not_fatal(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        registry_module,
        "entry_points",
        lambda *, group: [_ExplodingEntryPoint()],
    )
    registry = AdapterRegistry()
    registry.register(ClaudeCodeAdapter())
    registry.discover_plugins()
    assert len(registry.load_failures) == 1
    failure = registry.load_failures[0]
    assert failure.name == "exploding-plugin"
    assert "plugin import exploded" in failure.error
    assert registry.ids() == ("claude-code",)
    assert registry.get("claude-code") is not None

    # default_registry() also survives the bad plugin.
    defaults = default_registry()
    assert "claude-code" in defaults.ids()
    assert len(defaults.load_failures) == 1
    assert defaults.load_failures[0].name == "exploding-plugin"
