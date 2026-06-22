"""Held-out oracle for spec 00037 SC-1/SC-5 (live-wire, increment A of 00036).

RED until ``sandbox-policy-live-wire`` lands. Pins the testable seam
``build_agent_options(...)``: a helper that constructs ``ClaudeAgentOptions``
from the task-agent sandbox primitives, so ``_make_claude_code_invoke`` and this
oracle build options the same way. Under the ``fast`` primitives the constructed
options must equal today's field set (SC-1, back-compat); a changed primitive
must change the options (SC-5, proving the path is live, not dead code). Do not
weaken or delete assertions — the task brief fences this file.

The seam takes PLAIN PRIMITIVES, never the orchestrator's ``SandboxPolicy``
(core must not import downstream): the resolved policy is decomposed into these
primitives in ``run_task_object`` and threaded down, exactly as ``model`` already is.
"""

from __future__ import annotations

from pathlib import Path

from flywheel_core._sdk import ClaudeAgentOptions
from flywheel_core.workflow import build_agent_options

# The `fast` primitives == today's hardcoded construction (00037 baseline).
FAST = dict(
    permission_mode="bypassPermissions",
    skills="all",
    allowed_tools=(),
    denied_tools=(),
    setting_sources=None,
    mcp_servers=(),
    mcp_strict=False,
)


def test_fast_primitives_reproduce_todays_options() -> None:
    opts = build_agent_options(Path("/tmp/sbx"), model=None, max_turns=500, **FAST)
    assert isinstance(opts, ClaudeAgentOptions)
    assert opts.cwd == "/tmp/sbx"
    assert list(opts.add_dirs) == ["/tmp/sbx"]
    assert opts.permission_mode == "bypassPermissions"
    assert opts.skills == "all"
    assert opts.max_turns == 500
    assert opts.model is None
    # omit-on-unset: today set NONE of these, so the SDK derives setting_sources
    # from skills="all". The fast mapping must leave them at their SDK defaults.
    assert opts.setting_sources is None
    assert not opts.allowed_tools
    assert not opts.disallowed_tools
    assert not opts.mcp_servers


def test_changed_primitive_changes_options() -> None:
    opts = build_agent_options(
        Path("/tmp/sbx"),
        model=None,
        max_turns=500,
        **{**FAST, "permission_mode": "default", "allowed_tools": ("Read",)},
    )
    assert opts.permission_mode == "default"
    assert tuple(opts.allowed_tools) == ("Read",)


def test_max_turns_and_model_thread() -> None:
    opts = build_agent_options(
        Path("/tmp/sbx"), model="claude-opus-4-8", max_turns=7, **FAST
    )
    assert opts.max_turns == 7
    assert opts.model == "claude-opus-4-8"
