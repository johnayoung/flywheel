//! `fw worker --task ID` end to end through the real binary: a git repo
//! adopted by flywheel with several queued tasks, a scripted `claude`
//! stand-in on PATH that commits a file and reports `verify`, real command
//! graders, the real merge strategy. Only the named tasks run; their
//! prerequisites are refused or pulled in; the exit code and the one-line
//! summary follow the outcome; a pool never outnumbers its scope; and a
//! claim another worker holds is waited out, never stolen.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use chrono::Utc;
use flywheel_orchestrator::claims::{ClaimStore, SqliteClaimStore};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long one worker invocation may take before the test calls it hung.
const DEADLINE: Duration = Duration::from_secs(90);

/// The agent: records which task it ran (its worktree is named after the
/// task), commits a change there, and ends the iteration with `verify`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
id=$(basename "$PWD")
echo "$id" >> "$RAN_LOG"
date +%s%N > "work-$id.txt"
git add -A >/dev/null 2>&1 && git commit -qm "work $id" >/dev/null 2>&1
E='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
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

impl Repo {
    /// `alpha`, `beta`, `gamma` independent; `delta` after `alpha`;
    /// `broken`'s grader always fails.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "scope@example.invalid"]);
        git(&repo, &["config", "user.name", "worker scope"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        std::fs::write(
            repo.join("flywheel.toml"),
            "[source]\nkind = \"directory\"\n",
        )
        .unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        let phase = repo.join(".flywheel/tasks/active/01-scope");
        std::fs::create_dir_all(&phase).unwrap();
        let task = |id: &str, run: &str, prerequisites: &[&str]| {
            let body = serde_json::json!({
                "id": id,
                "goal": format!("Goal {id}."),
                "graders": [{"type": "command", "name": "unit", "run": run}],
                "prerequisites": prerequisites,
            });
            std::fs::write(phase.join(format!("{id}.json")), body.to_string()).unwrap();
        };
        task("alpha", "true", &[]);
        task("beta", "true", &[]);
        task("gamma", "true", &[]);
        task("delta", "true", &["alpha"]);
        task("broken", "false", &[]);
        Self {
            _dir: dir,
            root,
            repo,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .arg("worker")
            .args(args)
            .args(["--heartbeat", "0", "--poll-interval", "1"])
            .current_dir(&self.repo)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", &self.root)
            .env_remove("FLYWHEEL_HOME")
            .env("RAN_LOG", self.root.join("ran.log"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.command(args).spawn().unwrap()
    }

    /// Run `fw worker ARGS` to completion (killed and failed past the
    /// deadline: a scoped worker must exit on its own).
    fn worker(&self, args: &[&str]) -> Run {
        finish(self.spawn(args), DEADLINE)
    }

    /// The task ids the agent ran, in order.
    fn ran(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("ran.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn db_path(&self) -> PathBuf {
        self.repo.join(".flywheel/flywheel.sqlite")
    }

    /// Every task with a lifecycle, sorted.
    fn touched(&self) -> Vec<String> {
        if !self.db_path().exists() {
            return Vec::new();
        }
        let db = rusqlite::Connection::open(self.db_path()).unwrap();
        db.busy_timeout(DEADLINE).unwrap();
        let mut stmt = db
            .prepare("SELECT DISTINCT task_id FROM lifecycles ORDER BY task_id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// The commit subjects on main, newest first.
    fn landed(&self) -> Vec<String> {
        git(&self.repo, &["log", "--format=%s", "main"])
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn finish(mut child: Child, deadline: Duration) -> Run {
    let until = Instant::now() + deadline;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > until {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "fw worker did not exit within {deadline:?}\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// The scope summary: stdout's last line.
fn summary(run: &Run) -> &str {
    run.stdout.lines().last().unwrap_or_default()
}

#[test]
fn only_the_named_task_runs_and_every_other_stays_untouched() {
    let repo = Repo::new();
    let run = repo.worker(&["--task", "beta"]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(summary(&run), "scope: 1/1 verified -- beta verified");
    assert!(run.stderr.contains("scoped to beta"), "{}", run.stderr);
    assert_eq!(repo.ran(), ["beta"]);
    assert_eq!(repo.touched(), ["beta"]);
    assert_eq!(repo.landed(), ["work beta", "init"]);
    // Named again once verified: nothing runs, and it reports so.
    let again = repo.worker(&["--task", "beta"]);
    assert_eq!(again.code, 0, "{}{}", again.stdout, again.stderr);
    assert_eq!(summary(&again), "scope: 1/1 verified -- beta verified");
    assert_eq!(repo.ran(), ["beta"]);
}

#[test]
fn unmet_prerequisites_refuse_and_with_prereqs_runs_them_first() {
    let repo = Repo::new();
    let refused = repo.worker(&["--task", "delta"]);
    assert_eq!(refused.code, 2, "{}{}", refused.stdout, refused.stderr);
    assert_eq!(
        refused.stderr.trim(),
        "flywheel worker: task delta waits on unmet prerequisite alpha (pending). \
         Run them first, or pass --with-prereqs to run them in this scope."
    );
    assert!(repo.ran().is_empty());
    assert!(repo.touched().is_empty());

    let unknown = repo.worker(&["--task", "nope"]);
    assert_eq!(unknown.code, 2);
    assert!(
        unknown.stderr.contains("unknown task nope"),
        "{}",
        unknown.stderr
    );

    let run = repo.worker(&["--task", "delta", "--with-prereqs"]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    // The pulled-in prerequisite counts in the line too.
    assert_eq!(
        summary(&run),
        "scope: 2/2 verified -- delta verified; prerequisite alpha verified"
    );
    assert!(
        run.stderr
            .contains("scoped to delta (with prerequisites alpha)"),
        "{}",
        run.stderr
    );
    assert_eq!(repo.ran(), ["alpha", "delta"]);
    assert_eq!(repo.touched(), ["alpha", "delta"]);
}

#[test]
fn a_failing_task_exits_one_without_idling_or_rerunning() {
    let repo = Repo::new();
    // A daemon (no --once): it must stop by itself once the task settles.
    let run = repo.worker(&["--task", "broken", "--max-retries", "0"]);
    assert_eq!(run.code, 1, "{}{}", run.stdout, run.stderr);
    let line = summary(&run);
    assert!(
        line.starts_with("scope: 0/1 verified -- broken failed"),
        "{line}"
    );
    assert_eq!(repo.ran(), ["broken"]);
    assert_eq!(repo.touched(), ["broken"]);
    // A mixed scope: one verified, one failed -> 1, both named.
    let mixed = repo.worker(&["--task", "gamma", "--task", "broken", "--max-retries", "0"]);
    assert_eq!(mixed.code, 1, "{}{}", mixed.stdout, mixed.stderr);
    assert!(
        summary(&mixed).starts_with("scope: 1/2 verified -- gamma verified; broken failed"),
        "{}",
        summary(&mixed)
    );
    // The earlier failure predates this scope, so broken runs again (once).
    let mut ran = repo.ran();
    ran.sort();
    assert_eq!(ran, ["broken", "broken", "gamma"]);
}

#[test]
fn a_pool_never_outnumbers_its_scope_and_runs_no_extras() {
    let repo = Repo::new();
    let run = repo.worker(&["--task", "gamma", "--concurrency", "2"]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert!(
        run.stderr
            .contains("concurrency 2 capped to 1: the scope has 1 task(s)"),
        "{}",
        run.stderr
    );
    assert_eq!(repo.ran(), ["gamma"]);
    assert_eq!(repo.touched(), ["gamma"]);

    // Two members sharing a two-task scope: each task once, nothing else.
    let pool = repo.worker(&["--task", "alpha", "--task", "beta", "--concurrency", "2"]);
    assert_eq!(pool.code, 0, "{}{}", pool.stdout, pool.stderr);
    assert!(
        pool.stderr.contains("started worker pool size=2"),
        "{}",
        pool.stderr
    );
    assert_eq!(
        summary(&pool),
        "scope: 2/2 verified -- alpha verified; beta verified"
    );
    let mut ran = repo.ran();
    ran.sort();
    assert_eq!(ran, ["alpha", "beta", "gamma"]);
    assert_eq!(repo.touched(), ["alpha", "beta", "gamma"]);
}

#[test]
fn a_claim_another_worker_holds_is_waited_out_never_stolen() {
    let repo = Repo::new();
    // The store exists once a worker opened it; open it the same way.
    let claims = SqliteClaimStore::open(repo.db_path()).unwrap();
    let held = claims
        .acquire_claim("beta", "peer-worker", Utc::now(), 600.0, &BTreeSet::new())
        .unwrap()
        .expect("the peer claims beta");

    let once = repo.worker(&["--task", "beta", "--once"]);
    assert_eq!(once.code, 1, "{}{}", once.stdout, once.stderr);
    assert_eq!(
        summary(&once),
        "scope: 0/1 verified -- beta pending (claimed by peer-worker)"
    );
    assert!(repo.ran().is_empty());
    assert_eq!(
        claims.load_claim("beta").unwrap().unwrap().worker_id,
        "peer-worker"
    );

    // As a daemon it waits for the peer, then runs it and exits.
    let mut daemon = repo.spawn(&["--task", "beta"]);
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "the scoped worker gave up on a held task"
    );
    assert!(repo.ran().is_empty());
    assert_eq!(
        claims.load_claim("beta").unwrap().unwrap().worker_id,
        "peer-worker"
    );
    claims.release_claim(&held, Some(Utc::now())).unwrap();
    let run = finish(daemon, DEADLINE);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(summary(&run), "scope: 1/1 verified -- beta verified");
    // While it waited it said so, once (not every poll).
    let waiting = "scope: waiting -- beta pending (claimed by peer-worker)";
    assert_eq!(run.stderr.matches(waiting).count(), 1, "{}", run.stderr);
    assert_eq!(repo.ran(), ["beta"]);
    assert_eq!(repo.touched(), ["beta"]);
}

#[test]
fn a_held_out_gate_block_parks_the_task_and_exits_one() {
    let repo = Repo::new();
    std::fs::write(
        repo.repo.join("flywheel.toml"),
        "[source]\nkind = \"directory\"\n\n[held_out]\nroot = \".flywheel/verification/held-out\"\n",
    )
    .unwrap();
    git(&repo.repo, &["commit", "-qam", "gate"]);
    let held_out = repo.repo.join(".flywheel/verification/held-out");
    std::fs::create_dir_all(&held_out).unwrap();
    std::fs::write(
        held_out.join("gamma.json"),
        r#"{"graders": [{"type": "command", "name": "oracle", "run": "false"}]}"#,
    )
    .unwrap();
    let run = repo.worker(&["--task", "gamma"]);
    assert_eq!(run.code, 1, "{}{}", run.stdout, run.stderr);
    assert!(
        summary(&run)
            .starts_with("scope: 0/1 verified -- gamma parked (landing parked: held-out-gate"),
        "{}",
        summary(&run)
    );
    assert_eq!(repo.ran(), ["gamma"]);
    assert_eq!(repo.landed(), ["gate", "init"]);
}

#[test]
fn a_sandbox_that_will_not_prepare_settles_parked_instead_of_idling() {
    let repo = Repo::new();
    // A stray directory where gamma's worktree goes: the merge strategy
    // refuses to clobber it on every attempt.
    std::fs::create_dir_all(repo.repo.join(".flywheel/worktrees/gamma")).unwrap();
    let run = repo.worker(&["--task", "gamma"]);
    assert_eq!(run.code, 1, "{}{}", run.stdout, run.stderr);
    assert!(
        summary(&run).starts_with(
            "scope: 0/1 verified -- gamma parked (its sandbox failed to prepare 3 times: PrepareError:"
        ),
        "{}",
        summary(&run)
    );
    assert!(
        run.stderr
            .contains("scope: waiting -- gamma pending (sandbox failed to prepare: PrepareError:"),
        "{}",
        run.stderr
    );
    assert!(repo.ran().is_empty());
}

#[test]
fn a_dependent_held_behind_an_unmerged_phase_is_refused_at_start() {
    let repo = Repo::new();
    let next = repo.repo.join(".flywheel/tasks/active/02-next");
    std::fs::create_dir_all(&next).unwrap();
    let omega = serde_json::json!({
        "id": "omega",
        "goal": "Goal omega.",
        "graders": [{"type": "command", "name": "unit", "run": "true"}],
        "prerequisites": ["alpha"],
    });
    std::fs::write(next.join("omega.json"), omega.to_string()).unwrap();
    let alpha = repo.worker(&["--task", "alpha"]);
    assert_eq!(alpha.code, 0, "{}{}", alpha.stdout, alpha.stderr);
    // Phase 01-scope's integration branch carries work main cannot see.
    git(
        &repo.repo,
        &["checkout", "-q", "-b", "flywheel/phase/01-scope"],
    );
    std::fs::write(repo.repo.join("phase.txt"), "phase work\n").unwrap();
    git(&repo.repo, &["add", "phase.txt"]);
    git(&repo.repo, &["commit", "-qm", "phase work"]);
    git(&repo.repo, &["checkout", "-q", "main"]);

    let held = repo.worker(&["--task", "omega"]);
    assert_eq!(held.code, 2, "{}{}", held.stdout, held.stderr);
    assert_eq!(
        held.stderr.trim(),
        "flywheel worker: task omega waits for done prerequisite alpha to reach HEAD: \
         merge flywheel/phase/01-scope first"
    );
    assert_eq!(repo.ran(), ["alpha"]);

    // Once the phase merges the same command runs it.
    git(
        &repo.repo,
        &["merge", "-q", "--no-edit", "flywheel/phase/01-scope"],
    );
    let run = repo.worker(&["--task", "omega"]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(repo.ran(), ["alpha", "omega"]);
}
