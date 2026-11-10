//! `fw reland TASK_ID [--db PATH]`: one operator-initiated landing attempt
//! for a parked task's latest DONE run (spec 00086, D-1/D-7).
//!
//! The verb assembles the same landing inputs the worker does -- the
//! policy-selected submit strategy, the held-out gate source, the store --
//! and hands them to the orchestrator's [`reland`] entry point, which owns
//! the refusals, the claim, the gate, and the submit. This module only adds
//! the git workspace seam ([`GitRelandWorkspace`]): a missing task branch
//! refuses, a pruned worktree is re-materialized from the surviving branch.
//!
//! Exit codes: 0 landed (prints the landed ref), 1 attempted and not landed
//! (prints `park_kind` and detail) or refused (prints why; nothing recorded,
//! nothing claimed), 2 usage.

use crate::daemon::{
    build_held_out_source, make_agents_resolve_conflict, make_logger, repo_root,
    resolve_landing_base, strategy_name_for, worker_registry,
};
use crate::submit_registry::SubmitBuildRequest;
use crate::submitter::{GitWorktreeSubmitter, LandingLedger, build_merge_submitter, git};
use flywheel_core::deadline_config::DeadlineClass;
use flywheel_core::harness::DEFAULT_AGENT_ID;
use flywheel_core::invoker_client::Clock;
use flywheel_orchestrator::held_out_gate::{GateOptions, HeldOutGraderSource};
use flywheel_orchestrator::orchestrate::{
    RelandContext, RelandOutcome, RelandWorkspace, harness_config_from_policy, reland,
    sandbox_agent_primitives,
};
use flywheel_orchestrator::policy::{WorkPolicy, resolve_grader_env, resolve_sandbox_root};
use flywheel_orchestrator::sources::{DirectoryWorkSource, WorkSource};
use flywheel_orchestrator::store_factory::build_store;
use flywheel_orchestrator::store_registry::environ_from_process;
use flywheel_orchestrator::strategy::SubmitRequest;
use flywheel_orchestrator::workflow::args::DEFAULT_LEASE_SECONDS;
use flywheel_orchestrator::workflow::{
    Access, default_store_registries, load_effective_policy, open_stores_for, store_config_for,
};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

/// The verb name the router dispatches on.
pub const RELAND_VERB: &str = "reland";

/// `fw reland --help`.
pub const RELAND_USAGE: &str = "usage: fw reland TASK_ID [--db PATH]

Make one landing attempt for TASK_ID's latest DONE run whose landing
parked, on the task branch's CURRENT head (commits added after the park
included), through the configured submit strategy: the held-out gate, the
rebase onto the current base with the task's command graders re-run,
`verify`, protected paths, the harness-owned Flywheel-* trailers, and the
project merge lock. A pruned worktree is rebuilt from the task branch.

arguments:
  TASK_ID      the task whose parked landing to retry

options:
  --db PATH    the sqlite store (default: the project's store)
  -h, --help   show this help

exit codes:
  0  landed (prints the landed ref)
  1  attempted and parked (prints park_kind and detail), or refused before
     attempting (prints why; nothing recorded, nothing claimed): no DONE
     run, never parked, already landed, resolved after its latest park,
     the claim is held by another worker, or the task branch is gone
  2  usage
";

const PROG: &str = "fw reland";

/// The parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelandArgs {
    pub task_id: String,
    pub db: Option<PathBuf>,
}

/// What the parser produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelandParsed {
    Args(RelandArgs),
    Help,
}

/// Parse `fw reland` argv (the verb itself excluded). An `Err` is a usage
/// error message.
pub fn parse_args(argv: &[String]) -> Result<RelandParsed, String> {
    let mut task_id: Option<String> = None;
    let mut db: Option<PathBuf> = None;
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(RelandParsed::Help),
            "--db" => match iter.next() {
                Some(value) if !value.is_empty() => db = Some(PathBuf::from(value)),
                _ => return Err("argument --db: expected one argument".to_owned()),
            },
            "--" => {
                for rest in iter.by_ref() {
                    set_task_id(&mut task_id, rest)?;
                }
            }
            other => {
                if let Some(value) = other.strip_prefix("--db=") {
                    if value.is_empty() {
                        return Err("argument --db: expected one argument".to_owned());
                    }
                    db = Some(PathBuf::from(value));
                } else if other.starts_with('-') && other.len() > 1 {
                    return Err(format!("unrecognized argument: {other}"));
                } else {
                    set_task_id(&mut task_id, other)?;
                }
            }
        }
    }
    let task_id =
        task_id.ok_or_else(|| "the following arguments are required: TASK_ID".to_owned())?;
    Ok(RelandParsed::Args(RelandArgs { task_id, db }))
}

fn set_task_id(slot: &mut Option<String>, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("TASK_ID must not be empty".to_owned());
    }
    if slot.is_some() {
        return Err(format!("unrecognized argument: {value}"));
    }
    *slot = Some(value.to_owned());
    Ok(())
}

/// The git workspace seam: the run's task branch must still exist, and the
/// sandbox the strategy lands from (`<worktrees>/<task-id>`) must hold that
/// branch -- re-materialized from the branch head when retention pruned it.
pub struct GitRelandWorkspace<'a> {
    pub submitter: &'a GitWorktreeSubmitter,
}

impl GitRelandWorkspace<'_> {
    fn branch_of(&self, request: &SubmitRequest) -> String {
        let phase = self.submitter.phase_of(request.task_file.as_deref());
        self.submitter.branch(&request.task_id, &phase)
    }
}

impl RelandWorkspace for GitRelandWorkspace<'_> {
    fn preflight(&self, request: &SubmitRequest) -> Result<(), String> {
        let branch = self.branch_of(request);
        match self.submitter.branch_exists(&branch) {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!(
                "task branch {branch} no longer exists; there is nothing to land"
            )),
            Err(err) => Err(format!("cannot inspect task branch {branch}: {err}")),
        }
    }

    fn materialize(&self, request: &SubmitRequest) -> Result<(), String> {
        let branch = self.branch_of(request);
        let worktree = self.submitter.worktree(&request.task_id);
        let repo_root = &self.submitter.repo_root;
        if worktree.is_dir() {
            let registered = self
                .submitter
                .is_registered_worktree(&worktree)
                .map_err(|err| err.to_string())?;
            if !registered {
                return Err(format!(
                    "{} exists but is not a registered worktree; refusing to clobber",
                    worktree.display()
                ));
            }
            let head = git(&worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                .map_err(|err| err.to_string())?;
            if !head.ok() || head.trimmed() != branch {
                return Err(format!(
                    "{} is not checked out on {branch}; refusing to land from it",
                    worktree.display()
                ));
            }
            return Ok(());
        }
        // Retention removed the directory (D-7): drop its stale
        // registration, then check the surviving branch out where the
        // strategy lands from. The branch is not rebased here -- submit
        // takes its own rebase + re-verify path.
        let pruned = git(repo_root, &["worktree", "prune"]).map_err(|err| err.to_string())?;
        if !pruned.ok() {
            return Err(format!(
                "git worktree prune failed: {}",
                pruned.stderr.trim()
            ));
        }
        let path = worktree.to_string_lossy().into_owned();
        let added =
            git(repo_root, &["worktree", "add", &path, &branch]).map_err(|err| err.to_string())?;
        if !added.ok() {
            return Err(format!(
                "cannot rebuild the worktree for {branch} at {}: {}",
                worktree.display(),
                added.stderr.trim()
            ));
        }
        self.submitter
            .set_commit_identity(&worktree)
            .map_err(|err| err.to_string())?;
        (self.submitter.log)(&format!(
            "rebuilt the pruned worktree {} from {branch}",
            worktree.display()
        ));
        Ok(())
    }
}

/// `fw reland` entry with injected stdout/stderr.
pub fn main_with(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let args = match parse_args(argv) {
        Ok(RelandParsed::Args(args)) => args,
        Ok(RelandParsed::Help) => {
            let _ = out.write_all(RELAND_USAGE.as_bytes());
            return 0;
        }
        Err(message) => {
            let _ = writeln!(err, "usage: fw reland TASK_ID [--db PATH]");
            let _ = writeln!(err, "{PROG}: error: {message}");
            return 2;
        }
    };
    match run(&args, out, err) {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(err, "{PROG}: {message}");
            1
        }
    }
}

/// Assemble the worker's landing inputs and make the one attempt.
fn run(args: &RelandArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let repo_root = repo_root().map_err(|failure| failure.to_string())?;
    let log = make_logger("[fw]");
    let policy = load_effective_policy(Access::Execute)
        .map_err(|failure| format!("policy error: {failure}"))?
        .filter(|p| p.project.is_some())
        .ok_or_else(|| "policy error: no home project for this repo".to_owned())?;
    let policy = Arc::new(policy);
    let Some(project) = policy.project.clone() else {
        return Err("policy error: no home project for this repo".to_owned());
    };
    let paths = project.paths();
    let phase_base = resolve_landing_base(&repo_root, Some(&policy), Some(&log))
        .map_err(|failure| format!("policy error: {failure}"))?;
    let tasks_dir = paths.tasks();
    let worktrees_dir = resolve_sandbox_root(None, &repo_root, &paths.worktrees());
    let db_path = args.db.clone().unwrap_or_else(|| paths.db());
    let lock_path = paths.merge_lock();

    let items = DirectoryWorkSource::new(&tasks_dir)
        .list_work()
        .map_err(|failure| format!("cannot list tasks in {}: {failure}", tasks_dir.display()))?;
    let Some(item) = items.into_iter().find(|item| item.task.id == args.task_id) else {
        let _ = writeln!(
            out,
            "{}: refused: no active task {} in {}; nothing recorded",
            args.task_id,
            args.task_id,
            tasks_dir.display()
        );
        return Ok(1);
    };

    let environ = environ_from_process();
    let registries = default_store_registries();
    let stores = open_stores_for(Some(&policy), &db_path, &registries, &environ)
        .map_err(|failure| format!("store error: {failure}"))?;
    let submit_store = build_store(
        &registries,
        &store_config_for(Some(&policy), &db_path),
        &environ,
    )
    .map_err(|failure| format!("store error: {failure}"))?;

    let held_out_source = build_held_out_source(Some(&policy), &repo_root).map(Arc::new);
    if let Some(source) = &held_out_source {
        source
            .check_root()
            .map_err(|failure| format!("held-out gate unusable: {failure}"))?;
        log(&format!(
            "held-out gate active root={}",
            source.root.display()
        ));
    }
    let held_out: Option<Arc<dyn HeldOutGraderSource>> =
        held_out_source.map(|source| source as Arc<dyn HeldOutGraderSource>);

    let build_request = landing_build_request(
        &policy,
        &environ,
        repo_root,
        tasks_dir,
        worktrees_dir,
        phase_base,
        lock_path,
        Arc::clone(&log),
        Arc::new(Arc::clone(&submit_store)),
        held_out.clone(),
    );
    let strategy = worker_registry()
        .build(strategy_name_for(Some(&policy)), &build_request)
        .map_err(|failure| failure.to_string())?;
    // The git half of the landing: the candidate tree the held-out gate
    // grades and the workspace preflight / re-materialization.
    let git_side = build_merge_submitter(&build_request);
    let workspace = GitRelandWorkspace {
        submitter: &git_side,
    };

    let agent = sandbox_agent_primitives(Some(&policy), &environ);
    let harness = harness_config_from_policy(Some(&policy), 0, agent.grader_env.clone());
    let gate_options = GateOptions {
        env: agent.grader_env.clone(),
        per_grader_timeout: harness
            .deadlines
            .duration_for_class(DeadlineClass::CommandGrader),
    };
    let lease_seconds = DEFAULT_LEASE_SECONDS;
    let heartbeat_interval = (lease_seconds / 3.0).max(0.001);
    let worker_id = format!("reland-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let clock: Clock = Arc::new(chrono::Utc::now);
    let sink = Arc::clone(&log);
    let ctx = RelandContext {
        control: &stores.core,
        claims: &stores.claims,
        strategy: strategy.as_ref(),
        candidate: &git_side,
        workspace: Some(&workspace),
        worker_id: &worker_id,
        held_out_source: held_out.as_deref(),
        gate_options: &gate_options,
        lease_seconds,
        heartbeat_interval,
        steal_grace_seconds: heartbeat_interval,
        now: &clock,
        log: Some(&sink),
    };
    let sandbox = git_side.worktree(&item.task.id);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failure| failure.to_string())?;
    let outcome = runtime
        .block_on(reland(&ctx, &item, sandbox))
        .map_err(|failure| failure.to_string())?;
    let _ = err.flush();
    Ok(report(&args.task_id, &outcome, out))
}

/// The [`SubmitBuildRequest`] the worker builds, from the same policy.
#[allow(clippy::too_many_arguments)]
fn landing_build_request(
    policy: &Arc<WorkPolicy>,
    environ: &std::collections::BTreeMap<String, String>,
    repo_root: PathBuf,
    tasks_dir: PathBuf,
    worktrees_dir: PathBuf,
    phase_base: String,
    lock_path: PathBuf,
    log: crate::submitter::Logger,
    ledger: Arc<dyn LandingLedger>,
    held_out: Option<Arc<dyn HeldOutGraderSource>>,
) -> SubmitBuildRequest {
    let mut request = SubmitBuildRequest::new(
        repo_root,
        tasks_dir,
        worktrees_dir,
        phase_base,
        lock_path,
        log,
    )
    .with_protected_paths(policy.protected_paths.clone())
    .with_retention(
        policy.sandbox.retention.on_done.clone(),
        policy.sandbox.retention.on_failure.clone(),
    )
    .with_store(ledger)
    .with_resolve_conflict(make_agents_resolve_conflict(
        policy.agent_id.as_deref().unwrap_or(DEFAULT_AGENT_ID),
        None,
        None,
    ))
    .with_policy(Arc::clone(policy));
    if let Some(env) = resolve_grader_env(&policy.sandbox.env, Some(environ)) {
        request = request.with_grader_env(env);
    }
    if let Some(source) = held_out {
        request = request.with_held_out_source(source);
    }
    request
}

/// Print the outcome line and return the exit code.
fn report(task_id: &str, outcome: &RelandOutcome, out: &mut dyn Write) -> i32 {
    match outcome {
        RelandOutcome::Landed {
            run_id,
            strategy,
            landed_ref,
        } => {
            let _ = writeln!(
                out,
                "{task_id}: landed run {run_id} via {strategy} as {landed_ref}"
            );
            0
        }
        RelandOutcome::Reparked {
            run_id,
            park_kind,
            detail,
        } => {
            let _ = writeln!(
                out,
                "{task_id}: re-land of run {run_id} parked: park_kind={park_kind} detail={detail}"
            );
            1
        }
        RelandOutcome::NoOp { run_id, reason } => {
            let _ = writeln!(
                out,
                "{task_id}: re-land of run {run_id} did not land: {reason}"
            );
            1
        }
        RelandOutcome::LeaseLost { run_id } => {
            let _ = writeln!(
                out,
                "{task_id}: re-land of run {run_id} lost the task lease before landing; \
                 nothing submitted"
            );
            1
        }
        RelandOutcome::Refused(refusal) => {
            let _ = writeln!(out, "{task_id}: refused: {refusal}; nothing recorded");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn parses_the_task_id_and_db() {
        assert_eq!(
            parse_args(&argv(&["t1", "--db", "/x.sqlite"])).unwrap(),
            RelandParsed::Args(RelandArgs {
                task_id: "t1".into(),
                db: Some(PathBuf::from("/x.sqlite")),
            })
        );
        assert_eq!(
            parse_args(&argv(&["--db=/y", "t2"])).unwrap(),
            RelandParsed::Args(RelandArgs {
                task_id: "t2".into(),
                db: Some(PathBuf::from("/y")),
            })
        );
        assert_eq!(parse_args(&argv(&["--help"])).unwrap(), RelandParsed::Help);
    }

    #[test]
    fn usage_errors() {
        assert!(parse_args(&argv(&[])).is_err());
        assert!(parse_args(&argv(&["a", "b"])).is_err());
        assert!(parse_args(&argv(&["a", "--db"])).is_err());
        assert!(parse_args(&argv(&["a", "--bogus"])).is_err());
        assert!(parse_args(&argv(&[""])).is_err());
    }

    #[test]
    fn missing_task_id_exits_2_and_help_names_it() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(main_with(&[], &mut out, &mut err), 2);
        assert!(String::from_utf8_lossy(&err).contains("TASK_ID"));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(main_with(&argv(&["--help"]), &mut out, &mut err), 0);
        assert!(String::from_utf8_lossy(&out).contains("fw reland TASK_ID [--db PATH]"));
    }
}
