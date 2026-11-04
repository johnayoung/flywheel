//! Codex's approval and sandbox settings for one folder: `approval_policy`
//! and `sandbox_mode` from the session's flags, the project's
//! `.codex/config.toml` layers (read only when the project is trusted),
//! and `$CODEX_HOME/config.toml` with its `[projects."<path>"]
//! trust_level`; the `prefix_rule`s in `rules/*.rules` beside each layer.
//! The hub writes only those documented keys and rules files.

use super::claude::{read_for_edit, write_atomic};
use super::edit;
use super::{Env, Folder};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// `approval_policy` values the hub writes (`untrusted` is unsupported
/// and `on-failure` deprecated in current Codex).
pub const APPROVAL_POLICIES: &[&str] = &["on-request", "never"];
pub const SANDBOX_MODES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];
pub const TRUST_LEVELS: &[&str] = &["trusted", "untrusted"];

/// One `config.toml` layer.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Layer {
    /// `project` or `user`.
    pub scope: &'static str,
    pub path: PathBuf,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
}

/// One rules file and the prefix rules read from it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RulesFile {
    pub scope: &'static str,
    pub path: PathBuf,
    /// Each `prefix_rule(...)`: its pattern words and decision.
    pub rules: Vec<PrefixRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrefixRule {
    pub pattern: Vec<String>,
    pub decision: String,
}

/// Codex's settings as they apply to a folder.
#[derive(Debug, Clone, Default, Serialize)]
pub struct View {
    pub home: PathBuf,
    /// The project key trust is recorded under (the folder's git top, else
    /// the folder).
    pub project: PathBuf,
    /// `trusted`, `untrusted`, or none recorded (Codex asks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_level: Option<String>,
    /// Which `[projects."..."]` entry it came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_from: Option<String>,
    /// Highest precedence first: the session's flags (as a pseudo-layer
    /// with no path), the project layers, the user's config.
    pub layers: Vec<Layer>,
    /// The values in force and where each came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<(String, String)>,
    pub rules: Vec<RulesFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

fn value_text(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn read_layer(scope: &'static str, path: PathBuf) -> (Layer, Option<toml::Table>) {
    let mut layer = Layer {
        scope,
        exists: path.exists(),
        path,
        ..Layer::default()
    };
    if !layer.exists {
        return (layer, None);
    }
    let table = match std::fs::read_to_string(&layer.path)
        .map_err(|e| e.to_string())
        .and_then(|t| {
            t.parse::<toml::Table>()
                .map_err(|e| format!("not valid TOML: {e}"))
        }) {
        Ok(t) => t,
        Err(e) => {
            layer.error = Some(e);
            return (layer, None);
        }
    };
    layer.approval_policy = table.get("approval_policy").map(value_text);
    layer.sandbox_mode = table.get("sandbox_mode").map(value_text);
    (layer, Some(table))
}

/// The `prefix_rule(...)` calls in a rules file (patterns of plain string
/// literals; anything else is listed raw).
pub fn parse_rules(text: &str) -> Vec<PrefixRule> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("prefix_rule(") {
        let body = &rest[at + "prefix_rule(".len()..];
        let end = body.find(')').unwrap_or(body.len());
        let call = &body[..end];
        let pattern = call
            .find("pattern")
            .and_then(|p| {
                let after = &call[p..];
                let open = after.find('[')?;
                let close = after[open..].find(']')? + open;
                serde_json::from_str::<Vec<String>>(&after[open..=close]).ok()
            })
            .unwrap_or_default();
        let decision = call
            .find("decision")
            .and_then(|d| {
                let after = &call[d..];
                let q = after.find('"')?;
                let e = after[q + 1..].find('"')? + q + 1;
                Some(after[q + 1..e].to_owned())
            })
            .unwrap_or_else(|| "allow".to_owned());
        if !pattern.is_empty() {
            out.push(PrefixRule { pattern, decision });
        }
        rest = &body[end..];
    }
    out
}

fn read_rules(scope: &'static str, dir: &Path, out: &mut Vec<RulesFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rules"))
        .collect();
    paths.sort();
    for path in paths {
        let mut file = RulesFile {
            scope,
            path: path.clone(),
            ..RulesFile::default()
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => file.rules = parse_rules(&text),
            Err(e) => file.error = Some(e.to_string()),
        }
        out.push(file);
    }
}

/// The project directories whose `.codex/` layers apply, outermost first:
/// the git top down to the folder.
fn project_dirs(folder: &Folder) -> Vec<PathBuf> {
    let top = folder.top.clone().unwrap_or_else(|| folder.dir.clone());
    let mut dirs = vec![folder.dir.clone()];
    let mut at = folder.dir.as_path();
    while at != top {
        match at.parent() {
            Some(p) if p.starts_with(&top) => {
                dirs.push(p.to_path_buf());
                at = p;
            }
            _ => break,
        }
    }
    dirs.reverse();
    dirs
}

/// What trusting a project turns on: every `.codex/` directory under the
/// trusted root (each applies once a session runs in or below its folder)
/// and, of those, what the hub writes only after a typed confirmation.
#[derive(Debug, Clone, Default)]
pub struct Activation {
    /// The `.codex/` directories trust would load, outermost first.
    pub dirs: Vec<PathBuf>,
    /// One line per setting that disables a safeguard, runs a program,
    /// or sends data or credentials somewhere; any line means typing.
    pub dangers: Vec<String>,
}

/// Directories skipped when looking for nested `.codex/`: git's own data
/// and dependency/build trees no session is started in.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target"];
/// Bounds on the walk: past either, the preview says the scan stopped
/// (which itself needs the typed confirmation).
const SCAN_MAX_DEPTH: usize = 12;
const SCAN_MAX_DIRS: usize = 20_000;

/// Every `.codex/` directory under `root` (not following symlinks),
/// sorted; `Err` carries the ones found when a bound stopped the walk.
fn codex_dirs_under(root: &Path) -> Result<Vec<PathBuf>, (Vec<PathBuf>, String)> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    let mut stopped = None;
    while let Some((dir, depth)) = stack.pop() {
        visited += 1;
        if visited > SCAN_MAX_DIRS {
            stopped = Some(format!("stopped after {SCAN_MAX_DIRS} folders"));
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            if name == ".codex" && (kind.is_dir() || kind.is_symlink()) {
                found.push(entry.path());
                continue;
            }
            if !kind.is_dir() || SKIP_DIRS.iter().any(|s| name == *s) {
                continue;
            }
            if depth + 1 > SCAN_MAX_DEPTH {
                stopped = Some(format!("not looked below {SCAN_MAX_DEPTH} levels"));
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
    found.sort();
    match stopped {
        None => Ok(found),
        Some(why) => Err((found, why)),
    }
}

fn str_list(value: Option<&toml::Value>) -> Option<String> {
    let value = value?;
    Some(match value {
        toml::Value::Array(items) => items.iter().map(value_text).collect::<Vec<_>>().join(" "),
        other => value_text(other),
    })
}

/// The dangers in one table of a Codex config: the top level (`scope`
/// empty) or a `[profiles.<name>]` table. `providers` resolves a
/// `model_provider` to its `[model_providers.<id>]` entry.
fn table_dangers(
    at: &str,
    scope: &str,
    table: &toml::Table,
    providers: Option<&toml::Table>,
    out: &mut Vec<String>,
) {
    for (setting, key) in [
        (Setting::ApprovalPolicy, "approval_policy"),
        (Setting::SandboxMode, "sandbox_mode"),
    ] {
        if let Some(value) = table.get(key).map(value_text)
            && let Some(why) = setting.danger(&value)
        {
            out.push(format!("{at}: {scope}{key} = {value} ({why})"));
        }
    }
    if let Some(write) = table
        .get("sandbox_workspace_write")
        .and_then(|v| v.as_table())
    {
        if write.get("network_access").and_then(|v| v.as_bool()) == Some(true) {
            out.push(format!(
                "{at}: {scope}sandbox_workspace_write.network_access = true (sandboxed commands reach the network)"
            ));
        }
        if let Some(roots) = str_list(write.get("writable_roots")).filter(|r| !r.is_empty()) {
            out.push(format!(
                "{at}: {scope}sandbox_workspace_write.writable_roots = {roots} (writes outside the workspace without asking)"
            ));
        }
    }
    if let Some(id) = table.get("model_provider").map(value_text) {
        let entry = providers
            .and_then(|p| p.get(&id))
            .and_then(|v| v.as_table());
        let target = entry
            .and_then(|e| e.get("base_url"))
            .map(value_text)
            .map(|u| format!(" at {u}"))
            .unwrap_or_default();
        out.push(format!(
            "{at}: {scope}model_provider = {id} (sends every prompt and the code it reads to that provider{target})"
        ));
    }
}

/// Everything in one `.codex/config.toml` that trust would turn on and
/// that needs typing. `user` (the user's config) resolves a `profile` or
/// `model_provider` the project names but does not define.
fn config_dangers(at: &str, table: &toml::Table, user: Option<&toml::Table>) -> Vec<String> {
    let mut out = Vec::new();
    let mut providers = user
        .and_then(|u| u.get("model_providers"))
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();
    if let Some(own) = table.get("model_providers").and_then(|v| v.as_table()) {
        providers.extend(own.clone());
    }
    table_dangers(at, "", table, Some(&providers), &mut out);
    let profiles = table.get("profiles").and_then(|v| v.as_table());
    let selected = table.get("profile").map(value_text);
    if let Some(name) = &selected
        && profiles.and_then(|p| p.get(name)).is_none()
        && let Some(theirs) = user
            .and_then(|u| u.get("profiles"))
            .and_then(|p| p.get(name))
            .and_then(|v| v.as_table())
    {
        let how = format!("profile = {name} selects the user's [profiles.{name}], whose ");
        table_dangers(at, &how, theirs, Some(&providers), &mut out);
    }
    for (name, profile) in profiles.into_iter().flatten() {
        let Some(profile) = profile.as_table() else {
            continue;
        };
        let how = if selected.as_deref() == Some(name.as_str()) {
            format!("[profiles.{name}] (selected) ")
        } else {
            format!("[profiles.{name}] (with --profile {name}) ")
        };
        table_dangers(at, &how, profile, Some(&providers), &mut out);
    }
    let own = table.get("model_providers").and_then(|v| v.as_table());
    for (id, provider) in own.into_iter().flatten() {
        let Some(provider) = provider.as_table() else {
            continue;
        };
        let url = provider
            .get("base_url")
            .map(value_text)
            .unwrap_or_else(|| "its default URL".to_owned());
        let mut creds: Vec<String> = Vec::new();
        if let Some(key) = provider.get("env_key").map(value_text) {
            creds.push(format!("${key}"));
        }
        if let Some(headers) = provider.get("env_http_headers").and_then(|v| v.as_table()) {
            creds.extend(headers.values().map(|v| format!("${}", value_text(v))));
        }
        let creds = if creds.is_empty() {
            String::new()
        } else {
            format!(", with {} from the environment", creds.join(", "))
        };
        out.push(format!(
            "{at}: model_providers.{id} (sends prompts and code to {url}{creds})"
        ));
    }
    if let Some(command) = str_list(table.get("notify")) {
        out.push(format!("{at}: notify runs `{command}` after every turn"));
    }
    for (name, server) in table
        .get("mcp_servers")
        .and_then(|v| v.as_table())
        .into_iter()
        .flatten()
    {
        let server = server.as_table();
        let command = server.and_then(|s| str_list(s.get("command")));
        let args = server
            .and_then(|s| str_list(s.get("args")))
            .filter(|a| !a.is_empty());
        let url = server.and_then(|s| s.get("url")).map(value_text);
        let what = match (command, url) {
            (Some(c), _) => match args {
                Some(a) => format!("runs `{c} {a}`"),
                None => format!("runs `{c}`"),
            },
            (None, Some(u)) => format!("connects to {u}"),
            (None, None) => "an MCP server".to_owned(),
        };
        let token = server
            .and_then(|s| s.get("bearer_token_env_var"))
            .map(|v| format!(", sending ${}", value_text(v)))
            .unwrap_or_default();
        out.push(format!("{at}: mcp_servers.{name} {what}{token}"));
    }
    if table.contains_key("hooks") {
        out.push(format!("{at}: hooks (run programs they name)"));
    }
    out
}

/// What trusting `folder`'s project turns on: the `.codex/` directories
/// from the git top down to the folder and every other one under the
/// trusted root (the same trust covers a session started there), with the
/// dangerous settings, program-running and data-sending entries
/// (`profile`/`[profiles.*]`, `model_provider`/`[model_providers.*]`,
/// `[mcp_servers.*]`, `notify`, hooks), and dangerous allow rules.
pub fn trust_activates(folder: &Folder, env: &Env, anchors: &super::rule::Anchors) -> Activation {
    let root = project_key(folder);
    let (_, user) = read_layer("user", env.codex_home.join("config.toml"));
    let mut act = Activation::default();
    let mut dirs: Vec<PathBuf> = project_dirs(folder)
        .into_iter()
        .map(|d| d.join(".codex"))
        .filter(|d| d.exists())
        .collect();
    match codex_dirs_under(&root) {
        Ok(found) => dirs.extend(found),
        Err((found, why)) => {
            dirs.extend(found);
            act.dangers.push(format!(
                "{}: the scan for nested .codex/ directories {why}, so trust may turn on more than listed",
                root.display()
            ));
        }
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    for codex in &dirs {
        if std::fs::symlink_metadata(codex).is_ok_and(|m| m.file_type().is_symlink()) {
            act.dangers.push(format!(
                "{}: a symlink (to {}), so its contents can change after this preview",
                codex.display(),
                std::fs::read_link(codex)
                    .map(|t| t.display().to_string())
                    .unwrap_or_else(|_| "?".into())
            ));
        }
        let config = codex.join("config.toml");
        let (layer, table) = read_layer("project", config.clone());
        let at = config.display().to_string();
        if let Some(e) = &layer.error {
            act.dangers.push(format!(
                "{at}: cannot be read ({e}), so the hub cannot judge it"
            ));
        }
        if let Some(table) = &table {
            act.dangers
                .extend(config_dangers(&at, table, user.as_ref()));
        }
        if codex.join("hooks.json").exists() {
            act.dangers.push(format!(
                "{}: hooks (run programs they name)",
                codex.join("hooks.json").display()
            ));
        }
        let mut files = Vec::new();
        read_rules("project", &codex.join("rules"), &mut files);
        for file in files {
            if let Some(e) = &file.error {
                act.dangers.push(format!(
                    "{}: cannot be read ({e}), so the hub cannot judge it",
                    file.path.display()
                ));
            }
            for rule in file.rules.iter().filter(|r| r.decision == "allow") {
                let risk = super::synth::codex_candidate(
                    rule.pattern.clone(),
                    "custom",
                    anchors,
                    folder.repo(),
                )
                .map(|c| c.risk)
                .unwrap_or_else(|e| super::rule::Risk {
                    dangerous: true,
                    reason: e,
                });
                if risk.dangerous {
                    act.dangers.push(format!(
                        "{}: allow {:?} ({})",
                        file.path.display(),
                        rule.pattern,
                        risk.reason
                    ));
                }
            }
        }
    }
    act.dirs = dirs;
    act
}

/// The project key trust is recorded under.
pub fn project_key(folder: &Folder) -> PathBuf {
    folder.top.clone().unwrap_or_else(|| folder.dir.clone())
}

/// Codex's settings for `folder` (`launch`: the session's
/// `--ask-for-approval` and `--sandbox`).
pub fn view(folder: &Folder, env: &Env, launch: Option<(Option<&str>, Option<&str>)>) -> View {
    let mut view = View {
        home: env.codex_home.clone(),
        project: project_key(folder),
        ..View::default()
    };
    let (user, user_table) = read_layer("user", env.codex_home.join("config.toml"));
    // Trust: the nearest recorded entry for the project or a folder above it.
    if let Some(projects) = user_table
        .as_ref()
        .and_then(|t| t.get("projects"))
        .and_then(|p| p.as_table())
    {
        let mut best: Option<(usize, String, String)> = None;
        for (key, entry) in projects {
            let Some(level) = entry.get("trust_level").and_then(|v| v.as_str()) else {
                continue;
            };
            let path = Path::new(key);
            if view.project.starts_with(path) || folder.dir.starts_with(path) {
                let depth = path.components().count();
                if best.as_ref().is_none_or(|(d, _, _)| depth > *d) {
                    best = Some((depth, level.to_owned(), key.clone()));
                }
            }
        }
        if let Some((_, level, key)) = best {
            view.trust_level = Some(level);
            view.trust_from = Some(key);
        }
    }
    let trusted = view.trust_level.as_deref() == Some("trusted");
    if let Some((approval, sandbox)) = launch
        && (approval.is_some() || sandbox.is_some())
    {
        view.layers.push(Layer {
            scope: "command line",
            exists: true,
            approval_policy: approval.map(str::to_owned),
            sandbox_mode: sandbox.map(str::to_owned),
            ..Layer::default()
        });
    }
    // Project layers: the one closest to the folder wins.
    let mut project_layers: Vec<Layer> = project_dirs(folder)
        .into_iter()
        .map(|dir| read_layer("project", dir.join(".codex/config.toml")).0)
        .filter(|l| l.exists)
        .collect();
    project_layers.reverse();
    if !project_layers.is_empty() && !trusted {
        view.notes.push(
            "the project's .codex/ layers (config, rules) load only once the project is trusted"
                .into(),
        );
    }
    view.layers.extend(project_layers);
    view.layers.push(user);
    for layer in &view.layers {
        if layer.scope == "project" && !trusted {
            continue;
        }
        let from = || match layer.scope {
            "command line" => "the session's flags".to_owned(),
            _ => layer.path.display().to_string(),
        };
        if view.approval_policy.is_none()
            && let Some(v) = &layer.approval_policy
        {
            view.approval_policy = Some((v.clone(), from()));
        }
        if view.sandbox_mode.is_none()
            && let Some(v) = &layer.sandbox_mode
        {
            view.sandbox_mode = Some((v.clone(), from()));
        }
    }
    read_rules("user", &env.codex_home.join("rules"), &mut view.rules);
    for dir in project_dirs(folder) {
        read_rules("project", &dir.join(".codex/rules"), &mut view.rules);
    }
    view
}

/// A Codex setting the hub can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// `[projects."<project>"] trust_level` in the user's config.
    TrustLevel,
    /// `approval_policy` in the project's `.codex/config.toml`.
    ApprovalPolicy,
    /// `sandbox_mode` in the project's `.codex/config.toml`.
    SandboxMode,
}

impl Setting {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "trust_level" => Ok(Self::TrustLevel),
            "approval_policy" => Ok(Self::ApprovalPolicy),
            "sandbox_mode" => Ok(Self::SandboxMode),
            other => Err(format!(
                "unknown Codex setting {other:?}: trust_level, approval_policy, or sandbox_mode"
            )),
        }
    }

    fn allowed(self) -> &'static [&'static str] {
        match self {
            Self::TrustLevel => TRUST_LEVELS,
            Self::ApprovalPolicy => APPROVAL_POLICIES,
            Self::SandboxMode => SANDBOX_MODES,
        }
    }

    /// Why a value needs a typed confirmation (`None`: a confirm is enough).
    pub fn danger(self, value: &str) -> Option<&'static str> {
        match (self, value) {
            (Self::ApprovalPolicy, "never") => {
                Some("Codex will never ask: every command runs without approval")
            }
            (Self::SandboxMode, "danger-full-access") => {
                Some("Codex runs commands with no sandbox: full disk and network access")
            }
            _ => None,
        }
    }

    pub fn sentence(self, value: &str, project: &Path) -> String {
        match (self, value) {
            (Self::TrustLevel, "trusted") => format!(
                "Codex will load {}'s own .codex/ config, rules, and hooks, and stop asking whether to trust it",
                project.display()
            ),
            (Self::TrustLevel, _) => {
                format!("Codex will skip {}'s .codex/ layers", project.display())
            }
            (Self::ApprovalPolicy, "on-request") => {
                "Codex asks only when a command needs to leave its sandbox".to_owned()
            }
            (Self::ApprovalPolicy, _) => "Codex never asks for approval".to_owned(),
            (Self::SandboxMode, "read-only") => {
                "Codex may read files but must ask to change any".to_owned()
            }
            (Self::SandboxMode, "workspace-write") => {
                "Codex may change files in the workspace without asking (no network by default)"
                    .to_owned()
            }
            (Self::SandboxMode, _) => {
                "Codex runs everything with full access to this machine".to_owned()
            }
        }
    }
}

/// The file and table a setting is written in.
pub fn setting_place(
    setting: Setting,
    folder: &Folder,
    env: &Env,
) -> (PathBuf, Vec<String>, &'static str) {
    match setting {
        Setting::TrustLevel => (
            env.codex_home.join("config.toml"),
            vec![
                "projects".to_owned(),
                project_key(folder).to_string_lossy().into_owned(),
            ],
            "trust_level",
        ),
        Setting::ApprovalPolicy => (
            project_key(folder).join(".codex/config.toml"),
            Vec::new(),
            "approval_policy",
        ),
        Setting::SandboxMode => (
            project_key(folder).join(".codex/config.toml"),
            Vec::new(),
            "sandbox_mode",
        ),
    }
}

/// The edit setting `setting` to `value` makes: `(path, old, new)`.
pub fn plan_setting(
    setting: Setting,
    value: &str,
    folder: &Folder,
    env: &Env,
) -> Result<(PathBuf, Option<String>, String), String> {
    if !setting.allowed().contains(&value) {
        return Err(format!(
            "{value:?} is not one of {}",
            setting.allowed().join(", ")
        ));
    }
    let (path, table, key) = setting_place(setting, folder, env);
    if setting != Setting::TrustLevel {
        no_symlink_in_project(folder, &path)?;
    }
    let old = read_for_edit(&path)?;
    let new = edit::set_toml_key(
        old.as_deref().unwrap_or(""),
        &table,
        key,
        &edit::toml_string(value),
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, old, new))
}

/// Refuse a project path reached through a symlink (`.codex -> ~/.codex`
/// would turn a project write into a user-wide one).
pub fn no_symlink_in_project(folder: &Folder, path: &Path) -> Result<(), String> {
    let root = project_key(folder);
    let Ok(rest) = path.strip_prefix(&root) else {
        return Err(format!("{} is outside {}", path.display(), root.display()));
    };
    let mut at = root.clone();
    for part in rest.components() {
        at.push(part);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{} is a symlink: edit it by hand", at.display()));
        }
    }
    Ok(())
}

/// Where a prefix rule goes: the project's (`project`) or the user's
/// (`user`) `rules/default.rules` -- the file Codex itself writes.
pub fn rules_path(user: bool, folder: &Folder, env: &Env) -> PathBuf {
    if user {
        env.codex_home.join("rules/default.rules")
    } else {
        project_key(folder).join(".codex/rules/default.rules")
    }
}

/// The edit appending `lines` (prefix rules) to the rules file at `path`
/// makes: `(old, new, lines added)`; a rule already there is skipped.
pub fn plan_rules(
    path: &Path,
    rules: &[(Vec<String>, String)],
) -> Result<(Option<String>, String, Vec<String>), String> {
    let old = read_for_edit(path)?;
    let text = old.clone().unwrap_or_default();
    let present = parse_rules(&text);
    let mut new = text.clone();
    let mut added = Vec::new();
    for (pattern, line) in rules {
        if present
            .iter()
            .any(|r| &r.pattern == pattern && r.decision == "allow")
            || added.contains(line)
        {
            continue;
        }
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(line);
        new.push('\n');
        added.push(line.clone());
    }
    Ok((old, new, added))
}

pub fn write(path: &Path, text: &str) -> Result<(), String> {
    write_atomic(path, text)
}
