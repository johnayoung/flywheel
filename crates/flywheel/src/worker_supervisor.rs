//! Port of the Python `flywheel._worker_supervisor` module: the console's
//! subprocess supervisor for the engine-on-launch worker, plus the
//! spawn / supervise / stop machinery the autopilot supervisor shares.
//!
//! Bare `flywheel` (or `fw`) opens the operator console; this module is how
//! the console keeps a git-worktree worker running underneath it (the
//! worker is `flywheel worker`, launched as a child of the console binary).
//! The supervisor owns three responsibilities and intentionally no more:
//!
//! * **Liveness detection** -- a read of the orchestrator's `task_claims`
//!   table. A row whose `lease_expires_at` is still in the future counts as
//!   a live worker; a lapsed lease is treated as no live worker (the
//!   existing lease-lapse semantics).
//! * **Spawn / supervise** -- when no live worker is detected, spawn a child
//!   running the worker daemon and remember it as ours. The child's
//!   stdout/stderr are redirected to a per-spawn supervisor log under
//!   `.flywheel/logs/worker/`; the log directory's footprint is bounded by
//!   reclaiming the oldest logs on every spawn.
//! * **Quit handoff** -- `detach` drops the reference to the child (the
//!   worker keeps running, the console can exit); `stop` signals the
//!   child's whole process group with SIGTERM and waits so the worker's
//!   graceful-shutdown path finalizes the in-flight lifecycle to
//!   `interrupted`, escalating to SIGKILL when the group outlives the wait.
//!
//! The child is placed in its own session so a Ctrl+C on the console
//! terminal never reaches it silently -- `stop` is the only path that
//! signals it. The supervisor manages only children it spawned: an
//! externally-started worker shows as `Detached` and is never signaled.
//!
//! [`SupervisedProcess`] is the shared core (spawn into a fresh session,
//! per-spawn log, death capture, crash-loop respawn through the shared
//! [`SupervisionPolicy`], group shutdown); [`WorkerSupervisor`] layers the
//! lease probe and the worker argv on top, and the autopilot supervisor
//! layers its liveness record the same way.

use crate::tui::{DaemonState, DaemonStatus};
use chrono::{DateTime, Utc};
use flywheel_orchestrator::claims::{ClaimStore, SqliteClaimStore};
use flywheel_orchestrator::supervision_policy::{RespawnDecision, SupervisionPolicy};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// How long `stop` waits for the child to exit after SIGTERM before
/// escalating to SIGKILL. The worker's graceful shutdown finalizes the
/// in-flight lifecycle; a few seconds of headroom covers the post-finalize
/// cleanup. Tests pass a smaller value so the suite stays snappy.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the worker supervisor's per-spawn logs land by default, next to
/// the worker's own per-run forensics.
pub const DEFAULT_WORKER_LOG_DIR: &str = ".flywheel/logs/worker";

/// Every worker supervisor log is `supervisor-<ts>-<seq>.log`; the reclaim
/// only ever touches files carrying this prefix.
pub const WORKER_LOG_PREFIX: &str = "supervisor-";

/// Default-on ceiling for a supervisor log directory's total footprint:
/// every spawn reclaims the oldest logs to hold the directory at or under
/// this many bytes. Bounded and default-on is the requirement; the
/// magnitude is a sane default a caller may override.
pub const DEFAULT_MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// A captured death reason is capped at this many characters.
pub const DEATH_REASON_MAX_LEN: usize = 240;

/// SIGTERM's number on every unix.
pub const SIGTERM: i32 = 15;
/// SIGKILL's number on every unix.
pub const SIGKILL: i32 = 9;

/// How often the stop path polls the child while waiting it out.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);

// --- Process primitives --------------------------------------------------------------

#[cfg(unix)]
unsafe extern "C" {
    fn killpg(pgrp: i32, sig: i32) -> i32;
    fn setsid() -> i32;
}

/// Python's `Popen.returncode`: the exit code, or the negated signal number
/// for a signal-killed child.
#[cfg(unix)]
pub fn python_exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code,
        None => -status.signal().unwrap_or(0),
    }
}

#[cfg(not(unix))]
pub fn python_exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// Signal the child's whole process group; `false` when it no longer
/// exists.
///
/// The child is a session leader (spawned through `setsid`), so its pgid
/// equals its pid and the signal reaches every descendant that stayed in
/// its group. Agents and command graders do NOT: each runs in a process
/// group of its own, registered in `flywheel_agents::process_groups`. The
/// daemon's SIGTERM/SIGINT handler terminates every registered group
/// (SIGTERM, then SIGKILL after the agent kill grace) the moment the
/// signal lands, so the sweep finishes inside [`DEFAULT_STOP_TIMEOUT`] and
/// reaches the agent trees and their MCP / tool children before any
/// escalation to SIGKILL here.
#[cfg(unix)]
fn signal_group(child: &mut Child, sig: i32) -> bool {
    let Ok(pid) = i32::try_from(child.id()) else {
        return false;
    };
    // SAFETY: killpg(2) takes two plain integers and touches no memory. The
    // group id is the child's own pid because the child was spawned as a
    // session leader, so no unrelated process group can be targeted; a
    // group that is already gone reports ESRCH, which reads as `false`.
    unsafe { killpg(pid, sig) == 0 }
}

#[cfg(not(unix))]
fn signal_group(child: &mut Child, _sig: i32) -> bool {
    child.kill().is_ok()
}

/// Spawn `argv` in its own session with stdout+stderr redirected to `log`.
fn spawn_in_own_session(argv: &[String], log: File) -> io::Result<Child> {
    let Some((program, rest)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "spawn argv is empty",
        ));
    };
    let stderr = log.try_clone()?;
    let mut command = Command::new(program);
    command
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid(2) is async-signal-safe and touches no memory; it
        // runs in the forked child before exec, which is exactly Python's
        // `start_new_session=True`. A failure (the child is never already a
        // group leader, so none is expected) leaves the child in the
        // console's session, which only weakens Ctrl+C isolation.
        unsafe {
            command.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
    }
    command.spawn()
}

/// Spin until `child` has exited or `timeout` elapses; `Some(status)` when
/// it exited. `try_wait` reaps and caches the status, so a later poll sees
/// the same exit.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(STOP_POLL_INTERVAL);
    }
}

// --- Death reasons ----------------------------------------------------------------------

/// Extract a human-readable death reason from a dead child's log file.
///
/// A supervised child's stdout+stderr are redirected to `log_path`; on an
/// unexpected exit the most informative line is typically the last
/// non-blank one -- e.g. `flywheel worker: policy error: ...` (a config
/// problem the operator can fix). Returns that line (capped), skipping the
/// `RuntimeWarning` noise a Python-launched agent may emit, or `None` when
/// the log is empty/unreadable. Best-effort: a missing reason simply
/// degrades to the bare exit code.
pub fn read_supervised_death_reason(log_path: Option<&Path>) -> Option<String> {
    let bytes = fs::read(log_path?).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let meaningful: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !line.contains("RuntimeWarning") && !line.contains("found in sys.modules"))
        .collect();
    let candidates = if meaningful.is_empty() {
        &lines
    } else {
        &meaningful
    };
    let reason = *candidates.last()?;
    if reason.chars().count() > DEATH_REASON_MAX_LEN {
        let head: String = reason.chars().take(DEATH_REASON_MAX_LEN - 1).collect();
        return Some(format!("{head}\u{2026}"));
    }
    Some(reason.to_owned())
}

/// Compose the dead-state message: the exit code plus, when known, the
/// captured failure reason so the console explains *why* the child died
/// instead of only that it did. An unknown exit renders as `exit=None`.
pub fn format_dead_message(exit_code: Option<i32>, reason: Option<&str>) -> String {
    let base = match exit_code {
        Some(code) => format!("exit={code}"),
        None => "exit=None".to_owned(),
    };
    match reason {
        Some(reason) if !reason.is_empty() => format!("{base}: {reason}"),
        _ => base,
    }
}

// --- Liveness ---------------------------------------------------------------------------

/// Whether any worker currently holds a live lease in `task_claims`.
///
/// Reads the orchestrator's claim store at `db_path`. A lease whose
/// `lease_expires_at` is at or before `now` (a lapsed lease) does NOT count
/// as a live worker, matching the acquire-steal semantics. Returns
/// `Ok(false)` when the database file does not exist (a fresh project that
/// has never been run); opening an existing store bootstraps the
/// orchestrator schema when it is absent, which the worker's own startup
/// would do anyway. Any other store failure is returned so the caller can
/// surface it rather than read a corrupted store as idle.
pub fn has_live_lease(db_path: &Path, now: DateTime<Utc>) -> Result<bool, String> {
    if !db_path.exists() {
        return Ok(false);
    }
    let claims = SqliteClaimStore::open(db_path).map_err(|err| err.to_string())?;
    let rows = claims.list_claims().map_err(|err| err.to_string())?;
    Ok(rows.iter().any(|claim| claim.lease_expires_at > now))
}

// --- Spawn argv ----------------------------------------------------------------------

/// The console binary the supervisors re-invoke for `worker` / `autopilot`:
/// the running executable (both `flywheel` and `fw` route every verb), or
/// the bare `flywheel` program name resolved through `PATH` when the
/// executable path cannot be determined.
pub fn console_program() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"))
}

/// Compose the `<program> worker --db <path> [--tasks-dir D] [--model M]`
/// argv the production supervisor launches.
///
/// The args are the minimal set the worker needs to land on the same store
/// the console is watching; everything else (heartbeat, lease seconds)
/// defaults to the worker's own parser defaults. `model` is appended as
/// `--model <value>` when set and omitted entirely otherwise so the agent
/// runtime falls through to its own default.
pub fn build_default_spawn_argv(
    program: &Path,
    db_path: &Path,
    tasks_dir: Option<&Path>,
    model: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        program.to_string_lossy().into_owned(),
        "worker".to_owned(),
        "--db".to_owned(),
        db_path.to_string_lossy().into_owned(),
    ];
    if let Some(dir) = tasks_dir {
        argv.push("--tasks-dir".to_owned());
        argv.push(dir.to_string_lossy().into_owned());
    }
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    argv
}

// --- Log reclaim ---------------------------------------------------------------------

/// Delete the oldest `<prefix>*.log` files under `log_dir` until the
/// footprint is at or under `max_bytes`, always preserving the single
/// most-recent log.
///
/// Reclaims whole files oldest-first (by mtime, then name -- the filename
/// encodes the creation timestamp, so it is a stable tiebreaker for logs
/// that share an mtime). The most-recent log is never deleted or truncated:
/// even a lone log larger than `max_bytes` survives intact, so recent
/// forensic content always outlives the bound. A directory holding one log
/// or none is left untouched. Callers open the *new* log only after this
/// returns, so the log preserved here is the most-recent content-bearing
/// one, not a just-created empty file.
pub fn reclaim_supervisor_logs(log_dir: &Path, prefix: &str, max_bytes: u64) {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return;
    };
    let mut logs: Vec<(SystemTime, String, PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(prefix) || !name.ends_with(".log") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        logs.push((mtime, name, entry.path(), meta.len()));
    }
    if logs.len() <= 1 {
        return;
    }
    logs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut total: u64 = logs.iter().map(|entry| entry.3).sum();
    // Oldest first, stopping before the newest entry so one log always
    // survives -- the directory is never emptied.
    for (_mtime, _name, path, size) in &logs[..logs.len() - 1] {
        if total <= max_bytes {
            break;
        }
        if fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*size);
        }
    }
}

// --- The shared supervised child ------------------------------------------------------

/// What the supervisor recorded about its last dead child.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Death {
    pid: u32,
    exit: Option<i32>,
    reason: Option<String>,
}

impl Death {
    fn message(&self) -> String {
        format_dead_message(self.exit, self.reason.as_deref())
    }
}

/// The spawn / poll / stop core both console supervisors share.
///
/// Owns at most one child at a time. `owned_status` resolves everything the
/// supervisor knows locally -- a live child, a death (respawned or not), a
/// spawn error -- and returns `None` only when the caller must consult its
/// cross-process liveness probe for the detached / none distinction.
pub struct SupervisedProcess {
    label: &'static str,
    log_dir: PathBuf,
    log_prefix: &'static str,
    max_log_bytes: u64,
    log_seq: u32,
    policy: Option<SupervisionPolicy>,
    child: Option<Child>,
    /// Path of the current (or last) child's redirected log; kept after the
    /// child is reaped so a death can be explained by reading its tail.
    log_path: Option<PathBuf>,
    last_error: Option<String>,
    dead: Option<Death>,
    /// Latched once a policy-governed child exhausts the crash-loop budget:
    /// the supervisor stops respawning and reports `DeadAfterBudget` until
    /// an operator `start` re-arms it.
    dead_after_budget: bool,
    /// The most recent death's detail, kept across respawns and detaches so
    /// the console can still say why the previous child died.
    last_death: Option<String>,
}

impl std::fmt::Debug for SupervisedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisedProcess")
            .field("label", &self.label)
            .field("log_dir", &self.log_dir)
            .field("child", &self.child.as_ref().map(Child::id))
            .field("dead", &self.dead)
            .field("dead_after_budget", &self.dead_after_budget)
            .field("last_error", &self.last_error)
            .finish_non_exhaustive()
    }
}

impl SupervisedProcess {
    /// A core that has spawned nothing. `label` names the daemon in error
    /// messages (`cannot open worker log: ...`).
    pub fn new(
        label: &'static str,
        log_dir: PathBuf,
        log_prefix: &'static str,
        max_log_bytes: u64,
        policy: Option<SupervisionPolicy>,
    ) -> Self {
        Self {
            label,
            log_dir,
            log_prefix,
            max_log_bytes,
            log_seq: 0,
            policy,
            child: None,
            log_path: None,
            last_error: None,
            dead: None,
            dead_after_budget: false,
            last_death: None,
        }
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub fn set_log_dir(&mut self, log_dir: PathBuf) {
        self.log_dir = log_dir;
    }

    pub fn max_log_bytes(&self) -> u64 {
        self.max_log_bytes
    }

    pub fn set_max_log_bytes(&mut self, max_log_bytes: u64) {
        self.max_log_bytes = max_log_bytes;
    }

    pub fn set_policy(&mut self, policy: Option<SupervisionPolicy>) {
        self.policy = policy;
    }

    /// The owned child's pid while one is held (live or not yet observed
    /// dead).
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// The path of the current or last child's supervisor log.
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }

    /// The last recorded death detail regardless of the current state.
    pub fn death_reason(&self) -> Option<String> {
        self.last_death.clone()
    }

    /// Resolve the locally-known state; `None` when no child is owned and
    /// no death / spawn error is latched, so the caller probes liveness.
    ///
    /// A child that died on its own is reaped here, its reason captured
    /// from the log tail, and the crash-loop policy decides whether this
    /// tick auto-respawns it (returning `Supervised` with the new pid) or
    /// latches the exhausted terminal state.
    pub fn owned_status(&mut self, spawn_argv: &[String]) -> Option<DaemonStatus> {
        if let Some(child) = self.child.as_mut() {
            let pid = child.id();
            match child.try_wait() {
                Ok(Some(status)) => {
                    let exit = python_exit_code(status);
                    let reason = read_supervised_death_reason(self.log_path.as_deref());
                    self.child = None;
                    if let Some(respawned) =
                        self.respawn_or_retire(pid, Some(exit), reason, spawn_argv)
                    {
                        return Some(respawned);
                    }
                }
                // Still running, or the poll itself failed (in which case
                // the child is still ours and still reported live).
                Ok(None) | Err(_) => {
                    return Some(DaemonStatus::new(DaemonState::Supervised).with_pid(pid));
                }
            }
        }
        if self.dead_after_budget
            && let Some(dead) = &self.dead
        {
            return Some(
                DaemonStatus::new(DaemonState::DeadAfterBudget)
                    .with_pid(dead.pid)
                    .with_message(dead.message()),
            );
        }
        if let Some(dead) = &self.dead {
            return Some(
                DaemonStatus::new(DaemonState::Dead)
                    .with_pid(dead.pid)
                    .with_message(dead.message()),
            );
        }
        if let Some(error) = &self.last_error {
            return Some(DaemonStatus::new(DaemonState::Error).with_message(error.clone()));
        }
        None
    }

    /// Forget any leftover dead / dead-after-budget / error state from
    /// prior attempts so the snapshot matches reality post-spawn.
    pub fn clear_markers(&mut self) {
        self.dead = None;
        self.dead_after_budget = false;
        self.last_error = None;
    }

    /// An operator start: re-arm the crash-loop budget (a manual restart
    /// begins with a full window again) and spawn.
    pub fn arm_and_spawn(&mut self, spawn_argv: &[String]) -> DaemonStatus {
        if let Some(policy) = self.policy.as_mut() {
            policy.reset();
        }
        self.spawn_child(spawn_argv)
    }

    /// Decide what to do about a child that died on its own.
    ///
    /// With no policy, or a disabled (budget-0) policy, the death is recorded
    /// and `None` returned so the status falls through to plain `Dead` --
    /// never respawning (the unattended-base-branch safety override). With an
    /// active policy the death is charged against the shared windowed
    /// budget: inside budget a real new child is launched (returning
    /// `Supervised` with the new pid); past budget respawning stops and the
    /// distinct `DeadAfterBudget` state latches.
    fn respawn_or_retire(
        &mut self,
        pid: u32,
        exit: Option<i32>,
        reason: Option<String>,
        spawn_argv: &[String],
    ) -> Option<DaemonStatus> {
        let decision = match self.policy.as_mut() {
            Some(policy) if !policy.budget().disabled() => policy.record_death(),
            _ => {
                self.record_death(pid, exit, reason);
                return None;
            }
        };
        if decision == RespawnDecision::Respawn {
            // Remember why the previous child died, then present a clean
            // Supervised rather than trailing a stale dead reason.
            self.last_death = Some(format_dead_message(exit, reason.as_deref()));
            self.clear_markers();
            let spawned = self.spawn_child(spawn_argv);
            if spawned.state == DaemonState::Supervised {
                return Some(spawned);
            }
            // The respawn's own spawn failed: `last_error` is latched and
            // the status falls through to surface it.
            return None;
        }
        self.record_death(pid, exit, reason);
        self.dead_after_budget = true;
        None
    }

    fn record_death(&mut self, pid: u32, exit: Option<i32>, reason: Option<String>) {
        let death = Death { pid, exit, reason };
        self.last_death = Some(death.message());
        self.dead = Some(death);
    }

    /// Spawn one child, redirecting its output to a fresh log.
    ///
    /// The single spawn seam shared by the operator start and the automatic
    /// in-budget respawn, so both take the identical orphan-safe path: the
    /// child is a session leader (a console Ctrl+C never reaches it; only
    /// `stop` signals it) and its stdout/stderr land in a per-spawn log.
    /// Returns `Supervised` with the new pid, or `Error` (with the cause
    /// latched) when the log cannot be opened or the spawn fails.
    fn spawn_child(&mut self, spawn_argv: &[String]) -> DaemonStatus {
        let log = match self.open_log() {
            Ok(log) => log,
            Err(err) => {
                let message = format!("cannot open {} log: {err}", self.label);
                self.last_error = Some(message.clone());
                return DaemonStatus::new(DaemonState::Error).with_message(message);
            }
        };
        match spawn_in_own_session(spawn_argv, log) {
            Ok(child) => {
                let pid = child.id();
                self.child = Some(child);
                DaemonStatus::new(DaemonState::Supervised).with_pid(pid)
            }
            Err(err) => {
                let message = format!("spawn failed: {err}");
                self.last_error = Some(message.clone());
                DaemonStatus::new(DaemonState::Error).with_message(message)
            }
        }
    }

    /// Open the next per-spawn log in append mode.
    ///
    /// The directory is reclaimed BEFORE the new (empty) log is created: at
    /// that point the newest log on disk is the previous spawn's
    /// content-bearing one, so it is the log the reclaim preserves. A
    /// per-supervisor sequence makes each filename unique even when two
    /// spawns land in the same clock second, so a rapid restart never
    /// appends to a prior spawn's log.
    pub fn open_log(&mut self) -> io::Result<File> {
        fs::create_dir_all(&self.log_dir)?;
        reclaim_supervisor_logs(&self.log_dir, self.log_prefix, self.max_log_bytes);
        let stamp = Utc::now().format("%Y%m%dT%H%M%S");
        let name = format!("{}{stamp}-{:04}.log", self.log_prefix, self.log_seq);
        self.log_seq = self.log_seq.wrapping_add(1);
        let path = self.log_dir.join(name);
        let file = OpenOptions::new().append(true).create(true).open(&path)?;
        self.log_path = Some(path);
        Ok(file)
    }

    /// Stop the owned child and everything it spawned.
    ///
    /// Signals the child's whole process group: a graceful SIGTERM first
    /// (giving the daemon's shutdown path `timeout` to finalize the
    /// in-flight work); if the group is still alive after that -- a worker
    /// blocked mid-agent-call cannot honor the signal until the call
    /// returns -- it is escalated to SIGKILL so the console never exits
    /// leaving an orphan. Returns `true` when a running child was stopped
    /// (gracefully or force-killed), `false` when there was no supervised
    /// child or it had already died. Idempotent.
    pub fn stop(&mut self, timeout: Duration) -> bool {
        let Some(mut child) = self.child.take() else {
            return false;
        };
        let pid = child.id();
        if let Ok(Some(status)) = child.try_wait() {
            // Already exited before we signaled it (an unexpected death):
            // capture why so the dead status can explain it.
            let reason = read_supervised_death_reason(self.log_path.as_deref());
            self.record_death(pid, Some(python_exit_code(status)), reason);
            return false;
        }
        if !signal_group(&mut child, SIGTERM) {
            // Vanished between the poll and the signal: nothing is left
            // running, so report it stopped.
            let exit = wait_with_timeout(&mut child, timeout).map(python_exit_code);
            let reason = read_supervised_death_reason(self.log_path.as_deref());
            self.record_death(pid, Some(exit.unwrap_or(0)), reason);
            return true;
        }
        let exit = match wait_with_timeout(&mut child, timeout) {
            Some(status) => Some(python_exit_code(status)),
            None => {
                // Blocked past the graceful window: force the whole group
                // down rather than orphan it, then reap so no zombie is left.
                signal_group(&mut child, SIGKILL);
                wait_with_timeout(&mut child, timeout).map(python_exit_code)
            }
        };
        self.record_death(pid, exit, None);
        true
    }

    /// Forget the owned child without signaling it: the child keeps
    /// running and the console can exit.
    pub fn detach(&mut self) {
        self.child = None;
        self.dead = None;
        self.dead_after_budget = false;
    }

    /// Block until the owned child exits or `timeout` elapses; `true` when
    /// it exited. The exit is cached, so the next `owned_status` observes
    /// the death. A diagnostic aid for deterministic waits; `false` when no
    /// child is owned.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        match self.child.as_mut() {
            Some(child) => wait_with_timeout(child, timeout).is_some(),
            None => false,
        }
    }
}

// --- The worker supervisor ----------------------------------------------------------

/// Spawn, detach from, and stop one git-worktree worker child.
///
/// Construction spawns nothing; call [`start`](Self::start). `status` is
/// cheap (a child poll plus a `task_claims` read) and is what the dashboard
/// calls each tick. The supervisor manages only the child it spawned: a
/// live lease it does not own reads `Detached` and is never signaled.
pub struct WorkerSupervisor {
    db_path: PathBuf,
    tasks_dir: Option<PathBuf>,
    model: Option<String>,
    spawn_argv: Option<Vec<String>>,
    process: SupervisedProcess,
}

impl std::fmt::Debug for WorkerSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerSupervisor")
            .field("db_path", &self.db_path)
            .field("tasks_dir", &self.tasks_dir)
            .field("model", &self.model)
            .field("spawn_argv", &self.spawn_argv)
            .field("process", &self.process)
            .finish()
    }
}

impl WorkerSupervisor {
    /// A supervisor over the shared store at `db_path`, used both for the
    /// liveness probe and as the spawned worker's `--db`. Logs land under
    /// [`DEFAULT_WORKER_LOG_DIR`]; no crash-loop policy (an unexpected
    /// death is reported `Dead` and never auto-respawned) until
    /// [`with_policy`](Self::with_policy).
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            tasks_dir: None,
            model: None,
            spawn_argv: None,
            process: SupervisedProcess::new(
                "worker",
                PathBuf::from(DEFAULT_WORKER_LOG_DIR),
                WORKER_LOG_PREFIX,
                DEFAULT_MAX_LOG_BYTES,
                None,
            ),
        }
    }

    /// Where the per-spawn supervisor logs land.
    pub fn with_log_dir(mut self, log_dir: impl Into<PathBuf>) -> Self {
        self.process.set_log_dir(log_dir.into());
        self
    }

    /// Override the spawn command. Production callers leave the default
    /// `<console> worker --db <path>` argv; tests pass a trivial child so
    /// the ownership / quit-path behaviour is exercised without launching
    /// the real worker. `tasks_dir` / `model` are ignored when set.
    pub fn with_spawn_argv(mut self, argv: Vec<String>) -> Self {
        self.spawn_argv = Some(argv);
        self
    }

    /// Forwarded to the spawned worker as `--tasks-dir` so it watches the
    /// same tree the console resolves.
    pub fn with_tasks_dir(mut self, tasks_dir: Option<PathBuf>) -> Self {
        self.tasks_dir = tasks_dir;
        self
    }

    /// Forwarded to the spawned worker as `--model` when set.
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// The shared crash-loop policy: a death inside its windowed budget is
    /// auto-respawned from `status` (no operator start), a death past it
    /// latches `DeadAfterBudget`. A disabled (budget-0) policy behaves like
    /// none at all.
    pub fn with_policy(mut self, policy: SupervisionPolicy) -> Self {
        self.process.set_policy(Some(policy));
        self
    }

    /// The log directory's footprint ceiling.
    pub fn with_max_log_bytes(mut self, max_log_bytes: u64) -> Self {
        self.process.set_max_log_bytes(max_log_bytes);
        self
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn log_dir(&self) -> &Path {
        self.process.log_dir()
    }

    pub fn max_log_bytes(&self) -> u64 {
        self.process.max_log_bytes()
    }

    /// The argv the next spawn launches.
    pub fn spawn_argv(&self) -> Vec<String> {
        self.spawn_argv.clone().unwrap_or_else(|| {
            build_default_spawn_argv(
                &console_program(),
                &self.db_path,
                self.tasks_dir.as_deref(),
                self.model.as_deref(),
            )
        })
    }

    /// Compute the current status for the status bar.
    ///
    /// Order is significant: a supervisor that owns a live child reports
    /// `Supervised` without touching the database; only when no child (or
    /// death / error) is owned does it consult `task_claims` for the
    /// detached / none distinction. A probe failure is returned so a
    /// corrupted store never silently reads as idle.
    pub fn status(&mut self) -> Result<DaemonStatus, String> {
        let argv = self.spawn_argv();
        if let Some(owned) = self.process.owned_status(&argv) {
            return Ok(owned);
        }
        if has_live_lease(&self.db_path, Utc::now())? {
            return Ok(DaemonStatus::new(DaemonState::Detached));
        }
        Ok(DaemonStatus::new(DaemonState::None))
    }

    /// Whether this supervisor has a live child it spawned. Side-effects
    /// through `status` so a child that just died reads `Dead`.
    pub fn owns_supervised_child(&mut self) -> Result<bool, String> {
        Ok(self.status()?.state == DaemonState::Supervised)
    }

    /// Spawn the worker if no live worker exists; idempotent otherwise.
    ///
    /// Already `Supervised` returns unchanged; `Detached` (another worker
    /// holds a live lease) spawns nothing; a successful spawn returns
    /// `Supervised` with the new pid; a failed spawn returns `Error` with
    /// the cause. Leftover dead / error markers are cleared first and the
    /// lease re-checked, so a live lease (the just-died child's own
    /// not-yet-expired lease, or a peer's) is never missed behind a stale
    /// `Dead` -- that would spawn a duplicate.
    pub fn start(&mut self) -> Result<DaemonStatus, String> {
        let current = self.status()?;
        if matches!(
            current.state,
            DaemonState::Supervised | DaemonState::Detached
        ) {
            return Ok(current);
        }
        self.process.clear_markers();
        let rechecked = self.status()?;
        if rechecked.state == DaemonState::Detached {
            return Ok(rechecked);
        }
        let argv = self.spawn_argv();
        Ok(self.process.arm_and_spawn(&argv))
    }

    /// Stop the supervised worker and everything it spawned (see
    /// [`SupervisedProcess::stop`]).
    pub fn stop(&mut self, timeout: Duration) -> bool {
        self.process.stop(timeout)
    }

    /// Forget the supervised child so the console can exit; nothing is
    /// signaled. A later `status` reports `Detached` once `task_claims`
    /// sees the worker's lease, `None` until then.
    pub fn detach(&mut self) {
        self.process.detach();
    }

    /// Drop resources without affecting the child (a `detach` alias for
    /// the console's exit path).
    pub fn close(&mut self) {
        self.detach();
    }

    /// The last recorded death detail, regardless of the current state.
    pub fn death_reason(&self) -> Option<String> {
        self.process.death_reason()
    }

    /// The owned child's pid, if one is held.
    pub fn child_pid(&self) -> Option<u32> {
        self.process.child_pid()
    }

    /// Block until the owned child exits (see
    /// [`SupervisedProcess::wait_for_exit`]).
    pub fn wait_for_child_exit(&mut self, timeout: Duration) -> bool {
        self.process.wait_for_exit(timeout)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Fake children for the supervisor tests: plain `sh` one-liners, so
    //! the ownership / quit-path behaviour is exercised without launching a
    //! real daemon.

    use std::path::Path;
    use std::time::{Duration, Instant};

    pub fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_owned()).collect()
    }

    /// A child that sleeps `seconds` (long enough that no test races its
    /// self-exit; tests always stop or kill it).
    pub fn sleep_argv(seconds: u32) -> Vec<String> {
        argv(&["sh", "-c", &format!("sleep {seconds}")])
    }

    /// A child that exits `code` immediately.
    pub fn exit_argv(code: i32) -> Vec<String> {
        argv(&["sh", "-c", &format!("exit {code}")])
    }

    /// A child that writes `message` to stderr and exits `code`.
    pub fn dying_argv(message: &str, code: i32) -> Vec<String> {
        argv(&[
            "sh",
            "-c",
            &format!("printf '%s\\n' '{message}' >&2; exit {code}"),
        ])
    }

    /// A child that ignores SIGTERM and spawns a grandchild that does too,
    /// writing the grandchild's pid to `pidfile`. Only the escalation to
    /// SIGKILL (which cannot be caught) can end them.
    pub fn ignore_term_with_child_argv(pidfile: &Path) -> Vec<String> {
        let script = format!(
            "trap '' TERM\nsh -c 'trap \"\" TERM; sleep 300' &\necho $! > '{}'\nsleep 300\n",
            pidfile.display()
        );
        argv(&["sh", "-c", &script])
    }

    /// Whether `pid` is still a runnable process: gone or a zombie (killed,
    /// awaiting reap) both read as dead.
    pub fn process_alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let state = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .unwrap_or("");
                state != "Z"
            }
            Err(_) => false,
        }
    }

    pub fn wait_dead(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !process_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        !process_alive(pid)
    }

    pub fn read_pid(pidfile: &Path, timeout: Duration) -> u32 {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(pidfile)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("grandchild pid file never appeared");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use flywheel_orchestrator::supervision_policy::SupervisionBudget;
    use std::collections::BTreeSet;
    use std::fs;
    use tempfile::TempDir;

    fn policy(max_respawns: i64, window_seconds: f64) -> SupervisionPolicy {
        // A frozen clock so the window never decays under the test.
        SupervisionPolicy::with_clock(
            SupervisionBudget::new(max_respawns, window_seconds).unwrap(),
            Box::new(|| 0.0),
        )
    }

    fn supervisor(tmp: &TempDir, argv: Vec<String>) -> WorkerSupervisor {
        WorkerSupervisor::new(tmp.path().join("absent.sqlite"))
            .with_log_dir(tmp.path().join("logs"))
            .with_spawn_argv(argv)
    }

    fn logs(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(WORKER_LOG_PREFIX))
            })
            .collect();
        paths.sort();
        paths
    }

    fn seed_lease(db: &Path, task_id: &str, worker_id: &str, lease_seconds: f64) {
        let claims = SqliteClaimStore::open(db).unwrap();
        claims
            .acquire_claim(
                task_id,
                worker_id,
                Utc::now(),
                lease_seconds,
                &BTreeSet::new(),
            )
            .unwrap()
            .expect("lease acquired");
    }

    // --- has_live_lease ------------------------------------------------------

    #[test]
    fn has_live_lease_is_false_for_a_missing_db() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            has_live_lease(&tmp.path().join("nope.sqlite"), Utc::now()),
            Ok(false)
        );
    }

    #[test]
    fn has_live_lease_is_true_for_an_unexpired_lease_and_false_once_it_lapses() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("store.sqlite");
        seed_lease(&db, "task-a", "worker-1", 60.0);
        assert_eq!(has_live_lease(&db, Utc::now()), Ok(true));
        let later = Utc::now() + chrono::Duration::seconds(120);
        assert_eq!(has_live_lease(&db, later), Ok(false));
    }

    // --- spawn argv ----------------------------------------------------------

    #[test]
    fn default_spawn_argv_targets_the_worker_verb() {
        let argv = build_default_spawn_argv(
            Path::new("/opt/fw"),
            Path::new("/tmp/db.sqlite"),
            None,
            None,
        );
        assert_eq!(
            argv,
            argv_of(&["/opt/fw", "worker", "--db", "/tmp/db.sqlite"])
        );
        assert!(!argv.iter().any(|a| a == "--model"));
    }

    #[test]
    fn default_spawn_argv_forwards_tasks_dir_and_model() {
        let argv = build_default_spawn_argv(
            Path::new("fw"),
            Path::new("db"),
            Some(Path::new("/t/tasks")),
            Some("claude-opus-4"),
        );
        assert_eq!(
            argv,
            argv_of(&[
                "fw",
                "worker",
                "--db",
                "db",
                "--tasks-dir",
                "/t/tasks",
                "--model",
                "claude-opus-4"
            ])
        );
    }

    fn argv_of(parts: &[&str]) -> Vec<String> {
        argv(parts)
    }

    // --- death reasons -------------------------------------------------------

    #[test]
    fn read_reason_returns_the_last_meaningful_line() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("s.log");
        let policy_error = "flywheel worker: policy error: configured landing base 'main' is \
                            the operator's currently-checked-out branch";
        fs::write(
            &log,
            format!(
                "<frozen runpy>:128: RuntimeWarning: 'x' found in sys.modules ...\n{policy_error}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            read_supervised_death_reason(Some(&log)).as_deref(),
            Some(policy_error)
        );
    }

    #[test]
    fn read_reason_caps_length_and_handles_missing_or_empty_logs() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("long.log");
        fs::write(&log, "x".repeat(500)).unwrap();
        let reason = read_supervised_death_reason(Some(&log)).unwrap();
        assert_eq!(reason.chars().count(), DEATH_REASON_MAX_LEN);
        assert!(reason.ends_with('\u{2026}'));
        assert_eq!(read_supervised_death_reason(None), None);
        assert_eq!(
            read_supervised_death_reason(Some(&tmp.path().join("missing.log"))),
            None
        );
        let empty = tmp.path().join("empty.log");
        fs::write(&empty, "\n  \n").unwrap();
        assert_eq!(read_supervised_death_reason(Some(&empty)), None);
    }

    #[test]
    fn format_dead_message_includes_the_reason_when_present() {
        assert_eq!(format_dead_message(Some(2), None), "exit=2");
        assert_eq!(format_dead_message(Some(2), Some("")), "exit=2");
        assert_eq!(
            format_dead_message(Some(2), Some("policy error")),
            "exit=2: policy error"
        );
        assert_eq!(format_dead_message(None, None), "exit=None");
    }

    // --- log reclaim ---------------------------------------------------------

    fn write_log(dir: &Path, seq: u32, size: usize, age_seconds: u64) -> PathBuf {
        let path = dir.join(format!("{WORKER_LOG_PREFIX}20260101T000000-{seq:04}.log"));
        fs::write(&path, "x".repeat(size)).unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + age_seconds * 100);
        File::open(&path).unwrap().set_modified(mtime).unwrap();
        path
    }

    fn footprint(dir: &Path) -> u64 {
        logs(dir)
            .iter()
            .map(|p| fs::metadata(p).unwrap().len())
            .sum()
    }

    #[test]
    fn reclaim_holds_footprint_at_or_under_the_ceiling_oldest_first() {
        let tmp = TempDir::new().unwrap();
        for seq in 0..5 {
            write_log(tmp.path(), seq, 100, u64::from(seq));
        }
        reclaim_supervisor_logs(tmp.path(), WORKER_LOG_PREFIX, 250);
        assert!(footprint(tmp.path()) <= 250);
        let survivors = logs(tmp.path());
        assert_eq!(survivors.len(), 2);
        assert!(survivors[1].ends_with(format!("{WORKER_LOG_PREFIX}20260101T000000-0004.log")));
    }

    #[test]
    fn reclaim_never_empties_the_directory_and_ignores_other_files() {
        let tmp = TempDir::new().unwrap();
        write_log(tmp.path(), 0, 1000, 0);
        reclaim_supervisor_logs(tmp.path(), WORKER_LOG_PREFIX, 10);
        assert_eq!(logs(tmp.path()).len(), 1, "a lone oversized log survives");

        write_log(tmp.path(), 1, 1000, 1);
        fs::write(tmp.path().join("activity.json"), "{}").unwrap();
        fs::write(
            tmp.path().join("other-20260101T000000-0000.log"),
            "y".repeat(50),
        )
        .unwrap();
        reclaim_supervisor_logs(tmp.path(), WORKER_LOG_PREFIX, 10);
        let survivors = logs(tmp.path());
        assert_eq!(survivors.len(), 1);
        assert!(survivors[0].ends_with(format!("{WORKER_LOG_PREFIX}20260101T000000-0001.log")));
        assert!(tmp.path().join("activity.json").exists());
        assert!(tmp.path().join("other-20260101T000000-0000.log").exists());
    }

    #[test]
    fn repeated_spawns_hold_the_directory_under_the_ceiling() {
        let tmp = TempDir::new().unwrap();
        let mut process = SupervisedProcess::new(
            "worker",
            tmp.path().to_path_buf(),
            WORKER_LOG_PREFIX,
            400,
            None,
        );
        let payload = "y".repeat(300);
        for _ in 0..6 {
            let mut log = process.open_log().unwrap();
            assert!(footprint(tmp.path()) <= 400);
            io::Write::write_all(&mut log, payload.as_bytes()).unwrap();
        }
        let survivors = logs(tmp.path());
        assert!(!survivors.is_empty());
        assert_eq!(
            fs::read_to_string(survivors.last().unwrap()).unwrap(),
            payload
        );
    }

    // --- ownership / quit path ----------------------------------------------

    #[test]
    fn status_is_none_for_a_fresh_store() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, sleep_argv(60));
        assert_eq!(sup.status().unwrap().state, DaemonState::None);
        assert_eq!(sup.owns_supervised_child(), Ok(false));
    }

    #[test]
    fn start_spawns_a_supervised_child_idempotently() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, sleep_argv(60));
        let status = sup.start().unwrap();
        assert_eq!(status.state, DaemonState::Supervised);
        let pid = status.pid.unwrap();
        assert!(process_alive(pid));
        let again = sup.start().unwrap();
        assert_eq!(again.state, DaemonState::Supervised);
        assert_eq!(again.pid, Some(pid));
        assert_eq!(logs(&tmp.path().join("logs")).len(), 1);
        assert!(sup.stop(Duration::from_secs(2)));
        assert!(wait_dead(pid, Duration::from_secs(5)));
    }

    #[test]
    fn start_does_not_spawn_when_a_lease_is_live() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("store.sqlite");
        seed_lease(&db, "task-detached", "worker-external", 60.0);
        let mut sup = WorkerSupervisor::new(&db)
            .with_log_dir(tmp.path().join("logs"))
            .with_spawn_argv(sleep_argv(60));
        assert_eq!(sup.start().unwrap().state, DaemonState::Detached);
        assert_eq!(sup.owns_supervised_child(), Ok(false));
        assert!(logs(&tmp.path().join("logs")).is_empty());
    }

    #[test]
    fn start_after_a_death_still_honors_a_live_lease() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("store.sqlite");
        let mut sup = WorkerSupervisor::new(&db)
            .with_log_dir(tmp.path().join("logs"))
            .with_spawn_argv(exit_argv(0));
        assert_eq!(sup.start().unwrap().state, DaemonState::Supervised);
        assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
        assert_eq!(sup.status().unwrap().state, DaemonState::Dead);
        seed_lease(&db, "task-x", "worker-peer", 60.0);
        assert_eq!(sup.start().unwrap().state, DaemonState::Detached);
        assert_eq!(sup.owns_supervised_child(), Ok(false));
    }

    #[test]
    fn detach_keeps_the_child_alive() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, sleep_argv(60));
        let pid = sup.start().unwrap().pid.unwrap();
        sup.detach();
        assert_eq!(sup.status().unwrap().state, DaemonState::None);
        std::thread::sleep(Duration::from_millis(200));
        assert!(process_alive(pid), "detached child must keep running");
        // Clean up the orphan ourselves.
        let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        assert!(
            !sup.stop(Duration::from_secs(1)),
            "stop after detach is a no-op"
        );
    }

    #[test]
    fn stop_terminates_and_reaps_and_is_a_noop_without_a_child() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, sleep_argv(60));
        assert!(!sup.stop(Duration::from_secs(1)));
        let pid = sup.start().unwrap().pid.unwrap();
        assert!(sup.stop(Duration::from_secs(5)));
        assert!(wait_dead(pid, Duration::from_secs(5)));
        let dead = sup.status().unwrap();
        assert_eq!(dead.state, DaemonState::Dead);
        assert_eq!(dead.pid, Some(pid));
        assert_eq!(dead.message.as_deref(), Some("exit=-15"));
        assert!(!sup.stop(Duration::from_secs(1)));
    }

    #[test]
    fn status_reports_dead_with_the_exit_code_after_an_unexpected_exit() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, exit_argv(7));
        let pid = sup.start().unwrap().pid.unwrap();
        assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
        let dead = sup.status().unwrap();
        assert_eq!(dead.state, DaemonState::Dead);
        assert_eq!(dead.pid, Some(pid));
        assert_eq!(dead.message.as_deref(), Some("exit=7"));
        assert_eq!(sup.death_reason().as_deref(), Some("exit=7"));
        // The death stays visible across polls until the operator restarts.
        assert_eq!(sup.status().unwrap().state, DaemonState::Dead);
    }

    #[test]
    fn dead_status_surfaces_the_captured_reason_and_restart_clears_it() {
        let tmp = TempDir::new().unwrap();
        let reason = "flywheel worker: policy error: configured landing base is checked out";
        let mut sup = supervisor(&tmp, dying_argv(reason, 2));
        sup.start().unwrap();
        assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
        let dead = sup.status().unwrap();
        assert_eq!(dead.state, DaemonState::Dead);
        assert_eq!(dead.message.as_deref(), Some(&*format!("exit=2: {reason}")));

        sup = sup.with_spawn_argv(sleep_argv(60));
        let respawned = sup.start().unwrap();
        assert_eq!(respawned.state, DaemonState::Supervised);
        assert_eq!(respawned.message, None);
        assert_eq!(sup.owns_supervised_child(), Ok(true));
        // The previous death is still queryable after the restart.
        assert_eq!(
            sup.death_reason().as_deref(),
            Some(&*format!("exit=2: {reason}"))
        );
        assert!(sup.stop(Duration::from_secs(2)));
    }

    #[test]
    fn start_failure_surfaces_as_error_until_a_successful_start() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist").display().to_string();
        let mut sup = supervisor(&tmp, vec![missing]);
        let status = sup.start().unwrap();
        assert_eq!(status.state, DaemonState::Error);
        assert!(status.message.unwrap().starts_with("spawn failed: "));
        assert_eq!(sup.status().unwrap().state, DaemonState::Error);
        sup = sup.with_spawn_argv(sleep_argv(60));
        assert_eq!(sup.start().unwrap().state, DaemonState::Supervised);
        assert!(sup.stop(Duration::from_secs(2)));
    }

    #[test]
    fn unwritable_log_dir_surfaces_as_error() {
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let mut sup = WorkerSupervisor::new(tmp.path().join("absent.sqlite"))
            .with_log_dir(blocker.join("logs"))
            .with_spawn_argv(sleep_argv(60));
        let status = sup.start().unwrap();
        assert_eq!(status.state, DaemonState::Error);
        assert!(
            status
                .message
                .unwrap()
                .starts_with("cannot open worker log: ")
        );
    }

    // --- crash-loop policy ---------------------------------------------------

    #[test]
    fn respawn_within_window_launches_a_real_new_child() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, exit_argv(2)).with_policy(policy(2, 1000.0));
        let first = sup.start().unwrap();
        assert_eq!(first.state, DaemonState::Supervised);
        assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
        let respawned = sup.status().unwrap();
        assert_eq!(respawned.state, DaemonState::Supervised);
        assert_ne!(respawned.pid, first.pid, "a new OS process, not a relabel");
        assert_eq!(sup.death_reason().as_deref(), Some("exit=2"));
        sup.stop(Duration::from_secs(5));
    }

    #[test]
    fn budget_exhausted_latches_dead_after_budget_until_an_operator_start() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, exit_argv(2)).with_policy(policy(2, 1000.0));
        sup.start().unwrap();
        // Deaths 1 and 2 respawn; death 3 exhausts the budget.
        let mut last = DaemonStatus::new(DaemonState::None);
        for _ in 0..3 {
            assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
            last = sup.status().unwrap();
            if last.state != DaemonState::Supervised {
                break;
            }
        }
        assert_eq!(last.state, DaemonState::DeadAfterBudget);
        assert!(last.pid.is_some());
        assert_eq!(last.message.as_deref(), Some("exit=2"));
        assert_eq!(sup.status().unwrap().state, DaemonState::DeadAfterBudget);
        // An operator start re-arms a fresh window.
        sup = sup.with_spawn_argv(sleep_argv(60));
        assert_eq!(sup.start().unwrap().state, DaemonState::Supervised);
        assert!(sup.stop(Duration::from_secs(2)));
    }

    #[test]
    fn budget_zero_never_respawns() {
        let tmp = TempDir::new().unwrap();
        let mut sup = supervisor(&tmp, exit_argv(2)).with_policy(policy(0, 1000.0));
        sup.start().unwrap();
        assert!(sup.wait_for_child_exit(Duration::from_secs(5)));
        let dead = sup.status().unwrap();
        assert_eq!(dead.state, DaemonState::Dead);
        assert_eq!(sup.status().unwrap().state, DaemonState::Dead);
    }

    // --- group shutdown ------------------------------------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn stop_force_kills_a_sigterm_ignoring_group() {
        let tmp = TempDir::new().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");
        let mut sup = supervisor(&tmp, ignore_term_with_child_argv(&pidfile));
        let child_pid = sup.start().unwrap().pid.unwrap();
        let grandchild_pid = read_pid(&pidfile, Duration::from_secs(5));
        assert!(process_alive(child_pid) && process_alive(grandchild_pid));
        // SIGTERM is ignored, so this must escalate to SIGKILL in the window.
        assert!(sup.stop(Duration::from_millis(500)));
        assert!(wait_dead(child_pid, Duration::from_secs(5)));
        assert!(
            wait_dead(grandchild_pid, Duration::from_secs(5)),
            "the group kill must reach the grandchild"
        );
        let dead = sup.status().unwrap();
        assert_eq!(dead.state, DaemonState::Dead);
        assert_eq!(dead.message.as_deref(), Some("exit=-9"));
    }
}
