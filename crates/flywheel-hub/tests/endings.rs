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
        options.lease_block_secs = 1;
        // A lease with no history is stuck after 3 s while someone waits.
        options.lease_stuck = (Duration::from_secs(1), Duration::from_secs(3));
        options.lease_stuck_look_ms = 200;
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
    // A topic keeps it its own item (untopical, it would join L1).
    lead.ok(
        "message_session",
        json!({"to": "worker", "text": "L2", "topic": "later"}),
    )
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
async fn a_newer_message_on_a_topic_supersedes_the_older_one() {
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
    lead.ok(
        "message_session",
        json!({"to": "worker", "text": "stop the capture", "topic": "ruling:capture"}),
    )
    .await;
    other
        .ok(
            "message_session",
            json!({"to": "worker", "text": "O1", "topic": "ruling:capture"}),
        )
        .await;
    lead.ok("message_session", json!({"to": "worker", "text": "L2"}))
        .await;
    let first = hub.queue(&worker.id).await["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Unread: replaced in place, same id and place; another sender's
    // message on the same topic is its own.
    let said = lead
        .ok(
            "message_session",
            json!({"to": "worker", "text": "keep capturing until 12:00", "topic": "ruling:capture"}),
        )
        .await;
    assert!(said.starts_with("Replaced your unread message"), "{said}");
    assert!(said.contains("still #1 in its queue"), "{said}");
    let view = hub.queue(&worker.id).await;
    assert_eq!(
        waiting(&view),
        ["keep capturing until 12:00", "O1", "L2"],
        "{view}"
    );
    assert_eq!(view["items"][0]["id"], first.as_str());
    assert_eq!(view["items"][0]["topic"], "ruling:capture");
    assert!(
        !view.to_string().contains("stop the capture"),
        "the old text is gone"
    );
    log_has(&hub, "message", &["message.superseded"]).await;
    let log = hub
        .get(&format!(
            "/api/log?filter=kind%3Dmessage.superseded%20session%3D{}",
            worker.id
        ))
        .await;
    let event = &log["events"][0];
    assert_eq!(event["body"]["from"], lead.id.as_str(), "{log}");
    assert_eq!(event["body"]["topic"], "ruling:capture");
    assert_eq!(event["body"]["replaced"], true);
    // The sender's view lists every target with topics.
    let mine = lead.ok("queued_messages", json!({})).await;
    assert!(mine.starts_with("2 messages of yours wait"), "{mine}");
    assert!(mine.contains("worker (id"), "{mine}");
    assert!(mine.contains("topic ruling:capture, sent"), "{mine}");

    // Read: the next one on the topic says it supersedes it.
    worker.hook("Stop", json!({})).await;
    worker.typed_has("keep capturing until 12:00").await;
    worker
        .hook("UserPromptSubmit", json!({"prompt": "go"}))
        .await;
    let said = lead
        .ok(
            "message_session",
            json!({"to": "worker", "text": "capture done: stop", "topic": "ruling:capture"}),
        )
        .await;
    assert!(said.contains("this one says it supersedes it"), "{said}");
    let view = hub.queue(&worker.id).await;
    let text = view["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["text"].as_str().unwrap().ends_with("capture done: stop"))
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("From lead"), "{text}");
    assert!(lines[2].starts_with("superseded: ruling:capture"), "{text}");

    // A bad topic is refused; no topic is today's message.
    let (error, said) = lead
        .tool(
            "message_session",
            json!({"to": "worker", "text": "x", "topic": "has space"}),
        )
        .await;
    assert!(error && said.contains("topic"), "{said}");

    // The operator's topics are its own, replaced the same way.
    let path = format!("/api/sessions/{}/queue", worker.id);
    for text in ["op first", "op second"] {
        let (status, _) = hub
            .call(
                reqwest::Method::POST,
                &path,
                Some(json!({"text": text, "topic": "ruling:capture"})),
            )
            .await;
        assert!(status.is_success());
    }
    let waiting = waiting(&hub.queue(&worker.id).await);
    assert!(waiting.iter().any(|t| t == "op second"), "{waiting:?}");
    assert!(!waiting.iter().any(|t| t == "op first"), "{waiting:?}");
    assert!(
        waiting.iter().any(|t| t.ends_with("capture done: stop")),
        "the lead's message on the same topic stays: {waiting:?}"
    );
}

/// Wait until the log holds an event of each of `kinds` (`family`: its
/// filter family); the kinds it holds. Events reach the log at its next
/// flush, not at once.
async fn log_has(hub: &TestHub, family: &str, kinds: &[&str]) -> Vec<String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let found: Vec<String> = hub
            .get(&format!("/api/log?filter=kind%3D{family}&limit=1000"))
            .await["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_owned())
            .collect();
        if kinds.iter().all(|k| found.iter().any(|f| f == k)) {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "want {kinds:?}, log has {found:?}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_wait_in_line_wake_a_long_wait_and_end_with_their_holder() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let w1 = hub.claude("w1", Some(&lead), json!({})).await;
    let w2 = hub.claude("w2", Some(&lead), json!({})).await;
    for agent in [&lead, &w1, &w2] {
        agent.turn().await;
    }
    let said = w1
        .ok(
            "lease",
            json!({"action": "acquire", "resource": "cpu:quiet", "note": "bench"}),
        )
        .await;
    assert!(said.starts_with("Granted l"), "{said}");
    // Not free, and no wait asked: answered at once.
    let said = w2
        .ok(
            "lease",
            json!({"action": "acquire", "resource": "cpu:quiet"}),
        )
        .await;
    assert!(said.starts_with("Not free: held by l"), "{said}");
    assert!(said.contains("w1"), "{said}");
    // A wait inside the call that runs out: out of line.
    let said = w2
        .ok(
            "lease",
            json!({"action": "acquire", "resource": "cpu:quiet", "wait_secs": 1}),
        )
        .await;
    assert!(said.starts_with("Not free"), "{said}");
    assert!(
        hub.get("/api/leases").await["waiters"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // A longer wait goes on after the call: the grant is a prompt.
    let said = w2
        .ok(
            "lease",
            json!({"action": "acquire", "resource": "cpu:quiet", "wait_secs": 600}),
        )
        .await;
    assert!(said.starts_with("Waiting: #1 in line"), "{said}");
    // Only the holder (or its owner) releases it.
    let (error, said) = w2
        .tool(
            "lease",
            json!({"action": "release", "resource": "cpu:quiet"}),
        )
        .await;
    assert!(!error && said.starts_with("Stopped waiting"), "{said}");
    w2.ok(
        "lease",
        json!({"action": "acquire", "resource": "cpu:quiet", "wait_secs": 600}),
    )
    .await;
    let lease = hub.get("/api/leases").await["leases"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (error, said) = w2
        .tool("lease", json!({"action": "release", "lease": lease}))
        .await;
    assert!(error && said.contains("held by"), "{said}");
    let said = w1
        .ok(
            "lease",
            json!({"action": "release", "resource": "cpu:quiet"}),
        )
        .await;
    assert!(said.starts_with("Released l"), "{said}");
    w2.typed_has("your lease on cpu:quiet (exclusive) is granted")
        .await;
    let list = lead.ok("lease", json!({"action": "list"})).await;
    assert!(list.contains("cpu:quiet w2 (exclusive"), "{list}");

    // A process holder: the operator's API, ended when its process is.
    let (status, got) = hub
        .call(
            reqwest::Method::POST,
            "/api/leases",
            Some(json!({"resource": "node:x", "ttl_secs": 60, "holder": {
                "host": flywheel_hub::machines::host_name(), "pid": 999_999_999u32,
                "label": "gone"}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    let deadline = Instant::now() + WAIT;
    while hub.get("/api/leases?resource=node:x").await["leases"]
        .as_array()
        .is_some_and(|l| !l.is_empty())
    {
        assert!(Instant::now() < deadline, "the dead process's lease stayed");
        sleep(Duration::from_millis(100)).await;
    }

    // Its holder ends: the lease goes.
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}", w2.id),
            None,
        )
        .await;
    assert!(status.is_success());
    assert!(
        hub.get("/api/leases").await["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    log_has(
        &hub,
        "lease",
        &[
            "lease.granted",
            "lease.waiting",
            "lease.cancelled",
            "lease.released",
            "lease.dropped",
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_asking_for_a_lease_its_ancestor_holds_fails_at_once() {
    let hub = TestHub::start().await;
    let host = flywheel_hub::machines::host_name();
    let me = std::process::id();
    let holder = |pid: u32, label: &str| {
        json!({"host": host, "pid": pid,
               "started": flywheel_hub::hub::leases::process_start(pid),
               "label": label})
    };
    let (status, got) = hub
        .call(
            reqwest::Method::POST,
            "/api/leases",
            Some(json!({"resource": "cpu:test-suite", "ttl_secs": 60,
                        "holder": holder(me, "check.sh")})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    // A child of the holder (a test its suite runs) asks for it too,
    // ready to wait an hour: refused at once, naming the holder.
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let started = Instant::now();
    let (status, got) = hub
        .call(
            reqwest::Method::POST,
            "/api/leases",
            Some(
                json!({"resource": "cpu:test-suite", "ttl_secs": 60, "wait_secs": 3600,
                        "block_secs": 5, "holder": holder(child.id(), "fw eval")}),
            ),
        )
        .await;
    let _ = child.kill();
    let _ = child.wait();
    assert!(started.elapsed() < Duration::from_secs(4), "it waited");
    assert!(status.is_client_error(), "{status} {got}");
    let text = got.to_string();
    assert!(
        text.contains("already held by your ancestor") && text.contains(&format!("pid {me}")),
        "{text}"
    );
    assert!(
        hub.get("/api/leases").await["waiters"]
            .as_array()
            .unwrap()
            .is_empty(),
        "it never joined the line"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_wait_that_runs_out_names_who_holds_it() {
    let hub = TestHub::start().await;
    let mut holder = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let (status, got) = hub
        .call(
            reqwest::Method::POST,
            "/api/leases",
            Some(
                json!({"resource": "cpu:test-suite", "ttl_secs": 600, "holder": {
                "host": flywheel_hub::machines::host_name(), "pid": holder.id(),
                "label": "scripts/check.sh in /x"}}),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    let args: Vec<String> = [
        "lease",
        "acquire",
        "cpu:test-suite",
        "--wait",
        "2s",
        "--url",
        &format!("http://{}", hub.addr),
        "--token",
        hub.hub.token().expose(),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let started = Instant::now();
    let (code, err) = tokio::task::spawn_blocking(move || {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = flywheel_hub::cli::run(&args, &mut out, &mut err);
        (code, String::from_utf8_lossy(&err).into_owned())
    })
    .await
    .unwrap();
    let _ = holder.kill();
    let _ = holder.wait();
    assert_ne!(code, 0, "{err}");
    assert!(started.elapsed() < Duration::from_secs(30), "{err}");
    assert!(
        err.contains("no lease on cpu:test-suite within")
            && err.contains(&format!(
                "held by scripts/check.sh in /x (pid {}",
                holder.id()
            )),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stuck_lease_holder_is_on_needs_you_and_released_from_there() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let w1 = hub.claude("w1", Some(&lead), json!({})).await;
    for agent in [&lead, &w1] {
        agent.turn().await;
    }
    lead.ok(
        "watch",
        json!({"filter": "children kind=lease", "prompt": "a lease your worker waits on is stuck"}),
    )
    .await;
    // The holder: a suite runner whose child is stuck (in its own process
    // group, as a shell command is).
    use std::os::unix::process::CommandExt;
    let mut holder = std::process::Command::new("sh")
        .args(["-c", "sleep 61 & wait"])
        .process_group(0)
        .spawn()
        .unwrap();
    let (status, got) = hub
        .call(
            reqwest::Method::POST,
            "/api/leases",
            Some(
                json!({"resource": "cpu:test-suite", "ttl_secs": 600, "holder": {
                "host": flywheel_hub::machines::host_name(), "pid": holder.id(),
                "started": flywheel_hub::hub::leases::process_start(holder.id()),
                "label": "scripts/check.sh"}}),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    let said = w1
        .ok(
            "lease",
            json!({"action": "acquire", "resource": "cpu:test-suite", "wait_secs": 600}),
        )
        .await;
    assert!(said.starts_with("Waiting"), "{said}");
    // Past the threshold: a hold on Needs you says who holds it, what its
    // process does, and who waits.
    let hold = || async {
        let deadline = Instant::now() + WAIT;
        loop {
            let holds = hub.get("/api/holds").await;
            let found = holds["groups"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["holds"].as_array().unwrap().clone())
                .find(|h| h["cause"] == "lease cpu:test-suite stuck");
            if let Some(found) = found {
                return found;
            }
            assert!(Instant::now() < deadline, "no stuck hold: {holds}");
            sleep(Duration::from_millis(100)).await;
        }
    };
    let found = hold().await;
    assert_eq!(found["kind"], "awaiting_approval", "{found}");
    assert_eq!(found["session"].as_str(), Some(w1.id.as_str()), "{found}");
    let question = found["question"].as_str().unwrap();
    for needle in ["scripts/check.sh", "sleep 61", "w1 waits", "no history"] {
        assert!(question.contains(needle), "{needle}: {question}");
    }
    assert_eq!(found["answers"], json!(["release", "keep waiting"]));
    // The inspector's lease view says so too, and the lead's watch fires.
    let leases = hub.session(&w1.id).await["leases"].clone();
    assert_eq!(
        leases["stuck"][0]["lease"]["resource"], "cpu:test-suite",
        "{leases}"
    );
    log_has(&hub, "lease", &["lease.stuck"]).await;
    lead.typed_has("a lease your worker waits on is stuck")
        .await;

    // Keep waiting: gone until another threshold passes.
    let answer = |id: String, answer: &'static str| {
        let hub = &hub;
        async move {
            hub.call(
                reqwest::Method::POST,
                "/api/holds/answer",
                Some(json!({"holds": [id], "answer": answer})),
            )
            .await
        }
    };
    let id = found["id"].as_str().unwrap().to_owned();
    let (status, got) = answer(id.clone(), "keep waiting").await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert!(got.to_string().contains("keeping"), "{got}");
    let holds = hub.get("/api/holds").await.to_string();
    assert!(!holds.contains("cpu:test-suite stuck"), "{holds}");
    // Back after it, then released: the lease is broken, the holder's
    // process group ends, and the waiter is granted.
    let again = hold().await;
    let (status, got) = answer(again["id"].as_str().unwrap().to_owned(), "release").await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert!(got.to_string().contains("SIGTERM"), "{got}");
    let deadline = Instant::now() + WAIT;
    while holder.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the holder still runs");
        sleep(Duration::from_millis(50)).await;
    }
    w1.typed_has("your lease on cpu:test-suite (exclusive) is granted")
        .await;
    log_has(&hub, "lease", &["lease.broken"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_and_waits_outlive_a_restart() {
    let hub = TestHub::start().await;
    let me = json!({"host": flywheel_hub::machines::host_name(), "pid": std::process::id(),
                    "label": "test"});
    let (status, first) = hub
        .call(
            reqwest::Method::POST,
            "/api/leases",
            Some(json!({"resource": "cpu:test-suite", "ttl_secs": 600, "holder": me})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, waiting) = hub
        .call(
            reqwest::Method::POST,
            "/api/leases",
            Some(
                json!({"resource": "cpu:test-suite", "wait_secs": 600, "holder": {
                "host": "elsewhere", "pid": 4, "label": "other"}}),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{waiting}");
    let waiter = waiting["waiter"]["id"].as_str().unwrap().to_owned();
    let hub = hub.restart(|_| {}).await;
    let view = hub.get("/api/leases").await;
    assert_eq!(view["leases"][0]["id"], first["lease"]["id"], "{view}");
    assert_eq!(view["waiters"][0]["id"], waiter.as_str(), "{view}");
    // Released by its holder: the wait is granted, collected by its id.
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!(
                "/api/leases/{}/release",
                first["lease"]["id"].as_str().unwrap()
            ),
            Some(json!({"holder": me})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let got = hub
        .get(&format!("/api/leases/waiters/{waiter}?wait=1"))
        .await;
    assert_eq!(got["state"], "granted", "{got}");
    assert_eq!(got["lease"]["holder"]["host"], "elsewhere");
    // Someone else's lease: refused without break, broken with it.
    let id = got["lease"]["id"].as_str().unwrap();
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/leases/{id}/release"),
            Some(json!({"holder": me})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, broken) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/leases/{id}/release"),
            Some(json!({"break": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{broken}");
    log_has(&hub, "lease", &["lease.broken"]).await;
}

/// `git` in `dir`, its trimmed output.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_messages_to_a_busy_session_is_one_prompt() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    let busy = hub.claude("busy", None, json!({})).await;
    let other = hub.claude("other", None, json!({})).await;
    for agent in [&lead, &busy, &other] {
        agent.turn().await;
    }
    busy.hook("UserPromptSubmit", json!({"prompt": "work"}))
        .await;
    let said = lead
        .ok("message_session", json!({"to": "busy", "text": "one"}))
        .await;
    assert!(said.starts_with("Queued for busy"), "{said}");
    let said = lead
        .ok("message_session", json!({"to": "busy", "text": "two"}))
        .await;
    assert!(said.contains("joined to your unread message"), "{said}");
    let said = lead
        .ok("message_session", json!({"to": "busy", "text": "two"}))
        .await;
    assert!(said.contains("not added again"), "{said}");
    other
        .ok("message_session", json!({"to": "busy", "text": "three"}))
        .await;
    let view = hub.queue(&busy.id).await;
    assert_eq!(waiting(&view), ["two", "three"], "one item per sender");
    let joined = view["items"][0]["text"].as_str().unwrap();
    assert!(
        joined.ends_with("one\n\n-- and then, from the same session:\n\ntwo"),
        "{joined}"
    );
    assert_eq!(joined.matches("From lead").count(), 1, "{joined}");
    // Its turn ends: the lead's burst goes in as one prompt.
    busy.hook("Stop", json!({})).await;
    busy.typed_has("-- and then, from the same session:").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requested_review_starts_a_reviewer_child_at_the_head() {
    let mut hub = TestHub::start().await;
    // The workspace folder itself is the repo the lead works in.
    let repo = hub.root.join("repo");
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let mut shas = Vec::new();
    for n in 0..3 {
        std::fs::write(repo.join("f"), format!("{n}\n")).unwrap();
        git(&repo, &["add", "f"]);
        git(&repo, &["commit", "-q", "-m", &format!("c{n}")]);
        shas.push(git(&repo, &["rev-parse", "HEAD"]));
    }
    let lead = hub.claude("lead", None, json!({})).await;
    lead.turn().await;
    hub.policy(&lead, "allowed").await;

    // Not a range: refused, nothing started.
    let (error, said) = lead
        .tool("request_review", json!({"base": shas[2], "head": shas[0]}))
        .await;
    assert!(error && said.contains("not an ancestor"), "{said}");

    let said = lead
        .ok(
            "request_review",
            json!({"base": shas[0], "head": "main", "focus": "f counts up"}),
        )
        .await;
    let short = &shas[2][..8];
    assert!(
        said.starts_with(&format!("Started reviewer review-{short} (id ")),
        "{said}"
    );
    assert!(said.contains("(2 commits)"), "{said}");
    let reviewer = hub
        .named(&format!("review-{short}"))
        .await
        .expect("the reviewer session");
    assert_eq!(reviewer["opened_by"], lead.id.as_str(), "{reviewer}");
    // Its worktree is checked out at the head under review.
    let dir = PathBuf::from(reviewer["worktree"]["path"].as_str().unwrap());
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), shas[2]);
    // Its verdict comes back through the lead's children watch.
    let watches = hub.get(&format!("/api/watches?session={}", lead.id)).await;
    assert_eq!(
        watches["watches"][0]["filter"]["children"],
        lead.id.as_str(),
        "{watches}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviews_are_recorded_by_sha_and_coverage_says_what_is_missing() {
    let mut hub = TestHub::start().await;
    let proj = hub.root.join("repo/proj");
    std::fs::create_dir_all(&proj).unwrap();
    git(&proj, &["init", "-q", "-b", "main"]);
    let mut shas = Vec::new();
    for n in 0..4 {
        std::fs::write(proj.join("f"), format!("{n}\n")).unwrap();
        git(&proj, &["add", "f"]);
        git(&proj, &["commit", "-q", "-m", &format!("c{n}")]);
        shas.push(git(&proj, &["rev-parse", "HEAD"]));
    }
    std::fs::write(proj.join("REVIEW.md"), "# Review of c1..c2\n\nfine\n").unwrap();
    let lead = hub.claude("lead", None, json!({})).await;
    let rev = hub.claude("rev", None, json!({})).await;
    for agent in [&lead, &rev] {
        agent.turn().await;
    }
    let repo = proj.to_str().unwrap();
    let said = rev
        .ok(
            "record_review",
            json!({"repo": repo, "base": shas[0], "head": shas[2], "verdict": "READY",
                   "findings": {"low": 2}, "report": "proj/REVIEW.md"}),
        )
        .await;
    assert!(said.starts_with("Recorded review r"), "{said}");
    assert!(said.contains("(2 commits): READY, 2 findings"), "{said}");
    // A report outside the session's directories is refused.
    let (error, said) = rev
        .tool(
            "record_review",
            json!({"repo": repo, "base": shas[0], "head": shas[1], "verdict": "READY",
                   "report": "/etc/hostname"}),
        )
        .await;
    assert!(error && said.contains("outside"), "{said}");
    // base must be an ancestor of head.
    let (error, said) = rev
        .tool(
            "record_review",
            json!({"repo": repo, "base": shas[2], "head": shas[1], "verdict": "READY"}),
        )
        .await;
    assert!(error && said.contains("not an ancestor"), "{said}");

    let said = lead
        .ok(
            "check_reviewed",
            json!({"repo": repo, "range": format!("{}..{}", shas[0], shas[3])}),
        )
        .await;
    assert!(
        said.starts_with("NOT ok: 1 of 3 commits uncovered"),
        "{said}"
    );
    assert!(said.contains("c3: no review"), "{said}");
    let said = lead
        .ok(
            "check_reviewed",
            json!({"repo": repo, "range": format!("{}..{}", shas[0], shas[2])}),
        )
        .await;
    assert!(said.starts_with("ok: all 2 commits"), "{said}");

    // By SHA, with the hub's copy of the report.
    let found = hub
        .get(&format!(
            "/api/verdicts?repo={}&sha={}",
            urlencode(repo),
            shas[1]
        ))
        .await;
    let review = &found["reviews"][0];
    assert_eq!(review["verdict"], "READY", "{found}");
    assert_eq!(review["reviewer"], rev.id.as_str());
    let stored = review["report"]["stored"].as_str().unwrap();
    assert!(std::fs::read_to_string(stored).unwrap().contains("fine"));

    // The operator's NOT_READY follow-up uncovers; retracting it covers again.
    let (status, follow) = hub
        .call(
            reqwest::Method::POST,
            "/api/verdicts",
            Some(json!({"repo": repo, "base": shas[1], "head": shas[2],
                        "verdict": "NOT_READY", "follow_up_of": review["id"]})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{follow}");
    let range = format!("{}..{}", shas[0], shas[2]);
    let coverage = hub
        .get(&format!(
            "/api/verdicts/coverage?repo={}&range={range}",
            urlencode(repo)
        ))
        .await;
    assert_eq!(coverage["ok"], false, "{coverage}");
    assert_eq!(coverage["uncovered"][0]["why"], "not_ready");
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/verdicts/{}/retract", follow["id"].as_str().unwrap()),
            Some(json!({"reason": "recorded on the wrong range"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let coverage = hub
        .get(&format!(
            "/api/verdicts/coverage?repo={}&range={range}",
            urlencode(repo)
        ))
        .await;
    assert_eq!(coverage["ok"], true, "{coverage}");
    log_has(
        &hub,
        "verdict",
        &["verdict.recorded", "verdict.coverage", "verdict.retracted"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_end_closes_its_worktree_only_when_nothing_is_lost() {
    let mut hub = TestHub::start().await;
    let repo = hub.root.join("repo");
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let worker = hub
        .claude(
            "worker",
            Some(&lead),
            json!({"worktree": {"task": "feat-x"}}),
        )
        .await;
    let messy = hub
        .claude(
            "messy",
            Some(&lead),
            json!({"worktree": {"task": "feat-y"}}),
        )
        .await;
    for agent in [&lead, &worker, &messy] {
        agent.turn().await;
    }
    let dir_of = |view: &Value| PathBuf::from(view["worktree"]["path"].as_str().unwrap());
    let wt = dir_of(&hub.session(&worker.id).await);
    std::fs::write(wt.join("STATUS.md"), "# Worker done\n\nshipped x\n").unwrap();
    git(&wt, &["add", "STATUS.md"]);
    git(&wt, &["commit", "-q", "-m", "x"]);
    std::fs::create_dir_all(wt.join("target/debug")).unwrap();
    std::fs::write(wt.join("target/debug/blob"), vec![0u8; 2048]).unwrap();
    let messy_wt = dir_of(&hub.session(&messy.id).await);
    std::fs::write(messy_wt.join("notes.md"), "unsaved").unwrap();

    // The checklist: nothing ends.
    let said = lead
        .ok(
            "end_session",
            json!({"to": "worker", "mode": "at_rest", "dry_run": true}),
        )
        .await;
    assert!(
        said.contains("NOT merged: 1 commit(s) main lacks"),
        "{said}"
    );
    assert!(
        said.contains("remove: allowed (its branch is kept)"),
        "{said}"
    );
    assert!(said.contains("clean build: allowed"), "{said}");
    assert_eq!(hub.session(&worker.id).await["state"], "running");

    // Ended with cleanups: the report is kept, the build and worktree
    // go, the branch stays (its commit is nowhere else).
    lead.ok(
        "end_session",
        json!({"to": "worker", "mode": "at_rest",
               "close": {"worktree": "remove", "build": "clean", "report": "STATUS.md"}}),
    )
    .await;
    hub.ended(&worker.id).await;
    let deadline = Instant::now() + WAIT;
    let close = loop {
        let view = hub.session(&worker.id).await;
        if !view["ended"]["close"].is_null() {
            break view["ended"]["close"].clone();
        }
        assert!(Instant::now() < deadline, "no close recorded: {view}");
        sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(close["worktree"]["outcome"], "done", "{close}");
    assert_eq!(close["build"]["outcome"], "done", "{close}");
    assert_eq!(close["report"]["title"], "Worker done", "{close}");
    assert!(!wt.exists());
    assert!(git(&repo, &["branch", "--list", "*feat-x"]).contains("feat-x"));
    let stored = close["report"]["stored"].as_str().unwrap();
    assert!(
        std::fs::read_to_string(stored)
            .unwrap()
            .contains("shipped x")
    );
    let text = hub
        .http
        .get(format!(
            "http://{}/api/sessions/{}/report",
            hub.addr, worker.id
        ))
        .bearer_auth(hub.hub.token().expose())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("shipped x"), "{text}");
    hub.told(&lead, "closed worker").await;

    // The operator's close: refused while untracked work would be lost,
    // and the work stays.
    let (status, done) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/close", messy.id),
            Some(json!({"worktree": "remove"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["close"]["worktree"]["outcome"], "refused", "{done}");
    assert!(messy_wt.join("notes.md").exists());
    log_has(&hub, "close", &["close.checked", "close.recorded"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worktree_made_with_plain_git_is_adopted_and_closed_by_the_rules() {
    let mut hub = TestHub::start().await;
    let repo = hub.root.join("repo");
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("f"), "0").unwrap();
    git(&repo, &["add", "f"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let old = hub.root.join("repo.worktrees/old-work");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "old-work",
            old.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::write(old.join("g"), "1").unwrap();
    git(&old, &["add", "g"]);
    git(&old, &["commit", "-q", "-m", "work"]);
    let detached = hub.root.join("repo.worktrees/detached");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
        ],
    );
    let lead = hub.claude("lead", None, json!({})).await;
    lead.turn().await;

    // Not tracked yet: listed.
    let view = hub.get("/api/changes?untracked=1").await;
    let untracked: Vec<String> = view["untracked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["path"].as_str().unwrap().to_owned())
        .collect();
    assert!(untracked.iter().any(|p| p.ends_with("old-work")), "{view}");

    let said = lead
        .ok("adopt_worktree", json!({"path": old, "session": "lead"}))
        .await;
    assert!(said.starts_with("Tracking "), "{said}");
    assert!(
        said.contains("branch old-work, base main (from the branch's reflog"),
        "{said}"
    );
    assert!(said.contains("1 unmerged"), "{said}");
    let again = lead.ok("adopt_worktree", json!({"path": old})).await;
    assert!(again.starts_with("Already tracked:"), "{again}");
    for (bad, why) in [
        (repo.clone(), "main checkout"),
        (detached.clone(), "detached HEAD"),
    ] {
        let (error, said) = lead.tool("adopt_worktree", json!({"path": bad})).await;
        assert!(error && said.contains(why), "{said}");
    }
    let view = hub.get("/api/changes?untracked=1").await;
    let change = &view["changes"][0];
    assert_eq!(change["worktree"]["branch"], "old-work", "{view}");
    assert_eq!(change["check"]["unmerged"], 1);
    assert_eq!(change["session_name"], "lead");
    assert!(
        !view["untracked"].to_string().contains("old-work"),
        "{view}"
    );

    // Closed by the rules: untracked work refuses; once clean it goes and
    // its unmerged branch stays.
    std::fs::write(old.join("notes"), "x").unwrap();
    let path = format!("/api/changes/{}/close", change["id"].as_str().unwrap());
    let remove = || Some(json!({"worktree": "remove"}));
    let (status, done) = hub.call(reqwest::Method::POST, &path, remove()).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["close"]["worktree"]["outcome"], "refused", "{done}");
    assert!(old.join("notes").exists());
    std::fs::remove_file(old.join("notes")).unwrap();
    // Its linked session still runs: refused too.
    let (_, done) = hub.call(reqwest::Method::POST, &path, remove()).await;
    assert_eq!(
        done["close"]["worktree"]["why"], "lead also work there",
        "{done}"
    );
    hub.call(
        reqwest::Method::POST,
        &format!("/api/sessions/{}/kill", lead.id),
        None,
    )
    .await;
    let deadline = Instant::now() + WAIT;
    while hub.session(&lead.id).await["state"] != "exited" {
        assert!(Instant::now() < deadline, "lead never exited");
        sleep(Duration::from_millis(50)).await;
    }
    let (_, done) = hub.call(reqwest::Method::POST, &path, remove()).await;
    assert_eq!(done["close"]["worktree"]["outcome"], "done", "{done}");
    assert!(!old.exists());
    assert!(git(&repo, &["branch", "--list", "old-work"]).contains("old-work"));
    assert!(
        hub.get("/api/changes").await["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    log_has(&hub, "change", &["change.adopted"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_leads_successor_hears_its_children() {
    let mut hub = TestHub::start().await;
    let top = hub.claude("top", None, json!({})).await;
    hub.policy(&top, "allowed").await;
    let lead = hub.claude("lead", Some(&top), json!({})).await;
    hub.policy(&lead, "allowed").await;
    let kid = hub.claude("kid", Some(&lead), json!({})).await;
    for agent in [&top, &lead, &kid] {
        agent.turn().await;
    }
    let mine = |id: &str| format!("/api/watches?session={id}");
    assert_eq!(
        hub.get(&mine(&lead.id)).await["watches"][0]["filter"]["children"],
        lead.id.as_str()
    );
    kid.hook("UserPromptSubmit", json!({"prompt": "work"}))
        .await;
    top.ok(
        "replace_session",
        json!({"to": "lead", "name": "lead-2", "prompt": "Carry on.", "final_prompt": "Wrap up."}),
    )
    .await;
    lead.typed_has("Wrap up.").await;
    lead.turn().await;
    hub.ended(&lead.id).await;
    let next = hub.named("lead-2").await.expect("the successor started");
    let next = hub.agent(&next);
    next.env(None).await;
    next.turn().await;
    // The successor watches the children it took over.
    let watches = hub.get(&mine(&next.id)).await;
    assert_eq!(
        watches["watches"][0]["filter"]["children"],
        next.id.as_str(),
        "{watches}"
    );
    // The child's report reaches it.
    kid.hook(
        "Stop",
        json!({"last_assistant_message": "Outcome: done, branch kid at abc"}),
    )
    .await;
    next.typed_has("Outcome: done, branch kid at abc").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_stopped_by_a_rate_limit_is_woken_when_it_resets() {
    use flywheel_hub::watch::RESUME_PROMPT;
    let mut hub = TestHub::start().await;
    let agent = hub.claude("worker", None, json!({})).await;
    agent.turn().await;
    let mine = format!("/api/watches?session={}", agent.id);
    agent
        .hook("UserPromptSubmit", json!({"prompt": "build it"}))
        .await;
    agent
        .hook(
            "StopFailure",
            json!({"error": "rate_limit", "error_details": "5-hour limit reached"}),
        )
        .await;
    // A one-shot resume, in the fallback's 15 minutes (no window said when).
    let deadline = Instant::now() + WAIT;
    let watch = loop {
        let listed = hub.get(&mine).await;
        if let Some(w) = listed["watches"].as_array().unwrap().first() {
            break w.clone();
        }
        assert!(Instant::now() < deadline, "no resume armed: {listed}");
        sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(watch["prompt"], RESUME_PROMPT, "{watch}");
    let next = chrono::DateTime::parse_from_rfc3339(watch["time"]["next"].as_str().unwrap())
        .unwrap()
        .timestamp();
    let ahead = next - chrono::Utc::now().timestamp();
    assert!((14 * 60..=15 * 60).contains(&ahead), "{ahead} s: {watch}");
    // Hit again: still one.
    agent
        .hook("StopFailure", json!({"error": "rate_limit"}))
        .await;
    assert_eq!(hub.get(&mine).await["watches"].as_array().unwrap().len(), 1);
    // A prompt reaches it first: the resume is moot.
    agent
        .hook("UserPromptSubmit", json!({"prompt": "go on"}))
        .await;
    let deadline = Instant::now() + WAIT;
    while !hub.get(&mine).await["watches"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(Instant::now() < deadline, "the resume stayed armed");
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_time_watch_wakes_its_owner_and_passes_to_its_successor() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let old = hub.claude("old", Some(&lead), json!({})).await;
    lead.turn().await;
    old.turn().await;

    // Once, soon: it fires, then it is gone.
    let said = old
        .ok(
            "watch",
            json!({"at": "+2s", "prompt": "check the captures are done"}),
        )
        .await;
    assert!(said.starts_with("Watch w"), "{said}");
    assert!(said.contains("survives hub restarts"), "{said}");
    old.typed_has("check the captures are done").await;
    old.typed_has("] due ").await;
    // (The lead's own children watch, armed when it started old, stays.)
    let deadline = Instant::now() + WAIT;
    while !hub.get(&format!("/api/watches?session={}", old.id)).await["watches"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(Instant::now() < deadline, "a one-shot stayed armed");
        sleep(Duration::from_millis(100)).await;
    }

    // Refused: both a filter and a time, too short a period, the past.
    for (args, why) in [
        (
            json!({"filter": "children", "at": "+1h", "prompt": "x"}),
            "not both",
        ),
        (json!({"every": "1m", "prompt": "x"}), "at least 5m"),
        (
            json!({"at": "2020-01-01T00:00Z", "prompt": "x"}),
            "in the past",
        ),
    ] {
        let (error, said) = old.tool("watch", args).await;
        assert!(error && said.contains(why), "{said}");
    }

    // A repeating one survives a restart and passes to the successor.
    old.turn().await;
    old.ok("watch", json!({"every": "6h", "prompt": "rotate the logs"}))
        .await;
    let mut hub = hub.restart(|_| {}).await;
    let watches = hub.get(&format!("/api/watches?session={}", old.id)).await;
    assert_eq!(
        watches["watches"][0]["time"]["every_secs"], 21600,
        "{watches}"
    );
    let lead = hub.agent(&hub.session(&lead.id).await);
    let old = hub.agent(&hub.session(&old.id).await);
    lead.env(None).await;
    old.env(None).await;
    lead.turn().await;
    old.turn().await;
    lead.ok(
        "replace_session",
        json!({"to": "old", "name": "new", "prompt": "Carry on.", "final_prompt": "Wrap up."}),
    )
    .await;
    old.typed_has("Wrap up.").await;
    old.turn().await;
    hub.ended(&old.id).await;
    let new = hub.named("new").await.expect("the successor started");
    let deadline = Instant::now() + WAIT;
    loop {
        let watches = hub
            .get(&format!(
                "/api/watches?session={}",
                new["id"].as_str().unwrap()
            ))
            .await;
        if watches["watches"][0]["owner"] == new["id"] {
            break;
        }
        assert!(Instant::now() < deadline, "not passed on: {watches}");
        sleep(Duration::from_millis(100)).await;
    }
    log_has(&hub, "watch", &["watch.fired", "watch.moved"]).await;
    let _ = &mut hub;
}

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
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

    // The handoff's successor takes over at once (the old lead stays on
    // the hub): its children, its watches, and a brief of them.
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
    assert_eq!(hub.session(&worker.id).await["owner"], lead2.id.as_str());
    assert_eq!(
        hub.session(&worker.id).await["labels"]["fw.parent"],
        lead2.id.as_str()
    );
    let argv = std::fs::read_to_string(
        hub.root
            .join("log")
            .join(format!("claude-{}.argv", lead2.id)),
    )
    .unwrap();
    assert!(argv.contains("Goal: carry on."), "{argv}");
    assert!(
        argv.contains("--- Handed over by the hub from lead ---"),
        "{argv}"
    );
    assert!(
        argv.contains(&format!("- worker (id {}), ", worker.id)),
        "{argv}"
    );
    let watches = hub.get(&format!("/api/watches?session={}", lead2.id)).await;
    assert_eq!(
        watches["watches"][0]["filter"]["children"],
        lead2.id.as_str(),
        "{watches}"
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

/// Session `sid`'s view once its `parent` (and `fw.parent`) reads `want`.
async fn parent_is(hub: &TestHub, sid: &str, want: Option<&str>) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let view = hub.session(sid).await;
        if view["parent"].as_str() == want && view["labels"]["fw.parent"].as_str() == want {
            return view;
        }
        assert!(Instant::now() < deadline, "parent never {want:?}: {view}");
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_given_session_moves_under_its_new_owner_and_stays_there() {
    let mut hub = TestHub::start().await;
    let old = hub.claude("old lead", None, json!({})).await;
    hub.policy(&old, "allowed").await;
    let worker = hub.claude("worker", Some(&old), json!({})).await;
    let new = hub.claude("new lead", None, json!({})).await;
    old.turn().await;
    worker.turn().await;
    new.turn().await;
    parent_is(&hub, &worker.id, Some(&old.id)).await;

    // Given: its parent is the new owner; who started it stays history.
    old.ok("give_session", json!({"to": "worker", "owner": "new lead"}))
        .await;
    let view = parent_is(&hub, &worker.id, Some(&new.id)).await;
    assert_eq!(view["opened_by"], old.id.as_str());
    let mine = new.ok("list_sessions", json!({"mine": true})).await;
    assert!(mine.contains("worker (id"), "{mine}");
    assert!(
        mine.contains("started by old lead, now led by you"),
        "{mine}"
    );
    let theirs = old.ok("list_sessions", json!({"mine": true})).await;
    assert!(!theirs.contains("worker (id"), "{theirs}");

    // The old lead removed: nothing changes for the new one.
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}", old.id),
            None,
        )
        .await;
    assert!(status.is_success());
    parent_is(&hub, &worker.id, Some(&new.id)).await;
    let mine = new.ok("list_sessions", json!({"mine": true})).await;
    assert!(mine.contains("worker (id"), "{mine}");
    let status_text = new.ok("session_status", json!({"to": "worker"})).await;
    assert!(
        status_text.contains(&format!(
            "started by a removed session (id {}), now led by you (new lead",
            old.id
        )),
        "{status_text}"
    );

    // Its `children` watch wakes the new lead.
    new.ok(
        "watch",
        json!({"filter": "children state=done", "prompt": "children finished"}),
    )
    .await;
    worker.turn().await;
    hub.told(&new, "children finished").await;
    hub.told(&new, " worker (").await;

    // Its owner removed too, and its opener gone: a root.
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}", new.id),
            None,
        )
        .await;
    assert!(status.is_success());
    parent_is(&hub, &worker.id, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_from_before_owners_and_labels_get_their_parent() {
    let mut hub = TestHub::start().await;
    let lead = hub.claude("lead", None, json!({})).await;
    hub.policy(&lead, "allowed").await;
    let worker = hub.claude("worker", Some(&lead), json!({})).await;
    let given = hub.claude("given", Some(&lead), json!({})).await;
    let other = hub.claude("other", None, json!({})).await;
    lead.turn().await;
    lead.ok("give_session", json!({"to": "given", "owner": "other"}))
        .await;
    parent_is(&hub, &given.id, Some(&other.id)).await;
    // Their records as an older hub wrote them: no labels; one never given.
    let (worker_id, given_id) = (worker.id.clone(), given.id.clone());
    let hub = hub
        .restart(|doc| {
            for session in doc["sessions"].as_array_mut().unwrap() {
                let record = session.as_object_mut().unwrap();
                record.remove("labels");
                if record["id"] == worker_id.as_str() {
                    record.remove("owner");
                }
            }
        })
        .await;
    parent_is(&hub, &worker_id, Some(&lead.id)).await;
    parent_is(&hub, &given_id, Some(&other.id)).await;
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
    // The set-aside message is back: waiting, or already typed in (old
    // rests, so the queue may deliver it before this looks).
    let queue = hub.queue(&old.id).await;
    let kept: Vec<&str> = queue["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["text"].as_str())
        .filter(|t| t.ends_with("keep me"))
        .collect();
    assert_eq!(kept.len(), 1, "{queue}");
    assert!(!waiting(&queue).iter().any(|t| t == "Stop."), "{queue}");
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

/// The noise-floor session (2026-11-19): a background task's notice
/// started a turn, its last tool call returned, and while the model was
/// still thinking Claude reported `idle_prompt`. The hub read that as at
/// rest and relaunched it mid-turn, so the turn was lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_report_inside_an_open_turn_does_not_relaunch_it() {
    let mut hub = TestHub::start().await;
    let agent = hub.claude("thinking", None, json!({})).await;
    agent.turn().await;
    agent
        .hook(
            "UserPromptSubmit",
            json!({"prompt": "(a background task finished)"}),
        )
        .await;
    let call = json!({"tool_name": "Bash", "tool_use_id": "tu1",
                      "tool_input": {"command": "true"}});
    agent.hook("PreToolUse", call.clone()).await;
    agent.hook("PostToolUse", call).await;
    agent
        .hook("Notification", json!({"notification_type": "idle_prompt"}))
        .await;
    let before = agent.env(None).await[report::SECRET_ENV].clone();
    hub.hub.mark_stale("0.0.1");
    sleep(WOULD_HAVE).await;
    assert_eq!(agent.env(None).await[report::SECRET_ENV], before);
    let view = hub.session(&agent.id).await;
    assert_eq!(view["stale"]["waits"], "its turn is still open", "{view}");

    // The turn ends: relaunched then.
    agent.hook("Stop", json!({})).await;
    respawned(&hub, &agent, &before).await;
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
