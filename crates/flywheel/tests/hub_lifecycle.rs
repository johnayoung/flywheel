//! `fw hub start | status | stop | logs` and the scripting verbs (`new`,
//! `send`, `read`, `ls`, `kill`) end to end through the real binary: a
//! detached hub on 127.0.0.1:0 with real PTYs running `/bin/sh`, stopped
//! and started again to restore its shells where they were.

#![cfg(unix)]

use serde_json::Value;
use std::path::{Path, PathBuf};
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
        Self {
            _dir: dir,
            root,
            state,
        }
    }

    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        repo
    }

    fn fw(&self, args: &[&str]) -> Output {
        // Right after the verb: anything after `--` is a session's argv.
        let mut argv: Vec<&str> = vec!["hub", args[0], "--state-dir", self.state.to_str().unwrap()];
        argv.extend_from_slice(&args[1..]);
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            .env("PS1", "$ ")
            .env_remove("XDG_STATE_HOME")
            .output()
            .unwrap()
    }

    /// Run and require exit 0; stdout.
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

    /// Poll `fw hub read` until the screen contains `needle`.
    fn read_until(&self, session: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = self.ok(&["read", session]);
            if screen.contains(needle) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "{needle:?} never appeared in {session}:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn sessions(&self) -> Vec<Value> {
        let listed: Value = serde_json::from_str(&self.ok(&["ls", "--json"])).unwrap();
        listed["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|w| w["sessions"].as_array().unwrap().clone())
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Never leave a daemon behind, whatever the test did.
        let _ = self.fw(&["stop", "--force", "--timeout", "10"]);
    }
}

fn status_code(env: &Env) -> i32 {
    env.fw(&["status"]).status.code().unwrap()
}

#[test]
fn start_script_stop_and_restore_shells_where_they_were() {
    let env = Env::new();
    let repo = env.repo("repo");
    let gone = env.repo("gone");
    let deeper = repo.join("sub").join("dir");
    std::fs::create_dir_all(&deeper).unwrap();

    assert_eq!(status_code(&env), 3, "not running yet");
    let started = env.ok(&["start", "--port", "0"]);
    assert!(started.contains("flywheel hub started (pid "), "{started}");
    assert!(started.contains("/login?code="), "{started}");
    assert_eq!(status_code(&env), 0);
    // Idempotent.
    let again = env.ok(&["start", "--port", "0"]);
    assert!(again.contains("already running"), "{again}");

    // Sessions by CLI: a workspace created on the fly, names, ids.
    let alpha = env
        .ok(&[
            "new",
            "proj",
            "--repo",
            repo.to_str().unwrap(),
            "--create",
            "--name",
            "alpha",
        ])
        .trim()
        .to_owned();
    assert_eq!(alpha.len(), 32, "{alpha}");
    let beta = env.ok(&["new", "proj", "--name", "beta"]).trim().to_owned();
    let doomed = env
        .ok(&[
            "new",
            "elsewhere",
            "--repo",
            gone.to_str().unwrap(),
            "--create",
            "--name",
            "doomed",
        ])
        .trim()
        .to_owned();
    // Not a shell `fw hub` would relaunch as is: it comes back as a fresh
    // shell instead of rerunning the command.
    let script = env
        .ok(&[
            "new",
            "proj",
            "--name",
            "script",
            "--",
            "/bin/sh",
            "-c",
            "echo ran-once; exec sh",
        ])
        .trim()
        .to_owned();
    env.read_until("script", "ran-once");

    // send / read by name and by id prefix; a multi-line send arrives as
    // typed lines.
    env.ok(&[
        "send",
        "alpha",
        &format!("cd {}", deeper.display()),
        "--enter",
    ]);
    env.ok(&["send", &beta[..6], "echo one\necho two", "--enter"]);
    env.read_until("beta", "two");
    env.ok(&["send", "alpha", "pwd", "--enter"]);
    env.read_until("alpha", &deeper.display().to_string());
    let recent = env.ok(&["read", "beta", "--source", "recent", "--lines", "50"]);
    assert!(
        recent.contains("one\n") && recent.contains("two\n"),
        "{recent}"
    );
    let screen: Value = serde_json::from_str(&env.ok(&["read", "alpha", "--json"])).unwrap();
    assert_eq!(screen["source"], "visible");

    // Ambiguity and misses are errors, not guesses.
    let output = env.fw(&["read", "nope"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no session named"));

    let status = env.ok(&["status"]);
    assert!(status.contains("workspaces: 2"), "{status}");
    assert!(status.contains("sessions:   4 (4 running)"), "{status}");
    let ls = env.ok(&["ls"]);
    assert!(ls.contains(&alpha[..8]) && ls.contains("running"), "{ls}");

    // Graceful stop: sessions recorded for restore, nothing left running.
    let stopped = env.ok(&["stop"]);
    assert!(
        stopped.contains("4 running sessions recorded for restore"),
        "{stopped}"
    );
    assert_eq!(status_code(&env), 3);
    assert!(!env.state.join("server.json").exists());
    std::fs::rename(&gone, env.root.join("moved")).unwrap();

    let started = env.ok(&["start", "--port", "0"]);
    assert!(
        started.contains("restored 3 sessions: \"alpha\", \"beta\", \"script\""),
        "{started}"
    );
    let sessions = env.sessions();
    let ids: Vec<&str> = sessions.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            alpha.as_str(),
            beta.as_str(),
            script.as_str(),
            doomed.as_str()
        ]
    );
    let doomed_record = &sessions[3];
    assert_eq!(doomed_record["state"], "stopped");
    assert!(
        doomed_record["restore_error"]
            .as_str()
            .unwrap()
            .contains("no longer exists"),
        "{doomed_record}"
    );
    assert_eq!(sessions[2]["command"], serde_json::json!(["/bin/sh"]));

    // The restored shell starts where alpha's shell had `cd`'d to.
    env.ok(&["send", "alpha", "pwd", "--enter"]);
    env.read_until("alpha", &deeper.display().to_string());
    env.ok(&["send", "script", "echo fresh-shell", "--enter"]);
    let fresh = env.read_until("script", "\nfresh-shell");
    assert!(!fresh.contains("ran-once"), "{fresh}");

    env.ok(&["kill", "beta"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.sessions()[1]["state"] != "exited" {
        assert!(Instant::now() < deadline, "beta never exited");
        std::thread::sleep(Duration::from_millis(100));
    }

    let logs = env.ok(&["logs", "-n", "0"]);
    assert!(logs.contains("restored session \"alpha\""), "{logs}");
    assert!(logs.contains("cannot restore session \"doomed\""), "{logs}");
    assert!(!logs.contains("/login?code="), "no login codes in the log");
    let token = std::fs::read_to_string(env.state.join("token")).unwrap();
    assert!(!logs.contains(token.trim()), "no token in the log");

    let stopped = env.ok(&["stop"]);
    assert!(stopped.contains("flywheel hub stopped"), "{stopped}");
    let output = env.fw(&["stop"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("no hub is running"));

    // --no-restore leaves them stopped.
    env.ok(&["start", "--port", "0", "--no-restore"]);
    assert!(env.sessions().iter().all(|s| s["state"] != "running"));
    env.ok(&["stop"]);
}

#[test]
fn a_hub_that_cannot_start_reports_why() {
    let env = Env::new();
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = blocker.local_addr().unwrap().port().to_string();
    let output = env.fw(&["start", "--port", &port]);
    assert_eq!(output.status.code(), Some(1));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("exited during startup") && err.contains("cannot listen on"),
        "{err}"
    );
    assert_eq!(status_code(&env), 3);
}

#[test]
fn install_service_prints_a_unit_running_serve() {
    let env = Env::new();
    let output = env.fw(&["install-service", "--print", "--port", "7777"]);
    assert!(output.status.success());
    let unit = String::from_utf8(output.stdout).unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_fw")).canonicalize().unwrap();
    assert!(
        unit.contains(&format!(
            "ExecStart=\"{}\" \"hub\" \"serve\" \"--bind\" \"127.0.0.1\" \"--port\" \"7777\"",
            exe.display()
        )),
        "{unit}"
    );
    assert!(unit.contains(&format!("\"--state-dir\" \"{}\"", env.state.display())));
    assert!(unit.contains("Environment=\"SHELL=/bin/sh\""), "{unit}");
    // Written under XDG_CONFIG_HOME, never enabled; refused twice.
    let config = env.root.join("config");
    let write = || {
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(["hub", "install-service", "--state-dir"])
            .arg(&env.state)
            .env("XDG_CONFIG_HOME", &config)
            .output()
            .unwrap()
    };
    let first = write();
    assert!(first.status.success(), "{first:?}");
    let path = config.join("systemd/user/flywheel-hub.service");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("KillMode=mixed")
    );
    assert!(
        String::from_utf8_lossy(&first.stdout)
            .contains("systemctl --user enable --now flywheel-hub")
    );
    let second = write();
    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stderr).contains("already exists"));
}
