//! `fw hub doctor`: check what a hub on this machine depends on and say
//! how to fix what is off. Read-only: it creates, removes, and rewrites
//! nothing (a port is bound for a moment to see that it is free, and
//! programs are asked for `--version` / `--help`; scripts found on `PATH`
//! are read, never run).

use crate::agent::{self, SessionKind};
use crate::cli::{DEFAULT_PORT, StateDirArg, resolve_state_dir};
use crate::review::runner::{self, Spec};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long one probed program (`claude --version`, `systemctl`, ...) may
/// take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Free space below this on the state dir's filesystem fails (logs,
/// transcripts' commands, recordings would stop being written).
pub const DISK_FAIL_BYTES: u64 = 512 * 1024 * 1024;
/// Free space below this warns.
pub const DISK_WARN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Ports tried after a taken one, to suggest a free one.
const PORT_SUGGESTIONS: u16 = 20;

#[derive(Debug, clap::Args)]
pub(crate) struct DoctorArgs {
    /// Print JSON: {"version","binary","state_dir","checks":[{"id",
    /// "level":"ok|info|warn|fail","summary","details":[..],"fix"}],
    /// "counts":{..}}.
    #[arg(long)]
    json: bool,
    /// The port to check [default: hub.toml server.port, else 7420].
    #[arg(long)]
    port: Option<u16>,
    #[command(flatten)]
    state: StateDirArg,
}

/// How a check came out (ordered: worst last).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    /// Worth knowing; nothing to fix (an optional tool missing).
    Info,
    /// Works, but something is off or will bite later.
    Warn,
    /// Broken: the hub (or that part of it) will not work.
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

/// One finding.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub level: Level,
    pub summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    /// What to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Check {
    fn new(id: &'static str, level: Level, summary: impl Into<String>) -> Self {
        Self {
            id,
            level,
            summary: summary.into(),
            details: Vec::new(),
            fix: None,
        }
    }

    fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.details.push(detail.into());
        self
    }
}

pub(crate) fn doctor(args: &DoctorArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let dir = match resolve_state_dir(&args.state) {
        Ok(dir) => dir,
        Err(message) => return crate::util::fail(err, &message),
    };
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.canonicalize().unwrap_or(p));
    let path = std::env::var_os("PATH");
    let checks = run_checks(&dir, exe.as_deref(), path.as_deref(), args.port);
    let count = |level| checks.iter().filter(|c| c.level == level).count();
    if args.json {
        let body = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "binary": exe,
            "state_dir": dir,
            "checks": checks,
            "counts": {
                "ok": count(Level::Ok),
                "info": count(Level::Info),
                "warn": count(Level::Warn),
                "fail": count(Level::Fail),
            },
        });
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
    } else {
        let _ = writeln!(
            out,
            "flywheel hub doctor: flywheel {} ({}), state dir {}\n",
            env!("CARGO_PKG_VERSION"),
            exe.as_deref()
                .map_or_else(|| "this program".into(), |p| p.display().to_string()),
            dir.display()
        );
        let _ = out.write_all(render(&checks).as_bytes());
    }
    i32::from(checks.iter().any(|c| c.level == Level::Fail))
}

/// The text report.
pub fn render(checks: &[Check]) -> String {
    let width = checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
    let mut text = String::new();
    for check in checks {
        text.push_str(&format!(
            "  {:<4}  {:<width$}  {}\n",
            check.level.label(),
            check.id,
            check.summary
        ));
        let pad = " ".repeat(width + 10);
        for detail in &check.details {
            text.push_str(&format!("{pad}{detail}\n"));
        }
        if let Some(fix) = &check.fix {
            text.push_str(&format!("{pad}fix: {fix}\n"));
        }
    }
    let count = |level| checks.iter().filter(|c| c.level == level).count();
    text.push_str(&format!(
        "\n{} ok, {} info, {} warn, {} fail\n",
        count(Level::Ok),
        count(Level::Info),
        count(Level::Warn),
        count(Level::Fail)
    ));
    text
}

/// Every check, in report order.
fn run_checks(
    dir: &Path,
    exe: Option<&Path>,
    path: Option<&OsStr>,
    port: Option<u16>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    for name in ["fw", "flywheel"] {
        checks.push(on_path_check(name, &executables_named(name, path), exe));
    }
    let hub = running_hub(dir);
    checks.push(hub_check(&hub, exe));
    let hub_path = hub.as_ref().and_then(|h| h.path.clone());
    let claude = agent_check(SessionKind::Claude, path, hub_path.as_deref());
    let claude_program = agent::resolve_program("claude", path);
    checks.push(claude);
    checks.push(hooks_check(claude_program.as_deref(), exe));
    checks.push(agent_check(SessionKind::Codex, path, hub_path.as_deref()));
    checks.extend(state_dir_checks(dir));
    checks.extend(config_checks(dir));
    // Where the next `fw hub start` would listen: --port, else hub.toml,
    // else the defaults (a bad file: the defaults).
    let flags = crate::config::Flags {
        port,
        ..crate::config::Flags::default()
    };
    let effective = crate::config::read_file(&dir.join(crate::config::CONFIG_FILE))
        .ok()
        .flatten()
        .and_then(|file| crate::config::resolve(&file, &flags, None).ok());
    let bind = effective
        .as_ref()
        .map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |e| e.bind);
    let port = effective
        .as_ref()
        .map_or(port.unwrap_or(DEFAULT_PORT), |e| e.port);
    checks.push(port_check(bind, port, hub.as_ref()));
    checks.push(tailscale_check(hub.as_ref().map_or(port, |h| h.port)));
    checks.push(browser_check());
    checks.extend(systemd_checks());
    checks.push(clock_check());
    checks.push(disk_check(dir, DISK_WARN_BYTES, DISK_FAIL_BYTES));
    checks
}

// --- PATH ------------------------------------------------------------------------

/// Every executable called `name` on `path`, in `PATH` order (the first
/// is what a shell runs).
pub fn executables_named(name: &str, path: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(path) = path else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(path).filter(|d| !d.as_os_str().is_empty()) {
        let candidate = dir.join(name);
        if let Some(hit) = agent::resolve_program(&candidate.to_string_lossy(), None)
            && !found.contains(&hit)
        {
            found.push(hit);
        }
    }
    found
}

/// What a program found on `PATH` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// This very program (same file, or a byte-identical copy: `cargo
    /// install` writes `fw` and `flywheel` as two copies).
    This,
    /// A script: its `#!` interpreter line.
    Script(String),
    /// A flywheel with `fw hub`, but another build.
    OtherFlywheel,
    /// Some other program.
    Other,
}

/// Classify `candidate` against this program `exe`. Scripts are read, not
/// run; a binary that differs is asked for `hub --help`.
pub fn classify(candidate: &Path, exe: Option<&Path>) -> Found {
    let real = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    if let Some(exe) = exe {
        if real == exe {
            return Found::This;
        }
        if same_contents(&real, exe) {
            return Found::This;
        }
    }
    let mut head = [0u8; 256];
    let read = std::fs::File::open(&real)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    if head[..read].starts_with(b"#!") {
        let line = String::from_utf8_lossy(&head[2..read]);
        let interpreter = line.lines().next().unwrap_or_default().trim().to_owned();
        return Found::Script(interpreter);
    }
    let help = bounded(&real, &["hub", "--help"]);
    match help {
        Some(out) if out.success() && out.stdout_text().contains("The flywheel hub") => {
            Found::OtherFlywheel
        }
        _ => Found::Other,
    }
}

fn same_contents(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    match (digest(a), digest(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn digest(path: &Path) -> Option<[u8; 32]> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(hasher.finalize().into())
}

/// The check of what `name` runs in a shell: `found` (in `PATH` order).
pub fn on_path_check(name: &'static str, found: &[PathBuf], exe: Option<&Path>) -> Check {
    on_path_check_with(name, found, exe, &|p| classify(p, exe))
}

fn on_path_check_with(
    name: &'static str,
    found: &[PathBuf],
    exe: Option<&Path>,
    classify: &dyn Fn(&Path) -> Found,
) -> Check {
    let id = if name == "fw" {
        "fw on PATH"
    } else {
        "flywheel on PATH"
    };
    let this = exe.map_or_else(|| "this program".to_owned(), |e| e.display().to_string());
    let exe_dir = exe
        .and_then(Path::parent)
        .map_or_else(|| "its directory".to_owned(), |d| d.display().to_string());
    let Some(first) = found.first() else {
        // `fw` is what docs and hooks' users type; `flywheel` is the
        // long name.
        let level = if name == "fw" {
            Level::Warn
        } else {
            Level::Info
        };
        return Check::new(id, level, format!("no `{name}` on PATH (this is {this})"))
            .fix(format!("add {exe_dir} to PATH"));
    };
    let kind = classify(first);
    let mut check = match &kind {
        Found::This => Check::new(id, Level::Ok, format!("{} is this binary", first.display())),
        Found::OtherFlywheel => Check::new(
            id,
            Level::Warn,
            format!(
                "{} is another flywheel build than this one ({this})",
                first.display()
            ),
        )
        .fix(format!(
            "reinstall one build (`cargo install --path crates/flywheel`), or put {exe_dir} \
             first on PATH"
        )),
        Found::Script(_) | Found::Other => {
            let what = match &kind {
                Found::Script(interpreter) => format!("a script run by {interpreter}"),
                _ => "another program".to_owned(),
            };
            // The flywheel it shadows, when one is further down PATH: its
            // directory is the one to move up.
            let shadowed = found[1..]
                .iter()
                .find(|p| matches!(classify(p), Found::This | Found::OtherFlywheel))
                .and_then(|p| p.parent())
                .map_or_else(|| exe_dir.clone(), |d| d.display().to_string());
            Check::new(
                id,
                Level::Fail,
                format!(
                    "`{name}` runs {} ({what}), not this flywheel: `{name} hub ...` in a shell \
                     goes to it",
                    first.display()
                ),
            )
            .detail(format!("this flywheel: {this}"))
            .fix(format!(
                "remove or rename {} (a Python venv's entry point: deactivate the venv or \
                 uninstall the package that installs it), or put {shadowed} before {} on PATH; \
                 then `hash -r`",
                first.display(),
                first
                    .parent()
                    .map_or_else(String::new, |d| d.display().to_string())
            ))
        }
    };
    for later in &found[1..] {
        check = check.detail(format!("also on PATH (shadowed): {}", later.display()));
    }
    check
}

// --- the running hub -----------------------------------------------------------------

/// The hub serving the state dir, read without touching anything.
#[derive(Debug, Clone)]
pub struct Running {
    pub pid: u32,
    pub listen: SocketAddr,
    pub port: u16,
    pub version: Option<String>,
    pub binary: Option<PathBuf>,
    /// Its environment's `PATH` (what its sessions resolve agents with).
    pub path: Option<OsString>,
}

/// The hub serving `dir`: its runtime file, believed only while its serve
/// lock is held (tested on the existing file, never created or removed).
fn running_hub(dir: &Path) -> Option<Running> {
    let info = crate::remote::RuntimeInfo::read(dir).ok().flatten()?;
    let lock = std::fs::File::open(dir.join(crate::remote::LOCK_FILE)).ok()?;
    match lock.try_lock_shared() {
        // Nobody holds it: a stale file from a hub that died.
        Ok(()) => {
            let _ = lock.unlock();
            return None;
        }
        Err(std::fs::TryLockError::WouldBlock) => {}
        Err(std::fs::TryLockError::Error(_)) => return None,
    }
    let description = crate::handoff::describe_within(dir, Duration::from_secs(2)).ok();
    Some(Running {
        pid: info.pid,
        listen: info.listen,
        port: info.listen.port(),
        version: description.as_ref().map(|d| d.version.clone()),
        binary: description.and_then(|d| d.binary),
        path: process_path(info.pid),
    })
}

/// `PATH` from `/proc/<pid>/environ` (Linux; same user).
fn process_path(pid: u32) -> Option<OsString> {
    use std::os::unix::ffi::OsStrExt;
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ
        .split(|b| *b == 0)
        .find_map(|entry| entry.strip_prefix(b"PATH="))
        .map(|value| OsStr::from_bytes(value).to_os_string())
}

fn hub_check(hub: &Option<Running>, exe: Option<&Path>) -> Check {
    let id = "hub";
    let Some(hub) = hub else {
        return Check::new(id, Level::Info, "not running for this state dir")
            .fix("`fw hub start` (detached) or `fw hub serve` (foreground)");
    };
    let version = hub
        .version
        .as_deref()
        .map_or_else(String::new, |v| format!(" flywheel {v}"));
    let mut check = Check::new(
        id,
        Level::Ok,
        format!("running (pid {}{version}) on {}", hub.pid, hub.listen),
    );
    if let Some(binary) = &hub.binary {
        check = check.detail(format!("binary: {}", binary.display()));
    }
    let skew = hub
        .version
        .as_deref()
        .is_some_and(|v| v != env!("CARGO_PKG_VERSION"));
    if skew {
        check.level = Level::Warn;
        check.summary.push_str(&format!(
            "; this CLI is flywheel {}",
            env!("CARGO_PKG_VERSION")
        ));
        check = check.fix(match exe {
            Some(exe) => format!(
                "`fw hub upgrade --binary {}` (live handoff: no session restarts)",
                exe.display()
            ),
            None => "`fw hub upgrade` (live handoff: no session restarts)".to_owned(),
        });
    }
    check
}

// --- agents --------------------------------------------------------------------

/// Run `program args` (bounded), from `/`.
fn bounded(program: &Path, args: &[&str]) -> Option<runner::Output> {
    let program = program.to_string_lossy();
    runner::run(&Spec {
        program: &program,
        dir: Path::new("/"),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        stdin: None,
        timeout: PROBE_TIMEOUT,
        limit: 64 * 1024,
    })
    .ok()
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn agent_check(kind: SessionKind, path: Option<&OsStr>, hub_path: Option<&OsStr>) -> Check {
    let name = kind.as_str();
    let (id, install) = match kind {
        SessionKind::Codex => ("codex", "npm install -g @openai/codex"),
        _ => (
            "claude",
            "install Claude Code (https://docs.claude.com/en/docs/claude-code)",
        ),
    };
    let Some(program) = agent::resolve_program(name, path) else {
        // Codex is optional; Claude Code is the main agent.
        let level = if kind == SessionKind::Codex {
            Level::Info
        } else {
            Level::Warn
        };
        return Check::new(
            id,
            level,
            format!("`{name}` not found on PATH: {name} sessions cannot start"),
        )
        .fix(format!("{install}, or ignore it if you do not run {name}"));
    };
    let mut check = match bounded(&program, &["--version"]) {
        Some(out) if out.success() => Check::new(
            id,
            Level::Ok,
            format!("{} ({})", program.display(), first_line(&out.stdout_text())),
        ),
        Some(out) => Check::new(
            id,
            Level::Fail,
            format!(
                "{} --version failed: {}",
                program.display(),
                first_line(&out.error_text())
            ),
        )
        .fix(format!("reinstall it ({install})")),
        None => Check::new(
            id,
            Level::Fail,
            format!("{} could not be run", program.display()),
        )
        .fix(format!("reinstall it ({install})")),
    };
    // The running hub resolves agents with its own environment.
    if let Some(hub_path) = hub_path {
        match agent::resolve_program(name, Some(hub_path)) {
            None => {
                check.level = check.level.max(Level::Warn);
                check = check
                    .detail(format!("the running hub's PATH has no `{name}`"))
                    .fix(
                        "restart the hub from a shell where it is on PATH (`fw hub stop`, then \
                         `fw hub start`; sessions are restored)",
                    );
            }
            Some(theirs) if theirs != program => {
                check = check.detail(format!("the running hub runs {}", theirs.display()));
            }
            Some(_) => {}
        }
    }
    check
}

/// Whether Claude Code takes the hub's `--settings` (status hooks), and
/// whether the hooks' command answers.
fn hooks_check(claude: Option<&Path>, exe: Option<&Path>) -> Check {
    let id = "claude hooks";
    let Some(exe) = exe else {
        return Check::new(id, Level::Warn, "cannot locate this program for the hooks");
    };
    let report = vec![exe.to_string_lossy().into_owned(), "hub".to_owned()];
    let reporter = bounded(exe, &["hub", "report", "--help"]);
    if !reporter.as_ref().is_some_and(runner::Output::success) {
        return Check::new(
            id,
            Level::Fail,
            format!(
                "the hooks' command `{} hub report` does not run",
                exe.display()
            ),
        )
        .fix("reinstall flywheel (`cargo install --path crates/flywheel`)");
    }
    let Some(claude) = claude else {
        return Check::new(id, Level::Info, "skipped (no claude)");
    };
    let settings = agent::claude_settings(&report).to_string();
    match bounded(claude, &["--settings", &settings, "--version"]) {
        Some(out) if out.success() => Check::new(
            id,
            Level::Ok,
            "claude accepts the hub's --settings; hooks run `fw hub report`",
        )
        .detail(format!(
            "{} hook events -> {} hub report",
            agent::claude_settings(&report)["hooks"]
                .as_object()
                .map_or(0, serde_json::Map::len),
            exe.display()
        )),
        Some(out) => Check::new(
            id,
            Level::Fail,
            format!(
                "claude refused the hub's --settings: {}",
                first_line(&out.error_text())
            ),
        )
        .fix("update Claude Code (`claude update`); status would fall back to screen reading"),
        None => Check::new(id, Level::Warn, "claude --settings did not answer in time"),
    }
}

// --- state dir -------------------------------------------------------------------

/// The state dir and its secrets: owned by this user, private.
pub fn state_dir_checks(dir: &Path) -> Vec<Check> {
    use std::os::unix::fs::MetadataExt;
    let id = "state dir";
    let meta = match std::fs::metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return vec![Check::new(
                id,
                Level::Info,
                format!(
                    "{} does not exist yet (the first serve creates it, mode 700)",
                    dir.display()
                ),
            )];
        }
        Err(e) => {
            return vec![
                Check::new(id, Level::Fail, format!("{}: {e}", dir.display()))
                    .fix("fix its permissions, or pass another --state-dir"),
            ];
        }
    };
    let uid = unsafe_getuid();
    let mut checks = Vec::new();
    let mode = meta.mode() & 0o777;
    if !meta.is_dir() {
        checks.push(
            Check::new(
                id,
                Level::Fail,
                format!("{} is not a directory", dir.display()),
            )
            .fix("move it away, or pass another --state-dir"),
        );
        return checks;
    }
    if meta.uid() != uid {
        checks.push(
            Check::new(
                id,
                Level::Fail,
                format!(
                    "{} is owned by uid {}, not you ({uid})",
                    dir.display(),
                    meta.uid()
                ),
            )
            .fix(format!("sudo chown -R $(id -u):$(id -g) {}", dir.display())),
        );
    } else if !writable(dir) {
        checks.push(
            Check::new(
                id,
                Level::Fail,
                format!("{} is not writable", dir.display()),
            )
            .fix(format!("chmod u+rwx {}", dir.display())),
        );
    } else if mode & 0o077 != 0 {
        checks.push(
            Check::new(
                id,
                Level::Warn,
                format!(
                    "{} is mode {mode:o}: other users can look inside (the hub sets 700 when \
                     it opens it)",
                    dir.display()
                ),
            )
            .fix(format!("chmod 700 {}", dir.display())),
        );
    } else {
        checks.push(Check::new(
            id,
            Level::Ok,
            format!("{} (mode {mode:o}, yours)", dir.display()),
        ));
    }
    for secret in [
        crate::auth::TOKEN_FILE,
        crate::auth::LOGINS_FILE,
        crate::notify::CONFIG_FILE,
        "machines.json",
    ] {
        let path = dir.join(secret);
        if let Ok(meta) = std::fs::metadata(&path) {
            let mode = meta.mode() & 0o777;
            if mode & 0o077 != 0 {
                checks.push(
                    Check::new(
                        "secrets",
                        Level::Fail,
                        format!(
                            "{} is mode {mode:o}: readable by other users",
                            path.display()
                        ),
                    )
                    .fix(format!(
                        "chmod 600 {}{}",
                        path.display(),
                        if secret == crate::auth::TOKEN_FILE {
                            "; then `fw hub token --rotate` (it may have been read)"
                        } else {
                            ""
                        }
                    )),
                );
            }
        }
    }
    checks
}

fn unsafe_getuid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string for the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

// --- config files -------------------------------------------------------------------

/// `hub.toml`, `notify.toml`, `settings.json`, and plugin manifests parse.
pub fn config_checks(dir: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    let hub_toml = dir.join(crate::config::CONFIG_FILE);
    checks.push(match crate::config::read_file(&hub_toml) {
        Ok(None) => Check::new("hub.toml", Level::Ok, "none (every setting is its default)"),
        Ok(Some(file)) => {
            match crate::config::resolve(&file, &crate::config::Flags::default(), None) {
                Ok(_) => Check::new("hub.toml", Level::Ok, format!("{} is valid", hub_toml.display())),
                Err(errors) => errors.into_iter().fold(
                    Check::new("hub.toml", Level::Fail, format!("{} has bad values", hub_toml.display()))
                        .fix("fix them (`fw hub config check` re-checks); the hub refuses to start until then"),
                    Check::detail,
                ),
            }
        }
        Err(message) => Check::new("hub.toml", Level::Fail, message)
            .fix("fix it (`fw hub config check` re-checks); the hub refuses to start until then"),
    });
    checks.push(match crate::notify::NotifyConfig::load(dir, None) {
        Ok(None) => Check::new("notify.toml", Level::Ok, "none (no push notifications)"),
        Ok(Some(config)) => Check::new(
            "notify.toml",
            Level::Ok,
            format!("pushes to {}", config.host()),
        ),
        Err(message) => Check::new("notify.toml", Level::Fail, message)
            .fix("fix it; until then the hub starts without push notifications"),
    });
    match crate::settings::read(&dir.join(crate::settings::SETTINGS_FILE)) {
        Ok(_) => {}
        Err(message) => checks.push(
            Check::new("settings.json", Level::Fail, message)
                .fix("fix or remove it (the UI's settings are not saved over it until then)"),
        ),
    }
    let broken: Vec<String> = crate::plugins::scan(dir)
        .into_iter()
        .filter_map(|p| p.manifest.err().map(|e| format!("{}: {e}", p.name)))
        .collect();
    if !broken.is_empty() {
        checks.push(
            broken.into_iter().fold(
                Check::new("plugins", Level::Warn, "some plugin manifests do not load")
                    .fix("fix their plugin.toml (`fw hub plugin ls` lists them)"),
                Check::detail,
            ),
        );
    }
    checks
}

// --- network -------------------------------------------------------------------------

/// Whether the hub can listen on `bind:port`.
pub fn port_check(bind: IpAddr, port: u16, hub: Option<&Running>) -> Check {
    let id = "port";
    if let Some(hub) = hub {
        let mut check = Check::new(
            id,
            Level::Ok,
            format!("{} is this state dir's hub (pid {})", hub.listen, hub.pid),
        );
        if hub.port != port {
            check = check.detail(format!(
                "configured port {port}: the next start listens there"
            ));
        }
        return check;
    }
    match TcpListener::bind(SocketAddr::new(bind, port)) {
        Ok(listener) => {
            drop(listener);
            Check::new(id, Level::Ok, format!("{bind}:{port} is free"))
        }
        Err(e) => {
            let free = (1..=PORT_SUGGESTIONS)
                .filter_map(|n| port.checked_add(n))
                .find(|p| TcpListener::bind(SocketAddr::new(bind, *p)).is_ok());
            let fix = match free {
                Some(p) => format!(
                    "stop what listens there, or use another port: `fw hub start --port {p}` \
                     (or [server] port = {p} in hub.toml)"
                ),
                None => "stop what listens there, or pick another port with --port".to_owned(),
            };
            Check::new(
                id,
                Level::Fail,
                format!("{bind}:{port} is not available: {e}"),
            )
            .fix(fix)
        }
    }
}

fn tailscale_check(port: u16) -> Check {
    let id = "tailscale";
    let Some(program) = crate::remote::tailscale_program() else {
        return Check::new(
            id,
            Level::Info,
            "not installed: the hub is reachable from this machine only (or with --bind)",
        )
        .fix("for a phone or another machine: install Tailscale, then `tailscale serve`");
    };
    match crate::remote::detect_tailscale(&program, port) {
        None => Check::new(
            id,
            Level::Warn,
            format!("{} is installed but not connected", program.display()),
        )
        .fix("`tailscale up`"),
        Some(ts) if !ts.serve_urls.is_empty() => Check::new(
            id,
            Level::Ok,
            format!(
                "tailscale serve: {} -> port {port}",
                ts.serve_urls.join(", ")
            ),
        ),
        Some(ts) => Check::new(
            id,
            Level::Info,
            format!(
                "connected{}; not serving port {port}",
                ts.dns_name
                    .map_or_else(String::new, |name| format!(" as {name}"))
            ),
        )
        .fix(format!(
            "for remote access: `tailscale serve --bg http://127.0.0.1:{port}`"
        )),
    }
}

// --- this machine ----------------------------------------------------------------------

fn browser_check() -> Check {
    let id = "browser";
    let host = crate::browser::Host::current();
    let page = Path::new("/tmp/fw-hub-doctor.html");
    let openers = crate::browser::openers(page, Some(r"\\wsl.localhost\doctor.html"), &host);
    match openers.first() {
        Some(argv) => Check::new(
            id,
            Level::Ok,
            format!("`fw hub open` opens with {}", argv[0]),
        ),
        None if host.wsl() => Check::new(
            id,
            Level::Warn,
            "no way to open a Windows browser from WSL (`fw hub open` prints the URL)",
        )
        .fix("sudo apt install wslu (for wslview), or set BROWSER"),
        None => Check::new(
            id,
            Level::Warn,
            "no browser opener (`fw hub open` prints the URL)",
        )
        .fix("install xdg-utils, or set BROWSER"),
    }
}

/// `systemctl --user is-system-running`'s answer, read.
pub fn systemd_state(output: Option<&runner::Output>) -> Result<String, String> {
    let Some(output) = output else {
        return Err("systemctl did not answer".to_owned());
    };
    let state = first_line(&output.stdout_text());
    match state.as_str() {
        // Non-zero exits for degraded/starting still mean a manager.
        "running" | "degraded" | "starting" | "initializing" | "maintenance" => Ok(state),
        _ => Err(if state.is_empty() {
            first_line(&output.error_text())
        } else {
            state
        }),
    }
}

fn systemd_checks() -> Vec<Check> {
    let id = "systemd --user";
    let wsl = crate::browser::Host::current().wsl();
    let Some(systemctl) = agent::which("systemctl") else {
        return vec![Check::new(
            id,
            Level::Info,
            "no systemctl: `fw hub install-service` is not for this machine",
        )];
    };
    let answer = bounded(&systemctl, &["--user", "is-system-running"]);
    let mut checks = Vec::new();
    match systemd_state(answer.as_ref()) {
        Ok(state) => {
            let level = if state == "running" {
                Level::Ok
            } else {
                Level::Info
            };
            checks.push(Check::new(
                id,
                level,
                format!("user manager {state}: `fw hub install-service` works here"),
            ));
            if let Some(user) = std::env::var_os("USER").filter(|u| !u.is_empty())
                && let Some(loginctl) = agent::which("loginctl")
                && let Some(out) = bounded(
                    &loginctl,
                    &[
                        "show-user",
                        &user.to_string_lossy(),
                        "--property=Linger",
                        "--value",
                    ],
                )
                && out.success()
                && first_line(&out.stdout_text()) == "no"
            {
                checks.push(
                    Check::new(
                        "linger",
                        Level::Info,
                        "off: a service hub stops when your last login ends",
                    )
                    .fix("`loginctl enable-linger` (keeps it running)"),
                );
            }
        }
        Err(why) => checks.push(
            Check::new(
                id,
                Level::Warn,
                format!("no user manager ({why}): `fw hub install-service` units will not run"),
            )
            .fix(if wsl {
                "enable systemd in WSL: [boot] systemd=true in /etc/wsl.conf, then `wsl --shutdown` \
                 from Windows; or run `fw hub start`"
            } else {
                "log in through a session with a user manager, or run `fw hub start`"
            }),
        ),
    }
    checks
}

/// `timedatectl show -p NTPSynchronized --value`'s answer.
pub fn ntp_synchronized(output: &str) -> Option<bool> {
    match first_line(output).as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn clock_check() -> Check {
    let id = "clock";
    let now = chrono::Utc::now();
    // Login codes, cookies, and transcripts' times assume a sane clock.
    if now.timestamp() < 1_700_000_000 {
        return Check::new(
            id,
            Level::Fail,
            format!("the system clock reads {now}: logins and codes will not validate"),
        )
        .fix("set the clock (`sudo hwclock -s` on WSL)");
    }
    let synced = agent::which("timedatectl")
        .and_then(|t| bounded(&t, &["show", "--property=NTPSynchronized", "--value"]))
        .and_then(|out| ntp_synchronized(&out.stdout_text()));
    let wsl = crate::browser::Host::current().wsl();
    match synced {
        Some(true) => Check::new(
            id,
            Level::Ok,
            format!("NTP-synchronized ({})", now.format("%Y-%m-%d %H:%M:%S UTC")),
        ),
        Some(false) => Check::new(
            id,
            Level::Warn,
            format!(
                "not NTP-synchronized (reads {}): one-time codes and login expiry follow it",
                now.format("%Y-%m-%d %H:%M:%S UTC")
            ),
        )
        .fix(if wsl {
            "WSL drifts after the host sleeps: `sudo hwclock -s`, or `wsl --shutdown` from Windows"
        } else {
            "`sudo timedatectl set-ntp true`"
        }),
        None => Check::new(
            id,
            Level::Info,
            format!(
                "sync state unknown (no timedatectl answer); reads {}",
                now.format("%Y-%m-%d %H:%M:%S UTC")
            ),
        ),
    }
}

/// Free space on the state dir's filesystem (the nearest existing
/// ancestor before it is created), and what its logs take.
pub fn disk_check(dir: &Path, warn_below: u64, fail_below: u64) -> Check {
    let id = "disk";
    let mut probe = dir.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            break;
        }
    }
    let Some(free) = free_bytes(&probe) else {
        return Check::new(
            id,
            Level::Info,
            format!("cannot read free space at {}", probe.display()),
        );
    };
    let logs: u64 = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("hub.log"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    let summary = format!(
        "{} free at {} (logs {})",
        human_bytes(free),
        probe.display(),
        human_bytes(logs)
    );
    if free < fail_below {
        Check::new(id, Level::Fail, summary)
            .fix("free some space: logs, transcripts, and recordings stop being written")
    } else if free < warn_below {
        Check::new(id, Level::Warn, summary).fix("free some space soon")
    } else {
        Check::new(id, Level::Ok, summary)
    }
}

fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero statvfs is a valid out-parameter.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `stat` is writable for the call.
    if unsafe { libc::statvfs(c.as_ptr(), &raw mut stat) } != 0 {
        return None;
    }
    // `u64` here; narrower on other platforms (macOS: 32-bit block counts).
    #[allow(clippy::useless_conversion)]
    let free = u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize));
    Some(free)
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_script;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_script_first_on_path_shadowing_this_binary_fails_with_a_fix() {
        let dir = tempfile::tempdir().unwrap();
        let venv = dir.path().join("venv/bin");
        let cargo = dir.path().join("cargo/bin");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::create_dir_all(&cargo).unwrap();
        write_script(
            &venv.join("fw"),
            "#!/usr/bin/python3\nfrom flywheel._cli import main\n",
        );
        // "This binary": an executable copy in cargo/bin.
        write_script(&cargo.join("fw"), "#!/bin/sh\nexit 0\n");
        let exe = cargo.join("fw").canonicalize().unwrap();
        let path = std::env::join_paths([&venv, &cargo]).unwrap();
        let found = executables_named("fw", Some(&path));
        assert_eq!(found, vec![venv.join("fw"), cargo.join("fw")]);
        let check = on_path_check("fw", &found, Some(&exe));
        assert_eq!(check.level, Level::Fail, "{check:?}");
        assert!(
            check.summary.contains("a script run by /usr/bin/python3"),
            "{}",
            check.summary
        );
        assert!(check.summary.contains("venv/bin/fw"), "{}", check.summary);
        let fix = check.fix.unwrap();
        assert!(
            fix.contains("hash -r") && fix.contains("cargo/bin"),
            "{fix}"
        );
        assert!(
            check.details.iter().any(|d| d.contains("shadowed")),
            "{:?}",
            check.details
        );

        // This binary first: ok, the later one noted.
        let path = std::env::join_paths([&cargo, &venv]).unwrap();
        let found = executables_named("fw", Some(&path));
        let check = on_path_check("fw", &found, Some(&exe));
        assert_eq!(check.level, Level::Ok, "{check:?}");
        assert!(check.details[0].contains("venv/bin/fw"));
    }

    #[test]
    fn a_byte_identical_copy_is_this_binary_and_another_flywheel_warns() {
        let dir = tempfile::tempdir().unwrap();
        write_script(&dir.path().join("flywheel"), "#!/bin/sh\necho same\n");
        write_script(&dir.path().join("fw"), "#!/bin/sh\necho same\n");
        let exe = dir.path().join("flywheel").canonicalize().unwrap();
        assert_eq!(classify(&dir.path().join("fw"), Some(&exe)), Found::This);
        let other = |_: &Path| Found::OtherFlywheel;
        let check = on_path_check_with("fw", &[dir.path().join("fw")], Some(&exe), &other);
        assert_eq!(check.level, Level::Warn);
        assert!(check.fix.unwrap().contains("cargo install"));
        let check = on_path_check_with("fw", &[], Some(&exe), &other);
        assert_eq!(check.level, Level::Warn);
        assert!(check.summary.contains("no `fw` on PATH"));
        assert_eq!(
            on_path_check_with("flywheel", &[], Some(&exe), &other).level,
            Level::Info
        );
    }

    #[test]
    fn a_taken_port_fails_and_suggests_a_free_one() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let check = port_check(IpAddr::V4(Ipv4Addr::LOCALHOST), port, None);
        assert_eq!(check.level, Level::Fail, "{check:?}");
        assert!(check.fix.unwrap().contains("--port"));
        drop(taken);
        let check = port_check(IpAddr::V4(Ipv4Addr::LOCALHOST), port, None);
        assert_eq!(check.level, Level::Ok, "{check:?}");
    }

    #[test]
    fn state_dir_modes_and_secrets_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("hub");
        let checks = state_dir_checks(&state);
        assert_eq!(checks[0].level, Level::Info, "{checks:?}");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(state.join("token"), "t").unwrap();
        std::fs::set_permissions(state.join("token"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let checks = state_dir_checks(&state);
        assert_eq!(checks[0].level, Level::Warn, "{checks:?}");
        assert!(checks[0].fix.as_deref().unwrap().contains("chmod 700"));
        assert_eq!(checks[1].level, Level::Fail, "{checks:?}");
        assert!(checks[1].fix.as_deref().unwrap().contains("--rotate"));
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(state.join("token"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let checks = state_dir_checks(&state);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].level, Level::Ok, "{checks:?}");
    }

    #[test]
    fn bad_config_files_fail_and_absent_ones_pass() {
        let dir = tempfile::tempdir().unwrap();
        let checks = config_checks(dir.path());
        assert!(checks.iter().all(|c| c.level == Level::Ok), "{checks:?}");
        std::fs::write(dir.path().join("hub.toml"), "[server]\nport = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("notify.toml"), "url = 3\n").unwrap();
        std::fs::set_permissions(
            dir.path().join("notify.toml"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(dir.path().join("settings.json"), "{nope").unwrap();
        let checks = config_checks(dir.path());
        let level = |id: &str| checks.iter().find(|c| c.id == id).map(|c| c.level);
        assert_eq!(level("hub.toml"), Some(Level::Fail), "{checks:?}");
        assert_eq!(level("notify.toml"), Some(Level::Fail), "{checks:?}");
        assert_eq!(level("settings.json"), Some(Level::Fail), "{checks:?}");
    }

    #[test]
    fn disk_thresholds_and_answers_parse() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(disk_check(dir.path(), 0, 0).level, Level::Ok);
        assert_eq!(disk_check(dir.path(), u64::MAX, 0).level, Level::Warn);
        assert_eq!(
            disk_check(dir.path(), u64::MAX, u64::MAX).level,
            Level::Fail
        );
        // A dir not created yet: its nearest existing parent's filesystem.
        assert_eq!(disk_check(&dir.path().join("a/b"), 0, 0).level, Level::Ok);
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(ntp_synchronized("yes\n"), Some(true));
        assert_eq!(ntp_synchronized("no"), Some(false));
        assert_eq!(ntp_synchronized(""), None);
        let out = |stdout: &str, stderr: &str, code| runner::Output {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.to_owned(),
            truncated: false,
            timed_out: false,
        };
        assert_eq!(
            systemd_state(Some(&out("degraded\n", "", 1))),
            Ok("degraded".into())
        );
        assert_eq!(
            systemd_state(Some(&out(
                "",
                "Failed to connect to bus: No medium found",
                1
            ))),
            Err("Failed to connect to bus: No medium found".into())
        );
        assert_eq!(
            systemd_state(Some(&out("offline\n", "", 1))),
            Err("offline".into())
        );
    }

    #[test]
    fn the_report_renders_levels_details_and_fixes() {
        let checks = vec![
            Check::new("fw on PATH", Level::Fail, "bad")
                .detail("d")
                .fix("do x"),
            Check::new("disk", Level::Ok, "fine"),
        ];
        let text = render(&checks);
        assert!(text.contains("  FAIL  fw on PATH  bad\n"), "{text}");
        assert!(text.contains("fix: do x"), "{text}");
        assert!(text.ends_with("1 ok, 0 info, 0 warn, 1 fail\n"), "{text}");
    }
}
