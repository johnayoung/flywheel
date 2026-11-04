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

/// What trusting `folder`'s project turns on that the hub writes only after
/// a typed confirmation: the project's `.codex/` layers' dangerous
/// settings, dangerous allow rules, and anything that runs programs
/// (`notify`, `mcp_servers`, hooks). One line each.
pub fn trust_activates(folder: &Folder, anchors: &super::rule::Anchors) -> Vec<String> {
    let mut out = Vec::new();
    for dir in project_dirs(folder) {
        let codex = dir.join(".codex");
        let config = codex.join("config.toml");
        let (layer, table) = read_layer("project", config.clone());
        let at = config.display();
        if let Some(e) = &layer.error {
            out.push(format!(
                "{at}: cannot be read ({e}), so the hub cannot judge it"
            ));
        }
        for (setting, value) in [
            (Setting::ApprovalPolicy, &layer.approval_policy),
            (Setting::SandboxMode, &layer.sandbox_mode),
        ] {
            if let Some(value) = value
                && let Some(why) = setting.danger(value)
            {
                let key = match setting {
                    Setting::ApprovalPolicy => "approval_policy",
                    _ => "sandbox_mode",
                };
                out.push(format!("{at}: {key} = {value} ({why})"));
            }
        }
        if let Some(table) = &table {
            for key in ["notify", "mcp_servers", "hooks"] {
                if table.contains_key(key) {
                    out.push(format!("{at}: {key} (runs programs it names)"));
                }
            }
        }
        if codex.join("hooks.json").exists() {
            out.push(format!(
                "{}: hooks (run programs it names)",
                codex.join("hooks.json").display()
            ));
        }
        let mut files = Vec::new();
        read_rules("project", &codex.join("rules"), &mut files);
        for file in files {
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
                    out.push(format!(
                        "{}: allow {:?} ({})",
                        file.path.display(),
                        rule.pattern,
                        risk.reason
                    ));
                }
            }
        }
    }
    out
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
