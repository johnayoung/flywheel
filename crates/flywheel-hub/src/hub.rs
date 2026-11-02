//! The hub: persisted workspaces and session records plus the live PTY
//! sessions the server owns.

use crate::auth::{Token, TokenError};
use crate::paths::ensure_private_dir;
use crate::pty::{ExitInfo, PtySession, SpawnError, SpawnSpec};
use crate::scrollback::DEFAULT_SCROLLBACK_BYTES;
use crate::state::{
    HubStore, SessionRecord, SessionState, StateError, Workspace, WorkspaceInput, new_id,
    validate_name,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
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
    pub scrollback_bytes: usize,
    /// Command for sessions created without one; defaults to `$SHELL`
    /// (falling back to `/bin/sh`).
    pub default_shell: Vec<String>,
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
            scrollback_bytes: DEFAULT_SCROLLBACK_BYTES,
            default_shell: vec![shell],
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
    /// argv; defaults to the hub's shell.
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
    pub scrollback_bytes: usize,
}

/// The hub.
pub struct Hub {
    options: HubOptions,
    token: Token,
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
            token,
            store: Mutex::new(store),
            live: Mutex::new(HashMap::new()),
        }))
    }

    pub fn token(&self) -> &Token {
        &self.token
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

    pub fn delete_workspace(&self, id: &str) -> Result<(), HubError> {
        Ok(self.store().delete_workspace(id)?)
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
                scrollback_bytes: s.scrollback_len(),
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
        let command = match request.command {
            Some(argv) if argv.is_empty() || argv[0].trim().is_empty() => {
                return Err(StateError::Invalid("command must not be empty".into()).into());
            }
            Some(argv) => argv,
            None => self.options.default_shell.clone(),
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
            cwd: cwd.clone(),
            command: command.clone(),
            created_at: Utc::now(),
            state: SessionState::Running,
        };

        // Insert the live entry and the record under the store lock so the
        // exit callback (which also takes it) cannot observe a session that
        // is live but unrecorded.
        let mut store = self.store();
        let weak: Weak<Self> = Arc::downgrade(self);
        let exit_id = id.clone();
        let session = PtySession::spawn(
            SpawnSpec {
                argv: command,
                cwd,
                env: vec![
                    ("FLYWHEEL_HUB_SESSION".into(), id.clone()),
                    ("FLYWHEEL_HUB_WORKSPACE".into(), workspace.id.clone()),
                ],
                cols: request.cols.unwrap_or(DEFAULT_COLS),
                rows: request.rows.unwrap_or(DEFAULT_ROWS),
                scrollback_bytes: self.options.scrollback_bytes,
            },
            move |exit| {
                if let Some(hub) = weak.upgrade() {
                    hub.on_session_exit(&exit_id, exit);
                }
            },
        )?;
        if let Err(e) = store.insert_session(record.clone()) {
            drop(store);
            session.kill(Duration::ZERO);
            return Err(e.into());
        }
        self.live().insert(id, session);
        drop(store);
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

    /// Hang up a running session (SIGHUP, SIGKILL after [`KILL_GRACE`]).
    /// The record stays (as exited) until [`Hub::remove_session`].
    pub fn kill_session(&self, id: &str) -> Result<(), HubError> {
        self.store().session(id)?;
        if let Some(session) = self.live_session(id) {
            session.kill(KILL_GRACE);
        }
        Ok(())
    }

    /// Kill (if running) and forget a session.
    pub fn remove_session(&self, id: &str) -> Result<(), HubError> {
        let mut store = self.store();
        store.remove_session(id)?;
        let live = self.live().remove(id);
        drop(store);
        if let Some(session) = live {
            session.kill(KILL_GRACE);
        }
        Ok(())
    }

    /// Kill every live session, wait up to `timeout` for them to exit, and
    /// record anything still running as stopped.
    pub fn shutdown(&self, timeout: Duration) {
        let sessions: Vec<Arc<PtySession>> = self.live().values().cloned().collect();
        let grace = timeout / 2;
        for session in &sessions {
            session.kill(grace);
        }
        let deadline = Instant::now() + timeout;
        for session in &sessions {
            let remaining = deadline.saturating_duration_since(Instant::now());
            session.wait_exit(remaining);
        }
        let _ = self.store().mark_all_stopped();
    }
}
