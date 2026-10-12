//! flywheel agents: the multi-agent execution layer.
//!
//! Bottom of the stack, below core: adapters, normalized events, execution
//! hosts. Imports nothing from any other flywheel crate. Design doc:
//! `docs/agent-harness.md`.
//!
//! One consistent way to run coding-agent harnesses. Adapters translate
//! vendor protocols into normalized events; the runtime folds each run into
//! a [`CompletedRun`]. Two adapters ship: `claude-code` (headless
//! `claude --output-format stream-json`, driven over `--input-format
//! stream-json` so interrupt, model switching, prompt injection, context
//! usage, and tool-permission approvals all work without a vendor SDK) and
//! `codex` (headless `codex exec --json`). Both run under any
//! [`ExecutionHost`]: a local process group or `docker exec` against a
//! bind-mounted worktree.

use std::future::Future;
use std::pin::Pin;

/// Boxed, sendable future: the return type of every async trait method in
/// this crate (keeps the adapter/host traits object-safe).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub mod adapter;
pub mod capabilities;
pub mod claude_code;
pub mod codex;
pub mod config;
pub mod errors;
pub mod events;
pub mod fold;
pub mod hosts;
pub mod jsonl;
pub mod models;
pub mod probe;
pub mod registry;
pub mod runtime;

pub use adapter::{
    AdapterServices, AgentAdapter, AgentControl, AgentModeOption, AgentOptions, ApprovalDecision,
    ApprovalRequest, ApprovalResolver, DiscoveryContext, ModelOption, PluginLoadFailure,
    ReasoningOption, RunningAgent, deny_all_resolver,
};
pub use capabilities::AgentCapabilities;
pub use config::{AdapterOptions, AgentConfiguration, AuthenticationPolicy, PermissionPolicy};
pub use errors::AgentError;
pub use events::{
    AgentEvent, EventSink, EventType, MemorySink, NullSink, Payload, RawAgentEvent, RunRecorder,
};
pub use fold::EventFolder;
pub use hosts::{
    DockerExecHost, ExecutionHost, LocalHost, ProcessPlan, RunningProcess, StdinPlan, StdinWriter,
};
pub use models::{
    AdapterCompatibility, AgentDescriptor, AgentExit, AgentFault, AgentProbeResult, AssuranceLevel,
    AuthenticationKind, CompletedRun, FaultEvidence, PermissionDenial, RateLimitInfo, RunFailure,
    RunRequest, StopInfo, StopReason, ToolInteraction, ToolResult, Usage,
};
pub use probe::ProbeEnvironment;
pub use registry::{AdapterRegistry, default_registry};
pub use runtime::{ActiveRun, AgentRuntime, CancelHandle, RunOptions};
