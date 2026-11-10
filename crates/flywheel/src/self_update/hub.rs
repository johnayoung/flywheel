//! The hub step: hand this home's serving hub off to the installed `fw`
//! exactly as `fw hub upgrade --binary <install dir>/fw` does -- by running
//! that verb, never with `--allow-restart` -- and the hub API client the
//! restart step uses.
//!
//! A hub that is not serving is skipped: nothing is started and no hub
//! file is written. `fw hub upgrade`'s exit codes map to the summary's
//! `hub.action`: 0 `handoff`, 3 `not-running` (it went away meanwhile),
//! 4 (refused as incompatible) and 5 (busy) `refused`, anything else
//! `failed`. A refused or failed handoff is not reverted: the binaries
//! and stores are consistent, and the hub is left to `fw hub upgrade
//! --allow-restart`.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `fw hub upgrade`'s exit codes (`flywheel_hub::handoff`).
const EXIT_NO_HUB: i32 = 3;
const EXIT_REFUSED: i32 = 4;
const EXIT_BUSY: i32 = 5;
/// How long the handed-off hub may take to answer its API.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// One API request's connect / read bound.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// How the hub step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handoff {
    /// No hub serves this home: nothing was started.
    NotRunning,
    /// Handed off from `old_pid` to `new_pid`.
    Done { old_pid: u32, new_pid: u32 },
    /// The running hub refused (incompatible or busy) and carries on.
    Refused { old_pid: u32, why: String },
    /// The handoff did not complete.
    Failed { old_pid: u32, why: String },
}

impl Handoff {
    /// The summary's `hub` object.
    pub fn to_json(&self) -> Value {
        let (action, old, new) = match self {
            Self::NotRunning => ("not-running", None, None),
            Self::Done { old_pid, new_pid } => ("handoff", Some(*old_pid), Some(*new_pid)),
            Self::Refused { old_pid, .. } => ("refused", Some(*old_pid), None),
            Self::Failed { old_pid, .. } => ("failed", Some(*old_pid), None),
        };
        serde_json::json!({"action": action, "old_pid": old, "new_pid": new})
    }
}

/// The `fw hub upgrade` command line this step runs, as a string a human
/// can run (`allow_restart`: the manual follow-up after a refusal).
pub fn upgrade_command(fw: &Path, hub_dir: &Path, allow_restart: bool) -> String {
    let fw = fw.display().to_string();
    let dir = hub_dir.display().to_string();
    let mut argv = vec![
        fw.as_str(),
        "hub",
        "upgrade",
        "--binary",
        fw.as_str(),
        "--state-dir",
        dir.as_str(),
    ];
    if allow_restart {
        argv.push("--allow-restart");
    }
    super::restart::shell_join(argv)
}

/// Hand the hub serving `hub_dir` off to `fw` (the installed binary) by
/// running `fw hub upgrade --binary <fw> --state-dir <hub_dir>`.
pub fn hand_off(fw: &Path, hub_dir: &Path) -> Handoff {
    let Some(old_pid) = super::drain::hub_pid(hub_dir) else {
        return Handoff::NotRunning;
    };
    let output = Command::new(fw)
        .args(["hub", "upgrade", "--binary"])
        .arg(fw)
        .arg("--state-dir")
        .arg(hub_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) => {
            return Handoff::Failed {
                old_pid,
                why: format!("cannot run {} hub upgrade: {e}", fw.display()),
            };
        }
    };
    let said = last_words(&output.stderr);
    match output.status.code() {
        Some(0) => match super::drain::hub_pid(hub_dir) {
            Some(new_pid) if new_pid != old_pid => Handoff::Done { old_pid, new_pid },
            Some(_) => Handoff::Failed {
                old_pid,
                why: format!("fw hub upgrade exited 0 but pid {old_pid} still serves"),
            },
            None => Handoff::Failed {
                old_pid,
                why: "fw hub upgrade exited 0 but no hub serves this home".to_owned(),
            },
        },
        Some(EXIT_NO_HUB) => Handoff::NotRunning,
        Some(code @ (EXIT_REFUSED | EXIT_BUSY)) => Handoff::Refused {
            old_pid,
            why: format!("fw hub upgrade exited {code}: {said}"),
        },
        status => Handoff::Failed {
            old_pid,
            why: format!(
                "fw hub upgrade {}: {said}",
                status.map_or_else(
                    || "was killed by a signal".to_owned(),
                    |c| format!("exited {c}")
                )
            ),
        },
    }
}

/// The last non-empty stderr line (what `fw hub upgrade` concluded).
fn last_words(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(|| "no detail".to_owned(), str::to_owned)
}

/// The hub's REST API, as its own token holder (`<hub_dir>/token`) on its
/// loopback address.
#[derive(Debug, Clone)]
pub struct Client {
    addr: SocketAddr,
    token: String,
}

impl Client {
    /// A client of the hub serving `hub_dir` now.
    pub fn connect(hub_dir: &Path) -> Result<Self, String> {
        let info = flywheel_hub::remote::RuntimeInfo::read(hub_dir)
            .map_err(|e| format!("cannot read the hub's runtime file: {e}"))?
            .ok_or_else(|| "no hub serves this home".to_owned())?;
        let token_file: PathBuf = hub_dir.join(flywheel_hub::auth::TOKEN_FILE);
        let token = std::fs::read_to_string(&token_file)
            .map_err(|e| format!("cannot read {}: {e}", token_file.display()))?
            .trim()
            .to_owned();
        Ok(Self {
            addr: info.connect_addr(),
            token,
        })
    }

    /// Wait until the hub answers `GET /api/sessions`.
    pub fn wait_ready(&self) -> Result<(), String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let why = match self.request("GET", "/api/sessions", None) {
                Ok((200, _)) => return Ok(()),
                Ok((code, body)) => format!("GET /api/sessions answered {code}: {body}"),
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "the hub did not answer within {}s: {why}",
                    READY_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// One HTTP/1.1 request: `(status, JSON body or null)`.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        let mut stream = TcpStream::connect_timeout(&self.addr, CONNECT_TIMEOUT)
            .map_err(|e| format!("cannot reach the hub at {}: {e}", self.addr))?;
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
        let body = body.map(Value::to_string).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
            self.token,
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| format!("{method} {path}: {e}"))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| format!("{method} {path}: {e}"))?;
        parse_response(&raw).ok_or_else(|| format!("{method} {path}: malformed response"))
    }
}

/// `(status, JSON body)` of a raw HTTP/1.1 response (chunked or not).
fn parse_response(raw: &[u8]) -> Option<(u16, Value)> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let payload = &raw[split + 4..];
    let status = head.split(' ').nth(1)?.parse().ok()?;
    let chunked = head
        .lines()
        .any(|l| l.to_ascii_lowercase().replace(' ', "") == "transfer-encoding:chunked");
    let body = if chunked {
        dechunk(payload)?
    } else {
        payload.to_vec()
    };
    Some((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line = rest.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&rest[..line]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_parse_plain_and_chunked() {
        let plain =
            b"HTTP/1.1 201 Created\r\ncontent-type: application/json\r\n\r\n{\"id\":\"s1\"}";
        assert_eq!(
            parse_response(plain),
            Some((201, serde_json::json!({"id": "s1"})))
        );
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n4\r\n:1}\n\r\n0\r\n\r\n";
        assert_eq!(
            parse_response(chunked),
            Some((200, serde_json::json!({"a": 1})))
        );
        assert_eq!(parse_response(b"garbage"), None);
    }

    #[test]
    fn hub_actions_follow_the_handoff() {
        let json = |h: Handoff| h.to_json();
        assert_eq!(
            json(Handoff::NotRunning),
            serde_json::json!({"action": "not-running", "old_pid": null, "new_pid": null})
        );
        assert_eq!(
            json(Handoff::Done {
                old_pid: 1,
                new_pid: 2
            }),
            serde_json::json!({"action": "handoff", "old_pid": 1, "new_pid": 2})
        );
        assert_eq!(
            json(Handoff::Refused {
                old_pid: 1,
                why: String::new()
            })["action"],
            "refused"
        );
        assert_eq!(
            json(Handoff::Failed {
                old_pid: 1,
                why: String::new()
            })["action"],
            "failed"
        );
    }

    #[test]
    fn the_manual_hub_command_allows_a_restart_and_quotes_paths() {
        let command = upgrade_command(Path::new("/opt/my bin/fw"), Path::new("/h/hub"), true);
        assert_eq!(
            command,
            "'/opt/my bin/fw' hub upgrade --binary '/opt/my bin/fw' --state-dir /h/hub --allow-restart"
        );
        assert!(!upgrade_command(Path::new("/b/fw"), Path::new("/h"), false).contains("--allow"));
    }

    #[test]
    fn a_hub_that_is_not_serving_is_skipped_without_running_anything() {
        let dir = tempfile::tempdir().unwrap();
        // A program that cannot run: never invoked when no hub serves.
        let outcome = hand_off(Path::new("/nonexistent/fw"), dir.path());
        assert_eq!(outcome, Handoff::NotRunning);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
