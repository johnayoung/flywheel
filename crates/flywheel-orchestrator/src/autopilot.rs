//! Autopilot: the unattended intake half of the loop (spec 00058).
//!
//! flywheel already *executes* graded tasks autonomously. Autopilot is the
//! loop that *authors* them: it discovers what a codebase needs (one
//! relevance agent per tier), sequences the findings by the
//! `docs/autopilot.md` tier model, and compiles each selected finding into a
//! grader-bearing [`Task`] the existing worker then drains and lands.
//!
//! This module owns the phase's shared, pure invariants -- the data model
//! every later layer imports and never redefines or extends:
//!
//! * [`Tier`] -- the 11 tiers of `docs/autopilot.md` with the preemptive
//!   (`<= 3`) vs scheduled (`>= 4`) boundary.
//! * [`Finding`] -- one concrete unit of work a tier agent surfaced, carrying
//!   its identity, tier, the four score axes (urgency, importance, blocks,
//!   effort), a `ready` status (the `Status == ready` gate the preemptive
//!   override keys on), and the descriptive fields (title/detail/evidence)
//!   the authoring layer compiles from.
//! * [`ScoreBreakdown`] -- the legible, recorded score: the five components
//!   plus the final, so a recommendation is auditable rather than an opaque
//!   number.
//!
//! The scoring engine here is pure: no file I/O, no agent calls, no
//! subprocess, no clock. Default weights are [`AutopilotWeights::default`]
//! (the `[autopilot.weights]` override lands in the policy layer, which
//! passes a [`ScoreWeights`] into these functions -- this module never reads
//! config).
//!
//! Agent access goes through two injectable seams -- [`AutopilotInvoker`]
//! (prompt in, text out: the per-tier fan-out, authoring, and `init`) and
//! [`DiscoverySessionRunner`] (prompt in, drained [`IterationResult`] out:
//! the single-session tier-subagent discovery) -- with the agents-runtime
//! implementations [`AgentsRepoInvoker`] and [`AgentsSingleSessionRunner`]
//! shipped as the production defaults. Both run under the
//! [`DeadlineClass::AutopilotAgent`] wall-clock ceiling (spec 00066
//! criterion #6) so a stalled stream is cancelled rather than parking the
//! daemon.

use crate::claims::{ClaimStore, ClaimStoreError, STOP_NO_OP_CYCLE};
use crate::policy::{AutopilotWeights, DEFAULT_AUTOPILOT_LANDING, DEFAULT_AUTOPILOT_TARGET_DEPTH};
use crate::sources::{DirectoryWorkSource, WorkSource, WorkSourceError, load_active_tasks};
use crate::surface_lint::conflict_key_for_path;
use chrono::{DateTime, Utc};
use flywheel_agents::{
    AgentConfiguration, AgentError, AgentRuntime, ExecutionHost, PermissionPolicy, RunOptions,
    RunRequest,
};
use flywheel_core::agents_invoke::{
    ColdSession, ColdSessionError, completed_run_to_iteration_result,
};
use flywheel_core::deadline::{DeadlineExceeded, run_with_deadline};
use flywheel_core::deadline_config::{
    DEFAULT_AUTOPILOT_AGENT_SECONDS, DeadlineClass, DeadlineConfig,
};
use flywheel_core::invoker::{IterationResult, ToolResultContent, ToolResultObservation};
use flywheel_core::lifecycle::Status;
use flywheel_core::loaders::{load_task_data, serialize_task};
use flywheel_core::store::{LifecycleFilter, LifecycleStore};
use flywheel_core::task::{Grader, Task};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

// --- Tier model (docs/autopilot.md) -----------------------------------------------

/// The inclusive upper bound of the preemptive band; tiers `<= 3` preempt.
pub const PREEMPTIVE_MAX_TIER: u8 = 3;

/// The 11 autopilot tiers, in priority order (1 highest).
///
/// Tiers 1-3 are *preemptive interrupts*: while any such finding is `ready`
/// it floats above everything below. Tiers 4-11 are *scheduled* by weighted
/// score, never by strict tier order. [`Tier::is_preemptive`] is the single
/// boundary every layer keys on, so the `<= 3` rule lives in exactly one
/// place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum Tier {
    ProductionDown = 1,
    ImminentSevereRisk = 2,
    BrokenBuild = 3,
    CommittedDeliverables = 4,
    CoreFeatureWork = 5,
    TestCoverage = 6,
    NonCriticalBugs = 7,
    TechDebt = 8,
    ObservabilityTooling = 9,
    Documentation = 10,
    Polish = 11,
}

impl Tier {
    /// Every tier in priority order (1 first).
    pub const ALL: [Tier; 11] = [
        Tier::ProductionDown,
        Tier::ImminentSevereRisk,
        Tier::BrokenBuild,
        Tier::CommittedDeliverables,
        Tier::CoreFeatureWork,
        Tier::TestCoverage,
        Tier::NonCriticalBugs,
        Tier::TechDebt,
        Tier::ObservabilityTooling,
        Tier::Documentation,
        Tier::Polish,
    ];

    /// The tier's numeric value (1-11).
    pub const fn value(self) -> u8 {
        self as u8
    }

    /// The tier for a numeric value, `None` outside 1-11.
    pub fn from_value(value: i64) -> Option<Tier> {
        Tier::ALL
            .into_iter()
            .find(|tier| i64::from(tier.value()) == value)
    }

    /// True for tiers 1-3 (the preemptive interrupt band).
    pub const fn is_preemptive(self) -> bool {
        self.value() <= PREEMPTIVE_MAX_TIER
    }

    /// Per-tier base weight (`TierWeight[Tier]` in the docs formula):
    /// `12 - tier`. A higher tier (lower number) carries more weight; the
    /// scheduled band uses tiers 4-11 (weights 8..1). Defined for all 11
    /// tiers so a non-ready preemptive finding still scores under the
    /// scheduled branch with a sensible weight.
    pub const fn weight(self) -> u32 {
        12 - self.value() as u32
    }

    /// The tier's class name from `docs/autopilot.md`.
    pub const fn name(self) -> &'static str {
        match self {
            Tier::ProductionDown => "Production down / active harm",
            Tier::ImminentSevereRisk => "Imminent severe risk",
            Tier::BrokenBuild => "Broken build / blocked pipeline",
            Tier::CommittedDeliverables => "Committed deliverables at risk",
            Tier::CoreFeatureWork => "Core feature work",
            Tier::TestCoverage => "Test coverage (shipped/in-flight)",
            Tier::NonCriticalBugs => "Non-critical bugs",
            Tier::TechDebt => "Tech debt / refactoring",
            Tier::ObservabilityTooling => "Observability & tooling",
            Tier::Documentation => "Documentation",
            Tier::Polish => "Polish / nice-to-have",
        }
    }

    /// The tier's one-line remit from `docs/autopilot.md`, handed to its
    /// relevance agent.
    pub const fn description(self) -> &'static str {
        match self {
            Tier::ProductionDown => {
                "Outages, in-progress data loss, active breach, payments failing. \
                 Nothing below advances while open."
            }
            Tier::ImminentSevereRisk => {
                "Actively/trivially exploitable vuln, spreading corruption, \
                 hard-deadline compliance violation, dependency about to break prod."
            }
            Tier::BrokenBuild => {
                "CI red, main can't deploy, team blocked from shipping. Blocks \
                 everyone's forward motion."
            }
            Tier::CommittedDeliverables => {
                "Work with external commitments (customer deadlines, contracts, \
                 dependent teams) about to slip."
            }
            Tier::CoreFeatureWork => {
                "The roadmap -- new functionality delivering primary value. Default \
                 steady state."
            }
            Tier::TestCoverage => {
                "Tests for code that exists or is being written. Untested code \
                 becomes tomorrow's Tier 1."
            }
            Tier::NonCriticalBugs => "Known defects, not blocking or severe. System functions.",
            Tier::TechDebt => "Cleanup that improves velocity and reduces risk.",
            Tier::ObservabilityTooling => {
                "Logging, metrics, dashboards, dev ergonomics. Compounding \
                 dividends, rarely urgent."
            }
            Tier::Documentation => {
                "READMEs, API docs, runbooks, onboarding. Valuable, almost never \
                 time-critical."
            }
            Tier::Polish => "Cosmetic tweaks, minor optimizations, \"wouldn't it be nice.\"",
        }
    }
}

impl TryFrom<u8> for Tier {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Tier::from_value(i64::from(value)).ok_or_else(|| format!("{value} is not a valid Tier"))
    }
}

impl From<Tier> for u8 {
    fn from(tier: Tier) -> u8 {
        tier.value()
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value())
    }
}

/// `TierWeight[Tier]` from the docs formula (see [`Tier::weight`]).
pub fn tier_weight(tier: Tier) -> u32 {
    tier.weight()
}

/// The tier's remit handed to its relevance agent: the class name and the
/// one-line description from `docs/autopilot.md`.
pub fn tier_description(tier: Tier) -> (&'static str, &'static str) {
    (tier.name(), tier.description())
}

// --- Score weights ------------------------------------------------------------------

/// `INTERRUPT_BASE` from the docs formula -- large enough that any ready
/// preemptive finding's `INTERRUPT_BASE + Urgency` dominates every possible
/// scheduled score, so the preemptive override is absolute by arithmetic as
/// well as by partition.
pub const INTERRUPT_BASE: f64 = 10_000.0;
pub const W_TIER: f64 = 10.0;
pub const W_URGENCY: f64 = 3.0;
pub const W_IMPORTANCE: f64 = 3.0;
pub const W_UNBLOCK: f64 = 2.0;
pub const W_EFFORT: f64 = 1.0;

/// The tunable weights of the `docs/autopilot.md` scheduled-score formula:
/// the policy's [`AutopilotWeights`] (defined once in [`crate::policy`];
/// its `Default` is the shipped weights above).
pub type ScoreWeights = AutopilotWeights;

/// The shipped weights (the module constants).
pub fn default_weights() -> ScoreWeights {
    ScoreWeights::default()
}

// --- Findings -----------------------------------------------------------------------

/// One concrete unit of work a tier relevance agent surfaced.
///
/// Carries an identity, its tier, the four score axes, and a `ready` status
/// (the `Status == ready` gate the preemptive override keys on), plus the
/// descriptive fields the authoring layer compiles a task from
/// (`title`/`detail`/`evidence`). Downstream layers import this shape and
/// add no field to it.
///
/// `urgency`/`importance`/`blocks`/`effort` are agent-supplied estimates;
/// the *final score* is computed here from them and is never an
/// agent-reported number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub tier: Tier,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub urgency: u32,
    #[serde(default)]
    pub importance: u32,
    #[serde(default)]
    pub blocks: u32,
    #[serde(default)]
    pub effort: u32,
    #[serde(default = "default_ready")]
    pub ready: bool,
}

fn default_ready() -> bool {
    true
}

impl Finding {
    /// A ready finding with every axis at zero and no detail/evidence.
    pub fn new(id: impl Into<String>, tier: Tier, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            tier,
            title: title.into(),
            detail: String::new(),
            evidence: Vec::new(),
            urgency: 0,
            importance: 0,
            blocks: 0,
            effort: 0,
            ready: true,
        }
    }
}

// --- Score breakdown (legible, recorded) --------------------------------------------

/// The legible score recorded on every sequenced finding.
///
/// Records the five components (`tier_weight`, `urgency`, `importance`,
/// `blocks`, `effort`) plus the `final_score` so the recommendation is
/// auditable: a test recomputes the documented formula from the recorded
/// components and asserts it equals the final. `preemptive` records which
/// formula branch produced it (the `INTERRUPT_BASE + Urgency` interrupt
/// branch, or the weighted scheduled branch), so the recompute is
/// unambiguous.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub tier: Tier,
    pub tier_weight: u32,
    pub urgency: u32,
    pub importance: u32,
    pub blocks: u32,
    pub effort: u32,
    #[serde(rename = "final")]
    pub final_score: f64,
    pub preemptive: bool,
}

/// A [`Finding`] paired with the [`ScoreBreakdown`] it earned.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredFinding {
    pub finding: Finding,
    pub breakdown: ScoreBreakdown,
}

/// Compute the [`ScoreBreakdown`] for one finding.
///
/// Implements the `docs/autopilot.md` formula verbatim:
///
/// * a ready Tier 1-3 finding scores `INTERRUPT_BASE + Urgency * w_urg`
///   (preemptive branch -- it floats above everything);
/// * everything else scores
///   `TierWeight*w_tier + Urgency*w_urg + Importance*w_imp
///   + BlocksCount*w_unblock - Effort*w_effort` (scheduled branch).
pub fn score_finding(finding: &Finding, weights: &ScoreWeights) -> ScoreBreakdown {
    let tier_weight = finding.tier.weight();
    let preemptive = finding.tier.is_preemptive() && finding.ready;
    let final_score = compute_final(
        tier_weight,
        finding.urgency,
        finding.importance,
        finding.blocks,
        finding.effort,
        preemptive,
        weights,
    );
    ScoreBreakdown {
        tier: finding.tier,
        tier_weight,
        urgency: finding.urgency,
        importance: finding.importance,
        blocks: finding.blocks,
        effort: finding.effort,
        final_score,
        preemptive,
    }
}

/// The two-branch scoring formula, applied to raw components.
fn compute_final(
    tier_weight: u32,
    urgency: u32,
    importance: u32,
    blocks: u32,
    effort: u32,
    preemptive: bool,
    weights: &ScoreWeights,
) -> f64 {
    if preemptive {
        return weights.interrupt_base + f64::from(urgency) * weights.urgency;
    }
    f64::from(tier_weight) * weights.tier
        + f64::from(urgency) * weights.urgency
        + f64::from(importance) * weights.importance
        + f64::from(blocks) * weights.unblock
        - f64::from(effort) * weights.effort
}

/// Re-evaluate the documented formula from a breakdown's own components.
///
/// The audit primitive behind criterion #3: feeding a recorded breakdown's
/// five components (and its `preemptive` branch flag) back through the
/// formula must reproduce its final. A breakdown whose final ignores a
/// component fails this round-trip.
pub fn recompute_final(breakdown: &ScoreBreakdown, weights: &ScoreWeights) -> f64 {
    compute_final(
        breakdown.tier_weight,
        breakdown.urgency,
        breakdown.importance,
        breakdown.blocks,
        breakdown.effort,
        breakdown.preemptive,
        weights,
    )
}

/// Sequence findings by the tier model: preemptive first, then weighted.
///
/// Every ready Tier 1-3 finding precedes every Tier 4-11 finding regardless
/// of the scheduled scores (the absolute preemptive override). Within each
/// band, order is by descending final score; ties break deterministically
/// by input order (a stable sort), never randomly. An empty input yields an
/// empty sequence.
pub fn sequence_findings(findings: &[Finding], weights: &ScoreWeights) -> Vec<ScoredFinding> {
    let (mut preemptive, mut scheduled): (Vec<ScoredFinding>, Vec<ScoredFinding>) = findings
        .iter()
        .map(|finding| ScoredFinding {
            finding: finding.clone(),
            breakdown: score_finding(finding, weights),
        })
        .partition(|scored| scored.breakdown.preemptive);
    let descending = |a: &ScoredFinding, b: &ScoredFinding| {
        b.breakdown
            .final_score
            .partial_cmp(&a.breakdown.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    preemptive.sort_by(descending);
    scheduled.sort_by(descending);
    preemptive.extend(scheduled);
    preemptive
}

/// Select the top `slots` findings in sequenced order (fill-to-depth).
///
/// The loop layer computes `slots` as `target_depth - current_depth` and
/// asks for that many; this returns the highest-priority findings up to
/// that count (fewer when the input has fewer). A non-positive `slots`
/// selects nothing.
pub fn select_findings(
    findings: &[Finding],
    slots: i64,
    weights: &ScoreWeights,
) -> Vec<ScoredFinding> {
    if slots <= 0 {
        return Vec::new();
    }
    let mut sequenced = sequence_findings(findings, weights);
    sequenced.truncate(usize::try_from(slots).unwrap_or(usize::MAX));
    sequenced
}

// --- Relevance verdict (the per-tier discovery output) ------------------------------

/// One tier's relevance verdict plus the findings it surfaced.
///
/// The structured output of the discovery fan-out: each of the 11 tiers
/// produces exactly one of these. A tier judged not relevant to the codebase
/// carries `relevant = false`, a `reason`, and zero `findings`; a relevant
/// tier may still surface zero findings (distinct from not-relevant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierVerdict {
    pub tier: Tier,
    pub relevant: bool,
    pub reason: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

impl TierVerdict {
    fn not_relevant(tier: Tier, reason: impl Into<String>) -> Self {
        Self {
            tier,
            relevant: false,
            reason: reason.into(),
            findings: Vec::new(),
        }
    }
}

// --- The agent seams --------------------------------------------------------------

/// A failure of one autopilot agent call.
///
/// The `Display` text leads with the failure class
/// (`DeadlineExceeded`, `AgentError`, `AgentRunFailure`) so a recorded drop
/// or error-verdict reason distinguishes a wall-clock timeout from an
/// ordinary agent failure -- the Rust spelling of the Python
/// `f"{type(exc).__name__}: {exc}"`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvokeError {
    /// The call outlived its wall-clock ceiling and was cancelled.
    #[error("DeadlineExceeded: {0}")]
    DeadlineExceeded(DeadlineExceeded),
    /// The agent run could not start (unknown agent, spawn failure, policy).
    #[error("AgentError: {0}")]
    Start(AgentError),
    /// The agent process or transport failed mid-run.
    #[error("AgentRunFailure: {error_type}: {message}")]
    Run {
        error_type: String,
        message: String,
        exit_code: Option<i32>,
        stderr: Option<String>,
    },
    /// Any other failure a scripted or foreign invoker reports.
    #[error("{0}")]
    Other(String),
}

impl From<DeadlineExceeded> for InvokeError {
    fn from(err: DeadlineExceeded) -> Self {
        InvokeError::DeadlineExceeded(err)
    }
}

impl From<AgentError> for InvokeError {
    fn from(err: AgentError) -> Self {
        InvokeError::Start(err)
    }
}

impl From<ColdSessionError> for InvokeError {
    fn from(err: ColdSessionError) -> Self {
        match err {
            ColdSessionError::Start(err) => InvokeError::Start(err),
            ColdSessionError::Run {
                error_type,
                message,
                exit_code,
                stderr,
            } => InvokeError::Run {
                error_type,
                message,
                exit_code,
                stderr,
            },
        }
    }
}

impl From<String> for InvokeError {
    fn from(message: String) -> Self {
        InvokeError::Other(message)
    }
}

impl From<&str> for InvokeError {
    fn from(message: &str) -> Self {
        InvokeError::Other(message.to_owned())
    }
}

/// The boxed future an [`AutopilotInvoker`] returns.
pub type AutopilotInvokeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, InvokeError>> + Send + 'a>>;

/// The injectable agent seam: a prompt in, the agent's response text out.
///
/// The shipped driver is [`AgentsRepoInvoker`] (a cold session through the
/// flywheel-agents runtime rooted in the repo); tests inject a scripted one
/// returning canned per-tier JSON so the fan-out is deterministic and
/// offline. Any `Fn(String) -> Fut` closure whose future is
/// `Send + 'static` implements the trait directly.
pub trait AutopilotInvoker: Send + Sync {
    fn invoke<'a>(&'a self, prompt: &'a str) -> AutopilotInvokeFuture<'a>;
}

impl<F, Fut> AutopilotInvoker for F
where
    F: Fn(String) -> Fut + Send + Sync,
    Fut: Future<Output = Result<String, InvokeError>> + Send + 'static,
{
    fn invoke<'a>(&'a self, prompt: &'a str) -> AutopilotInvokeFuture<'a> {
        Box::pin(self(prompt.to_owned()))
    }
}

/// The boxed future a [`DiscoverySessionRunner`] returns.
pub type SessionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<IterationResult, InvokeError>> + Send + 'a>>;

/// The single-session seam: drive one discovery session for `prompt` and
/// return its drained [`IterationResult`].
///
/// The shipped runner is [`AgentsSingleSessionRunner`]; tests inject one
/// returning a canned result with per-subagent tool-result blocks, so the
/// collector is exercised with no agent process and no live model.
pub trait DiscoverySessionRunner: Send + Sync {
    fn run<'a>(&'a self, prompt: &'a str) -> SessionFuture<'a>;
}

impl<F, Fut> DiscoverySessionRunner for F
where
    F: Fn(String) -> Fut + Send + Sync,
    Fut: Future<Output = Result<IterationResult, InvokeError>> + Send + 'static,
{
    fn run<'a>(&'a self, prompt: &'a str) -> SessionFuture<'a> {
        Box::pin(self(prompt.to_owned()))
    }
}

/// Bound `future` by `deadline_seconds` of wall-clock time.
///
/// `max_turns` is a turn budget, not a time budget: a stream that streams
/// forever without ever spending a turn would otherwise run unbounded. The
/// bound is total elapsed wall-clock; on timeout the future is dropped
/// (cancelling the run) and [`DeadlineExceeded`] surfaces so the cycle
/// records a distinguishable timeout outcome rather than parking the
/// daemon. `None` (the operator per-class opt-out) restores the unbounded
/// await.
async fn bounded<F: Future>(
    future: F,
    deadline_seconds: Option<f64>,
) -> Result<F::Output, DeadlineExceeded> {
    match deadline_seconds {
        None => Ok(future.await),
        Some(seconds) => {
            let ceiling = Duration::from_secs_f64(seconds.max(0.0));
            run_with_deadline(future, ceiling).await
        }
    }
}

/// Default per-tier turn budget for the agents-backed repo invoker.
pub const DEFAULT_DISCOVERY_MAX_TURNS: u32 = 60;

/// The agent every autopilot session runs on when the policy names none.
pub const DEFAULT_AGENT_ID: &str = "claude-code";

/// How [`build_repo_invoker`] configures its cold session.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoInvokerOptions {
    /// The `[agent] id`; [`DEFAULT_AGENT_ID`] when the policy names none.
    pub agent_id: String,
    /// The `[agent] transport` adapter option, when set.
    pub agent_transport: Option<String>,
    pub model: Option<String>,
    pub max_turns: u32,
    /// Wall-clock ceiling per call; `None` opts out (unbounded).
    pub deadline_seconds: Option<f64>,
    /// Test-only escape hatch substituting the agent executable.
    pub command_override: Option<Vec<String>>,
}

impl Default for RepoInvokerOptions {
    fn default() -> Self {
        Self {
            agent_id: DEFAULT_AGENT_ID.to_owned(),
            agent_transport: None,
            model: None,
            max_turns: DEFAULT_DISCOVERY_MAX_TURNS,
            deadline_seconds: Some(DEFAULT_AUTOPILOT_AGENT_SECONDS),
            command_override: None,
        }
    }
}

impl RepoInvokerOptions {
    /// Options from the policy's `[agent]` opt-in: an unset id falls back
    /// to [`DEFAULT_AGENT_ID`].
    pub fn from_policy(agent_id: Option<&str>, agent_transport: Option<&str>) -> Self {
        Self {
            agent_id: agent_id.unwrap_or(DEFAULT_AGENT_ID).to_owned(),
            agent_transport: agent_transport.map(str::to_owned),
            ..Self::default()
        }
    }
}

/// The agent configuration a repo invoker runs: auto permissions, the
/// caller's model and turn budget, the transport riding `adapter_options`.
fn repo_invoker_configuration(options: &RepoInvokerOptions) -> AgentConfiguration {
    let mut configuration = AgentConfiguration::new(options.agent_id.as_str());
    configuration.model_id = options.model.clone();
    configuration.max_turns = Some(options.max_turns);
    configuration.permission_policy = PermissionPolicy::Auto;
    configuration.command_override = options.command_override.clone();
    if let Some(transport) = &options.agent_transport {
        configuration
            .adapter_options
            .insert("transport".into(), json!(transport));
    }
    configuration
}

/// The shipped [`AutopilotInvoker`]: one cold agent session per call,
/// rooted in the repo, under the autopilot wall-clock ceiling.
///
/// The agent gets read access to the repo so it can judge tier relevance,
/// surface findings, and author tasks. A call that outlives
/// `deadline_seconds` is cancelled and reports
/// [`InvokeError::DeadlineExceeded`].
pub struct AgentsRepoInvoker {
    session: ColdSession,
    repo_root: PathBuf,
    deadline_seconds: Option<f64>,
}

impl fmt::Debug for AgentsRepoInvoker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentsRepoInvoker")
            .field("session", &self.session)
            .field("repo_root", &self.repo_root)
            .field("deadline_seconds", &self.deadline_seconds)
            .finish()
    }
}

impl AgentsRepoInvoker {
    pub fn with_runtime(mut self, runtime: Arc<AgentRuntime>) -> Self {
        self.session = self.session.with_runtime(runtime);
        self
    }

    pub fn with_host(mut self, host: Arc<dyn ExecutionHost>) -> Self {
        self.session = self.session.with_host(host);
        self
    }

    pub fn configuration(&self) -> &AgentConfiguration {
        self.session.configuration()
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn deadline_seconds(&self) -> Option<f64> {
        self.deadline_seconds
    }
}

impl AutopilotInvoker for AgentsRepoInvoker {
    fn invoke<'a>(&'a self, prompt: &'a str) -> AutopilotInvokeFuture<'a> {
        Box::pin(async move {
            let output = bounded(
                self.session.run(prompt, &self.repo_root, None),
                self.deadline_seconds,
            )
            .await??;
            Ok(output.text)
        })
    }
}

/// Build the production agent seam: a cold session rooted in `repo_root`.
///
/// `options.deadline_seconds` is the wall-clock ceiling the agent call runs
/// under (spec 00066 criterion #6); it defaults to the resolved-config
/// autopilot ceiling so the call is bounded by default.
pub fn build_repo_invoker(
    repo_root: impl Into<PathBuf>,
    options: RepoInvokerOptions,
) -> AgentsRepoInvoker {
    AgentsRepoInvoker {
        session: ColdSession::with_configuration(repo_invoker_configuration(&options)),
        repo_root: repo_root.into(),
        deadline_seconds: options.deadline_seconds,
    }
}

// --- Discovery: per-tier relevance fan-out ------------------------------------------
//
// One agent invocation per tier. Each agent reads the repo, decides whether
// its tier is even relevant to *this* codebase (D-1: relevance is judged
// per-repo by an agent, never by coded detectors), and surfaces zero or more
// concrete findings with evidence.

/// Build the relevance-and-findings prompt for one tier.
///
/// The prompt names the tier (so a scripted invoker can route on
/// `TIER: N`), states its remit, and pins the structured-JSON output
/// contract the fan-out parses. It explicitly authorizes a *not relevant*
/// verdict so a tier that does not apply to this repo contributes nothing
/// rather than inventing work.
pub fn tier_prompt(tier: Tier, repo_root: &Path) -> String {
    let (name, description) = tier_description(tier);
    format!(
        "You are the relevance agent for autopilot TIER: {} ({name}).\n\n\
         Tier remit: {description}\n\n\
         Read the repository at {} and decide whether THIS tier is relevant to THIS \
         codebase. Relevance is per-codebase: e.g. \"production down\" is meaningless \
         for a pure library. If the tier does not apply, say so -- do not invent work \
         to fill it.\n\n\
         If the tier is relevant, surface zero or more concrete findings, each backed \
         by evidence you actually observed in the repo. For each finding estimate, on \
         a 0-10 scale: urgency (how fast cost grows if untouched), importance \
         (value/risk-reduction if done), blocks (how many other efforts it unblocks), \
         effort (cost to do). Set ready=true when the work can start now.\n\n\
         Respond with exactly one fenced JSON block:\n\
         ```json\n\
         {{\n\
         \x20 \"relevant\": true,\n\
         \x20 \"reason\": \"one sentence on why this tier does or does not apply\",\n\
         \x20 \"findings\": [\n\
         \x20   {{\"id\": \"short-slug\", \"title\": \"...\", \"detail\": \"...\", \
         \"evidence\": [\"path:line or observation\"], \"urgency\": 0, \"importance\": 0, \
         \"blocks\": 0, \"effort\": 0, \"ready\": true}}\n\
         \x20 ]\n\
         }}\n\
         ```\n\
         A not-relevant verdict carries relevant=false, a reason, and an empty findings \
         list.",
        tier.value(),
        repo_root.display(),
    )
}

/// Extract the structured JSON object from an agent response.
///
/// Prefers a ```` ```json ```` fenced block (the contract in
/// [`tier_prompt`]); falls back to the outermost `{...}` span so a model
/// that omitted the fence still parses. Errors when no JSON object is
/// present so the caller records a parse failure rather than a silent
/// empty verdict.
pub fn extract_json_object(text: &str) -> Result<Map<String, Value>, String> {
    const FENCE: &str = "```json";
    if let Some(start) = text.find(FENCE) {
        let body_start = start + FENCE.len();
        if let Some(end) = text[body_start..].find("```") {
            let candidate = text[body_start..body_start + end].trim();
            let data: Value = serde_json::from_str(candidate).map_err(|e| e.to_string())?;
            return match data {
                Value::Object(map) => Ok(map),
                _ => Err("fenced JSON is not an object".to_owned()),
            };
        }
    }
    let first = text.find('{');
    let last = text.rfind('}');
    let (Some(first), Some(last)) = (first, last) else {
        return Err("no JSON object found in agent response".to_owned());
    };
    if last <= first {
        return Err("no JSON object found in agent response".to_owned());
    }
    let data: Value = serde_json::from_str(&text[first..=last]).map_err(|e| e.to_string())?;
    match data {
        Value::Object(map) => Ok(map),
        _ => Err("response JSON is not an object".to_owned()),
    }
}

/// Coerce an agent-supplied score axis to a non-negative int, default 0.
fn coerce_axis(value: Option<&Value>) -> u32 {
    match value {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                u32::try_from(i.max(0)).unwrap_or(u32::MAX)
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f > 0.0 {
                    u32::try_from(f.trunc() as i64).unwrap_or(u32::MAX)
                } else {
                    0
                }
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// The string at `key` when it is non-blank, verbatim (untrimmed).
fn non_blank_str<'m>(map: &'m Map<String, Value>, key: &str) -> Option<&'m str> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// The non-empty, non-blank string at `key`, trimmed.
fn trimmed_str(map: &Map<String, Value>, key: &str) -> Option<String> {
    non_blank_str(map, key).map(|s| s.trim().to_owned())
}

/// Build [`Finding`] values for one tier from the agent's list.
///
/// Each finding is stamped with `tier` here -- the agent never assigns its
/// own tier, so a finding cannot land in a tier its relevance agent was not
/// asked about. A malformed entry (not an object, or missing a title) is
/// skipped rather than aborting the tier.
fn parse_findings(tier: Tier, raw: Option<&Value>) -> Vec<Finding> {
    let Some(Value::Array(entries)) = raw else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    for (idx, entry) in entries.iter().enumerate() {
        let Value::Object(entry) = entry else {
            continue;
        };
        let Some(title) = trimmed_str(entry, "title") else {
            continue;
        };
        let slug = non_blank_str(entry, "id")
            .map(str::to_owned)
            .unwrap_or_else(|| idx.to_string());
        let evidence = match entry.get("evidence") {
            Some(Value::Array(items)) => items.iter().map(value_as_text).collect(),
            _ => Vec::new(),
        };
        findings.push(Finding {
            id: format!("t{}-{slug}", tier.value()),
            tier,
            title,
            detail: entry
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            evidence,
            urgency: coerce_axis(entry.get("urgency")),
            importance: coerce_axis(entry.get("importance")),
            blocks: coerce_axis(entry.get("blocks")),
            effort: coerce_axis(entry.get("effort")),
            ready: entry.get("ready").and_then(Value::as_bool).unwrap_or(true),
        });
    }
    findings
}

/// The Python `str(value)` of a JSON value: strings verbatim, everything
/// else in its JSON spelling.
fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parse one tier agent's response into a [`TierVerdict`].
///
/// A not-relevant verdict always carries zero findings: even if the agent
/// contradicts itself and lists findings under `relevant=false`, they are
/// dropped here so a not-relevant tier can never contribute work
/// (criterion #2). A response with no parseable JSON yields a not-relevant
/// verdict with the parse error as its reason -- the fan-out records the
/// failure rather than crashing.
pub fn parse_tier_verdict(tier: Tier, text: &str) -> TierVerdict {
    match extract_json_object(text) {
        Ok(data) => verdict_from_data(tier, &data),
        Err(err) => {
            TierVerdict::not_relevant(tier, format!("unparseable discovery response: {err}"))
        }
    }
}

/// Build a [`TierVerdict`] from an already-parsed verdict object.
///
/// Shared by [`parse_tier_verdict`] (one tier's own JSON) and the
/// aggregated-final-message fallback, so the relevance/reason/findings rules
/// -- a not-relevant verdict always drops any findings the agent listed --
/// live in exactly one place.
fn verdict_from_data(tier: Tier, data: &Map<String, Value>) -> TierVerdict {
    let relevant = data.get("relevant").is_some_and(json_truthy);
    let reason = trimmed_str(data, "reason");
    if !relevant {
        return TierVerdict::not_relevant(
            tier,
            reason.unwrap_or_else(|| "tier judged not relevant to this codebase".to_owned()),
        );
    }
    TierVerdict {
        tier,
        relevant: true,
        reason: reason.unwrap_or_else(|| "tier judged relevant to this codebase".to_owned()),
        findings: parse_findings(tier, data.get("findings")),
    }
}

/// Python `bool(value)` over a JSON value.
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Run one tier's relevance agent and parse its verdict.
///
/// Any failure of the agent call is contained and returned as a
/// not-relevant verdict carrying the error reason, so one failing tier never
/// aborts the fan-out (best-effort, mirroring the orchestrator's report
/// posture).
pub async fn discover_tier(
    tier: Tier,
    repo_root: &Path,
    invoker: &dyn AutopilotInvoker,
) -> TierVerdict {
    let prompt = tier_prompt(tier, repo_root);
    match invoker.invoke(&prompt).await {
        Ok(text) => parse_tier_verdict(tier, &text),
        Err(err) => TierVerdict::not_relevant(tier, format!("discovery agent error: {err}")),
    }
}

/// Drive every future to completion concurrently on the current task.
async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut pending: Vec<Option<Pin<Box<F>>>> =
        futures.into_iter().map(|f| Some(Box::pin(f))).collect();
    let mut outputs: Vec<Option<F::Output>> = (0..pending.len()).map(|_| None).collect();
    std::future::poll_fn(|cx: &mut TaskContext<'_>| {
        let mut all_done = true;
        for (slot, output) in pending.iter_mut().zip(outputs.iter_mut()) {
            if let Some(future) = slot.as_mut() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(value) => {
                        *output = Some(value);
                        *slot = None;
                    }
                    Poll::Pending => all_done = false,
                }
            }
        }
        if all_done {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    outputs
        .into_iter()
        .map(|o| o.expect("joined future produced output"))
        .collect()
}

/// Which path [`run_discovery`] drives.
pub enum DiscoveryDriver<'a> {
    /// The per-tier fan-out through a (scripted) invoker: one
    /// [`discover_tier`] call per tier, concurrent and best-effort.
    PerTier(&'a dyn AutopilotInvoker),
    /// The single-session tier-subagent path through an injected runner.
    SingleSession(&'a dyn DiscoverySessionRunner),
    /// Production: the agents-backed single-session runner built from
    /// these options.
    Production(SingleSessionOptions),
}

/// Run discovery and return all 11 verdicts, in tier order.
///
/// One observable contract (exactly one verdict per tier, 1-11) over the
/// paths of [`DiscoveryDriver`]: the per-tier fan-out through a scripted
/// invoker, or the single-session tier-subagent discovery
/// ([`run_single_session_discovery`]) -- ONE agent session whose
/// `agents` option registers all 11 tiers, collapsing the old 11-session
/// fan-out into one process and one MCP boot (spec 00059).
///
/// Every path is best-effort per tier: a tier whose agent fails or returns
/// malformed output still yields a (not-relevant, error-reason) verdict. The
/// single-session paths fail as a whole only when the one session itself
/// cannot run (its deadline fires or it cannot start).
pub async fn run_discovery(
    repo_root: &Path,
    driver: DiscoveryDriver<'_>,
) -> Result<Vec<TierVerdict>, InvokeError> {
    match driver {
        DiscoveryDriver::PerTier(invoker) => Ok(join_all(
            Tier::ALL
                .iter()
                .map(|tier| discover_tier(*tier, repo_root, invoker))
                .collect(),
        )
        .await),
        DiscoveryDriver::SingleSession(runner) => {
            run_single_session_discovery(repo_root, Some(runner), &SingleSessionOptions::default())
                .await
        }
        DiscoveryDriver::Production(options) => {
            run_single_session_discovery(repo_root, None, &options).await
        }
    }
}

// --- Single-session discovery: tier subagents in one session (spec 00059) -------------
//
// The per-tier session fan-out above opens one agent process (and one MCP
// boot) per tier. Single-session discovery collapses that to ONE session
// whose `agents` option registers all 11 tiers as subagents; the parent
// dispatches each via the built-in `Agent` (a.k.a. `Task`) tool, and every
// verdict is read back from the subagent's tool-RESULT block in the streamed
// messages -- never from the parent's prose, which the agent may
// re-summarize (D-2). A tier whose result is missing, errors, or fails to
// parse still yields a (not relevant, error-reason) verdict, so the run
// always returns 11.

/// The built-in subagent-dispatch tool. `Task` was renamed `Agent` in
/// Claude Code v2.1.63; both names appear in the stream, so both are
/// accepted on read and both are carried in `allowed_tools` on dispatch.
pub const SUBAGENT_TOOL_NAMES: [&str; 2] = ["Agent", "Task"];

/// Read-only repo tools handed to each tier subagent: enough to judge
/// relevance by reading the repo, nothing that can mutate it or drive a
/// browser.
pub const SUBAGENT_READONLY_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

/// A tier subagent is a relevance triage, not a deep task: a low effort and
/// a tight turn budget cap its per-tier cost (D-3).
pub const DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS: u32 = 8;
pub const DEFAULT_DISCOVERY_SUBAGENT_EFFORT: DiscoveryEffort = DiscoveryEffort::Low;

/// The agent every single-session discovery runs on: subagent definitions
/// are a claude-code capability with no cross-agent equivalent
/// (docs/agent-harness.md section 15.2).
pub const SUBAGENT_SESSION_AGENT_ID: &str = "claude-code";

/// The subagent `effort` levels the agent accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl DiscoveryEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            DiscoveryEffort::Low => "low",
            DiscoveryEffort::Medium => "medium",
            DiscoveryEffort::High => "high",
            DiscoveryEffort::Xhigh => "xhigh",
            DiscoveryEffort::Max => "max",
        }
    }
}

/// One tier-relevance subagent definition, in the agent's `--agents`
/// JSON shape (camelCase keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TierAgentDefinition {
    pub description: String,
    pub prompt: String,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub max_turns: u32,
    pub effort: DiscoveryEffort,
    pub permission_mode: String,
}

/// The `agents` key (and `subagent_type`) for one tier.
pub fn tier_agent_key(tier: Tier) -> String {
    format!("tier-{}", tier.value())
}

/// Map a dispatched `subagent_type` back to its [`Tier`].
///
/// Returns `None` for any value that is not one of the 11 `tier-N` keys, so
/// a stray tool call the parent makes (or a malformed dispatch) is ignored
/// rather than mis-attributed to a tier.
fn tier_from_agent_key(key: Option<&Value>) -> Option<Tier> {
    let key = key?.as_str()?;
    let value = key.strip_prefix("tier-")?.parse::<i64>().ok()?;
    Tier::from_value(value)
}

/// Build the 11 tier-relevance subagent definitions, keyed by tier.
///
/// Each subagent carries the tier's relevance-and-findings remit as its
/// system prompt, read-only repo tools, no MCP servers (no redundant
/// `playwright`/`proto` boots, D-3), and a cheap budget.
pub fn build_tier_agents(
    repo_root: &Path,
    model: Option<&str>,
    max_turns: u32,
    effort: DiscoveryEffort,
) -> BTreeMap<String, TierAgentDefinition> {
    Tier::ALL
        .iter()
        .map(|tier| {
            (
                tier_agent_key(*tier),
                TierAgentDefinition {
                    description: format!(
                        "Autopilot relevance triage for tier {} ({}).",
                        tier.value(),
                        tier.name()
                    ),
                    prompt: tier_prompt(*tier, repo_root),
                    tools: SUBAGENT_READONLY_TOOLS
                        .iter()
                        .map(|t| (*t).to_owned())
                        .collect(),
                    mcp_servers: Vec::new(),
                    model: model.map(str::to_owned),
                    max_turns,
                    effort,
                    permission_mode: "bypassPermissions".to_owned(),
                },
            )
        })
        .collect()
}

/// Build the deterministic dispatcher prompt for the parent session.
///
/// The parent is a dispatcher, not a judge: it must fan out every one of
/// the 11 tier subagents exactly once via the `Agent` tool and must NOT
/// pre-judge, skip, or merge any tier. It also emits an aggregated final
/// JSON purely as the fallback the collector reads only when it cannot key
/// a tool-result to a tier (spec 00059 D-2 supersession); the per-tier
/// tool-results remain the trusted source.
pub fn orchestrator_prompt(repo_root: &Path) -> String {
    let keys = Tier::ALL
        .iter()
        .map(|tier| tier_agent_key(*tier))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are autopilot's discovery dispatcher for the repository at {}.\n\n\
         Eleven tier-relevance subagents are registered: {keys}.\n\n\
         Use the Agent tool (a.k.a. Task) to dispatch EVERY one of these 11 subagents \
         exactly once, each with subagent_type set to its key. Tell each subagent to \
         read the repository, judge whether its tier is relevant to THIS codebase, and \
         return its verdict as the fenced JSON block its own instructions define. \
         Dispatch all 11 regardless of your own opinion -- do not skip, merge, or \
         pre-judge any tier. The per-tier tool-results are read directly, so you need \
         not restate them.\n\n\
         After all 11 subagents return, emit exactly one final fenced JSON block \
         aggregating every verdict as a safety net:\n\
         ```json\n\
         {{\"verdicts\": [{{\"tier\": 1, \"relevant\": false, \"reason\": \"...\", \
         \"findings\": []}}]}}\n\
         ```",
        repo_root.display(),
    )
}

/// How [`build_single_session_runner`] configures the one discovery
/// session and its 11 tier subagents.
#[derive(Debug, Clone, PartialEq)]
pub struct SingleSessionOptions {
    pub model: Option<String>,
    pub max_turns: u32,
    pub subagent_max_turns: u32,
    pub subagent_effort: DiscoveryEffort,
    /// Wall-clock ceiling for the whole session; `None` opts out.
    pub deadline_seconds: Option<f64>,
    /// Test-only escape hatch substituting the agent executable.
    pub command_override: Option<Vec<String>>,
}

impl Default for SingleSessionOptions {
    fn default() -> Self {
        Self {
            model: None,
            max_turns: DEFAULT_DISCOVERY_MAX_TURNS,
            subagent_max_turns: DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS,
            subagent_effort: DEFAULT_DISCOVERY_SUBAGENT_EFFORT,
            deadline_seconds: Some(DEFAULT_AUTOPILOT_AGENT_SECONDS),
            command_override: None,
        }
    }
}

/// The parent's tool allowlist: the dispatch tool (both names) plus the
/// read-only repo tools.
pub fn single_session_allowed_tools() -> Vec<String> {
    let mut dispatch: Vec<&str> = SUBAGENT_TOOL_NAMES.to_vec();
    dispatch.sort_unstable();
    dispatch
        .into_iter()
        .chain(SUBAGENT_READONLY_TOOLS)
        .map(str::to_owned)
        .collect()
}

/// Build the ONE session's agent configuration with all 11 tiers as
/// subagents.
///
/// The parent carries the `Agent`/`Task` dispatch tool plus the read-only
/// repo tools in `allowed_tools`; the 11 tier subagents ride the `agents`
/// adapter option verbatim onto the claude-code CLI (`--agents`).
pub fn build_single_session_configuration(
    repo_root: &Path,
    options: &SingleSessionOptions,
) -> AgentConfiguration {
    let mut configuration = AgentConfiguration::new(SUBAGENT_SESSION_AGENT_ID);
    configuration.model_id = options.model.clone();
    configuration.max_turns = Some(options.max_turns);
    configuration.permission_policy = PermissionPolicy::Auto;
    configuration.command_override = options.command_override.clone();
    let agents = build_tier_agents(
        repo_root,
        options.model.as_deref(),
        options.subagent_max_turns,
        options.subagent_effort,
    );
    configuration.adapter_options.insert(
        "agents".into(),
        serde_json::to_value(&agents).expect("tier agent definitions serialize"),
    );
    configuration.adapter_options.insert(
        "allowed_tools".into(),
        json!(single_session_allowed_tools()),
    );
    configuration
}

/// The shipped [`DiscoverySessionRunner`]: one claude-code session rooted
/// in the repo whose `agents` option registers the 11 tier subagents,
/// drained to an [`IterationResult`] (tool interactions and all) under the
/// autopilot wall-clock ceiling.
pub struct AgentsSingleSessionRunner {
    runtime: Arc<AgentRuntime>,
    host: Option<Arc<dyn ExecutionHost>>,
    configuration: AgentConfiguration,
    repo_root: PathBuf,
    deadline_seconds: Option<f64>,
}

impl fmt::Debug for AgentsSingleSessionRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentsSingleSessionRunner")
            .field("agent_id", &self.configuration.agent_id)
            .field("model_id", &self.configuration.model_id)
            .field("max_turns", &self.configuration.max_turns)
            .field("repo_root", &self.repo_root)
            .field("deadline_seconds", &self.deadline_seconds)
            .finish_non_exhaustive()
    }
}

impl AgentsSingleSessionRunner {
    pub fn with_runtime(mut self, runtime: Arc<AgentRuntime>) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn with_host(mut self, host: Arc<dyn ExecutionHost>) -> Self {
        self.host = Some(host);
        self
    }

    pub fn configuration(&self) -> &AgentConfiguration {
        &self.configuration
    }

    pub fn deadline_seconds(&self) -> Option<f64> {
        self.deadline_seconds
    }
}

impl DiscoverySessionRunner for AgentsSingleSessionRunner {
    fn run<'a>(&'a self, prompt: &'a str) -> SessionFuture<'a> {
        Box::pin(async move {
            let request = RunRequest {
                prompt: prompt.to_owned(),
                working_directory: self.repo_root.clone(),
                configuration: self.configuration.clone(),
                timeout_seconds: None,
            };
            let options = RunOptions {
                sink: None,
                host: self.host.clone(),
                approval_resolver: None,
            };
            let completed =
                bounded(self.runtime.run(request, options), self.deadline_seconds).await??;
            Ok(completed_run_to_iteration_result(&completed, Vec::new()))
        })
    }
}

/// Build the production single-session runner rooted at `repo_root`.
///
/// The returned runner drives one agent session (one claude-code process,
/// one MCP boot) whose configuration registers the 11 tier subagents, and
/// returns the drained [`IterationResult`] so the collector can read each
/// verdict from its subagent tool-result block. `options.deadline_seconds`
/// bounds the whole session (spec 00066 criterion #6).
pub fn build_single_session_runner(
    repo_root: impl Into<PathBuf>,
    options: &SingleSessionOptions,
) -> AgentsSingleSessionRunner {
    let repo_root = repo_root.into();
    AgentsSingleSessionRunner {
        runtime: Arc::new(AgentRuntime::new()),
        host: None,
        configuration: build_single_session_configuration(&repo_root, options),
        repo_root,
        deadline_seconds: options.deadline_seconds,
    }
}

/// Extract the subagent's response text from its tool-result block.
///
/// A subagent returns its final message as the `Agent` tool result, which
/// the stream surfaces either as a plain string or as a list of
/// content-block objects (`{"type": "text", "text": ...}`). Returns `None`
/// when the observation is absent, errored, or carries no text, so the
/// caller records a missing-result verdict rather than parsing an empty
/// string.
fn tool_result_text(observation: Option<&ToolResultObservation>) -> Option<String> {
    let observation = observation?;
    if observation.is_error == Some(true) {
        return None;
    }
    match observation.content.as_ref()? {
        ToolResultContent::Text(text) => (!text.is_empty()).then(|| text.clone()),
        ToolResultContent::Blocks(blocks) => {
            let text: String = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then_some(text)
        }
    }
}

/// Parse the orchestrator's aggregated final JSON into per-tier verdicts.
///
/// The D-2 fallback: when a subagent tool-result cannot be keyed to its
/// tier, the parent's final `{"verdicts": [...]}` block is parsed once.
/// Returns the verdicts it could read, keyed by tier; an absent or malformed
/// block yields an empty mapping so the caller falls through to the error
/// verdict.
fn parse_aggregated_verdicts(text: &str) -> BTreeMap<Tier, TierVerdict> {
    let mut out = BTreeMap::new();
    let Ok(data) = extract_json_object(text) else {
        return out;
    };
    let Some(Value::Array(entries)) = data.get("verdicts") else {
        return out;
    };
    for entry in entries {
        let Value::Object(entry) = entry else {
            continue;
        };
        let Some(tier) = entry
            .get("tier")
            .and_then(Value::as_i64)
            .and_then(Tier::from_value)
        else {
            continue;
        };
        out.insert(tier, verdict_from_data(tier, entry));
    }
    out
}

/// Which path produced the 11 collected verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectSource {
    /// Every filled tier came from its subagent tool-result.
    ToolResult,
    /// The final-JSON fallback supplied every tier.
    Aggregated,
    /// Both paths contributed.
    Mixed,
    /// Nothing keyed and no aggregate parsed.
    ErrorFallback,
}

impl CollectSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            CollectSource::ToolResult => COLLECT_SOURCE_TOOL_RESULT,
            CollectSource::Aggregated => COLLECT_SOURCE_AGGREGATED,
            CollectSource::Mixed => COLLECT_SOURCE_MIXED,
            CollectSource::ErrorFallback => COLLECT_SOURCE_ERROR,
        }
    }
}

pub const COLLECT_SOURCE_TOOL_RESULT: &str = "tool_result";
pub const COLLECT_SOURCE_AGGREGATED: &str = "aggregated";
pub const COLLECT_SOURCE_MIXED: &str = "mixed";
pub const COLLECT_SOURCE_ERROR: &str = "error_fallback";

/// The 11 collected verdicts plus a record of which path produced them.
///
/// `verdicts` is exactly one [`TierVerdict`] per tier in tier order.
/// `source` records whether the trusted per-subagent tool-results, the
/// aggregated-final-JSON fallback, or a mix produced them (spec 00059:
/// "record which path was used").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryCollection {
    pub verdicts: Vec<TierVerdict>,
    pub source: CollectSource,
}

/// Collect 11 tier verdicts from one discovery session's drained result.
///
/// Reads each verdict from its subagent `Agent`/`Task` tool-RESULT block
/// (keyed to its tier by `subagent_type`), never from the parent's prose
/// (D-2). A tier with a present-but-unparseable/errored result yields a
/// (not relevant, error-reason) verdict; a tier with NO keyed tool-result
/// is filled from the parent's aggregated final JSON, and if that too is
/// absent it yields a (not relevant) error verdict. Always returns exactly
/// 11 verdicts in tier order.
pub fn collect_tier_verdicts(result: &IterationResult) -> DiscoveryCollection {
    let mut from_tool: BTreeMap<Tier, TierVerdict> = BTreeMap::new();
    for interaction in &result.signals.tool_interactions {
        if !SUBAGENT_TOOL_NAMES.contains(&interaction.tool_name.as_str()) {
            continue;
        }
        let Some(tier) = tier_from_agent_key(interaction.tool_input.get("subagent_type")) else {
            continue;
        };
        let verdict = match tool_result_text(interaction.result.as_ref()) {
            Some(text) => parse_tier_verdict(tier, &text),
            None => {
                TierVerdict::not_relevant(tier, "subagent tool-result missing, errored, or empty")
            }
        };
        from_tool.insert(tier, verdict);
    }

    let missing = Tier::ALL.iter().any(|tier| !from_tool.contains_key(tier));
    let mut aggregated = if missing {
        parse_aggregated_verdicts(&result.transcript)
    } else {
        BTreeMap::new()
    };

    let (mut used_tool, mut used_aggregated, mut used_error) = (false, false, false);
    let verdicts = Tier::ALL
        .iter()
        .map(|tier| {
            if let Some(verdict) = from_tool.remove(tier) {
                used_tool = true;
                verdict
            } else if let Some(verdict) = aggregated.remove(tier) {
                used_aggregated = true;
                verdict
            } else {
                used_error = true;
                TierVerdict::not_relevant(
                    *tier,
                    "no discovery result for this tier: subagent was not dispatched and no \
                     aggregated verdict was emitted",
                )
            }
        })
        .collect();

    DiscoveryCollection {
        verdicts,
        source: collect_source(used_tool, used_aggregated, used_error),
    }
}

/// Summarize which path(s) produced the collected verdicts.
fn collect_source(used_tool: bool, used_aggregated: bool, used_error: bool) -> CollectSource {
    if used_tool && !used_aggregated && !used_error {
        CollectSource::ToolResult
    } else if used_aggregated && !used_tool && !used_error {
        CollectSource::Aggregated
    } else if !used_tool && !used_aggregated {
        CollectSource::ErrorFallback
    } else {
        CollectSource::Mixed
    }
}

/// Run discovery as ONE session of tier subagents; return all 11 verdicts.
///
/// Drives a single session (`session_runner` when injected, else the
/// production runner from [`build_single_session_runner`] rooted at
/// `repo_root`) whose configuration registers the 11 tiers, then collects
/// each verdict from its subagent tool-result block. Returns exactly one
/// verdict per tier (1-11) in tier order -- a tier whose result is missing
/// or malformed still yields a (not relevant, error-reason) verdict. Fails
/// only when the session itself cannot run (deadline or start failure).
pub async fn run_single_session_discovery(
    repo_root: &Path,
    session_runner: Option<&dyn DiscoverySessionRunner>,
    options: &SingleSessionOptions,
) -> Result<Vec<TierVerdict>, InvokeError> {
    let prompt = orchestrator_prompt(repo_root);
    let result = match session_runner {
        Some(runner) => runner.run(&prompt).await?,
        None => {
            build_single_session_runner(repo_root, options)
                .run(&prompt)
                .await?
        }
    };
    Ok(collect_tier_verdicts(&result).verdicts)
}

// --- Authoring: compile a finding into grader-bearing tasks ---------------------------
//
// The integrity crux of the phase. Each selected finding is compiled --
// entirely by an agent following the fw-spec / fw-plan contracts headlessly
// -- into one or more validated flywheel tasks. The authoritative grader on
// every emitted task must be either a pre-existing committed repo check or
// a registered held-out oracle, never a check the same task's own diff
// creates (criterion #8: the "entirely by agents" path must not emit
// self-attestation wearing a grader's clothes). A finding that cannot be
// lowered to such a task is DROPPED with a recorded reason, never written
// as a goal-only or grader-less stub (criterion #1).

/// The authoritative grader points at a check already committed in the repo.
pub const GRADER_SOURCE_REPO_COMMAND: &str = "repo_command";
/// The authoritative grader is a registered held-out oracle (out of the
/// agent's reach), declared by an absolute path outside the agent's
/// worktree.
pub const GRADER_SOURCE_HELD_OUT: &str = "held_out_oracle";

/// Where an emitted task's authoritative grader comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraderSource {
    RepoCommand,
    HeldOutOracle,
}

impl GraderSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            GraderSource::RepoCommand => GRADER_SOURCE_REPO_COMMAND,
            GraderSource::HeldOutOracle => GRADER_SOURCE_HELD_OUT,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            GRADER_SOURCE_REPO_COMMAND => Some(GraderSource::RepoCommand),
            GRADER_SOURCE_HELD_OUT => Some(GraderSource::HeldOutOracle),
            _ => None,
        }
    }
}

/// Default turn budget for the agents-backed authoring invoker.
pub const DEFAULT_AUTHORING_MAX_TURNS: u32 = 120;

/// Commit-before-done discipline that `/fw-plan` bakes into hand-authored
/// tasks (fw-plan template). The autopilot authoring prompt does not, so
/// every emitted task gets it injected deterministically -- a task that
/// finishes with uncommitted work is verified green against a dirty tree
/// yet silently fails to land (the worker parks it). Phrasing mirrors the
/// fw-plan example constraint.
pub const COMMIT_BEFORE_DONE_CONSTRAINT: &str =
    "Commit the change with a clear message before reporting done";

/// One validated, grader-bearing task autopilot authored from a finding.
///
/// `task` is the compiled core [`Task`] (it loaded through the
/// authoritative validator and carries at least one grader).
/// `authoritative_grader` is the run string of the out-of-band command
/// grader the work lands on; `grader_source` records whether it is a
/// pre-existing repo check or a held-out oracle; `grader_target` is the
/// pre-existing check it resolves to. `creates_files` are the files the
/// task's own diff is expected to create -- recorded so the
/// self-attestation guard can prove the authoritative grader does not
/// target any of them. `assumptions` are the ambiguities the authoring
/// agent resolved itself (D-2), recorded with the emitted task rather than
/// edited into a spec.
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedTask {
    pub finding: Finding,
    pub task: Task,
    pub authoritative_grader: String,
    pub grader_source: GraderSource,
    pub grader_target: String,
    pub creates_files: Vec<String>,
    pub assumptions: Vec<String>,
    pub held_out_oracle_path: Option<PathBuf>,
    pub prerequisites: Vec<String>,
}

impl EmittedTask {
    /// An emitted task with no creates/assumptions/prerequisites.
    pub fn new(
        finding: Finding,
        task: Task,
        authoritative_grader: impl Into<String>,
        grader_source: GraderSource,
        grader_target: impl Into<String>,
    ) -> Self {
        Self {
            finding,
            task,
            authoritative_grader: authoritative_grader.into(),
            grader_source,
            grader_target: grader_target.into(),
            creates_files: Vec::new(),
            assumptions: Vec::new(),
            held_out_oracle_path: None,
            prerequisites: Vec::new(),
        }
    }
}

/// A finding autopilot could not compile to a grader-bearing task.
///
/// Recorded with a `reason` instead of emitting a vaguer or grader-less
/// task (criterion #1 / D-2). The loop persists the reason so a clean repo
/// or an ungradeable finding is auditable rather than silently
/// filler-filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedFinding {
    pub finding: Finding,
    pub reason: String,
}

/// The outcome of compiling one or more findings: emitted tasks + drops.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AuthoringResult {
    pub emitted: Vec<EmittedTask>,
    pub dropped: Vec<DroppedFinding>,
}

impl AuthoringResult {
    fn dropped(finding: &Finding, reason: impl Into<String>) -> Self {
        Self {
            emitted: Vec::new(),
            dropped: vec![DroppedFinding {
                finding: finding.clone(),
                reason: reason.into(),
            }],
        }
    }
}

/// Build the headless authoring prompt for one selected finding.
///
/// Carries the load-bearing integrity rule: the authoritative grader must
/// be a pre-existing committed check (or a held-out oracle), never a check
/// this task's own diff creates. Ambiguity is resolved as a recorded
/// assumption, not by emitting a vaguer task (D-2).
pub fn authoring_prompt(finding: &Finding, repo_root: &Path) -> String {
    let evidence = if finding.evidence.is_empty() {
        "  (none)".to_owned()
    } else {
        finding
            .evidence
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "You are autopilot's headless authoring agent. Compile the finding below into \
         one or more flywheel tasks following the fw-spec and fw-plan contracts -- a \
         one-sentence goal and the strongest out-of-band grader you can express.\n\n\
         Repository: {}\n\
         Finding (Tier {}, {}): {}\n\
         Detail: {}\n\
         Evidence:\n{evidence}\n\n\
         HARD INTEGRITY RULES:\n\
         - Every emitted task must carry at least one grader.\n\
         - The authoritative grader MUST be either (a) a command that invokes a check \
         already committed in this repo (a test/lint/build command that exists BEFORE \
         this task runs), or (b) a registered held-out oracle by absolute path. It must \
         NEVER be a brand-new check this task's own diff creates -- that is \
         self-attestation.\n\
         - Resolve any ambiguity a human would be asked about as a recorded assumption; \
         never emit a vaguer task to dodge a question.\n\
         - If you cannot express an out-of-band grader for this finding, do not invent \
         one: set \"dropped\" to a one-sentence reason and emit no task.\n\n\
         Respond with exactly one fenced JSON block:\n\
         ```json\n\
         {{\n\
         \x20 \"tasks\": [\n\
         \x20   {{\n\
         \x20     \"task\": {{\"id\": \"kebab-id\", \"goal\": \"one sentence\", \
         \"graders\": [{{\"type\": \"command\", \"run\": \"<existing repo check>\"}}], \
         \"tags\": [], \"context\": {{}}}},\n\
         \x20     \"authoritative_grader\": \"<run string matching a command grader \
         above>\",\n\
         \x20     \"grader_source\": \"{GRADER_SOURCE_REPO_COMMAND}\",\n\
         \x20     \"grader_target\": \"<repo-relative path to the pre-existing check \
         the grader runs>\",\n\
         \x20     \"creates_files\": [\"<paths this task's diff will create>\"],\n\
         \x20     \"assumptions\": [\"<ambiguity you resolved yourself>\"]\n\
         \x20   }}\n\
         \x20 ],\n\
         \x20 \"dropped\": \"\"\n\
         }}\n\
         ```",
        repo_root.display(),
        finding.tier.value(),
        finding.tier.name(),
        finding.title,
        finding.detail,
    )
}

/// Inject the commit-before-done constraint on an authored task (in place).
///
/// Idempotent: a no-op when the task already carries a constraint
/// mentioning a commit, so an agent that authored its own commit discipline
/// is not duplicated. Constraints are advisory prompt text (the agent sees
/// them under `## Constraints`); the hard enforcement is the
/// landable-change gate.
fn ensure_commit_before_done(task: &mut Task) {
    if task
        .context
        .constraints
        .iter()
        .any(|c| c.to_lowercase().contains("commit"))
    {
        return;
    }
    task.context
        .constraints
        .push(COMMIT_BEFORE_DONE_CONSTRAINT.to_owned());
}

/// The string entries of a JSON array (non-strings skipped); empty when
/// the value is not an array.
fn string_entries(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Validate one authored-task entry into an [`EmittedTask`] or a drop
/// reason.
///
/// The checks are the deterministic enforcement of criteria #1 and #8 -- a
/// self-authored or grader-less entry is dropped, never emitted.
fn validate_task_entry(
    finding: &Finding,
    entry: &Value,
    repo_root: &Path,
) -> Result<EmittedTask, String> {
    let Value::Object(entry) = entry else {
        return Err("authored entry is not an object".to_owned());
    };
    let Some(task_data @ Value::Object(task_map)) = entry.get("task") else {
        return Err("authored entry has no task object".to_owned());
    };
    let mut task = load_task_data(task_data, &format!("autopilot:{}", finding.id))
        .map_err(|err| format!("authored task failed validation: {err}"))?;
    if task.graders.is_empty() {
        return Err("authored task carries no grader (criterion #1)".to_owned());
    }
    ensure_commit_before_done(&mut task);

    let Some(authoritative) = non_blank_str(entry, "authoritative_grader").map(str::to_owned)
    else {
        return Err("authored task names no authoritative grader".to_owned());
    };
    let is_command_run = task.graders.iter().any(|grader| match grader {
        Grader::Command(command) => command.run == authoritative,
        _ => false,
    });
    if !is_command_run {
        return Err("authoritative grader is not a command grader on the emitted task".to_owned());
    }

    let source = match entry.get("grader_source") {
        Some(Value::String(s)) => GraderSource::parse(s)
            .ok_or_else(|| format!("unknown grader_source {}", py_repr_str(s)))?,
        other => {
            return Err(format!(
                "unknown grader_source {}",
                other
                    .map(Value::to_string)
                    .unwrap_or_else(|| "None".to_owned())
            ));
        }
    };
    let Some(target) = non_blank_str(entry, "grader_target").map(str::to_owned) else {
        return Err("authoritative grader names no pre-existing target".to_owned());
    };

    let creates = string_entries(entry.get("creates_files"));

    // Self-attestation guard (criterion #8): the authoritative grader must
    // not name -- nor resolve to -- a file this task's own diff creates.
    for created in &creates {
        if created.is_empty() {
            continue;
        }
        if authoritative.contains(created.as_str()) {
            return Err(
                "authoritative grader names a file this task's own diff creates \
                 (self-attestation)"
                    .to_owned(),
            );
        }
        if *created == target {
            return Err(
                "authoritative grader target is a file this task's own diff creates \
                 (self-attestation)"
                    .to_owned(),
            );
        }
    }

    let held_out_oracle_path = match source {
        GraderSource::RepoCommand => {
            // The grader must point at a check that pre-exists in the repo
            // -- proof it is out-of-band, not created by the run it grades.
            if !repo_root.join(&target).exists() {
                return Err(
                    "authoritative grader target does not pre-exist in the repo (not an \
                     out-of-band check)"
                        .to_owned(),
                );
            }
            None
        }
        GraderSource::HeldOutOracle => {
            let oracle = PathBuf::from(&target);
            if !oracle.is_absolute() || !oracle.exists() {
                return Err("held-out oracle target is not an existing absolute path".to_owned());
            }
            Some(oracle)
        }
    };

    let assumptions = string_entries(entry.get("assumptions"));

    // `prerequisites` is an orchestration-layer edge core drops on load, so
    // read it straight from the authored task source (mirroring the
    // directory work source) and carry it on the emitted task for emission
    // to preserve.
    let prerequisites = string_entries(task_map.get("prerequisites"));

    Ok(EmittedTask {
        finding: finding.clone(),
        task,
        authoritative_grader: authoritative,
        grader_source: source,
        grader_target: target,
        creates_files: creates,
        assumptions,
        held_out_oracle_path,
        prerequisites,
    })
}

/// Python `repr()` of a string: single-quoted unless it contains a single
/// quote and no double quote.
fn py_repr_str(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', "\\'"))
    }
}

/// Compile one finding into emitted tasks (or a recorded drop).
///
/// Drives the authoring agent through the injectable seam, parses its
/// structured output, and validates every authored task against criteria
/// #1 and #8. A finding whose agent call fails, whose response is
/// unparseable, or that yields no valid grader-bearing task is dropped with
/// a recorded reason rather than written.
pub async fn author_finding(
    finding: &Finding,
    repo_root: &Path,
    invoker: &dyn AutopilotInvoker,
) -> AuthoringResult {
    let prompt = authoring_prompt(finding, repo_root);
    let text = match invoker.invoke(&prompt).await {
        Ok(text) => text,
        Err(err) => {
            return AuthoringResult::dropped(finding, format!("authoring agent error: {err}"));
        }
    };
    let data = match extract_json_object(&text) {
        Ok(data) => data,
        Err(err) => {
            return AuthoringResult::dropped(
                finding,
                format!("unparseable authoring response: {err}"),
            );
        }
    };

    let entries: &[Value] = match data.get("tasks") {
        Some(Value::Array(entries)) => entries,
        _ => &[],
    };
    let mut emitted = Vec::new();
    let mut drop_reasons = Vec::new();
    for entry in entries {
        match validate_task_entry(finding, entry, repo_root) {
            Ok(task) => emitted.push(task),
            Err(reason) => drop_reasons.push(reason),
        }
    }

    if !emitted.is_empty() {
        return AuthoringResult {
            emitted,
            dropped: Vec::new(),
        };
    }

    // No valid task: prefer the agent's own drop reason, else the first
    // validation failure, else a generic note.
    let reason = trimmed_str(&data, "dropped")
        .or_else(|| drop_reasons.into_iter().next())
        .unwrap_or_else(|| "no grader-bearing task authored".to_owned());
    AuthoringResult::dropped(finding, reason)
}

/// Author every finding in order; aggregate emitted tasks and drops.
pub async fn author_findings(
    findings: &[Finding],
    repo_root: &Path,
    invoker: &dyn AutopilotInvoker,
) -> AuthoringResult {
    let mut result = AuthoringResult::default();
    for finding in findings {
        let one = author_finding(finding, repo_root, invoker).await;
        result.emitted.extend(one.emitted);
        result.dropped.extend(one.dropped);
    }
    result
}

// --- The single refill pass: compose discovery -> score -> author -> emit -------------
//
// One pass that fills the work source up to the target queue depth from
// actionable findings. Structured as a function the daemon invokes in a
// loop -- no infinite loop here. On a clean repo (no actionable finding) it
// writes zero tasks and returns cleanly (D-5): the single pass under
// `--once` exits 0.

/// Default target queue depth when the `[autopilot]` table omits one.
pub const DEFAULT_TARGET_DEPTH: i64 = DEFAULT_AUTOPILOT_TARGET_DEPTH;
/// Default landing posture (FF-merge autonomy is the shipped default, D-3).
pub const DEFAULT_LANDING: &str = DEFAULT_AUTOPILOT_LANDING;
/// The phase directory autopilot emits authored tasks into.
pub const AUTOPILOT_PHASE: &str = "autopilot";

/// The recorded outcome of one refill pass, for logs and the daemon.
///
/// `emitted_paths` are the task files written into the work source this
/// pass; `emitted` the corresponding authored tasks; `dropped` the findings
/// that could not be compiled. `relevant_tiers` / `not_relevant_tiers`
/// record the per-tier relevance verdicts so a clean repo is auditable.
/// `reason` summarizes why the pass emitted what it did.
#[derive(Debug, Clone, PartialEq)]
pub struct AutopilotPassResult {
    pub emitted_paths: Vec<PathBuf>,
    pub emitted: Vec<EmittedTask>,
    pub dropped: Vec<DroppedFinding>,
    pub queue_depth_before: usize,
    pub target_depth: i64,
    pub relevant_tiers: Vec<Tier>,
    pub not_relevant_tiers: Vec<Tier>,
    pub landing: String,
    pub reason: String,
}

impl Default for AutopilotPassResult {
    fn default() -> Self {
        Self {
            emitted_paths: Vec::new(),
            emitted: Vec::new(),
            dropped: Vec::new(),
            queue_depth_before: 0,
            target_depth: 0,
            relevant_tiers: Vec::new(),
            not_relevant_tiers: Vec::new(),
            landing: DEFAULT_LANDING.to_owned(),
            reason: String::new(),
        }
    }
}

impl AutopilotPassResult {
    pub fn emitted_count(&self) -> usize {
        self.emitted_paths.len()
    }
}

/// Claim-time conflict keys for an emitted task: the repo source files it
/// contends for, so two autopilot tasks scoped to the same file serialize
/// at claim time instead of racing -- the dedupe-swarm failure mode (spec
/// 00061 Gap 3, spec 00064 P2).
///
/// Derived deterministically from `grader_target` (the file the task's
/// authoritative check resolves to) and `creates_files`, never from an
/// agent-reported value. Coarse targets (build manifests, directories) are
/// dropped so unrelated work that merely shares a manifest is not
/// over-serialized. Empty when neither yields a specific source file.
pub fn conflict_keys_for(emitted: &EmittedTask) -> Vec<String> {
    let mut keys = BTreeSet::new();
    if let Some(key) = conflict_key_for_path(&emitted.grader_target) {
        keys.insert(key);
    }
    for created in &emitted.creates_files {
        if let Some(key) = conflict_key_for_path(created) {
            keys.insert(key);
        }
    }
    keys.into_iter().collect()
}

/// Serialize an emitted task to the directory work source's file shape.
///
/// The body is the core task ([`serialize_task`]) plus the
/// orchestration-layer keys the directory source reads from top-level JSON:
/// `priority` (derived from the recorded final score so the scheduler
/// orders autopilot work), `prerequisites`, and `conflict_keys` (the source
/// files the task contends for -- omitted when empty). The full
/// [`ScoreBreakdown`] is recorded under an `autopilot` key (ignored by the
/// core loader) so the recommendation stays inspectable after the run --
/// the legible-score requirement (#3) persisted.
pub fn emitted_task_file(emitted: &EmittedTask, breakdown: &ScoreBreakdown) -> Value {
    let mut body = match serialize_task(&emitted.task) {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("task".into(), other);
            map
        }
    };
    body.insert(
        "priority".into(),
        json!(breakdown.final_score.round() as i64),
    );
    if !emitted.prerequisites.is_empty() {
        body.insert("prerequisites".into(), json!(emitted.prerequisites));
    }
    let conflict_keys = conflict_keys_for(emitted);
    if !conflict_keys.is_empty() {
        body.insert("conflict_keys".into(), json!(conflict_keys));
    }
    body.insert(
        "autopilot".into(),
        json!({
            "finding_id": emitted.finding.id,
            "tier": breakdown.tier.value(),
            "tier_weight": breakdown.tier_weight,
            "urgency": breakdown.urgency,
            "importance": breakdown.importance,
            "blocks": breakdown.blocks,
            "effort": breakdown.effort,
            "final": breakdown.final_score,
            "preemptive": breakdown.preemptive,
            "authoritative_grader": emitted.authoritative_grader,
            "grader_source": emitted.grader_source.as_str(),
            "grader_target": emitted.grader_target,
            "assumptions": emitted.assumptions,
        }),
    );
    Value::Object(body)
}

/// Write one emitted task into the directory work source; return its path.
///
/// Lands the file under `<tasks_dir>/active/<phase>/<task_id>.json` so the
/// existing worker drains it (the directory adapter lists it on its next
/// pass). Returns `Ok(None)` without overwriting when a file for that task
/// id already exists, so re-running autopilot does not duplicate or clobber
/// in-flight work.
pub fn emit_emitted_task(
    emitted: &EmittedTask,
    breakdown: &ScoreBreakdown,
    tasks_dir: &Path,
    phase: &str,
) -> std::io::Result<Option<PathBuf>> {
    let phase_dir = tasks_dir.join("active").join(phase);
    std::fs::create_dir_all(&phase_dir)?;
    let target = phase_dir.join(format!("{}.json", emitted.task.id));
    if target.exists() {
        return Ok(None);
    }
    let body = serde_json::to_string_pretty(&emitted_task_file(emitted, breakdown))
        .map_err(std::io::Error::other)?;
    std::fs::write(&target, format!("{body}\n"))?;
    Ok(Some(target))
}

/// A queue-depth probe: the current work-source depth for `tasks_dir`.
pub type QueueDepthFn<'a> = dyn Fn(&Path) -> Result<usize, WorkSourceError> + Send + Sync + 'a;

/// A clock the no-op-cycle ledger stamps `occurred_at` from.
pub type ClockFn<'a> = dyn Fn() -> DateTime<Utc> + Send + Sync + 'a;

/// Everything one refill pass needs. The agent seams default to the
/// agents-backed invokers rooted at `repo_root`; tests inject scripted
/// ones.
pub struct RefillPassOptions<'a> {
    pub tasks_dir: PathBuf,
    pub repo_root: PathBuf,
    pub target_depth: i64,
    /// A scripted per-tier discovery invoker; `None` drives the
    /// single-session tier-subagent path.
    pub discovery_invoker: Option<&'a dyn AutopilotInvoker>,
    /// An injected single-session runner for the production discovery
    /// route; `None` builds the agents-backed runner.
    pub discovery_session_runner: Option<&'a dyn DiscoverySessionRunner>,
    /// A scripted authoring invoker; `None` builds the agents-backed one.
    pub authoring_invoker: Option<&'a dyn AutopilotInvoker>,
    pub weights: ScoreWeights,
    pub landing: String,
    pub model: Option<String>,
    /// Measures the current work-source depth; defaults to the directory
    /// adapter's listing count ([`directory_queue_depth`]).
    pub queue_depth: Option<&'a QueueDepthFn<'a>>,
    /// The resolved `[deadlines]`; `None` keeps the built-in ceilings.
    pub deadlines: Option<DeadlineConfig>,
    /// The append-only stop ledger a no-op cycle is witnessed on.
    pub control: Option<&'a dyn ClaimStore>,
    pub now: Option<&'a ClockFn<'a>>,
    /// The policy's `[agent]` opt-in, forwarded to the default invoker
    /// constructions (an injected invoker is unaffected).
    pub agent_id: Option<String>,
    pub agent_transport: Option<String>,
}

impl<'a> RefillPassOptions<'a> {
    /// Defaults for `tasks_dir`/`repo_root`: target depth
    /// [`DEFAULT_TARGET_DEPTH`], landing [`DEFAULT_LANDING`], shipped
    /// weights, every seam at its production default.
    pub fn new(tasks_dir: impl Into<PathBuf>, repo_root: impl Into<PathBuf>) -> Self {
        Self {
            tasks_dir: tasks_dir.into(),
            repo_root: repo_root.into(),
            target_depth: DEFAULT_TARGET_DEPTH,
            discovery_invoker: None,
            discovery_session_runner: None,
            authoring_invoker: None,
            weights: ScoreWeights::default(),
            landing: DEFAULT_LANDING.to_owned(),
            model: None,
            queue_depth: None,
            deadlines: None,
            control: None,
            now: None,
            agent_id: None,
            agent_transport: None,
        }
    }
}

/// A refill pass could not complete.
#[derive(Debug, thiserror::Error)]
pub enum RefillError {
    /// The queue-depth probe failed.
    #[error("queue depth: {0}")]
    QueueDepth(#[from] WorkSourceError),
    /// The single discovery session itself could not run.
    #[error("discovery: {0}")]
    Discovery(#[from] InvokeError),
    /// Writing an emitted task file failed.
    #[error("emit {}: {source}", path.display())]
    Emit {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Witnessing the no-op cycle on the stop ledger failed.
    #[error("stop ledger: {0}")]
    StopLedger(#[from] ClaimStoreError),
}

/// Run one refill pass: discovery -> score/select -> author -> emit.
///
/// While the live queue depth is below `target_depth` and actionable
/// findings remain, emits authored tasks until the target is met or the
/// actionable findings are exhausted. When discovery yields nothing
/// actionable the pass writes ZERO tasks and returns cleanly -- never
/// emitting filler (D-5). It does not loop on an interval; the daemon calls
/// this once per cycle.
///
/// The discovery and authoring agent calls run under the policy's
/// [`DeadlineClass::AutopilotAgent`] ceiling (spec 00066 criterion #6):
/// resolved once and fed to both so a stalled stream is cancelled rather
/// than parking the daemon.
pub async fn run_refill_pass(
    options: RefillPassOptions<'_>,
) -> Result<AutopilotPassResult, RefillError> {
    let resolved_deadlines = options.deadlines.clone().unwrap_or_default();
    let autopilot_ceiling = resolved_deadlines.for_class(DeadlineClass::AutopilotAgent);

    let current = match options.queue_depth {
        Some(probe) => probe(&options.tasks_dir)?,
        None => directory_queue_depth(&options.tasks_dir)?,
    };
    let target_depth = options.target_depth;
    let slots = target_depth - i64::try_from(current).unwrap_or(i64::MAX);

    // Witness a no-op cycle on the append-only stop ledger (D-2): a cycle
    // that did discovery work yet emitted zero tasks. Audit-only -- the
    // pass still returns its zero-emit result unchanged. The subject is the
    // queue's tasks_dir; the detail names the reason plus the observed and
    // target queue depths. Never deduped: an idle cycle recurring across
    // daemon ticks is a recurring stop, and the recurrence is the signal.
    // The queue-already-at-target short-circuit below is deliberately NOT
    // recorded -- a full queue is healthy backpressure, not a dead-end.
    let record_no_op = |reason: &str| -> Result<(), ClaimStoreError> {
        let Some(control) = options.control else {
            return Ok(());
        };
        let occurred_at = options.now.map_or_else(Utc::now, |clock| clock());
        control.record_stop_event(
            STOP_NO_OP_CYCLE,
            &options.tasks_dir.to_string_lossy(),
            &format!("{reason} (observed queue depth {current}, target {target_depth})"),
            occurred_at,
            "",
        )
    };

    if slots <= 0 {
        return Ok(AutopilotPassResult {
            queue_depth_before: current,
            target_depth,
            landing: options.landing.clone(),
            reason: format!(
                "queue depth {current} already at or above target {target_depth}; nothing \
                 emitted"
            ),
            ..AutopilotPassResult::default()
        });
    }

    let default_author;
    let author: &dyn AutopilotInvoker = match options.authoring_invoker {
        Some(invoker) => invoker,
        None => {
            default_author = build_repo_invoker(
                &options.repo_root,
                RepoInvokerOptions {
                    model: options.model.clone(),
                    max_turns: DEFAULT_AUTHORING_MAX_TURNS,
                    deadline_seconds: autopilot_ceiling,
                    ..RepoInvokerOptions::from_policy(
                        options.agent_id.as_deref(),
                        options.agent_transport.as_deref(),
                    )
                },
            );
            &default_author
        }
    };

    // Discovery routes itself: a scripted `discovery_invoker` (tests)
    // drives the per-tier fan-out; `None` (production) drives the
    // single-session tier-subagent path. Either way `run_discovery`
    // returns exactly 11 verdicts.
    let driver = match (options.discovery_invoker, options.discovery_session_runner) {
        (Some(invoker), _) => DiscoveryDriver::PerTier(invoker),
        (None, Some(runner)) => DiscoveryDriver::SingleSession(runner),
        (None, None) => DiscoveryDriver::Production(SingleSessionOptions {
            model: options.model.clone(),
            deadline_seconds: autopilot_ceiling,
            ..SingleSessionOptions::default()
        }),
    };
    let verdicts = run_discovery(&options.repo_root, driver).await?;
    let relevant_tiers: Vec<Tier> = verdicts
        .iter()
        .filter(|v| v.relevant)
        .map(|v| v.tier)
        .collect();
    let not_relevant_tiers: Vec<Tier> = verdicts
        .iter()
        .filter(|v| !v.relevant)
        .map(|v| v.tier)
        .collect();
    let findings: Vec<Finding> = verdicts
        .iter()
        .filter(|v| v.relevant)
        .flat_map(|v| v.findings.iter().cloned())
        .collect();

    let sequenced = sequence_findings(&findings, &options.weights);
    if sequenced.is_empty() {
        let reason = "no actionable findings this cycle; idling without emitting";
        record_no_op(reason)?;
        return Ok(AutopilotPassResult {
            queue_depth_before: current,
            target_depth,
            relevant_tiers,
            not_relevant_tiers,
            landing: options.landing.clone(),
            reason: reason.to_owned(),
            ..AutopilotPassResult::default()
        });
    }

    let mut emitted = Vec::new();
    let mut emitted_paths: Vec<PathBuf> = Vec::new();
    let mut dropped = Vec::new();
    let slots = usize::try_from(slots).unwrap_or(usize::MAX);

    // Author the top-sequenced findings until the queue reaches the target
    // or the actionable findings run out; only enough to fill the
    // depth-to-target. Each emitted task carries the breakdown of the
    // finding it was authored from (never a lookup by finding id, which two
    // same-slug findings could collide on).
    for scored in &sequenced {
        if emitted_paths.len() >= slots {
            break;
        }
        let result = author_finding(&scored.finding, &options.repo_root, author).await;
        dropped.extend(result.dropped);
        for task in result.emitted {
            let path = emit_emitted_task(
                &task,
                &scored.breakdown,
                &options.tasks_dir,
                AUTOPILOT_PHASE,
            )
            .map_err(|source| RefillError::Emit {
                path: options
                    .tasks_dir
                    .join("active")
                    .join(AUTOPILOT_PHASE)
                    .join(format!("{}.json", task.task.id)),
                source,
            })?;
            if let Some(path) = path {
                emitted.push(task);
                emitted_paths.push(path);
            }
        }
    }

    let reason = if emitted_paths.is_empty() {
        let reason = "no grader-bearing task could be authored this cycle";
        record_no_op(reason)?;
        reason.to_owned()
    } else {
        format!(
            "emitted {} task(s) to fill queue from {current} toward target {target_depth}",
            emitted_paths.len()
        )
    };
    Ok(AutopilotPassResult {
        emitted_paths,
        emitted,
        dropped,
        queue_depth_before: current,
        target_depth,
        relevant_tiers,
        not_relevant_tiers,
        landing: options.landing,
        reason,
    })
}

/// Current work-source depth: the directory adapter's active-item count.
pub fn directory_queue_depth(tasks_dir: &Path) -> Result<usize, WorkSourceError> {
    Ok(DirectoryWorkSource::new(tasks_dir).list_work()?.len())
}

/// The lifecycle statuses that mean a task is finished work, not queued
/// work (the two states with no outgoing transition).
pub const TERMINAL_STATUSES: [Status; 2] = [Status::Done, Status::Failed];

/// Active task files whose task has no terminal lifecycle (spec 00062).
///
/// The refill decision needs *actionable* depth -- work the worker can
/// still drive -- not the raw active-file count [`directory_queue_depth`]
/// returns. A finished task's JSON lingers under `active/<phase>/` until
/// its WHOLE phase archives (archival is all-or-nothing per phase, gated on
/// every task reaching `DONE`), so counting terminal tasks pins depth at
/// target and suppresses refill; worse, a single terminally-`FAILED` task
/// never archives and would wedge intake permanently. Excluding
/// `DONE`/`FAILED` tasks decouples "is the queue full?" from "is the phase
/// fully archived?".
///
/// Resilient by construction: one store read lists every terminal
/// lifecycle; if it fails every task counts as actionable (conservative --
/// never under-reports depth) so a transient store hiccup can never crash a
/// daemon cycle or spuriously over-emit.
pub fn actionable_queue_depth(
    tasks_dir: &Path,
    store: &dyn LifecycleStore,
) -> Result<usize, WorkSourceError> {
    let active = load_active_tasks(tasks_dir)?;
    let terminal: BTreeSet<String> = store
        .list_lifecycles(&LifecycleFilter::default().with_statuses(TERMINAL_STATUSES))
        .map(|rows| rows.into_iter().map(|l| l.task_id).collect())
        .unwrap_or_default();
    Ok(active
        .iter()
        .filter(|(_, task)| !terminal.contains(&task.id))
        .count())
}
