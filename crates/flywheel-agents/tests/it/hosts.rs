//! Real-subprocess tests for the execution hosts. `LocalHost` is exercised
//! with `/bin/sh -c` scripts; `DockerExecHost` without Docker, `/bin/echo`
//! standing in for the docker client so a real spawn prints the assembled
//! argv back.

mod common;

use common::write_script;
#[cfg(unix)]
use common::{process_dead, wait_dead};
use flywheel_agents::hosts::DEFAULT_KILL_GRACE;
use flywheel_agents::{
    AgentExit, DockerExecHost, ExecutionHost, LocalHost, ProcessPlan, StdinPlan,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn sh(code: &str) -> ProcessPlan {
    ProcessPlan::new(vec!["/bin/sh".into(), "-c".into(), code.into()])
}

async fn run_to_exit(plan: ProcessPlan) -> (Vec<String>, AgentExit) {
    let mut process = LocalHost.spawn(plan).await.unwrap();
    let mut lines = Vec::new();
    while let Some(line) = process.next_line().await.unwrap() {
        lines.push(line);
    }
    (lines, process.wait().await)
}

#[tokio::test]
async fn stdout_lines_stream_in_order() {
    let (lines, exit) = run_to_exit(sh("for i in 0 1 2 3 4; do echo line-$i; done")).await;
    assert_eq!(
        lines,
        (0..5).map(|i| format!("line-{i}")).collect::<Vec<_>>()
    );
    assert_eq!(exit.returncode, Some(0));
}

#[tokio::test]
async fn unterminated_last_line_is_delivered() {
    let (lines, _) = run_to_exit(sh("printf 'a\\nb'")).await;
    assert_eq!(lines, vec!["a", "b"]);
}

#[tokio::test]
async fn stderr_tail_captured_and_bounded() {
    let (_, exit) = run_to_exit(sh(
        "head -c 70000 /dev/zero | tr '\\0' a >&2; printf 'ZTAIL-END' >&2",
    ))
    .await;
    let tail = exit.stderr_tail.unwrap();
    assert!(tail.len() <= 65536);
    assert!(tail.ends_with("ZTAIL-END"));
}

#[tokio::test]
async fn environment_merged_and_denied_removed() {
    // HOME rather than PATH: sh re-seeds an unset PATH with a default.
    let mut plan = sh(
        "echo \"base=${FLYWHEEL_TEST_BASE-unset} extra=${FLYWHEEL_TEST_EXTRA-unset} denied=${HOME-unset}\"",
    );
    plan.environment
        .insert("FLYWHEEL_TEST_EXTRA".into(), "extra-value".into());
    plan.environment
        .insert("FLYWHEEL_TEST_BASE".into(), "merged-over".into());
    plan.denied_environment.push("HOME".into());
    let (lines, exit) = run_to_exit(plan).await;
    assert_eq!(exit.returncode, Some(0));
    assert_eq!(
        lines,
        vec!["base=merged-over extra=extra-value denied=unset"]
    );
}

#[tokio::test]
async fn stdin_payload_delivered_then_eof() {
    let mut plan = sh("printf 'got:'; cat");
    plan.stdin = StdinPlan::Payload("hello prompt".into());
    let (lines, exit) = run_to_exit(plan).await;
    assert_eq!(lines, vec!["got:hello prompt"]);
    assert_eq!(exit.returncode, Some(0));
}

#[tokio::test]
async fn open_stdin_writer_streams_lines_until_closed() {
    let mut plan = sh("while IFS= read -r line; do echo \"echo:$line\"; done");
    plan.stdin = StdinPlan::Open;
    let mut process = LocalHost.spawn(plan).await.unwrap();
    let writer = process.stdin().expect("open stdin exposes a writer");
    writer.write_line("one").unwrap();
    assert_eq!(
        process.next_line().await.unwrap().as_deref(),
        Some("echo:one")
    );
    writer.write_line("two").unwrap();
    assert_eq!(
        process.next_line().await.unwrap().as_deref(),
        Some("echo:two")
    );
    writer.close();
    assert_eq!(process.next_line().await.unwrap(), None);
    assert_eq!(process.wait().await.returncode, Some(0));
    assert!(writer.write_line("late").is_err());
}

#[tokio::test]
async fn exit_code_propagates_through_wait_and_wait_is_repeatable() {
    let mut process = LocalHost.spawn(sh("exit 7")).await.unwrap();
    assert_eq!(process.next_line().await.unwrap(), None);
    let first = process.wait().await;
    assert_eq!(first.returncode, Some(7));
    assert!(first.duration_seconds.is_some());
    let second = process.wait().await;
    assert_eq!(second.returncode, Some(7));
}

#[tokio::test]
async fn spawn_failure_is_a_startup_error() {
    let err = LocalHost
        .spawn(ProcessPlan::new(vec!["/nonexistent/binary".into()]))
        .await
        .err()
        .unwrap();
    assert_eq!(err.code(), "agent_startup_failed");
    let err = LocalHost
        .spawn(ProcessPlan::new(Vec::new()))
        .await
        .err()
        .unwrap();
    assert_eq!(err.code(), "agent_startup_failed");
}

#[cfg(unix)]
#[tokio::test]
async fn kill_terminates_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "spawner.sh",
        "#!/bin/sh\nsleep 300 &\necho $!\nwait\n",
    );
    let mut process = LocalHost
        .spawn(ProcessPlan::new(vec![
            "/bin/sh".into(),
            script.to_string_lossy().into_owned(),
        ]))
        .await
        .unwrap();
    let grandchild: i32 = process
        .next_line()
        .await
        .unwrap()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let started = Instant::now();
    process.kill(DEFAULT_KILL_GRACE).await;
    let exit = process.wait().await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(exit.returncode.is_some_and(|c| c != 0), "{exit:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !process_dead(grandchild) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(process_dead(grandchild));
}

#[tokio::test]
async fn kill_after_exit_is_a_noop() {
    let mut process = LocalHost.spawn(sh("exit 0")).await.unwrap();
    assert_eq!(process.next_line().await.unwrap(), None);
    process.kill(DEFAULT_KILL_GRACE).await;
    assert_eq!(process.wait().await.returncode, Some(0));
}

#[cfg(unix)]
#[tokio::test]
async fn kill_sweeps_a_grandchild_that_ignores_sigterm() {
    let mut process = LocalHost
        .spawn(sh("(trap '' TERM; exec sleep 300) & echo $!; wait"))
        .await
        .unwrap();
    let grandchild: i32 = process
        .next_line()
        .await
        .unwrap()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    process.kill(Duration::from_millis(200)).await;
    process.wait().await;
    assert!(wait_dead(grandchild, Duration::from_secs(5)).await);
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_an_unreaped_handle_kills_its_whole_group() {
    let mut process = LocalHost
        .spawn(sh("sleep 300 & echo $!; wait"))
        .await
        .unwrap();
    let grandchild: i32 = process
        .next_line()
        .await
        .unwrap()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!process_dead(grandchild));
    drop(process);
    assert!(wait_dead(grandchild, Duration::from_secs(5)).await);
}

#[tokio::test]
async fn docker_exec_host_argv_assembly_through_a_real_spawn() {
    let host = DockerExecHost::new("cont-1").with_docker_argv(vec!["/bin/echo".into()]);
    let mut environment = BTreeMap::new();
    environment.insert("K".to_string(), "V".to_string());
    environment.insert("Z".to_string(), "9".to_string());
    let plan = ProcessPlan {
        argv: vec!["claude".into(), "--print".into()],
        cwd: Some(PathBuf::from("/inside/workdir")),
        environment,
        stdin: StdinPlan::Payload("prompt-on-stdin".into()),
        denied_environment: Vec::new(),
    };
    let mut process = host.spawn(plan).await.unwrap();
    let line = process.next_line().await.unwrap().unwrap();
    assert!(
        line.starts_with("exec -i -e K=V -e Z=9 -w /inside/workdir cont-1 /bin/sh -c "),
        "{line}"
    );
    assert!(line.contains(" /tmp/flywheel-agent-"), "{line}");
    assert!(line.ends_with(".pgid claude --print"), "{line}");
    assert_eq!(process.next_line().await.unwrap(), None);
    assert_eq!(process.wait().await.returncode, Some(0));
}

#[tokio::test]
async fn docker_exec_host_omits_stdin_flag_without_payload() {
    let host = DockerExecHost::new("cont-2").with_docker_argv(vec!["/bin/echo".into()]);
    let mut process = host
        .spawn(ProcessPlan::new(vec!["claude".into(), "--version".into()]))
        .await
        .unwrap();
    let line = process.next_line().await.unwrap().unwrap();
    assert!(line.starts_with("exec cont-2 /bin/sh -c "), "{line}");
    assert!(line.ends_with(".pgid claude --version"), "{line}");
    assert_eq!(process.wait().await.returncode, Some(0));
}
