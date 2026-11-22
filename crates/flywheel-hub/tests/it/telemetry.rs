//! Session telemetry end to end: real hubs on 127.0.0.1:0, a real PTY
//! running a stand-in for Claude Code (it records the argv and environment
//! the hub launched it with), and the HTTP API. Claude Code's hooks are
//! played by POSTing what it would POST, with the headers it would fill
//! from that environment. Everything lives in a temp dir.

use crate::support;

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

/// A stand-in Claude Code: records its argv (a line each) and environment
/// and waits.
fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\n\
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
        Self::start_knowing(upstream, Some("2.1.322")).await
    }

    /// A hub that has seen Claude Code `version` in an earlier run (the
    /// file a status line leaves), or none yet.
    async fn start_knowing(upstream: Option<&str>, version: Option<&str>) -> Self {
        Self::start_with(upstream, version, |_| {}).await
    }

    /// A hub with `tweak` applied to its options.
    async fn start_with(
        upstream: Option<&str>,
        version: Option<&str>,
        tweak: impl FnOnce(&mut HubOptions),
    ) -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo")).unwrap();
        if let Some(version) = version {
            std::fs::create_dir_all(dir.path().join("state")).unwrap();
            std::fs::write(dir.path().join("state/claude-version"), version).unwrap();
        }
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_claude(dir.path()).to_string_lossy().into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        options.codex_home = Some(dir.path().join("codex-home"));
        tweak(&mut options);
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
        self.claude_with(json!({ "name": "builder" })).await
    }

    /// A claude session in a new workspace over `repo`, `extra` merged
    /// into its create request.
    async fn claude_with(&self, extra: Value) -> (String, Vec<String>, HashMap<String, String>) {
        let repo = self.dir.path().join("repo").canonicalize().unwrap();
        let ws = self
            .post(
                "/api/workspaces",
                json!({ "name": "ws", "folders": [repo] }),
            )
            .await;
        let mut body = json!({ "workspace_id": ws["id"], "kind": "claude" });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        let session = self.post("/api/sessions", body).await;
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
        json!([
            "mcp__flywheel__report_progress",
            "mcp__flywheel__ask_operator",
            "mcp__flywheel__handoff",
            "mcp__flywheel__list_sessions",
            "mcp__flywheel__start_session",
            "mcp__flywheel__session_status",
            "mcp__flywheel__end_session",
            "mcp__flywheel__replace_session",
            "mcp__flywheel__queued_messages",
            "mcp__flywheel__cancel_message",
            "mcp__flywheel__give_session"
        ])
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
            // The labels it was created with (fw.kind).
            "labels.changed",
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
    // Each call carries the kind of work it is (the live map's station).
    assert_eq!(finished["station"], "shell");
    assert!(events[0].get("station").is_none());
    // Its turn, folded: the failed shell call on the turn's own clock.
    let turns = hub
        .get(&format!("/api/telemetry/sessions/{sid}/turns"))
        .await;
    let turn = &turns["turns"][0];
    assert_eq!(turn["id"], "t1", "{turns}");
    assert_eq!(turn["errors"], 1, "{turns}");
    assert!(
        turn["strip"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["station"] == "shell" && s["failed"] == true),
        "{turns}"
    );
    // The call ran under the plan item that was active.
    assert_eq!(events[6]["item"], "p1");
    assert_eq!(events[6]["work"], "t1");
    let after = hub
        .get(&format!(
            "/api/telemetry/sessions/{sid}/events?after=7&limit=1"
        ))
        .await;
    assert_eq!(after["events"].as_array().unwrap().len(), 1);
    assert_eq!(after["events"][0]["seq"], 8);

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

    // The agent declares what it needs, then ends its turn on it: the
    // session is asking, first after blocked ones, until the next prompt.
    for play in [
        json!({"hook_event_name": "PreToolUse", "tool_name": "mcp__flywheel__ask_operator",
               "tool_use_id": "tu9", "tool_input": {"kind": "run",
               "text": "Restart the relay; it needs sudo.",
               "command": "sudo systemctl restart rh-relay"}}),
        json!({"hook_event_name": "Stop",
               "last_assistant_message": "Over to you: run it and paste the output."}),
    ] {
        let response = hub.as_agent(hook_path, &headers, &env, &play).await;
        assert_eq!(response.status(), StatusCode::OK, "{play}");
    }
    let asking = hub.get("/api/telemetry/fleet?status=asking").await;
    assert_eq!(asking["total"], 1, "{asking}");
    let ask = &asking["sessions"][0]["ask"];
    assert_eq!(ask["kind"], "run", "{asking}");
    assert_eq!(ask["source"], "declared");
    assert_eq!(ask["command"], "sudo systemctl restart rh-relay");

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
    // Its creation labels, then what the agent did.
    assert_eq!(events["events"].as_array().unwrap().len(), 6);
    assert_eq!(events["events"][0]["kind"], "labels.changed");
    // The collector's event log is the fleet's: the edge's hub events
    // (its session's creation) are in it, under the edge's host.
    let log = collector
        .get(&format!(
            "/api/log?after=start&host={edge_host}&kind=session.created&limit=10"
        ))
        .await;
    let created = log["events"].as_array().unwrap();
    assert!(
        created.iter().any(|e| e["session"] == sid.as_str()),
        "{log}"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn a_collector_takes_each_hubs_build_budget_samples() {
    let collector = TestHub::start(None).await;
    let edge = TestHub::start_with(Some("collector"), Some("2.1.322"), |options| {
        options.build_budget = flywheel_hub::budget::BudgetConfig {
            enabled: true,
            jobs: 4,
            test_threads: 2,
        };
    })
    .await;
    let issued = collector
        .post("/api/peers", json!({ "name": "edge" }))
        .await;
    edge.post(
        "/api/machines",
        json!({ "name": "collector", "url": collector.base(), "credential": issued["credential"] }),
    )
    .await;

    // The edge's own reading: its pool, full, and the machine.
    let own = edge.get("/api/budget").await;
    assert_eq!(own["enabled"], true);
    assert_eq!(own["pool"]["jobs"], 4);
    assert_eq!(own["pool"]["free"], 3);
    assert_eq!(own["pool"]["in_use"], 0);
    assert!(own["constraint"].is_string(), "{own}");

    let edge_host = edge.hub.telemetry().host().to_owned();
    let samples = || async {
        collector
            .get(&format!(
                "/api/log?after=start&host={edge_host}&kind=budget.sample&limit=50"
            ))
            .await["events"]
            .as_array()
            .unwrap()
            .clone()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let first = loop {
        if let Some(first) = samples().await.first().cloned() {
            break first;
        }
        assert!(
            Instant::now() < deadline,
            "no budget sample reached the collector"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(first["body"]["pool"]["jobs"], 4);
    assert!(first["body"]["pool"].get("sessions").is_none(), "{first}");
    if cfg!(target_os = "linux") {
        assert!(
            first["body"]["pressure"]["mem_total_bytes"].as_u64() > Some(0),
            "{first}"
        );
    }
    // The collector's own budget view lists the edge.
    let theirs = collector.get("/api/budget").await;
    let fleet = theirs["fleet"].as_array().unwrap();
    assert!(
        fleet
            .iter()
            .any(|h| h["host"] == edge_host.as_str() && h["sample"]["pool"]["jobs"] == 4),
        "{theirs}"
    );
    // Within the period, however the machine moves: no second sample.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(samples().await.len(), 1);
    edge.shutdown().await;
    collector.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_hooks_wait_for_a_status_line_to_show_a_new_enough_cli() {
    let hub = TestHub::start_knowing(None, None).await;
    // The hub's first session ever: it runs no CLI to ask its version, so
    // every hook is the report command.
    let (sid, args, env) = hub.claude().await;
    let settings: Value = serde_json::from_str(arg_after(&args, "--settings")).unwrap();
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["type"],
        "command"
    );
    assert_eq!(
        settings["hooks"]["SubagentStart"][0]["hooks"][0]["type"],
        "command"
    );
    let status_line = |version: &str| flywheel_hub::report::Report {
        session: env[flywheel_hub::report::SESSION_ENV].clone(),
        secret: env[flywheel_hub::report::SECRET_ENV].clone(),
        source: "claude".into(),
        event: "statusline".into(),
        detail: None,
        payload: Some(json!({ "version": version, "cost": {"total_cost_usd": 0.1} })),
    };
    let socket = PathBuf::from(&env[flywheel_hub::report::SOCKET_ENV]);
    // Too old a CLI changes nothing; a current one is remembered.
    flywheel_hub::report::send(&socket, &status_line("2.1.100")).unwrap();
    let saved = hub.dir.path().join("state/claude-version");
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "2.1.100\n");
    flywheel_hub::report::send(&socket, &status_line("2.1.322")).unwrap();
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), "2.1.322\n");
    // Its next launch POSTs.
    std::fs::remove_file(hub.dir.path().join("claude.env")).unwrap();
    hub.post(&format!("/api/sessions/{sid}/kill"), json!({}))
        .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, _) = hub
            .call(
                reqwest::Method::POST,
                &format!("/api/sessions/{sid}/relaunch"),
                Some(json!({})),
            )
            .await;
        if status.is_success() {
            break;
        }
        assert!(Instant::now() < deadline, "never exited: {status}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    while !hub.dir.path().join("claude.env").exists() {
        assert!(Instant::now() < deadline, "never relaunched");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let args: Vec<String> = std::fs::read_to_string(hub.dir.path().join("claude.args"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let settings: Value = serde_json::from_str(arg_after(&args, "--settings")).unwrap();
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["type"],
        "http"
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_is_continued_in_a_linked_session_and_agents_message_each_other() {
    let hub = TestHub::start(None).await;
    let (sid, args, env) = hub.claude().await;
    let settings: Value = serde_json::from_str(arg_after(&args, "--settings")).unwrap();
    let hook_headers = settings["hooks"]["PreToolUse"][0]["hooks"][0]["headers"].clone();
    let mcp: Value = serde_json::from_str(arg_after(&args, "--mcp-config")).unwrap();
    let mcp_headers = mcp["mcpServers"]["flywheel"]["headers"].clone();
    let workspaces = hub.get("/api/workspaces").await;
    let ws = workspaces["workspaces"][0]["id"].clone();

    // Another agent, opened by the first (`fw hub new` inside it, which
    // its start policy allows).
    let (status, _) = hub
        .call(
            reqwest::Method::PUT,
            &format!("/api/sessions/{sid}/start-policy"),
            Some(json!({ "policy": "allowed" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let other = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "claude", "name": "reviewer", "opened_by": sid }),
        )
        .await;
    assert_eq!(other["opened_by"], sid.as_str(), "{other}");
    // An opener this hub does not know is no link.
    let stray = hub
        .post(
            "/api/sessions",
            json!({ "workspace_id": ws, "kind": "shell", "opened_by": "f".repeat(32) }),
        )
        .await;
    assert!(stray.get("opened_by").is_none(), "{stray}");

    // The first lists the others and messages one.
    let call = |name: &str, arguments: Value| {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}})
    };
    let path = "/api/telemetry/mcp";
    let listed: Value = hub
        .as_agent(path, &mcp_headers, &env, &call("list_sessions", json!({})))
        .await
        .json()
        .await
        .unwrap();
    let listed = listed["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(listed.contains("reviewer (id "), "{listed}");
    assert!(
        !listed.contains("builder"),
        "the asker is left out: {listed}"
    );
    let sent: Value = hub
        .as_agent(
            path,
            &mcp_headers,
            &env,
            &call(
                "message_session",
                json!({"to": "reviewer", "text": "rebased on main"}),
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    let said = sent["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(sent["result"]["isError"], false, "{sent}");
    assert!(
        said.starts_with("Delivered to reviewer") || said.starts_with("Queued for reviewer"),
        "{said}"
    );
    let to_self: Value = hub
        .as_agent(
            path,
            &mcp_headers,
            &env,
            &call("message_session", json!({"to": "builder", "text": "hi"})),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(to_self["result"]["isError"], true, "{to_self}");

    // The first proposes a handoff: the tool call itself keeps it.
    let proposed: Value = hub
        .as_agent(
            path,
            &mcp_headers,
            &env,
            &call(
                "handoff",
                json!({"prompt": "Goal: finish the route.\nDone: the store.",
                       "name": "route work", "reason": "Long context"}),
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(proposed["result"]["isError"], false, "{proposed}");
    let pending =
        |view: &Value| -> Vec<Value> { view["handoffs"].as_array().cloned().unwrap_or_default() };
    let first = pending(&hub.get(&format!("/api/sessions/{sid}")).await);
    assert_eq!(first.len(), 1, "{first:?}");
    let first_id = first[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(first[0]["name"], "route work");
    assert_eq!(first[0]["reason"], "Long context");
    let said = proposed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        said.contains(&format!("fw hub continue {sid} --handoff {first_id}")),
        "{said}"
    );
    // The hook path sees a tool call, not an ask; a new prompt, a view,
    // and the turn ending clear nothing.
    for play in [
        json!({"hook_event_name": "PreToolUse", "tool_name": "mcp__flywheel__handoff",
               "tool_use_id": "h1", "tool_input": {"prompt": "Goal: finish the route."}}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "Handed off."}),
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "one more thing"}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "Done."}),
    ] {
        let response = hub
            .as_agent("/api/telemetry/hook", &hook_headers, &env, &play)
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{play}");
    }
    let snap = hub.get(&format!("/api/telemetry/sessions/{sid}")).await;
    assert!(snap["session"]["ask"].is_null(), "{snap}");
    assert_eq!(
        pending(&hub.get(&format!("/api/sessions/{sid}")).await).len(),
        1
    );
    // A second handoff waits beside the first.
    let again: Value = hub
        .as_agent(
            path,
            &mcp_headers,
            &env,
            &call("handoff", json!({"prompt": "Goal: write the docs."})),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again["result"]["isError"], false, "{again}");
    let both = pending(&hub.get(&format!("/api/sessions/{sid}")).await);
    assert_eq!(both.len(), 2, "{both:?}");
    let second_id = both[1]["id"].as_str().unwrap().to_owned();
    // An empty prompt is refused, and kept nowhere.
    let empty: Value = hub
        .as_agent(
            path,
            &mcp_headers,
            &env,
            &call("handoff", json!({"prompt": " "})),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(empty["result"]["isError"], true, "{empty}");
    // Two pending: which one must be said.
    let (status, refused) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{sid}/continue"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let why = refused.to_string();
    assert!(why.contains(&first_id) && why.contains(&second_id), "{why}");

    // Starting the first: a linked session from its prompt, under its
    // name; the second still waits.
    let (status, next) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{sid}/continue"),
            Some(json!({"handoff": first_id})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{next}");
    assert_eq!(next["continues"], sid.as_str(), "{next}");
    assert_eq!(next["kind"], "claude");
    assert_eq!(next["name"], "route work");
    let left = pending(&hub.get(&format!("/api/sessions/{sid}")).await);
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["id"], second_id.as_str());
    // Started once: a second start of it is refused.
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{sid}/continue"),
            Some(json!({"handoff": first_id})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let next_id = next["id"].as_str().unwrap().to_owned();
    let args_file = hub.dir.path().join("claude.args");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let args = std::fs::read_to_string(&args_file).unwrap_or_default();
        if args.contains("Goal: finish the route.") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the successor never got its prompt: {args}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let old = hub.get(&format!("/api/telemetry/sessions/{sid}")).await;
    assert_eq!(old["session"]["status"], "done", "{old}");
    assert_eq!(old["session"]["continued_by"], next_id.as_str());
    // Nothing proposed: a prompt must be given.
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{next_id}/continue"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Dismissed: gone, unstarted.
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{sid}/handoffs/{second_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(pending(&hub.get(&format!("/api/sessions/{sid}")).await).is_empty());
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{sid}/handoffs/{second_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The only one pending is the default; a taken name is numbered on.
    let third: Value = hub
        .as_agent(
            path,
            &mcp_headers,
            &env,
            &call(
                "handoff",
                json!({"prompt": "Goal: polish.", "name": "route work"}),
            ),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(third["result"]["isError"], false, "{third}");
    let (status, polished) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{sid}/continue"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{polished}");
    assert_eq!(polished["name"], "route work 2");
    assert!(pending(&hub.get(&format!("/api/sessions/{sid}")).await).is_empty());
    // A requested name wins, and is checked as a rename is.
    let fourth: Value = hub
        .as_agent(
            path,
            &mcp_headers,
            &env,
            &call("handoff", json!({"prompt": "Goal: ship.", "name": "x"})),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fourth["result"]["isError"], false, "{fourth}");
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{sid}/continue"),
            Some(json!({"name": "a\u{7}b"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        pending(&hub.get(&format!("/api/sessions/{sid}")).await).len(),
        1,
        "a refused start keeps the handoff"
    );
    hub.shutdown().await;
}

/// POST a capture hook as Claude Code would: the installed hook's
/// headers, `$VAR` ones filled from `env` (empty when unset), the rest as
/// written.
async fn as_captured(
    hub: &TestHub,
    hook: &Value,
    env: &HashMap<String, String>,
    body: &Value,
) -> reqwest::Response {
    let mut request = hub.http.post(hook["url"].as_str().unwrap());
    for (name, value) in hook["headers"].as_object().unwrap() {
        let value = value.as_str().unwrap();
        let value = match value.strip_prefix('$') {
            Some(var) => env.get(var).cloned().unwrap_or_default(),
            None => value.to_owned(),
        };
        request = request.header(name.as_str(), value);
    }
    request.json(body).send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn captured_runs_report_under_their_parent_and_batch_and_are_priced() {
    let hub = TestHub::start(None).await;
    let (sid, args, parent_env) = hub.claude().await;
    let installed = hub.get("/api/telemetry/capture/hooks").await;
    assert!(
        installed["hooks"].get("SessionStart").is_none(),
        "{installed}"
    );
    let hook = installed["hooks"]["Stop"][0]["hooks"][0].clone();
    assert_eq!(
        hook["url"],
        format!("{}/api/telemetry/capture", hub.base()),
        "{hook}"
    );
    // The token is the hub's alone, made once.
    assert_eq!(
        hub.get("/api/telemetry/capture/hooks").await["hooks"]["Stop"][0]["hooks"][0],
        hook
    );

    // A transcript with one priced response.
    let projects = hub.dir.path().join("claude-config/projects/p");
    std::fs::create_dir_all(&projects).unwrap();
    let transcript = |conversation: &str| {
        let path = projects.join(format!("{conversation}.jsonl"));
        let line = json!({"type": "assistant", "timestamp": "2026-11-11T10:00:00Z",
            "message": {"id": format!("m-{conversation}"), "model": "claude-sonnet-4-5",
                        "usage": {"input_tokens": 1000, "output_tokens": 2000,
                                  "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}});
        std::fs::write(&path, format!("{line}\n")).unwrap();
        path.to_string_lossy().into_owned()
    };
    let run = |conversation: &str, event: &str| {
        json!({"hook_event_name": event, "session_id": conversation,
               "cwd": "/work/analyses/49", "prompt": "Analyse order 0xdaf3",
               "transcript_path": transcript(conversation),
               "last_assistant_message": "Report written."})
    };

    // A `claude -p` inside the hub session, in a named batch with a budget.
    let mut child_env = parent_env.clone();
    child_env.insert("FLYWHEEL_BATCH".into(), "post-trade analysts".into());
    child_env.insert("FLYWHEEL_BATCH_BUDGET".into(), "0.01".into());
    for event in ["UserPromptSubmit", "Stop", "SessionEnd"] {
        let sent = as_captured(&hub, &hook, &child_env, &run("c0ffee-1", event)).await;
        assert_eq!(sent.status(), StatusCode::OK);
    }
    // One with no batch: counted under the hub session it ran in.
    let sent = as_captured(
        &hub,
        &hook,
        &parent_env,
        &run("c0ffee-2", "UserPromptSubmit"),
    )
    .await;
    assert_eq!(sent.status(), StatusCode::OK);
    // One from a plain terminal: outside the hub.
    let sent = as_captured(
        &hub,
        &hook,
        &HashMap::new(),
        &run("c0ffee-3", "UserPromptSubmit"),
    )
    .await;
    assert_eq!(sent.status(), StatusCode::OK);
    // The hub session's own conversation: its own hooks report it.
    let own = arg_after(&args, "--session-id").to_owned();
    let sent = as_captured(&hub, &hook, &parent_env, &run(&own, "UserPromptSubmit")).await;
    assert_eq!(sent.status(), StatusCode::OK);
    // A wrong token is refused.
    let mut forged = hook.clone();
    forged["headers"]["x-flywheel-capture"] = json!("0".repeat(64));
    let sent = as_captured(
        &hub,
        &forged,
        &HashMap::new(),
        &run("c0ffee-4", "UserPromptSubmit"),
    )
    .await;
    assert_eq!(sent.status(), StatusCode::UNAUTHORIZED);

    let page = hub.get("/api/telemetry/fleet?rollup=1").await;
    let listed: Vec<&str> = page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session"].as_str().unwrap())
        .collect();
    assert!(listed.iter().all(|s| !s.starts_with("cc-")), "{page}");
    let batches = page["batches"].as_array().unwrap();
    let mut keys: Vec<&str> = batches.iter().map(|b| b["key"].as_str().unwrap()).collect();
    keys.sort_unstable();
    let under = format!("under:{sid}");
    assert_eq!(
        keys,
        ["batch:post-trade analysts", "outside", under.as_str()],
        "{page}"
    );
    let analysts = batches
        .iter()
        .find(|b| b["key"] == "batch:post-trade analysts")
        .unwrap();
    assert_eq!(analysts["sessions"], 1);
    assert_eq!(analysts["budget_usd"], 0.01);
    assert_eq!(analysts["over_budget"], true, "{analysts}");
    assert!(analysts["cost_usd"].as_f64().unwrap() > 0.01, "{analysts}");

    // The run itself: linked to its parent, priced, ended.
    let detail = hub
        .get("/api/telemetry/fleet?batch=batch:post-trade%20analysts")
        .await;
    let snap = &detail["sessions"][0];
    assert_eq!(snap["session"], "cc-c0ffee-1");
    assert_eq!(snap["status"], "ended", "{snap}");
    assert_eq!(snap["links"]["opened_by"], sid.as_str());
    assert_eq!(snap["links"]["captured"], true);
    assert_eq!(snap["label"], "49");
    assert_eq!(snap["model"], "claude-sonnet-4-5");
    assert_eq!(snap["usage"]["output"], 2000);
    // The hub session's own conversation made no captured run.
    let all = hub.get("/api/telemetry/fleet").await;
    let own_run = format!("cc-{own}");
    assert!(
        all["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["session"] != own_run.as_str()),
        "{all}"
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_eval_trials_captured_run_carries_the_hub_owned_eval_label() {
    let hub = TestHub::start(None).await;
    let installed = hub.get("/api/telemetry/capture/hooks").await;
    let hook = installed["hooks"]["UserPromptSubmit"][0]["hooks"][0].clone();
    let mut trial_env = HashMap::new();
    trial_env.insert("FLYWHEEL_BATCH".to_owned(), "eval:r7:t1".to_owned());
    let mut other_env = HashMap::new();
    other_env.insert("FLYWHEEL_BATCH".to_owned(), "nightly".to_owned());
    for (env, conversation) in [(&trial_env, "trial-1"), (&other_env, "run-2")] {
        let sent = as_captured(
            &hub,
            &hook,
            env,
            &json!({"hook_event_name": "UserPromptSubmit", "session_id": conversation, "prompt": "go"}),
        )
        .await;
        assert_eq!(sent.status(), StatusCode::OK);
    }
    let listed = |page: &Value| -> Vec<String> {
        page["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session"].as_str().unwrap().to_owned())
            .collect()
    };
    let page = hub.get("/api/telemetry/fleet?label=fw.eval%3Dr7").await;
    assert_eq!(listed(&page), ["cc-trial-1"], "{page}");
    let page = hub.get("/api/telemetry/fleet?label=fw.eval%3Dother").await;
    assert!(listed(&page).is_empty(), "{page}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lanes_place_sessions_by_what_they_need_grouped_by_workspace() {
    let hub = TestHub::start(None).await;
    let (sid, args, env) = hub.claude().await;
    let settings: Value = serde_json::from_str(arg_after(&args, "--settings")).unwrap();
    let headers = settings["hooks"]["Stop"][0]["hooks"][0]["headers"].clone();
    for body in [
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "Ship it"}),
        json!({"hook_event_name": "Stop", "last_assistant_message": "Ready. Should I push?"}),
    ] {
        let sent = hub
            .as_agent("/api/telemetry/hook", &headers, &env, &body)
            .await;
        assert_eq!(sent.status(), StatusCode::OK);
    }
    // And a captured run, as its batch.
    let installed = hub.get("/api/telemetry/capture/hooks").await;
    let hook = installed["hooks"]["UserPromptSubmit"][0]["hooks"][0].clone();
    let mut batch_env = HashMap::new();
    batch_env.insert("FLYWHEEL_BATCH".to_owned(), "nightly".to_owned());
    let sent = as_captured(
        &hub,
        &hook,
        &batch_env,
        &json!({"hook_event_name": "UserPromptSubmit", "session_id": "run-1", "prompt": "go"}),
    )
    .await;
    assert_eq!(sent.status(), StatusCode::OK);

    let view = hub.get("/api/telemetry/lanes").await;
    let lanes = view["lanes"].as_array().unwrap();
    let ids: Vec<&str> = lanes.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["needs", "moving", "waiting", "finished", "idle"]);
    let needs = &lanes[0];
    assert_eq!(needs["count"], 1, "{view}");
    assert_eq!(needs["groups"][0]["project"], "ws");
    let card = &needs["groups"][0]["cards"][0];
    assert_eq!(card["session"]["session"], sid.as_str());
    assert_eq!(card["why"], "Should I push?");
    let moving = &lanes[1];
    assert_eq!(moving["groups"][0]["project"], "captured runs");
    assert_eq!(
        moving["groups"][0]["cards"][0]["batch"]["key"],
        "batch:nightly"
    );
    assert_eq!(view["total"], 1);
    assert_eq!(view["runs"], 1);
    hub.shutdown().await;
}

/// Wait until session `id` was told `needle`: typed in (its screen) or
/// queued for when its turn is done.
async fn told(hub: &TestHub, id: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, screen) = hub
            .call(
                reqwest::Method::GET,
                &format!("/api/sessions/{id}/screen?source=recent&lines=200"),
                None,
            )
            .await;
        let (_, queue) = hub
            .call(
                reqwest::Method::GET,
                &format!("/api/sessions/{id}/queue"),
                None,
            )
            .await;
        // Pasted text wraps on the screen: compare without line breaks.
        let flat = |v: &Value| v.to_string().replace("\\n", "").replace('\n', "");
        if flat(&screen).contains(needle) || queue.to_string().contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{id} was never told {needle:?}: {screen} {queue}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until the newest claude launch's argv holds `needle`.
async fn launched_with(hub: &TestHub, needle: &str) -> String {
    let args_file = hub.dir.path().join("claude.args");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let args = std::fs::read_to_string(&args_file).unwrap_or_default();
        if args.contains(needle) {
            return args;
        }
        assert!(
            Instant::now() < deadline,
            "never launched with {needle:?}: {args}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_start_sessions_within_their_policy_and_limits() {
    let hub = TestHub::start(None).await;
    let (lead, args, env) = hub
        .claude_with(json!({ "name": "lead", "model": "opus", "permission_mode": "acceptEdits" }))
        .await;
    let mcp: Value = serde_json::from_str(arg_after(&args, "--mcp-config")).unwrap();
    let headers = mcp["mcpServers"]["flywheel"]["headers"].clone();
    let ws = hub.get(&format!("/api/sessions/{lead}")).await["workspace_id"].clone();
    let tool = |name: &str, arguments: Value| {
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": name, "arguments": arguments}});
        let hub = &hub;
        let headers = &headers;
        let env = &env;
        async move {
            let answer: Value = hub
                .as_agent("/api/telemetry/mcp", headers, env, &message)
                .await
                .json()
                .await
                .unwrap();
            let result = answer["result"].clone();
            (
                result["isError"] == true,
                result["content"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
            )
        }
    };
    let set_policy = |policy: &str| {
        let body = json!({ "policy": policy });
        let path = format!("/api/sessions/{lead}/start-policy");
        let hub = &hub;
        async move {
            let (status, view) = hub.call(reqwest::Method::PUT, &path, Some(body)).await;
            assert_eq!(status, StatusCode::OK, "{view}");
            view
        }
    };
    let requests = |view: &Value| -> Vec<Value> {
        view["start_requests"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };

    // The default is ask: two starts in one turn wait, the agent keeps
    // working; a folder outside the workspace is refused.
    let lead_view = hub.get(&format!("/api/sessions/{lead}")).await;
    assert_eq!(lead_view["start_policy"], "ask", "{lead_view}");
    let (error, said) = tool(
        "start_session",
        json!({"name": "worker", "prompt": "Goal: build part A."}),
    )
    .await;
    assert!(!error, "{said}");
    assert!(
        said.starts_with("Waiting for the operator (request "),
        "{said}"
    );
    assert!(said.contains("Keep working."), "{said}");
    let (error, said) = tool(
        "start_session",
        json!({"name": "docs", "prompt": "Goal: write the docs.", "model": "sonnet"}),
    )
    .await;
    assert!(!error, "{said}");
    let (error, said) = tool(
        "start_session",
        json!({"name": "far", "prompt": "Goal: x", "folder": "/"}),
    )
    .await;
    assert!(error, "{said}");
    assert!(said.contains("is not part of workspace"), "{said}");
    let waiting = requests(&hub.get(&format!("/api/sessions/{lead}")).await);
    assert_eq!(waiting.len(), 2, "{waiting:?}");
    assert_eq!(waiting[0]["name"], "worker");
    assert_eq!(waiting[0]["kind"], "claude");
    // The caller's own launch options; only the model may differ.
    assert_eq!(waiting[0]["launch"]["permission_mode"], "acceptEdits");
    assert_eq!(waiting[0]["launch"]["model"], "opus");
    assert_eq!(waiting[1]["launch"]["model"], "sonnet");
    assert_eq!(waiting[1]["launch"]["permission_mode"], "acceptEdits");
    let (worker_rid, docs_rid) = (
        waiting[0]["id"].as_str().unwrap().to_owned(),
        waiting[1]["id"].as_str().unwrap().to_owned(),
    );

    // Declined: gone, and the agent is told.
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{lead}/starts/{docs_rid}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    told(
        &hub,
        &lead,
        &format!("declined your start request {docs_rid}"),
    )
    .await;

    // Started: linked, named, its prompt under who started it; the agent
    // is told; a second start of it finds nothing.
    let (status, worker) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{lead}/starts/{worker_rid}"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{worker}");
    assert_eq!(worker["name"], "worker");
    assert_eq!(worker["opened_by"], lead.as_str());
    assert_eq!(worker["launch"]["permission_mode"], "acceptEdits");
    assert_eq!(worker["launch"]["model"], "opus");
    let argv = launched_with(&hub, "Goal: build part A.").await;
    assert!(argv.contains("From lead in repo: "), "{argv}");
    assert!(
        argv.contains(&format!("started by lead (id {lead})")),
        "{argv}"
    );
    let worker_id = worker["id"].as_str().unwrap().to_owned();
    told(&hub, &lead, &format!("started worker (id {worker_id})")).await;
    assert!(requests(&hub.get(&format!("/api/sessions/{lead}")).await).is_empty());
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &format!("/api/sessions/{lead}/starts/{worker_rid}"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Allowed: started at once, with its own model.
    let view = set_policy("allowed").await;
    assert_eq!(view["start_sessions"], "allowed");
    let (error, said) = tool(
        "start_session",
        json!({"name": "docs", "prompt": "Goal: write the docs.", "model": "sonnet"}),
    )
    .await;
    assert!(!error, "{said}");
    assert!(said.starts_with("Started docs (id "), "{said}");
    assert!(said.contains("opened by you"), "{said}");
    launched_with(&hub, "Goal: write the docs.").await;
    let sessions = hub.get("/api/sessions").await;
    let docs = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "docs")
        .cloned()
        .unwrap();
    assert_eq!(docs["opened_by"], lead.as_str());
    assert_eq!(docs["launch"]["model"], "sonnet");
    assert_eq!(docs["launch"]["permission_mode"], "acceptEdits");
    // A taken name is made unique.
    let (error, said) = tool(
        "start_session",
        json!({"name": "docs", "prompt": "Goal: more docs."}),
    )
    .await;
    assert!(!error, "{said}");
    assert!(said.starts_with("Started docs 2 (id "), "{said}");

    // Its status, read without messaging it; the caller's own list.
    let (error, status_text) = tool("session_status", json!({"to": "docs"})).await;
    assert!(!error, "{status_text}");
    assert!(status_text.starts_with("docs (id "), "{status_text}");
    assert!(status_text.contains("\nstate: "), "{status_text}");
    assert!(
        status_text.contains("started by you (lead"),
        "{status_text}"
    );
    assert!(status_text.contains("\nruntime: "), "{status_text}");
    let (_, mine) = tool("list_sessions", json!({"mine": true})).await;
    for name in ["worker (id", "docs (id", "docs 2 (id"] {
        assert!(mine.contains(name), "{mine}");
    }
    assert!(mine.contains("started by you"), "{mine}");

    // `fw hub new` inside the agent obeys the same policy: allowed opens,
    // ask waits, off refuses; a shell is not an agent start.
    let (status, made) = hub
        .call(
            reqwest::Method::POST,
            "/api/sessions",
            Some(json!({"workspace_id": ws, "kind": "claude", "name": "cli-made", "opened_by": lead})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["opened_by"], lead.as_str());
    set_policy("ask").await;
    let (status, held) = hub
        .call(
            reqwest::Method::POST,
            "/api/sessions",
            Some(json!({"workspace_id": ws, "kind": "claude", "name": "cli-held", "prompt": "Goal: y", "opened_by": lead})),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{held}");
    assert_eq!(held["opened_by"]["name"], "lead");
    assert_eq!(held["waiting"]["name"], "cli-held");
    let view = set_policy("off").await;
    assert_eq!(view["start_policy"], "off");
    let (error, said) = tool(
        "start_session",
        json!({"name": "nope", "prompt": "Goal: z"}),
    )
    .await;
    assert!(error, "{said}");
    assert!(said.contains("starting sessions is off for lead"), "{said}");
    let (status, refused) = hub
        .call(
            reqwest::Method::POST,
            "/api/sessions",
            Some(json!({"workspace_id": ws, "kind": "claude", "opened_by": lead})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, shell) = hub
        .call(
            reqwest::Method::POST,
            "/api/sessions",
            Some(json!({"workspace_id": ws, "kind": "shell", "opened_by": lead})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{shell}");

    // Each limit refuses by name. Four starts made (worker, docs, docs 2,
    // cli-made); removing one gives nothing back.
    set_policy("default").await;
    set_policy("allowed").await;
    let limit = |change: fn(&mut flywheel_hub::config::AgentStarts)| {
        let mut tunables = hub.hub.tunables();
        change(&mut tunables.agent_starts);
        hub.hub.set_tunables(tunables);
    };
    let refused_by = |name: &'static str| {
        let tool = &tool;
        async move {
            let (error, said) = tool(
                "start_session",
                json!({"name": "one more", "prompt": "Goal: w"}),
            )
            .await;
            assert!(error, "{name}: {said}");
            assert!(said.contains(name), "{name}: {said}");
        }
    };
    assert_eq!(
        hub.get(&format!("/api/sessions/{lead}")).await["starts_made"],
        4
    );
    limit(|l| l.max_total = 4);
    refused_by("agent_starts.max_total").await;
    let docs_id = docs["id"].as_str().unwrap();
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{docs_id}"),
            None,
        )
        .await;
    assert!(status.is_success(), "{status}");
    refused_by("agent_starts.max_total").await;
    // Three of its sessions still run: worker, docs 2, cli-made.
    limit(|l| {
        l.max_total = 40;
        l.max_running = 3;
    });
    refused_by("agent_starts.max_running").await;
    limit(|l| {
        l.max_running = 10;
        l.max_hub = 3;
    });
    refused_by("agent_starts.max_hub").await;
    limit(|l| {
        l.max_hub = 40;
        l.max_depth = 0;
    });
    refused_by("agent_starts.max_depth").await;
    // Under ask a start over a limit is refused at once, not kept.
    set_policy("ask").await;
    refused_by("agent_starts.max_depth").await;
    hub.shutdown().await;
}
