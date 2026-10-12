//! flywheel core: the lifecycle of a **single** task.
//!
//! Invoke the agent, validate the iteration envelope, verify completion
//! claims through graders, record attempts, retry. Knows nothing about who
//! calls it; cross-task concepts (prerequisites, scheduling, claims) live in
//! the orchestrator crate, never here.
//!
//! The authoritative specs live in `docs/` at the repository root and
//! override any behavior inferred from this code.

pub mod envelope;
pub mod lifecycle;
pub mod task;

pub use envelope::{
    BlockedRequirement, EnvelopeResult, Intent, OPENING_FENCE, CLOSING_FENCE, parse_envelope,
};
pub use lifecycle::{Attempt, Lifecycle, LifecycleTransitionError, Outcome, Status};
pub use task::{
    CommandGrader, Context, Grader, GraderType, ManualGrader, RubricGrader, Task, TaskBudgets,
    TranscriptGrader, ValidationError,
};
