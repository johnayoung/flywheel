//! Variants: a declared agent setup, resolved into content and hashed, so
//! every trial records exactly what its agent saw.
//!
//! ```toml
//! # evals/variants/rule-off.toml
//! agent = "claude"                     # claude | codex
//! model = "opus"
//! effort = "high"                      # optional
//! fw = "current"                       # current | /path/to/fw
//! permission_mode = "bypassPermissions"
//! setting_sources = ["project", "local"]   # never "user": user layers come in below
//! settings = "settings/strict.json"    # becomes .claude/settings.local.json (hooks, env, permissions)
//! [env]                                # set on the worker (the agent inherits it)
//! MAX_THINKING_TOKENS = "8000"
//! [[repo]]                             # applied to each task's base as one overlay commit
//! path = "CLAUDE.md"
//! op = "strip"                         # replace | append | ensure | strip | delete | replace_dir
//! pattern = '(?ms)^\*\*Exception: flywheel worker runs\*\*.*?\n\n'
//! [user]                               # what of the user's own setup comes along
//! claude_md = "inherit"                # none | inherit | PATH  -> CLAUDE.local.md
//! skills = ["fw-plan"]                 # "none" | "inherit" | [names] | PATH  -> .claude/skills/
//! mcp_servers = ["context7"]           # from ~/.claude.json -> .mcp.json (strict)
//! ```
//!
//! Claude trials never load a user-level layer: the worker runs with
//! `--setting-sources project,local` and the user's own CLAUDE.md, skills,
//! and MCP servers come in only as the overlay files the variant names.
//! Credentials are untouched (Claude Code keeps refreshing them where they
//! are). [`canary`] proves, before a replay, that this build of Claude Code
//! still honors the flag. Codex has no such switch: a Codex variant (v1)
//! may set model, effort, env, and repo overlays only, and its user files
//! are hashed so a change shows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::git;
use crate::set::{SetError, files_under, io, validate_name};

/// The file a variant's settings become in the overlay.
pub const SETTINGS_LOCAL: &str = ".claude/settings.local.json";
/// The file the user's CLAUDE.md becomes in the overlay.
pub const CLAUDE_LOCAL_MD: &str = "CLAUDE.local.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// The file becomes `from`'s content.
    Replace,
    /// `from`'s content is appended.
    Append,
    /// `from`'s content is appended unless `pattern` (or the content
    /// itself) is already there.
    Ensure,
    /// Every match of `pattern` (or `from`'s exact content) is removed.
    Strip,
    /// The file is removed.
    Delete,
    /// The directory becomes `from`'s directory.
    ReplaceDir,
}

/// One `[[repo]]` entry as written.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoOpSpec {
    pub path: String,
    pub op: OpKind,
    #[serde(default)]
    pub from: Option<PathBuf>,
    #[serde(default)]
    pub pattern: Option<String>,
}

/// `none`, `inherit`, or a path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Layer {
    #[default]
    None,
    Inherit,
    Path(PathBuf),
}

impl<'de> Deserialize<'de> for Layer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "none" => Self::None,
            "inherit" => Self::Inherit,
            _ => Self::Path(PathBuf::from(s)),
        })
    }
}

/// `"none"`, `"inherit"`, a list of skill names, or a skills directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SkillsSpec {
    #[default]
    None,
    Inherit,
    Named(Vec<String>),
    Dir(PathBuf),
}

impl<'de> Deserialize<'de> for SkillsSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Many(Vec<String>),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Many(names) => Self::Named(names),
            Raw::One(s) if s == "none" => Self::None,
            Raw::One(s) if s == "inherit" => Self::Inherit,
            Raw::One(s) => Self::Dir(PathBuf::from(s)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserSpec {
    #[serde(default)]
    pub claude_md: Layer,
    #[serde(default)]
    pub skills: SkillsSpec,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
}

/// A variant file as written.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantFile {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub fw: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub setting_sources: Option<Vec<String>>,
    #[serde(default)]
    pub settings: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub repo: Vec<RepoOpSpec>,
    #[serde(default)]
    pub user: UserSpec,
}

fn default_agent() -> String {
    "claude".to_owned()
}

/// One resolved repo op: its content pinned by blob digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoOp {
    pub path: String,
    pub op: OpKind,
    /// The content blob (`replace`, `append`, `ensure`, `strip` without a
    /// pattern).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// A directory's files, each relative path to its blob (`replace_dir`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tree: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// A variant with every file it names read, pinned, and hashed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedVariant {
    pub name: String,
    /// `claude-code` or `codex` (the worker's `--agent`).
    pub agent: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The `fw` binary trials run, and its digest and version.
    pub fw_path: PathBuf,
    pub fw_sha256: String,
    pub fw_version: String,
    /// `claude --version` (or `codex --version`) when resolved.
    pub agent_cli_version: String,
    pub permission_mode: String,
    /// The worker's `--setting-sources` (Claude only).
    pub setting_sources: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The `.claude/settings.local.json` content blob (Claude only).
    pub settings_blob: Option<String>,
    pub repo: Vec<RepoOp>,
    /// The user's CLAUDE.md, as `CLAUDE.local.md` (blob).
    pub claude_md_blob: Option<String>,
    /// Skill name -> its files (relative path -> blob).
    pub skills: BTreeMap<String, BTreeMap<String, String>>,
    /// MCP server name -> its config.
    pub mcp_servers: BTreeMap<String, serde_json::Value>,
    /// Digests of user files a Codex agent reads that the variant cannot
    /// leave out (so a change between runs shows).
    pub unisolated: BTreeMap<String, String>,
    /// The content hash: everything above but `name`.
    pub hash: String,
}

impl ResolvedVariant {
    pub fn short_hash(&self) -> &str {
        &self.hash[..self.hash.len().min(12)]
    }

    pub fn is_claude(&self) -> bool {
        self.agent == "claude-code"
    }
}

/// Where a variant resolves against.
#[derive(Debug, Clone)]
pub struct ResolveEnv {
    /// The variant file's directory (relative paths resolve here).
    pub base_dir: PathBuf,
    /// The Claude Code config dir (`$CLAUDE_CONFIG_DIR`, default `~/.claude`).
    pub claude_dir: PathBuf,
    /// The user's `~/.claude.json` (MCP servers).
    pub claude_json: PathBuf,
    /// `$CODEX_HOME` (default `~/.codex`).
    pub codex_home: PathBuf,
    /// The running `fw`, for `fw = "current"`.
    pub current_fw: PathBuf,
    /// Where pinned contents are kept (`evals/blobs`).
    pub blobs: PathBuf,
}

impl ResolveEnv {
    /// The user's real config locations.
    pub fn from_process(base_dir: &Path, blobs: &Path) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set")?;
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let claude_json = if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
            claude_dir.join(".claude.json")
        } else {
            home.join(".claude.json")
        };
        Ok(Self {
            base_dir: base_dir.to_owned(),
            claude_dir,
            claude_json,
            codex_home: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            current_fw: std::env::current_exe().map_err(|e| e.to_string())?,
            blobs: blobs.to_owned(),
        })
    }
}

/// Keep `bytes` in the blob store; its digest.
pub fn put_blob(blobs: &Path, bytes: &[u8]) -> Result<String, SetError> {
    let digest = hex::encode(Sha256::digest(bytes));
    let path = blobs.join(&digest[..2]).join(&digest);
    if !path.exists() {
        std::fs::create_dir_all(path.parent().expect("has a parent")).map_err(io(blobs))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes).map_err(io(&tmp))?;
        std::fs::rename(&tmp, &path).map_err(io(&path))?;
    }
    Ok(digest)
}

/// A blob's bytes.
pub fn get_blob(blobs: &Path, digest: &str) -> Result<Vec<u8>, SetError> {
    let path = blobs.join(&digest[..2.min(digest.len())]).join(digest);
    std::fs::read(&path).map_err(io(&path))
}

fn put_file(blobs: &Path, path: &Path) -> Result<String, SetError> {
    let bytes = std::fs::read(path).map_err(io(path))?;
    put_blob(blobs, &bytes)
}

fn put_dir(blobs: &Path, dir: &Path) -> Result<BTreeMap<String, String>, SetError> {
    let mut tree = BTreeMap::new();
    for rel in files_under(dir)? {
        tree.insert(rel.display().to_string(), put_file(blobs, &dir.join(&rel))?);
    }
    if tree.is_empty() {
        return Err(SetError::Invalid(format!(
            "{} holds no files",
            dir.display()
        )));
    }
    Ok(tree)
}

fn invalid<T>(message: impl Into<String>) -> Result<T, SetError> {
    Err(SetError::Invalid(message.into()))
}

fn safe_rel(path: &str) -> Result<(), SetError> {
    let p = Path::new(path);
    if path.is_empty()
        || p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || path.starts_with(".git/")
        || path == ".git"
    {
        return invalid(format!(
            "repo path {path:?}: a relative path inside the repo"
        ));
    }
    Ok(())
}

fn command_version(program: &Path, arg: &str) -> String {
    std::process::Command::new(program)
        .arg(arg)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Resolve `file` (named `name`) against `env`. `agent_cli` is the agent
/// CLI to read the version of (`claude`/`codex` on PATH by default).
pub fn resolve(
    name: &str,
    file: &VariantFile,
    env: &ResolveEnv,
    agent_cli: Option<&Path>,
) -> Result<ResolvedVariant, SetError> {
    validate_name(name).map_err(SetError::Invalid)?;
    let at = |p: &Path| -> PathBuf {
        if p.is_absolute() {
            p.to_owned()
        } else {
            env.base_dir.join(p)
        }
    };
    let agent = match file.agent.as_str() {
        "claude" | "claude-code" => "claude-code",
        "codex" => "codex",
        other => return invalid(format!("agent {other:?}: claude or codex")),
    };
    let claude = agent == "claude-code";
    if !claude {
        let user_level = file.setting_sources.is_some()
            || file.settings.is_some()
            || file.user != UserSpec::default()
            || file.permission_mode.is_some();
        if user_level {
            return invalid(
                "a codex variant takes model, effort, env, and repo overlays only: Codex \
                 cannot leave out user-level layers, so a variant cannot pin them",
            );
        }
    }
    let setting_sources = file
        .setting_sources
        .clone()
        .unwrap_or_else(|| vec!["project".to_owned(), "local".to_owned()]);
    if claude {
        if setting_sources.iter().any(|s| s == "user") {
            return invalid(
                "setting_sources must not include \"user\": user-level layers come in only as \
                 [user] overlays, so the trial's setup is pinned",
            );
        }
        for s in &setting_sources {
            if !["project", "local"].contains(&s.as_str()) {
                return invalid(format!("setting_sources {s:?}: project or local"));
            }
        }
    }
    let permission_mode = file
        .permission_mode
        .clone()
        .unwrap_or_else(|| "bypassPermissions".to_owned());
    if !["bypassPermissions", "plan", "default"].contains(&permission_mode.as_str()) {
        return invalid(format!(
            "permission_mode {permission_mode:?}: bypassPermissions, plan, or default"
        ));
    }
    for key in file.env.keys() {
        if key.starts_with("FLYWHEEL_") || key == "CLAUDE_CONFIG_DIR" || key == "CODEX_HOME" {
            return invalid(format!(
                "env {key}: flywheel and agent-home variables are the trial's own"
            ));
        }
    }

    let fw_path = match file.fw.as_deref() {
        None | Some("current") => env.current_fw.clone(),
        Some(path) => at(Path::new(path)),
    };
    if !fw_path.is_file() {
        return invalid(format!("fw {}: no such binary", fw_path.display()));
    }
    let fw_sha256 = hex::encode(Sha256::digest(
        std::fs::read(&fw_path).map_err(io(&fw_path))?,
    ));
    let fw_version = command_version(&fw_path, "--version");
    let cli = agent_cli
        .map(Path::to_owned)
        .unwrap_or_else(|| PathBuf::from(if claude { "claude" } else { "codex" }));
    let agent_cli_version = command_version(&cli, "--version");

    let settings_blob = match &file.settings {
        None => None,
        Some(path) => {
            let path = at(path);
            let text = std::fs::read_to_string(&path).map_err(io(&path))?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| SetError::Invalid(format!("{}: {e}", path.display())))?;
            if !value.is_object() {
                return invalid(format!("{}: a JSON object", path.display()));
            }
            Some(put_blob(&env.blobs, text.as_bytes())?)
        }
    };

    let mut repo = Vec::new();
    for spec in &file.repo {
        safe_rel(&spec.path)?;
        if let Some(p) = &spec.pattern {
            Regex::new(p).map_err(|e| SetError::Invalid(format!("pattern {p:?}: {e}")))?;
        }
        let needs_from = matches!(
            spec.op,
            OpKind::Replace | OpKind::Append | OpKind::ReplaceDir
        ) || (matches!(spec.op, OpKind::Ensure) && spec.from.is_none());
        if needs_from && spec.from.is_none() {
            return invalid(format!("repo {} {:?}: needs from", spec.path, spec.op));
        }
        if matches!(spec.op, OpKind::Strip) && spec.from.is_none() && spec.pattern.is_none() {
            return invalid(format!("repo {} strip: needs from or pattern", spec.path));
        }
        let (blob, tree) = match (&spec.from, spec.op) {
            (Some(from), OpKind::ReplaceDir) => (None, put_dir(&env.blobs, &at(from))?),
            (Some(from), _) => (Some(put_file(&env.blobs, &at(from))?), BTreeMap::new()),
            (None, _) => (None, BTreeMap::new()),
        };
        repo.push(RepoOp {
            path: spec.path.clone(),
            op: spec.op,
            blob,
            tree,
            pattern: spec.pattern.clone(),
        });
    }

    let claude_md_blob = match &file.user.claude_md {
        Layer::None => None,
        Layer::Inherit => {
            let path = env.claude_dir.join("CLAUDE.md");
            if path.is_file() {
                Some(put_file(&env.blobs, &path)?)
            } else {
                None
            }
        }
        Layer::Path(p) => Some(put_file(&env.blobs, &at(p))?),
    };

    let mut skills = BTreeMap::new();
    let user_skills = env.claude_dir.join("skills");
    let mut take_skill = |dir: &Path| -> Result<(), SetError> {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| SetError::Invalid(format!("skill dir {}", dir.display())))?
            .to_owned();
        skills.insert(name, put_dir(&env.blobs, dir)?);
        Ok(())
    };
    match &file.user.skills {
        SkillsSpec::None => {}
        SkillsSpec::Inherit => {
            if let Ok(entries) = std::fs::read_dir(&user_skills) {
                let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                dirs.sort();
                for dir in dirs.into_iter().filter(|d| d.join("SKILL.md").is_file()) {
                    take_skill(&dir)?;
                }
            }
        }
        SkillsSpec::Named(names) => {
            for name in names {
                validate_name(name).map_err(SetError::Invalid)?;
                let dir = user_skills.join(name);
                if !dir.join("SKILL.md").is_file() {
                    return invalid(format!(
                        "skill {name}: no {}",
                        dir.join("SKILL.md").display()
                    ));
                }
                take_skill(&dir)?;
            }
        }
        SkillsSpec::Dir(dir) => {
            let dir = at(dir);
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map_err(io(&dir))?
                .flatten()
                .map(|e| e.path())
                .collect();
            dirs.sort();
            for d in dirs.into_iter().filter(|d| d.join("SKILL.md").is_file()) {
                take_skill(&d)?;
            }
        }
    }

    let mut mcp_servers = BTreeMap::new();
    if !file.user.mcp_servers.is_empty() {
        let text = std::fs::read_to_string(&env.claude_json).map_err(io(&env.claude_json))?;
        let all: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| SetError::Invalid(format!("{}: {e}", env.claude_json.display())))?;
        for name in &file.user.mcp_servers {
            let server = all
                .get("mcpServers")
                .and_then(|m| m.get(name))
                .ok_or_else(|| {
                    SetError::Invalid(format!(
                        "MCP server {name}: not in {}",
                        env.claude_json.display()
                    ))
                })?;
            mcp_servers.insert(name.clone(), server.clone());
        }
    }

    let mut unisolated = BTreeMap::new();
    if !claude {
        for rel in ["AGENTS.md", "config.toml"] {
            let path = env.codex_home.join(rel);
            if path.is_file() {
                let digest = hex::encode(Sha256::digest(std::fs::read(&path).map_err(io(&path))?));
                unisolated.insert(format!("codex:{rel}"), digest);
            }
        }
        let skills_dir = env.codex_home.join("skills");
        if skills_dir.is_dir() {
            let mut h = Sha256::new();
            for rel in files_under(&skills_dir)? {
                h.update(rel.display().to_string());
                h.update(std::fs::read(skills_dir.join(&rel)).map_err(io(&skills_dir))?);
            }
            unisolated.insert("codex:skills".into(), hex::encode(h.finalize()));
        }
    }

    let mut resolved = ResolvedVariant {
        name: name.to_owned(),
        agent: agent.to_owned(),
        model: file.model.clone(),
        effort: file.effort.clone(),
        fw_path,
        fw_sha256,
        fw_version,
        agent_cli_version,
        permission_mode,
        setting_sources: if claude { setting_sources } else { Vec::new() },
        env: file.env.clone(),
        settings_blob,
        repo,
        claude_md_blob,
        skills,
        mcp_servers,
        unisolated,
        hash: String::new(),
    };
    resolved.hash = variant_hash(&resolved);
    Ok(resolved)
}

/// The content hash of a resolved variant (its name and the binary's path
/// excluded: two names for one setup are one variant).
pub fn variant_hash(v: &ResolvedVariant) -> String {
    let mut value = serde_json::to_value(v).expect("a resolved variant serializes");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("name");
        obj.remove("hash");
        obj.remove("fw_path");
    }
    let canonical = flywheel_core::loaders::canonical_json(&value);
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Parse a variant file.
pub fn load_file(path: &Path) -> Result<VariantFile, SetError> {
    let text = std::fs::read_to_string(path).map_err(io(path))?;
    toml::from_str(&text).map_err(|e| SetError::Invalid(format!("{}: {e}", path.display())))
}

// --- Applying a variant to a trial's repository ---------------------------------------

/// What one op did to one task's base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpEffect {
    pub path: String,
    pub op: OpKind,
    /// `changed`, `unchanged` (already as asked), or `absent` (nothing
    /// to strip).
    pub effect: String,
}

/// The overlay a variant made on one base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlay {
    /// The overlay commit (the trial's starting point).
    pub commit: String,
    /// Its tree id: the content the agent starts from.
    pub tree: String,
    pub effects: Vec<OpEffect>,
}

fn text_blob(blobs: &Path, digest: &str) -> Result<String, SetError> {
    String::from_utf8(get_blob(blobs, digest)?)
        .map_err(|_| SetError::Invalid(format!("blob {digest} is not text")))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), SetError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    std::fs::write(path, bytes).map_err(io(path))
}

/// Apply `variant` to the repository at `repo` (checked out at its base)
/// as one commit, merging `extra_settings` (the trial's capture hooks)
/// into `.claude/settings.local.json`. Author and date are fixed, so one
/// variant on one base always makes the same tree.
pub fn apply_overlay(
    variant: &ResolvedVariant,
    blobs: &Path,
    repo: &Path,
    extra_settings: Option<&serde_json::Value>,
) -> Result<Overlay, SetError> {
    let mut effects = Vec::new();
    for op in &variant.repo {
        let path = repo.join(&op.path);
        let current = std::fs::read(&path).ok();
        let effect = match op.op {
            OpKind::Replace => {
                let bytes = get_blob(blobs, op.blob.as_deref().unwrap_or(""))?;
                if current.as_deref() == Some(bytes.as_slice()) {
                    "unchanged"
                } else {
                    write_file(&path, &bytes)?;
                    "changed"
                }
            }
            OpKind::Append => {
                let add = text_blob(blobs, op.blob.as_deref().unwrap_or(""))?;
                let mut text = current
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&add);
                write_file(&path, text.as_bytes())?;
                "changed"
            }
            OpKind::Ensure => {
                let add = op
                    .blob
                    .as_deref()
                    .map(|b| text_blob(blobs, b))
                    .transpose()?
                    .unwrap_or_default();
                let text = current
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                let present = match &op.pattern {
                    Some(p) => Regex::new(p).expect("checked at resolve").is_match(&text),
                    None => text.contains(&add),
                };
                if present {
                    "unchanged"
                } else {
                    let mut out = text;
                    if !out.is_empty() && !out.ends_with("\n\n") {
                        out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
                    }
                    out.push_str(&add);
                    write_file(&path, out.as_bytes())?;
                    "changed"
                }
            }
            OpKind::Strip => {
                let text = current
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                let stripped = match (&op.pattern, &op.blob) {
                    (Some(p), _) => Regex::new(p)
                        .expect("checked at resolve")
                        .replace_all(&text, "")
                        .into_owned(),
                    (None, Some(b)) => text.replace(&text_blob(blobs, b)?, ""),
                    (None, None) => text.clone(),
                };
                if stripped == text {
                    "absent"
                } else {
                    write_file(&path, stripped.as_bytes())?;
                    "changed"
                }
            }
            OpKind::Delete => {
                if path.exists() {
                    if path.is_dir() {
                        std::fs::remove_dir_all(&path).map_err(io(&path))?;
                    } else {
                        std::fs::remove_file(&path).map_err(io(&path))?;
                    }
                    "changed"
                } else {
                    "absent"
                }
            }
            OpKind::ReplaceDir => {
                if path.exists() {
                    std::fs::remove_dir_all(&path).map_err(io(&path))?;
                }
                for (rel, blob) in &op.tree {
                    write_file(&path.join(rel), &get_blob(blobs, blob)?)?;
                }
                "changed"
            }
        };
        effects.push(OpEffect {
            path: op.path.clone(),
            op: op.op,
            effect: effect.to_owned(),
        });
    }
    if let Some(blob) = &variant.claude_md_blob {
        write_file(&repo.join(CLAUDE_LOCAL_MD), &get_blob(blobs, blob)?)?;
    }
    for (name, files) in &variant.skills {
        let dir = repo.join(".claude/skills").join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(io(&dir))?;
        }
        for (rel, blob) in files {
            write_file(&dir.join(rel), &get_blob(blobs, blob)?)?;
        }
    }
    if variant.is_claude() && !variant.mcp_servers.is_empty() {
        let path = repo.join(".mcp.json");
        let mut doc: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let servers = doc
            .as_object_mut()
            .ok_or_else(|| SetError::Invalid(".mcp.json is not an object".into()))?
            .entry("mcpServers")
            .or_insert_with(|| serde_json::json!({}));
        for (name, config) in &variant.mcp_servers {
            servers[name] = config.clone();
        }
        write_file(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&doc).expect("json")).as_bytes(),
        )?;
    }
    if variant.is_claude() && (variant.settings_blob.is_some() || extra_settings.is_some()) {
        let mut settings = match &variant.settings_blob {
            Some(b) => serde_json::from_str(&text_blob(blobs, b)?)
                .map_err(|e| SetError::Invalid(format!("settings: {e}")))?,
            None => serde_json::json!({}),
        };
        if let Some(extra) = extra_settings {
            merge_settings(&mut settings, extra);
        }
        write_file(
            &repo.join(SETTINGS_LOCAL),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&settings).expect("json")
            )
            .as_bytes(),
        )?;
    }
    git::run(repo, &["add", "-A", "--force", "."])?;
    git::run(
        repo,
        &[
            "-c",
            "user.name=flywheel eval",
            "-c",
            "user.email=eval@flywheel.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-verify",
            "--date",
            "2000-01-01T00:00:00Z",
            "-m",
            &format!(
                "eval: variant overlay {}\n\nFlywheel-Eval-Variant: {}",
                variant.name, variant.hash
            ),
        ],
    )?;
    Ok(Overlay {
        commit: git::rev_parse(repo, "HEAD")?,
        tree: git::run(repo, &["rev-parse", "HEAD^{tree}"])?,
        effects,
    })
}

/// Merge `extra` into `settings`: hook lists are concatenated per event,
/// every other key of `extra` wins.
pub fn merge_settings(settings: &mut serde_json::Value, extra: &serde_json::Value) {
    let (Some(target), Some(extra)) = (settings.as_object_mut(), extra.as_object()) else {
        return;
    };
    for (key, value) in extra {
        if key == "hooks"
            && let (Some(existing), Some(add)) = (
                target.get_mut("hooks").and_then(|h| h.as_object_mut()),
                value.as_object(),
            )
        {
            for (event, list) in add {
                let slot = existing
                    .entry(event.clone())
                    .or_insert_with(|| serde_json::json!([]));
                if let (Some(slot), Some(list)) = (slot.as_array_mut(), list.as_array()) {
                    slot.extend(list.iter().cloned());
                }
            }
            continue;
        }
        target.insert(key.clone(), value.clone());
    }
}

#[cfg(test)]
mod tests;
