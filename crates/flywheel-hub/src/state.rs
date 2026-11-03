//! The hub's persisted model: workspaces (a name plus 1..N folders, git
//! repos or plain directories) and session records, stored as one JSON
//! document (`state.json`, mode 0600, written atomically) in the state dir.
//!
//! Processes belong to the running server, not to this file: a session
//! recorded as `running` when the hub last stopped is loaded back as
//! `stopped` (not `clean`: the hub died without recording it, so it is not
//! restored).

use crate::agent::{AgentLaunch, SessionKind};
use crate::auth::write_private;
use crate::git::WorktreeRef;
use crate::layout::{Layout, Tab};
use crate::pty::ExitInfo;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// State file name inside the state dir.
pub const STATE_FILE: &str = "state.json";
/// On-disk schema version: 2 added per-workspace `layouts`, 3 launch
/// `templates` (and sessions' `launch` options). Older files load and are
/// written back as the current version; a newer hub's file is refused
/// rather than rewritten without what it does not know.
pub const STATE_VERSION: u32 = 3;
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

/// The persisted hub state.
#[derive(Debug)]
pub struct HubStore {
    path: PathBuf,
    doc: StateDoc,
    /// Changes stay in memory (a live handoff: another hub process owns
    /// the file from the freeze on).
    read_only: bool,
}

impl HubStore {
    /// Load `state.json` from `state_dir` (empty when absent), demoting
    /// sessions left `running` to `stopped`.
    pub fn open(state_dir: &Path) -> Result<Self, StateError> {
        Self::open_keeping(state_dir, &HashSet::new())
    }

    /// [`HubStore::open`], except that the sessions in `live` stay
    /// `running`: a live handoff brings their processes along.
    pub fn open_keeping(state_dir: &Path, live: &HashSet<String>) -> Result<Self, StateError> {
        let path = state_dir.join(STATE_FILE);
        let persist_err = |message: String| StateError::Persist {
            path: path.clone(),
            message,
        };
        let mut doc = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<StateDoc>(&bytes)
                .map_err(|e| persist_err(format!("invalid JSON: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StateDoc {
                version: STATE_VERSION,
                ..StateDoc::default()
            },
            Err(e) => return Err(persist_err(e.to_string())),
        };
        if !(STATE_VERSION_MIN..=STATE_VERSION).contains(&doc.version) {
            return Err(persist_err(format!(
                "unsupported version {} (expected {STATE_VERSION_MIN}..={STATE_VERSION})",
                doc.version
            )));
        }
        let migrated = doc.version != STATE_VERSION;
        doc.version = STATE_VERSION;
        // A layout must never name a session or workspace that is gone (a
        // hand-edited file, or one written by a hub that crashed between
        // the two changes).
        let mut pruned = false;
        {
            let StateDoc {
                workspaces,
                sessions,
                layouts,
                templates,
                ..
            } = &mut doc;
            let before = layouts.len();
            layouts.retain(|id, _| workspaces.iter().any(|w| &w.id == id));
            pruned |= layouts.len() != before;
            let before = templates.len();
            templates.retain(|id, _| workspaces.iter().any(|w| &w.id == id));
            pruned |= templates.len() != before;
            for (workspace_id, layout) in layouts.iter_mut() {
                let mut stale: Vec<String> = Vec::new();
                for tab in &layout.tabs {
                    for (_, session) in tab.root.panes() {
                        if let Some(session) = session
                            && !sessions
                                .iter()
                                .any(|s| s.id == session && &s.workspace_id == workspace_id)
                        {
                            stale.push(session.to_owned());
                        }
                    }
                }
                let mut changed = false;
                for session in stale {
                    changed |= layout.prune_session(&session);
                }
                if changed {
                    layout.rev += 1;
                    pruned = true;
                }
            }
        }
        // A folder may have become (or stopped being) a git repo, or
        // disappeared, since.
        for workspace in &mut doc.workspaces {
            workspace.refresh_disk();
        }
        let now = Utc::now();
        let mut demoted = false;
        for session in &mut doc.sessions {
            if session.state.is_running() && !live.contains(&session.id) {
                session.state = SessionState::Stopped {
                    at: now,
                    clean: false,
                    restore_error: Some(UNCLEAN_STOP.to_owned()),
                    last_cwd: None,
                };
                demoted = true;
            }
        }
        let store = Self {
            path,
            doc,
            read_only: false,
        };
        if demoted || migrated || pruned {
            store.save()?;
        }
        Ok(store)
    }

    /// Keep changes in memory only (`true`), or write them again
    /// (`false`, saving what changed meanwhile).
    pub(crate) fn set_read_only(&mut self, read_only: bool) -> Result<(), StateError> {
        let was = std::mem::replace(&mut self.read_only, read_only);
        if was && !read_only {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<(), StateError> {
        if self.read_only {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&self.doc).map_err(|e| StateError::Persist {
            path: self.path.clone(),
            message: e.to_string(),
        })?;
        write_private(&self.path, &bytes).map_err(|e| StateError::Persist {
            path: self.path.clone(),
            message: e.to_string(),
        })
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.doc.workspaces
    }

    pub fn workspace(&self, id: &str) -> Result<&Workspace, StateError> {
        self.doc
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| StateError::NotFound(format!("workspace {id}")))
    }

    pub fn sessions(&self) -> &[SessionRecord] {
        &self.doc.sessions
    }

    pub fn session(&self, id: &str) -> Result<&SessionRecord, StateError> {
        self.doc
            .sessions
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| StateError::NotFound(format!("session {id}")))
    }

    /// Create a workspace; `name` and at least one folder are required.
    pub fn create_workspace(&mut self, input: WorkspaceInput) -> Result<Workspace, StateError> {
        let name = validate_name(input.name.as_deref().unwrap_or(""))?;
        self.ensure_unique_name(&name, None)?;
        let folders = validate_folders(&input.folder_specs()?.unwrap_or_default(), &[])?;
        let workspace = Workspace {
            id: new_id(),
            name,
            folders,
            created_at: Utc::now(),
        };
        self.doc.workspaces.push(workspace.clone());
        self.save_or_rollback(|doc| {
            doc.workspaces.pop();
        })?;
        Ok(workspace)
    }

    /// The workspace named exactly `name`.
    pub fn workspace_named(&self, name: &str) -> Option<&Workspace> {
        self.doc.workspaces.iter().find(|w| w.name == name)
    }

    /// Rename a workspace and/or replace its folders (refused when that
    /// drops a folder a running session is in).
    pub fn update_workspace(
        &mut self,
        id: &str,
        input: WorkspaceInput,
    ) -> Result<Workspace, StateError> {
        let index = self.workspace_index(id)?;
        let name = input.name.as_deref().map(validate_name).transpose()?;
        if let Some(name) = &name {
            self.ensure_unique_name(name, Some(id))?;
        }
        let folders = input
            .folder_specs()?
            .map(|specs| validate_folders(&specs, &self.doc.workspaces[index].folders))
            .transpose()?;
        if let Some(folders) = &folders
            && let Some(stranded) = self.doc.sessions.iter().find(|s| {
                s.workspace_id == id
                    && s.state.is_running()
                    && !folders.iter().any(|f| f.path == s.folder())
            })
        {
            return Err(StateError::Conflict(format!(
                "session {:?} is running in {}, which the new folder list drops; kill it first",
                stranded.name,
                stranded.folder().display()
            )));
        }
        let previous = self.doc.workspaces[index].clone();
        let workspace = &mut self.doc.workspaces[index];
        if let Some(name) = name {
            workspace.name = name;
        }
        if let Some(folders) = folders {
            workspace.folders = folders;
        }
        let updated = workspace.clone();
        self.save_or_rollback(|doc| doc.workspaces[index] = previous)?;
        Ok(updated)
    }

    /// Create workspace `name` with `folders`, or -- with `update` --
    /// replace the folders of the one already so named (refused, like
    /// [`Self::update_workspace`], when that drops a folder a running
    /// session is in). Returns it and whether it was created.
    pub fn import_workspace(
        &mut self,
        name: &str,
        folders: Vec<FolderSpec>,
        update: bool,
    ) -> Result<(Workspace, bool), StateError> {
        let name = validate_name(name)?;
        let input = |name: Option<String>| WorkspaceInput {
            name,
            folders: Some(folders.clone()),
            repos: None,
        };
        match self.workspace_named(&name).map(|w| w.id.clone()) {
            Some(id) if update => Ok((self.update_workspace(&id, input(None))?, false)),
            Some(_) => Err(StateError::Conflict(format!(
                "a workspace named {name:?} already exists; update it (fw hub import --update) to replace its folders"
            ))),
            None => Ok((self.create_workspace(input(Some(name)))?, true)),
        }
    }

    /// Delete a workspace and its (non-running) session records, returning
    /// the removed session ids. Refused while any of its sessions is running.
    pub fn delete_workspace(&mut self, id: &str) -> Result<Vec<String>, StateError> {
        let index = self.workspace_index(id)?;
        let running = self
            .doc
            .sessions
            .iter()
            .filter(|s| s.workspace_id == id && s.state.is_running())
            .count();
        if running > 0 {
            return Err(StateError::Conflict(format!(
                "workspace {:?} has {running} running session{}; stop {} first",
                self.doc.workspaces[index].name,
                if running == 1 { "" } else { "s" },
                if running == 1 { "it" } else { "them" },
            )));
        }
        let previous = self.doc.clone();
        self.doc.workspaces.remove(index);
        let removed = self
            .doc
            .sessions
            .iter()
            .filter(|s| s.workspace_id == id)
            .map(|s| s.id.clone())
            .collect();
        self.doc.sessions.retain(|s| s.workspace_id != id);
        self.doc.layouts.remove(id);
        self.doc.templates.remove(id);
        self.save_or_rollback(|doc| *doc = previous)?;
        Ok(removed)
    }

    /// Record a newly spawned session: last, or -- a companion shell --
    /// right after its agent's other companions and the agent itself, so
    /// every listing keeps the pair together.
    pub fn insert_session(&mut self, record: SessionRecord) -> Result<(), StateError> {
        self.workspace(&record.workspace_id)?;
        let at = record
            .companion_of
            .as_deref()
            .and_then(|agent| {
                let sessions = &self.doc.sessions;
                let start = sessions.iter().position(|s| s.id == agent)?;
                let after = sessions[start + 1..]
                    .iter()
                    .take_while(|s| s.companion_of.as_deref() == Some(agent))
                    .count();
                Some(start + 1 + after)
            })
            .unwrap_or(self.doc.sessions.len());
        self.doc.sessions.insert(at, record);
        self.save_or_rollback(|doc| {
            doc.sessions.remove(at);
        })
    }

    /// Record a running session's exit. A session already recorded stopped
    /// (the hub hung it up on shutdown) keeps that state.
    pub fn mark_exited(&mut self, id: &str, exit: ExitInfo) -> Result<(), StateError> {
        let Some(session) = self
            .doc
            .sessions
            .iter_mut()
            .find(|s| s.id == id && s.state.is_running())
        else {
            // Removed, or stopped by shutdown: nothing to record.
            return Ok(());
        };
        session.state = SessionState::Exited {
            exit,
            at: Utc::now(),
        };
        self.save()
    }

    /// Record a relaunched session running again (same id, name, and
    /// place in its workspace). Refused while it still runs.
    pub fn mark_running(&mut self, id: &str) -> Result<SessionRecord, StateError> {
        let index = self
            .doc
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| StateError::NotFound(format!("session {id}")))?;
        if self.doc.sessions[index].state.is_running() {
            return Err(StateError::Conflict(format!(
                "session {:?} is still running",
                self.doc.sessions[index].name
            )));
        }
        let previous = self.doc.sessions[index].clone();
        let session = &mut self.doc.sessions[index];
        session.state = SessionState::Running;
        session.fallback_command = None;
        self.save_or_rollback(|doc| doc.sessions[index] = previous)?;
        Ok(self.doc.sessions[index].clone())
    }

    /// Mark every running session stopped by a clean shutdown (restored
    /// on the next start), with the working directory each was in, where
    /// known (`last_cwds`, by session id).
    pub fn mark_all_stopped(
        &mut self,
        last_cwds: &HashMap<String, PathBuf>,
    ) -> Result<(), StateError> {
        let now = Utc::now();
        for session in &mut self.doc.sessions {
            if session.state.is_running() {
                session.state = SessionState::Stopped {
                    at: now,
                    clean: true,
                    restore_error: None,
                    last_cwd: last_cwds.get(&session.id).cloned(),
                };
            }
        }
        self.save()
    }

    /// Record a session restored after a clean shutdown: running again
    /// (same id, name, kind, command, and place), as itself or -- with
    /// `fallback` -- running that in its place (an agent coming back as a
    /// shell). Its kind and command are never rewritten, so a later
    /// restore can still bring it back as itself.
    pub fn mark_restored(
        &mut self,
        id: &str,
        fallback: Option<Vec<String>>,
    ) -> Result<SessionRecord, StateError> {
        let index = self.session_index(id)?;
        if !self.doc.sessions[index].state.is_restorable() {
            return Err(StateError::Conflict(format!(
                "session {:?} was not stopped by a clean shutdown",
                self.doc.sessions[index].name
            )));
        }
        let previous = self.doc.sessions[index].clone();
        let session = &mut self.doc.sessions[index];
        session.fallback_command = fallback;
        session.state = SessionState::Running;
        self.save_or_rollback(|doc| doc.sessions[index] = previous)?;
        Ok(self.doc.sessions[index].clone())
    }

    /// Drop the pending restore of every cleanly stopped session (the hub
    /// started with restore off): they stay stopped until relaunched, on
    /// this and every later start. Returns how many there were.
    pub fn forgo_restore(&mut self) -> Result<usize, StateError> {
        let previous = self.doc.sessions.clone();
        let mut count = 0;
        for session in &mut self.doc.sessions {
            if let SessionState::Stopped {
                clean,
                restore_error,
                ..
            } = &mut session.state
                && *clean
            {
                *clean = false;
                *restore_error = None;
                count += 1;
            }
        }
        if count > 0 {
            self.save_or_rollback(|doc| doc.sessions = previous)?;
        }
        Ok(count)
    }

    /// Record why restoring a cleanly stopped session failed; it stays
    /// stopped (and restorable, so a later start retries).
    pub fn set_restore_error(&mut self, id: &str, message: &str) -> Result<(), StateError> {
        let index = self.session_index(id)?;
        let SessionState::Stopped { restore_error, .. } = &mut self.doc.sessions[index].state
        else {
            return Err(StateError::Conflict(format!("session {id} is not stopped")));
        };
        let previous = restore_error.replace(message.to_owned());
        self.save_or_rollback(|doc| {
            if let SessionState::Stopped { restore_error, .. } = &mut doc.sessions[index].state {
                *restore_error = previous;
            }
        })
    }

    fn session_index(&self, id: &str) -> Result<usize, StateError> {
        self.doc
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| StateError::NotFound(format!("session {id}")))
    }

    /// Rename a session.
    pub fn rename_session(&mut self, id: &str, name: &str) -> Result<SessionRecord, StateError> {
        let name = validate_name(name)?;
        let index = self
            .doc
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| StateError::NotFound(format!("session {id}")))?;
        let previous = std::mem::replace(&mut self.doc.sessions[index].name, name);
        self.save_or_rollback(|doc| doc.sessions[index].name = previous)?;
        Ok(self.doc.sessions[index].clone())
    }

    /// Record the conversation id an agent reported (Claude Code moves to
    /// a new one on `/clear`). Returns whether it changed.
    pub fn set_agent_session_id(
        &mut self,
        id: &str,
        agent_session_id: &str,
    ) -> Result<bool, StateError> {
        let index = self
            .doc
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| StateError::NotFound(format!("session {id}")))?;
        if self.doc.sessions[index].agent_session_id.as_deref() == Some(agent_session_id) {
            return Ok(false);
        }
        let previous = self.doc.sessions[index]
            .agent_session_id
            .replace(agent_session_id.to_owned());
        self.save_or_rollback(|doc| doc.sessions[index].agent_session_id = previous)?;
        Ok(true)
    }

    /// Forget a session record, closing the panes that showed it. Returns
    /// the workspace whose layout that changed, with its new revision.
    pub fn remove_session(&mut self, id: &str) -> Result<Option<(String, u64)>, StateError> {
        let index = self
            .doc
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| StateError::NotFound(format!("session {id}")))?;
        let removed = self.doc.sessions.remove(index);
        let workspace_id = removed.workspace_id.clone();
        let previous_layout = self.doc.layouts.get(&workspace_id).cloned();
        let relaid = self
            .doc
            .layouts
            .get_mut(&workspace_id)
            .filter(|layout| layout.shows(id))
            .map(|layout| {
                layout.prune_session(id);
                layout.rev += 1;
                (workspace_id.clone(), layout.rev)
            });
        self.save_or_rollback(|doc| {
            doc.sessions.insert(index, removed);
            if let Some(layout) = previous_layout {
                doc.layouts.insert(workspace_id, layout);
            }
        })?;
        Ok(relaid)
    }

    /// A workspace's layout (`rev` 0, no tabs: never saved).
    pub fn layout(&self, workspace_id: &str) -> Result<Layout, StateError> {
        self.workspace(workspace_id)?;
        Ok(self
            .doc
            .layouts
            .get(workspace_id)
            .cloned()
            .unwrap_or_default())
    }

    /// Replace a workspace's layout, written against revision `rev` (the
    /// one the client last read): a stale `rev` is a conflict. Panes may
    /// show only the workspace's own sessions. Returns the saved layout
    /// (its `rev` one higher).
    pub fn set_layout(
        &mut self,
        workspace_id: &str,
        rev: u64,
        mut tabs: Vec<Tab>,
    ) -> Result<Layout, StateError> {
        let current = self.layout(workspace_id)?;
        if current.rev != rev {
            return Err(StateError::Conflict(format!(
                "the layout changed meanwhile (revision {}, not {rev}); reload it",
                current.rev
            )));
        }
        let sessions: HashSet<&str> = self
            .doc
            .sessions
            .iter()
            .filter(|s| s.workspace_id == workspace_id)
            .map(|s| s.id.as_str())
            .collect();
        crate::layout::validate(&mut tabs, &sessions).map_err(StateError::Invalid)?;
        let layout = Layout {
            rev: current.rev + 1,
            tabs,
        };
        let previous = self
            .doc
            .layouts
            .insert(workspace_id.to_owned(), layout.clone());
        self.save_or_rollback(|doc| match previous {
            Some(previous) => {
                doc.layouts.insert(workspace_id.to_owned(), previous);
            }
            None => {
                doc.layouts.remove(workspace_id);
            }
        })?;
        Ok(layout)
    }

    /// A workspace's launch templates, by name.
    pub fn templates(&self, workspace_id: &str) -> Result<Vec<LaunchTemplate>, StateError> {
        self.workspace(workspace_id)?;
        Ok(self
            .doc
            .templates
            .get(workspace_id)
            .cloned()
            .unwrap_or_default())
    }

    /// Every workspace's templates (workspace id -> templates).
    pub fn all_templates(&self) -> &BTreeMap<String, Vec<LaunchTemplate>> {
        &self.doc.templates
    }

    /// Save a template. One of the same name is replaced only when
    /// `replace` (otherwise: a conflict, so a save never silently drops
    /// another's options).
    pub fn save_template(
        &mut self,
        workspace_id: &str,
        template: LaunchTemplate,
        replace: bool,
    ) -> Result<LaunchTemplate, StateError> {
        self.workspace(workspace_id)?;
        let mut template = template.validated()?;
        template.updated_at = Utc::now();
        let previous = self.doc.templates.get(workspace_id).cloned();
        let list = self
            .doc
            .templates
            .entry(workspace_id.to_owned())
            .or_default();
        match list.iter().position(|t| t.name == template.name) {
            Some(_) if !replace => {
                return Err(StateError::Conflict(format!(
                    "a template named {:?} already exists; replace it (every option it had is overwritten) or pick another name",
                    template.name
                )));
            }
            Some(at) => list[at] = template.clone(),
            None if list.len() >= MAX_TEMPLATES => {
                return Err(StateError::Conflict(format!(
                    "a workspace keeps at most {MAX_TEMPLATES} templates; remove one first"
                )));
            }
            None => {
                list.push(template.clone());
                list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            }
        }
        self.save_or_rollback(|doc| match previous {
            Some(previous) => {
                doc.templates.insert(workspace_id.to_owned(), previous);
            }
            None => {
                doc.templates.remove(workspace_id);
            }
        })?;
        Ok(template)
    }

    /// Remove the workspace's template `name`.
    pub fn remove_template(&mut self, workspace_id: &str, name: &str) -> Result<(), StateError> {
        let workspace = self.workspace(workspace_id)?.name.clone();
        let name = name.trim();
        let list = self.doc.templates.get_mut(workspace_id);
        let Some(at) = list
            .as_ref()
            .and_then(|l| l.iter().position(|t| t.name == name))
        else {
            return Err(StateError::NotFound(format!(
                "template {name:?} in workspace {workspace:?}"
            )));
        };
        let list = list.expect("found in it");
        let removed = list.remove(at);
        if list.is_empty() {
            self.doc.templates.remove(workspace_id);
        }
        self.save_or_rollback(|doc| {
            let list = doc.templates.entry(workspace_id.to_owned()).or_default();
            list.insert(at.min(list.len()), removed);
        })
    }

    fn workspace_index(&self, id: &str) -> Result<usize, StateError> {
        self.doc
            .workspaces
            .iter()
            .position(|w| w.id == id)
            .ok_or_else(|| StateError::NotFound(format!("workspace {id}")))
    }

    fn ensure_unique_name(&self, name: &str, except: Option<&str>) -> Result<(), StateError> {
        if self
            .doc
            .workspaces
            .iter()
            .any(|w| w.name == name && Some(w.id.as_str()) != except)
        {
            return Err(StateError::Conflict(format!(
                "a workspace named {name:?} already exists"
            )));
        }
        Ok(())
    }

    /// Persist; on failure undo the in-memory change so memory and disk
    /// never diverge.
    fn save_or_rollback(&mut self, undo: impl FnOnce(&mut StateDoc)) -> Result<(), StateError> {
        let result = self.save();
        if result.is_err() {
            undo(&mut self.doc);
        }
        result
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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
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
mod tests {
    use super::*;

    fn git_repo(root: &Path, name: &str) -> PathBuf {
        let repo = root.join(name);
        fs::create_dir_all(repo.join(".git")).unwrap();
        repo
    }

    fn input(name: &str, repos: Vec<PathBuf>) -> WorkspaceInput {
        WorkspaceInput {
            name: Some(name.into()),
            repos: Some(repos),
            ..WorkspaceInput::default()
        }
    }

    fn paths(workspace: &Workspace) -> Vec<PathBuf> {
        workspace.folders.iter().map(|f| f.path.clone()).collect()
    }

    #[test]
    fn templates_are_saved_replaced_removed_and_go_with_their_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(WorkspaceInput {
                name: Some("w".into()),
                folders: Some(vec![FolderSpec::Path(repo.clone())]),
                repos: None,
            })
            .unwrap();
        let template = |name: &str, model: &str| LaunchTemplate {
            name: name.into(),
            kind: SessionKind::Claude,
            prompt: Some("Fix the flaky test.\nThen run it.\n".into()),
            launch: AgentLaunch {
                model: Some(model.into()),
                permission_mode: Some("plan".into()),
                ..AgentLaunch::default()
            },
            worktree: true,
            updated_at: Utc::now(),
        };
        let saved = store
            .save_template(&ws.id, template(" fix ", "opus"), false)
            .unwrap();
        assert_eq!(saved.name, "fix");
        assert_eq!(
            saved.prompt.as_deref(),
            Some("Fix the flaky test.\nThen run it.")
        );
        store
            .save_template(&ws.id, template("Audit", "sonnet"), false)
            .unwrap();
        // Same name: refused unless replacing, then replaced, not added.
        assert!(matches!(
            store.save_template(&ws.id, template("fix ", "haiku"), false),
            Err(StateError::Conflict(_))
        ));
        store
            .save_template(&ws.id, template("fix", "haiku"), true)
            .unwrap();
        let names = |store: &HubStore| -> Vec<(String, Option<String>)> {
            store
                .templates(&ws.id)
                .unwrap()
                .into_iter()
                .map(|t| (t.name, t.launch.model))
                .collect()
        };
        assert_eq!(
            names(&store),
            [
                ("Audit".to_owned(), Some("sonnet".to_owned())),
                ("fix".to_owned(), Some("haiku".to_owned()))
            ]
        );
        // Checked like a launch.
        let mut bad = template("bad", "opus");
        bad.kind = SessionKind::Shell;
        bad.prompt = None;
        bad.launch = AgentLaunch::default();
        assert!(matches!(
            store.save_template(&ws.id, bad, false),
            Err(StateError::Invalid(_))
        ));
        let mut bad = template("bad", "opus");
        bad.launch.approval = Some("never".into());
        assert!(matches!(
            store.save_template(&ws.id, bad, false),
            Err(StateError::Invalid(_))
        ));
        assert!(matches!(
            store.save_template("nope", template("x", "opus"), false),
            Err(StateError::NotFound(_))
        ));
        // Persisted (schema 3), and loaded back.
        drop(store);
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(raw["version"], 3);
        assert_eq!(raw["templates"][&ws.id][1]["permission_mode"], "plan");
        let mut store = HubStore::open(dir.path()).unwrap();
        assert_eq!(names(&store).len(), 2);
        store.remove_template(&ws.id, "fix").unwrap();
        assert!(matches!(
            store.remove_template(&ws.id, "fix"),
            Err(StateError::NotFound(_))
        ));
        assert_eq!(names(&store).len(), 1);
        // Deleting the workspace deletes its templates.
        store.delete_workspace(&ws.id).unwrap();
        assert!(store.all_templates().is_empty());
    }

    fn running_session(workspace_id: &str, cwd: &Path) -> SessionRecord {
        SessionRecord {
            id: new_id(),
            workspace_id: workspace_id.into(),
            name: "shell".into(),
            kind: SessionKind::Shell,
            cwd: cwd.into(),
            command: vec!["/bin/sh".into()],
            agent_session_id: None,
            created_at: Utc::now(),
            state: SessionState::Running,
            fallback_command: None,
            worktree: None,
            companion_of: None,
            launch: None,
        }
    }

    #[test]
    fn agent_session_ids_persist_and_are_omitted_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let session = running_session(&ws.id, &repo);
        store.insert_session(session.clone()).unwrap();
        let raw = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(!raw.contains("agent_session_id"), "{raw}");
        assert!(store.set_agent_session_id(&session.id, "conv-1").unwrap());
        assert!(!store.set_agent_session_id(&session.id, "conv-1").unwrap());
        let reopened = HubStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened
                .session(&session.id)
                .unwrap()
                .agent_session_id
                .as_deref(),
            Some("conv-1")
        );
        assert!(matches!(
            store.set_agent_session_id("missing", "x"),
            Err(StateError::NotFound(_))
        ));
    }

    #[test]
    fn workspace_crud_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let a = git_repo(dir.path(), "a");
        let b = git_repo(dir.path(), "b");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("  proj  ", vec![a.clone(), b.clone(), a.clone()]))
            .unwrap();
        assert_eq!(ws.name, "proj");
        assert_eq!(
            paths(&ws),
            vec![a.canonicalize().unwrap(), b.canonicalize().unwrap()]
        );
        let renamed = store
            .update_workspace(
                &ws.id,
                WorkspaceInput {
                    name: Some("renamed".into()),
                    ..WorkspaceInput::default()
                },
            )
            .unwrap();
        assert_eq!(renamed.folders.len(), 2);

        let reopened = HubStore::open(dir.path()).unwrap();
        assert_eq!(reopened.workspaces(), std::slice::from_ref(&renamed));

        let mut store = reopened;
        store.delete_workspace(&ws.id).unwrap();
        assert!(store.workspaces().is_empty());
        assert!(HubStore::open(dir.path()).unwrap().workspaces().is_empty());
        assert!(matches!(
            store.delete_workspace(&ws.id),
            Err(StateError::NotFound(_))
        ));
    }

    #[test]
    fn rejects_bad_workspace_input() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        let invalid = |r: Result<Workspace, StateError>| matches!(r, Err(StateError::Invalid(_)));
        assert!(invalid(
            store.create_workspace(input(" ", vec![repo.clone()]))
        ));
        assert!(invalid(store.create_workspace(input("x", vec![]))));
        assert!(invalid(
            store.create_workspace(input("x", vec![PathBuf::from("rel")]))
        ));
        assert!(invalid(
            store.create_workspace(input("x", vec![dir.path().join("missing")]))
        ));
        assert!(invalid(store.create_workspace(input("x", vec![file]))));
        assert!(invalid(store.create_workspace(WorkspaceInput {
            name: Some("x".into()),
            folders: Some(vec![FolderSpec::Path(repo.clone())]),
            repos: Some(vec![repo.clone()]),
        })));
        assert!(invalid(store.create_workspace(WorkspaceInput {
            name: Some("x".into()),
            folders: Some(vec![FolderSpec::Labeled {
                path: repo.clone(),
                name: Some("a\tb".into()),
            }]),
            repos: None,
        })));
        assert!(invalid(
            store.create_workspace(input("a\nb", vec![repo.clone()]))
        ));
        store
            .create_workspace(input("x", vec![repo.clone()]))
            .unwrap();
        assert!(matches!(
            store.create_workspace(input("x", vec![repo])),
            Err(StateError::Conflict(_))
        ));
    }

    #[test]
    fn linked_worktree_with_git_file_is_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        fs::create_dir(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert!(is_git_root(&wt));
        let folders = validate_folders(&[FolderSpec::Path(wt)], &[]).unwrap();
        assert!(folders[0].git);
    }

    #[test]
    fn running_sessions_block_workspace_delete_and_load_as_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let session = running_session(&ws.id, &repo);
        store.insert_session(session.clone()).unwrap();
        match store.delete_workspace(&ws.id) {
            // Named for the operator, not by its id.
            Err(StateError::Conflict(message)) => {
                assert_eq!(
                    message,
                    "workspace \"w\" has 1 running session; stop it first"
                );
            }
            other => panic!("unexpected {other:?}"),
        }

        let reopened = HubStore::open(dir.path()).unwrap();
        assert!(matches!(
            reopened.session(&session.id).unwrap().state,
            SessionState::Stopped { .. }
        ));
        let mut store = reopened;
        store.delete_workspace(&ws.id).unwrap();
        assert!(store.sessions().is_empty());
    }

    #[test]
    fn repo_changes_may_not_strand_a_running_session() {
        let dir = tempfile::tempdir().unwrap();
        let a = git_repo(dir.path(), "a").canonicalize().unwrap();
        let b = git_repo(dir.path(), "b").canonicalize().unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![a.clone(), b.clone()]))
            .unwrap();
        store.insert_session(running_session(&ws.id, &a)).unwrap();
        let repos = |r: Vec<PathBuf>| WorkspaceInput {
            repos: Some(r),
            ..WorkspaceInput::default()
        };
        assert!(matches!(
            store.update_workspace(&ws.id, repos(vec![b.clone()])),
            Err(StateError::Conflict(_))
        ));
        assert_eq!(store.workspace(&ws.id).unwrap().folders.len(), 2);
        // Dropping a repo no running session uses is fine.
        let updated = store
            .update_workspace(&ws.id, repos(vec![a.clone()]))
            .unwrap();
        assert_eq!(paths(&updated), vec![a]);
    }

    #[test]
    fn plain_directories_are_folders_flagged_not_git_and_rechecked_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "repo").canonicalize().unwrap();
        let plain = dir.path().join("notes");
        fs::create_dir(&plain).unwrap();
        let plain = plain.canonicalize().unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(WorkspaceInput {
                name: Some("life".into()),
                folders: Some(vec![
                    FolderSpec::Labeled {
                        path: repo.clone(),
                        name: Some(" Main ".into()),
                    },
                    FolderSpec::Path(plain.clone()),
                ]),
                repos: None,
            })
            .unwrap();
        assert_eq!(
            ws.folders,
            vec![
                Folder {
                    path: repo.clone(),
                    name: Some("Main".into()),
                    git: true,
                    missing: false,
                },
                Folder {
                    path: plain.clone(),
                    name: None,
                    git: false,
                    missing: false,
                },
            ]
        );
        // Persisted with a legacy `repos` mirror of every folder.
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(
            raw["workspaces"][0]["repos"],
            serde_json::json!([repo, plain])
        );
        // The flag follows the disk on the next load.
        fs::create_dir(plain.join(".git")).unwrap();
        let reopened = HubStore::open(dir.path()).unwrap();
        assert!(reopened.workspaces()[0].folders.iter().all(|f| f.git));
        // An update that names a path without a label keeps its label;
        // an empty label clears it.
        let mut store = reopened;
        let kept = store
            .update_workspace(
                &ws.id,
                WorkspaceInput {
                    folders: Some(vec![FolderSpec::Path(repo.clone())]),
                    ..WorkspaceInput::default()
                },
            )
            .unwrap();
        assert_eq!(kept.folders[0].name.as_deref(), Some("Main"));
        let cleared = store
            .update_workspace(
                &ws.id,
                WorkspaceInput {
                    folders: Some(vec![FolderSpec::Labeled {
                        path: repo.clone(),
                        name: Some(String::new()),
                    }]),
                    ..WorkspaceInput::default()
                },
            )
            .unwrap();
        assert_eq!(cleared.folders[0].name, None);
    }

    #[test]
    fn a_folder_gone_from_disk_is_flagged_missing_and_may_stay_in_an_edit() {
        let dir = tempfile::tempdir().unwrap();
        let a = git_repo(dir.path(), "a").canonicalize().unwrap();
        let plain = dir.path().join("b");
        fs::create_dir(&plain).unwrap();
        let b = plain.canonicalize().unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![a.clone(), b.clone()]))
            .unwrap();
        assert!(ws.folders.iter().all(|f| !f.missing));
        fs::rename(&b, dir.path().join("b-moved")).unwrap();
        // Not mistaken for a plain folder: flagged on load and on refresh.
        let mut listed = ws.clone();
        listed.refresh_disk();
        assert!(listed.folders[1].missing && !listed.folders[1].git);
        let mut store2 = HubStore::open(dir.path()).unwrap();
        assert!(store2.workspaces()[0].folders[1].missing);
        assert!(!store2.workspaces()[0].folders[0].missing);
        // An edit that keeps it (and changes the rest) is accepted; adding
        // a different missing path is not.
        let kept = store2
            .update_workspace(&ws.id, input("w2", vec![b.clone()]))
            .unwrap();
        assert!(kept.folders[0].missing);
        assert!(matches!(
            store2.update_workspace(&ws.id, input("w2", vec![dir.path().join("nope")])),
            Err(StateError::Invalid(_))
        ));
        drop(store);
    }

    #[test]
    fn a_legacy_repos_state_file_loads_as_folders() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r").canonicalize().unwrap();
        let legacy = serde_json::json!({
            "version": 1,
            "workspaces": [{
                "id": "w1",
                "name": "old",
                "repos": [repo],
                "created_at": "2026-10-01T00:00:00Z"
            }],
            "sessions": []
        });
        fs::write(
            dir.path().join(STATE_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let store = HubStore::open(dir.path()).unwrap();
        let ws = &store.workspaces()[0];
        assert_eq!(ws.folders.len(), 1);
        assert_eq!(ws.folders[0].path, repo);
        assert!(ws.folders[0].git, "recomputed on load");
        assert!(ws.has_folder(&repo));
    }

    #[test]
    fn import_creates_or_with_update_replaces_folders() {
        let dir = tempfile::tempdir().unwrap();
        let a = git_repo(dir.path(), "a").canonicalize().unwrap();
        let b = git_repo(dir.path(), "b").canonicalize().unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        let (ws, created) = store
            .import_workspace("proj", vec![FolderSpec::Path(a.clone())], false)
            .unwrap();
        assert!(created);
        assert!(matches!(
            store.import_workspace("proj", vec![FolderSpec::Path(b.clone())], false),
            Err(StateError::Conflict(_))
        ));
        store.insert_session(running_session(&ws.id, &a)).unwrap();
        // Update may not drop the folder a running session is in.
        assert!(matches!(
            store.import_workspace("proj", vec![FolderSpec::Path(b.clone())], true),
            Err(StateError::Conflict(_))
        ));
        let (updated, created) = store
            .import_workspace(
                "proj",
                vec![FolderSpec::Path(a.clone()), FolderSpec::Path(b.clone())],
                true,
            )
            .unwrap();
        assert!(!created);
        assert_eq!(updated.id, ws.id);
        assert_eq!(paths(&updated), vec![a, b]);
    }

    #[test]
    fn a_session_stopped_by_shutdown_stays_stopped_when_its_exit_lands() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let session = running_session(&ws.id, &repo);
        store.insert_session(session.clone()).unwrap();
        store.mark_all_stopped(&HashMap::new()).unwrap();
        store
            .mark_exited(
                &session.id,
                ExitInfo {
                    code: None,
                    signal: Some("Hangup".into()),
                },
            )
            .unwrap();
        assert!(matches!(
            HubStore::open(dir.path())
                .unwrap()
                .session(&session.id)
                .unwrap()
                .state,
            SessionState::Stopped { .. }
        ));
    }

    #[test]
    fn only_a_clean_shutdown_makes_sessions_restorable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let crashed = running_session(&ws.id, &repo);
        store.insert_session(crashed.clone()).unwrap();
        drop(store);
        // Found running on load: the hub died without recording it.
        let mut store = HubStore::open(dir.path()).unwrap();
        assert!(!store.session(&crashed.id).unwrap().state.is_restorable());
        let raw = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(!raw.contains("\"clean\""), "{raw}");
        // Says why it was not restored.
        assert!(matches!(
            &store.session(&crashed.id).unwrap().state,
            SessionState::Stopped { restore_error: Some(e), .. } if e == UNCLEAN_STOP
        ));
        assert!(matches!(
            store.mark_restored(&crashed.id, None),
            Err(StateError::Conflict(_))
        ));

        let clean = running_session(&ws.id, &repo);
        store.insert_session(clean.clone()).unwrap();
        let cwds = HashMap::from([(clean.id.clone(), PathBuf::from("/somewhere/deeper"))]);
        store.mark_all_stopped(&cwds).unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        assert!(store.session(&clean.id).unwrap().state.is_restorable());
        assert!(matches!(
            &store.session(&clean.id).unwrap().state,
            SessionState::Stopped { last_cwd: Some(cwd), .. } if cwd == Path::new("/somewhere/deeper")
        ));
        store.set_restore_error(&clean.id, "cwd gone").unwrap();
        let reopened = HubStore::open(dir.path()).unwrap();
        match &reopened.session(&clean.id).unwrap().state {
            SessionState::Stopped {
                clean: true,
                restore_error: Some(message),
                ..
            } => assert_eq!(message, "cwd gone"),
            other => panic!("unexpected {other:?}"),
        }
        let mut store = reopened;
        let restored = store
            .mark_restored(&clean.id, Some(vec!["/bin/bash".into()]))
            .unwrap();
        assert!(restored.state.is_running());
        // What it is stays recorded; what runs in its place is separate.
        assert_eq!(restored.command, vec!["/bin/sh".to_owned()]);
        assert_eq!(restored.kind, SessionKind::Shell);
        assert_eq!(
            restored.fallback_command,
            Some(vec!["/bin/bash".to_owned()])
        );
        // Same place in the session order.
        assert_eq!(store.sessions()[1].id, clean.id);
        assert!(matches!(
            store.set_restore_error(&clean.id, "x"),
            Err(StateError::Conflict(_))
        ));
    }

    #[test]
    fn forgoing_restore_keeps_sessions_stopped_on_later_starts() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let session = running_session(&ws.id, &repo);
        store.insert_session(session.clone()).unwrap();
        store.mark_all_stopped(&HashMap::new()).unwrap();
        store.set_restore_error(&session.id, "cwd gone").unwrap();
        assert_eq!(store.forgo_restore().unwrap(), 1);
        assert_eq!(store.forgo_restore().unwrap(), 0);
        // A later clean shutdown (nothing running) leaves it alone.
        store.mark_all_stopped(&HashMap::new()).unwrap();
        let store = HubStore::open(dir.path()).unwrap();
        assert!(matches!(
            store.session(&session.id).unwrap().state,
            SessionState::Stopped {
                clean: false,
                restore_error: None,
                ..
            }
        ));
    }

    #[test]
    fn mark_running_clears_a_restore_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let mut agent = running_session(&ws.id, &repo);
        agent.kind = SessionKind::Claude;
        agent.command = vec!["claude".into()];
        store.insert_session(agent.clone()).unwrap();
        store.mark_all_stopped(&HashMap::new()).unwrap();
        let restored = store
            .mark_restored(&agent.id, Some(vec!["/bin/sh".into()]))
            .unwrap();
        assert_eq!(restored.kind, SessionKind::Claude);
        assert_eq!(restored.command, vec!["claude".to_owned()]);
        store
            .mark_exited(
                &agent.id,
                ExitInfo {
                    code: Some(0),
                    signal: None,
                },
            )
            .unwrap();
        let relaunched = store.mark_running(&agent.id).unwrap();
        assert_eq!(relaunched.fallback_command, None);
        assert_eq!(relaunched.kind, SessionKind::Claude);
    }

    #[test]
    fn mark_running_relaunches_an_ended_record_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let session = running_session(&ws.id, &repo);
        store.insert_session(session.clone()).unwrap();
        assert!(matches!(
            store.mark_running(&session.id),
            Err(StateError::Conflict(_))
        ));
        store
            .mark_exited(
                &session.id,
                ExitInfo {
                    code: Some(1),
                    signal: None,
                },
            )
            .unwrap();
        let record = store.mark_running(&session.id).unwrap();
        assert_eq!(record.state, SessionState::Running);
        assert_eq!(record.name, session.name);
        assert_eq!(store.sessions().len(), 1);
        assert!(matches!(
            store.mark_running("missing"),
            Err(StateError::NotFound(_))
        ));
    }

    #[test]
    fn session_exit_and_removal_persist() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let session = running_session(&ws.id, &repo);
        store.insert_session(session.clone()).unwrap();
        store
            .mark_exited(
                &session.id,
                ExitInfo {
                    code: Some(3),
                    signal: None,
                },
            )
            .unwrap();
        let reopened = HubStore::open(dir.path()).unwrap();
        match &reopened.session(&session.id).unwrap().state {
            SessionState::Exited { exit, .. } => assert_eq!(exit.code, Some(3)),
            other => panic!("unexpected {other:?}"),
        }
        let mut store = reopened;
        store.remove_session(&session.id).unwrap();
        assert!(HubStore::open(dir.path()).unwrap().sessions().is_empty());
        // Unknown workspace is rejected.
        let mut orphan = running_session("nope", &repo);
        orphan.workspace_id = "nope".into();
        assert!(matches!(
            store.insert_session(orphan),
            Err(StateError::NotFound(_))
        ));
    }

    #[test]
    fn state_file_is_private_and_versioned() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        store.create_workspace(input("w", vec![repo])).unwrap();
        let path = dir.path().join(STATE_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::write(&path, r#"{"version": 99}"#).unwrap();
        assert!(matches!(
            HubStore::open(dir.path()),
            Err(StateError::Persist { .. })
        ));
        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            HubStore::open(dir.path()),
            Err(StateError::Persist { .. })
        ));
    }

    #[test]
    fn sessions_rename_persistently_and_reject_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let session = running_session(&ws.id, &repo);
        store.insert_session(session.clone()).unwrap();

        let renamed = store.rename_session(&session.id, "  build  ").unwrap();
        assert_eq!(renamed.name, "build");
        assert_eq!(
            HubStore::open(dir.path())
                .unwrap()
                .session(&session.id)
                .unwrap()
                .name,
            "build"
        );
        let mut store = HubStore::open(dir.path()).unwrap();
        assert!(matches!(
            store.rename_session(&session.id, " "),
            Err(StateError::Invalid(_))
        ));
        assert!(matches!(
            store.rename_session(&session.id, "a\tb"),
            Err(StateError::Invalid(_))
        ));
        assert!(matches!(
            store.rename_session("missing", "x"),
            Err(StateError::NotFound(_))
        ));
        assert_eq!(store.session(&session.id).unwrap().name, "build");
    }

    #[test]
    fn records_without_a_kind_load_as_shells_and_kinds_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let legacy = format!(
            r#"{{"version":1,"workspaces":[{{"id":"w1","name":"w","repos":[{repo:?}],
            "created_at":"2026-11-01T00:00:00Z"}}],"sessions":[{{"id":"s1","workspace_id":"w1",
            "name":"old","cwd":{repo:?},"command":["/bin/sh"],
            "created_at":"2026-11-01T00:00:00Z","state":"exited","code":0,"signal":null,
            "at":"2026-11-01T00:00:01Z"}}]}}"#
        );
        fs::write(dir.path().join(STATE_FILE), legacy).unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        assert_eq!(store.session("s1").unwrap().kind, SessionKind::Shell);

        let mut agent = running_session("w1", &repo);
        agent.kind = SessionKind::Codex;
        store.insert_session(agent.clone()).unwrap();
        let reopened = HubStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened.session(&agent.id).unwrap().kind,
            SessionKind::Codex
        );
    }

    #[test]
    fn tilde_expands_to_home_only_as_a_leading_component() {
        let home = Path::new("/home/me");
        assert_eq!(
            expand_home(Path::new("~/code/x"), Some(home)),
            PathBuf::from("/home/me/code/x")
        );
        assert_eq!(
            expand_home(Path::new("~"), Some(home)),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            expand_home(Path::new("~other/x"), Some(home)),
            PathBuf::from("~other/x")
        );
        assert_eq!(
            expand_home(Path::new("/a/~/b"), Some(home)),
            PathBuf::from("/a/~/b")
        );
        assert_eq!(expand_home(Path::new("~/x"), None), PathBuf::from("~/x"));
    }

    #[test]
    fn validate_folder_explains_each_failure() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let plain = dir.path().join("plain");
        fs::create_dir(&plain).unwrap();
        assert_eq!(
            validate_folder(&repo).unwrap(),
            repo.canonicalize().unwrap()
        );
        // Plain directories are folders too.
        assert_eq!(
            validate_folder(&plain).unwrap(),
            plain.canonicalize().unwrap()
        );
        assert!(!is_git_root(&plain));
        let message = |p: &Path| validate_folder(p).unwrap_err().to_string();
        assert!(message(Path::new("rel/x")).contains("must be absolute"));
        assert!(message(&dir.path().join("missing")).ends_with("missing does not exist"));
        assert!(message(&file).contains("not a directory"));
    }

    fn pane(id: &str, session: Option<&str>) -> crate::layout::Node {
        crate::layout::Node::Pane {
            id: id.into(),
            session: session.map(Into::into),
        }
    }

    fn two_pane_tab(id: &str, name: &str, a: &str, b: &str) -> Tab {
        Tab {
            id: id.into(),
            name: name.into(),
            root: crate::layout::Node::Split {
                dir: crate::layout::SplitDir::Right,
                ratio: 0.6,
                a: Box::new(pane(&format!("{id}a"), Some(a))),
                b: Box::new(pane(&format!("{id}b"), Some(b))),
            },
        }
    }

    #[test]
    fn layouts_persist_per_workspace_with_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r");
        let mut store = HubStore::open(dir.path()).unwrap();
        let ws = store
            .create_workspace(input("w", vec![repo.clone()]))
            .unwrap();
        let other = store
            .create_workspace(input("o", vec![repo.clone()]))
            .unwrap();
        let s1 = running_session(&ws.id, &repo);
        let s2 = running_session(&ws.id, &repo);
        let foreign = running_session(&other.id, &repo);
        for s in [&s1, &s2, &foreign] {
            store.insert_session(s.clone()).unwrap();
        }
        assert_eq!(store.layout(&ws.id).unwrap(), Layout::default());
        assert!(matches!(store.layout("nope"), Err(StateError::NotFound(_))));

        let saved = store
            .set_layout(
                &ws.id,
                0,
                vec![two_pane_tab("t1", "agents", &s1.id, &s2.id)],
            )
            .unwrap();
        assert_eq!(saved.rev, 1);
        // Written against an old revision: refused, nothing changes.
        assert!(matches!(
            store.set_layout(&ws.id, 0, Vec::new()),
            Err(StateError::Conflict(_))
        ));
        // Another workspace's session is not this layout's to show.
        assert!(matches!(
            store.set_layout(
                &ws.id,
                1,
                vec![two_pane_tab("t1", "a", &s1.id, &foreign.id)]
            ),
            Err(StateError::Invalid(_))
        ));
        assert_eq!(store.layout(&ws.id).unwrap(), saved);
        drop(store);

        // Survives a restart (running sessions load as stopped; layouts
        // keep naming them, so a restore re-attaches in place).
        let mut store = HubStore::open(dir.path()).unwrap();
        assert_eq!(store.layout(&ws.id).unwrap(), saved);
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(raw["version"], STATE_VERSION);
        assert_eq!(raw["layouts"][&ws.id]["rev"], 1);

        // Removing a session closes its pane and bumps the revision.
        assert_eq!(
            store.remove_session(&s2.id).unwrap(),
            Some((ws.id.clone(), 2))
        );
        let layout = store.layout(&ws.id).unwrap();
        assert_eq!(layout.tabs[0].root, pane("t1a", Some(&s1.id)));
        // A session no pane shows leaves the layout alone.
        assert_eq!(store.remove_session(&foreign.id).unwrap(), None);
        // Deleting the workspace drops its layout.
        store
            .mark_exited(
                &s1.id,
                ExitInfo {
                    code: Some(0),
                    signal: None,
                },
            )
            .unwrap();
        store.delete_workspace(&ws.id).unwrap();
        drop(store);
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert!(raw["layouts"].get(&ws.id).is_none(), "{raw}");
    }

    #[test]
    fn version_1_files_migrate_and_stale_layout_entries_are_pruned_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git_repo(dir.path(), "r").canonicalize().unwrap();
        let path = dir.path().join(STATE_FILE);
        fs::write(
            &path,
            format!(
                r#"{{"version":1,"workspaces":[{{"id":"w1","name":"w","repos":[{repo:?}],
                "created_at":"2026-01-01T00:00:00Z"}}],"sessions":[]}}"#
            ),
        )
        .unwrap();
        let store = HubStore::open(dir.path()).unwrap();
        assert_eq!(store.layout("w1").unwrap(), Layout::default());
        drop(store);
        let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            raw["version"], STATE_VERSION,
            "rewritten as the current version"
        );

        // A layout naming a gone session or workspace (hand edit, crash).
        raw["layouts"] = serde_json::json!({
            "w1": {"rev": 4, "tabs": [{"id": "t1", "name": "a", "root":
                {"type": "split", "dir": "down", "ratio": 0.5,
                 "a": {"type": "pane", "id": "p1", "session": "gone"},
                 "b": {"type": "pane", "id": "p2", "session": null}}}]},
            "w-gone": {"rev": 1, "tabs": []}
        });
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let store = HubStore::open(dir.path()).unwrap();
        let layout = store.layout("w1").unwrap();
        assert_eq!(layout.rev, 5);
        assert_eq!(layout.tabs[0].root, pane("p2", None));
        drop(store);
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(raw["layouts"].get("w-gone").is_none(), "{raw}");
    }
}
