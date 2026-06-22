"""Held-out oracle for spec 00037 (sandbox config foundation, increment A of 00036).

Authored BEFORE implementation: these tests define the contract the
``sandbox-policy-surface`` task must satisfy, and are RED until ``SandboxPolicy``
and the ``[sandbox.*]`` parsing land in ``_policy.py``. Do not weaken or delete
assertions to make a change pass — the task brief fences this file as
non-editable grading surface.

Pinned contract (what increment A delivers and B-G build on):

    WorkPolicy.sandbox -> SandboxPolicy
    SandboxPolicy attrs: preset, backend, permission_mode, exec, capabilities,
                         network, env, limits, retention
    The ``fast`` preset == today's hardcoded values (00037 baseline table).
    Only ``fast`` is defined; any other preset name fails fast.
"""

from __future__ import annotations

import textwrap
from pathlib import Path

import pytest

from flywheel_orchestrator._policy import PolicyError, SandboxPolicy, load_policy


def _write(tmp_path: Path, body: str) -> Path:
    """Write a minimal directory-kind policy with ``body`` appended."""
    p = tmp_path / "flywheel.toml"
    p.write_text(
        '[source]\nkind = "directory"\n' + textwrap.dedent(body),
        encoding="utf-8",
    )
    return p


def test_absent_sandbox_section_resolves_to_fast(tmp_path: Path) -> None:
    sb = load_policy(_write(tmp_path, "")).sandbox
    assert isinstance(sb, SandboxPolicy)
    assert sb.preset == "fast"
    assert sb.backend == "worktree"
    assert sb.permission_mode == "bypassPermissions"
    assert sb.capabilities.skills == "all"
    assert sb.capabilities.allowed_tools == ()
    # omit-on-unset: today set no setting_sources, so the SDK derives them from
    # skills="all". The fast mapping must preserve that exactly.
    assert sb.capabilities.setting_sources is None
    assert sb.network.policy == "allow"
    assert sb.limits.max_turns == 500
    assert sb.limits.max_retries == 1
    assert sb.retention.on_done == "destroy"
    assert sb.retention.on_failure == "park"
    assert sb.exec.enabled is False


def test_setup_only_keeps_flat_field_and_resolves_to_fast(tmp_path: Path) -> None:
    # The existing flat [sandbox] setup key and WorkPolicy.sandbox_setup stay
    # untouched; the new nested SandboxPolicy lives alongside it.
    policy = load_policy(_write(tmp_path, '[sandbox]\nsetup = "uv sync"\n'))
    assert policy.sandbox_setup == "uv sync"
    assert policy.sandbox.preset == "fast"


def test_every_subtable_parses(tmp_path: Path) -> None:
    sb = load_policy(
        _write(
            tmp_path,
            """
            [sandbox]
            backend = "worktree"
            permission_mode = "default"

            [sandbox.exec]
            enabled = true

            [sandbox.capabilities]
            skills = "none"
            allowed_tools = ["Read", "Write"]

            [sandbox.capabilities.mcp]
            servers = ["proto"]
            strict = true

            [sandbox.network]
            policy = "deny"
            allow_hosts = ["api.github.com"]

            [sandbox.env]
            inherit_home = false

            [sandbox.limits]
            max_turns = 42
            max_cost_usd = 1.5

            [sandbox.retention]
            on_done = "preserve"
            """,
        )
    ).sandbox
    assert sb.permission_mode == "default"
    assert sb.exec.enabled is True
    assert sb.capabilities.skills == "none"
    assert sb.capabilities.allowed_tools == ("Read", "Write")
    assert sb.capabilities.mcp_servers == ("proto",)
    assert sb.capabilities.mcp_strict is True
    assert sb.network.policy == "deny"
    assert sb.network.allow_hosts == ("api.github.com",)
    assert sb.env.inherit_home is False
    assert sb.limits.max_turns == 42
    assert sb.limits.max_cost_usd == 1.5
    assert sb.retention.on_done == "preserve"


def test_per_key_override_keeps_other_fast_values(tmp_path: Path) -> None:
    sb = load_policy(_write(tmp_path, "[sandbox.limits]\nmax_turns = 99\n")).sandbox
    assert sb.limits.max_turns == 99  # overridden
    assert sb.limits.max_retries == 1  # untouched key keeps the fast value
    assert sb.capabilities.skills == "all"  # untouched table keeps fast


def test_unknown_preset_fails_fast(tmp_path: Path) -> None:
    with pytest.raises(PolicyError, match="preset"):
        load_policy(_write(tmp_path, '[sandbox]\npreset = "turbo"\n'))


@pytest.mark.parametrize(
    "body, needle",
    [
        ('[sandbox.limits]\nmax_turns = "lots"\n', "sandbox.limits.max_turns"),
        ('[sandbox.network]\npolicy = "sideways"\n', "sandbox.network.policy"),
        (
            '[sandbox.capabilities]\nallowed_tools = "Read"\n',
            "sandbox.capabilities.allowed_tools",
        ),
        ('[sandbox.retention]\non_done = "vaporize"\n', "sandbox.retention.on_done"),
    ],
)
def test_malformed_values_fail_fast_with_keyed_message(
    tmp_path: Path, body: str, needle: str
) -> None:
    with pytest.raises(PolicyError, match=needle):
        load_policy(_write(tmp_path, body))


def test_unknown_keys_are_ignored(tmp_path: Path) -> None:
    # forward-compat: unknown scalars and unknown sub-table keys load cleanly.
    sb = load_policy(
        _write(
            tmp_path,
            """
            [sandbox]
            future_scalar = "x"

            [sandbox.capabilities]
            future_key = 1
            """,
        )
    ).sandbox
    assert sb.preset == "fast"


def test_network_deny_is_parsed_but_inert(tmp_path: Path) -> None:
    # SC-7: deny is carried on the policy; nothing in increment A enforces it.
    sb = load_policy(_write(tmp_path, '[sandbox.network]\npolicy = "deny"\n')).sandbox
    assert sb.network.policy == "deny"
