use super::*;
use crate::set::{Provenance, SetTask, Source, Split, TaskKind, Tier};
use crate::store::{RunRow, RunVariantRow};
use std::sync::atomic::{AtomicUsize, Ordering};

fn task(id: &str, safe: bool) -> SetTask {
    SetTask {
        id: id.into(),
        source: Source::Landed,
        tier: Tier::Uncalibrated,
        split: Split::Dev,
        kind: TaskKind::Build,
        base: "b".into(),
        reference: "r".into(),
        files_digest: String::new(),
        parallel_safe: Some(safe),
        prior_cost_usd: Some(1.0),
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
        hash: Some("h".into()),
    }
}

struct Fake {
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    alone_violations: AtomicUsize,
    infra_once: Mutex<Vec<String>>,
    alone: Vec<String>,
}

impl CellRunner for Fake {
    fn run(&self, trial: &TrialRow) -> Result<TrialOutcome, String> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        if self.alone.contains(&trial.task_id) && now != 1 {
            self.alone_violations.fetch_add(1, Ordering::SeqCst);
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut row = trial.clone();
        let mut infra = self.infra_once.lock().unwrap();
        if let Some(i) = infra.iter().position(|t| *t == trial.trial_id) {
            infra.remove(i);
            row.status = Some(TrialStatus::InfraError);
        } else {
            row.status = Some(TrialStatus::Done);
            row.oracle = Some("pass".into());
            row.cost_usd = 1.0;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(TrialOutcome {
            row,
            checks: Vec::new(),
            artifacts: Vec::new(),
            dir: "/tmp".into(),
        })
    }
}

fn setup(m: &SetManifest, k: u32) -> (EvalStore, BTreeMap<(String, String), Prior>) {
    let store = EvalStore::in_memory("p").unwrap();
    let tasks: Vec<String> = m.tasks.iter().map(|t| t.id.clone()).collect();
    let variants = vec!["va".to_owned(), "vb".to_owned()];
    let cells = plan_cells(&tasks, &variants, k, 7);
    store
        .create_run(
            &RunRow {
                run_id: "r".into(),
                set_name: "core".into(),
                set_version: 1,
                set_hash: "h".into(),
                k,
                seed: 7,
                primary_metrics: String::new(),
                budget_usd: None,
                pipeline: "full".into(),
                parallel: 3,
                status: RunStatus::Running,
                started_at: Utc::now(),
                ended_at: None,
                analysis_md_hash: None,
                options_json: "{}".into(),
            },
            &[
                RunVariantRow {
                    ordinal: 0,
                    variant_hash: "va".into(),
                    label: "A".into(),
                },
                RunVariantRow {
                    ordinal: 1,
                    variant_hash: "vb".into(),
                    label: "B".into(),
                },
            ],
            &cells,
        )
        .unwrap();
    let priors = crate::estimate::priors(m, &variants, &crate::estimate::History::default());
    (store, priors)
}

fn fake(alone: &[&str], infra: &[String]) -> Arc<Fake> {
    Arc::new(Fake {
        in_flight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        alone_violations: AtomicUsize::new(0),
        infra_once: Mutex::new(infra.to_vec()),
        alone: alone.iter().map(|s| (*s).to_owned()).collect(),
    })
}

fn go(
    store: &EvalStore,
    m: &SetManifest,
    priors: &BTreeMap<(String, String), Prior>,
    parallel: u32,
    budget: Option<f64>,
    runner: Arc<Fake>,
) -> Stopped {
    let halt = || None;
    let progress = |_: &Progress| {};
    let now = Utc::now;
    drive(&Drive {
        store,
        run_id: "r",
        manifest: m,
        priors,
        parallel,
        budget_usd: budget,
        worker: "w",
        runner,
        halt: &halt,
        progress: &progress,
        now: &now,
    })
    .unwrap()
}

#[test]
fn cells_interleave_and_every_round_holds_every_variant() {
    let tasks = vec!["t1".to_owned(), "t2".to_owned()];
    let variants = vec!["va".to_owned(), "vb".to_owned()];
    let cells = plan_cells(&tasks, &variants, 3, 9);
    assert_eq!(cells.len(), 12);
    for pair in cells.chunks(2) {
        assert_eq!(pair[0].task_id, pair[1].task_id);
        assert_eq!(pair[0].k, pair[1].k);
        assert_ne!(pair[0].variant_hash, pair[1].variant_hash);
    }
    assert_eq!(
        plan_cells(&tasks, &variants, 3, 9),
        cells,
        "the seed decides"
    );
    let ords: Vec<u32> = cells.iter().map(|c| c.ordinal).collect();
    assert_eq!(ords, (0..12).collect::<Vec<_>>());
}

#[test]
fn every_cell_runs_within_the_parallelism_and_lone_oracles_run_alone() {
    let m = manifest(vec![
        task("t1", true),
        task("t2", true),
        task("solo", false),
    ]);
    let (store, priors) = setup(&m, 2);
    let runner = fake(&["solo"], &[]);
    assert_eq!(
        go(&store, &m, &priors, 3, None, Arc::clone(&runner)),
        Stopped::Done
    );
    let trials = store.trials("r").unwrap();
    assert!(trials.iter().all(|t| t.status() == TrialStatus::Done));
    assert!(runner.peak.load(Ordering::SeqCst) <= 3);
    assert!(
        runner.peak.load(Ordering::SeqCst) >= 2,
        "it ran in parallel"
    );
    assert_eq!(runner.alone_violations.load(Ordering::SeqCst), 0);
    assert_eq!(store.run("r").unwrap().status, RunStatus::Done);
}

#[test]
fn the_budget_pauses_before_passing_the_cap_and_a_resume_finishes() {
    let m = manifest(vec![task("t1", true), task("t2", true)]);
    let (store, priors) = setup(&m, 1);
    let stopped = go(&store, &m, &priors, 1, Some(2.5), fake(&[], &[]));
    assert_eq!(
        stopped,
        Stopped::Budget {
            spent: 2.0,
            next_prior: 1.0,
            remaining: 2
        }
    );
    assert_eq!(store.run("r").unwrap().status, RunStatus::Paused);
    let done = store
        .trials("r")
        .unwrap()
        .iter()
        .filter(|t| t.status() == TrialStatus::Done)
        .count();
    assert_eq!(done, 2);
    assert_eq!(
        go(&store, &m, &priors, 2, Some(10.0), fake(&[], &[])),
        Stopped::Done
    );
    assert_eq!(
        store
            .trials("r")
            .unwrap()
            .iter()
            .filter(|t| t.status() == TrialStatus::Done)
            .count(),
        4
    );
}

#[test]
fn an_infra_error_is_retried_once() {
    let m = manifest(vec![task("t1", true)]);
    let (store, priors) = setup(&m, 1);
    let first = store.trials("r").unwrap()[0].trial_id.clone();
    go(
        &store,
        &m,
        &priors,
        1,
        None,
        fake(&[], std::slice::from_ref(&first)),
    );
    assert_eq!(
        store.trial("r", &first).unwrap().status(),
        TrialStatus::Done
    );
    go(&store, &m, &priors, 1, None, fake(&[], &[]));
    let (store, priors) = setup(&m, 1);
    go(
        &store,
        &m,
        &priors,
        1,
        None,
        fake(&[], &[first.clone(), first.clone()]),
    );
    assert_eq!(
        store.trial("r", &first).unwrap().status(),
        TrialStatus::InfraError
    );
}
