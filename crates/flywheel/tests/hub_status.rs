//! `fw hub` agent status through the real binaries: `fw hub serve` launches
//! scripted stand-ins for `claude` and `codex`, and this test runs the hook
//! commands the hub registered -- the real `fw hub report` -- exactly as the
//! agent CLIs would (Claude Code: the `--settings` hook argv with the event
//! JSON on stdin; Codex: the `-c notify=[...]` argv plus the payload).

#![cfg(unix)]

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

/// Minimal HTTP/1.1 against the hub (bearer auth, JSON, `Connection: close`).
fn http(server: &Server, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
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
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, payload) = raw.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    // The hub answers small JSON bodies with Content-Length (no chunking).
    let value = serde_json::from_str(payload).unwrap_or(Value::Null);
    (status, value)
}

fn fake_agent(dir: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("bin").join(name);
    let dump = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nenv > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\n\
             printf '%s\\n' \"$@\" > '{d}.args.tmp' && mv '{d}.args.tmp' '{d}.args'\nexec cat\n",
            d = dump.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn launched(dir: &Path, name: &str) -> (Vec<String>, HashMap<String, String>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let (Ok(args), Ok(env)) = (
            std::fs::read_to_string(dir.join(format!("{name}.args"))),
            std::fs::read_to_string(dir.join(format!("{name}.env"))),
        ) {
            let env = env
                .lines()
                .filter_map(|l| l.split_once('='))
                .filter(|(k, _)| k.starts_with("FLYWHEEL_HUB_"))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            return (args.lines().map(str::to_owned).collect(), env);
        }
        assert!(Instant::now() < deadline, "{name} never launched");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn start(dir: &Path) -> Server {
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(["hub", "serve", "--port", "0", "--state-dir"])
        .arg(dir.join("state"))
        .env("PATH", path)
        .env("CLAUDE_CONFIG_DIR", dir.join("claude-config"))
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
    // Keep draining stdout so the server never blocks on it.
    std::thread::spawn(move || for _ in lines {});
    let token = std::fs::read_to_string(dir.join("state/token"))
        .unwrap()
        .trim()
        .to_owned();
    Server { child, port, token }
}

/// Run one hook command (argv + stdin) with the session's environment, as
/// the agent CLI would; it must succeed silently on stdout.
fn run_hook(argv: &[String], env: &HashMap<String, String>, stdin: &str) {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{argv:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "hooks must not print to stdout");
}

fn agent_status(server: &Server, sid: &str) -> String {
    let (_, view) = http(server, "GET", &format!("/api/sessions/{sid}"), None);
    view["agent"]["status"].as_str().unwrap_or("").to_owned()
}

fn claude_hook(settings: &Value, event: &str, matcher: Option<&str>) -> Vec<String> {
    let group = settings["hooks"][event]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g.get("matcher").and_then(Value::as_str) == matcher)
        .unwrap()
        .clone();
    let hook = &group["hooks"][0];
    std::iter::once(hook["command"].as_str().unwrap().to_owned())
        .chain(
            hook["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_owned()),
        )
        .collect()
}

/// `notify=["a","b",...]` (TOML strings without escapes here) -> argv.
fn codex_notify(arg: &str) -> Vec<String> {
    let list = arg
        .strip_prefix("notify=[")
        .and_then(|r| r.strip_suffix(']'))
        .unwrap();
    list.split("\",\"")
        .map(|s| s.trim_matches('"').to_owned())
        .collect()
}

#[test]
fn hooks_run_fw_hub_report_and_drive_agent_status() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("repo/.git")).unwrap();
    fake_agent(&dir, "claude");
    fake_agent(&dir, "codex");
    let server = start(&dir);

    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "e2e", "repos": [dir.join("repo")] })),
    );
    assert_eq!(code, 201, "{workspace}");
    let (code, session) = http(
        &server,
        "POST",
        "/api/sessions",
        Some(json!({ "workspace_id": workspace["id"], "kind": "claude" })),
    );
    assert_eq!(code, 201, "{session}");
    let sid = session["id"].as_str().unwrap().to_owned();
    let (args, env) = launched(&dir, "claude");
    assert_eq!(args[0], "--session-id");
    assert_eq!(args[1], session["agent_session_id"].as_str().unwrap());
    let settings: Value = serde_json::from_str(&args[3]).unwrap();
    // The hooks call this very binary.
    let start_hook = claude_hook(&settings, "SessionStart", None);
    assert_eq!(
        PathBuf::from(&start_hook[0]),
        PathBuf::from(env!("CARGO_BIN_EXE_fw"))
    );

    let wait = |want: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got = agent_status(&server, &sid);
            if got == want {
                return;
            }
            assert!(Instant::now() < deadline, "status {got:?}, want {want:?}");
            std::thread::sleep(Duration::from_millis(30));
        }
    };
    let payload = json!({ "session_id": args[1], "hook_event_name": "x" }).to_string();
    run_hook(&start_hook, &env, &payload);
    wait("idle");
    run_hook(
        &claude_hook(&settings, "UserPromptSubmit", None),
        &env,
        &payload,
    );
    wait("working");
    run_hook(
        &claude_hook(&settings, "Notification", Some("permission_prompt")),
        &env,
        &json!({ "message": "Claude needs your permission to use Bash" }).to_string(),
    );
    wait("blocked");
    let (_, view) = http(&server, "GET", &format!("/api/sessions/{sid}"), None);
    assert_eq!(
        view["agent"]["detail"],
        "Claude needs your permission to use Bash"
    );
    run_hook(&claude_hook(&settings, "PostToolUse", None), &env, &payload);
    wait("working");
    // A huge payload is cut to fit, not refused.
    let big = json!({ "session_id": args[1], "last_assistant_message": "x".repeat(2_000_000) });
    run_hook(
        &claude_hook(&settings, "Stop", None),
        &env,
        &big.to_string(),
    );
    wait("done");

    // Without the session's environment, report fails -- exit 1, not 2.
    let report = Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(["hub", "report", "--source", "claude", "--event", "Stop"])
        .env_remove("FLYWHEEL_HUB_REPORT_SECRET")
        .output()
        .unwrap();
    assert_eq!(report.status.code(), Some(1));
    assert!(report.stdout.is_empty());

    // Codex: run its notify argv with the payload appended.
    let (code, codex) = http(
        &server,
        "POST",
        "/api/sessions",
        Some(json!({ "workspace_id": workspace["id"], "kind": "codex" })),
    );
    assert_eq!(code, 201, "{codex}");
    let (args, codex_env) = launched(&dir, "codex");
    assert_eq!(args[0], "-c");
    let mut notify = codex_notify(&args[1]);
    notify.push(json!({ "type": "agent-turn-complete", "thread-id": "thr_9" }).to_string());
    run_hook(&notify, &codex_env, "");
    let cid = codex["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while agent_status(&server, cid) != "done" {
        assert!(Instant::now() < deadline, "codex never done");
        std::thread::sleep(Duration::from_millis(30));
    }

    // SIGTERM: the hub stops and removes its report socket.
    let socket = dir.join("state/report.sock");
    assert!(socket.exists());
    let mut server = server;
    unsafe {
        libc::kill(server.child.id() as i32, libc::SIGTERM);
    }
    let status = server.child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(!socket.exists());
}
