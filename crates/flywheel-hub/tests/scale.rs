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

    // Read the socket while the burst runs: each `sessions` signal is
    // answered the way a browser answers it, by fetching the session, so
    // what a client last saw is known. (Reading only after the burst would
    // pass with no trailing push at all: the first rename's immediate one
    // would still be queued.)
    let (burst_done, mut burst_done_rx) = oneshot::channel::<()>();
    let reader = {
        let http = http.clone();
        let token = token.clone();
        let id = id.clone();
        tokio::spawn(async move {
            let mut seen: Vec<(Instant, String, String)> = Vec::new();
            let mut deadline: Option<Instant> = None;
            loop {
                let next = match deadline {
                    Some(at) => match tokio::time::timeout_at(at.into(), ws.next()).await {
                        Ok(next) => next,
                        Err(_) => break,
                    },
                    None => tokio::select! {
                        next = ws.next() => next,
                        _ = &mut burst_done_rx => {
                            deadline = Some(Instant::now() + 3 * SESSIONS_PUSH_INTERVAL);
                            continue;
                        }
                    },
                };
                let Some(Ok(Message::Text(text))) = next else {
                    if next.is_none() {
                        break;
                    }
                    continue;
                };
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                if value["type"] != "sessions" {
                    continue;
                }
                let at = Instant::now();
                let got: Value = http
                    .get(format!("http://{addr}/api/sessions/{id}"))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                seen.push((
                    at,
                    text.as_str().to_owned(),
                    got["name"].as_str().unwrap_or_default().to_owned(),
                ));
            }
            seen
        })
    };

    // 40 renames, each a list change; spaced so the burst spans several
    // pacing intervals and its last rename lands inside one (where only
    // the trailing push can carry it).
    const RENAMES: usize = 40;
    let started = Instant::now();
    for n in 1..=RENAMES {
        call(
            reqwest::Method::PATCH,
            &format!("/api/sessions/{id}"),
            json!({ "name": format!("s{n}") }),
        )
        .await;
        tokio::time::sleep(SESSIONS_PUSH_INTERVAL / 16).await;
    }
    let burst = started.elapsed();
    let _ = burst_done.send(());
    let signals = timeout(Duration::from_secs(30), reader)
        .await
        .expect("reader finished")
        .unwrap();

    // Paced: at most one per interval over the burst, plus the trailing
    // one (one per rename without pacing).
    let bound = 2 + (burst.as_millis() / SESSIONS_PUSH_INTERVAL.as_millis()) as usize;
    assert!(
        signals.len() >= 2 && signals.len() <= bound && signals.len() < RENAMES,
        "{} sessions pushes for {RENAMES} renames in {burst:?} (bound {bound})",
        signals.len()
    );
    // Each is the bare signal (the list is fetched, never pushed).
    for (_, text, _) in &signals {
        assert_eq!(text, r#"{"type":"sessions"}"#);
    }
    // Nothing is lost: what a client fetched after the last signal is the
    // final state (without the trailing push it is a rename mid-burst).
    let (_, _, last_name) = signals.last().unwrap();
    assert_eq!(
        last_name,
        &format!("s{RENAMES}"),
        "the last sessions push left a client with a stale list: {:?}",
        signals.iter().map(|(_, _, n)| n).collect::<Vec<_>>()
    );
    // Paced in time too: consecutive pushes are an interval apart.
    for pair in signals.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!(
            gap >= SESSIONS_PUSH_INTERVAL.mul_f64(0.5),
            "two sessions pushes {gap:?} apart"
        );
    }

    let _ = stop.send(());
    timeout(Duration::from_secs(15), server)
        .await
        .expect("server shut down in time")
        .unwrap()
        .unwrap();
}
