//! Port of the Python `flywheel._cli` module: the `flywheel` / `fw` verb
//! router.
//!
//! Bare `flywheel` (or `fw`) opens the operator console; `--json` or a
//! non-TTY stdout prints one JSON snapshot and exits (delegated to
//! [`tui::main_with`], which already implements that fork, with the
//! console's worker / autopilot supervisors wired in). Every other verb
//! forwards its remaining argv to the pre-existing subcommand
//! implementation so output and exit codes match the originals
//! byte-for-byte. `flywheel` and `fw` are byte-identical entries on this
//! one implementation.
//!
//! The verb map is fixed by spec 00021's FR-4:
//!
//! * orchestrator: `init`, `status`, `live`, `history`, `show`, `archive`,
//!   `recover`, `resolve`, `recheck-blocked`, `validate`
//! * core (single-task producer verbs): `interrupt`, `approve`, `reject`,
//!   `say` (surface name) -> `steer` (core verb)
//! * worker: `worker [--once]` -> the git-worktree daemon loop
//! * daemons: `autopilot`, `triage` (and the headless
//!   `autopilot-supervised` run path)
//! * audit: `audit RUN_ID` -> the core audit stream
//! * docs: `docs [TOPIC]` -> the curated operator docs
//!
//! `next`, `orchestrate`, and the core single-task CLI (`run`, `is-done`,
//! `set-model`, ... under `workflow`) are plumbing, not product verbs -- the
//! spec keeps `worker --once` as the only blessed headless drain. They were
//! reachable in Python as `python -m flywheel_orchestrator.workflow` /
//! `python -m flywheel_core.workflow`; this build routes them as hidden verbs
//! (absent from [`USAGE`]) so no surface is lost:
//!
//! * `flywheel next` / `flywheel orchestrate` -> the orchestrator CLI (the
//!   latter through the `orchestrate` runner the shell binds)
//! * `flywheel workflow <run|is-done|interrupt|steer|set-model|approve|reject>`
//!   -> the core single-task CLI
//!
//! The two bindings that live in the worktree crate -- the `worker` verb
//! and the orchestrator's `orchestrate` runner -- are injected through
//! [`ShellBindings`] so the router stays testable without a git checkout;
//! [`default_bindings`] wires the real daemon.

use crate::autopilot_supervisor::AutopilotSupervisor;
use crate::docs;
use crate::tui::{
    self, DAEMON_AUTOPILOT, DAEMON_WORKER, DaemonControl, DaemonError, DaemonFactory,
    DaemonSpawnSpec, DaemonStatus, open_store_bundle, resolve_tasks_dir_for_worker,
};
use crate::worker_supervisor::{DEFAULT_STOP_TIMEOUT, WorkerSupervisor};
use flywheel_container::spawn_container_orphan_reap;
use flywheel_core::audit::{
    AuditArgs, DEFAULT_POLL_INTERVAL, PolicyError as AuditPolicyError, StopFlag, logs_root_for_db,
    resolve_db_path as resolve_audit_db_path, resolve_policy, run_audit,
    validate_flag_combinations,
};
use flywheel_core::harness::{DEFAULT_AGENT_ID, ShutdownSignal};
use flywheel_core::lifecycle::Status;
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_core::workflow::{self as core_workflow, RunStore};
use flywheel_orchestrator::autopilot_run::{
    self, looks_like_negative_number, match_long_option, parse_float_arg,
};
use flywheel_orchestrator::autopilot_supervised_run::{
    self, DEFAULT_MAX_RESPAWNS, DEFAULT_WINDOW_SECONDS,
};
use flywheel_orchestrator::held_out_gate::HeldOutGraderSource;
use flywheel_orchestrator::orchestrate::OrchestratorReport;
use flywheel_orchestrator::policy::{WorkPolicy, resolve_grader_env};
use flywheel_orchestrator::store_factory::build_store;
use flywheel_orchestrator::store_registry::environ_from_process;
use flywheel_orchestrator::supervision_policy::{SupervisionBudget, SupervisionPolicy};
use flywheel_orchestrator::triage_run;
use flywheel_orchestrator::workflow::{
    self as orchestrator_workflow, ArchiveRequest, ArchiveRunner, OrchestrateRequest,
    OrchestrateRunner, Verbs, default_store_registries, load_effective_policy, python_path_str,
    resolve_db_path, store_config_for,
};
use flywheel_worktree::daemon::{
    self, LandingLayout, RunOnceError, RunOnceRequest, arm_signals, build_held_out_source,
    make_agents_resolve_conflict, make_logger, prepare_held_out_root, resolve_landing_base,
    resolve_model, run_once, strategy_name_for, worker_registry, wrap_for_backend,
};
use flywheel_worktree::phase::{ArchiveOptions, archive_completed_phases};
use flywheel_worktree::submit_registry::SubmitBuildRequest;
use flywheel_worktree::submitter::{LandingLedger, Logger};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The program name every router-owned message carries.
pub const PROG: &str = "fw";

/// Verbs delegated verbatim to the orchestrator CLI (the multi-task
/// layer): its parser already owns their flags (`--policy` / `--tasks-dir`
/// / `--db` / `--json` / `--watch`) and exit codes, so the router hands the
/// argv through.
pub const ORCHESTRATOR_VERBS: [&str; 10] = [
    "init",
    "status",
    "live",
    "history",
    "show",
    "archive",
    "recover",
    "resolve",
    "recheck-blocked",
    "validate",
];

/// Producer verbs that enqueue control commands against an existing
/// lifecycle. `say` is the surface alias for core's `steer` verb; the
/// rename happens here so the underlying core CLI is unchanged.
pub const CORE_VERBS: [&str; 3] = ["interrupt", "approve", "reject"];
pub const SAY_VERB: &str = "say";
pub const CORE_STEER_VERB: &str = "steer";

pub const WORKER_VERB: &str = "worker";
pub const AUTOPILOT_VERB: &str = "autopilot";
/// The headless supervised autopilot run path (spec 00070 criterion #5).
/// Reachable only as a verb in this build (the Python reached it as
/// `python -m`), so it is routed but kept out of the operator usage text.
pub const AUTOPILOT_SUPERVISED_VERB: &str = "autopilot-supervised";
pub const TRIAGE_VERB: &str = "triage";
pub const AUDIT_VERB: &str = "audit";
/// Hidden orchestrator plumbing verbs (the Python reached them as
/// `python -m flywheel_orchestrator.workflow next|orchestrate`). Routed but
/// kept out of the operator usage text.
pub const HIDDEN_ORCHESTRATOR_VERBS: [&str; 2] = ["next", "orchestrate"];
/// The hidden core single-task CLI (the Python's
/// `python -m flywheel_core.workflow`): `flywheel workflow run GOAL_OR_FILE`,
/// `is-done`, `set-model`, and the producer verbs under their core names.
pub const WORKFLOW_VERB: &str = "workflow";
pub const DOCS_VERB: &str = "docs";

/// The `fw --help` text, byte-identical to the Python router's.
pub const USAGE: &str = "usage: flywheel [--json] [--help] <verb> [args...]
       fw [--json] [--help] <verb> [args...]

'flywheel' and 'fw' are byte-identical entries on one implementation.
Bare invocation opens the operator console in a TTY; '--json' (or any
invocation whose stdout is not a TTY) prints one JSON snapshot and
exits 0.

Verbs:
  init             scaffold .flywheel/ and a flywheel.toml work policy
  worker [--once]  run the git-worktree daemon loop (or a single drain)
  autopilot        keep the queue full (neverending; --once for one pass)
  triage           triage the intake board (neverending; --once for one pass)
  status [--json]  print every active task's state
  live [--watch N] print one line per in-flight run (optionally tailing)
  history [--json] list finished runs, one line per task, newest first
  show ID [--json] show one run in full (accepts a run_id or task id)
  say RUN_ID MSG   inject an operator message into a live run
  interrupt RUN_ID interrupt a live run
  approve RUN_ID   approve a parked manual-approval gate
  reject RUN_ID    reject a parked manual-approval gate (--feedback TEXT)
  archive          move done phases out of active/
  recover          finalize stranded lifecycles
  resolve ID        abandon a strand (--reason TEXT) so its phase archives
  recheck-blocked  re-evaluate blocked lifecycles' requires
  validate         statically validate active tasks' graders (lint)
  audit RUN_ID     stream the totally-ordered audit records for a run
  docs [TOPIC]     print a curated operator doc (bare: list the topics)

Each verb forwards its own --help to the underlying implementation,
so 'flywheel <verb> --help' (equivalently 'fw <verb> --help') lists
that verb's specific flags.
";

// --- Bindings ----------------------------------------------------------------------

/// The `worker` verb's entry: the git-worktree daemon loop.
pub type WorkerMain = fn(&[String]) -> i32;

/// The two shell bindings that depend on the worktree crate's daemon.
///
/// `worker` is the in-process `flywheel worker` entry; `orchestrate` is
/// the runner the orchestrator's `orchestrate` verb drives (the daemon's
/// single pass). An unbound field prints the deterministic
/// `error: <verb> is not available in this build` and exits 2, so the
/// router can be exercised without the daemon.
#[derive(Clone, Default)]
pub struct ShellBindings {
    pub worker: Option<WorkerMain>,
    pub orchestrate: Option<OrchestrateRunner>,
}

impl std::fmt::Debug for ShellBindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellBindings")
            .field("worker", &self.worker.is_some())
            .field("orchestrate", &self.orchestrate.is_some())
            .finish()
    }
}

impl ShellBindings {
    /// The orchestrator's injected verbs: the bound `orchestrate` runner
    /// plus the worktree crate's `archive` sweep.
    pub fn verbs(&self) -> Verbs {
        Verbs {
            orchestrate: self.orchestrate.clone(),
            archive: Some(archive_runner()),
        }
    }
}

/// The production bindings: `worker` is the worktree daemon's `main`, and
/// `orchestrate` is its single drain pass.
pub fn default_bindings() -> ShellBindings {
    ShellBindings {
        worker: Some(daemon::main),
        orchestrate: Some(orchestrate_runner()),
    }
}

// --- Entry points ------------------------------------------------------------------

/// Route one full process argv (`argv[0]` is the program) with the
/// production bindings; returns the exit code.
pub fn run(argv: Vec<String>) -> i32 {
    run_with(argv, default_bindings())
}

/// [`run`] with injected bindings: the testable seam.
pub fn run_with(argv: Vec<String>, bindings: ShellBindings) -> i32 {
    let args = argv.get(1..).unwrap_or(&[]);
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = route(args, &bindings, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}

/// Route `fw <verb> ...` (`args` excludes the program name) to the
/// pre-existing implementation and return its exit code unchanged.
///
/// The router's own output -- the usage text, the unknown-verb notice, the
/// policy / store errors raised before a delegation, and the `audit` and
/// `docs` verbs -- goes through `out` / `err`; the delegated core,
/// orchestrator, and daemon entries write to the process stdio directly.
/// Unknown verbs exit 2 (argparse convention) with a stderr line pointing
/// at `fw --help`.
/// One-shot, read-only verbs: they spawn no agents or graders, so a closed
/// stdout (`fw status | head`) may end them the way it ends any Unix filter.
const READ_ONLY_VERBS: [&str; 7] = [
    "status", "live", "history", "show", "validate", AUDIT_VERB, DOCS_VERB,
];

/// Restore the default SIGPIPE disposition (the Rust runtime ignores it, so
/// a closed pipe otherwise surfaces as an `error: Broken pipe` line). Only
/// for [`READ_ONLY_VERBS`]: daemons keep ignoring SIGPIPE so a vanished
/// reader can never kill a worker mid-run and orphan its process groups.
fn exit_quietly_on_closed_stdout() {
    #[cfg(unix)]
    // SAFETY: resetting a signal disposition to its default has no
    // preconditions; no handler is installed for SIGPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

pub fn route(
    args: &[String],
    bindings: &ShellBindings,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // Bare `fw` -> the TUI's TTY-vs--json fork already owns that decision;
    // hand it the empty argv so its parser picks defaults.
    let Some(first) = args.first() else {
        return tui::main_with(&[], Some(daemon_factory()));
    };
    if first == "-h" || first == "--help" {
        let _ = out.write_all(USAGE.as_bytes());
        return 0;
    }
    // Top-level flags without a verb (`fw --json`, `fw --db ...`) remain
    // TUI flags -- the TUI's parser already accepts them.
    if first.starts_with('-') {
        return tui::main_with(args, Some(daemon_factory()));
    }
    let verb = first.as_str();
    let rest = &args[1..];
    if READ_ONLY_VERBS.contains(&verb) {
        exit_quietly_on_closed_stdout();
    }

    if ORCHESTRATOR_VERBS.contains(&verb) || HIDDEN_ORCHESTRATOR_VERBS.contains(&verb) {
        return orchestrator_workflow::main(args, &bindings.verbs());
    }
    if verb == WORKFLOW_VERB {
        // The core CLI's own top level (`--help`, or no sub-verb) needs no
        // store; a sub-verb goes through the store-injecting delegate so a
        // postgres [store] policy is honoured exactly as for `fw say`.
        return match rest.first() {
            Some(sub) if !sub.starts_with('-') => delegate_producer(rest.to_vec(), err),
            _ => core_workflow::main(rest),
        };
    }
    if CORE_VERBS.contains(&verb) {
        // Producer verbs enqueue a control command; route through the
        // store-injecting delegate so a postgres [store] policy lands the
        // command in postgres instead of a stray sqlite file.
        return delegate_producer(args.to_vec(), err);
    }
    if verb == SAY_VERB {
        // Surface name rename: the shell's 'say' is core's 'steer' verb.
        let mut core_argv = vec![CORE_STEER_VERB.to_owned()];
        core_argv.extend_from_slice(rest);
        return delegate_producer(core_argv, err);
    }
    if verb == WORKER_VERB {
        // In-process delegation to the git-worktree daemon loop (spec
        // constraint: no shell-out).
        return match bindings.worker {
            Some(worker_main) => worker_main(rest),
            None => {
                let _ = writeln!(err, "error: {WORKER_VERB} is not available in this build");
                2
            }
        };
    }
    if verb == AUTOPILOT_VERB {
        // The autopilot intake daemon: neverending by default; --once runs
        // a single refill pass.
        return autopilot_run::main(rest);
    }
    if verb == AUTOPILOT_SUPERVISED_VERB {
        return autopilot_supervised_run::main(rest);
    }
    if verb == TRIAGE_VERB {
        // The triage intake daemon: neverending by default; --once runs a
        // single triage pass.
        return triage_run::main(rest);
    }
    if verb == AUDIT_VERB {
        return delegate_audit(rest, out, err);
    }
    if verb == DOCS_VERB {
        // Curated operator docs served from the binary -- knowledge, not
        // policy: no flywheel.toml load, no store, works from any directory.
        return docs::run_with(rest, out, err);
    }

    let _ = writeln!(err, "{PROG}: unknown command: {verb}");
    let _ = writeln!(err, "  see '{PROG} --help' for the verb list");
    2
}

// --- Delegates ---------------------------------------------------------------------

/// Forward a producer verb to core, injecting the policy-selected store.
///
/// Core opens a sqlite store at the resolved `--db` by default; under a
/// postgres `[store]` policy that would persist the command to the wrong
/// backend and dual-write a stray sqlite file. So when the effective policy
/// selects a non-sqlite backend, build that store here -- the layer that
/// owns policy and the DSN environment -- and inject it through core's
/// store seam, so the command lands in postgres. A sqlite policy (or no
/// policy) forwards untouched, `--db` semantics and all. A bare `--help`
/// forwards untouched too so help never requires a store. Postgres
/// fail-fast configuration errors print to stderr and exit 2, never a
/// silent sqlite fallback.
fn delegate_producer(core_argv: Vec<String>, err: &mut dyn Write) -> i32 {
    if core_argv.iter().skip(1).any(|a| a == "-h" || a == "--help") {
        return core_workflow::main(&core_argv);
    }
    let policy = match load_effective_policy(None) {
        Ok(policy) => policy,
        Err(failure) => {
            let _ = writeln!(err, "{PROG}: policy error: {failure}");
            return 2;
        }
    };
    let backend = policy
        .as_ref()
        .map_or("sqlite", |p| p.store_backend.as_str());
    if backend == "sqlite" {
        // Byte-identical default: core resolves --db and opens its own
        // sqlite store exactly as before.
        return core_workflow::main(&core_argv);
    }
    let db_path = resolve_db_path(None, policy.as_ref());
    let bundle = match open_store_bundle(policy.as_ref(), &db_path) {
        Ok(bundle) => bundle,
        Err(failure) => {
            let _ = writeln!(err, "{PROG}: {failure}");
            return 2;
        }
    };
    let store: Arc<dyn RunStore> = Arc::new(bundle.core);
    core_workflow::main_with_store(&core_argv, Some(store))
}

/// Forward `audit` to the core audit verb with the product's db resolution.
///
/// Core's audit plumbing defaults `--db` to `$FLYWHEEL_DB` or
/// `./flywheel.db`; every sibling verb resolves the store as `--db` >
/// policy `[paths] db` > `.flywheel/flywheel.sqlite`. Inject the resolved
/// path when the operator did not pass `--db` so `fw audit` reads the same
/// store `fw history` / `fw show` do. An explicit `--db` (or a bare
/// `--help`) forwards untouched.
fn delegate_audit(rest: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let explicit_db = rest.iter().any(|a| a == "--db" || a.starts_with("--db="));
    let help = rest.iter().any(|a| a == "-h" || a == "--help");
    if explicit_db || help {
        return audit_main(rest, out, err);
    }
    let policy = match load_effective_policy(None) {
        Ok(policy) => policy,
        Err(failure) => {
            let _ = writeln!(err, "{PROG}: policy error: {failure}");
            return 2;
        }
    };
    let db_path = resolve_db_path(None, policy.as_ref());
    let mut argv = vec!["--db".to_owned(), db_path.to_string_lossy().into_owned()];
    argv.extend_from_slice(rest);
    audit_main(&argv, out, err)
}

// --- The audit verb's command line --------------------------------------------------

/// The audit parser's program name: the verb as the operator invokes it.
pub const AUDIT_PROG: &str = "flywheel audit";

/// The usage block argparse prints on a bad argument (80-column wrap).
pub const AUDIT_USAGE: &str =
    "usage: flywheel audit [-h] [--db DB] [--logs-root LOGS_ROOT] [--json]
                      [--follow] [--poll-interval POLL_INTERVAL] [--redact]
                      [--redact-policy NAME] [--raw] [--dry-run]
                      [--redact-salt SALT]
                      run_id
";

/// The `-h` text, byte-identical to argparse's.
pub const AUDIT_HELP: &str = "usage: flywheel audit [-h] [--db DB] [--logs-root LOGS_ROOT] [--json]
                      [--follow] [--poll-interval POLL_INTERVAL] [--redact]
                      [--redact-policy NAME] [--raw] [--dry-run]
                      [--redact-salt SALT]
                      run_id

Print the totally-ordered audit stream for one run. Default output is a human-readable one-line-per-record view; --json switches to NDJSON; --follow tails until the lifecycle reaches a terminal status (Ctrl-C exits cleanly with code 130).

Redaction is on by default: payloads pass through a best-effort policy before display so the output is safe to paste into tickets or chats. Use --raw to opt out for authorized forensics. Redaction is best-effort -- unmatched secrets can still pass through, and the store remains the unredacted source of truth.

positional arguments:
  run_id                The run_id whose audit stream to read.

options:
  -h, --help            show this help message and exit
  --db DB               SQLite database path (default: $FLYWHEEL_DB or
                        flywheel.db).
  --logs-root LOGS_ROOT
                        Telemetry logs root holding runs/<run_id>.jsonl
                        (default: the 'logs' directory next to the database).
  --json                Emit one JSON object per record (NDJSON) instead of
                        the text view.
  --follow              Tail the audit stream live; exits when the lifecycle
                        reaches a terminal status (DONE, FAILED,
                        INTERNAL_ERROR, INTERRUPTED).
  --poll-interval POLL_INTERVAL
                        Polling interval for --follow in seconds (default:
                        0.25).
  --redact              Explicitly enable redaction (already on by default).
                        Useful in scripts that want unambiguous intent.
  --redact-policy NAME  Select a redaction policy: 'default' (pattern set +
                        env-value redactor seeded from ANTHROPIC_API_KEY) or
                        'strict' (default + denylist for credential-reading
                        tools Read/Bash). Any other value fails fast before
                        streaming.
  --raw                 Disable redaction and emit verbatim payloads exactly
                        as stored. Use only for authorized forensics on
                        trusted output sinks.
  --dry-run             Stream the run through the selected policy and emit a
                        coverage report (records scanned, records redacted,
                        hits per label) without printing any payload content.
                        Useful for verifying a policy catches the secrets you
                        expect before relying on it.
  --redact-salt SALT    Salt for stable [REDACTED:label:digest] tokens (HMAC-
                        SHA256). Identical secrets render to the same token
                        across records so a consumer can correlate
                        occurrences; the cleartext is never recoverable from a
                        token. Default: no salt -> bare [REDACTED:label]
                        tokens.

Redaction usage (on by default; best-effort -- the store remains the unredacted source of truth):
  --redact                   explicit opt-in (already the default)
  --redact-policy NAME       'default' | 'strict'
  --redact-salt SALT         stable [REDACTED:label:digest] tokens for correlation
  --dry-run                  coverage report only; no payload content printed
  --raw                      verbatim output for authorized forensics (disables redaction)
";

const AUDIT_OPTIONS: [&str; 11] = [
    "--help",
    "--db",
    "--logs-root",
    "--json",
    "--follow",
    "--poll-interval",
    "--redact",
    "--redact-policy",
    "--raw",
    "--dry-run",
    "--redact-salt",
];

/// The parsed `audit` command line: the core options plus the two the
/// shell resolves itself (the store path and the logs root).
#[derive(Debug, Clone)]
pub struct AuditCommand {
    pub args: AuditArgs,
    pub db: Option<String>,
    pub logs_root: Option<String>,
}

/// The outcome of parsing: the command, or the help text to print.
#[derive(Debug, Clone)]
pub enum AuditParsed {
    Command(AuditCommand),
    Help,
}

/// argparse's `--flag VALUE` / `--flag=VALUE` consumption: the inline
/// value, else the next token unless it is an option (a negative number
/// counts as a value).
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

/// Reject an inline value on a store-true flag (`--json=1`).
fn reject_inline(flag: &str, inline: Option<&str>) -> Result<(), String> {
    match inline {
        Some(value) => Err(format!(
            "argument {flag}: ignored explicit argument '{value}'"
        )),
        None => Ok(()),
    }
}

/// Parse the `audit` argv (everything after the verb) as argparse would:
/// every long option accepts the `--flag=value` spelling and a unique-prefix
/// abbreviation, `-h` / `--help` anywhere wins, `--` ends the options, a
/// token no option claims is collected into one `unrecognized arguments:`
/// error reported after the parse, and a missing `run_id` is reported
/// before any extras. The error is argparse's message for the failure.
pub fn parse_audit_args(argv: &[String]) -> Result<AuditParsed, String> {
    let mut run_id: Option<String> = None;
    let mut extras: Vec<String> = Vec::new();
    let mut db: Option<String> = None;
    let mut logs_root: Option<String> = None;
    let mut json = false;
    let mut follow = false;
    let mut poll_interval = DEFAULT_POLL_INTERVAL;
    let mut redact = false;
    let mut redact_policy: Option<String> = None;
    let mut raw = false;
    let mut dry_run = false;
    let mut redact_salt: Option<String> = None;
    let mut only_positionals = false;
    let mut iter = argv.iter();
    while let Some(token) = iter.next() {
        if only_positionals
            || !token.starts_with('-')
            || token == "-"
            || looks_like_negative_number(token)
        {
            if run_id.is_none() {
                run_id = Some(token.clone());
            } else {
                extras.push(token.clone());
            }
            continue;
        }
        if token == "--" {
            only_positionals = true;
            continue;
        }
        if token == "-h" {
            return Ok(AuditParsed::Help);
        }
        if !token.starts_with("--") {
            extras.push(token.clone());
            continue;
        }
        let (flag, inline) = match token.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (token.as_str(), None),
        };
        let Some(option) = match_long_option(token, flag, &AUDIT_OPTIONS)? else {
            extras.push(token.clone());
            continue;
        };
        match option {
            "--help" => return Ok(AuditParsed::Help),
            "--db" => db = Some(take_value(option, inline, &mut iter)?),
            "--logs-root" => logs_root = Some(take_value(option, inline, &mut iter)?),
            "--json" => {
                reject_inline(option, inline)?;
                json = true;
            }
            "--follow" => {
                reject_inline(option, inline)?;
                follow = true;
            }
            "--poll-interval" => {
                let value = take_value(option, inline, &mut iter)?;
                let seconds = parse_float_arg(option, &value)?;
                poll_interval =
                    Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or(DEFAULT_POLL_INTERVAL);
            }
            "--redact" => {
                reject_inline(option, inline)?;
                redact = true;
            }
            "--redact-policy" => redact_policy = Some(take_value(option, inline, &mut iter)?),
            "--raw" => {
                reject_inline(option, inline)?;
                raw = true;
            }
            "--dry-run" => {
                reject_inline(option, inline)?;
                dry_run = true;
            }
            "--redact-salt" => redact_salt = Some(take_value(option, inline, &mut iter)?),
            other => extras.push(other.to_owned()),
        }
    }
    let Some(run_id) = run_id else {
        return Err("the following arguments are required: run_id".to_owned());
    };
    if !extras.is_empty() {
        return Err(format!("unrecognized arguments: {}", extras.join(" ")));
    }
    let mut args = AuditArgs::new(run_id);
    args.json = json;
    args.follow = follow;
    args.poll_interval = poll_interval;
    args.redact = redact;
    args.redact_policy = redact_policy;
    args.raw = raw;
    args.dry_run = dry_run;
    args.redact_salt = redact_salt;
    Ok(AuditParsed::Command(AuditCommand {
        args,
        db,
        logs_root,
    }))
}

/// Arm a Ctrl-C watcher that trips `flag` so a `--follow` stream exits
/// cleanly with code 130 instead of dying mid-record. Best effort: when the
/// watcher cannot be installed, Ctrl-C keeps its default disposition.
fn arm_audit_interrupt(flag: StopFlag) {
    let _ = std::thread::Builder::new()
        .name("audit-interrupt".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    flag.set();
                }
            });
        });
}

/// The `audit` verb: parse, resolve the store and logs root, stream.
///
/// Mirrors the Python CLI's ordering: a disallowed flag combination or an
/// unresolvable `--redact-policy` fails fast (exit 2) before the store is
/// touched; a nonexistent store is an operator error (exit 2) rather than
/// an empty result, since opening it would silently bootstrap an empty
/// database; an unknown run exits 0 with a stderr notice.
pub fn audit_main(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let command = match parse_audit_args(argv) {
        Ok(AuditParsed::Command(command)) => command,
        Ok(AuditParsed::Help) => {
            let _ = out.write_all(AUDIT_HELP.as_bytes());
            return 0;
        }
        Err(message) => {
            let _ = writeln!(err, "{AUDIT_USAGE}{AUDIT_PROG}: error: {message}");
            return 2;
        }
    };
    let mut args = command.args;
    if let Some(message) = validate_flag_combinations(&args) {
        let _ = writeln!(err, "error: {message}");
        return 2;
    }
    if !args.raw
        && let Err(AuditPolicyError(message)) = resolve_policy(&args)
    {
        let _ = writeln!(err, "error: {message}");
        return 2;
    }
    let db_path = resolve_audit_db_path(command.db.as_deref());
    if !db_path.exists() {
        let _ = writeln!(
            err,
            "error: no store at {} (pass --db or set $FLYWHEEL_DB)",
            python_path_str(&db_path)
        );
        return 2;
    }
    let store = match SqliteStore::open(&db_path) {
        Ok(store) => store,
        Err(failure) => {
            let _ = writeln!(err, "error: {failure}");
            return 1;
        }
    };
    let logs_root = command
        .logs_root
        .map_or_else(|| logs_root_for_db(&db_path), PathBuf::from);
    if args.follow {
        let flag = StopFlag::new();
        arm_audit_interrupt(flag.clone());
        args.interrupt = Some(flag);
    }
    match run_audit(&args, &store, &logs_root, out, err) {
        Ok(code) => code,
        Err(failure) => {
            let _ = writeln!(err, "error: {failure}");
            1
        }
    }
}

// --- The orchestrator's injected verbs -----------------------------------------------

/// The `archive` sweep runner: the worktree crate's phase archival with the
/// same gates the worker and the console `/archive` apply (loop-path gate,
/// landed predicate, phase-verify, stop-event markers). Refusals logged by
/// the sweep are written to the verb's sink after the sweep returns.
pub fn archive_runner() -> ArchiveRunner {
    Arc::new(
        |request: ArchiveRequest<'_>, sink: &mut dyn Write| -> Result<Vec<PathBuf>, String> {
            let lines: Arc<Mutex<Vec<String>>> = Arc::default();
            let collector = Arc::clone(&lines);
            let logger: Logger = Arc::new(move |message: &str| {
                collector
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(message.to_owned());
            });
            let options = ArchiveOptions {
                repo_root: Some(&request.repo_root),
                log: Some(&logger),
                phase_verify: request.phase_verify.as_deref(),
                landing_base: Some(&request.landing_base),
                true_base: Some(&request.landing_base),
                claims: Some(request.claims.as_ref()),
                ..ArchiveOptions::default()
            };
            let result = archive_completed_phases(&request.tasks_dir, &request.store, &options)
                .map_err(|failure| failure.to_string());
            for line in lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drain(..)
            {
                let _ = writeln!(sink, "{line}");
            }
            result
        },
    )
}

/// The `orchestrate` runner: one drain pass of the worktree daemon over the
/// verb's resolved request, with the policy-selected landing strategy, the
/// held-out gate, and the sandbox backend wired exactly as `flywheel
/// worker --once` wires them. Prints the Python `[orchestrate]` report
/// lines to stderr; exits 1 when any run ended in a non-done state.
pub fn orchestrate_runner() -> OrchestrateRunner {
    Arc::new(
        |request: OrchestrateRequest<'_>, _out: &mut dyn Write, err: &mut dyn Write| -> i32 {
            run_orchestrate_pass(request, err)
        },
    )
}

fn clamp_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

fn run_orchestrate_pass(request: OrchestrateRequest<'_>, err: &mut dyn Write) -> i32 {
    let repo_root = match daemon::repo_root() {
        Ok(root) => root,
        Err(failure) => {
            let _ = writeln!(err, "{failure}");
            return 1;
        }
    };
    let log = make_logger("[orchestrate]");
    let policy: Option<Arc<WorkPolicy>> = request.policy.cloned().map(Arc::new);
    let phase_base = match resolve_landing_base(&repo_root, policy.as_deref(), Some(&log)) {
        Ok(base) => base,
        Err(failure) => {
            let _ = writeln!(err, "error: {failure}");
            return 2;
        }
    };
    let args = request.args;
    let model = resolve_model(args.model.as_deref(), policy.as_deref());
    let tasks_dir =
        resolve_tasks_dir_for_worker(args.common.tasks_dir.as_deref(), policy.as_deref())
            .unwrap_or_else(|| repo_root.join(".flywheel").join("tasks"));
    let db_path = request.db_path.clone();
    let worktrees_dir = request.sandbox_root.clone();
    let lock_path = repo_root.join(".flywheel").join(".merge.lock");
    let db_parent = db_path
        .parent()
        .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
    if let Err(failure) =
        std::fs::create_dir_all(&worktrees_dir).and_then(|()| std::fs::create_dir_all(&db_parent))
    {
        let _ = writeln!(err, "error: {failure}");
        return 1;
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
    let grader_env = policy
        .as_deref()
        .and_then(|p| resolve_grader_env(&p.sandbox.env, Some(&environ)));
    let registries = default_store_registries();
    let submit_store = match build_store(
        &registries,
        &store_config_for(policy.as_deref(), &db_path),
        &environ,
    ) {
        Ok(store) => store,
        Err(failure) => {
            let _ = writeln!(err, "error: store error: {failure}");
            return 1;
        }
    };
    let held_out_source = build_held_out_source(policy.as_deref(), &repo_root).map(Arc::new);
    if let Some(source) = &held_out_source {
        match prepare_held_out_root(source, &repo_root) {
            Ok(true) => log(&format!(
                "created the conventional held-out root {}",
                source.root.display()
            )),
            Ok(false) => {}
            Err(failure) => {
                let _ = writeln!(err, "error: held-out gate misconfigured: {failure}");
                return 1;
            }
        }
        log(&format!(
            "held-out gate active root={}",
            source.root.display()
        ));
    }
    let held_out: Option<Arc<dyn HeldOutGraderSource>> =
        held_out_source.map(|source| source as Arc<dyn HeldOutGraderSource>);
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
    let strategy_name = strategy_name_for(policy.as_deref()).to_owned();
    let submitter = match worker_registry().build(&strategy_name, &build_request) {
        Ok(strategy) => strategy,
        Err(failure) => {
            let _ = writeln!(err, "error: {failure}");
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
            let _ = writeln!(err, "error: {failure}");
            return 2;
        }
    };
    let layout = LandingLayout {
        repo_root,
        lock_path,
        phase_base,
        log: Arc::clone(&log),
    };
    // Reap any flywheel-owned container a prior killed worker left
    // orphaned, exactly as the daemon does alongside its first cycle; the
    // handle joins the reap when this function returns.
    let _orphan_reap = spawn_container_orphan_reap(Arc::clone(&log));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => Arc::new(runtime),
        Err(failure) => {
            let _ = writeln!(err, "error: {failure}");
            return 1;
        }
    };
    let shutdown = ShutdownSignal::new();
    arm_signals(
        &runtime,
        StopFlag::new(),
        shutdown.clone(),
        Arc::clone(&log),
    );

    let mut pass = RunOnceRequest::new(run_strategy, layout, tasks_dir, db_path, worktrees_dir);
    pass.model = model;
    pass.max_turns = clamp_u32(args.max_turns);
    pass.max_retries = clamp_u32(args.max_retries);
    pass.worker_id = args.worker_id.clone();
    pass.lease_seconds = args.lease_seconds;
    pass.reconcile_seconds = Some(args.reconcile_seconds).filter(|s| *s != 0.0);
    pass.policy = policy;
    pass.held_out_source = held_out;
    pass.shutdown = Some(shutdown);
    pass.runtime = Some(runtime);
    match run_once(pass) {
        Ok(report) => print_orchestrate_report(&report, err),
        Err(RunOnceError::Interrupted(report)) => {
            print_orchestrate_report(&report, err);
            130
        }
        Err(failure) => {
            let _ = writeln!(err, "error: {failure}");
            1
        }
    }
}

/// The Python `_cmd_orchestrate` report: recovered run ids, one line per
/// run, and exit 1 when any run ended in a non-done state so a CI driver
/// can tell the batch did not fully complete.
pub fn print_orchestrate_report(report: &OrchestratorReport, err: &mut dyn Write) -> i32 {
    for run_id in &report.recovered {
        let _ = writeln!(err, "[orchestrate] recovered: {run_id}");
    }
    for record in &report.runs {
        let _ = writeln!(
            err,
            "[orchestrate] {:<6} {} -> {}  ({})",
            record.mode.as_str(),
            record.task_id,
            record.status.as_str(),
            record.run_id
        );
    }
    if report.runs.iter().all(|r| r.status == Status::Done) {
        0
    } else {
        1
    }
}

// --- The console's daemon supervisors --------------------------------------------------

/// The shared crash-loop budget both console supervisors enforce (the same
/// one the headless supervised run uses): a handful of respawns inside a
/// rolling window so a transient death self-heals while a persistent
/// boot-loop is contained. Each supervisor gets its own policy built from
/// this budget so their windows are independent.
pub fn supervision_policy() -> Option<SupervisionPolicy> {
    SupervisionBudget::new(DEFAULT_MAX_RESPAWNS, DEFAULT_WINDOW_SECONDS)
        .ok()
        .map(SupervisionPolicy::new)
}

/// One [`DaemonControl`] fronting both console supervisors, keyed by
/// [`DAEMON_WORKER`] / [`DAEMON_AUTOPILOT`].
pub struct ConsoleDaemons {
    worker: Mutex<WorkerSupervisor>,
    autopilot: Mutex<AutopilotSupervisor>,
}

impl std::fmt::Debug for ConsoleDaemons {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleDaemons").finish_non_exhaustive()
    }
}

impl ConsoleDaemons {
    /// The production supervisors for a resolved console: the worker spawns
    /// against the console's store / tasks dir / model, autopilot beside it
    /// (never auto-spawned -- it writes to the base branch unattended).
    pub fn new(spec: &DaemonSpawnSpec) -> Self {
        let worker = WorkerSupervisor::new(&spec.db_path)
            .with_tasks_dir(spec.tasks_dir.clone())
            .with_model(spec.model.clone());
        let worker = match supervision_policy() {
            Some(policy) => worker.with_policy(policy),
            None => worker,
        };
        let autopilot = AutopilotSupervisor::new()
            .with_tasks_dir(spec.tasks_dir.clone())
            .with_model(spec.model.clone());
        let autopilot = match supervision_policy() {
            Some(policy) => autopilot.with_policy(policy),
            None => autopilot,
        };
        Self::with_supervisors(worker, autopilot)
    }

    /// A control over explicit supervisors.
    pub fn with_supervisors(worker: WorkerSupervisor, autopilot: AutopilotSupervisor) -> Self {
        Self {
            worker: Mutex::new(worker),
            autopilot: Mutex::new(autopilot),
        }
    }

    fn worker(&self) -> std::sync::MutexGuard<'_, WorkerSupervisor> {
        self.worker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn autopilot(&self) -> std::sync::MutexGuard<'_, AutopilotSupervisor> {
        self.autopilot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl DaemonControl for ConsoleDaemons {
    fn status(&self, name: &str) -> Result<DaemonStatus, DaemonError> {
        match name {
            DAEMON_WORKER => self.worker().status().map_err(DaemonError::Supervisor),
            DAEMON_AUTOPILOT => Ok(self.autopilot().status()),
            other => Err(DaemonError::Unknown(other.to_owned())),
        }
    }

    fn start(&mut self, name: &str) -> Result<DaemonStatus, DaemonError> {
        match name {
            DAEMON_WORKER => self.worker().start().map_err(DaemonError::Supervisor),
            DAEMON_AUTOPILOT => Ok(self.autopilot().start()),
            other => Err(DaemonError::Unknown(other.to_owned())),
        }
    }

    fn stop(&mut self, name: &str) -> Result<bool, DaemonError> {
        match name {
            DAEMON_WORKER => Ok(self.worker().stop(DEFAULT_STOP_TIMEOUT)),
            DAEMON_AUTOPILOT => Ok(self.autopilot().stop(DEFAULT_STOP_TIMEOUT)),
            other => Err(DaemonError::Unknown(other.to_owned())),
        }
    }

    fn detach(&mut self, name: &str) {
        match name {
            DAEMON_WORKER => self.worker().detach(),
            DAEMON_AUTOPILOT => self.autopilot().detach(),
            _ => {}
        }
    }

    fn death_reason(&self, name: &str) -> Option<String> {
        match name {
            DAEMON_WORKER => self.worker().death_reason(),
            DAEMON_AUTOPILOT => self.autopilot().death_reason(),
            _ => None,
        }
    }
}

/// The factory the console takes: builds [`ConsoleDaemons`] for the spawn
/// spec it resolved.
pub fn daemon_factory() -> DaemonFactory {
    Box::new(|spec: &DaemonSpawnSpec| Box::new(ConsoleDaemons::new(spec)) as Box<dyn DaemonControl>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::DaemonState;
    use crate::worker_supervisor::test_support::sleep_argv;
    use chrono::Utc;
    use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome};
    use flywheel_core::store::{AttemptStore, ControlCommandStore, LifecycleStore};
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// Serializes the tests that change the process cwd.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_owned()).collect()
    }

    fn run(parts: &[&str], bindings: &ShellBindings) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = route(&argv(parts), bindings, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn unbound() -> ShellBindings {
        ShellBindings::default()
    }

    fn fresh_store(path: &Path) {
        SqliteStore::open(path).unwrap();
    }

    fn seed(db: &Path, task_id: &str, run_id: &str, statuses: &[Status]) {
        let store = SqliteStore::open(db).unwrap();
        let now = Utc::now();
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        for status in statuses {
            lc.transition_to(*status, "", Some(now)).unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
    }

    fn seed_running(db: &Path, task_id: &str) -> String {
        let run_id = format!("run-{task_id}-running");
        seed(db, task_id, &run_id, &[Status::Ready, Status::Running]);
        run_id
    }

    /// A run parked the way the harness parks one: at `awaiting_approval`
    /// on manual gate ordinal 1, after a succeeded attempt 1.
    fn seed_awaiting(db: &Path, task_id: &str) -> String {
        let run_id = format!("run-{task_id}-awaiting");
        let store = SqliteStore::open(db).unwrap();
        let now = Utc::now();
        let mut lc = Lifecycle::new(task_id).with_run_id(&run_id);
        for status in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::AwaitingApproval,
        ] {
            lc.transition_to(status, "", Some(now)).unwrap();
        }
        lc.awaiting_manual_ordinal = Some(1);
        store.create_lifecycle(&lc).unwrap();
        let mut attempt = Attempt::new(1, now, &run_id);
        attempt.outcome = Some(Outcome::Succeeded);
        store.save_attempt(&run_id, &attempt, None).unwrap();
        run_id
    }

    fn claimed(db: &Path, run_id: &str) -> Vec<(String, serde_json::Value)> {
        let store = SqliteStore::open(db).unwrap();
        store
            .claim_commands(run_id, Utc::now())
            .unwrap()
            .into_iter()
            .map(|c| (c.kind, serde_json::Value::Object(c.payload)))
            .collect()
    }

    // --- top-level surface ---------------------------------------------------

    #[test]
    fn help_prints_the_verb_list_and_exits_zero() {
        let (code, out, err) = run(&["--help"], &unbound());
        assert_eq!(code, 0);
        assert_eq!(out, USAGE);
        assert_eq!(err, "");
        for verb in [
            "init",
            "worker",
            "autopilot",
            "triage",
            "status",
            "live",
            "history",
            "show",
            "say",
            "interrupt",
            "approve",
            "reject",
            "archive",
            "recover",
            "resolve",
            "recheck-blocked",
            "validate",
            "audit",
            "docs",
        ] {
            assert!(out.contains(&format!("\n  {verb}")), "verb {verb} missing");
        }
        assert_eq!(run(&["-h"], &unbound()).0, 0);
    }

    #[test]
    fn unknown_verb_exits_two_pointing_at_help() {
        let (code, out, err) = run(&["does-not-exist"], &unbound());
        assert_eq!(code, 2);
        assert_eq!(out, "");
        assert_eq!(
            err,
            "fw: unknown command: does-not-exist\n  see 'fw --help' for the verb list\n"
        );
    }

    #[test]
    fn run_with_strips_the_program_name() {
        assert_eq!(run_with(argv(&["fw", "--help"]), unbound()), 0);
        assert_eq!(run_with(argv(&["flywheel", "nope"]), unbound()), 2);
        assert_eq!(
            run_with(argv(&["fw", "docs", "no-such-topic"]), unbound()),
            2
        );
    }

    #[test]
    fn json_flag_emits_a_snapshot_without_the_supervisors() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        seed_running(&db, "beta");
        let (code, _out, err) = run(&["--json", "--db", db.to_str().unwrap()], &unbound());
        assert_eq!(code, 0, "stderr: {err}");
    }

    // --- bindings --------------------------------------------------------------

    #[test]
    fn worker_is_unavailable_when_unbound_and_delegates_when_bound() {
        let (code, _out, err) = run(&["worker", "--once"], &unbound());
        assert_eq!(code, 2);
        assert_eq!(err, "error: worker is not available in this build\n");
        assert_eq!(run(&["worker", "--help"], &default_bindings()).0, 0);
    }

    #[test]
    fn default_bindings_bind_both_seams() {
        let bindings = default_bindings();
        assert!(bindings.worker.is_some());
        assert!(bindings.orchestrate.is_some());
        let verbs = bindings.verbs();
        assert!(verbs.orchestrate.is_some());
        assert!(verbs.archive.is_some());
        assert_eq!(
            format!("{bindings:?}"),
            "ShellBindings { worker: true, orchestrate: true }"
        );
    }

    #[test]
    fn hidden_plumbing_verbs_route_but_stay_out_of_usage() {
        for verb in ["next", "orchestrate", "workflow"] {
            assert!(!USAGE.contains(&format!("\n  {verb}")), "{verb} leaked");
        }
        assert_eq!(run(&["next", "--help"], &unbound()).0, 0);
        assert_eq!(run(&["orchestrate", "--help"], &unbound()).0, 0);
        assert_eq!(run(&["workflow", "--help"], &unbound()).0, 0);
        for sub in ["run", "is-done", "set-model", "steer"] {
            assert_eq!(run(&["workflow", sub, "--help"], &unbound()).0, 0, "{sub}");
        }
        // Bare `workflow` is the core parser's own missing-sub-verb error.
        assert_eq!(run(&["workflow"], &unbound()).0, 2);

        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        let tasks = tmp.path().join("tasks");
        fs::create_dir_all(tasks.join("active")).unwrap();
        fresh_store(&db);
        let db_str = db.to_str().unwrap();
        assert_eq!(
            run(
                &[
                    "next",
                    "--db",
                    db_str,
                    "--tasks-dir",
                    tasks.to_str().unwrap()
                ],
                &unbound()
            )
            .0,
            1,
            "an empty queue has no next task"
        );
        // `workflow set-model` enqueues a set_model control command.
        let run_id = seed_running(&db, "gamma");
        assert_eq!(
            run(
                &["workflow", "set-model", "--db", db_str, &run_id, "claude-x"],
                &unbound()
            )
            .0,
            0
        );
        assert_eq!(
            claimed(&db, &run_id),
            vec![(
                "set_model".to_owned(),
                serde_json::json!({"model": "claude-x"})
            )]
        );
        // `workflow is-done` on a task with no done lifecycle exits 1.
        let task_file = tmp.path().join("t.json");
        fs::write(
            &task_file,
            r#"{"id": "never-run", "goal": "g", "graders": [{"type": "command", "run": "true"}]}"#,
        )
        .unwrap();
        assert_eq!(
            run(
                &[
                    "workflow",
                    "is-done",
                    "--db",
                    db_str,
                    task_file.to_str().unwrap()
                ],
                &unbound()
            )
            .0,
            1
        );
    }

    #[test]
    fn orchestrate_reports_unavailable_when_unbound() {
        let (code, _out, _err) = run(&["orchestrate"], &unbound());
        assert_eq!(code, 2);
    }

    // --- orchestrator-routed verbs --------------------------------------------

    #[test]
    fn status_live_history_and_show_delegate_to_the_orchestrator() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        let tasks = tmp.path().join("tasks");
        fs::create_dir_all(tasks.join("active")).unwrap();
        fresh_store(&db);
        let db = db.to_str().unwrap();
        let tasks = tasks.to_str().unwrap();
        assert_eq!(
            run(
                &["status", "--json", "--db", db, "--tasks-dir", tasks],
                &unbound()
            )
            .0,
            0
        );
        assert_eq!(run(&["live", "--db", db], &unbound()).0, 0);
        assert_eq!(
            run(&["history", "--db", db, "--tasks-dir", tasks], &unbound()).0,
            0
        );
        assert_eq!(
            run(
                &["show", "nope", "--db", db, "--tasks-dir", tasks],
                &unbound()
            )
            .0,
            1
        );
        assert_eq!(run(&["status", "--help"], &unbound()).0, 0);
    }

    // --- core-routed producer verbs -------------------------------------------

    #[test]
    fn interrupt_enqueues_kind_interrupt() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        let run_id = seed_running(&db, "task-i");
        assert_eq!(
            run(
                &["interrupt", &run_id, "--db", db.to_str().unwrap()],
                &unbound()
            )
            .0,
            0
        );
        let rows = claimed(&db, &run_id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "interrupt");
    }

    #[test]
    fn say_aliases_to_steer_and_persists_the_text() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        let run_id = seed_running(&db, "task-s");
        let message = "double-check the rubric finding";
        assert_eq!(
            run(
                &["say", &run_id, message, "--db", db.to_str().unwrap()],
                &unbound()
            )
            .0,
            0
        );
        let rows = claimed(&db, &run_id);
        assert_eq!(rows, vec![("say".to_owned(), json!({"text": message}))]);
    }

    #[test]
    fn approve_and_reject_refuse_a_run_still_validating() {
        // Nothing is parked yet: a decision now would be about a gate nobody
        // has seen, so it is refused and never queued.
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        let run_id = "run-task-v-validating";
        seed(
            &db,
            "task-v",
            run_id,
            &[Status::Ready, Status::Running, Status::Validating],
        );
        let db_arg = db.to_str().unwrap();
        for verb in [
            &["approve", run_id, "--db", db_arg][..],
            &["reject", run_id, "--feedback", "no", "--db", db_arg][..],
        ] {
            assert_eq!(run(verb, &unbound()).0, 1, "{verb:?}");
        }
        assert!(claimed(&db, run_id).is_empty());
    }

    #[test]
    fn approve_and_reject_enqueue_against_a_parked_gate() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        let approve_run = seed_awaiting(&db, "task-a");
        assert_eq!(
            run(
                &["approve", &approve_run, "--db", db.to_str().unwrap()],
                &unbound()
            )
            .0,
            0
        );
        assert_eq!(claimed(&db, &approve_run)[0].0, "approve");

        let reject_run = seed_awaiting(&db, "task-r");
        let feedback = "rollback strategy is missing";
        assert_eq!(
            run(
                &[
                    "reject",
                    &reject_run,
                    "--feedback",
                    feedback,
                    "--db",
                    db.to_str().unwrap()
                ],
                &unbound()
            )
            .0,
            0
        );
        assert_eq!(
            claimed(&db, &reject_run),
            vec![(
                "reject".to_owned(),
                json!({"feedback": feedback, "awaiting_ordinal": 1, "attempt_number": 1})
            )]
        );
        assert_eq!(run(&["approve", "--help"], &unbound()).0, 0);
    }

    // --- audit ------------------------------------------------------------------

    #[test]
    fn audit_unknown_run_exits_zero_with_a_stderr_notice() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db.sqlite");
        fresh_store(&db);
        let (code, out, err) = run(
            &["audit", "run-ghost", "--db", db.to_str().unwrap()],
            &unbound(),
        );
        assert_eq!(code, 0);
        assert_eq!(out, "");
        assert_eq!(err, "no records for run_id run-ghost\n");
    }

    #[test]
    fn audit_resolves_the_db_from_the_policy_and_fails_fast_on_a_missing_store() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let tmp = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let restore = |previous: &Path| std::env::set_current_dir(previous).unwrap();

        // No policy, no store: the injected default path does not exist.
        let (code, _out, err) = run(&["audit", "run-ghost"], &unbound());
        assert_eq!(code, 2);
        assert_eq!(
            err,
            "error: no store at .flywheel/flywheel.sqlite (pass --db or set $FLYWHEEL_DB)\n"
        );
        assert!(!tmp.path().join("flywheel.db").exists());

        // A policy naming [paths] db: that store is read, core's default
        // path is neither consulted nor created.
        let db = tmp.path().join("custom").join("store.sqlite");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fresh_store(&db);
        fs::write(
            tmp.path().join("flywheel.toml"),
            "[source]\nkind = \"directory\"\n[paths]\ndb = \"custom/store.sqlite\"\n",
        )
        .unwrap();
        let (code, _out, err) = run(&["audit", "run-ghost"], &unbound());
        restore(&previous);
        assert_eq!(code, 0);
        assert_eq!(err, "no records for run_id run-ghost\n");
        assert!(!tmp.path().join("flywheel.db").exists());
    }

    #[test]
    fn audit_help_and_argparse_errors_are_byte_identical() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(audit_main(&argv(&["--help"]), &mut out, &mut err), 0);
        assert_eq!(String::from_utf8(out).unwrap(), AUDIT_HELP);
        assert!(err.is_empty());
        assert_eq!(
            audit_main(&argv(&["run-1", "-h"]), &mut Vec::new(), &mut Vec::new()),
            0
        );

        let error = |parts: &[&str]| -> String {
            let mut out = Vec::new();
            let mut err = Vec::new();
            assert_eq!(audit_main(&argv(parts), &mut out, &mut err), 2);
            assert!(out.is_empty());
            String::from_utf8(err).unwrap()
        };
        let expect = |message: &str| format!("{AUDIT_USAGE}{AUDIT_PROG}: error: {message}\n");
        assert_eq!(
            error(&[]),
            expect("the following arguments are required: run_id")
        );
        assert_eq!(
            error(&["-x"]),
            expect("the following arguments are required: run_id")
        );
        assert_eq!(
            error(&["run-1", "--bogus"]),
            expect("unrecognized arguments: --bogus")
        );
        assert_eq!(
            error(&["run-1", "extra"]),
            expect("unrecognized arguments: extra")
        );
        assert_eq!(
            error(&["run-1", "--poll-interval", "abc"]),
            expect("argument --poll-interval: invalid float value: 'abc'")
        );
        assert_eq!(
            error(&["run-1", "--db"]),
            expect("argument --db: expected one argument")
        );
        assert_eq!(
            error(&["run-1", "--redact-salt"]),
            expect("argument --redact-salt: expected one argument")
        );
        assert_eq!(
            error(&["run-1", "--json=1"]),
            expect("argument --json: ignored explicit argument '1'")
        );
        assert_eq!(
            error(&["run-1", "--re"]),
            expect("ambiguous option: --re could match --redact, --redact-policy, --redact-salt")
        );
        assert_eq!(
            error(&["run-1", "--raw", "--dry-run"]),
            "error: --raw cannot be combined with --dry-run (redaction is either fully off or \
             fully on)\n"
        );
        assert_eq!(
            error(&["run-1", "--redact-pol", "x", "--js"]),
            "error: unknown built-in policy 'x'; expected one of ['default', 'strict'] or a \
             registered custom policy\n"
        );
        assert_eq!(
            error(&["run-1", "--redact-policy", "pkg.mod:factory"]),
            "error: --redact-policy 'pkg.mod:factory': dotted-path policy factories \
             ('module.sub:factory') are not supported; expected one of ['default', 'strict']\n"
        );
        assert!(!AUDIT_HELP.contains("module.sub"));
        assert!(!AUDIT_HELP.contains("DOTTED"));
        assert!(AUDIT_HELP.starts_with("usage: flywheel audit [-h]"));
    }

    #[test]
    fn audit_parser_accepts_abbreviations_inline_values_and_dashdash() {
        let parsed = parse_audit_args(&argv(&[
            "--js",
            "--poll-interval=1.5",
            "--follow",
            "--db=/tmp/x.sqlite",
            "--logs-root",
            "/tmp/logs",
            "--redact-policy",
            "strict",
            "--redact-salt=pepper",
            "--",
            "-run-1",
        ]))
        .unwrap();
        let AuditParsed::Command(command) = parsed else {
            panic!("expected a command");
        };
        assert_eq!(command.args.run_id, "-run-1");
        assert!(command.args.json);
        assert!(command.args.follow);
        assert_eq!(command.args.poll_interval, Duration::from_millis(1500));
        assert_eq!(command.db.as_deref(), Some("/tmp/x.sqlite"));
        assert_eq!(command.logs_root.as_deref(), Some("/tmp/logs"));
        assert_eq!(command.args.redact_policy.as_deref(), Some("strict"));
        assert_eq!(command.args.redact_salt.as_deref(), Some("pepper"));
        assert!(!command.args.raw && !command.args.dry_run && !command.args.redact);

        let AuditParsed::Command(negative) =
            parse_audit_args(&argv(&["-1", "--poll-interval", "-2"])).unwrap()
        else {
            panic!("expected a command");
        };
        assert_eq!(negative.args.run_id, "-1");
        assert_eq!(negative.args.poll_interval, Duration::ZERO);
    }

    #[test]
    fn audit_with_an_explicit_missing_db_exits_two_without_touching_the_policy() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope").join("db.sqlite");
        let (code, _out, err) = run(
            &["audit", "run-1", "--db", missing.to_str().unwrap()],
            &unbound(),
        );
        assert_eq!(code, 2);
        assert_eq!(
            err,
            format!(
                "error: no store at {} (pass --db or set $FLYWHEEL_DB)\n",
                missing.display()
            )
        );
        assert!(!missing.exists());
        assert_eq!(run(&["audit", "--help"], &unbound()).0, 0);
    }

    // --- daemons ------------------------------------------------------------------

    #[test]
    fn autopilot_and_triage_forward_help_to_their_parsers() {
        assert_eq!(run(&["autopilot", "--help"], &unbound()).0, 0);
        assert_eq!(run(&["triage", "--help"], &unbound()).0, 0);
        assert_eq!(run(&["autopilot-supervised", "--help"], &unbound()).0, 0);
    }

    // --- docs -----------------------------------------------------------------------

    #[test]
    fn docs_lists_topics_and_rejects_an_unknown_one() {
        let (code, out, err) = run(&["docs"], &unbound());
        assert_eq!(code, 0);
        assert!(!out.is_empty());
        assert_eq!(err, "");
        let (code, out, err) = run(&["docs", "no-such-topic"], &unbound());
        assert_eq!(code, 2);
        assert_eq!(out, "");
        assert!(err.contains("no-such-topic"));
    }

    // --- console daemons --------------------------------------------------------------

    fn console(tmp: &TempDir) -> ConsoleDaemons {
        let worker = WorkerSupervisor::new(tmp.path().join("absent.sqlite"))
            .with_log_dir(tmp.path().join("worker"))
            .with_spawn_argv(sleep_argv(60));
        let autopilot = AutopilotSupervisor::new()
            .with_log_dir(tmp.path().join("autopilot"))
            .with_spawn_argv(sleep_argv(60));
        ConsoleDaemons::with_supervisors(worker, autopilot)
    }

    #[test]
    fn console_daemons_route_by_name_and_reject_unknown_names() {
        let tmp = TempDir::new().unwrap();
        let mut control = console(&tmp);
        assert_eq!(
            control.status("other"),
            Err(DaemonError::Unknown("other".to_owned()))
        );
        assert_eq!(
            control.start("other"),
            Err(DaemonError::Unknown("other".to_owned()))
        );
        assert_eq!(
            control.stop("other"),
            Err(DaemonError::Unknown("other".to_owned()))
        );
        assert_eq!(control.death_reason("other"), None);
        control.detach("other");

        assert_eq!(
            control.status(DAEMON_WORKER).unwrap().state,
            DaemonState::None
        );
        assert_eq!(
            control.status(DAEMON_AUTOPILOT).unwrap().state,
            DaemonState::None
        );
        assert_eq!(control.stop(DAEMON_WORKER), Ok(false));
        assert_eq!(control.stop(DAEMON_AUTOPILOT), Ok(false));
    }

    #[test]
    fn console_daemons_supervise_both_children_independently() {
        let tmp = TempDir::new().unwrap();
        let mut control = console(&tmp);
        let worker = control.start(DAEMON_WORKER).unwrap();
        assert_eq!(worker.state, DaemonState::Supervised);
        assert_eq!(
            control.status(DAEMON_AUTOPILOT).unwrap().state,
            DaemonState::None,
            "starting the worker never touches autopilot"
        );
        let autopilot = control.start(DAEMON_AUTOPILOT).unwrap();
        assert_eq!(autopilot.state, DaemonState::Supervised);
        assert_ne!(worker.pid, autopilot.pid);

        assert_eq!(control.stop(DAEMON_AUTOPILOT), Ok(true));
        assert_eq!(
            control.status(DAEMON_WORKER).unwrap().state,
            DaemonState::Supervised,
            "stopping autopilot never touches the worker"
        );
        assert_eq!(
            control.status(DAEMON_AUTOPILOT).unwrap().state,
            DaemonState::Dead
        );
        assert_eq!(
            control.death_reason(DAEMON_AUTOPILOT).as_deref(),
            Some("exit=-15")
        );
        assert_eq!(control.stop(DAEMON_WORKER), Ok(true));
        assert_eq!(
            control.death_reason(DAEMON_WORKER).as_deref(),
            Some("exit=-15")
        );
        control.detach(DAEMON_WORKER);
        assert_eq!(
            control.status(DAEMON_WORKER).unwrap().state,
            DaemonState::None
        );
    }

    #[test]
    fn production_factory_builds_supervisors_for_the_spec() {
        let tmp = TempDir::new().unwrap();
        let spec = DaemonSpawnSpec {
            db_path: tmp.path().join("db.sqlite"),
            tasks_dir: Some(tmp.path().join("tasks")),
            model: Some("claude-opus-4".to_owned()),
            auto_start_worker: false,
        };
        let control = daemon_factory()(&spec);
        assert_eq!(
            control.status(DAEMON_WORKER).unwrap().state,
            DaemonState::None
        );
        assert_eq!(
            control.status(DAEMON_AUTOPILOT).unwrap().state,
            DaemonState::None
        );
        assert!(supervision_policy().is_some());
    }

    #[test]
    fn orchestrate_report_prints_the_python_lines_and_exit_code() {
        use flywheel_orchestrator::orchestrate::{RunMode, RunRecord};
        let report = OrchestratorReport {
            worker_id: "w".to_owned(),
            recovered: vec!["run-old".to_owned()],
            runs: vec![
                RunRecord {
                    task_id: "alpha".to_owned(),
                    run_id: "run-a".to_owned(),
                    status: Status::Done,
                    mode: RunMode::Fresh,
                    worker_id: "w".to_owned(),
                    gate: None,
                    gate_reason: String::new(),
                },
                RunRecord {
                    task_id: "beta".to_owned(),
                    run_id: "run-b".to_owned(),
                    status: Status::Failed,
                    mode: RunMode::Resume,
                    worker_id: "w".to_owned(),
                    gate: None,
                    gate_reason: String::new(),
                },
            ],
        };
        let mut err = Vec::new();
        assert_eq!(print_orchestrate_report(&report, &mut err), 1);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "[orchestrate] recovered: run-old\n[orchestrate] fresh  alpha -> done  (run-a)\n\
             [orchestrate] resume beta -> failed  (run-b)\n"
        );
        let done_only = OrchestratorReport {
            runs: vec![report.runs[0].clone()],
            ..report
        };
        assert_eq!(print_orchestrate_report(&done_only, &mut Vec::new()), 0);
    }
}
