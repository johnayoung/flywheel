//! Following agent transcripts for suggested commands and replies.
//!
//! Claude Code appends every reply to its transcript
//! (`<config>/projects/<dir>/<conversation>.jsonl`); Codex to its rollout
//! (`<codex home>/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`). The
//! hub tails the file of each agent session's current conversation from a
//! byte offset (no re-reads, memory bounded per line and per session),
//! pulls the fenced code blocks out of assistant text, and keeps the
//! newest [`MAX_COMMANDS`] per session; the assistant text itself is kept
//! as replies ([`crate::replies`], the newest [`MAX_REPLIES`]). A conversation switch (`/clear`,
//! a resume under a new id) moves the tail to the new file; a file that
//! is replaced or truncated is read again from the start, without
//! duplicating blocks already seen. Codex sessions with no rollout to be
//! found fall back to the `notify` payload's `last-assistant-message`.
//!
//! Each session's commands, replies, and tail position are saved under the
//! state dir (`commands/<session>.json`), so a restarted hub keeps every block,
//! including those of conversations before a `/clear`, under the same
//! `seq`, and resumes reading where it stopped.
//!
//! The same tail counts the session's token usage ([`crate::usage`]):
//! Claude Code's per-response `usage`, plus its subagents' transcripts
//! (`<conversation>/subagents/*.jsonl` beside the main one, read for usage
//! only), and Codex's `token_count` events. Usage is saved in the same
//! file; a removed session's moves to the hub's usage archive.

use crate::agent::{SessionKind, claude_transcript, codex_rollout, is_conversation_id};
pub(crate) use crate::agentlog::Format;
use crate::agentlog::{self, Event, JsonlTail, Line, PartKind, Read, Role};
pub use crate::agentlog::{MAX_LINE_BYTES, READ_CHUNK};
use crate::commands::{
    Command, CommandSource, MAX_BLOCK_BYTES, bang_spans, copyable, fenced_blocks, is_shell,
};
use crate::hub::{Hub, HubError, HubEvent};
use crate::pty::{ENTER_DELAY, WriteOutcome, paste_bytes};
pub use crate::replies::MAX_REPLIES;
use crate::replies::{AgentReply, ReplyLog, ReplyView};
use crate::state::{SessionRecord, StateError};
use crate::usage::{
    CodexCursor, RollupBuilder, SessionUsage, Turn, UsageArchive, UsageView, claude_usage,
    codex_usage,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

/// The state dir's subdirectory holding each session's saved commands.
pub const COMMANDS_DIR: &str = "commands";
/// Commands kept per session (oldest dropped first).
pub const MAX_COMMANDS: usize = 500;
/// Block keys remembered per session against re-reading a replaced file.
const MAX_KEYS: usize = 4096;
/// How often running agent sessions' transcripts are checked.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Least time between two searches for a session's file that was not found.
const LOOKUP_RETRY: Duration = Duration::from_secs(5);
/// Codex `notify` replies held until the watcher reads them.
const MAX_PENDING_NOTIFY: usize = 16;
/// A command is "fresh" (worth a toast) when written this recently.
pub const FRESH_WITHIN: Duration = Duration::from_secs(120);
/// Largest paste accepted.
pub const MAX_PASTE_BYTES: usize = 1024 * 1024;
/// Subagent transcripts followed per session (for usage).
const MAX_SUBAGENT_TAILS: usize = 256;
/// Files left behind by a conversation switch whose read positions are
/// kept (going back reads on from there instead of counting them again).
const MAX_PARKED_TAILS: usize = 512;

/// An incremental reader of one transcript: the file's tail and what is
/// known about the conversation in it.
#[derive(Debug)]
struct Tail {
    log: JsonlTail,
    format: Format,
    /// The conversation the file belongs to.
    conversation: Option<String>,
    /// Read for usage only (a Claude subagent's transcript).
    usage_only: bool,
    /// Where a Codex rollout's token counting stands.
    codex: CodexCursor,
}

impl std::ops::Deref for Tail {
    type Target = JsonlTail;

    fn deref(&self) -> &JsonlTail {
        &self.log
    }
}

/// What one transcript line holds for the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Reply(Reply),
    Usage(Turn),
}

/// One reply's text from a transcript line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub message_id: String,
    pub timestamp: Option<String>,
    /// The reply's text parts (each mined separately for fences).
    pub texts: Vec<String>,
}

impl Reply {
    /// The reply's text as one Markdown text: its parts, a blank line
    /// between each.
    fn text(&self) -> String {
        self.texts.join("\n\n")
    }
}

impl Tail {
    fn new(path: PathBuf, format: Format, conversation: Option<String>) -> Self {
        Self {
            log: JsonlTail::new(path),
            format,
            conversation,
            usage_only: false,
            codex: CodexCursor::default(),
        }
    }

    /// A tail of a Claude subagent's transcript, read for usage only.
    fn usage_only(path: PathBuf, conversation: Option<String>) -> Self {
        Self {
            usage_only: true,
            ..Self::new(path, Format::Claude, conversation)
        }
    }

    /// A tail resumed from a saved position (the start of a line).
    fn resume(saved: SavedTail) -> Self {
        Self {
            log: JsonlTail::resume(saved.path, saved.offset, saved.identity),
            format: saved.format,
            conversation: saved.conversation,
            usage_only: saved.usage_only,
            codex: saved.codex,
        }
    }

    fn saved(&self) -> SavedTail {
        SavedTail {
            path: self.path.clone(),
            format: self.format,
            conversation: self.conversation.clone(),
            offset: self.position(),
            identity: self.identity(),
            usage_only: self.usage_only,
            codex: self.codex.clone(),
        }
    }

    /// Read what was appended since the last call, handing `each` the
    /// replies and usage in the complete lines as they are read (so a
    /// long backlog is never held whole). A missing or unreadable file
    /// reads as nothing (it is tried again at the next poll).
    fn read(&mut self, each: &mut dyn FnMut(Item)) {
        let Self {
            log,
            format,
            usage_only,
            codex,
            ..
        } = self;
        let _ = log.read(None, &mut |read| match read {
            // A new file counts from nothing.
            Read::Restart => codex.total = None,
            Read::Line { bytes, start } => line(*format, *usage_only, codex, bytes, start, each),
        });
    }
}

/// One complete transcript line, starting at byte `start` of its file.
fn line(
    format: Format,
    usage_only: bool,
    codex: &mut CodexCursor,
    bytes: &[u8],
    start: u64,
    each: &mut dyn FnMut(Item),
) {
    // Cheap pre-filter: only lines with assistant text (a reply, maybe
    // with fences or an inline `! cmd`), or usage, are parsed (big tool
    // results are skipped unparsed).
    let text = !usage_only
        && match format {
            Format::Claude => {
                contains(bytes, b"\"type\":\"text\"") && contains(bytes, b"\"type\":\"assistant\"")
            }
            Format::Codex => contains(bytes, b"\"output_text\""),
        };
    let usage = match format {
        Format::Claude => {
            contains(bytes, b"\"usage\"") && contains(bytes, b"\"type\":\"assistant\"")
        }
        Format::Codex => contains(bytes, b"token_count") || contains(bytes, b"turn_context"),
    };
    if !text && !usage {
        return;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return;
    };
    let parsed = agentlog::parse(format, &value, start);
    if usage {
        let turn = match format {
            Format::Claude => claude_usage(&parsed),
            Format::Codex => codex_usage(&parsed).and_then(|l| codex.feed(l)),
        };
        if let Some(turn) = turn {
            each(Item::Usage(turn));
        }
    }
    if text && let Some(mut reply) = reply_of(format, parsed) {
        // An item without an id (Codex rollouts write none) is named by
        // where its line starts (see [`crate::agentlog`]).
        if reply.message_id.is_empty() {
            reply.message_id = format!("@{start}");
        }
        each(Item::Reply(reply));
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The reply in a parsed transcript line: the assistant's text. Claude's
/// text parts are mined separately; a Codex reply's `output_text` parts
/// are one text.
fn reply_of(format: Format, line: Line) -> Option<Reply> {
    line.events.into_iter().find_map(|event| match event {
        Event::Text {
            role: Role::Assistant,
            id,
            parts,
        } => {
            let texts: Vec<String> = match format {
                Format::Claude => parts.into_iter().map(|p| p.text).collect(),
                Format::Codex => {
                    let outputs: Vec<String> = parts
                        .into_iter()
                        .filter(|p| p.kind == PartKind::Output)
                        .map(|p| p.text)
                        .collect();
                    if outputs.is_empty() {
                        return None;
                    }
                    vec![outputs.concat()]
                }
            };
            Some(Reply {
                message_id: id,
                timestamp: line.timestamp.clone(),
                texts,
            })
        }
        _ => None,
    })
}

/// A session's commands as saved under the state dir.
#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    next_seq: u64,
    tail: Option<SavedTail>,
    /// Oldest first.
    commands: Vec<Command>,
    #[serde(default)]
    usage: SessionUsage,
    /// Claude subagent transcripts being read for usage.
    #[serde(default)]
    subagents: Vec<SavedTail>,
    /// Where earlier conversations' files were left.
    #[serde(default)]
    parked: Vec<SavedTail>,
    /// See [`SessionCommands::from_hook`].
    #[serde(default)]
    from_hook: Vec<(Option<String>, String)>,
    /// See [`SessionCommands::last_reply`].
    #[serde(default)]
    last_reply: Option<(Option<String>, Vec<String>)>,
    /// Oldest first.
    #[serde(default)]
    replies: Vec<AgentReply>,
    #[serde(default)]
    next_reply_seq: u64,
}

/// Where a tail stopped.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedTail {
    path: PathBuf,
    format: Format,
    conversation: Option<String>,
    /// The start of the first line not yet read.
    offset: u64,
    identity: Option<(u64, u64)>,
    #[serde(default)]
    usage_only: bool,
    #[serde(default)]
    codex: CodexCursor,
}

/// One session's commands and the tail feeding them.
#[derive(Debug, Default)]
struct SessionCommands {
    /// Oldest first.
    entries: VecDeque<Command>,
    next_seq: u64,
    keys: HashSet<(String, String, usize)>,
    key_order: VecDeque<(String, String, usize)>,
    tail: Option<Tail>,
    /// The transcript path the agent's hooks last named (Claude).
    hinted: Option<PathBuf>,
    /// Last failed search for the file, and for which conversation.
    missed: Option<(Option<String>, Instant)>,
    /// Codex `notify` replies (turn id, text) not yet read.
    notified: VecDeque<(String, String)>,
    /// Blocks of the last Claude Stop hook reply (conversation, text) its
    /// transcript has not written yet: that line, when it comes, adds
    /// nothing again. Only the next transcript reply is that line: one
    /// matching none of these clears them (a later turn's identical block
    /// is a new command).
    from_hook: Vec<(Option<String>, String)>,
    /// Every fence (in order) of the last Claude transcript reply read, and
    /// its conversation: a Stop hook's reply that is a prefix of it (the
    /// hook's payload may be cut short) was read already.
    last_reply: Option<(Option<String>, Vec<String>)>,
    /// The assistant's replies (see [`crate::replies`]).
    replies: ReplyLog,
    /// Where the log is saved (`None`: kept in memory only).
    file: Option<PathBuf>,
    /// Why the last save failed.
    save_error: Option<String>,
    /// Tokens and cost so far (its own lock: listings read it while a
    /// long transcript is being drained).
    usage: Arc<Mutex<SessionUsage>>,
    /// Claude subagent transcripts of the current conversation.
    subagents: Vec<Tail>,
    /// Where files of earlier conversations were left, oldest first.
    parked: VecDeque<SavedTail>,
    /// Usage changed since the last refresh reported it.
    usage_changed: bool,
    /// The session was removed: nothing is read or saved any more.
    gone: bool,
}

impl SessionCommands {
    /// The log saved at `file`, or an empty one when there is none (or it
    /// cannot be read: it is rewritten at the next save).
    fn load(file: Option<PathBuf>) -> Self {
        let mut log = Self {
            file,
            ..Self::default()
        };
        let Some(saved) = log
            .file
            .as_deref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
        else {
            return log;
        };
        let skip = saved.commands.len().saturating_sub(MAX_COMMANDS);
        for command in saved.commands.into_iter().skip(skip) {
            log.remember(key_of(
                command.conversation.as_deref(),
                &command.message_id,
                command.block_index,
            ));
            log.next_seq = log.next_seq.max(command.seq + 1);
            log.entries.push_back(command);
        }
        log.next_seq = log.next_seq.max(saved.next_seq);
        log.tail = saved.tail.map(Tail::resume);
        log.usage = Arc::new(Mutex::new(saved.usage));
        log.subagents = saved.subagents.into_iter().map(Tail::resume).collect();
        log.parked = saved.parked.into_iter().collect();
        log.from_hook = saved.from_hook;
        log.last_reply = saved.last_reply;
        log.replies = ReplyLog::restore(saved.replies, saved.next_reply_seq);
        log
    }

    fn usage(&self) -> std::sync::MutexGuard<'_, SessionUsage> {
        self.usage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keep where `tail` stopped, for a return to its file.
    fn park(&mut self, tail: &Tail) {
        self.parked.retain(|p| p.path != tail.path);
        if self.parked.len() == MAX_PARKED_TAILS {
            self.parked.pop_front();
        }
        self.parked.push_back(tail.saved());
    }

    /// A tail of `path`: on from where it was left, if it was read before.
    fn reopen(&mut self, path: PathBuf, format: Format, conversation: Option<String>) -> Tail {
        match self
            .parked
            .iter()
            .position(|p| p.path == path && p.format == format)
            .and_then(|at| self.parked.remove(at))
        {
            Some(saved) => Tail {
                conversation,
                ..Tail::resume(saved)
            },
            None => Tail::new(path, format, conversation),
        }
    }

    /// Write the log to its file (atomically, owner-only).
    fn save(&mut self) {
        if self.gone {
            return;
        }
        let Some(file) = self.file.as_deref() else {
            return;
        };
        let (replies, next_reply_seq) = self.replies.saved();
        let saved = Saved {
            next_seq: self.next_seq,
            tail: self.tail.as_ref().map(Tail::saved),
            commands: self.entries.iter().cloned().collect(),
            usage: self.usage().clone(),
            subagents: self.subagents.iter().map(Tail::saved).collect(),
            parked: self.parked.iter().cloned().collect(),
            from_hook: self.from_hook.clone(),
            last_reply: self.last_reply.clone(),
            replies,
            next_reply_seq,
        };
        let result = serde_json::to_vec(&saved)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                if let Some(dir) = file.parent() {
                    crate::paths::ensure_private_dir(dir)?;
                }
                crate::auth::write_private(file, &bytes)
            });
        self.save_error = result
            .err()
            .map(|e| format!("cannot save commands to {}: {e}", file.display()));
    }

    /// Add a reply's blocks; returns the new commands. `closed_only` keeps
    /// unclosed fences out (a text that may have been cut short).
    fn add(
        &mut self,
        reply: &Reply,
        conversation: Option<&str>,
        source: CommandSource,
        closed_only: bool,
    ) -> Vec<Command> {
        let mut added = Vec::new();
        let mut fences = Vec::new();
        let mut matched_hook = false;
        // Fences first, then inline `! cmd` spans: a fence keeps the index
        // it had before spans were read.
        let fenced = reply
            .texts
            .iter()
            .flat_map(|t| fenced_blocks(t))
            .map(|b| (b, true));
        let spans = reply
            .texts
            .iter()
            .flat_map(|t| bang_spans(t))
            .map(|b| (b, false));
        for (block_index, (block, is_fence)) in fenced.chain(spans).enumerate() {
            if source == CommandSource::Transcript && is_fence {
                fences.push(block.text.clone());
            }
            if (closed_only && !block.closed) || block.text.len() > MAX_BLOCK_BYTES {
                continue;
            }
            let key = key_of(conversation, &reply.message_id, block_index);
            if self.keys.contains(&key) {
                continue;
            }
            // The transcript line of a reply its Stop hook gave first.
            if source == CommandSource::Transcript
                && let Some(at) = self
                    .from_hook
                    .iter()
                    .position(|(c, t)| c.as_deref() == conversation && *t == block.text)
            {
                self.from_hook.remove(at);
                matched_hook = true;
                continue;
            }
            // A rollout found after its notify fallback was used: the
            // same text from the same conversation is the same block.
            if source == CommandSource::Rollout
                && self.entries.iter().any(|c| {
                    c.source == CommandSource::Notify
                        && c.conversation.as_deref() == conversation
                        && c.text == block.text
                })
            {
                continue;
            }
            self.remember(key);
            let lang = block.lang.clone();
            let command = Command {
                seq: self.next_seq,
                conversation: conversation.map(str::to_owned),
                message_id: reply.message_id.clone(),
                timestamp: reply.timestamp.clone(),
                block_index,
                shell: is_shell(lang.as_deref(), &block.text),
                command: copyable(lang.as_deref(), &block.text),
                lang,
                text: block.text,
                source,
            };
            self.next_seq += 1;
            if self.entries.len() == MAX_COMMANDS {
                self.entries.pop_front();
            }
            self.entries.push_back(command.clone());
            added.push(command);
        }
        if source == CommandSource::Transcript && !fences.is_empty() {
            // Not the line of the hook's reply: what is left of it never
            // comes (the hook's text differed), and must not swallow a
            // later turn's identical block.
            if !matched_hook {
                self.from_hook.clear();
            }
            self.last_reply = Some((conversation.map(str::to_owned), fences));
        }
        added
    }

    fn remember(&mut self, key: (String, String, usize)) {
        if self.keys.contains(&key) {
            return;
        }
        if self.key_order.len() == MAX_KEYS
            && let Some(old) = self.key_order.pop_front()
        {
            self.keys.remove(&old);
        }
        self.keys.insert(key.clone());
        self.key_order.push_back(key);
    }

    /// The blocks of a Stop hook's `reply` the transcript has not given
    /// yet (closed fences only: a long payload is cut to fit a report);
    /// they are noted so its transcript line adds them no second time.
    fn hook_commands(&mut self, reply: &Reply, conversation: Option<&str>) -> Vec<Command> {
        let closed: Vec<String> = reply
            .texts
            .iter()
            .flat_map(|t| fenced_blocks(t))
            .filter(|b| b.closed)
            .map(|b| b.text)
            .collect();
        if !closed.iter().any(|b| b.len() <= MAX_BLOCK_BYTES) {
            return Vec::new();
        }
        self.from_hook.clear();
        // Read already: the last transcript reply begins with these blocks
        // (the hook's text may be cut short, and its unclosed fences are
        // left out).
        let read = self.last_reply.as_ref().is_some_and(|(c, fences)| {
            c.as_deref() == conversation
                && closed.len() <= fences.len()
                && closed.iter().zip(fences).all(|(a, b)| a == b)
        });
        if read {
            return Vec::new();
        }
        let added = self.add(reply, conversation, CommandSource::Hook, true);
        self.from_hook = added
            .iter()
            .map(|c| (c.conversation.clone(), c.text.clone()))
            .collect();
        added
    }

    /// Keep `reply` as a reply too (see [`crate::replies`]).
    fn add_reply(
        &mut self,
        reply: &Reply,
        conversation: Option<&str>,
        source: CommandSource,
        added: &mut Refreshed,
    ) {
        if let Some(entry) = self.replies.add(
            conversation,
            &reply.message_id,
            reply.timestamp.as_deref(),
            &reply.text(),
            source,
        ) {
            crate::replies::push_bounded(&mut added.replies, entry);
        }
    }

    /// Read what `tail` has new into the log, adding to `added` (at most
    /// [`MAX_COMMANDS`] commands and [`MAX_REPLIES`] replies, the newest);
    /// returns the tail.
    fn drain(&mut self, mut tail: Tail, added: &mut Refreshed) -> Tail {
        let source = source_of(tail.format);
        let conversation = tail.conversation.clone();
        tail.read(&mut |item| match item {
            Item::Reply(reply) => {
                let more = self.add(&reply, conversation.as_deref(), source, false);
                push_bounded(&mut added.commands, more);
                self.add_reply(&reply, conversation.as_deref(), source, added);
            }
            Item::Usage(turn) => {
                let changed = self.usage().observe(&turn);
                self.usage_changed |= changed;
            }
        });
        tail
    }

    /// Follow the Claude subagent transcripts beside `main` (the current
    /// conversation's `<id>.jsonl`): `<id>/subagents/*.jsonl`.
    fn follow_subagents(&mut self, main: &Path, conversation: Option<&str>) {
        let Some(dir) = main
            .file_stem()
            .map(|stem| main.with_file_name(stem).join("subagents"))
        else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl") && p.is_file())
            .collect();
        found.sort();
        for path in found {
            if self.subagents.len() >= MAX_SUBAGENT_TAILS {
                break;
            }
            if !self.subagents.iter().any(|t| t.path == path) {
                let conversation = conversation.map(str::to_owned);
                let tail = if self.parked.iter().any(|p| p.path == path) {
                    self.reopen(path, Format::Claude, conversation)
                } else {
                    Tail::usage_only(path, conversation)
                };
                self.subagents.push(tail);
            }
        }
    }
}

/// A block's identity: conversation, reply, position in the reply.
fn key_of(
    conversation: Option<&str>,
    message_id: &str,
    block_index: usize,
) -> (String, String, usize) {
    (
        conversation.unwrap_or_default().to_owned(),
        message_id.to_owned(),
        block_index,
    )
}

/// Keep at most [`MAX_COMMANDS`] of `added`, the newest.
fn push_bounded(added: &mut Vec<Command>, more: Vec<Command>) {
    added.extend(more);
    if added.len() > MAX_COMMANDS {
        added.drain(..added.len() - MAX_COMMANDS);
    }
}

/// Every session's suggested commands.
#[derive(Debug)]
pub struct CommandBook {
    claude_config_dir: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    /// Where each session's log is saved (`<dir>/<session id>.json`).
    dir: Option<PathBuf>,
    /// Logs of sessions deleted while no hub ran have been removed.
    swept: std::sync::atomic::AtomicBool,
    sessions: Mutex<HashMap<String, Arc<Slot>>>,
    /// Sessions removed in this process: never followed again (a refresh
    /// racing the removal would read the transcript from the start and
    /// count it twice).
    forgotten: Mutex<HashSet<String>>,
    /// Held across a removal ([`CommandBook::retain`] and
    /// [`CommandBook::forget`]), so one session is archived once.
    removing: Mutex<()>,
    wake: Mutex<Option<mpsc::Sender<()>>>,
    /// Usage of removed sessions, and where it is saved.
    archive: Mutex<UsageArchive>,
    archive_file: Option<PathBuf>,
}

/// One session's log, and its usage (also reachable without the log's
/// lock, which a transcript drain holds).
#[derive(Debug)]
struct Slot {
    log: Mutex<SessionCommands>,
    usage: Arc<Mutex<SessionUsage>>,
}

impl Slot {
    fn new(log: SessionCommands) -> Self {
        Self {
            usage: Arc::clone(&log.usage),
            log: Mutex::new(log),
        }
    }

    fn log(&self) -> std::sync::MutexGuard<'_, SessionCommands> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn usage(&self) -> std::sync::MutexGuard<'_, SessionUsage> {
        self.usage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stop the log (a refresh holding it saves and reads nothing more)
    /// and take its usage.
    fn close(&self) -> SessionUsage {
        self.log().gone = true;
        std::mem::take(&mut *self.usage())
    }
}

/// What [`CommandBook::refresh_all`] found.
#[derive(Debug, Default)]
pub struct Refreshed {
    /// New commands, oldest first.
    pub commands: Vec<Command>,
    /// New replies (and fallbacks their transcript line replaced), oldest
    /// first.
    pub replies: Vec<AgentReply>,
    pub usage_changed: bool,
}

/// A session's replies, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct ReplyList {
    pub session_id: String,
    /// The conversation being followed.
    pub conversation: Option<String>,
    pub replies: Vec<ReplyView>,
    /// Why they could not be saved (kept in memory, lost at a restart).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_error: Option<String>,
}

/// Which commands [`CommandBook::list`] returns.
#[derive(Debug, Clone, Copy)]
pub struct CommandFilter {
    /// Non-shell blocks too.
    pub all: bool,
    /// At most this many, newest first.
    pub limit: usize,
}

/// A session's commands, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct CommandList {
    pub session_id: String,
    /// The conversation being followed.
    pub conversation: Option<String>,
    /// The file being followed, when one was found.
    pub transcript: Option<PathBuf>,
    pub commands: Vec<Command>,
    /// Why the commands could not be saved (they are kept in memory, and
    /// lost at a restart).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_error: Option<String>,
}

impl CommandBook {
    /// A book saving each session's log under `dir` (`None`: memory only).
    pub fn new(
        claude_config_dir: Option<PathBuf>,
        codex_home: Option<PathBuf>,
        dir: Option<PathBuf>,
    ) -> Self {
        Self {
            claude_config_dir,
            codex_home,
            dir,
            swept: std::sync::atomic::AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
            forgotten: Mutex::new(HashSet::new()),
            removing: Mutex::new(()),
            wake: Mutex::new(None),
            archive: Mutex::new(UsageArchive::default()),
            archive_file: None,
        }
    }

    /// Where session `id`'s log is saved (ids are the hub's own hex, but
    /// anything else is never turned into a path).
    fn file_of(&self, id: &str) -> Option<PathBuf> {
        let safe = !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        self.dir
            .as_deref()
            .filter(|_| safe)
            .map(|d| d.join(format!("{id}.json")))
    }

    fn entry(&self, id: &str) -> Arc<Slot> {
        if self.is_forgotten(id) {
            // Removed: an empty log of its own, never saved or kept.
            return Arc::new(Slot::new(SessionCommands {
                gone: true,
                ..SessionCommands::default()
            }));
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = sessions.get(id) {
            return Arc::clone(entry);
        }
        let entry = Arc::new(Slot::new(SessionCommands::load(self.file_of(id))));
        sessions.insert(id.to_owned(), Arc::clone(&entry));
        entry
    }

    fn is_forgotten(&self, id: &str) -> bool {
        self.forgotten
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(id)
    }

    /// Note what an agent report says about its transcript (Claude hooks
    /// carry `transcript_path`; Codex `notify` the reply itself) and have
    /// the watcher look now.
    pub fn note_report(
        &self,
        session_id: &str,
        kind: SessionKind,
        event: &str,
        payload: &serde_json::Value,
    ) {
        let entry = self.entry(session_id);
        {
            let mut log = entry.log();
            // The agent is active: its file may exist now.
            log.missed = None;
            match kind {
                SessionKind::Claude => {
                    if let Some(path) = payload
                        .get("transcript_path")
                        .and_then(|p| p.as_str())
                        .map(PathBuf::from)
                        .filter(|p| p.is_absolute() && p.extension().is_some_and(|e| e == "jsonl"))
                    {
                        log.hinted = Some(path);
                    }
                }
                SessionKind::Codex if event == "notify" => {
                    let message = payload
                        .get("last-assistant-message")
                        .and_then(|m| m.as_str());
                    // Every reply (a reply of its own; its fences, if any,
                    // commands).
                    if let Some(message) = message.filter(|m| !m.trim().is_empty()) {
                        let turn = payload
                            .get("turn-id")
                            .and_then(|t| t.as_str())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("notify-{}", log.next_seq));
                        if log.notified.len() == MAX_PENDING_NOTIFY {
                            log.notified.pop_front();
                        }
                        log.notified.push_back((turn, message.to_owned()));
                    }
                }
                _ => {}
            }
        }
        self.wake();
    }

    /// A Claude Code turn finished with `payload` (its Stop hook's): its
    /// `last_assistant_message` as a reply, and the blocks in it, when the
    /// transcript has not given them yet (it is written a moment after the
    /// hook). Call after reading the transcript. Returns what was added.
    pub fn note_stop(&self, record: &SessionRecord, payload: &serde_json::Value) -> Refreshed {
        let mut added = Refreshed::default();
        let Some(text) = payload
            .get("last_assistant_message")
            .and_then(|m| m.as_str())
            .filter(|m| !m.trim().is_empty())
        else {
            return added;
        };
        if !record.runs_agent() || record.kind != SessionKind::Claude {
            return added;
        }
        let conversation = record.agent_session_id.clone();
        let entry = self.entry(&record.id);
        let mut log = entry.log();
        if log.gone {
            return added;
        }
        let turn = payload
            .get("prompt_id")
            .and_then(|p| p.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}", log.next_seq));
        let reply = Reply {
            message_id: format!("stop-{turn}"),
            timestamp: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            texts: vec![text.to_owned()],
        };
        // A reply the transcript gave already adds nothing (see
        // [`ReplyLog::add`]).
        log.add_reply(
            &reply,
            conversation.as_deref(),
            CommandSource::Hook,
            &mut added,
        );
        if text.contains("```") || text.contains("~~~") {
            added.commands = log.hook_commands(&reply, conversation.as_deref());
        }
        if !added.commands.is_empty() || !added.replies.is_empty() {
            log.save();
        }
        added
    }

    fn wake(&self) {
        if let Some(wake) = &*self.wake.lock().unwrap_or_else(PoisonError::into_inner) {
            let _ = wake.send(());
        }
    }

    /// Where `record`'s current conversation is written.
    fn locate(&self, record: &SessionRecord, hinted: Option<&Path>) -> Option<(PathBuf, Format)> {
        let conversation = record.agent_session_id.as_deref();
        match record.kind {
            SessionKind::Claude => {
                // The hooks' path, when it is this conversation's (a hint
                // from before `/clear` names the old file).
                if let Some(path) = hinted.filter(|p| {
                    conversation.is_none_or(|c| p.file_stem().and_then(|s| s.to_str()) == Some(c))
                }) && path.is_file()
                {
                    return Some((path.to_path_buf(), Format::Claude));
                }
                let dir = self.claude_config_dir.as_deref()?;
                claude_transcript(dir, &record.cwd, conversation?).map(|p| (p, Format::Claude))
            }
            SessionKind::Codex => {
                let thread = conversation.filter(|c| is_conversation_id(c))?;
                codex_rollout(self.codex_home.as_deref()?, thread).map(|p| (p, Format::Codex))
            }
            SessionKind::Shell => None,
        }
    }

    /// The file of `record`'s current conversation, for search and the
    /// transcript viewer: the one being followed, else the one found
    /// (`None` for shells and while the agent has written none).
    pub(crate) fn transcript_file(&self, record: &SessionRecord) -> Option<(PathBuf, Format)> {
        if !record.kind.is_agent() {
            return None;
        }
        let entry = self.entry(&record.id);
        let hinted = {
            let log = entry.log();
            if let Some(tail) = log
                .tail
                .as_ref()
                .filter(|t| t.conversation == record.agent_session_id)
            {
                return Some((tail.path.clone(), tail.format));
            }
            log.hinted.clone()
        };
        self.locate(record, hinted.as_deref())
    }

    /// Hand `id`'s saved commands to `visit`, newest first, until it
    /// returns false.
    pub(crate) fn visit_commands(&self, id: &str, mut visit: impl FnMut(&Command) -> bool) {
        let entry = self.entry(id);
        let log = entry.log();
        for command in log.entries.iter().rev() {
            if !visit(command) {
                break;
            }
        }
    }

    /// Read whatever `record`'s agent wrote since the last call; returns
    /// the new commands (oldest first).
    pub fn refresh(&self, record: &SessionRecord) -> Vec<Command> {
        self.refresh_all(record).commands
    }

    /// [`Self::refresh`], also saying whether its usage changed. A
    /// transcript not found is looked for again only after
    /// [`LOOKUP_RETRY`] (the watcher polls every session).
    pub fn refresh_all(&self, record: &SessionRecord) -> Refreshed {
        self.refresh_looking(record, false)
    }

    /// [`Self::refresh_all`] for someone asking about this session now:
    /// its transcript is looked for even if a look just missed it (an
    /// agent's first reply creates the file, and whoever asks right after
    /// must find that reply).
    pub fn catch_up(&self, record: &SessionRecord) -> Refreshed {
        self.refresh_looking(record, true)
    }

    fn refresh_looking(&self, record: &SessionRecord, asked: bool) -> Refreshed {
        // A fallback shell (an agent restored without its conversation)
        // writes no transcript: its saved blocks stay listed, unfollowed.
        if !record.runs_agent() {
            return Refreshed::default();
        }
        let entry = self.entry(&record.id);
        let mut log = entry.log();
        if log.gone {
            return Refreshed::default();
        }
        {
            let mut usage = log.usage();
            if usage.workspace_id.as_deref() != Some(record.workspace_id.as_str()) {
                usage.workspace_id = Some(record.workspace_id.clone());
            }
        }
        let conversation = record.agent_session_id.clone();
        let mut added = Refreshed::default();
        let mut switched = false;
        let following = log
            .tail
            .as_ref()
            .is_some_and(|t| t.conversation == conversation);
        if !following {
            let recently_missed = log
                .missed
                .as_ref()
                .is_some_and(|(c, at)| *c == conversation && at.elapsed() < LOOKUP_RETRY);
            if asked || !recently_missed {
                match self.locate(record, log.hinted.as_deref()) {
                    Some((path, format)) => {
                        // Finish the old file first: its last reply may
                        // have landed after the previous look.
                        // Where each stops is kept: going back to that
                        // conversation reads on from there.
                        if let Some(old) = log.tail.take() {
                            let old = log.drain(old, &mut added);
                            log.park(&old);
                        }
                        // ...and its subagents' (they belong to it).
                        for old in std::mem::take(&mut log.subagents) {
                            let old = log.drain(old, &mut added);
                            log.park(&old);
                        }
                        let tail = log.reopen(path, format, conversation.clone());
                        log.tail = Some(tail);
                        log.missed = None;
                        switched = true;
                    }
                    None => log.missed = Some((conversation.clone(), Instant::now())),
                }
            }
        }
        if let Some(tail) = log.tail.take() {
            let tail = log.drain(tail, &mut added);
            if tail.format == Format::Claude {
                log.follow_subagents(&tail.path.clone(), conversation.as_deref());
            }
            log.tail = Some(tail);
        }
        for tail in std::mem::take(&mut log.subagents) {
            let tail = log.drain(tail, &mut added);
            log.subagents.push(tail);
        }
        // Codex replies with no rollout to read: from the notify payload
        // (possibly cut short by the report budget: closed fences only;
        // the rollout's line, once read, replaces the reply).
        let notified: Vec<(String, String)> = log.notified.drain(..).collect();
        if log.tail.is_none() {
            for (turn, text) in notified {
                let reply = Reply {
                    message_id: turn,
                    timestamp: Some(
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    ),
                    texts: vec![text],
                };
                let more = log.add(&reply, conversation.as_deref(), CommandSource::Notify, true);
                push_bounded(&mut added.commands, more);
                log.add_reply(
                    &reply,
                    conversation.as_deref(),
                    CommandSource::Notify,
                    &mut added,
                );
            }
        }
        added.usage_changed = std::mem::take(&mut log.usage_changed);
        if switched
            || !added.commands.is_empty()
            || !added.replies.is_empty()
            || added.usage_changed
        {
            log.save();
        }
        added
    }

    /// A session's token usage (`None` before its first counted call).
    pub fn usage(&self, record: &SessionRecord, today: chrono::NaiveDate) -> Option<UsageView> {
        if !record.kind.is_agent() {
            return None;
        }
        let entry = self.entry(&record.id);
        let usage = entry.usage();
        (!usage.is_empty()).then(|| usage.view(today))
    }

    /// Add `record`'s usage to `rollup`.
    pub fn add_usage(
        &self,
        record: &SessionRecord,
        rollup: &mut RollupBuilder,
        today: chrono::NaiveDate,
    ) {
        if !record.kind.is_agent() {
            return;
        }
        let entry = self.entry(&record.id);
        rollup.add_session(&entry.usage(), today);
    }

    /// Usage of removed sessions.
    pub fn archive(&self) -> UsageArchive {
        self.archive
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keep removed sessions' usage in the archive at `path` (loaded now).
    pub fn with_archive(mut self, path: PathBuf) -> Self {
        *self
            .archive
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = UsageArchive::load(&path);
        self.archive_file = Some(path);
        self
    }

    /// Move removed sessions' usage into the archive and save it.
    fn archive_usage(&self, removed: &[SessionUsage]) {
        let removed: Vec<&SessionUsage> = removed.iter().filter(|u| !u.is_empty()).collect();
        if removed.is_empty() {
            return;
        }
        let mut archive = self.archive.lock().unwrap_or_else(PoisonError::into_inner);
        for usage in removed {
            archive.fold(usage, "");
        }
        if let Some(file) = &self.archive_file {
            let _ = archive.save(file);
        }
    }

    /// A session's commands, newest first.
    pub fn list(&self, record: &SessionRecord, filter: CommandFilter) -> CommandList {
        let entry = self.entry(&record.id);
        let log = entry.log();
        CommandList {
            session_id: record.id.clone(),
            conversation: record.agent_session_id.clone(),
            transcript: log.tail.as_ref().map(|t| t.path.clone()),
            save_error: log.save_error.clone(),
            commands: log
                .entries
                .iter()
                .rev()
                .filter(|c| filter.all || c.shell)
                .take(filter.limit)
                .cloned()
                .collect(),
        }
    }

    /// A session's replies, at most `limit`, newest first.
    pub fn replies(&self, record: &SessionRecord, limit: usize) -> ReplyList {
        let entry = self.entry(&record.id);
        let log = entry.log();
        ReplyList {
            session_id: record.id.clone(),
            conversation: record.agent_session_id.clone(),
            save_error: log.save_error.clone(),
            replies: log
                .replies
                .newest(limit)
                .cloned()
                .map(ReplyView::from)
                .collect(),
        }
    }

    /// Drop (and delete the saved files of) the logs of sessions not in
    /// `keep`, which must be every session the hub has. The first call
    /// also removes files left by sessions deleted while no hub ran.
    pub fn retain(&self, keep: &HashSet<String>) {
        self.retain_existing(&|id| keep.contains(id));
    }

    /// [`Self::retain`], asking `exists` (the hub's store, now) about each
    /// session not followed any more. A session is only ever looked up
    /// after its record exists, so one created since a snapshot of the
    /// store is never taken for removed.
    pub fn retain_existing(&self, exists: &dyn Fn(&str) -> bool) {
        let _removing = self.removing.lock().unwrap_or_else(PoisonError::into_inner);
        let candidates: Vec<String> = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        let gone: Vec<String> = candidates.into_iter().filter(|id| !exists(id)).collect();
        let mut removed = Vec::new();
        for id in gone {
            removed.push(self.close(&id));
        }
        if let Some(dir) = self.dir.as_deref()
            && !self.swept.swap(true, std::sync::atomic::Ordering::Relaxed)
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for path in entries.filter_map(Result::ok).map(|e| e.path()) {
                let stale = path.extension().is_some_and(|e| e == "json")
                    && path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|id| !exists(id));
                if stale {
                    // Removed while no hub ran: its usage is archived too.
                    if let Some(saved) = std::fs::read(&path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
                    {
                        removed.push(saved.usage);
                    }
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        self.archive_usage(&removed);
    }

    /// Forget removed sessions now: their logs go, their usage moves to
    /// the archive (what [`Self::retain`] does on its next round).
    pub fn forget(&self, ids: &[String]) {
        let _removing = self.removing.lock().unwrap_or_else(PoisonError::into_inner);
        let removed: Vec<SessionUsage> = ids.iter().map(|id| self.close(id)).collect();
        self.archive_usage(&removed);
    }

    /// Mark `id` removed, stop its log and delete its file; its usage (to
    /// archive). Called with `removing` held; a second call for one id
    /// returns nothing, so its usage is archived once.
    fn close(&self, id: &str) -> SessionUsage {
        if !self
            .forgotten
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_owned())
        {
            return SessionUsage::default();
        }
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        let usage = match entry {
            Some(entry) => entry.close(),
            // Never loaded by this process: what its file says.
            None => SessionCommands::load(self.file_of(id)).usage().clone(),
        };
        if let Some(file) = self.file_of(id) {
            let _ = std::fs::remove_file(file);
        }
        usage
    }

    /// Save every loaded session's log now, with where reading stopped (a
    /// live handoff: the next hub process reads on from there).
    pub(crate) fn save_all(&self) {
        let entries: Vec<Arc<Slot>> = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for entry in entries {
            entry.log().save();
        }
    }

    /// Stop the watcher (it exits at its next wake).
    pub fn stop(&self) {
        self.wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

fn source_of(format: Format) -> CommandSource {
    match format {
        Format::Claude => CommandSource::Transcript,
        Format::Codex => CommandSource::Rollout,
    }
}

/// Whether a command's timestamp is within [`FRESH_WITHIN`] of now (one
/// without a timestamp counts as fresh).
pub fn is_fresh(command: &Command) -> bool {
    let Some(raw) = command.timestamp.as_deref() else {
        return true;
    };
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(at) => {
            let age = chrono::Utc::now().signed_duration_since(at.with_timezone(&chrono::Utc));
            age.to_std().map_or(true, |age| age <= FRESH_WITHIN)
        }
        Err(_) => true,
    }
}

/// Start the thread that follows running agent sessions' transcripts
/// every [`POLL_INTERVAL`] (and at once when a report nudges it), and
/// sweeps search's idle transcript indexes ([`crate::search::SearchIndex::sweep`]). It ends
/// when the hub is dropped or [`CommandBook::stop`] is called.
pub fn spawn_watcher(hub: &Arc<Hub>) {
    let (wake, woken) = mpsc::channel::<()>();
    *hub.command_book()
        .wake
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(wake);
    let weak: Weak<Hub> = Arc::downgrade(hub);
    let _ = std::thread::Builder::new()
        .name("hub-commands".into())
        .spawn(move || {
            // Ends when the sender is dropped (stop) or the hub is gone.
            while let Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) =
                woken.recv_timeout(POLL_INTERVAL)
            {
                // Coalesce a burst of nudges into one look.
                while woken.try_recv().is_ok() {}
                let Some(hub) = weak.upgrade() else {
                    break;
                };
                let sessions = hub.sessions(None);
                hub.command_book()
                    .retain_existing(&|id| hub.store().session(id).is_ok());
                for view in sessions {
                    let running = view.live.as_ref().is_some_and(|l| l.pid.is_some());
                    if view.record.runs_agent() && running {
                        hub.refresh_commands(&view.record);
                    }
                }
                // Search's transcript indexes: drop the idle ones.
                hub.search_index().sweep();
            }
        });
}

/// What [`Hub::paste`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PasteOutcome {
    /// Sent as a bracketed paste (the terminal had it enabled).
    pub bracketed: bool,
    /// Followed by Enter.
    pub ran: bool,
    pub bytes: usize,
}

/// Refuse a paste that is empty (whitespace only) or over
/// [`MAX_PASTE_BYTES`].
pub(crate) fn check_paste(text: &str) -> Result<(), HubError> {
    if text.trim().is_empty() {
        return Err(StateError::Invalid("nothing to paste".into()).into());
    }
    if text.len() > MAX_PASTE_BYTES {
        return Err(
            StateError::Invalid(format!("paste is larger than {MAX_PASTE_BYTES} bytes")).into(),
        );
    }
    Ok(())
}

impl Hub {
    /// Read `record`'s transcript for new commands and push them to events
    /// subscribers.
    pub(crate) fn refresh_commands(&self, record: &SessionRecord) {
        let refreshed = self.command_book().refresh_all(record);
        self.announce_refreshed(record, refreshed);
    }

    /// [`Hub::refresh_commands`] for a request about this one session:
    /// read up to date even if its transcript was just looked for and not
    /// there yet.
    pub(crate) fn catch_up_commands(&self, record: &SessionRecord) {
        let refreshed = self.command_book().catch_up(record);
        self.announce_refreshed(record, refreshed);
    }

    /// Push what a read of `record`'s transcript (or its Stop hook) added
    /// to events subscribers.
    pub(crate) fn announce_refreshed(&self, record: &SessionRecord, refreshed: Refreshed) {
        if !refreshed.commands.is_empty() {
            self.emit(HubEvent::Commands {
                session_id: record.id.clone(),
                added: refreshed.commands,
            });
        }
        if !refreshed.replies.is_empty() {
            self.emit(HubEvent::Replies {
                session_id: record.id.clone(),
                added: refreshed.replies,
            });
        }
        if refreshed.usage_changed {
            self.emit(HubEvent::Usage {
                session_id: record.id.clone(),
            });
        }
    }

    /// Token usage and cost: the whole hub's and each workspace's, today,
    /// this week, and all time (removed sessions' included).
    pub fn usage_rollups(&self) -> crate::usage::HubUsage {
        let today = chrono::Local::now().date_naive();
        let (workspaces, sessions) = {
            let store = self.store();
            (store.workspaces().to_vec(), store.sessions().to_vec())
        };
        let archive = self.command_book().archive();
        let mut hub = RollupBuilder::default();
        hub.add_archive(&archive, None, today);
        let workspaces = workspaces
            .into_iter()
            .map(|workspace| {
                let mut rollup = RollupBuilder::default();
                rollup.add_archive(&archive, Some(&workspace.id), today);
                for record in sessions.iter().filter(|s| s.workspace_id == workspace.id) {
                    self.command_book().add_usage(record, &mut rollup, today);
                    self.command_book().add_usage(record, &mut hub, today);
                }
                crate::usage::WorkspaceUsage {
                    workspace_id: workspace.id,
                    name: workspace.name,
                    usage: rollup.build(),
                }
            })
            .collect();
        crate::usage::HubUsage {
            hub: hub.build(),
            workspaces,
            day: today,
            week_start: crate::usage::week_start(today),
            price_source: crate::usage::PRICE_SOURCE,
            prices_as_of: crate::usage::PRICE_AS_OF,
        }
    }

    /// A session's suggested commands, newest first (read up to date
    /// first, whether or not the session runs).
    pub fn commands(&self, id: &str, filter: CommandFilter) -> Result<CommandList, HubError> {
        let record = self.session(id)?.record;
        self.catch_up_commands(&record);
        Ok(self.command_book().list(&record, filter))
    }

    /// A session's replies, at most `limit`, newest first (read up to date
    /// first, whether or not the session runs).
    pub fn replies(&self, id: &str, limit: usize) -> Result<ReplyList, HubError> {
        let record = self.session(id)?.record;
        self.catch_up_commands(&record);
        Ok(self.command_book().replies(&record, limit))
    }

    /// Paste `text` into a running session's terminal, as a bracketed
    /// paste when its program enabled that; Enter only when `run`. A
    /// multi-line paste into a terminal without bracketed paste would run
    /// each line, so it is refused unless `run`. It counts as typing into
    /// the session (a prompt queue waits it out), and no other paste
    /// writes between its text and its Enter.
    pub fn paste(&self, id: &str, text: &str, run: bool) -> Result<PasteOutcome, HubError> {
        let _turn = self.queue_runtime().paste_turn();
        self.paste_turn_held(id, text, run, true)
    }

    /// [`Hub::paste`] with the paste turn already taken; `typing`: note
    /// it as typing (a queue's own delivery is not).
    pub(crate) fn paste_turn_held(
        &self,
        id: &str,
        text: &str,
        run: bool,
        typing: bool,
    ) -> Result<PasteOutcome, HubError> {
        let record = self.session(id)?.record;
        check_paste(text)?;
        let session = self
            .live_session(id)
            .filter(|s| s.exit_info().is_none())
            .ok_or_else(|| {
                StateError::Conflict(format!("session {:?} is not running", record.name))
            })?;
        let bracketed = session.bracketed_paste();
        let multiline = text.trim_end_matches(['\n', '\r']).contains(['\n', '\r']);
        if multiline && !bracketed && !run {
            return Err(StateError::Conflict(format!(
                "{:?} has not enabled bracketed paste, so pasting several lines would run each one as it arrives; copy it instead and paste it where it is safe",
                record.name
            ))
            .into());
        }
        // A trailing newline is never an implicit Enter: only `run` runs.
        let bytes = paste_bytes(text.trim_end_matches(['\n', '\r']), bracketed);
        if typing {
            self.note_typing(id, &bytes);
            if run {
                self.note_typing(id, b"\r");
            }
        }
        let mut len = bytes.len();
        let mut outcome = session.write(bytes);
        if run && outcome == WriteOutcome::Queued {
            // Enter as its own keypress, as `fw hub send` does (a TUI
            // takes one fast burst as part of the paste).
            std::thread::sleep(ENTER_DELAY);
            outcome = session.write(b"\r".to_vec());
            len += 1;
        }
        if run && outcome == WriteOutcome::Queued {
            self.nudge_badge_soon(&record);
        }
        match outcome {
            WriteOutcome::Queued => Ok(PasteOutcome {
                bracketed,
                ran: run,
                bytes: len,
            }),
            WriteOutcome::Closed => Err(StateError::Conflict(format!(
                "session {:?} is not running",
                record.name
            ))
            .into()),
            WriteOutcome::Backlogged => Err(StateError::Conflict(format!(
                "{:?} is not reading its input; nothing was sent",
                record.name
            ))
            .into()),
            WriteOutcome::HandingOff => Err(StateError::Conflict(format!(
                "the hub is handing {:?} over to a new process; nothing was sent, retry",
                record.name
            ))
            .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;
    use serde_json::json;
    use std::io::Write;

    /// Append `bytes` to `path` in pieces of `size`, reading `tail` after
    /// each; everything it handed out.
    fn read_in_pieces(path: &Path, bytes: &[u8], size: usize, tail: &mut Tail) -> Vec<Item> {
        let mut items = Vec::new();
        for piece in bytes.chunks(size) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            file.write_all(piece).unwrap();
            tail.read(&mut |item| items.push(item));
        }
        items
    }

    #[test]
    fn captured_transcripts_give_the_golden_replies_usage_and_commands() {
        use std::fmt::Write as _;
        for &(name, codex) in crate::test_golden::TRANSCRIPTS {
            let format = if codex { Format::Codex } else { Format::Claude };
            let path = crate::test_golden::fixture(name);
            let conversation = Some("conv".to_owned());
            let mut items = Vec::new();
            Tail::new(path.clone(), format, conversation.clone()).read(&mut |i| items.push(i));
            // The same file appended to in pieces (lines split anywhere)
            // reads the same.
            let dir = tempfile::tempdir().unwrap();
            let copy = dir.path().join(name);
            let bytes = std::fs::read(&path).unwrap();
            for size in [1, 7, 64, 4096] {
                let _ = std::fs::remove_file(&copy);
                let mut tail = Tail::new(copy.clone(), format, conversation.clone());
                assert_eq!(
                    read_in_pieces(&copy, &bytes, size, &mut tail),
                    items,
                    "{name} by {size}"
                );
            }
            // A subagent's tail sees only the usage.
            let mut usage_only = Vec::new();
            Tail::usage_only(path.clone(), conversation.clone()).read(&mut |i| usage_only.push(i));
            let usage: Vec<&Item> = items
                .iter()
                .filter(|i| matches!(i, Item::Usage(_)))
                .collect();
            if !codex {
                assert_eq!(usage_only.iter().collect::<Vec<_>>(), usage, "{name}");
            }
            let mut log = SessionCommands::default();
            let mut added = Refreshed::default();
            log.drain(Tail::new(path, format, conversation), &mut added);
            let mut out = String::new();
            for item in &items {
                writeln!(out, "{item:?}").unwrap();
            }
            for command in &added.commands {
                writeln!(out, "{command:?}").unwrap();
            }
            for reply in added.replies {
                writeln!(out, "{:?}", ReplyView::from(reply)).unwrap();
            }
            crate::test_golden::assert_golden(&format!("{name}.commands.txt"), &out);
        }
    }

    fn record(kind: SessionKind, cwd: &Path, conversation: &str) -> SessionRecord {
        SessionRecord {
            id: "s1".into(),
            workspace_id: "w1".into(),
            name: "agent".into(),
            kind,
            cwd: cwd.to_path_buf(),
            command: vec!["x".into()],
            agent_session_id: Some(conversation.into()),
            created_at: chrono::Utc::now(),
            state: SessionState::Running,
            fallback_command: None,
            worktree: None,
            companion_of: None,
            launch: None,
            reviewer: None,
            review_of: None,
            account: None,
        }
    }

    fn claude_line(uuid: &str, text: &str) -> String {
        json!({
            "type": "assistant",
            "uuid": uuid,
            "timestamp": "2026-11-02T16:20:40.174Z",
            "isSidechain": false,
            "message": {"id": "msg_1", "role": "assistant", "content": [{"type": "text", "text": text}]},
        })
        .to_string()
            + "\n"
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn claude_setup(
        conversation: &str,
    ) -> (tempfile::TempDir, CommandBook, SessionRecord, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("claude");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = config
            .join("projects")
            .join(crate::agent::claude_project_dir_name(&cwd));
        std::fs::create_dir_all(&project).unwrap();
        let path = project.join(format!("{conversation}.jsonl"));
        let book = CommandBook::new(Some(config), None, None);
        let record = record(SessionKind::Claude, &cwd, conversation);
        (dir, book, record, path)
    }

    #[test]
    fn a_transcript_created_after_a_miss_is_found_by_whoever_asks() {
        let (_dir, book, record, path) = claude_setup("c1");
        // The watcher looks before the agent has written anything...
        assert!(book.refresh(&record).is_empty(), "no transcript yet");
        // ...then its first reply creates the file.
        append(&path, &claude_line("u1", "Run:\n```bash\nmake check\n```"));
        append(&path, "\n");
        // A poll backs off from the recent miss (the watcher's next look
        // reads it within LOOKUP_RETRY)...
        assert!(book.refresh(&record).is_empty(), "the poll backs off");
        // ...but a request about this session reads it now.
        let added = book.catch_up(&record).commands;
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].command, "make check");
        assert!(
            book.catch_up(&record).commands.is_empty(),
            "nothing re-read"
        );
    }

    #[test]
    fn claude_transcript_tails_incrementally_across_partial_lines() {
        let (_dir, book, record, path) = claude_setup("c1");
        assert!(book.refresh(&record).is_empty(), "no transcript yet");
        let heredoc = "cat <<'EOF' | sort | uniq -c\nb\na\nEOF";
        let line = claude_line(
            "u1",
            &format!("Try:\n```bash\n{heredoc}\n```\nand\n```json\n{{}}\n```"),
        );
        // Half a line: nothing yet.
        let (head, rest) = line.split_at(40);
        append(
            &path,
            &json!({"type": "user", "message": {"content": "```x```"}}).to_string(),
        );
        append(&path, "\n");
        append(&path, head);
        // The first refresh missed the file; a miss is retried only after
        // LOOKUP_RETRY, so forget it rather than wait.
        book.entry("s1").log().missed = None;
        assert!(book.refresh(&record).is_empty());
        append(&path, rest);
        let added = book.refresh(&record);
        assert_eq!(added.len(), 2);
        assert_eq!(added[0].text, heredoc);
        assert_eq!(added[0].command, heredoc);
        assert!(added[0].shell);
        assert_eq!(added[0].lang.as_deref(), Some("bash"));
        assert_eq!(added[0].message_id, "u1");
        assert_eq!((added[0].block_index, added[1].block_index), (0, 1));
        assert!(!added[1].shell);
        assert!(book.refresh(&record).is_empty(), "nothing re-read");

        let shell_only = book.list(
            &record,
            CommandFilter {
                all: false,
                limit: 50,
            },
        );
        assert_eq!(shell_only.commands.len(), 1);
        assert_eq!(shell_only.transcript.as_deref(), Some(path.as_path()));
        let all = book.list(
            &record,
            CommandFilter {
                all: true,
                limit: 50,
            },
        );
        assert_eq!(
            all.commands[0].lang.as_deref(),
            Some("json"),
            "newest first"
        );
    }

    #[test]
    fn inline_bang_commands_follow_the_fences_once_hook_or_transcript() {
        let (_dir, book, record, path) = claude_setup("c1");
        let reply = "Run `! gcloud auth login`, then:\n```bash\nls\n```";
        // The Stop hook gives the reply first; its transcript line adds
        // nothing more.
        let hooked = book
            .note_stop(&record, &json!({"last_assistant_message": reply}))
            .commands;
        assert_eq!(hooked.len(), 2);
        assert_eq!((hooked[0].text.as_str(), hooked[0].block_index), ("ls", 0));
        assert_eq!(hooked[1].text, "! gcloud auth login");
        assert_eq!(hooked[1].command, "gcloud auth login");
        assert_eq!(hooked[1].block_index, 1);
        assert!(hooked[1].shell);
        append(&path, &claude_line("u1", reply));
        book.entry("s1").log().missed = None;
        assert!(book.refresh(&record).is_empty(), "no duplicates");
        // A reply with only an inline one comes from the transcript.
        let next = claude_line("u2", "Type `! gh auth login` here.").replace("msg_1", "msg_2");
        append(&path, &next);
        let added = book.refresh(&record);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].command, "gh auth login");
    }

    #[test]
    fn rotation_rereads_without_duplicates_and_clear_switches_files() {
        let (_dir, book, mut record, path) = claude_setup("c1");
        append(&path, &claude_line("u1", "```sh\nls\n```"));
        assert_eq!(book.refresh(&record).len(), 1);
        // Replaced by a new file (new inode) holding the old line plus one.
        let replacement = path.with_extension("tmp");
        append(&replacement, &claude_line("u1", "```sh\nls\n```"));
        append(&replacement, &claude_line("u2", "```sh\npwd\n```"));
        std::fs::rename(&replacement, &path).unwrap();
        let added = book.refresh(&record);
        assert_eq!(
            added.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["pwd"]
        );
        // Truncated and rewritten shorter.
        std::fs::write(&path, claude_line("u3", "```sh\nid\n```")).unwrap();
        assert_eq!(book.refresh(&record)[0].text, "id");

        // `/clear`: a new conversation id and file, named by the hooks.
        let new_path = path.with_file_name("c2.jsonl");
        append(&path, &claude_line("u4", "```sh\nlast of c1\n```"));
        append(&new_path, &claude_line("v1", "```sh\nfirst of c2\n```"));
        book.note_report(
            "s1",
            SessionKind::Claude,
            "SessionStart",
            &json!({"session_id": "c2", "transcript_path": new_path}),
        );
        record.agent_session_id = Some("c2".into());
        let added = book.refresh(&record);
        let texts: Vec<_> = added
            .iter()
            .map(|c| (c.text.as_str(), c.conversation.as_deref()))
            .collect();
        assert_eq!(
            texts,
            [("last of c1", Some("c1")), ("first of c2", Some("c2"))]
        );
        let list = book.list(
            &record,
            CommandFilter {
                all: false,
                limit: 2,
            },
        );
        assert_eq!(list.commands.len(), 2);
        assert_eq!(list.transcript.as_deref(), Some(new_path.as_path()));
        assert!(list.commands[0].seq > list.commands[1].seq);
    }

    #[test]
    fn overlong_lines_and_sidechains_are_skipped_and_memory_is_bounded() {
        let (_dir, book, record, path) = claude_setup("c1");
        let huge = format!(
            "{{\"type\":\"user\",\"x\":\"```{}\"}}\n",
            "a".repeat(MAX_LINE_BYTES + 10)
        );
        append(&path, &huge);
        let mut side: serde_json::Value =
            serde_json::from_str(&claude_line("s", "```sh\nsecret\n```")).unwrap();
        side["isSidechain"] = json!(true);
        append(&path, &(side.to_string() + "\n"));
        append(&path, &claude_line("u1", "```sh\nok\n```"));
        let added = book.refresh(&record);
        assert_eq!(
            added.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["ok"]
        );
        let entry = book.entry("s1");
        assert!(
            entry.log().tail.as_ref().unwrap().pending.capacity() <= MAX_LINE_BYTES + READ_CHUNK
        );

        for i in 0..(MAX_COMMANDS + 20) {
            append(
                &path,
                &claude_line(&format!("m{i}"), &format!("```sh\necho {i}\n```")),
            );
        }
        book.refresh(&record);
        let all = book.list(
            &record,
            CommandFilter {
                all: true,
                limit: usize::MAX,
            },
        );
        assert_eq!(all.commands.len(), MAX_COMMANDS);
        assert_eq!(all.commands[0].text, format!("echo {}", MAX_COMMANDS + 19));
    }

    #[test]
    fn codex_rollout_is_found_and_notify_falls_back_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        let thread = "01a07c85-9201-7123-8801-041350ceed6a";
        let book = CommandBook::new(None, Some(home.clone()), None);
        let record = record(SessionKind::Codex, dir.path(), thread);
        // No rollout: the notify reply, closed fences only.
        book.note_report(
            "s1",
            SessionKind::Codex,
            "notify",
            &json!({
                "type": "agent-turn-complete",
                "thread-id": thread,
                "turn-id": "t1",
                "last-assistant-message": "Run:\n```bash\nmake test\n```\nthen\n```bash\ncut sho",
            }),
        );
        let added = book.refresh(&record);
        assert_eq!(
            added.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["make test"]
        );
        assert_eq!(added[0].source, CommandSource::Notify);

        // The rollout appears: its copy of that block is not repeated.
        let day = home.join("sessions/2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        let older = home.join("sessions/2026/01/01");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::write(older.join("rollout-2026-01-01T00-00-00-other.jsonl"), "").unwrap();
        let rollout = day.join(format!("rollout-2026-11-02T10-00-00-{thread}.jsonl"));
        // As codex-rs writes them: no item id, and items recorded in one
        // batch share a millisecond timestamp.
        let item = |text: &str| {
            json!({
                "timestamp": "2026-11-02T10:00:01.000Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]},
            })
            .to_string()
                + "\n"
        };
        append(
            &rollout,
            &json!({"type": "session_meta", "payload": {"id": thread}}).to_string(),
        );
        append(&rollout, "\n");
        append(&rollout, &item("Run:\n```bash\nmake test\n```"));
        append(&rollout, &item("```zsh\nls -la\n```"));
        // Old-style bare items (no timestamp either) and user messages.
        let bare = |role: &str, kind: &str, text: &str| {
            json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]})
                .to_string()
                + "\n"
        };
        append(&rollout, &bare("user", "input_text", "```sh\nno\n```"));
        append(
            &rollout,
            &bare("assistant", "output_text", "```sh\nuptime\n```"),
        );
        append(
            &rollout,
            &bare("assistant", "output_text", "```sh\nhostname\n```"),
        );
        assert_eq!(
            codex_rollout(&home, thread).as_deref(),
            Some(rollout.as_path())
        );
        book.entry("s1").log().missed = None;
        let added = book.refresh(&record);
        assert_eq!(
            added.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["ls -la", "uptime", "hostname"]
        );
        assert_eq!(added[0].source, CommandSource::Rollout);
        // Read again from the start (a replaced file): nothing repeats.
        let copy = rollout.with_extension("tmp");
        std::fs::copy(&rollout, &copy).unwrap();
        std::fs::rename(&copy, &rollout).unwrap();
        assert!(book.refresh(&record).is_empty());
        assert_eq!(
            added[0].timestamp.as_deref(),
            Some("2026-11-02T10:00:01.000Z")
        );
    }

    #[test]
    fn commands_survive_a_restart_with_their_seq_across_a_clear() {
        let (dir, _, mut record, path) = claude_setup("c1");
        let config = dir.path().join("claude");
        let store = dir.path().join("state").join(COMMANDS_DIR);
        let book = CommandBook::new(Some(config.clone()), None, Some(store.clone()));
        append(&path, &claude_line("u1", "```sh\ncat <<'EOF'\nx\nEOF\n```"));
        assert_eq!(book.refresh(&record).len(), 1);
        // `/clear`: the next conversation's file.
        let new_path = path.with_file_name("c2.jsonl");
        append(&new_path, &claude_line("v1", "```sh\nprintf x\n```"));
        record.agent_session_id = Some("c2".into());
        assert_eq!(book.refresh(&record).len(), 1);
        let before = book
            .list(
                &record,
                CommandFilter {
                    all: true,
                    limit: 50,
                },
            )
            .commands;
        drop(book);

        // A new hub on the same state dir: same blocks, same seq, and the
        // tail resumes (nothing is read twice; new lines still arrive).
        let book = CommandBook::new(Some(config.clone()), None, Some(store.clone()));
        let after = book
            .list(
                &record,
                CommandFilter {
                    all: true,
                    limit: 50,
                },
            )
            .commands;
        assert_eq!(after, before);
        assert_eq!(
            after
                .iter()
                .map(|c| (c.seq, c.conversation.as_deref()))
                .collect::<Vec<_>>(),
            [(1, Some("c2")), (0, Some("c1"))]
        );
        assert!(book.refresh(&record).is_empty());
        append(&new_path, &claude_line("v2", "```sh\nid\n```"));
        let added = book.refresh(&record);
        assert_eq!((added[0].seq, added[0].text.as_str()), (2, "id"));

        // A deleted session's file goes; so do strays from a hub that
        // was not running when theirs were deleted.
        std::fs::write(store.join("gone.json"), "{}").unwrap();
        book.retain(&HashSet::from(["s1".to_owned()]));
        assert!(store.join("s1.json").is_file());
        assert!(!store.join("gone.json").exists());
        book.retain(&HashSet::new());
        assert!(!store.join("s1.json").exists());
    }

    #[test]
    fn replies_follow_hook_transcript_clear_and_restart_without_duplicates() {
        let (dir, _, mut record, path) = claude_setup("c1");
        let config = dir.path().join("claude");
        let store = dir.path().join("state").join(COMMANDS_DIR);
        let book = CommandBook::new(Some(config.clone()), None, Some(store.clone()));
        let texts = |list: &ReplyList| -> Vec<(u64, String)> {
            list.replies
                .iter()
                .map(|r| (r.reply.seq, r.reply.text.clone()))
                .collect()
        };
        // Text between tool calls, then a tool call: one reply, no command.
        append(&path, &claude_line("u1", "Let me look."));
        append(
            &path,
            &json!({"type": "assistant", "uuid": "u2", "message": {"id": "msg_2", "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}]}})
            .to_string(),
        );
        append(&path, "\n");
        let added = book.refresh_all(&record);
        assert!(added.commands.is_empty());
        assert_eq!(added.replies.len(), 1);
        assert_eq!(added.replies[0].text, "Let me look.");
        // The turn ends: its Stop hook carries the final reply (cut short
        // to fit the report) before the transcript has it...
        let final_text = "Here is the message:\n\n- first\n- second\n\nThanks.";
        let hooked = book.note_stop(
            &record,
            &json!({"last_assistant_message": &final_text[..30], "prompt_id": "p1"}),
        );
        assert_eq!(hooked.replies.len(), 1);
        assert_eq!(hooked.replies[0].source, CommandSource::Hook);
        // ...and its line, once written, takes the hook reply's place.
        append(
            &path,
            &claude_line("u3", final_text).replace("msg_1", "msg_3"),
        );
        let added = book.refresh_all(&record);
        assert_eq!(added.replies.len(), 1);
        assert_eq!(added.replies[0].seq, hooked.replies[0].seq);
        assert_eq!(added.replies[0].text, final_text);
        assert_eq!(added.replies[0].source, CommandSource::Transcript);
        // A late hook for a reply read already adds nothing.
        assert!(
            book.note_stop(&record, &json!({"last_assistant_message": final_text}))
                .replies
                .is_empty()
        );
        // `/clear`: the next conversation's replies follow.
        let new_path = path.with_file_name("c2.jsonl");
        append(&new_path, &claude_line("v1", "Fresh start."));
        record.agent_session_id = Some("c2".into());
        assert_eq!(book.refresh_all(&record).replies.len(), 1);
        let before = book.replies(&record, MAX_REPLIES);
        assert_eq!(
            texts(&before),
            [
                (2, "Fresh start.".to_owned()),
                (1, final_text.to_owned()),
                (0, "Let me look.".to_owned())
            ]
        );
        assert_eq!(before.replies[1].blocks.len(), 3);
        assert_eq!(texts(&book.replies(&record, 1)).len(), 1);
        drop(book);

        // A new hub on the same state dir: the same replies, nothing read
        // twice, the next one numbered on.
        let book = CommandBook::new(Some(config), None, Some(store));
        assert_eq!(texts(&book.replies(&record, MAX_REPLIES)), texts(&before));
        assert!(book.refresh_all(&record).replies.is_empty());
        append(&new_path, &claude_line("v2", "Next."));
        let added = book.refresh_all(&record);
        assert_eq!(
            (added.replies[0].seq, added.replies[0].text.as_str()),
            (3, "Next.")
        );
    }

    #[test]
    fn codex_notify_replies_are_replaced_by_their_rollout_lines() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        let thread = "01a07c85-9201-7123-8801-041350ceed6a";
        let book = CommandBook::new(None, Some(home.clone()), None);
        let record = record(SessionKind::Codex, dir.path(), thread);
        // No rollout: the notify payload's reply (no fences: a reply only).
        book.note_report(
            "s1",
            SessionKind::Codex,
            "notify",
            &json!({"turn-id": "t1", "last-assistant-message": "All done: tests"}),
        );
        let added = book.refresh_all(&record);
        assert!(added.commands.is_empty());
        assert_eq!(added.replies.len(), 1);
        assert_eq!(added.replies[0].source, CommandSource::Notify);
        let day = home.join("sessions/2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join(format!("rollout-2026-11-02T10-00-00-{thread}.jsonl"));
        append(
            &rollout,
            &(json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "All done: tests pass."}]}})
            .to_string()
                + "\n"),
        );
        book.entry("s1").log().missed = None;
        let added = book.refresh_all(&record);
        assert_eq!(added.replies.len(), 1);
        assert_eq!(added.replies[0].seq, 0);
        assert_eq!(added.replies[0].text, "All done: tests pass.");
        assert_eq!(added.replies[0].source, CommandSource::Rollout);
        assert_eq!(book.replies(&record, MAX_REPLIES).replies.len(), 1);
    }

    #[test]
    fn a_long_backlog_reports_only_the_newest_commands() {
        let (_dir, book, record, path) = claude_setup("c1");
        let mut backlog = String::new();
        for i in 0..(MAX_COMMANDS * 2) {
            backlog.push_str(&claude_line(
                &format!("m{i}"),
                &format!("```sh\necho {i}\n```"),
            ));
        }
        append(&path, &backlog);
        let added = book.refresh(&record);
        assert_eq!(added.len(), MAX_COMMANDS);
        assert_eq!(
            added.last().unwrap().text,
            format!("echo {}", MAX_COMMANDS * 2 - 1)
        );
    }

    fn usage_line(id: &str, model: &str, input: u64, output: u64) -> String {
        json!({
            "type": "assistant",
            "uuid": format!("u-{id}"),
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "message": {"id": id, "model": model, "role": "assistant",
                "content": [{"type": "tool_use", "name": "Bash", "input": {}}],
                "usage": {"input_tokens": input, "output_tokens": output,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}},
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn claude_usage_is_counted_with_subagents_saved_and_archived_on_removal() {
        let (dir, _, record, path) = claude_setup("c1");
        let config = dir.path().join("claude");
        let store = dir.path().join("state").join(COMMANDS_DIR);
        let archive_file = dir.path().join("state").join(crate::usage::ARCHIVE_FILE);
        let open = || {
            CommandBook::new(Some(config.clone()), None, Some(store.clone()))
                .with_archive(archive_file.clone())
        };
        let book = open();
        let today = chrono::Local::now().date_naive();
        assert!(book.usage(&record, today).is_none(), "nothing counted yet");
        // A reply line without a fence still counts; its repeat does not.
        append(&path, &usage_line("m1", "claude-sonnet-5-5", 1000, 100));
        append(&path, &usage_line("m1", "claude-sonnet-5-5", 1000, 100));
        let refreshed = book.refresh_all(&record);
        assert!(refreshed.commands.is_empty() && refreshed.usage_changed);
        assert!(!book.refresh_all(&record).usage_changed, "nothing new");
        // A subagent's transcript beside the conversation's.
        let subagents = path.with_file_name("c1").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        append(
            &subagents.join("agent-a1.jsonl"),
            &usage_line("s1", "claude-haiku-5-5", 500, 50),
        );
        // Its fenced text is not a suggestion of the session's.
        append(
            &subagents.join("agent-a1.jsonl"),
            &claude_line("x", "```sh\nrm -rf /\n```"),
        );
        let refreshed = book.refresh_all(&record);
        assert!(refreshed.usage_changed);
        assert!(refreshed.commands.is_empty());
        let usage = book.usage(&record, today).unwrap();
        assert_eq!(usage.turns, 2);
        assert_eq!(usage.total.tokens.input, 1500);
        assert_eq!(usage.today.tokens.output, 150);
        let expected = (1000.0 * 2.0 + 100.0 * 10.0 + 500.0 * 0.10 + 50.0 * 0.50) / 1e6;
        assert!((usage.total.cost_usd - expected).abs() < 1e-12);
        drop(book);

        // Saved: a new hub has it, and reads on from where it stopped.
        let book = open();
        assert_eq!(book.usage(&record, today).unwrap().turns, 2);
        append(&path, &usage_line("m2", "claude-sonnet-5-5", 10, 1));
        assert!(book.refresh_all(&record).usage_changed);
        assert_eq!(book.usage(&record, today).unwrap().turns, 3);

        // Removed: the archive keeps it for the rollups.
        book.retain(&HashSet::new());
        let mut hub = RollupBuilder::default();
        hub.add_archive(&open().archive(), None, today);
        assert_eq!(hub.build().today.tokens.input, 1510);
        assert_eq!(hub.build().total.tokens.output, 151);
        let mut workspace = RollupBuilder::default();
        workspace.add_archive(&open().archive(), Some("w1"), today);
        assert_eq!(workspace.build().total.tokens.input, 1510);
    }

    #[test]
    fn codex_rollout_token_counts_are_counted_by_their_growth() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        let thread = "01a07c85-9201-7123-8801-041350ceed6a";
        let book = CommandBook::new(None, Some(home.clone()), None);
        let record = record(SessionKind::Codex, dir.path(), thread);
        let day = home.join("sessions/2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join(format!("rollout-2026-11-02T10-00-00-{thread}.jsonl"));
        let now = chrono::Utc::now().to_rfc3339();
        let count = |input: u64, cached: u64, output: u64| {
            json!({"timestamp": now, "type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": input, "cached_input_tokens": cached, "output_tokens": output},
            }}})
            .to_string()
                + "\n"
        };
        append(
            &rollout,
            &(json!({"type": "turn_context", "payload": {"model": "gpt-5-codex"}}).to_string()
                + "\n"),
        );
        append(&rollout, &count(2000, 500, 100));
        assert!(book.refresh_all(&record).usage_changed);
        append(&rollout, &count(2000, 500, 100));
        assert!(
            !book.refresh_all(&record).usage_changed,
            "a repeated total adds nothing"
        );
        append(&rollout, &count(5000, 2500, 300));
        assert!(book.refresh_all(&record).usage_changed);
        let usage = book
            .usage(&record, chrono::Local::now().date_naive())
            .unwrap();
        assert_eq!(usage.turns, 2);
        assert_eq!(usage.total.tokens.input, 2500);
        assert_eq!(usage.total.tokens.cache_read, 2500);
        assert_eq!(usage.total.tokens.output, 300);
        let expected = (2500.0 * 1.25 + 2500.0 * 0.125 + 300.0 * 10.0) / 1e6;
        assert!((usage.total.cost_usd - expected).abs() < 1e-12);
        assert!(usage.total.priced);
    }

    #[test]
    fn a_refresh_racing_a_removal_does_not_count_the_session_twice() {
        let (dir, _, record, path) = claude_setup("c1");
        let config = dir.path().join("claude");
        let store = dir.path().join("state").join(COMMANDS_DIR);
        let archive_file = dir.path().join("state").join(crate::usage::ARCHIVE_FILE);
        let book =
            CommandBook::new(Some(config), None, Some(store.clone())).with_archive(archive_file);
        append(&path, &usage_line("m1", "claude-sonnet-5-5", 1000, 100));
        assert!(book.refresh_all(&record).usage_changed);
        // A refresh already holding the session's log when it is removed.
        let held = book.entry(&record.id);
        book.forget(std::slice::from_ref(&record.id));
        assert!(held.log().gone, "the held log is stopped");
        held.log().save();
        // The watcher, working from a snapshot taken before the removal.
        let refreshed = book.refresh_all(&record);
        assert!(!refreshed.usage_changed && refreshed.commands.is_empty());
        assert!(
            !store.join("s1.json").exists(),
            "a removed session's log is not saved again"
        );
        book.retain(&HashSet::from([record.id.clone()]));
        book.retain(&HashSet::new());
        book.forget(std::slice::from_ref(&record.id));
        assert!(
            book.usage(&record, chrono::Local::now().date_naive())
                .is_none()
        );
        let mut hub = RollupBuilder::default();
        hub.add_archive(&book.archive(), None, chrono::Local::now().date_naive());
        let total = hub.build().total;
        assert_eq!(total.tokens.input, 1000, "archived once");
        assert_eq!(total.tokens.output, 100);
    }

    #[test]
    fn going_back_to_a_conversation_reads_on_instead_of_counting_it_again() {
        let (dir, _, mut record, first) = claude_setup("c1");
        let config = dir.path().join("claude");
        let store = dir.path().join("state").join(COMMANDS_DIR);
        let open = || CommandBook::new(Some(config.clone()), None, Some(store.clone()));
        let book = open();
        let today = chrono::Local::now().date_naive();
        // More responses than are remembered to replace a repeat.
        let mut backlog = String::new();
        for i in 0..(crate::usage::RECENT_TURNS + 100) {
            backlog.push_str(&usage_line(&format!("a{i}"), "claude-sonnet-5-5", 10, 1));
        }
        append(&first, &backlog);
        book.refresh_all(&record);
        let counted = book.usage(&record, today).unwrap().turns;
        assert_eq!(counted, (crate::usage::RECENT_TURNS + 100) as u64);
        // /clear into c2, then back to c1 (the restarted hub too).
        let second = first.with_file_name("c2.jsonl");
        append(&second, &usage_line("b1", "claude-sonnet-5-5", 10, 1));
        record.agent_session_id = Some("c2".into());
        book.refresh_all(&record);
        assert_eq!(book.usage(&record, today).unwrap().turns, counted + 1);
        drop(book);
        let book = open();
        record.agent_session_id = Some("c1".into());
        book.refresh_all(&record);
        assert_eq!(book.usage(&record, today).unwrap().turns, counted + 1);
        // What c1 adds after the return still counts.
        append(&first, &usage_line("a-new", "claude-sonnet-5-5", 10, 1));
        assert!(book.refresh_all(&record).usage_changed);
        assert_eq!(book.usage(&record, today).unwrap().turns, counted + 2);
        record.agent_session_id = Some("c2".into());
        assert!(!book.refresh_all(&record).usage_changed);
    }

    #[test]
    fn a_codex_rollout_followed_again_adds_only_its_growth() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        let a = "01a07c85-9201-7123-8801-041350ceed6a";
        let b = "01a07c85-9201-7123-8801-041350ceed6b";
        let book = CommandBook::new(None, Some(home.clone()), None);
        let mut record = record(SessionKind::Codex, dir.path(), a);
        let day = home.join("sessions/2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        let rollout =
            |thread: &str| day.join(format!("rollout-2026-11-02T10-00-00-{thread}.jsonl"));
        let count = |input: u64, output: u64| {
            json!({"timestamp": chrono::Utc::now().to_rfc3339(), "type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": input, "cached_input_tokens": 0, "output_tokens": output},
            }}})
            .to_string()
                + "\n"
        };
        append(&rollout(a), &count(1000, 100));
        append(&rollout(b), &count(50, 5));
        let today = chrono::Local::now().date_naive();
        book.refresh_all(&record);
        record.agent_session_id = Some(b.into());
        book.refresh_all(&record);
        record.agent_session_id = Some(a.into());
        assert!(!book.refresh_all(&record).usage_changed, "a's total again");
        append(&rollout(a), &count(1200, 150));
        assert!(book.refresh_all(&record).usage_changed);
        let usage = book.usage(&record, today).unwrap();
        assert_eq!(usage.total.tokens.input, 1250);
        assert_eq!(usage.total.tokens.output, 155);
    }

    #[test]
    fn usage_is_read_while_a_drain_holds_the_log() {
        let (_dir, book, record, path) = claude_setup("c1");
        append(&path, &usage_line("m1", "claude-sonnet-5-5", 1000, 100));
        book.refresh_all(&record);
        let book = Arc::new(book);
        let slot = book.entry(&record.id);
        let _draining = slot.log();
        let (tx, rx) = mpsc::channel();
        let reader = Arc::clone(&book);
        let listed = record.clone();
        std::thread::spawn(move || {
            let _ = tx.send(reader.usage(&listed, chrono::Local::now().date_naive()));
        });
        let usage = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("usage waited on the transcript lock");
        assert_eq!(usage.unwrap().turns, 1);
    }

    #[test]
    fn freshness_reads_the_agent_timestamp() {
        let mut command = Command {
            seq: 0,
            conversation: None,
            message_id: "m".into(),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
            block_index: 0,
            lang: None,
            shell: true,
            text: "ls".into(),
            command: "ls".into(),
            source: CommandSource::Transcript,
        };
        assert!(is_fresh(&command));
        command.timestamp = Some("2020-01-01T00:00:00Z".into());
        assert!(!is_fresh(&command));
    }
}
