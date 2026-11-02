//! Push notifications end to end: a real hub on 127.0.0.1:0 with a
//! scripted Claude Code stand-in reporting over the real report socket,
//! and a real HTTP server standing in for ntfy that records every POST.

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::routing::post;
use flywheel_hub::notify::{Notifier, NotifyConfig};
use flywheel_hub::report::{self, Report};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::SinkExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const REPORT_PROGRAM: &str = "/opt/flywheel/bin/fw";
const TOPIC: &str = "s3cret-topic-7f3a";

/// One POST the stand-in ntfy received.
#[derive(Debug, Clone)]
struct Received {
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

/// A real HTTP server recording POSTs; answers `status`.
struct Webhook {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Webhook {
    async fn start(status: u16) -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let app = axum::Router::new().route(
            "/{*topic}",
            post(
                move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
                    let sink = Arc::clone(&sink);
                    async move {
                        sink.lock().unwrap().push(Received {
                            path: uri.path().to_owned(),
                            headers: headers
                                .iter()
                                .map(|(k, v)| {
                                    (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned())
                                })
                                .collect(),
                            body: String::from_utf8_lossy(&body).into_owned(),
                        });
                        StatusCode::from_u16(status).unwrap()
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, received }
    }

    fn url(&self) -> String {
        format!("http://{}/{TOPIC}", self.addr)
    }

    fn all(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    /// Wait for the `n`th push (1-based).
    async fn nth(&self, n: usize) -> Received {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(r) = self.all().get(n - 1) {
                return r.clone();
            }
            assert!(Instant::now() < deadline, "push {n} never came");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    cookie: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start(notify: Option<NotifyConfig>) -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let claude = fake_claude(dir.path());
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.agents.claude = claude.to_string_lossy().into_owned();
        options.report_command = Some(vec![REPORT_PROGRAM.into(), "hub".into()]);
        options.claude_config_dir = Some(dir.path().join("claude-config"));
        let hub = Hub::open(options).unwrap();
        let notifier = notify.map(|config| {
            let notifier = Notifier::new(config).unwrap();
            notifier.watch(&hub);
            notifier
        });
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            hub,
            ServerOptions {
                notifier,
                ..ServerOptions::default()
            },
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
        let cookie = login.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        Self {
            dir,
            addr,
            token,
            cookie,
            http,
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn authed(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("http://{}{path}", self.addr))
            .bearer_auth(&self.token)
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let response = self
            .authed(reqwest::Method::POST, path)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.json().await.unwrap_or(Value::Null)
    }

    /// A claude session in a fresh workspace: (session id, its hook env).
    async fn claude(&self) -> (String, HashMap<String, String>) {
        let repo = self.dir.path().join("repo").canonicalize().unwrap();
        let workspace = self
            .post(
                "/api/workspaces",
                json!({ "name": "infrared", "repos": [repo] }),
            )
            .await;
        let session = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": workspace["id"], "kind": "claude", "name": "fixer" }),
            )
            .await;
        let env_file = self.dir.path().join("claude.env");
        let deadline = Instant::now() + Duration::from_secs(10);
        let env = loop {
            if let Ok(text) = std::fs::read_to_string(&env_file)
                && !text.is_empty()
            {
                break text
                    .lines()
                    .filter_map(|l| l.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect::<HashMap<_, _>>();
            }
            assert!(Instant::now() < deadline, "claude never launched");
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        (session["id"].as_str().unwrap().to_owned(), env)
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

fn fake_claude(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nenv > '{d}.env.tmp' && mv '{d}.env.tmp' '{d}.env'\nexec cat\n",
            d = path.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Send the report Claude's hook for `event` sends.
fn fire(env: &HashMap<String, String>, event: &str, detail: Option<&str>, payload: Value) {
    report::send(
        Path::new(&env[report::SOCKET_ENV]),
        &Report {
            session: env[report::SESSION_ENV].clone(),
            secret: env[report::SECRET_ENV].clone(),
            source: "claude".into(),
            event: event.into(),
            detail: detail.map(str::to_owned),
            payload: Some(payload),
        },
    )
    .unwrap();
}

fn config(webhook: &Webhook, file: &str) -> NotifyConfig {
    let mut config = NotifyConfig::from_parts(Some(file), Some(&webhook.url()))
        .unwrap()
        .unwrap();
    config.click_base = Some("https://box.tail.ts.net".into());
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_and_done_push_once_with_names_and_detail_only() {
    let webhook = Webhook::start(200).await;
    let hub = TestHub::start(Some(config(
        &webhook,
        "token = \"tk_test\"\nsettle_secs = 0\ndebounce_secs = 600\n",
    )))
    .await;
    let (sid, env) = hub.claude().await;

    fire(
        &env,
        "UserPromptSubmit",
        None,
        json!({ "prompt": "rm -rf secret-dir" }),
    );
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Bash", "tool_input": { "command": "cat ~/.ssh/id_rsa" } }),
    );
    let blocked = webhook.nth(1).await;
    assert_eq!(blocked.path, format!("/{TOPIC}"));
    assert_eq!(blocked.headers["title"], "fixer is blocked");
    assert_eq!(blocked.headers["priority"], "high");
    assert_eq!(blocked.headers["tags"], "flywheel,blocked");
    assert_eq!(blocked.headers["authorization"], "Bearer tk_test");
    assert_eq!(
        blocked.headers["click"],
        format!("https://box.tail.ts.net/#session={sid}")
    );
    assert_eq!(
        blocked.body,
        "infrared / fixer (claude) waits on you: permission to use Bash"
    );
    // Nothing from the prompt, the tool input, or the hub's secrets.
    for secret in [
        "secret-dir",
        "id_rsa",
        hub.token.as_str(),
        &env[report::SECRET_ENV],
    ] {
        assert!(!format!("{blocked:?}").contains(secret), "{secret} leaked");
    }

    fire(&env, "PostToolUse", None, json!({ "tool_name": "Bash" }));
    fire(&env, "Stop", None, json!({}));
    let done = webhook.nth(2).await;
    assert_eq!(done.headers["title"], "fixer is done");
    assert_eq!(done.headers["priority"], "default");
    assert_eq!(done.body, "infrared / fixer (claude) finished its turn.");

    // Debounced: the same statuses again within the window send nothing
    // (they would be held until the window ends).
    fire(&env, "UserPromptSubmit", None, json!({}));
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Edit" }),
    );
    fire(&env, "PostToolUse", None, json!({}));
    fire(&env, "Stop", None, json!({}));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(webhook.all().len(), 2, "{:?}", webhook.all());

    // The notify API shows the host only, and a test push goes out.
    let status: Value = hub
        .authed(reqwest::Method::GET, "/api/notify")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["push"]["host"], webhook.addr.to_string());
    assert_eq!(status["push"]["events"], json!(["blocked", "done"]));
    assert!(!status.to_string().contains(TOPIC));
    let test = hub
        .authed(reqwest::Method::POST, "/api/notify/test")
        .send()
        .await
        .unwrap();
    assert_eq!(test.status(), StatusCode::NO_CONTENT);
    let pushed = webhook.nth(3).await;
    assert_eq!(pushed.headers["tags"], "flywheel,test");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watched_session_or_a_spell_shorter_than_settle_does_not_push() {
    let webhook = Webhook::start(200).await;
    let hub = TestHub::start(Some(config(
        &webhook,
        "settle_secs = 1\ndebounce_secs = 0\nevents = [\"blocked\", \"done\"]\n",
    )))
    .await;
    let (sid, env) = hub.claude().await;

    // Approved within the settle time: no push.
    fire(&env, "UserPromptSubmit", None, json!({}));
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Bash" }),
    );
    fire(&env, "PostToolUse", None, json!({}));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(webhook.all().is_empty(), "{:?}", webhook.all());

    // A browser looking at the session: neither blocked nor done pushes.
    let mut request = format!("ws://{}/api/events?client=laptop", hub.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("cookie", hub.cookie.parse().unwrap());
    let (mut events, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    events
        .send(Message::Text(
            json!({ "type": "view", "session_id": sid })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Bash" }),
    );
    fire(&env, "PostToolUse", None, json!({}));
    fire(&env, "Stop", None, json!({}));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(webhook.all().is_empty(), "{:?}", webhook.all());

    // It looks away: the next blocked spell pushes.
    events
        .send(Message::Text(
            json!({ "type": "view", "session_id": null })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    fire(&env, "UserPromptSubmit", None, json!({}));
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Write" }),
    );
    let pushed = webhook.nth(1).await;
    assert!(
        pushed.body.ends_with("permission to use Write"),
        "{pushed:?}"
    );
    hub.shutdown().await;
}

/// An events socket for client `client` looking at `session` (or nothing).
async fn viewer(
    hub: &TestHub,
    client: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("ws://{}/api/events?client={client}", hub.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("cookie", hub.cookie.parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn look_at<S>(events: &mut S, session: Option<&str>)
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    events
        .send(Message::Text(
            json!({ "type": "view", "session_id": session })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spell_blocked_while_watched_pushes_once_the_last_viewer_leaves() {
    let webhook = Webhook::start(200).await;
    let hub = TestHub::start(Some(config(
        &webhook,
        "settle_secs = 1\ndebounce_secs = 0\nevents = [\"blocked\"]\n",
    )))
    .await;
    let (sid, env) = hub.claude().await;

    // Two tabs watch it block, and stay past the settle time: no push.
    let mut laptop = viewer(&hub, "laptop").await;
    let mut desk = viewer(&hub, "desk").await;
    look_at(&mut laptop, Some(&sid)).await;
    look_at(&mut desk, Some(&sid)).await;
    fire(&env, "UserPromptSubmit", None, json!({}));
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Bash" }),
    );
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert!(webhook.all().is_empty(), "{:?}", webhook.all());

    // One looks away: the other still watches.
    look_at(&mut laptop, None).await;
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert!(webhook.all().is_empty(), "{:?}", webhook.all());

    // The last one goes (its socket drops): the still-blocked spell
    // pushes, once.
    drop(desk);
    let pushed = webhook.nth(1).await;
    assert_eq!(pushed.headers["title"], "fixer is blocked");
    look_at(&mut laptop, Some(&sid)).await;
    look_at(&mut laptop, None).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(webhook.all().len(), 1, "{:?}", webhook.all());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_block_inside_the_debounce_window_pushes_when_it_ends() {
    let webhook = Webhook::start(200).await;
    let hub = TestHub::start(Some(config(
        &webhook,
        "settle_secs = 0\ndebounce_secs = 3\nevents = [\"blocked\"]\n",
    )))
    .await;
    let (_sid, env) = hub.claude().await;
    let started = Instant::now();
    fire(&env, "UserPromptSubmit", None, json!({}));
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Bash" }),
    );
    let first = webhook.nth(1).await;
    assert!(first.body.ends_with("permission to use Bash"), "{first:?}");
    // Approved, then the next prompt: held, not dropped.
    fire(&env, "PostToolUse", None, json!({}));
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Write" }),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(webhook.all().len(), 1, "{:?}", webhook.all());
    let second = webhook.nth(2).await;
    assert!(
        second.body.ends_with("permission to use Write"),
        "{second:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(2900));
    // A held spell answered before the window ends never pushes.
    fire(&env, "PostToolUse", None, json!({}));
    fire(
        &env,
        "PermissionRequest",
        None,
        json!({ "tool_name": "Edit" }),
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    fire(&env, "PostToolUse", None, json!({}));
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(webhook.all().len(), 2, "{:?}", webhook.all());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_push_the_api_says_off_and_a_failing_webhook_is_reported() {
    let hub = TestHub::start(None).await;
    let status: Value = hub
        .authed(reqwest::Method::GET, "/api/notify")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status, json!({ "push": null }));
    let test = hub
        .authed(reqwest::Method::POST, "/api/notify/test")
        .send()
        .await
        .unwrap();
    assert_eq!(test.status(), StatusCode::CONFLICT);
    hub.shutdown().await;

    let webhook = Webhook::start(403).await;
    let hub = TestHub::start(Some(config(&webhook, "settle_secs = 0\n"))).await;
    let test = hub
        .authed(reqwest::Method::POST, "/api/notify/test")
        .send()
        .await
        .unwrap();
    assert_eq!(test.status(), StatusCode::BAD_GATEWAY);
    let error: Value = test.json().await.unwrap();
    let message = error["error"].as_str().unwrap();
    assert!(message.contains("403"), "{message}");
    assert!(!message.contains(TOPIC), "{message}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirects_are_not_followed_and_errors_never_name_the_topic() {
    flywheel_hub::install_crypto_provider();
    let target = Webhook::start(200).await;
    let location = target.url();
    let app = axum::Router::new().route(
        "/{*topic}",
        post(move || {
            let location = location.clone();
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(reqwest::header::LOCATION, location)],
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let notifier = Notifier::new(
        NotifyConfig::from_parts(None, Some(&format!("http://{addr}/{TOPIC}")))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let message = notifier.send_test().await.unwrap_err();
    assert!(message.contains("307"), "{message}");
    assert!(!message.contains(TOPIC), "{message}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(target.all().is_empty(), "the redirect was followed");

    // Nothing listens: the error names the host only.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = closed.local_addr().unwrap();
    drop(closed);
    let notifier = Notifier::new(
        NotifyConfig::from_parts(None, Some(&format!("http://me:pw@{dead}/{TOPIC}")))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let message = notifier.send_test().await.unwrap_err();
    assert!(message.contains("cannot connect"), "{message}");
    assert!(!message.contains(TOPIC), "{message}");
    assert!(!message.contains("pw"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_credentials_are_sent_as_basic_auth_only() {
    flywheel_hub::install_crypto_provider();
    let webhook = Webhook::start(200).await;
    let url = format!("http://me:p%40ss@{}/{TOPIC}", webhook.addr);
    let notifier =
        Notifier::new(NotifyConfig::from_parts(None, Some(&url)).unwrap().unwrap()).unwrap();
    notifier.send_test().await.unwrap();
    let got = webhook.nth(1).await;
    assert_eq!(got.path, format!("/{TOPIC}"));
    // "me:p@ss"
    assert_eq!(
        got.headers.get("authorization").map(String::as_str),
        Some("Basic bWU6cEBzcw==")
    );
    assert_eq!(
        got.headers.get("title").map(String::as_str),
        Some("flywheel hub")
    );
    assert!(
        got.headers["user-agent"].starts_with("flywheel-hub/"),
        "{:?}",
        got.headers
    );
}
