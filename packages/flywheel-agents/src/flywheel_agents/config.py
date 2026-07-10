"""Run configuration: the generic knobs plus per-adapter escape hatch.

``adapter_options`` carries what the generic fields cannot honestly
represent — for claude-code that is the SDK vocabulary (``skills``,
``allowed_tools``, ``setting_sources``, ``mcp_servers``, ...). Adapters must
reject unknown permission policies rather than silently approximating them.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import StrEnum


class PermissionPolicy(StrEnum):
    AUTO = "auto"
    SUPERVISED = "supervised"
    PLAN = "plan"
    READ_ONLY = "read-only"


class AuthenticationPolicy(StrEnum):
    ACCOUNT_PREFERRED = "account-preferred"
    ACCOUNT_ONLY = "account-only"
    API_ALLOWED = "api-allowed"


@dataclass(frozen=True, slots=True, kw_only=True)
class AgentConfiguration:
    agent_id: str
    model_id: str | None = None
    mode_id: str | None = None
    reasoning_id: str | None = None
    permission_policy: PermissionPolicy = PermissionPolicy.SUPERVISED
    authentication_policy: AuthenticationPolicy = (
        AuthenticationPolicy.ACCOUNT_PREFERRED
    )
    max_turns: int | None = None
    command_override: tuple[str, ...] | None = None
    environment: Mapping[str, str] = field(default_factory=dict)
    adapter_options: Mapping[str, object] = field(default_factory=dict)
