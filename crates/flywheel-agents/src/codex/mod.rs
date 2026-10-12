//! The codex adapter: identity, probe, headless `codex exec --json`
//! transport, faults.
//!
//! One transport (`cli`) under any execution host. Requesting any other
//! transport fails explicitly rather than approximating.
//!
//! Capability notes: the normalizer emits `plan.updated` events from
//! `todo_list` items regardless of declaration (the capability set has no
//! plan-event flag). `max_turns` is ignored by the CLI plan (`codex exec`
//! has no turn-cap flag); the run's wall-clock ceiling is the bound. The
//! headless exec protocol is one-way, so no mid-turn control surface exists
//! (the JSON-RPC app server is the future home of that).

pub mod cli;
pub mod probe;

pub use cli::{CodexStreamNormalizer, build_cli_plan, start_cli_agent};
pub use probe::{probe_codex, probe_codex_in};

use crate::BoxFuture;
use crate::adapter::{AdapterServices, AgentAdapter, AgentOptions, DiscoveryContext, RunningAgent};
use crate::capabilities::AgentCapabilities;
use crate::errors::AgentError;
use crate::models::{
    AdapterCompatibility, AgentDescriptor, AgentFault, AgentProbeResult, FaultEvidence, RunRequest,
};

pub const AGENT_ID: &str = "codex";

fn capabilities() -> AgentCapabilities {
    AgentCapabilities {
        cancellation: true,
        model_selection: true,
        structured_tool_calls: true,
        tool_result_errors: true,
        thought_events: true,
        account_login_detection: true,
        api_key_detection: true,
        ..AgentCapabilities::default()
    }
}

#[derive(Debug, Clone)]
pub struct CodexAdapter {
    descriptor: AgentDescriptor,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            descriptor: AgentDescriptor {
                id: AGENT_ID.into(),
                display_name: "Codex".into(),
                adapter_version: "0.1.0".into(),
                vendor: Some("OpenAI".into()),
                executable_names: vec![probe::EXECUTABLE.into()],
                capabilities: capabilities(),
                compatibility: AdapterCompatibility::default(),
            },
        }
    }
}

impl AgentAdapter for CodexAdapter {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.descriptor
    }

    fn probe(&self) -> BoxFuture<'_, AgentProbeResult> {
        Box::pin(probe_codex())
    }

    fn discover_options(&self, _context: DiscoveryContext) -> BoxFuture<'_, AgentOptions> {
        Box::pin(async {
            AgentOptions {
                warnings: vec![
                    "model discovery not implemented; model_id passes through to the codex CLI unvalidated".into(),
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
            match request.configuration.option("transport") {
                None | Some(serde_json::Value::Null) => start_cli_agent(&request, &services).await,
                Some(serde_json::Value::String(t)) if t == "cli" => {
                    start_cli_agent(&request, &services).await
                }
                Some(other) => Err(AgentError::UnsupportedCapability(format!(
                    "unknown codex transport {other} (codex supports only cli)"
                ))),
            }
        })
    }

    /// No vetted codex fault strings yet: deliberately returns `None`.
    /// Recognizing vendor refusal text requires observed evidence; guessing
    /// strings would misclassify runs.
    fn classify_fault(&self, _evidence: &FaultEvidence) -> Option<AgentFault> {
        None
    }
}
