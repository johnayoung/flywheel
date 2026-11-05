//! Helpers shared by the integration tests (each test binary uses a
//! subset).
#![allow(dead_code)]

mod script;

#[allow(unused_imports)] // tests/remote.rs runs no stand-in scripts
pub use script::write_script;

use flywheel_hub::{Hub, HubError, HubOptions};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `Hub::open`, waiting out a serve lock still held for a hub just shut
/// down on the same state dir. Its lock outlives it while any child
/// another test thread forked meanwhile (PTY spawns) has not yet exec'd:
/// the child inherited the lock's descriptor (close-on-exec only closes
/// it at exec), and a flock belongs to the open file, not the descriptor.
pub fn open_hub(options: HubOptions) -> Arc<Hub> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Hub::open(options.clone()) {
            Err(HubError::InUse(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result.unwrap(),
        }
    }
}

/// Whether `pid`, a child of this test process (the hub runs in it, so
/// the hub's own children are its to reap), has been reaped: `kill` says
/// no such process. A zombie still counts as alive -- a hub that killed a
/// child and never waited for it leaked it.
pub fn child_reaped(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only probes for existence.
    let probed = unsafe { libc::kill(pid, 0) };
    probed != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Whether `pid`, a process this test spawned only indirectly (a plugin's
/// background child), has exited: gone, or a zombie awaiting a reaper that
/// is not this process. Such a process is reparented when its parent dies,
/// and whether that reaper waits for it is the environment's business, not
/// the hub's: in a container whose PID 1 is `cargo`, it never does, and the
/// exited process stays a zombie for good. Only positive evidence counts:
/// without `/proc` (macOS) a process `kill` still finds is alive.
pub fn orphan_exited(pid: i32) -> bool {
    if child_reaped(pid) {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        // `/proc/<pid>/stat`: `pid (comm) state ppid ...`; comm may hold
        // spaces and parentheses, so split after the last `)`.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let mut fields = stat.rsplit(')').next().unwrap_or("").split_whitespace();
                let state = fields.next();
                let ppid: Option<u32> = fields.next().and_then(|p| p.parse().ok());
                matches!(state, Some("Z" | "X" | "x")) && ppid != Some(std::process::id())
            }
            Err(err) => err.kind() == std::io::ErrorKind::NotFound,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}
