//! Agents ending, replacing, and handing on agent sessions, end to end:
//! real hubs on 127.0.0.1:0, a stand-in `claude` per session in a real
//! PTY that logs every byte typed into it, its status and telemetry
//! driven by the hooks Claude Code would POST, and the agent tools called
//! over the hub's MCP endpoint with each session's own credentials.

mod support;

use flywheel_hub::agent::{SECRET_HEADER, SESSION_HEADER, claude_project_dir_name};
use flywheel_hub::report;
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep, timeout};

const WAIT: Duration = Duration::from_secs(60);
/// The quiet window these hubs use (`queue.typing_quiet`).
const QUIET: Duration = Duration::from_secs(2);
/// Long enough for the hub to have acted if it were going to: the settle
/// time, the quiet window after its own paste, and a few looks.
const WOULD_HAVE: Duration = Duration::from_millis(4500);

/// A stand-in `claude`: per hub session, its argv (`claude-<id>.argv`,
/// NUL-separated), environment (`.env`), and every byte typed into it
/// (`.in`), in raw mode with bracketed paste. With `spawn-next` in the
/// log dir (taken by the next one to start), it first starts a background
/// job the way Claude Code's Bash tool does (a session of its own, under
/// a shell-snapshot shell) and writes the job's pid to `.bg`.
fn fake_claude(bin: &Path, log: &Path) -> PathBuf {
    let path = bin.join("claude");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nn='{log}/claude-'\"$FLYWHEEL_HUB_SESSION\"\n\
             for a in \"$@\"; do printf '%s\\000' \"$a\"; done > \"$n.argv\"\n\
             if rm '{log}/spawn-next' 2>/dev/null; then \
             setsid sh -c 'sleep 300; true' /u/.claude/shell-snapshots/t.sh & \
             echo $! > \"$n.bg\"; fi\n\
             env > \"$n.env.tmp\" && mv \"$n.env.tmp\" \"$n.env\"\n\
             stty raw -echo\nprintf '\\033[?2004h'\nexec cat >> \"$n.in\"\n",
            log = log.display()
        ),
    );
    path
}

struct TestHub {
    dir: Option<tempfile::TempDir>,
    root: PathBuf,
    addr: SocketAddr,
    hub: Arc<Hub>,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    workspace: Option<String>,
}

impl TestHub {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["repo/.git", "bin", "log"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self::open(dir, root, None).await
    }

    async fn open(dir: tempfile::TempDir, root: PathBuf, workspace: Option<String>) -> Self {
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
            workspace,
        }
    }

    /// Stop gracefully, let `edit` change the state file, and open a new
    /// hub over the same state dir (its sessions restored).
    async fn restart(mut self, edit: impl FnOnce(&mut Value)) -> Self {
        let dir = self.dir.take().unwrap();
        let root = self.root.clone();
        let workspace = self.workspace.clone();
        let _ = self.stop.take().unwrap().send(());
        timeout(WAIT, self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        drop(self);
        let path = root.join("state/state.json");
        let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        edit(&mut doc);
        std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        Self::open(dir, root, workspace).await
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

    async fn session(&self, sid: &str) -> Value {
        self.get(&format!("/api/sessions/{sid}")).await
    }

    /// The session named `name`, if any.
    async fn named(&self, name: &str) -> Option<Value> {
        self.get("/api/sessions").await["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .cloned()
    }

    async fn queue(&self, sid: &str) -> Value {
        self.get(&format!("/api/sessions/{sid}/queue")).await
    }

    /// A claude session named `name` (opened by `opened_by`), its
    /// transcript started (so a restart resumes it), and its stand-in up.
    async fn claude(&mut self, name: &str, opened_by: Option<&Agent>, extra: Value) -> Agent {
        if self.workspace.is_none() {
            let ws = self
                .post(
                    "/api/workspaces",
                    json!({ "name": "ws", "folders": [self.root.join("repo")] }),
                )
                .await;
            self.workspace = Some(ws["id"].as_str().unwrap().to_owned());
        }
        let mut body = json!({ "workspace_id": self.workspace, "kind": "claude", "name": name,
                               "cols": 100, "rows": 30 });
        if let Some(by) = opened_by {
            body["opened_by"] = json!(by.id);
        }
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        let session = self.post("/api/sessions", body).await;
        let agent = self.agent(&session);
        std::fs::write(&agent.transcript, "").unwrap();
        agent.env(None).await;
        agent
    }

    /// The stand-in of session `view`.
    fn agent(&self, view: &Value) -> Agent {
        assert!(view["id"].is_string(), "not a session: {view}");
        let id = view["id"].as_str().unwrap().to_owned();
        let conversation = view["agent_session_id"].as_str().unwrap().to_owned();
        let project = self
            .root
            .join("claude-config/projects")
            .join(claude_project_dir_name(&self.root.join("repo")));
        std::fs::create_dir_all(&project).unwrap();
        Agent {
            name: view["name"].as_str().unwrap().to_owned(),
            id,
            log: self.root.join("log"),
            transcript: project.join(format!("{conversation}.jsonl")),
            hub: format!("http://{}", self.addr),
            http: self.http.clone(),
        }
    }

    async fn policy(&self, agent: &Agent, policy: &str) {
        let (status, view) = self
            .call(
                reqwest::Method::PUT,
                &format!("/api/sessions/{}/start-policy", agent.id),
                Some(json!({ "policy": policy })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{view}");
    }

    /// Wait until session `sid` ended; its view.
    async fn ended(&self, sid: &str) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let view = self.session(sid).await;
            if view["state"] == "exited" && !view["ended"].is_null() {
                assert!(view["ending"].is_null(), "{view}");
                return view;
            }
            assert!(Instant::now() < deadline, "{sid} never ended: {view}");
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// The telemetry event kinds recorded for session `sid`.
    async fn kinds(&self, sid: &str) -> Vec<String> {
        self.get(&format!("/api/telemetry/sessions/{sid}/events?limit=500"))
            .await["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Wait until session `agent` was told `needle`: typed in, or queued
    /// for when its turn is done.
    async fn told(&self, agent: &Agent, needle: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let queued = self.queue(&agent.id).await.to_string();
            if agent.typed().contains(needle) || queued.contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{} was never told {needle:?}: {} {queued}",
                agent.name,
                agent.typed()
            );
            sleep(Duration::from_millis(50)).await;
        }
    }
}

/// One stand-in agent session.
struct Agent {
    name: String,
    id: String,
    log: PathBuf,
    transcript: PathBuf,
    hub: String,
    http: reqwest::Client,
}

impl Agent {
    fn path(&self, ext: &str) -> PathBuf {
        self.log.join(format!("claude-{}.{ext}", self.id))
    }

    /// The current spawn's environment (a different secret than
    /// `previous`: a restart's).
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
            assert!(Instant::now() < deadline, "{} never started", self.name);
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn post_as(&self, path: &str, body: &Value) -> reqwest::Response {
        let env = self.env(None).await;
        self.http
            .post(format!("{}{path}", self.hub))
            .header(SESSION_HEADER, &self.id)
            .header(SECRET_HEADER, &env[report::SECRET_ENV])
            .json(body)
            .send()
            .await
            .unwrap()
    }

    /// POST a Claude Code hook as this session.
    async fn hook(&self, event: &str, mut payload: Value) {
        payload["hook_event_name"] = json!(event);
        let response = self.post_as("/api/telemetry/hook", &payload).await;
        assert_eq!(response.status(), StatusCode::OK, "{event}");
    }

    /// A finished turn: a prompt taken (working), then done.
    async fn turn(&self) {
        self.hook("UserPromptSubmit", json!({"prompt": "go"})).await;
        sleep(Duration::from_millis(50)).await;
        self.hook("Stop", json!({})).await;
    }

    /// Call one of the hub's agent tools: whether it failed, and its text.
    async fn tool(&self, name: &str, arguments: Value) -> (bool, String) {
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": name, "arguments": arguments}});
        let answer: Value = self
            .post_as("/api/telemetry/mcp", &message)
            .await
            .json()
            .await
            .unwrap();
        let result = &answer["result"];
        (
            result["isError"] == true,
            result["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
        )
    }

    /// Call a tool that must succeed; its text.
    async fn ok(&self, name: &str, arguments: Value) -> String {
        let (error, said) = self.tool(name, arguments).await;
        assert!(!error, "{} {name}: {said}", self.name);
        said
    }

    fn typed(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.path("in")).unwrap_or_default()).into_owned()
    }

    async fn typed_has(&self, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.typed().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "{} never got {needle:?}: {}",
                self.name,
                self.typed()
            );
            sleep(Duration::from_millis(25)).await;
        }
    }
}

/// The phase of session `sid`'s end in progress.
async fn phase_is(hub: &TestHub, sid: &str, want: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let view = hub.session(sid).await;
        if view["ending"]["phase"] == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{sid} never reached {want}: {view}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

/// The last line of each item waiting in a queue view.
fn waiting(view: &Value) -> Vec<String> {
    view["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["state"] != "delivered")
        .map(|i| {
            i["text"]
                .as_str()
                .unwrap()
                .lines()
                .last()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_ends_its_sessions_gracefully_and_never_mid_flight() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    // Its agent left a background job running (Claude's Bash tool: a
    // session of its own, outside the terminal's).
    std::fs::write(hub.root.join("log/spawn-next"), "").unwrap();
    let idle = hub.claude("idle", Some(&lead), json!({})).await;
    let job: u32 = std::fs::read_to_string(idle.path("bg"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = |pid: u32| {
        Path::new(&format!("/proc/{pid}")).exists() && {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            !stat.contains(") Z ")
        }
    };
    assert!(alive(job));
    let busy = hub.claude("busy", Some(&lead), json!({})).await;
    let other = hub.claude("other", Some(&lead), json!({})).await;
    for agent in [&lead, &idle, &busy, &other] {
        agent.turn().await;
    }

    // Nobody ends itself; force is the operator's; graceful needs its
    // final prompt.
    let (error, said) = lead
        .tool("end_session", json!({"to": lead.id, "final_prompt": "x"}))
        .await;
    assert!(error && said.contains("your own session"), "{said}");
    let (error, said) = lead
        .tool("end_session", json!({"to": "idle", "mode": "force"}))
        .await;
    assert!(error, "{said}");
    assert_eq!(
        said,
        "Not ended: force is operator only (pane menu or fw hub kill)"
    );
    let (error, said) = lead.tool("end_session", json!({"to": "idle"})).await;
    assert!(error && said.contains("needs final_prompt"), "{said}");

    // At rest: its final prompt goes now, and once that turn is over it
    // ends.
    let said = lead
        .ok(
            "end_session",
            json!({"to": "idle", "final_prompt": "Commit and stop."}),
        )
        .await;
    assert!(said.starts_with("Ending idle (id "), "{said}");
    assert!(said.contains("scheduled, request "), "{said}");
    assert!(said.contains("What I can see would be lost: "), "{said}");
    assert!(
        said.contains(&format!(
            "{job} sh -c sleep 300; true /u/.claude/shell-snapshots/t.sh"
        )),
        "the job outside its terminal's session is named: {said}"
    );
    idle.typed_has("This is your final prompt").await;
    idle.typed_has("Commit and stop.").await;
    assert!(
        idle.typed().contains("From lead in repo"),
        "{}",
        idle.typed()
    );
    phase_is(&hub, &idle.id, "final_turn").await;
    // While it ends it takes no more messages.
    let (error, said) = other
        .tool("message_session", json!({"to": "idle", "text": "hi"}))
        .await;
    assert!(error && said.contains("is ending"), "{said}");
    idle.turn().await;
    let view = hub.ended(&idle.id).await;
    assert_eq!(
        view["ended"]["by"],
        json!({"kind": "session", "id": lead.id})
    );
    assert_eq!(view["ended"]["mode"], "graceful");
    hub.told(&lead, &format!("idle (id {}) ended", idle.id))
        .await;
    // The job went with it.
    let deadline = Instant::now() + WAIT;
    while alive(job) {
        assert!(
            Instant::now() < deadline,
            "the background job outlived its end"
        );
        sleep(Duration::from_millis(100)).await;
    }
    let kinds = hub.kinds(&idle.id).await;
    for kind in ["session.end_requested", "session.ended"] {
        assert!(kinds.iter().any(|k| k == kind), "{kinds:?}");
    }
    assert!(
        hub.kinds(&lead.id)
            .await
            .iter()
            .any(|k| k == "session.ended"),
        "the asker's log has it too"
    );

    // Working: its queue is set aside (its other sender told), and it
    // ends only once its turn is over and no tool call or subagent runs.
    busy.hook("UserPromptSubmit", json!({"prompt": "work"}))
        .await;
    let said = other
        .ok(
            "message_session",
            json!({"to": "busy", "text": "from other"}),
        )
        .await;
    assert!(said.starts_with("Queued for busy"), "{said}");
    lead.ok(
        "message_session",
        json!({"to": "busy", "text": "from lead"}),
    )
    .await;
    let said = lead
        .ok(
            "end_session",
            json!({"to": "busy", "final_prompt": "Wind down.", "wait_minutes": 30}),
        )
        .await;
    assert!(said.contains("Set aside 2 messages"), "{said}");
    assert!(said.contains("from other: from other"), "{said}");
    assert!(said.contains("from you: from lead"), "{said}");
    hub.told(&other, "was set aside: busy is ending").await;
    assert_eq!(waiting(&hub.queue(&busy.id).await), ["Wind down."]);
    sleep(WOULD_HAVE).await;
    assert!(
        !busy.typed().contains("Wind down."),
        "a final prompt never interrupts a turn"
    );
    busy.hook("Stop", json!({})).await;
    busy.typed_has("Wind down.").await;
    busy.hook("UserPromptSubmit", json!({"prompt": "Wind down."}))
        .await;
    busy.hook(
        "SubagentStart",
        json!({"agent_id": "sub1", "agent_type": "general-purpose"}),
    )
    .await;
    busy.hook(
        "PreToolUse",
        json!({"agent_id": "sub1", "tool_name": "Bash", "tool_use_id": "t1",
               "tool_input": {"command": "sleep 100"}}),
    )
    .await;
    busy.hook("Stop", json!({})).await;
    phase_is(&hub, &busy.id, "settling").await;
    sleep(WOULD_HAVE).await;
    assert_eq!(
        hub.session(&busy.id).await["state"],
        "running",
        "an open tool call keeps it"
    );
    busy.hook(
        "PostToolUse",
        json!({"agent_id": "sub1", "tool_name": "Bash", "tool_use_id": "t1"}),
    )
    .await;
    sleep(WOULD_HAVE).await;
    assert_eq!(
        hub.session(&busy.id).await["state"],
        "running",
        "a running subagent keeps it"
    );
    // A background subagent's end wakes its agent for a turn: still not
    // at rest until that turn is over.
    busy.hook("SubagentStop", json!({"agent_id": "sub1"})).await;
    sleep(WOULD_HAVE).await;
    assert_eq!(hub.session(&busy.id).await["state"], "running");
    busy.hook(
        "UserPromptSubmit",
        json!({"prompt": "<task-notification>\n<task-id>sub1</task-id>\n</task-notification>"}),
    )
    .await;
    busy.hook("Stop", json!({})).await;
    let view = hub.ended(&busy.id).await;
    assert_eq!(view["ended"]["by"]["id"], lead.id.as_str());
    // Ended: the set-aside messages went with it.
    assert!(waiting(&hub.queue(&busy.id).await).is_empty());

    // The operator's kill is recorded as theirs.
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/kill", other.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let view = hub.ended(&other.id).await;
    assert_eq!(view["ended"]["by"], json!({"kind": "operator"}));
    assert_eq!(view["ended"]["mode"], "force");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn senders_see_and_cancel_only_their_own_messages_and_priority_goes_first() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let other = hub.claude("other", Some(&lead), json!({})).await;
    let worker = hub.claude("worker", Some(&lead), json!({})).await;
    for agent in [&lead, &other, &worker] {
        agent.turn().await;
    }
    worker
        .hook("UserPromptSubmit", json!({"prompt": "work"}))
        .await;
    lead.ok("message_session", json!({"to": "worker", "text": "L1"}))
        .await;
    other
        .ok("message_session", json!({"to": "worker", "text": "O1"}))
        .await;
    let said = lead
        .ok(
            "message_session",
            json!({"to": "worker", "text": "LP1", "priority": true}),
        )
        .await;
    assert!(said.contains("as priority"), "{said}");
    assert!(said.contains("#1 in its queue"), "{said}");
    let said = other
        .ok(
            "message_session",
            json!({"to": "worker", "text": "OP1", "priority": true}),
        )
        .await;
    assert!(said.contains("#2 in its queue"), "{said}");
    lead.ok("message_session", json!({"to": "worker", "text": "L2"}))
        .await;
    let view = hub.queue(&worker.id).await;
    assert_eq!(waiting(&view), ["LP1", "OP1", "L1", "O1", "L2"]);
    let id_of = |text: &str| -> String {
        view["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["text"].as_str().unwrap().ends_with(text))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        view["items"][0]["from"],
        lead.id.as_str(),
        "the sender is recorded"
    );

    // Each sees only its own.
    let mine = lead.ok("queued_messages", json!({"to": "worker"})).await;
    assert!(
        mine.starts_with("3 messages of yours wait for worker"),
        "{mine}"
    );
    assert!(mine.contains("5 messages queued in all"), "{mine}");
    assert!(
        mine.contains(&format!("{} #1 priority, sent", id_of("LP1"))),
        "{mine}"
    );
    assert!(mine.contains(&format!("{} #3 sent", id_of("L1"))), "{mine}");
    assert!(!mine.contains("O1"), "{mine}");
    let theirs = other.ok("queued_messages", json!({"to": "worker"})).await;
    assert!(theirs.starts_with("2 messages of yours wait"), "{theirs}");

    // Each cancels only its own.
    let (error, said) = other
        .tool(
            "cancel_message",
            json!({"to": "worker", "item": id_of("L1")}),
        )
        .await;
    assert!(error && said.contains("not found"), "{said}");
    let said = lead
        .ok(
            "cancel_message",
            json!({"to": "worker", "item": &id_of("L1")[..8]}),
        )
        .await;
    assert!(said.starts_with("Cancelled 1 message of yours"), "{said}");
    let said = lead.ok("cancel_message", json!({"to": "worker"})).await;
    assert!(said.starts_with("Cancelled 2 messages of yours"), "{said}");
    assert!(said.contains("2 messages still queued"), "{said}");
    assert_eq!(waiting(&hub.queue(&worker.id).await), ["OP1", "O1"]);
    assert!(
        hub.kinds(&worker.id)
            .await
            .iter()
            .any(|k| k == "queue.cancelled")
    );

    // The priority message goes first, at the turn boundary.
    sleep(WOULD_HAVE).await;
    assert!(!worker.typed().contains("OP1"), "never mid-turn");
    worker.hook("Stop", json!({})).await;
    worker.typed_has("OP1").await;
    // The rest wait for the next turn.
    let deadline = Instant::now() + WAIT;
    while waiting(&hub.queue(&worker.id).await) != ["O1"] {
        assert!(Instant::now() < deadline, "{}", hub.queue(&worker.id).await);
        sleep(Duration::from_millis(50)).await;
    }
    sleep(WOULD_HAVE).await;
    assert_eq!(waiting(&hub.queue(&worker.id).await), ["O1"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ownership_passes_on_and_everything_else_asks_the_operator() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let worker = hub.claude("worker", Some(&lead), json!({})).await;
    let stranger = hub.claude("stranger", None, json!({})).await;
    for agent in [&lead, &worker, &stranger] {
        agent.turn().await;
    }

    // Not its own: kept for the operator; declined, the asker is told.
    let said = stranger
        .ok(
            "end_session",
            json!({"to": "worker", "final_prompt": "Stop."}),
        )
        .await;
    assert!(
        said.starts_with("Waiting for the operator's approval (end request "),
        "{said}"
    );
    assert!(said.contains("its owner is lead"), "{said}");
    let asked = hub.session(&stranger.id).await["end_requests"].clone();
    assert_eq!(asked[0]["why"], "not_owner", "{asked}");
    assert_eq!(asked[0]["target"], worker.id.as_str());
    let request = asked[0]["id"].as_str().unwrap().to_owned();
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}/ends/{request}", stranger.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    hub.told(&stranger, &format!("declined your end request {request}"))
        .await;
    assert!(hub.session(&worker.id).await["ending"].is_null());

    // An operator-started session: only ever with approval. Approved, it
    // ends as asked, by the asker.
    let said = lead
        .ok("end_session", json!({"to": "stranger", "mode": "at_rest"}))
        .await;
    assert!(said.contains("the operator started it"), "{said}");
    let request = hub.session(&lead.id).await["end_requests"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, view) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/ends/{request}", lead.id),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["ending"]["mode"], "at_rest");
    hub.told(&lead, &format!("approved your end request {request}"))
        .await;
    let view = hub.ended(&stranger.id).await;
    assert_eq!(view["ended"]["by"]["id"], lead.id.as_str());
    assert!(
        hub.session(&lead.id).await["end_requests"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );

    // The owner's handoff successor inherits once the owner has ended.
    let next = hub
        .post(
            &format!("/api/sessions/{}/continue", lead.id),
            json!({"prompt": "Goal: carry on.", "name": "lead 2"}),
        )
        .await;
    let lead2 = hub.agent(&next);
    lead2.env(None).await;
    hub.policy(&lead2, "allowed").await;
    lead2.turn().await;
    let said = lead2
        .ok("end_session", json!({"to": "worker", "mode": "at_rest"}))
        .await;
    assert!(
        said.starts_with("Waiting for the operator"),
        "lead still runs: {said}"
    );
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/kill", lead.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    hub.ended(&lead.id).await;

    // Given away: the new owner may, the old one no longer may.
    let helper = hub.claude("helper", Some(&lead2), json!({})).await;
    helper.turn().await;
    let (error, said) = helper
        .tool("give_session", json!({"to": "worker", "owner": "helper"}))
        .await;
    assert!(error && said.contains("only its owner gives"), "{said}");
    let said = lead2
        .ok("give_session", json!({"to": "worker", "owner": "helper"}))
        .await;
    assert!(said.contains("is now helper's"), "{said}");
    hub.told(&helper, "gave you control of worker").await;
    assert_eq!(hub.session(&worker.id).await["owner"], helper.id.as_str());
    assert!(
        hub.kinds(&worker.id)
            .await
            .iter()
            .any(|k| k == "session.owner_changed")
    );
    let said = lead2
        .ok("end_session", json!({"to": "worker", "mode": "at_rest"}))
        .await;
    assert!(said.starts_with("Waiting for the operator"), "{said}");
    let said = helper
        .ok("end_session", json!({"to": "worker", "mode": "at_rest"}))
        .await;
    assert!(said.starts_with("Ending worker (id "), "{said}");
    let view = hub.ended(&worker.id).await;
    assert_eq!(view["ended"]["by"]["id"], helper.id.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_starts_its_successor_then_ends_the_old_one() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let old = hub
        .claude(
            "old",
            Some(&lead),
            json!({"model": "opus", "permission_mode": "acceptEdits"}),
        )
        .await;
    lead.turn().await;
    old.turn().await;
    let said = lead
        .ok(
            "replace_session",
            json!({"to": "old", "name": "new", "prompt": "Goal: carry on from old.",
                   "final_prompt": "Commit and write a summary.", "model": "sonnet"}),
        )
        .await;
    assert!(said.starts_with("Replacing old (id "), "{said}");
    old.typed_has("Commit and write a summary.").await;
    assert!(
        hub.named("new").await.is_none(),
        "not before the old one rests"
    );
    old.turn().await;
    let view = hub.ended(&old.id).await;
    let new = hub.named("new").await.expect("the successor started");
    assert_eq!(view["ended"]["successor"], new["id"]);
    assert_eq!(view["ended"]["successor_name"], "new");
    assert_eq!(view["ended"]["by_name"], "lead");
    assert_eq!(new["opened_by"], lead.id.as_str());
    assert_eq!(new["owner"], lead.id.as_str());
    assert_eq!(new["continues"], old.id.as_str());
    assert_eq!(new["launch"]["model"], "sonnet");
    assert_eq!(new["launch"]["permission_mode"], "acceptEdits");
    assert_eq!(new["cwd"], view["cwd"]);
    let successor = hub.agent(&new);
    successor.env(None).await;
    let status = lead.ok("session_status", json!({"to": "new"})).await;
    assert!(
        status.contains("\nmodel: sonnet (as launched; not reported yet)\n"),
        "{status}"
    );
    successor
        .hook("SessionStart", json!({"model": "claude-sonnet-5-5"}))
        .await;
    let status = lead.ok("session_status", json!({"to": "new"})).await;
    assert!(status.contains("\nmodel: claude-sonnet-5-5\n"), "{status}");
    let argv = std::fs::read_to_string(successor.path("argv")).unwrap();
    assert!(argv.contains("Goal: carry on from old."), "{argv:?}");
    assert!(argv.contains("You were started by lead"), "{argv:?}");
    hub.told(&lead, "old (id ").await;
    hub.told(&lead, "is replaced by new").await;
    let kinds = hub.kinds(&old.id).await;
    assert!(kinds.iter().any(|k| k == "session.replaced"), "{kinds:?}");
    let snap = hub
        .get(&format!("/api/telemetry/sessions/{}", old.id))
        .await;
    assert_eq!(snap["session"]["continued_by"], new["id"]);

    // Under an ask start policy the whole replacement is one approval.
    hub.policy(&lead, "ask").await;
    successor.turn().await;
    let said = lead
        .ok(
            "replace_session",
            json!({"to": "new", "name": "newer", "prompt": "Goal: more.",
                   "final_prompt": "Stop."}),
        )
        .await;
    assert!(
        said.starts_with("Waiting for the operator's approval (replace request "),
        "{said}"
    );
    assert!(said.contains("start-sessions setting asks"), "{said}");
    assert!(
        said.contains("If it goes ahead, what I can see would be lost: "),
        "{said}"
    );
    let asked = hub.session(&lead.id).await["end_requests"][0].clone();
    assert_eq!(asked["why"], "start_policy_ask");
    assert_eq!(asked["replace"]["name"], "newer");
    hub.post(
        &format!(
            "/api/sessions/{}/ends/{}",
            lead.id,
            asked["id"].as_str().unwrap()
        ),
        json!({}),
    )
    .await;
    hub.told(&lead, "approved your replace request").await;
    successor.typed_has("Stop.").await;
    successor.turn().await;
    let view = hub.ended(&successor.id).await;
    let newer = hub.named("newer").await.expect("started once approved");
    assert_eq!(view["ended"]["successor"], newer["id"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_that_does_not_start_leaves_the_old_one_running() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let other = hub.claude("other", Some(&lead), json!({})).await;
    let old = hub.claude("old", Some(&lead), json!({})).await;
    for agent in [&lead, &other] {
        agent.turn().await;
    }
    old.hook("UserPromptSubmit", json!({"prompt": "work"}))
        .await;
    other
        .ok("message_session", json!({"to": "old", "text": "keep me"}))
        .await;
    lead.ok(
        "replace_session",
        json!({"to": "old", "name": "new", "prompt": "Goal: x", "final_prompt": "Stop."}),
    )
    .await;
    assert_eq!(waiting(&hub.queue(&old.id).await), ["Stop."]);
    // Its start becomes impossible meanwhile.
    let made = hub.session(&lead.id).await["starts_made"].as_u64().unwrap();
    let mut tunables = hub.hub.tunables();
    tunables.agent_starts.max_total = usize::try_from(made).unwrap();
    hub.hub.set_tunables(tunables);
    old.hook("Stop", json!({})).await;
    old.typed_has("Stop.").await;
    old.turn().await;
    hub.told(&lead, "its successor did not start").await;
    hub.told(&lead, "new was not started").await;
    let view = hub.session(&old.id).await;
    assert_eq!(view["state"], "running");
    assert!(view["ending"].is_null(), "{view}");
    assert!(view["ended"].is_null(), "{view}");
    assert!(hub.named("new").await.is_none());
    assert_eq!(waiting(&hub.queue(&old.id).await), ["keep me"]);
    assert!(
        hub.kinds(&old.id)
            .await
            .iter()
            .any(|k| k == "session.end_cancelled")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_end_survives_a_restart_and_one_that_never_settles_is_called_off() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let old = hub.claude("old", Some(&lead), json!({})).await;
    lead.turn().await;
    old.hook("UserPromptSubmit", json!({"prompt": "long work"}))
        .await;
    lead.ok(
        "message_session",
        json!({"to": "old", "text": "queued before"}),
    )
    .await;
    lead.ok(
        "replace_session",
        json!({"to": "old", "name": "new", "prompt": "Goal: x",
               "final_prompt": "Stop.", "wait_minutes": 60}),
    )
    .await;
    let request = hub.session(&old.id).await["ending"]["id"].clone();
    let before = old.env(None).await[report::SECRET_ENV].clone();
    // A restart (a live upgrade's too) carries the end on; its deadline
    // is brought near here so the test does not wait an hour.
    let soon = (chrono::Utc::now() + chrono::Duration::seconds(8)).to_rfc3339();
    let old_id = old.id.clone();
    let hub = hub
        .restart(move |doc| {
            let session = doc["sessions"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|s| s["id"] == old_id.as_str())
                .unwrap();
            assert_eq!(session["ending"]["phase"], "await_final", "{session}");
            session["ending"]["deadline"] = json!(soon);
        })
        .await;
    old.env(Some(&before)).await;
    let view = hub.session(&old.id).await;
    assert_eq!(view["ending"]["id"], request, "{view}");
    // It never comes back to rest: the end is called off at the deadline,
    // the successor never starts, and the set-aside message is back.
    let deadline = Instant::now() + WAIT;
    loop {
        let view = hub.session(&old.id).await;
        if view["ending"].is_null() {
            assert_eq!(view["state"], "running", "{view}");
            assert!(view["ended"].is_null(), "{view}");
            break;
        }
        assert!(Instant::now() < deadline, "never called off: {view}");
        sleep(Duration::from_millis(100)).await;
    }
    assert!(hub.named("new").await.is_none());
    assert_eq!(waiting(&hub.queue(&old.id).await), ["queued before"]);
    hub.told(
        &lead,
        "was called off: it was still waiting for its final prompt to be delivered",
    )
    .await;
    hub.told(&lead, "new was not started").await;
}

/// Wait until session `agent` runs a new process (a relaunch; the old
/// secret was `previous`): its environment.
async fn respawned(hub: &TestHub, agent: &Agent, previous: &str) -> HashMap<String, String> {
    let env = agent.env(Some(previous)).await;
    // The hub records it running (and no longer stale) once spawned.
    let deadline = Instant::now() + WAIT;
    loop {
        let view = hub.session(&agent.id).await;
        if view["state"] == "running" && view["stale"].is_null() {
            return env;
        }
        assert!(Instant::now() < deadline, "never running again: {view}");
        sleep(Duration::from_millis(25)).await;
    }
}

/// The argv of session `agent`'s current process.
fn argv(agent: &Agent) -> Vec<String> {
    String::from_utf8_lossy(&std::fs::read(agent.path("argv")).unwrap())
        .split('\0')
        .filter(|a| !a.is_empty())
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_relaunches_agents_at_rest_and_never_interrupts_busy_ones() {
    let mut hub = TestHub::start().await;
    let idle = hub.claude("idle", None, json!({})).await;
    let busy = hub.claude("busy", None, json!({})).await;
    let kept = hub.claude("kept", None, json!({})).await;
    let fresh = hub.claude("fresh", None, json!({})).await;
    // `fresh` has no conversation to resume yet.
    std::fs::remove_file(&fresh.transcript).unwrap();
    idle.turn().await;
    busy.hook("UserPromptSubmit", json!({"prompt": "long job"}))
        .await;
    kept.turn().await;
    hub.post(
        &format!("/api/sessions/{}/relaunch", kept.id),
        json!({ "never": true }),
    )
    .await;
    let secret = |a: &Agent| {
        let path = a.path("env");
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .find_map(|l| {
                l.strip_prefix(&format!("{}=", report::SECRET_ENV))
                    .map(str::to_owned)
            })
            .unwrap()
    };
    let (idle_secret, busy_secret, kept_secret, fresh_secret) =
        (secret(&idle), secret(&busy), secret(&kept), secret(&fresh));
    // A prompt waits in the busy one's queue for its turn to end.
    hub.post(
        &format!("/api/sessions/{}/queue", busy.id),
        json!({ "text": "AFTER-UPGRADE" }),
    )
    .await;

    let (marked, _) = hub.hub.mark_stale("0.0.1");
    assert_eq!(marked, 4);

    // At rest: relaunched at once, resuming its conversation in place.
    respawned(&hub, &idle, &idle_secret).await;
    let args = argv(&idle);
    let conversation = hub.session(&idle.id).await["agent_session_id"].clone();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--resume" && json!(w[1]) == conversation),
        "{args:?}"
    );
    let view = hub.session(&idle.id).await;
    assert_eq!(view["state"], "running", "{view}");
    assert!(view["stale"].is_null(), "{view}");
    // The timeline says so (recorded once the new process is up).
    let deadline = Instant::now() + WAIT;
    while !hub
        .kinds(&idle.id)
        .await
        .iter()
        .any(|k| k == "session.relaunched")
    {
        assert!(Instant::now() < deadline, "no session.relaunched event");
        sleep(Duration::from_millis(50)).await;
    }
    idle.hook("SessionStart", json!({})).await;

    // Busy: never interrupted, its prompt still queued.
    sleep(WOULD_HAVE).await;
    assert_eq!(secret(&busy), busy_secret);
    let view = hub.session(&busy.id).await;
    assert!(view["stale"].is_object(), "{view}");
    assert!(view["stale"]["held"].is_null(), "{view}");
    // It says why it waits.
    assert_eq!(view["stale"]["waits"], "working", "{view}");
    assert!(!busy.typed().contains("AFTER-UPGRADE"));
    // Opted out, nothing to resume: they keep the note, never relaunched.
    assert_eq!(secret(&kept), kept_secret);
    assert_eq!(secret(&fresh), fresh_secret);
    let view = hub.session(&kept.id).await;
    assert!(
        view["stale"]["held"]
            .as_str()
            .unwrap()
            .contains("opted out"),
        "{view}"
    );
    let view = hub.session(&fresh.id).await;
    assert!(
        view["stale"]["held"]
            .as_str()
            .unwrap()
            .contains("cannot be resumed"),
        "{view}"
    );

    // Its turn ends: relaunched before the queue delivers, so the prompt
    // reaches the new process once it reports.
    busy.hook("Stop", json!({})).await;
    respawned(&hub, &busy, &busy_secret).await;
    let typed_before = busy.typed();
    assert!(!typed_before.contains("AFTER-UPGRADE"), "{typed_before}");
    let view = hub.session(&busy.id).await;
    assert!(view["stale"].is_null(), "{view}");
    busy.hook("SessionStart", json!({})).await;
    busy.typed_has("AFTER-UPGRADE").await;
    assert!(
        !busy.typed()[..typed_before.len()].contains("AFTER-UPGRADE"),
        "delivered into the old process"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_asking_the_operator_is_relaunched_only_once_answered() {
    let mut hub = TestHub::start().await;
    let asker = hub.claude("asker", None, json!({})).await;
    asker
        .hook("UserPromptSubmit", json!({"prompt": "deploy"}))
        .await;
    let ask = json!({"tool_name": "mcp__flywheel__ask_operator", "tool_use_id": "tu1",
                     "tool_input": {"kind": "answer", "text": "Which region?"}});
    asker.hook("PreToolUse", ask.clone()).await;
    asker.hook("PostToolUse", ask).await;
    asker
        .hook("Stop", json!({"last_assistant_message": "Which region?"}))
        .await;
    let before = asker.env(None).await[report::SECRET_ENV].clone();
    hub.hub.mark_stale("0.0.1");
    sleep(WOULD_HAVE).await;
    assert_eq!(asker.env(None).await[report::SECRET_ENV], before);
    let view = hub.session(&asker.id).await;
    assert_eq!(view["agent"]["state"], "asking", "{view}");

    // The answer reaches the old process (never held back), its turn runs,
    // and only then is it relaunched.
    hub.post(
        &format!("/api/sessions/{}/input", asker.id),
        json!({ "data": "eu-west\r" }),
    )
    .await;
    asker.typed_has("eu-west").await;
    asker
        .hook("UserPromptSubmit", json!({"prompt": "eu-west"}))
        .await;
    sleep(WOULD_HAVE).await;
    assert_eq!(asker.env(None).await[report::SECRET_ENV], before);
    asker.hook("Stop", json!({})).await;
    respawned(&hub, &asker, &before).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagents_cut_short_by_an_exit_do_not_hold_a_relaunch() {
    let mut hub = TestHub::start().await;
    let agent = hub.claude("leaked", None, json!({})).await;
    // A turn ends with a subagent still running (background) ...
    agent
        .hook("UserPromptSubmit", json!({"prompt": "go"}))
        .await;
    agent
        .hook(
            "SubagentStart",
            json!({"agent_id": "sub1", "agent_type": "general-purpose"}),
        )
        .await;
    agent.hook("Stop", json!({})).await;
    // ... and the operator relaunches it by hand: the exit cuts the
    // subagent short, after the last turn ended. It wakes nothing.
    let first = agent.env(None).await[report::SECRET_ENV].clone();
    hub.hub.kill_session(&agent.id).unwrap();
    let deadline = Instant::now() + WAIT;
    while hub.session(&agent.id).await["state"] != "exited" {
        assert!(Instant::now() < deadline, "never exited");
        sleep(Duration::from_millis(25)).await;
    }
    hub.post(&format!("/api/sessions/{}/relaunch", agent.id), json!({}))
        .await;
    let second = agent.env(Some(&first)).await[report::SECRET_ENV].clone();
    agent.hook("SessionStart", json!({})).await;

    hub.hub.mark_stale("0.0.1");
    respawned(&hub, &agent, &second).await;
}

/// The hold ids of the group of `kind` and `cause` in a `/api/holds`
/// listing, waiting until it has `n`.
async fn group_of(hub: &TestHub, kind: &str, cause: &str, n: usize) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let listed = hub.get("/api/holds").await;
        let found = listed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["kind"] == kind && g["cause"] == cause)
            .cloned();
        if let Some(group) = found.filter(|g| g["holds"].as_array().unwrap().len() == n) {
            return group;
        }
        assert!(
            Instant::now() < deadline,
            "never {n} {kind} holds for {cause:?}: {listed}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

async fn decisions(hub: &TestHub, query: &str) -> Vec<Value> {
    hub.get(&format!("/api/decisions?{query}")).await["decisions"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn like_holds_are_answered_as_one_and_every_answer_is_kept() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    lead.turn().await;

    // Three start requests from one lead: one group, answered at once
    // with one left to its own answer.
    for n in 1..=3 {
        let said = lead
            .ok(
                "start_session",
                json!({"name": format!("w{n}"), "prompt": format!("task {n}")}),
            )
            .await;
        assert!(said.contains("operator"), "{said}");
    }
    let group = group_of(&hub, "awaiting_approval", "start_session from lead", 3).await;
    let ids: Vec<String> = group["holds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(group["holds"][0]["answers"], json!(["approve", "decline"]));
    let answered = hub
        .post(
            "/api/holds/answer",
            json!({"holds": ids, "answer": "approve", "overrides": {ids[2].clone(): "decline"}}),
        )
        .await;
    let batch = answered["batch"].as_str().expect("a batch").to_owned();
    assert!(
        answered["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["ok"] == true),
        "{answered}"
    );
    assert!(hub.named("w1").await.is_some() && hub.named("w2").await.is_some());
    assert!(hub.named("w3").await.is_none());
    hub.told(&lead, "declined your start request").await;
    let kept = decisions(&hub, "kind=awaiting_approval").await;
    assert_eq!(kept.len(), 3, "{kept:?}");
    let mut answers: Vec<&str> = kept.iter().map(|d| d["answer"].as_str().unwrap()).collect();
    answers.sort_unstable();
    assert_eq!(answers, ["approved", "approved", "declined"]);
    for d in &kept {
        assert_eq!(d["batch"], batch.as_str(), "{d}");
        assert_eq!(d["by"], "operator");
        assert_eq!(d["session_name"], "lead");
        assert!(
            d["question"]
                .as_str()
                .unwrap()
                .starts_with("lead asks to start"),
            "{d}"
        );
    }
    // Each is on the bus once (the activity journal's row: kind and who,
    // never the question or the answer).
    let deadline = Instant::now() + WAIT;
    loop {
        let rows = hub.get("/api/activity?kind=decision.made").await;
        let rows = rows["rows"].as_array().cloned().unwrap_or_default();
        if rows.len() == 3 {
            let text = serde_json::to_string(&rows).unwrap();
            assert!(
                !text.contains("asks to start") && !text.contains("task 1"),
                "{text}"
            );
            assert!(
                text.contains("awaiting approval hold answered by operator"),
                "{text}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "no decision rows: {rows:?}");
        sleep(Duration::from_millis(50)).await;
    }
    // A hold answered is gone; answering it again says so.
    let (status, again) = hub
        .call(
            reqwest::Method::POST,
            "/api/holds/answer",
            Some(json!({"holds": [ids[0].clone()], "answer": "approve"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["results"][0]["ok"], false, "{again}");

    // Two agents ask the same thing (the agent's kind, cause, and
    // recommendation): one group. One is answered with the group, the
    // other left out and answered in its terminal.
    let a = hub.claude("asker-a", None, json!({})).await;
    let b = hub.claude("asker-b", None, json!({})).await;
    for agent in [&a, &b] {
        agent
            .hook("UserPromptSubmit", json!({"prompt": "deploy"}))
            .await;
        let ask = json!({"tool_name": "mcp__flywheel__ask_operator", "tool_use_id": "tu1",
                         "tool_input": {"kind": "decide", "text": "Which region?",
                                        "options": ["eu-west", "us-east"], "hold": "awaiting_approval",
                                        "cause": "deploy region", "recommendation": "eu-west"}});
        agent.hook("PreToolUse", ask.clone()).await;
        agent.hook("PostToolUse", ask).await;
        agent
            .hook("Stop", json!({"last_assistant_message": "Which region?"}))
            .await;
    }
    let group = group_of(&hub, "awaiting_approval", "deploy region", 2).await;
    assert_eq!(group["holds"][0]["recommendation"], "eu-west");
    let hold_of = |agent: &Agent| {
        group["holds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["session"] == agent.id.as_str())
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (ha, hb) = (hold_of(&a), hold_of(&b));
    let answered = hub
        .post(
            "/api/holds/answer",
            json!({"holds": [ha, hb.clone()], "answer": "eu-west", "overrides": {hb: "skip"}}),
        )
        .await;
    assert!(answered["batch"].is_null(), "one answered: {answered}");
    a.typed_has("eu-west").await;
    assert!(!b.typed().contains("eu-west"));
    // The prompts arrive (the agents' hooks): each answers its ask.
    a.hook("UserPromptSubmit", json!({"prompt": "eu-west"}))
        .await;
    b.hook(
        "UserPromptSubmit",
        json!({"prompt": "us-east, it is cheaper"}),
    )
    .await;
    let kept = decisions(&hub, "kind=awaiting_approval&session=asker-a").await;
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["question"], "Which region?");
    assert_eq!(kept[0]["recommendation"], "eu-west");
    assert_eq!(kept[0]["answer"], "eu-west");
    assert_eq!(kept[0]["cause"], "deploy region");
    let kept = decisions(&hub, &format!("session={}", b.id)).await;
    assert_eq!(kept[0]["answer"], "us-east, it is cheaper");
    assert_eq!(kept[0]["by"], "operator");
    // An agent's signed message that answers is the agent's.
    a.hook("Stop", json!({"last_assistant_message": "Ship it now?"}))
        .await;
    a.hook(
        "UserPromptSubmit",
        json!({"prompt": "From lead in repo: a message from that agent session\n\nyes"}),
    )
    .await;
    let kept = decisions(&hub, "session=asker-a&kind=needs_input").await;
    assert_eq!(kept[0]["by"], "agent:lead", "{kept:?}");
    assert_eq!(kept[0]["question"], "Ship it now?");
    // Newest first, all kinds, and only since a time.
    let all = decisions(&hub, "").await;
    assert_eq!(all.len(), 6);
    let newest = all[0]["at"].as_i64().unwrap();
    assert_eq!(decisions(&hub, &format!("since={newest}")).await.len(), 1);
    let (status, _) = hub
        .call(reqwest::Method::GET, "/api/decisions?kind=bogus", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
