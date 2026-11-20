//! A replay: every task of a frozen set, under every variant, k times,
//! run with bounded parallelism, paired, resumable, and capped by a
//! budget.
//!
//! * **Order.** Cells are interleaved: tasks in a seeded order, then for
//!   each task its k rounds, each round every variant (in a seeded order
//!   per round). A run stopped early is still paired task by task.
//! * **Parallelism.** `parallel` runners claim cells from the store (a
//!   lease each, so a runner that dies leaves its cell to the next) and run
//!   each as its own process (`fw eval trial --plan`), optionally under a
//!   hub lease (`eval:slot:<i>`, visible on the board).
//! * **Alone.** A task whose oracle fails beside other `fw` processes
//!   (`parallel_safe = false`, V5) runs with no other cell in flight: its
//!   in-trial gate and its grading see a quiet machine.
//! * **Budget.** Before a cell starts: spent (finished cells) + the priors
//!   of the cells in flight + this cell's prior must stay within the cap,
//!   else no further cell starts and the run pauses (`fw eval replay
//!   --resume RUN --budget USD` continues). A running trial is never cut.
//! * **Infra errors** are re-queued once; a second is recorded.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::estimate::Prior;
use crate::set::SetManifest;
use crate::store::{EvalStore, EvalStoreError, RunStatus, TrialRow, TrialStatus};
use crate::trial::TrialOutcome;

/// The order of `ids` a seed gives (sha256 of seed and id).
fn seeded<T: Clone>(items: &[T], key: impl Fn(&T) -> String, seed: u64) -> Vec<T> {
    let mut keyed: Vec<(String, T)> = items
        .iter()
        .map(|i| {
            (
                hex::encode(Sha256::digest(format!("{seed}:{}", key(i)).as_bytes())),
                i.clone(),
            )
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, i)| i).collect()
}

/// The trial id of a cell.
pub fn trial_id(task: &str, variant_hash: &str, k: u32) -> String {
    format!("{task}-{}-k{k}", &variant_hash[..variant_hash.len().min(8)])
}

/// Every cell of `tasks` x `variants` x `k`, interleaved (see the module
/// doc), as planned trial rows.
pub fn plan_cells(tasks: &[String], variants: &[String], k: u32, seed: u64) -> Vec<TrialRow> {
    let mut out = Vec::new();
    let mut ordinal = 0;
    for task in seeded(tasks, Clone::clone, seed) {
        for round in 0..k {
            let order = seeded(variants, |v| format!("{task}:{round}:{v}"), seed);
            for v in order {
                out.push(TrialRow::planned(
                    trial_id(&task, &v, round),
                    ordinal,
                    v.clone(),
                    task.clone(),
                    round,
                ));
                ordinal += 1;
            }
        }
    }
    out
}

/// Runs one cell; the production one spawns `fw eval trial --plan`.
pub trait CellRunner: Send + Sync {
    fn run(&self, trial: &TrialRow) -> Result<TrialOutcome, String>;
}

/// Why a drive stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum Stopped {
    /// Every cell ran.
    Done,
    /// The next cell would pass the budget.
    Budget {
        spent: f64,
        next_prior: f64,
        remaining: usize,
    },
    /// The caller asked to stop (quota, an operator).
    Halted(String),
}

/// One finished cell, as the drive reports it.
#[derive(Debug, Clone)]
pub struct Progress {
    pub row: TrialRow,
    pub done: usize,
    pub total: usize,
    pub spent: f64,
}

/// What the drive needs.
pub struct Drive<'a> {
    pub store: &'a EvalStore,
    pub run_id: &'a str,
    pub manifest: &'a SetManifest,
    pub priors: &'a BTreeMap<(String, String), Prior>,
    pub parallel: u32,
    pub budget_usd: Option<f64>,
    /// The runner name in claims (`host:pid`).
    pub worker: &'a str,
    pub runner: Arc<dyn CellRunner>,
    /// Asked before each cell: `Some(reason)` stops launching.
    pub halt: &'a (dyn Fn() -> Option<String> + Sync),
    pub progress: &'a (dyn Fn(&Progress) + Sync),
    pub now: &'a (dyn Fn() -> DateTime<Utc> + Sync),
}

#[derive(Default)]
struct Gate {
    normal: usize,
    alone_waiting: usize,
    alone_running: bool,
    in_flight_prior: f64,
    stopped: Option<Stopped>,
}

/// Cells that must run alone wait for the machine to empty, and nothing
/// starts while one waits or runs.
struct Floor {
    state: Mutex<Gate>,
    changed: Condvar,
}

impl Floor {
    fn lock(&self) -> std::sync::MutexGuard<'_, Gate> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn enter(&self, alone: bool) {
        let mut g = self.lock();
        if alone {
            g.alone_waiting += 1;
            while g.normal > 0 || g.alone_running {
                g = self
                    .changed
                    .wait(g)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            g.alone_waiting -= 1;
            g.alone_running = true;
        } else {
            while g.alone_waiting > 0 || g.alone_running {
                g = self
                    .changed
                    .wait(g)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            g.normal += 1;
        }
    }

    fn leave(&self, alone: bool) {
        let mut g = self.lock();
        if alone {
            g.alone_running = false;
        } else {
            g.normal -= 1;
        }
        self.changed.notify_all();
    }
}

fn spent(store: &EvalStore, run_id: &str) -> Result<f64, EvalStoreError> {
    Ok(store
        .trials(run_id)?
        .iter()
        .filter(|t| matches!(t.status(), TrialStatus::Done | TrialStatus::InfraError))
        .map(|t| t.cost_usd)
        .sum())
}

/// Run the run's remaining cells. Returns why it stopped; the run's status
/// is set to match.
pub fn drive(d: &Drive<'_>) -> Result<Stopped, EvalStoreError> {
    let alone: BTreeMap<String, bool> = d
        .manifest
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.parallel_safe == Some(false)))
        .collect();
    let floor = Floor {
        state: Mutex::new(Gate::default()),
        changed: Condvar::new(),
    };
    let total = d.store.trials(d.run_id)?.len();
    let requeued: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());
    let failure: Mutex<Option<EvalStoreError>> = Mutex::new(None);
    let lease = chrono::Duration::hours(6);
    std::thread::scope(|scope| {
        for _ in 0..d.parallel.max(1) {
            scope.spawn(|| {
                loop {
                    // Claim under the floor's lock, so the budget check and
                    // the in-flight sum move together.
                    let claimed = {
                        let mut g = floor.lock();
                        if g.stopped.is_some() {
                            return;
                        }
                        if let Some(reason) = (d.halt)() {
                            g.stopped = Some(Stopped::Halted(reason));
                            return;
                        }
                        let now = (d.now)();
                        let next = d.store.trials(d.run_id).map(|ts| {
                            ts.into_iter().find(|t| match t.status() {
                                TrialStatus::Pending => true,
                                TrialStatus::Running => t.lease_expires_at.is_none_or(|l| l <= now),
                                _ => false,
                            })
                        });
                        let next = match next {
                            Ok(Some(t)) => t,
                            Ok(None) => return,
                            Err(e) => {
                                *failure.lock().expect("lock") = Some(e);
                                g.stopped = Some(Stopped::Halted("store error".into()));
                                return;
                            }
                        };
                        let prior = d
                            .priors
                            .get(&(next.task_id.clone(), next.variant_hash.clone()))
                            .map_or(0.0, |p| p.cost_usd);
                        if let Some(budget) = d.budget_usd {
                            let so_far = spent(d.store, d.run_id).unwrap_or(f64::INFINITY);
                            if so_far + g.in_flight_prior + prior > budget {
                                let remaining = d
                                    .store
                                    .trials(d.run_id)
                                    .map(|ts| {
                                        ts.iter()
                                            .filter(|t| t.status() == TrialStatus::Pending)
                                            .count()
                                    })
                                    .unwrap_or(0);
                                g.stopped = Some(Stopped::Budget {
                                    spent: so_far,
                                    next_prior: prior,
                                    remaining,
                                });
                                return;
                            }
                        }
                        let target = next.trial_id.clone();
                        let pick = |t: &TrialRow| t.trial_id != target;
                        match d
                            .store
                            .claim_next(d.run_id, d.worker, now, now + lease, &pick)
                        {
                            Ok(Some(t)) => {
                                g.in_flight_prior += prior;
                                (t, prior)
                            }
                            // Taken by another runner meanwhile: look again.
                            Ok(None) => continue,
                            Err(e) => {
                                *failure.lock().expect("lock") = Some(e);
                                g.stopped = Some(Stopped::Halted("store error".into()));
                                return;
                            }
                        }
                    };
                    let (trial, prior) = claimed;
                    let solo = alone.get(&trial.task_id).copied().unwrap_or(false);
                    floor.enter(solo);
                    let outcome = d.runner.run(&trial);
                    floor.leave(solo);
                    let mut g = floor.lock();
                    g.in_flight_prior -= prior;
                    drop(g);
                    let recorded = match outcome {
                        Ok(outcome) => {
                            let mut row = outcome.row;
                            row.ordinal = trial.ordinal;
                            row.k = trial.k;
                            let infra = row.status() == TrialStatus::InfraError;
                            let retry = infra && {
                                let mut r = requeued.lock().expect("lock");
                                let n = r.entry(trial.trial_id.clone()).or_insert(0);
                                *n += 1;
                                *n == 1
                            };
                            d.store
                                .finish(d.run_id, d.worker, &row)
                                .and_then(|()| {
                                    d.store.put_checks(
                                        d.run_id,
                                        &trial.trial_id,
                                        "oracle",
                                        &outcome.checks,
                                    )
                                })
                                .and_then(|()| {
                                    d.store.put_artifacts(
                                        d.run_id,
                                        &trial.trial_id,
                                        &outcome.artifacts,
                                    )
                                })
                                .and_then(|()| {
                                    if retry {
                                        d.store.requeue(d.run_id, &trial.trial_id)
                                    } else {
                                        Ok(())
                                    }
                                })
                                .map(|()| row)
                        }
                        Err(why) => {
                            let mut row = trial.clone();
                            row.status = Some(TrialStatus::InfraError);
                            row.validity = Some("infra_error".into());
                            row.validity_detail = Some(why);
                            row.ended_at = Some((d.now)());
                            d.store.finish(d.run_id, d.worker, &row).map(|()| row)
                        }
                    };
                    match recorded {
                        Ok(row) => {
                            let trials = d.store.trials(d.run_id).unwrap_or_default();
                            let done = trials
                                .iter()
                                .filter(|t| {
                                    matches!(
                                        t.status(),
                                        TrialStatus::Done | TrialStatus::InfraError
                                    )
                                })
                                .count();
                            (d.progress)(&Progress {
                                row,
                                done,
                                total,
                                spent: trials.iter().map(|t| t.cost_usd).sum(),
                            });
                        }
                        Err(e) => {
                            *failure.lock().expect("lock") = Some(e);
                            floor.lock().stopped = Some(Stopped::Halted("store error".into()));
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = failure.into_inner().expect("lock") {
        return Err(e);
    }
    let stopped = floor
        .state
        .into_inner()
        .expect("lock")
        .stopped
        .unwrap_or(Stopped::Done);
    let pending = d
        .store
        .trials(d.run_id)?
        .iter()
        .any(|t| matches!(t.status(), TrialStatus::Pending | TrialStatus::Running));
    let stopped = if matches!(stopped, Stopped::Done) && pending {
        Stopped::Halted("cells remain (another runner holds them)".into())
    } else {
        stopped
    };
    match &stopped {
        Stopped::Done => d
            .store
            .set_run_status(d.run_id, RunStatus::Done, Some((d.now)()))?,
        _ => d.store.set_run_status(d.run_id, RunStatus::Paused, None)?,
    }
    Ok(stopped)
}

#[cfg(test)]
mod tests;
