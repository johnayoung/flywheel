//! The activity journal end to end: real hubs on 127.0.0.1:0, real PTYs,
//! agent reports over the real report socket, the events WebSocket, and a
//! second hub registered as a machine.
//!
//! - rows follow the bus (session created, turn finished, blocked),
//!   arrive as `activity` pushes, page by cursor, and survive a restart;
//! - nothing a terminal printed (a fake token) reaches the journal;
//! - each client's digest starts where that client was last active, and
//!   a return after a while gets one `away` message;
//! - a hub merges a machine's rows and digest, labelled, and says
//!   `partial` when the machine is gone.

mod support;

use flywheel_hub::report::{self, Report};
use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const FAKE_TOKEN: &str = "ghp_FakeTokenZ9y8X7w6V5u4T3s2R1q0PpOoNn";

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    hub: Arc<Hub>,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start() -> Self {
        flywheel_hub::install_crypto_provider();
        Self::start_in(tempfile::tempdir().unwrap()).await
    }

    async fn start_in(dir: tempfile::TempDir) -> Self {
        std::fs::create_dir_all(dir.path().join("repo")).unwrap();
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
            dir,
            addr,
            hub,
            http: reqwest::Client::new(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn token(&self) -> String {
        self.hub.token().expose().to_owned()
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
            .bearer_auth(self.token());
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

    async fn get(&self, path: &str) -> Value {
        let (status, body) = self.call(reqwest::Method::GET, path, None).await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (status, answer) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(status.is_success(), "{path}: {status} {answer}");
        answer
    }

    async fn workspace(&self, name: &str) -> String {
        let ws = self
            .post(
                "/api/workspaces",
                json!({ "name": name, "folders": [self.dir.path().join("repo")] }),
            )
            .await;
        ws["id"].as_str().unwrap().to_owned()
    }

    async fn session(&self, workspace: &str, kind: &str) -> String {
        let session = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": workspace, "kind": kind, "cols": 80, "rows": 24 }),
            )
            .await;
        session["id"].as_str().unwrap().to_owned()
    }

    async fn events(&self, client: &str) -> Ws {
        let mut request = format!("ws://{}/api/events?client={client}", self.addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token()).parse().unwrap(),
        );
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    /// Every row `GET /api/activity` pages through at `limit` a page.
    async fn all_rows(&self, query: &str, limit: usize) -> (Vec<Value>, usize) {
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut path = format!("/api/activity?limit={limit}{query}");
            if let Some(c) = &cursor {
                path.push_str(&format!("&cursor={}", percent(c)));
            }
            let page = self.get(&path).await;
            pages += 1;
            let got = page["rows"].as_array().unwrap();
            assert!(got.len() <= limit, "{page}");
            rows.extend(got.iter().cloned());
            match page["next"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => return (rows, pages),
            }
            assert!(pages < 500, "paging never ends");
        }
    }

    async fn shutdown(mut self) -> tempfile::TempDir {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(15), self.server.take().unwrap())
            .await
            .expect("server shut down in time")
            .unwrap()
            .unwrap();
        self.dir
    }
}

fn percent(text: &str) -> String {
    text.replace(':', "%3A").replace(',', "%2C")
}

/// Wait until `GET /api/activity?{query}` has a row matching `want`.
async fn row_appears(hub: &TestHub, query: &str, want: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let page = hub.get(&format!("/api/activity?limit=500{query}")).await;
        if let Some(row) = page["rows"].as_array().unwrap().iter().find(|r| want(r)) {
            return row.clone();
        }
        assert!(Instant::now() < deadline, "no such row in {page}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// The environment the claude stand-in was launched with.
async fn launched_env(dir: &Path) -> HashMap<String, String> {
    let path = dir.join("claude.env");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(&path)
            && !text.is_empty()
        {
            let _ = std::fs::remove_file(&path);
            return text
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
        }
        assert!(Instant::now() < deadline, "claude never launched");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Send the report a Claude Code hook would for `event`.
fn hook(env: &HashMap<String, String>, event: &str, detail: Option<&str>) {
    report::send(
        Path::new(&env[report::SOCKET_ENV]),
        &Report {
            session: env[report::SESSION_ENV].clone(),
            secret: env[report::SECRET_ENV].clone(),
            source: "claude".into(),
            event: event.into(),
            detail: detail.map(str::to_owned),
            payload: Some(json!({ "session_id": "x" })),
        },
    )
    .unwrap();
}

async fn next_message(ws: &mut Ws, kind: &str, within: Duration) -> Option<Value> {
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, ws.next()).await {
            Err(_) => return None,
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == kind {
                    return Some(value);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(other) => panic!("events socket ended: {other:?}"),
        }
    }
}

async fn present(ws: &mut Ws, present: bool) {
    ws.send(Message::Text(
        json!({ "type": "view", "session_id": null, "visible": [], "present": present })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

/// One finished agent turn on `env`'s session: working, then done.
async fn turn(env: &HashMap<String, String>) {
    hook(env, "UserPromptSubmit", None);
    tokio::time::sleep(Duration::from_millis(50)).await;
    hook(env, "Stop", None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_journal_follows_the_bus_pages_by_cursor_and_survives_a_restart() {
    let hub = TestHub::start().await;
    let mut laptop = hub.events("laptop").await;
    let wid = hub.workspace("alpha").await;

    // A shell prints a token; nothing on a terminal reaches the journal.
    let shell = hub.session(&wid, "shell").await;
    let created = next_message(&mut laptop, "activity", Duration::from_secs(10))
        .await
        .expect("an activity push for the new session");
    assert_eq!(created["row"]["kind"], "session.created");
    assert_eq!(created["row"]["session_id"], shell);
    assert_eq!(created["row"]["workspace"], "alpha");
    hub.post(
        &format!("/api/sessions/{shell}/input"),
        json!({ "data": format!("echo {FAKE_TOKEN}; echo export API_KEY={FAKE_TOKEN}\n") }),
    )
    .await;

    // An agent: working, blocked, done.
    let agent = hub.session(&wid, "claude").await;
    let env = launched_env(hub.dir.path()).await;
    hook(&env, "UserPromptSubmit", None);
    hook(&env, "Notification", Some("permission_prompt"));
    let blocked = row_appears(&hub, "&kind=status.changed", |r| r["session_id"] == agent).await;
    assert_eq!(blocked["data"]["status"], "blocked");
    hook(&env, "Stop", None);
    let finished = row_appears(&hub, "&kind=turn", |r| r["kind"] == "turn.finished").await;
    assert_eq!(finished["session_id"], agent);
    // A watched turn is a finished turn too (seen at once: idle, not done).
    present(&mut laptop, true).await;
    laptop
        .send(Message::Text(
            json!({ "type": "view", "session_id": agent, "present": true })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    turn(&env).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let page = hub.get("/api/activity?kind=turn.finished").await;
        if page["rows"].as_array().unwrap().len() == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the watched turn was not recorded: {page}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // The shell's output arrived (so the journal had its chance).
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let screen = hub
            .get(&format!(
                "/api/sessions/{shell}/screen?source=recent&lines=50"
            ))
            .await;
        if screen.to_string().contains("API_KEY=") {
            break;
        }
        assert!(Instant::now() < deadline, "{screen}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let journal = hub.state().join("activity.jsonl");
    let text = std::fs::read_to_string(&journal).unwrap();
    assert!(!text.contains(FAKE_TOKEN), "{text}");
    assert!(!text.contains("API_KEY"), "{text}");
    assert!(!text.contains(&hub.token()), "{text}");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&journal).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // Paging: two rows a page covers everything once, newest first.
    let (all, _) = hub.all_rows("", 500).await;
    let (paged, pages) = hub.all_rows("", 2).await;
    assert!(pages >= 3, "{pages}");
    assert_eq!(paged, all);
    let ids: Vec<u64> = paged.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "{ids:?}");
    // Filters and bad queries.
    let (only_ws, _) = hub.all_rows(&format!("&ws={wid}"), 3).await;
    assert_eq!(only_ws.len(), all.len());
    let none = hub.get("/api/activity?ws=nope").await;
    assert_eq!(none["rows"], json!([]));
    assert_eq!(none["next"], Value::Null);
    for bad in ["limit=0", "limit=501", "since=soon", "cursor=local:x"] {
        let (status, _) = hub
            .call(reqwest::Method::GET, &format!("/api/activity?{bad}"), None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let future = hub.get("/api/activity?since=0s").await;
    assert_eq!(future["rows"], json!([]));

    // A restart keeps the rows; new ones carry on from the last id.
    let last_id = ids[0];
    drop(laptop);
    let dir = hub.shutdown().await;
    let hub = TestHub::start_in(dir).await;
    let (again, _) = hub.all_rows("", 500).await;
    assert!(again.len() >= all.len(), "{} < {}", again.len(), all.len());
    hub.session(&wid, "shell").await;
    let newest = row_appears(&hub, "&kind=session.created", |r| {
        r["id"].as_u64().unwrap() > last_id
    })
    .await;
    assert!(newest["id"].as_u64().unwrap() > last_id);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_client_gets_its_own_digest_and_one_away_message_on_return() {
    flywheel_hub::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    // The laptop was last here an hour ago, the desk 3 minutes ago.
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let hour_ago = chrono::Utc::now() - chrono::Duration::hours(1);
    let recent = chrono::Utc::now() - chrono::Duration::minutes(3);
    std::fs::write(
        state.join("activity-clients.json"),
        json!({ "clients": { "laptop": hour_ago, "desk": recent } }).to_string(),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let hub = TestHub::start_in(dir).await;
    let wid = hub.workspace("beta").await;
    let agent = hub.session(&wid, "claude").await;
    let env = launched_env(hub.dir.path()).await;
    turn(&env).await;
    row_appears(&hub, "&kind=turn.finished", |_| true).await;

    // The phone saw everything up to here.
    let (status, _) = hub
        .call(
            reqwest::Method::POST,
            "/api/activity/seen",
            Some(json!({ "client": "phone" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    tokio::time::sleep(Duration::from_millis(20)).await;
    turn(&env).await;
    turn(&env).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while hub.get("/api/activity?kind=turn.finished").await["rows"]
        .as_array()
        .unwrap()
        .len()
        < 3
    {
        assert!(Instant::now() < deadline, "three turns never recorded");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    hook(&env, "UserPromptSubmit", None);
    hook(&env, "Notification", Some("permission_prompt"));
    row_appears(&hub, "&kind=status.changed", |_| true).await;

    let digest = |client: &'static str| {
        let hub = &hub;
        async move {
            hub.get(&format!(
                "/api/activity/digest?since=last-seen&client={client}"
            ))
            .await
        }
    };
    let laptop = digest("laptop").await;
    assert_eq!(laptop["turns_finished"], 3, "{laptop}");
    assert_eq!(laptop["last_seen"], true);
    assert_eq!(laptop["empty"], false);
    let blocked = laptop["blocked_now"].as_array().unwrap();
    assert_eq!(blocked.len(), 1, "{laptop}");
    assert_eq!(blocked[0]["session_id"], agent);
    assert_eq!(laptop["workspaces"][0]["workspace"], "beta");
    let phone = digest("phone").await;
    assert_eq!(phone["turns_finished"], 2, "{phone}");
    // Never seen: the last 24 hours.
    let stranger = digest("stranger").await;
    assert_eq!(stranger["last_seen"], false);
    assert_eq!(stranger["turns_finished"], 3);
    // An explicit span.
    let none = hub.get("/api/activity/digest?since=0s").await;
    assert_eq!(none["turns_finished"], 0);
    assert_eq!(none["blocked_now"].as_array().unwrap().len(), 1);
    let (status, _) = hub
        .call(
            reqwest::Method::GET,
            "/api/activity/digest?since=last-seen",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Back after an hour: one `away` covering it; back after 3 minutes: none.
    let mut ws = hub.events("laptop").await;
    present(&mut ws, true).await;
    let away = next_message(&mut ws, "away", Duration::from_secs(10))
        .await
        .expect("an away message");
    let since = chrono::DateTime::parse_from_rfc3339(away["since"].as_str().unwrap()).unwrap();
    assert!(
        (since.timestamp() - hour_ago.timestamp()).abs() <= 1,
        "{away}"
    );
    present(&mut ws, false).await;
    present(&mut ws, true).await;
    assert!(
        next_message(&mut ws, "away", Duration::from_millis(500))
            .await
            .is_none(),
        "only once"
    );
    // While present, its digest is empty of rows (it is here now).
    let now = digest("laptop").await;
    assert_eq!(now["events"], 0, "{now}");
    let mut desk = hub.events("desk").await;
    present(&mut desk, true).await;
    assert!(
        next_message(&mut desk, "away", Duration::from_millis(500))
            .await
            .is_none()
    );
    drop(ws);
    drop(desk);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let marks: Value =
        serde_json::from_slice(&std::fs::read(hub.state().join("activity-clients.json")).unwrap())
            .unwrap();
    let laptop_mark =
        chrono::DateTime::parse_from_rfc3339(marks["clients"]["laptop"].as_str().unwrap()).unwrap();
    assert!(
        laptop_mark.timestamp() > hour_ago.timestamp() + 3000,
        "{marks}"
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_merges_its_machines_activity_and_says_partial_when_one_is_gone() {
    let a = TestHub::start().await;
    let b = TestHub::start().await;
    let a_ws = a.workspace("here").await;
    let b_ws = b.workspace("there").await;
    for _ in 0..3 {
        a.session(&a_ws, "shell").await;
        b.session(&b_ws, "shell").await;
    }
    b.session(&b_ws, "shell").await;
    row_appears(&a, "&machines=none", |_| true).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while b.get("/api/activity").await["rows"]
        .as_array()
        .unwrap()
        .len()
        < 4
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let issued = b.post("/api/peers", json!({ "name": "hub-a" })).await;
    let credential = issued["credential"].as_str().unwrap().to_owned();
    a.post(
        "/api/machines",
        json!({ "name": "b", "url": format!("http://{}", b.addr), "credential": credential }),
    )
    .await;

    let local = a.get("/api/activity?machines=none").await["rows"]
        .as_array()
        .unwrap()
        .len();
    let remote = b.get("/api/activity").await["rows"]
        .as_array()
        .unwrap()
        .len();
    let merged = a.get("/api/activity").await;
    assert_eq!(merged["partial"], false, "{merged}");
    assert_eq!(merged["machines"], json!([{ "name": "b", "ok": true }]));
    let rows = merged["rows"].as_array().unwrap();
    assert_eq!(rows.len(), local + remote);
    assert!(
        rows.iter()
            .filter(|r| r["machine"] == "b")
            .all(|r| r["workspace"] == "there")
    );
    assert_eq!(rows.iter().filter(|r| r["machine"] == "b").count(), remote);
    let ats: Vec<&str> = rows.iter().map(|r| r["at"].as_str().unwrap()).collect();
    let mut sorted = ats.clone();
    sorted.sort_by(|x, y| y.cmp(x));
    assert_eq!(ats, sorted, "newest first across machines");

    // Paging one row at a time walks both hubs once each.
    let (paged, pages) = a.all_rows("", 1).await;
    assert_eq!(paged.len(), local + remote);
    assert!(pages >= local + remote);
    let keys: HashSet<(String, u64)> = paged
        .iter()
        .map(|r| {
            (
                r["machine"].as_str().unwrap_or("local").to_owned(),
                r["id"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(keys.len(), paged.len(), "no row twice");
    // A filter reaches the machine too.
    let (there, _) = a.all_rows(&format!("&ws={b_ws}"), 2).await;
    assert_eq!(there.len(), remote);

    // The digest adds the machine's in.
    let digest = a.get("/api/activity/digest?since=1h").await;
    assert_eq!(
        digest["events"].as_u64().unwrap() as usize,
        local + remote,
        "{digest}"
    );
    assert!(
        digest["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["machine"] == "b")
    );
    assert_eq!(digest["partial"], false);
    // A peer's request is answered from that hub alone (no loops).
    let as_peer = a
        .http
        .get(format!("http://{}/api/activity", b.addr))
        .bearer_auth(&credential)
        .send()
        .await
        .unwrap();
    let as_peer: Value = as_peer.json().await.unwrap();
    assert_eq!(as_peer["machines"], json!([]));

    // B goes away: A answers with its own rows, partial, naming b.
    b.shutdown().await;
    let alone = a.get("/api/activity").await;
    assert_eq!(alone["partial"], true, "{alone}");
    assert_eq!(alone["machines"][0]["name"], "b");
    assert_eq!(alone["machines"][0]["ok"], false);
    assert_eq!(alone["rows"].as_array().unwrap().len(), local);
    let digest = a.get("/api/activity/digest?since=1h").await;
    assert_eq!(digest["partial"], true);
    assert_eq!(digest["events"].as_u64().unwrap() as usize, local);
    a.shutdown().await;
}
