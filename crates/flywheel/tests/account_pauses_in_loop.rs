//! Account-aware workers and session-limit pauses end to end through the
//! real binary: a disposable `FLYWHEEL_HOME` with two named accounts whose
//! names and tokens are random, a scripted `claude` first on PATH that
//! decides every invocation from the `CLAUDE_CODE_OAUTH_TOKEN` it receives
//! (it logs only a hash of it, to a control dir outside the home), and
//! workers launched with decoys for every auth variable.
//!
//! * A worker under account A hands A's token to every attempt, with
//!   `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` removed.
//! * A refusal under A records a pause on A only: the next A worker claims
//!   nothing while a B worker and a default worker each claim and verify.
//! * `fw pauses clear --account A` records the attributed clear and lets A
//!   claim and verify again.
//! * A pause seeded in a v10 orchestrator store (the committed
//!   `orchestrator-schema-v10.sql` fixture, before pause accounts) survives
//!   the real `fw db upgrade` as account `null` and holds every worker.
//!
//! Claims, pauses, and outcomes are read from the store, `fw pauses --json`,
//! and `fw show --json`, never from the agent's output. The tokens land
//! nowhere under the home but their own token files.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The agent. Logs `sha256(oauth)|api_key?|auth_token?` per invocation to
/// `$CONTROL_DIR/agent.log` (never the token itself). Refuses with a
/// session-limit result when the received token's hash is listed in
/// `$CONTROL_DIR/refuse`; consumes `$CONTROL_DIR/skip-once` by leaving the
/// work undone (the grader fails, so the run retries); otherwise commits
/// the work and reports `verify`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
if [ -n "${CLAUDE_CODE_OAUTH_TOKEN+x}" ]; then
  hash=$(printf '%s' "$CLAUDE_CODE_OAUTH_TOKEN" | sha256sum | cut -d' ' -f1)
else
  hash=unset
fi
api=absent; [ -n "${ANTHROPIC_API_KEY+x}" ] && api=present
auth=absent; [ -n "${ANTHROPIC_AUTH_TOKEN+x}" ] && auth=present
printf '%s|%s|%s|%s\n' "$hash" "$api" "$auth" "$(basename "$PWD")" >> "$CONTROL_DIR/agent.log"
if [ -f "$CONTROL_DIR/refuse" ] && grep -qx "$hash" "$CONTROL_DIR/refuse"; then
  reset=$(( $(date +%s) + 7200 ))
  R="Claude AI usage limit reached|$reset"
  printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$R"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
  printf '%s\n' '{"type":"result","subtype":"success","is_error":true,"result":"'"$R"'","num_turns":1,"total_cost_usd":0.0,"usage":{"input_tokens":7,"output_tokens":2}}'
  exit 1
fi
if [ -f "$CONTROL_DIR/skip-once" ]; then
  rm -f "$CONTROL_DIR/skip-once"
else
  echo done > "done-$(basename "$PWD").txt"
  git add -A >/dev/null 2>&1 && git commit -qm "work $(basename "$PWD")" >/dev/null 2>&1
fi
E='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

const DECOY_OAUTH: &str = "decoy-oauth-from-the-worker-env";

/// The queued tasks, in the order an unscoped worker selects them.
const TASKS: [&str; 6] = ["t1", "t2", "t3", "t4", "t5", "t6"];

/// A random marker (account names and tokens): `prefix` + 24 hex chars.
fn marker(prefix: &str) -> String {
    format!(
        "{prefix}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..24]
    )
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    project: PathBuf,
}

fn home(root: &Path) -> PathBuf {
    root.join("flywheel-home")
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

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

impl Env {
    /// A repo adopted by flywheel with the queued [`TASKS`], each graded on
    /// the agent having done the work. The store is not created here.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("control")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "pauses@example.invalid"]);
        git(&repo, &["config", "user.name", "account pauses"]);
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
        let phase = env.project.join("tasks/active/01-pauses");
        std::fs::create_dir_all(&phase).unwrap();
        for id in TASKS {
            let body = serde_json::json!({
                "id": id,
                "goal": format!("Goal {id}."),
                "graders": [{"type": "command", "name": "unit", "run": format!("test -f done-{id}.txt")}],
            });
            std::fs::write(phase.join(format!("{id}.json")), body.to_string()).unwrap();
        }
        env
    }

    fn control(&self) -> PathBuf {
        self.root.join("control")
    }

    fn token_file(&self, name: &str) -> PathBuf {
        home(&self.root)
            .join("hub/claude-accounts")
            .join(format!("{name}.token"))
    }

    fn add_account(&self, name: &str, token: &str) {
        let file = self.token_file(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, format!("{token}\n")).unwrap();
    }

    /// The agent refuses (session limit) only for these token hashes.
    fn refuse_hashes(&self, hashes: &[&str]) {
        let file = self.control().join("refuse");
        if hashes.is_empty() {
            let _ = std::fs::remove_file(file);
        } else {
            std::fs::write(file, format!("{}\n", hashes.join("\n"))).unwrap();
        }
    }

    fn base(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .args(args)
            .current_dir(&self.repo)
            .env("HOME", &self.root)
            .env("FLYWHEEL_HOME", home(&self.root))
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL")
            .stdin(Stdio::null());
        command
    }

    fn fw(&self, args: &[&str]) -> Output {
        self.base(args).output().unwrap()
    }

    /// `fw worker --once ARGS` with the scripted agent first on PATH and
    /// decoys for every auth variable in the worker's own environment.
    fn worker(&self, args: &[&str]) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let mut full = vec![
            "worker",
            "--once",
            "--heartbeat",
            "0",
            "--poll-interval",
            "1",
        ];
        full.extend_from_slice(args);
        self.base(&full)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CONTROL_DIR", self.control())
            .env("CLAUDE_CODE_OAUTH_TOKEN", DECOY_OAUTH)
            .env("ANTHROPIC_API_KEY", "decoy-api-key")
            .env("ANTHROPIC_AUTH_TOKEN", "decoy-auth-token")
            .output()
            .unwrap()
    }

    /// The agent invocations so far: `[hash, api, auth, task]` each.
    fn invocations(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.control().join("agent.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split('|').map(str::to_owned).collect())
            .collect()
    }

    fn db_path(&self) -> PathBuf {
        self.project.join("flywheel.sqlite")
    }

    fn partition(&self) -> String {
        self.project
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.db_path()).unwrap();
        db.busy_timeout(std::time::Duration::from_secs(30)).unwrap();
        db
    }

    /// `(task_id, run_id, status)` of every run in this project, by task.
    fn runs(&self) -> Vec<(String, String, String)> {
        if !self.db_path().exists() {
            return Vec::new();
        }
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT task_id, run_id, status FROM lifecycles WHERE project_id = ?1 \
                 ORDER BY task_id, run_id",
            )
            .unwrap();
        stmt.query_map([self.partition()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    /// The task ids holding a claim row in this project.
    fn claimed(&self) -> Vec<String> {
        if !self.db_path().exists() {
            return Vec::new();
        }
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT task_id FROM task_claims WHERE project_id = ?1 ORDER BY task_id")
            .unwrap();
        stmt.query_map([self.partition()], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// `(id, account, run_id)` of every pause row in this project.
    fn pause_rows(&self) -> Vec<(i64, Option<String>, String)> {
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT id, account, run_id FROM orchestrator_session_pauses \
                 WHERE project_id = ?1 ORDER BY id",
            )
            .unwrap();
        stmt.query_map([self.partition()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn pauses(&self) -> Vec<Value> {
        let out = self.fw(&["pauses", "--json"]);
        assert!(out.status.success(), "fw pauses --json: {}", text(&out));
        serde_json::from_slice::<Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    /// `fw show RUN --json`.
    fn show(&self, run_id: &str) -> Value {
        let out = self.fw(&["show", run_id, "--json"]);
        assert!(out.status.success(), "fw show {run_id}: {}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// The task's single run: it exists, is `status` in the store, and
    /// `fw show --json` agrees. Returns its run id.
    fn assert_run(&self, task_id: &str, status: &str) -> String {
        let runs: Vec<_> = self
            .runs()
            .into_iter()
            .filter(|(task, _, _)| task == task_id)
            .collect();
        assert_eq!(runs.len(), 1, "{task_id}: {runs:?}");
        let (_, run_id, stored) = &runs[0];
        assert_eq!(stored, status, "{task_id}: {runs:?}");
        let shown = self.show(run_id);
        assert_eq!(
            shown["run"]["status"],
            Value::from(status),
            "{task_id}: {shown}"
        );
        assert_eq!(shown["run"]["task_id"], Value::from(task_id), "{shown}");
        run_id.clone()
    }

    /// Every file under `dir` but `skip` whose bytes contain `needle`.
    fn files_containing(dir: &Path, needle: &str, skip: &[PathBuf]) -> Vec<PathBuf> {
        let mut hits = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    stack.push(path);
                } else if kind.is_file()
                    && !skip.contains(&path)
                    && std::fs::read(&path).is_ok_and(|bytes| {
                        bytes
                            .windows(needle.len())
                            .any(|window| window == needle.as_bytes())
                    })
                {
                    hits.push(path);
                }
            }
        }
        hits
    }
}

#[test]
fn a_refusal_under_one_account_pauses_only_that_account_until_cleared() {
    let env = Env::new();
    let (name_a, name_b) = (marker("acct-a-"), marker("acct-b-"));
    let (token_a, token_b) = (marker("sk-ant-oat01-a-"), marker("sk-ant-oat01-b-"));
    let (hash_a, hash_b) = (sha256_hex(&token_a), sha256_hex(&token_b));
    env.add_account(&name_a, &token_a);
    env.add_account(&name_b, &token_b);

    // 1. A worker under A: attempt 1 leaves the work undone, attempt 2
    //    does it. Both attempts receive A's token, nothing outranking it.
    std::fs::write(env.control().join("skip-once"), "").unwrap();
    let run = env.worker(&["--task", "t1", "--account", &name_a, "--max-retries", "1"]);
    assert_eq!(run.status.code(), Some(0), "{}", text(&run));
    let first = env.assert_run("t1", "done");
    assert_eq!(env.show(&first)["attempts"].as_array().unwrap().len(), 2);
    let calls = env.invocations();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for call in &calls {
        assert_eq!(
            call[..3],
            [hash_a.clone(), "absent".into(), "absent".into()]
        );
        assert_eq!(call[3], "t1");
    }
    assert!(env.pauses().is_empty());

    // 2. A is refused for its session limit: the pause carries A.
    env.refuse_hashes(&[&hash_a]);
    let run = env.worker(&["--task", "t2", "--account", &name_a, "--max-retries", "0"]);
    let refused_run = {
        let runs: Vec<_> = env.runs().into_iter().filter(|r| r.0 == "t2").collect();
        assert_eq!(runs.len(), 1, "{runs:?}\n{}", text(&run));
        runs[0].1.clone()
    };
    let refused = env.show(&refused_run);
    assert_eq!(refused["run"]["task_id"], Value::from("t2"), "{refused}");
    assert_ne!(refused["run"]["status"], Value::from("done"), "{refused}");
    let calls = env.invocations();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(
        calls[2][..4],
        [
            hash_a.clone(),
            "absent".into(),
            "absent".into(),
            "t2".into()
        ]
    );
    let pauses = env.pauses();
    assert_eq!(pauses.len(), 1, "{pauses:?}\n{}", text(&run));
    let pause = &pauses[0];
    assert_eq!(pause["account"], Value::from(name_a.as_str()), "{pause}");
    assert_eq!(
        pause["run_id"],
        Value::from(refused_run.as_str()),
        "{pause}"
    );
    assert_eq!(pause["active"], Value::Bool(true), "{pause}");
    assert_eq!(pause["cleared"], Value::Null, "{pause}");
    let pause_id = pause["id"].as_i64().unwrap();
    assert_eq!(
        env.pause_rows(),
        [(pause_id, Some(name_a.clone()), refused_run.clone())]
    );
    assert!(env.claimed().is_empty(), "{:?}", env.claimed());

    // 3. The next A worker claims nothing and says why.
    let before = env.runs();
    let held = env.worker(&["--task", "t3", "--account", &name_a]);
    assert!(
        String::from_utf8_lossy(&held.stderr).contains("session-limit pause active"),
        "{}",
        text(&held)
    );
    assert_eq!(env.runs(), before);
    assert!(env.claimed().is_empty());
    assert_eq!(env.invocations().len(), 3);

    // 4. In the same store a B worker and a default worker each claim and
    //    verify a queued task, under their own credentials.
    let run = env.worker(&["--task", "t3", "--account", &name_b]);
    assert_eq!(run.status.code(), Some(0), "{}", text(&run));
    env.assert_run("t3", "done");
    let run = env.worker(&["--task", "t4"]);
    assert_eq!(run.status.code(), Some(0), "{}", text(&run));
    env.assert_run("t4", "done");
    let calls = env.invocations();
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert_eq!(
        calls[3][..4],
        [
            hash_b.clone(),
            "absent".into(),
            "absent".into(),
            "t3".into()
        ]
    );
    // No --account: the worker's own environment, unchanged.
    assert_eq!(
        calls[4][..4],
        [
            sha256_hex(DECOY_OAUTH),
            "present".into(),
            "present".into(),
            "t4".into()
        ]
    );
    // A's pause is still the only one, and still active.
    let pauses = env.pauses();
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    assert_eq!(pauses[0]["active"], Value::Bool(true));

    // 5. Clearing A's pause records who/how/why; A resumes.
    env.refuse_hashes(&[]);
    let reason = format!("limit reset early {}", marker("r-"));
    let clear = env.fw(&["pauses", "clear", "--account", &name_a, "--reason", &reason]);
    assert_eq!(clear.status.code(), Some(0), "{}", text(&clear));
    assert_eq!(
        String::from_utf8_lossy(&clear.stdout),
        format!("cleared {pause_id}\n")
    );
    let after = env.pauses();
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0]["active"], Value::Bool(false), "{}", after[0]);
    assert_eq!(after[0]["cleared"]["reason"], Value::from(reason.as_str()));
    assert_eq!(after[0]["cleared"]["via"], Value::from("cli"));
    for key in [
        "id",
        "account",
        "run_id",
        "reset_at",
        "pause_until",
        "recorded_at",
    ] {
        assert_eq!(after[0][key], pause[key], "{key}");
    }
    let run = env.worker(&["--task", "t5", "--account", &name_a]);
    assert_eq!(run.status.code(), Some(0), "{}", text(&run));
    env.assert_run("t5", "done");
    let calls = env.invocations();
    assert_eq!(calls.len(), 6, "{calls:?}");
    assert_eq!(
        calls[5][..4],
        [
            hash_a.clone(),
            "absent".into(),
            "absent".into(),
            "t5".into()
        ]
    );

    // 6. The tokens are in their token files and nowhere else under the
    //    home (store + WAL, logs, transcripts, worktrees), nor in any
    //    operator readout.
    let skip = [env.token_file(&name_a), env.token_file(&name_b)];
    let status = env.fw(&["status"]);
    let pauses_json = env.fw(&["pauses", "--json"]);
    let show = env.fw(&["show", &refused_run, "--json"]);
    for token in [&token_a, &token_b] {
        let hits = Env::files_containing(&home(&env.root), token, &skip);
        assert!(hits.is_empty(), "a token leaked into {hits:?}");
        for output in [&status, &pauses_json, &show] {
            assert!(
                !text(output).contains(token.as_str()),
                "a token leaked to output"
            );
        }
    }
    // The control dir sits outside the home and holds hashes only.
    assert!(!env.control().starts_with(home(&env.root)));
    let log = std::fs::read_to_string(env.control().join("agent.log")).unwrap();
    assert!(!log.contains(&token_a) && !log.contains(&token_b));
}

#[test]
fn a_pause_seeded_in_a_v10_store_holds_every_worker_after_the_upgrade() {
    let env = Env::new();
    let (name_a, name_b) = (marker("acct-a-"), marker("acct-b-"));
    let (token_a, token_b) = (marker("sk-ant-oat01-a-"), marker("sk-ant-oat01-b-"));
    env.add_account(&name_a, &token_a);
    env.add_account(&name_b, &token_b);

    // The project's store at orchestrator schema v10 (before pause
    // accounts), holding an active project-wide pause for this project.
    let until = chrono::Utc::now() + chrono::Duration::hours(2);
    let recorded = chrono::Utc::now() - chrono::Duration::minutes(1);
    {
        let raw = rusqlite::Connection::open(env.db_path()).unwrap();
        raw.execute_batch(include_str!(
            "../../flywheel-orchestrator/tests/fixtures/orchestrator-schema-v10.sql"
        ))
        .unwrap();
        raw.execute(
            "INSERT INTO orchestrator_session_pauses \
               (project_id, run_id, reset_at, pause_until, recorded_at) \
             VALUES (?1, 'run-legacy-limit', ?2, ?2, ?3)",
            rusqlite::params![env.partition(), until.to_rfc3339(), recorded.to_rfc3339()],
        )
        .unwrap();
        raw.close().unwrap();
    }

    // The real forward upgrade.
    let upgrade = env.fw(&["db", "upgrade"]);
    assert!(
        upgrade.status.success(),
        "fw db upgrade: {}",
        text(&upgrade)
    );
    {
        let db = env.db();
        let version: i64 = db
            .query_row(
                "SELECT version FROM orchestrator_schema_version WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(version > 10, "orchestrator schema still at {version}");
    }
    let rows = env.pause_rows();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, None, "the legacy pause gained an account");
    assert_eq!(rows[0].2, "run-legacy-limit");
    let pauses = env.pauses();
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    assert_eq!(pauses[0]["account"], Value::Null, "{}", pauses[0]);
    assert_eq!(pauses[0]["active"], Value::Bool(true), "{}", pauses[0]);
    assert_eq!(pauses[0]["run_id"], Value::from("run-legacy-limit"));

    // Workers under A, under B, and with no account each claim nothing.
    for args in [
        vec!["--account", name_a.as_str()],
        vec!["--account", name_b.as_str()],
        vec![],
    ] {
        let held = env.worker(&args);
        assert!(
            String::from_utf8_lossy(&held.stderr).contains("session-limit pause active"),
            "{args:?}: {}",
            text(&held)
        );
        assert!(env.runs().is_empty(), "{args:?}: {:?}", env.runs());
        assert!(env.claimed().is_empty(), "{args:?}: {:?}", env.claimed());
        assert!(env.invocations().is_empty(), "{args:?}: the agent ran");
    }

    // Clearing it (`--all` covers pre-change pauses) releases them.
    let clear = env.fw(&[
        "pauses",
        "clear",
        "--all",
        "--reason",
        "legacy limit lifted",
    ]);
    assert_eq!(clear.status.code(), Some(0), "{}", text(&clear));
    let run = env.worker(&["--task", "t1", "--account", &name_b]);
    assert_eq!(run.status.code(), Some(0), "{}", text(&run));
    env.assert_run("t1", "done");
    assert_eq!(env.pause_rows().len(), 1, "the pause row was deleted");
}
