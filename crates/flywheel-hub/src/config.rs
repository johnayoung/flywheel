//! `<state-dir>/hub.toml`: the hub's server-side settings in one file.
//!
//! Everything `fw hub serve` takes as a flag can be set here too; a flag
//! given on the command line wins over the file, the file over the
//! built-in default (`flag > file > default`, per key; a list flag given
//! once replaces the file's whole list). The file is validated as a whole
//! (unknown keys and bad values are errors that name the key), re-read
//! within a second of a change by the running hub, and an invalid edit
//! keeps the last good settings in force while the UI and
//! `fw hub config check` show what is wrong.
//!
//! Keys apply either live (new sessions, worktrees, imports, agent
//! launches, plugins see them at once) or at the next start (the listener,
//! origins, restore); [`Entry::apply`] says which, and the running hub
//! lists file edits still waiting for a restart.
//!
//! Push notifications keep their own 0600 file (`notify.toml`: it holds
//! the topic and token, which are secrets) and detection overrides their
//! own directory (`agent-detection/`); [`Effective`] names both so
//! `config show` is the one place to look.

use crate::agent::{AgentLaunch, SessionKind};
use crate::util::file_identity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, SystemTime};

/// The settings file's name in the state dir.
pub const CONFIG_FILE: &str = "hub.toml";
/// Largest settings file read.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;
/// How often the running hub looks at the file.
pub const POLL: Duration = Duration::from_secs(1);
/// Default listen port.
pub const DEFAULT_PORT: u16 = crate::cli::DEFAULT_PORT;
/// Largest `sessions.scrollback_lines` (the emulator caps cells anyway).
pub const MAX_SCROLLBACK_LINES: usize = 1_000_000;
/// Default and bounds of `plugins.max_concurrent`.
pub const DEFAULT_PLUGIN_CONCURRENCY: usize = 4;
pub const MAX_PLUGIN_CONCURRENCY: usize = 16;

/// The file as written. Every key is optional; unknown keys are refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub sessions: SessionsSection,
    #[serde(default)]
    pub worktrees: WorktreesSection,
    #[serde(default)]
    pub import: ImportSection,
    #[serde(default)]
    pub flywheel: FlywheelSection,
    #[serde(default)]
    pub agents: AgentsSection,
    #[serde(default)]
    pub plugins: PluginsSection,
    #[serde(default)]
    pub recordings: RecordingsSection,
    #[serde(default)]
    pub shares: SharesSection,
    #[serde(default)]
    pub review: ReviewSection,
    #[serde(default)]
    pub queue: QueueSection,
    /// Scheduled digests (see [`crate::digest`]).
    #[serde(default)]
    pub digest: crate::digest::DigestSection,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub public_urls: Option<Vec<String>>,
    pub allow_origins: Option<Vec<String>>,
    pub trusted_proxies: Option<Vec<String>>,
    /// `auto`, `off`, or `CERT,KEY` (built-in https, `crate::tls`).
    pub tls: Option<String>,
    /// Extra names (DNS, IP, or IP/prefix) for the local CA and its leaf.
    pub tls_names: Option<Vec<String>>,
    /// The public ones among `tls_names` the operator opted in to.
    pub tls_public_names: Option<Vec<String>>,
    /// Days a browser login lasts after its last use (each use renews).
    pub login_days: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsSection {
    pub scrollback_lines: Option<usize>,
    pub restore: Option<bool>,
    pub agent_resume: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreesSection {
    pub branch_prefix: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportSection {
    pub root: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlywheelSection {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsSection {
    pub claude: Option<ClaudeDefaults>,
    pub codex: Option<CodexDefaults>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDefaults {
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodexDefaults {
    pub model: Option<String>,
    pub approval: Option<String>,
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsSection {
    pub enabled: Option<bool>,
    pub max_concurrent: Option<usize>,
}

/// Session recordings' caps: sizes as bytes or with a unit (`"64MiB"`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingsSection {
    pub part_cap: Option<SizeSetting>,
    pub dir_cap: Option<SizeSetting>,
}

/// New share links' expiry: durations (`"1h"`, `"7d"`) or seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharesSection {
    pub default_expiry: Option<DurationSetting>,
    pub max_expiry: Option<DurationSetting>,
}

/// "Ask for a review": which agent kind reviews a session's work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSection {
    /// `other` (default), `same`, `claude`, or `codex`.
    pub reviewer: Option<String>,
}

/// Prompt queues (see [`crate::queue`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueueSection {
    /// How long a session must go without typed input before its queue
    /// delivers: seconds, or `"8s"` / `"1m"`.
    pub typing_quiet: Option<DurationSetting>,
}

/// Which agent kind "ask for a review" launches, relative to the author's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerChoice {
    /// The other kind (Claude work -> a Codex reviewer, and back).
    #[default]
    Other,
    /// The author's own kind.
    Same,
    Claude,
    Codex,
}

impl ReviewerChoice {
    pub const ALL: [Self; 4] = [Self::Other, Self::Same, Self::Claude, Self::Codex];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Other => "other",
            Self::Same => "same",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == raw)
            .ok_or_else(|| format!("{raw:?} is not one of other, same, claude, codex"))
    }

    /// The kind preferred for reviewing `author`'s work (a shell's work
    /// counts as Claude's for `same`, and gets Claude under `other`'s
    /// fallback order: Codex first only when the author is Claude).
    pub fn preferred(self, author: SessionKind) -> SessionKind {
        match (self, author) {
            (Self::Claude, _) => SessionKind::Claude,
            (Self::Codex, _) => SessionKind::Codex,
            (Self::Same, SessionKind::Codex) | (Self::Other, SessionKind::Claude) => {
                SessionKind::Codex
            }
            (Self::Same | Self::Other, _) => SessionKind::Claude,
        }
    }
}

/// A size in the file: a number of bytes, or text with a unit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SizeSetting {
    Bytes(u64),
    Text(String),
}

impl SizeSetting {
    fn bytes(&self) -> Result<u64, String> {
        match self {
            Self::Bytes(n) => Ok(*n),
            Self::Text(text) => crate::record::parse_size(text),
        }
    }
}

/// A duration in the file: seconds, or text like `1h30m`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum DurationSetting {
    Seconds(u64),
    Text(String),
}

impl DurationSetting {
    fn duration(&self) -> Result<Duration, String> {
        match self {
            Self::Seconds(n) => crate::share::parse_ttl(&n.to_string()),
            Self::Text(text) => crate::share::parse_ttl(text),
        }
    }
}

/// What `fw hub serve` was given on its command line (`None` / empty /
/// `false`: not given, so the file or the default decides).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    pub bind: Option<IpAddr>,
    pub port: Option<u16>,
    pub public_urls: Vec<String>,
    pub allow_origins: Vec<String>,
    pub trusted_proxies: Vec<IpAddr>,
    pub tls: Option<crate::tls::TlsMode>,
    pub tls_names: Vec<crate::tls::NameSpec>,
    pub tls_public_names: Vec<crate::tls::NameSpec>,
    pub scrollback_lines: Option<usize>,
    pub no_restore: bool,
    pub no_agent_resume: bool,
    pub no_flywheel: bool,
    pub import_root: Option<PathBuf>,
    pub worktree_branch_prefix: Option<String>,
    pub record_part_cap: Option<u64>,
    pub record_dir_cap: Option<u64>,
    pub share_default_expiry: Option<Duration>,
    pub share_max_expiry: Option<Duration>,
    pub login_days: Option<u32>,
}

/// Where an effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Flag,
    File,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::File => "file",
            Self::Default => "default",
        }
    }
}

/// When a changed value takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    /// At once (what new sessions, launches, and plugin runs read).
    Live,
    /// At the next start of the hub.
    Restart,
}

/// Agent launch defaults per kind: applied to a launch that names none
/// of a field (`fw hub new`, the API, the web form left blank).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentDefaults {
    pub claude: AgentLaunch,
    pub codex: AgentLaunch,
}

impl AgentDefaults {
    /// `launch` with every field it leaves unset filled from `kind`'s
    /// defaults (shells take none).
    pub fn fill(&self, kind: SessionKind, launch: &AgentLaunch) -> AgentLaunch {
        let defaults = match kind {
            SessionKind::Claude => &self.claude,
            SessionKind::Codex => &self.codex,
            SessionKind::Shell => return launch.clone(),
        };
        let pick = |given: &Option<String>, fallback: &Option<String>| {
            given
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .or_else(|| fallback.clone())
        };
        AgentLaunch {
            model: pick(&launch.model, &defaults.model),
            permission_mode: pick(&launch.permission_mode, &defaults.permission_mode),
            approval: pick(&launch.approval, &defaults.approval),
            sandbox: pick(&launch.sandbox, &defaults.sandbox),
        }
    }
}

/// The settings the running hub can change without a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunables {
    pub scrollback_lines: usize,
    pub worktree_branch_prefix: String,
    pub import_root: Option<PathBuf>,
    pub agent_defaults: AgentDefaults,
    pub record_limits: crate::record::RecordLimits,
    pub share_policy: crate::share::SharePolicy,
    pub reviewer: ReviewerChoice,
    /// No typed input for this long before a queue delivers.
    pub queue_typing_quiet: Duration,
    /// Days a browser login lasts after its last use.
    pub login_days: u32,
}

/// Every setting resolved, with where each came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub bind: IpAddr,
    pub port: u16,
    pub public_urls: Vec<String>,
    pub allow_origins: Vec<String>,
    pub trusted_proxies: Vec<IpAddr>,
    pub tls: crate::tls::TlsMode,
    /// `tls_names`, each private or opted in (`tls_public_names`).
    pub tls_names: crate::tls::ExtraNames,
    pub scrollback_lines: usize,
    pub restore: bool,
    pub agent_resume: bool,
    pub worktree_branch_prefix: String,
    pub import_root: Option<PathBuf>,
    pub flywheel: bool,
    pub agent_defaults: AgentDefaults,
    pub plugins_enabled: bool,
    pub plugins_max_concurrent: usize,
    pub record_limits: crate::record::RecordLimits,
    pub share_policy: crate::share::SharePolicy,
    pub reviewer: ReviewerChoice,
    pub queue_typing_quiet: Duration,
    pub login_days: u32,
    pub digest: crate::digest::DigestConfig,
    sources: Vec<(&'static str, Source)>,
}

/// One row of `fw hub config show`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub key: &'static str,
    pub value: Value,
    pub source: Source,
    pub apply: Apply,
}

/// Every key with when it applies, in file order.
pub const KEYS: &[(&str, Apply)] = &[
    ("server.bind", Apply::Restart),
    ("server.port", Apply::Restart),
    ("server.public_urls", Apply::Restart),
    ("server.allow_origins", Apply::Restart),
    ("server.trusted_proxies", Apply::Restart),
    ("server.tls", Apply::Restart),
    ("server.tls_names", Apply::Restart),
    ("server.tls_public_names", Apply::Restart),
    ("server.login_days", Apply::Live),
    ("sessions.scrollback_lines", Apply::Live),
    ("sessions.restore", Apply::Restart),
    ("sessions.agent_resume", Apply::Restart),
    ("worktrees.branch_prefix", Apply::Live),
    ("import.root", Apply::Live),
    ("flywheel.enabled", Apply::Restart),
    ("agents.claude.model", Apply::Live),
    ("agents.claude.permission_mode", Apply::Live),
    ("agents.codex.model", Apply::Live),
    ("agents.codex.approval", Apply::Live),
    ("agents.codex.sandbox", Apply::Live),
    ("plugins.enabled", Apply::Live),
    ("plugins.max_concurrent", Apply::Restart),
    ("recordings.part_cap", Apply::Live),
    ("recordings.dir_cap", Apply::Live),
    ("shares.default_expiry", Apply::Live),
    ("shares.max_expiry", Apply::Live),
    ("review.reviewer", Apply::Live),
    ("queue.typing_quiet", Apply::Live),
    ("digest.times", Apply::Live),
    ("digest.timezone", Apply::Live),
    ("digest.channels", Apply::Live),
    ("digest.quiet_if_empty", Apply::Live),
];

impl Effective {
    /// Where `key` came from (`Default` for a key never resolved).
    pub fn source(&self, key: &str) -> Source {
        self.sources
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(Source::Default, |(_, s)| *s)
    }

    pub fn tunables(&self) -> Tunables {
        Tunables {
            scrollback_lines: self.scrollback_lines,
            worktree_branch_prefix: self.worktree_branch_prefix.clone(),
            import_root: self.import_root.clone(),
            agent_defaults: self.agent_defaults.clone(),
            record_limits: self.record_limits,
            share_policy: self.share_policy,
            reviewer: self.reviewer,
            queue_typing_quiet: self.queue_typing_quiet,
            login_days: self.login_days,
        }
    }

    /// The value of `key` as JSON.
    pub fn value(&self, key: &str) -> Value {
        let opt = |v: &Option<String>| v.clone().map_or(Value::Null, Value::String);
        match key {
            "server.bind" => json!(self.bind.to_string()),
            "server.port" => json!(self.port),
            "server.public_urls" => json!(self.public_urls),
            "server.allow_origins" => json!(self.allow_origins),
            "server.trusted_proxies" => json!(
                self.trusted_proxies
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            ),
            "server.tls" => json!(self.tls.to_string()),
            "server.tls_names" => json!(
                self.tls_names
                    .names()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            ),
            "server.tls_public_names" => json!(
                self.tls_names
                    .public()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            ),
            "server.login_days" => json!(self.login_days),
            "sessions.scrollback_lines" => json!(self.scrollback_lines),
            "sessions.restore" => json!(self.restore),
            "sessions.agent_resume" => json!(self.agent_resume),
            "worktrees.branch_prefix" => json!(self.worktree_branch_prefix),
            "import.root" => self
                .import_root
                .as_ref()
                .map_or(Value::Null, |p| json!(p.to_string_lossy())),
            "flywheel.enabled" => json!(self.flywheel),
            "agents.claude.model" => opt(&self.agent_defaults.claude.model),
            "agents.claude.permission_mode" => opt(&self.agent_defaults.claude.permission_mode),
            "agents.codex.model" => opt(&self.agent_defaults.codex.model),
            "agents.codex.approval" => opt(&self.agent_defaults.codex.approval),
            "agents.codex.sandbox" => opt(&self.agent_defaults.codex.sandbox),
            "plugins.enabled" => json!(self.plugins_enabled),
            "plugins.max_concurrent" => json!(self.plugins_max_concurrent),
            "recordings.part_cap" => {
                json!(crate::record::format_size(
                    self.record_limits.max_file_bytes
                ))
            }
            "recordings.dir_cap" => {
                json!(crate::record::format_size(
                    self.record_limits.max_total_bytes
                ))
            }
            "shares.default_expiry" => {
                json!(crate::share::format_ttl(self.share_policy.default_ttl))
            }
            "shares.max_expiry" => json!(crate::share::format_ttl(self.share_policy.max_ttl)),
            "review.reviewer" => json!(self.reviewer.as_str()),
            "queue.typing_quiet" => json!(crate::share::format_ttl(self.queue_typing_quiet)),
            "digest.times" => json!(self.digest.times_text()),
            "digest.timezone" => json!(self.digest.timezone.name()),
            "digest.channels" => json!(self.digest.channels.names()),
            "digest.quiet_if_empty" => json!(self.digest.quiet_if_empty),
            _ => Value::Null,
        }
    }

    /// Every key, its value, source, and when it applies.
    pub fn entries(&self) -> Vec<Entry> {
        KEYS.iter()
            .map(|(key, apply)| Entry {
                key,
                value: self.value(key),
                source: self.source(key),
                apply: *apply,
            })
            .collect()
    }

    /// Keys whose value differs from `running`'s but apply only at a
    /// restart: the file says one thing, the hub still does another.
    pub fn pending_restart(&self, running: &Self) -> Vec<&'static str> {
        KEYS.iter()
            .filter(|(key, apply)| {
                *apply == Apply::Restart && self.value(key) != running.value(key)
            })
            .map(|(key, _)| *key)
            .collect()
    }
}

/// Parse a settings file's text. Errors name the key (TOML's own message,
/// with its line and column).
pub fn parse(text: &str) -> Result<FileConfig, String> {
    toml::from_str::<FileConfig>(text).map_err(|e| e.to_string().trim_end().to_owned())
}

/// Read `<state-dir>/hub.toml`: `Ok(None)` when there is none.
pub fn read_file(path: &Path) -> Result<Option<FileConfig>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "{} is {} bytes; at most {MAX_CONFIG_BYTES} are read",
            path.display(),
            meta.len()
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(&text).map(Some)
}

/// Resolve every setting: `flags` over `file` over the defaults. Every
/// bad value is reported (`key: why`), not just the first.
pub fn resolve(
    file: &FileConfig,
    flags: &Flags,
    home: Option<&Path>,
) -> Result<Effective, Vec<String>> {
    let mut errors = Vec::new();
    let mut sources: Vec<(&'static str, Source)> = Vec::new();
    // Pick flag, else file, else default; records the source.
    macro_rules! pick {
        ($key:literal, $flag:expr, $file:expr, $default:expr) => {{
            match ($flag, $file) {
                (Some(v), _) => {
                    sources.push(($key, Source::Flag));
                    v
                }
                (None, Some(v)) => {
                    sources.push(($key, Source::File));
                    v
                }
                (None, None) => {
                    sources.push(($key, Source::Default));
                    $default
                }
            }
        }};
    }
    let file_bind =
        file.server
            .bind
            .as_deref()
            .and_then(|raw| match raw.trim().parse::<IpAddr>() {
                Ok(ip) => Some(ip),
                Err(_) => {
                    errors.push(format!(
                        "server.bind: {raw:?} is not an IP address (e.g. 127.0.0.1, ::, 0.0.0.0)"
                    ));
                    None
                }
            });
    let bind = pick!(
        "server.bind",
        flags.bind,
        file_bind,
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    );
    let port = pick!("server.port", flags.port, file.server.port, DEFAULT_PORT);
    let origins = |key: &str, list: &[String], errors: &mut Vec<String>| -> Vec<String> {
        list.iter()
            .filter_map(|raw| match crate::remote::normalize_public_url(raw) {
                Ok(origin) => Some(origin),
                Err(message) => {
                    errors.push(format!("{key}: {message}"));
                    None
                }
            })
            .collect()
    };
    let file_public = file
        .server
        .public_urls
        .as_ref()
        .map(|list| origins("server.public_urls", list, &mut errors));
    let flag_public = (!flags.public_urls.is_empty())
        .then(|| origins("--public-url", &flags.public_urls, &mut errors));
    let public_urls = pick!("server.public_urls", flag_public, file_public, Vec::new());
    let file_origins = file
        .server
        .allow_origins
        .as_ref()
        .map(|list| origins("server.allow_origins", list, &mut errors));
    let flag_origins = (!flags.allow_origins.is_empty()).then(|| flags.allow_origins.clone());
    let allow_origins = pick!(
        "server.allow_origins",
        flag_origins,
        file_origins,
        Vec::new()
    );
    let file_proxies = file.server.trusted_proxies.as_ref().map(|list| {
        list.iter()
            .filter_map(|raw| match raw.trim().parse::<IpAddr>() {
                Ok(ip) => Some(ip),
                Err(_) => {
                    errors.push(format!(
                        "server.trusted_proxies: {raw:?} is not an IP address"
                    ));
                    None
                }
            })
            .collect::<Vec<_>>()
    });
    let flag_proxies = (!flags.trusted_proxies.is_empty()).then(|| flags.trusted_proxies.clone());
    let trusted_proxies = pick!(
        "server.trusted_proxies",
        flag_proxies,
        file_proxies,
        Vec::new()
    );
    let file_tls = file.server.tls.as_deref().and_then(|raw| {
        match crate::tls::parse_mode(raw) {
            Ok(crate::tls::TlsMode::Files { cert, key }) => {
                let fix = |p: PathBuf| {
                    let p = crate::state::expand_home(&p, home);
                    p.is_absolute().then_some(p)
                };
                match (fix(cert), fix(key)) {
                    (Some(cert), Some(key)) => Some(crate::tls::TlsMode::Files { cert, key }),
                    _ => {
                        errors.push(format!(
                            "server.tls: {raw:?}: the certificate and key paths must be absolute (or ~/...)"
                        ));
                        None
                    }
                }
            }
            Ok(mode) => Some(mode),
            Err(message) => {
                errors.push(format!("server.tls: {message}"));
                None
            }
        }
    });
    let tls = pick!(
        "server.tls",
        flags.tls.clone(),
        file_tls,
        crate::tls::TlsMode::Off
    );
    let names_in = |key: &str, list: &[String], errors: &mut Vec<String>| {
        list.iter()
            .filter_map(|raw| match crate::tls::parse_name(raw) {
                Ok(name) => Some(name),
                Err(message) => {
                    errors.push(format!("{key}: {message}"));
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    let file_tls_names = file
        .server
        .tls_names
        .as_ref()
        .map(|list| names_in("server.tls_names", list, &mut errors));
    let flag_tls_names = (!flags.tls_names.is_empty()).then(|| flags.tls_names.clone());
    let tls_names = pick!(
        "server.tls_names",
        flag_tls_names,
        file_tls_names,
        Vec::new()
    );
    let file_public_names = file
        .server
        .tls_public_names
        .as_ref()
        .map(|list| names_in("server.tls_public_names", list, &mut errors));
    let flag_public_names =
        (!flags.tls_public_names.is_empty()).then(|| flags.tls_public_names.clone());
    let tls_public_names = pick!(
        "server.tls_public_names",
        flag_public_names,
        file_public_names,
        Vec::new()
    );
    let tls_key = if flags.tls_names.is_empty() && flags.tls_public_names.is_empty() {
        "server.tls_names"
    } else {
        "--tls-name"
    };
    let tls_names = crate::tls::ExtraNames::new(tls_names, tls_public_names).unwrap_or_else(|e| {
        errors.extend(e.into_iter().map(|m| format!("{tls_key}: {m}")));
        crate::tls::ExtraNames::default()
    });
    let file_scrollback = file.sessions.scrollback_lines.and_then(|n| {
        if (1..=MAX_SCROLLBACK_LINES).contains(&n) {
            Some(n)
        } else {
            errors.push(format!(
                "sessions.scrollback_lines: {n} is outside 1..={MAX_SCROLLBACK_LINES}"
            ));
            None
        }
    });
    let scrollback_lines = pick!(
        "sessions.scrollback_lines",
        flags.scrollback_lines,
        file_scrollback,
        crate::vt::DEFAULT_SCROLLBACK_LINES
    );
    let restore = pick!(
        "sessions.restore",
        flags.no_restore.then_some(false),
        file.sessions.restore,
        true
    );
    let agent_resume = pick!(
        "sessions.agent_resume",
        flags.no_agent_resume.then_some(false),
        file.sessions.agent_resume,
        true
    );
    let file_prefix = file.worktrees.branch_prefix.as_deref().and_then(|raw| {
        match crate::git::validate_prefix(raw) {
            Ok(prefix) => Some(prefix),
            Err(message) => {
                errors.push(format!("worktrees.branch_prefix: {message}"));
                None
            }
        }
    });
    let worktree_branch_prefix = pick!(
        "worktrees.branch_prefix",
        flags.worktree_branch_prefix.clone(),
        file_prefix,
        String::new()
    );
    let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
    let file_root = file.import.root.as_deref().and_then(|raw| {
        let raw = raw.trim();
        let path = crate::state::expand_home(Path::new(raw), home);
        if raw.is_empty() || !path.is_absolute() {
            errors.push(format!(
                "import.root: {raw:?} must be an absolute path or start with ~/"
            ));
            None
        } else {
            Some(Some(path))
        }
    });
    let flag_root = flags
        .import_root
        .as_ref()
        .map(|root| Some(absolute(crate::state::expand_home(root, home))));
    let import_root = pick!(
        "import.root",
        flag_root,
        file_root,
        home.map(|h| h.join("code"))
    );
    let flywheel = pick!(
        "flywheel.enabled",
        flags.no_flywheel.then_some(false),
        file.flywheel.enabled,
        true
    );
    let claude_file = file.agents.claude.clone().unwrap_or_default();
    let claude = launch_defaults(
        SessionKind::Claude,
        AgentLaunch {
            model: claude_file.model,
            permission_mode: claude_file.permission_mode,
            approval: None,
            sandbox: None,
        },
        &mut errors,
    );
    let codex_file = file.agents.codex.clone().unwrap_or_default();
    let codex = launch_defaults(
        SessionKind::Codex,
        AgentLaunch {
            model: codex_file.model,
            permission_mode: None,
            approval: codex_file.approval,
            sandbox: codex_file.sandbox,
        },
        &mut errors,
    );
    let field_source = |set: bool| if set { Source::File } else { Source::Default };
    sources.extend([
        ("agents.claude.model", field_source(claude.model.is_some())),
        (
            "agents.claude.permission_mode",
            field_source(claude.permission_mode.is_some()),
        ),
        ("agents.codex.model", field_source(codex.model.is_some())),
        (
            "agents.codex.approval",
            field_source(codex.approval.is_some()),
        ),
        (
            "agents.codex.sandbox",
            field_source(codex.sandbox.is_some()),
        ),
    ]);
    let plugins_enabled = pick!("plugins.enabled", None::<bool>, file.plugins.enabled, true);
    let file_concurrency = file.plugins.max_concurrent.and_then(|n| {
        if (1..=MAX_PLUGIN_CONCURRENCY).contains(&n) {
            Some(n)
        } else {
            errors.push(format!(
                "plugins.max_concurrent: {n} is outside 1..={MAX_PLUGIN_CONCURRENCY}"
            ));
            None
        }
    });
    let plugins_max_concurrent = pick!(
        "plugins.max_concurrent",
        None::<usize>,
        file_concurrency,
        DEFAULT_PLUGIN_CONCURRENCY
    );
    let (record_limits, share_policy) =
        resolve_recordings_and_shares(file, flags, &mut sources, &mut errors);
    let file_reviewer = file.review.reviewer.as_deref().and_then(|raw| {
        ReviewerChoice::parse(raw)
            .map_err(|message| errors.push(format!("review.reviewer: {message}")))
            .ok()
    });
    let reviewer = pick!(
        "review.reviewer",
        None::<ReviewerChoice>,
        file_reviewer,
        ReviewerChoice::Other
    );
    let file_quiet = file.queue.typing_quiet.as_ref().and_then(|setting| {
        let raw = match setting {
            DurationSetting::Seconds(n) => n.to_string(),
            DurationSetting::Text(text) => text.clone(),
        };
        crate::queue::parse_quiet(&raw)
            .map_err(|message| errors.push(format!("queue.typing_quiet: {message}")))
            .ok()
    });
    let queue_typing_quiet = pick!(
        "queue.typing_quiet",
        None::<Duration>,
        file_quiet,
        crate::queue::DEFAULT_TYPING_QUIET
    );
    let days_in = |key: &str, days: u32, errors: &mut Vec<String>| {
        use crate::auth::{MAX_LOGIN_DAYS, MIN_LOGIN_DAYS};
        if (MIN_LOGIN_DAYS..=MAX_LOGIN_DAYS).contains(&days) {
            Some(days)
        } else {
            errors.push(format!(
                "{key}: {days} days is out of range ({MIN_LOGIN_DAYS}-{MAX_LOGIN_DAYS})"
            ));
            None
        }
    };
    let file_login_days = file
        .server
        .login_days
        .and_then(|days| days_in("server.login_days", days, &mut errors));
    let flag_login_days = flags
        .login_days
        .and_then(|days| days_in("--login-days", days, &mut errors));
    let login_days = pick!(
        "server.login_days",
        flag_login_days,
        file_login_days,
        crate::auth::DEFAULT_LOGIN_DAYS
    );
    let digest = match crate::digest::DigestConfig::resolve(&file.digest) {
        Ok(digest) => digest,
        Err(found) => {
            errors.extend(found);
            crate::digest::DigestConfig::default()
        }
    };
    let section = &file.digest;
    for (key, set) in [
        ("digest.times", section.times.is_some()),
        ("digest.timezone", section.timezone.is_some()),
        ("digest.channels", section.channels.is_some()),
        ("digest.quiet_if_empty", section.quiet_if_empty.is_some()),
    ] {
        sources.push((key, if set { Source::File } else { Source::Default }));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Effective {
        bind,
        port,
        public_urls,
        allow_origins,
        trusted_proxies,
        tls,
        tls_names,
        scrollback_lines,
        restore,
        agent_resume,
        worktree_branch_prefix,
        import_root,
        flywheel,
        agent_defaults: AgentDefaults { claude, codex },
        plugins_enabled,
        plugins_max_concurrent,
        record_limits,
        share_policy,
        reviewer,
        queue_typing_quiet,
        login_days,
        digest,
        sources,
    })
}

/// `[recordings]` and `[shares]`, flag over file over default, each value
/// in its bounds and the pair consistent (the directory holds at least a
/// part; the default expiry is no longer than the longest).
fn resolve_recordings_and_shares(
    file: &FileConfig,
    flags: &Flags,
    sources: &mut Vec<(&'static str, Source)>,
    errors: &mut Vec<String>,
) -> (crate::record::RecordLimits, crate::share::SharePolicy) {
    use crate::record::{
        DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_TOTAL_BYTES, MAX_DIR_BYTES, MAX_PART_BYTES,
        MIN_PART_BYTES, RecordLimits, format_size,
    };
    use crate::share::{DEFAULT_TTL, MAX_TTL, SharePolicy, format_ttl};
    let mut pick = |key: &'static str, flag: Option<u64>, file: Option<u64>, default: u64| {
        let (value, source) = match (flag, file) {
            (Some(v), _) => (v, Source::Flag),
            (None, Some(v)) => (v, Source::File),
            (None, None) => (default, Source::Default),
        };
        sources.push((key, source));
        value
    };
    let mut size = |key: &str, setting: &Option<SizeSetting>| {
        setting.as_ref().and_then(|s| match s.bytes() {
            Ok(n) => Some(n),
            Err(message) => {
                errors.push(format!("{key}: {message}"));
                None
            }
        })
    };
    let file_part = size("recordings.part_cap", &file.recordings.part_cap);
    let file_dir = size("recordings.dir_cap", &file.recordings.dir_cap);
    let mut duration = |key: &str, setting: &Option<DurationSetting>| {
        setting.as_ref().and_then(|s| match s.duration() {
            Ok(d) => Some(d.as_secs()),
            Err(message) => {
                errors.push(format!("{key}: {message}"));
                None
            }
        })
    };
    let file_default = duration("shares.default_expiry", &file.shares.default_expiry);
    let file_max = duration("shares.max_expiry", &file.shares.max_expiry);
    let part = pick(
        "recordings.part_cap",
        flags.record_part_cap,
        file_part,
        DEFAULT_MAX_FILE_BYTES,
    );
    let dir = pick(
        "recordings.dir_cap",
        flags.record_dir_cap,
        file_dir,
        DEFAULT_MAX_TOTAL_BYTES,
    );
    let default_ttl = pick(
        "shares.default_expiry",
        flags.share_default_expiry.map(|d| d.as_secs()),
        file_default,
        DEFAULT_TTL.as_secs(),
    );
    let max_ttl = pick(
        "shares.max_expiry",
        flags.share_max_expiry.map(|d| d.as_secs()),
        file_max,
        MAX_TTL.as_secs(),
    );
    if !(MIN_PART_BYTES..=MAX_PART_BYTES).contains(&part) {
        errors.push(format!(
            "recordings.part_cap: {} is outside {}..={}",
            format_size(part),
            format_size(MIN_PART_BYTES),
            format_size(MAX_PART_BYTES)
        ));
    } else if dir < part || dir > MAX_DIR_BYTES {
        errors.push(format!(
            "recordings.dir_cap: {} is outside {}..={} (at least recordings.part_cap)",
            format_size(dir),
            format_size(part),
            format_size(MAX_DIR_BYTES)
        ));
    }
    // Each expiry is already within MIN_TTL..=MAX_TTL (parse_ttl).
    if default_ttl > max_ttl {
        errors.push(format!(
            "shares.default_expiry: {} is longer than shares.max_expiry ({})",
            format_ttl(Duration::from_secs(default_ttl)),
            format_ttl(Duration::from_secs(max_ttl))
        ));
    }
    (
        RecordLimits {
            max_file_bytes: part,
            max_total_bytes: dir,
        },
        SharePolicy {
            default_ttl: Duration::from_secs(default_ttl),
            max_ttl: Duration::from_secs(max_ttl),
        },
    )
}

/// A kind's launch defaults, validated as a launch of that kind is.
fn launch_defaults(
    kind: SessionKind,
    launch: AgentLaunch,
    errors: &mut Vec<String>,
) -> AgentLaunch {
    match launch.validated(kind) {
        Ok(launch) => launch,
        Err(message) => {
            errors.push(format!("agents.{}: {message}", kind.as_str()));
            AgentLaunch::default()
        }
    }
}

/// Load and resolve the state dir's file against `flags`; the errors
/// joined one per line, prefixed with the file's path.
pub fn load(state_dir: &Path, flags: &Flags, home: Option<&Path>) -> Result<Effective, String> {
    let path = state_dir.join(CONFIG_FILE);
    let file = read_file(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .unwrap_or_default();
    resolve(&file, flags, home).map_err(|errors| {
        format!(
            "{}: {}",
            path.display(),
            errors.join(&format!("\n{}: ", path.display()))
        )
    })
}

/// The process's home directory.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

// --- the running hub's view -------------------------------------------------

/// Identity, size, and mtime: a changed (or replaced, or removed) file.
type Fingerprint = Option<((u64, u64), u64, Option<SystemTime>)>;

fn fingerprint(path: &Path) -> Fingerprint {
    let meta = std::fs::metadata(path).ok()?;
    Some((file_identity(&meta), meta.len(), meta.modified().ok()))
}

#[derive(Debug)]
struct WatchState {
    fingerprint: Fingerprint,
    current: Effective,
    exists: bool,
    /// Why the file on disk is not in force (the last good one is).
    error: Option<String>,
    /// Bumped on every reload that changed what is in force or the error.
    rev: u64,
    loaded_at: chrono::DateTime<chrono::Utc>,
}

/// Applies a reloaded configuration (to the hub, the plugin host).
pub type ApplyFn = Box<dyn Fn(&Effective) + Send + Sync>;

/// The running hub's settings: what it started with, what is in force
/// now, and the file's state, re-read every [`POLL`].
pub struct ConfigWatch {
    path: PathBuf,
    flags: Flags,
    home: Option<PathBuf>,
    started: Effective,
    state: Mutex<WatchState>,
    appliers: Mutex<Vec<ApplyFn>>,
}

impl std::fmt::Debug for ConfigWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigWatch")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Why a settings write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The values asked for are invalid (each `key: why`).
    Invalid(Vec<String>),
    /// The file cannot take them as asked (invalid on disk, unreadable,
    /// shaped so it cannot be rewritten in place).
    Conflict(String),
    /// The file changed since the writer read it (`rev` moved on).
    Stale(u64),
}

/// What reloading the file did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reload {
    /// Nothing changed on disk.
    Unchanged,
    /// The new settings are in force.
    Applied,
    /// The file is invalid; the last good settings stay.
    Invalid(String),
}

impl ConfigWatch {
    /// Load `<state-dir>/hub.toml` under `flags`; an invalid file is an
    /// error (a hub never starts on settings it cannot read).
    pub fn open(state_dir: &Path, flags: Flags, home: Option<PathBuf>) -> Result<Self, String> {
        let path = state_dir.join(CONFIG_FILE);
        let print = fingerprint(&path);
        let started = load(state_dir, &flags, home.as_deref())?;
        Ok(Self {
            state: Mutex::new(WatchState {
                fingerprint: print,
                current: started.clone(),
                exists: print.is_some(),
                error: None,
                rev: 1,
                loaded_at: chrono::Utc::now(),
            }),
            path,
            flags,
            home,
            started,
            appliers: Mutex::new(Vec::new()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the hub started with (restart-only keys stay these).
    pub fn started(&self) -> &Effective {
        &self.started
    }

    /// What is in force now.
    pub fn current(&self) -> Effective {
        self.lock().current.clone()
    }

    /// The reload counter (bumped when what is in force, or the file's
    /// error, changed).
    pub fn rev(&self) -> u64 {
        self.lock().rev
    }

    /// Why the file on disk is not in force, if it is not.
    pub fn error(&self) -> Option<String> {
        self.lock().error.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `apply` on every reload that puts new settings in force.
    pub fn on_apply(&self, apply: ApplyFn) {
        self.appliers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(apply);
    }

    /// Re-read the file if it changed on disk.
    pub fn reload(&self) -> Reload {
        let print = fingerprint(&self.path);
        {
            let state = self.lock();
            if state.fingerprint == print {
                return Reload::Unchanged;
            }
        }
        let loaded = read_file(&self.path).and_then(|file| {
            resolve(&file.unwrap_or_default(), &self.flags, self.home.as_deref())
                .map_err(|errors| errors.join("; "))
        });
        let mut state = self.lock();
        state.fingerprint = print;
        state.exists = print.is_some();
        state.loaded_at = chrono::Utc::now();
        match loaded {
            Ok(effective) => {
                let changed = effective != state.current || state.error.is_some();
                state.error = None;
                if !changed {
                    return Reload::Unchanged;
                }
                state.current = effective.clone();
                state.rev += 1;
                drop(state);
                for apply in self
                    .appliers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                {
                    apply(&effective);
                }
                Reload::Applied
            }
            Err(message) => {
                if state.error.as_deref() != Some(&message) {
                    state.error = Some(message.clone());
                    state.rev += 1;
                }
                Reload::Invalid(message)
            }
        }
    }

    /// Poll the file on a thread until `alive` is gone, calling `changed`
    /// after every reload that changed something (applied or invalid).
    pub fn spawn<T: Send + Sync + 'static>(
        self: &Arc<Self>,
        alive: Weak<T>,
        changed: impl Fn(&T, &Reload) + Send + 'static,
    ) -> std::io::Result<()> {
        let watch = Arc::clone(self);
        std::thread::Builder::new()
            .name("hub-config".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(POLL);
                    let Some(owner) = alive.upgrade() else { return };
                    let before = watch.rev();
                    let reload = watch.reload();
                    match &reload {
                        Reload::Unchanged => {}
                        Reload::Applied => {
                            eprintln!("fw hub: reloaded {}", watch.path.display());
                        }
                        Reload::Invalid(message) => {
                            eprintln!(
                                "fw hub: {} is invalid, the last good settings stay: {message}",
                                watch.path.display()
                            );
                        }
                    }
                    if watch.rev() != before {
                        changed(&owner, &reload);
                    }
                }
            })
            .map(|_| ())
    }

    /// Keep `hub` (its live settings) and `plugins` (on or off) on what
    /// the file says, re-read every [`POLL`], telling every open UI
    /// (`HubEvent::Config`) when a reload applied or found the file
    /// invalid.
    pub fn attach(
        self: &Arc<Self>,
        hub: &Arc<crate::hub::Hub>,
        plugins: Option<Arc<crate::plugins::PluginHost>>,
    ) -> std::io::Result<()> {
        let weak = Arc::downgrade(hub);
        self.on_apply(Box::new(move |effective| {
            if let Some(hub) = weak.upgrade() {
                hub.set_tunables(effective.tunables());
            }
        }));
        if let Some(host) = plugins {
            self.on_apply(Box::new(move |effective| {
                host.set_master(effective.plugins_enabled);
            }));
        }
        let watching = Arc::clone(self);
        self.spawn(Arc::downgrade(hub), move |hub: &crate::hub::Hub, _| {
            hub.emit(crate::hub::HubEvent::Config {
                rev: watching.rev(),
            });
        })
    }

    /// Replace the file's `[digest]` table with `section` (Settings >
    /// Digest): the rest of the file is kept line for line, the whole is
    /// checked to resolve before it is written (atomically, its mode
    /// kept), then reloaded. Refused while the file on disk is invalid or
    /// sets `digest` keys some other way (a dotted key, an inline table).
    /// `change` makes the new table from the one on disk; `rev`: the
    /// [`Self::rev`] the writer read, refused as stale once it moved on.
    pub fn write_digest(
        &self,
        rev: Option<u64>,
        change: impl FnOnce(&crate::digest::DigestSection) -> crate::digest::DigestSection,
    ) -> Result<(), WriteError> {
        let _ = self.reload();
        if let Some(rev) = rev
            && rev != self.rev()
        {
            return Err(WriteError::Stale(self.rev()));
        }
        if let Some(error) = self.error() {
            return Err(WriteError::Conflict(format!(
                "{} is invalid; fix it first: {error}",
                self.path.display()
            )));
        }
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(WriteError::Conflict(format!(
                    "cannot read {}: {e}",
                    self.path.display()
                )));
            }
        };
        let by_hand = |why: String| {
            WriteError::Conflict(format!(
                "cannot rewrite [digest] in {} ({why}); edit it by hand",
                self.path.display()
            ))
        };
        let on_disk = parse(&text).map_err(&by_hand)?;
        let section = &change(&on_disk.digest);
        crate::digest::DigestConfig::resolve(section).map_err(WriteError::Invalid)?;
        let spliced = crate::digest::splice_section(&text, section).map_err(&by_hand)?;
        let file = parse(&spliced).map_err(&by_hand)?;
        if file.digest != *section {
            return Err(by_hand(
                "it sets digest keys outside a [digest] table".to_owned(),
            ));
        }
        resolve(&file, &self.flags, self.home.as_deref())
            .map_err(|errors| by_hand(errors.join("; ")))?;
        crate::util::write_atomic(
            &self.path,
            spliced.as_bytes(),
            crate::util::FileMode::KeepOr(0o600),
        )
        .map_err(|e| WriteError::Conflict(format!("cannot write {}: {e}", self.path.display())))?;
        let _ = self.reload();
        Ok(())
    }

    /// The `GET /api/config` / `fw hub config show` view.
    pub fn view(&self) -> Value {
        let state = self.lock();
        let current = &state.current;
        json!({
            "file": {
                "path": self.path.to_string_lossy(),
                "exists": state.exists,
                "error": state.error,
                "loaded_at": state.loaded_at,
            },
            "rev": state.rev,
            "values": current.entries(),
            "pending_restart": current.pending_restart(&self.started),
            "running": true,
        })
    }
}

/// `fw hub config show` without a running hub: the file over the defaults
/// (no flags known).
pub fn offline_view(state_dir: &Path, home: Option<&Path>) -> Result<Value, String> {
    let path = state_dir.join(CONFIG_FILE);
    let exists = path.exists();
    let effective = load(state_dir, &Flags::default(), home)?;
    Ok(json!({
        "file": {"path": path.to_string_lossy(), "exists": exists, "error": null},
        "values": effective.entries(),
        "pending_restart": [],
        "running": false,
    }))
}

/// A commented example of every key, for the docs and `config show`'s
/// pointer.
pub const EXAMPLE: &str = r#"[server]
bind = "127.0.0.1"
port = 7420
public_urls = ["https://box.tailnet.ts.net"]
allow_origins = []
trusted_proxies = []
tls = "off"                # "auto", or "/path/cert.pem,/path/key.pem"
tls_names = []             # e.g. ["192.168.0.0/16"]
tls_public_names = []
login_days = 30            # a login lasts this long after its last use

[sessions]
scrollback_lines = 5000
restore = true
agent_resume = true

[worktrees]
branch_prefix = "agent/"

[import]
root = "~/code"

[flywheel]
enabled = true

[agents.claude]            # a launch naming none gets these
model = "opus"
permission_mode = "acceptEdits"
[agents.codex]
model = "gpt-5-codex"
approval = "on-request"
sandbox = "workspace-write"

[plugins]
enabled = true
max_concurrent = 4

[recordings]
part_cap = "64MiB"
dir_cap = "1GiB"

[shares]
default_expiry = "1h"
max_expiry = "30d"

[review]
reviewer = "other"         # other (claude <-> codex) | same | claude | codex

[queue]
typing_quiet = "8s"        # no typing this long before a queue delivers

[digest]                   # scheduled summaries; off until times are set
times = ["08:00", "18:00"] # local times in timezone
timezone = "Europe/Berlin" # IANA name; default the system's
channels = ["push", "browser", "plugin"]
quiet_if_empty = true      # skip a scheduled one with nothing in it
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Option<&'static Path> {
        Some(Path::new("/home/u"))
    }

    #[test]
    fn the_example_parses_and_resolves() {
        let file = parse(EXAMPLE).unwrap();
        let effective = resolve(&file, &Flags::default(), home()).unwrap();
        assert_eq!(effective.port, 7420);
        assert_eq!(effective.worktree_branch_prefix, "agent/");
        assert_eq!(effective.import_root, Some(PathBuf::from("/home/u/code")));
        assert_eq!(
            effective.agent_defaults.claude.permission_mode.as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(effective.source("server.port"), Source::File);
        // Every documented key is in the example and in KEYS.
        for (key, _) in KEYS {
            let leaf = key.rsplit('.').next().unwrap();
            assert!(EXAMPLE.contains(&format!("{leaf} =")), "{key}");
        }
    }

    #[test]
    fn tls_names_take_public_names_only_by_name() {
        let spec = |raw: &str| crate::tls::parse_name(raw).unwrap();
        let file = parse("[server]\ntls_names = [\"hub.lan\", \"192.168.0.0/16\"]\n").unwrap();
        let e = resolve(&file, &Flags::default(), home()).unwrap();
        assert_eq!(
            e.tls_names.names(),
            [spec("hub.lan"), spec("192.168.0.0/16")]
        );
        assert!(e.tls_names.public().is_empty());
        // A public one from the file or a flag: refused, naming the rule
        // and the opt-in.
        for (file, flags) in [
            (
                parse("[server]\ntls_names = [\"hub.lan\", \"google.com\"]\n").unwrap(),
                Flags::default(),
            ),
            (
                FileConfig::default(),
                Flags {
                    tls_names: vec![spec("example.co.uk")],
                    ..Flags::default()
                },
            ),
        ] {
            let errors = resolve(&file, &flags, home()).unwrap_err();
            assert_eq!(errors.len(), 1, "{errors:?}");
            let key = if flags.tls_names.is_empty() {
                "server.tls_names: "
            } else {
                "--tls-name: "
            };
            assert!(errors[0].starts_with(key), "{errors:?}");
            assert!(
                errors[0].contains("public name") && errors[0].contains(".ts.net"),
                "{errors:?}"
            );
            assert!(errors[0].contains("--tls-allow-public-name"), "{errors:?}");
        }
        // Opted in (file or flag): taken, and shown as public.
        let file = parse(
            "[server]\ntls_names = [\"hub.example.com\"]\ntls_public_names = [\"hub.example.com\"]\n",
        )
        .unwrap();
        let e = resolve(&file, &Flags::default(), home()).unwrap();
        assert_eq!(e.tls_names.public(), [spec("hub.example.com")]);
        assert_eq!(
            e.value("server.tls_public_names"),
            json!(["hub.example.com"])
        );
        assert_eq!(e.source("server.tls_public_names"), Source::File);
        let flags = Flags {
            tls_names: vec![spec("8.8.8.8")],
            tls_public_names: vec![spec("8.8.8.8")],
            ..Flags::default()
        };
        let e = resolve(&FileConfig::default(), &flags, home()).unwrap();
        assert_eq!(e.tls_names.public(), [spec("8.8.8.8")]);
        // A stray opt-in is an error too.
        let flags = Flags {
            tls_public_names: vec![spec("google.com")],
            ..Flags::default()
        };
        let errors = resolve(&FileConfig::default(), &flags, home()).unwrap_err();
        assert!(errors[0].contains("not a --tls-name"), "{errors:?}");
    }

    #[test]
    fn flag_beats_file_beats_default() {
        let file = parse("[server]\nport = 7601\nbind = \"0.0.0.0\"\n[sessions]\nscrollback_lines = 900\nrestore = true\n").unwrap();
        let flags = Flags {
            port: Some(7602),
            no_restore: true,
            ..Flags::default()
        };
        let e = resolve(&file, &flags, home()).unwrap();
        assert_eq!((e.port, e.source("server.port")), (7602, Source::Flag));
        assert_eq!(
            (e.bind.to_string(), e.source("server.bind")),
            ("0.0.0.0".to_owned(), Source::File)
        );
        assert_eq!(
            (e.scrollback_lines, e.source("sessions.scrollback_lines")),
            (900, Source::File)
        );
        assert_eq!(
            (e.restore, e.source("sessions.restore")),
            (false, Source::Flag)
        );
        assert_eq!(
            (e.agent_resume, e.source("sessions.agent_resume")),
            (true, Source::Default)
        );
        assert_eq!(e.source("worktrees.branch_prefix"), Source::Default);
        // A list flag replaces the file's list.
        let file =
            parse("[server]\npublic_urls = [\"https://a.example\", \"https://b.example\"]\n")
                .unwrap();
        let flags = Flags {
            public_urls: vec!["https://C.example/".into()],
            ..Flags::default()
        };
        let e = resolve(&file, &flags, home()).unwrap();
        assert_eq!(e.public_urls, vec!["https://c.example".to_owned()]);
    }

    #[test]
    fn every_bad_value_is_reported_with_its_key() {
        let file = parse(
            r#"
[server]
bind = "localhost"
public_urls = ["https://box/path"]
trusted_proxies = ["proxy"]
login_days = 0
[sessions]
scrollback_lines = 0
[worktrees]
branch_prefix = "-x"
[import]
root = "relative/dir"
[agents.claude]
permission_mode = "yolo"
[agents.codex]
approval = "sometimes"
[plugins]
max_concurrent = 99
"#,
        )
        .unwrap();
        let errors = resolve(&file, &Flags::default(), home()).unwrap_err();
        for key in [
            "server.bind",
            "server.public_urls",
            "server.trusted_proxies",
            "server.login_days",
            "sessions.scrollback_lines",
            "worktrees.branch_prefix",
            "import.root",
            "agents.claude",
            "agents.codex",
            "plugins.max_concurrent",
        ] {
            assert!(
                errors.iter().any(|e| e.starts_with(key)),
                "{key} not in {errors:?}"
            );
        }
    }

    #[test]
    fn login_days_is_bounded_and_flag_beats_file() {
        let resolved = |text: &str, flag: Option<u32>| {
            let flags = Flags {
                login_days: flag,
                ..Flags::default()
            };
            resolve(&parse(text).unwrap(), &flags, home())
        };
        let e = resolved("", None).unwrap();
        assert_eq!(e.login_days, crate::auth::DEFAULT_LOGIN_DAYS);
        assert_eq!(e.source("server.login_days"), Source::Default);
        let e = resolved("[server]\nlogin_days = 365\n", None).unwrap();
        assert_eq!(
            (e.login_days, e.source("server.login_days")),
            (365, Source::File)
        );
        assert_eq!(e.value("server.login_days"), json!(365));
        assert_eq!(e.tunables().login_days, 365);
        let e = resolved("[server]\nlogin_days = 365\n", Some(1)).unwrap();
        assert_eq!(
            (e.login_days, e.source("server.login_days")),
            (1, Source::Flag)
        );
        for bad in ["0", "366"] {
            let errors = resolved(&format!("[server]\nlogin_days = {bad}\n"), None).unwrap_err();
            assert!(
                errors.iter().any(|m| m.starts_with("server.login_days:")),
                "{errors:?}"
            );
        }
        assert!(resolved("", Some(0)).is_err());
        assert!(parse("[server]\nlogin_days = -1\n").is_err());
        assert!(parse("[server]\nlogin_days = \"30d\"\n").is_err());
    }

    #[test]
    fn unknown_keys_and_wrong_types_are_refused() {
        let e = parse("[server]\nprot = 1\n").unwrap_err();
        assert!(e.contains("prot"), "{e}");
        let e = parse("[nope]\n").unwrap_err();
        assert!(e.contains("nope"), "{e}");
        let e = parse("[server]\nport = \"x\"\n").unwrap_err();
        assert!(e.contains("port"), "{e}");
        let e = parse("[server]\nport = 70000\n").unwrap_err();
        assert!(e.contains("port"), "{e}");
    }

    #[test]
    fn agent_defaults_fill_only_what_a_launch_leaves_unset() {
        let defaults = AgentDefaults {
            claude: AgentLaunch {
                model: Some("opus".into()),
                permission_mode: Some("plan".into()),
                ..AgentLaunch::default()
            },
            codex: AgentLaunch::default(),
        };
        let given = AgentLaunch {
            model: Some("sonnet".into()),
            permission_mode: Some("  ".into()),
            ..AgentLaunch::default()
        };
        let filled = defaults.fill(SessionKind::Claude, &given);
        assert_eq!(filled.model.as_deref(), Some("sonnet"));
        assert_eq!(filled.permission_mode.as_deref(), Some("plan"));
        assert_eq!(
            defaults.fill(SessionKind::Shell, &AgentLaunch::default()),
            AgentLaunch::default()
        );
    }

    #[test]
    fn reload_applies_good_edits_and_keeps_the_last_good_on_bad_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, "[worktrees]\nbranch_prefix = \"a/\"\n").unwrap();
        let watch = Arc::new(
            ConfigWatch::open(dir.path(), Flags::default(), Some("/home/u".into())).unwrap(),
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        watch.on_apply(Box::new(move |e| {
            sink.lock().unwrap().push(e.worktree_branch_prefix.clone());
        }));
        assert_eq!(watch.reload(), Reload::Unchanged);
        std::fs::write(
            &path,
            "[worktrees]\nbranch_prefix = \"bb/\"\n[server]\nport = 7690\n",
        )
        .unwrap();
        assert_eq!(watch.reload(), Reload::Applied);
        assert_eq!(watch.current().worktree_branch_prefix, "bb/");
        // The port waits for a restart; the view says so.
        let view = watch.view();
        assert_eq!(view["pending_restart"], json!(["server.port"]));
        std::fs::write(&path, "[worktrees]\nbranch_prefix = \"..\"\n").unwrap();
        match watch.reload() {
            Reload::Invalid(message) => assert!(message.contains("branch_prefix"), "{message}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(watch.current().worktree_branch_prefix, "bb/");
        assert!(watch.error().is_some());
        assert_eq!(*seen.lock().unwrap(), vec!["bb/".to_owned()]);
        // Removing the file is the defaults.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(watch.reload(), Reload::Applied);
        assert_eq!(watch.current().worktree_branch_prefix, "");
        assert_eq!(watch.error(), None);
    }

    #[test]
    fn a_start_refuses_an_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "[server]\nport = -1\n").unwrap();
        let e = ConfigWatch::open(dir.path(), Flags::default(), None).unwrap_err();
        assert!(e.contains("hub.toml"), "{e}");
    }
}
