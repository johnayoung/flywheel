//! Live handoff end to end through the real binary: a detached hub (A)
//! with shell and agent sessions is replaced by a new hub process (B) via
//! `fw hub upgrade`, and every session keeps its process, screen, and
//! scrollback; and a handoff the new side refuses or abandons leaves A
//! serving exactly as before.

#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = root.join("state");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("repo")).unwrap();
        Self {
            _dir: dir,
            root,
            state,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        // Right after the verb: anything after `--` is a session's argv.
        let mut argv: Vec<&str> = vec!["hub", args[0], "--state-dir", self.state.to_str().unwrap()];
        argv.extend_from_slice(&args[1..]);
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        command
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            // A scripted `claude` first on PATH: never the real one.
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("PS1", "$ ")
            .env_remove("XDG_STATE_HOME")
            .env_remove("INVOCATION_ID");
        command
    }

    fn fw(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.fw(args);
        assert!(
            output.status.success(),
            "fw hub {args:?}: {:?}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn status(&self) -> Value {
        serde_json::from_str(&self.ok(&["status", "--json"])).unwrap()
    }

    fn port(&self) -> u16 {
        let listen = self.status()["listen"].as_str().unwrap().to_owned();
        listen.rsplit(':').next().unwrap().parse().unwrap()
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.state.join("token"))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn sessions(&self) -> Vec<Value> {
        let listed: Value = serde_json::from_str(&self.ok(&["ls", "--json"])).unwrap();
        listed["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|w| w["sessions"].as_array().unwrap().clone())
            .collect()
    }

    fn session(&self, id: &str) -> Value {
        self.sessions()
            .into_iter()
            .find(|s| s["id"] == id)
            .unwrap_or_else(|| panic!("no session {id}"))
    }

    fn read_until(&self, session: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let screen = self.ok(&["read", session]);
            if screen.contains(needle) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "{needle:?} never appeared in {session}:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn state_doc(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.state.join("state.json")).unwrap()).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Never leave a daemon behind, whatever the test did.
        let _ = self.fw(&["stop", "--force", "--timeout", "10"]);
    }
}

/// Write an executable script without this process holding a writable
/// descriptor to it (see `flywheel-hub`'s test support: ETXTBSY).
fn write_script(path: &Path, contents: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// A scripted `claude`: logs its report secret, then echoes its terminal.
fn fake_claude(env: &Env) {
    write_script(
        &env.root.join("bin/claude"),
        &format!(
            "#!/bin/sh\necho \"secret=$FLYWHEEL_HUB_REPORT_SECRET\" > '{}'\necho claude-ready\nexec cat\n",
            env.root.join("claude.secret").display()
        ),
    );
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: u64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat[stat.rfind(')')? + 1..]
                .split_whitespace()
                .next()?
                .to_owned();
            Some(state != "Z" && state != "X")
        })
        .unwrap_or(false)
}

type Ws = tungstenite::WebSocket<TcpStream>;

/// Attach to a session's terminal: the socket and its snapshot bytes.
fn attach(env: &Env, port: u16, session: &str) -> (Ws, Vec<u8>) {
    use tungstenite::client::IntoClientRequest;
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut request = format!("ws://127.0.0.1:{port}/api/sessions/{session}/ws")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", env.token()).parse().unwrap(),
    );
    let (mut socket, _) = tungstenite::client(request, stream).unwrap();
    let header: Value = loop {
        if let tungstenite::Message::Text(text) = socket.read().unwrap() {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["type"] == "snapshot" {
                break value;
            }
        }
    };
    let want = usize::try_from(header["bytes"].as_u64().unwrap()).unwrap();
    let mut snapshot = Vec::new();
    while snapshot.len() < want {
        if let tungstenite::Message::Binary(bytes) = socket.read().unwrap() {
            snapshot.extend_from_slice(&bytes);
        }
    }
    assert_eq!(snapshot.len(), want, "the snapshot is exactly its bytes");
    (socket, snapshot)
}

/// Type into an attached terminal and read live output until `needle`.
fn type_and_see(socket: &mut Ws, input: &str, needle: &str) {
    socket
        .send(tungstenite::Message::Text(
            json!({"type": "input", "data": input}).to_string().into(),
        ))
        .unwrap();
    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !seen.contains(needle) {
        assert!(Instant::now() < deadline, "{needle:?} not in {seen:?}");
        if let tungstenite::Message::Binary(bytes) = socket.read().unwrap() {
            seen.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
}

fn healthz(port: u16) -> bool {
    healthz_within(port, Duration::from_secs(5))
}

fn healthz_within(port: u16, timeout: Duration) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let request =
        format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 200")
}

/// A hub with: a shell running `sleep 1000` (the leader itself), a shell
/// with 400 lines of scrollback, and a scripted Claude Code session.
struct Setup {
    sleeper: String,
    sleeper_pid: u64,
    scroll: String,
    agent: String,
    port: u16,
}

fn setup(env: &Env) -> Setup {
    fake_claude(env);
    // Only the first hub's environment carries the marker: an upgrade
    // passes the old hub's environment on, not the caller's.
    let started = env
        .command(&[
            "start",
            "--port",
            "0",
            "--public-url",
            "https://box.example",
        ])
        .env("HUB_ENV_MARKER", "from-the-first-hub")
        .output()
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    let started = String::from_utf8_lossy(&started.stdout);
    assert!(started.contains("flywheel hub started"), "{started}");
    let repo = env.root.join("repo");
    let repo = repo.to_str().unwrap();
    let sleeper = env
        .ok(&[
            "new",
            "--create",
            "--repo",
            repo,
            "--name",
            "sleeper",
            "ws",
            "--",
            "/bin/sh",
            "-c",
            "echo sleeping; exec sleep 1000",
        ])
        .trim()
        .to_owned();
    let scroll = env.ok(&["new", "--name", "scroll", "ws"]).trim().to_owned();
    let agent = env
        .ok(&["new", "--kind", "claude", "--name", "agent", "ws"])
        .trim()
        .to_owned();
    env.read_until(&sleeper, "sleeping");
    env.read_until(&agent, "claude-ready");
    env.ok(&[
        "send",
        &scroll,
        "--enter",
        "for i in $(seq 1 400); do echo scroll-line-$i; done",
    ]);
    env.read_until(&scroll, "scroll-line-400");
    let sleeper_pid = env.session(&sleeper)["live"]["pid"].as_u64().unwrap();
    assert!(alive(sleeper_pid));
    Setup {
        sleeper,
        sleeper_pid,
        scroll,
        agent,
        port: env.port(),
    }
}

/// Report an agent hook event the way the session's hooks would.
fn report(env: &Env, session: &str, event: &str) -> Output {
    let secret = std::fs::read_to_string(env.root.join("claude.secret")).unwrap();
    let secret = secret.trim().strip_prefix("secret=").unwrap().to_owned();
    let socket = if env.state.join("report.sock").exists() {
        env.state.join("report.sock")
    } else {
        panic!("no report socket in {}", env.state.display());
    };
    Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(["hub", "report", "--source", "claude", "--event", event])
        .env("FLYWHEEL_HUB_SESSION", session)
        .env("FLYWHEEL_HUB_REPORT_SOCKET", socket)
        .env("FLYWHEEL_HUB_REPORT_SECRET", secret)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn agent_status(env: &Env, session: &str) -> String {
    env.session(session)["agent"]["status"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

#[test]
fn upgrade_hands_every_session_to_the_new_process_alive() {
    let env = Env::new();
    let setup = setup(&env);
    let old_pid = env.status()["pid"].as_u64().unwrap();
    assert!(
        report(&env, &setup.agent, "UserPromptSubmit")
            .status
            .success()
    );
    wait_for("working", Duration::from_secs(5), || {
        agent_status(&env, &setup.agent) == "working"
    });
    let (_, scroll_before) = attach(&env, setup.port, &setup.scroll);
    let (_, sleeper_before) = attach(&env, setup.port, &setup.sleeper);
    // A browser attached across the handoff: its socket closes with 1012
    // (service restart), and it reconnects to the new one.
    let (mut open, _) = attach(&env, setup.port, &setup.scroll);
    // A scripted wait across the handoff: it reconnects and still sees
    // the turn finish.
    let waiting = env
        .command(&["wait", &setup.agent, "--until", "done", "--timeout", "60"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let records_before: Vec<(Value, Value)> = env.state_doc()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].clone(), s["state"].clone()))
        .collect();

    let upgraded = env.fw(&["upgrade", "--binary", env!("CARGO_BIN_EXE_fw")]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&upgraded.stdout),
        String::from_utf8_lossy(&upgraded.stderr)
    );
    assert!(upgraded.status.success(), "{said}");
    assert!(said.contains("handed off"), "{said}");

    let new_pid = env.status()["pid"].as_u64().unwrap();
    assert_ne!(new_pid, old_pid, "a new process serves");
    assert!(!alive(old_pid), "the old process exited");
    assert_eq!(env.port(), setup.port, "same port");
    assert!(healthz(setup.port));
    // The old socket was closed for the restart, not dropped.
    let code = loop {
        match open.read() {
            Ok(tungstenite::Message::Close(frame)) => break frame.map(|f| u16::from(f.code)),
            Ok(_) => {}
            Err(e) => panic!("the socket was dropped without a close: {e}"),
        }
    };
    assert_eq!(code, Some(1012));
    let new_env = std::fs::read(format!("/proc/{new_pid}/environ")).unwrap();
    assert!(
        String::from_utf8_lossy(&new_env).contains("HUB_ENV_MARKER=from-the-first-hub"),
        "the new hub runs in the old one's environment"
    );
    assert_eq!(env.status()["public_urls"], json!(["https://box.example"]));
    let status = env.ok(&["status"]);
    assert!(
        status.contains(&format!(
            "version:    flywheel {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{status}"
    );

    // The same processes, still running.
    assert!(alive(setup.sleeper_pid), "sleep survived");
    let sleeper = env.session(&setup.sleeper);
    assert_eq!(sleeper["state"], "running");
    assert_eq!(sleeper["live"]["pid"].as_u64(), Some(setup.sleeper_pid));
    // The same screens, scrollback included.
    let (mut scroll_socket, scroll_after) = attach(&env, setup.port, &setup.scroll);
    assert_eq!(
        String::from_utf8_lossy(&scroll_after),
        String::from_utf8_lossy(&scroll_before),
        "scrollback session snapshot"
    );
    assert!(String::from_utf8_lossy(&scroll_after).contains("scroll-line-1\r\n"));
    let (_, sleeper_after) = attach(&env, setup.port, &setup.sleeper);
    assert_eq!(sleeper_after, sleeper_before, "sleeper snapshot");
    // Typing reaches the same shell over a new attach.
    type_and_see(&mut scroll_socket, "echo typed-$((40+2))\r", "typed-42");
    // The state file still has every session running, nothing demoted.
    let records_after: Vec<(Value, Value)> = env.state_doc()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].clone(), s["state"].clone()))
        .collect();
    assert_eq!(records_after, records_before);
    assert!(records_after.iter().all(|(_, state)| state == "running"));
    // The agent's status came along, and its hooks' secret still works
    // through the handed-over report socket.
    assert_eq!(agent_status(&env, &setup.agent), "working");
    assert!(report(&env, &setup.agent, "Stop").status.success());
    wait_for("done", Duration::from_secs(5), || {
        matches!(agent_status(&env, &setup.agent).as_str(), "done" | "idle")
    });
    let waited = waiting.wait_with_output().unwrap();
    assert!(
        waited.status.success(),
        "fw hub wait across the upgrade: {waited:?}"
    );
    let wrong = Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(["hub", "report", "--source", "claude", "--event", "Stop"])
        .env("FLYWHEEL_HUB_SESSION", &setup.agent)
        .env("FLYWHEEL_HUB_REPORT_SOCKET", env.state.join("report.sock"))
        .env("FLYWHEEL_HUB_REPORT_SECRET", "0".repeat(64))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!wrong.status.success(), "a wrong secret is still refused");

    // Hand off again (B -> C): adopted sessions pass on too.
    env.ok(&["upgrade", "--binary", env!("CARGO_BIN_EXE_fw")]);
    assert_ne!(env.status()["pid"].as_u64().unwrap(), new_pid);
    assert!(alive(setup.sleeper_pid));
    let (mut again, scroll_again) = attach(&env, setup.port, &setup.scroll);
    assert!(String::from_utf8_lossy(&scroll_again).contains("typed-42"));
    type_and_see(&mut again, "echo again-$((1+1))\r", "again-2");

    // A process that is not the hub's child: its exit is still seen, with
    // an unknown code.
    // SAFETY: kill(2) on the test's own sleep process.
    unsafe { libc::kill(i32::try_from(setup.sleeper_pid).unwrap(), libc::SIGTERM) };
    wait_for("the sleeper's exit", Duration::from_secs(10), || {
        env.session(&setup.sleeper)["state"] == "exited"
    });
    let sleeper = env.session(&setup.sleeper);
    assert!(
        sleeper["code"].is_null() && sleeper["signal"].is_null(),
        "{sleeper}"
    );

    // An ended session lets go of its terminal, and still hands over
    // (its screen alone) on the next upgrade.
    let (_, ended_before) = attach(&env, setup.port, &setup.sleeper);
    env.ok(&["upgrade", "--binary", env!("CARGO_BIN_EXE_fw")]);
    let sleeper = env.session(&setup.sleeper);
    assert_eq!(sleeper["state"], "exited", "{sleeper}");
    let (_, ended_after) = attach(&env, setup.port, &setup.sleeper);
    assert_eq!(ended_after, ended_before, "an ended session's screen");
    assert!(String::from_utf8_lossy(&ended_after).contains("sleeping"));

    // A normal stop still records the running sessions for restore.
    let stopped = env.ok(&["stop"]);
    assert!(stopped.contains("recorded for restore"), "{stopped}");
    assert!(!env.state.join("handoff.sock").exists());
}

/// Speak the handoff protocol as a new hub would.
struct FakeSuccessor {
    stream: UnixStream,
}

impl FakeSuccessor {
    fn connect(env: &Env) -> Self {
        let stream = UnixStream::connect(env.state.join("handoff.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        Self { stream }
    }

    fn send(&mut self, msg: &Value) {
        let json = msg.to_string().into_bytes();
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&u32::try_from(json.len()).unwrap().to_be_bytes());
        self.stream.write_all(&header).unwrap();
        self.stream.write_all(&json).unwrap();
    }

    /// One frame's JSON (descriptors passed with it are discarded).
    fn recv(&mut self) -> Value {
        let mut header = [0u8; 8];
        self.stream.read_exact(&mut header).unwrap();
        let json_len = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let bin_len = u32::from_be_bytes(header[4..].try_into().unwrap()) as usize;
        let mut json = vec![0u8; json_len];
        self.stream.read_exact(&mut json).unwrap();
        let mut bin = vec![0u8; bin_len];
        self.stream.read_exact(&mut bin).unwrap();
        serde_json::from_slice(&json).unwrap()
    }

    fn hello(&mut self, protocol: u32) {
        self.send(&json!({
            "type": "hello",
            "protocol": protocol,
            "version": "test",
            "pid": std::process::id(),
        }));
    }

    /// Hello, then the whole offer: its session frames' ids.
    fn take_offer(&mut self) -> Vec<String> {
        self.hello(1);
        let offer = self.recv();
        assert_eq!(offer["type"], "offer", "{offer}");
        (0..offer["sessions"].as_u64().unwrap())
            .map(|_| {
                let session = self.recv();
                assert_eq!(session["type"], "session");
                session["id"].as_str().unwrap().to_owned()
            })
            .collect()
    }
}

fn assert_still_serving(env: &Env, setup: &Setup, old_pid: u64, marker: &str) {
    assert_eq!(env.status()["pid"].as_u64(), Some(old_pid));
    assert!(healthz(setup.port));
    assert!(alive(setup.sleeper_pid));
    let (mut socket, snapshot) = attach(env, setup.port, &setup.scroll);
    assert!(String::from_utf8_lossy(&snapshot).contains("scroll-line-400"));
    type_and_see(
        &mut socket,
        &format!("echo {marker}-$((2+3))\r"),
        &format!("{marker}-5"),
    );
    let doc = env.state_doc();
    assert!(
        doc["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["state"] == "running"),
        "{doc}"
    );
}

#[test]
fn a_refused_or_abandoned_handoff_leaves_the_old_hub_serving() {
    let env = Env::new();
    let setup = setup(&env);
    let old_pid = env.status()["pid"].as_u64().unwrap();

    // Version skew: refused before anything is frozen.
    let mut skewed = FakeSuccessor::connect(&env);
    skewed.hello(999);
    let refused = skewed.recv();
    assert_eq!(refused["type"], "refused");
    assert!(
        refused["reason"].as_str().unwrap().contains("protocol"),
        "{refused}"
    );
    assert_still_serving(&env, &setup, old_pid, "skew");

    // The new hub fails after receiving everything: the old one thaws.
    let mut failing = FakeSuccessor::connect(&env);
    let mut ids = failing.take_offer();
    ids.sort();
    let mut expected = vec![
        setup.sleeper.clone(),
        setup.scroll.clone(),
        setup.agent.clone(),
    ];
    expected.sort();
    assert_eq!(ids, expected, "every live session is offered");
    // Frozen meanwhile: the port stays open, but a new connection waits
    // in the backlog unanswered.
    assert!(!healthz_within(setup.port, Duration::from_millis(800)));
    failing.send(&json!({"type": "failed", "reason": "injected failure"}));
    drop(failing);
    wait_for("the thaw", Duration::from_secs(10), || healthz(setup.port));
    assert_still_serving(&env, &setup, old_pid, "failed");

    // A frozen hub closes its terminal sockets for the restart (1012)
    // rather than taking input it could no longer hand over.
    let (mut socket, _) = attach(&env, setup.port, &setup.scroll);
    let mut vanishing = FakeSuccessor::connect(&env);
    vanishing.take_offer();
    let code = loop {
        match socket.read() {
            Ok(tungstenite::Message::Close(frame)) => break frame.map(|f| u16::from(f.code)),
            Ok(_) => {}
            Err(e) => panic!("the socket was dropped without a close: {e}"),
        }
    };
    assert_eq!(code, Some(1012));
    // Meanwhile a second handoff is refused as busy (never a restart).
    let mut second = FakeSuccessor::connect(&env);
    second.hello(1);
    let refused = second.recv();
    assert_eq!(refused["type"], "refused", "{refused}");
    assert_eq!(refused["kind"], "busy", "{refused}");
    // The new hub dies before answering.
    drop(vanishing);
    wait_for("the thaw", Duration::from_secs(10), || healthz(setup.port));
    assert_still_serving(&env, &setup, old_pid, "vanished");

    // The new hub dies after it said `resuming`: the old one rolls back
    // (without typing held input a second time) and serves on.
    let mut resuming = FakeSuccessor::connect(&env);
    resuming.take_offer();
    resuming.send(&json!({"type": "ready"}));
    assert_eq!(resuming.recv()["type"], "commit");
    resuming.send(&json!({"type": "resuming"}));
    drop(resuming);
    wait_for("the rollback", Duration::from_secs(10), || {
        healthz(setup.port)
    });
    assert_still_serving(&env, &setup, old_pid, "resumed");

    // A new hub that goes silent is given up on after the time it asked
    // for, not the protocol's minute.
    let mut silent = FakeSuccessor::connect(&env);
    silent.send(&json!({
        "type": "hello",
        "protocol": 1,
        "version": "test",
        "pid": std::process::id(),
        "step_secs": 5,
    }));
    let offer = silent.recv();
    for _ in 0..offer["sessions"].as_u64().unwrap() {
        silent.recv();
    }
    let frozen_at = Instant::now();
    wait_for("the rollback", Duration::from_secs(20), || {
        healthz(setup.port)
    });
    assert!(
        frozen_at.elapsed() < Duration::from_secs(15),
        "frozen for {:?}",
        frozen_at.elapsed()
    );
    drop(silent);
    assert_still_serving(&env, &setup, old_pid, "silent");

    // `upgrade` with a flywheel from before live handoff: refused (exit
    // 4), nothing changes.
    let stub = env.root.join("old-fw");
    write_script(
        &stub,
        "#!/bin/sh\nif [ \"$*\" = 'hub start --help' ]; then\n  echo 'Usage: fw hub start [OPTIONS]'\n  echo '      --state-dir <DIR>'\n  exit 0\nfi\necho 'error: unexpected argument' >&2\nexit 2\n",
    );
    let upgraded = env.fw(&["upgrade", "--binary", stub.to_str().unwrap()]);
    assert_eq!(upgraded.status.code(), Some(4), "{upgraded:?}");
    let said = String::from_utf8_lossy(&upgraded.stderr);
    assert!(said.contains("predates live handoff"), "{said}");
    assert!(said.contains("--allow-restart"), "{said}");
    assert_still_serving(&env, &setup, old_pid, "stub");

    // A binary that is no flywheel at all: refused before anything is
    // asked of the hub.
    let upgraded = env.fw(&["upgrade", "--binary", "/bin/false"]);
    assert_eq!(upgraded.status.code(), Some(1), "{upgraded:?}");
    let said = String::from_utf8_lossy(&upgraded.stderr);
    assert!(said.contains("not a flywheel binary"), "{said}");
    assert!(said.contains("carries on"), "{said}");
    assert_still_serving(&env, &setup, old_pid, "false");
}

#[test]
fn a_restart_fallback_keeps_the_old_hubs_port_and_flags() {
    let env = Env::new();
    let setup = setup(&env);
    let old_pid = env.status()["pid"].as_u64().unwrap();
    // A hub that cannot hand off (one from before live handoff answers no
    // handoff socket).
    std::fs::remove_file(env.state.join("handoff.sock")).unwrap();
    let refused = env.fw(&["upgrade", "--binary", env!("CARGO_BIN_EXE_fw")]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert_eq!(env.status()["pid"].as_u64(), Some(old_pid));
    let restarted = env.fw(&[
        "upgrade",
        "--binary",
        env!("CARGO_BIN_EXE_fw"),
        "--allow-restart",
    ]);
    assert!(restarted.status.success(), "{restarted:?}");
    let status = env.status();
    assert_ne!(status["pid"].as_u64(), Some(old_pid));
    assert_eq!(
        env.port(),
        setup.port,
        "same port (it was picked by --port 0)"
    );
    assert_eq!(status["public_urls"], json!(["https://box.example"]));
    // Restored (a shell comes back; its process is new).
    env.read_until(&setup.scroll, "$");
}

#[test]
fn handoff_without_a_running_hub_is_refused_cleanly() {
    let env = Env::new();
    let upgraded = env.fw(&["upgrade"]);
    assert_eq!(upgraded.status.code(), Some(3), "{upgraded:?}");
    let serve = env.fw(&["serve", "--handoff", "--port", "0"]);
    assert_eq!(serve.status.code(), Some(3), "{serve:?}");
    assert!(
        String::from_utf8_lossy(&serve.stderr).contains("cannot hand off"),
        "{serve:?}"
    );
}
