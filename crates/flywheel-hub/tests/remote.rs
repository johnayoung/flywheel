//! Remote access end to end: one-time login codes, per-client failure
//! limits, token rotation on a running hub, proxy origins, the viewer theme
//! behind colour queries, and the `fw hub url` / `fw hub token --rotate`
//! verbs against a real server on 127.0.0.1:0.

use flywheel_hub::auth::{MAX_FAILURES, cookie_name};
use flywheel_hub::remote::{RuntimeInfo, ServeLock};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use reqwest::header::{COOKIE, ORIGIN, RETRY_AFTER, SET_COOKIE};
use serde_json::{Value, json};
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
    async fn start(options: ServerOptions) -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let mut hub_options = HubOptions::new(state_dir.clone());
        hub_options.default_shell = vec!["/bin/sh".into()];
        let hub = Hub::open(hub_options).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, Arc::clone(&hub), options, async {
            let _ = stopped.await;
        }));
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

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn authed(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .bearer_auth(self.token())
    }

    async fn mint(&self) -> String {
        let response = self
            .authed(reqwest::Method::POST, "/api/login-codes")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["expires_in"], 300);
        body["code"].as_str().unwrap().to_owned()
    }

    async fn shell(&self, command: &[&str]) -> String {
        let ws: Value = self
            .authed(reqwest::Method::POST, "/api/workspaces")
            .json(&json!({ "name": "w", "repos": [self.repo] }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let session: Value = self
            .authed(reqwest::Method::POST, "/api/sessions")
            .json(&json!({ "workspace_id": ws["id"], "command": command, "cols": 80, "rows": 24 }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        session["id"].as_str().unwrap().to_owned()
    }

    async fn connect(&self, session_id: &str, cookie: &str) -> Ws {
        let url = format!("ws://{}/api/sessions/{session_id}/ws", self.addr);
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    async fn connect_events(&self, cookie: &str) -> Ws {
        let url = format!("ws://{}/api/events?client=rotation-test", self.addr);
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    /// Stop the server; the state dir lives as long as the returned guard.
    async fn shutdown(mut self) -> tempfile::TempDir {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        self._dir
    }
}

/// The `name=value` of a login response's cookie.
fn cookie_pair(response: &reqwest::Response) -> String {
    let set = response.headers()[SET_COOKIE].to_str().unwrap();
    set.split(';').next().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_time_codes_log_in_once() {
    let hub = TestHub::start(ServerOptions::default()).await;
    // Minting needs the credential.
    let anonymous = hub
        .http
        .post(hub.url("/api/login-codes"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let code = hub.mint().await;
    let ok = hub
        .http
        .get(hub.url(&format!("/login?code={code}")))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    // The cookie carries a fresh per-login secret: never the code, and
    // never the long-lived token.
    let pair = cookie_pair(&ok);
    let (name, value) = pair.split_once('=').unwrap();
    assert_eq!(name, cookie_name(&hub.token()));
    assert_eq!(value.len(), 64);
    assert!(value != hub.token() && value != code, "{pair}");
    // The signed-in page replaces itself (and the spent code URL in
    // history) with the hub, keeping the URL fragment.
    let page = ok.text().await.unwrap();
    assert!(page.contains("data-signed-in") && page.contains("/assets/login.js"));
    let script = hub
        .http
        .get(hub.url("/assets/login.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        script.status(),
        StatusCode::OK,
        "public: the login page needs it"
    );
    assert!(
        script
            .text()
            .await
            .unwrap()
            .contains("location.replace(\"/\" + hash)")
    );
    let app = hub
        .http
        .get(hub.url("/api/workspaces"))
        .header(COOKIE, &pair)
        .send()
        .await
        .unwrap();
    assert_eq!(app.status(), StatusCode::OK);

    let again = hub
        .http
        .get(hub.url(&format!("/login?code={code}")))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::UNAUTHORIZED, "single use");
    assert!(again.headers().get(SET_COOKIE).is_none());

    // The form's one field takes a code too (typed in upper case, grouped).
    let typed = hub.mint().await.to_uppercase();
    let typed = format!("{}-{}", &typed[..10], &typed[10..]);
    let form = hub
        .http
        .post(hub.url("/login"))
        .header(ORIGIN, format!("http://{}", hub.addr))
        .form(&[("token", typed.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), StatusCode::OK);
    assert!(form.headers().get(SET_COOKIE).is_some());
    // A code is not a bearer credential.
    let code = hub.mint().await;
    let bearer = hub
        .http
        .get(hub.url("/api/workspaces"))
        .bearer_auth(&code)
        .send()
        .await
        .unwrap();
    assert_eq!(bearer.status(), StatusCode::UNAUTHORIZED);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_attempts_are_limited_per_client() {
    let hub = TestHub::start(ServerOptions::default()).await;
    let attacker = "203.0.113.9";
    let attempt = |xff: Option<&str>, token: String| {
        let mut request = hub.http.get(hub.url(&format!("/login?token={token}")));
        if let Some(xff) = xff {
            // The local proxy appends the address it saw: only the last
            // entry counts, so a client cannot pick its own bucket.
            request = request.header("x-forwarded-for", format!("10.9.9.9, {xff}"));
        }
        request.send()
    };
    for i in 0..MAX_FAILURES {
        let wrong = attempt(Some(attacker), format!("wrong{i}")).await.unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED, "attempt {i}");
    }
    // Locked: even the right token is refused, and says when to retry.
    let locked = attempt(Some(attacker), hub.token()).await.unwrap();
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = locked.headers()[RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry > 0 && retry <= 600, "{retry}");
    assert!(locked.headers().get(SET_COOKIE).is_none());
    assert!(
        locked
            .text()
            .await
            .unwrap()
            .contains("Too many failed attempts")
    );
    let api = hub
        .http
        .get(hub.url("/api/workspaces"))
        .bearer_auth(hub.token())
        .header("x-forwarded-for", attacker)
        .send()
        .await
        .unwrap();
    assert_eq!(api.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(api.headers().get(RETRY_AFTER).is_some());

    // Other clients are unaffected.
    let other = attempt(Some("198.51.100.4"), hub.token()).await.unwrap();
    assert_eq!(other.status(), StatusCode::OK);
    // A live login cookie still works from the locked-out address: an
    // attacker sharing a proxy or NAT address with a signed-in user cannot
    // lock the user out (and cannot guess a 256-bit login secret).
    let signed_in = cookie_pair(&other);
    for path in ["/api/workspaces", "/"] {
        let response = hub
            .http
            .get(hub.url(path))
            .header(COOKIE, &signed_in)
            .header("x-forwarded-for", attacker)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let guess = hub
        .http
        .get(hub.url("/api/workspaces"))
        .header(
            COOKIE,
            format!("{}={}", cookie_name(&hub.token()), "f".repeat(64)),
        )
        .header("x-forwarded-for", attacker)
        .send()
        .await
        .unwrap();
    assert_eq!(guess.status(), StatusCode::TOO_MANY_REQUESTS);
    // Direct connections from this machine (the CLI, a local browser) are
    // never locked out: the limit guards against the network.
    for i in 0..=MAX_FAILURES {
        let wrong = attempt(None, format!("local{i}")).await.unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
    let direct = attempt(None, hub.token()).await.unwrap();
    assert_eq!(direct.status(), StatusCode::OK);

    // Wrong bearers count too.
    for _ in 0..MAX_FAILURES {
        let wrong = hub
            .http
            .get(hub.url("/api/workspaces"))
            .bearer_auth("0".repeat(64))
            .header("x-forwarded-for", "198.51.100.77")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
    let locked = attempt(Some("198.51.100.77"), hub.token()).await.unwrap();
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    // Requests with no credential are not failures: the login page stays.
    let page = hub
        .http
        .get(hub.url("/"))
        .header("x-forwarded-for", "198.51.100.78")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotating_the_token_signs_everyone_out_without_a_restart() {
    let hub = TestHub::start(ServerOptions::default()).await;
    let old = hub.token();
    let login = hub
        .http
        .get(hub.url(&format!("/login?token={old}")))
        .send()
        .await
        .unwrap();
    let old_cookie = cookie_pair(&login);
    let sid = hub.shell(&["/bin/sh"]).await;
    let mut ws = hub.connect(&sid, &old_cookie).await;
    let mut events = hub.connect_events(&old_cookie).await;
    let pending_code = hub.mint().await;

    let rotated = hub
        .authed(reqwest::Method::POST, "/api/token/rotate")
        .send()
        .await
        .unwrap();
    assert_eq!(rotated.status(), StatusCode::NO_CONTENT);
    let new = hub.token();
    assert_ne!(new, old);
    assert_eq!(
        std::fs::read_to_string(hub.state_dir.join("token"))
            .unwrap()
            .trim(),
        new
    );

    // The open terminal and events connections are closed with 4001.
    for socket in [&mut ws, &mut events] {
        let close = loop {
            match timeout(Duration::from_secs(10), socket.next())
                .await
                .expect("closed in time")
            {
                Some(Ok(Message::Close(frame))) => break frame,
                Some(Ok(_)) => {}
                other => panic!("expected a close frame, got {other:?}"),
            }
        };
        assert_eq!(u16::from(close.unwrap().code), 4001);
    }

    // The old cookie, bearer, and code are dead; the new token works.
    for request in [
        hub.http
            .get(hub.url("/api/workspaces"))
            .header(COOKIE, &old_cookie),
        hub.http.get(hub.url("/api/workspaces")).bearer_auth(&old),
    ] {
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let code = hub
        .http
        .get(hub.url(&format!("/login?code={pending_code}")))
        .send()
        .await
        .unwrap();
    assert_eq!(code.status(), StatusCode::UNAUTHORIZED);
    let app = hub
        .http
        .get(hub.url("/"))
        .header(COOKIE, &old_cookie)
        .send()
        .await
        .unwrap();
    assert!(
        app.text().await.unwrap().contains("action=\"/login\""),
        "the old cookie gets the login page"
    );
    // Where the UI lands after a 4001 close: the login page says why.
    let notice = hub
        .http
        .get(hub.url("/?signed_out=rotated"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(notice.contains("token was rotated"), "{notice}");
    let fresh = hub
        .authed(reqwest::Method::GET, "/api/workspaces")
        .send()
        .await
        .unwrap();
    assert_eq!(fresh.status(), StatusCode::OK);
    // The session itself kept running.
    let session: Value = hub
        .authed(reqwest::Method::GET, &format!("/api/sessions/{sid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["state"], "running");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_origins_and_https_cookies() {
    let public = "https://box.tail1234.ts.net".to_owned();
    let hub = TestHub::start(ServerOptions {
        allowed_origins: vec![public.clone()],
        ..ServerOptions::default()
    })
    .await;
    let post = |origin: &str, forwarded_host: Option<&str>| {
        let mut request = hub
            .authed(reqwest::Method::POST, "/api/workspaces")
            .header(ORIGIN, origin)
            .json(&json!({ "name": "", "repos": [] }));
        if let Some(host) = forwarded_host {
            request = request.header("x-forwarded-host", host);
        }
        request.send()
    };
    // Past the origin guard means a 400 for the empty body, not a 403.
    assert_eq!(
        post(&public, None).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let proxied = "https://other.tail1234.ts.net";
    assert_eq!(
        post(proxied, None).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    // A local proxy's forwarded host is same-origin.
    assert_eq!(
        post(proxied, Some("other.tail1234.ts.net"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post("https://evil.example", Some("other.tail1234.ts.net"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    // Logins arriving under the https public URL get Secure cookies.
    let code = hub.mint().await;
    let login = hub
        .http
        .get(hub.url(&format!("/login?code={code}")))
        .header("host", "box.tail1234.ts.net")
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    assert!(
        login.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .ends_with("; Secure")
    );
    let code = hub.mint().await;
    let plain = hub
        .http
        .get(hub.url(&format!("/login?code={code}")))
        .send()
        .await
        .unwrap();
    assert!(
        !plain.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Secure")
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn colour_queries_answer_with_the_viewers_theme() {
    let hub = TestHub::start(ServerOptions::default()).await;
    // Wait for a go line, then ask for the background colour in raw mode
    // and show the reply.
    let script = "read _; stty raw -echo min 0 time 5; printf '\\033]11;?\\007'; sleep 1; \
                  r=$(dd bs=256 count=1 2>/dev/null); stty sane; \
                  printf '\\nreply:'; printf '%s' \"$r\" | od -An -c | tr -s ' '; echo end; \
                  sleep 30";
    let sid = hub.shell(&["/bin/sh", "-c", script]).await;
    let login = hub
        .http
        .get(hub.url(&format!("/login?token={}", hub.token())))
        .send()
        .await
        .unwrap();
    let mut ws = hub.connect(&sid, &cookie_pair(&login)).await;
    let ansi: Vec<String> = (0..16).map(|i| format!("#1020{i:02x}")).collect();
    let theme = json!({ "type": "theme", "foreground": "#23241f", "background": "#f2eee4",
                        "cursor": "#e2470c", "ansi": ansi });
    ws.send(Message::Text(theme.to_string().into()))
        .await
        .unwrap();
    // A malformed report is refused with an error, the last good one kept.
    let bad = json!({ "type": "theme", "foreground": "red", "background": "#000000",
                      "cursor": "#000000", "ansi": ansi });
    ws.send(Message::Text(bad.to_string().into()))
        .await
        .unwrap();
    ws.send(Message::Text(
        json!({ "type": "input", "data": "go\r" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let mut error = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut output = String::new();
    while !output.contains("end") {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, ws.next()).await.expect("reply in time") {
            Some(Ok(Message::Binary(bytes))) => output.push_str(&String::from_utf8_lossy(&bytes)),
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "error" {
                    error = Some(value["message"].as_str().unwrap().to_owned());
                }
            }
            Some(Ok(_)) => {}
            other => panic!("socket ended: {other:?}; saw {output:?}"),
        }
    }
    assert!(
        error.as_deref().is_some_and(|e| e.contains("foreground")),
        "{error:?}"
    );
    // ESC ] 1 1 ; r g b : f 2 f 2 / e e e e / e 4 e 4 BEL
    let reply = output.split("reply:").nth(1).unwrap().replace("\r\n", " ");
    let reply: String = reply.split_whitespace().collect();
    assert!(reply.starts_with("033]11;rgb:f2f2/eeee/e4e4\\a"), "{reply}");
    hub.shutdown().await;
}

/// Run `fw hub <args>` in-process.
fn cli(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = flywheel_hub::cli::run(&args, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_and_rotate_verbs_drive_the_running_hub() {
    let hub = TestHub::start(ServerOptions::default()).await;
    let state = hub.state_dir.to_str().unwrap().to_owned();

    // No runtime file: no running hub to ask.
    let (code, _, err) = tokio::task::spawn_blocking(move || cli(&["url", "--state-dir", &state]))
        .await
        .unwrap();
    assert_eq!(code, 1);
    assert!(err.contains("no hub is running"), "{err}");

    // The hub holds the serve lock; `fw hub serve` records this. Do it for
    // the test server (this process is its pid).
    assert!(
        ServeLock::acquire(&hub.state_dir).unwrap().is_none(),
        "the open hub holds the serve lock"
    );
    RuntimeInfo {
        pid: std::process::id(),
        listen: hub.addr,
        public_urls: vec!["https://box.tail1234.ts.net".into()],
        started_at: None,
        log_file: None,
    }
    .write(&hub.state_dir)
    .unwrap();
    let state = hub.state_dir.to_str().unwrap().to_owned();
    let (code, out, err) =
        tokio::task::spawn_blocking(move || cli(&["url", "--qr", "--state-dir", &state]))
            .await
            .unwrap();
    assert_eq!(code, 0, "{err}");
    let token = hub.token();
    assert!(!out.contains(&token), "the token is never shown");
    assert!(out.contains("public"), "{out}");
    assert!(out.contains("this machine"), "{out}");
    assert!(out.contains("scan to log in (public)"), "{out}");
    assert!(out.contains('\u{2588}'), "a QR code is drawn");
    let local = out
        .lines()
        .find(|l| l.contains("this machine"))
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned();
    assert!(local.starts_with(&format!("http://127.0.0.1:{}/login?code=", hub.addr.port())));
    let public = out
        .lines()
        .find(|l| l.trim_start().starts_with("public"))
        .unwrap();
    let login_code = local.rsplit('=').next().unwrap();
    assert!(public.ends_with(&format!(
        "https://box.tail1234.ts.net/login?code={login_code}"
    )));
    let login = hub.http.get(&local).send().await.unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = cookie_pair(&login);

    // Rotate through the running hub: the old login stops at once.
    let state = hub.state_dir.to_str().unwrap().to_owned();
    let (code, out, err) =
        tokio::task::spawn_blocking(move || cli(&["token", "--rotate", "--state-dir", &state]))
            .await
            .unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(out.is_empty(), "rotation prints no token: {out}");
    assert!(err.contains("signed out every browser"), "{err}");
    assert_ne!(hub.token(), token);
    let stale = hub
        .http
        .get(hub.url("/api/workspaces"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
    let state = hub.state_dir.to_str().unwrap().to_owned();
    let (_, printed, _) =
        tokio::task::spawn_blocking(move || cli(&["token", "--state-dir", &state]))
            .await
            .unwrap();
    assert_eq!(printed.trim(), hub.token());

    // A runtime file whose hub died without cleaning up (its lock is
    // gone) means no hub -- even when the recorded pid is alive again (here
    // this very process) and something listens at the recorded address
    // (here the hub, still up): nothing is sent there, rotation edits the
    // file, and the stale file is removed. The dead hub's state dir is a
    // copy of the live one's token with no lock holder.
    let dead_dir = tempfile::tempdir().unwrap();
    let dead = dead_dir.path().join("state");
    flywheel_hub::paths::ensure_private_dir(&dead).unwrap();
    std::fs::copy(hub.state_dir.join("token"), dead.join("token")).unwrap();
    RuntimeInfo {
        pid: std::process::id(),
        listen: hub.addr,
        public_urls: Vec::new(),
        started_at: None,
        log_file: None,
    }
    .write(&dead)
    .unwrap();
    let (code, _, err) = {
        let state = dead.to_str().unwrap().to_owned();
        tokio::task::spawn_blocking(move || cli(&["url", "--state-dir", &state]))
            .await
            .unwrap()
    };
    assert_eq!(code, 1);
    assert!(err.contains("no hub is running"), "{err}");
    assert!(!dead.join("server.json").exists(), "stale file removed");
    RuntimeInfo {
        pid: std::process::id(),
        listen: hub.addr,
        public_urls: Vec::new(),
        started_at: None,
        log_file: None,
    }
    .write(&dead)
    .unwrap();
    let served = hub.token();
    let before = std::fs::read_to_string(dead.join("token")).unwrap();
    let state = dead.to_str().unwrap().to_owned();
    let (code, _, err) =
        tokio::task::spawn_blocking(move || cli(&["token", "--rotate", "--state-dir", &state]))
            .await
            .unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("no hub is running"), "{err}");
    assert_ne!(std::fs::read_to_string(dead.join("token")).unwrap(), before);
    assert_eq!(
        hub.token(),
        served,
        "the listener was never asked to rotate"
    );
    let _dir = hub.shutdown().await;
}
