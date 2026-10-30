//! The `init` verb: scaffold `.flywheel/` and a `flywheel.toml` work policy
//! in the working directory, through an injectable host (prompt reader,
//! environment, origin lookup, postgres preflight, agent runner) so the
//! interactive flow is testable without a terminal.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use flywheel_core::telemetry_file::{JsonStyle, python_json_dumps};
use regex::Regex;
use serde_json::Value;

use crate::init_agent::{InitAgentOptions, InitAgentResult, run_agent_init};
use crate::pg_preflight::{
    PreflightOptions, PreflightOutcome, format_report, run_postgres_preflight,
};
use crate::policy::{DEFAULT_POLICY_FILENAME, load_policy};
use crate::skills::{DEFAULT_SKILLS_ROOT, SKILL_NAMES, install_skills, settings_from_policy};
use crate::store_factory::{PG_DSN_ENV, PG_DSN_FALLBACK_ENV, resolve_postgres_dsn};
use crate::store_registry::{Environ, environ_from_process};

use super::args::InitArgs;
use super::{WorkflowError, py_repr};

/// The runtime-state root `init` scaffolds.
pub const INIT_ROOT: &str = ".flywheel";

const INIT_GITIGNORE: &str = "# Flywheel runtime state: never committed.
flywheel.sqlite
flywheel.sqlite-shm
flywheel.sqlite-wal
sandboxes/
worktrees/
# Run telemetry JSONL (logs/runs/<run_id>.jsonl) and supervisor logs:
# verbatim, sensitive-by-default payloads. Never committed.
logs/
.merge.lock
# Blind held-out oracles authored by /fw-verify: throwaway scratch run at
# verify time, never landed (the durable artifact is the recorded proof).
verification/
";

const DOCKERIGNORE_ENTRY: &str = ".flywheel/";

/// The copy-pasteable pointer `init` prints (never writes) for an
/// operator's CLAUDE.md (spec 00072 D-3).
pub const INIT_CLAUDEMD_POINTER: &str = "This repository runs on flywheel: load the `flywheel-ops` skill to \
operate the loop, and run `fw docs <topic>` for authoritative flywheel documentation.";

/// The hardened `protected_paths` floor `init` scaffolds as LIVE config
/// (spec 00081, criterion 1): the CI directory and the policy file itself.
pub const INIT_PROTECTED_PATHS: [&str; 2] = [".github/**", "flywheel.toml"];

const INIT_STORE_BACKENDS: [&str; 2] = ["sqlite", "postgres"];
const INIT_SOURCE_KINDS: [&str; 2] = ["directory", "github"];
const INIT_DONE_ACTIONS: [&str; 2] = ["comment", "close"];

static GITHUB_REPO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?/[A-Za-z0-9._-]+$").unwrap()
});
static GITHUB_ORIGIN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"github\.com[:/](?P<owner>[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)/(?P<name>[A-Za-z0-9._-]+?)(?:\.git)?/?$",
    )
    .unwrap()
});
static PG_SCHEMA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());
static TOML_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\[\[?\s*(?P<name>[^\]]*?)\s*\]\]?\s*(?:#.*)?$").unwrap());

// --- Host seam ---------------------------------------------------------------------

/// Why a prompt could not be answered.
#[derive(Debug)]
pub enum PromptError {
    /// stdin closed before the prompts finished.
    Eof,
    /// The operator interrupted the prompts (Ctrl-C).
    Interrupted,
    Io(std::io::Error),
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::Eof => f.write_str("stdin closed during init prompts"),
            PromptError::Interrupted => f.write_str("interrupted"),
            PromptError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PromptError {}

/// The environment `init` runs against. The production host reads the real
/// terminal, process environment, git remote, and postgres; tests script
/// each.
pub trait InitHost {
    /// The working directory everything is scaffolded under.
    fn root(&self) -> PathBuf;
    /// Whether stdin is a terminal (prompts only on a TTY).
    fn stdin_is_tty(&self) -> bool;
    /// Read one answer line (without its line ending); `Eof` when stdin is
    /// exhausted so a closed pipe aborts init instead of accepting defaults.
    fn read_line(&mut self) -> Result<String, PromptError>;
    fn environ(&self) -> Environ;
    /// `owner/name` parsed from the `origin` remote, or `None`.
    fn github_repo_from_origin(&self) -> Option<String>;
    fn postgres_preflight(
        &self,
        dsn: &str,
        schema: &str,
        options: PreflightOptions,
    ) -> PreflightOutcome;
    /// Drive the agent-driven init (`--agent`); the error is the message to
    /// print after `error: `.
    fn run_agent_init(
        &self,
        repo_root: &Path,
        options: &InitAgentOptions,
    ) -> Result<InitAgentResult, String>;
}

/// The real host: the process cwd, stdin, environment, `git`, and postgres.
#[derive(Debug, Default)]
pub struct ProductionInitHost;

impl ProductionInitHost {
    pub fn new() -> Self {
        Self
    }
}

impl InitHost for ProductionInitHost {
    fn root(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_default()
    }

    fn stdin_is_tty(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn read_line(&mut self) -> Result<String, PromptError> {
        let mut line = String::new();
        let read = std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|err| match err.kind() {
                std::io::ErrorKind::Interrupted => PromptError::Interrupted,
                _ => PromptError::Io(err),
            })?;
        if read == 0 {
            return Err(PromptError::Eof);
        }
        Ok(line.trim_end_matches(['\n', '\r']).to_owned())
    }

    fn environ(&self) -> Environ {
        environ_from_process()
    }

    fn github_repo_from_origin(&self) -> Option<String> {
        let output = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(self.root())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        github_repo_from_origin_url(String::from_utf8_lossy(&output.stdout).trim())
    }

    fn postgres_preflight(
        &self,
        dsn: &str,
        schema: &str,
        options: PreflightOptions,
    ) -> PreflightOutcome {
        run_postgres_preflight(dsn, schema, options)
    }

    fn run_agent_init(
        &self,
        repo_root: &Path,
        options: &InitAgentOptions,
    ) -> Result<InitAgentResult, String> {
        // A dedicated thread owns the runtime so this works whether or not
        // the caller is already inside one.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|err| format!("cannot start the agent runtime: {err}"))?;
                    runtime
                        .block_on(run_agent_init(repo_root, None, options))
                        .map_err(|err| err.to_string())
                })
                .join()
                .unwrap_or_else(|_| Err("the agent init thread panicked".to_owned()))
        })
    }
}

/// Parse `owner/name` from a GitHub remote URL (ssh or https), or `None`.
pub fn github_repo_from_origin_url(url: &str) -> Option<String> {
    let caps = GITHUB_ORIGIN_RE.captures(url)?;
    Some(format!("{}/{}", &caps["owner"], &caps["name"]))
}

// --- Answers and rendering ---------------------------------------------------------

/// The resolved init choices, however they were answered (prompt, flag, or
/// default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitAnswers {
    pub store_backend: String,
    pub store_schema: Option<String>,
    pub source_kind: String,
    pub github_repo: Option<String>,
    pub github_label: Option<String>,
    pub github_done_action: String,
    /// The repo's current branch, surfaced as a COMMENTED `[submit] base`
    /// suggestion (never an active key).
    pub submit_base: Option<String>,
    pub install_skills: bool,
}

impl Default for InitAnswers {
    fn default() -> Self {
        Self {
            store_backend: "sqlite".to_owned(),
            store_schema: None,
            source_kind: "directory".to_owned(),
            github_repo: None,
            github_label: None,
            github_done_action: "comment".to_owned(),
            submit_base: None,
            install_skills: false,
        }
    }
}

/// `json.dumps(text)`: valid quoting for a TOML basic string.
fn toml_quote(text: &str) -> String {
    python_json_dumps(&Value::String(text.to_owned()), &JsonStyle::PYTHON_DEFAULT)
}

const INIT_POLICY_HEADER: &str =
    "# Flywheel work policy: where work comes from, where runtime state lives, and
# how finished work lands. Committed with the repo; CLI flags always override.
#
# This file is self-documenting. The ACTIVE keys below are what `flywheel init`
# chose for this repo; every other knob is shown COMMENTED with its default so
# you can see the full feature surface and turn things on by uncommenting.
# Only [source] and [store] are required. Full reference: docs/configuration.md.
";

const INIT_POLICY_TAIL_HEAD: &str = "[paths]
db = \".flywheel/flywheel.sqlite\"
# Worktree/sandbox root. A relative path anchors at the repo root; \"@cache\"
# (XDG cache dir keyed by repo identity) and \"@sibling\"
# (<repo-parent>/<repo>.worktrees) opt into out-of-tree layouts.
sandbox_root = \".flywheel/worktrees\"

# --- Default graders -------------------------------------------------------
# Verification commands for work items that declare none. Tracker sources
# (github) lean on these; directory task files always carry their own. One
# array-of-tables entry per grader, run in order; pass is exit code 0.
# [[defaults.graders]]
# type = \"command\"
# run = \"uv run pytest\"

# --- Agent -----------------------------------------------------------------
# The model id the worker passes to the agent SDK verbatim (no allowlist).
# CLI --model overrides. Unset uses the SDK default.
# [agent]
# model = \"claude-opus-4-8\"

# --- Sandbox ---------------------------------------------------------------
# Where the agent runs and how the sandbox is provisioned. `setup` runs (shell)
# inside every newly created sandbox before the agent enters (dependency
# install, codegen) so tasks never pay discovery cost for a bare worktree.
# `backend` selects the execution surface: \"worktree\" (default) or \"container\"
# (Docker; needs the flywheel-container extra + a [sandbox.container] block).
# `preset` is a code-owned baseline that subtable keys override.
# [sandbox]
# setup = \"uv sync\"
# backend = \"worktree\"               # worktree | container
# preset = \"fast\"                    # fast | balanced | hardened
# permission_mode = \"default\"        # default | acceptEdits | bypassPermissions
#
# Optional [sandbox.*] subtables (full reference: docs/sandbox.md):
#   [sandbox.env]          explicit env handed to the agent AND graders
#   [sandbox.capabilities] the agent's tool/skill/MCP surface inside the sandbox
#   [sandbox.network]      network policy (e.g. policy = \"offline\")
#   [sandbox.exec]         exec-time knobs
#   [sandbox.limits]       cpu / memory / pids / wall-time ceilings
#   [sandbox.retention]    on_done / on_failure = \"destroy\" | \"park\"
#   [sandbox.container]    image / model / auth for the container backend
";

const INIT_POLICY_TAIL_FOOT: &str =
    "# --- Phase-exit gate -------------------------------------------------------
# `verify` runs (shell) against the merged phase base once every task in a
# phase has landed; a non-zero exit leaves the phase active instead of
# archiving it. Unset = today's archival (no gate). Distinct from
# [submit] verify, which gates each individual land.
# [phase]
# verify = \"uv run pytest\"

# --- Held-out landing gate -------------------------------------------------
# `root` is a directory of operator-authored held-out grader registrations the
# execute-time gate runs against the committed sandbox before landing \u{2014} the
# agent never sees them, so it cannot game them. Unset = no gate.
# See docs/held-out-gate.md.
# [held_out]
# root = \".flywheel/verification\"

# --- Autopilot intake ------------------------------------------------------
# The intake daemon (`flywheel autopilot`) keeps the queue filled. `target_depth`
# is the queue target, `interval_seconds` the refill cadence, `landing` how
# authored work lands. The optional [autopilot.weights] sub-table tunes the
# scoring axes (unset weights keep the engine defaults). See docs/autopilot.md.
# [autopilot]
# target_depth = 5
# interval_seconds = 300
# landing = \"merge\"
# [autopilot.weights]
# urgency = 1.0
# importance = 1.0

# --- Worker ----------------------------------------------------------------
# `concurrency` is how many tasks one worker process drives in parallel
# (default 1). >1 relies on each task's conflict_keys to keep concurrent work
# off shared files; --concurrency overrides per run.
# [worker]
# concurrency = 1

# --- Execution -------------------------------------------------------------
# `mode` is \"local\" (default) or \"distributed\" (the latter requires
# store.backend = \"postgres\"). `capabilities` is THIS worker's advertised set;
# the scheduler offers it only tasks whose required_capabilities is a subset.
# (Distinct from [sandbox.capabilities], the agent's in-sandbox tool surface.)
# [execution]
# mode = \"local\"
# capabilities = [\"gpu\", \"cuda\"]
";

/// Render the `[submit]` section: `protected_paths` ACTIVE with the
/// hardened floor, the detected branch as a COMMENTED `base` suggestion.
pub fn render_submit_block(submit_base: Option<&str>) -> String {
    let suggestion = match submit_base {
        Some(base) if !base.is_empty() => toml_quote(base),
        _ => "\"main\"".to_owned(),
    };
    let detected_note = match submit_base {
        Some(base) if !base.is_empty() => format!(" (detected current branch: {base})"),
        _ => String::new(),
    };
    let protected = INIT_PROTECTED_PATHS
        .iter()
        .map(|p| toml_quote(p))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# --- Landing --------------------------------------------------------------\n\
         # How finished work reaches your branch. `strategy` is \"merge\" (FF-merge\n\
         # in-tree, default), \"phase\" (land on a per-phase integration branch,\n\
         # reviewed as one PR), or \"pr\" (push + open a pull request; uses\n\
         # `remote` and optional `pr_base`).\n\
         #\n\
         # `base` pins the branch finished work lands on and the worker resolves\n\
         # its phase base from. Leave it UNSET to FF-merge onto your checked-out\n\
         # branch{detected_note} \u{2014} the working default. Set base ONLY to a\n\
         # separate integration branch you do NOT have checked out; setting it to\n\
         # the checked-out branch is refused and the worker exits on startup.\n\
         #\n\
         # `verify` is the standing build gate (spec 00064): a repo-wide command\n\
         # re-run under the merge lock against the exact tree about to land, on\n\
         # every land path, independent of each task's own graders. A non-zero\n\
         # exit refuses the land and parks the work \u{2014} this is what stops a\n\
         # semantic merge skew (two independently-valid changes whose union does\n\
         # not build) from reaching your branch. Unset = no gate. It serializes\n\
         # landings, so a slow command bottlenecks throughput.\n\
         #\n\
         # `protected_paths` are globs a finished branch may not touch; a match\n\
         # refuses the land. The scaffolded floor (your CI directory and this\n\
         # policy file) is ACTIVE on purpose \u{2014} it stops authored work from\n\
         # rewriting the verification surface that judges it. Extend it with\n\
         # grader configs and .flywheel state as your surface grows.\n\
         [submit]\n\
         # base = {suggestion}\n\
         # verify = \"uv run pytest\"\n\
         protected_paths = [{protected}]\n\
         # strategy = \"merge\"          # merge | phase | pr\n\
         # remote = \"origin\"           # pr strategy push target\n\
         # pr_base = \"main\"            # pr strategy base branch\n"
    )
}

/// Render the `[source]` section the prompts own; `tasks_dir` is the one
/// directory key they do not answer (the reconfigure path threads the
/// existing value through).
pub fn render_source_block(answers: &InitAnswers, tasks_dir: &str) -> String {
    if answers.source_kind == "directory" {
        return format!(
            "[source]\nkind = \"directory\"\ntasks_dir = {}\n",
            toml_quote(tasks_dir)
        );
    }
    format!(
        "[source]\nkind = \"github\"\nrepo = \"{}\"\nlabel = \"{}\"\ndone_action = \"{}\"\n",
        answers.github_repo.as_deref().unwrap_or("None"),
        answers.github_label.as_deref().unwrap_or("None"),
        answers.github_done_action
    )
}

/// Render the `[store]` section the prompts own.
pub fn render_store_block(answers: &InitAnswers) -> String {
    let mut block = format!("[store]\nbackend = \"{}\"\n", answers.store_backend);
    if let Some(schema) = &answers.store_schema {
        block.push_str(&format!("schema = \"{schema}\"\n"));
    }
    block
}

/// Render `flywheel.toml` from the answers. No credential is ever an input
/// here: the postgres DSN lives only in the environment.
pub fn render_init_policy(answers: &InitAnswers) -> String {
    [
        INIT_POLICY_HEADER.to_owned(),
        "\n".to_owned(),
        render_source_block(answers, ".flywheel/tasks"),
        "\n".to_owned(),
        render_store_block(answers),
        "\n".to_owned(),
        INIT_POLICY_TAIL_HEAD.to_owned(),
        "\n".to_owned(),
        render_submit_block(answers.submit_base.as_deref()),
        "\n".to_owned(),
        INIT_POLICY_TAIL_FOOT.to_owned(),
    ]
    .concat()
}

// --- Prompts ------------------------------------------------------------------------

/// What the interactive flow can fail with.
#[derive(Debug)]
enum InitFlowError {
    /// Inconsistent or invalid flag values (exit 2, no policy written).
    Usage(String),
    Prompt(PromptError),
    Io(std::io::Error),
}

impl From<PromptError> for InitFlowError {
    fn from(err: PromptError) -> Self {
        InitFlowError::Prompt(err)
    }
}

impl From<std::io::Error> for InitFlowError {
    fn from(err: std::io::Error) -> Self {
        InitFlowError::Io(err)
    }
}

fn prompt_line(
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    prompt: &str,
) -> Result<String, InitFlowError> {
    out.write_all(prompt.as_bytes())?;
    out.flush()?;
    Ok(host.read_line()?.trim().to_owned())
}

fn prompt_choice(
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    label: &str,
    choices: &[&str],
    default: &str,
) -> Result<String, InitFlowError> {
    let rendered = choices
        .iter()
        .map(|c| {
            if *c == default {
                format!("[{c}]")
            } else {
                (*c).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    loop {
        let raw = prompt_line(host, out, &format!("{label} ({rendered}): "))?;
        if raw.is_empty() {
            return Ok(default.to_owned());
        }
        if choices.contains(&raw.as_str()) {
            return Ok(raw);
        }
        writeln!(
            out,
            "  invalid value {}; choose one of: {}",
            py_repr(&raw),
            choices.join(", ")
        )?;
    }
}

fn prompt_yes_no(
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    label: &str,
    default: bool,
) -> Result<bool, InitFlowError> {
    let rendered = if default { "[y]/n" } else { "y/[n]" };
    loop {
        let raw = prompt_line(host, out, &format!("{label} ({rendered}): "))?.to_lowercase();
        if raw.is_empty() {
            return Ok(default);
        }
        match raw.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(out, "  invalid value {}; answer y or n", py_repr(&raw))?,
        }
    }
}

fn validate_pg_schema(value: &str) -> Option<String> {
    (!PG_SCHEMA_RE.is_match(value)).then(|| {
        format!(
            "invalid schema name {}: must match [A-Za-z_][A-Za-z0-9_]*",
            py_repr(value)
        )
    })
}

fn validate_github_repo(value: &str) -> Option<String> {
    (!GITHUB_REPO_RE.is_match(value))
        .then(|| format!("invalid repo {}: expected owner/name", py_repr(value)))
}

fn validate_github_label(value: &str) -> Option<String> {
    (value.is_empty() || value.contains(['"', '\\', '\n'])).then(|| {
        format!(
            "invalid label {}: must be non-empty and contain no quotes or backslashes",
            py_repr(value)
        )
    })
}

fn prompt_pg_schema(
    host: &mut dyn InitHost,
    out: &mut dyn Write,
) -> Result<Option<String>, InitFlowError> {
    loop {
        let raw = prompt_line(host, out, "postgres schema [none]: ")?;
        if raw.is_empty() {
            return Ok(None);
        }
        match validate_pg_schema(&raw) {
            None => return Ok(Some(raw)),
            Some(error) => writeln!(out, "  {error}")?,
        }
    }
}

fn prompt_github_repo(
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    default: Option<&str>,
) -> Result<String, InitFlowError> {
    let suffix = default.map(|d| format!(" [{d}]")).unwrap_or_default();
    loop {
        let raw = prompt_line(host, out, &format!("github repo (owner/name){suffix}: "))?;
        let value = if raw.is_empty() {
            default.unwrap_or("").to_owned()
        } else {
            raw
        };
        if value.is_empty() {
            writeln!(out, "  a repo is required (owner/name)")?;
            continue;
        }
        match validate_github_repo(&value) {
            None => return Ok(value),
            Some(error) => writeln!(out, "  {error}")?,
        }
    }
}

fn prompt_github_label(
    host: &mut dyn InitHost,
    out: &mut dyn Write,
) -> Result<String, InitFlowError> {
    loop {
        let raw = prompt_line(host, out, "issue label [flywheel]: ")?;
        let value = if raw.is_empty() {
            "flywheel".to_owned()
        } else {
            raw
        };
        match validate_github_label(&value) {
            None => return Ok(value),
            Some(error) => writeln!(out, "  {error}")?,
        }
    }
}

// --- Checks and side effects --------------------------------------------------------

/// Keep `.flywheel/` out of docker build contexts: when a Dockerfile or
/// Containerfile sits at `root`, append the entry to `.dockerignore` once.
/// Returns the line appended, or `None` when nothing was written.
pub fn ensure_dockerignore_covers_flywheel(root: &Path) -> std::io::Result<Option<String>> {
    let has_dockerfile = std::fs::read_dir(root)?.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        name.starts_with("Dockerfile") || name.starts_with("Containerfile")
    });
    if !has_dockerfile {
        return Ok(None);
    }
    let dockerignore = root.join(".dockerignore");
    let mut prior = String::new();
    if dockerignore.is_file() {
        prior = std::fs::read_to_string(&dockerignore)?;
        let covered = prior.lines().any(|line| {
            matches!(
                line.trim(),
                ".flywheel" | ".flywheel/" | "/.flywheel" | "/.flywheel/"
            )
        });
        if covered {
            return Ok(None);
        }
    }
    let prefix = if prior.is_empty() || prior.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut handle = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&dockerignore)?;
    write!(
        handle,
        "{prefix}# Flywheel runtime state (worktrees, logs, db): keep out of image build contexts.\n{DOCKERIGNORE_ENTRY}\n"
    )?;
    Ok(Some(DOCKERIGNORE_ENTRY.to_owned()))
}

/// Print whether agent credentials are in place: `ANTHROPIC_API_KEY`, else a
/// `claude login` credential file, else a non-blocking warning.
pub fn report_agent_auth(environ: &Environ, out: &mut dyn Write) -> std::io::Result<()> {
    if environ
        .get("ANTHROPIC_API_KEY")
        .is_some_and(|v| !v.is_empty())
    {
        return writeln!(out, "agent auth: ANTHROPIC_API_KEY is set");
    }
    let creds = environ
        .get("HOME")
        .filter(|h| !h.is_empty())
        .map(|home| Path::new(home).join(".claude").join(".credentials.json"));
    if creds.is_some_and(|p| p.is_file()) {
        return writeln!(out, "agent auth: detected a `claude login` credential file");
    }
    writeln!(
        out,
        "warning: no agent credentials detected; set ANTHROPIC_API_KEY (or run `claude login`) \
         before the first worker run"
    )
}

/// Guided, failsafe Postgres bring-up at init time. With no DSN in the
/// environment it stays advisory; with one, the preflight checks run and a
/// blocking result (without `--allow-unverified`) is a usage error that
/// writes no policy.
fn postgres_preflight(
    args: &InitArgs,
    host: &dyn InitHost,
    schema: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), InitFlowError> {
    let Some(dsn) = resolve_postgres_dsn(&host.environ()) else {
        writeln!(
            out,
            "postgres: no DSN found; set {PG_DSN_ENV} (or {PG_DSN_FALLBACK_ENV}) before running \
             flywheel worker"
        )?;
        return Ok(());
    };
    let outcome = host.postgres_preflight(
        &dsn,
        schema.unwrap_or("public"),
        PreflightOptions {
            provision: args.provision,
            allow_unverified: args.allow_unverified,
        },
    );
    writeln!(out, "{}", format_report(&outcome.checks))?;
    if outcome.blocked() && !args.allow_unverified {
        return Err(InitFlowError::Usage(
            "postgres preflight found blocking issues (see the report above); fix them, or re-run \
             with --allow-unverified to scaffold the policy anyway"
                .to_owned(),
        ));
    }
    Ok(())
}

fn resolve_install_skills(
    args: &InitArgs,
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    interactive: bool,
) -> Result<bool, InitFlowError> {
    if let Some(choice) = args.skills {
        return Ok(choice);
    }
    if interactive {
        return prompt_yes_no(
            host,
            out,
            &format!("install Claude Code skills ({})?", SKILL_NAMES.join(", ")),
            true,
        );
    }
    Ok(false)
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Render and install the managed skills for the policy on disk (re-loaded
/// from `policy_path` so the render binds to exactly what the file says;
/// a missing file renders the built-in defaults).
fn install_skills_from_policy_file(
    root: &Path,
    policy_path: &Path,
    out: &mut dyn Write,
) -> Result<(), WorkflowError> {
    let policy = if policy_path.is_file() {
        Some(load_policy(policy_path)?)
    } else {
        None
    };
    let report = install_skills(
        root.join(DEFAULT_SKILLS_ROOT),
        &settings_from_policy(policy.as_ref()),
        &SKILL_NAMES,
    )
    .map_err(|err| WorkflowError::Message(err.to_string()))?;
    for path in &report.created {
        writeln!(out, "created: {}", display_relative(root, path))?;
    }
    for path in &report.updated {
        writeln!(out, "updated: {}", display_relative(root, path))?;
    }
    for path in &report.skipped {
        writeln!(
            out,
            "exists:  {} (user-owned, left untouched)",
            display_relative(root, path)
        )?;
    }
    Ok(())
}

fn collect_init_answers(
    args: &InitArgs,
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    interactive: bool,
) -> Result<InitAnswers, InitFlowError> {
    let submit_base = current_branch(&host.root());
    let backend = match &args.store {
        Some(backend) => backend.clone(),
        None if interactive => {
            prompt_choice(host, out, "store backend", &INIT_STORE_BACKENDS, "sqlite")?
        }
        None => "sqlite".to_owned(),
    };
    let mut schema: Option<String> = None;
    if backend == "postgres" {
        if let Some(flag_schema) = &args.pg_schema {
            let flag_schema = flag_schema.trim().to_owned();
            if let Some(error) = validate_pg_schema(&flag_schema) {
                return Err(InitFlowError::Usage(error));
            }
            schema = Some(flag_schema);
        } else if interactive {
            schema = prompt_pg_schema(host, out)?;
        }
        postgres_preflight(args, host, schema.as_deref(), out)?;
    } else if args.pg_schema.is_some() {
        return Err(InitFlowError::Usage(
            "--pg-schema applies only to the postgres store backend; the selected backend is \
             'sqlite'"
                .to_owned(),
        ));
    }

    let kind = match &args.source {
        Some(kind) => kind.clone(),
        None if interactive => {
            prompt_choice(host, out, "work source", &INIT_SOURCE_KINDS, "directory")?
        }
        None => "directory".to_owned(),
    };
    if kind == "directory" {
        if args.repo.is_some() || args.label.is_some() {
            return Err(InitFlowError::Usage(
                "--repo/--label apply only to the github work source; the selected source is \
                 'directory'"
                    .to_owned(),
            ));
        }
        return Ok(InitAnswers {
            store_backend: backend,
            store_schema: schema,
            source_kind: "directory".to_owned(),
            submit_base,
            install_skills: resolve_install_skills(args, host, out, interactive)?,
            ..InitAnswers::default()
        });
    }

    let origin_default = host.github_repo_from_origin();
    let repo = match &args.repo {
        Some(repo) => {
            let repo = repo.trim().to_owned();
            if let Some(error) = validate_github_repo(&repo) {
                return Err(InitFlowError::Usage(error));
            }
            repo
        }
        None if interactive => prompt_github_repo(host, out, origin_default.as_deref())?,
        None => match origin_default {
            Some(repo) => repo,
            None => {
                return Err(InitFlowError::Usage(
                    "the github work source needs --repo OWNER/NAME (the origin remote is absent \
                     or not GitHub-shaped)"
                        .to_owned(),
                ));
            }
        },
    };
    let label = match &args.label {
        Some(label) => {
            let label = label.trim().to_owned();
            if let Some(error) = validate_github_label(&label) {
                return Err(InitFlowError::Usage(error));
            }
            label
        }
        None if interactive => prompt_github_label(host, out)?,
        None => "flywheel".to_owned(),
    };
    let done_action = if interactive {
        prompt_choice(host, out, "done action", &INIT_DONE_ACTIONS, "comment")?
    } else {
        "comment".to_owned()
    };
    Ok(InitAnswers {
        store_backend: backend,
        store_schema: schema,
        source_kind: "github".to_owned(),
        github_repo: Some(repo),
        github_label: Some(label),
        github_done_action: done_action,
        submit_base,
        install_skills: resolve_install_skills(args, host, out, interactive)?,
    })
}

/// The next-steps epilogue every successful init prints.
pub fn print_init_next_steps(
    out: &mut dyn Write,
    store_backend: Option<&str>,
    skills_installed: bool,
) -> std::io::Result<()> {
    writeln!(out, "Next steps:")?;
    writeln!(
        out,
        "  1. Drop one JSON file per task into {INIT_ROOT}/tasks/active/<phase>/ ('goal' and \
         'graders' are the only required fields)."
    )?;
    if skills_installed {
        writeln!(
            out,
            "     Or, in Claude Code: /fw-spec to spec a feature, /fw-plan to queue tasks."
        )?;
    }
    writeln!(
        out,
        "  2. Authenticate the agent: set ANTHROPIC_API_KEY (or run `claude login`) before the \
         first worker run."
    )?;
    writeln!(out, "  3. Run: flywheel worker")?;
    writeln!(out, "  4. Watch: flywheel status / live")?;
    if store_backend == Some("postgres") {
        writeln!(out)?;
        writeln!(
            out,
            "The postgres DSN is read from {PG_DSN_ENV} (fallback: {PG_DSN_FALLBACK_ENV}) at \
             runtime; it is never stored in flywheel.toml."
        )?;
        writeln!(
            out,
            "  Use a session connection (not a transaction-mode pooler): for Supabase the Session \
             pooler (port 5432) or the direct connection, never port 6543."
        )?;
        writeln!(
            out,
            "  Re-run 'flywheel init --provision' with the DSN set to create the schema and tables \
             now instead of on the first worker run."
        )?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "To help future Claude Code sessions discover flywheel, paste this line into your \
         CLAUDE.md (init never edits it for you):"
    )?;
    writeln!(out, "  {INIT_CLAUDEMD_POINTER}")
}

// --- Reconfiguring an existing policy ----------------------------------------------

/// Split raw TOML text into `(header name, chunk)` pairs. The first chunk
/// (name `None`) is the root-table preamble; every other chunk starts at its
/// section header and runs to the next. Concatenating the chunks reproduces
/// the text byte-for-byte.
pub fn split_toml_sections(text: &str) -> Vec<(Option<String>, String)> {
    let mut chunks: Vec<(Option<String>, String)> = Vec::new();
    let mut current: (Option<String>, String) = (None, String::new());
    let mut rest = text;
    while !rest.is_empty() {
        let (line, remainder) = match rest.find('\n') {
            Some(idx) => rest.split_at(idx + 1),
            None => (rest, ""),
        };
        rest = remainder;
        let bare = line.trim_end_matches(['\n', '\r']);
        match TOML_HEADER_RE.captures(bare) {
            Some(caps) => chunks.push(std::mem::replace(
                &mut current,
                (Some(caps["name"].to_owned()), line.to_owned()),
            )),
            None => current.1.push_str(line),
        }
    }
    chunks.push(current);
    chunks
}

/// The run of whitespace-only lines closing `chunk`.
fn trailing_blank_lines(chunk: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut rest = chunk;
    while !rest.is_empty() {
        let (line, remainder) = match rest.find('\n') {
            Some(idx) => rest.split_at(idx + 1),
            None => (rest, ""),
        };
        lines.push(line);
        rest = remainder;
    }
    let mut tail: Vec<&str> = Vec::new();
    for line in lines.iter().rev() {
        if !line.trim().is_empty() {
            break;
        }
        tail.push(line);
    }
    tail.iter().rev().copied().collect()
}

/// Rewrite only the `[source]` and `[store]` sections of a policy; every
/// other section passes through verbatim, and an owned section the file
/// lacks is appended at the end. The directory source's `tasks_dir` is
/// carried over from the existing file.
pub fn rewrite_policy_text(original: &str, data: &toml::Table, answers: &InitAnswers) -> String {
    let tasks_dir = data
        .get("source")
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("tasks_dir"))
        .and_then(toml::Value::as_str)
        .unwrap_or(".flywheel/tasks");
    let mut pending: Vec<(&str, String)> = vec![
        ("source", render_source_block(answers, tasks_dir)),
        ("store", render_store_block(answers)),
    ];
    let mut text = String::new();
    for (name, chunk) in split_toml_sections(original) {
        let replacement = name
            .as_deref()
            .and_then(|n| pending.iter().position(|(key, _)| *key == n))
            .map(|idx| pending.remove(idx).1);
        match replacement {
            Some(block) => {
                text.push_str(&block);
                text.push_str(&trailing_blank_lines(&chunk));
            }
            None => text.push_str(&chunk),
        }
    }
    for (_, block) in pending {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
        text.push_str(&block);
    }
    text
}

/// One-line summary of the settings the reconfigure prompts own.
pub fn describe_current_policy(data: &toml::Table) -> String {
    let store = data.get("store").and_then(toml::Value::as_table);
    let backend = store
        .and_then(|t| t.get("backend"))
        .and_then(toml::Value::as_str)
        .unwrap_or("sqlite");
    let source = data.get("source").and_then(toml::Value::as_table);
    let kind = source
        .and_then(|t| t.get("kind"))
        .and_then(toml::Value::as_str)
        .unwrap_or("(unset)");
    let mut summary = format!("store backend = {backend}, work source = {kind}");
    if let Some(repo) = source
        .and_then(|t| t.get("repo"))
        .and_then(toml::Value::as_str)
    {
        summary.push_str(&format!(" ({repo})"));
    }
    summary
}

fn reconfigure_policy(
    args: &InitArgs,
    host: &mut dyn InitHost,
    root: &Path,
    policy_rel: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let policy_path = root.join(policy_rel);
    let original = std::fs::read_to_string(&policy_path)?;
    let data: toml::Table = match toml::from_str(&original) {
        Ok(data) => data,
        Err(exc) => {
            writeln!(
                err,
                "error: {} is not valid TOML ({}); left untouched",
                policy_rel.display(),
                exc.message()
            )?;
            return Ok(2);
        }
    };
    writeln!(
        out,
        "{}: {}",
        policy_rel.display(),
        describe_current_policy(&data)
    )?;
    let answers = (|| -> Result<Option<InitAnswers>, InitFlowError> {
        if !prompt_yes_no(host, out, "reconfigure?", false)? {
            return Ok(None);
        }
        collect_init_answers(args, host, out, true).map(Some)
    })();
    let answers = match answers {
        Ok(Some(answers)) => answers,
        Ok(None) => {
            writeln!(out, "exists:  {} (left untouched)", policy_rel.display())?;
            writeln!(out)?;
            print_init_next_steps(out, None, false)?;
            return Ok(0);
        }
        Err(InitFlowError::Usage(message)) => {
            writeln!(err, "error: {message}")?;
            return Ok(2);
        }
        Err(InitFlowError::Prompt(PromptError::Interrupted)) => {
            writeln!(err)?;
            writeln!(err, "init: interrupted; flywheel.toml was left untouched")?;
            return Ok(130);
        }
        Err(InitFlowError::Prompt(PromptError::Eof)) => {
            writeln!(
                err,
                "init: stdin closed before the prompts finished; flywheel.toml was left untouched"
            )?;
            return Ok(1);
        }
        Err(InitFlowError::Prompt(PromptError::Io(io))) | Err(InitFlowError::Io(io)) => {
            return Err(io.into());
        }
    };
    std::fs::write(
        &policy_path,
        rewrite_policy_text(&original, &data, &answers),
    )?;
    writeln!(out, "updated: {}", policy_rel.display())?;
    if answers.install_skills {
        install_skills_from_policy_file(root, &policy_path, out)?;
    }
    report_agent_auth(&host.environ(), out)?;
    writeln!(out)?;
    print_init_next_steps(out, Some(&answers.store_backend), answers.install_skills)?;
    Ok(0)
}

// --- git preflight -------------------------------------------------------------------

fn git_stdout(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// An actionable refusal message, or `None` when init may run: the working
/// directory must be a git repository with an attached branch, exactly the
/// preconditions the worker later enforces (spec D-1 / SI-8).
pub fn init_git_preflight_error(root: &Path) -> Option<String> {
    if git_stdout(root, &["rev-parse", "--show-toplevel"]).is_none() {
        return Some(
            "init: the working directory is not a git repository. flywheel drives work through \
             git worktrees, so run `git init` (and make at least one commit on a branch) before \
             initializing flywheel."
                .to_owned(),
        );
    }
    if current_branch(root).is_none() {
        return Some(
            "init: HEAD is detached. flywheel resolves a worker's phase base from the checked-out \
             branch, so check out a branch (`git switch -c <branch>`) before initializing \
             flywheel."
                .to_owned(),
        );
    }
    None
}

/// The repo's current branch name, or `None` when undetectable / detached.
pub fn current_branch(root: &Path) -> Option<String> {
    git_stdout(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|name| !name.is_empty() && name != "HEAD")
}

// --- The verbs -----------------------------------------------------------------------

/// Drive the agent-driven init path (`--agent`) and report the proposal
/// before the policy is written.
pub fn cmd_init_agent(
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let root = host.root();
    let options = InitAgentOptions {
        submit_base: current_branch(&root),
        model: None,
        policy_path: Some(root.join(DEFAULT_POLICY_FILENAME)),
    };
    let result = match host.run_agent_init(&root, &options) {
        Ok(result) => result,
        Err(message) => {
            writeln!(err, "error: {message}")?;
            return Ok(1);
        }
    };
    let proposal = &result.proposal;
    writeln!(out, "Agent proposal:")?;
    if !proposal.notes.is_empty() {
        writeln!(out, "  notes: {}", proposal.notes)?;
    }
    if proposal.default_graders.is_empty() {
        writeln!(out, "  default graders: (none)")?;
    } else {
        writeln!(out, "  default graders:")?;
        for run in &proposal.default_graders {
            writeln!(out, "    - {run}")?;
        }
    }
    writeln!(
        out,
        "  sandbox setup: {}",
        proposal.sandbox_setup.as_deref().unwrap_or("(none)")
    )?;
    if let Some(depth) = proposal.target_depth {
        writeln!(out, "  autopilot target_depth: {depth}")?;
    }
    if let Some(interval) = proposal.interval_seconds {
        writeln!(
            out,
            "  autopilot interval_seconds: {}",
            flywheel_core::task::python_float_repr(interval)
        )?;
    }
    writeln!(
        out,
        "created: {}",
        display_relative(&root, &result.policy_path)
    )?;
    for line in &result.gitignore_added {
        writeln!(out, "gitignore: +{line}")?;
    }
    if let Some(added) = ensure_dockerignore_covers_flywheel(&root)? {
        writeln!(out, "dockerignore: +{added}")?;
    }
    report_agent_auth(&host.environ(), out)?;
    writeln!(out)?;
    print_init_next_steps(out, Some(&result.policy.store_backend), false)?;
    Ok(0)
}

/// Scaffold `.flywheel/` and a `flywheel.toml` in the host's working
/// directory. Idempotent: existing files are left untouched and reported.
/// On a TTY (without `--defaults`) the store backend and work source are
/// prompted for; flags pre-answer individual prompts; a non-TTY stdin takes
/// every default. Answers are collected fully before the policy file is
/// written. With an existing `flywheel.toml`, an interactive run offers to
/// reconfigure it; a non-interactive run never touches it. The git
/// preflight is a hard gate: a non-repo or detached HEAD exits 2 with
/// nothing scaffolded.
pub fn cmd_init(
    args: &InitArgs,
    host: &mut dyn InitHost,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let root = host.root();
    if let Some(message) = init_git_preflight_error(&root) {
        writeln!(err, "{message}")?;
        return Ok(2);
    }

    let mut created: Vec<String> = Vec::new();
    let mut existing: Vec<String> = Vec::new();
    let mut ensure_file = |rel: &str, content: &str| -> std::io::Result<()> {
        let path = root.join(rel);
        if path.exists() {
            existing.push(rel.to_owned());
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        created.push(rel.to_owned());
        Ok(())
    };
    ensure_file(".flywheel/tasks/active/.gitkeep", "")?;
    ensure_file(".flywheel/tasks/archive/.gitkeep", "")?;
    ensure_file(".flywheel/.gitignore", INIT_GITIGNORE)?;
    for path in &created {
        writeln!(out, "created: {path}")?;
    }
    for path in &existing {
        writeln!(out, "exists:  {path} (left untouched)")?;
    }
    if let Some(added) = ensure_dockerignore_covers_flywheel(&root)? {
        writeln!(out, "updated: .dockerignore (+{added})")?;
    }

    let policy_rel = Path::new(DEFAULT_POLICY_FILENAME);
    let policy_path = root.join(policy_rel);
    let interactive = host.stdin_is_tty() && !args.defaults;
    if policy_path.exists() {
        if !interactive {
            writeln!(out, "exists:  {} (left untouched)", policy_rel.display())?;
            if args.skills == Some(true) {
                install_skills_from_policy_file(&root, &policy_path, out)?;
            }
            if args.provision {
                let existing_policy = load_policy(&policy_path)?;
                if existing_policy.store_backend == "postgres" {
                    match postgres_preflight(
                        args,
                        host,
                        existing_policy.store_schema.as_deref(),
                        out,
                    ) {
                        Ok(()) => {}
                        Err(InitFlowError::Usage(message)) => {
                            writeln!(err, "error: {message}")?;
                            return Ok(2);
                        }
                        Err(InitFlowError::Io(io))
                        | Err(InitFlowError::Prompt(PromptError::Io(io))) => {
                            return Err(io.into());
                        }
                        Err(InitFlowError::Prompt(_)) => {}
                    }
                } else {
                    writeln!(
                        out,
                        "note: --provision applies only to the postgres store backend; nothing to \
                         provision"
                    )?;
                }
            }
            writeln!(out)?;
            print_init_next_steps(out, None, args.skills == Some(true))?;
            return Ok(0);
        }
        return reconfigure_policy(args, host, &root, policy_rel, out, err);
    }

    if args.agent {
        return cmd_init_agent(host, out, err);
    }

    let answers = match collect_init_answers(args, host, out, interactive) {
        Ok(answers) => answers,
        Err(InitFlowError::Usage(message)) => {
            writeln!(err, "error: {message}")?;
            return Ok(2);
        }
        Err(InitFlowError::Prompt(PromptError::Interrupted)) => {
            writeln!(err)?;
            writeln!(err, "init: interrupted; flywheel.toml was not written")?;
            return Ok(130);
        }
        Err(InitFlowError::Prompt(PromptError::Eof)) => {
            writeln!(
                err,
                "init: stdin closed before the prompts finished; flywheel.toml was not written"
            )?;
            return Ok(1);
        }
        Err(InitFlowError::Prompt(PromptError::Io(io))) | Err(InitFlowError::Io(io)) => {
            return Err(io.into());
        }
    };
    std::fs::write(&policy_path, render_init_policy(&answers))?;
    writeln!(out, "created: {}", policy_rel.display())?;
    if answers.install_skills {
        install_skills_from_policy_file(&root, &policy_path, out)?;
    }
    report_agent_auth(&host.environ(), out)?;
    writeln!(out)?;
    print_init_next_steps(out, Some(&answers.store_backend), answers.install_skills)?;
    Ok(0)
}

#[cfg(test)]
pub(crate) mod test_host {
    //! A scripted [`InitHost`] for the verb tests.

    use super::*;
    use std::collections::VecDeque;

    type PreflightFn = Box<dyn Fn(&str, &str, PreflightOptions) -> PreflightOutcome>;
    type AgentFn = Box<dyn Fn(&Path, &InitAgentOptions) -> Result<InitAgentResult, String>>;

    pub struct ScriptedHost {
        pub root: PathBuf,
        pub tty: bool,
        pub lines: VecDeque<String>,
        pub interrupt: bool,
        pub environ: Environ,
        pub origin: Option<String>,
        pub preflight: Option<PreflightFn>,
        pub agent: Option<AgentFn>,
        pub preflight_calls: std::cell::RefCell<Vec<String>>,
    }

    impl Default for ScriptedHost {
        fn default() -> Self {
            Self {
                root: std::env::temp_dir(),
                tty: false,
                lines: VecDeque::new(),
                interrupt: false,
                environ: Environ::new(),
                origin: None,
                preflight: None,
                agent: None,
                preflight_calls: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl ScriptedHost {
        pub fn at(root: &Path) -> Self {
            Self {
                root: root.to_path_buf(),
                ..Self::default()
            }
        }

        pub fn interactive(mut self, lines: &[&str]) -> Self {
            self.tty = true;
            self.lines = lines.iter().map(|l| (*l).to_owned()).collect();
            self
        }
    }

    impl InitHost for ScriptedHost {
        fn root(&self) -> PathBuf {
            self.root.clone()
        }

        fn stdin_is_tty(&self) -> bool {
            self.tty
        }

        fn read_line(&mut self) -> Result<String, PromptError> {
            if self.interrupt {
                return Err(PromptError::Interrupted);
            }
            self.lines.pop_front().ok_or(PromptError::Eof)
        }

        fn environ(&self) -> Environ {
            self.environ.clone()
        }

        fn github_repo_from_origin(&self) -> Option<String> {
            self.origin.clone()
        }

        fn postgres_preflight(
            &self,
            dsn: &str,
            schema: &str,
            options: PreflightOptions,
        ) -> PreflightOutcome {
            self.preflight_calls.borrow_mut().push(dsn.to_owned());
            match &self.preflight {
                Some(f) => f(dsn, schema, options),
                None => PreflightOutcome {
                    checks: Vec::new(),
                    provisioned: false,
                },
            }
        }

        fn run_agent_init(
            &self,
            repo_root: &Path,
            options: &InitAgentOptions,
        ) -> Result<InitAgentResult, String> {
            match &self.agent {
                Some(f) => f(repo_root, options),
                None => Err("no agent configured".to_owned()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_host::ScriptedHost;
    use super::*;
    use crate::pg_preflight::{CheckStatus, PreflightCheck};
    use tempfile::TempDir;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_init_attached(repo: &Path) {
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "test@flywheel.invalid"]);
        git(repo, &["config", "user.name", "Flywheel Test"]);
        git(repo, &["commit", "-q", "--allow-empty", "-m", "root"]);
    }

    fn repo() -> (TempDir, ScriptedHost) {
        let tmp = TempDir::new().unwrap();
        git_init_attached(tmp.path());
        let host = ScriptedHost::at(tmp.path());
        (tmp, host)
    }

    fn run(args: &[&str], host: &mut ScriptedHost) -> (i32, String, String) {
        let parsed = super::super::parse_args(
            &["init"]
                .iter()
                .chain(args)
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let super::super::Parsed::Command(super::super::Command::Init(init_args)) = parsed else {
            panic!("not an init command");
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cmd_init(&init_args, host, &mut out, &mut err).unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn policy_text(tmp: &TempDir) -> String {
        std::fs::read_to_string(tmp.path().join("flywheel.toml")).unwrap()
    }

    #[test]
    fn git_preflight_refuses_non_repo_and_detached_head() {
        let tmp = TempDir::new().unwrap();
        let mut host = ScriptedHost::at(tmp.path());
        let (code, _, err) = run(&["--defaults"], &mut host);
        assert_eq!(code, 2);
        assert!(
            !tmp.path().join("flywheel.toml").exists() && !tmp.path().join(".flywheel").exists()
        );
        let lower = err.to_lowercase();
        assert!(lower.contains("git") && lower.contains("repo"));

        let (tmp, mut host) = repo();
        let head = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        git(
            tmp.path(),
            &[
                "checkout",
                "-q",
                String::from_utf8_lossy(&head.stdout).trim(),
            ],
        );
        let (code, _, err) = run(&["--defaults"], &mut host);
        assert_eq!(code, 2);
        assert!(err.to_lowercase().contains("detached"));
        assert!(!tmp.path().join("flywheel.toml").exists());
    }

    #[test]
    fn defaults_scaffold_layout_policy_and_epilogue() {
        let (tmp, mut host) = repo();
        let (code, out, err) = run(&[], &mut host);
        assert_eq!(code, 0, "{err}");
        assert!(tmp.path().join(".flywheel/tasks/active/.gitkeep").is_file());
        assert!(
            tmp.path()
                .join(".flywheel/tasks/archive/.gitkeep")
                .is_file()
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".flywheel/.gitignore")).unwrap(),
            INIT_GITIGNORE
        );
        assert!(out.starts_with("created: .flywheel/tasks/active/.gitkeep\ncreated: .flywheel/tasks/archive/.gitkeep\ncreated: .flywheel/.gitignore\ncreated: flywheel.toml\n"));
        assert!(out.contains("warning: no agent credentials detected"));
        assert!(out.ends_with("\nNext steps:\n  1. Drop one JSON file per task into .flywheel/tasks/active/<phase>/ ('goal' and 'graders' are the only required fields).\n  2. Authenticate the agent: set ANTHROPIC_API_KEY (or run `claude login`) before the first worker run.\n  3. Run: flywheel worker\n  4. Watch: flywheel status / live\n\nTo help future Claude Code sessions discover flywheel, paste this line into your CLAUDE.md (init never edits it for you):\n  This repository runs on flywheel: load the `flywheel-ops` skill to operate the loop, and run `fw docs <topic>` for authoritative flywheel documentation.\n"));
        let text = policy_text(&tmp);
        let expected = render_init_policy(&InitAnswers {
            submit_base: Some("main".into()),
            ..InitAnswers::default()
        });
        assert_eq!(text, expected);
        assert!(text.contains("# base = \"main\"\n"));
        assert!(
            text.contains("# branch (detected current branch: main) \u{2014} the working default.")
        );
        let policy = load_policy(tmp.path().join("flywheel.toml")).unwrap();
        assert_eq!(policy.source_kind, "directory");
        assert_eq!(policy.tasks_dir, Some(PathBuf::from(".flywheel/tasks")));
        assert_eq!(
            policy.db_path,
            Some(PathBuf::from(".flywheel/flywheel.sqlite"))
        );
        assert_eq!(
            policy.sandbox_root,
            Some(PathBuf::from(".flywheel/worktrees"))
        );
        assert_eq!(policy.model, None);
        assert_eq!(policy.store_backend, "sqlite");
        assert_eq!(policy.submit_base, None);
        assert!(policy.protected_paths.contains(&".github/**".to_owned()));
        assert!(policy.protected_paths.contains(&"flywheel.toml".to_owned()));
        assert!(!tmp.path().join("CLAUDE.md").exists());
        // Re-running is idempotent and byte-identical.
        let before = std::fs::read(tmp.path().join("flywheel.toml")).unwrap();
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("exists:  flywheel.toml (left untouched)"));
        assert!(out.contains("flywheel-ops") && out.contains("fw docs"));
        assert_eq!(
            std::fs::read(tmp.path().join("flywheel.toml")).unwrap(),
            before
        );
    }

    #[test]
    fn rendered_policy_is_byte_identical_to_python_for_github_postgres() {
        let answers = InitAnswers {
            store_backend: "postgres".into(),
            store_schema: Some("flywheel_ci".into()),
            source_kind: "github".into(),
            github_repo: Some("octo/widgets".into()),
            github_label: Some("work".into()),
            github_done_action: "close".into(),
            submit_base: Some("feature/x\"y".into()),
            install_skills: false,
        };
        let text = render_init_policy(&answers);
        assert!(text.contains("\n[source]\nkind = \"github\"\nrepo = \"octo/widgets\"\nlabel = \"work\"\ndone_action = \"close\"\n\n[store]\nbackend = \"postgres\"\nschema = \"flywheel_ci\"\n\n[paths]\n"));
        assert!(text.contains("# branch (detected current branch: feature/x\"y) \u{2014} the working default. Set base ONLY to a\n"));
        assert!(text.contains("[submit]\n# base = \"feature/x\\\"y\"\n# verify = \"uv run pytest\"\nprotected_paths = [\".github/**\", \"flywheel.toml\"]\n# strategy = \"merge\"          # merge | phase | pr\n"));
        let unset = render_init_policy(&InitAnswers::default());
        assert!(unset.contains("# branch \u{2014} the working default. Set base ONLY to a\n"));
        assert!(unset.contains("[submit]\n# base = \"main\"\n"));
        for section in [
            "[sandbox]",
            "[phase]",
            "[held_out]",
            "[autopilot]",
            "[worker]",
            "# [agent]\n# model = \"claude-opus-4-8\"",
        ] {
            assert!(
                unset.contains(section),
                "{section} missing from scaffold docs"
            );
        }
    }

    #[test]
    fn scaffold_examples_round_trip_through_the_loader() {
        let (tmp, mut host) = repo();
        run(&["--defaults"], &mut host);
        let text = policy_text(&tmp);
        let agent = text.replace(
            "# [agent]\n# model = \"claude-opus-4-8\"",
            "[agent]\nmodel = \"claude-opus-4-8\"",
        );
        std::fs::write(tmp.path().join("flywheel.toml"), &agent).unwrap();
        assert_eq!(
            load_policy(tmp.path().join("flywheel.toml"))
                .unwrap()
                .model
                .as_deref(),
            Some("claude-opus-4-8")
        );
        let verify = text.replace(
            "[submit]\n# base",
            "[submit]\nverify = \"uv run pytest\"\n# base",
        );
        std::fs::write(tmp.path().join("flywheel.toml"), &verify).unwrap();
        assert_eq!(
            load_policy(tmp.path().join("flywheel.toml"))
                .unwrap()
                .submit_verify
                .as_deref(),
            Some("uv run pytest")
        );
    }

    #[test]
    fn dockerignore_is_appended_once_only_when_a_dockerfile_exists() {
        let (tmp, mut host) = repo();
        std::fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(tmp.path().join(".dockerignore"), "node_modules").unwrap();
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("updated: .dockerignore (+.flywheel/)\n"));
        let text = std::fs::read_to_string(tmp.path().join(".dockerignore")).unwrap();
        assert_eq!(
            text,
            "node_modules\n# Flywheel runtime state (worktrees, logs, db): keep out of image build contexts.\n.flywheel/\n"
        );
        let (_, out, _) = run(&[], &mut host);
        assert!(!out.contains("updated: .dockerignore"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".dockerignore")).unwrap(),
            text
        );

        let (tmp, mut host) = repo();
        run(&[], &mut host);
        assert!(!tmp.path().join(".dockerignore").exists());
    }

    #[test]
    fn agent_auth_report_reads_the_environment() {
        let (tmp, mut host) = repo();
        host.environ
            .insert("ANTHROPIC_API_KEY".into(), "sk-ant-probe".into());
        let (_, out, _) = run(&[], &mut host);
        assert!(out.contains("agent auth: ANTHROPIC_API_KEY is set\n"));
        let (tmp2, mut host) = repo();
        let home = tmp2.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude").join(".credentials.json"), "{}").unwrap();
        host.environ
            .insert("HOME".into(), home.display().to_string());
        let (_, out, _) = run(&[], &mut host);
        assert!(out.contains("agent auth: detected a `claude login` credential file\n"));
        drop(tmp);
    }

    #[test]
    fn flags_pre_answer_prompts_and_usage_errors_write_nothing() {
        let (tmp, mut host) = repo();
        let (code, _, err) = run(&["--pg-schema", "x", "--defaults"], &mut host);
        assert_eq!(code, 2);
        assert!(err.contains("--pg-schema"));
        assert!(!tmp.path().join("flywheel.toml").exists());
        let (code, _, err) = run(&["--repo", "octo/widgets", "--defaults"], &mut host);
        assert_eq!(code, 2);
        assert!(err.contains("--repo/--label"));
        assert!(tmp.path().join(".flywheel/tasks/active/.gitkeep").is_file());
        let (code, _, err) = run(
            &["--source", "github", "--repo", "not-a-repo", "--defaults"],
            &mut host,
        );
        assert_eq!(code, 2);
        assert_eq!(
            err,
            "error: invalid repo 'not-a-repo': expected owner/name\n"
        );
        let (code, _, err) = run(&["--source", "github", "--defaults"], &mut host);
        assert_eq!(code, 2);
        assert!(err.contains("--repo"));

        host.origin = Some("octo/widgets".into());
        let (code, _, _) = run(&["--source", "github", "--defaults"], &mut host);
        assert_eq!(code, 0);
        let policy = load_policy(tmp.path().join("flywheel.toml")).unwrap();
        assert_eq!(policy.github_repo.as_deref(), Some("octo/widgets"));
        assert_eq!(policy.github_label.as_deref(), Some("flywheel"));
        assert_eq!(policy.github_done_action, "comment");

        let (tmp, mut host) = repo();
        let (code, out, _) = run(
            &[
                "--store",
                "postgres",
                "--pg-schema",
                "flywheel_ci",
                "--defaults",
            ],
            &mut host,
        );
        assert_eq!(code, 0);
        assert!(out.contains("postgres: no DSN found; set FLYWHEEL_PG_DSN (or DATABASE_URL) before running flywheel worker\n"));
        assert!(
            out.contains("The postgres DSN is read from FLYWHEEL_PG_DSN (fallback: DATABASE_URL)")
        );
        let policy = load_policy(tmp.path().join("flywheel.toml")).unwrap();
        assert_eq!(policy.store_backend, "postgres");
        assert_eq!(policy.store_schema.as_deref(), Some("flywheel_ci"));
    }

    #[test]
    fn non_tty_without_flags_equals_defaults() {
        let (plain, mut host_plain) = repo();
        let (flagged, mut host_flagged) = repo();
        assert_eq!(run(&[], &mut host_plain).0, 0);
        assert_eq!(run(&["--defaults"], &mut host_flagged).0, 0);
        let text = policy_text(&plain);
        assert_eq!(text, policy_text(&flagged));
        assert!(text.contains("[store]\nbackend = \"sqlite\"\n"));
    }

    #[test]
    fn interactive_prompts_default_reprompt_and_install_skills() {
        let (tmp, mut host) = repo();
        host = host.interactive(&["", "", ""]);
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("store backend ([sqlite]/postgres): "));
        assert!(out.contains("work source ([directory]/github): "));
        assert!(out.contains("install Claude Code skills (fw-spec, fw-plan, fw-verify, fw-retro, fw-improve, flywheel-ops)? ([y]/n): "));
        assert!(out.contains("created: .claude/skills/fw-plan/SKILL.md\n"));
        assert!(out.contains(
            "     Or, in Claude Code: /fw-spec to spec a feature, /fw-plan to queue tasks.\n"
        ));
        assert!(tmp.path().join(".claude/skills/fw-plan/SKILL.md").is_file());

        let (tmp, mut host) = repo();
        host = host.interactive(&["mysql", "sqlite", "", "n"]);
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("  invalid value 'mysql'; choose one of: sqlite, postgres\n"));
        assert_eq!(
            load_policy(tmp.path().join("flywheel.toml"))
                .unwrap()
                .store_backend,
            "sqlite"
        );
    }

    #[test]
    fn interactive_postgres_github_flow_and_origin_prefill() {
        let (tmp, mut host) = repo();
        host = host.interactive(&[
            "postgres",
            "flywheel_ci",
            "github",
            "octo/widgets",
            "",
            "close",
            "n",
        ]);
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("postgres schema [none]: "));
        assert!(out.contains("FLYWHEEL_PG_DSN"));
        let policy = load_policy(tmp.path().join("flywheel.toml")).unwrap();
        assert_eq!(policy.store_backend, "postgres");
        assert_eq!(policy.store_schema.as_deref(), Some("flywheel_ci"));
        assert_eq!(policy.source_kind, "github");
        assert_eq!(policy.github_repo.as_deref(), Some("octo/widgets"));
        assert_eq!(policy.github_label.as_deref(), Some("flywheel"));
        assert_eq!(policy.github_done_action, "close");

        let (tmp, mut host) = repo();
        host.origin = Some("octo/widgets".into());
        host = host.interactive(&["", "github", "", "", "", "n"]);
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("github repo (owner/name) [octo/widgets]: "));
        assert_eq!(
            load_policy(tmp.path().join("flywheel.toml"))
                .unwrap()
                .github_repo
                .as_deref(),
            Some("octo/widgets")
        );

        let (tmp, mut host) = repo();
        host = host.interactive(&["", "github", "not-a-repo", "octo/widgets", "", "", "n"]);
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("  invalid repo 'not-a-repo': expected owner/name\n"));
        assert_eq!(
            load_policy(tmp.path().join("flywheel.toml"))
                .unwrap()
                .github_repo
                .as_deref(),
            Some("octo/widgets")
        );

        let (tmp, mut host) = repo();
        host = host.interactive(&["", "", "n"]);
        let (code, out, _) = run(&["--store", "postgres"], &mut host);
        assert_eq!(code, 0);
        assert!(!out.contains("store backend"));
        assert!(out.contains("postgres schema [none]: "));
        assert_eq!(
            load_policy(tmp.path().join("flywheel.toml"))
                .unwrap()
                .store_backend,
            "postgres"
        );
    }

    const HAND_TUNED: &str = "# Hand-tuned policy: every key here must survive a reconfigure.
[source]
kind = \"directory\"
tasks_dir = \"custom/tasks\"

[paths]
db = \"custom/state.sqlite\"
sandbox_root = \"custom/sandboxes\"

[[defaults.graders]]
type = \"command\"
run = \"uv run pytest -q\"

[agent]
model = \"claude-opus-4-7\"
";

    #[test]
    fn rewrite_preserves_unowned_sections_byte_for_byte() {
        let data: toml::Table = toml::from_str(HAND_TUNED).unwrap();
        let answers = InitAnswers {
            store_backend: "postgres".into(),
            store_schema: Some("flywheel_ci".into()),
            ..InitAnswers::default()
        };
        assert_eq!(
            rewrite_policy_text(HAND_TUNED, &data, &answers),
            format!("{HAND_TUNED}\n[store]\nbackend = \"postgres\"\nschema = \"flywheel_ci\"\n")
        );
        let github = "[source]\nkind = \"github\"\nrepo = \"octo/widgets\"\nlabel = \"flywheel\"\ndone_action = \"comment\"\n\n[store]\nbackend = \"sqlite\"\n";
        let data: toml::Table = toml::from_str(github).unwrap();
        assert_eq!(
            rewrite_policy_text(github, &data, &InitAnswers::default()),
            "[source]\nkind = \"directory\"\ntasks_dir = \".flywheel/tasks\"\n\n[store]\nbackend = \"sqlite\"\n"
        );
        let odd = "x = 1\n[source]\nkind = \"directory\"\n[source.extra]\nk = 2  # c\n[store] # comment\nbackend = \"sqlite\"";
        let data: toml::Table = toml::from_str(odd).unwrap();
        let answers = InitAnswers {
            source_kind: "github".into(),
            github_repo: Some("o/r".into()),
            github_label: Some("l".into()),
            ..InitAnswers::default()
        };
        assert_eq!(
            rewrite_policy_text(odd, &data, &answers),
            "x = 1\n[source]\nkind = \"github\"\nrepo = \"o/r\"\nlabel = \"l\"\ndone_action = \"comment\"\n[source.extra]\nk = 2  # c\n[store]\nbackend = \"sqlite\"\n"
        );
        assert_eq!(
            describe_current_policy(&toml::from_str(HAND_TUNED).unwrap()),
            "store backend = sqlite, work source = directory"
        );
        assert_eq!(
            describe_current_policy(&toml::from_str(github).unwrap()),
            "store backend = sqlite, work source = github (octo/widgets)"
        );
        assert_eq!(
            describe_current_policy(&toml::Table::new()),
            "store backend = sqlite, work source = (unset)"
        );
    }

    #[test]
    fn reconfigure_declines_by_default_and_rewrites_only_owned_keys() {
        let (tmp, mut host) = repo();
        std::fs::write(tmp.path().join("flywheel.toml"), HAND_TUNED).unwrap();
        host = host.interactive(&[""]);
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert_eq!(policy_text(&tmp), HAND_TUNED);
        assert!(out.contains("flywheel.toml: store backend = sqlite, work source = directory\n"));
        assert!(out.contains("reconfigure? (y/[n]): "));
        assert!(out.contains("exists:  flywheel.toml (left untouched)\n"));

        host = host.interactive(&["y", "postgres", "flywheel_ci", "", "n"]);
        let (code, out, _) = run(&[], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("updated: flywheel.toml\n"));
        let text = policy_text(&tmp);
        assert!(
            text.contains("# Hand-tuned policy")
                && text.contains("model = \"claude-opus-4-7\"")
                && text.contains("tasks_dir = \"custom/tasks\"")
        );
        let policy = load_policy(tmp.path().join("flywheel.toml")).unwrap();
        assert_eq!(policy.store_backend, "postgres");
        assert_eq!(policy.store_schema.as_deref(), Some("flywheel_ci"));
        assert_eq!(policy.tasks_dir, Some(PathBuf::from("custom/tasks")));
        assert_eq!(policy.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(policy.default_graders.len(), 1);

        // github -> directory drops the github keys.
        std::fs::write(tmp.path().join("flywheel.toml"), "[source]\nkind = \"github\"\nrepo = \"octo/widgets\"\nlabel = \"flywheel\"\ndone_action = \"comment\"\n\n[store]\nbackend = \"sqlite\"\n").unwrap();
        host = host.interactive(&["y", "", "directory", "n"]);
        assert_eq!(run(&[], &mut host).0, 0);
        let text = policy_text(&tmp);
        assert!(
            !text.contains("repo =")
                && !text.contains("label =")
                && !text.contains("done_action =")
        );
        assert_eq!(
            load_policy(tmp.path().join("flywheel.toml"))
                .unwrap()
                .source_kind,
            "directory"
        );

        // Malformed TOML is reported and left alone.
        std::fs::write(tmp.path().join("flywheel.toml"), "[source\nkind =\n").unwrap();
        host = host.interactive(&["y"]);
        let (code, _, err) = run(&[], &mut host);
        assert_eq!(code, 2);
        assert!(err.contains("not valid TOML"));
        assert_eq!(policy_text(&tmp), "[source\nkind =\n");

        // --defaults never touches an existing policy.
        std::fs::write(tmp.path().join("flywheel.toml"), HAND_TUNED).unwrap();
        host = host.interactive(&["y", "y", "y"]);
        let (code, out, _) = run(&["--defaults"], &mut host);
        assert_eq!(code, 0);
        assert_eq!(policy_text(&tmp), HAND_TUNED);
        assert!(
            out.contains("exists:  flywheel.toml (left untouched)")
                && !out.contains("reconfigure?")
        );
    }

    #[test]
    fn aborting_mid_prompts_writes_no_policy() {
        let (tmp, mut host) = repo();
        host = host.interactive(&["postgres"]);
        let (code, _, err) = run(&[], &mut host);
        assert_eq!(code, 1);
        assert!(!tmp.path().join("flywheel.toml").exists());
        assert_eq!(
            err,
            "init: stdin closed before the prompts finished; flywheel.toml was not written\n"
        );
        assert!(tmp.path().join(".flywheel/tasks/active/.gitkeep").is_file());

        host.tty = true;
        host.interrupt = true;
        let (code, _, err) = run(&[], &mut host);
        assert_eq!(code, 130);
        assert!(!tmp.path().join("flywheel.toml").exists());
        assert_eq!(err, "\ninit: interrupted; flywheel.toml was not written\n");
    }

    #[test]
    fn postgres_preflight_blocks_unless_allowed_and_prefers_the_flywheel_dsn() {
        let dsn = "postgresql://flywheel:secretpw@127.0.0.1:1/flywheel";
        let blocking: PreflightOutcome = PreflightOutcome {
            checks: vec![PreflightCheck::new(
                "connection",
                CheckStatus::Block,
                "could not connect: <dsn>",
            )],
            provisioned: false,
        };
        let (tmp, mut host) = repo();
        host.environ.insert("FLYWHEEL_PG_DSN".into(), dsn.into());
        host.environ
            .insert("DATABASE_URL".into(), "postgresql://fallback/db".into());
        let blocked = blocking.clone();
        host.preflight = Some(Box::new(move |_, _, _| blocked.clone()));
        let (code, out, err) = run(&["--store", "postgres", "--defaults"], &mut host);
        assert_eq!(code, 2);
        assert!(out.contains("[BLOCK] connection"));
        assert!(err.contains("preflight found blocking issues"));
        assert!(!tmp.path().join("flywheel.toml").exists());
        assert_eq!(host.preflight_calls.borrow().as_slice(), [dsn.to_owned()]);

        let (tmp, mut host) = repo();
        host.environ.insert("FLYWHEEL_PG_DSN".into(), dsn.into());
        host.preflight = Some(Box::new(move |_, _, _| blocking.clone()));
        let (code, out, _) = run(
            &["--store", "postgres", "--defaults", "--allow-unverified"],
            &mut host,
        );
        assert_eq!(code, 0);
        assert!(out.contains("[BLOCK] connection"));
        let text = policy_text(&tmp);
        assert!(!text.contains(dsn) && !text.contains("secretpw"));
        assert_eq!(
            load_policy(tmp.path().join("flywheel.toml"))
                .unwrap()
                .store_backend,
            "postgres"
        );

        // --provision over an existing policy re-runs the preflight only
        // for the postgres backend; a blocking report still refuses.
        let (code, out, err) = run(&["--provision", "--defaults"], &mut host);
        assert_eq!(code, 2);
        assert!(out.contains("[BLOCK] connection"));
        assert!(err.contains("preflight found blocking issues"));
        let (code, _, _) = run(
            &["--provision", "--defaults", "--allow-unverified"],
            &mut host,
        );
        assert_eq!(code, 0);
        let (tmp, mut host) = repo();
        run(&["--defaults"], &mut host);
        let (code, out, _) = run(&["--provision", "--defaults"], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains(
            "note: --provision applies only to the postgres store backend; nothing to provision\n"
        ));
        drop(tmp);
    }

    #[test]
    fn skills_flag_regenerates_against_an_existing_policy() {
        let (tmp, mut host) = repo();
        run(&["--defaults"], &mut host);
        let (code, out, _) = run(&["--skills", "--defaults"], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("created: .claude/skills/fw-spec/SKILL.md\n"));
        assert!(out.contains("     Or, in Claude Code: /fw-spec"));
        assert!(
            tmp.path()
                .join(".claude/skills/flywheel-ops/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn agent_init_reports_the_proposal_and_epilogue() {
        use crate::init_agent::InitProposal;
        let (tmp, mut host) = repo();
        let root = tmp.path().to_path_buf();
        host.agent = Some(Box::new(move |repo_root, options| {
            assert_eq!(repo_root, root);
            assert_eq!(options.submit_base.as_deref(), Some("main"));
            let policy_path = options.policy_path.clone().unwrap();
            let rendered = render_init_policy(&InitAnswers::default());
            std::fs::write(&policy_path, &rendered).unwrap();
            Ok(InitAgentResult {
                proposal: InitProposal {
                    default_graders: vec!["cargo test".into()],
                    sandbox_setup: Some("cargo fetch".into()),
                    target_depth: Some(3),
                    interval_seconds: Some(120.0),
                    notes: "rust workspace".into(),
                    ..InitProposal::default()
                },
                policy_path: policy_path.clone(),
                policy: load_policy(&policy_path).unwrap(),
                gitignore_added: vec!["target/".into()],
            })
        }));
        let (code, out, _) = run(&["--agent"], &mut host);
        assert_eq!(code, 0);
        assert!(out.contains("Agent proposal:\n  notes: rust workspace\n  default graders:\n    - cargo test\n  sandbox setup: cargo fetch\n  autopilot target_depth: 3\n  autopilot interval_seconds: 120.0\ncreated: flywheel.toml\ngitignore: +target/\n"));
        assert!(out.ends_with("flywheel documentation.\n"));

        let (tmp, mut host) = repo();
        host.agent = Some(Box::new(|_, _| {
            Err("could not parse an init proposal".to_owned())
        }));
        let (code, _, err) = run(&["--agent"], &mut host);
        assert_eq!(code, 1);
        assert_eq!(err, "error: could not parse an init proposal\n");
        assert!(!tmp.path().join("flywheel.toml").exists());
    }

    #[test]
    fn origin_urls_parse_to_owner_name() {
        assert_eq!(
            github_repo_from_origin_url("git@github.com:octo/widgets.git").as_deref(),
            Some("octo/widgets")
        );
        assert_eq!(
            github_repo_from_origin_url("https://github.com/octo/widgets").as_deref(),
            Some("octo/widgets")
        );
        assert_eq!(
            github_repo_from_origin_url("https://github.com/octo/widgets.git/").as_deref(),
            Some("octo/widgets")
        );
        assert_eq!(
            github_repo_from_origin_url("https://gitlab.com/octo/widgets"),
            None
        );
    }

    #[test]
    fn split_toml_sections_keeps_preamble_and_round_trips() {
        let text = "# top\nkey = 1\n[store]\nbackend = \"sqlite\"\n\n[[submit.tiers]]\ntier = 1";
        let chunks = split_toml_sections(text);
        let names: Vec<Option<&str>> = chunks.iter().map(|(n, _)| n.as_deref()).collect();
        assert_eq!(names, [None, Some("store"), Some("submit.tiers")]);
        assert_eq!(chunks[0].1, "# top\nkey = 1\n");
        assert_eq!(chunks[2].1, "[[submit.tiers]]\ntier = 1");
        let joined: String = chunks.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(joined, text);

        // A leading header still yields the (empty) preamble chunk first.
        let chunks = split_toml_sections("[agent]\nmodel = \"x\"\n");
        assert_eq!(
            chunks,
            [
                (None, String::new()),
                (
                    Some("agent".to_owned()),
                    "[agent]\nmodel = \"x\"\n".to_owned()
                ),
            ]
        );
        assert_eq!(split_toml_sections(""), [(None, String::new())]);
    }
}
