//! `fw eval set`: build a draft from a project's past runs, validate it,
//! freeze it, and read it back -- through the real binary, in a confined
//! home, against a store seeded with a landed run whose held-out oracle
//! discriminates its base from its landed commit.

mod support;

use chrono::{Duration, TimeZone, Utc};
use flywheel_core::events::{DomainEvent, DomainEventPayload, Landed};
use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use flywheel_core::store::{AttemptStore, DomainEventStore, LifecycleStore};
use flywheel_core::store_sqlite::SqliteStore;
use support::{fw_confined, git, project_id_of, project_repo};

fn ok(out: &std::process::Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_landed_run_becomes_a_validated_frozen_set() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let fly = tmp.path().join("fly");
    let repo = tmp.path().join("repo");
    let project = project_repo(&repo, &home, &fly);
    let id = project_id_of(&project);

    // The work: one commit with the run's trailer, making a.txt say 2.
    std::fs::write(repo.join("a.txt"), "1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base", "--no-gpg-sign"]);
    let base = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "commit",
            "-qm",
            "feat: two\n\nFlywheel-Run: run-1",
            "--no-gpg-sign",
        ],
    );
    let landed = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

    // The task, its held-out registration, and its oracle in the project home.
    let phase = project.join("tasks/archive/01-x");
    std::fs::create_dir_all(&phase).unwrap();
    std::fs::write(
        phase.join("two.json"),
        r#"{"id": "two", "goal": "make a.txt say 2"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project.join("verification")).unwrap();
    std::fs::write(
        project.join("verification/oracle-two.sh"),
        "test \"$(cat a.txt)\" = 2\n",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("held-out")).unwrap();
    std::fs::write(
        project.join("held-out/two.json"),
        format!(
            r#"{{"graders": [{{"type": "command", "name": "oracle", "run": "sh {}/verification/oracle-two.sh"}}]}}"#,
            project.display()
        ),
    )
    .unwrap();

    // The store: one run that landed `landed` on its first attempt.
    let store = SqliteStore::open_partition(project.join("flywheel.sqlite"), &id).unwrap();
    let t0 = Utc.with_ymd_and_hms(2026, 11, 10, 9, 0, 0).unwrap();
    let mut lc = Lifecycle::new("two").with_run_id("run-1");
    for status in [
        Status::Ready,
        Status::Running,
        Status::Validating,
        Status::Done,
    ] {
        lc.transition_to(status, "", Some(t0)).unwrap();
    }
    store.create_lifecycle(&lc).unwrap();
    let mut attempt = Attempt::new(1, t0, "run-1");
    attempt.outcome = Some(Outcome::Succeeded);
    attempt.total_cost_usd = 2.25;
    store.save_attempt("run-1", &attempt, None).unwrap();
    let version = store.load_lifecycle("run-1").unwrap().unwrap().version;
    store
        .append_domain_event(
            &DomainEvent::new(
                "run-1",
                t0 + Duration::minutes(10),
                DomainEventPayload::Landed(Landed {
                    strategy: "merge".into(),
                    landed_ref: landed.clone(),
                    ..Landed::default()
                }),
            ),
            version,
        )
        .unwrap();
    drop(store);

    let built = ok(&fw_confined(
        &repo,
        &home,
        &fly,
        &["eval", "set", "build", "core"],
    ));
    assert!(
        built.contains("+ two") && built.contains("landed"),
        "{built}"
    );
    assert!(
        built.contains(&base[..10]),
        "base is the run's own parent: {built}"
    );

    let frozen_too_early = fw_confined(&repo, &home, &fly, &["eval", "set", "freeze", "core"]);
    assert!(!frozen_too_early.status.success());
    assert!(String::from_utf8_lossy(&frozen_too_early.stderr).contains("not validated"));

    let validated = ok(&fw_confined(
        &repo,
        &home,
        &fly,
        &["eval", "set", "validate", "core@draft", "--timeout", "60s"],
    ));
    assert!(validated.contains("ok   two"), "{validated}");
    assert!(validated.contains("1 of 1 validated"), "{validated}");

    let froze = ok(&fw_confined(
        &repo,
        &home,
        &fly,
        &["eval", "set", "freeze", "core"],
    ));
    assert!(froze.contains("froze core@v1 (1 tasks"), "{froze}");
    let pinned = git(&repo, &["rev-parse", "refs/flywheel/eval/core/v1/two/ref"]);
    assert_eq!(pinned.trim(), landed);

    let listed = ok(&fw_confined(&repo, &home, &fly, &["eval", "set", "ls"]));
    assert!(
        listed.contains("core@v1") && listed.contains("1 tasks"),
        "{listed}"
    );
    let shown = ok(&fw_confined(
        &repo,
        &home,
        &fly,
        &["eval", "set", "show", "core"],
    ));
    assert!(
        shown.contains("two") && shown.contains("validated"),
        "{shown}"
    );
    let json = ok(&fw_confined(
        &repo,
        &home,
        &fly,
        &["eval", "set", "show", "core@v1", "--json"],
    ));
    let manifest: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(manifest["tasks"][0]["base"], base.as_str());
    assert_eq!(manifest["tasks"][0]["parallel_safe"], true);
    // The set's oracle names no live home path: it was tokenized.
    let reg =
        std::fs::read_to_string(project.join("evals/sets/core/v1/oracles/two/registration.json"))
            .unwrap();
    assert!(
        reg.contains("@EVAL_PROJECT_HOME@") && !reg.contains(&project.display().to_string()),
        "{reg}"
    );
}
