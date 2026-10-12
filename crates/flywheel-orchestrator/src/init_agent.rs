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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::load_policy;
    use flywheel_core::task::Grader;
    use serde_json::json;
    use tempfile::TempDir;

    fn canned_proposal() -> Value {
        json!({
            "source_kind": "directory",
            "default_graders": [
                "cargo test --workspace",
                "cargo clippy --workspace -- -D warnings",
            ],
            "sandbox_setup": "cargo fetch",
            "autopilot": {"target_depth": 3, "interval_seconds": 600},
            "gitignore_additions": ["/target"],
            "notes": "Rust Cargo workspace; ~50 tests",
        })
    }

    /// An injectable invoker returning `payload` as a fenced block.
    fn scripted(payload: Value) -> impl AutopilotInvoker {
        move |_prompt: String| {
            let payload = payload.clone();
            async move { Ok(format!("Here is my proposal.\n\n```json\n{payload}\n```\n")) }
        }
    }

    fn canned() -> InitProposal {
        InitProposal {
            source_kind: "directory".to_owned(),
            default_graders: vec![
                "cargo test --workspace".to_owned(),
                "cargo clippy --workspace -- -D warnings".to_owned(),
            ],
            sandbox_setup: Some("cargo fetch".to_owned()),
            target_depth: Some(3),
            interval_seconds: Some(600.0),
            gitignore_additions: vec!["/target".to_owned()],
            notes: "Rust Cargo workspace; ~50 tests".to_owned(),
        }
    }

    const PY_INIT_PROMPT: &str = "You are the onboarding-assessment agent for `flywheel init`.\n\nInspect the repository at /repo: detect its toolchain (build system, test runner, linter), and propose the flywheel policy a maintainer would write by hand.\n\nDerive the default grader commands from checks that ALREADY exist in this repo (the test/lint/build commands a contributor runs today). Do not invent a command the repo cannot run. Propose a sandbox setup command that warms a fresh checkout (dependency install/fetch) when one applies, the autopilot intake cadence, and any build-output directories that belong in .gitignore.\n\nRespond with exactly one fenced JSON block:\n```json\n{\n  \"source_kind\": \"directory\",\n  \"default_graders\": [\"<existing test command>\", \"<existing lint command>\"],\n  \"sandbox_setup\": \"<dependency install/fetch command, or null>\",\n  \"autopilot\": {\"target_depth\": 3, \"interval_seconds\": 600},\n  \"gitignore_additions\": [\"<build-output dir>\"],\n  \"notes\": \"one sentence describing the detected toolchain\"\n}\n```";

    const PY_POLICY_NONE: &str = "# Flywheel work policy proposed by `flywheel init --agent`.\n# An agent inspected the repo and derived these settings; review and edit freely.\n# Committed with the repo; CLI flags always override.\n\n[source]\nkind = \"directory\"\ntasks_dir = \".flywheel/tasks\"\n\n[store]\nbackend = \"sqlite\"\n\n[paths]\ndb = \".flywheel/flywheel.sqlite\"\nsandbox_root = \".flywheel/worktrees\"\n\n# Default graders for work items that declare none (the verification\n# commands the agent derived from this repo's existing checks).\n[[defaults.graders]]\ntype = \"command\"\nrun = \"cargo test --workspace\"\n\n[[defaults.graders]]\ntype = \"command\"\nrun = \"cargo clippy --workspace -- -D warnings\"\n\n# Sandbox provisioning: runs (shell) inside every newly created sandbox\n# before the agent enters.\n[sandbox]\nsetup = \"cargo fetch\"\n\n# Autopilot intake daemon cadence (target queue depth and refill interval).\n[autopilot]\ntarget_depth = 3\ninterval_seconds = 600\n\n# Landing policy. Leave base UNSET to FF-merge work in-tree onto your\n# checked-out branch. Set base ONLY to a separate\n# integration branch you do NOT have checked out.\n[submit]\n# base = \"main\"\n";

    const PY_POLICY_MINIMAL: &str = "# Flywheel work policy proposed by `flywheel init --agent`.\n# An agent inspected the repo and derived these settings; review and edit freely.\n# Committed with the repo; CLI flags always override.\n\n[source]\nkind = \"directory\"\ntasks_dir = \".flywheel/tasks\"\n\n[store]\nbackend = \"sqlite\"\n\n[paths]\ndb = \".flywheel/flywheel.sqlite\"\nsandbox_root = \".flywheel/worktrees\"\n\n# Autopilot intake daemon cadence (target queue depth and refill interval).\n[autopilot]\ninterval_seconds = 0.5\n\n# Landing policy. Leave base UNSET to FF-merge work in-tree onto your\n# checked-out branch. Set base ONLY to a separate\n# integration branch you do NOT have checked out.\n[submit]\n# base = \"main\"\n";

    // --- prompt and render: byte-identical to the Python ------------------------------

    #[test]
    fn init_prompt_is_byte_identical_to_python() {
        assert_eq!(init_prompt(Path::new("/repo")), PY_INIT_PROMPT);
    }

    #[test]
    fn rendered_policy_is_byte_identical_to_python() {
        assert_eq!(render_agent_policy(&canned(), None), PY_POLICY_NONE);
        let with_base = render_agent_policy(&canned(), Some("main"));
        assert_eq!(
            with_base,
            PY_POLICY_NONE.replace(
                "# checked-out branch. Set base ONLY",
                "# checked-out branch (detected current branch: main). Set base ONLY"
            )
        );
        let minimal = InitProposal {
            interval_seconds: Some(0.5),
            ..InitProposal::default()
        };
        assert_eq!(render_agent_policy(&minimal, None), PY_POLICY_MINIMAL);
        // An empty detected branch reads as none.
        assert_eq!(render_agent_policy(&canned(), Some("")), PY_POLICY_NONE);
    }

    #[test]
    fn rendered_strings_are_toml_safe() {
        let proposal = InitProposal {
            default_graders: vec!["echo \"quoted\" \\ back".to_owned()],
            sandbox_setup: Some("sh -c 'a \"b\"'".to_owned()),
            ..InitProposal::default()
        };
        let rendered = render_agent_policy(&proposal, Some("feat/\"x\""));
        let policy = load_policy_str(&rendered, Path::new("flywheel.toml")).unwrap();
        assert_eq!(policy.sandbox_setup.as_deref(), Some("sh -c 'a \"b\"'"));
        match &policy.default_graders[0] {
            Grader::Command(command) => assert_eq!(command.run, "echo \"quoted\" \\ back"),
            other => panic!("expected a command grader, got {other:?}"),
        }
        assert!(rendered.contains("# base = \"feat/\\\"x\\\"\"\n"));
    }

    // --- proposal -> loadable policy ---------------------------------------------------

    #[tokio::test]
    async fn agent_init_writes_loadable_policy_reflecting_proposal() {
        let tmp = TempDir::new().unwrap();
        let invoker = scripted(canned_proposal());
        let result = run_agent_init(tmp.path(), Some(&invoker), &InitAgentOptions::default())
            .await
            .unwrap();

        let policy_path = tmp.path().join("flywheel.toml");
        assert!(policy_path.is_file());
        assert_eq!(result.policy_path, policy_path);
        assert_eq!(result.proposal, canned());

        let policy = load_policy(&policy_path).unwrap();
        assert_eq!(policy.source_kind, "directory");
        assert_eq!(policy.autopilot_target_depth, 3);
        assert_eq!(policy.autopilot_interval_seconds, 600.0);
        assert_eq!(policy.sandbox_setup.as_deref(), Some("cargo fetch"));
        let runs: Vec<&str> = policy
            .default_graders
            .iter()
            .map(|g| match g {
                Grader::Command(c) => c.run.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            runs,
            vec![
                "cargo test --workspace",
                "cargo clippy --workspace -- -D warnings"
            ]
        );
        // The validated policy is also returned on the result.
        assert_eq!(result.policy, policy);
    }

    #[tokio::test]
    async fn agent_init_honors_an_explicit_policy_path_and_submit_base() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("conf").join("custom.toml");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let invoker = scripted(canned_proposal());
        let result = run_agent_init(
            tmp.path(),
            Some(&invoker),
            &InitAgentOptions {
                submit_base: Some("develop".to_owned()),
                policy_path: Some(target.clone()),
                ..InitAgentOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.policy_path, target);
        assert!(!tmp.path().join("flywheel.toml").exists());
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("(detected current branch: develop)"));
        assert!(text.contains("# base = \"develop\"\n"));
        assert_eq!(result.policy.submit_base, None);
    }

    #[tokio::test]
    async fn agent_init_appends_gitignore_without_duplicating() {
        let tmp = TempDir::new().unwrap();
        let invoker = scripted(canned_proposal());
        let first = run_agent_init(tmp.path(), Some(&invoker), &InitAgentOptions::default())
            .await
            .unwrap();
        assert_eq!(first.gitignore_added, vec!["/target"]);
        let gitignore = tmp.path().join(".gitignore");
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "/target\n");

        let second = run_agent_init(tmp.path(), Some(&invoker), &InitAgentOptions::default())
            .await
            .unwrap();
        assert!(second.gitignore_added.is_empty());
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "/target\n");
    }

    #[tokio::test]
    async fn agent_init_preserves_existing_gitignore_lines() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "/build").unwrap();
        let invoker = scripted(canned_proposal());
        run_agent_init(tmp.path(), Some(&invoker), &InitAgentOptions::default())
            .await
            .unwrap();
        // A missing trailing newline is repaired before appending.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
            "/build\n/target\n"
        );
    }

    // --- failure modes write nothing -------------------------------------------------------

    #[tokio::test]
    async fn agent_init_unparseable_response_fails_and_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let no_json = |_prompt: String| async move {
            Ok("I could not figure out the toolchain. Sorry, no JSON here.".to_owned())
        };
        let err = run_agent_init(tmp.path(), Some(&no_json), &InitAgentOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, InitAgentError::Parse(_)), "{err:?}");
        assert!(
            err.to_string()
                .starts_with("could not parse an init proposal from the agent response")
        );
        assert!(!tmp.path().join("flywheel.toml").exists());
        assert!(!tmp.path().join(".gitignore").exists());
    }

    #[tokio::test]
    async fn agent_init_agent_failure_fails_and_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let failing = |_prompt: String| async move {
            Err::<String, _>(InvokeError::Other("agent exploded".to_owned()))
        };
        let err = run_agent_init(tmp.path(), Some(&failing), &InitAgentOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err, InitAgentError::Agent(InvokeError::Other(_))),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "init agent error: agent exploded");
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn invalid_render_gate_maps_policy_errors_and_writes_nothing() {
        // Every proposal coercion keeps the render loadable (blank/invalid
        // values fall back to their defaults), so the gate is exercised on
        // the loader's own rejection and its error mapping.
        let tmp = TempDir::new().unwrap();
        let mut payload = canned_proposal();
        payload["sandbox_setup"] = json!("   ");
        payload["autopilot"] = json!({"target_depth": 0, "interval_seconds": -1});
        let proposal = parse_init_proposal(&format!("```json\n{payload}\n```")).unwrap();
        assert_eq!(proposal.sandbox_setup, None);
        assert_eq!(proposal.target_depth, None);
        assert_eq!(proposal.interval_seconds, None);
        let invalid = "[source]\nkind = \"directory\"\n[store]\nbackend = \"bogus\"\n";
        let err = load_policy_str(invalid, &tmp.path().join("flywheel.toml")).unwrap_err();
        let gate = InitAgentError::InvalidPolicy(err);
        assert!(
            gate.to_string()
                .starts_with("the agent-proposed policy failed validation and was not written: ")
        );
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    // --- proposal parsing defensiveness ----------------------------------------------------

    #[test]
    fn parse_proposal_defaults_on_missing_fields() {
        let proposal = parse_init_proposal("```json\n{}\n```").unwrap();
        assert_eq!(proposal, InitProposal::default());
        assert_eq!(proposal.source_kind, "directory");
        assert!(proposal.default_graders.is_empty());
        assert_eq!(proposal.sandbox_setup, None);
        assert_eq!(proposal.target_depth, None);
    }

    #[test]
    fn parse_proposal_coerces_off_contract_values() {
        let payload = json!({
            "source_kind": "jira",
            "default_graders": ["  make test ", "", 7, "make lint"],
            "sandbox_setup": null,
            "autopilot": {"target_depth": 4.0, "interval_seconds": 90},
            "gitignore_additions": "not-a-list",
            "notes": 42,
        });
        let proposal = parse_init_proposal(&format!("prose\n{payload}")).unwrap();
        assert_eq!(proposal.source_kind, "directory");
        assert_eq!(proposal.default_graders, vec!["make test", "make lint"]);
        assert_eq!(proposal.sandbox_setup, None);
        assert_eq!(proposal.target_depth, Some(4));
        assert_eq!(proposal.interval_seconds, Some(90.0));
        assert!(proposal.gitignore_additions.is_empty());
        assert_eq!(proposal.notes, "");

        let github = parse_init_proposal("```json\n{\"source_kind\": \"github\"}\n```").unwrap();
        assert_eq!(github.source_kind, "github");
        let fractional = parse_init_proposal(
            "```json\n{\"autopilot\": {\"target_depth\": 2.5, \"interval_seconds\": true}}\n```",
        )
        .unwrap();
        assert_eq!(fractional.target_depth, None);
        assert_eq!(fractional.interval_seconds, None);
    }

    #[test]
    fn default_invoker_is_built_with_the_init_turn_budget() {
        let invoker = build_repo_invoker(
            "/repo",
            RepoInvokerOptions {
                max_turns: DEFAULT_INIT_MAX_TURNS,
                ..RepoInvokerOptions::default()
            },
        );
        assert_eq!(invoker.configuration().max_turns, Some(40));
    }
}
