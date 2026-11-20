//! The numbers a report states, and how sure it may be of them.
//!
//! Per task and variant: success, unbiased pass@k and pass^k, cost and
//! turns per success. Across tasks: macro means (each task weighs the
//! same: an overall average hides a bad task). Between two variants:
//! paired by task -- the per-task difference of means (log ratio for
//! cost, turns, and time), a 95% interval from a paired bootstrap over
//! tasks (trials resampled within each task), the minimal detectable
//! difference from this run's own spread, and a verdict: `better`,
//! `worse`, `tie` (the interval holds zero), or `no information` (every
//! task saturated for the metric: nothing could have moved).

use std::collections::BTreeMap;

use serde::Serialize;

/// z for two-sided alpha 0.05 plus z for power 0.80.
pub const Z_MDE: f64 = 1.959_964 + 0.841_621;
/// Bootstrap resamples.
pub const RESAMPLES: usize = 10_000;

/// One graded trial, as statistics see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Obs {
    pub passed: bool,
    pub cost: f64,
    pub turns: f64,
    pub agent_secs: f64,
}

/// `n choose k` as a float.
fn choose(n: u64, k: u64) -> f64 {
    if k > n {
        return 0.0;
    }
    let k = k.min(n - k);
    (0..k).fold(1.0, |acc, i| acc * (n - i) as f64 / (i + 1) as f64)
}

/// Unbiased pass@k from `c` passes in `n` trials (Chen et al. 2021):
/// `1 - C(n-c, k) / C(n, k)`; `None` when k > n.
pub fn pass_at_k(n: u64, c: u64, k: u64) -> Option<f64> {
    (k >= 1 && k <= n).then(|| 1.0 - choose(n - c, k) / choose(n, k))
}

/// Unbiased pass^k (every one of k trials passes): `C(c, k) / C(n, k)`.
pub fn pass_hat_k(n: u64, c: u64, k: u64) -> Option<f64> {
    (k >= 1 && k <= n).then(|| choose(c, k) / choose(n, k))
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn sd(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

/// One task under one variant.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cell {
    pub n: u64,
    pub passes: u64,
    pub success: f64,
    pub pass_at_k: Option<f64>,
    pub pass_hat_k: Option<f64>,
    pub cost_total: f64,
    pub cost_per_success: Option<f64>,
    pub turns_per_success: Option<f64>,
    pub agent_min_per_success: Option<f64>,
    pub cost_mean: f64,
    pub turns_mean: f64,
    pub agent_secs_mean: f64,
}

/// A cell's numbers, `k` being the report's k.
pub fn cell(obs: &[Obs], k: u64) -> Cell {
    let n = obs.len() as u64;
    let passes = obs.iter().filter(|o| o.passed).count() as u64;
    let cost_total: f64 = obs.iter().map(|o| o.cost).sum();
    let turns_total: f64 = obs.iter().map(|o| o.turns).sum();
    let secs_total: f64 = obs.iter().map(|o| o.agent_secs).sum();
    let per_success = |total: f64| (passes > 0).then(|| total / passes as f64);
    Cell {
        n,
        passes,
        success: if n == 0 {
            0.0
        } else {
            passes as f64 / n as f64
        },
        pass_at_k: pass_at_k(n, passes, k),
        pass_hat_k: pass_hat_k(n, passes, k),
        cost_total,
        cost_per_success: per_success(cost_total),
        turns_per_success: per_success(turns_total),
        agent_min_per_success: per_success(secs_total / 60.0),
        cost_mean: mean(&obs.iter().map(|o| o.cost).collect::<Vec<_>>()),
        turns_mean: mean(&obs.iter().map(|o| o.turns).collect::<Vec<_>>()),
        agent_secs_mean: mean(&obs.iter().map(|o| o.agent_secs).collect::<Vec<_>>()),
    }
}

/// A metric two variants are compared on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Share of trials passing (difference in points).
    Success,
    /// Cost per trial (log ratio).
    Cost,
    /// Turns per trial (log ratio).
    Turns,
    /// Agent time per trial (log ratio).
    Time,
}

impl Metric {
    pub const ALL: [Metric; 4] = [Self::Success, Self::Cost, Self::Turns, Self::Time];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Cost => "cost",
            Self::Turns => "turns",
            Self::Time => "time",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|m| m.as_str() == s)
            .ok_or_else(|| format!("metric {s:?}: success, cost, turns, or time"))
    }

    fn value(self, o: &Obs) -> f64 {
        match self {
            Self::Success => f64::from(u8::from(o.passed)),
            Self::Cost => o.cost,
            Self::Turns => o.turns,
            Self::Time => o.agent_secs,
        }
    }

    /// Whether a larger value is better.
    fn higher_is_better(self) -> bool {
        matches!(self, Self::Success)
    }

    fn is_ratio(self) -> bool {
        !matches!(self, Self::Success)
    }
}

/// The verdict on one metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Better,
    Worse,
    Tie,
    NoInformation,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Better => "better",
            Self::Worse => "worse",
            Self::Tie => "tie",
            Self::NoInformation => "no information",
        }
    }
}

/// B against A on one metric.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub metric: Metric,
    /// Tasks both variants have trials on.
    pub tasks: usize,
    /// Macro means of the per-trial metric.
    pub a: f64,
    pub b: f64,
    /// The paired effect: points for success, percent for the rest.
    pub delta: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    /// The smallest effect this run could detect (same unit as `delta`).
    pub mde: f64,
    pub verdict: Verdict,
}

/// splitmix64: a small seeded generator (the bootstrap must replay).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx]
}

/// The per-task effect of B over A for `metric` (log ratio of means for
/// ratio metrics, a difference otherwise).
fn effect(metric: Metric, a: &[f64], b: &[f64]) -> Option<f64> {
    let (ma, mb) = (mean(a), mean(b));
    if metric.is_ratio() {
        (ma > 0.0 && mb > 0.0).then(|| (mb / ma).ln())
    } else {
        Some(mb - ma)
    }
}

fn unit(metric: Metric, x: f64) -> f64 {
    if metric.is_ratio() {
        100.0 * (x.exp() - 1.0)
    } else {
        100.0 * x
    }
}

/// Compare B with A on `metric`, paired by task. `by_task` maps each task
/// to (A's trials, B's trials); tasks missing either side are left out.
pub fn compare(
    metric: Metric,
    by_task: &BTreeMap<String, (Vec<Obs>, Vec<Obs>)>,
    seed: u64,
) -> Comparison {
    let pairs: Vec<(Vec<f64>, Vec<f64>)> = by_task
        .values()
        .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        .map(|(a, b)| {
            (
                a.iter().map(|o| metric.value(o)).collect(),
                b.iter().map(|o| metric.value(o)).collect(),
            )
        })
        .collect();
    let a_mean = mean(&pairs.iter().map(|(a, _)| mean(a)).collect::<Vec<_>>());
    let b_mean = mean(&pairs.iter().map(|(_, b)| mean(b)).collect::<Vec<_>>());
    let effects: Vec<f64> = pairs
        .iter()
        .filter_map(|(a, b)| effect(metric, a, b))
        .collect();
    let t = effects.len();
    let empty = Comparison {
        metric,
        tasks: t,
        a: a_mean,
        b: b_mean,
        delta: 0.0,
        ci_low: 0.0,
        ci_high: 0.0,
        mde: 0.0,
        verdict: Verdict::NoInformation,
    };
    if t == 0 {
        return empty;
    }
    // Saturated: every task all-pass or all-fail in both arms alike.
    if metric == Metric::Success
        && pairs.iter().all(|(a, b)| {
            let (ma, mb) = (mean(a), mean(b));
            (ma == 0.0 || ma == 1.0) && ma == mb
        })
    {
        return Comparison {
            delta: 0.0,
            ..empty
        };
    }
    let point = mean(&effects);
    let mut rng = Rng(seed ^ 0x5EED);
    let mut boots = Vec::with_capacity(RESAMPLES);
    let usable: Vec<&(Vec<f64>, Vec<f64>)> = pairs
        .iter()
        .filter(|(a, b)| effect(metric, a, b).is_some())
        .collect();
    for _ in 0..RESAMPLES {
        let mut sum = 0.0;
        let mut count = 0;
        for _ in 0..usable.len() {
            let (a, b) = usable[rng.below(usable.len())];
            let ra: Vec<f64> = (0..a.len()).map(|_| a[rng.below(a.len())]).collect();
            let rb: Vec<f64> = (0..b.len()).map(|_| b[rng.below(b.len())]).collect();
            if let Some(e) = effect(metric, &ra, &rb) {
                sum += e;
                count += 1;
            }
        }
        if count > 0 {
            boots.push(sum / f64::from(count));
        }
    }
    boots.sort_by(f64::total_cmp);
    let (lo, hi) = (percentile(&boots, 0.025), percentile(&boots, 0.975));
    let mde = Z_MDE * sd(&effects) / (t as f64).sqrt();
    let verdict = if lo > 0.0 || hi < 0.0 {
        let b_larger = point > 0.0;
        if b_larger == metric.higher_is_better() {
            Verdict::Better
        } else {
            Verdict::Worse
        }
    } else {
        Verdict::Tie
    };
    Comparison {
        metric,
        tasks: t,
        a: a_mean,
        b: b_mean,
        delta: unit(metric, point),
        ci_low: unit(metric, lo),
        ci_high: unit(metric, hi),
        mde: if metric.is_ratio() {
            100.0 * (mde.exp() - 1.0)
        } else {
            100.0 * mde
        },
        verdict,
    }
}

/// The mean within-task coefficient of variation of `value` (tasks with
/// two or more trials and a positive mean).
pub fn within_task_cv(
    by_task: &BTreeMap<String, Vec<Obs>>,
    value: impl Fn(&Obs) -> f64,
) -> Option<f64> {
    let cvs: Vec<f64> = by_task
        .values()
        .filter(|obs| obs.len() > 1)
        .filter_map(|obs| {
            let xs: Vec<f64> = obs.iter().map(&value).collect();
            let m = mean(&xs);
            (m > 0.0).then(|| sd(&xs) / m)
        })
        .collect();
    (!cvs.is_empty()).then(|| mean(&cvs))
}

/// The effect (percent) two variants run `k` times each on `tasks` tasks
/// can be told apart by, for a metric with within-task CV `cv` (paired
/// by task; the noise floor's planning formula).
pub fn planned_mde_pct(cv: f64, tasks: usize, k: u32) -> f64 {
    100.0 * Z_MDE * 2f64.sqrt() * cv / ((tasks as f64) * f64::from(k)).sqrt()
}

#[cfg(test)]
mod tests;
