//! Launching a fan-out: one base commit, a worktree and an agent per
//! candidate, all or nothing.

use super::{Fanouts, conflict, invalid};
use crate::fanout::facts::{self};
use crate::fanout::{Candidate, CreateFanout, Fanout, FanoutView, task_name};
use crate::hub::{CreateSession, HubError, HubEvent, NewWorktree, RemoveOptions, SessionView};
use crate::state::new_id;
use chrono::Utc;

impl Fanouts {
    /// Launch a fan-out: every candidate's worktree cut from one commit of
    /// the base branch, its agent started on the prompt. All or nothing:
    /// a failure removes what was started.
    pub fn create(&self, request: CreateFanout) -> Result<FanoutView, HubError> {
        let hub = self.hub()?;
        let valid = request.validated().map_err(invalid)?;
        if self.runner.is_none() {
            return Err(conflict(
                "this hub cannot grade (no grader runner): start it with `fw hub start`",
            ));
        }
        if hub.store().fanouts().len() >= super::MAX_FANOUTS {
            return Err(conflict(format!(
                "the hub keeps at most {} fan-outs; remove finished ones first (fw hub fanout rm)",
                super::MAX_FANOUTS
            )));
        }
        let workspace = hub.workspace(&request.workspace_id)?;
        let folder = match &request.folder {
            Some(path) => {
                let canonical = path
                    .canonicalize()
                    .map_err(|e| invalid(format!("folder {}: {e}", path.display())))?;
                if !workspace.has_folder(&canonical) {
                    return Err(invalid(format!(
                        "folder {} is not part of workspace {}",
                        canonical.display(),
                        workspace.name
                    )));
                }
                canonical
            }
            None => workspace
                .folders
                .iter()
                .find(|f| f.git && !f.missing)
                .map(|f| f.path.clone())
                .ok_or_else(|| {
                    invalid(format!(
                        "workspace {} has no git folder to fan out in",
                        workspace.name
                    ))
                })?,
        };
        if !crate::state::is_git_root(&folder) {
            return Err(invalid(crate::git::not_a_repo_root(&folder)));
        }
        let base = match request
            .base
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            Some(base) => base.to_owned(),
            None => facts::current_branch(&self.tools, &folder).ok_or_else(|| {
                invalid(format!(
                    "{}'s HEAD is detached: name the base branch to fan out from",
                    folder.display()
                ))
            })?,
        };
        if base.starts_with('-') || !facts::is_local_branch(&self.tools, &folder, &base) {
            return Err(invalid(format!(
                "base {base:?} is not a local branch of {} (the winner is merged into it)",
                folder.display()
            )));
        }
        let base_sha = facts::resolve_commit(&self.tools, &folder, &base)
            .ok_or_else(|| invalid(format!("base {base:?} names no commit")))?;
        let kinds = hub.kinds();
        for (kind, _) in &valid.candidates {
            if !kinds.iter().any(|k| k.kind == *kind && k.available) {
                return Err(invalid(format!(
                    "{} is not installed on this hub's PATH",
                    kind.as_str()
                )));
            }
        }
        let id = new_id();
        let tag = &id[..4];
        let named = valid
            .name
            .clone()
            .unwrap_or_else(|| super::default_name(&workspace.name, &valid.prompt));
        let sent = super::sent_prompt(&valid.prompt, request.commit_instruction);
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut started: Vec<SessionView> = Vec::new();
        // A launch that fails part-way takes back everything it started:
        // the sessions, their worktrees, and the branches cut for them
        // (whatever an agent committed there in the meantime is not any
        // fan-out's work).
        let undo = |started: &[SessionView]| {
            for view in started {
                let _ = hub.remove_session_with(
                    &view.record.id,
                    RemoveOptions {
                        remove_worktree: true,
                        force: true,
                    },
                );
                if let Some(worktree) = &view.record.worktree
                    && !worktree.path.exists()
                {
                    crate::git::delete_branch(&worktree.repo, &worktree.branch);
                }
            }
        };
        let (pins, protect) =
            super::pin_graders(&valid.graders, &folder, valid.protect.clone()).map_err(invalid)?;
        for (i, (kind, launch)) in valid.candidates.iter().enumerate() {
            let label = format!("c{}", i + 1);
            let task = task_name(&named, tag, &label);
            // The label first: a sidebar cuts long names at the end.
            let session_name = format!(
                "{label}-{}",
                task.strip_suffix(&format!("-{label}")).unwrap_or(&task)
            );
            let created = hub.create_session(CreateSession {
                workspace_id: workspace.id.clone(),
                repo: Some(folder.clone()),
                name: Some(session_name),
                kind: *kind,
                command: None,
                cols: request.cols,
                rows: request.rows,
                worktree: Some(NewWorktree {
                    task,
                    base: Some(base.clone()),
                    // Cut at the commit read above, whatever the base does
                    // meanwhile: every candidate starts from it.
                    at: Some(base_sha.clone()),
                }),
                prompt: Some(sent.clone()),
                launch: launch.clone(),
            });
            let view = match created {
                Ok(view) => view,
                Err(e) => {
                    undo(&started);
                    return Err(e);
                }
            };
            started.push(view.clone());
            let Some(worktree) = view.record.worktree.clone() else {
                undo(&started);
                return Err(HubError::Internal(
                    "a fan-out session has no worktree".into(),
                ));
            };
            candidates.push(Candidate {
                label,
                session_id: view.record.id.clone(),
                kind: *kind,
                // As launched: hub.toml's [agents.*] defaults fill what the
                // request left unset.
                launch: view.record.launch.clone().unwrap_or_default(),
                worktree,
                pending: None,
                grade: None,
                gradings: 0,
                fixes: 0,
                cleaned: None,
            });
        }
        // Names resolve fan-outs (`fw hub fanout show NAME`): a second
        // fan-out of the same task or prompt gets `<name>-2`, and so on.
        let name = {
            let store = hub.store();
            let taken: Vec<&str> = store.fanouts().iter().map(|f| f.name.as_str()).collect();
            super::unique_name(&named, &taken)
        };
        let fanout = Fanout {
            id: id.clone(),
            name,
            workspace_id: workspace.id.clone(),
            repo: folder,
            base,
            base_sha,
            prompt: valid.prompt,
            commit_instruction: request.commit_instruction,
            graders: valid.graders,
            timeout_secs: valid.timeout_secs,
            protect,
            pins,
            created_at: Utc::now(),
            candidates,
            winner: None,
            origin: request.origin.clone(),
            promoted: Vec::new(),
            seen_graded_at: None,
        };
        if let Err(e) = hub.store().insert_fanout(fanout.clone()) {
            undo(&started);
            return Err(e.into());
        }
        eprintln!(
            "fw hub: fan-out {} ({id}): {} agents on {} at {} ({})",
            fanout.name,
            fanout.candidates.len(),
            fanout.base,
            &fanout.base_sha[..12.min(fanout.base_sha.len())],
            fanout
                .candidates
                .iter()
                .map(|c| format!(
                    "{} {}{}",
                    c.label,
                    c.kind.as_str(),
                    c.launch
                        .model
                        .as_deref()
                        .map(|m| format!(":{m}"))
                        .unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        hub.emit(HubEvent::Fanout { id: id.clone() });
        // A fast agent may have finished before the record existed.
        for candidate in &fanout.candidates {
            self.on_turn(&candidate.session_id);
        }
        self.on_sessions();
        self.view(&id)
    }
}
