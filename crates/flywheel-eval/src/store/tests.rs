use super::*;
use chrono::{Duration, TimeZone};

fn t(s: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap() + Duration::seconds(s)
}

fn run(id: &str) -> RunRow {
    RunRow {
        run_id: id.to_owned(),
        set_name: "core".into(),
        set_version: 1,
        set_hash: "h".into(),
        k: 2,
        seed: 7,
        primary_metrics: "success,cost".into(),
        budget_usd: Some(25.0),
        pipeline: "full".into(),
        parallel: 3,
        status: RunStatus::Running,
        started_at: t(0),
        ended_at: None,
        analysis_md_hash: None,
        options_json: "{}".into(),
    }
}

fn seeded(store: &EvalStore, id: &str) {
    let cells: Vec<TrialRow> = (0..4)
        .map(|i| {
            TrialRow::planned(
                format!("c{i}"),
                i,
                if i % 2 == 0 { "va" } else { "vb" }.into(),
                "task-a".into(),
                i / 2,
            )
        })
        .collect();
    store
        .create_run(
            &run(id),
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
}

#[test]
fn run_round_trips_with_its_cells_in_launch_order() {
    let store = EvalStore::in_memory("p1").unwrap();
    seeded(&store, "r1");
    assert_eq!(store.run("r1").unwrap(), run("r1"));
    assert_eq!(store.run_variants("r1").unwrap().len(), 2);
    let trials = store.trials("r1").unwrap();
    let ids: Vec<&str> = trials.iter().map(|t| t.trial_id.as_str()).collect();
    assert_eq!(ids, ["c0", "c1", "c2", "c3"]);
    assert!(trials.iter().all(|t| t.status() == TrialStatus::Pending));
    assert!(matches!(
        store.run("nope"),
        Err(EvalStoreError::NotFound(_))
    ));
}

#[test]
fn claims_go_in_order_skip_what_the_caller_skips_and_take_over_lapsed_leases() {
    let store = EvalStore::in_memory("p1").unwrap();
    seeded(&store, "r1");
    let none = |_: &TrialRow| false;
    let a = store
        .claim_next("r1", "w1", t(0), t(60), &none)
        .unwrap()
        .unwrap();
    assert_eq!(a.trial_id, "c0");
    let skip_c1 = |t: &TrialRow| t.trial_id == "c1";
    let b = store
        .claim_next("r1", "w2", t(1), t(61), &skip_c1)
        .unwrap()
        .unwrap();
    assert_eq!(b.trial_id, "c2");
    // w1's lease lapses: another runner takes c0 over; w1 can no longer finish it.
    let c = store
        .claim_next("r1", "w3", t(120), t(180), &skip_c1)
        .unwrap()
        .unwrap();
    assert_eq!(c.trial_id, "c0");
    let mut done = a.clone();
    done.status = Some(TrialStatus::Done);
    assert!(matches!(
        store.finish("r1", "w1", &done),
        Err(EvalStoreError::Conflict(_))
    ));
    let mut done = c.clone();
    done.status = Some(TrialStatus::Done);
    done.oracle = Some("pass".into());
    done.cost_usd = 2.5;
    done.turns = 31;
    done.ended_at = Some(t(200));
    store.finish("r1", "w3", &done).unwrap();
    let got = store.trial("r1", "c0").unwrap();
    assert_eq!(got.status(), TrialStatus::Done);
    assert_eq!(got.oracle.as_deref(), Some("pass"));
    assert_eq!(got.turns, 31);
    assert!(got.lease_expires_at.is_none());
    // Renew only for the holder; release puts the cell back.
    assert!(store.renew("r1", "c2", "w2", t(300)).unwrap());
    assert!(!store.renew("r1", "c2", "w1", t(300)).unwrap());
    store.release("r1", "c2", "w2").unwrap();
    assert_eq!(
        store.trial("r1", "c2").unwrap().status(),
        TrialStatus::Pending
    );
}

#[test]
fn partitions_do_not_see_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flywheel.sqlite");
    let a = EvalStore::open_sqlite(&path, "p1").unwrap();
    let b = EvalStore::open_sqlite(&path, "p2").unwrap();
    seeded(&a, "r1");
    assert!(b.runs().unwrap().is_empty());
    assert_eq!(a.runs().unwrap().len(), 1);
}

#[test]
fn a_set_version_freezes_once() {
    let store = EvalStore::in_memory("p1").unwrap();
    let set = SetRow {
        name: "core".into(),
        version: 1,
        hash: "abc".into(),
        created_at: t(0),
        task_count: 10,
        path: "/x".into(),
    };
    store.put_set(&set).unwrap();
    assert!(matches!(
        store.put_set(&set),
        Err(EvalStoreError::Conflict(_))
    ));
    assert_eq!(store.sets().unwrap(), vec![set]);
}

#[test]
fn checks_reviews_and_artifacts_replace_per_trial() {
    let store = EvalStore::in_memory("p1").unwrap();
    seeded(&store, "r1");
    let check = |id: &str, passed: bool| CheckRow {
        trial_id: "c0".into(),
        source: "oracle".into(),
        check_id: id.into(),
        passed,
        note: String::new(),
    };
    store
        .put_checks(
            "r1",
            "c0",
            "oracle",
            &[check("c1", true), check("c2", false)],
        )
        .unwrap();
    store
        .put_checks("r1", "c0", "oracle", &[check("c1", false)])
        .unwrap();
    assert_eq!(store.checks("r1").unwrap(), vec![check("c1", false)]);

    let review = ReviewRow {
        trial_id: "c0".into(),
        reader_model: "haiku".into(),
        verdict: "fake_pass".into(),
        confidence: "high".into(),
        evidence_json: "[]".into(),
        notes: "edited the test".into(),
        operator_decision: None,
        decided_at: None,
        reviewed_at: t(5),
    };
    store.put_review("r1", &review).unwrap();
    store.decide_review("r1", "c0", "confirmed", t(9)).unwrap();
    let got = store.reviews("r1").unwrap();
    assert_eq!(got[0].operator_decision.as_deref(), Some("confirmed"));
    assert!(store.decide_review("r1", "c3", "confirmed", t(9)).is_err());

    let art = |n: &str| ArtifactRow {
        name: n.into(),
        sha256: "s".into(),
        bytes: 3,
    };
    store
        .put_artifacts("r1", "c0", &[art("a"), art("b")])
        .unwrap();
    store.put_artifacts("r1", "c0", &[art("c")]).unwrap();
    assert_eq!(store.artifacts("r1", "c0").unwrap(), vec![art("c")]);
}

#[test]
fn a_store_at_another_eval_schema_is_refused_naming_the_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sqlite");
    drop(EvalStore::open_sqlite(&path, "p").unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("UPDATE eval_schema_version SET version = 99")
        .unwrap();
    drop(conn);
    let err = EvalStore::open_sqlite(&path, "p").unwrap_err().to_string();
    assert!(
        err.contains("schema is 99") && err.contains("fw db upgrade"),
        "{err}"
    );
}

#[test]
fn sqlite_and_postgres_ddl_declare_the_same_tables_and_columns() {
    fn shape(sql: &str) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        let mut current: Option<(String, Vec<String>)> = None;
        for line in sql.lines().map(str::trim) {
            if let Some(rest) = line.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
                current = Some((rest.trim_end_matches(" (").to_owned(), Vec::new()));
            } else if line.starts_with(");") {
                out.extend(current.take());
            } else if let Some((_, cols)) = current.as_mut()
                && let Some(name) = line.split_whitespace().next()
                && !line.starts_with("PRIMARY")
                && !line.starts_with("--")
            {
                cols.push(name.to_owned());
            }
        }
        out
    }
    let lite = shape(SCHEMA_SQL);
    assert_eq!(lite, shape(SCHEMA_SQL_POSTGRES));
    let mut tables: Vec<&str> = lite.iter().map(|(t, _)| t.as_str()).collect();
    tables.retain(|t| *t != EVAL_VERSION_TABLE);
    assert_eq!(tables, EVAL_TABLES);
}
