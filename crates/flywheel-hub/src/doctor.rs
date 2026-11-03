//! `fw hub doctor`: check what a hub on this machine depends on and say
//! how to fix what is off. Read-only: it creates, removes, and rewrites
//! nothing (a port is bound for a moment to see that it is free, and
//! programs are asked for `--version` / `--help`; scripts found on `PATH`
//! are read, never run).

use crate::agent::{self, SessionKind};
use crate::cli::{DEFAULT_PORT, StateDirArg, resolve_state_dir};
use crate::review::runner::{self, Spec};
use crate::util::shell_path;
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
    let pad = " ".repeat(width + 10);
    // Every line of a multi-line text (a TOML error's caret diagram)
    // indented under its row.
    let block = |text: &mut String, first: &str, body: &str| {
        let mut lines = body.lines();
        text.push_str(first);
        text.push_str(lines.next().unwrap_or_default());
        text.push('\n');
        for line in lines {
            text.push_str(&pad);
            text.push_str(line);
            text.push('\n');
        }
    };
    for check in checks {
        let row = format!("  {:<4}  {:<width$}  ", check.level.label(), check.id);
        block(&mut text, &row, &check.summary);
        for detail in &check.details {
            block(&mut text, &pad, detail);
        }
        if let Some(fix) = &check.fix {
            block(&mut text, &format!("{pad}fix: "), fix);
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
    let home = crate::config::home_dir();
    checks.extend(config_checks(dir, home.as_deref()));
    // What the next `fw hub start` would run with: --port, else hub.toml,
    // else the defaults (a bad file: the defaults).
    let flags = crate::config::Flags {
        port,
        ..crate::config::Flags::default()
    };
    let effective = crate::config::read_file(&dir.join(crate::config::CONFIG_FILE))
        .ok()
        .map(Option::unwrap_or_default)
        .and_then(|file| crate::config::resolve(&file, &flags, home.as_deref()).ok());
    let bind = effective
        .as_ref()
        .map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |e| e.bind);
    let port = effective
        .as_ref()
        .map_or(port.unwrap_or(DEFAULT_PORT), |e| e.port);
    checks.push(port_check(bind, port, hub.as_ref()));
    let tailscale = tailscale_check(hub.as_ref().map_or(port, |h| h.port));
    let tailscale_serves = tailscale.level == Level::Ok;
    checks.push(tailscale);
    let no_extra = crate::tls::ExtraNames::default();
    let extra = effective.as_ref().map_or(&no_extra, |e| &e.tls_names);
    // This machine's own names and addresses (not the tailnet's: those
    // are private anyway): a public one of them is not a warning.
    let own = crate::tls::hub_names(
        std::net::SocketAddr::new(bind, port),
        &crate::machines::host_name(),
        &crate::remote::interface_addresses(),
        None,
        &no_extra,
    );
    checks.push(tls_check(
        dir,
        effective
            .as_ref()
            .map_or(&crate::tls::TlsMode::Off, |e| &e.tls),
        extra,
        &own,
        tailscale_serves,
    ));
    checks.push(browser_check());
    checks.extend(systemd_checks());
    checks.push(clock_check());
    checks.push(disk_check(dir, DISK_WARN_BYTES, DISK_FAIL_BYTES));
    let limits = effective
        .as_ref()
        .map_or_else(crate::record::RecordLimits::default, |e| e.record_limits);
    checks.push(recordings_check(dir, limits));
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
    /// A flywheel with `fw hub`, but another build: its version, from
    /// `--version`, when it answers one (builds before `fw --version` do
    /// not).
    OtherFlywheel(Option<String>),
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
            if exe.is_some_and(|exe| installed_together(&real, exe)) {
                Found::This
            } else {
                Found::OtherFlywheel(
                    bounded(&real, &["--version"])
                        .filter(runner::Output::success)
                        .and_then(|out| parse_version(&out.stdout_text())),
                )
            }
        }
        _ => Found::Other,
    }
}

/// The version in a `fw --version` answer (`fw 0.6.1`, `flywheel 0.6.1`).
pub fn parse_version(text: &str) -> Option<String> {
    let line = first_line(text);
    let mut words = line.split_whitespace();
    let prog = words.next()?;
    let version = words.next()?;
    let looks_like = ["fw", "flywheel"].contains(&prog)
        && words.next().is_none()
        && version.starts_with(|c: char| c.is_ascii_digit());
    looks_like.then(|| version.to_owned())
}

/// Binaries one build writes side by side: `fw` and `flywheel` are two
/// bin targets, so never byte-identical, but one `cargo install` (or
/// `cargo build`) puts both in one directory within moments, at about the
/// same size. A copy from another build differs in time (and usually
/// size).
const SIBLING_NAMES: [&str; 2] = ["fw", "flywheel"];
/// How far apart one build writes its two binaries, at most.
const SIBLING_WRITE_WINDOW: Duration = Duration::from_secs(600);

pub fn installed_together(candidate: &Path, exe: &Path) -> bool {
    let name = |p: &Path| p.file_name().and_then(OsStr::to_str).map(str::to_owned);
    let (Some(a), Some(b)) = (name(candidate), name(exe)) else {
        return false;
    };
    if a == b || !SIBLING_NAMES.contains(&a.as_str()) || !SIBLING_NAMES.contains(&b.as_str()) {
        return false;
    }
    if candidate.parent() != exe.parent() {
        return false;
    }
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(candidate), std::fs::metadata(exe)) else {
        return false;
    };
    let (Ok(ta), Ok(tb)) = (ma.modified(), mb.modified()) else {
        return false;
    };
    let apart = ta.duration_since(tb).or_else(|_| tb.duration_since(ta));
    let (big, small) = (ma.len().max(mb.len()), ma.len().min(mb.len()));
    apart.is_ok_and(|d| d <= SIBLING_WRITE_WINDOW) && big - small <= big / 100
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
        Found::OtherFlywheel(version) => Check::new(
            id,
            Level::Warn,
            match version.as_deref() {
                Some(v) if v != env!("CARGO_PKG_VERSION") => format!(
                    "{} is flywheel {v}, not this one ({this}, flywheel {})",
                    first.display(),
                    env!("CARGO_PKG_VERSION")
                ),
                Some(v) => format!(
                    "{} is another flywheel {v} build than this one ({this})",
                    first.display()
                ),
                None => format!(
                    "{} is an older flywheel build (no `--version`) than this one ({this}, \
                     flywheel {})",
                    first.display(),
                    env!("CARGO_PKG_VERSION")
                ),
            },
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
                .find(|p| matches!(classify(p), Found::This | Found::OtherFlywheel(_)))
                .and_then(|p| p.parent())
                .map_or_else(|| exe_dir.clone(), |d| d.display().to_string());
            // What usually puts it there, when that is known.
            let origin = match &kind {
                Found::Script(interpreter) if interpreter.contains("python") => {
                    if first.parent().is_some_and(in_python_venv) {
                        " (a Python venv's entry point: deactivate the venv, or uninstall the \
                         package that installs it)"
                    } else {
                        " (a Python package's entry point: uninstall the package that \
                         installs it)"
                    }
                }
                _ => "",
            };
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
                "remove or rename {}{origin}, or put {shadowed} before {} on PATH; then \
                 `hash -r`",
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

/// A `bin` directory of a Python virtual environment (`pyvenv.cfg` beside
/// it).
fn in_python_venv(bin: &Path) -> bool {
    bin.parent()
        .is_some_and(|root| root.join("pyvenv.cfg").is_file())
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
/// lock is held. Observed, never taken: a probe lock (even a shared one)
/// would make a `fw hub start` racing it think another hub holds the dir.
fn running_hub(dir: &Path) -> Option<Running> {
    let info = crate::remote::RuntimeInfo::read(dir).ok().flatten()?;
    let held = match hub_holds_lock(info.pid, &dir.join(crate::remote::LOCK_FILE)) {
        Some(held) => held,
        // No `/proc` to read: the recorded process being alive.
        None => crate::daemon::process_alive(info.pid),
    };
    if !held {
        // A stale file from a hub that died.
        return None;
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

/// Whether process `pid` holds a `flock` on `path`: one of its open files
/// is `path` and its `fdinfo` lists a lock (Linux). `None` where `/proc`
/// cannot tell. (`/proc/locks` will not do: it hides a lock whose taker
/// has exited, and a handed-off hub holds the lock its predecessor took.)
fn hub_holds_lock(pid: u32, path: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let target = std::fs::metadata(path).ok()?;
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    for fd in fds.flatten() {
        let Ok(meta) = std::fs::metadata(fd.path()) else {
            continue;
        };
        if meta.dev() != target.dev() || meta.ino() != target.ino() {
            continue;
        }
        let info = std::fs::read_to_string(format!(
            "/proc/{pid}/fdinfo/{}",
            fd.file_name().to_string_lossy()
        ))
        .ok()?;
        if fdinfo_has_flock(&info) {
            return Some(true);
        }
    }
    Some(false)
}

/// Whether an `fdinfo` text lists a `flock` (`lock:\t1: FLOCK ...`).
pub fn fdinfo_has_flock(info: &str) -> bool {
    info.lines().any(|line| {
        line.strip_prefix("lock:")
            .is_some_and(|rest| rest.split_whitespace().nth(1) == Some("FLOCK"))
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
    let value = agent::claude_settings(&report);
    if let Err(problem) = hooks_shape(&value) {
        return Check::new(
            id,
            Level::Fail,
            format!("the hub's --settings are malformed: {problem}"),
        )
        .fix("a flywheel bug: reinstall a released build and report it");
    }
    let events = value["hooks"].as_object().map_or(0, serde_json::Map::len);
    // `--version` makes claude read --settings as JSON and stop; it checks
    // hook entries only when a session starts, and has no dry run. So this
    // shows the JSON is taken and well-formed, not that every hook loads.
    match bounded(claude, &["--settings", &value.to_string(), "--version"]) {
        Some(out) if out.success() => Check::new(
            id,
            Level::Ok,
            format!(
                "claude takes the hub's --settings JSON ({events} hook events, each running `{} \
                 hub report`)",
                exe.display()
            ),
        )
        .detail(
            "claude checks hook entries only when a session starts: a Claude session's dot \
             turning to working on its first prompt confirms them",
        ),
        Some(out) => Check::new(
            id,
            Level::Fail,
            format!(
                "claude refused the hub's --settings: {}",
                first_line(&out.error_text())
            ),
        )
        .fix("update Claude Code (`claude update`); status would fall back to screen reading"),
        None => Check::new(id, Level::Warn, "claude --settings did not answer in time")
            .fix("run `claude --version` by hand: a first run may wait on a prompt or an update"),
    }
}

/// The shape Claude Code's settings give hooks: `hooks` maps each event to
/// matcher groups, each a `hooks` list of `{"type":"command","command"}`.
pub fn hooks_shape(settings: &serde_json::Value) -> Result<(), String> {
    let hooks = settings["hooks"]
        .as_object()
        .ok_or("`hooks` is not an object")?;
    if hooks.is_empty() {
        return Err("no hook events".into());
    }
    for (event, groups) in hooks {
        let groups = groups
            .as_array()
            .ok_or(format!("`{event}` is not a list"))?;
        for group in groups {
            if group.get("matcher").is_some_and(|m| !m.is_string()) {
                return Err(format!("a `{event}` matcher is not a string"));
            }
            let entries = group["hooks"]
                .as_array()
                .filter(|e| !e.is_empty())
                .ok_or(format!("a `{event}` group has no `hooks` list"))?;
            for entry in entries {
                if entry["type"] != "command" {
                    return Err(format!("a `{event}` hook is not of type command"));
                }
                if entry["command"].as_str().is_none_or(str::is_empty) {
                    return Err(format!("a `{event}` hook has no command"));
                }
                if entry.get("args").is_some_and(|a| {
                    !a.as_array()
                        .is_some_and(|a| a.iter().all(serde_json::Value::is_string))
                }) {
                    return Err(format!("a `{event}` hook's args are not strings"));
                }
                if entry.get("timeout").is_some_and(|t| !t.is_u64()) {
                    return Err(format!("a `{event}` hook's timeout is not a number"));
                }
            }
        }
    }
    Ok(())
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
            .fix(format!(
                "sudo chown -R $(id -u):$(id -g) {}",
                shell_path(dir)
            )),
        );
    } else if !writable(dir) {
        checks.push(
            Check::new(
                id,
                Level::Fail,
                format!("{} is not writable", dir.display()),
            )
            .fix(format!("chmod u+rwx {}", shell_path(dir))),
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
            .fix(format!("chmod 700 {}", shell_path(dir))),
        );
    } else {
        checks.push(Check::new(
            id,
            Level::Ok,
            format!("{} (mode {mode:o}, yours)", dir.display()),
        ));
    }
    for secret in SECRET_FILES {
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
                        shell_path(&path),
                        if *secret == crate::auth::TOKEN_FILE {
                            "; then `fw hub token --rotate` (it may have been read)"
                        } else if *secret == crate::notify::CONFIG_FILE {
                            "; the hub ignores it (no push notifications) until then"
                        } else {
                            ""
                        }
                    )),
                );
            }
        }
    }
    // Directories of private files: recordings (whole terminal output)
    // and plugin logs (what plugins printed), written 0600.
    for (name, what) in PRIVATE_DIRS {
        let sub = dir.join(name);
        let Ok(entries) = std::fs::read_dir(&sub) else {
            continue;
        };
        let open: Vec<String> = entries
            .flatten()
            .filter(|e| {
                e.metadata()
                    .is_ok_and(|m| m.is_file() && m.mode() & 0o077 != 0)
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if open.is_empty() {
            continue;
        }
        let shown: Vec<&str> = open.iter().take(3).map(String::as_str).collect();
        checks.push(
            Check::new(
                "secrets",
                Level::Fail,
                format!(
                    "{} in {} {} readable by other users ({}{})",
                    crate::util::plural(open.len(), what),
                    sub.display(),
                    if open.len() == 1 { "is" } else { "are" },
                    shown.join(", "),
                    if open.len() > shown.len() {
                        ", ..."
                    } else {
                        ""
                    }
                ),
            )
            .fix(format!("chmod -R go-rwx {}", shell_path(&sub))),
        );
    }
    checks
}

/// State files holding credentials or their digests: never readable by
/// other users.
const SECRET_FILES: &[&str] = &[
    crate::auth::TOKEN_FILE,
    crate::auth::LOGINS_FILE,
    crate::notify::CONFIG_FILE,
    crate::machines::MACHINES_FILE,
    crate::peers::PEERS_FILE,
    crate::share::SHARES_FILE,
];

/// State directories whose files are private: (directory, what a file is).
const PRIVATE_DIRS: &[(&str, &str)] = &[
    (crate::record::RECORDINGS_DIR, "recording file"),
    (crate::plugins::LOG_DIR, "plugin log"),
];

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
/// `hub.toml` is checked by the validation `fw hub config check` and the
/// hub's start run (every key, `[recordings]` and `[shares]` included).
pub fn config_checks(dir: &Path, home: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();
    let hub_toml = dir.join(crate::config::CONFIG_FILE);
    checks.push(match crate::config::read_file(&hub_toml) {
        Ok(None) => Check::new("hub.toml", Level::Ok, "none (every setting is its default)"),
        Ok(Some(file)) => {
            match crate::config::resolve(&file, &crate::config::Flags::default(), home) {
                Ok(_) => Check::new("hub.toml", Level::Ok, format!("{} is valid", hub_toml.display())),
                Err(errors) => errors.into_iter().fold(
                    Check::new("hub.toml", Level::Fail, format!("{} has bad values", hub_toml.display()))
                        .fix("fix them (`fw hub config check` re-checks); the hub refuses to start until then"),
                    Check::detail,
                ),
            }
        }
        Err(message) => Check::new("hub.toml", Level::Fail, message).fix(
            "correct it where the error points (`fw hub config check` re-checks, `fw docs hub-reference` \
             lists the keys); the hub refuses to start until then",
        ),
    });
    // Its content only: a mode that is not private is the secrets check's
    // finding (and does not hide a parse error here).
    let notify_path = dir.join(crate::notify::CONFIG_FILE);
    match std::fs::read_to_string(&notify_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => checks.push(Check::new(
            "notify.toml",
            Level::Ok,
            "none (no push notifications)",
        )),
        Err(e) => checks.push(
            Check::new(
                "notify.toml",
                Level::Fail,
                format!("{}: {e}", notify_path.display()),
            )
            .fix(format!("chmod 600 {}", shell_path(&notify_path))),
        ),
        Ok(text) => checks.push(
            match crate::notify::NotifyConfig::from_parts(Some(&text), None) {
                Ok(Some(config)) => Check::new(
                    "notify.toml",
                    Level::Ok,
                    format!("pushes to {}", config.host()),
                ),
                Ok(None) => Check::new("notify.toml", Level::Ok, "no push notifications"),
                Err(message) => Check::new(
                    "notify.toml",
                    Level::Fail,
                    format!("{}: {message}", notify_path.display()),
                )
                .fix(
                    "correct it: url = \"https://ntfy.sh/<topic>\" (required), token = \"...\", \
                 events = [\"blocked\", \"done\"]; until then the hub starts without push \
                 notifications",
                ),
            },
        ),
    }
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
            if is_flywheel_hub(bind, port) {
                return Check::new(
                    id,
                    Level::Fail,
                    format!(
                        "{bind}:{port} is taken by another flywheel hub (one for another state \
                         dir)"
                    ),
                )
                .fix(format!(
                    "stop it (`fw hub stop --state-dir <its dir>`), or give this one another \
                     port{}",
                    free.map_or_else(String::new, |p| format!(
                        ": `fw hub start --port {p}` (or [server] port = {p} in hub.toml)"
                    ))
                ));
            }
            Check::new(
                id,
                Level::Fail,
                format!("{bind}:{port} is not available: {e}"),
            )
            .fix(fix)
        }
    }
}

/// Whether what listens on `bind:port` answers as a flywheel hub (its web
/// manifest, served without login).
fn is_flywheel_hub(bind: IpAddr, port: u16) -> bool {
    use std::net::TcpStream;
    let host = if bind.is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        bind
    };
    let timeout = Duration::from_secs(1);
    let Ok(mut stream) = TcpStream::connect_timeout(&SocketAddr::new(host, port), timeout) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = format!(
        "GET /manifest.webmanifest HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        SocketAddr::new(host, port)
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut body = Vec::new();
    let _ = stream.take(64 * 1024).read_to_end(&mut body);
    let text = String::from_utf8_lossy(&body);
    let Some((head, json)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    head.starts_with("HTTP/1.")
        && serde_json::from_str::<serde_json::Value>(json)
            .is_ok_and(|manifest| manifest["name"] == "flywheel hub")
}

fn tailscale_check(port: u16) -> Check {
    let id = "tailscale";
    let Some(program) = crate::remote::tailscale_program() else {
        return Check::new(
            id,
            Level::Info,
            "not installed: the hub is reachable from this machine only (or with --bind)",
        )
        .fix(
            "for a phone or another machine: install Tailscale, then `tailscale serve`; or \
             `--bind 0.0.0.0 --tls auto` (see the tls check)",
        );
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

/// Built-in TLS: off (and whether a phone then gets https any other way),
/// or the local CA's state (key mode, expiry), or the operator's files.
fn tls_check(
    dir: &Path,
    mode: &crate::tls::TlsMode,
    extra: &crate::tls::ExtraNames,
    own: &crate::tls::Names,
    tailscale_serves: bool,
) -> Check {
    use crate::tls::{LocalCa, TlsMode};
    let id = "tls";
    let allowed: Vec<String> = extra.public().iter().map(ToString::to_string).collect();
    match mode {
        TlsMode::Off if tailscale_serves => Check::new(
            id,
            Level::Ok,
            "built-in TLS off; `tailscale serve` gives phones https",
        ),
        TlsMode::Off => Check::new(
            id,
            Level::Info,
            "built-in TLS off: phones on the LAN get plain http (no app install, notifications, \
             or clipboard)",
        )
        .fix(
            "`fw hub serve --bind 0.0.0.0 --tls auto` (hub.toml [server] tls = \"auto\"): a local \
             CA name-constrained to this hub; phones install it from /ca",
        ),
        TlsMode::Files { cert, key } => {
            let missing: Vec<String> = [cert, key]
                .iter()
                .filter(|p| !p.is_file())
                .map(|p| p.display().to_string())
                .collect();
            if missing.is_empty() {
                Check::new(
                    id,
                    Level::Ok,
                    format!("your certificate {}", cert.display()),
                )
            } else {
                Check::new(
                    id,
                    Level::Fail,
                    format!("--tls files missing: {}", missing.join(", ")),
                )
                .fix("fix server.tls in hub.toml (absolute paths), or use `auto`")
            }
        }
        TlsMode::Auto => match LocalCa::load(dir) {
            Ok(None) if !allowed.is_empty() => Check::new(
                id,
                Level::Warn,
                format!(
                    "auto: the local CA made at the next start may sign for the public {} \
                     (--tls-allow-public-name): every device with it installed trusts this \
                     machine's CA key for them",
                    allowed.join(", ")
                ),
            )
            .fix("only for names you own; else drop them from tls_names and tls_public_names"),
            Ok(None) => Check::new(
                id,
                Level::Info,
                "auto: the local CA is made at the next start (or now: `fw hub tls ca`)",
            ),
            Err(e) => Check::new(id, Level::Fail, format!("auto: the local CA: {e}"))
                .fix("`fw hub tls rotate` makes a new one (devices re-install it)"),
            Ok(Some(ca)) => {
                let key = dir.join(crate::tls::TLS_DIR).join(crate::tls::CA_KEY_FILE);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(&key)
                        .map(|m| m.permissions().mode() & 0o777)
                        .unwrap_or(0);
                    if mode & 0o077 != 0 {
                        return Check::new(
                            id,
                            Level::Fail,
                            format!("the CA key {} is mode {mode:o}", key.display()),
                        )
                        .fix(format!(
                            "`chmod 600 {}`, then `fw hub tls rotate` (it may have been read)",
                            key.display()
                        ));
                    }
                }
                let left = ca
                    .expires()
                    .duration_since(std::time::SystemTime::now())
                    .unwrap_or_default();
                let summary = format!(
                    "auto: local CA {}",
                    crate::machines::display_fingerprint(ca.fingerprint())
                );
                let wide = ca.constraints().too_wide();
                let public = ca.constraints().public(own, extra);
                let unallowed: Vec<String> = public
                    .iter()
                    .filter(|s| !extra.public().contains(s))
                    .map(ToString::to_string)
                    .collect();
                let check = if !wide.is_empty() {
                    Check::new(
                        id,
                        Level::Fail,
                        format!(
                            "{summary} is constrained to {}, which covers a whole TLD",
                            wide.join(", ")
                        ),
                    )
                    .fix("`fw hub tls rotate`, then re-install the CA on every device")
                } else if !unallowed.is_empty() {
                    Check::new(
                        id,
                        Level::Warn,
                        format!(
                            "{summary} may sign for the public {}, which hub.toml does not opt \
                             in to (tls_public_names): every device with it installed trusts \
                             this machine's CA key for them",
                            unallowed.join(", ")
                        ),
                    )
                    .fix(
                        "unless you own them and opted in on the serve command line: `fw hub \
                         tls rotate`, then remove the old CA from every device",
                    )
                } else if !public.is_empty() || !allowed.is_empty() {
                    let signs: Vec<String> = public.iter().map(ToString::to_string).collect();
                    Check::new(
                        id,
                        Level::Warn,
                        if signs.is_empty() {
                            format!(
                                "{summary}: public names allowed ({}): the CA made by the next \
                                 `fw hub tls rotate` signs for them",
                                allowed.join(", ")
                            )
                        } else {
                            format!(
                                "{summary} may sign for the public {} (--tls-allow-public-name): \
                                 every device with it installed trusts this machine's CA key \
                                 for them",
                                signs.join(", ")
                            )
                        },
                    )
                    .fix(
                        "only for names you own; else drop them from tls_names and \
                         tls_public_names, then `fw hub tls rotate`",
                    )
                } else if left < Duration::from_secs(30 * 24 * 3600) {
                    Check::new(id, Level::Warn, format!("{summary} expires within 30 days"))
                        .fix("`fw hub tls rotate` (devices re-install it)")
                } else {
                    Check::new(id, Level::Ok, summary)
                };
                check.detail(format!(
                    "name-constrained to: {}",
                    ca.constraints().list().join(", ")
                ))
            }
        },
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
        crate::util::human_bytes(free),
        probe.display(),
        crate::util::human_bytes(logs)
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

/// What the recordings take against `[recordings]
/// dir_cap` (past it, the oldest finished ones are deleted).
pub fn recordings_check(dir: &Path, limits: crate::record::RecordLimits) -> Check {
    let id = "recordings";
    let recordings = crate::record::Recordings::new(dir, limits);
    let cap = limits.max_total_bytes;
    let cap_text = crate::record::format_size(cap);
    let list = match recordings.list() {
        Ok(list) => list,
        Err(e) => {
            return Check::new(id, Level::Warn, e.to_string()).fix(format!(
                "make {} readable by this user (chmod 700)",
                recordings.dir().display()
            ));
        }
    };
    let used: u64 = list.iter().map(|r| r.size).sum();
    let summary = format!(
        "{} of dir_cap {cap_text} in {} file{}",
        crate::util::human_bytes(used),
        list.len(),
        if list.len() == 1 { "" } else { "s" }
    );
    let check = if used > cap {
        Check::new(id, Level::Warn, summary).fix(format!(
            "over the cap: the next recording start or rotation deletes the oldest finished \
             ones; `fw hub record ls` / `fw hub record rm NAME` to choose, or raise \
             [recordings] dir_cap (now {cap_text})"
        ))
    } else if used.saturating_mul(10) >= cap.saturating_mul(9) {
        Check::new(id, Level::Info, summary).fix(format!(
            "near the cap: the oldest finished recordings go first past it; keep any with \
             `fw hub record get NAME`, or raise [recordings] dir_cap (now {cap_text})"
        ))
    } else {
        Check::new(id, Level::Ok, summary)
    };
    check.detail(format!("in {}", recordings.dir().display()))
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
        // No pyvenv.cfg: a Python package's entry point, not a venv's.
        assert!(fix.contains("a Python package's entry point"), "{fix}");
        std::fs::write(dir.path().join("venv/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        let fix = on_path_check("fw", &found, Some(&exe)).fix.unwrap();
        assert!(fix.contains("a Python venv's entry point"), "{fix}");
        // A shell script: no Python advice at all.
        write_script(&venv.join("fw"), "#!/bin/bash\necho hi\n");
        let check = on_path_check("fw", &found, Some(&exe));
        assert_eq!(check.level, Level::Fail);
        assert!(
            check.summary.contains("a script run by /bin/bash"),
            "{}",
            check.summary
        );
        assert!(!check.fix.unwrap().contains("Python"));
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
    fn the_tls_check_warns_of_public_names() {
        use crate::tls::{ExtraNames, LocalCa, TlsMode, hub_names, parse_name};
        let dir = tempfile::tempdir().unwrap();
        let listen = "0.0.0.0:7600".parse().unwrap();
        let interfaces = [("eth0".to_owned(), "192.168.1.20".parse().unwrap())];
        let own = hub_names(listen, "box", &interfaces, None, &ExtraNames::default());
        let name = parse_name("hub.example.com").unwrap();
        let allowed = ExtraNames::new(vec![name.clone()], vec![name]).unwrap();
        // Allowed, no CA yet: a warning before one is made.
        let check = tls_check(dir.path(), &TlsMode::Auto, &allowed, &own, false);
        assert_eq!(check.level, Level::Warn, "{}", check.summary);
        assert!(
            check.summary.contains("public hub.example.com"),
            "{}",
            check.summary
        );
        // A CA that signs for it: a warning while the opt-in stands ...
        let names = hub_names(listen, "box", &interfaces, None, &allowed);
        LocalCa::load_or_create(dir.path(), &names, &allowed).unwrap();
        let check = tls_check(dir.path(), &TlsMode::Auto, &allowed, &own, false);
        assert_eq!(check.level, Level::Warn, "{}", check.summary);
        assert!(
            check
                .summary
                .contains("may sign for the public hub.example.com"),
            "{}",
            check.summary
        );
        // ... and still once it is gone, now pointing at rotation.
        let check = tls_check(
            dir.path(),
            &TlsMode::Auto,
            &ExtraNames::default(),
            &own,
            false,
        );
        assert_eq!(check.level, Level::Warn, "{}", check.summary);
        assert!(
            check.summary.contains("does not opt in"),
            "{}",
            check.summary
        );
        assert!(check.fix.unwrap().contains("rotate"));
        // A private-only CA is fine (the machine's own addresses included).
        LocalCa::rotate(dir.path(), &own, &ExtraNames::default()).unwrap();
        let check = tls_check(
            dir.path(),
            &TlsMode::Auto,
            &ExtraNames::default(),
            &own,
            false,
        );
        assert_eq!(check.level, Level::Ok, "{}", check.summary);
    }

    #[test]
    fn a_byte_identical_copy_is_this_binary_and_another_flywheel_warns() {
        let dir = tempfile::tempdir().unwrap();
        write_script(&dir.path().join("flywheel"), "#!/bin/sh\necho same\n");
        write_script(&dir.path().join("fw"), "#!/bin/sh\necho same\n");
        let exe = dir.path().join("flywheel").canonicalize().unwrap();
        assert_eq!(classify(&dir.path().join("fw"), Some(&exe)), Found::This);
        let other = |_: &Path| Found::OtherFlywheel(None);
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
    fn fw_and_flywheel_written_together_are_one_install() {
        let dir = tempfile::tempdir().unwrap();
        let fw = dir.path().join("fw");
        let flywheel = dir.path().join("flywheel");
        std::fs::write(&fw, vec![1u8; 10_000]).unwrap();
        std::fs::write(&flywheel, vec![2u8; 10_050]).unwrap();
        assert!(installed_together(&fw, &flywheel));
        assert!(installed_together(&flywheel, &fw));
        // Not a pair: the same name, another name, another directory.
        assert!(!installed_together(&fw, &fw));
        let other = dir.path().join("sub");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("fw"), vec![1u8; 10_000]).unwrap();
        assert!(!installed_together(&other.join("fw"), &flywheel));
        std::fs::write(dir.path().join("fwx"), vec![1u8; 10_000]).unwrap();
        assert!(!installed_together(&dir.path().join("fwx"), &flywheel));
        // Another build: written an hour apart, or a different size.
        let file = std::fs::File::options().write(true).open(&fw).unwrap();
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        assert!(!installed_together(&fw, &flywheel));
        std::fs::write(&fw, vec![1u8; 20_000]).unwrap();
        assert!(!installed_together(&fw, &flywheel));
    }

    // `/proc/<pid>/fd` and `fdinfo`: elsewhere `hub_holds_lock` is `None`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_held_lock_is_seen_without_taking_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.lock");
        let holder = std::fs::File::create(&path).unwrap();
        let me = std::process::id();
        assert_eq!(hub_holds_lock(me, &path), Some(false));
        holder.lock().unwrap();
        assert_eq!(hub_holds_lock(me, &path), Some(true));
        // Looking took nothing: once the holder lets go, an exclusive lock
        // is granted at once.
        holder.unlock().unwrap();
        assert_eq!(hub_holds_lock(me, &path), Some(false));
        let other = std::fs::File::open(&path).unwrap();
        other.try_lock().unwrap();
        other.unlock().unwrap();
        // A process without the file open holds nothing.
        assert!(!hub_holds_lock(1, &path).unwrap_or(false));
    }

    #[test]
    fn an_fdinfo_flock_line_is_told_from_other_locks() {
        assert!(fdinfo_has_flock(
            "pos:\t0\nflags:\t02100002\nmnt_id:\t1363\nino:\t4366546\nlock:\t1: FLOCK  ADVISORY  WRITE 0 08:30:4366546 0 EOF\n"
        ));
        assert!(!fdinfo_has_flock("pos:\t0\nflags:\t02\n"));
        assert!(!fdinfo_has_flock(
            "lock:\t1: POSIX  ADVISORY  WRITE 7 08:30:5 0 EOF\n"
        ));
    }

    #[test]
    fn the_hubs_hook_settings_have_claudes_shape() {
        let report = vec!["/x/fw".to_owned(), "hub".to_owned()];
        assert_eq!(hooks_shape(&agent::claude_settings(&report)), Ok(()));
        let bad = |v: serde_json::Value| hooks_shape(&v).unwrap_err();
        assert!(bad(serde_json::json!({ "hooks": "notanobject" })).contains("not an object"));
        assert!(bad(serde_json::json!({ "hooks": { "Stop": {} } })).contains("not a list"));
        assert!(
            bad(serde_json::json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command" }] }] } }))
                .contains("no command")
        );
        assert!(
            bad(serde_json::json!({ "hooks": { "Stop": [{ "hooks": [
                { "type": "command", "command": "x", "args": [1] }
            ] }] } }))
            .contains("args")
        );
    }

    #[test]
    fn a_port_held_by_another_hub_is_named() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = taken.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = crate::web::MANIFEST;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            taken
        });
        let check = port_check(IpAddr::V4(Ipv4Addr::LOCALHOST), port, None);
        let taken = server.join().unwrap();
        assert_eq!(check.level, Level::Fail, "{check:?}");
        assert!(check.summary.contains("another flywheel hub"), "{check:?}");
        assert!(check.fix.unwrap().contains("fw hub stop"));
        drop(taken);
    }

    #[test]
    fn a_taken_port_fails_and_suggests_a_free_one() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let check = port_check(IpAddr::V4(Ipv4Addr::LOCALHOST), port, None);
        assert_eq!(check.level, Level::Fail, "{check:?}");
        assert!(check.fix.unwrap().contains("--port"));
        drop(taken);
        // A free port is ok. (Retried on fresh ports: once freed, a port
        // can be taken at once by a parallel test's connection.)
        let free = (0..10).any(|_| {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            port_check(IpAddr::V4(Ipv4Addr::LOCALHOST), port, None).level == Level::Ok
        });
        assert!(free);
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

        // The newer secrets too: share and peer digests, recordings (whole
        // terminal output), and plugin logs.
        let open = |path: &Path| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        };
        for file in [crate::share::SHARES_FILE, crate::peers::PEERS_FILE] {
            std::fs::write(state.join(file), "{}").unwrap();
            open(&state.join(file));
        }
        for (sub, name) in [
            (crate::record::RECORDINGS_DIR, "a.cast"),
            (crate::plugins::LOG_DIR, "p.jsonl"),
        ] {
            std::fs::create_dir(state.join(sub)).unwrap();
            std::fs::write(state.join(sub).join(name), "x").unwrap();
            open(&state.join(sub).join(name));
        }
        let failing: Vec<Check> = state_dir_checks(&state)
            .into_iter()
            .filter(|c| c.level == Level::Fail)
            .collect();
        let said = |needle: &str| failing.iter().any(|c| c.summary.contains(needle));
        assert_eq!(failing.len(), 4, "{failing:?}");
        assert!(said("shares.json") && said("peers.json"), "{failing:?}");
        assert!(
            said("1 recording file") && said("1 plugin log"),
            "{failing:?}"
        );
        assert!(
            failing
                .iter()
                .any(|c| c.fix.as_deref().unwrap().starts_with("chmod -R go-rwx ")),
            "{failing:?}"
        );
    }

    #[test]
    fn bad_config_files_fail_and_absent_ones_pass() {
        let dir = tempfile::tempdir().unwrap();
        let checks = config_checks(dir.path(), None);
        assert!(checks.iter().all(|c| c.level == Level::Ok), "{checks:?}");
        std::fs::write(dir.path().join("hub.toml"), "[server]\nport = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("notify.toml"), "url = 3\n").unwrap();
        std::fs::set_permissions(
            dir.path().join("notify.toml"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(dir.path().join("settings.json"), "{nope").unwrap();
        let checks = config_checks(dir.path(), None);
        let level = |id: &str| checks.iter().find(|c| c.id == id).map(|c| c.level);
        assert_eq!(level("hub.toml"), Some(Level::Fail), "{checks:?}");
        assert_eq!(level("notify.toml"), Some(Level::Fail), "{checks:?}");
        assert_eq!(level("settings.json"), Some(Level::Fail), "{checks:?}");
        assert!(checks.iter().all(|c| c.fix.as_deref() != Some("fix it")));

        // A garbage notify.toml readable by others: the mode is the secrets
        // check's (once), the parse error still shows here.
        std::fs::write(dir.path().join("notify.toml"), "garbage[[\n").unwrap();
        std::fs::set_permissions(
            dir.path().join("notify.toml"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let notify: Vec<Check> = config_checks(dir.path(), None)
            .into_iter()
            .filter(|c| c.id == "notify.toml")
            .collect();
        assert_eq!(notify.len(), 1, "{notify:?}");
        assert!(!notify[0].summary.contains("mode"), "{notify:?}");
        let secrets: Vec<Check> = state_dir_checks(dir.path())
            .into_iter()
            .filter(|c| c.id == "secrets")
            .collect();
        assert_eq!(secrets.len(), 1, "{secrets:?}");
        assert!(secrets[0].fix.as_deref().unwrap().contains("ignores it"));
    }

    #[test]
    fn version_answers_parse_and_others_do_not() {
        assert_eq!(parse_version("fw 0.6.1\n"), Some("0.6.1".to_owned()));
        assert_eq!(
            parse_version("flywheel 1.2.3-rc1"),
            Some("1.2.3-rc1".to_owned())
        );
        assert_eq!(
            parse_version("error: unexpected argument '--version' found"),
            None
        );
        assert_eq!(parse_version("fw"), None);
        assert_eq!(parse_version("claude 0.6.1"), None);
    }

    #[test]
    fn another_flywheel_names_its_version_against_this_one() {
        let dir = tempfile::tempdir().unwrap();
        write_script(&dir.path().join("fw"), "#!/bin/sh\nexit 0\n");
        let found = [dir.path().join("fw")];
        let older = |_: &Path| Found::OtherFlywheel(Some("0.0.1".to_owned()));
        let check = on_path_check_with("fw", &found, None, &older);
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.summary.contains("is flywheel 0.0.1, not this one"),
            "{}",
            check.summary
        );
        assert!(
            check.summary.contains(env!("CARGO_PKG_VERSION")),
            "{}",
            check.summary
        );
        let same = |_: &Path| Found::OtherFlywheel(Some(env!("CARGO_PKG_VERSION").to_owned()));
        let check = on_path_check_with("fw", &found, None, &same);
        assert!(
            check.summary.contains("another flywheel"),
            "{}",
            check.summary
        );
        let unknown = |_: &Path| Found::OtherFlywheel(None);
        let check = on_path_check_with("fw", &found, None, &unknown);
        assert!(
            check.summary.contains("no `--version`"),
            "{}",
            check.summary
        );
    }

    #[test]
    fn bad_recordings_and_shares_keys_fail_the_hub_toml_check() {
        let dir = tempfile::tempdir().unwrap();
        let level = |checks: &[Check]| checks.iter().find(|c| c.id == "hub.toml").map(|c| c.level);
        std::fs::write(
            dir.path().join("hub.toml"),
            "[recordings]\npart_cap = \"8MiB\"\ndir_cap = \"64MiB\"\n\
             [shares]\ndefault_expiry = \"1h\"\nmax_expiry = \"7d\"\n",
        )
        .unwrap();
        let checks = config_checks(dir.path(), None);
        assert_eq!(level(&checks), Some(Level::Ok), "{checks:?}");
        for bad in [
            "[recordings]\npart_cap = \"lots\"\n",
            "[recordings]\npart_cap = \"64MiB\"\ndir_cap = \"1MiB\"\n",
            "[shares]\ndefault_expiry = \"30d\"\nmax_expiry = \"1d\"\n",
        ] {
            std::fs::write(dir.path().join("hub.toml"), bad).unwrap();
            let checks = config_checks(dir.path(), None);
            assert_eq!(level(&checks), Some(Level::Fail), "{bad}: {checks:?}");
            let check = checks.iter().find(|c| c.id == "hub.toml").unwrap();
            assert!(
                check
                    .details
                    .iter()
                    .any(|d| d.contains("recordings.") || d.contains("shares.")),
                "{bad}: {check:?}"
            );
        }
    }

    #[test]
    fn recordings_usage_is_measured_against_dir_cap() {
        use crate::record::{MIN_PART_BYTES, RecordLimits};
        let dir = tempfile::tempdir().unwrap();
        let limits = RecordLimits {
            max_file_bytes: MIN_PART_BYTES,
            max_total_bytes: 10 * 1024,
        };
        let check = recordings_check(dir.path(), limits);
        assert_eq!(check.level, Level::Ok, "{check:?}");
        assert!(
            check.summary.contains("0 B of dir_cap"),
            "{}",
            check.summary
        );
        let recs = dir.path().join(crate::record::RECORDINGS_DIR);
        std::fs::create_dir_all(&recs).unwrap();
        std::fs::write(recs.join("a-1.cast"), vec![b'x'; 4 * 1024]).unwrap();
        // Not a recording: not counted.
        std::fs::write(recs.join("active.json"), vec![b'x'; 64 * 1024]).unwrap();
        let check = recordings_check(dir.path(), limits);
        assert_eq!(check.level, Level::Ok, "{check:?}");
        assert!(check.summary.contains("in 1 file"), "{}", check.summary);
        std::fs::write(recs.join("b-2.cast"), vec![b'x'; 5 * 1024 + 512]).unwrap();
        let check = recordings_check(dir.path(), limits);
        assert_eq!(check.level, Level::Info, "{check:?}");
        assert!(check.fix.as_deref().unwrap().contains("near the cap"));
        std::fs::write(recs.join("c-3.part2.cast"), vec![b'x'; 1024]).unwrap();
        let check = recordings_check(dir.path(), limits);
        assert_eq!(check.level, Level::Warn, "{check:?}");
        assert!(check.fix.as_deref().unwrap().contains("dir_cap"));
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
        assert_eq!(crate::util::human_bytes(1536), "1.5 KiB");
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
        // A multi-line summary stays in its column.
        let multi = render(&[Check::new(
            "hub.toml",
            Level::Fail,
            "TOML parse error\n  |\n1 | x\n",
        )]);
        let pad = " ".repeat("hub.toml".len() + 10);
        assert!(
            multi.starts_with(&format!(
                "  FAIL  hub.toml  TOML parse error\n{pad}  |\n{pad}1 | x\n"
            )),
            "{multi}"
        );
        assert!(text.contains("fix: do x"), "{text}");
        assert!(text.ends_with("1 ok, 0 info, 0 warn, 1 fail\n"), "{text}");
    }
}
