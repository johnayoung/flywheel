//! Following agent transcripts for suggested commands.
//!
//! Claude Code appends every reply to its transcript
//! (`<config>/projects/<dir>/<conversation>.jsonl`); Codex to its rollout
//! (`<codex home>/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`). The
//! hub tails the file of each agent session's current conversation from a
//! byte offset (no re-reads, memory bounded per line and per session),
//! pulls the fenced code blocks out of assistant text, and keeps the
//! newest [`MAX_COMMANDS`] per session. A conversation switch (`/clear`,
//! a resume under a new id) moves the tail to the new file; a file that
//! is replaced or truncated is read again from the start, without
//! duplicating blocks already seen. Codex sessions with no rollout to be
//! found fall back to the `notify` payload's `last-assistant-message`.

use crate::agent::{SessionKind, claude_transcript, is_conversation_id};
use crate::commands::{Command, CommandSource, MAX_BLOCK_BYTES, copyable, fenced_blocks, is_shell};
use crate::hub::{Hub, HubError, HubEvent};
use crate::pty::WriteOutcome;
use crate::state::{SessionRecord, StateError};
use crate::vt::TextSource;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

/// Commands kept per session (oldest dropped first).
pub const MAX_COMMANDS: usize = 500;
/// Block keys remembered per session against re-reading a replaced file.
const MAX_KEYS: usize = 4096;
/// A transcript line longer than this is skipped unread (tool output, not
/// a reply worth mining).
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Read size per syscall.
const READ_CHUNK: usize = 256 * 1024;
/// How often running agent sessions' transcripts are checked.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Least time between two searches for a session's file that was not found.
const LOOKUP_RETRY: Duration = Duration::from_secs(5);
/// Codex day directories searched for a rollout, newest first.
const MAX_ROLLOUT_DAYS: usize = 400;
/// Codex `notify` replies held until the watcher reads them.
const MAX_PENDING_NOTIFY: usize = 16;
/// A command is "fresh" (worth a toast) when written this recently.
pub const FRESH_WITHIN: Duration = Duration::from_secs(120);
/// Largest paste accepted.
pub const MAX_PASTE_BYTES: usize = 1024 * 1024;

/// Which agent's file format a tail reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Claude,
    Codex,
}

/// An incremental reader of one JSONL file.
#[derive(Debug)]
struct Tail {
    path: PathBuf,
    format: Format,
    /// The conversation the file belongs to.
    conversation: Option<String>,
    /// Bytes consumed from the file.
    offset: u64,
    /// (device, inode) when last read: a different file at the path
    /// (rotation) is read from the start.
    identity: Option<(u64, u64)>,
    /// The incomplete last line.
    pending: Vec<u8>,
    /// Inside an over-long line: drop bytes up to its newline.
    skipping: bool,
}

/// One reply's text from a transcript line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub message_id: String,
    pub timestamp: Option<String>,
    /// The reply's text parts (each mined separately for fences).
    pub texts: Vec<String>,
}

impl Tail {
    fn new(path: PathBuf, format: Format, conversation: Option<String>) -> Self {
        Self {
            path,
            format,
            conversation,
            offset: 0,
            identity: None,
            pending: Vec::new(),
            skipping: false,
        }
    }

    /// Read what was appended since the last call; return the replies in
    /// the complete lines. A missing file reads as nothing.
    fn read(&mut self) -> Vec<Reply> {
        let Ok(mut file) = File::open(&self.path) else {
            return Vec::new();
        };
        let Ok(meta) = file.metadata() else {
            return Vec::new();
        };
        let identity = file_identity(&meta);
        if self.identity.is_some_and(|known| known != identity) || meta.len() < self.offset {
            // Replaced or truncated: start over.
            self.offset = 0;
            self.pending.clear();
            self.skipping = false;
        }
        self.identity = Some(identity);
        if meta.len() == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut replies = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            let n = match file.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            self.offset += n as u64;
            let mut rest = &chunk[..n];
            while let Some(at) = rest.iter().position(|&b| b == b'\n') {
                let (head, tail) = rest.split_at(at);
                rest = &tail[1..];
                if self.skipping {
                    self.skipping = false;
                    continue;
                }
                if self.pending.is_empty() {
                    self.line(head, &mut replies);
                } else {
                    self.pending.extend_from_slice(head);
                    let line = std::mem::take(&mut self.pending);
                    self.line(&line, &mut replies);
                }
            }
            if !self.skipping {
                if self.pending.len() + rest.len() > MAX_LINE_BYTES {
                    self.pending = Vec::new();
                    self.skipping = true;
                } else {
                    self.pending.extend_from_slice(rest);
                }
            }
        }
        replies
    }

    fn line(&self, line: &[u8], replies: &mut Vec<Reply>) {
        // Cheap pre-filter: only lines with a fence are parsed (big tool
        // results without one are skipped unparsed).
        if !contains(line, b"```") && !contains(line, b"~~~") {
            return;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return;
        };
        let reply = match self.format {
            Format::Claude => claude_reply(&value),
            Format::Codex => codex_reply(&value),
        };
        replies.extend(reply);
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_identity(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// The reply in a Claude Code transcript line: an `assistant` line of the
/// main conversation (not a subagent's sidechain) with text content.
pub fn claude_reply(value: &serde_json::Value) -> Option<Reply> {
    if value.get("type")?.as_str()? != "assistant"
        || value.get("isSidechain").and_then(|v| v.as_bool()) == Some(true)
    {
        return None;
    }
    let message = value.get("message")?;
    let texts: Vec<String> = match message.get("content")? {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(str::to_owned))
            .collect(),
        _ => return None,
    };
    if texts.is_empty() {
        return None;
    }
    let message_id = value
        .get("uuid")
        .or_else(|| message.get("id"))
        .and_then(|v| v.as_str())?
        .to_owned();
    Some(Reply {
        message_id,
        timestamp: str_field(value, "timestamp"),
        texts,
    })
}

/// The reply in a Codex rollout line: a `response_item` wrapping an
/// assistant `message` (older rollouts write the item bare), its
/// `output_text` parts.
pub fn codex_reply(value: &serde_json::Value) -> Option<Reply> {
    let item = match value.get("type")?.as_str()? {
        "response_item" => value.get("payload")?,
        "message" => value,
        _ => return None,
    };
    if item.get("type")?.as_str()? != "message" || item.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let texts: Vec<String> = item
        .get("content")?
        .as_array()?
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("output_text"))
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(str::to_owned))
        .collect();
    if texts.is_empty() {
        return None;
    }
    // One reply's parts are one text.
    let texts = vec![texts.concat()];
    let timestamp = str_field(value, "timestamp");
    let message_id = str_field(item, "id")
        .or_else(|| timestamp.clone())
        .unwrap_or_default();
    Some(Reply {
        message_id,
        timestamp,
        texts,
    })
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

/// Codex's home: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home(
    codex_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(dir) = codex_home.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| Path::new(h).join(".codex"))
}

/// The rollout of Codex thread `thread`: `sessions/YYYY/MM/DD/rollout-*-<thread>.jsonl`,
/// searched newest day first.
pub fn codex_rollout(codex_home: &Path, thread: &str) -> Option<PathBuf> {
    let suffix = format!("-{thread}.jsonl");
    let sorted_dirs = |dir: &Path| -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        dirs.reverse();
        dirs
    };
    let mut days = 0;
    for year in sorted_dirs(&codex_home.join("sessions")) {
        for month in sorted_dirs(&year) {
            for day in sorted_dirs(&month) {
                days += 1;
                if days > MAX_ROLLOUT_DAYS {
                    return None;
                }
                let Ok(entries) = std::fs::read_dir(&day) else {
                    continue;
                };
                let found = entries.filter_map(Result::ok).map(|e| e.path()).find(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
                });
                if found.is_some() {
                    return found;
                }
            }
        }
    }
    None
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
}

impl SessionCommands {
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
        let mut index = 0;
        for text in &reply.texts {
            for block in fenced_blocks(text) {
                let block_index = index;
                index += 1;
                if (closed_only && !block.closed) || block.text.len() > MAX_BLOCK_BYTES {
                    continue;
                }
                let key = (
                    conversation.unwrap_or_default().to_owned(),
                    reply.message_id.clone(),
                    block_index,
                );
                if self.keys.contains(&key) {
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
        }
        added
    }

    fn remember(&mut self, key: (String, String, usize)) {
        if self.key_order.len() == MAX_KEYS
            && let Some(old) = self.key_order.pop_front()
        {
            self.keys.remove(&old);
        }
        self.keys.insert(key.clone());
        self.key_order.push_back(key);
    }
}

/// Every session's suggested commands.
#[derive(Debug)]
pub struct CommandBook {
    claude_config_dir: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    sessions: Mutex<HashMap<String, Arc<Mutex<SessionCommands>>>>,
    wake: Mutex<Option<mpsc::Sender<()>>>,
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
}

impl CommandBook {
    pub fn new(claude_config_dir: Option<PathBuf>, codex_home: Option<PathBuf>) -> Self {
        Self {
            claude_config_dir,
            codex_home,
            sessions: Mutex::new(HashMap::new()),
            wake: Mutex::new(None),
        }
    }

    fn entry(&self, id: &str) -> Arc<Mutex<SessionCommands>> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(sessions.entry(id.to_owned()).or_default())
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
            let mut log = entry.lock().unwrap_or_else(PoisonError::into_inner);
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
                    if let Some(message) =
                        message.filter(|m| m.contains("```") || m.contains("~~~"))
                    {
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

    /// Read whatever `record`'s agent wrote since the last call; returns
    /// the new commands (oldest first).
    pub fn refresh(&self, record: &SessionRecord) -> Vec<Command> {
        if !record.kind.is_agent() {
            return Vec::new();
        }
        let entry = self.entry(&record.id);
        let mut log = entry.lock().unwrap_or_else(PoisonError::into_inner);
        let conversation = record.agent_session_id.clone();
        let mut added = Vec::new();
        let following = log
            .tail
            .as_ref()
            .is_some_and(|t| t.conversation == conversation);
        if !following {
            let recently_missed = log
                .missed
                .as_ref()
                .is_some_and(|(c, at)| *c == conversation && at.elapsed() < LOOKUP_RETRY);
            if !recently_missed {
                match self.locate(record, log.hinted.as_deref()) {
                    Some((path, format)) => {
                        // Finish the old file first: its last reply may
                        // have landed after the previous look.
                        if let Some(mut old) = log.tail.take() {
                            let source = source_of(old.format);
                            let old_conversation = old.conversation.clone();
                            for reply in old.read() {
                                added.extend(log.add(
                                    &reply,
                                    old_conversation.as_deref(),
                                    source,
                                    false,
                                ));
                            }
                        }
                        log.tail = Some(Tail::new(path, format, conversation.clone()));
                        log.missed = None;
                    }
                    None => log.missed = Some((conversation.clone(), Instant::now())),
                }
            }
        }
        if let Some(mut tail) = log.tail.take() {
            let source = source_of(tail.format);
            for reply in tail.read() {
                added.extend(log.add(&reply, tail.conversation.as_deref(), source, false));
            }
            log.tail = Some(tail);
        }
        // Codex replies with no rollout to read: from the notify payload
        // (possibly cut short by the report budget: closed fences only).
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
                added.extend(log.add(&reply, conversation.as_deref(), CommandSource::Notify, true));
            }
        }
        added
    }

    /// A session's commands, newest first.
    pub fn list(&self, record: &SessionRecord, filter: CommandFilter) -> CommandList {
        let entry = self.entry(&record.id);
        let log = entry.lock().unwrap_or_else(PoisonError::into_inner);
        CommandList {
            session_id: record.id.clone(),
            conversation: record.agent_session_id.clone(),
            transcript: log.tail.as_ref().map(|t| t.path.clone()),
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

    /// Drop the logs of sessions not in `keep`.
    pub fn retain(&self, keep: &HashSet<String>) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|id, _| keep.contains(id));
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
/// every [`POLL_INTERVAL`] (and at once when a report nudges it). It ends
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
                let keep: HashSet<String> = sessions.iter().map(|s| s.record.id.clone()).collect();
                hub.command_book().retain(&keep);
                for view in sessions {
                    let running = view.live.as_ref().is_some_and(|l| l.pid.is_some());
                    if view.record.kind.is_agent() && running {
                        hub.refresh_commands(&view.record);
                    }
                }
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

/// The bytes that paste `text` into a terminal: newlines become CR (as a
/// terminal's own paste does), trailing newlines are dropped (never an
/// implicit Enter), and when `bracketed` it is wrapped in the
/// bracketed-paste markers with any markers inside removed, so the text
/// cannot end the paste early and run the rest. `run` appends one CR.
pub fn paste_bytes(text: &str, bracketed: bool, run: bool) -> Vec<u8> {
    let body = text
        .trim_end_matches(['\n', '\r'])
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    let mut out = Vec::with_capacity(body.len() + 16);
    if bracketed {
        let mut clean = body;
        while clean.contains("\x1b[201~") || clean.contains("\x1b[200~") {
            clean = clean.replace("\x1b[201~", "").replace("\x1b[200~", "");
        }
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(clean.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(body.as_bytes());
    }
    if run {
        out.push(b'\r');
    }
    out
}

impl Hub {
    /// Read `record`'s transcript for new commands and push them to events
    /// subscribers.
    pub(crate) fn refresh_commands(&self, record: &SessionRecord) {
        let added = self.command_book().refresh(record);
        if !added.is_empty() {
            self.emit(HubEvent::Commands {
                session_id: record.id.clone(),
                added,
            });
        }
    }

    /// A session's suggested commands, newest first (read up to date
    /// first, whether or not the session runs).
    pub fn commands(&self, id: &str, filter: CommandFilter) -> Result<CommandList, HubError> {
        let record = self.session(id)?.record;
        self.refresh_commands(&record);
        Ok(self.command_book().list(&record, filter))
    }

    /// Paste `text` into a running session's terminal, as a bracketed
    /// paste when its program enabled that; Enter only when `run`. A
    /// multi-line paste into a terminal without bracketed paste would run
    /// each line, so it is refused unless `run`.
    pub fn paste(&self, id: &str, text: &str, run: bool) -> Result<PasteOutcome, HubError> {
        let record = self.session(id)?.record;
        if text.trim().is_empty() {
            return Err(StateError::Invalid("nothing to paste".into()).into());
        }
        if text.len() > MAX_PASTE_BYTES {
            return Err(StateError::Invalid(format!(
                "paste is larger than {MAX_PASTE_BYTES} bytes"
            ))
            .into());
        }
        let session = self
            .live_session(id)
            .filter(|s| s.exit_info().is_none())
            .ok_or_else(|| {
                StateError::Conflict(format!("session {:?} is not running", record.name))
            })?;
        let bracketed = session
            .screen_text(TextSource::Visible)
            .modes
            .contains(&"bracketed_paste");
        let multiline = text.trim_end_matches(['\n', '\r']).contains(['\n', '\r']);
        if multiline && !bracketed && !run {
            return Err(StateError::Conflict(format!(
                "{:?} has not enabled bracketed paste, so pasting several lines would run each one; use Send + Run, or copy it",
                record.name
            ))
            .into());
        }
        let bytes = paste_bytes(text, bracketed, run);
        let len = bytes.len();
        match session.write(bytes) {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;
    use serde_json::json;
    use std::io::Write;

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
        let book = CommandBook::new(Some(config), None);
        let record = record(SessionKind::Claude, &cwd, conversation);
        (dir, book, record, path)
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
        book.entry("s1").lock().unwrap().missed = None;
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
            entry
                .lock()
                .unwrap()
                .tail
                .as_ref()
                .unwrap()
                .pending
                .capacity()
                <= MAX_LINE_BYTES + READ_CHUNK
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
        let book = CommandBook::new(None, Some(home.clone()));
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
        let item = |id: &str, text: &str| {
            json!({
                "timestamp": "2026-11-02T10:00:01.000Z",
                "type": "response_item",
                "payload": {"type": "message", "id": id, "role": "assistant",
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
        append(&rollout, &item("m1", "Run:\n```bash\nmake test\n```"));
        append(&rollout, &item("m2", "```zsh\nls -la\n```"));
        // Old-style bare items and user messages.
        append(&rollout, &(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "```sh\nno\n```"}]}).to_string() + "\n"));
        assert_eq!(
            codex_rollout(&home, thread).as_deref(),
            Some(rollout.as_path())
        );
        book.entry("s1").lock().unwrap().missed = None;
        let added = book.refresh(&record);
        assert_eq!(
            added.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["ls -la"]
        );
        assert_eq!(added[0].source, CommandSource::Rollout);
        assert_eq!(
            added[0].timestamp.as_deref(),
            Some("2026-11-02T10:00:01.000Z")
        );
    }

    #[test]
    fn paste_bytes_never_runs_unless_asked_and_cannot_break_out() {
        assert_eq!(
            paste_bytes("a\nb\n", true, false),
            b"\x1b[200~a\rb\x1b[201~"
        );
        assert_eq!(
            paste_bytes("a\r\nb", true, true),
            b"\x1b[200~a\rb\x1b[201~\r"
        );
        assert_eq!(
            paste_bytes("x\x1b[201~\x1b[20\x1b[201~1~rm -rf", true, false),
            b"\x1b[200~xrm -rf\x1b[201~"
        );
        assert_eq!(paste_bytes("ls -la\n", false, false), b"ls -la");
        assert_eq!(paste_bytes("ls", false, true), b"ls\r");
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

    #[test]
    fn codex_home_prefers_the_env_var() {
        use std::ffi::OsStr;
        assert_eq!(
            codex_home(Some(OsStr::new("/c")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            codex_home(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.codex"))
        );
        assert_eq!(codex_home(None, None), None);
    }
}
