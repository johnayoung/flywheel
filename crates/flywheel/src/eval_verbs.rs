//! `fw eval`: build frozen task sets from past work, replay them under
//! pinned agent-setup variants, and report the difference with its noise.
//! The engine is `flywheel-eval`; this module resolves the cwd project,
//! opens its stores, and prints. Doc: `docs/eval.md` (`fw docs eval`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};
use flywheel_eval::mine::{self, MineOptions};
use flywheel_eval::set::{self, SetDir, SetRef, SplitRatios, VersionRef};
use flywheel_eval::store::EvalStore;
use flywheel_eval::validate::{self, ValidateOptions};
use flywheel_eval::variant;
use flywheel_orchestrator::config::Access;
use flywheel_orchestrator::home::{Project, resolve_cwd_project};
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::workflow::{load_effective_policy, resolve_db_path};

pub const EVAL_VERB: &str = "eval";

#[derive(Debug, Parser)]
#[command(
    name = "fw eval",
    about = "Measure whether a change to the agent setup makes agents better or worse",
    after_help = "Docs: fw docs eval"
)]
struct EvalCli {
    #[command(subcommand)]
    command: EvalCommand,
}

#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Build, validate, freeze, and inspect task sets.
    #[command(subcommand)]
    Set(SetCommand),
    /// Resolve, pin, and compare agent-setup variants.
    #[command(subcommand)]
    Variant(VariantCommand),
    /// Run one trial: TASK of a frozen SET under --variant, recorded as a
    /// one-cell run (prints the trial's row as JSON last).
    Trial(TrialArgs),
    /// What a replay would cost and how small a difference it could show.
    Estimate(crate::eval_replay::EstimateArgs),
    /// Replay SET under every --variant, k times each (or --resume RUN).
    Replay(crate::eval_replay::ReplayArgs),
    /// Every run: set, variants, progress, spend.
    Runs(JsonArg),
    /// A run's numbers: per task and overall, and each variant against the
    /// baseline with its interval, smallest detectable difference, and
    /// verdict (better, worse, tie, no information).
    Report(crate::eval_replay::ReportArgs),
    /// The deterministic trace checks over a transcript dir (a trial's
    /// `transcripts/`), as JSON.
    Trace(TraceArgs),
    /// Read RUN's transcripts with a cheap reader agent per trial
    /// (failures and passes), then group the findings.
    Review(crate::eval_replay::ReviewArgs),
    /// Decide on a suspect pass: --confirm voids it, --clear keeps it.
    Decide(crate::eval_replay::DecideArgs),
}

#[derive(Debug, Args)]
struct TraceArgs {
    dir: PathBuf,
    /// A path the agent must not name (repeatable).
    #[arg(long = "forbid", value_name = "PATH")]
    forbid: Vec<String>,
}

#[derive(Debug, Args)]
struct TrialArgs {
    /// NAME@vN (or NAME: its latest version).
    #[arg(required_unless_present = "plan")]
    set: Option<String>,
    #[arg(required_unless_present = "plan")]
    task: Option<String>,
    #[arg(long, value_name = "NAME", required_unless_present = "plan")]
    variant: Option<String>,
    /// Run the trial a replay planned (its runner process); writes the
    /// outcome beside the plan.
    #[arg(long, value_name = "FILE", hide = true, conflicts_with_all = ["set", "task", "variant"])]
    plan: Option<PathBuf>,
    /// The worker's wall-clock bound.
    #[arg(long, default_value = "2h", value_name = "SPAN")]
    worker_timeout: String,
    /// One oracle run's bound.
    #[arg(long, default_value = "30m", value_name = "SPAN")]
    oracle_timeout: String,
}

#[derive(Debug, Subcommand)]
enum VariantCommand {
    /// Resolve NAME (evals/variants/NAME.toml, or --file) and record it.
    Pin(PinArgs),
    /// What NAME resolves to now.
    Show(VariantShowArgs),
    /// What differs between two variants.
    Diff(VariantDiffArgs),
    /// Every variant file and every pinned hash.
    Ls(JsonArg),
}

#[derive(Debug, Args)]
struct PinArgs {
    name: String,
    /// The variant file (default: evals/variants/NAME.toml).
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct VariantShowArgs {
    name: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct VariantDiffArgs {
    a: String,
    b: String,
}

#[derive(Debug, Subcommand)]
enum SetCommand {
    /// Mine this project's past runs into NAME's draft (adds to it).
    Build(BuildArgs),
    /// Prove each draft task can judge a trial (V1-V7); writes proofs.
    Validate(ValidateArgs),
    /// Freeze NAME's validated draft into its next immutable version.
    Freeze(FreezeArgs),
    /// Every set: frozen versions and drafts.
    Ls(JsonArg),
    /// One set's tasks (NAME = its latest version, NAME@vN, NAME@draft).
    Show(ShowArgs),
    /// What changed between two versions of a set.
    Diff(DiffArgs),
    /// Add each TASK's "don't act" twin to NAME's draft: the task replayed
    /// at its own reference, where the right move is to check and change
    /// nothing (validate the twins next).
    DontAct(DontActArgs),
    /// Run the baseline k times on each draft task and set its tier from
    /// how often it passed: frontier (never), discriminating (sometimes),
    /// regression (always).
    Calibrate(crate::eval_replay::CalibrateArgs),
    /// Add a recorded hub session to NAME's draft when it is a sound task
    /// (one intent, merged, code); its oracle must then be blind-authored.
    FromSession(FromSessionArgs),
}

#[derive(Debug, Args)]
struct FromSessionArgs {
    name: String,
    session: String,
    /// The hub that recorded it.
    #[arg(long, value_name = "DIR")]
    hub_state_dir: PathBuf,
}

#[derive(Debug, Args)]
struct DontActArgs {
    name: String,
    /// Validated tasks of the draft (comma-separated).
    tasks: String,
}

#[derive(Debug, Args)]
pub(crate) struct JsonArg {
    /// Print JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
struct BuildArgs {
    name: String,
    /// Which candidates: landed, hard (comma-separated).
    #[arg(long, default_value = "landed,hard", value_name = "KINDS")]
    include: String,
    /// Only runs started on or after this date (YYYY-MM-DD).
    #[arg(long, value_name = "DATE")]
    since: Option<String>,
    /// At most this many candidates (hard ones first).
    #[arg(long, value_name = "N")]
    max: Option<usize>,
    /// Only these task ids (comma-separated).
    #[arg(long, value_name = "IDS")]
    tasks: Option<String>,
}

#[derive(Debug, Args)]
struct ValidateArgs {
    /// NAME or NAME@draft.
    set: String,
    /// Only this task.
    #[arg(long, value_name = "TASK")]
    only: Option<String>,
    /// One oracle run's bound (`90s`, `30m`, `2h`).
    #[arg(long, default_value = "30m", value_name = "SPAN")]
    timeout: String,
    /// Skip V5's fw-process probe.
    #[arg(long)]
    no_noise: bool,
}

#[derive(Debug, Args)]
struct FreezeArgs {
    name: String,
    /// Split fractions, e.g. dev=.6,val=.2,test=.2 (default: all dev).
    #[arg(long, value_name = "RATIOS")]
    split: Option<String>,
    /// Seed of the split assignment.
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

#[derive(Debug, Args)]
struct ShowArgs {
    set: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DiffArgs {
    a: String,
    b: String,
}

/// What every eval verb works on.
pub(crate) struct Ctx {
    pub(crate) project: Project,
    pub(crate) policy: Option<WorkPolicy>,
    pub(crate) evals: PathBuf,
}

impl Ctx {
    pub(crate) fn resolve() -> Result<Self, String> {
        let project = resolve_cwd_project().map_err(|e| e.to_string())?;
        let policy =
            load_effective_policy(Access::ReadOnly).map_err(|e| format!("policy error: {e}"))?;
        let evals = project.paths.evals();
        Ok(Self {
            project,
            policy,
            evals,
        })
    }

    pub(crate) fn repo(&self) -> &Path {
        &self.project.meta.path
    }

    /// The eval tables of the project store.
    pub(crate) fn store(&self) -> Result<EvalStore, String> {
        crate::hub_eval::open_store(
            self.policy.as_ref(),
            &resolve_db_path(None, self.policy.as_ref()),
        )
    }

    fn core(&self) -> Result<std::sync::Arc<dyn flywheel_core::store::HarnessStore>, String> {
        let db = resolve_db_path(None, self.policy.as_ref());
        crate::tui::open_store_bundle(self.policy.as_ref(), &db).map(|b| b.core)
    }

    pub(crate) fn cache(&self) -> PathBuf {
        self.evals.join("cache")
    }
}

/// `90s`, `30m`, `2h`, or bare seconds.
pub fn parse_span(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let (num, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let n: u64 = num
        .parse()
        .map_err(|_| format!("span {raw:?}: like 90s, 30m, 2h"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("span {raw:?}: like 90s, 30m, 2h")),
    };
    Ok(Duration::from_secs(secs))
}

/// The variable naming the lock eval's oracle runs take turns on: a path,
/// or `0` for none.
pub const EVAL_LOCK_ENV: &str = "FLYWHEEL_EVAL_LOCK";

/// The lock oracle runs take turns on, from the environment:
/// `FLYWHEEL_EVAL_LOCK` (a path, or `0`/`off` for none) wins; else none
/// when this process already runs inside a `scripts/check.sh` test phase
/// (`FLYWHEEL_CHECK_PHASE=tests`, whose lock its caller holds: taking it
/// again would wait on ourselves) or the check lock is off
/// (`FLYWHEEL_CHECK_LOCK=0`); else the machine's test-suite lock,
/// `scripts/check.sh`'s, so oracle runs take turns with every test suite.
pub fn test_suite_lock_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    match env(EVAL_LOCK_ENV).as_deref().map(str::trim) {
        Some("0" | "off") => return None,
        Some(path) if !path.is_empty() => return Some(PathBuf::from(path)),
        _ => {}
    }
    if env("FLYWHEEL_CHECK_PHASE").as_deref() == Some("tests")
        || env("FLYWHEEL_CHECK_LOCK").as_deref() == Some("0")
    {
        return None;
    }
    Some(
        env("XDG_RUNTIME_DIR")
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
            .join("flywheel-check.lock"),
    )
}

pub(crate) fn test_suite_lock() -> Option<PathBuf> {
    test_suite_lock_from(&|k| std::env::var(k).ok())
}

/// Take the test-suite lock (see [`test_suite_lock`]) for the rest of the
/// process, waiting at most `wait`; `None` when there is no lock to take.
pub fn hold_test_suite_lock(
    wait: Duration,
    err: &mut dyn Write,
) -> Result<Option<flywheel_hub::flock::Held>, String> {
    match test_suite_lock() {
        None => Ok(None),
        Some(path) => hold_lock(&path, wait, err).map(Some),
    }
}

/// Take `path`'s exclusive flock through the hub's lock helper
/// ([`flywheel_hub::flock::hold`]): asked from inside a process that holds
/// it (an ancestor), it fails at once; else it waits at most `wait`, and
/// running out names the holder.
fn hold_lock(
    path: &Path,
    wait: Duration,
    err: &mut dyn Write,
) -> Result<flywheel_hub::flock::Held, String> {
    flywheel_hub::flock::hold(path, wait, "fw eval", &mut |holder| {
        let _ = writeln!(
            err,
            "fw eval: waiting for another test suite on this machine ({}, held by {holder})",
            path.display()
        );
    })
    .map_err(|e| format!("{e} (set {EVAL_LOCK_ENV}=0 to skip the test-suite lock)"))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

fn set_build(ctx: &Ctx, args: &BuildArgs, out: &mut dyn Write) -> Result<i32, String> {
    set::validate_name(&args.name)?;
    let kinds: Vec<&str> = args.include.split(',').map(str::trim).collect();
    for k in &kinds {
        if !["landed", "hard"].contains(k) {
            return Err(format!("--include {k:?}: landed or hard"));
        }
    }
    let since = args
        .since
        .as_deref()
        .map(|d| {
            NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight").and_utc())
                .map_err(|_| format!("--since {d:?}: YYYY-MM-DD"))
        })
        .transpose()?;
    let options = MineOptions {
        landed: kinds.contains(&"landed"),
        hard: kinds.contains(&"hard"),
        since,
        max: args.max,
    };
    let core = ctx.core()?;
    let (mut candidates, skipped) =
        mine::mine(core.as_ref(), &ctx.project.paths, ctx.repo(), &options)
            .map_err(|e| e.to_string())?;
    if let Some(only) = &args.tasks {
        let ids: Vec<&str> = only.split(',').map(str::trim).collect();
        candidates.retain(|c| ids.contains(&c.task.id.as_str()));
    }
    let manifest = mine::build_draft(
        &ctx.evals,
        &args.name,
        &ctx.project.id,
        ctx.repo(),
        &ctx.project.paths,
        &candidates,
        Utc::now(),
    )
    .map_err(|e| e.to_string())?;
    for c in &candidates {
        let _ = writeln!(
            out,
            "+ {:<36} {:<13} base {} ref {}  {}",
            c.task.id,
            c.task.source.as_str(),
            short(&c.task.base),
            if c.task.reference.is_empty() {
                "(bisect)"
            } else {
                short(&c.task.reference)
            },
            c.reason
        );
    }
    for s in &skipped {
        let _ = writeln!(out, "  skipped {:<28} {}", s.task_id, s.reason);
    }
    let _ = writeln!(
        out,
        "{} now has {} tasks. Next: fw eval set validate {}@draft",
        manifest.label(),
        manifest.tasks.len(),
        args.name
    );
    Ok(0)
}

fn set_validate(
    ctx: &Ctx,
    args: &ValidateArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let set_ref = SetRef::parse(&args.set)?;
    if matches!(set_ref.version, Some(VersionRef::V(_))) {
        return Err(format!("{} is frozen; validate NAME@draft", args.set));
    }
    let dir = SetDir::draft(&ctx.evals, &set_ref.name);
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let options = ValidateOptions {
        work: ctx.cache().join("validate").join(&set_ref.name),
        target_dir: ctx.cache().join("target"),
        timeout: parse_span(&args.timeout)?,
        noise: (!args.no_noise).then(|| vec![exe.display().to_string(), "--version".to_owned()]),
        search_head: "HEAD".to_owned(),
    };
    let _lock = hold_test_suite_lock(options.timeout, err)?;
    let mut failed = 0;
    let proofs = validate::validate_set(
        &dir,
        args.only.as_deref(),
        &options,
        &Utc::now,
        &mut |proof| {
            let mark = if proof.passed { "ok  " } else { "FAIL" };
            let _ = writeln!(
                out,
                "{mark} {:<36} {}{}",
                proof.task_id,
                proof
                    .checks
                    .iter()
                    .map(|c| format!("{}{}", c.name, if c.passed { "+" } else { "-" }))
                    .collect::<Vec<_>>()
                    .join(" "),
                if proof.passed && !proof.parallel_safe {
                    "  (graded alone)"
                } else {
                    ""
                }
            );
            for c in proof.checks.iter().filter(|c| !c.passed) {
                let _ = writeln!(out, "       {}: {}", c.name, c.detail);
            }
        },
    )
    .map_err(|e| e.to_string())?;
    for p in &proofs {
        if !p.passed {
            failed += 1;
        }
    }
    let _ = writeln!(
        out,
        "{} of {} validated. {}",
        proofs.len() - failed,
        proofs.len(),
        if failed == 0 {
            format!("Next: fw eval set freeze {}", set_ref.name)
        } else {
            "Fix or drop the failing tasks (a failing task blocks freeze).".to_owned()
        }
    );
    Ok(i32::from(failed > 0))
}

fn set_freeze(ctx: &Ctx, args: &FreezeArgs, out: &mut dyn Write) -> Result<i32, String> {
    let ratios = match &args.split {
        Some(raw) => SplitRatios::parse(raw)?,
        None => SplitRatios::default(),
    };
    let store = ctx.store()?;
    let manifest = set::freeze(
        &ctx.evals,
        &args.name,
        &store,
        ratios,
        args.seed,
        Utc::now(),
    )
    .map_err(|e| e.to_string())?;
    let _ = writeln!(
        out,
        "froze {} ({} tasks, hash {})",
        manifest.label(),
        manifest.tasks.len(),
        short(manifest.hash.as_deref().unwrap_or(""))
    );
    Ok(0)
}

fn set_ls(ctx: &Ctx, args: &JsonArg, out: &mut dyn Write) -> Result<i32, String> {
    let store = ctx.store()?;
    let frozen = store.sets().map_err(|e| e.to_string())?;
    let mut drafts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(ctx.evals.join("sets")) {
        for e in entries.flatten() {
            let dir = SetDir::draft(&ctx.evals, &e.file_name().to_string_lossy());
            if let Ok(m) = dir.load() {
                drafts.push(m);
            }
        }
    }
    drafts.sort_by(|a, b| a.name.cmp(&b.name));
    if args.json {
        let body = serde_json::json!({
            "frozen": frozen.iter().map(|s| serde_json::json!({
                "name": s.name, "version": s.version, "hash": s.hash, "tasks": s.task_count,
                "created_at": s.created_at, "path": s.path})).collect::<Vec<_>>(),
            "drafts": drafts.iter().map(|m| serde_json::json!({
                "name": m.name, "tasks": m.tasks.len()})).collect::<Vec<_>>(),
        });
        let _ = writeln!(out, "{body}");
        return Ok(0);
    }
    for s in &frozen {
        let _ = writeln!(
            out,
            "{:<24} {:>3} tasks  hash {}  {}",
            format!("{}@v{}", s.name, s.version),
            s.task_count,
            short(&s.hash),
            s.created_at.format("%Y-%m-%d %H:%M")
        );
    }
    for m in &drafts {
        let _ = writeln!(out, "{:<24} {:>3} tasks  (draft)", m.label(), m.tasks.len());
    }
    if frozen.is_empty() && drafts.is_empty() {
        let _ = writeln!(out, "no eval sets yet: fw eval set build NAME");
    }
    Ok(0)
}

fn set_show(ctx: &Ctx, args: &ShowArgs, out: &mut dyn Write) -> Result<i32, String> {
    let set_ref = SetRef::parse(&args.set)?;
    let dir = set::resolve(&ctx.evals, &set_ref).map_err(|e| e.to_string())?;
    let manifest = dir.load().map_err(|e| e.to_string())?;
    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&manifest).map_err(|e| e.to_string())?
        );
        return Ok(0);
    }
    let _ = writeln!(
        out,
        "{}  {} tasks{}",
        manifest.label(),
        manifest.tasks.len(),
        manifest
            .hash
            .as_deref()
            .map(|h| format!("  hash {}", short(h)))
            .unwrap_or_default()
    );
    for t in &manifest.tasks {
        let proof = dir.proof(&t.id).ok().flatten();
        let _ = writeln!(
            out,
            "  {:<36} {:<13} {:<14} {:<4} base {} ref {}  {}{}",
            t.id,
            t.source.as_str(),
            t.tier.as_str(),
            t.split.as_str(),
            short(&t.base),
            if t.reference.is_empty() {
                "-"
            } else {
                short(&t.reference)
            },
            match &proof {
                Some(p) if p.passed => "validated",
                Some(_) => "FAILED validation",
                None => "not validated",
            },
            match t.parallel_safe {
                Some(false) => " (graded alone)",
                _ => "",
            }
        );
    }
    Ok(0)
}

fn set_dont_act(ctx: &Ctx, args: &DontActArgs, out: &mut dyn Write) -> Result<i32, String> {
    let dir = SetDir::draft(&ctx.evals, &args.name);
    let mut manifest = dir.load().map_err(|e| e.to_string())?;
    for id in args
        .tasks
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let twin = flywheel_eval::dont_act::generate(&dir, &mut manifest, id)
            .map_err(|e| e.to_string())?;
        let _ = writeln!(out, "+ {} (at {})", twin.id, short(&twin.base));
    }
    let _ = writeln!(out, "Next: fw eval set validate {}@draft", args.name);
    Ok(0)
}

fn set_from_session(ctx: &Ctx, a: &FromSessionArgs, out: &mut dyn Write) -> Result<i32, String> {
    use flywheel_eval::sessions;
    let endpoint = flywheel_hub::endpoint::Endpoint::running(&a.hub_state_dir)?
        .ok_or_else(|| format!("no hub runs for {}", a.hub_state_dir.display()))?;
    let view = endpoint.blocking_call("GET", &format!("/api/sessions/{}", a.session), None)?;
    let worktree = &view["worktree"];
    let branch = worktree["branch"]
        .as_str()
        .ok_or("the session has no change branch (fw.change): nothing merged to compare")?;
    let default_branch = worktree["base"].as_str().unwrap_or("main");
    let repo = worktree["repo"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.repo().to_owned());
    let started = view["created_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
        .ok_or("the session has no start time")?;
    let conversation = view["agent_session_id"]
        .as_str()
        .ok_or("the session has no agent conversation")?;
    let cwd = view["cwd"]
        .as_str()
        .map(PathBuf::from)
        .ok_or("the session has no cwd")?;
    let transcript = claude_dir()
        .join("projects")
        .join(flywheel_eval::trial::claude_project_key(&cwd))
        .join(format!("{conversation}.jsonl"));
    let text = std::fs::read_to_string(&transcript)
        .map_err(|e| format!("{}: {e}", transcript.display()))?;
    let (prompts, reads) = sessions::from_transcript(&text);
    let (base, merged_tip, changed) = sessions::git_facts(&repo, branch, default_branch, started)?;
    let facts = sessions::SessionFacts {
        session_id: a.session.clone(),
        branch: branch.to_owned(),
        base,
        merged_tip,
        prompts,
        read_before_edit: reads
            .into_iter()
            .map(|p| {
                p.strip_prefix(&format!("{}/", cwd.display()))
                    .map_or(p.clone(), str::to_owned)
            })
            .collect(),
        changed_files: changed,
    };
    let verdict = sessions::judge(&facts);
    if !verdict.sound {
        for r in &verdict.reasons {
            let _ = writeln!(out, "  not sound: {r}");
        }
        return Ok(1);
    }
    let dir = SetDir::draft(&ctx.evals, &a.name);
    let mut manifest = if dir.manifest().is_file() {
        dir.load().map_err(|e| e.to_string())?
    } else {
        set::SetManifest {
            name: a.name.clone(),
            version: None,
            project_id: ctx.project.id.clone(),
            repo: ctx.repo().to_owned(),
            created_at: Utc::now(),
            tasks: Vec::new(),
            hash: None,
        }
    };
    let task = sessions::add_to_draft(&dir, &mut manifest, &facts).map_err(|e| e.to_string())?;
    let _ = writeln!(
        out,
        "+ {} from session {} (base {}, reference {}).\nNext: blind-author its oracle (/fw-verify, from the goal and the reference diff) into {} with its files under {}, then fw eval set validate {}@draft",
        task.id,
        a.session,
        short(&task.base),
        short(&task.reference),
        dir.registration(&task.id).display(),
        dir.oracle_dir(&task.id).join("home").display(),
        a.name
    );
    Ok(0)
}

fn set_diff(ctx: &Ctx, args: &DiffArgs, out: &mut dyn Write) -> Result<i32, String> {
    let load = |raw: &str| -> Result<set::SetManifest, String> {
        let r = SetRef::parse(raw)?;
        set::resolve(&ctx.evals, &r)
            .and_then(|d| d.load())
            .map_err(|e| e.to_string())
    };
    let (a, b) = (load(&args.a)?, load(&args.b)?);
    let lines = set::diff(&a, &b);
    if lines.is_empty() {
        let _ = writeln!(out, "no task changed");
    }
    for l in lines {
        let _ = writeln!(out, "{l}");
    }
    Ok(0)
}

fn variants_dir(ctx: &Ctx) -> PathBuf {
    ctx.evals.join("variants")
}

/// Resolve variant `name` (its file under `evals/variants/`, or `file`).
pub(crate) fn resolve_variant(
    evals: &Path,
    name: &str,
    file: Option<&Path>,
) -> Result<variant::ResolvedVariant, String> {
    let path = file.map_or_else(
        || evals.join("variants").join(format!("{name}.toml")),
        Path::to_owned,
    );
    let parsed = variant::load_file(&path).map_err(|e| e.to_string())?;
    let base_dir = path.parent().unwrap_or(Path::new(".")).to_owned();
    let env = variant::ResolveEnv::from_process(&base_dir, &evals.join("blobs"))?;
    let name = parsed.name.clone().unwrap_or_else(|| name.to_owned());
    variant::resolve(&name, &parsed, &env, None).map_err(|e| e.to_string())
}

fn describe_variant(v: &variant::ResolvedVariant) -> Vec<(String, String)> {
    let mut rows = vec![
        ("hash".to_owned(), v.short_hash().to_owned()),
        (
            "agent".to_owned(),
            format!(
                "{} ({})",
                v.agent,
                if v.agent_cli_version.is_empty() {
                    "version unknown"
                } else {
                    &v.agent_cli_version
                }
            ),
        ),
        (
            "model".to_owned(),
            v.model.clone().unwrap_or_else(|| "(agent default)".into()),
        ),
        (
            "fw".to_owned(),
            format!(
                "{} {} sha256 {}",
                v.fw_path.display(),
                v.fw_version,
                &v.fw_sha256[..12]
            ),
        ),
        ("permission_mode".to_owned(), v.permission_mode.clone()),
        (
            "setting_sources".to_owned(),
            if v.setting_sources.is_empty() {
                "(n/a: codex)".into()
            } else {
                v.setting_sources.join(",")
            },
        ),
        (
            "settings".to_owned(),
            v.settings_blob
                .as_deref()
                .map_or_else(|| "-".into(), |b| b[..12].to_owned()),
        ),
        (
            "env".to_owned(),
            v.env
                .iter()
                .map(|(k, val)| format!("{k}={val}"))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        (
            "claude_md (user)".to_owned(),
            v.claude_md_blob
                .as_deref()
                .map_or_else(|| "-".into(), |b| b[..12].to_owned()),
        ),
        (
            "skills (user)".to_owned(),
            v.skills.keys().cloned().collect::<Vec<_>>().join(","),
        ),
        (
            "mcp (user)".to_owned(),
            v.mcp_servers.keys().cloned().collect::<Vec<_>>().join(","),
        ),
    ];
    for op in &v.repo {
        rows.push((
            format!("repo {}", op.path),
            format!(
                "{:?}{}{}",
                op.op,
                op.blob
                    .as_deref()
                    .map(|b| format!(" {}", &b[..12]))
                    .unwrap_or_default(),
                op.pattern
                    .as_deref()
                    .map(|p| format!(" /{p}/"))
                    .unwrap_or_default()
            ),
        ));
    }
    for (k, d) in &v.unisolated {
        rows.push((format!("unisolated {k}"), d[..12].to_owned()));
    }
    rows
}

fn variant_pin(ctx: &Ctx, args: &PinArgs, out: &mut dyn Write) -> Result<i32, String> {
    let v = resolve_variant(&ctx.evals, &args.name, args.file.as_deref())?;
    let store = ctx.store()?;
    store
        .put_variant(&flywheel_eval::store::VariantRow {
            hash: v.hash.clone(),
            name: v.name.clone(),
            resolved_json: serde_json::to_string(&v).map_err(|e| e.to_string())?,
            created_at: Utc::now(),
        })
        .map_err(|e| e.to_string())?;
    let _ = writeln!(out, "pinned {}@{}", v.name, v.short_hash());
    Ok(0)
}

fn variant_show(ctx: &Ctx, args: &VariantShowArgs, out: &mut dyn Write) -> Result<i32, String> {
    let v = resolve_variant(&ctx.evals, &args.name, None)?;
    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&v).map_err(|e| e.to_string())?
        );
        return Ok(0);
    }
    let _ = writeln!(out, "{}", v.name);
    for (k, val) in describe_variant(&v) {
        let _ = writeln!(out, "  {k:<18} {val}");
    }
    Ok(0)
}

fn variant_diff(ctx: &Ctx, args: &VariantDiffArgs, out: &mut dyn Write) -> Result<i32, String> {
    let a = describe_variant(&resolve_variant(&ctx.evals, &args.a, None)?);
    let b = describe_variant(&resolve_variant(&ctx.evals, &args.b, None)?);
    let keys: std::collections::BTreeSet<&String> =
        a.iter().chain(b.iter()).map(|(k, _)| k).collect();
    let get = |rows: &[(String, String)], k: &str| {
        rows.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "-".into())
    };
    let mut same = true;
    for k in keys {
        let (va, vb) = (get(&a, k), get(&b, k));
        if va != vb {
            same = false;
            let _ = writeln!(out, "{k}\n  {}: {va}\n  {}: {vb}", args.a, args.b);
        }
    }
    if same {
        let _ = writeln!(out, "the same setup");
    }
    Ok(0)
}

fn variant_ls(ctx: &Ctx, args: &JsonArg, out: &mut dyn Write) -> Result<i32, String> {
    let mut files: Vec<String> = std::fs::read_dir(variants_dir(ctx))
        .map(|d| {
            d.flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|n| n.strip_suffix(".toml"))
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    if args.json {
        let _ = writeln!(out, "{}", serde_json::json!({"files": files}));
        return Ok(0);
    }
    if files.is_empty() {
        let _ = writeln!(
            out,
            "no variants yet: write {}/NAME.toml (fw docs eval)",
            variants_dir(ctx).display()
        );
    }
    for f in files {
        let line = match resolve_variant(&ctx.evals, &f, None) {
            Ok(v) => format!(
                "{f:<24} {}  {} {}",
                v.short_hash(),
                v.agent,
                v.model.as_deref().unwrap_or_default()
            ),
            Err(e) => format!("{f:<24} INVALID: {e}"),
        };
        let _ = writeln!(out, "{line}");
    }
    Ok(0)
}

/// Claude Code's config dir: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub(crate) fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map_or_else(
        || {
            std::env::var_os("HOME")
                .map_or_else(|| PathBuf::from("."), PathBuf::from)
                .join(".claude")
        },
        PathBuf::from,
    )
}

/// Prove the variant's Claude Code leaves out user layers (see
/// `flywheel_eval::canary`), with the variant's own PATH.
pub(crate) fn run_canary(ctx: &Ctx, v: &variant::ResolvedVariant) -> Result<(), String> {
    if !v.is_claude() {
        return Ok(());
    }
    let env: Vec<(String, String)> = v
        .env
        .iter()
        .filter(|(k, _)| *k == "PATH")
        .map(|(k, val)| (k.clone(), val.clone()))
        .collect();
    let scratch = ctx.cache().join("canary").join(v.short_hash());
    let _ = std::fs::remove_dir_all(&scratch);
    let result =
        flywheel_eval::canary::check(Path::new("claude"), &v.setting_sources, &scratch, &env);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

pub(crate) fn new_run_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        Utc::now().format("%Y%m%d%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    )
}

fn one_trial(ctx: &Ctx, args: &TrialArgs, out: &mut dyn Write) -> Result<i32, String> {
    use flywheel_eval::store::{RunRow, RunStatus, RunVariantRow, TrialRow};
    if let Some(plan) = &args.plan {
        return crate::eval_replay::run_plan(plan, out);
    }
    let (set_arg, task_arg, variant_arg) = (
        args.set.as_deref().unwrap_or_default(),
        args.task.as_deref().unwrap_or_default(),
        args.variant.as_deref().unwrap_or_default(),
    );
    let set_ref = SetRef::parse(set_arg)?;
    if set_ref.version == Some(VersionRef::Draft) {
        return Err("trials run on a frozen set: fw eval set freeze NAME".into());
    }
    let set_dir = set::resolve(&ctx.evals, &set_ref).map_err(|e| e.to_string())?;
    let manifest = set_dir.load().map_err(|e| e.to_string())?;
    let task = manifest
        .task(task_arg)
        .cloned()
        .ok_or_else(|| format!("no task {task_arg} in {}", manifest.label()))?;
    let v = resolve_variant(&ctx.evals, variant_arg, None)?;
    run_canary(ctx, &v)?;
    let store = ctx.store()?;
    store
        .put_variant(&flywheel_eval::store::VariantRow {
            hash: v.hash.clone(),
            name: v.name.clone(),
            resolved_json: serde_json::to_string(&v).map_err(|e| e.to_string())?,
            created_at: Utc::now(),
        })
        .map_err(|e| e.to_string())?;
    let run_id = new_run_id("trial");
    let trial_id = format!("{}-{}-k0", task.id, v.short_hash());
    store
        .create_run(
            &RunRow {
                run_id: run_id.clone(),
                set_name: manifest.name.clone(),
                set_version: manifest.version.unwrap_or(0),
                set_hash: manifest.hash.clone().unwrap_or_default(),
                k: 1,
                seed: 0,
                primary_metrics: String::new(),
                budget_usd: None,
                pipeline: "full".into(),
                parallel: 1,
                status: RunStatus::Running,
                started_at: Utc::now(),
                ended_at: None,
                analysis_md_hash: None,
                options_json: serde_json::json!({"kind": "single-trial"}).to_string(),
            },
            &[RunVariantRow {
                ordinal: 0,
                variant_hash: v.hash.clone(),
                label: v.name.clone(),
            }],
            &[TrialRow::planned(
                trial_id.clone(),
                0,
                v.hash.clone(),
                task.id.clone(),
                0,
            )],
        )
        .map_err(|e| e.to_string())?;
    let worker = format!("fw-eval-{}", std::process::id());
    let none = |_: &TrialRow| false;
    let lease = Utc::now() + chrono::Duration::hours(24);
    store
        .claim_next(&run_id, &worker, Utc::now(), lease, &none)
        .map_err(|e| e.to_string())?
        .ok_or("the trial could not be claimed")?;
    let run_dir = ctx.evals.join("runs").join(&run_id);
    let oracle = flywheel_eval::oracle::materialize(
        &set_dir,
        &task.id,
        &flywheel_eval::trial::run_oracle_dir(&run_dir, &task.id),
    )
    .map_err(|e| e.to_string())?;
    let plan = flywheel_eval::trial::TrialPlan {
        run_id: run_id.clone(),
        trial_id: trial_id.clone(),
        set: set_dir,
        manifest: manifest.clone(),
        task,
        variant: v,
        blobs: ctx.evals.join("blobs"),
        run_dir: run_dir.clone(),
        cache: ctx.cache(),
        oracle,
        worker_timeout_secs: parse_span(&args.worker_timeout)?.as_secs(),
        oracle_timeout_secs: parse_span(&args.oracle_timeout)?.as_secs(),
        worker_locking: flywheel_eval::proc::Locking::None,
        grade_locking: test_suite_lock().map_or(
            flywheel_eval::proc::Locking::None,
            flywheel_eval::proc::Locking::Flock,
        ),
        capture: None,
        batch_budget: None,
        claude_dir: claude_dir(),
    };
    let outcome = flywheel_eval::trial::run_trial(&plan, &Utc::now).map_err(|e| e.to_string())?;
    let mut row = outcome.row.clone();
    row.ordinal = 0;
    store
        .finish(&run_id, &worker, &row)
        .map_err(|e| e.to_string())?;
    store
        .put_checks(&run_id, &trial_id, "oracle", &outcome.checks)
        .map_err(|e| e.to_string())?;
    store
        .put_artifacts(&run_id, &trial_id, &outcome.artifacts)
        .map_err(|e| e.to_string())?;
    store
        .set_run_status(&run_id, RunStatus::Done, Some(Utc::now()))
        .map_err(|e| e.to_string())?;
    let _ = writeln!(
        out,
        "trial {trial_id} of run {run_id}: {} ({}), {} turns, ${:.2}; artifacts in {}",
        row.oracle.as_deref().unwrap_or("no verdict"),
        row.final_where.as_deref().unwrap_or("-"),
        row.turns,
        row.cost_usd,
        outcome.dir.display()
    );
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string(&row).map_err(|e| e.to_string())?
    );
    Ok(i32::from(
        row.status() != flywheel_eval::store::TrialStatus::Done,
    ))
}

/// `fw eval ...`.
pub fn run_eval(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let argv = std::iter::once("fw eval".to_owned()).chain(args.iter().cloned());
    let cli = match EvalCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            let text = e.render().to_string();
            if code == 0 {
                let _ = write!(out, "{text}");
            } else {
                let _ = write!(err, "{text}");
            }
            return code;
        }
    };
    let ctx = match Ctx::resolve() {
        Ok(ctx) => ctx,
        Err(message) => {
            let _ = writeln!(err, "fw eval: {message}");
            return 2;
        }
    };
    let result = match &cli.command {
        EvalCommand::Set(SetCommand::Build(a)) => set_build(&ctx, a, out),
        EvalCommand::Set(SetCommand::Validate(a)) => set_validate(&ctx, a, out, err),
        EvalCommand::Set(SetCommand::Freeze(a)) => set_freeze(&ctx, a, out),
        EvalCommand::Set(SetCommand::Ls(a)) => set_ls(&ctx, a, out),
        EvalCommand::Set(SetCommand::Show(a)) => set_show(&ctx, a, out),
        EvalCommand::Set(SetCommand::Diff(a)) => set_diff(&ctx, a, out),
        EvalCommand::Set(SetCommand::DontAct(a)) => set_dont_act(&ctx, a, out),
        EvalCommand::Set(SetCommand::FromSession(a)) => set_from_session(&ctx, a, out),
        EvalCommand::Set(SetCommand::Calibrate(a)) => {
            crate::eval_replay::calibrate(&ctx, a, out, err)
        }
        EvalCommand::Variant(VariantCommand::Pin(a)) => variant_pin(&ctx, a, out),
        EvalCommand::Variant(VariantCommand::Show(a)) => variant_show(&ctx, a, out),
        EvalCommand::Variant(VariantCommand::Diff(a)) => variant_diff(&ctx, a, out),
        EvalCommand::Variant(VariantCommand::Ls(a)) => variant_ls(&ctx, a, out),
        EvalCommand::Trial(a) => one_trial(&ctx, a, out),
        EvalCommand::Estimate(a) => crate::eval_replay::estimate(&ctx, a, out),
        EvalCommand::Replay(a) => crate::eval_replay::replay(&ctx, a, out, err),
        EvalCommand::Runs(a) => crate::eval_replay::runs(&ctx, a.json, out),
        EvalCommand::Report(a) => crate::eval_replay::report(&ctx, a, out),
        EvalCommand::Review(a) => crate::eval_replay::review(&ctx, a, out),
        EvalCommand::Decide(a) => crate::eval_replay::decide(&ctx, a, out),
        EvalCommand::Trace(a) => flywheel_eval::trace::read_dir(
            &a.dir,
            &flywheel_eval::trace::Bounds {
                forbidden: a.forbid.clone(),
            },
        )
        .map_err(|e| format!("{}: {e}", a.dir.display()))
        .and_then(|t| serde_json::to_string(&t).map_err(|e| e.to_string()))
        .map(|j| {
            let _ = writeln!(out, "{j}");
            0
        }),
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(err, "fw eval: {message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_suite_lock_follows_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(
            test_suite_lock_from(&env(&[("XDG_RUNTIME_DIR", "/run/u")])),
            Some(PathBuf::from("/run/u/flywheel-check.lock"))
        );
        assert_eq!(
            test_suite_lock_from(&env(&[("FLYWHEEL_CHECK_PHASE", "tests")])),
            None
        );
        assert_eq!(
            test_suite_lock_from(&env(&[("FLYWHEEL_CHECK_LOCK", "0")])),
            None
        );
        assert_eq!(
            test_suite_lock_from(&env(&[(EVAL_LOCK_ENV, "0"), ("XDG_RUNTIME_DIR", "/r")])),
            None
        );
        assert_eq!(
            test_suite_lock_from(&env(&[
                (EVAL_LOCK_ENV, "/t/l"),
                ("FLYWHEEL_CHECK_PHASE", "tests")
            ])),
            Some(PathBuf::from("/t/l"))
        );
    }

    #[test]
    fn a_held_lock_is_waited_for_only_as_long_as_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/lock");
        let mut err = Vec::new();
        let held = hold_lock(&path, Duration::from_secs(1), &mut err).unwrap();
        let started = std::time::Instant::now();
        let e = hold_lock(&path, Duration::from_secs(1), &mut err).unwrap_err();
        assert!(e.contains("waited 1s"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(held);
        hold_lock(&path, Duration::from_secs(1), &mut err).unwrap();
    }

    #[test]
    fn a_lock_asked_for_inside_its_holder_fails_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("check.lock");
        let mut err = Vec::new();
        let _held = hold_lock(&path, Duration::from_secs(1), &mut err).unwrap();
        // A child of the holder (a tool its test phase runs) asks with an
        // hour to wait: it must answer at once, naming the ancestor.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "eval_verbs::tests::child_asks_for_the_lock",
                "--nocapture",
            ])
            .env("FLYWHEEL_TEST_EVAL_LOCK", &path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!("the child waited on its own ancestor's lock");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
        assert!(
            out.contains("CHILD: ") && out.contains("already held by your ancestor"),
            "{out}"
        );
    }

    #[test]
    #[ignore = "run by a_lock_asked_for_inside_its_holder_fails_at_once"]
    fn child_asks_for_the_lock() {
        let Some(path) = std::env::var_os("FLYWHEEL_TEST_EVAL_LOCK") else {
            return;
        };
        let mut err = Vec::new();
        match hold_lock(Path::new(&path), Duration::from_secs(3600), &mut err) {
            Ok(_) => println!("CHILD: took it"),
            Err(e) => println!("CHILD: {e}"),
        }
    }

    #[test]
    fn spans_parse() {
        assert_eq!(parse_span("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_span("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_span("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_span("15").unwrap(), Duration::from_secs(15));
        assert!(parse_span("2d").is_err());
    }

    #[test]
    fn help_lists_the_set_verbs() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run_eval(&["set".into(), "--help".into()], &mut out, &mut err),
            0
        );
        let text = String::from_utf8(out).unwrap();
        for verb in ["build", "validate", "freeze", "ls", "show", "diff"] {
            assert!(text.contains(verb), "{verb}: {text}");
        }
    }
}
