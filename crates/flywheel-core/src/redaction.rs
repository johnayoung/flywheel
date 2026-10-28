//! Pure redaction layer over the audit stream.
//!
//! Wiring-agnostic transforms over a single [`AuditRecord`]. The same
//! [`Redactor`] object is reusable at the read seam (`crate::audit::stream`)
//! and at a persistence seam without modification: the trait takes one
//! record in and returns one record out, with no reference to streams,
//! cursors, or the store.
//!
//! Redaction is best-effort secret suppression, not a guarantee. Unmatched
//! secrets can still pass through; the store remains the unredacted source
//! of truth for forensics. Callers wanting verbatim data opt out.
//!
//! Purity discipline (mirrors `crate::task` / `crate::lifecycle`): this
//! module performs no file, stream, or network IO. The one accepted impurity
//! is [`EnvValueRedactor::new`] / [`EnvValueRedactor::with_options`], which
//! read the process environment at construction time and capture the
//! resulting values in memory; once constructed the redactor is a pure
//! transform like every other built-in.
//!
//! Output parity: for the same record, salt, and configuration every
//! redactor here produces byte-identical output to the Python
//! `flywheel_core.redaction` module (token format, HMAC-SHA256 digest
//! prefix, pattern order, overlap handling).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use regex::{NoExpand, Regex};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::store::{AuditRecord, EventRecord, JsonObject, SdkMessageRecord};

// --- Errors -------------------------------------------------------------------

/// A redactor could not be constructed from its configuration.
///
/// Every built-in validates its configuration eagerly (a malformed regex, an
/// empty label, a zero minimum length) so a bad policy fails fast at
/// construction, never midway through a stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RedactionError {
    /// A [`PatternRedactor`] regex failed to compile.
    #[error("invalid regex for label {label:?}: {message}")]
    InvalidPattern { label: String, message: String },
    /// Any other configuration defect (empty label, path, or tool name; a
    /// zero minimum length). The message names the offending redactor.
    #[error("{0}")]
    InvalidConfiguration(String),
}

fn configuration(message: impl Into<String>) -> RedactionError {
    RedactionError::InvalidConfiguration(message.into())
}

// --- Stable-token helper ------------------------------------------------------

/// Hex characters of the HMAC-SHA256 digest kept in a salted token.
pub const DIGEST_HEX_LEN: usize = 8;

const SHA256_BLOCK_LEN: usize = 64;

/// HMAC-SHA256 (RFC 2104) over `message` keyed by `key`, byte-identical to
/// Python's `hmac.new(key, message, hashlib.sha256).digest()`.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; SHA256_BLOCK_LEN];
    if key.len() > SHA256_BLOCK_LEN {
        let hashed = Sha256::digest(key);
        block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; SHA256_BLOCK_LEN];
    let mut outer_pad = [0x5cu8; SHA256_BLOCK_LEN];
    for i in 0..SHA256_BLOCK_LEN {
        inner_pad[i] ^= block[i];
        outer_pad[i] ^= block[i];
    }
    let inner = Sha256::new()
        .chain_update(inner_pad)
        .chain_update(message)
        .finalize();
    let outer = Sha256::new()
        .chain_update(outer_pad)
        .chain_update(inner)
        .finalize();
    outer.into()
}

/// Render the placeholder token for one redacted occurrence.
///
/// Without a salt (or when the redactor has no string cleartext to hash)
/// the token is the bare `[REDACTED:<label>]`. With a salt the token is
/// `[REDACTED:<label>:<digest>]` where `<digest>` is the first
/// [`DIGEST_HEX_LEN`] hex chars of HMAC-SHA256 over `cleartext` keyed by
/// `salt`. The digest is one-way; the cleartext is never recoverable from a
/// token.
///
/// Same `(label, cleartext, salt)` always yields the same token across
/// records and processes so consumers can correlate occurrences of the same
/// secret without ever seeing it.
pub fn format_token(label: &str, cleartext: Option<&str>, salt: Option<&[u8]>) -> String {
    match (salt, cleartext) {
        (Some(salt), Some(cleartext)) => {
            let digest = hex::encode(hmac_sha256(salt, cleartext.as_bytes()));
            format!("[REDACTED:{label}:{}]", &digest[..DIGEST_HEX_LEN])
        }
        _ => format!("[REDACTED:{label}]"),
    }
}

// --- Redactor trait -----------------------------------------------------------

/// A pure transform over a single [`AuditRecord`].
///
/// Implementations must:
///
/// * Return a record of the same variant as the input (`Event` stays
///   `Event`; `SdkMessage` stays `SdkMessage`).
/// * Leave `sequence`, `id`, `ts`, `run_id`, and every non-payload field
///   unchanged.
/// * Never mutate the input record; produce a fresh record instead.
/// * Perform no IO.
///
/// The trait intentionally references only [`AuditRecord`], not the store,
/// the stream, or any cursor, so the same object can be applied at read time
/// or at a persistence seam without change. `Send + Sync` so one policy can
/// be shared by a live subscription thread and the calling thread.
pub trait Redactor: Send + Sync {
    fn redact(&self, record: &AuditRecord) -> AuditRecord;
}

impl Redactor for Box<dyn Redactor> {
    fn redact(&self, record: &AuditRecord) -> AuditRecord {
        (**self).redact(record)
    }
}

impl Redactor for Arc<dyn Redactor> {
    fn redact(&self, record: &AuditRecord) -> AuditRecord {
        (**self).redact(record)
    }
}

// --- Deep traversal helpers ---------------------------------------------------

/// Return a deep copy of `value` with every string leaf passed through
/// `replace`. Objects and arrays become fresh containers; every non-string
/// leaf passes through untouched.
fn walk(value: &Value, replace: &dyn Fn(&str) -> Cow<'_, str>) -> Value {
    match value {
        Value::String(text) => Value::String(replace(text).into_owned()),
        Value::Object(map) => Value::Object(walk_object(map, replace)),
        Value::Array(items) => Value::Array(items.iter().map(|item| walk(item, replace)).collect()),
        other => other.clone(),
    }
}

fn walk_object(map: &JsonObject, replace: &dyn Fn(&str) -> Cow<'_, str>) -> JsonObject {
    map.iter()
        .map(|(key, item)| (key.clone(), walk(item, replace)))
        .collect()
}

/// Return a new record of `record`'s variant with the payload rewritten by
/// `replace` and every other field copied verbatim.
fn rebuild(record: &AuditRecord, replace: &dyn Fn(&str) -> Cow<'_, str>) -> AuditRecord {
    with_payload(record, |payload| walk_object(payload, replace))
}

/// Return a new record of `record`'s variant whose payload is
/// `transform(original payload)`; every other field is copied verbatim.
fn with_payload(
    record: &AuditRecord,
    transform: impl FnOnce(&JsonObject) -> JsonObject,
) -> AuditRecord {
    match record {
        AuditRecord::Event(event) => AuditRecord::Event(EventRecord {
            payload: transform(&event.payload),
            ..event.clone()
        }),
        AuditRecord::SdkMessage(message) => AuditRecord::SdkMessage(SdkMessageRecord {
            payload: transform(&message.payload),
            ..message.clone()
        }),
    }
}

// --- PatternRedactor ----------------------------------------------------------

/// Replaces substrings matching configured regexes inside any string leaf
/// of a record's payload.
///
/// Construction takes `(label, pattern)` pairs; each pattern is compiled
/// once and reused across every record. Matched substrings are replaced by
/// `[REDACTED:<label>]` (or `[REDACTED:<label>:<digest>]` when `salt` is
/// configured) in place inside the surrounding string, so a secret embedded
/// in a larger log line leaves the rest of the line intact.
///
/// The order of patterns is preserved: earlier rules apply first, so a
/// later pattern sees the output of the earlier one. This is the same
/// contract [`compose`] provides between redactors and lets callers layer
/// specific labels on top of generic catch-alls.
#[derive(Debug, Clone)]
pub struct PatternRedactor {
    patterns: Vec<(String, Regex)>,
    salt: Option<Vec<u8>>,
}

impl PatternRedactor {
    pub fn new(patterns: &[(&str, &str)], salt: Option<&[u8]>) -> Result<Self, RedactionError> {
        let mut compiled = Vec::with_capacity(patterns.len());
        for (label, pattern) in patterns {
            if label.is_empty() {
                return Err(configuration(
                    "PatternRedactor pattern label must be a non-empty string",
                ));
            }
            let regex = Regex::new(pattern).map_err(|err| RedactionError::InvalidPattern {
                label: (*label).to_owned(),
                message: err.to_string(),
            })?;
            compiled.push(((*label).to_owned(), regex));
        }
        Ok(Self {
            patterns: compiled,
            salt: salt.map(<[u8]>::to_vec),
        })
    }

    /// The configured `(label, pattern)` pairs in application order.
    pub fn patterns(&self) -> impl Iterator<Item = (&str, &str)> {
        self.patterns
            .iter()
            .map(|(label, regex)| (label.as_str(), regex.as_str()))
    }

    fn replace<'t>(&self, text: &'t str) -> Cow<'t, str> {
        let mut out = Cow::Borrowed(text);
        for (label, regex) in &self.patterns {
            let replaced = match &self.salt {
                None => {
                    let token = format_token(label, None, None);
                    regex.replace_all(&out, NoExpand(&token))
                }
                Some(salt) => regex.replace_all(&out, |caps: &regex::Captures<'_>| {
                    format_token(label, Some(&caps[0]), Some(salt))
                }),
            };
            if let Cow::Owned(replaced) = replaced {
                out = Cow::Owned(replaced);
            }
        }
        out
    }
}

impl Redactor for PatternRedactor {
    fn redact(&self, record: &AuditRecord) -> AuditRecord {
        rebuild(record, &|text| self.replace(text))
    }
}

// --- EnvValueRedactor ---------------------------------------------------------

/// Construction options for [`EnvValueRedactor`].
#[derive(Debug, Clone, Default)]
pub struct EnvValueOptions {
    /// Salt for stable `[REDACTED:<label>:<digest>]` tokens; `None` for bare
    /// tokens.
    pub salt: Option<Vec<u8>>,
    /// Values shorter than this many characters are never captured. `None`
    /// uses [`EnvValueRedactor::DEFAULT_MINIMUM_LENGTH`].
    pub minimum_length: Option<usize>,
    /// Per-variable token labels; a variable absent from the map is
    /// labelled with its own name.
    pub labels: HashMap<String, String>,
}

/// Redacts the literal values of named environment variables wherever they
/// appear in a payload string.
///
/// The redactor reads the environment exactly once, at construction time,
/// and captures each named variable's value in memory. From that point on
/// it is a pure transform: changing the process environment later does not
/// change what it redacts.
///
/// Guards against degenerate cases that would otherwise blank everything
/// out:
///
/// * A variable that is absent (or not valid Unicode) contributes no
///   pattern.
/// * A variable whose value is the empty string contributes no pattern.
/// * A variable whose value is shorter than the minimum length is excluded;
///   this stops common short substrings (e.g. a one-character flag) from
///   being treated as a secret.
///
/// Matching is literal substring matching, not regex, so values that look
/// like regex metacharacters are handled correctly. With a salt configured
/// the placeholder includes a stable HMAC digest of the captured value so
/// identical secrets correlate across records.
#[derive(Debug, Clone)]
pub struct EnvValueRedactor {
    /// `(label, value)` pairs sorted by value length descending.
    label_values: Vec<(String, String)>,
    salt: Option<Vec<u8>>,
}

impl EnvValueRedactor {
    pub const DEFAULT_MINIMUM_LENGTH: usize = 8;

    /// Capture `var_names` from the process environment with default
    /// options (no labels override, the default minimum length).
    pub fn new(var_names: &[&str], salt: Option<&[u8]>) -> Result<Self, RedactionError> {
        Self::with_options(
            var_names,
            &EnvValueOptions {
                salt: salt.map(<[u8]>::to_vec),
                ..EnvValueOptions::default()
            },
        )
    }

    /// Capture `var_names` from the process environment.
    pub fn with_options(
        var_names: &[&str],
        options: &EnvValueOptions,
    ) -> Result<Self, RedactionError> {
        Self::with_lookup(var_names, options, |name| std::env::var(name).ok())
    }

    /// Capture `var_names` through `lookup` instead of the process
    /// environment. The transform is identical; only the source of the
    /// captured values differs, which keeps the redactor testable without
    /// mutating process-global state.
    pub fn with_lookup(
        var_names: &[&str],
        options: &EnvValueOptions,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, RedactionError> {
        let minimum_length = options
            .minimum_length
            .unwrap_or(Self::DEFAULT_MINIMUM_LENGTH);
        if minimum_length < 1 {
            return Err(configuration(
                "EnvValueRedactor minimum_length must be >= 1",
            ));
        }
        let mut captured: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for name in var_names {
            if name.is_empty() {
                return Err(configuration(
                    "EnvValueRedactor var_names must be non-empty strings",
                ));
            }
            if !seen.insert(name) {
                continue;
            }
            let Some(value) = lookup(name) else {
                continue;
            };
            if value.is_empty() || value.chars().count() < minimum_length {
                continue;
            }
            let label = options
                .labels
                .get(*name)
                .map(String::as_str)
                .unwrap_or(name);
            if label.is_empty() {
                return Err(configuration(
                    "EnvValueRedactor labels values must be non-empty strings",
                ));
            }
            captured.push((label.to_owned(), value));
        }
        // Sort by value length descending so a long captured value is
        // redacted before any shorter value it might contain. This avoids
        // leaking the long value's prefix when two captured values overlap.
        // The sort is stable, so equal lengths keep declaration order.
        captured.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
        Ok(Self {
            label_values: captured,
            salt: options.salt.clone(),
        })
    }

    /// The captured `(label, value)` pairs in application order.
    pub fn captured(&self) -> impl Iterator<Item = (&str, &str)> {
        self.label_values
            .iter()
            .map(|(label, value)| (label.as_str(), value.as_str()))
    }

    fn replace<'t>(&self, text: &'t str) -> Cow<'t, str> {
        let mut out = Cow::Borrowed(text);
        for (label, value) in &self.label_values {
            if out.contains(value.as_str()) {
                let token = format_token(label, Some(value), self.salt.as_deref());
                out = Cow::Owned(out.replace(value.as_str(), &token));
            }
        }
        out
    }
}

impl Redactor for EnvValueRedactor {
    fn redact(&self, record: &AuditRecord) -> AuditRecord {
        rebuild(record, &|text| self.replace(text))
    }
}

// --- FieldPathRedactor --------------------------------------------------------

/// Redacts the entire value at operator-named payload key paths.
///
/// Paths use `.` as a segment separator and `*` as a wildcard for every
/// element of an array. Examples:
///
/// * `"env"` replaces the value at `payload["env"]`.
/// * `"tool_input.command"` replaces `payload["tool_input"]["command"]`.
/// * `"items.*.secret"` replaces the `"secret"` field of each element of
///   `payload["items"]`.
///
/// A path that does not exist in a particular payload is a silent no-op
/// (different SDK messages have different shapes). The replacement token is
/// `[REDACTED:<label>]`, or `[REDACTED:<label>:<digest>]` when `salt` is
/// configured and the redacted value is a string; non-string values yield
/// the bare token because the digest is defined only over UTF-8 cleartext.
#[derive(Debug, Clone)]
pub struct FieldPathRedactor {
    paths: Vec<Vec<String>>,
    salt: Option<Vec<u8>>,
    label: String,
}

impl FieldPathRedactor {
    pub const DEFAULT_LABEL: &'static str = "field";

    pub fn new(paths: &[&str], salt: Option<&[u8]>) -> Result<Self, RedactionError> {
        Self::with_label(paths, salt, Self::DEFAULT_LABEL)
    }

    pub fn with_label(
        paths: &[&str],
        salt: Option<&[u8]>,
        label: &str,
    ) -> Result<Self, RedactionError> {
        if label.is_empty() {
            return Err(configuration(
                "FieldPathRedactor label must be a non-empty string",
            ));
        }
        let mut compiled = Vec::with_capacity(paths.len());
        for path in paths {
            if path.is_empty() {
                return Err(configuration(
                    "FieldPathRedactor paths must be non-empty strings",
                ));
            }
            let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
            if segments.iter().any(String::is_empty) {
                return Err(configuration(format!(
                    "FieldPathRedactor path {path:?} contains an empty segment"
                )));
            }
            compiled.push(segments);
        }
        Ok(Self {
            paths: compiled,
            salt: salt.map(<[u8]>::to_vec),
            label: label.to_owned(),
        })
    }

    fn apply(&self, node: &mut Value, segments: &[String]) {
        let Some((segment, remaining)) = segments.split_first() else {
            return;
        };
        if segment == "*" {
            let Value::Array(items) = node else {
                return;
            };
            for item in items.iter_mut() {
                if remaining.is_empty() {
                    *item = self.replacement(item);
                } else {
                    self.apply(item, remaining);
                }
            }
            return;
        }
        let Value::Object(map) = node else {
            return;
        };
        let Some(child) = map.get_mut(segment) else {
            return;
        };
        if remaining.is_empty() {
            *child = self.replacement(child);
        } else {
            self.apply(child, remaining);
        }
    }

    fn replacement(&self, value: &Value) -> Value {
        Value::String(replace_value(value, &self.label, self.salt.as_deref()))
    }
}

impl Redactor for FieldPathRedactor {
    fn redact(&self, record: &AuditRecord) -> AuditRecord {
        with_payload(record, |payload| {
            let mut root = Value::Object(payload.clone());
            for segments in &self.paths {
                self.apply(&mut root, segments);
            }
            match root {
                Value::Object(map) => map,
                _ => Map::new(),
            }
        })
    }
}

// --- ToolDenylistRedactor -----------------------------------------------------

/// Redacts tool input/result data for tools on a configured denylist.
///
/// Only applies to [`AuditRecord::SdkMessage`]; an [`AuditRecord::Event`]
/// passes through as a fresh-but-equal record. Walks the payload tree and
/// recognises both recorded tool shapes:
///
/// - the agent SDK's content blocks (historical telemetry): a `ToolUseBlock`
///   `{"id", "name", "input"}` and a `ToolResultBlock`
///   `{"tool_use_id", "content"}`;
/// - the normalized agent events the Rust adapters record: a tool call
///   `{"tool_use_id", "tool_name", "tool_input"}`, an approval request
///   `{"tool_name", "tool_input" | "input"}`, and a tool result
///   `{"tool_use_id", "content"}`;
/// - a codex item the adapter could not normalize (no `id`), recorded raw
///   as `{"data": {"item": {"type" | "item_type", "command",
///   "aggregated_output", "arguments", "result", "error", ...}}}`.
///
/// An object naming a denylisted tool (`name` or `tool_name`) has its
/// sibling `input` / `tool_input` / `content` values replaced with
/// redaction tokens. A tool name is denylisted when it is listed exactly or
/// starts with a configured prefix ([`ToolDenylistRedactor::with_prefixes`],
/// for tool families with open-ended names such as MCP tools). An object
/// with no name key whose `type` / `item_type` is denylisted (a raw codex
/// item) has its codex data keys (`command`, `aggregated_output`,
/// `arguments`, `result`, `error`) redacted the same way. A tool *result* carries no name, so the redactor
/// remembers every tool call id it has seen and the name it was issued
/// under, and redacts a result's `content` when its `tool_use_id` maps to a
/// denylisted tool. A result whose originating call was never observed
/// (a stream read from the middle, a truncated file) is redacted too: under
/// a denylist the safe reading of an unattributable result is that it may
/// belong to a denylisted tool.
///
/// The correlation state lives inside the redactor, so one instance must
/// see a run's records in stream order (which every reader in this crate
/// guarantees); a fresh instance per independent stream keeps runs apart.
///
/// The envelope (record variant, `run_id`, `id`, `sequence`, `ts`,
/// `attempt_number`, `iteration_number`, `message_type`) is unchanged. The
/// denylisted tool's *name* is intentionally preserved in the payload so a
/// reader can still tell which tool was invoked; only its inputs and
/// outputs are suppressed.
#[derive(Debug)]
pub struct ToolDenylistRedactor {
    names: HashSet<String>,
    /// Name prefixes denylisting a whole tool family.
    prefixes: Vec<String>,
    salt: Option<Vec<u8>>,
    label: String,
    /// Tool call id -> the tool name it was issued under, learned from the
    /// call records already walked.
    calls: Mutex<HashMap<String, String>>,
}

impl Clone for ToolDenylistRedactor {
    fn clone(&self) -> Self {
        Self {
            names: self.names.clone(),
            prefixes: self.prefixes.clone(),
            salt: self.salt.clone(),
            label: self.label.clone(),
            calls: Mutex::new(self.lock_calls().clone()),
        }
    }
}

/// Keys naming the invoked tool, in both recorded shapes.
const TOOL_NAME_KEYS: [&str; 2] = ["name", "tool_name"];
/// Keys carrying a tool call's id, in both recorded shapes.
const TOOL_ID_KEYS: [&str; 2] = ["id", "tool_use_id"];
/// Keys carrying a denylisted tool's input or output.
const TOOL_DATA_KEYS: [&str; 3] = ["input", "tool_input", "content"];
/// Keys carrying a raw codex item's input or output (`command_execution`:
/// `command`, `aggregated_output`; `mcp_tool_call`: `arguments`, `result`,
/// `error`).
const CODEX_ITEM_DATA_KEYS: [&str; 5] = [
    "command",
    "aggregated_output",
    "arguments",
    "result",
    "error",
];
/// Keys carrying a raw codex item's type (the key drifted across releases).
const CODEX_ITEM_TYPE_KEYS: [&str; 2] = ["type", "item_type"];
/// The token suffix for a result whose originating call was never seen.
const UNATTRIBUTED_TOOL: &str = "unknown";

impl ToolDenylistRedactor {
    pub const DEFAULT_LABEL: &'static str = "tool";

    pub fn new(tool_names: &[&str], salt: Option<&[u8]>) -> Result<Self, RedactionError> {
        Self::with_label(tool_names, salt, Self::DEFAULT_LABEL)
    }

    pub fn with_label(
        tool_names: &[&str],
        salt: Option<&[u8]>,
        label: &str,
    ) -> Result<Self, RedactionError> {
        if label.is_empty() {
            return Err(configuration(
                "ToolDenylistRedactor label must be a non-empty string",
            ));
        }
        let mut names = HashSet::with_capacity(tool_names.len());
        for name in tool_names {
            if name.is_empty() {
                return Err(configuration(
                    "ToolDenylistRedactor tool_names must be non-empty strings",
                ));
            }
            names.insert((*name).to_owned());
        }
        Ok(Self {
            names,
            prefixes: Vec::new(),
            salt: salt.map(<[u8]>::to_vec),
            label: label.to_owned(),
            calls: Mutex::new(HashMap::new()),
        })
    }

    /// Also denylist every tool whose name starts with one of `prefixes`
    /// (e.g. `mcp__` for claude-code MCP tools, `mcp:` for codex ones).
    pub fn with_prefixes(mut self, prefixes: &[&str]) -> Result<Self, RedactionError> {
        for prefix in prefixes {
            if prefix.is_empty() {
                return Err(configuration(
                    "ToolDenylistRedactor prefixes must be non-empty strings",
                ));
            }
            self.prefixes.push((*prefix).to_owned());
        }
        Ok(self)
    }

    /// Whether `tool_name` is denylisted, exactly or by prefix.
    fn denies(&self, tool_name: &str) -> bool {
        self.names.contains(tool_name)
            || self
                .prefixes
                .iter()
                .any(|prefix| tool_name.starts_with(prefix.as_str()))
    }

    fn lock_calls(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn redact_keys(&self, map: &mut JsonObject, tool_name: &str, keys: &[&str]) {
        let token_label = format!("{}:{tool_name}", self.label);
        for key in keys {
            if let Some(value) = map.get_mut(*key) {
                *value = Value::String(replace_value(value, &token_label, self.salt.as_deref()));
            }
        }
    }

    fn walk(&self, node: &mut Value) {
        match node {
            Value::Object(map) => {
                let tool_name = TOOL_NAME_KEYS
                    .iter()
                    .find_map(|key| map.get(*key).and_then(Value::as_str))
                    .map(str::to_owned);
                match tool_name {
                    Some(tool_name) => {
                        if let Some(id) = TOOL_ID_KEYS
                            .iter()
                            .find_map(|key| map.get(*key).and_then(Value::as_str))
                        {
                            self.lock_calls().insert(id.to_owned(), tool_name.clone());
                        }
                        if self.denies(&tool_name) {
                            self.redact_keys(map, &tool_name, &TOOL_DATA_KEYS);
                        }
                    }
                    None => {
                        let item_type = CODEX_ITEM_TYPE_KEYS
                            .iter()
                            .find_map(|key| map.get(*key).and_then(Value::as_str))
                            .filter(|item_type| self.denies(item_type))
                            .map(str::to_owned);
                        if let Some(item_type) = item_type {
                            self.redact_keys(map, &item_type, &CODEX_ITEM_DATA_KEYS);
                        }
                        let result_of = map
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .filter(|_| map.contains_key("content"))
                            .map(|id| self.lock_calls().get(id).cloned());
                        match result_of {
                            Some(Some(origin)) if self.denies(&origin) => {
                                self.redact_keys(map, &origin, &["content"]);
                            }
                            Some(None) => self.redact_keys(map, UNATTRIBUTED_TOOL, &["content"]),
                            _ => {}
                        }
                    }
                }
                for value in map.values_mut() {
                    if matches!(value, Value::Object(_) | Value::Array(_)) {
                        self.walk(value);
                    }
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    if matches!(item, Value::Object(_) | Value::Array(_)) {
                        self.walk(item);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Redactor for ToolDenylistRedactor {
    fn redact(&self, record: &AuditRecord) -> AuditRecord {
        match record {
            AuditRecord::Event(_) => record.clone(),
            AuditRecord::SdkMessage(_) => with_payload(record, |payload| {
                let mut root = Value::Object(payload.clone());
                self.walk(&mut root);
                match root {
                    Value::Object(map) => map,
                    _ => Map::new(),
                }
            }),
        }
    }
}

/// Render a redaction token for an arbitrary payload value.
///
/// Strings produce a digest under `salt`; non-string values yield the bare
/// `[REDACTED:<label>]` token because the digest is defined only over UTF-8
/// cleartext.
fn replace_value(value: &Value, label: &str, salt: Option<&[u8]>) -> String {
    format_token(label, value.as_str(), salt)
}

// --- Composition --------------------------------------------------------------

/// Applies a fixed sequence of redactors in order; built by [`compose`].
pub struct ComposedRedactor {
    redactors: Vec<Box<dyn Redactor>>,
}

impl ComposedRedactor {
    pub fn len(&self) -> usize {
        self.redactors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.redactors.is_empty()
    }
}

impl std::fmt::Debug for ComposedRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposedRedactor")
            .field("len", &self.redactors.len())
            .finish()
    }
}

impl Redactor for ComposedRedactor {
    fn redact(&self, record: &AuditRecord) -> AuditRecord {
        let mut current = record.clone();
        for redactor in &self.redactors {
            current = redactor.redact(&current);
        }
        current
    }
}

/// Return a [`Redactor`] that applies each of `redactors` in order; the
/// output of one feeds the next.
///
/// Order is part of the contract: `compose([a, b])` always runs `a` first,
/// then `b` on `a`'s output, deterministically. `compose([])` is the
/// identity redactor (it still returns a fresh record).
pub fn compose(redactors: Vec<Box<dyn Redactor>>) -> ComposedRedactor {
    ComposedRedactor { redactors }
}

// --- default_policy -----------------------------------------------------------

/// Pattern set baked into [`default_policy`]. Order is significant: specific
/// labels (anthropic, github, pem, jwt, aws) run before the catch-all
/// `generic_api_key` so a more meaningful label is preserved in the redacted
/// output. Each entry is a `(label, regex)` pair.
pub const DEFAULT_PATTERNS: [(&str, &str); 8] = [
    // Anthropic API keys. `sk-ant-...` prefix; the body is base64url-ish so
    // allow letters, digits, `-` and `_`. Length floor keeps a stray
    // "sk-ant-X" log breadcrumb from being treated as a secret.
    ("anthropic_key", r"sk-ant-[A-Za-z0-9_\-]{16,}"),
    // PEM private-key blocks across newlines. Covers `PRIVATE KEY`, `RSA
    // PRIVATE KEY`, `EC PRIVATE KEY`, `OPENSSH PRIVATE KEY`, etc. Non-greedy
    // body so two blocks in one string are redacted separately.
    (
        "pem_private_key",
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    ),
    // JSON Web Tokens. Three base64url segments separated by `.`; the first
    // two start with `eyJ` (the base64 of `{"`). Length floors avoid matching
    // trivially short look-alikes.
    (
        "jwt",
        r"eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
    ),
    // GitHub tokens: personal access (ghp_), OAuth (gho_), server (ghs_),
    // user-to-server (ghu_), refresh (ghr_). 20+ url-safe chars.
    ("github_token", r"gh[pousr]_[A-Za-z0-9]{20,}"),
    // AWS access key IDs (long-term `AKIA` and short-term `ASIA`).
    ("aws_access_key_id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
    // AWS secret access keys, anchored on the literal key name (case
    // insensitive) followed by `:` or `=` and the 40-char value.
    (
        "aws_secret_access_key",
        r#"(?i)aws_secret_access_key["']?\s*[:=]\s*["']?[A-Za-z0-9/+=]{40}["']?"#,
    ),
    // Bearer-token headers, anchored on the literal `Bearer` keyword.
    ("bearer_token", r"\b[Bb]earer\s+[A-Za-z0-9._\-/+=]{16,}"),
    // Generic `sk-...` API keys (OpenAI etc). Runs AFTER `anthropic_key` so
    // `sk-ant-...` keeps the specific label.
    ("generic_api_key", r"\bsk-[A-Za-z0-9_\-]{16,}"),
];

/// Return a [`PatternRedactor`] over the standard built-in secret patterns.
///
/// Covers Anthropic API keys, generic `sk-` API keys, AWS access key IDs and
/// contextual secret keys, PEM private-key blocks, JSON Web Tokens, GitHub
/// tokens, and bearer-token headers. Best-effort: unmatched secrets still
/// pass through; consumers needing custom patterns should layer their own
/// redactors via [`compose`].
///
/// Supplying `salt` switches every token to the salted
/// `[REDACTED:<label>:<digest>]` form. Output is byte-identical across runs
/// and processes for the same `(record, salt)`.
pub fn default_policy(salt: Option<&[u8]>) -> PatternRedactor {
    PatternRedactor::new(&DEFAULT_PATTERNS, salt)
        .expect("DEFAULT_PATTERNS are valid regexes with non-empty labels")
}

/// Tools whose outputs commonly contain credentials (`.env` files, AWS
/// credential files, `~/.ssh` material, decoded JWTs, etc.).
///
/// claude-code: `Read` (and the legacy `NotebookRead`) return file contents;
/// `Grep` returns matching file lines (a `.env` hit echoes the secret);
/// `Bash` is a general shell routinely used to `cat` credential files, and
/// `BashOutput` / `TaskOutput` return a background shell's output. codex:
/// `command_execution` is its shell tool and `mcp_tool_call` the raw item
/// type of an MCP call. Left out on purpose: `Glob` (paths only) and
/// `WebFetch` (remote content, not local files; a token in its URL is
/// still caught by the default patterns). [`strict_policy`] suppresses
/// these tools' inputs and outputs entirely; only the tool name is
/// preserved so a reader can still follow the agent's actions.
pub const CREDENTIAL_READING_TOOLS: [&str; 8] = [
    "Read",
    "NotebookRead",
    "Grep",
    "Bash",
    "BashOutput",
    "TaskOutput",
    "command_execution",
    "mcp_tool_call",
];

/// Tool-name prefixes [`strict_policy`] denylists as whole families: MCP
/// tools take arbitrary inputs and return arbitrary data (a code-search
/// server reads files like `Grep` does). `mcp__` is claude-code's
/// `mcp__<server>__<tool>`; `mcp:` is the codex adapter's
/// `mcp:<server>.<tool>`.
pub const CREDENTIAL_READING_TOOL_PREFIXES: [&str; 2] = ["mcp__", "mcp:"];

/// Return a [`Redactor`] equal to [`default_policy`] plus a
/// [`ToolDenylistRedactor`] over [`CREDENTIAL_READING_TOOLS`] and
/// [`CREDENTIAL_READING_TOOL_PREFIXES`].
///
/// Suitable for runs where an agent may have `cat`-ed or read a credential
/// file: the denylist blanks the input and result blocks for those tools
/// while leaving every record's envelope intact, so the audit trail still
/// shows *which* tool was invoked without leaking *what* it read.
pub fn strict_policy(salt: Option<&[u8]>) -> ComposedRedactor {
    let denylist = ToolDenylistRedactor::new(&CREDENTIAL_READING_TOOLS, salt)
        .and_then(|denylist| denylist.with_prefixes(&CREDENTIAL_READING_TOOL_PREFIXES))
        .expect("CREDENTIAL_READING_TOOLS and prefixes are non-empty");
    compose(vec![Box::new(default_policy(salt)), Box::new(denylist)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use serde_json::json;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-02T12:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn object(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            other => panic!("expected object, got {other}"),
        }
    }

    fn event(payload: Value) -> AuditRecord {
        AuditRecord::Event(EventRecord {
            run_id: "run-1".into(),
            ts: at(),
            kind: "iteration".into(),
            payload: object(payload),
            attempt_number: Some(1),
            id: Some(42),
            sequence: Some(7),
        })
    }

    fn sdk(payload: Value) -> AuditRecord {
        AuditRecord::SdkMessage(SdkMessageRecord {
            run_id: "run-1".into(),
            attempt_number: 1,
            iteration_number: 2,
            message_type: "assistant".into(),
            payload: object(payload),
            ts: at(),
            sequence: Some(11),
            id: Some(99),
        })
    }

    fn payload(record: &AuditRecord) -> Value {
        match record {
            AuditRecord::Event(e) => Value::Object(e.payload.clone()),
            AuditRecord::SdkMessage(m) => Value::Object(m.payload.clone()),
        }
    }

    const ANTHROPIC: (&str, &str) = ("anthropic_key", r"sk-ant-[A-Za-z0-9\-_]+");

    fn anthropic() -> PatternRedactor {
        PatternRedactor::new(&[ANTHROPIC], None).unwrap()
    }

    // --- Token format and HMAC parity with Python ---------------------------

    #[test]
    fn format_token_matches_python_hmac_digests() {
        // Digests computed with Python's
        // `hmac.new(salt, text, hashlib.sha256).hexdigest()[:8]`.
        assert_eq!(
            format_token(
                "anthropic_key",
                Some("sk-ant-AAA"),
                Some(b"flywheel-test-salt-1")
            ),
            "[REDACTED:anthropic_key:02cf00f3]"
        );
        assert_eq!(
            format_token(
                "FLYWHEEL_HMAC",
                Some("secretpayloadvalue1234"),
                Some(b"keyed")
            ),
            "[REDACTED:FLYWHEEL_HMAC:b3988e77]"
        );
        assert_eq!(
            format_token("field", Some("hello-world-secret"), Some(b"keyed")),
            "[REDACTED:field:850689b3]"
        );
        assert_eq!(
            format_token("tool:Bash", Some("rm -rf /"), Some(b"keyed")),
            "[REDACTED:tool:Bash:c2bc0cb1]"
        );
        assert_eq!(
            format_token("x", Some("long-key-case"), Some(&[b'x'; 100])),
            "[REDACTED:x:d47080d4]"
        );
        assert_eq!(format_token("x", None, Some(b"salt")), "[REDACTED:x]");
        assert_eq!(format_token("x", Some("text"), None), "[REDACTED:x]");
    }

    // --- Type preservation --------------------------------------------------

    #[test]
    fn redact_preserves_event_variant_and_metadata() {
        let record = event(json!({"prompt": "key=sk-ant-AAA"}));
        let out = anthropic().redact(&record);
        let (AuditRecord::Event(original), AuditRecord::Event(redacted)) = (&record, &out) else {
            panic!("variant changed");
        };
        assert_eq!(redacted.run_id, original.run_id);
        assert_eq!(redacted.ts, original.ts);
        assert_eq!(redacted.kind, original.kind);
        assert_eq!(redacted.attempt_number, original.attempt_number);
        assert_eq!(redacted.id, original.id);
        assert_eq!(redacted.sequence, original.sequence);
        assert_eq!(
            payload(&out),
            json!({"prompt": "key=[REDACTED:anthropic_key]"})
        );
    }

    #[test]
    fn redact_preserves_sdk_variant_and_metadata() {
        let record = sdk(json!({"text": "leak: sk-ant-XYZ"}));
        let out = anthropic().redact(&record);
        let (AuditRecord::SdkMessage(original), AuditRecord::SdkMessage(redacted)) =
            (&record, &out)
        else {
            panic!("variant changed");
        };
        assert_eq!(redacted.run_id, original.run_id);
        assert_eq!(redacted.attempt_number, original.attempt_number);
        assert_eq!(redacted.iteration_number, original.iteration_number);
        assert_eq!(redacted.message_type, original.message_type);
        assert_eq!(redacted.ts, original.ts);
        assert_eq!(redacted.sequence, original.sequence);
        assert_eq!(redacted.id, original.id);
    }

    #[test]
    fn redact_does_not_mutate_input() {
        let record = event(json!({"prompt": "key=sk-ant-AAA", "nested": {"k": "sk-ant-BBB"}}));
        let snapshot = record.clone();
        let _ = anthropic().redact(&record);
        assert_eq!(record, snapshot);
    }

    // --- Pattern substitution -----------------------------------------------

    #[test]
    fn pattern_redactor_replaces_embedded_and_nested_matches() {
        let out = anthropic().redact(&event(json!({
            "log": "user said: sk-ant-leaked then continued typing",
            "outer": {"inner": {"secret": "sk-ant-deep"}, "fine": "hello"},
            "items": ["sk-ant-a", {"nested": ["sk-ant-b", "clean"]}, ["sk-ant-c"]],
        })));
        assert_eq!(
            payload(&out),
            json!({
                "log": "user said: [REDACTED:anthropic_key] then continued typing",
                "outer": {"inner": {"secret": "[REDACTED:anthropic_key]"}, "fine": "hello"},
                "items": [
                    "[REDACTED:anthropic_key]",
                    {"nested": ["[REDACTED:anthropic_key]", "clean"]},
                    ["[REDACTED:anthropic_key]"],
                ],
            })
        );
    }

    #[test]
    fn pattern_redactor_passes_non_string_leaves_and_edge_cases_through() {
        let original = json!({
            "count": 7, "flag": true, "missing": null, "fraction": 1.5,
            "nested": {"n": 0, "items": [1, 2, 3, false, null]},
            "prompt": "", "tags": ["a", "b"],
        });
        let out = anthropic().redact(&event(original.clone()));
        assert_eq!(payload(&out), original);
        assert_eq!(payload(&anthropic().redact(&event(json!({})))), json!({}));
    }

    #[test]
    fn pattern_redactor_label_containing_dollar_is_literal() {
        let redactor = PatternRedactor::new(&[("$1", "foo")], None).unwrap();
        let out = redactor.redact(&event(json!({"x": "foo"})));
        assert_eq!(payload(&out), json!({"x": "[REDACTED:$1]"}));
    }

    #[test]
    fn pattern_redactor_rejects_bad_regex_and_empty_label() {
        let err = PatternRedactor::new(&[("bad", "(unclosed")], None).unwrap_err();
        assert!(matches!(err, RedactionError::InvalidPattern { ref label, .. } if label == "bad"));
        assert!(
            err.to_string()
                .starts_with("invalid regex for label \"bad\": ")
        );
        let err = PatternRedactor::new(&[("", r"sk-ant-\w+")], None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "PatternRedactor pattern label must be a non-empty string"
        );
    }

    // --- Composition --------------------------------------------------------

    #[test]
    fn compose_applies_in_order_and_is_deterministic() {
        let first = PatternRedactor::new(&[("first", "foo")], None).unwrap();
        let second = PatternRedactor::new(&[("second", r"\[REDACTED:first\]")], None).unwrap();
        let composed = compose(vec![Box::new(first), Box::new(second)]);
        assert_eq!(composed.len(), 2);
        let out = composed.redact(&event(json!({"x": "foo"})));
        assert_eq!(payload(&out), json!({"x": "[REDACTED:second]"}));

        let composed = compose(vec![
            Box::new(anthropic()),
            Box::new(PatternRedactor::new(&[("aws_key", r"AKIA[0-9A-Z]{16}")], None).unwrap()),
        ]);
        let record =
            event(json!({"msg": "anthropic=sk-ant-zzz aws=AKIAIOSFODNN7EXAMPLE other=clean"}));
        let a = composed.redact(&record);
        let b = composed.redact(&record);
        assert_eq!(a, b);
        assert_eq!(
            payload(&a),
            json!({"msg": "anthropic=[REDACTED:anthropic_key] aws=[REDACTED:aws_key] other=clean"})
        );
    }

    #[test]
    fn compose_empty_is_identity_and_threads_variant_through() {
        let composed = compose(vec![]);
        assert!(composed.is_empty());
        let record = event(json!({"prompt": "sk-ant-untouched"}));
        assert_eq!(composed.redact(&record), record);
        let out =
            compose(vec![Box::new(anthropic())]).redact(&sdk(json!({"text": "sk-ant-keep-type"})));
        assert!(matches!(out, AuditRecord::SdkMessage(_)));
    }

    #[test]
    fn redactor_is_usable_through_box_and_arc() {
        let boxed: Box<dyn Redactor> = Box::new(anthropic());
        let shared: Arc<dyn Redactor> = Arc::new(anthropic());
        let record = event(json!({"x": "sk-ant-AAA"}));
        assert_eq!(boxed.redact(&record), shared.redact(&record));
        assert_eq!(
            payload(&boxed.redact(&record)),
            json!({"x": "[REDACTED:anthropic_key]"})
        );
    }

    // --- EnvValueRedactor ---------------------------------------------------

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    fn env_redactor(
        names: &[&str],
        vars: &[(&str, &str)],
        options: EnvValueOptions,
    ) -> EnvValueRedactor {
        EnvValueRedactor::with_lookup(names, &options, env(vars)).unwrap()
    }

    #[test]
    fn env_value_redactor_redacts_known_value_everywhere() {
        let redactor = env_redactor(
            &["FLYWHEEL_TEST_SECRET"],
            &[("FLYWHEEL_TEST_SECRET", "supersecretvalue12345")],
            EnvValueOptions::default(),
        );
        let out = redactor.redact(&event(json!({
            "prompt": "leaked=supersecretvalue12345 end",
            "ok": "no secret",
            "nested": {"items": ["supersecretvalue12345", "clean"],
                       "deep": {"k": "before supersecretvalue12345 after"}},
        })));
        assert_eq!(
            payload(&out),
            json!({
                "prompt": "leaked=[REDACTED:FLYWHEEL_TEST_SECRET] end",
                "ok": "no secret",
                "nested": {"items": ["[REDACTED:FLYWHEEL_TEST_SECRET]", "clean"],
                           "deep": {"k": "before [REDACTED:FLYWHEEL_TEST_SECRET] after"}},
            })
        );
    }

    #[test]
    fn env_value_redactor_guards_absent_empty_and_short_values() {
        let vars = [
            ("FLYWHEEL_EMPTY", ""),
            ("FLYWHEEL_SHORT", "abc"),
            ("FLYWHEEL_TINY", "x"),
        ];
        let redactor = env_redactor(
            &[
                "FLYWHEEL_MISSING",
                "FLYWHEEL_EMPTY",
                "FLYWHEEL_SHORT",
                "FLYWHEEL_TINY",
            ],
            &vars,
            EnvValueOptions::default(),
        );
        assert_eq!(redactor.captured().count(), 0);
        let original = json!({"prompt": "abc shows up abc and x marks the spot"});
        let out = redactor.redact(&event(original.clone()));
        assert_eq!(payload(&out), original);
    }

    #[test]
    fn env_value_redactor_captures_at_construction_and_supports_labels() {
        let redactor = env_redactor(
            &["ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
            &[("ANTHROPIC_API_KEY", "abcdefghijklmnopqrstu")],
            EnvValueOptions {
                labels: HashMap::from([(
                    "ANTHROPIC_API_KEY".to_owned(),
                    "anthropic_key".to_owned(),
                )]),
                ..EnvValueOptions::default()
            },
        );
        assert_eq!(redactor.captured().count(), 1);
        let out = redactor.redact(&event(
            json!({"prompt": "leak=abcdefghijklmnopqrstu new=rotated9876543210"}),
        ));
        assert_eq!(
            payload(&out),
            json!({"prompt": "leak=[REDACTED:anthropic_key] new=rotated9876543210"})
        );
    }

    #[test]
    fn env_value_redactor_is_literal_and_prefers_longer_values() {
        let redactor = env_redactor(
            &["SHORT", "FLYWHEEL_RX", "LONG"],
            &[
                ("SHORT", "abc[.*]+"),
                ("FLYWHEEL_RX", "abc[.*]+?value"),
                ("LONG", "abc[.*]+?value-longer"),
            ],
            EnvValueOptions::default(),
        );
        let labels: Vec<&str> = redactor.captured().map(|(label, _)| label).collect();
        assert_eq!(labels, vec!["LONG", "FLYWHEEL_RX", "SHORT"]);
        let out = redactor.redact(&event(
            json!({"prompt": "x=abc[.*]+?value y abc[.*]+?value-longer"}),
        ));
        assert_eq!(
            payload(&out),
            json!({"prompt": "x=[REDACTED:FLYWHEEL_RX] y [REDACTED:LONG]"})
        );
    }

    #[test]
    fn env_value_redactor_reads_process_environment_once() {
        // PATH is set in every realistic test environment; the assertion is
        // conditional on its presence so the test is hermetic either way.
        let redactor = EnvValueRedactor::new(&["PATH"], None).unwrap();
        match std::env::var("PATH") {
            Ok(value) if value.chars().count() >= EnvValueRedactor::DEFAULT_MINIMUM_LENGTH => {
                let out = redactor.redact(&event(json!({"p": format!("path={value}!")})));
                assert_eq!(payload(&out), json!({"p": "path=[REDACTED:PATH]!"}));
            }
            _ => assert_eq!(redactor.captured().count(), 0),
        }
    }

    #[test]
    fn env_value_redactor_rejects_bad_configuration() {
        let err = EnvValueRedactor::with_lookup(
            &["X"],
            &EnvValueOptions {
                minimum_length: Some(0),
                ..EnvValueOptions::default()
            },
            env(&[]),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "EnvValueRedactor minimum_length must be >= 1"
        );
        let err = EnvValueRedactor::with_lookup(&[""], &EnvValueOptions::default(), env(&[]))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "EnvValueRedactor var_names must be non-empty strings"
        );
        let err = EnvValueRedactor::with_lookup(
            &["X"],
            &EnvValueOptions {
                labels: HashMap::from([("X".to_owned(), String::new())]),
                ..EnvValueOptions::default()
            },
            env(&[("X", "longenoughvalue")]),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "EnvValueRedactor labels values must be non-empty strings"
        );
    }

    #[test]
    fn env_value_redactor_salt_yields_digest_token() {
        let redactor = env_redactor(
            &["FLYWHEEL_HMAC"],
            &[("FLYWHEEL_HMAC", "secretpayloadvalue1234")],
            EnvValueOptions {
                salt: Some(b"keyed".to_vec()),
                ..EnvValueOptions::default()
            },
        );
        let out = redactor.redact(&event(json!({"prompt": "leak=secretpayloadvalue1234"})));
        assert_eq!(
            payload(&out),
            json!({"prompt": "leak=[REDACTED:FLYWHEEL_HMAC:b3988e77]"})
        );
    }

    // --- FieldPathRedactor --------------------------------------------------

    #[test]
    fn field_path_redactor_replaces_top_level_nested_and_wildcards() {
        let redactor = FieldPathRedactor::new(
            &["env", "tool_input.command", "items.*", "rows.*.secret"],
            None,
        )
        .unwrap();
        let out = redactor.redact(&event(json!({
            "env": {"K": "V"}, "other": "ok",
            "tool_input": {"command": "rm -rf /", "args": ["x"]},
            "items": ["a", "b"],
            "rows": [{"secret": "x", "label": "a"}, {"secret": "y", "label": "b"}],
        })));
        assert_eq!(
            payload(&out),
            json!({
                "env": "[REDACTED:field]", "other": "ok",
                "tool_input": {"command": "[REDACTED:field]", "args": ["x"]},
                "items": ["[REDACTED:field]", "[REDACTED:field]"],
                "rows": [{"secret": "[REDACTED:field]", "label": "a"},
                         {"secret": "[REDACTED:field]", "label": "b"}],
            })
        );
    }

    #[test]
    fn field_path_redactor_is_silent_on_missing_paths_and_type_mismatches() {
        let redactor =
            FieldPathRedactor::new(&["does.not.exist", "also_missing", "b.*", "c.*.d"], None)
                .unwrap();
        let original = json!({"a": 1, "b": "two", "c": {"d": "e"}});
        let record = event(original.clone());
        let out = redactor.redact(&record);
        assert_eq!(payload(&out), original);
        assert_eq!(record, event(original));
    }

    #[test]
    fn field_path_redactor_tokens_for_non_string_values_are_bare() {
        let out = FieldPathRedactor::new(&["count"], Some(b"keyed"))
            .unwrap()
            .redact(&event(json!({"count": 42})));
        assert_eq!(payload(&out), json!({"count": "[REDACTED:field]"}));
        let out = FieldPathRedactor::new(&["nested.k"], Some(b"keyed"))
            .unwrap()
            .redact(&event(json!({"nested": {"k": "hello-world-secret"}})));
        assert_eq!(
            payload(&out),
            json!({"nested": {"k": "[REDACTED:field:850689b3]"}})
        );
    }

    #[test]
    fn field_path_redactor_rejects_bad_paths_and_labels() {
        assert_eq!(
            FieldPathRedactor::new(&[""], None).unwrap_err().to_string(),
            "FieldPathRedactor paths must be non-empty strings"
        );
        assert_eq!(
            FieldPathRedactor::new(&["a..b"], None)
                .unwrap_err()
                .to_string(),
            "FieldPathRedactor path \"a..b\" contains an empty segment"
        );
        assert_eq!(
            FieldPathRedactor::with_label(&["a"], None, "")
                .unwrap_err()
                .to_string(),
            "FieldPathRedactor label must be a non-empty string"
        );
        let out = FieldPathRedactor::with_label(&["a"], None, "custom")
            .unwrap()
            .redact(&event(json!({"a": 1})));
        assert_eq!(payload(&out), json!({"a": "[REDACTED:custom]"}));
    }

    // --- ToolDenylistRedactor -----------------------------------------------

    #[test]
    fn tool_denylist_redacts_input_and_content_of_named_tools_only() {
        let redactor = ToolDenylistRedactor::new(&["Bash", "ReadCredentials"], None).unwrap();
        let out = redactor.redact(&sdk(json!({
            "message_type": "AssistantMessage",
            "content": [
                {"name": "Bash", "input": {"command": "rm -rf /"}, "id": "tu-1"},
                {"name": "ReadCredentials", "content": "aws_secret=raw_value_here", "id": "tu-7"},
                {"name": "Read", "input": {"file": "ok.txt"}},
                {"text": "hello"},
            ],
            "outer": {"wrapper": [{"name": "Bash", "input": {"command": "ls"}}]},
        })));
        assert_eq!(
            payload(&out),
            json!({
                "message_type": "AssistantMessage",
                "content": [
                    {"name": "Bash", "input": "[REDACTED:tool:Bash]", "id": "tu-1"},
                    {"name": "ReadCredentials", "content": "[REDACTED:tool:ReadCredentials]", "id": "tu-7"},
                    {"name": "Read", "input": {"file": "ok.txt"}},
                    {"text": "hello"},
                ],
                "outer": {"wrapper": [{"name": "Bash", "input": "[REDACTED:tool:Bash]"}]},
            })
        );
        assert!(
            matches!(out, AuditRecord::SdkMessage(ref m) if m.sequence == Some(11) && m.id == Some(99))
        );
    }

    #[test]
    fn tool_denylist_leaves_event_records_untouched_and_never_mutates_input() {
        let redactor = ToolDenylistRedactor::new(&["Bash"], None).unwrap();
        let record =
            event(json!({"content": [{"name": "Bash", "input": {"command": "rm -rf /"}}]}));
        assert_eq!(redactor.redact(&record), record);
        let record = sdk(json!({"content": [{"name": "Bash", "input": {"command": "ls"}}]}));
        let snapshot = record.clone();
        let _ = redactor.redact(&record);
        assert_eq!(record, snapshot);
    }

    #[test]
    fn tool_denylist_salt_and_validation() {
        let out = ToolDenylistRedactor::new(&["Bash"], Some(b"keyed"))
            .unwrap()
            .redact(&sdk(
                json!({"content": [{"name": "Bash", "input": "rm -rf /", "id": "tu-1"}]}),
            ));
        assert_eq!(
            payload(&out),
            json!({"content": [{"name": "Bash", "input": "[REDACTED:tool:Bash:c2bc0cb1]", "id": "tu-1"}]})
        );
        assert_eq!(
            ToolDenylistRedactor::new(&[""], None)
                .unwrap_err()
                .to_string(),
            "ToolDenylistRedactor tool_names must be non-empty strings"
        );
        assert_eq!(
            ToolDenylistRedactor::with_label(&["Bash"], None, "")
                .unwrap_err()
                .to_string(),
            "ToolDenylistRedactor label must be a non-empty string"
        );
    }

    // --- default_policy / strict_policy -------------------------------------

    #[test]
    fn default_policy_covers_every_builtin_pattern() {
        let redactor = default_policy(None);
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Q\nu8VCt4N3p/AYbXk7w==\n-----END RSA PRIVATE KEY-----";
        let pem2 = "-----BEGIN PRIVATE KEY-----\nMIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQ==\n-----END PRIVATE KEY-----";
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4ifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redactor.redact(&event(json!({
            "anthropic": "ANTHROPIC_API_KEY=sk-ant-abc123DEF456ghi789JKLmno",
            "generic": "key=sk-proj1234567890abcdefghij",
            "aws_id": "id=AKIAIOSFODNN7EXAMPLE here",
            "aws_secret": "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "pem": pem, "pem2": pem2, "jwt": jwt,
            "github": "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD",
            "bearer": "Authorization: Bearer abcdef1234567890ABCDEFghij",
            "clean": "Hello, world. No secrets. sk-default",
        })));
        assert_eq!(
            payload(&out),
            json!({
                "anthropic": "ANTHROPIC_API_KEY=[REDACTED:anthropic_key]",
                "generic": "key=[REDACTED:generic_api_key]",
                "aws_id": "id=[REDACTED:aws_access_key_id] here",
                "aws_secret": "[REDACTED:aws_secret_access_key]",
                "pem": "[REDACTED:pem_private_key]",
                "pem2": "[REDACTED:pem_private_key]",
                "jwt": "[REDACTED:jwt]",
                "github": "token=[REDACTED:github_token]",
                "bearer": "Authorization: [REDACTED:bearer_token]",
                "clean": "Hello, world. No secrets. sk-default",
            })
        );
        assert_eq!(redactor.patterns().count(), DEFAULT_PATTERNS.len());
    }

    #[test]
    fn default_policy_salted_tokens_are_stable_and_distinct() {
        let salt = b"flywheel-salt";
        let record = event(
            json!({"a": "sk-ant-AAAAAAAAAAAAAAAA", "b": "sk-ant-BBBBBBBBBBBBBBBB", "log": "Bearer abcdefghij1234567890"}),
        );
        let a = default_policy(Some(salt)).redact(&record);
        let b = default_policy(Some(salt)).redact(&record);
        assert_eq!(a, b);
        let pa = payload(&a);
        let token_a = pa["a"].as_str().unwrap();
        let token_b = pa["b"].as_str().unwrap();
        assert_ne!(token_a, token_b);
        assert!(token_a.starts_with("[REDACTED:anthropic_key:"));
        let digest = token_a.rsplit(':').next().unwrap().trim_end_matches(']');
        assert_eq!(digest.len(), DIGEST_HEX_LEN);
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        let other_salt = default_policy(Some(b"saltB")).redact(&record);
        assert_ne!(payload(&other_salt)["a"], pa["a"]);
        // Identical cleartext correlates across record variants.
        let shared = PatternRedactor::new(&[ANTHROPIC], Some(b"stable")).unwrap();
        let e = shared.redact(&event(json!({"a": "sk-ant-SAME"})));
        let s = shared.redact(&sdk(json!({"b": "sk-ant-SAME"})));
        assert_eq!(payload(&e)["a"], payload(&s)["b"]);
        assert_eq!(payload(&e)["a"], "[REDACTED:anthropic_key:0704bbda]");
    }

    #[test]
    fn strict_policy_adds_tool_denylist_over_default_patterns() {
        let redactor = strict_policy(None);
        assert_eq!(redactor.len(), 2);
        let out = redactor.redact(&sdk(json!({
            "text": "sk-ant-abcdef0123456789ABCDEF",
            "content": [
                {"type": "tool_use", "name": "Read", "input": {"file_path": "/home/me/.env"}},
                {"type": "tool_use", "name": "Edit", "input": {"file_path": "/src/x"}},
            ],
        })));
        assert_eq!(
            payload(&out),
            json!({
                "text": "[REDACTED:anthropic_key]",
                "content": [
                    {"type": "tool_use", "name": "Read", "input": "[REDACTED:tool:Read]"},
                    {"type": "tool_use", "name": "Edit", "input": {"file_path": "/src/x"}},
                ],
            })
        );
        let _ = strict_policy(Some(b"x"));
    }

    /// One normalized agent event exactly as the Rust adapters record it:
    /// `RunTelemetry::sdk_message` over `event_to_message`, with the
    /// event's `type` as the record's message type.
    fn recorded_event(
        kind: &str,
        native: &str,
        event_payload: Value,
        sequence: u64,
    ) -> AuditRecord {
        AuditRecord::SdkMessage(SdkMessageRecord {
            run_id: "run-1".into(),
            attempt_number: 1,
            iteration_number: 1,
            message_type: kind.into(),
            payload: object(json!({
                "type": kind,
                "payload": event_payload,
                "source": "claude-code",
                "native_type": native,
                "sequence": sequence,
                "timestamp": "2026-06-02T12:00:00Z",
            })),
            ts: at(),
            sequence: Some(sequence),
            id: None,
        })
    }

    #[test]
    fn strict_policy_redacts_recorded_tool_calls_and_correlated_results() {
        let redactor = strict_policy(None);
        let read_call = redactor.redact(&recorded_event(
            "tool.started",
            "assistant",
            json!({"tool_use_id": "toolu_01", "tool_name": "Read", "tool_input": {"file_path": "/home/me/.env"}}),
            1,
        ));
        assert_eq!(
            payload(&read_call)["payload"],
            json!({"tool_use_id": "toolu_01", "tool_name": "Read", "tool_input": "[REDACTED:tool:Read]"})
        );
        let read_result = redactor.redact(&recorded_event(
            "tool.finished",
            "user",
            json!({"tool_use_id": "toolu_01", "is_error": false, "content": "DB_PASSWORD=hunter2"}),
            2,
        ));
        assert_eq!(
            payload(&read_result)["payload"],
            json!({"tool_use_id": "toolu_01", "is_error": false, "content": "[REDACTED:tool:Read]"})
        );
        // A non-denylisted tool's call and result pass through untouched.
        let edit_call = recorded_event(
            "tool.started",
            "assistant",
            json!({"tool_use_id": "toolu_02", "tool_name": "Edit", "tool_input": {"file_path": "src/x"}}),
            3,
        );
        assert_eq!(redactor.redact(&edit_call), edit_call);
        let edit_result = recorded_event(
            "tool.finished",
            "user",
            json!({"tool_use_id": "toolu_02", "is_error": false, "content": "edited"}),
            4,
        );
        assert_eq!(redactor.redact(&edit_result), edit_result);
        // A result whose originating call was never observed fails closed.
        let orphan = redactor.redact(&recorded_event(
            "tool.finished",
            "user",
            json!({"tool_use_id": "toolu_99", "is_error": false, "content": "cat output"}),
            5,
        ));
        assert_eq!(
            payload(&orphan)["payload"]["content"],
            "[REDACTED:tool:unknown]"
        );
        // The approval request carries the input both flat and in `detail`.
        let approval = redactor.redact(&recorded_event(
            "approval.requested",
            "control_request",
            json!({
                "request_id": "r1",
                "tool_name": "Bash",
                "tool_input": {"command": "cat ~/.aws/credentials"},
                "tool_use_id": "toolu_03",
                "detail": {"tool_name": "Bash", "input": {"command": "cat ~/.aws/credentials"}},
            }),
            6,
        ));
        let approval = payload(&approval);
        assert_eq!(approval["payload"]["tool_input"], "[REDACTED:tool:Bash]");
        assert_eq!(
            approval["payload"]["detail"]["input"],
            "[REDACTED:tool:Bash]"
        );
        assert_eq!(approval["payload"]["tool_name"], "Bash");
    }

    #[test]
    fn strict_policy_still_redacts_historical_sdk_shaped_results() {
        let redactor = strict_policy(None);
        let _ = redactor.redact(&sdk(json!({
            "message_type": "AssistantMessage",
            "content": [{"type": "tool_use", "id": "tu-1", "name": "Bash", "input": {"command": "cat .env"}}],
        })));
        let result = redactor.redact(&sdk(json!({
            "message_type": "UserMessage",
            "content": [{"type": "tool_result", "tool_use_id": "tu-1", "content": "SECRET=1"}],
        })));
        assert_eq!(
            payload(&result)["content"][0]["content"],
            "[REDACTED:tool:Bash]"
        );
    }

    #[test]
    fn strict_policy_redacts_codex_shell_results() {
        let redactor = strict_policy(None);
        let _ = redactor.redact(&recorded_event(
            "tool.started",
            "item.started",
            json!({"tool_use_id": "item_1", "tool_name": "command_execution", "tool_input": {"command": "cat .env"}}),
            1,
        ));
        let result = redactor.redact(&recorded_event(
            "tool.finished",
            "item.completed",
            json!({"tool_use_id": "item_1", "is_error": false, "content": "SECRET=1"}),
            2,
        ));
        assert_eq!(
            payload(&result)["payload"]["content"],
            "[REDACTED:tool:command_execution]"
        );
    }

    /// Feed `lines` through a real adapter normalizer and record each event
    /// exactly as the harness does (`event_to_message` into an SDK-message
    /// record), so the strict tests bind to the shapes actually persisted.
    fn adapter_records(
        mut feed: impl FnMut(&str) -> flywheel_agents::jsonl::LineOutcome,
        lines: &[Value],
    ) -> Vec<AuditRecord> {
        let mut records = Vec::new();
        for line in lines {
            for event in feed(&line.to_string()).events {
                let message = crate::agents_invoke::event_to_message(&event);
                let sequence = records.len() as u64 + 1;
                records.push(AuditRecord::SdkMessage(SdkMessageRecord {
                    run_id: "run-1".into(),
                    attempt_number: 1,
                    iteration_number: 1,
                    message_type: message["type"].as_str().unwrap_or_default().into(),
                    payload: object(message),
                    ts: at(),
                    sequence: Some(sequence),
                    id: None,
                }));
            }
        }
        records
    }

    fn redacted_text(redactor: &dyn Redactor, records: &[AuditRecord]) -> Vec<Value> {
        records
            .iter()
            .map(|record| payload(&redactor.redact(record)))
            .collect()
    }

    fn claude_tool_turn(id: &str, name: &str, input: Value, result: &str) -> [Value; 2] {
        [
            json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": id, "name": name, "input": input}
            ]}}),
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "content": result}
            ]}}),
        ]
    }

    #[test]
    fn strict_policy_redacts_claude_file_reading_and_mcp_tools() {
        let redactor = strict_policy(None);
        let mut normalizer = flywheel_agents::claude_code::ClaudeStreamNormalizer::new();
        let mut lines = Vec::new();
        for (id, name) in [
            ("toolu_g", "Grep"),
            ("toolu_o", "BashOutput"),
            ("toolu_t", "TaskOutput"),
            ("toolu_n", "NotebookRead"),
            ("toolu_m", "mcp__serena__search_for_pattern"),
        ] {
            lines.extend(claude_tool_turn(
                id,
                name,
                json!({"pattern": "DB_PASSWORD"}),
                ".env:1:DB_PASSWORD=hunter2",
            ));
        }
        let records = adapter_records(|line| normalizer.feed_line(line), &lines);
        let started: Vec<_> = records
            .iter()
            .filter(|record| matches!(record, AuditRecord::SdkMessage(m) if m.message_type == "tool.started"))
            .collect();
        assert_eq!(started.len(), 5, "every tool call was normalized");
        let out = redacted_text(&redactor, &records);
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("DB_PASSWORD"), "{text}");
        let grep_call = out
            .iter()
            .find(|message| message["payload"]["tool_name"] == "Grep")
            .unwrap();
        assert_eq!(grep_call["payload"]["tool_input"], "[REDACTED:tool:Grep]");
        // The MCP result is attributed through its call id, not left as an
        // unattributed fallback.
        assert!(text.contains("[REDACTED:tool:mcp__serena__search_for_pattern]"));

        // Paths-only and remote-content tools pass through untouched.
        let mut normalizer = flywheel_agents::claude_code::ClaudeStreamNormalizer::new();
        let mut lines = claude_tool_turn(
            "toolu_l",
            "Glob",
            json!({"pattern": "**/*.rs"}),
            "src/lib.rs",
        )
        .to_vec();
        lines.extend(claude_tool_turn(
            "toolu_w",
            "WebFetch",
            json!({"url": "https://example.com"}),
            "page",
        ));
        let fresh = strict_policy(None);
        for record in adapter_records(|line| normalizer.feed_line(line), &lines) {
            assert_eq!(fresh.redact(&record), record);
        }
    }

    #[test]
    fn strict_policy_redacts_codex_mcp_tool_calls_in_both_recorded_shapes() {
        let redactor = strict_policy(None);
        let mut normalizer = flywheel_agents::codex::CodexStreamNormalizer::new();
        let item = json!({
            "type": "mcp_tool_call",
            "server": "files",
            "tool": "read_file",
            "arguments": {"path": ".env"},
            "result": {"content": [{"type": "text", "text": "SECRET=hunter2"}]},
            "error": null,
            "status": "completed",
        });
        let mut with_id = item.clone();
        with_id["id"] = json!("item_5");
        let lines = [
            json!({"type": "item.completed", "item": with_id}),
            // No `id`: the adapter falls back to a raw event carrying the item.
            json!({"type": "item.completed", "item": item}),
        ];
        let records = adapter_records(|line| normalizer.feed_line(line), &lines);
        let kinds: Vec<_> = records
            .iter()
            .map(|record| match record {
                AuditRecord::SdkMessage(m) => m.message_type.clone(),
                AuditRecord::Event(_) => String::new(),
            })
            .collect();
        assert_eq!(kinds, ["tool.started", "tool.finished", "raw"]);
        let out = redacted_text(&redactor, &records);
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains(".env"), "{text}");
        assert_eq!(out[0]["payload"]["tool_name"], "mcp:files.read_file");
        assert_eq!(
            out[0]["payload"]["tool_input"],
            "[REDACTED:tool:mcp:files.read_file]"
        );
        assert_eq!(
            out[1]["payload"]["content"],
            "[REDACTED:tool:mcp:files.read_file]"
        );
        let raw_item = &out[2]["payload"]["data"]["item"];
        assert_eq!(raw_item["arguments"], "[REDACTED:tool:mcp_tool_call]");
        assert_eq!(raw_item["result"], "[REDACTED:tool:mcp_tool_call]");
        // The server and tool still say what was invoked.
        assert_eq!(raw_item["server"], "files");
        assert_eq!(raw_item["tool"], "read_file");
    }

    #[test]
    fn strict_policy_redacts_a_codex_shell_item_recorded_raw_without_an_id() {
        let redactor = strict_policy(None);
        let mut normalizer = flywheel_agents::codex::CodexStreamNormalizer::new();
        let lines = [
            json!({"type": "item.completed", "item": {
                "type": "command_execution",
                "command": "cat .env",
                "aggregated_output": "SECRET=hunter2",
                "exit_code": 0,
                "status": "completed",
            }}),
            // The drifted `item_type` key is recognised too.
            json!({"type": "item.completed", "item": {
                "item_type": "command_execution",
                "command": "cat ~/.aws/credentials",
                "aggregated_output": "aws_key=hunter2",
                "exit_code": 0,
            }}),
        ];
        let records = adapter_records(|line| normalizer.feed_line(line), &lines);
        assert_eq!(records.len(), 2);
        let out = redacted_text(&redactor, &records);
        for message in &out {
            assert_eq!(message["type"], "raw");
            let item = &message["payload"]["data"]["item"];
            assert_eq!(item["command"], "[REDACTED:tool:command_execution]");
            assert_eq!(
                item["aggregated_output"],
                "[REDACTED:tool:command_execution]"
            );
            assert_eq!(item["exit_code"], 0);
        }
        // A non-denylisted raw item is untouched.
        let other = adapter_records(
            |line| normalizer.feed_line(line),
            &[
                json!({"type": "item.completed", "item": {"type": "file_change", "command": "kept"}}),
            ],
        );
        assert_eq!(redactor.redact(&other[0]), other[0]);
    }

    #[test]
    fn tool_denylist_prefixes_reject_empty_and_match_families() {
        assert!(
            ToolDenylistRedactor::new(&["Read"], None)
                .unwrap()
                .with_prefixes(&[""])
                .is_err()
        );
        let redactor = ToolDenylistRedactor::new(&[], None)
            .unwrap()
            .with_prefixes(&["mcp__"])
            .unwrap();
        let out = redactor.redact(&sdk(json!({"content": [
            {"type": "tool_use", "id": "a", "name": "mcp__db__query", "input": {"sql": "select"}},
            {"type": "tool_use", "id": "b", "name": "Edit", "input": {"file_path": "x"}},
        ]})));
        let content = &payload(&out)["content"];
        assert_eq!(content[0]["input"], "[REDACTED:tool:mcp__db__query]");
        assert_eq!(content[1]["input"], json!({"file_path": "x"}));
    }
}
