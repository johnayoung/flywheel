//! Detection manifests: per-kind TOML rules read against the live bottom
//! of a session's rendered screen (the server's emulator, never a client's
//! scrolled viewport), each naming the state the screen shows: `working`,
//! `blocked`, `idle`, or `hold` (the screen cannot be read now -- keep the
//! current status). Herdr's model.
//!
//! * Bundled manifests (`crates/flywheel-hub/detect/<kind>.toml`) are
//!   embedded in the binary; `<state-dir>/agent-detection/<kind>.toml`
//!   replaces one wholesale. Overrides are validated at load: an invalid
//!   one is reported (hub log, `fw hub explain`) and the bundled manifest
//!   stays in force. Changes are picked up within a second, no restart.
//! * Rules are tried in file order; the first whose every condition holds
//!   names the state. No match is no verdict -- never `blocked`: a
//!   `blocked` rule must match a known approval or question UI.
//! * Cost is bounded: patterns are compiled once per load (size-limited
//!   linear-time regexes), a rule reads at most [`MAX_REGION_LINES`]
//!   lines of at most [`MAX_LINE_CHARS`] characters, and manifests are
//!   capped in rules and patterns.
//!
//! What a verdict does to a session's status (hooks win; the screen fills
//! the gaps) is [`tracker`]'s business; `fw hub explain` is [`explain`].

use crate::agent::SessionKind;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

/// What a screen verdict does to a session's status.
#[path = "detect/tracker.rs"]
pub mod tracker;

/// `fw hub explain` and `GET /api/sessions/{id}/explain`.
#[path = "detect/explain.rs"]
pub mod explain;

/// The override directory inside the state dir.
pub const OVERRIDE_DIR: &str = "agent-detection";
/// Largest manifest file read.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Most rules in one manifest.
pub const MAX_RULES: usize = 64;
/// Most `[[rule.when]]` clauses in one rule.
pub const MAX_CLAUSES: usize = 8;
/// Most patterns in one list (`all`, `any`, `none`, `line`, `not_line`).
pub const MAX_PATTERNS: usize = 16;
/// Longest pattern (characters).
pub const MAX_PATTERN_CHARS: usize = 400;
/// Most lines a clause reads (`lines`).
pub const MAX_REGION_LINES: usize = 100;
/// Lines a clause reads when it names none.
pub const DEFAULT_REGION_LINES: usize = 30;
/// Characters of each line a rule sees (wider rows are cut).
pub const MAX_LINE_CHARS: usize = 512;
/// Longest blocked detail (characters).
pub const MAX_DETAIL_CHARS: usize = 160;
/// Compiled size cap per regex (bytes of program).
const REGEX_SIZE_LIMIT: usize = 256 * 1024;
/// Least time between two looks at the override directory.
const RECHECK_EVERY: Duration = Duration::from_secs(1);

const BUNDLED_CLAUDE: &str = include_str!("../detect/claude.toml");
const BUNDLED_CODEX: &str = include_str!("../detect/codex.toml");
const BUNDLED_SHELL: &str = include_str!("../detect/shell.toml");

/// The manifest file name for a session kind.
pub fn file_name(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Claude => "claude.toml",
        SessionKind::Codex => "codex.toml",
        SessionKind::Shell => "shell.toml",
    }
}

fn bundled_text(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Claude => BUNDLED_CLAUDE,
        SessionKind::Codex => BUNDLED_CODEX,
        SessionKind::Shell => BUNDLED_SHELL,
    }
}

/// The state a rule names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenState {
    Working,
    Blocked,
    Idle,
    /// The screen cannot be read now (a transcript viewer over the turn):
    /// keep the current status and restart any settle timer.
    Hold,
}

impl ScreenState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
            Self::Hold => "hold",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "idle" => Self::Idle,
            "hold" => Self::Hold,
            _ => return None,
        })
    }
}

/// Where a manifest came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum Origin {
    /// Embedded in this binary.
    Bundled,
    /// `<state-dir>/agent-detection/<kind>.toml`.
    Override { path: PathBuf },
}

// --- the file format ---------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    kind: String,
    version: u32,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default, rename = "rule")]
    rules: Vec<RawRule>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClause {
    lines: Option<usize>,
    #[serde(default)]
    all: Vec<String>,
    #[serde(default)]
    any: Vec<String>,
    #[serde(default)]
    none: Vec<String>,
    #[serde(default)]
    line: Vec<String>,
    #[serde(default)]
    not_line: Vec<String>,
}

impl RawClause {
    fn is_empty(&self) -> bool {
        self.all.is_empty()
            && self.any.is_empty()
            && self.none.is_empty()
            && self.line.is_empty()
            && self.not_line.is_empty()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    state: String,
    detail: Option<String>,
    detail_line: Option<OneOrMany>,
    lines: Option<usize>,
    #[serde(default)]
    all: Vec<String>,
    #[serde(default)]
    any: Vec<String>,
    #[serde(default)]
    none: Vec<String>,
    #[serde(default)]
    line: Vec<String>,
    #[serde(default)]
    not_line: Vec<String>,
    #[serde(default)]
    when: Vec<RawClause>,
}

/// `detail_line = "re"` or `detail_line = ["re", "re"]`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
        }
    }
}

// --- compiled ----------------------------------------------------------------

#[derive(Debug, Clone)]
struct Pattern {
    source: String,
    regex: Regex,
}

#[derive(Debug, Clone)]
struct Clause {
    lines: usize,
    /// Lowercased substrings.
    all: Vec<String>,
    any: Vec<String>,
    none: Vec<String>,
    line: Vec<Pattern>,
    not_line: Vec<Pattern>,
}

#[derive(Debug, Clone)]
struct Rule {
    id: String,
    state: ScreenState,
    detail: Option<String>,
    /// Read in turn down the screen, each below the last one's line; the
    /// pieces found are joined into the detail.
    detail_line: Vec<Pattern>,
    clauses: Vec<Clause>,
}

/// One kind's rules, compiled.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub kind: SessionKind,
    pub version: u32,
    pub origin: Origin,
    rules: Vec<Rule>,
}

/// A manifest that failed to load.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ManifestError(pub String);

fn compile(source: &str, what: &str) -> Result<Pattern, ManifestError> {
    if source.chars().count() > MAX_PATTERN_CHARS {
        return Err(ManifestError(format!(
            "{what}: pattern longer than {MAX_PATTERN_CHARS} characters"
        )));
    }
    let regex = RegexBuilder::new(source)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| ManifestError(format!("{what}: bad regex {source:?}: {e}")))?;
    Ok(Pattern {
        source: source.to_owned(),
        regex,
    })
}

fn substrings(list: Vec<String>, what: &str) -> Result<Vec<String>, ManifestError> {
    check_count(list.len(), what)?;
    list.into_iter()
        .map(|s| {
            if s.trim().is_empty() {
                return Err(ManifestError(format!("{what}: empty string")));
            }
            if s.chars().count() > MAX_PATTERN_CHARS {
                return Err(ManifestError(format!(
                    "{what}: string longer than {MAX_PATTERN_CHARS} characters"
                )));
            }
            Ok(s.to_lowercase())
        })
        .collect()
}

fn regexes(list: Vec<String>, what: &str) -> Result<Vec<Pattern>, ManifestError> {
    check_count(list.len(), what)?;
    list.iter().map(|s| compile(s, what)).collect()
}

fn check_count(n: usize, what: &str) -> Result<(), ManifestError> {
    if n > MAX_PATTERNS {
        return Err(ManifestError(format!(
            "{what}: more than {MAX_PATTERNS} patterns"
        )));
    }
    Ok(())
}

fn clause(raw: RawClause, default_lines: usize, what: &str) -> Result<Clause, ManifestError> {
    let lines = raw.lines.unwrap_or(default_lines);
    if !(1..=MAX_REGION_LINES).contains(&lines) {
        return Err(ManifestError(format!(
            "{what}: lines must be 1..={MAX_REGION_LINES}"
        )));
    }
    Ok(Clause {
        lines,
        all: substrings(raw.all, &format!("{what}: all"))?,
        any: substrings(raw.any, &format!("{what}: any"))?,
        none: substrings(raw.none, &format!("{what}: none"))?,
        line: regexes(raw.line, &format!("{what}: line"))?,
        not_line: regexes(raw.not_line, &format!("{what}: not_line"))?,
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl Manifest {
    /// Parse and validate `text` as `kind`'s manifest.
    pub fn parse(text: &str, kind: SessionKind, origin: Origin) -> Result<Self, ManifestError> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|e| ManifestError(e.to_string().trim().to_owned()))?;
        if raw.kind != kind.as_str() {
            return Err(ManifestError(format!(
                "kind = {:?}, but this file is for {:?}",
                raw.kind,
                kind.as_str()
            )));
        }
        if raw.rules.is_empty() {
            return Err(ManifestError("no [[rule]]s".into()));
        }
        if raw.rules.len() > MAX_RULES {
            return Err(ManifestError(format!("more than {MAX_RULES} rules")));
        }
        let mut rules: Vec<Rule> = Vec::with_capacity(raw.rules.len());
        for (index, r) in raw.rules.into_iter().enumerate() {
            let what = format!("rule {} ({:?})", index + 1, r.id);
            if !valid_id(&r.id) {
                return Err(ManifestError(format!(
                    "{what}: id must be 1-64 of [a-z0-9_-]"
                )));
            }
            if rules.iter().any(|seen| seen.id == r.id) {
                return Err(ManifestError(format!("{what}: duplicate id")));
            }
            let state = ScreenState::parse(&r.state).ok_or_else(|| {
                ManifestError(format!(
                    "{what}: state must be working, blocked, idle, or hold (not {:?})",
                    r.state
                ))
            })?;
            if state != ScreenState::Blocked && (r.detail.is_some() || r.detail_line.is_some()) {
                return Err(ManifestError(format!(
                    "{what}: detail and detail_line are for blocked rules"
                )));
            }
            if let Some(detail) = &r.detail
                && (detail.trim().is_empty() || detail.chars().count() > MAX_DETAIL_CHARS)
            {
                return Err(ManifestError(format!(
                    "{what}: detail must be 1..={MAX_DETAIL_CHARS} characters"
                )));
            }
            let default_lines = r.lines.unwrap_or(DEFAULT_REGION_LINES);
            let own = RawClause {
                lines: r.lines,
                all: r.all,
                any: r.any,
                none: r.none,
                line: r.line,
                not_line: r.not_line,
            };
            if r.when.len() > MAX_CLAUSES {
                return Err(ManifestError(format!(
                    "{what}: more than {MAX_CLAUSES} [[rule.when]] clauses"
                )));
            }
            let mut clauses = Vec::new();
            if !own.is_empty() {
                clauses.push(clause(own, default_lines, &what)?);
            }
            for (n, when) in r.when.into_iter().enumerate() {
                if when.is_empty() {
                    return Err(ManifestError(format!(
                        "{what}: when {}: no conditions",
                        n + 1
                    )));
                }
                clauses.push(clause(
                    when,
                    default_lines,
                    &format!("{what}: when {}", n + 1),
                )?);
            }
            if clauses.is_empty() {
                return Err(ManifestError(format!("{what}: no conditions")));
            }
            let detail_line = regexes(
                r.detail_line.map(OneOrMany::into_vec).unwrap_or_default(),
                &format!("{what}: detail_line"),
            )?;
            rules.push(Rule {
                id: r.id,
                state,
                detail: r.detail,
                detail_line,
                clauses,
            });
        }
        Ok(Self {
            kind,
            version: raw.version,
            origin,
            rules,
        })
    }

    /// The manifest embedded in this binary for `kind`.
    pub fn bundled(kind: SessionKind) -> Arc<Self> {
        static BUNDLED: OnceLock<HashMap<SessionKind, Arc<Manifest>>> = OnceLock::new();
        let all = BUNDLED.get_or_init(|| {
            SessionKind::ALL
                .into_iter()
                .map(|kind| {
                    let manifest = Manifest::parse(bundled_text(kind), kind, Origin::Bundled)
                        // Every bundled manifest is parsed by the unit tests.
                        .expect("bundled detection manifest is valid");
                    (kind, Arc::new(manifest))
                })
                .collect()
        });
        Arc::clone(&all[&kind])
    }

    pub fn file(&self) -> &'static str {
        file_name(self.kind)
    }

    /// The file as a status source names it: `codex.toml` (bundled) or
    /// `agent-detection/codex.toml` (the state dir's override).
    pub fn source_file(&self) -> String {
        match self.origin {
            Origin::Bundled => self.file().to_owned(),
            Origin::Override { .. } => format!("{OVERRIDE_DIR}/{}", self.file()),
        }
    }

    /// The rule ids, in evaluation order.
    pub fn rule_ids(&self) -> impl Iterator<Item = &str> {
        self.rules.iter().map(|r| r.id.as_str())
    }

    /// The state the screen `rows` (top to bottom) show, by the first rule
    /// that matches.
    pub fn evaluate(&self, rows: &[String]) -> Verdict {
        let screen = Screen::new(rows);
        for rule in &self.rules {
            if let Ok(evidence) = rule.check(&screen) {
                return rule.verdict(&screen, evidence);
            }
        }
        Verdict::default()
    }

    /// [`Manifest::evaluate`] plus why every rule up to the match did or
    /// did not match.
    pub fn trace(&self, rows: &[String]) -> (Verdict, Vec<RuleTrace>) {
        let screen = Screen::new(rows);
        let mut traces = Vec::new();
        let mut verdict = None;
        for rule in &self.rules {
            let result = if verdict.is_some() {
                RuleTrace {
                    id: rule.id.clone(),
                    state: rule.state,
                    matched: false,
                    reason: Some("not tried: an earlier rule matched".into()),
                }
            } else {
                match rule.check(&screen) {
                    Ok(evidence) => {
                        verdict = Some(rule.verdict(&screen, evidence));
                        RuleTrace {
                            id: rule.id.clone(),
                            state: rule.state,
                            matched: true,
                            reason: None,
                        }
                    }
                    Err(reason) => RuleTrace {
                        id: rule.id.clone(),
                        state: rule.state,
                        matched: false,
                        reason: Some(reason),
                    },
                }
            };
            traces.push(result);
        }
        (verdict.unwrap_or_default(), traces)
    }
}

/// One screen line a rule matched on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evidence {
    /// 1-based screen row.
    pub row: usize,
    pub text: String,
    /// What matched there (a string or regex from the rule).
    pub matched: String,
}

/// What a manifest read off a screen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// `None`: no rule matched.
    pub state: Option<ScreenState>,
    pub rule: Option<String>,
    /// For blocked: what the agent waits on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub evidence: Vec<Evidence>,
}

/// Why one rule did or did not match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleTrace {
    pub id: String,
    pub state: ScreenState,
    pub matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The non-empty rows at the bottom of a screen, cut and lowercased once.
struct Screen {
    /// `(1-based row, text, lowercase)`, bottom-most last.
    lines: Vec<(usize, String, String)>,
}

impl Screen {
    fn new(rows: &[String]) -> Self {
        let mut lines: Vec<(usize, String, String)> = rows
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, row)| !row.trim().is_empty())
            .take(MAX_REGION_LINES)
            .map(|(i, row)| {
                let text: String = row.trim_end().chars().take(MAX_LINE_CHARS).collect();
                let lower = text.to_lowercase();
                (i + 1, text, lower)
            })
            .collect();
        lines.reverse();
        Self { lines }
    }

    /// The last `n` non-empty lines.
    fn region(&self, n: usize) -> &[(usize, String, String)] {
        &self.lines[self.lines.len().saturating_sub(n)..]
    }
}

fn quoted(list: &[String]) -> String {
    list.iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn evidence(line: &(usize, String, String), matched: &str) -> Evidence {
    Evidence {
        row: line.0,
        text: line.1.clone(),
        matched: matched.to_owned(),
    }
}

impl Clause {
    fn check(&self, screen: &Screen) -> Result<Vec<Evidence>, String> {
        let region = screen.region(self.lines);
        let within = format!("in the last {} lines", self.lines);
        let mut found = Vec::new();
        for needle in &self.all {
            match region.iter().find(|l| l.2.contains(needle.as_str())) {
                Some(line) => found.push(evidence(line, needle)),
                None => return Err(format!("no {needle:?} {within}")),
            }
        }
        if !self.any.is_empty() {
            let hit = self.any.iter().find_map(|needle| {
                region
                    .iter()
                    .find(|l| l.2.contains(needle.as_str()))
                    .map(|line| evidence(line, needle))
            });
            match hit {
                Some(e) => found.push(e),
                None => return Err(format!("none of {} {within}", quoted(&self.any))),
            }
        }
        for needle in &self.none {
            if let Some(line) = region.iter().find(|l| l.2.contains(needle.as_str())) {
                return Err(format!("row {} has {needle:?}", line.0));
            }
        }
        for pattern in &self.line {
            match region.iter().find(|l| pattern.regex.is_match(&l.1)) {
                Some(line) => found.push(evidence(line, &pattern.source)),
                None => return Err(format!("no line matches '{}' {within}", pattern.source)),
            }
        }
        for pattern in &self.not_line {
            if let Some(line) = region.iter().find(|l| pattern.regex.is_match(&l.1)) {
                return Err(format!("row {} matches '{}'", line.0, pattern.source));
            }
        }
        Ok(found)
    }
}

impl Rule {
    fn check(&self, screen: &Screen) -> Result<Vec<Evidence>, String> {
        let mut found = Vec::new();
        for (n, clause) in self.clauses.iter().enumerate() {
            match clause.check(screen) {
                Ok(e) => found.extend(e),
                Err(reason) if self.clauses.len() > 1 => {
                    return Err(format!("condition {}: {reason}", n + 1));
                }
                Err(reason) => return Err(reason),
            }
        }
        found.sort_by_key(|e| e.row);
        found.dedup_by(|a, b| a.row == b.row && a.matched == b.matched);
        Ok(found)
    }

    fn verdict(&self, screen: &Screen, evidence: Vec<Evidence>) -> Verdict {
        let region = screen.region(MAX_REGION_LINES);
        let mut from = 0;
        let mut pieces: Vec<&str> = Vec::new();
        for (n, pattern) in self.detail_line.iter().enumerate() {
            // A pattern with a group adds its capture 1; one without only
            // marks where the next one starts reading.
            let anchor = pattern.regex.captures_len() == 1;
            let hit = region[from..].iter().enumerate().find_map(|(i, l)| {
                let captures = pattern.regex.captures(&l.1)?;
                if anchor {
                    return Some((i, None));
                }
                let text = captures.get(1)?.as_str().trim();
                (!text.is_empty()).then_some((i, Some(text)))
            });
            match hit {
                Some((i, text)) => {
                    from += i + 1;
                    pieces.extend(text);
                }
                // The first names the detail; later ones only add to it.
                None if n == 0 => break,
                None => {}
            }
        }
        let from_line = (!pieces.is_empty()).then(|| {
            let joined = pieces.join(" ");
            let mut detail: String = joined.chars().take(MAX_DETAIL_CHARS).collect();
            if joined.chars().count() > MAX_DETAIL_CHARS {
                detail.pop();
                detail.push('\u{2026}');
            }
            detail
        });
        Verdict {
            state: Some(self.state),
            rule: Some(self.id.clone()),
            detail: from_line.or_else(|| self.detail.clone()),
            evidence,
        }
    }
}

// --- loading, overrides, hot reload -----------------------------------------

/// A manifest file that is not in force, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub path: PathBuf,
    pub error: String,
}

/// A file's identity for change detection.
type Stamp = (Option<SystemTime>, u64);

#[derive(Debug, Default)]
struct Loaded {
    active: HashMap<SessionKind, Arc<Manifest>>,
    problems: Vec<Problem>,
    /// What the override directory held at the last load.
    seen: BTreeMap<String, Stamp>,
    checked: Option<Instant>,
}

/// The manifests in force: bundled ones, replaced by valid overrides in
/// `<state-dir>/agent-detection/`, re-read when that directory changes.
#[derive(Debug)]
pub struct Detection {
    dir: Option<PathBuf>,
    /// Log loads and problems to stderr (the hub log).
    log: bool,
    loaded: Mutex<Loaded>,
}

impl Detection {
    /// The bundled manifests only.
    pub fn bundled() -> Self {
        Self {
            dir: None,
            log: false,
            loaded: Mutex::new(Loaded::default()),
        }
    }

    /// Bundled manifests with `<state_dir>/agent-detection/` overrides.
    /// Problems are logged (stderr: the hub log) as found.
    pub fn with_overrides(state_dir: &Path) -> Self {
        Self::load(state_dir, true)
    }

    /// [`Detection::with_overrides`] that logs nothing (the CLI shows
    /// problems itself).
    pub fn with_overrides_quiet(state_dir: &Path) -> Self {
        Self::load(state_dir, false)
    }

    fn load(state_dir: &Path, log: bool) -> Self {
        let detection = Self {
            dir: Some(state_dir.join(OVERRIDE_DIR)),
            log,
            loaded: Mutex::new(Loaded::default()),
        };
        detection.refresh(true);
        detection
    }

    /// The override directory, if any.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// `kind`'s manifest in force (re-reading changed overrides at most
    /// once a second).
    pub fn manifest(&self, kind: SessionKind) -> Arc<Manifest> {
        self.refresh(false);
        let loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        loaded
            .active
            .get(&kind)
            .cloned()
            .unwrap_or_else(|| Manifest::bundled(kind))
    }

    /// Override files not in force (invalid, or for no known kind).
    pub fn problems(&self) -> Vec<Problem> {
        self.refresh(false);
        self.loaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .problems
            .clone()
    }

    fn refresh(&self, force: bool) {
        let Some(dir) = &self.dir else {
            return;
        };
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if !force
            && loaded
                .checked
                .is_some_and(|at| now.duration_since(at) < RECHECK_EVERY)
        {
            return;
        }
        loaded.checked = Some(now);
        let seen = scan(dir);
        if !force && seen == loaded.seen {
            return;
        }
        let (active, problems) = load_overrides(dir, &seen);
        if self.log {
            for problem in &problems {
                eprintln!(
                    "fw hub: detection manifest {} not used: {}",
                    problem.path.display(),
                    problem.error
                );
            }
            for manifest in active.values() {
                eprintln!(
                    "fw hub: detection manifest {} (version {}) replaces the bundled {}",
                    dir.join(manifest.file()).display(),
                    manifest.version,
                    manifest.file()
                );
            }
            for kind in loaded.active.keys() {
                if !active.contains_key(kind) {
                    eprintln!(
                        "fw hub: detection manifest {}: back to the bundled one",
                        file_name(*kind)
                    );
                }
            }
        }
        loaded.active = active;
        loaded.problems = problems;
        loaded.seen = seen;
    }
}

/// The `*.toml` files in `dir` and their stamps (empty when it is
/// missing).
fn scan(dir: &Path) -> BTreeMap<String, Stamp> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.ends_with(".toml") {
                return None;
            }
            let meta = std::fs::metadata(entry.path()).ok()?;
            meta.is_file()
                .then(|| (name, (meta.modified().ok(), meta.len())))
        })
        .collect()
}

fn load_overrides(
    dir: &Path,
    seen: &BTreeMap<String, Stamp>,
) -> (HashMap<SessionKind, Arc<Manifest>>, Vec<Problem>) {
    let mut active = HashMap::new();
    let mut problems = Vec::new();
    for (name, (_, len)) in seen {
        let path = dir.join(name);
        let Some(kind) = SessionKind::ALL.into_iter().find(|k| file_name(*k) == name) else {
            problems.push(Problem {
                path,
                error: "no such kind (claude.toml, codex.toml, or shell.toml)".into(),
            });
            continue;
        };
        if *len > MAX_MANIFEST_BYTES {
            problems.push(Problem {
                path,
                error: format!("larger than {MAX_MANIFEST_BYTES} bytes"),
            });
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                problems.push(Problem {
                    path,
                    error: e.to_string(),
                });
                continue;
            }
        };
        match Manifest::parse(&text, kind, Origin::Override { path: path.clone() }) {
            Ok(manifest) => {
                active.insert(kind, Arc::new(manifest));
            }
            Err(e) => problems.push(Problem {
                path,
                error: format!("{e} (the bundled {name} is used)"),
            }),
        }
    }
    (active, problems)
}

#[cfg(test)]
#[path = "detect/tests.rs"]
mod tests;
