//! The status-report channel between agent sessions and the hub: a unix
//! socket in the (0700) state dir, mode 0600, so it is reachable only by
//! the hub's own user and never from the network, whatever `--bind` says.
//!
//! Agent sessions do not get the hub's access token. Each spawn of an
//! agent session gets its own random secret (in its environment, never on
//! a command line) that authorizes status reports for that one session and
//! nothing else -- not the API, not other sessions.
//!
//! Protocol: the client connects, writes one JSON [`Report`] line, and
//! reads one JSON line back: `{"ok":true}` or `{"ok":false,"error":"..."}`.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// Socket file name inside the state dir.
pub const SOCKET_FILE: &str = "report.sock";
/// Environment of an agent session: the socket path and its secret
/// (`FLYWHEEL_HUB_SESSION` names the session).
pub const SOCKET_ENV: &str = "FLYWHEEL_HUB_REPORT_SOCKET";
pub const SECRET_ENV: &str = "FLYWHEEL_HUB_REPORT_SECRET";
pub const SESSION_ENV: &str = "FLYWHEEL_HUB_SESSION";
/// Largest report line accepted (hook payloads carry prompts and tool
/// input; they are cut, not refused, by the client).
pub const MAX_REPORT_BYTES: usize = 256 * 1024;
/// How often the accept loop re-checks for [`ReportServer::stop`].
const STOP_POLL: Duration = Duration::from_millis(250);
/// Per-connection read/write deadline on both ends.
pub const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// One status report from an agent session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// The hub session id.
    pub session: String,
    /// The spawn's report secret.
    pub secret: String,
    /// Which agent sent it: `claude` or `codex`.
    pub source: String,
    /// The agent's event name (a Claude hook event, or `notify`).
    pub event: String,
    /// Event qualifier (a Claude notification type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The agent's own JSON payload (hook stdin, notify argument).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} for {}", self.source, self.event, self.session)
    }
}

/// The hub's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Report-channel failures.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("report socket {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("hub refused the report: {0}")]
    Refused(String),
    #[error("bad reply from the hub: {0}")]
    BadReply(String),
}

/// Handles one report; returns an error message to send back.
pub type Handler = dyn Fn(Report) -> Result<(), String> + Send + Sync;

/// The listening side: an accept thread that hands each report to the
/// handler on a short-lived thread.
pub struct ReportServer {
    path: PathBuf,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ReportServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReportServer")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ReportServer {
    /// Bind `<state_dir>/report.sock` (0600), replacing any socket file
    /// already there (one left by a crashed hub). The caller must hold the
    /// state dir's [`crate::remote::ServeLock`], so no live hub owns it.
    pub fn bind(state_dir: &Path, handler: Arc<Handler>) -> Result<Self, ReportError> {
        let path = state_dir.join(SOCKET_FILE);
        let io = |source| ReportError::Io {
            path: path.clone(),
            source,
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        let listener = UnixListener::bind(&path).map_err(io)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
        }
        let stopping = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stopping);
        let accept = std::thread::Builder::new()
            .name("hub-report".into())
            .spawn(move || accept_loop(listener, handler, stop_flag))
            .map_err(io)?;
        Ok(Self {
            path,
            stopping,
            accept: Some(accept),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting and remove the socket file. Does not depend on the
    /// socket file still existing (a removed state dir must not wedge it).
    pub fn stop(&mut self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for ReportServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Wait up to [`STOP_POLL`] for `listener` to have a connection pending.
fn readable(listener: &UnixListener) -> bool {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = libc::c_int::try_from(STOP_POLL.as_millis()).unwrap_or(250);
    // SAFETY: one valid pollfd, count 1.
    let ready = unsafe { libc::poll(&mut fd, 1, timeout) };
    ready > 0 && fd.revents & libc::POLLIN != 0
}

fn accept_loop(listener: UnixListener, handler: Arc<Handler>, stopping: Arc<AtomicBool>) {
    // Poll rather than block in accept, so `stop` never needs to connect.
    while !stopping.load(Ordering::SeqCst) {
        if !readable(&listener) {
            continue;
        }
        let Ok((stream, _)) = listener.accept() else {
            continue;
        };
        // Accepted sockets must block (the listener's mode is not inherited
        // on every platform, but be explicit).
        let _ = stream.set_nonblocking(false);
        let handler = Arc::clone(&handler);
        let spawned = std::thread::Builder::new()
            .name("hub-report-conn".into())
            .spawn(move || serve_connection(stream, &*handler));
        // Out of threads: the report is lost, the hub carries on.
        drop(spawned);
    }
}

fn serve_connection(stream: UnixStream, handler: &Handler) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let reply = match read_line(&stream) {
        Ok(line) => match serde_json::from_slice::<Report>(&line) {
            Ok(report) => handler(report),
            Err(e) => Err(format!("malformed report: {e}")),
        },
        Err(e) => Err(e),
    };
    let reply = match reply {
        Ok(()) => Reply {
            ok: true,
            error: None,
        },
        Err(error) => Reply {
            ok: false,
            error: Some(error),
        },
    };
    let mut stream = stream;
    if let Ok(mut line) = serde_json::to_vec(&reply) {
        line.push(b'\n');
        let _ = stream.write_all(&line);
    }
}

/// One newline-terminated line of at most [`MAX_REPORT_BYTES`].
fn read_line(stream: &UnixStream) -> Result<Vec<u8>, String> {
    let mut reader = BufReader::new(stream.take(MAX_REPORT_BYTES as u64 + 1));
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .map_err(|e| format!("reading the report: {e}"))?;
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > MAX_REPORT_BYTES {
            format!("report exceeds {MAX_REPORT_BYTES} bytes")
        } else {
            "report ended before its newline".to_owned()
        });
    }
    line.pop();
    Ok(line)
}

/// Send one report to the hub listening on `socket` and wait for its
/// answer.
pub fn send(socket: &Path, report: &Report) -> Result<(), ReportError> {
    let io = |source| ReportError::Io {
        path: socket.to_path_buf(),
        source,
    };
    let mut stream = UnixStream::connect(socket).map_err(io)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(io)?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(io)?;
    let mut line = serde_json::to_vec(report).map_err(|e| ReportError::BadReply(e.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(io)?;
    let mut answer = String::new();
    BufReader::new(stream.take(64 * 1024))
        .read_line(&mut answer)
        .map_err(io)?;
    let reply: Reply = serde_json::from_str(answer.trim_end())
        .map_err(|e| ReportError::BadReply(format!("{e}: {answer:?}")))?;
    if reply.ok {
        Ok(())
    } else {
        Err(ReportError::Refused(reply.error.unwrap_or_default()))
    }
}

/// Shrink an agent payload to fit a report: long strings (prompts, tool
/// output) are cut until the JSON is under `budget` bytes.
pub fn fit_payload(mut payload: serde_json::Value, budget: usize) -> serde_json::Value {
    let mut limit = 4096;
    while serde_json::to_vec(&payload).map_or(0, |v| v.len()) > budget && limit >= 16 {
        truncate_strings(&mut payload, limit);
        limit /= 4;
    }
    if serde_json::to_vec(&payload).map_or(0, |v| v.len()) > budget {
        // Still too big (a huge structure): keep only the scalar fields.
        if let serde_json::Value::Object(map) = &mut payload {
            map.retain(|_, v| !v.is_array() && !v.is_object());
        }
    }
    payload
}

fn truncate_strings(value: &mut serde_json::Value, limit: usize) {
    match value {
        serde_json::Value::String(s) if s.len() > limit => {
            let mut cut = limit;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s.truncate(cut);
        }
        serde_json::Value::Array(items) => {
            items.iter_mut().for_each(|v| truncate_strings(v, limit));
        }
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(|v| truncate_strings(v, limit));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn report(event: &str) -> Report {
        Report {
            session: "s1".into(),
            secret: "k".into(),
            source: "claude".into(),
            event: event.into(),
            detail: None,
            payload: Some(serde_json::json!({ "session_id": "abc" })),
        }
    }

    #[test]
    fn reports_round_trip_and_refusals_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mut server = ReportServer::bind(
            dir.path(),
            Arc::new(move |r: Report| {
                if r.event == "Bad" {
                    return Err("nope".into());
                }
                sink.lock().unwrap().push(r);
                Ok(())
            }),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(server.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        send(server.path(), &report("Stop")).unwrap();
        assert_eq!(seen.lock().unwrap().as_slice(), &[report("Stop")]);
        match send(server.path(), &report("Bad")) {
            Err(ReportError::Refused(message)) => assert_eq!(message, "nope"),
            other => panic!("{other:?}"),
        }

        // Garbage and oversized lines get an error reply, not a hang.
        let mut raw = UnixStream::connect(server.path()).unwrap();
        raw.write_all(b"not json\n").unwrap();
        let mut answer = String::new();
        BufReader::new(&raw).read_line(&mut answer).unwrap();
        assert!(answer.contains("malformed report"), "{answer}");
        let mut raw = UnixStream::connect(server.path()).unwrap();
        let _ = raw.write_all(&vec![b'x'; MAX_REPORT_BYTES + 10]);
        let mut answer = String::new();
        BufReader::new(&raw).read_line(&mut answer).unwrap();
        assert!(answer.contains("exceeds"), "{answer}");

        let path = server.path().to_path_buf();
        server.stop();
        assert!(!path.exists());
        assert!(send(&path, &report("Stop")).is_err());
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let stale = UnixListener::bind(dir.path().join(SOCKET_FILE)).unwrap();
        drop(stale); // the file stays, nothing listens
        let server = ReportServer::bind(dir.path(), Arc::new(|_| Ok(()))).unwrap();
        send(server.path(), &report("Stop")).unwrap();
    }

    #[test]
    fn payloads_are_cut_to_fit() {
        let big = serde_json::json!({
            "session_id": "abc",
            "prompt": "p".repeat(100_000),
            "tool_input": { "content": "é".repeat(50_000) },
        });
        let fitted = fit_payload(big, 16 * 1024);
        assert!(serde_json::to_vec(&fitted).unwrap().len() <= 16 * 1024);
        assert_eq!(fitted["session_id"], "abc");
        let small = serde_json::json!({ "a": "b" });
        assert_eq!(fit_payload(small.clone(), 1024), small);
    }
}
