//! A reader that stops reading is dropped after the send timeout, and
//! what it held is freed, while healthy clients carry on: the events
//! socket, and the `/m/<machine>/` proxy's relay in both directions (a
//! browser that stops reading, a machine that stops reading). A real hub
//! on 127.0.0.1:0 with a 1 s send timeout; the machine is a real
//! WebSocket server on 127.0.0.1 that floods, or never reads.

mod support;

use flywheel_hub::{Hub, HubOptions, ServerOptions, serve};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// The hub's send timeout here.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// A receive buffer small enough that a reader that stops reading stalls
/// the writer after a few frames.
const SMALL_BUFFER: u32 = 4096;

type Ws = WebSocketStream<TcpStream>;

struct TestHub {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    hub: Arc<Hub>,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl TestHub {
    /// A hub whose state dir holds `machines` (name, url) as registered.
    async fn start(machines: &[(&str, String)]) -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        if !machines.is_empty() {
            let records: Vec<Value> = machines
                .iter()
                .map(|(name, url)| json!({ "name": name, "url": url, "credential": "c", "added": 0 }))
                .collect();
            std::fs::write(
                state.join("machines.json"),
                json!({ "machines": records }).to_string(),
            )
            .unwrap();
        }
        let mut options = HubOptions::new(state);
        options.default_shell = vec!["/bin/sh".into()];
        let hub = support::open_hub(options);
        let token = hub.token().expose().to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            Arc::clone(&hub),
            ServerOptions {
                ws_send_timeout: Some(SEND_TIMEOUT),
                ..ServerOptions::default()
            },
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            _dir: dir,
            addr,
            token,
            hub,
            stop: Some(stop),
            server: Some(server),
        }
    }

    /// A WebSocket to `path`, its receive buffer `SMALL_BUFFER` when
    /// `small`.
    async fn ws(&self, path: &str, small: bool) -> Ws {
        let socket = TcpSocket::new_v4().unwrap();
        if small {
            socket.set_recv_buffer_size(SMALL_BUFFER).unwrap();
        }
        let stream = socket.connect(self.addr).await.unwrap();
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap()
            .0
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        reqwest::Client::new()
            .post(format!("http://{}{path}", self.addr))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
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

/// Write `message` until a write does not finish within a while (the
/// other side stopped reading, and every buffer between is full) or the
/// other side dropped the socket (it stopped reading and gave up on us
/// meanwhile). Returns how many went out.
async fn write_until_stalled(ws: &mut Ws, message: Message) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut wrote = 0;
    loop {
        match timeout(Duration::from_millis(300), ws.send(message.clone())).await {
            Err(_) | Ok(Err(_)) => return wrote,
            Ok(Ok(())) => wrote += 1,
        }
        assert!(Instant::now() < deadline, "writes never stalled");
    }
}

/// Read what is left until the socket ends; panics if it is still open
/// after `within`. Returns how many messages were left.
async fn ends_within(ws: &mut Ws, within: Duration) -> usize {
    let deadline = Instant::now() + within;
    let mut left = 0;
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match timeout(wait, ws.next()).await {
            Err(_) => panic!("still open after {within:?} ({left} messages read)"),
            Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return left,
            Ok(Some(Ok(_))) => left += 1,
        }
    }
}

/// The next text message of `kind`, within a few seconds.
async fn next_of(ws: &mut Ws, kind: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match timeout(wait, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                if value["type"] == kind {
                    return value;
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("no {kind:?} message: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_events_reader_is_dropped_and_frees_its_viewer() {
    let hub = TestHub::start(&[]).await;
    let mut healthy = hub.ws("/api/events?client=healthy", false).await;
    next_of(&mut healthy, "snapshot").await;
    let mut stalled = hub.ws("/api/events?client=stalled", true).await;
    // Someone is at it (its presence is held while its socket is) ...
    stalled
        .send(Message::text(
            json!({ "type": "view", "session_id": null, "present": true }).to_string(),
        ))
        .await
        .unwrap();
    let present = |hub: &TestHub| {
        hub.hub
            .activity()
            .last_active("stalled")
            .is_some_and(|at| chrono::Utc::now() - at < chrono::Duration::milliseconds(200))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !present(&hub) {
        assert!(Instant::now() < deadline, "the view never arrived");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // ... then every message it sends is answered (an error for one it
    // cannot parse), and it never reads the answers.
    assert!(write_until_stalled(&mut stalled, Message::text("?")).await > 0);

    // Past the send timeout the hub drops it: its connection, and with it
    // its presence, are gone (when it left stays put).
    let deadline = Instant::now() + SEND_TIMEOUT * 15;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if !present(&hub) {
            break;
        }
        assert!(Instant::now() < deadline, "the stalled reader was kept");
    }
    // The healthy client carries on ...
    let repo = hub._dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let made = hub
        .post(
            "/api/workspaces",
            json!({ "name": "after", "folders": [repo] }),
        )
        .await;
    assert!(made["id"].is_string(), "{made}");
    next_of(&mut healthy, "sessions").await;
    // ... and the stalled one gets what it had not read, then the end.
    ends_within(&mut stalled, Duration::from_secs(10)).await;
    hub.shutdown().await;
}

/// What a stand-in machine saw of one connection.
#[derive(Debug)]
enum Seen {
    /// The hub closed it (or it broke): `wrote` frames went out first.
    Ended { path: String, wrote: usize },
}

/// A WebSocket server standing in for a machine (its own TCP receive
/// buffer `SMALL_BUFFER`): `/api/flood` writes 16 KiB frames as fast as it
/// is read and reads what comes; `/api/deaf` never reads until `listen`
/// fires, then reads to the end. Each connection's end is sent on `seen`.
async fn stand_in_machine(
    mut listen: mpsc::Receiver<()>,
) -> (String, mpsc::UnboundedReceiver<Seen>) {
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(SMALL_BUFFER).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(16).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (seen, seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut deaf: Vec<(String, Ws)> = Vec::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { return };
                    let mut path = String::new();
                    let ws = tokio_tungstenite::accept_hdr_async(
                        stream,
                        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         response| {
                            path = request.uri().path().to_owned();
                            Ok(response)
                        },
                    )
                    .await
                    .unwrap();
                    if path == "/api/deaf" {
                        deaf.push((path, ws));
                        continue;
                    }
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let (mut tx, mut rx) = ws.split();
                        let mut wrote = 0;
                        let frame = Message::binary(vec![b'x'; 16 * 1024]);
                        loop {
                            tokio::select! {
                                sent = tx.send(frame.clone()) => {
                                    if sent.is_err() { break; }
                                    wrote += 1;
                                }
                                read = rx.next() => match read {
                                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                                    Some(Ok(_)) => {}
                                },
                            }
                        }
                        let _ = seen.send(Seen::Ended { path, wrote });
                    });
                }
                Some(()) = listen.recv() => {
                    for (path, mut ws) in deaf.drain(..) {
                        let seen = seen.clone();
                        tokio::spawn(async move {
                            while let Some(Ok(message)) = ws.next().await {
                                if message.is_close() { break; }
                            }
                            let _ = seen.send(Seen::Ended { path, wrote: 0 });
                        });
                    }
                }
            }
        }
    });
    (url, seen_rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_machine_proxy_drops_a_stalled_side_either_way() {
    let (listen, listen_rx) = mpsc::channel(1);
    let (url, mut seen) = stand_in_machine(listen_rx).await;
    let hub = TestHub::start(&[("box", url)]).await;

    // Machine -> browser: one browser stops reading, one keeps reading.
    let mut healthy = hub.ws("/m/box/api/flood", false).await;
    let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (stop_reading, mut stopped) = oneshot::channel::<()>();
    let reader = tokio::spawn({
        let frames = Arc::clone(&frames);
        async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => return Ok(healthy),
                    next = healthy.next() => match next {
                        Some(Ok(Message::Binary(_))) => {
                            frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        Some(Ok(_)) => {}
                        other => return Err(format!("the healthy relay ended: {other:?}")),
                    },
                }
            }
        }
    });
    let mut stalled = hub.ws("/m/box/api/flood", true).await;
    // The relay stalls on the stalled browser, then gives up on it: its
    // machine connection is closed, the other's goes on.
    // Within the stall plus one bounded close (no more writes to the side
    // that stalled), with room for the buffers to fill.
    let ended = timeout(SEND_TIMEOUT * 6, seen.recv())
        .await
        .expect("the stalled relay was kept")
        .unwrap();
    let Seen::Ended { path, wrote } = ended;
    assert_eq!(path, "/api/flood");
    assert!(wrote > 0);
    let at = frames.load(std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(SEND_TIMEOUT * 2).await;
    let after = frames.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        after > at + 10,
        "the healthy relay goes on: {at} -> {after}"
    );
    ends_within(&mut stalled, Duration::from_secs(10)).await;
    let _ = stop_reading.send(());
    let healthy = reader.await.unwrap().unwrap();
    assert!(
        seen.try_recv().is_err(),
        "only the stalled relay's machine connection ended"
    );
    drop(healthy);
    assert!(matches!(
        timeout(Duration::from_secs(10), seen.recv()).await,
        Ok(Some(Seen::Ended { .. }))
    ));

    // Browser -> machine: the machine stops reading; the relay gives up on
    // it and tells the browser the link was lost.
    let mut browser = hub.ws("/m/box/api/deaf", false).await;
    assert!(write_until_stalled(&mut browser, Message::binary(vec![b'y'; 16 * 1024])).await > 0);
    // Its close (1011) reaches the browser unless what the browser sent
    // and the hub never read turned the close into a reset.
    let deadline = Instant::now() + SEND_TIMEOUT * 6;
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match timeout(wait, browser.next()).await {
            Ok(Some(Ok(Message::Close(frame)))) => {
                assert_eq!(frame.map(|f| u16::from(f.code)), Some(1011));
                break;
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) => break,
            Err(_) => panic!("the relay to a deaf machine was kept"),
        }
    }
    // The machine's connection was dropped too: reading now, it ends.
    listen.send(()).await.unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(10), seen.recv()).await,
        Ok(Some(Seen::Ended { .. }))
    ));
    hub.shutdown().await;
}
