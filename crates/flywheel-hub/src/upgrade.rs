//! `fw hub upgrade` and `fw hub start --handoff`: replace the running hub
//! with a new process (a new binary) without ending its sessions, through
//! a live handoff (see [`crate::handoff`]); and the new process's side of
//! `fw hub serve --handoff`.

use crate::cli::{HubArgs, StateDirArg, resolve_state_dir, running_hub};
use crate::daemon::{self, LOG_FILE};
use crate::handoff::{self, EXIT_NO_HUB, EXIT_REFUSED, HandoffError, Link};
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
}

/// Receive the running hub's state and build this one from it (sessions
/// paused). An error is reported on `err` and comes back as the exit code
/// (the old hub carries on).
pub(crate) fn take_over(
    options: HubOptions,
    err: &mut dyn Write,
) -> Result<(Arc<Hub>, TakenOver), i32> {
    let state_dir = options.state_dir.clone();
    let received = match handoff::receive(&state_dir, options.scrollback_lines) {
        Ok(received) => received,
        Err(e @ HandoffError::NoHub(_)) => {
            let _ = writeln!(err, "fw hub: cannot hand off: {e}");
            return Err(EXIT_NO_HUB);
        }
        Err(e @ HandoffError::Refused(_)) => {
            let _ = writeln!(err, "fw hub: {e}");
            return Err(EXIT_REFUSED);
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
    /// start.
    pub(crate) fn finish(self, hub: &Arc<Hub>, out: &mut dyn Write) -> Result<(), String> {
        self.link.ready(hub).map_err(|e| e.to_string())?;
        let _ = writeln!(
            out,
            "took over {} session{} from the hub at pid {} (flywheel {}); nothing was restarted",
            self.sessions,
            if self.sessions == 1 { "" } else { "s" },
            self.old_pid,
            self.old_version
        );
        Ok(())
    }
}

// --- start --handoff --------------------------------------------------------

/// `fw hub start --handoff`: start this binary detached as a hub taking
/// over from the running one; returns once it serves and the old process
/// is gone. Exit codes: 0 handed off, [`EXIT_REFUSED`] refused (nothing
/// changed), 1 failed.
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
        return Err(format!(
            "no hub is running for state dir {} (`fw hub start` starts one)",
            dir.display()
        ));
    };
    let log = dir.join(LOG_FILE);
    let offset = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this program: {e}"))?;
    let mut argv = vec!["hub".to_owned(), "serve".to_owned(), "--handoff".to_owned()];
    argv.extend(hub.to_argv(&dir));
    argv.extend(["--log-file".to_owned(), log.to_string_lossy().into_owned()]);
    let mut child = daemon::spawn_detached(&exe, &argv, &log)
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let pid = child.id();
    let mut exited = None;
    let ready = daemon::wait_until(timeout, || {
        if let Ok(Some(status)) = child.try_wait() {
            exited = Some(status);
            return true;
        }
        matches!(RuntimeInfo::read(&dir), Ok(Some(info)) if info.pid == pid)
    });
    let output = || {
        daemon::read_from(&log, offset)
            .map(|text| daemon::last_lines(&text, 20).trim_end().to_owned())
            .unwrap_or_default()
    };
    if let Some(status) = exited {
        let _ = writeln!(err, "{}", output());
        return match status.code() {
            Some(EXIT_REFUSED) => {
                let _ = writeln!(
                    err,
                    "fw hub: the running hub (pid {}) refused the handoff and carries on unchanged",
                    old.pid
                );
                Ok(EXIT_REFUSED)
            }
            _ => Err(format!(
                "the new hub exited ({status}); the running hub (pid {}) carries on; log: {}",
                old.pid,
                log.display()
            )),
        };
    }
    if !ready {
        // Never signalled: mid-handoff it may hold the sessions.
        return Err(format!(
            "the new hub (pid {pid}) was not serving within {}s; see {} (`fw hub status`)",
            timeout.as_secs(),
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
        "flywheel hub handed off: pid {} -> pid {pid}{at}; every session kept running",
        old.pid
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

// --- upgrade ----------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct UpgradeArgs {
    /// The flywheel binary to run the hub with [default: `fw` on PATH,
    /// else this program].
    #[arg(long, value_name = "PATH")]
    binary: Option<PathBuf>,
    /// When a live handoff is impossible (version skew, an older hub, a
    /// systemd service), stop and start the hub instead: sessions are
    /// restored, their processes restarted.
    #[arg(long)]
    allow_restart: bool,
    /// Seconds to wait for the new hub to be serving.
    #[arg(long, value_name = "SECS", default_value_t = 90)]
    timeout: u64,
    #[command(flatten)]
    state: StateDirArg,
}

/// The binary `upgrade` runs.
fn upgrade_binary(given: Option<&Path>) -> Result<PathBuf, String> {
    let path = match given {
        Some(path) => path.to_path_buf(),
        None => match crate::agent::which("fw") {
            Some(path) => path,
            None => {
                std::env::current_exe().map_err(|e| format!("cannot locate this program: {e}"))?
            }
        },
    };
    let path = path
        .canonicalize()
        .map_err(|e| format!("--binary {}: {e}", path.display()))?;
    Ok(path)
}

pub(crate) fn upgrade(args: &UpgradeArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_upgrade(args, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn run_upgrade(
    args: &UpgradeArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let dir = resolve_state_dir(&args.state)?;
    let dir = std::path::absolute(&dir).map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    let Some(old) = running_hub(&dir)? else {
        let _ = writeln!(
            err,
            "fw hub: no hub is running for state dir {} (`fw hub start` starts one)",
            dir.display()
        );
        return Ok(crate::lifecycle::EXIT_NOT_RUNNING);
    };
    let binary = upgrade_binary(args.binary.as_deref())?;
    let state_args = vec!["--state-dir".to_owned(), dir.to_string_lossy().into_owned()];
    let (serve_args, impossible) = match handoff::describe(&dir) {
        Ok(description) => {
            let reason = description.unsupported.clone();
            (description.serve_args, reason)
        }
        Err(HandoffError::NoHub(_)) => (
            state_args.clone(),
            Some(format!(
                "the running hub (pid {}) predates live handoff",
                old.pid
            )),
        ),
        Err(e) => return Err(format!("cannot ask the running hub: {e}")),
    };
    if let Some(reason) = impossible {
        return restart_or_refuse(args, &binary, &dir, &serve_args, &reason, out, err);
    }
    let _ = writeln!(
        out,
        "handing the hub (pid {}) off to {} ...",
        old.pid,
        binary.display()
    );
    let output = Command::new(&binary)
        .args(["hub", "start", "--handoff", "--timeout"])
        .arg(args.timeout.to_string())
        .args(&serve_args)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", binary.display()))?;
    let _ = out.write_all(&output.stdout);
    let _ = err.write_all(&output.stderr);
    match output.status.code() {
        Some(0) => Ok(0),
        Some(EXIT_REFUSED) => restart_or_refuse(
            args,
            &binary,
            &dir,
            &serve_args,
            "the running hub refused the handoff (see above)",
            out,
            err,
        ),
        // clap's usage error: a binary from before `start --handoff`.
        Some(2) => restart_or_refuse(
            args,
            &binary,
            &dir,
            &serve_args,
            &format!("{} does not support live handoff", binary.display()),
            out,
            err,
        ),
        _ => Ok(1),
    }
}

/// Fall back to stop + start under `--allow-restart`; otherwise refuse
/// (exit [`EXIT_REFUSED`]: nothing changed).
fn restart_or_refuse(
    args: &UpgradeArgs,
    binary: &Path,
    dir: &Path,
    serve_args: &[String],
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
        "no live handoff ({reason}); restarting the hub instead (sessions restored, processes restarted)"
    );
    let mut run = |verb: &[&str], extra: &[String]| -> Result<i32, String> {
        let output = Command::new(binary)
            .arg("hub")
            .args(verb)
            .args(extra)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", binary.display()))?;
        let _ = out.write_all(&output.stdout);
        let _ = err.write_all(&output.stderr);
        Ok(output.status.code().unwrap_or(1))
    };
    let stop_args = vec!["--state-dir".to_owned(), dir.to_string_lossy().into_owned()];
    let stopped = run(&["stop"], &stop_args)?;
    if stopped != 0 {
        return Ok(stopped);
    }
    let mut start_args = vec!["--timeout".to_owned(), args.timeout.to_string()];
    start_args.extend(serve_args.iter().cloned());
    run(&["start"], &start_args)
}
