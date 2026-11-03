//! Agent status: what each agent session is doing, as reported by its
//! hooks (Claude Code), its turn-complete notifier (Codex), or its screen
//! ([`crate::detect`] manifests) -- and whether each client has seen a
//! finished turn yet.
//!
//! States (herdr's model):
//!
//! * `working` -- the agent is busy (a prompt was submitted, a tool runs).
//! * `blocked` -- the agent waits on a person: a permission prompt or a
//!   question.
//! * `done` -- the agent finished a turn this client has not looked at.
//! * `idle` -- finished and seen, or waiting for its first prompt.
//! * `unknown` -- no signal yet, or the agent ended.
//!
//! `done` versus `idle` is per client: a turn that finishes while a client
//! views the session is idle for that client at once; every other client
//! sees `done` until it views the session. A session rolls up into its
//! workspace as the most urgent of its sessions: blocked > working > done >
//! idle > unknown.
//!
//! Agent reports are advisory UI state, never lifecycle state: they cannot
//! start, stop, or change a session, only how it is drawn.

use crate::auth::Token;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Handing the board to another hub process (a live handoff).
#[path = "handoff/status_side.rs"]
pub(crate) mod handoff;

/// Longest status detail kept (characters).
pub const MAX_DETAIL_CHARS: usize = 200;

/// What an agent session is doing, as one client sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Working,
    Blocked,
    Done,
    Idle,
    Unknown,
}

impl AgentStatus {
    /// Urgency, for rollups: blocked > working > done > idle > unknown.
    pub fn urgency(self) -> u8 {
        match self {
            Self::Blocked => 4,
            Self::Working => 3,
            Self::Done => 2,
            Self::Idle => 1,
            Self::Unknown => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }
}

/// The most urgent of `statuses` (`None` when there are none).
pub fn rollup(statuses: impl IntoIterator<Item = AgentStatus>) -> Option<AgentStatus> {
    statuses.into_iter().max_by_key(|s| s.urgency())
}

/// A status change an agent (or the hub) reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// The agent waits for its first prompt (session start, `/clear`).
    Ready,
    /// The agent is busy.
    Working,
    /// The agent waits on a person.
    Blocked,
    /// The agent has waited for input a while: blocked if it was working
    /// (a question mid-turn), otherwise nothing new (a finished turn).
    AwaitingInput,
    /// The agent finished a turn.
    Done,
    /// The agent ended (its session closed or its process exited).
    Ended,
}

/// What set a session's status (`fw hub explain`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StatusSource {
    /// Nothing yet.
    #[default]
    None,
    /// The agent's integration: a Claude Code hook or Codex's `notify`.
    Hook { agent: String, event: String },
    /// A detection manifest rule read off the screen.
    Manifest { file: String, rule: String },
    /// A screen fallback with no rule behind it.
    Heuristic { reason: String },
    /// The hub itself (the process ended).
    Hub { reason: String },
}

impl StatusSource {
    /// `hook`, `manifest:<file>#<rule>`, `heuristic`, `hub`, or `none`.
    pub fn label(&self) -> String {
        match self {
            Self::None => "none".into(),
            Self::Hook { .. } => "hook".into(),
            Self::Manifest { file, rule } => format!("manifest:{file}#{rule}"),
            Self::Heuristic { .. } => "heuristic".into(),
            Self::Hub { .. } => "hub".into(),
        }
    }
}

/// The status an entry holds before per-client `seen` is applied, and
/// whether the screen (not a report) set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Raw {
    status: AgentStatus,
    heuristic: bool,
}

/// The transition `signal` causes from `raw`, if any.
fn transition(raw: Raw, signal: Signal) -> Option<Raw> {
    let reported = |status| {
        Some(Raw {
            status,
            heuristic: false,
        })
    };
    let next = match signal {
        Signal::Ready => reported(AgentStatus::Idle),
        Signal::Working => reported(AgentStatus::Working),
        Signal::Blocked => reported(AgentStatus::Blocked),
        Signal::AwaitingInput if raw.status == AgentStatus::Working => {
            reported(AgentStatus::Blocked)
        }
        Signal::AwaitingInput => None,
        Signal::Done => reported(AgentStatus::Done),
        Signal::Ended => reported(AgentStatus::Unknown),
    };
    next.filter(|n| *n != raw)
}

/// One session's status as a client sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusView {
    pub status: AgentStatus,
    /// Why, when known (e.g. the permission the agent asked for).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When the status last changed.
    pub since: DateTime<Utc>,
}

/// Who sees a status: one client, or "anyone" (the REST API, where a turn
/// counts as seen once any client looked at it).
#[derive(Debug, Clone, Copy)]
pub enum Audience<'a> {
    Client(&'a str),
    Anyone,
}

/// The credentials and generation of one spawn of an agent session.
#[derive(Debug, Clone)]
pub struct SpawnKey {
    /// Secret the session's hooks present (fresh per spawn).
    pub secret: Token,
    /// Increases per spawn of the session, so a relaunch ignores late
    /// signals from the process it replaced.
    pub generation: u64,
}

#[derive(Debug)]
struct Entry {
    workspace_id: String,
    raw: Raw,
    detail: Option<String>,
    since: DateTime<Utc>,
    /// Clients that viewed the session since its last `done`.
    seen_by: HashSet<String>,
    key: SpawnKey,
    /// Status changes so far (tells one spell of a status from the next).
    changes: u64,
    /// What set the current status.
    source: StatusSource,
}

impl Entry {
    fn view(&self, audience: Audience<'_>) -> StatusView {
        let seen = match audience {
            Audience::Client(client) => self.seen_by.contains(client),
            Audience::Anyone => !self.seen_by.is_empty(),
        };
        let status = match self.raw.status {
            AgentStatus::Done if seen => AgentStatus::Idle,
            other => other,
        };
        StatusView {
            status,
            detail: self.detail.clone(),
            since: self.since,
        }
    }
}

/// A connected client and the sessions it is looking at (a split layout
/// shows several at once).
#[derive(Debug)]
struct Viewer {
    client: String,
    sessions: Vec<String>,
}

/// Most sessions one connection counts as viewing at once.
pub const MAX_VIEWED: usize = 32;

/// Every agent session's status plus which client views what.
#[derive(Debug, Default)]
pub struct StatusBoard {
    entries: HashMap<String, Entry>,
    viewers: HashMap<u64, Viewer>,
    next_viewer: u64,
    next_generation: u64,
}

/// What a push notification about a session needs: its status before
/// per-client seen tracking, and whether a person has it in front of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub status: AgentStatus,
    pub detail: Option<String>,
    /// Status changes so far: tells one spell of a status from the next.
    pub spell: u64,
    /// A client is viewing the session right now.
    pub watched: bool,
    /// A finished turn some client has already seen.
    pub seen: bool,
}

/// Every session's status and every workspace's rollup, as one client
/// sees them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub sessions: BTreeMap<String, StatusView>,
    pub workspaces: BTreeMap<String, AgentStatus>,
}

impl StatusBoard {
    /// Start tracking a (re)spawned agent session as `unknown`; returns
    /// the spawn's key. A relaunch replaces the previous spawn's key.
    pub fn track(&mut self, session_id: &str, workspace_id: &str) -> Result<SpawnKey, String> {
        let secret = Token::generate().map_err(|e| e.to_string())?;
        self.next_generation += 1;
        let key = SpawnKey {
            secret,
            generation: self.next_generation,
        };
        self.entries.insert(
            session_id.to_owned(),
            Entry {
                workspace_id: workspace_id.to_owned(),
                raw: Raw {
                    status: AgentStatus::Unknown,
                    heuristic: false,
                },
                detail: None,
                since: Utc::now(),
                seen_by: HashSet::new(),
                key: key.clone(),
                changes: 0,
                source: StatusSource::None,
            },
        );
        Ok(key)
    }

    /// Stop tracking a removed session.
    pub fn untrack(&mut self, session_id: &str) {
        self.entries.remove(session_id);
        for viewer in self.viewers.values_mut() {
            viewer.sessions.retain(|s| s != session_id);
        }
    }

    /// Whether `secret` is the current spawn's secret for the session.
    pub fn authenticate(&self, session_id: &str, secret: &str) -> Option<u64> {
        let entry = self.entries.get(session_id)?;
        entry
            .key
            .secret
            .matches(secret)
            .then_some(entry.key.generation)
    }

    /// [`StatusBoard::apply_report`] with no source (tests).
    #[cfg(test)]
    pub(crate) fn apply(
        &mut self,
        session_id: &str,
        generation: u64,
        signal: Signal,
        detail: Option<&str>,
    ) -> bool {
        self.apply_report(session_id, generation, signal, detail, StatusSource::None)
    }

    /// Apply `signal` from spawn `generation` of a session, as set by
    /// `source`. Returns whether anything changed. Signals from a replaced
    /// spawn are ignored.
    pub fn apply_report(
        &mut self,
        session_id: &str,
        generation: u64,
        signal: Signal,
        detail: Option<&str>,
        source: StatusSource,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(session_id) else {
            return false;
        };
        if entry.key.generation != generation {
            return false;
        }
        let Some(next) = transition(entry.raw, signal) else {
            return false;
        };
        entry.raw = next;
        entry.changes += 1;
        entry.since = Utc::now();
        entry.source = source;
        entry.detail = match next.status {
            AgentStatus::Blocked => detail.map(bounded_detail),
            _ => None,
        };
        if next.status == AgentStatus::Done {
            // A turn finished: unseen by everyone except whoever is
            // looking at it right now.
            entry.seen_by = self
                .viewers
                .values()
                .filter(|v| v.sessions.iter().any(|s| s == session_id))
                .map(|v| v.client.clone())
                .collect();
        }
        true
    }

    /// Set the status the screen calls for -- only while the status is
    /// still spell `spell` of spawn `generation` (a report that landed
    /// since the screen was read wins). The screen never sets `done`.
    /// Returns whether anything changed.
    pub fn apply_screen(
        &mut self,
        session_id: &str,
        generation: u64,
        spell: u64,
        status: AgentStatus,
        detail: Option<&str>,
        source: StatusSource,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(session_id) else {
            return false;
        };
        if entry.key.generation != generation
            || entry.changes != spell
            || entry.raw.status == status
            || status == AgentStatus::Done
        {
            return false;
        }
        entry.raw = Raw {
            status,
            heuristic: true,
        };
        entry.changes += 1;
        entry.since = Utc::now();
        entry.source = source;
        entry.detail = match status {
            AgentStatus::Blocked => detail.map(bounded_detail),
            _ => None,
        };
        true
    }

    /// What set a session's current status.
    pub fn source(&self, session_id: &str) -> Option<&StatusSource> {
        self.entries.get(session_id).map(|e| &e.source)
    }

    /// Register a client connection; returns its viewer id.
    pub fn connect(&mut self, client: &str) -> u64 {
        self.next_viewer += 1;
        self.viewers.insert(
            self.next_viewer,
            Viewer {
                client: client.to_owned(),
                sessions: Vec::new(),
            },
        );
        self.next_viewer
    }

    /// Drop a client connection. Returns the sessions it was viewing that
    /// nobody else views any more.
    pub fn disconnect(&mut self, viewer: u64) -> Vec<String> {
        let Some(left) = self.viewers.remove(&viewer) else {
            return Vec::new();
        };
        left.sessions
            .into_iter()
            .filter(|s| !self.is_watched(s))
            .collect()
    }

    /// The sessions a connection is looking at.
    pub fn viewing(&self, viewer: u64) -> &[String] {
        self.viewers
            .get(&viewer)
            .map(|v| v.sessions.as_slice())
            .unwrap_or(&[])
    }

    /// Whether any connection is looking at `session_id`.
    pub fn is_watched(&self, session_id: &str) -> bool {
        self.viewers
            .values()
            .any(|v| v.sessions.iter().any(|s| s == session_id))
    }

    /// Record what a connection is looking at (`None`: nothing, or the
    /// page is hidden). Returns whether a finished turn became seen.
    #[cfg(test)]
    fn view(&mut self, viewer: u64, session_id: Option<&str>) -> bool {
        !self.view_many(viewer, session_id.as_slice()).is_empty()
    }

    /// Record every session a connection shows (the panes on its screen;
    /// empty: nothing, or the page is hidden). Untracked ids are ignored,
    /// duplicates collapse, and at most [`MAX_VIEWED`] count. Returns the
    /// sessions whose finished turn became seen.
    pub fn view_many(&mut self, viewer: u64, session_ids: &[&str]) -> Vec<String> {
        let Some(v) = self.viewers.get_mut(&viewer) else {
            return Vec::new();
        };
        let mut sessions: Vec<String> = Vec::new();
        for id in session_ids {
            if sessions.len() >= MAX_VIEWED {
                break;
            }
            if self.entries.contains_key(*id) && !sessions.iter().any(|s| s == id) {
                sessions.push((*id).to_owned());
            }
        }
        v.sessions = sessions.clone();
        let client = v.client.clone();
        sessions
            .into_iter()
            .filter(|id| match self.entries.get_mut(id) {
                Some(entry) if entry.raw.status == AgentStatus::Done => {
                    entry.seen_by.insert(client.clone())
                }
                _ => false,
            })
            .collect()
    }

    /// Spawn `generation`'s status before seen tracking, how many times
    /// it changed, and whether a report (not the screen) set it (`None`
    /// once the session or spawn is gone).
    pub fn raw_status(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Option<(AgentStatus, u64, bool)> {
        self.entries
            .get(session_id)
            .filter(|e| e.key.generation == generation)
            .map(|e| (e.raw.status, e.changes, !e.raw.heuristic))
    }

    /// The facts a push notification about `session_id` is decided on.
    pub fn alert(&self, session_id: &str) -> Option<Alert> {
        let entry = self.entries.get(session_id)?;
        Some(Alert {
            status: entry.raw.status,
            detail: entry.detail.clone(),
            spell: entry.changes,
            watched: self.is_watched(session_id),
            seen: entry.raw.status == AgentStatus::Done && !entry.seen_by.is_empty(),
        })
    }

    /// One session's status for `audience`.
    pub fn status(&self, session_id: &str, audience: Audience<'_>) -> Option<StatusView> {
        self.entries.get(session_id).map(|e| e.view(audience))
    }

    /// The workspace a tracked session belongs to.
    pub fn workspace_of(&self, session_id: &str) -> Option<&str> {
        self.entries
            .get(session_id)
            .map(|e| e.workspace_id.as_str())
    }

    /// A workspace's rollup for `audience` (`None` without agent sessions).
    pub fn workspace_status(
        &self,
        workspace_id: &str,
        audience: Audience<'_>,
    ) -> Option<AgentStatus> {
        rollup(
            self.entries
                .values()
                .filter(|e| e.workspace_id == workspace_id)
                .map(|e| e.view(audience).status),
        )
    }

    /// Everything, for `audience`.
    pub fn snapshot(&self, audience: Audience<'_>) -> Snapshot {
        let mut snapshot = Snapshot::default();
        for (id, entry) in &self.entries {
            let view = entry.view(audience);
            let rolled = snapshot
                .workspaces
                .entry(entry.workspace_id.clone())
                .or_insert(view.status);
            if view.status.urgency() > rolled.urgency() {
                *rolled = view.status;
            }
            snapshot.sessions.insert(id.clone(), view);
        }
        snapshot
    }
}

/// Trim, flatten whitespace, and bound a detail string.
fn bounded_detail(raw: &str) -> String {
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_DETAIL_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(MAX_DETAIL_CHARS - 1).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_with(session: &str, workspace: &str) -> (StatusBoard, u64) {
        let mut board = StatusBoard::default();
        let key = board.track(session, workspace).unwrap();
        (board, key.generation)
    }

    fn status(board: &StatusBoard, session: &str, client: &str) -> AgentStatus {
        board
            .status(session, Audience::Client(client))
            .unwrap()
            .status
    }

    #[test]
    fn reported_signals_drive_the_claude_cycle() {
        let (mut board, g) = board_with("s", "w");
        assert_eq!(status(&board, "s", "a"), AgentStatus::Unknown);
        assert!(board.apply("s", g, Signal::Ready, None));
        assert_eq!(status(&board, "s", "a"), AgentStatus::Idle);
        assert!(board.apply("s", g, Signal::Working, None));
        // A repeat is not a change.
        assert!(!board.apply("s", g, Signal::Working, None));
        assert!(board.apply(
            "s",
            g,
            Signal::Blocked,
            Some("  Claude needs\n your permission ")
        ));
        let view = board.status("s", Audience::Anyone).unwrap();
        assert_eq!(view.status, AgentStatus::Blocked);
        assert_eq!(view.detail.as_deref(), Some("Claude needs your permission"));
        assert!(board.apply("s", g, Signal::Working, None));
        assert_eq!(board.status("s", Audience::Anyone).unwrap().detail, None);
        assert!(board.apply("s", g, Signal::Done, None));
        assert_eq!(status(&board, "s", "a"), AgentStatus::Done);
        assert!(board.apply("s", g, Signal::Ended, None));
        assert_eq!(status(&board, "s", "a"), AgentStatus::Unknown);
    }

    #[test]
    fn awaiting_input_blocks_only_a_working_agent() {
        let (mut board, g) = board_with("s", "w");
        board.apply("s", g, Signal::Done, None);
        assert!(!board.apply("s", g, Signal::AwaitingInput, None));
        assert_eq!(status(&board, "s", "a"), AgentStatus::Done);
        board.apply("s", g, Signal::Working, None);
        assert!(board.apply("s", g, Signal::AwaitingInput, Some("question")));
        assert_eq!(status(&board, "s", "a"), AgentStatus::Blocked);
    }

    #[test]
    fn the_screen_sets_status_only_on_the_spell_it_read() {
        let (mut board, g) = board_with("s", "w");
        let rule = || StatusSource::Manifest {
            file: "codex.toml".into(),
            rule: "approval".into(),
        };
        assert_eq!(
            board.raw_status("s", g),
            Some((AgentStatus::Unknown, 0, true))
        );
        assert_eq!(board.raw_status("s", g + 1), None);
        // A stale spell or spawn, the same status, or done: refused.
        assert!(!board.apply_screen("s", g, 1, AgentStatus::Blocked, None, rule()));
        assert!(!board.apply_screen("s", g + 1, 0, AgentStatus::Blocked, None, rule()));
        assert!(!board.apply_screen("s", g, 0, AgentStatus::Unknown, None, rule()));
        assert!(!board.apply_screen("s", g, 0, AgentStatus::Done, None, rule()));
        assert!(board.apply_screen("s", g, 0, AgentStatus::Blocked, Some("run  ls?"), rule()));
        assert_eq!(
            board.raw_status("s", g),
            Some((AgentStatus::Blocked, 1, false))
        );
        let view = board.status("s", Audience::Anyone).unwrap();
        assert_eq!(view.detail.as_deref(), Some("run ls?"));
        assert_eq!(
            board.source("s").unwrap().label(),
            "manifest:codex.toml#approval"
        );
        // A report wins over a look taken before it.
        let hook = StatusSource::Hook {
            agent: "codex".into(),
            event: "notify".into(),
        };
        assert!(board.apply_report("s", g, Signal::Done, None, hook.clone()));
        assert!(!board.apply_screen("s", g, 1, AgentStatus::Idle, None, rule()));
        assert_eq!(board.source("s"), Some(&hook));
        assert_eq!(board.raw_status("s", g), Some((AgentStatus::Done, 2, true)));
        assert!(board.apply_screen("s", g, 2, AgentStatus::Working, None, rule()));
        assert_eq!(board.status("s", Audience::Anyone).unwrap().detail, None);
    }

    #[test]
    fn done_becomes_idle_per_client_on_view() {
        let (mut board, g) = board_with("s", "w");
        board.track("other", "w").unwrap();
        let laptop = board.connect("laptop");
        let phone = board.connect("phone");
        board.apply("s", g, Signal::Working, None);
        board.apply("s", g, Signal::Done, None);
        assert_eq!(status(&board, "s", "laptop"), AgentStatus::Done);
        assert_eq!(
            board.status("s", Audience::Anyone).unwrap().status,
            AgentStatus::Done
        );
        // Viewing something else does not count.
        assert!(!board.view(laptop, Some("other")));
        assert!(board.view(laptop, Some("s")));
        assert_eq!(status(&board, "s", "laptop"), AgentStatus::Idle);
        assert_eq!(status(&board, "s", "phone"), AgentStatus::Done);
        assert_eq!(
            board.status("s", Audience::Anyone).unwrap().status,
            AgentStatus::Idle
        );
        // A turn that finishes while the laptop looks is idle for it at once.
        board.apply("s", g, Signal::Working, None);
        board.apply("s", g, Signal::Done, None);
        assert_eq!(status(&board, "s", "laptop"), AgentStatus::Idle);
        assert_eq!(status(&board, "s", "phone"), AgentStatus::Done);
        // Once the laptop looks away, the next finished turn is unseen.
        board.view(laptop, None);
        board.apply("s", g, Signal::Working, None);
        board.apply("s", g, Signal::Done, None);
        assert_eq!(status(&board, "s", "laptop"), AgentStatus::Done);
        board.disconnect(phone);
        board.disconnect(laptop);
        assert!(!board.view(laptop, Some("s")));
    }

    #[test]
    fn rollups_pick_the_most_urgent_session() {
        let mut board = StatusBoard::default();
        let a = board.track("a", "w1").unwrap().generation;
        let b = board.track("b", "w1").unwrap().generation;
        let c = board.track("c", "w2").unwrap().generation;
        board.apply("a", a, Signal::Done, None);
        board.apply("b", b, Signal::Ready, None);
        board.apply("c", c, Signal::Working, None);
        let anyone = Audience::Anyone;
        assert_eq!(
            board.workspace_status("w1", anyone),
            Some(AgentStatus::Done)
        );
        assert_eq!(
            board.workspace_status("w2", anyone),
            Some(AgentStatus::Working)
        );
        assert_eq!(board.workspace_status("w3", anyone), None);
        board.apply("b", b, Signal::Blocked, None);
        let snap = board.snapshot(anyone);
        assert_eq!(snap.workspaces["w1"], AgentStatus::Blocked);
        assert_eq!(snap.workspaces["w2"], AgentStatus::Working);
        assert_eq!(snap.sessions.len(), 3);
        assert_eq!(
            rollup([AgentStatus::Idle, AgentStatus::Unknown, AgentStatus::Done]),
            Some(AgentStatus::Done)
        );
        assert_eq!(rollup([]), None);
        board.untrack("b");
        assert_eq!(
            board.workspace_status("w1", anyone),
            Some(AgentStatus::Done)
        );
    }

    #[test]
    fn a_relaunch_ignores_the_old_spawn_and_rotates_the_secret() {
        let mut board = StatusBoard::default();
        let old = board.track("s", "w").unwrap();
        assert_eq!(
            board.authenticate("s", old.secret.expose()),
            Some(old.generation)
        );
        assert_eq!(board.authenticate("s", "nope"), None);
        assert_eq!(board.authenticate("missing", old.secret.expose()), None);
        let new = board.track("s", "w").unwrap();
        assert_ne!(old.secret.expose(), new.secret.expose());
        assert_eq!(board.authenticate("s", old.secret.expose()), None);
        assert!(!board.apply("s", old.generation, Signal::Working, None));
        assert!(board.apply("s", new.generation, Signal::Working, None));
    }

    #[test]
    fn details_are_bounded() {
        let long = "x".repeat(500);
        let out = bounded_detail(&long);
        assert_eq!(out.chars().count(), MAX_DETAIL_CHARS);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn alerts_carry_the_raw_status_spell_and_who_is_looking() {
        let (mut board, generation) = board_with("s1", "w1");
        assert!(board.alert("nope").is_none());
        board.apply("s1", generation, Signal::Working, None);
        board.apply(
            "s1",
            generation,
            Signal::Blocked,
            Some("permission to use Bash"),
        );
        let blocked = board.alert("s1").unwrap();
        assert_eq!(blocked.status, AgentStatus::Blocked);
        assert_eq!(blocked.detail.as_deref(), Some("permission to use Bash"));
        assert!(!blocked.watched && !blocked.seen);
        let viewer = board.connect("phone");
        board.view(viewer, Some("s1"));
        assert!(board.alert("s1").unwrap().watched);
        // A turn that finishes in front of a viewer is seen at once.
        board.apply("s1", generation, Signal::Done, None);
        let done = board.alert("s1").unwrap();
        assert_eq!(done.status, AgentStatus::Done);
        assert!(done.watched && done.seen);
        assert!(done.spell > blocked.spell);
        board.view(viewer, None);
        let away = board.alert("s1").unwrap();
        assert!(!away.watched && away.seen, "{away:?}");
    }

    #[test]
    fn every_pane_on_screen_counts_as_viewed() {
        let mut board = StatusBoard::default();
        let g1 = board.track("a", "w").unwrap().generation;
        let g2 = board.track("b", "w").unwrap().generation;
        for (id, g) in [("a", g1), ("b", g2)] {
            board.apply(id, g, Signal::Working, None);
            board.apply(id, g, Signal::Done, None);
        }
        let laptop = board.connect("laptop");
        // Focused pane plus a visible one; unknown ids and repeats drop.
        let mut seen = board.view_many(laptop, &["a", "b", "nope", "a"]);
        seen.sort();
        assert_eq!(seen, ["a", "b"]);
        assert_eq!(board.viewing(laptop), ["a", "b"]);
        assert!(board.is_watched("a") && board.is_watched("b"));
        assert_eq!(status(&board, "b", "laptop"), AgentStatus::Idle);
        // A turn finishing in either pane is seen at once.
        board.apply("b", g2, Signal::Working, None);
        board.apply("b", g2, Signal::Done, None);
        assert_eq!(status(&board, "b", "laptop"), AgentStatus::Idle);
        board.untrack("b");
        assert_eq!(board.viewing(laptop), ["a"]);
        assert_eq!(board.disconnect(laptop), ["a"]);
        assert!(!board.is_watched("a"));
    }
}
