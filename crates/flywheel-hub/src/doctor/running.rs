//! The running hub: read the state dir's serve lock without taking it, and
//! whether that hub is this binary.

use super::{Check, Level};
use std::ffi::{OsStr, OsString};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
pub(crate) fn running_hub(dir: &Path) -> Option<Running> {
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
pub(crate) fn hub_holds_lock(pid: u32, path: &Path) -> Option<bool> {
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

pub(crate) fn hub_check(hub: &Option<Running>, exe: Option<&Path>, dir: &Path) -> Check {
    let id = "hub";
    let Some(hub) = hub else {
        return Check::new(id, Level::Info, crate::endpoint::no_hub_message(dir))
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
