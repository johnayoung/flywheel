//! The hub: persisted workspaces and session records plus the live PTY
//! sessions the server owns.

use crate::agent::{AgentPrograms, SessionKind, which};
use crate::auth::{LoginCodes, Token, TokenError};
use crate::paths::ensure_private_dir;
use crate::pty::{ExitInfo, PtySession, SpawnError, SpawnSpec};
use crate::state::{
    HubStore, SessionRecord, SessionState, StateError, Workspace, WorkspaceInput, new_id,
    validate_name,
};
use crate::vt::DEFAULT_SCROLLBACK_LINES;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant};

/// SIGHUP-to-SIGKILL grace when killing a session.
pub const KILL_GRACE: Duration = Duration::from_secs(3);
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
    #[error("internal error: {0}")]
    Internal(String),
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
    /// Bumped on every token rotation; open WebSockets close on a change.
    rotations: tokio::sync::watch::Sender<u64>,
    store: Mutex<HubStore>,
    live: Mutex<HashMap<String, Arc<PtySession>>>,
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
        let token = Token::load_or_create(&options.state_dir)?;
        let store = HubStore::open(&options.state_dir)?;
        Ok(Arc::new(Self {
            options,
            token: RwLock::new(token),
            login_codes: Mutex::new(LoginCodes::default()),
            rotations: tokio::sync::watch::Sender::new(0),
            store: Mutex::new(store),
            live: Mutex::new(HashMap::new()),
        }))
    }

    /// The current token.
    pub fn token(&self) -> Token {
        self.token
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replace the token (and its file) with a fresh one. Every login
    /// cookie (named and valued after the old token), outstanding login
    /// code, and open WebSocket stops working; bearer clients need the new
    /// token.
    pub fn rotate_token(&self) -> Result<(), HubError> {
        let mut token = self.token.write().unwrap_or_else(PoisonError::into_inner);
        *token = Token::rotate(&self.options.state_dir)?;
        self.login_codes().clear();
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

    // --- workspaces ---------------------------------------------------------

    pub fn workspaces(&self) -> Vec<Workspace> {
        self.store().workspaces().to_vec()
    }

    pub fn workspace(&self, id: &str) -> Result<Workspace, HubError> {
        Ok(self.store().workspace(id)?.clone())
    }

    pub fn create_workspace(&self, input: WorkspaceInput) -> Result<Workspace, HubError> {
        Ok(self.store().create_workspace(input)?)
    }

    pub fn update_workspace(&self, id: &str, input: WorkspaceInput) -> Result<Workspace, HubError> {
        Ok(self.store().update_workspace(id, input)?)
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
        for session in freed {
            forget(&session);
        }
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
        let live = self.live_session(&record.id).map(|s| {
            let (cols, rows) = s.size();
            LiveView {
                pid: s.pid().filter(|_| s.exit_info().is_none()),
                cols,
                rows,
                clients: s.subscriber_count(),
                scrollback_lines: s.scrollback_lines(),
            }
        });
        SessionView { record, live }
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
            created_at: Utc::now(),
            state: SessionState::Running,
        };

        // Insert the live entry and the record under the store lock so the
        // exit callback (which also takes it) cannot observe a session that
        // is live but unrecorded.
        let mut store = self.store();
        let session = self.spawn_pty(&record, request.cols, request.rows)?;
        if let Err(e) = store.insert_session(record.clone()) {
            drop(store);
            session.kill(Duration::ZERO);
            session.release();
            return Err(e.into());
        }
        self.live().insert(id, session);
        drop(store);
        Ok(self.view(record))
    }

    /// Spawn the PTY for `record` (its argv, cwd, and id); its exit is
    /// recorded against the id.
    fn spawn_pty(
        self: &Arc<Self>,
        record: &SessionRecord,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<Arc<PtySession>, HubError> {
        let weak: Weak<Self> = Arc::downgrade(self);
        let exit_id = record.id.clone();
        Ok(PtySession::spawn(
            SpawnSpec {
                argv: record.command.clone(),
                cwd: record.cwd.clone(),
                env: vec![
                    ("FLYWHEEL_HUB_SESSION".into(), record.id.clone()),
                    ("FLYWHEEL_HUB_WORKSPACE".into(), record.workspace_id.clone()),
                ],
                cols: cols.filter(|&c| c > 0).unwrap_or(DEFAULT_COLS),
                rows: rows.filter(|&r| r > 0).unwrap_or(DEFAULT_ROWS),
                scrollback_lines: self.options.scrollback_lines,
            },
            move |exit| {
                if let Some(hub) = weak.upgrade() {
                    hub.on_session_exit(&exit_id, exit);
                }
            },
        )?)
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
        let session = self.spawn_pty(&record, cols, rows)?;
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
        Ok(self.view(record))
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
        Ok(self.view(record))
    }

    fn default_session_name(&self, workspace_id: &str, command: &[String]) -> String {
        let program = command
            .first()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shell".into());
        let count = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.workspace_id == workspace_id)
            .count();
        format!("{program} {}", count + 1)
    }

    /// Record an exit. The exited PTY stays in the live map -- its
    /// scrollback remains attachable -- until the session is removed.
    fn on_session_exit(&self, id: &str, exit: ExitInfo) {
        let _ = self.store().mark_exited(id, exit);
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
        if let Some(session) = live {
            forget(&session);
        }
        Ok(())
    }

    /// Record every running session as stopped, then hang them all up, wait
    /// up to `timeout` for them to exit, and release their PTYs. (Stopped is
    /// recorded first: the hangup's own exit must not read as a crash.)
    pub fn shutdown(&self, timeout: Duration) {
        let _ = self.store().mark_all_stopped();
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

/// Hang up a session being forgotten and free its PTY threads.
fn forget(session: &Arc<PtySession>) {
    session.kill(KILL_GRACE);
    session.release();
}
