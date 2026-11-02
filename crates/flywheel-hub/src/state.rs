//! The hub's persisted model: workspaces (a name plus 1..N git repos) and
//! session records, stored as one JSON document (`state.json`, mode 0600,
//! written atomically) in the state dir.
//!
//! Processes belong to the running server, not to this file: a session
//! recorded as `running` when the hub last stopped is loaded back as
//! `stopped`.

use crate::auth::write_private;
use crate::pty::ExitInfo;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// State file name inside the state dir.
pub const STATE_FILE: &str = "state.json";
/// On-disk schema version.
pub const STATE_VERSION: u32 = 1;
/// Limits on workspace input.
pub const MAX_NAME_CHARS: usize = 100;
pub const MAX_REPOS: usize = 32;

/// A named set of git repositories worked on together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    /// Canonical absolute paths of git work-tree roots, in operator order.
    pub repos: Vec<PathBuf>,
    pub created_at: DateTime<Utc>,
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
    },
}

impl SessionState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// A terminal session within a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(flatten)]
    pub state: SessionState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateDoc {
    version: u32,
    #[serde(default)]
    workspaces: Vec<Workspace>,
    #[serde(default)]
    sessions: Vec<SessionRecord>,
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

/// Workspace fields an operator supplies.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkspaceInput {
    pub name: Option<String>,
    pub repos: Option<Vec<PathBuf>>,
}

/// The persisted hub state.
#[derive(Debug)]
pub struct HubStore {
    path: PathBuf,
    doc: StateDoc,
}

impl HubStore {
    /// Load `state.json` from `state_dir` (empty when absent), demoting
    /// sessions left `running` to `stopped`.
    pub fn open(state_dir: &Path) -> Result<Self, StateError> {
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
        if doc.version != STATE_VERSION {
            return Err(persist_err(format!(
                "unsupported version {} (expected {STATE_VERSION})",
                doc.version
            )));
        }
        let now = Utc::now();
        let mut demoted = false;
        for session in &mut doc.sessions {
            if session.state.is_running() {
                session.state = SessionState::Stopped { at: now };
                demoted = true;
            }
        }
        let store = Self { path, doc };
        if demoted {
            store.save()?;
        }
        Ok(store)
    }

    fn save(&self) -> Result<(), StateError> {
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

    /// Create a workspace; `name` and at least one repo are required.
    pub fn create_workspace(&mut self, input: WorkspaceInput) -> Result<Workspace, StateError> {
        let name = validate_name(input.name.as_deref().unwrap_or(""))?;
        self.ensure_unique_name(&name, None)?;
        let repos = validate_repos(&input.repos.unwrap_or_default())?;
        let workspace = Workspace {
            id: new_id(),
            name,
            repos,
            created_at: Utc::now(),
        };
        self.doc.workspaces.push(workspace.clone());
        self.save_or_rollback(|doc| {
            doc.workspaces.pop();
        })?;
        Ok(workspace)
    }

    /// Rename a workspace and/or replace its repos.
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
        let repos = input.repos.as_deref().map(validate_repos).transpose()?;
        let previous = self.doc.workspaces[index].clone();
        let workspace = &mut self.doc.workspaces[index];
        if let Some(name) = name {
            workspace.name = name;
        }
        if let Some(repos) = repos {
            workspace.repos = repos;
        }
        let updated = workspace.clone();
        self.save_or_rollback(|doc| doc.workspaces[index] = previous)?;
        Ok(updated)
    }

    /// Delete a workspace and its (non-running) session records. Refused
    /// while any of its sessions is running.
    pub fn delete_workspace(&mut self, id: &str) -> Result<(), StateError> {
        let index = self.workspace_index(id)?;
        if self
            .doc
            .sessions
            .iter()
            .any(|s| s.workspace_id == id && s.state.is_running())
        {
            return Err(StateError::Conflict(format!(
                "workspace {id} has running sessions; kill them first"
            )));
        }
        let previous = self.doc.clone();
        self.doc.workspaces.remove(index);
        self.doc.sessions.retain(|s| s.workspace_id != id);
        self.save_or_rollback(|doc| *doc = previous)
    }

    /// Record a newly spawned session.
    pub fn insert_session(&mut self, record: SessionRecord) -> Result<(), StateError> {
        self.workspace(&record.workspace_id)?;
        self.doc.sessions.push(record);
        self.save_or_rollback(|doc| {
            doc.sessions.pop();
        })
    }

    /// Record a session's exit.
    pub fn mark_exited(&mut self, id: &str, exit: ExitInfo) -> Result<(), StateError> {
        let Some(session) = self.doc.sessions.iter_mut().find(|s| s.id == id) else {
            // Removed while it was shutting down: nothing to record.
            return Ok(());
        };
        session.state = SessionState::Exited {
            exit,
            at: Utc::now(),
        };
        self.save()
    }

    /// Mark every running session stopped (hub shutdown).
    pub fn mark_all_stopped(&mut self) -> Result<(), StateError> {
        let now = Utc::now();
        for session in &mut self.doc.sessions {
            if session.state.is_running() {
                session.state = SessionState::Stopped { at: now };
            }
        }
        self.save()
    }

    /// Forget a session record.
    pub fn remove_session(&mut self, id: &str) -> Result<(), StateError> {
        let index = self
            .doc
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| StateError::NotFound(format!("session {id}")))?;
        let removed = self.doc.sessions.remove(index);
        self.save_or_rollback(|doc| doc.sessions.insert(index, removed))
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

/// Canonicalize and check repo paths: 1..=MAX_REPOS absolute, existing git
/// work-tree roots (a `.git` dir, or a `.git` file for linked worktrees),
/// without duplicates.
pub fn validate_repos(raw: &[PathBuf]) -> Result<Vec<PathBuf>, StateError> {
    if raw.is_empty() {
        return Err(StateError::Invalid(
            "a workspace needs at least one repo".into(),
        ));
    }
    if raw.len() > MAX_REPOS {
        return Err(StateError::Invalid(format!(
            "a workspace holds at most {MAX_REPOS} repos"
        )));
    }
    let mut seen = HashSet::new();
    let mut repos = Vec::with_capacity(raw.len());
    for path in raw {
        if !path.is_absolute() {
            return Err(StateError::Invalid(format!(
                "repo path {} must be absolute",
                path.display()
            )));
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| StateError::Invalid(format!("repo path {}: {e}", path.display())))?;
        if !canonical.is_dir() {
            return Err(StateError::Invalid(format!(
                "repo path {} is not a directory",
                canonical.display()
            )));
        }
        if !canonical.join(".git").exists() {
            return Err(StateError::Invalid(format!(
                "repo path {} is not a git work-tree root (no .git)",
                canonical.display()
            )));
        }
        if seen.insert(canonical.clone()) {
            repos.push(canonical);
        }
    }
    Ok(repos)
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
        }
    }

    fn running_session(workspace_id: &str, cwd: &Path) -> SessionRecord {
        SessionRecord {
            id: new_id(),
            workspace_id: workspace_id.into(),
            name: "shell".into(),
            cwd: cwd.into(),
            command: vec!["/bin/sh".into()],
            created_at: Utc::now(),
            state: SessionState::Running,
        }
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
            ws.repos,
            vec![a.canonicalize().unwrap(), b.canonicalize().unwrap()]
        );
        let renamed = store
            .update_workspace(
                &ws.id,
                WorkspaceInput {
                    name: Some("renamed".into()),
                    repos: None,
                },
            )
            .unwrap();
        assert_eq!(renamed.repos.len(), 2);

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
        let plain = dir.path().join("plain");
        fs::create_dir(&plain).unwrap();
        let mut store = HubStore::open(dir.path()).unwrap();
        let invalid = |r: Result<Workspace, StateError>| matches!(r, Err(StateError::Invalid(_)));
        assert!(invalid(
            store.create_workspace(input(" ", vec![repo.clone()]))
        ));
        assert!(invalid(store.create_workspace(input("x", vec![]))));
        assert!(invalid(
            store.create_workspace(input("x", vec![PathBuf::from("rel")]))
        ));
        assert!(invalid(store.create_workspace(input("x", vec![plain]))));
        assert!(invalid(
            store.create_workspace(input("x", vec![dir.path().join("missing")]))
        ));
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
        assert_eq!(validate_repos(std::slice::from_ref(&wt)).unwrap().len(), 1);
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
        assert!(matches!(
            store.delete_workspace(&ws.id),
            Err(StateError::Conflict(_))
        ));

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
}
