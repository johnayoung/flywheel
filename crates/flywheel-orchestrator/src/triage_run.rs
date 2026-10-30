//! `flywheel triage` entry: one triage pass and (daemon) the polling loop.
//!
//! Mirrors `flywheel autopilot`: a neverending label-polling daemon by
//! default that triages the intake board on an interval, and `--once` for a
//! single pass. The triage pass itself (list -> author -> grade -> label
//! with fail-first receipts) lives in [`crate::triage`]; this module is the
//! thin CLI/runtime shell over it -- argument parsing, policy resolution,
//! signal handling.
//!
//! Load discipline (spec 00082): the policy is resolved and validated BEFORE
//! the repo root or any GitHub seam is touched, so a malformed `[triage]`
//! policy exits 2 having issued no `gh` call and no git command -- the
//! write-free guarantee an idle or failing pass must honor. An idle pass
//! (empty backlog) writes nothing and the daemon simply schedules the next
//! cycle; it never terminates on an idle cycle.
//!
//! Every process-level effect sits behind [`TriageHost`] so the shell runs
//! hermetically in tests: stderr, the `gh` runner, the repo-root lookup, the
//! pass itself, and signal arming. [`ProductionHost`] is the real one.
//!
//! Authoritative specs: `docs/cli.md` (`triage`), `docs/configuration.md`
//! (`[triage]`), `docs/autopilot.md`.

use crate::autopilot_run::{
    looks_like_negative_number, match_long_option, parse_float_arg, reject_inline, split_flag,
    take_value,
};
use crate::github::{GhRunner, ProcessGhRunner};
use crate::sources::LogSink;
use crate::triage::{
    TriageAgentOptions, TriageBoard, TriageError, TriagePass, TriagePassResult, TriageSeams,
    build_grader_executor, build_triage_authoring_invoker, resolve_base_sha,
};
pub use crate::workflow::load_effective_policy;
use flywheel_core::audit::StopFlag;
use std::cell::Cell;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Consecutive whole-cycle failures (a triage pass erroring) before the
/// daemon gives up so an operator can inspect, rather than hot-looping.
/// Mirrors the autopilot daemon's cross-cycle backstop: a single failing
/// cycle is counted and backed off so the loop runs further cycles; a
/// subsequent success resets the count; on the bounded count the loop stops
/// by surfacing a give-up signal the caller turns into a non-zero exit
/// (never a silent exit).
pub const MAX_CONSECUTIVE_CYCLE_FAILURES: u32 = 5;
/// Seconds the daemon backs off after a failed cycle before the next one.
pub const CYCLE_FAILURE_BACKOFF_SECONDS: f64 = 10.0;

/// The prefix every daemon log line carries.
pub const LOG_PREFIX: &str = "[triage]";

/// The one-line usage argparse prints on a bad argument.
pub const USAGE: &str =
    "usage: flywheel triage [-h] [--model MODEL] [--interval INTERVAL] [--once]";

const HELP: &str = "usage: flywheel triage [-h] [--model MODEL] [--interval INTERVAL] [--once]

Triage the intake board: promote well-specified issues to the ready label and route \
under-specified ones to needs-detail, each with a fail-first receipt. Runs as a neverending \
label-polling daemon by default; --once runs a single triage pass and exits.

options:
  -h, --help           show this help message and exit
  --model MODEL
  --interval INTERVAL  Seconds between daemon cycles (overrides [triage] config).
  --once               Run a single triage pass and exit (no daemon loop).";

const REQUIRES_GITHUB: &str = "flywheel triage: requires [source] kind = \"github\" with a repo \
(the intake board triage reads and writes)";

// --- Arguments ------------------------------------------------------------------------

/// The parsed `flywheel triage` command line.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TriageArgs {
    pub model: Option<String>,
    pub interval: Option<f64>,
    pub once: bool,
    pub help: bool,
}

/// The long options `flywheel triage` defines, for unique-prefix matching.
const LONG_OPTIONS: [&str; 4] = ["--help", "--model", "--interval", "--once"];

/// Parse `argv` (everything after the verb) as argparse would: `--model M`,
/// `--interval SECONDS` (a float), `--once`, `-h`/`--help`; every long option
/// accepts the `--flag=value` spelling and a unique-prefix abbreviation
/// (`--int 5`, `--on`), a repeated option takes its last value, and every
/// token no option claims is collected into one `unrecognized arguments: ...`
/// error. The error is argparse's message for the failure.
pub fn parse_args(argv: &[String]) -> Result<TriageArgs, String> {
    let mut args = TriageArgs::default();
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
        let (flag, inline) = split_flag(arg);
        let Some(option) = match_long_option(arg, flag, &LONG_OPTIONS)? else {
            unrecognized.push(arg.clone());
            continue;
        };
        match option {
            "--help" => {
                reject_inline(option, inline)?;
                args.help = true;
            }
            "--once" => {
                reject_inline(option, inline)?;
                args.once = true;
            }
            "--model" => args.model = Some(take_value(option, inline, &mut iter)?),
            "--interval" => {
                let raw = take_value(option, inline, &mut iter)?;
                args.interval = Some(parse_float_arg(option, &raw)?);
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

// --- Logging ---------------------------------------------------------------------------

/// A stderr line sink prefixed with `prefix` (`[triage] <message>`).
///
/// A message that already carries the prefix (the engine's own `[triage]`
/// lines) is emitted once-prefixed rather than doubled.
pub fn make_logger(prefix: &str, emit: LogSink) -> LogSink {
    let prefixed = format!("{prefix} ");
    Arc::new(move |message: &str| {
        if message.starts_with(&prefixed) {
            emit(message);
        } else {
            emit(&format!("{prefixed}{message}"));
        }
    })
}

/// Python's `repr(float)` for the values an interval takes: an integral
/// value keeps its `.0`, anything else prints shortest round-trip.
pub fn py_float_repr(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e16 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

fn log_result(log: &LogSink, result: &TriagePassResult) {
    let failed = if result.failed.is_empty() {
        String::new()
    } else {
        format!(", {} failed", result.failed.len())
    };
    log(&format!(
        "pass complete: {} ready, {} needs-detail, {} deferred{failed}",
        result.ready().len(),
        result.needs_detail().len(),
        result.deferred
    ));
    for outcome in &result.outcomes {
        log(&format!("#{}: {}", outcome.number, outcome.decision));
    }
}

// --- Policy resolution ------------------------------------------------------------------

/// Resolve the repo root (`git rev-parse --show-toplevel`) of the working
/// directory. The error is the message to print before exiting 1.
pub fn resolve_repo_root() -> Result<PathBuf, String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|err| format!("ERROR: git failed to start: {err}"))?;
    if !output.status.success() {
        return Err("ERROR: not inside a git repository.".to_string());
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

// --- One pass ---------------------------------------------------------------------------

/// Everything one production pass needs beyond its seams.
#[derive(Debug, Clone, PartialEq)]
pub struct SinglePassSpec {
    pub repo_root: PathBuf,
    pub board: TriageBoard,
    /// The policy's `[triage] max_per_pass`, wired verbatim into the
    /// engine's cap (`None` = uncapped).
    pub per_pass_cap: Option<usize>,
    pub agent: TriageAgentOptions,
}

/// Drive exactly one triage pass with the real seams.
///
/// Resolves the base commit SHA the receipt binds to, builds the production
/// authoring invoker (a cold agent session rooted in the repo) and grader
/// executor, and runs the pass with the given `gh` `runner`.
pub async fn run_single_pass(
    spec: &SinglePassSpec,
    runner: Arc<dyn GhRunner>,
    log: Option<LogSink>,
) -> Result<TriagePassResult, TriageError> {
    let base_sha = resolve_base_sha(&spec.repo_root)?;
    let seams = TriageSeams {
        invoker: build_triage_authoring_invoker(&spec.repo_root, &spec.agent),
        executor: build_grader_executor(&spec.repo_root),
        runner,
    };
    let mut triage = TriagePass::new(spec.board.clone(), base_sha, seams)?
        .with_per_pass_cap(spec.per_pass_cap)?;
    if let Some(log) = log {
        triage = triage.with_log(log);
    }
    triage.run().await
}

// --- The neverending daemon loop (mirrors the autopilot daemon) -----------------------------

/// A failed-cycle observer: the error and the consecutive failure count.
pub type CycleFailureHook<'a> = &'a dyn Fn(&TriageError, u32);

/// The optional observers of [`run_daemon_loop`].
#[derive(Default, Clone, Copy)]
pub struct DaemonHooks<'a> {
    /// Runs before every cycle.
    pub before_cycle: Option<&'a dyn Fn()>,
    /// Runs after every successful cycle with its result.
    pub on_cycle: Option<&'a dyn Fn(&TriagePassResult)>,
    /// Runs after every failed cycle with the error and the consecutive
    /// failure count.
    pub on_cycle_failure: Option<CycleFailureHook<'a>>,
    /// Runs once when the circuit breaker gives up.
    pub on_give_up: Option<&'a dyn Fn(u32)>,
    /// A test-only safety bound on attempted cycles; production leaves it
    /// `None` (truly neverending).
    pub max_cycles: Option<u32>,
}

impl fmt::Debug for DaemonHooks<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonHooks")
            .field("before_cycle", &self.before_cycle.is_some())
            .field("on_cycle", &self.on_cycle.is_some())
            .field("on_cycle_failure", &self.on_cycle_failure.is_some())
            .field("on_give_up", &self.on_give_up.is_some())
            .field("max_cycles", &self.max_cycles)
            .finish()
    }
}

/// Run triage passes on an interval until an explicit stop signal.
///
/// The testable core of `flywheel triage` (no `--once`): each cycle runs
/// one `run_cycle` pass, then waits `interval_seconds` before the next. It
/// MUST NOT terminate on an idle (nothing-to-triage) cycle -- an empty pass
/// writes nothing and the loop continues. The loop exits only when
/// `should_stop()` is true (an injected flag in tests; a SIGTERM/SIGINT
/// flag in production), or when the circuit breaker gives up, and at no
/// other time.
///
/// Circuit breaker: a `run_cycle` that errors is contained -- the error is
/// counted, `on_cycle_failure` is notified, and the loop backs off
/// [`CYCLE_FAILURE_BACKOFF_SECONDS`] (via the injected, interruptible
/// `sleep`) before running a further cycle. A subsequent successful cycle
/// resets the consecutive-failure count. After
/// [`MAX_CONSECUTIVE_CYCLE_FAILURES`] consecutive failures the loop stops
/// and calls `on_give_up` so the caller can surface a visible non-zero
/// signal -- never a silent exit.
///
/// Every collaborator is injected so the loop runs with no real wall-clock
/// waits and no live model: `run_cycle` is the (scripted) pass, `sleep` the
/// interruptible wait (given the seconds and `should_stop`), `should_stop`
/// the stop signal. Returns the number of cycles attempted (successful and
/// failed).
pub fn run_daemon_loop(
    run_cycle: &mut dyn FnMut() -> Result<TriagePassResult, TriageError>,
    interval_seconds: f64,
    should_stop: &dyn Fn() -> bool,
    sleep: &dyn Fn(f64, &dyn Fn() -> bool),
    hooks: DaemonHooks<'_>,
) -> u32 {
    let mut cycles = 0u32;
    let mut consecutive_failures = 0u32;
    while !should_stop() {
        if let Some(before_cycle) = hooks.before_cycle {
            before_cycle();
        }
        match run_cycle() {
            Err(err) => {
                consecutive_failures += 1;
                cycles += 1;
                if let Some(on_cycle_failure) = hooks.on_cycle_failure {
                    on_cycle_failure(&err, consecutive_failures);
                }
                if consecutive_failures >= MAX_CONSECUTIVE_CYCLE_FAILURES {
                    if let Some(on_give_up) = hooks.on_give_up {
                        on_give_up(consecutive_failures);
                    }
                    break;
                }
                if hooks.max_cycles.is_some_and(|max| cycles >= max) {
                    break;
                }
                if should_stop() {
                    break;
                }
                sleep(CYCLE_FAILURE_BACKOFF_SECONDS, should_stop);
            }
            Ok(result) => {
                consecutive_failures = 0;
                cycles += 1;
                if let Some(on_cycle) = hooks.on_cycle {
                    on_cycle(&result);
                }
                if hooks.max_cycles.is_some_and(|max| cycles >= max) {
                    break;
                }
                if should_stop() {
                    break;
                }
                sleep(interval_seconds, should_stop);
            }
        }
    }
    cycles
}

/// Longest single wait (about 136 years): an unbounded interval waits for
/// the stop flag alone without overflowing the deadline arithmetic.
const MAX_WAIT: Duration = Duration::from_secs(u32::MAX as u64);

/// Sleep up to `seconds`, waking immediately when `stop` is set.
///
/// Honors fractional seconds exactly (the Python shell truncated the wait
/// to whole seconds and polled once per second).
pub fn interruptible_sleep(stop: &StopFlag, seconds: f64) {
    if seconds.is_nan() || seconds <= 0.0 {
        return;
    }
    let wait = Duration::try_from_secs_f64(seconds)
        .map(|d| d.min(MAX_WAIT))
        .unwrap_or(MAX_WAIT);
    stop.wait(wait);
}

// --- The host seam -----------------------------------------------------------------------

/// The process-level effects `flywheel triage` performs, behind one seam
/// so [`run_main`] is hermetic in tests.
pub trait TriageHost: Send + Sync {
    /// An explicit policy file, or `None` to auto-detect from the working
    /// directory ([`load_effective_policy`]).
    fn policy_path(&self) -> Option<PathBuf> {
        None
    }

    /// The raw stderr line sink (no prefix).
    fn stderr(&self) -> LogSink;

    /// The `gh` runner one pass reads and writes through.
    fn gh_runner(&self) -> Arc<dyn GhRunner>;

    /// The repo root a pass runs in; the error is the message to print
    /// before exiting 1.
    fn repo_root(&self) -> Result<PathBuf, String>;

    /// Run one triage pass.
    fn run_cycle(
        &self,
        spec: &SinglePassSpec,
        runner: Arc<dyn GhRunner>,
        log: LogSink,
    ) -> Result<TriagePassResult, TriageError>;

    /// Arm the daemon's stop signals: SIGTERM/SIGINT set `stop` and log the
    /// `stop signal <n>; exiting after the current cycle` line.
    fn arm_stop_signals(&self, stop: &StopFlag, log: &LogSink);

    /// Wait between cycles: up to `seconds`, waking as soon as `stop` is
    /// set ([`interruptible_sleep`] in production; a no-op in tests).
    fn sleep(&self, stop: &StopFlag, seconds: f64) {
        interruptible_sleep(stop, seconds);
    }
}

/// The real host: stderr, the `gh` binary, `git`, a tokio runtime driving
/// the pass, and OS signal handlers.
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

impl TriageHost for ProductionHost {
    fn stderr(&self) -> LogSink {
        Arc::new(|line: &str| eprintln!("{line}"))
    }

    fn gh_runner(&self) -> Arc<dyn GhRunner> {
        Arc::new(ProcessGhRunner)
    }

    fn repo_root(&self) -> Result<PathBuf, String> {
        resolve_repo_root()
    }

    fn run_cycle(
        &self,
        spec: &SinglePassSpec,
        runner: Arc<dyn GhRunner>,
        log: LogSink,
    ) -> Result<TriagePassResult, TriageError> {
        self.runtime
            .block_on(run_single_pass(spec, runner, Some(log)))
    }

    fn arm_stop_signals(&self, stop: &StopFlag, log: &LogSink) {
        let stop = stop.clone();
        let log = log.clone();
        self.runtime
            .spawn(async move { watch_stop_signals(stop, log).await });
    }
}

/// Set `stop` on every SIGTERM/SIGINT for the life of the process, logging
/// each one (the handler stays armed, as Python's did across cycles), and
/// terminate every live agent process group (each leads a group of its
/// own, out of reach of a signal aimed at this daemon's group).
#[cfg(unix)]
async fn watch_stop_signals(stop: StopFlag, log: LogSink) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(err) => {
            log(&format!("could not install the SIGTERM handler: {err}"));
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(err) => {
            log(&format!("could not install the SIGINT handler: {err}"));
            return;
        }
    };
    loop {
        let signum = tokio::select! {
            _ = term.recv() => SignalKind::terminate().as_raw_value(),
            _ = int.recv() => SignalKind::interrupt().as_raw_value(),
        };
        stop.set();
        flywheel_agents::process_groups::spawn_shutdown_sweep();
        log(&format!(
            "stop signal {signum}; exiting after the current cycle"
        ));
    }
}

#[cfg(not(unix))]
async fn watch_stop_signals(stop: StopFlag, log: LogSink) {
    loop {
        if let Err(err) = tokio::signal::ctrl_c().await {
            log(&format!("could not install the interrupt handler: {err}"));
            return;
        }
        stop.set();
        flywheel_agents::process_groups::spawn_shutdown_sweep();
        log("stop signal 2; exiting after the current cycle");
    }
}

// --- main -------------------------------------------------------------------------------

/// Run `flywheel triage` with the real host; returns the process exit code.
pub fn main(argv: &[String]) -> i32 {
    match ProductionHost::new() {
        Ok(host) => run_main(argv, &host),
        Err(err) => {
            eprintln!("flywheel triage: cannot start the runtime: {err}");
            1
        }
    }
}

/// Run `flywheel triage`: a neverending label-polling daemon, or one pass.
///
/// Bare `flywheel triage` runs the neverending polling loop on the
/// configured interval (idling, not exiting, on an empty cycle), stopping
/// only on SIGTERM/SIGINT. `--once` runs exactly one triage pass and exits
/// 0 (the testable unit and a manual escape hatch). Mirrors
/// `flywheel autopilot` (daemon by default, `--once` for a single pass).
///
/// The policy is resolved and validated first: a malformed `[triage]` value
/// prints to stderr and returns 2 having built no engine, resolved no repo
/// root, and issued no `gh` call. A failing `--once` pass prints its error
/// and returns 1; a daemon that gives up returns 1.
pub fn run_main(argv: &[String], host: &dyn TriageHost) -> i32 {
    let stderr = host.stderr();
    let args = match parse_args(argv) {
        Ok(args) => args,
        Err(message) => {
            stderr(USAGE);
            stderr(&format!("flywheel triage: error: {message}"));
            return 2;
        }
    };
    if args.help {
        println!("{HELP}");
        return 0;
    }
    let log = make_logger(LOG_PREFIX, stderr.clone());

    // Resolve and validate the policy BEFORE touching git or the gh seam: a
    // malformed [triage] value must exit 2 having issued no GitHub write and
    // no git command (spec 00082, the write-free guarantee).
    let policy = match load_effective_policy(host.policy_path().as_deref()) {
        Ok(policy) => policy,
        Err(err) => {
            stderr(&format!("flywheel triage: policy error: {err}"));
            return 2;
        }
    };
    let Some(policy) = policy.filter(|p| {
        p.source_kind == "github" && p.github_repo.as_deref().is_some_and(|r| !r.is_empty())
    }) else {
        stderr(REQUIRES_GITHUB);
        return 2;
    };
    let repo = policy.github_repo.clone().unwrap_or_default();
    let board = TriageBoard {
        repo: repo.clone(),
        intake_label: policy.triage_intake_label.clone(),
        ready_label: policy.triage_ready_label.clone(),
        needs_detail_label: policy.triage_needs_detail_label.clone(),
    };
    let agent = TriageAgentOptions {
        agent_id: policy
            .agent_id
            .clone()
            .unwrap_or_else(|| TriageAgentOptions::default().agent_id),
        agent_transport: policy.agent_transport.clone(),
        model: args.model.clone().or_else(|| policy.model.clone()),
        ..TriageAgentOptions::default()
    };
    let runner = host.gh_runner();

    let repo_root = match host.repo_root() {
        Ok(root) => root,
        Err(message) => {
            stderr(&message);
            return 1;
        }
    };
    let spec = SinglePassSpec {
        repo_root,
        board: board.clone(),
        per_pass_cap: policy
            .triage_max_per_pass
            .map(|n| usize::try_from(n.max(1)).unwrap_or(usize::MAX)),
        agent,
    };
    let mut run_cycle = || host.run_cycle(&spec, runner.clone(), log.clone());

    if args.once {
        log(&format!(
            "single pass repo={repo} intake={} ready={} needs_detail={}",
            board.intake_label, board.ready_label, board.needs_detail_label
        ));
        return match run_cycle() {
            Ok(result) => {
                log_result(&log, &result);
                0
            }
            Err(err) => {
                stderr(&format!("flywheel triage: {err}"));
                1
            }
        };
    }

    let interval = args.interval.unwrap_or(policy.triage_interval_seconds);
    log(&format!(
        "daemon started repo={repo} intake={} ready={} needs_detail={} interval={}s pid={}",
        board.intake_label,
        board.ready_label,
        board.needs_detail_label,
        py_float_repr(interval),
        std::process::id()
    ));

    let stop = StopFlag::new();
    host.arm_stop_signals(&stop, &log);
    let gave_up = Cell::new(false);

    let should_stop = || stop.is_set();
    let sleep = |seconds: f64, _should_stop: &dyn Fn() -> bool| host.sleep(&stop, seconds);
    let on_cycle = |result: &TriagePassResult| log_result(&log, result);
    let on_cycle_failure = |err: &TriageError, consecutive: u32| {
        log(&format!(
            "cycle failed ({}: {err}) [{consecutive}/{MAX_CONSECUTIVE_CYCLE_FAILURES}]",
            err.kind_name()
        ));
    };
    let on_give_up = |consecutive: u32| {
        gave_up.set(true);
        log(&format!(
            "too many consecutive cycle failures ({consecutive}); exiting non-zero for \
             operator inspection."
        ));
    };
    let cycles = run_daemon_loop(
        &mut run_cycle,
        interval,
        &should_stop,
        &sleep,
        DaemonHooks {
            before_cycle: None,
            on_cycle: Some(&on_cycle),
            on_cycle_failure: Some(&on_cycle_failure),
            on_give_up: Some(&on_give_up),
            max_cycles: None,
        },
    );
    log(&format!("daemon stopped after {cycles} cycle(s)"));
    if gave_up.get() { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyError;
    use crate::triage::test_support::ForbiddenGh;
    use crate::triage::{Decision, TriageOutcome};
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    const GITHUB_SOURCE: &str =
        "[source]\nkind = \"github\"\nrepo = \"octo/repo\"\nlabel = \"flywheel\"\n";

    fn idle() -> Result<TriagePassResult, TriageError> {
        Ok(TriagePassResult::default())
    }

    fn noop_sleep(_seconds: f64, _should_stop: &dyn Fn() -> bool) {}

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn write_policy(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("flywheel.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    // --- argument parsing ------------------------------------------------------------------

    #[test]
    fn parse_args_mirrors_argparse() {
        assert_eq!(parse_args(&[]).unwrap(), TriageArgs::default());
        let parsed = parse_args(&argv(&["--once", "--model", "m", "--interval", "2.5"])).unwrap();
        assert_eq!(
            parsed,
            TriageArgs {
                model: Some("m".into()),
                interval: Some(2.5),
                once: true,
                help: false
            }
        );
        let parsed = parse_args(&argv(&["--model=a", "--model=b", "--interval=7"])).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("b"));
        assert_eq!(parsed.interval, Some(7.0));
        assert!(parse_args(&argv(&["-h"])).unwrap().help);
        assert_eq!(
            parse_args(&argv(&["--interval", "x"])).unwrap_err(),
            "argument --interval: invalid float value: 'x'"
        );
        assert_eq!(
            parse_args(&argv(&["--model"])).unwrap_err(),
            "argument --model: expected one argument"
        );
        assert_eq!(
            parse_args(&argv(&["--bogus"])).unwrap_err(),
            "unrecognized arguments: --bogus"
        );
    }

    #[test]
    fn parse_args_accepts_unique_prefix_abbreviations() {
        let parsed = parse_args(&argv(&["--on", "--mod", "m", "--int=3"])).unwrap();
        assert_eq!(
            parsed,
            TriageArgs {
                model: Some("m".into()),
                interval: Some(3.0),
                once: true,
                help: false
            }
        );
        assert!(parse_args(&argv(&["--he"])).unwrap().help);
        assert_eq!(
            parse_args(&argv(&["--int", "x"])).unwrap_err(),
            "argument --interval: invalid float value: 'x'"
        );
        assert_eq!(
            parse_args(&argv(&["--o=1"])).unwrap_err(),
            "argument --once: ignored explicit argument '1'"
        );
        assert_eq!(
            parse_args(&argv(&["--onces"])).unwrap_err(),
            "unrecognized arguments: --onces"
        );
    }

    #[test]
    fn logger_prefixes_once() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = lines.clone();
        let emit: LogSink =
            Arc::new(move |line: &str| sink_lines.lock().unwrap().push(line.to_string()));
        let log = make_logger(LOG_PREFIX, emit);
        log("hello");
        log("[triage] per-pass cap 2 reached");
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            ["[triage] hello", "[triage] per-pass cap 2 reached"]
        );
    }

    #[test]
    fn float_repr_matches_python() {
        assert_eq!(py_float_repr(300.0), "300.0");
        assert_eq!(py_float_repr(0.0), "0.0");
        assert_eq!(py_float_repr(2.5), "2.5");
        assert_eq!(py_float_repr(-1.0), "-1.0");
        assert_eq!(py_float_repr(f64::INFINITY), "inf");
    }

    #[test]
    fn interruptible_sleep_wakes_on_stop_and_skips_non_positive() {
        let stop = StopFlag::new();
        let started = std::time::Instant::now();
        interruptible_sleep(&stop, 0.0);
        interruptible_sleep(&stop, -3.0);
        interruptible_sleep(&stop, f64::NAN);
        assert!(started.elapsed() < Duration::from_millis(200));
        stop.set();
        interruptible_sleep(&stop, 30.0);
        interruptible_sleep(&stop, f64::INFINITY);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn explicit_policy_path_is_loaded_and_empty_means_unspecified_default_detection() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_policy(
            dir.path(),
            &format!("{GITHUB_SOURCE}\n[triage]\nmax_per_pass = 3\n"),
        );
        let policy = load_effective_policy(Some(&path)).unwrap().unwrap();
        assert_eq!(policy.triage_max_per_pass, Some(3));
        let missing = dir.path().join("absent.toml");
        assert!(matches!(
            load_effective_policy(Some(&missing)),
            Err(PolicyError::Read { .. })
        ));
    }

    // --- the daemon loop -------------------------------------------------------------------

    #[test]
    fn idle_cycle_does_not_stop_the_daemon() {
        let calls = Cell::new(0);
        let mut run_cycle = || {
            calls.set(calls.get() + 1);
            idle()
        };
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| false,
            &noop_sleep,
            DaemonHooks {
                max_cycles: Some(3),
                ..DaemonHooks::default()
            },
        );
        assert_eq!(cycles, 3);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn loops_until_the_stop_signal() {
        let calls = Cell::new(0);
        let stop_after = 4;
        let mut run_cycle = || {
            calls.set(calls.get() + 1);
            idle()
        };
        let should_stop = || calls.get() >= stop_after;
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &should_stop,
            &noop_sleep,
            DaemonHooks::default(),
        );
        assert_eq!(cycles, stop_after);
        assert_eq!(calls.get(), stop_after);
    }

    #[test]
    fn stop_before_the_first_cycle_runs_nothing() {
        let mut run_cycle = || panic!("must not run");
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| true,
            &noop_sleep,
            DaemonHooks::default(),
        );
        assert_eq!(cycles, 0);
    }

    #[test]
    fn before_cycle_runs_each_iteration() {
        let armed = Cell::new(0);
        let before_cycle = || armed.set(armed.get() + 1);
        let mut run_cycle = idle;
        run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| false,
            &noop_sleep,
            DaemonHooks {
                before_cycle: Some(&before_cycle),
                max_cycles: Some(2),
                ..DaemonHooks::default()
            },
        );
        assert_eq!(armed.get(), 2);
    }

    #[test]
    fn failing_cycle_backs_off_then_gives_up() {
        let failures = Mutex::new(Vec::new());
        let gave_up = Mutex::new(Vec::new());
        let backoffs = Cell::new(0);
        let mut run_cycle = || Err(TriageError::Git("gh unreachable".into()));
        let on_cycle_failure =
            |_err: &TriageError, consecutive: u32| failures.lock().unwrap().push(consecutive);
        let on_give_up = |consecutive: u32| gave_up.lock().unwrap().push(consecutive);
        let counting_sleep = |seconds: f64, _should_stop: &dyn Fn() -> bool| {
            assert_eq!(seconds, CYCLE_FAILURE_BACKOFF_SECONDS);
            backoffs.set(backoffs.get() + 1);
        };
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| false,
            &counting_sleep,
            DaemonHooks {
                on_cycle_failure: Some(&on_cycle_failure),
                on_give_up: Some(&on_give_up),
                ..DaemonHooks::default()
            },
        );
        assert_eq!(cycles, MAX_CONSECUTIVE_CYCLE_FAILURES);
        let expected: Vec<u32> = (1..=MAX_CONSECUTIVE_CYCLE_FAILURES).collect();
        assert_eq!(*failures.lock().unwrap(), expected);
        assert_eq!(*gave_up.lock().unwrap(), [MAX_CONSECUTIVE_CYCLE_FAILURES]);
        assert_eq!(backoffs.get(), MAX_CONSECUTIVE_CYCLE_FAILURES - 1);
    }

    #[test]
    fn a_success_resets_the_failure_streak() {
        let outcomes: Mutex<VecDeque<Option<TriageError>>> = Mutex::new(VecDeque::from([
            Some(TriageError::Git("blip".into())),
            None,
            Some(TriageError::Git("blip".into())),
        ]));
        let failures = Mutex::new(Vec::new());
        let mut run_cycle = || match outcomes.lock().unwrap().pop_front().flatten() {
            Some(err) => Err(err),
            None => idle(),
        };
        let on_cycle_failure =
            |_err: &TriageError, consecutive: u32| failures.lock().unwrap().push(consecutive);
        run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| false,
            &noop_sleep,
            DaemonHooks {
                on_cycle_failure: Some(&on_cycle_failure),
                max_cycles: Some(3),
                ..DaemonHooks::default()
            },
        );
        assert_eq!(*failures.lock().unwrap(), [1, 1]);
    }

    // --- the entrypoint ------------------------------------------------------------------

    /// A hermetic host: an explicit policy file, captured stderr, a forbidden
    /// `gh` runner, an optional repo root (absent = must not be resolved),
    /// a scripted pass recording the specs it was handed, and a no-op sleep.
    /// `armed` holds the daemon's stop flag once signals are armed; with
    /// `stop_after_cycle` the scripted pass sets it, standing in for a
    /// signal arriving mid-cycle.
    struct FakeHost {
        policy: PathBuf,
        stderr: Arc<Mutex<Vec<String>>>,
        root: Option<PathBuf>,
        passes: Mutex<Vec<SinglePassSpec>>,
        outcome: Mutex<VecDeque<Result<TriagePassResult, TriageError>>>,
        armed: Mutex<Option<StopFlag>>,
        stop_after_cycle: bool,
    }

    impl FakeHost {
        fn new(policy: PathBuf, root: Option<PathBuf>) -> Self {
            Self {
                policy,
                stderr: Arc::new(Mutex::new(Vec::new())),
                root,
                passes: Mutex::new(Vec::new()),
                outcome: Mutex::new(VecDeque::new()),
                armed: Mutex::new(None),
                stop_after_cycle: false,
            }
        }

        fn err_text(&self) -> String {
            self.stderr.lock().unwrap().join("\n")
        }
    }

    impl TriageHost for FakeHost {
        fn policy_path(&self) -> Option<PathBuf> {
            Some(self.policy.clone())
        }

        fn stderr(&self) -> LogSink {
            let lines = self.stderr.clone();
            Arc::new(move |line: &str| lines.lock().unwrap().push(line.to_string()))
        }

        fn gh_runner(&self) -> Arc<dyn GhRunner> {
            Arc::new(ForbiddenGh)
        }

        fn repo_root(&self) -> Result<PathBuf, String> {
            match &self.root {
                Some(root) => Ok(root.clone()),
                None => panic!("repo root must not be resolved"),
            }
        }

        fn run_cycle(
            &self,
            spec: &SinglePassSpec,
            _runner: Arc<dyn GhRunner>,
            _log: LogSink,
        ) -> Result<TriagePassResult, TriageError> {
            self.passes.lock().unwrap().push(spec.clone());
            if self.stop_after_cycle
                && let Some(stop) = self.armed.lock().unwrap().as_ref()
            {
                stop.set();
            }
            self.outcome
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(TriagePassResult::default()))
        }

        fn arm_stop_signals(&self, stop: &StopFlag, _log: &LogSink) {
            *self.armed.lock().unwrap() = Some(stop.clone());
        }

        fn sleep(&self, _stop: &StopFlag, _seconds: f64) {}
    }

    #[test]
    fn malformed_triage_policy_exits_two_write_free() {
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(
            dir.path(),
            &format!("{GITHUB_SOURCE}\n[triage]\ninterval_seconds = -5\n"),
        );
        let host = FakeHost::new(policy, None);
        assert_eq!(run_main(&[], &host), 2);
        let err = host.err_text();
        assert!(err.contains("policy error"), "{err}");
        assert!(err.contains("triage.interval_seconds"), "{err}");
        assert!(host.passes.lock().unwrap().is_empty());
    }

    #[test]
    fn non_github_source_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(dir.path(), "[source]\nkind = \"directory\"\n");
        let host = FakeHost::new(policy, None);
        assert_eq!(run_main(&[], &host), 2);
        assert!(host.err_text().contains("github"));
    }

    #[test]
    fn bad_arguments_exit_two_before_the_policy() {
        let host = FakeHost::new(PathBuf::from("/nonexistent/flywheel.toml"), None);
        assert_eq!(run_main(&argv(&["--bogus"]), &host), 2);
        let err = host.err_text();
        assert!(err.starts_with(USAGE), "{err}");
        assert!(err.contains("flywheel triage: error: unrecognized arguments: --bogus"));
    }

    #[test]
    fn once_runs_exactly_one_pass_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(dir.path(), GITHUB_SOURCE);
        let host = FakeHost::new(policy, Some(dir.path().to_path_buf()));
        assert_eq!(run_main(&argv(&["--once"]), &host), 0);
        let passes = host.passes.lock().unwrap();
        assert_eq!(passes.len(), 1);
        let spec = &passes[0];
        assert_eq!(spec.repo_root, dir.path());
        assert_eq!(spec.board.repo, "octo/repo");
        assert_eq!(spec.board.intake_label, "flywheel");
        assert_eq!(spec.board.ready_label, "flywheel:ready");
        assert_eq!(spec.board.needs_detail_label, "flywheel:needs-detail");
        assert_eq!(spec.per_pass_cap, None);
        assert_eq!(spec.agent.agent_id, "claude-code");
        assert_eq!(spec.agent.model, None);
        assert!(host.armed.lock().unwrap().is_none());
        assert_eq!(
            host.err_text(),
            "[triage] single pass repo=octo/repo intake=flywheel ready=flywheel:ready \
             needs_detail=flywheel:needs-detail\n[triage] pass complete: 0 ready, 0 needs-detail, 0 deferred"
        );
    }

    #[test]
    fn once_wires_max_per_pass_model_and_agent_into_the_spec() {
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(
            dir.path(),
            &format!(
                "{GITHUB_SOURCE}\n[agent]\nid = \"codex\"\ntransport = \"cli\"\nmodel = \"claude-x\"\n\n[triage]\nmax_per_pass = 7\n"
            ),
        );
        let host = FakeHost::new(policy, Some(dir.path().to_path_buf()));
        host.outcome.lock().unwrap().push_back(Ok(TriagePassResult {
            outcomes: vec![
                TriageOutcome {
                    number: 3,
                    source_ref: "octo/repo#3".into(),
                    decision: Decision::Ready,
                    authoritative_command: Some("c".into()),
                    exit_code: Some(1),
                    receipt: None,
                    comment: None,
                    missing_information: vec![],
                },
                TriageOutcome {
                    number: 4,
                    source_ref: "octo/repo#4".into(),
                    decision: Decision::NeedsDetail,
                    authoritative_command: None,
                    exit_code: None,
                    receipt: None,
                    comment: Some("x".into()),
                    missing_information: vec![],
                },
            ],
            deferred: 2,
            failed: vec![],
        }));
        assert_eq!(run_main(&argv(&["--once", "--model", "m2"]), &host), 0);
        let passes = host.passes.lock().unwrap();
        assert_eq!(passes[0].per_pass_cap, Some(7));
        assert_eq!(passes[0].agent.model.as_deref(), Some("m2"));
        assert_eq!(passes[0].agent.agent_id, "codex");
        assert_eq!(passes[0].agent.agent_transport.as_deref(), Some("cli"));
        let err = host.err_text();
        assert!(err.contains("[triage] pass complete: 1 ready, 1 needs-detail, 2 deferred\n[triage] #3: ready\n[triage] #4: needs_detail"), "{err}");
        drop(passes);
        // Without --model the policy's [agent] model flows through.
        assert_eq!(run_main(&argv(&["--once"]), &host), 0);
        let passes = host.passes.lock().unwrap();
        assert_eq!(passes[1].agent.model.as_deref(), Some("claude-x"));
    }

    #[test]
    fn once_summary_counts_failed_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(dir.path(), GITHUB_SOURCE);
        let host = FakeHost::new(policy, Some(dir.path().to_path_buf()));
        host.outcome.lock().unwrap().push_back(Ok(TriagePassResult {
            outcomes: vec![TriageOutcome {
                number: 8,
                source_ref: "octo/repo#8".into(),
                decision: Decision::Ready,
                authoritative_command: Some("c".into()),
                exit_code: Some(1),
                receipt: None,
                comment: None,
                missing_information: vec![],
            }],
            deferred: 0,
            failed: vec![crate::triage::TriageFailure {
                number: 9,
                kind: "InvokeError",
                message: "authoring agent failed: x".into(),
            }],
        }));
        assert_eq!(run_main(&argv(&["--once"]), &host), 0);
        let err = host.err_text();
        assert!(
            err.contains("[triage] pass complete: 1 ready, 0 needs-detail, 0 deferred, 1 failed\n[triage] #8: ready"),
            "{err}"
        );
    }

    #[test]
    fn once_failing_pass_exits_one_with_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(dir.path(), GITHUB_SOURCE);
        let host = FakeHost::new(policy, Some(dir.path().to_path_buf()));
        host.outcome
            .lock()
            .unwrap()
            .push_back(Err(TriageError::Git("no HEAD".into())));
        assert_eq!(run_main(&argv(&["--once"]), &host), 1);
        assert!(host.err_text().ends_with("flywheel triage: no HEAD"));
    }

    #[test]
    fn missing_repo_root_exits_one_after_the_policy() {
        struct NoRoot(FakeHost);
        impl TriageHost for NoRoot {
            fn policy_path(&self) -> Option<PathBuf> {
                self.0.policy_path()
            }
            fn stderr(&self) -> LogSink {
                self.0.stderr()
            }
            fn gh_runner(&self) -> Arc<dyn GhRunner> {
                self.0.gh_runner()
            }
            fn repo_root(&self) -> Result<PathBuf, String> {
                Err("ERROR: not inside a git repository.".into())
            }
            fn run_cycle(
                &self,
                spec: &SinglePassSpec,
                runner: Arc<dyn GhRunner>,
                log: LogSink,
            ) -> Result<TriagePassResult, TriageError> {
                self.0.run_cycle(spec, runner, log)
            }
            fn arm_stop_signals(&self, stop: &StopFlag, log: &LogSink) {
                self.0.arm_stop_signals(stop, log)
            }
            fn sleep(&self, stop: &StopFlag, seconds: f64) {
                self.0.sleep(stop, seconds)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(dir.path(), GITHUB_SOURCE);
        let host = NoRoot(FakeHost::new(policy, None));
        assert_eq!(run_main(&argv(&["--once"]), &host), 1);
        assert_eq!(host.0.err_text(), "ERROR: not inside a git repository.");
        assert!(host.0.passes.lock().unwrap().is_empty());
    }

    #[test]
    fn daemon_arms_signals_and_stops_on_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(
            dir.path(),
            &format!("{GITHUB_SOURCE}\n[triage]\ninterval_seconds = 2.5\n"),
        );
        let mut host = FakeHost::new(policy, Some(dir.path().to_path_buf()));
        // The scripted pass sets the daemon's own stop flag mid-cycle (as a
        // signal handler would), so the loop exits after exactly one cycle.
        host.stop_after_cycle = true;
        assert_eq!(run_main(&argv(&["--interval", "0.25"]), &host), 0);
        assert!(host.armed.lock().unwrap().is_some());
        assert_eq!(host.passes.lock().unwrap().len(), 1);
        let err = host.err_text();
        assert!(err.contains("daemon started repo=octo/repo intake=flywheel ready=flywheel:ready needs_detail=flywheel:needs-detail interval=0.25s pid="), "{err}");
        assert!(
            err.ends_with("[triage] daemon stopped after 1 cycle(s)"),
            "{err}"
        );
    }

    #[test]
    fn daemon_gives_up_after_consecutive_failures_and_exits_one() {
        let dir = tempfile::tempdir().unwrap();
        let policy = write_policy(dir.path(), GITHUB_SOURCE);
        let host = FakeHost::new(policy, Some(dir.path().to_path_buf()));
        for _ in 0..MAX_CONSECUTIVE_CYCLE_FAILURES {
            host.outcome
                .lock()
                .unwrap()
                .push_back(Err(TriageError::Git("gh unreachable".into())));
        }
        // The fake host's sleep is a no-op, so the backoff between strikes
        // costs no wall-clock time; the host never sets the stop flag, so
        // only the circuit breaker can end the loop.
        assert_eq!(run_main(&[], &host), 1);
        assert_eq!(
            host.passes.lock().unwrap().len(),
            MAX_CONSECUTIVE_CYCLE_FAILURES as usize
        );
        let err = host.err_text();
        assert!(
            err.contains("[triage] cycle failed (GitError: gh unreachable) [1/5]"),
            "{err}"
        );
        assert!(
            err.contains("[triage] cycle failed (GitError: gh unreachable) [5/5]"),
            "{err}"
        );
        assert!(
            err.ends_with(
                "[triage] too many consecutive cycle failures (5); exiting non-zero for \
                 operator inspection.\n[triage] daemon stopped after 5 cycle(s)"
            ),
            "{err}"
        );
    }
}
