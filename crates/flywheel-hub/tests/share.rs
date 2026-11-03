//! Read-only share links end to end: a real hub on 127.0.0.1:0, real
//! HTTP and WebSockets, real PTYs. A link reaches its one session's
//! read-only stream, title, and status -- nothing else -- refuses input,
//! resizes, and pastes, never sizes the PTY or marks a turn seen, and
//! closes its open views on revoke and expiry.

mod support;

use flywheel_hub::report::{self, Report};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestHub {
    dir: tempfile::TempDir,
    hub: Arc<Hub>,
    addr: SocketAddr,
    token: String,
    cookie: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start() -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // A stand-in `claude`: dumps its environment, then echoes input.
        let claude = dir.path().join("claude");
        support::write_script(
            &claude,
            &format!(
                "#!/bin/sh\nenv > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\nexec cat\n",
                d = claude.display()
            ),
        );
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = claude.to_string_lossy().into_owned();
        options.report_command = Some(vec!["/opt/flywheel/bin/fw".into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        let hub = support::open_hub(options);
        let token = hub.token().expose().to_owned();
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
        let http = reqwest::Client::new();
        let login = http
            .get(format!("http://{addr}/login?token={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        Self {
            dir,
            hub,
            addr,
            token,
            cookie,
            http,
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo").canonicalize().unwrap()
    }

    fn authed(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .bearer_auth(&self.token)
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let response = self
            .authed(reqwest::Method::POST, path)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Value {
        self.authed(reqwest::Method::GET, path)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn session(&self, kind: &str) -> String {
        let (status, workspace) = self
            .post(
                "/api/workspaces",
                json!({ "name": format!("ws-{kind}"), "folders": [self.repo()] }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{workspace}");
        let (status, session) = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": workspace["id"], "kind": kind, "name": format!("{kind}-one"),
                        "cols": 80, "rows": 24 }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        session["id"].as_str().unwrap().to_owned()
    }

    async fn share(&self, session: &str, body: Value) -> Value {
        let mut body = body;
        body["session_id"] = json!(session);
        let (status, made) = self.post("/api/shares", body).await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        made
    }

    async fn ws(&self, path: &str, headers: &[(&'static str, String)]) -> Result<Ws, u16> {
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        for (name, value) in headers {
            request.headers_mut().insert(*name, value.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                Err(response.status().as_u16())
            }
            Err(e) => panic!("ws error: {e}"),
        }
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

/// The snapshot header and its bytes.
async fn snapshot(ws: &mut Ws) -> (Value, Vec<u8>) {
    let header = text_of(ws, "snapshot").await;
    let want = usize::try_from(header["bytes"].as_u64().unwrap()).unwrap();
    let mut bytes = Vec::new();
    while bytes.len() < want {
        match timeout(Duration::from_secs(30), ws.next()).await.unwrap() {
            Some(Ok(Message::Binary(b))) => bytes.extend_from_slice(&b),
            other => panic!("expected snapshot bytes: {other:?}"),
        }
    }
    (header, bytes)
}

/// The next text message of `kind` (output and other messages skipped).
async fn text_of(ws: &mut Ws, kind: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, ws.next()).await.expect("timed out") {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == kind {
                    return value;
                }
            }
            Some(Ok(_)) => {}
            other => panic!("socket ended waiting for {kind}: {other:?}"),
        }
    }
}

/// Read output until `needle`.
async fn output_until(ws: &mut Ws, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();
    while !seen.contains(needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, ws.next())
            .await
            .unwrap_or_else(|_| panic!("{needle:?} never came; saw {seen:?}"))
        {
            Some(Ok(Message::Binary(b))) => seen.push_str(&String::from_utf8_lossy(&b)),
            Some(Ok(_)) => {}
            other => panic!("socket ended waiting for {needle:?}: {other:?}"),
        }
    }
    seen
}

/// The close frame that ends the socket (within `within`).
async fn closed_with(ws: &mut Ws, within: Duration) -> (u16, String) {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, ws.next()).await.expect("never closed") {
            Some(Ok(Message::Close(Some(frame)))) => {
                return (u16::from(frame.code), frame.reason.to_string());
            }
            Some(Ok(Message::Close(None))) | None => return (0, String::new()),
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("socket error: {e}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_share_link_is_a_read_only_view_of_its_one_session() {
    let hub = TestHub::start().await;
    let sid = hub.session("shell").await;

    // Minting is validated; the credential and path come back once.
    for (body, want) in [
        (
            json!({ "session_id": sid, "expires_in": 5 }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "session_id": sid, "expires": "31d" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "session_id": sid, "expires": "1h", "expires_in": 60 }),
            StatusCode::BAD_REQUEST,
        ),
        (json!({ "session_id": "nope" }), StatusCode::NOT_FOUND),
        (
            json!({ "session_id": sid, "name": "\u{1b}[31m" }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let (status, answer) = hub.post("/api/shares", body.clone()).await;
        assert_eq!(status, want, "{body}: {answer}");
    }
    let made = hub
        .share(&sid, json!({ "expires": "10m", "name": "demo" }))
        .await;
    let credential = made["credential"].as_str().unwrap().to_owned();
    let path = made["path"].as_str().unwrap().to_owned();
    assert_eq!(path, format!("/share/{credential}"));
    assert!(credential.starts_with("fwview_"));
    assert_eq!(made["share"]["name"], "demo");
    // Digests only on disk.
    let on_disk = std::fs::read_to_string(hub.dir.path().join("state/shares.json")).unwrap();
    assert!(!on_disk.contains(credential.rsplit('_').next().unwrap()));
    // A label is unique among live links.
    let (status, _) = hub
        .post("/api/shares", json!({ "session_id": sid, "name": "demo" }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The viewer page and its own assets; nothing else of the app.
    let page = hub.http.get(hub.url(&path)).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["cache-control"], "no-store");
    let html = page.text().await.unwrap();
    assert!(html.contains(&format!("{path}/assets/viewer.js")), "{html}");
    for (asset, want) in [
        ("viewer.js", StatusCode::OK),
        ("term-view.js", StatusCode::OK),
        ("vendor/xterm/xterm.js", StatusCode::OK),
        ("app.js", StatusCode::FORBIDDEN),
        ("sharing.js", StatusCode::FORBIDDEN),
        ("settings.js", StatusCode::FORBIDDEN),
    ] {
        let response = hub
            .http
            .get(hub.url(&format!("{path}/assets/{asset}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), want, "{asset}");
    }
    for (method, tail) in [
        (reqwest::Method::POST, "/input"),
        (reqwest::Method::GET, "/screen"),
        (reqwest::Method::POST, "/paste"),
        (reqwest::Method::GET, "/api/sessions"),
        (reqwest::Method::GET, "/api/settings"),
        (reqwest::Method::GET, "/api/plugins"),
        (reqwest::Method::GET, "/api/config"),
    ] {
        let response = hub
            .http
            .request(method, hub.url(&format!("{path}{tail}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{tail}");
    }
    let info: Value = hub
        .http
        .get(hub.url(&format!("{path}/info")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["session"]["name"], "shell-one");
    assert_eq!(info["session"]["state"], "running");
    assert_eq!(info["share"]["name"], "demo");
    assert!(info["share"]["expires"].as_u64().unwrap() > 0);

    // As a bearer anywhere else: 403, never the API.
    for route in [
        "/api/sessions",
        &format!("/api/sessions/{sid}/screen"),
        "/api/shares",
        "/api/whoami",
        // Settings, plugins, hub.toml, and recordings are the operator's.
        "/api/settings",
        "/api/settings/keys/check",
        "/api/plugins",
        "/api/plugins/hello/logs",
        "/api/config",
        "/api/recordings",
        "/",
        "/assets/app.js",
        "/replay",
    ] {
        let response = hub
            .http
            .get(hub.url(route))
            .bearer_auth(&credential)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{route}");
    }
    let bearer = [("authorization", format!("Bearer {credential}"))];
    assert_eq!(
        hub.ws(&format!("/api/sessions/{sid}/ws"), &bearer)
            .await
            .err(),
        Some(403)
    );
    assert_eq!(hub.ws("/api/events", &bearer).await.err(), Some(403));
    // A cross-origin page cannot open the view.
    assert_eq!(
        hub.ws(
            &format!("{path}/ws"),
            &[("origin", "https://evil.example".to_owned())]
        )
        .await
        .err(),
        Some(403)
    );

    // The read-only stream: snapshot, then live output.
    let mut viewer = hub.ws(&format!("{path}/ws"), &[]).await.unwrap();
    let (header, _) = snapshot(&mut viewer).await;
    assert_eq!(
        (header["cols"].clone(), header["rows"].clone()),
        (json!(80), json!(24))
    );
    // Every kind of input is refused, and nothing reaches the terminal.
    let marker = hub.dir.path().join("pwned");
    let typed = format!("touch {}\r", marker.display());
    for message in [
        Message::Text(json!({ "type": "input", "data": typed }).to_string().into()),
        Message::Binary(typed.clone().into_bytes().into()),
        Message::Text(
            json!({ "type": "resize", "cols": 150, "rows": 50 })
                .to_string()
                .into(),
        ),
        Message::Text(
            json!({ "type": "theme", "foreground": "#ffffff", "background": "#000000",
                    "cursor": "#ffffff", "ansi": vec!["#000000"; 16] })
            .to_string()
            .into(),
        ),
    ] {
        viewer.send(message).await.unwrap();
        let error = text_of(&mut viewer, "error").await;
        assert!(
            error["message"].as_str().unwrap().contains("read-only"),
            "{error}"
        );
    }
    // The owner types; the viewer sees it.
    let (status, _) = hub
        .post(
            &format!("/api/sessions/{sid}/input"),
            json!({ "data": "echo shared-$((6*7))", "enter": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    output_until(&mut viewer, "shared-42").await;
    assert!(!marker.exists(), "a viewer's input reached the terminal");
    // Never sized the PTY; not counted among its clients.
    let session = hub.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(
        (
            session["live"]["cols"].clone(),
            session["live"]["rows"].clone()
        ),
        (json!(80), json!(24))
    );
    assert_eq!(session["live"]["clients"], 0, "{session}");
    assert_eq!(session["live"]["link_viewers"], 1, "{session}");
    let listed = hub.get("/api/shares").await;
    assert_eq!(listed["shares"][0]["viewers"], 1, "{listed}");
    assert_eq!(listed["shares"][0]["session"], "shell-one");
    // A viewer that does resize (the owner's browser) is followed.
    let mut owner = hub
        .ws(
            &format!("/api/sessions/{sid}/ws"),
            &[("cookie", hub.cookie.clone())],
        )
        .await
        .unwrap();
    snapshot(&mut owner).await;
    owner
        .send(Message::Text(
            json!({ "type": "resize", "cols": 100, "rows": 30 })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let size = text_of(&mut viewer, "size").await;
    assert_eq!(
        (size["cols"].clone(), size["rows"].clone()),
        (json!(100), json!(30))
    );
    let session = hub.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(session["live"]["clients"], 1, "{session}");

    // Revoking closes the open view at once; the link is dead.
    let id = made["share"]["id"].as_str().unwrap();
    let response = hub
        .authed(reqwest::Method::DELETE, &format!("/api/shares/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (code, reason) = closed_with(&mut viewer, Duration::from_secs(5)).await;
    assert_eq!(code, 4004, "{reason}");
    assert!(reason.contains("revoked"), "{reason}");
    for tail in ["", "/info"] {
        let response = hub
            .http
            .get(hub.url(&format!("{path}{tail}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{tail}");
    }
    assert_eq!(hub.ws(&format!("{path}/ws"), &[]).await.err(), Some(404));
    let session = hub.get(&format!("/api/sessions/{sid}")).await;
    assert!(session["live"].get("link_viewers").is_none(), "{session}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expiring_link_closes_its_views_and_then_answers_gone() {
    let hub = TestHub::start().await;
    let sid = hub.session("shell").await;
    // Two seconds (the API's floor is a minute; the store's is a second).
    let (info, credential) = hub
        .hub
        .shares()
        .issue(&sid, Duration::from_secs(2), None)
        .unwrap();
    let path = format!("/share/{credential}");
    let mut viewer = hub.ws(&format!("{path}/ws"), &[]).await.unwrap();
    snapshot(&mut viewer).await;
    let started = Instant::now();
    let (code, reason) = closed_with(&mut viewer, Duration::from_secs(10)).await;
    assert_eq!(code, 4004, "{reason}");
    assert!(reason.contains("expired"), "{reason}");
    assert!(started.elapsed() < Duration::from_secs(5));
    for tail in ["", "/info"] {
        let response = hub
            .http
            .get(hub.url(&format!("{path}{tail}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GONE, "{tail}");
    }
    assert!(
        hub.get("/api/shares").await["shares"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != info.id.as_str())
    );
    // Revoke-all ends every other link too.
    let (_, second) = hub
        .hub
        .shares()
        .issue(&sid, Duration::from_secs(600), Some("two"))
        .unwrap();
    let mut viewer = hub.ws(&format!("/share/{second}/ws"), &[]).await.unwrap();
    snapshot(&mut viewer).await;
    let response = hub
        .authed(reqwest::Method::DELETE, "/api/shares")
        .send()
        .await
        .unwrap();
    let revoked: Value = response.json().await.unwrap();
    assert_eq!(revoked["revoked"].as_array().unwrap().len(), 1, "{revoked}");
    assert_eq!(
        closed_with(&mut viewer, Duration::from_secs(5)).await.0,
        4004
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_share_viewer_never_marks_a_turn_seen() {
    let hub = TestHub::start().await;
    let sid = hub.session("claude").await;
    let session = hub.get(&format!("/api/sessions/{sid}")).await;
    let conversation = session["agent_session_id"].as_str().unwrap().to_owned();
    // The stand-in's environment carries its report socket and secret.
    let env_file = hub.dir.path().join("claude.env");
    let deadline = Instant::now() + Duration::from_secs(30);
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
    let fire = |event: &str| {
        report::send(
            Path::new(&env[report::SOCKET_ENV]),
            &Report {
                session: env[report::SESSION_ENV].clone(),
                secret: env[report::SECRET_ENV].clone(),
                source: "claude".into(),
                event: event.into(),
                detail: None,
                payload: Some(json!({ "session_id": conversation, "cwd": hub.repo() })),
            },
        )
        .unwrap();
    };
    fire("UserPromptSubmit");
    fire("Stop");
    let deadline = Instant::now() + Duration::from_secs(30);
    while agent_status(&hub, &sid).await != "done" {
        assert!(Instant::now() < deadline, "never done");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // Watched through a link for a while: still an unseen finished turn.
    let made = hub.share(&sid, json!({})).await;
    let path = made["path"].as_str().unwrap();
    let mut viewer = hub.ws(&format!("{path}/ws"), &[]).await.unwrap();
    snapshot(&mut viewer).await;
    let info: Value = hub
        .http
        .get(hub.url(&format!("{path}/info")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["session"]["agent"]["status"], "done", "{info}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(agent_status(&hub, &sid).await, "done");
    let mut events = hub
        .ws(
            "/api/events?client=phone",
            &[("cookie", hub.cookie.clone())],
        )
        .await
        .unwrap();
    let snapshot = text_of(&mut events, "snapshot").await;
    assert_eq!(snapshot["sessions"][&sid]["status"], "done", "{snapshot}");
    drop(viewer);
    hub.shutdown().await;
}

async fn agent_status(hub: &TestHub, sid: &str) -> Value {
    hub.get(&format!("/api/sessions/{sid}")).await["agent"]["status"].clone()
}

/// One raw HTTP/1.1 request on `stream`: the response head, lowercased.
async fn raw_head(stream: &mut TcpStream, path: &str) -> String {
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = timeout(Duration::from_secs(10), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 1, "closed mid-head: {}", String::from_utf8_lossy(&head));
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    // Drain the body.
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse().unwrap());
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await.unwrap();
    head
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_share_connection_authenticates_on_its_first_request() {
    let hub = TestHub::start().await;
    let sid = hub.session("shell").await;
    let made = hub.share(&sid, json!({})).await;
    let path = made["path"].as_str().unwrap();
    // A live link's connection joins the authenticated pool: kept alive
    // for a second request.
    let mut stream = TcpStream::connect(hub.addr).await.unwrap();
    let first = raw_head(&mut stream, &format!("{path}/info")).await;
    assert!(first.starts_with("http/1.1 200"), "{first}");
    assert!(!first.contains("connection: close"), "{first}");
    let second = raw_head(&mut stream, &format!("{path}/info")).await;
    assert!(second.starts_with("http/1.1 200"), "{second}");
    // A made-up one stays unauthenticated: answered, then closed.
    let forged = format!("/share/fwview_{}_{}", "0".repeat(12), "0".repeat(64));
    let mut stream = TcpStream::connect(hub.addr).await.unwrap();
    let refused = raw_head(&mut stream, &format!("{forged}/info")).await;
    assert!(refused.starts_with("http/1.1 404"), "{refused}");
    assert!(refused.contains("connection: close"), "{refused}");
    hub.shutdown().await;
}
