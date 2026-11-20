use super::*;
use crate::testutil::TempRepo;
use chrono::TimeZone;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

fn task(id: &str, base: &str, reference: &str) -> SetTask {
    SetTask {
        id: id.into(),
        source: Source::Landed,
        tier: Tier::Uncalibrated,
        split: Split::Dev,
        kind: TaskKind::Build,
        base: base.into(),
        reference: reference.into(),
        files_digest: String::new(),
        parallel_safe: None,
        prior_cost_usd: Some(2.0),
        oracle_secs: None,
        provenance: Provenance::default(),
    }
}

/// A draft with two tasks (files written) in a repo with two commits.
fn draft(evals: &Path, repo: &TempRepo) -> (SetDir, String, String) {
    let base = repo.commit("base", &[("a.txt", "1")]);
    let reference = repo.commit("ref", &[("a.txt", "2")]);
    let dir = SetDir::draft(evals, "core");
    let manifest = SetManifest {
        name: "core".into(),
        version: None,
        project_id: "p1".into(),
        repo: repo.path().to_owned(),
        created_at: now(),
        tasks: vec![task("t1", &base, &reference), task("t2", &base, &reference)],
        hash: None,
    };
    dir.save(&manifest).unwrap();
    for id in ["t1", "t2"] {
        write_json(
            &dir.task_file(id),
            &serde_json::json!({"id": id, "goal": "g"}),
        )
        .unwrap();
        std::fs::create_dir_all(dir.oracle_dir(id)).unwrap();
        std::fs::write(dir.oracle_dir(id).join("oracle.sh"), "exit 0\n").unwrap();
    }
    (dir, base, reference)
}

fn prove(dir: &SetDir, id: &str, base: &str, reference: &str, passed: bool) {
    dir.save_proof(&Proof {
        task_id: id.into(),
        passed,
        files_digest: files_digest(dir, id).unwrap(),
        base: base.into(),
        reference: reference.into(),
        parallel_safe: true,
        oracle_secs: 1.5,
        checks: Vec::new(),
        at: now(),
    })
    .unwrap();
}

#[test]
fn set_refs_parse_and_print() {
    assert_eq!(SetRef::parse("core").unwrap().to_string(), "core");
    assert_eq!(
        SetRef::parse("core@v3").unwrap().version,
        Some(VersionRef::V(3))
    );
    assert_eq!(
        SetRef::parse("core@draft").unwrap().to_string(),
        "core@draft"
    );
    assert!(SetRef::parse("core@v0").is_err());
    assert!(SetRef::parse("Core").is_err());
    assert!(SetRef::parse("../x").is_err());
}

#[test]
fn the_hash_ignores_order_and_time_but_not_content() {
    let mut a = SetManifest {
        name: "core".into(),
        version: Some(1),
        project_id: "p".into(),
        repo: "/r".into(),
        created_at: now(),
        tasks: vec![task("t1", "b", "r"), task("t2", "b", "r")],
        hash: None,
    };
    let h = set_hash(&a);
    a.tasks.reverse();
    a.created_at = now() + chrono::Duration::days(1);
    a.version = Some(2);
    assert_eq!(set_hash(&a), h);
    a.tasks[0].files_digest = "x".into();
    assert_ne!(set_hash(&a), h);
}

#[test]
fn freeze_refuses_unproven_failed_and_edited_tasks() {
    let repo = TempRepo::new();
    let evals = tempfile::tempdir().unwrap();
    let store = EvalStore::in_memory("p1").unwrap();
    let (dir, base, reference) = draft(evals.path(), &repo);
    let err = freeze(
        evals.path(),
        "core",
        &store,
        SplitRatios::default(),
        1,
        now(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("t1 (not validated)"), "{err}");
    prove(&dir, "t1", &base, &reference, true);
    prove(&dir, "t2", &base, &reference, false);
    let err = freeze(
        evals.path(),
        "core",
        &store,
        SplitRatios::default(),
        1,
        now(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("t2 (failed validation)"), "{err}");
    prove(&dir, "t2", &base, &reference, true);
    std::fs::write(dir.oracle_dir("t2").join("oracle.sh"), "exit 1\n").unwrap();
    let err = freeze(
        evals.path(),
        "core",
        &store,
        SplitRatios::default(),
        1,
        now(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("t2 (changed since"), "{err}");
}

#[test]
fn freeze_pins_refs_indexes_the_version_and_counts_up() {
    let repo = TempRepo::new();
    let evals = tempfile::tempdir().unwrap();
    let store = EvalStore::in_memory("p1").unwrap();
    let (dir, base, reference) = draft(evals.path(), &repo);
    prove(&dir, "t1", &base, &reference, true);
    prove(&dir, "t2", &base, &reference, true);
    let v1 = freeze(
        evals.path(),
        "core",
        &store,
        SplitRatios::default(),
        1,
        now(),
    )
    .unwrap();
    assert_eq!(v1.version, Some(1));
    assert!(
        v1.tasks
            .iter()
            .all(|t| t.parallel_safe == Some(true) && !t.files_digest.is_empty())
    );
    let frozen = SetDir::frozen(evals.path(), "core", 1);
    assert_eq!(frozen.load().unwrap(), v1);
    assert!(
        std::fs::metadata(frozen.manifest())
            .unwrap()
            .permissions()
            .readonly()
    );
    assert_eq!(
        repo.git(&["rev-parse", &pin_ref("core", 1, "t1", "base")]),
        base
    );
    assert_eq!(
        repo.git(&["rev-parse", &pin_ref("core", 1, "t2", "ref")]),
        reference
    );
    assert_eq!(store.sets().unwrap()[0].hash, v1.hash.clone().unwrap());

    let v2 = freeze(
        evals.path(),
        "core",
        &store,
        SplitRatios::default(),
        1,
        now(),
    )
    .unwrap();
    assert_eq!(v2.version, Some(2));
    assert_eq!(v2.hash, v1.hash, "same content, same hash");
    assert_eq!(
        resolve(evals.path(), &SetRef::parse("core").unwrap()).unwrap(),
        SetDir::frozen(evals.path(), "core", 2)
    );
    assert!(diff(&v1, &v2).is_empty());
}

#[test]
fn splits_are_deterministic_and_follow_the_ratios() {
    let mut tasks: Vec<SetTask> = (0..10).map(|i| task(&format!("t{i}"), "b", "r")).collect();
    let ratios = SplitRatios::parse("dev=.6,val=.2,test=.2").unwrap();
    assign_splits(&mut tasks, ratios, 42);
    let count = |s: Split, ts: &[SetTask]| ts.iter().filter(|t| t.split == s).count();
    assert_eq!(
        (
            count(Split::Dev, &tasks),
            count(Split::Val, &tasks),
            count(Split::Test, &tasks)
        ),
        (6, 2, 2)
    );
    let mut again = tasks.clone();
    assign_splits(&mut again, ratios, 42);
    assert_eq!(again, tasks);
    assert!(SplitRatios::parse("dev=.5").is_err());
    assert!(SplitRatios::parse("prod=1").is_err());
}

#[test]
fn tiers_follow_the_baseline_pass_count() {
    assert_eq!(Tier::of(0, 3), Tier::Frontier);
    assert_eq!(Tier::of(1, 3), Tier::Discriminating);
    assert_eq!(Tier::of(3, 3), Tier::Regression);
    assert_eq!(Tier::of(0, 0), Tier::Uncalibrated);
}
