//! `fw self-update`'s store steps: plan, backup, upgrade, Postgres gating,
//! and revert (spec 00088 #8, #17-#21).
//!
//! Every run is confined: `HOME` and `FLYWHEEL_HOME` are fresh temp dirs,
//! every inherited `FLYWHEEL_*` variable is removed from the child, the
//! install dir is an explicit temp dir seeded with copies of the `fw`
//! under test, and no test signals a process (none is spawned to drain).
//! Target versions come from the crates' constants, never literals.

mod support;

#[path = "../../flywheel-core/tests/support/pg_container.rs"]
mod pg_container;

use flywheel_core::store::CURRENT_SCHEMA_VERSION;
use flywheel_orchestrator::claims::CURRENT_ORCH_SCHEMA_VERSION;
use serde_json::Value;
use std::path::{Path, PathBuf};
use support::{
    Fixture, fw_confined, partition_values, project_id_of, project_repo, seed_behind_store,
    store_dump, store_versions,
};

const PREV_RUN: i64 = 14;
const PREV_ORCH: i64 = 9;

fn targets() -> (i64, i64) {
    (
        i64::from(CURRENT_SCHEMA_VERSION),
        i64::from(CURRENT_ORCH_SCHEMA_VERSION),
    )
}

/// A registered project repo under the fixture root; returns `(repo, project dir)`.
fn project(fx: &Fixture, name: &str) -> (PathBuf, PathBuf) {
    let repo = fx.root.join(name);
    let dir = project_repo(&repo, &fx.home, &fx.flywheel_home);
    (repo, dir)
}

/// A project whose store is seeded at the previous schema versions.
fn behind_project(fx: &Fixture, name: &str) -> (PathBuf, PathBuf) {
    let (repo, dir) = project(fx, name);
    seed_behind_store(&dir.join("flywheel.sqlite"), &project_id_of(&dir));
    assert_eq!(
        store_versions(&dir.join("flywheel.sqlite")),
        (Some(PREV_RUN), Some(PREV_ORCH))
    );
    (repo, dir)
}

/// A project whose store is at this build's versions (created by `fw db
/// upgrade` in its repo).
fn current_project(fx: &Fixture, name: &str) -> (PathBuf, PathBuf) {
    let (repo, dir) = project(fx, name);
    let out = fw_confined(&repo, &fx.home, &fx.flywheel_home, &["db", "upgrade"]);
    assert!(out.status.success(), "fw db upgrade: {out:?}");
    let (run, orch) = targets();
    assert_eq!(
        store_versions(&dir.join("flywheel.sqlite")),
        (Some(run), Some(orch))
    );
    (repo, dir)
}

fn statuses(run: &support::Run) -> Vec<(String, String)> {
    run.json_statuses()
}

fn status_of(run: &support::Run, step: &str) -> String {
    statuses(run)
        .into_iter()
        .find(|(s, _)| s == step)
        .map(|(_, status)| status)
        .unwrap_or_else(|| panic!("no step {step}: {run:?}"))
}

/// Copy `file` alone (no sidecars) into a fresh directory; returns the
/// copy. The copy runs in a `/bin/cp` child: opening and closing a live
/// store's file in this process would drop the POSIX locks a connection
/// held open here relies on, and SQLite would then corrupt the store.
fn copy_alone(file: &Path, into: &Path) -> PathBuf {
    std::fs::create_dir_all(into).unwrap();
    let copy = into.join(file.file_name().unwrap());
    support::copy_exe(file, &copy);
    copy
}

fn backups_of(project_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(project_dir.join("backups"))
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// `<UTC %Y%m%dT%H%M%SZ>-run<N>-orch<M>.sqlite`.
fn assert_backup_name(name: &str, run: i64, orch: i64) {
    let suffix = format!("-run{run}-orch{orch}.sqlite");
    let stamp = name
        .strip_suffix(&suffix)
        .unwrap_or_else(|| panic!("{name} does not end with {suffix}"));
    assert!(
        chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").is_ok(),
        "{name}: bad UTC stamp"
    );
}

#[test]
fn backups_capture_wal_only_rows_and_every_project_upgrades_in_its_own_partition() {
    let fx = Fixture::new();
    let (_, dir_a) = behind_project(&fx, "repo-a");
    let (_, dir_b) = behind_project(&fx, "repo-b");
    let (id_a, id_b) = (project_id_of(&dir_a), project_id_of(&dir_b));
    let db_a = dir_a.join("flywheel.sqlite");
    let db_b = dir_b.join("flywheel.sqlite");

    // Committed rows that live only in A's WAL, held open through the run.
    let holder = rusqlite::Connection::open(&db_a).unwrap();
    holder
        .execute_batch(
            "PRAGMA wal_autocheckpoint = 0;
             INSERT INTO task_claims (task_id, worker_id, claimed_at, lease_expires_at, version, \
               conflict_keys_json) VALUES ('wal-only', 'w9', '2026-05-28T13:00:00+00:00', \
               '2026-05-28T13:00:30+00:00', 1, '[]');",
        )
        .unwrap();
    holder
        .execute(
            "INSERT INTO tasks (project_id, id, first_seen_at, updated_at) VALUES (?1, \
             'wal-task', '2026-01-02T00:00:00+00:00', '2026-01-02T00:00:00+00:00')",
            [&id_a],
        )
        .unwrap();
    // Copying the main file alone misses them: the test exercises the WAL.
    let naive = copy_alone(&db_a, &fx.root.join("naive-copy"));
    assert!(!store_dump(&naive).contains("wal-only"));
    let pre_a = store_dump(&db_a);
    let pre_b = store_dump(&db_b);
    assert!(pre_a.contains("wal-only") && pre_a.contains("wal-task"));

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(status_of(&run, "backup"), "ok", "{run:?}");
    assert_eq!(status_of(&run, "install"), "ok", "{run:?}");
    assert_eq!(status_of(&run, "db-upgrade"), "ok", "{run:?}");
    assert_eq!(summary["outcome"], "updated");

    // Backups: one per behind project, named by its pre-upgrade versions,
    // self-contained, equal to the pre-run store.
    let backups = summary["backups"].as_array().unwrap();
    assert_eq!(backups.len(), 2, "{summary}");
    for (id, dir, pre) in [(&id_a, &dir_a, &pre_a), (&id_b, &dir_b, &pre_b)] {
        let entry = backups
            .iter()
            .find(|b| b["project_id"] == id.as_str())
            .unwrap_or_else(|| panic!("no backup for {id}: {summary}"));
        assert_eq!(entry["run_version"], PREV_RUN);
        assert_eq!(entry["orchestrator_version"], PREV_ORCH);
        let path = PathBuf::from(entry["path"].as_str().unwrap());
        assert_eq!(path.parent().unwrap(), dir.join("backups"));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_backup_name(&name, PREV_RUN, PREV_ORCH);
        assert_eq!(backups_of(dir), vec![name.clone()], "only the backup file");
        let alone = copy_alone(&path, &fx.root.join(format!("alone-{id}")));
        let conn = rusqlite::Connection::open(&alone).unwrap();
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        drop(conn);
        assert_eq!(store_versions(&alone), (Some(PREV_RUN), Some(PREV_ORCH)));
        assert_eq!(
            &store_dump(&alone),
            pre,
            "backup of {id} differs from the pre-run store"
        );
    }

    // Upgraded: both stores at the build's versions, every row kept, every
    // partitioned row in its own project's partition.
    let (run_v, orch_v) = targets();
    let upgraded = summary["upgraded"].as_array().unwrap();
    assert_eq!(upgraded.len(), 2, "{summary}");
    for (id, db) in [(&id_a, &db_a), (&id_b, &db_b)] {
        assert_eq!(store_versions(db), (Some(run_v), Some(orch_v)), "{id}");
        let entry = upgraded
            .iter()
            .find(|u| u["project_id"] == id.as_str())
            .unwrap_or_else(|| panic!("{id} not upgraded: {summary}"));
        assert_eq!(entry["store"], "sqlite");
        assert_eq!(entry["from"]["run"], PREV_RUN);
        assert_eq!(entry["from"]["orchestrator"], PREV_ORCH);
        assert_eq!(entry["to"]["run"], run_v);
        assert_eq!(entry["to"]["orchestrator"], orch_v);
        assert!(entry["backup"].as_str().is_some(), "{entry}");
        let partitions = partition_values(db);
        assert!(
            partitions.iter().any(|(t, _)| t == "task_claims"),
            "orchestrator rows are partitioned after the upgrade: {partitions:?}"
        );
        for (table, value) in &partitions {
            assert_eq!(value, id.as_str(), "{table} row in partition {value}");
        }
    }
    let post_a = store_dump(&db_a);
    for row in ["wal-only", "wal-task", "run-legacy", "'t1'"] {
        assert!(
            post_a.contains(row) || post_a.contains(&row.replace('\'', "\"")),
            "{row}"
        );
    }
    drop(holder);

    // The record names both upgraded stores and their backups.
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(fx.flywheel_home.join("self-update/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["stores"].as_array().unwrap().len(), 2, "{state}");
}

#[test]
fn a_failed_second_upgrade_restores_the_first_store_and_the_previous_binaries() {
    let fx = Fixture::new();
    let (_, dir_a) = behind_project(&fx, "repo-a");
    let (_, dir_b) = behind_project(&fx, "repo-b");
    let db_a = dir_a.join("flywheel.sqlite");
    let db_b = dir_b.join("flywheel.sqlite");
    let pre_a = store_dump(&db_a);
    let pre_b = store_dump(&db_b);
    let install_before = fx.install_manifest();

    // Hold B's write lock for the whole run: its upgrade cannot commit.
    let lock = rusqlite::Connection::open(&db_b).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 1, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "failed");
    assert_eq!(status_of(&run, "backup"), "ok", "{run:?}");
    assert_eq!(status_of(&run, "install"), "reverted", "{run:?}");
    assert_eq!(status_of(&run, "db-upgrade"), "reverted", "{run:?}");
    assert_eq!(status_of(&run, "hub"), "skipped", "{run:?}");
    assert_eq!(summary["backups"].as_array().unwrap().len(), 2, "{summary}");
    assert_eq!(summary["upgraded"], serde_json::json!([]), "{summary}");
    assert_eq!(summary["to"], summary["from"], "{summary}");

    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    assert_eq!(store_versions(&db_a), (Some(PREV_RUN), Some(PREV_ORCH)));
    assert_eq!(store_dump(&db_a), pre_a, "the first store was not restored");
    assert_eq!(store_versions(&db_b), (Some(PREV_RUN), Some(PREV_ORCH)));
    assert_eq!(store_dump(&db_b), pre_b);
    assert_eq!(
        fx.install_manifest(),
        install_before,
        "install dir not reverted"
    );
    assert!(
        !fx.flywheel_home.join("self-update/state.json").exists(),
        "a reverted update left its record"
    );
}

#[test]
fn upgrading_only_the_cwd_project_would_mis_partition_every_other_store() {
    // The discriminating counterpart of the partition check above: the
    // default (cwd-less) `fw db upgrade --db` keys orchestrator rows
    // `default`, which the self-update run must never do.
    let fx = Fixture::new();
    let (_, dir) = behind_project(&fx, "repo-a");
    let db = dir.join("flywheel.sqlite");
    let scratch = copy_alone(&db, &fx.root.join("scratch"));
    let out = fw_confined(
        &fx.root,
        &fx.home,
        &fx.flywheel_home,
        &["db", "upgrade", "--db", scratch.to_str().unwrap()],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        partition_values(&scratch)
            .iter()
            .any(|(t, v)| t == "task_claims" && v == "default")
    );

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let id = project_id_of(&dir);
    for (table, value) in partition_values(&db) {
        assert_eq!(value, id, "{table}");
    }
}

#[test]
fn dry_run_plans_each_behind_store_and_changes_nothing() {
    let fx = Fixture::new();
    let (_, dir_a) = behind_project(&fx, "repo-a");
    let (_, dir_c) = current_project(&fx, "repo-c");
    let db_a = dir_a.join("flywheel.sqlite");
    let pre_a = store_dump(&db_a);
    let pre_c = store_dump(&dir_c.join("flywheel.sqlite"));
    let home_before = fx.home_manifest();
    let install_before = fx.install_manifest();

    let run = fx.run(&fx.checkout, &["--dry-run", "--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    assert_eq!(summary["outcome"], "planned");
    assert_eq!(status_of(&run, "backup"), "planned");
    assert_eq!(status_of(&run, "db-upgrade"), "planned");
    let (run_v, orch_v) = targets();
    let upgraded = summary["upgraded"].as_array().unwrap();
    assert_eq!(upgraded.len(), 1, "only the behind store: {summary}");
    assert_eq!(upgraded[0]["project_id"], project_id_of(&dir_a));
    assert_eq!(upgraded[0]["from"]["run"], PREV_RUN);
    assert_eq!(upgraded[0]["from"]["orchestrator"], PREV_ORCH);
    assert_eq!(upgraded[0]["to"]["run"], run_v);
    assert_eq!(upgraded[0]["to"]["orchestrator"], orch_v);
    assert_eq!(summary["backups"], serde_json::json!([]));

    assert_eq!(
        fx.home_manifest(),
        home_before,
        "dry run wrote under FLYWHEEL_HOME"
    );
    assert_eq!(fx.install_manifest(), install_before);
    assert_eq!(store_dump(&db_a), pre_a);
    assert_eq!(store_dump(&dir_c.join("flywheel.sqlite")), pre_c);
}

#[test]
fn a_sixth_backup_prunes_the_oldest_of_that_project_only_and_current_stores_get_none() {
    let fx = Fixture::new();
    let (_, dir_a) = behind_project(&fx, "repo-a");
    let (_, dir_c) = current_project(&fx, "repo-c");
    let old: Vec<String> = (1..=5)
        .map(|day| format!("2026010{day}T000000Z-run{PREV_RUN}-orch{PREV_ORCH}.sqlite"))
        .collect();
    for dir in [&dir_a, &dir_c] {
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        for name in &old {
            std::fs::write(dir.join("backups").join(name), b"older backup").unwrap();
        }
    }
    let c_store_before = store_dump(&dir_c.join("flywheel.sqlite"));

    let run = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    let backups = summary["backups"].as_array().unwrap();
    assert_eq!(
        backups.len(),
        1,
        "a current store gets no backup: {summary}"
    );
    let new_name = PathBuf::from(backups[0]["path"].as_str().unwrap())
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let mut expected_a: Vec<String> = old[1..].to_vec();
    expected_a.push(new_name);
    expected_a.sort();
    assert_eq!(backups_of(&dir_a), expected_a);
    assert_eq!(
        backups_of(&dir_c),
        old,
        "another project's backups were pruned"
    );
    assert_eq!(store_dump(&dir_c.join("flywheel.sqlite")), c_store_before);
    assert_eq!(summary["upgraded"].as_array().unwrap().len(), 1);
}

#[test]
fn an_unreachable_postgres_store_refuses_before_any_change() {
    let fx = Fixture::new();
    let (_, dir_a) = behind_project(&fx, "repo-a");
    let (_, dir_p) = project(&fx, "repo-p");
    std::fs::write(
        dir_p.join("config.toml"),
        "store = \"postgresql://flywheel@127.0.0.1:1/flywheel?connect_timeout=3\"\n",
    )
    .unwrap();
    let pre_a = store_dump(&dir_a.join("flywheel.sqlite"));
    let home_before = fx.home_manifest();
    let install_before = fx.install_manifest();

    for args in [&["--json"][..], &["--json", "--upgrade-postgres"]] {
        let run = fx.run(&fx.checkout, args);
        assert_eq!(run.code, 3, "{args:?}: {run:?}");
        let summary = run.summary();
        assert_eq!(summary["outcome"], "refused");
        assert_eq!(status_of(&run, "drain"), "skipped");
        assert_eq!(status_of(&run, "backup"), "failed");
        assert_eq!(status_of(&run, "install"), "skipped");
        let detail = run.json_lines()[3]["detail"].as_str().unwrap().to_owned();
        assert!(detail.contains(&project_id_of(&dir_p)), "{detail}");
        assert_eq!(fx.home_manifest(), home_before, "{args:?}");
        assert_eq!(fx.install_manifest(), install_before, "{args:?}");
        assert_eq!(store_dump(&dir_a.join("flywheel.sqlite")), pre_a);
    }
}

#[test]
fn a_behind_postgres_store_needs_the_flag_and_is_then_upgraded_without_a_backup() {
    let Some(pg) = pg_container::Postgres::acquire() else {
        return;
    };
    let fx = Fixture::new();
    let (_, dir_p) = project(&fx, "repo-p");
    let id = project_id_of(&dir_p);
    let mut admin = pg.admin();
    admin
        .batch_execute(
            "DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public; SET search_path TO public;",
        )
        .unwrap();
    admin
        .batch_execute(include_str!(
            "../../flywheel-core/tests/fixtures/persistence-schema-postgres-v14.sql"
        ))
        .unwrap();
    admin
        .batch_execute(include_str!(
            "../../flywheel-orchestrator/tests/fixtures/orchestrator-schema-postgres-v9.sql"
        ))
        .unwrap();
    admin
        .execute(
            "INSERT INTO public.task_claims (task_id, worker_id, claimed_at, lease_expires_at, \
             version, conflict_keys_json) VALUES ('t1', 'w1', now(), now(), 1, '[]')",
            &[],
        )
        .unwrap();
    std::fs::write(
        dir_p.join("config.toml"),
        format!("store = \"{}\"\n", pg.dsn()),
    )
    .unwrap();
    let versions = |admin: &mut postgres::Client| -> (i32, i32) {
        let run: i32 = admin
            .query_one(
                "SELECT version FROM public.schema_version WHERE id = 1",
                &[],
            )
            .unwrap()
            .get(0);
        let orch: i32 = admin
            .query_one(
                "SELECT version FROM public.orchestrator_schema_version WHERE id = 1",
                &[],
            )
            .unwrap()
            .get(0);
        (run, orch)
    };
    assert_eq!(versions(&mut admin), (14, 9));
    let install_before = fx.install_manifest();

    let refused = fx.run(&fx.checkout, &["--json"]);
    assert_eq!(refused.code, 3, "{refused:?}");
    refused.summary();
    assert_eq!(versions(&mut admin), (14, 9));
    assert_eq!(fx.install_manifest(), install_before);
    assert!(
        !refused.stdout.contains("fw-test"),
        "the DSN password leaked"
    );

    let run = fx.run(&fx.checkout, &["--json", "--upgrade-postgres"]);
    assert_eq!(run.code, 0, "{run:?}");
    let summary = run.summary();
    let (run_v, orch_v) = targets();
    assert_eq!(
        versions(&mut admin),
        (
            i32::try_from(run_v).unwrap(),
            i32::try_from(orch_v).unwrap()
        )
    );
    let upgraded = summary["upgraded"].as_array().unwrap();
    assert_eq!(upgraded.len(), 1, "{summary}");
    assert_eq!(upgraded[0]["project_id"], id);
    assert_eq!(upgraded[0]["store"], "postgres");
    assert_eq!(upgraded[0]["backup"], Value::Null);
    assert_eq!(summary["backups"], serde_json::json!([]));
    assert!(backups_of(&dir_p).is_empty());
    let partition: String = admin
        .query_one(
            "SELECT project_id FROM public.task_claims WHERE task_id = 't1'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(partition, id);
}
