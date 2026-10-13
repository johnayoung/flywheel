//! Headless supervised autopilot: run the daemon under the shared policy.
//!
//! `flywheel autopilot` ([`crate::autopilot_run`]) is the neverending daemon
//! with no supervisor -- a crash simply ends it. This module is the headless
//! supervised entrypoint (spec 00070, criterion #5): it runs that same
//! daemon as a child under the SAME shared crash-loop budget
//! ([`SupervisionPolicy`]) and the SAME liveness-record adoption
//! (`read_live_activity`) the console autopilot supervisor enforces, so a
//! crashed headless daemon is restarted inside its window and an
//! already-live daemon is adopted rather than duplicated.
//!
//! It is a RUN path, not a cold-start default: nothing here auto-starts
//! autopilot where it was manual-start-only (the unattended-base-branch
//! safety default is unchanged). Invoke it explicitly.
//!
//! The spawned child is the neverending daemon (NO `--once`), placed in its
//! own process group so a Ctrl+C on the launching terminal never races it;
//! a SIGTERM/SIGINT to this supervisor is forwarded to the child's whole
//! group so the daemon shuts down gracefully and no agent/MCP grandchild is
//! orphaned on stop.
//!
//! Every process-level effect sits behind [`SupervisedHost`] so the shell
//! runs hermetically in tests; [`ProductionHost`] is the real one.

use crate::autopilot_activity::{AutopilotActivity, read_live_activity};
use crate::autopilot_run::{
    SIGTERM, SignalHandler, looks_like_negative_number, make_logger, match_long_option,
    parse_float_arg, parse_int_arg, watch_stop_signals,
};
use crate::sources::LogSink;
use crate::supervision_policy::{RespawnDecision, SupervisionBudget, SupervisionPolicy};
use crate::triage_run::{py_float_repr, resolve_repo_root};
use crate::workflow::python_path_str;
use flywheel_core::audit::StopFlag;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The same default crash-loop budget the console supervisors enforce: a
/// handful of respawns inside a rolling window so a transient death
/// self-heals while a persistent boot-loop is contained -- for autopilot,
/// that caps unattended writes to the base branch.
pub const DEFAULT_MAX_RESPAWNS: i64 = 5;
pub const DEFAULT_WINDOW_SECONDS: f64 = 300.0;

/// How long a graceful SIGTERM is given before escalating the child group
/// to SIGKILL, mirroring the console supervisor's stop window so a daemon
/// blocked mid-cycle is still brought down rather than orphaned.
pub const DEFAULT_STOP_TIMEOUT_SECONDS: f64 = 10.0;

/// The prefix every supervisor log line carries.
pub const LOG_PREFIX: &str = "[autopilot-supervised]";

/// The program name the usage/error surface reports.
pub const PROG: &str = "flywheel autopilot-supervised";

/// The usage block argparse prints on a bad argument (80-column wrap).
pub const USAGE: &str = "usage: flywheel autopilot-supervised [-h] [--tasks-dir TASKS_DIR]
                                     [--model MODEL]
                                     [--activity-file ACTIVITY_FILE]
                                     [--max-respawns MAX_RESPAWNS]
                                     [--window-seconds WINDOW_SECONDS]";

const HELP: &str = "usage: flywheel autopilot-supervised [-h] [--tasks-dir TASKS_DIR]
                                     [--model MODEL]
                                     [--activity-file ACTIVITY_FILE]
                                     [--max-respawns MAX_RESPAWNS]
                                     [--window-seconds WINDOW_SECONDS]

Run the autopilot daemon headless under the shared crash-loop budget: restart
a daemon that dies inside its window, adopt an already-live daemon via its
liveness record, and stop respawning once the budget is exhausted.

options:
  -h, --help            show this help message and exit
  --tasks-dir TASKS_DIR
  --model MODEL
  --activity-file ACTIVITY_FILE
                        Path to the daemon's activity/liveness record
                        (defaults to
                        <repo>/.flywheel/logs/autopilot/activity.json).
  --max-respawns MAX_RESPAWNS
                        Deaths tolerated inside the rolling window before
                        giving up.
  --window-seconds WINDOW_SECONDS
                        Rolling crash-loop window in seconds.";

/// The long options in argparse declaration order.
const LONG_OPTIONS: [&str; 6] = [
    "--help",
    "--tasks-dir",
    "--model",
    "--activity-file",
    "--max-respawns",
    "--window-seconds",
];

/// SIGKILL's number on every unix.
const SIGKILL: i32 = 9;

// --- Arguments ------------------------------------------------------------------------

/// The parsed `flywheel autopilot-supervised` command line.
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisedArgs {
    pub tasks_dir: Option<String>,
    pub model: Option<String>,
    pub activity_file: Option<String>,
    pub max_respawns: i64,
    pub window_seconds: f64,
    pub help: bool,
}

impl Default for SupervisedArgs {
    fn default() -> Self {
        Self {
            tasks_dir: None,
            model: None,
            activity_file: None,
            max_respawns: DEFAULT_MAX_RESPAWNS,
            window_seconds: DEFAULT_WINDOW_SECONDS,
            help: false,
        }
    }
}

fn take_value(
    flag: &str,
    inline: Option<&str>,
    iter: &mut std::slice::Iter<'_, String>,
) -> Result<String, String> {
    if let Some(inline) = inline {
        return Ok(inline.to_owned());
    }
    match iter.clone().next() {
        Some(next) if !next.starts_with('-') || looks_like_negative_number(next) => {
            iter.next();
            Ok(next.clone())
        }
        _ => Err(format!("argument {flag}: expected one argument")),
    }
}

/// Parse `argv` as argparse would (see `autopilot_run::parse_args` for the
/// shared rules: `--flag=value`, unique-prefix abbreviations, last value
/// wins, unrecognized tokens collected into one error).
pub fn parse_args(argv: &[String]) -> Result<SupervisedArgs, String> {
    let mut args = SupervisedArgs::default();
    let mut unrecognized: Vec<String> = Vec::new();
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if arg == "-h" {
            args.help = true;
            continue;
        }
        if !arg.starts_with("--") || looks_like_negative_number(arg) {
            unrecognized.push(arg.clone());
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg.as_str(), None),
        };
        let Some(option) = match_long_option(arg, flag, &LONG_OPTIONS)? else {
            unrecognized.push(arg.clone());
            continue;
        };
        match option {
            "--help" => {
                if let Some(value) = inline {
                    return Err(format!(
                        "argument --help: ignored explicit argument '{value}'"
                    ));
                }
                args.help = true;
            }
            "--tasks-dir" => args.tasks_dir = Some(take_value(option, inline, &mut iter)?),
            "--model" => args.model = Some(take_value(option, inline, &mut iter)?),
            "--activity-file" => {
                args.activity_file = Some(take_value(option, inline, &mut iter)?);
            }
            "--max-respawns" => {
                let raw = take_value(option, inline, &mut iter)?;
                args.max_respawns = parse_int_arg(option, &raw)?;
            }
            "--window-seconds" => {
                let raw = take_value(option, inline, &mut iter)?;
                args.window_seconds = parse_float_arg(option, &raw)?;
            }
            other => unrecognized.push(other.to_owned()),
        }
    }
    if !unrecognized.is_empty() {
        return Err(format!(
            "unrecognized arguments: {}",
            unrecognized.join(" ")
        ));
    }
    Ok(args)
}

// --- The daemon child -------------------------------------------------------------------

/// Compose the daemon child's argv from `program` (the command that runs
/// the daemon: the product binary's `autopilot` verb) and the flags.
///
/// Carries NO `--once` so the child is the neverending loop, and pins the
/// `--activity-file` so the daemon and this supervisor agree on the one
/// liveness record (the same contract the console supervisor uses).
pub fn build_daemon_argv(
    program: &[String],
    tasks_dir: Option<&str>,
    model: Option<&str>,
    activity_file: &Path,
) -> Vec<String> {
    let mut argv: Vec<String> = program.to_vec();
    argv.push("--activity-file".to_owned());
    argv.push(activity_file.to_string_lossy().into_owned());
    if let Some(tasks_dir) = tasks_dir {
        argv.push("--tasks-dir".to_owned());
        argv.push(tasks_dir.to_owned());
    }
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    argv
}

/// The command that runs the daemon child in production: this executable's
/// `autopilot` verb.
pub fn default_daemon_program() -> Result<Vec<String>, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot resolve the running executable: {err}"))?;
    Ok(vec![
        exe.to_string_lossy().into_owned(),
        "autopilot".to_owned(),
    ])
}

/// One spawned autopilot daemon the headless supervisor waits on.
///
/// A thin handle over the running daemon so [`run_supervised`] is agnostic
/// to how it was launched: `wait` blocks until the child exits and returns
/// its exit code (a crash OR a forwarded-signal graceful shutdown; a
/// signal-killed child reports the negated signal number, as Python did),
/// and `terminate` brings the child and everything it spawned down
/// orphan-free (SIGTERM the process group, escalate to SIGKILL). Both are
/// safe to call from any thread: the stop-signal handler terminates the
/// child while the main thread is blocked in `wait`. Tests inject scripted
/// closures so respawn is deterministic; production wraps a real process
/// ([`spawn_daemon`]).
pub struct SupervisedChild {
    pid: i64,
    wait: Box<dyn Fn() -> i32 + Send + Sync>,
    terminate: Box<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for SupervisedChild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupervisedChild")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl SupervisedChild {
    /// A child needing no orphan cleanup (`terminate` is a no-op).
    pub fn new(pid: i64, wait: impl Fn() -> i32 + Send + Sync + 'static) -> Self {
        Self {
            pid,
            wait: Box::new(wait),
            terminate: Box::new(|| {}),
        }
    }

    /// Attach the orphan-free `terminate`.
    pub fn with_terminate(mut self, terminate: impl Fn() + Send + Sync + 'static) -> Self {
        self.terminate = Box::new(terminate);
        self
    }

    pub fn pid(&self) -> i64 {
        self.pid
    }

    /// Block until the child exits; its exit code.
    pub fn wait(&self) -> i32 {
        (self.wait)()
    }

    /// Bring the child and its whole group down.
    pub fn terminate(&self) {
        (self.terminate)()
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn killpg(pgrp: i32, sig: i32) -> i32;
}

/// Signal the child's whole process group, ignoring an already-gone group.
///
/// The daemon is spawned as its own process-group leader, so its pgid
/// equals its pid and the signal reaches every agent/MCP descendant it
/// spawned -- signaling only the pid would orphan those grandchildren.
#[cfg(unix)]
fn signal_group(pid: i32, sig: i32) {
    // SAFETY: killpg(2) takes two plain integers and touches no memory. The
    // group id is the child's own pid because the child was spawned with
    // process_group(0), so no unrelated process group can be targeted; a
    // group that is already gone reports ESRCH, which is ignored.
    let _ = unsafe { killpg(pid, sig) };
}

/// Python's `Popen.returncode`: the exit code, or the negated signal number
/// for a signal-killed child.
#[cfg(unix)]
fn python_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code,
        None => -status.signal().unwrap_or(0),
    }
}

#[cfg(not(unix))]
fn python_exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// A real daemon process: a reaping `wait` and a group-killing `terminate`
/// that may run concurrently on different threads.
struct ProcessChild {
    pid: i32,
    child: Mutex<std::process::Child>,
    exit_code: Mutex<Option<i32>>,
    exited: StopFlag,
    stop_timeout: Duration,
}

impl ProcessChild {
    fn record_exit(&self, status: std::process::ExitStatus) -> i32 {
        let code = python_exit_code(status);
        *self.exit_code.lock().unwrap_or_else(|e| e.into_inner()) = Some(code);
        self.exited.set();
        code
    }

    fn recorded(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Block until the child exits (reaping it), or return the recorded
    /// code if another thread already reaped it.
    fn wait(&self) -> i32 {
        if let Some(code) = self.recorded() {
            return code;
        }
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(code) = self.recorded() {
            return code;
        }
        match child.wait() {
            Ok(status) => self.record_exit(status),
            Err(_) => {
                // Reaped out from under us: report whatever was recorded.
                self.exited.set();
                self.recorded().unwrap_or(1)
            }
        }
    }

    /// Wait up to `timeout` for the child to exit: reap it here when no
    /// thread holds the process (nobody is blocked in `wait`), otherwise
    /// watch the exit flag that thread sets.
    fn await_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.exited.is_set() {
                return true;
            }
            if let Ok(mut child) = self.child.try_lock() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        self.record_exit(status);
                        return true;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        self.exited.set();
                        return true;
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.exited.is_set();
            }
            if self.exited.wait(remaining.min(Duration::from_millis(50))) {
                return true;
            }
        }
    }

    /// SIGTERM the group, escalate to SIGKILL after the stop window; a
    /// no-op once the child has exited.
    fn terminate(&self) {
        if self.exited.is_set() {
            return;
        }
        #[cfg(unix)]
        {
            signal_group(self.pid, SIGTERM);
            if !self.await_exit(self.stop_timeout) {
                signal_group(self.pid, SIGKILL);
                self.await_exit(self.stop_timeout);
            }
        }
        #[cfg(not(unix))]
        {
            if let Ok(mut child) = self.child.try_lock() {
                let _ = child.kill();
            }
            self.await_exit(self.stop_timeout);
        }
    }
}

/// Open the per-spawn daemon log (`autopilot-supervised-<UTC stamp>.log`)
/// under `log_dir`, creating the directory as needed.
fn open_log(log_dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(log_dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join(format!("autopilot-supervised-{stamp}.log")))
}

/// Spawn one daemon child and wrap it as an orphan-safe [`SupervisedChild`].
///
/// The child is placed in its own process group so a Ctrl+C on the
/// launching terminal never reaches it and only an explicit stop signals
/// it; its stdout and stderr land in a fresh per-spawn log. `wait` blocks
/// on the child; `terminate` brings the whole group down (SIGTERM, then
/// SIGKILL after `stop_timeout` seconds) so a daemon blocked mid-cycle is
/// never left orphaned. The error is the message to print before exiting.
pub fn spawn_daemon(
    daemon_argv: &[String],
    log_dir: &Path,
    stop_timeout: f64,
) -> Result<Arc<SupervisedChild>, String> {
    let (program, rest) = daemon_argv
        .split_first()
        .ok_or_else(|| "daemon argv is empty".to_owned())?;
    let log = open_log(log_dir).map_err(|err| {
        format!(
            "cannot open the daemon log under {}: {err}",
            log_dir.display()
        )
    })?;
    let log_err = log
        .try_clone()
        .map_err(|err| format!("cannot duplicate the daemon log handle: {err}"))?;
    let mut command = std::process::Command::new(program);
    command
        .args(rest)
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|err| format!("cannot spawn the autopilot daemon ({program}): {err}"))?;
    let pid = i32::try_from(child.id()).unwrap_or(i32::MAX);
    let stop_timeout = Duration::try_from_secs_f64(stop_timeout.max(0.0))
        .unwrap_or(Duration::from_secs(u32::MAX as u64));
    let process = Arc::new(ProcessChild {
        pid,
        child: Mutex::new(child),
        exit_code: Mutex::new(None),
        exited: StopFlag::new(),
        stop_timeout,
    });
    let waiter = process.clone();
    let terminator = process.clone();
    Ok(Arc::new(
        SupervisedChild::new(i64::from(pid), move || waiter.wait())
            .with_terminate(move || terminator.terminate()),
    ))
}

// --- The supervised run ---------------------------------------------------------------------

/// How a headless supervised run ended.
///
/// `Adopted` -- a live daemon already held a fresh liveness record at
/// startup, so the entrypoint adopted it and spawned nothing (spawn count
/// 0). `Stopped` -- an external stop signal ended the run after a graceful
/// child shutdown. `DeadAfterBudget` -- the daemon kept dying and exhausted
/// the shared windowed crash-loop budget, so the entrypoint stopped
/// respawning and latched this loud terminal state instead of
/// crash-looping forever (the unattended-base-branch interlock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisedOutcome {
    Adopted,
    Stopped,
    DeadAfterBudget,
}

impl SupervisedOutcome {
    /// The wire spelling (`"adopted"` / `"stopped"` / `"dead_after_budget"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            SupervisedOutcome::Adopted => "adopted",
            SupervisedOutcome::Stopped => "stopped",
            SupervisedOutcome::DeadAfterBudget => "dead_after_budget",
        }
    }
}

impl fmt::Display for SupervisedOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The terminal snapshot of a [`run_supervised`] call.
///
/// `spawn_count` is the number of daemon children actually launched (0 on
/// adoption); `deaths` counts children that died on their own (a
/// forwarded-signal shutdown is not a death); `adopted_pid` names the
/// foreign daemon on `Adopted`; `last_pid` is the most recently spawned
/// child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisedRunResult {
    pub outcome: SupervisedOutcome,
    pub spawn_count: u32,
    pub deaths: u32,
    pub adopted_pid: Option<i64>,
    pub last_pid: Option<i64>,
}

/// Launches one daemon child; the error is the message to print.
pub type SpawnFn<'a> = &'a dyn Fn() -> Result<Arc<SupervisedChild>, String>;

/// The optional observers of [`run_supervised`].
#[derive(Default, Clone, Copy)]
pub struct SupervisedHooks<'a> {
    pub on_spawn: Option<&'a dyn Fn(&SupervisedChild)>,
    pub on_death: Option<&'a dyn Fn(i32, RespawnDecision)>,
    pub on_adopt: Option<&'a dyn Fn(&AutopilotActivity)>,
    /// A test-only safety bound (mirroring the daemon loop's
    /// `max_cycles`); production leaves it `None`.
    pub max_spawns: Option<u32>,
}

impl fmt::Debug for SupervisedHooks<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupervisedHooks")
            .field("on_spawn", &self.on_spawn.is_some())
            .field("on_death", &self.on_death.is_some())
            .field("on_adopt", &self.on_adopt.is_some())
            .field("max_spawns", &self.max_spawns)
            .finish()
    }
}

/// Run the autopilot daemon headless under the shared crash-loop policy.
///
/// Adoption first (criterion #6): if `read_liveness` returns a live record
/// at startup -- a console-detached or peer daemon already running -- the
/// entrypoint adopts it and returns `Adopted` with `spawn_count == 0`,
/// spawning no duplicate. A stale record reads back as `None` and does not
/// block a spawn.
///
/// Otherwise it spawns the daemon and waits. When a child dies on its own
/// it charges the death against the shared policy: `Respawn` while inside
/// budget launches a fresh child and the loop continues (criterion #5);
/// `Exhausted` once the windowed budget is spent stops respawning and
/// returns `DeadAfterBudget`. A child that exits after `should_stop`
/// flipped (a forwarded graceful shutdown) is not counted as a death and
/// is not respawned, and a still-held child is terminated on the way out
/// so the wrapper never leaves an orphan on stop. A spawn failure is
/// returned as `Err` after the same cleanup.
pub fn run_supervised(
    spawn: SpawnFn<'_>,
    policy: &mut SupervisionPolicy,
    should_stop: &dyn Fn() -> bool,
    read_liveness: &dyn Fn() -> Option<AutopilotActivity>,
    hooks: SupervisedHooks<'_>,
) -> Result<SupervisedRunResult, String> {
    if let Some(live) = read_liveness() {
        if let Some(on_adopt) = hooks.on_adopt {
            on_adopt(&live);
        }
        return Ok(SupervisedRunResult {
            outcome: SupervisedOutcome::Adopted,
            spawn_count: 0,
            deaths: 0,
            adopted_pid: Some(live.pid),
            last_pid: None,
        });
    }

    let mut spawn_count = 0u32;
    let mut deaths = 0u32;
    let mut last_pid: Option<i64> = None;
    let mut child: Option<Arc<SupervisedChild>> = None;
    let outcome = loop {
        if should_stop() {
            break Ok(SupervisedOutcome::Stopped);
        }
        if hooks.max_spawns.is_some_and(|max| spawn_count >= max) {
            break Ok(SupervisedOutcome::Stopped);
        }
        let next = match spawn() {
            Ok(next) => next,
            Err(err) => break Err(err),
        };
        spawn_count += 1;
        last_pid = Some(next.pid());
        child = Some(next.clone());
        if let Some(on_spawn) = hooks.on_spawn {
            on_spawn(&next);
        }
        let exit_code = next.wait();
        if should_stop() {
            // A forwarded stop signal brought the child down gracefully:
            // not a crash, so do not charge the budget and do not respawn.
            break Ok(SupervisedOutcome::Stopped);
        }
        deaths += 1;
        let decision = policy.record_death();
        if let Some(on_death) = hooks.on_death {
            on_death(exit_code, decision);
        }
        if decision == RespawnDecision::Exhausted {
            break Ok(SupervisedOutcome::DeadAfterBudget);
        }
        // Respawn: the dead child was reaped by its own wait(); drop the
        // handle so the orphan-free cleanup never re-signals a dead group.
        child = None;
    };
    if let Some(child) = child {
        child.terminate();
    }
    outcome.map(|outcome| SupervisedRunResult {
        outcome,
        spawn_count,
        deaths,
        adopted_pid: None,
        last_pid,
    })
}

// --- The host seam -----------------------------------------------------------------------------

/// The process-level effects the headless supervisor performs, behind one
/// seam so [`run_main`] is hermetic in tests.
pub trait SupervisedHost: Send + Sync {
    /// The raw stderr line sink (no prefix).
    fn stderr(&self) -> LogSink;

    /// The repo root; the error is the message to print before exiting 1.
    fn repo_root(&self) -> Result<PathBuf, String>;

    /// The command (program plus leading args) that runs the daemon child.
    fn daemon_program(&self) -> Result<Vec<String>, String> {
        default_daemon_program()
    }

    /// Launch one daemon child.
    fn spawn(
        &self,
        daemon_argv: &[String],
        log_dir: &Path,
        stop_timeout: f64,
    ) -> Result<Arc<SupervisedChild>, String>;

    /// The liveness record at `activity_path`, only if it reads live now.
    fn read_liveness(&self, activity_path: &Path) -> Option<AutopilotActivity> {
        read_live_activity(Some(activity_path), None)
    }

    /// The policy enforcing `budget` (a deterministic clock in tests).
    fn supervision_policy(&self, budget: SupervisionBudget) -> SupervisionPolicy {
        SupervisionPolicy::new(budget)
    }

    /// Arm SIGTERM/SIGINT to call `on_signal(signum)`.
    fn arm_stop_signals(&self, on_signal: SignalHandler, log: &LogSink);
}

/// The real host: stderr, `git`, real daemon processes, and OS signal
/// handlers driven by a tokio runtime.
pub struct ProductionHost {
    runtime: Arc<tokio::runtime::Runtime>,
}

impl fmt::Debug for ProductionHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProductionHost").finish_non_exhaustive()
    }
}

impl ProductionHost {
    /// A host over a fresh multi-thread tokio runtime.
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            runtime: Arc::new(runtime),
        })
    }

    /// A host over an existing runtime.
    pub fn with_runtime(runtime: Arc<tokio::runtime::Runtime>) -> Self {
        Self { runtime }
    }
}

impl SupervisedHost for ProductionHost {
    fn stderr(&self) -> LogSink {
        Arc::new(|line: &str| eprintln!("{line}"))
    }

    fn repo_root(&self) -> Result<PathBuf, String> {
        resolve_repo_root()
    }

    fn spawn(
        &self,
        daemon_argv: &[String],
        log_dir: &Path,
        stop_timeout: f64,
    ) -> Result<Arc<SupervisedChild>, String> {
        spawn_daemon(daemon_argv, log_dir, stop_timeout)
    }

    fn arm_stop_signals(&self, on_signal: SignalHandler, log: &LogSink) {
        let log = log.clone();
        self.runtime
            .spawn(async move { watch_stop_signals(on_signal, log).await });
    }
}

// --- main -------------------------------------------------------------------------------------

/// Run the headless supervisor with the real host; returns the exit code.
pub fn main(argv: &[String]) -> i32 {
    match ProductionHost::new() {
        Ok(host) => run_main(argv, &host),
        Err(err) => {
            eprintln!("{PROG}: cannot start the runtime: {err}");
            1
        }
    }
}

/// Run the autopilot daemon headless under the shared supervision policy.
///
/// Returns 0 on a clean stop or an adoption, 1 when the daemon exhausted
/// the crash-loop budget (the loud DEAD-after-budget terminal state) or
/// the repo root / daemon could not be resolved or spawned, and 2 on a bad
/// argument or budget value.
pub fn run_main(argv: &[String], host: &dyn SupervisedHost) -> i32 {
    let stderr = host.stderr();
    let args = match parse_args(argv) {
        Ok(args) => args,
        Err(message) => {
            stderr(USAGE);
            stderr(&format!("{PROG}: error: {message}"));
            return 2;
        }
    };
    if args.help {
        println!("{HELP}");
        return 0;
    }
    let log = make_logger(LOG_PREFIX, stderr.clone());

    if args.max_respawns < 0 {
        stderr("autopilot-supervised: --max-respawns must be >= 0");
        return 2;
    }
    if args.window_seconds.is_nan() || args.window_seconds <= 0.0 {
        stderr("autopilot-supervised: --window-seconds must be > 0");
        return 2;
    }

    let repo_root = match host.repo_root() {
        Ok(root) => root,
        Err(message) => {
            stderr(&message);
            return 1;
        }
    };
    let activity_path = match args.activity_file.as_deref().filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => repo_root
            .join(".flywheel")
            .join("logs")
            .join("autopilot")
            .join("activity.json"),
    };
    let log_dir = activity_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let program = match host.daemon_program() {
        Ok(program) => program,
        Err(message) => {
            stderr(&format!("autopilot-supervised: {message}"));
            return 1;
        }
    };
    let daemon_argv = build_daemon_argv(
        &program,
        args.tasks_dir.as_deref(),
        args.model.as_deref(),
        &activity_path,
    );
    let budget = match SupervisionBudget::new(args.max_respawns, args.window_seconds) {
        Ok(budget) => budget,
        Err(err) => {
            stderr(&format!("autopilot-supervised: {err}"));
            return 2;
        }
    };
    let mut policy = host.supervision_policy(budget);

    let stop = StopFlag::new();
    let current: Arc<Mutex<Option<Arc<SupervisedChild>>>> = Arc::new(Mutex::new(None));
    {
        let stop = stop.clone();
        let current = current.clone();
        let signal_log = log.clone();
        let on_signal: SignalHandler = Arc::new(move |signum: i32| {
            stop.set();
            let child = current.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(child) = child {
                child.terminate();
            }
            signal_log(&format!(
                "stop signal {signum}; shutting the daemon down and exiting"
            ));
        });
        host.arm_stop_signals(on_signal, &log);
    }

    let spawn = || {
        let child = host.spawn(&daemon_argv, &log_dir, DEFAULT_STOP_TIMEOUT_SECONDS)?;
        *current.lock().unwrap_or_else(|e| e.into_inner()) = Some(child.clone());
        Ok(child)
    };
    let on_spawn = |child: &SupervisedChild| {
        log(&format!("spawned autopilot daemon pid={}", child.pid()));
    };
    let on_death = |exit_code: i32, decision: RespawnDecision| {
        log(&format!(
            "daemon exited (exit={exit_code}); decision={decision}"
        ));
    };
    let on_adopt = |record: &AutopilotActivity| {
        log(&format!(
            "adopted live autopilot daemon pid={}; spawning nothing",
            record.pid
        ));
    };

    log(&format!(
        "headless supervised autopilot repo={} budget={}/{}s pid={}",
        python_path_str(&repo_root),
        args.max_respawns,
        py_float_repr(args.window_seconds),
        std::process::id()
    ));
    let result = run_supervised(
        &spawn,
        &mut policy,
        &|| stop.is_set(),
        &|| host.read_liveness(&activity_path),
        SupervisedHooks {
            on_spawn: Some(&on_spawn),
            on_death: Some(&on_death),
            on_adopt: Some(&on_adopt),
            max_spawns: None,
        },
    );
    let result = match result {
        Ok(result) => result,
        Err(message) => {
            stderr(&format!("autopilot-supervised: {message}"));
            return 1;
        }
    };

    match result.outcome {
        SupervisedOutcome::DeadAfterBudget => {
            log(&format!(
                "daemon exhausted the crash-loop budget after {} death(s) in the window; \
                 stopped respawning (dead_after_budget).",
                result.deaths
            ));
            1
        }
        SupervisedOutcome::Adopted => {
            log(&format!(
                "exiting; adopted daemon pid={} keeps running.",
                result.adopted_pid.unwrap_or_default()
            ));
            0
        }
        SupervisedOutcome::Stopped => {
            log(&format!("stopped after {} spawn(s).", result.spawn_count));
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot_activity::PHASE_IDLE;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    const INTERVAL: f64 = 300.0;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    fn budget(max_respawns: i64) -> SupervisionBudget {
        SupervisionBudget::new(max_respawns, INTERVAL).unwrap()
    }

    /// A policy on a scripted clock that advances one second per death.
    fn ticking_policy(max_respawns: i64) -> SupervisionPolicy {
        let tick = AtomicU32::new(0);
        SupervisionPolicy::with_clock(
            budget(max_respawns),
            Box::new(move || f64::from(tick.fetch_add(1, Ordering::SeqCst))),
        )
    }

    fn live_record(pid: i64) -> AutopilotActivity {
        AutopilotActivity {
            expires_at: Some(1000.0 + INTERVAL),
            ..AutopilotActivity::new(pid, PHASE_IDLE, 2, 1000.0, INTERVAL)
        }
    }

    // --- argument parsing ------------------------------------------------------------------

    #[test]
    fn parse_args_defaults_and_flags() {
        assert_eq!(parse_args(&[]).unwrap(), SupervisedArgs::default());
        let parsed = parse_args(&argv(&[
            "--tasks-dir",
            "t",
            "--model=m",
            "--activity-file",
            "a.json",
            "--max-respawns",
            "2",
            "--window-seconds",
            "30",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            SupervisedArgs {
                tasks_dir: Some("t".into()),
                model: Some("m".into()),
                activity_file: Some("a.json".into()),
                max_respawns: 2,
                window_seconds: 30.0,
                help: false,
            }
        );
        assert_eq!(
            parse_args(&argv(&["--max", "-1"])).unwrap().max_respawns,
            -1
        );
        assert!(parse_args(&argv(&["-h"])).unwrap().help);
    }

    #[test]
    fn parse_args_errors_match_argparse() {
        assert_eq!(
            parse_args(&argv(&["--max-respawns", "x"])).unwrap_err(),
            "argument --max-respawns: invalid int value: 'x'"
        );
        assert_eq!(
            parse_args(&argv(&["--window-seconds", "y"])).unwrap_err(),
            "argument --window-seconds: invalid float value: 'y'"
        );
        assert_eq!(
            parse_args(&argv(&["--m", "1"])).unwrap_err(),
            "ambiguous option: --m could match --model, --max-respawns"
        );
        assert_eq!(
            parse_args(&argv(&["--bogus"])).unwrap_err(),
            "unrecognized arguments: --bogus"
        );
        assert_eq!(
            parse_args(&argv(&["--model"])).unwrap_err(),
            "argument --model: expected one argument"
        );
    }

    #[test]
    fn daemon_argv_carries_no_once_and_pins_the_activity_file() {
        let program = argv(&["/bin/flywheel", "autopilot"]);
        let bare = build_daemon_argv(&program, None, None, Path::new("/r/act.json"));
        assert_eq!(
            bare,
            argv(&[
                "/bin/flywheel",
                "autopilot",
                "--activity-file",
                "/r/act.json"
            ])
        );
        let full = build_daemon_argv(&program, Some("tasks"), Some("m"), Path::new("a.json"));
        assert_eq!(
            full,
            argv(&[
                "/bin/flywheel",
                "autopilot",
                "--activity-file",
                "a.json",
                "--tasks-dir",
                "tasks",
                "--model",
                "m"
            ])
        );
        assert!(!full.iter().any(|a| a == "--once"));
    }

    // --- headless supervised run ---------------------------------------------------------------

    #[test]
    fn respawn_restarts_a_daemon_that_dies_once() {
        let mut policy = ticking_policy(5);
        let stop = Arc::new(StopFlag::new());
        let spawned = Arc::new(Mutex::new(Vec::new()));
        let spawn = {
            let stop = stop.clone();
            let spawned = spawned.clone();
            move || {
                let idx = spawned.lock().unwrap().len();
                let pid = 1000 + idx as i64;
                spawned.lock().unwrap().push(pid);
                let stop = stop.clone();
                Ok(Arc::new(SupervisedChild::new(pid, move || {
                    if idx == 0 {
                        // The first daemon crashes inside the window.
                        return 1;
                    }
                    // The restarted daemon ends the run the way a forwarded
                    // SIGTERM would -- a graceful stop, not a crash.
                    stop.set();
                    0
                })))
            }
        };
        let result = run_supervised(
            &spawn,
            &mut policy,
            &|| stop.is_set(),
            &|| None,
            SupervisedHooks {
                max_spawns: Some(5),
                ..SupervisedHooks::default()
            },
        )
        .unwrap();
        assert_eq!(*spawned.lock().unwrap(), vec![1000, 1001]);
        assert_eq!(result.spawn_count, 2);
        assert_eq!(result.deaths, 1);
        assert_eq!(result.outcome, SupervisedOutcome::Stopped);
        assert_eq!(result.last_pid, Some(1001));
    }

    #[test]
    fn adopts_live_record_and_spawns_nothing() {
        let mut policy = SupervisionPolicy::new(budget(5));
        let spawned = Cell::new(0);
        let adopted = Mutex::new(Vec::new());
        let spawn = || {
            spawned.set(spawned.get() + 1);
            panic!("must not spawn when a live daemon is adopted")
        };
        let on_adopt = |record: &AutopilotActivity| adopted.lock().unwrap().push(record.pid);
        let result = run_supervised(
            &spawn,
            &mut policy,
            &|| false,
            &|| Some(live_record(9999)),
            SupervisedHooks {
                on_adopt: Some(&on_adopt),
                max_spawns: Some(5),
                ..SupervisedHooks::default()
            },
        )
        .unwrap();
        assert_eq!(result.outcome, SupervisedOutcome::Adopted);
        assert_eq!(result.spawn_count, 0);
        assert_eq!(result.adopted_pid, Some(9999));
        assert_eq!(spawned.get(), 0);
        assert_eq!(*adopted.lock().unwrap(), vec![9999]);
    }

    #[test]
    fn stale_record_does_not_block_a_spawn() {
        let mut policy = SupervisionPolicy::new(budget(5));
        let stop = Arc::new(StopFlag::new());
        let spawned = Mutex::new(Vec::new());
        let spawn = || {
            let pid = 3000 + spawned.lock().unwrap().len() as i64;
            spawned.lock().unwrap().push(pid);
            let stop = stop.clone();
            Ok(Arc::new(SupervisedChild::new(pid, move || {
                stop.set();
                0
            })))
        };
        let result = run_supervised(
            &spawn,
            &mut policy,
            &|| stop.is_set(),
            &|| None,
            SupervisedHooks {
                max_spawns: Some(5),
                ..SupervisedHooks::default()
            },
        )
        .unwrap();
        assert_eq!(*spawned.lock().unwrap(), vec![3000]);
        assert_eq!(result.outcome, SupervisedOutcome::Stopped);
        assert_eq!(result.spawn_count, 1);
    }

    #[test]
    fn budget_exhausted_stops_respawning() {
        let mut policy = ticking_policy(2);
        let spawned = Mutex::new(Vec::new());
        let deaths = Mutex::new(Vec::new());
        let spawn = || {
            let pid = 2000 + spawned.lock().unwrap().len() as i64;
            spawned.lock().unwrap().push(pid);
            Ok(Arc::new(SupervisedChild::new(pid, || 1)))
        };
        let on_death = |code: i32, decision: RespawnDecision| {
            deaths.lock().unwrap().push((code, decision));
        };
        let result = run_supervised(
            &spawn,
            &mut policy,
            &|| false,
            &|| None,
            SupervisedHooks {
                on_death: Some(&on_death),
                max_spawns: Some(10),
                ..SupervisedHooks::default()
            },
        )
        .unwrap();
        // budget max_respawns=2: deaths 1 and 2 respawn, the 3rd exhausts.
        assert_eq!(result.outcome, SupervisedOutcome::DeadAfterBudget);
        assert_eq!(result.deaths, 3);
        assert_eq!(result.spawn_count, 3);
        assert_eq!(*spawned.lock().unwrap(), vec![2000, 2001, 2002]);
        assert_eq!(
            *deaths.lock().unwrap(),
            vec![
                (1, RespawnDecision::Respawn),
                (1, RespawnDecision::Respawn),
                (1, RespawnDecision::Exhausted)
            ]
        );
    }

    #[test]
    fn stop_before_first_spawn_runs_nothing() {
        let mut policy = SupervisionPolicy::new(budget(5));
        let spawn = || panic!("no spawn when stop is set up front");
        let result = run_supervised(
            &spawn,
            &mut policy,
            &|| true,
            &|| None,
            SupervisedHooks::default(),
        )
        .unwrap();
        assert_eq!(result.outcome, SupervisedOutcome::Stopped);
        assert_eq!(result.spawn_count, 0);
    }

    #[test]
    fn terminates_a_live_child_on_stop_orphan_free() {
        let mut policy = SupervisionPolicy::new(budget(5));
        let stop = Arc::new(StopFlag::new());
        let terminated = Arc::new(Mutex::new(Vec::new()));
        let spawn = || {
            let stop = stop.clone();
            let terminated = terminated.clone();
            Ok(Arc::new(
                SupervisedChild::new(4242, move || {
                    // The child is still "alive" when the operator asks the
                    // wrapper to stop; wait returns because the stop was
                    // forwarded to it.
                    stop.set();
                    0
                })
                .with_terminate(move || terminated.lock().unwrap().push(4242)),
            ))
        };
        let result = run_supervised(
            &spawn,
            &mut policy,
            &|| stop.is_set(),
            &|| None,
            SupervisedHooks {
                max_spawns: Some(5),
                ..SupervisedHooks::default()
            },
        )
        .unwrap();
        assert_eq!(result.outcome, SupervisedOutcome::Stopped);
        assert_eq!(*terminated.lock().unwrap(), vec![4242]);
    }

    #[test]
    fn spawn_failure_is_returned_as_the_error() {
        let mut policy = SupervisionPolicy::new(budget(5));
        let spawn = || Err("cannot spawn the autopilot daemon (x): missing".to_owned());
        let err = run_supervised(
            &spawn,
            &mut policy,
            &|| false,
            &|| None,
            SupervisedHooks::default(),
        )
        .unwrap_err();
        assert_eq!(err, "cannot spawn the autopilot daemon (x): missing");
    }

    // --- the entrypoint ------------------------------------------------------------------------

    /// A hermetic host: captured stderr, a repo root, scripted child exit
    /// codes (each spawn pops one; an empty script stops the run as a
    /// forwarded signal would), a scripted liveness record, a ticking
    /// supervision clock, and the armed signal handler.
    struct FakeHost {
        stderr: Arc<Mutex<Vec<String>>>,
        root: PathBuf,
        exits: Mutex<std::collections::VecDeque<i32>>,
        spawns: Mutex<Vec<(Vec<String>, PathBuf, f64)>>,
        liveness: Option<AutopilotActivity>,
        armed: Mutex<Option<SignalHandler>>,
        signal_after_spawns: Option<usize>,
        terminated: Arc<Mutex<Vec<i64>>>,
    }

    impl FakeHost {
        fn new(root: &Path) -> Self {
            Self {
                stderr: Arc::new(Mutex::new(Vec::new())),
                root: root.to_path_buf(),
                exits: Mutex::new(std::collections::VecDeque::new()),
                spawns: Mutex::new(Vec::new()),
                liveness: None,
                armed: Mutex::new(None),
                signal_after_spawns: None,
                terminated: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn exits(self, codes: &[i32]) -> Self {
            *self.exits.lock().unwrap() = codes.iter().copied().collect();
            self
        }

        fn live(mut self, record: AutopilotActivity) -> Self {
            self.liveness = Some(record);
            self
        }

        fn signal_after(mut self, spawns: usize) -> Self {
            self.signal_after_spawns = Some(spawns);
            self
        }

        fn lines(&self) -> Vec<String> {
            self.stderr.lock().unwrap().clone()
        }
    }

    impl SupervisedHost for FakeHost {
        fn stderr(&self) -> LogSink {
            let lines = self.stderr.clone();
            Arc::new(move |line: &str| lines.lock().unwrap().push(line.to_owned()))
        }

        fn repo_root(&self) -> Result<PathBuf, String> {
            if self.root.as_os_str().is_empty() {
                return Err("ERROR: not inside a git repository.".to_owned());
            }
            Ok(self.root.clone())
        }

        fn daemon_program(&self) -> Result<Vec<String>, String> {
            Ok(argv(&["/bin/flywheel", "autopilot"]))
        }

        fn spawn(
            &self,
            daemon_argv: &[String],
            log_dir: &Path,
            stop_timeout: f64,
        ) -> Result<Arc<SupervisedChild>, String> {
            let mut spawns = self.spawns.lock().unwrap();
            spawns.push((daemon_argv.to_vec(), log_dir.to_path_buf(), stop_timeout));
            let index = spawns.len();
            let pid = 5000 + index as i64;
            let exit = self.exits.lock().unwrap().pop_front();
            let signal = self
                .signal_after_spawns
                .filter(|n| index >= *n)
                .and_then(|_| self.armed.lock().unwrap().clone());
            let terminated = self.terminated.clone();
            Ok(Arc::new(
                SupervisedChild::new(pid, move || {
                    if let Some(on_signal) = &signal {
                        on_signal(SIGTERM);
                        return -SIGTERM;
                    }
                    match exit {
                        Some(code) => code,
                        None => panic!("script exhausted: spawn {index} had no exit code"),
                    }
                })
                .with_terminate(move || terminated.lock().unwrap().push(pid)),
            ))
        }

        fn read_liveness(&self, _activity_path: &Path) -> Option<AutopilotActivity> {
            self.liveness.clone()
        }

        fn supervision_policy(&self, budget: SupervisionBudget) -> SupervisionPolicy {
            let tick = AtomicU32::new(0);
            SupervisionPolicy::with_clock(
                budget,
                Box::new(move || f64::from(tick.fetch_add(1, Ordering::SeqCst))),
            )
        }

        fn arm_stop_signals(&self, on_signal: SignalHandler, _log: &LogSink) {
            *self.armed.lock().unwrap() = Some(on_signal);
        }
    }

    #[test]
    fn bad_argument_prints_usage_and_exits_2() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(dir.path());
        assert_eq!(run_main(&argv(&["--bogus"]), &host), 2);
        assert_eq!(
            host.lines(),
            vec![
                USAGE.to_owned(),
                "flywheel autopilot-supervised: error: unrecognized arguments: --bogus".to_owned()
            ]
        );
    }

    #[test]
    fn help_exits_0() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(dir.path());
        assert_eq!(run_main(&argv(&["--help"]), &host), 0);
        assert!(host.spawns.lock().unwrap().is_empty());
    }

    #[test]
    fn bad_budget_values_exit_2_before_touching_the_repo() {
        let host = FakeHost::new(Path::new(""));
        assert_eq!(run_main(&argv(&["--max-respawns", "-1"]), &host), 2);
        assert_eq!(
            host.lines(),
            vec!["autopilot-supervised: --max-respawns must be >= 0"]
        );
        let host = FakeHost::new(Path::new(""));
        assert_eq!(run_main(&argv(&["--window-seconds", "0"]), &host), 2);
        assert_eq!(
            host.lines(),
            vec!["autopilot-supervised: --window-seconds must be > 0"]
        );
    }

    #[test]
    fn missing_repo_root_exits_1() {
        let host = FakeHost::new(Path::new(""));
        assert_eq!(run_main(&[], &host), 1);
        assert_eq!(host.lines(), vec!["ERROR: not inside a git repository."]);
    }

    #[test]
    fn adoption_spawns_nothing_and_exits_0() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(dir.path()).live(live_record(9999));
        assert_eq!(run_main(&argv(&["--window-seconds", "60"]), &host), 0);
        let lines = host.lines();
        assert_eq!(
            lines[0],
            format!(
                "[autopilot-supervised] headless supervised autopilot repo={} budget=5/60.0s pid={}",
                python_path_str(dir.path()),
                std::process::id()
            )
        );
        assert_eq!(
            lines[1],
            "[autopilot-supervised] adopted live autopilot daemon pid=9999; spawning nothing"
        );
        assert_eq!(
            lines[2],
            "[autopilot-supervised] exiting; adopted daemon pid=9999 keeps running."
        );
        assert!(host.spawns.lock().unwrap().is_empty());
    }

    #[test]
    fn crash_loop_exhausts_the_budget_and_exits_1() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(dir.path()).exits(&[1, 1, 1]);
        let code = run_main(
            &argv(&[
                "--max-respawns",
                "2",
                "--tasks-dir",
                "queue",
                "--model",
                "m",
            ]),
            &host,
        );
        assert_eq!(code, 1);
        let lines = host.lines();
        assert_eq!(
            lines[1],
            "[autopilot-supervised] spawned autopilot daemon pid=5001"
        );
        assert_eq!(
            lines[2],
            "[autopilot-supervised] daemon exited (exit=1); decision=respawn"
        );
        assert_eq!(
            lines[6],
            "[autopilot-supervised] daemon exited (exit=1); decision=exhausted"
        );
        assert_eq!(
            lines[7],
            "[autopilot-supervised] daemon exhausted the crash-loop budget after 3 death(s) \
             in the window; stopped respawning (dead_after_budget)."
        );
        let spawns = host.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 3);
        let expected_activity = dir
            .path()
            .join(".flywheel")
            .join("logs")
            .join("autopilot")
            .join("activity.json");
        assert_eq!(
            spawns[0].0,
            argv(&[
                "/bin/flywheel",
                "autopilot",
                "--activity-file",
                expected_activity.to_str().unwrap(),
                "--tasks-dir",
                "queue",
                "--model",
                "m"
            ])
        );
        assert_eq!(spawns[0].1, expected_activity.parent().unwrap());
        assert_eq!(spawns[0].2, DEFAULT_STOP_TIMEOUT_SECONDS);
        // The exhausted child was reaped by its own wait; the cleanup
        // terminate on the dead handle is harmless.
        assert_eq!(*host.terminated.lock().unwrap(), vec![5003]);
    }

    #[test]
    fn forwarded_stop_signal_terminates_the_child_and_exits_0() {
        let dir = TempDir::new().unwrap();
        let activity = dir.path().join("act.json");
        let host = FakeHost::new(dir.path()).exits(&[1]).signal_after(2);
        let code = run_main(
            &argv(&["--activity-file", activity.to_str().unwrap()]),
            &host,
        );
        assert_eq!(code, 0);
        let lines = host.lines();
        assert_eq!(
            lines[2],
            "[autopilot-supervised] daemon exited (exit=1); decision=respawn"
        );
        assert_eq!(
            lines[4],
            "[autopilot-supervised] stop signal 15; shutting the daemon down and exiting"
        );
        assert_eq!(
            lines.last().unwrap(),
            "[autopilot-supervised] stopped after 2 spawn(s)."
        );
        // The handler terminated the live child, and the cleanup
        // terminated the (now dead) handle again on the way out.
        assert_eq!(*host.terminated.lock().unwrap(), vec![5002, 5002]);
        assert_eq!(host.spawns.lock().unwrap()[0].1, dir.path());
    }

    // --- real processes ---------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn spawned_process_reports_its_exit_code_and_logs_under_the_log_dir() {
        let dir = TempDir::new().unwrap();
        let log_dir = dir.path().join("logs");
        let child = spawn_daemon(&argv(&["sh", "-c", "echo hi; exit 3"]), &log_dir, 1.0).unwrap();
        assert!(child.pid() > 0);
        assert_eq!(child.wait(), 3);
        // A second wait returns the recorded code without re-reaping.
        assert_eq!(child.wait(), 3);
        child.terminate();
        let logs: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(logs.len(), 1);
        let name = logs[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("autopilot-supervised-") && name.ends_with(".log"));
        assert_eq!(std::fs::read_to_string(&logs[0]).unwrap(), "hi\n");
    }

    #[cfg(unix)]
    #[test]
    fn terminate_brings_a_blocked_child_down_while_another_thread_waits() {
        let dir = TempDir::new().unwrap();
        let child = spawn_daemon(&argv(&["sh", "-c", "sleep 30"]), dir.path(), 2.0).unwrap();
        let waiter = child.clone();
        let handle = std::thread::spawn(move || waiter.wait());
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        child.terminate();
        let code = handle.join().unwrap();
        assert_eq!(code, -SIGTERM);
        assert!(started.elapsed() < Duration::from_secs(5));
        // Idempotent once the child is gone.
        child.terminate();
    }

    #[cfg(unix)]
    #[test]
    fn terminate_escalates_to_sigkill_when_sigterm_is_ignored() {
        let dir = TempDir::new().unwrap();
        let child = spawn_daemon(
            &argv(&["sh", "-c", "trap '' TERM; sleep 30"]),
            dir.path(),
            0.5,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        child.terminate();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(child.wait(), -SIGKILL);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_failure_names_the_program() {
        let dir = TempDir::new().unwrap();
        let err = spawn_daemon(
            &argv(&["/nonexistent/flywheel-binary", "autopilot"]),
            dir.path(),
            1.0,
        )
        .unwrap_err();
        assert!(
            err.starts_with("cannot spawn the autopilot daemon (/nonexistent/flywheel-binary): "),
            "{err}"
        );
    }
}
