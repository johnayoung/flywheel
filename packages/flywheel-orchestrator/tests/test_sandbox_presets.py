"""Held-out oracle for spec 00038 (capability presets, increment B of 00036).

RED until ``sandbox-capability-presets`` lands; the seam-holdout test also
depends on ``sandbox-exec-wiring`` (the ``exec_*`` params of
``build_agent_options``). Pins the balanced/hardened preset values, the
per-key list-replace override, and the end-to-end policy -> build_agent_options
composition. Do not weaken or delete assertions.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

import pytest

from flywheel_core.workflow import build_agent_options
from flywheel_orchestrator._policy import PolicyError, load_policy

HARDENED_TOOLS = ("Bash", "Edit", "Glob", "Grep", "Read", "Write")


def _write(tmp_path: Path, body: str) -> Path:
    p = tmp_path / "flywheel.toml"
    p.write_text(
        '[source]\nkind = "directory"\n' + textwrap.dedent(body),
        encoding="utf-8",
    )
    return p


def test_balanced_drops_mcp_keeps_capability(tmp_path: Path) -> None:
    sb = load_policy(_write(tmp_path, '[sandbox]\npreset = "balanced"\n')).sandbox
    assert sb.preset == "balanced"
    assert sb.capabilities.mcp_strict is True
    assert sb.capabilities.mcp_servers == ()
    assert sb.capabilities.skills == "all"  # full coding capability kept
    assert sb.capabilities.allowed_tools == ()  # no allowlist in balanced
    assert sb.permission_mode == "bypassPermissions"  # autonomy preserved
    assert sb.exec.enabled is False


def test_hardened_locks_down(tmp_path: Path) -> None:
    sb = load_policy(_write(tmp_path, '[sandbox]\npreset = "hardened"\n')).sandbox
    assert sb.preset == "hardened"
    assert sb.capabilities.allowed_tools == HARDENED_TOOLS
    assert sb.capabilities.mcp_strict is True
    assert sb.capabilities.setting_sources == ("project",)
    assert sb.exec.enabled is True
    assert sb.permission_mode == "bypassPermissions"  # harden via allowlist
    # deferred aspects stay at fast — no false promises until C/D/G.
    assert sb.network.policy == "allow"
    assert sb.env.inherit_home is True
    assert sb.limits.max_cost_usd == 0.0


def test_override_replaces_preset_allowlist(tmp_path: Path) -> None:
    sb = load_policy(
        _write(
            tmp_path,
            """
            [sandbox]
            preset = "hardened"

            [sandbox.capabilities]
            allowed_tools = ["Read"]
            """,
        )
    ).sandbox
    assert sb.capabilities.allowed_tools == ("Read",)  # REPLACES the six
    assert sb.capabilities.mcp_strict is True  # untouched key keeps hardened


def test_unknown_preset_still_fails(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="preset"):
        load_policy(_write(tmp_path, '[sandbox]\npreset = "ludicrous"\n'))


def test_hardened_composes_into_locked_down_options(tmp_path: Path) -> None:
    # End-to-end seam: resolved hardened policy -> build_agent_options -> options.
    sb = load_policy(_write(tmp_path, '[sandbox]\npreset = "hardened"\n')).sandbox
    opts = build_agent_options(
        Path("/tmp/s"),
        model=None,
        max_turns=1,
        permission_mode=sb.permission_mode,
        skills=sb.capabilities.skills,
        allowed_tools=sb.capabilities.allowed_tools,
        denied_tools=sb.capabilities.denied_tools,
        setting_sources=sb.capabilities.setting_sources,
        mcp_servers=sb.capabilities.mcp_servers,
        mcp_strict=sb.capabilities.mcp_strict,
        exec_enabled=sb.exec.enabled,
        exec_auto_allow=sb.exec.auto_allow,
    )
    assert tuple(opts.allowed_tools) == HARDENED_TOOLS
    assert opts.strict_mcp_config is True
    assert list(opts.setting_sources) == ["project"]
    assert opts.sandbox is not None and opts.sandbox["enabled"] is True
