//! Execution hosts: argv construction and process spawning are separate axes.
//!
//! Adapters produce a [`ProcessPlan`]; hosts execute it. This is what lets
//! one adapter serve both a local worktree ([`LocalHost`]) and a
//! bind-mounted container worktree ([`DockerExecHost`]) without duplicating
//! stream logic.
//!
//! Commands are always argv arrays, never shell strings. `plan.environment`
//! is the *extra* environment contributed to the agent process: `LocalHost`
//! merges it over the parent environment (matching how the vendor SDKs and
//! the historical CLI paths behave); `DockerExecHost` passes it as
//! `docker exec -e` flags only, the container image supplying the base
//! environment.

use crate::BoxFuture;
use crate::errors::AgentError;
use crate::models::AgentExit;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Stream-json lines carry whole file edits inside tool_use inputs; 64 MiB is
/// a framing ceiling, not a typical size. Overrun is an `AgentProtocol` error.
pub const LINE_LIMIT_BYTES: usize = 1 << 26;
/// Bounded stderr retention, mirroring the container backend's 64 KiB tail.
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;
/// SIGTERM-to-SIGKILL grace applied by [`RunningProcess::kill`] callers.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(5);

/// How the agent's stdin is wired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinPlan {
    /// `/dev/null`.
    Closed,
    /// Write the payload, then close (prompt piped on stdin for ARG_MAX safety).
    Payload(String),
    /// Keep stdin open; the handle exposes a [`StdinWriter`] for the
    /// adapter's wire protocol (stream-json user messages, control requests).
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessPlan {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub stdin: StdinPlan,
    /// Variables that must NOT reach the agent (account-only auth policy).
    /// `LocalHost` removes them from the merged environment. `DockerExecHost`
    /// cannot unset container-image env; in-container auth hygiene stays
    /// with the container lifecycle owner.
    pub denied_environment: Vec<String>,
}

impl ProcessPlan {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            cwd: None,
            environment: BTreeMap::new(),
            stdin: StdinPlan::Closed,
            denied_environment: Vec::new(),
        }
    }
}

#[derive(Debug)]
enum StdinCommand {
    Write(String),
    Close,
}

/// Writer side of an [`StdinPlan::Open`] process. Cloneable; writes are
/// serialized by a dedicated task that owns the pipe.
#[derive(Debug, Clone)]
pub struct StdinWriter {
    tx: mpsc::UnboundedSender<StdinCommand>,
}

impl StdinWriter {
    fn channel() -> (Self, mpsc::UnboundedReceiver<StdinCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// Queue bytes for the agent's stdin.
    pub fn write(&self, data: impl Into<String>) -> Result<(), AgentError> {
        self.tx
            .send(StdinCommand::Write(data.into()))
            .map_err(|_| AgentError::AgentProcessExited("agent stdin is closed".into()))
    }

    /// Queue one line (a trailing newline is appended).
    pub fn write_line(&self, line: impl AsRef<str>) -> Result<(), AgentError> {
        let mut data = line.as_ref().to_string();
        data.push('\n');
        self.write(data)
    }

    /// Close stdin after every queued write has been flushed.
    pub fn close(&self) {
        let _ = self.tx.send(StdinCommand::Close);
    }
}

/// A spawned plan: line-oriented stdout, optional stdin writer, bounded
/// stderr tail, kill, wait.
pub trait RunningProcess: Send {
    /// Next stdout line without its terminator; `None` at EOF. Lines longer
    /// than [`LINE_LIMIT_BYTES`] are an `AgentProtocol` error.
    fn next_line(&mut self) -> BoxFuture<'_, Result<Option<String>, AgentError>>;

    /// The stdin writer when the plan asked for [`StdinPlan::Open`].
    fn stdin(&self) -> Option<StdinWriter>;

    /// Terminate the whole process tree: SIGTERM the group, SIGKILL after
    /// `grace`. Idempotent once the process has exited.
    fn kill(&mut self, grace: Duration) -> BoxFuture<'_, ()>;

    /// Reap the process and join the reader/writer tasks. Cancel-safe and
    /// repeatable: later calls return the cached exit.
    fn wait(&mut self) -> BoxFuture<'_, AgentExit>;
}

pub trait ExecutionHost: Send + Sync {
    fn spawn(
        &self,
        plan: ProcessPlan,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, AgentError>>;
}

/// Shared [`RunningProcess`] implementation over a tokio child process.
pub struct SubprocessHandle {
    child: Child,
    pid: u32,
    started_at: Instant,
    status: Option<ExitStatus>,
    reader: Option<BufReader<ChildStdout>>,
    pending: Vec<u8>,
    stderr_task: Option<JoinHandle<Vec<u8>>>,
    stderr_tail: Option<String>,
    stdin_task: Option<JoinHandle<()>>,
    stdin_writer: Option<StdinWriter>,
}

impl std::fmt::Debug for SubprocessHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubprocessHandle")
            .field("pid", &self.pid)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

async fn drain_stderr(mut stderr: ChildStderr) -> Vec<u8> {
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return tail,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > STDERR_TAIL_BYTES {
                    let excess = tail.len() - STDERR_TAIL_BYTES;
                    tail.drain(..excess);
                }
            }
        }
    }
}

async fn drive_stdin(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<StdinCommand>) {
    while let Some(command) = rx.recv().await {
        match command {
            StdinCommand::Write(data) => {
                // The agent may exit before consuming its input; wait()
                // reports that, so a broken pipe is not an error here.
                if stdin.write_all(data.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            StdinCommand::Close => break,
        }
    }
    let _ = stdin.shutdown().await;
}

#[cfg(unix)]
fn exit_code(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
}

#[cfg(not(unix))]
fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code()
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: libc::c_int) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: killpg is a plain syscall wrapper with no memory-safety
    // preconditions; a stale or foreign pgid yields ESRCH/EPERM, which is
    // ignored exactly as the Python host ignores ProcessLookupError.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

async fn spawn_plan(
    argv: &[String],
    cwd: Option<&PathBuf>,
    env: Option<BTreeMap<String, String>>,
    stdin: &StdinPlan,
) -> Result<SubprocessHandle, AgentError> {
    let Some(program) = argv.first() else {
        return Err(AgentError::AgentStartup(
            "cannot spawn an empty argv".into(),
        ));
    };
    let mut command = Command::new(program);
    command.args(&argv[1..]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(env) = env {
        command.env_clear();
        command.envs(env);
    }
    command
        .stdin(match stdin {
            StdinPlan::Closed => Stdio::null(),
            StdinPlan::Payload(_) | StdinPlan::Open => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|err| AgentError::AgentStartup(format!("failed to spawn {program:?}: {err}")))?;
    let pid = child.id().ok_or_else(|| {
        AgentError::AgentStartup(format!("{program:?} exited before it could be tracked"))
    })?;
    let reader = child
        .stdout
        .take()
        .map(|stdout| BufReader::with_capacity(64 * 1024, stdout));
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(drain_stderr(stderr)));
    let (stdin_task, stdin_writer) = match (stdin, child.stdin.take()) {
        (StdinPlan::Closed, _) | (_, None) => (None, None),
        (StdinPlan::Payload(payload), Some(pipe)) => {
            let (writer, rx) = StdinWriter::channel();
            let _ = writer.write(payload.clone());
            writer.close();
            (Some(tokio::spawn(drive_stdin(pipe, rx))), None)
        }
        (StdinPlan::Open, Some(pipe)) => {
            let (writer, rx) = StdinWriter::channel();
            (Some(tokio::spawn(drive_stdin(pipe, rx))), Some(writer))
        }
    };
    Ok(SubprocessHandle {
        child,
        pid,
        started_at: Instant::now(),
        status: None,
        reader,
        pending: Vec::new(),
        stderr_task,
        stderr_tail: None,
        stdin_task,
        stdin_writer,
    })
}

impl SubprocessHandle {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    async fn reap(&mut self) -> ExitStatus {
        if let Some(status) = self.status {
            return status;
        }
        // `wait` only fails when the child was already reaped out from
        // under us; report the default status rather than hanging.
        let status = self.child.wait().await.unwrap_or_default();
        self.status = Some(status);
        status
    }

    async fn read_line(&mut self) -> Result<Option<String>, AgentError> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(None);
        };
        loop {
            let buffer = reader.fill_buf().await.map_err(|err| {
                AgentError::AgentProtocol(format!("agent stdout read failed: {err}"))
            })?;
            if buffer.is_empty() {
                self.reader = None;
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(take_line(&mut self.pending)));
            }
            if let Some(index) = buffer.iter().position(|b| *b == b'\n') {
                self.pending.extend_from_slice(&buffer[..index]);
                reader.consume(index + 1);
                return Ok(Some(take_line(&mut self.pending)));
            }
            let len = buffer.len();
            self.pending.extend_from_slice(buffer);
            reader.consume(len);
            if self.pending.len() > LINE_LIMIT_BYTES {
                self.pending.clear();
                self.reader = None;
                return Err(AgentError::AgentProtocol(format!(
                    "agent stdout line exceeded {LINE_LIMIT_BYTES} bytes"
                )));
            }
        }
    }

    async fn kill_group(&mut self, grace: Duration) {
        if self.status.is_some() {
            return;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.status = Some(status);
            return;
        }
        #[cfg(unix)]
        signal_group(self.pid, libc::SIGTERM);
        #[cfg(not(unix))]
        let _ = self.child.start_kill();
        if let Ok(Ok(status)) = tokio::time::timeout(grace, self.child.wait()).await {
            self.status = Some(status);
            return;
        }
        #[cfg(unix)]
        signal_group(self.pid, libc::SIGKILL);
        let _ = self.child.start_kill();
    }

    async fn wait_exit(&mut self) -> AgentExit {
        let status = self.reap().await;
        if let Some(task) = self.stdin_task.as_mut() {
            // The process is gone: nothing still queued for its stdin can
            // matter, and a writer handle held elsewhere must not keep the
            // pipe task (and this wait) alive.
            task.abort();
            let _ = task.await;
            self.stdin_task = None;
        }
        if let Some(task) = self.stderr_task.as_mut() {
            let tail = task.await.unwrap_or_default();
            self.stderr_task = None;
            if !tail.is_empty() {
                self.stderr_tail = Some(String::from_utf8_lossy(&tail).into_owned());
            }
        }
        AgentExit {
            returncode: exit_code(status),
            stderr_tail: self.stderr_tail.clone(),
            duration_seconds: Some(self.started_at.elapsed().as_secs_f64()),
        }
    }
}

fn take_line(pending: &mut Vec<u8>) -> String {
    if pending.last() == Some(&b'\r') {
        pending.pop();
    }
    let line = String::from_utf8_lossy(pending).into_owned();
    pending.clear();
    line
}

impl RunningProcess for SubprocessHandle {
    fn next_line(&mut self) -> BoxFuture<'_, Result<Option<String>, AgentError>> {
        Box::pin(self.read_line())
    }

    fn stdin(&self) -> Option<StdinWriter> {
        self.stdin_writer.clone()
    }

    fn kill(&mut self, grace: Duration) -> BoxFuture<'_, ()> {
        Box::pin(self.kill_group(grace))
    }

    fn wait(&mut self) -> BoxFuture<'_, AgentExit> {
        Box::pin(self.wait_exit())
    }
}

/// Spawns the plan as a local process group.
///
/// The child environment is the parent environment merged with
/// `plan.environment` (extras win), minus `plan.denied_environment`. `kill`
/// signals the whole process group (the child is spawned as its leader).
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalHost;

impl LocalHost {
    /// The environment the spawned agent sees for `plan`.
    pub fn child_environment(plan: &ProcessPlan) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        for (key, value) in &plan.environment {
            env.insert(key.clone(), value.clone());
        }
        for name in &plan.denied_environment {
            env.remove(name);
        }
        env
    }
}

impl ExecutionHost for LocalHost {
    fn spawn(
        &self,
        plan: ProcessPlan,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, AgentError>> {
        Box::pin(async move {
            let env = Self::child_environment(&plan);
            let handle = spawn_plan(&plan.argv, plan.cwd.as_ref(), Some(env), &plan.stdin).await?;
            Ok(Box::new(handle) as Box<dyn RunningProcess>)
        })
    }
}

/// Executes the plan inside an already-running container via `docker exec`.
///
/// Container provisioning, teardown, networking, and auth mounts stay owned
/// by the caller; this host only executes plans.
///
/// `kill` terminates the local docker client, which does **not** reap the
/// in-container process (Docker's documented exec behavior). The container
/// lifecycle owner reaps survivors at teardown (stop/rm).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerExecHost {
    pub container_name: String,
    pub docker_argv: Vec<String>,
}

impl DockerExecHost {
    pub fn new(container_name: impl Into<String>) -> Self {
        Self {
            container_name: container_name.into(),
            docker_argv: vec!["docker".to_string()],
        }
    }

    pub fn with_docker_argv(mut self, docker_argv: Vec<String>) -> Self {
        self.docker_argv = docker_argv;
        self
    }

    /// The full `docker exec` argv for `plan`.
    pub fn assemble_argv(&self, plan: &ProcessPlan) -> Vec<String> {
        let mut argv = self.docker_argv.clone();
        argv.push("exec".to_string());
        if plan.stdin != StdinPlan::Closed {
            argv.push("-i".to_string());
        }
        for (key, value) in &plan.environment {
            argv.push("-e".to_string());
            argv.push(format!("{key}={value}"));
        }
        if let Some(cwd) = &plan.cwd {
            argv.push("-w".to_string());
            argv.push(cwd.to_string_lossy().into_owned());
        }
        argv.push(self.container_name.clone());
        argv.extend(plan.argv.iter().cloned());
        argv
    }
}

impl ExecutionHost for DockerExecHost {
    fn spawn(
        &self,
        plan: ProcessPlan,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, AgentError>> {
        Box::pin(async move {
            let argv = self.assemble_argv(&plan);
            // The docker *client* inherits the parent environment; the plan's
            // extras travel as -e flags into the container.
            let handle = spawn_plan(&argv, None, None, &plan.stdin).await?;
            Ok(Box::new(handle) as Box<dyn RunningProcess>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_argv_assembly_matches_contract() {
        let host = DockerExecHost::new("cont-1").with_docker_argv(vec!["/bin/echo".into()]);
        let mut plan = ProcessPlan::new(vec!["claude".into(), "--print".into()]);
        plan.cwd = Some(PathBuf::from("/inside/workdir"));
        plan.environment.insert("K".into(), "V".into());
        plan.environment.insert("Z".into(), "9".into());
        plan.stdin = StdinPlan::Payload("prompt-on-stdin".into());
        assert_eq!(
            host.assemble_argv(&plan),
            vec![
                "/bin/echo",
                "exec",
                "-i",
                "-e",
                "K=V",
                "-e",
                "Z=9",
                "-w",
                "/inside/workdir",
                "cont-1",
                "claude",
                "--print"
            ]
        );
    }

    #[test]
    fn docker_argv_omits_stdin_flag_without_payload() {
        let host = DockerExecHost::new("cont-2").with_docker_argv(vec!["/bin/echo".into()]);
        let plan = ProcessPlan::new(vec!["claude".into(), "--version".into()]);
        assert_eq!(
            host.assemble_argv(&plan),
            vec!["/bin/echo", "exec", "cont-2", "claude", "--version"]
        );
        let mut open = ProcessPlan::new(vec!["claude".into()]);
        open.stdin = StdinPlan::Open;
        assert_eq!(
            host.assemble_argv(&open),
            vec!["/bin/echo", "exec", "-i", "cont-2", "claude"]
        );
    }

    #[test]
    fn child_environment_merges_and_denies() {
        let mut plan = ProcessPlan::new(vec!["x".into()]);
        plan.environment
            .insert("FLYWHEEL_HOST_TEST_EXTRA".into(), "extra".into());
        plan.denied_environment.push("PATH".into());
        let env = LocalHost::child_environment(&plan);
        assert_eq!(
            env.get("FLYWHEEL_HOST_TEST_EXTRA").map(String::as_str),
            Some("extra")
        );
        assert!(!env.contains_key("PATH"));
    }

    #[test]
    fn take_line_strips_carriage_return() {
        let mut pending = b"abc\r".to_vec();
        assert_eq!(take_line(&mut pending), "abc");
        assert!(pending.is_empty());
    }
}
