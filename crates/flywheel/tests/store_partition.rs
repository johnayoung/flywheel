//! The partitioned store and `fw db upgrade`, end to end through the real
//! binary: `FLYWHEEL_HOME`, `HOME`, and `XDG_STATE_HOME` point at a tempdir,
//! repos are real git repos in tempdirs, stdin is `/dev/null`.
//!
//! A fresh repo's store has a `project_id` column on every table but the
//! schema-version sentinels and nothing lands in the repo; a store whose
//! sentinels are behind is refused naming `fw db upgrade`, which fixes it
//! (idempotently); a real v12 store is migrated with its rows assigned the
//! project's id, so the project's verbs read them back; an explicit `--db`
//! upgrade binds the `default` partition.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;

struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self { _dir: dir, root }
    }

    fn flywheel_home(&self) -> PathBuf {
        self.root.join("fwhome")
    }

    fn fw(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(args)
            .current_dir(cwd)
            .env("FLYWHEEL_HOME", self.flywheel_home())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL")
            .env_remove("FLYWHEEL_DB")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.fw(cwd, args);
        assert!(
            output.status.success(),
            "fw {args:?} exited {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "store@example.invalid"]);
        git(&repo, &["config", "user.name", "store test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        repo
    }

    fn db(&self, id: &str) -> PathBuf {
        self.flywheel_home()
            .join("projects")
            .join(id)
            .join("flywheel.sqlite")
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    stmt.query_map([], |row| row.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Decrement the first column of every `*schema_version` table by one.
fn age_sentinels(db: &Path) {
    let conn = Connection::open(db).unwrap();
    for table in tables(&conn) {
        if table.ends_with("schema_version") {
            let first = columns(&conn, &table).remove(0);
            conn.execute_batch(&format!("UPDATE {table} SET {first} = {first} - 1"))
                .unwrap();
        }
    }
}

#[test]
fn a_fresh_store_is_partitioned_and_a_behind_one_is_refused_until_upgraded() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    sandbox.ok(&repo, &["status", "--json"]);
    let id = git(&repo, &["config", "--get", "flywheel.projectid"]);
    let db = sandbox.db(&id);
    assert!(db.is_file());
    assert!(!repo.join(".flywheel").exists());
    {
        let conn = Connection::open(&db).unwrap();
        let all = tables(&conn);
        assert!(all.iter().any(|t| t == "lifecycles"), "{all:?}");
        assert!(all.iter().any(|t| t == "task_claims"), "{all:?}");
        for table in all {
            let cols = columns(&conn, &table);
            if table.ends_with("schema_version") {
                assert_eq!(cols[0], "version", "{table}");
            } else {
                assert!(cols.contains(&"project_id".to_owned()), "{table}: {cols:?}");
            }
        }
    }

    age_sentinels(&db);
    let refused = sandbox.fw(&repo, &["status", "--json"]);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("fw db upgrade"),
        "{}",
        text(&refused)
    );

    let report = sandbox.ok(&repo, &["db", "upgrade"]);
    assert!(report.contains(&id), "{report}");
    sandbox.ok(&repo, &["status", "--json"]);
    // Idempotent.
    let again = sandbox.ok(&repo, &["db", "upgrade"]);
    assert!(again.contains("already at schema"), "{again}");
    sandbox.ok(&repo, &["status", "--json"]);
    assert!(!repo.join(".flywheel").exists());
}

#[test]
fn a_v12_store_upgrades_with_its_rows_assigned_the_project_id() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    sandbox.ok(&repo, &["status", "--json"]);
    let id = git(&repo, &["config", "--get", "flywheel.projectid"]);
    let db = sandbox.db(&id);
    // Replace the store with a legacy v12 one holding one finished run.
    std::fs::remove_file(&db).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db.display())));
    }
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(include_str!(
            "../../flywheel-core/tests/fixtures/persistence-schema-v12.sql"
        ))
        .unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, first_seen_at, updated_at)
               VALUES ('legacy-task', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
             INSERT INTO lifecycles (run_id, task_id, status, version, retries, timestamps_json,
                                     updated_at)
               VALUES ('run-legacy', 'legacy-task', 'done', 4, 0, '{}',
                       '2026-01-01T00:00:00+00:00');",
        )
        .unwrap();
    }

    let refused = sandbox.fw(&repo, &["history", "--json"]);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("fw db upgrade"),
        "{}",
        text(&refused)
    );

    let report = sandbox.ok(&repo, &["db", "upgrade"]);
    assert!(report.contains("schema 12 -> 14"), "{report}");
    let conn = Connection::open(&db).unwrap();
    let owner: String = conn
        .query_row(
            "SELECT project_id FROM lifecycles WHERE run_id = 'run-legacy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner, id);
    drop(conn);
    let history = sandbox.ok(&repo, &["history", "--json"]);
    assert!(history.contains("legacy-task"), "{history}");
}

#[test]
fn an_explicit_db_upgrade_binds_the_default_partition() {
    let sandbox = Sandbox::new();
    let outside = sandbox.root.join("plain");
    std::fs::create_dir_all(&outside).unwrap();
    let db = outside.join("legacy.sqlite");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(include_str!(
            "../../flywheel-core/tests/fixtures/persistence-schema-v12.sql"
        ))
        .unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, first_seen_at, updated_at)
               VALUES ('t', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');",
        )
        .unwrap();
    }
    let db_arg = db.to_str().unwrap();
    // Outside any git repo: --db names the store, nothing is resolved.
    let report = sandbox.ok(&outside, &["db", "upgrade", "--db", db_arg]);
    assert!(report.contains("project default"), "{report}");
    let conn = Connection::open(&db).unwrap();
    let owner: String = conn
        .query_row("SELECT project_id FROM tasks WHERE id = 't'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(owner, "default");
    assert!(!sandbox.flywheel_home().join("projects").exists());
}

#[test]
fn a_newer_store_is_refused_by_verbs_and_by_the_upgrade() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    sandbox.ok(&repo, &["status", "--json"]);
    let id = git(&repo, &["config", "--get", "flywheel.projectid"]);
    let conn = Connection::open(sandbox.db(&id)).unwrap();
    conn.execute_batch("UPDATE schema_version SET version = version + 1")
        .unwrap();
    drop(conn);
    for args in [&["status", "--json"][..], &["db", "upgrade"][..]] {
        let output = sandbox.fw(&repo, args);
        assert!(!output.status.success(), "{args:?}");
        assert!(
            text(&output).contains("newer flywheel"),
            "{}",
            text(&output)
        );
    }
}
