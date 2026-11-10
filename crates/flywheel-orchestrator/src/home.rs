//! The flywheel home and project identity.
//!
//! Every byte flywheel writes lives under one home: `$FLYWHEEL_HOME`, else
//! `~/.flywheel`. Paths are composed by hand (no platform-dirs crate: macOS
//! would land in `~/Library/Application Support`). This module is the single
//! source for every home path:
//!
//! ```text
//! $FLYWHEEL_HOME/config.toml              user config
//! $FLYWHEEL_HOME/trust.json               trust approvals
//! $FLYWHEEL_HOME/hub/                     hub state
//!   claude-accounts/<name>.token          Claude account tokens (`fw worker --account`)
//! $FLYWHEEL_HOME/projects/<id>/
//!   meta.json                             path, git common dir, remote, first/last seen
//!   config.toml                           personal per-project overrides
//!   flywheel.sqlite                       default store
//!   specs/ tasks/                         authored assets
//!   transcripts/ worktrees/ held-out/ logs/
//! ```
//!
//! A project is a git repository, identified through its git common dir so
//! every linked worktree maps to one project. The id is a UUID v4 stored in
//! the repo's common `.git/config` under `flywheel.projectid`. A moved repo
//! keeps its id (meta.json's path follows it); a copy -- the recorded common
//! dir still exists and carries the same id -- mints a fresh id for the copy.
//! A directory outside any git repository is refused.
//!
//! `flywheel-hub` cannot depend on this crate; it computes its default state
//! dir with the same rule ([`home_from`]), and a test in the `flywheel` crate
//! pins the two together.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// The environment variable naming the flywheel home.
pub const HOME_ENV: &str = "FLYWHEEL_HOME";
/// The Claude account token directory under the hub state dir (mirrors
/// `flywheel_hub::accounts::DIR`; a test in the `flywheel` crate pins them).
pub const CLAUDE_ACCOUNTS_DIRNAME: &str = "claude-accounts";
/// The home directory name under `$HOME` when [`HOME_ENV`] is unset.
pub const DEFAULT_HOME_DIRNAME: &str = ".flywheel";
/// The git config key (in the repo's common config) holding the project id.
pub const PROJECT_ID_KEY: &str = "flywheel.projectid";
/// The per-project metadata file name.
pub const META_FILENAME: &str = "meta.json";

/// Anything resolving the home or a project can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectError {
    /// Neither `$FLYWHEEL_HOME` nor `$HOME` is usable.
    #[error("cannot locate the flywheel home: set ${HOME_ENV} or $HOME")]
    NoHome,
    /// The directory is not inside a git repository.
    #[error(
        "{} is not inside a git repository: flywheel identifies a project by its git repo \
         (run from a git checkout, or `git init` one)",
        dir.display()
    )]
    NotAGitRepo { dir: PathBuf },
    /// A git invocation failed.
    #[error("git {command} failed in {}: {message}", dir.display())]
    Git {
        command: String,
        dir: PathBuf,
        message: String,
    },
    /// The repo carries a `flywheel.projectid` that is not a UUID.
    #[error(
        "{}: git config {PROJECT_ID_KEY} = {value:?} is not a UUID (fix or unset it, or run \
         `fw project relink`)",
        config.display()
    )]
    MalformedId { config: PathBuf, value: String },
    /// A home file could not be read or written.
    #[error("{}: {message}", path.display())]
    Io { path: PathBuf, message: String },
    /// A user-facing refusal (bad relink target, ambiguous match, ...).
    #[error("{0}")]
    Invalid(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> ProjectError + '_ {
    move |err| ProjectError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

// --- The home --------------------------------------------------------------------

/// Resolve the home from raw environment values: `flywheel_home` when
/// non-empty (a relative value is anchored at `cwd`), else
/// `<home>/.flywheel`; `None` when neither is usable.
///
/// This is THE rule; `flywheel-hub` mirrors it and a cross-crate test pins
/// the two together.
pub fn home_from(flywheel_home: Option<&str>, home: Option<&str>, cwd: &Path) -> Option<PathBuf> {
    if let Some(value) = flywheel_home.filter(|v| !v.is_empty()) {
        let path = Path::new(value);
        return Some(if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        });
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(Path::new(home).join(DEFAULT_HOME_DIRNAME))
}

/// The flywheel home: its root and every fixed path under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// A home rooted at `root` (nothing is created).
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The home the process environment names ([`home_from`]).
    pub fn from_env() -> Result<Self, ProjectError> {
        let flywheel_home = std::env::var(HOME_ENV).ok();
        let home = std::env::var("HOME").ok();
        let cwd = std::env::current_dir().unwrap_or_default();
        home_from(flywheel_home.as_deref(), home.as_deref(), &cwd)
            .map(Self::at)
            .ok_or(ProjectError::NoHome)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `config.toml`: the user config.
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// `trust.json`: repo-file trust approvals.
    pub fn trust_file(&self) -> PathBuf {
        self.root.join("trust.json")
    }

    /// `hub/`: the hub's state dir.
    pub fn hub_dir(&self) -> PathBuf {
        self.root.join("hub")
    }

    /// `hub/claude-accounts/`: the Claude account token files `fw hub
    /// account add` writes.
    pub fn claude_accounts_dir(&self) -> PathBuf {
        self.hub_dir().join(CLAUDE_ACCOUNTS_DIRNAME)
    }

    /// `hub/claude-accounts/<name>.token`: account `name`'s token file. The
    /// name is not validated here (`crate::account::validate_name` is).
    pub fn account_token_path(&self, name: &str) -> PathBuf {
        self.claude_accounts_dir().join(format!("{name}.token"))
    }

    /// `self-update/`: `fw self-update`'s state dir (lock, previous pair,
    /// the record of the last successful update).
    pub fn self_update_dir(&self) -> PathBuf {
        self.root.join("self-update")
    }

    /// `self-update/lock`: the exclusive flock one `fw self-update` holds.
    pub fn self_update_lock(&self) -> PathBuf {
        self.self_update_dir().join("lock")
    }

    /// `self-update/previous/`: the `fw` / `flywheel` pair the last update
    /// replaced.
    pub fn self_update_previous_dir(&self) -> PathBuf {
        self.self_update_dir().join("previous")
    }

    /// `self-update/state.json`: commit, checkout, and installed sha256 of
    /// the last successful update.
    pub fn self_update_state(&self) -> PathBuf {
        self.self_update_dir().join("state.json")
    }

    /// `projects/`: one directory per project id.
    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    /// The paths of project `id` (nothing is created).
    pub fn project(&self, id: &str) -> ProjectPaths {
        ProjectPaths {
            root: self.projects_dir().join(id),
        }
    }
}

/// Every path under one `projects/<id>/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPaths {
    root: PathBuf,
}

impl ProjectPaths {
    /// The paths of the project dir at `root` (nothing is created).
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn meta(&self) -> PathBuf {
        self.root.join(META_FILENAME)
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn db(&self) -> PathBuf {
        self.root.join("flywheel.sqlite")
    }
    pub fn specs(&self) -> PathBuf {
        self.root.join("specs")
    }
    pub fn tasks(&self) -> PathBuf {
        self.root.join("tasks")
    }
    pub fn transcripts(&self) -> PathBuf {
        self.root.join("transcripts")
    }
    pub fn worktrees(&self) -> PathBuf {
        self.root.join("worktrees")
    }
    pub fn held_out(&self) -> PathBuf {
        self.root.join("held-out")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    /// `logs/autopilot`: the autopilot activity record and discovery cache.
    pub fn autopilot_logs(&self) -> PathBuf {
        self.logs().join("autopilot")
    }
    /// `logs/worker`: the console worker supervisor's logs.
    pub fn worker_logs(&self) -> PathBuf {
        self.logs().join("worker")
    }
    /// The landing lock serializing every worker of this project.
    pub fn merge_lock(&self) -> PathBuf {
        self.root.join(".merge.lock")
    }
}

// --- meta.json -------------------------------------------------------------------

/// `projects/<id>/meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub id: String,
    /// The canonical main worktree (repo top-level) path.
    pub path: PathBuf,
    /// The canonical git common dir.
    pub git_common_dir: PathBuf,
    /// `remote.origin.url`, when configured.
    pub remote: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
}

/// Read a project's meta.json; `Ok(None)` when it does not exist.
pub fn read_meta(paths: &ProjectPaths) -> Result<Option<ProjectMeta>, ProjectError> {
    let path = paths.meta();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_err(&path)(err)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| ProjectError::Io {
            path,
            message: format!("invalid meta.json: {err}"),
        })
}

/// Write a project's meta.json atomically (temp file + rename), creating the
/// project dir.
pub fn write_meta(paths: &ProjectPaths, meta: &ProjectMeta) -> Result<(), ProjectError> {
    fs::create_dir_all(paths.root()).map_err(io_err(paths.root()))?;
    let path = paths.meta();
    let tmp = paths.root().join(format!(
        ".{META_FILENAME}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut text = serde_json::to_string_pretty(meta).map_err(|err| ProjectError::Io {
        path: path.clone(),
        message: err.to_string(),
    })?;
    text.push('\n');
    fs::write(&tmp, text).map_err(io_err(&tmp))?;
    fs::rename(&tmp, &path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        io_err(&path)(err)
    })
}

fn now_stamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

// --- git -------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output, ProjectError> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| ProjectError::Git {
            command: args.join(" "),
            dir: dir.to_path_buf(),
            message: err.to_string(),
        })
}

fn stdout_line(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The canonical git common dir of the repo containing `dir`.
pub fn git_common_dir(dir: &Path) -> Result<PathBuf, ProjectError> {
    let output = git(dir, &["rev-parse", "--git-common-dir"])?;
    if !output.status.success() {
        return Err(ProjectError::NotAGitRepo {
            dir: dir.to_path_buf(),
        });
    }
    let raw = PathBuf::from(stdout_line(&output));
    let joined = if raw.is_absolute() {
        raw
    } else {
        dir.join(raw)
    };
    joined.canonicalize().map_err(io_err(&joined))
}

/// The canonical main worktree of the repo whose common dir is
/// `common_dir`: the first `git worktree list` entry (for a bare repo, the
/// repo dir itself).
fn main_worktree(common_dir: &Path) -> Result<PathBuf, ProjectError> {
    let output = git(common_dir, &["worktree", "list", "--porcelain"])?;
    if output.status.success()
        && let Some(path) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("worktree "))
    {
        let path = PathBuf::from(path);
        return Ok(path.canonicalize().unwrap_or(path));
    }
    // A conventional layout: the common dir is `<top>/.git`.
    if common_dir.file_name().is_some_and(|n| n == ".git")
        && let Some(parent) = common_dir.parent()
    {
        return Ok(parent.to_path_buf());
    }
    Ok(common_dir.to_path_buf())
}

/// The id stored in `common_dir`'s config (`None` when unset or the dir is
/// gone).
fn read_stored_id(common_dir: &Path) -> Result<Option<String>, ProjectError> {
    let config = common_dir.join("config");
    if !config.is_file() {
        return Ok(None);
    }
    let output = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(&config)
        .args(["--get", PROJECT_ID_KEY])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| ProjectError::Git {
            command: format!("config --get {PROJECT_ID_KEY}"),
            dir: common_dir.to_path_buf(),
            message: err.to_string(),
        })?;
    // Exit 1 = key unset.
    if !output.status.success() {
        return Ok(None);
    }
    let value = stdout_line(&output);
    if value.is_empty() {
        return Ok(None);
    }
    match uuid::Uuid::parse_str(&value) {
        Ok(parsed) => Ok(Some(parsed.hyphenated().to_string())),
        Err(_) => Err(ProjectError::MalformedId { config, value }),
    }
}

/// The project id the repo holding `dir` already carries, without minting
/// one or touching the home (`Ok(None)` when it has none). For readers
/// that must not create state, such as the hub's trust check.
pub fn stored_project_id(dir: &Path) -> Result<Option<String>, ProjectError> {
    read_stored_id(&git_common_dir(dir)?)
}

fn write_stored_id(common_dir: &Path, id: &str) -> Result<(), ProjectError> {
    let config = common_dir.join("config");
    let output = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(&config)
        .args([PROJECT_ID_KEY, id])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| ProjectError::Git {
            command: format!("config {PROJECT_ID_KEY}"),
            dir: common_dir.to_path_buf(),
            message: err.to_string(),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(ProjectError::Git {
        command: format!("config {PROJECT_ID_KEY}"),
        dir: common_dir.to_path_buf(),
        message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn origin_url(common_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(common_dir.join("config"))
        .args(["--get", "remote.origin.url"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let url = stdout_line(&output);
    (output.status.success() && !url.is_empty()).then_some(url)
}

/// Mint a fresh id into `common_dir`'s config. A concurrent minter that won
/// the config lock is honored: on a failed write the stored id (if any) is
/// re-read and adopted.
fn mint_id(common_dir: &Path) -> Result<String, ProjectError> {
    let id = uuid::Uuid::new_v4().hyphenated().to_string();
    match write_stored_id(common_dir, &id) {
        Ok(()) => Ok(id),
        Err(failure) => match read_stored_id(common_dir)? {
            Some(existing) => Ok(existing),
            None => Err(failure),
        },
    }
}

// --- Resolution ------------------------------------------------------------------

/// A resolved project: its id, its home paths, and its recorded metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub paths: ProjectPaths,
    pub meta: ProjectMeta,
}

/// The current worktree's top-level directory for `dir` (the linked
/// worktree's own root inside a linked worktree).
pub fn worktree_toplevel(dir: &Path) -> Result<PathBuf, ProjectError> {
    let output = git(dir, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Err(ProjectError::NotAGitRepo {
            dir: dir.to_path_buf(),
        });
    }
    let top = PathBuf::from(stdout_line(&output));
    Ok(top.canonicalize().unwrap_or(top))
}

/// Resolve (creating on first sight) the project of the git repo holding
/// `dir`, under `home`.
///
/// Mints and stores an id when the repo has none; follows a moved repo
/// (meta.json path updated, id kept); mints a fresh id for a copy (the
/// recorded common dir still exists and carries the same id), leaving the
/// original untouched. Always refreshes `last_seen`.
pub fn resolve_project(home: &Home, dir: &Path) -> Result<Project, ProjectError> {
    let common_dir = git_common_dir(dir)?;
    let path = main_worktree(&common_dir)?;
    let remote = origin_url(&common_dir);
    let now = now_stamp();

    let id = match read_stored_id(&common_dir)? {
        Some(id) => {
            let paths = home.project(&id);
            match read_meta(&paths)? {
                Some(meta) if meta.git_common_dir != common_dir && is_original(&meta, &id)? => {
                    // A copy: the recorded repo still carries this id.
                    mint_id(&common_dir)?
                }
                _ => id,
            }
        }
        None => mint_id(&common_dir)?,
    };

    let paths = home.project(&id);
    let meta = match read_meta(&paths)? {
        Some(mut meta) => {
            meta.id.clone_from(&id);
            meta.path = path;
            meta.git_common_dir = common_dir;
            meta.remote = remote;
            meta.last_seen = now;
            meta
        }
        None => ProjectMeta {
            id: id.clone(),
            path,
            git_common_dir: common_dir,
            remote,
            first_seen: now.clone(),
            last_seen: now,
        },
    };
    write_meta(&paths, &meta)?;
    Ok(Project { id, paths, meta })
}

/// Whether [`resolve_project`] on `dir` would mint a new id (and so create
/// a project): its repo carries no id yet, or it is a copy of the repo the
/// id's meta.json records. Read-only: for callers that must never create a
/// project (the hub's audit record).
pub fn would_mint(home: &Home, dir: &Path) -> Result<bool, ProjectError> {
    let common_dir = git_common_dir(dir)?;
    let Some(id) = read_stored_id(&common_dir)? else {
        return Ok(true);
    };
    Ok(match read_meta(&home.project(&id))? {
        Some(meta) => meta.git_common_dir != common_dir && is_original(&meta, &id)?,
        None => false,
    })
}

/// The project dir an invocation of `local` uses when its effective config
/// names `id` (a `project_id` override shares one project dir -- and one
/// Postgres partition -- across clones). `id == local.id` is `local`
/// itself; otherwise `id`'s meta.json is created or updated to record this
/// repo, so `fw project ls` shows it and `fw project prune` keeps it while
/// the repo's `.flywheel.toml` still declares it.
pub fn bind_project(home: &Home, local: &Project, id: &str) -> Result<Project, ProjectError> {
    if id == local.id {
        return Ok(local.clone());
    }
    let paths = home.project(id);
    let now = now_stamp();
    let meta = match read_meta(&paths)? {
        Some(mut meta) => {
            meta.id = id.to_owned();
            meta.path.clone_from(&local.meta.path);
            meta.git_common_dir.clone_from(&local.meta.git_common_dir);
            meta.remote.clone_from(&local.meta.remote);
            meta.last_seen = now;
            meta
        }
        None => ProjectMeta {
            id: id.to_owned(),
            path: local.meta.path.clone(),
            git_common_dir: local.meta.git_common_dir.clone(),
            remote: local.meta.remote.clone(),
            first_seen: now.clone(),
            last_seen: now,
        },
    };
    write_meta(&paths, &meta)?;
    Ok(Project {
        id: id.to_owned(),
        paths,
        meta,
    })
}

/// Whether the repo `meta` records still exists and carries `id`.
fn is_original(meta: &ProjectMeta, id: &str) -> Result<bool, ProjectError> {
    if !meta.git_common_dir.is_dir() {
        return Ok(false);
    }
    Ok(read_stored_id(&meta.git_common_dir)
        .ok()
        .flatten()
        .is_some_and(|stored| stored == id))
}

/// [`resolve_project`] against the process home and working directory.
pub fn resolve_cwd_project() -> Result<Project, ProjectError> {
    let home = Home::from_env()?;
    let cwd = std::env::current_dir().map_err(|err| ProjectError::Io {
        path: PathBuf::from("."),
        message: format!("cannot resolve the working directory: {err}"),
    })?;
    resolve_project(&home, &cwd)
}

// --- Listing, pruning, relinking -------------------------------------------------

/// One `fw project ls` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectEntry {
    pub id: String,
    pub path: PathBuf,
}

fn is_uuid(name: &str) -> bool {
    uuid::Uuid::parse_str(name).is_ok()
}

/// Every project dir under the home (UUID-named directories only), sorted
/// by id, with its meta (`None` when meta.json is missing or unreadable).
fn project_dirs(home: &Home) -> Result<Vec<(String, Option<ProjectMeta>)>, ProjectError> {
    let dir = home.projects_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_err(&dir)(err)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(&dir))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_dir() || !is_uuid(&name) {
            continue;
        }
        let meta = read_meta(&home.project(&name)).ok().flatten();
        out.push((name, meta));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Every known project, sorted by path then id. Dirs without a readable
/// meta.json are skipped (`fw project prune` reports them).
pub fn list_projects(home: &Home) -> Result<Vec<ProjectEntry>, ProjectError> {
    let mut rows: Vec<ProjectEntry> = project_dirs(home)?
        .into_iter()
        .filter_map(|(id, meta)| {
            meta.map(|meta| ProjectEntry {
                id,
                path: meta.path,
            })
        })
        .collect();
    rows.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
    Ok(rows)
}

/// One project dir `prune` removed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    pub id: String,
    pub path: Option<PathBuf>,
    pub reason: String,
}

/// Remove every project dir whose recorded repo no longer exists or no
/// longer carries that id -- neither in its `.git/config` nor as the
/// `project_id` its `.flywheel.toml` declares -- (and every UUID-named dir
/// without a readable meta.json).
pub fn prune_projects(home: &Home) -> Result<Vec<Pruned>, ProjectError> {
    let mut pruned = Vec::new();
    for (id, meta) in project_dirs(home)? {
        let reason = match &meta {
            None => Some("no readable meta.json".to_owned()),
            Some(meta) if !meta.git_common_dir.is_dir() => Some(format!(
                "recorded repo {} no longer exists",
                meta.path.display()
            )),
            // A shared project dir: the repo's committed file declares it.
            Some(meta)
                if crate::config::declared_project_id(&meta.path).as_deref() == Some(&id) =>
            {
                None
            }
            Some(meta) => match read_stored_id(&meta.git_common_dir) {
                Ok(Some(stored)) if stored == id => None,
                Ok(Some(stored)) => Some(format!(
                    "recorded repo {} now carries id {stored}",
                    meta.path.display()
                )),
                Ok(None) => Some(format!(
                    "recorded repo {} no longer carries a project id",
                    meta.path.display()
                )),
                Err(err) => Some(format!("recorded repo unreadable: {err}")),
            },
        };
        let Some(reason) = reason else { continue };
        let dir = home.project(&id);
        fs::remove_dir_all(dir.root()).map_err(io_err(dir.root()))?;
        pruned.push(Pruned {
            id,
            path: meta.map(|m| m.path),
            reason,
        });
    }
    Ok(pruned)
}

/// Re-point the repo holding `dir` at an existing project dir.
///
/// `target` names the project dir (a path, or a bare id under the home);
/// `None` adopts the single project whose recorded path is this repo's path
/// (the recovery for a re-clone that lost its `.git/config` key). The repo's
/// `flywheel.projectid` is overwritten and the project's meta.json updated.
pub fn relink_project(
    home: &Home,
    dir: &Path,
    target: Option<&str>,
) -> Result<Project, ProjectError> {
    let common_dir = git_common_dir(dir)?;
    let path = main_worktree(&common_dir)?;
    let id = match target {
        Some(target) => relink_target_id(home, target)?,
        None => {
            let matches: Vec<String> = project_dirs(home)?
                .into_iter()
                .filter(|(_, meta)| meta.as_ref().is_some_and(|m| m.path == path))
                .map(|(id, _)| id)
                .collect();
            match matches.as_slice() {
                [only] => only.clone(),
                [] => {
                    return Err(ProjectError::Invalid(format!(
                        "no project records {} as its path; pass the project dir to relink to \
                         (see `fw project ls`)",
                        path.display()
                    )));
                }
                many => {
                    return Err(ProjectError::Invalid(format!(
                        "{} projects record {} as their path ({}); pass the one to relink to",
                        many.len(),
                        path.display(),
                        many.join(", ")
                    )));
                }
            }
        }
    };
    write_stored_id(&common_dir, &id)?;
    let paths = home.project(&id);
    let now = now_stamp();
    let remote = origin_url(&common_dir);
    let meta = match read_meta(&paths)? {
        Some(mut meta) => {
            meta.id.clone_from(&id);
            meta.path = path;
            meta.git_common_dir = common_dir;
            meta.remote = remote;
            meta.last_seen = now;
            meta
        }
        None => ProjectMeta {
            id: id.clone(),
            path,
            git_common_dir: common_dir,
            remote,
            first_seen: now.clone(),
            last_seen: now,
        },
    };
    write_meta(&paths, &meta)?;
    Ok(Project { id, paths, meta })
}

/// The project id a relink target names: an existing directory whose name
/// is a UUID (a project dir), or a bare id with a dir under the home.
fn relink_target_id(home: &Home, target: &str) -> Result<String, ProjectError> {
    let as_path = Path::new(target);
    if as_path.is_dir() {
        let canonical = as_path.canonicalize().map_err(io_err(as_path))?;
        let name = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !is_uuid(&name) {
            return Err(ProjectError::Invalid(format!(
                "{} is not a project dir (its name must be the project id; see `fw project ls`)",
                canonical.display()
            )));
        }
        let expected = home.projects_dir().canonicalize().ok();
        if expected.as_deref() != canonical.parent() {
            return Err(ProjectError::Invalid(format!(
                "{} is not under {}",
                canonical.display(),
                home.projects_dir().display()
            )));
        }
        return Ok(uuid::Uuid::parse_str(&name)
            .map(|u| u.hyphenated().to_string())
            .unwrap_or(name));
    }
    if is_uuid(target) {
        let id = uuid::Uuid::parse_str(target)
            .map(|u| u.hyphenated().to_string())
            .unwrap_or_else(|_| target.to_owned());
        if home.project(&id).root().is_dir() {
            return Ok(id);
        }
    }
    Err(ProjectError::Invalid(format!(
        "no project dir at {target} (see `fw project ls`)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        run_git(dir, &["init", "-q"]);
        run_git(dir, &["config", "user.email", "t@example.com"]);
        run_git(dir, &["config", "user.name", "t"]);
        fs::write(dir.join("README"), "x").unwrap();
        run_git(dir, &["add", "README"]);
        run_git(dir, &["commit", "-q", "-m", "init"]);
    }

    fn stored(dir: &Path) -> Option<String> {
        read_stored_id(&git_common_dir(dir).unwrap()).unwrap()
    }

    fn project_count(home: &Home) -> usize {
        fs::read_dir(home.projects_dir()).map_or(0, Iterator::count)
    }

    #[test]
    fn home_rule_prefers_the_env_then_home() {
        let cwd = Path::new("/work");
        assert_eq!(
            home_from(Some("/x/fw"), Some("/home/u"), cwd),
            Some(PathBuf::from("/x/fw"))
        );
        assert_eq!(
            home_from(Some("rel"), Some("/home/u"), cwd),
            Some(PathBuf::from("/work/rel"))
        );
        assert_eq!(
            home_from(Some(""), Some("/home/u"), cwd),
            Some(PathBuf::from("/home/u/.flywheel"))
        );
        assert_eq!(
            home_from(None, Some("/home/u"), cwd),
            Some(PathBuf::from("/home/u/.flywheel"))
        );
        assert_eq!(home_from(None, None, cwd), None);
        assert_eq!(home_from(None, Some(""), cwd), None);
    }

    #[test]
    fn layout_paths_are_fixed_under_the_home() {
        let home = Home::at("/h");
        assert_eq!(home.config_file(), Path::new("/h/config.toml"));
        assert_eq!(home.trust_file(), Path::new("/h/trust.json"));
        assert_eq!(home.hub_dir(), Path::new("/h/hub"));
        assert_eq!(home.self_update_dir(), Path::new("/h/self-update"));
        assert_eq!(home.self_update_lock(), Path::new("/h/self-update/lock"));
        assert_eq!(
            home.self_update_previous_dir(),
            Path::new("/h/self-update/previous")
        );
        assert_eq!(
            home.self_update_state(),
            Path::new("/h/self-update/state.json")
        );
        let p = home.project("abc");
        assert_eq!(p.root(), Path::new("/h/projects/abc"));
        assert_eq!(p.meta(), Path::new("/h/projects/abc/meta.json"));
        assert_eq!(p.config(), Path::new("/h/projects/abc/config.toml"));
        assert_eq!(p.db(), Path::new("/h/projects/abc/flywheel.sqlite"));
        assert_eq!(p.specs(), Path::new("/h/projects/abc/specs"));
        assert_eq!(p.tasks(), Path::new("/h/projects/abc/tasks"));
        assert_eq!(p.transcripts(), Path::new("/h/projects/abc/transcripts"));
        assert_eq!(p.worktrees(), Path::new("/h/projects/abc/worktrees"));
        assert_eq!(p.held_out(), Path::new("/h/projects/abc/held-out"));
        assert_eq!(p.logs(), Path::new("/h/projects/abc/logs"));
    }

    #[test]
    fn a_fresh_repo_mints_a_uuid_and_records_meta() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let project = resolve_project(&home, &repo.join(".")).unwrap();
        let parsed = uuid::Uuid::parse_str(&project.id).unwrap();
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(project.id, project.id.to_lowercase());
        assert_eq!(stored(&repo).as_deref(), Some(project.id.as_str()));
        let meta = read_meta(&home.project(&project.id)).unwrap().unwrap();
        assert_eq!(meta.path, repo.canonicalize().unwrap());
        assert_eq!(
            meta.git_common_dir,
            repo.join(".git").canonicalize().unwrap()
        );
        assert_eq!(meta.remote, None);
        // Nothing but .git/config changed in the repo.
        let entries: Vec<_> = fs::read_dir(&repo)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 2, "{entries:?}");
        // Stable across calls, from a subdirectory too.
        fs::create_dir_all(repo.join("sub")).unwrap();
        let again = resolve_project(&home, &repo.join("sub")).unwrap();
        assert_eq!(again.id, project.id);
        assert_eq!(again.meta.first_seen, meta.first_seen);
        assert_eq!(project_count(&home), 1);
    }

    #[test]
    fn records_the_origin_remote() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        run_git(
            &repo,
            &["remote", "add", "origin", "https://example.com/r.git"],
        );
        let project = resolve_project(&home, &repo).unwrap();
        assert_eq!(
            project.meta.remote.as_deref(),
            Some("https://example.com/r.git")
        );
    }

    #[test]
    fn a_linked_worktree_shares_the_project() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let main = resolve_project(&home, &repo).unwrap();
        let linked = tmp.path().join("linked");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        let other = resolve_project(&home, &linked).unwrap();
        assert_eq!(other.id, main.id);
        assert_eq!(other.meta.path, repo.canonicalize().unwrap());
        assert_eq!(project_count(&home), 1);
    }

    #[test]
    fn a_moved_repo_keeps_its_id_and_meta_follows() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let before = resolve_project(&home, &repo).unwrap();
        let moved = tmp.path().join("moved");
        fs::rename(&repo, &moved).unwrap();
        let after = resolve_project(&home, &moved).unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.meta.path, moved.canonicalize().unwrap());
        let meta = read_meta(&home.project(&before.id)).unwrap().unwrap();
        assert_eq!(meta.path, moved.canonicalize().unwrap());
        assert_eq!(project_count(&home), 1);
    }

    #[test]
    fn a_copy_mints_a_new_id_and_leaves_the_original() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let original = resolve_project(&home, &repo).unwrap();
        let copy = tmp.path().join("copy");
        let status = Command::new("cp")
            .arg("-a")
            .arg(&repo)
            .arg(&copy)
            .status()
            .unwrap();
        assert!(status.success());
        let copied = resolve_project(&home, &copy).unwrap();
        assert_ne!(copied.id, original.id);
        assert_eq!(stored(&copy).as_deref(), Some(copied.id.as_str()));
        assert_eq!(stored(&repo).as_deref(), Some(original.id.as_str()));
        assert_eq!(copied.meta.path, copy.canonicalize().unwrap());
        let meta = read_meta(&home.project(&original.id)).unwrap().unwrap();
        assert_eq!(meta.path, repo.canonicalize().unwrap());
        assert_eq!(project_count(&home), 2);
        // The original still resolves to its own id.
        assert_eq!(resolve_project(&home, &repo).unwrap().id, original.id);
    }

    #[test]
    fn a_non_git_dir_is_refused_naming_git() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let bare = tmp.path().join("plain");
        fs::create_dir_all(&bare).unwrap();
        let err = resolve_project(&home, &bare).unwrap_err();
        assert!(matches!(err, ProjectError::NotAGitRepo { .. }));
        assert!(err.to_string().contains("git"), "{err}");
        assert!(!home.root().exists());
    }

    #[test]
    fn a_malformed_stored_id_is_refused() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        run_git(&repo, &["config", PROJECT_ID_KEY, "not-a-uuid"]);
        let err = resolve_project(&home, &repo).unwrap_err();
        assert!(matches!(err, ProjectError::MalformedId { .. }), "{err}");
    }

    #[test]
    fn list_prune_and_relink() {
        let tmp = TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let keep = tmp.path().join("keep");
        let gone = tmp.path().join("gone");
        init_repo(&keep);
        init_repo(&gone);
        let kept = resolve_project(&home, &keep).unwrap();
        let lost = resolve_project(&home, &gone).unwrap();
        let rows = list_projects(&home).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.id == kept.id));

        // A re-clone at the same path lost its key: relink with no target
        // adopts the project recording this path.
        run_git(&keep, &["config", "--unset", PROJECT_ID_KEY]);
        let relinked = relink_project(&home, &keep, None).unwrap();
        assert_eq!(relinked.id, kept.id);
        assert_eq!(stored(&keep).as_deref(), Some(kept.id.as_str()));

        fs::remove_dir_all(&gone).unwrap();
        let pruned = prune_projects(&home).unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].id, lost.id);
        assert!(!home.project(&lost.id).root().exists());
        assert!(home.project(&kept.id).root().exists());

        // Relink to an explicit project dir (by path and by id).
        let other = tmp.path().join("other");
        init_repo(&other);
        let target = home.project(&kept.id).root().display().to_string();
        assert_eq!(
            relink_project(&home, &other, Some(&target)).unwrap().id,
            kept.id
        );
        assert_eq!(stored(&other).as_deref(), Some(kept.id.as_str()));
        assert!(relink_project(&home, &other, Some("nope")).is_err());
        assert_eq!(
            relink_project(&home, &keep, Some(&kept.id)).unwrap().id,
            kept.id
        );
    }
}
