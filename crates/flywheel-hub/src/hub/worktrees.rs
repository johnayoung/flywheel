//! Sessions in sibling git worktrees: what a worktree holds, the plan
//! for removing one, and removing a session with its worktree.

use super::{
    Hub, HubError, KILL_GRACE, RemoveOptions, RemoveOutcome, WorktreeInfo, WorktreePlanView,
};
use crate::git;
use crate::state::{SessionRecord, StateError};
use std::path::Path;
use std::time::Duration;

impl Hub {
    /// A worktree session's worktree and what removing it would lose
    /// (runs git now).
    pub fn worktree_info(&self, id: &str) -> Result<WorktreeInfo, HubError> {
        let (record, companions, blocking) = self.worktree_neighbours(id)?;
        let worktree = record.worktree.clone().ok_or_else(|| {
            StateError::Invalid(format!(
                "session {:?} does not run in a worktree",
                record.name
            ))
        })?;
        let check = git::removal_check(&worktree).map_err(HubError::Internal)?;
        Ok(WorktreeInfo {
            refusal: check.refusal(),
            worktree,
            check,
            companions: companions.into_iter().map(|s| s.name).collect(),
            blocking: blocking.into_iter().map(|s| s.name).collect(),
        })
    }

    /// `id`'s record, its companions, and the other running sessions in
    /// its worktree.
    #[allow(clippy::type_complexity)]
    fn worktree_neighbours(
        &self,
        id: &str,
    ) -> Result<(SessionRecord, Vec<SessionRecord>, Vec<SessionRecord>), HubError> {
        let store = self.store();
        let record = store.session(id)?.clone();
        let companions: Vec<SessionRecord> = store
            .sessions()
            .iter()
            .filter(|s| s.companion_of.as_deref() == Some(id))
            .cloned()
            .collect();
        let blocking = match &record.worktree {
            Some(worktree) => store
                .sessions()
                .iter()
                // Stopped ones too: they would restart into a deleted
                // directory.
                .filter(|s| {
                    s.id != id
                        && s.companion_of.as_deref() != Some(id)
                        && (s.cwd.starts_with(&worktree.path)
                            || s.worktree.as_ref().is_some_and(|w| w.path == worktree.path))
                })
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        Ok((record, companions, blocking))
    }

    /// Remove a session; with `remove_worktree`, also its companion shells
    /// and its worktree (refused, nothing touched, when another session
    /// runs there, or -- unless `force` -- when it holds uncommitted work
    /// or commits its base lacks). The branch goes too when it has nothing
    /// its base lacks.
    pub fn remove_session_with(
        &self,
        id: &str,
        options: RemoveOptions,
    ) -> Result<RemoveOutcome, HubError> {
        let (record, companions, _) = self.worktree_neighbours(id)?;
        if !options.remove_worktree {
            // Companion shells belong to their agent: they go with it
            // (none is left pointing at a session that is gone).
            let mut removed: Vec<String> = vec![id.to_owned()];
            removed.extend(companions.into_iter().map(|s| s.id));
            for sid in &removed {
                match self.remove_session(sid) {
                    Ok(()) => {}
                    Err(HubError::State(StateError::NotFound(_))) if sid != id => {}
                    Err(e) => return Err(e),
                }
            }
            return Ok(RemoveOutcome {
                removed,
                worktree: None,
            });
        }
        if let Some(agent) = &record.companion_of {
            let agent = self
                .store()
                .session(agent)
                .map(|a| a.name.clone())
                .unwrap_or_else(|_| agent.clone());
            return Err(StateError::Invalid(format!(
                "{:?} is {agent:?}'s companion shell; the worktree is {agent:?}'s: close {agent:?} to remove it",
                record.name
            ))
            .into());
        }
        let info = self.worktree_info(id)?;
        if let Some(other) = info.blocking.first() {
            return Err(StateError::Conflict(format!(
                "session {other:?} also uses worktree {}; close it first",
                info.worktree.path.display()
            ))
            .into());
        }
        if !options.force
            && let Some(why) = &info.refusal
        {
            return Err(StateError::Conflict(why.clone()).into());
        }
        // Nothing may run in a directory being deleted: the companions and
        // the session go first, each waited for.
        let (_, companions, _) = self.worktree_neighbours(id)?;
        let mut removed: Vec<String> = companions.into_iter().map(|s| s.id).collect();
        removed.push(id.to_owned());
        for sid in &removed {
            if let Some(session) = self.live_session(sid)
                && session.exit_info().is_none()
            {
                session.kill(KILL_GRACE);
                session.wait_exit(KILL_GRACE + Duration::from_secs(1));
            }
        }
        for sid in &removed {
            match self.remove_session(sid) {
                Ok(()) | Err(HubError::State(StateError::NotFound(_))) => {}
                Err(e) => return Err(e),
            }
        }
        let worktree = git::remove(&info.worktree, options.force).map_err(|e| {
            StateError::Conflict(format!(
                "the session is closed, but its worktree was kept: {e}"
            ))
        })?;
        removed.rotate_right(1);
        Ok(RemoveOutcome {
            removed,
            worktree: Some(worktree),
        })
    }

    /// For the new-session form: where `folder`'s worktrees go, its
    /// branches and default base, and -- with `task` -- the worktree that
    /// would be cut (or why not). Runs git.
    pub fn worktree_plan(
        &self,
        folder: &Path,
        task: Option<&str>,
        base: Option<&str>,
    ) -> WorktreePlanView {
        let mut view = WorktreePlanView {
            dir: None,
            default_base: None,
            branches: Vec::new(),
            plan: None,
            error: None,
        };
        if !crate::state::is_git_root(folder) {
            view.error = Some(git::not_a_repo_root(folder));
            return view;
        }
        match git::worktrees_dir(folder) {
            Ok(dir) => view.dir = Some(dir),
            Err(e) => {
                view.error = Some(e);
                return view;
            }
        }
        view.branches = git::branches(folder);
        view.default_base = git::status(folder, None).ok().and_then(|s| s.branch);
        if let Some(task) = task.filter(|t| !t.trim().is_empty()) {
            match git::plan(folder, task, &self.options.worktree_branch_prefix, base) {
                Ok(plan) => view.plan = Some(plan),
                Err(e) => view.error = Some(e),
            }
        }
        view
    }
}
