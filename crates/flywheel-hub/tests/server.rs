//! End-to-end: a real hub server on 127.0.0.1:0, real HTTP, real
//! WebSockets, real PTYs running `/bin/sh`.

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
        let mut options = HubOptions::new(state_dir.clone());
        options.default_shell = vec!["/bin/sh".into()];
        let hub = Hub::open(options).unwrap();
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
            let _ = stopped.await;
        }));
        Self {
            _dir: dir,
            state_dir,
            repo,
            addr,
            token,
            http: reqwest::Client::new(),
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
        format!("fw_hub={}", self.token)
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
    for path in ["/api/workspaces", "/api/sessions", "/assets/app.js"] {
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
    assert!(cookie.starts_with(&format!("fw_hub={};", hub.token)));
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

    let logout = hub.http.post(hub.url("/logout")).send().await.unwrap();
    assert!(
        logout.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
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
