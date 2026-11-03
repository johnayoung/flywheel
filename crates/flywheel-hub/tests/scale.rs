//! Pushes stay bounded under bursts: a burst of list changes (each would
//! make every browser re-list every workspace and session) reaches an
//! events client as a few paced `sessions` signals -- the last after the
//! burst, so nothing is missed -- each a constant-size message carrying no
//! list.

mod support;

use flywheel_hub::server::SESSIONS_PUSH_INTERVAL;
use flywheel_hub::{HubOptions, ServerOptions, serve};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_list_changes_reaches_events_clients_as_few_paced_signals() {
    flywheel_hub::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut options = HubOptions::new(dir.path().join("state"));
    options.default_shell = vec!["/bin/sh".into()];
    let hub = support::open_hub(options);
    let token = hub.token().expose().to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
        let _ = stopped.await;
    }));
    let http = reqwest::Client::new();
    let call = async |method: reqwest::Method, path: &str, body: Value| -> Value {
        let response = http
            .request(method, format!("http://{addr}{path}"))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.json().await.unwrap()
    };
    let workspace = call(
        reqwest::Method::POST,
        "/api/workspaces",
        json!({ "name": "w", "folders": [repo] }),
    )
    .await;
    let session = call(
        reqwest::Method::POST,
        "/api/sessions",
        json!({ "workspace_id": workspace["id"], "name": "s0" }),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_owned();

    let mut request = format!("ws://{addr}/api/events?client=scale-test")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    // The snapshot first; then let the creations' own pushes pass.
    let first = timeout(Duration::from_secs(10), ws.next()).await.unwrap();
    assert!(matches!(first, Some(Ok(Message::Text(ref t))) if t.contains("snapshot")));
    let settle = Instant::now() + 2 * SESSIONS_PUSH_INTERVAL;
    while let Ok(Some(_)) = tokio::time::timeout_at(settle.into(), ws.next()).await {}

    // 40 renames, each a list change.
    const RENAMES: usize = 40;
    let started = Instant::now();
    for n in 1..=RENAMES {
        call(
            reqwest::Method::PATCH,
            &format!("/api/sessions/{id}"),
            json!({ "name": format!("s{n}") }),
        )
        .await;
    }
    let burst = started.elapsed();
    let ended = Instant::now();

    let mut signals: Vec<(Instant, String)> = Vec::new();
    let collect_until = Instant::now() + 3 * SESSIONS_PUSH_INTERVAL;
    while let Ok(Some(message)) = tokio::time::timeout_at(collect_until.into(), ws.next()).await {
        if let Ok(Message::Text(text)) = message {
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value["type"] == "sessions" {
                signals.push((Instant::now(), text.as_str().to_owned()));
            }
        }
    }
    // Paced: at most one per interval over the burst, plus the trailing
    // one (one per rename without pacing).
    let bound = 2 + (burst.as_millis() / SESSIONS_PUSH_INTERVAL.as_millis()) as usize;
    assert!(
        !signals.is_empty() && signals.len() <= bound && signals.len() < RENAMES,
        "{} sessions pushes for {RENAMES} renames in {burst:?} (bound {bound})",
        signals.len()
    );
    // Nothing is lost: a signal came after the last change.
    assert!(
        signals.iter().any(|(at, _)| *at >= ended),
        "no sessions push after the burst"
    );
    // Each is the bare signal (the list is fetched, never pushed).
    for (_, text) in &signals {
        assert_eq!(text, r#"{"type":"sessions"}"#);
    }
    // And the list it points at is current.
    let listed = call(
        reqwest::Method::GET,
        &format!("/api/sessions/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(listed["name"], format!("s{RENAMES}"));

    let _ = stop.send(());
    timeout(Duration::from_secs(15), server)
        .await
        .expect("server shut down in time")
        .unwrap()
        .unwrap();
}
