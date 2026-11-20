//! A run's report: one JSON shape (`EvalReport`, schema
//! [`REPORT_SCHEMA`]) that the CLI prints and the hub renders, so the two
//! cannot disagree.
//!
//! Which trials count: graded ones (`done`). A contaminated or voided pass
//! counts as a fail; a suspect pass still counts as a pass until the
//! operator decides; infra errors are left out (and counted).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::set::{SetManifest, TaskKind, Tier};
use crate::stats::{self, Cell, Comparison, Metric, Obs};
use crate::store::{RunRow, RunVariantRow, TrialRow, TrialStatus};

/// The report's JSON schema version (bumped on a breaking change).
pub const REPORT_SCHEMA: u32 = 1;

/// Whether a graded trial counts as a pass.
pub fn passed(t: &TrialRow) -> bool {
    t.oracle.as_deref() == Some("pass")
        && !matches!(t.validity.as_deref(), Some("contaminated" | "voided"))
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantInfo {
    pub label: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRow {
    pub task: String,
    pub tier: Tier,
    pub kind: TaskKind,
    /// Variant label -> its numbers on this task.
    pub cells: BTreeMap<String, Cell>,
    /// Variant label -> each trial's mark (`pass`, `fail`, `suspect`,
    /// `contaminated`, `voided`, `infra`, `pending`), in k order.
    pub marks: BTreeMap<String, Vec<TrialMark>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrialMark {
    pub trial: String,
    pub mark: String,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overall {
    pub variant: String,
    pub tasks: usize,
    pub trials: usize,
    /// Macro means over tasks.
    pub success: f64,
    pub pass_at_k: Option<f64>,
    pub pass_hat_k: Option<f64>,
    /// Total cost over passes.
    pub cost_per_success: Option<f64>,
    pub turns_per_success: Option<f64>,
    pub cost_total: f64,
    /// Don't-act tasks: share passed.
    pub abstain_accuracy: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantComparison {
    pub baseline: String,
    pub variant: String,
    pub comparisons: Vec<Comparison>,
    /// Metrics declared primary at launch.
    pub primary: Vec<Metric>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub schema: u32,
    pub run_id: String,
    pub set: String,
    pub set_hash: String,
    pub k: u32,
    pub status: String,
    pub variants: Vec<VariantInfo>,
    pub trials_total: usize,
    pub trials_graded: usize,
    pub trials_infra: usize,
    pub trials_pending: usize,
    pub spent_usd: f64,
    pub budget_usd: Option<f64>,
    pub tasks: Vec<TaskRow>,
    pub overall: Vec<Overall>,
    pub comparisons: Vec<VariantComparison>,
    /// Tasks every graded trial passed in every variant: no information
    /// about success.
    pub saturated_tasks: Vec<String>,
    /// The fewest-turn passes per variant (the fastest passes are where
    /// fake passes hide): trial ids worth a read.
    pub worth_a_read: BTreeMap<String, Vec<String>>,
}

fn obs(t: &TrialRow) -> Obs {
    Obs {
        passed: passed(t),
        cost: t.cost_usd,
        turns: f64::from(t.turns),
        agent_secs: t.agent_secs,
    }
}

fn mark(t: &TrialRow) -> String {
    match t.status() {
        TrialStatus::Pending | TrialStatus::Running => "pending".into(),
        TrialStatus::InfraError => "infra".into(),
        TrialStatus::Done => match t.validity.as_deref() {
            Some("contaminated") => "contaminated".into(),
            Some("voided") => "voided".into(),
            Some("suspect") => "suspect".into(),
            _ => t.oracle.clone().unwrap_or_else(|| "fail".into()),
        },
    }
}

/// Which task rows and comparisons a report keeps.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub tiers: Option<BTreeSet<Tier>>,
    /// The baseline variant label (default: the run's first).
    pub baseline: Option<String>,
}

/// Build the report of `run`.
pub fn build(
    run: &RunRow,
    variants: &[RunVariantRow],
    trials: &[TrialRow],
    manifest: &SetManifest,
    filter: &Filter,
) -> Result<EvalReport, String> {
    let keep_task = |id: &str| {
        let tier = manifest.task(id).map(|t| t.tier).unwrap_or_default();
        filter.tiers.as_ref().is_none_or(|ts| ts.contains(&tier))
    };
    let k = u64::from(run.k.max(1));
    let mut tasks = Vec::new();
    let mut by_variant_task: BTreeMap<String, BTreeMap<String, Vec<Obs>>> = BTreeMap::new();
    let mut task_ids: Vec<String> = trials
        .iter()
        .map(|t| t.task_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    task_ids.retain(|id| keep_task(id));
    for id in &task_ids {
        let set_task = manifest.task(id);
        let mut cells = BTreeMap::new();
        let mut marks = BTreeMap::new();
        for v in variants {
            let mut mine: Vec<&TrialRow> = trials
                .iter()
                .filter(|t| &t.task_id == id && t.variant_hash == v.variant_hash)
                .collect();
            mine.sort_by_key(|t| t.k);
            let graded: Vec<Obs> = mine
                .iter()
                .filter(|t| t.status() == TrialStatus::Done)
                .map(|t| obs(t))
                .collect();
            cells.insert(v.label.clone(), stats::cell(&graded, k));
            marks.insert(
                v.label.clone(),
                mine.iter()
                    .map(|t| TrialMark {
                        trial: t.trial_id.clone(),
                        mark: mark(t),
                        cost_usd: t.cost_usd,
                    })
                    .collect(),
            );
            by_variant_task
                .entry(v.label.clone())
                .or_default()
                .insert(id.clone(), graded);
        }
        tasks.push(TaskRow {
            task: id.clone(),
            tier: set_task.map(|t| t.tier).unwrap_or_default(),
            kind: set_task.map(|t| t.kind).unwrap_or_default(),
            cells,
            marks,
        });
    }

    let mut overall = Vec::new();
    for v in variants {
        let per_task = by_variant_task.get(&v.label).cloned().unwrap_or_default();
        let build_tasks: Vec<(&String, &Vec<Obs>)> = per_task
            .iter()
            .filter(|(id, o)| {
                !o.is_empty() && manifest.task(id).is_none_or(|t| t.kind == TaskKind::Build)
            })
            .collect();
        let cells: Vec<Cell> = build_tasks.iter().map(|(_, o)| stats::cell(o, k)).collect();
        let mean =
            |xs: Vec<f64>| (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64);
        let all: Vec<&Obs> = build_tasks.iter().flat_map(|(_, o)| o.iter()).collect();
        let passes = all.iter().filter(|o| o.passed).count();
        let cost_total: f64 = all.iter().map(|o| o.cost).sum();
        let turns_total: f64 = all.iter().map(|o| o.turns).sum();
        let abstain: Vec<&Obs> = per_task
            .iter()
            .filter(|(id, _)| {
                manifest
                    .task(id)
                    .is_some_and(|t| t.kind == TaskKind::DontAct)
            })
            .flat_map(|(_, o)| o.iter())
            .collect();
        overall.push(Overall {
            variant: v.label.clone(),
            tasks: cells.len(),
            trials: all.len(),
            success: mean(cells.iter().map(|c| c.success).collect()).unwrap_or(0.0),
            pass_at_k: mean(cells.iter().filter_map(|c| c.pass_at_k).collect()),
            pass_hat_k: mean(cells.iter().filter_map(|c| c.pass_hat_k).collect()),
            cost_per_success: (passes > 0).then(|| cost_total / passes as f64),
            turns_per_success: (passes > 0).then(|| turns_total / passes as f64),
            cost_total: cost_total + abstain.iter().map(|o| o.cost).sum::<f64>(),
            abstain_accuracy: (!abstain.is_empty())
                .then(|| abstain.iter().filter(|o| o.passed).count() as f64 / abstain.len() as f64),
        });
    }

    let primary: Vec<Metric> = run
        .primary_metrics
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(Metric::parse)
        .collect::<Result<_, _>>()?;
    let baseline = filter
        .baseline
        .clone()
        .or_else(|| variants.first().map(|v| v.label.clone()))
        .ok_or("the run has no variants")?;
    if !variants.iter().any(|v| v.label == baseline) {
        return Err(format!("no variant {baseline} in run {}", run.run_id));
    }
    let mut comparisons = Vec::new();
    for v in variants.iter().filter(|v| v.label != baseline) {
        let mut paired: BTreeMap<String, (Vec<Obs>, Vec<Obs>)> = BTreeMap::new();
        for id in &task_ids {
            if manifest
                .task(id)
                .is_some_and(|t| t.kind == TaskKind::DontAct)
            {
                continue;
            }
            let get = |label: &str| {
                by_variant_task
                    .get(label)
                    .and_then(|m| m.get(id))
                    .cloned()
                    .unwrap_or_default()
            };
            paired.insert(id.clone(), (get(&baseline), get(&v.label)));
        }
        comparisons.push(VariantComparison {
            baseline: baseline.clone(),
            variant: v.label.clone(),
            comparisons: Metric::ALL
                .iter()
                .map(|m| stats::compare(*m, &paired, run.seed))
                .collect(),
            primary: primary.clone(),
        });
    }

    let saturated_tasks = tasks
        .iter()
        .filter(|t| {
            t.kind == TaskKind::Build && t.cells.values().all(|c| c.n > 0 && c.passes == c.n)
        })
        .map(|t| t.task.clone())
        .collect();
    let mut worth_a_read = BTreeMap::new();
    for v in variants {
        let mut passes: Vec<&TrialRow> = trials
            .iter()
            .filter(|t| {
                t.variant_hash == v.variant_hash && t.status() == TrialStatus::Done && passed(t)
            })
            .collect();
        passes.sort_by_key(|t| (t.turns, t.trial_id.clone()));
        worth_a_read.insert(
            v.label.clone(),
            passes.iter().take(3).map(|t| t.trial_id.clone()).collect(),
        );
    }
    let count = |s: TrialStatus| trials.iter().filter(|t| t.status() == s).count();
    Ok(EvalReport {
        schema: REPORT_SCHEMA,
        run_id: run.run_id.clone(),
        set: format!("{}@v{}", run.set_name, run.set_version),
        set_hash: run.set_hash.clone(),
        k: run.k,
        status: run.status.as_str().to_owned(),
        variants: variants
            .iter()
            .map(|v| VariantInfo {
                label: v.label.clone(),
                hash: v.variant_hash.clone(),
            })
            .collect(),
        trials_total: trials.len(),
        trials_graded: count(TrialStatus::Done),
        trials_infra: count(TrialStatus::InfraError),
        trials_pending: count(TrialStatus::Pending) + count(TrialStatus::Running),
        spent_usd: trials.iter().map(|t| t.cost_usd).sum(),
        budget_usd: run.budget_usd,
        tasks,
        overall,
        comparisons,
        saturated_tasks,
        worth_a_read,
    })
}

fn pct(x: Option<f64>) -> String {
    x.map_or_else(|| "n/a".into(), |v| format!("{:.0}%", v * 100.0))
}

fn usd(x: Option<f64>) -> String {
    x.map_or_else(|| "n/a".into(), |v| format!("${v:.2}"))
}

/// The report as text (what `fw eval report` prints).
pub fn render(r: &EvalReport, per_task: bool) -> String {
    let mut out = String::new();
    let line = |out: &mut String, s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(
        &mut out,
        format!(
            "run {}  {}  k={}  {}  {}/{} graded ({} infra, {} pending)  ${:.2}{}",
            r.run_id,
            r.set,
            r.k,
            r.status,
            r.trials_graded,
            r.trials_total,
            r.trials_infra,
            r.trials_pending,
            r.spent_usd,
            r.budget_usd
                .map(|b| format!(" of ${b:.2}"))
                .unwrap_or_default()
        ),
    );
    line(&mut out, String::new());
    line(
        &mut out,
        format!(
            "{:<20} {:>6} {:>8} {:>8} {:>8} {:>12} {:>12} {:>9}",
            "variant",
            "tasks",
            "success",
            "pass@k",
            "pass^k",
            "cost/success",
            "turns/succ",
            "abstain"
        ),
    );
    for o in &r.overall {
        line(
            &mut out,
            format!(
                "{:<20} {:>6} {:>8} {:>8} {:>8} {:>12} {:>12} {:>9}",
                o.variant,
                o.tasks,
                pct(Some(o.success)),
                pct(o.pass_at_k),
                pct(o.pass_hat_k),
                usd(o.cost_per_success),
                o.turns_per_success
                    .map_or_else(|| "n/a".into(), |t| format!("{t:.0}")),
                pct(o.abstain_accuracy)
            ),
        );
    }
    for c in &r.comparisons {
        line(&mut out, String::new());
        line(
            &mut out,
            format!("{} vs {} (paired by task):", c.variant, c.baseline),
        );
        for m in &c.comparisons {
            let unit = if m.metric == Metric::Success {
                "pts"
            } else {
                "%"
            };
            let star = if c.primary.contains(&m.metric) {
                "*"
            } else {
                " "
            };
            let text = if m.verdict == stats::Verdict::NoInformation {
                format!(
                    "{star}{:<8} no information ({} tasks; saturated)",
                    m.metric.as_str(),
                    m.tasks
                )
            } else {
                format!(
                    "{star}{:<8} {:+.1}{unit} [{:+.1}, {:+.1}]  MDE {:.1}{unit}  {}",
                    m.metric.as_str(),
                    m.delta,
                    m.ci_low,
                    m.ci_high,
                    m.mde,
                    m.verdict.as_str()
                )
            };
            line(&mut out, format!("  {text}"));
        }
        if !c.primary.is_empty() {
            line(&mut out, "  (* primary; the rest are exploratory)".into());
        }
    }
    if !r.saturated_tasks.is_empty() {
        line(&mut out, String::new());
        line(
            &mut out,
            format!(
                "saturated (every trial passed in every variant: no success information): {}",
                r.saturated_tasks.join(", ")
            ),
        );
    }
    if r.worth_a_read.values().any(|v| !v.is_empty()) {
        line(&mut out, String::new());
        line(&mut out, "fastest passes, worth a read:".into());
        for (v, ids) in &r.worth_a_read {
            line(&mut out, format!("  {v}: {}", ids.join(", ")));
        }
    }
    if per_task {
        line(&mut out, String::new());
        for t in &r.tasks {
            line(
                &mut out,
                format!("{} ({}, {:?})", t.task, t.tier.as_str(), t.kind),
            );
            for (v, marks) in &t.marks {
                let c = &t.cells[v];
                line(
                    &mut out,
                    format!(
                        "  {:<18} {}  success {}  pass^k {}  cost/success {}",
                        v,
                        marks
                            .iter()
                            .map(|m| m.mark.as_str())
                            .collect::<Vec<_>>()
                            .join(" "),
                        pct(Some(c.success)),
                        pct(c.pass_hat_k),
                        usd(c.cost_per_success)
                    ),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
