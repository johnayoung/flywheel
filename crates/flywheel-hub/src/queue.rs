//! Prompt queue: per agent session, a FIFO of prompts the hub types in on
//! its own -- "when it's done, then do X" from a phone.
//!
//! Each item names a trigger:
//!
//! * `done` (default) -- the agent is at rest: its turn finished (seen or
//!   not) or it waits for its first prompt.
//! * `idle` -- at rest *and* someone has looked at the finished turn (a
//!   `done` some client viewed): "not before I have read its answer".
//! * `blocked-cleared` -- the agent blocked on a prompt (a permission
//!   dialog, a question) after the item reached the front, the block was
//!   answered, and the turn then finished.
//!
//! Whatever the trigger, the head item is delivered only while every one
//! of these holds:
//!
//! * the session runs its agent (restored and resumed, not a fallback
//!   shell), and the agent is not `working`, `blocked`, or `unknown`;
//! * its rest began after the previous delivery (a delivered prompt must
//!   start a turn and finish it before the next goes) and at least
//!   [`SETTLE`] ago;
//! * nobody typed into the session for `queue.typing_quiet` (input from a
//!   browser, `fw hub attach`, `fw hub send`, or Compose);
//! * no fan-out grading is queued or running on it, and no review action
//!   (commit, push, merge, ask for a review, send findings) is running in
//!   its directory.
//!
//! Delivery is the same path as every paste (bracketed paste, then Enter).
//! It is exactly-once: the item is recorded `sending` in `state.json`
//! before the paste and `delivered` (with its time) after; a hub that died
//! in between finds `sending` on its next start and looks in the agent's
//! transcript: a user message carrying the text since then means it went
//! (marked delivered, never typed again), else it is sent again. A live
//! handoff (`fw hub upgrade`) waits for a delivery in progress and starts
//! none until the new hub has taken over.
//!
//! A failure -- the agent exited, the session was not restored, the paste
//! was refused, a delivered prompt never started a turn -- stalls the
//! queue (nothing more is sent) until it is resumed; stalled queues show
//! in Needs you and announce `queue.stalled` to plugins. Each delivery
//! announces `queue.delivered`.
//!
//! The queue lives on the hub that owns the session; other hubs reach it
//! through their `/m/<machine>` proxy like every session call.

use crate::agentlog::{self, Event, Role};
use crate::hub::{Hub, HubError, HubEvent};
use crate::pty::ExitInfo;
use crate::state::{SessionRecord, SessionState, StateError, new_id};
use crate::status::{AgentStatus, StatusView};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Pending items per session.
pub const MAX_PENDING: usize = 50;
/// Delivered items kept per session (oldest dropped first).
pub const MAX_DELIVERED_KEPT: usize = 20;
/// Largest prompt queued.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Default `queue.typing_quiet`, and its bounds.
pub const DEFAULT_TYPING_QUIET: Duration = Duration::from_secs(8);
pub const MIN_TYPING_QUIET: Duration = Duration::from_secs(1);
pub const MAX_TYPING_QUIET: Duration = Duration::from_secs(600);
/// How long an agent must have been at rest before an item goes (a
/// fan-out queues its grading on the finished turn in that time; hooks
/// that fire right after `Stop` land).
pub const SETTLE: Duration = Duration::from_millis(1500);
/// A delivered prompt that has not started a turn after this long stalls
/// the queue (the agent did not take it).
pub const ACK_TIMEOUT: Duration = Duration::from_secs(120);
/// How often the hub looks at its queues.
const TICK: Duration = Duration::from_millis(250);
/// Characters of an item's text in a plugin event.
const EVENT_TEXT_CHARS: usize = 2000;
/// Transcript lines stamped this long before an item went `sending` may
/// still be its delivery (clock steps, the agent's own stamping).
const RECONCILE_SKEW: chrono::Duration = chrono::Duration::seconds(5);

/// When an item may be delivered (see the module doc).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum When {
    #[default]
    Done,
    Idle,
    BlockedCleared,
}

impl When {
    pub const ALL: [Self; 3] = [Self::Done, Self::Idle, Self::BlockedCleared];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Idle => "idle",
            Self::BlockedCleared => "blocked-cleared",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL
            .into_iter()
            .find(|w| w.as_str() == raw)
            .ok_or_else(|| format!("unknown trigger {raw:?}: done, idle, or blocked-cleared"))
    }
}

/// Where an item is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ItemState {
    Pending,
    /// The paste is (or was, when the hub died) under way.
    Sending {
        sending_at: DateTime<Utc>,
    },
    Delivered {
        delivered_at: DateTime<Utc>,
        /// Found in the agent's transcript on a start after the hub died
        /// mid-delivery (not typed again).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reconciled: bool,
    },
}

/// One queued prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub when: When,
    pub added_at: DateTime<Utc>,
    /// When the agent was seen blocked while this item was at the front
    /// (a `blocked-cleared` item waits for the rest after it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub state: ItemState,
}

impl QueueItem {
    pub fn is_delivered(&self) -> bool {
        matches!(self.state, ItemState::Delivered { .. })
    }
}

/// Why a queue stopped delivering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stall {
    pub at: DateTime<Utc>,
    pub reason: String,
    /// The item that was next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
}

/// A session's queue as stored: delivered items (oldest first), then the
/// one being sent, then the pending ones in delivery order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionQueue {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled: Option<Stall>,
    /// The latest delivery (the next item waits for a rest after it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivered_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub items: Vec<QueueItem>,
}

impl SessionQueue {
    /// Nothing worth keeping (the store drops it).
    pub fn is_vacant(&self) -> bool {
        self.items.is_empty()
            && !self.paused
            && self.stalled.is_none()
            && self.last_delivered_at.is_none()
    }

    /// Items not yet delivered (the one being sent first, if any).
    pub fn waiting(&self) -> impl Iterator<Item = &QueueItem> {
        self.items.iter().filter(|i| !i.is_delivered())
    }

    pub fn pending_count(&self) -> usize {
        self.waiting().count()
    }

    /// The next item to go (or the one going).
    pub fn head(&self) -> Option<&QueueItem> {
        self.waiting().next()
    }

    fn head_mut(&mut self) -> Option<&mut QueueItem> {
        self.items.iter_mut().find(|i| !i.is_delivered())
    }

    fn item_mut(&mut self, id: &str) -> Option<&mut QueueItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// Keep the newest [`MAX_DELIVERED_KEPT`] delivered items.
    fn trim_history(&mut self) {
        let delivered = self.items.iter().filter(|i| i.is_delivered()).count();
        let mut drop = delivered.saturating_sub(MAX_DELIVERED_KEPT);
        self.items.retain(|i| {
            if drop > 0 && i.is_delivered() {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }

    /// Put delivered items first (their delivery order), the rest after
    /// in their order: the invariant every edit keeps.
    fn settle_order(&mut self) {
        let (mut delivered, waiting): (Vec<QueueItem>, Vec<QueueItem>) =
            std::mem::take(&mut self.items)
                .into_iter()
                .partition(QueueItem::is_delivered);
        delivered.sort_by_key(|i| match i.state {
            ItemState::Delivered { delivered_at, .. } => delivered_at,
            _ => i.added_at,
        });
        delivered.extend(waiting);
        self.items = delivered;
    }
}

/// A queue's summary on its session (`fw hub ls`, the sidebar, Needs you).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueBrief {
    /// Items not yet delivered.
    pub pending: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled: Option<Stall>,
    /// The next item's trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_when: Option<When>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_delivered_at: Option<DateTime<Utc>>,
}

/// One item as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct ItemView {
    #[serde(flatten)]
    pub item: QueueItem,
    /// 1-based place among the items not yet delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
}

/// A session's queue as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct QueueView {
    pub session_id: String,
    pub session: String,
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled: Option<Stall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_delivered_at: Option<DateTime<Utc>>,
    pub pending: usize,
    pub items: Vec<ItemView>,
    /// Why the next item is not being sent now (`None`: nothing queued,
    /// or it goes at once).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
    /// `queue.typing_quiet` in force, in seconds.
    pub typing_quiet_secs: u64,
}

/// An item to add.
#[derive(Debug, Clone, Deserialize)]
pub struct NewItem {
    pub text: String,
    #[serde(default)]
    pub when: Option<String>,
}

/// An edit of a pending item.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemEdit {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub when: Option<String>,
}

/// Parse `queue.typing_quiet`: seconds, or `8s`, `1m`, `1m30s`.
pub fn parse_quiet(raw: &str) -> Result<Duration, String> {
    let text = raw.trim().to_ascii_lowercase();
    let secs = if let Ok(n) = text.parse::<u64>() {
        n
    } else {
        let mut total: u64 = 0;
        let mut digits = String::new();
        for c in text.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
                continue;
            }
            let unit = match c {
                's' => 1,
                'm' => 60,
                _ => return Err(format!("bad duration {raw:?}: like 8s, 30s, or 2m")),
            };
            let n: u64 = digits
                .parse()
                .map_err(|_| format!("bad duration {raw:?}: a number comes before each unit"))?;
            digits.clear();
            total = total.saturating_add(n.saturating_mul(unit));
        }
        if !digits.is_empty() {
            return Err(format!("bad duration {raw:?}: the last number has no unit"));
        }
        total
    };
    let quiet = Duration::from_secs(secs);
    if quiet < MIN_TYPING_QUIET || quiet > MAX_TYPING_QUIET {
        return Err(format!(
            "{} is outside {}s..={}s",
            crate::share::format_ttl(quiet),
            MIN_TYPING_QUIET.as_secs(),
            MAX_TYPING_QUIET.as_secs()
        ));
    }
    Ok(quiet)
}

/// A prompt as queued: trailing whitespace dropped, not empty, bounded.
pub fn validate_text(raw: &str) -> Result<String, StateError> {
    let text = raw.trim_end_matches([' ', '\t', '\n', '\r']);
    if text.trim().is_empty() {
        return Err(StateError::Invalid(
            "nothing to queue: the prompt is empty".into(),
        ));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(StateError::Invalid(format!(
            "the prompt is {} bytes; at most {MAX_TEXT_BYTES} are queued",
            text.len()
        )));
    }
    Ok(text.to_owned())
}

fn parse_when(raw: Option<&str>) -> Result<When, StateError> {
    raw.filter(|w| !w.trim().is_empty())
        .map_or(Ok(When::Done), When::parse)
        .map_err(StateError::Invalid)
}

// --- the runtime ---------------------------------------------------------------

/// What a queue delivery may not overlap: a handoff or shutdown.
#[derive(Debug, Default)]
struct Gate {
    /// No delivery starts (a live handoff froze the hub).
    paused: bool,
    /// No delivery starts, and the engine ends (the hub shuts down).
    stopped: bool,
    /// One is under way.
    delivering: bool,
}

/// Who holds sessions: review actions running on them.
#[derive(Debug, Default)]
struct Holds {
    next: u64,
    held: HashMap<u64, (Vec<String>, String)>,
}

/// The queues' in-memory side: typing times, holds, and the delivery gate.
#[derive(Debug, Default)]
pub struct QueueRuntime {
    /// Session id -> when input was last typed into it.
    typing: Mutex<HashMap<String, Instant>>,
    holds: Mutex<Holds>,
    gate: Mutex<Gate>,
    gate_changed: Condvar,
    started: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl QueueRuntime {
    /// Start a delivery unless the hub is frozen or stopping.
    fn begin_delivery(&self) -> bool {
        let mut gate = lock(&self.gate);
        if gate.paused || gate.stopped {
            return false;
        }
        gate.delivering = true;
        true
    }

    fn end_delivery(&self) {
        lock(&self.gate).delivering = false;
        self.gate_changed.notify_all();
    }

    fn wait_idle(&self, mut gate: MutexGuard<'_, Gate>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while gate.delivering {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            gate = self
                .gate_changed
                .wait_timeout(gate, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Start no delivery from now on, and wait up to `timeout` for the one
    /// under way (false: it is still going).
    pub(crate) fn pause(&self, timeout: Duration) -> bool {
        let mut gate = lock(&self.gate);
        gate.paused = true;
        self.wait_idle(gate, timeout)
    }

    /// Deliver again after [`QueueRuntime::pause`].
    pub(crate) fn resume(&self) {
        lock(&self.gate).paused = false;
    }

    /// Stop for good (shutdown), waiting up to `timeout` for a delivery.
    pub(crate) fn stop(&self, timeout: Duration) {
        let mut gate = lock(&self.gate);
        gate.stopped = true;
        self.wait_idle(gate, timeout);
    }

    fn is_stopped(&self) -> bool {
        lock(&self.gate).stopped
    }

    fn is_paused(&self) -> bool {
        let gate = lock(&self.gate);
        gate.paused || gate.stopped
    }

    fn typed_ago(&self, id: &str) -> Option<Duration> {
        lock(&self.typing).get(id).map(Instant::elapsed)
    }

    fn holder(&self, id: &str) -> Option<String> {
        lock(&self.holds)
            .held
            .values()
            .find(|(ids, _)| ids.iter().any(|i| i == id))
            .map(|(_, what)| what.clone())
    }
}

/// Sessions held by an action until this drops (see [`Hub::hold_sessions`]).
#[must_use = "the sessions are held only while this lives"]
pub struct SessionHold<'a> {
    runtime: &'a QueueRuntime,
    key: u64,
}

impl Drop for SessionHold<'_> {
    fn drop(&mut self) {
        lock(&self.runtime.holds).held.remove(&self.key);
    }
}

/// Ends a delivery however it returns.
struct Delivering<'a>(&'a QueueRuntime);

impl Drop for Delivering<'_> {
    fn drop(&mut self) {
        self.0.end_delivery();
    }
}

// --- deciding ------------------------------------------------------------------

/// Whether the session can take a prompt at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Presence {
    /// Its agent runs.
    Ready,
    /// It will (a restore or a launch is under way): wait.
    Starting(&'static str),
    /// It will not without someone acting: stall.
    Gone(String),
}

/// What the head item is decided on.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    pub now: DateTime<Utc>,
    pub presence: Presence,
    /// Its agent status, a finished turn counted as seen once any client
    /// viewed it.
    pub status: Option<StatusView>,
    /// Since input was last typed into it.
    pub typed_ago: Option<Duration>,
    pub quiet: Duration,
    /// What holds it now (a review action, a fan-out grading).
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Deliver(String),
    Wait(String),
    Stall(String),
    /// The agent is blocked with this `blocked-cleared` item at the front:
    /// record when (then wait).
    NoteBlocked {
        item: String,
        at: DateTime<Utc>,
    },
}

fn secs(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_millis() >= 500)
}

/// What to do about `queue`'s head item now. Pure.
pub(crate) fn decide(queue: &SessionQueue, facts: &Facts) -> Decision {
    let wait = |why: &str| Decision::Wait(why.to_owned());
    if let Some(stall) = &queue.stalled {
        return Decision::Wait(format!("stalled: {}", stall.reason));
    }
    if queue.paused {
        return wait("paused");
    }
    let Some(head) = queue.head() else {
        return wait("nothing queued");
    };
    if matches!(head.state, ItemState::Sending { .. }) {
        return wait("being delivered");
    }
    match &facts.presence {
        Presence::Gone(why) => return Decision::Stall(why.clone()),
        Presence::Starting(why) => return wait(why),
        Presence::Ready => {}
    }
    let Some(status) = &facts.status else {
        return wait("waiting for the agent to report a status");
    };
    match status.status {
        AgentStatus::Blocked => {
            if head.when == When::BlockedCleared && head.blocked_at.is_none() {
                return Decision::NoteBlocked {
                    item: head.id.clone(),
                    at: status.since,
                };
            }
            return wait("blocked on a prompt: never sent until it is answered");
        }
        AgentStatus::Working => return wait("working: waiting for its turn to finish"),
        AgentStatus::Unknown => return wait("waiting for the agent to report a status"),
        AgentStatus::Done | AgentStatus::Idle => {}
    }
    if let Some(last) = queue.last_delivered_at
        && status.since <= last
    {
        let ago = (facts.now - last).to_std().unwrap_or_default();
        if ago >= ACK_TIMEOUT {
            return Decision::Stall(format!(
                "the agent did not start a turn in the {}s after the last delivered prompt",
                ACK_TIMEOUT.as_secs()
            ));
        }
        return wait("waiting for the agent to take up the last delivered prompt");
    }
    if head.when == When::Idle && status.status != AgentStatus::Idle {
        return wait("finished: waiting until someone has looked at its answer (idle)");
    }
    if head.when == When::BlockedCleared {
        match head.blocked_at {
            None => return wait("waiting for the agent to block on a prompt (blocked-cleared)"),
            Some(at) if status.since <= at => {
                return wait("waiting for the turn after the block to finish (blocked-cleared)");
            }
            Some(_) => {}
        }
    }
    let rested = (facts.now - status.since).to_std().unwrap_or_default();
    if rested < SETTLE {
        return wait("just finished: settling");
    }
    if let Some(owner) = &facts.owner {
        return Decision::Wait(format!("{owner} is using the session"));
    }
    if let Some(ago) = facts.typed_ago
        && ago < facts.quiet
    {
        return Decision::Wait(format!(
            "someone typed {}s ago: waiting for {}s without typing",
            secs(ago),
            facts.quiet.as_secs()
        ));
    }
    Decision::Deliver(head.id.clone())
}

fn exit_text(exit: &ExitInfo) -> String {
    match (&exit.signal, exit.code) {
        (Some(signal), _) => format!("killed by {signal}"),
        (None, Some(code)) => format!("exit code {code}"),
        (None, None) => "exit code unknown".to_owned(),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}

/// Text compared for a reconcile: whitespace runs as one space.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --- the hub's side ------------------------------------------------------------

impl Hub {
    /// Record that input was typed into session `id` (a browser, `fw hub
    /// attach`, `fw hub send`, Compose). A lone focus report is not typing.
    pub fn note_typing(&self, id: &str, bytes: &[u8]) {
        if bytes.is_empty() || bytes == b"\x1b[I" || bytes == b"\x1b[O" {
            return;
        }
        lock(&self.queue_runtime().typing).insert(id.to_owned(), Instant::now());
    }

    /// Hold sessions `ids` for `what` (a review action) until the guard
    /// drops: their queues deliver nothing meanwhile.
    pub fn hold_sessions(&self, ids: Vec<String>, what: &str) -> SessionHold<'_> {
        let runtime = self.queue_runtime();
        let mut holds = lock(&runtime.holds);
        holds.next += 1;
        let key = holds.next;
        holds.held.insert(key, (ids, what.to_owned()));
        SessionHold { runtime, key }
    }

    /// Hold every agent session working in `dir` (or under it).
    pub fn hold_dir(&self, dir: &std::path::Path, what: &str) -> SessionHold<'_> {
        let ids = self
            .store()
            .sessions()
            .iter()
            .filter(|s| s.kind.is_agent() && s.cwd.starts_with(dir))
            .map(|s| s.id.clone())
            .collect();
        self.hold_sessions(ids, what)
    }

    /// A session's queue summary (`None`: nothing queued, delivered, or
    /// stalled).
    pub(crate) fn queue_brief(&self, id: &str) -> Option<QueueBrief> {
        let store = self.store();
        let queue = store.queue(id)?;
        Some(QueueBrief {
            pending: queue.pending_count(),
            paused: queue.paused,
            stalled: queue.stalled.clone(),
            next_when: queue.head().map(|i| i.when),
            last_delivered_at: queue.last_delivered_at,
        })
    }

    fn queue_record(&self, id: &str) -> Result<SessionRecord, HubError> {
        let record = self.store().session(id)?.clone();
        if !record.kind.is_agent() {
            return Err(StateError::Invalid(format!(
                "{} is a {} session: prompts queue for Claude Code and Codex sessions (fw hub send types into any)",
                record.label(),
                record.kind.as_str()
            ))
            .into());
        }
        Ok(record)
    }

    /// Whether the session is one of a fan-out's candidates with a grading
    /// queued or running.
    fn grading_owns(&self, id: &str) -> bool {
        self.store().fanouts().iter().any(|f| {
            f.candidates
                .iter()
                .any(|c| c.session_id == id && c.pending.is_some())
        })
    }

    fn presence(&self, record: &SessionRecord) -> Presence {
        match &record.state {
            SessionState::Running => match self.live_session(&record.id) {
                Some(pty) => match pty.exit_info() {
                    Some(exit) => {
                        Presence::Gone(format!("its agent exited ({})", exit_text(&exit)))
                    }
                    None if record.runs_agent() => Presence::Ready,
                    None => Presence::Gone(format!(
                        "{} came back as a plain shell after a restart: relaunch it, then resume the queue",
                        record.kind.as_str()
                    )),
                },
                None => Presence::Starting("its terminal is not up yet"),
            },
            SessionState::Exited { exit, .. } => Presence::Gone(format!(
                "its agent exited ({}): relaunch it, then resume the queue",
                exit_text(exit)
            )),
            SessionState::Stopped {
                clean: true,
                restore_error: None,
                ..
            } => Presence::Starting("waiting for the hub to restore it"),
            SessionState::Stopped {
                restore_error: Some(why),
                ..
            } => Presence::Gone(format!("it could not be restored: {why}")),
            SessionState::Stopped { .. } => Presence::Gone(
                "it is stopped (not restored): relaunch it, then resume the queue".into(),
            ),
        }
    }

    fn queue_facts(&self, record: &SessionRecord) -> Facts {
        let runtime = self.queue_runtime();
        let owner = runtime.holder(&record.id).or_else(|| {
            self.grading_owns(&record.id)
                .then(|| "a fan-out grading".to_owned())
        });
        Facts {
            now: Utc::now(),
            presence: self.presence(record),
            status: if record.runs_agent() {
                self.agent_status(&record.id)
            } else {
                None
            },
            typed_ago: runtime.typed_ago(&record.id),
            quiet: self.tunables().queue_typing_quiet,
            owner,
        }
    }

    /// Session `id`'s queue.
    pub fn queue_view(&self, id: &str) -> Result<QueueView, HubError> {
        let record = self.queue_record(id)?;
        let queue = self.store().queue(id).cloned().unwrap_or_default();
        let waiting = if queue.head().is_none() {
            None
        } else {
            match decide(&queue, &self.queue_facts(&record)) {
                Decision::Wait(why) => Some(why),
                Decision::Stall(why) => Some(format!("about to stall: {why}")),
                Decision::NoteBlocked { .. } => {
                    Some("blocked on a prompt: never sent until it is answered".into())
                }
                Decision::Deliver(_) => Some("ready: it goes within a moment".into()),
            }
        };
        let mut position = 0;
        let items = queue
            .items
            .iter()
            .map(|item| ItemView {
                position: (!item.is_delivered()).then(|| {
                    position += 1;
                    position
                }),
                item: item.clone(),
            })
            .collect();
        Ok(QueueView {
            session_id: record.id.clone(),
            session: record.name.clone(),
            paused: queue.paused,
            stalled: queue.stalled.clone(),
            last_delivered_at: queue.last_delivered_at,
            pending: queue.pending_count(),
            items,
            waiting,
            typing_quiet_secs: self.tunables().queue_typing_quiet.as_secs(),
        })
    }

    /// Every session with a queue.
    pub fn queue_views(&self) -> Vec<QueueView> {
        let ids: Vec<String> = self.store().queues().keys().cloned().collect();
        ids.iter()
            .filter_map(|id| self.queue_view(id).ok())
            .collect()
    }

    fn queue_changed(&self) {
        self.emit(HubEvent::Sessions);
    }

    /// Queue `item` at the end of session `id`'s queue.
    pub fn queue_add(&self, id: &str, item: &NewItem) -> Result<(QueueItem, QueueView), HubError> {
        let record = self.queue_record(id)?;
        let text = validate_text(&item.text)?;
        let when = parse_when(item.when.as_deref())?;
        let new = QueueItem {
            id: new_id(),
            text,
            when,
            added_at: Utc::now(),
            blocked_at: None,
            state: ItemState::Pending,
        };
        let added = new.clone();
        self.store().update_queue(id, move |queue| {
            if queue.pending_count() >= MAX_PENDING {
                return Err(StateError::Conflict(format!(
                    "{} has {MAX_PENDING} prompts queued already: remove some first",
                    record.label()
                )));
            }
            queue.items.push(new);
            Ok(())
        })??;
        eprintln!(
            "fw hub: queue {id}: added {} ({}, {} bytes)",
            &added.id[..8],
            added.when.as_str(),
            added.text.len()
        );
        self.queue_changed();
        Ok((added, self.queue_view(id)?))
    }

    /// Resolve an item reference: an id or unique id prefix (4+ chars), or
    /// a 1-based position among the pending items.
    fn queue_item_id(queue: &SessionQueue, reference: &str) -> Result<String, StateError> {
        let reference = reference.trim().trim_start_matches('#');
        if !reference.is_empty()
            && reference.len() <= 3
            && reference.chars().all(|c| c.is_ascii_digit())
        {
            let n: usize = reference.parse().unwrap_or(0);
            return queue
                .waiting()
                .nth(n.wrapping_sub(1))
                .map(|i| i.id.clone())
                .ok_or_else(|| {
                    StateError::NotFound(format!(
                        "queue position {n} ({} pending)",
                        queue.pending_count()
                    ))
                });
        }
        if reference.len() < 4 {
            return Err(StateError::Invalid(
                "name an item by its id (4+ characters) or its position".into(),
            ));
        }
        let found: Vec<&QueueItem> = queue
            .items
            .iter()
            .filter(|i| i.id.starts_with(reference))
            .collect();
        match found.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => Err(StateError::NotFound(format!("queue item {reference}"))),
            _ => Err(StateError::Invalid(format!(
                "queue item {reference} is ambiguous: give more of its id"
            ))),
        }
    }

    /// Edit pending item `reference`'s text and/or trigger.
    pub fn queue_edit(
        &self,
        id: &str,
        reference: &str,
        edit: &ItemEdit,
    ) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        let text = edit.text.as_deref().map(validate_text).transpose()?;
        let when = edit
            .when
            .as_deref()
            .map(|w| parse_when(Some(w)))
            .transpose()?;
        self.store().update_queue(id, |queue| {
            let item_id = Self::queue_item_id(queue, reference)?;
            let item = queue.item_mut(&item_id).expect("resolved");
            if !matches!(item.state, ItemState::Pending) {
                return Err(StateError::Conflict(
                    "that prompt was sent already: it cannot change".into(),
                ));
            }
            if let Some(text) = text {
                item.text = text;
            }
            if let Some(when) = when
                && when != item.when
            {
                item.when = when;
                item.blocked_at = None;
            }
            Ok(())
        })??;
        self.queue_changed();
        self.queue_view(id)
    }

    /// Remove item `reference` (pending or delivered; not one being sent).
    pub fn queue_remove(&self, id: &str, reference: &str) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        self.store().update_queue(id, |queue| {
            let item_id = Self::queue_item_id(queue, reference)?;
            if queue
                .items
                .iter()
                .any(|i| i.id == item_id && matches!(i.state, ItemState::Sending { .. }))
            {
                return Err(StateError::Conflict(
                    "that prompt is being delivered right now".into(),
                ));
            }
            queue.items.retain(|i| i.id != item_id);
            if queue
                .stalled
                .as_ref()
                .is_some_and(|s| s.item.as_deref() == Some(item_id.as_str()))
                && queue.head().is_none()
            {
                queue.stalled = None;
            }
            Ok(())
        })??;
        self.queue_changed();
        self.queue_view(id)
    }

    /// Remove every pending item (`all`: the delivered history too).
    pub fn queue_clear(&self, id: &str, all: bool) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        self.store().update_queue(id, |queue| {
            queue.items.retain(|i| match i.state {
                ItemState::Sending { .. } => true,
                ItemState::Delivered { .. } => !all,
                ItemState::Pending => false,
            });
            queue.stalled = None;
        })?;
        self.queue_changed();
        self.queue_view(id)
    }

    /// Move pending item `reference` to 1-based `position` among the
    /// pending items (clamped; never ahead of one being sent).
    pub fn queue_move(
        &self,
        id: &str,
        reference: &str,
        position: usize,
    ) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        self.store().update_queue(id, |queue| {
            let item_id = Self::queue_item_id(queue, reference)?;
            let Some(from) = queue.items.iter().position(|i| i.id == item_id) else {
                return Err(StateError::NotFound(format!("queue item {reference}")));
            };
            if !matches!(queue.items[from].state, ItemState::Pending) {
                return Err(StateError::Conflict(
                    "only prompts still waiting can move".into(),
                ));
            }
            let old_head = queue.head().map(|i| i.id.clone());
            let item = queue.items.remove(from);
            // Pending slots start after the delivered and sending items.
            let first = queue
                .items
                .iter()
                .position(|i| matches!(i.state, ItemState::Pending))
                .unwrap_or(queue.items.len());
            let pending = queue.items.len() - first;
            let at = first + position.saturating_sub(1).min(pending);
            queue.items.insert(at, item);
            queue.settle_order();
            // An item that lost the front: a block it saw no longer counts.
            if let Some(old) = old_head
                && queue.head().is_some_and(|h| h.id != old)
                && let Some(item) = queue.item_mut(&old)
            {
                item.blocked_at = None;
            }
            Ok(())
        })??;
        self.queue_changed();
        self.queue_view(id)
    }

    /// Pause (`true`) or resume session `id`'s queue. Resuming also
    /// clears a stall (the next item is tried again).
    pub fn queue_set_paused(&self, id: &str, paused: bool) -> Result<QueueView, HubError> {
        self.queue_record(id)?;
        self.store().update_queue(id, |queue| {
            queue.paused = paused;
            if !paused {
                queue.stalled = None;
            }
        })?;
        eprintln!(
            "fw hub: queue {id}: {}",
            if paused { "paused" } else { "resumed" }
        );
        self.queue_changed();
        self.queue_view(id)
    }

    /// The queue's start: settle items a dead hub left `sending`, then
    /// look at the queues every [`TICK`] until the hub stops. Once per
    /// hub; a live handoff's new hub starts it at commit.
    pub(crate) fn start_queues(self: &Arc<Self>) {
        if self.queue_runtime().started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.reconcile_queues();
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("hub-queue".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    let Some(hub) = weak.upgrade() else {
                        break;
                    };
                    if hub.queue_runtime().is_stopped() {
                        break;
                    }
                    if !hub.queue_runtime().is_paused() {
                        hub.queue_tick();
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("fw hub: prompt queues are off: cannot start their thread: {e}");
        }
    }

    /// Items left `sending` by a hub that stopped mid-delivery: delivered
    /// when the agent's transcript has the text as a user message from
    /// then on (never typed again), else back to pending (sent again).
    fn reconcile_queues(&self) {
        let sending: Vec<(String, String, String, DateTime<Utc>)> = self
            .store()
            .queues()
            .iter()
            .flat_map(|(sid, q)| {
                q.items.iter().filter_map(move |i| match i.state {
                    ItemState::Sending { sending_at } => {
                        Some((sid.clone(), i.id.clone(), i.text.clone(), sending_at))
                    }
                    _ => None,
                })
            })
            .collect();
        for (sid, item_id, text, sending_at) in sending {
            let record = self.store().session(&sid).ok().cloned();
            let found = record
                .as_ref()
                .is_some_and(|r| self.transcript_has(r, &text, sending_at));
            let result = self.store().update_queue(&sid, |queue| {
                if let Some(item) = queue.item_mut(&item_id) {
                    item.state = if found {
                        ItemState::Delivered {
                            delivered_at: sending_at,
                            reconciled: true,
                        }
                    } else {
                        ItemState::Pending
                    };
                }
                if found && queue.last_delivered_at.is_none_or(|t| t < sending_at) {
                    queue.last_delivered_at = Some(sending_at);
                }
                queue.settle_order();
            });
            eprintln!(
                "fw hub: queue {sid}: item {} was being sent when the hub stopped: {}{}",
                &item_id[..item_id.len().min(8)],
                if found {
                    "its transcript has it, so it is marked delivered (not sent again)"
                } else {
                    "its transcript does not have it, so it will be sent again"
                },
                match result {
                    Ok(()) => String::new(),
                    Err(e) => format!(" (not saved: {e})"),
                }
            );
        }
    }

    /// Whether `record`'s agent transcript holds a user message with
    /// `text` written from about `since` on.
    fn transcript_has(&self, record: &SessionRecord, text: &str, since: DateTime<Utc>) -> bool {
        let Some((path, format)) = self.command_book().transcript_file(record) else {
            return false;
        };
        let needle = normalized(text);
        let earliest = since - RECONCILE_SKEW;
        let mut tail = agentlog::JsonlTail::new(path);
        let mut found = false;
        let _ = tail.read(None, &mut |read| {
            let agentlog::Read::Line { bytes, start } = read else {
                return;
            };
            if found {
                return;
            }
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
                return;
            };
            let line = agentlog::parse(format, &value, start);
            if let Some(at) = line
                .timestamp
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                && at.with_timezone(&Utc) < earliest
            {
                return;
            }
            found = line.events.iter().any(|event| match event {
                Event::Text {
                    role: Role::User,
                    parts,
                    ..
                } => {
                    let joined: Vec<&str> = parts.iter().map(|p| p.text.as_str()).collect();
                    normalized(&joined.join("\n")).contains(&needle)
                }
                _ => false,
            });
        });
        found
    }

    /// One look at every queue with something to send.
    fn queue_tick(&self) {
        let candidates: Vec<(SessionQueue, SessionRecord)> = {
            let store = self.store();
            store
                .queues()
                .iter()
                .filter(|(_, q)| q.head().is_some() && !q.paused && q.stalled.is_none())
                .filter_map(|(id, q)| Some((q.clone(), store.session(id).ok()?.clone())))
                .collect()
        };
        for (queue, record) in candidates {
            let facts = self.queue_facts(&record);
            match decide(&queue, &facts) {
                Decision::Wait(_) => {}
                Decision::NoteBlocked { item, at } => {
                    let _ = self.store().update_queue(&record.id, |q| {
                        if let Some(i) = q.item_mut(&item) {
                            i.blocked_at.get_or_insert(at);
                        }
                    });
                }
                Decision::Stall(reason) => {
                    let item = queue.head().map(|i| i.id.clone());
                    self.stall_queue(&record.id, &reason, item.as_deref());
                }
                Decision::Deliver(item) => self.deliver(&record.id, &item),
            }
        }
    }

    fn stall_queue(&self, id: &str, reason: &str, item: Option<&str>) {
        let stall = Stall {
            at: Utc::now(),
            reason: reason.to_owned(),
            item: item.map(str::to_owned),
        };
        let pending = match self.store().update_queue(id, |queue| {
            queue.stalled = Some(stall.clone());
            queue.pending_count()
        }) {
            Ok(pending) => pending,
            Err(e) => {
                eprintln!("fw hub: queue {id}: stalling: {e}");
                return;
            }
        };
        eprintln!("fw hub: queue {id}: stalled: {reason}");
        self.queue_changed();
        if let Some(session) = self.event_session(id) {
            self.emit_lifecycle(
                "queue.stalled",
                serde_json::json!({
                    "session": session,
                    "reason": reason,
                    "item": item,
                    "pending": pending,
                }),
            );
        }
    }

    /// Deliver item `item_id` of session `id`: `sending` saved, the paste,
    /// then `delivered` saved (or the item back to pending and the queue
    /// stalled when the paste was refused).
    fn deliver(&self, id: &str, item_id: &str) {
        let runtime = self.queue_runtime();
        if !runtime.begin_delivery() {
            return;
        }
        let _delivering = Delivering(runtime);
        let now = Utc::now();
        let claimed = self.store().update_queue(id, |queue| {
            if queue.paused || queue.stalled.is_some() {
                return None;
            }
            let head = queue.head_mut()?;
            if head.id != item_id || !matches!(head.state, ItemState::Pending) {
                return None;
            }
            head.state = ItemState::Sending { sending_at: now };
            Some(head.clone())
        });
        let item = match claimed {
            Ok(Some(item)) => item,
            Ok(None) => return,
            Err(e) => {
                eprintln!("fw hub: queue {id}: not delivered: cannot record it: {e}");
                return;
            }
        };
        let pasted = self.paste(id, &item.text, true);
        let delivered_at = Utc::now();
        match pasted {
            Ok(_) => {
                let saved = self.store().update_queue(id, |queue| {
                    if let Some(i) = queue.item_mut(item_id) {
                        i.state = ItemState::Delivered {
                            delivered_at,
                            reconciled: false,
                        };
                    }
                    queue.last_delivered_at = Some(delivered_at);
                    queue.settle_order();
                    queue.trim_history();
                    queue.pending_count()
                });
                let pending = match saved {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("fw hub: queue {id}: delivered, but not recorded: {e}");
                        0
                    }
                };
                eprintln!(
                    "fw hub: queue {id}: delivered {} ({} bytes, {}); {pending} pending",
                    &item_id[..item_id.len().min(8)],
                    item.text.len(),
                    item.when.as_str()
                );
                self.queue_changed();
                if let Some(session) = self.event_session(id) {
                    self.emit_lifecycle(
                        "queue.delivered",
                        serde_json::json!({
                            "session": session,
                            "item": {
                                "id": item.id,
                                "text": one_line(&item.text, EVENT_TEXT_CHARS),
                                "bytes": item.text.len(),
                                "when": item.when,
                                "added_at": item.added_at,
                                "delivered_at": delivered_at,
                            },
                            "pending": pending,
                        }),
                    );
                }
            }
            Err(e) => {
                let _ = self.store().update_queue(id, |queue| {
                    if let Some(i) = queue.item_mut(item_id) {
                        i.state = ItemState::Pending;
                    }
                });
                self.stall_queue(id, &format!("delivery failed: {e}"), Some(item_id));
            }
        }
    }
}

#[cfg(test)]
mod tests;
