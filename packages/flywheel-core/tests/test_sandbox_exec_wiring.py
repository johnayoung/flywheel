"""Held-out oracle for spec 00038 SC-3 (exec wiring, increment B of 00036).

RED until ``sandbox-exec-wiring`` lands. ``build_agent_options`` gains
``exec_enabled`` / ``exec_auto_allow`` mapping to ``ClaudeAgentOptions.sandbox``
(the SDK ``SandboxSettings`` bash sandbox), omitted when disabled so the ``fast``
construction stays byte-identical. Do not weaken or delete assertions.
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core.workflow import build_agent_options


def test_exec_disabled_omits_sandbox() -> None:
    # fast default: no bash sandbox configured (byte-identical to today).
    opts = build_agent_options(Path("/tmp/s"), model=None, max_turns=1)
    assert opts.sandbox is None


def test_exec_enabled_sets_sandbox_settings() -> None:
    opts = build_agent_options(
        Path("/tmp/s"),
        model=None,
        max_turns=1,
        exec_enabled=True,
        exec_auto_allow=True,
    )
    assert opts.sandbox is not None
    assert opts.sandbox["enabled"] is True
    assert opts.sandbox["autoAllowBashIfSandboxed"] is True


def test_exec_auto_allow_false_is_carried() -> None:
    opts = build_agent_options(
        Path("/tmp/s"),
        model=None,
        max_turns=1,
        exec_enabled=True,
        exec_auto_allow=False,
    )
    assert opts.sandbox is not None
    assert opts.sandbox["enabled"] is True
    assert opts.sandbox["autoAllowBashIfSandboxed"] is False
