//! Session recording end to end: a real hub on 127.0.0.1:0, real PTYs.
//! Recordings parse back as asciinema v2 with monotonic times, resize
//! events, and the output in order; they rotate at their per-file cap,
//! the directory keeps to its cap, and a recording ends with its process.

mod support;

use flywheel_hub::record::{RecordLimits, parse_cast};
use flywheel_hub::{HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct TestHub {
    dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start(limits: RecordLimits) -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo")).unwrap();
        let mut options = HubOptions::new(dir.path().join("state"));
        options.default_shell = vec!["/bin/sh".into()];
        options.record_limits = limits;
        let hub = support::open_hub(options);
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, hub, ServerOptions::default(), async {
            let _ = stopped.await;
        }));
        Self {
            dir,
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

    fn recordings_dir(&self) -> PathBuf {
        self.dir.path().join("state/recordings")
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
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    async fn get(&self, path: &str) -> Value {
        self.call(reqwest::Method::GET, path, None).await.1
    }

    async fn shell(&self) -> String {
        let (_, workspace) = self
            .post(
                "/api/workspaces",
                json!({ "name": "rec", "folders": [self.dir.path().join("repo")] }),
            )
            .await;
        let (status, session) = self
            .post(
                "/api/sessions",
                json!({ "workspace_id": workspace["id"], "name": "taped", "cols": 80, "rows": 24 }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        session["id"].as_str().unwrap().to_owned()
    }

    async fn type_line(&self, sid: &str, line: &str) {
        let (status, answer) = self
            .post(
                &format!("/api/sessions/{sid}/input"),
                json!({ "data": line, "enter": true }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
    }

    async fn screen_shows(&self, sid: &str, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let screen = self
                .get(&format!(
                    "/api/sessions/{sid}/screen?source=recent&lines=200"
                ))
                .await;
            if screen["lines"]
                .as_array()
                .is_some_and(|l| l.iter().any(|l| l.as_str().unwrap_or("").contains(needle)))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{needle:?} never shown: {screen}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn download(&self, name: &str) -> String {
        let response = self
            .http
            .get(self.url(&format!("/api/recordings/{name}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "application/x-asciicast"
        );
        assert_eq!(
            response.headers()["content-disposition"],
            format!("attachment; filename=\"{name}\"").as_str()
        );
        response.text().await.unwrap()
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

fn monotonic(events: &[(f64, String, String)]) -> bool {
    events.windows(2).all(|w| w[0].0 <= w[1].0) && events.iter().all(|e| e.0 >= 0.0)
}

/// The time of the first output event containing `needle`.
fn time_of(events: &[(f64, String, String)], needle: &str) -> f64 {
    events
        .iter()
        .find(|e| e.1 == "o" && e.2.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not recorded"))
        .0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recording_is_valid_asciinema_with_timing_and_resizes() {
    let hub = TestHub::start(RecordLimits::default()).await;
    let sid = hub.shell().await;
    hub.type_line(&sid, "echo before-$((1+1))").await;
    hub.screen_shows(&sid, "before-2").await;

    let (status, started) = hub
        .post(&format!("/api/sessions/{sid}/record/start"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let name = started["recording"]["name"].as_str().unwrap().to_owned();
    assert!(name.starts_with(&sid) && name.ends_with(".cast"), "{name}");
    assert_eq!(started["recording"]["active"], true);
    // Starting again is the same recording.
    let (status, again) = hub
        .post(&format!("/api/sessions/{sid}/record/start"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["recording"]["name"], name.as_str());
    assert_eq!(again["started"], false);
    // The session says so; a recorder is not a client.
    let session = hub.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(session["recording"], name.as_str());
    assert_eq!(session["live"]["clients"], 0, "{session}");
    // The file is private.
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(hub.recordings_dir().join(&name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    hub.type_line(&sid, "echo first-$((2+3))").await;
    hub.screen_shows(&sid, "first-5").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    // A viewer resizes: recorded in order with the output.
    let mut request = format!("ws://{}/api/sessions/{sid}/ws", hub.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", hub.token).parse().unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.send(Message::Text(
        json!({ "type": "resize", "cols": 100, "rows": 30 })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(Ok(Message::Text(text))) =
            timeout(Duration::from_secs(10), ws.next()).await.unwrap()
            && serde_json::from_str::<Value>(&text).unwrap()["type"] == "size"
        {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    hub.type_line(&sid, "echo second-$((3+4))").await;
    hub.screen_shows(&sid, "second-7").await;
    // Mid-recording, the download is what was written so far.
    let partial = parse_cast(&hub.download(&name).await).unwrap();
    assert!(partial.output().contains("first-5"));
    // A recording being written cannot be deleted.
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/recordings/{name}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, stopped) = hub
        .post(&format!("/api/sessions/{sid}/record/stop"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    assert_eq!(stopped["recording"]["active"], false);
    let (status, _) = hub
        .post(&format!("/api/sessions/{sid}/record/stop"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        hub.get(&format!("/api/sessions/{sid}"))
            .await
            .get("recording")
            .is_none()
    );
    hub.type_line(&sid, "echo after-$((4+5))").await;
    hub.screen_shows(&sid, "after-9").await;

    let cast = parse_cast(&hub.download(&name).await).unwrap();
    assert_eq!(cast.header["version"], 2);
    assert_eq!(cast.header["width"], 80);
    assert_eq!(cast.header["height"], 24);
    assert_eq!(cast.header["title"], "rec/taped");
    assert_eq!(cast.header["x_fw_hub"]["session_id"], sid.as_str());
    assert!(cast.header["timestamp"].as_u64().unwrap() > 1_700_000_000);
    assert!(monotonic(&cast.events), "{:?}", cast.events);
    // It opens on the screen as it was (a snapshot: the earlier output).
    assert_eq!(cast.events[0].0, 0.0);
    assert!(cast.events[0].2.contains("before-2"));
    let output = cast.output();
    let (first, second) = (
        output.find("first-5").unwrap(),
        output.find("second-7").unwrap(),
    );
    assert!(first < second);
    assert!(!output.contains("after-9"), "recorded past its stop");
    let gap = time_of(&cast.events, "second-7") - time_of(&cast.events, "first-5");
    assert!(gap >= 0.35, "the pause between them was kept: {gap}");
    let resize = cast
        .events
        .iter()
        .position(|e| e.1 == "r")
        .expect("a resize event");
    assert_eq!(cast.events[resize].2, "100x30");
    assert!(cast.events[resize].0 >= time_of(&cast.events, "first-5"));
    assert!(cast.events[resize].0 <= time_of(&cast.events, "second-7"));

    // Listed; deleted once finished; bad names refused.
    let listed = hub.get("/api/recordings").await;
    assert_eq!(listed["recordings"][0]["name"], name.as_str());
    assert_eq!(listed["recordings"][0]["title"], "rec/taped");
    assert!(listed["limits"]["max_file_bytes"].as_u64().unwrap() > 0);
    for bad in ["..%2Fstate.json", "nope.cast", "x.txt"] {
        let (status, _) = hub
            .call(
                reqwest::Method::GET,
                &format!("/api/recordings/{bad}"),
                None,
            )
            .await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "{bad}: {status}"
        );
    }
    let (status, _) = hub
        .call(
            reqwest::Method::DELETE,
            &format!("/api/recordings/{name}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        hub.get("/api/recordings").await["recordings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recordings_rotate_at_their_cap_keep_the_directory_bounded_and_end_with_the_process() {
    let limits = RecordLimits {
        max_file_bytes: 8 * 1024,
        max_total_bytes: 48 * 1024,
    };
    let hub = TestHub::start(limits).await;
    let sid = hub.shell().await;
    hub.screen_shows(&sid, "$").await;
    let (status, started) = hub
        .post(&format!("/api/sessions/{sid}/record/start"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let first = started["recording"]["name"].as_str().unwrap().to_owned();
    // Plenty of output, in bursts so the recorder keeps up.
    for round in 0..6 {
        hub.type_line(
            &sid,
            &format!("seq 1 6000 | sed 's/^/r{round}-/'; echo done-{round}-$((0+1))"),
        )
        .await;
        hub.screen_shows(&sid, &format!("done-{round}-1")).await;
    }
    let listed = hub.get("/api/recordings").await;
    let recordings = listed["recordings"].as_array().unwrap().clone();
    assert!(recordings.len() >= 2, "{listed}");
    let parts: Vec<u64> = recordings
        .iter()
        .map(|r| r["part"].as_u64().unwrap())
        .collect();
    assert!(parts.iter().any(|p| *p >= 3), "rotated: {parts:?}");
    // The oldest parts made room: the first is gone, and the directory
    // holds at most its cap plus the part being written.
    assert!(
        !recordings.iter().any(|r| r["name"] == first.as_str()),
        "{listed}"
    );
    let total: u64 = std::fs::read_dir(hub.recordings_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".cast"))
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert!(
        total <= limits.max_total_bytes + limits.max_file_bytes + 128 * 1024,
        "{total}"
    );
    // Every part is a recording of its own, opening on a fresh screen.
    let base = first.trim_end_matches(".cast");
    for rec in &recordings {
        let name = rec["name"].as_str().unwrap();
        assert!(name.starts_with(base), "{name}");
        let cast = parse_cast(&hub.download(name).await).unwrap();
        assert!(monotonic(&cast.events), "{name}");
        assert_eq!(cast.header["x_fw_hub"]["part"], rec["part"]);
        assert!(
            cast.events[0].2.starts_with("\u{1b}c"),
            "{name} opens on a snapshot"
        );
    }
    // The process exits: the recording ends by itself, with a marker.
    hub.type_line(&sid, "exit 3").await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while hub
        .get(&format!("/api/sessions/{sid}"))
        .await
        .get("recording")
        .is_some()
    {
        assert!(
            Instant::now() < deadline,
            "the recording outlived its process"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let listed = hub.get("/api/recordings").await;
    let newest = &listed["recordings"][0];
    assert_eq!(newest["active"], false, "{listed}");
    let cast = parse_cast(&hub.download(newest["name"].as_str().unwrap()).await).unwrap();
    let last = cast.events.last().unwrap();
    assert_eq!((last.1.as_str(), last.2.as_str()), ("m", "exit: 3"));
    // Nothing to record on an ended session.
    let (status, _) = hub
        .post(&format!("/api/sessions/{sid}/record/start"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_hub_closes_recordings_and_a_restart_does_not_resume_them() {
    let hub = TestHub::start(RecordLimits::default()).await;
    let sid = hub.shell().await;
    let (_, started) = hub
        .post(&format!("/api/sessions/{sid}/record/start"), json!({}))
        .await;
    let name = started["recording"]["name"].as_str().unwrap().to_owned();
    hub.type_line(&sid, "echo kept-$((5+5))").await;
    hub.screen_shows(&sid, "kept-10").await;
    let active = hub.recordings_dir().join("active.json");
    assert!(active.exists());
    let dir = hub.shutdown().await;
    // Written out and closed: no recording left marked active.
    assert!(!dir.path().join("state/recordings/active.json").exists());
    let text = std::fs::read_to_string(dir.path().join("state/recordings").join(&name)).unwrap();
    let cast = parse_cast(&text).unwrap();
    assert!(cast.output().contains("kept-10"));
    assert_eq!(
        cast.events.last().unwrap().1,
        "m",
        "ends on the hangup's exit"
    );
}
