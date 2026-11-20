use super::*;

fn started(name: &str, input: serde_json::Value) -> String {
    serde_json::json!({"kind": "tool.started", "payload": {"payload": {"tool_name": name, "tool_input": input}}}).to_string()
}

fn transcript(lines: &[String]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("run.jsonl"), lines.join("\n")).unwrap();
    dir
}

#[test]
fn inspection_needs_a_named_read_and_a_build_or_test() {
    let relevant = vec!["crates/x/src/lib.rs".to_owned()];
    let both = transcript(&[
        started(
            "Read",
            serde_json::json!({"file_path": "/w/crates/x/src/lib.rs"}),
        ),
        started("Bash", serde_json::json!({"command": "cargo test -p x"})),
    ]);
    assert!(inspected(both.path(), &relevant).unwrap().0);
    let no_test = transcript(&[started(
        "Read",
        serde_json::json!({"file_path": "/w/crates/x/src/lib.rs"}),
    )]);
    let (ok, why) = inspected(no_test.path(), &relevant).unwrap();
    assert!(!ok && why.contains("never built or tested"));
    let wrong_file = transcript(&[
        started("Read", serde_json::json!({"file_path": "/w/README.md"})),
        started("Bash", serde_json::json!({"command": "cargo build"})),
    ]);
    assert!(!inspected(wrong_file.path(), &relevant).unwrap().0);
    let nothing = transcript(&[]);
    assert!(
        !inspected(nothing.path(), &[]).unwrap().0,
        "giving up is not inspecting"
    );
}

#[test]
fn relevant_paths_take_each_entrys_first_word() {
    let task = r#"{"context": {"relevant": ["crates/a.rs -- the thing", "@EVAL_PROJECT_HOME@/specs/s.md -- spec", "docs/x.md"]}}"#;
    assert_eq!(relevant_paths(task), ["crates/a.rs", "docs/x.md"]);
}

#[test]
fn the_no_change_command_passes_only_on_the_overlay_commit() {
    let repo = crate::testutil::TempRepo::new();
    let base = repo.commit("base", &[("a", "1")]);
    let cmd = no_change_command(&base);
    let run = |cmd: &str| {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    };
    assert!(run(&cmd), "at the base");
    repo.commit(
        "eval: variant overlay v\n\nFlywheel-Eval-Variant: x",
        &[("CLAUDE.md", "x")],
    );
    assert!(run(&cmd), "the overlay alone is no change");
    repo.commit("feat: the agent changed something", &[("a", "2")]);
    assert!(!run(&cmd));
}
