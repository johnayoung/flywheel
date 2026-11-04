//! The hub: persisted workspaces and session records plus the live PTY
//! sessions the server owns.

mod restore;
mod sessions;
mod status;
mod workspaces;
mod worktrees;

use crate::agent::{self, AgentPrograms, SessionKind};
use crate::auth::{LoginCodes, Logins, Token, TokenError, unix_now};
use crate::detect::Detection;
use crate::git::{self, BadgeKey, BadgeSource, GitStatus, GitWatch, WorktreeRef};
use crate::paths::ensure_private_dir;
use crate::pty::{PtySession, SpawnError};
use crate::remote::ServeLock;
use crate::report::{self, Report, ReportError, ReportServer};
use crate::restore::RestoreHow;
use crate::state::{HubStore, SessionRecord, StateError, Workspace};
use crate::status::{StatusBoard, StatusView};
use crate::transcript::CommandBook;
use crate::vscode;
use crate::vt::DEFAULT_SCROLLBACK_LINES;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
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
    /// Agent launch defaults per kind (`hub.toml` `[agents.*]`): fill
    /// the fields a launch leaves unset.
    pub agent_defaults: crate::config::AgentDefaults,
    /// Session recordings' size caps (see [`crate::record`]; `hub.toml`
    /// `[recordings]`).
    pub record_limits: crate::record::RecordLimits,
    /// New share links' default and longest expiry (`hub.toml`
    /// `[shares]`).
    pub share_policy: crate::share::SharePolicy,
    /// Which agent kind "ask for a review" launches (`review.reviewer`).
    pub reviewer: crate::config::ReviewerChoice,
    /// How long a session goes without typed input before its prompt
    /// queue delivers (`queue.typing_quiet`).
    pub queue_typing_quiet: Duration,
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
            agent_defaults: crate::config::AgentDefaults::default(),
            record_limits: crate::record::RecordLimits::default(),
            share_policy: crate::share::SharePolicy::default(),
            reviewer: crate::config::ReviewerChoice::Other,
            queue_typing_quiet: crate::queue::DEFAULT_TYPING_QUIET,
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
    /// Cut it at this commit of `base` instead of `base`'s tip (set by
    /// fan-outs, so every candidate starts at exactly one commit; not part
    /// of the API).
    #[serde(skip)]
    pub at: Option<String>,
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
    /// The recording being written (`fw hub record start`): its file name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording: Option<String>,
    /// Its prompt queue (see [`crate::queue`]), when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<crate::queue::QueueBrief>,
    /// A worker session's worker: running, or how it ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<crate::flywheel::WorkerRun>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if takes &T
fn is_zero(n: &usize) -> bool {
    *n == 0
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
    /// A verified fan-out changed (launched, graded, picked, removed):
    /// re-read it.
    Fanout { id: String },
    /// The stored UI settings were saved (`rev`: the new revision).
    Settings { rev: u64 },
    /// `hub.toml` was reloaded (applied, or found invalid).
    Config { rev: u64 },
    /// Plugins changed on disk, were enabled or disabled, or ran.
    Plugins,
    /// A plugin's response asked every open UI for a toast.
    PluginToast {
        plugin: String,
        message: String,
        tone: String,
    },
    /// A lifecycle event plugins subscribe to (`session.created`,
    /// `review.merged`, ... see [`crate::plugins::EVENTS`]) with its data.
    Lifecycle {
        event: &'static str,
        payload: serde_json::Value,
    },
    /// An agent finished a turn (its status before seen tracking became
    /// `done`), whether or not anyone was looking.
    TurnFinished { session_id: String },
    /// A plugin ran (on an event or as an action).
    PluginRan {
        plugin: String,
        trigger: String,
        ok: bool,
    },
    /// A row was added to the activity journal (see [`crate::activity`]).
    Activity { row: serde_json::Value },
}

/// Facts about the server-owned PTY (present until the session is removed
/// or the hub restarts; `pid` only while the process runs).
#[derive(Debug, Clone, Serialize)]
pub struct LiveView {
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub clients: usize,
    /// Read-only share-link viewers watching it (not in `clients`: they
    /// never size the terminal).
    #[serde(skip_serializing_if = "is_zero")]
    pub link_viewers: usize,
    /// Scrollback lines retained by the active screen.
    pub scrollback_lines: usize,
    /// The running process's working directory (where its shell `cd`'d
    /// to), where the platform exposes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The output stream (hex; as terminal sockets' `epoch`): another
    /// one once relaunched.
    pub epoch: String,
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
    /// Serializes "ask for a review" (one reviewer per author).
    pub(crate) review_lock: Mutex<()>,
    /// Screen detection manifests (bundled + state-dir overrides).
    detection: Arc<Detection>,
    /// Full-text search caches (see [`crate::search`]).
    search: crate::search::SearchIndex,
    /// Pauses (a handoff) and stops (a shutdown) fan-out gradings.
    grading: Arc<crate::fanout::GradingControl>,
    /// The settings `hub.toml` can change while the hub runs (seeded
    /// from the options; see [`crate::config`]).
    tunables: RwLock<crate::config::Tunables>,
    /// Read-only share links (`fw hub share`).
    shares: crate::share::ShareStore,
    /// Session recordings (`fw hub record`).
    recordings: Arc<crate::record::Recordings>,
    /// Prompt queues' typing times, holds, and delivery gate.
    queue: crate::queue::QueueRuntime,
    /// The activity journal and each client's presence.
    activity: crate::activity::Activity,
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
        // Recordings a previous hub left active ended with its processes.
        hub.recordings.clear_stale();
        let server = ReportServer::bind(&hub.options.state_dir, hub.report_handler())?;
        *hub.report.lock().unwrap_or_else(PoisonError::into_inner) = Some(server);
        crate::transcript::spawn_watcher(&hub);
        git::spawn_watcher(&hub);
        hub.start_queues();
        hub.start_activity();
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
        let tunables = crate::config::Tunables {
            scrollback_lines: options.scrollback_lines,
            worktree_branch_prefix: options.worktree_branch_prefix.clone(),
            import_root: options.import_root.clone(),
            agent_defaults: options.agent_defaults.clone(),
            record_limits: options.record_limits,
            share_policy: options.share_policy,
            reviewer: options.reviewer,
            queue_typing_quiet: options.queue_typing_quiet,
        };
        let record_limits = options.record_limits;
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
            review_lock: Mutex::new(()),
            detection: Arc::new(Detection::with_overrides(&options_state_dir)),
            search: crate::search::SearchIndex::default(),
            grading: Arc::default(),
            tunables: RwLock::new(tunables),
            shares: crate::share::ShareStore::load(&options_state_dir),
            recordings: Arc::new(crate::record::Recordings::new(
                &options_state_dir,
                record_limits,
            )),
            queue: crate::queue::QueueRuntime::default(),
            activity: crate::activity::Activity::open(&options_state_dir),
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

    /// Announce a plugin lifecycle event (one of
    /// [`crate::plugins::EVENTS`]) with `payload`.
    pub fn emit_lifecycle(&self, event: &'static str, payload: serde_json::Value) {
        debug_assert!(crate::plugins::EVENTS.contains(&event), "{event}");
        self.emit(HubEvent::Lifecycle { event, payload });
    }

    /// A session as plugin events describe it (`None`: no such session).
    pub fn event_session(&self, id: &str) -> Option<serde_json::Value> {
        let view = self.session(id).ok()?;
        let workspace = self.workspace(&view.record.workspace_id).ok();
        let record = &view.record;
        Some(serde_json::json!({
            "id": record.id,
            "name": record.name,
            "kind": record.kind,
            "state": record.state,
            "workspace_id": record.workspace_id,
            "workspace": workspace.map(|w| w.name),
            "cwd": record.cwd,
            "worktree": record.worktree,
            "companion_of": record.companion_of,
            "reviewer": record.reviewer,
            "review_of": record.review_of,
            "branch": view.git.as_ref().and_then(|g| g.branch.clone()),
            "agent": view.agent,
        }))
    }

    /// Suggested commands read from agent transcripts.
    pub(crate) fn command_book(&self) -> &CommandBook {
        &self.commands
    }

    /// The activity journal (see [`crate::activity`]).
    pub fn activity(&self) -> &crate::activity::Activity {
        &self.activity
    }

    /// What pauses and stops fan-out gradings (see [`crate::fanout`]).
    pub fn grading(&self) -> &Arc<crate::fanout::GradingControl> {
        &self.grading
    }

    /// Read-only share links.
    pub fn shares(&self) -> &crate::share::ShareStore {
        &self.shares
    }

    /// The prompt queues' runtime side.
    pub(crate) fn queue_runtime(&self) -> &crate::queue::QueueRuntime {
        &self.queue
    }

    /// Session recordings.
    pub fn recordings(&self) -> &Arc<crate::record::Recordings> {
        &self.recordings
    }

    /// Full-text search state.
    pub fn search_index(&self) -> &crate::search::SearchIndex {
        &self.search
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
    pub fn import_root(&self) -> Option<PathBuf> {
        self.tunables().import_root
    }

    /// The live-changeable settings in force (`hub.toml`, flags).
    pub fn tunables(&self) -> crate::config::Tunables {
        self.tunables
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Put reloaded settings in force: sessions, worktrees, imports,
    /// agent launches, and share links started from now on use them;
    /// recordings (running ones too) the new caps.
    pub fn set_tunables(&self, tunables: crate::config::Tunables) {
        self.recordings.set_limits(tunables.record_limits);
        *self
            .tunables
            .write()
            .unwrap_or_else(PoisonError::into_inner) = tunables;
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

    // --- sessions -----------------------------------------------------------

    // --- per-client status views ---------------------------------------------

    // --- layouts --------------------------------------------------------------

    /// Record every running session as stopped (with the directory its
    /// process is in, for restore), then hang them all up, wait up to
    /// `timeout` for them to exit, and release their PTYs. (Stopped is
    /// recorded first: the hangup's own exit must not read as a crash.)
    pub fn shutdown(&self, timeout: Duration) {
        // No prompt is typed into a session being hung up (one under way
        // finishes first).
        self.queue.stop(KILL_GRACE);
        // Graders running now are killed; they run again on the next start.
        self.grading.stop();
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
        let running = self
            .live()
            .values()
            .filter(|s| s.exit_info().is_none())
            .count();
        self.activity.shutdown(running);
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
        // Their exits are written out; every recording is closed.
        self.recordings.stop_all();
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
