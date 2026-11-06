//! `fw hub start | stop | status | logs | install-service`: running the
//! hub as a background daemon on this machine.

use crate::cli::{
    HubArgs, Qr, StateDirArg, browse_host, mint_remote_code, resolve_state_dir, running_hub,
    show_login,
};
use crate::daemon::{self, LOG_FILE};
use crate::endpoint::Endpoint;
use crate::remote::{RuntimeInfo, ServeLock};
use crate::util::fail;
use chrono::Utc;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use crate::endpoint::EXIT_NOT_RUNNING;
/// How long `status` (a health probe) and `start`'s report wait for the hub
/// to answer: a hub that is alive but wedged must not stall them.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The state dir, absolute (the daemon runs from `/`) and created 0700.
fn prepare_state_dir(state: &StateDirArg) -> Result<PathBuf, String> {
    let dir = resolve_state_dir(state)?;
    let dir = std::path::absolute(&dir).map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    crate::paths::ensure_private_dir(&dir)
        .map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The URL a browser on this machine opens.
fn local_url(info: &RuntimeInfo) -> String {
    format!("http://{}", browse_host(info.listen))
}

/// `3d 4h`, `2h 5m`, `4m 10s`, `9s`.
pub fn human_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    let (d, h, m, sec) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60, s % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {sec}s")
    } else {
        format!("{sec}s")
    }
}

// --- start ------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct StartArgs {
    #[command(flatten)]
    hub: HubArgs,
    /// Seconds to wait for the hub to be serving.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    timeout: u64,
    /// Take over from the running hub without ending its sessions (live
    /// handoff to this binary; `fw hub upgrade` runs this).
    #[arg(long)]
    handoff: bool,
}

pub(crate) fn start(args: &StartArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match start_daemon(args, out, err) {
        Ok(code) => code,
        Err(message) => fail(err, &message),
    }
}

fn start_daemon(args: &StartArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    if args.handoff {
        return Ok(crate::upgrade::start_handoff(
            &args.hub,
            Duration::from_secs(args.timeout),
            out,
            err,
        ));
    }
    // Refuse bad options here, not in a log nobody is reading.
    args.hub.public_url_origins()?;
    crate::cli::notice_legacy_state(&args.hub.state, err);
    let dir = prepare_state_dir(&args.hub.state)?;
    if let Some(info) = running_hub(&dir)? {
        let _ = writeln!(
            out,
            "flywheel hub is already running (pid {}) at {}",
            info.pid,
            local_url(&info)
        );
        return Ok(0);
    }
    let log = dir.join(LOG_FILE);
    let offset = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this program: {e}"))?;
    let mut argv = vec!["hub".to_owned(), "serve".to_owned()];
    argv.extend(args.hub.to_argv(&dir));
    argv.extend(["--log-file".to_owned(), log.to_string_lossy().into_owned()]);
    let mut child = daemon::spawn_detached(&exe, &argv, &log)
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let pid = child.id();
    let mut exited = None;
    let ready = daemon::wait_until(Duration::from_secs(args.timeout), || {
        if let Ok(Some(status)) = child.try_wait() {
            exited = Some(status);
            return true;
        }
        // Only the child writes a runtime file carrying its pid, and only
        // once it holds the serve lock and listens.
        matches!(RuntimeInfo::read(&dir), Ok(Some(info)) if info.pid == pid)
    });
    let output = || {
        daemon::read_from(&log, offset)
            .map(|text| daemon::last_lines(&text, 20).trim_end().to_owned())
            .unwrap_or_default()
    };
    if let Some(status) = exited {
        return Err(format!(
            "the hub exited during startup ({status}); its log ({}):\n{}",
            log.display(),
            output()
        ));
    }
    if !ready {
        #[cfg(unix)]
        let _ = daemon::send_signal(pid, libc::SIGTERM);
        return Err(format!(
            "the hub (pid {pid}) was not serving within {}s, so it was stopped; its log ({}):\n{}",
            args.timeout,
            log.display(),
            output()
        ));
    }
    let info = RuntimeInfo::read(&dir)
        .ok()
        .flatten()
        .ok_or_else(|| "the hub's runtime file vanished".to_owned())?;
    let _ = writeln!(
        out,
        "flywheel hub started (pid {pid}) at {}",
        local_url(&info)
    );
    let _ = writeln!(out, "state dir: {}", dir.display());
    let _ = writeln!(out, "log: {} (`fw hub logs -f`)", log.display());
    // The daemon's startup warnings (a non-loopback bind, from a flag or
    // hub.toml) belong on this terminal too, not only in its log.
    for line in daemon::read_from(&log, offset)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("fw hub: WARNING:"))
    {
        let _ = writeln!(err, "{line}");
    }
    // A fresh hub runs exactly the sessions it restored.
    if let Ok(sessions) =
        Endpoint::local(&dir).and_then(|c| c.with_timeout(PROBE_TIMEOUT).blocking_sessions())
    {
        report_sessions(&sessions, out, err);
    }
    match mint_remote_code(&dir) {
        Ok((info, code)) => show_login(&info, &code, Qr::Auto, out, err),
        Err(message) => {
            let _ = writeln!(err, "fw hub: cannot mint a login code: {message}");
        }
    }
    Ok(0)
}

/// What a freshly started hub restored, and what it did not (with ids:
/// names need not be unique).
fn report_sessions(sessions: &[Value], out: &mut dyn Write, err: &mut dyn Write) {
    let name = |s: &Value| {
        s.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let id = |s: &Value| {
        let id = s.get("id").and_then(Value::as_str).unwrap_or("");
        id.get(..crate::script::SHORT_ID).unwrap_or(id).to_owned()
    };
    let restored: Vec<String> = sessions
        .iter()
        .filter(|s| s.get("state").and_then(Value::as_str) == Some("running"))
        .map(|s| format!("{:?}", name(s)))
        .collect();
    if !restored.is_empty() {
        let _ = writeln!(
            out,
            "restored {} session{}: {}",
            restored.len(),
            if restored.len() == 1 { "" } else { "s" },
            restored.join(", ")
        );
    }
    for session in sessions {
        if let Some(error) = crate::script::restore_error(session) {
            let _ = writeln!(
                err,
                "fw hub: session {:?} ({}) not restored: {error}; `fw hub relaunch {}` starts it",
                name(session),
                id(session),
                id(session)
            );
        }
    }
}

// --- stop -------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct StopArgs {
    /// Seconds to wait for a graceful stop.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    timeout: u64,
    /// SIGKILL the hub if it has not stopped within --timeout (its
    /// sessions are then not recorded for restore).
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    state: StateDirArg,
}

/// True once nothing holds the serve lock (the hub exited).
fn lock_free(dir: &Path) -> bool {
    matches!(ServeLock::acquire(dir), Ok(Some(_)))
}

pub(crate) fn stop(args: &StopArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let dir = match resolve_state_dir(&args.state) {
        Ok(dir) => dir,
        Err(message) => return fail(err, &message),
    };
    let info = match running_hub(&dir) {
        Ok(Some(info)) => info,
        Ok(None) => return crate::endpoint::not_running(&dir, err),
        Err(message) => return fail(err, &message),
    };
    stop_running(&dir, &info, args, out, err)
}

#[cfg(unix)]
fn stop_running(
    dir: &Path,
    info: &RuntimeInfo,
    args: &StopArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // Nothing is asked of the hub first: a wedged one would hold this up
    // past --timeout. What it recorded is read back once it has exited.
    let signalled_at = Utc::now() - chrono::Duration::seconds(1);
    if let Err(e) = daemon::send_signal(info.pid, libc::SIGTERM) {
        return fail(
            err,
            &format!("cannot signal the hub (pid {}): {e}", info.pid),
        );
    }
    let timeout = Duration::from_secs(args.timeout);
    if daemon::wait_until(timeout, || lock_free(dir)) {
        let recorded = match recorded_for_restore(dir, signalled_at) {
            0 => String::new(),
            1 => "; 1 running session recorded for restore".to_owned(),
            n => format!("; {n} running sessions recorded for restore"),
        };
        let _ = writeln!(out, "flywheel hub stopped (pid {}){recorded}", info.pid);
        return 0;
    }
    if !args.force {
        return fail(
            err,
            &format!(
                "the hub (pid {}) did not stop within {}s; `fw hub stop --force` kills it",
                info.pid, args.timeout
            ),
        );
    }
    let _ = daemon::send_signal(info.pid, libc::SIGKILL);
    if daemon::wait_until(Duration::from_secs(5), || lock_free(dir)) {
        let _ = writeln!(
            out,
            "flywheel hub killed (pid {}); its sessions were not recorded for restore",
            info.pid
        );
        0
    } else {
        fail(err, &format!("the hub (pid {}) survived SIGKILL", info.pid))
    }
}

/// Sessions a hub that just exited recorded stopped by its clean shutdown
/// (at or after `since`): what its next start restores.
#[cfg(unix)]
fn recorded_for_restore(dir: &Path, since: chrono::DateTime<Utc>) -> usize {
    let Ok(bytes) = std::fs::read(dir.join(crate::state::STATE_FILE)) else {
        return 0;
    };
    let Ok(doc) = serde_json::from_slice::<Value>(&bytes) else {
        return 0;
    };
    doc.get("sessions")
        .and_then(Value::as_array)
        .map_or(0, |sessions| {
            sessions
                .iter()
                .filter(|s| {
                    s.get("state").and_then(Value::as_str) == Some("stopped")
                        && s.get("clean").and_then(Value::as_bool) == Some(true)
                        && s.get("at")
                            .and_then(Value::as_str)
                            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                            .is_some_and(|at| at >= since)
                })
                .count()
        })
}

#[cfg(not(unix))]
fn stop_running(
    _dir: &Path,
    _info: &RuntimeInfo,
    _args: &StopArgs,
    _out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    fail(err, "`fw hub stop` is supported on unix only")
}

// --- status -----------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct StatusArgs {
    /// Print JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    state: StateDirArg,
}

/// Count sessions by their state label (running / exited / stopped / not
/// restored / gone).
fn session_counts(sessions: &[Value]) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for session in sessions {
        let label = crate::script::state_label(session);
        let key = if label == "not restored" {
            label
        } else {
            label.split([' ', '(']).next().unwrap_or("").to_owned()
        };
        match counts.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => counts.push((key, 1)),
        }
    }
    counts
}

pub(crate) fn status(args: &StatusArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let dir = match resolve_state_dir(&args.state) {
        Ok(dir) => dir,
        Err(message) => return fail(err, &message),
    };
    let info = match running_hub(&dir) {
        Ok(Some(info)) => info,
        Ok(None) => {
            if args.json {
                let _ = writeln!(out, "{}", json!({ "running": false, "state_dir": dir }));
                return EXIT_NOT_RUNNING;
            }
            return crate::endpoint::not_running(&dir, out);
        }
        Err(message) => return fail(err, &message),
    };
    let uptime = info
        .started_at
        .map(|at| (Utc::now() - at).num_seconds().max(0));
    let listed = Endpoint::local(&dir)
        .map(|c| c.with_timeout(PROBE_TIMEOUT))
        .and_then(|c| Ok((c.blocking_workspaces()?, c.blocking_sessions()?)));
    let (workspaces, sessions) = match listed {
        Ok(lists) => lists,
        Err(message) => {
            let _ = writeln!(
                out,
                "flywheel hub is running (pid {}) at {} but not answering",
                info.pid,
                local_url(&info)
            );
            return fail(err, &message);
        }
    };
    let counts = session_counts(&sessions);
    // Which build serves (it changes under `fw hub upgrade`).
    let build = crate::handoff::describe_within(&dir, PROBE_TIMEOUT).ok();
    let version = build.as_ref().map(|d| d.version.clone());
    let binary = build.and_then(|d| d.binary);
    if args.json {
        let by_state: serde_json::Map<String, Value> =
            counts.iter().map(|(k, n)| (k.clone(), json!(n))).collect();
        let body = json!({
            "running": true,
            "pid": info.pid,
            "version": version,
            "binary": binary,
            "url": local_url(&info),
            "listen": info.listen.to_string(),
            "public_urls": info.public_urls,
            "started_at": info.started_at,
            "uptime_seconds": uptime,
            "state_dir": dir,
            "log_file": info.log_file,
            "workspaces": workspaces.len(),
            "sessions": sessions.len(),
            "sessions_by_state": by_state,
        });
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
        return 0;
    }
    let up = uptime.map_or_else(|| "unknown".to_owned(), human_duration);
    let _ = writeln!(out, "flywheel hub is running (pid {}, up {up})", info.pid);
    if let Some(version) = &version {
        let at = binary
            .as_ref()
            .map(|b| format!(" ({})", b.display()))
            .unwrap_or_default();
        let _ = writeln!(out, "  version:    flywheel {version}{at}");
    }
    let _ = writeln!(out, "  url:        {}", local_url(&info));
    for url in &info.public_urls {
        let _ = writeln!(out, "  public:     {url}");
    }
    let _ = writeln!(out, "  state dir:  {}", dir.display());
    match &info.log_file {
        Some(log) => {
            let _ = writeln!(out, "  log:        {}", log.display());
        }
        None => {
            let _ = writeln!(out, "  log:        (foreground: its terminal or journal)");
        }
    }
    let _ = writeln!(out, "  workspaces: {}", workspaces.len());
    let detail = if counts.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            counts
                .iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let _ = writeln!(out, "  sessions:   {}{detail}", sessions.len());
    0
}

// --- logs -------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct LogsArgs {
    /// Keep printing what is appended (across rotations) until interrupted.
    #[arg(short, long)]
    follow: bool,
    /// Lines from the end to print first (0: the whole file).
    #[arg(short = 'n', long, value_name = "N", default_value_t = 100)]
    lines: usize,
    #[command(flatten)]
    state: StateDirArg,
}

pub(crate) fn logs(args: &LogsArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let dir = match resolve_state_dir(&args.state) {
        Ok(dir) => dir,
        Err(message) => return fail(err, &message),
    };
    let path = dir.join(LOG_FILE);
    let text = match daemon::read_from(&path, 0) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !args.follow => {
            return fail(
                err,
                &format!(
                    "no log at {} (it is written by a hub started with `fw hub start`)",
                    path.display()
                ),
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return fail(err, &format!("{}: {e}", path.display())),
    };
    let _ = out.write_all(daemon::last_lines(&text, args.lines).as_bytes());
    let _ = out.flush();
    if !args.follow {
        return 0;
    }
    let offset = u64::try_from(text.len()).unwrap_or(0);
    match daemon::follow(&path, offset, out) {
        Err(e) => fail(err, &format!("{}: {e}", path.display())),
        Ok(()) => 0,
    }
}

// --- install-service --------------------------------------------------------

#[derive(Debug, clap::Args)]
pub(crate) struct InstallServiceArgs {
    #[command(flatten)]
    hub: HubArgs,
    /// Unit name (written as NAME.service).
    #[arg(long, default_value = "flywheel-hub")]
    name: String,
    /// Replace an existing unit file.
    #[arg(long)]
    force: bool,
    /// Only print the unit; write nothing.
    #[arg(long)]
    print: bool,
}

pub(crate) fn install_service(
    args: &InstallServiceArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match write_service(args, out) {
        Ok(()) => 0,
        Err(message) => fail(err, &message),
    }
}

fn write_service(args: &InstallServiceArgs, out: &mut dyn Write) -> Result<(), String> {
    args.hub.public_url_origins()?;
    if args.name.is_empty()
        || !args
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
    {
        return Err(format!(
            "--name {:?}: use letters, digits, - _ . @",
            args.name
        ));
    }
    let dir = prepare_state_dir(&args.hub.state)?;
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("cannot locate this program: {e}"))?;
    let mut argv = vec![
        exe.to_string_lossy().into_owned(),
        "hub".to_owned(),
        "serve".to_owned(),
    ];
    argv.extend(args.hub.to_argv(&dir));
    // Its output goes to the journal: no one-time login code there.
    argv.push("--no-login-code".to_owned());
    // User services start with a minimal environment: carry over what
    // sessions need to find the shell, the agent CLIs, and the flywheel
    // home the `fw` verbs they run resolve.
    let env: Vec<(String, String)> = ["PATH", "SHELL", "FLYWHEEL_HOME"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (name.to_owned(), v))
        })
        .collect();
    let unit = daemon::systemd_unit(&argv, &env);
    if args.print {
        let _ = out.write_all(unit.as_bytes());
        return Ok(());
    }
    let unit_dir = daemon::systemd_user_dir_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .ok_or_else(|| "cannot resolve the systemd user unit dir: set HOME".to_owned())?;
    let path = unit_dir.join(format!("{}.service", args.name));
    if path.exists() && !args.force {
        return Err(format!(
            "{} already exists (--force replaces it, --print shows the new one)",
            path.display()
        ));
    }
    std::fs::create_dir_all(&unit_dir).map_err(|e| format!("{}: {e}", unit_dir.display()))?;
    std::fs::write(&path, &unit).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = &args.name;
    let _ = writeln!(out, "wrote {}:\n", path.display());
    let _ = out.write_all(unit.as_bytes());
    let _ = writeln!(
        out,
        "\nnot enabled. To run the hub as a user service:\n  \
         systemctl --user daemon-reload\n  \
         systemctl --user enable --now {name}\n  \
         loginctl enable-linger \"$USER\"   # optional: start at boot, keep running after logout\n\
         Manage it with systemctl --user (stop/restart/status) and journalctl --user -u {name};\n\
         log in with a one-time URL from `fw hub url` (none is written to the journal);\n\
         `fw hub status` and the scripting verbs work against it as usual."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(9), "9s");
        assert_eq!(human_duration(250), "4m 10s");
        assert_eq!(human_duration(2 * 3600 + 5 * 60 + 3), "2h 5m");
        assert_eq!(human_duration(3 * 86_400 + 4 * 3600), "3d 4h");
        assert_eq!(human_duration(-5), "0s");
    }

    #[test]
    fn session_counts_group_by_state() {
        let sessions = vec![
            json!({"state": "running", "live": {}}),
            json!({"state": "running", "live": {}}),
            json!({"state": "exited", "code": 1}),
            json!({"state": "stopped", "restore_error": "x"}),
        ];
        assert_eq!(
            session_counts(&sessions),
            vec![
                ("running".to_owned(), 2),
                ("exited".to_owned(), 1),
                ("not restored".to_owned(), 1)
            ]
        );
    }
}
