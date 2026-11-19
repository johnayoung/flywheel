//! Session labels end to end over a real hub on 127.0.0.1:0: labels given
//! at creation, the hub's `fw.*` keys, set and unset, conditional sets
//! (a claim: the first wins, the rest get 409 and change nothing), the
//! sessions list filtered by label, each change in the session's
//! telemetry log, and a broadcast routed by label.

mod support;

use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;

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
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        flywheel_hub::install_crypto_provider();
        let mut options = HubOptions::new(root.join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        let claude = root.join("bin/claude");
        support::write_script(&claude, "#!/bin/sh\nstty raw -echo\nexec cat > /dev/null\n");
        options.agents.claude = claude.to_string_lossy().into_owned();
        options.agents.codex = root.join("bin/no-codex").to_string_lossy().into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(root.join("claude-config"));
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

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
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

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.call(method, path, body).await;
        assert!(status.is_success(), "{path}: {status} {value}");
        value
    }

    async fn labelled(&self, label: &str) -> Vec<String> {
        let listed = self
            .ok(Method::GET, &format!("/api/sessions?label={label}"), None)
            .await;
        let mut names: Vec<String> = listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn labels_are_set_claimed_filtered_logged_and_route_a_broadcast() {
    let t = TestHub::start().await;
    let ws = t
        .ok(
            Method::POST,
            "/api/workspaces",
            Some(json!({ "name": "w", "folders": [t.root.join("repo")] })),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut ids = Vec::new();
    for (name, kind, team) in [
        ("a", "shell", "web"),
        ("b", "shell", "api"),
        ("c", "claude", "web"),
    ] {
        let made = t
            .ok(
                Method::POST,
                "/api/sessions",
                Some(json!({ "workspace_id": ws, "kind": kind, "name": name,
                             "labels": { "team": team } })),
            )
            .await;
        assert_eq!(made["labels"]["team"], team);
        assert_eq!(made["labels"]["fw.kind"], kind, "the hub's own key");
        ids.push(made["id"].as_str().unwrap().to_owned());
    }
    // fw.* keys and bad values are refused at creation and later.
    let (status, _) = t
        .call(
            Method::POST,
            "/api/sessions",
            Some(json!({ "workspace_id": ws, "labels": { "fw.pr": "1" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let a = format!("/api/sessions/{}/labels", ids[0]);
    let (status, _) = t
        .call(
            Method::PATCH,
            &a,
            Some(json!({ "labels": { "team": "a b" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    assert_eq!(t.labelled("team=web").await, ["a", "c"]);
    assert_eq!(t.labelled("team=web|api,fw.kind=shell").await, ["a", "b"]);
    assert_eq!(t.labelled("owner").await, Vec::<String>::new());

    // A claim: the first wins; the second is refused and changes nothing.
    let claim = |who: &str| json!({ "labels": { "owner": who }, "if_unset": true });
    let won = t.ok(Method::PATCH, &a, Some(claim("lead-1"))).await;
    assert_eq!(won["labels"]["owner"], "lead-1");
    let (status, lost) = t.call(Method::PATCH, &a, Some(claim("lead-2"))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{lost}");
    assert_eq!(lost["error"], "owner is already lead-1");
    // Handed on only by whoever holds it.
    let pass = json!({ "labels": { "owner": "lead-2" }, "if": "lead-1" });
    t.ok(Method::PATCH, &a, Some(pass.clone())).await;
    let (status, _) = t.call(Method::PATCH, &a, Some(pass)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Unset.
    let unset = t
        .ok(
            Method::PATCH,
            &a,
            Some(json!({ "labels": { "team": null } })),
        )
        .await;
    assert!(unset["labels"].get("team").is_none());
    assert_eq!(t.labelled("team=web").await, ["c"]);

    // Each change is in the session's telemetry log.
    t.hub.telemetry().flush();
    let events = t
        .ok(
            Method::GET,
            &format!("/api/telemetry/sessions/{}/events", ids[0]),
            None,
        )
        .await;
    let changes: Vec<&Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "labels.changed")
        .collect();
    assert_eq!(changes.len(), 4, "{events}");
    assert_eq!(changes[0]["body"]["set"]["team"], "web");
    assert_eq!(changes[2]["body"]["set"]["owner"], "lead-2");
    assert_eq!(changes[2]["body"]["before"]["owner"], "lead-1");
    assert_eq!(changes[3]["body"]["set"]["team"], Value::Null);

    // A broadcast by label reaches the labelled agents only.
    let sent = t
        .ok(
            Method::POST,
            "/api/broadcast",
            Some(json!({ "text": "rebase", "label": "team=web" })),
        )
        .await;
    let results = sent["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{sent}");
    assert_eq!(results[0]["session_id"], ids[2]);
    assert_ne!(results[0]["result"], "refused", "{sent}");
}
