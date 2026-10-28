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
use crate::process_groups::{self, GroupRegistration};
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
///
/// The child leads its own process group, registered in
/// [`crate::process_groups`] until the leader is reaped. Dropping a handle
/// whose process was never reaped (a run future dropped mid-flight, an
/// early return after spawn) SIGKILLs the whole group, so no MCP server or
/// tool subprocess outlives it.
pub struct SubprocessHandle {
    group: Option<GroupRegistration>,
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
    // process_group(0) makes the child its own group leader: pgid == pid.
    let group = if cfg!(unix) {
        i32::try_from(pid).ok().map(GroupRegistration::register)
    } else {
        None
    };
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
        group,
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

    /// Record the reaped leader's status and unregister its group.
    fn reaped(&mut self, status: ExitStatus) -> ExitStatus {
        self.status = Some(status);
        if let Some(group) = self.group.as_mut() {
            group.release();
        }
        status
    }

    async fn reap(&mut self) -> ExitStatus {
        if let Some(status) = self.status {
            return status;
        }
        // `wait` only fails when the child was already reaped out from
        // under us; report the default status rather than hanging.
        let status = self.child.wait().await.unwrap_or_default();
        self.reaped(status)
    }

    /// Signal the whole group while the leader is still unreaped (its pid
    /// pins the group id, so no unrelated group can be hit).
    fn signal_own_group(&self, signal: process_groups::Signal) {
        if self.status.is_none()
            && let Some(group) = &self.group
        {
            process_groups::signal_group(group.pgid(), signal);
        }
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
            self.reaped(status);
            return;
        }
        #[cfg(unix)]
        self.signal_own_group(process_groups::Signal::Term);
        #[cfg(not(unix))]
        let _ = self.child.start_kill();
        let exited = tokio::time::timeout(grace, self.child.wait()).await;
        // Sweep the group with SIGKILL whether or not the leader honored
        // SIGTERM: a tool child that ignores SIGTERM must not outlive the
        // run. The leader is reaped at most just now, so its group id
        // cannot have been recycled yet.
        if let Some(group) = &self.group {
            process_groups::signal_group(group.pgid(), process_groups::Signal::Kill);
        }
        match exited {
            Ok(Ok(status)) => {
                self.reaped(status);
            }
            _ => {
                let _ = self.child.start_kill();
            }
        }
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

impl Drop for SubprocessHandle {
    fn drop(&mut self) {
        // Never reaped: the run was abandoned mid-flight. `kill_on_drop`
        // only reaches the leader; take the whole group down with it.
        self.signal_own_group(process_groups::Signal::Kill);
        if let Some(group) = self.group.as_mut() {
            group.release();
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
/// Killing the local docker client does **not** stop the in-container
/// process (Docker's documented exec behavior), so the agent is launched
/// under a `/bin/sh` wrapper that records its own pid in a per-run file
/// under [`CONTAINER_PGID_DIR`] and then `exec`s the agent argv. `docker
/// exec` starts every process as a session (and therefore group) leader,
/// so that pid is the in-container process-group id. `kill` signals that
/// group through a second `docker exec` (SIGTERM, the grace, then a SIGKILL
/// sweep that also removes the file) before reaping the local client; a
/// handle dropped without a kill issues the SIGKILL sweep from a background
/// thread. The image must provide `/bin/sh` with a `kill` that accepts a
/// negative pid (POSIX sh, dash, bash, and busybox all do).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerExecHost {
    pub container_name: String,
    pub docker_argv: Vec<String>,
}

/// In-container directory holding the per-run process-group files.
pub const CONTAINER_PGID_DIR: &str = "/tmp";

/// Wall-clock ceiling for one in-container signal `docker exec`.
pub const CONTAINER_SIGNAL_TIMEOUT: Duration = Duration::from_secs(15);

/// The agent wrapper: record `$$` (pid == pgid == sid under `docker exec`)
/// in the file named by `$0`, then become the agent. A file that cannot be
/// written never blocks the agent; it only loses in-container cancellation.
const PGID_WRAPPER: &str = r#"{ printf '%s\n' "$$" > "$0"; } 2>/dev/null; exec "$@""#;

/// Signal the recorded group: `$1` is the file, `$2` the signal name. A
/// missing or malformed file is a no-op; the KILL sweep removes the file.
const PGID_SIGNAL_SCRIPT: &str = r#"f=$1; s=$2
[ -r "$f" ] || exit 0
read -r g < "$f" || [ -n "$g" ] || exit 0
case $g in ''|*[!0-9]*) exit 0 ;; esac
[ "$g" -gt 1 ] || exit 0
kill -s "$s" "-$g" 2>/dev/null
if [ "$s" = KILL ]; then rm -f "$f"; fi
exit 0"#;

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

    /// A fresh in-container process-group file path for one spawn.
    pub fn new_pgid_file() -> String {
        format!(
            "{CONTAINER_PGID_DIR}/flywheel-agent-{}.pgid",
            uuid::Uuid::new_v4().simple()
        )
    }

    /// The full `docker exec` argv for `plan`, the agent wrapped so it
    /// records its process-group id in `pgid_file`.
    pub fn assemble_argv(&self, plan: &ProcessPlan, pgid_file: &str) -> Vec<String> {
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
        argv.extend([
            "/bin/sh".to_string(),
            "-c".to_string(),
            PGID_WRAPPER.to_string(),
            pgid_file.to_string(),
        ]);
        argv.extend(plan.argv.iter().cloned());
        argv
    }

    /// The `docker exec` argv that sends `signal` (`TERM` / `KILL`) to the
    /// process group recorded in `pgid_file`.
    pub fn signal_argv(&self, pgid_file: &str, signal: &str) -> Vec<String> {
        let mut argv = self.docker_argv.clone();
        argv.extend([
            "exec".to_string(),
            self.container_name.clone(),
            "/bin/sh".to_string(),
            "-c".to_string(),
            PGID_SIGNAL_SCRIPT.to_string(),
            "flywheel-signal".to_string(),
            pgid_file.to_string(),
            signal.to_string(),
        ]);
        argv
    }
}

impl ExecutionHost for DockerExecHost {
    fn spawn(
        &self,
        plan: ProcessPlan,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, AgentError>> {
        Box::pin(async move {
            if plan.argv.is_empty() {
                return Err(AgentError::AgentStartup(
                    "cannot spawn an empty argv".into(),
                ));
            }
            let pgid_file = Self::new_pgid_file();
            let argv = self.assemble_argv(&plan, &pgid_file);
            // The docker *client* inherits the parent environment; the plan's
            // extras travel as -e flags into the container.
            let client = spawn_plan(&argv, None, None, &plan.stdin).await?;
            Ok(Box::new(DockerExecProcess {
                client,
                term_argv: self.signal_argv(&pgid_file, "TERM"),
                kill_argv: self.signal_argv(&pgid_file, "KILL"),
                swept: false,
            }) as Box<dyn RunningProcess>)
        })
    }
}

/// A `docker exec` agent: the local client plus the in-container group it
/// started. See [`DockerExecHost`].
struct DockerExecProcess {
    client: SubprocessHandle,
    term_argv: Vec<String>,
    kill_argv: Vec<String>,
    /// The in-container SIGKILL sweep has run.
    swept: bool,
}

/// Run one in-container signal `docker exec` to completion, bounded by
/// [`CONTAINER_SIGNAL_TIMEOUT`]. Best effort: a container that is already
/// gone took the agent with it.
async fn run_signal(argv: &[String]) {
    let Some((program, rest)) = argv.split_first() else {
        return;
    };
    let mut command = Command::new(program);
    command
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Ok(mut child) = command.spawn()
        && tokio::time::timeout(CONTAINER_SIGNAL_TIMEOUT, child.wait())
            .await
            .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

impl DockerExecProcess {
    async fn kill_tree(&mut self, grace: Duration) {
        if self.swept {
            return;
        }
        let mut client_exited = self.client.status.is_some();
        if !client_exited && let Ok(Some(status)) = self.client.child.try_wait() {
            self.client.reaped(status);
            client_exited = true;
        }
        if !client_exited {
            run_signal(&self.term_argv).await;
            // The client exits once the in-container leader does.
            client_exited = tokio::time::timeout(grace, self.client.reap())
                .await
                .is_ok();
        }
        // Sweep survivors that ignored SIGTERM (or outlived a leader that
        // exited on its own) and drop the pgid file.
        run_signal(&self.kill_argv).await;
        self.swept = true;
        if !client_exited {
            self.client.kill_group(Duration::ZERO).await;
        }
    }
}

impl RunningProcess for DockerExecProcess {
    fn next_line(&mut self) -> BoxFuture<'_, Result<Option<String>, AgentError>> {
        Box::pin(self.client.read_line())
    }

    fn stdin(&self) -> Option<StdinWriter> {
        self.client.stdin_writer.clone()
    }

    fn kill(&mut self, grace: Duration) -> BoxFuture<'_, ()> {
        Box::pin(self.kill_tree(grace))
    }

    fn wait(&mut self) -> BoxFuture<'_, AgentExit> {
        Box::pin(self.client.wait_exit())
    }
}

impl Drop for DockerExecProcess {
    fn drop(&mut self) {
        if self.swept {
            return;
        }
        // Dropped without a kill (abandoned mid-run, or finished without
        // one): SIGKILL whatever is left of the in-container group and
        // remove its file. Drop cannot await, so the docker call runs on a
        // detached thread.
        let argv = std::mem::take(&mut self.kill_argv);
        let _ = std::thread::Builder::new()
            .name("flywheel-docker-kill".to_string())
            .spawn(move || run_signal_blocking(&argv));
    }
}

/// [`run_signal`] for a caller with no async context (a `Drop`).
fn run_signal_blocking(argv: &[String]) {
    let Some((program, rest)) = argv.split_first() else {
        return;
    };
    let Ok(mut child) = std::process::Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let deadline = Instant::now() + CONTAINER_SIGNAL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
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
            host.assemble_argv(&plan, "/tmp/run.pgid"),
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
                "/bin/sh",
                "-c",
                PGID_WRAPPER,
                "/tmp/run.pgid",
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
            host.assemble_argv(&plan, "f")[..3],
            ["/bin/echo", "exec", "cont-2"]
        );
        let mut open = ProcessPlan::new(vec!["claude".into()]);
        open.stdin = StdinPlan::Open;
        assert_eq!(
            host.assemble_argv(&open, "f")[..4],
            ["/bin/echo", "exec", "-i", "cont-2"]
        );
    }

    #[test]
    fn docker_signal_argv_targets_the_recorded_group() {
        let host = DockerExecHost::new("cont-3");
        assert_eq!(
            host.signal_argv("/tmp/run.pgid", "TERM"),
            vec![
                "docker",
                "exec",
                "cont-3",
                "/bin/sh",
                "-c",
                PGID_SIGNAL_SCRIPT,
                "flywheel-signal",
                "/tmp/run.pgid",
                "TERM"
            ]
        );
        let file = DockerExecHost::new_pgid_file();
        assert!(file.starts_with("/tmp/flywheel-agent-") && file.ends_with(".pgid"));
        assert_ne!(file, DockerExecHost::new_pgid_file());
    }

    #[cfg(unix)]
    #[test]
    fn pgid_wrapper_records_its_pid_then_execs_the_agent() {
        let dir = std::env::temp_dir().join(format!("fw-wrap-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("run.pgid");
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", PGID_WRAPPER])
            .arg(&file)
            .args(["/bin/sh", "-c", "echo $$"])
            .output()
            .unwrap();
        let exec_pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let recorded = std::fs::read_to_string(&file).unwrap();
        assert_eq!(recorded.trim(), exec_pid);
        std::fs::remove_dir_all(&dir).unwrap();
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
