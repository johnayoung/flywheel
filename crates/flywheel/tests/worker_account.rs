//! `fw worker --account NAME` end to end through the real binary: a git
//! repo adopted by flywheel, a scripted `claude` stand-in on PATH that
//! records the credential each invocation receives, and the worker's own
//! environment carrying decoys for every auth variable. The account's
//! token (read from `$FLYWHEEL_HOME/hub/claude-accounts/NAME.token` at each
//! spawn) is the only credential the agent sees, a token replaced between
//! attempts reaches the next attempt, the token lands nowhere under the
//! home, and a bad `--account` exits 2 having claimed nothing.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const TOKEN_ONE: &str = "sk-ant-oat01-first-3c9d1f";
const TOKEN_TWO: &str = "sk-ant-oat01-second-8e2b7a";

/// The agent: logs `oauth|api_key|auth_token` as it received them. Under
/// the first token it replaces the account's token file and leaves the
/// task undone (the grader fails, so the run retries); under any other
/// credential it does the work and reports `verify`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
printf '%s|%s|%s\n' "${CLAUDE_CODE_OAUTH_TOKEN-unset}" "${ANTHROPIC_API_KEY-unset}" \
  "${ANTHROPIC_AUTH_TOKEN-unset}" >> "$CONTROL_LOG"
if [ "${CLAUDE_CODE_OAUTH_TOKEN-}" = "$TOKEN_ONE" ]; then
  printf '%s\n' "$TOKEN_TWO" > "$ROTATE_FILE"
else
  echo done > done.txt
  git add -A >/dev/null 2>&1 && git commit -qm "work" >/dev/null 2>&1
fi
E='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    project: PathBuf,
}

fn home(root: &Path) -> PathBuf {
    root.join("flywheel-home")
}

fn test_env(command: &mut Command, root: &Path) {
    command
        .env("HOME", root)
        .env("FLYWHEEL_HOME", home(root))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude-config"))
        .env_remove("FLYWHEEL_PG_DSN")
        .env_remove("DATABASE_URL");
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

impl Repo {
    /// One task whose grader passes only once the agent has done the work.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("control")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "account@example.invalid"]);
        git(&repo, &["config", "user.name", "worker account"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        let mut path = Command::new(env!("CARGO_BIN_EXE_fw"));
        path.args(["project", "path"])
            .current_dir(&repo)
            .stdin(Stdio::null());
        test_env(&mut path, &root);
        let output = path.output().unwrap();
        assert!(output.status.success(), "fw project path: {output:?}");
        let project = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        let phase = project.join("tasks/active/01-account");
        std::fs::create_dir_all(&phase).unwrap();
        let body = serde_json::json!({
            "id": "acct",
            "goal": "Goal acct.",
            "graders": [{"type": "command", "name": "unit", "run": "test -f done.txt"}],
        });
        std::fs::write(phase.join("acct.json"), body.to_string()).unwrap();
        Self {
            _dir: dir,
            root,
            repo,
            project,
        }
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

    fn control_log(&self) -> PathBuf {
        self.root.join("control/agent.log")
    }

    /// `fw worker --once ARGS` with decoys for every auth variable.
    fn worker(&self, args: &[&str]) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .args([
                "worker",
                "--once",
                "--heartbeat",
                "0",
                "--poll-interval",
                "1",
            ])
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CONTROL_LOG", self.control_log())
            .env("TOKEN_ONE", TOKEN_ONE)
            .env("TOKEN_TWO", TOKEN_TWO)
            .env("ROTATE_FILE", self.token_file("work"))
            .env("CLAUDE_CODE_OAUTH_TOKEN", "decoy-oauth")
            .env("ANTHROPIC_API_KEY", "decoy-api")
            .env("ANTHROPIC_AUTH_TOKEN", "decoy-auth")
            .stdin(Stdio::null());
        test_env(&mut command, &self.root);
        command.output().unwrap()
    }

    fn fw(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .args(args)
            .current_dir(&self.repo)
            .stdin(Stdio::null());
        test_env(&mut command, &self.root);
        command.output().unwrap()
    }

    /// The credentials each agent invocation received, in order.
    fn received(&self) -> Vec<String> {
        std::fs::read_to_string(self.control_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn db_rows(&self, table: &str) -> i64 {
        let db = self.project.join("flywheel.sqlite");
        if !db.exists() {
            return 0;
        }
        let db = rusqlite::Connection::open(db).unwrap();
        let exists: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        if exists == 0 {
            return 0;
        }
        db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every file under `dir` but `skip` whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str, skip: &Path) -> Vec<PathBuf> {
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
                && path != skip
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

#[test]
fn worker_account_token_reaches_every_attempt_and_nothing_else() {
    let repo = Repo::new();
    repo.add_account("work", TOKEN_ONE);
    let run = repo.worker(&["--account", "work"]);
    assert_eq!(run.status.code(), Some(0), "{}", text(&run));

    let received = repo.received();
    assert!(received.len() >= 2, "{received:?}\n{}", text(&run));
    // Attempt 1 under the token on file at its spawn; every later spawn
    // under the token that replaced it. No decoy, no outranking variable.
    assert_eq!(received[0], format!("{TOKEN_ONE}|unset|unset"));
    for line in &received[1..] {
        assert_eq!(line, &format!("{TOKEN_TWO}|unset|unset"), "{received:?}");
    }
    assert_eq!(
        git(&repo.repo, &["log", "--format=%s", "main"]),
        "work\ninit\n"
    );

    // The token is in the token file and nowhere else under the home.
    let status = repo.fw(&["status"]);
    let show = repo.fw(&["show", "acct", "--json"]);
    for token in [TOKEN_ONE, TOKEN_TWO] {
        let hits = files_containing(&home(&repo.root), token, &repo.token_file("work"));
        assert!(hits.is_empty(), "{token} leaked into {hits:?}");
        for output in [&run, &status, &show] {
            assert!(!text(output).contains(token), "{token} leaked to output");
        }
    }
}

#[test]
fn worker_account_absent_keeps_the_inherited_environment() {
    let repo = Repo::new();
    repo.add_account("work", TOKEN_ONE);
    let run = repo.worker(&[]);
    assert_eq!(run.status.code(), Some(0), "{}", text(&run));
    let received = repo.received();
    assert!(!received.is_empty(), "{}", text(&run));
    for line in &received {
        assert_eq!(line, "decoy-oauth|decoy-api|decoy-auth");
    }
}

#[test]
fn worker_account_refusals_exit_2_before_anything_is_claimed() {
    let repo = Repo::new();
    repo.add_account("work", TOKEN_ONE);
    let cases: [(&[&str], &str); 6] = [
        (&["--account", "../x"], "not a valid account name"),
        (&["--account="], "not a valid account name"),
        (&["--account", "ghost"], "no Claude account \"ghost\""),
        (
            &[
                "--account",
                "work",
                "--sandbox",
                "container",
                "--image",
                "img",
            ],
            "--sandbox container",
        ),
        (
            &["--account", "work", "--sandbox", "container"],
            "--sandbox container",
        ),
        (&["--account", "work", "--agent", "codex"], "--agent codex"),
    ];
    for (args, cause) in cases {
        let run = repo.worker(args);
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert_eq!(run.status.code(), Some(2), "{args:?}: {}", text(&run));
        assert!(stderr.contains(cause), "{args:?}: {stderr}");
        assert!(stderr.contains("--account"), "{args:?}: {stderr}");
        assert!(!stderr.contains(TOKEN_ONE));
        assert!(repo.received().is_empty(), "{args:?}: the agent ran");
        assert_eq!(repo.db_rows("task_claims"), 0, "{args:?}");
        assert_eq!(repo.db_rows("lifecycles"), 0, "{args:?}");
    }
}
