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
//!   browser, `fw hub attach`, `fw hub send`, Compose, or any paste the
//!   hub makes: a handoff, send findings, a plugin action);
//! * nothing typed into the agent's input line sits there unsent (a
//!   [`Draft`] someone left half-typed: the paste and its Enter would send
//!   the two merged). The input line is read off the screen, which
//!   wins either way when it shows one; else the bytes typed into the
//!   session say (keys answering a blocked prompt do not count);
//! * no editor has the item open (a lease the drawer renews);
//! * no fan-out grading is queued or running on it, and no review action
//!   (commit, push, merge, ask for a review, send findings) or handoff is
//!   running on it.
//!
//! Delivery is the same path as every paste (bracketed paste, then Enter),
//! under the paste turn every paste takes, so no other paste writes
//! between its text and its Enter; the decision is taken again under it.
//! It is exactly-once: the item is recorded `sending` in `state.json`
//! before the paste and `delivered` (with its time) after; a hub that died
//! in between finds `sending` on its next start and looks in the agent's
//! transcript: a user message that is the text since then means it went
//! (marked delivered, never typed again), else it is sent again. A live
//! handoff (`fw hub upgrade`) waits for a delivery in progress, starts
//! none until the new hub has taken over, and carries typing times and
//! drafts over to it.
//!
//! A failure -- the agent exited, the session was not restored, the paste
//! was refused, a delivered prompt never started a turn -- stalls the
//! queue (nothing more is sent) until it is resumed; stalled queues show
//! in Needs you and announce `queue.stalled` to plugins (a relaunch of the
//! session clears the stall too). Each delivery announces
//! `queue.delivered`; both events carry the item in one shape.
//!
//! The queue lives on the hub that owns the session; other hubs reach it
//! through their `/m/<machine>` proxy like every session call.

use crate::state::StateError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

mod api;
mod deciding;
mod delivery;
mod input_line;
mod runtime;

pub use api::Delivery;
pub(crate) use deciding::{Presence, event_item, event_text};
pub use runtime::{EDIT_LEASE, QueueRuntime, SessionHold, TypingExport};

/// Pending items per session.
pub const MAX_PENDING: usize = 50;
/// Delivered items kept per session (oldest dropped first).
pub const MAX_DELIVERED_KEPT: usize = 20;
/// The longest topic.
pub const MAX_TOPIC_CHARS: usize = 64;
/// Topics remembered as delivered per session.
pub const MAX_TOPICS_SEEN: usize = 256;
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
/// A delivered prompt that has not started a turn after this long is sent
/// again when the agent's transcript lacks it (the paste was lost), else
/// (or lost again) stalls the queue.
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
    /// The agent session that sent it (`message_session`): its id.
    /// `None`: the operator, the API, or the hub itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Sent ahead of every waiting item that is not (behind earlier
    /// priority items; never ahead of one being sent).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub priority: bool,
    /// The watch that queued it (see [`crate::watch`]): while it is
    /// pending, the watch's next matches join it rather than queue more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<String>,
    /// What it is about, as its sender keyed it (`message_session`'s
    /// `topic`, `fw hub queue add --topic`): a newer item from the same
    /// sender with the same topic supersedes it (see [`Supersede`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
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
    /// The delivered item sent again because the agent never took it up
    /// (the agent rested past [`ACK_TIMEOUT`] with no turn): lost a second time, the queue stalls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resent: Option<String>,
    /// Topics delivered to this session, by sender (see [`topic_key`]):
    /// a newer message on one of them arrives saying it supersedes the
    /// one the agent already read. At most [`MAX_TOPICS_SEEN`], the
    /// oldest dropped first.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub topics_seen: BTreeMap<String, TopicSeen>,
}

/// A topic's latest delivery to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicSeen {
    /// The item delivered (`None`: typed in at once, never queued).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub delivered_at: DateTime<Utc>,
}

/// What a message with a topic did to its sender's earlier one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supersede {
    /// No earlier message on the topic.
    None,
    /// The earlier one still waited: its text was replaced in place (same
    /// id); `place`: 1-based among the items not yet delivered.
    Replaced { item: String, place: usize },
    /// The earlier one was delivered (or is being sent) at `at`: the new
    /// one says it supersedes it.
    AfterRead { at: DateTime<Utc> },
}

/// The key of `topic` from `sender` (a session id; `None`: the operator)
/// in [`SessionQueue::topics_seen`].
pub fn topic_key(sender: Option<&str>, topic: &str) -> String {
    format!("{}\u{1f}{topic}", sender.unwrap_or("operator"))
}

/// `item` is `sender`'s message on `topic` (not a watch's prompt).
fn is_on_topic(item: &QueueItem, sender: Option<&str>, topic: &str) -> bool {
    item.from.as_deref() == sender && item.watch.is_none() && item.topic.as_deref() == Some(topic)
}

/// A topic as given: trimmed, 1-[`MAX_TOPIC_CHARS`] of `A-Z a-z 0-9 . _ : / -`.
pub fn validate_topic(raw: &str) -> Result<String, StateError> {
    let topic = raw.trim();
    let ok = !topic.is_empty()
        && topic.chars().count() <= MAX_TOPIC_CHARS
        && topic
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'));
    if ok {
        Ok(topic.to_owned())
    } else {
        Err(StateError::Invalid(format!(
            "topic {topic:?}: 1-{MAX_TOPIC_CHARS} characters of letters, digits, and . _ : / -"
        )))
    }
}

/// The line a superseding message starts with (after its sender line).
pub fn superseded_line(topic: &str, at: DateTime<Utc>) -> String {
    format!(
        "superseded: {topic} (replaces the message on this topic delivered at {}; act on this one)",
        at.format("%H:%M UTC")
    )
}

/// What [`SessionQueue::join_unread`] did with a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Joined {
    /// Appended to the sender's unread item `item`.
    Appended { item: String },
    /// The same text already waits in `item`: nothing added.
    Repeat { item: String },
}

/// The line between two messages joined into one item.
pub const JOINED_LINE: &str = "-- and then, from the same session:";

impl SessionQueue {
    /// Join `text` (a header paragraph, then the message) from agent
    /// session `sender` to its newest message still unread here (pending,
    /// no topic, not priority, not a watch's): one prompt, so a burst of
    /// messages is one turn and the queue holds one item per sender. The
    /// header is not repeated when it is the same; an exact repeat is
    /// dropped. `None`: nothing to join (or it would pass
    /// [`MAX_TEXT_BYTES`]): queue it on its own.
    pub fn join_unread(&mut self, sender: &str, text: &str) -> Option<Joined> {
        let (head, body) = text.split_once("\n\n").unwrap_or(("", text));
        let item = self.items.iter_mut().rev().find(|i| {
            matches!(i.state, ItemState::Pending)
                && i.from.as_deref() == Some(sender)
                && i.topic.is_none()
                && !i.priority
                && i.watch.is_none()
                && i.when == When::Done
        })?;
        let separator = format!("\n\n{JOINED_LINE}\n\n");
        let repeat = item.text.split(separator.as_str()).any(|part| {
            let part = part.trim();
            part == body.trim()
                || part
                    .split_once("\n\n")
                    .is_some_and(|(_, rest)| rest.trim() == body.trim())
        });
        if repeat {
            return Some(Joined::Repeat {
                item: item.id.clone(),
            });
        }
        let addition = if !head.is_empty() && item.text.starts_with(head) {
            body
        } else {
            text
        };
        let joined = format!("{}\n\n{JOINED_LINE}\n\n{addition}", item.text.trim_end());
        if joined.len() > MAX_TEXT_BYTES {
            return None;
        }
        item.text = joined;
        Some(Joined::Appended {
            item: item.id.clone(),
        })
    }

    /// Try again after a stall (the operator resumed it, or the session
    /// was relaunched): the stall is cleared, and so is the wait for the
    /// agent to take up the last delivered prompt, which is what stalls
    /// a queue whose prompt was lost (else it would stall straight back).
    pub fn retry(&mut self) {
        self.stalled = None;
        self.last_delivered_at = None;
    }

    /// Nothing worth keeping (the store drops it).
    pub fn is_vacant(&self) -> bool {
        self.items.is_empty()
            && !self.paused
            && self.stalled.is_none()
            && self.last_delivered_at.is_none()
            && self.topics_seen.is_empty()
    }

    /// Note `topic` from `sender` delivered at `at` (`item`: its queue
    /// item), keeping the newest [`MAX_TOPICS_SEEN`].
    pub(crate) fn note_topic(
        &mut self,
        sender: Option<&str>,
        topic: &str,
        item: Option<&str>,
        at: DateTime<Utc>,
    ) {
        self.topics_seen.insert(
            topic_key(sender, topic),
            TopicSeen {
                item: item.map(str::to_owned),
                delivered_at: at,
            },
        );
        while self.topics_seen.len() > MAX_TOPICS_SEEN {
            let oldest = self
                .topics_seen
                .iter()
                .min_by_key(|(_, seen)| seen.delivered_at)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => self.topics_seen.remove(&key),
                None => break,
            };
        }
    }

    /// When the agent read `sender`'s latest message on `topic`: its
    /// delivery (or the send under way now), else the topic's record.
    pub(crate) fn topic_read_at(&self, sender: Option<&str>, topic: &str) -> Option<DateTime<Utc>> {
        let mine = |i: &QueueItem| is_on_topic(i, sender, topic);
        let sending = self.items.iter().find_map(|i| match i.state {
            ItemState::Sending { sending_at } if mine(i) => Some(sending_at),
            _ => None,
        });
        let delivered = self.items.iter().rev().find_map(|i| match i.state {
            ItemState::Delivered { delivered_at, .. } if mine(i) => Some(delivered_at),
            _ => None,
        });
        let seen = self
            .topics_seen
            .get(&topic_key(sender, topic))
            .map(|s| s.delivered_at);
        sending.or(delivered).or(seen)
    }

    /// Supersede `sender`'s earlier message on `topic` with the text
    /// `compose` makes (given when the agent read an earlier one, if it
    /// did): a waiting one takes that text (and `priority`, if raised) in
    /// place, keeping its id and place, and any other waiting one on the
    /// topic goes. Nothing waiting: whether the agent read one.
    pub(crate) fn supersede(
        &mut self,
        sender: Option<&str>,
        topic: &str,
        priority: bool,
        compose: impl FnOnce(Option<DateTime<Utc>>) -> String,
    ) -> Supersede {
        let read_at = self.topic_read_at(sender, topic);
        let pending: Vec<String> = self
            .items
            .iter()
            .filter(|i| is_on_topic(i, sender, topic) && matches!(i.state, ItemState::Pending))
            .map(|i| i.id.clone())
            .collect();
        let Some((keep, rest)) = pending.split_first() else {
            return match read_at {
                Some(at) => Supersede::AfterRead { at },
                None => Supersede::None,
            };
        };
        self.items.retain(|i| !rest.contains(&i.id));
        let text = compose(read_at);
        let raise = self.item_mut(keep).is_some_and(|item| {
            item.text = text;
            item.added_at = Utc::now();
            let raise = priority && !item.priority;
            item.priority |= priority;
            raise
        });
        if raise && let Some(at) = self.items.iter().position(|i| &i.id == keep) {
            let item = self.items.remove(at);
            self.insert_pending(item);
        }
        let place = self
            .waiting()
            .position(|i| &i.id == keep)
            .map_or(1, |at| at + 1);
        Supersede::Replaced {
            item: keep.clone(),
            place,
        }
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

    /// Insert `item` among the pending items: a priority item behind the
    /// waiting priority items, ahead of the rest; any other at the end.
    /// Never ahead of an item being sent (it is not pending). The item
    /// that loses the front forgets a block it saw.
    pub(crate) fn insert_pending(&mut self, item: QueueItem) {
        let old_head = self.head().map(|i| i.id.clone());
        let at = if item.priority {
            self.items
                .iter()
                .position(|i| matches!(i.state, ItemState::Pending) && !i.priority)
                .unwrap_or(self.items.len())
        } else {
            self.items.len()
        };
        self.items.insert(at, item);
        self.settle_order();
        if let Some(old) = old_head
            && self.head().is_some_and(|h| h.id != old)
            && let Some(item) = self.item_mut(&old)
        {
            item.blocked_at = None;
        }
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
    /// The watches that queue prompts here (see [`crate::watch`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub watches: Vec<crate::watch::Watch>,
}

/// An item to add.
#[derive(Debug, Clone, Deserialize)]
pub struct NewItem {
    pub text: String,
    #[serde(default)]
    pub when: Option<String>,
    /// Supersedes the sender's earlier item on this topic (see
    /// [`Supersede`]).
    #[serde(default)]
    pub topic: Option<String>,
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

#[cfg(test)]
mod tests;
