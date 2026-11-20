//! One `fw eval` trial end to end through the real binary: a frozen set
//! task (a.txt must say 2), a variant that strips a CLAUDE.md block, a
//! scripted `claude` on PATH that does the work and commits it, the real
//! worker pipeline cutting from and landing on the trial branch, and the
//! oracle grading the landed commit out of band. Also: the agent's
//! Claude Code per-path state is copied out and removed, and a trial whose
//! agent does nothing is graded as a fail, not an error.

#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};

use support::{fw_confined, git, project_id_of, project_repo};

/// The agent: records its argv, leaves auto-memory where Claude Code
/// would (`$CLAUDE_CONFIG_DIR/projects/<key of $PWD>/memory/`), makes
/// a.txt say 2 unless told to idle, commits, and ends with `verify`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi
case "$*" in *--tools*)
  T='<!-- EVAL_REVIEW -->{\"verdict\":\"fake_pass\",\"confidence\":\"high\",\"evidence\":[{\"at\":\"final.diff\",\"quote\":\"+2\"}],\"notes\":\"hard-coded\"}<!-- /EVAL_REVIEW -->'
  printf '%s\n' '{"type":"system","subtype":"init","session_id":"r","model":"claude-haiku-fake"}'
  printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$T"'","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":1,"output_tokens":1}}'
  exit 0 ;;
esac
if [ "$2" = "canary" ]; then
  case "$*" in
    *setting-sources=user*)
      touch "$CLAUDE_CONFIG_DIR/../hook-fired"
      echo '{"type":"system","subtype":"init","skills":["fw-eval-canary"],"mcp_servers":[{"name":"fw-eval-canary"}]}' ;;
    *) echo '{"type":"system","subtype":"init","skills":[],"mcp_servers":[]}' ;;
  esac
  exit 0
fi
echo "$*" >> "$HOME/claude-argv.log"
key=$(printf '%s' "$PWD" | sed 's/[^a-zA-Z0-9]/-/g')
mkdir -p "$CLAUDE_CONFIG_DIR/projects/$key/memory"
echo "remembered" > "$CLAUDE_CONFIG_DIR/projects/$key/memory/MEMORY.md"
if [ ! -e "$HOME/idle" ]; then
  echo 2 > a.txt
  if [ -e "$HOME/act" ]; then echo changed > b.txt; fi
  git add -A >/dev/null 2>&1 && git commit -qm "feat: two" >/dev/null 2>&1
fi
E='<!-- LOOP_STATUS -->\n{\"intent\":\"verify\",\"reason\":\"done\"}\n<!-- /LOOP_STATUS -->'
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-fake","model":"claude-fake-1"}'
if [ -e "$HOME/peek" ]; then
  p=$(cat "$HOME/peek")
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cat '"$p"'"}}],"usage":{"input_tokens":1,"output_tokens":1}}}'
  printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"x","is_error":false}]}}'
fi
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$E"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"'"$E"'","num_turns":3,"total_cost_usd":0.25,"usage":{"input_tokens":7,"output_tokens":2}}'
"#;

fn ok(out: &std::process::Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct World {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    fly: PathBuf,
    repo: PathBuf,
    project: PathBuf,
}

/// A repo whose task `two` landed (base: a.txt = 1, CLAUDE.md with a
/// RULE block; reference: a.txt = 2), mined, validated, and frozen.
fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let fly = root.join("fly");
    let repo = root.join("repo");
    let project = project_repo(&repo, &home, &fly);
    std::fs::write(repo.join("CLAUDE.md"), "# rules\nRULE: skip check.sh\n").unwrap();
    std::fs::write(repo.join("a.txt"), "1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base", "--no-gpg-sign"]);
    std::fs::write(repo.join("a.txt"), "2\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "commit",
            "-qm",
            "feat: two\n\nFlywheel-Run: run-1",
            "--no-gpg-sign",
        ],
    );
    let landed = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let id = project_id_of(&project);
    let phase = project.join("tasks/archive/01-x");
    std::fs::create_dir_all(&phase).unwrap();
    std::fs::write(
        phase.join("two.json"),
        r#"{"id": "two", "goal": "make a.txt say 2", "graders": [{"type": "command", "run": "true"}]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project.join("verification")).unwrap();
    std::fs::write(
        project.join("verification/oracle-two.sh"),
        "test \"$(cat a.txt)\" = 2\n",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("held-out")).unwrap();
    std::fs::write(
        project.join("held-out/two.json"),
        format!(
            r#"{{"graders": [{{"type": "command", "name": "oracle", "run": "sh {}/verification/oracle-two.sh"}}]}}"#,
            project.display()
        ),
    )
    .unwrap();
    support::seed_landed_run(
        &project.join("flywheel.sqlite"),
        &id,
        "two",
        "run-1",
        &landed,
    );
    ok(&fw_confined(
        &repo,
        &home,
        &fly,
        &["eval", "set", "build", "core"],
    ));
    ok(&fw_confined(
        &repo,
        &home,
        &fly,
        &[
            "eval",
            "set",
            "validate",
            "core@draft",
            "--no-noise",
            "--timeout",
            "60s",
        ],
    ));
    ok(&fw_confined(
        &repo,
        &home,
        &fly,
        &["eval", "set", "freeze", "core"],
    ));
    std::fs::create_dir_all(home.join("bin")).unwrap();
    support::write_script(&home.join("bin/claude"), FAKE_CLAUDE);
    let variants = project.join("evals/variants");
    std::fs::create_dir_all(&variants).unwrap();
    std::fs::write(
        variants.join("rule-off.toml"),
        format!(
            "model = \"fake\"\n[env]\nPATH = \"{}:{}\"\n[[repo]]\npath = \"CLAUDE.md\"\nop = \"strip\"\npattern = '(?m)^RULE: skip check\\.sh\\n'\n",
            home.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    )
    .unwrap();
    World {
        _tmp: tmp,
        home,
        fly,
        repo,
        project,
    }
}

fn trial_dir(project: &Path) -> PathBuf {
    let runs = project.join("evals/runs");
    let run = std::fs::read_dir(&runs)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    std::fs::read_dir(run.join("trials"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path()
}

#[test]
fn a_trial_runs_the_real_worker_from_the_base_and_is_graded_out_of_band() {
    let w = world();
    let out = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "trial",
            "core@v1",
            "two",
            "--variant",
            "rule-off",
            "--worker-timeout",
            "120s",
        ],
    ));
    let outcome: serde_json::Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(outcome["status"], "done", "{outcome}");
    assert_eq!(outcome["oracle"], "pass", "{outcome}");
    assert_eq!(outcome["final_where"], "landed", "{outcome}");
    assert_eq!(outcome["flywheel_status"], "done", "{outcome}");
    assert!(outcome["cost_usd"].as_f64().unwrap() > 0.0, "{outcome}");

    let dir = trial_dir(&w.project);
    let overlay: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("overlay.json")).unwrap()).unwrap();
    assert_eq!(overlay["effects"][0]["effect"], "changed", "{overlay}");
    // The agent ran with no user-level layer and the variant's mode.
    let argv = std::fs::read_to_string(w.home.join("claude-argv.log")).unwrap();
    assert!(argv.contains("--setting-sources=project,local"), "{argv}");
    // Its per-path Claude Code state was kept as an artifact, then removed.
    let leftover: Vec<_> = std::fs::read_dir(w.home.join("claude-config/projects"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(leftover.is_empty(), "{leftover:?}");
    let kept = support::files_named(&dir.join("claude-projects"), "MEMORY.md");
    assert_eq!(kept.len(), 1, "{kept:?}");
    // The working directories are gone; the final commit is kept.
    assert!(!dir.join("work").exists());
    let results = dir.parent().unwrap().parent().unwrap().join("results.git");
    let trial_id = dir.file_name().unwrap().to_string_lossy().into_owned();
    let kept_commit = git(&results, &["rev-parse", &format!("refs/trials/{trial_id}")]);
    assert_eq!(
        kept_commit.trim(),
        outcome["final_commit"].as_str().unwrap()
    );
    // No trial history reached the operator's repo or its memory.
    assert!(
        git(&w.repo, &["branch", "--list", "eval/*"])
            .trim()
            .is_empty()
    );
}

#[test]
fn an_agent_that_does_nothing_fails_the_oracle_it_is_not_an_error() {
    let w = world();
    std::fs::write(w.home.join("idle"), "").unwrap();
    let out = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "trial",
            "core@v1",
            "two",
            "--variant",
            "rule-off",
            "--worker-timeout",
            "120s",
        ],
    ));
    let outcome: serde_json::Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(outcome["status"], "done", "{outcome}");
    assert_eq!(outcome["oracle"], "fail", "{outcome}");
}

#[test]
fn a_replay_pauses_at_its_budget_and_a_resume_finishes_every_cell() {
    let w = world();
    let variants = w.project.join("evals/variants");
    let off = std::fs::read_to_string(variants.join("rule-off.toml")).unwrap();
    std::fs::write(
        variants.join("rule-on.toml"),
        off.replace("op = \"strip\"", "op = \"ensure\"").replace(
            "pattern = '(?m)^RULE: skip check\\.sh\\n'",
            "pattern = '(?m)^RULE: skip check'\nfrom = \"rule.md\"",
        ),
    )
    .unwrap();
    std::fs::write(variants.join("rule.md"), "RULE: skip check.sh\n").unwrap();
    let est = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "estimate",
            "core",
            "--variant",
            "rule-on",
            "--variant",
            "rule-off",
            "-k",
            "2",
        ],
    ));
    assert!(est.contains("4 trials: $9.00 expected"), "{est}");
    let paused = fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "replay",
            "core",
            "--variant",
            "rule-on",
            "--variant",
            "rule-off",
            "-k",
            "2",
            "--parallel",
            "2",
            "--budget",
            "1",
            "--worker-timeout",
            "120s",
        ],
    );
    assert_eq!(
        paused.status.code(),
        Some(75),
        "{}",
        String::from_utf8_lossy(&paused.stderr)
    );
    let stderr = String::from_utf8_lossy(&paused.stderr);
    assert!(
        stderr.contains("paused at the budget") && stderr.contains("--resume"),
        "{stderr}"
    );
    let run_id = stderr
        .split("--resume ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let done = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "replay", "--resume", &run_id, "--budget", "100"],
    ));
    assert!(done.contains(&format!("run {run_id} done")), "{done}");
    let runs = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "runs", "--json"],
    ));
    let runs: serde_json::Value = serde_json::from_str(&runs).unwrap();
    let run = &runs["runs"][0];
    assert_eq!(
        (run["done"].as_u64(), run["total"].as_u64()),
        (Some(4), Some(4)),
        "{run}"
    );
    assert_eq!(run["status"], "done");
    let st = flywheel_eval::store::EvalStore::open_sqlite(
        &w.project.join("flywheel.sqlite"),
        &project_id_of(&w.project),
    )
    .unwrap();
    for t in st.trials(&run_id).unwrap() {
        assert_eq!(t.oracle.as_deref(), Some("pass"), "{t:?}");
    }
    assert!(
        (run["spent_usd"].as_f64().unwrap() - 1.0).abs() < 1e-6,
        "{run}"
    );
    let report = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "report", &run_id, "--per-task"],
    ));
    assert!(
        report.contains("rule-off vs rule-on (paired by task):"),
        "{report}"
    );
    assert!(
        report.contains("no information"),
        "every trial passed: {report}"
    );
    let json = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "report", &run_id, "--json"],
    ));
    let r: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(r["schema"], 1);
    assert_eq!(r["saturated_tasks"][0], "two");
    assert_eq!(r["overall"][0]["pass_hat_k"], 1.0);
}

#[test]
fn an_agent_that_reads_the_held_out_oracle_is_contaminated_and_fails() {
    let w = world();
    std::fs::write(
        w.home.join("peek"),
        w.project.join("held-out/two.json").display().to_string(),
    )
    .unwrap();
    let out = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "trial",
            "core@v1",
            "two",
            "--variant",
            "rule-off",
            "--worker-timeout",
            "120s",
        ],
    ));
    let outcome: serde_json::Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(outcome["oracle"], "pass", "it did the work: {outcome}");
    assert_eq!(outcome["validity"], "contaminated", "{outcome}");
    let trace: serde_json::Value =
        serde_json::from_str(outcome["trace_json"].as_str().unwrap()).unwrap();
    assert_eq!(trace["tool_calls"], 1, "{trace}");
    assert!(
        trace["reads_outside_trial"][0]
            .as_str()
            .unwrap()
            .contains("held-out/two.json")
    );
}

/// `fw` with the scripted agent first on PATH (the reader runs `claude`).
fn fw_with_agent(w: &World, args: &[&str]) -> std::process::Output {
    let mut command = std::process::Command::new(support::fw_under_test());
    command
        .args(args)
        .current_dir(&w.repo)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    support::confine(&mut command, &w.home, &w.fly, &[&w.home.join("bin")]);
    command.output().unwrap()
}

#[test]
fn a_readers_fake_pass_makes_a_pass_suspect_and_only_the_operator_voids_it() {
    let w = world();
    let out = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "trial",
            "core@v1",
            "two",
            "--variant",
            "rule-off",
            "--worker-timeout",
            "120s",
        ],
    ));
    let run_id = out
        .split(" of run ")
        .nth(1)
        .unwrap()
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    let trial = out
        .split("trial ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let reviewed = ok(&fw_with_agent(&w, &["eval", "review", &run_id]));
    assert!(
        reviewed.contains("fake_pass") && reviewed.contains("suspect"),
        "{reviewed}"
    );
    let report = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "report", &run_id, "--json"],
    ));
    let r: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(r["tasks"][0]["marks"]["rule-off"][0]["mark"], "suspect");
    assert_eq!(
        r["overall"][0]["success"], 1.0,
        "a suspect pass counts until decided"
    );
    ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "decide", &run_id, &trial, "--confirm"],
    ));
    let report = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "report", &run_id, "--json"],
    ));
    let r: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(r["tasks"][0]["marks"]["rule-off"][0]["mark"], "voided");
    assert_eq!(r["overall"][0]["success"], 0.0);
    let again = fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "decide", &run_id, &trial, "--clear"],
    );
    assert!(!again.status.success(), "only a suspect pass is decided");
}

#[test]
fn a_dont_act_twin_fails_an_agent_that_changes_things_and_one_that_never_looks() {
    let w = world();
    // The world froze core@v1; build the twin on a fresh draft of `core`.
    ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "set", "build", "core"],
    ));
    ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "set",
            "validate",
            "core@draft",
            "--no-noise",
            "--timeout",
            "60s",
        ],
    ));
    let added = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "set", "dont-act", "core", "two"],
    ));
    assert!(added.contains("+ two-dont-act"), "{added}");
    let validated = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "set",
            "validate",
            "core@draft",
            "--only",
            "two-dont-act",
            "--no-noise",
            "--timeout",
            "60s",
        ],
    ));
    assert!(validated.contains("ok   two-dont-act"), "{validated}");
    ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "set", "freeze", "core"],
    ));
    // An agent that changes something fails the no-change check.
    std::fs::write(w.home.join("act"), "").unwrap();
    let out = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "trial",
            "core@v2",
            "two-dont-act",
            "--variant",
            "rule-off",
            "--worker-timeout",
            "120s",
        ],
    ));
    let outcome: serde_json::Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(outcome["oracle"], "fail", "it changed things: {outcome}");
    std::fs::remove_file(w.home.join("act")).unwrap();
    // An agent that does nothing at all fails for never looking.
    std::fs::write(w.home.join("idle"), "").unwrap();
    let out = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "trial",
            "core@v2",
            "two-dont-act",
            "--variant",
            "rule-off",
            "--worker-timeout",
            "120s",
        ],
    ));
    let outcome: serde_json::Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(outcome["oracle"], "fail", "never looked: {outcome}");
    let dir = trial_dir_of(&w.project, outcome["trial_id"].as_str().unwrap());
    let checks = std::fs::read_to_string(dir.join("checks.json")).unwrap();
    assert!(
        checks.contains("\"inspected\"") && checks.contains("neither read"),
        "{checks}"
    );
}

fn trial_dir_of(project: &Path, trial: &str) -> PathBuf {
    support::files_named(&project.join("evals/runs"), "trial.json")
        .into_iter()
        .map(|p| p.parent().unwrap().to_path_buf())
        .find(|d| d.file_name().unwrap().to_string_lossy() == trial)
        .unwrap()
}

#[test]
fn calibration_sets_each_tasks_tier_from_the_baseline() {
    let w = world();
    ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "set", "build", "core"],
    ));
    let unvalidated = fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "set",
            "calibrate",
            "core",
            "--variant",
            "rule-off",
            "-k",
            "2",
            "--budget",
            "10",
        ],
    );
    assert!(String::from_utf8_lossy(&unvalidated.stderr).contains("not validated"));
    ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "set",
            "validate",
            "core@draft",
            "--no-noise",
            "--timeout",
            "60s",
        ],
    ));
    let out = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &[
            "eval",
            "set",
            "calibrate",
            "core",
            "--variant",
            "rule-off",
            "-k",
            "2",
            "--budget",
            "10",
            "--worker-timeout",
            "120s",
        ],
    ));
    assert!(out.contains("two") && out.contains("regression"), "{out}");
    ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "set", "freeze", "core"],
    ));
    let shown = ok(&fw_confined(
        &w.repo,
        &w.home,
        &w.fly,
        &["eval", "set", "show", "core@v2", "--json"],
    ));
    let m: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(m["tasks"][0]["tier"], "regression");
}
