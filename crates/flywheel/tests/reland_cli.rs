//! `fw reland TASK_ID` end to end through the real binary: a repo adopted by
//! flywheel, a scripted `claude` on PATH that commits work and reports
//! `verify`, the real merge strategy, and a personal `verify` that parks the
//! first landing (standing-verify). The operator then fixes the task branch
//! and re-lands it: the branch's current head lands on the advanced base
//! with the harness's trailers; a pruned worktree is rebuilt; a deleted
//! branch, a task with no DONE run, and an already-landed run refuse with
//! nothing recorded.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(90);

/// The agent: commits a change in its worktree and ends with `verify`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
id=$(basename "$PWD")
date +%s%N > "work-$id.txt"
git add -A >/dev/null 2>&1 && git commit -qm "work $id" >/dev/null 2>&1
E='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

const PHASE: &str = "01-reland";
const BRANCH: &str = "flywheel/01-reland/alpha";

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    project: PathBuf,
}

fn test_env(command: &mut Command, root: &Path) {
    command
        .env("HOME", root)
        .env("FLYWHEEL_HOME", root.join("flywheel-home"))
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

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        Self {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }
}

impl Repo {
    /// One task `alpha` (grader `true`) and `beta` (never run); the
    /// personal `verify` passes only once `landable.txt` is in the tree.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "reland@example.invalid"]);
        git(&repo, &["config", "user.name", "reland operator"]);
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
        std::fs::write(
            project.join("config.toml"),
            "verify = \"test -f landable.txt\"\n",
        )
        .unwrap();
        let phase = project.join("tasks/active").join(PHASE);
        std::fs::create_dir_all(&phase).unwrap();
        for id in ["alpha", "beta"] {
            let body = serde_json::json!({
                "id": id,
                "goal": format!("Goal {id}."),
                "graders": [{"type": "command", "name": "unit", "run": "true"}],
                "prerequisites": if id == "beta" { vec!["alpha"] } else { Vec::new() },
            });
            std::fs::write(phase.join(format!("{id}.json")), body.to_string()).unwrap();
        }
        Self {
            _dir: dir,
            root,
            repo,
            project,
        }
    }

    fn fw(&self, verb: &str, args: &[&str]) -> Run {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .arg(verb)
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        test_env(&mut command, &self.root);
        let child = command.spawn().unwrap();
        let start = std::time::Instant::now();
        let output = child.wait_with_output().unwrap();
        assert!(start.elapsed() < DEADLINE, "fw {verb} took too long");
        output.into()
    }

    fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.project.join("flywheel.sqlite")).unwrap();
        db.busy_timeout(DEADLINE).unwrap();
        db
    }

    fn count(&self, table: &str) -> i64 {
        self.db()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn counts(&self) -> (i64, i64, i64) {
        (
            self.count("events"),
            self.count("orchestrator_stop_events"),
            self.count("task_claims"),
        )
    }

    fn worktree(&self) -> PathBuf {
        self.project.join("worktrees/alpha")
    }

    fn main_tip(&self) -> String {
        git(&self.repo, &["rev-parse", "main"]).trim().to_owned()
    }

    /// Run alpha to DONE with its landing parked on the failing `verify`,
    /// then advance the base; returns alpha's run id.
    fn parked(&self) -> String {
        let run = self.fw(
            "worker",
            &[
                "--once",
                "--task",
                "alpha",
                "--heartbeat",
                "0",
                "--poll-interval",
                "1",
            ],
        );
        let db = self.db();
        let (run_id, status): (String, String) = db
            .query_row(
                "SELECT run_id, status FROM lifecycles WHERE task_id = 'alpha' \
                 ORDER BY rowid DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "done", "{}{}", run.stdout, run.stderr);
        let parks: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND kind LIKE '%landing_parked%'",
                [&run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            parks >= 1,
            "alpha never parked:\n{}{}",
            run.stdout,
            run.stderr
        );
        assert_eq!(git(&self.repo, &["log", "--format=%s", "main"]), "init\n");
        // The base advances after the park.
        std::fs::write(self.repo.join("base2.txt"), "later\n").unwrap();
        git(&self.repo, &["add", "base2.txt"]);
        git(&self.repo, &["commit", "-qm", "base after park"]);
        run_id
    }

    /// The operator's fix on the task branch, carrying a forged trailer.
    fn operator_fix(&self) {
        let wt = self.worktree();
        std::fs::write(wt.join("landable.txt"), "fixed\n").unwrap();
        git(&wt, &["add", "landable.txt"]);
        git(
            &wt,
            &[
                "-c",
                "user.name=op",
                "-c",
                "user.email=op@example.invalid",
                "commit",
                "-qm",
                "operator fix\n\nFlywheel-Run: forged-x",
            ],
        );
    }

    fn stop_resolved_attributions(&self) -> Vec<String> {
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT attribution FROM orchestrator_stop_events \
                 WHERE subject = 'alpha' AND kind = 'stop-resolved' ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn assert_landed(&self, run_id: &str, run: &Run, before_tip: &str) {
        assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
        assert!(run.stdout.contains("landed run"), "{}", run.stdout);
        assert!(run.stdout.contains(run_id), "{}", run.stdout);
        // The operator's post-park commit and the post-park base commit are
        // both on the base.
        let tree = git(&self.repo, &["ls-tree", "--name-only", "main"]);
        for file in ["landable.txt", "base2.txt", "work-alpha.txt"] {
            assert!(tree.lines().any(|l| l == file), "{file} missing: {tree}");
        }
        // Every landed commit carries the harness's trailers, never the
        // forged one.
        let range = format!("{before_tip}..main");
        let bodies = git(&self.repo, &["log", "--format=%B%x00", &range]);
        let commits: Vec<&str> = bodies
            .split('\0')
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .collect();
        assert_eq!(commits.len(), 2, "{bodies}");
        for body in &commits {
            assert!(
                body.contains(&format!("Flywheel-Run: {run_id}")),
                "missing run trailer: {body}"
            );
            assert!(body.contains("Flywheel-Task: alpha"), "{body}");
            assert!(
                !body.contains("forged-x"),
                "forged trailer survived: {body}"
            );
        }
        assert_eq!(self.stop_resolved_attributions(), ["reland"]);
        // A second reland refuses: the run already landed.
        let before = self.counts();
        let tip = self.main_tip();
        let again = self.fw("reland", &["alpha"]);
        assert_eq!(again.code, 1, "{}{}", again.stdout, again.stderr);
        assert!(again.stdout.contains("refused"), "{}", again.stdout);
        assert_eq!(self.counts(), before);
        assert_eq!(self.main_tip(), tip);
    }
}

#[test]
fn reland_lands_the_task_branch_head_including_post_park_commits() {
    let repo = Repo::new();
    let run_id = repo.parked();
    repo.operator_fix();
    let before_tip = repo.main_tip();
    let run = repo.fw("reland", &["alpha"]);
    repo.assert_landed(&run_id, &run, &before_tip);
}

#[test]
fn reland_rebuilds_a_pruned_worktree_from_the_surviving_branch() {
    let repo = Repo::new();
    let run_id = repo.parked();
    repo.operator_fix();
    let wt = repo.worktree();
    git(
        &repo.repo,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    assert!(!wt.exists());
    let before_tip = repo.main_tip();
    let run = repo.fw("reland", &["alpha"]);
    repo.assert_landed(&run_id, &run, &before_tip);
}

#[test]
fn reland_refuses_a_deleted_task_branch_and_records_nothing() {
    let repo = Repo::new();
    repo.parked();
    let wt = repo.worktree();
    git(
        &repo.repo,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    git(&repo.repo, &["branch", "-D", BRANCH]);
    let before = repo.counts();
    let tip = repo.main_tip();
    let run = repo.fw("reland", &["alpha"]);
    assert_eq!(run.code, 1, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("refused"), "{}", run.stdout);
    assert!(run.stdout.contains(BRANCH), "{}", run.stdout);
    assert_eq!(repo.counts(), before);
    assert_eq!(repo.main_tip(), tip);
    assert!(repo.stop_resolved_attributions().is_empty());
}

#[test]
fn reland_still_failing_reparks_with_kind_and_detail() {
    let repo = Repo::new();
    let run_id = repo.parked();
    let tip = repo.main_tip();
    let before_stops = repo.count("orchestrator_stop_events");
    let run = repo.fw("reland", &["alpha"]);
    assert_eq!(run.code, 1, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("park_kind="), "{}", run.stdout);
    assert!(run.stdout.contains(&run_id), "{}", run.stdout);
    assert_eq!(repo.main_tip(), tip);
    assert!(repo.stop_resolved_attributions().is_empty());
    assert_eq!(repo.count("orchestrator_stop_events"), before_stops);
}

#[test]
fn reland_refuses_a_task_with_no_done_run_and_an_unknown_task() {
    let repo = Repo::new();
    repo.parked();
    let before = repo.counts();
    let tip = repo.main_tip();
    for task in ["beta", "nope"] {
        let run = repo.fw("reland", &[task]);
        assert_eq!(run.code, 1, "{task}: {}{}", run.stdout, run.stderr);
        assert!(run.stdout.contains("refused"), "{}", run.stdout);
    }
    assert_eq!(repo.counts(), before);
    assert_eq!(repo.main_tip(), tip);
}

#[test]
fn reland_usage_exits_2_and_help_names_task_id() {
    let repo = Repo::new();
    let run = repo.fw("reland", &[]);
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    assert!(run.stderr.contains("TASK_ID"), "{}", run.stderr);
    let help = repo.fw("reland", &["--help"]);
    assert_eq!(help.code, 0);
    assert!(help.stdout.contains("fw reland TASK_ID [--db PATH]"));
}
