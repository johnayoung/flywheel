//! Prompt queues through the real binary: a detached hub whose `claude`
//! is a stand-in that runs its hooks itself (`fw hub report`, as Claude
//! Code's `--settings` hooks would) and writes its transcript. Prompts
//! queued with `fw hub queue add` go one finished turn at a time, and each
//! is typed exactly once across `fw hub upgrade` (live handoff) and `fw
//! hub stop` / `fw hub start` (restore with `--resume`).

#![cfg(target_os = "linux")]

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
        for sub in ["bin", "repo", "log"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let state = root.join("state");
        Self {
            _dir: dir,
            root,
            state,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        // Right after the verb (`queue`: after its sub-verb).
        let verb = if args[0] == "queue" { 2 } else { 1 };
        let mut argv: Vec<&str> = vec!["hub"];
        argv.extend_from_slice(&args[..verb]);
        argv.extend(["--state-dir", self.state.to_str().unwrap()]);
        argv.extend_from_slice(&args[verb..]);
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.root.join("bin")];
        paths.extend(std::env::split_paths(&path));
        command
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &self.root)
            // The stand-in `claude` first on PATH: never the real one.
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("INVOCATION_ID")
            .env_remove("FW_HUB_TOKEN");
        command
    }

    fn fw(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
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

    fn pid(&self) -> u64 {
        self.json(&["status", "--json"])["pid"].as_u64().unwrap()
    }

    /// The prompts the stand-in took, in order (one per line).
    fn taken(&self, session: &str) -> Vec<String> {
        std::fs::read_to_string(self.root.join(format!("log/claude-{session}.in")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn agent_status(&self, session: &str) -> String {
        let listed = self.json(&["ls", "--json"]);
        listed["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|w| w["sessions"].as_array().unwrap().clone())
            .find(|s| s["id"] == session)
            .and_then(|s| s["agent"]["status"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.fw(&["stop", "--force", "--timeout", "10"]);
    }
}

fn write_script(path: &Path, contents: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// A stand-in `claude` that behaves like the CLI for the hub: it reports
/// `SessionStart` once up, and for each prompt (one line, bracketed paste
/// markers stripped) appends it to its log and its transcript, reports
/// `UserPromptSubmit`, works for `<log>/work` seconds, then reports
/// `Stop` -- each through `fw hub report`, the program its hooks run.
fn fake_claude(env: &Env) {
    let fw = env!("CARGO_BIN_EXE_fw");
    write_script(
        &env.root.join("bin/claude"),
        &format!(
            r#"#!/bin/sh
log='{log}'
n="$log/claude-$FLYWHEEL_HUB_SESSION"
conv=""
while [ $# -gt 0 ]; do
  case "$1" in --session-id|--resume) conv="$2"; shift ;; esac
  shift
done
echo "$conv" > "$n.conv"
proj="$CLAUDE_CONFIG_DIR/projects/$(pwd | sed 's/[^A-Za-z0-9]/-/g')"
mkdir -p "$proj"
t="$proj/$conv.jsonl"
touch "$t"
hook() {{ '{fw}' hub report --source claude --event "$1" '{{}}' >/dev/null 2>&1; }}
esc=$(printf '\033')
printf '\033[?2004h'
echo claude-ready
hook SessionStart
while IFS= read -r line; do
  text=$(printf '%s' "$line" | sed "s/$esc\[20[01]~//g" | tr -d '\r')
  [ -n "$text" ] || continue
  printf '%s\n' "$text" >> "$n.in"
  printf '{{"type":"user","uuid":"%s","sessionId":"%s","timestamp":"%s","message":{{"role":"user","content":"%s"}}}}\n' \
    "u$(date +%s%N)" "$conv" "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" "$text" >> "$t"
  hook UserPromptSubmit
  sleep "$(cat "$log/work" 2>/dev/null || echo 1)"
  hook Stop
done
"#,
            log = env.root.join("log").display(),
        ),
    );
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

const WAIT: Duration = Duration::from_secs(90);

fn prompts(n: std::ops::RangeInclusive<usize>) -> Vec<String> {
    n.map(|i| format!("step {i} of the plan")).collect()
}

#[test]
fn queued_prompts_are_typed_exactly_once_across_upgrade_and_restart() {
    let env = Env::new();
    fake_claude(&env);
    std::fs::write(env.root.join("log/work"), "2").unwrap();
    env.ok(&["start", "--port", "0"]);
    let repo = env.root.join("repo");
    let agent = env
        .ok(&[
            "new",
            "--create",
            "--repo",
            repo.to_str().unwrap(),
            "--kind",
            "claude",
            "--name",
            "agent",
            "ws",
        ])
        .trim()
        .to_owned();
    wait_for("the agent to report in", WAIT, || {
        env.agent_status(&agent) == "idle"
    });

    // Three queued while it rests: the first goes at once, then one per
    // finished turn.
    for text in prompts(1..=3) {
        let said = env.ok(&["queue", "add", "agent", &text]);
        assert!(said.contains("queued"), "{said}");
    }
    wait_for("step 1", WAIT, || env.taken(&agent).len() == 1);
    let listed = env.ok(&["queue", "ls", "agent"]);
    assert!(listed.contains("2 pending"), "{listed}");
    assert!(listed.contains("sent "), "{listed}");

    // A live handoff while the agent works on step 1: nothing is typed
    // twice, and the new hub carries on with step 2 and 3.
    let old_pid = env.pid();
    env.ok(&["upgrade", "--binary", env!("CARGO_BIN_EXE_fw")]);
    assert_ne!(env.pid(), old_pid, "a new hub process serves");
    wait_for("steps 2 and 3", WAIT, || env.taken(&agent).len() == 3);
    assert_eq!(env.taken(&agent), prompts(1..=3));

    // Stopped mid-turn with two more queued: the one being worked on is
    // delivered already, so after the restart (claude --resume, idle
    // again) only the next goes.
    env.ok(&["queue", "add", "agent", "step 4 of the plan"]);
    env.ok(&[
        "queue",
        "add",
        "agent",
        "--when",
        "done",
        "step 5 of the plan",
    ]);
    wait_for("step 4", WAIT, || env.taken(&agent).len() == 4);
    env.ok(&["stop", "--timeout", "10"]);
    let queued: Value =
        serde_json::from_slice(&std::fs::read(env.state.join("state.json")).unwrap()).unwrap();
    let items = queued["queues"][&agent]["items"].as_array().unwrap();
    assert_eq!(
        items.iter().filter(|i| i["state"] == "delivered").count(),
        4
    );
    assert_eq!(items.last().unwrap()["state"], "pending");
    env.ok(&["start", "--port", "0"]);
    wait_for("step 5 after the restart", WAIT, || {
        env.taken(&agent).len() == 5
    });
    let argv_conv =
        std::fs::read_to_string(env.root.join(format!("log/claude-{agent}.conv"))).unwrap();
    assert!(!argv_conv.trim().is_empty(), "resumed its conversation");
    // Let it finish, then make sure nothing comes again.
    wait_for("the last turn", WAIT, || {
        env.agent_status(&agent) != "working"
    });
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(env.taken(&agent), prompts(1..=5));
    let done = env.json(&["queue", "ls", "agent", "--json"]);
    assert_eq!(done["pending"], 0);
    let all = env.json(&["queue", "ls", "--json"]);
    assert_eq!(all.as_array().unwrap().len(), 1);

    // A shell has no queue.
    env.ok(&["new", "--name", "sh", "ws"]);
    let shell = env.fw(&["queue", "add", "sh", "hi"]);
    assert!(!shell.status.success());
    assert!(
        String::from_utf8_lossy(&shell.stderr).contains("Claude Code and Codex"),
        "{shell:?}"
    );
}
