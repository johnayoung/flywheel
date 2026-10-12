//! Harness: the single-task orchestration loop.
//!
//! Port of the Python `flywheel_core.harness`. Wires the invoker, the
//! envelope parser, the prompt builder, and the grader runners into one
//! driver: given a [`Task`] and a [`Lifecycle`], [`run_task`] carries the
//! lifecycle to a terminal status (`done` / `failed`) or pauses it
//! (`interrupted` / `awaiting_approval`) using a single owned state machine.
//!
//! The harness is the **sole owner of lifecycle transitions**: graders
//! persist receipts, the invoker captures agent signals, the envelope parser
//! classifies the iteration outcome -- but only this module appends the
//! `TransitionedTo` events that move a lifecycle. Every transition goes
//! through the domain-event ledger ([`state::Run::append`]); the in-memory
//! lifecycle is the fold of that log.
//!
//! Per `docs/loop.md`'s detection map this module dispatches on the envelope
//! `intent` (`verify` / `blocked` / `continue` / `abort`), transcript budget
//! breaches, malformed / missing / duplicate / truncated envelopes (protocol
//! failures, never coerced to `continue`), invoker crashes, transient rate
//! limits, the repeated-failure (STUCK) and tuple-repetition (THRASH) loop
//! guards, the inter-message hang watchdog, the wall-clock iteration
//! deadline, the context-recovery policy (boundary and mid-turn), per-run
//! budget ceilings, the manual-approval gate, and the landability gate.
//!
//! What `docs/loop.md` still flags as TODO is explicitly deferred (see
//! [`DEFERRED_LOOP_SUBSYSTEMS`]) and not papered over with heuristics.

mod attempt;
mod iterate;
mod ops;
mod requires;
mod state;
mod telemetry;
#[cfg(test)]
mod tests;
mod validate;

pub use crate::agents_invoke::{
    InvocationObservers, InvocationRequest, InvokeFn, InvokeFunc, InvokeFuture, MessageObserver,
};
pub use iterate::{build_observation, build_usage_breakdown, occupancy_from_usage};
pub use ops::{
    FinalizeStrandedOptions, RecheckOptions, RecheckOutcome, ResolveApprovalOptions,
    ResolveApprovalOutcome, evaluate_blocked_predicate, finalize_stranded_lifecycle,
    recheck_blocked_lifecycle, resolve_manual_approval,
};
pub use requires::{
    parse_blocked_requires_json, requires_json, serialize_requires,
    validate_blocked_requires_against_task,
};
pub use state::ShutdownSignal;
pub use telemetry::{
    DOMAIN_MIRROR_KIND_PREFIX, MirroringStore, RunTelemetry, SharedSink, TELEMETRY_FAILED_KIND,
    store_error_type,
};

use crate::deadline_config::DeadlineConfig;
use crate::events::{DomainEventPayload, LifecycleInitialized};
use crate::faults::BackoffPolicy;
use crate::grader_command::CommandGraderError;
use crate::grader_rubric::JudgeInvoke;
use crate::grader_transcript::{MonotonicClock, monotonic_seconds};
use crate::invoker_client::{Clock, ProgressProbe};
use crate::lifecycle::{Attempt, Lifecycle, Status};
use crate::loop_guard::LoopGuardConfig;
use crate::prompt::RecoveryHandoff;
use crate::recovery_summarizer::SummarizerInvoke;
use crate::store::{
    AttemptStore, HarnessStore, LifecycleStore, StoreConflictError, StoreError, TaskStore,
};
use crate::task::Task;
use chrono::{DateTime, Utc};
use serde_json::json;
use state::{Run, object};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

/// `docs/loop.md` subsystems still TODO after the safety-net work. They are
/// intentionally not implemented here so the "not silently faked" assertion
/// holds; the mechanical detectors that did ship (repeated-failure STUCK,
/// tuple-repetition THRASH, the hang watchdog mechanism) are deliberately
/// not in this list.
pub const DEFERRED_LOOP_SUBSYSTEMS: [&str; 5] = [
    "thrash net-diff detection (sub-problem b)",
    "thrash input-novelty score (sub-problem c)",
    "hang threshold default value (mechanism shipped, value ungrounded)",
    "fine-grained crash classification",
    "blocked_implicit same-question-re-asked detection",
];

/// The stable prefix every per-run budget-ceiling breach error carries
/// (specs 00039/00042): the durable discriminator a work re-driver uses to
/// tell a budget kill from an `intent=abort`.
pub const BUDGET_CEILING_ERROR_PREFIX: &str = "budget ceiling breached:";

/// The default agent id the harness builds its cold-session judge and
/// summarizer for when no injected seam is configured.
pub const DEFAULT_AGENT_ID: &str = "claude-code";

/// The default turn cap for the recovery summarizer's cold session.
pub const DEFAULT_SUMMARIZER_MAX_TURNS: u32 = 8;

/// The awaitable sleep seam the transient rate-limit retry waits on.
pub type SleepFn = Arc<dyn Fn(f64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The git-free landability hook (spec 00061): `None` when the finished
/// change is landable, a non-empty reason when it is not.
pub type LandabilityGate = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Per-run harness knobs.
///
/// Every field has the same meaning as its Python counterpart:
///
/// * `max_retries` -- the validation retry budget consumed by
///   [`Lifecycle::is_retry_eligible`].
/// * `max_cost_usd` / `max_tokens` / `wall_clock_seconds` -- PER-RUN
///   cumulative ceilings checked after the per-iteration rollup but before
///   grading; a breach ends the run terminal `FAILED` (non-retryable) with a
///   `harness.budget_ceiling_breached` event. `0` disables each.
/// * `max_iterations_per_attempt` -- caps the inner `intent=continue` loop.
/// * `max_transient_retries` / `transient_backoff` / `transient_sleep` --
///   the SEPARATE bounded budget for re-invoking a rate-limited iteration.
/// * `artifacts_root` -- base directory for per-attempt `attempt-NNN`
///   subdirectories (falls back to the lifecycle's `artifacts_dir`).
/// * `agent_context` -- the immutable snapshot persisted on every attempt.
/// * `worktree` / `grader_env` -- the command graders' and judge's working
///   directory and full replacement environment.
/// * `rubric_judge_model` / `rubric_judge_max_turns` / `rubric_judge_retries`
///   / `rubric_judge_invoke` -- the rubric judge knobs and test seam.
/// * `loop_guard` -- STUCK / THRASH thresholds plus `hang_timeout_seconds`.
/// * `deadlines` -- the default-on wall-clock ceilings per call class.
/// * `context_window_tokens` / `context_recovery_trigger_ratio` /
///   `max_context_recoveries` / `recovery_summarizer_invoke` -- the
///   context-recovery policy (disabled while the capacity is `None`).
/// * `landability_gate` -- consulted at the verify-passed `-> DONE` edge.
/// * `checkpoint_nudge_seconds` / `checkpoint_progress_probe` -- threaded
///   onto every invocation request for the live checkpoint nudge.
/// * `agent_id` / `recovery_summarizer_max_turns` -- the cold-session agent
///   the default judge / summarizer run on (Rust-only: the Python defaults
///   were the SDK's `query`).
#[derive(Clone)]
pub struct HarnessConfig {
    pub max_retries: u32,
    pub max_cost_usd: f64,
    pub max_tokens: u64,
    pub wall_clock_seconds: u64,
    pub max_iterations_per_attempt: u32,
    pub max_transient_retries: u32,
    pub transient_backoff: BackoffPolicy,
    pub transient_sleep: Option<SleepFn>,
    pub artifacts_root: Option<PathBuf>,
    pub agent_context: BTreeMap<String, String>,
    pub worktree: Option<PathBuf>,
    pub grader_env: Option<BTreeMap<String, String>>,
    pub rubric_judge_model: Option<String>,
    pub rubric_judge_max_turns: u32,
    pub rubric_judge_retries: u32,
    pub rubric_judge_invoke: Option<Arc<dyn JudgeInvoke>>,
    pub loop_guard: LoopGuardConfig,
    pub deadlines: DeadlineConfig,
    pub context_window_tokens: Option<u64>,
    pub context_recovery_trigger_ratio: f64,
    pub max_context_recoveries: u32,
    pub recovery_summarizer_invoke: Option<Arc<dyn SummarizerInvoke>>,
    pub landability_gate: Option<LandabilityGate>,
    pub checkpoint_nudge_seconds: f64,
    pub checkpoint_progress_probe: Option<ProgressProbe>,
    pub agent_id: String,
    pub recovery_summarizer_max_turns: u32,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            max_retries: 0,
            max_cost_usd: 0.0,
            max_tokens: 0,
            wall_clock_seconds: 0,
            max_iterations_per_attempt: 1,
            max_transient_retries: 6,
            transient_backoff: BackoffPolicy::default(),
            transient_sleep: None,
            artifacts_root: None,
            agent_context: BTreeMap::new(),
            worktree: None,
            grader_env: None,
            rubric_judge_model: None,
            rubric_judge_max_turns: 32,
            rubric_judge_retries: 2,
            rubric_judge_invoke: None,
            loop_guard: LoopGuardConfig::default(),
            deadlines: DeadlineConfig::default(),
            context_window_tokens: None,
            context_recovery_trigger_ratio: 0.9,
            max_context_recoveries: 1,
            recovery_summarizer_invoke: None,
            landability_gate: None,
            checkpoint_nudge_seconds: 300.0,
            checkpoint_progress_probe: None,
            agent_id: DEFAULT_AGENT_ID.to_owned(),
            recovery_summarizer_max_turns: DEFAULT_SUMMARIZER_MAX_TURNS,
        }
    }
}

impl fmt::Debug for HarnessConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HarnessConfig")
            .field("max_retries", &self.max_retries)
            .field("max_cost_usd", &self.max_cost_usd)
            .field("max_tokens", &self.max_tokens)
            .field("wall_clock_seconds", &self.wall_clock_seconds)
            .field(
                "max_iterations_per_attempt",
                &self.max_iterations_per_attempt,
            )
            .field("max_transient_retries", &self.max_transient_retries)
            .field("transient_backoff", &self.transient_backoff)
            .field("artifacts_root", &self.artifacts_root)
            .field("agent_context", &self.agent_context)
            .field("worktree", &self.worktree)
            .field("rubric_judge_model", &self.rubric_judge_model)
            .field("rubric_judge_max_turns", &self.rubric_judge_max_turns)
            .field("rubric_judge_retries", &self.rubric_judge_retries)
            .field("rubric_judge_invoke", &self.rubric_judge_invoke.is_some())
            .field("loop_guard", &self.loop_guard)
            .field("deadlines", &self.deadlines)
            .field("context_window_tokens", &self.context_window_tokens)
            .field(
                "context_recovery_trigger_ratio",
                &self.context_recovery_trigger_ratio,
            )
            .field("max_context_recoveries", &self.max_context_recoveries)
            .field(
                "recovery_summarizer_invoke",
                &self.recovery_summarizer_invoke.is_some(),
            )
            .field("landability_gate", &self.landability_gate.is_some())
            .field("checkpoint_nudge_seconds", &self.checkpoint_nudge_seconds)
            .field(
                "checkpoint_progress_probe",
                &self.checkpoint_progress_probe.is_some(),
            )
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

/// An out-of-range [`HarnessConfig`] knob, rejected at `run_task` entry
/// rather than silently clamped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HarnessConfigError(pub String);

impl HarnessConfig {
    /// Validate the knobs the Python dataclass rejected at construction.
    pub fn validate(&self) -> Result<(), HarnessConfigError> {
        let ratio = self.context_recovery_trigger_ratio;
        if !ratio.is_finite() || ratio <= 0.0 || ratio > 1.0 {
            return Err(HarnessConfigError(format!(
                "context_recovery_trigger_ratio must be in (0, 1], got {ratio}"
            )));
        }
        if let Some(capacity) = self.context_window_tokens
            && capacity == 0
        {
            return Err(HarnessConfigError(
                "context_window_tokens must be a positive int or None, got 0".into(),
            ));
        }
        Ok(())
    }
}

/// Final return value of [`run_task`].
///
/// `lifecycle` is the lifecycle in its terminal-or-paused state;
/// `attempts` is the full attempt history as persisted (in `number`
/// order); `session_limit_reset` carries the derived session-limit reset
/// instant when the run fast-aborted on a future reset.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessOutcome {
    pub lifecycle: Lifecycle,
    pub attempts: Vec<Attempt>,
    pub session_limit_reset: Option<DateTime<Utc>>,
}

/// Every failure [`run_task`] can surface to its caller.
///
/// Every variant except [`HarnessError::Interrupted`] is the "entry crash"
/// class: the harness records one `harness.crash` event, walks the
/// lifecycle to `FAILED`, and returns the error. `Interrupted` is the
/// re-raised operator shutdown: the open attempt was finalized in-band and
/// the lifecycle parked `INTERRUPTED`.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness config: {0}")]
    Config(#[from] HarnessConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    CommandGrader(#[from] CommandGraderError),
    #[error("harness io: {0}")]
    Io(#[from] std::io::Error),
    #[error("harness encountered unexpected lifecycle status: {0}")]
    UnexpectedStatus(Status),
    #[error("operator interrupted the run")]
    Interrupted,
}

impl HarnessError {
    /// A stable error-class label (the Rust spelling of Python's
    /// `type(exc).__name__`).
    pub fn error_type(&self) -> &'static str {
        match self {
            HarnessError::Config(_) => "HarnessConfigError",
            HarnessError::Store(err) => store_error_type(err),
            HarnessError::CommandGrader(CommandGraderError::Store(err)) => store_error_type(err),
            HarnessError::CommandGrader(CommandGraderError::Artifact { .. }) => {
                "ArtifactWriteError"
            }
            HarnessError::Io(_) => "IoError",
            HarnessError::UnexpectedStatus(_) => "RuntimeError",
            HarnessError::Interrupted => "Interrupted",
        }
    }
}

/// The injectable seams of one [`run_task`] call: the telemetry sink, the
/// wall and monotonic clocks, and the shutdown signal.
#[derive(Clone, Default)]
pub struct RunOptions {
    pub sink: Option<SharedSink>,
    pub now: Option<Clock>,
    pub monotonic: Option<MonotonicClock>,
    pub shutdown: Option<ShutdownSignal>,
}

impl fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunOptions")
            .field("sink", &self.sink.is_some())
            .field("now", &self.now.is_some())
            .field("monotonic", &self.monotonic.is_some())
            .field("shutdown", &self.shutdown.is_some())
            .finish()
    }
}

impl RunOptions {
    pub fn with_sink(mut self, sink: SharedSink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_now(mut self, now: Clock) -> Self {
        self.now = Some(now);
        self
    }

    pub fn with_monotonic(mut self, monotonic: MonotonicClock) -> Self {
        self.monotonic = Some(monotonic);
        self
    }

    pub fn with_shutdown(mut self, shutdown: ShutdownSignal) -> Self {
        self.shutdown = Some(shutdown);
        self
    }
}

pub(crate) fn default_clock() -> Clock {
    Arc::new(Utc::now)
}

pub(crate) fn default_monotonic() -> MonotonicClock {
    Arc::new(monotonic_seconds)
}

/// Mutable per-`run_task` recovery accounting (spec 00018 FR-4).
///
/// `recoveries_used` is shared with the mid-turn threshold checker so the
/// boundary and mid-turn paths decrement the same budget; `pending_handoff`
/// is consumed exactly once by the next attempt's first iteration prompt.
pub(crate) struct RecoveryState {
    pub recoveries_used: Arc<AtomicU32>,
    pub pending_handoff: Option<RecoveryHandoff>,
}

impl RecoveryState {
    fn new() -> Self {
        Self {
            recoveries_used: Arc::new(AtomicU32::new(0)),
            pending_handoff: None,
        }
    }
}

/// Mutable per-`run_task` holder for a derived session-limit reset.
#[derive(Default)]
pub(crate) struct SessionLimitState {
    pub reset_at: Option<DateTime<Utc>>,
}

/// Drive `task` against `lifecycle` to a terminal or paused status.
///
/// The flow per `docs/loop.md`:
///
/// 1. Persist the task definition and seed the lifecycle by appending the
///    `LifecycleInitialized` event (the resume case -- the row already exists
///    -- is swallowed and the persisted row then wins over the caller's
///    in-memory fields). Bring `pending` / `interrupted` to `ready`.
/// 2. While not terminal / parked: `ready -> running`, start an attempt,
///    drive iterations, classify the terminal iteration (see
///    `attempt::run_attempt_body`), and on `failed_validation` /
///    `internal_error` consult [`Lifecycle::is_retry_eligible`] -- `ready`
///    with `harness.retry_scheduled` when retries remain, else `failed`.
/// 3. Return the lifecycle and the full attempt history.
///
/// `lifecycle` is mutated in place so the caller's handle reflects the same
/// state the outcome carries, on every exit path. `store` failures and
/// other infrastructure faults are recorded as one `harness.crash`
/// (`classification='entry_error'`) with the lifecycle walked to `FAILED`,
/// then returned; an operator shutdown returns
/// [`HarnessError::Interrupted`] after the in-band `INTERRUPTED` park.
pub async fn run_task(
    task: &Task,
    lifecycle: &mut Lifecycle,
    store: Arc<dyn HarnessStore>,
    config: &HarnessConfig,
    invoke: InvokeFn,
    options: RunOptions,
) -> Result<HarnessOutcome, HarnessError> {
    config.validate()?;
    let clock = options.now.unwrap_or_else(default_clock);
    let mclock = options.monotonic.unwrap_or_else(default_monotonic);
    let telemetry = Arc::new(RunTelemetry::new(
        options.sink,
        lifecycle.run_id.clone(),
        clock.clone(),
    ));
    let store = Arc::new(MirroringStore::new(store, telemetry.clone()));

    // Persist the task definition before seeding the lifecycle so every run
    // records the exact task it executed; the digest pins this run to its
    // task version via LifecycleInitialized -> Lifecycle.task_content_hash.
    let task_content_hash = store.save_task(task, clock())?;
    lifecycle.task_content_hash = task_content_hash.clone();

    let run = Run::new(
        store.clone(),
        telemetry,
        lifecycle.clone(),
        clock,
        mclock,
        options.shutdown,
    );

    let seed = run.append_payload(
        run.now(),
        None,
        DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
            task_id: lifecycle.task_id.clone(),
            worker_id: lifecycle.worker_id.clone(),
            artifacts_dir: lifecycle.artifacts_dir.clone(),
            task_content_hash,
            source: lifecycle.source.clone(),
        }),
    );
    match seed {
        Ok(()) => {}
        // Resume: the row was persisted by a prior run_task call. Fall
        // through to the load so the canonical state replaces any stale
        // fields the caller may be holding.
        Err(StoreError::Conflict(StoreConflictError::LifecycleAlreadyExists { .. })) => {}
        Err(err) => {
            *lifecycle = run.snapshot();
            return Err(err.into());
        }
    }

    let result = match store.load_lifecycle(&run.run_id) {
        Ok(stored) => {
            if let Some(stored) = stored {
                run.with_lifecycle(|lc| {
                    lc.replace_from(&stored);
                    lc.attempts = stored.attempts.clone();
                });
            }
            drive(task, &run, config, &invoke).await
        }
        Err(err) => Err(err.into()),
    };

    let outcome = match result {
        Ok(session_state) => match store.list_attempts(&run.run_id) {
            Ok(attempts) => Ok((attempts, session_state.reset_at)),
            Err(err) => Err(HarnessError::Store(err)),
        },
        Err(err) => Err(err),
    };
    let outcome = match outcome {
        Ok((attempts, session_limit_reset)) => Ok((attempts, session_limit_reset)),
        Err(HarnessError::Interrupted) => Err(HarnessError::Interrupted),
        Err(err) => {
            // Everything else is internal failure: record one harness.crash
            // and walk the lifecycle to FAILED before surfacing the error so
            // the worker still exits non-zero with the original fault.
            match record_entry_crash(&run, &err) {
                Ok(()) => Err(err),
                Err(secondary) => Err(secondary.into()),
            }
        }
    };

    *lifecycle = run.snapshot();
    outcome.map(|(attempts, session_limit_reset)| HarnessOutcome {
        lifecycle: lifecycle.clone(),
        attempts,
        session_limit_reset,
    })
}

async fn drive(
    task: &Task,
    run: &Run,
    config: &HarnessConfig,
    invoke: &InvokeFn,
) -> Result<SessionLimitState, HarnessError> {
    let mut recovery_state = RecoveryState::new();
    let mut session_state = SessionLimitState::default();

    // Entry-time normalization: bring resumable states to `ready`.
    match run.status() {
        Status::Pending | Status::Interrupted => run.transition(Status::Ready, "")?,
        _ => {}
    }

    loop {
        match run.status() {
            Status::Done | Status::Failed | Status::Interrupted | Status::AwaitingApproval => {
                break;
            }
            Status::Ready => {
                attempt::run_attempt(
                    task,
                    run,
                    config,
                    invoke,
                    &mut recovery_state,
                    &mut session_state,
                )
                .await?;
            }
            Status::FailedValidation | Status::InternalError => {
                let (eligible, retries, error) = run.with_lifecycle(|lc| {
                    (
                        lc.is_retry_eligible(config.max_retries),
                        lc.retries,
                        lc.error.clone(),
                    )
                });
                if eligible {
                    run.emit(
                        "harness.retry_scheduled",
                        object(json!({
                            "retries_used": retries,
                            "max_retries": config.max_retries,
                        })),
                        None,
                    );
                    run.transition(Status::Ready, "")?;
                    continue;
                }
                let terminal_error = if error.is_empty() {
                    format!("retries exhausted ({retries}/{})", config.max_retries)
                } else {
                    error
                };
                run.transition(Status::Failed, &terminal_error)?;
            }
            other => return Err(HarnessError::UnexpectedStatus(other)),
        }
    }
    Ok(session_state)
}

/// Per-status walk from an entry-crash detection point to `FAILED`. Statuses
/// absent from this table (DONE, FAILED, INTERRUPTED, AWAITING_APPROVAL) are
/// terminal or quiescent -- no walk is attempted.
fn entry_crash_path(status: Status) -> &'static [Status] {
    match status {
        Status::Pending => &[Status::Ready, Status::Running, Status::Failed],
        Status::Ready => &[Status::Running, Status::Failed],
        Status::Running => &[Status::Failed],
        Status::Validating => &[Status::InternalError, Status::Failed],
        Status::FailedValidation | Status::InternalError => &[Status::Failed],
        Status::Done | Status::Failed | Status::Interrupted | Status::AwaitingApproval => &[],
    }
}

/// Emit one `harness.crash` (`classification='entry_error'`) and walk the
/// lifecycle to `FAILED` along the legal edge set from its current status.
/// A transition failure (an optimistic-concurrency loser racing another
/// worker on the same run) propagates so the caller learns its lifecycle is
/// owned elsewhere.
fn record_entry_crash(run: &Run, err: &HarnessError) -> Result<(), StoreError> {
    let error = format!("harness entry crash: {}: {err}", err.error_type());
    run.emit(
        "harness.crash",
        object(json!({
            "classification": "entry_error",
            "exception_type": err.error_type(),
            "message": err.to_string(),
        })),
        None,
    );
    for target in entry_crash_path(run.status()) {
        let target_error = if target.requires_error() {
            error.as_str()
        } else {
            ""
        };
        run.transition(*target, target_error)?;
    }
    Ok(())
}
