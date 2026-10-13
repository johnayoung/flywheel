//! The worker pool supervisor (spec 00060, D-5): one `flywheel worker`
//! invocation with concurrency > 1 supervises N single-task worker
//! subprocesses.
//!
//! Each pool member is its own OS process in its own session, running
//! `flywheel worker --concurrency 1` with a distinct `--worker-id` against
//! the one shared store. Per-task leases keep members off the same task and
//! the repo merge-flock serializes their landings, so `size` members give
//! up to `size` tasks executing at once and never more. Surplus members
//! find nothing claimable and idle -- or, in `--once` mode, drain and exit
//! 0 -- without double-claiming.
//!
//! Crash policy is restart-and-reclaim: a member that exits non-zero is
//! respawned so the live pool returns to `size`, and its in-flight task's
//! lease lapses (or, on a same-worker-id respawn, is reclaimed immediately)
//! so a live member finishes it exactly once. Per-slot restarts are bounded
//! by a *windowed* crash-loop budget (spec 00070): a slot that exhausts its
//! budget is retired in isolation while every other live member keeps
//! running -- one bad slot never group-kills the healthy fleet.
//!
//! Shutdown reuses the group-kill supervisor model (commit 36a0622): each
//! member is a session leader whose agent/MCP children share its process
//! group, so [`WorkerPool::stop`] signals every member's whole group
//! (SIGTERM, then SIGKILL on the survivors after the stop window) and
//! returns with nothing left alive.

use crate::daemon::WorkerArgs;
use crate::submitter::Logger;
use chrono::Utc;
use flywheel_core::task::python_float_repr;
use flywheel_orchestrator::supervision_policy::{
    RespawnDecision, SupervisionBudget, SupervisionBudgetError, SupervisionPolicy,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

// --- Constants ----------------------------------------------------------------------------

/// How long the pool's group shutdown gives each member's SIGTERM window
/// before escalating to SIGKILL (mirrors the console worker supervisor).
pub const DEFAULT_POOL_STOP_TIMEOUT_SECONDS: f64 = 10.0;
/// How often the supervise loop polls members for exit: short so a stop
/// signal or a member crash is noticed promptly without busy-spinning.
pub const POOL_SUPERVISE_POLL_SECONDS: f64 = 0.2;
/// Per-slot windowed crash-loop budget (spec 00070): a member that keeps
/// crashing is respawned at most this many times within a rolling window;
/// the next death inside the window retires that one slot in isolation.
pub const DEFAULT_POOL_RESTARTS_PER_SLOT: i64 = 5;
/// The rolling window the per-slot budget is measured over.
pub const DEFAULT_POOL_RESTART_WINDOW_SECONDS: f64 = 300.0;
/// How much of a dead member's captured output is read back for the death
/// log (bytes from the tail of its log file).
pub const DEATH_LOG_TAIL_BYTES: u64 = 4096;
/// How many trailing output lines a death log reports.
pub const DEATH_LOG_TAIL_LINES: usize = 8;

/// Maps a member's worker id to the argv that launches it, keeping the
/// production worker entrypoint and a test substitute (a trivial child) on
/// the same supervision path.
pub type SpawnMember = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

/// A monotonic clock reading in seconds (the crash-loop window's time
/// source; injected so tests drive it deterministically).
pub type PoolClock = Arc<dyn Fn() -> f64 + Send + Sync>;

// --- Errors / exits ------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("worker pool size must be >= 1, got {0}")]
    InvalidSize(i64),
    #[error(transparent)]
    Budget(#[from] SupervisionBudgetError),
    #[error("cannot spawn pool member {worker_id}: {source}")]
    Spawn {
        worker_id: String,
        #[source]
        source: std::io::Error,
    },
}

/// How a member died, rendered the way the Python supervisor printed
/// `Popen.returncode`: the exit code, or the negated signal number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl MemberExit {
    fn of(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
        }
    }

    /// Whether the member drained cleanly (exit 0).
    pub fn is_clean(&self) -> bool {
        self.code == Some(0)
    }
}

impl fmt::Display for MemberExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "{code}"),
            (None, Some(signal)) => write!(f, "-{signal}"),
            (None, None) => f.write_str("?"),
        }
    }
}

// --- Members ---------------------------------------------------------------------------------

/// One supervised pool member: a single-task worker subprocess.
///
/// The child is its own session leader (`setsid`), so its agent/MCP
/// children share its process group and a group signal reaches the whole
/// subtree. `log_path` is the redirected stdout/stderr sink (`None` when
/// output is inherited), kept for forensics after the member dies.
pub struct PoolMember {
    pub slot: usize,
    pub worker_id: String,
    pub pid: u32,
    pub log_path: Option<PathBuf>,
    child: Child,
    exit: Option<ExitStatus>,
}

impl fmt::Debug for PoolMember {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolMember")
            .field("slot", &self.slot)
            .field("worker_id", &self.worker_id)
            .field("pid", &self.pid)
            .field("log_path", &self.log_path)
            .field("exited", &self.exit.is_some())
            .finish()
    }
}

impl PoolMember {
    /// The member's exit status if it has exited (reaping it), else `None`.
    fn poll(&mut self) -> Option<ExitStatus> {
        if let Some(status) = self.exit {
            return Some(status);
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit = Some(status);
                Some(status)
            }
            Ok(None) => None,
            // A wait error means the child is gone and already reaped
            // elsewhere; treat it as exited without a status.
            Err(_) => {
                let status = ExitStatus::default();
                self.exit = Some(status);
                Some(status)
            }
        }
    }

    fn is_running(&mut self) -> bool {
        self.poll().is_none()
    }

    /// Wait up to `timeout` for the member to exit; whether it did.
    fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.poll().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Everything [`WorkerPool::new`] takes; `new` fills the Python keyword
/// defaults.
#[derive(Clone)]
pub struct PoolOptions {
    pub size: i64,
    pub spawn_member: SpawnMember,
    pub log: Logger,
    pub once: bool,
    pub prefix: String,
    /// Where member stdout/stderr is redirected (`pool-<id>-<ts>.log`);
    /// `None` inherits the supervisor's.
    pub log_dir: Option<PathBuf>,
    /// The members' full environment; `None` inherits the supervisor's.
    pub env: Option<BTreeMap<String, String>>,
    pub stop_timeout: f64,
    pub poll_interval: f64,
    pub max_restarts_per_slot: i64,
    pub restart_window_seconds: f64,
    /// The crash-loop window clock; the process-monotonic clock when unset.
    pub clock: Option<PoolClock>,
}

impl fmt::Debug for PoolOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolOptions")
            .field("size", &self.size)
            .field("once", &self.once)
            .field("prefix", &self.prefix)
            .field("log_dir", &self.log_dir)
            .field("stop_timeout", &self.stop_timeout)
            .field("poll_interval", &self.poll_interval)
            .field("max_restarts_per_slot", &self.max_restarts_per_slot)
            .field("restart_window_seconds", &self.restart_window_seconds)
            .finish_non_exhaustive()
    }
}

impl PoolOptions {
    pub fn new(size: i64, spawn_member: SpawnMember, log: Logger, once: bool) -> Self {
        Self {
            size,
            spawn_member,
            log,
            once,
            prefix: "pool".to_owned(),
            log_dir: None,
            env: None,
            stop_timeout: DEFAULT_POOL_STOP_TIMEOUT_SECONDS,
            poll_interval: POOL_SUPERVISE_POLL_SECONDS,
            max_restarts_per_slot: DEFAULT_POOL_RESTARTS_PER_SLOT,
            restart_window_seconds: DEFAULT_POOL_RESTART_WINDOW_SECONDS,
            clock: None,
        }
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub fn with_log_dir(mut self, log_dir: impl Into<PathBuf>) -> Self {
        self.log_dir = Some(log_dir.into());
        self
    }

    pub fn with_poll_interval(mut self, seconds: f64) -> Self {
        self.poll_interval = seconds;
        self
    }

    pub fn with_restart_budget(mut self, max_restarts_per_slot: i64, window_seconds: f64) -> Self {
        self.max_restarts_per_slot = max_restarts_per_slot;
        self.restart_window_seconds = window_seconds;
        self
    }

    pub fn with_clock(mut self, clock: PoolClock) -> Self {
        self.clock = Some(clock);
        self
    }
}

// --- Process-wide stop signal ------------------------------------------------------------------

/// The last SIGTERM/SIGINT the supervisor received (0 = none). Written by
/// the async-signal-safe handler, read by the supervise loop.
static STOP_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn on_stop_signal(signum: libc::c_int) {
    STOP_SIGNAL.store(signum, Ordering::SeqCst);
}

/// Install the SIGTERM/SIGINT shutdown handler. The pool parent runs no
/// async runtime, so the handler is installed once and stays armed for the
/// whole invocation; it only records the signal number.
#[cfg(unix)]
fn arm_stop_signals() {
    STOP_SIGNAL.store(0, Ordering::SeqCst);
    for sig in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: installing a handler that only stores into an atomic; the
        // handler is async-signal-safe and the signal numbers are valid.
        unsafe {
            libc::signal(
                sig,
                on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

#[cfg(not(unix))]
fn arm_stop_signals() {
    STOP_SIGNAL.store(0, Ordering::SeqCst);
}

/// The pending stop signal, consuming it.
fn take_stop_signal() -> Option<i32> {
    match STOP_SIGNAL.swap(0, Ordering::SeqCst) {
        0 => None,
        signum => Some(signum),
    }
}

/// Signal a process group; `false` if it is gone.
#[cfg(unix)]
fn signal_group(pid: u32, sig: libc::c_int) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: plain libc calls on a pid this supervisor spawned; both
    // report a vanished process through their return value.
    unsafe {
        let pgid = libc::getpgid(pid);
        if pgid < 0 {
            return false;
        }
        libc::killpg(pgid, sig) == 0
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _sig: i32) -> bool {
    false
}

#[cfg(unix)]
const TERM_SIGNAL: libc::c_int = libc::SIGTERM;
#[cfg(unix)]
const KILL_SIGNAL: libc::c_int = libc::SIGKILL;
#[cfg(not(unix))]
const TERM_SIGNAL: i32 = 15;
#[cfg(not(unix))]
const KILL_SIGNAL: i32 = 9;

// --- The pool -------------------------------------------------------------------------------------

/// Supervise up to `size` single-task worker subprocesses from one
/// `flywheel worker` invocation. Construction spawns nothing; call
/// [`run_supervised`](Self::run_supervised) to drive the pool, or drive
/// [`start`](Self::start) / [`supervise_tick`](Self::supervise_tick) /
/// [`stop`](Self::stop) directly.
pub struct WorkerPool {
    size: usize,
    spawn_member: SpawnMember,
    log: Logger,
    once: bool,
    prefix: String,
    log_dir: Option<PathBuf>,
    env: Option<BTreeMap<String, String>>,
    stop_timeout: f64,
    poll_interval: f64,
    budget: SupervisionBudget,
    clock: PoolClock,
    /// One windowed crash-loop policy per slot so slots decay independently.
    policies: BTreeMap<usize, SupervisionPolicy>,
    members: BTreeMap<usize, PoolMember>,
    /// Slots no longer supervised: cleanly drained (`--once`, exit 0) or
    /// quarantined after exhausting their crash-loop budget (`exhausted`).
    retired: BTreeSet<usize>,
    exhausted: BTreeSet<usize>,
    stop_requested: bool,
    stopped: bool,
    exit_code: i32,
}

impl fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerPool")
            .field("size", &self.size)
            .field("once", &self.once)
            .field("prefix", &self.prefix)
            .field("members", &self.members.keys().collect::<Vec<_>>())
            .field("retired", &self.retired)
            .field("exhausted", &self.exhausted)
            .field("stop_requested", &self.stop_requested)
            .field("exit_code", &self.exit_code)
            .finish_non_exhaustive()
    }
}

impl WorkerPool {
    pub fn new(options: PoolOptions) -> Result<Self, PoolError> {
        if options.size < 1 {
            return Err(PoolError::InvalidSize(options.size));
        }
        let budget = SupervisionBudget::new(
            options.max_restarts_per_slot.max(0),
            options.restart_window_seconds,
        )?;
        let clock = options.clock.unwrap_or_else(|| {
            let anchor = Instant::now();
            Arc::new(move || anchor.elapsed().as_secs_f64())
        });
        Ok(Self {
            size: usize::try_from(options.size).unwrap_or(usize::MAX),
            spawn_member: options.spawn_member,
            log: options.log,
            once: options.once,
            prefix: options.prefix,
            log_dir: options.log_dir,
            env: options.env,
            stop_timeout: options.stop_timeout,
            poll_interval: options.poll_interval,
            budget,
            clock,
            policies: BTreeMap::new(),
            members: BTreeMap::new(),
            retired: BTreeSet::new(),
            exhausted: BTreeSet::new(),
            stop_requested: false,
            stopped: false,
            exit_code: 0,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Slots no longer supervised (drained or quarantined).
    pub fn retired(&self) -> &BTreeSet<usize> {
        &self.retired
    }

    /// Slots quarantined after exhausting their crash-loop budget.
    pub fn exhausted(&self) -> &BTreeSet<usize> {
        &self.exhausted
    }

    pub fn stop_requested(&self) -> bool {
        self.stop_requested
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// The pid of the member currently filling `slot`, if any.
    pub fn member_pid(&self, slot: usize) -> Option<u32> {
        self.members.get(&slot).map(|m| m.pid)
    }

    /// Spawn the pool, supervise it, and return the process exit code.
    ///
    /// Installs the SIGTERM/SIGINT shutdown handler, spawns `size` members,
    /// then loops: in `--once` mode it returns 0 once every member has
    /// drained and exited cleanly; as a daemon it runs until a stop signal.
    /// Either way every surviving member is group-killed on the way out so
    /// the invocation never leaves an orphan behind.
    pub fn run_supervised(&mut self) -> i32 {
        arm_stop_signals();
        let code = self.supervise();
        self.stop(None);
        code
    }

    fn supervise(&mut self) -> i32 {
        if let Err(err) = self.start() {
            (self.log)(&format!("pool start failed ({err}); stopping members."));
            self.exit_code = 1;
            return self.exit_code;
        }
        let poll = Duration::try_from_secs_f64(self.poll_interval.max(0.0))
            .unwrap_or(Duration::from_millis(200));
        while !self.stop_requested {
            if self.once && self.is_done() {
                break;
            }
            std::thread::sleep(poll);
            if let Some(signum) = take_stop_signal() {
                self.stop_requested = true;
                (self.log)(&format!(
                    "pool shutdown requested (signal {signum}); stopping members."
                ));
                break;
            }
            if let Err(err) = self.supervise_tick() {
                (self.log)(&format!(
                    "pool supervision failed ({err}); stopping members."
                ));
                self.exit_code = 1;
                break;
            }
        }
        self.exit_code
    }

    /// Spawn one member into every not-yet-filled, not-retired slot.
    pub fn start(&mut self) -> Result<(), PoolError> {
        for slot in 0..self.size {
            if !self.members.contains_key(&slot) && !self.retired.contains(&slot) {
                self.spawn_slot(slot)?;
            }
        }
        Ok(())
    }

    /// `--once` only: every slot has drained and exited cleanly.
    pub fn is_done(&self) -> bool {
        self.once && self.retired.len() >= self.size
    }

    /// PIDs of members currently alive.
    pub fn live_member_pids(&mut self) -> Vec<u32> {
        self.members
            .values_mut()
            .filter_map(|m| m.is_running().then_some(m.pid))
            .collect()
    }

    /// Wait up to `timeout` for the member in `slot` to exit; whether it
    /// did (`false` also when the slot is empty).
    pub fn wait_member(&mut self, slot: usize, timeout: Duration) -> bool {
        match self.members.get_mut(&slot) {
            Some(member) => member.wait_for_exit(timeout),
            None => false,
        }
    }

    /// One supervision pass: reap exited members, retire cleanly drained
    /// `--once` members, respawn crashed ones within their windowed budget,
    /// and retire (in isolation) a slot that exhausts that budget.
    ///
    /// Budget exhaustion retires only the offending slot and leaves every
    /// other live member running -- it never sets a pool-wide stop or
    /// group-kills the fleet. Once every slot is retired the pool has
    /// nothing left to supervise, so the loop is asked to stop rather than
    /// spin below-strength forever.
    pub fn supervise_tick(&mut self) -> Result<(), PoolError> {
        for slot in 0..self.size {
            if self.retired.contains(&slot) {
                continue;
            }
            let Some(member) = self.members.get_mut(&slot) else {
                continue;
            };
            let Some(status) = member.poll() else {
                continue; // still running
            };
            let exit = MemberExit::of(status);
            let worker_id = member.worker_id.clone();
            let log_path = member.log_path.clone();
            self.members.remove(&slot);
            if exit.is_clean() && self.once {
                // Clean drain: this slot is done, do not respawn (else the
                // pool would never terminate in --once mode).
                self.retired.insert(slot);
                (self.log)(&format!("pool member {worker_id} drained (exit 0)"));
                continue;
            }
            // Non-zero exit, or an unexpected daemon-mode exit: charge the
            // death against this slot's windowed crash-loop budget. The
            // dead member's task is reclaimed by a live worker via the
            // existing lease machinery.
            for line in death_log_tail(log_path.as_deref()) {
                (self.log)(&format!("pool member {worker_id} last output: {line}"));
            }
            let budget = self.budget;
            let clock = Arc::clone(&self.clock);
            let policy = self.policies.entry(slot).or_insert_with(|| {
                SupervisionPolicy::with_clock(budget, Box::new(move || clock()))
            });
            if policy.record_death() == RespawnDecision::Respawn {
                let deaths = policy.deaths_in_window();
                (self.log)(&format!(
                    "pool member {worker_id} exited {exit}; respawning (death {deaths} within {}s \
                     window, budget {})",
                    format_g(self.budget.window_seconds()),
                    self.budget.max_respawns()
                ));
                self.spawn_slot(slot)?;
                continue;
            }
            // Budget exhausted: retire THIS slot only and keep supervising
            // the rest of the fleet -- a smaller live fleet beats a dead one.
            self.retired.insert(slot);
            self.exhausted.insert(slot);
            self.exit_code = 1;
            (self.log)(&format!(
                "pool member {worker_id} exhausted its crash-loop budget ({} respawns within {}s, \
                 last exit {exit}); retiring this slot -- other members keep running",
                self.budget.max_respawns(),
                format_g(self.budget.window_seconds())
            ));
        }
        if self.retired.len() >= self.size {
            self.stop_requested = true;
        }
        Ok(())
    }

    /// Group-kill every surviving member; idempotent.
    ///
    /// Sends SIGTERM to each live member's process group first (a shared
    /// graceful window so the pool does not pay `timeout` per member),
    /// waits out the window, then escalates the survivors to SIGKILL so the
    /// pool always returns with nothing left alive -- no member, no agent,
    /// no MCP child. A force-killed member's lease simply lapses and
    /// another worker reclaims its task.
    pub fn stop(&mut self, timeout: Option<f64>) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let window = Duration::try_from_secs_f64(timeout.unwrap_or(self.stop_timeout).max(0.0))
            .unwrap_or(Duration::ZERO);
        let live: Vec<usize> = self
            .members
            .iter_mut()
            .filter_map(|(slot, m)| m.is_running().then_some(*slot))
            .collect();
        for slot in &live {
            if let Some(member) = self.members.get(slot) {
                signal_group(member.pid, TERM_SIGNAL);
            }
        }
        let deadline = Instant::now() + window;
        for slot in &live {
            if let Some(member) = self.members.get_mut(slot) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                member.wait_for_exit(remaining);
            }
        }
        for slot in &live {
            if let Some(member) = self.members.get_mut(slot)
                && member.is_running()
            {
                signal_group(member.pid, KILL_SIGNAL);
                member.wait_for_exit(window);
            }
        }
        for member in self.members.values_mut() {
            member.poll();
        }
        self.members.clear();
    }

    fn spawn_slot(&mut self, slot: usize) -> Result<(), PoolError> {
        let worker_id = format!("{}-{slot}", self.prefix);
        let argv = (self.spawn_member)(&worker_id);
        let spawn_error = |source: std::io::Error| PoolError::Spawn {
            worker_id: worker_id.clone(),
            source,
        };
        let Some((program, rest)) = argv.split_first() else {
            return Err(spawn_error(std::io::Error::other(
                "spawn_member returned an empty argv",
            )));
        };
        let (log_file, log_path) = self.open_member_log(&worker_id).map_err(spawn_error)?;
        let mut command = Command::new(program);
        command.args(rest);
        if let Some(file) = log_file {
            let stderr = file.try_clone().map_err(spawn_error)?;
            command.stdout(Stdio::from(file));
            command.stderr(Stdio::from(stderr));
        }
        if let Some(env) = &self.env {
            command.env_clear().envs(env);
        }
        // Own session so each member's agent/MCP children share its process
        // group and a group signal takes the whole subtree down; also so a
        // Ctrl+C on the console terminal does not reach members behind the
        // pool's back.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: `setsid` is async-signal-safe and touches no memory of
            // the forked parent; it only moves the child into a new session.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
        }
        let child = command.spawn().map_err(spawn_error)?;
        let pid = child.id();
        self.members.insert(
            slot,
            PoolMember {
                slot,
                worker_id: worker_id.clone(),
                pid,
                log_path,
                child,
                exit: None,
            },
        );
        (self.log)(&format!("pool member {worker_id} spawned pid={pid}"));
        Ok(())
    }

    fn open_member_log(&self, worker_id: &str) -> std::io::Result<(Option<File>, Option<PathBuf>)> {
        let Some(log_dir) = &self.log_dir else {
            return Ok((None, None));
        };
        std::fs::create_dir_all(log_dir)?;
        let ts = Utc::now().format("%Y%m%dT%H%M%S");
        let path = log_dir.join(format!("pool-{worker_id}-{ts}.log"));
        let file = OpenOptions::new().append(true).create(true).open(&path)?;
        Ok((Some(file), Some(path)))
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // A pool dropped mid-supervision (a panic, an early return) still
        // leaves nothing alive.
        self.stop(None);
    }
}

/// The last [`DEATH_LOG_TAIL_LINES`] non-empty lines of a dead member's
/// captured output, read from at most [`DEATH_LOG_TAIL_BYTES`] of the file's
/// tail; empty when output was inherited or the file is unreadable.
pub fn death_log_tail(log_path: Option<&Path>) -> Vec<String> {
    let Some(path) = log_path else {
        return Vec::new();
    };
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(DEATH_LOG_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    if start > 0 && !lines.is_empty() {
        // The first line of a mid-file read is almost always a fragment.
        lines.remove(0);
    }
    let keep = lines.len().saturating_sub(DEATH_LOG_TAIL_LINES);
    lines.split_off(keep)
}

/// Python `%g` (precision 6) for the budget-window log fields.
fn format_g(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    let exponent = value.abs().log10().floor() as i32;
    if !(-4..6).contains(&exponent) {
        let rendered = format!("{value:.5e}");
        let (mantissa, exp) = rendered.split_once('e').unwrap_or((&rendered, "0"));
        let mantissa = trim_fraction(mantissa);
        let exp: i32 = exp.parse().unwrap_or(0);
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    let decimals = usize::try_from(5 - exponent).unwrap_or(0);
    trim_fraction(&format!("{value:.decimals$}"))
}

fn trim_fraction(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_owned()
}

// --- Member argv / the pool entry point --------------------------------------------------------

/// The launcher a pool member is re-invoked through: this executable's
/// `worker` verb (`flywheel worker` / `fw worker`, one implementation).
pub fn member_program() -> std::io::Result<Vec<String>> {
    let exe = std::env::current_exe()?;
    Ok(vec![
        exe.to_string_lossy().into_owned(),
        "worker".to_owned(),
    ])
}

/// Compose the argv for one pool member: `program` (the worker launcher)
/// re-invoked at `--concurrency 1` with a distinct `--worker-id`.
///
/// Every other run knob the operator passed is forwarded verbatim so a pool
/// member behaves exactly like a hand-launched single worker against the
/// same store; `--concurrency 1` is pinned explicitly so the member runs one
/// task at a time and never recurses into another pool, regardless of
/// config.
pub fn pool_member_argv(
    program: &[String],
    args: &WorkerArgs,
    worker_id: &str,
    model: Option<&str>,
) -> Vec<String> {
    let mut argv: Vec<String> = program.to_vec();
    argv.extend(
        [
            "--concurrency",
            "1",
            "--worker-id",
            worker_id,
            "--max-turns",
            &args.max_turns.to_string(),
            "--max-retries",
            &args.max_retries.to_string(),
            "--worktree-retention-days",
            &args.worktree_retention_days.to_string(),
            "--run-log-retention",
            &args.run_log_retention.to_string(),
            "--heartbeat",
            &args.heartbeat.to_string(),
            "--poll-interval",
            &args.poll_interval.to_string(),
            "--lease-seconds",
            &python_float_repr(args.lease_seconds),
            "--reconcile-seconds",
            &python_float_repr(args.reconcile_seconds),
        ]
        .into_iter()
        .map(str::to_owned),
    );
    if let Some(tasks_dir) = &args.tasks_dir {
        argv.extend(["--tasks-dir".to_owned(), tasks_dir.clone()]);
    }
    if let Some(db) = &args.db {
        argv.extend(["--db".to_owned(), db.clone()]);
    }
    if let Some(sandbox_root) = &args.sandbox_root {
        argv.extend(["--sandbox-root".to_owned(), sandbox_root.clone()]);
    }
    if let Some(model) = model {
        argv.extend(["--model".to_owned(), model.to_owned()]);
    }
    if args.once {
        argv.push("--once".to_owned());
    }
    argv
}

/// Drive a [`WorkerPool`] of `concurrency` single-task members.
///
/// The members share the one store at `db_path` and land through the same
/// merge-flock as a hand-launched fleet would; this only supervises them
/// from one invocation. The pool's worker-id prefix is the operator's
/// `--worker-id` when given (so members read `<id>-0`, `<id>-1`, ...), else
/// a fresh per-invocation prefix, keeping every member's id distinct so
/// claims attribute to the right worker. Member output lands under
/// `<db dir>/logs/worker/`.
pub fn run_pool(
    program: Vec<String>,
    args: &WorkerArgs,
    concurrency: u32,
    model: Option<&str>,
    db_path: &Path,
    worker_id: Option<&str>,
    log: Logger,
) -> i32 {
    let prefix = worker_id.map_or_else(
        || format!("pool-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
        str::to_owned,
    );
    let member_args = args.clone();
    let model = model.map(str::to_owned);
    let spawn_member: SpawnMember = Arc::new(move |member_id: &str| {
        pool_member_argv(&program, &member_args, member_id, model.as_deref())
    });
    let log_dir = db_path
        .parent()
        .map(|p| p.join("logs").join("worker"))
        .unwrap_or_else(|| PathBuf::from("logs").join("worker"));
    let options = PoolOptions::new(
        i64::from(concurrency),
        spawn_member,
        Arc::clone(&log),
        args.once,
    )
    .with_prefix(prefix.clone())
    .with_log_dir(log_dir);
    let mut pool = match WorkerPool::new(options) {
        Ok(pool) => pool,
        Err(err) => {
            log(&format!("cannot start the worker pool: {err}"));
            return 1;
        }
    };
    log(&format!(
        "started worker pool size={concurrency} prefix={prefix} once={} pid={}",
        if args.once { "True" } else { "False" },
        std::process::id()
    ));
    pool.run_supervised()
}
