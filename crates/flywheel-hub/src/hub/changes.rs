//! Adopting worktrees on the hub (see [`crate::changes`]): the
//! `adopt_worktree` tool, `fw hub change`, `/api/changes`. Each adoption
//! is a `change.adopted` event; a close of one, `close.recorded`.

use super::{Hub, HubError, HubEvent};
use crate::changes::{BaseFrom, Change, ChangeView, MAX_CHANGES};
use crate::close::CloseChoices;
use crate::git::{self, WorktreeEntry, WorktreeRef};
use crate::state::{SessionRecord, StateError, new_id};
use chrono::Utc;
use std::path::{Path, PathBuf};

/// What an adoption asks for.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Adopt {
    /// The worktree's directory.
    pub path: String,
    /// The branch it merges into (default: worked out from git).
    #[serde(default)]
    pub base: Option<String>,
    /// A session that works in it (name or id).
    #[serde(default)]
    pub session: Option<String>,
}

impl Hub {
    /// Every tracked change, with what its worktree holds now (`git`:
    /// look; runs git per change).
    pub fn changes(&self, git_now: bool) -> Vec<ChangeView> {
        let (changes, names): (Vec<Change>, Vec<(String, String)>) = {
            let store = self.store();
            (
                store.changes().to_vec(),
                store
                    .sessions()
                    .iter()
                    .map(|s| (s.id.clone(), s.name.clone()))
                    .collect(),
            )
        };
        changes
            .into_iter()
            .map(|change| ChangeView {
                check: git_now
                    .then(|| git::removal_check(&change.worktree).ok())
                    .flatten(),
                session_name: change
                    .session
                    .as_ref()
                    .and_then(|id| names.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone())),
                change,
            })
            .collect()
    }

    /// A change by id or unique id prefix (with or without its `c`).
    pub fn change_named(&self, reference: &str) -> Result<Change, HubError> {
        let wanted = reference.trim().trim_start_matches('c');
        if wanted.len() < 4 {
            return Err(
                StateError::Invalid("name a change by its id (4+ characters)".into()).into(),
            );
        }
        let found: Vec<Change> = self
            .store()
            .changes()
            .iter()
            .filter(|c| c.id.trim_start_matches('c').starts_with(wanted))
            .cloned()
            .collect();
        match found.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(StateError::NotFound(format!("change {reference}")).into()),
            _ => {
                Err(StateError::Invalid(format!("{reference} matches more than one change")).into())
            }
        }
    }

    /// The workspace folders `caller` may adopt worktrees of (`None`: the
    /// operator: every workspace's).
    fn adoptable_repos(&self, caller: Option<&SessionRecord>) -> Vec<PathBuf> {
        let store = self.store();
        store
            .workspaces()
            .iter()
            .filter(|w| caller.is_none_or(|c| c.workspace_id == w.id))
            .flat_map(|w| w.folders.iter().map(|f| f.path.clone()))
            .filter_map(|p| p.canonicalize().ok())
            .collect()
    }

    /// Adopt the worktree `adopt` names. `caller`: the agent adopting it
    /// (`None`: the operator).
    pub fn adopt_worktree(
        &self,
        caller: Option<&str>,
        adopt: &Adopt,
    ) -> Result<(Change, bool), HubError> {
        let record = match caller {
            Some(id) => Some(self.store().session(id)?.clone()),
            None => None,
        };
        let raw = adopt.path.trim();
        if raw.is_empty() {
            return Err(StateError::Invalid("give the worktree's path".into()).into());
        }
        let given = match &record {
            Some(r) if Path::new(raw).is_relative() => r.cwd.join(raw),
            _ => PathBuf::from(raw),
        };
        let path = given
            .canonicalize()
            .map_err(|e| StateError::Invalid(format!("{}: {e}", given.display())))?;
        let (repo, branch) = git::linked_worktree_of(&path).map_err(StateError::Invalid)?;
        let repo = repo.canonicalize().unwrap_or(repo);
        if !self.adoptable_repos(record.as_ref()).contains(&repo) {
            return Err(StateError::Invalid(format!(
                "{} is a worktree of {}, which is not one of {} folders",
                path.display(),
                repo.display(),
                if record.is_some() {
                    "your workspace's"
                } else {
                    "the workspaces'"
                }
            ))
            .into());
        }
        // Tracked already: by a change, or as a session's own.
        if let Some(found) = self
            .store()
            .changes()
            .iter()
            .find(|c| c.worktree.path == path)
        {
            return Ok((found.clone(), false));
        }
        if let Some(owner) = self
            .store()
            .sessions()
            .iter()
            .find(|s| s.worktree.as_ref().is_some_and(|w| w.path == path))
        {
            return Err(StateError::Conflict(format!(
                "{} is session {}'s worktree already: the hub tracks it",
                path.display(),
                owner.name
            ))
            .into());
        }
        let (base, base_from) = match adopt
            .base
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            Some(base) => {
                git::commit_id(&repo, base).map_err(StateError::Invalid)?;
                (base.to_owned(), BaseFrom::Given)
            }
            None => {
                if let Some(from) = git::created_from(&repo, &branch) {
                    (from, BaseFrom::Reflog)
                } else if let Some(up) = git::upstream_branch(&repo, &branch) {
                    (up, BaseFrom::Upstream)
                } else {
                    let current = git::status(&repo, None)
                        .ok()
                        .and_then(|s| s.branch)
                        .ok_or_else(|| {
                            StateError::Invalid(format!(
                                "no base for {branch} could be worked out: give base"
                            ))
                        })?;
                    (current, BaseFrom::RepoBranch)
                }
            }
        };
        let session = match adopt
            .session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => {
                let target = self.session_named(s)?;
                if let Some(caller) = caller
                    && target.id != caller
                    && self.session_owner(&target.id)?.as_deref() != Some(caller)
                {
                    return Err(StateError::Invalid(format!(
                        "{} is not yours: link a session you own (or yourself)",
                        target.name
                    ))
                    .into());
                }
                Some(target)
            }
            None => None,
        };
        let worktree = WorktreeRef {
            repo,
            path: path.clone(),
            branch,
            base,
        };
        let change = Change {
            id: format!("c{}", &new_id()[..7]),
            worktree: worktree.clone(),
            base_from,
            session: session.as_ref().map(|s| s.id.clone()),
            by: caller.unwrap_or("operator").to_owned(),
            at: Utc::now(),
        };
        self.store().add_change(change.clone(), MAX_CHANGES)?;
        // A session working inside it takes it as its own worktree.
        if let Some(s) = &session
            && s.worktree.is_none()
            && s.cwd.canonicalize().is_ok_and(|cwd| cwd.starts_with(&path))
        {
            let given = worktree.clone();
            self.store()
                .update_session(&s.id, |r| r.worktree = Some(given))?;
        }
        self.emit(HubEvent::Sessions);
        self.log_hub_event(
            "change.adopted",
            change.session.as_deref().or(caller),
            serde_json::json!({
                "change": change.id,
                "path": change.worktree.path,
                "branch": change.worktree.branch,
                "base": change.worktree.base,
                "base_from": change.base_from,
                "by": change.by,
                "session": change.session,
            }),
        );
        Ok((change, true))
    }

    /// Linked worktrees of the workspaces' git folders that nothing
    /// tracks (no session's worktree, no change).
    pub fn untracked_worktrees(&self) -> Vec<(PathBuf, WorktreeEntry)> {
        let tracked: Vec<PathBuf> = {
            let store = self.store();
            store
                .sessions()
                .iter()
                .filter_map(|s| s.worktree.as_ref().map(|w| w.path.clone()))
                .chain(store.changes().iter().map(|c| c.worktree.path.clone()))
                .collect()
        };
        let mut found = Vec::new();
        for repo in self.adoptable_repos(None) {
            for entry in git::linked_worktrees(&repo).unwrap_or_default() {
                let path = entry
                    .path
                    .canonicalize()
                    .unwrap_or_else(|_| entry.path.clone());
                if !tracked.contains(&path)
                    && !found
                        .iter()
                        .any(|(_, e): &(PathBuf, WorktreeEntry)| e.path == entry.path)
                {
                    found.push((repo.clone(), entry));
                }
            }
        }
        found
    }

    /// Close change `id`: its checklist (`dry_run`), or `choices` applied
    /// (the rules of [`crate::close`]); a change whose worktree is gone is
    /// no longer tracked. Refused while its session runs.
    pub fn close_change(
        &self,
        id: &str,
        choices: &CloseChoices,
        dry_run: bool,
    ) -> Result<serde_json::Value, HubError> {
        let change = self.change_named(id)?;
        let others: Vec<String> = {
            let store = self.store();
            store
                .sessions()
                .iter()
                .filter(|s| s.state == crate::state::SessionState::Running)
                .filter(|s| {
                    s.cwd.starts_with(&change.worktree.path)
                        || change.session.as_deref() == Some(s.id.as_str())
                })
                .map(|s| s.name.clone())
                .collect()
        };
        if dry_run {
            let list =
                crate::close::checklist(&change.worktree.path, Some(&change.worktree), others);
            return Ok(serde_json::json!({ "checklist": list, "text": list.text() }));
        }
        let done = crate::close::apply(
            choices,
            &change.worktree.path,
            Some(&change.worktree),
            others,
            self.state_dir(),
            &change.id,
            None,
            crate::telemetry::now_ms(),
        );
        if !change.worktree.path.exists() {
            self.store().remove_change(&change.id)?;
        }
        self.emit(HubEvent::Sessions);
        self.log_hub_event(
            "close.recorded",
            change.session.as_deref(),
            serde_json::json!({
                "change": change.id,
                "worktree": done.worktree,
                "build": done.build,
                "report": done.report.as_ref().map(|r| &r.stored),
            }),
        );
        Ok(serde_json::json!({ "close": done, "text": super::close_words(&done) }))
    }

    /// The `adopt_worktree` agent tool.
    pub fn agent_adopt(&self, caller: &str, adopt: &Adopt) -> Result<String, HubError> {
        let (change, new) = self.adopt_worktree(Some(caller), adopt)?;
        let check = git::removal_check(&change.worktree).ok();
        Ok(format!(
            "{} {} (change {}): branch {}, base {} ({}){}{}. The hub refuses to remove it while \
             it holds work; fw hub change close {} closes it.",
            if new { "Tracking" } else { "Already tracked:" },
            change.worktree.path.display(),
            change.id,
            change.worktree.branch,
            change.worktree.base,
            change.base_from.words(),
            change
                .session
                .as_deref()
                .map(|s| format!(", linked to session {s}"))
                .unwrap_or_default(),
            check
                .map(|c| format!(
                    "; now {} uncommitted, {} untracked, {} unmerged",
                    c.dirty, c.untracked, c.unmerged
                ))
                .unwrap_or_default(),
            change.id
        ))
    }
}
