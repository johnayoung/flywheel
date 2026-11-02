//! Session kinds -- a plain shell, or one of the agent CLIs the hub
//! launches (Claude Code, Codex) -- resolving their programs on `PATH`, the
//! extra arguments that wire their status reporting back to the hub, and
//! what each report means.

use crate::status::Signal;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    ("Notification", Some("idle_prompt")),
    ("Stop", None),
    ("StopFailure", None),
    ("SessionEnd", None),
];

/// Seconds Claude Code waits on one status hook (a report takes
/// milliseconds; this only bounds a wedged hub).
pub const CLAUDE_HOOK_TIMEOUT_SECS: u64 = 10;

/// `--settings` JSON that makes Claude Code run `report` (an argv prefix
/// such as `[/path/to/fw, hub]`) with `report --source claude --event E
/// [--detail D] --stdin` on every status-relevant hook. The hook gets the
/// event's JSON on stdin; `args` spawns the program directly (no shell).
pub fn claude_settings(report: &[String]) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    for (event, detail) in CLAUDE_HOOKS {
        let mut args: Vec<String> = report[1..].to_vec();
        args.extend(["report", "--source", "claude", "--event", event].map(str::to_owned));
        if let Some(detail) = detail {
            args.extend(["--detail".to_owned(), (*detail).to_owned()]);
        }
        args.push("--stdin".to_owned());
        let mut group = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": report[0],
                "args": args,
                "timeout": CLAUDE_HOOK_TIMEOUT_SECS,
            }]
        });
        if let Some(detail) = detail {
            group["matcher"] = serde_json::Value::String((*detail).to_owned());
        }
        hooks
            .entry(event.to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .expect("hook groups are arrays")
            .push(group);
    }
    serde_json::json!({ "hooks": hooks })
}

/// Extra argv for a Claude Code session: its conversation id (`--resume`
/// when a transcript for it already exists, so a relaunch continues the
/// conversation) and, when the hub can take reports, the status hooks.
pub fn claude_args(session_id: &str, resume: bool, report: Option<&[String]>) -> Vec<String> {
    let flag = if resume { "--resume" } else { "--session-id" };
    let mut args = vec![flag.to_owned(), session_id.to_owned()];
    if let Some(report) = report.filter(|r| !r.is_empty()) {
        args.push("--settings".to_owned());
        args.push(claude_settings(report).to_string());
    }
    args
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

// --- reports -> signals ------------------------------------------------------

/// What a Claude Code hook event means for the session's status.
pub fn claude_signal(event: &str, detail: Option<&str>) -> Option<Signal> {
    Some(match event {
        "SessionStart" => Signal::Ready,
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Signal::Working,
        "PermissionRequest" => Signal::Blocked,
        "Notification" => match detail? {
            "permission_prompt" | "elicitation_dialog" => Signal::Blocked,
            "idle_prompt" => Signal::AwaitingInput,
            _ => return None,
        },
        "Stop" | "StopFailure" => Signal::Done,
        "SessionEnd" => Signal::Ended,
        _ => return None,
    })
}

/// A human line for a blocked Claude Code session, from its hook payload.
pub fn claude_detail(event: &str, payload: &serde_json::Value) -> Option<String> {
    let text = |key: &str| payload.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    match event {
        "Notification" => text("message"),
        "PermissionRequest" => text("tool_name").map(|tool| format!("permission to use {tool}")),
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

// --- codex screen heuristic --------------------------------------------------

/// The phrase Codex's TUI shows while a turn runs ("Esc to interrupt").
const CODEX_BUSY_PHRASE: &str = "esc to interrupt";
/// Output silence after which a busy indicator counts as gone.
pub const CODEX_QUIET_AFTER: Duration = Duration::from_secs(5);
/// Printable tail kept to match the phrase across output chunks.
const TAIL_CHARS: usize = 64;

/// Escape-sequence parser state, so a phrase split by colour codes or
/// cursor moves still matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Esc {
    Ground,
    /// Just saw ESC.
    Escape,
    /// In a CSI sequence (`ESC [` ... final byte).
    Csi,
    /// In an OSC/DCS/APC/PM/SOS string (ends with BEL or `ESC \`).
    Str,
    /// Saw ESC inside a string (maybe the `ESC \` terminator).
    StrEscape,
}

/// Conservative "Codex is working" detection from its raw output: busy
/// when its busy phrase is drawn, quiet once the output has been silent
/// for [`CODEX_QUIET_AFTER`] (a running turn redraws its timer and
/// spinner continuously). Typing never makes it busy.
#[derive(Debug)]
pub struct CodexActivity {
    esc: Esc,
    tail: String,
    busy: bool,
    last_output: Option<Instant>,
}

impl Default for CodexActivity {
    fn default() -> Self {
        Self {
            esc: Esc::Ground,
            tail: String::new(),
            busy: false,
            last_output: None,
        }
    }
}

impl CodexActivity {
    /// Feed output that arrived at `now`; `Some(Busy)` when the busy
    /// indicator was drawn.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Option<Signal> {
        self.last_output = Some(now);
        let mut matched = false;
        for chunk in String::from_utf8_lossy(bytes).chars() {
            if let Some(c) = self.printable(chunk) {
                self.tail.extend(c.to_lowercase());
                if self.tail.ends_with(CODEX_BUSY_PHRASE) {
                    matched = true;
                    self.tail.clear();
                }
                let excess = self.tail.chars().count().saturating_sub(TAIL_CHARS);
                if excess > 0 {
                    self.tail = self.tail.chars().skip(excess).collect();
                }
            }
        }
        if matched {
            self.busy = true;
            return Some(Signal::Busy);
        }
        None
    }

    /// `Some(Quiet)` once a busy indicator has been silent long enough.
    pub fn tick(&mut self, now: Instant) -> Option<Signal> {
        let silent = self
            .last_output
            .is_none_or(|at| now.saturating_duration_since(at) >= CODEX_QUIET_AFTER);
        if self.busy && silent {
            self.busy = false;
            return Some(Signal::Quiet);
        }
        None
    }

    /// Run one char through the escape parser; the char if it is text.
    fn printable(&mut self, c: char) -> Option<char> {
        const ESC: char = '\u{1b}';
        const BEL: char = '\u{7}';
        match self.esc {
            Esc::Ground => match c {
                ESC => {
                    self.esc = Esc::Escape;
                    None
                }
                // Cursor moves often separate words on a redraw.
                '\r' | '\n' | '\t' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            },
            Esc::Escape => {
                self.esc = match c {
                    '[' => Esc::Csi,
                    ']' | 'P' | '_' | '^' | 'X' => Esc::Str,
                    _ => Esc::Ground,
                };
                None
            }
            Esc::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    self.esc = Esc::Ground;
                    // A cursor move: a diffing TUI (ratatui) jumps over
                    // unchanged cells -- often the blank between two
                    // words -- so it reads as a word gap.
                    if matches!(c, 'H' | 'f' | 'A' | 'B' | 'E' | 'F' | 'G' | 'd') {
                        return Some(' ');
                    }
                }
                None
            }
            Esc::Str => {
                match c {
                    BEL => self.esc = Esc::Ground,
                    ESC => self.esc = Esc::StrEscape,
                    _ => {}
                }
                None
            }
            Esc::StrEscape => {
                self.esc = if c == '\\' { Esc::Ground } else { Esc::Str };
                None
            }
        }
    }
}

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

    #[test]
    fn claude_settings_register_every_status_hook_without_a_shell() {
        let settings = claude_settings(&report_prefix());
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
            ["permission_prompt", "elicitation_dialog", "idle_prompt"]
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
        // Every hook maps to a signal (nothing registered is ignored).
        for (event, groups) in hooks {
            for group in groups.as_array().unwrap() {
                let detail = group.get("matcher").and_then(|m| m.as_str());
                assert!(claude_signal(event, detail).is_some(), "{event} {detail:?}");
            }
        }
    }

    #[test]
    fn claude_args_pick_session_id_or_resume() {
        let id = "8d2c4f8e-2f4b-4d7e-9a51-0c6f3b0a1e22";
        assert_eq!(claude_args(id, false, None), ["--session-id", id]);
        assert_eq!(claude_args(id, true, Some(&[])), ["--resume", id]);
        let args = claude_args(id, false, Some(&report_prefix()));
        assert_eq!(args[..3], ["--session-id", id, "--settings"]);
        let settings: serde_json::Value = serde_json::from_str(&args[3]).unwrap();
        assert!(settings["hooks"]["Stop"].is_array());
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
            claude_detail("Notification", &payload).as_deref(),
            Some("needs you")
        );
        assert_eq!(
            claude_detail("PermissionRequest", &payload).as_deref(),
            Some("permission to use Bash")
        );
        assert_eq!(claude_detail("Stop", &payload), None);
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
    fn codex_activity_sees_the_busy_line_through_escapes_and_chunks() {
        let t0 = Instant::now();
        let mut a = CodexActivity::default();
        assert_eq!(a.feed(b"typing esc to interr", t0), None);
        assert_eq!(a.tick(t0 + CODEX_QUIET_AFTER * 2), None);
        let mut a = CodexActivity::default();
        assert_eq!(
            a.feed(b"\x1b[1mWorking\x1b[0m (0s \xe2\x80\xa2 Esc to ", t0),
            None
        );
        assert_eq!(
            a.feed(b"\x1b]0;title\x07\x1b[2minterrupt\x1b[0m)", t0),
            Some(Signal::Busy)
        );
        // Redraws keep it busy; silence ends it once.
        let later = t0 + CODEX_QUIET_AFTER - Duration::from_millis(1);
        assert_eq!(a.feed(b"\x1b[10G3s", later), None);
        assert_eq!(a.tick(later + Duration::from_millis(10)), None);
        assert_eq!(a.tick(later + CODEX_QUIET_AFTER), Some(Signal::Quiet));
        assert_eq!(a.tick(later + CODEX_QUIET_AFTER * 3), None);
        // A diffing TUI skips unchanged blank cells with a cursor move:
        // that reads as the gap between words; other escapes are not gaps.
        let mut a = CodexActivity::default();
        assert_eq!(a.feed(b"esc to\x1b[3;12Hinterrupt", t0), Some(Signal::Busy));
        let mut a = CodexActivity::default();
        assert_eq!(a.feed(b"esc\x1b[Kto interrupt", t0), None);
    }
}
