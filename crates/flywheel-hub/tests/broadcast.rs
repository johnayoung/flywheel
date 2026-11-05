//! Broadcast end to end: real hubs on 127.0.0.1:0, stand-in `claude`s in
//! real PTYs that log every byte typed into them (raw mode, bracketed
//! paste on, as the real TUI runs), their statuses driven the way Claude
//! Code's hooks drive them. One prompt to agents at rest, working,
//! blocked, and typed into: each sent now or queued (and delivered once
//! its turn is done), never typed while blocked or typed into; shells and
//! unknown names refused with why; the provenance header; the
//! `broadcast.sent` event and activity row; and a broadcast on another
//! machine through the proxy.

mod support;

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
/// Long enough for the hub to have delivered if it were going to.
const WOULD_HAVE: Duration = Duration::from_millis(3000);

/// A stand-in `claude`: per hub session, its environment (`.env`) and
/// every byte typed into it (`.in`), in raw mode with bracketed paste.
fn fake_claude(bin: &Path, log: &Path) -> PathBuf {
    let path = bin.join("claude");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nn='{log}/claude-'\"$FLYWHEEL_HUB_SESSION\"\n\
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
    _dir: tempfile::TempDir,
    root: PathBuf,
    addr: SocketAddr,
    hub: Arc<Hub>,
    http: reqwest::Client,
    _stop: oneshot::Sender<()>,
}

impl TestHub {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["repo/.git", "bin", "log"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(serve(
            listener,
            Arc::clone(&hub),
            ServerOptions::default(),
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            _dir: dir,
            root,
            addr,
            hub,
            http: reqwest::Client::new(),
            _stop: stop,
        }
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

    async fn workspace(&self, name: &str) -> String {
        let ws = self
            .post(
                "/api/workspaces",
                json!({ "name": name, "folders": [self.root.join("repo")] }),
            )
            .await;
        ws["id"].as_str().unwrap().to_owned()
    }

    async fn session(&self, workspace: &str, kind: &str, name: &str) -> String {
        let session = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": workspace, "kind": kind, "name": name, "cols": 100, "rows": 30 }),
            )
            .await;
        session["id"].as_str().unwrap().to_owned()
    }

    /// A stand-in claude session, up.
    async fn claude(&self, workspace: &str, name: &str) -> Agent {
        let id = self.session(workspace, "claude", name).await;
        let agent = Agent {
            id,
            log: self.root.join("log"),
        };
        agent.env().await;
        agent
    }

    async fn status(&self, sid: &str) -> Value {
        self.get("/api/sessions").await["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == sid)
            .map_or(Value::Null, |s| s["agent"]["status"].clone())
    }

    async fn status_is(&self, sid: &str, want: &str) {
        let deadline = Instant::now() + WAIT;
        while self.status(sid).await != want {
            assert!(Instant::now() < deadline, "{sid} never became {want}");
            sleep(Duration::from_millis(25)).await;
        }
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

/// One stand-in agent session.
struct Agent {
    id: String,
    log: PathBuf,
}

impl Agent {
    fn path(&self, ext: &str) -> PathBuf {
        self.log.join(format!("claude-{}.{ext}", self.id))
    }

    async fn env(&self) -> HashMap<String, String> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(text) = std::fs::read_to_string(self.path("env")) {
                let env: HashMap<String, String> = text
                    .lines()
                    .filter_map(|l| l.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
                if env.contains_key(report::SECRET_ENV) {
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
        let env = self.env().await;
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

    fn typed(&self) -> Vec<u8> {
        std::fs::read(self.path("in")).unwrap_or_default()
    }

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

    async fn stays(&self, want: &[u8]) {
        sleep(WOULD_HAVE).await;
        assert_eq!(
            String::from_utf8_lossy(&self.typed()),
            String::from_utf8_lossy(want),
            "typed while it should have waited"
        );
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

/// The result for session `sid` in a broadcast's answer.
fn result_for<'a>(done: &'a Value, sid: &str) -> &'a Value {
    done["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == sid)
        .unwrap_or_else(|| panic!("no result for {sid}: {done}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_prompt_reaches_agents_in_every_state_each_on_its_own_terms() {
    let hub = TestHub::start().await;
    let ws = hub.workspace("api").await;
    let resting = hub.claude(&ws, "resting").await;
    let busy = hub.claude(&ws, "busy").await;
    let asking = hub.claude(&ws, "asking").await;
    let typed_into = hub.claude(&ws, "typed-into").await;
    let shell = hub.session(&ws, "shell", "sh-1").await;
    for agent in [&resting, &typed_into] {
        agent.fire("SessionStart", None).await;
        hub.status_is(&agent.id, "idle").await;
    }
    busy.fire("UserPromptSubmit", None).await;
    hub.status_is(&busy.id, "working").await;
    asking.fire("UserPromptSubmit", None).await;
    asking.fire("Notification", Some("permission_prompt")).await;
    hub.status_is(&asking.id, "blocked").await;
    // Settle the fresh rests, so only typing holds one back.
    sleep(Duration::from_secs(2)).await;
    // Someone is typing into one of them, its line still half-typed.
    let mut term = hub.ws(&format!("/api/sessions/{}/ws", typed_into.id)).await;
    term.send(Message::Binary(b"wip".to_vec().into()))
        .await
        .unwrap();
    typed_into.typed_becomes(b"wip").await;

    let mut events = hub.hub.subscribe();
    let done = hub
        .post(
            "/api/broadcast",
            json!({
                "targets": ["api", "sh-1", "resting", "nope"],
                "text": "rebase on main, then run the tests",
                "header": true,
            }),
        )
        .await;
    let prompt = "Broadcast to 4 agent sessions (resting, busy, asking, typed-into):\n\n\
                  rebase on main, then run the tests";
    assert_eq!(done["header"], true);
    assert_eq!(done["bytes"], prompt.len(), "{done}");
    // The workspace's four agents (the shell is not one of them; named
    // again, `resting` still gets it once), the shell, and `nope`.
    assert_eq!(done["results"].as_array().unwrap().len(), 6, "{done}");
    assert_eq!(
        done["counts"],
        json!({ "sent": 1, "queued": 3, "refused": 2 }),
        "{done}"
    );
    assert_eq!(result_for(&done, &resting.id)["result"], "sent");
    assert_eq!(result_for(&done, &resting.id)["target"], "api");
    let why = |sid: &str| result_for(&done, sid)["why"].as_str().unwrap().to_owned();
    assert_eq!(result_for(&done, &busy.id)["result"], "queued");
    assert!(why(&busy.id).contains("working"), "{}", why(&busy.id));
    assert_eq!(result_for(&done, &asking.id)["result"], "queued");
    assert!(why(&asking.id).contains("blocked"), "{}", why(&asking.id));
    assert_eq!(result_for(&done, &typed_into.id)["result"], "queued");
    // Typed into just now, and its line holds "wip": either holds it.
    let held = why(&typed_into.id);
    assert!(
        held.contains("typed") || held.contains("characters"),
        "{held}"
    );
    let shell_result = result_for(&done, &shell);
    assert_eq!(shell_result["result"], "refused");
    assert!(
        shell_result["why"]
            .as_str()
            .unwrap()
            .contains("shell session"),
        "{shell_result}"
    );
    let unknown = done["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["target"] == "nope")
        .unwrap();
    assert_eq!(unknown["result"], "refused");
    assert!(unknown["session_id"].is_null());
    assert!(
        unknown["why"]
            .as_str()
            .unwrap()
            .contains("no session or workspace"),
        "{unknown}"
    );

    // The one at rest has it now; the others have not.
    resting.typed_becomes(&pasted(prompt)).await;
    busy.stays(b"").await;
    assert_eq!(asking.typed(), b"");
    assert_eq!(typed_into.typed(), b"wip");

    // One event (plugins and the timeline), every target in it.
    let event = lifecycle(&mut events, "broadcast.sent").await;
    assert_eq!(event["broadcast"]["id"], done["id"]);
    assert_eq!(event["broadcast"]["counts"], done["counts"]);
    assert_eq!(event["workspace"]["name"], "api");
    assert_eq!(event["targets"].as_array().unwrap().len(), 6);
    assert!(
        event["broadcast"]["text"]
            .as_str()
            .unwrap()
            .contains("rebase on main")
    );
    let deadline = Instant::now() + WAIT;
    let row = loop {
        let page = hub.get("/api/activity?kind=broadcast").await;
        if let Some(row) = page["rows"].as_array().and_then(|r| r.first()).cloned() {
            break row;
        }
        assert!(Instant::now() < deadline, "no broadcast activity row");
        sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        row["text"],
        "broadcast a prompt to 6 targets: 1 sent, 3 queued, 2 refused"
    );
    assert_eq!(row["workspace"], "api");
    assert!(!row.to_string().contains("rebase on main"), "{row}");

    // Each queued one goes once its agent is done: the busy one when its
    // turn finishes; the blocked one only after the answer and the turn.
    busy.fire("Stop", None).await;
    busy.typed_becomes(&pasted(prompt)).await;
    asking.fire("PreToolUse", None).await;
    asking.stays(b"").await;
    asking.fire("Stop", None).await;
    asking.typed_becomes(&pasted(prompt)).await;
    // The half-typed line is cleared: then, once quiet, it goes.
    term.send(Message::Binary(b"\x7f\x7f\x7f".to_vec().into()))
        .await
        .unwrap();
    let mut want = b"wip\x7f\x7f\x7f".to_vec();
    typed_into.typed_becomes(&want).await;
    want.extend(pasted(prompt));
    typed_into.typed_becomes(&want).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_broadcasts_are_refused_whole() {
    let hub = TestHub::start().await;
    let ws = hub.workspace("solo").await;
    let shells = hub.workspace("shells").await;
    hub.session(&shells, "shell", "only-a-shell").await;
    let agent = hub.claude(&ws, "agent").await;
    for (body, needle) in [
        (json!({ "targets": [], "text": "hi" }), "name the sessions"),
        (json!({ "targets": ["agent"], "text": "  " }), "empty"),
        (
            json!({ "targets": vec!["agent"; 65], "text": "hi" }),
            "at most 64",
        ),
    ] {
        let (status, answer) = hub
            .call(reqwest::Method::POST, "/api/broadcast", Some(body))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        assert!(answer.to_string().contains(needle), "{needle}: {answer}");
    }
    // A workspace with no agent is a refused target, not an error; the
    // agent still gets it (queued: its status is not known yet).
    let done = hub
        .post(
            "/api/broadcast",
            json!({ "targets": ["shells", agent.id], "text": "status?" }),
        )
        .await;
    assert_eq!(done["counts"]["refused"], 1, "{done}");
    assert_eq!(
        done["counts"]["sent"].as_u64().unwrap() + done["counts"]["queued"].as_u64().unwrap(),
        1
    );
    assert!(
        done.to_string()
            .contains("has no Claude Code or Codex session"),
        "{done}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broadcast_on_another_machine_works_through_the_proxy() {
    let b = TestHub::start().await;
    let a = TestHub::start().await;
    let ws = b.workspace("remote-ws").await;
    let one = b.claude(&ws, "one").await;
    let two = b.claude(&ws, "two").await;
    one.fire("SessionStart", None).await;
    two.fire("UserPromptSubmit", None).await;
    b.status_is(&one.id, "idle").await;
    b.status_is(&two.id, "working").await;
    sleep(Duration::from_secs(2)).await;
    let issued = b.post("/api/peers", json!({ "name": "hub-a" })).await;
    a.post(
        "/api/machines",
        json!({ "name": "b", "url": format!("http://{}", b.addr), "credential": issued["credential"] }),
    )
    .await;

    let done = a
        .post(
            "/m/b/api/broadcast",
            json!({ "targets": [ws], "text": "from the laptop" }),
        )
        .await;
    assert_eq!(
        done["counts"],
        json!({ "sent": 1, "queued": 1, "refused": 0 })
    );
    assert_eq!(result_for(&done, &one.id)["result"], "sent");
    assert_eq!(result_for(&done, &two.id)["result"], "queued");
    one.typed_becomes(&pasted("from the laptop")).await;
    // Kept by the hub that owns the sessions.
    assert!(a.hub.queue_views().is_empty());
    assert_eq!(b.hub.queue_views().len(), 1);
    two.fire("Stop", None).await;
    two.typed_becomes(&pasted("from the laptop")).await;
}
