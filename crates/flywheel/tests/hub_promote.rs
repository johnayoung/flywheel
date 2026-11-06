//! A verified fan-out and a flywheel task, both ways, through the real
//! `fw` binary: a detached hub on 127.0.0.1:0 over a flywheel-adopted git
//! repo (`fw init`), scripted `claude` stand-ins on PATH, graders run by
//! flywheel-core's runner. Promote writes a task file the loop's loader
//! and `fw status --json` read; "fan out this task" seeds a fan-out with
//! the task's command graders plus its held-out graders, which run but
//! never reach an agent (prompt, argv, ask-to-fix); promoting that fan-out
//! back gives the task's command graders again.

#![cfg(unix)]

#[path = "../../flywheel-hub/tests/support/script.rs"]
mod script;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    repo: PathBuf,
}

const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Promote"),
    ("GIT_AUTHOR_EMAIL", "promote@example.invalid"),
    ("GIT_COMMITTER_NAME", "Promote"),
    ("GIT_COMMITTER_EMAIL", "promote@example.invalid"),
];

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = root.join("state");
        let repo = root.join("code").join("demo");
        std::fs::create_dir_all(&repo).unwrap();
        let env = Self {
            _dir: dir,
            root,
            state,
            repo,
        };
        env.git(&["init", "-q", "-b", "main"]);
        std::fs::write(env.repo.join("README.md"), "demo\n").unwrap();
        env.git(&["add", "-A"]);
        env.git(&["commit", "-qm", "initial"]);
        let init = env.fw_in_repo(&["init"]);
        assert!(init.status.success(), "fw init: {init:?}");
        let mut policy = std::fs::read_to_string(env.repo.join("flywheel.toml")).unwrap();
        policy.push_str("\n[held_out]\nroot = \".flywheel/verification/held-out\"\n");
        std::fs::write(env.repo.join("flywheel.toml"), policy).unwrap();
        env.git(&["add", "-A"]);
        env.git(&["commit", "-qm", "adopt flywheel"]);
        env
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .envs(IDENTITY)
            .env("HOME", &self.root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        command
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("FW_BIN", env!("CARGO_BIN_EXE_fw"))
            .env("FANOUT_ROOT", &self.root)
            .envs(IDENTITY)
            .env_remove("XDG_STATE_HOME")
            .env_remove("FLYWHEEL_HOME")
            .env_remove("FW_HUB_TOKEN")
            .env_remove("FLYWHEEL_PG_DSN")
            .env_remove("DATABASE_URL");
        command
    }

    /// `fw ARGS` run in the repo (the loop's own verbs).
    fn fw_in_repo(&self, args: &[&str]) -> Output {
        self.base_command()
            .args(args)
            .current_dir(&self.repo)
            .output()
            .unwrap()
    }

    /// `fw hub ARGS` against this env's state dir.
    fn fw(&self, args: &[&str]) -> Output {
        let mut argv: Vec<&str> = vec!["hub", args[0]];
        let mut rest = args[1..].to_vec();
        if args[0] == "fanout" {
            argv.push(rest.remove(0));
        }
        argv.extend(["--state-dir", self.state.to_str().unwrap()]);
        argv.extend(rest);
        self.base_command()
            .args(&argv)
            .current_dir(&self.root)
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

    fn status(&self) -> Vec<Value> {
        // `fw init` scaffolded a legacy flywheel.toml: name it explicitly.
        let output = self.fw_in_repo(&["status", "--json", "--policy", "flywheel.toml"]);
        assert!(output.status.success(), "fw status: {output:?}");
        serde_json::from_slice::<Value>(&output.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.fw(&["stop", "--force", "--timeout", "10"]);
    }
}

/// `claude` stand-in: `--model pass` commits 42 (and who it is, so it
/// always changes something), `fail` 41, `sneaky` 420
/// (which a substring check passes and an exact one does not). It writes
/// the prompt it was given (its positional argument) beside the env.
fn fake_claude(env: &Env) {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    script::write_script(
        &bin.join("claude"),
        r#"#!/bin/sh
model=""; resume=""; sid=""
while [ $# -gt 0 ]; do
  case "$1" in
    --model) model="$2"; shift 2 ;;
    --resume) resume=1; shift 2 ;;
    --session-id) sid="$2"; shift 2 ;;
    --) shift; break ;;
    *) shift ;;
  esac
done
printf '%s' "$*" > "$FANOUT_ROOT/prompt-$(basename "$PWD")"
if [ -n "$sid" ]; then
  dir="$CLAUDE_CONFIG_DIR/projects/$(printf %s "$PWD" | sed 's/[^A-Za-z0-9]/-/g')"
  mkdir -p "$dir" && echo '{}' > "$dir/$sid.jsonl"
fi
echo "claude ready ($model)"
if [ -n "$resume" ]; then exec cat; fi
"$FW_BIN" hub report --source claude --event UserPromptSubmit
case "$model" in
  pass) echo 42 > answer.txt; basename "$PWD" > who.txt ;;
  fail) echo 41 > answer.txt ;;
  sneaky) echo 420 > answer.txt ;;
esac
git add -A && git commit -qm "answer"
"$FW_BIN" hub report --source claude --event Stop
exec cat
"#,
    );
}

fn graders(value: &Value) -> Vec<(String, Option<String>, bool)> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["run"].as_str().unwrap().to_owned(),
                g["name"].as_str().map(str::to_owned),
                g["held_out"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

fn candidate<'a>(board: &'a Value, label: &str) -> &'a Value {
    board["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["label"] == label)
        .unwrap()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn a_fan_out_becomes_a_flywheel_task_and_a_task_fans_out_with_its_held_out_graders() {
    let env = Env::new();
    fake_claude(&env);
    env.ok(&["start", "--port", "0"]);
    let repo = env.repo.to_str().unwrap().to_owned();
    env.ok(&["new", "demo", "--create", "--folder", &repo, "--name", "sh"]);

    let id = env
        .ok(&[
            "fanout",
            "new",
            "demo",
            "--name",
            "answer",
            "--prompt",
            "Write the answer to answer.txt\n\nThe answer is 42.",
            "--agent",
            "claude:pass",
            "--agent",
            "claude:fail",
            "--grader",
            "exists=test -f answer.txt",
            "--grader",
            "answer=grep -q 42 answer.txt",
            "--protect",
            "README.md",
            "--timeout",
            "60",
        ])
        .trim()
        .to_owned();
    env.ok(&["fanout", "wait", &id, "--timeout", "90"]);
    // Promoted instead of picked: the task's worker lands the work.
    let base = env.git(&["rev-parse", "main"]);
    let board = env.json(&["fanout", "show", &id, "--json"]);
    let head = candidate(&board, "c1")["grade"]["head"]
        .as_str()
        .unwrap_or_else(|| panic!("c1 graded at a commit: {board:#}"))
        .to_owned();

    // The dry run shows exactly what is written, and writes nothing.
    let plan = env.json(&["fanout", "promote", &id, "--reference", "c1", "--json"]);
    let task_id = format!("answer-{}", &id[..4]);
    let relative = format!(".flywheel/tasks/active/01-{task_id}/{task_id}.json");
    assert_eq!(plan["plan"]["relative"], relative.as_str(), "{plan:#}");
    assert_eq!(plan["plan"]["task_id"], task_id.as_str());
    assert_eq!(plan["reference"], head.as_str(), "the winner's commit");
    let path = env.repo.join(&relative);
    assert!(!path.exists(), "a dry run writes nothing");
    let contents = plan["plan"]["contents"].as_str().unwrap().to_owned();
    assert!(
        plan["plan"]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("README.md")),
        "the protected path not covered by [submit] protected_paths is named: {plan:#}"
    );

    let done = env.json(&[
        "fanout",
        "promote",
        &id,
        "--reference",
        "c1",
        "--yes",
        "--json",
    ]);
    assert_eq!(done["written"]["listed"], true, "{done:#}");
    assert_eq!(
        read(&path),
        contents,
        "the bytes reviewed are the bytes written"
    );
    let task: Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(task["goal"], "Write the answer to answer.txt");
    assert!(
        task["context"]["notes"]
            .as_str()
            .unwrap()
            .contains("The answer is 42.")
    );
    assert!(
        task["context"]["references"][0]
            .as_str()
            .unwrap()
            .contains(&head)
    );
    assert!(
        task["context"]["constraints"][0]
            .as_str()
            .unwrap()
            .contains("README.md")
    );
    assert_eq!(
        graders(&task["graders"]),
        vec![
            ("test -f answer.txt".into(), Some("exists".into()), false),
            ("grep -q 42 answer.txt".into(), Some("answer".into()), false),
        ]
    );
    assert!(
        task["graders"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["type"] == "command")
    );
    // Uncommitted: the hub never commits to the operator's branch.
    assert_eq!(env.git(&["rev-parse", "main"]), base);
    assert!(
        env.git(&["status", "--porcelain"])
            .contains(".flywheel/tasks/")
    );
    // What the loop reads.
    let listed = env.status();
    let entry = listed
        .iter()
        .find(|e| e["task_id"] == task_id.as_str())
        .unwrap_or_else(|| panic!("fw status lists {task_id}: {listed:?}"));
    assert_eq!(entry["state"], "fresh");
    assert_eq!(entry["task_file"], relative.as_str());
    let board = env.json(&["fanout", "show", &id, "--json"]);
    assert_eq!(board["promoted"][0]["task_id"], task_id.as_str());
    // Never twice.
    let again = env.fw(&["fanout", "promote", &id, "--reference", "c1", "--yes"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already defined"));

    // A held-out oracle for the task, out of every worktree.
    let held = env.repo.join(".flywheel/verification/held-out");
    std::fs::create_dir_all(&held).unwrap();
    let oracle = held.join("exact.sh");
    std::fs::write(&oracle, "grep -qx 42 answer.txt\n").unwrap();
    let oracle_run = format!("sh {}", oracle.display());
    std::fs::write(
        held.join(format!("{task_id}.json")),
        serde_json::json!({"graders": [
            {"type": "command", "run": oracle_run, "name": "held-out-oracle:exact.sh"}
        ]})
        .to_string(),
    )
    .unwrap();

    // A prompt naming the oracle is refused before anything starts.
    let leak = env.fw(&[
        "fanout",
        "new",
        "demo",
        "--from-task",
        &task_id,
        "--prompt",
        &format!("make {} pass", oracle.display()),
        "--agent",
        "claude:pass",
        "--agent",
        "claude:sneaky",
    ]);
    assert!(!leak.status.success());
    assert!(String::from_utf8_lossy(&leak.stderr).contains("never shown to the agents"));

    let seeded = env.json(&[
        "fanout",
        "new",
        "demo",
        "--from-task",
        &task_id,
        "--agent",
        "claude:pass",
        "--agent",
        "claude:sneaky",
        "--json",
    ]);
    let sid = seeded["id"].as_str().unwrap().to_owned();
    assert_eq!(seeded["origin"]["task_id"], task_id.as_str());
    assert_eq!(seeded["origin"]["held_out"], 1);
    let protect: Vec<&str> = seeded["protect"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    for path in ["README.md", "flywheel.toml", ".github"] {
        assert!(
            protect.contains(&path),
            "the task's constraint and the repo's [submit] protected_paths hold the candidates: \
             {protect:?}"
        );
    }
    assert_eq!(seeded["name"], task_id.as_str());
    assert_eq!(
        graders(&seeded["graders"]),
        vec![
            ("test -f answer.txt".into(), Some("exists".into()), false),
            ("grep -q 42 answer.txt".into(), Some("answer".into()), false),
            (
                oracle_run.clone(),
                Some("held-out-oracle:exact.sh".into()),
                true
            ),
        ],
        "the task's command graders, then its held-out ones"
    );
    let sent = seeded["sent_prompt"].as_str().unwrap();
    assert!(
        sent.starts_with("# Goal\n\nWrite the answer to answer.txt"),
        "{sent}"
    );
    assert!(sent.contains("`grep -q 42 answer.txt` (answer)"), "{sent}");
    assert!(
        !sent.contains("exact.sh") && !sent.contains("verification"),
        "{sent}"
    );

    let settled = env.json(&["fanout", "wait", &sid, "--timeout", "90", "--json"]);
    // What the agents really got: no held-out grader.
    for c in settled["candidates"].as_array().unwrap() {
        let dir = Path::new(c["path"].as_str().unwrap());
        let got = read(&env.root.join(format!(
            "prompt-{}",
            dir.file_name().unwrap().to_string_lossy()
        )));
        assert!(got.contains("# Goal"), "{got}");
        assert!(!got.contains("exact.sh"), "{got}");
    }
    assert_eq!(candidate(&settled, "c1")["verdict"], "pass", "{settled:#}");
    let sneaky = candidate(&settled, "c2");
    assert_eq!(sneaky["verdict"], "fail", "the held-out grader caught it");
    let runs = sneaky["grade"]["graders"].as_array().unwrap();
    assert_eq!(
        runs.iter()
            .map(|g| g["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["passed", "passed", "failed"],
        "the visible graders pass 420; the held-out one does not"
    );
    assert_eq!(runs[2]["held_out"], true);
    // Asking it to fix says a held-out check failed, nothing more.
    let fix = env.json(&["fanout", "fix", &sid, "c2", "--json"]);
    let prompt = fix["prompt"].as_str().unwrap();
    assert!(prompt.contains("held-out check"), "{prompt}");
    assert!(
        !prompt.contains("exact.sh") && !prompt.contains("grep -qx"),
        "{prompt}"
    );

    // The task it came from is the durable one: promoting it again needs
    // a new id, named on purpose.
    let dup = env.fw(&["fanout", "promote", &sid, "--yes"]);
    assert!(!dup.status.success());
    let dup = String::from_utf8_lossy(&dup.stderr);
    assert!(
        dup.contains(&format!("came from flywheel task {task_id}")) && dup.contains("fw worker"),
        "{dup}"
    );

    // Round trip: the seeded fan-out promoted gives the task's command
    // graders back (the held-out one stays in the registry).
    let back = env.json(&[
        "fanout",
        "promote",
        &sid,
        "--task-id",
        "answer-again",
        "--phase",
        &format!("01-{task_id}"),
        "--reference",
        "c1",
        "--yes",
        "--json",
    ]);
    let again: Value =
        serde_json::from_str(&read(Path::new(back["written"]["path"].as_str().unwrap()))).unwrap();
    assert_eq!(again["goal"], task["goal"]);
    assert_eq!(graders(&again["graders"]), graders(&task["graders"]));
    // Field for field, not flattened into one prompt and nested again.
    assert_eq!(again["context"]["notes"], task["context"]["notes"]);
    assert_eq!(
        again["context"]["constraints"], task["context"]["constraints"],
        "{again:#}"
    );
    let references = again["context"]["references"].as_array().unwrap();
    assert_eq!(references.len(), 2, "the original's, then this fan-out's");
    assert_eq!(references[0], task["context"]["references"][0]);
    assert!(
        references[1]
            .as_str()
            .unwrap()
            .contains(&format!("seeded from flywheel task {task_id}"))
    );
    assert_eq!(
        back["written"]["queue"],
        serde_json::json!([task_id]),
        "the worker would drain the original too"
    );
    assert!(
        back["warnings"][0]
            .as_str()
            .unwrap()
            .contains(&format!("registered for task {task_id}")),
        "{back:#}"
    );
    let listed = env.status();
    for id in [task_id.as_str(), "answer-again"] {
        assert!(
            listed
                .iter()
                .any(|e| e["task_id"] == id && e["state"] == "fresh"),
            "{id}: {listed:?}"
        );
    }
    // The oracle changed under the fan-out: that is the operator's to
    // restore, so its agent is asked nothing (nor told a held-out grader
    // exists).
    let mut changed = read(&oracle);
    changed.push_str("# edited\n");
    std::fs::write(&oracle, changed).unwrap();
    env.ok(&["fanout", "grade", &sid, "c2"]);
    let regraded = env.json(&["fanout", "wait", &sid, "--timeout", "90", "--json"]);
    let grade = &candidate(&regraded, "c2")["grade"];
    assert_eq!(grade["operator"], true, "{grade:#}");
    assert!(
        grade["error"]
            .as_str()
            .unwrap()
            .contains("1 held-out grader script")
    );
    let refused = env.fw(&["fanout", "fix", &sid, "c2", "--json"]);
    assert!(!refused.status.success());
    let refused = String::from_utf8_lossy(&refused.stderr);
    assert!(refused.contains("not its agent's to fix"), "{refused}");
    assert!(!refused.contains("exact.sh"), "{refused}");

    // Picked, the work is on the base: a task for it would have nothing
    // to do (a real worker's agent stops: "already on the base"), so it
    // is not written.
    // Picking a promoted fan-out says what it does to the task.
    let shown = env.fw(&["fanout", "pick", &id, "c1"]);
    let shown = String::from_utf8_lossy(&shown.stderr);
    assert!(
        shown.contains(&format!("promoted to flywheel task {task_id}"))
            && shown.contains("its worker finds nothing left to do"),
        "{shown}"
    );
    env.git(&["stash", "push", "-q", "--include-untracked"]);
    env.ok(&["fanout", "pick", &id, "c1", "--yes"]);
    assert_eq!(env.git(&["rev-parse", "main"]), head);
    let late = env.fw(&[
        "fanout",
        "promote",
        &id,
        "--task-id",
        "answer-late",
        "--json",
    ]);
    assert!(!late.status.success());
    let late = String::from_utf8_lossy(&late.stderr);
    assert!(
        late.contains("c1 was picked") && late.contains("nothing left to do"),
        "{late}"
    );
}
