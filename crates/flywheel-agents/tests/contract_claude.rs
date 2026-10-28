//! Adapter contract suite (design doc section 17) through the real stack:
//! `AgentRuntime` + `LocalHost` + the claude-code CLI adapter drive the
//! fake vendor CLI end to end (real subprocess, real stream-json parsing,
//! real fold, real control protocol). No vendor CLI required.

mod common;

use common::{ScenarioBuilder, fake_claude, read_pid_file, wait_dead};
use flywheel_agents::{
    AgentRuntime, ApprovalDecision, AuthenticationPolicy, CompletedRun, EventType, MemorySink,
    RunOptions, StopReason,
};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn drive(builder: ScenarioBuilder) -> (CompletedRun, MemorySink) {
    let sink = MemorySink::new();
    let runtime = AgentRuntime::new();
    let run = runtime
        .run(
            builder.request(),
            RunOptions::default().with_sink(Arc::new(sink.clone())),
        )
        .await
        .expect("run starts");
    (run, sink)
}

#[tokio::test]
async fn happy_path_fold() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "happy",
        dir.path().to_path_buf(),
    ))
    .await;
    let started = sink.events_of(EventType::SessionStarted);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].str_field("session_id"), Some("sess-1"));
    assert_eq!(run.final_text, "Hello, world.");
    assert_eq!(run.stop.reason, StopReason::Completed);
    assert_eq!(run.stop.native_subtype.as_deref(), Some("success"));
    assert!(!run.stop.is_error);
    assert!(!run.stop.pending_tool_use);
    assert!(run.stop.finished);
    let usage = run.usage.clone().unwrap();
    assert_eq!(usage["input_tokens"], 100);
    assert_eq!(usage["output_tokens"], 25);
    assert_eq!(usage["cache_creation_input_tokens"], 7);
    assert_eq!(usage["cache_read_input_tokens"], 3);
    assert_eq!(run.num_turns, Some(3));
    assert_eq!(run.total_cost_usd, Some(0.0125));
    assert_eq!(run.native_session_id.as_deref(), Some("sess-1"));
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert!(run.fault.is_none());
    assert_eq!(run.exit.returncode, Some(0));
    assert_eq!(run.tool_interactions.len(), 1);
    let interaction = &run.tool_interactions[0];
    assert_eq!(interaction.tool_use_id, "tool-1");
    assert_eq!(interaction.tool_name, "Bash");
    assert_eq!(
        interaction.tool_input.get("command"),
        Some(&json!("echo hi"))
    );
    assert_eq!(interaction.result.as_ref().unwrap().is_error, Some(false));
}

#[tokio::test]
async fn happy_path_sequences_and_sources() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "happy",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none());
    let raw = sink.raw();
    let events = sink.events();
    assert!(!raw.is_empty());
    assert!(!events.is_empty());
    let raw_seq: Vec<u64> = raw.iter().map(|e| e.sequence).collect();
    let event_seq: Vec<u64> = events.iter().map(|e| e.sequence).collect();
    assert!(raw_seq.windows(2).all(|w| w[1] > w[0]));
    assert!(event_seq.windows(2).all(|w| w[1] > w[0]));
    let mut merged: Vec<u64> = raw_seq.iter().chain(event_seq.iter()).copied().collect();
    merged.sort_unstable();
    assert_eq!(merged, (1..=merged.len() as u64).collect::<Vec<_>>());
    assert!(events.iter().all(|e| e.source == "claude-code"));
    assert!(raw.iter().all(|e| e.adapter_id == "claude-code"));
    // The prompt itself is recorded raw on the stdin stream before any stdout.
    assert_eq!(raw[0].stream, "stdin");
    assert_eq!(raw[0].native_type.as_deref(), Some("user"));
    assert!(raw[1..].iter().all(|e| e.stream == "stdout"));
}

#[tokio::test]
async fn tool_error_reported_on_interaction() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "tool_error",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none());
    assert_eq!(run.tool_interactions.len(), 1);
    assert_eq!(run.tool_interactions[0].tool_use_id, "tool-err");
    assert_eq!(
        run.tool_interactions[0].result.as_ref().unwrap().is_error,
        Some(true)
    );
}

#[tokio::test]
async fn malformed_lines_become_raw_and_run_completes() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
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
}

#[tokio::test]
async fn crash_without_result_is_agent_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "crash",
        dir.path().to_path_buf(),
    ))
    .await;
    let failure = run.failure.as_ref().expect("failure");
    assert_eq!(failure.error_type, "agent_exit");
    assert_eq!(failure.exit_code, Some(3));
    let captured = format!(
        "{}{}",
        failure.stderr.clone().unwrap_or_default(),
        run.exit.stderr_tail.clone().unwrap_or_default()
    );
    assert!(captured.contains("fatal explosion"));
    assert_eq!(run.stop.reason, StopReason::Unknown);
    assert!(!run.stop.finished);
    assert_eq!(run.exit.returncode, Some(3));
    assert_eq!(run.final_text, "about to crash");
}

#[tokio::test]
async fn hang_bounded_by_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let (run, _) = drive(
        ScenarioBuilder::claude(fake_claude(dir.path()), "hang", dir.path().to_path_buf())
            .timeout(1.5),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(15));
    let failure = run.failure.as_ref().expect("failure");
    assert_eq!(failure.error_type, "timeout");
    assert_eq!(run.stop.reason, StopReason::Unknown);
    assert!(run.exit.returncode.is_some_and(|c| c != 0));
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_terminates_the_whole_agent_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(
        ScenarioBuilder::claude(
            fake_claude(dir.path()),
            "grandchild",
            dir.path().to_path_buf(),
        )
        .timeout(1.0),
    )
    .await;
    assert_eq!(run.failure.as_ref().unwrap().error_type, "timeout");
    let grandchild = read_pid_file(&dir.path().join("grandchild.pid"), Duration::ZERO).await;
    // The grandchild ignores SIGTERM: only the group SIGKILL sweep stops it.
    assert!(wait_dead(grandchild, Duration::from_secs(5)).await);
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_a_run_future_mid_flight_kills_the_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let request = ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "grandchild",
        dir.path().to_path_buf(),
    )
    .request();
    let runtime = AgentRuntime::new();
    let pid_file = dir.path().join("grandchild.pid");
    let grandchild = {
        let run = runtime.run(request, RunOptions::default());
        tokio::pin!(run);
        tokio::select! {
            _ = &mut run => panic!("the hanging run cannot finish"),
            pid = read_pid_file(&pid_file, Duration::from_secs(10)) => pid,
        }
        // `run` is dropped here without the runtime ever cancelling it.
    };
    assert!(wait_dead(grandchild, Duration::from_secs(5)).await);
}

#[tokio::test]
async fn stderr_tail_captured() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "stderr",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none());
    assert!(
        run.exit
            .stderr_tail
            .as_deref()
            .unwrap()
            .contains("FAKE-AGENT-STDERR-MARKER")
    );
}

#[tokio::test]
async fn working_directory_respected() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("work");
    std::fs::create_dir(&workdir).unwrap();
    let (run, _) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "cwd",
        workdir.clone(),
    ))
    .await;
    assert!(run.failure.is_none());
    assert_eq!(
        std::fs::canonicalize(&run.final_text).unwrap(),
        std::fs::canonicalize(&workdir).unwrap()
    );
}

#[tokio::test]
async fn environment_marker_passes_through_and_account_only_strips_key() {
    let dir = tempfile::tempdir().unwrap();
    // The extras travel through the plan environment (not the test process
    // env), so both cases are exercised without mutating global state.
    let (run, _) = drive(
        ScenarioBuilder::claude(fake_claude(dir.path()), "env", dir.path().to_path_buf())
            .env("TEST_MARKER", "marker-123")
            .env("ANTHROPIC_API_KEY", "sk-test"),
    )
    .await;
    assert!(run.failure.is_none());
    assert_eq!(run.final_text, "marker=marker-123; api_key=sk-test");

    let (run, _) = drive(
        ScenarioBuilder::claude(fake_claude(dir.path()), "env", dir.path().to_path_buf())
            .env("TEST_MARKER", "marker-123")
            .env("ANTHROPIC_API_KEY", "sk-test")
            .auth(AuthenticationPolicy::AccountOnly),
    )
    .await;
    assert!(run.failure.is_none());
    assert_eq!(run.final_text, "marker=marker-123; api_key=unset");
}

#[tokio::test]
async fn usage_limit_classified_as_session_limit_fault() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "usage_limit",
        dir.path().to_path_buf(),
    ))
    .await;
    let fault = run.fault.as_ref().expect("fault");
    assert_eq!(fault.kind, "session_limit");
    assert_eq!(fault.resets_at_epoch, Some(1751990400.0));
    assert_eq!(run.stop.reason, StopReason::Error);
}

#[tokio::test]
async fn no_result_clean_exit_is_unknown_stop() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "no_result",
        dir.path().to_path_buf(),
    ))
    .await;
    assert_eq!(run.stop.reason, StopReason::Unknown);
    assert!(!run.stop.pending_tool_use);
    assert!(run.failure.is_none());
    assert_eq!(run.final_text, "partial work");
}

#[tokio::test]
async fn rate_limit_event_folds_with_reset_instant() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "rate_limit",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none());
    assert_eq!(run.rate_limit_events.len(), 1);
    assert_eq!(run.rate_limit_events[0].resets_at_epoch, Some(1751990400.0));
    let limited = sink.events_of(EventType::RateLimited);
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].str_field("status"), Some("allowed_warning"));
}

#[tokio::test]
async fn text_input_mode_pipes_prompt_and_still_folds() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(
        ScenarioBuilder::claude(fake_claude(dir.path()), "happy", dir.path().to_path_buf())
            .option("input_mode", json!("text")),
    )
    .await;
    assert!(run.failure.is_none());
    assert_eq!(run.final_text, "Hello, world.");
    assert!(sink.raw().iter().all(|e| e.stream == "stdout"));
}

#[tokio::test]
async fn stream_json_mode_delivers_prompt_as_user_message() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "first_line",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none());
    assert_eq!(run.final_text, "exercise the fake agent");
}

#[tokio::test]
async fn interrupt_round_trips_over_the_control_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let request = ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "interrupt",
        dir.path().to_path_buf(),
    )
    .request();
    let runtime = AgentRuntime::new();
    let mut active = runtime.start(request, RunOptions::default()).await.unwrap();
    let control = active
        .control
        .clone()
        .expect("stream-json mode exposes control");
    loop {
        let event = active.events.recv().await.expect("events before interrupt");
        if event.kind == EventType::AssistantMessage {
            break;
        }
    }
    control.interrupt().await.expect("interrupt acknowledged");
    let run = active.finish().await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert_eq!(run.final_text, "workingcontrol:interrupt");
}

#[tokio::test]
async fn control_error_response_surfaces_as_error() {
    let dir = tempfile::tempdir().unwrap();
    let request = ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "control_error",
        dir.path().to_path_buf(),
    )
    .request();
    let runtime = AgentRuntime::new();
    let mut active = runtime.start(request, RunOptions::default()).await.unwrap();
    let control = active.control.clone().unwrap();
    while let Some(event) = active.events.recv().await {
        if event.kind == EventType::AssistantMessage {
            break;
        }
    }
    let err = control
        .set_model(Some("claude-fake-2".into()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "control_request_failed");
    assert!(err.to_string().contains("not now"));
    let run = active.finish().await;
    assert_eq!(run.final_text, "workingrejected");
}

#[tokio::test]
async fn approval_defaults_to_deny_and_emits_events() {
    let dir = tempfile::tempdir().unwrap();
    let (run, sink) = drive(ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "approval",
        dir.path().to_path_buf(),
    ))
    .await;
    assert!(run.failure.is_none(), "{:?}", run.failure);
    assert_eq!(run.final_text, "approval:deny");
    let requested = sink.events_of(EventType::ApprovalRequested);
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0].str_field("tool_name"), Some("Bash"));
    assert_eq!(requested[0].str_field("request_id"), Some("creq-1"));
    assert_eq!(
        requested[0].payload["tool_input"]["command"],
        "rm -rf build"
    );
    let resolved = sink.events_of(EventType::ApprovalResolved);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].str_field("behavior"), Some("deny"));
    assert_eq!(resolved[0].str_field("resolution"), Some("resolver"));
}

#[tokio::test]
async fn approval_resolver_can_allow() {
    let dir = tempfile::tempdir().unwrap();
    let request = ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "approval",
        dir.path().to_path_buf(),
    )
    .request();
    let options = RunOptions::default().with_approval_resolver(Arc::new(|request| {
        Box::pin(async move {
            assert_eq!(request.tool_use_id.as_deref(), Some("tu-9"));
            ApprovalDecision::Allow {
                updated_input: None,
            }
        })
    }));
    let run = AgentRuntime::new().run(request, options).await.unwrap();
    assert_eq!(run.final_text, "approval:allow");
}

#[tokio::test]
async fn approval_resolver_timeout_denies() {
    let dir = tempfile::tempdir().unwrap();
    let sink = MemorySink::new();
    let request = ScenarioBuilder::claude(
        fake_claude(dir.path()),
        "approval",
        dir.path().to_path_buf(),
    )
    .option("approval_timeout_seconds", json!(0.2))
    .request();
    let options = RunOptions::default()
        .with_sink(Arc::new(sink.clone()))
        .with_approval_resolver(Arc::new(|_| {
            Box::pin(async { std::future::pending().await })
        }));
    let run = AgentRuntime::new().run(request, options).await.unwrap();
    assert_eq!(run.final_text, "approval:deny");
    let resolved = sink.events_of(EventType::ApprovalResolved);
    assert_eq!(resolved[0].str_field("resolution"), Some("timeout"));
}

#[tokio::test]
async fn inject_message_reaches_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let request =
        ScenarioBuilder::claude(fake_claude(dir.path()), "inject", dir.path().to_path_buf())
            .request();
    let runtime = AgentRuntime::new();
    let mut active = runtime.start(request, RunOptions::default()).await.unwrap();
    let control = active.control.clone().unwrap();
    while let Some(event) = active.events.recv().await {
        if event.kind == EventType::AssistantMessage {
            break;
        }
    }
    control
        .inject_message("a second thought".into())
        .await
        .unwrap();
    let run = active.finish().await;
    assert_eq!(run.final_text, "readya second thought");
}

#[tokio::test]
async fn cancel_handle_stops_a_hanging_run() {
    let dir = tempfile::tempdir().unwrap();
    let request =
        ScenarioBuilder::claude(fake_claude(dir.path()), "hang", dir.path().to_path_buf())
            .request();
    let runtime = AgentRuntime::new();
    let mut active = runtime.start(request, RunOptions::default()).await.unwrap();
    let first = active.events.recv().await.unwrap();
    assert_eq!(first.kind, EventType::SessionStarted);
    let started = Instant::now();
    active.cancel.cancel();
    let run = active.finish().await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(run.failure.as_ref().unwrap().error_type, "cancelled");
    assert_eq!(run.stop.reason, StopReason::Cancelled);
}

#[tokio::test]
async fn text_mode_exposes_no_control_surface() {
    let dir = tempfile::tempdir().unwrap();
    let request =
        ScenarioBuilder::claude(fake_claude(dir.path()), "happy", dir.path().to_path_buf())
            .option("input_mode", json!("text"))
            .request();
    let runtime = AgentRuntime::new();
    let active = runtime.start(request, RunOptions::default()).await.unwrap();
    assert!(active.control.is_none());
    let run = active.finish().await;
    assert_eq!(run.stop.reason, StopReason::Completed);
}

#[tokio::test]
async fn sdk_transport_is_rejected_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let request =
        ScenarioBuilder::claude(fake_claude(dir.path()), "happy", dir.path().to_path_buf())
            .option("transport", json!("sdk"))
            .request();
    let err = AgentRuntime::new()
        .run(request, RunOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "unsupported_capability");
}
