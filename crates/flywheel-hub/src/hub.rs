//! The hub: persisted workspaces and session records plus the live PTY
//! sessions the server owns.

use crate::agent::{
    self, AgentPrograms, ClaudeScreen, CodexActivity, SessionKind, claude_signal, codex_signal,
    which,
};
use crate::auth::{LoginCodes, Logins, Token, TokenError, unix_now};
use crate::paths::ensure_private_dir;
use crate::pty::{ExitInfo, PtySession, SessionEvent, SpawnError, SpawnSpec};
use crate::remote::ServeLock;
use crate::report::{self, Report, ReportError, ReportServer};
use crate::restore::RestoreHow;
use crate::state::{
    HubStore, SessionRecord, SessionState, StateError, Workspace, WorkspaceInput, new_id,
    validate_name,
};
use crate::status::{AgentStatus, Audience, Signal, Snapshot, StatusBoard, StatusView};
use crate::transcript::CommandBook;
use crate::vt::{DEFAULT_SCROLLBACK_LINES, TextSource};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// SIGHUP-to-SIGKILL grace when killing a session.
pub const KILL_GRACE: Duration = Duration::from_secs(3);
/// Hub-wide event broadcast depth; a subscriber further behind resyncs.
const EVENT_DEPTH: usize = 256;
/// How often an agent activity monitor re-checks for quiet.
const MONITOR_POLL: Duration = Duration::from_millis(100);
/// Least time between two looks at a Claude session's screen.
const SCREEN_LOOK: Duration = Duration::from_millis(250);
/// Default terminal size for sessions created without one.
pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 32;

/// Hub construction options.
#[derive(Debug, Clone)]
pub struct HubOptions {
    pub state_dir: PathBuf,
    /// Scrollback lines kept per session.
    pub scrollback_lines: usize,
    /// Command for sessions created without one; defaults to `$SHELL`
    /// (falling back to `/bin/sh`).
    pub default_shell: Vec<String>,
    /// Programs launched for agent sessions (names searched on `PATH`, or
    /// paths).
    pub agents: AgentPrograms,
    /// argv prefix that reaches `fw hub` (e.g. `[/path/to/fw, hub]`); agent
    /// hooks run it with `report ...`. `None` launches agents without
    /// status hooks (their status stays unknown, bar the Codex heuristic).
    pub report_command: Option<Vec<String>>,
    /// Claude Code's config dir (where its transcripts live), for resuming
    /// a relaunched conversation. Defaults to `$CLAUDE_CONFIG_DIR`, else
    /// `~/.claude`.
    pub claude_config_dir: Option<PathBuf>,
    /// Codex's home (where its rollouts live), for resuming a restored
    /// conversation and for suggested commands. Defaults to `$CODEX_HOME`,
    /// else `~/.codex`.
    pub codex_home: Option<PathBuf>,
    /// Restore agent sessions by resuming their conversations (Claude
    /// Code's `--resume`, `codex resume`). Off: they come back as fresh
    /// shells (`fw hub serve --no-agent-resume`).
    pub agent_resume: bool,
}

impl HubOptions {
    /// Defaults for `state_dir`.
    pub fn new(state_dir: PathBuf) -> Self {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_owned());
        Self {
            state_dir,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            default_shell: vec![shell],
            agents: AgentPrograms::default(),
            report_command: None,
            claude_config_dir: agent::claude_config_dir(
                std::env::var_os("CLAUDE_CONFIG_DIR").as_deref(),
                std::env::var_os("HOME").as_deref(),
            ),
            codex_home: agent::codex_home(
                std::env::var_os("CODEX_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            ),
            agent_resume: true,
        }
    }
}

/// Hub failures.
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Spawn(#[from] SpawnError),
    #[error(transparent)]
    Token(#[from] TokenError),
    #[error(transparent)]
    Report(#[from] ReportError),
    #[error("internal error: {0}")]
    Internal(String),
    /// Another hub holds the state dir's serve lock.
    #[error("another hub is already serving state dir {}", .0.display())]
    InUse(PathBuf),
    #[error("state dir {path}: {source}")]
    StateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A request to open a session.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateSession {
    pub workspace_id: String,
    /// One of the workspace's repos; defaults to the first.
    pub repo: Option<PathBuf>,
    pub name: Option<String>,
    /// Shell (the default) or an agent CLI.
    #[serde(default)]
    pub kind: SessionKind,
    /// argv for a shell session; defaults to the hub's shell. Agent
    /// sessions run their CLI and take no command.
    pub command: Option<Vec<String>>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

/// A session record joined with its live state.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    #[serde(flatten)]
    pub record: SessionRecord,
    /// Present while the server holds the session's PTY.
    pub live: Option<LiveView>,
    /// An agent session's status (a finished turn counts as seen once any
    /// client viewed it; the events socket gives each client its own).
    pub agent: Option<StatusView>,
}

/// A hub-wide change, pushed to every events subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubEvent {
    /// An agent session suggested new commands (oldest first).
    Commands {
        session_id: String,
        added: Vec<crate::commands::Command>,
    },
    /// A session's agent status changed (re-read it per client).
    Status { session_id: String },
    /// Sessions or workspaces were created, removed, renamed, or changed
    /// state: re-list them.
    Sessions,
}

/// Facts about the server-owned PTY (present until the session is removed
/// or the hub restarts; `pid` only while the process runs).
#[derive(Debug, Clone, Serialize)]
pub struct LiveView {
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub clients: usize,
    /// Scrollback lines retained by the active screen.
    pub scrollback_lines: usize,
    /// The running process's working directory (where its shell `cd`'d
    /// to), where the platform exposes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

/// What [`Hub::restore_sessions`] did with one session.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub id: String,
    pub name: String,
    /// Where it started (its recorded directory when it failed).
    pub cwd: PathBuf,
    /// How it came back, or why it could not.
    pub result: Result<RestoreHow, String>,
    /// What the operator should know about a restore that worked (its
    /// last directory was gone, so it started in its repo).
    pub note: Option<String>,
}

/// Whether a session kind can be launched on this host.
#[derive(Debug, Clone, Serialize)]
pub struct KindAvailability {
    pub kind: SessionKind,
    /// The resolved program (`None` when not found).
    pub program: Option<PathBuf>,
    pub available: bool,
}

/// The hub.
pub struct Hub {
    options: HubOptions,
    token: RwLock<Token>,
    /// One-time login codes (cleared when the token rotates).
    login_codes: Mutex<LoginCodes>,
    /// Browser logins (cleared when the token rotates).
    logins: Mutex<Logins>,
    /// Bumped on every token rotation; open WebSockets close on a change.
    rotations: tokio::sync::watch::Sender<u64>,
    store: Mutex<HubStore>,
    live: Mutex<HashMap<String, Arc<PtySession>>>,
    board: Mutex<StatusBoard>,
    events: broadcast::Sender<HubEvent>,
    report: Mutex<Option<ReportServer>>,
    /// Sessions whose output an activity monitor subscribes to (session ->
    /// spawn generation), so it is not counted as a client.
    monitored: Mutex<HashMap<String, u64>>,
    /// Suggested commands read from agent transcripts.
    commands: CommandBook,
    /// Held until the hub is dropped (see [`ServeLock`]).
    _serve_lock: ServeLock,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("state_dir", &self.options.state_dir)
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// Open (creating as needed) the state dir, its token, and its state.
    pub fn open(options: HubOptions) -> Result<Arc<Self>, HubError> {
        ensure_private_dir(&options.state_dir).map_err(|source| HubError::StateDir {
            path: options.state_dir.clone(),
            source,
        })?;
        // One hub per state dir, for the hub's whole life: a second would
        // overwrite the first's runtime file and report socket.
        let serve_lock = match ServeLock::acquire(&options.state_dir) {
            Ok(Some(lock)) => lock,
            Ok(None) => return Err(HubError::InUse(options.state_dir.clone())),
            Err(source) => {
                return Err(HubError::StateDir {
                    path: options.state_dir.join(crate::remote::LOCK_FILE),
                    source,
                });
            }
        };
        let token = Token::load_or_create(&options.state_dir)?;
        let logins = Logins::load(&options.state_dir, unix_now());
        let store = HubStore::open(&options.state_dir)?;
        let (events, _) = broadcast::channel(EVENT_DEPTH);
        let hub = Arc::new(Self {
            commands: CommandBook::new(
                options.claude_config_dir.clone(),
                options.codex_home.clone(),
                Some(options.state_dir.join(crate::transcript::COMMANDS_DIR)),
            ),
            options,
            token: RwLock::new(token),
            login_codes: Mutex::new(LoginCodes::default()),
            logins: Mutex::new(logins),
            rotations: tokio::sync::watch::Sender::new(0),
            store: Mutex::new(store),
            live: Mutex::new(HashMap::new()),
            board: Mutex::new(StatusBoard::default()),
            events,
            report: Mutex::new(None),
            monitored: Mutex::new(HashMap::new()),
            _serve_lock: serve_lock,
        });
        let weak = Arc::downgrade(&hub);
        let server = ReportServer::bind(
            &hub.options.state_dir,
            Arc::new(move |report: Report| match weak.upgrade() {
                Some(hub) => hub.handle_report(report),
                None => Err("the hub is shutting down".into()),
            }),
        )?;
        *hub.report.lock().unwrap_or_else(PoisonError::into_inner) = Some(server);
        crate::transcript::spawn_watcher(&hub);
        Ok(hub)
    }

    /// The report socket agent sessions send status to.
    pub fn report_socket(&self) -> PathBuf {
        match &*self.report.lock().unwrap_or_else(PoisonError::into_inner) {
            Some(server) => server.path().to_path_buf(),
            None => self.options.state_dir.join(report::SOCKET_FILE),
        }
    }

    /// Hub-wide events (status changes, session list changes).
    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.events.subscribe()
    }

    pub(crate) fn emit(&self, event: HubEvent) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }

    /// Suggested commands read from agent transcripts.
    pub(crate) fn command_book(&self) -> &CommandBook {
        &self.commands
    }

    /// The current token.
    pub fn token(&self) -> Token {
        self.token
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replace the token (and its file) with a fresh one. Every browser
    /// login, outstanding login code, and open WebSocket stops working;
    /// bearer clients need the new token.
    pub fn rotate_token(&self) -> Result<(), HubError> {
        let mut token = self.token.write().unwrap_or_else(PoisonError::into_inner);
        *token = Token::rotate(&self.options.state_dir)?;
        self.login_codes().clear();
        {
            let mut logins = self.logins();
            logins.clear();
            logins.save(&self.options.state_dir)?;
        }
        drop(token);
        self.rotations.send_modify(|n| *n += 1);
        Ok(())
    }

    /// Notified on every token rotation.
    pub fn subscribe_rotations(&self) -> tokio::sync::watch::Receiver<u64> {
        self.rotations.subscribe()
    }

    /// Mint a one-time login code (valid [`crate::auth::LOGIN_CODE_TTL`]).
    pub fn mint_login_code(&self) -> Result<String, HubError> {
        Ok(self.login_codes().mint(Instant::now())?)
    }

    /// Spend a one-time login code; true when it was live.
    pub fn redeem_login_code(&self, presented: &str) -> bool {
        self.login_codes().redeem(presented, Instant::now())
    }

    /// Start a browser login: the secret its cookie carries.
    pub fn issue_login(&self) -> Result<String, HubError> {
        let mut logins = self.logins();
        let secret = logins.issue(unix_now())?;
        logins.save(&self.options.state_dir)?;
        Ok(secret)
    }

    /// Whether `secret` is a live browser login.
    pub fn login_valid(&self, secret: &str) -> bool {
        self.logins().valid(secret, unix_now())
    }

    /// End the browser login `secret` (logout).
    pub fn revoke_login(&self, secret: &str) -> Result<(), HubError> {
        let mut logins = self.logins();
        if logins.revoke(secret) {
            logins.save(&self.options.state_dir)?;
        }
        Ok(())
    }

    fn logins(&self) -> MutexGuard<'_, Logins> {
        self.logins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn login_codes(&self) -> MutexGuard<'_, LoginCodes> {
        self.login_codes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn state_dir(&self) -> &std::path::Path {
        &self.options.state_dir
    }

    fn store(&self) -> MutexGuard<'_, HubStore> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live(&self) -> MutexGuard<'_, HashMap<String, Arc<PtySession>>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn board(&self) -> MutexGuard<'_, StatusBoard> {
        self.board.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn monitored(&self) -> MutexGuard<'_, HashMap<String, u64>> {
        self.monitored
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    // --- workspaces ---------------------------------------------------------

    pub fn workspaces(&self) -> Vec<Workspace> {
        self.store().workspaces().to_vec()
    }

    pub fn workspace(&self, id: &str) -> Result<Workspace, HubError> {
        Ok(self.store().workspace(id)?.clone())
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
        self.emit(HubEvent::Sessions);
        Ok(())
    }

    // --- sessions -----------------------------------------------------------

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

    fn view(&self, record: SessionRecord) -> SessionView {
        let monitors = usize::from(self.monitored().contains_key(&record.id));
        let live = self.live_session(&record.id).map(|s| {
            let (cols, rows) = s.size();
            let pid = s.pid().filter(|_| s.exit_info().is_none());
            LiveView {
                pid,
                cols,
                rows,
                clients: s.subscriber_count().saturating_sub(monitors),
                scrollback_lines: s.scrollback_lines(),
                cwd: pid.and_then(process_cwd),
            }
        });
        let agent = record
            .runs_agent()
            .then(|| self.board().status(&record.id, Audience::Anyone))
            .flatten();
        SessionView {
            record,
            live,
            agent,
        }
    }

    /// The PTY (running, or exited with its scrollback) for a session this
    /// server spawned.
    pub fn live_session(&self, id: &str) -> Option<Arc<PtySession>> {
        self.live().get(id).cloned()
    }

    /// Spawn a session in one of a workspace's repos.
    pub fn create_session(
        self: &Arc<Self>,
        request: CreateSession,
    ) -> Result<SessionView, HubError> {
        let workspace = self.workspace(&request.workspace_id)?;
        let cwd = match &request.repo {
            None => workspace.repos[0].clone(),
            Some(repo) => {
                let canonical = repo
                    .canonicalize()
                    .map_err(|e| StateError::Invalid(format!("repo {}: {e}", repo.display())))?;
                if !workspace.repos.contains(&canonical) {
                    return Err(StateError::Invalid(format!(
                        "repo {} is not part of workspace {}",
                        canonical.display(),
                        workspace.name
                    ))
                    .into());
                }
                canonical
            }
        };
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
        let id = new_id();
        let name = match request.name.as_deref() {
            Some(raw) => validate_name(raw)?,
            None => self.default_session_name(&workspace.id, &command),
        };
        let record = SessionRecord {
            id: id.clone(),
            workspace_id: workspace.id.clone(),
            name,
            kind: request.kind,
            cwd,
            command,
            // Claude takes its conversation id from the hub, so its
            // transcript and `--resume` are known from the start.
            agent_session_id: (request.kind == SessionKind::Claude)
                .then(|| uuid::Uuid::new_v4().hyphenated().to_string()),
            created_at: Utc::now(),
            state: SessionState::Running,
            fallback_command: None,
        };

        // Insert the live entry and the record under the store lock so the
        // exit callback (which also takes it) cannot observe a session that
        // is live but unrecorded.
        let mut store = self.store();
        let session = match self.spawn_pty(&record, request.cols, request.rows, false) {
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
        self.emit(HubEvent::Sessions);
        Ok(self.view(record))
    }

    /// Spawn the PTY for `record` (its argv, cwd, and id); its exit is
    /// recorded against the id. An agent session also gets its launch
    /// arguments, a fresh report secret, and a fresh `unknown` status --
    /// unless it runs its `fallback_command` (restored as a plain shell),
    /// which launches as a shell. Claude Code resumes its conversation
    /// whenever a transcript exists; Codex only with `resume` (a restore
    /// whose plan said so), as `codex resume <id>`.
    fn spawn_pty(
        self: &Arc<Self>,
        record: &SessionRecord,
        cols: Option<u16>,
        rows: Option<u16>,
        resume: bool,
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
            match kind {
                SessionKind::Claude => {
                    if let Some(conversation) = &record.agent_session_id {
                        argv.extend(agent::claude_args(
                            conversation,
                            self.claude_transcript_exists(record, conversation),
                            report,
                        ));
                    }
                }
                SessionKind::Codex => {
                    argv.extend(agent::codex_args(report));
                    if let Some(thread) = record.agent_session_id.as_deref().filter(|_| resume) {
                        argv.extend(agent::codex_resume_args(thread));
                    }
                }
                SessionKind::Shell => {}
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
                scrollback_lines: self.options.scrollback_lines,
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
    fn conversation_exists(&self, record: &SessionRecord) -> bool {
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

    /// Watch an agent session's terminal for what its reports miss: Codex's
    /// busy indicator (it has no "turn started" hook), and for Claude Code
    /// an interrupted turn (Esc fires no hook) and the folder-trust dialog
    /// (shown before any hook). Runs until the process exits or the PTY
    /// is dropped.
    fn monitor_agent(
        self: &Arc<Self>,
        kind: SessionKind,
        id: &str,
        generation: u64,
        session: &Arc<PtySession>,
    ) {
        // Screen-only snapshot: the monitor wants the live stream.
        let attachment = session.attach_with(0);
        if attachment.exit.is_some() {
            // Exited before the monitor subscribed: its Exit event is gone.
            return;
        }
        let mut events = attachment.events;
        let weak = Arc::downgrade(self);
        let pty = Arc::downgrade(session);
        let id = id.to_owned();
        let thread_id = id.clone();
        self.monitored().insert(id.clone(), generation);
        let spawned = std::thread::Builder::new()
            .name("hub-agent-monitor".into())
            .spawn(move || {
                let id = thread_id;
                let mut codex = CodexActivity::default();
                let mut claude = ClaudeScreen::default();
                // Claude: look at the screen after output, at most every
                // SCREEN_LOOK, and while a settle is pending.
                let mut dirty = true;
                let mut looked: Option<Instant> = None;
                let mut last_spell: Option<u64> = None;
                loop {
                    let mut signal = None;
                    match events.try_recv() {
                        Ok(SessionEvent::Output(bytes)) => {
                            dirty = true;
                            if kind == SessionKind::Codex {
                                signal = codex.feed(&bytes, Instant::now());
                            }
                        }
                        Ok(SessionEvent::Exit(_)) | Err(broadcast::error::TryRecvError::Closed) => {
                            break;
                        }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => dirty = true,
                        Err(broadcast::error::TryRecvError::Empty) => {
                            std::thread::sleep(MONITOR_POLL);
                            if kind == SessionKind::Codex {
                                signal = codex.tick(Instant::now());
                            }
                        }
                    }
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    let now = Instant::now();
                    let due = looked.is_none_or(|at| now.duration_since(at) >= SCREEN_LOOK);
                    if kind == SessionKind::Claude {
                        let raw = hub.board().raw_status(&id, generation);
                        let Some((status, spell)) = raw else {
                            break;
                        };
                        // A report started a new spell: look again even if
                        // the screen did not change (its markers may
                        // already be up).
                        dirty |= last_spell != Some(spell);
                        last_spell = Some(spell);
                        if !((dirty || claude.pending()) && due) {
                            continue;
                        }
                        let Some(pty) = pty.upgrade() else {
                            break;
                        };
                        looked = Some(now);
                        dirty = false;
                        let screen = pty.screen_text(TextSource::Visible);
                        signal = claude.observe(status, spell, &screen.lines, now);
                    }
                    if let Some(signal) = signal {
                        let detail =
                            (signal == Signal::Dialog).then_some(agent::CLAUDE_TRUST_DETAIL);
                        hub.apply_signal(&id, generation, signal, detail);
                    }
                }
                if let Some(hub) = weak.upgrade() {
                    let mut monitored = hub.monitored();
                    if monitored.get(&id) == Some(&generation) {
                        monitored.remove(&id);
                    }
                }
            });
        if spawned.is_err() {
            self.monitored().remove(&id);
        }
    }

    /// Apply a status signal; push the change if there was one.
    fn apply_signal(&self, id: &str, generation: u64, signal: Signal, detail: Option<&str>) {
        let changed = self.board().apply(id, generation, signal, detail);
        if changed {
            self.emit(HubEvent::Status {
                session_id: id.to_owned(),
            });
        }
    }

    /// Handle one agent report from the report socket: authenticate it
    /// against the spawn's secret, then apply what it means.
    pub fn handle_report(&self, report: Report) -> Result<(), String> {
        let (kind, conversation) = {
            let store = self.store();
            let record = store.session(&report.session).map_err(|e| e.to_string())?;
            (record.kind, record.agent_session_id.clone())
        };
        if !kind.is_agent() || report.source != kind.as_str() {
            return Err(format!(
                "session {} is a {} session, not {}",
                report.session,
                kind.as_str(),
                report.source
            ));
        }
        let Some(generation) = self.board().authenticate(&report.session, &report.secret) else {
            return Err(format!(
                "report secret does not match session {}",
                report.session
            ));
        };
        let payload = report.payload.clone().unwrap_or(serde_json::Value::Null);
        let (signal, detail) = match kind {
            SessionKind::Claude => (
                claude_signal(&report.event, report.detail.as_deref()),
                agent::claude_detail(&report.event, &payload),
            ),
            SessionKind::Codex if report.event == "notify" => (codex_signal(&payload), None),
            _ => (None, None),
        };
        if let Some(signal) = signal {
            self.apply_signal(&report.session, generation, signal, detail.as_deref());
        }
        if let Some(reported) = agent::reported_conversation(kind, &payload)
            && conversation.as_deref() != Some(reported.as_str())
        {
            let changed = self
                .store()
                .set_agent_session_id(&report.session, &reported)
                .map_err(|e| e.to_string())?;
            if changed {
                self.emit(HubEvent::Sessions);
            }
        }
        self.commands
            .note_report(&report.session, kind, &report.event, &payload);
        Ok(())
    }

    // --- per-client status views ---------------------------------------------

    /// Register an events client (a browser, by its persistent client id).
    pub fn connect_viewer(&self, client: &str) -> u64 {
        self.board().connect(client)
    }

    pub fn disconnect_viewer(&self, viewer: u64) {
        self.board().disconnect(viewer);
    }

    /// Record which session a client connection is looking at (`None`:
    /// none, or the page is hidden). Viewing a finished turn marks it seen.
    pub fn set_viewing(&self, viewer: u64, session_id: Option<&str>) {
        let seen = self.board().view(viewer, session_id);
        if let (true, Some(id)) = (seen, session_id) {
            self.emit(HubEvent::Status {
                session_id: id.to_owned(),
            });
        }
    }

    /// Every agent session's status and workspace rollup, for one client.
    pub fn status_snapshot(&self, client: &str) -> Snapshot {
        self.board().snapshot(Audience::Client(client))
    }

    /// One session's status and its workspace's rollup, for one client
    /// (`None` once it is no longer tracked).
    pub fn status_for(
        &self,
        client: &str,
        session_id: &str,
    ) -> Option<(StatusView, String, Option<AgentStatus>)> {
        let board = self.board();
        let view = board.status(session_id, Audience::Client(client))?;
        let workspace = board.workspace_of(session_id)?.to_owned();
        let rollup = board.workspace_status(&workspace, Audience::Client(client));
        Some((view, workspace, rollup))
    }

    /// Relaunch an ended session in place: same id, name, kind, command,
    /// and repo, a fresh PTY and scrollback. Works for a session that
    /// exited on this server and for one left stopped by a hub restart.
    /// Refused (409) while it still runs.
    ///
    /// The record's state gates this: it leaves `running` only once the
    /// old PTY's exit callback has run (or the hub restarted), so no late
    /// exit of the old process can land on the relaunched one.
    pub fn restart_session(
        self: &Arc<Self>,
        id: &str,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<SessionView, HubError> {
        let mut store = self.store();
        let record = store.session(id)?.clone();
        if record.state.is_running() {
            return Err(StateError::Conflict(format!(
                "session {:?} is still running",
                record.name
            ))
            .into());
        }
        let workspace = store.workspace(&record.workspace_id)?;
        if !workspace.repos.contains(&record.cwd) {
            return Err(StateError::Invalid(format!(
                "repo {} is no longer part of workspace {}",
                record.cwd.display(),
                workspace.name
            ))
            .into());
        }
        let mut record = record;
        // Relaunching runs the session as itself again (`mark_running`
        // clears a restore's fallback in the record too).
        record.fallback_command = None;
        if record.kind == SessionKind::Claude && record.agent_session_id.is_none() {
            // A record from before the hub assigned conversation ids: give
            // it one, or it would launch without its status hooks.
            let conversation = uuid::Uuid::new_v4().hyphenated().to_string();
            store.set_agent_session_id(id, &conversation)?;
            record.agent_session_id = Some(conversation);
        }
        let session = self.spawn_pty(&record, cols, rows, false)?;
        let record = match store.mark_running(id) {
            Ok(record) => record,
            Err(e) => {
                drop(store);
                session.kill(Duration::ZERO);
                session.release();
                return Err(e.into());
            }
        };
        let previous = self.live().insert(record.id.clone(), session);
        drop(store);
        if let Some(previous) = previous {
            forget(&previous);
        }
        self.emit(HubEvent::Sessions);
        Ok(self.view(record))
    }

    /// Restore every session a clean shutdown stopped (see
    /// [`crate::restore`]), oldest first: each keeps its id, name, and
    /// place, with a fresh PTY in its saved working directory. One that
    /// cannot come back (its directory is gone, its repo left the
    /// workspace, the spawn failed) stays stopped with the reason recorded
    /// as `restore_error`, and is retried on the next start.
    pub fn restore_sessions(self: &Arc<Self>) -> Vec<RestoreOutcome> {
        let candidates: Vec<SessionRecord> = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.state.is_restorable())
            .cloned()
            .collect();
        candidates
            .into_iter()
            .map(|record| {
                let (cwd, result, note) = match self.restore_one(&record) {
                    Ok((how, cwd, note)) => (cwd, Ok(how), note),
                    Err(message) => {
                        let _ = self.store().set_restore_error(&record.id, &message);
                        (record.cwd, Err(message), None)
                    }
                };
                RestoreOutcome {
                    id: record.id,
                    name: record.name,
                    cwd,
                    result,
                    note,
                }
            })
            .collect()
    }

    /// Drop the pending restore of every cleanly stopped session (the hub
    /// started with restore off): they stay stopped, on later starts too,
    /// until relaunched. Returns how many.
    pub fn forgo_restore(&self) -> Result<usize, HubError> {
        Ok(self.store().forgo_restore()?)
    }

    /// Restore one session: how, the directory it started in, and a note
    /// for the operator.
    fn restore_one(
        self: &Arc<Self>,
        record: &SessionRecord,
    ) -> Result<(RestoreHow, PathBuf, Option<String>), String> {
        if !record.cwd.is_dir() {
            return Err(format!(
                "its working directory {} no longer exists",
                record.cwd.display()
            ));
        }
        // Where its shell had `cd`'d to, while that still exists; else the
        // session's repo.
        let (cwd, note) = match &record.state {
            SessionState::Stopped {
                last_cwd: Some(last),
                ..
            } if last.is_dir() => (last.clone(), None),
            SessionState::Stopped {
                last_cwd: Some(last),
                ..
            } if *last != record.cwd => (
                record.cwd.clone(),
                Some(format!(
                    "its last directory {} no longer exists",
                    last.display()
                )),
            ),
            _ => (record.cwd.clone(), None),
        };
        let resume = crate::restore::Resumable {
            enabled: self.options.agent_resume,
            conversation_exists: record.kind.is_agent() && self.conversation_exists(record),
        };
        let plan = crate::restore::plan(record, &self.options.default_shell, resume);
        // An agent resumes where its CLI filed the conversation: the
        // directory it was started in (its repo), not one it reported.
        let (cwd, note) = if plan.how == RestoreHow::Resume {
            (record.cwd.clone(), None)
        } else {
            (cwd, note)
        };
        let mut store = self.store();
        let workspace = store
            .workspace(&record.workspace_id)
            .map_err(|e| e.to_string())?;
        if !workspace.repos.contains(&record.cwd) {
            return Err(format!(
                "repo {} is no longer part of workspace {}",
                record.cwd.display(),
                workspace.name
            ));
        }
        let fallback = (plan.how == RestoreHow::Shell).then_some(plan.command);
        let mut relaunched = record.clone();
        relaunched.fallback_command = fallback.clone();
        relaunched.cwd = cwd.clone();
        let session = self
            .spawn_pty(&relaunched, None, None, plan.how == RestoreHow::Resume)
            .map_err(|e| e.to_string())?;
        if let Err(e) = store.mark_restored(&record.id, fallback) {
            drop(store);
            session.kill(Duration::ZERO);
            session.release();
            return Err(e.to_string());
        }
        let previous = self.live().insert(record.id.clone(), session);
        drop(store);
        if let Some(previous) = previous {
            forget(&previous);
        }
        Ok((plan.how, cwd, note))
    }

    /// Resolve an agent kind's CLI, or explain that it is not installed.
    fn agent_program(&self, kind: SessionKind) -> Result<PathBuf, HubError> {
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
        let count = sessions
            .iter()
            .filter(|s| s.workspace_id == workspace_id)
            .count();
        (count + 1..)
            .map(|n| format!("{program} {n}"))
            .find(|name| !sessions.iter().any(|s| s.name == *name))
            .unwrap_or_else(|| program.clone())
    }

    /// Record an exit. The exited PTY stays in the live map -- its
    /// scrollback remains attachable -- until the session is removed.
    fn on_session_exit(&self, id: &str, generation: Option<u64>, exit: ExitInfo) {
        let _ = self.store().mark_exited(id, exit);
        if let Some(generation) = generation {
            self.apply_signal(id, generation, Signal::Ended, None);
        }
        self.emit(HubEvent::Sessions);
    }

    /// Hang up a session's processes (SIGHUP, SIGKILL after
    /// [`KILL_GRACE`]): the leader if it runs, plus anything it left behind
    /// in its session. The record stays (as exited) until
    /// [`Hub::remove_session`]. A conflict when there is nothing to kill.
    pub fn kill_session(&self, id: &str) -> Result<(), HubError> {
        self.store().session(id)?;
        let Some(session) = self.live_session(id) else {
            return Err(StateError::Conflict(format!(
                "session {id} has no process on this server"
            ))
            .into());
        };
        if session.exit_info().is_some() && session.session_processes().is_empty() {
            return Err(StateError::Conflict(format!("session {id} has already exited")).into());
        }
        session.kill(KILL_GRACE);
        Ok(())
    }

    /// Kill (if running) and forget a session.
    pub fn remove_session(&self, id: &str) -> Result<(), HubError> {
        let mut store = self.store();
        store.remove_session(id)?;
        let live = self.live().remove(id);
        drop(store);
        self.board().untrack(id);
        if let Some(session) = live {
            forget(&session);
        }
        self.emit(HubEvent::Sessions);
        Ok(())
    }

    /// Record every running session as stopped (with the directory its
    /// process is in, for restore), then hang them all up, wait up to
    /// `timeout` for them to exit, and release their PTYs. (Stopped is
    /// recorded first: the hangup's own exit must not read as a crash.)
    pub fn shutdown(&self, timeout: Duration) {
        if let Some(mut server) = self
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            server.stop();
        }
        self.commands.stop();
        let last_cwds: HashMap<String, PathBuf> = self
            .live()
            .iter()
            .filter(|(_, s)| s.exit_info().is_none())
            .filter_map(|(id, s)| Some((id.clone(), process_cwd(s.pid()?)?)))
            .collect();
        let _ = self.store().mark_all_stopped(&last_cwds);
        let sessions: Vec<Arc<PtySession>> = self.live().drain().map(|(_, s)| s).collect();
        let grace = timeout / 2;
        for session in &sessions {
            session.kill(grace);
        }
        let deadline = Instant::now() + timeout;
        for session in &sessions {
            let remaining = deadline.saturating_duration_since(Instant::now());
            session.wait_exit(remaining);
        }
        for session in &sessions {
            session.release();
        }
    }
}

/// The working directory of process `pid`, where the platform exposes it.
fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Hang up a session being forgotten and free its PTY threads.
fn forget(session: &Arc<PtySession>) {
    session.kill(KILL_GRACE);
    session.release();
}
