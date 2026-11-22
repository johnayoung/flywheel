//! Control-protocol and process-lifetime edges of the claude-code CLI
//! transport, end to end through `AgentRuntime` + `LocalHost` + the fake
//! vendor CLI: what the host writes back on stdin (approval answers,
//! unsupported-request errors, the permission-mode switch), when stdin is
//! closed, and how the configured timeouts bound a run. Pinned by a
//! mutation survey of the adapter.

mod common;

use common::{ScenarioBuilder, fake_claude};
use flywheel_agents::{
    AgentRuntime, ApprovalDecision, CompletedRun, EventType, MemorySink, PermissionPolicy,
    RunOptions,
};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const BOUND: Duration = Duration::from_secs(20);

fn scenario(dir: &Path, name: &str) -> ScenarioBuilder {
    ScenarioBuilder::claude(fake_claude(dir), name, dir.to_path_buf())
}

async fn run_bounded(builder: ScenarioBuilder, options: RunOptions) -> (CompletedRun, Duration) {
    let started = Instant::now();
    let run = tokio::time::timeout(BOUND, async {
        AgentRuntime::new()
            .run(builder.request(), options)
            .await
            .expect("run starts")
    })
    .await
    .expect("run finishes within the bound");
    (run, started.elapsed())
}

fn resolver(decision: ApprovalDecision) -> RunOptions {
    RunOptions::default().with_approval_resolver(Arc::new(move |_| {
        let decision = decision.clone();
        Box::pin(async move { decision })
    }))
}

#[tokio::test]
async fn allow_without_updated_input_echoes_the_original_input() {
    let dir = tempfile::tempdir().unwrap();
    let options = resolver(ApprovalDecision::Allow {
        updated_input: None,
    });
    let (run, _) = run_bounded(scenario(dir.path(), "approval_echo"), options).await;
    assert_eq!(run.final_text, "approval:allow:no-interrupt:rm -rf build");
}

#[tokio::test]
async fn allow_with_updated_input_sends_the_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let options = resolver(ApprovalDecision::Allow {
        updated_input: Some(json!({"command": "make clean"})),
    });
    let (run, _) = run_bounded(scenario(dir.path(), "approval_echo"), options).await;
    assert_eq!(run.final_text, "approval:allow:no-interrupt:make clean");
}

#[tokio::test]
async fn deny_with_interrupt_carries_the_interrupt_flag() {
    let dir = tempfile::tempdir().unwrap();
    let options = resolver(ApprovalDecision::Deny {
        message: "no".into(),
        interrupt: true,
    });
    let (run, _) = run_bounded(scenario(dir.path(), "approval_echo"), options).await;
    assert_eq!(run.final_text, "approval:deny:interrupt:");
}

#[tokio::test]
async fn unsupported_inbound_control_request_is_answered_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _) = run_bounded(
        scenario(dir.path(), "unsupported_control"),
        RunOptions::default(),
    )
    .await;
    assert_eq!(run.final_text, "control:error:creq-3");
}

#[tokio::test]
async fn approval_after_stdin_closed_surfaces_a_delivery_warning() {
    let dir = tempfile::tempdir().unwrap();
    let sink = MemorySink::new();
    let options = RunOptions::default().with_sink(Arc::new(sink.clone()));
    let (run, _) = run_bounded(scenario(dir.path(), "approval_after_result"), options).await;
    assert_eq!(run.final_text, "done");
    let warnings = sink.events_of(EventType::Warning);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let message = warnings[0].str_field("message").unwrap_or_default();
    assert!(
        message.contains("approval response for creq-4 not delivered"),
        "{message}"
    );
}

#[tokio::test]
async fn permission_policy_switch_sends_the_cli_mode() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = AgentRuntime::new();
    let mut active = runtime
        .start(
            scenario(dir.path(), "control_echo").request(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    let control = active.control.clone().expect("stream-json exposes control");
    while let Some(event) = active.events.recv().await {
        if event.kind == EventType::AssistantMessage {
            break;
        }
    }
    control
        .set_permission_policy(PermissionPolicy::Plan)
        .await
        .expect("mode switch acknowledged");
    let run = active.finish().await;
    assert_eq!(run.final_text, "workingset_permission_mode:plan");
}

#[tokio::test]
async fn result_closes_stdin_so_a_cli_waiting_for_eof_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let builder = scenario(dir.path(), "wait_stdin_eof").option("exit_grace_seconds", json!(1));
    let (run, _) = run_bounded(builder, RunOptions::default()).await;
    assert_eq!(run.final_text, "done");
    assert_eq!(run.exit.returncode, Some(0), "{:?}", run.exit);
}

#[tokio::test]
async fn exit_grace_bounds_a_cli_that_lingers_after_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let builder = scenario(dir.path(), "linger").option("exit_grace_seconds", json!(0.3));
    let (run, elapsed) = run_bounded(builder, RunOptions::default()).await;
    assert_eq!(run.final_text, "done");
    assert!(run.stop.finished);
    assert_ne!(run.exit.returncode, Some(0), "{:?}", run.exit);
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
}

#[tokio::test]
async fn approval_timeout_option_bounds_a_silent_resolver() {
    let dir = tempfile::tempdir().unwrap();
    let builder = scenario(dir.path(), "approval").option("approval_timeout_seconds", json!(0.2));
    let options = RunOptions::default().with_approval_resolver(Arc::new(|_| {
        Box::pin(async { std::future::pending().await })
    }));
    let (run, elapsed) = run_bounded(builder, options).await;
    assert_eq!(run.final_text, "approval:deny");
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
}

#[tokio::test]
async fn control_timeout_option_bounds_an_unanswered_request() {
    let dir = tempfile::tempdir().unwrap();
    let request = scenario(dir.path(), "control_silent")
        .option("control_timeout_seconds", json!(0.3))
        .request();
    let mut active = AgentRuntime::new()
        .start(request, RunOptions::default())
        .await
        .unwrap();
    let control = active.control.clone().unwrap();
    while let Some(event) = active.events.recv().await {
        if event.kind == EventType::AssistantMessage {
            break;
        }
    }
    let started = Instant::now();
    let err = tokio::time::timeout(BOUND, control.interrupt())
        .await
        .expect("interrupt resolves within the bound")
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(err.to_string().contains("control request timeout"), "{err}");
    active.cancel.cancel();
    active.finish().await;
}

#[tokio::test]
async fn pending_request_fails_when_the_cli_exits_without_answering() {
    let dir = tempfile::tempdir().unwrap();
    let request = scenario(dir.path(), "control_exit")
        .option("control_timeout_seconds", json!(60))
        .request();
    let mut active = AgentRuntime::new()
        .start(request, RunOptions::default())
        .await
        .unwrap();
    let control = active.control.clone().unwrap();
    while let Some(event) = active.events.recv().await {
        if event.kind == EventType::AssistantMessage {
            break;
        }
    }
    let err = tokio::time::timeout(BOUND, control.interrupt())
        .await
        .expect("interrupt resolves within the bound")
        .unwrap_err();
    assert!(err.to_string().contains("agent stdout closed"), "{err}");
    active.finish().await;
}

#[tokio::test]
async fn control_requests_after_the_stream_ended_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = AgentRuntime::new();
    let active = runtime
        .start(
            scenario(dir.path(), "happy").request(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    let control = active.control.clone().unwrap();
    let run = active.finish().await;
    assert_eq!(run.final_text, "Hello, world.");
    let err = control.interrupt().await.unwrap_err();
    assert!(
        err.to_string().contains("agent control channel is closed"),
        "{err}"
    );
}

#[tokio::test]
async fn negative_timeouts_fall_back_to_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = scenario(dir.path(), "happy");
    for key in [
        "approval_timeout_seconds",
        "exit_grace_seconds",
        "control_timeout_seconds",
    ] {
        builder = builder.option(key, json!(-1));
    }
    let (run, _) = run_bounded(builder, RunOptions::default()).await;
    assert_eq!(run.final_text, "Hello, world.");
    assert_eq!(run.exit.returncode, Some(0));
}
