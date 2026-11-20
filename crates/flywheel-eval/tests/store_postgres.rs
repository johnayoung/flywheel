//! The eval store on Postgres: the same rows and claim rules as SQLite.
//! Runs against the throwaway container from
//! `flywheel-core/tests/support/pg_container.rs`; skips without Docker.

#[path = "../../flywheel-core/tests/support/pg_container.rs"]
mod pg_container;

use chrono::{Duration, TimeZone, Utc};
use flywheel_eval::store::{
    CheckRow, EvalStore, RunRow, RunStatus, RunVariantRow, SetRow, TrialRow, TrialStatus,
};
use pg_container::Postgres;

fn t(s: i64) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap() + Duration::seconds(s)
}

#[test]
fn postgres_store_round_trips_runs_claims_and_scores() {
    let Some(pg) = Postgres::acquire() else {
        return;
    };
    let schema = pg.fresh_schema("eval");
    let store = EvalStore::connect_postgres(&pg.dsn(), &schema, "p1").unwrap();
    let other = EvalStore::connect_postgres(&pg.dsn(), &schema, "p2").unwrap();
    store
        .put_set(&SetRow {
            name: "core".into(),
            version: 1,
            hash: "h".into(),
            created_at: t(0),
            task_count: 2,
            path: "/s".into(),
        })
        .unwrap();
    let run = RunRow {
        run_id: "r1".into(),
        set_name: "core".into(),
        set_version: 1,
        set_hash: "h".into(),
        k: 1,
        seed: 3,
        primary_metrics: "cost".into(),
        budget_usd: None,
        pipeline: "full".into(),
        parallel: 2,
        status: RunStatus::Running,
        started_at: t(0),
        ended_at: None,
        analysis_md_hash: None,
        options_json: "{}".into(),
    };
    store
        .create_run(
            &run,
            &[RunVariantRow {
                ordinal: 0,
                variant_hash: "va".into(),
                label: "A".into(),
            }],
            &[
                TrialRow::planned("c0".into(), 0, "va".into(), "t1".into(), 0),
                TrialRow::planned("c1".into(), 1, "va".into(), "t2".into(), 0),
            ],
        )
        .unwrap();
    assert_eq!(store.run("r1").unwrap(), run);
    assert!(other.runs().unwrap().is_empty());
    let none = |_: &TrialRow| false;
    let mut claimed = store
        .claim_next("r1", "w", t(1), t(60), &none)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.trial_id, "c0");
    claimed.status = Some(TrialStatus::Done);
    claimed.cost_usd = 1.25;
    claimed.oracle = Some("pass".into());
    store.finish("r1", "w", &claimed).unwrap();
    let got = store.trial("r1", "c0").unwrap();
    assert_eq!((got.status(), got.cost_usd), (TrialStatus::Done, 1.25));
    store
        .put_checks(
            "r1",
            "c0",
            "oracle",
            &[CheckRow {
                trial_id: "c0".into(),
                source: "oracle".into(),
                check_id: "c14".into(),
                passed: false,
                note: "n".into(),
            }],
        )
        .unwrap();
    assert_eq!(store.checks("r1").unwrap().len(), 1);
    // Reopening a current schema is a no-op.
    drop(EvalStore::connect_postgres(&pg.dsn(), &schema, "p1").unwrap());
}
