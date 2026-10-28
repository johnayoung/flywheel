//! Process-wide registry of live child process groups.
//!
//! Agents ([`crate::hosts`]) and command graders (flywheel core) each run in
//! a process group of their own, so a signal aimed at the owning daemon's
//! group never reaches them. Every such group is registered here for as
//! long as its leader is unreaped; a daemon's shutdown path calls
//! [`terminate_all`] (or [`terminate_all_blocking`] on the way out of
//! `main`) to bring every one of them down instead of orphaning them.
//!
//! A [`GroupRegistration`] is the RAII handle: [`GroupRegistration::release`]
//! once the leader has been reaped; dropping a registration that was never
//! released SIGKILLs the group, so a dropped run future anywhere cannot
//! leave its process tree running.
//!
//! The registry is global to the process. Signalling it from a test binary
//! reaches every group any concurrently running test registered, so
//! [`terminate_all`] is exercised from a dedicated test binary.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How often the termination sweeps re-probe for surviving groups.
const SWEEP_POLL: Duration = Duration::from_millis(50);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static REGISTRY: Mutex<BTreeMap<u64, i32>> = Mutex::new(BTreeMap::new());

fn registry() -> std::sync::MutexGuard<'static, BTreeMap<u64, i32>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A registered live process group. See the module docs.
#[derive(Debug)]
pub struct GroupRegistration {
    id: u64,
    pgid: i32,
    active: bool,
}

impl GroupRegistration {
    /// Register `pgid` (the pid of a child spawned as its own group leader).
    pub fn register(pgid: i32) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        registry().insert(id, pgid);
        Self {
            id,
            pgid,
            active: true,
        }
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// Whether the group is still registered (not yet released).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Unregister without signalling: the owner reaped the leader.
    /// Idempotent.
    pub fn release(&mut self) {
        if self.active {
            self.active = false;
            registry().remove(&self.id);
        }
    }
}

impl Drop for GroupRegistration {
    fn drop(&mut self) {
        if self.active {
            // The leader was never reaped, so its pid still pins the group
            // id: no unrelated group can be targeted.
            signal_group(self.pgid, Signal::Kill);
            self.release();
        }
    }
}

/// The signals the sweeps send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// Send `signal` to process group `pgid`; `false` when no member is left
/// (or the platform has no process groups).
#[cfg(unix)]
pub fn signal_group(pgid: i32, signal: Signal) -> bool {
    if pgid <= 1 {
        return false;
    }
    let signum = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: killpg is a plain syscall wrapper with no memory-safety
    // preconditions; a stale pgid yields ESRCH, reported as `false`.
    unsafe { libc::killpg(pgid, signum) == 0 }
}

#[cfg(not(unix))]
pub fn signal_group(_pgid: i32, _signal: Signal) -> bool {
    false
}

/// Whether any member of group `pgid` still exists (an unreaped zombie
/// leader counts).
#[cfg(unix)]
pub fn group_alive(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

#[cfg(not(unix))]
pub fn group_alive(_pgid: i32) -> bool {
    false
}

/// Every currently registered process group id.
pub fn live_groups() -> Vec<i32> {
    let mut groups: Vec<i32> = registry().values().copied().collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

fn signal_all(groups: &[i32], signal: Signal) {
    for pgid in groups {
        signal_group(*pgid, signal);
    }
}

/// Registered groups that still have a member.
fn survivors() -> Vec<i32> {
    live_groups()
        .into_iter()
        .filter(|pgid| group_alive(*pgid))
        .collect()
}

/// SIGTERM every registered group, give them `grace` to exit, then
/// SIGKILL whatever is still registered. Returns the number of groups that
/// were signalled.
pub async fn terminate_all(grace: Duration) -> usize {
    let groups = live_groups();
    if groups.is_empty() {
        return 0;
    }
    signal_all(&groups, Signal::Term);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && !survivors().is_empty() {
        tokio::time::sleep(SWEEP_POLL).await;
    }
    signal_all(&live_groups(), Signal::Kill);
    groups.len()
}

/// [`terminate_all`] for a caller with no async runtime (the end of
/// `main`).
pub fn terminate_all_blocking(grace: Duration) -> usize {
    let groups = live_groups();
    if groups.is_empty() {
        return 0;
    }
    signal_all(&groups, Signal::Term);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && !survivors().is_empty() {
        std::thread::sleep(SWEEP_POLL);
    }
    signal_all(&live_groups(), Signal::Kill);
    groups.len()
}

/// Start [`terminate_all`] with the agent kill grace on the current tokio
/// runtime and return at once: what a daemon's SIGTERM/SIGINT handler
/// calls so its agents and graders, each in a group of their own, stop
/// with it instead of being orphaned when the daemon exits.
pub fn spawn_shutdown_sweep() -> tokio::task::JoinHandle<usize> {
    tokio::spawn(terminate_all(crate::hosts::DEFAULT_KILL_GRACE))
}

/// Wait until every registered group has been released by its owner, up to
/// `timeout`, without signalling anything. `true` when the registry
/// drained. Lets a caller about to drop its async runtime give in-flight
/// teardowns (SIGTERM, grace, SIGKILL, reap) the chance to finish.
pub async fn wait_released(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if registry().is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(SWEEP_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_unregisters_without_signalling() {
        // pgid 1 is never signalled (guarded), so this is hermetic.
        let mut registration = GroupRegistration::register(1);
        assert!(registration.is_active());
        assert!(registry().values().any(|pgid| *pgid == 1));
        registration.release();
        registration.release();
        assert!(!registration.is_active());
        drop(registration);
    }

    #[test]
    fn guarded_pgids_are_never_signalled() {
        assert!(!signal_group(0, Signal::Term));
        assert!(!signal_group(1, Signal::Kill));
        assert!(!group_alive(0));
    }
}
