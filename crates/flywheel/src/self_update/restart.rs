//! The restart step: bring back what the drain stopped.
//!
//! A `hub-worker` gets a new worker session through `POST
//! /api/flywheel/worker` with its session's `folder`, `workspace_id`,
//! `tasks`, and `with_prereqs` (always with `another`: it brings back a
//! worker this run stopped, so a sibling still running in the folder -- one
//! left `not_stopped` by a drain timeout, or one restarted just before it --
//! is no duplicate); a `hub-autopilot` restarts through `POST
//! /api/flywheel/autopilot/start`. Either runs whatever binary the hub
//! runs: the newly installed `fw` after a handoff, the previous one when a
//! reverted run restarts what it stopped. A restart counts only once its
//! daemon process is seen running under the new session's wrapper.
//!
//! A `terminal` process -- and a hub process that cannot be restarted --
//! gets a `manual` entry instead: `cd <cwd> && <install dir>/fw <original
//! args after argv[0]>`, shell-quoted. Credential variables it ran with
//! ([`super::drain::SECRET_ENV`]) are named in its notes, never valued.

use super::drain::{self, Drainable, Kind, Role};
use super::hub::Client;
use flywheel_hub::flywheel::WorkerScope;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long a restarted session may take to show its daemon process.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(30);

/// What the restart did.
#[derive(Debug, Clone, Default)]
pub struct Restarted {
    /// `{kind, session_id, folder, pid}` per restarted process.
    pub restarted: Vec<Value>,
    /// `{pid, cwd, command, notes}` per process left to the operator.
    pub manual: Vec<Value>,
    /// Hub restarts that were tried and did not happen.
    pub failures: Vec<String>,
}

impl Restarted {
    /// The restart step's one-line detail.
    pub fn detail(&self) -> String {
        let mut parts = Vec::new();
        if !self.restarted.is_empty() {
            let each: Vec<String> = self
                .restarted
                .iter()
                .map(|r| {
                    format!(
                        "{} pid {} (session {})",
                        r["kind"].as_str().unwrap_or(""),
                        r["pid"],
                        r["session_id"].as_str().unwrap_or("")
                    )
                })
                .collect();
            parts.push(format!("restarted {}", each.join(", ")));
        }
        if !self.failures.is_empty() {
            parts.push(format!("not restarted: {}", self.failures.join("; ")));
        }
        if !self.manual.is_empty() {
            let each: Vec<String> = self
                .manual
                .iter()
                .map(|m| {
                    let notes: Vec<&str> = m["notes"]
                        .as_array()
                        .map(|n| n.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    if notes.is_empty() {
                        format!("pid {}", m["pid"])
                    } else {
                        format!("pid {} ({})", m["pid"], notes.join("; "))
                    }
                })
                .collect();
            parts.push(format!(
                "left to the operator (see manual): {}",
                each.join(", ")
            ));
        }
        if parts.is_empty() {
            "nothing was stopped".to_owned()
        } else {
            parts.join("; ")
        }
    }

    /// Whether any hub restart was attempted.
    pub fn attempted(&self) -> bool {
        !self.restarted.is_empty() || !self.failures.is_empty()
    }
}

/// Restart `stopped`: hub kinds through `hub` (when given and the process
/// has a recorded session), everything else as a `manual` entry running
/// `install_fw`. `no_hub` is the note a hub process gets when `hub` is
/// `None`.
pub fn restart(
    stopped: &[Drainable],
    hub: Option<&Client>,
    install_fw: &Path,
    no_hub: &str,
) -> Restarted {
    let mut out = Restarted::default();
    for process in stopped {
        if process.kind == Kind::Terminal {
            out.manual.push(manual_entry(process, install_fw, None));
            continue;
        }
        let Some(session) = process.session.as_ref() else {
            let note = format!("{} without a recorded hub session", process.kind.name());
            out.manual
                .push(manual_entry(process, install_fw, Some(note)));
            continue;
        };
        let Some(client) = hub else {
            out.manual
                .push(manual_entry(process, install_fw, Some(no_hub.to_owned())));
            continue;
        };
        let folder = session
            .folder
            .clone()
            .or_else(|| process.cwd.clone())
            .unwrap_or_default();
        let (path, verb, body) = if process.kind == Kind::HubWorker {
            let scope = WorkerScope::of_command(&session.command).unwrap_or_default();
            (
                "/api/flywheel/worker",
                "worker",
                json!({
                    "folder": folder,
                    "workspace_id": session.workspace_id,
                    "tasks": scope.tasks,
                    "with_prereqs": scope.with_prereqs,
                    "another": true,
                }),
            )
        } else {
            (
                "/api/flywheel/autopilot/start",
                "autopilot",
                json!({
                    "folder": folder,
                    "workspace_id": session.workspace_id,
                    "confirm": true,
                }),
            )
        };
        match start(client, path, &body, verb) {
            Ok((session_id, pid)) => out.restarted.push(json!({
                "kind": process.kind.name(),
                "session_id": session_id,
                "folder": folder.display().to_string(),
                "pid": pid,
            })),
            Err(why) => {
                let why = format!(
                    "{} pid {} in {}: {why}",
                    process.kind.name(),
                    process.pid,
                    folder.display()
                );
                out.manual.push(manual_entry(
                    process,
                    install_fw,
                    Some(format!("the hub restart failed: {why}")),
                ));
                out.failures.push(why);
            }
        }
    }
    out
}

/// POST `body` to `path`, then wait for the new session's `verb` daemon:
/// `(session id, daemon pid)`.
fn start(client: &Client, path: &str, body: &Value, verb: &str) -> Result<(String, u32), String> {
    let (code, answer) = client.request("POST", path, Some(body))?;
    if code != 201 {
        let why = answer
            .get("error")
            .and_then(Value::as_str)
            .map_or_else(|| answer.to_string(), str::to_owned);
        return Err(format!("POST {path} answered {code}: {why}"));
    }
    let session_id = answer
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("POST {path} answered no session id"))?
        .to_owned();
    let deadline = Instant::now() + DAEMON_TIMEOUT;
    let mut wrapper = wrapper_pid(&answer);
    loop {
        if wrapper.is_none() {
            wrapper = client
                .request("GET", &format!("/api/sessions/{session_id}"), None)
                .ok()
                .and_then(|(_, view)| wrapper_pid(&view));
        }
        if let Some(pid) = wrapper.and_then(|w| daemon_under(w, verb)) {
            return Ok((session_id, pid));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "session {session_id} started but no `fw {verb}` ran in it within {}s",
                DAEMON_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wrapper_pid(view: &Value) -> Option<u32> {
    view.get("live")?
        .get("pid")?
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
}

/// The `fw <verb>` daemon whose parent is `wrapper`.
fn daemon_under(wrapper: u32, verb: &str) -> Option<u32> {
    drain::scan(drain::current_uid())
        .into_iter()
        .find(|p| p.ppid == wrapper && drain::role(&p.argv) == Role::Daemon(verb.to_owned()))
        .map(|p| p.pid)
}

/// The note a process that ran with `name` set gets.
pub fn secret_note(name: &str) -> String {
    format!("ran with {name} from an unknown source; set it before restarting")
}

/// `{pid, cwd, command, notes}` for `process`: `cd <cwd> && <install_fw>
/// <args after argv[0]>`, shell-quoted; `extra` first among the notes.
pub fn manual_entry(process: &Drainable, install_fw: &Path, extra: Option<String>) -> Value {
    let cwd = process.cwd_display();
    let fw = install_fw.display().to_string();
    let mut argv: Vec<&str> = vec![fw.as_str()];
    argv.extend(process.argv.iter().skip(1).map(String::as_str));
    let command = format!("cd {} && {}", shell_quote(&cwd), shell_join(argv));
    let mut notes: Vec<String> = extra.into_iter().collect();
    notes.extend(process.secret_env.iter().map(|name| secret_note(name)));
    json!({
        "pid": process.pid,
        "cwd": cwd,
        "command": command,
        "notes": notes,
    })
}

/// `argv` as one POSIX shell command line.
pub fn shell_join<'a>(argv: impl IntoIterator<Item = &'a str>) -> String {
    argv.into_iter()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `word` quoted for a POSIX shell: bare when it is only safe characters,
/// else single-quoted (`'` as `'\''`).
pub fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn terminal(argv: &[&str], cwd: &str, secret_env: Vec<&'static str>) -> Drainable {
        Drainable {
            pid: 42,
            start: 1,
            kind: Kind::Terminal,
            verb: "worker".to_owned(),
            cwd: Some(PathBuf::from(cwd)),
            project_id: None,
            session_id: None,
            session: None,
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
            secret_env,
        }
    }

    #[test]
    fn words_are_quoted_only_when_needed() {
        assert_eq!(shell_quote("--poll-interval=3"), "--poll-interval=3");
        assert_eq!(shell_quote("/a/b-c_d.e"), "/a/b-c_d.e");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME;rm"), "'$HOME;rm'");
    }

    #[test]
    fn a_manual_command_reruns_the_original_args_on_the_installed_fw() {
        let process = terminal(
            &[
                "/old/fw",
                "worker",
                "--poll-interval",
                "7",
                "--label",
                "a b",
                "",
            ],
            "/repo dir",
            vec![],
        );
        let entry = manual_entry(&process, Path::new("/inst/fw"), None);
        assert_eq!(
            entry["command"],
            "cd '/repo dir' && /inst/fw worker --poll-interval 7 --label 'a b' ''"
        );
        assert_eq!(entry["pid"], 42);
        assert_eq!(entry["cwd"], "/repo dir");
        assert_eq!(entry["notes"], json!([]));
    }

    #[test]
    fn credential_variables_are_named_never_valued() {
        let process = terminal(
            &["fw", "worker"],
            "/r",
            vec!["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        );
        let entry = manual_entry(&process, Path::new("/i/fw"), Some("first".to_owned()));
        assert_eq!(
            entry["notes"],
            json!([
                "first",
                "ran with CLAUDE_CODE_OAUTH_TOKEN from an unknown source; set it before restarting",
                "ran with ANTHROPIC_API_KEY from an unknown source; set it before restarting",
            ])
        );
        assert_eq!(entry["command"], "cd /r && /i/fw worker");
    }

    #[test]
    fn without_a_hub_every_process_is_left_to_the_operator() {
        let mut hub_worker = terminal(&["fw", "worker", "--task=a"], "/r", vec![]);
        hub_worker.kind = Kind::HubWorker;
        hub_worker.session = Some(drain::HubSession {
            id: "s1".to_owned(),
            marker: flywheel_hub::flywheel::WORKER_MARKER,
            workspace_id: Some("w1".to_owned()),
            folder: Some(PathBuf::from("/r")),
            command: vec![],
        });
        let stopped = [terminal(&["fw", "triage"], "/t", vec![]), hub_worker];
        let out = restart(&stopped, None, Path::new("/i/fw"), "no hub");
        assert!(out.restarted.is_empty());
        assert!(!out.attempted());
        assert_eq!(out.manual.len(), 2);
        assert_eq!(out.manual[1]["notes"], json!(["no hub"]));
        assert_eq!(out.manual[1]["command"], "cd /r && /i/fw worker --task=a");
        assert!(out.detail().contains("left to the operator"));
    }
}
