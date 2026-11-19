//! The event log end to end over a real hub on 127.0.0.1:0: hub
//! lifecycle events, label changes, and agent telemetry in one stream read
//! by cursor and filter (`GET /api/log`), a long poll woken by the event it
//! waits for, and the events socket resuming from a cursor instead of a
//! snapshot.

mod support;

use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::StreamExt;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

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

async fn types_until_resumed(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("the socket answers")
            .expect("open")
            .unwrap();
        let Message::Text(text) = next else { continue };
        let value: Value = serde_json::from_str(&text).unwrap();
        let done = matches!(value["type"].as_str(), Some("resumed" | "snapshot"));
        seen.push(value);
        if done {
            return seen;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_log_reads_by_cursor_waits_for_a_match_and_resumes_the_events_socket() {
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
    // Where the log is before anything happens.
    let start = t.ok(Method::GET, "/api/log?limit=1", None).await["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut ids = Vec::new();
    for (name, team) in [("a", "web"), ("b", "api")] {
        let made = t
            .ok(
                Method::POST,
                "/api/sessions",
                Some(json!({ "workspace_id": ws, "kind": "shell", "name": name,
                             "labels": { "team": team } })),
            )
            .await;
        ids.push(made["id"].as_str().unwrap().to_owned());
    }
    // Lifecycle and label events, named, in order, filtered.
    let created = t
        .ok(
            Method::GET,
            &format!("/api/log?after={start}&kind=session.created"),
            None,
        )
        .await;
    let names: Vec<&str> = created["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"], "{created}");
    assert_eq!(created["events"][0]["workspace"], "w");
    let web = t
        .ok(
            Method::GET,
            &format!("/api/log?after={start}&filter=team%3Dweb"),
            None,
        )
        .await;
    assert!(
        web["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["session"] == ids[0].as_str()),
        "{web}"
    );
    // A cursor from another log is refused.
    let (status, _) = t.call(Method::GET, "/api/log?after=deadbeef1", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A long poll from now answers when the event it waits for arrives.
    let now = t.ok(Method::GET, "/api/log?limit=1", None).await["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let waiting = {
        let url = format!(
            "http://{}/api/log?after={now}&wait=20&filter=kind%3Dlabels.changed%20team%3Dapi",
            t.addr
        );
        let request = t.http.get(url).bearer_auth(t.hub.token().expose());
        tokio::spawn(async move { request.send().await.unwrap().json::<Value>().await.unwrap() })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let asked = std::time::Instant::now();
    t.ok(
        Method::PATCH,
        &format!("/api/sessions/{}/labels", ids[1]),
        Some(json!({ "labels": { "area": "db" } })),
    )
    .await;
    let woke = waiting.await.unwrap();
    assert!(
        asked.elapsed() < std::time::Duration::from_secs(5),
        "woken, not timed out"
    );
    assert_eq!(woke["events"][0]["body"]["set"]["area"], "db", "{woke}");

    // The events socket: a snapshot with a cursor; back with that cursor,
    // only what changed since, then `resumed`.
    let mut socket = t.ws("/api/events?client=e2e").await;
    let first = types_until_resumed(&mut socket).await;
    let snapshot = first.last().unwrap();
    assert_eq!(snapshot["type"], "snapshot");
    let cursor = snapshot["cursor"].as_str().unwrap().to_owned();
    drop(socket);
    t.ok(
        Method::PATCH,
        &format!("/api/sessions/{}/labels", ids[0]),
        Some(json!({ "labels": { "area": "ui" } })),
    )
    .await;
    let mut socket = t
        .ws(&format!("/api/events?client=e2e&after={cursor}"))
        .await;
    let resumed = types_until_resumed(&mut socket).await;
    let types: Vec<&str> = resumed
        .iter()
        .map(|m| m["type"].as_str().unwrap())
        .collect();
    assert_eq!(types.last(), Some(&"resumed"), "{types:?}");
    assert!(!types.contains(&"snapshot"), "{types:?}");
    assert!(
        resumed
            .iter()
            .any(|m| m["type"] == "status" && m["session_id"] == ids[0].as_str()),
        "the changed session's status is sent: {resumed:?}"
    );
    assert!(
        !resumed.iter().any(|m| m["session_id"] == ids[1].as_str()),
        "nothing about the unchanged one: {resumed:?}"
    );
    // A cursor that is not this log's gets a snapshot.
    let mut socket = t.ws("/api/events?client=e2e&after=deadbeef1").await;
    let fresh = types_until_resumed(&mut socket).await;
    assert_eq!(fresh.last().unwrap()["type"], "snapshot");
}
