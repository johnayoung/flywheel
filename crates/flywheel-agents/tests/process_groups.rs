//! The process-group registry's shutdown sweeps, against real process
//! trees. A dedicated test binary holding a single test: `terminate_all`
//! signals every group registered in the process, so nothing else may run
//! alongside it.
#![cfg(unix)]

mod common;

use common::{process_dead, read_pid_file, wait_dead};
use flywheel_agents::process_groups::{self, GroupRegistration};
use flywheel_agents::{ExecutionHost, LocalHost, ProcessPlan};
use std::os::unix::process::CommandExt;
use std::time::Duration;

/// A foreign (non-agent) process tree in its own group, the way a command
/// grader runs: returns the leader and its grandchild's pid.
async fn spawn_foreign_tree(dir: &std::path::Path) -> (std::process::Child, i32) {
    let pid_file = dir.join(format!("{}.pid", uuid_like()));
    let leader = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 300 & echo $! > \"$0\"; wait"])
        .arg(&pid_file)
        .process_group(0)
        .spawn()
        .unwrap();
    let grandchild = read_pid_file(&pid_file, Duration::from_secs(10)).await;
    (leader, grandchild)
}

fn uuid_like() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[tokio::test]
async fn shutdown_sweeps_reach_every_registered_group() {
    let dir = tempfile::tempdir().unwrap();

    // A spawned agent registers its group; the async sweep reaches its
    // grandchild even though it ignores SIGTERM.
    let mut agent = LocalHost
        .spawn(ProcessPlan::new(vec![
            "/bin/sh".into(),
            "-c".into(),
            "(trap '' TERM; exec sleep 300) & echo $!; wait".into(),
        ]))
        .await
        .unwrap();
    let agent_grandchild: i32 = agent
        .next_line()
        .await
        .unwrap()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(process_groups::live_groups().len(), 1);
    assert_eq!(
        process_groups::terminate_all(Duration::from_millis(300)).await,
        1
    );
    assert!(wait_dead(agent_grandchild, Duration::from_secs(5)).await);
    agent.wait().await;
    assert!(process_groups::live_groups().is_empty());
    assert!(process_groups::wait_released(Duration::ZERO).await);

    // A foreign group registered by hand is swept by the blocking variant
    // the product entry point runs on its way out.
    let (mut grader, grader_grandchild) = spawn_foreign_tree(dir.path()).await;
    let mut registration = GroupRegistration::register(grader.id() as i32);
    assert!(!process_dead(grader_grandchild));
    assert_eq!(
        process_groups::terminate_all_blocking(Duration::from_millis(300)),
        1
    );
    grader.wait().unwrap();
    registration.release();
    assert!(wait_dead(grader_grandchild, Duration::from_secs(5)).await);

    // A registration dropped without a release kills its group.
    let (mut abandoned, abandoned_grandchild) = spawn_foreign_tree(dir.path()).await;
    drop(GroupRegistration::register(abandoned.id() as i32));
    abandoned.wait().unwrap();
    assert!(wait_dead(abandoned_grandchild, Duration::from_secs(5)).await);
    assert!(process_groups::live_groups().is_empty());
}
