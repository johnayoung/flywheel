//! Session telemetry end to end: real hubs on 127.0.0.1:0, a real PTY
//! running a stand-in for Claude Code (it records the argv and environment
//! the hub launched it with), and the HTTP API. Claude Code's hooks are
//! played by POSTing what it would POST, with the headers it would fill
//! from that environment. Everything lives in a temp dir.

mod support;

use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    hub: Arc<Hub>,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

/// A stand-in Claude Code: answers `--version` like a current CLI, else
/// records its argv (a line each) and environment and waits.
fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo '2.1.322 (Claude Code)'; exit 0; fi\n\
             for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{d}.args'\n\
             env > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\n\
             sleep 600\n",
            d = path.display()
        ),
    );
    path
}

impl TestHub {
    async fn start(upstream: Option<&str>) -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo")).unwrap();
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_claude(dir.path()).to_string_lossy().into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        options.codex_home = Some(dir.path().join("codex-home"));
        let hub = support::open_hub(options);
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        hub.set_agent_url(format!("http://{addr}"));
        let (stop, stopped) = oneshot::channel::<()>();
        let server_options = ServerOptions {
            telemetry_upstream: upstream.map(str::to_owned),
            ..ServerOptions::default()
        };
        let server = tokio::spawn(serve(listener, Arc::clone(&hub), server_options, async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            addr,
            hub,
            token,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base()))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Value {
        let (status, body) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(status, StatusCode::OK, "GET {path}: {body}");
        body
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (status, answer) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(status.is_success(), "POST {path}: {status} {answer}");
        answer
    }

    /// Launch a Claude session; its id, the argv it got, and its
    /// environment.
    async fn claude(&self) -> (String, Vec<String>, HashMap<String, String>) {
        let repo = self.dir.path().join("repo").canonicalize().unwrap();
        let ws = self
            .post(
                "/api/workspaces",
                json!({ "name": "ws", "folders": [repo] }),
            )
            .await;
        let session = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": ws["id"], "kind": "claude", "name": "builder" }),
            )
            .await;
        let id = session["id"].as_str().unwrap().to_owned();
        let env_file = self.dir.path().join("claude.env");
        let deadline = Instant::now() + Duration::from_secs(60);
        let env: HashMap<String, String> = loop {
            if let Ok(text) = std::fs::read_to_string(&env_file)
                && !text.is_empty()
            {
                break text
                    .lines()
                    .filter_map(|l| l.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
            }
            assert!(Instant::now() < deadline, "claude never launched");
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        let args = std::fs::read_to_string(self.dir.path().join("claude.args"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        (id, args, env)
    }

    /// POST as Claude Code would: no credential but the session's own
    /// headers, filled from `env` as the hook configuration says.
    async fn as_agent(
        &self,
        path: &str,
        headers: &Value,
        env: &HashMap<String, String>,
        body: &Value,
    ) -> reqwest::Response {
        let mut request = self.http.post(format!("{}{path}", self.base()));
        for (name, value) in headers.as_object().unwrap() {
            let var = value
                .as_str()
                .unwrap()
                .trim_start_matches('$')
                .trim_start_matches('{')
                .trim_end_matches('}');
            request = request.header(name.as_str(), env.get(var).cloned().unwrap_or_default());
        }
        request.json(body).send().await.unwrap()
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

/// The value after `flag` in `args`.
fn arg_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let at = args.iter().position(|a| a == flag).unwrap_or_else(|| {
        panic!("{flag} missing from {args:?}");
    });
    &args[at + 1]
}

#[tokio::test(flavor = "multi_thread")]
async fn hooks_posted_to_the_hub_drive_status_and_are_recorded() {
    let hub = TestHub::start(None).await;
    let (sid, args, env) = hub.claude().await;

    // The session was launched with http hooks to this hub, the status
    // line, and the progress tool, pre-allowed.
    let settings: Value = serde_json::from_str(arg_after(&args, "--settings")).unwrap();
    let pre = &settings["hooks"]["PreToolUse"][0]["hooks"][0];
    assert_eq!(pre["type"], "http", "{settings}");
    assert_eq!(pre["url"], format!("{}/api/telemetry/hook", hub.base()));
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["type"],
        "command"
    );
    assert!(
        settings["statusLine"]["command"]
            .as_str()
            .unwrap()
            .contains("--event statusline --stdin"),
        "{settings}"
    );
    assert_eq!(
        settings["permissions"]["allow"],
        json!(["mcp__flywheel__report_progress"])
    );
    let mcp: Value = serde_json::from_str(arg_after(&args, "--mcp-config")).unwrap();
    let server = &mcp["mcpServers"]["flywheel"];
    assert_eq!(server["url"], format!("{}/api/telemetry/mcp", hub.base()));
    assert!(arg_after(&args, "--append-system-prompt").contains("report_progress"));
    let headers = pre["headers"].clone();
    for var in pre["allowedEnvVars"].as_array().unwrap() {
        assert!(env.contains_key(var.as_str().unwrap()), "{var} not in env");
    }

    // The hooks, as Claude Code POSTs them: an empty 200 each.
    let hook_path = "/api/telemetry/hook";
    let plays = [
        json!({"hook_event_name": "UserPromptSubmit", "session_id": "conv-1",
               "prompt": "wire the route with token ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"}),
        json!({"hook_event_name": "PreToolUse", "tool_name": "mcp__flywheel__report_progress",
               "tool_use_id": "tu0", "tool_input": {"items": [
                   {"title": "Write the handler", "status": "in_progress"},
                   {"title": "Document it", "status": "pending"}]}}),
        json!({"hook_event_name": "PostToolUse", "tool_name": "mcp__flywheel__report_progress",
               "tool_use_id": "tu0", "duration_ms": 3}),
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "tu1",
               "tool_input": {"command": "cargo test -p flywheel-hub"}}),
    ];
    for play in &plays {
        let response = hub.as_agent(hook_path, &headers, &env, play).await;
        assert_eq!(response.status(), StatusCode::OK, "{play}");
        assert_eq!(
            response.text().await.unwrap(),
            "",
            "no body: the hook passed"
        );
    }

    // The same reports drive the session's status...
    let session = hub.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(session["agent"]["status"], "working", "{session}");

    // ...and are the telemetry.
    let detail = hub.get(&format!("/api/telemetry/sessions/{sid}")).await;
    let state = &detail["session"];
    assert_eq!(state["status"], "working");
    assert_eq!(state["harness"], "claude");
    assert_eq!(state["label"], "builder");
    assert_eq!(state["tool"]["name"], "Bash");
    assert_eq!(state["tool"]["summary"], "cargo test -p flywheel-hub");
    assert_eq!(state["plan"]["total"], 2);
    assert_eq!(state["plan"]["current"], "Write the handler");
    assert_eq!(state["work"][0]["source"], "turn");
    let title = state["turn_title"].as_str().unwrap();
    assert!(title.starts_with("wire the route"), "{title}");
    assert!(
        !title.contains("ghp_"),
        "a token-shaped word is scrubbed: {title}"
    );

    // A blocking prompt, then the call finishing: blocked, then the time
    // it waited is counted.
    let wait = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                      "tool_input": {"command": "cargo test -p flywheel-hub"}});
    assert_eq!(
        hub.as_agent(hook_path, &headers, &env, &wait)
            .await
            .status(),
        StatusCode::OK
    );
    let blocked = hub.get("/api/telemetry/fleet?status=blocked").await;
    assert_eq!(blocked["total"], 1, "{blocked}");
    assert_eq!(blocked["sessions"][0]["session"], sid.as_str());
    assert_eq!(blocked["sessions"][0]["blocked_on"], "permission");
    assert_eq!(blocked["host"], blocked["sessions"][0]["host"]);
    let done = json!({"hook_event_name": "PostToolUseFailure", "tool_name": "Bash",
                      "tool_use_id": "tu1", "duration_ms": 0, "error": "exit 101"});
    hub.as_agent(hook_path, &headers, &env, &done).await;
    let events = hub
        .get(&format!("/api/telemetry/sessions/{sid}/events"))
        .await;
    let events = events["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "session.started",
            "turn.started",
            "tool.started",
            "plan.set",
            "tool.finished",
            "tool.started",
            "wait.started",
            "tool.finished"
        ]
    );
    let finished = events.last().unwrap();
    assert_eq!(finished["body"]["id"], "tu1");
    assert_eq!(finished["body"]["outcome"], "error");
    // The call ran under the plan item that was active.
    assert_eq!(events[5]["item"], "p1");
    assert_eq!(events[5]["work"], "t1");
    let after = hub
        .get(&format!(
            "/api/telemetry/sessions/{sid}/events?after=6&limit=1"
        ))
        .await;
    assert_eq!(after["events"].as_array().unwrap().len(), 1);
    assert_eq!(after["events"][0]["seq"], 7);

    // The status line's input arrives over the report socket (it is a
    // command) and is telemetry alone.
    flywheel_hub::report::send(
        Path::new(&env[flywheel_hub::report::SOCKET_ENV]),
        &flywheel_hub::report::Report {
            session: env[flywheel_hub::report::SESSION_ENV].clone(),
            secret: env[flywheel_hub::report::SECRET_ENV].clone(),
            source: "claude".into(),
            event: "statusline".into(),
            detail: None,
            payload: Some(json!({
                "model": {"id": "claude-fable-5-2"},
                "cost": {"total_cost_usd": 0.75},
                "context_window": {"used_percentage": 30.0, "context_window_size": 200000},
                "rate_limits": {"five_hour": {"used_percentage": 40.0, "resets_at": 1900000000}},
            })),
        },
    )
    .unwrap();
    let fleet = hub.get("/api/telemetry/fleet").await;
    let row = &fleet["sessions"][0];
    assert_eq!(row["context"]["pct"], 30.0, "{row}");
    assert_eq!(row["usage"]["cost_usd"], 0.75);
    assert_eq!(row["rate"][0]["window"], "five_hour");
    assert_eq!(row["model"], "claude-fable-5-2");
    assert_eq!(row["tool_errors"], 1);

    // Who may not: a wrong secret, no headers, no credential.
    let mut forged = env.clone();
    forged.insert(flywheel_hub::report::SECRET_ENV.into(), "nope".into());
    let refused = hub.as_agent(hook_path, &headers, &forged, &plays[0]).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let unnamed = hub
        .http
        .post(format!("{}{hook_path}", hub.base()))
        .json(&plays[0])
        .send()
        .await
        .unwrap();
    assert_eq!(unnamed.status(), StatusCode::UNAUTHORIZED);
    for path in [
        "/api/telemetry/fleet".to_owned(),
        format!("/api/telemetry/sessions/{sid}"),
        format!("/api/telemetry/sessions/{sid}/events"),
    ] {
        let anonymous = hub
            .http
            .get(format!("{}{path}", hub.base()))
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    let (status, _) = hub
        .call(
            reqwest::Method::GET,
            "/api/telemetry/sessions/unknown",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_progress_tool_answers_a_session_and_nobody_else() {
    let hub = TestHub::start(None).await;
    let (_sid, args, env) = hub.claude().await;
    let mcp: Value = serde_json::from_str(arg_after(&args, "--mcp-config")).unwrap();
    let headers = mcp["mcpServers"]["flywheel"]["headers"].clone();
    let path = "/api/telemetry/mcp";
    let ask = |method: &str, params: Value| json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});

    let init = hub
        .as_agent(
            path,
            &headers,
            &env,
            &ask("initialize", json!({"protocolVersion": "2025-06-18"})),
        )
        .await;
    assert_eq!(init.status(), StatusCode::OK);
    let init: Value = init.json().await.unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "flywheel");
    let note = hub
        .as_agent(
            path,
            &headers,
            &env,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
    assert_eq!(note.status(), StatusCode::ACCEPTED);
    let list: Value = hub
        .as_agent(path, &headers, &env, &ask("tools/list", json!({})))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list["result"]["tools"][0]["name"], "report_progress");
    let call: Value = hub
        .as_agent(
            path,
            &headers,
            &env,
            &ask(
                "tools/call",
                json!({"name": "report_progress", "arguments": {"items": [{"title": "a", "status": "pending"}]}}),
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(call["result"]["isError"], false, "{call}");

    let mut forged = env.clone();
    forged.insert(flywheel_hub::report::SECRET_ENV.into(), "nope".into());
    let refused = hub
        .as_agent(path, &headers, &forged, &ask("tools/list", json!({})))
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hub_forwards_its_events_to_its_collector() {
    let collector = TestHub::start(None).await;
    let edge = TestHub::start(Some("collector")).await;

    // The edge records before it knows its collector: nothing is lost.
    let (sid, args, env) = edge.claude().await;
    let settings: Value = serde_json::from_str(arg_after(&args, "--settings")).unwrap();
    let headers = settings["hooks"]["Stop"][0]["hooks"][0]["headers"].clone();
    for play in [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "build it"}),
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "t1",
               "tool_input": {"command": "make"}}),
        json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "t1",
               "duration_ms": 1200}),
        json!({"hook_event_name": "Stop"}),
    ] {
        let sent = edge
            .as_agent("/api/telemetry/hook", &headers, &env, &play)
            .await;
        assert_eq!(sent.status(), StatusCode::OK);
    }

    // Pair them: the collector mints a peer credential, the edge
    // registers it as the machine its `[telemetry] upstream` names.
    let issued = collector
        .post("/api/peers", json!({ "name": "edge" }))
        .await;
    edge.post(
        "/api/machines",
        json!({ "name": "collector", "url": collector.base(), "credential": issued["credential"] }),
    )
    .await;

    let edge_host = edge.hub.telemetry().host().to_owned();
    let deadline = Instant::now() + Duration::from_secs(30);
    let row = loop {
        let fleet = collector
            .get(&format!("/api/telemetry/fleet?host={edge_host}"))
            .await;
        if let Some(row) = fleet["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["status"] == "done")
        {
            break row.clone();
        }
        assert!(Instant::now() < deadline, "never forwarded: {fleet}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(row["session"], sid.as_str());
    assert_eq!(row["host"], edge_host.as_str());
    assert_eq!(row["label"], "builder");
    assert_eq!(row["tools"], 1);
    assert_eq!(row["turn_title"], "build it");

    // The collector's copy of the session is the edge's own.
    let theirs = collector
        .get(&format!("/api/telemetry/sessions/{sid}?host={edge_host}"))
        .await;
    let ours = edge.get(&format!("/api/telemetry/sessions/{sid}")).await;
    assert_eq!(theirs["session"], ours["session"]);
    let events = collector
        .get(&format!(
            "/api/telemetry/sessions/{sid}/events?host={edge_host}"
        ))
        .await;
    assert_eq!(events["events"].as_array().unwrap().len(), 5);
    // Its own board is unchanged, and it lists two hubs' worth at most.
    let own = collector.get("/api/telemetry/fleet?host=local").await;
    assert_eq!(own["total"], 0);

    // A batch that claims the collector's own sessions, or is not a
    // batch, is refused; a repeat is taken and ignored.
    let line = serde_json::to_string(&events["events"][0]).unwrap();
    let again = collector
        .http
        .post(format!("{}/api/telemetry/batch", collector.base()))
        .bearer_auth(&collector.token)
        .body(line.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::OK);
    let again: Value = again.json().await.unwrap();
    assert_eq!(
        (again["received"].clone(), again["accepted"].clone()),
        (json!(1), json!(0))
    );
    let own_host = collector.hub.telemetry().host().to_owned();
    let forged = line.replace(&edge_host, &own_host);
    for body in [forged, "not json".to_owned()] {
        let refused = collector
            .http
            .post(format!("{}/api/telemetry/batch", collector.base()))
            .bearer_auth(&collector.token)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    }
    edge.shutdown().await;
    collector.shutdown().await;
}
