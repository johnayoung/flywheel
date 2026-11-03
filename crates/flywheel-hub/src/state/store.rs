//! The store: the loaded document, its queries and edits, each saved
//! atomically (rolled back in memory when the save fails).

use super::{
    FolderSpec, LaunchTemplate, MAX_TEMPLATES, STATE_FILE, STATE_VERSION, STATE_VERSION_MIN,
    SessionRecord, SessionState, StateDoc, StateError, UNCLEAN_STOP, Workspace, WorkspaceInput,
    new_id, validate_folders, validate_name,
};
use crate::auth::write_private;
use crate::fanout::{Fanout, MAX_FANOUTS};
use crate::layout::{Layout, Tab};
use crate::pty::ExitInfo;
use chrono::Utc;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

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
                fanouts,
                ..
            } = &mut doc;
            let before = layouts.len();
            layouts.retain(|id, _| workspaces.iter().any(|w| &w.id == id));
            pruned |= layouts.len() != before;
            let before = templates.len();
            templates.retain(|id, _| workspaces.iter().any(|w| &w.id == id));
            pruned |= templates.len() != before;
            let before = fanouts.len();
            fanouts.retain(|f| workspaces.iter().any(|w| w.id == f.workspace_id));
            pruned |= fanouts.len() != before;
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
        self.doc.fanouts.retain(|f| f.workspace_id != id);
        self.save_or_rollback(|doc| *doc = previous)?;
        Ok(removed)
    }

    /// Record a newly spawned session: last, or -- a companion shell or a
    /// reviewer -- right after the session it belongs to and that one's
    /// other companions and reviewers, so every listing keeps them
    /// together.
    pub fn insert_session(&mut self, record: SessionRecord) -> Result<(), StateError> {
        self.workspace(&record.workspace_id)?;
        let at = record
            .companion_of
            .as_deref()
            .or(record.review_of.as_deref())
            .and_then(|anchor| {
                let sessions = &self.doc.sessions;
                let start = sessions.iter().position(|s| s.id == anchor)?;
                let after = sessions[start + 1..]
                    .iter()
                    .take_while(|s| {
                        s.companion_of.as_deref() == Some(anchor)
                            || s.review_of.as_deref() == Some(anchor)
                    })
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

    /// Pair `author` with the reviewer agent `reviewer` (both records
    /// say so: the author's `reviewer`, the reviewer's `review_of`). The
    /// reviewer it replaces, if any, no longer says it reviews `author`:
    /// its id is returned.
    pub fn set_review_pair(
        &mut self,
        author: &str,
        reviewer: &str,
    ) -> Result<Option<String>, StateError> {
        if author == reviewer {
            return Err(StateError::Invalid(
                "a session cannot review its own work".into(),
            ));
        }
        let position = |id: &str| {
            self.doc
                .sessions
                .iter()
                .position(|s| s.id == id)
                .ok_or_else(|| StateError::NotFound(format!("session {id}")))
        };
        let (a, r) = (position(author)?, position(reviewer)?);
        let before = (
            self.doc.sessions[a].reviewer.clone(),
            self.doc.sessions[r].review_of.clone(),
        );
        // The replaced reviewer, when it still says it reviews `author`.
        let replaced = before
            .0
            .as_deref()
            .filter(|old| *old != reviewer)
            .and_then(|old| self.doc.sessions.iter().position(|s| s.id == old))
            .filter(|&o| self.doc.sessions[o].review_of.as_deref() == Some(author));
        self.doc.sessions[a].reviewer = Some(reviewer.to_owned());
        self.doc.sessions[r].review_of = Some(author.to_owned());
        if let Some(o) = replaced {
            self.doc.sessions[o].review_of = None;
        }
        let author_id = author.to_owned();
        self.save_or_rollback(|doc| {
            doc.sessions[a].reviewer = before.0;
            doc.sessions[r].review_of = before.1;
            if let Some(o) = replaced {
                doc.sessions[o].review_of = Some(author_id);
            }
        })?;
        Ok(replaced.map(|o| self.doc.sessions[o].id.clone()))
    }

    /// Forget a session record, closing the panes that showed it (and
    /// unpairing the sessions it reviewed or was reviewed by). Returns
    /// the workspace whose layout that changed, with its new revision.
    pub fn remove_session(&mut self, id: &str) -> Result<Option<(String, u64)>, StateError> {
        let index = self
            .doc
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| StateError::NotFound(format!("session {id}")))?;
        let previous_pairs: Vec<(Option<String>, Option<String>)> = self
            .doc
            .sessions
            .iter()
            .map(|s| (s.reviewer.clone(), s.review_of.clone()))
            .collect();
        for session in &mut self.doc.sessions {
            if session.reviewer.as_deref() == Some(id) {
                session.reviewer = None;
            }
            if session.review_of.as_deref() == Some(id) {
                session.review_of = None;
            }
        }
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
            for (session, (reviewer, review_of)) in doc.sessions.iter_mut().zip(previous_pairs) {
                session.reviewer = reviewer;
                session.review_of = review_of;
            }
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

    // --- fan-outs ------------------------------------------------------------

    /// Every fan-out, oldest first.
    pub fn fanouts(&self) -> &[Fanout] {
        &self.doc.fanouts
    }

    pub fn fanout(&self, id: &str) -> Result<&Fanout, StateError> {
        self.doc
            .fanouts
            .iter()
            .find(|f| f.id == id)
            .ok_or_else(|| StateError::NotFound(format!("fan-out {id}")))
    }

    /// Record a new fan-out (its workspace must exist).
    pub fn insert_fanout(&mut self, fanout: Fanout) -> Result<(), StateError> {
        self.workspace(&fanout.workspace_id)?;
        if self.doc.fanouts.iter().any(|f| f.id == fanout.id) {
            return Err(StateError::Conflict(format!(
                "fan-out {} exists already",
                fanout.id
            )));
        }
        if self.doc.fanouts.len() >= MAX_FANOUTS {
            return Err(StateError::Conflict(format!(
                "the hub keeps at most {MAX_FANOUTS} fan-outs; remove finished ones first \
                 (fw hub fanout rm)"
            )));
        }
        self.doc.fanouts.push(fanout);
        self.save_or_rollback(|doc| {
            doc.fanouts.pop();
        })
    }

    /// Change fan-out `id` with `edit` and save (put back as it was when
    /// the save fails).
    pub fn update_fanout<R>(
        &mut self,
        id: &str,
        edit: impl FnOnce(&mut Fanout) -> R,
    ) -> Result<R, StateError> {
        let index = self
            .doc
            .fanouts
            .iter()
            .position(|f| f.id == id)
            .ok_or_else(|| StateError::NotFound(format!("fan-out {id}")))?;
        let previous = self.doc.fanouts[index].clone();
        let result = edit(&mut self.doc.fanouts[index]);
        if self.doc.fanouts[index] == previous {
            return Ok(result);
        }
        self.save_or_rollback(|doc| doc.fanouts[index] = previous)?;
        Ok(result)
    }

    /// Forget fan-out `id` (its sessions and worktrees are left alone).
    pub fn remove_fanout(&mut self, id: &str) -> Result<Fanout, StateError> {
        let index = self
            .doc
            .fanouts
            .iter()
            .position(|f| f.id == id)
            .ok_or_else(|| StateError::NotFound(format!("fan-out {id}")))?;
        let removed = self.doc.fanouts.remove(index);
        let kept = removed.clone();
        self.save_or_rollback(|doc| doc.fanouts.insert(index, kept))?;
        Ok(removed)
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
