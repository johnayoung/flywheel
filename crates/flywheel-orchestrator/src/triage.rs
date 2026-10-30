//! Triage pass: compile intake-labeled issues to ready or needs-detail.
//!
//! A triage pass reads every open issue carrying the intake label and ends
//! each one in exactly one terminal state:
//!
//! * **ready** -- the issue body gains an appended `flywheel` spec block
//!   whose authoritative grader was *executed against the current base and
//!   observed to fail* (non-zero exit). The block carries a
//!   [`TriageReceipt`] recording that command, the observed exit code, the
//!   base SHA, and the content hash of the issue's human text, so the
//!   drain-side trust rule ([`crate::github::GithubWorkSource`] built with
//!   `require_triage_receipt`) can schedule it. The ready label is added
//!   and the intake label removed.
//! * **needs-detail** -- the issue gains the needs-detail label and exactly
//!   one comment naming the specific missing information. No spec block is
//!   appended and no receipt is minted. This is the outcome when the
//!   authoring agent cannot derive an authoritative grader from the issue's
//!   stated intent, or when the candidate grader it derives exits 0 against
//!   the base (a vacuous check that proves nothing).
//!
//! Every external effect is an injectable seam so a pass runs hermetically
//! in tests -- no network, no agent, no real subprocess:
//!
//! * the `gh` subprocess seam ([`GhRunner`]), reused for the issue reads and
//!   every label / body / comment write;
//! * the authoring-agent seam ([`TriageInvoke`]: prompt in, text out), whose
//!   production implementation is the autopilot repo invoker
//!   ([`crate::autopilot::AgentsRepoInvoker`]): a cold flywheel-agents
//!   session rooted in the repo;
//! * the grader-command seam ([`TriageGraderExecutor`]): given the
//!   authoritative command, run it against the current base and return its
//!   observed exit code. The ready flip is ordered strictly after this call
//!   and records the exit code the executor observed, never one the agent
//!   reported -- agent claims are untrusted, so the receipt is unforgeable
//!   by construction.
//!
//! The pass writes only where issue state demands it. Its backlog is the
//! intake issues (always first-time candidates) merged with the *ready*
//! issues whose receipt drifted from their current content
//! ([`ready_needs_retriage`]); a settled ready issue -- one still carrying a
//! fail-first receipt matching its content -- is not a candidate, so a pass
//! over unchanged state records zero GitHub writes. Re-triage of a drifted
//! ready issue rewrites its body so no stale receipt survives -- refreshed
//! in place when it stays ready, block-stripped when it demotes to
//! needs-detail. Each pass processes at most `per_pass_cap` candidates in
//! issue-number order and surfaces the deferred remainder through the log
//! seam and [`TriagePassResult::deferred`].
//!
//! The pass records no stop events: its only side channel is the log seam
//! (the per-pass cap line and the skipped-issue lines). There is no CLI
//! verb, no daemon loop, and no `[triage]` policy binding here -- those live
//! in [`crate::triage_run`]; the repo, labels, base SHA, and cap arrive as
//! explicit constructor arguments.
//!
//! Authoritative specs: `docs/autopilot.md` (triage), `docs/work-sources.md`
//! (the triage trust rule).

use crate::autopilot::{
    AgentsRepoInvoker, AutopilotInvoker, RepoInvokerOptions, build_repo_invoker,
};
use crate::github::{GhRunner, ISSUE_LIST_FIELDS, LIST_LIMIT, emit_truncation_warning, run_gh};
use crate::sources::{LogSink, WorkSourceError};
use crate::triage_receipt::{
    RECEIPT_KEY, SPEC_FENCE_CLOSE, SPEC_FENCE_OPEN, TriageReceipt, content_hash, is_fail_first,
    parse_receipt, py_splitlines, strip_spec_block,
};
use flywheel_core::deadline_config::DEFAULT_AUTOPILOT_AGENT_SECONDS;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// Default turn budget for the production authoring invoker (mirrors the
/// autopilot authoring budget).
pub const DEFAULT_TRIAGE_MAX_TURNS: u32 = 120;

/// The agent the production authoring invoker drives when the policy names
/// none (`[agent] id` unset).
pub use crate::autopilot::DEFAULT_AGENT_ID as DEFAULT_TRIAGE_AGENT_ID;

/// The `--limit` the issue listing passes to `gh` (one page of
/// [`LIST_LIMIT`]).
const LIST_LIMIT_ARG: &str = "200";

const _: () = assert!(LIST_LIMIT == 200);

// --- Errors ------------------------------------------------------------------------

/// A triage pass could not complete.
///
/// A board read error aborts the pass (the daemon counts it as one failed
/// cycle). An error while triaging one candidate is contained to that
/// candidate (see [`TriagePass::run`]) and fails the pass only when no
/// candidate succeeded; a partially written board is re-read on the next
/// pass, where the write discipline makes the remaining work idempotent.
#[derive(Debug, thiserror::Error)]
pub enum TriageError {
    /// `gh issue list` printed something that is not JSON.
    #[error("gh issue list returned invalid JSON: {0}")]
    InvalidListJson(String),
    /// `gh issue list` printed JSON that is not a list; carries the Python
    /// type name of what it was (`dict`, `str`, ...).
    #[error("gh issue list returned {0}, expected a list")]
    ListNotAList(String),
    /// A `gh` invocation failed (non-zero exit or spawn failure).
    #[error(transparent)]
    Gh(#[from] WorkSourceError),
    /// The authoring agent failed before producing a response.
    #[error("authoring agent failed: {0}")]
    Invoke(String),
    /// The grader executor could not run the authoritative command at all.
    #[error("grader executor failed: {0}")]
    Execute(String),
    /// `git` could not resolve the base commit.
    #[error("{0}")]
    Git(String),
    /// `base_sha` was empty at construction.
    #[error("base_sha must be a non-empty commit SHA")]
    EmptyBaseSha,
    /// `per_pass_cap` was zero at construction.
    #[error("per_pass_cap must be a positive integer or None")]
    InvalidPerPassCap,
}

impl TriageError {
    /// A short class name for the `cycle failed (<kind>: <message>)` log
    /// line, standing in for Python's `type(exc).__name__`.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::InvalidListJson(_) | Self::ListNotAList(_) => "TriageError",
            Self::Gh(_) => "WorkSourceError",
            Self::Invoke(_) => "InvokeError",
            Self::Execute(_) => "ExecuteError",
            Self::Git(_) => "GitError",
            Self::EmptyBaseSha | Self::InvalidPerPassCap => "ValueError",
        }
    }
}

// --- Seams -------------------------------------------------------------------------

/// The future a [`TriageInvoke`] returns: the agent's response text.
pub type TriageInvokeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, TriageError>> + Send + 'a>>;

/// The authoring-agent seam: a prompt in, the agent's response text out.
///
/// Structurally the same shape as the autopilot's invoker seam; the
/// production implementation is [`crate::autopilot::AgentsRepoInvoker`]. Tests script it.
pub trait TriageInvoke: Send + Sync {
    fn invoke<'a>(&'a self, prompt: &'a str) -> TriageInvokeFuture<'a>;
}

/// The grader-command seam: given the authoritative command string, run it
/// against the current base and return its observed exit code.
///
/// This is the sole authority for the fail-first proof -- the ready flip
/// reads its return value, never an agent-reported status. A process killed
/// by a signal reports the negated signal number (non-zero), as Python's
/// `subprocess` does.
pub trait TriageGraderExecutor: Send + Sync {
    fn execute(&self, command: &str) -> Result<i32, TriageError>;
}

// --- Decisions and outcomes -------------------------------------------------------------

/// Terminal triage decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Ready,
    NeedsDetail,
}

impl Decision {
    /// The wire / log spelling (`ready`, `needs_detail`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NeedsDetail => "needs_detail",
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A compilable triage plan the authoring agent derived from an issue.
///
/// `authoritative_grader` is the single fail-first command; `graders` are
/// the grader objects to embed in the spec block (the authoritative command
/// is guaranteed to appear among them when embedded). `goal` / `context` /
/// `tags` / `prerequisites` are optional spec-block fields carried verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TriagePlan {
    pub authoritative_grader: String,
    pub graders: Vec<Map<String, Value>>,
    pub goal: Option<String>,
    pub context: Option<Map<String, Value>>,
    pub tags: Vec<String>,
    pub prerequisites: Vec<String>,
}

/// The authoring agent could not derive an authoritative grader.
///
/// `missing_information` names the specific facts the issue would need to
/// become compilable -- one entry per gap, phrased for THIS issue.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CannotCompile {
    pub missing_information: Vec<String>,
}

/// What [`parse_authoring_response`] made of the agent's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthoringResponse {
    Plan(TriagePlan),
    CannotCompile(CannotCompile),
}

/// The terminal triage decision for one issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageOutcome {
    pub number: i64,
    pub source_ref: String,
    pub decision: Decision,
    pub authoritative_command: Option<String>,
    pub exit_code: Option<i64>,
    pub receipt: Option<TriageReceipt>,
    pub comment: Option<String>,
    pub missing_information: Vec<String>,
}

/// A candidate whose triage errored this pass: it reached no terminal
/// decision and stays on the board for a later pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageFailure {
    pub number: i64,
    /// [`TriageError::kind_name`] of the error.
    pub kind: &'static str,
    /// The error's display text.
    pub message: String,
}

/// The outcomes of one triage pass, in board order.
///
/// `deferred` counts the work candidates cut past this pass's
/// `per_pass_cap` -- issues left untouched this pass (0 when uncapped or
/// within the cap). `failed` lists the candidates whose triage errored, in
/// board order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TriagePassResult {
    pub outcomes: Vec<TriageOutcome>,
    pub deferred: usize,
    pub failed: Vec<TriageFailure>,
}

impl TriagePassResult {
    /// The outcomes that ended ready, in board order.
    pub fn ready(&self) -> Vec<&TriageOutcome> {
        self.by_decision(Decision::Ready)
    }

    /// The outcomes that ended needs-detail, in board order.
    pub fn needs_detail(&self) -> Vec<&TriageOutcome> {
        self.by_decision(Decision::NeedsDetail)
    }

    fn by_decision(&self, decision: Decision) -> Vec<&TriageOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.decision == decision)
            .collect()
    }
}

// --- The authoring prompt and the comments -------------------------------------------------

const PROMPT_HEAD: &str = "You are flywheel's issue-triage authoring agent. A human filed the \
GitHub issue below. Decide whether its stated intent can be turned into a single, concrete, \
fail-first grader: a command that FAILS today precisely because the outcome the issue asks for \
does not yet hold, and would PASS once that outcome is delivered.\n\nIssue title: ";

const PROMPT_BODY_LEAD: &str = "\nIssue body (human-authored):\n";

const PROMPT_RULES: &str = concat!(
    "\n\nHARD RULES:\n",
    "- Derive the authoritative grader from the intent the human states in the issue above. It ",
    "must encode THAT outcome. Do NOT reach for some unrelated check merely because it happens to ",
    "fail on the current base -- a grader that fails for a reason the issue did not ask about is ",
    "wrong, even though it is red today.\n",
    "- The grader must be a single shell command runnable from the repo root (a test invocation, ",
    "a lint/build command, an assertion script).\n",
    "- If the issue does not carry enough information to write such a grader -- the desired ",
    "behavior, the affected module, or the acceptance signal is missing or ambiguous -- do NOT ",
    "invent one. Instead list the SPECIFIC facts you would need, each naming the exact gap for ",
    "THIS issue (e.g. \"which module owns the retry logic\", not \"add more detail\").\n\n",
    "Respond with exactly one fenced JSON block:\n",
    "```json\n",
    "{\n",
    "  \"authoritative_grader\": \"<one shell command that fails today for the reason the issue ",
    "states, or empty string if you cannot write one>\",\n",
    "  \"graders\": [{\"type\": \"command\", \"run\": \"<same or additional commands>\"}],\n",
    "  \"goal\": \"<one sentence naming the outcome the grader proves>\",\n",
    "  \"context\": {\"relevant\": [\"<path>\"]},\n",
    "  \"missing_information\": [\"<specific fact this issue is missing>\"]\n",
    "}\n",
    "```\n",
    "Populate \"missing_information\" (and leave \"authoritative_grader\" empty) when and only ",
    "when the issue cannot be compiled; otherwise leave \"missing_information\" empty.",
);

/// Build the headless triage-authoring prompt for one issue.
///
/// The prompt instructs the agent to derive the authoritative grader from
/// the intent the human states in the issue -- a command that fails today
/// *because* that stated outcome does not yet hold -- not merely any check
/// that happens to fail on the current base. When the issue lacks the
/// information to write such a grader, the agent must name the specific
/// gaps for THIS issue rather than emit a vague check. Byte-identical to
/// the Python engine's prompt.
pub fn triage_authoring_prompt(title: &str, body: &str) -> String {
    let stripped = strip_spec_block(body);
    let human = if stripped.is_empty() {
        "(the issue body is empty)"
    } else {
        stripped.as_str()
    };
    let mut prompt = String::with_capacity(
        PROMPT_HEAD.len() + title.len() + PROMPT_BODY_LEAD.len() + human.len() + PROMPT_RULES.len(),
    );
    prompt.push_str(PROMPT_HEAD);
    prompt.push_str(title);
    prompt.push_str(PROMPT_BODY_LEAD);
    prompt.push_str(human);
    prompt.push_str(PROMPT_RULES);
    prompt
}

const NEEDS_DETAIL_PREAMBLE: &str = "flywheel triage could not compile this issue into a \
runnable, fail-first check from its current text. To move it to ready, please add the following \
specific detail:";

const DEFAULT_GAPS: [&str; 3] = [
    "the concrete behavior or outcome this issue should produce",
    "the module or file the change affects",
    "the signal that proves the work is done (a test or command)",
];

/// The needs-detail comment naming the specific missing facts.
///
/// Enumerates the gaps the authoring agent named for THIS issue. When the
/// agent signalled cannot-compile without naming any gap, the load-bearing
/// facts every issue needs are requested instead of an empty ask.
pub fn needs_detail_comment(missing_information: &[String]) -> String {
    let mut gaps: Vec<&str> = missing_information
        .iter()
        .map(|g| g.trim())
        .filter(|g| !g.is_empty())
        .collect();
    if gaps.is_empty() {
        gaps = DEFAULT_GAPS.to_vec();
    }
    let bullets: Vec<String> = gaps.iter().map(|g| format!("- {g}")).collect();
    format!("{NEEDS_DETAIL_PREAMBLE}\n\n{}", bullets.join("\n"))
}

/// The needs-detail comment for a candidate grader that passes on the base.
///
/// Names the exact command triage derived and explains that, because it is
/// already green against the current base, it is a vacuous check that
/// proves nothing is broken -- so there is no fail-first work to schedule.
pub fn vacuity_comment(command: &str) -> String {
    format!(
        "flywheel triage derived the check below from this issue, but it already passes \
         against the current base -- a vacuous check that proves nothing is broken:\n\n    \
         {command}\n\nA check that is green today cannot be the fail-first proof for new work. \
         Please sharpen the issue so the intended gap is expressed by a check that fails now -- \
         name the exact behavior that is missing or the input that is mishandled -- and it will \
         be re-triaged."
    )
}

// --- Parsing the agent's response -----------------------------------------------------------

/// Extract the structured JSON object from an agent response.
///
/// Prefers a ```` ```json ```` fenced block (the contract in
/// [`triage_authoring_prompt`]); falls back to the outermost `{...}` span so
/// a model that omitted the fence still parses. `None` when no JSON object
/// is present (a closed fence holding invalid JSON is also `None`: the
/// agent meant that block, so a brace-span fallback would parse the wrong
/// thing).
fn extract_json_object(text: &str) -> Option<Map<String, Value>> {
    const FENCE: &str = "```json";
    if let Some(start) = text.find(FENCE) {
        let body_start = start + FENCE.len();
        if let Some(end) = text[body_start..].find("```") {
            let fenced = text[body_start..body_start + end].trim();
            return match serde_json::from_str::<Value>(fenced) {
                Ok(Value::Object(map)) => Some(map),
                _ => None,
            };
        }
    }
    let first = text.find('{')?;
    let last = text.rfind('}')?;
    if last <= first {
        return None;
    }
    match serde_json::from_str::<Value>(&text[first..=last]) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Python's `str(value)` for the scalar shapes a spec list can carry.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

/// Parse the authoring agent's response into a plan or a cannot-compile.
///
/// A response whose `authoritative_grader` is absent or empty is a
/// cannot-compile (its `missing_information` names the gaps). An
/// unparseable response is treated as cannot-compile with no named gaps --
/// the pass still routes the issue to needs-detail rather than stranding it.
pub fn parse_authoring_response(text: &str) -> AuthoringResponse {
    let Some(data) = extract_json_object(text) else {
        return AuthoringResponse::CannotCompile(CannotCompile::default());
    };
    let authoritative = data
        .get("authoritative_grader")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(authoritative) = authoritative else {
        let missing_information = match data.get("missing_information") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .filter(|m| !m.trim().is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        return AuthoringResponse::CannotCompile(CannotCompile {
            missing_information,
        });
    };
    let graders = match data.get("graders") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|g| g.as_object().cloned())
            .collect(),
        _ => Vec::new(),
    };
    let goal = data
        .get("goal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_string);
    let context = data.get("context").and_then(Value::as_object).cloned();
    let list_of_str = |key: &str| -> Vec<String> {
        match data.get(key) {
            Some(Value::Array(items)) => items.iter().map(py_str).collect(),
            _ => Vec::new(),
        }
    };
    AuthoringResponse::Plan(TriagePlan {
        authoritative_grader: authoritative.to_string(),
        graders,
        goal,
        context,
        tags: list_of_str("tags"),
        prerequisites: list_of_str("prerequisites"),
    })
}

// --- The spec block -----------------------------------------------------------------------

/// The spec-block grader list, guaranteed to carry the authoritative run.
///
/// The authoritative command must be a command grader on the resulting task
/// so the drain schedules a run that executes it. If the agent's grader
/// list already includes it, it is used verbatim; otherwise it is prepended.
fn ensure_authoritative_grader(plan: &TriagePlan) -> Vec<Map<String, Value>> {
    let mut graders = plan.graders.clone();
    let present = graders.iter().any(|g| {
        g.get("type").and_then(Value::as_str) == Some("command")
            && g.get("run").and_then(Value::as_str) == Some(plan.authoritative_grader.as_str())
    });
    if !present {
        let mut grader = Map::new();
        grader.insert("type".into(), json!("command"));
        grader.insert("run".into(), json!(plan.authoritative_grader));
        graders.insert(0, grader);
    }
    graders
}

/// `json.dumps(value, indent=2)` as Python prints it: two-space pretty
/// output with every non-ASCII character written as a `\u` escape (`ensure_ascii`),
/// so a block the Rust engine embeds is byte-identical to one the Python
/// engine embeds.
fn py_json_dumps_indent2(value: &Value) -> String {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
    let mut out = String::with_capacity(pretty.len());
    let mut units = [0u16; 2];
    for ch in pretty.chars() {
        if ch.is_ascii() {
            out.push(ch);
            continue;
        }
        for unit in ch.encode_utf16(&mut units) {
            out.push_str(&format!("\\u{unit:04x}"));
        }
    }
    out
}

/// Append the machine spec block, preserving the human text verbatim.
///
/// Any pre-existing `flywheel` block is stripped so the drain reads exactly
/// one block -- the fresh one carrying this pass's receipt. The human prose
/// is preserved (only its boundary whitespace is normalized, matching how
/// [`content_hash`] reads it), so the appended body hashes identically to
/// the receipt's `content_hash`.
pub fn embed_spec_block(body: &str, plan: &TriagePlan, receipt: &TriageReceipt) -> String {
    let human = strip_spec_block(body);
    let mut spec = Map::new();
    if let Some(goal) = plan.goal.as_deref().filter(|g| !g.is_empty()) {
        spec.insert("goal".into(), json!(goal));
    }
    spec.insert(
        "graders".into(),
        Value::Array(
            ensure_authoritative_grader(plan)
                .into_iter()
                .map(Value::Object)
                .collect(),
        ),
    );
    if let Some(context) = plan.context.as_ref().filter(|c| !c.is_empty()) {
        spec.insert("context".into(), Value::Object(context.clone()));
    }
    if !plan.tags.is_empty() {
        spec.insert("tags".into(), json!(plan.tags));
    }
    if !plan.prerequisites.is_empty() {
        spec.insert("prerequisites".into(), json!(plan.prerequisites));
    }
    spec.insert(
        RECEIPT_KEY.into(),
        json!({
            "command": receipt.command,
            "exit_code": receipt.exit_code,
            "base_sha": receipt.base_sha,
            "content_hash": receipt.content_hash,
        }),
    );
    let block = format!(
        "{SPEC_FENCE_OPEN}\n{}\n{SPEC_FENCE_CLOSE}",
        py_json_dumps_indent2(&Value::Object(spec))
    );
    if human.is_empty() {
        block
    } else {
        format!("{human}\n\n{block}")
    }
}

/// The issue body's `flywheel` spec block as an object, or `None`.
///
/// A tolerant reader for the triage side: unlike the drain's strict
/// [`crate::github::extract_spec_block`] (which errors on a malformed block
/// an author meant), a missing, unterminated, non-JSON, or non-object block
/// all yield `None`. The re-triage caller treats every "no trustworthy
/// block" case uniformly as drift, so an error here would be wrong -- the
/// point is to *fix* such an issue.
pub fn spec_block_of(body: &str) -> Option<Map<String, Value>> {
    let lines = py_splitlines(body);
    let open_idx = lines.iter().position(|l| l.trim() == SPEC_FENCE_OPEN)?;
    let mut block: Vec<&str> = Vec::new();
    for line in &lines[open_idx + 1..] {
        if line.trim() != SPEC_FENCE_CLOSE {
            block.push(line);
            continue;
        }
        return match serde_json::from_str::<Value>(&block.join("\n")) {
            Ok(Value::Object(map)) => Some(map),
            _ => None,
        };
    }
    None
}

/// Whether a ready-labeled issue must be re-triaged this pass.
///
/// A ready issue is *settled* -- left untouched, so an idempotent pass
/// writes nothing -- exactly when its spec block carries a fail-first triage
/// receipt whose `content_hash` still matches the issue's current human
/// content. Drift (a human edited the title/body after triage), a missing or
/// malformed receipt, or a non-fail-first receipt all mean the receipt no
/// longer attests the current content, so the issue is re-triaged. The
/// receipt schema and the content-hash definition come from
/// [`crate::triage_receipt`], never redefined here.
pub fn ready_needs_retriage(title: &str, body: &str) -> bool {
    let receipt = spec_block_of(body)
        .and_then(|spec| spec.get(RECEIPT_KEY).cloned())
        .and_then(|raw| parse_receipt(&raw));
    match receipt {
        Some(receipt) if is_fail_first(&receipt) => {
            receipt.content_hash != content_hash(title, body)
        }
        _ => true,
    }
}

// --- Issue payload helpers -----------------------------------------------------------

/// Python's `str(issue.get(key) or "")`: a falsy value is the empty string,
/// anything else is stringified.
fn py_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => String::new(),
        Some(Value::Array(a)) if a.is_empty() => String::new(),
        Some(Value::Object(o)) if o.is_empty() => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Python's `type(value).__name__` for a decoded JSON value.
fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

// --- The pass ----------------------------------------------------------------------

/// The board a pass reads and writes: the repo and its three labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageBoard {
    pub repo: String,
    pub intake_label: String,
    pub ready_label: String,
    pub needs_detail_label: String,
}

/// The three injectable seams a pass runs through.
#[derive(Clone)]
pub struct TriageSeams {
    pub invoker: Arc<dyn TriageInvoke>,
    pub executor: Arc<dyn TriageGraderExecutor>,
    pub runner: Arc<dyn GhRunner>,
}

/// Why a candidate is on the backlog: a first-time intake issue, or a ready
/// issue whose receipt drifted. Both consume a per-pass cap slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
    Intake,
    ReadyDrift,
}

/// One backlog entry: the issue fields the pass reads, already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    kind: CandidateKind,
    number: i64,
    title: String,
    body: String,
}

/// One triage pass over a repo's open intake-labeled issues.
///
/// `base_sha` is the commit the receipt binds to. `per_pass_cap` bounds how
/// many work candidates a single pass processes (`None` is unbounded); the
/// remainder is deferred untouched and surfaced through the log seam and
/// [`TriagePassResult::deferred`].
pub struct TriagePass {
    board: TriageBoard,
    base_sha: String,
    seams: TriageSeams,
    per_pass_cap: Option<usize>,
    log: Option<LogSink>,
}

impl fmt::Debug for TriagePass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TriagePass")
            .field("board", &self.board)
            .field("base_sha", &self.base_sha)
            .field("per_pass_cap", &self.per_pass_cap)
            .finish_non_exhaustive()
    }
}

impl TriagePass {
    /// A pass over `board` binding receipts to `base_sha`, driven through
    /// `seams`. Errors when `base_sha` is empty.
    pub fn new(
        board: TriageBoard,
        base_sha: impl Into<String>,
        seams: TriageSeams,
    ) -> Result<Self, TriageError> {
        let base_sha = base_sha.into();
        if base_sha.is_empty() {
            return Err(TriageError::EmptyBaseSha);
        }
        Ok(Self {
            board,
            base_sha,
            seams,
            per_pass_cap: None,
            log: None,
        })
    }

    /// Bound the candidates processed per pass; `Some(0)` is an error.
    pub fn with_per_pass_cap(mut self, cap: Option<usize>) -> Result<Self, TriageError> {
        if cap == Some(0) {
            return Err(TriageError::InvalidPerPassCap);
        }
        self.per_pass_cap = cap;
        Ok(self)
    }

    pub fn with_log(mut self, log: LogSink) -> Self {
        self.log = Some(log);
        self
    }

    pub fn board(&self) -> &TriageBoard {
        &self.board
    }

    pub fn base_sha(&self) -> &str {
        &self.base_sha
    }

    pub fn per_pass_cap(&self) -> Option<usize> {
        self.per_pass_cap
    }

    /// Triage the board with write discipline; return the terminal outcomes.
    ///
    /// The backlog is the intake issues (first-time candidates) merged with
    /// the drifted ready issues, deduplicated and ordered by issue number.
    /// Settled ready issues are not candidates, so a pass over unchanged
    /// state writes nothing. Candidates are processed up to `per_pass_cap`;
    /// the remainder is deferred untouched, counted, and logged.
    ///
    /// A candidate whose triage errors (agent, executor, or board write) is
    /// logged, recorded in [`TriagePassResult::failed`], and skipped, so a
    /// permanently failing issue cannot starve the issues after it in board
    /// order. The pass itself errors -- with the first candidate error --
    /// only when candidates were attempted and every one failed, which keeps
    /// a systemic fault (agent CLI missing, `gh` auth lost) visible to the
    /// daemon's circuit breaker.
    pub async fn run(&self) -> Result<TriagePassResult, TriageError> {
        let intake = self.list_issues(&self.board.intake_label)?;
        let ready = self.list_issues(&self.board.ready_label)?;
        let backlog = self.backlog(intake, ready);
        let total = backlog.len();
        let (selected, deferred) = match self.per_pass_cap {
            Some(cap) if total > cap => {
                let deferred = total - cap;
                self.log_line(&format!(
                    "[triage] per-pass cap {cap} reached: processing {cap} of {total} \
                     candidates, deferring {deferred} to a later pass"
                ));
                (&backlog[..cap], deferred)
            }
            _ => (&backlog[..], 0),
        };
        let mut outcomes = Vec::with_capacity(selected.len());
        let mut failed = Vec::new();
        let mut first_error = None;
        for candidate in selected {
            match self.triage_candidate(candidate).await {
                Ok(outcome) => outcomes.push(outcome),
                Err(err) => {
                    self.log_line(&format!(
                        "[triage] #{} failed ({}: {err}); skipped this pass",
                        candidate.number,
                        err.kind_name()
                    ));
                    failed.push(TriageFailure {
                        number: candidate.number,
                        kind: err.kind_name(),
                        message: err.to_string(),
                    });
                    first_error.get_or_insert(err);
                }
            }
        }
        match first_error {
            Some(err) if outcomes.is_empty() => Err(err),
            _ => Ok(TriagePassResult {
                outcomes,
                deferred,
                failed,
            }),
        }
    }

    /// The ordered work backlog: intake issues plus drifted ready issues.
    ///
    /// Every intake issue is a first-time candidate; a ready issue enters
    /// only when [`ready_needs_retriage`] holds. A number is admitted at most
    /// once -- intake and ready are disjoint labels in reality, and the
    /// guard also keeps a runner that conflates the two honest. The backlog
    /// is sorted by issue number so a drifted ready issue mid-board still
    /// consumes a per-pass cap slot.
    fn backlog(
        &self,
        intake: Vec<Map<String, Value>>,
        ready: Vec<Map<String, Value>>,
    ) -> Vec<Candidate> {
        let mut items = Vec::with_capacity(intake.len() + ready.len());
        let mut seen: HashSet<i64> = HashSet::new();
        for (kind, issues) in [
            (CandidateKind::Intake, intake),
            (CandidateKind::ReadyDrift, ready),
        ] {
            for issue in issues {
                let Some(number) = issue.get("number").and_then(Value::as_i64) else {
                    self.log_line(&format!(
                        "[triage] skipping issue with non-integer number: {}",
                        Value::Object(issue)
                    ));
                    continue;
                };
                if !seen.insert(number) {
                    continue;
                }
                let title = py_text(issue.get("title"));
                let body = py_text(issue.get("body"));
                if kind == CandidateKind::ReadyDrift && !ready_needs_retriage(&title, &body) {
                    continue;
                }
                items.push(Candidate {
                    kind,
                    number,
                    title,
                    body,
                });
            }
        }
        items.sort_by_key(|c| c.number);
        items
    }

    /// `gh issue list` for one label: the open issues as objects (non-object
    /// entries dropped), unordered.
    fn list_issues(&self, label: &str) -> Result<Vec<Map<String, Value>>, TriageError> {
        let stdout = run_gh(
            self.seams.runner.as_ref(),
            &[
                "issue",
                "list",
                "--repo",
                &self.board.repo,
                "--label",
                label,
                "--state",
                "open",
                "--json",
                ISSUE_LIST_FIELDS,
                "--limit",
                LIST_LIMIT_ARG,
            ],
        )?;
        let text = if stdout.is_empty() { "[]" } else { &stdout };
        let issues: Value = serde_json::from_str(text)
            .map_err(|err| TriageError::InvalidListJson(err.to_string()))?;
        match issues {
            Value::Array(items) => {
                // `gh issue list` reports no `hasNextPage`; a page filled to
                // the cap is the only truncation evidence, measured on the
                // raw page before non-object entries are dropped.
                if items.len() == LIST_LIMIT {
                    emit_truncation_warning(self.log.as_ref(), "triage", &format!("{label} issue"));
                }
                Ok(items
                    .into_iter()
                    .filter_map(|i| match i {
                        Value::Object(map) => Some(map),
                        _ => None,
                    })
                    .collect())
            }
            other => Err(TriageError::ListNotAList(py_type_name(&other).to_string())),
        }
    }

    /// Triage one candidate to its terminal state and write it to the board.
    ///
    /// Intake and drifted-ready candidates share the decision path; only the
    /// writes differ (see [`Self::write_needs_detail`] / the ready branch).
    /// The ready flip is gated on the executor's observed exit code, read
    /// BEFORE any GitHub write -- the receipt can only record what the
    /// executor saw, never what the agent claimed.
    async fn triage_candidate(&self, candidate: &Candidate) -> Result<TriageOutcome, TriageError> {
        let number = candidate.number;
        let source_ref = format!("{}#{number}", self.board.repo);
        let prompt = triage_authoring_prompt(&candidate.title, &candidate.body);
        let response = self.seams.invoker.invoke(&prompt).await?;
        let plan = match parse_authoring_response(&response) {
            AuthoringResponse::CannotCompile(cannot) => {
                let comment = needs_detail_comment(&cannot.missing_information);
                self.write_needs_detail(candidate, &comment)?;
                return Ok(TriageOutcome {
                    number,
                    source_ref,
                    decision: Decision::NeedsDetail,
                    authoritative_command: None,
                    exit_code: None,
                    receipt: None,
                    comment: Some(comment),
                    missing_information: cannot.missing_information,
                });
            }
            AuthoringResponse::Plan(plan) => plan,
        };

        let exit_code = self.seams.executor.execute(&plan.authoritative_grader)?;
        if exit_code == 0 {
            let comment = vacuity_comment(&plan.authoritative_grader);
            self.write_needs_detail(candidate, &comment)?;
            return Ok(TriageOutcome {
                number,
                source_ref,
                decision: Decision::NeedsDetail,
                authoritative_command: Some(plan.authoritative_grader),
                exit_code: Some(0),
                receipt: None,
                comment: Some(comment),
                missing_information: Vec::new(),
            });
        }

        let receipt = TriageReceipt {
            command: plan.authoritative_grader.clone(),
            exit_code: i64::from(exit_code),
            base_sha: self.base_sha.clone(),
            content_hash: content_hash(&candidate.title, &candidate.body),
        };
        let new_body = embed_spec_block(&candidate.body, &plan, &receipt);
        match candidate.kind {
            CandidateKind::Intake => self.apply_ready(number, &new_body)?,
            CandidateKind::ReadyDrift => self.refresh_ready(number, &new_body)?,
        }
        Ok(TriageOutcome {
            number,
            source_ref,
            decision: Decision::Ready,
            authoritative_command: Some(plan.authoritative_grader),
            exit_code: Some(i64::from(exit_code)),
            receipt: Some(receipt),
            comment: None,
            missing_information: Vec::new(),
        })
    }

    /// Route a candidate to needs-detail: an intake issue keeps its body
    /// ([`Self::apply_needs_detail`]); a drifted ready issue has its stale
    /// block stripped ([`Self::demote_to_needs_detail`]).
    fn write_needs_detail(&self, candidate: &Candidate, comment: &str) -> Result<(), TriageError> {
        match candidate.kind {
            CandidateKind::Intake => self.apply_needs_detail(candidate.number, comment),
            CandidateKind::ReadyDrift => {
                self.demote_to_needs_detail(candidate.number, &candidate.body, comment)
            }
        }
    }

    fn gh(&self, argv: &[&str]) -> Result<(), TriageError> {
        run_gh(self.seams.runner.as_ref(), argv)?;
        Ok(())
    }

    /// Flip an intake issue to ready: the reminted body, ready added, intake
    /// removed.
    fn apply_ready(&self, number: i64, new_body: &str) -> Result<(), TriageError> {
        let number = number.to_string();
        self.gh(&[
            "issue",
            "edit",
            &number,
            "--repo",
            &self.board.repo,
            "--body",
            new_body,
            "--add-label",
            &self.board.ready_label,
            "--remove-label",
            &self.board.intake_label,
        ])
    }

    /// Refresh a still-ready issue's body with the fresh spec block/receipt.
    ///
    /// [`embed_spec_block`] strips the drifted block before appending the
    /// new one, so no stale receipt survives. The issue is already
    /// ready-labeled; re-adding the label keeps the write idempotent and
    /// touches no other label (unlike [`Self::apply_ready`], there is no
    /// intake label to remove).
    fn refresh_ready(&self, number: i64, new_body: &str) -> Result<(), TriageError> {
        let number = number.to_string();
        self.gh(&[
            "issue",
            "edit",
            &number,
            "--repo",
            &self.board.repo,
            "--body",
            new_body,
            "--add-label",
            &self.board.ready_label,
        ])
    }

    /// Demote a drifted ready issue to needs-detail, stripping the stale
    /// block.
    ///
    /// The body is rewritten with its `flywheel` spec block removed so the
    /// stale receipt does not survive the demotion; the ready label is
    /// dropped, the needs-detail label added, and exactly one comment left.
    fn demote_to_needs_detail(
        &self,
        number: i64,
        body: &str,
        comment: &str,
    ) -> Result<(), TriageError> {
        let number = number.to_string();
        self.gh(&[
            "issue",
            "edit",
            &number,
            "--repo",
            &self.board.repo,
            "--body",
            &strip_spec_block(body),
            "--add-label",
            &self.board.needs_detail_label,
            "--remove-label",
            &self.board.ready_label,
        ])?;
        self.comment(&number, comment)
    }

    /// Route an intake issue to needs-detail: label flip plus one comment,
    /// body untouched.
    fn apply_needs_detail(&self, number: i64, comment: &str) -> Result<(), TriageError> {
        let number = number.to_string();
        self.gh(&[
            "issue",
            "edit",
            &number,
            "--repo",
            &self.board.repo,
            "--add-label",
            &self.board.needs_detail_label,
            "--remove-label",
            &self.board.intake_label,
        ])?;
        self.comment(&number, comment)
    }

    fn comment(&self, number: &str, comment: &str) -> Result<(), TriageError> {
        self.gh(&[
            "issue",
            "comment",
            number,
            "--repo",
            &self.board.repo,
            "--body",
            comment,
        ])
    }

    fn log_line(&self, line: &str) {
        if let Some(log) = &self.log {
            log(line);
        }
    }
}

// --- Production seams -------------------------------------------------------------------

/// How the production authoring invoker drives its agent.
#[derive(Debug, Clone, PartialEq)]
pub struct TriageAgentOptions {
    /// The flywheel-agents adapter id (`[agent] id`).
    pub agent_id: String,
    /// The adapter transport (`[agent] transport`), when the policy sets one.
    pub agent_transport: Option<String>,
    /// The model (`--model`, else `[policy] model`); `None` is the agent's
    /// default.
    pub model: Option<String>,
    pub max_turns: u32,
    /// The wall-clock ceiling of one authoring call; `None` is unbounded.
    pub deadline_seconds: Option<f64>,
}

impl Default for TriageAgentOptions {
    fn default() -> Self {
        Self {
            agent_id: DEFAULT_TRIAGE_AGENT_ID.to_string(),
            agent_transport: None,
            model: None,
            max_turns: DEFAULT_TRIAGE_MAX_TURNS,
            deadline_seconds: Some(DEFAULT_AUTOPILOT_AGENT_SECONDS),
        }
    }
}

impl TriageAgentOptions {
    /// The repo-invoker options this authoring seam resolves to.
    pub fn repo_invoker_options(&self) -> RepoInvokerOptions {
        RepoInvokerOptions {
            agent_id: self.agent_id.clone(),
            agent_transport: self.agent_transport.clone(),
            model: self.model.clone(),
            max_turns: self.max_turns,
            deadline_seconds: self.deadline_seconds,
            command_override: None,
        }
    }
}

/// The autopilot repo invoker is the production authoring seam: one cold
/// session per prompt, rooted in the repo, under the configured turn
/// budget and wall-clock ceiling. Only the agent's text crosses the seam.
impl TriageInvoke for AgentsRepoInvoker {
    fn invoke<'a>(&'a self, prompt: &'a str) -> TriageInvokeFuture<'a> {
        Box::pin(async move {
            AutopilotInvoker::invoke(self, prompt)
                .await
                .map_err(|err| TriageError::Invoke(err.to_string()))
        })
    }
}

/// The production authoring seam for `repo_root` under `options`: the same
/// repo invoker autopilot authors tasks with.
pub fn build_triage_authoring_invoker(
    repo_root: impl Into<PathBuf>,
    options: &TriageAgentOptions,
) -> Arc<dyn TriageInvoke> {
    Arc::new(build_repo_invoker(
        repo_root,
        options.repo_invoker_options(),
    ))
}

/// The production grader-command seam: run the command through the POSIX
/// shell rooted in `repo_root` (the checkout at the current base),
/// mirroring how command graders run, and return the observed exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellGraderExecutor {
    repo_root: PathBuf,
}

impl ShellGraderExecutor {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

#[cfg(unix)]
const SHELL: &str = "/bin/sh";
#[cfg(not(unix))]
const SHELL: &str = "sh";

impl TriageGraderExecutor for ShellGraderExecutor {
    fn execute(&self, command: &str) -> Result<i32, TriageError> {
        let output = std::process::Command::new(SHELL)
            .arg("-c")
            .arg(command)
            .current_dir(&self.repo_root)
            .output()
            .map_err(|err| TriageError::Execute(format!("could not start {SHELL}: {err}")))?;
        Ok(exit_code_of(&output.status))
    }
}

/// The exit code Python's `subprocess` would report: the code, or the
/// negated signal number for a signal-terminated process.
fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    -1
}

/// The production grader-command seam for `repo_root`.
pub fn build_grader_executor(repo_root: impl Into<PathBuf>) -> Arc<dyn TriageGraderExecutor> {
    Arc::new(ShellGraderExecutor::new(repo_root))
}

/// Resolve the current base commit SHA of `repo_root` for the receipt.
pub fn resolve_base_sha(repo_root: &Path) -> Result<String, TriageError> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .map_err(|err| TriageError::Git(format!("git rev-parse HEAD failed to start: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TriageError::Git(format!(
            "git rev-parse HEAD failed (exit {}): {}",
            exit_code_of(&output.status),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Scripted seams shared with the daemon tests: a label-aware `gh`
    //! runner that records its write-set, a scripted authoring invoker, and
    //! a scripted grader executor.

    use super::{
        TriageBoard, TriageError, TriageGraderExecutor, TriageInvoke, TriageInvokeFuture,
        TriagePass, TriageSeams,
    };
    use crate::github::{GhOutput, GhRunner};
    use crate::sources::WorkSourceError;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    pub const REPO: &str = "octo/widgets";
    pub const BASE_SHA: &str = "basesha0000000000000000000000000000000000";

    /// An ordered trace of seam calls: `("invoke" | "exec" | "gh", detail)`.
    pub type Events = Arc<Mutex<Vec<(String, String)>>>;

    /// Scripted `gh`: a distinct issue list per label (an unlisted label is
    /// an empty board; `any` answers every label), every argv recorded.
    pub struct ScriptedGh {
        by_label: HashMap<String, String>,
        any: Option<String>,
        pub calls: Mutex<Vec<Vec<String>>>,
        events: Option<Events>,
    }

    impl ScriptedGh {
        /// The same issue list for every label query.
        pub fn all(issues: Vec<Value>) -> Arc<Self> {
            Arc::new(Self {
                by_label: HashMap::new(),
                any: Some(Value::Array(issues).to_string()),
                calls: Mutex::new(Vec::new()),
                events: None,
            })
        }

        pub fn all_with_events(issues: Vec<Value>, events: Events) -> Arc<Self> {
            Arc::new(Self {
                by_label: HashMap::new(),
                any: Some(Value::Array(issues).to_string()),
                calls: Mutex::new(Vec::new()),
                events: Some(events),
            })
        }

        /// A distinct issue list per label.
        pub fn by_label(lists: Vec<(&str, Vec<Value>)>) -> Arc<Self> {
            Arc::new(Self {
                by_label: lists
                    .into_iter()
                    .map(|(label, issues)| (label.to_string(), Value::Array(issues).to_string()))
                    .collect(),
                any: None,
                calls: Mutex::new(Vec::new()),
                events: None,
            })
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        fn starting(&self, head: [&str; 2]) -> Vec<Vec<String>> {
            self.calls()
                .into_iter()
                .filter(|argv| argv.len() >= 2 && argv[0] == head[0] && argv[1] == head[1])
                .collect()
        }

        pub fn edits(&self) -> Vec<Vec<String>> {
            self.starting(["issue", "edit"])
        }

        pub fn comments(&self) -> Vec<Vec<String>> {
            self.starting(["issue", "comment"])
        }

        pub fn writes(&self) -> Vec<Vec<String>> {
            let mut writes = self.edits();
            writes.extend(self.comments());
            writes
        }
    }

    impl GhRunner for ScriptedGh {
        fn run_gh(&self, argv: &[String]) -> Result<GhOutput, WorkSourceError> {
            self.calls.lock().unwrap().push(argv.to_vec());
            if argv.len() >= 2 && argv[0] == "issue" && argv[1] == "list" {
                let label = arg_after(argv, "--label");
                let stdout = self
                    .by_label
                    .get(label)
                    .cloned()
                    .or_else(|| self.any.clone())
                    .unwrap_or_else(|| "[]".to_string());
                return Ok(GhOutput::ok(stdout));
            }
            if let Some(events) = &self.events {
                events
                    .lock()
                    .unwrap()
                    .push(("gh".to_string(), argv[1].clone()));
            }
            Ok(GhOutput::ok(""))
        }
    }

    /// A `gh` runner that must never be reached.
    pub struct ForbiddenGh;

    impl GhRunner for ForbiddenGh {
        fn run_gh(&self, argv: &[String]) -> Result<GhOutput, WorkSourceError> {
            panic!("no gh call may be issued, got: {argv:?}");
        }
    }

    /// Scripted authoring agent: a fixed response, or a response routed by a
    /// needle found in the prompt; `Boom` must never be reached.
    pub enum ScriptedInvoke {
        Fixed(String, Option<Events>),
        Routing(Vec<(String, String)>),
        Boom,
    }

    impl ScriptedInvoke {
        pub fn fixed(response: impl Into<String>) -> Arc<Self> {
            Arc::new(Self::Fixed(response.into(), None))
        }

        pub fn fixed_with_events(response: impl Into<String>, events: Events) -> Arc<Self> {
            Arc::new(Self::Fixed(response.into(), Some(events)))
        }

        pub fn routing(mapping: Vec<(&str, String)>) -> Arc<Self> {
            Arc::new(Self::Routing(
                mapping
                    .into_iter()
                    .map(|(needle, response)| (needle.to_string(), response))
                    .collect(),
            ))
        }

        pub fn boom() -> Arc<Self> {
            Arc::new(Self::Boom)
        }
    }

    impl TriageInvoke for ScriptedInvoke {
        fn invoke<'a>(&'a self, prompt: &'a str) -> TriageInvokeFuture<'a> {
            Box::pin(async move {
                match self {
                    Self::Fixed(response, events) => {
                        if let Some(events) = events {
                            events
                                .lock()
                                .unwrap()
                                .push(("invoke".to_string(), prompt.to_string()));
                        }
                        Ok(response.clone())
                    }
                    Self::Routing(mapping) => mapping
                        .iter()
                        .find(|(needle, _)| prompt.contains(needle.as_str()))
                        .map(|(_, response)| response.clone())
                        .ok_or_else(|| {
                            panic!("no scripted response for prompt: {}", &prompt[..80])
                        }),
                    Self::Boom => panic!("authoring agent must not run for settled state"),
                }
            })
        }
    }

    /// Scripted grader executor: records commands, returns a fixed exit
    /// code; `Boom` must never be reached.
    pub struct ScriptedExecutor {
        exit_code: Option<i32>,
        pub commands: Mutex<Vec<String>>,
        events: Option<Events>,
    }

    impl ScriptedExecutor {
        pub fn exiting(exit_code: i32) -> Arc<Self> {
            Arc::new(Self {
                exit_code: Some(exit_code),
                commands: Mutex::new(Vec::new()),
                events: None,
            })
        }

        pub fn exiting_with_events(exit_code: i32, events: Events) -> Arc<Self> {
            Arc::new(Self {
                exit_code: Some(exit_code),
                commands: Mutex::new(Vec::new()),
                events: Some(events),
            })
        }

        pub fn boom() -> Arc<Self> {
            Arc::new(Self {
                exit_code: None,
                commands: Mutex::new(Vec::new()),
                events: None,
            })
        }

        pub fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl TriageGraderExecutor for ScriptedExecutor {
        fn execute(&self, command: &str) -> Result<i32, TriageError> {
            let Some(exit_code) = self.exit_code else {
                panic!("executor must not run for settled state");
            };
            self.commands.lock().unwrap().push(command.to_string());
            if let Some(events) = &self.events {
                events
                    .lock()
                    .unwrap()
                    .push(("exec".to_string(), command.to_string()));
            }
            Ok(exit_code)
        }
    }

    pub fn events() -> Events {
        Arc::new(Mutex::new(Vec::new()))
    }

    pub fn fenced(obj: Value) -> String {
        format!("```json\n{obj}\n```")
    }

    pub fn issue(number: i64, title: &str, body: &str) -> Value {
        json!({
            "number": number,
            "title": title,
            "body": body,
            "url": format!("https://github.com/octo/widgets/issues/{number}"),
        })
    }

    pub fn ready_response(command: &str) -> String {
        fenced(json!({
            "authoritative_grader": command,
            "graders": [{"type": "command", "run": command}],
            "goal": "Widget retries dropped requests under load.",
        }))
    }

    pub fn board() -> TriageBoard {
        TriageBoard {
            repo: REPO.to_string(),
            intake_label: "intake".to_string(),
            ready_label: "ready".to_string(),
            needs_detail_label: "needs-detail".to_string(),
        }
    }

    pub fn pass(
        gh: Arc<ScriptedGh>,
        executor: Arc<ScriptedExecutor>,
        invoker: Arc<ScriptedInvoke>,
    ) -> TriagePass {
        TriagePass::new(
            board(),
            BASE_SHA,
            TriageSeams {
                invoker,
                executor,
                runner: gh,
            },
        )
        .expect("valid pass")
    }

    pub fn arg_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let idx = argv
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag} missing from {argv:?}"));
        &argv[idx + 1]
    }

    pub fn body_of(argv: &[String]) -> &str {
        arg_after(argv, "--body")
    }

    pub fn has_flag(argv: &[String], flag: &str) -> bool {
        argv.iter().any(|a| a == flag)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::github::GithubWorkSource;
    use crate::sources::WorkSource;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::sync::Mutex;

    fn sha256_hex(text: &str) -> String {
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    fn run(pass: &TriagePass) -> TriagePassResult {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(pass.run())
            .expect("pass completes")
    }

    fn extract_block(body: &str) -> Map<String, Value> {
        spec_block_of(body).expect("a flywheel block")
    }

    fn capture_log() -> (LogSink, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = Arc::clone(&lines);
        let sink: LogSink = Arc::new(move |line: &str| {
            sink_lines.lock().unwrap().push(line.to_string());
        });
        (sink, lines)
    }

    fn stale_receipt(command: &str) -> Value {
        json!({
            "command": command,
            "exit_code": 1,
            "base_sha": "OLDBASE",
            "content_hash": "deadbeefdeadbeef",
        })
    }

    fn body_with_receipt(prose: &str, receipt: &Value) -> String {
        let spec = json!({
            "graders": [{"type": "command", "run": receipt["command"]}],
            RECEIPT_KEY: receipt,
        });
        format!(
            "{prose}\n\n```flywheel\n{}\n```",
            serde_json::to_string_pretty(&spec).unwrap()
        )
    }

    fn issue_with_block(number: i64, title: &str, prose: &str, block_spec: Value) -> Value {
        issue(
            number,
            title,
            &format!("{prose}\n\n```flywheel\n{block_spec}\n```"),
        )
    }

    // --- byte-identity with the Python engine ---------------------------------------

    #[test]
    fn authoring_prompt_is_byte_identical_to_python() {
        // SHA-256 of the strings the Python `triage_authoring_prompt` emits.
        assert_eq!(
            sha256_hex(&triage_authoring_prompt("TITLE_X", "BODY_X")),
            "8a7dc3fa63af0fd6262b8d0fd9b15199285344961fc8e6d7dbcef5857f36d197"
        );
        assert_eq!(
            sha256_hex(&triage_authoring_prompt("T", "")),
            "18a986b111e727dcac7798a5725212d47f39631458a541fe148b3f25092c5738"
        );
        assert_eq!(
            sha256_hex(&triage_authoring_prompt(
                "T",
                "Prose.\n\n```flywheel\n{}\n```\n"
            )),
            "291e153e14662a3f0d186272e0a091fdccd5461fd0417e9274bb462cb48a9172"
        );
    }

    #[test]
    fn comments_are_byte_identical_to_python() {
        assert_eq!(
            sha256_hex(&needs_detail_comment(&[])),
            "9bfbf64b0a399eff1155416d1e22ad46114647b6498292f293c673cc5751d105"
        );
        assert_eq!(
            sha256_hex(&needs_detail_comment(&[
                " a ".to_string(),
                String::new(),
                "b".to_string()
            ])),
            "92e52b8e8a4a9354554a73d710456b5c7247ce55d5944205ba13a4b646690c3b"
        );
        assert_eq!(
            sha256_hex(&vacuity_comment("cmd-x")),
            "b7dd3f8d8d1e82ae693e0e0a9c2ae2902ed07e17536412045c0f007bc1243dec"
        );
    }

    #[test]
    fn embedded_spec_block_is_byte_identical_to_python_json_dumps() {
        let receipt = TriageReceipt {
            command: "cmd".into(),
            exit_code: 1,
            base_sha: "S".into(),
            content_hash: "H".into(),
        };
        let plan = TriagePlan {
            authoritative_grader: "cmd".into(),
            graders: vec![
                json!({"type": "rubric", "criteria": ["a", "b"]})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ],
            goal: Some("G\u{e9} \"q\"".into()),
            context: json!({"relevant": ["p"]}).as_object().cloned(),
            tags: vec!["t".into()],
            prerequisites: vec!["gh-1".into()],
        };
        let body = embed_spec_block(
            "Prose here.\n\n```flywheel\n{\"old\": 1}\n```\n",
            &plan,
            &receipt,
        );
        assert!(
            body.starts_with(
                "Prose here.\n\n```flywheel\n{\n  \"goal\": \"G\\u00e9 \\\"q\\\"\",\n"
            )
        );
        assert_eq!(
            sha256_hex(&body),
            "a62b66bd62408ba0244d53129a4253b3d7e72a92d4f37391f1c0b77f954ee0a9"
        );
        // An empty body and an empty context: block only, `context` omitted.
        let bare = TriagePlan {
            authoritative_grader: "c".into(),
            context: Some(Map::new()),
            ..TriagePlan::default()
        };
        let body = embed_spec_block("", &bare, &receipt);
        assert!(body.starts_with("```flywheel\n"));
        assert!(!body.contains("context"));
        assert_eq!(
            sha256_hex(&body),
            "0836cd6cdc6af59e2ee849523c7ae0a81d3d3c91d99718b05c319f63d592458b"
        );
    }

    #[test]
    fn py_json_dumps_escapes_non_ascii_as_python_does() {
        assert_eq!(
            py_json_dumps_indent2(&json!({"k": "\u{1F600}\u{e9}"})),
            "{\n  \"k\": \"\\ud83d\\ude00\\u00e9\"\n}"
        );
        assert_eq!(
            py_json_dumps_indent2(&json!({"a": [], "b": {}})),
            "{\n  \"a\": [],\n  \"b\": {}\n}"
        );
    }

    // --- ready: the fail-first happy path ------------------------------------------------

    #[test]
    fn fail_first_grader_flips_issue_to_ready() {
        let (title, prose) = ("Fix the widget", "Widget drops retries under load.");
        let gh = ScriptedGh::all(vec![issue(12, title, prose)]);
        let ex = ScriptedExecutor::exiting(1);
        let result = run(&pass(
            gh.clone(),
            ex.clone(),
            ScriptedInvoke::fixed(ready_response("uv run pytest tests/widget")),
        ));

        let ready = result.ready();
        assert_eq!(ready.len(), 1);
        assert!(result.needs_detail().is_empty());
        let ready = ready[0];
        assert_eq!(ready.number, 12);
        assert_eq!(ready.source_ref, "octo/widgets#12");
        assert_eq!(ready.decision, Decision::Ready);
        let receipt = ready.receipt.as_ref().expect("receipt");
        assert_eq!(receipt.command, "uv run pytest tests/widget");
        assert_eq!(receipt.exit_code, 1);
        assert_eq!(receipt.base_sha, BASE_SHA);
        assert_eq!(receipt.content_hash, content_hash(title, prose));

        assert_eq!(ex.commands(), ["uv run pytest tests/widget"]);

        // Two reads (intake, ready), one edit, no comment.
        let lists: Vec<Vec<String>> = gh.calls().into_iter().filter(|a| a[1] == "list").collect();
        assert_eq!(lists.len(), 2);
        assert_eq!(
            lists[0],
            [
                "issue",
                "list",
                "--repo",
                REPO,
                "--label",
                "intake",
                "--state",
                "open",
                "--json",
                "number,title,body,url",
                "--limit",
                "200"
            ]
        );
        assert_eq!(arg_after(&lists[1], "--label"), "ready");
        let edits = gh.edits();
        assert_eq!(edits.len(), 1);
        let edit = &edits[0];
        assert_eq!(&edit[..3], ["issue", "edit", "12"]);
        assert_eq!(arg_after(edit, "--repo"), REPO);
        assert_eq!(arg_after(edit, "--add-label"), "ready");
        assert_eq!(arg_after(edit, "--remove-label"), "intake");
        let body = body_of(edit);
        assert!(body.contains(prose));
        assert!(body.contains("```flywheel"));
        assert!(body.contains(RECEIPT_KEY));
        assert!(gh.comments().is_empty());
    }

    #[test]
    fn ready_block_carries_the_authoritative_command_as_a_grader() {
        let gh = ScriptedGh::all(vec![issue(3, "Fix the widget", "Widget drops retries.")]);
        let response = fenced(json!({
            "authoritative_grader": "cmd-x",
            "graders": [{"type": "rubric", "criteria": ["c"]}],
        }));
        run(&pass(
            gh.clone(),
            ScriptedExecutor::exiting(2),
            ScriptedInvoke::fixed(response),
        ));

        let block = extract_block(body_of(&gh.edits()[0]));
        let graders = block["graders"].as_array().unwrap();
        assert_eq!(graders.len(), 2);
        assert_eq!(graders[0], json!({"type": "command", "run": "cmd-x"}));
        assert_eq!(block[RECEIPT_KEY]["command"], "cmd-x");
        assert_eq!(block[RECEIPT_KEY]["exit_code"], 2);
    }

    #[test]
    fn ready_flip_is_ordered_after_executor_and_uses_observed_exit_code() {
        let forged = fenced(json!({
            "authoritative_grader": "cmd",
            "graders": [{"type": "command", "run": "cmd"}],
            "exit_code": 0,
        }));
        let trace = events();
        let gh = ScriptedGh::all_with_events(
            vec![issue(5, "Fix the widget", "Widget drops retries.")],
            trace.clone(),
        );
        let ex = ScriptedExecutor::exiting_with_events(7, trace.clone());
        let result = run(&pass(
            gh.clone(),
            ex,
            ScriptedInvoke::fixed_with_events(forged, trace.clone()),
        ));

        let ready = result.ready();
        assert_eq!(ready[0].receipt.as_ref().unwrap().exit_code, 7);

        let kinds: Vec<String> = trace
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(kinds, ["invoke", "exec", "gh"]);

        let block = extract_block(body_of(&gh.edits()[0]));
        assert_eq!(block[RECEIPT_KEY]["exit_code"], 7);
    }

    // --- needs-detail: vacuous check -------------------------------------------------

    #[test]
    fn grader_passing_on_base_routes_to_needs_detail_as_vacuous() {
        let gh = ScriptedGh::all(vec![issue(9, "Fix the widget", "Widget drops retries.")]);
        let result = run(&pass(
            gh.clone(),
            ScriptedExecutor::exiting(0),
            ScriptedInvoke::fixed(ready_response("true")),
        ));

        assert!(result.ready().is_empty());
        let nd = result.needs_detail();
        assert_eq!(nd.len(), 1);
        assert_eq!(nd[0].decision, Decision::NeedsDetail);
        assert_eq!(nd[0].exit_code, Some(0));
        assert_eq!(nd[0].authoritative_command.as_deref(), Some("true"));
        assert!(nd[0].receipt.is_none());

        let edits = gh.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0],
            [
                "issue",
                "edit",
                "9",
                "--repo",
                REPO,
                "--add-label",
                "needs-detail",
                "--remove-label",
                "intake"
            ]
        );
        let comments = gh.comments();
        assert_eq!(comments.len(), 1);
        assert_eq!(&comments[0][..5], ["issue", "comment", "9", "--repo", REPO]);
        let text = body_of(&comments[0]);
        assert!(text.to_lowercase().contains("vacuous"));
        assert!(text.contains("    true\n"));
        assert_eq!(nd[0].comment.as_deref(), Some(text));
    }

    // --- needs-detail: cannot compile -----------------------------------------------

    #[test]
    fn cannot_compile_routes_to_needs_detail_naming_the_specific_gaps() {
        let gaps = [
            "which module owns the retry logic -- src/widget.py or src/net.py",
            "the expected retry count and backoff policy",
        ];
        let response = fenced(json!({"authoritative_grader": "", "missing_information": gaps}));
        let gh = ScriptedGh::all(vec![issue(4, "Make it better", "please improve retries")]);
        let ex = ScriptedExecutor::exiting(1);
        let result = run(&pass(
            gh.clone(),
            ex.clone(),
            ScriptedInvoke::fixed(response),
        ));

        assert!(result.ready().is_empty());
        let nd = result.needs_detail();
        assert_eq!(nd.len(), 1);
        assert_eq!(nd[0].missing_information, gaps);
        assert!(nd[0].authoritative_command.is_none());
        assert!(ex.commands().is_empty());

        let comments = gh.comments();
        assert_eq!(comments.len(), 1);
        let text = body_of(&comments[0]);
        for gap in gaps {
            assert!(text.contains(gap));
        }
        let edits = gh.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(arg_after(&edits[0], "--add-label"), "needs-detail");
        assert_eq!(arg_after(&edits[0], "--remove-label"), "intake");
        assert!(!has_flag(&edits[0], "--body"));
    }

    #[test]
    fn cannot_compile_issue_yields_no_workitem_from_the_drain() {
        let response =
            fenced(json!({"authoritative_grader": "", "missing_information": ["name the module"]}));
        let item = issue(4, "Make it better", "please improve retries");
        let gh = ScriptedGh::all(vec![item.clone()]);
        run(&pass(
            gh,
            ScriptedExecutor::exiting(1),
            ScriptedInvoke::fixed(response),
        ));

        let drain = ScriptedGh::all(vec![item]);
        let source = GithubWorkSource::new(REPO, "ready")
            .unwrap()
            .with_runner(drain)
            .with_require_triage_receipt(true);
        assert!(source.list_work().unwrap().is_empty());
    }

    // --- pre-existing spec block still requires the fail-first proof ----------------------

    #[test]
    fn human_block_does_not_short_circuit_when_grader_passes_on_base() {
        let (title, prose) = ("Fix the widget", "Widget drops retries.");
        let forged = json!({
            "command": "old",
            "exit_code": 1,
            "base_sha": "OLDBASE",
            "content_hash": content_hash(title, prose),
        });
        let gh = ScriptedGh::all(vec![issue_with_block(
            7,
            title,
            prose,
            json!({RECEIPT_KEY: forged}),
        )]);
        let result = run(&pass(
            gh,
            ScriptedExecutor::exiting(0),
            ScriptedInvoke::fixed(ready_response("true")),
        ));
        assert!(result.ready().is_empty());
        assert_eq!(result.needs_detail().len(), 1);
    }

    #[test]
    fn human_block_is_replaced_by_a_fresh_receipt_when_grader_fails() {
        let (title, prose) = ("Fix the widget", "Widget drops retries.");
        let gh = ScriptedGh::all(vec![issue_with_block(
            8,
            title,
            prose,
            json!({RECEIPT_KEY: stale_receipt("old")}),
        )]);
        let result = run(&pass(
            gh.clone(),
            ScriptedExecutor::exiting(1),
            ScriptedInvoke::fixed(ready_response("uv run pytest tests/widget")),
        ));

        let ready = result.ready();
        let receipt = ready[0].receipt.as_ref().unwrap();
        assert_eq!(receipt.base_sha, BASE_SHA);
        assert_eq!(receipt.command, "uv run pytest tests/widget");

        let edits = gh.edits();
        let body = body_of(&edits[0]);
        assert_eq!(body.matches("```flywheel").count(), 1);
        assert!(body.contains(prose));
        let block = extract_block(body);
        assert_eq!(block[RECEIPT_KEY]["base_sha"], BASE_SHA);
        assert_eq!(block[RECEIPT_KEY]["command"], "uv run pytest tests/widget");
    }

    // --- end to end: a ready issue is schedulable by the drain -------------------------------

    #[test]
    fn ready_issue_is_scheduled_by_the_drain_trust_rule() {
        let (title, prose) = ("Fix the widget", "Widget drops retries under load.");
        let gh = ScriptedGh::all(vec![issue(12, title, prose)]);
        run(&pass(
            gh.clone(),
            ScriptedExecutor::exiting(1),
            ScriptedInvoke::fixed(ready_response("uv run pytest tests/widget")),
        ));
        let new_body = body_of(&gh.edits()[0]).to_string();

        let drain = ScriptedGh::all(vec![issue(12, title, &new_body)]);
        let source = GithubWorkSource::new(REPO, "ready")
            .unwrap()
            .with_runner(drain)
            .with_require_triage_receipt(true);
        let items = source.list_work().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].task.id, "gh-12");
        match &items[0].task.graders[0] {
            flywheel_core::task::Grader::Command(g) => {
                assert_eq!(g.run, "uv run pytest tests/widget")
            }
            other => panic!("expected a command grader, got {other:?}"),
        }
    }

    // --- a whole pass partitions the board ------------------------------------------------

    #[test]
    fn pass_partitions_ready_and_needs_detail_across_the_board() {
        let ready = fenced(json!({
            "authoritative_grader": "uv run pytest tests/alpha",
            "graders": [{"type": "command", "run": "uv run pytest tests/alpha"}],
        }));
        let stuck =
            fenced(json!({"authoritative_grader": "", "missing_information": ["name the module"]}));
        let gh = ScriptedGh::all(vec![
            issue(1, "Alpha needs a retry fix", "alpha drops work"),
            issue(2, "Beta is vague", "make beta nicer"),
        ]);
        let invoker = ScriptedInvoke::routing(vec![("Alpha", ready), ("Beta", stuck)]);
        let result = run(&pass(gh.clone(), ScriptedExecutor::exiting(1), invoker));

        let ready_numbers: Vec<i64> = result.ready().iter().map(|o| o.number).collect();
        let nd_numbers: Vec<i64> = result.needs_detail().iter().map(|o| o.number).collect();
        assert_eq!(ready_numbers, [1]);
        assert_eq!(nd_numbers, [2]);
        let comments = gh.comments();
        assert_eq!(comments.len(), 1);
        assert_eq!(&comments[0][..3], ["issue", "comment", "2"]);
        assert_eq!(gh.edits().len(), 2);
    }

    // --- the authoring prompt and response parser (unit) ---------------------------------

    #[test]
    fn authoring_prompt_targets_stated_intent_and_asks_for_specific_gaps() {
        let prompt = triage_authoring_prompt("Fix the widget", "Widget drops retries under load.");
        assert!(prompt.contains("Widget drops retries under load."));
        assert!(prompt.contains("Fix the widget"));
        let low = prompt.to_lowercase();
        assert!(low.contains("intent"));
        assert!(low.contains("unrelated"));
        assert!(low.contains("fails today"));
        assert!(low.contains("specific"));
        // The human text is read with the spec block stripped.
        let with_block =
            triage_authoring_prompt("T", "Prose.\n\n```flywheel\n{\"goal\": \"x\"}\n```");
        assert!(with_block.contains("Issue body (human-authored):\nProse.\n\nHARD RULES"));
        assert!(!with_block.contains("```flywheel"));
    }

    #[test]
    fn parse_authoring_response_variants() {
        match parse_authoring_response(&ready_response("cmd")) {
            AuthoringResponse::Plan(plan) => {
                assert_eq!(plan.authoritative_grader, "cmd");
                assert_eq!(
                    plan.goal.as_deref(),
                    Some("Widget retries dropped requests under load.")
                );
                assert_eq!(plan.graders.len(), 1);
            }
            other => panic!("expected a plan, got {other:?}"),
        }
        let stuck = parse_authoring_response(&fenced(
            json!({"authoritative_grader": "", "missing_information": ["a", "", 3, "b"]}),
        ));
        assert_eq!(
            stuck,
            AuthoringResponse::CannotCompile(CannotCompile {
                missing_information: vec!["a".into(), "b".into()]
            })
        );
        assert_eq!(
            parse_authoring_response("not json at all"),
            AuthoringResponse::CannotCompile(CannotCompile::default())
        );
        // No fence: the outermost brace span still parses.
        match parse_authoring_response(
            "Sure: {\"authoritative_grader\": \" cmd \", \"tags\": [\"x\", 1]} done",
        ) {
            AuthoringResponse::Plan(plan) => {
                assert_eq!(plan.authoritative_grader, "cmd");
                assert_eq!(plan.tags, ["x", "1"]);
            }
            other => panic!("expected a plan, got {other:?}"),
        }
        // A closed fence holding invalid JSON is cannot-compile, not a brace-span fallback.
        assert_eq!(
            parse_authoring_response("```json\n{bad\n```\n{\"authoritative_grader\": \"c\"}"),
            AuthoringResponse::CannotCompile(CannotCompile::default())
        );
        // A non-object JSON payload is cannot-compile.
        assert_eq!(
            parse_authoring_response(&fenced(json!(["list"]))),
            AuthoringResponse::CannotCompile(CannotCompile::default())
        );
    }

    #[test]
    fn spec_block_reader_is_tolerant() {
        assert!(spec_block_of("no block").is_none());
        assert!(spec_block_of("```flywheel\n{\"a\": 1}").is_none());
        assert!(spec_block_of("```flywheel\nnot json\n```").is_none());
        assert!(spec_block_of("```flywheel\n[1]\n```").is_none());
        let block = spec_block_of("x\n  ```flywheel \n{\"a\": 1}\n ```\ny").unwrap();
        assert_eq!(block["a"], 1);
    }

    #[test]
    fn ready_needs_retriage_covers_every_untrustworthy_shape() {
        let (title, prose) = ("Fix", "Prose.");
        let good = json!({
            "command": "c", "exit_code": 1, "base_sha": "S",
            "content_hash": content_hash(title, prose),
        });
        assert!(!ready_needs_retriage(
            title,
            &body_with_receipt(prose, &good)
        ));
        assert!(ready_needs_retriage(
            title,
            &body_with_receipt(prose, &stale_receipt("c"))
        ));
        let mut passing = good.clone();
        passing["exit_code"] = json!(0);
        assert!(ready_needs_retriage(
            title,
            &body_with_receipt(prose, &passing)
        ));
        assert!(ready_needs_retriage(title, prose));
        assert!(ready_needs_retriage(
            "Edited",
            &body_with_receipt(prose, &good)
        ));
    }

    #[test]
    fn crlf_issue_body_mints_a_receipt_that_stays_settled_once_embedded() {
        // Bodies typed in GitHub's web editor arrive with CRLF line endings.
        // The receipt is minted from the block-less body, then re-checked
        // against the body carrying the embedded block; both reads must
        // hash the same human content.
        let title = "Fix the widget";
        let prose = "Widget drops retries.\r\n\r\nRepro: run the load test.\r\n";
        let gh = ScriptedGh::all(vec![issue(9, title, prose)]);
        let ex = ScriptedExecutor::exiting(1);
        let result = run(&pass(
            gh.clone(),
            ex,
            ScriptedInvoke::fixed(ready_response("cmd")),
        ));
        assert_eq!(result.ready().len(), 1);
        let edits = gh.edits();
        assert_eq!(edits.len(), 1);
        let written = body_of(&edits[0]).to_string();

        assert!(
            !ready_needs_retriage(title, &written),
            "the freshly minted receipt must not read as drifted"
        );
        assert_eq!(
            crate::github::triage_skip_reason(Some(&extract_block(&written)), title, &written),
            None
        );

        // The next pass sees the issue ready and settled: no agent, no grader, no write.
        let gh2 = ScriptedGh::by_label(vec![("ready", vec![issue(9, title, &written)])]);
        let ex2 = ScriptedExecutor::exiting(1);
        run(&pass(
            gh2.clone(),
            ex2.clone(),
            ScriptedInvoke::fixed(ready_response("cmd")),
        ));
        assert!(ex2.commands().is_empty());
        assert!(gh2.edits().is_empty());
    }

    // --- drift: a drifted ready issue is re-triaged within the pass ---------------------------

    #[test]
    fn drifted_ready_issue_is_retriaged_and_reminted_ready() {
        let (title, prose) = ("Fix the widget", "Widget drops retries under load.");
        let drifted = issue(
            7,
            title,
            &body_with_receipt(prose, &stale_receipt("old-cmd")),
        );
        let gh = ScriptedGh::by_label(vec![("ready", vec![drifted])]);
        let ex = ScriptedExecutor::exiting(1);
        let result = run(&pass(
            gh.clone(),
            ex.clone(),
            ScriptedInvoke::fixed(ready_response("new-cmd")),
        ));

        let ready = result.ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].number, 7);
        let receipt = ready[0].receipt.as_ref().unwrap();
        assert_eq!(receipt.command, "new-cmd");
        assert_eq!(receipt.base_sha, BASE_SHA);
        assert_eq!(receipt.content_hash, content_hash(title, prose));
        assert_eq!(ex.commands(), ["new-cmd"]);

        let edits = gh.edits();
        assert_eq!(edits.len(), 1);
        assert!(gh.comments().is_empty());
        let edit = &edits[0];
        let body = body_of(edit);
        assert_eq!(body.matches("```flywheel").count(), 1);
        assert!(!body.contains("deadbeefdeadbeef"));
        assert!(!body.contains("OLDBASE"));
        let block = extract_block(body);
        assert_eq!(
            block[RECEIPT_KEY]["content_hash"],
            content_hash(title, prose)
        );
        assert_eq!(block[RECEIPT_KEY]["base_sha"], BASE_SHA);
        assert_eq!(arg_after(edit, "--add-label"), "ready");
        assert!(!has_flag(edit, "--remove-label"));
        assert_eq!(&edit[..5], ["issue", "edit", "7", "--repo", REPO]);
    }

    #[test]
    fn drift_retriage_to_needs_detail_strips_the_stale_receipt() {
        let (title, prose) = ("Fix the widget", "Widget drops retries.");
        let drifted = issue(
            8,
            title,
            &body_with_receipt(prose, &stale_receipt("old-cmd")),
        );
        let gh = ScriptedGh::by_label(vec![("ready", vec![drifted])]);
        let result = run(&pass(
            gh.clone(),
            ScriptedExecutor::exiting(0),
            ScriptedInvoke::fixed(ready_response("true")),
        ));

        assert!(result.ready().is_empty());
        let nd = result.needs_detail();
        assert_eq!(nd.len(), 1);
        assert_eq!(nd[0].number, 8);

        let edits = gh.edits();
        assert_eq!(edits.len(), 1);
        let body = body_of(&edits[0]);
        assert!(!body.contains("```flywheel"));
        assert!(!body.contains("deadbeefdeadbeef"));
        assert_eq!(body, prose);
        assert_eq!(arg_after(&edits[0], "--add-label"), "needs-detail");
        assert_eq!(arg_after(&edits[0], "--remove-label"), "ready");

        let comments = gh.comments();
        assert_eq!(comments.len(), 1);
        assert!(body_of(&comments[0]).to_lowercase().contains("vacuous"));
    }

    #[test]
    fn drift_retriage_cannot_compile_strips_block_and_names_gaps() {
        let (title, prose) = ("Make it better", "please improve retries");
        let drifted = issue(
            9,
            title,
            &body_with_receipt(prose, &stale_receipt("old-cmd")),
        );
        let gh = ScriptedGh::by_label(vec![("ready", vec![drifted])]);
        let ex = ScriptedExecutor::exiting(1);
        let gap = "which module owns the retry logic -- src/widget.py or src/net.py";
        let response = fenced(json!({"authoritative_grader": "", "missing_information": [gap]}));
        let result = run(&pass(
            gh.clone(),
            ex.clone(),
            ScriptedInvoke::fixed(response),
        ));

        assert!(result.ready().is_empty());
        let nd = result.needs_detail();
        assert_eq!(nd[0].missing_information, [gap]);
        assert!(ex.commands().is_empty());

        let edits = gh.edits();
        assert_eq!(edits.len(), 1);
        let body = body_of(&edits[0]);
        assert!(!body.contains("```flywheel"));
        assert!(!body.contains("deadbeefdeadbeef"));
        let comments = gh.comments();
        assert_eq!(comments.len(), 1);
        assert!(body_of(&comments[0]).contains(gap));
    }

    // --- idempotency: an unchanged second pass writes nothing -------------------------------

    #[test]
    fn second_pass_over_unchanged_state_records_zero_writes() {
        let (title, prose) = ("Fix the widget", "Widget drops retries under load.");
        let gh1 = ScriptedGh::by_label(vec![("intake", vec![issue(12, title, prose)])]);
        run(&pass(
            gh1.clone(),
            ScriptedExecutor::exiting(1),
            ScriptedInvoke::fixed(ready_response("uv run pytest tests/widget")),
        ));
        let ready_body = body_of(&gh1.edits()[0]).to_string();

        let gh2 = ScriptedGh::by_label(vec![
            ("intake", vec![]),
            ("ready", vec![issue(12, title, &ready_body)]),
            ("needs-detail", vec![issue(3, "Vague", "please improve")]),
        ]);
        let result = run(&pass(
            gh2.clone(),
            ScriptedExecutor::boom(),
            ScriptedInvoke::boom(),
        ));

        assert_eq!(result, TriagePassResult::default());
        assert!(gh2.writes().is_empty());
    }

    #[test]
    fn needs_detail_issue_is_not_recommented_on_later_passes() {
        let gh = ScriptedGh::by_label(vec![
            ("intake", vec![]),
            ("ready", vec![]),
            (
                "needs-detail",
                vec![issue(4, "Vague", "please improve retries")],
            ),
        ]);
        let result = run(&pass(
            gh.clone(),
            ScriptedExecutor::boom(),
            ScriptedInvoke::boom(),
        ));
        assert!(result.outcomes.is_empty());
        assert!(gh.writes().is_empty());
    }

    // --- the per-pass cap ----------------------------------------------------------------

    #[test]
    fn per_pass_cap_processes_cap_issues_and_defers_the_remainder() {
        let issues: Vec<Value> = [4, 1, 3, 2]
            .into_iter()
            .map(|n| issue(n, &format!("Fix {n}"), &format!("widget {n} drops retries")))
            .collect();
        let gh = ScriptedGh::by_label(vec![("intake", issues)]);
        let (log, lines) = capture_log();
        let triage = pass(
            gh.clone(),
            ScriptedExecutor::exiting(1),
            ScriptedInvoke::fixed(ready_response("cmd")),
        )
        .with_per_pass_cap(Some(2))
        .unwrap()
        .with_log(log);
        let result = run(&triage);

        let numbers: Vec<i64> = result.outcomes.iter().map(|o| o.number).collect();
        assert_eq!(numbers, [1, 2]);
        let edited: Vec<String> = gh.edits().iter().map(|e| e[2].clone()).collect();
        assert_eq!(edited, ["1", "2"]);
        assert!(gh.comments().is_empty());
        assert_eq!(result.deferred, 2);
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            [
                "[triage] per-pass cap 2 reached: processing 2 of 4 candidates, deferring 2 to a later pass"
            ]
        );
    }

    #[test]
    fn drift_on_a_ready_issue_counts_against_the_per_pass_cap() {
        let intake = vec![
            issue(1, "Alpha", "alpha drops work"),
            issue(3, "Gamma", "gamma drops work"),
        ];
        let drifted = issue(
            2,
            "Beta",
            &body_with_receipt("beta drifted", &stale_receipt("old-cmd")),
        );
        let gh = ScriptedGh::by_label(vec![("intake", intake), ("ready", vec![drifted])]);
        let (log, lines) = capture_log();
        let invoker = ScriptedInvoke::routing(vec![
            ("Alpha", ready_response("cmd-a")),
            ("Beta", ready_response("cmd-b")),
            ("Gamma", ready_response("cmd-c")),
        ]);
        let triage = pass(gh.clone(), ScriptedExecutor::exiting(1), invoker)
            .with_per_pass_cap(Some(2))
            .unwrap()
            .with_log(log);
        let result = run(&triage);

        let numbers: Vec<i64> = result.outcomes.iter().map(|o| o.number).collect();
        assert_eq!(numbers, [1, 2]);
        let edited: Vec<String> = gh.edits().iter().map(|e| e[2].clone()).collect();
        assert_eq!(edited, ["1", "2"]);
        assert_eq!(result.deferred, 1);
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("deferring 1"))
        );

        let beta = result.outcomes.iter().find(|o| o.number == 2).unwrap();
        assert_eq!(beta.decision, Decision::Ready);
        assert_eq!(beta.receipt.as_ref().unwrap().command, "cmd-b");
    }

    #[test]
    fn construction_validates_cap_and_base_sha() {
        let seams = TriageSeams {
            invoker: ScriptedInvoke::boom(),
            executor: ScriptedExecutor::boom(),
            runner: ScriptedGh::by_label(vec![]),
        };
        assert!(matches!(
            TriagePass::new(board(), "", seams.clone()),
            Err(TriageError::EmptyBaseSha)
        ));
        let triage = TriagePass::new(board(), BASE_SHA, seams).unwrap();
        assert!(matches!(
            triage.with_per_pass_cap(Some(0)),
            Err(TriageError::InvalidPerPassCap)
        ));
    }

    // --- board reads ---------------------------------------------------------------------

    #[test]
    fn malformed_issue_numbers_are_skipped_and_logged() {
        let bad = json!({"number": "seven", "title": "x", "body": "y"});
        let gh = ScriptedGh::by_label(vec![("intake", vec![bad.clone(), json!("not an object")])]);
        let (log, lines) = capture_log();
        let triage =
            pass(gh.clone(), ScriptedExecutor::boom(), ScriptedInvoke::boom()).with_log(log);
        let result = run(&triage);
        assert!(result.outcomes.is_empty());
        assert!(gh.writes().is_empty());
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            [format!(
                "[triage] skipping issue with non-integer number: {bad}"
            )]
        );
    }

    #[test]
    fn list_errors_name_the_malformed_payload() {
        struct Stdout(&'static str);
        impl GhRunner for Stdout {
            fn run_gh(&self, _argv: &[String]) -> Result<crate::github::GhOutput, WorkSourceError> {
                Ok(crate::github::GhOutput::ok(self.0))
            }
        }
        let build = |runner: Arc<dyn GhRunner>| {
            TriagePass::new(
                board(),
                BASE_SHA,
                TriageSeams {
                    invoker: ScriptedInvoke::boom(),
                    executor: ScriptedExecutor::boom(),
                    runner,
                },
            )
            .unwrap()
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(build(Arc::new(Stdout("{\"a\": 1}"))).run())
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "gh issue list returned dict, expected a list"
        );
        let err = rt
            .block_on(build(Arc::new(Stdout("nope"))).run())
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("gh issue list returned invalid JSON: ")
        );
        assert_eq!(err.kind_name(), "TriageError");
        // Empty stdout is an empty board.
        let result = rt.block_on(build(Arc::new(Stdout(""))).run()).unwrap();
        assert_eq!(result, TriagePassResult::default());
        // A failing gh aborts the pass with the branded message.
        let failing = crate::github::test_support::FakeGh::failing("boom");
        let err = rt.block_on(build(failing).run()).unwrap_err();
        assert_eq!(err.kind_name(), "WorkSourceError");
        assert!(err.to_string().contains("failed (exit 1): boom"));
    }

    // --- per-candidate failure isolation --------------------------------------------------

    /// An authoring agent that errors on any prompt naming `poison` and
    /// answers every other prompt with a fail-first plan.
    struct PoisonInvoke {
        poison: &'static str,
    }

    impl TriageInvoke for PoisonInvoke {
        fn invoke<'a>(&'a self, prompt: &'a str) -> TriageInvokeFuture<'a> {
            Box::pin(async move {
                if prompt.contains(self.poison) {
                    Err(TriageError::Invoke(
                        "prompt exceeds the context window".into(),
                    ))
                } else {
                    Ok(ready_response("cargo test widget_retry"))
                }
            })
        }
    }

    /// Delegates to [`ScriptedGh`] but refuses every `issue edit` of `number`
    /// (e.g. a body GitHub rejects as too long).
    struct EditRefusingGh {
        inner: Arc<ScriptedGh>,
        number: &'static str,
    }

    impl GhRunner for EditRefusingGh {
        fn run_gh(&self, argv: &[String]) -> Result<crate::github::GhOutput, WorkSourceError> {
            if argv.len() >= 3 && argv[0] == "issue" && argv[1] == "edit" && argv[2] == self.number
            {
                self.inner.calls.lock().unwrap().push(argv.to_vec());
                return Ok(crate::github::GhOutput {
                    stdout: String::new(),
                    stderr: "body is too long (maximum is 65536 characters)".into(),
                    exit_code: Some(1),
                });
            }
            self.inner.run_gh(argv)
        }
    }

    fn isolating_pass(gh: &Arc<ScriptedGh>, log: LogSink) -> TriagePass {
        TriagePass::new(
            board(),
            BASE_SHA,
            TriageSeams {
                invoker: Arc::new(PoisonInvoke { poison: "POISON" }),
                executor: ScriptedExecutor::exiting(1),
                runner: Arc::new(EditRefusingGh {
                    inner: Arc::clone(gh),
                    number: "3",
                }),
            },
        )
        .unwrap()
        .with_log(log)
    }

    #[test]
    fn a_failing_candidate_does_not_starve_the_rest_of_the_board() {
        // #1 (lowest number, processed first) fails at the authoring agent;
        // #3 fails at its board write. Neither may abort the pass: #2 and #4
        // are still triaged, and the failures are logged and left for a
        // later pass instead of blocking every pass forever.
        let gh = ScriptedGh::by_label(vec![(
            "intake",
            vec![
                issue(4, "Fix 4", "widget 4 drops retries"),
                issue(1, "Fix 1", "POISON widget 1 drops retries"),
                issue(3, "Fix 3", "widget 3 drops retries"),
                issue(2, "Fix 2", "widget 2 drops retries"),
            ],
        )]);
        let (log, lines) = capture_log();
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(isolating_pass(&gh, log).run())
            .expect("per-candidate failures must not abort the pass");

        let numbers: Vec<i64> = result.outcomes.iter().map(|o| o.number).collect();
        assert_eq!(numbers, [2, 4]);
        assert!(
            result
                .outcomes
                .iter()
                .all(|o| o.decision == Decision::Ready)
        );
        let edited: Vec<String> = gh.edits().iter().map(|e| e[2].clone()).collect();
        assert_eq!(edited, ["2", "3", "4"]);
        assert!(gh.comments().is_empty());
        let lines = lines.lock().unwrap().clone();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0],
            "[triage] #1 failed (InvokeError: authoring agent failed: prompt exceeds the \
             context window); skipped this pass"
        );
        assert!(
            lines[1].starts_with("[triage] #3 failed (WorkSourceError: gh issue edit 3 ")
                && lines[1].ends_with(
                    "failed (exit 1): body is too long (maximum is 65536 characters)); \
                     skipped this pass"
                ),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn failed_candidates_are_recorded_and_retried_on_the_next_pass() {
        let gh = ScriptedGh::by_label(vec![(
            "intake",
            vec![
                issue(1, "Fix 1", "POISON widget 1 drops retries"),
                issue(2, "Fix 2", "widget 2 drops retries"),
            ],
        )]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (log, _lines) = capture_log();
        let result = rt.block_on(isolating_pass(&gh, log).run()).unwrap();
        assert_eq!(
            result.failed,
            [TriageFailure {
                number: 1,
                kind: "InvokeError",
                message: "authoring agent failed: prompt exceeds the context window".into(),
            }]
        );
        assert_eq!(result.ready().len(), 1);
        assert_eq!(result.deferred, 0);
        // The failed issue was not written, so it is still an intake
        // candidate: the next pass attempts it again.
        let (log, lines) = capture_log();
        rt.block_on(isolating_pass(&gh, log).run()).unwrap();
        assert!(
            lines.lock().unwrap()[0].starts_with("[triage] #1 failed (InvokeError: "),
            "{:?}",
            lines.lock().unwrap()
        );
    }

    #[test]
    fn a_pass_where_every_candidate_fails_errors_with_the_first_failure() {
        // No candidate made progress: a systemic fault must still reach the
        // daemon's circuit breaker as a failed cycle.
        let gh = ScriptedGh::by_label(vec![(
            "intake",
            vec![
                issue(3, "Fix 3", "widget 3 drops retries"),
                issue(1, "Fix 1", "POISON widget 1 drops retries"),
            ],
        )]);
        let (log, lines) = capture_log();
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(isolating_pass(&gh, log).run())
            .unwrap_err();
        assert_eq!(err.kind_name(), "InvokeError");
        assert_eq!(
            err.to_string(),
            "authoring agent failed: prompt exceeds the context window"
        );
        // Both candidates were still attempted and logged.
        assert_eq!(lines.lock().unwrap().len(), 2);
        let edited: Vec<String> = gh.edits().iter().map(|e| e[2].clone()).collect();
        assert_eq!(edited, ["3"]);
    }

    #[test]
    fn a_full_issue_page_warns_that_the_listing_was_truncated() {
        // Entries the backlog skips (non-integer numbers), so the pass
        // writes nothing and runs no agent: only the page size matters.
        let full = vec![json!({"number": "n/a", "title": "t", "body": "b"}); LIST_LIMIT];
        let short = full[..LIST_LIMIT - 1].to_vec();
        for (issues, expect_warning) in [(full, true), (short, false)] {
            let gh = ScriptedGh::by_label(vec![("intake", issues)]);
            let (log, lines) = capture_log();
            let triage =
                pass(gh.clone(), ScriptedExecutor::boom(), ScriptedInvoke::boom()).with_log(log);
            run(&triage);
            let warnings: Vec<String> = lines
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.contains("truncated"))
                .cloned()
                .collect();
            if expect_warning {
                assert_eq!(
                    warnings,
                    [
                        "[triage] intake issue listing truncated at one page; some items were \
                         not read this pass"
                    ]
                );
            } else {
                assert!(warnings.is_empty(), "{warnings:?}");
            }
            assert!(gh.writes().is_empty());
        }
    }

    // --- production seams -----------------------------------------------------------------

    #[test]
    fn shell_executor_reports_the_observed_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ShellGraderExecutor::new(dir.path());
        assert_eq!(executor.execute("exit 0").unwrap(), 0);
        assert_eq!(executor.execute("exit 7").unwrap(), 7);
        assert_eq!(executor.execute("test -f marker").unwrap(), 1);
        std::fs::write(dir.path().join("marker"), "").unwrap();
        assert_eq!(executor.execute("test -f marker").unwrap(), 0);
        #[cfg(unix)]
        assert_eq!(executor.execute("kill -9 $$").unwrap(), -9);
    }

    #[test]
    fn resolve_base_sha_reads_head() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        let err = resolve_base_sha(dir.path()).unwrap_err();
        assert!(matches!(err, TriageError::Git(_)));
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        let sha = resolve_base_sha(dir.path()).unwrap();
        assert_eq!(sha.len(), 40);
        assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn agents_invoker_configures_a_cold_session_from_options() {
        let options = TriageAgentOptions {
            agent_id: "codex".into(),
            agent_transport: Some("cli".into()),
            model: Some("m".into()),
            max_turns: 9,
            deadline_seconds: Some(5.0),
        };
        let invoker = build_repo_invoker("/repo", options.repo_invoker_options());
        let configuration = invoker.configuration();
        assert_eq!(configuration.agent_id, "codex");
        assert_eq!(configuration.model_id.as_deref(), Some("m"));
        assert_eq!(configuration.max_turns, Some(9));
        assert_eq!(
            configuration.adapter_options.get("transport"),
            Some(&json!("cli"))
        );
        let defaults = TriageAgentOptions::default();
        assert_eq!(defaults.agent_id, DEFAULT_TRIAGE_AGENT_ID);
        assert_eq!(defaults.max_turns, DEFAULT_TRIAGE_MAX_TURNS);
        assert_eq!(
            defaults.deadline_seconds,
            Some(DEFAULT_AUTOPILOT_AGENT_SECONDS)
        );
        let _: Arc<dyn TriageInvoke> = build_triage_authoring_invoker("/repo", &defaults);
        let _: Arc<dyn TriageGraderExecutor> = build_grader_executor("/repo");
    }

    #[test]
    fn decision_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Decision::NeedsDetail).unwrap(),
            "\"needs_detail\""
        );
        assert_eq!(Decision::Ready.to_string(), "ready");
        assert_eq!(
            serde_json::from_str::<Decision>("\"needs_detail\"").unwrap(),
            Decision::NeedsDetail
        );
    }
}
