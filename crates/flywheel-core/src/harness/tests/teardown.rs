//! A hang, a deadline, or a shutdown returns only after the cancelled
//! agent's whole process tree is dead, so a retry can never start a new
//! agent beside the old one. Real processes: the agents-backed invoker
//! drives the fake CLI's `tree` scenario, whose grandchild ignores SIGTERM
//! (only the group SIGKILL sweep stops it within the kill grace).
#![cfg(target_os = "linux")]

use super::support::*;
use crate::agents_invoke::AgentsInvoker;
use crate::agents_invoke::test_support::{fake_configuration, write_fake_claude};
use crate::deadline_config::DeadlineConfig;
use crate::harness::{HarnessConfig, HarnessError, InvokeFn, ShutdownSignal};
use crate::lifecycle::Status;
use crate::loop_guard::LoopGuardConfig;
use std::path::Path;
use std::time::{Duration, Instant};

fn tree_invoker(dir: &Path) -> InvokeFn {
    let script = write_fake_claude(dir);
    AgentsInvoker::new(fake_configuration(&script, "tree"), dir).into_invoke_fn()
}

/// Gone, or a zombie awaiting its reaper.
fn process_gone(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.split_whitespace().next() == Some("Z")),
        Err(_) => true,
    }
}

/// Immediately on return the agent was cancelled gracefully and its
/// grandchild is dead: the teardown was awaited, not left detached (a
/// detached teardown leaves the grandchild alive for the whole kill
/// grace). The short poll only absorbs SIGKILL delivery latency.
fn assert_tree_dead_on_return(dir: &Path) {
    assert!(
        dir.join("terminated.marker").exists(),
        "the agent was never sent SIGTERM"
    );
    let pid = std::fs::read_to_string(dir.join("grandchild.pid")).unwrap();
    let pid = pid.trim();
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline && !process_gone(pid) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(process_gone(pid), "grandchild {pid} outlived the attempt");
}

/// Resolve once the fake agent has recorded its grandchild.
async fn agent_started(dir: &Path) {
    let pid_file = dir.join("grandchild.pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&pid_file).is_ok_and(|text| !text.trim().is_empty()) {
        assert!(Instant::now() < deadline, "the fake agent never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn hang_waits_for_the_agent_tree_to_die() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let mut lc = lifecycle("run-teardown-hang");
    let config = HarnessConfig {
        loop_guard: LoopGuardConfig {
            hang_timeout_seconds: Some(0.5),
            ..LoopGuardConfig::default()
        },
        ..HarnessConfig::default()
    };
    let outcome = h
        .run(&task(vec![]), &mut lc, &config, tree_invoker(dir.path()))
        .await;
    assert!(outcome.attempts[0].error.contains("hang watchdog"));
    assert_tree_dead_on_return(dir.path());
}

#[tokio::test]
async fn deadline_waits_for_the_agent_tree_to_die() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let mut lc = lifecycle("run-teardown-deadline");
    let config = HarnessConfig {
        deadlines: DeadlineConfig {
            agent_iteration_seconds: Some(1.0),
            ..DeadlineConfig::default()
        },
        ..HarnessConfig::default()
    };
    let outcome = h
        .run(&task(vec![]), &mut lc, &config, tree_invoker(dir.path()))
        .await;
    assert!(outcome.attempts[0].error.contains("deadline"));
    assert_tree_dead_on_return(dir.path());
}

#[tokio::test]
async fn shutdown_waits_for_the_agent_tree_to_die() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new();
    let mut lc = lifecycle("run-teardown-shutdown");
    let shutdown = ShutdownSignal::new();
    let trigger = shutdown.clone();
    let watched = dir.path().to_path_buf();
    tokio::spawn(async move {
        agent_started(&watched).await;
        trigger.trigger();
    });
    let err = h
        .run_with(
            &task(vec![]),
            &mut lc,
            &HarnessConfig::default(),
            tree_invoker(dir.path()),
            h.options().with_shutdown(shutdown),
        )
        .await
        .expect_err("interrupted");
    assert!(matches!(err, HarnessError::Interrupted));
    assert_tree_dead_on_return(dir.path());
    let reloaded = crate::store::LifecycleStore::load_lifecycle(&*h.store, "run-teardown-shutdown")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, Status::Interrupted);
}
