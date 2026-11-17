//! The hub's persisted model: workspaces (a name plus 1..N folders, git
//! repos or plain directories) and session records, stored as one JSON
//! document (`state.json`, mode 0600, written atomically) in the state dir.
//!
//! Processes belong to the running server, not to this file: a session
//! recorded as `running` when the hub last stopped is loaded back as
//! `stopped` (not `clean`: the hub died without recording it, so it is not
//! restored).

mod store;
pub use store::HubStore;

use crate::agent::{AgentLaunch, SessionKind};
use crate::config::home_dir;
use crate::git::WorktreeRef;
use crate::layout::Layout;
use crate::pty::ExitInfo;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// State file name inside the state dir.
pub const STATE_FILE: &str = "state.json";
/// On-disk schema version: 2 added per-workspace `layouts`, 3 launch
/// `templates` (and sessions' `launch` options), 4 verified `fanouts`, 5
/// per-session prompt `queues`, 6 board tabs in layouts (since removed:
/// dropped on load, see [`crate::layout`]).
/// Older files load and are written back as the current version; a newer
/// hub's file is refused rather than rewritten without what it does not
/// know.
pub const STATE_VERSION: u32 = 6;
/// The oldest schema version still loaded.
pub const STATE_VERSION_MIN: u32 = 1;
/// Limits on workspace input.
pub const MAX_NAME_CHARS: usize = 100;
/// Most handoffs that wait on the operator per session.
pub const MAX_PENDING_HANDOFFS: usize = 20;
/// Most session starts that wait on the operator per session.
pub const MAX_PENDING_STARTS: usize = 20;
/// Most ends that wait on the operator per session.
pub const MAX_PENDING_ENDS: usize = 20;
pub const MAX_FOLDERS: usize = 32;
/// Launch templates kept per workspace.
pub const MAX_TEMPLATES: usize = 50;

/// One directory of a workspace: a git work-tree root or a plain folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Canonical absolute path.
    pub path: PathBuf,
    /// Display label (a VS Code workspace folder's `name`); the UI shows
    /// the directory's name when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether it is a git work-tree root (a `.git` dir, or a `.git` file
    /// for linked worktrees). Not authoritative on disk: recomputed when
    /// the state loads and whenever workspaces are listed. Repo-only
    /// features (worktrees, branch badges) key off it.
    #[serde(default)]
    pub git: bool,
    /// The directory is gone (deleted, moved, or an unmounted volume) as
    /// of the last check. Recomputed like `git`; sessions cannot open
    /// there until it is back.
    #[serde(default)]
    pub missing: bool,
}

impl Folder {
    /// A folder at canonical `path`, its flags read from disk.
    pub fn at(path: PathBuf, name: Option<String>) -> Self {
        let mut folder = Self {
            path,
            name,
            git: false,
            missing: false,
        };
        folder.refresh();
        folder
    }

    /// Re-read the `git` and `missing` flags from disk.
    pub fn refresh(&mut self) {
        self.missing = !self.path.is_dir();
        self.git = !self.missing && is_git_root(&self.path);
    }
}

/// A named set of folders (git repos or plain directories) worked on
/// together.
///
/// Serialized with `folders` plus a legacy `repos` mirror (every folder's
/// path, git or not), so readers of the old shape keep working; loads
/// either (`folders` wins when both are present).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "WorkspaceRepr", into = "WorkspaceRepr")]
pub struct Workspace {
    pub id: String,
    pub name: String,
    /// In operator order; never empty.
    pub folders: Vec<Folder>,
    pub created_at: DateTime<Utc>,
}

impl Workspace {
    /// Whether `path` (canonical) is one of its folders.
    pub fn has_folder(&self, path: &Path) -> bool {
        self.folders.iter().any(|f| f.path == path)
    }

    /// The folder sessions open in by default.
    pub fn first_folder(&self) -> &Path {
        &self.folders[0].path
    }

    /// Re-read every folder's `git` and `missing` flags from disk.
    pub fn refresh_disk(&mut self) {
        for folder in &mut self.folders {
            folder.refresh();
        }
    }
}

#[derive(Serialize, Deserialize)]
struct WorkspaceRepr {
    id: String,
    name: String,
    #[serde(default)]
    folders: Option<Vec<Folder>>,
    /// Legacy: paths only (state files and API clients from before plain
    /// folders). Written as a mirror of `folders`.
    #[serde(default)]
    repos: Option<Vec<PathBuf>>,
    created_at: DateTime<Utc>,
}

impl From<WorkspaceRepr> for Workspace {
    fn from(repr: WorkspaceRepr) -> Self {
        let folders = match (repr.folders, repr.repos) {
            (Some(folders), _) => folders,
            (None, Some(repos)) => repos
                .into_iter()
                .map(|path| Folder {
                    path,
                    name: None,
                    git: false,
                    missing: false,
                })
                .collect(),
            (None, None) => Vec::new(),
        };
        Self {
            id: repr.id,
            name: repr.name,
            folders,
            created_at: repr.created_at,
        }
    }
}

impl From<Workspace> for WorkspaceRepr {
    fn from(workspace: Workspace) -> Self {
        let repos = workspace.folders.iter().map(|f| f.path.clone()).collect();
        Self {
            id: workspace.id,
            name: workspace.name,
            folders: Some(workspace.folders),
            repos: Some(repos),
            created_at: workspace.created_at,
        }
    }
}

/// A session's lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Running,
    Exited {
        #[serde(flatten)]
        exit: ExitInfo,
        at: DateTime<Utc>,
    },
    /// The hub stopped while the session was running.
    Stopped {
        at: DateTime<Utc>,
        /// Recorded by the hub's own (graceful) shutdown, rather than
        /// inferred on load after the hub died: the next start restores it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clean: bool,
        /// Why the last attempt to restore it failed (e.g. its working
        /// directory is gone). Retried on every start until it succeeds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restore_error: Option<String>,
        /// The process's working directory when the hub stopped it (where
        /// the shell had `cd`'d to), where a restore starts it again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_cwd: Option<PathBuf>,
    },
}

impl SessionState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// Stopped by a clean hub shutdown (so restored on start).
    pub fn is_restorable(&self) -> bool {
        matches!(self, Self::Stopped { clean: true, .. })
    }

    /// A state stopped at `at` by the hub's own shutdown (`clean`) or found
    /// running after the hub died.
    pub fn stopped(at: DateTime<Utc>, clean: bool) -> Self {
        Self::Stopped {
            at,
            clean,
            restore_error: None,
            last_cwd: None,
        }
    }
}

/// A terminal session within a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    /// Shell or agent CLI (records from before kinds existed are shells).
    #[serde(default)]
    pub kind: SessionKind,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    /// The agent's own conversation id (Claude Code's `--session-id`,
    /// Codex's thread id): the key to its transcript and to resuming it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(flatten)]
    pub state: SessionState,
    /// What runs in place of `command` after a restore that could not
    /// bring the session back as itself (an agent without native resume
    /// comes back as a hub shell). `kind` and `command` keep what the
    /// session is; relaunching it runs `command` again and clears this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_command: Option<Vec<String>>,
    /// The sibling worktree the session runs in (`cwd` is its directory),
    /// cut from one of the workspace's git folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeRef>,
    /// A shell opened as an agent session's companion (same directory):
    /// the agent's id. Suggested commands go there by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub companion_of: Option<String>,
    /// An agent's model and permission options (every launch, relaunch,
    /// and restore runs it with them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<AgentLaunch>,
    /// The reviewer agent this session's work was last sent to ("ask for
    /// a review"): its id. That session's `review_of` names this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// A reviewer agent: the session whose work it reviews (in that
    /// session's directory, read-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_of: Option<String>,
    /// A Claude session's account ([`crate::accounts`]): it runs under that
    /// token instead of the CLI's own login, on every relaunch and restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The session this one continues (a handoff: its agent handed its
    /// work on): its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<String>,
    /// The agent session that opened this one (`fw hub new` run inside
    /// it): its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_by: Option<String>,
    /// Handoffs its agent proposed (the `handoff` tool) that wait on the
    /// operator: each is removed only when started or dismissed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handoffs: Vec<PendingHandoff>,
    /// Whether its agent may start other agent sessions (the
    /// `start_session` tool, `fw hub new` run inside it); `None`: the
    /// hub's `agent_starts.policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_sessions: Option<StartPolicy>,
    /// Sessions its agent asked to start that wait on the operator: each
    /// is removed only when started or declined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub start_requests: Vec<PendingStart>,
    /// Sessions its agent has started, ever (`agent_starts.max_total`):
    /// removing one does not give the start back.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub starts_made: u32,
    /// The agent session given control of this one (`give_session`): its
    /// id. `None`: whoever opened it (see [`crate::hub`]'s endings for the
    /// effective owner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// An end in progress (`end_session`, `replace_session`): kept until
    /// it ends the session or is called off, across a hub restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ending: Option<Ending>,
    /// How it was ended, when an agent or the operator ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<EndRecord>,
    /// Ends and replacements its agent asked for that wait on the
    /// operator: each is removed only when approved or declined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub end_requests: Vec<PendingEnd>,
}

/// Who acted: an agent session (its id), or the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Actor {
    Session(String),
    Operator,
}

impl Actor {
    /// The session id, for an agent.
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Session(id) => Some(id),
            Self::Operator => None,
        }
    }

    /// As telemetry records it: the session's id, or `operator`.
    pub fn tag(&self) -> String {
        self.session().unwrap_or("operator").to_owned()
    }
}

/// How a session is ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndMode {
    /// A final prompt first (its wind-down), then the end once that turn
    /// is over and nothing runs.
    Graceful,
    /// At its next rest: no final prompt.
    AtRest,
    /// At once (the operator only: the pane menu, `fw hub kill`).
    Force,
}

impl EndMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::AtRest => "at_rest",
            Self::Force => "force",
        }
    }
}

/// Where an end in progress is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndPhase {
    /// The final prompt waits in its queue for the turn boundary.
    AwaitFinal,
    /// The final prompt went: its turn runs.
    FinalTurn,
    /// Waiting for the agent to rest with nothing running.
    Settling,
}

impl EndPhase {
    /// In words, for the agent told where an end stuck.
    pub fn words(self) -> &'static str {
        match self {
            Self::AwaitFinal => "waiting for its final prompt to be delivered",
            Self::FinalTurn => "running the final prompt's turn",
            Self::Settling => "waiting for it to rest with no tool call or subagent running",
        }
    }
}

/// The session a replacement starts once the old one is at rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Successor {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub prompt: String,
    /// One of the workspace's folders; `None`: the old session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeRequest>,
}

/// A queued message an end set aside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedItem {
    pub id: String,
    /// Its sender session (`None`: the operator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Its first line.
    pub preview: String,
}

/// An end in progress (see [`crate::hub`]'s endings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ending {
    pub id: String,
    pub by: Actor,
    pub mode: EndMode,
    /// The final prompt's queue item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_item: Option<String>,
    /// When the final prompt was delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_at: Option<DateTime<Utc>>,
    pub phase: EndPhase,
    pub deadline: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<DroppedItem>,
    /// The set-aside items themselves: put back when the end is called off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<crate::queue::QueueItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<Successor>,
    /// The successor, once started (it starts before the old one ends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<String>,
    pub requested_at: DateTime<Utc>,
}

/// A process an end killed with the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessNote {
    pub pid: u32,
    pub command: String,
    /// The agent's own (an MCP server, its tooling): it ends with any
    /// session, so it is counted, not named, as a loss.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub helper: bool,
}

/// Work an ended session left in its directory (nothing is removed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyNote {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub dirty: u32,
    pub untracked: u32,
    pub unmerged: u32,
}

/// How a session was ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndRecord {
    pub by: Actor,
    pub at: DateTime<Utc>,
    pub mode: EndMode,
    /// Processes other than the agent that went with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub killed: Vec<ProcessNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<DirtyNote>,
    /// The session that replaced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<String>,
}

/// Why an end waits on the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalWhy {
    /// The asker does not own the session.
    NotOwner,
    /// A replacement starts a session, and the asker's start policy is ask.
    StartPolicyAsk,
    Both,
}

/// An end (or replacement) an agent asked for, waiting until the
/// operator approves or declines it. Kept on the asker's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEnd {
    pub id: String,
    /// The session to end: its id.
    pub target: String,
    pub mode: EndMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<Successor>,
    pub why: ApprovalWhy,
    pub created_at: DateTime<Utc>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if takes &T
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// May an agent session start other agent sessions?
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartPolicy {
    /// Each start waits on the operator's Needs-you list.
    #[default]
    Ask,
    /// Starts at once, within the hub's limits.
    Allowed,
    /// Refused.
    Off,
}

impl StartPolicy {
    pub const ALL: [Self; 3] = [Self::Ask, Self::Allowed, Self::Off];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Allowed => "allowed",
            Self::Off => "off",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        Self::ALL
            .into_iter()
            .find(|p| p.as_str().eq_ignore_ascii_case(raw))
            .ok_or_else(|| format!("{raw:?} is not one of ask, allowed, off"))
    }
}

/// A session an agent asked to start, waiting until the operator starts
/// or declines it. Its options are fixed when asked (the agent's own,
/// for the `start_session` tool); its directory is resolved at start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingStart {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: SessionKind,
    /// Its first prompt, as the agent wrote it (the provenance line is
    /// added at start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// One of the workspace's folders; `None`: the asking session's own
    /// directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Run it in a new sibling worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<AgentLaunch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A new worktree a pending start asks for: its task, and the branch to
/// cut it from (default: the repo's current one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRequest {
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// A fresh session an agent proposed to continue its work in: the new
/// session's first prompt, waiting until the operator starts or
/// dismisses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingHandoff {
    pub id: String,
    /// The new session's name, as the agent proposed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Why hand off now, in a line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub prompt: String,
    pub created_at: DateTime<Utc>,
}

/// A saved way to start an agent in a workspace: kind, first prompt,
/// model and permission options, and whether it gets its own worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchTemplate {
    /// Unique within its workspace.
    pub name: String,
    pub kind: SessionKind,
    /// The first user turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(flatten)]
    pub launch: AgentLaunch,
    /// Run it in a new sibling worktree (of a git folder).
    #[serde(default)]
    pub worktree: bool,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

impl LaunchTemplate {
    /// Checked and normalized: a valid name, an agent kind's options only,
    /// a bounded prompt.
    pub fn validated(mut self) -> Result<Self, StateError> {
        self.name = validate_name(&self.name)?;
        if !self.kind.is_agent() {
            return Err(StateError::Invalid(
                "a template starts an agent: kind claude or codex".into(),
            ));
        }
        self.launch = self
            .launch
            .validated(self.kind)
            .map_err(StateError::Invalid)?;
        self.prompt =
            crate::agent::validate_prompt(self.prompt.as_deref()).map_err(StateError::Invalid)?;
        Ok(self)
    }
}

impl SessionRecord {
    /// How errors name the session: `name (id)`.
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.id)
    }

    /// The workspace folder the session belongs to: its worktree's repo,
    /// else its own directory.
    pub fn folder(&self) -> &Path {
        self.worktree.as_ref().map_or(&self.cwd, |w| &w.repo)
    }

    /// Whether the session runs its agent CLI: an agent kind not restored
    /// as a fallback shell (which has no hooks, status, or transcript of
    /// its own).
    pub fn runs_agent(&self) -> bool {
        self.kind.is_agent() && self.fallback_command.is_none()
    }
}

/// `restore_error` of a session found `running` on load: the hub died
/// (killed, crashed, the machine went down) without its own shutdown.
pub const UNCLEAN_STOP: &str = "the hub was not shut down cleanly (killed or crashed)";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateDoc {
    version: u32,
    #[serde(default)]
    workspaces: Vec<Workspace>,
    #[serde(default)]
    sessions: Vec<SessionRecord>,
    /// Workspace id -> its tabs and splits (absent: never laid out).
    #[serde(default)]
    layouts: BTreeMap<String, Layout>,
    /// Workspace id -> its launch templates, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    templates: BTreeMap<String, Vec<LaunchTemplate>>,
    /// Verified fan-outs (see [`crate::fanout`]), oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fanouts: Vec<crate::fanout::Fanout>,
    /// Session id -> its prompt queue (see [`crate::queue`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    queues: BTreeMap<String, crate::queue::SessionQueue>,
}

/// Store and validation failures.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("state file {path}: {message}")]
    Persist { path: PathBuf, message: String },
}

/// A folder as an operator gives it: a path, or `{"path","name"?}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FolderSpec {
    Path(PathBuf),
    Labeled {
        path: PathBuf,
        /// Display label. Absent: an updated workspace keeps the label the
        /// folder already had; empty: no label.
        #[serde(default)]
        name: Option<String>,
    },
}

impl FolderSpec {
    pub fn path(&self) -> &Path {
        match self {
            Self::Path(path) | Self::Labeled { path, .. } => path,
        }
    }
}

/// Workspace fields an operator supplies. `repos` is the legacy spelling
/// of `folders` (paths only).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkspaceInput {
    pub name: Option<String>,
    pub folders: Option<Vec<FolderSpec>>,
    pub repos: Option<Vec<PathBuf>>,
}

impl WorkspaceInput {
    /// The folders given, under either spelling (not both).
    fn folder_specs(&self) -> Result<Option<Vec<FolderSpec>>, StateError> {
        match (&self.folders, &self.repos) {
            (Some(_), Some(_)) => Err(StateError::Invalid(
                "give folders or repos (its legacy name), not both".into(),
            )),
            (Some(folders), None) => Ok(Some(folders.clone())),
            (None, Some(repos)) => Ok(Some(repos.iter().cloned().map(FolderSpec::Path).collect())),
            (None, None) => Ok(None),
        }
    }
}

/// A fresh opaque id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Trim and bound a display name.
pub fn validate_name(raw: &str) -> Result<String, StateError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(StateError::Invalid("name must not be empty".into()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(StateError::Invalid(format!(
            "name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(StateError::Invalid(
            "name must not contain control characters".into(),
        ));
    }
    Ok(name.to_owned())
}

/// Expand a leading `~` (alone or as `~/...`) to `home`.
pub fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Whether `path` is a git work-tree root: a `.git` dir, or the `.git`
/// file of a linked worktree.
pub fn is_git_root(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Check one folder path (`~` expanded): absolute, existing, a directory.
/// Returns it canonical. Git repos and plain directories both qualify.
pub fn validate_folder(raw: &Path) -> Result<PathBuf, StateError> {
    let path = expand_home(raw, home_dir().as_deref());
    if !path.is_absolute() {
        return Err(StateError::Invalid(format!(
            "folder path {} must be absolute",
            raw.display()
        )));
    }
    let canonical = path.canonicalize().map_err(|e| {
        let why = match e.kind() {
            std::io::ErrorKind::NotFound => "does not exist".to_owned(),
            std::io::ErrorKind::PermissionDenied => {
                "is not accessible (permission denied)".to_owned()
            }
            std::io::ErrorKind::NotADirectory => "is not a directory".to_owned(),
            _ => format!("cannot be resolved: {e}"),
        };
        StateError::Invalid(format!("folder path {} {why}", path.display()))
    })?;
    if !canonical.is_dir() {
        return Err(StateError::Invalid(format!(
            "folder path {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Validate a folder label: trimmed, bounded, no control characters;
/// empty means none.
fn validate_label(raw: &str) -> Result<Option<String>, StateError> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    validate_name(raw)
        .map(Some)
        .map_err(|e| StateError::Invalid(format!("folder label: {e}")))
}

/// Canonicalize and check folder specs: 1..=MAX_FOLDERS paths passing
/// [`validate_folder`] (or already in `previous`, kept even while
/// missing), duplicates dropped (the first keeps its place and label). A spec without a label keeps the one its path has in
/// `previous` (the folders being replaced).
pub fn validate_folders(
    specs: &[FolderSpec],
    previous: &[Folder],
) -> Result<Vec<Folder>, StateError> {
    if specs.is_empty() {
        return Err(StateError::Invalid(
            "a workspace needs at least one folder".into(),
        ));
    }
    if specs.len() > MAX_FOLDERS {
        return Err(StateError::Invalid(format!(
            "a workspace holds at most {MAX_FOLDERS} folders"
        )));
    }
    let mut seen = HashSet::new();
    let mut folders = Vec::with_capacity(specs.len());
    for spec in specs {
        // A folder the workspace already has may stay while it is missing
        // (an unmounted volume, a directory moved for a moment): editing
        // the rest of the list must not force it out.
        let path = match previous.iter().find(|f| f.path == spec.path()) {
            Some(kept) if !kept.path.is_dir() => kept.path.clone(),
            _ => validate_folder(spec.path())?,
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let name = match spec {
            FolderSpec::Labeled {
                name: Some(name), ..
            } => validate_label(name)?,
            _ => previous
                .iter()
                .find(|f| f.path == path)
                .and_then(|f| f.name.clone()),
        };
        folders.push(Folder::at(path, name));
    }
    Ok(folders)
}

#[cfg(test)]
mod tests;
