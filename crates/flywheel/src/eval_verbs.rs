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
use flywheel_orchestrator::config::Access;
use flywheel_orchestrator::home::{Project, resolve_cwd_project};
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::workflow::{load_effective_policy, resolve_db_path, store_partition};

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
}

#[derive(Debug, Args)]
struct JsonArg {
    /// Print JSON.
    #[arg(long)]
    json: bool,
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
struct Ctx {
    project: Project,
    policy: Option<WorkPolicy>,
    evals: PathBuf,
}

impl Ctx {
    fn resolve() -> Result<Self, String> {
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

    fn repo(&self) -> &Path {
        &self.project.meta.path
    }

    /// The eval tables of the project store.
    fn store(&self) -> Result<EvalStore, String> {
        let partition = store_partition(self.policy.as_ref());
        let backend = self.policy.as_ref().map(|p| p.store_backend.as_str());
        if backend == Some("postgres") {
            let dsn = self
                .policy
                .as_ref()
                .and_then(|p| p.store_dsn.as_ref())
                .map(|d| d.expose().to_owned())
                .or_else(|| {
                    flywheel_orchestrator::store_factory::resolve_postgres_dsn(
                        &crate::tui::process_environ(),
                    )
                })
                .ok_or("store backend is postgres but no DSN is configured")?;
            return EvalStore::connect_postgres(
                &dsn,
                flywheel_core::store_postgres::DEFAULT_SCHEMA,
                &partition,
            )
            .map_err(|e| e.to_string());
        }
        let db = resolve_db_path(None, self.policy.as_ref());
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        EvalStore::open_sqlite(&db, &partition).map_err(|e| e.to_string())
    }

    fn core(&self) -> Result<std::sync::Arc<dyn flywheel_core::store::HarnessStore>, String> {
        let db = resolve_db_path(None, self.policy.as_ref());
        crate::tui::open_store_bundle(self.policy.as_ref(), &db).map(|b| b.core)
    }

    fn cache(&self) -> PathBuf {
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

/// The machine's test-suite lock (`scripts/check.sh`'s), held for the rest
/// of the process: oracle runs take turns with every test suite here.
pub fn hold_test_suite_lock(err: &mut dyn Write) -> Result<std::fs::File, String> {
    let dir =
        std::env::var_os("XDG_RUNTIME_DIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let path = dir.join("flywheel-check.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let _ = writeln!(
            err,
            "fw eval: waiting for another test suite on this machine ({})",
            path.display()
        );
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "flock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(file)
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
    let _lock = hold_test_suite_lock(err)?;
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
