//! Prompt queues end to end: real hubs on 127.0.0.1:0, a stand-in
//! `claude` in a real PTY that logs every byte typed into it (raw mode,
//! bracketed paste on, as the real TUI runs), and its status driven the
//! way Claude Code's hooks drive it -- reports over the real report
//! socket. Ordering, triggers, pause, never while blocked or typed into,
//! holds, stalls, exactly-once across a restart (reconciled from the
//! transcript), and a queue on another machine through the proxy.

mod support;

use flywheel_hub::agent::claude_project_dir_name;
use flywheel_hub::report::{self, Report};
use flywheel_hub::{Hub, HubEvent, HubOptions, ServerOptions, serve};
use futures_util::SinkExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const WAIT: Duration = Duration::from_secs(60);
/// The quiet window these hubs use (`queue.typing_quiet`).
const QUIET: Duration = Duration::from_secs(2);
/// Long enough for the hub to have delivered if it were going to: the
/// settle time, a few looks, and the paste's Enter.
const WOULD_HAVE: Duration = Duration::from_millis(3000);

/// A stand-in `claude`: per hub session, its argv (`claude-<id>.argv`,
/// NUL-separated), environment (`.env`), and every byte typed into it
/// (`.in`, appended across relaunches), in raw mode with bracketed paste.
fn fake_claude(bin: &Path, log: &Path) -> PathBuf {
    let path = bin.join("claude");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nn='{log}/claude-'\"$FLYWHEEL_HUB_SESSION\"\n\
             for a in \"$@\"; do printf '%s\\000' \"$a\"; done > \"$n.argv\"\n\
             env > \"$n.env.tmp\" && mv \"$n.env.tmp\" \"$n.env\"\n\
             stty raw -echo\nprintf '\\033[?2004h'\nexec cat >> \"$n.in\"\n",
            log = log.display()
        ),
    );
    path
}

/// What the hub types for one delivered prompt.
fn pasted(text: &str) -> Vec<u8> {
    let mut bytes = flywheel_hub::pty::paste_bytes(text, true);
    bytes.push(b'\r');
    bytes
}

struct TestHub {
    dir: Option<tempfile::TempDir>,
    root: PathBuf,
    addr: SocketAddr,
    hub: Arc<Hub>,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["repo/.git", "bin", "log"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self::open(dir, root).await
    }

    async fn open(dir: tempfile::TempDir, root: PathBuf) -> Self {
        flywheel_hub::install_crypto_provider();
        let mut options = HubOptions::new(root.join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_claude(&root.join("bin"), &root.join("log"))
            .to_string_lossy()
            .into_owned();
        options.agents.codex = root.join("bin/no-codex").to_string_lossy().into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(root.join("claude-config"));
        options.queue_typing_quiet = QUIET;
        let hub = support::open_hub(options);
        hub.restore_sessions();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            Arc::clone(&hub),
            ServerOptions::default(),
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            dir: Some(dir),
            root,
            addr,
            hub,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    /// Stop gracefully and open a new hub over the same state dir (its
    /// sessions restored, agents resumed).
    async fn restart(mut self) -> Self {
        let dir = self.dir.take().unwrap();
        let root = self.root.clone();
        let _ = self.stop.take().unwrap().send(());
        timeout(WAIT, self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        drop(self);
        Self::open(dir, root).await
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
            .bearer_auth(self.hub.token().expose());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
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

    /// A claude session in a fresh workspace, its transcript started (so
    /// a restart resumes it), and its stand-in up.
    async fn claude(&self) -> Agent {
        let ws = self
            .post(
                "/api/workspaces",
                json!({ "name": format!("ws-{}", rand_tag()), "folders": [self.root.join("repo")] }),
            )
            .await;
        let session = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": ws["id"], "kind": "claude", "cols": 100, "rows": 30 }),
            )
            .await;
        let id = session["id"].as_str().unwrap().to_owned();
        let conversation = session["agent_session_id"].as_str().unwrap().to_owned();
        let project = self
            .root
            .join("claude-config/projects")
            .join(claude_project_dir_name(&self.root.join("repo")));
        std::fs::create_dir_all(&project).unwrap();
        let transcript = project.join(format!("{conversation}.jsonl"));
        std::fs::write(&transcript, "").unwrap();
        let agent = Agent {
            id,
            log: self.root.join("log"),
            transcript,
        };
        agent.env(None).await;
        agent
    }

    async fn queue(&self, sid: &str) -> Value {
        self.get(&format!("/api/sessions/{sid}/queue")).await
    }

    async fn add(&self, sid: &str, text: &str, when: Option<&str>) -> Value {
        self.post(
            &format!("/api/sessions/{sid}/queue"),
            json!({ "text": text, "when": when }),
        )
        .await
    }

    async fn session(&self, sid: &str) -> Value {
        self.get("/api/sessions").await["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == sid)
            .cloned()
            .unwrap_or(Value::Null)
    }

    async fn ws(
        &self,
        path: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.hub.token().expose())
                .parse()
                .unwrap(),
        );
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
}

fn rand_tag() -> String {
    flywheel_hub::state::new_id()[..8].to_owned()
}

/// One stand-in agent session.
struct Agent {
    id: String,
    log: PathBuf,
    transcript: PathBuf,
}

impl Agent {
    fn path(&self, ext: &str) -> PathBuf {
        self.log.join(format!("claude-{}.{ext}", self.id))
    }

    /// The current spawn's environment (a different secret than
    /// `previous`: a relaunch's).
    async fn env(&self, previous: Option<&str>) -> HashMap<String, String> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(text) = std::fs::read_to_string(self.path("env")) {
                let env: HashMap<String, String> = text
                    .lines()
                    .filter_map(|l| l.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
                if env
                    .get(report::SECRET_ENV)
                    .is_some_and(|s| Some(s.as_str()) != previous)
                {
                    return env;
                }
            }
            assert!(
                Instant::now() < deadline,
                "claude {} never started",
                self.id
            );
            sleep(Duration::from_millis(25)).await;
        }
    }

    /// Run the hook for `event` the way Claude Code does.
    async fn fire(&self, event: &str, detail: Option<&str>) {
        let env = self.env(None).await;
        report::send(
            Path::new(&env[report::SOCKET_ENV]),
            &Report {
                session: env[report::SESSION_ENV].clone(),
                secret: env[report::SECRET_ENV].clone(),
                source: "claude".into(),
                event: event.into(),
                detail: detail.map(str::to_owned),
                payload: None,
            },
        )
        .unwrap();
    }

    /// A finished turn: the prompt was taken (working), then done.
    async fn turn(&self) {
        self.fire("UserPromptSubmit", None).await;
        sleep(Duration::from_millis(50)).await;
        self.fire("Stop", None).await;
    }

    fn typed(&self) -> Vec<u8> {
        std::fs::read(self.path("in")).unwrap_or_default()
    }

    /// Wait until exactly `want` has been typed into it.
    async fn typed_becomes(&self, want: &[u8]) {
        let deadline = Instant::now() + WAIT;
        loop {
            let got = self.typed();
            if got == want {
                return;
            }
            assert!(
                want.starts_with(&got) && Instant::now() < deadline,
                "typed {:?}, want {:?}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(want)
            );
            sleep(Duration::from_millis(25)).await;
        }
    }

    /// Nothing more than `want` is typed in [`WOULD_HAVE`].
    async fn stays(&self, want: &[u8]) {
        sleep(WOULD_HAVE).await;
        assert_eq!(
            String::from_utf8_lossy(&self.typed()),
            String::from_utf8_lossy(want),
            "typed while it should have waited"
        );
    }
}

/// Wait for status `want` on the hub (the stand-in's reports are applied
/// asynchronously).
async fn status_is(hub: &TestHub, sid: &str, want: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        if hub.session(sid).await["agent"]["status"] == want {
            return;
        }
        assert!(Instant::now() < deadline, "{sid} never became {want}");
        sleep(Duration::from_millis(25)).await;
    }
}

/// The next plugin lifecycle event `name`'s payload.
async fn lifecycle(events: &mut broadcast::Receiver<HubEvent>, name: &str) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, events.recv()).await {
            Ok(Ok(HubEvent::Lifecycle { event, payload })) if event == name => return payload,
            Ok(Ok(_) | Err(broadcast::error::RecvError::Lagged(_))) => {}
            other => panic!("no {name} event: {other:?}"),
        }
    }
}

fn texts(view: &Value, state: &str) -> Vec<String> {
    view["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["state"] == state)
        .map(|i| i["text"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_prompts_go_one_finished_turn_at_a_time_in_order() {
    let hub = TestHub::start().await;
    let agent = hub.claude().await;
    let mut events = hub.hub.subscribe();
    agent.fire("UserPromptSubmit", None).await;
    status_is(&hub, &agent.id, "working").await;

    hub.add(&agent.id, "first: write the tests", None).await;
    hub.add(&agent.id, "second: make them pass", Some("done"))
        .await;
    let added = hub.add(&agent.id, "zeroth: read the spec", None).await;
    assert_eq!(added["queue"]["pending"], 3);
    assert_eq!(added["item"]["when"], "done");
    // The third goes to the front.
    let third = added["item"]["id"].as_str().unwrap().to_owned();
    let moved = hub
        .post(
            &format!(
                "/api/sessions/{}/queue/items/{}/move",
                agent.id,
                &third[..8]
            ),
            json!({ "position": 1 }),
        )
        .await;
    assert_eq!(
        texts(&moved, "pending"),
        [
            "zeroth: read the spec",
            "first: write the tests",
            "second: make them pass"
        ]
    );
    assert!(
        moved["waiting"].as_str().unwrap().contains("working"),
        "{moved}"
    );
    // The row and the list both count it.
    assert_eq!(hub.session(&agent.id).await["queue"]["pending"], 3);

    // Never while the turn runs.
    agent.stays(b"").await;

    // Done: the head goes, alone.
    agent.fire("Stop", None).await;
    let mut want = pasted("zeroth: read the spec");
    agent.typed_becomes(&want).await;
    let delivered = lifecycle(&mut events, "queue.delivered").await;
    assert_eq!(delivered["session"]["id"], agent.id.as_str());
    assert_eq!(delivered["item"]["text"], "zeroth: read the spec");
    assert_eq!(delivered["pending"], 2);
    let view = hub.queue(&agent.id).await;
    assert_eq!(texts(&view, "delivered"), ["zeroth: read the spec"]);
    assert!(view["items"][0]["delivered_at"].is_string(), "{view}");
    assert!(
        view["waiting"].as_str().unwrap().contains("take up"),
        "{view}"
    );
    // The agent is still at rest from before the delivery: the next one
    // waits for the turn the delivered prompt starts to finish.
    agent.stays(&want).await;

    agent.turn().await;
    want.extend(pasted("first: write the tests"));
    agent.typed_becomes(&want).await;
    agent.turn().await;
    want.extend(pasted("second: make them pass"));
    agent.typed_becomes(&want).await;
    for text in ["first: write the tests", "second: make them pass"] {
        assert_eq!(
            lifecycle(&mut events, "queue.delivered").await["item"]["text"],
            text
        );
    }
    // Emptied: nothing more goes on the next turn.
    agent.turn().await;
    agent.stays(&want).await;
    let view = hub.queue(&agent.id).await;
    assert_eq!(view["pending"], 0);
    assert_eq!(texts(&view, "delivered").len(), 3);
    assert_eq!(hub.session(&agent.id).await["queue"]["pending"], 0);

    // Edits and removals keep to pending items; a shell has no queue.
    // Paused first: the finished turn would take an item added to a
    // running queue at its next tick.
    hub.post(
        &format!("/api/sessions/{}/queue/pause", agent.id),
        json!({}),
    )
    .await;
    let item = hub.add(&agent.id, "typo", None).await["item"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, edited) = hub
        .call(
            reqwest::Method::PATCH,
            &format!("/api/sessions/{}/queue/items/{item}", agent.id),
            Some(json!({ "text": "fixed", "when": "idle" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(texts(&edited, "pending"), ["fixed"]);
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}/queue/items/1", agent.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let cleared = hub
        .post(
            &format!("/api/sessions/{}/queue/clear", agent.id),
            json!({ "all": true }),
        )
        .await;
    assert_eq!(cleared["items"], json!([]));
    let (status, refused) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/queue", agent.id),
            Some(json!({ "text": "  " })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_while_blocked_and_blocked_cleared_waits_for_the_answer() {
    let hub = TestHub::start().await;
    let agent = hub.claude().await;
    agent.fire("UserPromptSubmit", None).await;
    hub.add(&agent.id, "after the approval", None).await;
    hub.add(&agent.id, "once you have asked me", Some("blocked-cleared"))
        .await;
    agent.fire("Notification", Some("permission_prompt")).await;
    status_is(&hub, &agent.id, "blocked").await;
    agent.stays(b"").await;
    let view = hub.queue(&agent.id).await;
    assert!(
        view["waiting"].as_str().unwrap().contains("blocked"),
        "{view}"
    );

    // Answered, the turn finishes: the first goes.
    agent.fire("PreToolUse", None).await;
    agent.fire("Stop", None).await;
    let mut want = pasted("after the approval");
    agent.typed_becomes(&want).await;

    // blocked-cleared: a turn without a block does not release it.
    agent.turn().await;
    agent.stays(&want).await;
    assert!(
        hub.queue(&agent.id).await["waiting"]
            .as_str()
            .unwrap()
            .contains("block on"),
        "waits for a block"
    );
    agent.fire("UserPromptSubmit", None).await;
    agent.fire("Notification", Some("permission_prompt")).await;
    status_is(&hub, &agent.id, "blocked").await;
    agent.stays(&want).await;
    agent.fire("PreToolUse", None).await;
    status_is(&hub, &agent.id, "working").await;
    agent.stays(&want).await;
    agent.fire("Stop", None).await;
    want.extend(pasted("once you have asked me"));
    agent.typed_becomes(&want).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_while_someone_types_and_idle_waits_for_a_look() {
    let hub = TestHub::start().await;
    let agent = hub.claude().await;
    agent.fire("SessionStart", None).await;
    status_is(&hub, &agent.id, "idle").await;
    // Settle the fresh status first, so only typing holds it back.
    sleep(Duration::from_secs(2)).await;

    // Someone types at the terminal: the queue holds off until they have
    // been quiet for queue.typing_quiet.
    let mut term = hub.ws(&format!("/api/sessions/{}/ws", agent.id)).await;
    let mut want = Vec::new();
    for key in ["h", "e", "y"] {
        term.send(Message::Text(
            json!({ "type": "input", "data": key }).to_string().into(),
        ))
        .await
        .unwrap();
        want.extend(key.as_bytes());
    }
    let typed_at = Instant::now();
    agent.typed_becomes(&want).await;
    hub.add(&agent.id, "while you type", None).await;
    let waiting = hub.queue(&agent.id).await["waiting"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(waiting.contains("typed"), "{waiting}");
    // Keep typing for a while: still nothing.
    for _ in 0..4 {
        sleep(Duration::from_millis(600)).await;
        term.send(Message::Binary(b"!".to_vec().into()))
            .await
            .unwrap();
        want.push(b'!');
    }
    agent.typed_becomes(&want).await;
    assert!(typed_at.elapsed() > QUIET);
    // Quiet now, but "hey!!!!" sits unsent in its input: pasting the
    // prompt and Enter would send the two merged. It waits for that
    // draft to be sent or cleared, however long that takes.
    agent.stays(&want).await;
    let waiting = hub.queue(&agent.id).await["waiting"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(waiting.contains("7 characters"), "{waiting}");
    // Cleared with Backspace: the quiet window still applies from it.
    for _ in 0..7 {
        term.send(Message::Binary(b"\x7f".to_vec().into()))
            .await
            .unwrap();
        want.push(0x7f);
    }
    let last_key = Instant::now();
    agent.typed_becomes(&want).await;
    want.extend(pasted("while you type"));
    agent.typed_becomes(&want).await;
    assert!(
        last_key.elapsed() >= QUIET,
        "delivered {:?} after the last key",
        last_key.elapsed()
    );
    drop(term);

    // idle: a finished turn nobody has looked at keeps it waiting.
    agent.turn().await;
    status_is(&hub, &agent.id, "done").await;
    hub.add(&agent.id, "after you read it", Some("idle")).await;
    agent.stays(&want).await;
    assert!(
        hub.queue(&agent.id).await["waiting"]
            .as_str()
            .unwrap()
            .contains("looked"),
        "waits for a look"
    );
    let mut viewer = hub.ws("/api/events?client=phone").await;
    viewer
        .send(Message::Text(
            json!({ "type": "view", "session_id": agent.id })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    want.extend(pasted("after you read it"));
    agent.typed_becomes(&want).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hubs_own_pastes_count_as_typing_and_a_draft_is_kept_across_an_answer() {
    let hub = TestHub::start().await;
    let agent = hub.claude().await;
    agent.fire("SessionStart", None).await;
    status_is(&hub, &agent.id, "idle").await;
    sleep(Duration::from_secs(2)).await;
    // Paused before the item goes in: added to a running queue, the item
    // is deliverable at once and the next tick could send it before a
    // pause landed.
    hub.post(
        &format!("/api/sessions/{}/queue/pause", agent.id),
        json!({}),
    )
    .await;
    hub.add(&agent.id, "queued meanwhile", None).await;
    // A paste the hub makes (handoff, send findings, a plugin action, the
    // paste API) types into the agent: the queue waits it out like keys.
    hub.post(
        &format!("/api/sessions/{}/paste", agent.id),
        json!({ "text": "a handoff", "run": true }),
    )
    .await;
    let pasted_at = Instant::now();
    let mut want = pasted("a handoff");
    agent.typed_becomes(&want).await;
    let resumed = hub
        .post(
            &format!("/api/sessions/{}/queue/resume", agent.id),
            json!({}),
        )
        .await;
    assert!(
        resumed["waiting"].as_str().unwrap().contains("typed"),
        "{resumed}"
    );
    want.extend(pasted("queued meanwhile"));
    agent.typed_becomes(&want).await;
    assert!(pasted_at.elapsed() >= QUIET, "{:?}", pasted_at.elapsed());

    // A paste without Enter leaves a draft: nothing goes on top of it.
    agent.turn().await;
    hub.add(&agent.id, "not onto a draft", None).await;
    hub.post(
        &format!("/api/sessions/{}/paste", agent.id),
        json!({ "text": "half", "run": false }),
    )
    .await;
    want.extend(flywheel_hub::pty::paste_bytes("half", true));
    agent.typed_becomes(&want).await;
    agent.stays(&want).await;
    assert!(
        hub.queue(&agent.id).await["waiting"]
            .as_str()
            .unwrap()
            .contains("4 characters"),
    );
    // Cleared (Ctrl-U): it goes once the quiet window passes.
    let mut term = hub.ws(&format!("/api/sessions/{}/ws", agent.id)).await;
    term.send(Message::Binary(b"\x15".to_vec().into()))
        .await
        .unwrap();
    want.push(0x15);
    want.extend(pasted("not onto a draft"));
    agent.typed_becomes(&want).await;

    // Keys answering a permission prompt never reach the input line: not
    // a draft, so the turn's end lets the next one go.
    agent.fire("UserPromptSubmit", None).await;
    hub.add(&agent.id, "after the answer", None).await;
    agent.fire("Notification", Some("permission_prompt")).await;
    status_is(&hub, &agent.id, "blocked").await;
    term.send(Message::Binary(b"1".to_vec().into()))
        .await
        .unwrap();
    want.push(b'1');
    agent.typed_becomes(&want).await;
    agent.fire("PreToolUse", None).await;
    status_is(&hub, &agent.id, "working").await;
    agent.fire("Stop", None).await;
    want.extend(pasted("after the answer"));
    agent.typed_becomes(&want).await;
    drop(term);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_open_in_an_editor_waits_and_stall_events_carry_the_item() {
    let hub = TestHub::start().await;
    let agent = hub.claude().await;
    let mut events = hub.hub.subscribe();
    agent.fire("UserPromptSubmit", None).await;
    let added = hub.add(&agent.id, "being edited", None).await;
    let item = added["item"]["id"].as_str().unwrap().to_owned();
    let editing = format!("/api/sessions/{}/queue/items/{item}/editing", agent.id);
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &editing,
            Some(json!({ "editing": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    agent.fire("Stop", None).await;
    agent.stays(b"").await;
    assert!(
        hub.queue(&agent.id).await["waiting"]
            .as_str()
            .unwrap()
            .contains("editor")
    );
    let (status, _) = hub
        .call(
            reqwest::Method::PATCH,
            &format!("/api/sessions/{}/queue/items/{item}", agent.id),
            Some(json!({ "text": "edited" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    hub.post(&editing, json!({ "editing": false })).await;
    let want = pasted("edited");
    agent.typed_becomes(&want).await;
    let delivered = lifecycle(&mut events, "queue.delivered").await;
    assert_eq!(delivered["item"]["text"], "edited");

    // queue.stalled names its item the same way.
    agent.turn().await;
    hub.add(&agent.id, "to a dead agent", Some("idle")).await;
    hub.post(&format!("/api/sessions/{}/kill", agent.id), json!({}))
        .await;
    let stalled = lifecycle(&mut events, "queue.stalled").await;
    assert_eq!(stalled["item"]["text"], "to a dead agent", "{stalled}");
    assert_eq!(stalled["item"]["when"], "idle");
    assert!(stalled["item"]["id"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_resume_holds_and_stalls_that_surface() {
    let hub = TestHub::start().await;
    let agent = hub.claude().await;
    let mut events = hub.hub.subscribe();
    agent.fire("UserPromptSubmit", None).await;
    hub.add(&agent.id, "held back", None).await;
    let paused = hub
        .post(
            &format!("/api/sessions/{}/queue/pause", agent.id),
            json!({}),
        )
        .await;
    assert_eq!(paused["paused"], true);
    assert_eq!(hub.session(&agent.id).await["queue"]["paused"], true);
    agent.fire("Stop", None).await;
    agent.stays(b"").await;
    hub.post(
        &format!("/api/sessions/{}/queue/resume", agent.id),
        json!({}),
    )
    .await;
    let mut want = pasted("held back");
    agent.typed_becomes(&want).await;

    // A review action (or any holder) keeps it back while it runs.
    agent.turn().await;
    let held = hub
        .hub
        .hold_sessions(vec![agent.id.clone()], "a review merge");
    hub.add(&agent.id, "after the merge", None).await;
    agent.stays(&want).await;
    assert!(
        hub.queue(&agent.id).await["waiting"]
            .as_str()
            .unwrap()
            .contains("a review merge is using the session"),
    );
    drop(held);
    want.extend(pasted("after the merge"));
    agent.typed_becomes(&want).await;

    // The agent exits: the queue stalls, says why, and announces it.
    agent.turn().await;
    hub.add(&agent.id, "never typed into a dead agent", None)
        .await;
    hub.post(&format!("/api/sessions/{}/kill", agent.id), json!({}))
        .await;
    let stalled = lifecycle(&mut events, "queue.stalled").await;
    assert!(
        stalled["reason"].as_str().unwrap().contains("exited"),
        "{stalled}"
    );
    assert_eq!(stalled["pending"], 1);
    let row = hub.session(&agent.id).await;
    assert!(
        row["queue"]["stalled"]["reason"]
            .as_str()
            .unwrap()
            .contains("exited"),
        "{row}"
    );
    // Resumed while the agent is still gone, a prompt added says so at
    // once: the answer carries the stall, not a pending item that never
    // goes.
    hub.post(
        &format!("/api/sessions/{}/queue/resume", agent.id),
        json!({}),
    )
    .await;
    let added = hub.add(&agent.id, "and this one", None).await;
    assert!(
        added["queue"]["stalled"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("exited")),
        "{added}"
    );

    // Relaunched, it goes once the new agent is at rest.
    let previous = agent.env(None).await[report::SECRET_ENV].clone();
    hub.post(&format!("/api/sessions/{}/restart", agent.id), json!({}))
        .await;
    agent.env(Some(&previous)).await;
    // The relaunch the stall asked for clears it: no separate resume.
    assert!(hub.queue(&agent.id).await["stalled"].is_null());
    agent.stays(&want).await;
    agent.fire("SessionStart", None).await;
    want.extend(pasted("never typed into a dead agent"));
    agent.typed_becomes(&want).await;
    agent.turn().await;
    want.extend(pasted("and this one"));
    agent.typed_becomes(&want).await;
}

/// Mark the first pending item with `text` as `sending` in the stopped
/// hub's state file.
fn mark_sending(state: &Path, sid: &str, text: &str) {
    let path = state.join("state.json");
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(doc["version"], 6);
    let items = doc["queues"][sid]["items"].as_array_mut().unwrap();
    let item = items.iter_mut().find(|i| i["text"] == text).unwrap();
    item["state"] = json!("sending");
    item["sending_at"] = json!(chrono::Utc::now().to_rfc3339());
    std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_delivers_each_prompt_exactly_once() {
    let hub = TestHub::start().await;
    let agent = hub.claude().await;
    agent.fire("UserPromptSubmit", None).await;
    for text in ["went before it died", "never got there", "still to come"] {
        hub.add(&agent.id, text, None).await;
    }
    let state = hub.root.join("state");
    let before = agent.env(None).await[report::SECRET_ENV].clone();
    let hub = {
        // Stopped as if it died mid-delivery twice over: one paste reached
        // the agent (its transcript has the prompt), one did not.
        let root = hub.root.clone();
        let mut hub = hub;
        let tmp = hub.dir.take().unwrap();
        let _ = hub.stop.take().unwrap().send(());
        timeout(WAIT, hub.server.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(hub);
        mark_sending(&state, &agent.id, "went before it died");
        mark_sending(&state, &agent.id, "never got there");
        let line = json!({
            "type": "user",
            "uuid": "u-1",
            "sessionId": "c",
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "message": { "role": "user", "content": "went before it died" },
        });
        std::fs::write(&agent.transcript, format!("{line}\n")).unwrap();
        TestHub::open(tmp, root).await
    };
    // Settled on start: the first is delivered (from the transcript), the
    // second pending again.
    let view = hub.queue(&agent.id).await;
    assert_eq!(texts(&view, "delivered"), ["went before it died"]);
    assert_eq!(view["items"][0]["reconciled"], true);
    assert_eq!(
        texts(&view, "pending"),
        ["never got there", "still to come"]
    );

    // Resumed (claude --resume), but nothing goes until it reports in.
    agent.env(Some(&before)).await;
    let argv = std::fs::read_to_string(agent.path("argv")).unwrap();
    assert!(argv.starts_with("--resume\0"), "{argv:?}");
    agent.stays(b"").await;
    agent.fire("SessionStart", None).await;
    let mut want = pasted("never got there");
    agent.typed_becomes(&want).await;
    agent.turn().await;
    want.extend(pasted("still to come"));
    agent.typed_becomes(&want).await;

    // A clean restart with nothing in flight types nothing again.
    hub.add(&agent.id, "after the restart", None).await;
    let before = agent.env(None).await[report::SECRET_ENV].clone();
    let hub = hub.restart().await;
    agent.env(Some(&before)).await;
    agent.stays(&want).await;
    agent.fire("SessionStart", None).await;
    want.extend(pasted("after the restart"));
    agent.typed_becomes(&want).await;
    agent.turn().await;
    agent.stays(&want).await;
    assert_eq!(texts(&hub.queue(&agent.id).await, "delivered").len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queue_on_another_machine_works_through_the_proxy() {
    let b = TestHub::start().await;
    let a = TestHub::start().await;
    let agent = b.claude().await;
    agent.fire("UserPromptSubmit", None).await;
    let issued = b.post("/api/peers", json!({ "name": "hub-a" })).await;
    a.post(
        "/api/machines",
        json!({ "name": "b", "url": format!("http://{}", b.addr), "credential": issued["credential"] }),
    )
    .await;

    let added = a
        .post(
            &format!("/m/b/api/sessions/{}/queue", agent.id),
            json!({ "text": "from the laptop", "when": "done" }),
        )
        .await;
    assert_eq!(added["queue"]["pending"], 1);
    // Kept by the hub that owns the session, shown on its row through A.
    assert!(a.hub.queue_views().is_empty());
    let listed = a.get("/m/b/api/sessions").await;
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == agent.id.as_str())
        .cloned()
        .unwrap();
    assert_eq!(row["queue"]["pending"], 1);

    agent.fire("Stop", None).await;
    agent.typed_becomes(&pasted("from the laptop")).await;
    let view = a
        .get(&format!("/m/b/api/sessions/{}/queue", agent.id))
        .await;
    assert_eq!(texts(&view, "delivered"), ["from the laptop"]);
    let paused = a
        .post(
            &format!("/m/b/api/sessions/{}/queue/pause", agent.id),
            json!({}),
        )
        .await;
    assert_eq!(paused["paused"], true);
    assert_eq!(b.queue(&agent.id).await["paused"], true);
}
