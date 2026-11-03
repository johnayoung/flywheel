//! A flood of slow unauthenticated clients against a real detached hub
//! (descriptor limit 256, so 64 main slots and 16 unauthenticated ones):
//! clients trickling request heads and login bodies never starve the
//! operator -- authenticated requests, `fw hub upgrade`, and `fw hub stop`
//! all still go through promptly -- and one client address (as a trusted
//! proxy's `X-Forwarded-For` names it) holds at most its share.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Slow request heads in the flood (more than the 64 main slots).
const SLOW_HEADS: usize = 100;
/// Slow login bodies in it.
const SLOW_BODIES: usize = 6;
/// Slow heads beside the bodies when the point is requests in flight: few
/// enough that the unauthenticated pool keeps every body.
const FEW_SLOW_HEADS: usize = 8;
/// "Promptly": well under the 10 s header deadline the flood exploited.
const PROMPT: Duration = Duration::from_secs(3);

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
        std::fs::create_dir_all(root.join("bin")).unwrap();
        // A `claude` first on PATH that is never the real one.
        let claude = root.join("bin/claude");
        std::fs::write(&claude, "#!/bin/sh\nexec cat\n").unwrap();
        let status = Command::new("chmod")
            .arg("755")
            .arg(&claude)
            .status()
            .unwrap();
        assert!(status.success());
        Self {
            _dir: dir,
            root,
            state,
        }
    }

    /// `fw hub <args>`; `limit`: run it under `ulimit -n` (a started hub
    /// keeps it).
    fn command(&self, args: &[&str], limit: Option<u32>) -> Command {
        let mut argv: Vec<String> = vec![
            "hub".into(),
            args[0].into(),
            "--state-dir".into(),
            self.state.to_str().unwrap().into(),
        ];
        argv.extend(args[1..].iter().map(|a| (*a).to_owned()));
        let mut command = match limit {
            Some(n) => {
                let mut sh = Command::new("/bin/sh");
                sh.arg("-c")
                    .arg(format!("ulimit -n {n} && exec \"$0\" \"$@\""))
                    .arg(env!("CARGO_BIN_EXE_fw"));
                sh
            }
            None => Command::new(env!("CARGO_BIN_EXE_fw")),
        };
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        command
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("INVOCATION_ID")
            .env_remove("FW_HUB_TOKEN");
        command
    }

    fn fw(&self, args: &[&str]) -> Output {
        self.command(args, None).output().unwrap()
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

    /// Start a hub with 256 descriptors; its port.
    fn start(&self) -> u16 {
        let started = self
            .command(&["start", "--port", "0", "--no-flywheel"], Some(256))
            .output()
            .unwrap();
        assert!(started.status.success(), "{started:?}");
        let status: serde_json::Value =
            serde_json::from_str(&self.ok(&["status", "--json"])).unwrap();
        let listen = status["listen"].as_str().unwrap().to_owned();
        listen.rsplit(':').next().unwrap().parse().unwrap()
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.state.join("token"))
            .unwrap()
            .trim()
            .to_owned()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.fw(&["stop", "--force", "--timeout", "10"]);
    }
}

/// Slow clients, trickling on a thread until dropped.
struct Flood {
    stop: Arc<AtomicBool>,
    trickler: Option<JoinHandle<()>>,
}

impl Flood {
    /// `bodies` connections sending a login form a byte at a time, then
    /// `heads` sending a request head a header line at a time, each half a
    /// second apart; `extra` goes into every head (e.g. a forwarded-for).
    fn start(port: u16, heads: usize, bodies: usize, extra: &str) -> Self {
        let mut streams = Vec::new();
        for _ in 0..bodies {
            let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
                continue;
            };
            let head = format!(
                "POST /login HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Content-Type: \
                 application/x-www-form-urlencoded\r\nContent-Length: 1000\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            streams.push((stream, b"a".to_vec()));
        }
        for _ in 0..heads {
            let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
                continue;
            };
            let head = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}");
            let _ = stream.write_all(head.as_bytes());
            streams.push((stream, b"X-Slow: 1\r\n".to_vec()));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let trickler = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                for (stream, drip) in &mut streams {
                    let _ = stream.write_all(drip);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        // Let the hub accept them all before the operator connects.
        std::thread::sleep(Duration::from_millis(500));
        Self {
            stop,
            trickler: Some(trickler),
        }
    }
}

impl Drop for Flood {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(trickler) = self.trickler.take() {
            let _ = trickler.join();
        }
    }
}

/// Send `request` on a fresh connection; the status line ("" when none
/// came within `wait`).
fn status_line(port: u16, request: &str, wait: Duration) -> String {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return String::new();
    };
    let _ = stream.set_read_timeout(Some(wait));
    if stream.write_all(request.as_bytes()).is_err() {
        return String::new();
    }
    let mut answer = Vec::new();
    let mut buf = [0u8; 4096];
    while !answer.windows(2).any(|w| w == b"\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => answer.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&answer)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn authenticated_get(port: u16, token: &str) -> String {
    status_line(
        port,
        &format!(
            "GET /api/sessions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer \
             {token}\r\nConnection: close\r\n\r\n"
        ),
        Duration::from_secs(15),
    )
}

#[test]
fn a_slow_unauthenticated_flood_does_not_starve_the_operator() {
    let env = Env::new();
    let port = env.start();
    let token = env.token();
    let _flood = Flood::start(port, SLOW_HEADS, SLOW_BODIES, "");

    let started = Instant::now();
    let status = authenticated_get(port, &token);
    let took = started.elapsed();
    assert!(status.contains(" 200 "), "{status:?} after {took:?}");
    assert!(took < PROMPT, "an authenticated request took {took:?}");

    // The CLI too (it authenticates with the state dir's token).
    let started = Instant::now();
    env.ok(&["ls", "--json"]);
    let took = started.elapsed();
    assert!(took < PROMPT, "`fw hub ls` took {took:?}");

    // A fresh unauthenticated request with a prompt head is still served.
    let health = status_line(
        port,
        &format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        PROMPT,
    );
    assert!(health.contains(" 200 "), "{health:?}");
}

#[test]
fn upgrade_hands_off_promptly_under_a_slow_flood() {
    let env = Env::new();
    let port = env.start();
    let old = env.ok(&["status", "--json"]);
    let _flood = Flood::start(port, FEW_SLOW_HEADS, SLOW_BODIES, "");

    let started = Instant::now();
    let upgraded = env.fw(&["upgrade", "--binary", env!("CARGO_BIN_EXE_fw")]);
    let took = started.elapsed();
    assert!(
        upgraded.status.success(),
        "{}{}",
        String::from_utf8_lossy(&upgraded.stdout),
        String::from_utf8_lossy(&upgraded.stderr)
    );
    assert!(took < PROMPT, "`fw hub upgrade` took {took:?}");
    let new = env.ok(&["status", "--json"]);
    assert_ne!(old, new, "a new process serves");
    let status = authenticated_get(port, &env.token());
    assert!(status.contains(" 200 "), "{status:?}");
}

#[test]
fn stop_is_prompt_under_a_slow_flood() {
    let env = Env::new();
    let port = env.start();
    let _flood = Flood::start(port, FEW_SLOW_HEADS, SLOW_BODIES, "");

    let started = Instant::now();
    let stopped = env.fw(&["stop", "--timeout", "30"]);
    let took = started.elapsed();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(took < PROMPT, "`fw hub stop` took {took:?}");
}

#[test]
fn one_forwarded_client_holds_at_most_its_share() {
    let env = Env::new();
    let port = env.start();
    // Through a proxy on this machine (loopback is trusted): eight slow
    // logins from one client address fill its share.
    let _flood = Flood::start(port, 0, 8, "X-Forwarded-For: 198.51.100.7\r\n");
    let started = Instant::now();
    let refused = status_line(
        port,
        &format!(
            "POST /login HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Forwarded-For: \
             198.51.100.7\r\nContent-Length: 1000\r\n\r\n"
        ),
        Duration::from_secs(15),
    );
    assert!(refused.contains(" 429 "), "{refused:?}");
    assert!(started.elapsed() < PROMPT, "{:?}", started.elapsed());
    // Another client behind the same proxy is served.
    let other = status_line(
        port,
        &format!(
            "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Forwarded-For: \
             198.51.100.8\r\n\r\n"
        ),
        PROMPT,
    );
    assert!(other.contains(" 200 "), "{other:?}");
}
