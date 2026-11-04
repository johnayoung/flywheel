//! Reading agent transcripts: one parser and one tail for every consumer.
//!
//! Claude Code writes `<config>/projects/<dir>/<conversation>.jsonl`;
//! Codex writes `<codex home>/sessions/YYYY/MM/DD/rollout-*.jsonl`. Both
//! are JSON Lines appended to while the agent runs. [`JsonlTail`] reads
//! such a file incrementally from a byte offset (bounded per line, a
//! replaced or truncated file read again from the start); [`parse`] turns
//! one line into typed [`Event`]s. The commands drawer
//! ([`crate::transcript`]), token usage ([`crate::usage`]), and search and
//! the transcript viewer ([`crate::search`]) each project the events they
//! need.
//!
//! What the parser decides for every consumer:
//!
//! * a Claude line on a sidechain (a subagent's turn) reports only its
//!   usage, never its text or tool calls;
//! * a Claude line's id is its `uuid` (else `message.id`); a line with
//!   neither has no text or tool calls; the usage of one API response is
//!   keyed by `message.id` (else `uuid`), so repeats replace each other;
//! * a Codex item's id is its `id` (else `call_id`), else `@<offset>` (the
//!   line's byte offset: rollouts do not write message ids, and several
//!   items can share a millisecond);
//! * context Codex injects as user messages (`<environment_context>`,
//!   `<user_instructions>`) is not the user's words and is dropped.

mod tail;

pub use tail::{JsonlTail, Read};

use crate::usage::{CodexTotals, Tokens, Turn};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A transcript line longer than this is skipped unread (tool output, not
/// a message worth keeping).
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Read size per syscall.
pub const READ_CHUNK: usize = 256 * 1024;

/// Which agent's file format a transcript is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Claude,
    Codex,
}

/// Who wrote a transcript message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// What kind of text part a message part is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    /// Claude's `text`.
    Text,
    /// Codex's `input_text`.
    Input,
    /// Codex's `output_text`.
    Output,
}

/// One text part of a message, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub kind: PartKind,
    pub text: String,
}

/// What one transcript line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The conversation the line belongs to: Claude's `sessionId` (on
    /// every line), Codex's `session_meta`.
    Session(String),
    /// The model in use: Claude's on each reply, Codex's `turn_context`
    /// for the turns that follow.
    Model(String),
    /// A user's or the assistant's message: its text parts, in order.
    Text {
        role: Role,
        id: String,
        parts: Vec<Part>,
    },
    /// A tool call: the tool's name and its input as text ([`tool_text`]);
    /// `call` names it for its result (Claude's `tool_use` id, Codex's
    /// `call_id`).
    ToolCall {
        id: String,
        name: String,
        input: String,
        call: Option<String>,
    },
    /// What a tool call returned (Claude's `tool_result`, Codex's
    /// `function_call_output`): its output as text, for call `call`.
    ToolResult {
        id: String,
        call: String,
        output: String,
        error: bool,
    },
    /// One Claude API response's usage.
    Usage(Turn),
    /// A Codex `token_count` event: the conversation's running totals
    /// and/or the last call's counts.
    TokenCount {
        total: Option<CodexTotals>,
        last: Option<CodexTotals>,
    },
}

/// One parsed transcript line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub timestamp: Option<String>,
    pub events: Vec<Event>,
}

/// Parse one transcript line (`offset`: where it starts in its file).
pub fn parse(format: Format, value: &Value, offset: u64) -> Line {
    let mut line = Line {
        timestamp: str_of(value, "timestamp").map(str::to_owned),
        events: Vec::new(),
    };
    match format {
        Format::Claude => claude(value, &mut line.events),
        Format::Codex => codex(value, offset, &mut line.events),
    }
    line
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
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

// --- Claude Code --------------------------------------------------------

fn claude(value: &Value, events: &mut Vec<Event>) {
    if let Some(session) = str_of(value, "sessionId") {
        events.push(Event::Session(session.to_owned()));
    }
    let role = match str_of(value, "type") {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return,
    };
    let Some(message) = value.get("message") else {
        return;
    };
    if role == Role::Assistant {
        if let Some(model) = str_of(message, "model").filter(|m| real_model(m)) {
            events.push(Event::Model(model.to_owned()));
        }
        if let Some(turn) = claude_usage(value, message) {
            events.push(Event::Usage(turn));
        }
    }
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let Some(id) = str_of(value, "uuid").or_else(|| str_of(message, "id")) else {
        return;
    };
    let mut parts = Vec::new();
    let mut tools = Vec::new();
    let mut results = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => parts.push(Part {
            kind: PartKind::Text,
            text: text.clone(),
        }),
        Some(Value::Array(content)) => {
            for part in content {
                match str_of(part, "type") {
                    Some("text") => parts.extend(str_of(part, "text").map(|text| Part {
                        kind: PartKind::Text,
                        text: text.to_owned(),
                    })),
                    Some("tool_use") => tools.push(part),
                    Some("tool_result") => results.push(part),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if !parts.is_empty() {
        events.push(Event::Text {
            role,
            id: id.to_owned(),
            parts,
        });
    }
    for (i, part) in tools.into_iter().enumerate() {
        events.push(Event::ToolCall {
            id: format!("{id}#tool{i}"),
            name: str_of(part, "name").unwrap_or("tool").to_owned(),
            input: part.get("input").map(tool_text).unwrap_or_default(),
            call: str_of(part, "id").map(str::to_owned),
        });
    }
    for (i, part) in results.into_iter().enumerate() {
        events.push(Event::ToolResult {
            id: format!("{id}#result{i}"),
            call: str_of(part, "tool_use_id").unwrap_or_default().to_owned(),
            output: part.get("content").map(result_text).unwrap_or_default(),
            error: part.get("is_error").and_then(Value::as_bool) == Some(true),
        });
    }
}

/// A tool result's content as text: a string as written, or its text
/// parts joined by newlines (an image part says so).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match str_of(p, "type") {
                Some("text") => str_of(p, "text").map(str::to_owned),
                Some("image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A model that bills (synthetic messages are written as `<synthetic>`).
fn real_model(model: &str) -> bool {
    !model.is_empty() && !model.starts_with('<')
}

/// The usage on a Claude Code `assistant` line: `message.usage` with a
/// real model.
fn claude_usage(value: &Value, message: &Value) -> Option<Turn> {
    let usage = message.get("usage").filter(|u| u.is_object())?;
    let model = str_of(message, "model").filter(|m| real_model(m))?;
    let written = count(usage, "cache_creation_input_tokens");
    let (write_5m, write_1h) = match usage.get("cache_creation") {
        Some(split) if split.is_object() => (
            count(split, "ephemeral_5m_input_tokens"),
            count(split, "ephemeral_1h_input_tokens"),
        ),
        // No split: the default (5-minute) TTL.
        _ => (written, 0),
    };
    let tokens = Tokens {
        input: count(usage, "input_tokens"),
        output: count(usage, "output_tokens"),
        cache_read: count(usage, "cache_read_input_tokens"),
        cache_write_5m: write_5m,
        cache_write_1h: write_1h,
    };
    if tokens.is_zero() {
        return None;
    }
    Some(Turn {
        id: str_of(message, "id")
            .or_else(|| str_of(value, "uuid"))
            .map(str::to_owned),
        model: model.to_owned(),
        fast: str_of(usage, "speed") == Some("fast"),
        timestamp: str_of(value, "timestamp").map(str::to_owned),
        tokens,
    })
}

// --- Codex --------------------------------------------------------------

/// Context Codex injects as user messages (not the user's words).
const CODEX_CONTEXT_PREFIXES: &[&str] = &["<environment_context>", "<user_instructions>"];

fn codex(value: &Value, offset: u64, events: &mut Vec<Event>) {
    let item = match str_of(value, "type") {
        Some("session_meta") => {
            if let Some(id) = value.get("payload").and_then(|p| str_of(p, "id")) {
                events.push(Event::Session(id.to_owned()));
            }
            return;
        }
        Some("turn_context") => {
            if let Some(model) = value
                .get("payload")
                .and_then(|p| str_of(p, "model"))
                .filter(|m| !m.is_empty())
            {
                events.push(Event::Model(model.to_owned()));
            }
            return;
        }
        Some("event_msg") => {
            events.extend(value.get("payload").and_then(codex_token_count));
            return;
        }
        Some("response_item") => match value.get("payload") {
            Some(payload) => payload,
            None => return,
        },
        // Older rollouts write the item bare.
        _ => value,
    };
    let id = || {
        str_of(item, "id")
            .or_else(|| str_of(item, "call_id"))
            .map_or_else(|| format!("@{offset}"), str::to_owned)
    };
    let call = |name: &str, input: String| Event::ToolCall {
        id: id(),
        name: name.to_owned(),
        input,
        call: str_of(item, "call_id").map(str::to_owned),
    };
    let event = match str_of(item, "type") {
        Some("message") => {
            let role = match str_of(item, "role") {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => return,
            };
            let parts: Vec<Part> = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|p| {
                    let kind = match str_of(p, "type") {
                        Some("input_text") => PartKind::Input,
                        Some("output_text") => PartKind::Output,
                        _ => return None,
                    };
                    str_of(p, "text").map(|text| Part {
                        kind,
                        text: text.to_owned(),
                    })
                })
                .collect();
            if parts.is_empty() || (role == Role::User && injected_context(&parts)) {
                return;
            }
            Event::Text {
                role,
                id: id(),
                parts,
            }
        }
        Some("function_call") => call(
            str_of(item, "name").unwrap_or("function"),
            item.get("arguments").map(tool_text).unwrap_or_default(),
        ),
        Some("custom_tool_call") => call(
            str_of(item, "name").unwrap_or("tool"),
            item.get("input").map(tool_text).unwrap_or_default(),
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
            call("shell", command)
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let Some(call) = str_of(item, "call_id") else {
                return;
            };
            // A JSON string `{"output": ..., "metadata": ...}`, or the text.
            let output = match item.get("output") {
                Some(Value::String(text)) => serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|v| v.get("output").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_else(|| text.clone()),
                Some(other) => result_text(other),
                None => String::new(),
            };
            Event::ToolResult {
                id: format!("{}#result", id()),
                call: call.to_owned(),
                output,
                error: false,
            }
        }
        _ => return,
    };
    events.push(event);
}

/// Whether a user message is context Codex injected: its text (parts
/// joined by newlines) starts, after blanks, with a context tag.
fn injected_context(parts: &[Part]) -> bool {
    let text = parts
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim_start();
    CODEX_CONTEXT_PREFIXES
        .iter()
        .any(|p| trimmed.starts_with(p))
}

/// A Codex `token_count` event's counts.
fn codex_token_count(payload: &Value) -> Option<Event> {
    if str_of(payload, "type")? != "token_count" {
        return None;
    }
    let info = payload.get("info").filter(|i| i.is_object())?;
    let total = info
        .get("total_token_usage")
        .and_then(CodexTotals::from_json);
    let last = info
        .get("last_token_usage")
        .and_then(CodexTotals::from_json);
    (total.is_some() || last.is_some()).then_some(Event::TokenCount { total, last })
}

#[cfg(test)]
mod tests;
