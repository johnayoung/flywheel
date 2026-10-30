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
//! The scoring engine (the `scoring` submodule) is pure: no file I/O, no
//! agent calls, no subprocess, no clock. Default weights are
//! [`AutopilotWeights::default`](crate::policy::AutopilotWeights::default)
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

mod scoring;

pub use scoring::{
    Finding, INTERRUPT_BASE, PREEMPTIVE_MAX_TIER, ScoreBreakdown, ScoreWeights, ScoredFinding,
    Tier, W_EFFORT, W_IMPORTANCE, W_TIER, W_UNBLOCK, W_URGENCY, default_weights, recompute_final,
    score_finding, select_findings, sequence_findings, tier_description, tier_weight,
};

use crate::autopilot_context::{
    DiscoveryCache, DiscoveryCacheEntry, DiscoveryCacheStatus, RepoInventory, RepoState,
    TierPrefilterSkip, build_repo_digest, prompt_with_digest, sha256_hex, structural_prefilter,
};
use crate::claims::{ClaimStore, ClaimStoreError, STOP_NO_OP_CYCLE};
use crate::policy::{
    DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY, DEFAULT_AUTOPILOT_DISCOVERY_CACHE_TTL_SECONDS,
    DEFAULT_AUTOPILOT_LANDING, DEFAULT_AUTOPILOT_TARGET_DEPTH,
};
use crate::sources::{DirectoryWorkSource, WorkSource, WorkSourceError, load_active_tasks};
use crate::surface_lint::conflict_key_for_path;
use chrono::{DateTime, Utc};
use flywheel_agents::hosts::DEFAULT_KILL_GRACE;
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
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

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
    /// True when the verdict is an error fallback (the agent failed, its
    /// output did not parse, or no result came back) rather than a judgment.
    /// An errored verdict is never cached, so the next cycle re-asks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub errored: bool,
}

impl TierVerdict {
    pub(crate) fn not_relevant(tier: Tier, reason: impl Into<String>) -> Self {
        Self {
            tier,
            relevant: false,
            reason: reason.into(),
            findings: Vec::new(),
            errored: false,
        }
    }

    /// A not-relevant verdict standing in for a failed discovery.
    fn errored(tier: Tier, reason: impl Into<String>) -> Self {
        Self {
            errored: true,
            ..Self::not_relevant(tier, reason)
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

/// The runtime's failure marker for a run that hit `RunRequest.timeout_seconds`.
const RUNTIME_TIMEOUT: &str = "timeout";

/// Slack past an agent call's deadline for the runtime's own cancellation
/// (SIGTERM to the agent's process group, the kill grace, the SIGKILL
/// sweep, the reap) before the outer backstop drops the run outright.
const AGENT_TEARDOWN_SLACK: Duration = DEFAULT_KILL_GRACE.saturating_mul(2);

/// Run an agent call under its wall-clock deadline.
///
/// The deadline travels into the run itself (`RunRequest.timeout_seconds`)
/// so the runtime cancels the agent and waits for its process tree to die;
/// `call` maps the runtime's own `timeout` failure to
/// [`DeadlineExceeded`] via `timed_out`. The outer bound is only a backstop
/// for a teardown that itself wedges, set past the deadline by
/// [`AGENT_TEARDOWN_SLACK`] so it never preempts the runtime's
/// cancellation.
async fn agent_deadline<F, T>(
    call: F,
    deadline_seconds: Option<f64>,
    timed_out: impl FnOnce(&T) -> bool,
) -> Result<T, DeadlineExceeded>
where
    F: Future<Output = T>,
{
    let started = tokio::time::Instant::now();
    let backstop =
        deadline_seconds.map(|seconds| seconds.max(0.0) + AGENT_TEARDOWN_SLACK.as_secs_f64());
    let output = bounded(call, backstop)
        .await
        .map_err(|exceeded| DeadlineExceeded {
            ceiling: Duration::from_secs_f64(deadline_seconds.unwrap_or_default().max(0.0)),
            elapsed: exceeded.elapsed,
        })?;
    match deadline_seconds {
        Some(seconds) if timed_out(&output) => Err(DeadlineExceeded {
            ceiling: Duration::from_secs_f64(seconds.max(0.0)),
            elapsed: started.elapsed(),
        }),
        _ => Ok(output),
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
            let output = agent_deadline(
                self.session.run(prompt, &self.repo_root, None),
                self.deadline_seconds,
                |outcome| {
                    matches!(
                        outcome,
                        Err(ColdSessionError::Run { error_type, .. }) if error_type == RUNTIME_TIMEOUT
                    )
                },
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
        session: ColdSession::with_configuration(repo_invoker_configuration(&options))
            .with_timeout_seconds(options.deadline_seconds),
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
        Err(err) => TierVerdict::errored(tier, format!("unparseable discovery response: {err}")),
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
        errored: false,
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
    discover_tier_with(tier, repo_root, None, invoker).await
}

/// [`discover_tier`] with the cycle's shared repo digest appended to the
/// tier prompt ([`prompt_with_digest`]).
pub async fn discover_tier_with(
    tier: Tier,
    repo_root: &Path,
    digest: Option<&str>,
    invoker: &dyn AutopilotInvoker,
) -> TierVerdict {
    let prompt = prompt_with_digest(tier_prompt(tier, repo_root), digest);
    match invoker.invoke(&prompt).await {
        Ok(text) => parse_tier_verdict(tier, &text),
        Err(err) => TierVerdict::errored(tier, format!("discovery agent error: {err}")),
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
    run_discovery_for(repo_root, driver, &Tier::ALL, None).await
}

/// [`run_discovery`] over a subset of tiers, with the cycle's shared repo
/// digest injected into every tier prompt.
///
/// Returns exactly one verdict per tier in `tiers`, in tier order; tiers
/// outside `tiers` are never dispatched (the structural pre-filter and the
/// verdict cache supply them). An empty `tiers` runs no agent at all.
pub async fn run_discovery_for(
    repo_root: &Path,
    driver: DiscoveryDriver<'_>,
    tiers: &[Tier],
    digest: Option<&str>,
) -> Result<Vec<TierVerdict>, InvokeError> {
    let tiers: Vec<Tier> = Tier::ALL
        .into_iter()
        .filter(|tier| tiers.contains(tier))
        .collect();
    if tiers.is_empty() {
        return Ok(Vec::new());
    }
    match driver {
        DiscoveryDriver::PerTier(invoker) => Ok(join_all(
            tiers
                .iter()
                .map(|tier| discover_tier_with(*tier, repo_root, digest, invoker))
                .collect(),
        )
        .await),
        DiscoveryDriver::SingleSession(runner) => {
            let options = SingleSessionOptions {
                tiers,
                digest: digest.map(str::to_owned),
                ..SingleSessionOptions::default()
            };
            run_single_session_discovery(repo_root, Some(runner), &options).await
        }
        DiscoveryDriver::Production(options) => {
            let options = SingleSessionOptions {
                tiers,
                digest: digest.map(str::to_owned),
                ..options
            };
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
    build_tier_agents_for(repo_root, &Tier::ALL, None, model, max_turns, effort)
}

/// [`build_tier_agents`] for a subset of tiers, each subagent prompt
/// carrying the cycle's shared repo digest.
pub fn build_tier_agents_for(
    repo_root: &Path,
    tiers: &[Tier],
    digest: Option<&str>,
    model: Option<&str>,
    max_turns: u32,
    effort: DiscoveryEffort,
) -> BTreeMap<String, TierAgentDefinition> {
    tiers
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
                    prompt: prompt_with_digest(tier_prompt(*tier, repo_root), digest),
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
    orchestrator_prompt_for(repo_root, &Tier::ALL)
}

/// The capitalized English count word for 1-11 (the dispatcher prompt's
/// "Eleven tier-relevance subagents" spelling), digits beyond.
fn count_word(n: usize) -> String {
    const WORDS: [&str; 11] = [
        "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten", "Eleven",
    ];
    n.checked_sub(1)
        .and_then(|i| WORDS.get(i))
        .map_or_else(|| n.to_string(), |w| (*w).to_owned())
}

/// [`orchestrator_prompt`] for the subset of tiers actually dispatched this
/// cycle (byte-identical to it for all 11).
pub fn orchestrator_prompt_for(repo_root: &Path, tiers: &[Tier]) -> String {
    let keys = tiers
        .iter()
        .map(|tier| tier_agent_key(*tier))
        .collect::<Vec<_>>()
        .join(", ");
    let n = tiers.len();
    let word = count_word(n);
    format!(
        "You are autopilot's discovery dispatcher for the repository at {}.\n\n\
         {word} tier-relevance subagents are registered: {keys}.\n\n\
         Use the Agent tool (a.k.a. Task) to dispatch EVERY one of these {n} subagents \
         exactly once, each with subagent_type set to its key. Tell each subagent to \
         read the repository, judge whether its tier is relevant to THIS codebase, and \
         return its verdict as the fenced JSON block its own instructions define. \
         Dispatch all {n} regardless of your own opinion -- do not skip, merge, or \
         pre-judge any tier. The per-tier tool-results are read directly, so you need \
         not restate them.\n\n\
         After all {n} subagents return, emit exactly one final fenced JSON block \
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
    /// The tiers registered and dispatched (tier order); all 11 by default.
    pub tiers: Vec<Tier>,
    /// The cycle's shared repo digest, appended to every subagent prompt.
    pub digest: Option<String>,
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
            tiers: Tier::ALL.to_vec(),
            digest: None,
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
    let agents = build_tier_agents_for(
        repo_root,
        &options.tiers,
        options.digest.as_deref(),
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
                timeout_seconds: self.deadline_seconds,
            };
            let options = RunOptions {
                sink: None,
                host: self.host.clone(),
                approval_resolver: None,
            };
            let completed = agent_deadline(
                self.runtime.run(request, options),
                self.deadline_seconds,
                |outcome| {
                    matches!(
                        outcome,
                        Ok(run) if run.failure.as_ref().is_some_and(|f| f.error_type == RUNTIME_TIMEOUT)
                    )
                },
            )
            .await??;
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
    collect_tier_verdicts_for(result, &Tier::ALL)
}

/// [`collect_tier_verdicts`] for the tiers actually dispatched: exactly one
/// verdict per tier in `tiers` (tier order); a result keyed to any other
/// tier is ignored.
pub fn collect_tier_verdicts_for(result: &IterationResult, tiers: &[Tier]) -> DiscoveryCollection {
    let tiers: Vec<Tier> = Tier::ALL
        .into_iter()
        .filter(|tier| tiers.contains(tier))
        .collect();
    let mut from_tool: BTreeMap<Tier, TierVerdict> = BTreeMap::new();
    for interaction in &result.signals.tool_interactions {
        if !SUBAGENT_TOOL_NAMES.contains(&interaction.tool_name.as_str()) {
            continue;
        }
        let Some(tier) = tier_from_agent_key(interaction.tool_input.get("subagent_type"))
            .filter(|tier| tiers.contains(tier))
        else {
            continue;
        };
        let verdict = match tool_result_text(interaction.result.as_ref()) {
            Some(text) => parse_tier_verdict(tier, &text),
            None => TierVerdict::errored(tier, "subagent tool-result missing, errored, or empty"),
        };
        from_tool.insert(tier, verdict);
    }

    let missing = tiers.iter().any(|tier| !from_tool.contains_key(tier));
    let mut aggregated = if missing {
        parse_aggregated_verdicts(&result.transcript)
    } else {
        BTreeMap::new()
    };

    let (mut used_tool, mut used_aggregated, mut used_error) = (false, false, false);
    let verdicts = tiers
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
                TierVerdict::errored(
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
    let prompt = orchestrator_prompt_for(repo_root, &options.tiers);
    let result = match session_runner {
        Some(runner) => runner.run(&prompt).await?,
        None => {
            build_single_session_runner(repo_root, options)
                .run(&prompt)
                .await?
        }
    };
    Ok(collect_tier_verdicts_for(&result, &options.tiers).verdicts)
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
    author_finding_with(finding, repo_root, None, invoker).await
}

/// [`author_finding`] with the cycle's shared repo digest appended to the
/// authoring prompt ([`prompt_with_digest`]).
pub async fn author_finding_with(
    finding: &Finding,
    repo_root: &Path,
    digest: Option<&str>,
    invoker: &dyn AutopilotInvoker,
) -> AuthoringResult {
    let prompt = prompt_with_digest(authoring_prompt(finding, repo_root), digest);
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
    /// Tiers the structural pre-filter skipped before discovery, each with
    /// its reason (also counted in `not_relevant_tiers`).
    pub prefiltered: Vec<TierPrefilterSkip>,
    /// How discovery used the durable verdict cache this pass.
    pub discovery_cache: DiscoveryCacheStatus,
    /// Finding ids skipped because an earlier cycle already authored them
    /// at this same repo state.
    pub already_authored: Vec<String>,
    /// Non-fatal operational notes (e.g. a cache write failure).
    pub notes: Vec<String>,
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
            prefiltered: Vec::new(),
            discovery_cache: DiscoveryCacheStatus::NotConsulted,
            already_authored: Vec::new(),
            notes: Vec::new(),
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
    /// Authoring sessions run at once (`[autopilot] authoring_concurrency`);
    /// 1 is the serial loop. Emission order and the write cap are
    /// independent of it.
    pub authoring_concurrency: usize,
    /// The durable discovery-verdict cache file; `None` disables caching
    /// (the digest and pre-filter still run, uncached).
    pub discovery_cache: Option<PathBuf>,
    /// Cached-verdict lifetime (`[autopilot] discovery_cache_ttl_seconds`);
    /// `0` disables caching.
    pub discovery_cache_ttl_seconds: f64,
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
            authoring_concurrency: usize::try_from(DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY)
                .unwrap_or(1),
            discovery_cache: None,
            discovery_cache_ttl_seconds: DEFAULT_AUTOPILOT_DISCOVERY_CACHE_TTL_SECONDS,
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
    // single-session tier-subagent path. Either way `run_discovery_for`
    // returns exactly one verdict per dispatched tier.
    let (driver, driver_label) = match (options.discovery_invoker, options.discovery_session_runner)
    {
        (Some(invoker), _) => (DiscoveryDriver::PerTier(invoker), "per_tier"),
        (None, Some(runner)) => (DiscoveryDriver::SingleSession(runner), "single_session"),
        (None, None) => (
            DiscoveryDriver::Production(SingleSessionOptions {
                model: options.model.clone(),
                deadline_seconds: autopilot_ceiling,
                ..SingleSessionOptions::default()
            }),
            "single_session",
        ),
    };

    // The cycle context (P2/P5): the repo state keys a durable cache of the
    // shared digest, the structural pre-filter decisions, and every tier
    // verdict, so an unchanged repo reuses all three across cycles and
    // daemon restarts.
    let now_epoch = epoch_seconds(options.now.map_or_else(Utc::now, |clock| clock()));
    let mut notes = Vec::new();
    let state = RepoState::probe(&options.repo_root);
    let mut cache: Result<DiscoveryCache, String> = match (&options.discovery_cache, &state) {
        (None, _) => Err("no cache path configured".to_owned()),
        (Some(_), _) if options.discovery_cache_ttl_seconds <= 0.0 => {
            Err("discovery_cache_ttl_seconds is 0".to_owned())
        }
        (Some(_), Err(reason)) => Err(reason.clone()),
        (Some(path), Ok(_)) => Ok(DiscoveryCache::load(
            path,
            options.discovery_cache_ttl_seconds,
            now_epoch,
        )),
    };
    let state_key = state.as_ref().map(RepoState::state_key).unwrap_or_default();
    let mut entry = match cache.as_ref().ok().and_then(|c| c.entry(&state_key)) {
        Some(entry) => entry.clone(),
        None => {
            let inventory = RepoInventory::scan(&options.repo_root);
            let head = state.as_ref().ok().map(|s| s.head.clone());
            DiscoveryCacheEntry {
                state_key: state_key.clone(),
                head: head.clone().unwrap_or_default(),
                created_at: now_epoch,
                digest: build_repo_digest(&options.repo_root, &inventory, head.as_deref()),
                prefiltered: structural_prefilter(&inventory),
                verdicts: Vec::new(),
                consumed: BTreeSet::new(),
            }
        }
    };
    let digest = entry.digest.clone();
    let prefiltered = entry.prefiltered.clone();
    let dispatch: Vec<Tier> = Tier::ALL
        .into_iter()
        .filter(|tier| !prefiltered.iter().any(|skip| skip.tier == *tier))
        .collect();
    let config = discovery_config_fingerprint(&options, driver_label);
    let keys: BTreeMap<Tier, String> = dispatch
        .iter()
        .map(|tier| {
            let prompt = prompt_with_digest(tier_prompt(*tier, &options.repo_root), Some(&digest));
            (
                *tier,
                sha256_hex(&format!("{state_key}\n{config}\n{prompt}")),
            )
        })
        .collect();
    let reused: BTreeMap<Tier, TierVerdict> = if cache.is_ok() {
        keys.iter()
            .filter_map(|(tier, key)| entry.verdict_for(*tier, key).map(|v| (*tier, v.clone())))
            .collect()
    } else {
        BTreeMap::new()
    };
    let to_discover: Vec<Tier> = dispatch
        .iter()
        .copied()
        .filter(|tier| !reused.contains_key(tier))
        .collect();
    let fresh = run_discovery_for(&options.repo_root, driver, &to_discover, Some(&digest)).await?;
    for verdict in fresh.iter().filter(|v| !v.errored) {
        if let Some(key) = keys.get(&verdict.tier) {
            entry.put_verdict(key.clone(), verdict.clone());
        }
    }
    let discovery_cache = match &cache {
        Err(reason) => DiscoveryCacheStatus::Disabled(reason.clone()),
        Ok(_) if to_discover.is_empty() => DiscoveryCacheStatus::Hit,
        Ok(_) if reused.is_empty() => DiscoveryCacheStatus::Miss,
        Ok(_) => DiscoveryCacheStatus::Partial {
            reused: reused.keys().copied().collect(),
        },
    };
    persist_cache(&mut cache, &entry, &mut notes);

    let mut fresh_by_tier: BTreeMap<Tier, TierVerdict> =
        fresh.into_iter().map(|v| (v.tier, v)).collect();
    let verdicts: Vec<TierVerdict> = Tier::ALL
        .iter()
        .map(|tier| {
            if let Some(skip) = prefiltered.iter().find(|skip| skip.tier == *tier) {
                TierVerdict::not_relevant(*tier, skip.reason.clone())
            } else if let Some(verdict) = reused.get(tier) {
                verdict.clone()
            } else {
                fresh_by_tier.remove(tier).unwrap_or_else(|| {
                    TierVerdict::errored(*tier, "no discovery result for this tier")
                })
            }
        })
        .collect();
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
    // A finding already authored at this repo state (emitted or dropped by
    // an earlier cycle reusing the same cached verdict) is never authored
    // again: re-authoring it could only duplicate queued work.
    let mut already_authored = Vec::new();
    let mut findings: Vec<Finding> = Vec::new();
    for finding in verdicts
        .iter()
        .filter(|v| v.relevant)
        .flat_map(|v| v.findings.iter())
    {
        if entry.consumed.contains(&finding.id) {
            already_authored.push(finding.id.clone());
        } else {
            findings.push(finding.clone());
        }
    }
    let base = AutopilotPassResult {
        queue_depth_before: current,
        target_depth,
        relevant_tiers,
        not_relevant_tiers,
        landing: options.landing.clone(),
        prefiltered,
        discovery_cache,
        already_authored,
        ..AutopilotPassResult::default()
    };

    let sequenced = sequence_findings(&findings, &options.weights);
    if sequenced.is_empty() {
        let reason = if base.already_authored.is_empty() {
            "no actionable findings this cycle; idling without emitting".to_owned()
        } else {
            format!(
                "no actionable findings this cycle ({} already authored at this repo state); \
                 idling without emitting",
                base.already_authored.len()
            )
        };
        record_no_op(&reason)?;
        return Ok(AutopilotPassResult {
            reason,
            notes,
            ..base
        });
    }

    let mut emitted = Vec::new();
    let mut emitted_paths: Vec<PathBuf> = Vec::new();
    let mut dropped = Vec::new();
    let slots = usize::try_from(slots).unwrap_or(usize::MAX);
    let limit = options.authoring_concurrency.max(1);

    // Author the top-sequenced findings until the queue reaches the target
    // or the actionable findings run out; only enough to fill the
    // depth-to-target. Up to `limit` authoring sessions run at once, never
    // more than the slots still open, and results are emitted strictly in
    // sequence order regardless of which session finishes first -- so the
    // emitted set and the write cap match the serial loop exactly (limit 1
    // IS the serial loop). Leaving the loop drops every in-flight session,
    // which cancels it and kills its process group. Each emitted task
    // carries the breakdown of the finding it was authored from (never a
    // lookup by finding id, which two same-slug findings could collide on).
    {
        let mut window: VecDeque<InFlight<_>> = VecDeque::new();
        let mut next = 0usize;
        loop {
            while window.len() < limit
                && next < sequenced.len()
                && emitted_paths.len() + window.len() < slots
            {
                window.push_back(InFlight::new(
                    next,
                    author_finding_with(
                        &sequenced[next].finding,
                        &options.repo_root,
                        Some(&digest),
                        author,
                    ),
                ));
                next += 1;
            }
            let Some((index, result)) = next_in_order(&mut window).await else {
                break;
            };
            let scored = &sequenced[index];
            entry.consumed.insert(scored.finding.id.clone());
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
            if emitted_paths.len() >= slots {
                break;
            }
        }
    }
    persist_cache(&mut cache, &entry, &mut notes);

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
        landing: options.landing,
        reason,
        notes,
        ..base
    })
}

/// Epoch seconds (millisecond precision) of a UTC instant.
fn epoch_seconds(at: DateTime<Utc>) -> f64 {
    at.timestamp_millis() as f64 / 1000.0
}

/// Everything besides the rendered prompt that changes a tier verdict: the
/// discovery route and the agent configuration it runs under.
fn discovery_config_fingerprint(options: &RefillPassOptions<'_>, driver: &str) -> String {
    json!({
        "driver": driver,
        "model": options.model,
        "agent_id": options.agent_id,
        "agent_transport": options.agent_transport,
        "max_turns": DEFAULT_DISCOVERY_MAX_TURNS,
        "subagent_max_turns": DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS,
        "subagent_effort": DEFAULT_DISCOVERY_SUBAGENT_EFFORT.as_str(),
        "dispatcher": sha256_hex(&orchestrator_prompt(&options.repo_root)),
    })
    .to_string()
}

/// Upsert `entry` and write the cache; a write failure is a recorded note,
/// never a failed cycle.
fn persist_cache(
    cache: &mut Result<DiscoveryCache, String>,
    entry: &DiscoveryCacheEntry,
    notes: &mut Vec<String>,
) {
    if let Ok(cache) = cache {
        cache.upsert(entry.clone());
        if let Err(err) = cache.save() {
            notes.push(format!(
                "discovery cache write failed ({}): {err}",
                cache.path().display()
            ));
        }
    }
}

/// One authoring session in the ordered fan-out window: its sequence
/// index, the running future, and its output once ready.
struct InFlight<F: Future> {
    index: usize,
    future: Option<Pin<Box<F>>>,
    output: Option<F::Output>,
}

impl<F: Future> InFlight<F> {
    fn new(index: usize, future: F) -> Self {
        Self {
            index,
            future: Some(Box::pin(future)),
            output: None,
        }
    }
}

/// Drive every in-flight future until the OLDEST one is done, then hand
/// back its `(index, output)`; later futures that finish first keep their
/// output buffered for their turn. `None` when the window is empty.
async fn next_in_order<F: Future>(
    window: &mut VecDeque<InFlight<F>>,
) -> Option<(usize, F::Output)> {
    if window.is_empty() {
        return None;
    }
    std::future::poll_fn(|cx: &mut TaskContext<'_>| {
        for slot in window.iter_mut() {
            if let Some(future) = slot.future.as_mut()
                && let Poll::Ready(output) = future.as_mut().poll(cx)
            {
                slot.output = Some(output);
                slot.future = None;
            }
        }
        if window.front().is_some_and(|slot| slot.output.is_some()) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    let head = window.pop_front()?;
    Some((head.index, head.output?))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::InMemoryClaimStore;
    use flywheel_core::envelope::parse_envelope;
    use flywheel_core::invoker::{InvocationSignals, ToolInteraction};
    use flywheel_core::lifecycle::Lifecycle;
    use flywheel_core::store::StoreError;
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::CommandGrader;
    use sha2::{Digest, Sha256};
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    // --- helpers -------------------------------------------------------------------

    fn sha256(text: &str) -> String {
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    fn repo() -> PathBuf {
        PathBuf::from("/repo")
    }

    /// A scripted invoker returning `response` for every prompt.
    fn scripted(response: impl Into<String>) -> impl AutopilotInvoker {
        let response = response.into();
        move |_prompt: String| {
            let response = response.clone();
            async move { Ok(response) }
        }
    }

    fn fenced(body: &Value) -> String {
        format!("```json\n{body}\n```")
    }

    /// The `TIER: N` marker a tier prompt carries.
    fn tier_in_prompt(prompt: &str) -> u8 {
        let rest = &prompt[prompt.find("TIER: ").expect("prompt names its tier") + 6..];
        rest.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap()
    }

    const NOT_RELEVANT_TIERS: [u8; 4] = [1, 2, 3, 4];

    /// Canned per-tier JSON for a pure-library repo: tiers 1-4 not relevant,
    /// the rest surface one finding.
    fn library_response(tier_value: u8) -> String {
        let body = if NOT_RELEVANT_TIERS.contains(&tier_value) {
            json!({
                "relevant": false,
                "reason": format!("tier {tier_value} does not apply to a pure library"),
                "findings": [],
            })
        } else {
            json!({
                "relevant": true,
                "reason": format!("tier {tier_value} applies"),
                "findings": [{
                    "id": "f1",
                    "title": format!("finding for tier {tier_value}"),
                    "detail": "detail",
                    "evidence": ["src/lib.py:10"],
                    "urgency": 3,
                    "importance": 4,
                    "blocks": 0,
                    "effort": 2,
                    "ready": true,
                }],
            })
        };
        format!("Here is my assessment.\n{}\n", fenced(&body))
    }

    /// A scripted per-tier invoker routing on the prompt's tier marker,
    /// recording every tier it saw.
    fn library_invoker(seen: Arc<Mutex<Vec<u8>>>) -> impl AutopilotInvoker {
        move |prompt: String| {
            let seen = seen.clone();
            async move {
                let tier = tier_in_prompt(&prompt);
                seen.lock().unwrap().push(tier);
                Ok(library_response(tier))
            }
        }
    }

    fn by_tier(verdicts: &[TierVerdict]) -> BTreeMap<u8, &TierVerdict> {
        verdicts.iter().map(|v| (v.tier.value(), v)).collect()
    }

    fn seed_repo(tmp: &TempDir) -> PathBuf {
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::write(
            repo.join("tests/test_existing.py"),
            "def test_ok():\n    pass\n",
        )
        .unwrap();
        repo
    }

    fn coverage_finding() -> Finding {
        Finding {
            detail: "the parser module has no tests".to_owned(),
            evidence: vec!["src/parser.py:1".to_owned()],
            urgency: 3,
            importance: 5,
            effort: 2,
            ..Finding::new("f1", Tier::TestCoverage, "add coverage for the parser")
        }
    }

    fn authored_entry(id: &str, run: &str, target: &str) -> Value {
        json!({
            "task": {
                "id": id,
                "goal": "The parser module is covered by tests.",
                "graders": [{"type": "command", "run": run}],
            },
            "authoritative_grader": run,
            "grader_source": GRADER_SOURCE_REPO_COMMAND,
            "grader_target": target,
        })
    }

    fn command_task(id: &str, run: &str) -> Task {
        Task::new(
            format!("Goal for {id}."),
            vec![Grader::Command(CommandGrader::new(run).unwrap())],
        )
        .with_id(id)
    }

    fn emitted(task_id: &str, grader_target: &str, creates_files: &[&str]) -> EmittedTask {
        EmittedTask {
            creates_files: creates_files.iter().map(|s| (*s).to_owned()).collect(),
            ..EmittedTask::new(
                Finding::new(format!("find-{task_id}"), Tier::TestCoverage, task_id),
                command_task(task_id, "cargo test --workspace"),
                "cargo test --workspace",
                GraderSource::RepoCommand,
                grader_target,
            )
        }
    }

    fn breakdown() -> ScoreBreakdown {
        ScoreBreakdown {
            tier: Tier::TestCoverage,
            tier_weight: 1,
            urgency: 1,
            importance: 1,
            blocks: 0,
            effort: 1,
            final_score: 10.0,
            preemptive: false,
        }
    }

    fn subagent_interaction(
        tier_value: u8,
        tool_name: &str,
        content: Option<ToolResultContent>,
        is_error: Option<bool>,
    ) -> ToolInteraction {
        let tool_use_id = format!("tu-{tier_value}");
        let mut tool_input = Map::new();
        tool_input.insert(
            "subagent_type".into(),
            json!(tier_agent_key(
                Tier::from_value(i64::from(tier_value)).unwrap()
            )),
        );
        tool_input.insert(
            "prompt".into(),
            json!(format!("evaluate tier {tier_value}")),
        );
        ToolInteraction {
            tool_use_id: tool_use_id.clone(),
            tool_name: tool_name.to_owned(),
            tool_input,
            result: content.map(|content| ToolResultObservation {
                tool_use_id,
                is_error,
                content: Some(content),
            }),
        }
    }

    fn verdict_json(tier_value: u8, relevant: bool) -> String {
        let body = if relevant {
            json!({
                "relevant": true,
                "reason": format!("tier {tier_value} applies"),
                "findings": [{
                    "id": "f1",
                    "title": format!("finding for tier {tier_value}"),
                    "evidence": ["src/lib.py:1"],
                    "urgency": 3, "importance": 4, "blocks": 0, "effort": 2, "ready": true,
                }],
            })
        } else {
            json!({
                "relevant": false,
                "reason": format!("tier {tier_value} does not apply"),
                "findings": [],
            })
        };
        format!("Subagent summary.\n{}\n", fenced(&body))
    }

    fn agent_interaction(tier_value: u8) -> ToolInteraction {
        subagent_interaction(
            tier_value,
            "Agent",
            Some(ToolResultContent::Text(verdict_json(tier_value, true))),
            None,
        )
    }

    fn session_result(interactions: Vec<ToolInteraction>, transcript: &str) -> IterationResult {
        IterationResult {
            transcript: transcript.to_owned(),
            messages: Vec::new(),
            envelope: parse_envelope(transcript),
            signals: InvocationSignals {
                tool_interactions: interactions,
                ..InvocationSignals::default()
            },
            failure: None,
            usage: None,
        }
    }

    fn all_tiers_result() -> IterationResult {
        session_result(
            Tier::ALL
                .iter()
                .map(|t| agent_interaction(t.value()))
                .collect(),
            "",
        )
    }

    #[test]
    fn breakdown_serializes_final_under_the_python_key() {
        let value = serde_json::to_value(breakdown()).unwrap();
        assert_eq!(value["final"], json!(10.0));
        assert_eq!(value["tier"], json!(6));
    }

    // --- prompts: byte-identical to the Python ---------------------------------------

    const PY_TIER1_PROMPT: &str = "You are the relevance agent for autopilot TIER: 1 (Production down / active harm).\n\nTier remit: Outages, in-progress data loss, active breach, payments failing. Nothing below advances while open.\n\nRead the repository at /repo and decide whether THIS tier is relevant to THIS codebase. Relevance is per-codebase: e.g. \"production down\" is meaningless for a pure library. If the tier does not apply, say so -- do not invent work to fill it.\n\nIf the tier is relevant, surface zero or more concrete findings, each backed by evidence you actually observed in the repo. For each finding estimate, on a 0-10 scale: urgency (how fast cost grows if untouched), importance (value/risk-reduction if done), blocks (how many other efforts it unblocks), effort (cost to do). Set ready=true when the work can start now.\n\nRespond with exactly one fenced JSON block:\n```json\n{\n  \"relevant\": true,\n  \"reason\": \"one sentence on why this tier does or does not apply\",\n  \"findings\": [\n    {\"id\": \"short-slug\", \"title\": \"...\", \"detail\": \"...\", \"evidence\": [\"path:line or observation\"], \"urgency\": 0, \"importance\": 0, \"blocks\": 0, \"effort\": 0, \"ready\": true}\n  ]\n}\n```\nA not-relevant verdict carries relevant=false, a reason, and an empty findings list.";

    /// sha256 of `tier_prompt(tier, Path("/repo"))` for every tier, from
    /// the Python module.
    const PY_TIER_PROMPT_HASHES: [&str; 11] = [
        "613b11fa0086476ab46f435c2ddb4f50ce28940e2770471df0b7f19501e48eeb",
        "6ffaa4c9270c1a2f38f565dd621c71486613fe55a028dbc8745cd087a000e6ae",
        "a6c4deff9b32f7752da15cc5a3d6146ea3f3605489aea1dc1a53dffbd8547174",
        "be760d8b2e80cc878182092fdbb3e66c116acf8f38d1fb7fa8245eeb721c57df",
        "7e59df016b7208d58b6dba2aa5419b265910b85f9c1349455359b0d946e5a287",
        "50e5fd178fc018fa68b4aecde9d74969a10ba05ea6f00ba834457a349235e344",
        "b005859d812429916fd703e06c41cf764bf9686026bc2c2aae14b5c9abe08040",
        "58d0b9049409290773a0507703385f2726ab6068c9da59f4298050192b19881b",
        "148f78d3e423b041fb16fc057bf3209fb49d6f2e46ac4d08d1579e16ed6e34a3",
        "94986b3fc6629f3cdef65504364384d4aeb275c1066fcfb136122d474ea20964",
        "62f860999d5372f675f12290f00fc55423d6f95499315c224f2169aa3a5bb15a",
    ];

    const PY_ORCHESTRATOR_PROMPT: &str = "You are autopilot's discovery dispatcher for the repository at /repo.\n\nEleven tier-relevance subagents are registered: tier-1, tier-2, tier-3, tier-4, tier-5, tier-6, tier-7, tier-8, tier-9, tier-10, tier-11.\n\nUse the Agent tool (a.k.a. Task) to dispatch EVERY one of these 11 subagents exactly once, each with subagent_type set to its key. Tell each subagent to read the repository, judge whether its tier is relevant to THIS codebase, and return its verdict as the fenced JSON block its own instructions define. Dispatch all 11 regardless of your own opinion -- do not skip, merge, or pre-judge any tier. The per-tier tool-results are read directly, so you need not restate them.\n\nAfter all 11 subagents return, emit exactly one final fenced JSON block aggregating every verdict as a safety net:\n```json\n{\"verdicts\": [{\"tier\": 1, \"relevant\": false, \"reason\": \"...\", \"findings\": []}]}\n```";

    const PY_AUTHORING_PROMPT: &str = "You are autopilot's headless authoring agent. Compile the finding below into one or more flywheel tasks following the fw-spec and fw-plan contracts -- a one-sentence goal and the strongest out-of-band grader you can express.\n\nRepository: /repo\nFinding (Tier 6, Test coverage (shipped/in-flight)): add coverage for the parser\nDetail: the parser module has no tests\nEvidence:\n  - src/parser.py:1\n  - src/parser.py:40\n\nHARD INTEGRITY RULES:\n- Every emitted task must carry at least one grader.\n- The authoritative grader MUST be either (a) a command that invokes a check already committed in this repo (a test/lint/build command that exists BEFORE this task runs), or (b) a registered held-out oracle by absolute path. It must NEVER be a brand-new check this task's own diff creates -- that is self-attestation.\n- Resolve any ambiguity a human would be asked about as a recorded assumption; never emit a vaguer task to dodge a question.\n- If you cannot express an out-of-band grader for this finding, do not invent one: set \"dropped\" to a one-sentence reason and emit no task.\n\nRespond with exactly one fenced JSON block:\n```json\n{\n  \"tasks\": [\n    {\n      \"task\": {\"id\": \"kebab-id\", \"goal\": \"one sentence\", \"graders\": [{\"type\": \"command\", \"run\": \"<existing repo check>\"}], \"tags\": [], \"context\": {}},\n      \"authoritative_grader\": \"<run string matching a command grader above>\",\n      \"grader_source\": \"repo_command\",\n      \"grader_target\": \"<repo-relative path to the pre-existing check the grader runs>\",\n      \"creates_files\": [\"<paths this task's diff will create>\"],\n      \"assumptions\": [\"<ambiguity you resolved yourself>\"]\n    }\n  ],\n  \"dropped\": \"\"\n}\n```";

    #[test]
    fn tier_prompt_is_byte_identical_to_python() {
        assert_eq!(tier_prompt(Tier::ProductionDown, &repo()), PY_TIER1_PROMPT);
        for (tier, expected) in Tier::ALL.iter().zip(PY_TIER_PROMPT_HASHES) {
            assert_eq!(sha256(&tier_prompt(*tier, &repo())), expected, "{tier:?}");
        }
    }

    #[test]
    fn orchestrator_prompt_is_byte_identical_to_python() {
        assert_eq!(orchestrator_prompt(&repo()), PY_ORCHESTRATOR_PROMPT);
    }

    #[test]
    fn authoring_prompt_is_byte_identical_to_python() {
        let mut f = coverage_finding();
        f.evidence.push("src/parser.py:40".to_owned());
        assert_eq!(authoring_prompt(&f, &repo()), PY_AUTHORING_PROMPT);
        let bare = Finding::new("t11-x", Tier::Polish, "tidy");
        let prompt = authoring_prompt(&bare, &repo());
        assert_eq!(
            sha256(&prompt),
            "8bd600c9e92e75f48b0b9edacab4204b53e257540bff9b3e4606d90abce5d347"
        );
        assert!(prompt.contains("Detail: \nEvidence:\n  (none)\n"));
    }

    // --- JSON extraction ------------------------------------------------------------

    #[test]
    fn extract_json_object_prefers_the_fence_then_falls_back_to_braces() {
        let fenced_text = "prose {\"decoy\": 1}\n```json\n{\"a\": 1}\n```\ntrailing";
        assert_eq!(extract_json_object(fenced_text).unwrap()["a"], json!(1));
        assert_eq!(
            extract_json_object("no fence {\"b\": [1, {\"c\": 2}]} tail").unwrap()["b"][1]["c"],
            json!(2)
        );
        // An unclosed fence falls through to the brace span.
        assert_eq!(
            extract_json_object("```json\n{\"d\": 3}").unwrap()["d"],
            json!(3)
        );
        assert!(extract_json_object("no json here at all").is_err());
        assert!(
            extract_json_object("```json\n[1, 2]\n```")
                .unwrap_err()
                .contains("not an object")
        );
        assert!(extract_json_object("} {").is_err());
        // Malformed fenced JSON is a parse error, never silently re-tried.
        assert!(extract_json_object("```json\n{bad}\n```\n{\"ok\": 1}").is_err());
    }

    #[test]
    fn score_axes_coerce_defensively() {
        assert_eq!(coerce_axis(Some(&json!(true))), 0);
        assert_eq!(coerce_axis(Some(&json!(-4))), 0);
        assert_eq!(coerce_axis(Some(&json!(3.9))), 3);
        assert_eq!(coerce_axis(Some(&json!("7"))), 0);
        assert_eq!(coerce_axis(None), 0);
        assert_eq!(coerce_axis(Some(&json!(6))), 6);
    }

    // --- discovery: per-tier fan-out --------------------------------------------------

    #[tokio::test]
    async fn exactly_one_verdict_per_tier() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let invoker = library_invoker(seen.clone());
        let verdicts = run_discovery(&repo(), DiscoveryDriver::PerTier(&invoker))
            .await
            .unwrap();
        assert_eq!(
            verdicts.iter().map(|v| v.tier).collect::<Vec<_>>(),
            Tier::ALL.to_vec()
        );
        let mut seen = seen.lock().unwrap().clone();
        seen.sort_unstable();
        assert_eq!(seen, (1..=11).collect::<Vec<u8>>());
    }

    #[tokio::test]
    async fn not_relevant_tiers_contribute_zero_findings_with_reason() {
        let invoker = library_invoker(Arc::new(Mutex::new(Vec::new())));
        let verdicts = run_discovery(&repo(), DiscoveryDriver::PerTier(&invoker))
            .await
            .unwrap();
        let by_tier = by_tier(&verdicts);
        for tier_value in NOT_RELEVANT_TIERS {
            let verdict = by_tier[&tier_value];
            assert!(!verdict.relevant);
            assert!(verdict.findings.is_empty());
            assert!(!verdict.reason.is_empty());
        }
        for verdict in &verdicts {
            if verdict.relevant {
                assert_eq!(verdict.findings.len(), 1);
                assert!(verdict.findings.iter().all(|f| f.tier == verdict.tier));
            }
        }
    }

    #[test]
    fn not_relevant_verdict_drops_any_findings_the_agent_lists() {
        let contradictory = json!({
            "relevant": false,
            "reason": "does not apply",
            "findings": [{"id": "x", "title": "sneaky", "urgency": 9}],
        });
        let verdict = parse_tier_verdict(Tier::ProductionDown, &fenced(&contradictory));
        assert!(!verdict.relevant);
        assert!(verdict.findings.is_empty());
        assert_eq!(verdict.reason, "does not apply");
    }

    #[test]
    fn relevant_with_no_findings_is_distinct_from_not_relevant() {
        let empty_relevant = json!({
            "relevant": true,
            "reason": "applies but nothing actionable now",
            "findings": [],
        });
        let verdict = parse_tier_verdict(Tier::CoreFeatureWork, &fenced(&empty_relevant));
        assert!(verdict.relevant);
        assert!(verdict.findings.is_empty());
    }

    #[test]
    fn verdict_reasons_default_when_absent() {
        let relevant = parse_tier_verdict(Tier::Polish, &fenced(&json!({"relevant": 1})));
        assert_eq!(relevant.reason, "tier judged relevant to this codebase");
        let not = parse_tier_verdict(Tier::Polish, &fenced(&json!({"reason": "  "})));
        assert_eq!(not.reason, "tier judged not relevant to this codebase");
    }

    #[tokio::test]
    async fn one_failing_tier_does_not_abort_the_fan_out() {
        let invoker = |prompt: String| async move {
            let tier = tier_in_prompt(&prompt);
            if tier == 5 {
                return Err(InvokeError::Other("agent exploded on tier 5".to_owned()));
            }
            Ok(library_response(tier))
        };
        let verdicts = run_discovery(&repo(), DiscoveryDriver::PerTier(&invoker))
            .await
            .unwrap();
        assert_eq!(verdicts.len(), 11);
        let by_tier = by_tier(&verdicts);
        assert!(!by_tier[&5].relevant);
        assert_eq!(
            by_tier[&5].reason,
            "discovery agent error: agent exploded on tier 5"
        );
        assert!(by_tier[&6].relevant);
    }

    #[test]
    fn unparseable_response_yields_not_relevant_error_verdict() {
        let verdict = parse_tier_verdict(Tier::TechDebt, "no json here at all");
        assert!(!verdict.relevant);
        assert!(verdict.findings.is_empty());
        assert!(verdict.reason.contains("unparseable"));
    }

    #[test]
    fn findings_carry_the_full_score_axes_and_skip_malformed_entries() {
        let body = json!({
            "relevant": true,
            "reason": "applies",
            "findings": [
                {"id": "slug", "title": "real finding", "urgency": 7, "importance": 8,
                 "blocks": 2, "effort": 1, "ready": false, "evidence": ["a", 2]},
                {"title": "   "},
                "not an object",
                {"title": "untitled slug", "detail": 5},
            ],
        });
        let verdict = parse_tier_verdict(Tier::TestCoverage, &fenced(&body));
        assert_eq!(verdict.findings.len(), 2);
        let f = &verdict.findings[0];
        assert_eq!((f.urgency, f.importance, f.blocks, f.effort), (7, 8, 2, 1));
        assert!(!f.ready);
        assert_eq!(f.id, "t6-slug");
        assert_eq!(f.evidence, vec!["a".to_owned(), "2".to_owned()]);
        // A missing id falls back to the entry's index; a non-string detail
        // reads as empty.
        assert_eq!(verdict.findings[1].id, "t6-3");
        assert_eq!(verdict.findings[1].detail, "");
    }

    #[tokio::test]
    async fn production_path_runs_one_session_and_returns_eleven() {
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let runner = {
            let sessions = sessions.clone();
            move |prompt: String| {
                let sessions = sessions.clone();
                async move {
                    sessions.lock().unwrap().push(prompt);
                    Ok(all_tiers_result())
                }
            }
        };
        let verdicts = run_discovery(&repo(), DiscoveryDriver::SingleSession(&runner))
            .await
            .unwrap();
        let sessions = sessions.lock().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0], orchestrator_prompt(&repo()));
        assert_eq!(
            verdicts.iter().map(|v| v.tier).collect::<Vec<_>>(),
            Tier::ALL.to_vec()
        );
    }

    // --- single-session collector ----------------------------------------------------

    #[test]
    fn collects_eleven_verdicts_from_tool_results_in_tier_order() {
        let collection = collect_tier_verdicts(&all_tiers_result());
        assert_eq!(
            collection
                .verdicts
                .iter()
                .map(|v| v.tier)
                .collect::<Vec<_>>(),
            Tier::ALL.to_vec()
        );
        assert_eq!(collection.source, CollectSource::ToolResult);
        assert_eq!(collection.source.as_str(), COLLECT_SOURCE_TOOL_RESULT);
        assert!(collection.verdicts.iter().all(|v| v.relevant));
    }

    #[test]
    fn both_task_and_agent_tool_names_parse_identically() {
        let via = |name: &str| {
            collect_tier_verdicts(&session_result(
                Tier::ALL
                    .iter()
                    .map(|t| {
                        subagent_interaction(
                            t.value(),
                            name,
                            Some(ToolResultContent::Text(verdict_json(t.value(), true))),
                            None,
                        )
                    })
                    .collect(),
                "",
            ))
        };
        assert_eq!(via("Agent"), via("Task"));
        assert!(SUBAGENT_TOOL_NAMES.contains(&"Agent") && SUBAGENT_TOOL_NAMES.contains(&"Task"));
    }

    #[test]
    fn tool_result_content_as_block_list_is_concatenated() {
        let blocks = vec![
            json!({"type": "text", "text": "intro "})
                .as_object()
                .unwrap()
                .clone(),
            json!({"type": "image"}).as_object().unwrap().clone(),
            json!({"type": "text", "text": verdict_json(5, true)})
                .as_object()
                .unwrap()
                .clone(),
        ];
        let interaction =
            subagent_interaction(5, "Agent", Some(ToolResultContent::Blocks(blocks)), None);
        let collection = collect_tier_verdicts(&session_result(vec![interaction], ""));
        let by_tier = by_tier(&collection.verdicts);
        assert!(by_tier[&5].relevant);
        assert_eq!(collection.source, CollectSource::Mixed);
    }

    #[test]
    fn verdicts_come_from_tool_results_not_the_parents_prose() {
        let interactions = Tier::ALL
            .iter()
            .map(|t| {
                subagent_interaction(
                    t.value(),
                    "Agent",
                    Some(ToolResultContent::Text(verdict_json(
                        t.value(),
                        t.value() % 2 == 0,
                    ))),
                    None,
                )
            })
            .collect();
        let lies = json!({
            "verdicts": Tier::ALL.iter().map(|t| json!({
                "tier": t.value(), "relevant": false, "reason": "lies", "findings": []
            })).collect::<Vec<_>>()
        });
        let misleading_prose = format!("All tiers are irrelevant.\n{}\n", fenced(&lies));
        let collection = collect_tier_verdicts(&session_result(interactions, &misleading_prose));
        for verdict in &collection.verdicts {
            assert_eq!(verdict.relevant, verdict.tier.value() % 2 == 0);
        }
        assert_eq!(collection.source, CollectSource::ToolResult);
    }

    #[test]
    fn missing_errored_and_malformed_tool_results_yield_error_verdicts() {
        let interactions = Tier::ALL
            .iter()
            .map(|t| match t {
                Tier::TechDebt => subagent_interaction(t.value(), "Agent", None, None),
                Tier::Polish => subagent_interaction(
                    t.value(),
                    "Agent",
                    Some(ToolResultContent::Text("boom".to_owned())),
                    Some(true),
                ),
                Tier::Documentation => subagent_interaction(
                    t.value(),
                    "Agent",
                    Some(ToolResultContent::Text("no json here at all".to_owned())),
                    None,
                ),
                Tier::NonCriticalBugs => subagent_interaction(
                    t.value(),
                    "Agent",
                    Some(ToolResultContent::Text(String::new())),
                    None,
                ),
                other => agent_interaction(other.value()),
            })
            .collect();
        let collection = collect_tier_verdicts(&session_result(interactions, ""));
        assert_eq!(collection.verdicts.len(), 11);
        let by_tier = by_tier(&collection.verdicts);
        for tier in [Tier::TechDebt, Tier::Polish, Tier::NonCriticalBugs] {
            let verdict = by_tier[&tier.value()];
            assert!(!verdict.relevant);
            assert!(verdict.findings.is_empty());
            assert_eq!(
                verdict.reason,
                "subagent tool-result missing, errored, or empty"
            );
        }
        assert!(!by_tier[&10].relevant);
        assert!(by_tier[&10].reason.contains("unparseable"));
        // Present-but-bad results are still tool-result sourced.
        assert_eq!(collection.source, CollectSource::ToolResult);
    }

    #[test]
    fn each_tier_present_exactly_once_even_with_no_interactions() {
        let collection = collect_tier_verdicts(&session_result(Vec::new(), ""));
        assert_eq!(
            collection
                .verdicts
                .iter()
                .map(|v| v.tier)
                .collect::<Vec<_>>(),
            Tier::ALL.to_vec()
        );
        assert!(collection.verdicts.iter().all(|v| !v.relevant));
        assert_eq!(collection.source, CollectSource::ErrorFallback);
    }

    #[test]
    fn non_subagent_and_unkeyed_tool_calls_are_ignored() {
        let mut stray_input = Map::new();
        stray_input.insert("file_path".into(), json!("/repo/README.md"));
        let stray = ToolInteraction {
            tool_use_id: "x".to_owned(),
            tool_name: "Read".to_owned(),
            tool_input: stray_input,
            result: Some(ToolResultObservation {
                tool_use_id: "x".to_owned(),
                is_error: None,
                content: Some(ToolResultContent::Text("contents".to_owned())),
            }),
        };
        let mut unkeyed = agent_interaction(3);
        unkeyed
            .tool_input
            .insert("subagent_type".into(), json!("tier-99"));
        let mut interactions = vec![stray, unkeyed];
        interactions.extend(Tier::ALL.iter().map(|t| agent_interaction(t.value())));
        let collection = collect_tier_verdicts(&session_result(interactions, ""));
        assert_eq!(collection.verdicts.len(), 11);
        assert_eq!(collection.source, CollectSource::ToolResult);
    }

    #[test]
    fn aggregated_fallback_when_no_tool_results_key_to_tiers() {
        let aggregate = json!({
            "verdicts": Tier::ALL.iter().map(|t| json!({
                "tier": t.value(),
                "relevant": t.value() >= 5,
                "reason": format!("tier {} aggregated", t.value()),
                "findings": if t.value() >= 5 {
                    json!([{"id": "g1", "title": format!("agg finding {}", t.value()), "urgency": 1}])
                } else {
                    json!([])
                },
            })).collect::<Vec<_>>()
        });
        let transcript = format!("Final summary.\n{}\n", fenced(&aggregate));
        let collection = collect_tier_verdicts(&session_result(Vec::new(), &transcript));
        assert_eq!(
            collection
                .verdicts
                .iter()
                .map(|v| v.tier)
                .collect::<Vec<_>>(),
            Tier::ALL.to_vec()
        );
        assert_eq!(collection.source, CollectSource::Aggregated);
        let by_tier = by_tier(&collection.verdicts);
        assert!(!by_tier[&1].relevant);
        assert!(by_tier[&5].relevant);
        assert_eq!(by_tier[&5].findings.len(), 1);
        assert_eq!(by_tier[&5].findings[0].id, "t5-g1");
    }

    #[test]
    fn partial_tool_results_filled_from_aggregate_is_mixed() {
        let interactions = [1, 2, 3].into_iter().map(agent_interaction).collect();
        let aggregate = json!({
            "verdicts": Tier::ALL.iter().map(|t| json!({
                "tier": t.value(), "relevant": false, "reason": "agg", "findings": []
            })).chain([json!({"tier": true}), json!({"tier": 42}), json!("junk")]).collect::<Vec<_>>()
        });
        let collection = collect_tier_verdicts(&session_result(interactions, &fenced(&aggregate)));
        assert_eq!(collection.verdicts.len(), 11);
        assert_eq!(collection.source, CollectSource::Mixed);
        let by_tier = by_tier(&collection.verdicts);
        assert!(by_tier[&1].relevant);
        assert!(!by_tier[&4].relevant);
    }

    #[tokio::test]
    async fn run_single_session_discovery_returns_eleven_in_order() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let runner = {
            let captured = captured.clone();
            move |prompt: String| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push(prompt);
                    Ok(all_tiers_result())
                }
            }
        };
        let verdicts =
            run_single_session_discovery(&repo(), Some(&runner), &SingleSessionOptions::default())
                .await
                .unwrap();
        assert_eq!(
            verdicts.iter().map(|v| v.tier).collect::<Vec<_>>(),
            Tier::ALL.to_vec()
        );
        assert_eq!(captured.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn single_session_runner_failure_propagates() {
        let runner = |_prompt: String| async move {
            Err::<IterationResult, _>(InvokeError::Other("session died".to_owned()))
        };
        let err =
            run_single_session_discovery(&repo(), Some(&runner), &SingleSessionOptions::default())
                .await
                .unwrap_err();
        assert_eq!(err, InvokeError::Other("session died".to_owned()));
    }

    // --- structural shape of the single session --------------------------------------

    #[test]
    fn tier_agents_declare_no_mcp_servers_and_a_turn_budget() {
        let agents = build_tier_agents(
            &repo(),
            None,
            DEFAULT_DISCOVERY_SUBAGENT_MAX_TURNS,
            DEFAULT_DISCOVERY_SUBAGENT_EFFORT,
        );
        let expected: BTreeSet<String> = Tier::ALL.iter().map(|t| tier_agent_key(*t)).collect();
        assert_eq!(agents.keys().cloned().collect::<BTreeSet<_>>(), expected);
        for (key, definition) in &agents {
            assert!(definition.mcp_servers.is_empty(), "{key}");
            assert!(definition.max_turns > 0);
            assert_eq!(definition.tools, vec!["Read", "Grep", "Glob"]);
            assert_eq!(definition.permission_mode, "bypassPermissions");
        }
        let value = serde_json::to_value(&agents["tier-3"]).unwrap();
        assert_eq!(value["mcpServers"], json!([]));
        assert_eq!(value["maxTurns"], json!(8));
        assert_eq!(value["effort"], json!("low"));
        assert_eq!(value["permissionMode"], json!("bypassPermissions"));
        assert!(value.get("model").is_none());
        assert_eq!(
            value["description"],
            json!("Autopilot relevance triage for tier 3 (Broken build / blocked pipeline).")
        );
        assert_eq!(
            value["prompt"],
            json!(tier_prompt(Tier::BrokenBuild, &repo()))
        );
    }

    #[test]
    fn single_session_configuration_carries_dispatch_tool_and_agents() {
        let options = SingleSessionOptions {
            model: Some("claude-fake-1".to_owned()),
            ..SingleSessionOptions::default()
        };
        let configuration = build_single_session_configuration(&repo(), &options);
        assert_eq!(configuration.agent_id, SUBAGENT_SESSION_AGENT_ID);
        assert_eq!(configuration.model_id.as_deref(), Some("claude-fake-1"));
        assert_eq!(configuration.max_turns, Some(DEFAULT_DISCOVERY_MAX_TURNS));
        assert_eq!(configuration.permission_policy, PermissionPolicy::Auto);
        let allowed = configuration.option_string_list("allowed_tools").unwrap();
        for tool in SUBAGENT_TOOL_NAMES {
            assert!(allowed.contains(&tool.to_owned()), "{tool}");
        }
        assert_eq!(allowed, vec!["Agent", "Task", "Read", "Grep", "Glob"]);
        let agents = configuration.option("agents").unwrap().as_object().unwrap();
        let expected: BTreeSet<String> = Tier::ALL.iter().map(|t| tier_agent_key(*t)).collect();
        assert_eq!(agents.keys().cloned().collect::<BTreeSet<_>>(), expected);
        assert_eq!(agents["tier-1"]["model"], json!("claude-fake-1"));
        // The sdk transport does not exist in the Rust runtime: nothing is
        // pinned so the adapter's cli default applies.
        assert!(configuration.option("transport").is_none());
    }

    // --- authoring -------------------------------------------------------------------

    #[tokio::test]
    async fn emitted_task_loads_and_carries_grader() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let mut entry = authored_entry(
            "test-parser",
            "pytest tests/test_existing.py",
            "tests/test_existing.py",
        );
        entry["task"]["tags"] = json!(["autopilot"]);
        entry["task"]["context"] = json!({});
        entry["creates_files"] = json!(["src/parser_test_helpers.py"]);
        entry["assumptions"] = json!(["assume pytest is the runner", 7]);
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert_eq!(result.emitted.len(), 1);
        assert!(result.dropped.is_empty());
        let emitted = &result.emitted[0];
        assert!(!emitted.task.graders.is_empty());
        assert_eq!(emitted.assumptions, vec!["assume pytest is the runner"]);
        assert_eq!(emitted.creates_files, vec!["src/parser_test_helpers.py"]);
        assert_eq!(emitted.grader_source, GraderSource::RepoCommand);
        assert_eq!(emitted.held_out_oracle_path, None);
        assert_eq!(emitted.task.tags, vec!["autopilot"]);

        // Round-trip through the authoritative validator.
        let reloaded = load_task_data(&serialize_task(&emitted.task), "round-trip").unwrap();
        assert!(!reloaded.graders.is_empty());
        assert_eq!(reloaded.id, "test-parser");
    }

    #[tokio::test]
    async fn emitted_task_carries_commit_before_done_constraint() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let mut entry = authored_entry(
            "test-parser",
            "pytest tests/test_existing.py",
            "tests/test_existing.py",
        );
        entry["task"]["context"] = json!({});
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        let constraints = &result.emitted[0].task.context.constraints;
        assert!(constraints.contains(&COMMIT_BEFORE_DONE_CONSTRAINT.to_owned()));
        let reloaded = load_task_data(&serialize_task(&result.emitted[0].task), "rt").unwrap();
        assert!(
            reloaded
                .context
                .constraints
                .contains(&COMMIT_BEFORE_DONE_CONSTRAINT.to_owned())
        );
    }

    #[tokio::test]
    async fn existing_commit_constraint_is_not_duplicated() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let agent_constraint = "Commit each change atomically before reporting done";
        let mut entry = authored_entry(
            "test-parser",
            "pytest tests/test_existing.py",
            "tests/test_existing.py",
        );
        entry["task"]["context"] = json!({"constraints": [agent_constraint]});
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert_eq!(
            result.emitted[0].task.context.constraints,
            vec![agent_constraint]
        );
    }

    #[tokio::test]
    async fn grader_target_must_pre_exist_in_repo() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let entry = authored_entry(
            "test-parser",
            "pytest tests/test_new.py",
            "tests/test_new.py",
        );
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert!(result.emitted.is_empty());
        assert_eq!(result.dropped.len(), 1);
        assert!(result.dropped[0].reason.contains("pre-exist"));
        assert_eq!(result.dropped[0].finding, coverage_finding());
    }

    #[tokio::test]
    async fn grader_naming_a_diff_created_file_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        std::fs::write(repo.join("tests/test_self.py"), "def test_x():\n    pass\n").unwrap();
        let mut entry = authored_entry(
            "test-parser",
            "pytest tests/test_self.py",
            "tests/test_self.py",
        );
        entry["creates_files"] = json!(["tests/test_self.py"]);
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert!(result.emitted.is_empty());
        assert!(result.dropped[0].reason.contains("self-attestation"));

        // The target-equals-created guard, with a grader that does not name
        // the file verbatim.
        let mut entry = authored_entry("test-parser", "make check", "tests/test_self.py");
        entry["creates_files"] = json!(["tests/test_self.py"]);
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert!(
            result.dropped[0]
                .reason
                .contains("target is a file this task's own diff")
        );
    }

    #[tokio::test]
    async fn held_out_oracle_is_an_acceptable_authoritative_source() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let oracle = tmp.path().join("held-out").join("oracle.py");
        std::fs::create_dir_all(oracle.parent().unwrap()).unwrap();
        std::fs::write(&oracle, "import sys\nsys.exit(0)\n").unwrap();
        let run = format!("python3 {}", oracle.display());
        let mut entry = authored_entry("test-parser", &run, &oracle.to_string_lossy());
        entry["grader_source"] = json!(GRADER_SOURCE_HELD_OUT);
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert_eq!(result.emitted.len(), 1);
        assert_eq!(result.emitted[0].grader_source, GraderSource::HeldOutOracle);
        assert_eq!(result.emitted[0].held_out_oracle_path, Some(oracle.clone()));

        // A relative or missing oracle path is rejected.
        let mut entry = authored_entry("test-parser", "python3 oracle.py", "oracle.py");
        entry["grader_source"] = json!(GRADER_SOURCE_HELD_OUT);
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert!(result.dropped[0].reason.contains("existing absolute path"));
    }

    #[tokio::test]
    async fn grader_less_and_invalid_tasks_are_dropped_not_emitted() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let graderless = json!({
            "task": {"id": "noop", "goal": "Do a thing.", "graders": []},
            "authoritative_grader": "pytest tests/test_existing.py",
            "grader_source": GRADER_SOURCE_REPO_COMMAND,
            "grader_target": "tests/test_existing.py",
        });
        let response = fenced(&json!({"tasks": [graderless], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert!(result.emitted.is_empty());
        assert_eq!(result.dropped.len(), 1);
        assert!(result.dropped[0].reason.contains("no grader"));

        let invalid = json!({"task": {"id": "x"}, "authoritative_grader": "y"});
        let response = fenced(&json!({"tasks": [invalid, "junk", {"nope": 1}], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert!(result.emitted.is_empty());
        assert!(result.dropped[0].reason.contains("failed validation"));
    }

    #[tokio::test]
    async fn finding_with_no_expressible_grader_is_dropped_with_reason() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let response = fenced(&json!({
            "tasks": [],
            "dropped": "no out-of-band check exists for this cosmetic finding",
        }));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert!(result.emitted.is_empty());
        assert_eq!(result.dropped.len(), 1);
        assert!(result.dropped[0].reason.contains("cosmetic"));

        let response = fenced(&json!({"tasks": "not a list"}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert_eq!(result.dropped[0].reason, "no grader-bearing task authored");
    }

    #[tokio::test]
    async fn authoritative_grader_must_be_a_command_grader_on_the_task() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let mut entry = authored_entry(
            "test-parser",
            "pytest tests/test_existing.py",
            "tests/test_existing.py",
        );
        entry["authoritative_grader"] = json!("pytest tests/other.py");
        let response = fenced(&json!({"tasks": [entry], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert!(result.emitted.is_empty());
        assert!(
            result.dropped[0]
                .reason
                .contains("authoritative grader is not a command grader")
        );

        let mut entry = authored_entry(
            "test-parser",
            "pytest tests/test_existing.py",
            "tests/test_existing.py",
        );
        entry["authoritative_grader"] = json!("  ");
        let response = fenced(&json!({"tasks": [entry]}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert_eq!(
            result.dropped[0].reason,
            "authored task names no authoritative grader"
        );

        let mut entry = authored_entry(
            "test-parser",
            "pytest tests/test_existing.py",
            "tests/test_existing.py",
        );
        entry["grader_source"] = json!("vibes");
        let response = fenced(&json!({"tasks": [entry]}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert_eq!(result.dropped[0].reason, "unknown grader_source 'vibes'");

        let mut entry = authored_entry(
            "test-parser",
            "pytest tests/test_existing.py",
            "tests/test_existing.py",
        );
        entry["grader_target"] = json!("");
        let response = fenced(&json!({"tasks": [entry]}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert_eq!(
            result.dropped[0].reason,
            "authoritative grader names no pre-existing target"
        );
    }

    #[tokio::test]
    async fn unparseable_or_failing_authoring_drops_the_finding() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let result = author_finding(&coverage_finding(), &repo, &scripted("no json here")).await;
        assert!(result.emitted.is_empty());
        assert!(result.dropped[0].reason.contains("unparseable"));

        let failing = |_prompt: String| async move {
            Err::<String, _>(InvokeError::Other("agent exploded".to_owned()))
        };
        let result = author_finding(&coverage_finding(), &repo, &failing).await;
        assert_eq!(
            result.dropped[0].reason,
            "authoring agent error: agent exploded"
        );
    }

    #[tokio::test]
    async fn a_single_finding_may_compile_to_multiple_tasks() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        std::fs::write(
            repo.join("tests/test_other.py"),
            "def test_y():\n    pass\n",
        )
        .unwrap();
        let entry_a = authored_entry(
            "task-a",
            "pytest tests/test_existing.py",
            "tests/test_existing.py",
        );
        let mut entry_b = authored_entry(
            "task-b",
            "pytest tests/test_other.py",
            "tests/test_other.py",
        );
        entry_b["task"]["prerequisites"] = json!(["task-a"]);
        let response = fenced(&json!({"tasks": [entry_a, entry_b], "dropped": ""}));
        let result = author_finding(&coverage_finding(), &repo, &scripted(response)).await;
        assert_eq!(result.emitted.len(), 2);
        assert!(result.emitted.iter().all(|e| !e.task.graders.is_empty()));
        assert!(
            result
                .emitted
                .iter()
                .all(|e| repo.join(&e.grader_target).exists())
        );
        assert_eq!(result.emitted[1].prerequisites, vec!["task-a"]);
        assert!(result.emitted[0].prerequisites.is_empty());
    }

    #[tokio::test]
    async fn author_findings_aggregates_in_order() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let invoker = |prompt: String| async move {
            if prompt.contains("Finding (Tier 6") {
                Ok(fenced(&json!({"tasks": [authored_entry(
                    "task-6", "pytest tests/test_existing.py", "tests/test_existing.py"
                )]})))
            } else {
                Ok(fenced(&json!({"tasks": [], "dropped": "nothing to grade"})))
            }
        };
        let findings = [
            coverage_finding(),
            Finding::new("t11-x", Tier::Polish, "tidy"),
        ];
        let result = author_findings(&findings, &repo, &invoker).await;
        assert_eq!(result.emitted.len(), 1);
        assert_eq!(result.emitted[0].task.id, "task-6");
        assert_eq!(result.dropped.len(), 1);
        assert_eq!(result.dropped[0].reason, "nothing to grade");
    }

    // --- conflict keys ----------------------------------------------------------------

    #[test]
    fn keys_derive_from_grader_target_and_creates_files() {
        assert_eq!(
            conflict_keys_for(&emitted("t1", "crates/infrared-feed/src/tycho.rs", &[])),
            vec!["crates/infrared-feed/src/tycho.rs"]
        );
        assert_eq!(
            conflict_keys_for(&emitted(
                "t1",
                "crates/a/src/lib.rs",
                &["crates/b/src/new.rs", "crates/a/src/lib.rs", "Cargo.toml"],
            )),
            vec!["crates/a/src/lib.rs", "crates/b/src/new.rs"]
        );
        assert!(conflict_keys_for(&emitted("t1", "Cargo.toml", &[])).is_empty());
    }

    #[test]
    fn emitted_file_carries_conflict_keys_into_work_item() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let e = emitted("t1", "crates/infrared-feed/src/tycho.rs", &[]);
        let path = emit_emitted_task(&e, &breakdown(), &tasks_dir, AUTOPILOT_PHASE)
            .unwrap()
            .unwrap();
        assert_eq!(
            path,
            tasks_dir
                .join("active")
                .join(AUTOPILOT_PHASE)
                .join("t1.json")
        );
        let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].conflict_keys,
            BTreeSet::from(["crates/infrared-feed/src/tycho.rs".to_owned()])
        );
        assert_eq!(items[0].task.id, "t1");
    }

    #[test]
    fn coarse_target_emits_no_conflict_keys_field() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let e = emitted("t1", "Cargo.toml", &[]);
        let path = emit_emitted_task(&e, &breakdown(), &tasks_dir, AUTOPILOT_PHASE)
            .unwrap()
            .unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw.get("conflict_keys").is_none());
        let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();
        assert!(items[0].conflict_keys.is_empty());
    }

    #[test]
    fn swarm_tasks_on_same_file_share_a_conflict_key() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let target = "crates/infrared-feed/src/tycho.rs";
        for i in 0..3 {
            emit_emitted_task(
                &emitted(&format!("dedupe-{i}"), target, &[]),
                &breakdown(),
                &tasks_dir,
                AUTOPILOT_PHASE,
            )
            .unwrap();
        }
        let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();
        assert_eq!(items.len(), 3);
        let keysets: BTreeSet<BTreeSet<String>> =
            items.iter().map(|i| i.conflict_keys.clone()).collect();
        assert_eq!(
            keysets,
            BTreeSet::from([BTreeSet::from([target.to_owned()])])
        );
    }

    #[test]
    fn emitted_file_records_priority_provenance_and_prerequisites_idempotently() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let mut e = emitted("t1", "crates/a/src/lib.rs", &[]);
        e.prerequisites = vec!["t0".to_owned()];
        e.assumptions = vec!["assume x".to_owned()];
        let bd = ScoreBreakdown {
            final_score: 42.6,
            ..breakdown()
        };
        let path = emit_emitted_task(&e, &bd, &tasks_dir, AUTOPILOT_PHASE)
            .unwrap()
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("}\n"));
        let raw: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(raw["priority"], json!(43));
        assert_eq!(raw["prerequisites"], json!(["t0"]));
        assert_eq!(raw["autopilot"]["finding_id"], json!("find-t1"));
        assert_eq!(raw["autopilot"]["tier"], json!(6));
        assert_eq!(raw["autopilot"]["final"], json!(42.6));
        assert_eq!(raw["autopilot"]["grader_source"], json!("repo_command"));
        assert_eq!(raw["autopilot"]["assumptions"], json!(["assume x"]));
        assert_eq!(raw["autopilot"]["preemptive"], json!(false));
        // The core loader ignores the orchestration keys.
        assert_eq!(load_task_data(&raw, "emitted").unwrap().id, "t1");
        // Re-emitting the same task id never overwrites.
        std::fs::write(&path, "sentinel").unwrap();
        assert_eq!(
            emit_emitted_task(&e, &bd, &tasks_dir, AUTOPILOT_PHASE).unwrap(),
            None
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sentinel");
        let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();
        assert_eq!(items.len(), 0, "the sentinel is not a loadable task");
    }

    // --- the refill pass ---------------------------------------------------------------

    /// An authoring invoker that compiles every finding into one task named
    /// after the finding's tier, graded by the seeded repo check.
    fn tier_task_author() -> impl AutopilotInvoker {
        |prompt: String| async move {
            let start = prompt.find("Finding (Tier ").unwrap() + "Finding (Tier ".len();
            let tier: String = prompt[start..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            Ok(fenced(&json!({"tasks": [authored_entry(
                &format!("task-t{tier}"), "pytest tests/test_existing.py", "tests/test_existing.py"
            )]})))
        }
    }

    fn task_ids_in(tasks_dir: &Path) -> Vec<String> {
        let mut ids: Vec<String> = DirectoryWorkSource::new(tasks_dir)
            .list_work()
            .unwrap()
            .into_iter()
            .map(|i| i.task.id)
            .collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn refill_pass_emits_up_to_target_depth_from_actionable_findings() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let tasks_dir = tmp.path().join("tasks");
        let discovery = library_invoker(Arc::new(Mutex::new(Vec::new())));
        let author = tier_task_author();
        let control = InMemoryClaimStore::new();
        let result = run_refill_pass(RefillPassOptions {
            target_depth: 3,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            control: Some(&control),
            ..RefillPassOptions::new(&tasks_dir, &repo)
        })
        .await
        .unwrap();
        assert_eq!(result.emitted_count(), 3);
        assert_eq!(result.emitted.len(), 3);
        assert_eq!(result.queue_depth_before, 0);
        assert_eq!(result.target_depth, 3);
        assert_eq!(result.landing, DEFAULT_LANDING);
        assert_eq!(
            result.reason,
            "emitted 3 task(s) to fill queue from 0 toward target 3"
        );
        assert_eq!(
            result
                .relevant_tiers
                .iter()
                .map(|t| t.value())
                .collect::<Vec<_>>(),
            vec![5, 6, 7, 8, 9, 10, 11]
        );
        assert_eq!(
            result
                .not_relevant_tiers
                .iter()
                .map(|t| t.value())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        // The three highest-scoring findings (equal axes, so highest tier
        // weight first) were authored, in sequence order.
        assert_eq!(
            task_ids_in(&tasks_dir),
            vec!["task-t5", "task-t6", "task-t7"]
        );
        assert!(result.emitted_paths.iter().all(|p| p.is_file()));
        assert!(control.list_stop_events().unwrap().is_empty());

        // A second pass sees depth 3 at target 3 and short-circuits without
        // touching the agents.
        let panicking = |_prompt: String| async move { panic!("must not be invoked") };
        let again = run_refill_pass(RefillPassOptions {
            target_depth: 3,
            discovery_invoker: Some(&panicking),
            authoring_invoker: Some(&panicking),
            control: Some(&control),
            ..RefillPassOptions::new(&tasks_dir, &repo)
        })
        .await
        .unwrap();
        assert_eq!(again.emitted_count(), 0);
        assert_eq!(again.queue_depth_before, 3);
        assert_eq!(
            again.reason,
            "queue depth 3 already at or above target 3; nothing emitted"
        );
        assert!(control.list_stop_events().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refill_pass_idles_on_a_clean_repo_and_witnesses_the_no_op_cycle() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let tasks_dir = tmp.path().join("tasks");
        let nothing = |_prompt: String| async move {
            Ok(fenced(
                &json!({"relevant": false, "reason": "clean", "findings": []}),
            ))
        };
        let panicking = |_prompt: String| async move { panic!("nothing to author") };
        let control = InMemoryClaimStore::new();
        let at = DateTime::parse_from_rfc3339("2026-10-12T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = move || at;
        let result = run_refill_pass(RefillPassOptions {
            target_depth: 5,
            discovery_invoker: Some(&nothing),
            authoring_invoker: Some(&panicking),
            control: Some(&control),
            now: Some(&clock),
            ..RefillPassOptions::new(&tasks_dir, &repo)
        })
        .await
        .unwrap();
        assert_eq!(result.emitted_count(), 0);
        assert!(result.relevant_tiers.is_empty());
        assert_eq!(result.not_relevant_tiers.len(), 11);
        assert_eq!(
            result.reason,
            "no actionable findings this cycle; idling without emitting"
        );
        assert!(!tasks_dir.exists());
        let events = control.list_stop_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, STOP_NO_OP_CYCLE);
        assert_eq!(events[0].subject, tasks_dir.to_string_lossy());
        assert_eq!(
            events[0].detail,
            "no actionable findings this cycle; idling without emitting (observed queue depth \
             0, target 5)"
        );
        assert_eq!(events[0].occurred_at, at);
    }

    #[tokio::test]
    async fn refill_pass_records_no_op_when_nothing_is_authorable() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let tasks_dir = tmp.path().join("tasks");
        let discovery = library_invoker(Arc::new(Mutex::new(Vec::new())));
        let dropper = scripted(fenced(&json!({"tasks": [], "dropped": "ungradeable"})));
        let control = InMemoryClaimStore::new();
        let depth = |_: &Path| Ok(2usize);
        let result = run_refill_pass(RefillPassOptions {
            target_depth: 4,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&dropper),
            control: Some(&control),
            queue_depth: Some(&depth),
            ..RefillPassOptions::new(&tasks_dir, &repo)
        })
        .await
        .unwrap();
        assert_eq!(result.emitted_count(), 0);
        assert_eq!(result.queue_depth_before, 2);
        // Every actionable finding was tried (7 relevant tiers) and dropped.
        assert_eq!(result.dropped.len(), 7);
        assert!(result.dropped.iter().all(|d| d.reason == "ungradeable"));
        assert_eq!(
            result.reason,
            "no grader-bearing task could be authored this cycle"
        );
        let events = control.list_stop_events().unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            events[0]
                .detail
                .ends_with("(observed queue depth 2, target 4)")
        );
    }

    #[tokio::test]
    async fn refill_pass_propagates_a_failed_discovery_session() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let runner = |_prompt: String| async move {
            Err::<IterationResult, _>(InvokeError::DeadlineExceeded(DeadlineExceeded {
                ceiling: Duration::from_millis(50),
                elapsed: Duration::from_millis(51),
            }))
        };
        let panicking = |_prompt: String| async move { panic!("nothing to author") };
        let err = run_refill_pass(RefillPassOptions {
            discovery_session_runner: Some(&runner),
            authoring_invoker: Some(&panicking),
            ..RefillPassOptions::new(tmp.path().join("tasks"), &repo)
        })
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            RefillError::Discovery(InvokeError::DeadlineExceeded(d)) if d.ceiling_seconds() == 0.05
        ));
    }

    // --- actionable queue depth ---------------------------------------------------------

    fn write_task(phase: &Path, task_id: &str) {
        std::fs::create_dir_all(phase).unwrap();
        std::fs::write(
            phase.join(format!("{task_id}.json")),
            json!({
                "id": task_id,
                "goal": format!("Goal for {task_id}."),
                "graders": [{"type": "command", "run": "true"}],
            })
            .to_string(),
        )
        .unwrap();
    }

    fn seed(store: &InMemoryStore, task_id: &str, terminal: Option<Status>) {
        let mut lc = Lifecycle::new(task_id);
        lc.run_id = format!("run-{task_id}");
        lc.transition_to(Status::Ready, "", None).unwrap();
        lc.transition_to(Status::Running, "", None).unwrap();
        match terminal {
            Some(Status::Done) => {
                lc.transition_to(Status::Validating, "", None).unwrap();
                lc.transition_to(Status::Done, "", None).unwrap();
            }
            Some(Status::Failed) => lc.transition_to(Status::Failed, "boom", None).unwrap(),
            Some(status) => lc.transition_to(status, "", None).unwrap(),
            None => {}
        }
        store.create_lifecycle(&lc).unwrap();
    }

    #[test]
    fn actionable_depth_excludes_terminal_tasks() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("autopilot");
        for tid in ["fresh", "done", "failed", "running", "interrupted"] {
            write_task(&phase, tid);
        }
        let store = InMemoryStore::new();
        seed(&store, "done", Some(Status::Done));
        seed(&store, "failed", Some(Status::Failed));
        seed(&store, "running", None);
        seed(&store, "interrupted", Some(Status::Interrupted));
        assert_eq!(directory_queue_depth(tmp.path()).unwrap(), 5);
        assert_eq!(actionable_queue_depth(tmp.path(), &store).unwrap(), 3);
    }

    #[test]
    fn actionable_depth_drops_to_zero_when_all_terminal() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("autopilot");
        write_task(&phase, "landed");
        write_task(&phase, "dead");
        let store = InMemoryStore::new();
        seed(&store, "landed", Some(Status::Done));
        seed(&store, "dead", Some(Status::Failed));
        assert_eq!(directory_queue_depth(tmp.path()).unwrap(), 2);
        assert_eq!(actionable_queue_depth(tmp.path(), &store).unwrap(), 0);
    }

    struct BrokenStore;

    impl LifecycleStore for BrokenStore {
        fn create_lifecycle(&self, _lifecycle: &Lifecycle) -> Result<(), StoreError> {
            Ok(())
        }
        fn update_lifecycle(
            &self,
            _lifecycle: &Lifecycle,
            _expected: u64,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn load_lifecycle(&self, _run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
            Ok(None)
        }
        fn list_lifecycles(&self, _filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
            Err(StoreError::Io(std::io::Error::other("database is locked")))
        }
        fn summarize_spend(
            &self,
            _window: &flywheel_core::store::SpendWindow,
        ) -> Result<flywheel_core::store::SpendSummary, StoreError> {
            Ok(flywheel_core::store::SpendSummary::default())
        }
    }

    #[test]
    fn actionable_depth_survives_store_errors() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("active").join("autopilot");
        write_task(&phase, "a");
        write_task(&phase, "b");
        assert_eq!(actionable_queue_depth(tmp.path(), &BrokenStore).unwrap(), 2);
    }

    // --- the agents-backed seams under the wall-clock deadline --------------------------

    const FAKE_PRELUDE: &str = r#"#!/bin/sh
IFS= read -r FIRST_LINE || true
emit() { printf '%s\n' "$1"; }
init() { emit '{"type":"system","subtype":"init","session_id":"sess-auto","model":"claude-fake-1"}'; }
text() { emit '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$1"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'; }
result() { emit '{"type":"result","subtype":"success","is_error":false,"result":"'"$1"'","num_turns":'"$2"',"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'; }
"#;

    /// A fake claude-code CLI: reads the prompt line, then runs `body`.
    fn fake_agent(dir: &Path, name: &str, body: &str) -> Vec<String> {
        let path = dir.join(name);
        std::fs::write(&path, format!("{FAKE_PRELUDE}{body}\n")).unwrap();
        vec!["/bin/sh".to_owned(), path.to_string_lossy().into_owned()]
    }

    const TEXT_BODY: &str = r#"init
text "scripted agent text"
result "scripted agent text" 1"#;

    const SLEEPING_BODY: &str = r#"init
sleep 60"#;

    /// Records a graceful SIGTERM, and leaves a grandchild that ignores
    /// SIGTERM (an MCP server / Bash tool child stand-in).
    const PROCESS_TREE_BODY: &str = r#"init
trap 'echo graceful > terminated.marker; exit 143' TERM
(trap '' TERM; exec sleep 300) &
echo $! > grandchild.pid
sleep 60 &
wait $!"#;

    /// Gone, or a zombie awaiting its reaper (Linux `/proc`).
    fn process_gone(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.split_whitespace().next() == Some("Z")),
            Err(_) => true,
        }
    }

    /// The deadline reached the agent through the runtime (a graceful
    /// SIGTERM the fake recorded, not a bare drop) and the whole tree,
    /// SIGTERM-ignoring grandchild included, is dead.
    async fn assert_tree_terminated(dir: &Path) {
        assert!(
            dir.join("terminated.marker").exists(),
            "the runtime never cancelled the agent"
        );
        let pid = std::fs::read_to_string(dir.join("grandchild.pid")).unwrap();
        let pid = pid.trim();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !process_gone(pid) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(process_gone(pid), "grandchild {pid} outlived the deadline");
    }

    const STREAMING_BODY: &str = r#"init
while true; do text "still streaming"; sleep 0.01; done"#;

    const WALL_BUDGET: Duration = Duration::from_secs(5);

    #[test]
    fn autopilot_agent_deadline_default_ceiling_is_non_null() {
        let ceiling = DeadlineConfig::default()
            .for_class(DeadlineClass::AutopilotAgent)
            .unwrap();
        assert!(ceiling > 0.0 && ceiling.is_finite());
        assert_eq!(ceiling, DEFAULT_AUTOPILOT_AGENT_SECONDS);
        assert_eq!(
            RepoInvokerOptions::default().deadline_seconds,
            Some(DEFAULT_AUTOPILOT_AGENT_SECONDS)
        );
        assert_eq!(
            SingleSessionOptions::default().deadline_seconds,
            Some(DEFAULT_AUTOPILOT_AGENT_SECONDS)
        );
    }

    #[test]
    fn repo_invoker_configuration_mirrors_the_policy_opt_in() {
        let options = RepoInvokerOptions {
            model: Some("m".to_owned()),
            max_turns: 7,
            command_override: Some(vec!["x".to_owned()]),
            ..RepoInvokerOptions::from_policy(Some("codex"), Some("cli"))
        };
        let invoker = build_repo_invoker("/repo", options);
        let configuration = invoker.configuration();
        assert_eq!(configuration.agent_id, "codex");
        assert_eq!(configuration.model_id.as_deref(), Some("m"));
        assert_eq!(configuration.max_turns, Some(7));
        assert_eq!(configuration.permission_policy, PermissionPolicy::Auto);
        assert_eq!(configuration.option_str("transport"), Some("cli"));
        assert_eq!(configuration.command_override, Some(vec!["x".to_owned()]));
        assert_eq!(invoker.repo_root(), Path::new("/repo"));
        assert_eq!(
            invoker.deadline_seconds(),
            Some(DEFAULT_AUTOPILOT_AGENT_SECONDS)
        );
        let default = build_repo_invoker("/repo", RepoInvokerOptions::from_policy(None, None));
        assert_eq!(default.configuration().agent_id, DEFAULT_AGENT_ID);
        assert!(default.configuration().option("transport").is_none());
    }

    #[tokio::test]
    async fn agents_invoker_returns_agent_text() {
        let tmp = TempDir::new().unwrap();
        let invoker = build_repo_invoker(
            tmp.path(),
            RepoInvokerOptions {
                command_override: Some(fake_agent(tmp.path(), "text.sh", TEXT_BODY)),
                ..RepoInvokerOptions::default()
            },
        );
        assert_eq!(
            invoker.invoke("prompt").await.unwrap(),
            "scripted agent text"
        );
    }

    #[tokio::test]
    async fn agents_invoker_start_failure_is_a_typed_error() {
        let tmp = TempDir::new().unwrap();
        let invoker = build_repo_invoker(
            tmp.path(),
            RepoInvokerOptions {
                agent_id: "no-such-agent".to_owned(),
                ..RepoInvokerOptions::default()
            },
        );
        let err = invoker.invoke("prompt").await.unwrap_err();
        assert!(
            matches!(err, InvokeError::Start(AgentError::UnknownAgent(_))),
            "{err:?}"
        );
        assert!(err.to_string().starts_with("AgentError: "));
    }

    #[tokio::test]
    async fn autopilot_agent_deadline_cancels_authoring_stall() {
        let tmp = TempDir::new().unwrap();
        let invoker = build_repo_invoker(
            tmp.path(),
            RepoInvokerOptions {
                deadline_seconds: Some(0.2),
                command_override: Some(fake_agent(tmp.path(), "sleep.sh", SLEEPING_BODY)),
                ..RepoInvokerOptions::default()
            },
        );
        let started = Instant::now();
        let result = author_findings(&[coverage_finding()], tmp.path(), &invoker).await;
        assert!(started.elapsed() < WALL_BUDGET);
        assert!(result.emitted.is_empty());
        assert_eq!(result.dropped.len(), 1);
        assert!(
            result.dropped[0]
                .reason
                .starts_with("authoring agent error: DeadlineExceeded: deadline of 0.2s exceeded"),
            "{}",
            result.dropped[0].reason
        );
    }

    #[tokio::test]
    async fn autopilot_agent_deadline_cancels_discovery_stall() {
        let tmp = TempDir::new().unwrap();
        let runner = build_single_session_runner(
            tmp.path(),
            &SingleSessionOptions {
                deadline_seconds: Some(0.2),
                command_override: Some(fake_agent(tmp.path(), "sleep.sh", SLEEPING_BODY)),
                ..SingleSessionOptions::default()
            },
        );
        let started = Instant::now();
        let err = run_single_session_discovery(
            tmp.path(),
            Some(&runner),
            &SingleSessionOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < WALL_BUDGET);
        match err {
            InvokeError::DeadlineExceeded(exceeded) => assert_eq!(exceeded.ceiling_seconds(), 0.2),
            other => panic!("expected DeadlineExceeded, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn autopilot_invoker_deadline_terminates_the_agent_process_tree() {
        let tmp = TempDir::new().unwrap();
        let invoker = build_repo_invoker(
            tmp.path(),
            RepoInvokerOptions {
                deadline_seconds: Some(0.5),
                command_override: Some(fake_agent(tmp.path(), "tree.sh", PROCESS_TREE_BODY)),
                ..RepoInvokerOptions::default()
            },
        );
        let err = invoker.invoke("prompt").await.unwrap_err();
        match err {
            InvokeError::DeadlineExceeded(exceeded) => assert_eq!(exceeded.ceiling_seconds(), 0.5),
            other => panic!("expected DeadlineExceeded, got {other:?}"),
        }
        assert_tree_terminated(tmp.path()).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn autopilot_discovery_deadline_terminates_the_agent_process_tree() {
        let tmp = TempDir::new().unwrap();
        let runner = build_single_session_runner(
            tmp.path(),
            &SingleSessionOptions {
                deadline_seconds: Some(0.5),
                command_override: Some(fake_agent(tmp.path(), "tree.sh", PROCESS_TREE_BODY)),
                ..SingleSessionOptions::default()
            },
        );
        let err = runner.run("prompt").await.unwrap_err();
        assert!(matches!(err, InvokeError::DeadlineExceeded(_)), "{err:?}");
        assert_tree_terminated(tmp.path()).await;
    }

    #[tokio::test]
    async fn autopilot_agent_deadline_cuts_off_a_forever_streaming_session() {
        let tmp = TempDir::new().unwrap();
        let runner = build_single_session_runner(
            tmp.path(),
            &SingleSessionOptions {
                deadline_seconds: Some(0.2),
                command_override: Some(fake_agent(tmp.path(), "stream.sh", STREAMING_BODY)),
                ..SingleSessionOptions::default()
            },
        );
        let started = Instant::now();
        let err = runner.run("prompt").await.unwrap_err();
        assert!(started.elapsed() < WALL_BUDGET);
        assert!(matches!(err, InvokeError::DeadlineExceeded(_)), "{err:?}");
    }

    #[tokio::test]
    async fn autopilot_agent_deadline_normal_completion_not_tripped() {
        let tmp = TempDir::new().unwrap();
        let script = fake_agent(tmp.path(), "text.sh", TEXT_BODY);
        let runner = build_single_session_runner(
            tmp.path(),
            &SingleSessionOptions {
                command_override: Some(script.clone()),
                ..SingleSessionOptions::default()
            },
        );
        let result = runner.run("prompt").await.unwrap();
        assert_eq!(result.transcript, "scripted agent text");
        assert_eq!(result.signals.session_id.as_deref(), Some("sess-auto"));
        // Collected over a session with no subagent dispatch: 11 error
        // verdicts, never a crash.
        let verdicts = run_single_session_discovery(
            tmp.path(),
            Some(&runner),
            &SingleSessionOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(verdicts.len(), 11);

        let invoker = build_repo_invoker(
            tmp.path(),
            RepoInvokerOptions {
                command_override: Some(script),
                ..RepoInvokerOptions::default()
            },
        );
        assert_eq!(
            invoker.invoke("prompt").await.unwrap(),
            "scripted agent text"
        );
    }

    #[tokio::test]
    async fn unbounded_deadline_is_a_plain_await() {
        let value = bounded(async { 7 }, None).await.unwrap();
        assert_eq!(value, 7);
        let value = bounded(async { 8 }, Some(1.0)).await.unwrap();
        assert_eq!(value, 8);
        let err = bounded(std::future::pending::<()>(), Some(0.01))
            .await
            .unwrap_err();
        assert_eq!(err.ceiling_seconds(), 0.01);
    }

    // --- cycle cost controls: pre-filter, digest, verdict cache, fan-out --------------

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `seed_repo` committed to a fresh git repository.
    fn git_seed_repo(tmp: &TempDir) -> PathBuf {
        let repo = seed_repo(tmp);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        repo
    }

    fn values(tiers: &[Tier]) -> Vec<u8> {
        tiers.iter().map(|t| t.value()).collect()
    }

    fn seen_sorted(seen: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
        let mut out = seen.lock().unwrap().clone();
        out.sort_unstable();
        out
    }

    const SUBSET_DISPATCHER_PROMPT: &str = "You are autopilot's discovery dispatcher for the repository at /repo.\n\nTwo tier-relevance subagents are registered: tier-5, tier-10.\n\nUse the Agent tool (a.k.a. Task) to dispatch EVERY one of these 2 subagents exactly once, each with subagent_type set to its key. Tell each subagent to read the repository, judge whether its tier is relevant to THIS codebase, and return its verdict as the fenced JSON block its own instructions define. Dispatch all 2 regardless of your own opinion -- do not skip, merge, or pre-judge any tier. The per-tier tool-results are read directly, so you need not restate them.\n\nAfter all 2 subagents return, emit exactly one final fenced JSON block aggregating every verdict as a safety net:\n```json\n{\"verdicts\": [{\"tier\": 1, \"relevant\": false, \"reason\": \"...\", \"findings\": []}]}\n```";

    #[test]
    fn dispatcher_prompt_for_a_subset_names_only_those_tiers() {
        assert_eq!(
            orchestrator_prompt_for(&repo(), &Tier::ALL),
            PY_ORCHESTRATOR_PROMPT
        );
        assert_eq!(
            orchestrator_prompt_for(&repo(), &[Tier::CoreFeatureWork, Tier::Documentation]),
            SUBSET_DISPATCHER_PROMPT
        );
    }

    #[test]
    fn digest_rides_after_the_unchanged_tier_and_authoring_prompts() {
        let tier = prompt_with_digest(tier_prompt(Tier::ProductionDown, &repo()), Some("DIGEST"));
        assert_eq!(
            tier,
            format!(
                "{PY_TIER1_PROMPT}\n\nSHARED REPOSITORY DIGEST (built once this cycle by \
                 autopilot; use it for the repo's layout, manifests, and recent history instead \
                 of re-surveying them, and open files only to confirm specific evidence):\n\
                 <repo-digest>\nDIGEST\n</repo-digest>"
            )
        );
        let mut f = coverage_finding();
        f.evidence.push("src/parser.py:40".to_owned());
        let authoring = prompt_with_digest(authoring_prompt(&f, &repo()), Some("DIGEST"));
        assert!(authoring.starts_with(PY_AUTHORING_PROMPT));
        assert!(authoring.ends_with("<repo-digest>\nDIGEST\n</repo-digest>"));
    }

    #[test]
    fn single_session_configuration_registers_only_dispatched_tiers_with_the_digest() {
        let options = SingleSessionOptions {
            tiers: vec![Tier::TestCoverage, Tier::Documentation],
            digest: Some("DIGEST".to_owned()),
            ..SingleSessionOptions::default()
        };
        let config = build_single_session_configuration(&repo(), &options);
        let agents = config.adapter_options["agents"].as_object().unwrap();
        assert_eq!(
            agents.keys().cloned().collect::<Vec<_>>(),
            vec!["tier-10", "tier-6"]
        );
        for agent in agents.values() {
            assert!(
                agent["prompt"]
                    .as_str()
                    .unwrap()
                    .ends_with("DIGEST\n</repo-digest>")
            );
        }
    }

    #[test]
    fn subset_collection_ignores_results_for_undispatched_tiers() {
        let collection =
            collect_tier_verdicts_for(&all_tiers_result(), &[Tier::BrokenBuild, Tier::Polish]);
        assert_eq!(
            collection
                .verdicts
                .iter()
                .map(|v| v.tier.value())
                .collect::<Vec<_>>(),
            vec![3, 11]
        );
        assert_eq!(collection.source, CollectSource::ToolResult);
        let missing = collect_tier_verdicts_for(&session_result(Vec::new(), ""), &[Tier::Polish]);
        assert!(missing.verdicts[0].errored);
        assert!(!collection.verdicts[0].errored);
    }

    #[tokio::test]
    async fn discovery_for_a_subset_dispatches_only_those_tiers() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let invoker = library_invoker(seen.clone());
        let verdicts = run_discovery_for(
            &repo(),
            DiscoveryDriver::PerTier(&invoker),
            &[Tier::Polish, Tier::BrokenBuild],
            Some("DIGEST"),
        )
        .await
        .unwrap();
        assert_eq!(
            verdicts.iter().map(|v| v.tier.value()).collect::<Vec<_>>(),
            vec![3, 11]
        );
        assert_eq!(seen_sorted(&seen), vec![3, 11]);
        let none = run_discovery_for(&repo(), DiscoveryDriver::PerTier(&invoker), &[], None)
            .await
            .unwrap();
        assert!(none.is_empty());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn refill_pass_prefilters_structurally_absent_tiers_and_records_why() {
        let tmp = TempDir::new().unwrap();
        // Only a Python test file: no deployment config, no build manifest
        // or CI -- tiers 1 and 3 cannot apply.
        let repo = seed_repo(&tmp);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let discovery = library_invoker(seen.clone());
        let author = tier_task_author();
        let result = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            ..RefillPassOptions::new(tmp.path().join("tasks"), &repo)
        })
        .await
        .unwrap();
        assert_eq!(seen_sorted(&seen), vec![2, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(
            result
                .prefiltered
                .iter()
                .map(|s| s.tier.value())
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(
            result
                .prefiltered
                .iter()
                .all(|s| s.reason.starts_with("structural pre-filter: "))
        );
        assert_eq!(values(&result.not_relevant_tiers), vec![1, 2, 3, 4]);
        assert_eq!(
            result.discovery_cache,
            DiscoveryCacheStatus::Disabled("no cache path configured".to_owned())
        );
        assert_eq!(result.emitted_count(), 1);
    }

    #[tokio::test]
    async fn refill_pass_injects_one_shared_digest_into_discovery_and_authoring() {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let prompts = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = prompts.clone();
        let discovery = move |prompt: String| {
            let recorded = recorded.clone();
            async move {
                let tier = tier_in_prompt(&prompt);
                recorded.lock().unwrap().push(prompt);
                Ok(library_response(tier))
            }
        };
        let authored = Arc::new(Mutex::new(Vec::<String>::new()));
        let author_log = authored.clone();
        let author = move |prompt: String| {
            let author_log = author_log.clone();
            async move {
                author_log.lock().unwrap().push(prompt);
                Ok(fenced(&json!({"tasks": [authored_entry(
                    "task-x", "pytest tests/test_existing.py", "tests/test_existing.py"
                )]})))
            }
        };
        run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            ..RefillPassOptions::new(tmp.path().join("tasks"), &repo)
        })
        .await
        .unwrap();
        let digest_of = |prompt: &str| -> String {
            let start = prompt.find("<repo-digest>\n").expect("digest injected");
            prompt[start..].to_owned()
        };
        let discovery_prompts = prompts.lock().unwrap();
        let authoring_prompts = authored.lock().unwrap();
        assert_eq!(discovery_prompts.len(), 9);
        assert_eq!(authoring_prompts.len(), 1);
        let shared = digest_of(&discovery_prompts[0]);
        assert!(shared.contains("tests/ (1 files)"));
        assert!(shared.contains("deployment: none found"));
        for prompt in discovery_prompts.iter().chain(authoring_prompts.iter()) {
            assert_eq!(digest_of(prompt), shared);
        }
    }

    #[tokio::test]
    async fn discovery_cache_reuses_verdicts_until_the_repo_state_changes() {
        let tmp = TempDir::new().unwrap();
        let repo = git_seed_repo(&tmp);
        let tasks_dir = tmp.path().join("tasks");
        let cache_path = tmp.path().join("state/discovery-cache.json");
        let author = tier_task_author();
        let empty_queue = |_: &Path| Ok(0usize);

        // Cycle 1: a cold cache discovers every dispatchable tier.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let discovery = library_invoker(seen.clone());
        let first = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            queue_depth: Some(&empty_queue),
            discovery_cache: Some(cache_path.clone()),
            ..RefillPassOptions::new(&tasks_dir, &repo)
        })
        .await
        .unwrap();
        assert_eq!(first.discovery_cache, DiscoveryCacheStatus::Miss);
        assert_eq!(seen_sorted(&seen), vec![2, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(task_ids_in(&tasks_dir), vec!["task-t5"]);
        assert!(cache_path.is_file());

        // Cycle 2 (a fresh pass, as after a daemon restart): same HEAD, so
        // no discovery agent runs, and the finding cycle 1 already authored
        // is skipped rather than re-authored into a duplicate.
        let panicking = |_prompt: String| async move { panic!("cache hit must not rediscover") };
        let second = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&panicking),
            authoring_invoker: Some(&author),
            queue_depth: Some(&empty_queue),
            discovery_cache: Some(cache_path.clone()),
            ..RefillPassOptions::new(&tasks_dir, &repo)
        })
        .await
        .unwrap();
        assert_eq!(second.discovery_cache, DiscoveryCacheStatus::Hit);
        assert_eq!(second.already_authored, vec!["t5-f1"]);
        assert_eq!(values(&second.relevant_tiers), vec![5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(
            second
                .prefiltered
                .iter()
                .map(|s| s.tier.value())
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(task_ids_in(&tasks_dir), vec!["task-t5", "task-t6"]);

        // Emitting into .flywheel/ inside the repo does not invalidate it.
        let in_repo_tasks = repo.join(".flywheel").join("tasks");
        let third = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&panicking),
            authoring_invoker: Some(&author),
            queue_depth: Some(&empty_queue),
            discovery_cache: Some(cache_path.clone()),
            ..RefillPassOptions::new(&in_repo_tasks, &repo)
        })
        .await
        .unwrap();
        assert_eq!(third.discovery_cache, DiscoveryCacheStatus::Hit);
        assert_eq!(third.already_authored, vec!["t5-f1", "t6-f1"]);

        // A commit moves HEAD: the cache misses and discovery runs again.
        std::fs::write(
            repo.join("tests/test_more.py"),
            "def test_more():\n    pass\n",
        )
        .unwrap();
        git(&repo, &["add", "tests/test_more.py"]);
        git(&repo, &["commit", "-q", "-m", "more"]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let discovery = library_invoker(seen.clone());
        let moved = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            queue_depth: Some(&empty_queue),
            discovery_cache: Some(cache_path.clone()),
            ..RefillPassOptions::new(&tasks_dir, &repo)
        })
        .await
        .unwrap();
        assert_eq!(moved.discovery_cache, DiscoveryCacheStatus::Miss);
        assert!(moved.already_authored.is_empty());
        assert_eq!(seen.lock().unwrap().len(), 9);
    }

    #[tokio::test]
    async fn errored_tier_verdicts_are_not_cached_and_are_rediscovered_alone() {
        let tmp = TempDir::new().unwrap();
        let repo = git_seed_repo(&tmp);
        let cache_path = tmp.path().join("discovery-cache.json");
        let full_queue = |_: &Path| Ok(0usize);
        let dropper = scripted(fenced(&json!({"tasks": [], "dropped": "ungradeable"})));
        let flaky = |prompt: String| async move {
            let tier = tier_in_prompt(&prompt);
            if tier == 5 {
                Err(InvokeError::from("tier 5 agent crashed"))
            } else {
                Ok(library_response(tier))
            }
        };
        let first = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&flaky),
            authoring_invoker: Some(&dropper),
            queue_depth: Some(&full_queue),
            discovery_cache: Some(cache_path.clone()),
            ..RefillPassOptions::new(tmp.path().join("tasks"), &repo)
        })
        .await
        .unwrap();
        assert_eq!(first.discovery_cache, DiscoveryCacheStatus::Miss);
        assert!(!first.relevant_tiers.contains(&Tier::CoreFeatureWork));

        let seen = Arc::new(Mutex::new(Vec::new()));
        let discovery = library_invoker(seen.clone());
        let second = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&dropper),
            queue_depth: Some(&full_queue),
            discovery_cache: Some(cache_path.clone()),
            ..RefillPassOptions::new(tmp.path().join("tasks"), &repo)
        })
        .await
        .unwrap();
        assert_eq!(seen_sorted(&seen), vec![5]);
        assert_eq!(
            second.discovery_cache,
            DiscoveryCacheStatus::Partial {
                reused: vec![
                    Tier::ImminentSevereRisk,
                    Tier::CommittedDeliverables,
                    Tier::TestCoverage,
                    Tier::NonCriticalBugs,
                    Tier::TechDebt,
                    Tier::ObservabilityTooling,
                    Tier::Documentation,
                    Tier::Polish,
                ]
            }
        );
        assert!(second.relevant_tiers.contains(&Tier::CoreFeatureWork));
        // Dropped findings count as authored: tiers 6-11 were all tried in
        // cycle 1 and are not re-authored; only tier 5's new finding is.
        assert_eq!(second.dropped.len(), 1);
        assert_eq!(second.dropped[0].finding.id, "t5-f1");
    }

    #[tokio::test]
    async fn cache_is_disabled_by_a_zero_ttl_or_outside_git() {
        let tmp = TempDir::new().unwrap();
        let git_repo = git_seed_repo(&tmp);
        let discovery = library_invoker(Arc::new(Mutex::new(Vec::new())));
        let author = tier_task_author();
        let zero = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            discovery_cache: Some(tmp.path().join("c.json")),
            discovery_cache_ttl_seconds: 0.0,
            ..RefillPassOptions::new(tmp.path().join("tasks-a"), &git_repo)
        })
        .await
        .unwrap();
        assert_eq!(
            zero.discovery_cache,
            DiscoveryCacheStatus::Disabled("discovery_cache_ttl_seconds is 0".to_owned())
        );
        assert!(!tmp.path().join("c.json").exists());

        let plain = TempDir::new().unwrap();
        let plain_repo = seed_repo(&plain);
        let outside = run_refill_pass(RefillPassOptions {
            target_depth: 1,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            discovery_cache: Some(plain.path().join("c.json")),
            ..RefillPassOptions::new(plain.path().join("tasks"), &plain_repo)
        })
        .await
        .unwrap();
        assert_eq!(
            outside.discovery_cache,
            DiscoveryCacheStatus::Disabled("repository has no git HEAD".to_owned())
        );
    }

    /// An authoring invoker that tracks how many sessions are in flight,
    /// records completion order, and finishes EARLIER tiers LATER so a
    /// fan-out completes out of sequence order.
    fn timed_author(
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
        finished: Arc<Mutex<Vec<u8>>>,
    ) -> impl AutopilotInvoker {
        use std::sync::atomic::Ordering;
        move |prompt: String| {
            let (in_flight, peak, finished) = (in_flight.clone(), peak.clone(), finished.clone());
            async move {
                let start = prompt.find("Finding (Tier ").unwrap() + "Finding (Tier ".len();
                let tier: u8 = prompt[start..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap();
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let delay = 30 * u64::from(12 - tier);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                finished.lock().unwrap().push(tier);
                Ok(fenced(&json!({"tasks": [authored_entry(
                    &format!("task-t{tier}"), "pytest tests/test_existing.py", "tests/test_existing.py"
                )]})))
            }
        }
    }

    async fn fan_out_pass(
        concurrency: usize,
        target_depth: i64,
    ) -> (AutopilotPassResult, usize, Vec<u8>) {
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let discovery = library_invoker(Arc::new(Mutex::new(Vec::new())));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let finished = Arc::new(Mutex::new(Vec::new()));
        let author = timed_author(
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak.clone(),
            finished.clone(),
        );
        let result = run_refill_pass(RefillPassOptions {
            target_depth,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            authoring_concurrency: concurrency,
            ..RefillPassOptions::new(tmp.path().join("tasks"), &repo)
        })
        .await
        .unwrap();
        let finished = finished.lock().unwrap().clone();
        (
            result,
            peak.load(std::sync::atomic::Ordering::SeqCst),
            finished,
        )
    }

    fn emitted_ids(result: &AutopilotPassResult) -> Vec<&str> {
        result.emitted.iter().map(|e| e.task.id.as_str()).collect()
    }

    #[tokio::test]
    async fn authoring_fans_out_but_emits_in_sequence_order() {
        let (result, peak, finished) = fan_out_pass(3, 3).await;
        assert_eq!(peak, 3);
        // Sessions finished out of order (tier 7 first) ...
        assert_eq!(finished, vec![7, 6, 5]);
        // ... yet emission follows the score sequence exactly.
        assert_eq!(emitted_ids(&result), vec!["task-t5", "task-t6", "task-t7"]);
        assert!(result.emitted_paths[0].ends_with("task-t5.json"));
    }

    #[tokio::test]
    async fn concurrency_one_is_the_serial_loop() {
        let (result, peak, finished) = fan_out_pass(1, 3).await;
        assert_eq!(peak, 1);
        assert_eq!(finished, vec![5, 6, 7]);
        assert_eq!(emitted_ids(&result), vec!["task-t5", "task-t6", "task-t7"]);
    }

    #[tokio::test]
    async fn fan_out_never_launches_more_sessions_than_open_slots() {
        let (result, peak, finished) = fan_out_pass(5, 2).await;
        assert_eq!(peak, 2);
        assert_eq!(finished.len(), 2);
        assert_eq!(emitted_ids(&result), vec!["task-t5", "task-t6"]);
    }

    #[tokio::test]
    async fn cancelling_a_pass_drops_every_in_flight_authoring_session() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Dropped(Arc<AtomicUsize>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let tmp = TempDir::new().unwrap();
        let repo = seed_repo(&tmp);
        let discovery = library_invoker(Arc::new(Mutex::new(Vec::new())));
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let (s, d) = (started.clone(), dropped.clone());
        let author = move |_prompt: String| {
            let (s, d) = (s.clone(), d.clone());
            async move {
                let _guard = Dropped(d);
                s.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<String, InvokeError>>().await
            }
        };
        let pass = run_refill_pass(RefillPassOptions {
            target_depth: 5,
            discovery_invoker: Some(&discovery),
            authoring_invoker: Some(&author),
            authoring_concurrency: 3,
            ..RefillPassOptions::new(tmp.path().join("tasks"), &repo)
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), pass)
                .await
                .is_err()
        );
        assert_eq!(started.load(Ordering::SeqCst), 3);
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }
}
