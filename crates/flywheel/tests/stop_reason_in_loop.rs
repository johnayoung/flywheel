//! The stop reason end to end through the real binary: a home project whose
//! store starts at schema v14 (the committed pre-stop-reason fixture, holding
//! a finalized attempt) is refused by the worker, migrated by `fw db
//! upgrade`, then driven by real `fw worker` cycles against a scripted
//! `claude` stand-in. Each attempt's stop reason is read back from the store
//! (and `fw show --json`), never from the agent's output: abort ->
//! agent_gave_up, already_satisfied -> agent_claimed_already_done, a failing
//! check -> checks_failed{check}, success -> verified; the pre-upgrade
//! attempt reads back with no reason.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use flywheel_core::lifecycle::{Lifecycle, Outcome, Status, StopReason};
use flywheel_core::store::{CURRENT_SCHEMA_VERSION, LifecycleFilter, LifecycleStore};
use flywheel_core::store_sqlite::SqliteStore;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The agent: its worktree is named after the task, which picks the
/// iteration's envelope. Every branch commits a change first so the outcome
/// turns on the envelope and the graders alone.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
id=$(basename "$PWD")
date +%s%N > "work-$id.txt"
git add -A >/dev/null 2>&1 && git commit -qm "work $id" >/dev/null 2>&1
case "$id" in
  gives-up) intent=abort ;;
  satisfied) intent=already_satisfied ;;
  *) intent=verify ;;
esac
E='<!-- LOOP_STATUS -->\n{\"intent\":\"'"$intent"'\",\"reason\":\"scripted\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

/// The tasks the worker runs: (id, the `unit` grader's command).
const TASKS: [(&str, &str); 4] = [
    ("gives-up", "true"),
    ("satisfied", "true"),
    ("failing", "false"),
    ("passes", "true"),
];

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    /// The repo's home project dir (`$FLYWHEEL_HOME/projects/<id>`).
    project: PathBuf,
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "stop@example.invalid"]);
        git(&repo, &["config", "user.name", "stop reason"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        let mut env = Self {
            _dir: dir,
            root,
            repo,
            project: PathBuf::new(),
        };
        let path = env.fw(&["project", "path"]);
        assert!(path.status.success(), "fw project path: {path:?}");
        env.project = PathBuf::from(String::from_utf8(path.stdout).unwrap().trim());
        let phase = env.project.join("tasks/active/01-stop");
        std::fs::create_dir_all(&phase).unwrap();
        for (id, run) in TASKS {
            let body = serde_json::json!({
                "id": id,
                "goal": format!("Goal {id}."),
                "graders": [{"type": "command", "name": "unit", "run": run}],
            });
            std::fs::write(phase.join(format!("{id}.json")), body.to_string()).unwrap();
        }
        env
    }

    /// `fw ARGS` in the repo with a tempdir home (never the operator's) and
    /// the scripted agent first on PATH.
    fn fw(&self, args: &[&str]) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", &self.root)
            .env("FLYWHEEL_HOME", self.root.join("flywheel-home"))
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn db_path(&self) -> PathBuf {
        self.project.join("flywheel.sqlite")
    }

    /// The store partition: the project dir's name is its id.
    fn partition(&self) -> String {
        self.project
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// Build the project's store from the frozen v14 DDL with one
    /// pre-existing run whose attempt finalized before stop reasons existed.
    fn seed_v14(&self) {
        let raw = rusqlite::Connection::open(self.db_path()).unwrap();
        raw.execute_batch(include_str!(
            "../../flywheel-core/tests/fixtures/persistence-schema-v14.sql"
        ))
        .unwrap();
        let seed = "INSERT INTO tasks (project_id, id, first_seen_at, updated_at)
               VALUES (?1, 'legacy', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
             INSERT INTO task_versions
               (project_id, task_id, content_hash, goal, graders_json, tags_json, context_json,
                created_at)
               VALUES (?1, 'legacy', 'h-legacy', 'Goal legacy.',
                       '[{\"type\": \"command\", \"run\": \"true\"}]', '[]', 'null',
                       '2026-01-01T00:00:00+00:00');
             INSERT INTO lifecycles
               (project_id, run_id, task_id, status, version, retries, timestamps_json,
                updated_at, task_content_hash, source)
               VALUES (?1, 'run-legacy', 'legacy', 'failed', 3, 0, '{}',
                       '2026-01-01T00:00:00+00:00', 'h-legacy', '');
             INSERT INTO attempts
               (project_id, run_id, number, attempt_run_id, started_at, ended_at, outcome,
                agent_output, error, agent_context_json, input_tokens, turns, total_cost_usd)
               VALUES (?1, 'run-legacy', 1, 'run-legacy', '2026-01-01T00:00:00+00:00',
                       '2026-01-01T00:01:00+00:00', 'agent_error', 'out', 'boom',
                       '{\"model_id\": \"m\"}', 42, 3, 0.25);";
        for statement in seed.split(';').filter(|s| !s.trim().is_empty()) {
            raw.execute(statement, [self.partition()]).unwrap();
        }
        raw.close().unwrap();
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::open_partition(self.db_path(), &self.partition()).unwrap()
    }

    /// The task's single run, as the store folds it.
    fn lifecycle(&self, store: &SqliteStore, task_id: &str) -> Lifecycle {
        let filter = LifecycleFilter {
            task_id: Some(task_id.to_owned()),
            ..LifecycleFilter::default()
        };
        let mut runs = store.list_lifecycles(&filter).unwrap();
        assert_eq!(runs.len(), 1, "{task_id}: {runs:?}");
        runs.remove(0)
    }

    /// `fw show RUN --json`'s per-attempt `stop_reason` values.
    fn shown_reasons(&self, run_id: &str) -> Vec<serde_json::Value> {
        let out = self.fw(&["show", run_id, "--json"]);
        assert!(out.status.success(), "fw show {run_id}: {out:?}");
        let detail: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        detail["attempts"]
            .as_array()
            .unwrap_or_else(|| panic!("{detail}"))
            .iter()
            .map(|attempt| attempt["stop_reason"].clone())
            .collect()
    }
}

#[test]
fn a_v14_store_upgrades_and_real_worker_cycles_record_each_stop_reason() {
    let env = Env::new();
    env.seed_v14();

    // The worker refuses the behind store and names the upgrade verb.
    let refused = env.fw(&["worker", "--task", "passes", "--once"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("fw db upgrade"),
        "{refused:?}"
    );

    let upgrade = env.fw(&["db", "upgrade"]);
    assert!(upgrade.status.success(), "fw db upgrade: {upgrade:?}");
    {
        let raw = rusqlite::Connection::open(env.db_path()).unwrap();
        let version: i64 = raw
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, i64::from(CURRENT_SCHEMA_VERSION));
    }

    let mut args = vec!["worker", "--max-retries", "0"];
    for (id, _) in TASKS {
        args.extend(["--task", id]);
    }
    args.extend(["--heartbeat", "0", "--poll-interval", "1"]);
    let run = env.fw(&args);
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    // passes verifies; the other three fail, so the scope exits 1.
    assert_eq!(run.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stdout.contains("scope: 1/4 verified"), "{stdout}{stderr}");

    let store = env.store();
    let expected = [
        ("gives-up", Status::Failed, StopReason::AgentGaveUp),
        (
            "satisfied",
            Status::Failed,
            StopReason::AgentClaimedAlreadyDone,
        ),
        (
            "failing",
            Status::Failed,
            StopReason::ChecksFailed {
                check: "unit".to_owned(),
            },
        ),
        ("passes", Status::Done, StopReason::Verified),
    ];
    for (task_id, status, reason) in expected {
        let lifecycle = env.lifecycle(&store, task_id);
        assert_eq!(lifecycle.status, status, "{task_id}: {lifecycle:?}");
        assert_eq!(lifecycle.attempts.len(), 1, "{task_id}: {lifecycle:?}");
        let attempt = &lifecycle.attempts[0];
        assert_eq!(attempt.stop_reason.as_ref(), Some(&reason), "{task_id}");
        // The run read straight from the store agrees with the listing.
        let loaded = store.load_lifecycle(&lifecycle.run_id).unwrap().unwrap();
        assert_eq!(loaded.attempts[0].stop_reason.as_ref(), Some(&reason));
        // And the operator readout reports exactly that reason.
        assert_eq!(
            env.shown_reasons(&lifecycle.run_id),
            [serde_json::to_value(&reason).unwrap()],
            "{task_id}"
        );
    }
    // The already-satisfied claim is never success.
    let satisfied = env.lifecycle(&store, "satisfied");
    assert_ne!(satisfied.attempts[0].outcome, Some(Outcome::Succeeded));
    assert_eq!(
        serde_json::to_value(StopReason::ChecksFailed {
            check: "unit".to_owned()
        })
        .unwrap(),
        serde_json::json!({"kind": "checks_failed", "check": "unit"})
    );

    // The pre-upgrade attempt is kept verbatim and reports no reason.
    let legacy = store.load_lifecycle("run-legacy").unwrap().unwrap();
    assert_eq!(legacy.task_id, "legacy");
    assert_eq!(legacy.status, Status::Failed);
    assert_eq!(legacy.version, 3);
    assert_eq!(legacy.attempts.len(), 1);
    assert_eq!(legacy.attempts[0].outcome, Some(Outcome::AgentError));
    assert_eq!(legacy.attempts[0].error, "boom");
    assert_eq!(legacy.attempts[0].input_tokens, 42);
    assert_eq!(legacy.attempts[0].stop_reason, None);
    assert_eq!(env.shown_reasons("run-legacy"), [serde_json::Value::Null]);
    store.close().unwrap();
}
