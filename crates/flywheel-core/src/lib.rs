//! flywheel core: the lifecycle of a **single** task.
//!
//! Invoke the agent, validate the iteration envelope, verify completion
//! claims through graders, record attempts, retry. Knows nothing about who
//! calls it; cross-task concepts (prerequisites, scheduling, claims) live in
//! the orchestrator crate, never here.
//!
//! The authoritative specs live in `docs/` at the repository root and
//! override any behavior inferred from this code.

pub mod agents_invoke;
pub mod audit;
pub mod deadline;
pub mod deadline_config;
pub mod envelope;
pub mod event_serde;
pub mod events;
pub mod faults;
pub mod grader_command;
pub mod grader_manual;
pub mod grader_rubric;
pub mod grader_transcript;
pub mod harness;
pub mod invoker;
pub mod invoker_client;
pub mod lifecycle;
pub mod loaders;
pub mod loop_guard;
pub mod loop_path_marker;
pub mod notifier;
pub mod prompt;
pub mod recovery_summarizer;
pub mod redaction;
pub mod store;
pub mod store_memory;
pub mod store_postgres;
pub mod store_sqlite;
pub mod strategy;
pub mod task;
pub mod telemetry_file;
pub mod validation;
pub mod workflow;

pub use envelope::{
    BlockedRequirement, CLOSING_FENCE, EnvelopeResult, Intent, OPENING_FENCE, ValidEnvelope,
    parse_envelope,
};
pub use harness::{
    HarnessConfig, HarnessError, HarnessOutcome, RecheckOutcome, ResolveApprovalOutcome,
    RunOptions, ShutdownSignal, finalize_stranded_lifecycle, recheck_blocked_lifecycle,
    resolve_manual_approval, run_task,
};
pub use lifecycle::{Attempt, Lifecycle, LifecycleTransitionError, Outcome, Status};
pub use task::{
    CommandGrader, Context, Grader, GraderType, ManualGrader, RubricGrader, Task, TaskBudgets,
    TranscriptGrader, ValidationError,
};
pub use workflow::{
    AgentOptions, EventsMode, RunStore, RunTaskOptions, WorkflowError, build_inline_task,
    recover_stranded_lifecycles, run_task_file, run_task_object,
};
