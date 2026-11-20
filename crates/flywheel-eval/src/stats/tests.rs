use super::*;

fn obs(passed: bool, cost: f64) -> Obs {
    Obs {
        passed,
        cost,
        turns: cost * 10.0,
        agent_secs: cost * 100.0,
    }
}

#[test]
fn pass_at_k_and_pass_hat_k_are_unbiased() {
    assert_eq!(pass_at_k(3, 3, 3), Some(1.0));
    assert_eq!(pass_at_k(3, 0, 1), Some(0.0));
    assert!((pass_at_k(5, 2, 2).unwrap() - (1.0 - 3.0 / 10.0)).abs() < 1e-12);
    assert!((pass_hat_k(5, 3, 2).unwrap() - 3.0 / 10.0).abs() < 1e-12);
    assert_eq!(pass_hat_k(3, 3, 3), Some(1.0));
    assert_eq!(pass_at_k(2, 1, 3), None, "k beyond n");
}

#[test]
fn a_cell_counts_failed_trials_into_cost_per_success() {
    let c = cell(&[obs(true, 2.0), obs(false, 1.0), obs(true, 3.0)], 3);
    assert_eq!((c.n, c.passes), (3, 2));
    assert_eq!(c.cost_per_success, Some(3.0));
    assert!((c.success - 2.0 / 3.0).abs() < 1e-12);
    assert_eq!(cell(&[obs(false, 1.0)], 1).cost_per_success, None);
}

fn paired(pairs: &[(&[Obs], &[Obs])]) -> BTreeMap<String, (Vec<Obs>, Vec<Obs>)> {
    pairs
        .iter()
        .enumerate()
        .map(|(i, (a, b))| (format!("t{i}"), (a.to_vec(), b.to_vec())))
        .collect()
}

#[test]
fn a_clear_cost_cut_is_better_and_noise_is_a_tie() {
    // B costs 20% less on every task, with 3% noise: better.
    let mut tasks = Vec::new();
    for t in 0..10 {
        let base = 1.0 + f64::from(t);
        let a = vec![
            obs(true, base),
            obs(true, base * 1.03),
            obs(true, base * 0.97),
        ];
        let b = vec![
            obs(true, base * 0.8),
            obs(true, base * 0.82),
            obs(true, base * 0.78),
        ];
        tasks.push((a, b));
    }
    let by: BTreeMap<String, (Vec<Obs>, Vec<Obs>)> = tasks
        .iter()
        .enumerate()
        .map(|(i, (a, b))| (format!("t{i}"), (a.clone(), b.clone())))
        .collect();
    let cost = compare(Metric::Cost, &by, 1);
    assert_eq!(cost.verdict, Verdict::Better, "{cost:?}");
    assert!((cost.delta + 20.0).abs() < 1.0, "{cost:?}");
    assert!(cost.ci_high < 0.0);
    // Success saturated in both arms: nothing could have moved.
    assert_eq!(
        compare(Metric::Success, &by, 1).verdict,
        Verdict::NoInformation
    );

    // Differences that flip sign from task to task: a tie.
    let by = paired(&[
        (&[obs(true, 1.0)], &[obs(true, 1.1)]),
        (&[obs(true, 1.0)], &[obs(true, 0.9)]),
        (&[obs(true, 2.0)], &[obs(true, 2.1)]),
        (&[obs(true, 2.0)], &[obs(true, 1.9)]),
    ]);
    let tie = compare(Metric::Cost, &by, 1);
    assert_eq!(tie.verdict, Verdict::Tie, "{tie:?}");
    assert!(tie.mde > 0.0);
    assert_eq!(compare(Metric::Cost, &by, 1), tie, "the seed replays");
}

#[test]
fn success_differences_are_points_and_a_drop_is_worse() {
    let all = [obs(true, 1.0), obs(true, 1.0), obs(true, 1.0)];
    let none = [obs(false, 1.0), obs(false, 1.0), obs(false, 1.0)];
    let mut pairs: Vec<(&[Obs], &[Obs])> = Vec::new();
    for _ in 0..8 {
        pairs.push((&all, &none));
    }
    let by = paired(&pairs);
    let s = compare(Metric::Success, &by, 3);
    assert_eq!(s.verdict, Verdict::Worse);
    assert!((s.delta + 100.0).abs() < 1e-9);
}

/// The noise floor's 30 trials (tmp/noise-floor/results.json): the same
/// per-task cost per success, within-task CVs, and planned MDEs.
#[test]
fn the_noise_floor_numbers_come_out_the_same() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/noise_floor.json")).unwrap();
    let mut by_task: BTreeMap<String, Vec<Obs>> = BTreeMap::new();
    for t in fixture["trials"].as_array().unwrap() {
        by_task
            .entry(t["task"].as_str().unwrap().to_owned())
            .or_default()
            .push(Obs {
                passed: t["oracle_pass"].as_bool().unwrap(),
                cost: t["cost_usd"].as_f64().unwrap(),
                turns: t["turns"].as_f64().unwrap(),
                agent_secs: t["agent_seconds"].as_f64().unwrap(),
            });
    }
    let expected = &fixture["expected"];
    assert_eq!(by_task.len() as u64, expected["tasks"].as_u64().unwrap());
    for (task, obs) in &by_task {
        let c = cell(obs, 3);
        let want = &expected["per_task"][task];
        assert_eq!(c.passes, want["successes"].as_u64().unwrap(), "{task}");
        let cps = c.cost_per_success.unwrap();
        assert!(
            (cps - want["cost_per_success"].as_f64().unwrap()).abs() < 0.006,
            "{task}: {cps}"
        );
        assert_eq!(c.pass_hat_k, Some(1.0));
    }
    let total: f64 = by_task.values().flatten().map(|o| o.cost).sum();
    let passes = by_task.values().flatten().filter(|o| o.passed).count() as f64;
    assert!((total / passes - expected["cost_per_success"].as_f64().unwrap()).abs() < 0.006);
    for (field, value) in [
        (
            "cost_usd",
            Box::new(|o: &Obs| o.cost) as Box<dyn Fn(&Obs) -> f64>,
        ),
        ("turns", Box::new(|o: &Obs| o.turns)),
        ("agent_seconds", Box::new(|o: &Obs| o.agent_secs)),
    ] {
        let cv = within_task_cv(&by_task, value).unwrap();
        let want = &expected["continuous"][field];
        assert!(
            (cv - want["cv_mean_within_task"].as_f64().unwrap()).abs() < 0.0006,
            "{field}: {cv}"
        );
        for (k, key) in [(1, "mde_pct_k1"), (3, "mde_pct_k3")] {
            let mde = planned_mde_pct(cv, by_task.len(), k);
            assert!(
                (mde - want[key].as_f64().unwrap()).abs() < 0.06,
                "{field} k={k}: {mde}"
            );
        }
    }
}
