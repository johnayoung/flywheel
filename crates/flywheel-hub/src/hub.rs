//! The hub: persisted workspaces and session records plus the live PTY
//! sessions the server owns.

use crate::agent::{self, AgentPrograms, SessionKind, claude_signal, codex_signal, which};
use crate::auth::{LoginCodes, Logins, Token, TokenError, unix_now};
use crate::detect::Detection;
use crate::detect::tracker::{Look, ScreenTracker};
use crate::git::{self, BadgeKey, BadgeSource, GitStatus, GitWatch, WorktreeRef};
use crate::paths::ensure_private_dir;
use crate::pty::{ExitInfo, PtySession, SessionEvent, SpawnError, SpawnSpec};
use crate::remote::ServeLock;
use crate::report::{self, Report, ReportError, ReportServer};
use crate::restore::RestoreHow;
use crate::state::{
    FolderSpec, HubStore, SessionRecord, SessionState, StateError, Workspace, WorkspaceInput,
    expand_home, new_id, validate_folder, validate_name,
};
use crate::status::{
    AgentStatus, Audience, Signal, Snapshot, StatusBoard, StatusSource, StatusView,
};
use crate::transcript::CommandBook;
use crate::vscode;
use crate::vt::{DEFAULT_SCROLLBACK_LINES, TextSource};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Handing the hub's live state to another hub process (a live handoff).
#[path = "handoff/hub_side.rs"]
pub(crate) mod handoff;

/// SIGHUP-to-SIGKILL grace when killing a session.
pub const KILL_GRACE: Duration = Duration::from_secs(3);
/// Hub-wide event broadcast depth; a subscriber further behind resyncs.
const EVENT_DEPTH: usize = 256;
/// How often an agent screen monitor wakes with no output (a settle or
/// stall may be due).
const MONITOR_POLL: Duration = Duration::from_millis(100);
/// Least time between two looks at an agent session's screen: output
/// marks the screen dirty, and it is read at most this often (and once
/// more after the output stops).
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
    /// status hooks (their status comes from the screen alone: detection manifests).
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
    /// Where the UI's "Import VS Code workspace" looks for
    /// `*.code-workspace` files (to [`vscode::DEFAULT_SEARCH_DEPTH`]) when
    /// the browser names no root. Defaults to `~/code`.
    pub import_root: Option<PathBuf>,
    /// Prefix of the branches new session worktrees are cut on
    /// (`<prefix><task>`; `fw hub serve --worktree-branch-prefix`).
    pub worktree_branch_prefix: String,
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
            import_root: std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join("code")),
            worktree_branch_prefix: String::new(),
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

/// What [`Hub::import_code_workspace`] did.
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub workspace: Workspace,
    /// Created (else an existing workspace's folders were replaced).
    pub created: bool,
    /// The workspace file, absolute.
    pub file: PathBuf,
    /// Entries not imported, with why.
    pub skipped: Vec<vscode::Skipped>,
}

/// A request to open a session.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateSession {
    pub workspace_id: String,
    /// One of the workspace's folders; defaults to the first. Also
    /// accepted as `folder`.
    #[serde(alias = "folder")]
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
    /// Run it in a new sibling worktree of its (git) folder.
    pub worktree: Option<NewWorktree>,
    /// An agent's first user turn (handed to its CLI at launch; never
    /// again on a relaunch or restore).
    pub prompt: Option<String>,
    /// An agent's model and permission options.
    #[serde(flatten)]
    pub launch: agent::AgentLaunch,
}

/// A new worktree for a session: `<repo-parent>/<repo>.worktrees/<task>`
/// on branch `<prefix><task>`, cut from `base` (default: the repo's
/// current branch).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewWorktree {
    pub task: String,
    pub base: Option<String>,
}

/// How long a new companion shell gets to draw its prompt before a paste
/// goes in anyway.
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Output this long quiet (after some) reads as a drawn prompt.
const READY_QUIET: Duration = Duration::from_millis(500);
const READY_POLL: Duration = Duration::from_millis(25);
/// When a run paste's effect on the git badge is looked at (and again at
/// three times this, for slower commands).
const BADGE_AFTER_RUN: Duration = Duration::from_millis(1500);

/// How a shell was judged ready for a paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    /// It was running already.
    Running,
    /// Its line editor enabled bracketed paste (a prompt is up).
    Bracketed,
    /// It drew output, then went quiet.
    Quiet,
    /// Neither within [`READY_TIMEOUT`]; pasted anyway (typed ahead).
    Timeout,
}

/// What [`Hub::send_to_companion`] did.
#[derive(Debug, Clone, Serialize)]
pub struct CompanionPaste {
    pub session: SessionView,
    /// The companion shell was started (or relaunched) for this paste.
    pub created: bool,
    pub ready: Readiness,
    pub paste: crate::transcript::PasteOutcome,
}

/// What removing a session should also do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct RemoveOptions {
    /// Also remove its worktree (and its companion shells, which run
    /// there); refused when that loses work unless `force`.
    #[serde(default)]
    pub remove_worktree: bool,
    #[serde(default)]
    pub force: bool,
}

/// What [`Hub::remove_session_with`] did.
#[derive(Debug, Clone, Serialize)]
pub struct RemoveOutcome {
    /// Sessions forgotten (the session, then its companions).
    pub removed: Vec<String>,
    pub worktree: Option<git::Removed>,
}

/// A worktree session's worktree, and what removing it would lose.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeInfo {
    pub worktree: WorktreeRef,
    #[serde(flatten)]
    pub check: git::RemovalCheck,
    /// Why removal is refused without force (`None`: safe).
    pub refusal: Option<String>,
    /// Companion shells removed along with it.
    pub companions: Vec<String>,
    /// Other running sessions in it (removal is refused while any runs).
    pub blocking: Vec<String>,
}

/// [`Hub::worktree_plan`]'s answer for the new-session form.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreePlanView {
    /// Where worktrees of this folder go.
    pub dir: Option<PathBuf>,
    /// The base used when none is given.
    pub default_base: Option<String>,
    pub branches: Vec<String>,
    /// The resolved worktree, when `task` was given and is usable.
    pub plan: Option<git::WorktreePlan>,
    pub error: Option<String>,
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
    /// Branch, changes, and ahead/behind of its directory (git folders
    /// and worktrees), from the badge cache: refreshed in the background.
    pub git: Option<GitStatus>,
    /// An agent session's tokens and cost, from its transcript (absent
    /// before its first counted call).
    pub usage: Option<crate::usage::UsageView>,
    /// Its agent runs with its safety checks off (Claude
    /// bypassPermissions, or Codex with no sandbox and no approvals).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unchecked: bool,
}

/// A launch template with the workspace it belongs to.
#[derive(Debug, Clone, Serialize)]
pub struct TemplateView {
    pub workspace_id: String,
    #[serde(flatten)]
    pub template: crate::state::LaunchTemplate,
    /// It starts an agent with its safety checks off.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unchecked: bool,
}

impl TemplateView {
    fn new(workspace_id: String, template: crate::state::LaunchTemplate) -> Self {
        Self {
            unchecked: template.launch.bypasses_permissions(),
            workspace_id,
            template,
        }
    }
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
    /// The last client looking at a session looked away (or left): a
    /// status nobody was told about may now deserve a push.
    Unwatched { session_id: String },
    /// Sessions or workspaces were created, removed, renamed, or changed
    /// state: re-list them.
    Sessions,
    /// A workspace's tabs and splits were saved (or a removed session's
    /// panes closed): `rev` is the new revision.
    Layout { workspace_id: String, rev: u64 },
    /// An agent session's token usage grew (re-read its usage).
    Usage { session_id: String },
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
    /// Bumped whenever a browser login may have ended (logout, eviction);
    /// WebSockets a login opened re-check it on a change.
    login_changes: tokio::sync::watch::Sender<u64>,
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
    /// Session git badges.
    git: GitWatch,
    /// Serializes companion shell creation (one per agent).
    companion_lock: Mutex<()>,
    /// Screen detection manifests (bundled + state-dir overrides).
    detection: Arc<Detection>,
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
        let store = HubStore::open(&options.state_dir)?;
        let hub = Self::assemble(options, serve_lock, store, StatusBoard::default())?;
        let server = ReportServer::bind(&hub.options.state_dir, hub.report_handler())?;
        *hub.report.lock().unwrap_or_else(PoisonError::into_inner) = Some(server);
        crate::transcript::spawn_watcher(&hub);
        git::spawn_watcher(&hub);
        Ok(hub)
    }

    /// The hub over an opened store and status board, holding the serve
    /// lock; no report server or watchers yet.
    fn assemble(
        options: HubOptions,
        serve_lock: ServeLock,
        store: HubStore,
        board: StatusBoard,
    ) -> Result<Arc<Self>, HubError> {
        let token = Token::load_or_create(&options.state_dir)?;
        let logins = Logins::load(&options.state_dir, unix_now());
        let (events, _) = broadcast::channel(EVENT_DEPTH);
        let options_state_dir = options.state_dir.clone();
        Ok(Arc::new(Self {
            commands: CommandBook::new(
                options.claude_config_dir.clone(),
                options.codex_home.clone(),
                Some(options.state_dir.join(crate::transcript::COMMANDS_DIR)),
            )
            .with_archive(crate::usage::archive_path(&options.state_dir)),
            options,
            token: RwLock::new(token),
            login_codes: Mutex::new(LoginCodes::default()),
            logins: Mutex::new(logins),
            rotations: tokio::sync::watch::Sender::new(0),
            login_changes: tokio::sync::watch::Sender::new(0),
            store: Mutex::new(store),
            live: Mutex::new(HashMap::new()),
            board: Mutex::new(board),
            events,
            report: Mutex::new(None),
            monitored: Mutex::new(HashMap::new()),
            git: GitWatch::default(),
            companion_lock: Mutex::new(()),
            detection: Arc::new(Detection::with_overrides(&options_state_dir)),
            _serve_lock: serve_lock,
        }))
    }

    /// What the report server hands each report to.
    fn report_handler(self: &Arc<Self>) -> Arc<report::Handler> {
        let weak = Arc::downgrade(self);
        Arc::new(move |report: Report| match weak.upgrade() {
            Some(hub) => hub.handle_report(report),
            None => Err("the hub is shutting down".into()),
        })
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

    /// Start a browser login: the secret its cookie carries. Issuing may
    /// evict the oldest login (see [`crate::auth::MAX_LOGINS`]).
    pub fn issue_login(&self) -> Result<String, HubError> {
        let mut logins = self.logins();
        let before = logins.len();
        let secret = logins.issue(unix_now())?;
        logins.save(&self.options.state_dir)?;
        if logins.len() <= before {
            drop(logins);
            self.login_changes.send_modify(|n| *n += 1);
        }
        Ok(secret)
    }

    /// Notified whenever a browser login may have ended early (logout or
    /// eviction; expiry is not announced -- re-check [`Hub::login_valid`]
    /// now and then).
    pub fn subscribe_login_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.login_changes.subscribe()
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
            drop(logins);
            self.login_changes.send_modify(|n| *n += 1);
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

    /// The default root the import picker searches.
    pub fn import_root(&self) -> Option<&Path> {
        self.options.import_root.as_deref()
    }

    pub(crate) fn store(&self) -> MutexGuard<'_, HubStore> {
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
        let git = self.git.get(&badge_key(&record));
        let usage = self
            .commands
            .usage(&record, chrono::Local::now().date_naive());
        let unchecked = record
            .launch
            .as_ref()
            .is_some_and(agent::AgentLaunch::bypasses_permissions);
        SessionView {
            record,
            live,
            agent,
            git,
            usage,
            unchecked,
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
        let options = request
            .launch
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
                let plan = git::plan(
                    &cwd,
                    &new.task,
                    &self.options.worktree_branch_prefix,
                    new.base.as_deref(),
                )
                .map_err(StateError::Invalid)?;
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
    fn launch(
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

    /// Watch an agent session's screen for what its reports miss (see
    /// [`crate::detect::tracker`]): Codex's turns and approval prompts,
    /// Claude Code's interrupts (Esc fires no hook) and folder-trust
    /// dialog (shown before any hook). The kind's detection manifest reads
    /// the server's emulator after output, at most every [`SCREEN_LOOK`].
    /// Runs until the process exits or the PTY is dropped.
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
                let mut tracker = ScreenTracker::new(kind);
                let mut dirty = true;
                let mut looked: Option<Instant> = None;
                let mut last_output: Option<Instant> = Some(Instant::now());
                let mut last_spell: Option<u64> = None;
                loop {
                    match events.try_recv() {
                        Ok(SessionEvent::Output(_)) => {
                            dirty = true;
                            last_output = Some(Instant::now());
                        }
                        // A resize redraws: look again.
                        Ok(SessionEvent::Resize { .. }) => dirty = true,
                        Ok(SessionEvent::Exit(_)) | Err(broadcast::error::TryRecvError::Closed) => {
                            break;
                        }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => {
                            dirty = true;
                            last_output = Some(Instant::now());
                        }
                        Err(broadcast::error::TryRecvError::Empty) => {
                            std::thread::sleep(MONITOR_POLL);
                        }
                    }
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    let now = Instant::now();
                    let due = looked.is_none_or(|at| now.duration_since(at) >= SCREEN_LOOK);
                    let Some((status, spell, reported)) = hub.board().raw_status(&id, generation)
                    else {
                        break;
                    };
                    // A report started a new spell: look again even if the
                    // screen did not change (its markers may be up).
                    dirty |= last_spell != Some(spell);
                    last_spell = Some(spell);
                    if !((dirty || tracker.pending()) && due) {
                        continue;
                    }
                    let Some(pty) = pty.upgrade() else {
                        break;
                    };
                    looked = Some(now);
                    dirty = false;
                    let screen = pty.screen_text(TextSource::Visible);
                    drop(pty);
                    let manifest = hub.detection.manifest(kind);
                    let verdict = manifest.evaluate(&screen.lines);
                    let file = manifest.source_file();
                    let decision = tracker.observe(Look {
                        status,
                        reported,
                        spell,
                        verdict: &verdict,
                        file: &file,
                        now,
                        last_output,
                    });
                    if let Some(decision) = decision {
                        hub.apply_screen(&id, generation, spell, decision);
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

    /// Apply what the screen calls for, unless a report landed since it
    /// was read; push the change if there was one.
    fn apply_screen(
        &self,
        id: &str,
        generation: u64,
        spell: u64,
        decision: crate::detect::tracker::Decision,
    ) {
        let changed = self.board().apply_screen(
            id,
            generation,
            spell,
            decision.status,
            decision.detail.as_deref(),
            decision.source,
        );
        if changed {
            self.status_changed(id);
        }
    }

    /// The detection manifests in force.
    pub fn detection(&self) -> &Arc<Detection> {
        &self.detection
    }

    /// Why session `id` shows its status: what set it, and a fresh read
    /// of its live screen with its kind's manifest (`fw hub explain`).
    pub fn explain(&self, id: &str) -> Result<crate::detect::explain::Explanation, HubError> {
        let record = self.store().session(id)?.clone();
        let screen = self
            .live_session(id)
            .map(|pty| pty.screen_text(TextSource::Visible).lines);
        let mut explanation =
            crate::detect::explain::explain_screen(&self.detection, record.kind, screen.as_deref());
        explanation.session = Some(crate::detect::explain::SessionInfo {
            id: record.id.clone(),
            name: record.name.clone(),
        });
        if record.kind.is_agent() {
            let board = self.board();
            explanation.status = board.status(id, Audience::Anyone);
            explanation.source = board.source(id).cloned();
            explanation.source_label = explanation.source.as_ref().map(StatusSource::label);
        }
        Ok(explanation)
    }

    /// Apply a status signal; push the change if there was one.
    fn apply_signal(
        &self,
        id: &str,
        generation: u64,
        signal: Signal,
        detail: Option<&str>,
        source: StatusSource,
    ) {
        let changed = self
            .board()
            .apply_report(id, generation, signal, detail, source);
        if changed {
            self.status_changed(id);
        }
    }

    /// A session's status changed: refresh its badge, tell clients.
    fn status_changed(&self, id: &str) {
        // A turn started or ended: its files may have changed.
        let key = self.store().session(id).ok().map(badge_key);
        if let Some(key) = key {
            self.git.nudge(key);
        }
        self.emit(HubEvent::Status {
            session_id: id.to_owned(),
        });
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
            let source = StatusSource::Hook {
                agent: report.source.clone(),
                event: match &report.detail {
                    Some(qualifier) => format!("{} {qualifier}", report.event),
                    None => report.event.clone(),
                },
            };
            self.apply_signal(
                &report.session,
                generation,
                signal,
                detail.as_deref(),
                source,
            );
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
        let left = self.board().disconnect(viewer);
        for session_id in left {
            self.emit(HubEvent::Unwatched { session_id });
        }
    }

    /// Record every session a client connection shows (each pane of a
    /// split layout; empty: none, or the page is hidden).
    pub fn set_viewing_many(&self, viewer: u64, session_ids: &[&str]) {
        let (seen, left) = {
            let mut board = self.board();
            let before = board.viewing(viewer).to_vec();
            let seen = board.view_many(viewer, session_ids);
            let left: Vec<String> = before
                .into_iter()
                .filter(|b| !board.is_watched(b))
                .collect();
            (seen, left)
        };
        for session_id in left {
            self.emit(HubEvent::Unwatched { session_id });
        }
        for session_id in seen {
            self.emit(HubEvent::Status { session_id });
        }
    }

    /// What a push notification about session `id` would say, and the
    /// facts deciding whether to send it (`None` once it is not tracked).
    pub fn alert_subject(&self, id: &str) -> Option<crate::notify::Subject> {
        let alert = self.board().alert(id)?;
        let store = self.store();
        let session = store.session(id).ok()?;
        let workspace = store
            .workspace(&session.workspace_id)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        let session = session.clone();
        drop(store);
        // A worktree session's branch: the badge cache's (it may have been
        // switched), else the one it was cut on.
        let branch = session.worktree.as_ref().map(|worktree| {
            self.git
                .get(&badge_key(&session))
                .and_then(|git| git.branch)
                .unwrap_or_else(|| worktree.branch.clone())
        });
        Some(crate::notify::Subject {
            session_id: session.id,
            session: session.name,
            workspace,
            branch,
            kind: session.kind,
            alert,
        })
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
        if !workspace.has_folder(record.folder()) {
            return Err(StateError::Invalid(format!(
                "folder {} is no longer part of workspace {}",
                record.folder().display(),
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
        let session = self.spawn_pty(&record, cols, rows, false, None)?;
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
        self.git.nudge(badge_key(&record));
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
        if !workspace.has_folder(record.folder()) {
            return Err(format!(
                "folder {} is no longer part of workspace {}",
                record.folder().display(),
                workspace.name
            ));
        }
        let fallback = (plan.how == RestoreHow::Shell).then_some(plan.command);
        let mut relaunched = record.clone();
        relaunched.fallback_command = fallback.clone();
        relaunched.cwd = cwd.clone();
        let session = self
            .spawn_pty(
                &relaunched,
                None,
                None,
                plan.how == RestoreHow::Resume,
                None,
            )
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

    /// `base` when no session has that name, else `base 2`, `base 3`, ...
    fn unique_session_name(&self, base: &str) -> String {
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
            let source = StatusSource::Hub {
                reason: "the agent's process exited".into(),
            };
            self.apply_signal(id, generation, Signal::Ended, None, source);
        }
        self.emit(HubEvent::Sessions);
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
        let mut store = self.store();
        let relaid = store.remove_session(id)?;
        let live = self.live().remove(id);
        drop(store);
        self.board().untrack(id);
        if let Some(session) = live {
            forget(&session);
        }
        // Its usage moves to the archive now, so no rollup misses it.
        self.commands.forget(&[id.to_owned()]);
        self.emit(HubEvent::Sessions);
        if let Some((workspace_id, rev)) = relaid {
            self.emit(HubEvent::Layout { workspace_id, rev });
        }
        Ok(())
    }

    // --- layouts --------------------------------------------------------------

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
        self.git.stop();
        let last_cwds: HashMap<String, PathBuf> = self
            .live()
            .iter()
            .filter(|(_, s)| s.exit_info().is_none())
            .filter_map(|(id, s)| Some((id.clone(), process_cwd(s.pid()?)?)))
            .collect();
        let _ = self.store().mark_all_stopped(&last_cwds);
        let sessions: Vec<Arc<PtySession>> = self.live().drain().map(|(_, s)| s).collect();
        // On this thread, not `kill`'s detached escalation: the process may
        // exit as soon as this returns, and a `nohup`ed child must not
        // outlive it.
        let grace = timeout / 2;
        PtySession::terminate_all(&sessions, grace, timeout - grace);
        for session in &sessions {
            session.release();
        }
    }
}

impl BadgeSource for Hub {
    fn git_watch(&self) -> &GitWatch {
        &self.git
    }

    fn badge_keys(&self) -> Vec<(BadgeKey, bool)> {
        let store = self.store();
        let live = self.live();
        store
            .sessions()
            .iter()
            .map(|s| {
                let running = live.get(&s.id).is_some_and(|l| l.exit_info().is_none());
                (badge_key(s), running)
            })
            .collect()
    }

    fn badges_changed(&self) {
        self.emit(HubEvent::Sessions);
    }
}

/// What a session's git badge is read for.
fn badge_key(record: &SessionRecord) -> BadgeKey {
    BadgeKey {
        dir: record.cwd.clone(),
        base: record.worktree.as_ref().map(|w| w.base.clone()),
    }
}

/// Wait until a just-started shell is ready for a paste: bracketed paste
/// enabled (a line editor's prompt is up), or output drawn and then quiet
/// for [`READY_QUIET`]; after `timeout`, go ahead anyway (the terminal
/// buffers typed-ahead input). An exit while waiting is a conflict.
fn wait_ready(session: &PtySession, timeout: Duration) -> Result<Readiness, HubError> {
    let deadline = Instant::now() + timeout;
    let mut seen = session.output_bytes();
    let mut since = Instant::now();
    loop {
        if session.exit_info().is_some() {
            return Err(
                StateError::Conflict("the shell exited before its prompt came up".into()).into(),
            );
        }
        if session.bracketed_paste() {
            return Ok(Readiness::Bracketed);
        }
        let now = session.output_bytes();
        if now != seen {
            seen = now;
            since = Instant::now();
        } else if now > 0 && since.elapsed() >= READY_QUIET {
            return Ok(Readiness::Quiet);
        }
        if Instant::now() >= deadline {
            return Ok(Readiness::Timeout);
        }
        std::thread::sleep(READY_POLL);
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
