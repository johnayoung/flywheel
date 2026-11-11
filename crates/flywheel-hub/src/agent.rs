//! Session kinds -- a plain shell, or one of the agent CLIs the hub
//! launches (Claude Code, Codex) -- resolving their programs on `PATH`, the
//! extra arguments that wire their status reporting back to the hub, and
//! what each report means.

use crate::status::Signal;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// What a session runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// The hub's shell, or an explicit argv.
    #[default]
    Shell,
    /// The `claude` CLI (Claude Code).
    Claude,
    /// The `codex` CLI.
    Codex,
}

impl SessionKind {
    pub const ALL: [Self; 3] = [Self::Shell, Self::Claude, Self::Codex];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn is_agent(self) -> bool {
        !matches!(self, Self::Shell)
    }
}

/// The agent CLIs the hub launches, by program name or path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPrograms {
    pub claude: String,
    pub codex: String,
}

impl Default for AgentPrograms {
    fn default() -> Self {
        Self {
            claude: "claude".into(),
            codex: "codex".into(),
        }
    }
}

impl AgentPrograms {
    /// The configured program for an agent kind (`None` for shells).
    pub fn program(&self, kind: SessionKind) -> Option<&str> {
        match kind {
            SessionKind::Shell => None,
            SessionKind::Claude => Some(&self.claude),
            SessionKind::Codex => Some(&self.codex),
        }
    }
}

/// Resolve `program` the way a shell would: a name containing `/` is used
/// as is (when it is an executable file), a bare name is searched for in
/// `path` (a `PATH`-style list).
pub fn resolve_program(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return is_executable(&candidate).then_some(candidate);
    }
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// Resolve against this process's `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    resolve_program(program, std::env::var_os("PATH").as_deref())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

// --- agent launch arguments -------------------------------------------------

/// Variables an enclosing agent session sets for everything it runs. A hub
/// started from inside Claude Code or Codex would hand them to its own
/// sessions, which would then believe they are nested in that agent
/// (Claude Code, for one, stops saving transcripts -- and `--resume` with
/// them). Session-scoped markers only; user configuration such as
/// `CLAUDE_CONFIG_DIR` or `ANTHROPIC_*` passes through.
pub const INHERITED_AGENT_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "AI_AGENT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
];

/// The Claude Code hook events the hub listens to, the signal each means,
/// and (for `Notification`) the notification types that matter.
const CLAUDE_HOOKS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PreToolUse", None),
    ("PostToolUse", None),
    ("PostToolUseFailure", None),
    ("PermissionRequest", None),
    ("Notification", Some("permission_prompt")),
    ("Notification", Some("elicitation_dialog")),
    ("Notification", Some("elicitation_url_dialog")),
    ("Notification", Some("agent_needs_input")),
    ("Notification", Some("idle_prompt")),
    ("Stop", None),
    ("StopFailure", None),
    ("SessionEnd", None),
];

/// Seconds Claude Code waits on one status hook (a report takes
/// milliseconds; this only bounds a wedged hub).
pub const CLAUDE_HOOK_TIMEOUT_SECS: u64 = 10;

/// The Claude Code release the hub's http hooks were verified on. Until
/// a session's status line has shown a CLI at or past it, sessions get a
/// command hook per event instead (the same reports, one process each).
pub const CLAUDE_HTTP_HOOKS_SINCE: (u32, u32, u32) = (2, 1, 322);

/// The path agent hooks POST to on their hub.
pub const HOOK_PATH: &str = "/api/telemetry/hook";
/// The path of the hub's progress tool (an MCP server over http).
pub const MCP_PATH: &str = "/api/telemetry/mcp";
/// The request headers a session names itself with on those two paths
/// (Claude Code fills them from the session's environment).
pub const SESSION_HEADER: &str = "x-flywheel-session";
pub const SECRET_HEADER: &str = "x-flywheel-secret";

/// How a Claude Code session is wired to its hub.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeWiring<'a> {
    /// The argv prefix of this binary's `hub` verb (`[/path/to/fw, hub]`).
    pub report: Option<&'a [String]>,
    /// The hub's own http address (`http://127.0.0.1:7420`), once it
    /// listens: where the progress tool is.
    pub hub_url: Option<&'a str>,
    /// Hooks POST to `hub_url` instead of running a process per event.
    pub http_hooks: bool,
    /// The user's own `statusLine` setting: its command runs after the
    /// hub's, and its other fields are kept.
    pub status_line: Option<&'a serde_json::Value>,
}

/// One hook handler: a POST to the hub when the wiring allows it for
/// `event`, else the report command. `SessionStart` is always a command
/// (Claude Code runs no http hook for it).
fn claude_hook(
    wiring: &ClaudeWiring<'_>,
    event: &str,
    detail: Option<&str>,
) -> Option<serde_json::Value> {
    if let Some(url) = wiring
        .hub_url
        .filter(|_| wiring.http_hooks && event != "SessionStart")
    {
        return Some(serde_json::json!({
            "type": "http",
            "url": format!("{url}{HOOK_PATH}"),
            "timeout": CLAUDE_HOOK_TIMEOUT_SECS,
            "headers": {
                SESSION_HEADER: format!("${}", crate::report::SESSION_ENV),
                SECRET_HEADER: format!("${}", crate::report::SECRET_ENV),
            },
            "allowedEnvVars": [crate::report::SESSION_ENV, crate::report::SECRET_ENV],
        }));
    }
    let report = wiring.report.filter(|r| !r.is_empty())?;
    let mut args: Vec<String> = report[1..].to_vec();
    args.extend(["report", "--source", "claude", "--event", event].map(str::to_owned));
    if let Some(detail) = detail {
        args.extend(["--detail".to_owned(), detail.to_owned()]);
    }
    args.push("--stdin".to_owned());
    Some(serde_json::json!({
        "type": "command",
        "command": report[0],
        "args": args,
        "timeout": CLAUDE_HOOK_TIMEOUT_SECS,
    }))
}

/// A word quoted for `sh`.
pub fn sh_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The hub's status line: the report command, told to run the user's own
/// command (`--chain`) for what is shown. The user's other fields (its
/// padding, its refresh interval) are kept.
fn claude_status_line(wiring: &ClaudeWiring<'_>) -> Option<serde_json::Value> {
    let report = wiring.report.filter(|r| !r.is_empty())?;
    let mut words: Vec<String> = report.iter().map(|w| sh_quote(w)).collect();
    words.extend(
        [
            "report",
            "--source",
            "claude",
            "--event",
            crate::telemetry::claude::STATUS_LINE_EVENT,
            "--stdin",
        ]
        .map(str::to_owned),
    );
    let theirs = wiring
        .status_line
        .filter(|s| s.get("type").and_then(serde_json::Value::as_str) == Some("command"));
    if let Some(command) = theirs
        .and_then(|s| s.get("command"))
        .and_then(serde_json::Value::as_str)
        .filter(|c| !c.trim().is_empty())
    {
        words.push("--chain".to_owned());
        words.push(sh_quote(command));
    }
    let mut line = match theirs {
        Some(serde_json::Value::Object(theirs)) => theirs.clone(),
        _ => serde_json::Map::new(),
    };
    line.insert("type".to_owned(), "command".into());
    line.insert("command".to_owned(), words.join(" ").into());
    Some(serde_json::Value::Object(line))
}

/// `--settings` JSON that makes Claude Code tell its hub what it does:
/// every status-relevant hook ([`CLAUDE_HOOKS`]) and the telemetry-only
/// ones ([`crate::telemetry::claude::EXTRA_HOOKS`]) as a POST to the hub
/// (or `report --source claude --event E [--detail D] --stdin`, spawned
/// directly with no shell, where http is not wired), and the status line
/// that carries the context window, cost, and rate limits.
pub fn claude_settings(wiring: &ClaudeWiring<'_>) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    let extra = crate::telemetry::claude::EXTRA_HOOKS
        .iter()
        .map(|event| (*event, None));
    for (event, detail) in CLAUDE_HOOKS.iter().copied().chain(extra) {
        let Some(handler) = claude_hook(wiring, event, detail) else {
            continue;
        };
        let mut group = serde_json::json!({ "hooks": [handler] });
        if let Some(detail) = detail {
            group["matcher"] = serde_json::Value::String(detail.to_owned());
        }
        hooks
            .entry(event.to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .expect("hook groups are arrays")
            .push(group);
    }
    let mut settings = serde_json::json!({ "hooks": hooks });
    if let Some(line) = claude_status_line(wiring) {
        settings["statusLine"] = line;
    }
    if wiring.hub_url.is_some() {
        // The hub's own tools that only declare or read never wait on a
        // permission prompt; message_session types into another agent, so
        // it asks like any tool until the operator allows it.
        settings["permissions"] = serde_json::json!({ "allow": [
            crate::telemetry::claude::PROGRESS_TOOL,
            crate::telemetry::claude::ASK_TOOL,
            crate::telemetry::claude::HANDOFF_TOOL,
            crate::telemetry::claude::LIST_TOOL,
        ] });
    }
    settings
}

/// `--mcp-config` JSON giving a session the hub's progress tool
/// ([`crate::telemetry::mcp`]); Claude Code fills the headers from the
/// session's environment.
pub fn claude_mcp_config(hub_url: &str) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            crate::telemetry::mcp::SERVER_NAME: {
                "type": "http",
                "url": format!("{hub_url}{MCP_PATH}"),
                "headers": {
                    SESSION_HEADER: format!("${{{}}}", crate::report::SESSION_ENV),
                    SECRET_HEADER: format!("${{{}}}", crate::report::SECRET_ENV),
                },
            }
        }
    })
}

/// Extra argv for a Claude Code session: its conversation id (`--resume`
/// when a transcript for it already exists, so a relaunch continues the
/// conversation) and, when the hub can take reports, its hooks, status
/// line, and progress tool.
pub fn claude_args(session_id: &str, resume: bool, wiring: &ClaudeWiring<'_>) -> Vec<String> {
    let flag = if resume { "--resume" } else { "--session-id" };
    let mut args = vec![flag.to_owned(), session_id.to_owned()];
    if wiring.report.is_some_and(|r| !r.is_empty()) {
        args.push("--settings".to_owned());
        args.push(claude_settings(wiring).to_string());
        if let Some(url) = wiring.hub_url {
            // `--mcp-config` takes several values: a flag follows it.
            args.push("--mcp-config".to_owned());
            args.push(claude_mcp_config(url).to_string());
            args.push("--append-system-prompt".to_owned());
            args.push(crate::telemetry::mcp::NUDGE.to_owned());
        }
    }
    args
}

/// What the user's own Claude Code settings say that the hub's wiring
/// must respect.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeUserSettings {
    /// Their `statusLine` (the most specific file's).
    pub status_line: Option<serde_json::Value>,
    /// Some settings file lists `allowedHttpHookUrls`: only those URLs'
    /// http hooks run, so the hub keeps to command hooks.
    pub restricts_http_hooks: bool,
}

/// Read the settings files Claude Code would for a session in `cwd`: the
/// user's, the project's, the project's local ones, and managed policy.
pub fn claude_user_settings(config_dir: Option<&Path>, cwd: &Path) -> ClaudeUserSettings {
    let mut found = ClaudeUserSettings::default();
    let files = [
        Some(PathBuf::from("/etc/claude-code/managed-settings.json")),
        config_dir.map(|dir| dir.join("settings.json")),
        Some(cwd.join(".claude").join("settings.json")),
        Some(cwd.join(".claude").join("settings.local.json")),
    ];
    // Least specific first: a later file's status line wins.
    for file in files.into_iter().flatten() {
        let Some(settings) = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        else {
            continue;
        };
        if settings.get("allowedHttpHookUrls").is_some() {
            found.restricts_http_hooks = true;
        }
        if let Some(line) = settings.get("statusLine").filter(|l| l.is_object()) {
            found.status_line = Some(line.clone());
        }
    }
    found
}

/// The `major.minor.patch` in a `claude --version` line.
pub fn parse_claude_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = word.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

// --- launch options -----------------------------------------------------------

/// Claude Code's `--permission-mode` choices (claude 2.1.306). Omitted:
/// the CLI's own default (its settings, else asking before edits).
pub const CLAUDE_PERMISSION_MODES: &[&str] = &[
    "manual",
    "acceptEdits",
    "plan",
    "auto",
    "dontAsk",
    "bypassPermissions",
];
/// The permission mode that skips every check (the UI warns about it).
pub const CLAUDE_BYPASS_MODE: &str = "bypassPermissions";
/// Codex's `--ask-for-approval` policies.
pub const CODEX_APPROVALS: &[&str] = &["untrusted", "on-failure", "on-request", "never"];
/// Codex's `--sandbox` policies.
pub const CODEX_SANDBOXES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];
/// Longest model id accepted.
pub const MAX_MODEL_CHARS: usize = 100;
/// Largest initial prompt (it travels as one argv element; Linux caps
/// one at 128 KiB).
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;

/// How an agent CLI is started, beyond the hub's own wiring: its model and
/// permission policy. Recorded with the session, so a relaunch or restore
/// runs it the same way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentLaunch {
    /// `claude --model` / `codex -m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Claude Code's `--permission-mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Codex's `--ask-for-approval`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    /// Codex's `--sandbox`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

impl AgentLaunch {
    /// Nothing set (the CLI's defaults).
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Blank fields dropped, every value checked against `kind`: a shell
    /// takes none, Claude no Codex policy and the other way round.
    pub fn validated(&self, kind: SessionKind) -> Result<Self, String> {
        let clean = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let launch = Self {
            model: clean(&self.model),
            permission_mode: clean(&self.permission_mode),
            approval: clean(&self.approval),
            sandbox: clean(&self.sandbox),
        };
        if launch.is_empty() {
            return Ok(launch);
        }
        let name = kind.as_str();
        if kind == SessionKind::Shell {
            return Err("a shell session takes no model or permission mode".into());
        }
        if let Some(model) = &launch.model {
            validate_model(model)?;
        }
        let pick = |value: &Option<String>, allowed: &[&str], what: &str, flag: &str| match value {
            Some(v) if !allowed.contains(&v.as_str()) => Err(format!(
                "{what} {v:?} is not one of {} ({name} {flag})",
                allowed.join(", ")
            )),
            _ => Ok(()),
        };
        match kind {
            SessionKind::Claude => {
                if launch.approval.is_some() || launch.sandbox.is_some() {
                    return Err(
                        "approval and sandbox are Codex policies; Claude takes a permission mode"
                            .into(),
                    );
                }
                pick(
                    &launch.permission_mode,
                    CLAUDE_PERMISSION_MODES,
                    "permission mode",
                    "--permission-mode",
                )?;
            }
            SessionKind::Codex => {
                if launch.permission_mode.is_some() {
                    return Err(
                        "a permission mode is Claude's; Codex takes an approval policy and a sandbox"
                            .into(),
                    );
                }
                pick(&launch.approval, CODEX_APPROVALS, "approval policy", "-a")?;
                pick(&launch.sandbox, CODEX_SANDBOXES, "sandbox", "-s")?;
            }
            SessionKind::Shell => unreachable!("shells were refused above"),
        }
        Ok(launch)
    }

    /// The CLI flags for these options (after the hub's own wiring).
    pub fn args(&self, kind: SessionKind) -> Vec<String> {
        let mut args = Vec::new();
        let mut push = |flag: &str, value: &Option<String>| {
            if let Some(value) = value {
                args.push(flag.to_owned());
                args.push(value.clone());
            }
        };
        match kind {
            SessionKind::Claude => {
                push("--model", &self.model);
                push("--permission-mode", &self.permission_mode);
            }
            SessionKind::Codex => {
                push("-m", &self.model);
                push("-a", &self.approval);
                push("-s", &self.sandbox);
            }
            SessionKind::Shell => {}
        }
        args
    }

    /// Whether these options switch the agent's safety checks off.
    pub fn bypasses_permissions(&self) -> bool {
        self.permission_mode.as_deref() == Some(CLAUDE_BYPASS_MODE)
            || (self.approval.as_deref() == Some("never")
                && self.sandbox.as_deref() == Some("danger-full-access"))
    }
}

/// A model id: letters, digits and `.-_:/@[]`, not starting with `-` (it
/// is a flag's value; never another flag).
pub fn validate_model(model: &str) -> Result<(), String> {
    if model.chars().count() > MAX_MODEL_CHARS {
        return Err(format!("model is longer than {MAX_MODEL_CHARS} characters"));
    }
    let ok = model.starts_with(|c: char| c.is_ascii_alphanumeric())
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_:/@[]".contains(c));
    if !ok {
        return Err(format!(
            "model {model:?} may only hold letters, digits and . - _ : / @ [ ] (and starts with a letter or digit)"
        ));
    }
    Ok(())
}

/// An initial prompt as given: `None` when blank; refused when too large
/// or holding a NUL (it cannot travel in argv).
pub fn validate_prompt(prompt: Option<&str>) -> Result<Option<String>, String> {
    let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) else {
        return Ok(None);
    };
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "the initial prompt is larger than {MAX_PROMPT_BYTES} bytes"
        ));
    }
    if prompt.contains('\0') {
        return Err("the initial prompt holds a NUL byte".into());
    }
    Ok(Some(prompt.trim_end().to_owned()))
}

/// argv that hands an agent CLI its first user turn: both take the prompt
/// as their positional argument (`claude [options] [prompt]`, `codex
/// [OPTIONS] [PROMPT]`), after `--` so a prompt starting with `-` (or
/// naming a Codex subcommand) stays the prompt. Goes last.
pub fn prompt_args(kind: SessionKind, prompt: &str) -> Vec<String> {
    match kind {
        SessionKind::Claude | SessionKind::Codex => vec!["--".to_owned(), prompt.to_owned()],
        SessionKind::Shell => Vec::new(),
    }
}

/// Extra argv for a Codex session: its turn-complete notifier runs
/// `report --source codex --event notify <payload-json>`.
pub fn codex_args(report: Option<&[String]>) -> Vec<String> {
    let Some(report) = report.filter(|r| !r.is_empty()) else {
        return Vec::new();
    };
    let argv: Vec<String> = report
        .iter()
        .map(String::as_str)
        .chain(["report", "--source", "codex", "--event", "notify"])
        .map(toml_string)
        .collect();
    vec!["-c".to_owned(), format!("notify=[{}]", argv.join(","))]
}

/// A TOML basic string (`-c key=value` values are TOML).
pub fn toml_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Claude Code's config dir: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_config_dir(config_dir: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(dir) = config_dir.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| Path::new(h).join(".claude"))
}

/// The project-directory name Claude Code files a `cwd`'s transcripts
/// under: every character but ASCII letters and digits becomes `-`.
pub fn claude_project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The transcript (`<id>.jsonl`) of conversation `session_id`, if one
/// exists: first where Claude Code files `cwd`'s conversations, then in any
/// project dir (long paths get a hashed name).
pub fn claude_transcript(config_dir: &Path, cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let file = format!("{session_id}.jsonl");
    let projects = config_dir.join("projects");
    let direct = projects.join(claude_project_dir_name(cwd)).join(&file);
    if direct.is_file() {
        return Some(direct);
    }
    std::fs::read_dir(&projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&file))
        .find(|candidate| candidate.is_file())
}

/// Extra argv that resumes Codex conversation `thread_id` (`codex resume
/// <id>`); goes after [`codex_args`], whose `-c` overrides are top-level
/// options.
pub fn codex_resume_args(thread_id: &str) -> Vec<String> {
    vec!["resume".to_owned(), thread_id.to_owned()]
}

/// Codex's home: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home(codex_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(dir) = codex_home.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| Path::new(h).join(".codex"))
}

/// Codex day directories searched for a rollout, newest first.
pub const MAX_ROLLOUT_DAYS: usize = 400;

/// The rollout (`sessions/YYYY/MM/DD/rollout-<timestamp>-<id>.jsonl`) of
/// Codex conversation `thread_id`, if one exists: searched newest day
/// first (a live conversation is found at once), at most
/// [`MAX_ROLLOUT_DAYS`] days back.
pub fn codex_rollout(codex_home: &Path, thread_id: &str) -> Option<PathBuf> {
    if !is_conversation_id(thread_id) {
        return None;
    }
    let suffix = format!("-{thread_id}.jsonl");
    let newest_first = |dir: &Path| -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort_unstable_by(|a, b| b.cmp(a));
        dirs
    };
    let days = newest_first(&codex_home.join("sessions"))
        .into_iter()
        .flat_map(|year| newest_first(&year))
        .flat_map(|month| newest_first(&month))
        .take(MAX_ROLLOUT_DAYS);
    for day in days {
        let Ok(entries) = std::fs::read_dir(&day) else {
            continue;
        };
        let found = entries.filter_map(Result::ok).find(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
        });
        if let Some(found) = found {
            return Some(found.path());
        }
    }
    None
}

// --- reports -> signals ------------------------------------------------------

/// What a Claude Code hook event means for the session's status.
pub fn claude_signal(event: &str, detail: Option<&str>) -> Option<Signal> {
    Some(match event {
        "SessionStart" => Signal::Ready,
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Signal::Working,
        "PermissionRequest" => Signal::Blocked,
        "Notification" => match detail? {
            "permission_prompt"
            | "elicitation_dialog"
            | "elicitation_url_dialog"
            | "agent_needs_input" => Signal::Blocked,
            "idle_prompt" => Signal::AwaitingInput,
            _ => return None,
        },
        "Stop" | "StopFailure" => Signal::Done,
        "SessionEnd" => Signal::Ended,
        _ => return None,
    })
}

/// A human line for a blocked Claude Code session, from its hook payload
/// (a notification without a message: from its type, `qualifier`).
pub fn claude_detail(
    event: &str,
    qualifier: Option<&str>,
    payload: &serde_json::Value,
) -> Option<String> {
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned)
    };
    match event {
        "Notification" => text("message").or_else(|| {
            Some(
                match qualifier? {
                    "permission_prompt" => "Claude needs your permission",
                    "elicitation_dialog" | "elicitation_url_dialog" => {
                        "Claude is asking a question"
                    }
                    "agent_needs_input" => "Claude needs your input",
                    "idle_prompt" => "Claude is waiting for your input",
                    _ => return None,
                }
                .to_owned(),
            )
        }),
        "PermissionRequest" => {
            let tool = text("tool_name")?;
            // A question tool asks its question; the permission is moot.
            let question = payload
                .pointer("/tool_input/questions/0/question")
                .and_then(|q| q.as_str())
                .filter(|q| !q.trim().is_empty());
            Some(match question {
                Some(question) => question.to_owned(),
                None => format!("permission to use {tool}"),
            })
        }
        _ => None,
    }
}

/// What a Codex `notify` payload means for the session's status.
pub fn codex_signal(payload: &serde_json::Value) -> Option<Signal> {
    match payload.get("type").and_then(|t| t.as_str())? {
        "agent-turn-complete" => Some(Signal::Done),
        _ => None,
    }
}

/// The conversation id an agent report carries (Claude's `session_id`
/// changes on `/clear`; Codex's notify names its `thread-id`).
pub fn reported_conversation(kind: SessionKind, payload: &serde_json::Value) -> Option<String> {
    let key = match kind {
        SessionKind::Claude => "session_id",
        SessionKind::Codex => "thread-id",
        SessionKind::Shell => return None,
    };
    payload
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|id| is_conversation_id(id))
        .map(str::to_owned)
}

/// A plausible agent conversation id: a short token of letters, digits,
/// `-` and `_` (UUIDs in practice).
pub fn is_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// Screen detection (Claude Code's interrupts and trust dialog, Codex's
// turns and approvals) lives in `crate::detect`.

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &Path, name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn resolves_bare_names_on_path_in_order_and_skips_non_executables() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        script(&first, "claude", 0o644);
        let found = script(&second, "claude", 0o755);
        script(&first, "codex", 0o755);
        std::fs::create_dir(second.join("codex")).unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();

        assert_eq!(resolve_program("claude", Some(&path)), Some(found));
        assert_eq!(
            resolve_program("codex", Some(&path)),
            Some(first.join("codex"))
        );
        assert_eq!(resolve_program("missing", Some(&path)), None);
        assert_eq!(resolve_program("claude", None), None);
        assert_eq!(resolve_program("", Some(&path)), None);
    }

    #[test]
    fn paths_with_a_slash_are_used_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(dir.path(), "agent", 0o755);
        let plain = script(dir.path(), "notes", 0o600);
        let exe_str = exe.to_str().unwrap();
        assert_eq!(resolve_program(exe_str, None), Some(exe.clone()));
        assert_eq!(resolve_program(plain.to_str().unwrap(), None), None);
        // A slash means "do not search", even when PATH has a match.
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_program("./agent-missing", Some(&path)), None);
    }

    #[test]
    fn kinds_serialize_as_lowercase_names() {
        for kind in SessionKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
        assert_eq!(SessionKind::default(), SessionKind::Shell);
        assert!(SessionKind::Claude.is_agent() && !SessionKind::Shell.is_agent());
    }

    fn report_prefix() -> Vec<String> {
        vec!["/usr/local/bin/fw".into(), "hub".into()]
    }

    /// Wired with the report command alone (no hub address yet).
    fn wired(report: &[String]) -> ClaudeWiring<'_> {
        ClaudeWiring {
            report: Some(report),
            ..ClaudeWiring::default()
        }
    }

    #[test]
    fn claude_settings_register_every_status_hook_without_a_shell() {
        let report = report_prefix();
        let settings = claude_settings(&wired(&report));
        let hooks = settings["hooks"].as_object().unwrap();
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionRequest",
            "Notification",
            "Stop",
            "StopFailure",
            "SessionEnd",
        ] {
            assert!(hooks.contains_key(event), "{event} missing");
        }
        let stop = &hooks["Stop"][0];
        assert!(stop.get("matcher").is_none());
        assert_eq!(
            stop["hooks"][0],
            serde_json::json!({
                "type": "command",
                "command": "/usr/local/bin/fw",
                "args": ["hub", "report", "--source", "claude", "--event", "Stop", "--stdin"],
                "timeout": CLAUDE_HOOK_TIMEOUT_SECS,
            })
        );
        let matchers: Vec<&str> = hooks["Notification"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["matcher"].as_str().unwrap())
            .collect();
        assert_eq!(
            matchers,
            [
                "permission_prompt",
                "elicitation_dialog",
                "elicitation_url_dialog",
                "agent_needs_input",
                "idle_prompt"
            ]
        );
        assert_eq!(
            hooks["Notification"][0]["hooks"][0]["args"],
            serde_json::json!([
                "hub",
                "report",
                "--source",
                "claude",
                "--event",
                "Notification",
                "--detail",
                "permission_prompt",
                "--stdin"
            ])
        );
        // Every status hook maps to a signal (nothing registered is
        // ignored); the telemetry-only ones change no status.
        for (event, groups) in hooks {
            if crate::telemetry::claude::EXTRA_HOOKS.contains(&event.as_str()) {
                assert!(claude_signal(event, None).is_none(), "{event}");
                continue;
            }
            for group in groups.as_array().unwrap() {
                let detail = group.get("matcher").and_then(|m| m.as_str());
                assert!(claude_signal(event, detail).is_some(), "{event} {detail:?}");
            }
        }
    }

    #[test]
    fn claude_args_pick_session_id_or_resume() {
        let id = "8d2c4f8e-2f4b-4d7e-9a51-0c6f3b0a1e22";
        let none = ClaudeWiring::default();
        assert_eq!(claude_args(id, false, &none), ["--session-id", id]);
        assert_eq!(claude_args(id, true, &wired(&[])), ["--resume", id]);
        let report = report_prefix();
        let args = claude_args(id, false, &wired(&report));
        assert_eq!(args[..3], ["--session-id", id, "--settings"]);
        assert_eq!(args.len(), 4, "no hub address: no progress tool");
        let settings: serde_json::Value = serde_json::from_str(&args[3]).unwrap();
        assert!(settings["hooks"]["Stop"].is_array());
    }

    #[test]
    fn a_listening_hub_gets_http_hooks_a_status_line_and_the_progress_tool() {
        let report = report_prefix();
        let theirs = serde_json::json!({
            "type": "command", "command": "~/.claude/line.sh 'x'", "padding": 0
        });
        let wiring = ClaudeWiring {
            report: Some(&report),
            hub_url: Some("http://127.0.0.1:7420"),
            http_hooks: true,
            status_line: Some(&theirs),
        };
        let settings = claude_settings(&wiring);
        // No process per event: every hook but SessionStart is a POST
        // that names its session from the environment.
        let pre = &settings["hooks"]["PreToolUse"][0]["hooks"][0];
        assert_eq!(pre["type"], "http");
        assert_eq!(pre["url"], "http://127.0.0.1:7420/api/telemetry/hook");
        assert_eq!(pre["headers"][SECRET_HEADER], "$FLYWHEEL_HUB_REPORT_SECRET");
        assert_eq!(pre["headers"][SESSION_HEADER], "$FLYWHEEL_HUB_SESSION");
        assert_eq!(
            pre["allowedEnvVars"],
            serde_json::json!(["FLYWHEEL_HUB_SESSION", "FLYWHEEL_HUB_REPORT_SECRET"])
        );
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["hooks"][0]["type"],
            "command"
        );
        for event in crate::telemetry::claude::EXTRA_HOOKS {
            assert_eq!(
                settings["hooks"][event][0]["hooks"][0]["type"], "http",
                "{event}"
            );
        }
        assert_eq!(
            settings["hooks"]["Notification"].as_array().unwrap().len(),
            5,
            "the matchers stay"
        );
        // The user's status line still shows: the hub's runs it.
        let line = &settings["statusLine"];
        assert_eq!(line["padding"], 0);
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        assert_eq!(
            line["command"],
            format!(
                "'/usr/local/bin/fw' 'hub' report --source claude --event statusline --stdin \
                 --chain {}",
                sh_quote("~/.claude/line.sh 'x'")
            )
        );
        let args = claude_args("id", false, &wiring);
        assert_eq!(args[4], "--mcp-config");
        let mcp: serde_json::Value = serde_json::from_str(&args[5]).unwrap();
        let server = &mcp["mcpServers"]["flywheel"];
        assert_eq!(server["url"], "http://127.0.0.1:7420/api/telemetry/mcp");
        assert_eq!(
            server["headers"][SECRET_HEADER],
            "${FLYWHEEL_HUB_REPORT_SECRET}"
        );
        assert_eq!(args[6], "--append-system-prompt");
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!([
                "mcp__flywheel__report_progress",
                "mcp__flywheel__ask_operator",
                "mcp__flywheel__handoff",
                "mcp__flywheel__list_sessions"
            ]),
            "the hub's tools never prompt"
        );
        // Without http hooks (an older CLI, a restricting policy) the
        // same events run the report command.
        let commands = claude_settings(&ClaudeWiring {
            http_hooks: false,
            ..wiring
        });
        assert_eq!(
            commands["hooks"]["SubagentStart"][0]["hooks"][0]["type"],
            "command"
        );
    }

    #[test]
    fn the_users_settings_are_read_most_specific_last() {
        let dir = tempfile::tempdir().unwrap();
        let (config, cwd) = (dir.path().join("config"), dir.path().join("repo"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        assert_eq!(
            claude_user_settings(Some(&config), &cwd),
            ClaudeUserSettings::default()
        );
        std::fs::write(
            config.join("settings.json"),
            r#"{"statusLine": {"type": "command", "command": "user"}}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(".claude/settings.local.json"),
            r#"{"statusLine": {"type": "command", "command": "local"}, "allowedHttpHookUrls": []}"#,
        )
        .unwrap();
        let found = claude_user_settings(Some(&config), &cwd);
        assert_eq!(found.status_line.unwrap()["command"], "local");
        assert!(found.restricts_http_hooks);
        assert_eq!(
            parse_claude_version("2.1.322 (Claude Code)"),
            Some((2, 1, 322))
        );
        assert_eq!(parse_claude_version("claude 2.2.0-beta.1"), Some((2, 2, 0)));
        assert_eq!(parse_claude_version("nope"), None);
    }

    #[test]
    fn launch_options_build_each_clis_flags_and_refuse_the_wrong_kind() {
        let claude = AgentLaunch {
            model: Some(" opus ".into()),
            permission_mode: Some("acceptEdits".into()),
            ..AgentLaunch::default()
        }
        .validated(SessionKind::Claude)
        .unwrap();
        assert_eq!(
            claude.args(SessionKind::Claude),
            ["--model", "opus", "--permission-mode", "acceptEdits"]
        );
        assert!(!claude.bypasses_permissions());
        let codex = AgentLaunch {
            model: Some("gpt-5-codex".into()),
            approval: Some("on-request".into()),
            sandbox: Some("workspace-write".into()),
            ..AgentLaunch::default()
        }
        .validated(SessionKind::Codex)
        .unwrap();
        assert_eq!(
            codex.args(SessionKind::Codex),
            [
                "-m",
                "gpt-5-codex",
                "-a",
                "on-request",
                "-s",
                "workspace-write"
            ]
        );
        // Blank fields are no fields; nothing set is no flags.
        let blank = AgentLaunch {
            model: Some("  ".into()),
            ..AgentLaunch::default()
        };
        let blank = blank.validated(SessionKind::Shell).unwrap();
        assert!(blank.is_empty() && blank.args(SessionKind::Claude).is_empty());
        let bypass = AgentLaunch {
            permission_mode: Some(CLAUDE_BYPASS_MODE.into()),
            ..AgentLaunch::default()
        };
        assert!(
            bypass
                .validated(SessionKind::Claude)
                .unwrap()
                .bypasses_permissions()
        );
        let refused = |launch: AgentLaunch, kind: SessionKind| launch.validated(kind).unwrap_err();
        let model = |m: &str| AgentLaunch {
            model: Some(m.into()),
            ..AgentLaunch::default()
        };
        assert!(refused(model("opus"), SessionKind::Shell).contains("shell"));
        assert!(
            refused(model("--dangerously-skip-permissions"), SessionKind::Claude).contains("model")
        );
        assert!(refused(model("a b"), SessionKind::Claude).contains("model"));
        assert!(
            model("claude-opus-5-5[1m]")
                .validated(SessionKind::Claude)
                .is_ok()
        );
        let mode = |m: &str| AgentLaunch {
            permission_mode: Some(m.into()),
            ..AgentLaunch::default()
        };
        assert!(refused(mode("yolo"), SessionKind::Claude).contains("acceptEdits"));
        assert!(refused(mode("plan"), SessionKind::Codex).contains("Claude"));
        let approval = AgentLaunch {
            approval: Some("never".into()),
            ..AgentLaunch::default()
        };
        assert!(refused(approval.clone(), SessionKind::Claude).contains("Codex"));
        let sandbox = AgentLaunch {
            sandbox: Some("everything".into()),
            ..AgentLaunch::default()
        };
        assert!(refused(sandbox, SessionKind::Codex).contains("read-only"));
    }

    #[test]
    fn prompts_go_last_after_a_double_dash() {
        assert_eq!(
            prompt_args(SessionKind::Claude, "-v means verbose\nfix it"),
            ["--", "-v means verbose\nfix it"]
        );
        assert_eq!(prompt_args(SessionKind::Codex, "review"), ["--", "review"]);
        assert!(prompt_args(SessionKind::Shell, "x").is_empty());
        assert_eq!(validate_prompt(None), Ok(None));
        assert_eq!(validate_prompt(Some(" \n ")), Ok(None));
        assert_eq!(validate_prompt(Some("go\n\n")), Ok(Some("go".into())));
        assert!(validate_prompt(Some("a\0b")).is_err());
        assert!(validate_prompt(Some(&"x".repeat(MAX_PROMPT_BYTES + 1))).is_err());
    }

    #[test]
    fn codex_notify_is_a_toml_array_of_the_report_command() {
        assert!(codex_args(None).is_empty());
        let args = codex_args(Some(&["/opt/my \"fw\"\\bin".to_owned(), "hub".to_owned()]));
        assert_eq!(
            args,
            [
                "-c",
                r#"notify=["/opt/my \"fw\"\\bin","hub","report","--source","codex","--event","notify"]"#
            ]
        );
        assert_eq!(toml_string("a\nb\u{1}"), "\"a\\nb\\u0001\"");
    }

    #[test]
    fn claude_signals_and_details() {
        assert_eq!(claude_signal("SessionStart", None), Some(Signal::Ready));
        assert_eq!(claude_signal("PreToolUse", None), Some(Signal::Working));
        assert_eq!(
            claude_signal("PermissionRequest", None),
            Some(Signal::Blocked)
        );
        assert_eq!(
            claude_signal("Notification", Some("idle_prompt")),
            Some(Signal::AwaitingInput)
        );
        assert_eq!(claude_signal("Notification", Some("auth_success")), None);
        assert_eq!(claude_signal("Notification", None), None);
        assert_eq!(claude_signal("Stop", None), Some(Signal::Done));
        assert_eq!(claude_signal("SessionEnd", None), Some(Signal::Ended));
        assert_eq!(claude_signal("FileChanged", None), None);
        let payload = serde_json::json!({ "message": "needs you", "tool_name": "Bash" });
        assert_eq!(
            claude_detail("Notification", Some("permission_prompt"), &payload).as_deref(),
            Some("needs you")
        );
        assert_eq!(
            claude_detail("PermissionRequest", None, &payload).as_deref(),
            Some("permission to use Bash")
        );
        assert_eq!(claude_detail("Stop", None, &payload), None);
        // No message: the notification type says what it is.
        let bare = serde_json::json!({});
        assert_eq!(
            claude_detail("Notification", Some("permission_prompt"), &bare).as_deref(),
            Some("Claude needs your permission")
        );
        assert_eq!(
            claude_detail("Notification", Some("auth_success"), &bare),
            None
        );
        assert_eq!(claude_detail("Notification", None, &bare), None);
        let question = serde_json::json!({
            "tool_name": "AskUserQuestion",
            "tool_input": { "questions": [{ "question": "Which color do you prefer?" }] },
        });
        assert_eq!(
            claude_detail("PermissionRequest", None, &question).as_deref(),
            Some("Which color do you prefer?")
        );
        for matcher in ["elicitation_url_dialog", "agent_needs_input"] {
            assert_eq!(
                claude_signal("Notification", Some(matcher)),
                Some(Signal::Blocked)
            );
        }
        let done = serde_json::json!({ "type": "agent-turn-complete", "thread-id": "t-1" });
        assert_eq!(codex_signal(&done), Some(Signal::Done));
        assert_eq!(codex_signal(&serde_json::json!({ "type": "other" })), None);
        assert_eq!(
            reported_conversation(SessionKind::Codex, &done).as_deref(),
            Some("t-1")
        );
        let bad = serde_json::json!({ "session_id": "../../etc" });
        assert_eq!(reported_conversation(SessionKind::Claude, &bad), None);
        assert_eq!(reported_conversation(SessionKind::Shell, &done), None);
    }

    #[test]
    fn claude_transcripts_are_found_by_project_dir_or_scan() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = Path::new("/home/me/code/my.repo_x");
        assert_eq!(claude_project_dir_name(cwd), "-home-me-code-my-repo-x");
        let id = "abc";
        assert_eq!(claude_transcript(dir.path(), cwd, id), None);
        let direct = dir.path().join("projects/-home-me-code-my-repo-x");
        std::fs::create_dir_all(&direct).unwrap();
        std::fs::write(direct.join("abc.jsonl"), "{}").unwrap();
        assert_eq!(
            claude_transcript(dir.path(), cwd, id),
            Some(direct.join("abc.jsonl"))
        );
        let hashed = dir.path().join("projects/-very-long-name-1a2b3c");
        std::fs::create_dir_all(&hashed).unwrap();
        std::fs::write(hashed.join("def.jsonl"), "{}").unwrap();
        assert_eq!(
            claude_transcript(dir.path(), cwd, "def"),
            Some(hashed.join("def.jsonl"))
        );
        assert_eq!(
            claude_config_dir(Some(OsStr::new("/cfg")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/cfg"))
        );
        assert_eq!(
            claude_config_dir(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.claude"))
        );
        assert_eq!(claude_config_dir(None, None), None);
    }

    #[test]
    fn codex_rollouts_are_found_by_thread_id() {
        let home = tempfile::tempdir().unwrap();
        let id = "0199a213-81c0-7800-8aa1-bbab2a035a53";
        assert_eq!(codex_rollout(home.path(), id), None);
        let day = home.path().join("sessions/2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-11-02T10-00-00-other.jsonl"), "").unwrap();
        assert_eq!(codex_rollout(home.path(), id), None);
        let file = day.join(format!("rollout-2026-11-02T10-00-00-{id}.jsonl"));
        std::fs::write(&file, "{}").unwrap();
        assert_eq!(codex_rollout(home.path(), id), Some(file));
        // Never a path built from an id that is not one.
        assert_eq!(codex_rollout(home.path(), "../x"), None);
        assert_eq!(
            codex_home(None, Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.codex"))
        );
        assert_eq!(
            codex_home(Some(OsStr::new("/c")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(codex_resume_args(id), ["resume", id]);
    }
}
