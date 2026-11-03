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

use crate::endpoint::{Endpoint, EndpointArgs, Socket, client_id};
use crate::script::resolve;
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
    let start = Instant::now();
    let deadline = timeout.map(|t| start + t);
    let outcome = runtime.block_on(async {
        let endpoint = match Endpoint::resolve(&args.endpoint) {
            Ok(endpoint) => endpoint,
            Err(message) => return Outcome::Failed(message),
        };
        let setup_deadline = deadline.map(|d| d.max(start + CHECK_FLOOR));
        run(
            &endpoint,
            &args.session,
            args.until,
            args.changed,
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

async fn run(
    endpoint: &Endpoint,
    query: &str,
    until: Until,
    changed: bool,
    deadline: Option<Instant>,
    setup_deadline: Option<Instant>,
) -> Outcome {
    let setup = async {
        // Subscribe before reading the record, so no change falls between.
        let mut socket = endpoint
            .websocket(&format!("/api/events?client={}", client_id("wait")))
            .await?;
        let sessions = endpoint.sessions().await?;
        // The hub's first message is every session's status for this client.
        let snapshot = loop {
            match next_event(&mut socket).await? {
                Some(event) if event.get("type").and_then(Value::as_str) == Some("snapshot") => {
                    break event;
                }
                _ => {}
            }
        };
        Ok::<_, String>((socket, sessions, snapshot))
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
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    if until != Until::Exit && !is_agent(&record) {
        return Outcome::Failed(format!(
            "session {name:?} is not an agent session (no status to wait for); \
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
            "timed out waiting for {:?} to {} (now {now})",
            tracker.name,
            until.goal()
        ))
    })
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
            Err(message) => return Outcome::Failed(message),
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
                    return Outcome::Failed(format!("session {:?} was removed", tracker.name));
                }
                Err(message) => return Outcome::Failed(message),
            }
            if let Some(outcome) = tracker.on_record(record, false) {
                return outcome;
            }
        }
    }
}

/// The next JSON event of the events socket (`None`: not one), or why the
/// socket ended.
async fn next_event(socket: &mut Socket) -> Result<Option<Value>, String> {
    let text = match socket.next().await {
        Some(Ok(Message::Text(text))) => text,
        Some(Ok(Message::Close(frame))) => {
            let why = frame
                .map(|f| f.reason.to_string())
                .filter(|r| !r.is_empty());
            return Err(format!(
                "the hub closed the events connection{}",
                why.map(|w| format!(": {w}")).unwrap_or_default()
            ));
        }
        Some(Ok(_)) => return Ok(None),
        Some(Err(e)) => return Err(format!("events connection failed: {e}")),
        None => return Err("the hub closed the events connection".into()),
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
    changed: bool,
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
            if self.changed && initial {
                return Some(Outcome::Failed(format!(
                    "session {:?} has already ended ({ended}); nothing to wait for",
                    self.name
                )));
            }
            return Some(Outcome::Met(ended));
        }
        Some(Outcome::Failed(format!(
            "session {:?} ended ({ended}) before it was {}",
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
        if self.changed {
            let unchanged = matches!(&self.baseline, Some(Some((_, s))) if *s == since);
            if first || unchanged {
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

    fn tracker(until: Until, changed: bool) -> Tracker {
        Tracker {
            until,
            changed,
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
