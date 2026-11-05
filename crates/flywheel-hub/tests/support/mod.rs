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

/// Whether `pid` has exited: gone, or a zombie awaiting its reaper. Only
/// positive evidence counts as dead: `kill` saying no such process, its
/// `/proc` entry gone, or a zombie/dead state. A process a test spawned
/// indirectly (a plugin's background child) is reparented when its parent
/// dies, and whether that reaper waits for it is the environment's
/// business, not the hub's: in a container whose PID 1 is `cargo`, it
/// never is, and the exited process stays a zombie for good.
pub fn process_dead(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only probes for existence.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| matches!(state, "Z" | "X" | "x")),
        Err(err) => err.kind() == std::io::ErrorKind::NotFound,
    }
}
