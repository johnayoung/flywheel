use super::*;
use crate::set::{Provenance, SetTask, Source, Split};
use crate::store::RunStatus;
use chrono::Utc;

fn task(id: &str, kind: TaskKind, tier: Tier) -> SetTask {
    SetTask {
        id: id.into(),
        source: Source::Landed,
        tier,
        split: Split::Dev,
        kind,
        base: "b".into(),
        reference: "r".into(),
        files_digest: String::new(),
        parallel_safe: Some(true),
        prior_cost_usd: None,
        oracle_secs: None,
        prerequisites: BTreeMap::new(),
        provenance: Provenance::default(),
    }
}

fn t(
    task: &str,
    v: &str,
    k: u32,
    oracle: &str,
    validity: Option<&str>,
    cost: f64,
    turns: u32,
) -> TrialRow {
    TrialRow {
        trial_id: format!("{task}-{v}-k{k}"),
        task_id: task.into(),
        variant_hash: v.into(),
        k,
        status: Some(TrialStatus::Done),
        oracle: Some(oracle.into()),
        validity: validity.map(str::to_owned),
        cost_usd: cost,
        turns,
        trace_json: "{}".into(),
        ..TrialRow::default()
    }
}

fn run() -> RunRow {
    RunRow {
        run_id: "r".into(),
        set_name: "core".into(),
        set_version: 1,
        set_hash: "h".into(),
        k: 2,
        seed: 4,
        primary_metrics: "success,cost".into(),
        budget_usd: Some(50.0),
        pipeline: "full".into(),
        parallel: 3,
        status: RunStatus::Done,
        started_at: Utc::now(),
        ended_at: None,
        analysis_md_hash: None,
        options_json: "{}".into(),
    }
}

fn variants() -> Vec<RunVariantRow> {
    vec![
        RunVariantRow {
            ordinal: 0,
            variant_hash: "ha".into(),
            label: "rule-on".into(),
        },
        RunVariantRow {
            ordinal: 1,
            variant_hash: "hb".into(),
            label: "rule-off".into(),
        },
    ]
}

#[test]
fn the_report_counts_contamination_as_failure_and_keeps_dont_act_apart() {
    let manifest = SetManifest {
        name: "core".into(),
        version: Some(1),
        project_id: "p".into(),
        repo: "/r".into(),
        created_at: Utc::now(),
        tasks: vec![
            task("t1", TaskKind::Build, Tier::Regression),
            task("t2", TaskKind::Build, Tier::Discriminating),
            task("t1-dont-act", TaskKind::DontAct, Tier::Uncalibrated),
        ],
        hash: Some("h".into()),
    };
    let trials = vec![
        t("t1", "ha", 0, "pass", Some("valid"), 2.0, 30),
        t("t1", "ha", 1, "pass", Some("valid"), 2.2, 32),
        t("t1", "hb", 0, "pass", Some("valid"), 3.0, 40),
        t("t1", "hb", 1, "pass", Some("contaminated"), 1.0, 5),
        t("t2", "ha", 0, "fail", Some("valid"), 4.0, 60),
        t("t2", "ha", 1, "pass", Some("suspect"), 4.0, 50),
        t("t2", "hb", 0, "fail", Some("valid"), 5.0, 70),
        t("t2", "hb", 1, "fail", Some("valid"), 5.0, 70),
        t("t1-dont-act", "ha", 0, "pass", Some("valid"), 0.5, 10),
        t("t1-dont-act", "hb", 0, "fail", Some("valid"), 0.8, 12),
    ];
    let r = build(&run(), &variants(), &trials, &manifest, &Filter::default()).unwrap();
    let t1 = r.tasks.iter().find(|t| t.task == "t1").unwrap();
    assert_eq!(
        t1.cells["rule-off"].passes, 1,
        "the contaminated pass is a fail"
    );
    assert_eq!(t1.marks["rule-off"][1].mark, "contaminated");
    let t2 = r.tasks.iter().find(|t| t.task == "t2").unwrap();
    assert_eq!(
        t2.cells["rule-on"].passes, 1,
        "a suspect pass counts until decided"
    );
    let on = r.overall.iter().find(|o| o.variant == "rule-on").unwrap();
    assert_eq!(
        on.tasks, 2,
        "don't-act tasks are not in the success average"
    );
    assert!((on.success - 0.75).abs() < 1e-9);
    assert_eq!(on.abstain_accuracy, Some(1.0));
    assert_eq!(r.overall[1].abstain_accuracy, Some(0.0));
    assert!(r.saturated_tasks.is_empty());
    let c = &r.comparisons[0];
    assert_eq!(
        (c.baseline.as_str(), c.variant.as_str()),
        ("rule-on", "rule-off")
    );
    assert_eq!(c.primary, vec![Metric::Success, Metric::Cost]);
    assert_eq!(c.comparisons.len(), 4);
    assert_eq!(r.worth_a_read["rule-on"][0], "t1-dont-act-ha-k0");
    let text = render(&r, true);
    assert!(
        text.contains("rule-off vs rule-on (paired by task):"),
        "{text}"
    );
    assert!(text.contains("*success"), "{text}");
    assert!(text.contains("contaminated"), "{text}");
    // Tier filter and baseline choice.
    let only = Filter {
        tiers: Some([Tier::Regression].into()),
        baseline: Some("rule-off".into()),
    };
    let r = build(&run(), &variants(), &trials, &manifest, &only).unwrap();
    assert_eq!(r.tasks.len(), 1);
    assert_eq!(r.comparisons[0].baseline, "rule-off");
    assert!(
        build(
            &run(),
            &variants(),
            &trials,
            &manifest,
            &Filter {
                baseline: Some("x".into()),
                ..Filter::default()
            }
        )
        .is_err()
    );
}
