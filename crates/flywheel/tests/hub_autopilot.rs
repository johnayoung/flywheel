//! Autopilot from the hub, end to end: a temp flywheel-adopted git repo
//! (directory work source, SQLite store), a `claude` stand-in on PATH
//! that answers autopilot's discovery and authoring prompts and a worker's
//! iteration, and the real `fw hub serve`. Through the hub's API: start
//! the real `fw autopilot` daemon in a hub session, read what it decided
//! and why, pause and resume it, run its task with a scoped worker until
//! the task parks on its manual gate, approve that gate (the decision lands
//! in the store exactly as `fw approve` enqueues it, and the next worker
//! run lands the task), and stop it. Then the CLI verb against the same
//! hub, and the provider's deny path against a store seeded through the
//! real APIs.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use chrono::Utc;
use flywheel::hub_flywheel::{AUTOPILOT_ACTIVITY, HubFlywheel};
use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome, Status};
use flywheel_core::store::{AttemptStore, LifecycleStore};
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_hub::autopilot::{GateDecision, GateRefused};
use flywheel_hub::flywheel::FlywheelProvider;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use script::write_script;

const POLICY: &str = "[source]\nkind = \"directory\"\ntasks_dir = \".flywheel/tasks\"\n\n\
                      [paths]\ndb = \".flywheel/flywheel.sqlite\"\n\n\
                      [autopilot]\ntarget_depth = 1\ninterval_seconds = 2\n";

/// The `claude` stand-in: discovery finds a tier-6 and a (higher-scoring)
/// tier-4 finding; authoring drops the tier-4 one with a reason and
/// compiles the other into a task with a committed command grader and a
/// manual gate (the one slot of target depth 1); anything else is a
/// worker iteration that commits and asks to verify.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
IFS= read -r first || first=""
case "$first" in
*"discovery dispatcher"*)
  J='```json\n{\"verdicts\": [{\"tier\": 6, \"relevant\": true, \"reason\": \"the parser has no tests\", \"findings\": [{\"id\": \"parser-tests\", \"title\": \"Cover the parser\", \"detail\": \"no tests\", \"evidence\": [\"src/parser.sh:1\"], \"urgency\": 4, \"importance\": 6, \"blocks\": 1, \"effort\": 2, \"ready\": true}]}, {\"tier\": 4, \"relevant\": true, \"reason\": \"a README was promised\", \"findings\": [{\"id\": \"readme\", \"title\": \"Write the promised README\", \"urgency\": 5, \"importance\": 5, \"effort\": 1}]}]}\n```'
  ;;
*"headless authoring agent"*"Cover the parser"*)
  J='```json\n{\"tasks\": [{\"task\": {\"id\": \"cover-parser\", \"goal\": \"Cover the parser with tests.\", \"graders\": [{\"type\": \"command\", \"name\": \"check\", \"run\": \"sh check.sh\"}, {\"type\": \"manual\", \"instruction\": \"Confirm the parser fixtures are realistic\"}], \"tags\": [], \"context\": {}}, \"authoritative_grader\": \"sh check.sh\", \"grader_source\": \"repo_command\", \"grader_target\": \"check.sh\", \"creates_files\": [\"tests/parser_test.sh\"], \"assumptions\": [\"shell tests are fine\"]}], \"dropped\": \"\"}\n```'
  ;;
*"headless authoring agent"*)
  J='```json\n{\"tasks\": [], \"dropped\": \"no committed check can grade a README\"}\n```'
  ;;
*)
  id=$(basename "$PWD")
  date +%s%N > "work-$id.txt"
  git add -A >/dev/null 2>&1 && git commit -qm "work $id" >/dev/null 2>&1
  J='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
  ;;
esac
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$J"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// The adopted repo: a committed check (`check.sh`) the authored task's
/// grader runs, and nothing queued.
fn adopted_repo(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("flywheel.toml"), POLICY).unwrap();
    std::fs::write(root.join(".gitignore"), ".flywheel/\n").unwrap();
    std::fs::write(root.join("check.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::write(root.join("src/parser.sh"), "echo parse\n").unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(
        root,
        &["config", "user.email", "hub-autopilot@example.invalid"],
    );
    git(root, &["config", "user.name", "hub autopilot"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "adopt"]);
}

struct Server {
    child: Child,
    port: u16,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn path_with_bin(dir: &Path) -> String {
    format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn start(dir: &Path) -> Server {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(["hub", "serve", "--port", "0", "--state-dir"])
        .arg(dir.join("state"))
        .env("PATH", path_with_bin(dir))
        .env("SHELL", "/bin/sh")
        .env("HOME", &home)
        .env_remove("FLYWHEEL_HOME")
        .env("CLAUDE_CONFIG_DIR", dir.join("claude-config"))
        .env("CODEX_HOME", dir.join("codex-home"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let port = first
        .rsplit(':')
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("unexpected banner {first:?}"));
    std::thread::spawn(move || for _ in lines {});
    let token = std::fs::read_to_string(dir.join("state/token"))
        .unwrap()
        .trim()
        .to_owned();
    Server { child, port, token }
}

fn http(server: &Server, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        server.port,
        server.token,
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw);
    let (head, payload) = raw.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let payload = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(payload)
    } else {
        payload.to_owned()
    };
    (
        status,
        serde_json::from_str(&payload).unwrap_or(Value::Null),
    )
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&tail[..size]);
        rest = &tail[size + 2..];
    }
    out
}

fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn query(path: &Path) -> String {
    path.to_string_lossy()
        .replace('%', "%25")
        .replace('/', "%2F")
}

fn screen_text(server: &Server, sid: &str) -> String {
    let (_, screen) = http(server, "GET", &format!("/api/sessions/{sid}/screen"), None);
    screen["lines"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The pending control commands for `run_id`, read without claiming them.
fn queued(db: &Path, run_id: &str) -> Vec<(String, Value)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT kind, payload_json FROM control_commands \
             WHERE run_id = ?1 AND claimed_at IS NULL ORDER BY id",
        )
        .unwrap();
    statement
        .query_map([run_id], |row| {
            let kind: String = row.get(0)?;
            let payload: String = row.get(1)?;
            Ok((kind, serde_json::from_str(&payload).unwrap()))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn activity_kinds(server: &Server) -> Vec<String> {
    let (code, page) = http(server, "GET", "/api/activity?machines=none&limit=200", None);
    assert_eq!(code, 200, "{page}");
    page["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["kind"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn the_hub_starts_reads_pauses_gates_and_stops_the_real_autopilot() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    write_script(&dir.join("bin/claude"), FAKE_CLAUDE);
    let root = dir.join("repo");
    adopted_repo(&root);
    let db = root.join(".flywheel/flywheel.sqlite");
    let server = start(&dir);
    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "pilot", "folders": [{"path": root}] })),
    );
    assert_eq!(code, 201, "{workspace}");
    let workspace_id = workspace["id"].as_str().unwrap().to_owned();
    let view = || {
        let (code, answer) = http(
            &server,
            "GET",
            &format!("/api/flywheel/autopilot?folder={}", query(&root)),
            None,
        );
        assert_eq!(code, 200, "{answer}");
        answer
    };

    // Watched (events are changes between the monitor's reads), never
    // ran here: absent, with the policy's configuration and the tier
    // model.
    wait_for("the monitor to watch the folder", || {
        let (_, list) = http(&server, "GET", "/api/flywheel", None);
        (list["folders"][0]["folder"] == json!(root)).then_some(())
    });
    let before = view();
    assert_eq!(before["view"]["state"], "absent");
    assert_eq!(before["view"]["live"], false);
    assert_eq!(before["view"]["tiers"].as_array().unwrap().len(), 11);
    assert_eq!(before["view"]["config"]["landing"], "merge");

    // Start is confirmed: autopilot writes the base branch unattended.
    let start_body = |confirm: bool| json!({ "folder": root, "workspace_id": workspace_id, "confirm": confirm, "cols": 120, "rows": 30 });
    let (code, refused) = http(
        &server,
        "POST",
        "/api/flywheel/autopilot/start",
        Some(start_body(false)),
    );
    assert_eq!(code, 400, "{refused}");
    let (code, session) = http(
        &server,
        "POST",
        "/api/flywheel/autopilot/start",
        Some(start_body(true)),
    );
    assert_eq!(code, 201, "{session}");
    assert_eq!(session["name"], "fw autopilot");
    let argv: Vec<&str> = session["command"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(argv[3], "fw-autopilot");
    // The folder's flywheel.toml is named: the daemon no longer
    // auto-detects it.
    let policy_flag = format!("--policy={}", root.join("flywheel.toml").display());
    assert_eq!(
        argv[5..],
        ["autopilot", policy_flag.as_str()],
        "the daemon, never --once"
    );
    let sid = session["id"].as_str().unwrap().to_owned();

    // Its first cycle: what it decided, and why.
    let decided = wait_for("the first cycle's decision", || {
        let answer = view();
        (answer["view"]["decision"]["emitted"] == 1).then_some(answer)
    });
    let pilot = &decided["view"];
    assert_eq!(pilot["live"], true, "{pilot}");
    assert_eq!(decided["sessions"][0]["id"], json!(sid));
    let decision = &pilot["decision"];
    assert_eq!(decision["dropped"], 1);
    assert_eq!(decision["emitted_tasks"][0]["task_id"], "cover-parser");
    assert_eq!(
        decision["dropped_findings"][0]["reason"],
        "no committed check can grade a README"
    );
    assert_eq!(decision["dropped_findings"][0]["tier"], 4);
    let tier = |n: usize| pilot["tiers"][n - 1].clone();
    assert_eq!(tier(1)["verdict"], "prefiltered");
    assert_eq!(tier(1)["preemptive"], true);
    assert_eq!(tier(6)["verdict"], "relevant");
    assert_eq!(tier(6)["emitted"], 1);
    assert_eq!(tier(4)["verdict"], "relevant");
    assert_eq!(tier(4)["emitted"], 0);
    let candidate = &pilot["candidates"][0];
    assert_eq!(candidate["task_id"], "cover-parser");
    assert_eq!(candidate["tier"], 6);
    assert_eq!(candidate["score"], 90.0);
    assert_eq!(candidate["grader_source"], "repo_command");
    assert_eq!(candidate["assumptions"], json!(["shell tests are fine"]));
    assert_eq!(candidate["state"], "pending");

    wait_for("the started and decision rows", || {
        let kinds = activity_kinds(&server);
        ["autopilot.started", "autopilot.decision"]
            .iter()
            .all(|k| kinds.iter().any(|seen| seen == k))
            .then_some(())
    });

    // One daemon per folder.
    let (code, again) = http(
        &server,
        "POST",
        "/api/flywheel/autopilot/start",
        Some(start_body(true)),
    );
    assert_eq!(code, 409, "{again}");
    assert_eq!(again["running"]["id"], json!(sid));

    // Pause and resume through the daemon's own marker.
    let marker = PathBuf::from(format!(
        "{}.paused",
        root.join(AUTOPILOT_ACTIVITY).display()
    ));
    let (code, paused) = http(
        &server,
        "POST",
        "/api/flywheel/autopilot/pause",
        Some(json!({ "folder": root })),
    );
    assert_eq!(code, 200, "{paused}");
    assert_eq!(paused, json!({ "paused": true, "changed": true }));
    assert!(marker.is_file());
    wait_for("the daemon to hold", || {
        (view()["view"]["state"] == "paused").then_some(())
    });
    let (code, resumed) = http(
        &server,
        "POST",
        "/api/flywheel/autopilot/resume",
        Some(json!({ "folder": root })),
    );
    assert_eq!(code, 200, "{resumed}");
    assert!(!marker.exists());
    wait_for("the daemon to run cycles again", || {
        let state = view()["view"]["state"].clone();
        (state == "idle" || state == "running").then_some(())
    });

    // Its task, run by a scoped worker, parks on the manual gate.
    let (code, worker) = http(
        &server,
        "POST",
        "/api/flywheel/worker",
        Some(json!({ "folder": root, "workspace_id": workspace_id, "tasks": ["cover-parser"] })),
    );
    assert_eq!(code, 201, "{worker}");
    let worker_id = worker["id"].as_str().unwrap().to_owned();
    wait_for("the scoped worker to settle", || {
        screen_text(&server, &worker_id)
            .contains("fw worker exited with status")
            .then_some(())
    });
    let gated = wait_for("the gate to show", || {
        let answer = view();
        (!answer["view"]["gates"].as_array().unwrap().is_empty()).then_some(answer)
    });
    let gate = gated["view"]["gates"][0].clone();
    assert_eq!(gate["task_id"], "cover-parser");
    assert_eq!(gate["autopilot"], true);
    assert_eq!(
        gate["instruction"],
        "Confirm the parser fixtures are realistic"
    );
    let run_id = gate["run_id"].as_str().unwrap().to_owned();
    let ordinal = gate["gate"].as_u64().unwrap();
    assert_eq!(gate["attempt"], 1);
    assert_eq!(gate["pending"], Value::Null);
    // The folder's summary counts it (sidebar badge, Needs you).
    wait_for("the summary to count the gate", || {
        let (_, list) = http(&server, "GET", "/api/flywheel", None);
        (list["folders"][0]["autopilot_gates"] == 1).then_some(())
    });
    // Needs you says it once: the awaiting alert, not also the human-gate
    // stop `fw status` keeps.
    let (_, panel) = http(
        &server,
        "GET",
        &format!("/api/flywheel/panel?folder={}", query(&root)),
        None,
    );
    let alerts: Vec<&str> = panel["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["kind"].as_str())
        .collect();
    assert_eq!(alerts, ["awaiting"], "{panel}");
    // Cycles since the first held the queue at its target: why it emitted
    // and dropped is still there.
    let later = view();
    let kept = &later["view"]["last_decided"];
    assert_eq!(kept["cycle"], decision["cycle"], "{later}");
    assert_eq!(
        kept["dropped_findings"][0]["reason"],
        "no committed check can grade a README"
    );
    assert_eq!(later["view"]["tiers"][5]["verdict"], "relevant");
    assert_eq!(later["view"]["tiers"][3]["dropped"], 1);

    let decide = |body: Value| {
        http(
            &server,
            "POST",
            "/api/flywheel/autopilot/decide",
            Some(body),
        )
    };
    // Deny with feedback: the run goes back for another attempt, and only
    // one decision can be queued on a gate.
    let (code, denied) = decide(json!({
        "folder": root, "run": run_id, "decision": "deny", "confirm": true,
        "gate": ordinal, "attempt": 1, "feedback": "use real parser samples",
    }));
    assert_eq!(code, 200, "{denied}");
    assert_eq!(denied["kind"], "reject");
    let pending = wait_for("the gate to show its queued decision", || {
        let answer = view();
        let gate = answer["view"]["gates"][0].clone();
        (gate["pending"]["kind"] == "reject").then_some(gate)
    });
    assert_eq!(pending["pending"]["command_id"], denied["command_id"]);
    let (code, twice) = decide(json!({
        "folder": root, "run": run_id, "decision": "approve", "confirm": true,
        "gate": ordinal, "attempt": 1,
    }));
    assert_eq!(code, 409, "{twice}");
    assert_eq!(queued(&db, &run_id).len(), 1, "nothing more enqueued");
    let row = wait_for("the decision's activity row", || {
        let (_, page) = http(
            &server,
            "GET",
            "/api/activity?machines=none&limit=200",
            None,
        );
        page["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["kind"] == "autopilot.denied")
            .cloned()
    });
    assert_eq!(
        row["data"]["run_id"],
        json!(run_id),
        "a run id is not a secret"
    );
    assert_eq!(row["data"]["who"], "hub");
    assert_eq!(row["data"]["gate"], json!(ordinal));
    assert_eq!(row["data"]["attempt"], 1);
    assert_eq!(row["data"]["feedback"], true);
    assert!(
        !row.to_string().contains("use real parser samples"),
        "the feedback stays in the store: {row}"
    );
    // Decided: Needs you lets go of it (the panel still shows it, marked).
    wait_for("Needs you to drop the decided gate", || {
        let (_, list) = http(&server, "GET", "/api/flywheel", None);
        let folder = &list["folders"][0];
        let awaiting = folder["alerts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|a| a["kind"] == "awaiting");
        (folder["autopilot_gates"] == 0 && !awaiting).then_some(())
    });
    // The panel's history keeps it with the cycle decision before it.
    let kinds_of = |answer: &Value| -> Vec<String> {
        answer["view"]["history"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["kind"].as_str().map(str::to_owned))
            .collect()
    };
    let history = wait_for("the history to hold the deny", || {
        let answer = view();
        let kinds = kinds_of(&answer);
        (kinds.first().map(String::as_str) == Some("autopilot.denied")).then_some(kinds)
    });
    assert!(
        history.iter().any(|k| k == "autopilot.decision"),
        "{history:?}"
    );

    // The next worker run applies the deny and retries the task on the
    // same run (the feedback in its prompt): it parks on the gate again,
    // attempt 2.
    let (code, worker) = http(
        &server,
        "POST",
        "/api/flywheel/worker",
        Some(json!({ "folder": root, "workspace_id": workspace_id, "tasks": ["cover-parser"] })),
    );
    assert_eq!(code, 201, "{worker}");
    let worker_id = worker["id"].as_str().unwrap().to_owned();
    wait_for("the retried task to settle", || {
        screen_text(&server, &worker_id)
            .contains("fw worker exited with status")
            .then_some(())
    });
    let regated = wait_for("the retry's gate to show", || {
        let answer = view();
        let gate = answer["view"]["gates"][0].clone();
        (gate["attempt"] == 2).then_some(gate)
    });
    assert_eq!(regated["run_id"], json!(run_id), "the same run, retried");
    assert_eq!(regated["pending"], Value::Null);
    let attempt = 2;

    // Approve: confirmed, pinned to the gate shown; a stale pin is
    // refused and enqueues nothing.
    let (code, _) = decide(json!({
        "folder": root, "run": run_id, "decision": "approve", "gate": ordinal, "attempt": attempt,
    }));
    assert_eq!(code, 400, "unconfirmed");
    let (code, stale) = decide(json!({
        "folder": root, "run": run_id, "decision": "approve", "confirm": true,
        "gate": ordinal, "attempt": 1,
    }));
    assert_eq!(code, 409, "{stale}");
    assert!(queued(&db, &run_id).is_empty());
    let (code, unknown) = decide(json!({
        "folder": root, "run": "run-nope", "decision": "approve", "confirm": true,
    }));
    assert_eq!(code, 404, "{unknown}");
    let (code, approved) = decide(json!({
        "folder": root, "run": run_id, "decision": "approve", "confirm": true,
        "gate": ordinal, "attempt": attempt,
    }));
    assert_eq!(code, 200, "{approved}");
    assert_eq!(approved["kind"], "approve");
    assert_eq!(approved["task_id"], "cover-parser");
    assert_eq!(
        queued(&db, &run_id),
        vec![(
            "approve".to_owned(),
            json!({ "awaiting_ordinal": ordinal, "attempt_number": attempt })
        )],
        "exactly what `fw approve --gate --attempt` enqueues"
    );

    wait_for("the approve row", || {
        kinds_of(&view())
            .first()
            .is_some_and(|k| k == "autopilot.approved")
            .then_some(())
    });

    // Plugin events, journaled as activity rows.
    let kinds = wait_for("the approval row", || {
        let kinds = activity_kinds(&server);
        kinds
            .iter()
            .any(|k| k == "autopilot.approval_needed")
            .then_some(kinds)
    });
    assert!(!kinds.iter().any(|k| k == "autopilot.stopped"), "{kinds:?}");
    // The emit, then the queue held at its target: each cycle that only
    // repeats that is not news.
    let decisions = |kinds: &[String]| kinds.iter().filter(|k| *k == "autopilot.decision").count();
    let announced = decisions(&kinds);
    assert!((1..=2).contains(&announced), "{kinds:?}");

    // Stop: Ctrl-C to the session's daemon; it stops gracefully and the
    // session becomes its shell.
    let (code, stopping) = http(
        &server,
        "POST",
        "/api/flywheel/autopilot/stop",
        Some(json!({ "folder": root })),
    );
    assert_eq!(code, 202, "{stopping}");
    assert_eq!(stopping["stopping"][0]["id"], json!(sid));
    let screen = wait_for("the daemon to exit", || {
        let text = screen_text(&server, &sid);
        text.contains("fw autopilot exited with status")
            .then_some(text)
    });
    assert!(screen.contains("daemon stopped after"), "{screen}");
    wait_for("the view to say stopped", || {
        let answer = view();
        (answer["view"]["state"] == "stopped" && answer["view"]["live"] == false).then_some(())
    });
    let kinds = wait_for("the stopped event", || {
        let kinds = activity_kinds(&server);
        kinds
            .iter()
            .any(|k| k == "autopilot.stopped")
            .then_some(kinds)
    });
    assert_eq!(decisions(&kinds), announced, "{kinds:?}");
    let (code, _) = http(
        &server,
        "POST",
        "/api/flywheel/autopilot/stop",
        Some(json!({ "folder": root })),
    );
    assert_eq!(code, 409, "nothing runs now");

    // The approval is applied by the next worker run: the task lands.
    let (code, worker) = http(
        &server,
        "POST",
        "/api/flywheel/worker",
        Some(json!({ "folder": root, "workspace_id": workspace_id, "tasks": ["cover-parser"] })),
    );
    assert_eq!(code, 201, "{worker}");
    let worker_id = worker["id"].as_str().unwrap().to_owned();
    let screen = wait_for("the approved task to land", || {
        let text = screen_text(&server, &worker_id);
        text.contains("fw worker exited with status")
            .then_some(text)
    });
    assert!(screen.contains("cover-parser verified"), "{screen}");
    let landed = wait_for("the view to show it verified", || {
        let answer = view();
        (answer["view"]["candidates"][0]["state"] == "verified").then_some(answer)
    });
    assert_eq!(landed["view"]["candidates"][0]["archived"], true);
    assert!(landed["view"]["gates"].as_array().unwrap().is_empty());

    // The CLI verb against the same hub.
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(["hub", "autopilot"])
            .arg(&root)
            .args(args)
            .arg("--state-dir")
            .arg(dir.join("state"))
            .env("HOME", dir.join("home"))
            .env_remove("FLYWHEEL_HOME")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let status = cli(&["status"]);
    let text = String::from_utf8_lossy(&status.stdout);
    assert_eq!(status.status.code(), Some(0), "{text}");
    assert!(text.starts_with("autopilot: stopped"), "{text}");
    // The last cycle (the queue held at its target), and the emitted
    // task with its score and landed run.
    assert!(
        text.contains("already at or above target 1; nothing emitted"),
        "{text}"
    );
    // ...and, kept since, the cycle that emitted it and why it dropped the
    // other finding.
    assert!(text.contains("emitted 1, dropped 1"), "{text}");
    assert!(
        text.contains("(tier 4): no committed check can grade a README"),
        "{text}"
    );
    assert!(
        text.contains("verified   tier  6 score     90.0  cover-parser  (run run-"),
        "{text}"
    );
    // Its decision history: the gate decisions after the cycle's.
    let history = text.split("decisions (newest first):").nth(1).unwrap_or("");
    let approved = history.find("approved gate").expect(&text);
    let denied = history.find("denied gate").expect(&text);
    let cycle = history.find("emitted 1, dropped 1").expect(&text);
    assert!(approved < denied && denied < cycle, "{text}");
    let unconfirmed = cli(&["start"]);
    assert_eq!(unconfirmed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&unconfirmed.stderr).contains("not started"));
    let paused = cli(&["pause", "--json"]);
    let answer: Value = serde_json::from_slice(&paused.stdout).unwrap();
    assert_eq!(answer, json!({ "paused": true, "changed": true }));
    assert!(marker.is_file());
    assert_eq!(cli(&["resume"]).status.code(), Some(0));
    assert!(!marker.exists());
}

fn seed_parked(db: &Path, task_id: &str, run_id: &str) {
    let store = SqliteStore::open(db).unwrap();
    let now = Utc::now();
    let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
    for status in [
        Status::Ready,
        Status::Running,
        Status::Validating,
        Status::AwaitingApproval,
    ] {
        lc.transition_to(status, "", Some(now)).unwrap();
    }
    lc.awaiting_manual_ordinal = Some(2);
    store.create_lifecycle(&lc).unwrap();
    let mut attempt = Attempt::new(1, now, run_id);
    attempt.outcome = Some(Outcome::Succeeded);
    store.save_attempt(run_id, &attempt, None).unwrap();
}

#[test]
fn a_deny_carries_feedback_and_binds_to_the_parked_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("flywheel.toml"), POLICY).unwrap();
    let db = root.join(".flywheel/flywheel.sqlite");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    seed_parked(&db, "gated", "run-gated");
    let provider = HubFlywheel::new("/usr/bin/false");

    let deny = |gate: Option<u32>, attempt: Option<u32>| {
        provider.decide_gate(
            &root,
            &GateDecision {
                run_id: "run-gated".into(),
                approve: false,
                feedback: Some("fixtures are synthetic".into()),
                gate,
                attempt,
            },
        )
    };
    match deny(Some(1), None) {
        Err(GateRefused::NotThatGate(message)) => {
            assert!(message.contains("awaiting_ordinal=2"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(queued(&db, "run-gated").is_empty());
    let denied = deny(Some(2), Some(1)).unwrap();
    assert_eq!(denied.kind, "reject");
    assert_eq!((denied.gate, denied.attempt), (2, 1));
    assert_eq!(denied.task_id, "gated");
    assert_eq!(
        queued(&db, "run-gated"),
        vec![(
            "reject".to_owned(),
            json!({ "feedback": "fixtures are synthetic", "awaiting_ordinal": 2, "attempt_number": 1 })
        )]
    );
    assert!(matches!(
        provider.decide_gate(
            &root,
            &GateDecision {
                run_id: "run-missing".into(),
                approve: true,
                feedback: None,
                gate: None,
                attempt: None,
            }
        ),
        Err(GateRefused::UnknownRun(_))
    ));

    // Pause and resume write the daemon's marker, as `fw autopilot
    // --pause` does.
    assert!(provider.set_autopilot_paused(&root, true).unwrap());
    let view = provider.panel(&root).intake.unwrap();
    assert!(view.paused);
    assert_eq!(view.state.as_str(), "absent");
    assert_eq!(view.gates.len(), 0, "the gated task is not listed");
    assert!(provider.set_autopilot_paused(&root, false).unwrap());
}
