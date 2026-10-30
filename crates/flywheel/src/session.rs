//! Pure transcript domain model + cursor-driven tailer for one run.
//!
//! The session screen renders a chat-style transcript of one run's merged
//! audit stream. The shape of "what to render" is a function of
//! [`AuditRecord`] only, so this module carries no terminal code and is
//! exercised by ordinary unit tests.
//!
//! Two surfaces are exported:
//!
//! * [`classify`] -- convert one [`AuditRecord`] into zero or more
//!   [`TranscriptEntry`] values, each tagged with an [`EntryKind`] (agent
//!   text / tool call / tool result / operator say / lifecycle / gate /
//!   system / result / user text). Multiple content blocks inside one
//!   `AssistantMessage` produce multiple entries that share the record's
//!   `sequence` and disambiguate via `sub_index` so the strict per-run
//!   ordering survives a fan-out.
//! * [`TranscriptTailer`] -- cursor-incremental reader over the run's
//!   telemetry JSONL file. Each `fetch` returns only the lines appended
//!   since the last call (tailing never re-reads full history per tick).
//!   Records are piped through the same default redaction policy the audit
//!   CLI applies on read so the session inherits the operator-facing read
//!   path's best-effort secret suppression.

use chrono::{DateTime, Utc};
use flywheel_core::audit::{FileCursor, read_records_since};
use flywheel_core::lifecycle::Status;
use flywheel_core::redaction::{
    ComposedRedactor, EnvValueRedactor, Redactor, compose, default_policy,
};
use flywheel_core::store::{AuditRecord, EventRecord, JsonObject, SdkMessageRecord};
use flywheel_core::telemetry_file::{JsonStyle, python_json_dumps};
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};

/// Lifecycle statuses at which a run no longer makes forward progress.
///
/// Mirrors the audit follow-loop's exit predicate so the session view's
/// terminal-banner gate stays aligned with the stream's stopping condition.
pub const TERMINAL_STATUSES: [Status; 5] = [
    Status::Done,
    Status::Failed,
    Status::FailedValidation,
    Status::InternalError,
    Status::Interrupted,
];

/// Whether `status` is a leaf in the lifecycle state graph for the session
/// view (the screen and the `--json` snapshot share one predicate).
pub fn is_terminal(status: Status) -> bool {
    TERMINAL_STATUSES.contains(&status)
}

/// Maximum characters retained from a tool-call argument or tool-result
/// body when collapsing into a transcript line.
const COLLAPSED_BODY_LIMIT: usize = 240;

/// Maximum characters retained from an individual `key=value` pair when
/// summarising tool-call args.
const TOOL_ARG_VALUE_LIMIT: usize = 60;

/// Maximum lines of detail retained on an `is_error` tool result; anything
/// beyond is replaced with a `... +N more lines` suffix.
const TOOL_ERROR_LINE_LIMIT: usize = 10;

/// Per-record cap on the number of content blocks fanned out into separate
/// transcript entries.
const MAX_BLOCKS_PER_MESSAGE: usize = 32;

/// Environment variables whose ambient values seed the default-policy
/// [`EnvValueRedactor`] so the operator's own key is redacted out of the
/// transcript even when an agent has echoed it.
const DEFAULT_ENV_SEEDS: [&str; 1] = ["ANTHROPIC_API_KEY"];

/// Per-tool primary-argument map for the TOOL_CALL summary line: these
/// tools render their key argument verbatim so the line is scannable at a
/// glance; tools not listed fall back to the generic `k=v, k=v` summary of
/// the first two input keys.
const TOOL_KEY_ARG: [(&str, &str); 6] = [
    ("Edit", "file_path"),
    ("Read", "file_path"),
    ("Write", "file_path"),
    ("Bash", "command"),
    ("Grep", "pattern"),
    ("Glob", "pattern"),
];

/// Telemetry event kinds the session view promotes to dedicated gate lines.
const GATE_EVENT_KINDS: [&str; 3] = [
    "harness.awaiting_approval",
    "harness.manual_approved",
    "harness.manual_rejected",
];

/// The visible classes of transcript line the session screen renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    AgentText,
    ToolCall,
    ToolResult,
    UserText,
    OperatorSay,
    System,
    Result,
    Lifecycle,
    Gate,
}

impl EntryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            EntryKind::AgentText => "agent_text",
            EntryKind::ToolCall => "tool_call",
            EntryKind::ToolResult => "tool_result",
            EntryKind::UserText => "user_text",
            EntryKind::OperatorSay => "operator_say",
            EntryKind::System => "system",
            EntryKind::Result => "result",
            EntryKind::Lifecycle => "lifecycle",
            EntryKind::Gate => "gate",
        }
    }
}

/// One rendered chat-style line in the session transcript.
///
/// A single [`AuditRecord`] may expand into more than one entry when an
/// `AssistantMessage` carries multiple content blocks. All entries from one
/// record share its `sequence` and disambiguate via `sub_index`.
///
/// `header` is the short tag printed at the line head (`agent`,
/// `tool(Edit)`, `operator(say)`, ...); `body` is the collapsed payload.
/// `control_command_id` carries the store-side id of the control command
/// this entry resolves (set on `harness.control_command_applied` /
/// `_failed` events only); `control_command_error` is the human-readable
/// failure detail on the failed variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub sequence: u64,
    pub sub_index: u32,
    pub ts: DateTime<Utc>,
    pub kind: EntryKind,
    pub header: String,
    pub body: String,
    pub attempt_number: Option<u32>,
    pub iteration_number: Option<u32>,
    pub control_command_id: Option<i64>,
    pub control_command_error: Option<String>,
}

// --- Text helpers --------------------------------------------------------------

/// Python's `str()` for a JSON value: strings verbatim, `None` /
/// `True` / `False` for null and booleans, numbers as written, containers
/// as compact Python-style JSON.
fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(n) => n.to_string(),
        other => python_json_dumps(other, &JsonStyle::PYTHON_DEFAULT),
    }
}

/// Python truthiness for a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Flatten `text` to one line and cap it at `limit` characters.
///
/// Newlines become spaces; an over-budget string keeps `limit - 3`
/// characters and ends in an ASCII `...` so the result is exactly `limit`
/// wide.
pub fn short(text: &str, limit: usize) -> String {
    let flat = flatten_to_line(text);
    if flat.chars().count() <= limit {
        return flat;
    }
    let keep = limit.saturating_sub(3).max(1);
    let mut out: String = flat.chars().take(keep).collect();
    out.push_str("...");
    out
}

fn short_value(value: &Value, limit: usize) -> String {
    short(&value_to_text(value), limit)
}

/// Collapse `text` to a single line without truncating it.
fn flatten_to_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// Sanitise an AGENT_TEXT body without flattening or truncating: CRLF and
/// CR become LF, per-line trailing whitespace is stripped, trailing
/// whitespace at the very end is stripped. Leading whitespace survives.
fn normalize_agent_text(text: &str) -> String {
    let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
    let joined = cleaned
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    joined.trim_end().to_owned()
}

/// Whether `value` is an integer (not a float, not a boolean).
fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => n.as_i64(),
        _ => None,
    }
}

/// Whether `value` is a number of either flavour (not a boolean).
fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn optional_str(payload: &JsonObject, key: &str) -> Option<String> {
    match payload.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn require_int(payload: &JsonObject, key: &str) -> Option<i64> {
    payload.get(key).and_then(as_int)
}

/// Collapse a tool_use `input` to a scannable summary (see
/// [`TOOL_KEY_ARG`]). Never fails: an odd payload still renders.
fn summarise_tool_input(name: &str, input: Option<&Value>) -> String {
    let Some(Value::Object(map)) = input else {
        return input.map_or_else(
            || "None".to_owned(),
            |v| short_value(v, COLLAPSED_BODY_LIMIT),
        );
    };
    let primary = TOOL_KEY_ARG
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, key)| *key);
    if let Some(key) = primary
        && let Some(value) = map.get(key)
    {
        let flattened = flatten_to_line(&value_to_text(value));
        if !flattened.is_empty() {
            return flattened;
        }
    }
    if map.is_empty() {
        return String::new();
    }
    map.iter()
        .take(2)
        .map(|(key, value)| format!("{key}={}", short_value(value, TOOL_ARG_VALUE_LIMIT)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render a payload as compact JSON for the lifecycle-line body: sorted
/// keys, no whitespace (Python's `json.dumps(sort_keys=True,
/// separators=(",", ":"))`). An empty payload renders as the empty string.
fn payload_digest(payload: &JsonObject) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let style = JsonStyle {
        item_separator: ",",
        key_separator: ":",
        ensure_ascii: true,
        sort_keys: true,
    };
    python_json_dumps(&Value::Object(payload.clone()), &style)
}

// --- Assistant classification ---------------------------------------------------

/// The one constructor every classifier branch funnels through; the
/// argument count mirrors the entry's own fields.
#[allow(clippy::too_many_arguments)]
fn entry(
    record_ts: DateTime<Utc>,
    sequence: u64,
    sub_index: u32,
    kind: EntryKind,
    header: impl Into<String>,
    body: impl Into<String>,
    attempt_number: Option<u32>,
    iteration_number: Option<u32>,
) -> TranscriptEntry {
    TranscriptEntry {
        sequence,
        sub_index,
        ts: record_ts,
        kind,
        header: header.into(),
        body: body.into(),
        attempt_number,
        iteration_number,
        control_command_id: None,
        control_command_error: None,
    }
}

/// `payload["content"] or []`: the block list, or `None` for a non-list
/// (odd) payload shape.
fn content_blocks(payload: &JsonObject) -> Option<Vec<&Value>> {
    match payload.get("content") {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(value) if !truthy(value) => Some(Vec::new()),
        Some(Value::Array(items)) => Some(items.iter().take(MAX_BLOCKS_PER_MESSAGE).collect()),
        Some(content @ Value::String(_)) => Some(vec![content]),
        Some(_) => None,
    }
}

/// The thinking prose of a block: `thinking` when truthy, else `text`.
fn thinking_text(block: &JsonObject) -> Option<&Value> {
    match block.get("thinking") {
        Some(value) if truthy(value) => Some(value),
        _ => block.get("text"),
    }
}

fn is_thinking_block(block: &JsonObject) -> bool {
    match block.get("type") {
        Some(Value::String(t)) => t == "thinking",
        None => block.contains_key("thinking"),
        Some(_) => false,
    }
}

fn is_tool_use_block(block: &JsonObject) -> bool {
    match block.get("type") {
        Some(Value::String(t)) => t == "tool_use",
        None => block.contains_key("name") && block.contains_key("input"),
        Some(_) => false,
    }
}

/// True for a thinking block whose prose collapses to nothing (the
/// keep-alive `{'thinking': '', 'signature': ...}` shape).
fn is_suppressed_thinking_block(block: &Value) -> bool {
    let Value::Object(map) = block else {
        return false;
    };
    if !is_thinking_block(map) {
        return false;
    }
    match thinking_text(map) {
        Some(Value::String(text)) => normalize_agent_text(text).is_empty(),
        _ => true,
    }
}

fn classify_assistant_block(
    record: &SdkMessageRecord,
    sequence: u64,
    sub_index: u32,
    block: &Value,
) -> Option<TranscriptEntry> {
    let attempt = Some(record.attempt_number);
    let iteration = Some(record.iteration_number);
    let Value::Object(map) = block else {
        let body = short_value(block, COLLAPSED_BODY_LIMIT);
        if body.is_empty() {
            return None;
        }
        return Some(entry(
            record.ts,
            sequence,
            sub_index,
            EntryKind::AgentText,
            "agent",
            body,
            attempt,
            iteration,
        ));
    };
    if is_tool_use_block(map) {
        let name = map
            .get("name")
            .map_or_else(|| "?".to_owned(), value_to_text);
        let summary = summarise_tool_input(&name, map.get("input"));
        return Some(entry(
            record.ts,
            sequence,
            sub_index,
            EntryKind::ToolCall,
            format!("tool({name})"),
            summary,
            attempt,
            iteration,
        ));
    }
    if is_thinking_block(map) {
        let Some(Value::String(text)) = thinking_text(map) else {
            return None;
        };
        let body = normalize_agent_text(text);
        if body.is_empty() {
            return None;
        }
        return Some(entry(
            record.ts,
            sequence,
            sub_index,
            EntryKind::AgentText,
            "agent(thinking)",
            body,
            attempt,
            iteration,
        ));
    }
    if let Some(Value::String(text)) = map.get("text") {
        let body = normalize_agent_text(text);
        if body.is_empty() {
            return None;
        }
        return Some(entry(
            record.ts,
            sequence,
            sub_index,
            EntryKind::AgentText,
            "agent",
            body,
            attempt,
            iteration,
        ));
    }
    let body = short_value(block, COLLAPSED_BODY_LIMIT);
    if body.is_empty() {
        return None;
    }
    Some(entry(
        record.ts,
        sequence,
        sub_index,
        EntryKind::AgentText,
        "agent",
        body,
        attempt,
        iteration,
    ))
}

fn fallback_entry(
    record: &SdkMessageRecord,
    sequence: u64,
    kind: EntryKind,
    header: &str,
) -> Vec<TranscriptEntry> {
    vec![entry(
        record.ts,
        sequence,
        0,
        kind,
        header,
        "(empty)",
        Some(record.attempt_number),
        Some(record.iteration_number),
    )]
}

/// Expand one `AssistantMessage` into per-block transcript entries.
///
/// A message whose content is nothing but empty thinking blocks produces
/// zero entries (not the `(empty)` fallback) so signature-only keep-alive
/// turns leave no trace.
fn classify_assistant(record: &SdkMessageRecord, sequence: u64) -> Vec<TranscriptEntry> {
    let Some(blocks) = content_blocks(&record.payload) else {
        return fallback_entry(record, sequence, EntryKind::AgentText, "agent");
    };
    let mut entries = Vec::new();
    let mut sub_index = 0u32;
    for block in &blocks {
        if let Some(entry) = classify_assistant_block(record, sequence, sub_index, block) {
            entries.push(entry);
            sub_index += 1;
        }
    }
    if entries.is_empty() {
        if !blocks.is_empty() && blocks.iter().all(|b| is_suppressed_thinking_block(b)) {
            return Vec::new();
        }
        return fallback_entry(record, sequence, EntryKind::AgentText, "agent");
    }
    entries
}

// --- User classification --------------------------------------------------------

/// Format a tool result body for the indented outcome line.
///
/// Success collapses to one brief line: `ok`, `ok: <line>` for exactly one
/// line of output, or `ok (N lines)`. Error keeps up to
/// [`TOOL_ERROR_LINE_LIMIT`] lines verbatim plus a `... +N more lines`
/// suffix. Carriage returns are normalised first.
fn format_tool_result_body(text: &str, is_error: bool) -> String {
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
    let normalised = normalised.trim_matches('\n');
    if is_error {
        format_tool_error_body(normalised)
    } else {
        format_tool_success_body(normalised)
    }
}

fn format_tool_success_body(text: &str) -> String {
    if text.trim().is_empty() {
        return "ok".to_owned();
    }
    let non_empty: Vec<&str> = text.split('\n').filter(|l| !l.trim().is_empty()).collect();
    match non_empty.as_slice() {
        [] => "ok".to_owned(),
        [line] => format!("ok: {}", short(line.trim(), COLLAPSED_BODY_LIMIT)),
        lines => format!("ok ({} lines)", lines.len()),
    }
}

fn format_tool_error_body(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= TOOL_ERROR_LINE_LIMIT {
        return text.to_owned();
    }
    let remaining = lines.len() - TOOL_ERROR_LINE_LIMIT;
    format!(
        "{}\n... +{remaining} more lines",
        lines[..TOOL_ERROR_LINE_LIMIT].join("\n")
    )
}

fn classify_user_block(
    record: &SdkMessageRecord,
    sequence: u64,
    sub_index: u32,
    block: &Value,
) -> Option<TranscriptEntry> {
    let attempt = Some(record.attempt_number);
    let iteration = Some(record.iteration_number);
    let Value::Object(map) = block else {
        let body = short_value(block, COLLAPSED_BODY_LIMIT);
        if body.is_empty() {
            return None;
        }
        return Some(entry(
            record.ts,
            sequence,
            sub_index,
            EntryKind::UserText,
            "user",
            body,
            attempt,
            iteration,
        ));
    };
    if map.contains_key("tool_use_id") {
        let raw_text = match map.get("content") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => short_value(other, COLLAPSED_BODY_LIMIT),
        };
        let is_error = map.get("is_error").is_some_and(truthy);
        let header = if is_error {
            "tool_result(error)"
        } else {
            "tool_result"
        };
        let body = format_tool_result_body(&raw_text, is_error);
        return Some(entry(
            record.ts,
            sequence,
            sub_index,
            EntryKind::ToolResult,
            header,
            body,
            attempt,
            iteration,
        ));
    }
    if let Some(Value::String(text)) = map.get("text") {
        if text.is_empty() {
            return None;
        }
        return Some(entry(
            record.ts,
            sequence,
            sub_index,
            EntryKind::UserText,
            "user",
            short(text, COLLAPSED_BODY_LIMIT),
            attempt,
            iteration,
        ));
    }
    let body = short_value(block, COLLAPSED_BODY_LIMIT);
    if body.is_empty() {
        return None;
    }
    Some(entry(
        record.ts,
        sequence,
        sub_index,
        EntryKind::UserText,
        "user",
        body,
        attempt,
        iteration,
    ))
}

/// Expand one `UserMessage` into per-block entries: tool-result blocks
/// become TOOL_RESULT lines, plain text becomes USER_TEXT. Operator `say`
/// injections are attributed by the matching `control_command_applied`
/// event instead (see [`classify_event`]).
fn classify_user(record: &SdkMessageRecord, sequence: u64) -> Vec<TranscriptEntry> {
    let Some(blocks) = content_blocks(&record.payload) else {
        return fallback_entry(record, sequence, EntryKind::UserText, "user");
    };
    let mut entries = Vec::new();
    let mut sub_index = 0u32;
    for block in blocks {
        if let Some(entry) = classify_user_block(record, sequence, sub_index, block) {
            entries.push(entry);
            sub_index += 1;
        }
    }
    if entries.is_empty() {
        return fallback_entry(record, sequence, EntryKind::UserText, "user");
    }
    entries
}

fn classify_system(record: &SdkMessageRecord, sequence: u64) -> Vec<TranscriptEntry> {
    let subtype = match record.payload.get("subtype") {
        None | Some(Value::Null) => "(system)".to_owned(),
        Some(value) => value_to_text(value),
    };
    vec![entry(
        record.ts,
        sequence,
        0,
        EntryKind::System,
        "system",
        subtype,
        Some(record.attempt_number),
        Some(record.iteration_number),
    )]
}

fn classify_result(record: &SdkMessageRecord, sequence: u64) -> Vec<TranscriptEntry> {
    let field = |key: &str| {
        record
            .payload
            .get(key)
            .map_or_else(|| "?".to_owned(), value_to_text)
    };
    let body = format!(
        "subtype={} turns={} dur={}ms",
        field("subtype"),
        field("num_turns"),
        field("duration_ms")
    );
    vec![entry(
        record.ts,
        sequence,
        0,
        EntryKind::Result,
        "result",
        body,
        Some(record.attempt_number),
        Some(record.iteration_number),
    )]
}

// --- Event classification -------------------------------------------------------

fn command_id_of(payload: &JsonObject) -> Option<i64> {
    payload.get("command_id").and_then(as_int)
}

fn command_kind_of(payload: &JsonObject) -> String {
    match payload.get("kind") {
        None | Some(Value::Null) => "?".to_owned(),
        Some(value) => value_to_text(value),
    }
}

/// Render one [`EventRecord`] as a lifecycle / gate / operator line.
///
/// * `harness.control_command_applied` with `kind == "say"` becomes an
///   OPERATOR_SAY entry; every other applied command becomes a `control`
///   lifecycle line. Both carry `control_command_id`.
/// * `harness.control_command_failed` becomes a `control` lifecycle line
///   with `control_command_error` populated.
/// * The gate kinds become GATE lines.
/// * Everything else is a LIFECYCLE line: a humanized phrase for the known
///   kinds, the `kind + JSON digest` pair otherwise (or when a known kind's
///   payload is malformed).
fn classify_event(record: &EventRecord, sequence: u64) -> Vec<TranscriptEntry> {
    let kind = record.kind.as_str();
    let payload = &record.payload;
    let attempt = record.attempt_number;
    if kind == "harness.control_command_applied" {
        let command_kind = command_kind_of(payload);
        let command_id = command_id_of(payload);
        if command_kind == "say" {
            let text = match payload.get("payload") {
                Some(Value::Object(inner)) => match inner.get("text") {
                    Some(Value::String(s)) => short(s, COLLAPSED_BODY_LIMIT),
                    _ => "(no text)".to_owned(),
                },
                _ => "(no text)".to_owned(),
            };
            let mut e = entry(
                record.ts,
                sequence,
                0,
                EntryKind::OperatorSay,
                "operator(say)",
                text,
                attempt,
                None,
            );
            e.control_command_id = command_id;
            return vec![e];
        }
        let mut e = entry(
            record.ts,
            sequence,
            0,
            EntryKind::Lifecycle,
            "control",
            format!("{command_kind} applied"),
            attempt,
            None,
        );
        e.control_command_id = command_id;
        return vec![e];
    }
    if kind == "harness.control_command_failed" {
        let command_kind = command_kind_of(payload);
        let command_id = command_id_of(payload);
        let error_type = optional_str(payload, "error_type");
        let message = optional_str(payload, "message");
        let error_detail = match (error_type, message) {
            (Some(t), Some(m)) => format!("{t}: {m}"),
            (None, Some(m)) => m,
            (Some(t), None) => t,
            (None, None) => "(no detail)".to_owned(),
        };
        let mut e = entry(
            record.ts,
            sequence,
            0,
            EntryKind::Lifecycle,
            "control",
            format!("{command_kind} failed: {error_detail}"),
            attempt,
            None,
        );
        e.control_command_id = command_id;
        e.control_command_error = Some(error_detail);
        return vec![e];
    }
    if GATE_EVENT_KINDS.contains(&kind) {
        let head = match kind {
            "harness.awaiting_approval" => "gate(awaiting)",
            "harness.manual_approved" => "gate(approved)",
            _ => "gate(rejected)",
        };
        let mut parts = Vec::new();
        if let Some(name) = optional_str(payload, "grader_name") {
            parts.push(format!("grader={name}"));
        }
        if let Some(ordinal) = payload.get("awaiting_ordinal").and_then(as_int) {
            parts.push(format!("ordinal={ordinal}"));
        }
        let body = if parts.is_empty() {
            short_value(&Value::Object(payload.clone()), COLLAPSED_BODY_LIMIT)
        } else {
            parts.join(" ")
        };
        return vec![entry(
            record.ts,
            sequence,
            0,
            EntryKind::Gate,
            head,
            body,
            attempt,
            None,
        )];
    }
    let (header, body) = lifecycle_header_body(kind, payload);
    vec![entry(
        record.ts,
        sequence,
        0,
        EntryKind::Lifecycle,
        header,
        body,
        attempt,
        None,
    )]
}

/// Resolve the `(header, body)` pair for a lifecycle telemetry event.
///
/// Known harness kinds get a friendly header plus a brace-free human
/// phrase built from named payload fields. Anything else -- a future
/// plugin event, an unrecognized kind, or a known kind whose payload is
/// missing / mistyped a required field -- falls back to the full kind as
/// the header and the sorted-key JSON digest of the payload as the body.
pub fn lifecycle_header_body(kind: &str, payload: &JsonObject) -> (String, String) {
    let humanized = match kind {
        "harness.iteration_completed" => fmt_iteration_completed(payload),
        "harness.attempt_started" => fmt_attempt_started(payload),
        "harness.attempt_finalized" => fmt_attempt_finalized(payload),
        "harness.retry_scheduled" => fmt_retry_scheduled(payload),
        "harness.blocked" => Some(fmt_blocked(payload)),
        "harness.unblocked" => Some(fmt_unblocked(payload)),
        "harness.interrupted" => Some(fmt_interrupted(payload)),
        "harness.stuck" => Some(fmt_stuck(payload)),
        "harness.thrash_detected" => Some(fmt_thrash_detected(payload)),
        "harness.hang_detected" => Some(fmt_hang_detected(payload)),
        "harness.budget_exceeded" => Some(fmt_budget_exceeded(payload)),
        "harness.context_threshold_crossed" => fmt_context_threshold_crossed(payload),
        "harness.context_recovery" => Some(fmt_context_recovery(payload)),
        "harness.rubric_invoked" => Some(fmt_rubric_invoked(payload)),
        "harness.rubric_verdict" => fmt_rubric_verdict(payload),
        "harness.crash" => Some(fmt_crash(payload)),
        "harness.audit_write_failed" => Some(fmt_audit_write_failed(payload)),
        "harness.protocol_failure" => Some(fmt_protocol_failure(payload)),
        "harness.recheck_attempted" => Some(fmt_recheck_attempted(payload)),
        _ => None,
    };
    humanized.unwrap_or_else(|| {
        (
            kind.to_owned(),
            short(&payload_digest(payload), COLLAPSED_BODY_LIMIT),
        )
    })
}

/// The separator between phrase fragments on a humanized lifecycle line.
pub const PHRASE_SEPARATOR: &str = " | ";

/// Render a token count compactly: `500`, `1.2k`, `3.4M`.
pub fn format_token_count(value: i64) -> String {
    if value < 1000 {
        value.to_string()
    } else if value < 1_000_000 {
        format!("{:.1}k", value as f64 / 1000.0)
    } else {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    }
}

fn fmt_iteration_completed(payload: &JsonObject) -> Option<(String, String)> {
    let iteration = require_int(payload, "iteration")?;
    let mut parts = vec![format!("iteration {iteration}")];
    if let Some(Value::Object(usage)) = payload.get("usage")
        && let Some(total) = usage.get("total_tokens").and_then(as_int)
        && total > 0
    {
        parts.push(format!("{} tokens", format_token_count(total)));
    }
    if let Some(turns) = payload.get("num_turns").and_then(as_int)
        && turns > 0
    {
        parts.push(format!("{turns} turn{}", if turns != 1 { "s" } else { "" }));
    }
    if let Some(Value::Object(failure)) = payload.get("failure") {
        let ferr = optional_str(failure, "error_type");
        let fmsg = optional_str(failure, "message");
        match (ferr, fmsg) {
            (Some(e), Some(m)) => parts.push(format!("failure: {e}: {m}")),
            (Some(e), None) => parts.push(format!("failure: {e}")),
            (None, Some(m)) => parts.push(format!("failure: {m}")),
            (None, None) => {}
        }
    }
    Some(("iteration".to_owned(), parts.join(PHRASE_SEPARATOR)))
}

fn fmt_attempt_started(payload: &JsonObject) -> Option<(String, String)> {
    let number = require_int(payload, "number")?;
    Some(("attempt".to_owned(), format!("attempt {number} started")))
}

fn fmt_attempt_finalized(payload: &JsonObject) -> Option<(String, String)> {
    let number = require_int(payload, "number")?;
    let mut body = format!("attempt {number} finalized");
    if let Some(outcome) = optional_str(payload, "outcome") {
        body.push_str(&format!(" ({outcome})"));
    }
    if let Some(error) = optional_str(payload, "error") {
        body.push_str(&format!(": {error}"));
    }
    Some(("attempt".to_owned(), body))
}

fn fmt_retry_scheduled(payload: &JsonObject) -> Option<(String, String)> {
    let used = require_int(payload, "retries_used")?;
    let max = require_int(payload, "max_retries")?;
    Some(("retry".to_owned(), format!("retry {used}/{max} scheduled")))
}

fn fmt_blocked(payload: &JsonObject) -> (String, String) {
    let body = match optional_str(payload, "reason") {
        Some(reason) => format!("blocked: {reason}"),
        None => "blocked".to_owned(),
    };
    ("blocked".to_owned(), body)
}

fn fmt_unblocked(payload: &JsonObject) -> (String, String) {
    let body = match (
        optional_str(payload, "from_status"),
        optional_str(payload, "to_status"),
    ) {
        (Some(from), Some(to)) => format!("unblocked: {from} -> {to}"),
        _ => "unblocked".to_owned(),
    };
    ("unblocked".to_owned(), body)
}

fn fmt_interrupted(payload: &JsonObject) -> (String, String) {
    let mut body = "interrupted".to_owned();
    if let Some(c) = optional_str(payload, "classification") {
        body.push_str(&format!(" ({c})"));
    }
    if let Some(m) = optional_str(payload, "message") {
        body.push_str(&format!(": {m}"));
    }
    ("interrupted".to_owned(), body)
}

fn tool_reason_suffix(payload: &JsonObject, mut body: String) -> String {
    if let Some(tool) = optional_str(payload, "tool_name") {
        body.push_str(&format!(" on {tool}"));
    }
    if let Some(reason) = optional_str(payload, "reason") {
        body.push_str(&format!(": {reason}"));
    }
    body
}

fn fmt_stuck(payload: &JsonObject) -> (String, String) {
    (
        "stuck".to_owned(),
        tool_reason_suffix(payload, "stuck".to_owned()),
    )
}

fn fmt_thrash_detected(payload: &JsonObject) -> (String, String) {
    (
        "thrash".to_owned(),
        tool_reason_suffix(payload, "thrash detected".to_owned()),
    )
}

fn fmt_hang_detected(payload: &JsonObject) -> (String, String) {
    let mut parts = vec!["hang detected".to_owned()];
    if let Some(iteration) = payload.get("iteration").and_then(as_int) {
        parts.push(format!("at iteration {iteration}"));
    }
    if let Some(silence) = payload.get("silence_seconds").and_then(as_number) {
        parts.push(format!("silence {silence:.1}s"));
    }
    if let Some(timeout) = payload.get("hang_timeout_seconds").and_then(as_number) {
        parts.push(format!("threshold {timeout:.1}s"));
    }
    ("hang".to_owned(), parts.join(PHRASE_SEPARATOR))
}

fn fmt_budget_exceeded(payload: &JsonObject) -> (String, String) {
    let body = match optional_str(payload, "breached") {
        Some(breached) => format!("budget exceeded: {breached}"),
        None => "budget exceeded".to_owned(),
    };
    ("budget".to_owned(), body)
}

fn fmt_context_threshold_crossed(payload: &JsonObject) -> Option<(String, String)> {
    let pct = match payload.get("percentage").and_then(as_number) {
        Some(p) => format!("{p:.0}%"),
        None => {
            let tier = payload.get("tier").and_then(as_number)?;
            format!("{:.0}%", tier * 100.0)
        }
    };
    let mut body = format!("context threshold {pct} crossed");
    if let Some(source) = optional_str(payload, "capacity_source") {
        body.push_str(&format!(" ({source})"));
    }
    Some(("context".to_owned(), body))
}

fn fmt_context_recovery(payload: &JsonObject) -> (String, String) {
    let mut parts = vec!["context recovery".to_owned()];
    if let Some(trigger) = optional_str(payload, "trigger") {
        parts.push(format!("({trigger})"));
    }
    if let Some(iteration) = payload.get("iteration").and_then(as_int) {
        parts.push(format!("at iteration {iteration}"));
    }
    ("context".to_owned(), parts.join(" "))
}

fn fmt_rubric_invoked(payload: &JsonObject) -> (String, String) {
    let body = match optional_str(payload, "grader_name") {
        Some(name) => format!("rubric invoked: {name}"),
        None => "rubric invoked".to_owned(),
    };
    ("rubric".to_owned(), body)
}

fn fmt_rubric_verdict(payload: &JsonObject) -> Option<(String, String)> {
    let passed = payload.get("passed")?;
    let unknown = payload.get("unknown").is_some_and(truthy);
    let verdict = if unknown {
        "unknown"
    } else {
        match passed {
            Value::Bool(true) => "pass",
            Value::Bool(false) => "fail",
            _ => return None,
        }
    };
    let body = match optional_str(payload, "grader_name") {
        Some(name) => format!("rubric {verdict}: {name}"),
        None => format!("rubric {verdict}"),
    };
    Some(("rubric".to_owned(), body))
}

fn fmt_crash(payload: &JsonObject) -> (String, String) {
    let mut body = "crash".to_owned();
    if let Some(c) = optional_str(payload, "classification") {
        body.push_str(&format!(" ({c})"));
    }
    let bits: Vec<String> = [
        optional_str(payload, "exception_type"),
        optional_str(payload, "message"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !bits.is_empty() {
        body.push_str(": ");
        body.push_str(&bits.join(": "));
    }
    ("crash".to_owned(), body)
}

fn fmt_audit_write_failed(payload: &JsonObject) -> (String, String) {
    let mut body = "audit write failed".to_owned();
    let bits: Vec<String> = [
        optional_str(payload, "error_type"),
        optional_str(payload, "message"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !bits.is_empty() {
        body.push_str(": ");
        body.push_str(&bits.join(": "));
    }
    ("audit".to_owned(), body)
}

fn fmt_protocol_failure(payload: &JsonObject) -> (String, String) {
    let mut body = "protocol failure".to_owned();
    let mut bits = Vec::new();
    if let Some(inner) = optional_str(payload, "kind") {
        bits.push(inner);
    }
    if let Some(reason) = optional_str(payload, "reason") {
        bits.push(reason);
    } else if let Some(detail) = optional_str(payload, "detail") {
        bits.push(detail);
    }
    if !bits.is_empty() {
        body.push_str(": ");
        body.push_str(&bits.join(": "));
    }
    ("protocol".to_owned(), body)
}

fn fmt_recheck_attempted(payload: &JsonObject) -> (String, String) {
    let verdict = match payload.get("all_satisfied") {
        Some(Value::Bool(true)) => "satisfied",
        Some(Value::Bool(false)) => "unsatisfied",
        _ => "complete",
    };
    let mut body = format!("recheck attempted: {verdict}");
    if payload.get("dry_run") == Some(&Value::Bool(true)) {
        body.push_str(" (dry-run)");
    }
    ("recheck".to_owned(), body)
}

/// Convert one [`AuditRecord`] into ordered transcript entries.
///
/// The record's `sequence` is normally assigned by the file reader; a
/// defensive `0` is substituted for a synthetic record without one.
pub fn classify(record: &AuditRecord) -> Vec<TranscriptEntry> {
    let sequence = record.sequence().unwrap_or(0);
    match record {
        AuditRecord::Event(event) => classify_event(event, sequence),
        AuditRecord::SdkMessage(message) => match message.message_type.as_str() {
            "AssistantMessage" => classify_assistant(message, sequence),
            "UserMessage" => classify_user(message, sequence),
            "SystemMessage" => classify_system(message, sequence),
            "ResultMessage" => classify_result(message, sequence),
            other => {
                let header = if other.is_empty() { "sdk" } else { other };
                vec![entry(
                    message.ts,
                    sequence,
                    0,
                    EntryKind::System,
                    header,
                    short(&payload_digest(&message.payload), COLLAPSED_BODY_LIMIT),
                    Some(message.attempt_number),
                    Some(message.iteration_number),
                )]
            }
        },
    }
}

// --- Tailer --------------------------------------------------------------------

/// The default-policy redactor the session uses: the built-in pattern
/// policy composed with an [`EnvValueRedactor`] seeded from
/// [`DEFAULT_ENV_SEEDS`], so the operator's own API key is caught even when
/// an agent has echoed it verbatim. Absent / too-short env values are
/// silently dropped, so this is safe where no secrets are set.
pub fn build_default_redactor() -> ComposedRedactor {
    let env_redactor = EnvValueRedactor::new(&DEFAULT_ENV_SEEDS, None)
        .expect("the constant env seed list is a valid redactor configuration");
    compose(vec![Box::new(default_policy(None)), Box::new(env_redactor)])
}

/// Cursor-incremental tailer over one run's telemetry JSONL file.
///
/// Reads `<logs_root>/runs/<run_id>.jsonl` via
/// [`read_records_since`], so each [`fetch`](Self::fetch) returns only the
/// lines appended since the previous call. The cursor is a byte-offset /
/// line-count pair; a missing file reads as empty (the tailer keeps
/// polling until it appears), and a partial trailing line is withheld
/// until the writer completes it. Records pass through the redactor (when
/// set) before classification. Terminal-status detection stays with the
/// lifecycle row -- the session screen samples the store separately.
pub struct TranscriptTailer {
    run_file: PathBuf,
    run_id: String,
    cursor: FileCursor,
    redactor: Option<Box<dyn Redactor>>,
}

impl TranscriptTailer {
    /// A tailer with the default-policy redactor.
    pub fn new(run_file: impl Into<PathBuf>, run_id: impl Into<String>) -> Self {
        Self::with_redactor(run_file, run_id, Some(Box::new(build_default_redactor())))
    }

    /// A tailer with an explicit redactor; `None` is raw mode.
    pub fn with_redactor(
        run_file: impl Into<PathBuf>,
        run_id: impl Into<String>,
        redactor: Option<Box<dyn Redactor>>,
    ) -> Self {
        Self {
            run_file: run_file.into(),
            run_id: run_id.into(),
            cursor: FileCursor::default(),
            redactor,
        }
    }

    /// The count of complete lines the tailer has consumed.
    pub fn cursor(&self) -> u64 {
        self.cursor.line
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn run_file(&self) -> &Path {
        &self.run_file
    }

    /// Every transcript entry appended since the last fetch.
    ///
    /// An empty vector means "no new activity"; the cursor is unchanged in
    /// that case so a future write is never skipped. An IO failure other
    /// than a missing file surfaces so the screen can show it.
    pub fn fetch(&mut self) -> io::Result<Vec<TranscriptEntry>> {
        let (records, cursor) = read_records_since(&self.run_file, self.cursor, false)?;
        self.cursor = cursor;
        let mut entries = Vec::new();
        for raw in &records {
            let record = match &self.redactor {
                Some(redactor) => redactor.redact(raw),
                None => raw.clone(),
            };
            entries.extend(classify(&record));
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use flywheel_core::store::{TelemetryRecord, TelemetrySink};
    use flywheel_core::telemetry_file::FileTelemetrySink;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap()
    }

    fn obj(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => panic!("expected object"),
        }
    }

    fn assistant(content: Value, sequence: u64) -> AuditRecord {
        AuditRecord::SdkMessage(SdkMessageRecord {
            run_id: "run-x".into(),
            attempt_number: 1,
            iteration_number: 1,
            message_type: "AssistantMessage".into(),
            payload: obj(json!({"content": content})),
            ts: now(),
            sequence: Some(sequence),
            id: None,
        })
    }

    fn user(content: Value) -> AuditRecord {
        AuditRecord::SdkMessage(SdkMessageRecord {
            run_id: "run-x".into(),
            attempt_number: 1,
            iteration_number: 1,
            message_type: "UserMessage".into(),
            payload: obj(json!({"content": content})),
            ts: now(),
            sequence: Some(1),
            id: None,
        })
    }

    fn event(kind: &str, payload: Value, sequence: u64) -> AuditRecord {
        AuditRecord::Event(EventRecord {
            run_id: "run-x".into(),
            ts: now(),
            kind: kind.into(),
            payload: obj(payload),
            attempt_number: Some(1),
            id: None,
            sequence: Some(sequence),
        })
    }

    fn one(record: &AuditRecord) -> TranscriptEntry {
        let entries = classify(record);
        assert_eq!(entries.len(), 1, "expected one entry, got {entries:?}");
        entries.into_iter().next().unwrap()
    }

    #[test]
    fn assistant_text_block_renders_agent_entry() {
        let e = one(&assistant(
            json!([{"type": "text", "text": "I'll edit the README now."}]),
            10,
        ));
        assert_eq!(e.kind, EntryKind::AgentText);
        assert_eq!(e.header, "agent");
        assert_eq!(e.body, "I'll edit the README now.");
        assert_eq!(e.sequence, 10);
        assert_eq!(e.attempt_number, Some(1));
        assert_eq!(e.iteration_number, Some(1));
    }

    #[test]
    fn assistant_multi_paragraph_text_preserves_line_breaks_and_length() {
        let prose = "First paragraph explains the plan.\n\nSecond paragraph dives into the details.\nThird line.";
        let e = one(&assistant(json!([{"type": "text", "text": prose}]), 11));
        assert_eq!(e.body, prose);
        let long = "x".repeat(10_000);
        let e = one(&assistant(json!([{"type": "text", "text": long}]), 12));
        assert_eq!(e.body.len(), 10_000);
        assert!(!e.body.contains("..."));
    }

    #[test]
    fn assistant_text_strips_carriage_returns_and_trailing_whitespace() {
        let raw = "line one  \r\nline two\t\r\n\r\nline four   \r\n";
        let e = one(&assistant(json!([{"type": "text", "text": raw}]), 13));
        assert_eq!(e.body, "line one\nline two\n\nline four");
    }

    #[test]
    fn assistant_whitespace_only_text_block_falls_back_to_empty_marker() {
        let entries = classify(&assistant(
            json!([{"type": "text", "text": "   \r\n\t  \n"}]),
            14,
        ));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "(empty)");
    }

    #[test]
    fn assistant_thinking_block_preserves_prose_under_thinking_header() {
        let thinking = "Step 1: read the spec.\nStep 2: write the test.\nStep 3: ship.";
        let e = one(&assistant(
            json!([{"type": "thinking", "thinking": thinking}]),
            15,
        ));
        assert_eq!(e.kind, EntryKind::AgentText);
        assert_eq!(e.header, "agent(thinking)");
        assert_eq!(e.body, thinking);
    }

    #[test]
    fn tool_use_mapped_tools_show_primary_argument() {
        let e = one(&assistant(
            json!([{"type": "tool_use", "name": "Edit", "input": {"file_path": "README.md", "old": "x"}}]),
            1,
        ));
        assert_eq!(e.kind, EntryKind::ToolCall);
        assert_eq!(e.header, "tool(Edit)");
        assert_eq!(e.body, "README.md");
        let e = one(&assistant(
            json!([{"type": "tool_use", "name": "Bash", "input": {"command": "ls -la /tmp", "description": "list"}}]),
            1,
        ));
        assert_eq!(e.header, "tool(Bash)");
        assert_eq!(e.body, "ls -la /tmp");
        let e = one(&assistant(
            json!([{"type": "tool_use", "name": "Grep", "input": {"pattern": "TODO", "glob": "*.py"}}]),
            1,
        ));
        assert_eq!(e.header, "tool(Grep)");
        assert_eq!(e.body, "TODO");
    }

    #[test]
    fn tool_use_unmapped_or_missing_key_falls_back_to_kv_form() {
        let e = one(&assistant(
            json!([{"type": "tool_use", "name": "MysteryTool", "input": {"alpha": "one", "beta": "two", "gamma": "three"}}]),
            1,
        ));
        assert_eq!(e.header, "tool(MysteryTool)");
        assert_eq!(e.body, "alpha=one, beta=two");
        let e = one(&assistant(
            json!([{"type": "tool_use", "name": "Edit", "input": {"old": "a", "new": "b"}}]),
            1,
        ));
        assert_eq!(e.body, "old=a, new=b");
        let e = one(&assistant(
            json!([{"type": "tool_use", "name": "Edit", "input": "not a mapping"}]),
            1,
        ));
        assert_eq!(e.kind, EntryKind::ToolCall);
        assert_eq!(e.body, "not a mapping");
        let e = one(&assistant(
            json!([{"type": "tool_use", "name": "Edit", "input": {}}]),
            1,
        ));
        assert_eq!(e.body, "");
    }

    #[test]
    fn tool_use_long_command_is_not_truncated() {
        let long_command = format!("echo {}", "x".repeat(1000));
        let e = one(&assistant(
            json!([{"type": "tool_use", "name": "Bash", "input": {"command": long_command}}]),
            1,
        ));
        assert_eq!(e.body, long_command);
    }

    #[test]
    fn typeless_serialized_blocks_classify_by_shape() {
        // ToolUseBlock persisted without a `type` discriminator.
        let e = one(&assistant(
            json!([{"id": "toolu_01JUhk", "name": "Bash", "input": {"command": "uv run pytest 2>&1 | tail -5"}}]),
            40,
        ));
        assert_eq!(e.kind, EntryKind::ToolCall);
        assert_eq!(e.header, "tool(Bash)");
        assert_eq!(e.body, "uv run pytest 2>&1 | tail -5");
        // ThinkingBlock persisted as {thinking, signature}.
        let prose = "Plan: fix the classifier first.\nThen extend the tests.";
        let e = one(&assistant(
            json!([{"thinking": prose, "signature": "CAISxwMKYggOGAIqQ"}]),
            41,
        ));
        assert_eq!(e.header, "agent(thinking)");
        assert_eq!(e.body, prose);
        assert!(!e.body.contains("CAISxw"));
        // Signature-only keep-alives produce nothing at all.
        let entries = classify(&assistant(
            json!([{"thinking": "", "signature": "a"}, {"thinking": "", "signature": "b"}]),
            42,
        ));
        assert!(entries.is_empty());
        // Mixed: the empty thinking block is dropped, the tool call kept.
        let entries = classify(&assistant(
            json!([{"thinking": "", "signature": "a"}, {"id": "t", "name": "Bash", "input": {"command": "uv run pytest", "timeout": 600000}}]),
            44,
        ));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].header, "tool(Bash)");
        assert_eq!(entries[0].body, "uv run pytest");
        assert_eq!(entries[0].sub_index, 0);
        // Unknown typeless block keeps a stringified fallback.
        let e = one(&assistant(json!([{"mystery": "value"}]), 45));
        assert_eq!(e.kind, EntryKind::AgentText);
        assert!(e.body.contains("mystery"));
    }

    #[test]
    fn assistant_fans_out_text_and_tool_use_sharing_sequence() {
        let entries = classify(&assistant(
            json!([{"type": "text", "text": "Calling Edit."}, {"type": "tool_use", "name": "Edit", "input": {"file": "a"}}]),
            7,
        ));
        assert_eq!(
            entries.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![EntryKind::AgentText, EntryKind::ToolCall]
        );
        assert_eq!(
            entries.iter().map(|e| e.sub_index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(entries.iter().all(|e| e.sequence == 7));
    }

    #[test]
    fn assistant_odd_or_empty_content_falls_back() {
        let entries = classify(&assistant(json!({"not": "a list"}), 1));
        assert_eq!(entries[0].body, "(empty)");
        let entries = classify(&assistant(Value::Null, 1));
        assert_eq!(entries[0].body, "(empty)");
        let many: Vec<Value> = (0..40)
            .map(|i| json!({"type": "text", "text": format!("b{i}")}))
            .collect();
        assert_eq!(
            classify(&assistant(Value::Array(many), 1)).len(),
            MAX_BLOCKS_PER_MESSAGE
        );
    }

    #[test]
    fn tool_result_success_shapes() {
        let e = one(&user(
            json!([{"tool_use_id": "t1", "content": "stdout line 1\nstdout line 2", "is_error": false}]),
        ));
        assert_eq!(e.kind, EntryKind::ToolResult);
        assert_eq!(e.header, "tool_result");
        assert_eq!(e.body, "ok (2 lines)");
        let e = one(&user(
            json!([{"tool_use_id": "t1", "content": "file written", "is_error": false}]),
        ));
        assert_eq!(e.body, "ok: file written");
        let e = one(&user(
            json!([{"tool_use_id": "t1", "content": "", "is_error": false}]),
        ));
        assert_eq!(e.body, "ok");
        let e = one(&user(json!([{"tool_use_id": "t1"}])));
        assert_eq!(e.body, "ok");
        let e = one(&user(
            json!([{"tool_use_id": "t1", "content": [{"type": "text", "text": "x"}]}]),
        ));
        assert!(e.body.starts_with("ok: "));
    }

    #[test]
    fn tool_result_error_shapes() {
        let e = one(&user(
            json!([{"tool_use_id": "t1", "content": "boom", "is_error": true}]),
        ));
        assert_eq!(e.header, "tool_result(error)");
        assert_eq!(e.body, "boom");
        let body = (0..30)
            .map(|i| format!("err line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let e = one(&user(
            json!([{"tool_use_id": "t1", "content": body, "is_error": true}]),
        ));
        let lines: Vec<&str> = e.body.split('\n').collect();
        assert_eq!(lines.len(), 11);
        assert_eq!(
            lines[..10],
            (0..10).map(|i| format!("err line {i}")).collect::<Vec<_>>()
        );
        assert_eq!(lines[10], "... +20 more lines");
        let e = one(&user(
            json!([{"tool_use_id": "t1", "content": "first\nsecond\nthird", "is_error": true}]),
        ));
        assert_eq!(e.body, "first\nsecond\nthird");
        let e = one(&user(
            json!([{"tool_use_id": "t1", "content": "", "is_error": true}]),
        ));
        assert_eq!(e.body, "");
    }

    #[test]
    fn user_text_block_and_fallbacks() {
        let e = one(&user(json!([{"type": "text", "text": "thanks"}])));
        assert_eq!(e.kind, EntryKind::UserText);
        assert_eq!(e.header, "user");
        assert_eq!(e.body, "thanks");
        let entries = classify(&user(json!([{"type": "text", "text": ""}])));
        assert_eq!(entries[0].body, "(empty)");
        let e = one(&user(json!(["bare string"])));
        assert_eq!(e.body, "bare string");
    }

    #[test]
    fn operator_say_event_attributes_text() {
        let e = one(&event(
            "harness.control_command_applied",
            json!({"command_id": 42, "kind": "say", "payload": {"text": "please add docstrings"}}),
            20,
        ));
        assert_eq!(e.kind, EntryKind::OperatorSay);
        assert_eq!(e.header, "operator(say)");
        assert_eq!(e.body, "please add docstrings");
        assert_eq!(e.control_command_id, Some(42));
        assert_eq!(e.iteration_number, None);
        let e = one(&event(
            "harness.control_command_applied",
            json!({"kind": "say"}),
            21,
        ));
        assert_eq!(e.body, "(no text)");
        assert_eq!(e.control_command_id, None);
    }

    #[test]
    fn non_say_control_applied_renders_control_line() {
        let e = one(&event(
            "harness.control_command_applied",
            json!({"command_id": 1, "kind": "interrupt", "payload": {}}),
            1,
        ));
        assert_eq!(e.kind, EntryKind::Lifecycle);
        assert_eq!(e.header, "control");
        assert_eq!(e.body, "interrupt applied");
        assert_eq!(e.control_command_id, Some(1));
        assert!(e.control_command_error.is_none());
    }

    #[test]
    fn control_failed_surfaces_error_detail() {
        let e = one(&event(
            "harness.control_command_failed",
            json!({"command_id": 7, "kind": "say", "error_type": "SDKDisconnected", "message": "session was closed"}),
            1,
        ));
        assert_eq!(e.header, "control");
        assert_eq!(e.body, "say failed: SDKDisconnected: session was closed");
        assert_eq!(e.control_command_id, Some(7));
        assert_eq!(
            e.control_command_error.as_deref(),
            Some("SDKDisconnected: session was closed")
        );
        let e = one(&event(
            "harness.control_command_failed",
            json!({"kind": "say"}),
            1,
        ));
        assert_eq!(e.body, "say failed: (no detail)");
        let e = one(&event(
            "harness.control_command_failed",
            json!({"message": "m"}),
            1,
        ));
        assert_eq!(e.body, "? failed: m");
    }

    #[test]
    fn gate_events_carry_grader_name_and_ordinal() {
        let e = one(&event(
            "harness.awaiting_approval",
            json!({"awaiting_ordinal": 2, "grader_name": "review-migration"}),
            1,
        ));
        assert_eq!(e.kind, EntryKind::Gate);
        assert_eq!(e.header, "gate(awaiting)");
        assert_eq!(e.body, "grader=review-migration ordinal=2");
        let e = one(&event(
            "harness.manual_approved",
            json!({"grader_name": "g"}),
            1,
        ));
        assert_eq!(e.header, "gate(approved)");
        assert_eq!(e.body, "grader=g");
        let e = one(&event(
            "harness.manual_rejected",
            json!({"feedback": "no"}),
            1,
        ));
        assert_eq!(e.header, "gate(rejected)");
        assert!(e.body.contains("feedback"));
    }

    #[test]
    fn iteration_completed_renders_humanized_phrase() {
        let e = one(&event(
            "harness.iteration_completed",
            json!({"iteration": 3, "usage": {"total_tokens": 1200}}),
            1,
        ));
        assert_eq!(e.kind, EntryKind::Lifecycle);
        assert_eq!(e.header, "iteration");
        assert_eq!(e.body, "iteration 3 | 1.2k tokens");
        let e = one(&event(
            "harness.iteration_completed",
            json!({"iteration": 5, "usage": {"total_tokens": 500}, "num_turns": 4,
                   "failure": {"error_type": "EnvelopeError", "message": "missing envelope"}}),
            1,
        ));
        assert_eq!(
            e.body,
            "iteration 5 | 500 tokens | 4 turns | failure: EnvelopeError: missing envelope"
        );
        let e = one(&event(
            "harness.iteration_completed",
            json!({"iteration": 1, "num_turns": 1}),
            1,
        ));
        assert_eq!(e.body, "iteration 1 | 1 turn");
    }

    #[test]
    fn attempt_and_retry_phrases() {
        let e = one(&event(
            "harness.attempt_started",
            json!({"number": 2, "agent_context": {"model": "t"}}),
            1,
        ));
        assert_eq!(e.header, "attempt");
        assert_eq!(e.body, "attempt 2 started");
        let e = one(&event(
            "harness.attempt_finalized",
            json!({"number": 1, "outcome": "agent_error", "error": "protocol failure: missing envelope"}),
            1,
        ));
        assert_eq!(
            e.body,
            "attempt 1 finalized (agent_error): protocol failure: missing envelope"
        );
        let e = one(&event(
            "harness.retry_scheduled",
            json!({"retries_used": 1, "max_retries": 3}),
            1,
        ));
        assert_eq!(e.header, "retry");
        assert_eq!(e.body, "retry 1/3 scheduled");
    }

    #[test]
    fn remaining_lifecycle_phrases() {
        let cases: Vec<(&str, Value, &str, &str)> = vec![
            (
                "harness.blocked",
                json!({"reason": "needs creds"}),
                "blocked",
                "blocked: needs creds",
            ),
            ("harness.blocked", json!({}), "blocked", "blocked"),
            (
                "harness.unblocked",
                json!({"from_status": "interrupted", "to_status": "ready"}),
                "unblocked",
                "unblocked: interrupted -> ready",
            ),
            (
                "harness.interrupted",
                json!({"classification": "worker_interrupted", "message": "operator"}),
                "interrupted",
                "interrupted (worker_interrupted): operator",
            ),
            (
                "harness.stuck",
                json!({"tool_name": "Bash", "reason": "same call"}),
                "stuck",
                "stuck on Bash: same call",
            ),
            (
                "harness.thrash_detected",
                json!({"tool_name": "Edit"}),
                "thrash",
                "thrash detected on Edit",
            ),
            (
                "harness.hang_detected",
                json!({"iteration": 4, "silence_seconds": 12.34, "hang_timeout_seconds": 10}),
                "hang",
                "hang detected | at iteration 4 | silence 12.3s | threshold 10.0s",
            ),
            (
                "harness.budget_exceeded",
                json!({"breached": "turns>10"}),
                "budget",
                "budget exceeded: turns>10",
            ),
            (
                "harness.context_threshold_crossed",
                json!({"percentage": 75, "capacity_source": "sdk"}),
                "context",
                "context threshold 75% crossed (sdk)",
            ),
            (
                "harness.context_threshold_crossed",
                json!({"tier": 0.5}),
                "context",
                "context threshold 50% crossed",
            ),
            (
                "harness.context_recovery",
                json!({"trigger": "mid_turn", "iteration": 7}),
                "context",
                "context recovery (mid_turn) at iteration 7",
            ),
            (
                "harness.rubric_invoked",
                json!({"grader_name": "review"}),
                "rubric",
                "rubric invoked: review",
            ),
            (
                "harness.rubric_verdict",
                json!({"passed": true, "grader_name": "review"}),
                "rubric",
                "rubric pass: review",
            ),
            (
                "harness.rubric_verdict",
                json!({"passed": false}),
                "rubric",
                "rubric fail",
            ),
            (
                "harness.rubric_verdict",
                json!({"passed": null, "unknown": true}),
                "rubric",
                "rubric unknown",
            ),
            (
                "harness.crash",
                json!({"classification": "entry_error", "exception_type": "RuntimeError", "message": "stack overflow"}),
                "crash",
                "crash (entry_error): RuntimeError: stack overflow",
            ),
            (
                "harness.audit_write_failed",
                json!({"error_type": "SqliteError", "message": "db locked"}),
                "audit",
                "audit write failed: SqliteError: db locked",
            ),
            (
                "harness.protocol_failure",
                json!({"kind": "malformed", "detail": "bad json"}),
                "protocol",
                "protocol failure: malformed: bad json",
            ),
            (
                "harness.recheck_attempted",
                json!({"all_satisfied": false, "dry_run": true}),
                "recheck",
                "recheck attempted: unsatisfied (dry-run)",
            ),
            (
                "harness.recheck_attempted",
                json!({}),
                "recheck",
                "recheck attempted: complete",
            ),
        ];
        for (kind, payload, header, body) in cases {
            let e = one(&event(kind, payload, 1));
            assert_eq!(e.header, header, "{kind}");
            assert_eq!(e.body, body, "{kind}");
        }
    }

    #[test]
    fn malformed_known_kind_and_unknown_kind_fall_back_to_digest() {
        let e = one(&event(
            "harness.iteration_completed",
            json!({"usage": {"total_tokens": 100}}),
            1,
        ));
        assert_eq!(e.header, "harness.iteration_completed");
        assert_eq!(e.body, "{\"usage\":{\"total_tokens\":100}}");
        let e = one(&event(
            "harness.iteration_completed",
            json!({"iteration": 2.5}),
            1,
        ));
        assert_eq!(e.header, "harness.iteration_completed");
        let e = one(&event(
            "harness.rubric_verdict",
            json!({"passed": "yes"}),
            1,
        ));
        assert_eq!(e.header, "harness.rubric_verdict");
        let e = one(&event(
            "harness.context_threshold_crossed",
            json!({"capacity_source": "sdk"}),
            1,
        ));
        assert_eq!(e.header, "harness.context_threshold_crossed");
        let e = one(&event(
            "harness.audit_finalized",
            json!({"reason": "shutdown", "attempts": 4}),
            1,
        ));
        assert_eq!(e.kind, EntryKind::Lifecycle);
        assert_eq!(e.header, "harness.audit_finalized");
        assert_eq!(e.body, "{\"attempts\":4,\"reason\":\"shutdown\"}");
        let e = one(&event("harness.audit_finalized", json!({}), 1));
        assert_eq!(e.body, "");
    }

    #[test]
    fn system_result_and_unknown_sdk_messages() {
        let mk = |message_type: &str, payload: Value| {
            AuditRecord::SdkMessage(SdkMessageRecord {
                run_id: "run-x".into(),
                attempt_number: 1,
                iteration_number: 1,
                message_type: message_type.into(),
                payload: obj(payload),
                ts: now(),
                sequence: Some(99),
                id: None,
            })
        };
        let e = one(&mk("SystemMessage", json!({"subtype": "init"})));
        assert_eq!(e.kind, EntryKind::System);
        assert_eq!(e.header, "system");
        assert_eq!(e.body, "init");
        let e = one(&mk("SystemMessage", json!({})));
        assert_eq!(e.body, "(system)");
        let e = one(&mk(
            "ResultMessage",
            json!({"subtype": "end_turn", "num_turns": 4, "duration_ms": 1234}),
        ));
        assert_eq!(e.kind, EntryKind::Result);
        assert_eq!(e.body, "subtype=end_turn turns=4 dur=1234ms");
        let e = one(&mk("ResultMessage", json!({})));
        assert_eq!(e.body, "subtype=? turns=? dur=?ms");
        let e = one(&mk("StreamEvent", json!({"b": 1, "a": 2})));
        assert_eq!(e.kind, EntryKind::System);
        assert_eq!(e.header, "StreamEvent");
        assert_eq!(e.body, "{\"a\":2,\"b\":1}");
        let e = one(&mk("", json!({})));
        assert_eq!(e.header, "sdk");
    }

    #[test]
    fn short_caps_width_with_ascii_ellipsis() {
        assert_eq!(short("abc\ndef", 10), "abc def");
        let capped = short(&"y".repeat(300), COLLAPSED_BODY_LIMIT);
        assert_eq!(capped.chars().count(), COLLAPSED_BODY_LIMIT);
        assert!(capped.ends_with("..."));
        assert_eq!(short("abcdef", 4), "a...");
        assert_eq!(format_token_count(500), "500");
        assert_eq!(format_token_count(1200), "1.2k");
        assert_eq!(format_token_count(3_400_000), "3.4M");
    }

    #[test]
    fn is_terminal_matches_lifecycle_leaf_states() {
        for status in [
            Status::Done,
            Status::Failed,
            Status::FailedValidation,
            Status::InternalError,
            Status::Interrupted,
        ] {
            assert!(is_terminal(status));
        }
        assert!(!is_terminal(Status::Running));
        assert!(!is_terminal(Status::AwaitingApproval));
        assert!(TERMINAL_STATUSES.contains(&Status::Done));
    }

    fn append_assistant_line(sink: &FileTelemetrySink, run_id: &str, payload: Value) {
        let mut record = TelemetryRecord::new(run_id, now(), "AssistantMessage");
        record.payload = obj(payload);
        record.attempt_number = Some(1);
        record.iteration_number = Some(1);
        sink.append_telemetry(&record).unwrap();
    }

    #[test]
    fn tailer_returns_only_new_records_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileTelemetrySink::new(dir.path().join("logs"));
        append_assistant_line(
            &sink,
            "run-alpha",
            json!({"content": [{"type": "text", "text": "first"}]}),
        );
        let mut tailer =
            TranscriptTailer::with_redactor(sink.path_for("run-alpha").unwrap(), "run-alpha", None);
        let first = tailer.fetch().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].body, "first");
        let after_first = tailer.cursor();
        assert!(after_first > 0);
        assert!(tailer.fetch().unwrap().is_empty());
        assert_eq!(tailer.cursor(), after_first);
        append_assistant_line(
            &sink,
            "run-alpha",
            json!({"content": [{"type": "text", "text": "second"}]}),
        );
        let second = tailer.fetch().unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].body, "second");
        assert!(tailer.cursor() > after_first);
        assert_eq!(tailer.run_id(), "run-alpha");
    }

    #[test]
    fn tailer_default_redactor_suppresses_anthropic_keys() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileTelemetrySink::new(dir.path().join("logs"));
        let leaked = format!("sk-ant-api03-{}", "A".repeat(95));
        append_assistant_line(
            &sink,
            "run-beta",
            json!({"content": [{"type": "text", "text": format!("my key is {leaked}")}]}),
        );
        let mut tailer = TranscriptTailer::new(sink.path_for("run-beta").unwrap(), "run-beta");
        let entries = tailer.fetch().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].body.contains(&leaked));
        assert!(entries[0].body.contains("[REDACTED"));
    }

    #[test]
    fn tailer_custom_redactor_is_honoured() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileTelemetrySink::new(dir.path().join("logs"));
        append_assistant_line(
            &sink,
            "run-gamma",
            json!({"content": [{"type": "text", "text": "hello world"}]}),
        );
        let mut tailer = TranscriptTailer::with_redactor(
            sink.path_for("run-gamma").unwrap(),
            "run-gamma",
            Some(Box::new(default_policy(None))),
        );
        assert_eq!(tailer.fetch().unwrap()[0].body, "hello world");
    }

    #[test]
    fn tailer_missing_file_reads_as_empty_and_survives_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let run_file = dir.path().join("logs/runs/run-gone.jsonl");
        let mut tailer = TranscriptTailer::with_redactor(&run_file, "run-gone", None);
        assert!(tailer.fetch().unwrap().is_empty());
        assert_eq!(tailer.cursor(), 0);
        let sink = FileTelemetrySink::new(dir.path().join("logs"));
        append_assistant_line(
            &sink,
            "run-gone",
            json!({"content": [{"type": "text", "text": "hello"}]}),
        );
        sink.close();
        assert_eq!(
            tailer
                .fetch()
                .unwrap()
                .iter()
                .map(|e| e.body.as_str())
                .collect::<Vec<_>>(),
            vec!["hello"]
        );
        std::fs::remove_file(&run_file).unwrap();
        assert!(tailer.fetch().unwrap().is_empty());
        assert_eq!(tailer.run_file(), run_file.as_path());
    }

    #[test]
    fn build_default_redactor_round_trips_plain_text() {
        let redactor = build_default_redactor();
        let record = assistant(json!([{"type": "text", "text": "hello"}]), 1);
        let redacted = redactor.redact(&record);
        match redacted {
            AuditRecord::SdkMessage(m) => assert_eq!(m.payload["content"][0]["text"], "hello"),
            AuditRecord::Event(_) => panic!("kind changed"),
        }
    }
}
