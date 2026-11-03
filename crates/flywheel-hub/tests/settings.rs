//! Settings and hub.toml end to end: a real hub on 127.0.0.1:0, the
//! settings API (validation, keymap conflicts, revisions, persistence
//! across a restart, the events push), and `<state-dir>/hub.toml` read
//! with flag > file > default precedence, hot-reloaded while the hub runs
//! (agent launch defaults reach a real PTY's argv), an invalid edit kept
//! out and reported, and `fw hub config check|show`.

mod support;

use flywheel_hub::config::{ConfigWatch, Flags};
use flywheel_hub::remote::RuntimeInfo;
use flywheel_hub::{HubOptions, ServerOptions, serve};
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct TestHub {
    dir: PathBuf,
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

/// A stand-in `claude` that records its argv (NUL-separated) and waits.
fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    let dump = dir.join("claude.args");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{d}.tmp' && mv '{d}.tmp' '{d}'\nexec sleep 600\n",
            d = dump.display()
        ),
    );
    path
}

impl TestHub {
    /// A hub over `dir/state`, its hub.toml read under `flags` and
    /// watched as `fw hub serve` does.
    async fn start(dir: &Path, flags: Flags) -> Self {
        flywheel_hub::install_crypto_provider();
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let config = Arc::new(ConfigWatch::open(&state, flags, Some(dir.to_path_buf())).unwrap());
        let effective = config.started().clone();
        let mut options = HubOptions::new(state.clone());
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_claude(dir).to_string_lossy().into_owned();
        options.claude_config_dir = Some(dir.join("claude-config"));
        options.scrollback_lines = effective.scrollback_lines;
        options.worktree_branch_prefix = effective.worktree_branch_prefix.clone();
        options.agent_defaults = effective.agent_defaults.clone();
        options.record_limits = effective.record_limits;
        options.share_policy = effective.share_policy;
        let hub = support::open_hub(options);
        config.attach(&hub, None).unwrap();
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        RuntimeInfo {
            pid: std::process::id(),
            listen: addr,
            public_urls: Vec::new(),
            started_at: None,
            log_file: None,
        }
        .write(&state)
        .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            hub,
            ServerOptions {
                config: Some(config),
                ..ServerOptions::default()
            },
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            dir: dir.to_path_buf(),
            addr,
            token,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    async fn stop(mut self) {
        RuntimeInfo::remove_if_owned(&self.dir.join("state"), std::process::id());
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = timeout(Duration::from_secs(10), server).await;
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .http
            .request(method, self.url(path))
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
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        body
    }

    async fn events(
        &self,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let mut request = format!("ws://{}/api/events?client=settings-test", self.addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws
    }

    async fn cli(&self, args: &[&str]) -> (i32, String, String) {
        cli_in(&self.dir.join("state"), args).await
    }
}

async fn cli_in(state: &Path, args: &[&str]) -> (i32, String, String) {
    let mut args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    args.push("--state-dir".into());
    args.push(state.to_string_lossy().into_owned());
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

/// Wait for an events message of `kind`.
async fn next_of(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    kind: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let message = timeout(left, ws.next())
            .await
            .unwrap_or_else(|_| panic!("no {kind} message"))
            .unwrap()
            .unwrap();
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value["type"] == kind {
                return value;
            }
        }
    }
}

fn value_of<'a>(config: &'a Value, key: &str) -> &'a Value {
    config["values"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == key)
        .unwrap_or_else(|| panic!("no {key}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_validate_conflicts_persist_and_push() {
    let dir = tempfile::tempdir().unwrap();
    let hub = TestHub::start(dir.path(), Flags::default()).await;
    let first = hub.get("/api/settings").await;
    assert_eq!(first["rev"], 0);
    assert_eq!(first["keymap"]["pane.zoom"], json!(["alt+z"]));
    assert!(
        first["prefs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "appearance.theme")
    );
    assert!(
        first["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "session.jump" && a["indexed"] == true)
    );
    // Fan-out's board and launcher, sharing and recording are rebindable
    // (unbound by default).
    for id in ["fanout.open", "fanout.new", "share.open", "record.toggle"] {
        assert!(
            first["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == id && a["defaults"] == json!([])),
            "{id}"
        );
    }
    assert!(
        first["rules"]["reserved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["key"] == "ctrl+w")
    );

    let mut events = hub.events().await;
    // A good change: canonicalized, saved, pushed.
    let (status, saved) = hub
        .call(
            reqwest::Method::PUT,
            "/api/settings",
            Some(json!({
                "rev": 0,
                "values": {"appearance.theme": "light", "terminal.scrollback": 20000},
                "keys": {"pane.zoom": ["Shift+Alt+M"], "pane.close": ["alt+b"]},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["rev"], 1);
    assert_eq!(saved["keymap"]["pane.zoom"], json!(["alt+shift+m"]));
    assert!(
        saved["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["key"] == "alt+b"),
        "readline's alt+b warns: {saved}"
    );
    let pushed = next_of(&mut events, "settings").await;
    assert_eq!(pushed["rev"], 1);

    // Stale writes are refused with the current settings.
    let (status, stale) = hub
        .call(
            reqwest::Method::PUT,
            "/api/settings",
            Some(json!({"rev": 0, "values": {"appearance.theme": "dark"}})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["settings"]["rev"], 1);

    // A conflict, a reserved key, a terminal key, a bad value: all named,
    // nothing saved.
    let (status, refused) = hub
        .call(
            reqwest::Method::PUT,
            "/api/settings",
            Some(json!({
                "rev": 1,
                "values": {"appearance.font_size": 99, "appearance.nope": 1},
                "keys": {
                    "tab.new": ["alt+n"],
                    "pane.split_right": ["ctrl+w"],
                    "pane.split_down": ["ctrl+c"],
                    "review.toggle": ["g"],
                },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let errors = refused["errors"].as_array().unwrap();
    let has = |action: &str, needle: &str| {
        errors
            .iter()
            .any(|e| e["action"] == action && e["message"].as_str().unwrap().contains(needle))
    };
    assert!(has("tab.new", "bound twice"), "{refused}");
    assert!(has("pane.split_right", "reserved"), "{refused}");
    assert!(has("pane.split_down", "terminal needs it"), "{refused}");
    assert!(has("review.toggle", "terminal needs it"), "{refused}");
    assert!(has("appearance.font_size", "9..=28"), "{refused}");
    assert!(has("appearance.nope", "not a setting"), "{refused}");
    assert_eq!(hub.get("/api/settings").await["rev"], 1);

    // The check endpoint (device-only keymaps) says the same, saving nothing.
    let (status, checked) = hub
        .call(
            reqwest::Method::POST,
            "/api/settings/keys/check",
            Some(json!({"keys": {"tab.new": ["alt+n"]}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        checked["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"].as_str().unwrap().contains("bound twice"))
    );

    // Resetting a key with null puts the default back.
    let (status, reset) = hub
        .call(
            reqwest::Method::PUT,
            "/api/settings",
            Some(json!({"rev": 1, "keys": {"pane.close": null}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert_eq!(reset["keymap"]["pane.close"], json!(["alt+x"]));
    assert!(reset["keys"].get("pane.close").is_none());
    hub.stop().await;

    // A restart keeps them (settings.json, 0600).
    let file = dir.path().join("state/settings.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let hub = TestHub::start(dir.path(), Flags::default()).await;
    let again = hub.get("/api/settings").await;
    assert_eq!(again["rev"], 2);
    assert_eq!(again["values"]["appearance.theme"], "light");
    assert_eq!(again["values"]["terminal.scrollback"], 20000);
    assert_eq!(again["keymap"]["pane.zoom"], json!(["alt+shift+m"]));
    hub.stop().await;
}

/// Wait until `/api/config` reaches revision `above` + 1 or more.
async fn config_after(hub: &TestHub, above: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let config = hub.get("/api/config").await;
        if config["rev"].as_u64().unwrap() > above {
            return config;
        }
        assert!(
            Instant::now() < deadline,
            "hub.toml was not re-read: {config}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The argv the stand-in claude was last started with.
async fn claude_args(dir: &Path, since: Option<std::time::SystemTime>) -> Vec<String> {
    let path = dir.join("claude.args");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(meta) = std::fs::metadata(&path)
            && since.is_none_or(|t| meta.modified().unwrap() > t)
            && let Ok(bytes) = std::fs::read(&path)
        {
            return bytes
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
        }
        assert!(Instant::now() < deadline, "claude never started");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_toml_precedence_hot_reload_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let toml = state.join("hub.toml");
    std::fs::write(
        &toml,
        "[server]\nport = 7655\n[sessions]\nscrollback_lines = 777\n[agents.claude]\nmodel = \"opus-from-file\"\n",
    )
    .unwrap();
    // `config check` with no hub: valid.
    let (code, out, _) = cli_in(&state, &["config", "check"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("ok (3 settings from the file)"), "{out}");
    // A --file that does not exist is a mistyped path, not "all defaults".
    let missing = dir.path().join("nope.toml");
    let (code, _, err) = cli_in(
        &state,
        &["config", "check", "--file", missing.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("no such file"), "{err}");
    // Offline show: the file over the defaults.
    let (code, out, _) = cli_in(&state, &["config", "show", "--json"]).await;
    assert_eq!(code, 0);
    let offline: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(offline["running"], false);
    assert_eq!(value_of(&offline, "server.port")["source"], "file");

    // A flag beats the file; the file beats the default.
    let hub = TestHub::start(
        dir.path(),
        Flags {
            scrollback_lines: Some(1234),
            ..Flags::default()
        },
    )
    .await;
    let config = hub.get("/api/config").await;
    assert_eq!(
        value_of(&config, "sessions.scrollback_lines")["value"],
        1234
    );
    assert_eq!(
        value_of(&config, "sessions.scrollback_lines")["source"],
        "flag"
    );
    assert_eq!(value_of(&config, "server.port")["value"], 7655);
    assert_eq!(value_of(&config, "server.port")["source"], "file");
    assert_eq!(value_of(&config, "server.bind")["source"], "default");
    assert_eq!(
        value_of(&config, "agents.claude.model")["value"],
        "opus-from-file"
    );
    let (code, out, _) = hub.cli(&["config", "show"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("sessions.scrollback_lines") && out.contains("flag"),
        "{out}"
    );

    // The file's agent default reaches a launch that names no model.
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let (_, workspace) = hub
        .call(
            reqwest::Method::POST,
            "/api/workspaces",
            Some(json!({"name": "w", "folders": [repo]})),
        )
        .await;
    let launch = |model: Option<&str>| {
        let mut body = json!({"workspace_id": workspace["id"], "kind": "claude"});
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        body
    };
    let (status, created) = hub
        .call(reqwest::Method::POST, "/api/sessions", Some(launch(None)))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let args = claude_args(dir.path(), None).await;
    assert_eq!(
        flag_value(&args, "--model").as_deref(),
        Some("opus-from-file")
    );
    assert_eq!(created["launch"]["model"], "opus-from-file");

    // Hot reload: a new default, pushed to the UI, in force for the next
    // launch; the port edit waits for a restart.
    let mut events = hub.events().await;
    let rev = config["rev"].as_u64().unwrap();
    let before = std::fs::metadata(dir.path().join("claude.args"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::write(
        &toml,
        "[server]\nport = 7656\n[agents.claude]\nmodel = \"sonnet-reloaded\"\npermission_mode = \"plan\"\n",
    )
    .unwrap();
    let reloaded = config_after(&hub, rev).await;
    assert_eq!(
        value_of(&reloaded, "agents.claude.model")["value"],
        "sonnet-reloaded"
    );
    assert_eq!(reloaded["pending_restart"], json!(["server.port"]));
    assert!(reloaded["file"]["error"].is_null());
    next_of(&mut events, "config").await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (status, _) = hub
        .call(reqwest::Method::POST, "/api/sessions", Some(launch(None)))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let args = claude_args(dir.path(), Some(before)).await;
    assert_eq!(
        flag_value(&args, "--model").as_deref(),
        Some("sonnet-reloaded")
    );
    assert_eq!(
        flag_value(&args, "--permission-mode").as_deref(),
        Some("plan")
    );
    // A launch that names a model keeps it.
    let before = std::fs::metadata(dir.path().join("claude.args"))
        .unwrap()
        .modified()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            "/api/sessions",
            Some(launch(Some("haiku-given"))),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let args = claude_args(dir.path(), Some(before)).await;
    assert_eq!(flag_value(&args, "--model").as_deref(), Some("haiku-given"));

    // An invalid edit: reported (API, CLI, push), the last good stays.
    let rev = reloaded["rev"].as_u64().unwrap();
    std::fs::write(&toml, "[agents.claude]\npermission_mode = \"yolo\"\n").unwrap();
    let broken = config_after(&hub, rev).await;
    let error = broken["file"]["error"].as_str().unwrap();
    assert!(error.contains("agents.claude"), "{error}");
    assert_eq!(
        value_of(&broken, "agents.claude.model")["value"],
        "sonnet-reloaded"
    );
    next_of(&mut events, "config").await;
    let (code, _, err) = hub.cli(&["config", "check"]).await;
    assert_eq!(code, 1);
    assert!(err.contains("yolo"), "{err}");
    let (_, out, _) = hub.cli(&["config", "show"]).await;
    assert!(out.contains("INVALID"), "{out}");
    // Unknown keys too.
    std::fs::write(&toml, "[sessions]\nscrollbak = 3\n").unwrap();
    let (code, _, err) = hub.cli(&["config", "check"]).await;
    assert_eq!(code, 1);
    assert!(err.contains("scrollbak"), "{err}");
    hub.stop().await;

    // A start refuses an invalid file.
    let e = ConfigWatch::open(&state, Flags::default(), None).unwrap_err();
    assert!(e.contains("scrollbak"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_toml_recording_caps_and_share_expiry_apply_live() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let toml = state.join("hub.toml");
    std::fs::write(
        &toml,
        "[recordings]\npart_cap = \"128KiB\"\ndir_cap = 1048576\n[shares]\ndefault_expiry = \"10m\"\nmax_expiry = \"2h\"\n",
    )
    .unwrap();
    // A flag beats the file (the longest expiry), the file the default.
    let hub = TestHub::start(
        dir.path(),
        Flags {
            share_max_expiry: Some(Duration::from_secs(3 * 3600)),
            ..Flags::default()
        },
    )
    .await;
    let config = hub.get("/api/config").await;
    for (key, value, source) in [
        ("recordings.part_cap", "128KiB", "file"),
        ("recordings.dir_cap", "1MiB", "file"),
        ("shares.default_expiry", "10m", "file"),
        ("shares.max_expiry", "3h", "flag"),
    ] {
        let entry = value_of(&config, key);
        assert_eq!(entry["value"], value, "{key}: {entry}");
        assert_eq!(entry["source"], source, "{key}: {entry}");
        assert_eq!(entry["apply"], "live", "{key}: {entry}");
    }
    let limits = &hub.get("/api/recordings").await["limits"];
    assert_eq!(limits["max_file_bytes"], 128 * 1024);
    assert_eq!(limits["max_total_bytes"], 1024 * 1024);
    let policy = &hub.get("/api/shares").await["policy"];
    assert_eq!(policy["default_expires_in"], 600);
    assert_eq!(policy["max_expires_in"], 3 * 3600);

    // Share links follow the policy: the default, and nothing longer
    // than the longest.
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let (_, workspace) = hub
        .call(
            reqwest::Method::POST,
            "/api/workspaces",
            Some(json!({"name": "w", "folders": [repo]})),
        )
        .await;
    let (status, session) = hub
        .call(
            reqwest::Method::POST,
            "/api/sessions",
            Some(json!({"workspace_id": workspace["id"], "kind": "shell"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let sid = session["id"].as_str().unwrap().to_owned();
    let lasts = |made: &Value| {
        made["share"]["expires"].as_u64().unwrap() - made["share"]["created"].as_u64().unwrap()
    };
    let (status, made) = hub
        .call(
            reqwest::Method::POST,
            "/api/shares",
            Some(json!({"session_id": sid})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(lasts(&made), 600);
    let (status, refused) = hub
        .call(
            reqwest::Method::POST,
            "/api/shares",
            Some(json!({"session_id": sid, "expires": "4h"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["error"].as_str().unwrap_or_default().contains("3h"),
        "{refused}"
    );

    // Hot reload: new caps in force for recordings, a new default for
    // links made from now on.
    let rev = config["rev"].as_u64().unwrap();
    std::fs::write(
        &toml,
        "[recordings]\npart_cap = \"256KiB\"\ndir_cap = \"2MiB\"\n[shares]\ndefault_expiry = \"20m\"\n",
    )
    .unwrap();
    let reloaded = config_after(&hub, rev).await;
    assert!(reloaded["file"]["error"].is_null(), "{reloaded}");
    assert!(
        reloaded["pending_restart"].as_array().unwrap().is_empty(),
        "{reloaded}"
    );
    let limits = &hub.get("/api/recordings").await["limits"];
    assert_eq!(limits["max_file_bytes"], 256 * 1024);
    assert_eq!(limits["max_total_bytes"], 2 * 1024 * 1024);
    let (status, made) = hub
        .call(
            reqwest::Method::POST,
            "/api/shares",
            Some(json!({"session_id": sid})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(lasts(&made), 1200);

    // Inconsistent or out-of-bounds caps: reported by key, the last good
    // settings stay.
    let rev = reloaded["rev"].as_u64().unwrap();
    std::fs::write(
        &toml,
        "[recordings]\npart_cap = \"1MiB\"\ndir_cap = \"512KiB\"\n[shares]\ndefault_expiry = \"3d\"\nmax_expiry = \"1d\"\n",
    )
    .unwrap();
    let broken = config_after(&hub, rev).await;
    let error = broken["file"]["error"].as_str().unwrap();
    assert!(error.contains("recordings.dir_cap"), "{error}");
    assert!(error.contains("shares.default_expiry"), "{error}");
    assert_eq!(
        hub.get("/api/recordings").await["limits"]["max_file_bytes"],
        256 * 1024
    );
    std::fs::write(&toml, "[recordings]\npart_cap = \"1KiB\"\n").unwrap();
    let (code, _, err) = hub.cli(&["config", "check"]).await;
    assert_eq!(code, 1);
    assert!(err.contains("recordings.part_cap"), "{err}");
    hub.stop().await;
}
