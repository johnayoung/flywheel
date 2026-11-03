//! `fw hub attach` and `fw hub wait` end to end: a real `fw hub serve`
//! (temp state dir, free port, stand-in `claude` first on PATH), the real
//! `fw hub attach` running inside a real PTY whose output is rendered by a
//! terminal emulator, and `fw hub wait` released by the real `fw hub
//! report` hooks the hub registered with the agent.

#![cfg(unix)]

use flywheel_hub::vt::{Emulator, TextSource};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FW: &str = env!("CARGO_BIN_EXE_fw");
const WAIT: Duration = Duration::from_secs(20);

struct Server {
    child: std::process::Child,
    port: u16,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

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
    (status, serde_json::from_str(payload).unwrap_or(Value::Null))
}

/// A stand-in `claude`: records its argv and environment, then echoes.
fn fake_claude(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("bin/claude");
    let dump = dir.join("claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nenv > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\n\
             printf '%s\\n' \"$@\" > '{d}.args.tmp' && mv '{d}.args.tmp' '{d}.args'\n\
             echo fake-claude-ready\nexec cat\n",
            d = dump.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_env(dir: &Path) -> String {
    format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn start(dir: &Path) -> Server {
    let mut child = Command::new(FW)
        .args([
            "hub",
            "serve",
            "--port",
            "0",
            "--no-flywheel",
            "--state-dir",
        ])
        .arg(dir.join("state"))
        .env("PATH", path_env(dir))
        .env("SHELL", "/bin/sh")
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
    std::thread::spawn(move || for _ in lines {});
    let token = std::fs::read_to_string(dir.join("state/token"))
        .unwrap()
        .trim()
        .to_owned();
    Server { child, port, token }
}

/// `fw hub attach ARGS` in a PTY of `cols` x `rows`, its output rendered.
struct Attached {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<Emulator>>,
    raw: Arc<Mutex<Vec<u8>>>,
    /// Stop reading the client's output (a terminal that cannot keep up).
    paused: Arc<std::sync::atomic::AtomicBool>,
}

impl Attached {
    fn spawn(dir: &Path, args: &[&str], cols: u16, rows: u16) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(FW);
        cmd.args(["hub", "attach"]);
        cmd.args(args);
        cmd.arg("--state-dir");
        cmd.arg(dir.join("state"));
        cmd.env("TERM", "xterm-256color");
        cmd.env("PATH", path_env(dir));
        cmd.env_remove("FW_HUB_TOKEN");
        cmd.cwd(dir);
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(Emulator::new(cols, rows, 1000)));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s, r, p) = (Arc::clone(&screen), Arc::clone(&raw), Arc::clone(&paused));
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                while p.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let Ok(n) = reader.read(&mut buf) else {
                    break;
                };
                if n == 0 {
                    break;
                }
                s.lock().unwrap().process(&buf[..n]);
                r.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self {
            child,
            master: pair.master,
            writer,
            screen,
            raw,
            paused,
        }
    }

    fn pause(&self, on: bool) {
        self.paused.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// The client's resident memory (KiB).
    fn rss_kib(&self) -> u64 {
        let pid = self.child.process_id().unwrap();
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap()
    }

    fn lines(&self) -> Vec<String> {
        self.screen.lock().unwrap().text(TextSource::Visible).lines
    }

    fn wait_for(&self, what: &str, test: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let lines = self.lines();
            if test(&lines) {
                return lines;
            }
            assert!(
                Instant::now() < deadline,
                "never saw {what}; screen:\n{}",
                lines.join("\n")
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn wait_text(&self, needle: &str) -> Vec<String> {
        self.wait_for(needle, |lines| lines.iter().any(|l| l.contains(needle)))
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.screen.lock().unwrap().resize(cols, rows);
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }

    /// Wait for the client to exit; its exit code.
    fn exit_code(&mut self) -> u32 {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            assert!(Instant::now() < deadline, "fw hub attach did not exit");
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw.lock().unwrap()).into_owned()
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn screen_text(server: &Server, sid: &str) -> String {
    let (code, screen) = http(
        server,
        "GET",
        &format!("/api/sessions/{sid}/screen?source=recent"),
        None,
    );
    assert_eq!(code, 200, "{screen}");
    screen["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_until(what: &str, mut test: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !test() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, Server, String) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    fake_claude(&dir);
    let server = start(&dir);
    let (code, workspace) = http(
        &server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "demo", "folders": [dir.join("repo")] })),
    );
    assert_eq!(code, 201, "{workspace}");
    let ws = workspace["id"].as_str().unwrap().to_owned();
    (tmp, dir, server, ws)
}

#[test]
fn attach_shows_the_snapshot_forwards_input_and_detaches_leaving_the_session_running() {
    let (_tmp, dir, server, ws) = setup();
    let (code, session) = http(
        &server,
        "POST",
        "/api/sessions",
        Some(json!({ "workspace_id": ws, "name": "work", "command": ["/bin/sh"] })),
    );
    assert_eq!(code, 201, "{session}");
    let sid = session["id"].as_str().unwrap().to_owned();
    let (code, _) = http(
        &server,
        "POST",
        &format!("/api/sessions/{sid}/input"),
        Some(json!({ "data": "echo before-$((40+2))\r" })),
    );
    assert_eq!(code, 200);
    wait_until("the shell ran the command", || {
        screen_text(&server, &sid).contains("before-42")
    });

    // Attach: the snapshot replays what ran before; the status bar sits on
    // the last row and the session gets the rows above it.
    let mut term = Attached::spawn(&dir, &["work"], 80, 24);
    term.wait_text("before-42");
    let lines = term.wait_for("the status bar", |l| {
        l[23].contains("demo/work") && l[23].contains("0 blocked")
    });
    assert!(lines[23].contains("C-b ? help"), "{}", lines[23]);
    term.type_bytes(b"stty size; echo typed-$((6*7))\r");
    term.wait_text("typed-42");
    term.wait_text("23 80");

    // SIGWINCH: the session follows the terminal's new size.
    term.resize(100, 30);
    term.wait_for("the bar on the new last row", |l| {
        l.len() == 30 && l[29].contains("demo/work")
    });
    term.type_bytes(b"stty size\r");
    term.wait_text("29 100");

    // Prefix twice sends the prefix itself (Ctrl+B = 0x02).
    term.type_bytes(b"od -An -c\r");
    std::thread::sleep(Duration::from_millis(300));
    term.type_bytes(b"\x02\x02\r\x04");
    term.wait_text("002");

    // The bar toggles off: the session gets every row.
    term.type_bytes(b"\x02t");
    std::thread::sleep(Duration::from_millis(300));
    term.type_bytes(b"clear; stty size\r");
    term.wait_text("30 100");
    term.type_bytes(b"\x02t");
    std::thread::sleep(Duration::from_millis(300));
    term.type_bytes(b"clear; stty size\r");
    term.wait_text("29 100");

    // Detach: the client exits 0, restoring the terminal; the session runs.
    term.type_bytes(b"\x02d");
    assert_eq!(term.exit_code(), 0);
    assert!(term.raw_text().contains("[detached from demo/work]"));
    let raw = term.raw_text();
    let tail = &raw[raw.rfind("\x1b[?2026l").expect("terminal reset on detach")..];
    assert!(
        tail.contains("\x1b[r") && tail.contains("\x1b[?2004l"),
        "{tail:?}"
    );
    let (_, view) = http(&server, "GET", &format!("/api/sessions/{sid}"), None);
    assert_eq!(view["state"], "running", "{view}");
    assert!(view["live"]["pid"].is_u64(), "{view}");
    // It keeps running while nobody is attached.
    http(
        &server,
        "POST",
        &format!("/api/sessions/{sid}/input"),
        Some(json!({ "data": "echo while-detached\r" })),
    );
    wait_until("output while detached", || {
        screen_text(&server, &sid).contains("while-detached")
    });

    // Re-attach (by id prefix): everything is there.
    let mut again = Attached::spawn(&dir, &[&sid[..8]], 80, 24);
    again.wait_text("while-detached");
    again.wait_text("29 100");
    again.type_bytes(b"\x02d");
    assert_eq!(again.exit_code(), 0);
}

#[test]
fn the_picker_lists_sessions_and_switches_between_them() {
    let (_tmp, dir, server, ws) = setup();
    for (name, marker) in [("alpha", "in-alpha"), ("beta", "in-beta")] {
        let (code, session) = http(
            &server,
            "POST",
            "/api/sessions",
            Some(json!({ "workspace_id": ws, "name": name, "command": ["/bin/sh"] })),
        );
        assert_eq!(code, 201, "{session}");
        let sid = session["id"].as_str().unwrap();
        http(
            &server,
            "POST",
            &format!("/api/sessions/{sid}/input"),
            Some(json!({ "data": format!("echo {marker}\r") })),
        );
        wait_until("the marker", || screen_text(&server, sid).contains(marker));
    }
    let mut term = Attached::spawn(&dir, &[], 80, 24);
    term.wait_for("both sessions listed", |l| {
        let all = l.join("\n");
        all.contains("demo") && all.contains("alpha") && all.contains("beta")
    });
    // The first is selected: Enter attaches it.
    term.type_bytes(b"\r");
    term.wait_text("in-alpha");
    // Prefix s: the picker again; j moves to beta.
    term.type_bytes(b"\x02s");
    term.wait_text("Esc back");
    term.type_bytes(b"j\r");
    term.wait_text("in-beta");
    term.wait_for("the bar names beta", |l| l[23].contains("demo/beta"));
    // Esc from the picker returns to the session it came from.
    term.type_bytes(b"\x02s");
    term.wait_text("Esc back");
    term.type_bytes(b"\x1b");
    term.wait_for("back on beta", |l| l[23].contains("demo/beta"));
    // q from the switch picker detaches from the session it came from,
    // leaving none of it (nor the bar) on the shell's screen.
    term.type_bytes(b"\x02s");
    term.wait_text("Esc back");
    term.type_bytes(b"q");
    assert_eq!(term.exit_code(), 0);
    assert!(term.raw_text().contains("[detached from demo/beta]"));
    let lines = term.wait_text("[detached from demo/beta]");
    let all = lines.join("\n");
    assert!(
        !all.contains("in-beta") && !all.contains("blocked"),
        "left on screen:\n{all}"
    );
}

#[test]
fn the_client_ends_with_its_session_shows_the_key_table_and_survives_rotation() {
    let (_tmp, dir, server, ws) = setup();
    let (code, session) = http(
        &server,
        "POST",
        "/api/sessions",
        Some(json!({ "workspace_id": ws, "name": "work", "command": ["/bin/sh"] })),
    );
    assert_eq!(code, 201, "{session}");
    let mut term = Attached::spawn(&dir, &["work"], 60, 24);
    term.wait_for("the status bar", |l| l[23].contains("demo/work"));

    // Prefix ?: the key table, a binding a line (a narrow terminal shows
    // every one); any key returns to the session.
    term.type_bytes(b"echo marker-$((2+3))\r");
    term.wait_text("marker-5");
    term.type_bytes(b"\x02?");
    let lines = term.wait_text("this table");
    let all = lines.join("\n");
    for binding in ["C-b d", "C-b s", "C-b t", "C-b r", "C-b C-b"] {
        assert!(all.contains(binding), "{binding} missing:\n{all}");
    }
    term.type_bytes(b"x");
    term.wait_text("marker-5");
    term.wait_for("the table gone", |l| !l.join("\n").contains("this table"));

    // A rotated token (read from the state dir): the client reconnects.
    let rotated = Command::new(FW)
        .args(["hub", "token", "--rotate", "--state-dir"])
        .arg(dir.join("state"))
        .output()
        .unwrap();
    assert!(rotated.status.success(), "{rotated:?}");
    std::thread::sleep(Duration::from_millis(500));
    term.type_bytes(b"echo after-$((3*3))\r");
    term.wait_text("after-9");
    assert!(term.child.try_wait().unwrap().is_none(), "client ended");

    // The session's process ends: so does the client, its last screen kept.
    term.type_bytes(b"exit 7\r");
    assert_eq!(term.exit_code(), 0);
    let lines = term.wait_text("[demo/work exited 7]");
    let all = lines.join("\n");
    assert!(all.contains("after-9"), "{all}");
    // The goodbye goes at the bottom (the bar's row), not over the session.
    let at = lines
        .iter()
        .position(|l| l.contains("[demo/work exited 7]"))
        .unwrap();
    assert!(at >= 21 && !lines[at].contains("$"), "{all}");
}

#[test]
fn a_slow_terminal_stalls_the_socket_instead_of_queueing_in_the_client() {
    let (_tmp, dir, server, ws) = setup();
    let (code, session) = http(
        &server,
        "POST",
        "/api/sessions",
        Some(json!({ "workspace_id": ws, "name": "flood", "command": ["/bin/sh"] })),
    );
    assert_eq!(code, 201, "{session}");
    let mut term = Attached::spawn(&dir, &["flood"], 80, 24);
    term.wait_for("the status bar", |l| l[23].contains("demo/flood"));
    let before = term.rss_kib();
    // The terminal stops reading while the session floods.
    term.pause(true);
    term.type_bytes(
        b"yes 0123456789012345678901234567890123456789012345678901234567890123456789\r",
    );
    // Unbounded, the client grew by the hub's rate (~10 MiB/s here);
    // flow-controlled, by little more than its credit.
    std::thread::sleep(Duration::from_secs(5));
    let during = term.rss_kib();
    assert!(
        during < before + 16 * 1024,
        "the client grew from {before} KiB to {during} KiB"
    );
    // The terminal catches up: detach is acted on promptly.
    term.pause(false);
    std::thread::sleep(Duration::from_millis(300));
    let asked = Instant::now();
    term.type_bytes(b"\x02d");
    assert_eq!(term.exit_code(), 0);
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "detach took {:?}",
        asked.elapsed()
    );
    let sid = session["id"].as_str().unwrap();
    http(
        &server,
        "POST",
        &format!("/api/sessions/{sid}/input"),
        Some(json!({ "data": "\u{3}" })),
    );
}

fn launched(dir: &Path) -> (Vec<String>, HashMap<String, String>) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let (Ok(args), Ok(env)) = (
            std::fs::read_to_string(dir.join("claude.args")),
            std::fs::read_to_string(dir.join("claude.env")),
        ) {
            let env = env
                .lines()
                .filter_map(|l| l.split_once('='))
                .filter(|(k, _)| k.starts_with("FLYWHEEL_HUB_"))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            return (args.lines().map(str::to_owned).collect(), env);
        }
        assert!(Instant::now() < deadline, "claude never launched");
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Run the `--settings` hook for `event` as Claude Code would.
fn run_hook(settings: &Value, event: &str, env: &HashMap<String, String>, stdin: &str) {
    let group = settings["hooks"][event].as_array().unwrap()[0].clone();
    let hook = &group["hooks"][0];
    let mut child = Command::new(hook["command"].as_str().unwrap())
        .args(
            hook["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap()),
        )
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
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
        "{event}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_cmd(dir: &Path, args: &[&str]) -> std::process::Child {
    Command::new(FW)
        .args(["hub", "wait"])
        .args(args)
        .arg("--state-dir")
        .arg(dir.join("state"))
        .env_remove("FW_HUB_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn finish(child: std::process::Child) -> (Option<i32>, String, String) {
    let output = child.wait_with_output().unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    )
}

#[test]
fn wait_returns_when_the_agent_reports_done_and_times_out_with_124() {
    let (_tmp, dir, server, ws) = setup();
    let (code, session) = http(
        &server,
        "POST",
        "/api/sessions",
        Some(json!({ "workspace_id": ws, "kind": "claude", "name": "agent" })),
    );
    assert_eq!(code, 201, "{session}");
    let (args, env) = launched(&dir);
    let settings: Value = serde_json::from_str(&args[3]).unwrap();
    let payload = json!({ "session_id": args[1] }).to_string();

    // Nothing reported yet: a short wait times out (exit 124).
    let (code, _, stderr) = finish(wait_cmd(
        &dir,
        &["agent", "--until", "done", "--timeout", "0.5"],
    ));
    assert_eq!(code, Some(124), "{stderr}");
    assert!(stderr.contains("timed out"), "{stderr}");

    // A wait started now returns once the turn finishes.
    let waiting = wait_cmd(
        &dir,
        &["agent", "--until", "done", "--changed", "--timeout", "30"],
    );
    std::thread::sleep(Duration::from_millis(500));
    run_hook(&settings, "UserPromptSubmit", &env, &payload);
    run_hook(&settings, "Stop", &env, &payload);
    let (code, stdout, stderr) = finish(waiting);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "done");

    // Already done: a plain wait is met at once; --changed waits for the
    // next turn instead.
    let (code, stdout, _) = finish(wait_cmd(
        &dir,
        &["agent", "--until", "idle", "--timeout", "5"],
    ));
    assert_eq!((code, stdout.as_str()), (Some(0), "done"));
    let (code, _, _) = finish(wait_cmd(
        &dir,
        &["agent", "--until", "done", "--changed", "--timeout", "0.5"],
    ));
    assert_eq!(code, Some(124));

    // --timeout 0 checks once: met now, so 0.
    let (code, stdout, stderr) = finish(wait_cmd(
        &dir,
        &["agent", "--until", "done", "--timeout", "0"],
    ));
    assert_eq!((code, stdout.as_str()), (Some(0), "done"), "{stderr}");
    let (code, _, stderr) = finish(wait_cmd(
        &dir,
        &["agent", "--until", "blocked", "--timeout", "0"],
    ));
    assert_eq!(code, Some(124), "{stderr}");
    assert!(stderr.contains("(now done)"), "{stderr}");

    // --url without a token: the local token goes only to the local hub.
    let url = |port: u16| format!("http://127.0.0.1:{port}");
    let (code, stdout, stderr) = finish(wait_cmd(
        &dir,
        &[
            "agent",
            "--until",
            "done",
            "--timeout",
            "5",
            "--url",
            &url(server.port),
        ],
    ));
    assert_eq!((code, stdout.as_str()), (Some(0), "done"), "{stderr}");
    let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other = elsewhere.local_addr().unwrap().port();
    let (code, _, stderr) = finish(wait_cmd(
        &dir,
        &[
            "agent",
            "--until",
            "done",
            "--timeout",
            "5",
            "--url",
            &url(other),
        ],
    ));
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("--url needs the hub's token"), "{stderr}");
    elsewhere.set_nonblocking(true).unwrap();
    assert!(
        elsewhere.accept().is_err(),
        "the other address was contacted"
    );

    // A shell has no agent status; --until exit works for it.
    let (code, shell) = http(
        &server,
        "POST",
        "/api/sessions",
        Some(json!({ "workspace_id": ws, "name": "sh", "command": ["/bin/sh"] })),
    );
    assert_eq!(code, 201);
    let (code, _, stderr) = finish(wait_cmd(&dir, &["sh", "--until", "done"]));
    assert_eq!(code, Some(1));
    assert!(stderr.contains("not an agent session"), "{stderr}");
    let exiting = wait_cmd(&dir, &["sh", "--until", "exit", "--timeout", "30"]);
    std::thread::sleep(Duration::from_millis(500));
    let sid = shell["id"].as_str().unwrap();
    http(
        &server,
        "POST",
        &format!("/api/sessions/{sid}/input"),
        Some(json!({ "data": "exit 3\r" })),
    );
    let (code, stdout, stderr) = finish(exiting);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "exited 3");

    // Unknown session: exit 1.
    let (code, _, stderr) = finish(wait_cmd(&dir, &["nope", "--until", "exit"]));
    assert_eq!(code, Some(1));
    assert!(stderr.contains("no session matches"), "{stderr}");
}

#[test]
fn a_turn_seen_in_one_attach_stays_seen_in_the_next() {
    let (_tmp, dir, server, ws) = setup();
    let (code, session) = http(
        &server,
        "POST",
        "/api/sessions",
        Some(json!({ "workspace_id": ws, "kind": "claude", "name": "agent" })),
    );
    assert_eq!(code, 201, "{session}");
    let (args, env) = launched(&dir);
    let settings: Value = serde_json::from_str(&args[3]).unwrap();
    let payload = json!({ "session_id": args[1] }).to_string();
    let mut term = Attached::spawn(&dir, &["agent"], 80, 24);
    term.wait_text("fake-claude-ready");
    term.wait_for("the status bar", |l| l[23].contains("demo/agent"));
    std::thread::sleep(Duration::from_millis(300));
    // A turn finishes while this client watches: idle for it.
    run_hook(&settings, "UserPromptSubmit", &env, &payload);
    run_hook(&settings, "Stop", &env, &payload);
    term.wait_for("idle on the bar", |l| {
        l[23].contains("[idle]") && l[23].contains("0 done")
    });
    term.type_bytes(b"\x02d");
    assert_eq!(term.exit_code(), 0);
    // A new attach (the picker) has seen it too.
    let picker = Attached::spawn(&dir, &[], 80, 24);
    let agent_row = |l: &[String]| {
        l.iter()
            .skip(1)
            .find(|r| r.contains("agent"))
            .cloned()
            .unwrap_or_default()
    };
    // Live status arrives on the events socket: wait for it.
    let lines = picker.wait_for("the agent's status", |l| {
        let row = agent_row(l);
        row.contains("idle") || row.contains("done")
    });
    assert!(lines[0].contains("0 done"), "{}", lines[0]);
    let row = agent_row(&lines);
    assert!(row.contains("idle") && !row.contains("done"), "{row}");
}
