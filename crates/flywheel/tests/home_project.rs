//! The flywheel home and project identity, end to end through the real
//! binary: `FLYWHEEL_HOME`, `HOME`, and `XDG_STATE_HOME` point at a tempdir,
//! repos are real git repos in tempdirs, and no `flywheel.toml` exists.
//!
//! A fresh repo's store verbs work and create exactly one project dir while
//! writing nothing into the repo but the `.git/config` id; linked worktrees
//! share the project; a moved repo keeps its id; a copy mints its own; a
//! non-git dir is refused naming git; the hub keeps its state under
//! `$FLYWHEEL_HOME/hub`; `fw project ls|prune|relink`; and a worker drains a
//! home project's queue and lands the work.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The agent stand-in: commits a file in its worktree and ends the
/// iteration with `verify`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
id=$(basename "$PWD")
echo "$id" > "work-$id.txt"
git add -A >/dev/null 2>&1 && git commit -qm "work $id" >/dev/null 2>&1
E='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

/// A hermetic sandbox: its own home, flywheel home, and XDG state dir.
struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        Self { _dir: dir, root }
    }

    fn flywheel_home(&self) -> PathBuf {
        self.root.join("fwhome")
    }

    fn xdg_state(&self) -> PathBuf {
        self.root.join("xdg-state")
    }

    fn projects(&self) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(self.flywheel_home().join("projects"))
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    fn meta(&self, id: &str) -> serde_json::Value {
        let path = self
            .flywheel_home()
            .join("projects")
            .join(id)
            .join("meta.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn fw(&self, cwd: &Path, args: &[&str]) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        Command::new(env!("CARGO_BIN_EXE_fw"))
            .args(args)
            .current_dir(cwd)
            .env("FLYWHEEL_HOME", self.flywheel_home())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.xdg_state())
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL")
            .env_remove("FW_HUB_TOKEN")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.fw(cwd, args);
        assert!(
            output.status.success(),
            "fw {args:?} exited {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// A fresh git repo with one commit on `main`.
    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "home@example.invalid"]);
        git(&repo, &["config", "user.name", "home test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        repo
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn stored_id(repo: &Path) -> String {
    git(repo, &["config", "--get", "flywheel.projectid"])
}

fn is_lowercase_uuid(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && id
            .chars()
            .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// The repo's top-level entries and its git-visible state.
fn tree(repo: &Path) -> (Vec<String>, String) {
    let mut entries: Vec<String> = std::fs::read_dir(repo)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    (
        entries,
        git(
            repo,
            &[
                "status",
                "--porcelain",
                "--ignored",
                "--untracked-files=all",
            ],
        ),
    )
}

#[test]
fn a_fresh_repo_gets_one_project_and_nothing_in_its_tree() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    let before = tree(&repo);

    let out = sandbox.ok(&repo, &["status", "--json"]);
    let snapshot: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(snapshot, serde_json::json!([]));

    let projects = sandbox.projects();
    assert_eq!(projects.len(), 1, "{projects:?}");
    let id = stored_id(&repo);
    assert!(is_lowercase_uuid(&id), "{id}");
    assert_eq!(projects[0], id);
    let meta = sandbox.meta(&id);
    assert_eq!(meta["id"], id.as_str());
    assert_eq!(meta["path"], repo.to_str().unwrap());
    assert_eq!(
        meta["git_common_dir"],
        repo.join(".git").canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(meta["remote"], serde_json::Value::Null);
    assert!(meta["first_seen"].is_string() && meta["last_seen"].is_string());
    assert!(
        sandbox
            .flywheel_home()
            .join("projects")
            .join(&id)
            .join("flywheel.sqlite")
            .is_file()
    );
    assert_eq!(tree(&repo), before, "the repo tree changed");

    // The other store verbs read the same project; still one project dir,
    // still nothing in the tree.
    for verb in [
        &["status"][..],
        &["history", "--json"],
        &["live"],
        &["validate"],
        &["recover"],
        &["archive"],
        &["--json"],
    ] {
        sandbox.ok(&repo, verb);
    }
    let sub = repo.join("src").join("deep");
    std::fs::create_dir_all(&sub).unwrap();
    sandbox.ok(&sub, &["status", "--json"]);
    std::fs::remove_dir_all(repo.join("src")).unwrap();
    assert_eq!(sandbox.projects(), vec![id]);
    assert_eq!(tree(&repo), before, "the repo tree changed");
    assert!(!sandbox.xdg_state().exists());
}

#[test]
fn the_hub_keeps_its_state_under_the_flywheel_home() {
    let sandbox = Sandbox::new();
    let token = sandbox.ok(&sandbox.root, &["hub", "token"]);
    assert!(!token.trim().is_empty());
    assert!(sandbox.flywheel_home().join("hub").join("token").is_file());
    assert!(!sandbox.xdg_state().exists());
    assert!(!sandbox.root.join("home/.local").exists());
}

#[test]
fn linked_worktrees_share_and_moves_keep_and_copies_mint() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    sandbox.ok(&repo, &["status", "--json"]);
    let id = stored_id(&repo);

    // A linked worktree resolves to the same project.
    let linked = sandbox.root.join("linked");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );
    sandbox.ok(&linked, &["status", "--json"]);
    assert_eq!(stored_id(&linked), id);
    assert_eq!(sandbox.projects(), vec![id.clone()]);
    git(&repo, &["worktree", "remove", linked.to_str().unwrap()]);

    // A moved repo keeps its id; meta.json follows it.
    let moved = sandbox.root.join("moved");
    std::fs::rename(&repo, &moved).unwrap();
    sandbox.ok(&moved, &["status", "--json"]);
    assert_eq!(stored_id(&moved), id);
    assert_eq!(sandbox.meta(&id)["path"], moved.to_str().unwrap());
    assert_eq!(sandbox.projects(), vec![id.clone()]);

    // A copy mints its own id; the original keeps its own.
    let copy = sandbox.root.join("copy");
    let status = Command::new("cp")
        .arg("-a")
        .arg(&moved)
        .arg(&copy)
        .status()
        .unwrap();
    assert!(status.success());
    sandbox.ok(&copy, &["status", "--json"]);
    let copy_id = stored_id(&copy);
    assert_ne!(copy_id, id);
    assert!(is_lowercase_uuid(&copy_id));
    assert_eq!(stored_id(&moved), id);
    assert_eq!(sandbox.meta(&copy_id)["path"], copy.to_str().unwrap());
    assert_eq!(sandbox.meta(&id)["path"], moved.to_str().unwrap());
    sandbox.ok(&moved, &["status", "--json"]);
    assert_eq!(stored_id(&moved), id);
    assert_eq!(sandbox.projects().len(), 2);

    // ls lists both; prune removes the deleted copy's dir; relink re-adopts
    // a project after the repo lost its key.
    let listed: serde_json::Value =
        serde_json::from_str(&sandbox.ok(&sandbox.root, &["project", "ls", "--json"])).unwrap();
    let mut rows: Vec<(String, String)> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["path"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    rows.sort();
    let mut expected = vec![
        (id.clone(), moved.to_str().unwrap().to_owned()),
        (copy_id.clone(), copy.to_str().unwrap().to_owned()),
    ];
    expected.sort();
    assert_eq!(rows, expected);

    std::fs::remove_dir_all(&copy).unwrap();
    let pruned = sandbox.ok(&sandbox.root, &["project", "prune"]);
    assert!(pruned.contains(&format!("removed {copy_id}")), "{pruned}");
    assert_eq!(sandbox.projects(), vec![id.clone()]);

    git(&moved, &["config", "--unset", "flywheel.projectid"]);
    let relinked = sandbox.ok(&moved, &["project", "relink"]);
    assert!(relinked.contains(&id), "{relinked}");
    assert_eq!(stored_id(&moved), id);
    let dir = sandbox.flywheel_home().join("projects").join(&id);
    sandbox.ok(&moved, &["project", "relink", dir.to_str().unwrap()]);
    assert_eq!(stored_id(&moved), id);
}

#[test]
fn a_non_git_dir_is_refused_naming_git() {
    let sandbox = Sandbox::new();
    let plain = sandbox.root.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    for verb in [
        &["status", "--json"][..],
        &["history"],
        &["--json"],
        &["say", "run-1", "hi"],
        &["worker", "--once"],
        &["project", "relink"],
    ] {
        let output = sandbox.fw(&plain, verb);
        assert_ne!(output.status.code(), Some(0), "fw {verb:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("git"), "fw {verb:?}: {stderr}");
    }
    assert!(sandbox.projects().is_empty());
    assert_eq!(std::fs::read_dir(&plain).unwrap().count(), 0);
}

#[test]
fn a_worker_drains_a_home_project_and_lands_the_work() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    sandbox.ok(&repo, &["status", "--json"]);
    let id = stored_id(&repo);
    let project = sandbox.flywheel_home().join("projects").join(&id);
    let phase = project.join("tasks/active/01-home");
    std::fs::create_dir_all(&phase).unwrap();
    let task = serde_json::json!({
        "id": "alpha",
        "goal": "Goal alpha.",
        "graders": [{"type": "command", "name": "unit", "run": "test -f work-alpha.txt"}],
    });
    std::fs::write(phase.join("alpha.json"), task.to_string()).unwrap();

    let status = sandbox.ok(&repo, &["status"]);
    assert!(status.contains("01-home/alpha"), "{status}");

    let output = sandbox.fw(
        &repo,
        &[
            "worker",
            "--once",
            "--heartbeat",
            "0",
            "--poll-interval",
            "1",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains(project.to_str().unwrap()), "{stderr}");

    // The work landed on the operator's branch; flywheel's own state stayed
    // in the home.
    let log = git(&repo, &["log", "--format=%s", "main"]);
    assert!(log.lines().any(|s| s == "work alpha"), "{log}");
    assert!(repo.join("work-alpha.txt").is_file());
    assert!(!repo.join(".flywheel").exists());
    assert!(project.join("flywheel.sqlite").is_file());
    // The run is in the home store, and the completed phase archived into
    // the home tasks dir.
    let history = sandbox.ok(&repo, &["history"]);
    assert!(history.contains("alpha"), "{history}");
    assert!(project.join("tasks/archive/01-home/alpha.json").is_file());
}

#[test]
fn project_help_lists_the_subcommands() {
    let sandbox = Sandbox::new();
    let help = sandbox.ok(&sandbox.root, &["project", "--help"]);
    for sub in ["ls", "prune", "relink"] {
        assert!(help.contains(sub), "{help}");
    }
    let usage = sandbox.ok(&sandbox.root, &["--help"]);
    assert!(usage.contains("project ls"), "{usage}");
}
