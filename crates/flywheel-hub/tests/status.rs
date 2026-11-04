//! Agent status end to end: a real hub on 127.0.0.1:0, real PTYs running
//! scripted stand-ins for the agent CLIs, reports over the real report
//! socket, and the hub-wide events WebSocket.
//!
//! The stand-ins record the argv the hub launched them with; the test then
//! plays the agent's part exactly as the real CLI would: it runs the hook
//! the hub's `--settings` JSON registered for an event (Claude Code), or
//! the `-c notify=[...]` program (Codex) -- here by sending the report
//! those `fw hub report` invocations send.

mod support;

use flywheel_hub::report::{self, Report, ReportError};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
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

/// Stand-in for `fw hub`: hooks are parsed, not run, so it need not exist.
const REPORT_PROGRAM: &str = "/opt/flywheel/bin/fw";

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    /// `name=value` of a browser login made at start.
    cookie: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    /// A hub whose `claude` and `codex` are the given script bodies; each
    /// first dumps its argv and environment into `<dir>/<name>.args|env`.
    async fn start(claude: &str, codex: &str) -> Self {
        flywheel_hub::install_crypto_provider();
        Self::start_in(tempfile::tempdir().unwrap(), claude, codex).await
    }

    /// [`TestHub::start`] on an existing dir (state and repo kept).
    async fn start_in(dir: tempfile::TempDir, claude: &str, codex: &str) -> Self {
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let claude = fake_agent(dir.path(), "claude", claude);
        let codex = fake_agent(dir.path(), "codex", codex);
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = claude.to_string_lossy().into_owned();
        options.agents.codex = codex.to_string_lossy().into_owned();
        options.report_command = Some(vec![REPORT_PROGRAM.into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        let hub = support::open_hub(options);
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
            let _ = stopped.await;
        }));
        let http = reqwest::Client::new();
        let login = http
            .get(format!("http://{addr}/login?token={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        Self {
            dir,
            addr,
            token,
            cookie,
            http,
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo").canonicalize().unwrap()
    }

    fn authed(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("http://{}{path}", self.addr))
            .bearer_auth(&self.token)
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let response = self
            .authed(reqwest::Method::POST, path)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.json().await.unwrap_or(Value::Null)
    }

    async fn get(&self, path: &str) -> Value {
        self.authed(reqwest::Method::GET, path)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn ws(&self, path: &str) -> Result<Ws, u16> {
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("cookie", self.cookie.parse().unwrap());
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                Err(response.status().as_u16())
            }
            Err(e) => panic!("ws error: {e}"),
        }
    }

    async fn events(&self, client: &str) -> Ws {
        self.ws(&format!("/api/events?client={client}"))
            .await
            .unwrap()
    }

    /// Stop the server; returns its dir (state and repo).
    async fn shutdown(mut self) -> tempfile::TempDir {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        self.dir
    }
}

fn fake_agent(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let dump = dir.join(name);
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nenv > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\n\
             printf '%s\\n' \"$@\" > '{d}.args.tmp' && mv '{d}.args.tmp' '{d}.args'\n{body}\n",
            d = dump.display()
        ),
    );
    path
}

/// What the stand-in for `name` was launched with: (argv after the
/// program, environment).
async fn launched(hub: &TestHub, name: &str) -> (Vec<String>, HashMap<String, String>) {
    let args = hub.dir.path().join(format!("{name}.args"));
    let env = hub.dir.path().join(format!("{name}.env"));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let (Ok(a), Ok(e)) = (
            std::fs::read_to_string(&args),
            std::fs::read_to_string(&env),
        ) && !e.is_empty()
        {
            let _ = std::fs::remove_file(&args);
            let env = e
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            return (a.lines().map(str::to_owned).collect(), env);
        }
        assert!(Instant::now() < deadline, "{name} never launched");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Next text message of `kind` on an events socket.
async fn next_event(ws: &mut Ws, kind: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {kind}"))
        {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == kind {
                    return value;
                }
            }
            Some(Ok(_)) => {}
            other => panic!("events socket ended: {other:?}"),
        }
    }
}

/// Wait until `session`'s status on this events socket is `want`.
async fn status_becomes(ws: &mut Ws, session: &str, want: &str) -> Value {
    loop {
        let event = next_event(ws, "status").await;
        if event["session_id"] == session && event["status"]["status"] == want {
            return event;
        }
    }
}

/// Wait until session `sid`'s live screen reads as manifest rule `rule`
/// (the server's emulator has drawn it), then give the screen monitor its
/// look: it reads the screen at most every 250 ms after output.
async fn screen_reads(hub: &TestHub, sid: &str, rule: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let why = hub.get(&format!("/api/sessions/{sid}/explain")).await;
        if why["screen"]["rule"] == rule {
            break;
        }
        assert!(Instant::now() < deadline, "screen never read {rule}: {why}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    tokio::time::sleep(MONITOR_LOOK).await;
}

/// Wait until session `sid`'s visible screen shows `text`.
async fn screen_shows(hub: &TestHub, sid: &str, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let screen = hub
            .get(&format!("/api/sessions/{sid}/screen?source=visible"))
            .await;
        if screen["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().is_some_and(|l| l.contains(text)))
        {
            return;
        }
        assert!(Instant::now() < deadline, "{text:?} never drawn: {screen}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Comfortably more than the screen monitor's look interval (250 ms) plus
/// its poll (100 ms).
const MONITOR_LOOK: Duration = Duration::from_millis(600);

async fn view(ws: &mut Ws, session: Option<&str>) {
    ws.send(Message::Text(
        json!({ "type": "view", "session_id": session })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

/// A launched Claude Code stand-in, driven the way the real CLI drives its
/// hooks.
struct Claude {
    settings: Value,
    env: HashMap<String, String>,
}

impl Claude {
    /// Run the hook registered for `event` (and notification type
    /// `matcher`): check it is `fw hub report ... --stdin`, then send the
    /// report that command sends.
    fn fire(&self, event: &str, matcher: Option<&str>, payload: Value) -> Result<(), ReportError> {
        let groups = self.settings["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("no {event} hooks in {}", self.settings));
        let group = groups
            .iter()
            .find(|g| g.get("matcher").and_then(Value::as_str) == matcher)
            .unwrap_or_else(|| panic!("no {event} hook for {matcher:?}"));
        let hook = &group["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["command"], REPORT_PROGRAM);
        assert!(hook["timeout"].as_u64().unwrap() <= 30);
        let args: Vec<&str> = hook["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let mut expected = vec!["hub", "report", "--source", "claude", "--event", event];
        if let Some(m) = matcher {
            expected.extend(["--detail", m]);
        }
        expected.push("--stdin");
        assert_eq!(args, expected);
        send(&self.env, "claude", event, matcher, Some(payload))
    }
}

fn send(
    env: &HashMap<String, String>,
    source: &str,
    event: &str,
    detail: Option<&str>,
    payload: Option<Value>,
) -> Result<(), ReportError> {
    report::send(
        Path::new(&env[report::SOCKET_ENV]),
        &Report {
            session: env[report::SESSION_ENV].clone(),
            secret: env[report::SECRET_ENV].clone(),
            source: source.into(),
            event: event.into(),
            detail: detail.map(str::to_owned),
            payload,
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_hooks_drive_status_with_per_client_seen_tracking() {
    let hub = TestHub::start("exec cat", "exec cat").await;
    let workspace = hub
        .post(
            "/api/workspaces",
            json!({ "name": "agents", "repos": [hub.repo()] }),
        )
        .await;
    let wid = workspace["id"].as_str().unwrap().to_owned();
    let mut laptop = hub.events("laptop").await;
    let snapshot = next_event(&mut laptop, "snapshot").await;
    assert_eq!(snapshot["sessions"], json!({}));

    let session = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "kind": "claude", "cols": 80, "rows": 24 }),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    next_event(&mut laptop, "sessions").await;
    let conversation = session["agent_session_id"].as_str().unwrap().to_owned();
    assert_eq!(session["agent"]["status"], "unknown");

    // Launched with the hub's conversation id and its hooks; the secret is
    // in the environment only, and the hub's token is nowhere.
    let (args, env) = launched(&hub, "claude").await;
    assert_eq!(args[..2], ["--session-id".to_owned(), conversation.clone()]);
    assert_eq!(args[2], "--settings");
    assert_eq!(args.len(), 4, "{args:?}");
    let settings: Value = serde_json::from_str(&args[3]).unwrap();
    assert_eq!(env[report::SESSION_ENV], sid);
    let secret = env[report::SECRET_ENV].clone();
    assert_eq!(secret.len(), 64);
    assert!(!args.iter().any(|a| a.contains(&secret)));
    assert!(!args.iter().any(|a| a.contains(&hub.token)));
    assert!(!env.values().any(|v| v.contains(&hub.token)));
    let socket = PathBuf::from(&env[report::SOCKET_ENV]);
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let claude = Claude { settings, env };
    let payload = |extra: Value| {
        let mut p = json!({ "session_id": conversation, "cwd": hub.repo() });
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        p
    };

    claude
        .fire("SessionStart", None, payload(json!({})))
        .unwrap();
    let event = status_becomes(&mut laptop, &sid, "idle").await;
    assert_eq!(event["workspace_id"], wid);
    assert_eq!(event["workspace_status"], "idle");
    claude
        .fire("UserPromptSubmit", None, payload(json!({ "prompt": "go" })))
        .unwrap();
    status_becomes(&mut laptop, &sid, "working").await;
    claude
        .fire("PreToolUse", None, payload(json!({ "tool_name": "Bash" })))
        .unwrap();
    claude
        .fire(
            "Notification",
            Some("permission_prompt"),
            payload(json!({ "message": "Claude needs your permission to use Bash" })),
        )
        .unwrap();
    let blocked = status_becomes(&mut laptop, &sid, "blocked").await;
    assert_eq!(
        blocked["status"]["detail"],
        "Claude needs your permission to use Bash"
    );
    assert_eq!(blocked["workspace_status"], "blocked");
    // idle_prompt while blocked changes nothing; approval resumes work.
    claude
        .fire("Notification", Some("idle_prompt"), payload(json!({})))
        .unwrap();
    claude
        .fire("PostToolUse", None, payload(json!({ "tool_name": "Bash" })))
        .unwrap();
    let working = status_becomes(&mut laptop, &sid, "working").await;
    assert!(working["status"].get("detail").is_none(), "{working}");
    claude.fire("Stop", None, payload(json!({}))).unwrap();
    let done = status_becomes(&mut laptop, &sid, "done").await;
    assert_eq!(done["workspace_status"], "done");
    assert_eq!(
        hub.get(&format!("/api/sessions/{sid}")).await["agent"]["status"],
        "done"
    );

    // The laptop views it: idle for the laptop, still done for the phone.
    view(&mut laptop, Some(&sid)).await;
    let idle = status_becomes(&mut laptop, &sid, "idle").await;
    assert_eq!(idle["workspace_status"], "idle");
    let mut phone = hub.events("phone").await;
    let snapshot = next_event(&mut phone, "snapshot").await;
    assert_eq!(snapshot["sessions"][&sid]["status"], "done");
    assert_eq!(snapshot["workspaces"][&wid], "done");
    // The API counts a turn seen once anyone saw it.
    assert_eq!(
        hub.get(&format!("/api/sessions/{sid}")).await["agent"]["status"],
        "idle"
    );

    // A turn that finishes while the laptop watches is never "done" for it.
    claude
        .fire("UserPromptSubmit", None, payload(json!({})))
        .unwrap();
    status_becomes(&mut laptop, &sid, "working").await;
    claude.fire("Stop", None, payload(json!({}))).unwrap();
    status_becomes(&mut laptop, &sid, "idle").await;
    status_becomes(&mut phone, &sid, "done").await;

    // `/clear` moves Claude to a new conversation id: the record follows.
    let cleared = "0b9f4a52-7f6e-4f3e-9d55-5c1c2b8e9a10";
    claude
        .fire("SessionStart", None, json!({ "session_id": cleared }))
        .unwrap();
    next_event(&mut laptop, "sessions").await;
    assert_eq!(
        hub.get(&format!("/api/sessions/{sid}")).await["agent_session_id"],
        cleared
    );

    // Forged reports are refused: wrong secret, wrong source, unknown id.
    let mut forged = claude.env.clone();
    forged.insert(report::SECRET_ENV.into(), "0".repeat(64));
    assert!(matches!(
        send(&forged, "claude", "Stop", None, None),
        Err(ReportError::Refused(m)) if m.contains("secret")
    ));
    assert!(matches!(
        send(&claude.env, "codex", "notify", None, None),
        Err(ReportError::Refused(m)) if m.contains("not codex")
    ));
    let mut unknown = claude.env.clone();
    unknown.insert(report::SESSION_ENV.into(), "nope".into());
    assert!(matches!(
        send(&unknown, "claude", "Stop", None, None),
        Err(ReportError::Refused(_))
    ));
    // Unknown events are accepted and ignored.
    send(&claude.env, "claude", "FileChanged", None, None).unwrap();

    // The process ending makes the status unknown.
    let killed = hub
        .authed(reqwest::Method::POST, &format!("/api/sessions/{sid}/kill"))
        .send()
        .await
        .unwrap();
    assert_eq!(killed.status(), StatusCode::ACCEPTED);
    status_becomes(&mut laptop, &sid, "unknown").await;

    // A relaunch rotates the secret (the old one is refused) and resumes
    // the conversation once Claude has a transcript for it.
    let transcript_dir = hub
        .dir
        .path()
        .join("claude-config/projects")
        .join(flywheel_hub::agent::claude_project_dir_name(&hub.repo()));
    std::fs::create_dir_all(&transcript_dir).unwrap();
    std::fs::write(transcript_dir.join(format!("{cleared}.jsonl")), "{}\n").unwrap();
    wait_exited(&hub, &sid).await;
    hub.post(&format!("/api/sessions/{sid}/restart"), json!({}))
        .await;
    let (args, env) = launched(&hub, "claude").await;
    assert_eq!(args[..2], ["--resume".to_owned(), cleared.to_owned()]);
    assert_ne!(env[report::SECRET_ENV], secret);
    assert!(matches!(
        send(&claude.env, "claude", "Stop", None, None),
        Err(ReportError::Refused(_))
    ));
    let relaunched = Claude {
        settings: serde_json::from_str(&args[3]).unwrap(),
        env,
    };
    relaunched
        .fire("UserPromptSubmit", None, json!({}))
        .unwrap();
    status_becomes(&mut laptop, &sid, "working").await;

    // Removing the session drops its status.
    let removed = hub
        .authed(reqwest::Method::DELETE, &format!("/api/sessions/{sid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    next_event(&mut laptop, "sessions").await;
    let mut late = hub.events("late").await;
    assert_eq!(
        next_event(&mut late, "snapshot").await["sessions"],
        json!({})
    );
    hub.shutdown().await;
}

/// A Claude Code stand-in that draws what the real TUI (2.1.306) draws:
/// the folder-trust dialog first, then per typed line a running turn's
/// footer (`work`), a permission dialog (`dialog`), or the bare prompt it
/// returns to after an Esc interrupt (`stop`).
const CLAUDE_TUI: &str = r#"printf ' Accessing workspace:\r\n\r\n Quick safety check: Is this a project you created or one you trust?\r\n\r\n \342\235\257 No, exit\r\n   Yes, I trust this folder\r\n\r\n Enter to confirm \302\267 Esc to cancel\r\n'
while read line; do
  case "$line" in
    work) printf '\033[2J\033[H> fix it\r\n\342\235\257\r\n  manual mode on \302\267 esc to interrupt\r\n' ;;
    dialog) printf '\033[2J\033[H Do you want to proceed?\r\n \342\235\257 1. Yes\r\n   2. No\r\n Esc to cancel \302\267 Tab to amend\r\n' ;;
    stop) printf '\033[2J\033[H  Interrupted\r\n\342\235\257 \r\n' ;;
  esac
done"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_interrupts_and_the_trust_dialog_read_from_the_screen() {
    let hub = TestHub::start(CLAUDE_TUI, "exec cat").await;
    let workspace = hub
        .post(
            "/api/workspaces",
            json!({ "name": "agents", "repos": [hub.repo()] }),
        )
        .await;
    let wid = workspace["id"].as_str().unwrap().to_owned();
    let mut events = hub.events("laptop").await;
    next_event(&mut events, "snapshot").await;
    let session = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "kind": "claude", "cols": 80, "rows": 24 }),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    let (args, env) = launched(&hub, "claude").await;
    let claude = Claude {
        settings: serde_json::from_str(&args[3]).unwrap(),
        env,
    };
    // The trust dialog waits on a person before any hook ran.
    let blocked = status_becomes(&mut events, &sid, "blocked").await;
    assert_eq!(
        blocked["status"]["detail"],
        "asks whether to trust this folder"
    );
    let why = hub.get(&format!("/api/sessions/{sid}/explain")).await;
    assert_eq!(why["source_label"], "manifest:claude.toml#folder_trust");
    assert_eq!(why["screen"]["rule"], "folder_trust");
    // The screen monitor is not a viewer.
    assert_eq!(
        hub.get(&format!("/api/sessions/{sid}")).await["live"]["clients"],
        0
    );
    claude.fire("SessionStart", None, json!({})).unwrap();
    status_becomes(&mut events, &sid, "idle").await;

    let mut term = hub.ws(&format!("/api/sessions/{sid}/ws")).await.unwrap();
    let mut type_line = async |text: &str| {
        term.send(Message::Text(
            json!({ "type": "input", "data": text }).to_string().into(),
        ))
        .await
        .unwrap();
    };
    let api_status = async |hub: &TestHub| {
        hub.get(&format!("/api/sessions/{sid}")).await["agent"]["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    // Esc mid-turn fires no hook: once the turn's footer is gone for a
    // while, the session is idle again.
    claude.fire("UserPromptSubmit", None, json!({})).unwrap();
    status_becomes(&mut events, &sid, "working").await;
    type_line("work\r").await;
    screen_reads(&hub, &sid, "turn_footer").await;
    let interrupted = Instant::now();
    type_line("stop\r").await;
    status_becomes(&mut events, &sid, "idle").await;
    assert!(
        interrupted.elapsed() >= Duration::from_secs(2),
        "{:?}",
        interrupted.elapsed()
    );
    // A late idle_prompt for the interrupted turn is not a question.
    claude
        .fire("Notification", Some("idle_prompt"), json!({}))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(api_status(&hub).await, "idle");

    // A permission prompt cancelled with Esc -- drawn before its hook
    // arrived, so nothing on screen changes when it does.
    type_line("dialog\r").await;
    screen_reads(&hub, &sid, "permission_prompt").await;
    claude
        .fire("PermissionRequest", None, json!({ "tool_name": "Bash" }))
        .unwrap();
    status_becomes(&mut events, &sid, "blocked").await;
    tokio::time::sleep(MONITOR_LOOK).await;
    type_line("stop\r").await;
    let idle = status_becomes(&mut events, &sid, "idle").await;
    assert!(idle["status"].get("detail").is_none(), "{idle}");

    // An approved permission prompt closes onto the busy footer: working
    // again before the tool's PostToolUse.
    claude
        .fire("PermissionRequest", None, json!({ "tool_name": "Bash" }))
        .unwrap();
    status_becomes(&mut events, &sid, "blocked").await;
    type_line("dialog\r").await;
    screen_reads(&hub, &sid, "permission_prompt").await;
    type_line("work\r").await;
    status_becomes(&mut events, &sid, "working").await;
    type_line("stop\r").await;
    status_becomes(&mut events, &sid, "idle").await;

    // A turn whose footer never showed (cut off, say) is left alone.
    claude.fire("UserPromptSubmit", None, json!({})).unwrap();
    status_becomes(&mut events, &sid, "working").await;
    type_line("stop\r").await;
    screen_reads(&hub, &sid, "prompt").await;
    tokio::time::sleep(flywheel_hub::detect::tracker::SETTLE_AFTER + Duration::from_secs(1)).await;
    assert_eq!(api_status(&hub).await, "working");
    drop(term);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relaunching_a_claude_record_without_a_conversation_id_assigns_one() {
    let hub = TestHub::start("exec cat", "exec cat").await;
    let workspace = hub
        .post(
            "/api/workspaces",
            json!({ "name": "agents", "repos": [hub.repo()] }),
        )
        .await;
    let wid = workspace["id"].as_str().unwrap().to_owned();
    let session = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "kind": "claude" }),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    launched(&hub, "claude").await;
    let dir = hub.shutdown().await;

    // Rewrite the record the way a hub from before conversation ids left it.
    let state_file = dir.path().join("state/state.json");
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
    let record = state["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["id"] == sid)
        .unwrap();
    record.as_object_mut().unwrap().remove("agent_session_id");
    std::fs::write(&state_file, serde_json::to_vec(&state).unwrap()).unwrap();

    let hub = TestHub::start_in(dir, "exec cat", "exec cat").await;
    let stopped = hub.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(stopped["state"], "stopped");
    assert!(stopped.get("agent_session_id").is_none(), "{stopped}");
    let relaunched = hub
        .post(&format!("/api/sessions/{sid}/restart"), json!({}))
        .await;
    let conversation = relaunched["agent_session_id"].as_str().unwrap().to_owned();
    let (args, env) = launched(&hub, "claude").await;
    assert_eq!(
        args[..3],
        [
            "--session-id".to_owned(),
            conversation,
            "--settings".to_owned()
        ]
    );
    let claude = Claude {
        settings: serde_json::from_str(&args[3]).unwrap(),
        env,
    };
    let mut events = hub.events("laptop").await;
    claude.fire("UserPromptSubmit", None, json!({})).unwrap();
    status_becomes(&mut events, &sid, "working").await;
    hub.shutdown().await;
}

async fn wait_exited(hub: &TestHub, sid: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while hub.get(&format!("/api/sessions/{sid}")).await["state"] != "exited" {
        assert!(Instant::now() < deadline, "session never exited");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A Codex stand-in that draws what the codex TUI draws (its strings from
/// openai/codex `codex-rs/tui`): the composer at start; per typed line a
/// turn (`work`: the styled, cursor-moved status line redrawn with its
/// timer, then the composer again), a status line that freezes (`stall`),
/// an exec approval (`approve`), and the running turn an answer (`y`)
/// resumes.
const CODEX_TUI: &str = r#"composer() {
  printf '\033[2J\033[H\342\200\272 Ask Codex to do anything\r\n\r\n  ? for shortcuts      100%% context left\r\n'
}
busy() {
  printf '\033[2J\033[H\342\200\242 \033[1mWorking\033[0m (0s \342\200\242 esc to \033[2minterrupt\033[0m)\r\n\r\n\342\200\272 \r\n'
}
composer
while read line; do
  case "$line" in
    work)
      busy
      i=1; while [ $i -le 4 ]; do sleep 0.3; printf '\033[1;12H%ss' $i; i=$((i+1)); done
      composer ;;
    stall) busy ;;
    approve) printf '\033[2J\033[H\342\200\242 I will run the tests.\r\n\r\n  Would you like to run the following command?\r\n\r\n  $ cargo test\r\n\r\n\342\200\272 1. Yes, proceed (y)\r\n  2. No, and tell Codex what to do differently (esc)\r\n\r\n  Press enter to confirm or esc to cancel\r\n' ;;
    y) busy; i=1; while [ $i -le 6 ]; do sleep 0.3; printf '\033[1;12H%ss' $i; i=$((i+1)); done ;;
  esac
done"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_status_from_its_screen_and_turn_complete_notify() {
    let hub = TestHub::start("exec cat", CODEX_TUI).await;
    let workspace = hub
        .post(
            "/api/workspaces",
            json!({ "name": "codex", "repos": [hub.repo()] }),
        )
        .await;
    let wid = workspace["id"].as_str().unwrap().to_owned();
    let mut events = hub.events("laptop").await;
    next_event(&mut events, "snapshot").await;
    let session = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "kind": "codex" }),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    assert!(session.get("agent_session_id").is_none(), "{session}");

    // Launched with `-c notify=[<fw>, "hub", "report", ...]` (TOML).
    let (args, env) = launched(&hub, "codex").await;
    assert_eq!(
        args,
        [
            "-c".to_owned(),
            format!(
                r#"notify=["{REPORT_PROGRAM}","hub","report","--source","codex","--event","notify"]"#
            )
        ]
    );
    // The composer: idle before any turn.
    status_becomes(&mut events, &sid, "idle").await;
    // The screen monitor is not a viewer.
    let view: Value = hub.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(view["live"]["clients"], 0, "{view}");

    let mut term = hub.ws(&format!("/api/sessions/{sid}/ws")).await.unwrap();
    let mut type_line = async |text: &str| {
        term.send(Message::Text(
            json!({ "type": "input", "data": text }).to_string().into(),
        ))
        .await
        .unwrap();
    };
    type_line("work\r").await;
    status_becomes(&mut events, &sid, "working").await;
    let why = hub.get(&format!("/api/sessions/{sid}/explain")).await;
    assert_eq!(why["source_label"], "manifest:codex.toml#turn_status");
    // Codex runs `notify` once the turn has ended (its status line gone,
    // the composer back): the stand-in's turn redraws for 1.2 s. A notify
    // sent while it still draws "Working" would be followed by screens
    // that honestly read as working again.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    // Codex runs `notify` with the payload as its last argument.
    let notify = json!({
        "type": "agent-turn-complete",
        "thread-id": "thr_123",
        "turn-id": "1",
        "last-assistant-message": "Fixed.",
    });
    send(&env, "codex", "notify", None, Some(notify)).unwrap();
    let done = status_becomes(&mut events, &sid, "done").await;
    assert_eq!(done["workspace_status"], "done");
    // The thread it names is recorded before the turn reads done (its
    // rollout is read for the turn's commands first).
    assert_eq!(
        hub.get(&format!("/api/sessions/{sid}")).await["agent_session_id"],
        "thr_123"
    );
    let why = hub.get(&format!("/api/sessions/{sid}/explain")).await;
    assert_eq!(
        why["source"],
        json!({"kind": "hook", "agent": "codex", "event": "notify"})
    );

    // Typing into the composer never reads as working, and the composer
    // after the turn does not undo done; a busy line that freezes with no
    // turn-complete is honestly unknown.
    type_line("plain typing").await;
    // Drawn, then held for several of the monitor's looks.
    screen_shows(&hub, &sid, "plain typing").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        hub.get(&format!("/api/sessions/{sid}")).await["agent"]["status"],
        "done"
    );
    type_line("\u{15}stall\r").await;
    status_becomes(&mut events, &sid, "working").await;
    let quiet = Instant::now();
    status_becomes(&mut events, &sid, "unknown").await;
    assert!(
        quiet.elapsed() >= Duration::from_secs(4),
        "{:?}",
        quiet.elapsed()
    );
    let why = hub.get(&format!("/api/sessions/{sid}/explain")).await;
    assert_eq!(why["source_label"], "heuristic");
    // And it stays unknown while the frozen line holds: no flipping back
    // to working (each flip would push a status event and a git run).
    let since = hub.get(&format!("/api/sessions/{sid}")).await["agent"]["since"].clone();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let agent = hub.get(&format!("/api/sessions/{sid}")).await["agent"].clone();
    assert_eq!(agent["status"], "unknown", "{agent}");
    assert_eq!(agent["since"], since, "{agent}");
    drop(term);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_approval_prompts_block_until_answered() {
    let hub = TestHub::start("exec cat", CODEX_TUI).await;
    let workspace = hub
        .post(
            "/api/workspaces",
            json!({ "name": "codex", "repos": [hub.repo()] }),
        )
        .await;
    let wid = workspace["id"].as_str().unwrap().to_owned();
    let mut events = hub.events("phone").await;
    next_event(&mut events, "snapshot").await;
    let session = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "kind": "codex", "cols": 100, "rows": 30 }),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    let (_, env) = launched(&hub, "codex").await;
    status_becomes(&mut events, &sid, "idle").await;
    let mut term = hub.ws(&format!("/api/sessions/{sid}/ws")).await.unwrap();
    let mut type_line = async |text: &str| {
        term.send(Message::Text(
            json!({ "type": "input", "data": text }).to_string().into(),
        ))
        .await
        .unwrap();
    };

    // The approval overlay: blocked, with its question as the detail, and
    // the workspace rolls up blocked.
    type_line("approve\r").await;
    let blocked = status_becomes(&mut events, &sid, "blocked").await;
    assert_eq!(
        blocked["status"]["detail"],
        "Would you like to run the following command? $ cargo test"
    );
    assert_eq!(blocked["workspace_status"], "blocked");
    let why = hub.get(&format!("/api/sessions/{sid}/explain")).await;
    assert_eq!(why["status"]["status"], "blocked");
    assert_eq!(why["source_label"], "manifest:codex.toml#approval");
    assert_eq!(why["screen"]["state"], "blocked");
    let evidence: Vec<&str> = why["screen"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["text"].as_str().unwrap().trim())
        .collect();
    assert!(
        evidence.contains(&"Would you like to run the following command?")
            && evidence.contains(&"Press enter to confirm or esc to cancel"),
        "{why}"
    );
    assert!(
        why["note"]
            .as_str()
            .unwrap()
            .starts_with("idle is not authorization")
    );

    // Answered: the turn runs on at once, then notify ends it.
    type_line("y\r").await;
    status_becomes(&mut events, &sid, "working").await;
    send(
        &env,
        "codex",
        "notify",
        None,
        Some(json!({ "type": "agent-turn-complete", "thread-id": "thr_9" })),
    )
    .unwrap();
    status_becomes(&mut events, &sid, "done").await;

    // A shell session explains with the shell manifest and has no status.
    let shell = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": wid, "command": ["cat"] }),
        )
        .await;
    let why = hub
        .get(&format!(
            "/api/sessions/{}/explain",
            shell["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(why["kind"], "shell");
    assert_eq!(why["manifest"]["file"], "shell.toml");
    assert!(why["status"].is_null(), "{why}");
    let missing = hub
        .authed(reqwest::Method::GET, "/api/sessions/nope/explain")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    drop(term);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_socket_checks_origin_auth_and_client_ids() {
    let hub = TestHub::start("exec cat", "exec cat").await;
    assert_eq!(hub.ws("/api/events?client=bad%20id").await.err(), Some(400));
    let long = "a".repeat(65);
    assert_eq!(
        hub.ws(&format!("/api/events?client={long}")).await.err(),
        Some(400)
    );
    // No client id: the connection gets its own.
    let mut anonymous = hub.ws("/api/events").await.unwrap();
    next_event(&mut anonymous, "snapshot").await;
    // Bad messages get an error, not a drop.
    anonymous
        .send(Message::Text("{\"type\":\"bogus\"}".into()))
        .await
        .unwrap();
    assert!(
        next_event(&mut anonymous, "error").await["message"]
            .as_str()
            .unwrap()
            .contains("bad message")
    );

    let url = format!("ws://{}/api/events?client=x", hub.addr);
    let mut cross = url.clone().into_client_request().unwrap();
    cross
        .headers_mut()
        .insert("cookie", hub.cookie.parse().unwrap());
    cross
        .headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    match tokio_tungstenite::connect_async(cross).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => assert_eq!(r.status(), 403),
        other => panic!("cross-origin events socket accepted: {:?}", other.is_ok()),
    }
    let anon = url.into_client_request().unwrap();
    match tokio_tungstenite::connect_async(anon).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => assert_eq!(r.status(), 401),
        other => panic!(
            "unauthenticated events socket accepted: {:?}",
            other.is_ok()
        ),
    }

    // Shell sessions carry no agent status; workspace edits push `sessions`.
    let workspace = hub
        .post(
            "/api/workspaces",
            json!({ "name": "shells", "repos": [hub.repo()] }),
        )
        .await;
    next_event(&mut anonymous, "sessions").await;
    let shell = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": workspace["id"], "command": ["/bin/sh"] }),
        )
        .await;
    assert!(shell["agent"].is_null(), "{shell}");
    next_event(&mut anonymous, "sessions").await;
    // A second hub on the same state dir is refused (the first holds its
    // serve lock), and the first's report socket keeps working.
    let second = Hub::open(HubOptions::new(hub.dir.path().join("state")));
    assert!(
        second
            .unwrap_err()
            .to_string()
            .contains("another hub is already serving"),
    );
    let socket = report::socket_path(&hub.dir.path().join("state")).unwrap();
    assert!(
        std::os::unix::net::UnixStream::connect(&socket).is_ok(),
        "the refused hub left the first's report socket alone"
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_rotation_closes_events_sockets_but_not_agent_hooks() {
    let hub = TestHub::start("exec cat", "exec cat").await;
    let workspace = hub
        .post(
            "/api/workspaces",
            json!({ "name": "agents", "repos": [hub.repo()] }),
        )
        .await;
    let session = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": workspace["id"], "kind": "claude" }),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    let (args, env) = launched(&hub, "claude").await;
    let claude = Claude {
        settings: serde_json::from_str(&args[3]).unwrap(),
        env,
    };
    let mut events = hub.events("phone").await;
    next_event(&mut events, "snapshot").await;

    let rotated = hub
        .authed(reqwest::Method::POST, "/api/token/rotate")
        .send()
        .await
        .unwrap();
    assert_eq!(rotated.status(), StatusCode::NO_CONTENT);
    // The events socket of the old login closes with 4001.
    let close = loop {
        match timeout(Duration::from_secs(10), events.next())
            .await
            .expect("closed in time")
        {
            Some(Ok(Message::Close(frame))) => break frame,
            Some(Ok(_)) => {}
            other => panic!("expected a close frame, got {other:?}"),
        }
    };
    assert_eq!(u16::from(close.unwrap().code), 4001);
    // The old login cannot open a new one.
    assert_eq!(hub.ws("/api/events?client=phone").await.err(), Some(401));

    // The running agent's hooks use its per-spawn secret, not the token:
    // they keep reporting.
    claude
        .fire("UserPromptSubmit", None, json!({ "prompt": "hi" }))
        .unwrap();
    let new_token = std::fs::read_to_string(hub.dir.path().join("state/token"))
        .unwrap()
        .trim()
        .to_owned();
    assert_ne!(new_token, hub.token);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let view: Value = hub
            .http
            .get(format!("http://{}/api/sessions/{sid}", hub.addr))
            .bearer_auth(&new_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if view["agent"]["status"] == "working" {
            break;
        }
        assert!(Instant::now() < deadline, "{view}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    hub.shutdown().await;
}
