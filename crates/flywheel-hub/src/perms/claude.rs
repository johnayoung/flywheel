//! Claude Code's settings files for one folder, highest precedence first
//! (managed > command line > local > project > user), the permission rules
//! each holds, and the hub's writes to the three a user owns.

use super::{Env, Folder};
use super::{edit, rule};
use crate::review::runner;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Largest settings file read.
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

/// The files the hub may write a rule to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum File {
    /// `.claude/settings.local.json` at the repo's main checkout (where
    /// Claude Code keeps it): yours, usually git-ignored. The default.
    Local,
    /// `.claude/settings.json` in the folder: shared once committed.
    Project,
    /// `~/.claude/settings.json`: every project on this machine. Written
    /// only when asked for by name.
    User,
}

impl File {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "" | "local" => Ok(Self::Local),
            "project" => Ok(Self::Project),
            "user" => Ok(Self::User),
            other => Err(format!(
                "unknown settings file {other:?}: local, project, or user"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Project => "project",
            Self::User => "user",
        }
    }

    /// Where it is for `folder`.
    pub fn path(self, folder: &Folder, env: &Env) -> PathBuf {
        match self {
            Self::Local => folder.local_root(env).join(".claude/settings.local.json"),
            Self::Project => folder.dir.join(".claude/settings.json"),
            Self::User => env.claude_dir.join("settings.json"),
        }
    }

    /// Who a rule in it reaches.
    pub fn reach(self, folder: &Folder, env: &Env) -> String {
        match self {
            Self::Local => format!(
                "in your Claude Code sessions in {}{}",
                folder.local_root(env).display(),
                if folder.main.is_some() {
                    " and its worktrees"
                } else {
                    ""
                }
            ),
            Self::Project => format!(
                "for everyone whose Claude Code reads {} (shared once committed)",
                Self::Project.path(folder, env).display()
            ),
            Self::User => "in every project on this machine".to_owned(),
        }
    }
}

/// One settings source and the permission rules in it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Source {
    /// `managed`, `command line`, `local`, `project`, `user`.
    pub scope: &'static str,
    /// What it is (`managed-settings.json`, `legacy local`, ...).
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub exists: bool,
    /// It could not be read (or parsed): Claude Code skips it too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
    /// The hub may write to it (`local`, `project`, `user`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<File>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Allow rules here that a deny or ask rule (in any source) overrides
    /// whole: they never stop a prompt.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overridden: Vec<Override>,
}

/// An allow rule a deny or ask rule overrides: Claude Code checks deny,
/// then ask, then allow, whatever file each is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Override {
    /// The allow rule.
    pub rule: String,
    /// The rule that wins.
    pub by: String,
    /// `deny` or `ask`.
    pub decision: &'static str,
    /// The source the winning rule is in.
    pub scope: &'static str,
}

impl Override {
    /// "deny rule Bash(x *) (project) overrides it: the agent is refused".
    pub fn text(&self) -> String {
        format!(
            "the {} rule {} ({}) overrides {}: {}",
            self.decision,
            self.by,
            self.scope,
            self.rule,
            if self.decision == "deny" {
                "the agent is still refused"
            } else {
                "the agent still asks"
            }
        )
    }
}

/// The deny or ask rule that overrides allow rule `allow`, if any (deny
/// first, as Claude Code checks them).
pub fn overriding(sources: &[Source], allow: &str, anchors: &rule::Anchors) -> Option<Override> {
    let allow_rule = rule::Rule::parse_any(allow)?;
    for decision in ["deny", "ask"] {
        for source in sources.iter().filter(|s| s.error.is_none()) {
            let list = if decision == "deny" {
                &source.deny
            } else {
                &source.ask
            };
            for raw in list {
                if let Some(by) = rule::Rule::parse_any(raw)
                    && rule::subsumes(&by, &allow_rule, anchors)
                {
                    return Some(Override {
                        rule: allow.to_owned(),
                        by: raw.clone(),
                        decision,
                        scope: source.scope,
                    });
                }
            }
        }
    }
    None
}

/// Mark every source's overridden allow rules.
pub fn mark_overridden(sources: &mut [Source], anchors: &rule::Anchors) {
    let marks: Vec<Vec<Override>> = sources
        .iter()
        .map(|s| {
            s.allow
                .iter()
                .filter_map(|a| overriding(sources, a, anchors))
                .collect()
        })
        .collect();
    for (source, marks) in sources.iter_mut().zip(marks) {
        source.overridden = marks;
    }
}

fn strings(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Read a settings file into `source`.
fn read_into(source: &mut Source, path: &Path) -> Option<serde_json::Value> {
    source.path = Some(path.to_path_buf());
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            source.exists = true;
            source.error = Some(e.to_string());
            return None;
        }
    };
    source.exists = true;
    if meta.len() > MAX_SETTINGS_BYTES {
        source.error = Some(format!("larger than {} KiB", MAX_SETTINGS_BYTES / 1024));
        return None;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            source.error = Some(e.to_string());
            return None;
        }
    };
    if text.trim().is_empty() {
        return None;
    }
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            source.error = Some(format!("not valid JSON: {e} (Claude Code skips this file)"));
            return None;
        }
    };
    let perms = value.get("permissions");
    source.allow = strings(perms.and_then(|p| p.get("allow")));
    source.ask = strings(perms.and_then(|p| p.get("ask")));
    source.deny = strings(perms.and_then(|p| p.get("deny")));
    source.default_mode = perms
        .and_then(|p| p.get("defaultMode"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    source.additional_directories = strings(perms.and_then(|p| p.get("additionalDirectories")));
    Some(value)
}

/// Every settings source for a session in `folder`, highest precedence
/// first. `launch_mode`: the session's `--permission-mode`; `session`: the
/// view is for a hub session (its command-line source is shown).
pub fn sources(
    folder: &Folder,
    env: &Env,
    session: bool,
    launch_mode: Option<&str>,
) -> (Vec<Source>, bool) {
    let mut out = Vec::new();
    let mut managed_only = false;
    // Managed: managed-settings.json, then managed-settings.d/*.json in
    // name order.
    let mut managed_files = vec![env.managed_dir.join("managed-settings.json")];
    if let Ok(entries) = std::fs::read_dir(env.managed_dir.join("managed-settings.d")) {
        let mut names: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|x| x == "json")
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .collect();
        names.sort();
        managed_files.extend(names);
    }
    for path in managed_files {
        let mut source = Source {
            scope: "managed",
            label: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ..Source::default()
        };
        let value = read_into(&mut source, &path);
        if value
            .as_ref()
            .and_then(|v| v.get("allowManagedPermissionRulesOnly"))
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            managed_only = true;
            source.notes.push(
                "allowManagedPermissionRulesOnly: rules from every other source are ignored".into(),
            );
        }
        if source.exists {
            out.push(source);
        }
    }
    if out.iter().all(|s| s.scope != "managed") {
        out.push(Source {
            scope: "managed",
            label: "managed-settings.json".into(),
            path: Some(env.managed_dir.join("managed-settings.json")),
            notes: vec![
                "none on this machine (server-managed policy, if any, is not visible here)".into(),
            ],
            ..Source::default()
        });
    }
    if session {
        out.push(Source {
            scope: "command line",
            label: "the hub's --settings: status hooks only, no rules".into(),
            exists: true,
            default_mode: launch_mode.map(str::to_owned),
            notes: launch_mode
                .map(|m| vec![format!("launched with --permission-mode {m}")])
                .unwrap_or_default(),
            ..Source::default()
        });
    }
    let local_path = File::Local.path(folder, env);
    let mut local = Source {
        scope: "local",
        label: ".claude/settings.local.json".into(),
        file: Some(File::Local),
        ..Source::default()
    };
    read_into(&mut local, &local_path);
    out.push(local);
    // Before 2.1.211 the local file stayed in the starting directory;
    // one left there is still read.
    let legacy = folder.dir.join(".claude/settings.local.json");
    if legacy != local_path && legacy.exists() {
        let mut source = Source {
            scope: "local",
            label: "legacy .claude/settings.local.json (still read)".into(),
            ..Source::default()
        };
        read_into(&mut source, &legacy);
        out.push(source);
    }
    let mut project = Source {
        scope: "project",
        label: ".claude/settings.json".into(),
        file: Some(File::Project),
        ..Source::default()
    };
    read_into(&mut project, &File::Project.path(folder, env));
    if !project.allow.is_empty() {
        project.notes.push(
            "its allow rules apply once the folder is trusted (the workspace trust dialog)".into(),
        );
    }
    out.push(project);
    let mut user = Source {
        scope: "user",
        label: "~/.claude/settings.json".into(),
        file: Some(File::User),
        ..Source::default()
    };
    read_into(&mut user, &File::User.path(folder, env));
    out.push(user);
    for source in &mut out {
        if source.scope != "managed" && source.scope != "command line" {
            if managed_only && !source.allow.is_empty() {
                source
                    .notes
                    .push("ignored: managed settings allow only their own rules".into());
            }
            if matches!(
                source.default_mode.as_deref(),
                Some("auto" | "bypassPermissions")
            ) && matches!(source.scope, "local" | "project")
            {
                source.notes.push(
                    "defaultMode auto / bypassPermissions has no effect from this file".into(),
                );
            }
        }
    }
    (out, managed_only)
}

const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Run git in `dir`; `Some(exit code)` (None: could not run it).
fn git_code(dir: &Path, args: &[&str]) -> Option<i32> {
    let output = runner::run(&runner::Spec {
        program: "git",
        dir,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        stdin: None,
        timeout: GIT_TIMEOUT,
        limit: 64 * 1024,
    })
    .ok()?;
    if output.timed_out {
        return None;
    }
    output.code
}

/// Whether git tracks `path`, and whether it ignores it (`None`: not in a
/// git repository, or git could not tell).
pub fn git_state(path: &Path) -> (Option<bool>, Option<bool>) {
    // The nearest existing directory: the file and `.claude` may not exist.
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.is_dir() {
            break;
        }
        dir = d.parent();
    }
    let Some(dir) = dir else {
        return (None, None);
    };
    if git_code(dir, &["rev-parse", "--is-inside-work-tree"]) != Some(0) {
        return (None, None);
    }
    let p = path.to_string_lossy();
    let tracked = git_code(dir, &["ls-files", "--error-unmatch", "--", &p]).map(|c| c == 0);
    let ignored = match git_code(dir, &["check-ignore", "-q", "--", &p]) {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    };
    (tracked, ignored)
}

/// A file the hub can write, as the UI offers it.
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub file: File,
    pub path: PathBuf,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored: Option<bool>,
    /// Shown before writing (a tracked local file, a shared project file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Who a rule in it reaches.
    pub reach: String,
}

/// What writing to `file` would mean for `folder`.
pub fn target(file: File, folder: &Folder, env: &Env) -> Target {
    let path = file.path(folder, env);
    let (tracked, ignored) = if file == File::User {
        (None, None)
    } else {
        git_state(&path)
    };
    let warning = match file {
        File::Local if tracked == Some(true) => Some(
            "this local settings file is tracked by git: the rule would be committed for everyone, and Claude Code holds a tracked local file's allow rules until the folder is trusted".to_owned(),
        ),
        File::Local if tracked == Some(false) && ignored == Some(false) => Some(
            "this local settings file is not git-ignored: `git add -A` would commit it (the hub can add it to .gitignore: `--gitignore`, or the box in the preview)".to_owned(),
        ),
        File::Project => Some(
            "the project file is shared: once committed, everyone who uses the repo gets the rule (after they trust the folder)".to_owned(),
        ),
        File::User => Some("user settings: the rule applies in every project on this machine".to_owned()),
        File::Local => None,
    };
    Target {
        file,
        exists: path.exists(),
        path,
        tracked,
        ignored,
        warning,
        reach: file.reach(folder, env),
    }
}

/// The digest of a file's content (`""` when absent): the base a preview
/// was computed from.
pub fn digest(text: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    match text {
        None => String::new(),
        Some(text) => Sha256::digest(text.as_bytes())[..12]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    }
}

/// A file's text (`None`: absent). Symlinks are refused: an atomic
/// rewrite would replace the link, and Claude Code treats a linked
/// `.claude` as repository-supplied.
pub fn read_for_edit(path: &Path) -> Result<Option<String>, String> {
    for p in [Some(path), path.parent()].into_iter().flatten() {
        if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{} is a symlink: edit it by hand", p.display()));
        }
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_SETTINGS_BYTES => {
            return Err(format!(
                "{} is larger than {} KiB",
                path.display(),
                MAX_SETTINGS_BYTES / 1024
            ));
        }
        _ => {}
    }
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The edit `rules` make to `path`: `(old text, new text, rules added)`.
pub fn plan(
    path: &Path,
    rules: &[String],
) -> Result<(Option<String>, String, Vec<String>), String> {
    let old = read_for_edit(path)?;
    let (new, added) = edit::add_allow_rules(old.as_deref().unwrap_or(""), rules)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((old, new, added))
}

/// Replace `path` with `text` atomically (a temp file beside it, synced,
/// renamed over it), keeping its permission bits (new files: 0644, the
/// directory created 0755).
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mode = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o7777)
                .unwrap_or(0o644)
        }
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Unique per write (two writers in one hub must not share it).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.fw-hub-{}-{seq}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("cannot write {}: {e}", path.display()))
}
