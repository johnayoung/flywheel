//! Agent-driven `flywheel init`: inspect the repo and propose the policy.
//!
//! `flywheel init` normally prompts (or takes defaults) for the handful of
//! choices it owns. `flywheel init --agent` automates the onboarding
//! *assessment* a human would otherwise do by hand: an agent reads the
//! repo, infers the toolchain, and proposes the verification/grader
//! commands, the autopilot cadence, the sandbox provisioning step, and any
//! `.gitignore` additions. This module owns that single agent round-trip
//! and the rendering of its proposal into a valid `flywheel.toml`.
//!
//! The agent seam mirrors the one in [`crate::autopilot`] exactly -- the
//! injectable [`AutopilotInvoker`], the agents-backed [`build_repo_invoker`]
//! driver, and the fenced-JSON extraction. The entry point
//! [`run_agent_init`] takes an optional invoker so a test scripts the agent
//! offline; the CLI handler injects the production driver. A proposal that
//! does not parse, or that renders a policy [`load_policy_str`] rejects,
//! raises [`InitAgentError`] and writes nothing -- init never lands a broken
//! policy.

use crate::autopilot::{
    AutopilotInvoker, InvokeError, RepoInvokerOptions, build_repo_invoker, extract_json_object,
};
use crate::policy::{DEFAULT_POLICY_FILENAME, PolicyError, WorkPolicy, load_policy_str};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Turn budget for the agents-backed init invoker. Init's assessment is a
/// single bounded inspection (read the repo, return one JSON proposal), so
/// it needs far fewer turns than the autopilot discovery/authoring fan-outs.
pub const DEFAULT_INIT_MAX_TURNS: u32 = 40;

/// The work-source kinds init's agent path is allowed to propose. Init only
/// renders the directory and github sources today (mirroring the
/// interactive `--source` choices); an unknown kind falls back to
/// `directory` before rendering.
pub const PROPOSAL_SOURCE_KINDS: [&str; 2] = ["directory", "github"];

/// The agent proposal could not be obtained, parsed, or rendered into a
/// valid policy.
///
/// Raised before anything is written, so an init-agent run that fails
/// leaves the working directory exactly as it was -- no half-written
/// `flywheel.toml`, no partial `.gitignore`.
#[derive(Debug, thiserror::Error)]
pub enum InitAgentError {
    /// The agent call itself failed (deadline, start, run failure).
    #[error("init agent error: {0}")]
    Agent(#[from] InvokeError),
    /// No JSON object could be extracted from the response.
    #[error("could not parse an init proposal from the agent response: {0}")]
    Parse(String),
    /// The rendered policy failed [`load_policy_str`] validation.
    #[error("the agent-proposed policy failed validation and was not written: {0}")]
    InvalidPolicy(#[source] PolicyError),
    /// Writing the policy or `.gitignore` failed.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The agent's parsed onboarding proposal.
///
/// Every field is the validated, typed projection of the agent's fenced
/// JSON (see [`parse_init_proposal`]); the agent never writes a path or
/// runs a command, it only *proposes* the strings init then renders and
/// re-validates. `default_graders` are the verification commands every
/// grader-less work item inherits; `sandbox_setup` is the provisioning
/// command run in each fresh sandbox; `target_depth`/`interval_seconds`
/// tune the autopilot intake daemon; `gitignore_additions` are lines
/// appended to `.gitignore`; `notes` is the agent's one-line rationale,
/// echoed to the operator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitProposal {
    pub source_kind: String,
    pub default_graders: Vec<String>,
    pub sandbox_setup: Option<String>,
    pub target_depth: Option<i64>,
    pub interval_seconds: Option<f64>,
    pub gitignore_additions: Vec<String>,
    pub notes: String,
}

impl Default for InitProposal {
    fn default() -> Self {
        Self {
            source_kind: "directory".to_owned(),
            default_graders: Vec::new(),
            sandbox_setup: None,
            target_depth: None,
            interval_seconds: None,
            gitignore_additions: Vec::new(),
            notes: String::new(),
        }
    }
}

/// Build the onboarding-assessment prompt for the init agent.
///
/// Asks the agent to inspect the repo and return exactly one fenced JSON
/// proposal in the shape [`parse_init_proposal`] reads. The contract is
/// pinned in the prompt so a scripted invoker can return the same shape,
/// and the agent is told to derive commands from checks that ALREADY exist
/// in the repo (a test/lint/build command), never to invent one.
pub fn init_prompt(repo_root: &Path) -> String {
    format!(
        "You are the onboarding-assessment agent for `flywheel init`.\n\n\
         Inspect the repository at {}: detect its toolchain (build system, test runner, \
         linter), and propose the flywheel policy a maintainer would write by hand.\n\n\
         Derive the default grader commands from checks that ALREADY exist in this repo \
         (the test/lint/build commands a contributor runs today). Do not invent a command \
         the repo cannot run. Propose a sandbox setup command that warms a fresh checkout \
         (dependency install/fetch) when one applies, the autopilot intake cadence, and \
         any build-output directories that belong in .gitignore.\n\n\
         Respond with exactly one fenced JSON block:\n\
         ```json\n\
         {{\n\
         \x20 \"source_kind\": \"directory\",\n\
         \x20 \"default_graders\": [\"<existing test command>\", \"<existing lint command>\"],\n\
         \x20 \"sandbox_setup\": \"<dependency install/fetch command, or null>\",\n\
         \x20 \"autopilot\": {{\"target_depth\": 3, \"interval_seconds\": 600}},\n\
         \x20 \"gitignore_additions\": [\"<build-output dir>\"],\n\
         \x20 \"notes\": \"one sentence describing the detected toolchain\"\n\
         }}\n\
         ```",
        repo_root.display(),
    )
}

/// Coerce an agent-supplied list into the trimmed, non-empty strings it
/// holds.
fn coerce_str_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(entries)) => entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Coerce an agent-supplied positive int, else `None` (use the default).
fn coerce_optional_int(value: Option<&Value>) -> Option<i64> {
    let Some(Value::Number(n)) = value else {
        return None;
    };
    if let Some(i) = n.as_i64() {
        return (i > 0).then_some(i);
    }
    let f = n.as_f64()?;
    (f > 0.0 && f.fract() == 0.0).then_some(f as i64)
}

/// Coerce an agent-supplied positive number, else `None` (use default).
fn coerce_optional_number(value: Option<&Value>) -> Option<f64> {
    let Some(Value::Number(n)) = value else {
        return None;
    };
    n.as_f64().filter(|f| *f > 0.0)
}

/// Parse the agent's response text into a typed [`InitProposal`].
///
/// Pulls the fenced JSON object (via the shared autopilot extractor) and
/// projects each field with defensive coercion -- a missing or wrong-typed
/// field falls back to its default rather than failing, so a slightly
/// off-contract response still yields a usable proposal. Fails only when no
/// JSON object can be extracted at all (the unrecoverable case), so init
/// fails loudly instead of writing a default-everything policy the operator
/// did not ask for.
pub fn parse_init_proposal(text: &str) -> Result<InitProposal, InitAgentError> {
    let data = extract_json_object(text).map_err(InitAgentError::Parse)?;

    let source_kind = data
        .get("source_kind")
        .and_then(Value::as_str)
        .filter(|kind| PROPOSAL_SOURCE_KINDS.contains(kind))
        .unwrap_or("directory")
        .to_owned();

    let sandbox_setup = data
        .get("sandbox_setup")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let (target_depth, interval_seconds) = match data.get("autopilot") {
        Some(Value::Object(autopilot)) => (
            coerce_optional_int(autopilot.get("target_depth")),
            coerce_optional_number(autopilot.get("interval_seconds")),
        ),
        _ => (None, None),
    };

    let notes = data
        .get("notes")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();

    Ok(InitProposal {
        source_kind,
        default_graders: coerce_str_list(data.get("default_graders")),
        sandbox_setup,
        target_depth,
        interval_seconds,
        gitignore_additions: coerce_str_list(data.get("gitignore_additions")),
        notes,
    })
}

/// A TOML basic string for `value` (JSON string quoting is valid TOML
/// basic-string escaping, so an arbitrary command -- quotes, backslashes --
/// cannot break the rendered file).
fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string serializes")
}

/// Render one `[[defaults.graders]]` array-of-tables entry.
fn render_grader_block(run: &str) -> String {
    format!(
        "[[defaults.graders]]\ntype = \"command\"\nrun = {}\n",
        toml_string(run)
    )
}

/// Python `repr(float)` for a finite, non-integral float: the shortest
/// round-tripping decimal, which `{}` on an `f64` also produces.
fn render_interval(interval: f64) -> String {
    if interval.fract() == 0.0 {
        format!("{}", interval as i64)
    } else {
        format!("{interval}")
    }
}

/// Render a complete `flywheel.toml` from an agent proposal.
///
/// The layout mirrors the hand-written init scaffold (source -> store ->
/// paths -> defaults.graders -> sandbox -> autopilot -> submit), but the
/// grader/sandbox/autopilot sections are ACTIVE keys derived from the
/// proposal rather than commented suggestions. Every string is quoted as a
/// TOML basic string, so an arbitrary command or branch name cannot break
/// the file. The rendered text is re-validated by [`load_policy_str`] in
/// [`run_agent_init`] before it is written.
pub fn render_agent_policy(proposal: &InitProposal, submit_base: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(
        "# Flywheel work policy proposed by `flywheel init --agent`.\n\
         # An agent inspected the repo and derived these settings; review and edit freely.\n\
         # Committed with the repo; CLI flags always override.\n",
    );
    out.push('\n');
    out.push_str("[source]\nkind = \"directory\"\ntasks_dir = \".flywheel/tasks\"\n");
    out.push('\n');
    out.push_str("[store]\nbackend = \"sqlite\"\n");
    out.push('\n');
    out.push_str(
        "[paths]\n\
         db = \".flywheel/flywheel.sqlite\"\n\
         sandbox_root = \".flywheel/worktrees\"\n",
    );

    if !proposal.default_graders.is_empty() {
        out.push('\n');
        out.push_str(
            "# Default graders for work items that declare none (the verification\n\
             # commands the agent derived from this repo's existing checks).\n",
        );
        out.push_str(
            &proposal
                .default_graders
                .iter()
                .map(|run| render_grader_block(run))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    if let Some(setup) = &proposal.sandbox_setup {
        out.push('\n');
        out.push_str(
            "# Sandbox provisioning: runs (shell) inside every newly created sandbox\n\
             # before the agent enters.\n",
        );
        out.push_str(&format!("[sandbox]\nsetup = {}\n", toml_string(setup)));
    }

    if proposal.target_depth.is_some() || proposal.interval_seconds.is_some() {
        out.push('\n');
        out.push_str(
            "# Autopilot intake daemon cadence (target queue depth and refill interval).\n",
        );
        out.push_str("[autopilot]\n");
        if let Some(depth) = proposal.target_depth {
            out.push_str(&format!("target_depth = {depth}\n"));
        }
        if let Some(interval) = proposal.interval_seconds {
            out.push_str(&format!(
                "interval_seconds = {}\n",
                render_interval(interval)
            ));
        }
    }

    out.push('\n');
    let base = submit_base.filter(|b| !b.is_empty());
    let suggestion = base.map_or_else(|| "\"main\"".to_owned(), toml_string);
    let detected_note =
        base.map_or_else(String::new, |b| format!(" (detected current branch: {b})"));
    out.push_str(&format!(
        "# Landing policy. Leave base UNSET to FF-merge work in-tree onto your\n\
         # checked-out branch{detected_note}. Set base ONLY to a separate\n\
         # integration branch you do NOT have checked out.\n\
         [submit]\n\
         # base = {suggestion}\n"
    ));
    out
}

/// The outcome of an agent-driven init: the proposal and what it wrote.
///
/// `policy_path` is the written `flywheel.toml`; `gitignore_added` are the
/// lines actually appended to `.gitignore` this run (empty when the
/// proposal added none or they were already present). `policy` is the
/// re-validated [`WorkPolicy`] parsed from the written text -- proof the
/// emitted policy loads.
#[derive(Debug, Clone, PartialEq)]
pub struct InitAgentResult {
    pub proposal: InitProposal,
    pub policy_path: PathBuf,
    pub policy: WorkPolicy,
    pub gitignore_added: Vec<String>,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> InitAgentError + '_ {
    move |source| InitAgentError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Append never-before-seen `additions` to `root/.gitignore`.
///
/// Creates the file when absent. A line already present (exact match,
/// after stripping trailing whitespace) is skipped so a second run never
/// duplicates an entry. Returns the lines actually written this call.
fn append_gitignore(additions: &[String], root: &Path) -> Result<Vec<String>, InitAgentError> {
    if additions.is_empty() {
        return Ok(Vec::new());
    }
    let gitignore = root.join(".gitignore");
    let prior_text = if gitignore.is_file() {
        std::fs::read_to_string(&gitignore).map_err(io_err(&gitignore))?
    } else {
        String::new()
    };
    let existing: std::collections::BTreeSet<&str> =
        prior_text.lines().map(str::trim_end).collect();
    let to_add: Vec<String> = additions
        .iter()
        .filter(|a| !existing.contains(a.trim_end()))
        .cloned()
        .collect();
    if to_add.is_empty() {
        return Ok(Vec::new());
    }
    let prefix = if !prior_text.is_empty() && !prior_text.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let body = format!("{prefix}{}\n", to_add.join("\n"));
    let mut text = prior_text;
    text.push_str(&body);
    std::fs::write(&gitignore, text).map_err(io_err(&gitignore))?;
    Ok(to_add)
}

/// The knobs of one agent-driven init run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InitAgentOptions {
    /// The detected current branch, echoed as the commented `[submit] base`
    /// suggestion.
    pub submit_base: Option<String>,
    /// The agent model for the production invoker.
    pub model: Option<String>,
    /// Where to write the policy; defaults to `repo_root/flywheel.toml`.
    pub policy_path: Option<PathBuf>,
}

/// Drive the agent, render the proposed policy, validate, and write it.
///
/// The single entry point the CLI calls. `invoker` defaults to the
/// production agents-backed seam rooted at `repo_root`; tests inject a
/// scripted one to run offline.
///
/// The proposal is parsed, rendered, and -- critically -- re-validated
/// through [`load_policy_str`] BEFORE anything is written: an unparseable
/// response or a proposal that renders an invalid policy fails with
/// [`InitAgentError`] and leaves the working directory untouched. Only
/// after validation passes does it write `flywheel.toml` and (optionally)
/// append to `.gitignore`.
pub async fn run_agent_init(
    repo_root: &Path,
    invoker: Option<&dyn AutopilotInvoker>,
    options: &InitAgentOptions,
) -> Result<InitAgentResult, InitAgentError> {
    let default_invoker;
    let seam: &dyn AutopilotInvoker = match invoker {
        Some(invoker) => invoker,
        None => {
            default_invoker = build_repo_invoker(
                repo_root,
                RepoInvokerOptions {
                    model: options.model.clone(),
                    max_turns: DEFAULT_INIT_MAX_TURNS,
                    ..RepoInvokerOptions::default()
                },
            );
            &default_invoker
        }
    };
    let prompt = init_prompt(repo_root);
    let text = seam.invoke(&prompt).await?;
    let proposal = parse_init_proposal(&text)?;
    let rendered = render_agent_policy(&proposal, options.submit_base.as_deref());

    let target = options
        .policy_path
        .clone()
        .unwrap_or_else(|| repo_root.join(DEFAULT_POLICY_FILENAME));
    // Validate the rendered policy in memory before writing the real one,
    // so a broken render never lands.
    let policy = load_policy_str(&rendered, &target).map_err(InitAgentError::InvalidPolicy)?;

    std::fs::write(&target, rendered).map_err(io_err(&target))?;
    let gitignore_added = append_gitignore(&proposal.gitignore_additions, repo_root)?;
    Ok(InitAgentResult {
        proposal,
        policy_path: target,
        policy,
        gitignore_added,
    })
}
