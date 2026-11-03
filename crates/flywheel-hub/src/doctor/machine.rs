//! This machine: a browser opener, the systemd unit, the clock, free disk
//! and the recordings' usage.

use super::agents::{bounded, first_line};
use super::{Check, Level};
use crate::agent::{self};
use crate::review::runner::{self};
use std::path::Path;

pub(crate) fn browser_check() -> Check {
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

pub(crate) fn systemd_checks() -> Vec<Check> {
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

pub(crate) fn clock_check() -> Check {
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
