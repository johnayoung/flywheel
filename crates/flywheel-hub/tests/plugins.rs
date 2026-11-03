//! Plugins end to end: a real hub on 127.0.0.1:0 running the shipped
//! sample plugin (`crates/flywheel-hub/plugins/hello`) and a probe plugin
//! as real processes -- events delivered with the right JSON on stdin,
//! a disabled plugin never run, actions returning a toast and a paste
//! into a real PTY, the deadline killing a hung run's process group, the
//! environment scrubbed of the hub's secrets, and `fw hub plugin`.

mod support;

use flywheel_hub::plugins::{HostOptions, PluginHost};
use flywheel_hub::remote::RuntimeInfo;
use flywheel_hub::report::{self, Report};
use flywheel_hub::{HubOptions, ServerOptions, serve};
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const LEAKED_TOKEN: &str = "hub-token-must-not-leak-7d1e";
const LEAKED_DSN: &str = "postgres://user:pw-must-not-leak@db/x";

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

fn state(dir: &Path) -> PathBuf {
    dir.join("state")
}

/// Copy the shipped sample plugin into the state dir.
fn install_sample(state: &Path) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/hello");
    let to = state.join("plugins/hello");
    std::fs::create_dir_all(&to).unwrap();
    std::fs::copy(from.join("plugin.toml"), to.join("plugin.toml")).unwrap();
    support::write_script(
        &to.join("hello.sh"),
        &std::fs::read_to_string(from.join("hello.sh")).unwrap(),
    );
    to
}

/// A probe plugin: `env` dumps its environment and cwd, `slow` records
/// its pid and hangs, `toast` answers a toast (on session.removed).
fn install_probe(state: &Path) -> PathBuf {
    let dir = state.join("plugins/probe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        r#"description = "test probe"
command = ["./probe.sh"]
timeout_secs = 1
[[events]]
on = "session.removed"
command = ["./probe.sh", "toast"]
[[actions]]
id = "env"
title = "Dump the environment"
command = ["./probe.sh", "env"]
[[actions]]
id = "slow"
title = "Hang"
command = ["./probe.sh", "slow"]
[[actions]]
id = "bad"
title = "Answer garbage"
command = ["./probe.sh", "bad"]
"#,
    )
    .unwrap();
    support::write_script(
        &dir.join("probe.sh"),
        r#"#!/bin/sh
cat > /dev/null
case "$1" in
  env) env > "$FW_HUB_PLUGIN_DIR/env.txt"; pwd > "$FW_HUB_PLUGIN_DIR/cwd.txt"; echo '{"toast":"env dumped"}' ;;
  slow) echo $$ > "$FW_HUB_PLUGIN_DIR/slow.pid"; sleep 30 & echo $! > "$FW_HUB_PLUGIN_DIR/child.pid"; wait ;;
  toast) echo '{"toast":{"message":"a session went away","tone":"warn"}}' ;;
  bad) echo '{"toast": 12}' ;;
esac
"#,
    );
    dir
}

fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    support::write_script(
        &path,
        &format!(
            "#!/bin/sh\nenv > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\nexec cat\n",
            d = path.display()
        ),
    );
    path
}

impl TestHub {
    async fn start() -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo")).unwrap();
        let mut options = HubOptions::new(state(dir.path()));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = fake_claude(dir.path()).to_string_lossy().into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        let hub = support::open_hub(options);
        // The hub's environment carries secrets plugins must not see.
        let mut base_env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        base_env.push(("FW_HUB_TOKEN".into(), LEAKED_TOKEN.into()));
        base_env.push(("FLYWHEEL_PG_DSN".into(), LEAKED_DSN.into()));
        base_env.push(("FLYWHEEL_HUB_REPORT_SECRET".into(), LEAKED_TOKEN.into()));
        let plugins = Arc::new(PluginHost::new(
            &state(dir.path()),
            HostOptions {
                max_concurrent: 4,
                base_env,
                enabled: true,
            },
        ));
        plugins.watch(&hub);
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
        .write(&state(dir.path()))
        .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            hub,
            ServerOptions {
                plugins: Some(plugins),
                ..ServerOptions::default()
            },
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            dir,
            addr,
            token,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn state(&self) -> PathBuf {
        state(self.dir.path())
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

    async fn post(&self, path: &str, body: Value) -> Value {
        let (status, value) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(status.is_success(), "{path}: {status} {value}");
        value
    }

    async fn cli(&self, args: &[&str]) -> (i32, String, String) {
        let mut args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        args.push("--state-dir".into());
        args.push(self.state().to_string_lossy().into_owned());
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

    async fn stop(mut self) {
        RuntimeInfo::remove_if_owned(&self.state(), std::process::id());
        let _ = self.stop.take().unwrap().send(());
        let _ = timeout(Duration::from_secs(15), self.server.take().unwrap()).await;
    }
}

/// The sample plugin's recorded events.
fn recorded(plugin: &Path) -> Vec<Value> {
    std::fs::read_to_string(plugin.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Wait for a recorded event matching `pred`.
async fn wait_event(plugin: &Path, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(event) = recorded(plugin).into_iter().find(|e| pred(e)) {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "event never recorded; have {:?}",
            recorded(plugin)
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn fire(env: &HashMap<String, String>, event: &str, payload: Value) {
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

fn alive(pid: i32) -> bool {
    // Signal 0: the process exists (a zombie counts; ours are reaped).
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugins_receive_events_answer_actions_and_are_bounded() {
    let hub = TestHub::start().await;
    let hello = install_sample(&hub.state());
    let probe = install_probe(&hub.state());
    let repo = hub.dir.path().join("repo").canonicalize().unwrap();
    let workspace = hub
        .post(
            "/api/workspaces",
            json!({"name": "demo", "folders": [repo]}),
        )
        .await;
    let ws = workspace["id"].as_str().unwrap().to_owned();

    // Found, and disabled until enabled.
    let (_, listed) = hub.call(reqwest::Method::GET, "/api/plugins", None).await;
    let names: Vec<&str> = listed["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["hello", "probe"]);
    assert!(
        listed["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["enabled"] == false && p["valid"] == true)
    );

    // A disabled plugin is never run.
    hub.post(
        "/api/sessions",
        json!({"workspace_id": ws, "name": "quiet"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(recorded(&hello).is_empty());
    let (status, refused) = hub
        .call(
            reqwest::Method::POST,
            "/api/plugins/hello/actions/hello",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("disabled"));

    // Enabled from the UI's endpoint: the next session is an event.
    let (status, toggled) = hub
        .call(
            reqwest::Method::PUT,
            "/api/plugins/hello",
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{toggled}");
    assert_eq!(toggled["enabled"], true);
    let created = hub
        .post("/api/sessions", json!({"workspace_id": ws, "name": "loud"}))
        .await;
    let event = wait_event(&hello, |e| e["event"] == "session.created").await;
    assert_eq!(event["version"], 1);
    assert_eq!(event["type"], "event");
    assert_eq!(event["plugin"], "hello");
    assert_eq!(event["payload"]["session"]["id"], created["id"]);
    assert_eq!(event["payload"]["session"]["name"], "loud");
    assert_eq!(event["payload"]["session"]["kind"], "shell");
    assert_eq!(event["payload"]["session"]["workspace"], "demo");
    assert_eq!(
        recorded(&hello).len(),
        1,
        "the disabled-time session never ran"
    );

    // status.changed, filtered to blocked/done.
    let claude = hub
        .post(
            "/api/sessions",
            json!({"workspace_id": ws, "kind": "claude", "name": "fixer"}),
        )
        .await;
    let env_file = hub.dir.path().join("claude.env");
    let deadline = Instant::now() + Duration::from_secs(20);
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
    fire(&env, "UserPromptSubmit", json!({"prompt": "go"}));
    fire(
        &env,
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
    );
    let blocked = wait_event(&hello, |e| e["event"] == "status.changed").await;
    assert_eq!(blocked["payload"]["status"], "blocked");
    assert_eq!(blocked["payload"]["previous"], "working");
    assert_eq!(blocked["payload"]["session"]["id"], claude["id"]);
    assert_eq!(blocked["payload"]["session"]["kind"], "claude");
    fire(&env, "Stop", json!({}));
    wait_event(&hello, |e| {
        e["event"] == "status.changed" && e["payload"]["status"] == "done"
    })
    .await;
    assert!(
        !recorded(&hello)
            .iter()
            .any(|e| e["payload"]["status"] == "working"),
        "working is filtered out"
    );

    // An action's toast.
    let said = hub
        .post("/api/plugins/hello/actions/hello", json!({}))
        .await;
    assert_eq!(said["ok"], true, "{said}");
    assert_eq!(said["toast"]["message"], "hello from the hello plugin");
    assert_eq!(said["toast"]["tone"], "ok");
    // An action's paste, into a real shell.
    let (status, needs) = hub
        .call(
            reqwest::Method::POST,
            "/api/plugins/hello/actions/git-status",
            Some(json!({})),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a session action without one: {needs}"
    );
    let typed = hub
        .post(
            "/api/plugins/hello/actions/git-status",
            json!({"session_id": created["id"]}),
        )
        .await;
    assert_eq!(typed["ok"], true, "{typed}");
    assert_eq!(typed["paste"]["ran"], true);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, screen) = hub
            .call(
                reqwest::Method::GET,
                &format!("/api/sessions/{}/screen", created["id"].as_str().unwrap()),
                None,
            )
            .await;
        let text = screen["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains("git status --short") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "paste never reached the shell: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The probe, enabled from the CLI.
    let (code, out, err) = hub.cli(&["plugin", "enable", "probe"]).await;
    assert_eq!(code, 0, "{out}{err}");
    // A scrubbed environment, cwd the plugin dir.
    let deadline = Instant::now() + Duration::from_secs(5);
    let dumped = loop {
        let (status, body) = hub
            .call(
                reqwest::Method::POST,
                "/api/plugins/probe/actions/env",
                Some(json!({})),
            )
            .await;
        if status == StatusCode::OK {
            break body;
        }
        assert!(Instant::now() < deadline, "probe never enabled: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(dumped["toast"]["message"], "env dumped", "{dumped}");
    let env = std::fs::read_to_string(probe.join("env.txt")).unwrap();
    assert!(!env.contains(LEAKED_TOKEN), "{env}");
    assert!(!env.contains("pw-must-not-leak"), "{env}");
    assert!(!env.contains("FW_HUB_TOKEN"), "{env}");
    assert!(env.contains("FW_HUB_PLUGIN=probe"), "{env}");
    assert!(env.contains("FW_HUB_PLUGIN_TRIGGER=action:env"), "{env}");
    assert!(env.lines().any(|l| l.starts_with("PATH=")), "{env}");
    let cwd = std::fs::read_to_string(probe.join("cwd.txt")).unwrap();
    assert_eq!(
        Path::new(cwd.trim()).canonicalize().unwrap(),
        probe.canonicalize().unwrap()
    );

    // The deadline kills a hung run and its children.
    let started = Instant::now();
    let slow = hub.post("/api/plugins/probe/actions/slow", json!({})).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(slow["ok"], false);
    assert_eq!(slow["log"]["timed_out"], true);
    assert!(
        slow["log"]["error"]
            .as_str()
            .unwrap()
            .contains("timed out after 1s"),
        "{slow}"
    );
    for file in ["slow.pid", "child.pid"] {
        let pid: i32 = std::fs::read_to_string(probe.join(file))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(pid) {
            assert!(
                Instant::now() < deadline,
                "{file} {pid} survived the deadline"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // A malformed response is the run's error.
    let bad = hub.post("/api/plugins/probe/actions/bad", json!({})).await;
    assert_eq!(bad["ok"], false);
    assert!(
        bad["log"]["error"].as_str().unwrap().contains("toast"),
        "{bad}"
    );

    // An event hook's toast reaches every open UI.
    let mut request = format!("ws://{}/api/events?client=plugin-test", hub.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", hub.token).parse().unwrap(),
    );
    let (mut events, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}", created["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let toast = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(Ok(Message::Text(text))) = events.next().await {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                if value["type"] == "plugin_toast" {
                    return value;
                }
            }
        }
    })
    .await
    .expect("no plugin toast pushed");
    assert_eq!(toast["plugin"], "probe");
    assert_eq!(toast["message"], "a session went away");
    assert_eq!(toast["tone"], "warn");

    // Logs and listing from the CLI.
    let (code, out, _) = hub.cli(&["plugin", "logs", "probe"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("action:slow") && out.contains("timed out"),
        "{out}"
    );
    let (code, out, _) = hub.cli(&["plugin", "ls"]).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("hello  [enabled]") && out.contains("probe  [enabled]"),
        "{out}"
    );
    let (_, listed) = hub.call(reqwest::Method::GET, "/api/plugins", None).await;
    let probe_view = listed["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "probe")
        .unwrap()
        .clone();
    assert!(
        probe_view["last_error"]["error"].as_str().is_some(),
        "{probe_view}"
    );

    // Disabled again: events stop.
    let (code, _, _) = hub.cli(&["plugin", "disable", "hello"]).await;
    assert_eq!(code, 0);
    let before = recorded(&hello).len();
    hub.post(
        "/api/sessions",
        json!({"workspace_id": ws, "name": "after"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(recorded(&hello).len(), before);
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            "/api/plugins/hello/actions/hello",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    hub.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_manifests_are_listed_not_run() {
    let hub = TestHub::start().await;
    let dir = hub.state().join("plugins/broken");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "command = [\"./x\"]\n[[events]]\non = \"sesion.created\"\n",
    )
    .unwrap();
    let (_, listed) = hub.call(reqwest::Method::GET, "/api/plugins", None).await;
    let broken = &listed["plugins"][0];
    assert_eq!(broken["valid"], false);
    assert!(
        broken["error"].as_str().unwrap().contains("unknown event"),
        "{broken}"
    );
    let (code, out, _) = hub.cli(&["plugin", "ls"]).await;
    assert_eq!(code, 0);
    assert!(out.contains("broken  [invalid]"), "{out}");
    // Enabling it is allowed (it runs once fixed), running it is not.
    let (code, _, err) = hub.cli(&["plugin", "enable", "broken"]).await;
    assert_eq!(code, 0);
    assert!(err.contains("invalid"), "{err}");
    hub.stop().await;
}
