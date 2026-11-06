//! The audit record end to end through the real binary: a worker's run
//! leaves a `transcript` row whose `file://` URI names its body under
//! `$FLYWHEEL_HOME/projects/<id>/transcripts/` with a matching SHA-256;
//! `fw purge` deletes the bodies but not the rows; `fw autopilot --once`
//! leaves `autopilot.*` rows; `fw purge --project` refuses unattended
//! without `--yes` and deletes the project dir with it. The agent is a
//! stand-in script; `FLYWHEEL_HOME`, `HOME`, and `XDG_STATE_HOME` point at
//! a tempdir and the repo is a real git repo in it.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use flywheel_core::transcripts::{sha256_hex, uri_path};
use serde_json::Value;

/// The agent stand-in: commits a file in its worktree and ends the
/// iteration with `verify`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
id=$(basename "$PWD")
echo "$id" > "work-$id.txt"
git add -A >/dev/null 2>&1 && git commit -qm "work $id" >/dev/null 2>&1
E='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        Self { _dir: dir, root }
    }

    fn flywheel_home(&self) -> PathBuf {
        self.root.join("fwhome")
    }

    fn fw(&self, cwd: &Path, args: &[&str]) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(args)
            .current_dir(cwd)
            .env("FLYWHEEL_HOME", self.flywheel_home())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL")
            .env_remove("FW_HUB_TOKEN")
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
        git(&repo, &["config", "user.email", "audit@example.invalid"]);
        git(&repo, &["config", "user.name", "audit test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        repo
    }

    fn audit(&self, repo: &Path) -> Vec<Value> {
        self.ok(repo, &["audit", "--project", "--json"])
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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

fn files_under(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            if e.file_type().unwrap().is_dir() {
                files_under(&e.path())
            } else {
                1
            }
        })
        .sum()
}

#[test]
fn a_run_leaves_a_transcript_row_that_outlives_its_body() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    sandbox.ok(&repo, &["status", "--json"]);
    let id = git(&repo, &["config", "--get", "flywheel.projectid"]);
    let project = sandbox.flywheel_home().join("projects").join(&id);
    let phase = project.join("tasks/active/p1");
    std::fs::create_dir_all(&phase).unwrap();
    let task = serde_json::json!({
        "id": "hello",
        "goal": "Say hello.",
        "graders": [{"type": "command", "name": "unit", "run": "test -f work-hello.txt"}],
    });
    std::fs::write(phase.join("hello.json"), task.to_string()).unwrap();

    let output = sandbox.fw(
        &repo,
        &[
            "worker",
            "--once",
            "--heartbeat",
            "0",
            "--poll-interval",
            "1",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // One transcript row; its body lives under the project's transcripts/
    // and hashes to the recorded sha256.
    let rows = sandbox.audit(&repo);
    let transcripts: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "transcript").collect();
    assert_eq!(transcripts.len(), 1, "{rows:?}");
    let row = transcripts[0];
    assert!(
        chrono::DateTime::parse_from_rfc3339(row["at"].as_str().unwrap()).is_ok(),
        "{row}"
    );
    let body = uri_path(row["uri"].as_str().unwrap()).unwrap();
    assert!(body.starts_with(project.join("transcripts")), "{row}");
    let bytes = std::fs::read(&body).unwrap();
    assert_eq!(row["sha256"], sha256_hex(&bytes));
    assert_eq!(row["bytes"], bytes.len());
    assert!(!row["run_id"].as_str().unwrap().is_empty());
    assert!(!project.join("logs/runs").exists());
    // Newest last, plain text form too.
    let text = sandbox.ok(&repo, &["audit", "--project"]);
    assert!(text.contains("transcript  run="), "{text}");

    // fw purge deletes every body; the row stays.
    let purged = sandbox.ok(&repo, &["purge"]);
    assert!(purged.contains("deleted 1 transcript body"), "{purged}");
    assert_eq!(files_under(&project.join("transcripts")), 0);
    let after = sandbox.audit(&repo);
    assert!(
        after
            .iter()
            .any(|r| r["kind"] == "transcript" && r["sha256"] == row["sha256"]),
        "{after:?}"
    );

    // An autopilot pass records its decisions as rows.
    let _ = sandbox.fw(&repo, &["autopilot", "--once"]);
    let rows = sandbox.audit(&repo);
    assert!(
        rows.iter()
            .any(|r| r["kind"].as_str().unwrap().starts_with("autopilot.")),
        "{rows:?}"
    );

    // The whole project dir: refused unattended without --yes.
    let refused = sandbox.fw(&repo, &["purge", "--project"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes"));
    assert!(project.is_dir());
    let deleted = sandbox.ok(&repo, &["purge", "--project", "--yes"]);
    assert!(deleted.contains("deleted project dir"), "{deleted}");
    assert!(!project.exists());
}
