//! Per-adapter capability declaration.
//!
//! Capabilities describe what the **adapter implementation** actually
//! supports, not theoretical vendor functionality. The signal-fidelity group
//! maps one flag per loop-guard input in flywheel's `docs/loop.md` detection
//! table; consumers degrade explicitly per flag, never silently.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentCapabilities {
    // session
    pub native_resume: bool,
    pub native_fork: bool,

    // control
    pub cancellation: bool,
    pub mid_turn_interrupt: bool,
    pub mid_turn_model_change: bool,
    pub mid_turn_prompt_injection: bool,
    pub approvals: bool,

    // selection / discovery
    pub model_selection: bool,
    pub model_discovery: bool,
    pub mode_selection: bool,
    pub reasoning_selection: bool,

    // signal fidelity (loop-guard inputs)
    pub structured_tool_calls: bool,
    pub tool_result_errors: bool,
    pub thought_events: bool,
    pub context_usage: bool,
    pub rate_limit_events: bool,
    pub hook_events: bool,

    // ecosystem
    pub subagents: bool,
    pub mcp_configuration: bool,
    pub slash_commands: bool,

    // auth
    pub account_login_detection: bool,
    pub api_key_detection: bool,
}

impl Default for AgentCapabilities {
    fn default() -> Self {
        Self {
            native_resume: false,
            native_fork: false,
            cancellation: true,
            mid_turn_interrupt: false,
            mid_turn_model_change: false,
            mid_turn_prompt_injection: false,
            approvals: false,
            model_selection: false,
            model_discovery: false,
            mode_selection: false,
            reasoning_selection: false,
            structured_tool_calls: false,
            tool_result_errors: false,
            thought_events: false,
            context_usage: false,
            rate_limit_events: false,
            hook_events: false,
            subagents: false,
            mcp_configuration: false,
            slash_commands: false,
            account_login_detection: false,
            api_key_detection: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_declares_only_cancellation() {
        let caps = AgentCapabilities::default();
        assert!(caps.cancellation);
        assert!(!caps.native_resume);
        assert!(!caps.structured_tool_calls);
        let round: AgentCapabilities =
            serde_json::from_str(&serde_json::to_string(&caps).unwrap()).unwrap();
        assert_eq!(round, caps);
    }
}
