//! `fw eval estimate`, `fw eval replay`, `fw eval runs`, and the runner
//! side of `fw eval trial --plan`: the CLI over `flywheel_eval::schedule`.
//!
//! A replay runs each cell as its own process (`fw eval trial --plan
//! FILE`), so one trial's crash never takes the run down; with a hub
//! (`--hub-state-dir`), each holds the shared lease `eval:<run>` (visible
//! on the board) and its agent reports through the hub's capture hooks,
//! labelled `fw.eval=<run>`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use clap::Args;
use flywheel_eval::estimate::{self, History};
use flywheel_eval::oracle::Oracle;
use flywheel_eval::proc::{self, Locking, Spec};
use flywheel_eval::schedule::{self, CellRunner, Drive, Progress, Stopped};
use flywheel_eval::set::{self, SetDir, SetManifest, SetRef, Split, Tier, VersionRef};
use flywheel_eval::stats;
use flywheel_eval::store::{EvalStore, RunRow, RunStatus, RunVariantRow, TrialRow, VariantRow};
use flywheel_eval::trial::{TrialOutcome, TrialPlan};
use flywheel_eval::variant::ResolvedVariant;

use crate::eval_verbs::{
    Ctx, claude_dir, new_run_id, parse_span, resolve_variant, run_canary, test_suite_lock,
};

/// Above this estimate (P90, USD) a replay needs an explicit `--budget`.
pub const EXPLICIT_BUDGET_ABOVE: f64 = 50.0;
/// The noise floor's within-task CVs (2026-11-20): the planning default
/// for turns and agent time until a run measures its own.
pub const NOISE_FLOOR_TURNS_CV: f64 = 0.13;
pub const NOISE_FLOOR_TIME_CV: f64 = 0.34;

#[derive(Debug, Args)]
pub struct EstimateArgs {
    /// NAME@vN (or NAME: its latest version).
    pub set: String,
    #[arg(long = "variant", value_name = "NAME", required = true)]
    pub variants: Vec<String>,
    #[arg(short, long, default_value_t = 3)]
    pub k: u32,
    #[arg(long, default_value_t = 3)]
    pub parallel: u32,
    #[command(flatten)]
    pub select: Select,
}

/// Which of the set's tasks run.
#[derive(Debug, Args, Clone, Default)]
pub struct Select {
    /// Only these task ids (comma-separated).
    #[arg(long, value_name = "IDS")]
    pub tasks: Option<String>,
    /// Only tasks of this calibration tier.
    #[arg(long, value_name = "TIER")]
    pub tier: Option<String>,
    /// Only tasks of this split (dev, val, test).
    #[arg(long, value_name = "SPLIT")]
    pub split: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// NAME@vN (or NAME: its latest version); omitted with --resume.
    #[arg(required_unless_present = "resume")]
    pub set: Option<String>,
    #[arg(long = "variant", value_name = "NAME")]
    pub variants: Vec<String>,
    #[arg(short, long, default_value_t = 3)]
    pub k: u32,
    #[arg(long, default_value_t = 3)]
    pub parallel: u32,
    /// The spend cap (USD): no cell starts that would pass it. Default:
    /// the estimate's P90 x 1.1; required above $50.
    #[arg(long, value_name = "USD")]
    pub budget: Option<f64>,
    /// Metrics decided before the run (success, cost, turns, time); the
    /// report marks the rest exploratory.
    #[arg(long, default_value = "success,cost", value_name = "LIST")]
    pub primary: String,
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    #[arg(long, default_value = "2h", value_name = "SPAN")]
    pub worker_timeout: String,
    #[arg(long, default_value = "30m", value_name = "SPAN")]
    pub oracle_timeout: String,
    /// Report to the hub of this state dir: capture hooks (fw.eval label)
    /// and the shared lease eval:<run>.
    #[arg(long, value_name = "DIR")]
    pub hub_state_dir: Option<PathBuf>,
    /// Continue RUN (its set, variants, and k), optionally with a new
    /// --budget.
    #[arg(long, value_name = "RUN")]
    pub resume: Option<String>,
    #[command(flatten)]
    pub select: Select,
    /// Run a draft set (calibration only).
    #[arg(long, hide = true)]
    pub allow_draft: bool,
}

fn load_set(ctx: &Ctx, raw: &str, allow_draft: bool) -> Result<(SetDir, SetManifest), String> {
    let r = SetRef::parse(raw)?;
    if r.version == Some(VersionRef::Draft) && !allow_draft {
        return Err("replays run on a frozen set: fw eval set freeze NAME".into());
    }
    let dir = set::resolve(&ctx.evals, &r).map_err(|e| e.to_string())?;
    let mut manifest = dir.load().map_err(|e| e.to_string())?;
    if manifest.version.is_none() {
        // A draft (calibration): only validated tasks, with what their
        // proofs found (a frozen set carries these itself).
        for t in &mut manifest.tasks {
            match dir.proof(&t.id).map_err(|e| e.to_string())? {
                Some(p) if p.passed && p.base == t.base && p.reference == t.reference => {
                    t.parallel_safe = Some(p.parallel_safe);
                }
                _ => {
                    return Err(format!(
                        "{} is not validated: fw eval set validate {}@draft",
                        t.id, manifest.name
                    ));
                }
            }
        }
    }
    Ok((dir, manifest))
}

/// The set's tasks a selection keeps.
pub fn select(manifest: &SetManifest, s: &Select) -> Result<Vec<String>, String> {
    let ids: Option<Vec<&str>> = s
        .tasks
        .as_deref()
        .map(|t| t.split(',').map(str::trim).collect());
    let tier = s
        .tier
        .as_deref()
        .map(|t| {
            [
                Tier::Uncalibrated,
                Tier::Frontier,
                Tier::Discriminating,
                Tier::Regression,
            ]
            .into_iter()
            .find(|x| x.as_str() == t)
            .ok_or_else(|| {
                format!("--tier {t:?}: frontier, discriminating, regression, uncalibrated")
            })
        })
        .transpose()?;
    let split = s.split.as_deref().map(Split::parse).transpose()?;
    if let Some(ids) = &ids {
        for id in ids {
            if manifest.task(id).is_none() {
                return Err(format!("no task {id} in {}", manifest.label()));
            }
        }
    }
    let out: Vec<String> = manifest
        .tasks
        .iter()
        .filter(|t| ids.as_ref().is_none_or(|ids| ids.contains(&t.id.as_str())))
        .filter(|t| tier.is_none_or(|x| t.tier == x))
        .filter(|t| split.is_none_or(|x| t.split == x))
        .map(|t| t.id.clone())
        .collect();
    if out.is_empty() {
        return Err("the selection keeps no task".into());
    }
    Ok(out)
}

fn subset(manifest: &SetManifest, ids: &[String]) -> SetManifest {
    let mut m = manifest.clone();
    m.tasks.retain(|t| ids.contains(&t.id));
    m
}

fn print_estimate(
    out: &mut dyn Write,
    e: &estimate::Estimate,
    history: &History,
    tasks: usize,
    k: u32,
) {
    let _ = writeln!(
        out,
        "{} trials: ${:.2} expected, ${:.2} at P90 (cost CV {:.0}%); about {:.1} h of wall time",
        e.cells,
        e.mean_usd,
        e.p90_usd,
        e.cost_cv * 100.0,
        e.wall_hours
    );
    let _ = writeln!(
        out,
        "priors: {}",
        e.sources
            .iter()
            .map(|(s, n)| format!("{n} from {s}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let measured = history.cost_cv().is_some();
    let _ = writeln!(
        out,
        "smallest difference this can show (paired by task, {tasks} tasks, k={k}): cost {:.1}%, turns {:.1}%, agent time {:.1}%{}",
        stats::planned_mde_pct(e.cost_cv, tasks, k),
        stats::planned_mde_pct(NOISE_FLOOR_TURNS_CV, tasks, k),
        stats::planned_mde_pct(NOISE_FLOOR_TIME_CV, tasks, k),
        if measured { "" } else { " (noise-floor CVs)" }
    );
    let _ = writeln!(
        out,
        "success: only tasks that sometimes fail can move it; saturated tasks give no information"
    );
}

/// `fw eval estimate`.
pub(crate) fn estimate(ctx: &Ctx, a: &EstimateArgs, out: &mut dyn Write) -> Result<i32, String> {
    let (_, manifest) = load_set(ctx, &a.set, false)?;
    let ids = select(&manifest, &a.select)?;
    let manifest = subset(&manifest, &ids);
    let variants: Vec<ResolvedVariant> = a
        .variants
        .iter()
        .map(|v| resolve_variant(&ctx.evals, v, None))
        .collect::<Result<_, _>>()?;
    let store = ctx.store()?;
    let history = History::from_trials(&store.all_trials().map_err(|e| e.to_string())?);
    let hashes: Vec<String> = variants.iter().map(|v| v.hash.clone()).collect();
    let e = estimate::estimate(&manifest, &hashes, a.k, a.parallel, &history);
    print_estimate(out, &e, &history, ids.len(), a.k);
    Ok(0)
}

/// Runs each cell as `fw eval trial --plan FILE`.
struct SubprocessRunner {
    exe: PathBuf,
    run_id: String,
    run_dir: PathBuf,
    set: SetDir,
    manifest: SetManifest,
    variants: BTreeMap<String, ResolvedVariant>,
    oracles: BTreeMap<String, Oracle>,
    blobs: PathBuf,
    cache: PathBuf,
    worker_timeout_secs: u64,
    oracle_timeout_secs: u64,
    capture: Option<serde_json::Value>,
    budget: Option<f64>,
    lease: Locking,
}

impl CellRunner for SubprocessRunner {
    fn run(&self, trial: &TrialRow) -> Result<TrialOutcome, String> {
        let task = self
            .manifest
            .task(&trial.task_id)
            .cloned()
            .ok_or_else(|| format!("no task {} in the set", trial.task_id))?;
        let variant = self
            .variants
            .get(&trial.variant_hash)
            .cloned()
            .ok_or_else(|| format!("no variant {}", trial.variant_hash))?;
        let oracle = self
            .oracles
            .get(&trial.task_id)
            .cloned()
            .ok_or_else(|| format!("no oracle for {}", trial.task_id))?;
        let plan = TrialPlan {
            run_id: self.run_id.clone(),
            trial_id: trial.trial_id.clone(),
            set: self.set.clone(),
            manifest: self.manifest.clone(),
            task,
            variant,
            blobs: self.blobs.clone(),
            run_dir: self.run_dir.clone(),
            cache: self.cache.clone(),
            oracle,
            worker_timeout_secs: self.worker_timeout_secs,
            oracle_timeout_secs: self.oracle_timeout_secs,
            worker_locking: Locking::None,
            grade_locking: test_suite_lock().map_or(Locking::None, Locking::Flock),
            capture: self.capture.clone(),
            batch_budget: self.budget,
            claude_dir: claude_dir(),
        };
        let plans = self.run_dir.join("plans");
        std::fs::create_dir_all(&plans).map_err(|e| e.to_string())?;
        let file = plans.join(format!("{}.json", trial.trial_id));
        let outcome_file = plans.join(format!("{}.outcome.json", trial.trial_id));
        let _ = std::fs::remove_file(&outcome_file);
        std::fs::write(
            &file,
            serde_json::to_string(&plan).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let ran = proc::run(&Spec {
            argv: vec![
                self.exe.display().to_string(),
                "eval".into(),
                "trial".into(),
                "--plan".into(),
                file.display().to_string(),
            ],
            log: Some(&plans.join(format!("{}.log", trial.trial_id))),
            timeout: Some(std::time::Duration::from_secs(
                self.worker_timeout_secs + 2 * self.oracle_timeout_secs + 900,
            )),
            locking: self.lease.clone(),
            ..Spec::default()
        })
        .map_err(|e| format!("cannot start the trial process: {e}"))?;
        let text = std::fs::read_to_string(&outcome_file).map_err(|_| {
            format!(
                "the trial process left no outcome ({}); see {}",
                ran.describe(),
                plans.display()
            )
        })?;
        serde_json::from_str(&text).map_err(|e| format!("trial outcome: {e}"))
    }
}

/// `fw eval trial --plan FILE`: run the planned trial, write its outcome
/// beside the plan.
pub fn run_plan(plan_file: &Path, out: &mut dyn Write) -> Result<i32, String> {
    let text =
        std::fs::read_to_string(plan_file).map_err(|e| format!("{}: {e}", plan_file.display()))?;
    let plan: TrialPlan =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", plan_file.display()))?;
    let outcome = flywheel_eval::trial::run_trial(&plan, &Utc::now).map_err(|e| e.to_string())?;
    let json = serde_json::to_string(&outcome).map_err(|e| e.to_string())?;
    let dest = plan_file.with_extension("outcome.json");
    std::fs::write(&dest, &json).map_err(|e| format!("{}: {e}", dest.display()))?;
    let _ = writeln!(out, "{json}");
    Ok(0)
}

fn hub_capture(state_dir: &Path) -> Result<serde_json::Value, String> {
    let endpoint = flywheel_hub::endpoint::Endpoint::running(state_dir)?
        .ok_or_else(|| format!("no hub runs for {}", state_dir.display()))?;
    endpoint.blocking_call("GET", "/api/telemetry/capture/hooks", None)
}

fn pin(store: &EvalStore, v: &ResolvedVariant) -> Result<(), String> {
    store
        .put_variant(&VariantRow {
            hash: v.hash.clone(),
            name: v.name.clone(),
            resolved_json: serde_json::to_string(v).map_err(|e| e.to_string())?,
            created_at: Utc::now(),
        })
        .map_err(|e| e.to_string())
}

/// `fw eval replay`.
pub(crate) fn replay(
    ctx: &Ctx,
    a: &ReplayArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    replay_run(ctx, a, out, err).map(|(code, _)| code)
}

/// [`replay`], with the run's id.
fn replay_run(
    ctx: &Ctx,
    a: &ReplayArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(i32, String), String> {
    let store = ctx.store()?;
    let all = store.all_trials().map_err(|e| e.to_string())?;
    let (run, set_dir, manifest, variants) = if let Some(run_id) = &a.resume {
        let run = store.run(run_id).map_err(|e| e.to_string())?;
        let set_dir = if run.set_version == 0 {
            SetDir::draft(&ctx.evals, &run.set_name)
        } else {
            SetDir::frozen(&ctx.evals, &run.set_name, run.set_version)
        };
        let manifest = set_dir.load().map_err(|e| e.to_string())?;
        let mut variants = Vec::new();
        for rv in store.run_variants(run_id).map_err(|e| e.to_string())? {
            let row = store
                .variant(&rv.variant_hash)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("variant {} is not pinned", rv.variant_hash))?;
            let v: ResolvedVariant =
                serde_json::from_str(&row.resolved_json).map_err(|e| e.to_string())?;
            variants.push(v);
        }
        if let Some(budget) = a.budget {
            store
                .set_run_budget(run_id, Some(budget))
                .map_err(|e| e.to_string())?;
        }
        store
            .set_run_status(run_id, RunStatus::Running, None)
            .map_err(|e| e.to_string())?;
        let run = store.run(run_id).map_err(|e| e.to_string())?;
        (run, set_dir, manifest, variants)
    } else {
        let raw = a.set.as_deref().unwrap_or_default();
        let (set_dir, manifest) = load_set(ctx, raw, a.allow_draft)?;
        if a.variants.is_empty() {
            return Err("name at least one --variant".into());
        }
        let ids = select(&manifest, &a.select)?;
        let variants: Vec<ResolvedVariant> = a
            .variants
            .iter()
            .map(|v| resolve_variant(&ctx.evals, v, None))
            .collect::<Result<_, _>>()?;
        let mut seen = std::collections::BTreeSet::new();
        for v in &variants {
            if !seen.insert(v.hash.clone()) {
                return Err(format!(
                    "two variants resolve to the same setup ({}): nothing to compare",
                    v.short_hash()
                ));
            }
        }
        for m in a
            .primary
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            stats::Metric::parse(m)?;
        }
        let history = History::from_trials(&all);
        let hashes: Vec<String> = variants.iter().map(|v| v.hash.clone()).collect();
        let sub = subset(&manifest, &ids);
        let e = estimate::estimate(&sub, &hashes, a.k, a.parallel, &history);
        print_estimate(out, &e, &history, ids.len(), a.k);
        let budget = match a.budget {
            Some(b) => b,
            None if e.p90_usd > EXPLICIT_BUDGET_ABOVE => {
                return Err(format!(
                    "the estimate is ${:.2} at P90, above ${EXPLICIT_BUDGET_ABOVE:.0}: pass --budget USD to go ahead",
                    e.p90_usd
                ));
            }
            None => (e.p90_usd * 1.1 * 100.0).ceil() / 100.0,
        };
        for v in &variants {
            pin(&store, v)?;
        }
        let run_id = new_run_id("eval");
        let cells = schedule::plan_cells(&ids, &hashes, a.k, a.seed);
        let run = RunRow {
            run_id: run_id.clone(),
            set_name: manifest.name.clone(),
            set_version: manifest.version.unwrap_or(0),
            set_hash: manifest.hash.clone().unwrap_or_default(),
            k: a.k,
            seed: a.seed,
            primary_metrics: a.primary.clone(),
            budget_usd: Some(budget),
            pipeline: "full".into(),
            parallel: a.parallel,
            status: RunStatus::Running,
            started_at: Utc::now(),
            ended_at: None,
            analysis_md_hash: None,
            options_json: serde_json::json!({
                "tasks": ids, "worker_timeout": a.worker_timeout, "oracle_timeout": a.oracle_timeout,
                "hub_state_dir": a.hub_state_dir,
            })
            .to_string(),
        };
        let run_variants: Vec<RunVariantRow> = variants
            .iter()
            .enumerate()
            .map(|(i, v)| RunVariantRow {
                ordinal: u32::try_from(i).unwrap_or(u32::MAX),
                variant_hash: v.hash.clone(),
                label: v.name.clone(),
            })
            .collect();
        store
            .create_run(&run, &run_variants, &cells)
            .map_err(|e| e.to_string())?;
        let _ = writeln!(
            out,
            "run {run_id}: {} cells, budget ${budget:.2}",
            cells.len()
        );
        (run, set_dir, manifest, variants)
    };
    for v in &variants {
        run_canary(ctx, v)?;
    }
    let run_dir = ctx.evals.join("runs").join(&run.run_id);
    let oracles = flywheel_eval::trial::materialize_oracles(&set_dir, &manifest, &run_dir)
        .map_err(|e| e.to_string())?;
    let capture = match &a.hub_state_dir {
        Some(dir) => Some(hub_capture(dir)?),
        None => None,
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let lease = match &a.hub_state_dir {
        Some(dir) => Locking::Lease {
            fw: exe.clone(),
            endpoint: vec!["--state-dir".into(), dir.display().to_string()],
            resource: format!("eval:{}", run.run_id),
            shared: true,
            ttl: "3h".into(),
            wait: "0".into(),
            note: format!("fw eval replay {}", run.run_id),
            flock: None,
        },
        None => Locking::None,
    };
    let runner = Arc::new(SubprocessRunner {
        exe,
        run_id: run.run_id.clone(),
        run_dir: run_dir.clone(),
        set: set_dir,
        manifest: manifest.clone(),
        variants: variants
            .iter()
            .map(|v| (v.hash.clone(), v.clone()))
            .collect(),
        oracles,
        blobs: ctx.evals.join("blobs"),
        cache: ctx.cache(),
        worker_timeout_secs: parse_span(&a.worker_timeout)?.as_secs(),
        oracle_timeout_secs: parse_span(&a.oracle_timeout)?.as_secs(),
        capture,
        budget: run.budget_usd,
        lease,
    });
    let hashes: Vec<String> = variants.iter().map(|v| v.hash.clone()).collect();
    let priors = estimate::priors(&manifest, &hashes, &History::from_trials(&all));
    let labels: BTreeMap<String, String> = variants
        .iter()
        .map(|v| (v.hash.clone(), v.name.clone()))
        .collect();
    // Live progress goes to stderr, one line per finished cell.
    let progress = |p: &Progress| {
        let r = &p.row;
        eprintln!(
            "[{}/{}] {:<44} {:<12} {:<12} {:>4} turns ${:>6.2}  spent ${:.2}",
            p.done,
            p.total,
            r.trial_id,
            labels.get(&r.variant_hash).map_or("?", String::as_str),
            match r.status() {
                flywheel_eval::store::TrialStatus::InfraError => "infra-error".to_owned(),
                _ => r.oracle.clone().unwrap_or_else(|| "-".into()),
            },
            r.turns,
            r.cost_usd,
            p.spent
        );
    };
    let halt = || None;
    let worker = format!("{}:{}", hostname(), std::process::id());
    let stopped = schedule::drive(&Drive {
        store: &store,
        run_id: &run.run_id,
        manifest: &manifest,
        priors: &priors,
        parallel: run.parallel,
        budget_usd: run.budget_usd,
        worker: &worker,
        runner,
        halt: &halt,
        progress: &progress,
        now: &Utc::now,
    })
    .map_err(|e| e.to_string())?;
    match stopped {
        Stopped::Done => {
            let _ = writeln!(
                out,
                "run {} done. Next: fw eval report {}",
                run.run_id, run.run_id
            );
            Ok((0, run.run_id.clone()))
        }
        Stopped::Budget {
            spent,
            next_prior,
            remaining,
        } => {
            let _ = writeln!(
                err,
                "run {} paused at the budget: ${spent:.2} spent, the next cell is about ${next_prior:.2}, {remaining} cells left.\n\
                 Continue with: fw eval replay --resume {} --budget USD",
                run.run_id, run.run_id
            );
            Ok((75, run.run_id.clone()))
        }
        Stopped::Halted(why) => {
            let _ = writeln!(
                err,
                "run {} paused: {why}. Continue with: fw eval replay --resume {}",
                run.run_id, run.run_id
            );
            Ok((75, run.run_id.clone()))
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "host".into())
}

/// `fw eval runs`.
pub(crate) fn runs(ctx: &Ctx, json: bool, out: &mut dyn Write) -> Result<i32, String> {
    let store = ctx.store()?;
    let runs = store.runs().map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for r in &runs {
        let trials = store.trials(&r.run_id).map_err(|e| e.to_string())?;
        let variants = store.run_variants(&r.run_id).map_err(|e| e.to_string())?;
        let done = trials
            .iter()
            .filter(|t| {
                !matches!(
                    t.status(),
                    flywheel_eval::store::TrialStatus::Pending
                        | flywheel_eval::store::TrialStatus::Running
                )
            })
            .count();
        rows.push(serde_json::json!({
            "run_id": r.run_id, "set": format!("{}@v{}", r.set_name, r.set_version), "k": r.k,
            "status": r.status.as_str(), "variants": variants.iter().map(|v| v.label.clone()).collect::<Vec<_>>(),
            "done": done, "total": trials.len(), "spent_usd": trials.iter().map(|t| t.cost_usd).sum::<f64>(),
            "budget_usd": r.budget_usd, "started_at": r.started_at,
        }));
    }
    if json {
        let _ = writeln!(out, "{}", serde_json::json!({"runs": rows}));
        return Ok(0);
    }
    if rows.is_empty() {
        let _ = writeln!(
            out,
            "no eval runs yet: fw eval replay SET --variant A --variant B"
        );
    }
    for r in rows {
        let _ = writeln!(
            out,
            "{:<32} {:<12} {:<8} {}/{} ${:.2}{}  {}",
            r["run_id"].as_str().unwrap_or(""),
            r["set"].as_str().unwrap_or(""),
            r["status"].as_str().unwrap_or(""),
            r["done"],
            r["total"],
            r["spent_usd"].as_f64().unwrap_or(0.0),
            r["budget_usd"]
                .as_f64()
                .map(|b| format!(" of ${b:.2}"))
                .unwrap_or_default(),
            r["variants"]
                .as_array()
                .map(|v| v
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" vs "))
                .unwrap_or_default()
        );
    }
    Ok(0)
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    pub run: String,
    /// The variant every other is compared with (default: the first).
    #[arg(long, value_name = "LABEL")]
    pub baseline: Option<String>,
    /// Only tasks of these tiers (comma-separated).
    #[arg(long, value_name = "TIERS")]
    pub tier: Option<String>,
    /// Every task's trials too.
    #[arg(long)]
    pub per_task: bool,
    /// The report as JSON (the hub's view reads the same).
    #[arg(long)]
    pub json: bool,
}

/// The report of `run_id` (shared with the hub's view).
pub fn build_report(
    store: &EvalStore,
    evals: &Path,
    run_id: &str,
    filter: &flywheel_eval::report::Filter,
) -> Result<flywheel_eval::report::EvalReport, String> {
    let run = store.run(run_id).map_err(|e| e.to_string())?;
    let variants = store.run_variants(run_id).map_err(|e| e.to_string())?;
    let trials = store.trials(run_id).map_err(|e| e.to_string())?;
    let set_dir = if run.set_version == 0 {
        SetDir::draft(evals, &run.set_name)
    } else {
        SetDir::frozen(evals, &run.set_name, run.set_version)
    };
    let manifest = set_dir.load().map_err(|e| e.to_string())?;
    flywheel_eval::report::build(&run, &variants, &trials, &manifest, filter)
}

/// `fw eval report`.
pub(crate) fn report(ctx: &Ctx, a: &ReportArgs, out: &mut dyn Write) -> Result<i32, String> {
    let tiers = a
        .tier
        .as_deref()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .map(|t| {
                    [
                        Tier::Uncalibrated,
                        Tier::Frontier,
                        Tier::Discriminating,
                        Tier::Regression,
                    ]
                    .into_iter()
                    .find(|x| x.as_str() == t)
                    .ok_or_else(|| format!("--tier {t:?}"))
                })
                .collect::<Result<std::collections::BTreeSet<_>, _>>()
        })
        .transpose()?;
    let filter = flywheel_eval::report::Filter {
        tiers,
        baseline: a.baseline.clone(),
    };
    let store = ctx.store()?;
    let r = build_report(&store, &ctx.evals, &a.run, &filter)?;
    if a.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&r).map_err(|e| e.to_string())?
        );
    } else {
        let _ = write!(out, "{}", flywheel_eval::report::render(&r, a.per_task));
    }
    Ok(0)
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    pub run: String,
    /// The reader model.
    #[arg(long, default_value = "haiku")]
    pub model: String,
    #[arg(long, default_value_t = 40)]
    pub max_turns: u32,
    /// Seed of the pass sample (runs with more than 200 passes).
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Skip the coordinator's grouped summary.
    #[arg(long)]
    pub no_coordinator: bool,
}

#[derive(Debug, Args)]
pub struct DecideArgs {
    pub run: String,
    pub trial: String,
    /// The reader was right: the pass is voided (counts as a fail).
    #[arg(long, conflicts_with = "clear", required_unless_present = "clear")]
    pub confirm: bool,
    /// The reader was wrong: the pass stands.
    #[arg(long)]
    pub clear: bool,
}

/// `fw eval review`.
pub(crate) fn review(ctx: &Ctx, a: &ReviewArgs, out: &mut dyn Write) -> Result<i32, String> {
    use flywheel_eval::review;
    let store = ctx.store()?;
    let run = store.run(&a.run).map_err(|e| e.to_string())?;
    let trials = store.trials(&a.run).map_err(|e| e.to_string())?;
    let run_dir = ctx.evals.join("runs").join(&a.run);
    let set_dir = if run.set_version == 0 {
        SetDir::draft(&ctx.evals, &run.set_name)
    } else {
        SetDir::frozen(&ctx.evals, &run.set_name, run.set_version)
    };
    let reader = review::AgentReader::new(&a.model, a.max_turns, None);
    let picked = review::pick(&trials, a.seed);
    let _ = writeln!(
        out,
        "reading {} of {} trials with {}",
        picked.len(),
        trials.len(),
        a.model
    );
    store
        .set_analysis_hash(&a.run, &review::analysis_hash())
        .map_err(|e| e.to_string())?;
    let mut findings = Vec::new();
    let mut suspects = 0;
    for id in &picked {
        let trial = store.trial(&a.run, id).map_err(|e| e.to_string())?;
        let dir = run_dir.join("trials").join(id);
        let goal = std::fs::read_to_string(set_dir.task_file(&trial.task_id))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["goal"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let (verdict, confidence, evidence, notes) =
            match review::review_one(&reader, &trial, &dir, &goal) {
                Ok(v) => {
                    if review::suspect(&trial, &v) {
                        suspects += 1;
                        store
                            .set_validity(&a.run, id, "suspect", &format!("reader: {}", v.notes))
                            .map_err(|e| e.to_string())?;
                    }
                    findings.push((id.clone(), v.clone()));
                    (
                        v.verdict,
                        v.confidence,
                        serde_json::to_string(&v.evidence).unwrap_or_else(|_| "[]".into()),
                        v.notes,
                    )
                }
                Err(e) => (
                    "reader_error".to_owned(),
                    "low".to_owned(),
                    "[]".to_owned(),
                    e,
                ),
            };
        let _ = writeln!(
            out,
            "  {id:<44} {verdict:<16} {confidence:<6} {}",
            notes
                .replace('\n', " ")
                .chars()
                .take(100)
                .collect::<String>()
        );
        store
            .put_review(
                &a.run,
                &flywheel_eval::store::ReviewRow {
                    trial_id: id.clone(),
                    reader_model: reader_model_name(&reader),
                    verdict,
                    confidence,
                    evidence_json: evidence,
                    notes,
                    operator_decision: None,
                    decided_at: None,
                    reviewed_at: Utc::now(),
                },
            )
            .map_err(|e| e.to_string())?;
    }
    if !a.no_coordinator && !findings.is_empty() {
        use flywheel_eval::review::Reader;
        match reader.read(&review::coordinator_prompt(&findings), &run_dir) {
            Ok(summary) => {
                let path = review::summary_path(&run_dir);
                std::fs::write(&path, summary).map_err(|e| format!("{}: {e}", path.display()))?;
                let _ = writeln!(out, "grouped findings: {}", path.display());
            }
            Err(e) => {
                let _ = writeln!(out, "the coordinator failed: {e}");
            }
        }
    }
    if suspects > 0 {
        let _ = writeln!(
            out,
            "{suspects} pass(es) are suspect: decide each with fw eval decide {} TRIAL --confirm|--clear",
            a.run
        );
    }
    Ok(0)
}

fn reader_model_name(reader: &flywheel_eval::review::AgentReader) -> String {
    use flywheel_eval::review::Reader;
    reader.model()
}

/// `fw eval decide`.
pub(crate) fn decide(ctx: &Ctx, a: &DecideArgs, out: &mut dyn Write) -> Result<i32, String> {
    let store = ctx.store()?;
    let trial = store.trial(&a.run, &a.trial).map_err(|e| e.to_string())?;
    if trial.validity.as_deref() != Some("suspect") {
        return Err(format!(
            "trial {} is {}, not suspect: only a reader's suspect pass is decided",
            a.trial,
            trial.validity.as_deref().unwrap_or("unchecked")
        ));
    }
    let (validity, decision) = if a.confirm {
        ("voided", "confirmed")
    } else {
        ("valid", "cleared")
    };
    store
        .set_validity(
            &a.run,
            &a.trial,
            validity,
            &format!("operator {decision} the reader"),
        )
        .map_err(|e| e.to_string())?;
    store
        .decide_review(&a.run, &a.trial, decision, Utc::now())
        .map_err(|e| e.to_string())?;
    let _ = writeln!(out, "trial {}: {validity}", a.trial);
    Ok(0)
}

#[derive(Debug, Args)]
pub struct CalibrateArgs {
    /// NAME or NAME@draft.
    pub set: String,
    /// The baseline variant.
    #[arg(long, value_name = "NAME", required_unless_present = "from_run")]
    pub variant: Option<String>,
    #[arg(short, long, default_value_t = 3)]
    pub k: u32,
    #[arg(long, default_value_t = 3)]
    pub parallel: u32,
    #[arg(long, value_name = "USD")]
    pub budget: Option<f64>,
    #[arg(long, value_name = "DIR")]
    pub hub_state_dir: Option<PathBuf>,
    #[arg(long, default_value = "2h", value_name = "SPAN")]
    pub worker_timeout: String,
    /// Take the tiers from this finished run instead of running one.
    #[arg(long, value_name = "RUN")]
    pub from_run: Option<String>,
}

/// The tier each task's baseline trials put it in.
pub fn tiers_of(trials: &[TrialRow]) -> BTreeMap<String, Tier> {
    let mut counts: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for t in trials
        .iter()
        .filter(|t| t.status() == flywheel_eval::store::TrialStatus::Done)
    {
        let c = counts.entry(t.task_id.clone()).or_insert((0, 0));
        c.1 += 1;
        if flywheel_eval::report::passed(t) {
            c.0 += 1;
        }
    }
    counts
        .into_iter()
        .map(|(id, (p, n))| (id, Tier::of(p, n)))
        .collect()
}

/// `fw eval set calibrate`.
pub(crate) fn calibrate(
    ctx: &Ctx,
    a: &CalibrateArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let r = SetRef::parse(&a.set)?;
    if matches!(r.version, Some(VersionRef::V(_))) {
        return Err("calibrate a draft (tiers are fixed at freeze)".into());
    }
    let dir = SetDir::draft(&ctx.evals, &r.name);
    let run_id = match &a.from_run {
        Some(run) => run.clone(),
        None => {
            let args = ReplayArgs {
                set: Some(format!("{}@draft", r.name)),
                variants: a.variant.iter().cloned().collect(),
                k: a.k,
                parallel: a.parallel,
                budget: a.budget,
                primary: "success".into(),
                seed: 1,
                worker_timeout: a.worker_timeout.clone(),
                oracle_timeout: "30m".into(),
                hub_state_dir: a.hub_state_dir.clone(),
                resume: None,
                select: Select::default(),
                allow_draft: true,
            };
            let (code, run_id) = replay_run(ctx, &args, out, err)?;
            if code != 0 {
                let _ = writeln!(
                    err,
                    "calibration paused; tiers are set once run {run_id} finishes (fw eval set calibrate {} --from-run {run_id})",
                    r.name
                );
                return Ok(code);
            }
            run_id
        }
    };
    let store = ctx.store()?;
    let tiers = tiers_of(&store.trials(&run_id).map_err(|e| e.to_string())?);
    let mut manifest = dir.load().map_err(|e| e.to_string())?;
    for t in &mut manifest.tasks {
        if let Some(tier) = tiers.get(&t.id) {
            t.tier = *tier;
            let _ = writeln!(out, "  {:<40} {}", t.id, tier.as_str());
        }
    }
    dir.save(&manifest).map_err(|e| e.to_string())?;
    let _ = writeln!(
        out,
        "tiers from run {run_id} set on {}@draft. Next: fw eval set freeze {}",
        r.name, r.name
    );
    Ok(0)
}
