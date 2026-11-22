//! Rolling browser logins through the real binary: a login made before
//! expiries slid (backdated in `logins.json`) renews on its next use --
//! the expiry persisted and the cookie re-sent -- at most once per
//! interval; the renewed expiry survives `fw hub upgrade` (a live
//! handoff) and a restart; rotating the token still ends it at once.

#![cfg(target_os = "linux")]

use flywheel_hub::auth::{
    DEFAULT_LOGIN_DAYS, LOGIN_RENEW_INTERVAL_SECS, LOGINS_FILE, login_lifetime_secs, unix_now,
};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

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
        Self {
            _dir: dir,
            root,
            state,
        }
    }

    fn fw(&self, args: &[&str]) -> Output {
        let mut argv: Vec<&str> = vec!["hub"];
        argv.extend_from_slice(args);
        argv.extend(["--state-dir", self.state.to_str().unwrap()]);
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            .env_remove("XDG_STATE_HOME")
            .env_remove("FLYWHEEL_HOME")
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

    fn port(&self) -> u16 {
        let status: Value = serde_json::from_str(&self.ok(&["status", "--json"])).unwrap();
        let listen = status["listen"].as_str().unwrap().to_owned();
        listen.rsplit(':').next().unwrap().parse().unwrap()
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.state.join("token"))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn logins(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.state.join(LOGINS_FILE)).unwrap()).unwrap()
    }

    /// The only login's expiry in `logins.json`.
    fn expires(&self) -> u64 {
        let logins = self.logins();
        let list = logins["logins"].as_array().unwrap();
        assert_eq!(list.len(), 1, "{logins}");
        list[0]["expires"].as_u64().unwrap()
    }

    /// Rewrite the only login's expiry (the hub must be stopped).
    fn set_expires(&self, expires: u64) {
        let mut logins = self.logins();
        logins["logins"][0]["expires"] = expires.into();
        std::fs::write(
            self.state.join(LOGINS_FILE),
            serde_json::to_vec(&logins).unwrap(),
        )
        .unwrap();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.fw(&["stop", "--force", "--timeout", "10"]);
    }
}

/// `GET path` with `cookie`: the status code and every `Set-Cookie`.
fn get(port: u16, path: &str, cookie: Option<&str>) -> (u16, Vec<String>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let cookie = cookie.map_or(String::new(), |c| format!("Cookie: {c}\r\n"));
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);
    let head = response.split("\r\n\r\n").next().unwrap_or("");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let cookies = head
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie")
                .then(|| value.trim().to_owned())
        })
        .collect();
    (status, cookies)
}

fn max_age(set_cookie: &str) -> u64 {
    set_cookie
        .split(';')
        .find_map(|part| part.trim().strip_prefix("Max-Age="))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no Max-Age in {set_cookie}"))
}

fn near(actual: u64, expected: u64) -> bool {
    actual.abs_diff(expected) <= 120
}

#[test]
fn a_used_login_rolls_across_upgrade_restart_and_dies_with_rotation() {
    let env = Env::new();
    let started = env.fw(&["start", "--port", "0"]);
    assert!(started.status.success(), "{started:?}");
    let port = env.port();
    let (status, cookies) = get(port, &format!("/login?token={}", env.token()), None);
    assert_eq!(status, 200);
    let lifetime = login_lifetime_secs(DEFAULT_LOGIN_DAYS);
    assert!(near(max_age(&cookies[0]), lifetime), "{cookies:?}");
    let cookie = cookies[0].split(';').next().unwrap().to_owned();

    // Within the interval a use writes nothing and sends no cookie.
    let issued = env.expires();
    let (status, cookies) = get(port, "/api/sessions", Some(&cookie));
    assert_eq!(status, 200);
    assert!(cookies.is_empty(), "{cookies:?}");
    assert_eq!(env.expires(), issued);

    // A login made 25 days ago (before expiries slid): 5 days left.
    env.ok(&["stop", "--timeout", "10"]);
    let old = unix_now() + 5 * 24 * 60 * 60;
    env.set_expires(old);
    env.ok(&["start", "--port", "0"]);
    let port = env.port();
    let (status, cookies) = get(port, "/api/sessions", Some(&cookie));
    assert_eq!(status, 200);
    assert_eq!(cookies.len(), 1, "renewed: the cookie follows");
    assert!(near(max_age(&cookies[0]), lifetime), "{cookies:?}");
    let renewed = env.expires();
    assert!(near(renewed, unix_now() + lifetime), "{renewed}");
    assert!(renewed >= old + LOGIN_RENEW_INTERVAL_SECS);

    // Rate limited: the next use does not write again.
    let (status, cookies) = get(port, "/api/sessions", Some(&cookie));
    assert_eq!(status, 200);
    assert!(cookies.is_empty(), "{cookies:?}");
    assert_eq!(env.expires(), renewed);
    // A page load re-sends the cookie with what is left (no write).
    let (status, cookies) = get(port, "/", Some(&cookie));
    assert_eq!(status, 200);
    assert_eq!(cookies.len(), 1, "{cookies:?}");
    assert!(near(max_age(&cookies[0]), lifetime), "{cookies:?}");
    assert_eq!(env.expires(), renewed);

    // A live handoff: the new process has the renewed expiry (a stale one
    // would renew again here).
    let upgraded = env.fw(&["upgrade", "--binary", env!("CARGO_BIN_EXE_fw")]);
    assert!(
        upgraded.status.success(),
        "{}{}",
        String::from_utf8_lossy(&upgraded.stdout),
        String::from_utf8_lossy(&upgraded.stderr)
    );
    let (status, cookies) = get(port, "/api/sessions", Some(&cookie));
    assert_eq!(status, 200);
    assert!(cookies.is_empty(), "{cookies:?}");
    assert_eq!(env.expires(), renewed);

    // A restart keeps it too.
    env.ok(&["stop", "--timeout", "10"]);
    env.ok(&["start", "--port", "0"]);
    let port = env.port();
    let (status, cookies) = get(port, "/api/sessions", Some(&cookie));
    assert_eq!(status, 200);
    assert!(cookies.is_empty(), "{cookies:?}");
    assert_eq!(env.expires(), renewed);

    // Rotating the token ends the renewed login at once.
    env.ok(&["token", "--rotate"]);
    let (status, _) = get(port, "/api/sessions", Some(&cookie));
    assert_eq!(status, 401);
    assert!(env.logins()["logins"].as_array().unwrap().is_empty());
}

#[test]
fn an_unused_login_still_expires_and_login_days_sets_the_lifetime() {
    let env = Env::new();
    let started = env.fw(&["start", "--port", "0"]);
    assert!(started.status.success(), "{started:?}");
    let port = env.port();
    let login = |port: u16| {
        let (status, cookies) = get(port, &format!("/login?token={}", env.token()), None);
        assert_eq!(status, 200);
        cookies[0].split(';').next().unwrap().to_owned()
    };
    let cookie = login(port);
    env.ok(&["stop", "--timeout", "10"]);
    // Unused past its lifetime: expired, never renewed.
    env.set_expires(unix_now() - 1);
    env.ok(&["start", "--port", "0"]);
    let port = env.port();
    let (status, cookies) = get(port, "/api/sessions", Some(&cookie));
    assert_eq!(status, 401);
    assert!(cookies.is_empty(), "{cookies:?}");

    // `server.login_days` sets the lifetime of new logins.
    std::fs::write(env.state.join("hub.toml"), "[server]\nlogin_days = 2\n").unwrap();
    let config: Value = serde_json::from_str(&env.ok(&["config", "show", "--json"])).unwrap();
    let text = config.to_string();
    assert!(text.contains("server.login_days"), "{text}");
    // Live: re-read within a second.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (status, cookies) = get(port, &format!("/login?token={}", env.token()), None);
        assert_eq!(status, 200);
        if near(max_age(&cookies[0]), login_lifetime_secs(2)) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{cookies:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}
