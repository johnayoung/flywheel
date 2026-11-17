//! Agents starting agents: the `start_session` tool ([`Hub::agent_start`])
//! and `fw hub new` run inside an agent session ([`Hub::open_session`]).
//!
//! Whether a session's agent may start sessions is its own setting
//! (`start_sessions`), else the hub's `agent_starts.policy`: `allowed`
//! starts at once, `ask` keeps the start on the asking session's record
//! until the operator starts or declines it (never cleared otherwise),
//! `off` refuses. Every start is held to the `[agent_starts]` limits,
//! checked again when a waiting one is started. A started session is
//! linked to its starter (`opened_by`), and its first prompt says who
//! started it and where its results go.
//!
//! These rules bind the tool and the CLI, not an agent with a shell: it
//! can read the hub's token and call the API itself.

use super::{CreateSession, Hub, HubError, HubEvent, NewWorktree, SessionRef, SessionView};
use crate::agent::{self, AgentLaunch};
use crate::config::AgentStarts;
use crate::git::WorktreeRef;
use crate::state::{
    PendingStart, SessionRecord, SessionState, StartPolicy, StateError, WorktreeRequest, new_id,
    validate_name,
};
use crate::telemetry::ItemStatus;
use chrono::Utc;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};

/// What an agent asks the `start_session` tool for.
#[derive(Debug, Clone, Default)]
pub struct AgentStart {
    pub name: String,
    /// The new session's whole first prompt.
    pub prompt: String,
    /// One of the workspace's folders; default: the agent's own directory.
    pub folder: Option<PathBuf>,
    /// Run it in a new sibling worktree.
    pub worktree: Option<WorktreeRequest>,
    /// Default: the agent's own.
    pub model: Option<String>,
}

/// What became of a request to open a session.
#[derive(Debug)]
pub enum Opened {
    Started(Box<SessionView>),
    /// Kept for the operator on the record of `from` (the asking session).
    Waiting {
        from: SessionRef,
        request: Box<PendingStart>,
    },
}

pub(super) fn refused(why: impl Into<String>) -> HubError {
    StateError::Invalid(why.into()).into()
}

pub(super) fn running_agent(record: &SessionRecord) -> bool {
    record.runs_agent() && record.state == SessionState::Running
}

/// `record`'s place in its opened-by chain: 0 for a session nobody
/// started, 1 for one its starter's operator opened, and so on.
fn depth_of(record: &SessionRecord, sessions: &[SessionRecord]) -> usize {
    let mut depth = 0;
    let mut seen = HashSet::from([record.id.as_str()]);
    let mut at = record.opened_by.as_deref();
    while let Some(id) = at {
        if !seen.insert(id) {
            break;
        }
        depth += 1;
        at = sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.opened_by.as_deref());
    }
    depth
}

/// Why `caller` may not start one more session now, naming the limit.
fn over_limit(
    caller: &SessionRecord,
    sessions: &[SessionRecord],
    limits: &AgentStarts,
) -> Option<String> {
    let mine = sessions
        .iter()
        .filter(|s| running_agent(s) && s.opened_by.as_deref() == Some(caller.id.as_str()))
        .count();
    if mine >= limits.max_running {
        return Some(format!(
            "limit agent_starts.max_running ({}) reached: {} already runs {mine} sessions it \
             started; one must end first",
            limits.max_running, caller.name
        ));
    }
    if caller.starts_made as usize >= limits.max_total {
        return Some(format!(
            "limit agent_starts.max_total ({}) reached: {} has started {} sessions",
            limits.max_total, caller.name, caller.starts_made
        ));
    }
    let depth = depth_of(caller, sessions) + 1;
    if depth > limits.max_depth {
        return Some(format!(
            "limit agent_starts.max_depth ({}) reached: the new session would be {depth} \
             starts deep",
            limits.max_depth
        ));
    }
    let hub = sessions
        .iter()
        .filter(|s| running_agent(s) && s.opened_by.is_some())
        .count();
    if hub >= limits.max_hub {
        return Some(format!(
            "limit agent_starts.max_hub ({}) reached: {hub} agent-started sessions run on this hub",
            limits.max_hub
        ));
    }
    None
}

pub(super) fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn plan_mark(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Done => "[x]",
        ItemStatus::Active => "[>]",
        ItemStatus::Cancelled => "[-]",
        ItemStatus::Pending => "[ ]",
    }
}

/// `ms` milliseconds as `3h 12m`, `12m`, or `40s`.
pub(super) fn span(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

impl Hub {
    /// Whether session `record`'s agent may start sessions: its own
    /// setting, else the hub's.
    pub fn start_policy(&self, record: &SessionRecord) -> StartPolicy {
        record
            .start_sessions
            .unwrap_or_else(|| self.tunables().agent_starts.policy)
    }

    /// Set whether session `id`'s agent may start sessions (`None`: the
    /// hub's default).
    pub fn set_start_policy(
        &self,
        id: &str,
        policy: Option<StartPolicy>,
    ) -> Result<SessionView, HubError> {
        {
            let store = self.store();
            if !store.session(id)?.runs_agent() {
                return Err(refused(
                    "only an agent session starts sessions: a shell has no such setting",
                ));
            }
        }
        let record = self.store().set_start_policy(id, policy)?;
        self.emit(HubEvent::Sessions);
        Ok(self.view(record))
    }

    /// Why agent session `id` may not start one more session now.
    pub(super) fn check_start_limits(&self, id: &str) -> Result<SessionRecord, HubError> {
        let limits = self.tunables().agent_starts;
        let store = self.store();
        let caller = store.session(id)?.clone();
        match over_limit(&caller, store.sessions(), &limits) {
            Some(why) => Err(refused(why)),
            None => Ok(caller),
        }
    }

    /// Start `request` for agent session `caller`, within the limits:
    /// linked to it, its prompt under a line saying who started it, and
    /// counted against its total.
    pub(super) fn start_for(
        self: &Arc<Self>,
        caller: &str,
        mut request: CreateSession,
    ) -> Result<SessionView, HubError> {
        let _gate = self
            .start_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let caller = self.check_start_limits(caller)?;
        if let Some(prompt) = request.prompt.take().filter(|p| !p.trim().is_empty()) {
            request.prompt = Some(started_prompt(&caller, &prompt));
        }
        request.opened_by = Some(caller.id.clone());
        let view = self.create_session(request)?;
        // The caller may be gone meanwhile: nothing left to count on.
        let _ = self.store().count_start(&caller.id);
        Ok(view)
    }

    /// Agent session `caller` starts a new agent session (the
    /// `start_session` tool): its own kind, account, and launch options
    /// (but `model`), in its directory unless `start` names a folder.
    /// Started at once, kept for the operator, or refused, as its policy
    /// says.
    pub fn agent_start(
        self: &Arc<Self>,
        caller: &str,
        start: AgentStart,
    ) -> Result<Opened, HubError> {
        let from = self.store().session(caller)?.clone();
        if !from.runs_agent() {
            return Err(refused(format!(
                "{} is not an agent session: only an agent starts sessions",
                from.name
            )));
        }
        let policy = self.start_policy(&from);
        if policy == StartPolicy::Off {
            return Err(refused(format!(
                "starting sessions is off for {} (its start-sessions setting, or hub.toml \
                 agent_starts.policy); ask the operator to start it, or hand off",
                from.name
            )));
        }
        let name = trimmed(Some(&start.name))
            .ok_or_else(|| refused("give the new session a short name"))?;
        let name = validate_name(&name)?;
        let prompt = agent::validate_prompt(Some(&start.prompt))
            .map_err(StateError::Invalid)?
            .ok_or_else(|| refused("give the new session's whole first prompt"))?;
        let workspace = self.workspace(&from.workspace_id)?;
        let folder = start
            .folder
            .as_deref()
            .map(|f| self.workspace_folder(&workspace, f))
            .transpose()?;
        let worktree = match start.worktree {
            Some(w) => Some(WorktreeRequest {
                task: trimmed(Some(&w.task))
                    .ok_or_else(|| refused("a worktree needs a task name"))?,
                base: trimmed(w.base.as_deref()),
            }),
            None => None,
        };
        let mut launch: AgentLaunch = from.launch.clone().unwrap_or_default();
        if let Some(model) = trimmed(start.model.as_deref()) {
            launch.model = Some(model);
        }
        let pending = PendingStart {
            id: new_id(),
            workspace_id: from.workspace_id.clone(),
            name: Some(self.unique_session_name(&name)),
            kind: from.kind,
            prompt: Some(prompt),
            folder,
            worktree,
            launch: (!launch.is_empty()).then_some(launch),
            account: from.account.clone(),
            created_at: Utc::now(),
        };
        // Resolved now, so a bad folder is refused now.
        self.start_request(&from, &pending)?;
        self.decide_start(&from, policy, pending)
    }

    /// `fw hub new` (the API) opening a session: a local agent session
    /// opening an agent session is held to its start policy and the
    /// limits; anything else opens at once.
    pub fn open_session(self: &Arc<Self>, request: CreateSession) -> Result<Opened, HubError> {
        let opener = trimmed(request.opened_by.as_deref())
            .and_then(|id| self.store().session(&id).ok().cloned())
            .filter(SessionRecord::runs_agent);
        let Some(opener) = opener.filter(|_| request.kind.is_agent()) else {
            return self
                .create_session(request)
                .map(|view| Opened::Started(Box::new(view)));
        };
        let policy = self.start_policy(&opener);
        if policy == StartPolicy::Off {
            return Err(refused(format!(
                "starting sessions is off for {} (its start-sessions setting, or hub.toml \
                 agent_starts.policy): fw hub new from inside it is refused",
                opener.name
            )));
        }
        if policy == StartPolicy::Allowed {
            return self
                .start_for(&opener.id, request)
                .map(|view| Opened::Started(Box::new(view)));
        }
        if request.resume.is_some() {
            return Err(refused(
                "a resumed conversation cannot wait on the operator's approval",
            ));
        }
        let workspace = self.workspace(&request.workspace_id)?;
        let folder = match &request.repo {
            Some(repo) => self.workspace_folder(&workspace, repo)?,
            None => workspace.first_folder().to_path_buf(),
        };
        let pending = PendingStart {
            id: new_id(),
            workspace_id: workspace.id.clone(),
            name: request.name.as_deref().map(validate_name).transpose()?,
            kind: request.kind,
            prompt: agent::validate_prompt(request.prompt.as_deref())
                .map_err(StateError::Invalid)?,
            folder: Some(folder),
            worktree: request.worktree.map(|w| WorktreeRequest {
                task: w.task,
                base: w.base,
            }),
            launch: (!request.launch.is_empty()).then_some(request.launch),
            account: trimmed(request.account.as_deref()),
            created_at: Utc::now(),
        };
        self.decide_start(&opener, policy, pending)
    }

    /// Start `pending` now (`allowed`) or keep it for the operator (`ask`).
    fn decide_start(
        self: &Arc<Self>,
        from: &SessionRecord,
        policy: StartPolicy,
        pending: PendingStart,
    ) -> Result<Opened, HubError> {
        match policy {
            StartPolicy::Allowed => {
                let request = self.start_request(from, &pending)?;
                self.start_for(&from.id, request)
                    .map(|view| Opened::Started(Box::new(view)))
            }
            StartPolicy::Ask => {
                // A start that could not run now is refused now, not after
                // the operator says yes.
                self.check_start_limits(&from.id)?;
                self.store().add_start_request(&from.id, pending.clone())?;
                self.emit(HubEvent::Sessions);
                Ok(Opened::Waiting {
                    from: SessionRef {
                        id: from.id.clone(),
                        name: from.name.clone(),
                    },
                    request: Box::new(pending),
                })
            }
            StartPolicy::Off => Err(refused(format!(
                "starting sessions is off for {}",
                from.name
            ))),
        }
    }

    /// The session request `pending` (asked by `from`) opens: its folder,
    /// else `from`'s own directory (a new worktree is cut from `from`'s
    /// repo; without one it shares `from`'s worktree).
    pub(super) fn start_request(
        &self,
        from: &SessionRecord,
        pending: &PendingStart,
    ) -> Result<CreateSession, HubError> {
        let (repo, shared): (Option<PathBuf>, Option<WorktreeRef>) =
            match (&pending.folder, &pending.worktree, &from.worktree) {
                (Some(folder), _, _) => (Some(folder.clone()), None),
                (None, Some(_), Some(own)) => (Some(own.repo.clone()), None),
                (None, None, Some(own)) => (None, Some(own.clone())),
                (None, _, None) => (Some(from.cwd.clone()), None),
            };
        if let Some(repo) = &repo {
            let workspace = self.workspace(&pending.workspace_id)?;
            self.workspace_folder(&workspace, repo)?;
        }
        Ok(CreateSession {
            workspace_id: pending.workspace_id.clone(),
            repo,
            name: pending.name.as_deref().map(|n| self.unique_session_name(n)),
            kind: pending.kind,
            worktree: pending.worktree.as_ref().map(|w| NewWorktree {
                task: w.task.clone(),
                base: w.base.clone(),
                at: None,
            }),
            prompt: pending.prompt.clone(),
            launch: pending.launch.clone().unwrap_or_default(),
            account: pending.account.clone(),
            shared_worktree: shared,
            ..CreateSession::default()
        })
    }

    /// The operator starts session `id`'s pending start `start`, with
    /// `prompt` and `name` in place of the asked ones when given; the
    /// asking agent is told. Limits are checked again.
    pub fn approve_start(
        self: &Arc<Self>,
        id: &str,
        start: &str,
        prompt: Option<&str>,
        name: Option<&str>,
    ) -> Result<SessionView, HubError> {
        let from = self.store().session(id)?.clone();
        let mut pending = from
            .start_requests
            .iter()
            .find(|s| s.id == start)
            .cloned()
            .ok_or_else(|| {
                StateError::NotFound(format!("{} has no pending start {start}", from.name))
            })?;
        if let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) {
            pending.prompt = agent::validate_prompt(Some(prompt)).map_err(StateError::Invalid)?;
        }
        if let Some(name) = trimmed(name) {
            pending.name = Some(validate_name(&name)?);
        }
        // Taken before the launch, so two clicks start one session; put
        // back if it does not start.
        let taken = self.store().remove_start_request(id, start)?;
        let started = self
            .start_request(&from, &pending)
            .and_then(|request| self.start_for(id, request));
        let view = match started {
            Ok(view) => view,
            Err(failed) => {
                let _ = self.store().add_start_request(id, taken);
                self.emit(HubEvent::Sessions);
                return Err(failed);
            }
        };
        self.emit(HubEvent::Sessions);
        self.tell_starter(
            &from,
            &format!(
                "From the flywheel hub: the operator started {} (id {}) from your start request \
                 {start}. It shows in list_sessions; message it with message_session, check on it \
                 with session_status.",
                view.record.name, view.record.id
            ),
        );
        Ok(view)
    }

    /// The operator declines session `id`'s pending start `start`; the
    /// asking agent is told.
    pub fn decline_start(&self, id: &str, start: &str) -> Result<(), HubError> {
        let removed = self.store().remove_start_request(id, start)?;
        self.emit(HubEvent::Sessions);
        // Read, then let go of the store: telling it takes it again.
        let from = self.store().session(id).cloned();
        if let Ok(from) = from {
            let named = removed
                .name
                .as_deref()
                .map_or_else(String::new, |n| format!(" ({n})"));
            self.tell_starter(
                &from,
                &format!(
                    "From the flywheel hub: the operator declined your start request {start}{named}; \
                     that session was not started."
                ),
            );
        }
        Ok(())
    }

    /// Type `text` into the agent that asked for a start, now or once its
    /// turn is done. A starter that no longer runs is not told.
    pub(super) fn tell_starter(&self, from: &SessionRecord, text: &str) {
        if !running_agent(from) {
            return;
        }
        if let Err(e) = self.send_or_queue(&from.id, text) {
            eprintln!("fw hub: could not tell {} about its start: {e}", from.name);
        }
    }

    /// Session `to` (name or id) as agent session `asker` sees it: its
    /// state, plan, open ask, last note, who started it, runtime, and cost.
    pub fn agent_session_status(&self, asker: &str, to: &str) -> Result<String, HubError> {
        let (target, starter) = {
            let store = self.store();
            store.session(asker)?;
            let target = crate::script::find_by(
                store.sessions(),
                |s| (s.id.as_str(), s.name.as_str()),
                to,
                "session",
            )
            .map_err(StateError::Invalid)?
            .cloned()
            .ok_or_else(|| {
                StateError::Invalid(format!(
                    "no session matches {:?} (list_sessions names them)",
                    to.trim()
                ))
            })?;
            let starter = target.opened_by.as_deref().map(|id| {
                store.session(id).map_or_else(
                    |_| format!("a removed session (id {id})"),
                    |s| {
                        if s.id == asker {
                            format!("you ({}, id {})", s.name, s.id)
                        } else {
                            format!("{} (id {})", s.name, s.id)
                        }
                    },
                )
            });
            (target, starter)
        };
        let snap = self.telemetry().snapshot(&target.id);
        let mut lines = vec![format!(
            "{} (id {}) [{}] in {}{}",
            target.name,
            target.id,
            target.kind.as_str(),
            target.cwd.display(),
            target
                .worktree
                .as_ref()
                .map_or_else(String::new, |w| format!(", branch {}", w.branch))
        )];
        let now = crate::telemetry::now_ms();
        let state = if target.state != SessionState::Running {
            "exited".to_owned()
        } else {
            match &snap {
                None => "idle (no turn yet)".to_owned(),
                Some(s) => {
                    let mut state = format!("{} for {}", s.status, span(now - s.since));
                    if let Some(tool) = &s.blocked_tool {
                        state.push_str(&format!(", waiting on the operator to allow {tool}"));
                    }
                    state
                }
            }
        };
        lines.push(format!("state: {state}"));
        // What the agent reports it runs, else what it was launched with.
        let reported = snap.as_ref().and_then(|s| s.model.clone());
        let launched = target.launch.as_ref().and_then(|l| l.model.clone());
        lines.push(format!(
            "model: {}",
            match (reported, launched) {
                (Some(model), _) => model,
                (None, Some(model)) => format!("{model} (as launched; not reported yet)"),
                (None, None) => "its CLI's default (not reported yet)".to_owned(),
            }
        ));
        lines.push(format!(
            "started by: {}",
            starter.unwrap_or_else(|| "the operator".to_owned())
        ));
        lines.push(format!(
            "runtime: {}",
            span(now - target.created_at.timestamp_millis())
        ));
        if let Some(cost) = snap
            .as_ref()
            .and_then(|s| s.usage.as_ref())
            .and_then(|u| u.cost_usd)
        {
            lines.push(format!("cost: ${cost:.2}"));
        }
        if let Some(s) = &snap {
            if let Some(plan) = &s.plan {
                let mut line = format!("plan: {} of {} done", plan.done, plan.total);
                if let Some(current) = &plan.current {
                    line.push_str(&format!("; now: {current}"));
                }
                lines.push(line);
            }
            if let Some(title) = &s.turn_title {
                lines.push(format!("turn: {title}"));
            }
            if let Some(ask) = &s.ask {
                let kind = match ask.kind {
                    crate::telemetry::event::AskKind::Answer => "a reply",
                    crate::telemetry::event::AskKind::Decide => "a decision",
                    crate::telemetry::event::AskKind::Run => "a command run",
                    crate::telemetry::event::AskKind::Handoff => "a handoff",
                };
                lines.push(format!("asks the operator for {kind}: {}", ask.text));
            }
            if let Some(error) = &s.last_error {
                lines.push(format!("last error: {error}"));
            }
        }
        if let Some(plan) = self
            .telemetry()
            .detail(None, &target.id)
            .and_then(|d| d.plans.into_iter().find(|p| p.owner.is_none()))
        {
            if let Some(note) = &plan.note {
                lines.push(format!("last note: {note}"));
            }
            for item in &plan.items {
                lines.push(format!("  {} {}", plan_mark(item.status), item.title));
            }
        }
        Ok(lines.join("\n"))
    }
}

/// `prompt` under the lines that say who started the session and where
/// its results go.
fn started_prompt(caller: &SessionRecord, prompt: &str) -> String {
    let from = crate::agent_handoff::header(
        &crate::agent_handoff::Provenance {
            session_id: caller.id.clone(),
            session: caller.name.clone(),
            kind: caller.kind,
            branch: caller.worktree.as_ref().map(|w| w.branch.clone()),
            machine: None,
        },
        caller.cwd.file_name().and_then(|n| n.to_str()),
        "this session was started by that agent session",
    );
    format!(
        "{from}\n\nYou were started by {name} (id {id}). Report your results to it with \
         mcp__flywheel__message_session (to: {id}); ask the operator only what it cannot \
         answer.\n\n{prompt}",
        name = caller.name,
        id = caller.id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::SessionKind;

    fn record(id: &str, opened_by: Option<&str>, running: bool) -> SessionRecord {
        SessionRecord {
            id: id.to_owned(),
            workspace_id: "w".to_owned(),
            name: format!("s-{id}"),
            kind: SessionKind::Claude,
            cwd: PathBuf::from("/r"),
            command: vec!["claude".to_owned()],
            agent_session_id: None,
            created_at: Utc::now(),
            state: if running {
                SessionState::Running
            } else {
                SessionState::stopped(Utc::now(), true)
            },
            fallback_command: None,
            worktree: None,
            companion_of: None,
            launch: None,
            reviewer: None,
            review_of: None,
            account: None,
            continues: None,
            opened_by: opened_by.map(str::to_owned),
            handoffs: Vec::new(),
            start_sessions: None,
            start_requests: Vec::new(),
            starts_made: 0,
            owner: None,
            ending: None,
            ended: None,
            end_requests: Vec::new(),
        }
    }

    fn limits() -> AgentStarts {
        AgentStarts {
            policy: StartPolicy::Allowed,
            max_running: 2,
            max_total: 3,
            max_depth: 2,
            max_hub: 4,
        }
    }

    fn says(why: Option<String>, limit: &str) {
        let why = why.unwrap_or_else(|| panic!("expected {limit} to refuse"));
        assert!(why.contains(limit), "{why}");
    }

    #[test]
    fn each_limit_refuses_by_name() {
        let lead = record("lead", None, true);
        // Two running children: max_running.
        let sessions = vec![
            lead.clone(),
            record("a", Some("lead"), true),
            record("b", Some("lead"), true),
        ];
        says(
            over_limit(&lead, &sessions, &limits()),
            "agent_starts.max_running",
        );
        // An exited child does not count against running.
        let sessions = vec![
            lead.clone(),
            record("a", Some("lead"), true),
            record("b", Some("lead"), false),
        ];
        assert_eq!(over_limit(&lead, &sessions, &limits()), None);
        // The total counts starts made, whatever became of them.
        let mut spent = lead.clone();
        spent.starts_made = 3;
        says(
            over_limit(&spent, &[spent.clone()], &limits()),
            "agent_starts.max_total",
        );
        // lead <- mid <- low: a start by low would be 3 deep.
        let mid = record("mid", Some("lead"), true);
        let low = record("low", Some("mid"), true);
        let chain = vec![lead.clone(), mid.clone(), low.clone()];
        assert_eq!(depth_of(&low, &chain), 2);
        assert_eq!(over_limit(&mid, &chain, &limits()), None);
        says(
            over_limit(&low, &chain, &limits()),
            "agent_starts.max_depth",
        );
        // Four agent-started sessions run on the hub.
        let other = record("other", None, true);
        let busy = vec![
            other.clone(),
            record("x1", Some("p"), true),
            record("x2", Some("p"), true),
            record("x3", Some("q"), true),
            record("x4", Some("q"), true),
        ];
        says(over_limit(&other, &busy, &limits()), "agent_starts.max_hub");
    }

    #[test]
    fn a_cycle_in_the_chain_ends_the_walk() {
        let a = record("a", Some("b"), true);
        let b = record("b", Some("a"), true);
        assert_eq!(depth_of(&a, &[a.clone(), b]), 1);
    }

    #[test]
    fn the_started_prompt_names_its_starter() {
        let lead = record("lead1", None, true);
        let prompt = started_prompt(&lead, "Goal: fix it");
        assert!(prompt.starts_with("From s-lead1 in r: "), "{prompt}");
        assert!(prompt.contains("message_session (to: lead1)"), "{prompt}");
        assert!(prompt.ends_with("Goal: fix it"), "{prompt}");
    }

    #[test]
    fn spans_read_short() {
        assert_eq!(span(40_000), "40s");
        assert_eq!(span(12 * 60_000), "12m");
        assert_eq!(span(3 * 3_600_000 + 12 * 60_000), "3h 12m");
    }
}
