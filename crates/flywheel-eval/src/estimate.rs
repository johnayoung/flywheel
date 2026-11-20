//! What a replay will cost before it runs, from what trials cost before.
//!
//! Each cell's prior is the first of:
//!
//! 1. the median cost of this task under this variant in earlier trials;
//! 2. the median cost of this task under any variant;
//! 3. the task's original run cost (recorded in the set at mining);
//! 4. the median of the cells that have one.
//!
//! The spread: each cell's cost is taken to vary with the measured
//! run-to-run coefficient of variation (the noise floor's 10% unless the
//! history says otherwise), independently, so the total's P90 is
//! `mean + 1.2816 * sqrt(sum of variances)`. Wall time is the median
//! trial duration times the cells over the parallelism.

use std::collections::BTreeMap;

use crate::set::SetManifest;
use crate::store::{TrialRow, TrialStatus};

/// The run-to-run cost CV the noise floor measured (2026-11-20).
pub const NOISE_FLOOR_COST_CV: f64 = 0.10;
/// The noise floor's median trial wall time (seconds), the fallback when
/// no trial has a duration.
pub const NOISE_FLOOR_TRIAL_SECS: f64 = 21.0 * 60.0;
/// z for a one-sided 90th percentile.
const Z90: f64 = 1.281_551_6;

/// One cell's prior and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Prior {
    pub cost_usd: f64,
    pub secs: f64,
    /// `this variant`, `any variant`, `original run`, or `set median`.
    pub source: &'static str,
}

/// The median of `xs` (`None` when empty).
pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        f64::midpoint(v[n / 2 - 1], v[n / 2])
    })
}

/// Earlier finished trials, by task and by (task, variant).
#[derive(Debug, Default)]
pub struct History {
    by_cell: BTreeMap<(String, String), Vec<(f64, f64)>>,
    by_task: BTreeMap<String, Vec<(f64, f64)>>,
}

impl History {
    /// Index every graded trial (done, with a cost).
    pub fn from_trials<'a>(trials: impl IntoIterator<Item = &'a TrialRow>) -> Self {
        let mut h = Self::default();
        for t in trials {
            if t.status() != TrialStatus::Done || t.cost_usd <= 0.0 {
                continue;
            }
            let point = (t.cost_usd, t.wall_secs);
            h.by_cell
                .entry((t.task_id.clone(), t.variant_hash.clone()))
                .or_default()
                .push(point);
            h.by_task.entry(t.task_id.clone()).or_default().push(point);
        }
        h
    }

    /// The run-to-run cost CV within (task, variant) cells that have two
    /// or more trials, pooled; `None` without such cells.
    pub fn cost_cv(&self) -> Option<f64> {
        let mut cvs = Vec::new();
        for points in self.by_cell.values().filter(|p| p.len() >= 2) {
            let costs: Vec<f64> = points.iter().map(|p| p.0).collect();
            let mean = costs.iter().sum::<f64>() / costs.len() as f64;
            let var =
                costs.iter().map(|c| (c - mean).powi(2)).sum::<f64>() / (costs.len() - 1) as f64;
            if mean > 0.0 {
                cvs.push(var.sqrt() / mean);
            }
        }
        (!cvs.is_empty()).then(|| cvs.iter().sum::<f64>() / cvs.len() as f64)
    }
}

/// The prior of every (task, variant) cell.
pub fn priors(
    manifest: &SetManifest,
    variants: &[String],
    history: &History,
) -> BTreeMap<(String, String), Prior> {
    let secs_all: Vec<f64> = history
        .by_task
        .values()
        .flatten()
        .map(|p| p.1)
        .filter(|s| *s > 0.0)
        .collect();
    let secs_default = median(&secs_all).unwrap_or(NOISE_FLOOR_TRIAL_SECS);
    let mut out = BTreeMap::new();
    let mut unknown = Vec::new();
    for task in &manifest.tasks {
        for v in variants {
            let key = (task.id.clone(), v.clone());
            let cell = history.by_cell.get(&key);
            let any = history.by_task.get(&task.id);
            let pick = |points: &Vec<(f64, f64)>, source| Prior {
                cost_usd: median(&points.iter().map(|p| p.0).collect::<Vec<_>>()).unwrap_or(0.0),
                secs: median(
                    &points
                        .iter()
                        .map(|p| p.1)
                        .filter(|s| *s > 0.0)
                        .collect::<Vec<_>>(),
                )
                .unwrap_or(secs_default),
                source,
            };
            let prior = if let Some(points) = cell {
                Some(pick(points, "this variant"))
            } else if let Some(points) = any {
                Some(pick(points, "any variant"))
            } else {
                task.prior_cost_usd.filter(|c| *c > 0.0).map(|c| Prior {
                    cost_usd: c,
                    secs: secs_default,
                    source: "original run",
                })
            };
            match prior {
                Some(p) => {
                    out.insert(key, p);
                }
                None => unknown.push(key),
            }
        }
    }
    let known: Vec<f64> = out.values().map(|p| p.cost_usd).collect();
    let fallback = median(&known).unwrap_or(0.0);
    for key in unknown {
        out.insert(
            key,
            Prior {
                cost_usd: fallback,
                secs: secs_default,
                source: "set median",
            },
        );
    }
    out
}

/// A replay's estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub cells: usize,
    pub mean_usd: f64,
    pub p90_usd: f64,
    /// Wall hours at the given parallelism.
    pub wall_hours: f64,
    pub cost_cv: f64,
    /// Cells per prior source.
    pub sources: BTreeMap<&'static str, usize>,
}

/// Estimate `k` trials of every task under every variant at `parallel`.
pub fn estimate(
    manifest: &SetManifest,
    variants: &[String],
    k: u32,
    parallel: u32,
    history: &History,
) -> Estimate {
    let priors = priors(manifest, variants, history);
    let cv = history.cost_cv().unwrap_or(NOISE_FLOOR_COST_CV);
    let k = f64::from(k);
    let mut mean = 0.0;
    let mut var = 0.0;
    let mut secs = 0.0;
    let mut sources = BTreeMap::new();
    for prior in priors.values() {
        mean += prior.cost_usd * k;
        var += (cv * prior.cost_usd).powi(2) * k;
        secs += prior.secs * k;
        *sources.entry(prior.source).or_insert(0) += 1;
    }
    Estimate {
        cells: priors.len() * k as usize,
        mean_usd: mean,
        p90_usd: mean + Z90 * var.sqrt(),
        wall_hours: secs / f64::from(parallel.max(1)) / 3600.0,
        cost_cv: cv,
        sources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::{Provenance, SetTask, Source, Split, TaskKind, Tier};
    use chrono::Utc;

    fn task(id: &str, prior: Option<f64>) -> SetTask {
        SetTask {
            id: id.into(),
            source: Source::Landed,
            tier: Tier::Uncalibrated,
            split: Split::Dev,
            kind: TaskKind::Build,
            base: "b".into(),
            reference: "r".into(),
            files_digest: String::new(),
            parallel_safe: Some(true),
            prior_cost_usd: prior,
            oracle_secs: None,
            prerequisites: BTreeMap::new(),
            provenance: Provenance::default(),
        }
    }

    fn manifest(tasks: Vec<SetTask>) -> SetManifest {
        SetManifest {
            name: "core".into(),
            version: Some(1),
            project_id: "p".into(),
            repo: "/r".into(),
            created_at: Utc::now(),
            tasks,
            hash: None,
        }
    }

    fn done(task: &str, variant: &str, cost: f64, secs: f64) -> TrialRow {
        TrialRow {
            task_id: task.into(),
            variant_hash: variant.into(),
            status: Some(TrialStatus::Done),
            cost_usd: cost,
            wall_secs: secs,
            ..TrialRow::default()
        }
    }

    #[test]
    fn priors_fall_back_in_order() {
        let m = manifest(vec![
            task("t1", Some(9.0)),
            task("t2", Some(4.0)),
            task("t3", None),
        ]);
        let trials = [
            done("t1", "va", 2.0, 600.0),
            done("t1", "va", 4.0, 1200.0),
            done("t2", "vb", 1.0, 300.0),
        ];
        let h = History::from_trials(&trials);
        let p = priors(&m, &["va".into(), "vb".into()], &h);
        let get = |t: &str, v: &str| p[&(t.to_owned(), v.to_owned())].clone();
        assert_eq!(
            (get("t1", "va").cost_usd, get("t1", "va").source),
            (3.0, "this variant")
        );
        assert_eq!(
            (get("t1", "vb").cost_usd, get("t1", "vb").source),
            (3.0, "any variant")
        );
        assert_eq!(get("t2", "vb").source, "this variant");
        assert_eq!(
            (get("t2", "va").cost_usd, get("t2", "va").source),
            (1.0, "any variant")
        );
        assert_eq!(get("t3", "va").source, "set median");
        assert_eq!(get("t1", "va").secs, 900.0);
    }

    #[test]
    fn the_estimate_scales_with_k_and_spreads_with_the_cv() {
        let m = manifest(vec![task("t1", Some(2.0)), task("t2", Some(4.0))]);
        let h = History::default();
        let e = estimate(&m, &["va".into(), "vb".into()], 3, 3, &h);
        assert_eq!(e.cells, 12);
        assert!((e.mean_usd - 36.0).abs() < 1e-9);
        let var: f64 = 3.0 * 2.0 * ((0.1f64 * 2.0).powi(2) + (0.1f64 * 4.0).powi(2));
        assert!((e.p90_usd - (36.0 + Z90 * var.sqrt())).abs() < 1e-9);
        assert!((e.wall_hours - 12.0 * NOISE_FLOOR_TRIAL_SECS / 3.0 / 3600.0).abs() < 1e-9);
        assert_eq!(e.sources["original run"], 4);
        // A measured CV replaces the default.
        let trials = [done("t1", "va", 2.0, 60.0), done("t1", "va", 3.0, 60.0)];
        let cv = History::from_trials(&trials).cost_cv().unwrap();
        assert!((cv - (0.5f64.sqrt() / 2.5)).abs() < 1e-9);
    }
}
