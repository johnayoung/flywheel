//! Codex adapter contract suite through the real stack plus `build_cli_plan`
//! flag mapping, and the design-doc section 18 agent-swap acceptance test.

mod common;

use common::{ScenarioBuilder, fake_claude, fake_codex};
use flywheel_agents::codex::build_cli_plan;
use flywheel_agents::{
    AgentConfiguration, AgentRuntime, AuthenticationPolicy, CompletedRun, EventType, MemorySink,
    PermissionPolicy, RunOptions, RunRequest, StdinPlan, StopReason,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

async fn drive(builder: ScenarioBuilder) -> (CompletedRun, MemorySink) {
    let sink = MemorySink::new();
    let run = AgentRuntime::new()
        .run(
            builder.request(),
            RunOptions::default().with_sink(Arc::new(sink.clone())),
        )
        .await
        .unwrap();
    (run, sink)
}

#[tokio::test]
async fn happy_path_fold() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::codex(
        fake_codex(dir.path()),
        "happy",
        dir.path().to_path_buf(),
    ))
    .await;
    let started = sink.events_of(EventType::SessionStarted);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].str_field("session_id"), Some("codex-1"));
    assert_eq!(run.final_text, "Hello from codex. All done.");
    assert_eq!(run.stop.reason, StopReason::Completed);
    assert_eq!(run.stop.native.as_deref(), Some("turn.completed"));
    assert!(!run.stop.is_error);
    assert!(!run.stop.pending_tool_use);
    let usage = run.usage.clone().unwrap();
    assert_eq!(usage["input_tokens"], 11);
    assert_eq!(usage["output_tokens"], 3);
    assert_eq!(usage["cache_read_input_tokens"], 4);
    assert_eq!(usage["cache_creation_input_tokens"], 0);
    assert_eq!(run.num_turns, Some(1));
    assert_eq!(run.total_cost_usd, None);
    assert_eq!(run.native_session_id.as_deref(), Some("codex-1"));
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(run.fault.is_none());
    assert_eq!(run.exit.returncode, Some(0));
    assert_eq!(run.tool_interactions.len(), 2);
    let command = &run.tool_interactions[0];
    assert_eq!(command.tool_use_id, "cmd-1");
    assert_eq!(command.tool_name, "command_execution");
    assert_eq!(command.tool_input.get("command"), Some(&json!("echo hi")));
    assert_eq!(command.result.as_ref().unwrap().is_error, Some(false));
    assert_eq!(
        command.result.as_ref().unwrap().content,
        Some(json!("hi\n"))
    );
    let file_change = &run.tool_interactions[1];
    assert_eq!(file_change.tool_use_id, "fc-1");
    assert_eq!(file_change.tool_name, "file_change");
    assert_eq!(file_change.result.as_ref().unwrap().is_error, Some(false));
    let changed = sink.events_of(EventType::FileChanged);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].str_field("path"), Some("src/app.py"));
    assert_eq!(sink.events_of(EventType::PlanUpdated).len(), 1);
}

#[tokio::test]
async fn happy_path_sequences_and_sources() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::codex(
        fake_codex(dir.path()),
        "happy",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none());
    let raw = sink.raw();
    let events = sink.events();
    let mut merged: Vec<u64> = raw
        .iter()
        .map(|e| e.sequence)
        .chain(events.iter().map(|e| e.sequence))
        .collect();
    merged.sort_unstable();
    assert_eq!(merged, (1..=merged.len() as u64).collect::<Vec<_>>());
    assert!(events.iter().all(|e| e.source == "codex"));
    assert!(
        raw.iter()
            .all(|e| e.adapter_id == "codex" && e.stream == "stdout")
    );
}

#[tokio::test]
async fn tool_error_reported_on_interaction() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::codex(
        fake_codex(dir.path()),
        "tool_error",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none());
    assert_eq!(run.tool_interactions.len(), 1);
    assert_eq!(run.tool_interactions[0].tool_use_id, "cmd-err");
    assert_eq!(
        run.tool_interactions[0].result.as_ref().unwrap().is_error,
        Some(true)
    );
}

#[tokio::test]
async fn turn_failed_is_error_stop_without_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::codex(
        fake_codex(dir.path()),
        "turn_failed",
        dir.path().to_path_buf(),
    ))
    .await;
    assert_eq!(run.stop.reason, StopReason::Error);
    assert!(run.stop.is_error);
    assert_eq!(run.stop.native.as_deref(), Some("turn.failed"));
    assert!(run.failure.is_none());
    assert_eq!(run.exit.returncode, Some(0));
    assert!(
        sink.events_of(EventType::Error)
            .iter()
            .any(|e| e.str_field("message") == Some("model stream disconnected"))
    );
}

#[tokio::test]
async fn malformed_lines_become_raw_and_run_completes() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::codex(
        fake_codex(dir.path()),
        "malformed",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none());
    assert_eq!(run.stop.reason, StopReason::Completed);
    assert_eq!(run.final_text, "before garbage after garbage");
    let raw = sink.events_of(EventType::Raw);
    assert!(raw.len() >= 3);
    assert!(
        raw.iter()
            .any(|e| e.native_type.as_deref() == Some("mystery"))
    );
    assert!(
        raw.iter()
            .any(|e| e.native_type.as_deref() == Some("item.completed"))
    );
}

#[tokio::test]
async fn crash_without_turn_completed_is_agent_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::codex(
        fake_codex(dir.path()),
        "crash",
        dir.path().to_path_buf(),
    ))
    .await;
    let failure = run.failure.as_ref().unwrap();
    assert_eq!(failure.error_type, "agent_exit");
    assert_eq!(failure.exit_code, Some(3));
    assert!(
        failure
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("fatal explosion")
    );
    assert_eq!(run.stop.reason, StopReason::Unknown);
}

#[tokio::test]
async fn working_directory_and_environment_respected() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::codex(
        fake_codex(dir.path()),
        "cwd",
        dir.path().to_path_buf(),
    ))
    .await;
    assert_eq!(
        std::fs::canonicalize(&run.final_text).unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );
    let (run, _) = drive(
        ScenarioBuilder::codex(fake_codex(dir.path()), "env", dir.path().to_path_buf())
            .env("TEST_MARKER", "marker-123")
            .env("OPENAI_API_KEY", "sk-test"),
    )
    .await;
    assert_eq!(run.final_text, "marker=marker-123; api_key=sk-test");
    let (run, _) = drive(
        ScenarioBuilder::codex(fake_codex(dir.path()), "env", dir.path().to_path_buf())
            .env("TEST_MARKER", "marker-123")
            .env("OPENAI_API_KEY", "sk-test")
            .auth(AuthenticationPolicy::AccountOnly),
    )
    .await;
    assert_eq!(run.final_text, "marker=marker-123; api_key=unset");
}

#[tokio::test]
async fn unknown_transport_rejected_and_registry_lists_both() {
    let dir = tempfile::tempdir().unwrap();
    let request = ScenarioBuilder::codex(fake_codex(dir.path()), "happy", dir.path().to_path_buf())
        .option("transport", json!("sdk"))
        .request();
    let runtime = AgentRuntime::new();
    let err = runtime
        .run(request, RunOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "unsupported_capability");
    assert_eq!(
        runtime.registry().ids(),
        vec!["claude-code".to_string(), "codex".to_string()]
    );
    let options = runtime.discover_options("codex", None).await.unwrap();
    assert!(!options.warnings.is_empty());
    assert!(options.models.is_empty());
    assert!(
        runtime
            .start(
                ScenarioBuilder::codex(fake_codex(dir.path()), "happy", dir.path().to_path_buf())
                    .request(),
                RunOptions::default()
            )
            .await
            .unwrap()
            .control
            .is_none()
    );
}

// --- build_cli_plan flag mapping -------------------------------------------

fn request(configure: impl FnOnce(&mut AgentConfiguration)) -> RunRequest {
    let mut configuration = AgentConfiguration::new("codex");
    configure(&mut configuration);
    RunRequest {
        prompt: "the prompt".into(),
        working_directory: PathBuf::from("/tmp/workdir"),
        configuration,
        timeout_seconds: None,
    }
}

#[test]
fn plan_defaults_supervised_sandbox_and_stdin_prompt() {
    let plan = build_cli_plan(&request(|_| {})).unwrap();
    assert_eq!(
        plan.argv[..3],
        ["codex", "exec", "--json"].map(String::from)
    );
    assert_eq!(
        plan.argv[3..5],
        ["--sandbox", "workspace-write"].map(String::from)
    );
    assert_eq!(plan.argv.last().map(String::as_str), Some("-"));
    assert_eq!(plan.stdin, StdinPlan::Payload("the prompt".into()));
    assert_eq!(plan.cwd, Some(PathBuf::from("/tmp/workdir")));
    assert!(plan.denied_environment.is_empty());
    assert!(!plan.argv.contains(&"--max-turns".to_string()));
}

#[test]
fn plan_permission_policies() {
    let auto = build_cli_plan(&request(|c| c.permission_policy = PermissionPolicy::Auto))
        .unwrap()
        .argv;
    assert!(auto.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    assert!(!auto.contains(&"--sandbox".to_string()));
    let read_only = build_cli_plan(&request(|c| {
        c.permission_policy = PermissionPolicy::ReadOnly
    }))
    .unwrap()
    .argv;
    let index = read_only.iter().position(|a| a == "--sandbox").unwrap();
    assert_eq!(read_only[index + 1], "read-only");
    let err =
        build_cli_plan(&request(|c| c.permission_policy = PermissionPolicy::Plan)).unwrap_err();
    assert_eq!(err.code(), "unsupported_capability");
}

#[test]
fn plan_model_max_turns_extra_args_and_flags() {
    let argv = build_cli_plan(&request(|c| {
        c.model_id = Some("gpt-5-codex".into());
        c.max_turns = Some(5);
        c.adapter_options
            .insert("extra_cli_args".into(), json!(["--profile", "ci"]));
        c.adapter_options
            .insert("skip_git_repo_check".into(), json!(true));
    }))
    .unwrap()
    .argv;
    let index = argv.iter().position(|a| a == "--model").unwrap();
    assert_eq!(argv[index + 1], "gpt-5-codex");
    assert!(!argv.contains(&"--max-turns".to_string()));
    assert!(!argv.contains(&"5".to_string()));
    assert_eq!(
        argv[argv.len() - 4..],
        ["--profile", "ci", "--skip-git-repo-check", "-"].map(String::from)
    );
    assert!(
        !build_cli_plan(&request(|_| {}))
            .unwrap()
            .argv
            .contains(&"--skip-git-repo-check".to_string())
    );
}

#[test]
fn plan_account_only_and_command_override() {
    let plan = build_cli_plan(&request(|c| {
        c.authentication_policy = AuthenticationPolicy::AccountOnly
    }))
    .unwrap();
    assert_eq!(plan.denied_environment, vec!["OPENAI_API_KEY".to_string()]);
    let plan = build_cli_plan(&request(|c| {
        c.command_override = Some(vec!["/usr/bin/env".into(), "fake".into()])
    }))
    .unwrap();
    assert_eq!(
        plan.argv[..4],
        ["/usr/bin/env", "fake", "exec", "--json"].map(String::from)
    );
}

// --- agent swap (design doc section 18) ------------------------------------

/// Agent-agnostic orchestration: no branching on agent_id anywhere.
async fn orchestrate(
    agent_id: &str,
    script: &Path,
    scenario_var: &str,
    scenario: &str,
    workdir: &Path,
) -> CompletedRun {
    let mut configuration = AgentConfiguration::new(agent_id);
    configuration.permission_policy = PermissionPolicy::Auto;
    configuration.command_override = Some(vec![
        "/bin/sh".into(),
        script.to_string_lossy().into_owned(),
    ]);
    configuration
        .environment
        .insert(scenario_var.into(), scenario.into());
    let request = RunRequest {
        prompt: "do the task".into(),
        working_directory: workdir.to_path_buf(),
        configuration,
        timeout_seconds: None,
    };
    AgentRuntime::new()
        .run(request, RunOptions::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn agent_swap_changes_only_agent_id() {
    let dir = tempfile::tempdir().unwrap();
    let claude = orchestrate(
        "claude-code",
        &fake_claude(dir.path()),
        "FAKE_AGENT_SCENARIO",
        "happy",
        dir.path(),
    )
    .await;
    let codex = orchestrate(
        "codex",
        &fake_codex(dir.path()),
        "FAKE_CODEX_SCENARIO",
        "happy",
        dir.path(),
    )
    .await;
    for (agent_id, run) in [("claude-code", &claude), ("codex", &codex)] {
        assert_eq!(run.stop.reason, StopReason::Completed, "{agent_id}");
        assert!(!run.final_text.is_empty(), "{agent_id}");
        assert!(run.usage.is_some(), "{agent_id}");
        assert!(run.native_session_id.is_some(), "{agent_id}");
    }
}

#[tokio::test]
async fn final_text_round_trips_through_both_adapters() {
    let dir = tempfile::tempdir().unwrap();
    let codex = orchestrate(
        "codex",
        &fake_codex(dir.path()),
        "FAKE_CODEX_SCENARIO",
        "envelope",
        dir.path(),
    )
    .await;
    assert_eq!(codex.stop.reason, StopReason::Completed);
    assert!(codex.final_text.contains("<!-- LOOP_STATUS -->"));
    assert!(codex.final_text.contains("<!-- /LOOP_STATUS -->"));
    assert!(codex.final_text.contains("\"intent\": \"verify\""));
    let claude = orchestrate(
        "claude-code",
        &fake_claude(dir.path()),
        "FAKE_AGENT_SCENARIO",
        "happy",
        dir.path(),
    )
    .await;
    assert_eq!(claude.stop.reason, StopReason::Completed);
    assert_eq!(claude.final_text, "Hello, world.");
}
