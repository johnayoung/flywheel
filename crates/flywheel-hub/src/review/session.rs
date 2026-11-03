//! Review for hub sessions: which work tree and base a session's review
//! reads, its actions run against the session (logged to the hub log),
//! and asking the session's agent to write a commit message or resolve a
//! rebase's conflicts.

use super::{
    ActionError, BaseSource, DiffPage, Outcome, Plan, PrRequest, Review, Scope, Target, Tools,
};
use crate::hub::{Hub, HubError, WorktreeInfo};
use crate::state::{SessionRecord, StateError};
use crate::transcript::PasteOutcome;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

/// A session's review: the work tree's [`Review`] plus who it is for.
#[derive(Debug, Clone, Serialize)]
pub struct SessionReview {
    pub session_id: String,
    pub session: String,
    /// The agent session "ask the agent" pastes into (`None`: none here).
    pub agent: Option<AgentRef>,
    /// Merging into the base applies (a worktree session).
    pub mergeable: bool,
    #[serde(flatten)]
    pub review: Review,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentRef {
    pub id: String,
    pub name: String,
}

/// An action and what it needs.
#[derive(Debug, Clone)]
pub enum Action {
    Commit {
        message: String,
    },
    Push {
        remote: Option<String>,
    },
    Pr(PrRequest),
    Rebase,
    /// Abort a stopped rebase.
    Abort,
    Merge,
}

impl Action {
    fn name(&self) -> &'static str {
        match self {
            Self::Commit { .. } => "commit",
            Self::Push { .. } => "push",
            Self::Pr(_) => "pr",
            Self::Rebase => "rebase",
            Self::Abort => "abort",
            Self::Merge => "merge",
        }
    }
}

/// What an action did, and -- after a merge -- whether the worktree can
/// now be removed without losing work.
#[derive(Debug, Clone, Serialize)]
pub struct ActionReport {
    #[serde(flatten)]
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeInfo>,
}

/// What to ask the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// Write a commit message for the uncommitted changes.
    CommitMessage,
    /// Resolve a stopped rebase's conflicts and continue it.
    ResolveConflicts,
}

/// The prompt "ask the agent" pastes, and where.
#[derive(Debug, Clone, Serialize)]
pub struct AskPlan {
    pub agent: AgentRef,
    pub prompt: String,
    /// Set once pasted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste: Option<PasteOutcome>,
}

fn action_error(e: ActionError) -> HubError {
    match e {
        ActionError::Invalid(m) => StateError::Invalid(m).into(),
        ActionError::Conflict(m) => StateError::Conflict(m).into(),
    }
}

/// A base the caller named: a ref-ish word (git checks it resolves).
fn check_base(raw: &str) -> Result<String, HubError> {
    let base = raw.trim();
    if base.is_empty()
        || base.starts_with('-')
        || base.contains(char::is_whitespace)
        || base.contains(['\0', '\\'])
        || base.len() > 255
    {
        return Err(StateError::Invalid(format!("base {raw:?} is not a branch or commit")).into());
    }
    Ok(base.to_owned())
}

impl Hub {
    /// The work tree, base, and worktree a session's review reads: its
    /// worktree (a companion shell shares its agent's), else its folder.
    fn review_target(
        &self,
        tools: &Tools,
        id: &str,
        base: Option<&str>,
    ) -> Result<(SessionRecord, Target), HubError> {
        let record = self.session(id)?.record;
        let worktree = record.worktree.clone().or_else(|| {
            let agent = record.companion_of.as_deref()?;
            let agent = self.session(agent).ok()?.record;
            agent.worktree.filter(|w| w.path == record.cwd)
        });
        let dir = worktree.as_ref().map_or(&record.cwd, |w| &w.path);
        if !dir.is_dir() {
            return Err(StateError::Conflict(format!(
                "{}'s directory {} is gone",
                record.label(),
                dir.display()
            ))
            .into());
        }
        let top = super::toplevel(tools, dir).ok_or_else(|| {
            StateError::Invalid(format!(
                "{} does not run in a git repository ({})",
                record.label(),
                dir.display()
            ))
        })?;
        let given = base.map(check_base).transpose()?;
        if let Some(base) = &given
            && !super::resolves(tools, &top, base)
        {
            return Err(StateError::Invalid(format!(
                "base {base:?} names no branch or commit in {}",
                top.display()
            ))
            .into());
        }
        let (base, base_source) = match (given, &worktree) {
            (Some(base), _) => (Some(base), BaseSource::Given),
            (None, Some(w)) => (Some(w.base.clone()), BaseSource::Worktree),
            (None, None) => (None, BaseSource::Upstream),
        };
        Ok((
            record,
            Target {
                dir: top,
                base,
                base_source,
                worktree,
            },
        ))
    }

    /// The agent session asking pastes into: the session itself, its
    /// agent (a companion shell), else the one agent running in the same
    /// directory.
    fn review_agent(&self, record: &SessionRecord, dir: &Path) -> Option<AgentRef> {
        let as_ref = |r: &SessionRecord| AgentRef {
            id: r.id.clone(),
            name: r.name.clone(),
        };
        if record.runs_agent() {
            return Some(as_ref(record));
        }
        if let Some(agent) = record.companion_of.as_deref()
            && let Ok(view) = self.session(agent)
        {
            return Some(as_ref(&view.record));
        }
        let here: Vec<SessionRecord> = self
            .sessions(Some(&record.workspace_id))
            .into_iter()
            .map(|v| v.record)
            .filter(|r| {
                r.runs_agent() && r.worktree.as_ref().map_or(&r.cwd, |w| &w.path).as_path() == dir
            })
            .collect();
        match here.as_slice() {
            [only] => Some(as_ref(only)),
            _ => None,
        }
    }

    /// A session's review (runs git now).
    pub fn review(
        &self,
        tools: &Tools,
        id: &str,
        base: Option<&str>,
    ) -> Result<SessionReview, HubError> {
        let (record, target) = self.review_target(tools, id, base)?;
        let review = super::read(tools, &target).map_err(StateError::Conflict)?;
        Ok(SessionReview {
            session_id: record.id.clone(),
            session: record.name.clone(),
            agent: self.review_agent(&record, &target.dir),
            mergeable: target.worktree.is_some(),
            review,
        })
    }

    /// A page of one file's diff in a session's review.
    #[allow(clippy::too_many_arguments)]
    pub fn review_diff(
        &self,
        tools: &Tools,
        id: &str,
        base: Option<&str>,
        scope: Scope,
        path: &str,
        old_path: Option<&str>,
        offset: usize,
    ) -> Result<DiffPage, HubError> {
        let (_, target) = self.review_target(tools, id, base)?;
        super::diff(tools, &target, scope, path, old_path, offset)
            .map_err(|e| StateError::Invalid(e).into())
    }

    /// What `action` would do (nothing runs).
    pub fn review_plan(&self, tools: &Tools, id: &str, action: &Action) -> Result<Plan, HubError> {
        let (record, target) = self.review_target(tools, id, None)?;
        let plan = match action {
            Action::Commit { message } => {
                super::check_message(message).map_err(action_error)?;
                super::plan_commit(tools, &target, message)
            }
            Action::Push { remote } => super::plan_push(tools, &target, remote.as_deref()),
            Action::Pr(request) => super::plan_pr(tools, &target, request),
            Action::Rebase => super::plan_rebase(tools, &target),
            Action::Abort => super::plan_abort(tools, &target),
            Action::Merge => super::plan_merge(tools, &target),
        };
        let plan = plan.map_err(|e| HubError::from(StateError::Conflict(e)))?;
        self.log_refusal(&record, action.name(), plan.refusal.as_deref());
        Ok(plan)
    }

    /// Log a plan's refusal to the hub log, once per session and action
    /// until it changes (a dialog re-plans as you type).
    fn log_refusal(&self, record: &SessionRecord, action: &str, refusal: Option<&str>) {
        static LAST: Mutex<Option<HashMap<(String, String), String>>> = Mutex::new(None);
        let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
        let last = last.get_or_insert_with(HashMap::new);
        let key = (record.id.clone(), action.to_owned());
        match refusal {
            None => {
                last.remove(&key);
            }
            Some(why) if last.get(&key).map(String::as_str) != Some(why) => {
                eprintln!(
                    "fw hub: review {}: {action}: refused: {}",
                    record.label(),
                    one_line(why)
                );
                last.insert(key, why.to_owned());
            }
            Some(_) => {}
        }
    }

    /// Run `action` as confirmed (`confirm`: the plan's fingerprint).
    /// Logged to the hub log either way.
    pub fn review_run(
        &self,
        tools: &Tools,
        id: &str,
        action: &Action,
        confirm: &str,
    ) -> Result<ActionReport, HubError> {
        let (record, target) = self.review_target(tools, id, None)?;
        let result = match action {
            Action::Commit { message } => super::commit(tools, &target, message, confirm),
            Action::Push { remote } => super::push(tools, &target, remote.as_deref(), confirm),
            Action::Pr(request) => super::pr(tools, &target, request, confirm),
            Action::Rebase => super::rebase(tools, &target, confirm),
            Action::Abort => super::abort(tools, &target, confirm),
            Action::Merge => super::merge(tools, &target, confirm),
        };
        match &result {
            Ok(outcome) => {
                let commands = outcome
                    .ran
                    .iter()
                    .map(|r| r.display.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                eprintln!(
                    "fw hub: review {}: {}: {commands} -> {}",
                    record.label(),
                    action.name(),
                    if outcome.ok {
                        "ok".to_owned()
                    } else {
                        format!("stopped: {}", one_line(&outcome.summary))
                    }
                );
            }
            Err(e) => eprintln!(
                "fw hub: review {}: {}: not run or failed: {}",
                record.label(),
                action.name(),
                one_line(&e.to_string())
            ),
        }
        let outcome = result.map_err(action_error)?;
        self.nudge_review_badges(&target);
        let worktree = match action {
            Action::Merge if outcome.ok => self.worktree_info(id).ok(),
            _ => None,
        };
        Ok(ActionReport { outcome, worktree })
    }

    /// Re-read the badges of every session in the work tree acted on, and
    /// (a merge) in the main checkout.
    fn nudge_review_badges(&self, target: &Target) {
        let main = target.worktree.as_ref().map(|w| w.repo.clone());
        for view in self.sessions(None) {
            let cwd = &view.record.cwd;
            if cwd.starts_with(&target.dir) || main.as_ref().is_some_and(|m| cwd.starts_with(m)) {
                self.nudge_badge_soon(&view.record);
            }
        }
    }

    /// What asking the agent would paste, and into which session; with
    /// `run`, paste it (then Enter) and log it.
    pub fn review_ask(
        &self,
        tools: &Tools,
        id: &str,
        kind: AskKind,
        run: bool,
    ) -> Result<AskPlan, HubError> {
        let (record, target) = self.review_target(tools, id, None)?;
        let agent = self.review_agent(&record, &target.dir).ok_or_else(|| {
            StateError::Conflict(format!(
                "no agent session runs in {} to ask",
                target.dir.display()
            ))
        })?;
        let review = super::read(tools, &target).map_err(StateError::Conflict)?;
        let prompt = ask_prompt(kind, &review)?;
        let mut plan = AskPlan {
            agent,
            prompt,
            paste: None,
        };
        if run {
            let pasted = self.paste(&plan.agent.id, &plan.prompt, true);
            eprintln!(
                "fw hub: review {}: ask {} ({kind:?}) -> {}",
                record.label(),
                plan.agent.name,
                match &pasted {
                    Ok(_) => "pasted".to_owned(),
                    Err(e) => format!("failed: {e}"),
                }
            );
            plan.paste = Some(pasted?);
        }
        Ok(plan)
    }
}

/// A message on one log line.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// The prompt pasted into the agent.
fn ask_prompt(kind: AskKind, review: &Review) -> Result<String, HubError> {
    match kind {
        AskKind::CommitMessage => {
            if review.clean() {
                return Err(StateError::Conflict(
                    "nothing to commit: the work tree is clean".into(),
                )
                .into());
            }
            Ok(
                "Write a commit message for all the uncommitted changes in this work tree \
                 (staged, unstaged, and untracked: see `git status` and `git diff HEAD`), \
                 following this repository's convention (see `git log --oneline -10`). \
                 Reply with only the message, in one ```text fenced code block. \
                 Do not run git commit or change any file."
                    .to_owned(),
            )
        }
        AskKind::ResolveConflicts => {
            if review.operation.as_deref() != Some("rebase") {
                return Err(StateError::Conflict("no rebase is stopped here".into()).into());
            }
            let base = review.base.as_ref().map_or("its base", |b| b.name.as_str());
            let files = if review.conflicts.is_empty() {
                "the files git reports".to_owned()
            } else {
                review.conflicts.join(", ")
            };
            Ok(format!(
                "A rebase of this branch onto {base} stopped on conflicts in: {files}. \
                 Resolve each conflict keeping the intent of both sides, `git add` the \
                 resolved files, and run `git rebase --continue`; repeat until the rebase \
                 finishes. Do not abort the rebase and do not push."
            ))
        }
    }
}
