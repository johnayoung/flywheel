//! The reference worker daemon: the `flywheel worker` entry point.
//!
//! Port of the daemon half of the Python `flywheel_worktree.worker` module
//! (the submit strategies live in [`crate::submitter`] / [`crate::phase`] /
//! [`crate::pr`] / [`crate::tiering`]; the multi-worker process pool in
//! [`crate::pool`]). It owns the concerns flywheel deliberately does not:
//!
//! * **the cycle** -- [`run_once`] records phase base refs, drains every
//!   eligible task to quiescence through the git-submit seam (one
//!   `orchestrate` session with the policy-selected landing strategy), and
//!   archives completed phases under the merge lock;
//! * **the loop** -- [`run_daemon_loop`] re-invokes the cycle on a poll
//!   cadence with a cross-cycle circuit breaker (transient strikes vs. a
//!   permanent stop), stopping on SIGTERM/SIGINT after the current cycle;
//! * **the wiring** -- the [`RunTask`] binding onto the core workflow
//!   ([`CoreRunTask`]), the agents-backed conflict resolver the submitter's
//!   recovery rung escalates to ([`make_agents_resolve_conflict`]), the
//!   operator heartbeat ([`Heartbeat`]), the container-backend wrap, and the
//!   `flywheel worker` flag surface ([`parse_args`] / [`main`]).
//!
//! The worker never commits to the operator's branch: landing is the
//! `SubmitStrategy`'s job. Authoritative specs: `docs/cli.md` (the `worker`
//! verb), `docs/strategy.md`, `docs/sandbox.md`, `docs/orchestration.md`,
//! `docs/configuration.md` (`[worker]`, `[execution]`), `docs/container-backend.md`.

use crate::phase::{ArchiveError, ArchiveSweep, archive_phases, record_phase_bases};
use crate::pr::{PhaseEvidenceStore, PhasePrPublisher, register_pr_strategy};
use crate::retention::{
    DEFAULT_RETENTION_DAYS, DEFAULT_RUN_LOG_RETENTION, retention_cadence_tick, retention_targets,
    sweep_run_logs, wall_clock_seconds,
};
use crate::submit_registry::{
    MERGE_STRATEGY, SubmitBuildRequest, SubmitStrategyRegistry, builtin_registry,
};
use crate::submitter::{
    ConflictResolutionError, ConflictResolutionReport, ConflictResolutionRequest, ConflictResolver,
    GitError, GitWorktreeSubmitter, LandingLedger, Logger, checked_out_branch, git,
    git_progress_probe, merge_lock,
};
use crate::tiering::{TIER_STRATEGY, register_tier_strategy};
use chrono::{DateTime, Utc};
use flywheel_agents::hosts::ExecutionHost;
use flywheel_agents::{
    AgentConfiguration, AgentRuntime, PermissionPolicy, RunOptions as AgentRunOptions, RunRequest,
};
use flywheel_container::{
    ConfigError, ContainerInvokeSpec, HostInvokeBuilder, maybe_wrap_for_backend,
    reap_container_orphans,
};
use flywheel_core::agents_invoke::{AgentsInvoker, InvokeFn};
use flywheel_core::audit::StopFlag;
use flywheel_core::faults::{FaultClass, classify_error};
use flywheel_core::harness::{DEFAULT_AGENT_ID, HarnessError, ShutdownSignal};
use flywheel_core::invoker_client::ProgressProbe as CoreProgressProbe;
use flywheel_core::loaders::py_str_repr;
use flywheel_core::workflow::{
    AgentOptions, EventsMode, LineSink, RunStore, RunTaskOptions, Skills as CoreSkills,
    WorkflowError, run_task_object,
};
use flywheel_orchestrator::claims::ClaimStoreError;
use flywheel_orchestrator::github::GhRunner;
use flywheel_orchestrator::held_out_gate::{FilesystemHeldOutGraderSource, HeldOutGraderSource};
use flywheel_orchestrator::orchestrate::{
    DEFAULT_LANDING_REDRIVE_BOUND, DEFAULT_PREREQ_REDRIVE_BOUND,
    DEFAULT_SESSION_PAUSE_CEILING_SECONDS, DEFAULT_SWEEP_SECONDS, DEFAULT_WORKER_CONCURRENCY,
    GraphSnapshotPolicy, OrchestrateError, OrchestrateRequest, OrchestratorReport,
    ProgressProbeFactory, RunTask, RunTaskError, RunTaskFuture, RunTaskRequest, orchestrate,
};
use flywheel_orchestrator::policy::{
    PolicyError, Skills as PolicySkills, WorkPolicy, resolve_grader_env, resolve_sandbox_root,
};
use flywheel_orchestrator::sources::DirectoryWorkSource;
use flywheel_orchestrator::store_factory::{StoreConfigError, build_store};
use flywheel_orchestrator::store_registry::environ_from_process;
use flywheel_orchestrator::strategy::SubmitStrategy;
use flywheel_orchestrator::workflow::args::{
    DEFAULT_LEASE_SECONDS, DEFAULT_MAX_RETRIES, DEFAULT_MAX_TURNS, DEFAULT_RECONCILE_SECONDS,
};
use flywheel_orchestrator::workflow::{
    LiveRunRow, collect_live_rows, default_store_registries, load_effective_policy,
    open_stores_for, store_config_for,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

// --- Constants ----------------------------------------------------------------------------

/// Per-in-flight-task heartbeat cadence (seconds); `0` disables.
pub const DEFAULT_HEARTBEAT_SECONDS: i64 = 10;
/// Idle wait between drain cycles (seconds).
pub const DEFAULT_POLL_INTERVAL_SECONDS: i64 = 5;
/// Consecutive whole-cycle failures (a cycle failing unexpectedly) before
/// the daemon gives up so an operator can inspect, rather than hot-looping.
/// The per-task starvation guard lives in orchestrate (`attempted_fresh`);
/// this is the cross-cycle backstop.
pub const MAX_CONSECUTIVE_CYCLE_FAILURES: u32 = 5;
/// Backoff between failed cycles (seconds).
pub const CYCLE_FAILURE_BACKOFF_SECONDS: f64 = 10.0;
/// Distinct exit code for a permanent stop: a cycle fault that can never
/// succeed on retry (a schema-version mismatch reopening the store every
/// cycle). Separate from the transient give-up exit (1) so an operator can
/// tell "wrong on-disk schema" from "five flaky cycles".
pub const PERMANENT_STOP_EXIT_CODE: i32 = 2;
/// Hard ceiling on the rendered heartbeat action so a runaway tool-call
/// payload can never wrap the single-line heartbeat unboundedly.
pub const HEARTBEAT_DETAIL_MAX_WIDTH: usize = 100;
/// The setting named by every concurrency diagnostic.
pub const CONCURRENCY_SETTING: &str = "--concurrency / [worker] concurrency";

// --- Logging ------------------------------------------------------------------------------

/// The one centralized log line format: `<UTC timestamp> <prefix> <message>`.
pub fn format_log_line(prefix: &str, message: &str, now: DateTime<Utc>) -> String {
    format!("{} {prefix} {message}", now.format("%Y-%m-%dT%H:%M:%SZ"))
}

/// Build the stderr logger every worker/pool/heartbeat line goes through.
///
/// The timestamp is stamped here -- the single construction seam -- so pool
/// member logs, supervisor lines, and heartbeats are all correlatable
/// against each other and against store timestamps.
pub fn make_logger(prefix: &str) -> Logger {
    let prefix = prefix.to_owned();
    Arc::new(move |message: &str| {
        let line = format_log_line(&prefix, message, Utc::now());
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        let _ = writeln!(handle, "{line}");
        let _ = handle.flush();
    })
}

// --- Heartbeat ----------------------------------------------------------------------------

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let keep = limit.saturating_sub(1).max(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('\u{2026}');
    out
}

/// Single-line per-in-flight-run progress: lifecycle-position breadcrumb
/// (`attempt=N iter=K`), running tokens/cost/turns rolled up from the run's
/// `harness.iteration_completed` events, age of the latest activity, and
/// the current agent action. Byte-identical to the Python renderer.
pub fn format_heartbeat(row: &LiveRunRow, now: DateTime<Utc>) -> String {
    let age = match row.last_ts {
        None => "\u{2014}".to_owned(),
        Some(ts) => format!("{}s", (now - ts).num_seconds().max(0)),
    };
    let attempt_str = row
        .attempt
        .map_or_else(|| "attempt=?".to_owned(), |a| format!("attempt={a}"));
    let iter_str = row
        .iteration
        .map_or_else(|| "iter=?".to_owned(), |i| format!("iter={i}"));
    // No iteration has completed yet, so the per-iteration rollup has no
    // data: render that honestly and say WHEN numbers will appear -- a
    // single long iteration keeps this placeholder for its whole duration,
    // which reads as a stall unless the line says so.
    let totals = if row.iterations_completed == 0 {
        "working (iteration 1 in progress; totals land at iteration boundaries)".to_owned()
    } else {
        format!(
            "tokens={} cost=${:.4} turns={}",
            row.tokens_total, row.cost_usd_total, row.turns_total
        )
    };
    let detail = truncate(&row.last_detail, HEARTBEAT_DETAIL_MAX_WIDTH);
    format!(
        "{} {} {attempt_str} {iter_str} {totals} age={age} {} {detail}",
        row.task_id,
        row.status.as_str(),
        row.last_kind
    )
}

/// One heartbeat tick: open the policy-selected core store on `db_path`,
/// snapshot the in-flight runs, and render one line per run.
pub fn heartbeat_lines(
    db_path: &Path,
    policy: Option<&WorkPolicy>,
    now: DateTime<Utc>,
) -> Result<Vec<String>, StoreConfigError> {
    let registries = default_store_registries();
    let environ = environ_from_process();
    let store = build_store(&registries, &store_config_for(policy, db_path), &environ)?;
    let rows = collect_live_rows(store.as_ref())?;
    Ok(rows.iter().map(|row| format_heartbeat(row, now)).collect())
}

/// Background thread printing per-in-flight-run progress lines so a
/// watcher can tell the agent is still moving. Quiet when nothing is in
/// flight; a transient store read failure is retried on the next tick.
pub struct Heartbeat {
    db_path: PathBuf,
    interval: i64,
    log: Logger,
    policy: Option<Arc<WorkPolicy>>,
    stop: StopFlag,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for Heartbeat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Heartbeat")
            .field("db_path", &self.db_path)
            .field("interval", &self.interval)
            .field("running", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

impl Heartbeat {
    pub fn new(
        db_path: impl Into<PathBuf>,
        interval_seconds: i64,
        log: Logger,
        policy: Option<Arc<WorkPolicy>>,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            interval: interval_seconds,
            log,
            policy,
            stop: StopFlag::new(),
            thread: None,
        }
    }

    /// Start the thread (a non-positive interval starts nothing).
    pub fn start(mut self) -> Self {
        if self.interval > 0 && self.thread.is_none() {
            let db_path = self.db_path.clone();
            let interval = Duration::from_secs(self.interval as u64);
            let log = Arc::clone(&self.log);
            let policy = self.policy.clone();
            let stop = self.stop.clone();
            let thread = std::thread::Builder::new()
                .name("worker-heartbeat".to_owned())
                .spawn(move || {
                    while !stop.wait(interval) {
                        // Rebuilt every tick through the store factory; a
                        // failed read is retried next tick.
                        let Ok(lines) = heartbeat_lines(&db_path, policy.as_deref(), Utc::now())
                        else {
                            continue;
                        };
                        for line in lines {
                            log(&line);
                        }
                    }
                });
            self.thread = thread.ok();
        }
        self
    }

    /// Stop the thread and wait for it to exit; idempotent.
    pub fn stop(&mut self) {
        self.stop.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- The agents-backed conflict resolver ------------------------------------------------

/// Drive `future` to completion from synchronous code, inside or outside a
/// tokio runtime.
///
/// The submit chain is synchronous by contract but executes ON orchestrate's
/// runtime thread, where `block_on` would panic ("cannot start a runtime from
/// within a runtime"). A dedicated thread with its own current-thread runtime
/// behaves identically in both contexts; the caller blocks for the result
/// exactly as the rest of the landing ladder blocks on graders and git.
pub fn run_session_blocking<T, F>(future: F) -> Result<T, ConflictResolutionError>
where
    T: Send,
    F: Future<Output = T> + Send,
{
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("flywheel-resolver".to_owned())
            .spawn_scoped(scope, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| {
                        ConflictResolutionError(format!(
                            "cannot start the conflict-resolution session runtime: {err}"
                        ))
                    })?;
                Ok(runtime.block_on(future))
            })
            .map_err(|err| {
                ConflictResolutionError(format!(
                    "cannot spawn the conflict-resolution session thread: {err}"
                ))
            })?;
        handle.join().map_err(|_| {
            ConflictResolutionError("the conflict-resolution session thread panicked".to_owned())
        })?
    })
}

/// Build the agents-runtime conflict resolver (docs/agent-harness.md
/// section 15.2): a synchronous callable that drives one bounded session
/// rooted in the conflicted worktree -- a turn budget via the
/// configuration's `max_turns` and a wall-clock ceiling via the runtime's
/// `timeout_seconds` -- and reports usage only.
///
/// A wall-clock timeout folds into a run failure with
/// `error_type == "timeout"` and is reported AT the bounds (the true turn
/// count is unavailable after cancellation), so a slow session parks within
/// its bounds rather than failing. A session that cannot start is an error
/// the submitter contains into a `merge-conflict` park. One runtime per
/// factory: the submitter builds the resolver once and every escalation
/// reuses it. `command_override` substitutes the agent executable (a
/// scripted stream-json binary in tests).
pub fn make_agents_resolve_conflict(
    agent_id: &str,
    transport: Option<&str>,
    command_override: Option<Vec<String>>,
) -> ConflictResolver {
    let runtime = Arc::new(AgentRuntime::new());
    let agent_id = agent_id.to_owned();
    let transport = transport.map(str::to_owned);
    Arc::new(move |request: &ConflictResolutionRequest| {
        let mut configuration = AgentConfiguration::new(&agent_id);
        configuration.model_id = request.model.clone();
        configuration.max_turns = Some(request.max_turns);
        configuration.permission_policy = PermissionPolicy::Auto;
        configuration.command_override = command_override.clone();
        if let Some(transport) = &transport {
            configuration
                .adapter_options
                .insert("transport".to_owned(), json!(transport));
        }
        let run_request = RunRequest {
            prompt: request.prompt.clone(),
            working_directory: request.worktree.clone(),
            configuration,
            timeout_seconds: Some(request.max_wall_seconds),
        };
        let started = Instant::now();
        let runtime = Arc::clone(&runtime);
        let completed = run_session_blocking(async move {
            runtime.run(run_request, AgentRunOptions::default()).await
        })?
        .map_err(|err| ConflictResolutionError(err.to_string()))?;
        if completed
            .failure
            .as_ref()
            .is_some_and(|failure| failure.error_type == "timeout")
        {
            return Ok(ConflictResolutionReport {
                turns: request.max_turns,
                wall_seconds: request.max_wall_seconds,
            });
        }
        let turns = completed
            .num_turns
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        let wall_seconds = started
            .elapsed()
            .as_secs_f64()
            .min(request.max_wall_seconds);
        Ok(ConflictResolutionReport {
            turns,
            wall_seconds,
        })
    })
}

// --- Git helpers -------------------------------------------------------------------------

/// The worker was started outside a git repository.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ERROR: not inside a git repository.")]
pub struct NotARepository;

/// The toplevel of the repository the working directory is inside.
pub fn repo_root() -> Result<PathBuf, NotARepository> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
        }
        _ => Err(NotARepository),
    }
}

/// Best-effort fresh fetch of the configured base ref from its remote so a
/// landing targets the up-to-date base, not a stale local ref.
///
/// The remote is `branch.<base>.remote` when configured, else `origin`;
/// when neither exists the fetch is a no-op (a purely local repo). The
/// fetch FF-updates the local base ref (`<base>:<base>`); a base that then
/// cannot fast-forward becomes the divergent-base park, never a failure
/// here. The caller guarantees the base is not the operator's checked-out
/// branch, so the refspec fetch into `refs/heads/<base>` is not refused.
pub fn fetch_base_fresh(
    repo_root: &Path,
    base: &str,
    log: Option<&Logger>,
) -> Result<(), GitError> {
    let remotes: Vec<String> = git(repo_root, &["remote"])?
        .stdout
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if remotes.is_empty() {
        return Ok(());
    }
    let key = format!("branch.{base}.remote");
    let configured = git(repo_root, &["config", &key])?.trimmed().to_owned();
    let remote = if !configured.is_empty() {
        configured
    } else if remotes.iter().any(|r| r == "origin") {
        "origin".to_owned()
    } else {
        remotes[0].clone()
    };
    if !remotes.contains(&remote) {
        return Ok(());
    }
    let refspec = format!("{base}:{base}");
    let res = git(repo_root, &["fetch", &remote, &refspec])?;
    if !res.ok()
        && let Some(log) = log
    {
        log(&format!(
            "fresh fetch of base {} from {} failed ({}); landing will use the local ref",
            py_str_repr(base),
            py_str_repr(&remote),
            res.stderr.trim()
        ));
    }
    Ok(())
}

/// Resolve the merge-strategy landing base, fetching it fresh.
///
/// The base is `[submit] base` when configured; otherwise the operator's
/// currently-checked-out branch. A configured base equal to the checked-out
/// branch is refused (never land into the working checkout), as is a
/// detached `HEAD` with no base configured.
pub fn resolve_landing_base(
    repo_root: &Path,
    policy: Option<&WorkPolicy>,
    log: Option<&Logger>,
) -> Result<String, PolicyError> {
    let checked_out =
        checked_out_branch(repo_root).map_err(|err| PolicyError::Invalid(err.to_string()))?;
    if let Some(configured) = policy.and_then(|p| p.submit_base.clone()) {
        if checked_out.as_deref() == Some(configured.as_str()) {
            return Err(PolicyError::Invalid(format!(
                "configured landing base {} is the operator's currently-checked-out branch; \
                 refusing to land into the working checkout (check out a different branch or \
                 change [submit] base)",
                py_str_repr(&configured)
            )));
        }
        fetch_base_fresh(repo_root, &configured, log)
            .map_err(|err| PolicyError::Invalid(err.to_string()))?;
        return Ok(configured);
    }
    checked_out.ok_or_else(|| {
        PolicyError::Invalid(
            "worker started on detached HEAD with no [submit] base configured; set [submit] base \
             to a landing branch or check out a branch."
                .to_owned(),
        )
    })
}

// --- Flag / policy resolution ------------------------------------------------------------

/// The agent model id: an explicit `--model` wins, else `[agent] model`,
/// else `None` (the agent's own default).
pub fn resolve_model(flag: Option<&str>, policy: Option<&WorkPolicy>) -> Option<String> {
    if let Some(model) = flag.filter(|m| !m.is_empty()) {
        return Some(model.to_owned());
    }
    policy.and_then(|p| p.model.clone())
}

/// Python `int()` over a flag value: surrounding whitespace, an optional
/// sign, and single underscores between digits are accepted.
fn parse_py_int(raw: &str) -> Option<i64> {
    let text = raw.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    let value: i64 = cleaned.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// The worker pool size: an explicit `--concurrency` wins, else `[worker]
/// concurrency` (default 1). A resolved value below 1 -- or a flag that is
/// not an integer -- is a hard error naming the setting, raised BEFORE any
/// store opens or task is claimed; a silent clamp would hide the mistake.
pub fn resolve_concurrency(
    flag: Option<&str>,
    policy: Option<&WorkPolicy>,
) -> Result<u32, PolicyError> {
    let value = match flag {
        Some(raw) => parse_py_int(raw).ok_or_else(|| {
            PolicyError::Invalid(format!(
                "{CONCURRENCY_SETTING} must be an integer >= 1, got {}",
                py_str_repr(raw)
            ))
        })?,
        None => policy.map_or(DEFAULT_WORKER_CONCURRENCY, |p| p.worker_concurrency),
    };
    if value < 1 {
        return Err(PolicyError::Invalid(format!(
            "{CONCURRENCY_SETTING} must resolve to an integer >= 1, got {value}"
        )));
    }
    Ok(u32::try_from(value).unwrap_or(u32::MAX))
}

/// The execute-time held-out grader source, built only when `[held_out]
/// root` is configured (the gate is opt-in). A relative root resolves
/// against `repo_root` regardless of the worker's cwd or any sandbox path;
/// an absolute root is honored as written.
pub fn build_held_out_source(
    policy: Option<&WorkPolicy>,
    repo_root: &Path,
) -> Option<FilesystemHeldOutGraderSource> {
    let root = policy?.held_out_root.as_ref()?;
    Some(FilesystemHeldOutGraderSource::new(repo_root.join(root)))
}

// --- The RunTask binding ---------------------------------------------------------------

/// Map the policy's skill surface onto the core agent options: the `"all"`
/// sentinel is the full surface, `"none"` an empty allow-list, any other
/// single name a one-entry allow-list, and a list is passed through.
pub fn core_skills(skills: &PolicySkills) -> CoreSkills {
    match skills {
        PolicySkills::Named(name) if name == "all" => CoreSkills::All,
        PolicySkills::Named(name) if name == "none" => CoreSkills::Named(Vec::new()),
        PolicySkills::Named(name) => CoreSkills::Named(vec![name.clone()]),
        PolicySkills::List(names) => CoreSkills::Named(names.clone()),
    }
}

/// The [`RunTaskOptions`] one [`RunTaskRequest`] maps onto: every primitive
/// the loop decomposed (agent options, budgets, deadlines, the bound
/// landability gate and checkpoint probe, the seed's run id / worker id /
/// source, the injected store, sink, invoke, and shutdown signal).
pub fn run_task_options_for(
    request: &RunTaskRequest,
    stream: Option<LineSink>,
    events: EventsMode,
) -> RunTaskOptions {
    let agent = &request.agent;
    let config = &request.config;
    let store: Arc<dyn RunStore> = Arc::new(Arc::clone(&request.store));
    RunTaskOptions {
        agent_id: agent.agent_id.clone(),
        agent: AgentOptions {
            model: request.model.clone(),
            max_turns: request.max_turns,
            permission_mode: agent.permission_mode.clone(),
            skills: core_skills(&agent.skills),
            allowed_tools: agent.allowed_tools.clone(),
            denied_tools: agent.denied_tools.clone(),
            setting_sources: agent.setting_sources.clone(),
            mcp_servers: agent.mcp_servers.clone(),
            mcp_strict: agent.mcp_strict,
            exec_enabled: agent.exec_enabled,
            exec_auto_allow: agent.exec_auto_allow,
            agent_env: agent.agent_env.clone(),
            transport: agent.agent_transport.clone(),
            command_override: None,
        },
        max_retries: Some(config.max_retries),
        grader_env: config.grader_env.clone(),
        max_cost_usd: config.max_cost_usd,
        max_tokens: config.max_tokens,
        wall_clock_seconds: config.wall_clock_seconds,
        deadlines: Some(config.deadlines.clone()),
        rubric_judge_max_turns: Some(config.rubric_judge_max_turns),
        invoke: request.invoke.clone(),
        stream,
        events_out: None,
        run_id: Some(request.lifecycle.run_id.clone()),
        events,
        source: Some(request.source_ref.clone()).filter(|s| !s.is_empty()),
        worker_id: Some(request.worker_id.clone()),
        sink: request.sink.clone(),
        store: Some(store),
        artifacts_root: config.artifacts_root.clone(),
        landability_gate: config.landability_gate.clone(),
        checkpoint_nudge_seconds: Some(config.checkpoint_nudge_seconds),
        checkpoint_progress_probe: config.checkpoint_progress_probe.clone(),
        shutdown: request.shutdown.clone(),
    }
}

/// The production [`RunTask`]: hands each request to the core workflow's
/// [`run_task_object`] with every field mapped by [`run_task_options_for`].
/// `db_path` is the store path the workflow derives its telemetry root
/// from when no sink is injected (the loop always injects one).
pub struct CoreRunTask {
    db_path: PathBuf,
    stream: Option<LineSink>,
    events: EventsMode,
}

impl fmt::Debug for CoreRunTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreRunTask")
            .field("db_path", &self.db_path)
            .field("stream", &self.stream.is_some())
            .field("events", &self.events)
            .finish()
    }
}

impl CoreRunTask {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            stream: None,
            events: EventsMode::None,
        }
    }

    /// Diagnostics destination (`[workflow]` lines); stderr when unset.
    pub fn with_stream(mut self, stream: LineSink) -> Self {
        self.stream = Some(stream);
        self
    }

    pub fn with_events(mut self, events: EventsMode) -> Self {
        self.events = events;
        self
    }
}

fn workflow_error_to_run_task(err: WorkflowError) -> RunTaskError {
    match err {
        WorkflowError::Harness(err) => RunTaskError::from(err),
        WorkflowError::Store(err) => RunTaskError::Store(err),
        other => RunTaskError::Other(other.to_string()),
    }
}

impl RunTask for CoreRunTask {
    fn run(&self, request: RunTaskRequest) -> RunTaskFuture {
        let options = run_task_options_for(&request, self.stream.clone(), self.events);
        let db_path = self.db_path.clone();
        Box::pin(async move {
            run_task_object(&request.task, &db_path, &request.sandbox, options)
                .await
                .map_err(workflow_error_to_run_task)
        })
    }
}

// --- Container backend -----------------------------------------------------------------

/// The agent configuration one in-container invocation runs with: the
/// strategy's resolved model and permission policy on the `claude-code`
/// adapter (the Python container backend's `make_agents_invoke` parameters).
pub fn container_agent_configuration(spec: &ContainerInvokeSpec) -> AgentConfiguration {
    let mut configuration = AgentConfiguration::new(&spec.agent_id);
    configuration.model_id = Some(spec.model.clone());
    configuration.permission_policy = match spec.permission_policy.as_str() {
        "auto" => PermissionPolicy::Auto,
        "plan" => PermissionPolicy::Plan,
        "read_only" | "read-only" => PermissionPolicy::ReadOnly,
        _ => PermissionPolicy::Supervised,
    };
    configuration
}

/// The host-to-invoke builder the container strategy calls per run: the core
/// agents invoker over the `DockerExecHost` the strategy provides, under the
/// strategy's exec timeout.
pub fn host_invoke_builder() -> HostInvokeBuilder {
    Arc::new(|host: Arc<dyn ExecutionHost>, spec: &ContainerInvokeSpec| {
        AgentsInvoker::new(
            container_agent_configuration(spec),
            spec.working_directory.clone(),
        )
        .with_host(host)
        .with_timeout_seconds(spec.timeout_seconds)
        .into_invoke_fn()
    })
}

/// Select the run's submit strategy from `[sandbox] backend` (spec 00045):
/// `worktree` (the default) returns `inner` unchanged; `container` wraps it
/// so the worktree backend still provisions and lands while the agent runs
/// in the container through [`host_invoke_builder`].
pub fn wrap_for_backend(
    inner: Arc<dyn SubmitStrategy>,
    policy: Option<&WorkPolicy>,
    model: Option<&str>,
    environ: &BTreeMap<String, String>,
    log: &Logger,
) -> Result<Arc<dyn SubmitStrategy>, ConfigError> {
    maybe_wrap_for_backend(
        inner,
        policy,
        model,
        environ,
        host_invoke_builder(),
        Arc::clone(log),
    )
}

// --- The strategy registry the worker assembles -----------------------------------------

/// The full landing-strategy table: the built-ins (`merge`, `phase`) plus
/// the `pr` strategy and the tier router, registered from their own modules.
pub fn worker_registry() -> SubmitStrategyRegistry {
    let mut registry = builtin_registry();
    register_pr_strategy(&mut registry).expect("fresh registry has no pr entry");
    register_tier_strategy(&mut registry).expect("fresh registry has no tiered entry");
    registry
}

/// The registry name the policy selects: the tier router when
/// `[[submit.tiers]]` rules are configured (the route is per-diff, not
/// repo-global), else `[submit] strategy`, else `merge`.
pub fn strategy_name_for(policy: Option<&WorkPolicy>) -> &str {
    match policy {
        Some(p) if !p.submit_tiers.is_empty() => TIER_STRATEGY,
        Some(p) => p.submit_strategy.as_str(),
        None => MERGE_STRATEGY,
    }
}

// --- The cycle ---------------------------------------------------------------------------

/// The landing layout a cycle's bookkeeping runs against: the repo, the
/// merge lock, the resolved landing base, and the log sink -- the fields
/// the Python cycle read off its `GitWorktreeSubmitter`.
#[derive(Clone)]
pub struct LandingLayout {
    pub repo_root: PathBuf,
    pub lock_path: PathBuf,
    pub phase_base: String,
    pub log: Logger,
}

impl fmt::Debug for LandingLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LandingLayout")
            .field("repo_root", &self.repo_root)
            .field("lock_path", &self.lock_path)
            .field("phase_base", &self.phase_base)
            .finish_non_exhaustive()
    }
}

impl LandingLayout {
    /// The layout a worktree submitter was built for.
    pub fn of(submitter: &GitWorktreeSubmitter) -> Self {
        Self {
            repo_root: submitter.repo_root.clone(),
            lock_path: submitter.lock_path.clone(),
            phase_base: submitter.phase_base.clone(),
            log: Arc::clone(&submitter.log),
        }
    }
}

/// Everything one [`run_once`] cycle needs. `new` fills the Python keyword
/// defaults; the seams (`invoke`, `run_task`, `gh`, `environ`, `runtime`)
/// are test injection points.
#[derive(Clone)]
pub struct RunOnceRequest {
    pub strategy: Arc<dyn SubmitStrategy>,
    pub layout: LandingLayout,
    pub tasks_dir: PathBuf,
    pub db_path: PathBuf,
    pub worktrees_dir: PathBuf,
    pub model: Option<String>,
    pub max_turns: u32,
    pub max_retries: u32,
    pub worker_id: Option<String>,
    pub lease_seconds: f64,
    pub reconcile_seconds: Option<f64>,
    pub sweep_seconds: Option<f64>,
    pub landing_redrive_bound: Option<u32>,
    pub invoke: Option<InvokeFn>,
    pub stream: Option<LineSink>,
    pub policy: Option<Arc<WorkPolicy>>,
    pub held_out_source: Option<Arc<dyn HeldOutGraderSource>>,
    pub shutdown: Option<ShutdownSignal>,
    pub run_task: Option<Arc<dyn RunTask>>,
    pub environ: Option<BTreeMap<String, String>>,
    pub gh: Option<Arc<dyn GhRunner>>,
    pub runtime: Option<Arc<Runtime>>,
}

impl fmt::Debug for RunOnceRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunOnceRequest")
            .field("layout", &self.layout)
            .field("tasks_dir", &self.tasks_dir)
            .field("db_path", &self.db_path)
            .field("worktrees_dir", &self.worktrees_dir)
            .field("model", &self.model)
            .field("max_turns", &self.max_turns)
            .field("max_retries", &self.max_retries)
            .field("worker_id", &self.worker_id)
            .field("lease_seconds", &self.lease_seconds)
            .field("reconcile_seconds", &self.reconcile_seconds)
            .field("sweep_seconds", &self.sweep_seconds)
            .field("landing_redrive_bound", &self.landing_redrive_bound)
            .field("policy", &self.policy.is_some())
            .finish_non_exhaustive()
    }
}

impl RunOnceRequest {
    /// A request over an already-built strategy with the Python defaults.
    pub fn new(
        strategy: Arc<dyn SubmitStrategy>,
        layout: LandingLayout,
        tasks_dir: impl Into<PathBuf>,
        db_path: impl Into<PathBuf>,
        worktrees_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            strategy,
            layout,
            tasks_dir: tasks_dir.into(),
            db_path: db_path.into(),
            worktrees_dir: worktrees_dir.into(),
            model: None,
            max_turns: u32::try_from(DEFAULT_MAX_TURNS).unwrap_or(u32::MAX),
            max_retries: u32::try_from(DEFAULT_MAX_RETRIES).unwrap_or(u32::MAX),
            worker_id: None,
            lease_seconds: DEFAULT_LEASE_SECONDS,
            reconcile_seconds: None,
            sweep_seconds: Some(DEFAULT_SWEEP_SECONDS),
            landing_redrive_bound: Some(DEFAULT_LANDING_REDRIVE_BOUND),
            invoke: None,
            stream: None,
            policy: None,
            held_out_source: None,
            shutdown: None,
            run_task: None,
            environ: None,
            gh: None,
            runtime: None,
        }
    }

    /// A request driving `submitter` itself as the strategy (the worktree
    /// backend, unwrapped).
    pub fn for_submitter(
        submitter: &GitWorktreeSubmitter,
        tasks_dir: impl Into<PathBuf>,
        db_path: impl Into<PathBuf>,
        worktrees_dir: impl Into<PathBuf>,
    ) -> Self {
        Self::new(
            Arc::new(submitter.clone()),
            LandingLayout::of(submitter),
            tasks_dir,
            db_path,
            worktrees_dir,
        )
    }
}

/// Why a cycle did not complete normally.
#[derive(Debug, thiserror::Error)]
pub enum RunOnceError {
    /// The worker was shut down mid-session; the report carries the runs
    /// so far and the phase sweep already ran.
    #[error("orchestrate interrupted after {} run(s)", .0.runs.len())]
    Interrupted(OrchestratorReport),
    #[error("{0}")]
    StoreConfig(#[from] StoreConfigError),
    #[error("{0}")]
    Orchestrate(#[from] OrchestrateError),
    #[error("{0}")]
    Archive(#[from] ArchiveError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl RunOnceError {
    /// The Python exception class the failure corresponds to, for log
    /// lines that name the fault type.
    pub fn kind(&self) -> &'static str {
        match self {
            RunOnceError::Interrupted(_) => "KeyboardInterrupt",
            RunOnceError::StoreConfig(_) => "StoreConfigError",
            RunOnceError::Orchestrate(_) => "OrchestrateError",
            RunOnceError::Archive(_) => "ArchiveError",
            RunOnceError::Io(_) => "OSError",
        }
    }

    /// The breaker's bucket for this failure (see [`classify_cycle_error`]).
    pub fn fault_class(&self) -> Option<FaultClass> {
        match self {
            RunOnceError::Interrupted(_) => None,
            RunOnceError::StoreConfig(err) => match err {
                StoreConfigError::Core(err) => classify_error(err),
                StoreConfigError::Claims(err) => classify_claims_error(err),
                other => classify_error(other),
            },
            RunOnceError::Orchestrate(err) => match err {
                OrchestrateError::Store(err) => classify_error(err),
                OrchestrateError::Claims(err) => classify_claims_error(err),
                OrchestrateError::RunTask(RunTaskError::Store(err)) => classify_error(err),
                OrchestrateError::RunTask(RunTaskError::Harness(HarnessError::Store(err))) => {
                    classify_error(err)
                }
                other => classify_error(other),
            },
            RunOnceError::Archive(err) => match err {
                ArchiveError::Store(err) => classify_error(err),
                ArchiveError::Claims(err) => classify_claims_error(err),
                other => classify_error(other),
            },
            RunOnceError::Io(err) => classify_error(err),
        }
    }
}

/// A claim-store schema-version mismatch is as permanent as a core one;
/// anything else classifies by its source chain.
fn classify_claims_error(err: &ClaimStoreError) -> Option<FaultClass> {
    if err.is_schema_mismatch() {
        return Some(FaultClass::Permanent);
    }
    classify_error(err)
}

/// Bucket a cycle failure for the breaker. A [`RunOnceError`] is unwrapped
/// variant by variant: the store errors it carries are `transparent`
/// wrappers whose `source()` skips the typed `StoreError` level, so the
/// generic source-chain walk alone would miss a schema mismatch and burn
/// every strike on a fault that can never clear.
pub fn classify_cycle_error(err: &CycleError) -> Option<FaultClass> {
    if let Some(run_once) = err.downcast_ref::<RunOnceError>() {
        return run_once.fault_class();
    }
    let fault: &(dyn std::error::Error + 'static) = err.as_ref();
    classify_error(fault)
}

fn git_probe_factory() -> ProgressProbeFactory {
    Arc::new(|sandbox: &Path| {
        let probe = git_progress_probe(sandbox);
        Arc::new(move || probe().ok().map(Value::String)) as CoreProgressProbe
    })
}

/// One cycle: record phase bases, drain every eligible task to quiescence
/// through the git-submit seam, archive completed phases.
///
/// The stores are opened through the orchestrator's store factory (the
/// policy selects the backend); the git checkpoint-nudge probe factory is
/// always handed to `orchestrate`, which binds it per-run over each task's
/// sandbox. The archive sweep runs under the same merge lock the FF-merge
/// path uses: pool members share one repo and lock, so two members
/// finishing the last task of one phase would otherwise both pass the
/// unlocked `dest.exists()` check and race the move. Under the phase
/// strategy (or tier routing) the sweep hands a completed phase to the
/// phase PR publisher instead of archiving it.
///
/// Per-run forensics are the telemetry JSONL files the harness's sink
/// writes under `<db dir>/logs/runs/`; the worker renders no `.log` file.
pub fn run_once(request: RunOnceRequest) -> Result<OrchestratorReport, RunOnceError> {
    let layout = request.layout.clone();
    let log = Arc::clone(&layout.log);
    record_phase_bases(
        &layout.repo_root,
        &request.tasks_dir,
        &layout.lock_path,
        &log,
    )?;

    let environ = request.environ.clone().unwrap_or_else(environ_from_process);
    let registries = default_store_registries();
    let policy = request.policy.clone();
    let stores = open_stores_for(policy.as_deref(), &request.db_path, &registries, &environ)?;
    let runtime = match request.runtime.clone() {
        Some(runtime) => runtime,
        None => Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?,
        ),
    };
    let run_task: Arc<dyn RunTask> = match request.run_task.clone() {
        Some(run_task) => run_task,
        None => {
            let mut core = CoreRunTask::new(&request.db_path);
            if let Some(stream) = request.stream.clone() {
                core = core.with_stream(stream);
            }
            Arc::new(core)
        }
    };
    let logs_root = request
        .db_path
        .parent()
        .map(|p| p.join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"));
    let orchestrate_request = OrchestrateRequest {
        source: Arc::new(DirectoryWorkSource::new(&request.tasks_dir)),
        control: Arc::clone(&stores.core),
        claims: Arc::clone(&stores.claims),
        run_task,
        sandbox_root: request.worktrees_dir.clone(),
        policy: policy.as_deref().cloned(),
        invoke: request.invoke.clone(),
        model: request.model.clone(),
        max_turns: request.max_turns,
        max_retries: request.max_retries,
        worker_id: request.worker_id.clone(),
        lease_seconds: request.lease_seconds,
        steal_grace_seconds: None,
        log: Some(Arc::clone(&log)),
        now: None,
        strategy: Some(Arc::clone(&request.strategy)),
        reconcile_seconds: request.reconcile_seconds,
        sweep_seconds: request.sweep_seconds,
        landing_redrive_bound: request.landing_redrive_bound,
        prereq_redrive_bound: DEFAULT_PREREQ_REDRIVE_BOUND,
        session_pause_ceiling_seconds: policy
            .as_deref()
            .map_or(DEFAULT_SESSION_PAUSE_CEILING_SECONDS, |p| {
                p.worker_session_pause_ceiling_seconds
            }),
        repo_root: Some(layout.repo_root.clone()),
        held_out_source: request.held_out_source.clone(),
        progress_probe_factory: Some(git_probe_factory()),
        telemetry_root: Some(logs_root),
        sink: None,
        shutdown: request.shutdown.clone(),
        // A long-lived daemon idling over an unchanged queue must not
        // accumulate duplicate graph snapshots.
        snapshot_policy: GraphSnapshotPolicy::SkipUnchanged,
        environ: Some(environ),
    };
    let (report, interrupted) = match runtime.block_on(orchestrate(orchestrate_request)) {
        Ok(report) => (report, false),
        Err(OrchestrateError::Interrupted { report }) => (report, true),
        Err(err) => return Err(err.into()),
    };

    {
        let _lock = merge_lock(&layout.lock_path)?;
        // Phase-strategy completion seam (spec 00079, criterion 3): the true
        // base for the phase PR is the operator's base (`[submit] pr_base`
        // or the resolved landing base), never the phase branch. Built only
        // for the phase strategy and for tier routing.
        let publisher = policy
            .as_deref()
            .filter(|p| p.submit_strategy == "phase" || !p.submit_tiers.is_empty())
            .map(|p| {
                let pr_base = p
                    .submit_pr_base
                    .clone()
                    .unwrap_or_else(|| layout.phase_base.clone());
                let evidence: Arc<dyn PhaseEvidenceStore> = Arc::new(Arc::clone(&stores.core));
                let mut publisher = PhasePrPublisher::new(
                    evidence,
                    &layout.repo_root,
                    p.submit_remote.clone(),
                    pr_base,
                    Arc::clone(&log),
                )
                .with_phase_verify(p.phase_verify.clone());
                if let Some(gh) = request.gh.clone() {
                    publisher = publisher.with_gh(gh);
                }
                publisher
            });
        let completion = publisher.as_ref().map(PhasePrPublisher::completion);
        let sweep = ArchiveSweep {
            repo_root: Some(&layout.repo_root),
            landing_base: Some(&layout.phase_base),
            true_base: None,
            policy: policy.as_deref(),
            phase_completion: completion
                .as_ref()
                .map(|c| c as &dyn Fn(&Path, &[flywheel_core::task::Task])),
        };
        archive_phases(
            &request.tasks_dir,
            &stores.core,
            Some(stores.claims.as_ref()),
            &log,
            &sweep,
        )?;
    }
    if interrupted {
        return Err(RunOnceError::Interrupted(report));
    }
    Ok(report)
}

// --- The daemon loop ----------------------------------------------------------------------

/// How one cycle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed,
    /// The operator interrupted the cycle; the loop stops cleanly.
    Interrupted,
}

/// A cycle failure the breaker classifies.
pub type CycleError = Box<dyn std::error::Error + Send + Sync + 'static>;
/// Observes a counted strike: the failure and the consecutive-failure count.
pub type CycleFailureHook<'a> = &'a dyn Fn(&CycleError, u32);
/// Observes the permanent-stop verdict with the fault that caused it.
pub type PermanentStopHook<'a> = &'a dyn Fn(&CycleError);

/// The loop's observer hooks and the test-only cycle bound.
#[derive(Default)]
pub struct LoopHooks<'a> {
    /// Runs before every cycle (re-arming signal handlers in the Python
    /// daemon; unused by the Rust production loop, which arms once).
    pub before_cycle: Option<&'a dyn Fn()>,
    pub on_cycle_failure: Option<CycleFailureHook<'a>>,
    pub on_give_up: Option<&'a dyn Fn(u32)>,
    pub on_permanent_stop: Option<PermanentStopHook<'a>>,
    pub on_interrupt: Option<&'a dyn Fn()>,
    /// A test-only safety bound on the number of cycles.
    pub max_cycles: Option<u32>,
}

impl fmt::Debug for LoopHooks<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoopHooks")
            .field("max_cycles", &self.max_cycles)
            .finish_non_exhaustive()
    }
}

/// The worker daemon's cross-cycle circuit breaker.
///
/// Each pass runs one cycle, then waits `poll_interval` before the next;
/// `once` stops after the first successful pass. The loop exits only on
/// `should_stop` (a SIGTERM/SIGINT flag in production) or a breaker
/// verdict. The breaker sits ABOVE the store's own bounded transient retry:
/// a fault that clears within that budget never reaches here. A cycle that
/// does fail counts a strike (`on_cycle_failure`); after
/// [`MAX_CONSECUTIVE_CYCLE_FAILURES`] consecutive strikes the loop gives up
/// (`on_give_up`, returns 1). A successful cycle resets the count. A
/// PERMANENT-classified fault (a schema-version mismatch reopening the
/// store every cycle) can never succeed on retry, so it is not counted as a
/// strike: the loop signals `on_permanent_stop` and returns
/// [`PERMANENT_STOP_EXIT_CODE`] after exactly one cycle. An interrupted
/// cycle signals `on_interrupt` and stops the loop (0).
pub fn run_daemon_loop(
    run_cycle: &mut dyn FnMut() -> Result<CycleOutcome, CycleError>,
    once: bool,
    poll_interval: f64,
    should_stop: &dyn Fn() -> bool,
    sleep: &dyn Fn(f64, &dyn Fn() -> bool),
    hooks: &LoopHooks<'_>,
) -> i32 {
    let mut consecutive_failures: u32 = 0;
    let mut cycles: u32 = 0;
    while !should_stop() {
        if let Some(before) = hooks.before_cycle {
            before();
        }
        match run_cycle() {
            Ok(CycleOutcome::Interrupted) => {
                if let Some(on_interrupt) = hooks.on_interrupt {
                    on_interrupt();
                }
                break;
            }
            Err(err) => {
                if classify_cycle_error(&err) == Some(FaultClass::Permanent) {
                    if let Some(on_permanent_stop) = hooks.on_permanent_stop {
                        on_permanent_stop(&err);
                    }
                    return PERMANENT_STOP_EXIT_CODE;
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
                    return 1;
                }
                if hooks.max_cycles.is_some_and(|max| cycles >= max) {
                    break;
                }
                if should_stop() {
                    break;
                }
                sleep(CYCLE_FAILURE_BACKOFF_SECONDS, should_stop);
                continue;
            }
            Ok(CycleOutcome::Completed) => {}
        }
        consecutive_failures = 0;
        cycles += 1;
        if once {
            break;
        }
        if hooks.max_cycles.is_some_and(|max| cycles >= max) {
            break;
        }
        if should_stop() {
            break;
        }
        sleep(poll_interval, should_stop);
    }
    0
}

/// Sleep up to `seconds` (whole seconds, as the Python loop did), waking
/// each second to re-check `should_stop`.
pub fn interruptible_sleep(seconds: f64, should_stop: &dyn Fn() -> bool) {
    let whole = if seconds.is_finite() {
        (seconds as i64).max(0)
    } else {
        0
    };
    for _ in 0..whole {
        if should_stop() {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Arm the daemon's shutdown signals for the life of the process: every
/// SIGTERM/SIGINT sets `stop` (the loop ends after the current cycle),
/// triggers `shutdown` (the in-flight run parks `interrupted` in-band), and
/// logs the request. Spawned on `runtime`; a no-op off unix.
pub fn arm_signals(runtime: &Runtime, stop: StopFlag, shutdown: ShutdownSignal, log: Logger) {
    runtime.spawn(watch_signals(stop, shutdown, log));
}

#[cfg(unix)]
async fn watch_signals(stop: StopFlag, shutdown: ShutdownSignal, log: Logger) {
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
        shutdown.trigger();
        log(&format!(
            "Shutdown requested (signal {signum}); stopping after the current cycle."
        ));
    }
}

#[cfg(not(unix))]
async fn watch_signals(_stop: StopFlag, _shutdown: ShutdownSignal, _log: Logger) {}

// --- The `flywheel worker` command line ---------------------------------------------------

/// The parsed `flywheel worker` flags, with argparse's defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerArgs {
    pub tasks_dir: Option<String>,
    pub db: Option<String>,
    pub sandbox_root: Option<String>,
    pub model: Option<String>,
    pub max_turns: i64,
    pub max_retries: i64,
    pub worktree_retention_days: i64,
    pub run_log_retention: i64,
    pub heartbeat: i64,
    pub poll_interval: i64,
    pub worker_id: Option<String>,
    /// Validated later by [`resolve_concurrency`], exactly as the Python
    /// parser left it a string.
    pub concurrency: Option<String>,
    pub lease_seconds: f64,
    pub reconcile_seconds: f64,
    pub once: bool,
}

impl Default for WorkerArgs {
    fn default() -> Self {
        Self {
            tasks_dir: None,
            db: None,
            sandbox_root: None,
            model: None,
            max_turns: DEFAULT_MAX_TURNS,
            max_retries: DEFAULT_MAX_RETRIES,
            worktree_retention_days: i64::try_from(DEFAULT_RETENTION_DAYS).unwrap_or(i64::MAX),
            run_log_retention: i64::try_from(DEFAULT_RUN_LOG_RETENTION).unwrap_or(i64::MAX),
            heartbeat: DEFAULT_HEARTBEAT_SECONDS,
            poll_interval: DEFAULT_POLL_INTERVAL_SECONDS,
            worker_id: None,
            concurrency: None,
            lease_seconds: DEFAULT_LEASE_SECONDS,
            reconcile_seconds: DEFAULT_RECONCILE_SECONDS,
            once: false,
        }
    }
}

/// The outcome of parsing: the flags, or the help text to print.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Args(WorkerArgs),
    Help(String),
}

/// An argparse usage error: rendered as the usage block plus the
/// `flywheel worker: error: <message>` line, exit 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError {
    pub message: String,
}

impl ArgError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The exact stderr text argparse prints.
    pub fn render(&self) -> String {
        format!("{USAGE}flywheel worker: error: {}\n", self.message)
    }
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "flywheel worker: error: {}", self.message)
    }
}

impl std::error::Error for ArgError {}

/// The argparse usage block (`COLUMNS=80`).
pub const USAGE: &str = "usage: flywheel worker [-h] [--tasks-dir TASKS_DIR] [--db DB]
                       [--sandbox-root SANDBOX_ROOT] [--model MODEL]
                       [--max-turns MAX_TURNS] [--max-retries MAX_RETRIES]
                       [--worktree-retention-days WORKTREE_RETENTION_DAYS]
                       [--run-log-retention RUN_LOG_RETENTION]
                       [--heartbeat HEARTBEAT] [--poll-interval POLL_INTERVAL]
                       [--worker-id WORKER_ID] [--concurrency CONCURRENCY]
                       [--lease-seconds LEASE_SECONDS]
                       [--reconcile-seconds RECONCILE_SECONDS] [--once]
";

/// The argparse `-h` text (`COLUMNS=80`).
pub const HELP: &str = "usage: flywheel worker [-h] [--tasks-dir TASKS_DIR] [--db DB]
                       [--sandbox-root SANDBOX_ROOT] [--model MODEL]
                       [--max-turns MAX_TURNS] [--max-retries MAX_RETRIES]
                       [--worktree-retention-days WORKTREE_RETENTION_DAYS]
                       [--run-log-retention RUN_LOG_RETENTION]
                       [--heartbeat HEARTBEAT] [--poll-interval POLL_INTERVAL]
                       [--worker-id WORKER_ID] [--concurrency CONCURRENCY]
                       [--lease-seconds LEASE_SECONDS]
                       [--reconcile-seconds RECONCILE_SECONDS] [--once]

Git-worktree worker: drive flywheel tasks under .flywheel/tasks/active/, each
in its own worktree, FF-merging on done and parking on failure.

options:
  -h, --help            show this help message and exit
  --tasks-dir TASKS_DIR
  --db DB
  --sandbox-root SANDBOX_ROOT
                        Root under which each task's worktree is created
                        (default: [paths] sandbox_root in flywheel.toml, else
                        .flywheel/worktrees). Relative paths anchor at the
                        repo root; @cache and @sibling select out-of-tree
                        layouts.
  --model MODEL
  --max-turns MAX_TURNS
  --max-retries MAX_RETRIES
  --worktree-retention-days WORKTREE_RETENTION_DAYS
  --run-log-retention RUN_LOG_RETENTION
                        Cap on per-run telemetry JSONL files kept under
                        .flywheel/logs/runs/; the oldest past the cap are
                        reclaimed each cycle so the most-recent runs survive
                        (0 disables).
  --heartbeat HEARTBEAT
                        Per-in-flight-task progress line every N seconds (0
                        disables).
  --poll-interval POLL_INTERVAL
                        Seconds to wait between drain cycles when idle.
  --worker-id WORKER_ID
  --concurrency CONCURRENCY
                        Worker pool size: how many tasks this invocation
                        drives concurrently. Overrides [worker] concurrency in
                        flywheel.toml (default 1). Must resolve to an integer
                        >= 1.
  --lease-seconds LEASE_SECONDS
  --reconcile-seconds RECONCILE_SECONDS
                        Steering bridge: re-list the work source every N
                        seconds and interrupt in-flight runs whose item
                        vanished (e.g. its task file was deleted). 0 disables
                        (default: 15).
  --once                Run a single drain cycle and exit (no daemon loop).
";

/// Whether an option takes a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    Flag,
    One,
}

/// The option table in declaration order (argparse's prefix-match order).
const OPTIONS: [(&str, Arity); 17] = [
    ("-h", Arity::Flag),
    ("--help", Arity::Flag),
    ("--tasks-dir", Arity::One),
    ("--db", Arity::One),
    ("--sandbox-root", Arity::One),
    ("--model", Arity::One),
    ("--max-turns", Arity::One),
    ("--max-retries", Arity::One),
    ("--worktree-retention-days", Arity::One),
    ("--run-log-retention", Arity::One),
    ("--heartbeat", Arity::One),
    ("--poll-interval", Arity::One),
    ("--worker-id", Arity::One),
    ("--concurrency", Arity::One),
    ("--lease-seconds", Arity::One),
    ("--reconcile-seconds", Arity::One),
    ("--once", Arity::Flag),
];

fn is_negative_number(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };
    let int_ok = int_part.chars().all(|c| c.is_ascii_digit());
    match frac_part {
        None => int_ok && !int_part.is_empty(),
        Some(f) => {
            int_ok
                && f.chars().all(|c| c.is_ascii_digit())
                && !(int_part.is_empty() && f.is_empty())
        }
    }
}

fn looks_like_option(token: &str) -> bool {
    token.starts_with('-') && token.len() > 1 && !is_negative_number(token)
}

/// Resolve `token` against the option table: an exact match, else a unique
/// long-option prefix (an ambiguous prefix is an error, an unknown one is
/// an unrecognized argument).
fn resolve_option(token: &str) -> Result<Option<(&'static str, Arity)>, ArgError> {
    if let Some(spec) = OPTIONS.iter().find(|(name, _)| *name == token) {
        return Ok(Some(*spec));
    }
    if !token.starts_with("--") {
        return Ok(None);
    }
    let matches: Vec<(&'static str, Arity)> = OPTIONS
        .iter()
        .filter(|(name, _)| name.starts_with("--") && name.starts_with(token))
        .copied()
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => {
            let names: Vec<&str> = matches.iter().map(|(name, _)| *name).collect();
            Err(ArgError::new(format!(
                "ambiguous option: {token} could match {}",
                names.join(", ")
            )))
        }
    }
}

fn parse_int_flag(name: &str, raw: &str) -> Result<i64, ArgError> {
    parse_py_int(raw)
        .ok_or_else(|| ArgError::new(format!("argument {name}: invalid int value: '{raw}'")))
}

fn parse_float_flag(name: &str, raw: &str) -> Result<f64, ArgError> {
    let text = raw.trim();
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    if text.is_empty() {
        return Err(ArgError::new(format!(
            "argument {name}: invalid float value: '{raw}'"
        )));
    }
    cleaned
        .parse::<f64>()
        .map_err(|_| ArgError::new(format!("argument {name}: invalid float value: '{raw}'")))
}

/// Parse the `flywheel worker` argv (everything after the verb) with
/// argparse-identical usage, help, and error text: long options accept any
/// unambiguous prefix and the `--flag=value` spelling, a repeated scalar
/// option takes its last value, and leftover tokens (an unknown option or a
/// positional) are `unrecognized arguments`.
pub fn parse_args(argv: &[String]) -> Result<Parsed, ArgError> {
    let mut args = WorkerArgs::default();
    let mut extras: Vec<String> = Vec::new();
    let mut iter = argv.iter().peekable();
    let mut only_positionals = false;
    while let Some(arg) = iter.next() {
        if only_positionals || !looks_like_option(arg) {
            extras.push(arg.clone());
            continue;
        }
        if arg == "--" {
            only_positionals = true;
            continue;
        }
        let (flag_text, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let Some((name, arity)) = resolve_option(flag_text)? else {
            extras.push(arg.clone());
            continue;
        };
        let value = match arity {
            Arity::Flag => {
                if let Some(value) = inline {
                    return Err(ArgError::new(format!(
                        "argument {name}: ignored explicit argument '{value}'"
                    )));
                }
                None
            }
            Arity::One => Some(match inline {
                Some(value) => value,
                None => match iter.peek() {
                    Some(next) if !looks_like_option(next) => {
                        let v = (*next).clone();
                        iter.next();
                        v
                    }
                    _ => {
                        return Err(ArgError::new(format!(
                            "argument {name}: expected one argument"
                        )));
                    }
                },
            }),
        };
        let v = value.unwrap_or_default();
        match name {
            "-h" | "--help" => return Ok(Parsed::Help(HELP.to_owned())),
            "--tasks-dir" => args.tasks_dir = Some(v),
            "--db" => args.db = Some(v),
            "--sandbox-root" => args.sandbox_root = Some(v),
            "--model" => args.model = Some(v),
            "--max-turns" => args.max_turns = parse_int_flag(name, &v)?,
            "--max-retries" => args.max_retries = parse_int_flag(name, &v)?,
            "--worktree-retention-days" => {
                args.worktree_retention_days = parse_int_flag(name, &v)?;
            }
            "--run-log-retention" => args.run_log_retention = parse_int_flag(name, &v)?,
            "--heartbeat" => args.heartbeat = parse_int_flag(name, &v)?,
            "--poll-interval" => args.poll_interval = parse_int_flag(name, &v)?,
            "--worker-id" => args.worker_id = Some(v),
            "--concurrency" => args.concurrency = Some(v),
            "--lease-seconds" => args.lease_seconds = parse_float_flag(name, &v)?,
            "--reconcile-seconds" => args.reconcile_seconds = parse_float_flag(name, &v)?,
            "--once" => args.once = true,
            _ => {}
        }
    }
    if !extras.is_empty() {
        return Err(ArgError::new(format!(
            "unrecognized arguments: {}",
            extras.join(" ")
        )));
    }
    Ok(Parsed::Args(args))
}

// --- The entry point ----------------------------------------------------------------------

fn clamp_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

fn clamp_u64(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(0)
}

fn clamp_usize(value: i64) -> usize {
    usize::try_from(value.max(0)).unwrap_or(usize::MAX)
}

fn describe_cycle_error(err: &CycleError) -> String {
    match err.downcast_ref::<RunOnceError>() {
        Some(err) => format!("{}: {err}", err.kind()),
        None => err.to_string(),
    }
}

/// `flywheel worker` against the real stdio; returns the exit code.
pub fn main(argv: &[String]) -> i32 {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let code = main_with(argv, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}

/// `flywheel worker` with injected stdout/stderr sinks for the parser and
/// startup diagnostics (the worker's own log lines go to the process stderr
/// through [`make_logger`], as the Python daemon's did).
///
/// Resolution order mirrors the Python `main`: policy, landing base, pool
/// size, and worktree root resolve BEFORE any store opens (a policy error
/// exits 2 having claimed nothing); a resolved concurrency above 1 makes
/// this invocation the pool supervisor ([`crate::pool::run_pool`]); the
/// single-worker path builds the policy-selected landing strategy (wrapped
/// for the container backend when configured), reaps orphan containers,
/// starts the heartbeat, arms the shutdown signals, and drives
/// [`run_daemon_loop`] over [`run_once`].
pub fn main_with(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let args = match parse_args(argv) {
        Ok(Parsed::Args(args)) => args,
        Ok(Parsed::Help(text)) => {
            let _ = out.write_all(text.as_bytes());
            return 0;
        }
        Err(arg_error) => {
            let _ = err.write_all(arg_error.render().as_bytes());
            return 2;
        }
    };
    let repo_root = match repo_root() {
        Ok(root) => root,
        Err(failure) => {
            let _ = writeln!(err, "{failure}");
            return 1;
        }
    };
    let log = make_logger("[worker]");

    // Loaded once: the same policy resolves the agent model, the landing
    // base, and the store backend for every store this process constructs.
    // Base resolution reads [submit] base, so it runs AFTER the policy
    // loads; the pool size and the worktree root resolve before any store
    // opens so an operator mistake fails fast with nothing claimed.
    let resolved: Result<_, PolicyError> = (|| {
        let policy = load_effective_policy(None)?.map(Arc::new);
        let phase_base = resolve_landing_base(&repo_root, policy.as_deref(), Some(&log))?;
        let concurrency = resolve_concurrency(args.concurrency.as_deref(), policy.as_deref())?;
        let configured_root = args
            .sandbox_root
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| policy.as_deref().and_then(|p| p.sandbox_root.clone()));
        let worktrees_dir = resolve_sandbox_root(configured_root.as_deref(), &repo_root)?;
        Ok((policy, phase_base, concurrency, worktrees_dir))
    })();
    let (policy, phase_base, concurrency, worktrees_dir) = match resolved {
        Ok(resolved) => resolved,
        Err(failure) => {
            let _ = writeln!(err, "flywheel worker: policy error: {failure}");
            return 2;
        }
    };
    let model = resolve_model(args.model.as_deref(), policy.as_deref());

    let tasks_dir = args
        .tasks_dir
        .as_deref()
        .filter(|s| !s.is_empty())
        .map_or_else(|| repo_root.join(".flywheel").join("tasks"), PathBuf::from);
    let db_path = args.db.as_deref().filter(|s| !s.is_empty()).map_or_else(
        || repo_root.join(".flywheel").join("flywheel.sqlite"),
        PathBuf::from,
    );
    let lock_path = repo_root.join(".flywheel").join(".merge.lock");
    let db_parent = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(failure) =
        std::fs::create_dir_all(&worktrees_dir).and_then(|()| std::fs::create_dir_all(&db_parent))
    {
        let _ = writeln!(err, "flywheel worker: {failure}");
        return 1;
    }

    // A resolved concurrency > 1 makes this invocation the pool supervisor,
    // not a worker: it spawns N single-task members (each re-invoking this
    // verb at --concurrency 1) against the one shared store.
    if concurrency > 1 {
        let program = match crate::pool::member_program() {
            Ok(program) => program,
            Err(failure) => {
                let _ = writeln!(err, "flywheel worker: {failure}");
                return 1;
            }
        };
        return crate::pool::run_pool(
            program,
            &args,
            concurrency,
            model.as_deref(),
            &db_path,
            args.worker_id.as_deref(),
            log,
        );
    }

    let environ = environ_from_process();
    let protected_paths = policy
        .as_deref()
        .map(|p| p.protected_paths.clone())
        .unwrap_or_default();
    let setup_command = policy.as_deref().and_then(|p| p.sandbox_setup.clone());
    let (on_done, on_failure) = policy.as_deref().map_or_else(
        || ("destroy".to_owned(), "park".to_owned()),
        |p| {
            (
                p.sandbox.retention.on_done.clone(),
                p.sandbox.retention.on_failure.clone(),
            )
        },
    );
    // Resolved [sandbox.env] for submit-time re-verification, matching the
    // env the in-run command graders (and the agent) saw.
    let grader_env = policy
        .as_deref()
        .and_then(|p| resolve_grader_env(&p.sandbox.env, Some(&environ)));
    // The submitter's own store handle: it records a queryable
    // LANDING_PARKED / LANDED event on the run ledger. Same backing store
    // (policy-selected) as the run's lifecycle, opened on db_path.
    let registries = default_store_registries();
    let submit_store = match build_store(
        &registries,
        &store_config_for(policy.as_deref(), &db_path),
        &environ,
    ) {
        Ok(store) => store,
        Err(failure) => {
            let _ = writeln!(err, "flywheel worker: store error: {failure}");
            return 1;
        }
    };
    let held_out_source = build_held_out_source(policy.as_deref(), &repo_root).map(Arc::new);
    if let Some(source) = &held_out_source {
        log(&format!(
            "held-out gate active root={}",
            source.root.display()
        ));
    }
    let held_out: Option<Arc<dyn HeldOutGraderSource>> = held_out_source
        .clone()
        .map(|source| source as Arc<dyn HeldOutGraderSource>);
    let ledger: Arc<dyn LandingLedger> = Arc::new(Arc::clone(&submit_store));
    let mut build_request = SubmitBuildRequest::new(
        repo_root.clone(),
        tasks_dir.clone(),
        worktrees_dir.clone(),
        phase_base.clone(),
        lock_path.clone(),
        Arc::clone(&log),
    )
    .with_protected_paths(protected_paths)
    .with_retention(on_done, on_failure)
    .with_store(ledger)
    .with_resolve_conflict(make_agents_resolve_conflict(
        policy
            .as_deref()
            .and_then(|p| p.agent_id.as_deref())
            .unwrap_or(DEFAULT_AGENT_ID),
        policy.as_deref().and_then(|p| p.agent_transport.as_deref()),
        None,
    ));
    if let Some(policy) = policy.clone() {
        build_request = build_request.with_policy(policy);
    }
    if let Some(command) = setup_command {
        build_request = build_request.with_setup_command(command);
    }
    if let Some(env) = grader_env {
        build_request = build_request.with_grader_env(env);
    }
    if let Some(source) = held_out.clone() {
        build_request = build_request.with_held_out_source(source);
    }
    let registry = worker_registry();
    let strategy_name = strategy_name_for(policy.as_deref()).to_owned();
    let submitter = match registry.build(&strategy_name, &build_request) {
        Ok(strategy) => strategy,
        Err(failure) => {
            let _ = writeln!(err, "flywheel worker: {failure}");
            return 2;
        }
    };
    let run_strategy = match wrap_for_backend(
        submitter,
        policy.as_deref(),
        model.as_deref(),
        &environ,
        &log,
    ) {
        Ok(strategy) => strategy,
        Err(failure) => {
            let _ = writeln!(err, "flywheel worker: {failure}");
            return 2;
        }
    };
    let layout = LandingLayout {
        repo_root: repo_root.clone(),
        lock_path,
        phase_base: phase_base.clone(),
        log: Arc::clone(&log),
    };

    let run_logs_dir = db_parent.join("logs").join("runs");
    log(&format!(
        "started pid={} base={phase_base} db={} concurrency={concurrency}",
        std::process::id(),
        db_path.display()
    ));
    log(&format!(
        "tasks={} worktrees={} logs={}",
        tasks_dir.display(),
        worktrees_dir.display(),
        run_logs_dir.display()
    ));

    // Reap any flywheel-owned container a prior killed worker left
    // orphaned (the SIGKILL/OOM case the exit-time registry cannot cover).
    reap_container_orphans(&*log);
    let mut heartbeat = Heartbeat::new(
        &db_path,
        args.heartbeat,
        make_logger("[heartbeat]"),
        policy.clone(),
    )
    .start();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => Arc::new(runtime),
        Err(failure) => {
            let _ = writeln!(err, "flywheel worker: {failure}");
            return 1;
        }
    };
    let stop = StopFlag::new();
    let shutdown = ShutdownSignal::new();
    arm_signals(&runtime, stop.clone(), shutdown.clone(), Arc::clone(&log));

    let cycle_log = Arc::clone(&log);
    let retention_days = clamp_u64(args.worktree_retention_days);
    let run_log_retention = clamp_usize(args.run_log_retention);
    let mut run_cycle = || -> Result<CycleOutcome, CycleError> {
        // Default-on run-log retention: hold .flywheel/logs/runs/ at or under
        // the configured bound before draining. Worktree retention runs every
        // cycle, not once at boot, with the clock read fresh each tick; when
        // the configured root moved off the legacy .flywheel/worktrees, that
        // location is swept too until it empties out.
        sweep_run_logs(&run_logs_dir, run_log_retention, &*cycle_log);
        for sweep_root in retention_targets(&repo_root, &worktrees_dir) {
            retention_cadence_tick(
                &repo_root,
                &sweep_root,
                retention_days,
                &*cycle_log,
                &wall_clock_seconds,
            );
        }
        let mut request = RunOnceRequest::new(
            Arc::clone(&run_strategy),
            layout.clone(),
            &tasks_dir,
            &db_path,
            &worktrees_dir,
        );
        request.model = model.clone();
        request.max_turns = clamp_u32(args.max_turns);
        request.max_retries = clamp_u32(args.max_retries);
        request.worker_id = args.worker_id.clone();
        request.lease_seconds = args.lease_seconds;
        request.reconcile_seconds = Some(args.reconcile_seconds).filter(|s| *s != 0.0);
        request.policy = policy.clone();
        request.held_out_source = held_out.clone();
        request.shutdown = Some(shutdown.clone());
        request.runtime = Some(Arc::clone(&runtime));
        match run_once(request) {
            Ok(_) => Ok(CycleOutcome::Completed),
            Err(RunOnceError::Interrupted(_)) => Ok(CycleOutcome::Interrupted),
            Err(failure) => Err(Box::new(failure) as CycleError),
        }
    };
    let should_stop = || stop.is_set();
    let sleep = |seconds: f64, stop: &dyn Fn() -> bool| interruptible_sleep(seconds, stop);
    let on_cycle_failure = |failure: &CycleError, n: u32| {
        log(&format!(
            "Cycle failed ({}) [{n}/{MAX_CONSECUTIVE_CYCLE_FAILURES}]",
            describe_cycle_error(failure)
        ));
    };
    let on_give_up = |_n: u32| {
        log("Too many consecutive cycle failures; exiting for operator inspection.");
    };
    let on_permanent_stop = |failure: &CycleError| {
        log(&format!(
            "Cycle hit a permanent fault ({}); stopping the worker loop after one cycle for \
             operator inspection (retrying cannot reconcile it).",
            describe_cycle_error(failure)
        ));
    };
    let on_interrupt = || {
        log("Interrupted mid-run; in-flight task finalized to interrupted. Shutting down.");
    };
    let hooks = LoopHooks {
        before_cycle: None,
        on_cycle_failure: Some(&on_cycle_failure),
        on_give_up: Some(&on_give_up),
        on_permanent_stop: Some(&on_permanent_stop),
        on_interrupt: Some(&on_interrupt),
        max_cycles: None,
    };
    let code = run_daemon_loop(
        &mut run_cycle,
        args.once,
        args.poll_interval as f64,
        &should_stop,
        &sleep,
        &hooks,
    );
    heartbeat.stop();
    log("Shutting down.");
    code
}
