//! The hub's persisted model: workspaces (a name plus 1..N folders, git
//! repos or plain directories) and session records, stored as one JSON
//! document (`state.json`, mode 0600, written atomically) in the state dir.
//!
//! Processes belong to the running server, not to this file: a session
//! recorded as `running` when the hub last stopped is loaded back as
//! `stopped` (not `clean`: the hub died without recording it, so it is not
//! restored).

mod store;
pub use store::HubStore;

use crate::agent::{AgentLaunch, SessionKind};
use crate::config::home_dir;
use crate::git::WorktreeRef;
use crate::layout::Layout;
use crate::pty::ExitInfo;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// State file name inside the state dir.
pub const STATE_FILE: &str = "state.json";
/// On-disk schema version: 2 added per-workspace `layouts`, 3 launch
/// `templates` (and sessions' `launch` options), 4 verified `fanouts`, 5
/// per-session prompt `queues`.
/// Older files load and are written back as the current version; a newer
/// hub's file is refused rather than rewritten without what it does not
/// know.
pub const STATE_VERSION: u32 = 5;
/// The oldest schema version still loaded.
pub const STATE_VERSION_MIN: u32 = 1;
/// Limits on workspace input.
pub const MAX_NAME_CHARS: usize = 100;
pub const MAX_FOLDERS: usize = 32;
/// Launch templates kept per workspace.
pub const MAX_TEMPLATES: usize = 50;

/// One directory of a workspace: a git work-tree root or a plain folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Canonical absolute path.
    pub path: PathBuf,
    /// Display label (a VS Code workspace folder's `name`); the UI shows
    /// the directory's name when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether it is a git work-tree root (a `.git` dir, or a `.git` file
    /// for linked worktrees). Not authoritative on disk: recomputed when
    /// the state loads and whenever workspaces are listed. Repo-only
    /// features (worktrees, branch badges) key off it.
    #[serde(default)]
    pub git: bool,
    /// The directory is gone (deleted, moved, or an unmounted volume) as
    /// of the last check. Recomputed like `git`; sessions cannot open
    /// there until it is back.
    #[serde(default)]
    pub missing: bool,
}

impl Folder {
    /// A folder at canonical `path`, its flags read from disk.
    pub fn at(path: PathBuf, name: Option<String>) -> Self {
        let mut folder = Self {
            path,
            name,
            git: false,
            missing: false,
        };
        folder.refresh();
        folder
    }

    /// Re-read the `git` and `missing` flags from disk.
    pub fn refresh(&mut self) {
        self.missing = !self.path.is_dir();
        self.git = !self.missing && is_git_root(&self.path);
    }
}

/// A named set of folders (git repos or plain directories) worked on
/// together.
///
/// Serialized with `folders` plus a legacy `repos` mirror (every folder's
/// path, git or not), so readers of the old shape keep working; loads
/// either (`folders` wins when both are present).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "WorkspaceRepr", into = "WorkspaceRepr")]
pub struct Workspace {
    pub id: String,
    pub name: String,
    /// In operator order; never empty.
    pub folders: Vec<Folder>,
    pub created_at: DateTime<Utc>,
}

impl Workspace {
    /// Whether `path` (canonical) is one of its folders.
    pub fn has_folder(&self, path: &Path) -> bool {
        self.folders.iter().any(|f| f.path == path)
    }

    /// The folder sessions open in by default.
    pub fn first_folder(&self) -> &Path {
        &self.folders[0].path
    }

    /// Re-read every folder's `git` and `missing` flags from disk.
    pub fn refresh_disk(&mut self) {
        for folder in &mut self.folders {
            folder.refresh();
        }
    }
}

#[derive(Serialize, Deserialize)]
struct WorkspaceRepr {
    id: String,
    name: String,
    #[serde(default)]
    folders: Option<Vec<Folder>>,
    /// Legacy: paths only (state files and API clients from before plain
    /// folders). Written as a mirror of `folders`.
    #[serde(default)]
    repos: Option<Vec<PathBuf>>,
    created_at: DateTime<Utc>,
}

impl From<WorkspaceRepr> for Workspace {
    fn from(repr: WorkspaceRepr) -> Self {
        let folders = match (repr.folders, repr.repos) {
            (Some(folders), _) => folders,
            (None, Some(repos)) => repos
                .into_iter()
                .map(|path| Folder {
                    path,
                    name: None,
                    git: false,
                    missing: false,
                })
                .collect(),
            (None, None) => Vec::new(),
        };
        Self {
            id: repr.id,
            name: repr.name,
            folders,
            created_at: repr.created_at,
        }
    }
}

impl From<Workspace> for WorkspaceRepr {
    fn from(workspace: Workspace) -> Self {
        let repos = workspace.folders.iter().map(|f| f.path.clone()).collect();
        Self {
            id: workspace.id,
            name: workspace.name,
            folders: Some(workspace.folders),
            repos: Some(repos),
            created_at: workspace.created_at,
        }
    }
}

/// A session's lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Running,
    Exited {
        #[serde(flatten)]
        exit: ExitInfo,
        at: DateTime<Utc>,
    },
    /// The hub stopped while the session was running.
    Stopped {
        at: DateTime<Utc>,
        /// Recorded by the hub's own (graceful) shutdown, rather than
        /// inferred on load after the hub died: the next start restores it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clean: bool,
        /// Why the last attempt to restore it failed (e.g. its working
        /// directory is gone). Retried on every start until it succeeds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restore_error: Option<String>,
        /// The process's working directory when the hub stopped it (where
        /// the shell had `cd`'d to), where a restore starts it again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_cwd: Option<PathBuf>,
    },
}

impl SessionState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// Stopped by a clean hub shutdown (so restored on start).
    pub fn is_restorable(&self) -> bool {
        matches!(self, Self::Stopped { clean: true, .. })
    }

    /// A state stopped at `at` by the hub's own shutdown (`clean`) or found
    /// running after the hub died.
    pub fn stopped(at: DateTime<Utc>, clean: bool) -> Self {
        Self::Stopped {
            at,
            clean,
            restore_error: None,
            last_cwd: None,
        }
    }
}

/// A terminal session within a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    /// Shell or agent CLI (records from before kinds existed are shells).
    #[serde(default)]
    pub kind: SessionKind,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    /// The agent's own conversation id (Claude Code's `--session-id`,
    /// Codex's thread id): the key to its transcript and to resuming it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(flatten)]
    pub state: SessionState,
    /// What runs in place of `command` after a restore that could not
    /// bring the session back as itself (an agent without native resume
    /// comes back as a hub shell). `kind` and `command` keep what the
    /// session is; relaunching it runs `command` again and clears this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_command: Option<Vec<String>>,
    /// The sibling worktree the session runs in (`cwd` is its directory),
    /// cut from one of the workspace's git folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeRef>,
    /// A shell opened as an agent session's companion (same directory):
    /// the agent's id. Suggested commands go there by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub companion_of: Option<String>,
    /// An agent's model and permission options (every launch, relaunch,
    /// and restore runs it with them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<AgentLaunch>,
    /// The reviewer agent this session's work was last sent to ("ask for
    /// a review"): its id. That session's `review_of` names this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// A reviewer agent: the session whose work it reviews (in that
    /// session's directory, read-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_of: Option<String>,
    /// A Claude session's account ([`crate::accounts`]): it runs under that
    /// token instead of the CLI's own login, on every relaunch and restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// A saved way to start an agent in a workspace: kind, first prompt,
/// model and permission options, and whether it gets its own worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchTemplate {
    /// Unique within its workspace.
    pub name: String,
    pub kind: SessionKind,
    /// The first user turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(flatten)]
    pub launch: AgentLaunch,
    /// Run it in a new sibling worktree (of a git folder).
    #[serde(default)]
    pub worktree: bool,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

impl LaunchTemplate {
    /// Checked and normalized: a valid name, an agent kind's options only,
    /// a bounded prompt.
    pub fn validated(mut self) -> Result<Self, StateError> {
        self.name = validate_name(&self.name)?;
        if !self.kind.is_agent() {
            return Err(StateError::Invalid(
                "a template starts an agent: kind claude or codex".into(),
            ));
        }
        self.launch = self
            .launch
            .validated(self.kind)
            .map_err(StateError::Invalid)?;
        self.prompt =
            crate::agent::validate_prompt(self.prompt.as_deref()).map_err(StateError::Invalid)?;
        Ok(self)
    }
}

impl SessionRecord {
    /// How errors name the session: `name (id)`.
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.id)
    }

    /// The workspace folder the session belongs to: its worktree's repo,
    /// else its own directory.
    pub fn folder(&self) -> &Path {
        self.worktree.as_ref().map_or(&self.cwd, |w| &w.repo)
    }

    /// Whether the session runs its agent CLI: an agent kind not restored
    /// as a fallback shell (which has no hooks, status, or transcript of
    /// its own).
    pub fn runs_agent(&self) -> bool {
        self.kind.is_agent() && self.fallback_command.is_none()
    }
}

/// `restore_error` of a session found `running` on load: the hub died
/// (killed, crashed, the machine went down) without its own shutdown.
pub const UNCLEAN_STOP: &str = "the hub was not shut down cleanly (killed or crashed)";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateDoc {
    version: u32,
    #[serde(default)]
    workspaces: Vec<Workspace>,
    #[serde(default)]
    sessions: Vec<SessionRecord>,
    /// Workspace id -> its tabs and splits (absent: never laid out).
    #[serde(default)]
    layouts: BTreeMap<String, Layout>,
    /// Workspace id -> its launch templates, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    templates: BTreeMap<String, Vec<LaunchTemplate>>,
    /// Verified fan-outs (see [`crate::fanout`]), oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fanouts: Vec<crate::fanout::Fanout>,
    /// Session id -> its prompt queue (see [`crate::queue`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    queues: BTreeMap<String, crate::queue::SessionQueue>,
}

/// Store and validation failures.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("state file {path}: {message}")]
    Persist { path: PathBuf, message: String },
}

/// A folder as an operator gives it: a path, or `{"path","name"?}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FolderSpec {
    Path(PathBuf),
    Labeled {
        path: PathBuf,
        /// Display label. Absent: an updated workspace keeps the label the
        /// folder already had; empty: no label.
        #[serde(default)]
        name: Option<String>,
    },
}

impl FolderSpec {
    pub fn path(&self) -> &Path {
        match self {
            Self::Path(path) | Self::Labeled { path, .. } => path,
        }
    }
}

/// Workspace fields an operator supplies. `repos` is the legacy spelling
/// of `folders` (paths only).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkspaceInput {
    pub name: Option<String>,
    pub folders: Option<Vec<FolderSpec>>,
    pub repos: Option<Vec<PathBuf>>,
}

impl WorkspaceInput {
    /// The folders given, under either spelling (not both).
    fn folder_specs(&self) -> Result<Option<Vec<FolderSpec>>, StateError> {
        match (&self.folders, &self.repos) {
            (Some(_), Some(_)) => Err(StateError::Invalid(
                "give folders or repos (its legacy name), not both".into(),
            )),
            (Some(folders), None) => Ok(Some(folders.clone())),
            (None, Some(repos)) => Ok(Some(repos.iter().cloned().map(FolderSpec::Path).collect())),
            (None, None) => Ok(None),
        }
    }
}

/// A fresh opaque id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Trim and bound a display name.
pub fn validate_name(raw: &str) -> Result<String, StateError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(StateError::Invalid("name must not be empty".into()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(StateError::Invalid(format!(
            "name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(StateError::Invalid(
            "name must not contain control characters".into(),
        ));
    }
    Ok(name.to_owned())
}

/// Expand a leading `~` (alone or as `~/...`) to `home`.
pub fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Whether `path` is a git work-tree root: a `.git` dir, or the `.git`
/// file of a linked worktree.
pub fn is_git_root(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Check one folder path (`~` expanded): absolute, existing, a directory.
/// Returns it canonical. Git repos and plain directories both qualify.
pub fn validate_folder(raw: &Path) -> Result<PathBuf, StateError> {
    let path = expand_home(raw, home_dir().as_deref());
    if !path.is_absolute() {
        return Err(StateError::Invalid(format!(
            "folder path {} must be absolute",
            raw.display()
        )));
    }
    let canonical = path.canonicalize().map_err(|e| {
        let why = match e.kind() {
            std::io::ErrorKind::NotFound => "does not exist".to_owned(),
            std::io::ErrorKind::PermissionDenied => {
                "is not accessible (permission denied)".to_owned()
            }
            std::io::ErrorKind::NotADirectory => "is not a directory".to_owned(),
            _ => format!("cannot be resolved: {e}"),
        };
        StateError::Invalid(format!("folder path {} {why}", path.display()))
    })?;
    if !canonical.is_dir() {
        return Err(StateError::Invalid(format!(
            "folder path {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Validate a folder label: trimmed, bounded, no control characters;
/// empty means none.
fn validate_label(raw: &str) -> Result<Option<String>, StateError> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    validate_name(raw)
        .map(Some)
        .map_err(|e| StateError::Invalid(format!("folder label: {e}")))
}

/// Canonicalize and check folder specs: 1..=MAX_FOLDERS paths passing
/// [`validate_folder`] (or already in `previous`, kept even while
/// missing), duplicates dropped (the first keeps its place and label). A spec without a label keeps the one its path has in
/// `previous` (the folders being replaced).
pub fn validate_folders(
    specs: &[FolderSpec],
    previous: &[Folder],
) -> Result<Vec<Folder>, StateError> {
    if specs.is_empty() {
        return Err(StateError::Invalid(
            "a workspace needs at least one folder".into(),
        ));
    }
    if specs.len() > MAX_FOLDERS {
        return Err(StateError::Invalid(format!(
            "a workspace holds at most {MAX_FOLDERS} folders"
        )));
    }
    let mut seen = HashSet::new();
    let mut folders = Vec::with_capacity(specs.len());
    for spec in specs {
        // A folder the workspace already has may stay while it is missing
        // (an unmounted volume, a directory moved for a moment): editing
        // the rest of the list must not force it out.
        let path = match previous.iter().find(|f| f.path == spec.path()) {
            Some(kept) if !kept.path.is_dir() => kept.path.clone(),
            _ => validate_folder(spec.path())?,
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let name = match spec {
            FolderSpec::Labeled {
                name: Some(name), ..
            } => validate_label(name)?,
            _ => previous
                .iter()
                .find(|f| f.path == path)
                .and_then(|f| f.name.clone()),
        };
        folders.push(Folder::at(path, name));
    }
    Ok(folders)
}

#[cfg(test)]
mod tests;
