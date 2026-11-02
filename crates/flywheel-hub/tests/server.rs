//! End-to-end: a real hub server on 127.0.0.1:0, real HTTP, real
//! WebSockets, real PTYs running `/bin/sh`.

use flywheel_hub::vt::{Emulator, TextSource};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use reqwest::header::{COOKIE, ORIGIN, SET_COOKIE};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
    token: String,
    /// `name=value` of a browser login made at start.
    cookie: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        Self::start_in(dir, state_dir, repo).await
    }

    async fn start_in(dir: tempfile::TempDir, state_dir: PathBuf, repo: PathBuf) -> Self {
        Self::start_with(dir, state_dir, repo, |_| {}).await
    }

    async fn start_with(
        dir: tempfile::TempDir,
        state_dir: PathBuf,
        repo: PathBuf,
        configure: impl FnOnce(&mut HubOptions),
    ) -> Self {
        let mut options = HubOptions::new(state_dir.clone());
        options.default_shell = vec!["/bin/sh".into()];
        configure(&mut options);
        let hub = Hub::open(options).unwrap();
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
            let _ = stopped.await;
        }));
        let http = reqwest::Client::new();
        let login = http
            .get(format!("http://{addr}/login?token={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        Self {
            _dir: dir,
            state_dir,
            repo,
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

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn cookie(&self) -> String {
        self.cookie.clone()
    }

    fn authed(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .bearer_auth(&self.token)
    }

    async fn create_workspace(&self, name: &str) -> Value {
        let response = self
            .authed(reqwest::Method::POST, "/api/workspaces")
            .json(&json!({ "name": name, "repos": [self.repo] }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json().await.unwrap()
    }

    async fn create_session(&self, workspace_id: &str, command: Option<Vec<&str>>) -> Value {
        let mut body = json!({ "workspace_id": workspace_id, "cols": 80, "rows": 24 });
        if let Some(command) = command {
            body["command"] = json!(command);
        }
        let response = self
            .authed(reqwest::Method::POST, "/api/sessions")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json().await.unwrap()
    }

    async fn connect(
        &self,
        session_id: &str,
        origin: Option<&str>,
        cookie: bool,
    ) -> Result<Ws, u16> {
        let url = format!("ws://{}/api/sessions/{session_id}/ws", self.addr);
        let mut request = url.into_client_request().unwrap();
        if cookie {
            request
                .headers_mut()
                .insert("cookie", self.cookie().parse().unwrap());
        }
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
            Err(e) => panic!("unexpected ws error: {e}"),
        }
    }

    async fn shutdown(mut self) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        (self._dir, self.state_dir, self.repo)
    }
}

/// Read frames until the accumulated terminal output contains `needle`.
/// Returns (output, control messages).
async fn read_until(ws: &mut Ws, needle: &str) -> (String, Vec<Value>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut output = String::new();
    let mut control = Vec::new();
    while !output.contains(needle) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = timeout(remaining, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; saw {output:?}"));
        match frame {
            Some(Ok(Message::Binary(bytes))) => output.push_str(&String::from_utf8_lossy(&bytes)),
            Some(Ok(Message::Text(text))) => control.push(serde_json::from_str(&text).unwrap()),
            Some(Ok(_)) => {}
            other => panic!("socket ended waiting for {needle:?}: {other:?}; saw {output:?}"),
        }
    }
    (output, control)
}

/// Read frames until a control message of `kind` arrives.
async fn read_control(ws: &mut Ws, kind: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, ws.next()).await.expect("timed out") {
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

async fn input(ws: &mut Ws, data: &str) {
    ws.send(Message::Text(
        json!({ "type": "input", "data": data }).to_string().into(),
    ))
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn everything_but_login_requires_the_token() {
    let hub = TestHub::start().await;
    let get = |path: &str| hub.http.get(hub.url(path)).send();

    assert_eq!(get("/healthz").await.unwrap().status(), StatusCode::OK);
    for path in [
        "/api/workspaces",
        "/api/sessions",
        "/api/kinds",
        "/assets/app.js",
        "/assets/app.css",
        "/assets/vendor/xterm/xterm.js",
    ] {
        assert_eq!(
            get(path).await.unwrap().status(),
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    let bad = hub
        .http
        .get(hub.url("/api/workspaces"))
        .bearer_auth("0".repeat(64))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);

    // Unauthenticated `/` is the login page, not the app.
    let page = get("/").await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(csp.contains("frame-ancestors 'none'"));
    // `no-referrer` would make browsers send `Origin: null` on the login
    // form post, which the origin guard refuses.
    assert_eq!(page.headers()["referrer-policy"], "same-origin");
    let body = page.text().await.unwrap();
    assert!(body.contains("action=\"/login\""));
    assert!(!body.contains("xterm.js"));

    // The login page's fonts and icon are public.
    for path in [
        "/assets/icon.svg",
        "/assets/vendor/fonts/martian-mono.woff2",
    ] {
        assert_eq!(get(path).await.unwrap().status(), StatusCode::OK, "{path}");
    }

    // A WebSocket without credentials is refused before upgrade.
    assert_eq!(hub.connect("whatever", None, false).await.unwrap_err(), 401);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_trades_the_token_for_a_strict_http_only_cookie() {
    let hub = TestHub::start().await;
    let wrong = hub
        .http
        .get(hub.url("/login?token=nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert!(wrong.headers().get(SET_COOKIE).is_none());

    let ok = hub
        .http
        .get(hub.url(&format!("/login?token={}", hub.token)))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let cookie = ok.headers()[SET_COOKIE].to_str().unwrap().to_owned();
    let name = flywheel_hub::auth::cookie_name(&hub.token);
    assert!(name.starts_with("fw_hub_"), "{name}");
    // The cookie carries a per-login secret, never the token.
    assert!(cookie.starts_with(&format!("{name}=")), "{cookie}");
    assert!(!cookie.contains(&hub.token), "{cookie}");
    assert_ne!(
        cookie.split(';').next().unwrap(),
        hub.cookie(),
        "one per login"
    );
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    assert_eq!(ok.headers()["cache-control"], "no-store");
    assert!(ok.text().await.unwrap().contains("url=/"));

    // The form login works the same way.
    let form = hub
        .http
        .post(hub.url("/login"))
        .header(ORIGIN, hub.origin())
        .form(&[("token", hub.token.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), StatusCode::OK);
    assert!(form.headers().get(SET_COOKIE).is_some());

    // The cookie opens the app and its assets.
    let app = hub
        .http
        .get(hub.url("/"))
        .header(COOKIE, hub.cookie())
        .send()
        .await
        .unwrap();
    assert!(
        app.text()
            .await
            .unwrap()
            .contains("/assets/vendor/xterm/xterm.js")
    );
    let js = hub
        .http
        .get(hub.url("/assets/vendor/xterm/xterm.js"))
        .header(COOKIE, hub.cookie())
        .send()
        .await
        .unwrap();
    assert_eq!(js.status(), StatusCode::OK);
    assert!(
        js.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    let missing = hub
        .http
        .get(hub.url("/assets/nope.js"))
        .header(COOKIE, hub.cookie())
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let logout = hub
        .http
        .post(hub.url("/logout"))
        .header(ORIGIN, hub.origin())
        .header(COOKIE, hub.cookie())
        .send()
        .await
        .unwrap();
    assert!(
        logout.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    // Logging out ends that login server side (a copied cookie is dead);
    // other logins are unaffected.
    let after = hub
        .http
        .get(hub.url("/api/workspaces"))
        .header(COOKIE, hub.cookie())
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    let other = hub
        .http
        .get(hub.url("/api/workspaces"))
        .header(COOKIE, cookie.split(';').next().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::OK);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_crud_validation_and_cross_origin_refusal() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("alpha").await;
    let id = ws["id"].as_str().unwrap().to_owned();
    assert_eq!(ws["name"], "alpha");
    assert_eq!(
        ws["repos"][0].as_str().unwrap(),
        hub.repo.canonicalize().unwrap().to_str().unwrap()
    );

    let list: Value = hub
        .authed(reqwest::Method::GET, "/api/workspaces")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["workspaces"].as_array().unwrap().len(), 1);

    let patched = hub
        .authed(reqwest::Method::PATCH, &format!("/api/workspaces/{id}"))
        .json(&json!({ "name": "beta" }))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);
    let got: Value = hub
        .authed(reqwest::Method::GET, &format!("/api/workspaces/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["name"], "beta");

    let status_of = |body: Value| {
        let request = hub
            .authed(reqwest::Method::POST, "/api/workspaces")
            .json(&body);
        async move { request.send().await.unwrap().status() }
    };
    assert_eq!(
        status_of(json!({ "name": "x", "repos": [] })).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status_of(json!({ "name": "x", "repos": ["/definitely/not/here"] })).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status_of(json!({ "name": "beta", "repos": [hub.repo] })).await,
        StatusCode::CONFLICT
    );

    // A cross-origin browser request may not change state, even with the
    // credential; same-origin may.
    let evil = hub
        .authed(reqwest::Method::DELETE, &format!("/api/workspaces/{id}"))
        .header(ORIGIN, "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(evil.status(), StatusCode::FORBIDDEN);
    let same = hub
        .authed(reqwest::Method::DELETE, &format!("/api/workspaces/{id}"))
        .header(ORIGIN, hub.origin())
        .send()
        .await
        .unwrap();
    assert_eq!(same.status(), StatusCode::NO_CONTENT);
    let gone = hub
        .authed(reqwest::Method::GET, &format!("/api/workspaces/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_session_over_websocket_replays_then_streams_live() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("term").await;
    let workspace_id = ws["id"].as_str().unwrap();
    let session = hub.create_session(workspace_id, None).await;
    let sid = session["id"].as_str().unwrap().to_owned();
    assert_eq!(session["state"], "running");
    assert_eq!(session["command"], json!(["/bin/sh"]));
    assert!(session["live"]["pid"].as_u64().is_some());

    // Cross-origin upgrades are refused; same-origin is accepted.
    assert_eq!(
        hub.connect(&sid, Some("http://evil.example"), true)
            .await
            .unwrap_err(),
        403
    );
    let origin = hub.origin();
    let mut first = hub.connect(&sid, Some(&origin), true).await.unwrap();

    // Type into the shell; see the evaluated output (not just the echo).
    input(&mut first, "echo hub-$((6*7)); pwd\r").await;
    let (output, _) = read_until(&mut first, "hub-42").await;
    assert!(output.contains("hub-42"), "{output}");

    // A second client attaching later replays what already happened, then
    // both see new output live.
    let mut second = hub.connect(&sid, Some(&origin), true).await.unwrap();
    let (replay, _) = read_until(&mut second, "hub-42").await;
    assert!(replay.contains(hub.repo.file_name().unwrap().to_str().unwrap()));
    let listed: Value = hub
        .authed(reqwest::Method::GET, &format!("/api/sessions/{sid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["live"]["clients"], 2);

    // Resize from one client is visible to the child.
    second
        .send(Message::Text(
            json!({ "type": "resize", "cols": 101, "rows": 41 })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    // Give the resize a moment to land before asking the tty.
    tokio::time::sleep(Duration::from_millis(100)).await;
    input(&mut second, "stty size\r").await;
    read_until(&mut first, "41 101").await;
    read_until(&mut second, "41 101").await;

    // Raw binary frames are input too.
    first
        .send(Message::Binary(b"echo bin-$((2+3))\r".to_vec().into()))
        .await
        .unwrap();
    read_until(&mut second, "bin-5").await;

    // A malformed control message is answered, not fatal.
    first
        .send(Message::Text("{\"type\":\"bogus\"}".into()))
        .await
        .unwrap();
    assert_eq!(read_control(&mut first, "error").await["type"], "error");

    // Kill: every client learns the exit; the record keeps the status.
    let kill = hub
        .authed(reqwest::Method::POST, &format!("/api/sessions/{sid}/kill"))
        .send()
        .await
        .unwrap();
    assert_eq!(kill.status(), StatusCode::ACCEPTED);
    let exit = read_control(&mut first, "exit").await;
    assert!(
        exit["signal"].is_string() || exit["code"].is_number(),
        "{exit}"
    );
    read_control(&mut second, "exit").await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let view: Value = hub
            .authed(reqwest::Method::GET, &format!("/api/sessions/{sid}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if view["state"] == "exited" {
            assert!(view["live"]["pid"].is_null());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session never recorded exited: {view}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Nothing is left to kill: a no-op is a conflict, not 202.
    let again = hub
        .authed(reqwest::Method::POST, &format!("/api/sessions/{sid}/kill"))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);

    // An exited session still replays its scrollback and the exit.
    let mut late = hub.connect(&sid, Some(&origin), true).await.unwrap();
    let (replay, _) = read_until(&mut late, "bin-5").await;
    assert!(replay.contains("hub-42"));
    read_control(&mut late, "exit").await;

    // Delete removes the record and the terminal.
    let deleted = hub
        .authed(reqwest::Method::DELETE, &format!("/api/sessions/{sid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(hub.connect(&sid, None, true).await.unwrap_err(), 404);
    let list: Value = hub
        .authed(reqwest::Method::GET, "/api/sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list["sessions"].as_array().unwrap().is_empty());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_requests_are_validated() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("v").await;
    let workspace_id = ws["id"].as_str().unwrap();
    let post = |body: Value| {
        let request = hub
            .authed(reqwest::Method::POST, "/api/sessions")
            .json(&body);
        async move { request.send().await.unwrap().status() }
    };
    assert_eq!(
        post(json!({ "workspace_id": "missing" })).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(json!({ "workspace_id": workspace_id, "repo": "/tmp" })).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(json!({ "workspace_id": workspace_id, "command": [] })).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(json!({ "workspace_id": workspace_id, "command": ["/no/such/program"] })).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );

    // A workspace with a running session cannot be deleted.
    let session = hub
        .create_session(workspace_id, Some(vec!["/bin/sh", "-c", "sleep 30"]))
        .await;
    let refused = hub
        .authed(
            reqwest::Method::DELETE,
            &format!("/api/workspaces/{workspace_id}"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let filtered: Value = hub
        .authed(
            reqwest::Method::GET,
            &format!("/api/sessions?workspace_id={workspace_id}"),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(filtered["sessions"][0]["id"], session["id"]);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_hangs_up_sessions_and_state_survives_restart() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("persist").await;
    let workspace_id = ws["id"].as_str().unwrap().to_owned();
    let session = hub
        .create_session(
            &workspace_id,
            Some(vec!["/bin/sh", "-c", "echo $$; sleep 300"]),
        )
        .await;
    let pid = session["live"]["pid"].as_u64().unwrap() as i32;
    let sid = session["id"].as_str().unwrap().to_owned();
    // Keep a client attached: shutdown must not wait on it forever.
    let mut client = hub.connect(&sid, None, true).await.unwrap();
    read_until(&mut client, &pid.to_string()).await;

    let (dir, state_dir, repo) = hub.shutdown().await;
    assert!(
        !process_alive(pid),
        "session process {pid} survived shutdown"
    );

    let restarted = TestHub::start_in(dir, state_dir, repo).await;
    let list: Value = restarted
        .authed(reqwest::Method::GET, "/api/sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let record = &list["sessions"][0];
    assert_eq!(record["id"], sid.as_str());
    // Recorded as stopped by the hub, not as killed by its own hangup.
    assert_eq!(record["state"], "stopped", "{record}");
    assert!(record["live"].is_null());
    let workspaces: Value = restarted
        .authed(reqwest::Method::GET, "/api/workspaces")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(workspaces["workspaces"][0]["id"], workspace_id.as_str());
    // The token persisted too: the same credential still works.
    assert_eq!(
        std::fs::read_to_string(state_path(&restarted.state_dir, "token"))
            .unwrap()
            .trim(),
        restarted.token
    );
    restarted.shutdown().await;
}

fn state_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(name)
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_close_completes_the_closing_handshake() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("close").await;
    let session = hub
        .create_session(
            ws["id"].as_str().unwrap(),
            Some(vec!["/bin/sh", "-c", "echo up; sleep 30"]),
        )
        .await;
    let mut client = hub
        .connect(session["id"].as_str().unwrap(), None, true)
        .await
        .unwrap();
    read_until(&mut client, "up").await;
    client.close(None).await.unwrap();
    // The server answers with its own Close frame, then ends the stream.
    let mut saw_close = false;
    while let Some(frame) = timeout(Duration::from_secs(10), client.next())
        .await
        .expect("server answered the close")
    {
        match frame {
            Ok(Message::Close(_)) => saw_close = true,
            Ok(_) => {}
            Err(e) => panic!("unclean close: {e}"),
        }
    }
    assert!(saw_close, "server never replied with a Close frame");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_workspace_frees_its_exited_sessions() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("ended").await;
    let workspace_id = ws["id"].as_str().unwrap().to_owned();
    let session = hub
        .create_session(&workspace_id, Some(vec!["/bin/sh", "-c", "echo bye"]))
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let view: Value = hub
            .authed(reqwest::Method::GET, &format!("/api/sessions/{sid}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if view["state"] == "exited" {
            break;
        }
        assert!(Instant::now() < deadline, "never exited: {view}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The exited terminal is still attachable before the delete...
    drop(hub.connect(&sid, None, true).await.unwrap());
    let deleted = hub
        .authed(
            reqwest::Method::DELETE,
            &format!("/api/workspaces/{workspace_id}"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    // ...and gone with it afterwards, not pinned in the live map.
    assert_eq!(hub.connect(&sid, None, true).await.unwrap_err(), 404);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_errors_are_json_and_edge_inputs_are_handled() {
    let hub = TestHub::start().await;
    let other = hub.repo.parent().unwrap().join("repo2");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    let created: Value = hub
        .authed(reqwest::Method::POST, "/api/workspaces")
        .json(&json!({ "name": "two", "repos": [hub.repo, other] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let workspace_id = created["id"].as_str().unwrap().to_owned();

    // Body rejections answer in the same {"error": ...} JSON as the rest.
    for (body, status) in [
        ("garbage", StatusCode::BAD_REQUEST),
        (
            r#"{"workspace_id":"x","cols":100000}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let response = hub
            .authed(reqwest::Method::POST, "/api/sessions")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        let error: Value = response.json().await.expect("JSON error body");
        assert!(error["error"].as_str().is_some_and(|e| !e.is_empty()));
    }

    // A zero size means "default", not a 1x1 terminal.
    let zero: Value = hub
        .authed(reqwest::Method::POST, "/api/sessions")
        .json(&json!({ "workspace_id": workspace_id, "cols": 0, "rows": 0,
                       "command": ["/bin/sh", "-c", "sleep 30"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(zero["live"]["cols"], 120);
    assert_eq!(zero["live"]["rows"], 32);

    // The running session's repo cannot be dropped from the workspace.
    let patch = |repos: Value| {
        let request = hub
            .authed(
                reqwest::Method::PATCH,
                &format!("/api/workspaces/{workspace_id}"),
            )
            .json(&json!({ "repos": repos }));
        async move { request.send().await.unwrap().status() }
    };
    assert_eq!(patch(json!([other])).await, StatusCode::CONFLICT);
    assert_eq!(patch(json!([hub.repo])).await, StatusCode::OK);
    hub.shutdown().await;
}

/// Write an executable script standing in for an agent CLI.
fn fake_agent(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_sessions_launch_their_cli_and_report_availability() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = fake_agent(
        &bin,
        "claude",
        r#"echo "fake-claude in $(pwd) args=[$*] session=$FLYWHEEL_HUB_SESSION"; exec cat"#,
    );
    let missing_codex = bin.join("codex-not-installed");
    let claude_path = claude.clone();
    let hub = TestHub::start_with(dir, state_dir, repo, move |options| {
        options.agents.claude = claude_path.to_string_lossy().into_owned();
        options.agents.codex = missing_codex.to_string_lossy().into_owned();
    })
    .await;

    let kinds: Value = hub
        .authed(reqwest::Method::GET, "/api/kinds")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kinds = kinds["kinds"].as_array().unwrap();
    let by_kind = |k: &str| kinds.iter().find(|v| v["kind"] == k).unwrap().clone();
    assert_eq!(by_kind("shell")["available"], true);
    assert_eq!(by_kind("shell")["program"], "/bin/sh");
    assert_eq!(by_kind("claude")["available"], true);
    assert_eq!(by_kind("claude")["program"], claude.to_str().unwrap());
    assert_eq!(by_kind("codex")["available"], false);
    assert!(by_kind("codex")["program"].is_null());

    let workspace = hub.create_workspace("agents").await;
    let workspace_id = workspace["id"].as_str().unwrap();
    let response = hub
        .authed(reqwest::Method::POST, "/api/sessions")
        .json(&json!({ "workspace_id": workspace_id, "kind": "claude", "cols": 90, "rows": 20 }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let session: Value = response.json().await.unwrap();
    let id = session["id"].as_str().unwrap();
    assert_eq!(session["kind"], "claude");
    assert_eq!(session["name"], "claude 1");
    assert_eq!(session["command"], json!([claude.to_str().unwrap()]));

    let mut ws = hub.connect(id, None, true).await.unwrap();
    let repo = hub.repo.canonicalize().unwrap();
    let (output, _) = read_until(&mut ws, &format!("session={id}")).await;
    // The hub names the conversation; without a report command (library
    // use) no hooks are injected.
    let conversation = session["agent_session_id"].as_str().unwrap();
    assert_eq!(conversation.len(), 36, "{session}");
    assert!(
        output.contains(&format!(
            "fake-claude in {} args=[--session-id {conversation}]",
            repo.display()
        )),
        "{output:?}"
    );
    assert_eq!(session["agent"]["status"], "unknown", "{session}");
    input(&mut ws, "ping-through-agent\r").await;
    read_until(&mut ws, "ping-through-agent").await;

    // An agent that is not installed is a clear 400, not a spawn failure.
    let response = hub
        .authed(reqwest::Method::POST, "/api/sessions")
        .json(&json!({ "workspace_id": workspace_id, "kind": "codex" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = response.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("codex is not installed"),
        "{error}"
    );
    // Agents run their CLI: an argv alongside is refused, as is an unknown kind.
    for body in [json!({ "workspace_id": workspace_id, "kind": "claude", "command": ["/bin/sh"] })]
    {
        let response = hub
            .authed(reqwest::Method::POST, "/api/sessions")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = hub
        .authed(reqwest::Method::POST, "/api/sessions")
        .json(&json!({ "workspace_id": workspace_id, "kind": "emacs" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // The kind survives in the listing.
    let listed: Value = hub
        .authed(reqwest::Method::GET, "/api/sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["sessions"][0]["kind"], "claude");
    drop(ws);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_rename_over_patch() {
    let hub = TestHub::start().await;
    let workspace = hub.create_workspace("rename").await;
    let session = hub
        .create_session(
            workspace["id"].as_str().unwrap(),
            Some(vec!["/bin/sh", "-c", "sleep 30"]),
        )
        .await;
    let id = session["id"].as_str().unwrap();
    assert_eq!(session["kind"], "shell");
    let path = format!("/api/sessions/{id}");

    let response = hub
        .authed(reqwest::Method::PATCH, &path)
        .json(&json!({ "name": "  dev server " }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let renamed: Value = response.json().await.unwrap();
    assert_eq!(renamed["name"], "dev server");
    assert!(renamed["live"].is_object(), "still live after rename");

    let response = hub
        .authed(reqwest::Method::PATCH, &path)
        .json(&json!({ "name": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = hub
        .authed(reqwest::Method::PATCH, "/api/sessions/nope")
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Cross-origin renames are refused like every other write.
    let response = hub
        .authed(reqwest::Method::PATCH, &path)
        .header(ORIGIN, "http://evil.example")
        .json(&json!({ "name": "pwned" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let fetched: Value = hub
        .authed(reqwest::Method::GET, &path)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fetched["name"], "dev server");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_check_validates_a_single_path() {
    let hub = TestHub::start().await;
    let check = |path: String| {
        let request = hub
            .authed(reqwest::Method::GET, "/api/repos/check")
            .query(&[("path", path)]);
        async move {
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.json::<Value>().await.unwrap()
        }
    };
    let ok = check(format!(" {} ", hub.repo.display())).await;
    assert_eq!(ok["ok"], true);
    assert_eq!(
        ok["path"],
        hub.repo.canonicalize().unwrap().to_str().unwrap()
    );

    assert_eq!(ok["git"], true);

    // A plain directory is a folder too, flagged as not a git repo.
    let plain = hub.repo.parent().unwrap().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let not_git = check(plain.display().to_string()).await;
    assert_eq!(not_git["ok"], true);
    assert_eq!(not_git["git"], false);
    let file = hub.repo.parent().unwrap().join("file");
    std::fs::write(&file, "").unwrap();
    let not_dir = check(file.display().to_string()).await;
    assert_eq!(not_dir["ok"], false);
    assert!(
        not_dir["error"]
            .as_str()
            .unwrap()
            .contains("not a directory")
    );
    // The folders spelling of the same check.
    let folders = hub
        .authed(reqwest::Method::GET, "/api/folders/check")
        .query(&[("path", plain.display().to_string())])
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(folders["git"], false);
    assert_eq!(check("relative/path".into()).await["ok"], false);
    assert_eq!(check(String::new()).await["ok"], false);

    // No `path` at all still answers 200 JSON, not a plain-text rejection.
    let missing = hub
        .authed(reqwest::Method::GET, "/api/repos/check")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::OK);
    let missing: Value = missing.json().await.unwrap();
    assert_eq!(missing["ok"], false);
    assert_eq!(missing["error"], "path is required");

    let anonymous = hub
        .http
        .get(hub.url("/api/repos/check?path=/"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    hub.shutdown().await;
}

async fn wait_state(hub: &TestHub, sid: &str, want: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let view: Value = hub
            .authed(reqwest::Method::GET, &format!("/api/sessions/{sid}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if view["state"] == want {
            return view;
        }
        assert!(Instant::now() < deadline, "never reached {want}: {view}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_relaunches_an_ended_session_in_place() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("relaunch").await;
    let workspace_id = ws["id"].as_str().unwrap().to_owned();
    let session = hub
        .create_session(
            &workspace_id,
            Some(vec![
                "/bin/sh",
                "-c",
                "echo ready; read x; echo got-$x; exit 3",
            ]),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    let restart = |sid: String| {
        hub.authed(
            reqwest::Method::POST,
            &format!("/api/sessions/{sid}/restart"),
        )
        .json(&json!({ "cols": 90, "rows": 20 }))
        .send()
    };

    // Still running: refused.
    assert_eq!(
        restart(sid.clone()).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    let mut client = hub.connect(&sid, None, true).await.unwrap();
    read_until(&mut client, "ready").await;
    input(&mut client, "one\n").await;
    read_until(&mut client, "got-one").await;
    assert_eq!(read_control(&mut client, "exit").await["code"], 3);
    wait_state(&hub, &sid, "exited").await;

    let relaunched = restart(sid.clone()).await.unwrap();
    assert_eq!(relaunched.status(), StatusCode::OK);
    let view: Value = relaunched.json().await.unwrap();
    assert_eq!(view["id"], sid.as_str());
    assert_eq!(view["name"], session["name"]);
    assert_eq!(view["state"], "running");
    assert_eq!(view["live"]["cols"], 90);
    // One record, not a look-alike second one.
    let list: Value = hub
        .authed(reqwest::Method::GET, "/api/sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);

    // The relaunched process answers on the same session id, with fresh
    // scrollback (the first run's output is not replayed).
    let mut client = hub.connect(&sid, None, true).await.unwrap();
    let (replay, _) = read_until(&mut client, "ready").await;
    assert!(!replay.contains("got-one"), "{replay:?}");
    input(&mut client, "two\n").await;
    read_until(&mut client, "got-two").await;
    assert_eq!(read_control(&mut client, "exit").await["code"], 3);
    wait_state(&hub, &sid, "exited").await;

    // A session left stopped by a hub restart relaunches too.
    let (dir, state_dir, repo) = hub.shutdown().await;
    let hub = TestHub::start_in(dir, state_dir, repo).await;
    let long = hub
        .create_session(&workspace_id, Some(vec!["/bin/sh", "-c", "sleep 300"]))
        .await;
    let long_id = long["id"].as_str().unwrap().to_owned();
    let (dir, state_dir, repo) = hub.shutdown().await;
    let hub = TestHub::start_in(dir, state_dir, repo).await;
    let stopped = wait_state(&hub, &long_id, "stopped").await;
    assert!(stopped["live"].is_null());
    let relaunched = hub
        .authed(
            reqwest::Method::POST,
            &format!("/api/sessions/{long_id}/restart"),
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(relaunched.status(), StatusCode::OK);
    let view: Value = relaunched.json().await.unwrap();
    assert_eq!(view["state"], "running");
    assert!(view["live"]["pid"].is_u64(), "{view}");

    let missing = hub
        .authed(reqwest::Method::POST, "/api/sessions/nope/restart")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    hub.shutdown().await;
}

// --- terminal state: snapshots on attach, rendered text -----------------------

impl TestHub {
    async fn screen(&self, sid: &str, query: &str) -> Value {
        let response = self
            .authed(
                reqwest::Method::GET,
                &format!("/api/sessions/{sid}/screen{query}"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    /// Poll the server's rendered screen until `done` holds.
    async fn wait_screen(&self, sid: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let screen = self.screen(sid, "?source=visible").await;
            if done(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "screen never settled: {screen:#}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// A client-side terminal fed from a WebSocket: starts from the snapshot
/// header's size, then replays every binary frame.
struct Viewer {
    term: Emulator,
    header: Value,
}

impl Viewer {
    /// Attach; the first message must be the snapshot header.
    async fn attach(hub: &TestHub, sid: &str) -> (Self, Ws) {
        let origin = hub.origin();
        let mut ws = hub.connect(sid, Some(&origin), true).await.unwrap();
        let first = timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("a first message")
            .expect("open socket")
            .unwrap();
        let Message::Text(text) = first else {
            panic!("expected the snapshot header, got {first:?}");
        };
        let header: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(header["type"], "snapshot", "{header}");
        let cols = u16::try_from(header["cols"].as_u64().unwrap()).unwrap();
        let rows = u16::try_from(header["rows"].as_u64().unwrap()).unwrap();
        let term = Emulator::new(cols, rows, 1000);
        (Self { term, header }, ws)
    }

    fn visible(&mut self) -> Value {
        let text = self.term.text(TextSource::Visible);
        json!({ "lines": text.lines, "cursor": text.cursor,
                "alt_screen": text.alt_screen, "title": text.title, "modes": text.modes })
    }

    /// Replay frames until this viewer shows exactly what the server's
    /// emulator shows and `done` holds for it.
    async fn converge(
        &mut self,
        ws: &mut Ws,
        hub: &TestHub,
        sid: &str,
        done: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let server = hub.screen(sid, "?source=visible").await;
            let mine = self.visible();
            let same = ["lines", "cursor", "alt_screen", "title", "modes"]
                .iter()
                .all(|k| server[k] == mine[k]);
            if same && done(&mine) {
                return mine;
            }
            assert!(
                Instant::now() < deadline,
                "viewer never matched the server\nserver: {server:#}\nviewer: {mine:#}"
            );
            match timeout(Duration::from_millis(100), ws.next()).await {
                Ok(Some(Ok(Message::Binary(bytes)))) => self.term.process(&bytes),
                Ok(Some(Ok(_))) | Err(_) => {}
                Ok(other) => panic!("socket ended: {other:?}"),
            }
        }
    }
}

fn lines_of(screen: &Value) -> Vec<String> {
    screen["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

/// A full-screen program in plain sh: draws on the alternate screen with
/// colours, a title, bracketed paste and SGR mouse on, waits for a line of
/// input, then restores the shell screen.
const FULL_SCREEN_SCRIPT: &str = r#"
printf 'before-alt 1\r\nbefore-alt 2\r\n'
printf '\033[?1049h\033[?2004h\033[?1002h\033[?1006h\033[?1h\033]2;tui-title\007'
printf '\033[H\033[2J\033[1;44;37m header bar \033[0m'
printf '\033[5;10H\033[38;5;208mcentered\033[0m'
printf '\033[24;1H\033[7mstatus: READY\033[0m\033[3;3H'
read answer
printf '\033[?2004l\033[?1002l\033[?1006l\033[?1l\033[?1049l'
printf 'answered %s\r\n' "$answer"
sleep 30
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reattached_client_sees_the_same_full_screen_program() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("tui").await;
    let session = hub
        .create_session(
            ws["id"].as_str().unwrap(),
            Some(vec!["/bin/sh", "-c", FULL_SCREEN_SCRIPT]),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    let ready = hub
        .wait_screen(&sid, |s| {
            s["lines"][23].as_str().is_some_and(|l| l.contains("READY"))
        })
        .await;
    assert_eq!(ready["alt_screen"], true);
    assert_eq!(ready["title"], "tui-title");
    assert_eq!(
        ready["cursor"],
        json!({ "row": 2, "col": 2, "visible": true })
    );
    for mode in ["bracketed_paste", "mouse_drag", "sgr_mouse", "app_cursor"] {
        assert!(
            ready["modes"].as_array().unwrap().contains(&json!(mode)),
            "{ready}"
        );
    }

    // Two clients attach after the fact (one, then a reattach): each
    // rebuilds the alternate screen, cursor, modes, and title exactly.
    for _ in 0..2 {
        let (mut viewer, mut socket) = Viewer::attach(&hub, &sid).await;
        assert_eq!(viewer.header["alt_screen"], true);
        assert_eq!(viewer.header["cols"], 80);
        let seen = viewer.converge(&mut socket, &hub, &sid, |_| true).await;
        assert_eq!(seen["lines"][0], " header bar");
        assert_eq!(seen["lines"][4], "         centered");
        drop(socket);
    }

    // Live output continues the snapshot with no gap: leaving the
    // alternate screen restores the shell rows the snapshot carried.
    let (mut viewer, mut socket) = Viewer::attach(&hub, &sid).await;
    viewer.converge(&mut socket, &hub, &sid, |_| true).await;
    input(&mut socket, "yes\r").await;
    let after = viewer
        .converge(&mut socket, &hub, &sid, |s| {
            s["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l == "answered yes")
        })
        .await;
    assert_eq!(after["alt_screen"], false);
    assert_eq!(after["modes"], json!([]));
    let lines = lines_of(&after);
    assert_eq!(
        &lines[..3],
        ["before-alt 1", "before-alt 2", "answered yes"]
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reattached_client_sees_the_same_vim_screen() {
    if std::process::Command::new("vim")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: vim is not installed");
        return;
    }
    let hub = TestHub::start().await;
    let file = hub.repo.join("notes.txt");
    let text: String = (1..=40).map(|i| format!("note line {i}\n")).collect();
    std::fs::write(&file, text).unwrap();
    let ws = hub.create_workspace("vim").await;
    let session = hub
        .create_session(
            ws["id"].as_str().unwrap(),
            Some(vec![
                "vim",
                "-u",
                "NONE",
                "-N",
                "-i",
                "NONE",
                "-n",
                "+set number",
                file.to_str().unwrap(),
            ]),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    hub.wait_screen(&sid, |s| {
        s["alt_screen"] == true
            && s["lines"][0]
                .as_str()
                .is_some_and(|l| l.contains("note line 1"))
    })
    .await;

    let (mut viewer, mut socket) = Viewer::attach(&hub, &sid).await;
    let seen = viewer.converge(&mut socket, &hub, &sid, |_| true).await;
    assert!(
        seen["lines"][0].as_str().unwrap().ends_with("note line 1"),
        "{seen}"
    );

    // Drive vim through the reattached client: jump to the end, then quit.
    input(&mut socket, "G").await;
    viewer
        .converge(&mut socket, &hub, &sid, |s| {
            s["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l.as_str().unwrap().ends_with("note line 40"))
        })
        .await;
    input(&mut socket, ":q!\r").await;
    let exit = read_control(&mut socket, "exit").await;
    assert_eq!(exit["code"], 0);
    let last = hub.screen(&sid, "").await;
    assert_eq!(last["alt_screen"], false);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_text_sources_and_validation() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("text").await;
    let script = "i=1; while [ $i -le 60 ]; do echo \"row $i\"; i=$((i+1)); done; \
                  printf '%0100d\\n' 7; sleep 30";
    let session = hub
        .create_session(
            ws["id"].as_str().unwrap(),
            Some(vec!["/bin/sh", "-c", script]),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    let long = format!("{}7", "0".repeat(99));
    let visible = hub
        .wait_screen(&sid, |s| {
            s["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l == "00000000000000000007")
        })
        .await;
    assert_eq!(visible["source"], "visible");
    assert_eq!(visible["rows"], 24);
    assert_eq!(visible["lines"].as_array().unwrap().len(), 24);

    // Recent joins the soft-wrapped 100-char line and reaches into history.
    let recent = hub.screen(&sid, "?source=recent&lines=3").await;
    assert_eq!(recent["source"], "recent");
    assert_eq!(lines_of(&recent), ["row 59", "row 60", long.as_str()]);
    let all = hub.screen(&sid, "?source=recent&lines=1000").await;
    let all = lines_of(&all);
    assert_eq!(all.first().map(String::as_str), Some("row 1"));
    assert_eq!(all.len(), 61);
    let listed = hub
        .authed(reqwest::Method::GET, &format!("/api/sessions/{sid}"))
        .send()
        .await
        .unwrap();
    let listed: Value = listed.json().await.unwrap();
    assert!(listed["live"]["scrollback_lines"].as_u64().unwrap() > 0);

    for query in [
        "?source=nope",
        "?source=recent&lines=0",
        "?source=recent&lines=abc",
        "?lines=abc",
        "?source=visible&lines=5",
        "?source=recent&lines=999999999",
    ] {
        let response = hub
            .authed(
                reqwest::Method::GET,
                &format!("/api/sessions/{sid}/screen{query}"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        let body: Value = response.json().await.expect("a JSON error body");
        assert!(body["error"].is_string(), "{query}: {body}");
    }
    let missing = hub
        .authed(reqwest::Method::GET, "/api/sessions/nope/screen")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let anonymous = hub
        .http
        .get(hub.url(&format!("/api/sessions/{sid}/screen")))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    hub.shutdown().await;
}

/// The next text message (skipping nothing else: binary is an error here).
async fn next_text(ws: &mut Ws) -> Value {
    match timeout(Duration::from_secs(30), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected a text message, got {other:?}"),
    }
}

/// Read exactly `len` bytes of binary frames (the snapshot).
async fn read_snapshot(ws: &mut Ws, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        match timeout(Duration::from_secs(30), ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => out.extend_from_slice(&bytes),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            other => panic!("expected snapshot bytes, got {other:?}"),
        }
    }
    assert_eq!(out.len(), len, "the snapshot is exactly `bytes` long");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_falls_behind_gets_the_screen_alone_and_catches_up() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("lag").await;
    // History first (a large snapshot), then, on cue, a flood far past the
    // broadcast depth while the client is not reading.
    let script = "i=0; while [ $i -lt 3000 ]; do echo \"history line $i with some padding\"; \
                  i=$((i+1)); done; read go; \
                  yes 'flood flood flood flood flood flood flood flood' | head -c 40000000; \
                  echo FLOOD-DONE; sleep 60";
    let session = hub
        .create_session(
            ws["id"].as_str().unwrap(),
            Some(vec!["/bin/sh", "-c", script]),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    hub.wait_screen(&sid, |s| {
        s["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == "history line 2999 with some padding")
    })
    .await;
    let origin = hub.origin();
    let mut socket = hub.connect(&sid, Some(&origin), true).await.unwrap();
    let header = next_text(&mut socket).await;
    assert_eq!(header["type"], "snapshot");
    let first = usize::try_from(header["bytes"].as_u64().unwrap()).unwrap();
    assert!(
        first > 100_000,
        "the first snapshot carries history: {header}"
    );
    read_snapshot(&mut socket, first).await;

    input(&mut socket, "go\r").await;
    // Stop reading until the flood is over.
    hub.wait_screen(&sid, |s| {
        s["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().ends_with("FLOOD-DONE"))
    })
    .await;
    let reset_at = loop {
        match timeout(Duration::from_secs(30), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "reset" {
                    break value;
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("socket ended before a reset: {other:?}"),
        }
    };
    assert_eq!(reset_at["type"], "reset");
    let header = next_text(&mut socket).await;
    assert_eq!(header["type"], "snapshot", "{header}");
    let bytes = usize::try_from(header["bytes"].as_u64().unwrap()).unwrap();
    assert!(
        bytes < 16 * 1024,
        "a lagged client gets the screen alone: {header}"
    );
    let snapshot = read_snapshot(&mut socket, bytes).await;
    let mut viewer = Viewer {
        term: Emulator::new(80, 24, 1000),
        header,
    };
    viewer.term.process(&snapshot);
    assert_eq!(viewer.term.history_lines(), 0);
    let seen = viewer
        .converge(&mut socket, &hub, &sid, |s| {
            s["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l.as_str().unwrap().ends_with("FLOOD-DONE"))
        })
        .await;
    assert_eq!(seen["alt_screen"], false);
    hub.shutdown().await;
}

async fn send_input(hub: &TestHub, sid: &str, body: Value) -> (StatusCode, Value) {
    let response = hub
        .authed(reqwest::Method::POST, &format!("/api/sessions/{sid}/input"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_is_pasted_bracketed_only_when_the_program_asked() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("input").await;
    let workspace_id = ws["id"].as_str().unwrap().to_owned();
    // `cat -v` shows exactly what arrives (ESC as `^[`); echo off so only
    // cat's copy is on screen.
    let bracketed = hub
        .create_session(
            &workspace_id,
            Some(vec![
                "/bin/sh",
                "-c",
                "stty -echo; printf '\\033[?2004h'; echo ready; exec cat -v",
            ]),
        )
        .await;
    let sid = bracketed["id"].as_str().unwrap().to_owned();
    hub.wait_screen(&sid, |s| lines_of(s).iter().any(|l| l == "ready"))
        .await;
    let (status, body) = send_input(
        &hub,
        &sid,
        json!({"data": "one\ntwo\u{1b}[201~three", "paste": true, "enter": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bracketed"], true);
    let screen = hub
        .wait_screen(&sid, |s| lines_of(s).iter().any(|l| l.contains("201~")))
        .await;
    let lines = lines_of(&screen);
    // One paste: the embedded end marker was dropped, newlines became CR
    // (the tty turns them into line ends), Enter followed separately.
    assert!(lines.contains(&"^[[200~one".to_owned()), "{lines:?}");
    assert!(lines.contains(&"twothree^[[201~".to_owned()), "{lines:?}");
    // Raw input is passed through untouched.
    let (status, body) =
        send_input(&hub, &sid, json!({"data": "\u{1b}[Araw\n", "paste": false})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bracketed"], false);
    hub.wait_screen(&sid, |s| lines_of(s).iter().any(|l| l == "^[[Araw"))
        .await;

    // A program that did not enable bracketed paste gets plain lines.
    let plain = hub
        .create_session(
            &workspace_id,
            Some(vec!["/bin/sh", "-c", "stty -echo; echo ready; exec cat -v"]),
        )
        .await;
    let plain_id = plain["id"].as_str().unwrap().to_owned();
    hub.wait_screen(&plain_id, |s| lines_of(s).iter().any(|l| l == "ready"))
        .await;
    let (_, body) = send_input(&hub, &plain_id, json!({"data": "a\nb\n", "paste": true})).await;
    assert_eq!(body["bracketed"], false);
    let screen = hub
        .wait_screen(&plain_id, |s| lines_of(s).iter().any(|l| l == "b"))
        .await;
    assert!(lines_of(&screen).contains(&"a".to_owned()));

    // Refusals.
    let (status, _) = send_input(&hub, &sid, json!({"data": ""})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send_input(&hub, "missing", json!({"data": "x"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let ended = hub
        .create_session(&workspace_id, Some(vec!["/bin/sh", "-c", "exit 0"]))
        .await;
    let ended_id = ended["id"].as_str().unwrap().to_owned();
    wait_state(&hub, &ended_id, "exited").await;
    let (status, body) = send_input(&hub, &ended_id, json!({"data": "x"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_shutdown_restores_shells_in_place_on_the_next_open() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("restore").await;
    let workspace_id = ws["id"].as_str().unwrap().to_owned();
    let deeper = hub.repo.join("sub");
    std::fs::create_dir_all(&deeper).unwrap();
    let deeper = deeper.canonicalize().unwrap();
    let shell = hub.create_session(&workspace_id, None).await;
    let sid = shell["id"].as_str().unwrap().to_owned();
    let command = hub
        .create_session(
            &workspace_id,
            Some(vec!["/bin/sh", "-c", "echo ran; sleep 300"]),
        )
        .await;
    let cid = command["id"].as_str().unwrap().to_owned();
    let exited = hub
        .create_session(&workspace_id, Some(vec!["/bin/sh", "-c", "exit 3"]))
        .await;
    let eid = exited["id"].as_str().unwrap().to_owned();
    wait_state(&hub, &eid, "exited").await;
    send_input(&hub, &sid, json!({"data": "cd sub", "enter": true})).await;
    send_input(&hub, &sid, json!({"data": "pwd", "enter": true})).await;
    let marker = deeper.display().to_string();
    hub.wait_screen(&sid, |s| lines_of(s).contains(&marker))
        .await;

    let (dir, state_dir, repo) = hub.shutdown().await;
    let raw = std::fs::read_to_string(state_dir.join("state.json")).unwrap();
    assert!(raw.contains("\"clean\": true"), "{raw}");

    // What `fw hub serve` does on start.
    let mut options = HubOptions::new(state_dir.clone());
    options.default_shell = vec!["/bin/sh".into()];
    let reopened = Hub::open(options).unwrap();
    let outcomes = reopened.restore_sessions();
    assert_eq!(outcomes.len(), 2, "the exited session is not restored");
    assert_eq!(outcomes[0].id, sid);
    assert_eq!(outcomes[0].cwd, deeper, "where its shell had cd'd to");
    assert_eq!(outcomes[1].id, cid);
    assert_eq!(
        outcomes[1].result.as_ref().unwrap(),
        &flywheel_hub::restore::RestoreHow::Shell
    );
    let views = reopened.sessions(None);
    let order: Vec<&str> = views.iter().map(|v| v.record.id.as_str()).collect();
    assert_eq!(order, [sid.as_str(), cid.as_str(), eid.as_str()]);
    assert!(views[0].record.state.is_running() && views[0].live.is_some());
    // What it is stays recorded; the shell runs in its place.
    assert_eq!(
        views[1].record.command,
        vec!["/bin/sh", "-c", "echo ran; sleep 300"]
    );
    assert_eq!(
        views[1].record.fallback_command,
        Some(vec!["/bin/sh".to_owned()])
    );
    assert!(!views[2].record.state.is_running());
    // A second restore finds nothing left to do.
    assert!(reopened.restore_sessions().is_empty());
    reopened.shutdown(Duration::from_secs(4));
    drop(reopened);

    // A hub that died leaves its sessions recorded `running`: loaded back
    // as stopped, but not restored.
    let path = state_dir.join("state.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for session in doc["sessions"].as_array_mut().unwrap() {
        if session["state"] == "stopped" {
            let session = session.as_object_mut().unwrap();
            session.insert("state".into(), json!("running"));
            session.remove("at");
            session.remove("clean");
            session.remove("last_cwd");
        }
    }
    std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let mut options = HubOptions::new(state_dir.clone());
    options.default_shell = vec!["/bin/sh".into()];
    let after_crash = Hub::open(options).unwrap();
    assert!(after_crash.restore_sessions().is_empty());
    let views = after_crash.sessions(None);
    assert!(!views[0].record.state.is_restorable());
    assert!(matches!(
        &views[0].record.state,
        flywheel_hub::state::SessionState::Stopped { clean: false, restore_error: Some(e), .. }
            if e == flywheel_hub::state::UNCLEAN_STOP
    ));
    drop(after_crash);
    drop((dir, repo));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_directory_is_refused_not_replaced_by_home() {
    let hub = TestHub::start().await;
    let ws = hub.create_workspace("moving").await;
    let workspace_id = ws["id"].as_str().unwrap().to_owned();
    let session = hub
        .create_session(&workspace_id, Some(vec!["/bin/sh", "-c", "exit 0"]))
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    // The live view reports where the process is.
    let live = hub.create_session(&workspace_id, None).await;
    let lid = live["id"].as_str().unwrap().to_owned();
    let repo = hub.repo.canonicalize().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let view: Value = hub
            .authed(reqwest::Method::GET, &format!("/api/sessions/{lid}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if view["live"]["cwd"] == json!(repo) {
            break;
        }
        assert!(Instant::now() < deadline, "no live cwd: {view}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_state(&hub, &sid, "exited").await;

    let moved = hub.repo.with_file_name("repo-moved");
    std::fs::rename(&hub.repo, &moved).unwrap();
    let created = hub
        .authed(reqwest::Method::POST, "/api/sessions")
        .json(&json!({ "workspace_id": workspace_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::BAD_REQUEST);
    let body: Value = created.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("no longer exists"),
        "{body}"
    );
    let relaunched = hub
        .authed(
            reqwest::Method::POST,
            &format!("/api/sessions/{sid}/restart"),
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(relaunched.status(), StatusCode::BAD_REQUEST);
    let body: Value = relaunched.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("no longer exists"),
        "{body}"
    );
    // Nothing was started, and the record did not change.
    let sessions: Value = hub
        .authed(reqwest::Method::GET, "/api/sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(sessions["sessions"][0]["state"], "exited");
    std::fs::rename(&moved, &hub.repo).unwrap();
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspaces_hold_plain_folders_and_sessions_open_in_them() {
    let hub = TestHub::start().await;
    let plain = hub.repo.parent().unwrap().join("finances");
    std::fs::create_dir_all(&plain).unwrap();
    let plain = plain.canonicalize().unwrap();
    let response = hub
        .authed(reqwest::Method::POST, "/api/workspaces")
        .json(&json!({ "name": "life", "folders": [
            { "path": hub.repo, "name": "Code" },
            plain,
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let ws: Value = response.json().await.unwrap();
    assert_eq!(ws["folders"][0]["name"], "Code");
    assert_eq!(ws["folders"][0]["git"], true);
    assert_eq!(ws["folders"][1]["git"], false);
    assert!(ws["folders"][1].get("name").is_none());
    // The legacy mirror lists every folder.
    assert_eq!(ws["repos"][1], plain.to_str().unwrap());
    let response = hub
        .authed(reqwest::Method::POST, "/api/sessions")
        .json(&json!({ "workspace_id": ws["id"], "folder": plain, "command": ["/bin/sh"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let session: Value = response.json().await.unwrap();
    assert_eq!(session["cwd"], plain.to_str().unwrap());
    // `git init` shows on the next listing.
    std::fs::create_dir(plain.join(".git")).unwrap();
    let listed: Value = hub
        .authed(reqwest::Method::GET, "/api/workspaces")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["workspaces"][0]["folders"][1]["git"], true);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vscode_workspaces_import_list_and_update() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let code = root.join("code");
    let repo = code.join("app");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let notes = root.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let files = code.join("workspaces");
    std::fs::create_dir_all(&files).unwrap();
    let file = files.join("personal.code-workspace");
    std::fs::write(
        &file,
        r#"{
        // JSONC, as VS Code writes it
        "folders": [
            {"name": "The App", "path": "../app"},
            {"path": "../../notes"},
            {"path": "../gone"},
            {"uri": "vscode-remote://ssh-remote+box/srv"},
        ],
        "settings": {},
    }"#,
    )
    .unwrap();
    std::fs::write(files.join("broken.code-workspace"), "{").unwrap();
    let hub = TestHub::start_with(dir, root.join("state"), repo.clone(), |options| {
        options.import_root = Some(code.clone());
    })
    .await;

    let import = |body: Value| {
        let request = hub
            .authed(reqwest::Method::POST, "/api/workspaces/import")
            .json(&body);
        async move {
            let response = request.send().await.unwrap();
            let status = response.status();
            (status, response.json::<Value>().await.unwrap())
        }
    };
    let (status, report) = import(json!({ "path": file })).await;
    assert_eq!(status, StatusCode::CREATED, "{report}");
    assert_eq!(report["created"], true);
    let ws = &report["workspace"];
    assert_eq!(ws["name"], "personal");
    let folders = ws["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0]["path"], repo.to_str().unwrap());
    assert_eq!(folders[0]["name"], "The App");
    assert_eq!(folders[0]["git"], true);
    assert_eq!(folders[1]["path"], notes.to_str().unwrap());
    assert_eq!(folders[1]["git"], false);
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 2, "{report}");
    assert_eq!(skipped[0]["entry"], "../gone");
    assert_eq!(skipped[0]["reason"], "does not exist");
    assert_eq!(skipped[0]["path"], code.join("gone").to_str().unwrap());
    assert!(
        skipped[1]["reason"]
            .as_str()
            .unwrap()
            .contains("ssh-remote+box")
    );

    // Again: refused without update.
    let (status, again) = import(json!({ "path": file })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");

    // The picker lists both files, the importable one with its workspace.
    let listed: Value = hub
        .authed(reqwest::Method::GET, "/api/code-workspaces")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["root"], code.to_str().unwrap());
    let found = listed["files"].as_array().unwrap();
    assert_eq!(found.len(), 2, "{listed}");
    assert_eq!(found[0]["name"], "broken");
    assert!(found[0]["error"].as_str().unwrap().contains("invalid JSON"));
    assert_eq!(found[1]["path"], file.to_str().unwrap());
    assert_eq!(found[1]["folders"], 3);
    assert_eq!(found[1]["skipped"], 1);
    assert_eq!(found[1]["workspace_id"], ws["id"]);
    let elsewhere = hub
        .authed(reqwest::Method::GET, "/api/code-workspaces")
        .query(&[("root", "relative/dir")])
        .send()
        .await
        .unwrap();
    assert_eq!(elsewhere.status(), StatusCode::BAD_REQUEST);

    // Update replaces the folders -- but not out from under a running
    // session.
    let session: Value = hub
        .authed(reqwest::Method::POST, "/api/sessions")
        .json(&json!({ "workspace_id": ws["id"], "repo": notes, "command": ["/bin/sh"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    std::fs::write(&file, r#"{"folders": [{"path": "../app"}]}"#).unwrap();
    let (status, refused) = import(json!({ "path": file, "update": true })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("kill it first"));
    let killed = hub
        .authed(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}", session["id"].as_str().unwrap()),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(killed.status(), StatusCode::NO_CONTENT);
    let (status, updated) = import(json!({ "path": file, "update": true })).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["created"], false);
    assert_eq!(updated["workspace"]["id"], ws["id"]);
    let folders = updated["workspace"]["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert!(
        folders[0].get("name").is_none(),
        "label dropped with the file's"
    );

    // A file with nothing importable, and a relative path, are 400s.
    std::fs::write(&file, r#"{"folders": [{"path": "../gone"}]}"#).unwrap();
    let (status, none) = import(json!({ "path": file, "name": "other" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        none["error"]
            .as_str()
            .unwrap()
            .contains("no folder to import")
    );
    let (status, _) = import(json!({ "path": "personal.code-workspace" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    hub.shutdown().await;
}
