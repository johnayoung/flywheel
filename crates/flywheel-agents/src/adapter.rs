//! The adapter contract: lifecycle and capability translation per agent.
//!
//! Adapters translate; the runtime orchestrates. An adapter uses an
//! execution host plus a wire transport; the runtime sees only
//! [`RunningAgent`]. Mid-turn control and approvals are capability-gated
//! surfaces: a transport that cannot provide them returns `None` from
//! [`RunningAgent::control`] and never emits approval events.

use crate::BoxFuture;
use crate::config::PermissionPolicy;
use crate::errors::AgentError;
use crate::events::{AgentEvent, RunRecorder};
use crate::hosts::ExecutionHost;
use crate::models::{
    AgentDescriptor, AgentExit, AgentFault, AgentProbeResult, FaultEvidence, RunRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryContext {
    pub working_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningOption {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub reasoning_options: Vec<ReasoningOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModeOption {
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

/// Discovered options. Static fallback lists must say so in `warnings`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOptions {
    #[serde(default)]
    pub models: Vec<ModelOption>,
    #[serde(default)]
    pub modes: Vec<AgentModeOption>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A tool-permission request surfaced by the agent mid-run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub tool_name: String,
    pub tool_input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// The vendor's full request envelope.
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
    },
    Deny {
        message: String,
        #[serde(default)]
        interrupt: bool,
    },
}

/// Auto-resolving approval policy. flywheel runs unattended: the resolver
/// must decide without a human, and the default denies.
pub type ApprovalResolver =
    Arc<dyn Fn(ApprovalRequest) -> BoxFuture<'static, ApprovalDecision> + Send + Sync>;

/// The default policy: deny every request with a stable message.
pub fn deny_all_resolver() -> ApprovalResolver {
    Arc::new(|request: ApprovalRequest| {
        Box::pin(async move {
            ApprovalDecision::Deny {
                message: format!(
                    "flywheel runs unattended; {} was not pre-approved by the permission policy",
                    request.tool_name
                ),
                interrupt: false,
            }
        })
    })
}

/// What the runtime lends an adapter for one run.
#[derive(Clone)]
pub struct AdapterServices {
    pub host: Arc<dyn ExecutionHost>,
    pub recorder: Arc<RunRecorder>,
    pub approval_resolver: ApprovalResolver,
}

impl AdapterServices {
    pub fn new(host: Arc<dyn ExecutionHost>, recorder: Arc<RunRecorder>) -> Self {
        Self {
            host,
            recorder,
            approval_resolver: deny_all_resolver(),
        }
    }

    pub fn with_approval_resolver(mut self, resolver: ApprovalResolver) -> Self {
        self.approval_resolver = resolver;
        self
    }
}

/// Mid-turn control over a live run. Available only when the running agent
/// reports it from [`RunningAgent::control`].
pub trait AgentControl: Send + Sync {
    /// Interrupt the current turn (the agent keeps the session open).
    fn interrupt(&self) -> BoxFuture<'_, Result<(), AgentError>>;
    /// Switch the model for subsequent turns; `None` restores the default.
    fn set_model(&self, model: Option<String>) -> BoxFuture<'_, Result<(), AgentError>>;
    /// Switch the permission policy for subsequent tool calls.
    fn set_permission_policy(
        &self,
        policy: PermissionPolicy,
    ) -> BoxFuture<'_, Result<(), AgentError>>;
    /// Queue a user message onto the live conversation.
    fn inject_message(&self, text: String) -> BoxFuture<'_, Result<(), AgentError>>;
    /// The agent's own context-window usage breakdown.
    fn context_usage(&self) -> BoxFuture<'_, Result<Value, AgentError>>;
}

/// A live agent run. `next_event` is single-consumer.
pub trait RunningAgent: Send {
    fn native_session_id(&self) -> Option<String>;

    /// The next normalized event, `None` when the stream has ended. Errors
    /// are transport failures; the runtime folds them into a `RunFailure`.
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<AgentEvent>, AgentError>>;

    /// Mid-turn control, when the transport supports it.
    fn control(&self) -> Option<Arc<dyn AgentControl>>;

    /// Terminate the run and its process tree.
    fn cancel(&mut self) -> BoxFuture<'_, ()>;

    fn wait(&mut self) -> BoxFuture<'_, AgentExit>;
}

pub trait AgentAdapter: Send + Sync {
    fn descriptor(&self) -> &AgentDescriptor;

    fn probe(&self) -> BoxFuture<'_, AgentProbeResult>;

    fn discover_options(&self, context: DiscoveryContext) -> BoxFuture<'_, AgentOptions>;

    fn start(
        &self,
        request: RunRequest,
        services: AdapterServices,
    ) -> BoxFuture<'_, Result<Box<dyn RunningAgent>, AgentError>>;

    /// Recognize vendor-specific fault strings. Default: no opinion.
    fn classify_fault(&self, _evidence: &FaultEvidence) -> Option<AgentFault> {
        None
    }
}

/// A third-party adapter that failed to load. Never fatal to the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLoadFailure {
    pub name: String,
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn deny_all_resolver_denies_with_tool_name() {
        let resolver = deny_all_resolver();
        let decision = resolver(ApprovalRequest {
            request_id: "r".into(),
            tool_name: "Bash".into(),
            tool_input: json!({}),
            tool_use_id: None,
            detail: json!({}),
        })
        .await;
        match decision {
            ApprovalDecision::Deny { message, interrupt } => {
                assert!(message.contains("Bash"));
                assert!(!interrupt);
            }
            ApprovalDecision::Allow { .. } => panic!("expected deny"),
        }
    }

    #[test]
    fn approval_decision_serializes_with_behavior_tag() {
        let allow = ApprovalDecision::Allow {
            updated_input: None,
        };
        assert_eq!(
            serde_json::to_value(&allow).unwrap(),
            json!({"behavior": "allow"})
        );
        let deny = ApprovalDecision::Deny {
            message: "no".into(),
            interrupt: true,
        };
        assert_eq!(
            serde_json::to_value(&deny).unwrap(),
            json!({"behavior": "deny", "message": "no", "interrupt": true})
        );
    }
}
