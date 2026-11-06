//! Hub session actions as audit rows, end to end through the real binary:
//! `FLYWHEEL_HOME`, `HOME`, and XDG point at a tempdir; a temp git repo
//! that is already a flywheel project (with a trusted `.flywheel.toml`) is
//! imported as a hub workspace, a shell session is opened and killed, the
//! hub is stopped, and `fw audit --project --json` in the repo lists the
//! session's `hub.session.start` and `hub.session.end` rows. A session in a
//! git repo flywheel never ran in records nothing and creates nothing.

#![cfg(unix)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["home", "fwhome", "xdg-state", "xdg-config"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self { _dir: dir, root }
    }

    fn fw(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(args)
            .current_dir(cwd)
            .env("FLYWHEEL_HOME", self.root.join("fwhome"))
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("SHELL", "/bin/sh")
            .env("PS1", "$ ")
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.fw(cwd, args);
        assert!(
            output.status.success(),
            "fw {args:?}: {:?}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn git_repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("README"), "x\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        repo
    }

    fn workspace_file(&self, name: &str, folder: &Path) -> PathBuf {
        let file = self.root.join(format!("{name}.code-workspace"));
        let body = serde_json::json!({ "folders": [{ "path": folder }] });
        std::fs::write(&file, body.to_string()).unwrap();
        file
    }

    /// Poll `fw hub ls --json` until session `id` is no longer running.
    fn wait_exited(&self, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let listed: Value =
                serde_json::from_str(&self.ok(&self.root, &["hub", "ls", "--json"])).unwrap();
            let state = listed["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|w| w["sessions"].as_array().unwrap().clone())
                .find(|s| s["id"] == id)
                .map(|s| s["state"].as_str().unwrap_or_default().to_owned());
            if state.as_deref() != Some("running") {
                return;
            }
            assert!(Instant::now() < deadline, "session {id} never exited");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn audit_rows(sandbox: &Sandbox, repo: &Path) -> Vec<Value> {
    sandbox
        .ok(repo, &["audit", "--project", "--json"])
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn hub_sessions_are_audited_in_their_project_and_nowhere_else() {
    let sandbox = Sandbox::new();
    let project = sandbox.git_repo("proj");
    std::fs::write(project.join(".flywheel.toml"), "land = \"merge\"\n").unwrap();
    sandbox.ok(&project, &["status"]);
    sandbox.ok(&project, &["trust"]);
    // A git repo flywheel never ran in.
    let stranger = sandbox.git_repo("stranger");

    let port = free_port().to_string();
    sandbox.ok(&sandbox.root, &["hub", "start", "--port", &port]);
    let run = || -> (String, String, String) {
        let file = sandbox.workspace_file("proj", &project);
        sandbox.ok(&sandbox.root, &["hub", "import", file.to_str().unwrap()]);
        let file = sandbox.workspace_file("stranger", &stranger);
        sandbox.ok(&sandbox.root, &["hub", "import", file.to_str().unwrap()]);
        let id = sandbox.ok(&sandbox.root, &["hub", "new", "proj", "--", "sh"]);
        let id = id.trim().to_owned();
        let other = sandbox.ok(&sandbox.root, &["hub", "new", "stranger", "--", "sh"]);
        let other = other.trim().to_owned();
        // Left running: `fw hub stop` hangs it up.
        let left = sandbox.ok(&sandbox.root, &["hub", "new", "proj", "--", "sh"]);
        let left = left.trim().to_owned();
        sandbox.ok(&sandbox.root, &["hub", "kill", &id]);
        sandbox.wait_exited(&id);
        (id, other, left)
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    // Stopped either way: no hub outlives the test.
    let stopped = sandbox.fw(&sandbox.root, &["hub", "stop"]);
    let (id, other, left) = outcome.unwrap();
    assert!(stopped.status.success(), "fw hub stop: {stopped:?}");

    let rows = audit_rows(&sandbox, &project);
    let kinds: Vec<&str> = rows.iter().filter_map(|r| r["kind"].as_str()).collect();
    assert!(kinds.contains(&"hub.session.start"), "{kinds:?}");
    assert!(kinds.contains(&"hub.session.end"), "{kinds:?}");
    let start = rows
        .iter()
        .find(|r| r["kind"] == "hub.session.start")
        .unwrap();
    assert_eq!(start["session_id"], id.as_str());
    assert_eq!(start["session_kind"], "shell");
    assert_eq!(start["cwd"], project.display().to_string());
    assert_eq!(start["repo"], project.display().to_string());
    assert!(start["at"].as_str().is_some_and(|at| !at.is_empty()));
    let end = rows
        .iter()
        .find(|r| r["kind"] == "hub.session.end")
        .unwrap();
    assert_eq!(end["session_id"], id.as_str());
    assert!(end.get("exit_code").is_some() && end.get("signal").is_some());
    assert!(end.get("hub_stopping").is_none(), "{end}");
    // The session the stop hung up was written before the hub exited.
    let hung_up = rows
        .iter()
        .find(|r| r["kind"] == "hub.session.end" && r["session_id"] == left.as_str())
        .unwrap_or_else(|| panic!("no end row for {left}: {rows:?}"));
    assert_eq!(hung_up["hub_stopping"], true);
    // Only this project's sessions: the stranger's never land here.
    assert!(
        rows.iter().all(|r| r["session_id"] != other.as_str()),
        "{rows:?}"
    );

    // The stranger repo stays no project: no id written, no project dir.
    let config = std::fs::read_to_string(stranger.join(".git/config")).unwrap();
    assert!(!config.contains("projectid"), "{config}");
    let projects: Value =
        serde_json::from_str(&sandbox.ok(&sandbox.root, &["project", "ls", "--json"])).unwrap();
    let projects = projects.as_array().unwrap();
    assert_eq!(projects.len(), 1, "{projects:?}");
    assert_eq!(projects[0]["path"], project.display().to_string());
}
