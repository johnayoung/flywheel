//! The `RunTask` seam: how the orchestrate loop hands one task to a runner.
//!
//! The Python loop called `flywheel_core.workflow.run_task_object` directly.
//! The Rust loop instead owns a narrow seam -- [`RunTask`] -- carrying
//! everything the loop hands the runner (task, seeded lifecycle, sandbox,
//! agent env, invoke wrapper, policy-derived harness config, telemetry
//! root, worker id, source ref, budgets). The product daemon binds it to the
//! core workflow; tests drive it with scripted outcomes. The loop never
//! inspects how the runner reaches its [`HarnessOutcome`]: it only reads
//! the authoritative lifecycle the runner finalized in the store.

use crate::policy::Skills;
use flywheel_core::agents_invoke::InvokeFn;
use flywheel_core::harness::ShutdownSignal;
use flywheel_core::harness::{HarnessConfig, HarnessError, HarnessOutcome, SharedSink};
use flywheel_core::invoker_client::Clock;
use flywheel_core::lifecycle::Lifecycle;
use flywheel_core::store::{HarnessStore, StoreConflictError, StoreError};
use flywheel_core::task::Task;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::sources::LogSink;

/// Whether a run is minted fresh or continues a persisted lifecycle on its
/// own run id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Fresh,
    Resume,
}

impl RunMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            RunMode::Fresh => "fresh",
            RunMode::Resume => "resume",
        }
    }
}

impl fmt::Display for RunMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The plain agent-side primitives decomposed from the resolved
/// `[sandbox.*]` policy (spec 00037 SC-5/SC-6) plus the `[agent]` opt-in.
///
/// Only primitives cross the seam: the runner builds its agent options from
/// these the same way the Python `run_task_object` did. `agent_env` already
/// carries the sandbox handle's contribution (the handle wins on
/// collision); `grader_env` is the full replacement environment command
/// graders run with (`None` -- inherit as before).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentPrimitives {
    pub agent_id: Option<String>,
    pub agent_transport: Option<String>,
    pub permission_mode: String,
    pub skills: Skills,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub setting_sources: Option<Vec<String>>,
    pub mcp_servers: Vec<String>,
    pub mcp_strict: bool,
    pub exec_enabled: bool,
    pub exec_auto_allow: bool,
    pub agent_env: BTreeMap<String, String>,
    pub grader_env: Option<BTreeMap<String, String>>,
}

/// Everything the loop hands the runner for one task execution.
///
/// `lifecycle` is the seed: on a fresh run it is a new `Lifecycle` whose
/// `run_id` the loop minted, on a resume it carries the persisted run id;
/// its `worker_id` and `source` are set to `worker_id` / `source_ref`. The
/// runner seeds the store with it exactly as `run_task` does (the resume
/// case folds the persisted row over it). `config` is the policy-derived
/// [`HarnessConfig`] with the landability gate and checkpoint progress probe
/// already bound over `sandbox`; `config.max_retries == max_retries`.
#[derive(Clone)]
pub struct RunTaskRequest {
    pub task: Task,
    pub lifecycle: Lifecycle,
    pub mode: RunMode,
    pub store: Arc<dyn HarnessStore>,
    pub sandbox: PathBuf,
    pub agent: AgentPrimitives,
    pub invoke: Option<InvokeFn>,
    pub config: HarnessConfig,
    pub model: Option<String>,
    pub max_turns: u32,
    pub max_retries: u32,
    pub worker_id: String,
    pub source_ref: String,
    pub telemetry_root: Option<PathBuf>,
    pub sink: Option<SharedSink>,
    pub now: Clock,
    pub shutdown: Option<ShutdownSignal>,
    pub log: Option<LogSink>,
}

impl fmt::Debug for RunTaskRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunTaskRequest")
            .field("task_id", &self.task.id)
            .field("run_id", &self.lifecycle.run_id)
            .field("mode", &self.mode)
            .field("sandbox", &self.sandbox)
            .field("agent", &self.agent)
            .field("invoke", &self.invoke.is_some())
            .field("model", &self.model)
            .field("max_turns", &self.max_turns)
            .field("max_retries", &self.max_retries)
            .field("worker_id", &self.worker_id)
            .field("source_ref", &self.source_ref)
            .field("telemetry_root", &self.telemetry_root)
            .finish_non_exhaustive()
    }
}

impl RunTaskRequest {
    /// The run id this request drives (minted or resumed).
    pub fn run_id(&self) -> &str {
        &self.lifecycle.run_id
    }
}

/// Why a runner did not return a [`HarnessOutcome`].
///
/// `Interrupted` is the harness's re-raised shutdown: the open attempt was
/// finalized in-band and the lifecycle parked `INTERRUPTED` -- whether by
/// the operator's SIGINT/SIGTERM or an in-band `interrupt` control command
/// (the loop tells the two apart; see `drive_or_relinquish`). `Store` and
/// `Harness` are infrastructure failures the harness already recorded as an
/// entry crash; an optimistic-concurrency conflict inside them is read as a
/// mid-run claim loss and relinquished rather than propagated.
#[derive(Debug, thiserror::Error)]
pub enum RunTaskError {
    #[error("operator interrupted the run")]
    Interrupted,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Harness(HarnessError),
    #[error("{0}")]
    Other(String),
}

impl From<HarnessError> for RunTaskError {
    fn from(err: HarnessError) -> Self {
        match err {
            HarnessError::Interrupted => RunTaskError::Interrupted,
            HarnessError::Store(err) => RunTaskError::Store(err),
            other => RunTaskError::Harness(other),
        }
    }
}

impl RunTaskError {
    /// True when the failure is a lost optimistic-concurrency race on the
    /// lifecycle: a peer finalized the run out from under this worker.
    pub fn is_optimistic_concurrency(&self) -> bool {
        let store_err = match self {
            RunTaskError::Store(err) => err,
            RunTaskError::Harness(HarnessError::Store(err)) => err,
            RunTaskError::Harness(HarnessError::CommandGrader(
                flywheel_core::grader_command::CommandGraderError::Store(err),
            )) => err,
            _ => return false,
        };
        matches!(
            store_err,
            StoreError::Conflict(StoreConflictError::OptimisticConcurrency { .. })
        )
    }
}

/// The runner's future.
pub type RunTaskFuture = Pin<Box<dyn Future<Output = Result<HarnessOutcome, RunTaskError>> + Send>>;

/// The runner seam the loop dispatches through.
///
/// Implementations must finalize the lifecycle in `request.store` before
/// returning: the loop reads the authoritative status from the outcome's
/// `lifecycle` and the store, never from any agent claim.
pub trait RunTask: Send + Sync {
    fn run(&self, request: RunTaskRequest) -> RunTaskFuture;
}

impl<F> RunTask for F
where
    F: Fn(RunTaskRequest) -> RunTaskFuture + Send + Sync,
{
    fn run(&self, request: RunTaskRequest) -> RunTaskFuture {
        self(request)
    }
}

/// A shareable runner.
pub type SharedRunTask = Arc<dyn RunTask>;
