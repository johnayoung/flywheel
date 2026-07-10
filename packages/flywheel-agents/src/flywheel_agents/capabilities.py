"""Per-adapter capability declaration.

Capabilities describe what the **adapter implementation** actually supports,
not theoretical vendor functionality. The signal-fidelity group maps one flag
per loop-guard input in flywheel's ``docs/loop.md`` detection table; consumers
degrade explicitly per flag, never silently.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentCapabilities:
    # session
    native_resume: bool = False
    native_fork: bool = False

    # control
    cancellation: bool = True
    mid_turn_interrupt: bool = False
    mid_turn_model_change: bool = False
    mid_turn_prompt_injection: bool = False
    approvals: bool = False

    # selection / discovery
    model_selection: bool = False
    model_discovery: bool = False
    mode_selection: bool = False
    reasoning_selection: bool = False

    # signal fidelity (loop-guard inputs)
    structured_tool_calls: bool = False
    tool_result_errors: bool = False
    thought_events: bool = False
    context_usage: bool = False
    rate_limit_events: bool = False
    hook_events: bool = False

    # ecosystem
    subagents: bool = False
    mcp_configuration: bool = False
    slash_commands: bool = False

    # auth
    account_login_detection: bool = False
    api_key_detection: bool = False
