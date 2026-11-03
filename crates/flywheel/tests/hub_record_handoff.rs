//! Recording and share links across a live handoff, through the real
//! binary: a session recorded by hub A keeps being recorded, in the same
//! file, by hub B after `fw hub upgrade` (no gap, times monotonic); a
//! share link minted on A still opens on B.

#![cfg(target_os = "linux")]

use flywheel_hub::record::parse_cast;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = root.join("state");
        std::fs::create_dir_all(root.join("repo")).unwrap();
        Self {
            _dir: dir,
            root,
            state,
        }
    }

    fn fw(&self, args: &[&str]) -> Output {
        // Last: subcommand verbs (`record start`) take it after theirs.
        let mut argv: Vec<&str> = vec!["hub"];
        argv.extend_from_slice(args);
        argv.extend(["--state-dir", self.state.to_str().unwrap()]);
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            .env("PS1", "$ ")
            .env_remove("XDG_STATE_HOME")
            .env_remove("INVOCATION_ID")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.fw(args);
        assert!(
            output.status.success(),
            "fw hub {args:?}: {:?}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    fn read_until(&self, session: &str, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if self.ok(&["read", session]).contains(needle) {
                return;
            }
            assert!(Instant::now() < deadline, "{needle:?} never appeared");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn port(&self) -> u16 {
        let listen = self.json(&["status", "--json"])["listen"]
            .as_str()
            .unwrap()
            .to_owned();
        listen.rsplit(':').next().unwrap().parse().unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.fw(&["stop", "--force", "--timeout", "10"]);
    }
}

/// `GET path` on the hub with no credential but the one in the path: the
/// status code.
fn get_status(port: u16, path: &str) -> u16 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

#[test]
fn a_recording_carries_on_in_the_same_file_across_an_upgrade() {
    let env = Env::new();
    let started = env.fw(&["start", "--port", "0"]);
    assert!(started.status.success(), "{started:?}");
    let repo = env.root.join("repo");
    let session = env
        .ok(&[
            "new",
            "--create",
            "--folder",
            repo.to_str().unwrap(),
            "--name",
            "taped",
            "ws",
        ])
        .trim()
        .to_owned();
    env.read_until(&session, "$");

    let said = env.ok(&["record", "start", "taped"]);
    assert!(said.starts_with("recording taped to "), "{said}");
    let shared = env.json(&[
        "share",
        "taped",
        "--expires",
        "30m",
        "--name",
        "pair",
        "--json",
    ]);
    let path = shared["path"].as_str().unwrap().to_owned();
    let port = env.port();
    assert_eq!(get_status(port, &format!("{path}/info")), 200);
    env.ok(&["send", "taped", "--enter", "echo before-$((1+1))"]);
    env.read_until(&session, "before-2");

    let upgraded = env.fw(&["upgrade", "--binary", env!("CARGO_BIN_EXE_fw")]);
    assert!(
        upgraded.status.success(),
        "{}{}",
        String::from_utf8_lossy(&upgraded.stdout),
        String::from_utf8_lossy(&upgraded.stderr)
    );
    // Still recording, to the same file, on the new hub.
    let listed = env.json(&["record", "ls", "--json"]);
    let recordings = listed["recordings"].as_array().unwrap();
    assert_eq!(recordings.len(), 1, "{listed}");
    assert_eq!(recordings[0]["active"], true, "{listed}");
    let name = recordings[0]["name"].as_str().unwrap().to_owned();
    env.ok(&["send", "taped", "--enter", "echo after-$((2+2))"]);
    env.read_until(&session, "after-4");
    // The share link minted on the old hub opens on the new one.
    assert_eq!(get_status(port, &format!("{path}/info")), 200);
    let listed = env.json(&["share", "ls", "--json"]);
    assert_eq!(listed["shares"][0]["name"], "pair", "{listed}");

    let said = env.ok(&["record", "stop", "taped"]);
    assert!(said.contains(&name), "{said}");
    let file = env.root.join("dl.cast");
    let got = env.fw(&["record", "get", &name, "-o", file.to_str().unwrap()]);
    assert!(got.status.success(), "{got:?}");
    let cast = parse_cast(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let output = cast.output();
    let before = output.find("before-2").expect("before the upgrade");
    let after = output.find("after-4").expect("after the upgrade");
    assert!(before < after);
    assert!(
        cast.events.windows(2).all(|w| w[0].0 <= w[1].0),
        "times stay monotonic across the handoff: {:?}",
        cast.events
    );
    // One header: the new hub appended to the file, it did not restart it.
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().filter(|l| l.starts_with('{')).count(), 1);
    // Getting it again refuses to overwrite.
    let again = env.fw(&["record", "get", &name, "-o", file.to_str().unwrap()]);
    assert!(!again.status.success());
    // Revoked through the CLI: the link answers 404.
    let id = shared["share"]["id"].as_str().unwrap();
    let said = env.ok(&["share", "revoke", id]);
    assert!(said.contains(id), "{said}");
    assert_eq!(get_status(port, &format!("{path}/info")), 404);
    env.ok(&["record", "rm", &name]);
    assert!(
        env.json(&["record", "ls", "--json"])["recordings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
