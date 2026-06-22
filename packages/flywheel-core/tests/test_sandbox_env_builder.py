"""Held-out oracle for spec 00040 SC-1 (env injection builder, increment C).

RED until ``sandbox-env-builder`` lands. ``build_agent_options`` gains an
``agent_env`` param mapped onto ``ClaudeAgentOptions.env`` (additive — the SDK
merges it over the inherited environment). A ``None``/empty value leaves
``env`` at the SDK default ``{}`` so the no-env (``fast``) construction is
byte-identical. Do not weaken or delete assertions.
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core.workflow import build_agent_options


def test_no_agent_env_leaves_options_env_default() -> None:
    opts = build_agent_options(Path("/tmp/s"), model=None, max_turns=1)
    assert opts.env == {}  # fast: nothing injected, byte-identical to today


def test_none_agent_env_is_unset() -> None:
    opts = build_agent_options(
        Path("/tmp/s"), model=None, max_turns=1, agent_env=None
    )
    assert opts.env == {}


def test_agent_env_is_injected() -> None:
    opts = build_agent_options(
        Path("/tmp/s"),
        model=None,
        max_turns=1,
        agent_env={"FOO": "bar", "TOKEN": "secret"},
    )
    assert opts.env == {"FOO": "bar", "TOKEN": "secret"}
