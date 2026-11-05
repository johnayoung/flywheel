//! Plugins: local executables the hub runs on its events and as palette
//! actions (herdr's model, local only).
//!
//! A plugin is a directory `<state-dir>/plugins/<name>/` with a
//! `plugin.toml` naming what it subscribes to and what it offers:
//!
//! ```toml
//! description = "Ping me when an agent is stuck"
//! command = ["./hook.sh"]          # argv, no shell; ./ is the plugin dir
//! timeout_secs = 10                # 1..=300
//! [[events]]
//! on = "status.changed"
//! statuses = ["blocked", "done"]   # status.changed only
//! kinds = ["claude"]               # sessions of these kinds only
//! [[actions]]
//! id = "summarize"
//! title = "Summarize this session"
//! context = "session"              # needs an attached session (else "hub")
//! command = ["./summarize.py"]     # else the plugin's command
//! ```
//!
//! Each run: the command, never a shell, cwd the plugin dir, a scrubbed
//! environment (an allowlist: `PATH`, `HOME`, locale, XDG, display; never
//! the hub's token, peer secrets, report secrets, or database DSNs), one
//! JSON document on stdin, a deadline (the process group is killed at
//! it), stdout and stderr capped. Stdout that is a JSON object is the
//! response: `{"toast": "text" | {"message","tone"}}` shows a toast in
//! every open UI; an action may also return `{"paste": {"text","run"?,
//! "session"?}}`, pasted into the session (as `POST /paste`). Every run is
//! logged to `<state-dir>/plugin-logs/<name>.jsonl` (`fw hub plugin logs`).
//!
//! A plugin found on disk is **disabled** until enabled (`fw hub plugin
//! enable`, or Settings): `<state-dir>/plugins.json`. Plugins run as the
//! hub's user with everything that user can do -- the hub reviews and
//! sandboxes nothing; the UI can toggle and run them but never installs
//! one (there is no download path).

use crate::hub::{Hub, HubEvent};
use crate::util::file_identity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use tokio::sync::broadcast::error::RecvError;

/// The plugins directory in the state dir.
pub const PLUGINS_DIR: &str = "plugins";
/// Each plugin's manifest.
pub const MANIFEST_FILE: &str = "plugin.toml";
/// Which plugins are enabled.
pub const ENABLED_FILE: &str = "plugins.json";
/// Where run logs go.
pub const LOG_DIR: &str = "plugin-logs";
/// Every event a plugin can subscribe to.
pub const EVENTS: &[&str] = &[
    "hub.started",
    "session.created",
    "session.exited",
    "session.removed",
    "status.changed",
    "review.committed",
    "review.pushed",
    "review.pr_opened",
    "review.rebased",
    "review.merged",
    "review.requested",
    "handoff.sent",
    "fanout.finished",
    "share.created",
    "share.revoked",
    "record.started",
    "record.stopped",
    "queue.delivered",
    "queue.stalled",
    "perms.allowed",
    "pr.checks_failed",
    "pr.checks_passed",
    "pr.review_changes_requested",
    "pr.merged",
    "autopilot.started",
    "autopilot.stopped",
    "autopilot.decision",
    "autopilot.approval_needed",
    "autopilot.approved",
    "autopilot.denied",
    "digest.ready",
];
const STATUSES: &[&str] = &["working", "blocked", "done", "idle", "unknown"];
const KINDS: &[&str] = &["shell", "claude", "codex"];

/// Default and largest per-run deadline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_TIMEOUT_SECS: u64 = 300;
/// Output kept per stream (the rest is read and dropped).
pub const OUTPUT_CAP: usize = 64 * 1024;
/// Output kept per stream in a log entry.
const LOG_OUTPUT: usize = 4 * 1024;
/// A log file is trimmed to its newest [`LOG_KEEP`] entries past this.
const LOG_TRIM_BYTES: u64 = 512 * 1024;
const LOG_KEEP: usize = 200;
/// Runs waiting for a slot before more are dropped.
const MAX_QUEUED: usize = 64;
/// Grace between SIGTERM and SIGKILL at the deadline.
const KILL_GRACE: Duration = Duration::from_millis(500);
/// How often the plugins directory is looked at for changes.
const SCAN_POLL: Duration = Duration::from_secs(2);
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ARGS: usize = 32;
const MAX_ARG_CHARS: usize = 4096;
const MAX_ITEMS: usize = 32;
/// Toast text kept (characters).
const MAX_TOAST: usize = 300;
/// Largest paste a plugin may ask for.
const MAX_PASTE: usize = 64 * 1024;

/// Environment variables a plugin inherits from the hub (everything else
/// is dropped, secrets included).
pub const ENV_ALLOW: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    "TMPDIR",
    "TERM",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    command: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    events: Vec<EventFile>,
    #[serde(default)]
    actions: Vec<ActionFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventFile {
    on: String,
    statuses: Option<Vec<String>>,
    kinds: Option<Vec<String>>,
    command: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionFile {
    id: String,
    title: String,
    context: Option<String>,
    command: Option<Vec<String>>,
}

/// An event subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subscription {
    pub on: String,
    pub statuses: Vec<String>,
    pub kinds: Vec<String>,
    pub command: Vec<String>,
}

impl Subscription {
    /// Whether an event with `payload` is one this subscription takes:
    /// its `statuses` (when set) name the payload's `status`, its `kinds`
    /// the session's kind. (`"done"` hears every finished turn, watched or
    /// not, once: the `done` each one says. It no longer also takes a
    /// shown `idle` marked `turn_finished`: that one follows its turn's
    /// `done`, and taking it too would say the turn twice.)
    pub fn takes(&self, payload: &Value) -> bool {
        let status = payload.get("status").and_then(Value::as_str);
        let kind = payload.pointer("/session/kind").and_then(Value::as_str);
        let status_ok = self.statuses.is_empty()
            || status.is_some_and(|s| self.statuses.iter().any(|w| w == s));
        let kind_ok =
            self.kinds.is_empty() || kind.is_some_and(|k| self.kinds.iter().any(|w| w == k));
        status_ok && kind_ok
    }
}

/// What a plugin action needs to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    /// Nothing (runs from anywhere).
    Hub,
    /// A session (the attached one in the UI).
    Session,
}

/// A palette action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionDecl {
    pub id: String,
    pub title: String,
    pub context: Context,
    pub command: Vec<String>,
}

/// A validated manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub description: Option<String>,
    pub version: Option<String>,
    pub timeout_secs: u64,
    pub events: Vec<Subscription>,
    pub actions: Vec<ActionDecl>,
}

/// A plugin directory's name: `[a-z0-9][a-z0-9_-]{0,31}`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn valid_action_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn check_argv(what: &str, argv: &[String]) -> Result<(), String> {
    if argv.is_empty() || argv[0].trim().is_empty() {
        return Err(format!("{what}: command must name a program"));
    }
    if argv.len() > MAX_ARGS {
        return Err(format!("{what}: at most {MAX_ARGS} arguments"));
    }
    if argv
        .iter()
        .any(|a| a.chars().count() > MAX_ARG_CHARS || a.contains('\0'))
    {
        return Err(format!(
            "{what}: arguments are at most {MAX_ARG_CHARS} characters, no NUL"
        ));
    }
    Ok(())
}

/// Parse and validate a manifest for plugin `name`.
pub fn parse_manifest(name: &str, text: &str) -> Result<Manifest, String> {
    let file: ManifestFile =
        toml::from_str(text).map_err(|e| e.to_string().trim_end().to_owned())?;
    if let Some(declared) = &file.name
        && declared != name
    {
        return Err(format!(
            "name = {declared:?} but the plugin's directory is {name:?}"
        ));
    }
    let timeout_secs = file.timeout_secs.unwrap_or(DEFAULT_TIMEOUT.as_secs());
    if !(1..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
        return Err(format!("timeout_secs must be 1..={MAX_TIMEOUT_SECS}"));
    }
    if let Some(command) = &file.command {
        check_argv("command", command)?;
    }
    if file.events.len() > MAX_ITEMS || file.actions.len() > MAX_ITEMS {
        return Err(format!(
            "at most {MAX_ITEMS} events and {MAX_ITEMS} actions"
        ));
    }
    if file.events.is_empty() && file.actions.is_empty() {
        return Err("declares no [[events]] and no [[actions]]".into());
    }
    let command_for = |what: &str, own: &Option<Vec<String>>| -> Result<Vec<String>, String> {
        match own.as_ref().or(file.command.as_ref()) {
            Some(argv) => {
                check_argv(what, argv)?;
                Ok(argv.clone())
            }
            None => Err(format!("{what}: no command (and no top-level command)")),
        }
    };
    let mut events = Vec::new();
    for (i, event) in file.events.iter().enumerate() {
        let what = format!("events[{i}] ({})", event.on);
        if !EVENTS.contains(&event.on.as_str()) {
            return Err(format!(
                "{what}: unknown event; one of {}",
                EVENTS.join(", ")
            ));
        }
        let statuses = event.statuses.clone().unwrap_or_default();
        if !statuses.is_empty() && event.on != "status.changed" {
            return Err(format!("{what}: statuses is for status.changed only"));
        }
        if let Some(bad) = statuses.iter().find(|s| !STATUSES.contains(&s.as_str())) {
            return Err(format!(
                "{what}: status {bad:?} is not one of {}",
                STATUSES.join(", ")
            ));
        }
        let kinds = event.kinds.clone().unwrap_or_default();
        if let Some(bad) = kinds.iter().find(|k| !KINDS.contains(&k.as_str())) {
            return Err(format!(
                "{what}: kind {bad:?} is not one of {}",
                KINDS.join(", ")
            ));
        }
        events.push(Subscription {
            on: event.on.clone(),
            statuses,
            kinds,
            command: command_for(&what, &event.command)?,
        });
    }
    let mut actions: Vec<ActionDecl> = Vec::new();
    for (i, action) in file.actions.iter().enumerate() {
        let what = format!("actions[{i}]");
        if !valid_action_id(&action.id) {
            return Err(format!(
                "{what}: id {:?} must be 1-32 of [a-z0-9_-]",
                action.id
            ));
        }
        if actions.iter().any(|a| a.id == action.id) {
            return Err(format!("{what}: id {:?} twice", action.id));
        }
        let title = action.title.trim();
        if title.is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control) {
            return Err(format!("{what}: title must be 1-80 printable characters"));
        }
        let context = match action.context.as_deref().unwrap_or("hub") {
            "hub" => Context::Hub,
            "session" => Context::Session,
            other => return Err(format!("{what}: context {other:?} is not hub or session")),
        };
        actions.push(ActionDecl {
            id: action.id.clone(),
            title: title.to_owned(),
            context,
            command: command_for(&what, &action.command)?,
        });
    }
    Ok(Manifest {
        description: file.description.map(|d| d.chars().take(200).collect()),
        version: file.version.map(|v| v.chars().take(40).collect()),
        timeout_secs,
        events,
        actions,
    })
}

/// A plugin found on disk.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub dir: PathBuf,
    pub manifest: Result<Manifest, String>,
    pub enabled: bool,
}

/// One run, as logged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunLog {
    pub at: chrono::DateTime<chrono::Utc>,
    pub plugin: String,
    /// `event:<name>` or `action:<id>`.
    pub trigger: String,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub ok: bool,
    pub error: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub response: Option<Value>,
}

/// A run's parsed response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Response {
    pub toast: Option<Toast>,
    pub paste: Option<PasteRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Toast {
    pub message: String,
    pub tone: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PasteRequest {
    pub text: String,
    pub run: bool,
    pub session: Option<String>,
}

/// Control characters (but newlines) out, cut to `max` characters.
fn clean_text(text: &str, max: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(max)
        .collect()
}

/// Parse stdout as a response: `Ok(None)` for no JSON object (plain
/// output is just logged), else the response or why it is malformed.
pub fn parse_response(stdout: &str) -> Result<Option<Response>, String> {
    let trimmed = stdout.trim();
    if !trimmed.starts_with('{') {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("response is not JSON: {e}"))?;
    let Some(object) = value.as_object() else {
        return Err("response must be a JSON object".into());
    };
    let mut response = Response::default();
    for (key, value) in object {
        match key.as_str() {
            "toast" => {
                let (message, tone) = match value {
                    Value::String(message) => (message.clone(), "info".to_owned()),
                    Value::Object(toast) => (
                        toast
                            .get("message")
                            .and_then(Value::as_str)
                            .ok_or("toast.message must be a string")?
                            .to_owned(),
                        toast
                            .get("tone")
                            .and_then(Value::as_str)
                            .unwrap_or("info")
                            .to_owned(),
                    ),
                    _ => return Err("toast must be a string or {\"message\",\"tone\"}".into()),
                };
                if !["info", "warn", "error", "ok"].contains(&tone.as_str()) {
                    return Err("toast.tone must be info, ok, warn, or error".into());
                }
                let message = clean_text(message.trim(), MAX_TOAST);
                if message.is_empty() {
                    return Err("toast.message is empty".into());
                }
                response.toast = Some(Toast { message, tone });
            }
            "paste" => {
                let Some(paste) = value.as_object() else {
                    return Err("paste must be {\"text\",\"run\"?,\"session\"?}".into());
                };
                let text = paste
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("paste.text must be a string")?;
                if text.len() > MAX_PASTE {
                    return Err(format!("paste.text is over {MAX_PASTE} bytes"));
                }
                response.paste = Some(PasteRequest {
                    text: text.to_owned(),
                    run: paste.get("run").and_then(Value::as_bool).unwrap_or(false),
                    session: paste
                        .get("session")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            other => return Err(format!("response key {other:?} is not toast or paste")),
        }
    }
    Ok(Some(response))
}

/// What running a command produced.
#[derive(Debug, Clone)]
struct Ran {
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    /// The command ended but something it started (outside its process
    /// group, so not killed) kept stdout/stderr open: the run was cut.
    held_open: bool,
    stdout: String,
    stderr: String,
    /// Stdout went past [`OUTPUT_CAP`] (the rest was dropped).
    stdout_cut: bool,
    truncated: bool,
    spawn_error: Option<String>,
}

/// What a reader kept: the first bytes, and whether it dropped any.
type Sink = Arc<Mutex<(Vec<u8>, bool)>>;

/// Read `stream` to its end into `sink`, keeping the first `cap` bytes
/// (what was read so far stays in `sink` if the read is aborted).
async fn capped(mut stream: impl tokio::io::AsyncRead + Unpin, cap: usize, sink: Sink) {
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut kept = sink.lock().unwrap_or_else(PoisonError::into_inner);
                let room = cap.saturating_sub(kept.0.len());
                if n > room {
                    kept.1 = true;
                }
                kept.0.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

fn take(sink: &Sink) -> (Vec<u8>, bool) {
    std::mem::take(&mut *sink.lock().unwrap_or_else(PoisonError::into_inner))
}

#[cfg(unix)]
fn signal_group(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid.and_then(|p| libc::pid_t::try_from(p).ok()) {
        // The child leads its own process group (`process_group(0)`).
        unsafe {
            libc::killpg(pid, signal);
        }
    }
}

/// Kills a run's process group and stops its pipe tasks if the run is
/// dropped before it finished (the caller went away, the runtime shut
/// down): nothing a plugin started outlives the run that way.
struct RunGuard {
    /// The group to kill; `None` once the child is reaped (its pid may
    /// then be reused).
    pid: Option<u32>,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        signal_group(self.pid, libc::SIGKILL);
        // Finished tasks ignore this.
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Run `argv` in `dir` with `env`, `input` on stdin, killed at `timeout`.
/// The whole run is bounded: `timeout` for the command, then a grace for
/// its group to die and its pipes to close; output still held open past
/// that (by a process that left the group) is cut, and the run ends.
async fn run_command(
    dir: &Path,
    argv: &[String],
    env: &[(OsString, OsString)],
    input: Vec<u8>,
    timeout: Duration,
) -> Ran {
    let program = resolve_program(dir, &argv[0], env);
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&argv[1..])
        .current_dir(dir)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Ran {
                exit_code: None,
                signal: None,
                timed_out: false,
                held_open: false,
                stdout: String::new(),
                stderr: String::new(),
                stdout_cut: false,
                truncated: false,
                spawn_error: Some(format!("cannot run {}: {e}", program.display())),
            };
        }
    };
    let pid = child.id();
    let stdin = child.stdin.take();
    let writer = tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            // A plugin that never reads stdin is fine (EPIPE ignored).
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        }
    });
    let out_sink: Sink = Arc::default();
    let err_sink: Sink = Arc::default();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out = tokio::spawn({
        let sink = Arc::clone(&out_sink);
        async move {
            if let Some(s) = stdout {
                capped(s, OUTPUT_CAP, sink).await;
            }
        }
    });
    let mut err = tokio::spawn({
        let sink = Arc::clone(&err_sink);
        async move {
            if let Some(s) = stderr {
                capped(s, OUTPUT_CAP, sink).await;
            }
        }
    });
    let mut guard = RunGuard {
        pid,
        tasks: vec![
            writer.abort_handle(),
            out.abort_handle(),
            err.abort_handle(),
        ],
    };
    let mut timed_out = false;
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            timed_out = true;
            #[cfg(unix)]
            {
                signal_group(pid, libc::SIGTERM);
                if tokio::time::timeout(KILL_GRACE, child.wait())
                    .await
                    .is_err()
                {
                    signal_group(pid, libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    };
    // Whatever is left of the group (a backgrounded grandchild holding
    // the pipes) goes too, so the readers end. A process that left the
    // group (setsid) is out of reach: its pipes get a grace, then the
    // readers are stopped with what they have.
    #[cfg(unix)]
    signal_group(pid, libc::SIGKILL);
    guard.pid = None;
    writer.abort();
    let drained = tokio::time::timeout(KILL_GRACE, async {
        let _ = (&mut out).await;
        let _ = (&mut err).await;
    })
    .await
    .is_ok();
    if !drained {
        out.abort();
        err.abort();
    }
    let (stdout, out_cut) = take(&out_sink);
    let (stderr, err_cut) = take(&err_sink);
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.and_then(|s| s.signal())
    };
    #[cfg(not(unix))]
    let signal = None;
    Ran {
        exit_code: status.and_then(|s| s.code()),
        signal,
        timed_out,
        held_open: !drained,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        stdout_cut: out_cut,
        truncated: out_cut || err_cut,
        spawn_error: None,
    }
}

/// `argv[0]`: a path (with a `/`) is the plugin dir's; a bare name is
/// looked up on the plugin's `PATH`.
fn resolve_program(dir: &Path, program: &str, env: &[(OsString, OsString)]) -> PathBuf {
    if program.contains('/') {
        return dir.join(program);
    }
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_os_str());
    crate::agent::resolve_program(program, path).unwrap_or_else(|| PathBuf::from(program))
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// How the host runs plugins.
#[derive(Debug, Clone)]
pub struct HostOptions {
    /// Runs at once, hub-wide.
    pub max_concurrent: usize,
    /// The environment plugins draw their allowlist from (the hub's own).
    pub base_env: Vec<(OsString, OsString)>,
    /// `hub.toml` `plugins.enabled`.
    pub enabled: bool,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            max_concurrent: crate::config::DEFAULT_PLUGIN_CONCURRENCY,
            base_env: std::env::vars_os().collect(),
            enabled: true,
        }
    }
}

type ScanPrint = Vec<(String, Option<((u64, u64), u64, Option<SystemTime>)>)>;

#[derive(Debug, Default)]
struct Scanned {
    print: Option<ScanPrint>,
    plugins: Arc<Vec<Plugin>>,
}

/// What `enabled` names, from `plugins.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct EnabledFile {
    #[serde(default)]
    enabled: BTreeSet<String>,
}

/// Why an action did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    NotFound(String),
    Refused(String),
}

/// What running an action did.
#[derive(Debug, Clone, Serialize)]
pub struct ActionResult {
    pub ok: bool,
    pub toast: Option<Toast>,
    pub paste: Option<crate::transcript::PasteOutcome>,
    pub log: RunLog,
}

/// The hub's plugins: discovery, enablement, runs, logs.
pub struct PluginHost {
    state_dir: PathBuf,
    enabled: AtomicBool,
    slots: Arc<Semaphore>,
    queued: AtomicUsize,
    base_env: Vec<(OsString, OsString)>,
    scanned: Mutex<Scanned>,
    log_lock: Mutex<()>,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHost")
            .field("state_dir", &self.state_dir)
            .finish_non_exhaustive()
    }
}

impl PluginHost {
    pub fn new(state_dir: &Path, options: HostOptions) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            enabled: AtomicBool::new(options.enabled),
            slots: Arc::new(Semaphore::new(options.max_concurrent.max(1))),
            queued: AtomicUsize::new(0),
            base_env: options.base_env,
            scanned: Mutex::new(Scanned::default()),
            log_lock: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> PathBuf {
        self.state_dir.join(PLUGINS_DIR)
    }

    /// `hub.toml` `plugins.enabled` (off: nothing runs).
    pub fn set_master(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    pub fn master(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn print(&self) -> ScanPrint {
        let meta = |path: &Path| {
            std::fs::metadata(path)
                .ok()
                .map(|m| (file_identity(&m), m.len(), m.modified().ok()))
        };
        let mut print = vec![(
            ENABLED_FILE.to_owned(),
            meta(&self.state_dir.join(ENABLED_FILE)),
        )];
        if let Ok(entries) = std::fs::read_dir(self.dir()) {
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for name in names {
                let manifest = self.dir().join(&name).join(MANIFEST_FILE);
                print.push((name, meta(&manifest)));
            }
        }
        print
    }

    /// The plugins on disk (re-read when anything there changed). Returns
    /// whether the list changed.
    fn rescan(&self) -> (Arc<Vec<Plugin>>, bool) {
        let print = self.print();
        let mut scanned = self.scanned.lock().unwrap_or_else(PoisonError::into_inner);
        if scanned.print.as_ref() == Some(&print) {
            return (Arc::clone(&scanned.plugins), false);
        }
        let enabled = read_enabled(&self.state_dir);
        let mut plugins = Vec::new();
        for (name, _) in print.iter().skip(1) {
            let dir = self.dir().join(name);
            if !dir.is_dir() || name.starts_with('.') {
                continue;
            }
            let manifest = if !valid_name(name) {
                Err(format!(
                    "the directory name {name:?} is not a plugin name (1-32 of [a-z0-9_-])"
                ))
            } else {
                read_manifest(&dir.join(MANIFEST_FILE)).and_then(|text| parse_manifest(name, &text))
            };
            plugins.push(Plugin {
                enabled: enabled.contains(name),
                name: name.clone(),
                dir,
                manifest,
            });
        }
        scanned.print = Some(print);
        scanned.plugins = Arc::new(plugins);
        (Arc::clone(&scanned.plugins), true)
    }

    /// Every plugin found.
    pub fn plugins(&self) -> Arc<Vec<Plugin>> {
        self.rescan().0
    }

    pub fn plugin(&self, name: &str) -> Option<Plugin> {
        self.plugins().iter().find(|p| p.name == name).cloned()
    }

    /// Enable or disable `name` (written to `plugins.json`).
    pub fn set_enabled(&self, name: &str, on: bool) -> Result<(), String> {
        set_enabled(&self.state_dir, name, on)?;
        self.rescan();
        Ok(())
    }

    /// `GET /api/plugins`: every plugin with its manifest (or error),
    /// enabled state, and last run.
    pub fn view(&self) -> Value {
        let plugins: Vec<Value> = self
            .plugins()
            .iter()
            .map(|p| plugin_view(&self.state_dir, p))
            .collect();
        json!({
            "enabled": self.master(),
            "dir": self.dir().to_string_lossy(),
            "plugins": plugins,
            "events": EVENTS,
        })
    }

    /// The environment a run gets: the allowlist from the hub's, plus the
    /// plugin's own variables.
    fn env_for(&self, plugin: &Plugin, trigger: &str) -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = self
            .base_env
            .iter()
            .filter(|(k, _)| k.to_str().is_some_and(|k| ENV_ALLOW.contains(&k)))
            .cloned()
            .collect();
        env.extend([
            ("FW_HUB_PLUGIN".into(), plugin.name.clone().into()),
            (
                "FW_HUB_PLUGIN_DIR".into(),
                plugin.dir.clone().into_os_string(),
            ),
            ("FW_HUB_PLUGIN_TRIGGER".into(), trigger.into()),
            (
                "FW_HUB_STATE_DIR".into(),
                self.state_dir.clone().into_os_string(),
            ),
        ]);
        env
    }

    /// Run one of `plugin`'s commands for `trigger` with `input` on
    /// stdin, waiting for a slot (the caller logs the run once it acted
    /// on the response).
    pub async fn run(
        &self,
        plugin: &Plugin,
        command: &[String],
        trigger: &str,
        input: &Value,
    ) -> (RunLog, Option<Response>) {
        let timeout = plugin
            .manifest
            .as_ref()
            .map_or(DEFAULT_TIMEOUT, |m| Duration::from_secs(m.timeout_secs));
        let _slot = Arc::clone(&self.slots).acquire_owned().await;
        let started = Instant::now();
        let at = chrono::Utc::now();
        let env = self.env_for(plugin, trigger);
        let bytes = serde_json::to_vec(input).unwrap_or_default();
        let ran = run_command(&plugin.dir, command, &env, bytes, timeout).await;
        let mut error = ran.spawn_error.clone();
        if ran.timed_out {
            error = Some(format!("timed out after {}s; killed", timeout.as_secs()));
        } else if ran.held_open {
            error = Some(
                "its output stayed open after it exited: a process it started left its \
                 process group (setsid, a daemon) and is not the hub's to kill; the run was cut"
                    .into(),
            );
        }
        let parsed = if ran.stdout_cut && ran.stdout.trim_start().starts_with('{') {
            Err(format!(
                "response is over the {} KiB output cap",
                OUTPUT_CAP / 1024
            ))
        } else {
            parse_response(&ran.stdout)
        };
        let response = match parsed {
            Ok(response) => response,
            Err(message) => {
                error.get_or_insert(message);
                None
            }
        };
        if error.is_none() && ran.exit_code != Some(0) {
            error = Some(match (ran.exit_code, ran.signal) {
                (Some(code), _) => format!("exited {code}"),
                (None, Some(signal)) => format!("killed by signal {signal}"),
                (None, None) => "ended without a status".into(),
            });
        }
        let log = RunLog {
            at,
            plugin: plugin.name.clone(),
            trigger: trigger.to_owned(),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            exit_code: ran.exit_code,
            signal: ran.signal,
            timed_out: ran.timed_out,
            ok: error.is_none(),
            error,
            stdout: cut(&ran.stdout, LOG_OUTPUT),
            stderr: cut(&ran.stderr, LOG_OUTPUT),
            truncated: ran.truncated,
            response: response.as_ref().and_then(|r| serde_json::to_value(r).ok()),
        };
        (log, response)
    }

    fn append_log(&self, log: &RunLog) {
        let _guard = self.log_lock.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(e) = append_log(&self.state_dir, log) {
            eprintln!("fw hub: plugin {}: cannot write its log: {e}", log.plugin);
        }
        if !log.ok {
            eprintln!(
                "fw hub: plugin {} ({}): {}",
                log.plugin,
                log.trigger,
                log.error.as_deref().unwrap_or("failed")
            );
        }
    }

    /// Run every enabled subscription to `event` (each on its own task;
    /// waits for a slot). `payload` is the event's data. Returns how many
    /// runs were started.
    pub fn dispatch(self: &Arc<Self>, hub: &Arc<Hub>, event: &str, payload: Value) -> usize {
        if !self.master() {
            return 0;
        }
        let mut started = 0;
        for plugin in self.plugins().iter().filter(|p| p.enabled) {
            let Ok(manifest) = &plugin.manifest else {
                continue;
            };
            for sub in manifest.events.iter().filter(|s| s.on == event) {
                if !sub.takes(&payload) {
                    continue;
                }
                if self.queued.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUED {
                    self.queued.fetch_sub(1, Ordering::SeqCst);
                    eprintln!(
                        "fw hub: plugin {}: {event} dropped ({MAX_QUEUED} runs already waiting)",
                        plugin.name
                    );
                    continue;
                }
                let input = json!({
                    "version": 1,
                    "type": "event",
                    "plugin": plugin.name,
                    "event": event,
                    "at": chrono::Utc::now(),
                    "hub": {"version": env!("CARGO_PKG_VERSION")},
                    "payload": payload,
                });
                let host = Arc::clone(self);
                let plugin = plugin.clone();
                let command = sub.command.clone();
                let trigger = format!("event:{event}");
                let weak = Arc::downgrade(hub);
                tokio::spawn(async move {
                    let (mut log, response) = host.run(&plugin, &command, &trigger, &input).await;
                    host.queued.fetch_sub(1, Ordering::SeqCst);
                    let hub = weak.upgrade();
                    if let Some(response) = response {
                        if let (Some(toast), Some(hub)) = (response.toast, &hub) {
                            hub.emit(HubEvent::PluginToast {
                                plugin: plugin.name.clone(),
                                message: toast.message,
                                tone: toast.tone,
                            });
                        }
                        if response.paste.is_some() {
                            // Only an action (someone asked) may type into
                            // a session.
                            log.ok = false;
                            log.error = Some(
                                "an event hook's response may not paste (actions only)".into(),
                            );
                        }
                    }
                    host.append_log(&log);
                    if let Some(hub) = hub {
                        hub.emit(HubEvent::PluginRan {
                            plugin: log.plugin.clone(),
                            trigger: log.trigger.clone(),
                            ok: log.ok,
                        });
                        hub.emit(HubEvent::Plugins);
                    }
                });
                started += 1;
            }
        }
        started
    }

    /// Run `name`'s action `id` (for `session`, when given or needed).
    /// The run is its own task: a caller that goes away (an HTTP client
    /// that hung up) does not cut it short -- it still keeps its
    /// deadline, kills its group, and is logged.
    pub async fn run_action(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        name: &str,
        id: &str,
        session: Option<String>,
    ) -> Result<ActionResult, ActionError> {
        let host = Arc::clone(self);
        let hub = Arc::clone(hub);
        let (name, id) = (name.to_owned(), id.to_owned());
        tokio::spawn(async move { host.run_action_here(&hub, &name, &id, session).await })
            .await
            .unwrap_or_else(|e| {
                Err(ActionError::Refused(format!(
                    "the action's run failed: {e}"
                )))
            })
    }

    async fn run_action_here(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        name: &str,
        id: &str,
        session: Option<String>,
    ) -> Result<ActionResult, ActionError> {
        let plugin = self
            .plugin(name)
            .ok_or_else(|| ActionError::NotFound(format!("no plugin {name:?}")))?;
        let manifest = plugin
            .manifest
            .as_ref()
            .map_err(|e| ActionError::Refused(format!("plugin {name} is invalid: {e}")))?;
        let action = manifest
            .actions
            .iter()
            .find(|a| a.id == id)
            .ok_or_else(|| ActionError::NotFound(format!("plugin {name} has no action {id:?}")))?
            .clone();
        if !self.master() {
            return Err(ActionError::Refused(
                "plugins are off (hub.toml plugins.enabled = false)".into(),
            ));
        }
        if !plugin.enabled {
            return Err(ActionError::Refused(format!(
                "plugin {name} is disabled (fw hub plugin enable {name}, or Settings)"
            )));
        }
        let session_payload = match &session {
            Some(id) => Some(
                hub.event_session(id)
                    .ok_or_else(|| ActionError::NotFound(format!("no session {id:?}")))?,
            ),
            None if action.context == Context::Session => {
                return Err(ActionError::Refused(format!(
                    "{} needs a session: attach one first",
                    action.title
                )));
            }
            None => None,
        };
        let input = json!({
            "version": 1,
            "type": "action",
            "plugin": plugin.name,
            "action": action.id,
            "at": chrono::Utc::now(),
            "hub": {"version": env!("CARGO_PKG_VERSION")},
            "payload": {"session": session_payload},
        });
        let trigger = format!("action:{}", action.id);
        let (mut log, response) = self.run(&plugin, &action.command, &trigger, &input).await;
        let mut pasted = None;
        let mut toast = None;
        if let Some(response) = response {
            toast = response.toast;
            if let Some(paste) = response.paste {
                let target = paste.session.clone().or(session.clone());
                match target {
                    Some(target) => {
                        let hub = Arc::clone(hub);
                        let text = paste.text.clone();
                        let run = paste.run;
                        let result = tokio::task::spawn_blocking(move || {
                            let resolved = hub.session(&target).map(|v| v.record.id)?;
                            hub.paste(&resolved, &text, run)
                        })
                        .await;
                        match result {
                            Ok(Ok(outcome)) => pasted = Some(outcome),
                            Ok(Err(e)) => {
                                log.ok = false;
                                log.error = Some(format!("paste: {e}"));
                            }
                            Err(e) => {
                                log.ok = false;
                                log.error = Some(format!("paste: {e}"));
                            }
                        }
                    }
                    None => {
                        log.ok = false;
                        log.error = Some("paste names no session (and the action had none)".into());
                    }
                }
            }
        }
        self.append_log(&log);
        hub.emit(HubEvent::PluginRan {
            plugin: log.plugin.clone(),
            trigger: log.trigger.clone(),
            ok: log.ok,
        });
        hub.emit(HubEvent::Plugins);
        Ok(ActionResult {
            ok: log.ok,
            toast,
            paste: pasted,
            log,
        })
    }

    /// Follow the hub's events and run subscriptions (on the current
    /// tokio runtime) until the hub goes away; announces `hub.started`
    /// first (on the bus, so the activity journal hears it too). Re-scans
    /// the plugins dir every [`SCAN_POLL`] (pushing `plugins` to the UI on
    /// a change). `status.changed` comes from the hub
    /// (`Hub::status_changed`), like every other lifecycle event.
    pub fn watch(self: &Arc<Self>, hub: &Arc<Hub>) {
        let mut events = hub.subscribe();
        let weak: Weak<Hub> = Arc::downgrade(hub);
        let host = Arc::clone(self);
        let sessions = hub.sessions(None).len();
        hub.emit_lifecycle(
            "hub.started",
            json!({"version": env!("CARGO_PKG_VERSION"), "sessions": sessions}),
        );
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                };
                let Some(hub) = weak.upgrade() else { break };
                if let HubEvent::Lifecycle { event, payload } = event {
                    host.dispatch(&hub, event, payload);
                }
            }
        });
        let host = Arc::clone(self);
        let weak = Arc::downgrade(hub);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SCAN_POLL).await;
                let Some(hub) = weak.upgrade() else { break };
                let host = Arc::clone(&host);
                let changed = tokio::task::spawn_blocking(move || host.rescan().1)
                    .await
                    .unwrap_or(false);
                if changed {
                    hub.emit(HubEvent::Plugins);
                }
            }
        });
    }
}

fn read_manifest(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{MANIFEST_FILE}: {e}"))?;
    if meta.len() > MAX_MANIFEST_BYTES {
        return Err(format!("{MANIFEST_FILE} is over 64 KiB"));
    }
    std::fs::read_to_string(path).map_err(|e| format!("{MANIFEST_FILE}: {e}"))
}

fn read_enabled(state_dir: &Path) -> BTreeSet<String> {
    let path = state_dir.join(ENABLED_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<EnabledFile>(&bytes) {
            Ok(file) => file.enabled,
            Err(e) => {
                eprintln!(
                    "fw hub: {} is invalid ({e}); every plugin is disabled",
                    path.display()
                );
                BTreeSet::new()
            }
        },
        Err(_) => BTreeSet::new(),
    }
}

/// Enable or disable plugin `name` in `<state-dir>/plugins.json` (the
/// plugin must exist on disk to be enabled).
pub fn set_enabled(state_dir: &Path, name: &str, on: bool) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("{name:?} is not a plugin name"));
    }
    let dir = state_dir.join(PLUGINS_DIR).join(name);
    if on && !dir.join(MANIFEST_FILE).is_file() {
        return Err(format!(
            "no plugin {name}: expected {}",
            dir.join(MANIFEST_FILE).display()
        ));
    }
    let mut enabled = read_enabled(state_dir);
    if on {
        enabled.insert(name.to_owned());
    } else {
        enabled.remove(name);
    }
    let bytes = serde_json::to_vec_pretty(&EnabledFile { enabled }).map_err(|e| e.to_string())?;
    crate::paths::ensure_private_dir(state_dir).map_err(|e| e.to_string())?;
    crate::auth::write_private(&state_dir.join(ENABLED_FILE), &bytes)
        .map_err(|e| format!("cannot write {ENABLED_FILE}: {e}"))
}

fn log_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(LOG_DIR).join(format!("{name}.jsonl"))
}

fn append_log(state_dir: &Path, log: &RunLog) -> std::io::Result<()> {
    use std::io::Write;
    crate::paths::ensure_private_dir(&state_dir.join(LOG_DIR))?;
    let path = log_path(state_dir, &log.plugin);
    let mut line = serde_json::to_vec(log).map_err(std::io::Error::other)?;
    line.push(b'\n');
    {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?.write_all(&line)?;
    }
    if std::fs::metadata(&path)?.len() > LOG_TRIM_BYTES {
        let entries = read_logs(state_dir, &log.plugin, LOG_KEEP);
        let mut out = Vec::new();
        for entry in entries {
            out.extend(serde_json::to_vec(&entry).map_err(std::io::Error::other)?);
            out.push(b'\n');
        }
        crate::auth::write_private(&path, &out)?;
    }
    Ok(())
}

/// The newest `limit` runs of plugin `name`, oldest first.
pub fn read_logs(state_dir: &Path, name: &str, limit: usize) -> Vec<RunLog> {
    let Ok(text) = std::fs::read_to_string(log_path(state_dir, name)) else {
        return Vec::new();
    };
    let entries: Vec<RunLog> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let skip = entries.len().saturating_sub(limit);
    entries.into_iter().skip(skip).collect()
}

/// Every plugin on disk, without a running hub (`fw hub plugin ls`).
pub fn scan(state_dir: &Path) -> Vec<Plugin> {
    let host = PluginHost::new(
        state_dir,
        HostOptions {
            base_env: Vec::new(),
            ..HostOptions::default()
        },
    );
    host.plugins().as_ref().clone()
}

/// A plugin as `GET /api/plugins` and `fw hub plugin ls --json` show it.
pub fn plugin_view(state_dir: &Path, plugin: &Plugin) -> Value {
    let last = read_logs(state_dir, &plugin.name, 1).pop();
    let last_error = read_logs(state_dir, &plugin.name, 20)
        .into_iter()
        .rev()
        .find(|l| !l.ok)
        .map(|l| json!({"at": l.at, "trigger": l.trigger, "error": l.error}));
    let (manifest, error) = match &plugin.manifest {
        Ok(m) => (Some(m), None),
        Err(e) => (None, Some(e.clone())),
    };
    json!({
        "name": plugin.name,
        "dir": plugin.dir.to_string_lossy(),
        "enabled": plugin.enabled,
        "valid": manifest.is_some(),
        "error": error,
        "description": manifest.and_then(|m| m.description.clone()),
        "version": manifest.and_then(|m| m.version.clone()),
        "timeout_secs": manifest.map(|m| m.timeout_secs),
        "events": manifest.map(|m| m.events.iter().map(|e| json!({"on": e.on, "statuses": e.statuses, "kinds": e.kinds})).collect::<Vec<_>>()),
        "actions": manifest.map(|m| m.actions.iter().map(|a| json!({"id": a.id, "title": a.title, "context": a.context})).collect::<Vec<_>>()),
        "last_run": last.map(|l| json!({"at": l.at, "trigger": l.trigger, "ok": l.ok, "error": l.error, "duration_ms": l.duration_ms})),
        "last_error": last_error,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn subscriptions_filter_by_status_and_kind() {
        let sub = |statuses: &[&str], kinds: &[&str]| super::Subscription {
            on: "status.changed".into(),
            statuses: statuses.iter().map(|s| (*s).to_owned()).collect(),
            kinds: kinds.iter().map(|s| (*s).to_owned()).collect(),
            command: Vec::new(),
        };
        let event = |status: &str, kind: &str, turn_finished: bool| serde_json::json!({"status": status, "session": {"kind": kind}, "turn_finished": turn_finished});
        assert!(sub(&[], &[]).takes(&event("idle", "shell", false)));
        assert!(sub(&["blocked"], &[]).takes(&event("blocked", "claude", false)));
        assert!(!sub(&["blocked"], &[]).takes(&event("working", "claude", false)));
        assert!(!sub(&[], &["codex"]).takes(&event("done", "claude", true)));
        assert!(sub(&["done"], &["claude"]).takes(&event("done", "claude", true)));
        // A watched turn's idle (turn_finished) follows its done: "done"
        // takes the done only, "idle" the idle.
        assert!(!sub(&["done"], &[]).takes(&event("idle", "claude", true)));
        assert!(sub(&["idle"], &[]).takes(&event("idle", "claude", true)));
    }

    use super::*;

    #[test]
    fn a_full_manifest_parses() {
        let m = parse_manifest(
            "ping",
            r#"
name = "ping"
description = "d"
command = ["./hook.sh", "--x"]
timeout_secs = 3
[[events]]
on = "status.changed"
statuses = ["blocked", "done"]
kinds = ["claude"]
[[events]]
on = "session.created"
command = ["/bin/true"]
[[actions]]
id = "say-hi"
title = "Say hi"
context = "session"
"#,
        )
        .unwrap();
        assert_eq!(m.timeout_secs, 3);
        assert_eq!(m.events[0].command, vec!["./hook.sh", "--x"]);
        assert_eq!(m.events[1].command, vec!["/bin/true"]);
        assert_eq!(m.actions[0].context, Context::Session);
        assert_eq!(m.actions[0].command, vec!["./hook.sh", "--x"]);
    }

    #[test]
    fn bad_manifests_say_why() {
        for (text, needle) in [
            (
                "command=[\"x\"]\n[[events]]\non=\"nope\"\n",
                "unknown event",
            ),
            ("[[events]]\non=\"session.created\"\n", "no command"),
            (
                "command=[]\n[[events]]\non=\"session.created\"\n",
                "must name a program",
            ),
            (
                "command=[\"x\"]\n[[events]]\non=\"session.created\"\nstatuses=[\"done\"]\n",
                "status.changed only",
            ),
            (
                "command=[\"x\"]\n[[events]]\non=\"status.changed\"\nstatuses=[\"bored\"]\n",
                "bored",
            ),
            (
                "command=[\"x\"]\n[[actions]]\nid=\"A B\"\ntitle=\"t\"\n",
                "id",
            ),
            (
                "command=[\"x\"]\n[[actions]]\nid=\"a\"\ntitle=\"t\"\n[[actions]]\nid=\"a\"\ntitle=\"u\"\n",
                "twice",
            ),
            ("command=[\"x\"]\n", "no [[events]]"),
            (
                "command=[\"x\"]\ntimeout_secs=0\n[[events]]\non=\"hub.started\"\n",
                "timeout_secs",
            ),
            (
                "name=\"other\"\ncommand=[\"x\"]\n[[events]]\non=\"hub.started\"\n",
                "directory",
            ),
            (
                "command=[\"x\"]\nshell=true\n[[events]]\non=\"hub.started\"\n",
                "shell",
            ),
        ] {
            let e = parse_manifest("p", text).unwrap_err();
            assert!(e.contains(needle), "{text:?}: {e}");
        }
    }

    #[test]
    fn responses_parse_strictly() {
        assert_eq!(parse_response("just a log line\n").unwrap(), None);
        assert_eq!(parse_response("").unwrap(), None);
        let r = parse_response(r#"{"toast":"hi\u0007 there"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(r.toast.unwrap().message, "hi there");
        let r = parse_response(
            r#"{"toast":{"message":"m","tone":"warn"},"paste":{"text":"ls","run":true}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.toast.unwrap().tone, "warn");
        assert!(r.paste.unwrap().run);
        assert!(parse_response("{nope").is_err());
        assert!(parse_response(r#"{"exec":"rm -rf"}"#).is_err());
        assert!(parse_response(r#"{"toast":{"message":"m","tone":"loud"}}"#).is_err());
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_name("slack-ping_2"));
        assert!(!valid_name("Slack"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("../x"));
        assert!(!valid_name(""));
    }

    #[test]
    fn enabling_needs_the_plugin_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(set_enabled(dir.path(), "p", true).is_err());
        let plugin = dir.path().join(PLUGINS_DIR).join("p");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(MANIFEST_FILE),
            "command=[\"./x\"]\n[[events]]\non=\"hub.started\"\n",
        )
        .unwrap();
        assert!(!scan(dir.path())[0].enabled);
        set_enabled(dir.path(), "p", true).unwrap();
        assert!(scan(dir.path())[0].enabled);
        set_enabled(dir.path(), "p", false).unwrap();
        assert!(!scan(dir.path())[0].enabled);
    }
}
