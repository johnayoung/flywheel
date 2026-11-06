//! The orchestrator's operator verbs (`next`, `status`, `live`, `history`,
//! `show`, `validate`, `recheck-blocked`, `recover`, `resolve`, `init`) and
//! the argparse-identical entry point the product shell calls in-process.
//!
//! Every verb is a library function taking parsed arguments, already-open
//! stores, and the two output sinks, and returning the process exit code.
//! [`main`] / [`run`] wire them to the command line: resolve the effective
//! policy (the home project's built-in defaults plus the effective config),
//! build the work
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

use std::collections::{BTreeMap, BTreeSet};
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
pub use crate::config::Access;
use crate::config::{ConfigError, ConfigFlags, EffectiveConfig, Interaction, load_config};
use crate::github::register_github_sources;
use crate::history::build_task_phase_index;
use crate::home;
use crate::policy::{
    LEGACY_POLICY_FILENAME, PolicyError, ProjectBinding, WorkPolicy, resolve_sandbox_root,
};
use crate::source_registry::{SourceBuildContext, SourceRegistry, SourceRegistryError};
use crate::sources::{DirectoryWorkSource, WorkSource, WorkSourceError};
use crate::store_factory::{StoreBundle, StoreConfigError, open_stores};
use crate::store_registry::{
    DEFAULT_BACKEND, DEFAULT_PARTITION, Environ, StoreConfig, StoreRegistries, environ_from_process,
};

pub use args::{
    ArgError, Command, CommonArgs, HistoryArgs, InitArgs, LiveArgs, OrchestrateArgs, Parsed,
    RecheckBlockedArgs, RecoverArgs, ResolveArgs, ShowArgs, StatusArgs, parse_args,
};
pub use init::{
    DetectedVerify, InitHost, ProductionInitHost, cmd_init, detect_verify,
    render_suggested_repo_file,
};
pub use live::{LiveRunRow, cmd_live, cmd_live_until, collect_live_rows, format_live_line};
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
/// exit 2 surface; store-side failures exit
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

/// Load the policy the orchestrator verbs honor for an invocation: the
/// working directory's home project ([`home_project_policy`]). Every
/// default (store, tasks, worktrees, logs) lives under
/// `$FLYWHEEL_HOME/projects/<id>/`, the effective config
/// ([`crate::config`]) is applied, and a directory outside any git repo is
/// refused. `access` says whether this verb runs repo-file commands (an
/// untrusted `.flywheel.toml` is then refused or prompted for) or only
/// reads (it is then warned about and ignored). On success the result is
/// always `Some`; the `Option` keeps the signature callers match on.
pub fn load_effective_policy(access: Access) -> Result<Option<WorkPolicy>, PolicyError> {
    home_project_policy(access).map(Some)
}

/// Print `line` on stderr once per process (repeated policy loads in one
/// invocation must not repeat a notice).
pub fn notice_once(line: &str) {
    static SHOWN: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
    let mut shown = SHOWN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if shown.insert(line.to_owned()) {
        eprintln!("{line}");
    }
}

/// The one-line notice for a repo still carrying the old layout
/// (`flywheel.toml`, `.flywheel/`), or `None`. The flywheel home itself is
/// never mistaken for the old `.flywheel/` (a home-dir repo holds it).
pub fn legacy_layout_notice(repo_root: &Path, home: &home::Home) -> Option<String> {
    let mut found = Vec::new();
    if repo_root.join(LEGACY_POLICY_FILENAME).is_file() {
        found.push(LEGACY_POLICY_FILENAME.to_owned());
    }
    let dot_dir = repo_root.join(".flywheel");
    let is_home = match (dot_dir.canonicalize(), home.root().canonicalize()) {
        (Ok(dir), Ok(root)) => dir == root,
        _ => false,
    };
    if dot_dir.is_dir() && !is_home {
        found.push(".flywheel/".to_owned());
    }
    if found.is_empty() {
        return None;
    }
    let (names, verb) = match found.as_slice() {
        [one] => (one.clone(), "is"),
        _ => (found.join(" and "), "are"),
    };
    Some(format!(
        "fw: notice: {names} in {} {verb} ignored (state lives under $FLYWHEEL_HOME; repo \
         settings go in {}); {} can be deleted",
        repo_root.display(),
        crate::config::REPO_CONFIG_FILENAME,
        if found.len() == 1 { "it" } else { "they" },
    ))
}

/// Print [`legacy_layout_notice`] for `repo_root`, once.
pub fn notice_legacy_layout(repo_root: &Path, home: &home::Home) {
    if let Some(line) = legacy_layout_notice(repo_root, home) {
        notice_once(&line);
    }
}

/// Load the effective config for `project` at `repo_root`, prompting on the
/// terminal when `access` executes repo-file commands and stdin is a TTY
/// (unattended otherwise). Warnings are printed once on stderr.
pub fn load_project_config(
    home: &home::Home,
    project: &home::Project,
    repo_root: &Path,
    access: Access,
    flags: &ConfigFlags,
) -> Result<EffectiveConfig, ConfigError> {
    use std::io::IsTerminal;
    let env_dsn = crate::config::env_dsn_from_process();
    let stdin = std::io::stdin();
    let loaded = if access == Access::Execute && stdin.is_terminal() {
        let mut input = stdin.lock();
        let mut output = std::io::stderr();
        load_config(
            home,
            project,
            repo_root,
            access,
            flags,
            env_dsn,
            Interaction::Prompt {
                input: &mut input,
                output: &mut output,
            },
        )?
    } else {
        load_config(
            home,
            project,
            repo_root,
            access,
            flags,
            env_dsn,
            Interaction::Unattended,
        )?
    };
    for warning in &loaded.warnings {
        notice_once(warning);
    }
    Ok(loaded.config)
}

/// The policy for the working directory's home project.
///
/// Resolves (creating on first sight) the project of the git repo holding
/// the working directory, RE-ANCHORS the process at the current worktree's
/// top level (so a subdirectory invocation behaves like a repo-root one),
/// notes an ignored legacy layout, loads the effective config, binds the
/// project dir its `project_id` names (the local id unless a config file
/// overrides it), creates the tasks dir, and returns the default policy
/// with the store, tasks dir, and worktree root under that project dir and
/// the config applied. Nothing is written into the repo tree except the
/// `.git/config` id.
pub fn home_project_policy(access: Access) -> Result<WorkPolicy, PolicyError> {
    let home = home::Home::from_env()?;
    let cwd = std::env::current_dir().map_err(|err| PolicyError::Read {
        path: PathBuf::from("."),
        message: format!("cannot resolve the working directory: {err}"),
    })?;
    let project = home::resolve_project(&home, &cwd)?;
    let repo_root = home::worktree_toplevel(&cwd)?;
    if cwd.canonicalize().ok().as_deref() != Some(repo_root.as_path()) {
        std::env::set_current_dir(&repo_root).map_err(|err| PolicyError::Read {
            path: repo_root.clone(),
            message: format!("cannot re-anchor to {}: {err}", repo_root.display()),
        })?;
    }
    notice_legacy_layout(&repo_root, &home);
    let config = load_project_config(&home, &project, &repo_root, access, &ConfigFlags::default())?;
    let bound = home::bind_project(&home, &project, &config.project_id.value)?;
    let mut policy = policy_for_project(&bound, repo_root)?;
    config.apply_to_policy(&mut policy);
    Ok(policy)
}

/// Whether `folder` belongs to a project flywheel already knows, without
/// minting an id or writing anything: its git repo carries a project id
/// (`.git/config`, or a `project_id` in its `.flywheel.toml`) whose
/// project dir exists under `home`. The hub's Flywheel panel shows exactly
/// these folders.
pub fn known_project(home: &home::Home, folder: &Path) -> Option<String> {
    let declared = home::worktree_toplevel(folder)
        .ok()
        .and_then(|root| crate::config::declared_project_id(&root));
    let stored = home::stored_project_id(folder).ok().flatten();
    [declared, stored]
        .into_iter()
        .flatten()
        .find(|id| home.project(id).root().is_dir())
}

/// The policy for the project of the git repo holding `folder`, resolved
/// without touching the process (no cwd change, no prompt, no stderr):
/// the cwd-free twin of [`home_project_policy`] for a long-lived process
/// serving many repos (the hub). The repo file is read with `access`
/// (an untrusted one is ignored for [`Access::ReadOnly`]); config
/// warnings are returned, not printed.
pub fn project_policy_at(
    home: &home::Home,
    folder: &Path,
    access: Access,
) -> Result<(WorkPolicy, Vec<String>), PolicyError> {
    let project = home::resolve_project(home, folder)?;
    let repo_root = home::worktree_toplevel(folder)?;
    let loaded = load_config(
        home,
        &project,
        &repo_root,
        access,
        &ConfigFlags::default(),
        crate::config::env_dsn_from_process(),
        Interaction::Unattended,
    )?;
    let bound = home::bind_project(home, &project, &loaded.config.project_id.value)?;
    let mut policy = policy_for_project(&bound, repo_root)?;
    loaded.config.apply_to_policy(&mut policy);
    Ok((policy, loaded.warnings))
}

/// The default policy bound to `project`, its tasks dir created.
pub fn policy_for_project(
    project: &home::Project,
    repo_root: PathBuf,
) -> Result<WorkPolicy, home::ProjectError> {
    let paths = &project.paths;
    let active = paths.tasks().join("active");
    std::fs::create_dir_all(&active).map_err(|err| home::ProjectError::Io {
        path: active.clone(),
        message: err.to_string(),
    })?;
    Ok(WorkPolicy {
        source_kind: "directory".to_owned(),
        tasks_dir: Some(paths.tasks()),
        db_path: Some(paths.db()),
        sandbox_root: Some(paths.worktrees()),
        project: Some(ProjectBinding {
            id: project.id.clone(),
            dir: paths.root().to_path_buf(),
            repo_root,
        }),
        ..WorkPolicy::default()
    })
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
        "directory" => {
            if let Some(tasks_dir) = &policy.tasks_dir {
                settings.insert("tasks_dir".into(), s(&tasks_dir.to_string_lossy()));
            }
        }
        // Any other kind carries only `kind`: the registry rejects a name it
        // does not know, so an unknown kind can never be built as a
        // directory source.
        _ => {}
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
            "no {DEFAULT_TASKS_DIR} in {}: run inside a git repo (its project's tasks live \
             under $FLYWHEEL_HOME), or pass --tasks-dir",
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

/// The repo root for `tasks_dir`: a home project's tasks live outside the
/// repo, so a home-bound policy answers with its recorded repo root;
/// otherwise [`repo_root_for_tasks_dir`].
pub fn repo_root_for(tasks_dir: &Path, policy: Option<&WorkPolicy>) -> PathBuf {
    if let Some(project) = policy.and_then(|p| p.project.as_ref())
        && policy.and_then(|p| p.tasks_dir.as_deref()) == Some(tasks_dir)
    {
        return project.repo_root.clone();
    }
    repo_root_for_tasks_dir(tasks_dir)
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
    let repo_root = repo_root_for(&tasks_dir, policy);
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
    let mut registries = StoreRegistries::with_sqlite(Arc::new(|path: &Path, partition: &str| {
        SqliteStore::open_partition(path, partition)
            .map(|store| Arc::new(store) as Arc<dyn HarnessStore>)
    }));
    // The fresh registries cannot already hold the postgres name.
    let _ = register_postgres_backends(&mut registries);
    registries
}

/// The partition key (`project_id` column) the stores of `policy` are
/// opened with: the bound home project's id (the config `project_id`
/// override, else the repo's `.git/config` id) when the invocation resolved
/// one, else [`DEFAULT_PARTITION`] -- a `--db` + `--tasks-dir` pair that
/// skips project resolution.
pub fn store_partition(policy: Option<&WorkPolicy>) -> String {
    policy.and_then(|p| p.project.as_ref()).map_or_else(
        || DEFAULT_PARTITION.to_owned(),
        |project| project.id.clone(),
    )
}

/// The [`StoreConfig`] a policy selects for `db_path`.
pub fn store_config_for(policy: Option<&WorkPolicy>, db_path: &Path) -> StoreConfig {
    StoreConfig {
        backend: policy
            .map(|p| p.store_backend.clone())
            .unwrap_or_else(|| DEFAULT_BACKEND.to_owned()),
        db_path: db_path.to_path_buf(),
        schema: None,
        dsn: policy.and_then(|p| p.store_dsn.clone()),
        project_id: store_partition(policy),
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
    // Explicit flags that cover every path a verb reads skip the home
    // project, so `--db` / `--tasks-dir` keep working outside a git repo.
    let given = |v: &Option<String>| v.as_deref().is_some_and(|v| !v.is_empty());
    // Verbs that run repo-file commands (the orchestrate loop, the archive
    // sweep's phase-exit verify) need a trusted `.flywheel.toml`; the rest
    // only read.
    let access = match &command {
        Command::Orchestrate(_) | Command::Archive(_) => Access::Execute,
        _ => Access::ReadOnly,
    };
    let policy_of = |explicit: bool| -> Result<Option<WorkPolicy>, WorkflowError> {
        if explicit {
            return Ok(None);
        }
        Ok(load_effective_policy(access)?)
    };
    match command {
        Command::Init(args) => Ok(cmd_init(&args, init_host, out, err)?),
        Command::Next(common) => {
            let policy = policy_of(given(&common.db) && given(&common.tasks_dir))?;
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
            let policy = policy_of(given(&args.common.db) && given(&args.common.tasks_dir))?;
            let source =
                resolve_work_source(args.common.tasks_dir.as_deref(), policy.as_ref(), &sources)?;
            let db_path = resolve_db_path(args.common.db.as_deref(), policy.as_ref());
            let configured: Option<PathBuf> = args
                .sandbox_root
                .as_deref()
                .filter(|r| !r.is_empty())
                .map(PathBuf::from);
            let cwd = std::env::current_dir()?;
            let Some(default_root) = policy
                .as_ref()
                .and_then(|p| p.sandbox_root.clone())
                .or_else(|| configured.clone())
            else {
                return Err(WorkflowError::Message(
                    "orchestrate needs --sandbox-root when it runs outside a git repo".to_owned(),
                ));
            };
            let sandbox_root = resolve_sandbox_root(configured.as_deref(), &cwd, &default_root);
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
            let policy = policy_of(given(&args.common.db) && given(&args.common.tasks_dir))?;
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
            let policy = policy_of(given(&args.db))?;
            let db_path = resolve_db_path(args.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            cmd_live(&args, &*stores.core, out)
        }
        Command::History(args) => {
            let policy = policy_of(given(&args.common.db))?;
            let db_path = resolve_db_path(args.common.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let fallback =
                resolve_fallback_phases(args.common.tasks_dir.as_deref(), policy.as_ref());
            cmd_history(&args, &*stores.core, &fallback, out)
        }
        Command::Show(args) => {
            let policy = policy_of(given(&args.common.db))?;
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
            let policy = policy_of(given(&common.db) && given(&common.tasks_dir))?;
            let tasks_dir = resolve_directory_tasks_dir(
                "archive",
                common.tasks_dir.as_deref(),
                policy.as_ref(),
            )?;
            let db_path = resolve_db_path(common.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let repo_root = repo_root_for(&tasks_dir, policy.as_ref());
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
            let policy = policy_of(given(&common.tasks_dir))?;
            let tasks_dir = resolve_directory_tasks_dir(
                "validate",
                common.tasks_dir.as_deref(),
                policy.as_ref(),
            )?;
            let repo_root = repo_root_for(&tasks_dir, policy.as_ref());
            cmd_validate(&tasks_dir, &repo_root, out, err)
        }
        Command::RecheckBlocked(args) => {
            let policy = policy_of(given(&args.common.db) && given(&args.common.tasks_dir))?;
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
            let policy = policy_of(given(&args.db))?;
            let db_path = resolve_db_path(args.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            let logs_root = db_path.parent().unwrap_or(Path::new("")).join("logs");
            cmd_recover(
                &args,
                &stores.core,
                &*stores.claims,
                chrono::Utc::now(),
                &logs_root,
                out,
                err,
            )
        }
        Command::Resolve(args) => {
            if args.reason.trim().is_empty() {
                writeln!(err, "error: --reason must not be empty")?;
                return Ok(2);
            }
            let policy = policy_of(given(&args.db))?;
            let db_path = resolve_db_path(args.db.as_deref(), policy.as_ref());
            ensure_db_parent(&db_path)?;
            let stores = open_stores_for(policy.as_ref(), &db_path, &registries, &environ)?;
            cmd_resolve(
                &args,
                &*stores.core,
                &*stores.claims,
                chrono::Utc::now(),
                out,
                err,
            )
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
    fn unknown_source_kind_is_an_error_never_a_directory_fallback() {
        // `fw worker --source` rejects it outright...
        let err = crate::policy::RunFlags {
            source: Some("jira".into()),
            ..crate::policy::RunFlags::default()
        }
        .apply(&mut WorkPolicy::default())
        .unwrap_err()
        .to_string();
        assert!(err.contains("--source must be one of"), "{err}");
        // ...and a directly constructed policy fails at the registry.
        let policy = WorkPolicy {
            source_kind: "jira".into(),
            tasks_dir: Some(PathBuf::from("tasks")),
            ..WorkPolicy::default()
        };
        let settings = source_settings_from_policy(&policy);
        assert_eq!(settings.len(), 1, "{settings:?}");
        let Err(err) = build_work_source(&policy, &default_source_registry()) else {
            panic!("an unknown source kind built a work source");
        };
        assert!(err.to_string().contains("jira"), "{err}");
        let Err(err) = resolve_work_source(None, Some(&policy), &default_source_registry()) else {
            panic!("an unknown source kind resolved a work source");
        };
        assert!(err.to_string().contains("jira"), "{err}");
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

    /// The explicit flags `verb` accepts that pin it to `tasks_dir` and a
    /// sqlite store at `db` (explicit paths skip the home project, so the
    /// verbs never touch a real `$FLYWHEEL_HOME`).
    fn pinned(verb: &str, tasks_dir: &Path, db: &Path) -> Vec<String> {
        let mut argv = vec![verb.to_owned()];
        if matches!(verb, "next" | "status" | "history" | "show" | "validate") {
            argv.extend(["--tasks-dir".to_owned(), tasks_dir.display().to_string()]);
        }
        if verb != "validate" {
            argv.extend(["--db".to_owned(), db.display().to_string()]);
        }
        argv
    }

    #[test]
    fn main_next_status_and_recover_round_trip_through_a_sqlite_store() {
        let tmp = TempDir::new().unwrap();
        let tasks = tmp.path().join("tasks");
        let written = test_support::write_task(&tasks.join("active/01-phase/a.json"), "a", &[]);
        test_support::write_task(&tasks.join("active/01-phase/b.json"), "b", &["a"]);
        let db = tmp.path().join("state").join("db.sqlite");
        let common = |verb: &str| -> Vec<String> { pinned(verb, &tasks, &db) };

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

    /// `recover` is the operator's verb for a run whose worker died or lost
    /// its lease. A running lifecycle whose task claim is still held by a
    /// live worker is that worker's in-flight run, not a strand: recover
    /// must leave it alone (and say so) rather than finalize it out from
    /// under the worker. A lapsed lease is recovered, and recover's own
    /// bookkeeping claim is released afterwards.
    #[test]
    fn recover_skips_a_run_whose_task_claim_a_live_worker_holds() {
        let tmp = TempDir::new().unwrap();
        let tasks = tmp.path().join("tasks");
        test_support::write_task(&tasks.join("active/01-phase/a.json"), "a", &[]);
        test_support::write_task(&tasks.join("active/01-phase/b.json"), "b", &[]);
        let db = tmp.path().join("state").join("db.sqlite");
        let recover = pinned("recover", &tasks, &db);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let registries = default_store_registries();
        let bundle = open_stores_for(None, &db, &registries, &Environ::new()).unwrap();
        test_support::seed_running_dyn(&*bundle.core, "a");
        test_support::seed_running_dyn(&*bundle.core, "b");
        let now = chrono::Utc::now();
        let keys = std::collections::BTreeSet::new();
        bundle
            .claims
            .acquire_claim("a", "worker-live", now, 3600.0, &keys)
            .unwrap()
            .unwrap();
        bundle
            .claims
            .acquire_claim(
                "b",
                "worker-dead",
                now - chrono::Duration::seconds(600),
                60.0,
                &keys,
            )
            .unwrap()
            .unwrap();
        drop(bundle);

        let (code, out, err) = run_capture(&recover);
        assert_eq!(code, 0);
        assert_eq!(out, "run-b-running\n");
        assert_eq!(
            err,
            "skipped run-a-running: task 'a' is claimed by live worker 'worker-live'; \
             a live run is not stranded\n"
        );

        let bundle = open_stores_for(None, &db, &registries, &Environ::new()).unwrap();
        let status = |run_id: &str| bundle.core.load_lifecycle(run_id).unwrap().unwrap().status;
        assert_eq!(
            status("run-a-running"),
            flywheel_core::lifecycle::Status::Running
        );
        assert_eq!(
            status("run-b-running"),
            flywheel_core::lifecycle::Status::Interrupted
        );
        assert_eq!(
            bundle.claims.load_claim("a").unwrap().unwrap().worker_id,
            "worker-live"
        );
        assert!(
            bundle.claims.load_claim("b").unwrap().is_none(),
            "recover releases the claim it took to finalize the strand"
        );
    }

    #[test]
    fn status_never_reads_a_legacy_policy_and_refuses_outside_a_git_repo() {
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
        // The removed --policy flag is an unrecognized argument.
        let (code, _, err) = run_capture(&argv(&[
            "status".to_owned(),
            "--policy".to_owned(),
            root.join("flywheel.toml").display().to_string(),
        ]));
        assert_eq!(code, 2);
        assert!(err.contains("unrecognized arguments: --policy"), "{err}");

        // A flywheel.toml is never auto-detected (here or in a parent): the
        // directory is not a git repo, so the home project refuses.
        for dir in [root.clone(), root.join("src").join("deep")] {
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_current_dir(&dir).unwrap();
            let (code, out, err) = run_capture(&argv(&["status".to_owned()]));
            std::env::set_current_dir(&original).unwrap();
            assert_eq!(code, 2);
            assert!(err.contains("not inside a git repository"), "{err}");
            assert!(!out.contains("01-phase/t1"));
        }
    }

    fn git_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn known_project_reads_without_minting_and_project_policy_is_cwd_free() {
        let tmp = TempDir::new().unwrap();
        let home = home::Home::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        git_repo(&repo);
        assert_eq!(known_project(&home, &repo), None);
        assert_eq!(
            home::stored_project_id(&repo).unwrap(),
            None,
            "nothing minted"
        );
        assert!(!home.root().exists(), "nothing written to the home");

        let cwd = std::env::current_dir().unwrap();
        let (policy, warnings) = project_policy_at(&home, &repo, Access::ReadOnly).unwrap();
        assert_eq!(std::env::current_dir().unwrap(), cwd, "the cwd never moves");
        assert!(warnings.is_empty(), "{warnings:?}");
        let project = policy.project.clone().unwrap();
        let paths = project.paths();
        assert_eq!(known_project(&home, &repo), Some(project.id.clone()));
        assert_eq!(policy.tasks_dir, Some(paths.tasks()));
        assert_eq!(policy.db_path, Some(paths.db()));
        assert_eq!(policy.sandbox_root, Some(paths.worktrees()));
        assert_eq!(policy.held_out_root(), Some(paths.held_out()));
        assert!(paths.tasks().join("active").is_dir());

        // A project id another clone declared is known once its dir exists.
        let other = tmp.path().join("other");
        git_repo(&other);
        std::fs::write(
            other.join(crate::config::REPO_CONFIG_FILENAME),
            format!("project_id = \"{}\"\n", project.id),
        )
        .unwrap();
        assert_eq!(known_project(&home, &other), Some(project.id));
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
