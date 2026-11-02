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

    fn command(&self, args: &[&str]) -> Command {
        // Right after the verb: anything after `--` is a session's argv.
        let mut argv: Vec<&str> = vec!["hub", args[0], "--state-dir", self.state.to_str().unwrap()];
        argv.extend_from_slice(&args[1..]);
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        command
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            // Scripted `claude` / `codex` stand-ins (see `fake_agents`).
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("FW_BIN", env!("CARGO_BIN_EXE_fw"))
            .env("PS1", "$ ")
            .env_remove("XDG_STATE_HOME");
        command
    }

    fn fw(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Run with `input` on stdin; require exit 0.
    fn ok_with_stdin(&self, args: &[&str], input: &str) {
        use std::io::Write;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "fw hub {args:?}: {status:?}");
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

/// Scripted `claude` and `codex` on the hub's PATH. Each launch appends
/// its cwd, report secret, and argv to `<root>/<name>.log` (`---` between
/// launches), then reads its terminal. A fresh `claude --session-id <id>`
/// files a transcript for `<id>` where Claude Code would (unless its repo
/// has a `.no-transcript`); a fresh `codex` writes a rollout and reports its
/// thread id the way Codex's notifier does.
fn fake_agents(env: &Env) {
    use std::os::unix::fs::PermissionsExt;
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = |name: &str| {
        format!(
            "{{ echo \"cwd=$PWD\"; echo \"secret=$FLYWHEEL_HUB_REPORT_SECRET\"; \
             printf 'arg=%s\\n' \"$@\"; echo ---; }} >> '{}'\n",
            env.root.join(format!("{name}.log")).display()
        )
    };
    let claude = format!(
        "#!/bin/sh\n{}\
         if [ \"$1\" = --session-id ] && [ ! -e .no-transcript ]; then\n\
         \x20 dir=\"$CLAUDE_CONFIG_DIR/projects/$(printf %s \"$PWD\" | sed 's/[^A-Za-z0-9]/-/g')\"\n\
         \x20 mkdir -p \"$dir\" && echo '{{}}' > \"$dir/$2.jsonl\"\n\
         fi\n\
         echo claude-ready\nexec cat\n",
        log("claude")
    );
    let codex = format!(
        "#!/bin/sh\n{}\
         case \" $* \" in *\" resume \"*) ;; *)\n\
         \x20 id=0199a213-81c0-7800-8aa1-bbab2a035a53\n\
         \x20 mkdir -p \"$CODEX_HOME/sessions/2026/11/02\"\n\
         \x20 echo '{{}}' > \"$CODEX_HOME/sessions/2026/11/02/rollout-2026-11-02T10-00-00-$id.jsonl\"\n\
         \x20 \"$FW_BIN\" hub report --source codex --event notify \
         \"{{\\\"type\\\":\\\"agent-turn-complete\\\",\\\"thread-id\\\":\\\"$id\\\"}}\" ;;\n\
         esac\n\
         echo codex-ready\nexec cat\n",
        log("codex")
    );
    for (name, script) in [("claude", claude), ("codex", codex)] {
        let path = bin.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Each launch of fake agent `name` so far: its `cwd=`, `secret=`, and
/// `arg=` lines.
fn launches(env: &Env, name: &str) -> Vec<Vec<String>> {
    let log = std::fs::read_to_string(env.root.join(format!("{name}.log"))).unwrap_or_default();
    log.split("---\n")
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.lines().map(str::to_owned).collect())
        .collect()
}

fn args_of(launch: &[String]) -> Vec<&str> {
    launch
        .iter()
        .filter_map(|l| l.strip_prefix("arg="))
        .collect()
}

fn field<'a>(launch: &'a [String], key: &str) -> &'a str {
    launch
        .iter()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_default()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn restore_resumes_agent_conversations_natively() {
    let env = Env::new();
    fake_agents(&env);
    let repo = env.repo("repo");
    let fresh = env.repo("fresh");
    std::fs::write(fresh.join(".no-transcript"), "").unwrap();
    env.ok(&["start", "--port", "0"]);
    let new = |ws: &str, repo: &Path, kind: &str, name: &str| {
        env.ok(&[
            "new",
            ws,
            "--repo",
            repo.to_str().unwrap(),
            "--create",
            "--kind",
            kind,
            "--name",
            name,
        ])
        .trim()
        .to_owned()
    };
    new("proj", &repo, "claude", "cc");
    new("proj", &repo, "codex", "cx");
    new("other", &fresh, "claude", "unsaved");
    env.read_until("cc", "claude-ready");
    env.read_until("unsaved", "claude-ready");
    env.read_until("cx", "codex-ready");
    let thread = "0199a213-81c0-7800-8aa1-bbab2a035a53";
    wait_for("codex's thread id", || {
        env.sessions()
            .iter()
            .any(|s| s["agent_session_id"] == thread)
    });
    let sessions = env.sessions();
    let by_name = |sessions: &[Value], name: &str| {
        sessions.iter().find(|s| s["name"] == name).unwrap().clone()
    };
    let conversation = by_name(&sessions, "cc")["agent_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let first_claude = launches(&env, "claude");
    let first_cc = first_claude
        .iter()
        .find(|l| field(l, "cwd=") == repo.to_str().unwrap())
        .unwrap()
        .clone();
    assert_eq!(
        &args_of(&first_cc)[..2],
        ["--session-id", conversation.as_str()]
    );

    env.ok(&["stop"]);
    let started = env.ok(&["start", "--port", "0"]);
    assert!(started.contains("restored 3 sessions"), "{started}");
    env.read_until("cc", "claude-ready");
    env.read_until("cx", "codex-ready");
    wait_for("the restored agents", || {
        launches(&env, "claude").len() == 3 && launches(&env, "codex").len() == 2
    });

    // Claude with a transcript: `--resume <id>` in its repo, with the
    // status hooks and a fresh per-spawn secret; still a claude session.
    let claude = launches(&env, "claude");
    let resumed = &claude[2];
    assert_eq!(field(resumed, "cwd="), repo.to_str().unwrap());
    let args = args_of(resumed);
    assert_eq!(&args[..2], ["--resume", conversation.as_str()], "{args:?}");
    assert_eq!(args[2], "--settings", "{args:?}");
    assert!(args[3].contains("\"report\""), "{args:?}");
    assert!(!field(resumed, "secret=").is_empty());
    assert_ne!(field(resumed, "secret="), field(&first_cc, "secret="));
    // Codex with a rollout: `codex -c notify=[...] resume <thread>`.
    let codex = launches(&env, "codex");
    let args = args_of(&codex[1]);
    assert_eq!(args[0], "-c", "{args:?}");
    assert!(args[1].starts_with("notify=["), "{args:?}");
    assert_eq!(&args[2..], ["resume", thread], "{args:?}");
    assert_eq!(field(&codex[1], "cwd="), repo.to_str().unwrap());

    let sessions = env.sessions();
    for name in ["cc", "cx"] {
        let s = by_name(&sessions, name);
        assert_eq!(s["state"], "running", "{s}");
        assert!(s.get("fallback_command").is_none(), "{s}");
    }
    assert_eq!(by_name(&sessions, "cc")["kind"], "claude");
    assert_eq!(by_name(&sessions, "cx")["kind"], "codex");
    // No transcript: a fresh shell in its place, the record still claude.
    let unsaved = by_name(&sessions, "unsaved");
    assert_eq!(unsaved["kind"], "claude", "{unsaved}");
    assert_eq!(unsaved["fallback_command"], serde_json::json!(["/bin/sh"]));
    let logs = env.ok(&["logs", "-n", "0"]);
    assert!(
        logs.contains("restored session \"cc\"") && logs.contains("resumed its conversation"),
        "{logs}"
    );

    // --no-agent-resume: agents come back as shells, losslessly...
    env.ok(&["stop"]);
    env.ok(&["start", "--port", "0", "--no-agent-resume"]);
    let sessions = env.sessions();
    for name in ["cc", "cx"] {
        let s = by_name(&sessions, name);
        assert_eq!(s["state"], "running", "{s}");
        assert_eq!(s["fallback_command"], serde_json::json!(["/bin/sh"]), "{s}");
    }
    assert_eq!(launches(&env, "claude").len(), 3);
    assert_eq!(launches(&env, "codex").len(), 2);
    // ...so a later start resumes them again.
    env.ok(&["stop"]);
    env.ok(&["start", "--port", "0"]);
    wait_for("the resumed claude", || launches(&env, "claude").len() == 4);
    let claude = launches(&env, "claude");
    assert_eq!(
        &args_of(&claude[3])[..2],
        ["--resume", conversation.as_str()]
    );
    let sessions = env.sessions();
    assert!(by_name(&sessions, "cc").get("fallback_command").is_none());
    env.ok(&["stop"]);
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
    // A taken name is refused: it would no longer address either.
    let output = env.fw(&["new", "proj", "--name", "beta"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
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
    // Piped text with a final newline runs (the newline is the Enter).
    env.ok_with_stdin(&["send", "beta", "-"], "echo piped-ran\n");
    env.read_until("beta", "\npiped-ran");
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
    // The script session keeps what it is; a shell runs in its place.
    assert_eq!(
        sessions[2]["command"],
        serde_json::json!(["/bin/sh", "-c", "echo ran-once; exec sh"])
    );
    assert_eq!(
        sessions[2]["fallback_command"],
        serde_json::json!(["/bin/sh"])
    );
    // ls shows where alpha's shell is, not just its repo.
    let ls = env.ok(&["ls"]);
    assert!(ls.contains(&deeper.display().to_string()), "{ls}");
    assert!(ls.contains("not restored"), "{ls}");

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

    // --no-restore leaves them stopped, on later starts too.
    env.ok(&["start", "--port", "0", "--no-restore"]);
    assert!(env.sessions().iter().all(|s| s["state"] != "running"));
    env.ok(&["stop"]);
    let started = env.ok(&["start", "--port", "0"]);
    assert!(!started.contains("restored"), "{started}");
    assert!(env.sessions().iter().all(|s| s["state"] != "running"));
    // Relaunched by hand.
    let relaunched = env.ok(&["relaunch", "alpha"]);
    assert!(
        relaunched.contains("relaunched session \"alpha\""),
        "{relaunched}"
    );
    env.ok(&["send", "alpha", "echo back-again", "--enter"]);
    env.read_until("alpha", "\nback-again");
    env.ok(&["stop"]);
}

fn hub_pid(env: &Env) -> i32 {
    let status: Value = serde_json::from_str(&env.ok(&["status", "--json"])).unwrap();
    i32::try_from(status["pid"].as_i64().unwrap()).unwrap()
}

#[test]
fn a_crash_is_reported_and_a_wedged_hub_does_not_stall_status_or_stop() {
    let env = Env::new();
    let repo = env.repo("repo");
    env.ok(&["start", "--port", "0"]);
    env.ok(&[
        "new",
        "proj",
        "--repo",
        repo.to_str().unwrap(),
        "--create",
        "--name",
        "alpha",
    ]);
    // Killed outright: its sessions are not restored, and start says so.
    let pid = hub_pid(&env);
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while status_code(&env) != 3 {
        assert!(Instant::now() < deadline, "the killed hub still runs");
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = env.fw(&["start", "--port", "0"]);
    assert!(output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("session \"alpha\"")
            && err.contains("not shut down cleanly")
            && err.contains("fw hub relaunch"),
        "{err}"
    );
    let ls = env.ok(&["ls"]);
    assert!(ls.contains("not restored"), "{ls}");

    // Alive but not answering: status and stop are bounded by their own
    // timeouts, not the scripting verbs' 30s.
    let pid = hub_pid(&env);
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let started = Instant::now();
    let output = env.fw(&["status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let started = Instant::now();
    let output = env.fw(&["stop", "--timeout", "1"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("did not stop within 1s"));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    // The queued SIGTERM lands once it runs again: a clean stop.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let deadline = Instant::now() + Duration::from_secs(15);
    while status_code(&env) != 3 {
        assert!(Instant::now() < deadline, "the hub never stopped");
        std::thread::sleep(Duration::from_millis(100));
    }
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
    // Its output is the journal: no login code there.
    assert!(unit.contains("\"--no-login-code\""), "{unit}");
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
