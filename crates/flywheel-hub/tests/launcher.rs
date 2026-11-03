//! The agent launcher end to end: a real hub on 127.0.0.1:0, real PTYs
//! running scripted stand-ins for `claude` and `codex` that record the
//! exact argv they were started with (NUL-separated, so a multi-line
//! prompt is one argument), launch templates over the HTTP API, and token
//! usage read from a transcript the stand-in writes where Claude Code
//! would, pushed over the events WebSocket and rolled up by `/api/usage`.

mod support;

use flywheel_hub::remote::RuntimeInfo;
use flywheel_hub::{HubOptions, ServerOptions, serve};
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

/// A stand-in agent CLI: records its argv (NUL-separated) in
/// `<dir>/<name>.args`, then runs `body`.
fn fake_agent(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let dump = dir.join(format!("{name}.args"));
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{d}.tmp' && mv '{d}.tmp' '{d}'\n{body}\n",
            d = dump.display()
        ),
    );
    path
}

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
}

/// `fw hub ARGS --state-dir <the test hub's>`: (exit code, stdout, stderr).
async fn cli(hub: &TestHub, args: &[&str]) -> (i32, String, String) {
    let mut args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    args.push("--state-dir".into());
    args.push(hub.dir.path().join("state").to_string_lossy().into_owned());
    tokio::task::spawn_blocking(move || {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = flywheel_hub::cli::run(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    })
    .await
    .unwrap()
}

impl TestHub {
    async fn start(dir: tempfile::TempDir, claude: &str) -> Self {
        flywheel_hub::install_crypto_provider();
        let repo = dir.path().join("repo");
        if !repo.exists() {
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "-q", "-b", "main"]);
            std::fs::write(repo.join("README"), "hi\n").unwrap();
            git(&repo, &["add", "README"]);
            git(&repo, &["commit", "-q", "-m", "init"]);
        }
        let claude = fake_agent(dir.path(), "claude", claude);
        let codex = fake_agent(dir.path(), "codex", "exec sleep 600");
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = claude.to_string_lossy().into_owned();
        options.agents.codex = codex.to_string_lossy().into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        options.codex_home = Some(dir.path().join("codex-home"));
        let hub = support::open_hub(options);
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // `fw hub <verb> --state-dir` finds this hub by its runtime file.
        RuntimeInfo {
            pid: std::process::id(),
            listen: addr,
            public_urls: Vec::new(),
            started_at: None,
            log_file: None,
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

    async fn stop(mut self) -> tempfile::TempDir {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = timeout(Duration::from_secs(10), server).await;
        }
        self.dir
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo").canonicalize().unwrap()
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

    async fn ok(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.call(method, path, body).await;
        assert!(status.is_success(), "{path}: {status} {value}");
        value
    }

    async fn workspace(&self) -> String {
        let created = self
            .ok(
                reqwest::Method::POST,
                "/api/workspaces",
                Some(json!({ "name": "w", "folders": [self.repo()] })),
            )
            .await;
        created["id"].as_str().unwrap().to_owned()
    }

    /// The argv the stand-in `name` was last started with (consumed).
    async fn launched(&self, name: &str) -> Vec<String> {
        let path = self.dir.path().join(format!("{name}.args"));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(raw) = std::fs::read(&path) {
                let _ = std::fs::remove_file(&path);
                let text = String::from_utf8(raw).unwrap();
                return text
                    .strip_suffix('\0')
                    .unwrap_or(&text)
                    .split('\0')
                    .map(str::to_owned)
                    .collect();
            }
            assert!(Instant::now() < deadline, "{name} never launched");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn wait_exited(&self, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let session = self
                .ok(reqwest::Method::GET, &format!("/api/sessions/{id}"), None)
                .await;
            if session["state"] == "exited" {
                return;
            }
            assert!(Instant::now() < deadline, "{id} never exited: {session}");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
}

#[tokio::test]
async fn agents_launch_with_their_first_prompt_model_and_permission_mode() {
    let hub = TestHub::start(tempfile::tempdir().unwrap(), "exec sleep 600").await;
    let wid = hub.workspace().await;
    let prompt = "-p is not a flag here\nfix the flaky test\n";
    let created = hub
        .ok(
            reqwest::Method::POST,
            "/api/sessions",
            Some(json!({
                "workspace_id": wid, "kind": "claude", "prompt": prompt,
                "model": "opus", "permission_mode": "acceptEdits",
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let conversation = created["agent_session_id"].as_str().unwrap().to_owned();
    assert_eq!(
        created["launch"],
        json!({ "model": "opus", "permission_mode": "acceptEdits" })
    );
    let args = hub.launched("claude").await;
    assert_eq!(
        args[..3],
        ["--session-id", conversation.as_str(), "--settings"]
    );
    let settings: Value = serde_json::from_str(&args[3]).unwrap();
    assert!(settings["hooks"]["Stop"].is_array());
    // Options after the hub's wiring; the prompt last, one argument,
    // after `--` (so `-p ...` is not read as a flag).
    assert_eq!(
        args[4..],
        [
            "--model",
            "opus",
            "--permission-mode",
            "acceptEdits",
            "--",
            "-p is not a flag here\nfix the flaky test"
        ]
    );

    // Relaunched: the same options, but the prompt was the first turn
    // only. With a transcript on disk it resumes.
    hub.ok(
        reqwest::Method::POST,
        &format!("/api/sessions/{id}/kill"),
        None,
    )
    .await;
    hub.wait_exited(&id).await;
    let project = hub
        .dir
        .path()
        .join("claude-config/projects")
        .join(flywheel_hub::agent::claude_project_dir_name(&hub.repo()));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(format!("{conversation}.jsonl")), "{}\n").unwrap();
    hub.ok(
        reqwest::Method::POST,
        &format!("/api/sessions/{id}/relaunch"),
        Some(json!({})),
    )
    .await;
    let args = hub.launched("claude").await;
    assert_eq!(args[..2], ["--resume", conversation.as_str()]);
    assert_eq!(
        args[4..],
        ["--model", "opus", "--permission-mode", "acceptEdits"]
    );

    // Codex: model, approval policy, sandbox, then the prompt.
    hub.ok(
        reqwest::Method::POST,
        "/api/sessions",
        Some(json!({
            "workspace_id": wid, "kind": "codex", "prompt": "review",
            "model": "gpt-5-codex", "approval": "on-request", "sandbox": "workspace-write",
        })),
    )
    .await;
    let args = hub.launched("codex").await;
    assert_eq!(args[0], "-c");
    assert!(args[1].starts_with("notify=["), "{}", args[1]);
    assert_eq!(
        args[2..],
        [
            "-m",
            "gpt-5-codex",
            "-a",
            "on-request",
            "-s",
            "workspace-write",
            "--",
            "review"
        ]
    );

    // Refused before anything starts.
    for (body, needle) in [
        (json!({ "kind": "shell", "model": "opus" }), "shell"),
        (json!({ "kind": "shell", "prompt": "hi" }), "prompt"),
        (
            json!({ "kind": "claude", "permission_mode": "yolo" }),
            "permission mode",
        ),
        (json!({ "kind": "claude", "approval": "never" }), "Codex"),
        (
            json!({ "kind": "codex", "permission_mode": "plan" }),
            "Claude",
        ),
        (
            json!({ "kind": "claude", "model": "--dangerously-skip-permissions" }),
            "model",
        ),
    ] {
        let mut body = body;
        body["workspace_id"] = json!(wid);
        let (status, error) = hub
            .call(reqwest::Method::POST, "/api/sessions", Some(body.clone()))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(error.to_string().contains(needle), "{body}: {error}");
    }
    hub.stop().await;
}

#[tokio::test]
async fn templates_are_saved_listed_replaced_and_removed_and_survive_a_restart() {
    let hub = TestHub::start(tempfile::tempdir().unwrap(), "exec sleep 600").await;
    let wid = hub.workspace().await;
    let path = format!("/api/workspaces/{wid}/templates");
    let saved = hub
        .ok(
            reqwest::Method::POST,
            &path,
            Some(json!({
                "name": "Fix tests", "kind": "claude", "prompt": "Fix the tests.\nRun them.",
                "model": "sonnet", "permission_mode": "plan", "worktree": true,
            })),
        )
        .await;
    assert_eq!(saved["workspace_id"], json!(wid));
    assert_eq!(saved["name"], "Fix tests");
    assert_eq!(saved["worktree"], true);
    hub.ok(
        reqwest::Method::POST,
        &path,
        Some(json!({ "name": "Review", "kind": "codex", "approval": "never", "sandbox": "read-only" })),
    )
    .await;
    // Same name again: replaced.
    hub.ok(
        reqwest::Method::POST,
        &path,
        Some(json!({ "name": "Fix tests", "kind": "claude", "model": "opus" })),
    )
    .await;
    let listed = hub.ok(reqwest::Method::GET, "/api/templates", None).await;
    let templates = listed["templates"].as_array().unwrap();
    assert_eq!(templates.len(), 2);
    assert_eq!(templates[0]["name"], "Fix tests");
    assert_eq!(templates[0]["model"], "opus");
    assert!(templates[0].get("prompt").is_none());
    // The workspace listing carries them for the UI.
    let workspaces = hub.ok(reqwest::Method::GET, "/api/workspaces", None).await;
    assert_eq!(
        workspaces["workspaces"][0]["templates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Checked like a launch.
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            &path,
            Some(json!({ "name": "bad", "kind": "claude", "sandbox": "read-only" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A restart keeps them.
    let dir = hub.stop().await;
    let hub = TestHub::start(dir, "exec sleep 600").await;
    let listed = hub
        .ok(
            reqwest::Method::GET,
            &format!("/api/templates?workspace_id={wid}"),
            None,
        )
        .await;
    assert_eq!(listed["templates"].as_array().unwrap().len(), 2);
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("{path}/Fix%20tests"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("{path}/Fix%20tests"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let listed = hub.ok(reqwest::Method::GET, "/api/templates", None).await;
    assert_eq!(listed["templates"][0]["name"], "Review");
    hub.stop().await;
}

/// A transcript line as Claude Code writes one, dated now.
fn assistant_line(id: &str, model: &str, input: u64, output: u64, cache_read: u64) -> String {
    json!({
        "type": "assistant",
        "uuid": format!("u-{id}"),
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "message": {
            "id": id, "model": model, "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {
                "input_tokens": input, "output_tokens": output,
                "cache_read_input_tokens": cache_read, "cache_creation_input_tokens": 0,
            },
        },
    })
    .to_string()
}

#[tokio::test]
async fn usage_from_the_transcript_reaches_the_session_the_events_socket_and_rollups() {
    // The stand-in writes its transcript where Claude Code would: under
    // the config dir, by its --session-id.
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("claude-config/projects");
    let line1 = assistant_line("msg_1", "claude-opus-5-5", 100_000, 2_000, 50_000);
    let line2 = assistant_line("msg_2", "claude-unknown-9", 10, 10, 0);
    let script = format!(
        "while [ \"$1\" != --session-id ]; do shift; done\n\
         d='{config}'/$(pwd | sed 's/[^A-Za-z0-9]/-/g')\nmkdir -p \"$d\"\n\
         printf '%s\\n' '{line1}' '{line1}' >> \"$d/$2.jsonl\"\nsleep 2\n\
         printf '%s\\n' '{line2}' >> \"$d/$2.jsonl\"\nexec sleep 600",
        config = config.display()
    );
    let hub = TestHub::start(dir, &script).await;
    let wid = hub.workspace().await;
    let mut request = format!("ws://{}/api/events?client=t", hub.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", hub.token).parse().unwrap(),
    );
    let (mut events, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let created = hub
        .ok(
            reqwest::Method::POST,
            "/api/sessions",
            Some(json!({ "workspace_id": wid, "kind": "claude" })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    // Pushed as it is read.
    let deadline = Instant::now() + Duration::from_secs(20);
    let usage = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = timeout(remaining, events.next())
            .await
            .expect("no usage event")
            .unwrap()
            .unwrap();
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value["type"] == "usage" && value["session_id"] == json!(id) {
                break value["usage"].clone();
            }
        }
    };
    // The repeated line counted once; Opus 5.5 at $4 / $20, cache reads
    // $0.20 per million.
    assert_eq!(usage["turns"], 1);
    assert_eq!(usage["tokens"]["input"], 100_000);
    let expected = (100_000.0 * 4.0 + 2_000.0 * 20.0 + 50_000.0 * 0.20) / 1e6;
    assert!((usage["cost_usd"].as_f64().unwrap() - expected).abs() < 1e-9);
    assert_eq!(usage["priced"], true);
    assert_eq!(usage["today"]["total_tokens"], 152_000);

    // A model without a price: its tokens count, its cost is not guessed.
    let deadline = Instant::now() + Duration::from_secs(20);
    let session = loop {
        let session = hub
            .ok(reqwest::Method::GET, &format!("/api/sessions/{id}"), None)
            .await;
        if session["usage"]["turns"] == 2 {
            break session;
        }
        assert!(
            Instant::now() < deadline,
            "second turn never counted: {session}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(session["usage"]["priced"], false);
    assert_eq!(
        session["usage"]["unpriced_models"],
        json!(["claude-unknown-9"])
    );
    assert!((session["usage"]["cost_usd"].as_f64().unwrap() - expected).abs() < 1e-9);
    let rollup = hub.ok(reqwest::Method::GET, "/api/usage", None).await;
    assert_eq!(rollup["today"]["total_tokens"], 152_020);
    assert_eq!(rollup["workspaces"][0]["workspace_id"], json!(wid));
    assert_eq!(rollup["workspaces"][0]["week"]["total_tokens"], 152_020);
    assert!(
        rollup["price_source"]
            .as_str()
            .unwrap()
            .contains("Anthropic")
    );

    // Removed: the hub's rollups keep what it used.
    hub.ok(
        reqwest::Method::POST,
        &format!("/api/sessions/{id}/kill"),
        None,
    )
    .await;
    hub.wait_exited(&id).await;
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{id}"),
            None,
        )
        .await;
    assert!(status.is_success());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rollup = hub.ok(reqwest::Method::GET, "/api/usage", None).await;
        if rollup["today"]["total_tokens"] == 152_020
            && rollup["workspaces"][0]["total"]["total_tokens"] == 152_020
        {
            break;
        }
        assert!(Instant::now() < deadline, "archive lost it: {rollup}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_templates_and_new_from_a_template() {
    let hub = TestHub::start(tempfile::tempdir().unwrap(), "exec sleep 600").await;
    hub.workspace().await;
    let (code, out, err) = cli(
        &hub,
        &[
            "template",
            "save",
            "w",
            "Fix tests",
            "--kind",
            "claude",
            "--prompt",
            "Fix the flaky test.",
            "--model",
            "sonnet",
            "--permission-mode",
            "plan",
            "--worktree",
        ],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("saved template \"Fix tests\""), "{out}");
    let (code, _, err) = cli(
        &hub,
        &[
            "template",
            "save",
            "w",
            "bad",
            "--kind",
            "claude",
            "--sandbox",
            "read-only",
        ],
    )
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("Codex"), "{err}");
    let (code, out, _) = cli(&hub, &["template", "ls"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("Fix tests  claude +worktree  sonnet, plan  Fix the flaky test."),
        "{out}"
    );

    // From the template, a flag overriding its model: claude in a new
    // worktree named after it, with the template's prompt.
    let (code, out, err) = cli(
        &hub,
        &[
            "new",
            "w",
            "--template",
            "Fix tests",
            "--model",
            "opus",
            "--json",
        ],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let view: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(view["kind"], "claude");
    let task = view["worktree"]["branch"].as_str().unwrap();
    assert!(task.starts_with("fix-tests-"), "{task}");
    let args = hub.launched("claude").await;
    assert_eq!(
        args[4..],
        [
            "--model",
            "opus",
            "--permission-mode",
            "plan",
            "--",
            "Fix the flaky test."
        ]
    );
    let (code, _, err) = cli(&hub, &["new", "w", "--template", "nope"]).await;
    assert_eq!(code, 1);
    assert!(err.contains("no template named \"nope\""), "{err}");

    // ls --json carries the hub's usage rollups.
    let (code, out, _) = cli(&hub, &["ls", "--json"]).await;
    assert_eq!(code, 0);
    let listing: Value = serde_json::from_str(&out).unwrap();
    assert!(listing["usage"]["today"].is_object(), "{listing}");
    assert_eq!(listing["usage"]["workspaces"][0]["name"], "w");

    let (code, _, _) = cli(&hub, &["template", "rm", "w", "Fix tests"]).await;
    assert_eq!(code, 0);
    let (_, out, _) = cli(&hub, &["template", "ls", "w"]).await;
    assert!(out.starts_with("no templates"), "{out}");
    hub.stop().await;
}
