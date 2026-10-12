//! The claude-code adapter: identity, probe, CLI stream-json transport,
//! faults.
//!
//! One transport, two input modes. The default drives the headless CLI
//! exactly as the vendor SDKs do (`--input-format stream-json`): the prompt
//! travels as a user message on stdin, the stream stays bidirectional, and
//! interrupt / model switch / permission switch / prompt injection /
//! context-usage queries ride the `control_request` protocol. The
//! `input_mode = "text"` opt-out reproduces the historical container
//! command (prompt piped as plain text, stdin closed), which cannot steer.
//!
//! Requesting any other transport (the Python package's `sdk`) fails
//! explicitly: there is no Rust agent SDK, and none is needed because the
//! SDK transport was itself a wrapper over this exact wire protocol.

pub mod cli;
pub mod common;
pub mod control;
pub mod faults;
pub mod probe;

pub use cli::{ClaudeStreamNormalizer, InputMode, build_cli_plan, start_cli_agent};
pub use control::ClaudeControl;
pub use faults::classify_claude_fault;
pub use probe::{probe_claude_code, probe_claude_code_in};

use crate::BoxFuture;
use crate::adapter::{AdapterServices, AgentAdapter, AgentOptions, DiscoveryContext, RunningAgent};
use crate::capabilities::AgentCapabilities;
use crate::errors::AgentError;
use crate::models::{
    AdapterCompatibility, AgentDescriptor, AgentFault, AgentProbeResult, FaultEvidence, RunRequest,
};

pub const AGENT_ID: &str = "claude-code";

fn capabilities() -> AgentCapabilities {
    AgentCapabilities {
        native_resume: true,
        native_fork: true,
        cancellation: true,
        mid_turn_interrupt: true,
        mid_turn_model_change: true,
        mid_turn_prompt_injection: true,
        approvals: true,
        model_selection: true,
        model_discovery: false,
        mode_selection: false,
        reasoning_selection: true,
        structured_tool_calls: true,
        tool_result_errors: true,
        thought_events: true,
        context_usage: true,
        rate_limit_events: true,
        // Hook lifecycle frames are forwarded RAW (opt-in via
        // include_hook_events); SDK-style in-process hook callbacks have no
        // CLI equivalent. Settings-file command hooks ride adapter option
        // `hooks`.
        hook_events: false,
        subagents: true,
        mcp_configuration: true,
        slash_commands: false,
        account_login_detection: true,
        api_key_detection: true,
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeCodeAdapter {
    descriptor: AgentDescriptor,
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        Self {
            descriptor: AgentDescriptor {
                id: AGENT_ID.into(),
                display_name: "Claude Code".into(),
                adapter_version: "0.2.0".into(),
                vendor: Some("Anthropic".into()),
                executable_names: vec![probe::EXECUTABLE.into()],
                capabilities: capabilities(),
                compatibility: AdapterCompatibility::default(),
            },
        }
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.descriptor
    }

    fn probe(&self) -> BoxFuture<'_, AgentProbeResult> {
        Box::pin(probe_claude_code())
    }

    fn discover_options(&self, _context: DiscoveryContext) -> BoxFuture<'_, AgentOptions> {
        // No native discovery surface is wired; model_id passes through
        // verbatim (the CLI validates it), so an empty list is honest.
        Box::pin(async {
            AgentOptions {
                warnings: vec![
                    "model discovery not implemented; model_id passes through to the claude CLI unvalidated".into(),
                ],
                ..AgentOptions::default()
            }
        })
    }

    fn start(
        &self,
        request: RunRequest,
        services: AdapterServices,
    ) -> BoxFuture<'_, Result<Box<dyn RunningAgent>, AgentError>> {
        Box::pin(async move {
            match request.configuration.option_str("transport").unwrap_or("cli") {
                "cli" => start_cli_agent(&request, &services).await,
                "sdk" => Err(AgentError::UnsupportedCapability(
                    "the sdk transport does not exist in the Rust runtime; the cli transport speaks the same stream-json control protocol the SDK used".into(),
                )),
                other => Err(AgentError::UnsupportedCapability(format!(
                    "unknown claude-code transport {other:?} (expected cli)"
                ))),
            }
        })
    }

    fn classify_fault(&self, evidence: &FaultEvidence) -> Option<AgentFault> {
        classify_claude_fault(evidence)
    }
}
