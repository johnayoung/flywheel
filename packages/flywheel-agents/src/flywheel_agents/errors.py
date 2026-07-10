"""Error taxonomy for the agents runtime.

Every error carries a stable ``code`` (machine-routable) and ``retryable``
(whether the same call may succeed later without operator action). Errors must
never carry secrets, tokens, or environment dumps in their messages.
"""

from __future__ import annotations


class AgentHarnessError(Exception):
    """Base class for every flywheel-agents error."""

    code: str = "agent_harness_error"
    retryable: bool = False


class UnknownAgentError(AgentHarnessError):
    """No adapter is registered for the requested ``agent_id``."""

    code = "unknown_agent"


class AgentNotInstalledError(AgentHarnessError):
    """The agent executable (or SDK) is not present on this host."""

    code = "agent_not_installed"


class AgentAuthenticationError(AgentHarnessError):
    """The agent is installed but not authenticated."""

    code = "agent_authentication_required"
    retryable = True


class AuthenticationPolicyError(AgentHarnessError):
    """The requested authentication policy cannot be verified or enforced."""

    code = "authentication_policy_violation"


class UnsupportedCapabilityError(AgentHarnessError):
    """A capability was requested that this adapter does not implement."""

    code = "unsupported_capability"


class AgentStartupError(AgentHarnessError):
    """The agent process or transport failed before the run began."""

    code = "agent_startup_failed"
    retryable = True


class AgentProtocolError(AgentHarnessError):
    """The native protocol violated its own framing or size contract."""

    code = "agent_protocol_error"


class AgentProcessExitedError(AgentHarnessError):
    """The agent process died mid-run in a way the adapter cannot fold."""

    code = "agent_process_exited"
    retryable = True


class ApprovalTimeoutError(AgentHarnessError):
    """An approval request expired without a decision (resolved as deny)."""

    code = "approval_timeout"
    retryable = True
