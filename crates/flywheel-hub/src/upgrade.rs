//! `fw hub upgrade` and `fw hub start --handoff`: replace the running hub
//! with a new process (a new binary) without ending its sessions, through
//! a live handoff (see [`crate::handoff`]); and the new process's side of
//! `fw hub serve --handoff`.

use crate::cli::{HubArgs, StateDirArg, resolve_state_dir, running_hub};
use crate::daemon::{self, LOG_FILE};
use crate::handoff::{self, EXIT_BUSY, EXIT_NO_HUB, EXIT_REFUSED, HandoffError, Link, Refusal};
use crate::hub::{Hub, HubOptions};
use crate::remote::RuntimeInfo;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// How long `start --handoff` waits for the old process to exit once the
/// new one serves.
const OLD_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How much longer than its `--timeout` `start --handoff` waits: the old
/// hub gives up on a silent new one after `--timeout` and rolls back.
const ROLLBACK_GRACE: Duration = Duration::from_secs(10);
/// How long probing a binary (`hub start --help`) may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

fn fail(err: &mut dyn Write, message: &str) -> i32 {
    let _ = writeln!(err, "fw hub: {message}");
    1
}

// --- serve --handoff --------------------------------------------------------

/// A hub built from a handoff, until it has taken over.
pub(crate) struct TakenOver {
    listener: Option<std::net::TcpListener>,
    link: Link,
    old_pid: u32,
    old_version: String,
    sessions: usize,
    /// Of `sessions`, those whose process still ran.
    running: usize,
}

/// Receive the running hub's state and build this one from it (sessions
/// paused). An error is reported on `err` and comes back as the exit code
/// (the old hub carries on).
pub(crate) fn take_over(
    options: HubOptions,
    step: Option<Duration>,
    err: &mut dyn Write,
) -> Result<(Arc<Hub>, TakenOver), i32> {
    let state_dir = options.state_dir.clone();
    let received = match handoff::receive(&state_dir, options.scrollback_lines, step) {
        Ok(received) => received,
        Err(e @ HandoffError::NoHub(_)) => {
            let _ = writeln!(err, "fw hub: cannot hand off: {e}");
            return Err(EXIT_NO_HUB);
        }
        Err(e @ HandoffError::Refused { kind, .. }) => {
            let _ = writeln!(err, "fw hub: {e}");
            return Err(if kind == Refusal::Protocol {
                EXIT_REFUSED
            } else {
                EXIT_BUSY
            });
        }
        Err(e) => {
            let _ = writeln!(
                err,
                "fw hub: handoff failed, the running hub carries on: {e}"
            );
            return Err(1);
        }
    };
    let old_pid = received.old_pid;
    let old_version = received.old_version.clone();
    let sessions = received.sessions;
    let running = received.running();
    let (incoming, listener, link) = received.take();
    match Hub::adopt_handoff(options, incoming) {
        Ok(hub) => Ok((
            hub,
            TakenOver {
                listener: Some(listener),
                link,
                old_pid,
                old_version,
                sessions,
                running,
            },
        )),
        Err(e) => {
            link.fail(&e.to_string());
            let _ = writeln!(
                err,
                "fw hub: cannot take over (the running hub carries on): {e}"
            );
            Err(1)
        }
    }
}

impl TakenOver {
    /// The handed-over listening socket, for the runtime.
    pub(crate) fn listener(
        &mut self,
        runtime: &tokio::runtime::Runtime,
    ) -> std::io::Result<tokio::net::TcpListener> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| std::io::Error::other("the listening socket was already taken"))?;
        listener.set_nonblocking(true)?;
        let _enter = runtime.enter();
        tokio::net::TcpListener::from_std(listener)
    }

    /// Complete the handoff: the old hub lets go, this one's sessions
    /// start. Returns what to tell the operator.
    pub(crate) fn finish(self, hub: &Arc<Hub>) -> Result<String, String> {
        self.link.ready(hub).map_err(|e| e.to_string())?;
        let exited = self.sessions - self.running;
        Ok(format!(
            "took over {} running session{}{} from the hub at pid {} (flywheel {}); nothing was \
             restarted",
            self.running,
            if self.running == 1 { "" } else { "s" },
            if exited == 0 {
                String::new()
            } else {
                format!(" (and {exited} exited)")
            },
            self.old_pid,
            self.old_version
        ))
    }
}

// --- start --handoff --------------------------------------------------------

/// `fw hub start --handoff`: start this binary detached as a hub taking
/// over from the running one; returns once it serves and the old process
/// is gone. Exit codes: 0 handed off, [`EXIT_REFUSED`] refused as
/// incompatible, [`EXIT_BUSY`] refused for now (nothing changed either
/// way), [`EXIT_NO_HUB`] no hub running, 1 failed.
pub(crate) fn start_handoff(
    hub: &HubArgs,
    timeout: Duration,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match run_start_handoff(hub, timeout, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn run_start_handoff(
    hub: &HubArgs,
    timeout: Duration,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    hub.public_url_origins()?;
    let dir = resolve_state_dir(&hub.state)?;
    let dir = std::path::absolute(&dir).map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    let Some(old) = running_hub(&dir)? else {
        return Ok(crate::endpoint::not_running(&dir, err));
    };
    let log = dir.join(LOG_FILE);
    let offset = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this program: {e}"))?;
    let mut argv = vec![
        "hub".to_owned(),
        "serve".to_owned(),
        "--handoff".to_owned(),
        "--handoff-timeout".to_owned(),
        timeout.as_secs().max(1).to_string(),
    ];
    argv.extend(hub.to_argv(&dir));
    argv.extend(["--log-file".to_owned(), log.to_string_lossy().into_owned()]);
    let mut child = daemon::spawn_detached(&exe, &argv, &log)
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let pid = child.id();
    let mut exited = None;
    let rollback_line = format!("handoff to pid {pid} failed, still serving");
    let rolled_back =
        || daemon::read_from(&log, offset).is_ok_and(|text| text.contains(&rollback_line));
    let mut gave_up = false;
    // The old hub gives up on us after `timeout` (rolling back); wait a
    // little longer to see how it ended.
    let ready = daemon::wait_until(timeout + ROLLBACK_GRACE, || {
        if let Ok(Some(status)) = child.try_wait() {
            exited = Some(status);
            return true;
        }
        if matches!(RuntimeInfo::read(&dir), Ok(Some(info)) if info.pid == pid) {
            return true;
        }
        // The old hub rolled back while this one still runs: stuck.
        gave_up = rolled_back();
        gave_up
    }) && !gave_up;
    let output = || {
        daemon::read_from(&log, offset)
            .map(|text| daemon::last_lines(&text, 20).trim_end().to_owned())
            .unwrap_or_default()
    };
    if let Some(status) = exited {
        let _ = writeln!(err, "{}", output());
        return match status.code() {
            Some(code @ (EXIT_REFUSED | EXIT_BUSY)) => {
                let _ = writeln!(
                    err,
                    "fw hub: the running hub (pid {}) refused the handoff and carries on unchanged",
                    old.pid
                );
                Ok(code)
            }
            _ => Err(format!(
                "the new hub exited ({status}); {}; log: {}",
                carries_on(&dir, old.pid),
                log.display()
            )),
        };
    }
    if !ready {
        if (gave_up || rolled_back()) && handoff::process_alive(old.pid) {
            // The old hub let go of nothing and thawed: the stuck new hub
            // holds only copies of its descriptors, so ending it ends no
            // session.
            kill(pid);
            let _ = child.wait();
            return Err(format!(
                "the new hub (pid {pid}) was stuck (not ready within {}s) and was killed; the \
                 running hub (pid {}) rolled back and carries on unchanged; log: {}",
                timeout.as_secs(),
                old.pid,
                log.display()
            ));
        }
        // Never signalled: mid-handoff it may hold the sessions.
        return Err(format!(
            "the handoff to the new hub (pid {pid}) is unresolved after {}s: {}; the new hub may \
             still finish or give up -- check `fw hub status` and {}",
            (timeout + ROLLBACK_GRACE).as_secs(),
            if handoff::process_alive(old.pid) {
                format!(
                    "the old hub (pid {}) is still frozen or not answering",
                    old.pid
                )
            } else {
                format!("the old hub (pid {}) has exited", old.pid)
            },
            log.display()
        ));
    }
    let gone = handoff::wait_gone(old.pid, OLD_EXIT_TIMEOUT);
    let info = RuntimeInfo::read(&dir).ok().flatten();
    let at = info
        .as_ref()
        .map(|i| format!(" at http://{}", crate::cli::browse_host(i.listen)))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "flywheel hub handed off: pid {} -> pid {pid} (flywheel {}, {}){at}; every session kept \
         running",
        old.pid,
        env!("CARGO_PKG_VERSION"),
        exe.display()
    );
    if !gone {
        let _ = writeln!(
            err,
            "fw hub: the old hub (pid {}) has not exited yet",
            old.pid
        );
    }
    let _ = writeln!(out, "log: {} (`fw hub logs -f`)", log.display());
    Ok(0)
}

fn kill(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: kill(2) only sends a signal.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// Who serves `dir` now that a handoff from `old_pid` did not go through.
fn carries_on(dir: &Path, old_pid: u32) -> String {
    if handoff::process_alive(old_pid) {
        return format!("the running hub (pid {old_pid}) carries on");
    }
    match crate::remote::running_hub(dir) {
        Ok(Some(info)) => format!("the hub is now served by pid {}", info.pid),
        _ => format!(
            "the old hub (pid {old_pid}) is gone and no hub runs: `fw hub start` restores the \
             sessions"
        ),
    }
}

// --- upgrade ----------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct UpgradeArgs {
    /// The flywheel binary to run the hub with [default: `fw` on PATH when
    /// it is a flywheel with `fw hub`, else this program].
    #[arg(long, value_name = "PATH")]
    binary: Option<PathBuf>,
    /// When a live handoff is impossible because of version skew (an
    /// older hub, a binary without live handoff), stop and start the hub
    /// instead: sessions are restored, their processes restarted. Never
    /// for a systemd service (restart the unit) or a busy hub.
    #[arg(long)]
    allow_restart: bool,
    /// Seconds to wait for the new hub to be serving (the old hub stays
    /// frozen at most this long waiting for it, then rolls back). Exit
    /// codes: 0 handed off (or, with --allow-restart, restarted), 1
    /// failed (the hub carries on, or was rolled back), 3 no hub running,
    /// 4 refused as incompatible (nothing changed; --allow-restart), 5
    /// busy (nothing changed; retry).
    #[arg(long, value_name = "SECS", default_value_t = 90)]
    timeout: u64,
    #[command(flatten)]
    state: StateDirArg,
}

/// What a binary can do, from `<binary> hub start --help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Support {
    /// A flywheel with `fw hub start --handoff`.
    Handoff,
    /// A flywheel with `fw hub`, from before live handoff.
    HubOnly,
}

/// Run `<binary> hub start --help` (bounded) and read what it supports.
fn probe(binary: &Path) -> Result<Support, String> {
    let mut child = Command::new(binary)
        .args(["hub", "start", "--help"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run it: {e}"))?;
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = std::io::Read::read_to_string(pipe, &mut text);
        }
        text
    });
    let mut status = None;
    let finished = daemon::wait_until(PROBE_TIMEOUT, || {
        status = child.try_wait().ok().flatten();
        status.is_some()
    });
    if !finished {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "`hub start --help` did not finish within {}s",
            PROBE_TIMEOUT.as_secs()
        ));
    }
    let help = reader.join().unwrap_or_default();
    let ok = status.is_some_and(|s| s.success());
    if !ok
        || !help.contains("Usage:")
        || !help.contains("hub start")
        || !help.contains("--state-dir")
    {
        return Err(format!(
            "`hub start --help` failed ({}): not a flywheel binary with `fw hub`",
            status.map_or_else(|| "no status".to_owned(), |s| s.to_string())
        ));
    }
    Ok(if help.contains("--handoff") {
        Support::Handoff
    } else {
        Support::HubOnly
    })
}

/// The binary `upgrade` runs, and what it supports.
fn upgrade_binary(given: Option<&Path>, err: &mut dyn Write) -> Result<(PathBuf, Support), String> {
    let this = || std::env::current_exe().map_err(|e| format!("cannot locate this program: {e}"));
    let resolve = |path: &Path| {
        path.canonicalize()
            .map_err(|e| format!("--binary {}: {e}", path.display()))
    };
    if let Some(given) = given {
        let path = resolve(given)?;
        let support = probe(&path).map_err(|e| {
            format!(
                "--binary {}: {e}; the hub carries on unchanged",
                path.display()
            )
        })?;
        return Ok((path, support));
    }
    if let Some(on_path) = crate::agent::which("fw").and_then(|p| p.canonicalize().ok()) {
        match probe(&on_path) {
            Ok(support) => return Ok((on_path, support)),
            Err(e) => {
                let _ = writeln!(
                    err,
                    "fw hub: `fw` on PATH ({}) is unusable: {e}; using this program instead \
                     (`--binary PATH` picks one)",
                    on_path.display()
                );
            }
        }
    }
    let path = resolve(&this()?)?;
    let support = probe(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, support))
}

pub(crate) fn upgrade(args: &UpgradeArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_upgrade(args, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

/// How the old hub runs, for a restart.
struct OldHub {
    pid: u32,
    /// Its `serve` options (absolute state dir included).
    serve_args: Vec<String>,
    /// Its environment (Linux `/proc`), so the new hub resolves programs
    /// the way the old one did; `None`: this process's.
    env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
    /// Its binary, to come back with when a restart's start fails.
    exe: Option<PathBuf>,
}

fn run_upgrade(
    args: &UpgradeArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let dir = resolve_state_dir(&args.state)?;
    let dir = std::path::absolute(&dir).map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    let Some(info) = running_hub(&dir)? else {
        return Ok(crate::endpoint::not_running(&dir, err));
    };
    let (binary, support) = upgrade_binary(args.binary.as_deref(), err)?;
    let mut old = OldHub {
        pid: info.pid,
        serve_args: Vec::new(),
        env: process_env(info.pid),
        exe: process_exe(info.pid),
    };
    match handoff::describe(&dir) {
        Ok(description) => {
            if let Some(reason) = description.unsupported {
                // A systemd unit's hub: a stop + detached start would take
                // it out of its unit; restarting the unit is the way.
                let _ = writeln!(
                    err,
                    "fw hub: no live handoff: {reason}; the hub carries on unchanged"
                );
                return Ok(EXIT_REFUSED);
            }
            old.serve_args = description.serve_args;
        }
        Err(HandoffError::NoHub(_)) => {
            if let Some(unit) = handoff::systemd_unit(info.pid) {
                let _ = writeln!(
                    err,
                    "fw hub: the running hub (pid {}) predates live handoff and runs as the systemd \
                     service {unit}: restart it with `systemctl --user restart {unit}`; the hub \
                     carries on unchanged",
                    info.pid
                );
                return Ok(EXIT_REFUSED);
            }
            old.serve_args = old_serve_args(&dir, &info);
            return restart_or_refuse(
                args,
                &binary,
                &old,
                &format!("the running hub (pid {}) predates live handoff", info.pid),
                out,
                err,
            );
        }
        Err(e) => return Err(format!("cannot ask the running hub: {e}; it carries on")),
    }
    if support == Support::HubOnly {
        return restart_or_refuse(
            args,
            &binary,
            &old,
            &format!("{} predates live handoff", binary.display()),
            out,
            err,
        );
    }
    let _ = writeln!(
        out,
        "handing the hub (pid {}) off to {} ...",
        old.pid,
        binary.display()
    );
    let mut command = Command::new(&binary);
    command
        .args(["hub", "start", "--handoff", "--timeout"])
        .arg(args.timeout.to_string())
        .args(&old.serve_args);
    with_env(&mut command, old.env.as_deref());
    let output = command
        .output()
        .map_err(|e| format!("cannot run {}: {e}; the hub carries on", binary.display()))?;
    let _ = out.write_all(&output.stdout);
    let _ = err.write_all(&output.stderr);
    match output.status.code() {
        Some(0) => Ok(0),
        Some(EXIT_REFUSED) => restart_or_refuse(
            args,
            &binary,
            &old,
            "the running hub refused the handoff as incompatible (see above)",
            out,
            err,
        ),
        Some(EXIT_BUSY) => {
            let _ = writeln!(
                err,
                "fw hub: the running hub cannot hand off right now (see above); nothing changed, \
                 retry shortly"
            );
            Ok(EXIT_BUSY)
        }
        Some(EXIT_NO_HUB) => Ok(EXIT_NO_HUB),
        // `start --handoff` said what happened.
        Some(1) => Ok(1),
        status => {
            let _ = writeln!(
                err,
                "fw hub: the handoff did not complete ({}); {}",
                status.map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
                carries_on(&dir, old.pid)
            );
            Ok(1)
        }
    }
}

/// Fall back to stop + start under `--allow-restart`; otherwise refuse
/// (exit [`EXIT_REFUSED`]: nothing changed). The restart keeps the old
/// hub's serve options and environment; a start that fails brings the old
/// binary back.
fn restart_or_refuse(
    args: &UpgradeArgs,
    binary: &Path,
    old: &OldHub,
    reason: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    if !args.allow_restart {
        let _ = writeln!(
            err,
            "fw hub: no live handoff: {reason}; the hub carries on unchanged. \
             `fw hub upgrade --allow-restart` stops and starts it instead (sessions are \
             restored, their processes restarted)"
        );
        return Ok(EXIT_REFUSED);
    }
    let _ = writeln!(
        out,
        "no live handoff ({reason}); restarting the hub instead (sessions restored, processes \
         restarted)"
    );
    let env = old.env.as_deref();
    let run =
        |program: &Path, verb: &str, extra: &[String], out: &mut dyn Write, err: &mut dyn Write| {
            run_hub(program, verb, extra, env, out, err)
        };
    let state_dir = state_dir_of(&old.serve_args);
    let stopped = run(
        binary,
        "stop",
        &[
            "--timeout".to_owned(),
            args.timeout.to_string(),
            "--state-dir".to_owned(),
            state_dir.clone(),
        ],
        out,
        err,
    )?;
    // Gone meanwhile (EXIT_NO_HUB; an older binary said 0): start it all
    // the same.
    if stopped != 0 && stopped != EXIT_NO_HUB {
        return Ok(stopped);
    }
    let mut start_args = vec!["--timeout".to_owned(), args.timeout.to_string()];
    start_args.extend(old.serve_args.iter().cloned());
    let started = run(binary, "start", &start_args, out, err)?;
    if started == 0 {
        return Ok(0);
    }
    let Some(exe) = old.exe.as_deref().filter(|exe| exe.exists()) else {
        let _ = writeln!(
            err,
            "fw hub: the new hub did not start and the old binary is unknown: no hub runs; \
             `fw hub start` with the old flags restores the sessions"
        );
        return Ok(1);
    };
    let _ = writeln!(
        err,
        "fw hub: the new hub did not start; starting the old one again ({})",
        exe.display()
    );
    let back = run(exe, "start", &start_args, out, err)?;
    if back == 0 {
        let _ = writeln!(
            err,
            "fw hub: the old hub is back (sessions restored); the upgrade did not happen"
        );
    }
    Ok(1)
}

/// Run `<program> hub <verb> <extra>`, passing its output on.
fn run_hub(
    program: &Path,
    verb: &str,
    extra: &[String],
    env: Option<&[(std::ffi::OsString, std::ffi::OsString)]>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let mut command = Command::new(program);
    command.args(["hub", verb]).args(extra);
    with_env(&mut command, env);
    let output = command
        .output()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let _ = out.write_all(&output.stdout);
    let _ = err.write_all(&output.stderr);
    Ok(output.status.code().unwrap_or(1))
}

fn with_env(command: &mut Command, env: Option<&[(std::ffi::OsString, std::ffi::OsString)]>) {
    if let Some(env) = env {
        command.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
}

/// `--state-dir`'s value in serve options.
fn state_dir_of(serve_args: &[String]) -> String {
    serve_args
        .iter()
        .position(|a| a == "--state-dir")
        .and_then(|i| serve_args.get(i + 1))
        .cloned()
        .unwrap_or_default()
}

/// The serve options of a hub that cannot describe itself: its command
/// line after `serve` (Linux), less what only a detached start adds;
/// else what its runtime file records (address and public URLs).
fn old_serve_args(dir: &Path, info: &RuntimeInfo) -> Vec<String> {
    let dir = dir.to_string_lossy().into_owned();
    if let Some(argv) = process_cmdline(info.pid)
        && let Some(args) = serve_args_from_cmdline(&argv, &dir, info.listen.port())
    {
        return args;
    }
    let mut args = vec![
        "--bind".to_owned(),
        info.listen.ip().to_string(),
        "--port".to_owned(),
        info.listen.port().to_string(),
    ];
    for url in &info.public_urls {
        args.extend(["--public-url".to_owned(), url.clone()]);
    }
    args.extend(["--state-dir".to_owned(), dir]);
    args
}

/// `fw hub serve <options>` -> the options, without `--log-file`,
/// `--no-login-code`, `--handoff`, `--handoff-timeout` (a start adds its
/// own), with `--state-dir <dir>` (absolute) and `--port <port>` (the
/// one it listens on: `--port 0` would pick another).
fn serve_args_from_cmdline(argv: &[String], dir: &str, port: u16) -> Option<Vec<String>> {
    let at = argv
        .windows(2)
        .position(|w| w[0] == "hub" && w[1] == "serve")?;
    let mut args = Vec::new();
    let mut rest = argv[at + 2..].iter();
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value)),
            _ => (arg.as_str(), None),
        };
        match name {
            "--log-file" | "--handoff-timeout" | "--state-dir" | "--port" => {
                if inline.is_none() {
                    rest.next();
                }
            }
            "--no-login-code" | "--handoff" => {}
            _ => args.push(arg.clone()),
        }
    }
    args.extend([
        "--port".to_owned(),
        port.to_string(),
        "--state-dir".to_owned(),
        dir.to_owned(),
    ]);
    Some(args)
}

fn process_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

fn process_env(pid: u32) -> Option<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    use std::os::unix::ffi::OsStrExt;
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let env: Vec<_> = raw
        .split(|&b| b == 0)
        .filter_map(|entry| {
            let eq = entry.iter().position(|&b| b == b'=')?;
            (eq > 0).then(|| {
                (
                    std::ffi::OsStr::from_bytes(&entry[..eq]).to_owned(),
                    std::ffi::OsStr::from_bytes(&entry[eq + 1..]).to_owned(),
                )
            })
        })
        .collect();
    (!env.is_empty()).then_some(env)
}

fn process_exe(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|&a| a.to_owned()).collect()
    }

    #[test]
    fn a_restart_keeps_the_old_hubs_serve_options() {
        let argv = strings(&[
            "/usr/bin/fw",
            "hub",
            "serve",
            "--bind",
            "0.0.0.0",
            "--port",
            "7634",
            "--public-url",
            "https://box.tailnet.ts.net",
            "--state-dir",
            "rel/state",
            "--log-file=/s/hub.log",
            "--no-login-code",
            "--notify-url",
            "https://ntfy.sh/x",
        ]);
        assert_eq!(
            serve_args_from_cmdline(&argv, "/abs/state", 7634).unwrap(),
            strings(&[
                "--bind",
                "0.0.0.0",
                "--public-url",
                "https://box.tailnet.ts.net",
                "--notify-url",
                "https://ntfy.sh/x",
                "--port",
                "7634",
                "--state-dir",
                "/abs/state",
            ])
        );
        // `--port 0` picked a port: the restart keeps that one.
        let any = strings(&["fw", "hub", "serve", "--port=0"]);
        assert_eq!(
            serve_args_from_cmdline(&any, "/d", 41234).unwrap(),
            strings(&["--port", "41234", "--state-dir", "/d"])
        );
        assert!(serve_args_from_cmdline(&strings(&["/bin/sh", "-c", "x"]), "/d", 1).is_none());
    }

    #[test]
    fn without_a_command_line_the_runtime_file_gives_address_and_urls() {
        let info = RuntimeInfo {
            pid: u32::MAX,
            listen: "127.0.0.1:7634".parse().unwrap(),
            public_urls: vec!["https://box.example".into()],
            started_at: None,
            log_file: None,
            tls: None,
        };
        assert_eq!(
            old_serve_args(Path::new("/abs/state"), &info),
            strings(&[
                "--bind",
                "127.0.0.1",
                "--port",
                "7634",
                "--public-url",
                "https://box.example",
                "--state-dir",
                "/abs/state",
            ])
        );
    }

    #[test]
    fn probing_tells_flywheel_binaries_from_others() {
        // Echoing its arguments back is not answering `--help`.
        assert!(probe(Path::new("/bin/echo")).is_err());
        let error = probe(Path::new("/bin/false")).unwrap_err();
        assert!(error.contains("not a flywheel binary"), "{error}");
    }
}
