//! The effective config: eight keys, three TOML files, one precedence rule.
//!
//! ```text
//! $FLYWHEEL_HOME/config.toml               user        (any key)
//! <repo top-level>/.flywheel.toml          repo        (verify, protected_paths, land, project_id)
//! $FLYWHEEL_HOME/projects/<id>/config.toml personal    (any key)
//! ```
//!
//! Precedence: CLI flag > env > personal > repo > user > built-in default.
//! The only env input is `FLYWHEEL_PG_DSN` (`store`). Any key outside the
//! eight -- including every old `flywheel.toml` section -- is an error that
//! names it. The repo file is committed, so it is refused outright when it
//! carries anything credential-shaped (see [`credential_key`]), and it is
//! honored only once approved in `trust.json` ([`crate::trust`]) under the
//! LOCAL project id and the sha256 of its bytes.
//!
//! [`EffectiveConfig::apply_to_policy`] maps the result onto the
//! [`WorkPolicy`] every verb already runs on.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json};
use toml::{Table, Value};

use crate::home::{Home, Project, ProjectError};
use crate::policy::WorkPolicy;
use crate::store_factory::PG_DSN_ENV;
use crate::store_registry::{DEFAULT_BACKEND, Dsn};
use crate::trust::{self, TrustError};

/// The committed repo file, at the repo's top level.
pub const REPO_CONFIG_FILENAME: &str = ".flywheel.toml";

/// Every effective-config key, in display order.
pub const KEYS: [&str; 8] = [
    "model",
    "concurrency",
    "store",
    "transcript_retention_days",
    "verify",
    "protected_paths",
    "land",
    "project_id",
];

/// The keys the committed repo file may set.
pub const REPO_KEYS: [&str; 4] = ["verify", "protected_paths", "land", "project_id"];

/// Built-in worker concurrency.
pub const DEFAULT_CONCURRENCY: i64 = 1;
/// Built-in transcript retention window, in days.
pub const DEFAULT_TRANSCRIPT_RETENTION_DAYS: i64 = 90;

/// Key-name fragments that mark a repo-file key as credential-shaped.
const CREDENTIAL_NAME_FRAGMENTS: [&str; 5] = ["dsn", "password", "passwd", "token", "secret"];

// --- Sources and values ------------------------------------------------------------

/// Where an effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Default,
    User,
    Repo,
    Personal,
    Env,
    Flag,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
            Self::Repo => "repo",
            Self::Personal => "personal",
            Self::Env => "env",
            Self::Flag => "flag",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which file a layer was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    /// `$FLYWHEEL_HOME/config.toml`.
    User,
    /// `$FLYWHEEL_HOME/projects/<id>/config.toml`.
    Personal,
    /// `<repo>/.flywheel.toml`.
    Repo,
}

impl LayerKind {
    pub fn source(self) -> Source {
        match self {
            Self::User => Source::User,
            Self::Personal => Source::Personal,
            Self::Repo => Source::Repo,
        }
    }

    fn allowed(self) -> &'static [&'static str] {
        match self {
            Self::Repo => &REPO_KEYS,
            Self::User | Self::Personal => &KEYS,
        }
    }
}

/// The `store` setting: SQLite at the project's db path, or Postgres.
#[derive(Clone, PartialEq, Eq)]
pub enum StoreSetting {
    Sqlite,
    Postgres(Dsn),
}

impl StoreSetting {
    /// The operator-facing form: `sqlite`, or the DSN with any password
    /// replaced by `***`.
    pub fn redacted(&self) -> String {
        match self {
            Self::Sqlite => DEFAULT_BACKEND.to_owned(),
            Self::Postgres(dsn) => redact_dsn(dsn.expose()),
        }
    }
}

impl fmt::Debug for StoreSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

/// How a verified task lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Land {
    Merge,
    Pr,
}

impl Land {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Pr => "pr",
        }
    }
}

/// One file's settings (every field optional).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLayer {
    pub model: Option<String>,
    pub concurrency: Option<i64>,
    pub store: Option<StoreSetting>,
    pub transcript_retention_days: Option<i64>,
    /// `Some(None)`: the file set `verify = ""` (no verify gate).
    pub verify: Option<Option<String>>,
    pub protected_paths: Option<Vec<String>>,
    pub land: Option<Land>,
    pub project_id: Option<String>,
}

/// The CLI flags that map onto config keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigFlags {
    pub model: Option<String>,
    pub concurrency: Option<i64>,
}

/// A value and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Source,
}

/// The repo file as the loader found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFile {
    pub path: PathBuf,
    pub sha256: String,
    /// Approved in trust.json (its keys apply) or not (they do not).
    pub trusted: bool,
}

/// The resolved config: every key with its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    /// `None`: the agent CLI's own default model.
    pub model: Sourced<Option<String>>,
    pub concurrency: Sourced<i64>,
    pub store: Sourced<StoreSetting>,
    pub transcript_retention_days: Sourced<i64>,
    pub verify: Sourced<Option<String>>,
    pub protected_paths: Sourced<Vec<String>>,
    pub land: Sourced<Land>,
    /// The project whose home dir this invocation uses.
    pub project_id: Sourced<String>,
    /// The project id in the repo's `.git/config` (trust is keyed by it).
    pub local_project_id: String,
    pub repo_file: Option<RepoFile>,
}

/// Everything [`resolve`] merges.
#[derive(Debug, Clone, Default)]
pub struct ConfigInputs {
    pub user: Option<ConfigLayer>,
    /// Only a trusted repo file's layer belongs here.
    pub repo: Option<ConfigLayer>,
    pub personal: Option<ConfigLayer>,
    /// `FLYWHEEL_PG_DSN`, when set and non-blank.
    pub env_dsn: Option<String>,
    pub flags: ConfigFlags,
    pub local_project_id: String,
}

fn pick<T: Clone>(default: T, layers: [(Source, Option<&T>); 5]) -> Sourced<T> {
    let mut out = Sourced {
        value: default,
        source: Source::Default,
    };
    for (source, value) in layers {
        if let Some(value) = value {
            out = Sourced {
                value: value.clone(),
                source,
            };
        }
    }
    out
}

/// Merge `inputs` by precedence (flag > env > personal > repo > user >
/// default). Pure.
pub fn resolve(inputs: &ConfigInputs) -> EffectiveConfig {
    let user = inputs.user.clone().unwrap_or_default();
    let repo = inputs.repo.clone().unwrap_or_default();
    let personal = inputs.personal.clone().unwrap_or_default();
    let env_store = inputs
        .env_dsn
        .as_ref()
        .filter(|v| !v.trim().is_empty())
        .map(|v| StoreSetting::Postgres(Dsn::new(v.clone())));
    let flag_model = inputs
        .flags
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .map(Some);
    let user_model = user.model.clone().map(Some);
    let personal_model = personal.model.clone().map(Some);
    EffectiveConfig {
        model: pick(
            None,
            [
                (Source::User, user_model.as_ref()),
                (Source::Repo, None),
                (Source::Personal, personal_model.as_ref()),
                (Source::Env, None),
                (Source::Flag, flag_model.as_ref()),
            ],
        ),
        concurrency: pick(
            DEFAULT_CONCURRENCY,
            [
                (Source::User, user.concurrency.as_ref()),
                (Source::Repo, None),
                (Source::Personal, personal.concurrency.as_ref()),
                (Source::Env, None),
                (Source::Flag, inputs.flags.concurrency.as_ref()),
            ],
        ),
        store: pick(
            StoreSetting::Sqlite,
            [
                (Source::User, user.store.as_ref()),
                (Source::Repo, None),
                (Source::Personal, personal.store.as_ref()),
                (Source::Env, env_store.as_ref()),
                (Source::Flag, None),
            ],
        ),
        transcript_retention_days: pick(
            DEFAULT_TRANSCRIPT_RETENTION_DAYS,
            [
                (Source::User, user.transcript_retention_days.as_ref()),
                (Source::Repo, None),
                (
                    Source::Personal,
                    personal.transcript_retention_days.as_ref(),
                ),
                (Source::Env, None),
                (Source::Flag, None),
            ],
        ),
        verify: pick(
            None,
            [
                (Source::User, user.verify.as_ref()),
                (Source::Repo, repo.verify.as_ref()),
                (Source::Personal, personal.verify.as_ref()),
                (Source::Env, None),
                (Source::Flag, None),
            ],
        ),
        protected_paths: pick(
            Vec::new(),
            [
                (Source::User, user.protected_paths.as_ref()),
                (Source::Repo, repo.protected_paths.as_ref()),
                (Source::Personal, personal.protected_paths.as_ref()),
                (Source::Env, None),
                (Source::Flag, None),
            ],
        ),
        land: pick(
            Land::Merge,
            [
                (Source::User, user.land.as_ref()),
                (Source::Repo, repo.land.as_ref()),
                (Source::Personal, personal.land.as_ref()),
                (Source::Env, None),
                (Source::Flag, None),
            ],
        ),
        project_id: pick(
            inputs.local_project_id.clone(),
            [
                (Source::User, user.project_id.as_ref()),
                (Source::Repo, repo.project_id.as_ref()),
                (Source::Personal, personal.project_id.as_ref()),
                (Source::Env, None),
                (Source::Flag, None),
            ],
        ),
        local_project_id: inputs.local_project_id.clone(),
        repo_file: None,
    }
}

impl EffectiveConfig {
    /// The pinned `fw config --json` shape: exactly [`KEYS`], each
    /// `{"value": ..., "source": ...}`; the store DSN's password redacted.
    pub fn to_json(&self) -> Json {
        fn entry(value: Json, source: Source) -> Json {
            let mut map = Map::new();
            map.insert("value".into(), value);
            map.insert("source".into(), Json::String(source.as_str().into()));
            Json::Object(map)
        }
        let opt = |v: &Option<String>| v.clone().map_or(Json::Null, Json::String);
        let mut map = Map::new();
        map.insert(
            "model".into(),
            entry(opt(&self.model.value), self.model.source),
        );
        map.insert(
            "concurrency".into(),
            entry(self.concurrency.value.into(), self.concurrency.source),
        );
        map.insert(
            "store".into(),
            entry(Json::String(self.store.value.redacted()), self.store.source),
        );
        map.insert(
            "transcript_retention_days".into(),
            entry(
                self.transcript_retention_days.value.into(),
                self.transcript_retention_days.source,
            ),
        );
        map.insert(
            "verify".into(),
            entry(opt(&self.verify.value), self.verify.source),
        );
        map.insert(
            "protected_paths".into(),
            entry(
                Json::Array(
                    self.protected_paths
                        .value
                        .iter()
                        .cloned()
                        .map(Json::String)
                        .collect(),
                ),
                self.protected_paths.source,
            ),
        );
        map.insert(
            "land".into(),
            entry(
                Json::String(self.land.value.as_str().into()),
                self.land.source,
            ),
        );
        map.insert(
            "project_id".into(),
            entry(
                Json::String(self.project_id.value.clone()),
                self.project_id.source,
            ),
        );
        Json::Object(map)
    }

    /// The text form: one `key  value  (source)` line per key.
    pub fn render_text(&self) -> String {
        let json = self.to_json();
        let mut out = String::new();
        for key in KEYS {
            let entry = &json[key];
            let value = match &entry["value"] {
                Json::Null => "(none)".to_owned(),
                Json::String(s) => s.clone(),
                Json::Array(items) if items.is_empty() => "[]".to_owned(),
                Json::Array(items) => items
                    .iter()
                    .map(|i| i.as_str().unwrap_or_default().to_owned())
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            let source = entry["source"].as_str().unwrap_or_default();
            out.push_str(&format!("{key:<26} {value}  ({source})\n"));
        }
        out
    }

    /// Map the config onto `policy`: model, worker concurrency, the submit
    /// AND phase-exit verify gates (one command, which is also the default
    /// grader a tracker item without graders is verified by), submit
    /// protected paths, the landing strategy (task work and autopilot
    /// work alike), the transcript retention window, and the store backend
    /// (+ DSN).
    pub fn apply_to_policy(&self, policy: &mut WorkPolicy) {
        policy.model.clone_from(&self.model.value);
        policy.worker_concurrency = self.concurrency.value;
        policy.submit_verify.clone_from(&self.verify.value);
        policy.phase_verify.clone_from(&self.verify.value);
        policy.default_graders = self
            .verify
            .value
            .iter()
            .filter_map(|command| flywheel_core::task::CommandGrader::new(command.clone()).ok())
            .map(flywheel_core::task::Grader::Command)
            .collect();
        policy.autopilot_landing = self.land.value.as_str().to_owned();
        policy
            .protected_paths
            .clone_from(&self.protected_paths.value);
        policy.submit_strategy = self.land.value.as_str().to_owned();
        policy.transcript_retention_days =
            u64::try_from(self.transcript_retention_days.value).unwrap_or(0);
        match &self.store.value {
            StoreSetting::Sqlite => {
                policy.store_backend = DEFAULT_BACKEND.to_owned();
                policy.store_dsn = None;
            }
            StoreSetting::Postgres(dsn) => {
                policy.store_backend = "postgres".to_owned();
                policy.store_dsn = Some(dsn.clone());
            }
        }
    }
}

// --- DSN redaction -----------------------------------------------------------------

/// `scheme://user:password@host/...` -> `scheme://user:***@host/...`;
/// keyword DSNs (`... password=secret ...`) get `password=***`. Anything
/// else is returned unchanged.
pub fn redact_dsn(dsn: &str) -> String {
    if let Some(scheme_end) = dsn.find("://") {
        let rest_start = scheme_end + 3;
        let rest = &dsn[rest_start..];
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if let Some(at) = authority.rfind('@') {
            let userinfo = &authority[..at];
            if let Some(colon) = userinfo.find(':') {
                return format!(
                    "{}{}:***{}",
                    &dsn[..rest_start],
                    &userinfo[..colon],
                    &dsn[rest_start + at..]
                );
            }
        }
        return dsn.to_owned();
    }
    dsn.split(' ')
        .map(|part| match part.split_once('=') {
            Some((key, _)) if key.eq_ignore_ascii_case("password") => format!("{key}=***"),
            _ => part.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `text` holds a `scheme://userinfo@...` URL; with
/// `require_password`, only one whose userinfo carries a `:password`.
fn has_userinfo_url(text: &str, require_password: bool) -> bool {
    let mut search = 0;
    while let Some(found) = text[search..].find("://") {
        let at = search + found;
        let scheme = text[..at]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
            .count();
        let rest = &text[at + 3..];
        let end = rest
            .find(|c: char| matches!(c, '/' | '?' | '#' | '"' | '\'') || c.is_whitespace())
            .unwrap_or(rest.len());
        let authority = &rest[..end];
        if scheme > 0
            && let Some(sep) = authority.rfind('@')
        {
            let userinfo = &authority[..sep];
            if !userinfo.is_empty() && (!require_password || userinfo.contains(':')) {
                return true;
            }
        }
        search = at + 3;
    }
    false
}

// --- Errors ------------------------------------------------------------------------

/// Anything loading the config can fail with. No message ever echoes a
/// value from a file (a DSN or token must never reach a terminal or log).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A config file could not be read.
    #[error("{}: cannot read: {message}", path.display())]
    Read { path: PathBuf, message: String },
    /// A config file is invalid (bad TOML, an unknown key, a bad value).
    #[error("{}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
    /// The repo file carries a credential-shaped key.
    #[error(
        "{}: refusing credential-shaped key `{key}`: a committed {REPO_CONFIG_FILENAME} must \
         never hold a DSN, password, token, or secret (set the store in \
         $FLYWHEEL_HOME/config.toml or {PG_DSN_ENV} instead)",
        path.display()
    )]
    Credential { path: PathBuf, key: String },
    /// The repo file is not approved and this verb would run its commands
    /// unattended.
    #[error(
        "{} is not trusted (it is new, or changed since it was approved); review it, then run \
         `fw trust` from {} (cd {} && fw trust)",
        path.display(),
        dir.display(),
        dir.display()
    )]
    Untrusted { path: PathBuf, dir: PathBuf },
    /// The operator declined the trust prompt.
    #[error(
        "{} was not approved; nothing was run (run `fw trust` from {} to approve it)",
        path.display(),
        dir.display()
    )]
    Declined { path: PathBuf, dir: PathBuf },
    /// trust.json could not be read or written.
    #[error(transparent)]
    Trust(#[from] TrustError),
    /// The project could not be resolved.
    #[error(transparent)]
    Project(#[from] ProjectError),
}

// --- Parsing -----------------------------------------------------------------------

fn invalid(path: &Path, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

/// The first credential-shaped key in a repo-file table (dotted path), if
/// any. A key is credential-shaped when it is named `store`, when its name
/// contains `dsn`, `password`, `passwd`, `token`, or `secret`
/// (case-insensitive), when it is a url/uri-named key whose value holds a
/// `scheme://user@` URL, or when any string value holds a
/// `scheme://user:password@` URL. Nested tables and arrays are searched.
pub fn credential_key(table: &Table) -> Option<String> {
    fn name_is_credential(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        lower == "store" || CREDENTIAL_NAME_FRAGMENTS.iter().any(|f| lower.contains(f))
    }
    fn value_is_credential(name: &str, value: &Value) -> bool {
        let lower = name.to_ascii_lowercase();
        let url_named = lower.contains("url") || lower.contains("uri");
        match value {
            Value::String(text) => has_userinfo_url(text, !url_named),
            Value::Array(items) => items.iter().any(|item| value_is_credential(name, item)),
            _ => false,
        }
    }
    fn walk(table: &Table, prefix: &str) -> Option<String> {
        for (name, value) in table {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if name_is_credential(name) || value_is_credential(name, value) {
                return Some(path);
            }
            match value {
                Value::Table(inner) => {
                    if let Some(found) = walk(inner, &path) {
                        return Some(found);
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        if let Value::Table(inner) = item
                            && let Some(found) = walk(inner, &path)
                        {
                            return Some(found);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }
    walk(table, "")
}

/// Parse TOML without ever echoing the source: the `toml` crate's
/// `Display` quotes the offending line, which may hold a credential.
fn parse_table(text: &str, path: &Path) -> Result<Table, ConfigError> {
    toml::from_str::<Table>(text).map_err(|err| {
        let line = err
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
        let message = err.message().trim().to_owned();
        match line {
            Some(line) => invalid(path, format!("invalid TOML at line {line}: {message}")),
            None => invalid(path, format!("invalid TOML: {message}")),
        }
    })
}

/// Parse and validate one config file's text.
pub fn parse_layer(text: &str, path: &Path, kind: LayerKind) -> Result<ConfigLayer, ConfigError> {
    let table = parse_table(text, path)?;
    if kind == LayerKind::Repo
        && let Some(key) = credential_key(&table)
    {
        return Err(ConfigError::Credential {
            path: path.to_path_buf(),
            key,
        });
    }
    let allowed = kind.allowed();
    let unknown: Vec<String> = table
        .iter()
        .filter(|(name, _)| !allowed.contains(&name.as_str()))
        .map(|(name, value)| match value {
            Value::Table(_) => format!("[{name}]"),
            _ => format!("`{name}`"),
        })
        .collect();
    if !unknown.is_empty() {
        let what = match kind {
            LayerKind::Repo => format!("{REPO_CONFIG_FILENAME} accepts only"),
            LayerKind::User | LayerKind::Personal => "a flywheel config accepts only".to_owned(),
        };
        let noun = if unknown.len() == 1 { "key" } else { "keys" };
        return Err(invalid(
            path,
            format!(
                "unknown {noun} {}; {what} {} (old flywheel.toml sections are not read)",
                unknown.join(", "),
                allowed.join(", ")
            ),
        ));
    }

    let mut layer = ConfigLayer::default();
    for (name, value) in &table {
        match name.as_str() {
            "model" => layer.model = Some(non_empty_string(path, name, value)?),
            "concurrency" => layer.concurrency = Some(positive_int(path, name, value)?),
            "store" => layer.store = Some(store_setting(path, value)?),
            "transcript_retention_days" => {
                // 0 keeps transcript bodies forever.
                layer.transcript_retention_days = Some(non_negative_int(path, name, value)?);
            }
            "verify" => {
                let Value::String(command) = value else {
                    return Err(invalid(path, "`verify` must be a string (a shell command)"));
                };
                let command = command.trim();
                layer.verify = Some((!command.is_empty()).then(|| command.to_owned()));
            }
            "protected_paths" => {
                let Value::Array(items) = value else {
                    return Err(invalid(path, "`protected_paths` must be an array of globs"));
                };
                let mut globs = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str().map(str::trim) {
                        Some(glob) if !glob.is_empty() => globs.push(glob.to_owned()),
                        _ => {
                            return Err(invalid(
                                path,
                                "`protected_paths` entries must be non-empty strings",
                            ));
                        }
                    }
                }
                layer.protected_paths = Some(globs);
            }
            "land" => {
                layer.land = Some(match value.as_str() {
                    Some("merge") => Land::Merge,
                    Some("pr") => Land::Pr,
                    _ => return Err(invalid(path, "`land` must be \"merge\" or \"pr\"")),
                });
            }
            "project_id" => {
                let id = value
                    .as_str()
                    .and_then(|s| uuid::Uuid::parse_str(s.trim()).ok())
                    .ok_or_else(|| invalid(path, "`project_id` must be a UUID string"))?;
                layer.project_id = Some(id.hyphenated().to_string());
            }
            _ => unreachable!("unknown keys were rejected above"),
        }
    }
    Ok(layer)
}

fn non_empty_string(path: &Path, name: &str, value: &Value) -> Result<String, ConfigError> {
    match value.as_str().map(str::trim) {
        Some(text) if !text.is_empty() => Ok(text.to_owned()),
        _ => Err(invalid(
            path,
            format!("`{name}` must be a non-empty string"),
        )),
    }
}

fn non_negative_int(path: &Path, name: &str, value: &Value) -> Result<i64, ConfigError> {
    match value.as_integer() {
        Some(n) if n >= 0 => Ok(n),
        _ => Err(invalid(path, format!("`{name}` must be an integer >= 0"))),
    }
}

fn positive_int(path: &Path, name: &str, value: &Value) -> Result<i64, ConfigError> {
    match value.as_integer() {
        Some(n) if n >= 1 => Ok(n),
        _ => Err(invalid(path, format!("`{name}` must be an integer >= 1"))),
    }
}

fn store_setting(path: &Path, value: &Value) -> Result<StoreSetting, ConfigError> {
    let text = value.as_str().map(str::trim).unwrap_or_default();
    if text == DEFAULT_BACKEND {
        return Ok(StoreSetting::Sqlite);
    }
    if text.starts_with("postgres://") || text.starts_with("postgresql://") {
        return Ok(StoreSetting::Postgres(Dsn::new(text)));
    }
    // Never echo the value: it may be a mistyped DSN with a password.
    Err(invalid(
        path,
        "`store` must be \"sqlite\" or a postgres:// (postgresql://) DSN",
    ))
}

/// Read and validate the config file at `path`; `Ok(None)` when absent.
pub fn read_layer(path: &Path, kind: LayerKind) -> Result<Option<ConfigLayer>, ConfigError> {
    match read_text(path)? {
        Some(text) => parse_layer(&text, path, kind).map(Some),
        None => Ok(None),
    }
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            message: err.to_string(),
        }),
    }
}

fn read_text(path: &Path) -> Result<Option<String>, ConfigError> {
    match read_bytes(path)? {
        Some(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| invalid(path, "not valid UTF-8")),
        None => Ok(None),
    }
}

/// The `project_id` a repo's `.flywheel.toml` declares, if it parses (used
/// by `fw project prune` to keep a shared project dir; never errors).
pub fn declared_project_id(repo_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(repo_root.join(REPO_CONFIG_FILENAME)).ok()?;
    let table = toml::from_str::<Table>(&text).ok()?;
    let id = uuid::Uuid::parse_str(table.get("project_id")?.as_str()?.trim()).ok()?;
    Some(id.hyphenated().to_string())
}

// --- The repo file and trust --------------------------------------------------------

/// A parsed, validated repo file and the hash trust is keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFileContents {
    pub path: PathBuf,
    pub text: String,
    pub sha256: String,
    pub layer: ConfigLayer,
}

/// Read and validate `<repo_root>/.flywheel.toml`; `Ok(None)` when absent.
/// Validation does not depend on trust: a malformed or credential-bearing
/// file is an error either way.
pub fn read_repo_file(repo_root: &Path) -> Result<Option<RepoFileContents>, ConfigError> {
    let path = repo_root.join(REPO_CONFIG_FILENAME);
    let Some(bytes) = read_bytes(&path)? else {
        return Ok(None);
    };
    let sha256 = trust::sha256_hex(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| invalid(&path, "not valid UTF-8"))?;
    let layer = parse_layer(&text, &path, LayerKind::Repo)?;
    Ok(Some(RepoFileContents {
        path,
        text,
        sha256,
        layer,
    }))
}

/// What a repo layer would make flywheel run or change, one line each (the
/// trust prompt and `fw trust` show it).
pub fn describe_repo_layer(layer: &ConfigLayer) -> Vec<String> {
    let mut lines = Vec::new();
    match &layer.verify {
        Some(Some(command)) => lines.push(format!(
            "runs `{command}` in the repo before every landing and at every phase exit"
        )),
        Some(None) => lines.push("disables the verify gate".to_owned()),
        None => {}
    }
    if let Some(paths) = &layer.protected_paths {
        lines.push(format!("protects paths: {}", paths.join(", ")));
    }
    if let Some(land) = layer.land {
        lines.push(format!("lands by {}", land.as_str()));
    }
    if let Some(id) = &layer.project_id {
        lines.push(format!("uses project id {id} (shared project dir)"));
    }
    if lines.is_empty() {
        lines.push("sets nothing".to_owned());
    }
    lines
}

/// Whether the verb being run executes repo-file commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads state only: an untrusted repo file is warned about and its
    /// keys do not apply.
    ReadOnly,
    /// Runs `verify` (worker, autopilot, archive, ...): an untrusted repo
    /// file is refused unattended and prompted for on a TTY.
    Execute,
}

/// How the loader may ask about an untrusted repo file.
pub enum Interaction<'a> {
    /// No operator to ask (stdin is not a TTY).
    Unattended,
    /// Show the file on `output` and read one `[y/N]` answer from `input`.
    Prompt {
        input: &'a mut dyn BufRead,
        output: &'a mut dyn Write,
    },
}

/// A loaded config plus the one-line warnings to show the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub config: EffectiveConfig,
    pub warnings: Vec<String>,
}

/// Ask once whether to trust `repo`; `Ok(true)` on `y`/`yes`.
fn prompt_trust(
    repo: &RepoFileContents,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> std::io::Result<bool> {
    writeln!(
        output,
        "fw: {} is not trusted yet (new, or changed since it was approved):",
        repo.path.display()
    )?;
    writeln!(output, "----")?;
    output.write_all(repo.text.as_bytes())?;
    if !repo.text.ends_with('\n') {
        writeln!(output)?;
    }
    writeln!(output, "----")?;
    for line in describe_repo_layer(&repo.layer) {
        writeln!(output, "  it {line}")?;
    }
    write!(output, "Trust this file? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Load the effective config for `project` (resolved from the repo at
/// `repo_root`): the user file, the personal file under the LOCAL project
/// dir, the repo file (validated always, applied only when trusted under
/// the local id), `FLYWHEEL_PG_DSN` from `env_dsn`, and `flags`.
///
/// An untrusted repo file under [`Access::ReadOnly`] adds a warning and is
/// treated as absent; under [`Access::Execute`] it is refused
/// ([`ConfigError::Untrusted`]) when unattended, or prompted for once.
pub fn load_config(
    home: &Home,
    project: &Project,
    repo_root: &Path,
    access: Access,
    flags: &ConfigFlags,
    env_dsn: Option<String>,
    interaction: Interaction<'_>,
) -> Result<LoadedConfig, ConfigError> {
    let user = read_layer(&home.config_file(), LayerKind::User)?;
    let personal = read_layer(&project.paths.config(), LayerKind::Personal)?;
    let mut warnings = Vec::new();
    let mut repo_layer = None;
    let mut repo_file = None;
    if let Some(repo) = read_repo_file(repo_root)? {
        let mut trusted = trust::is_trusted(home, &project.id, &repo.sha256)?;
        if !trusted {
            match (access, interaction) {
                (Access::ReadOnly, _) => warnings.push(format!(
                    "fw: warning: {} is not trusted, so its settings do not apply (run `fw \
                     trust` from {} to approve it)",
                    repo.path.display(),
                    repo_root.display()
                )),
                (Access::Execute, Interaction::Unattended) => {
                    return Err(ConfigError::Untrusted {
                        path: repo.path.clone(),
                        dir: repo_root.to_path_buf(),
                    });
                }
                (Access::Execute, Interaction::Prompt { input, output }) => {
                    let approved =
                        prompt_trust(&repo, input, output).map_err(|err| ConfigError::Read {
                            path: repo.path.clone(),
                            message: format!("trust prompt failed: {err}"),
                        })?;
                    if !approved {
                        return Err(ConfigError::Declined {
                            path: repo.path.clone(),
                            dir: repo_root.to_path_buf(),
                        });
                    }
                    trust::approve(home, &project.id, &repo.sha256, &repo.path)?;
                    trusted = true;
                }
            }
        }
        if trusted {
            repo_layer = Some(repo.layer.clone());
        }
        repo_file = Some(RepoFile {
            path: repo.path,
            sha256: repo.sha256,
            trusted,
        });
    }
    let mut config = resolve(&ConfigInputs {
        user,
        repo: repo_layer,
        personal,
        env_dsn,
        flags: flags.clone(),
        local_project_id: project.id.clone(),
    });
    config.repo_file = repo_file;
    Ok(LoadedConfig { config, warnings })
}

/// `FLYWHEEL_PG_DSN` from the process environment (blank counts as unset).
pub fn env_dsn_from_process() -> Option<String> {
    std::env::var(PG_DSN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Every key the effective config understands, as a set (tests and docs).
pub fn key_set() -> BTreeSet<&'static str> {
    KEYS.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(text: &str, kind: LayerKind) -> Result<ConfigLayer, ConfigError> {
        parse_layer(text, Path::new("/x/f.toml"), kind)
    }

    #[test]
    fn every_key_parses_in_a_user_file() {
        let parsed = layer(
            r#"
model = "claude-x"
concurrency = 3
store = "postgres://u:p@h:5432/db"
transcript_retention_days = 30
verify = "make check"
protected_paths = ["ci/**", "Cargo.lock"]
land = "pr"
project_id = "6F9619FF-8B86-D011-B42D-00C04FC964FF"
"#,
            LayerKind::User,
        )
        .unwrap();
        assert_eq!(parsed.model.as_deref(), Some("claude-x"));
        assert_eq!(parsed.concurrency, Some(3));
        assert_eq!(
            parsed.store,
            Some(StoreSetting::Postgres(Dsn::new("postgres://u:p@h:5432/db")))
        );
        assert_eq!(parsed.transcript_retention_days, Some(30));
        assert_eq!(parsed.verify, Some(Some("make check".into())));
        assert_eq!(parsed.land, Some(Land::Pr));
        assert_eq!(
            parsed.project_id.as_deref(),
            Some("6f9619ff-8b86-d011-b42d-00c04fc964ff")
        );
        assert_eq!(
            layer("verify = \"\"", LayerKind::User).unwrap().verify,
            Some(None)
        );
    }

    #[test]
    fn unknown_keys_and_old_sections_are_named() {
        let err = layer("[sandbox]\npreset = \"fast\"\n", LayerKind::User).unwrap_err();
        assert!(err.to_string().contains("[sandbox]"), "{err}");
        let err = layer("verify = \"x\"\nmodel = \"m\"\n", LayerKind::Repo).unwrap_err();
        assert!(err.to_string().contains("`model`"), "{err}");
        let err = layer("concurrency = 2\n", LayerKind::Repo).unwrap_err();
        assert!(err.to_string().contains("`concurrency`"), "{err}");
        let err = layer(
            "[submit]\nverify = \"x\"\n[source]\nkind = \"d\"\n",
            LayerKind::Repo,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("[submit]") && err.contains("[source]"),
            "{err}"
        );
    }

    #[test]
    fn zero_retention_parses_as_keep_forever() {
        let parsed = layer("transcript_retention_days = 0", LayerKind::User).unwrap();
        assert_eq!(parsed.transcript_retention_days, Some(0));
    }

    #[test]
    fn bad_values_are_refused() {
        for text in [
            "concurrency = 0",
            "concurrency = \"2\"",
            "land = \"squash\"",
            "project_id = \"nope\"",
            "protected_paths = \"a\"",
            "transcript_retention_days = -1",
            "model = \"\"",
            "store = \"mysql://h/db\"",
        ] {
            assert!(layer(text, LayerKind::User).is_err(), "{text}");
        }
        // A bad store value is never echoed.
        let err = layer("store = \"mysql://u:hunter2@h/db\"", LayerKind::User).unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    #[test]
    fn credential_shaped_repo_keys_are_refused_without_echoing_the_value() {
        for (text, key) in [
            ("dsn = \"postgres://u:p@h/db\"", "dsn"),
            ("store = \"sqlite\"", "store"),
            ("api_token = \"abc\"", "api_token"),
            ("[sandbox]\nPassword = \"abc\"", "sandbox.Password"),
            ("client_secret = \"abc\"", "client_secret"),
            ("database_url = \"postgres://admin@h/db\"", "database_url"),
            ("verify = \"curl https://u:pw@example.com/x\"", "verify"),
            ("[[hooks]]\nauth_token = \"z\"", "hooks.auth_token"),
        ] {
            let err = layer(text, LayerKind::Repo).unwrap_err();
            assert!(
                matches!(&err, ConfigError::Credential { key: k, .. } if k == key),
                "{text}: {err:?}"
            );
            let shown = err.to_string();
            assert!(shown.contains(key), "{shown}");
            assert!(!shown.contains("u:p@") && !shown.contains("abc"), "{shown}");
        }
        // A URL without credentials in an allowed key is fine.
        assert!(layer("verify = \"curl https://example.com/x\"", LayerKind::Repo).is_ok());
        assert!(layer("verify = \"git fetch ssh://git@host/r\"", LayerKind::Repo).is_ok());
        // User files may hold the store DSN.
        assert!(layer("store = \"postgres://u:p@h/db\"", LayerKind::User).is_ok());
    }

    #[test]
    fn toml_errors_never_quote_the_source() {
        let err = layer(
            "dsn = \"postgres://u:secretpw@h/db\" garbage",
            LayerKind::Repo,
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("secretpw"), "{err}");
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn precedence_is_flag_env_personal_repo_user_default() {
        let user = ConfigLayer {
            land: Some(Land::Pr),
            concurrency: Some(2),
            model: Some("user-model".into()),
            store: Some(StoreSetting::Postgres(Dsn::new("postgres://a:b@user/db"))),
            ..ConfigLayer::default()
        };
        let repo = ConfigLayer {
            land: Some(Land::Merge),
            verify: Some(Some("make".into())),
            ..ConfigLayer::default()
        };
        let mut inputs = ConfigInputs {
            user: Some(user),
            repo: Some(repo),
            local_project_id: "local".into(),
            ..ConfigInputs::default()
        };
        let config = resolve(&inputs);
        assert_eq!(
            (config.land.value, config.land.source),
            (Land::Merge, Source::Repo)
        );
        assert_eq!(
            (config.concurrency.value, config.concurrency.source),
            (2, Source::User)
        );
        assert_eq!(config.verify.source, Source::Repo);
        assert_eq!(config.project_id.value, "local");
        assert_eq!(config.project_id.source, Source::Default);
        assert_eq!(config.transcript_retention_days.value, 90);

        inputs.personal = Some(ConfigLayer {
            land: Some(Land::Pr),
            ..ConfigLayer::default()
        });
        inputs.env_dsn = Some("postgres://e:f@env/db".into());
        inputs.flags = ConfigFlags {
            model: Some("flag-model".into()),
            concurrency: Some(5),
        };
        let config = resolve(&inputs);
        assert_eq!(
            (config.land.value, config.land.source),
            (Land::Pr, Source::Personal)
        );
        assert_eq!(config.store.source, Source::Env);
        assert_eq!(config.store.value.redacted(), "postgres://e:***@env/db");
        assert_eq!(config.model.value.as_deref(), Some("flag-model"));
        assert_eq!(config.model.source, Source::Flag);
        assert_eq!(config.concurrency.value, 5);
    }

    #[test]
    fn json_has_exactly_the_eight_keys_and_redacts_the_store() {
        let config = resolve(&ConfigInputs {
            user: Some(ConfigLayer {
                store: Some(StoreSetting::Postgres(Dsn::new(
                    "postgres://me:hunter2@db.example:5432/fw",
                ))),
                ..ConfigLayer::default()
            }),
            local_project_id: "id".into(),
            ..ConfigInputs::default()
        });
        let json = config.to_json();
        let keys: BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, key_set());
        assert_eq!(
            json["store"]["value"],
            "postgres://me:***@db.example:5432/fw"
        );
        assert_eq!(json["store"]["source"], "user");
        assert_eq!(json["model"]["value"], Json::Null);
        assert_eq!(json["verify"]["source"], "default");
        assert!(!json.to_string().contains("hunter2"));
        assert!(!format!("{config:?}").contains("hunter2"));
        let text = config.render_text();
        assert_eq!(text.lines().count(), 8);
        assert!(!text.contains("hunter2"));
    }

    #[test]
    fn redaction_handles_urls_and_keyword_dsns() {
        assert_eq!(redact_dsn("postgres://h/db"), "postgres://h/db");
        assert_eq!(redact_dsn("postgres://u@h/db"), "postgres://u@h/db");
        assert_eq!(
            redact_dsn("postgres://u:p@h/db?x=1"),
            "postgres://u:***@h/db?x=1"
        );
        assert_eq!(
            redact_dsn("host=h user=u password=p dbname=d"),
            "host=h user=u password=*** dbname=d"
        );
    }

    #[test]
    fn apply_maps_onto_the_policy() {
        let config = resolve(&ConfigInputs {
            user: Some(ConfigLayer {
                model: Some("m".into()),
                concurrency: Some(4),
                verify: Some(Some("make gate".into())),
                protected_paths: Some(vec!["ci/**".into()]),
                land: Some(Land::Pr),
                store: Some(StoreSetting::Postgres(Dsn::new("postgres://h/db"))),
                transcript_retention_days: Some(0),
                ..ConfigLayer::default()
            }),
            local_project_id: "id".into(),
            ..ConfigInputs::default()
        });
        let mut policy = WorkPolicy::default();
        assert_eq!(policy.transcript_retention_days, 90);
        config.apply_to_policy(&mut policy);
        // 0 keeps transcript bodies forever.
        assert_eq!(policy.transcript_retention_days, 0);
        assert_eq!(policy.model.as_deref(), Some("m"));
        assert_eq!(policy.worker_concurrency, 4);
        assert_eq!(policy.submit_verify.as_deref(), Some("make gate"));
        assert_eq!(policy.phase_verify.as_deref(), Some("make gate"));
        assert_eq!(policy.protected_paths, vec!["ci/**".to_owned()]);
        assert_eq!(policy.submit_strategy, "pr");
        assert_eq!(policy.store_backend, "postgres");
        assert_eq!(
            policy.store_dsn.as_ref().map(Dsn::expose),
            Some("postgres://h/db")
        );
    }
}
