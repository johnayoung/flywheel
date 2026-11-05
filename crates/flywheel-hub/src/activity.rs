//! The activity journal: what happened on this hub, as a bounded,
//! append-only timeline (`<state-dir>/activity.jsonl`, 0600), and the
//! "while you were away" digest each client gets on its return.
//!
//! Rows are fed from the hub's one event bus ([`crate::hub::HubEvent`]):
//! the plugin lifecycle events ([`crate::plugins::EVENTS`]), finished
//! turns, suggested commands, usage growth, and plugin runs -- the same
//! emissions plugins and the UI hear, never a second path. A row holds
//! only what the UI already shows in short: the event kind, the session's
//! and workspace's ids and names, and a few whitelisted facts (a status,
//! a branch, a count, a cost). Never terminal output, prompts, command
//! text, grader output, credentials, or paths; free text is capped and
//! scrubbed of token-shaped words ([`scrub`]).
//!
//! Bounds: the file rotates at [`FILE_CAP`] bytes or once its first row is
//! [`ROTATE_AGE`] (a week) old into `activity.1.jsonl` ..
//! `activity.<KEEP_ROTATED>` (the oldest dropped); rows older than
//! [`MAX_AGE_DAYS`] are forgotten. So a hub keeps 30 days, or 6 MiB on a
//! very busy one, whichever is less.
//! The files outlive restarts and live handoffs (`fw hub upgrade`).
//!
//! Presence ([`marks`]): each client id (the browser's per-device id, the
//! CLI's) has a last-active time, kept in `activity-clients.json`; a client
//! back after [`AWAY_MIN`] is sent one `away` message, and its digest
//! covers the rows since then. Operator doc: `docs/hub/workflows.md`
//! (Activity).

mod digest;
mod feed;
mod journal;
mod marks;
#[cfg(test)]
mod tests;

pub use digest::{Digest, DigestFanout, DigestSession, DigestWorkspace};
pub use feed::Activity;
pub(crate) use feed::digest_row;
#[cfg(test)]
pub(crate) use feed::draft_for;
pub use journal::{Journal, Limits};
pub use marks::{Marks, Return};

use chrono::{DateTime, Duration as Span, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Row kinds besides the plugin events ([`crate::plugins::EVENTS`]).
pub const EXTRA_KINDS: &[&str] = &[
    "turn.finished",
    "commands.suggested",
    "usage",
    "plugin.ran",
    "hub.stopped",
];
/// The journal file in the state dir (rotated ones are `activity.N.jsonl`).
pub const JOURNAL_FILE: &str = "activity.jsonl";
/// Each client's last-active time.
pub const MARKS_FILE: &str = "activity-clients.json";
/// The journal file's size cap before it rotates.
pub const FILE_CAP: u64 = 1024 * 1024;
/// Rotated files kept besides the current one.
pub const KEEP_ROTATED: usize = 5;
/// A journal file whose first row is this old rotates on the next row.
pub const ROTATE_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 86_400);
/// Rows older than this are forgotten.
pub const MAX_AGE_DAYS: i64 = 30;
// Age rotation alone never drops a row younger than MAX_AGE_DAYS: at a
// rotation the kept files span at least KEEP_ROTATED * ROTATE_AGE.
const _: () = assert!(KEEP_ROTATED as u64 * ROTATE_AGE.as_secs() >= MAX_AGE_DAYS as u64 * 86_400);
/// Away at least this long: the return gets a digest.
pub const AWAY_MIN: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// Rows per page: the default and the most.
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;
/// The longest free text a row keeps (characters).
pub const TEXT_CAP: usize = 160;

/// One thing that happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// Increases with every row of this hub (the paging cursor).
    pub id: u64,
    pub at: DateTime<Utc>,
    /// A plugin event name (`session.created`, `review.merged`, ...) or
    /// `turn.finished`, `commands.suggested`, `usage`, `plugin.ran`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The session's kind (`shell`, `claude`, `codex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// One line, as the timeline shows it ("merged hub/41 into main").
    pub text: String,
    /// The kind's whitelisted facts: what a row links to (`fanout_id`,
    /// `seq`, `message`, `recording`) and what a digest counts (`count`,
    /// `cost_usd`, `winner`).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub data: Map<String, Value>,
}

impl Row {
    /// The row with its free text scrubbed again (see [`scrub`]): what
    /// another hub sent is held to this hub's rule, whatever its version.
    #[must_use]
    pub fn scrubbed(mut self) -> Self {
        let again = |text: &mut Option<String>| {
            if let Some(t) = text {
                *t = scrub(t);
            }
        };
        again(&mut self.session);
        again(&mut self.session_kind);
        again(&mut self.workspace);
        self.text = scrub(&self.text);
        for value in self.data.values_mut() {
            match value {
                Value::String(s) => *s = scrub(s),
                Value::Number(_) | Value::Bool(_) | Value::Null => {}
                // Facts are scalars; anything else is not this hub's.
                other => *other = Value::Null,
            }
        }
        self
    }
}

/// A row before the journal numbers and dates it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub kind: String,
    pub session_id: Option<String>,
    pub session: Option<String>,
    pub session_kind: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace: Option<String>,
    pub text: String,
    pub data: Map<String, Value>,
}

impl Draft {
    pub fn new(kind: &str, text: impl Into<String>) -> Self {
        Self {
            kind: kind.to_owned(),
            text: scrub(&text.into()),
            ..Self::default()
        }
    }

    /// The session (and its workspace) a plugin event's `session` object
    /// names (`null` and other shapes leave them unset).
    pub fn about(mut self, session: &Value) -> Self {
        let field = |key: &str| session.get(key).and_then(Value::as_str);
        if session.is_object() {
            // Ids are the hub's own (never scrubbed); names are typed.
            self.session_id = field("id").map(str::to_owned);
            self.session = field("name").map(scrub);
            self.session_kind = field("kind").map(scrub);
            self.workspace_id = field("workspace_id").map(str::to_owned);
            self.workspace = field("workspace").map(scrub);
        }
        self
    }

    /// The workspace, for a row about no session.
    pub fn in_workspace(mut self, id: Option<&str>, name: Option<&str>) -> Self {
        self.workspace_id = id.map(str::to_owned);
        self.workspace = name.map(scrub);
        self
    }

    /// Set fact `key` to one of the hub's own ids (kept as is).
    pub fn with_id(mut self, key: &str, id: Option<&str>) -> Self {
        if let Some(id) = id {
            self.data
                .insert(key.to_owned(), Value::String(id.to_owned()));
        }
        self
    }

    /// Set fact `key` (strings are scrubbed).
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        let value = match value.into() {
            Value::String(s) => Value::String(scrub(&s)),
            Value::Null => return self,
            other => other,
        };
        self.data.insert(key.to_owned(), value);
        self
    }

    pub(crate) fn into_row(self, id: u64, at: DateTime<Utc>) -> Row {
        Row {
            id,
            at,
            kind: self.kind,
            session_id: self.session_id,
            session: self.session,
            session_kind: self.session_kind,
            workspace_id: self.workspace_id,
            workspace: self.workspace,
            text: self.text,
            data: self.data,
        }
    }
}

/// Which rows a page or digest covers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    /// Rows at or after this.
    pub since: Option<DateTime<Utc>>,
    /// Rows before this.
    pub until: Option<DateTime<Utc>>,
    /// The workspace: its id, an id prefix (4+ characters), or its name
    /// (see [`selects`]; the API resolves a live one to its id first).
    pub workspace: Option<String>,
    /// The session, the same way.
    pub session: Option<String>,
    /// Kinds (`review` covers every `review.*`); empty: every kind.
    pub kinds: Vec<String>,
}

/// Whether `selector` names the thing with `id` and `name`: the id, a
/// prefix of it of at least 4 characters, or the name. Rows outlive what
/// they name, so a removed session (or a machine's workspace) is still
/// picked by its name.
pub fn selects(selector: &str, id: Option<&str>, name: Option<&str>) -> bool {
    id == Some(selector)
        || name == Some(selector)
        || (selector.len() >= 4 && id.is_some_and(|id| id.starts_with(selector)))
}

/// Whether `kind` names row kinds: one ([`crate::plugins::EVENTS`],
/// [`EXTRA_KINDS`]) or a family (`review` for every `review.*`).
pub fn known_kind(kind: &str) -> bool {
    crate::plugins::EVENTS.iter().chain(EXTRA_KINDS).any(|k| {
        *k == kind
            || k.strip_prefix(kind)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

impl Filter {
    pub fn matches(&self, row: &Row) -> bool {
        self.since.is_none_or(|since| row.at >= since)
            && self.until.is_none_or(|until| row.at < until)
            && self
                .workspace
                .as_deref()
                .is_none_or(|w| selects(w, row.workspace_id.as_deref(), row.workspace.as_deref()))
            && self
                .session
                .as_deref()
                .is_none_or(|s| selects(s, row.session_id.as_deref(), row.session.as_deref()))
            && (self.kinds.is_empty()
                || self.kinds.iter().any(|k| {
                    row.kind == *k
                        || row
                            .kind
                            .strip_prefix(k.as_str())
                            .is_some_and(|rest| rest.starts_with('.'))
                }))
    }
}

/// A point in time as the API and CLI take it: RFC 3339, unix
/// milliseconds, or an age before `now` (`90s`, `30m`, `2h`, `7d`, `1w`).
pub fn parse_time(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let raw = raw.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(raw) {
        return Ok(at.with_timezone(&Utc));
    }
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
        return raw
            .parse::<i64>()
            .ok()
            .and_then(DateTime::from_timestamp_millis)
            .ok_or_else(|| format!("{raw}: not a time"));
    }
    let age = parse_age(raw)?;
    Ok(now - age)
}

/// An age: `90s`, `30m`, `1h30m`, `7d`, `1w` (any case; at most 366
/// days). A plain number is not one (it is unix ms to [`parse_time`]).
pub fn parse_age(raw: &str) -> Result<Span, String> {
    let bad = || format!("{raw}: want an RFC 3339 time, unix ms, or an age like 30m, 1h30m, 7d");
    if raw.trim().bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let seconds = crate::util::parse_duration(raw).map_err(|_| bad())?;
    if seconds > 366 * 86_400 {
        return Err(bad());
    }
    Ok(Span::seconds(i64::try_from(seconds).map_err(|_| bad())?))
}

/// The paging cursor across this hub and its machines: where each source
/// left off (`local:120,laptop:88`; id 0 = that source has no more).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor(pub Vec<(String, u64)>);

/// The cursor's name for this hub (machines can never be named `local`).
pub const LOCAL: &str = "local";

impl Cursor {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        for part in raw.split(',').filter(|p| !p.is_empty()) {
            let (source, id) = part
                .rsplit_once(':')
                .ok_or_else(|| format!("bad cursor part {part:?}"))?;
            let id = id
                .parse::<u64>()
                .map_err(|_| format!("bad cursor part {part:?}"))?;
            if source.is_empty() || source.len() > 64 {
                return Err(format!("bad cursor part {part:?}"));
            }
            parts.push((source.to_owned(), id));
        }
        Ok(Self(parts))
    }

    /// Rows of `source` before this id (`None`: from the newest).
    pub fn before(&self, source: &str) -> Option<u64> {
        self.0.iter().find(|(s, _)| s == source).map(|(_, id)| *id)
    }

    pub fn set(&mut self, source: &str, id: u64) {
        match self.0.iter_mut().find(|(s, _)| s == source) {
            Some(part) => part.1 = id,
            None => self.0.push((source.to_owned(), id)),
        }
    }

    /// Every source is spent.
    pub fn exhausted(&self) -> bool {
        self.0.iter().all(|(_, id)| *id == 0)
    }

    pub fn encode(&self) -> String {
        self.0
            .iter()
            .map(|(s, id)| format!("{s}:{id}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// `text` fit for a row: one line, at most [`TEXT_CAP`] characters, and
/// every token-shaped word (API keys, bearer tokens, long random strings)
/// replaced by `[redacted]`.
pub fn scrub(text: &str) -> String {
    let line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out = Vec::new();
    let mut bearer = false;
    for word in line.split(' ') {
        if bearer && !word.is_empty() {
            out.push("[redacted]".to_owned());
            bearer = false;
            continue;
        }
        bearer = word.eq_ignore_ascii_case("bearer") || word.eq_ignore_ascii_case("token");
        out.push(if secret_like(word) {
            "[redacted]".to_owned()
        } else {
            word.to_owned()
        });
    }
    let joined = out.join(" ");
    let trimmed = joined.trim();
    if trimmed.chars().count() <= TEXT_CAP {
        return trimmed.to_owned();
    }
    let mut cut: String = trimmed.chars().take(TEXT_CAP - 1).collect();
    cut.push('…');
    cut
}

/// `text` with the credentials [`scrub`] finds replaced by `[redacted]`,
/// kept as it was otherwise (lines, spacing, length): for text handed on
/// whole, like a CI log pasted into an agent. Also the value after a
/// secret's name (`GITHUB_TOKEN=...`, `password: ...`, `Bearer ...`) when
/// it looks like one (8+ characters, not all letters: `Unexpected token
/// '}'` stays), and the password in a URL (`https://user:pw@host`).
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        redact_line(line, &mut out);
    }
    out
}

fn redact_line(line: &str, out: &mut String) {
    // The word after one of these is redacted when it looks like a value.
    let mut after_name = false;
    let mut rest = line;
    while !rest.is_empty() {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(word_end);
        let gap_end = tail
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(tail.len());
        let (gap, next) = tail.split_at(gap_end);
        if word.is_empty() {
            out.push_str(gap);
            rest = next;
            continue;
        }
        let value_like = |w: &str| {
            let core = w.trim_matches(|c: char| "\"'`,;()[]{}<>".contains(c));
            core.len() >= 8 && !core.chars().all(char::is_alphabetic)
        };
        if let Some((name, value)) = word.split_once('=')
            && !value.is_empty()
            && (secret_like(value) || (secret_name(name) && value_like(value)))
        {
            out.push_str(name);
            out.push_str("=[redacted]");
            after_name = false;
        } else if secret_like(word) || (after_name && value_like(word)) {
            out.push_str("[redacted]");
            after_name = false;
        } else if let Some(cut) = url_password(word) {
            out.push_str(&cut);
            after_name = false;
        } else {
            let bare = word.trim_end_matches(':');
            // `--password VALUE`, `--client-secret VALUE`.
            let flag = word.starts_with('-')
                && word.len() > 2
                && secret_name(word.trim_start_matches('-'));
            after_name = word.eq_ignore_ascii_case("bearer")
                || word.eq_ignore_ascii_case("token")
                || flag
                || (word.ends_with(':') && secret_name(bare));
            out.push_str(word);
        }
        out.push_str(gap);
        rest = next;
    }
}

/// A variable or field name that holds a secret (`GH_TOKEN`,
/// `db_password`, `Authorization`).
fn secret_name(name: &str) -> bool {
    let upper = name
        .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|s| upper.contains(s))
}

/// `word` with the password of a URL's `user:password@` cut, when it has
/// one.
fn url_password(word: &str) -> Option<String> {
    let scheme = word.find("://")? + 3;
    let host_end = word[scheme..]
        .find(['/', '?', '#'])
        .map_or(word.len(), |i| scheme + i);
    let at = word[scheme..host_end].rfind('@')? + scheme;
    let colon = word[scheme..at].find(':')? + scheme;
    if colon + 1 >= at {
        return None;
    }
    Some(format!("{}[redacted]{}", &word[..=colon], &word[at..]))
}

/// The shape of an AWS secret access key: 40 characters of base64
/// (letters of both cases, a digit or `+`, a few `/` at most). A git
/// commit (40 hex, one case) or a path does not have it.
fn aws_secret_like(core: &str) -> bool {
    core.len() == 40
        && core
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '+')
        && core.chars().any(|c| c.is_ascii_uppercase())
        && core.chars().any(|c| c.is_ascii_lowercase())
        && core.chars().any(|c| c.is_ascii_digit() || c == '+')
        && core.chars().filter(|c| *c == '/').count() <= 4
        && !core.starts_with('/')
}

/// A word that looks like a credential: a known key prefix, or a long run
/// of letters and digits mixed (what a random token is, and a branch or
/// name is not).
fn secret_like(word: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_",
        "pk_",
        "rk_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "glpat-",
        "AKIA",
        "ASIA",
        "AIza",
        "eyJ",
    ];
    let core = word.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || "-_+/=.".contains(c)));
    // A flywheel run id (`run-` + 32 lowercase hex) names a run; rows link
    // to it.
    if core.strip_prefix("run-").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }) {
        return false;
    }
    if core.len() >= 16 && PREFIXES.iter().any(|p| core.starts_with(p)) {
        return true;
    }
    // Long unbroken hex is a key or a digest (the hub's own token is 64
    // hex); shorter (a git commit's 40, the hub's ids) names things.
    if core.len() > 40 && core.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    if aws_secret_like(core) {
        return true;
    }
    if core.len() < 24
        || core
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c) || c == '-' || c == '/')
    {
        return false;
    }
    let tokenish = core
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_+/=".contains(c));
    let digits = core.chars().filter(char::is_ascii_digit).count();
    let letters = core.chars().filter(char::is_ascii_alphabetic).count();
    let separators = core.chars().filter(|c| *c == '-' || *c == '/').count();
    // A long word of mixed letters and digits with few separators (a
    // branch like `feature/fix-the-thing-2` has many).
    tokenish && digits >= 4 && letters >= 4 && separators * 8 < core.len()
}
