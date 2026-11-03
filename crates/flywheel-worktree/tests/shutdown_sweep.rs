//! The worker's SIGTERM path reaches agent process groups. Agents run in
//! groups of their own, so the supervisor's signal to the worker's group
//! never touches them; the worker's own signal handler must sweep them.
//!
//! A dedicated test binary holding a single test: it signals its own
//! process, and the sweep reaches every group registered in it.
#![cfg(target_os = "linux")]

use flywheel_agents::{ExecutionHost, LocalHost, ProcessPlan};
use flywheel_core::audit::StopFlag;
use flywheel_core::harness::ShutdownSignal;
use flywheel_worktree::daemon::arm_signals;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Gone, or a zombie awaiting its reaper.
fn process_gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.split_whitespace().next() == Some("Z")),
        Err(_) => true,
    }
}

#[test]
fn worker_sigterm_terminates_agent_process_groups() {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap(),
    );
    let stop = StopFlag::new();
    let shutdown = ShutdownSignal::new();
    arm_signals(
        &runtime,
        stop.clone(),
        shutdown.clone(),
        Arc::new(|_: &str| {}),
    );

    // An agent with an MCP-server stand-in that ignores SIGTERM.
    let (mut agent, grandchild) = runtime.block_on(async {
        let mut agent = LocalHost
            .spawn(ProcessPlan::new(vec![
                "/bin/sh".into(),
                "-c".into(),
                "(trap '' TERM; exec sleep 300) & echo $!; wait".into(),
            ]))
            .await
            .unwrap();
        let grandchild: i32 = agent
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        (agent, grandchild)
    });
    assert!(!process_gone(grandchild));

    // `arm_signals` installed the handler before returning, so one SIGTERM
    // is caught (tests/signal_arming.rs pins that).
    // SAFETY: signalling our own pid with a handled signal.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGTERM);
    }
    // The watcher sets `stop`, then triggers `shutdown`: wait on the last.
    let seen = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(60), shutdown.triggered()).await
    });
    assert!(seen.is_ok(), "the worker never saw SIGTERM");
    assert!(stop.is_set());

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && !process_gone(grandchild) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        process_gone(grandchild),
        "agent grandchild {grandchild} outlived the worker's shutdown"
    );
    runtime.block_on(agent.wait());
}
