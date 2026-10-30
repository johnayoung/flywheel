//! Repo-owned work policy: `flywheel.toml`.
//!
//! The policy file is the consumer repo's versioned contract with the
//! orchestrator -- *where work comes from* and *what "runnable" means by
//! default* -- so switching a project between a task directory and an issue
//! tracker is a committed config change, not a flywheel code change.
//!
//! Port of the Python `flywheel_orchestrator._policy` module. One function
//! owns the whole surface: [`load_policy`] parses the file into a validated
//! [`WorkPolicy`]; every key is validated here and every default lives here.
//! The contract is *strict on values, lenient on keys*: a wrong-typed or
//! out-of-enum value fails fast with a [`PolicyError`] naming the offending
//! file and key, while unknown keys under a known table and unknown section
//! tables are ignored for forward-compatibility. `[source]` is the only
//! required table. Authoritative reference: `docs/configuration.md`,
//! `docs/sandbox.md`, `docs/strategy.md`, `docs/orchestration.md`,
//! `docs/autopilot.md`.
//!
//! Credentials and DSNs never live in the file: `[sandbox.env]` and
//! `[sandbox.container]` carry environment variable *names*, never values.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use flywheel_core::deadline_config::{DeadlineConfig, deadline_config_from_mapping};
use flywheel_core::loaders::load_graders;
use flywheel_core::task::Grader;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml::{Table, Value};

/// The policy file name auto-detected at a repo root.
pub const DEFAULT_POLICY_FILENAME: &str = "flywheel.toml";

/// The historical default task directory for `kind = "directory"`.
pub const DEFAULT_TASKS_DIR: &str = ".flywheel/tasks";

/// The work-source kinds `[source] kind` accepts; mirrors the source
/// registry so an unknown kind is rejected at load, before any source is
/// built.
pub const SOURCE_KINDS: [&str; 4] = ["directory", "github", "github_ci", "github_review"];

/// The `[source] done_action` vocabulary for the github kind.
pub const DONE_ACTIONS: [&str; 2] = ["comment", "close"];

/// Default autopilot cycle interval (seconds) when `[autopilot]` omits one.
pub const DEFAULT_AUTOPILOT_INTERVAL_SECONDS: f64 = 300.0;

/// Default queue depth autopilot fills to when `[autopilot]` omits one.
pub const DEFAULT_AUTOPILOT_TARGET_DEPTH: i64 = 5;

/// Default landing posture for autopilot-authored work (FF-merge autonomy).
pub const DEFAULT_AUTOPILOT_LANDING: &str = "merge";

/// Default number of autopilot authoring sessions run concurrently per
/// refill pass (`[autopilot] authoring_concurrency`; 1 = serial).
pub const DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY: i64 = 3;

/// Default lifetime (seconds) of a cached autopilot discovery verdict
/// (`[autopilot] discovery_cache_ttl_seconds`; 0 disables the cache).
pub const DEFAULT_AUTOPILOT_DISCOVERY_CACHE_TTL_SECONDS: f64 = 21_600.0;

fn is_default_authoring_concurrency(value: &i64) -> bool {
    *value == DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY
}

fn default_authoring_concurrency() -> i64 {
    DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY
}

fn is_default_discovery_cache_ttl(value: &f64) -> bool {
    *value == DEFAULT_AUTOPILOT_DISCOVERY_CACHE_TTL_SECONDS
}

fn default_discovery_cache_ttl() -> f64 {
    DEFAULT_AUTOPILOT_DISCOVERY_CACHE_TTL_SECONDS
}

/// Landing postures autopilot work can take.
pub const AUTOPILOT_LANDINGS: [&str; 2] = ["merge", "pr"];

/// Default triage board labels and cadence when `[triage]` omits them. The
/// intake label matches the github source's default label so the board the
/// drain reads is the board triage writes.
pub const DEFAULT_TRIAGE_INTAKE_LABEL: &str = "flywheel";
pub const DEFAULT_TRIAGE_READY_LABEL: &str = "flywheel:ready";
pub const DEFAULT_TRIAGE_NEEDS_DETAIL_LABEL: &str = "flywheel:needs-detail";
pub const DEFAULT_TRIAGE_INTERVAL_SECONDS: f64 = 300.0;

/// The `[store] backend` vocabulary.
pub const STORE_BACKENDS: [&str; 2] = ["sqlite", "postgres"];
/// Valid `[agent] transport` values: every shipped adapter (claude-code,
/// codex) runs its agent CLI headless; there is no other transport.
pub const AGENT_TRANSPORTS: [&str; 1] = ["cli"];

/// The `[execution] mode` vocabulary.
pub const EXECUTION_MODES: [&str; 2] = ["local", "distributed"];

/// Default worker pool size when `[worker]` omits `concurrency` (single
/// serial worker, spec 00060 D-1).
pub const DEFAULT_WORKER_CONCURRENCY: i64 = 1;

/// Default checkpoint-nudge threshold (seconds) when `[worker]` omits
/// `checkpoint_nudge_seconds`. Default-on; `0` disables the nudge.
pub const DEFAULT_CHECKPOINT_NUDGE_SECONDS: f64 = 300.0;

/// Default ceiling (seconds) clamping a session-limit-driven pool-wide claim
/// pause when `[worker]` omits `session_pause_ceiling_seconds` (6h); `0`
/// disables pausing entirely.
pub const DEFAULT_SESSION_PAUSE_CEILING_SECONDS: f64 = 21600.0;

/// Default turn ceiling for the merge strategy's bounded conflict-resolution
/// session when `[submit]` omits `recovery_agent_max_turns`; `0` disables
/// the rung.
pub const DEFAULT_RECOVERY_AGENT_MAX_TURNS: i64 = 30;

/// Default wall-clock ceiling (seconds) for that session when `[submit]`
/// omits `recovery_agent_max_wall_seconds`.
pub const DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS: f64 = 900.0;

/// The `[submit] strategy` vocabulary.
pub const SUBMIT_STRATEGIES: [&str; 3] = ["merge", "pr", "phase"];

/// The strategies that serialize landings through the single-machine merge
/// lock (`.flywheel/.merge.lock` -- advisory, kernel-local). Shared invariant
/// of the distributed-mode guard in [`load_policy`] and the
/// `docs/team-mode.md` strategy table (spec 00081).
pub const LOCAL_LOCK_STRATEGIES: [&str; 2] = ["merge", "phase"];

/// The closed risk-tier vocabulary of `[[submit.tiers]]` (spec 00080): tier 0
/// lands direct (merge), tier 1 on the phase integration branch, tier 2 via a
/// PR requiring human approval.
pub const SUBMIT_TIER_LEVELS: [i64; 3] = [0, 1, 2];

/// The tier a file matching no `[[submit.tiers]]` rule classifies at (D-3),
/// so a newly invented path is never a silent direct-merge lane.
pub const SUBMIT_TIER_DEFAULT: i64 = 1;

/// The `[sandbox] preset` vocabulary, in declaration order.
pub const SANDBOX_PRESET_NAMES: [&str; 3] = ["fast", "balanced", "hardened"];

/// The `[sandbox] backend` vocabulary.
pub const SANDBOX_BACKENDS: [&str; 2] = ["worktree", "container"];

/// The `[sandbox.network] policy` vocabulary.
pub const NETWORK_POLICIES: [&str; 2] = ["allow", "deny"];

/// The `[sandbox.container] auth` vocabulary.
pub const CONTAINER_AUTH_MODES: [&str; 4] = ["oauth", "session", "api_key", "none"];

/// The `[sandbox.retention] on_done` vocabulary.
pub const RETENTION_ON_DONE: [&str; 2] = ["destroy", "preserve"];

/// The `[sandbox.retention] on_failure` vocabulary.
pub const RETENTION_ON_FAILURE: [&str; 2] = ["park", "destroy"];

/// The `[paths] sandbox_root` token for an XDG-cache-keyed worktree root.
pub const SANDBOX_ROOT_CACHE_TOKEN: &str = "@cache";

/// The `[paths] sandbox_root` token for a `<repo>.worktrees` sibling root.
pub const SANDBOX_ROOT_SIBLING_TOKEN: &str = "@sibling";

/// Raised when a policy file is missing, unparseable, or invalid.
///
/// The message always identifies the offending file (and key, where
/// applicable) so the operator's fix is unambiguous. The message text
/// mirrors the Python module so operators see the same diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The file could not be read.
    #[error("{}: cannot read policy file: {message}", path.display())]
    Read { path: PathBuf, message: String },
    /// The file is not valid UTF-8 TOML.
    #[error("{}: invalid TOML: {message}", path.display())]
    InvalidToml { path: PathBuf, message: String },
    /// A value failed validation; the message names the file and key.
    #[error("{0}")]
    Invalid(String),
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

/// Resolved `[sandbox.*]` configuration: a preset baseline with sparse
/// per-key repo overrides merged on top.
///
/// The defaults of this struct (and its nested sub-structs) *are* the `fast`
/// preset. Holds only primitives -- no SDK types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub preset: String,
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
            preset: "fast".to_string(),
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

/// The named, code-owned sandbox preset `name` resolves to, or `None` for
/// an unknown name.
///
/// `fast` is [`SandboxPolicy::default`]. `balanced` adds strict MCP gating
/// (no implicit servers) while keeping full coding capability. `hardened`
/// further locks the tool allowlist to a minimal coding set, scopes settings
/// to the project, and enables bash exec isolation. Every preset keeps
/// `permission_mode == "bypassPermissions"`; env/limits/network/retention
/// stay at `fast` so a preset never advertises a guarantee it cannot
/// enforce.
pub fn sandbox_preset(name: &str) -> Option<SandboxPolicy> {
    match name {
        "fast" => Some(SandboxPolicy::default()),
        "balanced" => Some(SandboxPolicy {
            preset: "balanced".to_string(),
            capabilities: SandboxCapabilities {
                mcp_strict: true,
                ..SandboxCapabilities::default()
            },
            ..SandboxPolicy::default()
        }),
        "hardened" => Some(SandboxPolicy {
            preset: "hardened".to_string(),
            capabilities: SandboxCapabilities {
                allowed_tools: ["Bash", "Edit", "Glob", "Grep", "Read", "Write"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                setting_sources: Some(vec!["project".to_string()]),
                mcp_strict: true,
                ..SandboxCapabilities::default()
            },
            exec: SandboxExec {
                enabled: true,
                ..SandboxExec::default()
            },
            ..SandboxPolicy::default()
        }),
        _ => None,
    }
}

/// One `[[submit.tiers]]` path rule (spec 00080).
///
/// `paths` are glob patterns over a DONE branch's changed files (the same
/// vocabulary as `[submit] protected_paths`); `tier` is one of
/// [`SUBMIT_TIER_LEVELS`]. The classifier is worker-side and highest-wins; an
/// unmatched file classifies at [`SUBMIT_TIER_DEFAULT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitTierRule {
    pub tier: i64,
    pub paths: Vec<String>,
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

/// Parsed, validated `flywheel.toml` contents.
///
/// Field groups:
///
/// * **source** -- `source_kind` plus the per-kind keys: `tasks_dir`
///   (directory), `github_repo`/`github_label`/`github_done_action`/
///   `github_require_triage_receipt` (github), `github_ci_repo`/
///   `github_ci_failure_filter` (github_ci), `github_review_repo`
///   (github_review). `default_graders` is `[[defaults.graders]]`.
/// * **paths** -- `db_path`/`sandbox_root`, `None` when unset (the CLI falls
///   back to its built-in defaults). `sandbox_root` is stored verbatim -- a
///   path or an `@cache`/`@sibling` token -- and turned into the absolute
///   worktree root by [`resolve_sandbox_root`] at use time.
/// * **agent** -- `model` (opaque, passed verbatim), `agent_id` (the
///   multi-agent opt-in), `agent_transport` (requires `agent_id`).
/// * **store** -- `store_backend` (`sqlite` default) / `store_schema`.
/// * **execution** -- `execution_mode` (`local` default; `distributed`
///   requires postgres and the `pr` strategy) and `execution_capabilities`
///   (this worker's advertised set, distinct from `[sandbox.capabilities]`).
/// * **worker** -- `worker_concurrency` (the `< 1` range is deliberately NOT
///   checked at load: `--concurrency` overrides it, so the worker validates
///   the resolved value), `worker_checkpoint_nudge_seconds`,
///   `worker_session_pause_ceiling_seconds`.
/// * **submit** -- `protected_paths`, `submit_tiers`, `submit_strategy`,
///   `submit_remote`, `submit_pr_base`, `submit_base`, `submit_verify`,
///   `submit_recovery_agent_max_turns`,
///   `submit_recovery_agent_max_wall_seconds`.
/// * **sandbox** -- `sandbox_setup` (the flat `[sandbox] setup` command) and
///   the resolved `[sandbox.*]` tables in `sandbox`.
/// * **phase / held_out** -- `phase_verify`, `held_out_root` (no default:
///   a default would silently activate gating on upgrade).
/// * **deadlines** -- the core [`DeadlineConfig`] (spec 00066).
/// * **autopilot** -- `autopilot_target_depth`, `autopilot_landing`,
///   `autopilot_interval_seconds`, `autopilot_weights`,
///   `autopilot_authoring_concurrency`,
///   `autopilot_discovery_cache_ttl_seconds` (the last two serialize only
///   when set off their defaults).
/// * **triage** -- the three board labels, `triage_interval_seconds`,
///   `triage_max_per_pass` (`None` = uncapped).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub agent_transport: Option<String>,
    pub store_backend: String,
    pub store_schema: Option<String>,
    pub execution_mode: String,
    pub execution_capabilities: BTreeSet<String>,
    pub worker_concurrency: i64,
    pub worker_checkpoint_nudge_seconds: f64,
    pub worker_session_pause_ceiling_seconds: f64,
    pub protected_paths: Vec<String>,
    pub submit_tiers: Vec<SubmitTierRule>,
    pub submit_strategy: String,
    pub submit_remote: String,
    pub submit_pr_base: Option<String>,
    pub submit_base: Option<String>,
    pub submit_verify: Option<String>,
    pub submit_recovery_agent_max_turns: i64,
    pub submit_recovery_agent_max_wall_seconds: f64,
    pub sandbox_setup: Option<String>,
    pub phase_verify: Option<String>,
    pub held_out_root: Option<PathBuf>,
    pub sandbox: SandboxPolicy,
    pub deadlines: DeadlineConfig,
    pub autopilot_target_depth: i64,
    pub autopilot_landing: String,
    pub autopilot_interval_seconds: f64,
    pub autopilot_weights: Option<AutopilotWeights>,
    #[serde(
        default = "default_authoring_concurrency",
        skip_serializing_if = "is_default_authoring_concurrency"
    )]
    pub autopilot_authoring_concurrency: i64,
    #[serde(
        default = "default_discovery_cache_ttl",
        skip_serializing_if = "is_default_discovery_cache_ttl"
    )]
    pub autopilot_discovery_cache_ttl_seconds: f64,
    pub triage_intake_label: String,
    pub triage_ready_label: String,
    pub triage_needs_detail_label: String,
    pub triage_interval_seconds: f64,
    pub triage_max_per_pass: Option<i64>,
}

impl Default for WorkPolicy {
    /// The back-compat defaults every optional section resolves to, with a
    /// `directory` source whose `tasks_dir` is unset. [`load_policy`] always
    /// populates the per-kind source fields; direct construction is a
    /// first-class API for callers that assemble a policy without a file.
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
            agent_transport: None,
            store_backend: "sqlite".to_string(),
            store_schema: None,
            execution_mode: "local".to_string(),
            execution_capabilities: BTreeSet::new(),
            worker_concurrency: DEFAULT_WORKER_CONCURRENCY,
            worker_checkpoint_nudge_seconds: DEFAULT_CHECKPOINT_NUDGE_SECONDS,
            worker_session_pause_ceiling_seconds: DEFAULT_SESSION_PAUSE_CEILING_SECONDS,
            protected_paths: Vec::new(),
            submit_tiers: Vec::new(),
            submit_strategy: "merge".to_string(),
            submit_remote: "origin".to_string(),
            submit_pr_base: None,
            submit_base: None,
            submit_verify: None,
            submit_recovery_agent_max_turns: DEFAULT_RECOVERY_AGENT_MAX_TURNS,
            submit_recovery_agent_max_wall_seconds: DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS,
            sandbox_setup: None,
            phase_verify: None,
            held_out_root: None,
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
        }
    }
}

// --- Loading -----------------------------------------------------------------

/// Parse and validate the policy file at `path`.
///
/// `path` may name the file itself or a directory (a repo root), in which
/// case [`DEFAULT_POLICY_FILENAME`] inside it is loaded. Every diagnostic
/// names the resolved file.
pub fn load_policy(path: impl AsRef<Path>) -> Result<WorkPolicy, PolicyError> {
    let mut path = path.as_ref().to_path_buf();
    if path.is_dir() {
        path.push(DEFAULT_POLICY_FILENAME);
    }
    let bytes = fs::read(&path).map_err(|err| PolicyError::Read {
        path: path.clone(),
        message: err.to_string(),
    })?;
    let text = String::from_utf8(bytes).map_err(|err| PolicyError::InvalidToml {
        path: path.clone(),
        message: err.to_string(),
    })?;
    load_policy_str(&text, &path)
}

/// Parse and validate policy `text`, attributing diagnostics to
/// `policy_file`. The in-memory twin of [`load_policy`].
pub fn load_policy_str(text: &str, policy_file: &Path) -> Result<WorkPolicy, PolicyError> {
    let data: Table = toml::from_str(text).map_err(|err| PolicyError::InvalidToml {
        path: policy_file.to_path_buf(),
        message: err.to_string(),
    })?;
    load_policy_table(&data, policy_file)
}

/// Validate an already-parsed TOML document into a [`WorkPolicy`].
pub fn load_policy_table(data: &Table, policy_file: &Path) -> Result<WorkPolicy, PolicyError> {
    let ctx = Ctx { file: policy_file };

    let Some(source) = data.get("source").and_then(Value::as_table) else {
        return Err(ctx.err("missing required [source] table"));
    };
    let kind = match source.get("kind").and_then(Value::as_str) {
        Some(kind) if SOURCE_KINDS.contains(&kind) => kind,
        _ => {
            return Err(ctx.err(format!(
                "source.kind must be one of {}, got {}",
                py_tuple(&SOURCE_KINDS),
                py_repr_opt(source.get("kind"))
            )));
        }
    };

    let defaults = ctx.section(data, "defaults")?;
    let raw_graders = defaults
        .get("graders")
        .map(toml_to_json)
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    let default_graders = load_graders(
        &raw_graders,
        &format!("{}: defaults", policy_file.display()),
    )
    .map_err(|err| PolicyError::Invalid(err.to_string()))?;

    let paths = ctx.section(data, "paths")?;
    let db_path = ctx.optional_path(paths, "db")?;
    let sandbox_root = ctx.optional_path(paths, "sandbox_root")?;

    let agent = ctx.section(data, "agent")?;
    let model = ctx.optional_nonempty_str(agent, "model", "agent.model")?;
    let agent_id = ctx.optional_nonempty_str(agent, "id", "agent.id")?;
    let agent_transport = ctx.optional_nonempty_str(agent, "transport", "agent.transport")?;
    if agent_transport.is_some() && agent_id.is_none() {
        return Err(ctx.err(
            "agent.transport requires agent.id (the multi-agent opt-in; see docs/agent-harness.md)",
        ));
    }
    if let Some(transport) = agent_transport.as_deref()
        && !AGENT_TRANSPORTS.contains(&transport)
    {
        // Rejected here rather than at the first run: every shipped adapter
        // speaks only the CLI transport, so any other value would fail every
        // run after the daemon started.
        return Err(ctx.err(format!(
            "agent.transport must be one of {}, got {}; the Python-era \"sdk\" transport is \
             gone (the cli transport speaks the same stream-json control protocol) -- set \
             transport = \"cli\" or remove the key",
            py_tuple(&AGENT_TRANSPORTS),
            py_str_repr(transport)
        )));
    }

    let store = ctx.section(data, "store")?;
    let store_backend =
        ctx.choice_or_default(store, "backend", "sqlite", "store.backend", &STORE_BACKENDS)?;
    let store_schema = ctx.optional_nonempty_str(store, "schema", "store.schema")?;

    let execution = ctx.section(data, "execution")?;
    let execution_mode = ctx.choice_or_default(
        execution,
        "mode",
        "local",
        "execution.mode",
        &EXECUTION_MODES,
    )?;
    let execution_capabilities: BTreeSet<String> = ctx
        .optional_str_list(execution, "capabilities", "execution.capabilities")?
        .unwrap_or_default()
        .into_iter()
        .collect();
    if execution_mode == "distributed" && store_backend != "postgres" {
        return Err(ctx.err(format!(
            "execution.mode = 'distributed' requires store.backend = 'postgres', got store.backend = {}",
            py_str_repr(&store_backend)
        )));
    }

    let submit = ctx.section(data, "submit")?;
    let protected_paths = ctx
        .optional_str_list(submit, "protected_paths", "submit.protected_paths")?
        .unwrap_or_default();
    let submit_tiers = ctx.optional_submit_tiers(submit)?;
    let submit_strategy = ctx.choice_or_default(
        submit,
        "strategy",
        "merge",
        "submit.strategy",
        &SUBMIT_STRATEGIES,
    )?;
    let submit_remote = ctx.override_str(submit, "remote", "origin", "submit.remote", None)?;
    let submit_pr_base = ctx.optional_nonempty_str(submit, "pr_base", "submit.pr_base")?;
    let submit_base = ctx.optional_nonempty_str(submit, "base", "submit.base")?;
    let submit_verify = ctx.optional_nonempty_str(submit, "verify", "submit.verify")?;
    let (submit_recovery_agent_max_turns, submit_recovery_agent_max_wall_seconds) =
        ctx.optional_submit_recovery_agent(submit)?;

    // Team-mode guard (spec 00081, criterion 2): the merge lock is a flock on
    // a local path -- advisory, kernel-local, single-machine by construction.
    // Distributed execution with a landing that serializes through it would
    // let two machines silently race the base branch, so it is a load-time
    // configuration error. The distributed landing route is 'pr': GitHub's
    // merge queue and branch protection are the distributed merge lock.
    if execution_mode == "distributed" {
        if LOCAL_LOCK_STRATEGIES.contains(&submit_strategy.as_str()) {
            return Err(ctx.err(format!(
                "execution.mode = 'distributed' cannot use submit.strategy = {}: the \
                 {submit_strategy} strategy serializes landings through the single-machine \
                 merge lock (.flywheel/.merge.lock), which cannot span machines. Use \
                 submit.strategy = 'pr' with a merge queue (docs/team-mode.md), or \
                 execution.mode = 'local'",
                py_str_repr(&submit_strategy)
            )));
        }
        if !submit_tiers.is_empty() {
            return Err(ctx.err(
                "execution.mode = 'distributed' cannot use [[submit.tiers]]: tier routing \
                 lands tiers 0 and 1 through the single-machine merge lock \
                 (.flywheel/.merge.lock), which cannot span machines. Use \
                 submit.strategy = 'pr' with a merge queue (docs/team-mode.md), or \
                 execution.mode = 'local'",
            ));
        }
    }

    let sandbox = ctx.section(data, "sandbox")?;
    let sandbox_setup = ctx.optional_nonempty_str(sandbox, "setup", "sandbox.setup")?;
    let sandbox_policy = ctx.optional_sandbox_policy(sandbox)?;

    let phase = ctx.section(data, "phase")?;
    let phase_verify = ctx.optional_nonempty_str(phase, "verify", "phase.verify")?;

    let held_out = ctx.section(data, "held_out")?;
    let held_out_root = ctx
        .optional_nonempty_str(held_out, "root", "held_out.root")?
        .map(PathBuf::from);

    let deadlines = ctx.optional_deadlines(data.get("deadlines"))?;

    let autopilot = ctx.section(data, "autopilot")?;
    let (autopilot_target_depth, autopilot_landing, autopilot_interval_seconds, autopilot_weights) =
        ctx.optional_autopilot(autopilot)?;
    let (autopilot_authoring_concurrency, autopilot_discovery_cache_ttl_seconds) =
        ctx.optional_autopilot_cost_controls(autopilot)?;

    let triage = ctx.section(data, "triage")?;
    let (
        triage_intake_label,
        triage_ready_label,
        triage_needs_detail_label,
        triage_interval_seconds,
        triage_max_per_pass,
    ) = ctx.optional_triage(triage)?;

    let worker = ctx.section(data, "worker")?;
    let worker_concurrency = ctx.override_int(
        worker,
        "concurrency",
        DEFAULT_WORKER_CONCURRENCY,
        "worker.concurrency",
    )?;
    let worker_checkpoint_nudge_seconds = ctx.override_float(
        worker,
        "checkpoint_nudge_seconds",
        DEFAULT_CHECKPOINT_NUDGE_SECONDS,
        "worker.checkpoint_nudge_seconds",
    )?;
    let worker_session_pause_ceiling_seconds = ctx.override_float(
        worker,
        "session_pause_ceiling_seconds",
        DEFAULT_SESSION_PAUSE_CEILING_SECONDS,
        "worker.session_pause_ceiling_seconds",
    )?;

    let mut policy = WorkPolicy {
        source_kind: kind.to_string(),
        default_graders,
        db_path,
        sandbox_root,
        model,
        agent_id,
        agent_transport,
        store_backend,
        store_schema,
        execution_mode,
        execution_capabilities,
        worker_concurrency,
        worker_checkpoint_nudge_seconds,
        worker_session_pause_ceiling_seconds,
        protected_paths,
        submit_tiers,
        submit_strategy,
        submit_remote,
        submit_pr_base,
        submit_base,
        submit_verify,
        submit_recovery_agent_max_turns,
        submit_recovery_agent_max_wall_seconds,
        sandbox_setup,
        phase_verify,
        held_out_root,
        sandbox: sandbox_policy,
        deadlines,
        autopilot_target_depth,
        autopilot_landing,
        autopilot_interval_seconds,
        autopilot_weights,
        autopilot_authoring_concurrency,
        autopilot_discovery_cache_ttl_seconds,
        triage_intake_label,
        triage_ready_label,
        triage_needs_detail_label,
        triage_interval_seconds,
        triage_max_per_pass,
        ..WorkPolicy::default()
    };

    match kind {
        "directory" => {
            let tasks_dir = match source.get("tasks_dir") {
                None => None,
                Some(Value::String(dir)) => Some(dir.as_str()),
                Some(_) => return Err(ctx.err("source.tasks_dir must be a string")),
            };
            policy.tasks_dir = Some(PathBuf::from(match tasks_dir {
                Some(dir) if !dir.is_empty() => dir,
                _ => DEFAULT_TASKS_DIR,
            }));
        }
        "github_ci" => {
            policy.github_ci_repo = Some(ctx.required_repo(source, "github_ci")?);
            let failure_filter = match source.get("failure_filter") {
                None => "failure",
                Some(Value::String(filter)) if !filter.is_empty() => filter.as_str(),
                Some(_) => {
                    return Err(ctx.err("source.failure_filter must be a non-empty string"));
                }
            };
            policy.github_ci_failure_filter = failure_filter.to_string();
        }
        "github_review" => {
            policy.github_review_repo = Some(ctx.required_repo(source, "github_review")?);
        }
        _ => {
            let repo = ctx.required_repo(source, "github")?;
            let label = match source.get("label") {
                Some(Value::String(label)) if !label.is_empty() => label.clone(),
                _ => return Err(ctx.err("source.label is required for kind = \"github\"")),
            };
            let done_action = match source.get("done_action") {
                None => "comment".to_string(),
                Some(Value::String(action)) if DONE_ACTIONS.contains(&action.as_str()) => {
                    action.clone()
                }
                Some(other) => {
                    return Err(ctx.err(format!(
                        "source.done_action must be one of {}, got {}",
                        py_tuple(&DONE_ACTIONS),
                        py_repr(other)
                    )));
                }
            };
            policy.github_repo = Some(repo);
            policy.github_label = Some(label);
            policy.github_done_action = done_action;
            policy.github_require_triage_receipt = ctx.override_bool(
                source,
                "require_triage_receipt",
                false,
                "source.require_triage_receipt",
            )?;
        }
    }
    Ok(policy)
}

static EMPTY_TABLE: LazyLock<Table> = LazyLock::new(Table::new);

/// Per-load validation context: the file every diagnostic is attributed to.
struct Ctx<'a> {
    file: &'a Path,
}

impl Ctx<'_> {
    fn err(&self, message: impl std::fmt::Display) -> PolicyError {
        PolicyError::Invalid(format!("{}: {message}", self.file.display()))
    }

    /// An optional top-level section: the empty table when absent, an error
    /// naming the section when present but not a table.
    fn section<'t>(&self, data: &'t Table, name: &str) -> Result<&'t Table, PolicyError> {
        match data.get(name) {
            None => Ok(&EMPTY_TABLE),
            Some(Value::Table(table)) => Ok(table),
            Some(_) => Err(self.err(format!("[{name}] must be a table"))),
        }
    }

    /// A nested `[sandbox.*]` sub-table, or the empty table when absent. A
    /// present-but-non-table value fails fast so a scalar typed where a table
    /// belongs never silently degrades to defaults.
    fn subtable<'t>(
        &self,
        table: &'t Table,
        key: &str,
        path: &str,
    ) -> Result<&'t Table, PolicyError> {
        match table.get(key) {
            None => Ok(&EMPTY_TABLE),
            Some(Value::Table(sub)) => Ok(sub),
            Some(_) => Err(self.err(format!("[{path}] must be a table"))),
        }
    }

    fn optional_path(&self, table: &Table, key: &str) -> Result<Option<PathBuf>, PolicyError> {
        match table.get(key) {
            None => Ok(None),
            Some(Value::String(value)) if !value.is_empty() => Ok(Some(PathBuf::from(value))),
            Some(_) => Err(self.err(format!("paths.{key} must be a non-empty string"))),
        }
    }

    /// An optional string key that, when present, must be non-empty and not
    /// whitespace-only; absent means `None` so the consumer keeps its default.
    fn optional_nonempty_str(
        &self,
        table: &Table,
        key: &str,
        path: &str,
    ) -> Result<Option<String>, PolicyError> {
        match table.get(key) {
            None => Ok(None),
            Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
            Some(_) => Err(self.err(format!("{path} must be a non-empty string"))),
        }
    }

    /// An optional list of non-empty strings; absent means `None`.
    fn optional_str_list(
        &self,
        table: &Table,
        key: &str,
        path: &str,
    ) -> Result<Option<Vec<String>>, PolicyError> {
        match table.get(key) {
            None => Ok(None),
            Some(value) => match str_list(value) {
                Some(items) => Ok(Some(items)),
                None => Err(self.err(format!("{path} must be a list of non-empty strings"))),
            },
        }
    }

    /// A closed-vocabulary key whose absence means `default`. The value is
    /// matched as-is against `choices` (a non-string never matches).
    fn choice_or_default(
        &self,
        table: &Table,
        key: &str,
        default: &str,
        path: &str,
        choices: &[&str],
    ) -> Result<String, PolicyError> {
        match table.get(key) {
            None => Ok(default.to_string()),
            Some(Value::String(value)) if choices.contains(&value.as_str()) => Ok(value.clone()),
            Some(other) => Err(self.err(format!(
                "{path} must be one of {}, got {}",
                py_tuple(choices),
                py_repr(other)
            ))),
        }
    }

    fn required_repo(&self, source: &Table, kind: &str) -> Result<String, PolicyError> {
        match source.get("repo") {
            Some(Value::String(repo)) if !repo.is_empty() => Ok(repo.clone()),
            _ => Err(self.err(format!("source.repo is required for kind = \"{kind}\""))),
        }
    }

    fn override_bool(
        &self,
        table: &Table,
        key: &str,
        default: bool,
        path: &str,
    ) -> Result<bool, PolicyError> {
        match table.get(key) {
            None => Ok(default),
            Some(Value::Boolean(value)) => Ok(*value),
            Some(_) => Err(self.err(format!("{path} must be a boolean"))),
        }
    }

    /// An integer override. A TOML boolean or float is rejected so `= true`
    /// or `= 1.0` is a typo, never `1`.
    fn override_int(
        &self,
        table: &Table,
        key: &str,
        default: i64,
        path: &str,
    ) -> Result<i64, PolicyError> {
        match table.get(key) {
            None => Ok(default),
            Some(Value::Integer(value)) => Ok(*value),
            Some(_) => Err(self.err(format!("{path} must be an integer"))),
        }
    }

    /// A numeric override accepting an integer or a float; booleans rejected.
    fn override_float(
        &self,
        table: &Table,
        key: &str,
        default: f64,
        path: &str,
    ) -> Result<f64, PolicyError> {
        match table.get(key) {
            None => Ok(default),
            Some(value) => match as_number(value) {
                Some(number) => Ok(number),
                None => Err(self.err(format!("{path} must be a number"))),
            },
        }
    }

    /// A non-empty string override, optionally constrained to `choices`.
    fn override_str(
        &self,
        table: &Table,
        key: &str,
        default: &str,
        path: &str,
        choices: Option<&[&str]>,
    ) -> Result<String, PolicyError> {
        let Some(value) = table.get(key) else {
            return Ok(default.to_string());
        };
        let Value::String(value) = value else {
            return Err(self.err(format!("{path} must be a non-empty string")));
        };
        if value.trim().is_empty() {
            return Err(self.err(format!("{path} must be a non-empty string")));
        }
        if let Some(choices) = choices
            && !choices.contains(&value.as_str())
        {
            return Err(self.err(format!(
                "{path} must be one of {}, got {}",
                py_tuple(choices),
                py_str_repr(value)
            )));
        }
        Ok(value.clone())
    }

    fn override_str_list(
        &self,
        table: &Table,
        key: &str,
        default: Option<&[String]>,
        path: &str,
    ) -> Result<Option<Vec<String>>, PolicyError> {
        match self.optional_str_list(table, key, path)? {
            Some(items) => Ok(Some(items)),
            None => Ok(default.map(<[String]>::to_vec)),
        }
    }

    fn override_str_map(
        &self,
        table: &Table,
        key: &str,
        default: &BTreeMap<String, String>,
        path: &str,
    ) -> Result<BTreeMap<String, String>, PolicyError> {
        let Some(value) = table.get(key) else {
            return Ok(default.clone());
        };
        let mapped = value.as_table().and_then(|entries| {
            entries
                .iter()
                .map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<Option<BTreeMap<_, _>>>()
        });
        mapped.ok_or_else(|| self.err(format!("{path} must be a table of string values")))
    }

    /// Resolve `skills`: `"all"`/`"none"` (or any name) or a list.
    fn override_skills(
        &self,
        table: &Table,
        key: &str,
        default: &Skills,
        path: &str,
    ) -> Result<Skills, PolicyError> {
        match table.get(key) {
            None => Ok(default.clone()),
            Some(Value::String(name)) if !name.trim().is_empty() => Ok(Skills::Named(name.clone())),
            Some(value) => match str_list(value) {
                Some(items) => Ok(Skills::List(items)),
                None => Err(self.err(format!(
                    "{path} must be \"all\", \"none\", or a list of non-empty strings"
                ))),
            },
        }
    }

    /// An optional positive-integer override, `None` when absent so the
    /// consumer keeps its own default. A TOML boolean, a non-integer, or a
    /// non-positive value raises naming `path`.
    fn optional_positive_int(
        &self,
        table: &Table,
        key: &str,
        path: &str,
    ) -> Result<Option<i64>, PolicyError> {
        match table.get(key) {
            None => Ok(None),
            Some(Value::Integer(value)) if *value > 0 => Ok(Some(*value)),
            Some(_) => Err(self.err(format!("{path} must be a positive integer"))),
        }
    }

    /// Validate the optional `[[submit.tiers]]` rules (spec 00080).
    ///
    /// `()` when absent (or an explicit empty array). Each rule must be a
    /// table carrying exactly `tier` (an integer in [`SUBMIT_TIER_LEVELS`])
    /// and `paths` (a non-empty list of non-empty glob strings). Anything
    /// else -- a non-array value, a stray key, a bool-typed or out-of-range
    /// tier, a malformed paths list -- raises naming the offense.
    fn optional_submit_tiers(&self, table: &Table) -> Result<Vec<SubmitTierRule>, PolicyError> {
        let Some(value) = table.get("tiers") else {
            return Ok(Vec::new());
        };
        let items: Vec<&Table> = value
            .as_array()
            .and_then(|items| items.iter().map(Value::as_table).collect())
            .ok_or_else(|| {
                self.err(
                    "submit.tiers must be an array of tables ([[submit.tiers]] entries with \
                     'tier' and 'paths' keys)",
                )
            })?;
        let mut rules = Vec::with_capacity(items.len());
        for (index, item) in items.into_iter().enumerate() {
            let unknown: Vec<&String> = item
                .keys()
                .filter(|key| key.as_str() != "tier" && key.as_str() != "paths")
                .collect();
            if !unknown.is_empty() {
                let mut names: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
                names.sort_unstable();
                return Err(self.err(format!(
                    "submit.tiers[{index}] has unknown key(s) {}; each rule takes exactly \
                     'tier' and 'paths'",
                    py_list(&names)
                )));
            }
            let tier = match item.get("tier") {
                Some(Value::Integer(tier)) if SUBMIT_TIER_LEVELS.contains(tier) => *tier,
                other => {
                    return Err(self.err(format!(
                        "submit.tiers[{index}].tier must be one of {}, got {}",
                        py_int_tuple(&SUBMIT_TIER_LEVELS),
                        py_repr_opt(other)
                    )));
                }
            };
            let paths = item
                .get("paths")
                .and_then(str_list)
                .filter(|paths| !paths.is_empty())
                .ok_or_else(|| {
                    self.err(format!(
                        "submit.tiers[{index}].paths must be a non-empty list of non-empty strings"
                    ))
                })?;
            rules.push(SubmitTierRule { tier, paths });
        }
        Ok(rules)
    }

    /// Validate the merge strategy's bounded conflict-resolution session
    /// bounds: `(recovery_agent_max_turns, recovery_agent_max_wall_seconds)`.
    fn optional_submit_recovery_agent(&self, table: &Table) -> Result<(i64, f64), PolicyError> {
        let max_turns = match table.get("recovery_agent_max_turns") {
            None => DEFAULT_RECOVERY_AGENT_MAX_TURNS,
            Some(Value::Integer(turns)) if *turns >= 0 => *turns,
            Some(_) => {
                return Err(
                    self.err("submit.recovery_agent_max_turns must be a non-negative integer")
                );
            }
        };
        let max_wall = match table.get("recovery_agent_max_wall_seconds") {
            None => DEFAULT_RECOVERY_AGENT_MAX_WALL_SECONDS,
            Some(value) => match as_number(value) {
                Some(wall) if wall > 0.0 => wall,
                _ => {
                    return Err(self
                        .err("submit.recovery_agent_max_wall_seconds must be a positive number"));
                }
            },
        };
        Ok((max_turns, max_wall))
    }

    /// Resolve the optional top-level `[deadlines]` table (spec 00066) via
    /// the core resolver, so a key set to `0` opts that class out.
    fn optional_deadlines(&self, table: Option<&Value>) -> Result<DeadlineConfig, PolicyError> {
        let Some(table) = table else {
            return Ok(DeadlineConfig::default());
        };
        let Value::Table(table) = table else {
            return Err(self.err("[deadlines] must be a table"));
        };
        let json: serde_json::Map<String, serde_json::Value> = table
            .iter()
            .map(|(key, value)| (key.clone(), toml_to_json(value)))
            .collect();
        deadline_config_from_mapping(Some(&json)).map_err(|err| self.err(err))
    }

    /// Validate the optional `[autopilot]` table:
    /// `(target_depth, landing, interval_seconds, weights)`.
    fn optional_autopilot(
        &self,
        table: &Table,
    ) -> Result<(i64, String, f64, Option<AutopilotWeights>), PolicyError> {
        let target_depth = self.override_int(
            table,
            "target_depth",
            DEFAULT_AUTOPILOT_TARGET_DEPTH,
            "autopilot.target_depth",
        )?;
        if target_depth <= 0 {
            return Err(self.err("autopilot.target_depth must be a positive integer"));
        }
        let landing = self.override_str(
            table,
            "landing",
            DEFAULT_AUTOPILOT_LANDING,
            "autopilot.landing",
            Some(&AUTOPILOT_LANDINGS),
        )?;
        let interval = self.override_float(
            table,
            "interval_seconds",
            DEFAULT_AUTOPILOT_INTERVAL_SECONDS,
            "autopilot.interval_seconds",
        )?;
        if interval <= 0.0 {
            return Err(self.err("autopilot.interval_seconds must be positive"));
        }
        let Some(weights_value) = table.get("weights") else {
            return Ok((target_depth, landing, interval, None));
        };
        let Value::Table(weights_tbl) = weights_value else {
            return Err(self.err("[autopilot.weights] must be a table"));
        };
        let base = AutopilotWeights::default();
        let weight = |key: &str, default: f64| {
            self.override_float(
                weights_tbl,
                key,
                default,
                &format!("autopilot.weights.{key}"),
            )
        };
        let weights = AutopilotWeights {
            tier: weight("tier", base.tier)?,
            urgency: weight("urgency", base.urgency)?,
            importance: weight("importance", base.importance)?,
            unblock: weight("unblock", base.unblock)?,
            effort: weight("effort", base.effort)?,
            interrupt_base: weight("interrupt_base", base.interrupt_base)?,
        };
        Ok((target_depth, landing, interval, Some(weights)))
    }

    /// Validate the `[autopilot]` cycle-cost knobs:
    /// `(authoring_concurrency, discovery_cache_ttl_seconds)`.
    fn optional_autopilot_cost_controls(&self, table: &Table) -> Result<(i64, f64), PolicyError> {
        let concurrency = self.override_int(
            table,
            "authoring_concurrency",
            DEFAULT_AUTOPILOT_AUTHORING_CONCURRENCY,
            "autopilot.authoring_concurrency",
        )?;
        if concurrency <= 0 {
            return Err(self.err("autopilot.authoring_concurrency must be a positive integer"));
        }
        let ttl = self.override_float(
            table,
            "discovery_cache_ttl_seconds",
            DEFAULT_AUTOPILOT_DISCOVERY_CACHE_TTL_SECONDS,
            "autopilot.discovery_cache_ttl_seconds",
        )?;
        if !ttl.is_finite() || ttl < 0.0 {
            return Err(self.err("autopilot.discovery_cache_ttl_seconds must be >= 0"));
        }
        Ok((concurrency, ttl))
    }

    /// Validate the optional `[triage]` table: `(intake_label, ready_label,
    /// needs_detail_label, interval_seconds, max_per_pass)`.
    fn optional_triage(
        &self,
        table: &Table,
    ) -> Result<(String, String, String, f64, Option<i64>), PolicyError> {
        let intake_label = self.override_str(
            table,
            "intake_label",
            DEFAULT_TRIAGE_INTAKE_LABEL,
            "triage.intake_label",
            None,
        )?;
        let ready_label = self.override_str(
            table,
            "ready_label",
            DEFAULT_TRIAGE_READY_LABEL,
            "triage.ready_label",
            None,
        )?;
        let needs_detail_label = self.override_str(
            table,
            "needs_detail_label",
            DEFAULT_TRIAGE_NEEDS_DETAIL_LABEL,
            "triage.needs_detail_label",
            None,
        )?;
        let interval = self.override_float(
            table,
            "interval_seconds",
            DEFAULT_TRIAGE_INTERVAL_SECONDS,
            "triage.interval_seconds",
        )?;
        if interval <= 0.0 {
            return Err(self.err("triage.interval_seconds must be positive"));
        }
        if !table.contains_key("max_per_pass") {
            return Ok((
                intake_label,
                ready_label,
                needs_detail_label,
                interval,
                None,
            ));
        }
        let max_per_pass = self.override_int(table, "max_per_pass", 0, "triage.max_per_pass")?;
        if max_per_pass < 1 {
            return Err(self.err("triage.max_per_pass must be a positive integer"));
        }
        Ok((
            intake_label,
            ready_label,
            needs_detail_label,
            interval,
            Some(max_per_pass),
        ))
    }

    /// Resolve the `[sandbox.*]` tables into a [`SandboxPolicy`].
    ///
    /// Selects the named preset (default `fast`; an unknown name fails fast),
    /// then merges sparse per-key overrides from each sub-table onto that
    /// baseline with list-replace semantics. Absent keys (and unknown keys
    /// under any `[sandbox.*]` table) keep the preset value. The flat `setup`
    /// key is handled by the caller; this resolver ignores it.
    fn optional_sandbox_policy(&self, sandbox: &Table) -> Result<SandboxPolicy, PolicyError> {
        let (preset_name, base) = match sandbox.get("preset") {
            None => ("fast".to_string(), SandboxPolicy::default()),
            Some(value) => {
                let named = value
                    .as_str()
                    .and_then(|name| sandbox_preset(name).map(|base| (name.to_string(), base)));
                match named {
                    Some(named) => named,
                    None => {
                        return Err(self.err(format!(
                            "sandbox.preset {} is not available (defined presets: {})",
                            py_repr(value),
                            py_tuple(&SANDBOX_PRESET_NAMES)
                        )));
                    }
                }
            }
        };

        let backend = self.override_str(
            sandbox,
            "backend",
            &base.backend,
            "sandbox.backend",
            Some(&SANDBOX_BACKENDS),
        )?;
        let permission_mode = self.override_str(
            sandbox,
            "permission_mode",
            &base.permission_mode,
            "sandbox.permission_mode",
            None,
        )?;

        let exec_tbl = self.subtable(sandbox, "exec", "sandbox.exec")?;
        let exec = SandboxExec {
            enabled: self.override_bool(
                exec_tbl,
                "enabled",
                base.exec.enabled,
                "sandbox.exec.enabled",
            )?,
            auto_allow: self.override_bool(
                exec_tbl,
                "auto_allow",
                base.exec.auto_allow,
                "sandbox.exec.auto_allow",
            )?,
        };

        let cap_tbl = self.subtable(sandbox, "capabilities", "sandbox.capabilities")?;
        let mcp_tbl = self.subtable(cap_tbl, "mcp", "sandbox.capabilities.mcp")?;
        let capabilities = SandboxCapabilities {
            skills: self.override_skills(
                cap_tbl,
                "skills",
                &base.capabilities.skills,
                "sandbox.capabilities.skills",
            )?,
            allowed_tools: self
                .override_str_list(
                    cap_tbl,
                    "allowed_tools",
                    Some(&base.capabilities.allowed_tools),
                    "sandbox.capabilities.allowed_tools",
                )?
                .unwrap_or_default(),
            denied_tools: self
                .override_str_list(
                    cap_tbl,
                    "denied_tools",
                    Some(&base.capabilities.denied_tools),
                    "sandbox.capabilities.denied_tools",
                )?
                .unwrap_or_default(),
            setting_sources: self.override_str_list(
                cap_tbl,
                "setting_sources",
                base.capabilities.setting_sources.as_deref(),
                "sandbox.capabilities.setting_sources",
            )?,
            mcp_servers: self
                .override_str_list(
                    mcp_tbl,
                    "servers",
                    Some(&base.capabilities.mcp_servers),
                    "sandbox.capabilities.mcp.servers",
                )?
                .unwrap_or_default(),
            mcp_strict: self.override_bool(
                mcp_tbl,
                "strict",
                base.capabilities.mcp_strict,
                "sandbox.capabilities.mcp.strict",
            )?,
        };

        let net_tbl = self.subtable(sandbox, "network", "sandbox.network")?;
        let network = SandboxNetwork {
            policy: self.override_str(
                net_tbl,
                "policy",
                &base.network.policy,
                "sandbox.network.policy",
                Some(&NETWORK_POLICIES),
            )?,
            allow_hosts: self
                .override_str_list(
                    net_tbl,
                    "allow_hosts",
                    Some(&base.network.allow_hosts),
                    "sandbox.network.allow_hosts",
                )?
                .unwrap_or_default(),
            allow_unix_sockets: self
                .override_str_list(
                    net_tbl,
                    "allow_unix_sockets",
                    Some(&base.network.allow_unix_sockets),
                    "sandbox.network.allow_unix_sockets",
                )?
                .unwrap_or_default(),
        };

        let env_tbl = self.subtable(sandbox, "env", "sandbox.env")?;
        let env = SandboxEnv {
            passthrough: self
                .override_str_list(
                    env_tbl,
                    "pass",
                    Some(&base.env.passthrough),
                    "sandbox.env.pass",
                )?
                .unwrap_or_default(),
            set_values: self.override_str_map(
                env_tbl,
                "set",
                &base.env.set_values,
                "sandbox.env.set",
            )?,
            inherit_home: self.override_bool(
                env_tbl,
                "inherit_home",
                base.env.inherit_home,
                "sandbox.env.inherit_home",
            )?,
        };

        let limits_tbl = self.subtable(sandbox, "limits", "sandbox.limits")?;
        let limits = SandboxLimits {
            max_turns: self.override_int(
                limits_tbl,
                "max_turns",
                base.limits.max_turns,
                "sandbox.limits.max_turns",
            )?,
            max_retries: self.override_int(
                limits_tbl,
                "max_retries",
                base.limits.max_retries,
                "sandbox.limits.max_retries",
            )?,
            lease_seconds: self.override_int(
                limits_tbl,
                "lease_seconds",
                base.limits.lease_seconds,
                "sandbox.limits.lease_seconds",
            )?,
            wall_clock_seconds: self.override_int(
                limits_tbl,
                "wall_clock_seconds",
                base.limits.wall_clock_seconds,
                "sandbox.limits.wall_clock_seconds",
            )?,
            max_cost_usd: self.override_float(
                limits_tbl,
                "max_cost_usd",
                base.limits.max_cost_usd,
                "sandbox.limits.max_cost_usd",
            )?,
            max_tokens: self.override_int(
                limits_tbl,
                "max_tokens",
                base.limits.max_tokens,
                "sandbox.limits.max_tokens",
            )?,
            rubric_judge_max_turns: self.optional_positive_int(
                limits_tbl,
                "rubric_judge_max_turns",
                "sandbox.limits.rubric_judge_max_turns",
            )?,
        };

        let retention_tbl = self.subtable(sandbox, "retention", "sandbox.retention")?;
        let retention = SandboxRetention {
            on_done: self.override_str(
                retention_tbl,
                "on_done",
                &base.retention.on_done,
                "sandbox.retention.on_done",
                Some(&RETENTION_ON_DONE),
            )?,
            on_failure: self.override_str(
                retention_tbl,
                "on_failure",
                &base.retention.on_failure,
                "sandbox.retention.on_failure",
                Some(&RETENTION_ON_FAILURE),
            )?,
            sweep_days: self.override_int(
                retention_tbl,
                "sweep_days",
                base.retention.sweep_days,
                "sandbox.retention.sweep_days",
            )?,
        };

        let container_tbl = self.subtable(sandbox, "container", "sandbox.container")?;
        let container = SandboxContainer {
            image: self.override_str(
                container_tbl,
                "image",
                &base.container.image,
                "sandbox.container.image",
                None,
            )?,
            model: self.override_str(
                container_tbl,
                "model",
                &base.container.model,
                "sandbox.container.model",
                None,
            )?,
            auth: self.override_str(
                container_tbl,
                "auth",
                &base.container.auth,
                "sandbox.container.auth",
                Some(&CONTAINER_AUTH_MODES),
            )?,
            auth_env: self.override_str(
                container_tbl,
                "auth_env",
                &base.container.auth_env,
                "sandbox.container.auth_env",
                None,
            )?,
            exec_timeout: self.override_int(
                container_tbl,
                "exec_timeout",
                base.container.exec_timeout,
                "sandbox.container.exec_timeout",
            )?,
            egress_network: self.override_str(
                container_tbl,
                "egress_network",
                &base.container.egress_network,
                "sandbox.container.egress_network",
                None,
            )?,
        };
        if backend == "container" && container.image.is_empty() {
            return Err(self
                .err("[sandbox.container] image is required when sandbox.backend = 'container'"));
        }

        Ok(SandboxPolicy {
            preset: preset_name,
            backend,
            permission_mode,
            exec,
            capabilities,
            network,
            env,
            limits,
            retention,
            container,
        })
    }
}

// --- Value helpers -------------------------------------------------------------

/// An array of non-empty, non-whitespace strings, or `None` for anything else.
fn str_list(value: &Value) -> Option<Vec<String>> {
    value.as_array().and_then(|items| {
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            })
            .collect()
    })
}

/// An integer or float as `f64`; booleans and everything else are `None`.
fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Convert a TOML value into the JSON shape the core loaders consume.
fn toml_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Integer(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        Value::Array(items) => serde_json::Value::Array(items.iter().map(toml_to_json).collect()),
        Value::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

/// Python `repr()` of a string: single-quoted with backslash/quote escaping.
fn py_str_repr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// Python `repr()` of a float: always carries a decimal point when integral.
fn py_float_repr(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{f:.1}")
    } else if f.is_nan() {
        "nan".to_string()
    } else if f.is_infinite() {
        if f > 0.0 { "inf" } else { "-inf" }.to_string()
    } else {
        format!("{f}")
    }
}

/// Python `repr()` of a decoded TOML value, so `got {value!r}` diagnostics
/// read identically to the Python module's.
fn py_repr(value: &Value) -> String {
    match value {
        Value::String(s) => py_str_repr(s),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => py_float_repr(*f),
        Value::Boolean(true) => "True".to_string(),
        Value::Boolean(false) => "False".to_string(),
        Value::Datetime(dt) => py_str_repr(&dt.to_string()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Table(table) => {
            let mut out = String::from("{");
            for (i, (k, v)) in table.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{}: {}", py_str_repr(k), py_repr(v));
            }
            out.push('}');
            out
        }
    }
}

fn py_repr_opt(value: Option<&Value>) -> String {
    value.map(py_repr).unwrap_or_else(|| "None".to_string())
}

/// Python `repr()` of a tuple of strings: `('a', 'b')`.
fn py_tuple(items: &[&str]) -> String {
    let parts: Vec<String> = items.iter().map(|s| py_str_repr(s)).collect();
    match parts.len() {
        1 => format!("({},)", parts[0]),
        _ => format!("({})", parts.join(", ")),
    }
}

/// Python `repr()` of a tuple of ints: `(0, 1, 2)`.
fn py_int_tuple(items: &[i64]) -> String {
    let parts: Vec<String> = items.iter().map(i64::to_string).collect();
    match parts.len() {
        1 => format!("({},)", parts[0]),
        _ => format!("({})", parts.join(", ")),
    }
}

/// Python `repr()` of a list of strings: `['a', 'b']`.
fn py_list(items: &[&str]) -> String {
    let parts: Vec<String> = items.iter().map(|s| py_str_repr(s)).collect();
    format!("[{}]", parts.join(", "))
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

/// Resolve `[paths] sandbox_root` to the absolute worktree root.
///
/// `None` (unset) keeps the built-in nested default
/// `<repo_root>/.flywheel/worktrees`. A relative path anchors at `repo_root`
/// -- never the process cwd -- so the same policy file means the same
/// location no matter where the CLI is invoked from. An absolute path is
/// used verbatim. Two tokens opt into out-of-tree layouts:
///
/// * `@cache` -- `<cache-base>/flywheel/<repo-name>-<id>/worktrees`, where
///   `<cache-base>` is the first writable of `$XDG_CACHE_HOME`, `~/.cache`,
///   and the platform tmpdir, and `<id>` keys the repo by the realpath of its
///   git common dir (two clones never collide; a linked-worktree checkout
///   maps to its main repo).
/// * `@sibling` -- `<repo-parent>/<repo-name>.worktrees`, refused with
///   [`PolicyError`] when the parent directory is not writable (read-only CI
///   mounts), rather than failing later mid-task.
pub fn resolve_sandbox_root(
    configured: Option<&Path>,
    repo_root: &Path,
) -> Result<PathBuf, PolicyError> {
    let Some(configured) = configured else {
        return Ok(resolve_nonstrict(
            &repo_root.join(".flywheel").join("worktrees"),
        ));
    };
    let raw = configured.to_string_lossy();
    if raw == SANDBOX_ROOT_CACHE_TOKEN {
        return cache_sandbox_root(repo_root);
    }
    if raw == SANDBOX_ROOT_SIBLING_TOKEN {
        return sibling_sandbox_root(repo_root);
    }
    if raw.starts_with('@') {
        return Err(PolicyError::Invalid(format!(
            "paths.sandbox_root: unknown token {} (supported: {SANDBOX_ROOT_CACHE_TOKEN}, \
             {SANDBOX_ROOT_SIBLING_TOKEN})",
            py_str_repr(&raw)
        )));
    }
    if configured.is_absolute() {
        return Ok(configured.to_path_buf());
    }
    Ok(resolve_nonstrict(&repo_root.join(configured)))
}

fn sibling_sandbox_root(repo_root: &Path) -> Result<PathBuf, PolicyError> {
    let root = resolve_nonstrict(repo_root);
    let Some(parent) = root.parent().filter(|parent| *parent != root) else {
        return Err(PolicyError::Invalid(format!(
            "paths.sandbox_root: @sibling needs a parent directory, but {} is the filesystem root",
            root.display()
        )));
    };
    if !dir_is_writable(parent) {
        return Err(PolicyError::Invalid(format!(
            "paths.sandbox_root: @sibling requires a writable parent directory, but {} is not \
             writable (read-only mount?); use @cache or a literal path instead",
            parent.display()
        )));
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(parent.join(format!("{name}.worktrees")))
}

fn cache_sandbox_root(repo_root: &Path) -> Result<PathBuf, PolicyError> {
    let base = first_writable_cache_base()?;
    let identity = repo_identity(repo_root);
    let digest = hex::encode(Sha256::digest(identity.to_string_lossy().as_bytes()));
    let resolved = resolve_nonstrict(repo_root);
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "repo".to_string());
    Ok(base
        .join("flywheel")
        .join(format!("{name}-{}", &digest[..12]))
        .join("worktrees"))
}

fn first_writable_cache_base() -> Result<PathBuf, PolicyError> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(xdg));
    }
    if let Some(home) = std::env::home_dir() {
        candidates.push(home.join(".cache"));
    }
    candidates.push(std::env::temp_dir());
    for candidate in &candidates {
        if fs::create_dir_all(candidate).is_err() {
            continue;
        }
        if dir_is_writable(candidate) {
            return Ok(candidate.clone());
        }
    }
    let tried: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    Err(PolicyError::Invalid(format!(
        "paths.sandbox_root: @cache found no writable cache directory (tried {})",
        tried.join(", ")
    )))
}

/// The path that names this repo for `@cache` keying.
///
/// The realpath of the git common dir, so every checkout of one repo --
/// including linked worktrees -- shares a cache dir, while two clones at
/// different paths never collide. Outside a git repo the realpath of
/// `repo_root` itself is the identity.
fn repo_identity(repo_root: &Path) -> PathBuf {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--git-common-dir"])
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let common = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !common.is_empty() {
            let common = PathBuf::from(common);
            let common = if common.is_absolute() {
                common
            } else {
                repo_root.join(common)
            };
            return resolve_nonstrict(&common);
        }
    }
    resolve_nonstrict(repo_root)
}

/// Whether the current process can create entries in `dir`: a real write
/// probe (create then remove a uniquely named file), which is the only
/// portable answer that also respects read-only mounts and ACLs.
fn dir_is_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(
        ".flywheel-write-probe-{}-{nanos}",
        std::process::id()
    ));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
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
    use serde_json::json;

    const FILE: &str = "/repo/flywheel.toml";

    fn load(body: &str) -> Result<WorkPolicy, PolicyError> {
        load_policy_str(body, Path::new(FILE))
    }

    fn load_dir(body: &str) -> Result<WorkPolicy, PolicyError> {
        load(&format!("[source]\nkind = \"directory\"\n{body}"))
    }

    fn ok_dir(body: &str) -> WorkPolicy {
        load_dir(body).expect("policy loads")
    }

    fn err_dir(body: &str) -> String {
        match load_dir(body) {
            Ok(policy) => panic!("expected PolicyError, loaded {policy:?}"),
            Err(err) => err.to_string(),
        }
    }

    /// Like `err_dir` but with `body` placed BEFORE `[source]`, for top-level
    /// scalars typed where a section table belongs.
    fn err_top(body: &str) -> String {
        match load(&format!("{body}[source]\nkind = \"directory\"\n")) {
            Ok(policy) => panic!("expected PolicyError, loaded {policy:?}"),
            Err(err) => err.to_string(),
        }
    }

    const GITHUB_HEAD: &str =
        "[source]\nkind = \"github\"\nrepo = \"octo/widgets\"\nlabel = \"flywheel\"\n";

    fn ok_github(body: &str) -> WorkPolicy {
        load(&format!("{GITHUB_HEAD}{body}")).expect("github policy loads")
    }

    // --- source ------------------------------------------------------------

    #[test]
    fn directory_policy_defaults_tasks_dir() {
        let policy = ok_dir("");
        assert_eq!(policy.source_kind, "directory");
        assert_eq!(policy.tasks_dir, Some(PathBuf::from(".flywheel/tasks")));
        assert!(policy.default_graders.is_empty());
    }

    #[test]
    fn directory_policy_explicit_tasks_dir() {
        let policy = ok_dir("tasks_dir = \"work/items\"\n");
        assert_eq!(policy.tasks_dir, Some(PathBuf::from("work/items")));
        assert_eq!(
            err_dir("tasks_dir = 5\n"),
            format!("{FILE}: source.tasks_dir must be a string")
        );
    }

    #[test]
    fn github_policy_parses() {
        let policy = ok_github("done_action = \"close\"\nrequire_triage_receipt = true\n");
        assert_eq!(policy.source_kind, "github");
        assert_eq!(policy.github_repo.as_deref(), Some("octo/widgets"));
        assert_eq!(policy.github_label.as_deref(), Some("flywheel"));
        assert_eq!(policy.github_done_action, "close");
        assert!(policy.github_require_triage_receipt);
        assert_eq!(policy.tasks_dir, None);
        let plain = ok_github("");
        assert_eq!(plain.github_done_action, "comment");
        assert!(!plain.github_require_triage_receipt);
    }

    #[test]
    fn github_requires_repo_and_label() {
        let no_repo = load("[source]\nkind = \"github\"\nlabel = \"x\"\n").unwrap_err();
        assert_eq!(
            no_repo.to_string(),
            format!("{FILE}: source.repo is required for kind = \"github\"")
        );
        let no_label = load("[source]\nkind = \"github\"\nrepo = \"a/b\"\n").unwrap_err();
        assert_eq!(
            no_label.to_string(),
            format!("{FILE}: source.label is required for kind = \"github\"")
        );
    }

    #[test]
    fn bad_done_action_raises() {
        let err = load(&format!("{GITHUB_HEAD}done_action = \"delete\"\n")).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("{FILE}: source.done_action must be one of ('comment', 'close'), got 'delete'")
        );
        let err = load(&format!("{GITHUB_HEAD}require_triage_receipt = 1\n")).unwrap_err();
        assert!(
            err.to_string()
                .contains("source.require_triage_receipt must be a boolean")
        );
    }

    #[test]
    fn github_ci_requires_repo_and_defaults_filter() {
        let err = load("[source]\nkind = \"github_ci\"\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("source.repo is required for kind = \"github_ci\"")
        );
        let policy = load("[source]\nkind = \"github_ci\"\nrepo = \"a/b\"\n").unwrap();
        assert_eq!(policy.github_ci_repo.as_deref(), Some("a/b"));
        assert_eq!(policy.github_ci_failure_filter, "failure");
        let policy = load(
            "[source]\nkind = \"github_ci\"\nrepo = \"a/b\"\nfailure_filter = \"timed_out\"\n",
        )
        .unwrap();
        assert_eq!(policy.github_ci_failure_filter, "timed_out");
        let err = load("[source]\nkind = \"github_ci\"\nrepo = \"a/b\"\nfailure_filter = \"\"\n")
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("source.failure_filter must be a non-empty string")
        );
    }

    #[test]
    fn github_review_requires_repo() {
        let err = load("[source]\nkind = \"github_review\"\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("source.repo is required for kind = \"github_review\"")
        );
        let policy = load("[source]\nkind = \"github_review\"\nrepo = \"a/b\"\n").unwrap();
        assert_eq!(policy.source_kind, "github_review");
        assert_eq!(policy.github_review_repo.as_deref(), Some("a/b"));
    }

    #[test]
    fn missing_source_table_and_unknown_kind() {
        assert_eq!(
            load("[paths]\ndb = \"x\"\n").unwrap_err().to_string(),
            format!("{FILE}: missing required [source] table")
        );
        assert_eq!(
            load("[source]\nkind = \"jira\"\n").unwrap_err().to_string(),
            format!(
                "{FILE}: source.kind must be one of ('directory', 'github', 'github_ci', \
                 'github_review'), got 'jira'"
            )
        );
        assert_eq!(
            load("[source]\n").unwrap_err().to_string(),
            format!(
                "{FILE}: source.kind must be one of ('directory', 'github', 'github_ci', \
                 'github_review'), got None"
            )
        );
    }

    #[test]
    fn invalid_toml_and_missing_file() {
        let err = load("[source\nkind = \"directory\"\n").unwrap_err();
        assert!(matches!(err, PolicyError::InvalidToml { .. }));
        assert!(
            err.to_string()
                .starts_with(&format!("{FILE}: invalid TOML: "))
        );

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        let err = load_policy(&missing).unwrap_err();
        assert!(matches!(err, PolicyError::Read { .. }));
        assert!(
            err.to_string()
                .starts_with(&format!("{}: cannot read policy file: ", missing.display()))
        );
    }

    #[test]
    fn load_policy_accepts_a_repo_root_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(DEFAULT_POLICY_FILENAME),
            "[source]\nkind = \"directory\"\n[worker]\nconcurrency = 4\n",
        )
        .unwrap();
        let policy = load_policy(dir.path()).unwrap();
        assert_eq!(policy.worker_concurrency, 4);
        let file_path = load_policy(dir.path().join(DEFAULT_POLICY_FILENAME)).unwrap();
        assert_eq!(policy, file_path);
    }

    // --- defaults.graders ------------------------------------------------------

    #[test]
    fn default_graders_parse_with_real_validation() {
        let policy = ok_dir("[[defaults.graders]]\ntype = \"command\"\nrun = \"uv run pytest\"\n");
        assert_eq!(policy.default_graders.len(), 1);
        match &policy.default_graders[0] {
            Grader::Command(grader) => assert_eq!(grader.run, "uv run pytest"),
            other => panic!("unexpected grader {other:?}"),
        }
        let err = err_dir("[[defaults.graders]]\ntype = \"telepathy\"\n");
        assert!(err.starts_with(&format!("{FILE}: defaults")), "{err}");
        assert!(err.contains("unknown type"), "{err}");
        assert!(err_dir("[defaults]\ngraders = \"x\"\n").contains("'graders' must be a list"));
        assert_eq!(
            err_top("defaults = 5\n"),
            format!("{FILE}: [defaults] must be a table")
        );
    }

    // --- paths ---------------------------------------------------------------

    #[test]
    fn paths_table_parses_and_defaults_to_none() {
        let policy = ok_dir("[paths]\ndb = \"state/fw.sqlite\"\nsandbox_root = \"@cache\"\n");
        assert_eq!(policy.db_path, Some(PathBuf::from("state/fw.sqlite")));
        assert_eq!(policy.sandbox_root, Some(PathBuf::from("@cache")));
        let unset = ok_dir("");
        assert_eq!(unset.db_path, None);
        assert_eq!(unset.sandbox_root, None);
        assert_eq!(
            err_dir("[paths]\ndb = 3\n"),
            format!("{FILE}: paths.db must be a non-empty string")
        );
        assert!(err_dir("[paths]\nsandbox_root = \"\"\n").contains("paths.sandbox_root"));
    }

    // --- agent ---------------------------------------------------------------

    #[test]
    fn agent_table_parses_and_validates() {
        assert_eq!(ok_dir("").model, None);
        assert_eq!(ok_dir("[agent]\n").model, None);
        let policy = ok_dir(
            "[agent]\nmodel = \"claude-opus-4-8\"\nid = \"codex\"\ntransport = \"cli\"\nfuture = 1\n",
        );
        assert_eq!(policy.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(policy.agent_id.as_deref(), Some("codex"));
        assert_eq!(policy.agent_transport.as_deref(), Some("cli"));
        assert_eq!(
            err_dir("[agent]\nmodel = 5\n"),
            format!("{FILE}: agent.model must be a non-empty string")
        );
        assert!(
            err_dir("[agent]\nmodel = \"   \"\n")
                .contains("agent.model must be a non-empty string")
        );
        assert!(err_dir("[agent]\nid = \"\"\n").contains("agent.id must be a non-empty string"));
        assert_eq!(
            err_dir("[agent]\nid = \"claude-code\"\ntransport = \"sdk\"\n"),
            format!(
                "{FILE}: agent.transport must be one of ('cli',), got 'sdk'; the Python-era \
                 \"sdk\" transport is gone (the cli transport speaks the same stream-json \
                 control protocol) -- set transport = \"cli\" or remove the key"
            )
        );
        assert!(
            err_dir("[agent]\nid = \"codex\"\ntransport = \"grpc\"\n")
                .contains("agent.transport must be one of ('cli',), got 'grpc'")
        );
        assert_eq!(
            err_dir("[agent]\ntransport = \"cli\"\n"),
            format!(
                "{FILE}: agent.transport requires agent.id (the multi-agent opt-in; see \
                 docs/agent-harness.md)"
            )
        );
        assert_eq!(
            err_top("agent = \"x\"\n"),
            format!("{FILE}: [agent] must be a table")
        );
        // A falsy scalar where a table belongs is a typo too (the Python
        // loader silently accepted `agent = false`).
        assert_eq!(
            err_top("agent = false\n"),
            format!("{FILE}: [agent] must be a table")
        );
    }

    // --- store / execution -------------------------------------------------------

    #[test]
    fn store_table_parses_and_validates() {
        let absent = ok_dir("");
        assert_eq!(absent.store_backend, "sqlite");
        assert_eq!(absent.store_schema, None);
        let policy = ok_dir("[store]\nbackend = \"postgres\"\nschema = \"fw_ci\"\n");
        assert_eq!(policy.store_backend, "postgres");
        assert_eq!(policy.store_schema.as_deref(), Some("fw_ci"));
        assert_eq!(
            err_dir("[store]\nbackend = \"mysql\"\n"),
            format!("{FILE}: store.backend must be one of ('sqlite', 'postgres'), got 'mysql'")
        );
        assert!(err_dir("[store]\nbackend = 1\n").contains("store.backend must be one of"));
        assert!(
            err_dir("[store]\nschema = \" \"\n")
                .contains("store.schema must be a non-empty string")
        );
        assert!(err_dir("[store]\nschema = 7\n").contains("store.schema"));
        assert_eq!(
            err_top("store = \"x\"\n"),
            format!("{FILE}: [store] must be a table")
        );
    }

    #[test]
    fn execution_table_parses_and_validates() {
        let absent = ok_dir("");
        assert_eq!(absent.execution_mode, "local");
        assert!(absent.execution_capabilities.is_empty());
        let policy = ok_dir(
            "[store]\nbackend = \"postgres\"\n[execution]\nmode = \"distributed\"\ncapabilities = [\"gpu\", \"cuda\"]\n[submit]\nstrategy = \"pr\"\n",
        );
        assert_eq!(policy.execution_mode, "distributed");
        assert_eq!(
            policy.execution_capabilities,
            BTreeSet::from(["cuda".to_string(), "gpu".to_string()])
        );
        assert_eq!(
            err_dir("[execution]\nmode = \"cluster\"\n"),
            format!(
                "{FILE}: execution.mode must be one of ('local', 'distributed'), got 'cluster'"
            )
        );
        assert_eq!(
            err_dir("[execution]\ncapabilities = [\"gpu\", \"\"]\n"),
            format!("{FILE}: execution.capabilities must be a list of non-empty strings")
        );
        assert_eq!(
            err_dir("[execution]\nmode = \"distributed\"\n[submit]\nstrategy = \"pr\"\n"),
            format!(
                "{FILE}: execution.mode = 'distributed' requires store.backend = 'postgres', \
                 got store.backend = 'sqlite'"
            )
        );
    }

    // --- distributed mode vs the local merge lock (spec 00081) ----------------------

    fn distributed(submit_lines: &str) -> Result<WorkPolicy, PolicyError> {
        load_dir(&format!(
            "[store]\nbackend = \"postgres\"\n[execution]\nmode = \"distributed\"\n{submit_lines}"
        ))
    }

    #[test]
    fn distributed_mode_rejects_local_lock_strategies_and_tiers() {
        let merge = distributed("[submit]\nstrategy = \"merge\"\n")
            .unwrap_err()
            .to_string();
        assert!(merge.contains("distributed"));
        assert!(merge.contains("submit.strategy = 'merge'"));
        assert!(merge.contains(".flywheel/.merge.lock"));
        assert!(
            distributed("")
                .unwrap_err()
                .to_string()
                .contains("submit.strategy")
        );
        assert!(
            distributed("[submit]\nstrategy = \"phase\"\n")
                .unwrap_err()
                .to_string()
                .contains("the phase strategy serializes landings")
        );
        let tiers = distributed(
            "[submit]\nstrategy = \"pr\"\n[[submit.tiers]]\ntier = 0\npaths = [\"docs/**\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(tiers.contains("cannot use [[submit.tiers]]"));
        let pr = distributed("[submit]\nstrategy = \"pr\"\n").unwrap();
        assert_eq!(pr.execution_mode, "distributed");
        assert_eq!(pr.submit_strategy, "pr");
        let local = ok_dir("[submit]\nstrategy = \"merge\"\n");
        assert_eq!(local.execution_mode, "local");
        assert_eq!(local.submit_strategy, "merge");
    }

    // --- worker --------------------------------------------------------------

    #[test]
    fn worker_table_parses_and_validates() {
        let absent = ok_dir("");
        assert_eq!(absent.worker_concurrency, 1);
        assert_eq!(absent.worker_checkpoint_nudge_seconds, 300.0);
        assert_eq!(absent.worker_session_pause_ceiling_seconds, 21600.0);
        let policy = ok_dir(
            "[worker]\nconcurrency = 3\ncheckpoint_nudge_seconds = 45\nsession_pause_ceiling_seconds = 0\n",
        );
        assert_eq!(policy.worker_concurrency, 3);
        assert_eq!(policy.worker_checkpoint_nudge_seconds, 45.0);
        assert_eq!(policy.worker_session_pause_ceiling_seconds, 0.0);
        // Sub-1 concurrency is deliberately legal at load (the flag overrides).
        assert_eq!(ok_dir("[worker]\nconcurrency = 0\n").worker_concurrency, 0);
        assert_eq!(
            err_dir("[worker]\nconcurrency = \"three\"\n"),
            format!("{FILE}: worker.concurrency must be an integer")
        );
        assert!(
            err_dir("[worker]\nconcurrency = true\n")
                .contains("worker.concurrency must be an integer")
        );
        assert!(
            err_dir("[worker]\nconcurrency = 2.0\n")
                .contains("worker.concurrency must be an integer")
        );
        assert_eq!(
            err_dir("[worker]\ncheckpoint_nudge_seconds = false\n"),
            format!("{FILE}: worker.checkpoint_nudge_seconds must be a number")
        );
        assert!(
            err_dir("[worker]\nsession_pause_ceiling_seconds = \"x\"\n")
                .contains("worker.session_pause_ceiling_seconds must be a number")
        );
        assert_eq!(
            err_top("worker = 1\n"),
            format!("{FILE}: [worker] must be a table")
        );
    }

    // --- submit --------------------------------------------------------------

    #[test]
    fn submit_defaults_when_table_absent() {
        let policy = ok_dir("");
        assert!(policy.protected_paths.is_empty());
        assert!(policy.submit_tiers.is_empty());
        assert_eq!(policy.submit_strategy, "merge");
        assert_eq!(policy.submit_remote, "origin");
        assert_eq!(policy.submit_pr_base, None);
        assert_eq!(policy.submit_base, None);
        assert_eq!(policy.submit_verify, None);
        assert_eq!(policy.submit_recovery_agent_max_turns, 30);
        assert_eq!(policy.submit_recovery_agent_max_wall_seconds, 900.0);
    }

    #[test]
    fn submit_table_parses() {
        let policy = ok_dir(
            "[submit]\nstrategy = \"pr\"\nremote = \"upstream\"\npr_base = \"main\"\nbase = \"integration\"\nverify = \"make check\"\nprotected_paths = [\".github/**\", \"flywheel.toml\"]\nrecovery_agent_max_turns = 0\nrecovery_agent_max_wall_seconds = 60\n",
        );
        assert_eq!(policy.submit_strategy, "pr");
        assert_eq!(policy.submit_remote, "upstream");
        assert_eq!(policy.submit_pr_base.as_deref(), Some("main"));
        assert_eq!(policy.submit_base.as_deref(), Some("integration"));
        assert_eq!(policy.submit_verify.as_deref(), Some("make check"));
        assert_eq!(policy.protected_paths, vec![".github/**", "flywheel.toml"]);
        assert_eq!(policy.submit_recovery_agent_max_turns, 0);
        assert_eq!(policy.submit_recovery_agent_max_wall_seconds, 60.0);
    }

    #[test]
    fn submit_table_validates() {
        assert_eq!(
            err_dir("[submit]\nstrategy = \"rebase\"\n"),
            format!(
                "{FILE}: submit.strategy must be one of ('merge', 'pr', 'phase'), got 'rebase'"
            )
        );
        // "tiered" is the tier router's internal registry name, selected only
        // by configuring [[submit.tiers]]; it is not a [submit] strategy.
        assert_eq!(
            err_dir("[submit]\nstrategy = \"tiered\"\n"),
            format!(
                "{FILE}: submit.strategy must be one of ('merge', 'pr', 'phase'), got 'tiered'"
            )
        );
        assert!(
            err_dir("[submit]\nremote = \"\"\n")
                .contains("submit.remote must be a non-empty string")
        );
        assert!(
            err_dir("[submit]\npr_base = 1\n")
                .contains("submit.pr_base must be a non-empty string")
        );
        assert!(
            err_dir("[submit]\nbase = \"  \"\n").contains("submit.base must be a non-empty string")
        );
        assert!(
            err_dir("[submit]\nverify = 3\n").contains("submit.verify must be a non-empty string")
        );
        assert_eq!(
            err_dir("[submit]\nprotected_paths = \"x\"\n"),
            format!("{FILE}: submit.protected_paths must be a list of non-empty strings")
        );
        assert!(
            err_dir("[submit]\nprotected_paths = [\"a\", \" \"]\n").contains("protected_paths")
        );
        for bad in ["-1", "true", "2.5"] {
            assert_eq!(
                err_dir(&format!("[submit]\nrecovery_agent_max_turns = {bad}\n")),
                format!("{FILE}: submit.recovery_agent_max_turns must be a non-negative integer")
            );
        }
        for bad in ["0", "-3", "true"] {
            assert_eq!(
                err_dir(&format!(
                    "[submit]\nrecovery_agent_max_wall_seconds = {bad}\n"
                )),
                format!("{FILE}: submit.recovery_agent_max_wall_seconds must be a positive number")
            );
        }
        assert_eq!(
            ok_dir("[submit]\nrecovery_agent_max_wall_seconds = 12.5\n")
                .submit_recovery_agent_max_wall_seconds,
            12.5
        );
    }

    #[test]
    fn submit_tiers_parse_and_validate() {
        let policy = ok_dir(
            "[[submit.tiers]]\ntier = 0\npaths = [\"docs/**\", \"*.md\"]\n[[submit.tiers]]\ntier = 2\npaths = [\"scripts/**\"]\n",
        );
        assert_eq!(
            policy.submit_tiers,
            vec![
                SubmitTierRule {
                    tier: 0,
                    paths: vec!["docs/**".into(), "*.md".into()]
                },
                SubmitTierRule {
                    tier: 2,
                    paths: vec!["scripts/**".into()]
                },
            ]
        );
        assert!(ok_dir("[submit]\ntiers = []\n").submit_tiers.is_empty());
        assert_eq!(
            err_dir("[submit]\ntiers = \"x\"\n"),
            format!(
                "{FILE}: submit.tiers must be an array of tables ([[submit.tiers]] entries with \
                 'tier' and 'paths' keys)"
            )
        );
        assert_eq!(
            err_dir("[[submit.tiers]]\ntier = 3\npaths = [\"a\"]\n"),
            format!("{FILE}: submit.tiers[0].tier must be one of (0, 1, 2), got 3")
        );
        assert!(
            err_dir("[[submit.tiers]]\ntier = true\npaths = [\"a\"]\n")
                .contains("submit.tiers[0].tier")
        );
        assert!(err_dir("[[submit.tiers]]\npaths = [\"a\"]\n").contains("got None"));
        assert_eq!(
            err_dir("[[submit.tiers]]\ntier = 1\n"),
            format!("{FILE}: submit.tiers[0].paths must be a non-empty list of non-empty strings")
        );
        assert!(err_dir("[[submit.tiers]]\ntier = 1\npaths = [\"a\"]\n[[submit.tiers]]\ntier = 1\npaths = []\n")
            .contains("submit.tiers[1].paths"));
        assert_eq!(
            err_dir("[[submit.tiers]]\ntier = 1\npaths = [\"a\"]\npathz = [\"b\"]\n"),
            format!(
                "{FILE}: submit.tiers[0] has unknown key(s) ['pathz']; each rule takes exactly \
                 'tier' and 'paths'"
            )
        );
    }

    #[test]
    fn submit_tiers_carry_on_github_policy() {
        // The Python loader dropped [[submit.tiers]] for kind = "github".
        let policy = ok_github("[[submit.tiers]]\ntier = 0\npaths = [\"docs/**\"]\n");
        assert_eq!(policy.submit_tiers.len(), 1);
        assert_eq!(policy.submit_tiers[0].tier, 0);
    }

    // --- sandbox (flat) / phase / held_out ----------------------------------------

    #[test]
    fn sandbox_setup_phase_verify_held_out_root() {
        let absent = ok_dir("");
        assert_eq!(absent.sandbox_setup, None);
        assert_eq!(absent.phase_verify, None);
        assert_eq!(absent.held_out_root, None);
        let policy = ok_dir(
            "[sandbox]\nsetup = \"uv sync\"\n[phase]\nverify = \"uv run pytest\"\n[held_out]\nroot = \".flywheel/held-out\"\n",
        );
        assert_eq!(policy.sandbox_setup.as_deref(), Some("uv sync"));
        assert_eq!(policy.sandbox.preset, "fast");
        assert_eq!(policy.phase_verify.as_deref(), Some("uv run pytest"));
        assert_eq!(
            policy.held_out_root,
            Some(PathBuf::from(".flywheel/held-out"))
        );
        assert!(
            err_dir("[sandbox]\nsetup = \"\"\n")
                .contains("sandbox.setup must be a non-empty string")
        );
        assert!(
            err_dir("[phase]\nverify = \" \"\n")
                .contains("phase.verify must be a non-empty string")
        );
        assert!(err_dir("[phase]\nverify = 1\n").contains("phase.verify"));
        assert!(
            err_dir("[held_out]\nroot = \"\"\n")
                .contains("held_out.root must be a non-empty string")
        );
        assert_eq!(
            err_top("held_out = \"x\"\n"),
            format!("{FILE}: [held_out] must be a table")
        );
        assert_eq!(
            err_top("phase = 1\n"),
            format!("{FILE}: [phase] must be a table")
        );
    }

    // --- deadlines -----------------------------------------------------------

    #[test]
    fn deadlines_table_resolves_through_core() {
        assert_eq!(ok_dir("").deadlines, DeadlineConfig::default());
        let policy = ok_dir(
            "[deadlines]\nagent_iteration_seconds = 0\nrubric_judge_seconds = 42.5\nfuture = \"x\"\n",
        );
        assert_eq!(policy.deadlines.agent_iteration_seconds, None);
        assert_eq!(policy.deadlines.rubric_judge_seconds, Some(42.5));
        assert_eq!(policy.deadlines.command_grader_seconds, Some(900.0));
        assert_eq!(
            err_dir("[deadlines]\nagent_iteration_seconds = \"soon\"\n"),
            format!("{FILE}: deadlines.agent_iteration_seconds must be a number, got \"soon\"")
        );
        assert!(
            err_dir("[deadlines]\nagent_iteration_seconds = true\n")
                .contains("deadlines.agent_iteration_seconds")
        );
        assert_eq!(
            err_top("deadlines = 5\n"),
            format!("{FILE}: [deadlines] must be a table")
        );
    }

    // --- autopilot -----------------------------------------------------------

    #[test]
    fn autopilot_table_parses_and_validates() {
        let absent = ok_dir("");
        assert_eq!(absent.autopilot_target_depth, 5);
        assert_eq!(absent.autopilot_landing, "merge");
        assert_eq!(absent.autopilot_interval_seconds, 300.0);
        assert_eq!(absent.autopilot_weights, None);
        let policy = ok_dir(
            "[autopilot]\ntarget_depth = 2\nlanding = \"pr\"\ninterval_seconds = 10\n[autopilot.weights]\ntier = 1.5\nunblock = 7\n",
        );
        assert_eq!(policy.autopilot_target_depth, 2);
        assert_eq!(policy.autopilot_landing, "pr");
        assert_eq!(policy.autopilot_interval_seconds, 10.0);
        assert_eq!(
            policy.autopilot_weights,
            Some(AutopilotWeights {
                tier: 1.5,
                unblock: 7.0,
                ..AutopilotWeights::default()
            })
        );
        assert_eq!(
            err_dir("[autopilot]\ntarget_depth = 0\n"),
            format!("{FILE}: autopilot.target_depth must be a positive integer")
        );
        assert!(
            err_dir("[autopilot]\ntarget_depth = \"x\"\n")
                .contains("autopilot.target_depth must be an integer")
        );
        assert_eq!(
            err_dir("[autopilot]\nlanding = \"phase\"\n"),
            format!("{FILE}: autopilot.landing must be one of ('merge', 'pr'), got 'phase'")
        );
        assert_eq!(
            err_dir("[autopilot]\ninterval_seconds = 0\n"),
            format!("{FILE}: autopilot.interval_seconds must be positive")
        );
        assert!(
            err_dir("[autopilot.weights]\ntier = \"x\"\n")
                .contains("autopilot.weights.tier must be a number")
        );
        assert_eq!(
            err_dir("[autopilot]\nweights = 1\n"),
            format!("{FILE}: [autopilot.weights] must be a table")
        );
    }

    #[test]
    fn autopilot_cost_controls_parse_and_validate() {
        let absent = ok_dir("");
        assert_eq!(absent.autopilot_authoring_concurrency, 3);
        assert_eq!(absent.autopilot_discovery_cache_ttl_seconds, 21_600.0);
        let set =
            ok_dir("[autopilot]\nauthoring_concurrency = 1\ndiscovery_cache_ttl_seconds = 0\n");
        assert_eq!(set.autopilot_authoring_concurrency, 1);
        assert_eq!(set.autopilot_discovery_cache_ttl_seconds, 0.0);
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(json["autopilot_authoring_concurrency"], 1);
        assert_eq!(json["autopilot_discovery_cache_ttl_seconds"], 0.0);
        // At the defaults the keys stay out of the serialized policy.
        let json = serde_json::to_value(&absent).unwrap();
        assert!(json.get("autopilot_authoring_concurrency").is_none());
        assert!(json.get("autopilot_discovery_cache_ttl_seconds").is_none());
        assert_eq!(
            err_dir("[autopilot]\nauthoring_concurrency = 0\n"),
            format!("{FILE}: autopilot.authoring_concurrency must be a positive integer")
        );
        assert_eq!(
            err_dir("[autopilot]\ndiscovery_cache_ttl_seconds = -1\n"),
            format!("{FILE}: autopilot.discovery_cache_ttl_seconds must be >= 0")
        );
    }

    // --- triage --------------------------------------------------------------

    #[test]
    fn triage_table_parses_and_validates() {
        let absent = ok_dir("");
        assert_eq!(absent.triage_intake_label, "flywheel");
        assert_eq!(absent.triage_ready_label, "flywheel:ready");
        assert_eq!(absent.triage_needs_detail_label, "flywheel:needs-detail");
        assert_eq!(absent.triage_interval_seconds, 300.0);
        assert_eq!(absent.triage_max_per_pass, None);
        let policy = ok_dir(
            "[triage]\nintake_label = \"in\"\nready_label = \"go\"\nneeds_detail_label = \"nd\"\ninterval_seconds = 5\nmax_per_pass = 3\n",
        );
        assert_eq!(policy.triage_intake_label, "in");
        assert_eq!(policy.triage_ready_label, "go");
        assert_eq!(policy.triage_needs_detail_label, "nd");
        assert_eq!(policy.triage_interval_seconds, 5.0);
        assert_eq!(policy.triage_max_per_pass, Some(3));
        assert!(
            err_dir("[triage]\nintake_label = \"\"\n")
                .contains("triage.intake_label must be a non-empty string")
        );
        assert_eq!(
            err_dir("[triage]\ninterval_seconds = -1\n"),
            format!("{FILE}: triage.interval_seconds must be positive")
        );
        assert_eq!(
            err_dir("[triage]\nmax_per_pass = 0\n"),
            format!("{FILE}: triage.max_per_pass must be a positive integer")
        );
        assert!(
            err_dir("[triage]\nmax_per_pass = true\n")
                .contains("triage.max_per_pass must be an integer")
        );
    }

    // --- sandbox.* -----------------------------------------------------------

    #[test]
    fn absent_sandbox_section_resolves_to_fast() {
        let sb = ok_dir("").sandbox;
        assert_eq!(sb, SandboxPolicy::default());
        assert_eq!(sb.preset, "fast");
        assert_eq!(sb.backend, "worktree");
        assert_eq!(sb.permission_mode, "bypassPermissions");
        assert_eq!(sb.capabilities.skills, Skills::Named("all".into()));
        assert!(sb.capabilities.allowed_tools.is_empty());
        assert_eq!(sb.capabilities.setting_sources, None);
        assert_eq!(sb.network.policy, "allow");
        assert_eq!(sb.limits.max_turns, 500);
        assert_eq!(sb.limits.max_retries, 1);
        assert_eq!(sb.retention.on_done, "destroy");
        assert_eq!(sb.retention.on_failure, "park");
        assert!(!sb.exec.enabled);
        assert_eq!(sb.container.auth, "oauth");
        assert_eq!(sb.container.exec_timeout, 1800);
    }

    #[test]
    fn every_sandbox_subtable_parses() {
        let sb = ok_dir(
            "[sandbox]\nbackend = \"worktree\"\npermission_mode = \"default\"\n\
             [sandbox.exec]\nenabled = true\n\
             [sandbox.capabilities]\nskills = \"none\"\nallowed_tools = [\"Read\", \"Write\"]\n\
             [sandbox.capabilities.mcp]\nservers = [\"proto\"]\nstrict = true\n\
             [sandbox.network]\npolicy = \"deny\"\nallow_hosts = [\"api.github.com\"]\n\
             [sandbox.env]\ninherit_home = false\npass = [\"HOME\"]\nset = { A = \"1\" }\n\
             [sandbox.limits]\nmax_turns = 42\nmax_cost_usd = 1.5\nrubric_judge_max_turns = 8\n\
             [sandbox.retention]\non_done = \"preserve\"\nsweep_days = 2\n",
        )
        .sandbox;
        assert_eq!(sb.permission_mode, "default");
        assert!(sb.exec.enabled);
        assert!(sb.exec.auto_allow);
        assert_eq!(sb.capabilities.skills, Skills::Named("none".into()));
        assert_eq!(sb.capabilities.allowed_tools, vec!["Read", "Write"]);
        assert_eq!(sb.capabilities.mcp_servers, vec!["proto"]);
        assert!(sb.capabilities.mcp_strict);
        assert_eq!(sb.network.policy, "deny");
        assert_eq!(sb.network.allow_hosts, vec!["api.github.com"]);
        assert!(!sb.env.inherit_home);
        assert_eq!(sb.env.passthrough, vec!["HOME"]);
        assert_eq!(
            sb.env.set_values,
            BTreeMap::from([("A".to_string(), "1".to_string())])
        );
        assert_eq!(sb.limits.max_turns, 42);
        assert_eq!(sb.limits.max_cost_usd, 1.5);
        assert_eq!(sb.limits.rubric_judge_max_turns, Some(8));
        assert_eq!(sb.retention.on_done, "preserve");
        assert_eq!(sb.retention.sweep_days, 2);
    }

    #[test]
    fn sandbox_presets_merge_sparse_overrides() {
        let sb = ok_dir("[sandbox.limits]\nmax_turns = 99\n").sandbox;
        assert_eq!(sb.limits.max_turns, 99);
        assert_eq!(sb.limits.max_retries, 1);
        assert_eq!(sb.capabilities.skills, Skills::Named("all".into()));

        let balanced = ok_dir("[sandbox]\npreset = \"balanced\"\n").sandbox;
        assert_eq!(balanced.preset, "balanced");
        assert!(balanced.capabilities.mcp_strict);
        assert!(balanced.capabilities.allowed_tools.is_empty());
        assert!(!balanced.exec.enabled);

        let hardened = ok_dir(
            "[sandbox]\npreset = \"hardened\"\n[sandbox.capabilities]\nskills = [\"a\", \"b\"]\n",
        )
        .sandbox;
        assert_eq!(hardened.preset, "hardened");
        assert_eq!(
            hardened.capabilities.allowed_tools,
            vec!["Bash", "Edit", "Glob", "Grep", "Read", "Write"]
        );
        assert_eq!(
            hardened.capabilities.setting_sources,
            Some(vec!["project".to_string()])
        );
        assert!(hardened.capabilities.mcp_strict);
        assert!(hardened.exec.enabled);
        assert_eq!(
            hardened.capabilities.skills,
            Skills::List(vec!["a".into(), "b".into()])
        );
        assert_eq!(hardened.permission_mode, "bypassPermissions");

        // List-replace, never append.
        let replaced = ok_dir("[sandbox]\npreset = \"hardened\"\n[sandbox.capabilities]\nallowed_tools = [\"Read\"]\n").sandbox;
        assert_eq!(replaced.capabilities.allowed_tools, vec!["Read"]);
    }

    #[test]
    fn unknown_preset_fails_fast() {
        assert_eq!(
            err_dir("[sandbox]\npreset = \"turbo\"\n"),
            format!(
                "{FILE}: sandbox.preset 'turbo' is not available (defined presets: ('fast', \
                 'balanced', 'hardened'))"
            )
        );
        assert!(err_dir("[sandbox]\npreset = 5\n").contains("sandbox.preset 5 is not available"));
    }

    #[test]
    fn malformed_sandbox_values_fail_fast_with_keyed_message() {
        let cases = [
            (
                "[sandbox.limits]\nmax_turns = \"lots\"\n",
                "sandbox.limits.max_turns must be an integer",
            ),
            (
                "[sandbox.network]\npolicy = \"sideways\"\n",
                "sandbox.network.policy must be one of ('allow', 'deny'), got 'sideways'",
            ),
            (
                "[sandbox.capabilities]\nallowed_tools = \"Read\"\n",
                "sandbox.capabilities.allowed_tools must be a list of non-empty strings",
            ),
            (
                "[sandbox.retention]\non_done = \"vaporize\"\n",
                "sandbox.retention.on_done must be one of ('destroy', 'preserve'), got 'vaporize'",
            ),
            (
                "[sandbox.retention]\non_failure = \"keep\"\n",
                "sandbox.retention.on_failure must be one of ('park', 'destroy'), got 'keep'",
            ),
            (
                "[sandbox.exec]\nenabled = \"yes\"\n",
                "sandbox.exec.enabled must be a boolean",
            ),
            (
                "[sandbox.capabilities]\nskills = 1\n",
                "sandbox.capabilities.skills must be \"all\", \"none\", or a list of non-empty strings",
            ),
            (
                "[sandbox.capabilities.mcp]\nstrict = 1\n",
                "sandbox.capabilities.mcp.strict must be a boolean",
            ),
            (
                "[sandbox.env]\nset = { A = 1 }\n",
                "sandbox.env.set must be a table of string values",
            ),
            (
                "[sandbox.env]\npass = \"HOME\"\n",
                "sandbox.env.pass must be a list of non-empty strings",
            ),
            (
                "[sandbox.limits]\nrubric_judge_max_turns = 0\n",
                "sandbox.limits.rubric_judge_max_turns must be a positive integer",
            ),
            (
                "[sandbox.limits]\nrubric_judge_max_turns = true\n",
                "sandbox.limits.rubric_judge_max_turns must be a positive integer",
            ),
            (
                "[sandbox.limits]\nrubric_judge_max_turns = 2.5\n",
                "sandbox.limits.rubric_judge_max_turns must be a positive integer",
            ),
            (
                "[sandbox.limits]\nmax_cost_usd = \"x\"\n",
                "sandbox.limits.max_cost_usd must be a number",
            ),
            (
                "[sandbox]\nbackend = \"containr\"\n",
                "sandbox.backend must be one of ('worktree', 'container'), got 'containr'",
            ),
            (
                "[sandbox]\npermission_mode = \"\"\n",
                "sandbox.permission_mode must be a non-empty string",
            ),
            ("[sandbox]\nexec = 1\n", "[sandbox.exec] must be a table"),
            (
                "[sandbox.capabilities]\nmcp = \"x\"\n",
                "[sandbox.capabilities.mcp] must be a table",
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(err_dir(body), format!("{FILE}: {expected}"), "body: {body}");
        }
        assert_eq!(
            err_top("sandbox = \"x\"\n"),
            format!("{FILE}: [sandbox] must be a table")
        );
    }

    #[test]
    fn unknown_sandbox_keys_are_ignored() {
        let sb = ok_dir("[sandbox]\nfuture_scalar = \"x\"\n[sandbox.capabilities]\nfuture_key = 1\n[future_section]\nx = 1\n").sandbox;
        assert_eq!(sb.preset, "fast");
    }

    #[test]
    fn container_table_parses_and_requires_image() {
        let absent = ok_dir("").sandbox.container;
        assert_eq!(absent, SandboxContainer::default());
        assert_eq!(
            ok_dir("[sandbox]\nbackend = \"worktree\"\n")
                .sandbox
                .backend,
            "worktree"
        );
        assert_eq!(
            err_dir("[sandbox]\nbackend = \"container\"\n"),
            format!(
                "{FILE}: [sandbox.container] image is required when sandbox.backend = 'container'"
            )
        );
        let policy = ok_dir(
            "[sandbox]\nbackend = \"container\"\n[sandbox.container]\nimage = \"flywheel-agent:latest\"\nmodel = \"claude-sonnet-4-6\"\nauth = \"oauth\"\nauth_env = \"TOK\"\nexec_timeout = 900\negress_network = \"egress-proxy\"\n",
        );
        assert_eq!(policy.sandbox.backend, "container");
        let c = policy.sandbox.container;
        assert_eq!(c.image, "flywheel-agent:latest");
        assert_eq!(c.model, "claude-sonnet-4-6");
        assert_eq!(c.auth_env, "TOK");
        assert_eq!(c.exec_timeout, 900);
        assert_eq!(c.egress_network, "egress-proxy");
        assert_eq!(
            err_dir(
                "[sandbox]\nbackend = \"container\"\n[sandbox.container]\nimage = \"img\"\nauth = \"magic\"\n"
            ),
            format!(
                "{FILE}: sandbox.container.auth must be one of ('oauth', 'session', 'api_key', \
                 'none'), got 'magic'"
            )
        );
        for mode in CONTAINER_AUTH_MODES {
            let policy = ok_dir(&format!(
                "[sandbox]\nbackend = \"container\"\n[sandbox.container]\nimage = \"img\"\nauth = \"{mode}\"\n"
            ));
            assert_eq!(policy.sandbox.container.auth, mode);
        }
    }

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
        assert_eq!(
            resolve_sandbox_root(None, &repo).unwrap(),
            repo.join(".flywheel").join("worktrees")
        );
        assert_eq!(
            resolve_sandbox_root(Some(Path::new("wt/../boxes")), &repo).unwrap(),
            repo.join("boxes")
        );
        assert_eq!(
            resolve_sandbox_root(Some(Path::new("/abs/root")), &repo).unwrap(),
            PathBuf::from("/abs/root")
        );
        assert_eq!(
            resolve_sandbox_root(Some(Path::new("@nope")), &repo)
                .unwrap_err()
                .to_string(),
            "paths.sandbox_root: unknown token '@nope' (supported: @cache, @sibling)"
        );
    }

    #[test]
    fn sandbox_root_sibling_token() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        fs::create_dir(&repo).unwrap();
        let resolved = resolve_sandbox_root(Some(Path::new("@sibling")), &repo).unwrap();
        assert_eq!(
            resolved,
            fs::canonicalize(dir.path())
                .unwrap()
                .join("myrepo.worktrees")
        );
        let err = resolve_sandbox_root(Some(Path::new("@sibling")), Path::new("/")).unwrap_err();
        assert!(err.to_string().contains("is the filesystem root"));
    }

    #[test]
    fn sandbox_root_cache_token_keys_by_repo_identity() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let repo = dir.path().join("proj");
        fs::create_dir_all(&repo).unwrap();
        // Scope the XDG override to this test's resolution only.
        let previous = std::env::var_os("XDG_CACHE_HOME");
        // SAFETY: the test harness runs this single-threaded w.r.t. env use
        // of XDG_CACHE_HOME; no other test in this module reads it.
        unsafe { std::env::set_var("XDG_CACHE_HOME", &cache) };
        let first = resolve_sandbox_root(Some(Path::new("@cache")), &repo);
        let second = resolve_sandbox_root(Some(Path::new("@cache")), &repo);
        unsafe {
            match previous {
                Some(value) => std::env::set_var("XDG_CACHE_HOME", value),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
        }
        let first = first.unwrap();
        assert_eq!(first, second.unwrap());
        assert!(
            first.starts_with(cache.join("flywheel")),
            "{}",
            first.display()
        );
        assert!(first.ends_with("worktrees"));
        let keyed = first
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(keyed.starts_with("proj-"), "{keyed}");
        assert_eq!(keyed.len(), "proj-".len() + 12);
    }

    // --- parity with the Python loader ---------------------------------------------

    fn parity_check(policy: &WorkPolicy, expected: &str) {
        let actual = serde_json::to_value(policy).unwrap();
        let expected: serde_json::Value = serde_json::from_str(expected).unwrap();
        assert_eq!(
            actual, expected,
            "resolved policy diverges from the Python loader"
        );
    }

    /// The repository's own `flywheel.toml`, resolved by the Python loader
    /// (`flywheel_orchestrator._policy.load_policy`) and dumped as JSON.
    #[test]
    fn parity_repository_flywheel_toml() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let policy = load_policy(root.join("flywheel.toml")).unwrap();
        parity_check(
            &policy,
            r#"{"agent_id": null, "agent_transport": null, "autopilot_interval_seconds": 300.0, "autopilot_landing": "merge", "autopilot_target_depth": 5, "autopilot_weights": null, "db_path": ".flywheel/flywheel.sqlite", "deadlines": {"agent_iteration_seconds": 3600.0, "autopilot_agent_seconds": 1800.0, "command_grader_seconds": 900.0, "docker_management_seconds": 120.0, "rubric_judge_seconds": 600.0}, "default_graders": [], "execution_capabilities": [], "execution_mode": "local", "github_ci_failure_filter": "failure", "github_ci_repo": null, "github_done_action": "comment", "github_label": null, "github_repo": null, "github_require_triage_receipt": false, "github_review_repo": null, "held_out_root": ".flywheel/verification/held-out", "model": "claude-opus-4-8", "phase_verify": "scripts/check.sh", "protected_paths": [".github/**", "flywheel.toml", "scripts/**", "CLAUDE.md", ".claude/**"], "sandbox": {"backend": "worktree", "capabilities": {"allowed_tools": [], "denied_tools": [], "mcp_servers": [], "mcp_strict": false, "setting_sources": null, "skills": "all"}, "container": {"auth": "oauth", "auth_env": "", "egress_network": "", "exec_timeout": 1800, "image": "", "model": ""}, "env": {"inherit_home": true, "passthrough": [], "set_values": {}}, "exec": {"auto_allow": true, "enabled": false}, "limits": {"lease_seconds": 300, "max_cost_usd": 0.0, "max_retries": 1, "max_tokens": 0, "max_turns": 500, "rubric_judge_max_turns": null, "wall_clock_seconds": 0}, "network": {"allow_hosts": [], "allow_unix_sockets": [], "policy": "allow"}, "permission_mode": "bypassPermissions", "preset": "fast", "retention": {"on_done": "destroy", "on_failure": "park", "sweep_days": 7}}, "sandbox_root": ".flywheel/worktrees", "sandbox_setup": null, "source_kind": "directory", "store_backend": "sqlite", "store_schema": null, "submit_base": null, "submit_pr_base": null, "submit_recovery_agent_max_turns": 30, "submit_recovery_agent_max_wall_seconds": 900.0, "submit_remote": "origin", "submit_strategy": "merge", "submit_tiers": [], "submit_verify": "scripts/check.sh", "tasks_dir": ".flywheel/tasks", "triage_intake_label": "flywheel", "triage_interval_seconds": 300.0, "triage_max_per_pass": null, "triage_needs_detail_label": "flywheel:needs-detail", "triage_ready_label": "flywheel:ready", "worker_checkpoint_nudge_seconds": 300.0, "worker_concurrency": 3, "worker_session_pause_ceiling_seconds": 21600.0}"#,
        );
    }

    /// A github-kind policy exercising every optional section, resolved by
    /// the Python loader and dumped as JSON. The only representational
    /// difference is the grader: Python's `asdict` emits `"name": null`
    /// where the core `Grader` serializer omits the absent name.
    #[test]
    fn parity_rich_github_policy() {
        let body = r#"
[source]
kind = "github"
repo = "acme/widgets"
label = "fw"
done_action = "close"
require_triage_receipt = true

[[defaults.graders]]
type = "command"
run = "cargo test"

[paths]
db = "state/fw.sqlite"
sandbox_root = "@cache"

[agent]
model = "claude-sonnet-4-5"
id = "claude-code"
transport = "cli"

[store]
backend = "postgres"
schema = "fw_ci"

[execution]
mode = "distributed"
capabilities = ["gpu", "cuda"]

[worker]
concurrency = 0
checkpoint_nudge_seconds = 0
session_pause_ceiling_seconds = 120

[submit]
strategy = "pr"
remote = "upstream"
pr_base = "develop"
base = "integration"
verify = "make check"
protected_paths = ["ci/**"]
recovery_agent_max_turns = 0
recovery_agent_max_wall_seconds = 60

[phase]
verify = "make gate"

[held_out]
root = "oracles"

[deadlines]
agent_iteration_seconds = 0
rubric_judge_seconds = 42.5
future_key = "ignored"

[autopilot]
target_depth = 2
landing = "pr"
interval_seconds = 10
[autopilot.weights]
tier = 1.5
unblock = 7

[triage]
intake_label = "in"
ready_label = "ready"
needs_detail_label = "nd"
interval_seconds = 5
max_per_pass = 3

[sandbox]
preset = "hardened"
setup = "cargo fetch"
backend = "container"
permission_mode = "default"
[sandbox.exec]
auto_allow = false
[sandbox.capabilities]
skills = ["a", "b"]
denied_tools = ["WebFetch"]
[sandbox.capabilities.mcp]
servers = ["serena"]
[sandbox.network]
policy = "deny"
allow_hosts = ["api.github.com"]
allow_unix_sockets = ["/run/x.sock"]
[sandbox.env]
pass = ["HOME", "CARGO_HOME"]
set = { RUSTC_WRAPPER = "sccache" }
inherit_home = false
[sandbox.limits]
max_turns = 7
max_retries = 2
lease_seconds = 60
wall_clock_seconds = 99
max_cost_usd = 2
max_tokens = 1000
rubric_judge_max_turns = 8
[sandbox.retention]
on_done = "preserve"
on_failure = "destroy"
sweep_days = 3
[sandbox.container]
image = "img:1"
model = "m"
auth = "api_key"
auth_env = "TOK"
exec_timeout = 5
egress_network = "egress"
"#;
        let policy = load(body).unwrap();
        parity_check(
            &policy,
            r#"{"agent_id": "claude-code", "agent_transport": "cli", "autopilot_interval_seconds": 10.0, "autopilot_landing": "pr", "autopilot_target_depth": 2, "autopilot_weights": {"effort": 1.0, "importance": 3.0, "interrupt_base": 10000.0, "tier": 1.5, "unblock": 7.0, "urgency": 3.0}, "db_path": "state/fw.sqlite", "deadlines": {"agent_iteration_seconds": null, "autopilot_agent_seconds": 1800.0, "command_grader_seconds": 900.0, "docker_management_seconds": 120.0, "rubric_judge_seconds": 42.5}, "default_graders": [{"run": "cargo test", "type": "command"}], "execution_capabilities": ["cuda", "gpu"], "execution_mode": "distributed", "github_ci_failure_filter": "failure", "github_ci_repo": null, "github_done_action": "close", "github_label": "fw", "github_repo": "acme/widgets", "github_require_triage_receipt": true, "github_review_repo": null, "held_out_root": "oracles", "model": "claude-sonnet-4-5", "phase_verify": "make gate", "protected_paths": ["ci/**"], "sandbox": {"backend": "container", "capabilities": {"allowed_tools": ["Bash", "Edit", "Glob", "Grep", "Read", "Write"], "denied_tools": ["WebFetch"], "mcp_servers": ["serena"], "mcp_strict": true, "setting_sources": ["project"], "skills": ["a", "b"]}, "container": {"auth": "api_key", "auth_env": "TOK", "egress_network": "egress", "exec_timeout": 5, "image": "img:1", "model": "m"}, "env": {"inherit_home": false, "passthrough": ["HOME", "CARGO_HOME"], "set_values": {"RUSTC_WRAPPER": "sccache"}}, "exec": {"auto_allow": false, "enabled": true}, "limits": {"lease_seconds": 60, "max_cost_usd": 2.0, "max_retries": 2, "max_tokens": 1000, "max_turns": 7, "rubric_judge_max_turns": 8, "wall_clock_seconds": 99}, "network": {"allow_hosts": ["api.github.com"], "allow_unix_sockets": ["/run/x.sock"], "policy": "deny"}, "permission_mode": "default", "preset": "hardened", "retention": {"on_done": "preserve", "on_failure": "destroy", "sweep_days": 3}}, "sandbox_root": "@cache", "sandbox_setup": "cargo fetch", "source_kind": "github", "store_backend": "postgres", "store_schema": "fw_ci", "submit_base": "integration", "submit_pr_base": "develop", "submit_recovery_agent_max_turns": 0, "submit_recovery_agent_max_wall_seconds": 60.0, "submit_remote": "upstream", "submit_strategy": "pr", "submit_tiers": [], "submit_verify": "make check", "tasks_dir": null, "triage_intake_label": "in", "triage_interval_seconds": 5.0, "triage_max_per_pass": 3, "triage_needs_detail_label": "nd", "triage_ready_label": "ready", "worker_checkpoint_nudge_seconds": 0.0, "worker_concurrency": 0, "worker_session_pause_ceiling_seconds": 120.0}"#,
        );
    }

    #[test]
    fn policy_round_trips_through_serde() {
        let policy = ok_github(
            "[[submit.tiers]]\ntier = 2\npaths = [\"scripts/**\"]\n[sandbox.capabilities]\nskills = [\"x\"]\n",
        );
        let value = serde_json::to_value(&policy).unwrap();
        let back: WorkPolicy = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, policy);
        assert_eq!(
            value["submit_tiers"],
            json!([{"tier": 2, "paths": ["scripts/**"]}])
        );
    }

    #[test]
    fn python_repr_helpers_match_python() {
        assert_eq!(py_str_repr("it's"), "'it\\'s'");
        assert_eq!(py_float_repr(2.0), "2.0");
        assert_eq!(py_float_repr(2.5), "2.5");
        assert_eq!(py_repr(&Value::Boolean(true)), "True");
        assert_eq!(
            py_repr(&Value::Array(vec![
                Value::Integer(1),
                Value::String("a".into())
            ])),
            "[1, 'a']"
        );
        assert_eq!(py_tuple(&["only"]), "('only',)");
        assert_eq!(py_int_tuple(&[0, 1, 2]), "(0, 1, 2)");
        assert_eq!(py_list(&["a", "b"]), "['a', 'b']");
    }
}
