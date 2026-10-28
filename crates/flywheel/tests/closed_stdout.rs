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
