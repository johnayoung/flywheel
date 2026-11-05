//! `fw hub doctor`: check what a hub on this machine depends on and say
//! how to fix what is off. Read-only: it creates, removes, and rewrites
//! nothing (a port is bound for a moment to see that it is free, and
//! programs are asked for `--version` / `--help`; scripts found on `PATH`
//! are read, never run).

use crate::agent::{self, SessionKind};
use crate::cli::{DEFAULT_PORT, StateDirArg, resolve_state_dir};
use serde::Serialize;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::time::Duration;

mod agents;
mod files;
mod machine;
mod path;
mod running;

#[cfg(test)]
use crate::review::runner;
pub use agents::hooks_shape;
use agents::{agent_check, hooks_check};
pub use files::{config_checks, state_dir_checks};
use machine::{browser_check, clock_check, systemd_checks};
pub use machine::{disk_check, ntp_synchronized, recordings_check, systemd_state};
#[cfg(test)]
use path::on_path_check_with;
pub use path::{
    Found, classify, executables_named, installed_together, on_path_check, parse_version,
};
#[cfg(test)]
use running::hub_holds_lock;
pub use running::{Running, fdinfo_has_flock};
use running::{hub_check, running_hub};

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
    checks.push(hub_check(&hub, exe, dir));
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
        write_script(&venv.join("fw"), "#!/usr/bin/python3\nprint('hi')\n");
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
        // A shell script is reported the same way.
        write_script(&venv.join("fw"), "#!/bin/bash\necho hi\n");
        let check = on_path_check("fw", &found, Some(&exe));
        assert_eq!(check.level, Level::Fail);
        assert!(
            check.summary.contains("a script run by /bin/bash"),
            "{}",
            check.summary
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
        // A free port is ok. Probed below the ephemeral range: a port
        // `bind(:0)` handed out and this test freed is at once reusable by
        // any parallel test's outgoing connection (the kernel picks those
        // from the same range), so it is not reliably free a moment later.
        // Nothing in the process auto-assigns these; one skipped here is
        // held by a long-lived listener on this machine.
        let free = unassigned_ports()
            .map(|port| port_check(IpAddr::V4(Ipv4Addr::LOCALHOST), port, None))
            .find(|check| check.level == Level::Ok)
            .expect("a free port below the ephemeral range");
        assert!(free.summary.ends_with("is free"), "{free:?}");
        assert!(free.fix.is_none(), "{free:?}");
    }

    /// Ports the kernel never hands out on its own (not to `bind(:0)`, not
    /// to an outgoing connection): the 2000 just below the ephemeral range
    /// (Linux reads it; elsewhere it starts at 32768 or above).
    fn unassigned_ports() -> std::ops::Range<u16> {
        let start = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .ok()
            .and_then(|range| range.split_whitespace().next()?.parse::<u16>().ok())
            .unwrap_or(32768)
            .clamp(4096, 32768);
        (start - 2000)..start
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
