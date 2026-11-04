//! Transcript messages as search units: what each parsed line yields,
//! and the bounded per-file index read incrementally.

use super::scan::Corpus;
use super::{MAX_MESSAGE_BYTES, MAX_TRANSCRIPT_BYTES, MessageKind};
use crate::agentlog::{self, Event, Format, JsonlTail, Line, Read, Role};
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

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
    /// A tool call's or result's call id (pairs them up).
    pub call: Option<String>,
    /// A tool result that reports a failure.
    pub error: bool,
}

fn tool_message(
    id: String,
    name: &str,
    input: String,
    call: Option<String>,
    timestamp: Option<String>,
) -> Extracted {
    Extracted {
        id,
        role: Role::Assistant,
        kind: MessageKind::Tool,
        tool: Some(name.to_owned()),
        timestamp,
        call,
        error: false,
        text: if input.is_empty() {
            name.to_owned()
        } else {
            format!("{name}\n{input}")
        },
    }
}

/// The searchable messages in a parsed transcript line (see
/// [`crate::agentlog`]): its text as one message (Claude's parts joined
/// by blank lines, Codex's by newlines; blank text left out), each tool
/// call as another, and each tool result (its tool named by the index
/// that pairs it with its call). Thinking and images are left out.
pub fn messages_of(format: Format, line: Line) -> Vec<Extracted> {
    let joiner = match format {
        Format::Claude => "\n\n",
        Format::Codex => "\n",
    };
    let mut out = Vec::new();
    for event in line.events {
        match event {
            Event::Text { role, id, parts } => {
                let text = parts
                    .iter()
                    .map(|p| p.text.as_str())
                    .collect::<Vec<_>>()
                    .join(joiner);
                if !text.trim().is_empty() {
                    out.push(Extracted {
                        id,
                        role,
                        kind: MessageKind::Text,
                        tool: None,
                        timestamp: line.timestamp.clone(),
                        text,
                        call: None,
                        error: false,
                    });
                }
            }
            Event::ToolCall {
                id,
                name,
                input,
                call,
            } => {
                out.push(tool_message(id, &name, input, call, line.timestamp.clone()));
            }
            Event::ToolResult {
                id,
                call,
                output,
                error,
            } => out.push(Extracted {
                id,
                role: Role::User,
                kind: MessageKind::Result,
                tool: None,
                timestamp: line.timestamp.clone(),
                text: output,
                call: Some(call),
                error,
            }),
            Event::Session(_) | Event::Model(_) | Event::Usage(_) | Event::TokenCount { .. } => {}
        }
    }
    out
}

/// The messages in a Claude Code transcript line: a user or assistant
/// line of the main conversation (not a subagent's sidechain) -- its text
/// parts as one message, each `tool_use` and `tool_result` part as
/// another. Thinking and images are left out.
pub fn claude_messages(value: &Value) -> Vec<Extracted> {
    messages_of(Format::Claude, agentlog::parse(Format::Claude, value, 0))
}

/// The messages in a Codex rollout line (`response_item`, or the bare
/// item older rollouts write): user and assistant messages, and function,
/// custom tool, and local shell calls. `offset` (the line's byte offset)
/// names items without an id.
pub fn codex_messages(value: &Value, offset: u64) -> Vec<Extracted> {
    messages_of(Format::Codex, agentlog::parse(Format::Codex, value, offset))
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
    /// A tool result reporting a failure.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
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
pub(crate) struct TranscriptIndex {
    tail: JsonlTail,
    format: Format,
    pub(crate) messages: VecDeque<MessageMeta>,
    pub(crate) corpus: Corpus,
    pub(crate) next_index: u64,
    /// Why the file could not be read last time.
    pub(crate) error: Option<String>,
    /// Heap held by `messages` (see [`meta_cost`]).
    pub(crate) meta_bytes: usize,
    /// Most bytes it holds ([`MAX_TRANSCRIPT_BYTES`]).
    pub(crate) cap: usize,
    /// The newest tool calls' ids and tool names, to name their results.
    calls: VecDeque<(String, String)>,
}

/// Tool calls remembered to name their results (results follow their
/// calls closely; parallel calls are a handful).
const CALLS_KEPT: usize = 64;

/// Memory one message's metadata takes, counted toward
/// [`MAX_TRANSCRIPT_BYTES`] with its text.
pub(crate) fn meta_cost(meta: &MessageMeta) -> usize {
    std::mem::size_of::<MessageMeta>()
        + meta.message_id.len()
        + meta.tool.as_ref().map_or(0, String::len)
        + meta.timestamp.as_ref().map_or(0, String::len)
}

impl TranscriptIndex {
    pub(crate) fn new(path: PathBuf, format: Format) -> Self {
        Self {
            tail: JsonlTail::new(path),
            format,
            messages: VecDeque::new(),
            corpus: Corpus::default(),
            next_index: 0,
            error: None,
            meta_bytes: 0,
            cap: MAX_TRANSCRIPT_BYTES,
            calls: VecDeque::new(),
        }
    }

    /// Forget every message (the file is read again from the start).
    fn clear(&mut self) {
        self.messages.clear();
        self.corpus = Corpus::default();
        self.next_index = 0;
        self.error = None;
        self.meta_bytes = 0;
        self.calls.clear();
    }

    /// Read what the file has new, until `deadline`; returns whether it
    /// was read to its end.
    pub(crate) fn update(&mut self, deadline: Instant) -> bool {
        let mut tail = std::mem::take(&mut self.tail);
        let read = tail.read(Some(deadline), &mut |read| match read {
            Read::Restart => self.clear(),
            Read::Line { bytes, start } => self.line(bytes, start),
        });
        self.tail = tail;
        match read {
            Ok(complete) => {
                self.error = None;
                complete
            }
            Err(e) => {
                self.error = Some(format!("cannot read {}: {e}", self.tail.path.display()));
                true
            }
        }
    }

    fn line(&mut self, line: &[u8], offset: u64) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let messages = messages_of(self.format, agentlog::parse(self.format, &value, offset));
        for mut message in messages {
            match (message.kind, &message.call) {
                (MessageKind::Tool, Some(call)) => {
                    if self.calls.len() == CALLS_KEPT {
                        self.calls.pop_front();
                    }
                    let name = message.tool.clone().unwrap_or_default();
                    self.calls.push_back((call.clone(), name));
                }
                (MessageKind::Result, Some(call)) => {
                    message.tool = self
                        .calls
                        .iter()
                        .rev()
                        .find(|(id, _)| id == call)
                        .map(|(_, name)| name.clone());
                }
                _ => {}
            }
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
                error: message.error,
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
    pub(crate) fn footprint(&self) -> usize {
        self.corpus.text.len()
            + self.corpus.starts.len() * std::mem::size_of::<usize>()
            + self.meta_bytes
            + self.tail.pending_len()
    }

    /// Oldest index still held.
    pub(crate) fn first_index(&self) -> u64 {
        self.messages.front().map_or(self.next_index, |m| m.index)
    }
}
