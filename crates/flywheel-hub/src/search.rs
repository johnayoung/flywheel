//! Full-text search across the hub's sessions.
//!
//! Three sources, each read from what the hub already holds:
//!
//! * **scrollback** -- every logical line a session's server-side emulator
//!   retains (primary scrollback + rows, and the alternate screen while it
//!   is up), for every session with a terminal on this server, exited ones
//!   included while their terminal is kept;
//! * **transcript** -- an agent session's current conversation (Claude
//!   Code's JSONL, Codex's rollout): user and assistant text and tool
//!   calls, also for stopped and restored sessions;
//! * **commands** -- the suggested code blocks the hub saved per session.
//!
//! Work is bounded: a query is at most [`MAX_QUERY_BYTES`]; patterns go
//! through the `regex` crate (linear time, no backtracking) with compiled
//! size capped at [`REGEX_SIZE_LIMIT`]; every request has a time budget
//! ([`TIME_BUDGET`]) after which it answers with what it found (`partial`);
//! hits are capped per session and source and per request; at most
//! [`MAX_CONCURRENT`] searches run at once. Callers run [`Hub::search`] off
//! the async executor.
//!
//! Each source is scanned as one text (units joined by newlines) with one
//! regex pass rather than a call per line. Scrollback text is extracted
//! once per change of the session's output (keyed by bytes written and
//! size); transcripts are indexed incrementally from a byte offset, like
//! the commands tail, bounded per message ([`MAX_MESSAGE_BYTES`]) and per
//! transcript ([`MAX_TRANSCRIPT_BYTES`], oldest messages dropped first).

use crate::agent::SessionKind;
use crate::commands::Command;
use crate::hub::{Hub, HubError, SessionView};
use crate::pty::PtySession;
use crate::state::{SessionState, StateError};
use crate::transcript::{Format, MAX_LINE_BYTES};
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Longest query accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 1024;
/// Hits per request by default.
pub const DEFAULT_LIMIT: usize = 200;
/// Most hits one request may ask for.
pub const MAX_LIMIT: usize = 2000;
/// Hits per session and source by default (the newest).
pub const DEFAULT_PER_SESSION: usize = 20;
/// Most hits per session and source one request may ask for.
pub const MAX_PER_SESSION: usize = 1000;
/// How long one request may search before answering with what it found.
pub const TIME_BUDGET: Duration = Duration::from_millis(1500);
/// Compiled size cap of a pattern (and of its lazy DFA cache).
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// Searches running at once (more wait their turn).
pub const MAX_CONCURRENT: usize = 2;
/// Text of one transcript message kept for search, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Text kept per transcript, in bytes (oldest messages dropped first).
pub const MAX_TRANSCRIPT_BYTES: usize = 32 * 1024 * 1024;
/// Memory all transcript indexes together may hold; past it the least
/// recently searched are dropped (and re-read from their file when
/// searched again).
pub const MAX_INDEX_BYTES: usize = 256 * 1024 * 1024;
/// A transcript index not searched for this long is dropped.
pub const TRANSCRIPT_IDLE: Duration = Duration::from_secs(10 * 60);
/// Most hits per session and source one request may skip (paging).
pub const MAX_OFFSET: usize = 100_000;
/// Most messages one transcript page returns.
pub const MAX_PAGE_MESSAGES: usize = 200;
/// Characters of context before a match in a snippet (enough for a wide
/// row to show where a long line starts; narrow clients cut it from the
/// left).
const BEFORE_CHARS: usize = 160;
/// Characters of a snippet (before + match + after), at least.
const SNIPPET_CHARS: usize = 320;
/// Characters of the match itself shown in a snippet.
const MATCH_CHARS: usize = 200;
/// Characters of a scrollback match handed to the browser's terminal
/// search (`match_text`).
const MATCH_TEXT_CHARS: usize = 1000;
/// Read size per syscall when indexing a transcript.
const READ_CHUNK: usize = 256 * 1024;
/// Least time between two looks for a session's transcript that was not
/// found.
const LOCATE_RETRY: Duration = Duration::from_secs(5);
/// Check the clock every this many matches.
const CLOCK_EVERY: usize = 4096;

/// Where a hit was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Scrollback,
    Transcript,
    Commands,
}

impl Source {
    pub const ALL: [Self; 3] = [Self::Scrollback, Self::Transcript, Self::Commands];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scrollback => "scrollback",
            Self::Transcript => "transcript",
            Self::Commands => "commands",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

/// A compiled query.
#[derive(Debug, Clone)]
pub struct Matcher {
    re: Regex,
}

impl Matcher {
    /// `query` as a literal (`regex` false) or a `regex`-crate pattern;
    /// case-insensitive unless `case_sensitive`. `^` and `$` match at line
    /// boundaries.
    pub fn new(query: &str, regex: bool, case_sensitive: bool) -> Result<Self, String> {
        if query.is_empty() {
            return Err("the query is empty".into());
        }
        if query.len() > MAX_QUERY_BYTES {
            return Err(format!("the query is longer than {MAX_QUERY_BYTES} bytes"));
        }
        let pattern = if regex {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .multi_line(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| match e {
                regex::Error::CompiledTooBig(_) => format!(
                    "the pattern is too large (compiled over {} KiB); make it simpler",
                    REGEX_SIZE_LIMIT / 1024
                ),
                other => format!("invalid regex: {other}"),
            })?;
        Ok(Self { re })
    }

    /// The non-empty matches in `text`, as byte ranges.
    pub fn find_iter<'a>(&'a self, text: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.re
            .find_iter(text)
            .filter(|m| !m.is_empty())
            .map(|m| (m.start(), m.end()))
    }

    /// The first non-empty match in `text` starting at or after byte `at`
    /// (anchors and word boundaries see the text before `at`).
    pub fn find_at(&self, text: &str, mut at: usize) -> Option<(usize, usize)> {
        while at <= text.len() {
            let m = self.re.find_at(text, at)?;
            if !m.is_empty() {
                return Some((m.start(), m.end()));
            }
            // Step over the empty match by one character.
            at = m.start() + text[m.start()..].chars().next()?.len_utf8();
        }
        None
    }

    /// Whether `text` has a non-empty match.
    pub fn is_match(&self, text: &str) -> bool {
        self.find_iter(text).next().is_some()
    }
}

/// Units of text (lines, messages, blocks) joined by `\n`, scanned with
/// one regex pass.
#[derive(Debug, Default, Clone)]
struct Corpus {
    text: String,
    /// Where each unit starts in `text`.
    starts: Vec<usize>,
}

impl Corpus {
    fn from_units<I: IntoIterator<Item = S>, S: AsRef<str>>(units: I) -> Self {
        let mut corpus = Self::default();
        for unit in units {
            corpus.push(unit.as_ref());
        }
        corpus
    }

    fn push(&mut self, unit: &str) {
        if !self.starts.is_empty() {
            self.text.push('\n');
        }
        self.starts.push(self.text.len());
        self.text.push_str(unit);
    }

    fn len(&self) -> usize {
        self.starts.len()
    }

    fn end_of(&self, unit: usize) -> usize {
        self.starts
            .get(unit + 1)
            .map_or(self.text.len(), |next| next - 1)
    }

    fn unit(&self, unit: usize) -> &str {
        &self.text[self.starts[unit]..self.end_of(unit)]
    }

    /// The unit holding byte `offset` (a separator belongs to the unit
    /// before it).
    fn unit_of(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|&s| s <= offset)
            .saturating_sub(1)
    }

    /// Drop the first `n` units.
    fn drop_front(&mut self, n: usize) {
        if n >= self.starts.len() {
            self.text.clear();
            self.starts.clear();
            return;
        }
        let cut = self.starts[n];
        self.text.drain(..cut);
        self.starts.drain(..n);
        for start in &mut self.starts {
            *start -= cut;
        }
    }
}

/// A matched unit: its index and its first match (bytes within the unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnitHit {
    unit: usize,
    start: usize,
    end: usize,
    /// Matches in the unit.
    matches: usize,
}

/// What [`scan`] found.
#[derive(Debug, Default)]
struct Scan {
    /// The last `keep` matched units, oldest first.
    hits: VecDeque<UnitHit>,
    /// Units with a match (all of them, not only those kept).
    units: usize,
    /// The whole text was scanned (the budget did not run out).
    complete: bool,
}

/// Scan `corpus`, keeping the last `keep` matched units.
///
/// A match never runs across units: one that would (a pattern with `\n`
/// or `\s` spanning the separator) is retried inside its unit alone, so
/// what is reported is a match the unit itself holds.
fn scan(corpus: &Corpus, matcher: &Matcher, keep: usize, deadline: Instant) -> Scan {
    let mut found = Scan {
        complete: true,
        ..Scan::default()
    };
    let mut last: Option<UnitHit> = None;
    let mut record = |found: &mut Scan, unit: usize, start: usize, end: usize| {
        if let Some(hit) = last.as_mut()
            && hit.unit == unit
        {
            hit.matches += 1;
            return;
        }
        if let Some(done) = last.take() {
            keep_last(&mut found.hits, done, keep);
        }
        let base = corpus.starts[unit];
        last = Some(UnitHit {
            unit,
            start: start - base,
            end: end - base,
            matches: 1,
        });
        found.units += 1;
    };
    let text = corpus.text.as_str();
    let mut pos = 0;
    let mut steps = 0usize;
    while pos <= text.len() {
        steps += 1;
        if steps.is_multiple_of(CLOCK_EVERY) && Instant::now() >= deadline {
            found.complete = false;
            break;
        }
        let Some((start, end)) = matcher.find_at(text, pos) else {
            break;
        };
        let unit = corpus.unit_of(start);
        let unit_end = corpus.end_of(unit);
        if end <= unit_end {
            record(&mut found, unit, start, end);
            pos = end;
            continue;
        }
        // The match crosses into the next unit: look again inside this one.
        let mut inner = start;
        while inner < unit_end {
            let Some((s, e)) = matcher.find_at(&text[..unit_end], inner) else {
                break;
            };
            record(&mut found, unit, s, e);
            inner = e;
        }
        pos = unit_end + 1;
    }
    if let Some(done) = last {
        keep_last(&mut found.hits, done, keep);
    }
    found
}

fn keep_last(hits: &mut VecDeque<UnitHit>, hit: UnitHit, keep: usize) {
    if keep == 0 {
        return;
    }
    if hits.len() == keep {
        hits.pop_front();
    }
    hits.push_back(hit);
}

/// A match in context, on one line: `before` + `match` + `after`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snippet {
    /// Up to [`BEFORE_CHARS`] characters before the match, `…`-prefixed
    /// when cut.
    pub before: String,
    #[serde(rename = "match")]
    pub matched: String,
    /// The rest of the line, `…`-suffixed when cut.
    pub after: String,
    /// 0-based line of the match within its unit (a message or block).
    pub line: usize,
}

/// Control characters (tabs, newlines, escapes) shown as spaces.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The last `n` characters of `text`, and whether it was cut.
fn tail_chars(text: &str, n: usize) -> (&str, bool) {
    match text.char_indices().rev().nth(n.saturating_sub(1)) {
        Some((i, _)) if n > 0 && i > 0 => (&text[i..], true),
        _ if n == 0 => ("", !text.is_empty()),
        _ => (text, false),
    }
}

/// The first `n` characters of `text`, and whether it was cut.
fn head_chars(text: &str, n: usize) -> (&str, bool) {
    match text.char_indices().nth(n) {
        Some((i, _)) => (&text[..i], true),
        None => (text, false),
    }
}

/// The snippet of `unit`'s match at bytes `start..end`.
pub fn snippet(unit: &str, start: usize, end: usize) -> Snippet {
    let line_start = unit[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = unit[start..].find('\n').map_or(unit.len(), |i| start + i);
    // A match running past its line is shown up to the line's end (one
    // that starts on a newline, as itself).
    let end = if end.min(line_end) > start {
        end.min(line_end)
    } else {
        end
    };
    let (before, cut_before) = tail_chars(&unit[line_start..start], BEFORE_CHARS);
    let before = if cut_before {
        format!("…{}", clean(before))
    } else {
        clean(before).trim_start().to_owned()
    };
    let (matched, cut_match) = head_chars(&unit[start..end], MATCH_CHARS);
    let mut matched = clean(matched);
    if cut_match {
        matched.push('…');
    }
    let room = SNIPPET_CHARS
        .saturating_sub(before.chars().count() + matched.chars().count())
        .max(40);
    let rest = if end < line_end {
        &unit[end..line_end]
    } else {
        ""
    };
    let (after, cut_after) = head_chars(rest, room);
    let mut after = clean(after).trim_end().to_owned();
    if cut_after {
        after.push('…');
    }
    Snippet {
        before,
        matched,
        after,
        line: unit[..start].matches('\n').count(),
    }
}

/// Which screen a scrollback hit is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    Primary,
    Alternate,
}

/// Who wrote a transcript message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// What a transcript message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Prose (a prompt, a reply).
    Text,
    /// A tool call: the tool's name and its input.
    Tool,
}

/// One hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Hit {
    Scrollback {
        screen: Screen,
        /// 0-based logical line, oldest first.
        index: usize,
        /// Logical lines after it (the browser counts from the bottom:
        /// its terminal holds the newest lines of the same list).
        from_end: usize,
        matches: usize,
        /// The first match exactly, for the terminal's own search.
        match_text: String,
        /// Earlier occurrences of `match_text` on the line.
        occurrence: usize,
        #[serde(flatten)]
        snippet: Snippet,
    },
    Transcript {
        /// Message index in the conversation (see the transcript page).
        index: u64,
        message_id: String,
        role: Role,
        kind: MessageKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        timestamp: Option<String>,
        matches: usize,
        #[serde(flatten)]
        snippet: Snippet,
    },
    Commands {
        seq: u64,
        lang: Option<String>,
        shell: bool,
        timestamp: Option<String>,
        matches: usize,
        #[serde(flatten)]
        snippet: Snippet,
    },
}

/// Matched units per source (all of them; `hits` holds the newest).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub scrollback: usize,
    pub transcript: usize,
    pub commands: usize,
}

/// One session's hits, each source newest first.
#[derive(Debug, Clone, Serialize)]
pub struct SessionGroup {
    pub session_id: String,
    pub session: String,
    pub workspace_id: String,
    pub workspace: String,
    pub kind: SessionKind,
    /// `running`, `exited`, or `stopped`.
    pub state: &'static str,
    /// The server holds its terminal (scrollback searchable, attachable).
    pub terminal: bool,
    pub conversation: Option<String>,
    pub hits: Vec<Hit>,
    pub counts: Counts,
    /// Some source has matches past this page (`counts` minus `offset`
    /// minus `hits`). A group past the request's `limit` has `hits`
    /// empty and only its `counts`.
    pub more: bool,
}

/// How much was searched.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Searched {
    pub sessions: usize,
    pub lines: usize,
    pub messages: usize,
    pub commands: usize,
}

/// A search's answer.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub sources: Vec<Source>,
    pub groups: Vec<SessionGroup>,
    /// Hits returned.
    pub hits: usize,
    /// The request's `limit` was reached: the sessions after it are only
    /// counted (groups with `counts` and no `hits`).
    pub truncated: bool,
    /// Hits skipped per session and source (the request's `offset`).
    pub offset: usize,
    /// The time budget ran out (or a transcript is still being indexed):
    /// what was searched is complete, the rest was not reached.
    pub partial: bool,
    pub elapsed_ms: u64,
    pub searched: Searched,
    /// Why something could not be searched (an unreadable transcript).
    pub notes: Vec<String>,
}

/// What to search for, and where.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// Only this workspace (id).
    pub workspace: Option<String>,
    /// Only this session (id).
    pub session: Option<String>,
    pub kinds: Vec<SessionKind>,
    pub sources: Vec<Source>,
    /// Most hits in all.
    pub limit: usize,
    /// Most hits per session and source.
    pub per_session: usize,
    /// Hits per session and source to skip, newest first (the next page
    /// of a session: the same query with `offset` = hits seen so far).
    pub offset: usize,
    pub budget: Duration,
}

impl SearchRequest {
    /// Every session, every source, the defaults.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            regex: false,
            case_sensitive: false,
            workspace: None,
            session: None,
            kinds: Vec::new(),
            sources: Source::ALL.to_vec(),
            limit: DEFAULT_LIMIT,
            per_session: DEFAULT_PER_SESSION,
            offset: 0,
            budget: TIME_BUDGET,
        }
    }
}

// --- transcripts ------------------------------------------------------

/// One searchable message pulled out of a transcript line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub id: String,
    pub role: Role,
    pub kind: MessageKind,
    pub tool: Option<String>,
    pub timestamp: Option<String>,
    pub text: String,
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A tool's input as text: `key: value` lines for an object (strings as
/// written, the rest as JSON), a JSON string's content decoded first.
pub fn tool_text(input: &Value) -> String {
    match input {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ Value::Object(_)) => tool_text(&parsed),
            _ => text.clone(),
        },
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| match value {
                Value::String(text) => format!("{key}: {text}"),
                other => format!("{key}: {other}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn tool_message(id: String, name: &str, input: String, timestamp: Option<String>) -> Extracted {
    Extracted {
        id,
        role: Role::Assistant,
        kind: MessageKind::Tool,
        tool: Some(name.to_owned()),
        timestamp,
        text: if input.is_empty() {
            name.to_owned()
        } else {
            format!("{name}\n{input}")
        },
    }
}

/// The messages in a Claude Code transcript line: a user or assistant
/// line of the main conversation (not a subagent's sidechain) -- its text
/// parts as one message, each `tool_use` part as another. Thinking, tool
/// results, and images are left out.
pub fn claude_messages(value: &Value) -> Vec<Extracted> {
    let role = match str_of(value, "type") {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return Vec::new(),
    };
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    let Some(message) = value.get("message") else {
        return Vec::new();
    };
    let Some(id) = str_of(value, "uuid").or_else(|| str_of(message, "id")) else {
        return Vec::new();
    };
    let timestamp = str_of(value, "timestamp").map(str::to_owned);
    let mut out = Vec::new();
    let mut texts: Vec<&str> = Vec::new();
    let mut tools = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => texts.push(text),
        Some(Value::Array(parts)) => {
            for part in parts {
                match str_of(part, "type") {
                    Some("text") => texts.extend(str_of(part, "text")),
                    Some("tool_use") => tools.push(part),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let text = texts.join("\n\n");
    if !text.trim().is_empty() {
        out.push(Extracted {
            id: id.to_owned(),
            role,
            kind: MessageKind::Text,
            tool: None,
            timestamp: timestamp.clone(),
            text,
        });
    }
    for (i, part) in tools.into_iter().enumerate() {
        let name = str_of(part, "name").unwrap_or("tool");
        let input = part.get("input").map(tool_text).unwrap_or_default();
        out.push(tool_message(
            format!("{id}#tool{i}"),
            name,
            input,
            timestamp.clone(),
        ));
    }
    out
}

/// Context Codex injects as user messages (not the user's words).
const CODEX_CONTEXT_PREFIXES: &[&str] = &["<environment_context>", "<user_instructions>"];

/// The messages in a Codex rollout line (`response_item`, or the bare
/// item older rollouts write): user and assistant messages, and function,
/// custom tool, and local shell calls. `offset` (the line's byte offset)
/// names items without an id.
pub fn codex_messages(value: &Value, offset: u64) -> Vec<Extracted> {
    let item = match str_of(value, "type") {
        Some("response_item") => match value.get("payload") {
            Some(payload) => payload,
            None => return Vec::new(),
        },
        _ => value,
    };
    let timestamp = str_of(value, "timestamp").map(str::to_owned);
    let id = str_of(item, "id")
        .or_else(|| str_of(item, "call_id"))
        .map_or_else(|| format!("@{offset}"), str::to_owned);
    let message = match str_of(item, "type") {
        Some("message") => {
            let role = match str_of(item, "role") {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => return Vec::new(),
            };
            let text = item
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|p| matches!(str_of(p, "type"), Some("input_text" | "output_text")))
                        .filter_map(|p| str_of(p, "text"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let trimmed = text.trim_start();
            if trimmed.is_empty()
                || (role == Role::User
                    && CODEX_CONTEXT_PREFIXES
                        .iter()
                        .any(|p| trimmed.starts_with(p)))
            {
                return Vec::new();
            }
            Extracted {
                id,
                role,
                kind: MessageKind::Text,
                tool: None,
                timestamp,
                text,
            }
        }
        Some("function_call") => tool_message(
            id,
            str_of(item, "name").unwrap_or("function"),
            item.get("arguments").map(tool_text).unwrap_or_default(),
            timestamp,
        ),
        Some("custom_tool_call") => tool_message(
            id,
            str_of(item, "name").unwrap_or("tool"),
            item.get("input").map(tool_text).unwrap_or_default(),
            timestamp,
        ),
        Some("local_shell_call") => {
            let command = item
                .get("action")
                .and_then(|a| a.get("command"))
                .and_then(Value::as_array)
                .map(|argv| {
                    argv.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            tool_message(id, "shell", command, timestamp)
        }
        _ => return Vec::new(),
    };
    vec![message]
}

/// A message as indexed (its text is the corpus unit).
#[derive(Debug, Clone, Serialize)]
pub struct MessageMeta {
    pub index: u64,
    pub message_id: String,
    pub role: Role,
    pub kind: MessageKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub timestamp: Option<String>,
    /// Its text was cut at [`MAX_MESSAGE_BYTES`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
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

/// Cut `text` to at most `max` bytes, on a character boundary.
fn cut_to(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// One transcript file's messages, read incrementally.
#[derive(Debug)]
struct TranscriptIndex {
    path: PathBuf,
    format: Format,
    identity: Option<(u64, u64)>,
    /// Bytes consumed.
    offset: u64,
    /// The incomplete last line.
    pending: Vec<u8>,
    /// Inside an over-long line: drop bytes up to its newline.
    skipping: bool,
    messages: VecDeque<MessageMeta>,
    corpus: Corpus,
    next_index: u64,
    /// Why the file could not be read last time.
    error: Option<String>,
    /// Heap held by `messages` (see [`meta_cost`]).
    meta_bytes: usize,
    /// Most bytes it holds ([`MAX_TRANSCRIPT_BYTES`]).
    cap: usize,
}

/// Memory one message's metadata takes, counted toward
/// [`MAX_TRANSCRIPT_BYTES`] with its text.
fn meta_cost(meta: &MessageMeta) -> usize {
    std::mem::size_of::<MessageMeta>()
        + meta.message_id.len()
        + meta.tool.as_ref().map_or(0, String::len)
        + meta.timestamp.as_ref().map_or(0, String::len)
}

impl TranscriptIndex {
    fn new(path: PathBuf, format: Format) -> Self {
        Self {
            path,
            format,
            identity: None,
            offset: 0,
            pending: Vec::new(),
            skipping: false,
            messages: VecDeque::new(),
            corpus: Corpus::default(),
            next_index: 0,
            error: None,
            meta_bytes: 0,
            cap: MAX_TRANSCRIPT_BYTES,
        }
    }

    fn reset(&mut self) {
        let (path, format, cap) = (self.path.clone(), self.format, self.cap);
        *self = Self::new(path, format);
        self.cap = cap;
    }

    /// Read what the file has new, until `deadline`; returns whether it
    /// was read to its end.
    fn update(&mut self, deadline: Instant) -> bool {
        match self.read(deadline) {
            Ok(complete) => {
                self.error = None;
                complete
            }
            Err(e) => {
                self.error = Some(format!("cannot read {}: {e}", self.path.display()));
                true
            }
        }
    }

    fn read(&mut self, deadline: Instant) -> std::io::Result<bool> {
        let meta = std::fs::metadata(&self.path)?;
        let identity = file_identity(&meta);
        if self.identity.is_some_and(|known| known != identity) || meta.len() < self.offset {
            // Replaced or truncated: read again from the start.
            self.reset();
        }
        self.identity = Some(identity);
        if meta.len() == self.offset {
            return Ok(true);
        }
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Ok(true);
            }
            let mut pos = self.offset;
            self.offset += n as u64;
            let mut rest = &buf[..n];
            while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
                if self.skipping {
                    self.skipping = false;
                } else {
                    let line_start = pos - self.pending.len() as u64;
                    let mut line = std::mem::take(&mut self.pending);
                    line.extend_from_slice(&rest[..newline]);
                    self.line(&line, line_start);
                }
                pos += newline as u64 + 1;
                rest = &rest[newline + 1..];
            }
            if !self.skipping {
                if self.pending.len() + rest.len() > MAX_LINE_BYTES {
                    self.pending.clear();
                    self.skipping = true;
                } else {
                    self.pending.extend_from_slice(rest);
                }
            }
        }
    }

    fn line(&mut self, line: &[u8], offset: u64) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let messages = match self.format {
            Format::Claude => claude_messages(&value),
            Format::Codex => codex_messages(&value, offset),
        };
        for mut message in messages {
            let truncated = cut_to(&mut message.text, MAX_MESSAGE_BYTES);
            self.corpus.push(&message.text);
            let meta = MessageMeta {
                index: self.next_index,
                message_id: message.id,
                role: message.role,
                kind: message.kind,
                tool: message.tool,
                timestamp: message.timestamp,
                truncated,
            };
            self.meta_bytes += meta_cost(&meta);
            self.messages.push_back(meta);
            self.next_index += 1;
        }
        if self.footprint() > self.cap {
            // Drop the oldest down to three quarters, so trimming is rare.
            let excess = self.footprint() - self.cap * 3 / 4;
            let (mut freed, mut n) = (0, 0);
            while freed < excess && n < self.messages.len() {
                freed += self.unit_cost(n);
                n += 1;
            }
            self.corpus.drop_front(n);
            for meta in self.messages.drain(..n) {
                self.meta_bytes -= meta_cost(&meta);
            }
        }
    }

    /// Memory message `unit` takes: its text, separator, start, metadata.
    fn unit_cost(&self, unit: usize) -> usize {
        self.corpus.unit(unit).len()
            + 1
            + std::mem::size_of::<usize>()
            + meta_cost(&self.messages[unit])
    }

    /// Memory the index holds (text, unit starts, message metadata, the
    /// incomplete last line), as counted toward [`MAX_TRANSCRIPT_BYTES`].
    fn footprint(&self) -> usize {
        self.corpus.text.len()
            + self.corpus.starts.len() * std::mem::size_of::<usize>()
            + self.meta_bytes
            + self.pending.len()
    }

    /// Oldest index still held.
    fn first_index(&self) -> u64 {
        self.messages.front().map_or(self.next_index, |m| m.index)
    }
}

// --- the index --------------------------------------------------------

/// A transcript index with what eviction needs without locking it.
#[derive(Debug)]
struct TranscriptSlot {
    format: Format,
    index: Mutex<TranscriptIndex>,
    /// [`TranscriptIndex::footprint`] after its last update.
    bytes: AtomicUsize,
    /// When it was last searched or paged.
    used: Mutex<Instant>,
}

impl TranscriptSlot {
    /// The index, marked used now.
    fn lock(&self) -> MutexGuard<'_, TranscriptIndex> {
        *self.used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `index`'s size after an update.
    fn settle(&self, index: &TranscriptIndex) {
        self.bytes.store(index.footprint(), Ordering::Relaxed);
    }

    fn used(&self) -> Instant {
        *self.used.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A session's terminal text, read for one search.
#[derive(Debug)]
struct ScrollEntry {
    primary: Corpus,
    alternate: Option<Corpus>,
}

/// Where a session's transcript was last found.
#[derive(Debug, Clone)]
struct Located {
    conversation: Option<String>,
    file: Option<(PathBuf, Format)>,
    at: Instant,
}

/// The hub's search state: cached terminal text, transcript indexes, and
/// the concurrency limit.
#[derive(Debug)]
pub struct SearchIndex {
    permits: Arc<tokio::sync::Semaphore>,
    located: Mutex<HashMap<String, Located>>,
    transcripts: Mutex<HashMap<PathBuf, Arc<TranscriptSlot>>>,
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
            located: Mutex::new(HashMap::new()),
            transcripts: Mutex::new(HashMap::new()),
        }
    }
}

impl SearchIndex {
    /// A turn to search (at most [`MAX_CONCURRENT`] run at once).
    pub async fn permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("the search semaphore is never closed")
    }

    /// A session's terminal text now. Not kept here: the emulator keeps
    /// what it read last and re-reads only rows scrolled in since (see
    /// [`crate::vt::Emulator::search_text`]), so this is a copy, cheap to
    /// take, and no second copy of every scrollback stays in memory.
    fn scroll_entry(pty: &PtySession) -> ScrollEntry {
        let text = pty.search_text();
        ScrollEntry {
            primary: Corpus {
                text: text.primary.text,
                starts: text.primary.starts,
            },
            alternate: text.alternate.as_ref().map(Corpus::from_units),
        }
    }

    fn transcript(&self, path: &Path, format: Format) -> Arc<TranscriptSlot> {
        let mut transcripts = self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = transcripts.get(path).filter(|s| s.format == format) {
            return Arc::clone(slot);
        }
        let slot = Arc::new(TranscriptSlot {
            format,
            index: Mutex::new(TranscriptIndex::new(path.to_path_buf(), format)),
            bytes: AtomicUsize::new(0),
            used: Mutex::new(Instant::now()),
        });
        transcripts.insert(path.to_path_buf(), Arc::clone(&slot));
        slot
    }

    /// Bytes all transcript indexes hold.
    pub fn transcript_bytes(&self) -> usize {
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|s| s.bytes.load(Ordering::Relaxed))
            .sum()
    }

    /// Transcript indexes held.
    pub fn transcript_count(&self) -> usize {
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Drop transcript indexes idle for [`TRANSCRIPT_IDLE`], then the
    /// least recently used until all hold at most [`MAX_INDEX_BYTES`].
    /// One in use by a search right now is kept. Cheap (no file work):
    /// the transcript watcher calls it every poll, searches after each
    /// transcript.
    pub fn sweep(&self) {
        self.sweep_with(MAX_INDEX_BYTES, TRANSCRIPT_IDLE);
    }

    fn sweep_with(&self, cap: usize, idle: Duration) {
        let mut transcripts = self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Held only by the map: no search is using it.
        let free = |slot: &Arc<TranscriptSlot>| Arc::strong_count(slot) == 1;
        transcripts.retain(|_, slot| !free(slot) || slot.used().elapsed() < idle);
        let mut total: usize = transcripts
            .values()
            .map(|s| s.bytes.load(Ordering::Relaxed))
            .sum();
        if total <= cap {
            return;
        }
        let mut oldest: Vec<(Instant, PathBuf, usize)> = transcripts
            .iter()
            .filter(|(_, slot)| free(slot))
            .map(|(path, slot)| {
                (
                    slot.used(),
                    path.clone(),
                    slot.bytes.load(Ordering::Relaxed),
                )
            })
            .collect();
        oldest.sort();
        for (_, path, bytes) in oldest {
            if total <= cap {
                break;
            }
            transcripts.remove(&path);
            total -= bytes;
        }
    }

    /// Forget what belongs to sessions no longer there (`sessions`: every
    /// session the hub has).
    fn prune(&self, sessions: &HashSet<String>) {
        let mut located = self.located.lock().unwrap_or_else(PoisonError::into_inner);
        located.retain(|id, _| sessions.contains(id));
        let used: HashSet<PathBuf> = located
            .values()
            .filter_map(|l| l.file.as_ref().map(|(p, _)| p.clone()))
            .collect();
        drop(located);
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|path, _| used.contains(path));
    }
}

/// Which of a session's hits (per source, newest first) a request shows.
#[derive(Debug, Clone, Copy)]
struct Page {
    skip: usize,
    take: usize,
}

impl Page {
    /// Newest matched units to keep while scanning.
    fn keep(self) -> usize {
        self.skip + self.take
    }
}

fn state_name(state: &SessionState) -> &'static str {
    match state {
        SessionState::Running => "running",
        SessionState::Exited { .. } => "exited",
        SessionState::Stopped { .. } => "stopped",
    }
}

/// One transcript message with its text (the transcript page).
#[derive(Debug, Clone, Serialize)]
pub struct PageMessage {
    #[serde(flatten)]
    pub meta: MessageMeta,
    pub text: String,
}

/// A window of a session's current conversation.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptPage {
    pub session_id: String,
    pub conversation: Option<String>,
    /// The file read (`null`: none found yet).
    pub transcript: Option<PathBuf>,
    /// Oldest message index still held (older ones were dropped).
    pub first: u64,
    /// One past the newest message index.
    pub next: u64,
    /// The file was read to its end.
    pub complete: bool,
    pub messages: Vec<PageMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Hub {
    /// Where `view`'s current conversation is written, looked up at most
    /// every [`LOCATE_RETRY`] while it is not found.
    fn transcript_of(&self, view: &SessionView) -> Option<(PathBuf, Format)> {
        let record = &view.record;
        if !record.kind.is_agent() {
            return None;
        }
        let index = self.search_index();
        let cached = index
            .located
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&record.id)
            .cloned();
        if let Some(found) = cached.filter(|l| {
            l.conversation == record.agent_session_id
                && (l.file.is_some() || l.at.elapsed() < LOCATE_RETRY)
        }) {
            return found.file;
        }
        let file = self.command_book().transcript_file(record);
        index
            .located
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                record.id.clone(),
                Located {
                    conversation: record.agent_session_id.clone(),
                    file: file.clone(),
                    at: Instant::now(),
                },
            );
        file
    }

    /// Search the sessions `request` names. Blocking (reads terminals and
    /// files): call it off the async executor, holding a
    /// [`SearchIndex::permit`].
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse, HubError> {
        let started = Instant::now();
        let deadline = started + request.budget;
        let matcher = Matcher::new(&request.query, request.regex, request.case_sensitive)
            .map_err(StateError::Invalid)?;
        let limit = request.limit.clamp(1, MAX_LIMIT);
        let per_session = request.per_session.clamp(1, MAX_PER_SESSION);
        let offset = request.offset.min(MAX_OFFSET);
        let sources: Vec<Source> = Source::ALL
            .into_iter()
            .filter(|s| request.sources.contains(s))
            .collect();
        if let Some(id) = &request.workspace {
            self.workspace(id)?;
        }
        if let Some(id) = &request.session {
            self.session(id)?;
        }
        let workspaces: HashMap<String, String> = self
            .workspaces()
            .into_iter()
            .map(|w| (w.id, w.name))
            .collect();
        let all = self.sessions(None);
        let ids: HashSet<String> = all.iter().map(|s| s.record.id.clone()).collect();
        self.search_index().prune(&ids);

        let mut response = SearchResponse {
            query: request.query.clone(),
            regex: request.regex,
            case_sensitive: request.case_sensitive,
            sources: sources.clone(),
            groups: Vec::new(),
            hits: 0,
            truncated: false,
            offset,
            partial: false,
            elapsed_ms: 0,
            searched: Searched::default(),
            notes: Vec::new(),
        };
        let wanted = all.into_iter().filter(|view| {
            let r = &view.record;
            request
                .workspace
                .as_ref()
                .is_none_or(|w| &r.workspace_id == w)
                && request.session.as_ref().is_none_or(|s| &r.id == s)
                && (request.kinds.is_empty() || request.kinds.contains(&r.kind))
        });
        for view in wanted {
            if Instant::now() >= deadline {
                response.partial = true;
                break;
            }
            // Past the limit, sessions are still searched but only
            // counted, so every one that matches is listed.
            let counting = response.hits >= limit;
            if counting {
                response.truncated = true;
            }
            let page = if counting {
                Page { skip: 0, take: 0 }
            } else {
                Page {
                    skip: offset,
                    take: per_session,
                }
            };
            response.searched.sessions += 1;
            let record = &view.record;
            let mut group = SessionGroup {
                session_id: record.id.clone(),
                session: record.name.clone(),
                workspace_id: record.workspace_id.clone(),
                workspace: workspaces
                    .get(&record.workspace_id)
                    .cloned()
                    .unwrap_or_default(),
                kind: record.kind,
                state: state_name(&record.state),
                terminal: view.live.is_some(),
                conversation: record.agent_session_id.clone(),
                hits: Vec::new(),
                counts: Counts::default(),
                more: false,
            };
            for &source in &sources {
                if Instant::now() >= deadline {
                    response.partial = true;
                    break;
                }
                let complete = match source {
                    Source::Scrollback => self.search_scrollback(
                        &view,
                        &matcher,
                        page,
                        deadline,
                        &mut group,
                        &mut response.searched,
                    ),
                    Source::Transcript => self.search_transcript(
                        &view,
                        &matcher,
                        page,
                        deadline,
                        &mut group,
                        &mut response,
                    ),
                    Source::Commands => {
                        self.search_commands(
                            &view,
                            &matcher,
                            page,
                            &mut group,
                            &mut response.searched,
                        );
                        true
                    }
                };
                if !complete {
                    response.partial = true;
                }
            }
            if group.hits.is_empty() && !group.more {
                continue;
            }
            let room = limit.saturating_sub(response.hits);
            if group.hits.len() > room {
                group.hits.truncate(room);
                group.more = true;
                response.truncated = true;
            }
            response.hits += group.hits.len();
            response.groups.push(group);
        }
        response.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(response)
    }

    fn search_scrollback(
        &self,
        view: &SessionView,
        matcher: &Matcher,
        page: Page,
        deadline: Instant,
        group: &mut SessionGroup,
        searched: &mut Searched,
    ) -> bool {
        let keep = page.keep();
        let Some(pty) = self.live_session(&view.record.id) else {
            return true;
        };
        let entry = SearchIndex::scroll_entry(&pty);
        let mut complete = true;
        let mut hits = Vec::new();
        // The screen in view first (the alternate one, when up), then
        // the primary's newest lines.
        let screens = entry
            .alternate
            .as_ref()
            .map(|c| (Screen::Alternate, c))
            .into_iter()
            .chain(std::iter::once((Screen::Primary, &entry.primary)));
        for (screen, corpus) in screens {
            searched.lines += corpus.len();
            let found = scan(corpus, matcher, keep.saturating_sub(hits.len()), deadline);
            complete &= found.complete;
            group.counts.scrollback += found.units;
            for hit in found.hits.iter().rev() {
                let line = corpus.unit(hit.unit);
                let (match_text, _) = head_chars(&line[hit.start..hit.end], MATCH_TEXT_CHARS);
                hits.push(Hit::Scrollback {
                    screen,
                    index: hit.unit,
                    from_end: corpus.len() - 1 - hit.unit,
                    matches: hit.matches,
                    occurrence: line[..hit.start].matches(match_text).count(),
                    match_text: match_text.to_owned(),
                    snippet: snippet(line, hit.start, hit.end),
                });
            }
            if !complete {
                break;
            }
        }
        group.more |= group.counts.scrollback > hits.len();
        group.hits.extend(hits.into_iter().skip(page.skip));
        complete
    }

    fn search_transcript(
        &self,
        view: &SessionView,
        matcher: &Matcher,
        page: Page,
        deadline: Instant,
        group: &mut SessionGroup,
        response: &mut SearchResponse,
    ) -> bool {
        let Some((path, format)) = self.transcript_of(view) else {
            return true;
        };
        let slot = self.search_index().transcript(&path, format);
        let mut index = slot.lock();
        let read = index.update(deadline);
        slot.settle(&index);
        if let Some(error) = &index.error {
            response
                .notes
                .push(format!("{}: {error}", view.record.label()));
        }
        response.searched.messages += index.messages.len();
        let found = scan(&index.corpus, matcher, page.keep(), deadline);
        group.counts.transcript += found.units;
        group.more |= found.units > found.hits.len();
        for hit in found.hits.iter().rev().skip(page.skip) {
            let meta = &index.messages[hit.unit];
            let text = index.corpus.unit(hit.unit);
            group.hits.push(Hit::Transcript {
                index: meta.index,
                message_id: meta.message_id.clone(),
                role: meta.role,
                kind: meta.kind,
                tool: meta.tool.clone(),
                timestamp: meta.timestamp.clone(),
                matches: hit.matches,
                snippet: snippet(text, hit.start, hit.end),
            });
        }
        drop(index);
        drop(slot);
        self.search_index().sweep();
        read && found.complete
    }

    fn search_commands(
        &self,
        view: &SessionView,
        matcher: &Matcher,
        page: Page,
        group: &mut SessionGroup,
        searched: &mut Searched,
    ) {
        let keep = page.keep();
        if !view.record.kind.is_agent() {
            return;
        }
        // Up to date first, as the drawer's list is (a running agent's
        // blocks are otherwise read within the watcher's next poll).
        let running = view.live.as_ref().is_some_and(|l| l.pid.is_some());
        if running {
            self.refresh_commands(&view.record);
        }
        let mut hits = Vec::new();
        let mut count = 0;
        self.command_book()
            .visit_commands(&view.record.id, |command: &Command| {
                searched.commands += 1;
                let mut matches = matcher.find_iter(&command.text);
                if let Some((start, end)) = matches.next() {
                    count += 1;
                    if hits.len() < keep {
                        hits.push(Hit::Commands {
                            seq: command.seq,
                            lang: command.lang.clone(),
                            shell: command.shell,
                            timestamp: command.timestamp.clone(),
                            matches: 1 + matches.count(),
                            snippet: snippet(&command.text, start, end),
                        });
                    }
                }
                true
            });
        group.counts.commands += count;
        group.more |= count > hits.len();
        group.hits.extend(hits.into_iter().skip(page.skip));
    }

    /// Messages of session `id`'s current conversation: `before` up to
    /// and including message `around` and `after` after it (the newest
    /// `before` when `around` is `None`). Reads what the file has new
    /// first, within [`TIME_BUDGET`]. Blocking.
    pub fn transcript_page(
        &self,
        id: &str,
        around: Option<u64>,
        before: usize,
        after: usize,
    ) -> Result<TranscriptPage, HubError> {
        let view = self.session(id)?;
        let record = &view.record;
        if !record.kind.is_agent() {
            return Err(StateError::Invalid(format!(
                "session {} is a shell: it has no agent transcript",
                record.label()
            ))
            .into());
        }
        let mut page = TranscriptPage {
            session_id: record.id.clone(),
            conversation: record.agent_session_id.clone(),
            transcript: None,
            first: 0,
            next: 0,
            complete: true,
            messages: Vec::new(),
            error: None,
        };
        let Some((path, format)) = self.transcript_of(&view) else {
            return Ok(page);
        };
        let slot = self.search_index().transcript(&path, format);
        let mut index = slot.lock();
        page.complete = index.update(Instant::now() + TIME_BUDGET);
        slot.settle(&index);
        page.transcript = Some(path);
        page.first = index.first_index();
        page.next = index.next_index;
        page.error = index.error.clone();
        let before = before.min(MAX_PAGE_MESSAGES);
        let after = after.min(MAX_PAGE_MESSAGES - before);
        let (from, to) = match around {
            Some(at) => (
                at.saturating_sub(before as u64),
                at.saturating_add(after as u64).saturating_add(1),
            ),
            None => (
                index.next_index.saturating_sub(before as u64),
                index.next_index,
            ),
        };
        let first = index.first_index();
        for i in from.max(first)..to.min(index.next_index) {
            let unit = (i - first) as usize;
            page.messages.push(PageMessage {
                meta: index.messages[unit].clone(),
                text: index.corpus.unit(unit).to_owned(),
            });
        }
        drop(index);
        drop(slot);
        self.search_index().sweep();
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn literal_queries_match_case_insensitively_and_escape_metacharacters() {
        let m = Matcher::new("a.b(", false, false).unwrap();
        assert!(m.is_match("x A.B( y"));
        assert!(!m.is_match("axb("));
        let m = Matcher::new("Error", false, true).unwrap();
        assert!(m.is_match("an Error"));
        assert!(!m.is_match("an error"));
    }

    #[test]
    fn regex_queries_compile_with_limits_and_skip_empty_matches() {
        let m = Matcher::new(r"^cargo (test|build)$", true, false).unwrap();
        let corpus =
            Corpus::from_units(["cargo test", "  cargo test", "CARGO BUILD", "cargo testing"]);
        let found = scan(&corpus, &m, 10, far());
        let units: Vec<usize> = found.hits.iter().map(|h| h.unit).collect();
        assert_eq!(units, [0, 2], "^ and $ anchor at line boundaries");
        assert!(
            Matcher::new("(", true, false)
                .unwrap_err()
                .contains("invalid regex")
        );
        // A pattern whose compiled form is huge is refused, not built.
        let huge = Matcher::new(r"\w{1000}\w{1000}\w{1000}", true, false).unwrap_err();
        assert!(huge.contains("too large"), "{huge}");
        assert!(Matcher::new("", false, false).is_err());
        assert!(Matcher::new(&"x".repeat(MAX_QUERY_BYTES + 1), false, false).is_err());
        // `a*` matches the empty string everywhere: only real matches count.
        let m = Matcher::new("z*", true, false).unwrap();
        let found = scan(&Corpus::from_units(["abc", "zz", "q"]), &m, 10, far());
        assert_eq!(found.units, 1);
        assert_eq!(found.hits[0].unit, 1);
    }

    #[test]
    fn scans_keep_the_newest_hits_count_all_and_one_per_unit() {
        let lines: Vec<String> = (0..100)
            .map(|i| format!("line {i} needle needle"))
            .collect();
        let corpus = Corpus::from_units(&lines);
        let found = scan(
            &corpus,
            &Matcher::new("needle", false, false).unwrap(),
            3,
            far(),
        );
        assert!(found.complete);
        assert_eq!(found.units, 100);
        let units: Vec<usize> = found.hits.iter().map(|h| h.unit).collect();
        assert_eq!(units, [97, 98, 99]);
        assert!(found.hits.iter().all(|h| h.matches == 2));
        let last = found.hits[2];
        assert_eq!(&corpus.unit(99)[last.start..last.end], "needle");
        assert_eq!(last.start, "line 99 ".len());
    }

    #[test]
    fn a_spent_budget_stops_the_scan() {
        let lines: Vec<String> = (0..20_000).map(|i| format!("x{i}")).collect();
        let corpus = Corpus::from_units(&lines);
        let found = scan(
            &corpus,
            &Matcher::new("x", false, false).unwrap(),
            5,
            Instant::now(),
        );
        assert!(!found.complete);
        assert!(found.units < 20_000);
    }

    #[test]
    fn matches_never_run_across_units() {
        let corpus = Corpus::from_units(["build done", "next step"]);
        let spanning = Matcher::new(r"done\s+next", true, false).unwrap();
        let found = scan(&corpus, &spanning, 5, far());
        assert_eq!(found.units, 0, "neither unit holds the pattern");
        // A separator-only match is no hit (no empty match text).
        for pattern in [r"\s+$", r"\n", r"\s"] {
            let m = Matcher::new(pattern, true, false).unwrap();
            let found = scan(&Corpus::from_units(["ab", "cd", "ef"]), &m, 5, far());
            assert_eq!(found.units, 0, "{pattern}");
        }
        // A crossing match is retried inside the unit: `o\s\w+` crosses
        // from "go" into "on" but "do it" holds its own match later on.
        let corpus = Corpus::from_units(["do it go", "on", "multi\nline x"]);
        let m = Matcher::new(r"o\s\w+", true, false).unwrap();
        let found = scan(&corpus, &m, 5, far());
        let hits: Vec<(usize, &str)> = found
            .hits
            .iter()
            .map(|h| (h.unit, &corpus.unit(h.unit)[h.start..h.end]))
            .collect();
        assert_eq!(hits, [(0, "o it")]);
        assert_eq!(
            found.hits[0].matches, 1,
            "the crossing \"o\\non\" is not counted"
        );
        // Newlines inside one unit (a message, a block) still match.
        let m = Matcher::new(r"multi\nline", true, false).unwrap();
        let found = scan(&corpus, &m, 5, far());
        assert_eq!(found.hits[0].unit, 2);
        // `^`/`$` still see unit edges.
        let m = Matcher::new(r"^on$", true, false).unwrap();
        assert_eq!(scan(&corpus, &m, 5, far()).hits[0].unit, 1);
    }

    #[test]
    fn corpus_units_and_front_drops() {
        let mut corpus = Corpus::from_units(["a", "", "multi\nline", "z"]);
        assert_eq!(corpus.len(), 4);
        assert_eq!(corpus.unit(1), "");
        assert_eq!(corpus.unit(2), "multi\nline");
        assert_eq!(corpus.unit_of(corpus.starts[2] + 7), 2);
        assert_eq!(
            corpus.unit_of(1),
            0,
            "a separator belongs to the unit before"
        );
        corpus.drop_front(2);
        assert_eq!(corpus.len(), 2);
        assert_eq!(corpus.unit(0), "multi\nline");
        assert_eq!(corpus.unit(1), "z");
        corpus.drop_front(5);
        assert_eq!(corpus.len(), 0);
    }

    #[test]
    fn snippets_show_the_match_line_in_bounded_context() {
        let s = snippet("first\n  run cargo test now\nlast", 12, 17);
        assert_eq!(
            (s.before.as_str(), s.matched.as_str(), s.after.as_str()),
            ("run ", "cargo", " test now")
        );
        assert_eq!(s.line, 1);
        let long = format!("{}needle{}", "a".repeat(500), "b\tc".repeat(200));
        let s = snippet(&long, 500, 506);
        assert!(s.before.starts_with('…'));
        assert_eq!(s.before.chars().count(), BEFORE_CHARS + 1);
        assert_eq!(s.matched, "needle");
        assert!(s.after.ends_with('…'));
        assert!(!s.after.contains('\t'), "controls become spaces");
        // Multi-byte text is cut on character boundaries.
        let text = format!("{}é needle ü{}", "ü".repeat(100), "é".repeat(300));
        let at = text.find("needle").unwrap();
        let s = snippet(&text, at, at + 6);
        assert_eq!(s.matched, "needle");
        assert!(s.before.ends_with("é "));
    }

    #[test]
    fn claude_lines_give_text_and_tool_calls_but_not_thinking_or_results() {
        let line = json!({
            "type": "assistant", "uuid": "a1", "timestamp": "2026-11-02T10:00:00Z",
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret plan"},
                {"type": "text", "text": "I will run the tests."},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test -p hub", "description": "Run tests"}}
            ]}
        });
        let messages = claude_messages(&line);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "I will run the tests.");
        assert_eq!(messages[0].role, Role::Assistant);
        assert_eq!(messages[1].kind, MessageKind::Tool);
        assert_eq!(messages[1].tool.as_deref(), Some("Bash"));
        assert_eq!(messages[1].id, "a1#tool0");
        assert!(messages[1].text.contains("command: cargo test -p hub"));
        let user = json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "fix the login bug"}});
        assert_eq!(claude_messages(&user)[0].role, Role::User);
        let result = json!({"type": "user", "uuid": "u2", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}});
        assert!(claude_messages(&result).is_empty());
        let side = json!({"type": "assistant", "uuid": "s", "isSidechain": true,
            "message": {"content": "subagent"}});
        assert!(claude_messages(&side).is_empty());
        assert!(claude_messages(&json!({"type": "summary", "summary": "x"})).is_empty());
    }

    #[test]
    fn codex_lines_give_messages_and_calls_without_injected_context() {
        let reply = json!({"timestamp": "2026-11-02T10:00:00Z", "type": "response_item", "payload": {
            "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}});
        let m = codex_messages(&reply, 42);
        assert_eq!(m[0].text, "Done.");
        assert_eq!(m[0].id, "@42");
        let user = json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "<environment_context>cwd</environment_context>"}]}});
        assert!(codex_messages(&user, 0).is_empty());
        let call = json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
            "arguments": "{\"command\":[\"bash\",\"-lc\",\"make test\"]}", "call_id": "c1"}});
        let m = codex_messages(&call, 0);
        assert_eq!(m[0].kind, MessageKind::Tool);
        assert_eq!(m[0].id, "c1");
        assert!(m[0].text.contains("make test"), "{}", m[0].text);
        let local = json!({"type": "local_shell_call", "action": {"type": "exec", "command": ["ls", "-la"]}});
        assert_eq!(codex_messages(&local, 7)[0].text, "shell\nls -la");
        let dev = json!({"type": "response_item", "payload": {"type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": "rules"}]}});
        assert!(codex_messages(&dev, 0).is_empty());
    }

    #[test]
    fn transcript_indexes_read_incrementally_and_reread_replaced_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let line = |uuid: &str, text: &str| {
            json!({"type": "assistant", "uuid": uuid, "message": {"content": text}}).to_string()
        };
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "{}", line("a", "first answer")).unwrap();
        // A half-written line is held until its newline arrives.
        write!(
            file,
            "{{\"type\":\"user\",\"uuid\":\"b\",\"message\":{{\"content\":\"sec"
        )
        .unwrap();
        file.flush().unwrap();
        let mut index = TranscriptIndex::new(path.clone(), Format::Claude);
        assert!(index.update(far()));
        assert_eq!(index.messages.len(), 1);
        writeln!(file, "ond prompt\"}}}}").unwrap();
        writeln!(file, "not json").unwrap();
        assert!(index.update(far()));
        assert_eq!(index.messages.len(), 2);
        assert_eq!(index.corpus.unit(1), "second prompt");
        assert_eq!(index.messages[1].role, Role::User);
        // Replaced (new inode): read from the start, renumbered.
        let other = dir.path().join("new.jsonl");
        std::fs::write(&other, format!("{}\n", line("z", "fresh"))).unwrap();
        std::fs::rename(&other, &path).unwrap();
        assert!(index.update(far()));
        assert_eq!(index.messages.len(), 1);
        assert_eq!(index.corpus.unit(0), "fresh");
        // Gone: an error, nothing kept from the failed read.
        std::fs::remove_file(&path).unwrap();
        index.update(far());
        assert!(index.error.as_deref().unwrap().contains("cannot read"));
    }

    #[test]
    fn the_transcript_cap_counts_message_metadata_not_only_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        // Tool calls with a one-character name and no input: next to no
        // text, but a uuid, timestamp, and tool name each.
        let mut text = String::new();
        for i in 0..2000 {
            text.push_str(
                &json!({"type": "assistant", "uuid": format!("{i:036}"),
                    "timestamp": "2026-11-02T10:11:12.000Z",
                    "message": {"content": [{"type": "tool_use", "name": "x", "input": {}}]}})
                .to_string(),
            );
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        let mut index = TranscriptIndex::new(path, Format::Claude);
        index.cap = 64 * 1024;
        assert!(index.update(far()));
        assert!(
            index.corpus.text.len() < 16 * 1024,
            "the text alone is far under the cap"
        );
        assert!(
            index.footprint() <= index.cap,
            "{} > {}",
            index.footprint(),
            index.cap
        );
        assert!(index.first_index() > 0, "the oldest were dropped");
        assert_eq!(index.next_index, 2000);
        assert_eq!(index.messages.len(), index.corpus.len());
        let counted: usize = index.messages.iter().map(meta_cost).sum();
        assert_eq!(index.meta_bytes, counted);
    }

    #[test]
    fn idle_and_least_recently_used_transcript_indexes_are_evicted() {
        let search = SearchIndex::default();
        let slot = |name: &str, bytes: usize, ago: u64| {
            let slot = search.transcript(Path::new(name), Format::Claude);
            slot.bytes.store(bytes, Ordering::Relaxed);
            *slot.used.lock().unwrap() = Instant::now() - Duration::from_secs(ago);
            slot
        };
        drop(slot("/a", 100, 50));
        drop(slot("/b", 100, 40));
        drop(slot("/c", 100, 30));
        drop(slot("/idle", 100, 1000));
        // One a search holds right now is never dropped, however old.
        let busy = slot("/busy", 100, 2000);
        assert_eq!(search.transcript_count(), 5);
        search.sweep_with(1000, Duration::from_secs(600));
        assert_eq!(search.transcript_count(), 4, "the idle one went");
        // Over the cap: the least recently used go first.
        search.sweep_with(250, Duration::from_secs(600));
        let left: HashSet<PathBuf> = search.transcripts.lock().unwrap().keys().cloned().collect();
        assert_eq!(left, HashSet::from(["/c".into(), "/busy".into()]));
        assert_eq!(search.transcript_bytes(), 200);
        drop(busy);
        search.sweep_with(250, Duration::from_secs(600));
        assert_eq!(search.transcript_count(), 1, "the idle busy one, now free");
    }

    #[test]
    fn over_long_lines_and_messages_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let big = "x".repeat(MAX_MESSAGE_BYTES + 100);
        let mut text =
            json!({"type": "assistant", "uuid": "a", "message": {"content": big}}).to_string();
        text.push('\n');
        text.push_str(&"y".repeat(MAX_LINE_BYTES + 10));
        text.push('\n');
        text.push_str(
            &json!({"type": "assistant", "uuid": "b", "message": {"content": "after"}}).to_string(),
        );
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        let mut index = TranscriptIndex::new(path, Format::Claude);
        assert!(index.update(far()));
        assert_eq!(index.messages.len(), 2, "the over-long line is skipped");
        assert!(index.messages[0].truncated);
        assert_eq!(index.corpus.unit(0).len(), MAX_MESSAGE_BYTES);
        assert_eq!(index.corpus.unit(1), "after");
    }

    #[test]
    fn transcript_reading_stops_at_the_deadline_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut text = String::new();
        for i in 0..20_000 {
            text.push_str(&json!({"type": "user", "uuid": format!("u{i}"), "message": {"content": format!("prompt {i}")}}).to_string());
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        let mut index = TranscriptIndex::new(path, Format::Claude);
        assert!(!index.update(Instant::now()), "no time: nothing read yet");
        assert!(index.update(far()));
        assert_eq!(index.messages.len(), 20_000);
        assert_eq!(index.messages[19_999].index, 19_999);
    }
}
