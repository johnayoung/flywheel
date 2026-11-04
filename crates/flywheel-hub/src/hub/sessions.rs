//! Session lifecycle: listing, creating (agents and companion shells),
//! spawning their PTYs, naming, killing, and removing sessions.

use super::{
    BADGE_AFTER_RUN, CompanionPaste, CreateSession, DEFAULT_COLS, DEFAULT_ROWS, Hub, HubError,
    HubEvent, KILL_GRACE, KindAvailability, LiveView, READY_TIMEOUT, Readiness, SessionView,
    badge_key, forget, process_cwd, wait_ready,
};
use crate::agent::{self, SessionKind, which};
use crate::git;
use crate::pty::{ExitInfo, PtySession, SpawnSpec};
use crate::report;
use crate::state::{SessionRecord, SessionState, StateError, new_id, validate_name};
use crate::status::{Audience, Signal, StatusSource};
use chrono::Utc;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, Weak};
use std::time::Duration;

impl Hub {
    /// Every session, optionally filtered to one workspace, oldest first.
    pub fn sessions(&self, workspace_id: Option<&str>) -> Vec<SessionView> {
        let records: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| workspace_id.is_none_or(|w| s.workspace_id == w))
            .cloned()
            .collect();
        records.into_iter().map(|r| self.view(r)).collect()
    }

    pub fn session(&self, id: &str) -> Result<SessionView, HubError> {
        let record = self.store().session(id)?.clone();
        Ok(self.view(record))
    }

    pub(super) fn view(&self, record: SessionRecord) -> SessionView {
        // Subscribers that are not viewers: the agent screen monitor, a
        // recorder, and read-only share-link viewers.
        let monitors = usize::from(self.monitored().contains_key(&record.id))
            + usize::from(self.recordings().is_recording(&record.id));
        let link_viewers = self.shares().session_viewers(&record.id);
        let recording = self.recordings().active_file(&record.id);
        let live = self.live_session(&record.id).map(|s| {
            let (cols, rows) = s.size();
            let pid = s.pid().filter(|_| s.exit_info().is_none());
            LiveView {
                pid,
                cols,
                rows,
                clients: s.subscriber_count().saturating_sub(monitors + link_viewers),
                link_viewers,
                scrollback_lines: s.scrollback_lines(),
                cwd: pid.and_then(process_cwd),
            }
        });
        let agent = record
            .runs_agent()
            .then(|| self.board().status(&record.id, Audience::Anyone))
            .flatten();
        let git = self.git.get(&badge_key(&record));
        let usage = self
            .commands
            .usage(&record, chrono::Local::now().date_naive());
        let unchecked = record
            .launch
            .as_ref()
            .is_some_and(agent::AgentLaunch::bypasses_permissions);
        let queue = self.queue_brief(&record.id);
        let worker = crate::flywheel::worker_run(&record, live.as_ref().and_then(|l| l.pid));
        SessionView {
            record,
            live,
            agent,
            git,
            usage,
            unchecked,
            recording,
            queue,
            worker,
        }
    }

    /// The PTY (running, or exited with its scrollback) for a session this
    /// server spawned.
    pub fn live_session(&self, id: &str) -> Option<Arc<PtySession>> {
        self.live().get(id).cloned()
    }

    /// Spawn a session in one of a workspace's folders.
    pub fn create_session(
        self: &Arc<Self>,
        request: CreateSession,
    ) -> Result<SessionView, HubError> {
        let workspace = self.workspace(&request.workspace_id)?;
        let cwd = match &request.repo {
            None => workspace.first_folder().to_path_buf(),
            Some(repo) if workspace.has_folder(repo) => repo.clone(),
            Some(repo) => {
                let canonical = repo
                    .canonicalize()
                    .map_err(|e| StateError::Invalid(format!("folder {}: {e}", repo.display())))?;
                if !workspace.has_folder(&canonical) {
                    return Err(StateError::Invalid(format!(
                        "folder {} is not part of workspace {}",
                        canonical.display(),
                        workspace.name
                    ))
                    .into());
                }
                canonical
            }
        };
        if !cwd.is_dir() {
            return Err(StateError::Invalid(format!(
                "folder {} of workspace {} no longer exists (moved or deleted?); restore it or edit the workspace",
                cwd.display(),
                workspace.name
            ))
            .into());
        }
        if request.worktree.is_some() && !crate::state::is_git_root(&cwd) {
            return Err(StateError::Invalid(git::not_a_repo_root(&cwd)).into());
        }
        let command = match (request.kind, request.command) {
            (SessionKind::Shell, Some(argv)) if argv.is_empty() || argv[0].trim().is_empty() => {
                return Err(StateError::Invalid("command must not be empty".into()).into());
            }
            (SessionKind::Shell, Some(argv)) => argv,
            (SessionKind::Shell, None) => self.options.default_shell.clone(),
            (kind, Some(_)) => {
                return Err(StateError::Invalid(format!(
                    "a {} session runs its CLI; it takes no command",
                    kind.as_str()
                ))
                .into());
            }
            (kind, None) => vec![self.agent_program(kind)?.to_string_lossy().into_owned()],
        };
        let name = request.name.as_deref().map(validate_name).transpose()?;
        let tunables = self.tunables();
        let options = tunables
            .agent_defaults
            .fill(request.kind, &request.launch)
            .validated(request.kind)
            .map_err(StateError::Invalid)?;
        let prompt =
            agent::validate_prompt(request.prompt.as_deref()).map_err(StateError::Invalid)?;
        if prompt.is_some() && !request.kind.is_agent() {
            return Err(StateError::Invalid(
                "an initial prompt is for agent sessions; send text to a shell with fw hub send"
                    .into(),
            )
            .into());
        }
        // Everything else checked: only now cut the worktree (git work).
        let worktree = match &request.worktree {
            Some(new) => {
                let mut plan = git::plan(
                    &cwd,
                    &new.task,
                    &tunables.worktree_branch_prefix,
                    new.base.as_deref(),
                )
                .map_err(StateError::Invalid)?;
                plan.start = new.at.clone();
                Some(git::create(&cwd, &plan).map_err(HubError::Internal)?)
            }
            None => None,
        };
        let name = match (name, &worktree) {
            (Some(name), _) => name,
            (None, Some(_)) => self.unique_session_name(
                &request
                    .worktree
                    .as_ref()
                    .map(|w| w.task.trim().to_owned())
                    .unwrap_or_default(),
            ),
            (None, None) => self.default_session_name(&workspace.id, &command),
        };
        let record = SessionRecord {
            id: new_id(),
            workspace_id: workspace.id.clone(),
            name,
            kind: request.kind,
            cwd: worktree.as_ref().map_or(cwd, |w| w.path.clone()),
            command,
            // Claude takes its conversation id from the hub, so its
            // transcript and `--resume` are known from the start.
            agent_session_id: (request.kind == SessionKind::Claude)
                .then(|| uuid::Uuid::new_v4().hyphenated().to_string()),
            created_at: Utc::now(),
            state: SessionState::Running,
            fallback_command: None,
            worktree: worktree.clone(),
            companion_of: None,
            launch: (!options.is_empty()).then_some(options),
            reviewer: None,
            review_of: None,
        };
        if record
            .launch
            .as_ref()
            .is_some_and(agent::AgentLaunch::bypasses_permissions)
        {
            // Said where the operator's hub logs go, not only in a browser.
            eprintln!(
                "fw hub: session {} ({}) starts {} with its permission checks off",
                record.name,
                record.id,
                record.kind.as_str()
            );
        }
        let launched = self.launch(record, request.cols, request.rows, prompt.as_deref());
        if launched.is_err()
            && let Some(worktree) = &worktree
        {
            // Nothing runs in it and nothing was done there: undo the cut.
            let _ = git::remove(worktree, true);
        }
        launched
    }

    /// Spawn and record a new session.
    pub(crate) fn launch(
        self: &Arc<Self>,
        record: SessionRecord,
        cols: Option<u16>,
        rows: Option<u16>,
        prompt: Option<&str>,
    ) -> Result<SessionView, HubError> {
        let id = record.id.clone();
        // Insert the live entry and the record under the store lock so the
        // exit callback (which also takes it) cannot observe a session that
        // is live but unrecorded.
        let mut store = self.store();
        let session = match self.spawn_pty(&record, cols, rows, false, prompt) {
            Ok(session) => session,
            Err(e) => {
                self.board().untrack(&id);
                return Err(e);
            }
        };
        if let Err(e) = store.insert_session(record.clone()) {
            drop(store);
            self.board().untrack(&id);
            session.kill(Duration::ZERO);
            session.release();
            return Err(e.into());
        }
        self.live().insert(id, session);
        drop(store);
        self.git.nudge(badge_key(&record));
        self.emit(HubEvent::Sessions);
        if let Some(session) = self.event_session(&record.id) {
            self.emit_lifecycle("session.created", serde_json::json!({ "session": session }));
        }
        Ok(self.view(record))
    }

    /// Re-read `record`'s git badge shortly (a command just ran there).
    pub(crate) fn nudge_badge_soon(&self, record: &SessionRecord) {
        self.git
            .nudge_after(badge_key(record), &[BADGE_AFTER_RUN, BADGE_AFTER_RUN * 4]);
    }

    /// An agent session's companion shell (same directory, worktree
    /// included), started on demand: the running one, else its ended one
    /// relaunched in place, else a new shell placed right after the
    /// agent. Returns it and whether it was (re)started now.
    pub fn companion(
        self: &Arc<Self>,
        agent_id: &str,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<(SessionView, bool), HubError> {
        let _one = self
            .companion_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (agent, companions) = {
            let store = self.store();
            let agent = store.session(agent_id)?.clone();
            let companions: Vec<SessionRecord> = store
                .sessions()
                .iter()
                .filter(|s| s.companion_of.as_deref() == Some(agent.id.as_str()))
                .cloned()
                .collect();
            (agent, companions)
        };
        if !agent.kind.is_agent() {
            return Err(StateError::Invalid(format!(
                "session {:?} is a {} session; only agent sessions have companion shells",
                agent.name,
                agent.kind.as_str()
            ))
            .into());
        }
        let alive = |s: &SessionRecord| {
            s.state.is_running()
                && self
                    .live_session(&s.id)
                    .is_some_and(|l| l.exit_info().is_none())
        };
        if let Some(running) = companions.iter().find(|s| alive(s)) {
            return Ok((self.view(running.clone()), false));
        }
        if let Some(ended) = companions.iter().find(|s| !s.state.is_running()) {
            return Ok((self.restart_session(&ended.id, cols, rows)?, true));
        }
        let workspace = self.workspace(&agent.workspace_id)?;
        if !workspace.has_folder(agent.folder()) {
            return Err(StateError::Invalid(format!(
                "folder {} is no longer part of workspace {}",
                agent.folder().display(),
                workspace.name
            ))
            .into());
        }
        if !agent.cwd.is_dir() {
            return Err(StateError::Invalid(format!(
                "{}'s directory {} no longer exists",
                agent.name,
                agent.cwd.display()
            ))
            .into());
        }
        let record = SessionRecord {
            id: new_id(),
            workspace_id: agent.workspace_id.clone(),
            name: self.unique_session_name(&format!("{} shell", agent.name)),
            kind: SessionKind::Shell,
            cwd: agent.cwd.clone(),
            command: self.options.default_shell.clone(),
            agent_session_id: None,
            created_at: Utc::now(),
            state: SessionState::Running,
            fallback_command: None,
            worktree: agent.worktree.clone(),
            companion_of: Some(agent.id.clone()),
            launch: None,
            reviewer: None,
            review_of: None,
        };
        Ok((self.launch(record, cols, rows, None)?, true))
    }

    /// Paste `text` into an agent session's companion shell, starting it
    /// first when none runs and waiting (at most [`READY_TIMEOUT`]) for its
    /// prompt; Enter only with `run` (as [`Hub::paste`]).
    pub fn send_to_companion(
        self: &Arc<Self>,
        agent_id: &str,
        text: &str,
        run: bool,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<CompanionPaste, HubError> {
        // Refuse a bad paste before starting anything for it.
        crate::transcript::check_paste(text)?;
        let (view, created) = self.companion(agent_id, cols, rows)?;
        let id = view.record.id.clone();
        let ready = if created {
            let session = self.live_session(&id).ok_or_else(|| {
                StateError::Conflict(format!("{:?} did not start", view.record.name))
            })?;
            wait_ready(&session, READY_TIMEOUT)?
        } else {
            Readiness::Running
        };
        let paste = self.paste(&id, text, run)?;
        Ok(CompanionPaste {
            session: self.session(&id)?,
            created,
            ready,
            paste,
        })
    }

    /// Spawn the PTY for `record` (its argv, cwd, and id); its exit is
    /// recorded against the id. An agent session also gets its launch
    /// arguments, a fresh report secret, and a fresh `unknown` status --
    /// unless it runs its `fallback_command` (restored as a plain shell),
    /// which launches as a shell. Claude Code resumes its conversation
    /// whenever a transcript exists; Codex only with `resume` (a restore
    /// whose plan said so), as `codex resume <id>`.
    pub(super) fn spawn_pty(
        self: &Arc<Self>,
        record: &SessionRecord,
        cols: Option<u16>,
        rows: Option<u16>,
        resume: bool,
        prompt: Option<&str>,
    ) -> Result<Arc<PtySession>, HubError> {
        // Never fall back to wherever the PTY layer would start it (the
        // hub's own directory or $HOME) while the record says otherwise.
        if !record.cwd.is_dir() {
            return Err(StateError::Invalid(format!(
                "its working directory {} no longer exists",
                record.cwd.display()
            ))
            .into());
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        let exit_id = record.id.clone();
        let (kind, mut argv) = match &record.fallback_command {
            Some(fallback) => (SessionKind::Shell, fallback.clone()),
            None => (record.kind, record.command.clone()),
        };
        let mut env = vec![
            (report::SESSION_ENV.to_owned(), record.id.clone()),
            ("FLYWHEEL_HUB_WORKSPACE".into(), record.workspace_id.clone()),
        ];
        let mut generation = None;
        if kind.is_agent() {
            let key = self
                .board()
                .track(&record.id, &record.workspace_id)
                .map_err(HubError::Internal)?;
            generation = Some(key.generation);
            let report = self.options.report_command.as_deref();
            let options = record.launch.clone().unwrap_or_default();
            // The first prompt goes to a new conversation only.
            let mut resuming = resume;
            match kind {
                SessionKind::Claude => {
                    if let Some(conversation) = &record.agent_session_id {
                        let exists = self.claude_transcript_exists(record, conversation);
                        resuming |= exists;
                        argv.extend(agent::claude_args(conversation, exists, report));
                    }
                    argv.extend(options.args(kind));
                }
                SessionKind::Codex => {
                    argv.extend(agent::codex_args(report));
                    argv.extend(options.args(kind));
                    if let Some(thread) = record.agent_session_id.as_deref().filter(|_| resume) {
                        argv.extend(agent::codex_resume_args(thread));
                    }
                }
                SessionKind::Shell => {}
            }
            if let Some(prompt) = prompt.filter(|_| !resuming) {
                argv.extend(agent::prompt_args(kind, prompt));
            }
            env.push((
                report::SOCKET_ENV.to_owned(),
                self.report_socket().to_string_lossy().into_owned(),
            ));
            env.push((
                report::SECRET_ENV.to_owned(),
                key.secret.expose().to_owned(),
            ));
        }
        let session = PtySession::spawn(
            SpawnSpec {
                argv,
                cwd: record.cwd.clone(),
                env,
                // Another hub's report channel (a hub run inside a hub
                // session) and an enclosing agent's markers stay out.
                env_remove: agent::INHERITED_AGENT_ENV
                    .iter()
                    .copied()
                    .chain([report::SOCKET_ENV, report::SECRET_ENV])
                    .map(str::to_owned)
                    .collect(),
                cols: cols.filter(|&c| c > 0).unwrap_or(DEFAULT_COLS),
                rows: rows.filter(|&r| r > 0).unwrap_or(DEFAULT_ROWS),
                scrollback_lines: self.tunables().scrollback_lines,
            },
            move |exit| {
                if let Some(hub) = weak.upgrade() {
                    hub.on_session_exit(&exit_id, generation, exit);
                }
            },
        )?;
        if let Some(generation) = generation {
            self.monitor_agent(kind, &record.id, generation, &session);
        }
        Ok(session)
    }

    /// Whether the agent CLI has `record`'s conversation on disk to resume:
    /// Claude Code's transcript, Codex's rollout.
    pub(super) fn conversation_exists(&self, record: &SessionRecord) -> bool {
        let Some(conversation) = record.agent_session_id.as_deref() else {
            return false;
        };
        match record.kind {
            SessionKind::Claude => self.claude_transcript_exists(record, conversation),
            SessionKind::Codex => self
                .options
                .codex_home
                .as_deref()
                .and_then(|home| agent::codex_rollout(home, conversation))
                .is_some(),
            SessionKind::Shell => false,
        }
    }

    /// Whether Claude Code already has a transcript for `conversation` (a
    /// relaunch then resumes it; reusing `--session-id` would be refused).
    fn claude_transcript_exists(&self, record: &SessionRecord, conversation: &str) -> bool {
        self.options
            .claude_config_dir
            .as_deref()
            .and_then(|dir| agent::claude_transcript(dir, &record.cwd, conversation))
            .is_some()
    }

    /// Resolve an agent kind's CLI, or explain that it is not installed.
    pub(crate) fn agent_program(&self, kind: SessionKind) -> Result<PathBuf, HubError> {
        let program = self.options.agents.program(kind).unwrap_or_default();
        which(program).ok_or_else(|| {
            StateError::Invalid(format!(
                "{} is not installed: no {program:?} on the hub's PATH",
                kind.as_str()
            ))
            .into()
        })
    }

    /// Which session kinds this host can launch.
    pub fn kinds(&self) -> Vec<KindAvailability> {
        SessionKind::ALL
            .into_iter()
            .map(|kind| {
                let program = match kind {
                    SessionKind::Shell => self
                        .options
                        .default_shell
                        .first()
                        .and_then(|shell| which(shell)),
                    agent => self.agent_program(agent).ok(),
                };
                KindAvailability {
                    kind,
                    available: program.is_some(),
                    program,
                }
            })
            .collect()
    }

    /// Rename a session.
    pub fn rename_session(&self, id: &str, name: &str) -> Result<SessionView, HubError> {
        let record = self.store().rename_session(id, name)?;
        self.emit(HubEvent::Sessions);
        Ok(self.view(record))
    }

    /// `base` when no session has that name, else `base 2`, `base 3`, ...
    pub(crate) fn unique_session_name(&self, base: &str) -> String {
        let store = self.store();
        let taken = |name: &str| store.sessions().iter().any(|s| s.name == name);
        let base: String = base
            .chars()
            .take(crate::state::MAX_NAME_CHARS - 4)
            .collect();
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base} {n}"))
            .find(|name| !taken(name))
            .unwrap_or(base)
    }

    fn default_session_name(&self, workspace_id: &str, command: &[String]) -> String {
        let program = command
            .first()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shell".into());
        // Unique across the hub, so the default name addresses it (`fw hub
        // send <name>`).
        let store = self.store();
        let sessions = store.sessions();
        // Numbered per program: the first Claude session is `claude 1`
        // even after two shells.
        let prefix = format!("{program} ");
        let count = sessions
            .iter()
            .filter(|s| s.workspace_id == workspace_id && s.name.starts_with(&prefix))
            .count();
        (count + 1..)
            .map(|n| format!("{program} {n}"))
            .find(|name| !sessions.iter().any(|s| s.name == *name))
            .unwrap_or_else(|| program.clone())
    }

    /// Record an exit. The exited PTY stays in the live map -- its
    /// scrollback remains attachable -- until the session is removed.
    pub(super) fn on_session_exit(&self, id: &str, generation: Option<u64>, exit: ExitInfo) {
        let exit_json = serde_json::json!({ "code": exit.code, "signal": exit.signal });
        let _ = self.store().mark_exited(id, exit);
        if let Some(generation) = generation {
            let source = StatusSource::Hub {
                reason: "the agent's process exited".into(),
            };
            self.apply_signal(id, generation, Signal::Ended, None, source);
        }
        self.emit(HubEvent::Sessions);
        if let Some(session) = self.event_session(id) {
            self.emit_lifecycle(
                "session.exited",
                serde_json::json!({ "session": session, "exit": exit_json }),
            );
        }
    }

    /// Hang up a session's processes (SIGHUP, SIGKILL after
    /// [`KILL_GRACE`]): the leader if it runs, plus anything it left behind
    /// in its session. The record stays (as exited) until
    /// [`Hub::remove_session`]. A conflict when there is nothing to kill.
    pub fn kill_session(&self, id: &str) -> Result<(), HubError> {
        let label = self.store().session(id)?.label();
        let Some(session) = self.live_session(id) else {
            return Err(StateError::Conflict(format!(
                "session {label} has no process on this server"
            ))
            .into());
        };
        if session.exit_info().is_some() && session.session_processes().is_empty() {
            return Err(StateError::Conflict(format!("session {label} has already exited")).into());
        }
        session.kill(KILL_GRACE);
        Ok(())
    }

    /// Kill (if running) and forget a session.
    pub fn remove_session(&self, id: &str) -> Result<(), HubError> {
        let described = self.event_session(id);
        let mut store = self.store();
        let queued = store
            .queue(id)
            .map_or(0, crate::queue::SessionQueue::pending_count);
        let next = store
            .queue(id)
            .and_then(|q| q.head().map(|i| crate::queue::event_item(i, None)));
        let relaid = store.remove_session(id)?;
        let live = self.live().remove(id);
        drop(store);
        self.board().untrack(id);
        self.queue_runtime().forget(id);
        if let Some(session) = live {
            forget(&session);
        }
        // Its usage moves to the archive now, so no rollup misses it.
        self.commands.forget(&[id.to_owned()]);
        self.emit(HubEvent::Sessions);
        if let Some((workspace_id, rev)) = relaid {
            self.emit(HubEvent::Layout { workspace_id, rev });
        }
        // Its share links end with it: a link to nothing would still count
        // against the live-link cap and answer for a session that is gone.
        match self.shares().revoke_all(Some(id)) {
            Ok(revoked) => {
                for info in revoked {
                    self.emit_lifecycle(
                        "share.revoked",
                        serde_json::json!({ "session": described.clone(), "share": info.event() }),
                    );
                }
            }
            Err(e) => eprintln!("fw hub: removing session {id}: its share links stay: {e}"),
        }
        if let Some(session) = described {
            if queued > 0 {
                // Its queue goes with it: whoever queued must hear of it.
                self.emit_lifecycle(
                    "queue.stalled",
                    serde_json::json!({
                        "session": session.clone(),
                        "reason": "the session was removed",
                        "item": next,
                        "pending": queued,
                    }),
                );
            }
            self.emit_lifecycle("session.removed", serde_json::json!({ "session": session }));
        }
        Ok(())
    }
}
