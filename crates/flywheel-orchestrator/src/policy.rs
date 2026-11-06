//! The work policy: the resolved, in-memory parameters every verb runs on.
//!
//! There is no policy file. A [`WorkPolicy`] is synthesized for the home
//! project ([`crate::workflow::policy_for_project`]), the eight-key
//! effective config is applied ([`crate::config::EffectiveConfig::apply_to_policy`]),
//! and a few verb flags fill in the rest (`fw worker --source/--repo/--label`,
//! `--sandbox container --image`, `--agent`; `fw triage --repo`). Every other
//! field is a built-in default defined here. Authoritative reference:
//! `docs/configuration.md` (the table of removed knobs and their fates).
//!
//! Direct construction is a first-class API: tests and embedders build a
//! policy with struct-update syntax over [`WorkPolicy::default`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use flywheel_core::deadline_config::DeadlineConfig;
use flywheel_core::task::Grader;
use serde::{Deserialize, Serialize};

/// The legacy policy file name. Never read: a repo still carrying one gets
/// a one-line "ignored" notice ([`crate::workflow::legacy_layout_notice`]).
pub const LEGACY_POLICY_FILENAME: &str = "flywheel.toml";

/// The work-source kinds `fw worker --source` accepts; mirrors the source
/// registry.
pub const SOURCE_KINDS: [&str; 4] = ["directory", "github", "github_ci", "github_review"];

/// The github source's default intake label (`fw worker --label` overrides).
pub const DEFAULT_GITHUB_LABEL: &str = "flywheel";

/// Autopilot cycle interval (seconds); `fw autopilot --interval` overrides.
pub const DEFAULT_AUTOPILOT_INTERVAL_SECONDS: f64 = 300.0;

/// Queue depth autopilot fills to.
pub const DEFAULT_AUTOPILOT_TARGET_DEPTH: i64 = 5;

/// Default landing posture for autopilot-authored work when the effective
/// config does not say (`land` is applied over it).
pub const DEFAULT_AUTOPILOT_LANDING: &str = "merge";

/// Autopilot authoring sessions run concurrently per refill pass.
pub const DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY: i64 = 3;

/// Lifetime (seconds) of a cached autopilot discovery verdict.
pub const DEFAULT_AUTOPILOT_DISCOVERY_CACHE_TTL_SECONDS: f64 = 21_600.0;

/// Landing postures autopilot work can take.
pub const AUTOPILOT_LANDINGS: [&str; 2] = ["merge", "pr"];

/// Triage board labels and cadence. The intake label matches the github
/// source's default label so the board the drain reads is the board triage
/// writes.
pub const DEFAULT_TRIAGE_INTAKE_LABEL: &str = "flywheel";
pub const DEFAULT_TRIAGE_READY_LABEL: &str = "flywheel:ready";
pub const DEFAULT_TRIAGE_NEEDS_DETAIL_LABEL: &str = "flywheel:needs-detail";
pub const DEFAULT_TRIAGE_INTERVAL_SECONDS: f64 = 300.0;

/// Worker pool size when neither `--concurrency` nor the `concurrency`
/// config key says otherwise (single serial worker).
pub const DEFAULT_WORKER_CONCURRENCY: i64 = 1;

/// Checkpoint-nudge threshold (seconds).
pub const DEFAULT_CHECKPOINT_NUDGE_SECONDS: f64 = 300.0;

/// Ceiling (seconds) clamping a session-limit-driven pool-wide claim pause
/// (6h).
pub const DEFAULT_SESSION_PAUSE_CEILING_SECONDS: f64 = 21600.0;

/// Turn ceiling for the merge strategy's bounded conflict-resolution
/// session.
pub const DEFAULT_RECOVERY_AGENT_MAX_TURNS: i64 = 30;

/// Wall-clock ceiling (seconds) for that session.
pub const DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS: f64 = 900.0;

/// The landing strategies (`land` config key).
pub const SUBMIT_STRATEGIES: [&str; 2] = ["merge", "pr"];

/// The `fw worker --sandbox` vocabulary.
pub const SANDBOX_BACKENDS: [&str; 2] = ["worktree", "container"];

/// Raised when the policy for an invocation cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// A path the policy needs could not be read or resolved.
    #[error("{}: {message}", path.display())]
    Read { path: PathBuf, message: String },
    /// A flag or derived value failed validation.
    #[error("{0}")]
    Invalid(String),
    /// The home project for the working directory could not be resolved
    /// (not a git repo, unusable home, ...).
    #[error("{0}")]
    Project(#[from] crate::home::ProjectError),
    /// The effective config (user / repo / personal files, trust) could
    /// not be loaded, or an untrusted repo file was refused.
    #[error("{0}")]
    Config(#[from] crate::config::ConfigError),
}

/// `[sandbox.exec]` -- bash command isolation (SDK `sandbox` option).
///
/// `fast`/`balanced` disable it and `hardened` enables it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxExec {
    pub enabled: bool,
    pub auto_allow: bool,
}

impl Default for SandboxExec {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_allow: true,
        }
    }
}

/// `[sandbox.capabilities] skills`: `"all"`/`"none"` (or any single name) or
/// an explicit list of skill names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Skills {
    Named(String),
    List(Vec<String>),
}

impl Default for Skills {
    fn default() -> Self {
        Skills::Named("all".to_string())
    }
}

/// `[sandbox.capabilities]` (+ `.mcp`) -- skills/tools/MCP surface.
///
/// `setting_sources` uses an omit-on-unset `None` sentinel: the `fast`
/// baseline sets nothing so the SDK keeps deriving `["user", "project"]`
/// from `skills = "all"`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub skills: Skills,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub setting_sources: Option<Vec<String>>,
    pub mcp_servers: Vec<String>,
    pub mcp_strict: bool,
}

/// `[sandbox.network]` -- network policy.
///
/// Advisory under the worktree backend (enforces nothing); has real teeth
/// only under `[sandbox] backend = "container"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxNetwork {
    pub policy: String,
    pub allow_hosts: Vec<String>,
    pub allow_unix_sockets: Vec<String>,
}

impl Default for SandboxNetwork {
    fn default() -> Self {
        Self {
            policy: "allow".to_string(),
            allow_hosts: Vec::new(),
            allow_unix_sockets: Vec::new(),
        }
    }
}

/// `[sandbox.env]` -- credential/secret name allowlist (declaration half).
///
/// `passthrough` mirrors the `pass` name allowlist; `set_values` mirrors the
/// inline `set` literals. The `fast` baseline inherits the operator's full
/// ambient environment (`inherit_home` true, no explicit scoping). Resolved
/// into the agent environment by [`resolve_agent_env`] and into the
/// command-grader environment by [`resolve_grader_env`], so the agent and
/// the graders that verify its work share one environment (e.g. a build
/// cache).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxEnv {
    pub passthrough: Vec<String>,
    pub set_values: BTreeMap<String, String>,
    pub inherit_home: bool,
}

impl Default for SandboxEnv {
    fn default() -> Self {
        Self {
            passthrough: Vec::new(),
            set_values: BTreeMap::new(),
            inherit_home: true,
        }
    }
}

/// `[sandbox.limits]` -- resource/budget ceilings.
///
/// `max_cost_usd`/`max_tokens`/`wall_clock_seconds` are enforced per-run
/// cumulatively by the harness (`0` = unenforced). `max_turns`/
/// `max_retries`/`lease_seconds` mirror the historical hardcoded values and
/// keep their CLI/default path. `rubric_judge_max_turns` is the per-judge
/// turn budget; `None` (an absent key) keeps the harness default (32).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxLimits {
    pub max_turns: i64,
    pub max_retries: i64,
    pub lease_seconds: i64,
    pub wall_clock_seconds: i64,
    pub max_cost_usd: f64,
    pub max_tokens: i64,
    pub rubric_judge_max_turns: Option<i64>,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_turns: 500,
            max_retries: 1,
            lease_seconds: 300,
            wall_clock_seconds: 0,
            max_cost_usd: 0.0,
            max_tokens: 0,
            rubric_judge_max_turns: None,
        }
    }
}

/// `[sandbox.retention]` -- teardown/disposal policy.
///
/// `on_done`/`on_failure` are threaded through the worktree submitter (done
/// destroy, fail park by default). `sweep_days` stays on the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxRetention {
    pub on_done: String,
    pub on_failure: String,
    pub sweep_days: i64,
}

impl Default for SandboxRetention {
    fn default() -> Self {
        Self {
            on_done: "destroy".to_string(),
            on_failure: "park".to_string(),
            sweep_days: 7,
        }
    }
}

/// `[sandbox.container]` -- container backend config (spec 00045).
///
/// Inert unless `[sandbox] backend = "container"`, at which point `image` is
/// required. `auth` declares a MODE and `auth_env` the env var NAME the token
/// is read from at run time -- the value never lives in the policy file.
/// `model` falls back to the worker's resolved agent model when empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxContainer {
    pub image: String,
    pub model: String,
    pub auth: String,
    pub auth_env: String,
    pub exec_timeout: i64,
    pub egress_network: String,
}

impl Default for SandboxContainer {
    fn default() -> Self {
        Self {
            image: String::new(),
            model: String::new(),
            auth: "oauth".to_string(),
            auth_env: String::new(),
            exec_timeout: 1800,
            egress_network: String::new(),
        }
    }
}

/// The agent execution environment a run gets.
///
/// Not user-configurable: every field is a built-in default except
/// `backend` and `container.image`, which `fw worker --sandbox container
/// --image IMAGE` set. Holds only primitives -- no SDK types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub backend: String,
    pub permission_mode: String,
    pub exec: SandboxExec,
    pub capabilities: SandboxCapabilities,
    pub network: SandboxNetwork,
    pub env: SandboxEnv,
    pub limits: SandboxLimits,
    pub retention: SandboxRetention,
    pub container: SandboxContainer,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            backend: "worktree".to_string(),
            permission_mode: "bypassPermissions".to_string(),
            exec: SandboxExec::default(),
            capabilities: SandboxCapabilities::default(),
            network: SandboxNetwork::default(),
            env: SandboxEnv::default(),
            limits: SandboxLimits::default(),
            retention: SandboxRetention::default(),
            container: SandboxContainer::default(),
        }
    }
}


/// The tunable weights of the `docs/autopilot.md` scheduled-score formula
/// (`[autopilot.weights]`). Defaults are the shipped weights.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutopilotWeights {
    pub tier: f64,
    pub urgency: f64,
    pub importance: f64,
    pub unblock: f64,
    pub effort: f64,
    pub interrupt_base: f64,
}

impl Default for AutopilotWeights {
    fn default() -> Self {
        Self {
            tier: 10.0,
            urgency: 3.0,
            importance: 3.0,
            unblock: 2.0,
            effort: 1.0,
            interrupt_base: 10_000.0,
        }
    }
}

/// The resolved parameters a verb runs on.
///
/// Field groups and where each value comes from:
///
/// * **source** -- `source_kind` (`directory` unless `fw worker --source`),
///   `tasks_dir` (the project's central `tasks/`), the github-kind keys
///   (`--repo`, `--label`, `--require-triage-receipt`; `done_action`
///   `comment`, CI `failure_filter` `failure`). `default_graders` is the
///   `verify` command as one command grader (the graders a tracker item
///   that carries none is verified by).
/// * **paths** -- `db_path` / `sandbox_root` under the project dir.
/// * **agent** -- `model` (config), `agent_id` (`--agent`; `None` is the
///   claude-code adapter).
/// * **store** -- `store_backend` / `store_dsn` (config `store`). A
///   postgres store is what enables cross-machine claims
///   ([`WorkPolicy::distributed`]).
/// * **worker** -- `worker_concurrency` (config `concurrency`), the nudge
///   and pause ceilings (defaults).
/// * **submit** -- `protected_paths`, `submit_strategy` (`land`),
///   `submit_verify` (`verify`), remote/base defaults.
/// * **phase** -- `phase_verify` (the same `verify` command).
/// * **sandbox / deadlines / autopilot / triage** -- built-in defaults
///   (the sandbox backend from `fw worker --sandbox`).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkPolicy {
    pub source_kind: String,
    pub tasks_dir: Option<PathBuf>,
    pub github_repo: Option<String>,
    pub github_label: Option<String>,
    pub github_done_action: String,
    pub github_require_triage_receipt: bool,
    pub github_ci_repo: Option<String>,
    pub github_ci_failure_filter: String,
    pub github_review_repo: Option<String>,
    pub default_graders: Vec<Grader>,
    pub db_path: Option<PathBuf>,
    pub sandbox_root: Option<PathBuf>,
    pub model: Option<String>,
    pub agent_id: Option<String>,
    pub store_backend: String,
    pub execution_capabilities: BTreeSet<String>,
    pub worker_concurrency: i64,
    pub worker_checkpoint_nudge_seconds: f64,
    pub worker_session_pause_ceiling_seconds: f64,
    pub protected_paths: Vec<String>,
    pub submit_strategy: String,
    pub submit_remote: String,
    pub submit_pr_base: Option<String>,
    pub submit_base: Option<String>,
    pub submit_verify: Option<String>,
    pub submit_recovery_agent_max_turns: i64,
    pub submit_recovery_agent_max_wall_seconds: f64,
    pub phase_verify: Option<String>,
    pub sandbox: SandboxPolicy,
    pub deadlines: DeadlineConfig,
    pub autopilot_target_depth: i64,
    pub autopilot_landing: String,
    pub autopilot_interval_seconds: f64,
    pub autopilot_weights: Option<AutopilotWeights>,
    pub autopilot_authoring_concurrency: i64,
    pub autopilot_discovery_cache_ttl_seconds: f64,
    pub triage_intake_label: String,
    pub triage_ready_label: String,
    pub triage_needs_detail_label: String,
    pub triage_interval_seconds: f64,
    pub triage_max_per_pass: Option<i64>,
    /// The home project this policy is bound to: every runtime artifact
    /// lives under `$FLYWHEEL_HOME/projects/<id>/`. `None` only for a
    /// directly constructed policy (tests, embedders).
    pub project: Option<ProjectBinding>,
    /// The postgres DSN the effective config selected (`store =
    /// "postgres://..."`, or `FLYWHEEL_PG_DSN`); `None` lets a postgres
    /// backend resolve its DSN from the environment. Its `Debug` form is
    /// redacted.
    pub store_dsn: Option<crate::store_registry::Dsn>,
}

impl WorkPolicy {
    /// Whether claims must span machines: a postgres store is shared, so
    /// several machines may drain one project (the old `[execution] mode`
    /// knob is gone -- choosing postgres is what enables it).
    pub fn distributed(&self) -> bool {
        self.store_backend == "postgres"
    }

    /// The held-out oracle root: fixed at the project's `held-out/` (the
    /// gate activates once that directory holds registrations). `None` for
    /// a policy bound to no project.
    pub fn held_out_root(&self) -> Option<PathBuf> {
        self.project.as_ref().map(|p| p.paths().held_out())
    }
}

/// The home project a policy is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectBinding {
    pub id: String,
    /// `$FLYWHEEL_HOME/projects/<id>`.
    pub dir: PathBuf,
    /// The current worktree's top-level directory (the invocation's repo
    /// root; graders and git operations anchor here).
    pub repo_root: PathBuf,
}

impl ProjectBinding {
    /// Every path under the bound project dir.
    pub fn paths(&self) -> crate::home::ProjectPaths {
        crate::home::ProjectPaths::at(&self.dir)
    }
}

impl Default for WorkPolicy {
    /// Every built-in default, with a `directory` source whose `tasks_dir`
    /// is unset.
    fn default() -> Self {
        Self {
            source_kind: "directory".to_string(),
            tasks_dir: None,
            github_repo: None,
            github_label: None,
            github_done_action: "comment".to_string(),
            github_require_triage_receipt: false,
            github_ci_repo: None,
            github_ci_failure_filter: "failure".to_string(),
            github_review_repo: None,
            default_graders: Vec::new(),
            db_path: None,
            sandbox_root: None,
            model: None,
            agent_id: None,
            store_backend: "sqlite".to_string(),
            execution_capabilities: BTreeSet::new(),
            worker_concurrency: DEFAULT_WORKER_CONCURRENCY,
            worker_checkpoint_nudge_seconds: DEFAULT_CHECKPOINT_NUDGE_SECONDS,
            worker_session_pause_ceiling_seconds: DEFAULT_SESSION_PAUSE_CEILING_SECONDS,
            protected_paths: Vec::new(),
            submit_strategy: "merge".to_string(),
            submit_remote: "origin".to_string(),
            submit_pr_base: None,
            submit_base: None,
            submit_verify: None,
            submit_recovery_agent_max_turns: DEFAULT_RECOVERY_AGENT_MAX_TURNS,
            submit_recovery_agent_max_wall_seconds: DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS,
            phase_verify: None,
            sandbox: SandboxPolicy::default(),
            deadlines: DeadlineConfig::default(),
            autopilot_target_depth: DEFAULT_AUTOPILOT_TARGET_DEPTH,
            autopilot_landing: DEFAULT_AUTOPILOT_LANDING.to_string(),
            autopilot_interval_seconds: DEFAULT_AUTOPILOT_INTERVAL_SECONDS,
            autopilot_weights: None,
            autopilot_authoring_concurrency: DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY,
            autopilot_discovery_cache_ttl_seconds: DEFAULT_AUTOPILOT_DISCOVERY_CACHE_TTL_SECONDS,
            triage_intake_label: DEFAULT_TRIAGE_INTAKE_LABEL.to_string(),
            triage_ready_label: DEFAULT_TRIAGE_READY_LABEL.to_string(),
            triage_needs_detail_label: DEFAULT_TRIAGE_NEEDS_DETAIL_LABEL.to_string(),
            triage_interval_seconds: DEFAULT_TRIAGE_INTERVAL_SECONDS,
            triage_max_per_pass: None,
            project: None,
            store_dsn: None,
        }
    }
}

// --- Verb flags ------------------------------------------------------------------

/// The work-source and sandbox flags `fw worker` (and, for the github
/// source, `fw triage`) maps onto a policy. Every field is optional; an
/// unset field leaves the policy's default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFlags {
    /// `--source KIND`: one of [`SOURCE_KINDS`].
    pub source: Option<String>,
    /// `--repo OWNER/NAME`: the GitHub repository a github* source reads.
    pub repo: Option<String>,
    /// `--label L`: the github source's intake label.
    pub label: Option<String>,
    /// `--require-triage-receipt`: the github source admits only issues a
    /// triage pass marked ready (team mode).
    pub require_triage_receipt: bool,
    /// `--sandbox BACKEND`: one of [`SANDBOX_BACKENDS`].
    pub sandbox: Option<String>,
    /// `--image IMAGE`: the container backend's image.
    pub image: Option<String>,
    /// `--agent ID`: the agent adapter (`claude-code` default, `codex`).
    pub agent: Option<String>,
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
    )
}

impl RunFlags {
    /// Apply the flags to `policy`, validating them together: a github*
    /// source needs `--repo OWNER/NAME`; `--label` and
    /// `--require-triage-receipt` only mean something for `github`;
    /// `--image` needs `--sandbox container` and the container backend
    /// needs an image.
    pub fn apply(&self, policy: &mut WorkPolicy) -> Result<(), PolicyError> {
        let invalid = |m: String| Err(PolicyError::Invalid(m));
        let kind = self.source.as_deref().unwrap_or("directory");
        if !SOURCE_KINDS.contains(&kind) {
            return invalid(format!(
                "--source must be one of {}, got {kind:?}",
                SOURCE_KINDS.join(", ")
            ));
        }
        if let Some(repo) = &self.repo
            && !valid_repo(repo)
        {
            return invalid(format!("--repo must be OWNER/NAME, got {repo:?}"));
        }
        if kind == "directory" && self.repo.is_some() {
            return invalid("--repo needs --source github, github_ci, or github_review".into());
        }
        if kind != "github" && (self.label.is_some() || self.require_triage_receipt) {
            return invalid(
                "--label and --require-triage-receipt need --source github".to_owned(),
            );
        }
        if kind != "directory" && self.repo.is_none() {
            return invalid(format!("--source {kind} needs --repo OWNER/NAME"));
        }
        policy.source_kind = kind.to_owned();
        match kind {
            "github" => {
                policy.github_repo.clone_from(&self.repo);
                policy.github_label = Some(
                    self.label
                        .clone()
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(|| DEFAULT_GITHUB_LABEL.to_owned()),
                );
                policy.github_require_triage_receipt = self.require_triage_receipt;
            }
            "github_ci" => policy.github_ci_repo.clone_from(&self.repo),
            "github_review" => policy.github_review_repo.clone_from(&self.repo),
            _ => {}
        }
        let backend = self.sandbox.as_deref().unwrap_or("worktree");
        if !SANDBOX_BACKENDS.contains(&backend) {
            return invalid(format!(
                "--sandbox must be one of {}, got {backend:?}",
                SANDBOX_BACKENDS.join(", ")
            ));
        }
        if backend != "container" && self.image.is_some() {
            return invalid("--image needs --sandbox container".to_owned());
        }
        if backend == "container" && self.image.as_deref().is_none_or(str::is_empty) {
            return invalid("--sandbox container needs --image IMAGE".to_owned());
        }
        policy.sandbox.backend = backend.to_owned();
        if let Some(image) = &self.image {
            policy.sandbox.container.image.clone_from(image);
        }
        if let Some(agent) = self.agent.as_deref().filter(|a| !a.is_empty()) {
            policy.agent_id = Some(agent.to_owned());
        }
        Ok(())
    }

    /// The argv that reproduces these flags (a pool supervisor hands them
    /// to its members).
    pub fn to_argv(&self) -> Vec<String> {
        let mut argv = Vec::new();
        let mut one = |flag: &str, value: &Option<String>| {
            if let Some(value) = value {
                argv.push(format!("{flag}={value}"));
            }
        };
        one("--source", &self.source);
        one("--repo", &self.repo);
        one("--label", &self.label);
        one("--sandbox", &self.sandbox);
        one("--image", &self.image);
        one("--agent", &self.agent);
        if self.require_triage_receipt {
            argv.push("--require-triage-receipt".to_owned());
        }
        argv
    }
}

// --- Environment resolution ---------------------------------------------------

/// Resolve `[sandbox.env]` into the agent-side *overrides* the host runtime
/// merges over the inherited environment: the declared `pass` names
/// forwarded from `base_environ` (present-only) plus the inline `set`
/// literals, which win on collision. The agent-side companion of
/// [`resolve_grader_env`]; both must see the same values so an agent build
/// and the command grader verifying it share, e.g., a build cache.
///
/// `base_environ` defaults to the process environment when `None`.
pub fn resolve_agent_env(
    env: &SandboxEnv,
    base_environ: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let process_env;
    let environ = match base_environ {
        Some(environ) => environ,
        None => {
            process_env = process_environ();
            &process_env
        }
    };
    let mut overrides: BTreeMap<String, String> = env
        .passthrough
        .iter()
        .filter_map(|name| environ.get(name).map(|v| (name.clone(), v.clone())))
        .collect();
    overrides.extend(env.set_values.iter().map(|(k, v)| (k.clone(), v.clone())));
    overrides
}

/// Resolve `[sandbox.env]` into the full environment graders run with.
///
/// Command graders run via a subprocess whose environment REPLACES the
/// inherited one, so this returns the FULL env (not just the overrides): the
/// declared `pass` names forwarded from `base_environ` (present-only) plus
/// the inline `set` literals (which win on collision), laid over the ambient
/// environment when `inherit_home` is true, or scoped to only those values
/// when it is false.
///
/// Returns `None` when there is nothing to change (no overrides AND
/// `inherit_home`), so callers pass no env and graders inherit exactly as
/// before. `base_environ` defaults to the process environment when `None`.
pub fn resolve_grader_env(
    env: &SandboxEnv,
    base_environ: Option<&BTreeMap<String, String>>,
) -> Option<BTreeMap<String, String>> {
    let process_env;
    let environ = match base_environ {
        Some(environ) => environ,
        None => {
            process_env = process_environ();
            &process_env
        }
    };
    let overrides = resolve_agent_env(env, Some(environ));
    if env.inherit_home {
        if overrides.is_empty() {
            return None;
        }
        let mut merged = environ.clone();
        merged.extend(overrides);
        return Some(merged);
    }
    Some(overrides)
}

fn process_environ() -> BTreeMap<String, String> {
    std::env::vars().collect()
}


// --- Sandbox root resolution ----------------------------------------------------

/// Resolve `fw worker --sandbox-root PATH` to the absolute worktree root.
///
/// `None` keeps `default` (the project's `worktrees/` under
/// `$FLYWHEEL_HOME`). A relative path anchors at `repo_root` -- never the
/// process cwd; an absolute path is used verbatim.
pub fn resolve_sandbox_root(configured: Option<&Path>, repo_root: &Path, default: &Path) -> PathBuf {
    match configured {
        None => resolve_nonstrict(default),
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => resolve_nonstrict(&repo_root.join(path)),
    }
}

/// Make `path` absolute and resolve symlinks without requiring it to exist
/// (the twin of Python's non-strict `Path.resolve()`): the longest existing
/// prefix is canonicalized and the remaining components are appended
/// lexically, with `.` dropped and `..` popping the preceding component.
fn resolve_nonstrict(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let components: Vec<Component<'_>> = absolute.components().collect();
    for split in (1..=components.len()).rev() {
        let prefix: PathBuf = components[..split].iter().collect();
        let Ok(mut out) = fs::canonicalize(&prefix) else {
            continue;
        };
        for component in &components[split..] {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        return out;
    }
    absolute
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- env resolution ------------------------------------------------------------

    fn environ(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn grader_env_none_when_nothing_to_change() {
        let env = SandboxEnv::default();
        assert_eq!(
            resolve_grader_env(&env, Some(&environ(&[("HOME", "/h")]))),
            None
        );
        assert!(resolve_agent_env(&env, Some(&environ(&[("HOME", "/h")]))).is_empty());
    }

    #[test]
    fn grader_env_merges_overrides_over_ambient_when_inheriting() {
        let env = SandboxEnv {
            passthrough: vec!["CARGO_HOME".into(), "MISSING".into()],
            set_values: BTreeMap::from([("RUSTC_WRAPPER".to_string(), "sccache".to_string())]),
            inherit_home: true,
        };
        let base = environ(&[
            ("HOME", "/h"),
            ("CARGO_HOME", "/c"),
            ("RUSTC_WRAPPER", "old"),
        ]);
        let resolved = resolve_grader_env(&env, Some(&base)).unwrap();
        assert_eq!(
            resolved,
            environ(&[
                ("HOME", "/h"),
                ("CARGO_HOME", "/c"),
                ("RUSTC_WRAPPER", "sccache")
            ])
        );
        assert_eq!(
            resolve_agent_env(&env, Some(&base)),
            environ(&[("CARGO_HOME", "/c"), ("RUSTC_WRAPPER", "sccache")])
        );
    }

    #[test]
    fn grader_env_scopes_to_declared_values_when_not_inheriting() {
        let env = SandboxEnv {
            passthrough: vec!["PATH".into()],
            set_values: BTreeMap::new(),
            inherit_home: false,
        };
        let base = environ(&[("PATH", "/bin"), ("SECRET", "x")]);
        assert_eq!(
            resolve_grader_env(&env, Some(&base)),
            Some(environ(&[("PATH", "/bin")]))
        );
        let bare = SandboxEnv {
            inherit_home: false,
            ..SandboxEnv::default()
        };
        assert_eq!(
            resolve_grader_env(&bare, Some(&base)),
            Some(BTreeMap::new())
        );
    }

    // --- sandbox root --------------------------------------------------------------

    #[test]
    fn sandbox_root_default_relative_and_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fs::canonicalize(dir.path()).unwrap();
        let home_default = repo.join("home").join("worktrees");
        assert_eq!(
            resolve_sandbox_root(None, &repo, &home_default),
            home_default
        );
        assert_eq!(
            resolve_sandbox_root(Some(Path::new("wt/../boxes")), &repo, &home_default),
            repo.join("boxes")
        );
        assert_eq!(
            resolve_sandbox_root(Some(Path::new("/abs/root")), &repo, &home_default),
            PathBuf::from("/abs/root")
        );
    }

    // --- run flags -----------------------------------------------------------------

    fn applied(flags: RunFlags) -> Result<WorkPolicy, String> {
        let mut policy = WorkPolicy::default();
        flags.apply(&mut policy).map_err(|e| e.to_string())?;
        Ok(policy)
    }

    #[test]
    fn no_flags_keep_the_directory_source_and_worktree_backend() {
        let policy = applied(RunFlags::default()).unwrap();
        assert_eq!(policy, WorkPolicy::default());
    }

    #[test]
    fn github_flags_select_the_issue_source() {
        let policy = applied(RunFlags {
            source: Some("github".into()),
            repo: Some("octo/widgets".into()),
            require_triage_receipt: true,
            ..RunFlags::default()
        })
        .unwrap();
        assert_eq!(policy.source_kind, "github");
        assert_eq!(policy.github_repo.as_deref(), Some("octo/widgets"));
        assert_eq!(policy.github_label.as_deref(), Some(DEFAULT_GITHUB_LABEL));
        assert!(policy.github_require_triage_receipt);
        let ci = applied(RunFlags {
            source: Some("github_ci".into()),
            repo: Some("octo/widgets".into()),
            ..RunFlags::default()
        })
        .unwrap();
        assert_eq!(ci.github_ci_repo.as_deref(), Some("octo/widgets"));
        let review = applied(RunFlags {
            source: Some("github_review".into()),
            repo: Some("octo/widgets".into()),
            ..RunFlags::default()
        })
        .unwrap();
        assert_eq!(review.github_review_repo.as_deref(), Some("octo/widgets"));
    }

    #[test]
    fn inconsistent_source_flags_are_refused() {
        let err = |flags: RunFlags| applied(flags).unwrap_err();
        assert!(err(RunFlags { source: Some("jira".into()), ..RunFlags::default() })
            .contains("--source must be one of"));
        assert!(err(RunFlags { source: Some("github".into()), ..RunFlags::default() })
            .contains("needs --repo OWNER/NAME"));
        assert!(err(RunFlags {
            source: Some("github".into()),
            repo: Some("noslash".into()),
            ..RunFlags::default()
        })
        .contains("--repo must be OWNER/NAME"));
        assert!(err(RunFlags { repo: Some("a/b".into()), ..RunFlags::default() })
            .contains("--repo needs --source"));
        assert!(err(RunFlags {
            source: Some("github_ci".into()),
            repo: Some("a/b".into()),
            label: Some("x".into()),
            ..RunFlags::default()
        })
        .contains("need --source github"));
    }

    #[test]
    fn container_flags_select_the_container_backend() {
        let policy = applied(RunFlags {
            sandbox: Some("container".into()),
            image: Some("ghcr.io/o/i:1".into()),
            agent: Some("codex".into()),
            ..RunFlags::default()
        })
        .unwrap();
        assert_eq!(policy.sandbox.backend, "container");
        assert_eq!(policy.sandbox.container.image, "ghcr.io/o/i:1");
        assert_eq!(policy.agent_id.as_deref(), Some("codex"));
        let err = |flags: RunFlags| applied(flags).unwrap_err();
        assert!(err(RunFlags { sandbox: Some("container".into()), ..RunFlags::default() })
            .contains("needs --image"));
        assert!(err(RunFlags { image: Some("i".into()), ..RunFlags::default() })
            .contains("--image needs --sandbox container"));
        assert!(err(RunFlags { sandbox: Some("vm".into()), ..RunFlags::default() })
            .contains("--sandbox must be one of"));
    }

    #[test]
    fn run_flags_round_trip_through_argv() {
        let flags = RunFlags {
            source: Some("github".into()),
            repo: Some("o/r".into()),
            label: Some("ready".into()),
            require_triage_receipt: true,
            sandbox: Some("container".into()),
            image: Some("img".into()),
            agent: Some("codex".into()),
        };
        assert_eq!(
            flags.to_argv(),
            [
                "--source=github",
                "--repo=o/r",
                "--label=ready",
                "--sandbox=container",
                "--image=img",
                "--agent=codex",
                "--require-triage-receipt"
            ]
        );
    }

    #[test]
    fn a_postgres_store_is_what_makes_a_policy_distributed() {
        assert!(!WorkPolicy::default().distributed());
        let pg = WorkPolicy {
            store_backend: "postgres".into(),
            ..WorkPolicy::default()
        };
        assert!(pg.distributed());
    }
}
