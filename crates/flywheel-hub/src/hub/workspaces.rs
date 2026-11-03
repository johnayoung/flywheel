//! Workspaces: their folders, launch templates, and layouts.

use super::{Hub, HubError, HubEvent, ImportReport, TemplateView, forget};
use crate::pty::PtySession;
use crate::state::{
    FolderSpec, StateError, Workspace, WorkspaceInput, expand_home, validate_folder,
};
use crate::vscode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl Hub {
    /// Every workspace, each folder's `git`/`missing` flags read from disk
    /// now (a folder may have been `git init`ed, or moved away). Blocking
    /// filesystem I/O: call off the async executor.
    pub fn workspaces(&self) -> Vec<Workspace> {
        let mut workspaces = self.store().workspaces().to_vec();
        for workspace in &mut workspaces {
            workspace.refresh_disk();
        }
        workspaces
    }

    pub fn workspace(&self, id: &str) -> Result<Workspace, HubError> {
        let mut workspace = self.store().workspace(id)?.clone();
        workspace.refresh_disk();
        Ok(workspace)
    }

    pub fn create_workspace(&self, input: WorkspaceInput) -> Result<Workspace, HubError> {
        let workspace = self.store().create_workspace(input)?;
        self.emit(HubEvent::Sessions);
        Ok(workspace)
    }

    pub fn update_workspace(&self, id: &str, input: WorkspaceInput) -> Result<Workspace, HubError> {
        let workspace = self.store().update_workspace(id, input)?;
        self.emit(HubEvent::Sessions);
        Ok(workspace)
    }

    /// Import a VS Code workspace file (`file`: absolute or `~/...`) as
    /// workspace `name` (default: the file's name without
    /// `.code-workspace`). Its existing folders become the workspace's,
    /// labels kept; missing ones and entries naming no local directory are
    /// reported, not imported. An existing same-named workspace is refused
    /// unless `update`, which replaces its folders (refused when that drops
    /// a running session's folder).
    pub fn import_code_workspace(
        &self,
        file: &Path,
        name: Option<&str>,
        update: bool,
    ) -> Result<ImportReport, HubError> {
        let env = vscode::ResolveEnv::current();
        let file = expand_home(file, env.home.as_deref());
        if !file.is_absolute() {
            return Err(StateError::Invalid(format!(
                "workspace file {} must be an absolute path",
                file.display()
            ))
            .into());
        }
        let parsed = vscode::read(&file, &env).map_err(StateError::Invalid)?;
        let mut skipped = parsed.skipped;
        let mut specs = Vec::new();
        // Canonical path -> the entry that brought it in.
        let mut seen: HashMap<PathBuf, String> = HashMap::new();
        for folder in parsed.folders {
            match validate_folder(&folder.path) {
                Ok(path) if seen.contains_key(&path) => skipped.push(vscode::Skipped {
                    index: folder.index,
                    reason: format!("the same folder as {}", seen[&path]),
                    entry: folder.entry,
                    path: Some(path),
                }),
                Ok(path) => {
                    seen.insert(path.clone(), folder.entry);
                    specs.push(FolderSpec::Labeled {
                        path,
                        // Explicit, so an update drops a label the file no
                        // longer has.
                        name: Some(folder.name.unwrap_or_default()),
                    });
                }
                Err(_) if !folder.path.exists() => skipped.push(vscode::Skipped {
                    index: folder.index,
                    entry: folder.entry,
                    path: Some(folder.path),
                    reason: "does not exist".into(),
                }),
                Err(e) => skipped.push(vscode::Skipped {
                    index: folder.index,
                    entry: folder.entry,
                    path: Some(folder.path),
                    reason: e.to_string(),
                }),
            }
        }
        skipped.sort_by_key(|s| s.index);
        if specs.is_empty() {
            let why = skipped
                .iter()
                .map(|s| format!("{}: {}", s.entry, s.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(StateError::Invalid(format!(
                "{} has no folder to import{}",
                file.display(),
                if why.is_empty() {
                    String::new()
                } else {
                    format!(" ({why})")
                }
            ))
            .into());
        }
        let name = name.map(str::to_owned).unwrap_or(parsed.name);
        let (workspace, created) = self.store().import_workspace(&name, specs, update)?;
        self.emit(HubEvent::Sessions);
        Ok(ImportReport {
            workspace,
            created,
            file,
            skipped,
        })
    }

    /// Delete a workspace (refused while any of its sessions runs) and
    /// forget its sessions, freeing the PTYs of the exited ones.
    pub fn delete_workspace(&self, id: &str) -> Result<(), HubError> {
        let mut store = self.store();
        let removed = store.delete_workspace(id)?;
        let freed: Vec<Arc<PtySession>> = {
            let mut live = self.live();
            removed.iter().filter_map(|sid| live.remove(sid)).collect()
        };
        drop(store);
        {
            let mut board = self.board();
            for sid in &removed {
                board.untrack(sid);
            }
        }
        for session in freed {
            forget(&session);
        }
        self.commands.forget(&removed);
        self.emit(HubEvent::Sessions);
        Ok(())
    }

    /// Launch templates: one workspace's, or every workspace's (in
    /// workspace order), each with its workspace id.
    pub fn templates(&self, workspace_id: Option<&str>) -> Result<Vec<TemplateView>, HubError> {
        let store = self.store();
        let ids: Vec<String> = match workspace_id {
            Some(id) => vec![store.workspace(id)?.id.clone()],
            None => store.workspaces().iter().map(|w| w.id.clone()).collect(),
        };
        let mut views = Vec::new();
        for id in ids {
            for template in store.templates(&id)? {
                views.push(TemplateView::new(id.clone(), template));
            }
        }
        Ok(views)
    }

    /// Save a workspace's launch template (replacing one of the same name
    /// only when `replace`).
    pub fn save_template(
        &self,
        workspace_id: &str,
        template: crate::state::LaunchTemplate,
        replace: bool,
    ) -> Result<TemplateView, HubError> {
        let template = self
            .store()
            .save_template(workspace_id, template, replace)?;
        self.emit(HubEvent::Sessions);
        Ok(TemplateView::new(workspace_id.to_owned(), template))
    }

    /// Remove a workspace's launch template.
    pub fn remove_template(&self, workspace_id: &str, name: &str) -> Result<(), HubError> {
        self.store().remove_template(workspace_id, name)?;
        self.emit(HubEvent::Sessions);
        Ok(())
    }

    /// A workspace's tabs and splits (`rev` 0, no tabs: never saved).
    pub fn layout(&self, workspace_id: &str) -> Result<crate::layout::Layout, HubError> {
        Ok(self.store().layout(workspace_id)?)
    }

    /// Save a workspace's layout written against revision `rev` (see
    /// [`HubStore::set_layout`]); every events client hears of it.
    pub fn set_layout(
        &self,
        workspace_id: &str,
        rev: u64,
        tabs: Vec<crate::layout::Tab>,
    ) -> Result<crate::layout::Layout, HubError> {
        let layout = self.store().set_layout(workspace_id, rev, tabs)?;
        self.emit(HubEvent::Layout {
            workspace_id: workspace_id.to_owned(),
            rev: layout.rev,
        });
        Ok(layout)
    }
}
