//! In-loop verification of spec 00086 (operator re-land + landed-only
//! prerequisites) through the real `fw worker` and `fw reland` binaries: a
//! repo adopted by flywheel in a disposable `FLYWHEEL_HOME`, a scripted
//! `claude` on PATH (the only scripted piece), real command graders, the
//! real merge strategy, and a held-out oracle registered in the home.
//!
//! A prerequisite's held-out oracle fails until an operator commit and a
//! later base commit are both in the graded tree, so its landing parks and
//! the re-driver routes it to human review while its dependent is never
//! claimed. The operator fixes the task branch with plain git; `fw reland`
//! lands it with the harness's trailers and a `reland`-attributed
//! `stop-resolved`; the next worker pass builds the dependent on a base
//! carrying the prerequisite's change. Every outcome is read from the store
//! and git, never from the agent's own output.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long one `fw` invocation may take before the test calls it hung.
const DEADLINE: Duration = Duration::from_secs(180);

/// The agent: records, before touching anything, whether the
/// prerequisite's marker file is in its worktree; then commits a change
/// there and ends the iteration with `verify`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
id=$(basename "$PWD")
if [ -f "$PREREQ_MARKER" ]; then echo "$id marker" >> "$RAN_LOG"; else echo "$id no-marker" >> "$RAN_LOG"; fi
date +%s%N > "work-$id.txt"
git add -A >/dev/null 2>&1 && git commit -qm "work $id" >/dev/null 2>&1
E='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

/// The park kind the prerequisite's human-review routing row carries.
const PARK_KIND: &str = "held-out-gate";

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

/// A short random token for task ids and marker names.
fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mixed =
        (nanos ^ (u128::from(std::process::id()) << 40)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{:010x}", mixed as u64 & 0xff_ffff_ffff)
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn text(&self) -> String {
        format!("exit {}\n{}{}", self.code, self.stdout, self.stderr)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    project: PathBuf,
    phase: String,
    prereq: String,
    dependent: String,
    /// Added only by the operator's fix commit on the prerequisite's branch.
    op_marker: String,
    /// Committed to the base only after the prerequisite is routed.
    base_marker: String,
    /// Committed by the agent on the prerequisite's branch: the dependent's
    /// agent looks for it at start.
    prereq_marker: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "inloop@example.invalid"]);
        git(&repo, &["config", "user.name", "in-loop operator"]);
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

        let t = token();
        let phase = format!("01-inloop-{t}");
        let prereq = format!("pre-{t}");
        let dependent = format!("dep-{t}");
        let op_marker = format!("op-{}.txt", token());
        let base_marker = format!("base-{}.txt", token());
        let prereq_marker = format!("work-{prereq}.txt");

        let dir_phase = project.join("tasks/active").join(&phase);
        std::fs::create_dir_all(&dir_phase).unwrap();
        for (id, prerequisites) in [(&prereq, Vec::new()), (&dependent, vec![prereq.clone()])] {
            let body = serde_json::json!({
                "id": id,
                "goal": format!("Goal {id}."),
                "graders": [{"type": "command", "name": "unit", "run": "true"}],
                "prerequisites": prerequisites,
            });
            std::fs::write(dir_phase.join(format!("{id}.json")), body.to_string()).unwrap();
        }
        // The held-out oracle, registered in the disposable home: it passes
        // only when both the operator's marker and the post-routing base
        // marker are in the tree it grades.
        let held_out = project.join("held-out");
        std::fs::create_dir_all(&held_out).unwrap();
        let oracle = serde_json::json!({
            "graders": [{
                "type": "command",
                "name": "oracle",
                "run": format!("test -f {op_marker} && test -f {base_marker}"),
            }],
        });
        std::fs::write(held_out.join(format!("{prereq}.json")), oracle.to_string()).unwrap();

        Self {
            _dir: dir,
            root,
            repo,
            project,
            phase,
            prereq,
            dependent,
            op_marker,
            base_marker,
            prereq_marker,
        }
    }

    fn command(&self, verb: &str, args: &[&str]) -> Command {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .arg(verb)
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("RAN_LOG", self.root.join("ran.log"))
            .env("PREREQ_MARKER", &self.prereq_marker)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        test_env(&mut command, &self.root);
        command
    }

    fn fw(&self, verb: &str, args: &[&str]) -> Run {
        finish(self.command(verb, args).spawn().unwrap(), verb)
    }

    /// One unscoped worker drain cycle: selection over the whole queue.
    fn worker_pass(&self) -> Run {
        self.fw(
            "worker",
            &["--once", "--heartbeat", "0", "--poll-interval", "1"],
        )
    }

    /// The agent's start-of-run records, in order.
    fn ran(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("ran.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.project.join("flywheel.sqlite")).unwrap();
        db.busy_timeout(DEADLINE).unwrap();
        db
    }

    fn count(&self, sql: &str, task: &str) -> i64 {
        self.db().query_row(sql, [task], |row| row.get(0)).unwrap()
    }

    fn lifecycles(&self, task: &str) -> i64 {
        self.count("SELECT COUNT(*) FROM lifecycles WHERE task_id = ?1", task)
    }

    fn claims(&self, task: &str) -> i64 {
        self.count("SELECT COUNT(*) FROM task_claims WHERE task_id = ?1", task)
    }

    /// The task's latest run id and status.
    fn latest_run(&self, task: &str) -> (String, String) {
        self.db()
            .query_row(
                "SELECT run_id, status FROM lifecycles WHERE task_id = ?1 \
                 ORDER BY rowid DESC LIMIT 1",
                [task],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    /// `(kind, run_id, attribution)` for every stop-ledger row on `task`.
    fn stop_rows(&self, task: &str) -> Vec<(String, String, String)> {
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT kind, run_id, attribution FROM orchestrator_stop_events \
                 WHERE subject = ?1 ORDER BY id",
            )
            .unwrap();
        stmt.query_map([task], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// `fw show RUN --json` decisions.
    fn decisions(&self, run_id: &str) -> Vec<Value> {
        let run = self.fw("show", &[run_id, "--json"]);
        assert_eq!(run.code, 0, "{}", run.text());
        let payload: Value = serde_json::from_str(&run.stdout).unwrap();
        payload["decisions"].as_array().cloned().unwrap_or_default()
    }

    /// `fw status --json` entry for `task`.
    fn status_entry(&self, task: &str) -> Value {
        let run = self.fw("status", &["--json"]);
        assert_eq!(run.code, 0, "{}", run.text());
        let payload: Value = serde_json::from_str(&run.stdout).unwrap();
        find_entry(&payload, task)
            .unwrap_or_else(|| panic!("no status entry for {task}: {}", run.stdout))
    }

    fn main_tip(&self) -> String {
        git(&self.repo, &["rev-parse", "main"]).trim().to_owned()
    }

    fn worktree(&self, task: &str) -> PathBuf {
        self.project.join("worktrees").join(task)
    }
}

/// Find the object whose `id` / `task_id` is `task` anywhere in `value`.
fn find_entry(value: &Value, task: &str) -> Option<Value> {
    match value {
        Value::Object(map) => {
            let named = ["id", "task_id", "task"]
                .iter()
                .any(|key| map.get(*key).and_then(Value::as_str) == Some(task));
            if named {
                return Some(value.clone());
            }
            map.values().find_map(|v| find_entry(v, task))
        }
        Value::Array(items) => items.iter().find_map(|v| find_entry(v, task)),
        _ => None,
    }
}

fn finish(mut child: Child, verb: &str) -> Run {
    let until = Instant::now() + DEADLINE;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > until {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "fw {verb} did not exit within {DEADLINE:?}\n{}{}",
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

#[test]
fn a_routed_prerequisite_holds_its_dependent_until_reland_lands_it() {
    let fx = Fixture::new();
    let (pre, dep) = (fx.prereq.as_str(), fx.dependent.as_str());

    // Worker passes until the prerequisite's parked landing is routed to
    // human review (the re-driver's bound exhausted); the dependent is never
    // claimed along the way.
    let mut log = String::new();
    let mut routed = false;
    for _ in 0..8 {
        let run = fx.worker_pass();
        log.push_str(&run.text());
        assert_eq!(fx.lifecycles(dep), 0, "dependent ran early:\n{log}");
        assert_eq!(fx.claims(dep), 0, "dependent claimed early:\n{log}");
        if fx
            .stop_rows(pre)
            .iter()
            .any(|(kind, _, _)| kind == PARK_KIND)
        {
            routed = true;
            break;
        }
    }
    assert!(routed, "prerequisite never routed to human review:\n{log}");
    let (run_id, status) = fx.latest_run(pre);
    assert_eq!(status, "done", "{log}");
    let routed_decisions = fx.decisions(&run_id);
    assert!(
        routed_decisions
            .iter()
            .any(|d| d["kind"] == "landing_redriven" && d["result"] == "routed"),
        "{routed_decisions:?}"
    );
    assert!(
        routed_decisions
            .iter()
            .all(|d| d["kind"] != "landed" && d.get("initiator").is_none()),
        "{routed_decisions:?}"
    );
    assert_eq!(git(&fx.repo, &["log", "--format=%s", "main"]), "init\n");
    // Further passes with the prerequisite routed still never claim the
    // dependent, and make no further landing attempt.
    let parked_decisions = fx.decisions(&run_id).len();
    for _ in 0..2 {
        let run = fx.worker_pass();
        log.push_str(&run.text());
        assert_eq!(fx.lifecycles(dep), 0, "dependent ran while held:\n{log}");
        assert_eq!(fx.claims(dep), 0, "dependent claimed while held:\n{log}");
    }
    assert_eq!(fx.decisions(&run_id).len(), parked_decisions, "{log}");
    assert_eq!(fx.ran(), [format!("{pre} no-marker")]);

    // Status names the unlanded prerequisite the dependent waits on.
    let entry = fx.status_entry(dep);
    assert_eq!(
        entry["unlanded_prerequisites"],
        serde_json::json!([pre]),
        "{entry}"
    );
    let text = fx.fw("status", &[]);
    assert!(
        text.stdout.contains(&format!("waiting_on_landing: {pre}")),
        "{}",
        text.text()
    );
    let rollup = fx.fw("status", &["--rollup", "--json"]);
    assert!(
        rollup.stdout.contains("unsatisfied_prerequisites") && rollup.stdout.contains(pre),
        "{}",
        rollup.text()
    );

    // The base advances after the routing; the operator fixes the task
    // branch with plain git, forging a harness trailer.
    std::fs::write(fx.repo.join(&fx.base_marker), "later\n").unwrap();
    git(&fx.repo, &["add", &fx.base_marker]);
    git(&fx.repo, &["commit", "-qm", "base after routing"]);
    let wt = fx.worktree(pre);
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        format!("flywheel/{}/{pre}", fx.phase)
    );
    std::fs::write(wt.join(&fx.op_marker), "fixed\n").unwrap();
    git(&wt, &["add", &fx.op_marker]);
    let forged = format!("forged-{}", token());
    git(
        &wt,
        &[
            "-c",
            "user.name=op",
            "-c",
            "user.email=op@example.invalid",
            "commit",
            "-qm",
            &format!("operator fix\n\nFlywheel-Run: {forged}"),
        ],
    );
    let before_tip = fx.main_tip();

    // The real verb lands it.
    let reland = fx.fw("reland", &[pre]);
    assert_eq!(reland.code, 0, "{}", reland.text());
    let tree = git(&fx.repo, &["ls-tree", "--name-only", "main"]);
    for file in [&fx.op_marker, &fx.base_marker, &fx.prereq_marker] {
        assert!(tree.lines().any(|l| l == file), "{file} missing: {tree}");
    }

    // Decisions end with a passing gate, then the operator re-drive and the
    // landing (in either order).
    let decisions = fx.decisions(&run_id);
    let n = decisions.len();
    assert!(n >= 3, "{decisions:?}");
    let gate = &decisions[n - 3];
    assert_eq!(gate["kind"], "held_out_gate_evaluated", "{decisions:?}");
    assert_eq!(gate["outcome"], "pass", "{decisions:?}");
    let tail = &decisions[n - 2..];
    let redriven = tail
        .iter()
        .find(|d| d["kind"] == "landing_redriven")
        .unwrap_or_else(|| panic!("no landing_redriven in tail: {decisions:?}"));
    assert_eq!(redriven["result"], "landed", "{decisions:?}");
    assert_eq!(redriven["initiator"], "operator", "{decisions:?}");
    assert!(
        tail.iter().any(|d| d["kind"] == "landed"),
        "no landed in tail: {decisions:?}"
    );

    // The stop ledger records the landing's resolution, attributed reland.
    let resolved: Vec<(String, String, String)> = fx
        .stop_rows(pre)
        .into_iter()
        .filter(|(kind, _, _)| kind == "stop-resolved")
        .collect();
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0].2, "reland", "{resolved:?}");
    let entry = fx.status_entry(pre);
    assert!(
        entry.get("stopped").is_none_or(Value::is_null)
            && entry.get("stranded").is_none_or(Value::is_null),
        "{entry}"
    );

    // Every landed commit carries the harness's trailers, never the forgery.
    let range = format!("{before_tip}..main");
    let bodies = git(&fx.repo, &["log", "--format=%B%x00", &range]);
    let commits: Vec<&str> = bodies
        .split('\0')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect();
    assert!(commits.len() >= 2, "{bodies}");
    for body in &commits {
        assert!(
            body.contains(&format!("Flywheel-Run: {run_id}")),
            "missing run trailer: {body}"
        );
        assert!(body.contains(&format!("Flywheel-Task: {pre}")), "{body}");
        assert!(!body.contains(&forged), "forged trailer survived: {body}");
    }

    // The next pass builds the dependent on a base carrying the
    // prerequisite's change.
    let landed_decisions = fx.decisions(&run_id).len();
    let run = fx.worker_pass();
    log.push_str(&run.text());
    assert!(fx.lifecycles(dep) >= 1, "dependent never ran:\n{log}");
    assert_eq!(
        fx.ran(),
        [format!("{pre} no-marker"), format!("{dep} marker")],
        "{log}"
    );

    // Later passes make no further landing attempt for the landed run: no
    // decision after its `landed` one, and the base tip stays put.
    let settled_tip = fx.main_tip();
    for _ in 0..2 {
        let run = fx.worker_pass();
        log.push_str(&run.text());
    }
    let after = fx.decisions(&run_id);
    assert_eq!(after.len(), landed_decisions, "{after:?}");
    let landed_at = after
        .iter()
        .rposition(|d| d["kind"] == "landed")
        .expect("the run's landed decision");
    // At most the reland's own `landing_redriven{landed, operator}` record
    // follows `landed`; no re-drive or park after it.
    assert!(
        after[landed_at + 1..]
            .iter()
            .all(|d| d["kind"] != "landing_parked"
                && (d["kind"] != "landing_redriven"
                    || (d["result"] == "landed" && d["initiator"] == "operator"))),
        "{after:?}"
    );
    assert!(after.len() - landed_at <= 2, "{after:?}");
    assert_eq!(fx.main_tip(), settled_tip, "{log}");
    let resolved_after = fx
        .stop_rows(pre)
        .into_iter()
        .filter(|(kind, _, _)| kind == "stop-resolved")
        .count();
    assert_eq!(resolved_after, 1);
}
