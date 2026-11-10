//! `fw pauses` / `fw pauses clear` end to end through the real binary: a
//! sqlite store seeded with pauses on two named accounts, the default
//! login, and one expired pause; the JSON listing, each clear selector, the
//! cli attribution, and the exit-1 / exit-2 paths that must leave the pause
//! data byte-identical.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{Duration, Utc};
use flywheel_orchestrator::claims::{ClaimStore, SqliteClaimStore};
use serde_json::Value;

struct Store {
    dir: tempfile::TempDir,
    db: PathBuf,
}

/// Pauses 1 (account A), 2 (B), 3 (default ""), 4 (A, expired), each
/// recorded a minute apart so newest-first is 4, 3, 2, 1.
fn seeded() -> Store {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("flywheel.sqlite");
    let store = SqliteClaimStore::open(&db).unwrap();
    let base = Utc::now() - Duration::minutes(30);
    let seed = [
        ("A", "run-a", Duration::hours(5)),
        ("B", "run-b", Duration::hours(5)),
        ("", "run-d", Duration::hours(5)),
        ("A", "run-a-old", Duration::minutes(10)),
    ];
    for (i, (account, run, reset)) in seed.into_iter().enumerate() {
        let at = base + Duration::minutes(i as i64);
        store
            .record_session_pause(at + reset, at, 86_400.0, run, account)
            .unwrap()
            .unwrap();
    }
    Store { dir, db }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn fw(store: &Store, args: &[&str]) -> Run {
    let root = store.dir.path();
    let output = Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(args)
        .args(["--db", store.db.to_str().unwrap()])
        .current_dir(root)
        .stdin(Stdio::null())
        .env("HOME", root)
        .env("FLYWHEEL_HOME", root.join("flywheel-home"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env_remove("FLYWHEEL_PG_DSN")
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn listing(store: &Store) -> Vec<Value> {
    let run = fw(store, &["pauses", "--json"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    serde_json::from_str::<Value>(&run.stdout)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn entry(entries: &[Value], id: i64) -> Value {
    entries.iter().find(|e| e["id"] == id).unwrap().clone()
}

/// Every pause and clear row, verbatim, for byte-identity comparisons.
fn pause_rows(db: &Path) -> Vec<Vec<String>> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut rows = Vec::new();
    for table in [
        "orchestrator_session_pauses",
        "orchestrator_session_pause_clears",
    ] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY id"))
            .unwrap();
        let width = stmt.column_count();
        let mapped = stmt
            .query_map([], |row| {
                (0..width)
                    .map(|i| row.get_ref(i).map(|v| format!("{table}:{v:?}")))
                    .collect::<Result<Vec<String>, _>>()
            })
            .unwrap();
        for row in mapped {
            rows.push(row.unwrap());
        }
    }
    rows
}

#[test]
fn pauses_verb_json_lists_every_pause_newest_first() {
    let store = seeded();
    let entries = listing(&store);
    let ids: Vec<i64> = entries.iter().map(|e| e["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [4, 3, 2, 1]);
    for e in &entries {
        let mut keys: Vec<&String> = e.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "account",
                "active",
                "cleared",
                "id",
                "pause_until",
                "recorded_at",
                "reset_at",
                "run_id"
            ]
        );
        assert_eq!(e["cleared"], Value::Null);
    }
    assert_eq!(entry(&entries, 4)["active"], Value::Bool(false));
    assert_eq!(entry(&entries, 3)["account"], Value::from(""));
    assert_eq!(entry(&entries, 1)["account"], Value::from("A"));
    assert_eq!(entry(&entries, 1)["run_id"], Value::from("run-a"));
    assert_eq!(entry(&entries, 1)["active"], Value::Bool(true));
}

#[test]
fn pauses_verb_clear_account_clears_only_that_account_with_a_cli_record() {
    let store = seeded();
    let before = listing(&store);
    let reason = "  limit reset early -- \"resume\" now ";
    let run = fw(
        &store,
        &["pauses", "clear", "--account", "A", "--reason", reason],
    );
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "cleared 1\n"),
        "{}",
        run.stderr
    );
    let after = listing(&store);
    let a = entry(&after, 1);
    assert_eq!(a["active"], Value::Bool(false));
    assert_eq!(a["cleared"]["reason"], Value::from(reason));
    assert_eq!(a["cleared"]["via"], Value::from("cli"));
    assert!(a["cleared"]["at"].is_string());
    for key in [
        "run_id",
        "reset_at",
        "pause_until",
        "recorded_at",
        "account",
    ] {
        assert_eq!(a[key], entry(&before, 1)[key], "{key}");
    }
    // B's and the default account's pauses stay active; the expired A
    // pause is not cleared.
    assert_eq!(entry(&after, 2), entry(&before, 2));
    assert_eq!(entry(&after, 3), entry(&before, 3));
    assert_eq!(entry(&after, 4), entry(&before, 4));
    assert_eq!(after.len(), before.len());
}

#[test]
fn pauses_verb_clear_default_ids_and_all() {
    let store = seeded();
    let run = fw(&store, &["pauses", "clear", "--default", "--reason", "d"]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "cleared 3\n"),
        "{}",
        run.stderr
    );
    let run = fw(&store, &["pauses", "clear", "2", "--reason", "i"]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "cleared 2\n"),
        "{}",
        run.stderr
    );
    let run = fw(&store, &["pauses", "clear", "--all", "--reason", "a"]);
    assert_eq!(
        (run.code, run.stdout.as_str()),
        (0, "cleared 1\n"),
        "{}",
        run.stderr
    );
    let entries = listing(&store);
    assert!(entries.iter().all(|e| e["active"] == Value::Bool(false)));
    let reasons: Vec<&Value> = [3, 2, 1]
        .iter()
        .map(|id| &entries.iter().find(|e| e["id"] == *id).unwrap()["cleared"]["reason"])
        .collect();
    assert_eq!(
        reasons,
        [&Value::from("d"), &Value::from("i"), &Value::from("a")]
    );
}

#[test]
fn pauses_verb_no_match_and_empty_reason_leave_pause_data_identical() {
    let store = seeded();
    let before = pause_rows(&store.db);
    let run = fw(
        &store,
        &["pauses", "clear", "--account", "C", "--reason", "r"],
    );
    assert_eq!((run.code, run.stdout.as_str()), (1, ""));
    assert!(
        run.stderr.contains("no active session pause matches"),
        "{}",
        run.stderr
    );
    let run = fw(&store, &["pauses", "clear", "4", "--reason", "r"]);
    assert_eq!(run.code, 1, "an expired pause is not clearable");
    for reason in ["", "  "] {
        let run = fw(&store, &["pauses", "clear", "--all", "--reason", reason]);
        assert_eq!((run.code, run.stdout.as_str()), (2, ""));
        assert!(
            run.stderr.contains("--reason must not be empty"),
            "{}",
            run.stderr
        );
    }
    let run = fw(&store, &["pauses", "clear", "--all"]);
    assert_eq!(run.code, 2, "a missing --reason is a usage error");
    assert_eq!(pause_rows(&store.db), before);
}

#[test]
fn pauses_verb_expired_only_store_matches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("flywheel.sqlite");
    let claims = SqliteClaimStore::open(&db).unwrap();
    let at = Utc::now() - Duration::hours(2);
    claims
        .record_session_pause(at + Duration::hours(1), at, 86_400.0, "run-x", "A")
        .unwrap()
        .unwrap();
    drop(claims);
    let store = Store { dir, db };
    let before = pause_rows(&store.db);
    let run = fw(&store, &["pauses", "clear", "--all", "--reason", "r"]);
    assert_eq!((run.code, run.stdout.as_str()), (1, ""));
    assert_eq!(pause_rows(&store.db), before);
    let entries = listing(&store);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["active"], Value::Bool(false));
}

#[test]
fn pauses_verb_is_listed_in_fw_help() {
    let store = seeded();
    let output = Command::new(env!("CARGO_BIN_EXE_fw"))
        .arg("--help")
        .current_dir(store.dir.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("\n  pauses")
    );
}
