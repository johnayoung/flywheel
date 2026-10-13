//! `flywheel autopilot` entry: the single refill pass and (daemon) loop.
//!
//! `--once` runs exactly one refill pass; bare `flywheel autopilot` wraps
//! the same pass in a neverending loop on an interval. The composed pass
//! (discovery -> score -> author -> emit) lives in [`crate::autopilot`];
//! this module is the thin CLI/runtime shell over it -- argument parsing,
//! policy resolution, logging, the circuit breaker, the activity/liveness
//! record, the no-progress back-off and signal handling -- mirroring how
//! `flywheel worker` shells over the worktree daemon.
//!
//! Every process-level effect sits behind [`AutopilotHost`] so the shell
//! runs hermetically in tests: stderr, the repo-root lookup, the store
//! factories, the pass itself, the clocks, the wait and signal arming.
//! [`ProductionHost`] is the real one.
//!
//! Authoritative specs: `docs/autopilot.md`, `docs/cli.md` (`autopilot`),
//! `docs/configuration.md` (`[autopilot]`).

use crate::autopilot::{
    AutopilotPassResult, DEFAULT_LANDING, DEFAULT_TARGET_DEPTH, QueueDepthFn, RefillError,
    RefillPassOptions, ScoreWeights, actionable_queue_depth, run_refill_pass,
};
use crate::autopilot_activity::{
    AutopilotActivity, EmittedSummary, PHASE_IDLE, PHASE_RUNNING, PHASE_STARTING, epoch_now,
    write_activity,
};
use crate::claims::{ClaimStore, ClaimStoreError, OrchestratorSchemaError};
use crate::orchestrate::{
    DEFAULT_NO_PROGRESS_BOUND, NoProgressObservation, NoProgressOutcome, NoProgressResult,
    OrchestrateError, redrive_no_progress,
};
use crate::policy::{DEFAULT_AUTOPILOT_INTERVAL_SECONDS, WorkPolicy};
use crate::sources::{LogSink, WorkSourceError};
use crate::store_factory::{StoreConfigError, build_claim_store, build_store};
use crate::store_registry::{Environ, StoreRegistries, environ_from_process};
use crate::triage_run::{interruptible_sleep, py_float_repr, resolve_repo_root};
use crate::workflow::{
    DEFAULT_DB_PATH, DEFAULT_TASKS_DIR, default_store_registries, load_effective_policy,
    python_path_str, store_config_for,
};
use chrono::Utc;
use flywheel_core::audit::StopFlag;
use flywheel_core::deadline_config::DeadlineConfig;
use flywheel_core::faults::{FaultClass, classify_error};
use flywheel_core::invoker_client::Clock;
use flywheel_core::store::{HarnessStore, StoreError, StoreSchemaError};
use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The prefix every daemon log line carries.
pub const LOG_PREFIX: &str = "[autopilot]";

/// The usage block argparse prints on a bad argument (80-column wrap).
pub const USAGE: &str = "usage: flywheel autopilot [-h] [--tasks-dir TASKS_DIR] [--model MODEL]
                          [--target-depth TARGET_DEPTH] [--interval INTERVAL]
                          [--once] [--activity-file ACTIVITY_FILE]";

const HELP: &str = "usage: flywheel autopilot [-h] [--tasks-dir TASKS_DIR] [--model MODEL]
                          [--target-depth TARGET_DEPTH] [--interval INTERVAL]
                          [--once] [--activity-file ACTIVITY_FILE]

Keep the work queue full with verifiable, tier-prioritized tasks. Runs as a
neverending daemon by default; --once runs a single refill pass and exits.

options:
  -h, --help            show this help message and exit
  --tasks-dir TASKS_DIR
  --model MODEL
  --target-depth TARGET_DEPTH
                        Fill the queue up to this depth (overrides [autopilot]
                        config).
  --interval INTERVAL   Seconds between daemon cycles (overrides [autopilot]
                        config).
  --once                Run a single refill pass and exit (no daemon loop).
  --activity-file ACTIVITY_FILE
                        Path to write the live activity snapshot the console
                        reads (defaults to
                        <repo>/.flywheel/logs/autopilot/activity.json).";

/// The long options in argparse declaration order (abbreviation matching
/// reports ambiguities in this order).
const LONG_OPTIONS: [&str; 7] = [
    "--help",
    "--tasks-dir",
    "--model",
    "--target-depth",
    "--interval",
    "--once",
    "--activity-file",
];

// --- Arguments ------------------------------------------------------------------------

/// The parsed `flywheel autopilot` command line.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AutopilotArgs {
    pub tasks_dir: Option<String>,
    pub model: Option<String>,
    pub target_depth: Option<i64>,
    pub interval: Option<f64>,
    pub once: bool,
    pub activity_file: Option<String>,
    pub help: bool,
}

/// Resolve a `--flag` token against `options` the way argparse does: an
/// exact match wins; otherwise a unique prefix selects its option; an
/// ambiguous prefix is the `ambiguous option: ...` error; no match is
/// `Ok(None)` (the token is collected as unrecognized).
pub fn match_long_option<'a>(
    token: &str,
    flag: &str,
    options: &[&'a str],
) -> Result<Option<&'a str>, String> {
    if let Some(exact) = options.iter().find(|o| **o == flag) {
        return Ok(Some(exact));
    }
    let matches: Vec<&str> = options
        .iter()
        .copied()
        .filter(|o| o.starts_with(flag))
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one)),
        many => Err(format!(
            "ambiguous option: {token} could match {}",
            many.join(", ")
        )),
    }
}

/// argparse's negative-number test: a token like `-1` or `-.5` is a value,
/// not an option, when the parser defines no negative-number-like option.
pub fn looks_like_negative_number(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('-') else {
        return false;
    };
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    match rest.split_once('.') {
        Some((whole, frac)) => {
            whole.bytes().all(|b| b.is_ascii_digit())
                && !frac.is_empty()
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Python `int(raw)` for an argparse `type=int` option.
pub fn parse_int_arg(name: &str, raw: &str) -> Result<i64, String> {
    let text = raw.trim();
    let text = text.strip_prefix('+').unwrap_or(text);
    let invalid = || format!("argument {name}: invalid int value: '{raw}'");
    if text.is_empty() || text.starts_with('_') || text.ends_with('_') || text.contains("__") {
        return Err(invalid());
    }
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    cleaned.parse::<i64>().map_err(|_| invalid())
}

/// Python `float(raw)` for an argparse `type=float` option.
pub fn parse_float_arg(name: &str, raw: &str) -> Result<f64, String> {
    let text = raw.trim();
    let invalid = || format!("argument {name}: invalid float value: '{raw}'");
    if text.is_empty() || text.starts_with('_') || text.ends_with('_') || text.contains("__") {
        return Err(invalid());
    }
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    match cleaned.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => Ok(f64::INFINITY),
        "-inf" | "-infinity" => Ok(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => Ok(f64::NAN),
        _ => cleaned.parse::<f64>().map_err(|_| invalid()),
    }
}

/// A long-option token split into its flag and inline `=value`.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
        _ => (arg, None),
    }
}

/// Take an option's value: the inline `=value`, else the next token unless
/// it is missing or looks like another option.
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

/// Reject an inline value on a store-true flag (`--once=1`).
fn reject_inline(flag: &str, inline: Option<&str>) -> Result<(), String> {
    match inline {
        Some(value) => Err(format!(
            "argument {flag}: ignored explicit argument '{value}'"
        )),
        None => Ok(()),
    }
}

/// Parse `argv` (everything after the verb) as argparse would: every long
/// option accepts the `--flag=value` spelling and a unique-prefix
/// abbreviation, a repeated option takes its last value, `--once=x` is
/// rejected, and every token no option claims is collected into one
/// `unrecognized arguments: ...` error reported after the parse. The error
/// is argparse's message for the failure.
pub fn parse_args(argv: &[String]) -> Result<AutopilotArgs, String> {
    let mut args = AutopilotArgs::default();
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
            "--tasks-dir" => args.tasks_dir = Some(take_value(option, inline, &mut iter)?),
            "--model" => args.model = Some(take_value(option, inline, &mut iter)?),
            "--activity-file" => {
                args.activity_file = Some(take_value(option, inline, &mut iter)?);
            }
            "--target-depth" => {
                let raw = take_value(option, inline, &mut iter)?;
                args.target_depth = Some(parse_int_arg(option, &raw)?);
            }
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

/// A stderr line sink prefixed with `prefix` (`[autopilot] <message>`).
pub fn make_logger(prefix: &str, emit: LogSink) -> LogSink {
    let prefix = prefix.to_owned();
    Arc::new(move |message: &str| emit(&format!("{prefix} {message}")))
}

/// The innermost error of `err`'s source chain: the exception the Python
/// pass raised, before the Rust wrappers added their context.
fn innermost<'a>(err: &'a (dyn Error + 'static)) -> &'a (dyn Error + 'static) {
    let mut current = err;
    while let Some(next) = current.source() {
        current = next;
    }
    current
}

/// The schema-mismatch marker `err` carries at this chain level, if any.
///
/// `#[error(transparent)]` wrappers forward `source()` to the inner error's
/// own source, so a wrapped [`OrchestratorSchemaError`]/[`StoreSchemaError`]
/// never appears as its own link in the chain: the wrapping `Schema`
/// variants are matched directly, exactly as the core classifier matches
/// `StoreError::Schema`.
fn schema_marker(err: &(dyn Error + 'static)) -> Option<&'static str> {
    if err.downcast_ref::<OrchestratorSchemaError>().is_some() {
        return Some("OrchestratorSchemaError");
    }
    if err.downcast_ref::<StoreSchemaError>().is_some() {
        return Some("StoreSchemaError");
    }
    if let Some(claims) = err.downcast_ref::<ClaimStoreError>()
        && matches!(claims, ClaimStoreError::Schema(_))
    {
        return Some("OrchestratorSchemaError");
    }
    if let Some(store) = err.downcast_ref::<StoreError>()
        && matches!(store, StoreError::Schema(_))
    {
        return Some("StoreSchemaError");
    }
    match err.downcast_ref::<StoreConfigError>() {
        Some(StoreConfigError::Core(StoreError::Schema(_))) => Some("StoreSchemaError"),
        Some(StoreConfigError::Claims(ClaimStoreError::Schema(_))) => {
            Some("OrchestratorSchemaError")
        }
        _ => None,
    }
}

/// The first schema-mismatch marker anywhere in `err`'s source chain.
fn schema_marker_in_chain(err: &(dyn Error + 'static)) -> Option<&'static str> {
    let mut current: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(level) = current {
        if let Some(marker) = schema_marker(level) {
            return Some(marker);
        }
        current = level.source();
    }
    None
}

/// The error's class name for the `(<name>: <message>)` log parenthetical,
/// standing in for Python's `type(exc).__name__`: a schema mismatch
/// anywhere in the chain is named as such; otherwise the innermost error
/// of the chain (the exception the Python pass raised, before the Rust
/// wrappers added their context) is named, with the Rust error types
/// mapped to the exception classes the Python shell would have raised.
pub fn error_type_name(err: &(dyn Error + 'static)) -> &'static str {
    if let Some(marker) = schema_marker_in_chain(err) {
        return marker;
    }
    let inner = innermost(err);
    if inner.downcast_ref::<StoreError>().is_some() {
        "StoreError"
    } else if inner.downcast_ref::<ClaimStoreError>().is_some() {
        "ClaimStoreError"
    } else if inner.downcast_ref::<StoreConfigError>().is_some() {
        "StoreConfigError"
    } else if inner.downcast_ref::<WorkSourceError>().is_some() {
        "WorkSourceError"
    } else if inner.downcast_ref::<RefillError>().is_some() {
        "RefillError"
    } else if inner.downcast_ref::<std::io::Error>().is_some() {
        "OSError"
    } else if inner.downcast_ref::<rusqlite::Error>().is_some() {
        "OperationalError"
    } else {
        "Exception"
    }
}

fn log_result(log: &LogSink, result: &AutopilotPassResult) {
    log(&result.reason);
    for path in &result.emitted_paths {
        log(&format!("emitted {}", python_path_str(path)));
    }
    for drop in &result.dropped {
        log(&format!("dropped {}: {}", drop.finding.id, drop.reason));
    }
}

// --- Runtime resolution -------------------------------------------------------------------

/// `(tasks_dir, target_depth, landing, weights)` resolved from flags + policy
/// with the documented precedence: CLI flag > policy > built-in default.
/// `landing` and the weights come from policy only (no CLI flag).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRuntime {
    pub tasks_dir: PathBuf,
    pub target_depth: i64,
    pub landing: String,
    pub weights: ScoreWeights,
}

/// Resolve the runtime knobs from `args` and the effective `policy`.
pub fn resolve_runtime(args: &AutopilotArgs, policy: Option<&WorkPolicy>) -> ResolvedRuntime {
    let tasks_dir = match args.tasks_dir.as_deref().filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => policy
            .and_then(|p| p.tasks_dir.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TASKS_DIR)),
    };
    let target_depth = args.target_depth.unwrap_or_else(|| {
        policy
            .map(|p| p.autopilot_target_depth)
            .unwrap_or(DEFAULT_TARGET_DEPTH)
    });
    let landing = policy
        .map(|p| p.autopilot_landing.clone())
        .unwrap_or_else(|| DEFAULT_LANDING.to_owned());
    let weights = policy
        .and_then(|p| p.autopilot_weights.clone())
        .unwrap_or_default();
    ResolvedRuntime {
        tasks_dir,
        target_depth,
        landing,
        weights,
    }
}

/// The store path the daemon's counters open: the policy's `[paths] db`
/// (default `.flywheel/flywheel.sqlite`), anchored under `repo_root` when
/// relative -- exactly as the worker resolves it.
pub fn resolve_daemon_db_path(policy: Option<&WorkPolicy>, repo_root: &Path) -> PathBuf {
    let raw = policy
        .and_then(|p| p.db_path.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DB_PATH));
    if raw.is_absolute() {
        raw
    } else {
        repo_root.join(raw)
    }
}

// --- One pass ---------------------------------------------------------------------------

/// Everything one production pass needs beyond its injected probe.
///
/// `deadlines` is the policy-resolved [`DeadlineConfig`] (spec 00066) whose
/// `[deadlines] autopilot_agent_seconds` ceiling bounds the discovery and
/// authoring agent calls; `None` is forwarded unchanged so the pass falls
/// back to the built-in ceilings. `agent_id`/`agent_transport` are the
/// policy's `[agent]` opt-in, forwarded to the pass's default invoker
/// constructions.
#[derive(Debug, Clone, PartialEq)]
pub struct SinglePassSpec {
    pub repo_root: PathBuf,
    pub tasks_dir: PathBuf,
    pub target_depth: i64,
    pub landing: String,
    pub weights: ScoreWeights,
    pub model: Option<String>,
    pub deadlines: Option<DeadlineConfig>,
    pub agent_id: Option<String>,
    pub agent_transport: Option<String>,
}

impl SinglePassSpec {
    /// A spec at the built-in defaults for `repo_root`/`tasks_dir`.
    pub fn new(repo_root: impl Into<PathBuf>, tasks_dir: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            tasks_dir: tasks_dir.into(),
            target_depth: DEFAULT_TARGET_DEPTH,
            landing: DEFAULT_LANDING.to_owned(),
            weights: ScoreWeights::default(),
            model: None,
            deadlines: None,
            agent_id: None,
            agent_transport: None,
        }
    }
}

/// Drive exactly one refill pass with the real agents-backed invokers.
///
/// `queue_depth` measures actionable work for the fill-to-target decision;
/// `None` falls back to the directory adapter's raw active-file count
/// (spec 00062: the daemon passes a store-backed counter that excludes
/// terminal tasks so a finished-but-unarchived task file never suppresses
/// refill).
pub async fn run_single_pass(
    spec: &SinglePassSpec,
    queue_depth: Option<&QueueDepthFn<'_>>,
) -> Result<AutopilotPassResult, RefillError> {
    let mut options = RefillPassOptions::new(&spec.tasks_dir, &spec.repo_root);
    options.target_depth = spec.target_depth;
    options.weights = spec.weights.clone();
    options.landing = spec.landing.clone();
    options.model = spec.model.clone();
    options.queue_depth = queue_depth;
    options.deadlines = spec.deadlines.clone();
    options.agent_id = spec.agent_id.clone();
    options.agent_transport = spec.agent_transport.clone();
    run_refill_pass(options).await
}

// --- Store-backed counters (best-effort) ----------------------------------------------------

/// Open the core store the actionable-depth counter reads, or `None`.
///
/// Spec 00062: a build failure (missing/locked db, missing backend extra)
/// degrades to the raw active-file count rather than crashing the daemon.
pub fn open_queue_depth_store(
    policy: Option<&WorkPolicy>,
    db_path: &Path,
    registries: &StoreRegistries,
    environ: &Environ,
    log: &LogSink,
) -> Option<Arc<dyn HarnessStore>> {
    match build_store(registries, &store_config_for(policy, db_path), environ) {
        Ok(store) => Some(store),
        Err(err) => {
            log(&format!(
                "queue-depth store unavailable ({}: {err}); falling back to raw active-file count",
                error_type_name(&err)
            ));
            None
        }
    }
}

/// Open the claim store the no-progress back-off records witnesses on, or
/// `None` (back-off disabled; the loop simply keeps running).
pub fn open_backoff_claims(
    policy: Option<&WorkPolicy>,
    db_path: &Path,
    registries: &StoreRegistries,
    environ: &Environ,
    log: &LogSink,
) -> Option<Arc<dyn ClaimStore>> {
    match build_claim_store(registries, &store_config_for(policy, db_path), environ) {
        Ok(claims) => Some(claims),
        Err(err) => {
            log(&format!(
                "no-progress back-off store unavailable ({}: {err}); back-off disabled",
                error_type_name(&err)
            ));
            None
        }
    }
}

// --- No-progress back-off around the refill pass (spec 00069, #9/#13) ------------------------

/// Did this refill cycle make observable progress on the repo unit?
///
/// Progress is a task authored this cycle OR a queue already at/above
/// target -- healthy backpressure, not a dead-end, exactly as the no-op
/// ledger treats it. Only a below-target cycle that did discovery work yet
/// authored nothing is a genuine no-progress cycle.
pub fn cycle_made_progress(result: &AutopilotPassResult) -> bool {
    if result.emitted_count() > 0 {
        return true;
    }
    i64::try_from(result.queue_depth_before).unwrap_or(i64::MAX) >= result.target_depth
}

/// Feed one autopilot cycle to the bounded no-progress back-off re-driver.
///
/// The autopilot repo IS the "unit": after `bound` consecutive
/// below-target cycles that authored nothing the re-driver backs the repo
/// off -- routing it to the human-review queue with the machine-readable
/// `no-progress` reason -- and a `Queued` outcome is the daemon's cue to
/// stop. A cycle that makes progress resets the streak.
pub fn apply_no_progress_backoff(
    result: &AutopilotPassResult,
    claims: &dyn ClaimStore,
    unit_id: &str,
    bound: u32,
    now: &Clock,
    log: Option<&LogSink>,
) -> Result<NoProgressOutcome, OrchestrateError> {
    let observation = NoProgressObservation {
        unit_id: unit_id.to_owned(),
        progressed: cycle_made_progress(result),
        detail: result.reason.clone(),
        task_id: String::new(),
    };
    redrive_no_progress(claims, &[observation], bound, now, log)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            OrchestrateError::Config("no-progress re-driver returned no outcome".to_owned())
        })
}

// --- The neverending daemon loop ---------------------------------------------------------------

/// Consecutive whole-cycle failures (a refill pass erroring) before the
/// daemon gives up so an operator can inspect, rather than hot-looping.
/// Mirrors the worker daemon's cross-cycle backstop: a single failing cycle
/// is counted and backed off so the loop runs further cycles; a subsequent
/// success resets the count; on the bounded count the loop stops by
/// surfacing a give-up signal the caller turns into a non-zero exit (never
/// a silent exit).
pub const MAX_CONSECUTIVE_CYCLE_FAILURES: u32 = 5;
/// Seconds the daemon backs off after a failed cycle before the next one.
pub const CYCLE_FAILURE_BACKOFF_SECONDS: f64 = 10.0;

/// Any error one cycle can fail with.
pub type CycleError = Box<dyn Error + Send + Sync + 'static>;

/// Bucket a cycle's error as TRANSIENT/PERMANENT, or `None` when out of
/// scope. The core classifier ([`classify_error`]) knows the core store's
/// schema mismatch; the orchestrator store's [`OrchestratorSchemaError`] is
/// the same PERMANENT marker (the Python classes share one base), so it is
/// recognized here anywhere in the source chain.
pub fn classify_cycle_fault(err: &(dyn Error + 'static)) -> Option<FaultClass> {
    if schema_marker_in_chain(err).is_some() {
        return Some(FaultClass::Permanent);
    }
    classify_error(err)
}

/// A failed-cycle observer: the error and the consecutive failure count.
pub type CycleFailureHook<'a> = &'a dyn Fn(&CycleError, u32);

/// The optional observers of [`run_daemon_loop`].
#[derive(Default, Clone, Copy)]
pub struct DaemonHooks<'a> {
    /// Runs before every cycle.
    pub before_cycle: Option<&'a dyn Fn()>,
    /// Runs after every successful cycle with its result.
    pub on_cycle: Option<&'a dyn Fn(&AutopilotPassResult)>,
    /// Runs after every transiently failed cycle with the error and the
    /// consecutive failure count.
    pub on_cycle_failure: Option<CycleFailureHook<'a>>,
    /// Runs once when the circuit breaker gives up.
    pub on_give_up: Option<&'a dyn Fn(u32)>,
    /// Runs once when a PERMANENT fault stops the loop (distinct from the
    /// transient breaker so a caller can assert on it).
    pub on_permanent_stop: Option<&'a dyn Fn(&CycleError)>,
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
            .field("on_permanent_stop", &self.on_permanent_stop.is_some())
            .field("max_cycles", &self.max_cycles)
            .finish()
    }
}

/// Run refill passes on an interval until an explicit stop signal.
///
/// The testable core of `flywheel autopilot` (no `--once`): each cycle runs
/// one `run_cycle` pass, then waits `interval_seconds` before the next. It
/// MUST NOT terminate on an idle (nothing-actionable) cycle -- an empty
/// pass writes nothing and the loop continues (D-5). The loop exits only
/// when `should_stop()` is true (an injected flag in tests; a
/// SIGTERM/SIGINT flag in production), or when the circuit breaker gives
/// up, or on a permanent fault, and at no other time.
///
/// Circuit breaker (mirrors the worker daemon): a `run_cycle` that errors is
/// contained -- the error is counted, `on_cycle_failure` is notified, and
/// the loop backs off [`CYCLE_FAILURE_BACKOFF_SECONDS`] (via the injected,
/// interruptible `sleep`) before running a further cycle. A subsequent
/// successful cycle resets the consecutive-failure count. After
/// [`MAX_CONSECUTIVE_CYCLE_FAILURES`] consecutive failures the loop stops
/// and calls `on_give_up` so the caller can surface a visible non-zero
/// signal -- never a silent exit.
///
/// Permanent stop (distinct from the transient breaker): a fault that
/// [`classify_cycle_fault`] buckets PERMANENT -- a schema-version mismatch
/// reopening a store every cycle -- can never succeed on retry. It is NOT
/// counted as a transient strike (it would otherwise burn all five and
/// back off between each); the loop counts exactly this one cycle, calls
/// `on_permanent_stop`, and stops immediately.
///
/// Every collaborator is injected so the loop runs with no real wall-clock
/// waits and no live model. Returns the number of cycles attempted
/// (successful and failed).
pub fn run_daemon_loop(
    run_cycle: &mut dyn FnMut() -> Result<AutopilotPassResult, CycleError>,
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
                if classify_cycle_fault(err.as_ref()) == Some(FaultClass::Permanent) {
                    cycles += 1;
                    if let Some(on_permanent_stop) = hooks.on_permanent_stop {
                        on_permanent_stop(&err);
                    }
                    break;
                }
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

// --- The activity / liveness record -----------------------------------------------------------

/// The liveness record's freshness window is one interval (the gap between
/// the daemon's record writes while it idles between cycles) plus this
/// grace, so a healthy daemon -- idling between cycles or busy inside one
/// -- always writes its next record before the deadline lapses, exactly as
/// the worker's lease (300s) comfortably outlives its heartbeat (10s). Kept
/// modestly larger than one interval so a dead daemon's record still
/// lapses within roughly one cycle rather than reading live for hours --
/// the "don't adopt a corpse" bound.
pub const LIVENESS_GRACE_SECONDS: f64 = 60.0;

/// An epoch-seconds clock; injected so `next_cycle_at`/`expires_at` are
/// deterministic in tests.
pub type EpochClock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// The previous cycle's summary the console keeps showing while the next
/// cycle runs. Only the summary fields are retained, never the full pass
/// result.
#[derive(Debug, Clone, PartialEq, Default)]
struct LastCycle {
    emitted: i64,
    dropped: i64,
    reason: String,
    relevant_tiers: Vec<u8>,
    emitted_tasks: Vec<EmittedSummary>,
}

impl LastCycle {
    fn from_result(result: &AutopilotPassResult) -> Self {
        Self {
            emitted: i64::try_from(result.emitted_count()).unwrap_or(i64::MAX),
            dropped: i64::try_from(result.dropped.len()).unwrap_or(i64::MAX),
            reason: result.reason.clone(),
            relevant_tiers: result.relevant_tiers.iter().map(|t| u8::from(*t)).collect(),
            emitted_tasks: result
                .emitted
                .iter()
                .map(|e| EmittedSummary {
                    task_id: e.task.id.clone(),
                    tier: u8::from(e.finding.tier),
                })
                .collect(),
        }
    }
}

#[derive(Default)]
struct RecorderState {
    cycle_index: i64,
    last: Option<LastCycle>,
}

/// Writes the daemon's per-cycle activity snapshots for the console.
///
/// `starting` before the first cycle, `running` at each cycle start
/// (carrying the *previous* cycle's summary so the console keeps showing
/// "last: N emitted" while the next cycle runs), and `idle` with
/// `next_cycle_at` after each cycle. Snapshot I/O is best-effort -- a write
/// failure never crashes a cycle.
///
/// Every snapshot doubles as the daemon's liveness record: each write
/// stamps `expires_at = now + interval + LIVENESS_GRACE_SECONDS` so a second
/// supervisor reading the file (via `read_live_activity`) sees a fresh
/// deadline while this daemon lives and a lapsed one once it dies.
pub struct ActivityRecorder {
    path: PathBuf,
    interval: f64,
    pid: i64,
    clock: EpochClock,
    state: Mutex<RecorderState>,
}

impl fmt::Debug for ActivityRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActivityRecorder")
            .field("path", &self.path)
            .field("interval", &self.interval)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ActivityRecorder {
    /// A recorder for this process on the shared machine clock.
    pub fn new(path: impl Into<PathBuf>, interval_seconds: f64) -> Self {
        Self {
            path: path.into(),
            interval: interval_seconds,
            pid: i64::from(std::process::id()),
            clock: Arc::new(epoch_now),
            state: Mutex::new(RecorderState::default()),
        }
    }

    /// Record under `pid` instead of this process's id.
    pub fn with_pid(mut self, pid: i64) -> Self {
        self.pid = pid;
        self
    }

    /// Stamp timestamps from `clock` instead of the machine clock.
    pub fn with_clock(mut self, clock: EpochClock) -> Self {
        self.clock = clock;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, phase: &str, now: f64, next_cycle_at: Option<f64>) {
        let (cycle_index, last) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (state.cycle_index, state.last.clone().unwrap_or_default())
        };
        let activity = AutopilotActivity {
            pid: self.pid,
            phase: phase.to_owned(),
            cycle_index,
            updated_at: now,
            interval_seconds: self.interval,
            next_cycle_at,
            expires_at: Some(now + self.interval + LIVENESS_GRACE_SECONDS),
            last_emitted: last.emitted,
            last_dropped: last.dropped,
            last_reason: last.reason,
            last_relevant_tiers: last.relevant_tiers,
            last_emitted_tasks: last.emitted_tasks,
        };
        // Best-effort by contract: a snapshot write failure never crashes a cycle.
        let _ = write_activity(&self.path, &activity);
    }

    /// The `starting` snapshot, before the first cycle.
    pub fn starting(&self) {
        self.write(PHASE_STARTING, (self.clock)(), None);
    }

    /// The `running` snapshot at a cycle start (bumps the cycle index).
    pub fn before_cycle(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.cycle_index += 1;
        }
        self.write(PHASE_RUNNING, (self.clock)(), None);
    }

    /// The `idle` snapshot after a cycle, carrying its summary.
    pub fn on_cycle(&self, result: &AutopilotPassResult) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.last = Some(LastCycle::from_result(result));
        }
        let now = (self.clock)();
        self.write(PHASE_IDLE, now, Some(now + self.interval));
    }
}

// --- Signals -----------------------------------------------------------------------------------

/// A stop-signal observer: receives the signal number.
pub type SignalHandler = Arc<dyn Fn(i32) + Send + Sync>;

/// SIGTERM's number on every unix.
pub const SIGTERM: i32 = 15;
/// SIGINT's number on every unix.
pub const SIGINT: i32 = 2;

/// Call `on_signal(signum)` on every SIGTERM/SIGINT for the life of the
/// process (the handler stays armed across cycles, as Python's re-arming
/// achieved).
#[cfg(unix)]
pub(crate) async fn watch_stop_signals(on_signal: SignalHandler, log: LogSink) {
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
            _ = term.recv() => SIGTERM,
            _ = int.recv() => SIGINT,
        };
        on_signal(signum);
    }
}

#[cfg(not(unix))]
pub(crate) async fn watch_stop_signals(on_signal: SignalHandler, log: LogSink) {
    loop {
        if let Err(err) = tokio::signal::ctrl_c().await {
            log(&format!("could not install the interrupt handler: {err}"));
            return;
        }
        on_signal(SIGINT);
    }
}

// --- The host seam -----------------------------------------------------------------------------

/// The process-level effects `flywheel autopilot` performs, behind one seam
/// so [`run_main`] is hermetic in tests.
pub trait AutopilotHost: Send + Sync {
    /// An explicit policy file, or `None` to auto-detect from the working
    /// directory (`load_effective_policy`).
    fn policy_path(&self) -> Option<PathBuf> {
        None
    }

    /// The raw stderr line sink (no prefix).
    fn stderr(&self) -> LogSink;

    /// The repo root a pass runs in; the error is the message to print
    /// before exiting 1.
    fn repo_root(&self) -> Result<PathBuf, String>;

    /// The core store behind the actionable-depth counter, or `None` to
    /// fall back to the raw active-file count.
    fn queue_depth_store(
        &self,
        policy: Option<&WorkPolicy>,
        db_path: &Path,
        log: &LogSink,
    ) -> Option<Arc<dyn HarnessStore>> {
        open_queue_depth_store(
            policy,
            db_path,
            &default_store_registries(),
            &environ_from_process(),
            log,
        )
    }

    /// The claim store the no-progress back-off witnesses on, or `None`
    /// to disable back-off.
    fn backoff_claims(
        &self,
        policy: Option<&WorkPolicy>,
        db_path: &Path,
        log: &LogSink,
    ) -> Option<Arc<dyn ClaimStore>> {
        open_backoff_claims(
            policy,
            db_path,
            &default_store_registries(),
            &environ_from_process(),
            log,
        )
    }

    /// Run one refill pass.
    fn run_cycle(
        &self,
        spec: &SinglePassSpec,
        queue_depth: Option<&QueueDepthFn<'_>>,
    ) -> Result<AutopilotPassResult, CycleError>;

    /// Arm the daemon's stop signals: SIGTERM/SIGINT set `stop` and log the
    /// `stop signal <n>; exiting after the current cycle` line.
    fn arm_stop_signals(&self, stop: &StopFlag, log: &LogSink);

    /// Wait between cycles: up to `seconds`, waking as soon as `stop` is
    /// set ([`interruptible_sleep`] in production; a no-op in tests).
    fn sleep(&self, stop: &StopFlag, seconds: f64) {
        interruptible_sleep(stop, seconds);
    }

    /// The epoch-seconds clock the activity record is stamped from.
    fn epoch_clock(&self) -> EpochClock {
        Arc::new(epoch_now)
    }

    /// The wall clock the no-progress witnesses are stamped from.
    fn utc_clock(&self) -> Clock {
        Arc::new(Utc::now)
    }
}

/// The real host: stderr, `git`, the store factories, a tokio runtime
/// driving the pass, and OS signal handlers.
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

impl AutopilotHost for ProductionHost {
    fn stderr(&self) -> LogSink {
        Arc::new(|line: &str| eprintln!("{line}"))
    }

    fn repo_root(&self) -> Result<PathBuf, String> {
        resolve_repo_root()
    }

    fn run_cycle(
        &self,
        spec: &SinglePassSpec,
        queue_depth: Option<&QueueDepthFn<'_>>,
    ) -> Result<AutopilotPassResult, CycleError> {
        self.runtime
            .block_on(run_single_pass(spec, queue_depth))
            .map_err(|err| Box::new(err) as CycleError)
    }

    fn arm_stop_signals(&self, stop: &StopFlag, log: &LogSink) {
        let stop = stop.clone();
        let signal_log = log.clone();
        let on_signal: SignalHandler = Arc::new(move |signum: i32| {
            stop.set();
            signal_log(&format!(
                "stop signal {signum}; exiting after the current cycle"
            ));
        });
        let log = log.clone();
        self.runtime
            .spawn(async move { watch_stop_signals(on_signal, log).await });
    }
}

// --- main -------------------------------------------------------------------------------------

/// Run `flywheel autopilot` with the real host; returns the process exit code.
pub fn main(argv: &[String]) -> i32 {
    match ProductionHost::new() {
        Ok(host) => run_main(argv, &host),
        Err(err) => {
            eprintln!("flywheel autopilot: cannot start the runtime: {err}");
            1
        }
    }
}

/// Run `flywheel autopilot`: a neverending daemon, or one pass.
///
/// Bare `flywheel autopilot` runs the neverending refill loop on the
/// configured interval (idling, not exiting, on an empty cycle, D-5),
/// stopping only on SIGTERM/SIGINT, on a bounded no-progress back-off, or
/// when the circuit breaker gives up. `--once` runs exactly one refill
/// pass and exits 0 (the testable unit and a manual escape hatch). Mirrors
/// `flywheel worker` (daemon by default, `--once` for a single drain).
///
/// Exit codes: 2 for a bad argument or a policy error; 1 when the repo
/// root cannot be resolved, a `--once` pass fails, the breaker gives up,
/// or a permanent fault stops the loop; 0 otherwise.
pub fn run_main(argv: &[String], host: &dyn AutopilotHost) -> i32 {
    let stderr = host.stderr();
    let args = match parse_args(argv) {
        Ok(args) => args,
        Err(message) => {
            stderr(USAGE);
            stderr(&format!("flywheel autopilot: error: {message}"));
            return 2;
        }
    };
    if args.help {
        println!("{HELP}");
        return 0;
    }
    let repo_root = match host.repo_root() {
        Ok(root) => root,
        Err(message) => {
            stderr(&message);
            return 1;
        }
    };
    let log = make_logger(LOG_PREFIX, stderr.clone());

    let policy = match load_effective_policy(host.policy_path().as_deref()) {
        Ok(policy) => policy,
        Err(err) => {
            stderr(&format!("flywheel autopilot: policy error: {err}"));
            return 2;
        }
    };
    let policy = policy.as_ref();

    let runtime = resolve_runtime(&args, policy);
    let model = args
        .model
        .clone()
        .or_else(|| policy.and_then(|p| p.model.clone()));
    // Resolve the store path once; both best-effort counters open from it.
    let db_path = resolve_daemon_db_path(policy, &repo_root);
    let depth_store = host.queue_depth_store(policy, &db_path, &log);
    let queue_depth = depth_store
        .map(|store| move |tasks_dir: &Path| actionable_queue_depth(tasks_dir, store.as_ref()));
    let queue_depth_probe: Option<&QueueDepthFn<'_>> = match &queue_depth {
        Some(probe) => Some(probe),
        None => None,
    };
    let spec = SinglePassSpec {
        repo_root: repo_root.clone(),
        tasks_dir: runtime.tasks_dir.clone(),
        target_depth: runtime.target_depth,
        landing: runtime.landing.clone(),
        weights: runtime.weights.clone(),
        model,
        // A loaded policy always carries a DeadlineConfig (spec 00066); a
        // None policy forwards None, which the pass maps to the built-in
        // ceilings.
        deadlines: policy.map(|p| p.deadlines.clone()),
        agent_id: policy.and_then(|p| p.agent_id.clone()),
        agent_transport: policy.and_then(|p| p.agent_transport.clone()),
    };
    let repo_text = python_path_str(&repo_root);
    let tasks_text = python_path_str(&runtime.tasks_dir);
    let mut run_cycle = || host.run_cycle(&spec, queue_depth_probe);

    if args.once {
        log(&format!(
            "single pass repo={repo_text} tasks={tasks_text} target_depth={} landing={}",
            runtime.target_depth, runtime.landing
        ));
        return match run_cycle() {
            Ok(result) => {
                log_result(&log, &result);
                0
            }
            Err(err) => {
                stderr(&format!("flywheel autopilot: {err}"));
                1
            }
        };
    }

    let interval = args.interval.unwrap_or_else(|| {
        policy
            .map(|p| p.autopilot_interval_seconds)
            .unwrap_or(DEFAULT_AUTOPILOT_INTERVAL_SECONDS)
    });
    log(&format!(
        "daemon started repo={repo_text} tasks={tasks_text} target_depth={} landing={} \
         interval={}s pid={}",
        runtime.target_depth,
        runtime.landing,
        py_float_repr(interval),
        std::process::id()
    ));

    let activity_path = match args.activity_file.as_deref().filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => repo_root
            .join(".flywheel")
            .join("logs")
            .join("autopilot")
            .join("activity.json"),
    };
    let recorder = ActivityRecorder::new(activity_path, interval).with_clock(host.epoch_clock());
    recorder.starting();

    let stop = StopFlag::new();
    host.arm_stop_signals(&stop, &log);
    let gave_up = Cell::new(false);

    // Bounded no-progress back-off (spec 00069, #9/#13): a repo that keeps
    // authoring nothing while below target is backed off after a finite
    // number of fruitless cycles -- routed to the human-review queue and no
    // longer re-attempted -- so a never-progressing repo cannot burn agent
    // cost forever.
    let backoff_claims = host.backoff_claims(policy, &db_path, &log);
    let backoff_unit = tasks_text.clone();
    let now_clock = host.utc_clock();

    let should_stop = || stop.is_set();
    let sleep = |seconds: f64, _should_stop: &dyn Fn() -> bool| host.sleep(&stop, seconds);
    let before_cycle = || recorder.before_cycle();
    let on_cycle = |result: &AutopilotPassResult| {
        log_result(&log, result);
        recorder.on_cycle(result);
        if let Some(claims) = &backoff_claims {
            match apply_no_progress_backoff(
                result,
                claims.as_ref(),
                &backoff_unit,
                DEFAULT_NO_PROGRESS_BOUND,
                &now_clock,
                None,
            ) {
                Ok(outcome) if outcome.result == NoProgressResult::Queued => {
                    stop.set();
                    log(&format!(
                        "no progress for {} consecutive cycle(s); backed off and routed to the \
                         human-review queue (reason 'no-progress'); stopping.",
                        outcome.cycles
                    ));
                }
                Ok(_) => {}
                // A ledger hiccup must not take the daemon down: the
                // back-off is best-effort, like its store open.
                Err(err) => log(&format!(
                    "no-progress back-off failed ({}: {err}); continuing",
                    error_type_name(&err)
                )),
            }
        }
    };
    let on_cycle_failure = |err: &CycleError, consecutive: u32| {
        log(&format!(
            "cycle failed ({}: {err}) [{consecutive}/{MAX_CONSECUTIVE_CYCLE_FAILURES}]",
            error_type_name(err.as_ref())
        ));
    };
    let on_give_up = |consecutive: u32| {
        gave_up.set(true);
        log(&format!(
            "too many consecutive cycle failures ({consecutive}); exiting non-zero for \
             operator inspection."
        ));
    };
    let on_permanent_stop = |err: &CycleError| {
        gave_up.set(true);
        log(&format!(
            "permanent fault ({}: {err}); cannot succeed on retry; exiting non-zero for \
             operator inspection.",
            error_type_name(err.as_ref())
        ));
    };
    let cycles = run_daemon_loop(
        &mut run_cycle,
        interval,
        &should_stop,
        &sleep,
        DaemonHooks {
            before_cycle: Some(&before_cycle),
            on_cycle: Some(&on_cycle),
            on_cycle_failure: Some(&on_cycle_failure),
            on_give_up: Some(&on_give_up),
            on_permanent_stop: Some(&on_permanent_stop),
            max_cycles: None,
        },
    );
    log(&format!("daemon stopped after {cycles} cycle(s)"));
    if gave_up.get() { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::{DroppedFinding, EmittedTask, Finding, GraderSource, Tier};
    use crate::autopilot_activity::{is_record_live, read_activity, read_live_activity};
    use crate::claims::{InMemoryClaimStore, REASON_NO_PROGRESS, STOP_NO_PROGRESS};
    use flywheel_core::deadline_config::{DEFAULT_AUTOPILOT_AGENT_SECONDS, DeadlineClass};
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::{CommandGrader, Grader, Task};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    // --- helpers -------------------------------------------------------------------------

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    fn noop_sleep(_seconds: f64, _should_stop: &dyn Fn() -> bool) {}

    fn idle(reason: &str) -> AutopilotPassResult {
        AutopilotPassResult {
            reason: reason.to_owned(),
            target_depth: 5,
            queue_depth_before: 0,
            ..AutopilotPassResult::default()
        }
    }

    fn emitted_task(task_id: &str, tier: Tier) -> EmittedTask {
        let task = Task::new(
            "cover it",
            vec![Grader::Command(CommandGrader::new("pytest").unwrap())],
        )
        .with_id(task_id);
        EmittedTask::new(
            Finding::new(format!("find-{task_id}"), tier, task_id),
            task,
            "pytest",
            GraderSource::RepoCommand,
            "tests/x.py",
        )
    }

    fn result_with(
        emitted: Vec<EmittedTask>,
        dropped: Vec<DroppedFinding>,
        relevant: Vec<Tier>,
        reason: &str,
    ) -> AutopilotPassResult {
        AutopilotPassResult {
            emitted_paths: emitted
                .iter()
                .map(|e| PathBuf::from(format!("{}.json", e.task.id)))
                .collect(),
            emitted,
            dropped,
            relevant_tiers: relevant,
            reason: reason.to_owned(),
            target_depth: 5,
            ..AutopilotPassResult::default()
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct Boom(String);

    fn boom(message: &str) -> CycleError {
        Box::new(Boom(message.to_owned()))
    }

    fn orchestrator_schema_fault() -> CycleError {
        Box::new(RefillError::StopLedger(ClaimStoreError::Schema(
            OrchestratorSchemaError {
                observed: Some(99),
                expected: 1,
            },
        )))
    }

    fn write_policy(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("flywheel.toml");
        std::fs::write(&path, format!("[source]\nkind = \"directory\"\n{body}")).unwrap();
        path
    }

    // --- argument parsing ------------------------------------------------------------------

    #[test]
    fn parse_args_mirrors_argparse() {
        assert_eq!(parse_args(&[]).unwrap(), AutopilotArgs::default());
        let parsed = parse_args(&argv(&[
            "--once",
            "--tasks-dir",
            "t",
            "--model",
            "m",
            "--target-depth",
            "8",
            "--interval",
            "2.5",
            "--activity-file",
            "a.json",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            AutopilotArgs {
                tasks_dir: Some("t".into()),
                model: Some("m".into()),
                target_depth: Some(8),
                interval: Some(2.5),
                once: true,
                activity_file: Some("a.json".into()),
                help: false,
            }
        );
        let parsed = parse_args(&argv(&["--model=a", "--model=b", "--interval=7"])).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("b"));
        assert_eq!(parsed.interval, Some(7.0));
        assert!(parse_args(&argv(&["-h"])).unwrap().help);
        assert!(parse_args(&argv(&["--help"])).unwrap().help);
        assert_eq!(
            parse_args(&argv(&["--interval", "-1"])).unwrap().interval,
            Some(-1.0)
        );
        assert_eq!(
            parse_args(&argv(&["--target-depth", " +1_000 "]))
                .unwrap()
                .target_depth,
            Some(1000)
        );
    }

    #[test]
    fn parse_args_accepts_unique_abbreviations_and_reports_ambiguity() {
        let parsed = parse_args(&argv(&["--targ", "3", "--inter=1", "--on"])).unwrap();
        assert_eq!(parsed.target_depth, Some(3));
        assert_eq!(parsed.interval, Some(1.0));
        assert!(parsed.once);
        assert_eq!(
            parse_args(&argv(&["--t", "1"])).unwrap_err(),
            "ambiguous option: --t could match --tasks-dir, --target-depth"
        );
        assert_eq!(
            parse_args(&argv(&["--t=1"])).unwrap_err(),
            "ambiguous option: --t=1 could match --tasks-dir, --target-depth"
        );
    }

    #[test]
    fn parse_args_errors_match_argparse_messages() {
        assert_eq!(
            parse_args(&argv(&["--interval", "x"])).unwrap_err(),
            "argument --interval: invalid float value: 'x'"
        );
        assert_eq!(
            parse_args(&argv(&["--target-depth", "1.5"])).unwrap_err(),
            "argument --target-depth: invalid int value: '1.5'"
        );
        assert_eq!(
            parse_args(&argv(&["--model"])).unwrap_err(),
            "argument --model: expected one argument"
        );
        assert_eq!(
            parse_args(&argv(&["--model", "--once"])).unwrap_err(),
            "argument --model: expected one argument"
        );
        assert_eq!(
            parse_args(&argv(&["--once=1"])).unwrap_err(),
            "argument --once: ignored explicit argument '1'"
        );
        assert_eq!(
            parse_args(&argv(&["--bogus", "foo", "-x"])).unwrap_err(),
            "unrecognized arguments: --bogus foo -x"
        );
        // A type error is raised during the parse, before the unrecognized
        // tokens are reported.
        assert_eq!(
            parse_args(&argv(&["--bogus", "--interval", "x"])).unwrap_err(),
            "argument --interval: invalid float value: 'x'"
        );
    }

    #[test]
    fn negative_number_detection_matches_argparse() {
        assert!(looks_like_negative_number("-1"));
        assert!(looks_like_negative_number("-.5"));
        assert!(looks_like_negative_number("-10.25"));
        assert!(!looks_like_negative_number("-"));
        assert!(!looks_like_negative_number("-x"));
        assert!(!looks_like_negative_number("--1"));
        assert!(!looks_like_negative_number("1"));
    }

    // --- runtime resolution ----------------------------------------------------------------

    #[test]
    fn resolve_runtime_prefers_flag_then_policy_then_default() {
        let dir = TempDir::new().unwrap();
        let path = write_policy(
            dir.path(),
            "tasks_dir = \"queue\"\n[autopilot]\ntarget_depth = 7\nlanding = \"pr\"\n\
             interval_seconds = 42\n[autopilot.weights]\ntier = 1.5\n",
        );
        let policy = load_effective_policy(Some(&path)).unwrap().unwrap();

        let defaults = resolve_runtime(&AutopilotArgs::default(), None);
        assert_eq!(defaults.tasks_dir, PathBuf::from(DEFAULT_TASKS_DIR));
        assert_eq!(defaults.target_depth, DEFAULT_TARGET_DEPTH);
        assert_eq!(defaults.landing, DEFAULT_LANDING);
        assert_eq!(defaults.weights, ScoreWeights::default());

        let from_policy = resolve_runtime(&AutopilotArgs::default(), Some(&policy));
        assert_eq!(from_policy.tasks_dir, PathBuf::from("queue"));
        assert_eq!(from_policy.target_depth, 7);
        assert_eq!(from_policy.landing, "pr");
        assert_eq!(from_policy.weights.tier, 1.5);

        let from_flags = resolve_runtime(
            &AutopilotArgs {
                tasks_dir: Some("flag-dir".into()),
                target_depth: Some(2),
                ..AutopilotArgs::default()
            },
            Some(&policy),
        );
        assert_eq!(from_flags.tasks_dir, PathBuf::from("flag-dir"));
        assert_eq!(from_flags.target_depth, 2);
        // landing and the weights come from policy only.
        assert_eq!(from_flags.landing, "pr");
        assert_eq!(from_flags.weights.tier, 1.5);
        // An empty --tasks-dir counts as unspecified (Python truthiness).
        let empty = resolve_runtime(
            &AutopilotArgs {
                tasks_dir: Some(String::new()),
                ..AutopilotArgs::default()
            },
            Some(&policy),
        );
        assert_eq!(empty.tasks_dir, PathBuf::from("queue"));
    }

    #[test]
    fn daemon_db_path_anchors_relative_paths_under_the_repo() {
        let repo = Path::new("/repo");
        assert_eq!(
            resolve_daemon_db_path(None, repo),
            PathBuf::from("/repo/.flywheel/flywheel.sqlite")
        );
        let dir = TempDir::new().unwrap();
        let relative = write_policy(dir.path(), "[paths]\ndb = \"state/fw.sqlite\"\n");
        let policy = load_effective_policy(Some(&relative)).unwrap().unwrap();
        assert_eq!(
            resolve_daemon_db_path(Some(&policy), repo),
            PathBuf::from("/repo/state/fw.sqlite")
        );
        let absolute = write_policy(dir.path(), "[paths]\ndb = \"/var/fw.sqlite\"\n");
        let policy = load_effective_policy(Some(&absolute)).unwrap().unwrap();
        assert_eq!(
            resolve_daemon_db_path(Some(&policy), repo),
            PathBuf::from("/var/fw.sqlite")
        );
    }

    // --- no-progress back-off -------------------------------------------------------------------

    #[test]
    fn progress_is_an_emit_or_a_full_queue() {
        assert!(!cycle_made_progress(&idle("nothing actionable")));
        assert!(cycle_made_progress(&result_with(
            vec![emitted_task("fix-a", Tier::CoreFeatureWork)],
            vec![],
            vec![],
            "emitted 1"
        )));
        let full = AutopilotPassResult {
            queue_depth_before: 5,
            target_depth: 5,
            ..AutopilotPassResult::default()
        };
        assert!(cycle_made_progress(&full));
    }

    #[test]
    fn backoff_queues_the_repo_after_the_bound_and_resets_on_progress() {
        let claims = InMemoryClaimStore::new();
        let now: Clock = Arc::new(Utc::now);
        let unit = "/repo/.flywheel/tasks";
        let first = apply_no_progress_backoff(&idle("idle"), &claims, unit, 3, &now, None).unwrap();
        assert_eq!(first.result, NoProgressResult::Waiting);
        assert_eq!(first.cycles, 1);
        let productive = result_with(
            vec![emitted_task("fix-a", Tier::CoreFeatureWork)],
            vec![],
            vec![],
            "emitted 1",
        );
        let reset = apply_no_progress_backoff(&productive, &claims, unit, 3, &now, None).unwrap();
        assert_eq!(reset.result, NoProgressResult::Progressed);
        for expected in 1..=2 {
            let outcome =
                apply_no_progress_backoff(&idle("idle"), &claims, unit, 3, &now, None).unwrap();
            assert_eq!(outcome.result, NoProgressResult::Waiting);
            assert_eq!(outcome.cycles, expected);
        }
        let queued =
            apply_no_progress_backoff(&idle("idle again"), &claims, unit, 3, &now, None).unwrap();
        assert_eq!(queued.result, NoProgressResult::Queued);
        assert_eq!(queued.cycles, 3);
        let queue = claims.list_human_review_queue().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].reason, REASON_NO_PROGRESS);
        assert_eq!(queue[0].task_id, unit);
        let witnesses = claims
            .list_subject_stop_events(unit)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == STOP_NO_PROGRESS)
            .count();
        assert_eq!(witnesses, 4);
    }

    // --- the daemon loop ------------------------------------------------------------------------

    #[test]
    fn idle_cycle_does_not_exit_and_a_later_cycle_authors() {
        let calls = Cell::new(0u32);
        let results = Mutex::new(Vec::new());
        let mut run_cycle = || {
            calls.set(calls.get() + 1);
            let result = if calls.get() == 1 {
                idle("idle")
            } else {
                result_with(
                    vec![emitted_task("fix-a", Tier::CoreFeatureWork)],
                    vec![],
                    vec![Tier::CoreFeatureWork],
                    "emitted 1",
                )
            };
            results.lock().unwrap().push(result.clone());
            Ok(result)
        };
        let stop = Cell::new(false);
        let sleeps = Cell::new(0u32);
        let sleep = |_seconds: f64, _should_stop: &dyn Fn() -> bool| {
            sleeps.set(sleeps.get() + 1);
            if sleeps.get() >= 2 {
                stop.set(true);
            }
        };
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| stop.get(),
            &sleep,
            DaemonHooks {
                max_cycles: Some(10),
                ..DaemonHooks::default()
            },
        );
        assert!(cycles >= 2);
        let results = results.lock().unwrap();
        assert_eq!(results[0].emitted_count(), 0);
        assert!(results[1..].iter().any(|r| r.emitted_count() >= 1));
    }

    #[test]
    fn loop_runs_until_stop_signal_not_until_empty() {
        let calls = Cell::new(0u32);
        let stop = Cell::new(false);
        let mut run_cycle = || {
            calls.set(calls.get() + 1);
            Ok(idle("idle"))
        };
        let sleep = |_seconds: f64, _should_stop: &dyn Fn() -> bool| {
            if calls.get() >= 3 {
                stop.set(true);
            }
        };
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| stop.get(),
            &sleep,
            DaemonHooks {
                max_cycles: Some(50),
                ..DaemonHooks::default()
            },
        );
        assert!(cycles >= 3);
        assert_eq!(calls.get(), cycles);
    }

    #[test]
    fn stop_before_first_cycle_runs_nothing() {
        let mut run_cycle = || panic!("no cycle should run when stop is set up front");
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
    fn one_failing_cycle_is_contained_then_a_further_cycle_runs() {
        let calls = Cell::new(0u32);
        let failures = Mutex::new(Vec::new());
        let gave_up = Mutex::new(Vec::new());
        let stop = Cell::new(false);
        let mut run_cycle = || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(boom("transient cycle failure"))
            } else {
                Ok(idle("recovered"))
            }
        };
        let sleep = |_seconds: f64, _should_stop: &dyn Fn() -> bool| {
            if calls.get() >= 2 {
                stop.set(true);
            }
        };
        let on_cycle_failure = |err: &CycleError, n: u32| {
            failures.lock().unwrap().push((err.to_string(), n));
        };
        let on_give_up = |n: u32| gave_up.lock().unwrap().push(n);
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| stop.get(),
            &sleep,
            DaemonHooks {
                on_cycle_failure: Some(&on_cycle_failure),
                on_give_up: Some(&on_give_up),
                max_cycles: Some(50),
                ..DaemonHooks::default()
            },
        );
        assert!(calls.get() >= 2);
        assert!(gave_up.lock().unwrap().is_empty());
        assert_eq!(
            *failures.lock().unwrap(),
            [("transient cycle failure".to_owned(), 1)]
        );
        assert_eq!(cycles, 2);
    }

    #[test]
    fn every_cycle_failing_stops_bounded_and_surfaces_give_up() {
        let calls = Cell::new(0u32);
        let gave_up = Mutex::new(Vec::new());
        let backoffs = Mutex::new(Vec::new());
        let mut run_cycle = || {
            calls.set(calls.get() + 1);
            Err(boom("always fails"))
        };
        let sleep = |seconds: f64, _should_stop: &dyn Fn() -> bool| {
            backoffs.lock().unwrap().push(seconds);
        };
        let on_give_up = |n: u32| gave_up.lock().unwrap().push(n);
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| false,
            &sleep,
            DaemonHooks {
                on_give_up: Some(&on_give_up),
                max_cycles: Some(10_000),
                ..DaemonHooks::default()
            },
        );
        assert_eq!(calls.get(), MAX_CONSECUTIVE_CYCLE_FAILURES);
        assert_eq!(cycles, MAX_CONSECUTIVE_CYCLE_FAILURES);
        assert_eq!(*gave_up.lock().unwrap(), [MAX_CONSECUTIVE_CYCLE_FAILURES]);
        let expected =
            vec![CYCLE_FAILURE_BACKOFF_SECONDS; (MAX_CONSECUTIVE_CYCLE_FAILURES - 1) as usize];
        assert_eq!(*backoffs.lock().unwrap(), expected);
    }

    #[test]
    fn consecutive_count_resets_across_an_intervening_success() {
        let calls = Cell::new(0u32);
        let gave_up = Mutex::new(Vec::new());
        let stop = Cell::new(false);
        let mut run_cycle = || {
            calls.set(calls.get() + 1);
            if calls.get() % 2 == 1 {
                Err(boom("odd cycle fails"))
            } else {
                Ok(idle("even ok"))
            }
        };
        let sleep = |_seconds: f64, _should_stop: &dyn Fn() -> bool| {
            if calls.get() >= 2 * MAX_CONSECUTIVE_CYCLE_FAILURES + 2 {
                stop.set(true);
            }
        };
        let on_give_up = |n: u32| gave_up.lock().unwrap().push(n);
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| stop.get(),
            &sleep,
            DaemonHooks {
                on_give_up: Some(&on_give_up),
                max_cycles: Some(10_000),
                ..DaemonHooks::default()
            },
        );
        assert!(gave_up.lock().unwrap().is_empty());
        assert!(cycles >= 2 * MAX_CONSECUTIVE_CYCLE_FAILURES);
    }

    #[test]
    fn permanent_fault_stops_after_one_cycle_without_burning_strikes() {
        let calls = Cell::new(0u32);
        let failures = Mutex::new(Vec::new());
        let permanent = Mutex::new(Vec::new());
        let gave_up = Mutex::new(Vec::new());
        let slept = Cell::new(false);
        let mut run_cycle = || {
            calls.set(calls.get() + 1);
            Err(orchestrator_schema_fault())
        };
        let sleep = |_seconds: f64, _should_stop: &dyn Fn() -> bool| slept.set(true);
        let on_cycle_failure = |_err: &CycleError, n: u32| failures.lock().unwrap().push(n);
        let on_permanent_stop = |err: &CycleError| permanent.lock().unwrap().push(err.to_string());
        let on_give_up = |n: u32| gave_up.lock().unwrap().push(n);
        let cycles = run_daemon_loop(
            &mut run_cycle,
            0.0,
            &|| false,
            &sleep,
            DaemonHooks {
                on_cycle_failure: Some(&on_cycle_failure),
                on_permanent_stop: Some(&on_permanent_stop),
                on_give_up: Some(&on_give_up),
                max_cycles: Some(10_000),
                ..DaemonHooks::default()
            },
        );
        assert_eq!(cycles, 1);
        assert_eq!(calls.get(), 1);
        assert!(failures.lock().unwrap().is_empty());
        assert!(gave_up.lock().unwrap().is_empty());
        assert!(!slept.get());
        let permanent = permanent.lock().unwrap();
        assert_eq!(permanent.len(), 1);
        assert!(permanent[0].contains("orchestrator_schema_version=99"));
    }

    #[test]
    fn cycle_fault_classification_recognizes_both_schema_markers() {
        let orchestrator = orchestrator_schema_fault();
        assert_eq!(
            classify_cycle_fault(orchestrator.as_ref()),
            Some(FaultClass::Permanent)
        );
        assert_eq!(
            error_type_name(orchestrator.as_ref()),
            "OrchestratorSchemaError"
        );
        let core: CycleError = Box::new(StoreError::Schema(StoreSchemaError {
            observed_version: Some(7),
            expected_version: 1,
        }));
        assert_eq!(
            classify_cycle_fault(core.as_ref()),
            Some(FaultClass::Permanent)
        );
        assert_eq!(error_type_name(core.as_ref()), "StoreSchemaError");
        let generic = boom("boom");
        assert_eq!(classify_cycle_fault(generic.as_ref()), None);
        assert_eq!(error_type_name(generic.as_ref()), "Exception");
        let io: CycleError = Box::new(std::io::Error::other("disk"));
        assert_eq!(error_type_name(io.as_ref()), "OSError");
    }

    // --- the activity recorder ------------------------------------------------------------------

    fn recorder_at(path: &Path, interval: f64, pid: i64, ticks: &[f64]) -> ActivityRecorder {
        let ticks: Vec<f64> = ticks.to_vec();
        let index = AtomicU32::new(0);
        ActivityRecorder::new(path, interval)
            .with_pid(pid)
            .with_clock(Arc::new(move || {
                let i = index.fetch_add(1, Ordering::SeqCst) as usize;
                ticks[i.min(ticks.len() - 1)]
            }))
    }

    #[test]
    fn recorder_starting_then_running_then_idle() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("activity.json");
        let recorder = recorder_at(&path, 300.0, 999, &[10.0, 20.0, 30.0]);

        recorder.starting();
        let starting = read_activity(Some(&path)).unwrap();
        assert_eq!(starting.phase, PHASE_STARTING);
        assert_eq!(starting.pid, 999);
        assert_eq!(starting.cycle_index, 0);
        assert_eq!(
            starting.expires_at,
            Some(10.0 + 300.0 + LIVENESS_GRACE_SECONDS)
        );

        recorder.before_cycle();
        let running = read_activity(Some(&path)).unwrap();
        assert_eq!(running.phase, PHASE_RUNNING);
        assert_eq!(running.cycle_index, 1);
        assert_eq!(running.last_emitted, 0);
        assert_eq!(running.next_cycle_at, None);

        let result = result_with(
            vec![emitted_task("fix-a", Tier::ProductionDown)],
            vec![DroppedFinding {
                finding: Finding::new("d1", Tier::BrokenBuild, "x"),
                reason: "ungradeable".into(),
            }],
            vec![Tier::ProductionDown],
            "emitted 1, dropped 1",
        );
        recorder.on_cycle(&result);
        let idle = read_activity(Some(&path)).unwrap();
        assert_eq!(idle.phase, PHASE_IDLE);
        assert_eq!(idle.last_emitted, 1);
        assert_eq!(idle.last_dropped, 1);
        assert_eq!(idle.last_reason, "emitted 1, dropped 1");
        assert_eq!(idle.last_relevant_tiers, vec![1]);
        assert_eq!(
            idle.last_emitted_tasks,
            vec![EmittedSummary {
                task_id: "fix-a".into(),
                tier: 1
            }]
        );
        assert_eq!(idle.next_cycle_at, Some(330.0));
    }

    #[test]
    fn running_snapshot_carries_previous_cycle_summary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("activity.json");
        let recorder = recorder_at(&path, 60.0, 1, &[0.0]);
        recorder.before_cycle();
        recorder.on_cycle(&result_with(
            vec![emitted_task("fix-a", Tier::ProductionDown)],
            vec![],
            vec![],
            "cycle 1",
        ));
        recorder.before_cycle();
        let running = read_activity(Some(&path)).unwrap();
        assert_eq!(running.phase, PHASE_RUNNING);
        assert_eq!(running.cycle_index, 2);
        assert_eq!(running.last_emitted, 1);
        assert_eq!(running.last_reason, "cycle 1");
    }

    #[test]
    fn recorder_drives_through_daemon_loop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("activity.json");
        let recorder = recorder_at(&path, 42.0, 7, &[100.0]);
        let results = Mutex::new(VecDeque::from([
            idle("idle cycle"),
            result_with(
                vec![emitted_task("fix-z", Tier::ImminentSevereRisk)],
                vec![],
                vec![],
                "emitted 1",
            ),
        ]));
        let ran = Cell::new(0u32);
        let mut run_cycle = || {
            ran.set(ran.get() + 1);
            Ok(results.lock().unwrap().pop_front().unwrap())
        };
        let before_cycle = || recorder.before_cycle();
        let on_cycle = |result: &AutopilotPassResult| recorder.on_cycle(result);
        let cycles = run_daemon_loop(
            &mut run_cycle,
            42.0,
            &|| ran.get() >= 2,
            &noop_sleep,
            DaemonHooks {
                before_cycle: Some(&before_cycle),
                on_cycle: Some(&on_cycle),
                max_cycles: Some(2),
                ..DaemonHooks::default()
            },
        );
        assert_eq!(cycles, 2);
        let last = read_activity(Some(&path)).unwrap();
        assert_eq!(last.phase, PHASE_IDLE);
        assert_eq!(last.cycle_index, 2);
        assert_eq!(last.last_emitted, 1);
        assert_eq!(last.last_reason, "emitted 1");
        assert_eq!(last.next_cycle_at, Some(142.0));
    }

    #[test]
    fn liveness_record_publishes_then_lapses() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("activity.json");
        let recorder = recorder_at(&path, 300.0, 4242, &[1000.0]);
        recorder.starting();
        let published = read_activity(Some(&path)).unwrap();
        let deadline = published.expires_at.unwrap();
        assert_eq!(deadline, 1000.0 + 300.0 + LIVENESS_GRACE_SECONDS);
        let adopted = read_live_activity(Some(&path), Some(1200.0)).unwrap();
        assert_eq!(adopted, published);
        assert_eq!(adopted.pid, 4242);
        assert!(read_live_activity(Some(&path), Some(deadline - 0.001)).is_some());
        assert!(read_live_activity(Some(&path), Some(deadline)).is_none());
        assert!(read_live_activity(Some(&path), Some(5000.0)).is_none());
        assert!(!is_record_live(Some(&published), deadline + 1.0));
        assert!(read_live_activity(Some(&dir.path().join("never.json")), Some(1.0)).is_none());
    }

    // --- the entrypoint ------------------------------------------------------------------------

    /// A hermetic host: an explicit policy file, captured stderr, a repo root,
    /// scripted pass outcomes (an idle result once the script is exhausted),
    /// optional in-memory stores, and the armed stop flag so the script can
    /// stand in for a signal arriving mid-cycle.
    struct FakeHost {
        policy: PathBuf,
        stderr: Arc<Mutex<Vec<String>>>,
        root: PathBuf,
        specs: Mutex<Vec<SinglePassSpec>>,
        probed: Mutex<Vec<Option<usize>>>,
        outcomes: Mutex<VecDeque<Result<AutopilotPassResult, CycleError>>>,
        depth_store: Option<Arc<InMemoryStore>>,
        claims: Option<Arc<InMemoryClaimStore>>,
        armed: Mutex<Option<StopFlag>>,
        stop_after_cycles: Option<u32>,
        cycles: AtomicU32,
    }

    impl FakeHost {
        fn new(policy: PathBuf, root: PathBuf) -> Self {
            Self {
                policy,
                stderr: Arc::new(Mutex::new(Vec::new())),
                root,
                specs: Mutex::new(Vec::new()),
                probed: Mutex::new(Vec::new()),
                outcomes: Mutex::new(VecDeque::new()),
                depth_store: None,
                claims: None,
                armed: Mutex::new(None),
                stop_after_cycles: None,
                cycles: AtomicU32::new(0),
            }
        }

        fn script(self, outcomes: Vec<Result<AutopilotPassResult, CycleError>>) -> Self {
            *self.outcomes.lock().unwrap() = outcomes.into();
            self
        }

        fn stop_after(mut self, cycles: u32) -> Self {
            self.stop_after_cycles = Some(cycles);
            self
        }

        fn with_claims(mut self, claims: Arc<InMemoryClaimStore>) -> Self {
            self.claims = Some(claims);
            self
        }

        fn with_depth_store(mut self, store: Arc<InMemoryStore>) -> Self {
            self.depth_store = Some(store);
            self
        }

        fn lines(&self) -> Vec<String> {
            self.stderr.lock().unwrap().clone()
        }

        fn err_text(&self) -> String {
            self.lines().join("\n")
        }
    }

    impl AutopilotHost for FakeHost {
        fn policy_path(&self) -> Option<PathBuf> {
            Some(self.policy.clone())
        }

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

        fn queue_depth_store(
            &self,
            _policy: Option<&WorkPolicy>,
            _db_path: &Path,
            _log: &LogSink,
        ) -> Option<Arc<dyn HarnessStore>> {
            self.depth_store.clone().map(|s| s as Arc<dyn HarnessStore>)
        }

        fn backoff_claims(
            &self,
            _policy: Option<&WorkPolicy>,
            _db_path: &Path,
            _log: &LogSink,
        ) -> Option<Arc<dyn ClaimStore>> {
            self.claims.clone().map(|c| c as Arc<dyn ClaimStore>)
        }

        fn run_cycle(
            &self,
            spec: &SinglePassSpec,
            queue_depth: Option<&QueueDepthFn<'_>>,
        ) -> Result<AutopilotPassResult, CycleError> {
            self.specs.lock().unwrap().push(spec.clone());
            self.probed
                .lock()
                .unwrap()
                .push(queue_depth.map(|probe| probe(&spec.tasks_dir).unwrap()));
            let count = self.cycles.fetch_add(1, Ordering::SeqCst) + 1;
            if self.stop_after_cycles.is_some_and(|n| count >= n)
                && let Some(stop) = self.armed.lock().unwrap().as_ref()
            {
                stop.set();
            }
            match self.outcomes.lock().unwrap().pop_front() {
                Some(outcome) => outcome,
                None => {
                    // An exhausted script with no scheduled stop ends the run
                    // the way a signal would, so no test can loop forever.
                    if self.stop_after_cycles.is_none()
                        && let Some(stop) = self.armed.lock().unwrap().as_ref()
                    {
                        stop.set();
                    }
                    Ok(idle("idle"))
                }
            }
        }

        fn arm_stop_signals(&self, stop: &StopFlag, _log: &LogSink) {
            *self.armed.lock().unwrap() = Some(stop.clone());
        }

        fn sleep(&self, _stop: &StopFlag, _seconds: f64) {}

        fn epoch_clock(&self) -> EpochClock {
            Arc::new(|| 1000.0)
        }
    }

    fn host(dir: &TempDir, policy_body: &str) -> FakeHost {
        let policy = write_policy(dir.path(), policy_body);
        FakeHost::new(policy, dir.path().to_path_buf())
    }

    #[test]
    fn bad_argument_prints_usage_and_exits_2() {
        let dir = TempDir::new().unwrap();
        let host = host(&dir, "");
        assert_eq!(run_main(&argv(&["--bogus"]), &host), 2);
        assert_eq!(
            host.lines(),
            vec![
                USAGE.to_owned(),
                "flywheel autopilot: error: unrecognized arguments: --bogus".to_owned()
            ]
        );
        assert!(host.specs.lock().unwrap().is_empty());
    }

    #[test]
    fn help_exits_0_without_running() {
        let dir = TempDir::new().unwrap();
        let host = host(&dir, "");
        assert_eq!(run_main(&argv(&["--help"]), &host), 0);
        assert!(host.specs.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_repo_root_exits_1_with_the_message() {
        let dir = TempDir::new().unwrap();
        let policy = write_policy(dir.path(), "");
        let host = FakeHost::new(policy, PathBuf::new());
        assert_eq!(run_main(&argv(&["--once"]), &host), 1);
        assert_eq!(host.lines(), vec!["ERROR: not inside a git repository."]);
    }

    #[test]
    fn policy_error_exits_2_before_any_pass() {
        let dir = TempDir::new().unwrap();
        let host = host(&dir, "[autopilot]\ntarget_depth = \"many\"\n");
        assert_eq!(run_main(&argv(&["--once"]), &host), 2);
        let text = host.err_text();
        assert!(
            text.starts_with("flywheel autopilot: policy error: "),
            "{text}"
        );
        assert!(host.specs.lock().unwrap().is_empty());
    }

    #[test]
    fn once_runs_one_pass_logs_the_result_and_exits_0() {
        let dir = TempDir::new().unwrap();
        let result = result_with(
            vec![emitted_task("fix-a", Tier::CoreFeatureWork)],
            vec![DroppedFinding {
                finding: Finding::new("d1", Tier::TechDebt, "x"),
                reason: "ungradeable".into(),
            }],
            vec![Tier::CoreFeatureWork],
            "emitted 1, dropped 1",
        );
        let host = host(&dir, "[autopilot]\ntarget_depth = 3\nlanding = \"pr\"\n")
            .script(vec![Ok(result)]);
        assert_eq!(run_main(&argv(&["--once", "--model", "m"]), &host), 0);
        let root = python_path_str(dir.path());
        assert_eq!(
            host.lines(),
            vec![
                format!(
                    "[autopilot] single pass repo={root} tasks=.flywheel/tasks target_depth=3 landing=pr"
                ),
                "[autopilot] emitted 1, dropped 1".to_owned(),
                "[autopilot] emitted fix-a.json".to_owned(),
                "[autopilot] dropped d1: ungradeable".to_owned(),
            ]
        );
        let specs = host.specs.lock().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].model.as_deref(), Some("m"));
        assert_eq!(specs[0].landing, "pr");
        assert_eq!(specs[0].target_depth, 3);
        assert_eq!(specs[0].repo_root, dir.path());
        // Without a store the pass falls back to the raw active-file count.
        assert_eq!(*host.probed.lock().unwrap(), vec![None]);
    }

    #[test]
    fn once_failing_pass_exits_1() {
        let dir = TempDir::new().unwrap();
        let host = host(&dir, "").script(vec![Err(boom("agent unreachable"))]);
        assert_eq!(run_main(&argv(&["--once"]), &host), 1);
        assert_eq!(
            host.lines().last().map(String::as_str),
            Some("flywheel autopilot: agent unreachable")
        );
    }

    #[test]
    fn once_threads_the_policy_deadline_override_to_the_pass() {
        let dir = TempDir::new().unwrap();
        let host = host(&dir, "[deadlines]\nautopilot_agent_seconds = 0.05\n");
        assert_eq!(run_main(&argv(&["--once"]), &host), 0);
        let specs = host.specs.lock().unwrap();
        let deadlines = specs[0].deadlines.clone().unwrap();
        assert_eq!(
            deadlines.for_class(DeadlineClass::AutopilotAgent),
            Some(0.05)
        );
        assert_ne!(
            deadlines.for_class(DeadlineClass::AutopilotAgent),
            Some(DEFAULT_AUTOPILOT_AGENT_SECONDS)
        );
        // The zero opt-out reaches the seam as the unbounded ceiling.
        let host = host_with(&dir, "[deadlines]\nautopilot_agent_seconds = 0\n");
        assert_eq!(run_main(&argv(&["--once"]), &host), 0);
        let specs = host.specs.lock().unwrap();
        let deadlines = specs[0].deadlines.clone().unwrap();
        assert_eq!(deadlines.for_class(DeadlineClass::AutopilotAgent), None);
    }

    fn host_with(dir: &TempDir, policy_body: &str) -> FakeHost {
        host(dir, policy_body)
    }

    #[test]
    fn no_policy_spec_forwards_none_deadlines_and_defaults() {
        let args = AutopilotArgs::default();
        let runtime = resolve_runtime(&args, None);
        let spec = SinglePassSpec {
            tasks_dir: runtime.tasks_dir,
            target_depth: runtime.target_depth,
            landing: runtime.landing,
            weights: runtime.weights,
            ..SinglePassSpec::new("/repo", "/repo/.flywheel/tasks")
        };
        assert_eq!(spec.deadlines, None);
        assert_eq!(
            DeadlineConfig::default().for_class(DeadlineClass::AutopilotAgent),
            Some(DEFAULT_AUTOPILOT_AGENT_SECONDS)
        );
        assert_eq!(spec.target_depth, DEFAULT_TARGET_DEPTH);
        assert_eq!(spec.landing, DEFAULT_LANDING);
    }

    #[test]
    fn daemon_uses_the_store_backed_actionable_depth_probe() {
        let dir = TempDir::new().unwrap();
        let host = host(&dir, "").with_depth_store(Arc::new(InMemoryStore::new()));
        assert_eq!(run_main(&argv(&["--once"]), &host), 0);
        assert_eq!(*host.probed.lock().unwrap(), vec![Some(0)]);
    }

    #[test]
    fn daemon_returns_non_zero_when_it_gives_up() {
        let dir = TempDir::new().unwrap();
        let outcomes = (0..MAX_CONSECUTIVE_CYCLE_FAILURES)
            .map(|_| Err(boom("cycle blew up")))
            .collect();
        let activity = dir.path().join("act.json");
        let host = host(&dir, "").script(outcomes);
        let code = run_main(
            &argv(&[
                "--interval",
                "0",
                "--activity-file",
                activity.to_str().unwrap(),
            ]),
            &host,
        );
        assert_eq!(code, 1);
        let root = python_path_str(dir.path());
        let lines = host.lines();
        assert_eq!(
            lines[0],
            format!(
                "[autopilot] daemon started repo={root} tasks=.flywheel/tasks target_depth=5 \
                 landing=merge interval=0.0s pid={}",
                std::process::id()
            )
        );
        assert_eq!(
            lines[1],
            "[autopilot] cycle failed (Exception: cycle blew up) [1/5]"
        );
        assert_eq!(
            lines[5],
            "[autopilot] cycle failed (Exception: cycle blew up) [5/5]"
        );
        assert_eq!(
            lines[6],
            "[autopilot] too many consecutive cycle failures (5); exiting non-zero for \
             operator inspection."
        );
        assert_eq!(lines[7], "[autopilot] daemon stopped after 5 cycle(s)");
        // The activity record reports the last cycle as running with the pid.
        let activity = read_activity(Some(&activity)).unwrap();
        assert_eq!(activity.phase, PHASE_RUNNING);
        assert_eq!(activity.cycle_index, 5);
        assert_eq!(activity.pid, i64::from(std::process::id()));
    }

    #[test]
    fn daemon_permanent_fault_exits_non_zero_after_one_cycle() {
        let dir = TempDir::new().unwrap();
        let host = host(&dir, "").script(vec![Err(orchestrator_schema_fault())]);
        let activity = dir.path().join("act.json");
        let code = run_main(
            &argv(&["--activity-file", activity.to_str().unwrap()]),
            &host,
        );
        assert_eq!(code, 1);
        let text = host.err_text();
        assert!(
            text.contains("[autopilot] permanent fault (OrchestratorSchemaError: stop ledger: "),
            "{text}"
        );
        assert!(
            text.contains("cannot succeed on retry; exiting non-zero"),
            "{text}"
        );
        assert!(
            text.ends_with("[autopilot] daemon stopped after 1 cycle(s)"),
            "{text}"
        );
        assert!(!text.contains("cycle failed ("));
    }

    #[test]
    fn daemon_stops_on_the_armed_signal_after_the_current_cycle() {
        let dir = TempDir::new().unwrap();
        let activity = dir.path().join("logs").join("act.json");
        let host = host(&dir, "").stop_after(2);
        let code = run_main(
            &argv(&[
                "--interval",
                "42",
                "--activity-file",
                activity.to_str().unwrap(),
            ]),
            &host,
        );
        assert_eq!(code, 0);
        let lines = host.lines();
        assert!(lines[0].contains("interval=42.0s"), "{}", lines[0]);
        assert_eq!(
            lines.last().unwrap(),
            "[autopilot] daemon stopped after 2 cycle(s)"
        );
        let activity = read_activity(Some(&activity)).unwrap();
        assert_eq!(activity.phase, PHASE_IDLE);
        assert_eq!(activity.cycle_index, 2);
        assert_eq!(activity.interval_seconds, 42.0);
        assert_eq!(activity.next_cycle_at, Some(1042.0));
        assert_eq!(
            activity.expires_at,
            Some(1000.0 + 42.0 + LIVENESS_GRACE_SECONDS)
        );
    }

    #[test]
    fn daemon_backs_off_a_never_progressing_repo_and_stops() {
        let dir = TempDir::new().unwrap();
        let claims = Arc::new(InMemoryClaimStore::new());
        let host = host(&dir, "").with_claims(claims.clone()).script(vec![
            Ok(idle("idle 1")),
            Ok(idle("idle 2")),
            Ok(idle("idle 3")),
        ]);
        let code = run_main(
            &argv(&[
                "--activity-file",
                dir.path().join("act.json").to_str().unwrap(),
            ]),
            &host,
        );
        assert_eq!(code, 0);
        assert_eq!(host.cycles.load(Ordering::SeqCst), 3);
        let text = host.err_text();
        assert!(
            text.contains(
                "[autopilot] no progress for 3 consecutive cycle(s); backed off and routed to \
                 the human-review queue (reason 'no-progress'); stopping."
            ),
            "{text}"
        );
        assert!(
            text.ends_with("[autopilot] daemon stopped after 3 cycle(s)"),
            "{text}"
        );
        let queue = claims.list_human_review_queue().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].reason, REASON_NO_PROGRESS);
        assert_eq!(queue[0].task_id, ".flywheel/tasks");
    }

    #[test]
    fn daemon_default_activity_path_lands_under_the_repo_logs() {
        let dir = TempDir::new().unwrap();
        let host = host(&dir, "").stop_after(1);
        assert_eq!(run_main(&[], &host), 0);
        let expected = dir
            .path()
            .join(".flywheel")
            .join("logs")
            .join("autopilot")
            .join("activity.json");
        assert!(read_activity(Some(&expected)).is_some());
    }
}
