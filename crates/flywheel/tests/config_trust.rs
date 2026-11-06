//! The effective config and repo-file trust, end to end through the real
//! binary: `FLYWHEEL_HOME`, `HOME`, and `XDG_STATE_HOME` point at a tempdir,
//! repos are real git repos in tempdirs, and stdin is never a terminal.
//!
//! `fw config --json` shows every key with its source under the precedence
//! flag > env > personal > repo > user > default; a repo file with a
//! non-repo key, an old section, or a credential is refused naming the key
//! (never echoing a credential); an unapproved repo file is warned about by
//! read-only verbs and refused by `fw worker` (naming `fw trust`) before it
//! runs anything; `fw trust` approves exactly the current bytes, an edit
//! revokes it, and `fw trust --revoke` drops it; a trusted `verify` gates the
//! worker's landing; a legacy `flywheel.toml` / `.flywheel/` is ignored with
//! a notice and left untouched.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use serde_json::Value;
use std::collections::BTreeMap;
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
        std::fs::create_dir_all(root.join("fwhome")).unwrap();
        script::write_script(&root.join("bin/claude"), FAKE_CLAUDE);
        Self { _dir: dir, root }
    }

    fn flywheel_home(&self) -> PathBuf {
        self.root.join("fwhome")
    }

    fn fw_env(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .args(args)
            .current_dir(cwd)
            .env("FLYWHEEL_HOME", self.flywheel_home())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude-config"))
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL")
            .stdin(Stdio::null());
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn fw(&self, cwd: &Path, args: &[&str]) -> Output {
        self.fw_env(cwd, args, &[])
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

    /// `fw config --json` as `{key: (value, source)}`.
    fn config(&self, cwd: &Path) -> BTreeMap<String, (Value, String)> {
        let json: Value = serde_json::from_str(&self.ok(cwd, &["config", "--json"])).unwrap();
        json.as_object()
            .unwrap()
            .iter()
            .map(|(key, entry)| {
                (
                    key.clone(),
                    (
                        entry["value"].clone(),
                        entry["source"].as_str().unwrap().to_owned(),
                    ),
                )
            })
            .collect()
    }

    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "cfg@example.invalid"]);
        git(&repo, &["config", "user.name", "config test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);
        repo
    }

    fn project_dir(&self, repo: &Path) -> PathBuf {
        self.flywheel_home()
            .join("projects")
            .join(git(repo, &["config", "--get", "flywheel.projectid"]))
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

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn entry(value: impl Into<Value>, source: &str) -> (Value, String) {
    (value.into(), source.to_owned())
}

#[test]
fn defaults_and_the_exact_key_set() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    let config = sandbox.config(&repo);
    let keys: Vec<&str> = config.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "concurrency",
            "land",
            "model",
            "project_id",
            "protected_paths",
            "store",
            "transcript_retention_days",
            "verify"
        ]
    );
    let id = git(&repo, &["config", "--get", "flywheel.projectid"]);
    assert_eq!(config["model"], entry(Value::Null, "default"));
    assert_eq!(config["concurrency"], entry(1, "default"));
    assert_eq!(config["store"], entry("sqlite", "default"));
    assert_eq!(config["transcript_retention_days"], entry(90, "default"));
    assert_eq!(config["verify"], entry(Value::Null, "default"));
    assert_eq!(
        config["protected_paths"],
        entry(Vec::<Value>::new(), "default")
    );
    assert_eq!(config["land"], entry("merge", "default"));
    assert_eq!(config["project_id"], entry(id, "default"));

    // The text form: one line per key, each naming its source.
    let text = sandbox.ok(&repo, &["config"]);
    assert_eq!(text.lines().count(), 8, "{text}");
    assert!(text.lines().all(|l| l.ends_with("(default)")), "{text}");
}

#[test]
fn precedence_user_repo_personal_env_flag() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    std::fs::write(
        sandbox.flywheel_home().join("config.toml"),
        "land = \"pr\"\nconcurrency = 2\nmodel = \"user-model\"\n",
    )
    .unwrap();
    std::fs::write(repo.join(".flywheel.toml"), "land = \"merge\"\n").unwrap();
    sandbox.ok(&repo, &["trust"]);
    let config = sandbox.config(&repo);
    assert_eq!(config["land"], entry("merge", "repo"));
    assert_eq!(config["concurrency"], entry(2, "user"));

    std::fs::write(
        sandbox.project_dir(&repo).join("config.toml"),
        "land = \"pr\"\ntranscript_retention_days = 30\n",
    )
    .unwrap();
    let config = sandbox.config(&repo);
    assert_eq!(config["land"], entry("pr", "personal"));
    assert_eq!(config["transcript_retention_days"], entry(30, "personal"));

    // env (FLYWHEEL_PG_DSN) selects the store, its password redacted.
    let output = sandbox.fw_env(
        &repo,
        &["config", "--json"],
        &[(
            "FLYWHEEL_PG_DSN",
            "postgres://me:hunter2@db.invalid:5432/fw",
        )],
    );
    assert!(output.status.success(), "{}", text(&output));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["store"]["source"], "env");
    assert_eq!(
        json["store"]["value"],
        "postgres://me:***@db.invalid:5432/fw"
    );
    assert!(!text(&output).contains("hunter2"));

    // Flags win.
    let json: Value = serde_json::from_str(&sandbox.ok(
        &repo,
        &[
            "config",
            "--json",
            "--model",
            "flag-model",
            "--concurrency=4",
        ],
    ))
    .unwrap();
    assert_eq!(json["model"]["value"], "flag-model");
    assert_eq!(json["model"]["source"], "flag");
    assert_eq!(json["concurrency"]["value"], 4);
    assert_eq!(json["concurrency"]["source"], "flag");
}

#[test]
fn a_user_store_dsn_is_redacted() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    std::fs::write(
        sandbox.flywheel_home().join("config.toml"),
        "store = \"postgres://fw:s3cret@db.invalid/fw\"\n",
    )
    .unwrap();
    let out = sandbox.ok(&repo, &["config", "--json"]);
    assert!(!out.contains("s3cret"), "{out}");
    let json: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["store"]["value"], "postgres://fw:***@db.invalid/fw");
    assert_eq!(json["store"]["source"], "user");
}

#[test]
fn bad_repo_files_are_refused_naming_the_key() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    for (body, key) in [
        ("[sandbox]\npreset = \"fast\"\n", "sandbox"),
        ("[submit]\nverify = \"make\"\n", "submit"),
        ("concurrency = 3\n", "concurrency"),
        ("model = \"x\"\n", "model"),
    ] {
        std::fs::write(repo.join(".flywheel.toml"), body).unwrap();
        let output = sandbox.fw(&repo, &["config", "--json"]);
        assert_ne!(output.status.code(), Some(0), "{body}");
        assert!(text(&output).contains(key), "{body}: {}", text(&output));
    }
    for (body, key) in [
        ("dsn = \"postgres://u:p@h/db\"\n", "dsn"),
        ("store = \"postgres://u:p@h/db\"\n", "store"),
        ("verify = \"curl https://u:p@h/x\"\n", "verify"),
    ] {
        std::fs::write(repo.join(".flywheel.toml"), body).unwrap();
        for args in [&["config", "--json"][..], &["trust"][..], &["status"][..]] {
            let output = sandbox.fw(&repo, args);
            let shown = text(&output);
            assert_ne!(output.status.code(), Some(0), "{args:?} {body}");
            assert!(shown.contains(key), "{args:?} {body}: {shown}");
            assert!(!shown.contains("u:p@"), "{args:?} echoed: {shown}");
        }
    }
    // A user config with an old section is refused too.
    std::fs::remove_file(repo.join(".flywheel.toml")).unwrap();
    std::fs::write(
        sandbox.flywheel_home().join("config.toml"),
        "[worker]\nconcurrency = 2\n",
    )
    .unwrap();
    let output = sandbox.fw(&repo, &["config"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(text(&output).contains("worker"), "{}", text(&output));
}

#[test]
fn trust_gates_the_worker_and_follows_the_bytes() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    let marker = sandbox.root.join("marker");
    std::fs::write(
        repo.join(".flywheel.toml"),
        format!("verify = \"touch {}\"\n", marker.display()),
    )
    .unwrap();

    // Read-only verbs warn and continue without the file.
    let output = sandbox.fw(&repo, &["config", "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not trusted") && stderr.contains("fw trust"),
        "{stderr}"
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["verify"]["source"], "default");
    let output = sandbox.fw(&repo, &["status", "--json"]);
    assert!(output.status.success(), "{}", text(&output));

    // The worker refuses unattended, naming the command, and runs nothing.
    let output = sandbox.fw(&repo, &["worker", "--once"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(text(&output).contains("fw trust"), "{}", text(&output));
    assert!(!marker.exists());
    // So does the autopilot.
    let output = sandbox.fw(&repo, &["autopilot", "--once"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(text(&output).contains("fw trust"), "{}", text(&output));

    let out = sandbox.ok(&repo, &["trust"]);
    assert!(out.contains("trusted"), "{out}");
    let trust: Value =
        serde_json::from_slice(&std::fs::read(sandbox.flywheel_home().join("trust.json")).unwrap())
            .unwrap();
    assert_eq!(
        trust["approvals"][0]["project_id"],
        git(&repo, &["config", "--get", "flywheel.projectid"])
    );
    let config = sandbox.config(&repo);
    assert_eq!(
        config["verify"],
        entry(format!("touch {}", marker.display()), "repo")
    );

    // Editing the file revokes trust.
    std::fs::write(
        repo.join(".flywheel.toml"),
        format!("verify = \"touch {}\"\nland = \"pr\"\n", marker.display()),
    )
    .unwrap();
    let output = sandbox.fw(&repo, &["worker", "--once"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(text(&output).contains("fw trust"), "{}", text(&output));
    assert!(!marker.exists());

    sandbox.ok(&repo, &["trust"]);
    assert_eq!(sandbox.config(&repo)["land"], entry("pr", "repo"));
    let out = sandbox.ok(&repo, &["trust", "--revoke"]);
    assert!(out.contains("revoked 2 approvals"), "{out}");
    let config = sandbox.config(&repo);
    assert_eq!(config["verify"].1, "default");
    assert_eq!(config["land"].1, "default");

    // Nothing to trust without a file.
    std::fs::remove_file(repo.join(".flywheel.toml")).unwrap();
    let output = sandbox.fw(&repo, &["trust"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output).contains("nothing to trust"));
}

#[test]
fn a_repo_project_id_shares_the_project_dir_but_not_the_trust_key() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    let shared = "6f9619ff-8b86-4011-b42d-00c04fc964ff";
    sandbox.ok(&repo, &["status", "--json"]);
    let local = git(&repo, &["config", "--get", "flywheel.projectid"]);
    std::fs::write(
        repo.join(".flywheel.toml"),
        format!("project_id = \"{shared}\"\n"),
    )
    .unwrap();
    // Untrusted: the local id applies.
    assert_eq!(
        sandbox.config(&repo)["project_id"],
        entry(local.as_str(), "default")
    );
    assert_ne!(local, shared);
    sandbox.ok(&repo, &["trust"]);
    let config = sandbox.config(&repo);
    assert_eq!(config["project_id"], entry(shared, "repo"));
    // The store lives in the shared project dir; trust stays keyed locally.
    sandbox.ok(&repo, &["status", "--json"]);
    let shared_dir = sandbox.flywheel_home().join("projects").join(shared);
    assert!(shared_dir.join("tasks/active").is_dir());
    assert!(shared_dir.join("meta.json").is_file());
    let trust = std::fs::read_to_string(sandbox.flywheel_home().join("trust.json")).unwrap();
    assert!(trust.contains(&local) && !trust.contains(shared), "{trust}");
    // prune keeps the shared dir while the repo declares it.
    let pruned = sandbox.ok(&repo, &["project", "prune"]);
    assert!(!pruned.contains(shared), "{pruned}");
    assert!(shared_dir.is_dir());
}

#[test]
fn a_trusted_verify_gates_the_landing() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    let marker = sandbox.root.join("verified");
    std::fs::write(
        repo.join(".flywheel.toml"),
        format!("verify = \"echo ran >> {}\"\n", marker.display()),
    )
    .unwrap();
    git(&repo, &["add", ".flywheel.toml"]);
    git(&repo, &["commit", "-qm", "flywheel config"]);
    sandbox.ok(&repo, &["trust"]);
    let phase = sandbox.project_dir(&repo).join("tasks/active/01-cfg");
    std::fs::create_dir_all(&phase).unwrap();
    let task = serde_json::json!({
        "id": "alpha",
        "goal": "Goal alpha.",
        "graders": [{"type": "command", "name": "unit", "run": "test -f work-alpha.txt"}],
    });
    std::fs::write(phase.join("alpha.json"), task.to_string()).unwrap();
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
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let log = git(&repo, &["log", "--format=%s", "main"]);
    assert!(log.lines().any(|s| s == "work alpha"), "{log}");
    // The repo's verify ran (the standing submit gate, then the phase exit).
    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        ran.lines().count() >= 1,
        "verify never ran: {}",
        text(&output)
    );
}

#[test]
fn a_legacy_layout_is_ignored_with_a_notice_and_left_untouched() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("repo");
    std::fs::write(
        repo.join("flywheel.toml"),
        "[source]\nkind = \"directory\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.join(".flywheel/tasks/active/01-old")).unwrap();
    std::fs::write(repo.join(".flywheel/tasks/active/01-old/t.json"), "{}").unwrap();
    let snapshot = |dir: &Path| -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            for item in std::fs::read_dir(&next).unwrap() {
                let path = item.unwrap().path();
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path.clone());
                    out.push((path, Vec::new()));
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    out.push((path, bytes));
                }
            }
        }
        out.sort();
        out
    };
    let before = snapshot(&repo);
    let output = sandbox.fw(&repo, &["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ignored"), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        listed,
        Value::Array(Vec::new()),
        "the old tasks are not read"
    );
    assert_eq!(snapshot(&repo), before);
}
