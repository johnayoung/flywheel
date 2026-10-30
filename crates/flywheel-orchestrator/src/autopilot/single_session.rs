//! Autopilot single-session discovery: all tiers as subagents of ONE agent
//! session (spec 00059).

use super::{
    DEFAULT_DISCOVERY_MAX_TURNS, DiscoverySessionRunner, InvokeError, RUNTIME_TIMEOUT,
    SessionFuture, Tier, TierVerdict, agent_deadline, extract_json_object, parse_tier_verdict,
    tier_prompt, verdict_from_data,
};
use crate::autopilot_context::prompt_with_digest;
use flywheel_agents::{
    AgentConfiguration, AgentRuntime, ExecutionHost, PermissionPolicy, RunOptions, RunRequest,
};
use flywheel_core::agents_invoke::completed_run_to_iteration_result;
use flywheel_core::deadline_config::DEFAULT_AUTOPILOT_AGENT_SECONDS;
use flywheel_core::invoker::{IterationResult, ToolResultContent, ToolResultObservation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
