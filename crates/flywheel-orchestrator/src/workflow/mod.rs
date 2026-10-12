//! Port of the Python `flywheel_orchestrator._workflow` module: the
//! orchestrator's operator verbs (`next`, `status`, `live`, `history`,
//! `show`, `validate`, `recheck-blocked`, `recover`, `resolve`, `init`) and
//! the argparse-identical entry point the product shell calls in-process.
//!
//! Every verb is a library function taking parsed arguments, already-open
//! stores, and the two output sinks, and returning the process exit code.
//! [`main`] / [`run`] wire them to the command line: resolve the effective
//! policy (flag > `flywheel.toml` > built-in default), build the work
//! source through the source registry, open the policy-selected stores,
//! then dispatch. The two verbs whose bodies live elsewhere -- the
//! `orchestrate` loop and the `archive` sweep (which belongs to the worktree
//! layer, above this crate) -- are injected through [`Verbs`] by the shell.
//!
//! Authoritative specs: `docs/cli.md`, `docs/orchestration.md`,
//! `docs/configuration.md`.

pub mod args;
pub mod init;
pub mod live;
pub mod ops;
pub mod select;
pub mod show;
pub mod status;
pub mod validate;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flywheel_core::loaders::TaskLoadError;
use flywheel_core::loaders::py_str_repr as py_repr;
use flywheel_core::store::{HarnessStore, StoreError};
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_core::task::Grader;
use flywheel_core::telemetry_file::{JsonStyle, python_json_dumps};
use serde_json::Value;

use crate::claims::{ClaimStore, ClaimStoreError};
use crate::claims_postgres::register_postgres_backends;
use crate::github::register_github_sources;
use crate::history::build_task_phase_index;
use crate::policy::{
    DEFAULT_POLICY_FILENAME, PolicyError, WorkPolicy, load_policy, resolve_sandbox_root,
};
use crate::source_registry::{SourceBuildContext, SourceRegistry, SourceRegistryError};
use crate::sources::{DirectoryWorkSource, WorkSource, WorkSourceError};
use crate::store_factory::{StoreBundle, StoreConfigError, open_stores};
use crate::store_registry::{
    DEFAULT_BACKEND, Environ, StoreConfig, StoreRegistries, environ_from_process,
};

pub use args::{
    ArgError, Command, CommonArgs, HistoryArgs, InitArgs, LiveArgs, OrchestrateArgs, Parsed,
    RecheckBlockedArgs, RecoverArgs, ResolveArgs, ShowArgs, StatusArgs, parse_args,
};
pub use init::{
    InitHost, ProductionInitHost, PromptError, cmd_init, cmd_init_agent, render_init_policy,
};
pub use live::{LiveRunRow, cmd_live, collect_live_rows, format_live_line};
pub use ops::{
    cmd_recheck_blocked, cmd_recover, cmd_resolve, format_recheck_line, recover_stranded_lifecycles,
};
pub use select::{
    LatestLifecycleRow, SelectOptions, StatusRow, build_status_rows, cmd_next,
    latest_lifecycle_row, reachability_held_prerequisites, select_next_task, status_rows,
};
pub use show::{
    CommitProvenance, TRAILER_KEY_RUN, cmd_history, cmd_show, resolve_commit_provenance,
};
pub use status::cmd_status;
pub use validate::cmd_validate;

/// The default tasks root when neither a flag nor a policy names one.
pub const DEFAULT_TASKS_DIR: &str = ".flywheel/tasks";
/// The default SQLite store path (`flywheel_core.workflow.DEFAULT_DB_PATH`).
pub const DEFAULT_DB_PATH: &str = ".flywheel/flywheel.sqlite";

// --- Errors ------------------------------------------------------------------------

/// Anything a verb can fail with. [`run`] maps the input-side variants
/// (policy, work source, task file, missing file) to the `error: ...` /
/// exit 2 surface the Python entry point prints; store-side failures exit
/// 1 with the same prefix.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("{0}")]
    Policy(#[from] PolicyError),
    #[error("{0}")]
    Source(#[from] WorkSourceError),
    #[error("{0}")]
    SourceBuild(#[from] SourceRegistryError),
    #[error("{0}")]
    TaskLoad(#[from] TaskLoadError),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Claims(#[from] ClaimStoreError),
    #[error("{0}")]
    StoreConfig(#[from] StoreConfigError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

impl WorkflowError {
    /// The exit code [`run`] returns for this failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            WorkflowError::Policy(_)
            | WorkflowError::Source(_)
            | WorkflowError::SourceBuild(_)
            | WorkflowError::TaskLoad(_) => 2,
            WorkflowError::Io(err) if err.kind() == std::io::ErrorKind::NotFound => 2,
            _ => 1,
        }
    }
}

// --- Policy, source, and store resolution -----------------------------------------

/// Load the policy the orchestrator verbs honor for an invocation.
///
/// Mirrors the orchestrate CLI's precedence so a downstream package can pin
/// to the same resolution:
///
/// * an explicit `policy_path` is loaded (a missing/invalid file is an
///   error, propagated as [`PolicyError`]);
/// * otherwise `flywheel.toml` in the working directory is auto-detected;
/// * otherwise the parents of the working directory are searched toward
///   the filesystem root, and a hit RE-ANCHORS the process there
///   (`set_current_dir`) before loading -- `fw` invoked from a
///   subdirectory used to resolve every cwd-relative default against the
///   subdirectory and silently report an empty queue, so the whole
///   invocation must behave exactly like a repo-root one;
/// * otherwise there is no policy and every default falls back to the
///   built-in `.flywheel/` layout.
///
/// An empty `policy_path` is treated as "not specified" to match the
/// argparse default surface.
pub fn load_effective_policy(
    policy_path: Option<&Path>,
) -> Result<Option<WorkPolicy>, PolicyError> {
    if let Some(path) = policy_path.filter(|p| !p.as_os_str().is_empty()) {
        return load_policy(path).map(Some);
    }
    let candidate = Path::new(DEFAULT_POLICY_FILENAME);
    if candidate.is_file() {
        return load_policy(candidate).map(Some);
    }
    let cwd = std::env::current_dir().map_err(|err| PolicyError::Read {
        path: PathBuf::from(DEFAULT_POLICY_FILENAME),
        message: format!("cannot resolve the working directory: {err}"),
    })?;
    for parent in cwd.ancestors().skip(1) {
        let found = parent.join(DEFAULT_POLICY_FILENAME);
        if found.is_file() {
            std::env::set_current_dir(parent).map_err(|err| PolicyError::Read {
                path: found.clone(),
                message: format!("cannot re-anchor to {}: {err}", parent.display()),
            })?;
            return load_policy(candidate).map(Some);
        }
    }
    Ok(None)
}

/// The shipped source registry: `directory` plus the three GitHub kinds.
pub fn default_source_registry() -> SourceRegistry {
    let mut registry = crate::source_registry::default_registry();
    register_github_sources(&mut registry);
    registry
}

/// The `[source]` settings table a policy describes, in the shape the
/// registry factories read (`kind` plus the per-kind keys).
pub fn source_settings_from_policy(policy: &WorkPolicy) -> toml::Table {
    let mut settings = toml::Table::new();
    let s = |v: &str| toml::Value::String(v.to_owned());
    settings.insert("kind".into(), s(&policy.source_kind));
    match policy.source_kind.as_str() {
        "github" => {
            if let Some(repo) = &policy.github_repo {
                settings.insert("repo".into(), s(repo));
            }
            if let Some(label) = &policy.github_label {
                settings.insert("label".into(), s(label));
            }
            settings.insert("done_action".into(), s(&policy.github_done_action));
            settings.insert(
                "require_triage_receipt".into(),
                toml::Value::Boolean(policy.github_require_triage_receipt),
            );
        }
        "github_ci" => {
            if let Some(repo) = &policy.github_ci_repo {
                settings.insert("repo".into(), s(repo));
            }
            settings.insert("failure_filter".into(), s(&policy.github_ci_failure_filter));
        }
        "github_review" => {
            if let Some(repo) = &policy.github_review_repo {
                settings.insert("repo".into(), s(repo));
            }
        }
        _ => {
            if let Some(tasks_dir) = &policy.tasks_dir {
                settings.insert("tasks_dir".into(), s(&tasks_dir.to_string_lossy()));
            }
        }
    }
    settings
}

/// Construct the [`WorkSource`] a policy describes, routed through
/// `registry` by `policy.source_kind`.
pub fn build_work_source(
    policy: &WorkPolicy,
    registry: &SourceRegistry,
) -> Result<Box<dyn WorkSource>, SourceRegistryError> {
    let graders: Vec<Grader> = policy.default_graders.clone();
    let context =
        SourceBuildContext::new(source_settings_from_policy(policy)).with_default_graders(graders);
    registry.build(&policy.source_kind, &context)
}

/// Resolve the work source for a CLI invocation.
///
/// Precedence: an explicit `--tasks-dir` always selects the directory
/// source; otherwise the policy decides; otherwise the default directory
/// layout applies -- and when even that is absent the invocation fails
/// LOUD instead of rendering an empty queue indistinguishable from a
/// caught-up repo: no policy up the directory tree plus no
/// `.flywheel/tasks` here means this is not a flywheel repo.
pub fn resolve_work_source(
    tasks_dir: Option<&str>,
    policy: Option<&WorkPolicy>,
    registry: &SourceRegistry,
) -> Result<Box<dyn WorkSource>, WorkflowError> {
    if let Some(dir) = tasks_dir.filter(|d| !d.is_empty()) {
        return Ok(Box::new(DirectoryWorkSource::new(dir)));
    }
    if let Some(policy) = policy {
        return Ok(build_work_source(policy, registry)?);
    }
    if !Path::new(DEFAULT_TASKS_DIR).is_dir() {
        let cwd = std::env::current_dir().unwrap_or_default();
        return Err(PolicyError::Invalid(format!(
            "no {DEFAULT_POLICY_FILENAME} found in {} or any parent directory, and no \
             {DEFAULT_TASKS_DIR} here: not a flywheel repo (run from one, or pass --policy / \
             --tasks-dir)",
            cwd.display()
        ))
        .into());
    }
    Ok(Box::new(DirectoryWorkSource::new(DEFAULT_TASKS_DIR)))
}

/// Resolve the SQLite store path with orchestrate's precedence: the
/// explicit `--db` flag, else the policy's `[paths] db`, else the built-in
/// `.flywheel/flywheel.sqlite`. An empty `db` counts as "not specified".
pub fn resolve_db_path(db: Option<&str>, policy: Option<&WorkPolicy>) -> PathBuf {
    if let Some(db) = db.filter(|d| !d.is_empty()) {
        return PathBuf::from(db);
    }
    if let Some(path) = policy.and_then(|p| p.db_path.clone()) {
        return path;
    }
    PathBuf::from(DEFAULT_DB_PATH)
}

/// The tasks dir a directory-only verb (`validate`, `archive`) operates on:
/// the flag, else a directory policy's `tasks_dir`, else the default; a
/// tracker policy is refused with `<verb> applies to directory work sources
/// only`.
pub fn resolve_directory_tasks_dir(
    verb: &str,
    tasks_dir: Option<&str>,
    policy: Option<&WorkPolicy>,
) -> Result<PathBuf, PolicyError> {
    if let Some(dir) = tasks_dir.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    match policy {
        Some(policy) if policy.source_kind == "directory" => Ok(policy
            .tasks_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TASKS_DIR))),
        Some(_) => Err(PolicyError::Invalid(format!(
            "{verb} applies to directory work sources only; the active policy selects a \
             tracker source (pass --tasks-dir to {verb} a directory layout explicitly)"
        ))),
        None => Ok(PathBuf::from(DEFAULT_TASKS_DIR)),
    }
}

/// Absolutize and lexically normalize `path` without requiring it to
/// exist (Python's non-strict `Path.resolve`).
fn resolve_nonstrict(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve the repo root a grader's `run` path tokens are relative to.
///
/// Tasks live under `<repo_root>/.flywheel/tasks`, so the repo root is the
/// ancestor that contains the `.flywheel` directory. A custom `--tasks-dir`
/// outside a `.flywheel` tree falls back to the current working directory.
pub fn repo_root_for_tasks_dir(tasks_dir: &Path) -> PathBuf {
    let resolved = resolve_nonstrict(tasks_dir);
    for parent in resolved.ancestors().skip(1) {
        if parent.file_name().is_some_and(|name| name == ".flywheel")
            && let Some(root) = parent.parent()
        {
            return root.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_default()
}

/// `(repo_root, true_base)` for the phase-prerequisite reachability hold:
/// the configured submit base else `HEAD`, and the `.flywheel` ancestor of
/// the resolved tasks dir (falling back to cwd).
pub fn reachability_context(
    tasks_dir: Option<&str>,
    policy: Option<&WorkPolicy>,
) -> (PathBuf, String) {
    let tasks_dir = match tasks_dir.filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => policy
            .and_then(|p| p.tasks_dir.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TASKS_DIR)),
    };
    let repo_root = repo_root_for_tasks_dir(&tasks_dir);
    let true_base = policy
        .and_then(|p| p.submit_base.clone())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "HEAD".to_owned());
    (repo_root, true_base)
}

/// Task-id -> phase fallback for runs recorded before schema v12: the
/// on-disk scan of a directory layout. Tracker sources have no layout to
/// scan and yield an empty map (those runs render ungrouped).
pub fn resolve_fallback_phases(
    tasks_dir: Option<&str>,
    policy: Option<&WorkPolicy>,
) -> BTreeMap<String, String> {
    if let Some(dir) = tasks_dir.filter(|d| !d.is_empty()) {
        return build_task_phase_index(dir);
    }
    match policy {
        None => build_task_phase_index(DEFAULT_TASKS_DIR),
        Some(policy) if policy.source_kind == "directory" => match &policy.tasks_dir {
            Some(dir) => build_task_phase_index(dir),
            None => BTreeMap::new(),
        },
        Some(_) => BTreeMap::new(),
    }
}

/// The shipped store registries: sqlite (core + claims) and postgres.
pub fn default_store_registries() -> StoreRegistries {
    let mut registries = StoreRegistries::with_sqlite(Arc::new(|path: &Path| {
        SqliteStore::open(path).map(|store| Arc::new(store) as Arc<dyn HarnessStore>)
    }));
    // The fresh registries cannot already hold the postgres name.
    let _ = register_postgres_backends(&mut registries);
    registries
}

/// The [`StoreConfig`] a policy selects for `db_path`.
pub fn store_config_for(policy: Option<&WorkPolicy>, db_path: &Path) -> StoreConfig {
    StoreConfig {
        backend: policy
            .map(|p| p.store_backend.clone())
            .unwrap_or_else(|| DEFAULT_BACKEND.to_owned()),
        db_path: db_path.to_path_buf(),
        schema: policy.and_then(|p| p.store_schema.clone()),
    }
}

/// Open the core store and the claim store the policy selects, bound to
/// `db_path` for the sqlite backend.
pub fn open_stores_for(
    policy: Option<&WorkPolicy>,
    db_path: &Path,
    registries: &StoreRegistries,
    environ: &Environ,
) -> Result<StoreBundle, StoreConfigError> {
    open_stores(registries, &store_config_for(policy, db_path), environ)
}

// --- Rendering helpers -------------------------------------------------------------

/// `json.dumps(value, indent=2)` exactly as Python renders it: two-space
/// nesting, `", "`-free line breaks, empty containers as `[]` / `{}`, and
/// every scalar in Python's default (`ensure_ascii`) spelling.
pub fn py_json_dumps_indent2(value: &Value) -> String {
    let mut out = String::new();
    write_indented(value, 0, &mut out);
    out
}

fn write_indented(value: &Value, depth: usize, out: &mut String) {
    match value {
        Value::Array(items) if !items.is_empty() => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&"  ".repeat(depth + 1));
                write_indented(item, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&"  ".repeat(depth + 1));
                out.push_str(&python_json_dumps(
                    &Value::String(key.clone()),
                    &JsonStyle::PYTHON_DEFAULT,
                ));
                out.push_str(": ");
                write_indented(item, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
            out.push('}');
        }
        other => out.push_str(&python_json_dumps(other, &JsonStyle::PYTHON_DEFAULT)),
    }
}

/// Render a path the way `str(pathlib.Path(p))` does: `.` components
/// dropped, repeated separators collapsed, an empty path as `.`.
pub fn python_path_str(path: &Path) -> String {
    let mut out = PathBuf::new();
    for component in path.components() {
        if !matches!(component, std::path::Component::CurDir) {
            out.push(component.as_os_str());
        }
    }
    if out.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        out.to_string_lossy().into_owned()
    }
}

// --- Injected verbs ----------------------------------------------------------------

/// Everything the `orchestrate` verb resolves before handing off to the
/// loop: the parsed flags, the effective policy, the built work source, the
/// resolved store path, and the sandbox root.
pub struct OrchestrateRequest<'a> {
    pub args: &'a OrchestrateArgs,
    pub policy: Option<&'a WorkPolicy>,
    pub source: Box<dyn WorkSource>,
    pub db_path: PathBuf,
    pub sandbox_root: PathBuf,
}

impl std::fmt::Debug for OrchestrateRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestrateRequest")
            .field("args", self.args)
            .field("policy", &self.policy.is_some())
            .field("db_path", &self.db_path)
            .field("sandbox_root", &self.sandbox_root)
            .finish_non_exhaustive()
    }
}

/// The `orchestrate` loop runner the shell binds: drives the loop over the
/// request and prints its report, returning the exit code.
pub type OrchestrateRunner =
    Arc<dyn Fn(OrchestrateRequest<'_>, &mut dyn Write, &mut dyn Write) -> i32 + Send + Sync>;

/// Everything the `archive` verb resolves before handing off to the sweep:
/// the tasks dir, the repo root, the landing (and true) base, the configured
/// phase-exit gate, the open stores, and the effective policy.
pub struct ArchiveRequest<'a> {
    pub tasks_dir: PathBuf,
    pub repo_root: PathBuf,
    pub landing_base: String,
    pub phase_verify: Option<String>,
    pub store: Arc<dyn HarnessStore>,
    pub claims: Arc<dyn ClaimStore>,
    pub policy: Option<&'a WorkPolicy>,
}

impl std::fmt::Debug for ArchiveRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveRequest")
            .field("tasks_dir", &self.tasks_dir)
            .field("repo_root", &self.repo_root)
            .field("landing_base", &self.landing_base)
            .field("phase_verify", &self.phase_verify)
            .field("policy", &self.policy.is_some())
            .finish_non_exhaustive()
    }
}

/// The `archive` sweep runner the shell binds: runs the sweep (logging
/// refusals to the given sink, which the verb points at stderr) and returns
/// the archive destinations it moved, or a message to print as `error:`.
pub type ArchiveRunner =
    Arc<dyn Fn(ArchiveRequest<'_>, &mut dyn Write) -> Result<Vec<PathBuf>, String> + Send + Sync>;

/// The two verbs whose implementations live outside this module, bound by
/// the product shell. A verb left unbound prints a deterministic
/// `error: <verb> is not available in this build` and exits 2.
#[derive(Clone, Default)]
pub struct Verbs {
    pub orchestrate: Option<OrchestrateRunner>,
    pub archive: Option<ArchiveRunner>,
}

impl std::fmt::Debug for Verbs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verbs")
            .field("orchestrate", &self.orchestrate.is_some())
            .field("archive", &self.archive.is_some())
            .finish()
    }
}

// --- Entry point -------------------------------------------------------------------

/// Run the orchestrator command line against the real stdio and the
/// process environment; returns the exit code.
pub fn main(argv: &[String], verbs: &Verbs) -> i32 {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut host = ProductionInitHost::new();
    let code = run(argv, verbs, &mut host, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}

/// Run the orchestrator command line with injected output sinks and init
/// host; returns the exit code. Parse errors print argparse's usage block
/// and exit 2; `-h` prints help and exits 0.
pub fn run(
    argv: &[String],
    verbs: &Verbs,
    init_host: &mut dyn InitHost,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let command = match parse_args(argv) {
        Ok(Parsed::Command(command)) => command,
        Ok(Parsed::Help(text)) => {
            let _ = out.write_all(text.as_bytes());
            return 0;
        }
        Err(arg_error) => {
            let _ = err.write_all(arg_error.render().as_bytes());
            return 2;
        }
    };
    match dispatch(command, verbs, init_host, out, err) {
        Ok(code) => code,
        Err(failure) => {
            let _ = writeln!(err, "error: {failure}");
            failure.exit_code()
        }
    }
}

fn ensure_db_parent(db_path: &Path) -> Result<(), WorkflowError> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Resolve policy, open the stores, and run one parsed command.
pub fn dispatch(
    command: Command,
    verbs: &Verbs,
    init_host: &mut dyn InitHost,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, WorkflowError> {
    let registries = default_store_registries();
    let sources = default_source_registry();
    let environ: Environ = environ_from_process();
    let policy_of = |p: &Option<String>| -> Result<Option<WorkPolicy>, WorkflowError> {
        Ok(load_effective_policy(p.as_deref().map(Path::new))?)
    };
    match command {
        Command::Init(args) => Ok(cmd_init(&args, init_host, out, err)?),
        Command::Next(common) => {
            let policy = policy_of(&common.policy)?;
            let source =
                resolve_work_source(common.tasks_dir.as_deref(), policy.as_ref(), &sources)?;
            let db_path = resolve_db_path(common.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let items = source.list_work()?;
            let (repo_root, true_base) =
                reachability_context(common.tasks_dir.as_deref(), policy.as_ref());
            cmd_next(
                &items,
                &*stores.core,
                policy.as_ref(),
                Some(&repo_root),
                Some(&true_base),
                out,
            )
        }
        Command::Orchestrate(args) => {
            let Some(runner) = &verbs.orchestrate else {
                writeln!(err, "error: orchestrate is not available in this build")?;
                return Ok(2);
            };
            let policy = policy_of(&args.common.policy)?;
            let source =
                resolve_work_source(args.common.tasks_dir.as_deref(), policy.as_ref(), &sources)?;
            let db_path = resolve_db_path(args.common.db.as_deref(), policy.as_ref());
            let configured: Option<PathBuf> = args
                .sandbox_root
                .as_deref()
                .filter(|r| !r.is_empty())
                .map(PathBuf::from)
                .or_else(|| policy.as_ref().and_then(|p| p.sandbox_root.clone()));
            let cwd = std::env::current_dir()?;
            let sandbox_root = resolve_sandbox_root(configured.as_deref(), &cwd)?;
            let request = OrchestrateRequest {
                args: &args,
                policy: policy.as_ref(),
                source,
                db_path,
                sandbox_root,
            };
            Ok(runner(request, out, err))
        }
        Command::Status(args) => {
            let policy = policy_of(&args.common.policy)?;
            let source =
                resolve_work_source(args.common.tasks_dir.as_deref(), policy.as_ref(), &sources)?;
            let db_path = resolve_db_path(args.common.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let items = source.list_work()?;
            let (repo_root, true_base) =
                reachability_context(args.common.tasks_dir.as_deref(), policy.as_ref());
            cmd_status(
                &args,
                &items,
                &*stores.core,
                &*stores.claims,
                Some(&repo_root),
                Some(&true_base),
                out,
            )
        }
        Command::Live(args) => {
            let policy = policy_of(&args.policy)?;
            let db_path = resolve_db_path(args.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            cmd_live(&args, &*stores.core, out)
        }
        Command::History(args) => {
            let policy = policy_of(&args.common.policy)?;
            let db_path = resolve_db_path(args.common.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let fallback =
                resolve_fallback_phases(args.common.tasks_dir.as_deref(), policy.as_ref());
            cmd_history(&args, &*stores.core, &fallback, out)
        }
        Command::Show(args) => {
            let policy = policy_of(&args.common.policy)?;
            let db_path = resolve_db_path(args.common.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let fallback =
                resolve_fallback_phases(args.common.tasks_dir.as_deref(), policy.as_ref());
            let repo_root = show::show_repo_root(args.common.tasks_dir.as_deref(), policy.as_ref());
            cmd_show(&args, &*stores.core, &repo_root, &fallback, out)
        }
        Command::Archive(common) => {
            let Some(runner) = &verbs.archive else {
                writeln!(err, "error: archive is not available in this build")?;
                return Ok(2);
            };
            let policy = policy_of(&common.policy)?;
            let tasks_dir = resolve_directory_tasks_dir(
                "archive",
                common.tasks_dir.as_deref(),
                policy.as_ref(),
            )?;
            let db_path = resolve_db_path(common.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let repo_root = repo_root_for_tasks_dir(&tasks_dir);
            let landing_base = policy
                .as_ref()
                .and_then(|p| p.submit_base.clone())
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| "HEAD".to_owned());
            let phase_verify = policy.as_ref().and_then(|p| p.phase_verify.clone());
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let request = ArchiveRequest {
                tasks_dir,
                repo_root,
                landing_base,
                phase_verify,
                store: stores.core.clone(),
                claims: stores.claims.clone(),
                policy: policy.as_ref(),
            };
            match runner(request, err) {
                Ok(moved) => {
                    for dest in moved {
                        writeln!(out, "{}", dest.display())?;
                    }
                    Ok(0)
                }
                Err(message) => Err(WorkflowError::Message(message)),
            }
        }
        Command::Validate(common) => {
            let policy = policy_of(&common.policy)?;
            let tasks_dir = resolve_directory_tasks_dir(
                "validate",
                common.tasks_dir.as_deref(),
                policy.as_ref(),
            )?;
            cmd_validate(&tasks_dir, out, err)
        }
        Command::RecheckBlocked(args) => {
            let policy = policy_of(&args.common.policy)?;
            let source =
                resolve_work_source(args.common.tasks_dir.as_deref(), policy.as_ref(), &sources)?;
            let db_path = resolve_db_path(args.common.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let items = source.list_work()?;
            let logs_root = db_path.parent().unwrap_or(Path::new("")).join("logs");
            cmd_recheck_blocked(&args, &items, stores.core.clone(), &logs_root, out, err)
        }
        Command::Recover(args) => {
            let policy = policy_of(&args.policy)?;
            let db_path = resolve_db_path(args.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let logs_root = db_path.parent().unwrap_or(Path::new("")).join("logs");
            cmd_recover(&args, stores.core.clone(), &logs_root, out)
        }
        Command::Resolve(args) => {
            if args.reason.trim().is_empty() {
                writeln!(err, "error: --reason must not be empty")?;
                return Ok(2);
            }
            let policy = policy_of(&args.policy)?;
            let db_path = resolve_db_path(args.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            cmd_resolve(&args, &*stores.claims, chrono::Utc::now(), out, err)
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the verb tests: seeded in-memory stores and task
    //! files laid out the way the directory source expects.

    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use flywheel_core::lifecycle::{Lifecycle, Status};
    use flywheel_core::store::LifecycleStore;
    use flywheel_core::store_memory::InMemoryStore;
    use serde_json::json;

    pub fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 8, 12, 0, 0).unwrap()
    }

    pub fn write_task(path: &Path, task_id: &str, prerequisites: &[&str]) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut payload = json!({
            "id": task_id,
            "goal": format!("Goal for {task_id}."),
            "graders": [{"type": "command", "run": "true"}],
        });
        if !prerequisites.is_empty() {
            payload["prerequisites"] = json!(prerequisites);
        }
        std::fs::write(path, serde_json::to_string(&payload).unwrap()).unwrap();
        path.to_path_buf()
    }

    pub fn write_task_json(path: &Path, payload: &Value) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string(payload).unwrap()).unwrap();
        path.to_path_buf()
    }

    pub fn walk(task_id: &str, run_id: &str, steps: &[Status], error: &str) -> Lifecycle {
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        for (i, status) in steps.iter().enumerate() {
            let err = if status.requires_error() { error } else { "" };
            lc.transition_to(
                *status,
                err,
                Some(t0() + chrono::Duration::seconds(i as i64)),
            )
            .unwrap();
        }
        lc
    }

    pub fn seed_done(store: &InMemoryStore, task_id: &str) -> Lifecycle {
        let lc = walk(
            task_id,
            &format!("run-{task_id}-ok"),
            &[
                Status::Ready,
                Status::Running,
                Status::Validating,
                Status::Done,
            ],
            "",
        );
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    pub fn seed_failed(store: &InMemoryStore, task_id: &str) -> Lifecycle {
        let lc = walk(
            task_id,
            &format!("run-{task_id}-fail"),
            &[Status::Ready, Status::Running, Status::Failed],
            "boom",
        );
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    pub fn seed_running(store: &InMemoryStore, task_id: &str) -> Lifecycle {
        let lc = walk(
            task_id,
            &format!("run-{task_id}-running"),
            &[Status::Ready, Status::Running],
            "",
        );
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    pub fn seed_interrupted(store: &InMemoryStore, task_id: &str) -> Lifecycle {
        let lc = walk(
            task_id,
            &format!("run-{task_id}-interrupted"),
            &[Status::Ready, Status::Running, Status::Interrupted],
            "",
        );
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    pub fn seed_internal_error(store: &InMemoryStore, task_id: &str) -> Lifecycle {
        let lc = walk(
            task_id,
            &format!("run-{task_id}-ierr"),
            &[Status::Ready, Status::Running, Status::InternalError],
            "infra blew up",
        );
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    pub fn seed_blocked(
        store: &InMemoryStore,
        task_id: &str,
        requires: &Value,
        run_id: Option<&str>,
    ) -> Lifecycle {
        let rid = run_id
            .map(str::to_owned)
            .unwrap_or_else(|| format!("run-{task_id}-blocked"));
        let mut lc = walk(
            task_id,
            &rid,
            &[Status::Ready, Status::Running, Status::Interrupted],
            "",
        );
        lc.blocked_requires_json = Some(serde_json::to_string(requires).unwrap());
        store.create_lifecycle(&lc).unwrap();
        lc
    }

    pub fn seed_running_dyn(store: &dyn HarnessStore, task_id: &str) {
        let lc = walk(
            task_id,
            &format!("run-{task_id}-running"),
            &[Status::Ready, Status::Running],
            "",
        );
        store.create_lifecycle(&lc).unwrap();
    }

    pub fn memory_store() -> Arc<InMemoryStore> {
        Arc::new(InMemoryStore::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn json_dumps_matches_python_indent_two() {
        let value = json!({
            "a": [],
            "b": {},
            "c": [1, {"d": "x\u{e9}", "e": null}],
            "f": 0.5,
            "g": true,
        });
        assert_eq!(
            py_json_dumps_indent2(&value),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"d\": \"x\\u00e9\",\n      \"e\": null\n    }\n  ],\n  \"f\": 0.5,\n  \"g\": true\n}"
        );
        assert_eq!(py_json_dumps_indent2(&json!([])), "[]");
        assert_eq!(py_json_dumps_indent2(&json!({})), "{}");
    }

    #[test]
    fn python_path_str_drops_cur_dir_components() {
        assert_eq!(
            python_path_str(Path::new("./.flywheel/tasks/./a.json")),
            ".flywheel/tasks/a.json"
        );
        assert_eq!(python_path_str(Path::new("")), ".");
        assert_eq!(python_path_str(Path::new("a//b/")), "a/b");
    }

    #[test]
    fn db_path_precedence_is_flag_then_policy_then_default() {
        let policy = WorkPolicy {
            db_path: Some(PathBuf::from("custom.sqlite")),
            ..WorkPolicy::default()
        };
        assert_eq!(
            resolve_db_path(Some("flag.sqlite"), Some(&policy)),
            PathBuf::from("flag.sqlite")
        );
        assert_eq!(
            resolve_db_path(Some(""), Some(&policy)),
            PathBuf::from("custom.sqlite")
        );
        assert_eq!(resolve_db_path(None, None), PathBuf::from(DEFAULT_DB_PATH));
    }

    #[test]
    fn repo_root_is_the_dot_flywheel_ancestor_or_cwd() {
        let tmp = TempDir::new().unwrap();
        let tasks = tmp.path().join(".flywheel").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        assert_eq!(
            repo_root_for_tasks_dir(&tasks),
            tmp.path().canonicalize().unwrap()
        );
        let elsewhere = tmp.path().join("plain");
        assert_eq!(
            repo_root_for_tasks_dir(&elsewhere),
            std::env::current_dir().unwrap()
        );
    }

    #[test]
    fn directory_tasks_dir_refuses_tracker_policies() {
        let policy = WorkPolicy {
            source_kind: "github".into(),
            ..WorkPolicy::default()
        };
        let err = resolve_directory_tasks_dir("validate", None, Some(&policy)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "validate applies to directory work sources only; the active policy selects a tracker source (pass --tasks-dir to validate a directory layout explicitly)"
        );
        assert_eq!(
            resolve_directory_tasks_dir("validate", Some("x"), Some(&policy)).unwrap(),
            PathBuf::from("x")
        );
        assert_eq!(
            resolve_directory_tasks_dir("archive", None, None).unwrap(),
            PathBuf::from(DEFAULT_TASKS_DIR)
        );
    }

    #[test]
    fn source_settings_carry_the_per_kind_keys() {
        let policy = WorkPolicy {
            source_kind: "github".into(),
            github_repo: Some("o/r".into()),
            github_label: Some("l".into()),
            ..WorkPolicy::default()
        };
        let settings = source_settings_from_policy(&policy);
        assert_eq!(settings["kind"].as_str(), Some("github"));
        assert_eq!(settings["repo"].as_str(), Some("o/r"));
        assert_eq!(settings["done_action"].as_str(), Some("comment"));
        assert_eq!(settings["require_triage_receipt"].as_bool(), Some(false));
        let source = build_work_source(&policy, &default_source_registry()).unwrap();
        assert!(source.source_name().contains("o/r"));
    }

    #[test]
    fn parse_failures_render_usage_and_exit_two() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut host = init::test_host::ScriptedHost::default();
        let code = run(
            &["bogus".to_owned()],
            &Verbs::default(),
            &mut host,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 2);
        assert!(out.is_empty());
        assert!(
            String::from_utf8(err)
                .unwrap()
                .starts_with("usage: flywheel [-h]\n")
        );
        let mut out = Vec::new();
        let code = run(
            &["-h".to_owned()],
            &Verbs::default(),
            &mut host,
            &mut out,
            &mut Vec::new(),
        );
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(out).unwrap(), args::TOP_HELP);
    }

    /// Serializes the tests that move the process working directory.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn argv(parts: &[String]) -> Vec<String> {
        parts.to_vec()
    }

    fn run_capture(parts: &[String]) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut host = init::test_host::ScriptedHost::default();
        let code = run(parts, &Verbs::default(), &mut host, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// A directory-source policy pinned to `tasks_dir` and a sqlite store at
    /// `db`, so the verbs never auto-detect this repository's own policy.
    fn write_policy(root: &Path, tasks_dir: &Path, db: &Path) -> PathBuf {
        let policy = root.join("flywheel.toml");
        std::fs::write(
            &policy,
            format!(
                "[source]\nkind = \"directory\"\ntasks_dir = {}\n\n[paths]\ndb = {}\n",
                serde_json::to_string(&tasks_dir.display().to_string()).unwrap(),
                serde_json::to_string(&db.display().to_string()).unwrap(),
            ),
        )
        .unwrap();
        policy
    }

    #[test]
    fn main_next_status_and_recover_round_trip_through_a_sqlite_store() {
        let tmp = TempDir::new().unwrap();
        let tasks = tmp.path().join("tasks");
        let written = test_support::write_task(&tasks.join("active/01-phase/a.json"), "a", &[]);
        test_support::write_task(&tasks.join("active/01-phase/b.json"), "b", &["a"]);
        let db = tmp.path().join("state").join("db.sqlite");
        let policy = write_policy(tmp.path(), &tasks, &db);
        let common = |verb: &str| -> Vec<String> {
            argv(&[
                verb.to_owned(),
                "--policy".to_owned(),
                policy.display().to_string(),
            ])
        };

        let (code, out, err) = run_capture(&common("next"));
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out.trim(), written.display().to_string());
        assert!(
            db.is_file(),
            "the sqlite store is created under the db parent"
        );

        let store = default_store_registries();
        let bundle = open_stores_for(None, &db, &store, &Environ::new()).unwrap();
        test_support::seed_running_dyn(&*bundle.core, "a");
        drop(bundle);

        let mut status_json = common("status");
        status_json.push("--json".to_owned());
        let (code, out, _) = run_capture(&status_json);
        assert_eq!(code, 0);
        let payload: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(payload[0]["task_id"], "a");
        assert_eq!(payload[0]["state"], "in_progress");
        assert_eq!(payload[1]["prerequisites"], serde_json::json!(["a"]));

        let (code, out, _) = run_capture(&common("recover"));
        assert_eq!(code, 0);
        assert_eq!(out, "run-a-running\n");
        assert!(
            db.parent()
                .unwrap()
                .join("logs/runs/run-a-running.jsonl")
                .is_file()
        );

        let (code, out, _) = run_capture(&common("status"));
        assert_eq!(code, 0);
        assert!(out.contains("  01-phase/a  interrupted      \n"));

        let (code, out, _) = run_capture(&common("live"));
        assert_eq!((code, out.as_str()), (0, "(no in-flight runs)\n"));
        let (code, out, _) = run_capture(&common("history"));
        assert_eq!((code, out.as_str()), (0, "(no finished runs)\n"));
        let (code, out, _) = run_capture(&common("validate"));
        assert_eq!((code, out.as_str()), (0, "All 2 active task(s) valid.\n"));
        let mut show = common("show");
        show.push("nope".to_owned());
        let (code, out, _) = run_capture(&show);
        assert_eq!(
            (code, out.as_str()),
            (1, "nope: no run or task with that id\n")
        );
        let mut resolve = common("resolve");
        resolve.extend(["a".to_owned(), "--reason".to_owned(), "x".to_owned()]);
        let (code, _, err) = run_capture(&resolve);
        assert_eq!(code, 1);
        assert_eq!(
            err,
            "error: task 'a' has no unresolved stop event to abandon (no stop event)\n"
        );
    }

    #[test]
    fn status_walks_up_to_the_policy_or_fails_loud_outside_a_repo() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::current_dir().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("repo");
        test_support::write_task(
            &root.join(".flywheel/tasks/active/01-phase/t1.json"),
            "t1",
            &[],
        );
        std::fs::write(
            root.join("flywheel.toml"),
            "[source]\nkind = \"directory\"\ntasks_dir = \".flywheel/tasks\"\n",
        )
        .unwrap();
        let sub = root.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        std::env::set_current_dir(&sub).unwrap();
        let (code, out, err) = run_capture(&argv(&["status".to_owned()]));
        std::env::set_current_dir(&original).unwrap();
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.contains("01-phase/t1"));
        assert!(!out.contains("(no active tasks)"));

        let bare = tmp.path().join("not-a-repo");
        std::fs::create_dir_all(&bare).unwrap();
        std::env::set_current_dir(&bare).unwrap();
        let (code, out, err) = run_capture(&argv(&["status".to_owned()]));
        std::env::set_current_dir(&original).unwrap();
        assert_eq!(code, 2);
        assert!(err.contains("not a flywheel repo"), "{err}");
        assert!(!out.contains("(no active tasks)"));
    }

    #[test]
    fn unbound_injected_verbs_fail_deterministically() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("active")).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut host = init::test_host::ScriptedHost::default();
        let argv: Vec<String> = ["archive", "--tasks-dir"]
            .iter()
            .map(|s| (*s).to_owned())
            .chain([tmp.path().display().to_string()])
            .chain([
                "--db".to_owned(),
                tmp.path().join("db.sqlite").display().to_string(),
            ])
            .collect();
        let code = run(&argv, &Verbs::default(), &mut host, &mut out, &mut err);
        assert_eq!(code, 2);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "error: archive is not available in this build\n"
        );
    }
}
