//! `fw hub wait SESSION --until done|blocked|idle|working|exit`: block until
//! a session reaches a state, watching the hub's events socket (no
//! polling). Exit 0 when met, 124 on `--timeout`, 1 on any error (the
//! session ended before reaching an agent state, the hub went away, ...).
//!
//! The session's state is always checked once, however short `--timeout`
//! (`--timeout 0`: check and return): reaching the hub and reading that
//! first state may take up to [`CHECK_FLOOR`] or the timeout, whichever is
//! longer; past it the wait times out (124).
//!
//! The wait is its own events client: it never views a session, so a turn
//! that finishes reads `done` to it whoever else looked.
//!
//! `--changed` against this machine's hub counts any status that began
//! after the wait process started (Linux: its start time, to the 10 ms
//! clock tick), so `fw hub wait --changed & fw hub send ...` cannot miss a
//! turn that finished before the wait had read the starting state.

use crate::endpoint::{Endpoint, EndpointArgs, Socket, client_id};
use crate::script::resolve;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde_json::Value;
use std::io::Write;
use std::time::Duration;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

/// Exit code: the condition was met.
pub const MET: i32 = 0;
/// Exit code: an error (bad session, hub unreachable, unreachable state).
pub const FAILED: i32 = 1;
/// Exit code: `--timeout` passed first (as `timeout(1)`).
pub const TIMED_OUT: i32 = 124;

/// How long the first check may take whatever `--timeout` says.
pub const CHECK_FLOOR: Duration = Duration::from_secs(1);
/// How long a wait keeps reconnecting after the hub closed its events
/// socket for a restart (`fw hub upgrade`).
const RECONNECT_WINDOW: Duration = Duration::from_secs(120);

/// Why the events socket ended.
enum Ended {
    /// The hub process is being replaced (close code 1012): reconnect.
    Restart,
    Failed(String),
}

fn events_path() -> String {
    format!("/api/events?client={}", client_id("wait"))
}

/// What to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Until {
    /// The agent finished a turn (unseen by this wait: any finished turn).
    Done,
    /// The agent waits on a person (a permission prompt, a question).
    Blocked,
    /// The agent is at rest: finished (`done`) or waiting for a prompt.
    Idle,
    /// The agent is busy.
    Working,
    /// The session's process ended (any session kind).
    Exit,
}

impl Until {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Exit => "exit",
        }
    }

    /// What is awaited, for messages (`be done`, `exit`).
    fn goal(self) -> String {
        match self {
            Self::Exit => "exit".into(),
            other => format!("be {}", other.as_str()),
        }
    }

    /// Whether agent status `status` meets this.
    fn met_by(self, status: &str) -> bool {
        match self {
            Self::Done => status == "done",
            Self::Blocked => status == "blocked",
            Self::Idle => matches!(status, "idle" | "done"),
            Self::Working => status == "working",
            Self::Exit => false,
        }
    }
}

#[derive(Debug, clap::Args)]
pub(crate) struct WaitArgs {
    /// Session id, unique id prefix, or name.
    session: String,
    /// The state to wait for (done/blocked/idle/working: agent sessions).
    #[arg(long, value_enum)]
    until: Until,
    /// Give up after this many seconds (exit 124). Default: wait forever.
    #[arg(long, value_name = "SECS")]
    timeout: Option<f64>,
    /// Ignore the state the session is in when the wait starts: only a
    /// change after it counts (e.g. the turn a just-sent prompt starts,
    /// not the previous one's `done`).
    #[arg(long)]
    changed: bool,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

/// How a wait ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Met: what to print (the status, or `exited <code>`).
    Met(String),
    TimedOut(String),
    Failed(String),
}

pub(crate) fn wait(args: &WaitArgs, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let timeout = match args.timeout {
        Some(secs) if !(secs.is_finite() && secs >= 0.0) => {
            let _ = writeln!(
                err,
                "fw hub wait: --timeout must be a number of seconds >= 0"
            );
            return FAILED;
        }
        Some(secs) => Some(Duration::from_secs_f64(secs.min(1e9))),
        None => None,
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err, "fw hub wait: cannot start: {e}");
            return FAILED;
        }
    };
    let started = process_started();
    let start = Instant::now();
    let deadline = timeout.map(|t| start + t);
    let outcome = runtime.block_on(async {
        let endpoint = match Endpoint::resolve(&args.endpoint) {
            Ok(endpoint) => endpoint,
            Err(message) => return Outcome::Failed(message),
        };
        let setup_deadline = deadline.map(|d| d.max(start + CHECK_FLOOR));
        // Only this machine's hub shares the wait's clock.
        let started = endpoint.local_info().map(|_| started);
        run(
            &endpoint,
            &args.session,
            args.until,
            Changed {
                after_baseline: args.changed,
                started,
            },
            deadline,
            setup_deadline,
        )
        .await
    });
    match outcome {
        Outcome::Met(what) => {
            let _ = writeln!(out, "{what}");
            MET
        }
        Outcome::TimedOut(message) => {
            let _ = writeln!(err, "fw hub wait: {message}");
            TIMED_OUT
        }
        Outcome::Failed(message) => {
            let _ = writeln!(err, "fw hub wait: {message}");
            FAILED
        }
    }
}

/// A session's process state: `None` while it runs.
fn ended(record: &Value) -> Option<String> {
    let state = record.get("state").and_then(Value::as_str).unwrap_or("");
    let live = record.get("live").is_some_and(|l| !l.is_null());
    let pid = record
        .get("live")
        .and_then(|l| l.get("pid"))
        .is_some_and(|p| !p.is_null());
    match state {
        "running" if live && pid => None,
        "running" if live => Some("exited".into()),
        "running" => Some("gone (no terminal on this hub)".into()),
        "exited" => Some(match record.get("signal").and_then(Value::as_str) {
            Some(signal) => format!("killed ({signal})"),
            None => format!(
                "exited {}",
                record
                    .get("code")
                    .and_then(Value::as_i64)
                    .map_or_else(|| "?".to_owned(), |c| c.to_string())
            ),
        }),
        other => Some(other.to_owned()),
    }
}

fn is_agent(record: &Value) -> bool {
    matches!(
        record.get("kind").and_then(Value::as_str),
        Some("claude" | "codex")
    ) && record.get("fallback_command").is_none_or(Value::is_null)
}

/// The tracker's view of one session's status: `(status, since)`.
fn status_of(view: &Value) -> Option<(String, String)> {
    let status = view.get("status")?.as_str()?.to_owned();
    let since = view
        .get("since")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Some((status, since))
}

/// When the wait process started: from the kernel's record of it on Linux
/// (so a status set by anything run after this process was spawned is
/// later), else now.
fn process_started() -> DateTime<Utc> {
    process_age().map_or_else(Utc::now, |age| {
        Utc::now() - chrono::Duration::from_std(age).unwrap_or_default()
    })
}

/// How long ago this process started (`/proc/self/stat` start time, in
/// clock ticks since boot, against `CLOCK_BOOTTIME`).
#[cfg(target_os = "linux")]
fn process_age() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Field 22; the command name (field 2) may hold spaces, so count from
    // its closing parenthesis (field 3 is the first after it).
    let ticks: u64 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    // SAFETY: sysconf only reads a constant.
    let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    if hz == 0 {
        return None;
    }
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for clock_gettime to fill.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) } != 0 {
        return None;
    }
    let up = Duration::new(
        u64::try_from(now.tv_sec).ok()?,
        u32::try_from(now.tv_nsec).ok()?,
    );
    let began =
        Duration::from_secs(ticks / hz) + Duration::from_nanos((ticks % hz) * 1_000_000_000 / hz);
    up.checked_sub(began)
}

#[cfg(not(target_os = "linux"))]
fn process_age() -> Option<Duration> {
    None
}

/// What `--changed` counts as a change.
#[derive(Debug, Clone, Copy)]
struct Changed {
    /// `--changed`: the status the wait first reads does not count...
    after_baseline: bool,
    /// ...unless it began at or after this (the wait's start, on the
    /// hub's clock; `None`: another machine's hub).
    started: Option<DateTime<Utc>>,
}

async fn run(
    endpoint: &Endpoint,
    query: &str,
    until: Until,
    changed: Changed,
    deadline: Option<Instant>,
    setup_deadline: Option<Instant>,
) -> Outcome {
    let setup = async {
        // A wait started while `fw hub upgrade` hands the hub over starts
        // against the next process.
        let retry_until = Instant::now() + RECONNECT_WINDOW;
        let mut delay = Duration::from_millis(200);
        loop {
            match start(endpoint).await {
                Ok(parts) => return Ok(parts),
                Err(Ended::Restart) if Instant::now() < retry_until => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
                Err(Ended::Restart) => {
                    return Err(format!(
                        "the hub restarted and did not come back within {}s",
                        RECONNECT_WINDOW.as_secs()
                    ));
                }
                Err(Ended::Failed(message)) => return Err(message),
            }
        }
    };
    let setup = match setup_deadline {
        Some(at) => match tokio::time::timeout_at(at, setup).await {
            Ok(result) => result,
            Err(_) => {
                return Outcome::TimedOut(format!(
                    "timed out reaching the hub at {} to wait for {query:?}",
                    endpoint.base()
                ));
            }
        },
        None => setup.await,
    };
    let (mut socket, sessions, snapshot) = match setup {
        Ok(parts) => parts,
        Err(message) => return Outcome::Failed(message),
    };
    let mut record = match resolve(&sessions, query, "session") {
        Ok(record) => record.clone(),
        Err(message) => return Outcome::Failed(message),
    };
    let id = record
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let name = crate::script::session_label(&record);
    if until != Until::Exit && !is_agent(&record) {
        return Outcome::Failed(format!(
            "session {name} is not an agent session (no status to wait for); \
             --until exit works for any session"
        ));
    }
    let mut tracker = Tracker {
        until,
        changed,
        name,
        baseline: None,
        last: None,
    };
    if let Some(outcome) = tracker.on_record(&record, true) {
        return outcome;
    }
    if let Some(outcome) = tracker.on_status(snapshot.get("sessions").and_then(|s| s.get(&id))) {
        return outcome;
    }
    let watched = watch(endpoint, &mut socket, &id, &mut tracker, &mut record);
    let outcome = match deadline {
        Some(at) => tokio::time::timeout_at(at, watched).await.ok(),
        None => Some(watched.await),
    };
    outcome.unwrap_or_else(|| {
        let now = tracker.last.as_ref().map_or_else(
            || ended(&record).unwrap_or_else(|| "running".into()),
            |(status, _)| status.clone(),
        );
        Outcome::TimedOut(format!(
            "timed out waiting for {} to {} (now {now})",
            tracker.name,
            until.goal()
        ))
    })
}

/// Subscribe to the events, read every session, and take the hub's first
/// message (every session's status for this client). A handoff under way
/// (the frozen hub's 503, or the socket closed for the restart) is
/// [`Ended::Restart`].
async fn start(endpoint: &Endpoint) -> Result<(Socket, Vec<Value>, Value), Ended> {
    let failed = |message: String| {
        if crate::handoff::handing_over(&message) {
            Ended::Restart
        } else {
            Ended::Failed(message)
        }
    };
    // Subscribe before reading the record, so no change falls between.
    let mut socket = endpoint.websocket(&events_path()).await.map_err(failed)?;
    let sessions = endpoint.sessions().await.map_err(failed)?;
    loop {
        match next_event(&mut socket).await? {
            Some(event) if event.get("type").and_then(Value::as_str) == Some("snapshot") => {
                return Ok((socket, sessions, event));
            }
            _ => {}
        }
    }
}

/// Follow the events socket until the tracker decides.
async fn watch(
    endpoint: &Endpoint,
    socket: &mut Socket,
    id: &str,
    tracker: &mut Tracker,
    record: &mut Value,
) -> Outcome {
    loop {
        let event = match next_event(socket).await {
            Ok(Some(event)) => event,
            Ok(None) => continue,
            Err(Ended::Failed(message)) => return Outcome::Failed(message),
            Err(Ended::Restart) => {
                if let Some(outcome) = resume(endpoint, socket, id, tracker, record).await {
                    return outcome;
                }
                continue;
            }
        };
        let reread = match event.get("type").and_then(Value::as_str) {
            Some("snapshot") => {
                let view = event.get("sessions").and_then(|s| s.get(id));
                if let Some(outcome) = tracker.on_status(view) {
                    return outcome;
                }
                false
            }
            Some("status") if event.get("session_id").and_then(Value::as_str) == Some(id) => {
                let view = event.get("status").filter(|v| !v.is_null());
                if let Some(outcome) = tracker.on_status(view) {
                    return outcome;
                }
                // No longer tracked: ended or removed.
                view.is_none()
            }
            Some("sessions") => true,
            _ => false,
        };
        if reread {
            match refresh(endpoint, id).await {
                Ok(Some(fresh)) => *record = fresh,
                Ok(None) => {
                    return Outcome::Failed(format!("session {} was removed", tracker.name));
                }
                // Frozen for a handoff since the event: this socket is
                // about to close for the restart; carry on in the next
                // process now.
                Err(message) if crate::handoff::handing_over(&message) => {
                    if let Some(outcome) = resume(endpoint, socket, id, tracker, record).await {
                        return outcome;
                    }
                    continue;
                }
                Err(message) => return Outcome::Failed(message),
            }
            if let Some(outcome) = tracker.on_record(record, false) {
                return outcome;
            }
        }
    }
}

/// A live handoff: the session carries on in the next hub process; its
/// first message (a snapshot) and a re-read record catch up on what
/// happened meanwhile. `Some` when that decides the wait.
async fn resume(
    endpoint: &Endpoint,
    socket: &mut Socket,
    id: &str,
    tracker: &mut Tracker,
    record: &mut Value,
) -> Option<Outcome> {
    match reconnect(endpoint, id).await {
        Ok((fresh, Some(read))) => {
            *socket = fresh;
            *record = read;
        }
        Ok((_, None)) => {
            return Some(Outcome::Failed(format!(
                "session {} was removed",
                tracker.name
            )));
        }
        Err(message) => return Some(Outcome::Failed(message)),
    }
    tracker.on_record(record, false)
}

/// Connect the events socket again after a restart and re-read session
/// `id` (`None`: removed), retrying while the next hub process comes up
/// (the frozen one answers 503 meanwhile).
async fn reconnect(endpoint: &Endpoint, id: &str) -> Result<(Socket, Option<Value>), String> {
    let deadline = Instant::now() + RECONNECT_WINDOW;
    let mut delay = Duration::from_millis(200);
    loop {
        let attempt = async {
            let socket = endpoint.websocket(&events_path()).await?;
            let record = refresh(endpoint, id).await?;
            Ok::<_, String>((socket, record))
        };
        match attempt.await {
            Ok(done) => return Ok(done),
            Err(message) if Instant::now() >= deadline => {
                return Err(format!(
                    "the hub restarted and did not come back within {}s: {message}",
                    RECONNECT_WINDOW.as_secs()
                ));
            }
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// The next JSON event of the events socket (`None`: not one), or why the
/// socket ended.
async fn next_event(socket: &mut Socket) -> Result<Option<Value>, Ended> {
    let text = match socket.next().await {
        Some(Ok(Message::Text(text))) => text,
        Some(Ok(Message::Close(frame))) => {
            if frame
                .as_ref()
                .is_some_and(|f| u16::from(f.code) == crate::server::WS_CLOSE_RESTART)
            {
                return Err(Ended::Restart);
            }
            let why = frame
                .map(|f| f.reason.to_string())
                .filter(|r| !r.is_empty());
            return Err(Ended::Failed(format!(
                "the hub closed the events connection{}",
                why.map(|w| format!(": {w}")).unwrap_or_default()
            )));
        }
        Some(Ok(_)) => return Ok(None),
        Some(Err(e)) => return Err(Ended::Failed(format!("events connection failed: {e}"))),
        None => return Err(Ended::Failed("the hub closed the events connection".into())),
    };
    Ok(serde_json::from_str::<Value>(text.as_str()).ok())
}

/// Re-read one session (`None`: it was removed).
async fn refresh(endpoint: &Endpoint, id: &str) -> Result<Option<Value>, String> {
    let sessions = endpoint.sessions().await?;
    Ok(sessions
        .into_iter()
        .find(|s| s.get("id").and_then(Value::as_str) == Some(id)))
}

/// Decides, from what the hub says, whether the wait is over.
struct Tracker {
    until: Until,
    changed: Changed,
    name: String,
    /// The status when the wait started (`--changed` ignores it).
    baseline: Option<Option<(String, String)>>,
    last: Option<(String, String)>,
}

impl Tracker {
    /// A session record (`initial`: as the wait starts): decides `exit`,
    /// and fails agent waits whose session ended. Under `--changed` an
    /// exit already true at the start fails (no new one can come).
    fn on_record(&mut self, record: &Value, initial: bool) -> Option<Outcome> {
        let ended = ended(record)?;
        if self.until == Until::Exit {
            if self.changed.after_baseline && initial {
                return Some(Outcome::Failed(format!(
                    "session {} has already ended ({ended}); nothing to wait for",
                    self.name
                )));
            }
            return Some(Outcome::Met(ended));
        }
        Some(Outcome::Failed(format!(
            "session {} ended ({ended}) before it was {}",
            self.name,
            self.until.as_str()
        )))
    }

    /// The session's agent status for this client (`None`: not tracked).
    fn on_status(&mut self, view: Option<&Value>) -> Option<Outcome> {
        let current = view.and_then(status_of);
        let first = self.baseline.is_none();
        if first {
            self.baseline = Some(current.clone());
        }
        self.last.clone_from(&current);
        if self.until == Until::Exit {
            return None;
        }
        let (status, since) = current?;
        if !self.until.met_by(&status) {
            return None;
        }
        if self.changed.after_baseline {
            let unchanged = matches!(&self.baseline, Some(Some((_, s))) if *s == since);
            let began = DateTime::parse_from_rfc3339(&since).ok();
            let since_start = matches!(
                (began, self.changed.started),
                (Some(began), Some(started)) if began >= started
            );
            if (first || unchanged) && !since_start {
                return None;
            }
        }
        Some(Outcome::Met(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A wait begun while `fw hub upgrade` has the hub frozen (every
    /// request answered 503) waits for the next process instead of failing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wait_begun_while_the_hub_hands_over_waits_for_the_next_process() {
        use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let gate = crate::handoff::Gate::new();
        gate.set_open(false);
        let refused = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&refused);
        let app = axum::Router::new()
            .route(
                "/api/sessions",
                axum::routing::get(|| async {
                    axum::Json(json!({ "sessions": [{
                        "id": "s1", "name": "agent", "kind": "claude",
                        "state": "running", "live": { "pid": 1 },
                    }] }))
                }),
            )
            .route(
                "/api/events",
                axum::routing::get(|ws: WebSocketUpgrade| async move {
                    ws.on_upgrade(|mut socket| async move {
                        let snapshot = json!({ "type": "snapshot", "sessions": {
                            "s1": { "status": "done", "since": "2026-11-04T00:00:00Z" },
                        } });
                        let _ = socket
                            .send(WsMessage::Text(snapshot.to_string().into()))
                            .await;
                        while let Some(Ok(_)) = socket.recv().await {}
                    })
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                gate.clone(),
                crate::handoff::gate_requests,
            ))
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let counted = Arc::clone(&counted);
                    async move {
                        let response = next.run(request).await;
                        if response.status() == axum::http::StatusCode::SERVICE_UNAVAILABLE {
                            counted.fetch_add(1, Ordering::SeqCst);
                        }
                        response
                    }
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::resolve(&EndpointArgs {
            url: Some(format!("http://{addr}")),
            token: Some("test".into()),
            machine: None,
            fingerprint: None,
            state: crate::cli::StateDirArg {
                state_dir: Some(state.path().to_path_buf()),
            },
        })
        .unwrap();
        // The new process takes over once the wait has been refused.
        let opener = {
            let gate = gate.clone();
            let refused = Arc::clone(&refused);
            tokio::spawn(async move {
                while refused.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                gate.set_open(true);
            })
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        let outcome = run(
            &endpoint,
            "s1",
            Until::Done,
            Changed {
                after_baseline: false,
                started: None,
            },
            Some(deadline),
            Some(deadline),
        )
        .await;
        opener.await.unwrap();
        assert!(
            refused.load(Ordering::SeqCst) > 0,
            "the hub was never frozen"
        );
        assert!(matches!(outcome, Outcome::Met(_)), "{outcome:?}");
    }

    #[test]
    fn only_a_frozen_hubs_reply_is_a_handoff() {
        assert!(crate::handoff::handing_over(&format!(
            "{} (HTTP 503)",
            crate::handoff::FROZEN_REPLY
        )));
        assert!(!crate::handoff::handing_over(
            "the hub is at its connection limit (HTTP 503)"
        ));
        assert!(!crate::handoff::handing_over(crate::handoff::FROZEN_REPLY));
    }

    fn tracker(until: Until, changed: bool) -> Tracker {
        Tracker {
            until,
            changed: Changed {
                after_baseline: changed,
                started: None,
            },
            name: "s".into(),
            baseline: None,
            last: None,
        }
    }

    fn view(status: &str, since: &str) -> Value {
        json!({ "status": status, "since": since })
    }

    #[test]
    fn a_status_already_in_place_meets_a_plain_wait() {
        let mut t = tracker(Until::Done, false);
        assert_eq!(
            t.on_status(Some(&view("done", "t1"))),
            Some(Outcome::Met("done".into()))
        );
        let mut t = tracker(Until::Idle, false);
        assert_eq!(
            t.on_status(Some(&view("done", "t1"))),
            Some(Outcome::Met("done".into()))
        );
        let mut t = tracker(Until::Blocked, false);
        assert_eq!(t.on_status(Some(&view("working", "t1"))), None);
        assert_eq!(
            t.on_status(Some(&view("blocked", "t2"))),
            Some(Outcome::Met("blocked".into()))
        );
    }

    #[test]
    fn changed_ignores_the_starting_status_until_a_new_one() {
        let mut t = tracker(Until::Done, true);
        assert_eq!(t.on_status(Some(&view("done", "t1"))), None);
        // Re-sent unchanged (another client viewed it): still the old turn.
        assert_eq!(t.on_status(Some(&view("done", "t1"))), None);
        assert_eq!(t.on_status(Some(&view("working", "t2"))), None);
        assert_eq!(
            t.on_status(Some(&view("done", "t3"))),
            Some(Outcome::Met("done".into()))
        );
    }

    #[test]
    fn changed_counts_a_status_that_began_after_the_wait_started() {
        let started = DateTime::parse_from_rfc3339("2026-11-03T10:00:00.500Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut t = tracker(Until::Done, true);
        t.changed.started = Some(started);
        // The previous turn, done before the wait started: not a change.
        assert_eq!(
            t.on_status(Some(&view("done", "2026-11-03T10:00:00.499Z"))),
            None
        );
        // A turn that finished after the wait started but before it read
        // the session is the first status it sees: a change all the same.
        let mut t = tracker(Until::Done, true);
        t.changed.started = Some(started);
        assert_eq!(
            t.on_status(Some(&view("done", "2026-11-03T10:00:00.500Z"))),
            Some(Outcome::Met("done".into()))
        );
        // Another machine's hub (no shared clock): the baseline rule alone.
        let mut t = tracker(Until::Done, true);
        assert_eq!(
            t.on_status(Some(&view("done", "2026-11-03T10:00:01Z"))),
            None
        );
    }

    #[test]
    fn the_process_start_is_no_later_than_anything_this_process_did() {
        #[cfg(target_os = "linux")]
        let earlier = std::time::Instant::now();
        let before = Utc::now();
        std::thread::sleep(Duration::from_millis(30));
        assert!(process_started() <= before);
        #[cfg(target_os = "linux")]
        {
            let age = process_age().expect("/proc/self/stat start time");
            assert!(age >= earlier.elapsed(), "{age:?}");
            assert!(age < Duration::from_secs(24 * 3600), "{age:?}");
        }
    }

    #[test]
    fn an_agent_wait_fails_when_the_session_ends() {
        let mut t = tracker(Until::Done, false);
        let exited = json!({ "state": "exited", "code": 0, "live": { "pid": null } });
        assert!(
            matches!(t.on_record(&exited, false), Some(Outcome::Failed(m)) if m.contains("exited 0"))
        );
        let running = json!({ "state": "running", "live": { "pid": 5 } });
        assert_eq!(t.on_record(&running, false), None);
    }

    #[test]
    fn exit_waits_report_how_it_ended() {
        let mut t = tracker(Until::Exit, false);
        let killed = json!({ "state": "exited", "signal": "Hangup", "live": { "pid": null } });
        assert_eq!(
            t.on_record(&killed, true),
            Some(Outcome::Met("killed (Hangup)".into()))
        );
        let mut t = tracker(Until::Exit, true);
        assert!(matches!(
            t.on_record(&killed, true),
            Some(Outcome::Failed(_))
        ));
    }

    #[test]
    fn only_running_agent_kinds_have_status() {
        assert!(is_agent(&json!({ "kind": "claude" })));
        assert!(!is_agent(&json!({ "kind": "shell" })));
        assert!(!is_agent(
            &json!({ "kind": "codex", "fallback_command": ["sh"] })
        ));
    }
}
