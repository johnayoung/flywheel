//! A read-only verb piped into a reader that exits early (`fw status | head`)
//! ends quietly instead of printing `error: Broken pipe`.

use std::process::{Command, Stdio};

#[cfg(unix)]
#[test]
fn read_only_verb_exits_quietly_when_stdout_closes() {
    let bin = env!("CARGO_BIN_EXE_fw");
    let tmp = tempfile::tempdir().unwrap();
    let phase = tmp.path().join("tasks/active/01-many");
    std::fs::create_dir_all(&phase).unwrap();
    // Enough rows that `status` keeps writing well past the pipe buffer
    // after `head` has exited, so a write must hit the closed pipe.
    for i in 0..4000 {
        let id = format!("task-with-a-long-identifier-{i:05}");
        std::fs::write(
            phase.join(format!("{id}.json")),
            format!(r#"{{"id": "{id}", "goal": "g", "graders": [{{"type": "command", "run": "true"}}]}}"#),
        )
        .unwrap();
    }
    let script = format!(
        "'{bin}' status --tasks-dir '{tasks}' --db '{db}' | head -n 1 >/dev/null",
        tasks = tmp.path().join("tasks").display(),
        db = tmp.path().join("fw.sqlite").display(),
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run sh");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.is_empty(), "{stderr}");
}

/// `fw hub` verbs that print a lot, piped into a reader that is gone
/// against a running hub: exit 0 and nothing on stderr (one closed-pipe policy for every
/// verb, `util::quiet_on_closed_pipe`).
#[cfg(target_os = "linux")]
#[test]
fn hub_verbs_exit_quietly_when_stdout_closes() {
    use std::time::{Duration, Instant};
    let bin = env!("CARGO_BIN_EXE_fw");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let state = root.join("state");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let fw = |args: &[&str]| {
        let mut argv = vec!["hub", args[0], "--state-dir", state.to_str().unwrap()];
        argv.extend_from_slice(&args[1..]);
        Command::new(bin)
            .args(&argv)
            .env("SHELL", "/bin/sh")
            .env("HOME", &root)
            .env_remove("XDG_STATE_HOME")
            .env_remove("INVOCATION_ID")
            .env_remove("FW_HUB_TOKEN")
            .output()
            .unwrap()
    };
    struct Stop<'a>(&'a dyn Fn(&[&str]) -> std::process::Output);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = (self.0)(&["stop", "--force", "--timeout", "10"]);
        }
    }
    let started = fw(&["start", "--port", "0"]);
    assert!(started.status.success(), "{started:?}");
    let _stop = Stop(&fw);
    // Two shells that printed thousands of matching lines (search shows
    // at most 1000 hits a session).
    let mut ids = Vec::new();
    for name in ["big", "big2"] {
        let made = fw(&[
            "new",
            name,
            "--create",
            "--folder",
            repo.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            "i=0; while [ $i -lt 4000 ]; do echo \"needle-$i-padding-padding-padding-padding-padding-padding-padding\"; \
             i=$((i+1)); done; echo printed-all; sleep 600",
        ]);
        assert!(made.status.success(), "{made:?}");
        ids.push(String::from_utf8(made.stdout).unwrap().trim().to_owned());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    for id in &ids {
        loop {
            let read = fw(&["read", id]);
            if String::from_utf8_lossy(&read.stdout).contains("printed-all") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the shell never finished printing"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    // The reader exits without reading anything, so a verb printing more
    // than a pipe holds (64 KiB) must hit the closed pipe, however the
    // processes are scheduled. (`head -n 1` would read an unknown amount
    // first: a verb whose output fit in that plus the pipe never saw it
    // closed.)
    const PAST_THE_PIPE: usize = 2 * 64 * 1024;
    let state_arg = state.display().to_string();
    let read = format!("read {} --source recent --lines 4000 --json", ids[0]);
    for (verb, big) in [
        ("search needle --limit 2000 --per-session 1000", true),
        ("search needle --limit 2000 --per-session 1000 --json", true),
        (read.as_str(), true),
        // Short here; it shares the closed-pipe policy all the same.
        ("activity --limit 500 --json", false),
    ] {
        let (name, rest) = verb.split_once(' ').unwrap();
        if big {
            let mut argv = vec![name, "--state-dir", state_arg.as_str()];
            argv.extend(rest.split(' '));
            let all = Command::new(bin)
                .arg("hub")
                .args(&argv)
                .env("HOME", &root)
                .env_remove("XDG_STATE_HOME")
                .env_remove("FW_HUB_TOKEN")
                .output()
                .unwrap();
            assert!(
                all.stdout.len() > PAST_THE_PIPE,
                "fw hub {verb} prints only {} bytes: not past the pipe",
                all.stdout.len()
            );
        }
        let script = format!(
            "'{bin}' hub {name} --state-dir '{state_arg}' {rest} | true; \
             exit \"${{PIPESTATUS[0]}}\""
        );
        let output = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("HOME", &root)
            .env_remove("XDG_STATE_HOME")
            .env_remove("FW_HUB_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("run bash");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.is_empty(), "fw hub {verb}: {stderr}");
        assert_eq!(output.status.code(), Some(0), "fw hub {verb}");
    }
}
