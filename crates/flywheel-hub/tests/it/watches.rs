//! Watches end to end over a real hub on 127.0.0.1:0, stand-in `claude`s
//! in real PTYs driven the way Claude Code's hooks drive them: a lead
//! watches the sessions it started; their finished turns, coming together,
//! are queued into the lead as one prompt (later ones joining it while it
//! waits), typed in once the lead's turn is over; the watch ends with its
//! lead.

use crate::support;

use flywheel_hub::report::{self, Report};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep};

const WAIT: Duration = Duration::from_secs(60);
/// The quiet window these hubs use (`queue.typing_quiet`).
const QUIET: Duration = Duration::from_secs(2);

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

    /// [`Agent::fire`] with the hook's JSON payload.
    async fn fire_with(&self, event: &str, payload: Value) {
        let env = self.env().await;
        report::send(
            Path::new(&env[report::SOCKET_ENV]),
            &Report {
                session: env[report::SESSION_ENV].clone(),
                secret: env[report::SECRET_ENV].clone(),
                source: "claude".into(),
                event: event.into(),
                detail: None,
                payload: Some(payload),
            },
        )
        .unwrap();
    }

    fn typed(&self) -> Vec<u8> {
        std::fs::read(self.path("in")).unwrap_or_default()
    }
}

async fn queue_of(t: &TestHub, sid: &str) -> Value {
    t.get(&format!("/api/sessions/{sid}/queue")).await
}

/// The pending items a watch queued for `sid`, once there are some whose
/// text holds `want` lines.
async fn watch_items(t: &TestHub, sid: &str, lines: usize) -> Vec<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let queue = queue_of(t, sid).await;
        let items: Vec<Value> = queue["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["watch"].is_string() && i["state"] == "pending")
            .cloned()
            .collect();
        if items
            .iter()
            .any(|i| i["text"].as_str().unwrap().matches("\n- ").count() >= lines)
        {
            return items;
        }
        assert!(
            Instant::now() < deadline,
            "never queued {lines} lines: {queue}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lead_is_woken_once_for_children_finishing_together() {
    let t = TestHub::start().await;
    let ws = t.workspace("w").await;
    let lead = t.claude(&ws, "lead").await;
    lead.fire("SessionStart", None).await;
    lead.fire("UserPromptSubmit", None).await;
    t.status_is(&lead.id, "working").await;
    let (status, _) = t
        .call(
            reqwest::Method::PUT,
            &format!("/api/sessions/{}/start-policy", lead.id),
            Some(json!({ "policy": "allowed" })),
        )
        .await;
    assert!(status.is_success());
    let mut children = Vec::new();
    for name in ["c1", "c2", "c3"] {
        let made = t
            .post(
                "/api/sessions",
                json!({ "workspace_id": ws, "kind": "claude", "name": name,
                        "opened_by": lead.id, "cols": 100, "rows": 30 }),
            )
            .await;
        assert_eq!(made["labels"]["fw.parent"], lead.id.as_str(), "{made}");
        let child = Agent {
            id: made["id"].as_str().unwrap().to_owned(),
            log: t.root.join("log"),
        };
        child.env().await;
        child.fire("SessionStart", None).await;
        children.push(child);
    }
    // A shell is not woken: a watch is for agents.
    let shell = t.session(&ws, "shell", "sh").await;
    let (status, refused) = t
        .call(
            reqwest::Method::POST,
            "/api/watches",
            Some(json!({ "owner": shell, "filter": "state=done", "prompt": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let watch = t
        .post(
            "/api/watches",
            json!({ "owner": "lead", "filter": "children state=done|asking",
                    "prompt": "Children finished: review them.", "window_secs": 1,
                    "pace_secs": 30 }),
        )
        .await;
    assert_eq!(watch["owner"], lead.id.as_str());
    assert_eq!(watch["filter"]["children"], lead.id.as_str());
    assert_eq!(watch["pace_secs"], 30);
    // Two finish: held for the pace, nothing queued yet.
    for child in &children[..2] {
        child.fire("UserPromptSubmit", None).await;
        child.fire("Stop", None).await;
    }
    sleep(Duration::from_secs(2)).await;
    let queue = queue_of(&t, &lead.id).await;
    assert!(
        queue["items"].as_array().unwrap().is_empty(),
        "finished turns wait for the pace: {queue}"
    );
    // The third asks: the lead is woken at once, the held two with it.
    children[2].fire("UserPromptSubmit", None).await;
    children[2]
        .fire_with(
            "Stop",
            json!({ "last_assistant_message": "Built it. Should I merge c3 now?" }),
        )
        .await;
    let items = watch_items(&t, &lead.id, 3).await;
    assert_eq!(items.len(), 1, "coalesced into one prompt: {items:?}");
    assert!(
        items[0]["text"].as_str().unwrap().contains("-> asking"),
        "{items:?}"
    );
    let text = items[0]["text"].as_str().unwrap().to_owned();
    assert!(
        text.starts_with("Children finished: review them."),
        "{text}"
    );
    for name in ["c1", "c2", "c3"] {
        assert!(text.contains(&format!(" {name} (")), "{text}");
    }
    assert!(text.contains("-> done"), "{text}");
    // One more while it waits joins the same prompt.
    children[0].fire("UserPromptSubmit", None).await;
    children[0].fire("Stop", None).await;
    let items = watch_items(&t, &lead.id, 4).await;
    assert_eq!(items.len(), 1, "{items:?}");
    let joined = items[0]["text"].as_str().unwrap().to_owned();
    assert!(
        !joined.contains(" lead ("),
        "the lead's own events never match: {joined}"
    );
    let listed = t.get(&format!("/api/watches?session={}", lead.id)).await;
    assert_eq!(listed["watches"][0]["fired"], 1, "{listed}");
    assert_eq!(listed["watches"][0]["events"], 4, "{listed}");
    // The queue shows it armed.
    assert_eq!(
        queue_of(&t, &lead.id).await["watches"][0]["id"],
        watch["id"]
    );
    // The lead's turn ends: the prompt is typed in, once.
    lead.fire("Stop", None).await;
    let deadline = Instant::now() + WAIT;
    while !String::from_utf8_lossy(&lead.typed()).contains("review them.") {
        assert!(Instant::now() < deadline, "never delivered");
        sleep(Duration::from_millis(50)).await;
    }
    let typed = String::from_utf8_lossy(&lead.typed()).to_string();
    assert_eq!(typed.matches("review them.").count(), 1, "{typed}");
    // The watch ends with its lead.
    let (status, _) = t
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}", lead.id),
            None,
        )
        .await;
    assert!(status.is_success());
    let after = t.get("/api/watches").await;
    assert_eq!(after["watches"].as_array().unwrap().len(), 0, "{after}");
}

#[tokio::test(flavor = "multi_thread")]
async fn five_hundred_children_finishing_at_once_are_one_prompt() {
    use flywheel_hub::telemetry::{Draft, What, claude, now_ms};
    let t = TestHub::start().await;
    let ws = t.workspace("w").await;
    let lead = t.claude(&ws, "lead").await;
    lead.fire("SessionStart", None).await;
    lead.fire("UserPromptSubmit", None).await;
    t.status_is(&lead.id, "working").await;
    t.post(
        "/api/watches",
        json!({ "owner": lead.id, "filter": "children state=done|failed",
                "prompt": "Batch done.", "window_secs": 2, "pace_secs": 0 }),
    )
    .await;
    // Five hundred children the lead opened (their reports, as captured
    // runs would send them), finishing within a second.
    let telemetry = t.hub.telemetry();
    let ts = now_ms();
    for n in 0..500 {
        let child = format!("child-{n}");
        let mut drafts = vec![Draft::new(
            ts,
            What::LabelsChanged {
                set: [("fw.parent".to_owned(), Some(lead.id.clone()))].into(),
                before: Default::default(),
            },
        )];
        drafts.extend(claude::hook(
            "UserPromptSubmit",
            &json!({"prompt": "go"}),
            ts + 1,
        ));
        drafts.extend(claude::hook("Stop", &json!({}), ts + 2));
        telemetry.record(&child, drafts);
    }
    let items = watch_items(&t, &lead.id, flywheel_hub::watch::MAX_LINES).await;
    assert_eq!(items.len(), 1, "one prompt for the burst");
    let text = items[0]["text"].as_str().unwrap();
    assert!(text.contains("] 500 events"), "{text}");
    assert!(
        text.contains("- and 460 more: fw hub tail --after "),
        "{text}"
    );
    assert!(text.len() < 16 * 1024, "{} bytes", text.len());
}

/// The state a session shows (`agent.state`).
async fn shown(t: &TestHub, sid: &str) -> Value {
    t.get("/api/sessions").await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == sid)
        .map_or(Value::Null, |s| s["agent"]["state"].clone())
}

async fn shown_is(t: &TestHub, sid: &str, want: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let now = shown(t, sid).await;
        if now == want {
            return;
        }
        assert!(Instant::now() < deadline, "{sid} shows {now}, never {want}");
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_started_child_reports_by_its_closing_message_and_holds_its_lead() {
    use flywheel_hub::watch::{DEFAULT_CHILDREN_FILTER, DEFAULT_CHILDREN_PROMPT};
    let t = TestHub::start().await;
    let ws = t.workspace("w").await;
    let lead = t.claude(&ws, "lead").await;
    lead.fire("SessionStart", None).await;
    lead.fire_with("UserPromptSubmit", json!({ "prompt": "lead the work" }))
        .await;
    t.status_is(&lead.id, "working").await;
    let (status, _) = t
        .call(
            reqwest::Method::PUT,
            &format!("/api/sessions/{}/start-policy", lead.id),
            Some(json!({ "policy": "allowed" })),
        )
        .await;
    assert!(status.is_success());
    let made = t
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "claude", "name": "c1",
                    "opened_by": lead.id, "cols": 100, "rows": 30 }),
        )
        .await;
    let child = Agent {
        id: made["id"].as_str().unwrap().to_owned(),
        log: t.root.join("log"),
    };
    child.fire("SessionStart", None).await;
    child
        .fire_with("UserPromptSubmit", json!({ "prompt": "do it" }))
        .await;
    t.status_is(&child.id, "working").await;

    // Starting it armed the lead's children watch.
    let listed = t.get(&format!("/api/watches?session={}", lead.id)).await;
    let watches = listed["watches"].as_array().unwrap();
    assert_eq!(watches.len(), 1, "{listed}");
    assert_eq!(watches[0]["filter"]["children"], lead.id.as_str());
    assert_eq!(watches[0]["prompt"], DEFAULT_CHILDREN_PROMPT);
    let armed = watches[0]["id"].clone();
    // Arming the same filter again replaces its prompt: still one watch.
    t.post(
        "/api/watches",
        json!({ "owner": lead.id, "filter": DEFAULT_CHILDREN_FILTER, "prompt": "Mine.",
                "pace_secs": 0 }),
    )
    .await;
    let listed = t.get(&format!("/api/watches?session={}", lead.id)).await;
    let watches = listed["watches"].as_array().unwrap();
    assert_eq!(watches.len(), 1, "{listed}");
    assert_eq!(watches[0]["id"], armed);
    assert_eq!(watches[0]["prompt"], "Mine.");
    assert_eq!(watches[0]["pace_secs"], 0, "re-arming sets its pace");

    // The lead's turn ends while its child works: it waits, not done.
    lead.fire_with(
        "Stop",
        json!({ "last_assistant_message": "Waiting on c1." }),
    )
    .await;
    shown_is(&t, &lead.id, "background").await;

    // The child finishes: its closing message reaches the lead in full,
    // and the lead, waiting on nobody now, is done.
    let report = "Outcome: shipped.\nbranch c1 at abc123\nNeeds: review.";
    child
        .fire_with("Stop", json!({ "last_assistant_message": report }))
        .await;
    let deadline = Instant::now() + WAIT;
    let want =
        "closing message:\r    Outcome: shipped.\r    branch c1 at abc123\r    Needs: review.";
    let alt =
        "closing message:\n    Outcome: shipped.\n    branch c1 at abc123\n    Needs: review.";
    loop {
        let typed = String::from_utf8_lossy(&lead.typed()).to_string();
        if typed.contains(want) || typed.contains(alt) {
            assert!(typed.contains("Mine."), "{typed}");
            break;
        }
        assert!(Instant::now() < deadline, "never delivered: {typed:?}");
        sleep(Duration::from_millis(50)).await;
    }
    shown_is(&t, &lead.id, "done").await;
}
