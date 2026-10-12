//! Error taxonomy for the agents runtime.
//!
//! Every error carries a stable `code` (machine-routable) and `retryable`
//! (whether the same call may succeed later without operator action). Errors
//! must never carry secrets, tokens, or environment dumps in their messages.

/// Every failure the agents runtime can surface to a caller.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    /// No adapter is registered for the requested `agent_id`.
    #[error("{0}")]
    UnknownAgent(String),
    /// An adapter with the same id is already registered.
    #[error("{0}")]
    AdapterAlreadyRegistered(String),
    /// The agent executable is not present on this host.
    #[error("{0}")]
    AgentNotInstalled(String),
    /// The agent is installed but not authenticated.
    #[error("{0}")]
    AgentAuthentication(String),
    /// The requested authentication policy cannot be verified or enforced.
    #[error("{0}")]
    AuthenticationPolicy(String),
    /// A capability was requested that this adapter does not implement.
    #[error("{0}")]
    UnsupportedCapability(String),
    /// The agent process or transport failed before the run began.
    #[error("{0}")]
    AgentStartup(String),
    /// The native protocol violated its own framing or size contract.
    #[error("{0}")]
    AgentProtocol(String),
    /// The agent process died mid-run in a way the adapter cannot fold.
    #[error("{0}")]
    AgentProcessExited(String),
    /// An approval request expired without a decision (resolved as deny).
    #[error("{0}")]
    ApprovalTimeout(String),
    /// A mid-turn control request was rejected by the agent or timed out.
    #[error("{0}")]
    ControlRequest(String),
}

impl AgentError {
    /// Stable machine-routable code, matching the Python error classes.
    pub const fn code(&self) -> &'static str {
        match self {
            AgentError::UnknownAgent(_) => "unknown_agent",
            AgentError::AdapterAlreadyRegistered(_) => "adapter_already_registered",
            AgentError::AgentNotInstalled(_) => "agent_not_installed",
            AgentError::AgentAuthentication(_) => "agent_authentication_required",
            AgentError::AuthenticationPolicy(_) => "authentication_policy_violation",
            AgentError::UnsupportedCapability(_) => "unsupported_capability",
            AgentError::AgentStartup(_) => "agent_startup_failed",
            AgentError::AgentProtocol(_) => "agent_protocol_error",
            AgentError::AgentProcessExited(_) => "agent_process_exited",
            AgentError::ApprovalTimeout(_) => "approval_timeout",
            AgentError::ControlRequest(_) => "control_request_failed",
        }
    }

    /// Whether the same call may succeed later without operator action.
    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            AgentError::AgentAuthentication(_)
                | AgentError::AgentStartup(_)
                | AgentError::AgentProcessExited(_)
                | AgentError::ApprovalTimeout(_)
                | AgentError::ControlRequest(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_retryability_match_python_taxonomy() {
        assert_eq!(AgentError::UnknownAgent("x".into()).code(), "unknown_agent");
        assert!(!AgentError::UnknownAgent("x".into()).retryable());
        assert_eq!(
            AgentError::AgentAuthentication("x".into()).code(),
            "agent_authentication_required"
        );
        assert!(AgentError::AgentAuthentication("x".into()).retryable());
        assert!(AgentError::AgentStartup("x".into()).retryable());
        assert!(AgentError::AgentProcessExited("x".into()).retryable());
        assert!(AgentError::ApprovalTimeout("x".into()).retryable());
        assert!(!AgentError::AgentProtocol("x".into()).retryable());
        assert!(!AgentError::UnsupportedCapability("x".into()).retryable());
        assert_eq!(AgentError::AgentProtocol("boom".into()).to_string(), "boom");
    }
}
