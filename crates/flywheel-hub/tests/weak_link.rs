//! End-to-end, the terminal socket on a weak link (protocol 2): lite
//! pacing (a bounded update rate whose last screen is the emulator's),
//! resuming from an offset (what was missed equals a full snapshot),
//! deflated messages, input acks in order, and a raw viewer that fell
//! behind. A real hub on 127.0.0.1:0, real PTYs running `/bin/sh`.

mod support;

use flate2::{Decompress, FlushDecompress};
use flywheel_hub::vt::{Emulator, TextSource};
use flywheel_hub::{HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::net::SocketAddr;
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
    addr: SocketAddr,
    token: String,
    http: reqwest::Client,
    workspace: String,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    async fn start() -> Self {
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
        let workspace: Value = http
            .post(format!("http://{addr}/api/workspaces"))
            .bearer_auth(&token)
            .json(&json!({ "name": "weak", "folders": [repo] }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Self {
            _dir: dir,
            addr,
            token,
            http,
            workspace: workspace["id"].as_str().unwrap().to_owned(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    async fn session(&self, script: &str) -> String {
        let response = self
            .http
            .post(format!("http://{}/api/sessions", self.addr))
            .bearer_auth(&self.token)
            .json(&json!({
                "workspace_id": self.workspace,
                "command": ["/bin/sh", "-c", script],
                "cols": 60,
                "rows": 12,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let session: Value = response.json().await.unwrap();
        session["id"].as_str().unwrap().to_owned()
    }

    async fn screen(&self, id: &str) -> Vec<String> {
        self.screen_of(id, "").await
    }

    /// `/screen` with `query` (e.g. `?source=recent&lines=500`).
    async fn screen_of(&self, id: &str, query: &str) -> Vec<String> {
        let screen: Value = self
            .http
            .get(format!(
                "http://{}/api/sessions/{id}/screen{query}",
                self.addr
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        screen["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_owned())
            .collect()
    }

    async fn input(&self, id: &str, data: &str) {
        let response = self
            .http
            .post(format!("http://{}/api/sessions/{id}/input", self.addr))
            .bearer_auth(&self.token)
            .json(&json!({ "data": data }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn connect(&self, id: &str, query: &str) -> Ws {
        let url = format!("ws://{}/api/sessions/{id}/ws?{query}", self.addr);
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        tokio_tungstenite::connect_async(request).await.unwrap().0
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

/// A protocol-2 viewer: an emulator fed what the socket sends, the
/// offset it is at, and what it saw.
struct Viewer {
    ws: Ws,
    term: Emulator,
    seq: u64,
    epoch: String,
    inflate: Option<Decompress>,
    /// Bytes still owed to a snapshot or frame.
    owed: usize,
    /// Updates: frames, and raw messages outside one.
    updates: usize,
    update_times: Vec<Instant>,
    frames: usize,
    resumed: bool,
    continued: Option<bool>,
    acks: u64,
    /// Binary bytes on the wire.
    wire: usize,
}

impl Viewer {
    fn new(ws: Ws, deflated: bool) -> Self {
        Self {
            ws,
            term: Emulator::new(60, 12, 1000),
            seq: 0,
            epoch: String::new(),
            inflate: deflated.then(|| Decompress::new(false)),
            owed: 0,
            updates: 0,
            update_times: Vec::new(),
            frames: 0,
            resumed: false,
            continued: None,
            acks: 0,
            wire: 0,
        }
    }

    fn resize(&mut self, value: &Value) {
        let (cols, rows) = (
            value["cols"].as_u64().unwrap(),
            value["rows"].as_u64().unwrap(),
        );
        if self.term.size() != (cols as usize, rows as usize) {
            self.term.resize(cols as u16, rows as u16);
        }
    }

    fn payload(&mut self, data: &[u8]) -> Vec<u8> {
        self.wire += data.len();
        let Some(z) = &mut self.inflate else {
            return data.to_vec();
        };
        match data[0] {
            0 => data[1..].to_vec(),
            1 => {
                let len = u32::from_be_bytes(data[1..5].try_into().unwrap()) as usize;
                let mut out = Vec::with_capacity(len + 64);
                z.decompress_vec(&data[5..], &mut out, FlushDecompress::Sync)
                    .unwrap();
                assert_eq!(out.len(), len, "a message inflates whole");
                out
            }
            tag => panic!("bad tag {tag}"),
        }
    }

    /// Take one message; false once the socket ended.
    async fn step(&mut self, wait: Duration) -> Option<bool> {
        let message = match timeout(wait, self.ws.next()).await {
            Err(_) => return None,
            Ok(m) => m,
        };
        match message {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                match value["type"].as_str().unwrap() {
                    "snapshot" => {
                        self.resize(&value);
                        self.seq = value["seq"].as_u64().unwrap();
                        self.epoch = value["epoch"].as_str().unwrap().to_owned();
                        self.owed = value["bytes"].as_u64().unwrap() as usize;
                        self.continued = value["continued"].as_bool();
                    }
                    "resume" => {
                        assert_eq!(
                            value["seq"].as_u64().unwrap(),
                            self.seq,
                            "resumes where it was"
                        );
                        self.epoch = value["epoch"].as_str().unwrap().to_owned();
                        self.resumed = true;
                    }
                    "frame" => {
                        self.resize(&value);
                        self.seq = value["seq"].as_u64().unwrap();
                        self.owed = value["bytes"].as_u64().unwrap() as usize;
                        self.frames += 1;
                        self.updates += 1;
                        self.update_times.push(Instant::now());
                    }
                    "size" => self.resize(&value),
                    "ack" => {
                        let n = value["id"].as_u64().unwrap();
                        assert_eq!(n, self.acks + 1, "every input acknowledged, in order");
                        self.acks = n;
                    }
                    "exit" | "error" => {}
                    other => panic!("unexpected {other}"),
                }
                Some(true)
            }
            Some(Ok(Message::Binary(data))) => {
                let bytes = self.payload(&data);
                if self.owed > 0 {
                    assert!(bytes.len() <= self.owed, "no more than announced");
                    self.owed -= bytes.len();
                } else {
                    self.seq += bytes.len() as u64;
                    self.updates += 1;
                    self.update_times.push(Instant::now());
                }
                self.term.process(&bytes);
                let _ = self.term.take_replies();
                Some(true)
            }
            Some(Ok(_)) => Some(true),
            _ => Some(false),
        }
    }

    fn visible(&mut self) -> Vec<String> {
        self.term.text(TextSource::Visible).lines
    }

    /// Read until the screen shows `needle` (and nothing is owed).
    async fn until(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !(self.owed == 0 && self.visible().iter().any(|l| l.contains(needle))) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "never showed {needle:?}: {:?}",
                self.visible()
            );
            assert_eq!(self.step(left).await, Some(true), "socket ended");
        }
    }

    /// Read until nothing comes for `quiet`.
    async fn settle(&mut self, quiet: Duration) {
        while let Some(alive) = self.step(quiet).await {
            assert!(alive, "socket ended");
        }
    }

    async fn send(&mut self, value: Value) {
        self.ws
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    async fn close(mut self) -> (u64, String) {
        let _ = self.ws.close(None).await;
        (self.seq, self.epoch)
    }
}

/// Lines a `sh` loop prints `n` of, numbered (`tag-<i>`).
fn burst(tag: &str, n: usize) -> String {
    format!("i=0; while [ $i -lt {n} ]; do echo \"{tag}-$i\"; i=$((i+1)); done")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lite_viewer_gets_a_bounded_update_rate_ending_on_the_emulators_screen() {
    let hub = TestHub::start().await;
    // A status line redrawn as fast as sh can for two seconds, a log line
    // now and then, then the end.
    let id = hub
        .session(
            "read go; end=$(($(date +%s) + 2)); i=0; while [ $(date +%s) -lt $end ]; do \
             i=$((i+1)); printf '\\r\\033[2K\\033[33m*\\033[0m working %d' $i; \
             [ $((i % 200)) -eq 0 ] && printf '\\nlog %d\\n' $i; done; printf '\\nall done\\n'; sleep 30",
        )
        .await;
    let fps = 5;
    let mut viewer = Viewer::new(
        hub.connect(&id, &format!("proto=2&lite=1&fps={fps}&z=1"))
            .await,
        true,
    );
    viewer.settle(Duration::from_millis(300)).await;
    let before = viewer.updates;
    hub.input(&id, "go\r").await;
    let started = Instant::now();
    viewer.until("all done").await;
    viewer.settle(Duration::from_millis(500)).await;
    let elapsed = started.elapsed().as_secs_f64();
    let updates = viewer.updates - before;
    // At most `fps` a second (the first comes at once: one more).
    assert!(
        updates as f64 <= elapsed * f64::from(fps) + 2.0,
        "{updates} updates in {elapsed:.1}s"
    );
    assert!(viewer.frames > 0, "a busy screen goes by frames");
    // Updates are at least an interval apart (a little slack for timers).
    for pair in viewer.update_times.windows(2).skip(before) {
        let gap = pair[1] - pair[0];
        assert!(gap >= Duration::from_millis(150), "updates {gap:?} apart");
    }
    assert_eq!(
        viewer.visible(),
        hub.screen(&id).await,
        "the last screen is the emulator's"
    );
    // Its history has every log line too.
    let recent = viewer.term.text(TextSource::Recent { lines: 1000 }).lines;
    assert!(
        recent.iter().filter(|l| l.starts_with("log ")).count() >= 1,
        "{recent:?}"
    );
    viewer.close().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resuming_sends_only_what_was_missed_and_matches_a_full_snapshot() {
    let hub = TestHub::start().await;
    let id = hub
        .session(&format!("{}; exec cat", burst("before", 300)))
        .await;
    let mut first = Viewer::new(hub.connect(&id, "proto=2").await, false);
    first.until("before-299").await;
    first.settle(Duration::from_millis(200)).await;
    let snapshot_wire = first.wire;
    // The link drops; the session keeps writing.
    let mut term = std::mem::replace(&mut first.term, Emulator::new(1, 1, 0));
    let (seq, epoch) = first.close().await;
    hub.input(&id, "missed line one\r").await;
    hub.input(&id, "missed line two\r").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut back = Viewer::new(
        hub.connect(&id, &format!("proto=2&since={seq}&epoch={epoch}"))
            .await,
        false,
    );
    std::mem::swap(&mut back.term, &mut term);
    back.seq = seq;
    back.until("missed line two").await;
    back.settle(Duration::from_millis(200)).await;
    assert!(back.resumed, "a resume, not a snapshot");
    assert!(
        back.wire * 20 < snapshot_wire,
        "resume {} B vs snapshot {snapshot_wire} B",
        back.wire
    );

    let mut fresh = Viewer::new(hub.connect(&id, "proto=2").await, false);
    fresh.until("missed line two").await;
    assert_eq!(
        back.visible(),
        fresh.visible(),
        "resumed screen = a full snapshot's"
    );
    let history = |v: &mut Viewer| v.term.text(TextSource::Recent { lines: 5000 }).lines;
    assert_eq!(
        history(&mut back),
        history(&mut fresh),
        "and its scrollback"
    );
    let end = back.seq;
    assert_eq!(end, fresh.seq, "both at the end of the output");

    // Another stream's offset (a relaunched session): a snapshot saying
    // the session is not the one typed against.
    let mut other = Viewer::new(
        hub.connect(&id, &format!("proto=2&since={end}&epoch=1"))
            .await,
        false,
    );
    other.until("missed line two").await;
    assert!(!other.resumed);
    assert_eq!(other.continued, Some(false));
    // Past the output: the same.
    let mut past = Viewer::new(
        hub.connect(&id, &format!("proto=2&since={}&epoch={epoch}", end + 10))
            .await,
        false,
    );
    past.until("missed line two").await;
    assert_eq!(past.continued, Some(false));

    // More output than a lite viewer resumes raw: one frame instead (its
    // scrollback stays; the rows it missed are not replayed).
    let mut term = std::mem::replace(&mut back.term, Emulator::new(1, 1, 0));
    let (seq, epoch) = back.close().await;
    for i in 0..40 {
        hub.input(&id, &format!("{i:03} {}\r", "y".repeat(400)))
            .await;
    }
    hub.input(&id, "tail marker\r").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut lite = Viewer::new(
        hub.connect(&id, &format!("proto=2&lite=1&since={seq}&epoch={epoch}"))
            .await,
        false,
    );
    std::mem::swap(&mut lite.term, &mut term);
    lite.seq = seq;
    lite.until("tail marker").await;
    lite.settle(Duration::from_millis(400)).await;
    assert!(lite.resumed);
    assert_eq!(lite.frames, 1, "one frame for what it missed");
    assert!(lite.wire < 8 * 1024, "{} B", lite.wire);
    assert_eq!(lite.visible(), hub.screen(&id).await);
    other.close().await;
    past.close().await;
    fresh.close().await;
    lite.close().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_input_is_acknowledged_in_order_and_sent_again_never_typed_twice() {
    let hub = TestHub::start().await;
    let id = hub.session("stty -echo; exec cat").await;
    // A queue flushed at once after a reconnect: 200 small messages.
    let mut viewer = Viewer::new(hub.connect(&id, "proto=2&z=1&client=pane-1").await, true);
    viewer.settle(Duration::from_millis(300)).await;
    let mut expected = String::new();
    let key = |n: u64| {
        if n == 200 {
            "\r".to_owned()
        } else {
            format!("k{n} ")
        }
    };
    for n in 1..=200u64 {
        if n < 200 {
            expected.push_str(&key(n));
        }
        viewer
            .send(json!({ "type": "input", "data": key(n), "id": n }))
            .await;
    }
    viewer.until("k199").await;
    viewer.settle(Duration::from_millis(300)).await;
    assert_eq!(viewer.acks, 200);
    let (seq, epoch) = viewer.close().await;
    // The link dropped before the acks of 151.. came: they go again, then
    // what was typed meanwhile. Only that is new.
    let mut back = Viewer::new(
        hub.connect(
            &id,
            &format!("proto=2&client=pane-1&since={seq}&epoch={epoch}"),
        )
        .await,
        false,
    );
    back.seq = seq;
    back.acks = 150;
    back.settle(Duration::from_millis(300)).await;
    for n in 151..=201u64 {
        let data = if n == 201 { "end\r".to_owned() } else { key(n) };
        back.send(json!({ "type": "input", "data": data, "id": n }))
            .await;
    }
    back.until("end").await;
    back.settle(Duration::from_millis(300)).await;
    assert_eq!(back.acks, 201, "acknowledged, sent again or not");
    let all = hub
        .screen_of(&id, "?source=recent&lines=500")
        .await
        .join("");
    assert_eq!(all.matches("k151 ").count(), 1, "not typed twice: {all:?}");
    assert!(all.contains(expected.trim_end()), "in order: {all:?}");
    // Another pane's ids are its own.
    let mut other = Viewer::new(hub.connect(&id, "proto=2&client=pane-2").await, false);
    other.settle(Duration::from_millis(300)).await;
    other
        .send(json!({ "type": "input", "data": "other\r", "id": 1 }))
        .await;
    other.until("other").await;
    back.close().await;
    other.close().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_raw_viewer_that_fell_behind_catches_up_by_one_update_then_streams() {
    let hub = TestHub::start().await;
    let id = hub
        .session(&format!(
            "read go; {}; echo caught-up-end; exec cat",
            burst("flood", 200_000)
        ))
        .await;
    let mut viewer = Viewer::new(hub.connect(&id, "proto=2").await, false);
    viewer.settle(Duration::from_millis(300)).await;
    hub.input(&id, "go\r").await;
    // Not reading while the flood fills the socket and the queue behind it.
    tokio::time::sleep(Duration::from_secs(4)).await;
    viewer.until("caught-up-end").await;
    viewer.settle(Duration::from_millis(500)).await;
    assert!(viewer.frames >= 1, "it caught up by a frame");
    assert_eq!(viewer.visible(), hub.screen(&id).await);
    // Then the raw stream: typed text echoes back as bytes.
    let frames = viewer.frames;
    hub.input(&id, "after\r").await;
    viewer.until("after").await;
    assert_eq!(viewer.frames, frames, "raw again");
    viewer.close().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_pace_mid_socket_stays_exact_and_a_paused_viewer_gets_nothing() {
    let hub = TestHub::start().await;
    let id = hub.session("exec cat").await;
    let mut viewer = Viewer::new(hub.connect(&id, "proto=2").await, false);
    viewer.settle(Duration::from_millis(300)).await;
    viewer
        .send(json!({ "type": "mode", "lite": true, "fps": 10 }))
        .await;
    hub.input(&id, "lite one\r").await;
    viewer.until("lite one").await;
    // Paused (a hidden pane): nothing comes, however much is written.
    viewer
        .send(json!({ "type": "mode", "lite": true, "fps": 10, "paused": true }))
        .await;
    viewer.settle(Duration::from_millis(200)).await;
    let wire = viewer.wire;
    for i in 0..20 {
        hub.input(&id, &format!("hidden {i}\r")).await;
    }
    viewer.settle(Duration::from_millis(600)).await;
    assert_eq!(viewer.wire, wire, "paused: nothing sent");
    viewer
        .send(json!({ "type": "mode", "lite": true, "fps": 10, "paused": false }))
        .await;
    viewer.until("hidden 19").await;
    // Back to raw: the stream continues exactly.
    viewer.send(json!({ "type": "mode", "lite": false })).await;
    hub.input(&id, "raw again\r").await;
    viewer.until("raw again").await;
    viewer.settle(Duration::from_millis(300)).await;
    assert_eq!(viewer.visible(), hub.screen(&id).await);
    viewer.close().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_loads_gzipped_and_a_reload_needs_only_its_page() {
    let hub = TestHub::start().await;
    let get = |path: &str, gzip: bool| {
        let mut request = hub
            .http
            .get(format!("http://{}{path}", hub.addr))
            .bearer_auth(&hub.token);
        if gzip {
            request = request.header("accept-encoding", "gzip, deflate, br");
        }
        request.send()
    };
    let page = get("/", true).await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["content-encoding"], "gzip");
    assert_eq!(page.headers()["cache-control"], "no-store");
    let body = page.bytes().await.unwrap();
    assert_eq!(&body[..2], &[0x1f, 0x8b], "gzip");
    let html = {
        use std::io::Read as _;
        let mut out = String::new();
        flate2::read::GzDecoder::new(&body[..])
            .read_to_string(&mut out)
            .unwrap();
        out
    };
    // Scripts are named by build: cached for good, gzipped.
    let at = html.find("/assets/app.js?v=").expect("app.js by build");
    let url = &html[at..at + html[at..].find('"').unwrap()];
    let script = get(url, true).await.unwrap();
    assert_eq!(script.status(), StatusCode::OK);
    assert_eq!(script.headers()["content-encoding"], "gzip");
    assert!(
        script.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let etag = script.headers()["etag"].to_str().unwrap().to_owned();
    // Without the build (or gzip): revalidated, and unchanged is a 304.
    let plain = get("/assets/app.js", false).await.unwrap();
    assert_eq!(plain.headers()["cache-control"], "no-cache");
    assert!(plain.headers().get("content-encoding").is_none());
    let tag = plain.headers()["etag"].to_str().unwrap().to_owned();
    assert_ne!(tag, etag, "a gzip body is its own representation");
    let again = hub
        .http
        .get(format!("http://{}/assets/app.js", hub.addr))
        .bearer_auth(&hub.token)
        .header("if-none-match", &tag)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
    assert!(again.bytes().await.unwrap().is_empty());
    // Fonts are compressed already.
    let font = get("/assets/vendor/fonts/jetbrains-mono.woff2", true)
        .await
        .unwrap();
    assert!(font.headers().get("content-encoding").is_none());
    hub.shutdown().await;
}
