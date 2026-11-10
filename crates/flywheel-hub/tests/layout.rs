//! Workspace layouts (tabs + split panes) over the real HTTP API: saved,
//! validated, revisioned, pushed to events clients, pruned when a session
//! goes, and kept across a restart that restores the sessions they show.

mod support;

use flywheel_hub::{HubOptions, ServerOptions, serve};
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::ORIGIN;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestHub {
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    /// Open the hub on `state_dir`, restore what a clean shutdown left
    /// (as `fw hub serve` does), and serve it on 127.0.0.1:0.
    async fn start(state_dir: PathBuf) -> Self {
        flywheel_hub::install_crypto_provider();
        let mut options = HubOptions::new(state_dir);
        options.default_shell = vec!["/bin/sh".into()];
        let hub = support::open_hub(options);
        let restore = std::sync::Arc::clone(&hub);
        tokio::task::spawn_blocking(move || restore.restore_sessions())
            .await
            .unwrap();
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
            let _ = stopped.await;
        }));
        Self {
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

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .http
            .request(method, self.url(path))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn events(&self) -> Ws {
        let url = format!("ws://{}/api/events?client=layout-test", self.addr);
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws
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

/// The next events message of `kind`, skipping others.
async fn next_event(ws: &mut Ws, kind: &str) -> Value {
    timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let value: Value = serde_json::from_str(text.as_str()).unwrap();
                    if value["type"] == kind {
                        return value;
                    }
                }
                Some(Ok(_)) => {}
                other => panic!("events socket ended: {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} event"))
}

fn pane(id: &str, session: Option<&str>) -> Value {
    json!({ "type": "pane", "id": id, "session": session })
}

fn split(dir: &str, ratio: f64, a: Value, b: Value) -> Value {
    json!({ "type": "split", "dir": dir, "ratio": ratio, "a": a, "b": b })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layouts_save_validate_push_prune_and_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let hub = TestHub::start(state_dir.clone()).await;
    use reqwest::Method;

    let (status, ws) = hub
        .call(
            Method::POST,
            "/api/workspaces",
            Some(json!({ "name": "w", "folders": [repo] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let wid = ws["id"].as_str().unwrap().to_owned();
    let (_, other) = hub
        .call(
            Method::POST,
            "/api/workspaces",
            Some(json!({ "name": "o", "folders": [repo] })),
        )
        .await;
    let oid = other["id"].as_str().unwrap().to_owned();
    let mut sids = Vec::new();
    for workspace in [&wid, &wid, &wid, &oid] {
        let (status, session) = hub
            .call(
                Method::POST,
                "/api/sessions",
                Some(json!({ "workspace_id": workspace, "cols": 80, "rows": 24 })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        sids.push(session["id"].as_str().unwrap().to_owned());
    }
    let path = format!("/api/workspaces/{wid}/layout");

    // Never laid out: revision 0, no tabs. Unknown workspace: 404.
    let (status, empty) = hub.call(Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty, json!({ "workspace_id": wid, "rev": 0, "tabs": [] }));
    let (status, _) = hub
        .call(Method::GET, "/api/workspaces/nope/layout", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Token required; cross-origin writes refused.
    let anonymous = hub.http.get(hub.url(&path)).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let cross = hub
        .http
        .put(hub.url(&path))
        .bearer_auth(&hub.token)
        .header(ORIGIN, "http://evil.example")
        .json(&json!({ "rev": 0, "tabs": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), StatusCode::FORBIDDEN);

    let mut events = hub.events().await;
    // Three panes in "agents", one in "logs".
    let tabs = json!([
        { "id": "t1", "name": " agents ", "root": split("right", 0.6,
            pane("p1", Some(&sids[0])),
            split("down", 0.5, pane("p2", Some(&sids[1])), pane("p3", Some(&sids[2])))) },
        { "id": "t2", "name": "logs", "root": pane("p4", Some(&sids[2])) },
    ]);
    let (status, saved) = hub
        .call(Method::PUT, &path, Some(json!({ "rev": 0, "tabs": tabs })))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["rev"], 1);
    assert_eq!(saved["tabs"][0]["name"], "agents", "trimmed");
    let pushed = next_event(&mut events, "layout").await;
    assert_eq!(
        pushed,
        json!({ "type": "layout", "workspace_id": wid, "rev": 1 })
    );
    let (_, read) = hub.call(Method::GET, &path, None).await;
    assert_eq!(read, saved);

    // A stale revision: 409 with the current layout to rebase onto.
    let (status, stale) = hub
        .call(Method::PUT, &path, Some(json!({ "rev": 0, "tabs": [] })))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        stale["error"].as_str().unwrap().contains("revision 1"),
        "{stale}"
    );
    assert_eq!(stale["layout"], saved);

    // Validation: 400 with why, nothing saved.
    for (tabs, why) in [
        (
            json!([{ "id": "t1", "name": "a", "root": pane("p1", Some(&sids[3])) }]),
            "not a session of this workspace",
        ),
        (
            json!([{ "id": "t1", "name": "a", "root": split("right", 1.0, pane("p1", None), pane("p2", None)) }]),
            "ratio",
        ),
        (
            json!([{ "id": "t1", "name": "a", "root": split("right", 0.5, pane("p1", Some(&sids[0])), pane("p2", Some(&sids[0]))) }]),
            "two panes",
        ),
        (
            json!([{ "id": "t1", "name": "a", "root": pane("t1", None) }]),
            "twice",
        ),
    ] {
        let (status, body) = hub
            .call(Method::PUT, &path, Some(json!({ "rev": 1, "tabs": tabs })))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains(why), "{body}");
    }
    // A body of the wrong shape is the API's usual 422.
    let (status, body) = hub
        .call(
            Method::PUT,
            &path,
            Some(json!({ "rev": 1, "tabs": [{ "id": "t1", "name": "a", "root": { "type": "tile" } }] })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // A tree nested past serde's recursion limit is refused with a short
    // message (the field path is not echoed in full).
    let mut deep = pane("p0", None);
    for i in 1..200 {
        deep = split("right", 0.5, deep, pane(&format!("p{i}"), None));
    }
    let (status, body) = hub
        .call(
            Method::PUT,
            &path,
            Some(json!({ "rev": 1, "tabs": [{ "id": "t1", "name": "a", "root": deep }] })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    let message = body["error"].as_str().unwrap();
    assert!(
        message.chars().count() <= 260,
        "{} chars: {message}",
        message.len()
    );
    assert!(message.contains("recursion limit"), "{message}");
    let (_, read) = hub.call(Method::GET, &path, None).await;
    assert_eq!(read["rev"], 1);

    // Removing a session closes its panes: p2 goes, p3 takes its place.
    let (status, _) = hub
        .call(Method::DELETE, &format!("/api/sessions/{}", sids[1]), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let pushed = next_event(&mut events, "layout").await;
    assert_eq!(pushed["rev"], 2);
    let (_, pruned) = hub.call(Method::GET, &path, None).await;
    assert_eq!(
        pruned["tabs"][0]["root"],
        split(
            "right",
            0.6,
            pane("p1", Some(&sids[0])),
            pane("p3", Some(&sids[2]))
        )
    );
    drop(events);

    // A clean restart restores the sessions under the same ids, so the
    // saved layout re-attaches as it was.
    hub.shutdown().await;
    let restarted = TestHub::start(state_dir).await;
    let (_, after) = restarted.call(Method::GET, &path, None).await;
    assert_eq!(after, pruned);
    let (_, sessions) = restarted.call(Method::GET, "/api/sessions", None).await;
    for sid in [&sids[0], &sids[2]] {
        let session = sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == sid.as_str())
            .unwrap();
        assert_eq!(session["state"], "running", "{session}");
        assert!(
            !session["live"].is_null(),
            "restored with a terminal: {session}"
        );
    }
    restarted.shutdown().await;
}

impl TestHub {
    async fn attach(&self, session_id: &str) -> Ws {
        let url = format!("ws://{}/api/sessions/{session_id}/ws", self.addr);
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws
    }
}

/// The next text message of `kind` on a terminal socket (output skipped).
async fn next_control(ws: &mut Ws, kind: &str) -> Value {
    timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let value: Value = serde_json::from_str(text.as_str()).unwrap();
                    if value["type"] == kind {
                        return value;
                    }
                }
                Some(Ok(_)) => {}
                other => panic!("terminal socket ended: {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} message"))
}

/// A pane that does not size the terminal follows whoever does: every
/// attached client hears each size change, in order with the output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_viewer_hears_when_another_resizes_the_terminal() {
    use futures_util::SinkExt;
    use reqwest::Method;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let hub = TestHub::start(dir.path().join("state")).await;
    let (_, ws) = hub
        .call(
            Method::POST,
            "/api/workspaces",
            Some(json!({ "name": "w", "folders": [repo] })),
        )
        .await;
    let (_, session) = hub
        .call(
            Method::POST,
            "/api/sessions",
            Some(json!({ "workspace_id": ws["id"], "cols": 80, "rows": 24 })),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_owned();
    let mut big = hub.attach(&sid).await;
    let mut small = hub.attach(&sid).await;
    let snapshot = next_control(&mut small, "snapshot").await;
    assert_eq!(
        (snapshot["cols"].as_u64(), snapshot["rows"].as_u64()),
        (Some(80), Some(24))
    );
    next_control(&mut big, "snapshot").await;

    big.send(Message::Text(
        json!({ "type": "resize", "cols": 132, "rows": 40 })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let size = next_control(&mut small, "size").await;
    assert_eq!(size, json!({ "type": "size", "cols": 132, "rows": 40 }));
    // The resizer hears it too (it may be a pane following another).
    assert_eq!(next_control(&mut big, "size").await["cols"], 132);
    // The same size again changes nothing and says nothing.
    big.send(Message::Text(
        json!({ "type": "resize", "cols": 132, "rows": 40 })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    big.send(Message::Text(
        json!({ "type": "resize", "cols": 100, "rows": 30 })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    assert_eq!(next_control(&mut small, "size").await["cols"], 100);
    drop((big, small));
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_tabs_are_refused_off_root_or_off_workspace_and_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let repo = dir.path().join("repo");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let hub = TestHub::start(state_dir.clone()).await;
    use reqwest::Method;

    let (status, ws) = hub
        .call(
            Method::POST,
            "/api/workspaces",
            Some(json!({ "name": "w", "folders": [repo] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let wid = ws["id"].as_str().unwrap().to_owned();
    let folder = ws["folders"][0]["path"].as_str().unwrap().to_owned();
    let path = format!("/api/workspaces/{wid}/layout");
    let board = |id: &str, folder: &str| json!({ "type": "board", "id": id, "folder": folder });

    let tabs = json!([
        { "id": "t1", "name": "shell", "root": pane("p1", None) },
        { "id": "t2", "name": "board", "root": board("b1", &folder) },
    ]);
    let (status, saved) = hub
        .call(Method::PUT, &path, Some(json!({ "rev": 0, "tabs": tabs })))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["tabs"][1]["root"], board("b1", &folder));

    let outside = elsewhere.canonicalize().unwrap();
    for (tabs, why) in [
        (
            json!([{ "id": "t1", "name": "a", "root": board("b1", outside.to_str().unwrap()) }]),
            "not a folder of this workspace",
        ),
        (
            json!([{ "id": "t1", "name": "a", "root": split("right", 0.5, pane("p1", None), board("b1", &folder)) }]),
            "tab's root",
        ),
    ] {
        let (status, body) = hub
            .call(Method::PUT, &path, Some(json!({ "rev": 1, "tabs": tabs })))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains(why), "{body}");
    }
    let (_, read) = hub.call(Method::GET, &path, None).await;
    assert_eq!(read, saved, "a refused write leaves the layout unchanged");

    hub.shutdown().await;
    let restarted = TestHub::start(state_dir).await;
    let (_, after) = restarted.call(Method::GET, &path, None).await;
    assert_eq!(after, saved);
    restarted.shutdown().await;
}
