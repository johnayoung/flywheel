//! Multiple machines end to end: two real hubs on 127.0.0.1 (A local, B
//! remote; separate ports and state dirs). B mints a peer credential, A
//! registers B, lists B's sessions and attaches to one through its proxy
//! (HTTP and WebSockets), the peer scope is enforced on B, revoking the
//! peer cuts A off (open sockets included), and an https B behind a
//! self-signed certificate is reached only with its pinned fingerprint.

mod support;

use flywheel_hub::remote::{RuntimeInfo, ServeLock};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestHub {
    _dir: tempfile::TempDir,
    state_dir: PathBuf,
    repo: PathBuf,
    addr: SocketAddr,
    hub: Arc<Hub>,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start() -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let mut options = HubOptions::new(state_dir.clone());
        options.default_shell = vec!["/bin/sh".into()];
        let hub = support::open_hub(options);
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
            _dir: dir,
            state_dir,
            repo,
            addr,
            hub,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn token(&self) -> String {
        self.hub.token().expose().to_owned()
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// `method path` with `bearer` (the hub's token unless given).
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .http
            .request(method, self.url(path))
            .bearer_auth(bearer.map_or_else(|| self.token(), str::to_owned));
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

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(reqwest::Method::GET, path, None, None).await
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(reqwest::Method::POST, path, None, Some(body))
            .await
    }

    /// A workspace with one shell session; its id.
    async fn shell(&self) -> String {
        let (status, ws) = self
            .post(
                "/api/workspaces",
                json!({ "name": "remote-ws", "folders": [self.repo] }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{ws}");
        let (status, session) = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": ws["id"], "name": "bsh", "cols": 80, "rows": 24 }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        session["id"].as_str().unwrap().to_owned()
    }

    async fn ws(&self, path: &str, bearer: &str, origin: Option<&str>) -> Result<Ws, u16> {
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                Err(response.status().as_u16())
            }
            Err(e) => panic!("websocket: {e}"),
        }
    }

    /// Record this test server as the state dir's running hub (as `fw hub
    /// serve` does), so the CLI verbs reach it.
    fn as_running(&self) {
        assert!(ServeLock::acquire(&self.state_dir).unwrap().is_none());
        RuntimeInfo {
            pid: std::process::id(),
            listen: self.addr,
            public_urls: Vec::new(),
            started_at: None,
            log_file: None,
        }
        .write(&self.state_dir)
        .unwrap();
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

/// Read terminal output until `needle` shows (text frames are protocol).
async fn read_until(ws: &mut Ws, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    while !output.contains(needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                output.push_str(&String::from_utf8_lossy(&bytes))
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("no {needle:?} before {other:?}; saw {output:?}"),
        }
    }
    output
}

/// The close code the socket ends with.
async fn closed_with(ws: &mut Ws) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Close(frame)))) => {
                return frame.map_or(1005, |f| u16::from(f.code));
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("socket did not close cleanly: {other:?}"),
        }
    }
}

/// `fw hub <args>` in-process (blocking: run under spawn_blocking).
fn cli(args: Vec<String>) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = flywheel_hub::cli::run(&args, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

async fn run_cli(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    tokio::task::spawn_blocking(move || cli(args))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hub_drives_another_through_its_proxy() {
    let b = TestHub::start().await;
    let a = TestHub::start().await;
    let session = b.shell().await;

    // B mints a peer credential for A.
    let (status, issued) = b.post("/api/peers", json!({ "name": "hub-a" })).await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let credential = issued["credential"].as_str().unwrap().to_owned();
    assert!(credential.starts_with("fwpeer_"));

    // The peer scope on B: sessions yes, administration no.
    let peer = Some(credential.as_str());
    let (status, who) = b
        .call(reqwest::Method::GET, "/api/whoami", peer, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(who["scope"], "peer");
    assert_eq!(who["peer"]["name"], "hub-a");
    let (status, _) = b
        .call(reqwest::Method::GET, "/api/sessions", peer, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    for (method, path) in [
        (reqwest::Method::POST, "/api/token/rotate"),
        (reqwest::Method::POST, "/api/login-codes"),
        (reqwest::Method::GET, "/api/peers"),
        (reqwest::Method::POST, "/api/peers"),
        (reqwest::Method::DELETE, "/api/peers/hub-a"),
        (reqwest::Method::POST, "/api/workspaces/import"),
        (reqwest::Method::GET, "/api/code-workspaces"),
        (reqwest::Method::GET, "/api/machines"),
        (reqwest::Method::GET, "/m/x/api/sessions"),
    ] {
        let (status, body) = b
            .call(
                method.clone(),
                path,
                peer,
                Some(json!({ "name": "x", "path": "/etc" })),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }
    // Not a browser credential: no login, no app page.
    let login = b
        .http
        .get(b.url(&format!("/login?token={credential}")))
        .send()
        .await
        .unwrap();
    assert!(!login.headers().contains_key("set-cookie"));
    let page = b
        .http
        .get(b.url("/"))
        .bearer_auth(&credential)
        .send()
        .await
        .unwrap();
    assert!(page.headers().get("x-fw-hub-shell").is_none());
    assert!(!page.text().await.unwrap().contains("/assets/app.js"));
    let asset = b
        .http
        .get(b.url("/assets/app.js"))
        .bearer_auth(&credential)
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::UNAUTHORIZED);

    // A never stores a hub's token as a machine credential.
    let (status, refused) = a
        .post(
            "/api/machines",
            json!({ "name": "b", "url": b.base(), "credential": b.token() }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("token"),
        "{refused}"
    );
    assert!(!a.state_dir.join("machines.json").exists());

    // A registers B with the peer credential.
    let (status, view) = a
        .post(
            "/api/machines",
            json!({ "name": "b", "url": b.base(), "credential": credential }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    assert_eq!(view["status"]["state"], "online");
    assert_eq!(view["peer"]["name"], "hub-a");
    assert!(!view.to_string().contains(&credential));
    let (_, list) = a.get("/api/machines").await;
    assert!(!list.to_string().contains(&credential));
    let stored = std::fs::read_to_string(a.state_dir.join("machines.json")).unwrap();
    assert!(stored.contains(&credential) && !stored.contains(&b.token()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(a.state_dir.join("machines.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // B's sessions, through A, with A's own token only.
    let (status, sessions) = a.get("/m/b/api/sessions").await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    assert!(
        sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == session.as_str()),
        "{sessions}"
    );
    let (status, _) = a.get("/m/nope/api/sessions").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Only B's API: nothing else on B, nothing elsewhere.
    for path in ["/m/b/login", "/m/b/", "/m/b/healthz"] {
        let (status, body) = a.get(path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
    // Typing through the proxy, writes too.
    let (status, typed) = a
        .post(
            &format!("/m/b/api/sessions/{session}/input"),
            json!({ "data": "echo via-$((40+2))-rest\r" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{typed}");

    // Attach over A's WebSocket proxy and type.
    let token_a = a.token();
    let ws_path = format!("/m/b/api/sessions/{session}/ws");
    assert_eq!(
        a.ws(&ws_path, &token_a, Some("http://evil.example"))
            .await
            .err(),
        Some(403),
        "cross-origin upgrade refused"
    );
    assert_eq!(a.ws(&ws_path, "wrong", None).await.err(), Some(401));
    let mut term = a.ws(&ws_path, &token_a, None).await.unwrap();
    let first = timeout(Duration::from_secs(10), term.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(header) = first else {
        panic!("no snapshot header: {first:?}")
    };
    let header: Value = serde_json::from_str(header.as_str()).unwrap();
    assert_eq!(header["type"], "snapshot");
    read_until(&mut term, "via-42-rest").await;
    term.send(Message::Text(
        json!({ "type": "input", "data": "echo typed-$((6*7))-ok\r" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    read_until(&mut term, "typed-42-ok").await;
    term.send(Message::Binary(b"echo raw-$((7*6))-bin\r".to_vec().into()))
        .await
        .unwrap();
    read_until(&mut term, "raw-42-bin").await;

    // The events socket through the proxy.
    let mut events = a
        .ws("/m/b/api/events?client=machines-test", &token_a, None)
        .await
        .unwrap();
    let first = timeout(Duration::from_secs(10), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(snapshot) = first else {
        panic!("{first:?}")
    };
    let snapshot: Value = serde_json::from_str(snapshot.as_str()).unwrap();
    assert_eq!(snapshot["type"], "snapshot");

    // Rotating B's token leaves the peer (and A's sockets) alone.
    let (status, _) = b.post("/api/token/rotate", json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = a.get("/m/b/api/sessions").await;
    assert_eq!(status, StatusCode::OK);
    term.send(Message::Text(
        json!({ "type": "input", "data": "echo after-$((5*5))-rot\r" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    read_until(&mut term, "after-25-rot").await;

    // The CLI through A: `fw hub ls --machine b`, and the machine verbs.
    a.as_running();
    let state_a = a.state_dir.to_str().unwrap().to_owned();
    let (code, out, err) = run_cli(&["ls", "--machine", "b", "--state-dir", &state_a]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("remote-ws") && out.contains("bsh"), "{out}");
    let (code, out, err) = run_cli(&["machine", "ls", "--state-dir", &state_a]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains('b') && out.contains("online"), "{out}");
    let (code, out, err) =
        run_cli(&["read", &session, "--machine", "b", "--state-dir", &state_a]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("after-25-rot"), "{out}");

    // Revoking the peer on B cuts A off: the open sockets close (as a
    // machine refusal, never a sign-out of A), and calls are refused.
    let (status, _) = b
        .call(reqwest::Method::DELETE, "/api/peers/hub-a", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(closed_with(&mut term).await, 4010);
    assert_eq!(closed_with(&mut events).await, 4010);
    let (status, body) = a.get("/m/b/api/sessions").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["machine_state"], "refused");
    let (_, tested) = a.post("/api/machines/b/test", json!({})).await;
    assert_eq!(tested["ok"], false);
    assert_eq!(tested["machine"]["status"]["state"], "refused");
    let (code, _, _) = run_cli(&["machine", "test", "b", "--state-dir", &state_a]).await;
    assert_eq!(code, 1);
    // A's own sessions and login are untouched.
    let (status, _) = a.get("/api/sessions").await;
    assert_eq!(status, StatusCode::OK);

    // Re-pair by minting with B's token: only the minted credential is kept.
    let (status, _) = a
        .call(reqwest::Method::DELETE, "/api/machines/b", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, view) = a
        .post(
            "/api/machines",
            json!({ "name": "b", "url": b.base(), "token": b.token(), "peer_name": "hub-a2" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    assert_eq!(view["peer"]["name"], "hub-a2");
    let stored = std::fs::read_to_string(a.state_dir.join("machines.json")).unwrap();
    assert!(!stored.contains(&b.token()), "the token is never stored");
    let (_, peers) = b.get("/api/peers").await;
    assert!(peers.to_string().contains("hub-a2"));
    let (status, _) = a.get("/m/b/api/sessions").await;
    assert_eq!(status, StatusCode::OK);

    // Removing the machine revokes its credential on B, and closes the
    // proxied sockets it had open.
    let mut term = a.ws(&ws_path, &token_a, None).await.unwrap();
    let (code, out, err) = run_cli(&["machine", "rm", "b", "--state-dir", &state_a]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("revoked"), "{out}");
    let code = closed_with(&mut term).await;
    assert!(code == 4011 || code == 4010, "closed with {code}");
    let (_, peers) = b.get("/api/peers").await;
    assert_eq!(peers["peers"].as_array().unwrap().len(), 0, "{peers}");
    let (status, _) = a.get("/m/b/api/sessions").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_are_managed_with_the_cli_with_or_without_a_running_hub() {
    let b = TestHub::start().await;
    let state = b.state_dir.to_str().unwrap().to_owned();
    // Not running (no runtime file): the file is edited directly.
    let (code, out, err) =
        run_cli(&["peer", "issue", "--name", "laptop", "--state-dir", &state]).await;
    assert_eq!(code, 0, "{err}");
    let offline = out.trim().to_owned();
    assert!(offline.starts_with("fwpeer_"), "{out}");
    assert!(err.contains("shown once"), "{err}");
    // Running: through the hub, so its live store has it at once.
    b.as_running();
    let (code, out, err) = run_cli(&[
        "peer",
        "issue",
        "--name",
        "phone-hub",
        "--state-dir",
        &state,
    ])
    .await;
    assert_eq!(code, 0, "{err}");
    let live = out.trim().to_owned();
    let (status, who) = b
        .call(reqwest::Method::GET, "/api/whoami", Some(&live), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(who["peer"]["name"], "phone-hub");
    let (code, out, _) = run_cli(&["peer", "ls", "--state-dir", &state]).await;
    assert_eq!(code, 0);
    assert!(out.contains("phone-hub") && out.contains("laptop"), "{out}");
    assert!(!out.contains(&live[20..]), "never the secret");
    let (code, _, err) = run_cli(&[
        "peer",
        "issue",
        "--name",
        "phone-hub",
        "--state-dir",
        &state,
    ])
    .await;
    assert_eq!(code, 1);
    assert!(err.contains("exists"), "{err}");
    let (code, out, err) = run_cli(&["peer", "revoke", "phone-hub", "--state-dir", &state]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("revoked peer phone-hub"), "{out}");
    let (status, _) = b
        .call(reqwest::Method::GET, "/api/sessions", Some(&live), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A peer revokes itself (what `machine rm` asks), and nothing else.
    let (status, _) = b
        .call(reqwest::Method::DELETE, "/api/peer", None, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the token is not a peer");
    b.shutdown().await;
}

/// A TLS front for `backend` with a self-signed certificate (openssl);
/// returns its address and the certificate's SHA-256, or `None` without
/// openssl.
async fn tls_front(backend: SocketAddr, dir: &std::path::Path) -> Option<(SocketAddr, String)> {
    let run = |args: &[&str]| {
        std::process::Command::new("openssl")
            .args(args)
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    let made = run(&[
        "req",
        "-x509",
        "-newkey",
        "ec",
        "-pkeyopt",
        "ec_paramgen_curve:prime256v1",
        "-nodes",
        "-keyout",
        "key.pem",
        "-out",
        "cert.pem",
        "-days",
        "2",
        "-subj",
        "/CN=localhost",
        "-addext",
        "subjectAltName=IP:127.0.0.1",
    ]) && run(&[
        "x509", "-in", "cert.pem", "-outform", "der", "-out", "cert.der",
    ]) && run(&[
        "pkcs8", "-topk8", "-nocrypt", "-in", "key.pem", "-outform", "der", "-out", "key.der",
    ]);
    if !made {
        return None;
    }
    let cert = std::fs::read(dir.join("cert.der")).unwrap();
    let key = std::fs::read(dir.join("key.der")).unwrap();
    let fingerprint: String = Sha256::digest(&cert)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![rustls::pki_types::CertificateDer::from(cert)],
        rustls::pki_types::PrivateKeyDer::Pkcs8(key.into()),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut plain) = TcpStream::connect(backend).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
            });
        }
    });
    Some((addr, fingerprint))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_https_machine_needs_trusted_roots_or_its_pinned_fingerprint() {
    let b = TestHub::start().await;
    let a = TestHub::start().await;
    let certs = tempfile::tempdir().unwrap();
    let Some((front, fingerprint)) = tls_front(b.addr, certs.path()).await else {
        eprintln!("skipped: openssl is not installed");
        return;
    };
    let session = b.shell().await;
    let (_, issued) = b.post("/api/peers", json!({ "name": "hub-a" })).await;
    let credential = issued["credential"].as_str().unwrap().to_owned();
    let url = format!("https://127.0.0.1:{}", front.port());

    // Self-signed, nothing pinned: refused, and the refusal names the
    // fingerprint it saw.
    let (status, body) = a
        .post(
            "/api/machines",
            json!({ "name": "tls", "url": url, "credential": credential }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let shown = fingerprint
        .as_bytes()
        .chunks(2)
        .map(|p| String::from_utf8_lossy(p).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":");
    assert!(body["error"].as_str().unwrap().contains(&shown), "{body}");
    // A wrong pin: refused.
    let (status, _) = a
        .post(
            "/api/machines",
            json!({ "name": "tls", "url": url, "credential": credential, "fingerprint": "00".repeat(32) }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // The right pin (as openssl prints it): HTTP and WebSockets work.
    let (status, view) = a
        .post(
            "/api/machines",
            json!({ "name": "tls", "url": url, "credential": credential, "fingerprint": shown }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    let (status, sessions) = a.get("/m/tls/api/sessions").await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    let mut term = a
        .ws(
            &format!("/m/tls/api/sessions/{session}/ws"),
            &a.token(),
            None,
        )
        .await
        .unwrap();
    term.send(Message::Text(
        json!({ "type": "input", "data": "echo tls-$((9*9))-ok\r" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    read_until(&mut term, "tls-81-ok").await;
    drop(term);
    a.shutdown().await;
    b.shutdown().await;
}
