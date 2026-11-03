//! The queue's runtime: the delivery gate, edit leases, typing times and
//! the drafts left in agents' input lines.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

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
pub(crate) struct Holds {
    pub(crate) next: u64,
    pub(crate) held: HashMap<u64, (Vec<String>, String)>,
}

/// Most bytes of a draft kept (past this only "there is one" matters).
const MAX_DRAFT_BYTES: usize = 16 * 1024;
/// How long an open editor keeps its item from going without renewing.
pub const EDIT_LEASE: Duration = Duration::from_secs(30);

/// Input typed into one session: when, and what of it sits unsent in the
/// agent's input line (a draft someone left half-typed).
#[derive(Debug, Clone)]
struct Typed {
    at: Instant,
    draft: Draft,
}

/// A model of the agent's input line, fed every byte typed into the
/// session: printable text (and pasted text) adds, Backspace, Ctrl-W,
/// Ctrl-U, Ctrl-C, Escape twice, and Enter take away; escape sequences (arrows, focus,
/// mouse) do not count. Not exact (history recall, tab completion), but
/// it knows when someone typed and walked away without sending.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Draft {
    bytes: Vec<u8>,
    in_paste: bool,
    /// The last key was a lone Escape.
    escaped: bool,
}

impl Draft {
    pub(crate) fn from_text(text: &str) -> Self {
        let mut draft = Self::default();
        draft.push(text.as_bytes());
        draft
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > MAX_DRAFT_BYTES {
            let cut = self.bytes.len() - MAX_DRAFT_BYTES;
            self.bytes.drain(..cut);
        }
    }

    fn pop_char(&mut self) {
        while let Some(b) = self.bytes.pop() {
            if b & 0xC0 != 0x80 {
                break;
            }
        }
    }

    fn pop_word(&mut self) {
        while self.bytes.last().is_some_and(u8::is_ascii_whitespace) {
            self.bytes.pop();
        }
        while self.bytes.last().is_some_and(|b| !b.is_ascii_whitespace()) {
            self.bytes.pop();
        }
    }

    /// Apply `input` as the terminal sends it.
    pub(crate) fn feed(&mut self, input: &[u8]) {
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            if b == 0x1b {
                match input.get(i + 1) {
                    Some(b'[') => {
                        let mut end = i + 2;
                        while end < input.len() && !(0x40..=0x7e).contains(&input[end]) {
                            end += 1;
                        }
                        let seq = &input[i..(end + 1).min(input.len())];
                        match seq {
                            b"\x1b[200~" => self.in_paste = true,
                            b"\x1b[201~" => self.in_paste = false,
                            _ => {}
                        }
                        // X10 mouse: three raw bytes follow.
                        i = end + 1 + if seq == b"\x1b[M" { 3 } else { 0 };
                    }
                    Some(b'O') => i += 3,
                    // Alt+Enter: a line break in the draft.
                    Some(b'\r') => {
                        self.push(b"\n");
                        i += 2;
                    }
                    // A lone Escape; twice running clears the line.
                    Some(0x1b) | None => {
                        if self.escaped {
                            self.bytes.clear();
                        }
                        self.escaped = !self.escaped;
                        i += 1;
                        continue;
                    }
                    Some(_) => i += 2,
                }
                self.escaped = false;
                continue;
            }
            self.escaped = false;
            if self.in_paste {
                self.push(&[b]);
                i += 1;
                continue;
            }
            match b {
                b'\r' | b'\n' | 0x03 | 0x15 => self.bytes.clear(),
                0x7f | 0x08 => self.pop_char(),
                0x17 => self.pop_word(),
                0x00..=0x1f => {}
                _ => self.push(&[b]),
            }
            i += 1;
        }
    }

    /// Characters left unsent.
    pub(crate) fn chars(&self) -> usize {
        String::from_utf8_lossy(&self.bytes).chars().count()
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// One session's typing as a live handoff carries it to the next hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypingExport {
    pub session_id: String,
    /// Milliseconds since the last input.
    pub ago_ms: u64,
    /// What sits unsent in its input line.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub draft: String,
}

/// The queues' in-memory side: typing times and drafts, holds, edits
/// open on items, the paste turn, and the delivery gate.
#[derive(Debug, Default)]
pub struct QueueRuntime {
    /// Session id -> input last typed into it.
    typing: Mutex<HashMap<String, Typed>>,
    pub(crate) holds: Mutex<Holds>,
    /// Item id -> until when an open editor keeps it back.
    editing: Mutex<HashMap<String, Instant>>,
    /// Taken by every paste for its text and Enter, so two never
    /// interleave (a queued prompt merged into another's Enter).
    paste_turn: Mutex<()>,
    gate: Mutex<Gate>,
    gate_changed: Condvar,
    pub(crate) started: AtomicBool,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl QueueRuntime {
    /// Start a delivery unless the hub is frozen or stopping.
    pub(crate) fn begin_delivery(&self) -> bool {
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

    pub(crate) fn is_stopped(&self) -> bool {
        lock(&self.gate).stopped
    }

    pub(crate) fn is_paused(&self) -> bool {
        let gate = lock(&self.gate);
        gate.paused || gate.stopped
    }

    pub(crate) fn typed_ago(&self, id: &str) -> Option<Duration> {
        lock(&self.typing).get(id).map(|t| t.at.elapsed())
    }

    /// Characters left unsent in session `id`'s input line.
    pub(crate) fn draft_chars(&self, id: &str) -> usize {
        lock(&self.typing).get(id).map_or(0, |t| t.draft.chars())
    }

    pub(crate) fn note(&self, id: &str, bytes: &[u8], count_draft: bool) {
        let mut typing = lock(&self.typing);
        let typed = typing.entry(id.to_owned()).or_insert_with(|| Typed {
            at: Instant::now(),
            draft: Draft::default(),
        });
        typed.at = Instant::now();
        if count_draft {
            typed.draft.feed(bytes);
        }
    }

    /// A turn started: whatever sat in the input line was sent.
    pub(crate) fn clear_draft(&self, id: &str) {
        if let Some(typed) = lock(&self.typing).get_mut(id) {
            typed.draft = Draft::default();
        }
    }

    /// Forget a removed session.
    pub(crate) fn forget(&self, id: &str) {
        lock(&self.typing).remove(id);
    }

    /// Keep item `item` back for an open editor (`false`: let it go).
    pub(crate) fn set_editing(&self, item: &str, editing: bool) {
        let mut map = lock(&self.editing);
        let now = Instant::now();
        map.retain(|_, until| *until > now);
        if editing {
            map.insert(item.to_owned(), now + EDIT_LEASE);
        } else {
            map.remove(item);
        }
    }

    pub(crate) fn is_editing(&self, item: &str) -> bool {
        lock(&self.editing)
            .get(item)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Take the paste turn (held while text and Enter are written).
    pub(crate) fn paste_turn(&self) -> MutexGuard<'_, ()> {
        lock(&self.paste_turn)
    }

    /// Every session's typing, for a live handoff's next hub.
    pub(crate) fn typing_export(&self) -> Vec<TypingExport> {
        lock(&self.typing)
            .iter()
            .map(|(id, t)| TypingExport {
                session_id: id.clone(),
                ago_ms: u64::try_from(t.at.elapsed().as_millis()).unwrap_or(u64::MAX),
                draft: t.draft.text(),
            })
            .collect()
    }

    /// Take over what [`QueueRuntime::typing_export`] gave.
    pub(crate) fn typing_import(&self, exported: Vec<TypingExport>) {
        let now = Instant::now();
        let mut typing = lock(&self.typing);
        for e in exported {
            let at = now
                .checked_sub(Duration::from_millis(e.ago_ms))
                .unwrap_or(now);
            typing.insert(
                e.session_id,
                Typed {
                    at,
                    draft: Draft::from_text(&e.draft),
                },
            );
        }
    }

    pub(crate) fn holder(&self, id: &str) -> Option<String> {
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
    pub(crate) runtime: &'a QueueRuntime,
    pub(crate) key: u64,
}

impl Drop for SessionHold<'_> {
    fn drop(&mut self) {
        lock(&self.runtime.holds).held.remove(&self.key);
    }
}

/// Ends a delivery however it returns.
pub(crate) struct Delivering<'a>(pub(crate) &'a QueueRuntime);

impl Drop for Delivering<'_> {
    fn drop(&mut self) {
        self.0.end_delivery();
    }
}
