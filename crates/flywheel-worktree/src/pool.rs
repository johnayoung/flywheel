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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    fn quiet() -> Logger {
        Arc::new(|_m: &str| {})
    }

    fn capture() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&lines);
        (
            Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned())),
            lines,
        )
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()]
    }

    fn sleep_argv() -> SpawnMember {
        Arc::new(|_id: &str| sh("exec sleep 300"))
    }

    fn exit_zero_argv() -> SpawnMember {
        Arc::new(|_id: &str| sh("exit 0"))
    }

    fn crash_argv() -> SpawnMember {
        Arc::new(|_id: &str| sh("exit 7"))
    }

    /// A member that ignores SIGTERM and spawns a SIGTERM-ignoring
    /// grandchild whose pid it writes to `pidfile`: only the escalation to
    /// SIGKILL can end them -- a true group-escalation probe.
    fn ignore_term_argv(pidfile: &Path) -> Vec<String> {
        sh(&format!(
            "trap '' TERM; ( trap '' TERM; exec sleep 300 ) & echo $! > '{}'; wait",
            pidfile.display()
        ))
    }

    /// Alive and not a zombie (a reaped-by-init orphan reads as gone; a
    /// zombie would still answer `kill(pid, 0)`).
    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 performs no delivery; it only probes existence.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return false;
        }
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let after_comm = stat.rsplit(')').next().unwrap_or("");
                !after_comm.trim_start().starts_with('Z')
            }
            Err(_) => true,
        }
    }

    fn read_pid(pidfile: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(text) = fs::read_to_string(pidfile)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("grandchild pid file never appeared");
    }

    fn pool(size: i64, spawn: SpawnMember, once: bool, prefix: &str) -> WorkerPool {
        WorkerPool::new(PoolOptions::new(size, spawn, quiet(), once).with_prefix(prefix)).unwrap()
    }

    // --- spawn / identity ---------------------------------------------------------------

    #[test]
    fn pool_spawns_size_distinct_members() {
        let mut pool = pool(3, sleep_argv(), false, "t");
        pool.start().unwrap();
        let pids = pool.live_member_pids();
        assert_eq!(pids.len(), 3);
        assert_eq!(pids.iter().collect::<BTreeSet<_>>().len(), 3);
        pool.stop(Some(0.5));
        assert!(pool.live_member_pids().is_empty());
        assert_eq!(pool.size(), 3);
    }

    #[test]
    fn pool_member_worker_ids_are_distinct_and_prefixed() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&seen);
        let spawn: SpawnMember = Arc::new(move |id: &str| {
            sink.lock().unwrap().push(id.to_owned());
            sh("exec sleep 300")
        });
        let (log, lines) = capture();
        let mut pool =
            WorkerPool::new(PoolOptions::new(3, spawn, log, false).with_prefix("fleet")).unwrap();
        pool.start().unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["fleet-0", "fleet-1", "fleet-2"]
        );
        pool.stop(Some(0.5));
        let logged = lines.lock().unwrap().join("\n");
        assert!(
            logged.contains("pool member fleet-0 spawned pid="),
            "{logged}"
        );
    }

    #[test]
    fn pool_rejects_a_sub_one_size_and_a_bad_budget() {
        let err = WorkerPool::new(PoolOptions::new(0, sleep_argv(), quiet(), false)).unwrap_err();
        assert_eq!(err.to_string(), "worker pool size must be >= 1, got 0");
        let err = WorkerPool::new(
            PoolOptions::new(1, sleep_argv(), quiet(), false).with_restart_budget(1, 0.0),
        )
        .unwrap_err();
        assert!(matches!(err, PoolError::Budget(_)));
    }

    // --- --once drain / retire -----------------------------------------------------------

    #[test]
    fn pool_once_retires_cleanly_drained_members() {
        let mut pool = WorkerPool::new(
            PoolOptions::new(2, exit_zero_argv(), quiet(), true)
                .with_prefix("once")
                .with_poll_interval(0.02),
        )
        .unwrap();
        assert_eq!(pool.run_supervised(), 0);
        assert!(pool.is_done());
        assert!(pool.live_member_pids().is_empty());
        assert_eq!(pool.retired(), &BTreeSet::from([0, 1]));
        assert!(pool.exhausted().is_empty());
    }

    #[test]
    fn pool_once_does_not_respawn_clean_members() {
        let spawns: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&spawns);
        let spawn: SpawnMember = Arc::new(move |id: &str| {
            sink.lock().unwrap().push(id.to_owned());
            sh("exit 0")
        });
        let mut pool = WorkerPool::new(
            PoolOptions::new(2, spawn, quiet(), true)
                .with_prefix("once")
                .with_poll_interval(0.02),
        )
        .unwrap();
        assert_eq!(pool.run_supervised(), 0);
        let mut seen = spawns.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, ["once-0", "once-1"]);
    }

    // --- crash restart-and-reclaim (bounded) ----------------------------------------------

    #[test]
    fn pool_restarts_crashed_member_same_slot() {
        let mut pool = WorkerPool::new(
            PoolOptions::new(1, crash_argv(), quiet(), false)
                .with_prefix("r")
                .with_restart_budget(3, 300.0),
        )
        .unwrap();
        pool.start().unwrap();
        let first_pid = pool.member_pid(0).unwrap();
        assert!(pool.wait_member(0, Duration::from_secs(5)));
        pool.supervise_tick().unwrap();
        let restarted = pool.member_pid(0).unwrap();
        assert_ne!(restarted, first_pid);
        assert!(pool.retired().is_empty());
        pool.stop(Some(0.5));
    }

    #[test]
    fn pool_gives_up_after_restart_budget() {
        let (log, lines) = capture();
        let mut pool = WorkerPool::new(
            PoolOptions::new(1, crash_argv(), log, false)
                .with_prefix("g")
                .with_poll_interval(0.02)
                .with_restart_budget(2, 300.0),
        )
        .unwrap();
        // A member that crashes on every (re)start exhausts its windowed
        // crash-loop budget and the pool RETIRES that slot rather than
        // spinning forever; the single slot is the whole fleet, so the pool
        // ends with a non-zero exit for operator inspection.
        assert_eq!(pool.run_supervised(), 1);
        assert!(pool.live_member_pids().is_empty());
        assert_eq!(pool.exhausted(), &BTreeSet::from([0]));
        let logged = lines.lock().unwrap().join("\n");
        assert!(
            logged.contains(
                "pool member g-0 exited 7; respawning (death 1 within 300s window, budget 2)"
            ),
            "{logged}"
        );
        assert!(logged.contains("exhausted its crash-loop budget (2 respawns within 300s, last exit 7); retiring this slot"), "{logged}");
    }

    #[test]
    fn slot_retire_keeps_fleet() {
        // A pool of size 2 where one slot crash-loops past its budget
        // retires THAT slot in isolation and keeps the healthy member
        // running (spec 00070, D-C): the surviving pid is still alive after
        // slot 0 retires, no pool-wide stop was requested, and stop()
        // afterwards still leaves nothing alive.
        let spawn: SpawnMember = Arc::new(|id: &str| {
            if id.ends_with("-0") {
                sh("exit 7")
            } else {
                sh("exec sleep 300")
            }
        });
        let mut pool = WorkerPool::new(
            PoolOptions::new(2, spawn, quiet(), false)
                .with_prefix("fleet")
                .with_restart_budget(1, 300.0),
        )
        .unwrap();
        pool.start().unwrap();
        let healthy_pid = pool.member_pid(1).unwrap();
        assert!(alive(healthy_pid));
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pool.retired().contains(&0) && Instant::now() < deadline {
            pool.wait_member(0, Duration::from_secs(5));
            pool.supervise_tick().unwrap();
        }
        assert!(pool.retired().contains(&0), "failing slot never retired");
        assert!(pool.exhausted().contains(&0));
        assert!(alive(healthy_pid));
        assert_eq!(pool.live_member_pids(), vec![healthy_pid]);
        assert!(!pool.stop_requested());
        assert!(!pool.retired().contains(&1));
        pool.stop(Some(1.0));
        assert!(pool.live_member_pids().is_empty());
        assert!(!alive(healthy_pid));
    }

    #[test]
    fn pool_slot_budget_is_windowed_not_lifetime() {
        // Two crashes straddling the window both respawn: the budget is
        // windowed, not a lifetime counter (spec 00070, D-B).
        let now = Arc::new(Mutex::new(0.0f64));
        let clock_now = Arc::clone(&now);
        let spawns: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&spawns);
        let spawn: SpawnMember = Arc::new(move |id: &str| {
            sink.lock().unwrap().push(id.to_owned());
            sh("exit 7")
        });
        let mut pool = WorkerPool::new(
            PoolOptions::new(1, spawn, quiet(), false)
                .with_prefix("w")
                .with_restart_budget(1, 10.0)
                .with_clock(Arc::new(move || *clock_now.lock().unwrap())),
        )
        .unwrap();
        pool.start().unwrap();
        assert_eq!(spawns.lock().unwrap().len(), 1);
        assert!(pool.wait_member(0, Duration::from_secs(5)));
        pool.supervise_tick().unwrap();
        assert!(!pool.retired().contains(&0));
        assert_eq!(spawns.lock().unwrap().len(), 2);
        *now.lock().unwrap() = 25.0;
        assert!(pool.wait_member(0, Duration::from_secs(5)));
        pool.supervise_tick().unwrap();
        assert!(
            !pool.retired().contains(&0),
            "windowed budget must replenish"
        );
        assert!(!pool.exhausted().contains(&0));
        assert_eq!(spawns.lock().unwrap().len(), 3);
        pool.stop(Some(1.0));
    }

    // --- orphan-free shutdown ---------------------------------------------------------------

    #[test]
    fn pool_stop_force_kills_sigterm_ignoring_groups() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfiles: BTreeMap<String, PathBuf> = BTreeMap::from([
            ("k-0".to_owned(), tmp.path().join("gc-0.pid")),
            ("k-1".to_owned(), tmp.path().join("gc-1.pid")),
        ]);
        let files = pidfiles.clone();
        let spawn: SpawnMember = Arc::new(move |id: &str| ignore_term_argv(&files[id]));
        let mut pool = pool(2, spawn, false, "k");
        pool.start().unwrap();
        let member_pids = pool.live_member_pids();
        let grandchild_pids: Vec<u32> = ["k-0", "k-1"]
            .iter()
            .map(|id| read_pid(&pidfiles[*id]))
            .collect();
        assert_eq!(member_pids.len(), 2);
        assert!(
            member_pids
                .iter()
                .chain(&grandchild_pids)
                .all(|p| alive(*p))
        );
        // SIGTERM is ignored, so stop must escalate to SIGKILL within the
        // window and take down every member's whole process group.
        pool.stop(Some(0.5));
        assert!(member_pids.iter().all(|p| !alive(*p)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while grandchild_pids.iter().any(|p| alive(*p)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(grandchild_pids.iter().all(|p| !alive(*p)));
    }

    #[test]
    fn pool_stop_is_idempotent_and_drop_stops() {
        let mut pool = pool(1, sleep_argv(), false, "i");
        pool.start().unwrap();
        let pid = pool.member_pid(0).unwrap();
        pool.stop(Some(0.5));
        pool.stop(Some(0.5));
        assert!(pool.live_member_pids().is_empty());
        assert!(!alive(pid));
        let mut dropped = pool_with(1, sleep_argv(), "d");
        dropped.start().unwrap();
        let pid = dropped.member_pid(0).unwrap();
        drop(dropped);
        let deadline = Instant::now() + Duration::from_secs(12);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid));
    }

    fn pool_with(size: i64, spawn: SpawnMember, prefix: &str) -> WorkerPool {
        WorkerPool::new(
            PoolOptions::new(size, spawn, quiet(), false)
                .with_prefix(prefix)
                .with_poll_interval(0.02),
        )
        .unwrap()
    }

    #[test]
    fn member_output_is_captured_and_tailed_on_death() {
        let tmp = tempfile::tempdir().unwrap();
        let spawn: SpawnMember =
            Arc::new(|_id: &str| sh("echo first; echo second; echo oops >&2; exit 3"));
        let (log, lines) = capture();
        let mut pool = WorkerPool::new(
            PoolOptions::new(1, spawn, log, false)
                .with_prefix("c")
                .with_log_dir(tmp.path().join("logs"))
                .with_restart_budget(0, 300.0),
        )
        .unwrap();
        pool.start().unwrap();
        assert!(pool.wait_member(0, Duration::from_secs(5)));
        pool.supervise_tick().unwrap();
        let logged = lines.lock().unwrap().join("\n");
        assert!(
            logged.contains("pool member c-0 last output: first"),
            "{logged}"
        );
        assert!(
            logged.contains("pool member c-0 last output: oops"),
            "{logged}"
        );
        assert!(logged.contains("last exit 3"), "{logged}");
        let log_files: Vec<_> = fs::read_dir(tmp.path().join("logs"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(log_files.len(), 1);
        assert!(
            log_files[0]
                .file_name()
                .to_string_lossy()
                .starts_with("pool-c-0-")
        );
        assert!(death_log_tail(None).is_empty());
        assert!(death_log_tail(Some(Path::new("/nonexistent/x.log"))).is_empty());
    }

    #[test]
    fn spawn_failure_is_reported_not_panicked() {
        let spawn: SpawnMember = Arc::new(|_id: &str| vec!["/nonexistent/bin/worker".to_owned()]);
        let mut pool = pool(1, spawn, false, "s");
        let err = pool.start().unwrap_err();
        assert!(
            err.to_string()
                .starts_with("cannot spawn pool member s-0: "),
            "{err}"
        );
        let empty: SpawnMember = Arc::new(|_id: &str| Vec::new());
        let mut pool = pool_with(1, empty, "e");
        assert_eq!(pool.run_supervised(), 1);
    }

    // --- member argv plumbing ----------------------------------------------------------------

    fn program() -> Vec<String> {
        vec!["/opt/flywheel".to_owned(), "worker".to_owned()]
    }

    fn full_args() -> WorkerArgs {
        WorkerArgs {
            max_turns: 12,
            max_retries: 4,
            worktree_retention_days: 7,
            heartbeat: 30,
            poll_interval: 5,
            ..WorkerArgs::default()
        }
    }

    fn value_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let index = argv
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag} missing"));
        &argv[index + 1]
    }

    #[test]
    fn pool_member_argv_pins_concurrency_one_and_distinct_id() {
        let argv = pool_member_argv(&program(), &full_args(), "fleet-2", None);
        assert_eq!(&argv[..2], &program()[..]);
        assert_eq!(value_after(&argv, "--concurrency"), "1");
        assert_eq!(value_after(&argv, "--worker-id"), "fleet-2");
        let other = pool_member_argv(&program(), &full_args(), "fleet-1", None);
        assert_eq!(value_after(&other, "--worker-id"), "fleet-1");
    }

    #[test]
    fn pool_member_argv_forwards_run_knobs() {
        let args = WorkerArgs {
            max_turns: 9,
            max_retries: 2,
            lease_seconds: 120.0,
            run_log_retention: 42,
            tasks_dir: Some("/tmp/tasks".into()),
            db: Some("/tmp/db.sqlite".into()),
            sandbox_root: Some("@cache".into()),
            once: true,
            ..full_args()
        };
        let argv = pool_member_argv(&program(), &args, "fleet-0", Some("sonnet"));
        assert_eq!(value_after(&argv, "--max-turns"), "9");
        assert_eq!(value_after(&argv, "--max-retries"), "2");
        assert_eq!(value_after(&argv, "--lease-seconds"), "120.0");
        assert_eq!(value_after(&argv, "--reconcile-seconds"), "15.0");
        assert_eq!(value_after(&argv, "--run-log-retention"), "42");
        assert_eq!(value_after(&argv, "--heartbeat"), "30");
        assert_eq!(value_after(&argv, "--poll-interval"), "5");
        assert_eq!(value_after(&argv, "--worktree-retention-days"), "7");
        assert_eq!(value_after(&argv, "--tasks-dir"), "/tmp/tasks");
        assert_eq!(value_after(&argv, "--db"), "/tmp/db.sqlite");
        assert_eq!(value_after(&argv, "--sandbox-root"), "@cache");
        assert_eq!(value_after(&argv, "--model"), "sonnet");
        assert!(argv.iter().any(|a| a == "--once"));
        // The member argv round-trips through the worker parser.
        let parsed = crate::daemon::parse_args(&argv[2..]).unwrap();
        assert!(matches!(parsed, crate::daemon::Parsed::Args(ref a) if a.once && a.max_turns == 9));
    }

    #[test]
    fn pool_member_argv_omits_optional_flags_when_unset() {
        let argv = pool_member_argv(&program(), &full_args(), "fleet-0", None);
        for flag in ["--tasks-dir", "--db", "--sandbox-root", "--model", "--once"] {
            assert!(!argv.iter().any(|a| a == flag), "{flag} present");
        }
    }

    #[test]
    fn member_program_is_this_executable_worker_verb() {
        let program = member_program().unwrap();
        assert_eq!(program.len(), 2);
        assert_eq!(program[1], "worker");
    }

    // --- rendering helpers ----------------------------------------------------------------

    #[test]
    fn python_g_formatting() {
        assert_eq!(format_g(300.0), "300");
        assert_eq!(format_g(10.0), "10");
        assert_eq!(format_g(0.5), "0.5");
        assert_eq!(format_g(1.25), "1.25");
        assert_eq!(format_g(0.0), "0");
        assert_eq!(format_g(1e-5), "1e-05");
        assert_eq!(format_g(2_500_000.0), "2.5e+06");
        assert_eq!(format_g(f64::INFINITY), "inf");
        assert_eq!(format_g(f64::NAN), "nan");
    }

    #[test]
    fn member_exit_renders_like_popen_returncode() {
        let code = MemberExit {
            code: Some(7),
            signal: None,
        };
        assert_eq!(code.to_string(), "7");
        assert!(!code.is_clean());
        let signalled = MemberExit {
            code: None,
            signal: Some(9),
        };
        assert_eq!(signalled.to_string(), "-9");
        let unknown = MemberExit {
            code: None,
            signal: None,
        };
        assert_eq!(unknown.to_string(), "?");
        let stop = take_stop_signal();
        assert_eq!(stop, None);
    }
}
