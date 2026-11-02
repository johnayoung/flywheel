//! Suggested commands end to end: a real hub on 127.0.0.1:0 with scripted
//! stand-ins for the agent CLIs in real PTYs. The test writes the agent's
//! transcript the way the agent does (appending JSONL lines), then reads
//! the blocks back over REST, the events WebSocket, and `fw hub
//! commands`; pastes go into a real terminal that records the exact bytes
//! it received.

use flywheel_hub::agent::claude_project_dir_name;
use flywheel_hub::remote::RuntimeInfo;
use flywheel_hub::report::{self, Report};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo/.git")).unwrap();
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_agent(dir.path(), "claude")
            .to_string_lossy()
            .into_owned();
        options.agents.codex = fake_agent(dir.path(), "codex")
            .to_string_lossy()
            .into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        options.codex_home = Some(dir.path().join("codex-home"));
        let hub = Hub::open(options).unwrap();
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // What `fw hub serve` records, so the CLI finds this server.
        RuntimeInfo {
            pid: std::process::id(),
            listen: addr,
            public_urls: Vec::new(),
        }
        .write(&dir.path().join("state"))
        .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            addr,
            token,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo").canonicalize().unwrap()
    }

    fn state_dir(&self) -> String {
        self.dir.path().join("state").to_string_lossy().into_owned()
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .http
            .request(method, format!("http://{}{path}", self.addr))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (status, value) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(status.is_success(), "{path}: {status} {value}");
        value
    }

    async fn get(&self, path: &str) -> Value {
        let (status, value) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        value
    }

    async fn events(&self) -> Ws {
        let request = format!("ws://{}/api/events?client=cmdtest", self.addr)
            .into_client_request()
            .map(|mut r| {
                r.headers_mut().insert(
                    "authorization",
                    format!("Bearer {}", self.token).parse().unwrap(),
                );
                r
            })
            .unwrap();
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
    }
}

/// A stand-in agent CLI: dumps its environment, then echoes its input.
fn fake_agent(dir: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let env = dir.join(format!("{name}.env"));
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nenv > '{e}.tmp' && mv '{e}.tmp' '{e}'\nexec cat\n",
            e = env.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn agent_env(hub: &TestHub, name: &str) -> HashMap<String, String> {
    let path = hub.dir.path().join(format!("{name}.env"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(&path)
            && !text.is_empty()
        {
            return text
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
        }
        assert!(Instant::now() < deadline, "{name} never launched");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn next_commands(ws: &mut Ws) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, ws.next())
            .await
            .expect("timed out waiting for a commands event")
        {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "commands" {
                    return value;
                }
            }
            Some(Ok(_)) => {}
            other => panic!("events socket ended: {other:?}"),
        }
    }
}

fn append(path: &Path, line: &Value) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

fn cli(args: Vec<String>) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = flywheel_hub::cli::run(&args, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

const HEREDOC: &str = "cat <<'EOF' > /tmp/fw-notes.txt\n  keep   these   spaces\n\tand a tab\nEOF\ngrep -v '^#' /etc/hosts | awk '{print $2}' | sort -u | head -n 20";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_transcript_blocks_reach_rest_events_and_the_cli_exactly() {
    let hub = TestHub::start().await;
    let ws = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "repos": [hub.repo()] }),
        )
        .await;
    let wid = ws["id"].as_str().unwrap();
    let claude = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "kind": "claude", "name": "agent" }),
        )
        .await;
    let sid = claude["id"].as_str().unwrap().to_owned();
    let conversation = claude["agent_session_id"].as_str().unwrap().to_owned();
    let mut events = hub.events().await;

    let empty = hub.get(&format!("/api/sessions/{sid}/commands")).await;
    assert_eq!(empty["commands"], json!([]));

    // Claude Code writes its transcript where the hub expects it.
    let project = hub
        .dir
        .path()
        .join("claude-config/projects")
        .join(claude_project_dir_name(&hub.repo()));
    std::fs::create_dir_all(&project).unwrap();
    let transcript = project.join(format!("{conversation}.jsonl"));
    let now = chrono::Utc::now().to_rfc3339();
    append(
        &transcript,
        &json!({"type": "user", "uuid": "u0", "message": {"role": "user", "content": "suggest ```a``` command"}}),
    );
    append(
        &transcript,
        &json!({
            "type": "assistant", "uuid": "a1", "timestamp": now, "sessionId": conversation,
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "```sh\nnot this\n```"},
                {"type": "text", "text": format!("Here:\n\n```bash\n{HEREDOC}\n```\n\nConfig:\n```toml\nx = 1\n```\n\n```console\n$ cargo test\n$ cargo build --release\n```")}
            ]},
        }),
    );
    // The running session's transcript is picked up within a poll.
    let event = next_commands(&mut events).await;
    assert_eq!(event["session_id"], sid.as_str());
    assert_eq!(event["fresh"], true);
    let pushed: Vec<&str> = event["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        pushed,
        ["$ cargo test\n$ cargo build --release", "x = 1", HEREDOC],
        "newest first, all blocks"
    );

    let list = hub.get(&format!("/api/sessions/{sid}/commands")).await;
    let commands = list["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 2, "shell blocks only by default: {list}");
    assert_eq!(commands[0]["command"], "cargo test\ncargo build --release");
    assert_eq!(commands[0]["lang"], "console");
    assert_eq!(commands[1]["text"], HEREDOC);
    assert_eq!(commands[1]["command"], HEREDOC);
    assert_eq!(commands[1]["message_id"], "a1");
    assert_eq!(commands[1]["block_index"], 0);
    assert_eq!(commands[1]["conversation"], conversation.as_str());
    assert_eq!(commands[1]["source"], "transcript");
    assert_eq!(list["transcript"], transcript.to_string_lossy().as_ref());
    let all = hub
        .get(&format!("/api/sessions/{sid}/commands?all=true&limit=2"))
        .await;
    let langs: Vec<&str> = all["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["lang"].as_str().unwrap())
        .collect();
    assert_eq!(langs, ["console", "toml"]);
    for bad in ["limit=0", "limit=501", "limit=x", "all=maybe"] {
        let (status, body) = hub
            .call(
                reqwest::Method::GET,
                &format!("/api/sessions/{sid}/commands?{bad}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert!(body["error"].is_string());
    }
    let (status, _) = hub
        .call(reqwest::Method::GET, "/api/sessions/nope/commands", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // `/clear`: the hooks name the new conversation and its transcript.
    let env = agent_env(&hub, "claude").await;
    let cleared = "11111111-2222-4333-8444-555555555555";
    let new_transcript = project.join(format!("{cleared}.jsonl"));
    append(
        &new_transcript,
        &json!({"type": "assistant", "uuid": "b1", "timestamp": chrono::Utc::now().to_rfc3339(),
            "message": {"role": "assistant", "content": [{"type": "text", "text": "```sh\necho after clear\n```"}]}}),
    );
    report::send(
        Path::new(&env[report::SOCKET_ENV]),
        &Report {
            session: sid.clone(),
            secret: env[report::SECRET_ENV].clone(),
            source: "claude".into(),
            event: "SessionStart".into(),
            detail: None,
            payload: Some(json!({"session_id": cleared, "transcript_path": new_transcript})),
        },
    )
    .unwrap();
    let event = next_commands(&mut events).await;
    assert_eq!(event["commands"][0]["text"], "echo after clear");
    assert_eq!(event["commands"][0]["conversation"], cleared);

    // The CLI prints the newest exactly; and the older one by --json.
    let state = hub.state_dir();
    let args = |extra: &[&str]| {
        let mut v = vec![
            "commands".to_owned(),
            "agent".to_owned(),
            "--state-dir".to_owned(),
            state.clone(),
        ];
        v.extend(extra.iter().map(|s| (*s).to_owned()));
        v
    };
    let (code, out, err) = tokio::task::spawn_blocking({
        let a = args(&["--last"]);
        move || cli(a)
    })
    .await
    .unwrap();
    assert_eq!((code, out.as_str()), (0, "echo after clear\n"), "{err}");
    let (code, out, _) = tokio::task::spawn_blocking({
        let a = args(&["--json"]);
        move || cli(a)
    })
    .await
    .unwrap();
    assert_eq!(code, 0);
    let listed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(listed[2]["text"], HEREDOC);
    let (code, out, _) = tokio::task::spawn_blocking({
        let a = args(&[]);
        move || cli(a)
    })
    .await
    .unwrap();
    assert_eq!(code, 0);
    assert!(out.contains("# 0 bash 20"), "{out}");
    assert!(out.contains(HEREDOC), "{out}");
    let (code, _, err) = tokio::task::spawn_blocking({
        let mut a = args(&["--last"]);
        a[1] = "nobody".into();
        move || cli(a)
    })
    .await
    .unwrap();
    assert_eq!(code, 1);
    assert!(err.contains("no session"), "{err}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_notify_replies_are_mined_when_no_rollout_exists() {
    let hub = TestHub::start().await;
    let ws = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "repos": [hub.repo()] }),
        )
        .await;
    let codex = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws["id"], "kind": "codex" }),
        )
        .await;
    let sid = codex["id"].as_str().unwrap().to_owned();
    let env = agent_env(&hub, "codex").await;
    let mut events = hub.events().await;
    report::send(
        Path::new(&env[report::SOCKET_ENV]),
        &Report {
            session: sid.clone(),
            secret: env[report::SECRET_ENV].clone(),
            source: "codex".into(),
            event: "notify".into(),
            detail: None,
            payload: Some(json!({
                "type": "agent-turn-complete",
                "thread-id": "01a07c85-9201-7123-8801-041350ceed6a",
                "turn-id": "t1",
                "last-assistant-message": format!("Run:\n```bash\n{HEREDOC}\n```"),
            })),
        },
    )
    .unwrap();
    let event = next_commands(&mut events).await;
    assert_eq!(event["commands"][0]["text"], HEREDOC);
    assert_eq!(event["commands"][0]["source"], "notify");
    assert_eq!(event["commands"][0]["message_id"], "t1");
    let list = hub.get(&format!("/api/sessions/{sid}/commands")).await;
    assert_eq!(list["commands"][0]["command"], HEREDOC);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_is_bracketed_never_runs_unasked_and_refuses_unsafe_multiline() {
    let hub = TestHub::start().await;
    let ws = hub
        .post(
            "/api/workspaces",
            json!({ "name": "w", "repos": [hub.repo()] }),
        )
        .await;
    let wid = ws["id"].as_str().unwrap();
    let got = hub.dir.path().join("got.bin");
    // A program that enables bracketed paste and records its raw input.
    let script = format!(
        "stty raw -echo; printf '\\033[?2004h'; exec cat > '{}'",
        got.display()
    );
    let bracketed = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "command": ["/bin/sh", "-c", script] }),
        )
        .await;
    let bid = bracketed["id"].as_str().unwrap().to_owned();
    // Wait for the mode to be set.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = hub.get(&format!("/api/sessions/{bid}/screen")).await;
        if screen["modes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "bracketed_paste")
        {
            break;
        }
        assert!(Instant::now() < deadline, "bracketed paste never enabled");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let outcome = hub
        .post(
            &format!("/api/sessions/{bid}/paste"),
            json!({ "text": format!("{HEREDOC}\n") }),
        )
        .await;
    assert_eq!(outcome["bracketed"], true);
    assert_eq!(outcome["ran"], false);
    let outcome = hub
        .post(
            &format!("/api/sessions/{bid}/paste"),
            json!({ "text": "ls\x1b[201~; rm -rf x", "run": true }),
        )
        .await;
    assert_eq!(outcome["ran"], true);
    let want = format!(
        "\x1b[200~{}\x1b[201~\x1b[200~ls; rm -rf x\x1b[201~\r",
        HEREDOC.replace('\n', "\r")
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let bytes = std::fs::read(&got).unwrap_or_default();
        if bytes.len() >= want.len() {
            assert_eq!(String::from_utf8_lossy(&bytes), want);
            break;
        }
        assert!(Instant::now() < deadline, "paste never arrived: {bytes:?}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // A plain `cat` (no bracketed paste): one line is typed, not run;
    // several lines are refused unless the user asked to run them.
    let plain = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "command": ["/bin/sh", "-c", "exec cat"] }),
        )
        .await;
    let pid = plain["id"].as_str().unwrap();
    let outcome = hub
        .post(
            &format!("/api/sessions/{pid}/paste"),
            json!({ "text": "echo one\n" }),
        )
        .await;
    assert_eq!(outcome["bracketed"], false);
    let (status, body) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{pid}/paste"),
            Some(json!({ "text": "a\nb" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("bracketed paste"),
        "{body}"
    );
    hub.post(
        &format!("/api/sessions/{pid}/paste"),
        json!({ "text": "a\nb", "run": true }),
    )
    .await;
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{pid}/paste"),
            Some(json!({ "text": "  " })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // An ended session takes no paste.
    hub.post(&format!("/api/sessions/{pid}/kill"), json!({}))
        .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, _) = hub
            .call(
                reqwest::Method::POST,
                &format!("/api/sessions/{pid}/paste"),
                Some(json!({ "text": "x" })),
            )
            .await;
        if status == StatusCode::CONFLICT {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "still pasting into a killed session"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    hub.shutdown().await;
}
