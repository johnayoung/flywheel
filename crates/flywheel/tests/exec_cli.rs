//! `fw exec` end to end: the real binary driving a scripted fake
//! claude-code CLI (via `--command`) that emits valid stream-json.

#![cfg(unix)]

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A fake `claude --output-format stream-json`: records its argv (NUL
/// separated) and the first stdin line (the stream-json user message),
/// then plays `$FAKE_SCENARIO`.
const FAKE_AGENT: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\0' "$a"; done > "$FAKE_DIR/argv"
IFS= read -r FIRST_LINE || true
printf '%s\n' "$FIRST_LINE" > "$FAKE_DIR/stdin"
emit() { printf '%s\n' "$1"; }
init() { emit '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'; }
text() { emit '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$1"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'; }
case "$FAKE_SCENARIO" in
  ok)
    init
    text "fake says hi"
    emit '{"type":"result","subtype":"success","is_error":false,"result":"fake says hi","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'
    ;;
  error)
    init
    emit '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"","num_turns":1}'
    ;;
  silent)
    init
    ;;
  tree)
    init
    (trap '' TERM; exec sleep 300) &
    echo $! > "$FAKE_DIR/grandchild.pid"
    sleep 300 &
    wait $!
    ;;
esac
"#;

struct Fake {
    dir: tempfile::TempDir,
    script: PathBuf,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude.sh");
        std::fs::write(&script, FAKE_AGENT).unwrap();
        Self { dir, script }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Run `fw exec --command /bin/sh --command <fake> <args>` in `cwd`.
    fn exec(&self, scenario: &str, args: &[&str], stdin: Option<&str>, cwd: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fw"));
        command
            .arg("exec")
            .args(["--command", "/bin/sh", "--command"])
            .arg(&self.script)
            .args(args)
            .current_dir(cwd)
            .env("FAKE_DIR", self.path())
            .env("FAKE_SCENARIO", scenario)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn fw");
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(input.as_bytes()).unwrap();
        }
        child.wait_with_output().expect("wait fw")
    }

    fn argv(&self) -> Vec<String> {
        let raw = std::fs::read(self.path().join("argv")).expect("fake never ran");
        // Every arg is NUL-terminated: drop the empty tail, keep empty args.
        let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
        raw.split(|b| *b == 0)
            .map(|part| String::from_utf8(part.to_vec()).unwrap())
            .collect()
    }

    fn stdin_line(&self) -> Value {
        let raw = std::fs::read_to_string(self.path().join("stdin")).unwrap();
        serde_json::from_str(raw.trim()).unwrap()
    }
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON value ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// The flag's value: the argv element right after `flag`.
fn value_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .map(String::as_str)
}

fn prompt_text(message: &Value) -> String {
    message.to_string()
}

#[test]
fn json_mode_round_trips_the_completed_run() {
    let fake = Fake::new();
    let output = fake.exec("ok", &["--json", "say hi"], None, fake.path());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let run = stdout_json(&output);
    assert_eq!(run["final_text"], "fake says hi");
    assert_eq!(run["stop"]["reason"], "completed");
    assert_eq!(run["stop"]["is_error"], false);
    assert_eq!(run["stop"]["finished"], true);
    assert_eq!(run["total_cost_usd"], 0.01);
    assert!(run.get("failure").is_none());
    assert!(prompt_text(&fake.stdin_line()).contains("say hi"));
}

#[test]
fn text_mode_prints_only_the_final_text() {
    let fake = Fake::new();
    let output = fake.exec("ok", &["say hi"], None, fake.path());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "fake says hi\n");
}

#[test]
fn prompt_comes_from_stdin_for_dash_and_when_omitted() {
    for args in [&["--json", "-"][..], &["--json"]] {
        let fake = Fake::new();
        let output = fake.exec("ok", args, Some("piped prompt text"), fake.path());
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert!(
            prompt_text(&fake.stdin_line()).contains("piped prompt text"),
            "{args:?}"
        );
    }
}

#[test]
fn flags_options_and_passthrough_reach_the_agent_argv() {
    let fake = Fake::new();
    let output = fake.exec(
        "ok",
        &[
            "--json",
            "--model",
            "claude-fake-1",
            "--max-turns",
            "4",
            "--system-prompt",
            "be terse",
            "--option",
            r#"allowed_tools=["Read","Grep"]"#,
            "--option",
            "effort=high",
            "-",
            "--",
            "--tools",
            "",
            "--json-schema",
            r#"{"type":"object"}"#,
        ],
        Some("p"),
        fake.path(),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let argv = fake.argv();
    assert_eq!(value_after(&argv, "--model"), Some("claude-fake-1"));
    assert_eq!(value_after(&argv, "--max-turns"), Some("4"));
    assert_eq!(value_after(&argv, "--system-prompt"), Some("be terse"));
    // JSON list option, rendered by the adapter as a comma list.
    assert!(
        value_after(&argv, "--allowedTools").is_some_and(|v| v.starts_with("Read,Grep")),
        "{argv:?}"
    );
    // Non-JSON value fell back to a plain string.
    assert_eq!(value_after(&argv, "--effort"), Some("high"));
    // `--` passthrough, empty-string arg intact.
    assert_eq!(value_after(&argv, "--tools"), Some(""));
    assert_eq!(
        value_after(&argv, "--json-schema"),
        Some(r#"{"type":"object"}"#)
    );
    // The agent runs in the requested working directory.
    let cwd = fake.path().canonicalize().unwrap();
    assert_eq!(value_after(&argv, "--add-dir"), Some(cwd.to_str().unwrap()));
}

#[test]
fn explicit_cwd_is_where_the_agent_runs() {
    let fake = Fake::new();
    let work = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let cwd = work.path().to_str().unwrap();
    let output = fake.exec("ok", &["--cwd", cwd, "p"], None, elsewhere.path());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let canonical = work.path().canonicalize().unwrap();
    assert_eq!(
        value_after(&fake.argv(), "--add-dir"),
        Some(canonical.to_str().unwrap())
    );
}

#[test]
fn agent_error_exits_one_and_still_prints_the_run() {
    let fake = Fake::new();
    let output = fake.exec("error", &["--json", "p"], None, fake.path());
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let run = stdout_json(&output);
    assert_eq!(run["stop"]["is_error"], true);
    assert!(!output.stderr.is_empty());
}

#[test]
fn missing_result_envelope_is_a_failure() {
    let fake = Fake::new();
    let output = fake.exec("silent", &["--json", "p"], None, fake.path());
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(stdout_json(&output)["failure"]["error_type"], "no_result");
}

#[test]
fn missing_agent_executable_exits_one_with_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fw"))
        .args(["exec", "--json", "--command"])
        .arg(tmp.path().join("no-such-agent"))
        .arg("p")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let run = stdout_json(&output);
    assert!(run["failure"]["error_type"].is_string(), "{run}");
}

#[test]
fn usage_errors_exit_two_with_empty_stdout() {
    let fake = Fake::new();
    let cases: [(&[&str], Option<&str>); 9] = [
        (&["--agent", "no-such-agent", "p"], None),
        (&["--option", "novalue", "p"], None),
        (&["--option", "=1", "p"], None),
        (&["--max-turns", "many", "p"], None),
        (&["--timeout", "-3", "p"], None),
        (&["--cwd", "/definitely/not/a/dir", "p"], None),
        (&["--json"], Some("   \n")),
        (
            &["--system-prompt", "a", "--option", "system_prompt=b", "p"],
            None,
        ),
        (&["--no-such-flag", "p"], None),
    ];
    for (args, stdin) in cases {
        let output = fake.exec("ok", args, stdin, fake.path());
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: stdout not empty");
        assert!(!output.stderr.is_empty(), "{args:?}: no diagnostic");
    }
    assert!(
        !fake.path().join("argv").exists(),
        "a usage error must not start the agent"
    );
}

/// Gone, or a zombie awaiting its reaper (Linux `/proc`).
fn process_gone(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.split_whitespace().next() == Some("Z")),
        Err(_) => true,
    }
}

#[cfg(target_os = "linux")]
#[test]
fn timeout_kills_the_whole_agent_process_group() {
    let fake = Fake::new();
    let started = Instant::now();
    let output = fake.exec(
        "tree",
        &["--json", "--timeout", "1", "p"],
        None,
        fake.path(),
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(started.elapsed() < Duration::from_secs(30));
    let run = stdout_json(&output);
    assert_eq!(run["failure"]["error_type"], "timeout");
    let pid = std::fs::read_to_string(fake.path().join("grandchild.pid")).unwrap();
    let pid = pid.trim();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_gone(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        process_gone(pid),
        "TERM-ignoring grandchild {pid} outlived fw exec"
    );
}

#[test]
fn exec_never_creates_flywheel_state() {
    let fake = Fake::new();
    let work = tempfile::tempdir().unwrap();
    assert!(!work.path().join("flywheel.toml").exists());
    let output = fake.exec("ok", &["--json", "p"], None, work.path());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let entries: Vec<_> = std::fs::read_dir(work.path()).unwrap().collect();
    assert!(
        entries.is_empty(),
        "fw exec wrote into the cwd: {entries:?}"
    );
    assert!(!work.path().join(".flywheel").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn sigterm_cancels_the_run_and_kills_the_agent_group() {
    let fake = Fake::new();
    let child = Command::new(env!("CARGO_BIN_EXE_fw"))
        .arg("exec")
        .args(["--command", "/bin/sh", "--command"])
        .arg(&fake.script)
        .args(["--json", "p"])
        .current_dir(fake.path())
        .env("FAKE_DIR", fake.path())
        .env("FAKE_SCENARIO", "tree")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid_file = fake.path().join("grandchild.pid");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !pid_file.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(100));
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(stdout_json(&output)["failure"]["error_type"], "cancelled");
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let pid = pid.trim();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_gone(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        process_gone(pid),
        "grandchild {pid} outlived a SIGTERMed fw exec"
    );
}
