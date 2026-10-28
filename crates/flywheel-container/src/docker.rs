//! Docker lifecycle primitives for the container sandbox backend.
//!
//! Increment G3 of spec 00044. SDK-free: pure subprocess orchestration of the
//! `docker` CLI; the agent runs as *its own* CLI inside the image, so nothing
//! here links a vendor SDK.
//!
//! The argv construction is factored into pure [`build_run_argv`] /
//! [`build_exec_argv`] functions so the flag mapping is unit-testable without
//! a running daemon; the [`Docker`] client adds the subprocess I/O. Every
//! *management* call (image/network inspect, network create, container
//! stop/rm, the exit-time force-remove) runs under a wall-clock ceiling so a
//! hung `dockerd` surfaces as a distinguishable [`DockerError::Timeout`]
//! rather than wedging the caller while it holds a lease.
//!
//! Patterns adopted from `mattpocock/sandcastle`: a single long-lived
//! container per sandbox (`docker run -d` against an image whose `ENTRYPOINT`
//! is `sleep infinity`), `docker exec` per command, line-streamed stdout with
//! a bounded tail, UID/GID alignment for bind mounts, and a pre-flight
//! image+UID check.

use flywheel_core::deadline_config::{DeadlineClass, resolve_deadlines};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// The agent's home inside the image (`HOME`; `~/.claude` lives under it).
pub const DEFAULT_AGENT_HOME: &str = "/home/agent";
/// Where the host worktree is bind-mounted inside the container.
pub const DEFAULT_WORKDIR: &str = "/home/agent/workspace";
/// Cap on the retained stdout/stderr tail (characters per stream) so a long
/// agent run cannot accumulate unbounded output in memory; lines are still
/// delivered live to the `on_line` observer as they arrive.
pub const DEFAULT_MAX_TAIL_CHARS: usize = 64 * 1024;
/// Wall-clock ceiling for `docker run -d` (image pull + container start).
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(120);

// --- flywheel owner marker --------------------------------------------------
// Every container this backend creates carries OWNER_LABEL as a Docker label;
// the orphan-reap scan filters on the SAME identifier via OWNER_LABEL_SELECTOR.
// Both derive from one constant so the marker written at creation and the
// selector read by the scan can never drift: the scan matches exactly the
// containers this backend creates and never an unrelated one. The label rides
// in `docker ps` / `docker inspect` output, so the reaper keys on it, not on
// the disposable per-task `--name` (spec 00044 G5). The value is a presence
// flag; selection is by key, so its content is irrelevant.

/// The label key every flywheel-owned container carries.
pub const OWNER_LABEL: &str = "flywheel.owner";
/// The label value (a presence flag; selection is by key).
pub const OWNER_LABEL_VALUE: &str = "1";
/// The single `docker ps --filter <...>` argument that selects flywheel-owned
/// containers. Consumers import this rather than re-deriving the string.
pub const OWNER_LABEL_SELECTOR: &str = "label=flywheel.owner";

/// Poll interval for the deadline-bounded child wait.
const WAIT_POLL: Duration = Duration::from_millis(10);

/// The default-on wall-clock ceiling for every docker *management* call.
///
/// Resolved from the shared deadline config (spec 00066, D-1) so the
/// magnitude lives in one place and an operator override flows through the
/// same surface as the other call classes. `None` is the unbounded opt-out;
/// the default is finite so a hung `dockerd` cannot wedge the caller.
pub fn default_management_timeout() -> Option<Duration> {
    resolve_deadlines(std::iter::empty()).duration_for_class(DeadlineClass::DockerManagement)
}

/// A `docker` invocation failed.
///
/// [`DockerError::Timeout`] and [`DockerError::ExecTimeout`] are the
/// distinguishable *timeout* shapes (see [`DockerError::is_timeout`]): the
/// best-effort callers (`image_exists` / `network_exists` /
/// `remove_container` / `force_remove_container` / `list_owned_containers`)
/// swallow ordinary failures but propagate a timeout, because a timeout means
/// the daemon is wedged, which the caller must learn about rather than
/// silently treat as "absent" or "already gone".
#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    /// A management call exceeded its wall-clock ceiling.
    #[error("docker {subcommand} timed out after {}s", timeout.as_secs_f64())]
    Timeout {
        subcommand: String,
        timeout: Duration,
    },
    /// The `docker` binary could not be spawned.
    #[error("docker {subcommand} failed: {source}")]
    Spawn {
        subcommand: String,
        #[source]
        source: std::io::Error,
    },
    /// A non-zero exit.
    #[error("docker {subcommand} failed (exit {exit_code}): {detail}")]
    Failed {
        subcommand: String,
        exit_code: i32,
        detail: String,
    },
    /// The pre-flight found no local image.
    #[error("image {image:?} not found locally; build it first")]
    ImageNotFound { image: String },
    /// The pre-flight found the image's baked UID differs from the host's.
    #[error(
        "UID mismatch: image {image:?} was built with UID {image_uid}, but the host expects \
         {expected_uid}. Rebuild the image with --build-arg AGENT_UID={expected_uid}, or pass \
         container_uid={image_uid} to match the image."
    )]
    UidMismatch {
        image: String,
        image_uid: u32,
        expected_uid: u32,
    },
    /// A `docker exec` exceeded its wall-clock ceiling and was killed.
    #[error("docker exec timed out after {}s", timeout.as_secs_f64())]
    ExecTimeout { timeout: Duration },
}

impl DockerError {
    /// Whether this is a wall-clock timeout (a wedged daemon or a hung exec),
    /// as opposed to an ordinary failure a best-effort caller may swallow.
    pub fn is_timeout(&self) -> bool {
        matches!(
            self,
            DockerError::Timeout { .. } | DockerError::ExecTimeout { .. }
        )
    }
}

/// Outcome of one `docker exec`: bounded tails plus the exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    /// The exit code, or the negated signal number when signal-terminated.
    pub exit_code: i32,
}

/// A host-to-container bind mount (`-v host:sandbox[:ro]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    pub host_path: String,
    pub sandbox_path: String,
    pub readonly: bool,
}

impl VolumeMount {
    pub fn new(host_path: impl Into<String>, sandbox_path: impl Into<String>) -> Self {
        Self {
            host_path: host_path.into(),
            sandbox_path: sandbox_path.into(),
            readonly: false,
        }
    }

    pub fn readonly(mut self) -> Self {
        self.readonly = true;
        self
    }

    /// The `-v` flag value.
    pub fn to_flag(&self) -> String {
        let suffix = if self.readonly { ":ro" } else { "" };
        format!("{}:{}{suffix}", self.host_path, self.sandbox_path)
    }
}

/// Accumulate text but retain only the last `max_chars` characters.
struct BoundedTail {
    max_chars: usize,
    sep: &'static str,
    text: String,
}

impl BoundedTail {
    fn new(max_chars: usize, sep: &'static str) -> Self {
        Self {
            max_chars,
            sep,
            text: String::new(),
        }
    }

    fn push(&mut self, chunk: &str) {
        if !self.text.is_empty() {
            self.text.push_str(self.sep);
        }
        self.text.push_str(chunk);
        let len = self.text.chars().count();
        if len > self.max_chars {
            let keep_from = self
                .text
                .char_indices()
                .nth(len - self.max_chars)
                .map(|(index, _)| index)
                .unwrap_or(self.text.len());
            self.text.drain(..keep_from);
        }
    }

    fn into_string(self) -> String {
        self.text
    }
}

/// Everything `docker run -d` needs for one sandbox container.
///
/// A plain data shape so the argv mapping is pure and assertable; see
/// [`build_run_argv`] for the flag order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunSpec {
    pub name: String,
    pub image: String,
    pub env: BTreeMap<String, String>,
    pub mounts: Vec<VolumeMount>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub networks: Vec<String>,
    pub groups: Vec<String>,
    pub devices: Vec<String>,
    pub cpus: Option<f64>,
    pub labels: BTreeMap<String, String>,
    /// An explicit command override appended after the image (e.g. a
    /// generic base image kept alive with `sleep infinity`); empty relies on
    /// the image's own `ENTRYPOINT`.
    pub command: Vec<String>,
}

impl RunSpec {
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            ..Self::default()
        }
    }

    pub fn with_command(mut self, command: Vec<String>) -> Self {
        self.command = command;
        self
    }

    pub fn with_networks(mut self, networks: Vec<String>) -> Self {
        self.networks = networks;
        self
    }

    pub fn with_mounts(mut self, mounts: Vec<VolumeMount>) -> Self {
        self.mounts = mounts;
        self
    }

    pub fn with_workdir(mut self, workdir: impl Into<String>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }
}

/// Build the `docker run -d` argv. Pure, no subprocess.
///
/// Flag order is deterministic (env, volumes, workdir, user, network,
/// groups, devices, cpus, labels) so it is assertable in tests. The
/// container is detached: it stays alive for subsequent `docker exec` calls
/// via the image's `sleep infinity` entrypoint (or the explicit `command`
/// override). It is deliberately not auto-removed: a container whose
/// process died stays inspectable (`docker inspect` / `docker logs`) for
/// the post-mortem, and removal is owned by the explicit teardown
/// ([`Docker::remove_container`]), the exit-time [`CleanupRegistry`], and
/// the startup orphan reap, which all `docker rm` it. Labels emit `--label key=value`; the flywheel-owner
/// marker is threaded through here by [`Docker::start_container`] so every
/// real container carries it. No label is added when `labels` is empty.
pub fn build_run_argv(spec: &RunSpec) -> Vec<String> {
    let mut argv: Vec<String> = ["docker", "run", "-d", "--name"]
        .into_iter()
        .map(String::from)
        .collect();
    argv.push(spec.name.clone());
    for (key, value) in &spec.env {
        argv.push("-e".into());
        argv.push(format!("{key}={value}"));
    }
    for mount in &spec.mounts {
        argv.push("-v".into());
        argv.push(mount.to_flag());
    }
    if let Some(workdir) = &spec.workdir {
        argv.push("-w".into());
        argv.push(workdir.clone());
    }
    if let Some(user) = &spec.user {
        argv.push("--user".into());
        argv.push(user.clone());
    }
    for network in &spec.networks {
        argv.push("--network".into());
        argv.push(network.clone());
    }
    for group in &spec.groups {
        argv.push("--group-add".into());
        argv.push(group.clone());
    }
    for device in &spec.devices {
        argv.push("--device".into());
        argv.push(device.clone());
    }
    if let Some(cpus) = spec.cpus {
        argv.push("--cpus".into());
        argv.push(format!("{cpus:?}"));
    }
    for (key, value) in &spec.labels {
        argv.push("--label".into());
        argv.push(format!("{key}={value}"));
    }
    argv.push(spec.image.clone());
    argv.extend(spec.command.iter().cloned());
    argv
}

/// Build a `docker exec ... sh -c <command>` argv. Pure, no subprocess.
///
/// `interactive` adds `-i` so a prompt can be piped on stdin (avoiding the
/// Linux per-arg `ARG_MAX` limit). `cwd` sets the working directory.
pub fn build_exec_argv(
    name: &str,
    command: &str,
    interactive: bool,
    cwd: Option<&str>,
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["docker".into(), "exec".into()];
    if interactive {
        argv.push("-i".into());
    }
    if let Some(cwd) = cwd {
        argv.push("-w".into());
        argv.push(cwd.into());
    }
    argv.push(name.into());
    argv.push("sh".into());
    argv.push("-c".into());
    argv.push(command.into());
    argv
}

/// Options for [`Docker::exec_in_container`].
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Piped to the command's stdin (adds `-i`); `None` closes stdin.
    pub stdin: Option<String>,
    /// The in-container working directory (`-w`).
    pub cwd: Option<String>,
    /// Retained tail per stream; `None` is [`DEFAULT_MAX_TAIL_CHARS`].
    pub max_tail_chars: Option<usize>,
    /// Wall-clock ceiling for the whole exec; expiry kills the client and
    /// surfaces [`DockerError::ExecTimeout`]. `None` is unbounded (the
    /// caller passes the resolved `exec_timeout`).
    pub timeout: Option<Duration>,
}

#[cfg(unix)]
fn status_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(-1)
}

#[cfg(not(unix))]
fn status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// Poll `child` until it exits or `deadline` passes; on expiry kill it and
/// reap it, returning `None`.
fn wait_until(child: &Mutex<Child>, deadline: Option<Instant>) -> Option<ExitStatus> {
    loop {
        let mut guard = child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let _ = guard.kill();
            let _ = guard.wait();
            return None;
        }
        drop(guard);
        thread::sleep(WAIT_POLL);
    }
}

/// Read a pipe to its end on a thread, returning the bytes.
fn drain_to_end<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        buffer
    })
}

/// A `docker` CLI client.
///
/// `program` is the argv prefix every call is spliced onto, `["docker"]` by
/// default; tests point it at a script so the deadline and argv contracts
/// are provable against a real (hanging or recording) child. The same
/// prefix is handed to the `DockerExecHost` the strategy builds, so the
/// management calls and the agent exec share one docker binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Docker {
    program: Vec<String>,
}

impl Default for Docker {
    fn default() -> Self {
        Self::new()
    }
}

impl Docker {
    /// The real `docker` on `PATH`.
    pub fn new() -> Self {
        Self {
            program: vec!["docker".into()],
        }
    }

    /// A client that runs `program` in place of `docker` (first element is
    /// the executable, the rest leading arguments). Panics on an empty
    /// prefix: there is nothing to execute.
    pub fn with_program(program: Vec<String>) -> Self {
        assert!(
            !program.is_empty(),
            "Docker::with_program requires a non-empty argv prefix"
        );
        Self { program }
    }

    pub fn program(&self) -> &[String] {
        &self.program
    }

    /// Replace the leading `docker` of a built argv with this client's
    /// program prefix.
    fn rebase(&self, argv: Vec<String>) -> Vec<String> {
        let mut rebased = self.program.clone();
        rebased.extend(argv.into_iter().skip(1));
        rebased
    }

    /// Run a docker subcommand under `timeout`, returning stdout.
    fn run(&self, argv: Vec<String>, timeout: Option<Duration>) -> Result<String, DockerError> {
        let subcommand = argv.get(1).cloned().unwrap_or_default();
        let argv = self.rebase(argv);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| DockerError::Spawn {
                subcommand: subcommand.clone(),
                source,
            })?;
        let stdout = child.stdout.take().map(drain_to_end);
        let stderr = child.stderr.take().map(drain_to_end);
        let child = Mutex::new(child);
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let status = wait_until(&child, deadline);
        let stdout = stdout
            .and_then(|handle| handle.join().ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let stderr = stderr
            .and_then(|handle| handle.join().ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let Some(status) = status else {
            return Err(DockerError::Timeout {
                subcommand,
                timeout: timeout.unwrap_or_default(),
            });
        };
        if !status.success() {
            let detail = if stderr.trim().is_empty() {
                stdout.trim().to_owned()
            } else {
                stderr.trim().to_owned()
            };
            return Err(DockerError::Failed {
                subcommand,
                exit_code: status_code(status),
                detail,
            });
        }
        Ok(stdout)
    }

    /// Run a management subcommand whose ordinary failure is benign: `Ok(None)`
    /// on a non-zero exit or spawn error, `Err` only for a timeout.
    fn run_best_effort(
        &self,
        argv: Vec<String>,
        timeout: Option<Duration>,
    ) -> Result<Option<String>, DockerError> {
        match self.run(argv, timeout) {
            Ok(out) => Ok(Some(out)),
            Err(err) if err.is_timeout() => Err(err),
            Err(_) => Ok(None),
        }
    }

    /// Whether `image` is present locally. `Err` only on a timeout.
    pub fn image_exists(
        &self,
        image: &str,
        timeout: Option<Duration>,
    ) -> Result<bool, DockerError> {
        let argv = strings(&["docker", "image", "inspect", image, "--format", "{{.Id}}"]);
        Ok(self.run_best_effort(argv, timeout)?.is_some())
    }

    /// Pre-flight: the image must exist and its baked `USER` UID (when
    /// numeric) must equal `expected_uid`.
    ///
    /// Misaligned UIDs are the #1 bind-mount gotcha: files the container
    /// writes land owned by the wrong host user and the orchestrator cannot
    /// read them back. A non-numeric or absent `USER` cannot be compared and
    /// is allowed.
    pub fn check_image_uid(
        &self,
        image: &str,
        expected_uid: u32,
        timeout: Option<Duration>,
    ) -> Result<(), DockerError> {
        let argv = strings(&[
            "docker",
            "image",
            "inspect",
            image,
            "--format",
            "{{.Config.User}}",
        ]);
        let out = match self.run(argv, timeout) {
            Ok(out) => out,
            Err(err) if err.is_timeout() => return Err(err),
            Err(_) => {
                return Err(DockerError::ImageNotFound {
                    image: image.to_owned(),
                });
            }
        };
        let user = out.trim();
        if user.is_empty() {
            return Ok(());
        }
        let uid_part = user.split(':').next().unwrap_or_default();
        let Ok(image_uid) = uid_part.parse::<u32>() else {
            return Ok(());
        };
        if image_uid != expected_uid {
            return Err(DockerError::UidMismatch {
                image: image.to_owned(),
                image_uid,
                expected_uid,
            });
        }
        Ok(())
    }

    /// `docker run -d` a detached container; return its id.
    ///
    /// The flywheel-owner marker ([`OWNER_LABEL`]) is always folded in so
    /// every container this backend starts is selectable by
    /// [`OWNER_LABEL_SELECTOR`]; caller-supplied labels are merged on top.
    pub fn start_container(
        &self,
        spec: &RunSpec,
        timeout: Option<Duration>,
    ) -> Result<String, DockerError> {
        let mut labelled = spec.clone();
        labelled
            .labels
            .entry(OWNER_LABEL.to_owned())
            .or_insert_with(|| OWNER_LABEL_VALUE.to_owned());
        let out = self.run(build_run_argv(&labelled), timeout)?;
        Ok(out.trim().to_owned())
    }

    /// `docker exec` a shell command, streaming stdout line-by-line.
    ///
    /// Each stdout line is delivered live to `on_line` (for a stream parser
    /// or idle watchdog) and only a bounded tail is retained in the returned
    /// [`ExecResult`]. Stdin is written on a dedicated thread so a large
    /// prompt cannot deadlock against the child's early output. The whole
    /// operation is bounded by `options.timeout`, not just the final wait: a
    /// child that holds stdout open and silent is killed on expiry, which
    /// closes stdout and ends the read loop.
    pub fn exec_in_container(
        &self,
        name: &str,
        command: &str,
        options: ExecOptions,
        mut on_line: Option<&mut dyn FnMut(&str)>,
    ) -> Result<ExecResult, DockerError> {
        let argv = self.rebase(build_exec_argv(
            name,
            command,
            options.stdin.is_some(),
            options.cwd.as_deref(),
        ));
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(if options.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| DockerError::Spawn {
                subcommand: "exec".into(),
                source,
            })?;

        let max_tail = options.max_tail_chars.unwrap_or(DEFAULT_MAX_TAIL_CHARS);
        let mut stdout_tail = BoundedTail::new(max_tail, "\n");

        let stdin_thread = match (options.stdin, child.stdin.take()) {
            (Some(payload), Some(mut pipe)) => Some(thread::spawn(move || {
                let _ = pipe.write_all(payload.as_bytes());
                let _ = pipe.flush();
            })),
            _ => None,
        };
        let stderr_thread = child.stderr.take().map(|stderr| {
            thread::spawn(move || {
                let mut tail = BoundedTail::new(max_tail, "");
                let mut reader = stderr;
                let mut chunk = [0u8; 8192];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => tail.push(&String::from_utf8_lossy(&chunk[..n])),
                    }
                }
                tail.into_string()
            })
        });
        let stdout = child.stdout.take();
        let child = Arc::new(Mutex::new(child));
        let deadline = options.timeout.map(|timeout| Instant::now() + timeout);

        // Watchdog for the read phase: a child that never closes stdout would
        // block the line loop forever; on expiry it is killed, which closes
        // stdout. The sender is dropped after the loop to retire the watchdog.
        let timed_out = Arc::new(AtomicBool::new(false));
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let watchdog = options.timeout.map(|timeout| {
            let child = Arc::clone(&child);
            let timed_out = Arc::clone(&timed_out);
            thread::spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = cancel_rx.recv_timeout(timeout) {
                    timed_out.store(true, Ordering::SeqCst);
                    let mut guard = child
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let _ = guard.kill();
                }
            })
        });

        if let Some(stdout) = stdout {
            let mut reader = BufReader::with_capacity(64 * 1024, stdout);
            let mut raw = Vec::new();
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let mut line = String::from_utf8_lossy(&raw).into_owned();
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                stdout_tail.push(&line);
                if let Some(observer) = on_line.as_mut() {
                    observer(&line);
                }
            }
        }
        drop(cancel_tx);
        let status = wait_until(&child, deadline);
        if let Some(handle) = watchdog {
            let _ = handle.join();
        }
        if let Some(handle) = stdin_thread {
            let _ = handle.join();
        }
        let stderr = stderr_thread
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();

        if timed_out.load(Ordering::SeqCst) || status.is_none() {
            return Err(DockerError::ExecTimeout {
                timeout: options.timeout.unwrap_or_default(),
            });
        }
        Ok(ExecResult {
            stdout: stdout_tail.into_string(),
            stderr,
            exit_code: status.map(status_code).unwrap_or(-1),
        })
    }

    /// Whether a Docker network named `name` exists. `Err` only on a timeout.
    pub fn network_exists(
        &self,
        name: &str,
        timeout: Option<Duration>,
    ) -> Result<bool, DockerError> {
        let argv = strings(&["docker", "network", "inspect", name, "--format", "{{.Id}}"]);
        Ok(self.run_best_effort(argv, timeout)?.is_some())
    }

    /// Create `name` as a Docker `--internal` network if absent (idempotent).
    ///
    /// Internal networks have no external connectivity (Docker provisions no
    /// gateway), so a container attached to one cannot egress. This is the
    /// real enforcement for `[sandbox.network] policy = "deny"` with no
    /// `allow_hosts` (spec 00044 G6); fine-grained `allow_hosts` is delegated
    /// to an operator-provisioned egress-proxy network instead.
    pub fn ensure_internal_network(
        &self,
        name: &str,
        timeout: Option<Duration>,
    ) -> Result<(), DockerError> {
        if self.network_exists(name, timeout)? {
            return Ok(());
        }
        self.run(
            strings(&["docker", "network", "create", "--internal", name]),
            timeout,
        )?;
        Ok(())
    }

    /// Stop and remove a container, best-effort (ignores "not found").
    ///
    /// `timeout` bounds each stop/rm so a hung `dockerd` cannot wedge
    /// teardown while the worker holds its lease; an ordinary failure (e.g.
    /// the container is already gone) is swallowed, but
    /// a timeout propagates so it is not mistaken for a clean removal.
    pub fn remove_container(
        &self,
        name: &str,
        timeout: Option<Duration>,
    ) -> Result<(), DockerError> {
        self.run_best_effort(strings(&["docker", "stop", name]), timeout)?;
        self.run_best_effort(strings(&["docker", "rm", name]), timeout)?;
        Ok(())
    }

    /// `docker rm -f` a container, swallowing spawn/exit errors (crash
    /// cleanup). A timeout still propagates so the wedge is distinguishable;
    /// [`CleanupRegistry::flush`] catches it so exit-time cleanup completes.
    pub fn force_remove_container(
        &self,
        name: &str,
        timeout: Option<Duration>,
    ) -> Result<(), DockerError> {
        self.run_best_effort(strings(&["docker", "rm", "-f", name]), timeout)?;
        Ok(())
    }

    /// Names of every flywheel-owned container (running or exited).
    ///
    /// Runs `docker ps -a --filter <OWNER_LABEL_SELECTOR> --format {{.Names}}`,
    /// selecting on the shared owner label alone, so an unrelated container
    /// the daemon happens to be running is never returned. Best-effort: an
    /// ordinary docker failure (e.g. the daemon is absent) yields an empty
    /// list, but a timeout propagates so a wedged daemon is not silently
    /// mistaken for "no containers".
    pub fn list_owned_containers(
        &self,
        timeout: Option<Duration>,
    ) -> Result<Vec<String>, DockerError> {
        let argv = strings(&[
            "docker",
            "ps",
            "-a",
            "--filter",
            OWNER_LABEL_SELECTOR,
            "--format",
            "{{.Names}}",
        ]);
        let Some(out) = self.run_best_effort(argv, timeout)? else {
            return Ok(Vec::new());
        };
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect())
    }

    /// Force-remove every flywheel-owned orphan container; return their names.
    ///
    /// The startup backstop for the SIGKILL/OOM case the exit-time registry
    /// cannot cover: a killed worker's exit hook never ran, so its labelled
    /// container survives. This scans for flywheel-owned containers
    /// ([`Docker::list_owned_containers`]) and `docker rm -f`s each orphan,
    /// every one whose name is not in `active` (the containers a live worker
    /// in THIS process still owns; see [`CleanupRegistry::active_names`]).
    /// Unrelated, non-flywheel containers are invisible to the owner-label
    /// filter and are never touched.
    ///
    /// Best-effort per container: an ordinary removal failure is swallowed
    /// inside [`Docker::force_remove_container`] so one wedged container does
    /// not block reaping the rest; a timeout propagates so a wedged daemon
    /// surfaces.
    pub fn reap_orphan_containers(
        &self,
        active: &BTreeSet<String>,
        timeout: Option<Duration>,
    ) -> Result<Vec<String>, DockerError> {
        let mut reaped = Vec::new();
        for name in self.list_owned_containers(timeout)? {
            if active.contains(&name) {
                continue;
            }
            self.force_remove_container(&name, timeout)?;
            reaped.push(name);
        }
        Ok(reaped)
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

// --- crash-safety cleanup registry -----------------------------------------
// A best-effort backstop to the explicit per-run SandboxHandle teardown (G1):
// any container still registered at process exit is force-removed. Signal
// handling is intentionally NOT hijacked here (a library must not fight the
// worker's own SIGINT/SIGTERM handling); the exit hook covers normal exit and
// panics that unwind to main; the explicit teardown covers the steady-state
// path.

/// The set of containers this process still owns, each with the
/// docker-management ceiling its exit-time force-remove applies.
///
/// [`CleanupRegistry::global`] is the process-wide instance flushed at exit;
/// the strategy accepts any instance so tests can isolate registrations.
#[derive(Debug, Default)]
pub struct CleanupRegistry {
    registered: Mutex<BTreeMap<String, Option<Duration>>>,
}

static GLOBAL_REGISTRY: OnceLock<Arc<CleanupRegistry>> = OnceLock::new();

extern "C" fn flush_global_registry_at_exit() {
    flush_cleanup_registry();
}

impl CleanupRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-global registry; the first call installs the exit hook
    /// that flushes it.
    pub fn global() -> Arc<CleanupRegistry> {
        Arc::clone(GLOBAL_REGISTRY.get_or_init(|| {
            // SAFETY: `atexit` registers a plain `extern "C" fn` with no
            // captured state; the handler only touches the `OnceLock`-backed
            // registry and spawns `docker`, both valid at exit time.
            unsafe {
                libc::atexit(flush_global_registry_at_exit);
            }
            Arc::new(CleanupRegistry::new())
        }))
    }

    /// Register `name` for force-removal at process exit under `timeout`.
    pub fn register(&self, name: impl Into<String>, timeout: Option<Duration>) {
        self.lock().insert(name.into(), timeout);
    }

    /// Drop `name` once the container is disposed normally.
    pub fn unregister(&self, name: &str) {
        self.lock().remove(name);
    }

    /// Snapshot of the container names this registry still owns. The
    /// startup orphan reap excludes them so it targets only genuine orphans
    /// and never a container still in active use.
    pub fn active_names(&self) -> BTreeSet<String> {
        self.lock().keys().cloned().collect()
    }

    /// Force-remove every registered container via `docker` and clear the
    /// registry. Returns each name with its removal outcome; failures
    /// (including timeouts) are reported, never raised, because cleanup is
    /// strictly best-effort.
    pub fn flush(&self, docker: &Docker) -> Vec<(String, Result<(), DockerError>)> {
        let registered: Vec<(String, Option<Duration>)> =
            std::mem::take(&mut *self.lock()).into_iter().collect();
        registered
            .into_iter()
            .map(|(name, timeout)| {
                let outcome = docker.force_remove_container(&name, timeout);
                (name, outcome)
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Option<Duration>>> {
        self.registered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Register `name` on the global registry for force-removal at process exit.
pub fn register_container_cleanup(name: impl Into<String>, timeout: Option<Duration>) {
    CleanupRegistry::global().register(name, timeout);
}

/// Snapshot of the container names THIS process registered for cleanup.
pub fn active_container_names() -> BTreeSet<String> {
    match GLOBAL_REGISTRY.get() {
        Some(registry) => registry.active_names(),
        None => BTreeSet::new(),
    }
}

/// Flush the global registry against the real `docker` (the exit hook).
pub fn flush_cleanup_registry() {
    if let Some(registry) = GLOBAL_REGISTRY.get() {
        let _ = registry.flush(&Docker::new());
    }
}

/// Startup backstop: force-remove flywheel-owned orphan containers a prior
/// killed worker left behind, the SIGKILL/OOM case the exit-time registry
/// never covers (spec 00071 #5). Called by the worker at boot.
///
/// The scan selects strictly on the shared flywheel-owner label, so
/// unrelated containers are never touched, and the reap excludes any
/// container this live process still owns. Best-effort: a docker error (e.g.
/// the daemon is absent, or wedged) is logged and swallowed so the worker
/// still starts.
pub fn reap_container_orphans(log: &dyn Fn(&str)) {
    reap_container_orphans_with(&Docker::new(), log);
}

/// [`reap_container_orphans`] against an explicit client. A host with no
/// docker binary or no reachable daemon is silent (the scan's ordinary
/// failure reads as "no containers"); only a wedged daemon's timeout is
/// logged.
fn reap_container_orphans_with(docker: &Docker, log: &dyn Fn(&str)) {
    match docker.reap_orphan_containers(&active_container_names(), default_management_timeout()) {
        Ok(reaped) => {
            for name in reaped {
                log(&format!("Reaped orphan container {name}"));
            }
        }
        Err(err) => log(&format!("Container orphan reap skipped ({err})")),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Shared fixtures: a scripted `docker` stand-in that records every argv
    //! it receives (one line per call) and behaves per subcommand, plus the
    //! live-daemon gate.

    use super::*;
    use std::path::{Path, PathBuf};

    /// A fake `docker` written to `dir`: records `"$*"` to `log` then runs
    /// `body` (a POSIX `case` over `$1 $2`). Returns the client and log path.
    pub(crate) fn scripted_docker(dir: &Path, body: &str) -> (Docker, PathBuf) {
        let log = dir.join("docker.log");
        let script = dir.join("docker.sh");
        let text = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\nexit 0\n",
            log.display()
        );
        std::fs::write(&script, text).unwrap();
        // Run via `/bin/sh <script>` rather than exec'ing the file: a sibling
        // test thread forking while this file is still open for writing
        // would otherwise race into ETXTBSY on exec.
        (
            Docker::with_program(vec![
                "/bin/sh".to_owned(),
                script.to_string_lossy().into_owned(),
            ]),
            log,
        )
    }

    /// The default happy-path body: images exist with no baked user, networks
    /// never exist (so an internal one is created), `run` prints an id.
    pub(crate) const HAPPY_BODY: &str = r#"case "$1 $2" in
  "image inspect") echo "" ;;
  "network inspect") exit 1 ;;
  "run -d") echo cid-123 ;;
esac"#;

    /// A body that hangs on every subcommand (a wedged `dockerd`).
    pub(crate) const HANG_BODY: &str = "exec sleep 30";

    pub(crate) fn recorded_calls(log: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|line| line.split(' ').map(String::from).collect())
            .collect()
    }

    pub(crate) fn docker_available() -> bool {
        Docker::new()
            .run(strings(&["docker", "info"]), Some(Duration::from_secs(10)))
            .is_ok()
    }

    macro_rules! require_docker {
        () => {
            if !$crate::docker::testing::docker_available() {
                eprintln!("skipping: docker daemon not available");
                return;
            }
        };
    }
    pub(crate) use require_docker;

    /// Serialize the live tests: the orphan reap is destructive across the
    /// whole daemon, so a sibling test's container must never be mid-start
    /// while another scans and reaps.
    pub(crate) fn live_lock() -> std::sync::MutexGuard<'static, ()> {
        static LIVE_DAEMON: Mutex<()> = Mutex::new(());
        LIVE_DAEMON
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn unique_name(prefix: &str) -> String {
        format!(
            "{prefix}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{
        HANG_BODY, HAPPY_BODY, live_lock, recorded_calls, require_docker, scripted_docker,
        unique_name,
    };
    use super::*;
    use flywheel_core::deadline_config::DEFAULT_DOCKER_MANAGEMENT_SECONDS;

    /// The ceiling for the hang tests: a hung child must be cut off well
    /// within the wall budget.
    const TINY: Option<Duration> = Some(Duration::from_millis(200));
    /// The ceiling for happy-path scripted calls: generous, so a loaded test
    /// host spawning `sh` never trips it.
    const FAST: Option<Duration> = Some(Duration::from_secs(30));
    const WALL_BUDGET: Duration = Duration::from_secs(5);
    const ALPINE: &str = "alpine:latest";

    fn sv(items: &[&str]) -> Vec<String> {
        strings(items)
    }

    // --- argv builders -----------------------------------------------------

    #[test]
    fn build_run_argv_minimal() {
        assert_eq!(
            build_run_argv(&RunSpec::new("box", "img")),
            sv(&["docker", "run", "-d", "--name", "box", "img"])
        );
    }

    #[test]
    fn build_run_argv_full_flag_order() {
        let mut spec = RunSpec::new("box", "img");
        spec.env.insert("ANTHROPIC_API_KEY".into(), "x".into());
        spec.mounts.push(VolumeMount::new("/h", "/s"));
        spec.workdir = Some("/s".into());
        spec.user = Some("1000:1000".into());
        spec.networks = sv(&["netA", "netB"]);
        spec.groups = sv(&["docker", "999"]);
        spec.devices = sv(&["/dev/kvm"]);
        spec.cpus = Some(1.5);
        assert_eq!(
            build_run_argv(&spec),
            sv(&[
                "docker",
                "run",
                "-d",
                "--name",
                "box",
                "-e",
                "ANTHROPIC_API_KEY=x",
                "-v",
                "/h:/s",
                "-w",
                "/s",
                "--user",
                "1000:1000",
                "--network",
                "netA",
                "--network",
                "netB",
                "--group-add",
                "docker",
                "--group-add",
                "999",
                "--device",
                "/dev/kvm",
                "--cpus",
                "1.5",
                "img",
            ])
        );
    }

    #[test]
    fn build_run_argv_whole_cpus_keep_a_decimal_point() {
        let mut spec = RunSpec::new("box", "img");
        spec.cpus = Some(2.0);
        let argv = build_run_argv(&spec);
        assert_eq!(argv[argv.len() - 2], "2.0");
    }

    #[test]
    fn build_run_argv_appends_command_override() {
        let spec = RunSpec::new("box", "alpine").with_command(sv(&["sleep", "infinity"]));
        assert_eq!(
            build_run_argv(&spec),
            sv(&[
                "docker", "run", "-d", "--name", "box", "alpine", "sleep", "infinity"
            ])
        );
    }

    #[test]
    fn orphan_reap_is_silent_without_a_docker_binary_or_daemon() {
        let lines = Mutex::new(Vec::<String>::new());
        let log = |line: &str| lines.lock().unwrap().push(line.to_owned());
        let missing = Docker::with_program(sv(&["/nonexistent/flywheel-test/docker"]));
        reap_container_orphans_with(&missing, &log);
        // A docker CLI whose daemon is unreachable exits non-zero.
        let no_daemon = Docker::with_program(sv(&["/bin/sh", "-c", "exit 1", "docker"]));
        reap_container_orphans_with(&no_daemon, &log);
        assert!(
            lines.lock().unwrap().is_empty(),
            "{:?}",
            lines.lock().unwrap()
        );
    }

    #[test]
    fn build_exec_argv_buffered_vs_stdin() {
        assert_eq!(
            build_exec_argv("box", "ls", false, None),
            sv(&["docker", "exec", "box", "sh", "-c", "ls"])
        );
        assert_eq!(
            build_exec_argv("box", "claude -p -", true, Some("/s")),
            sv(&[
                "docker",
                "exec",
                "-i",
                "-w",
                "/s",
                "box",
                "sh",
                "-c",
                "claude -p -"
            ])
        );
    }

    #[test]
    fn volume_mount_flag_readonly() {
        assert_eq!(VolumeMount::new("/h", "/s").to_flag(), "/h:/s");
        assert_eq!(
            VolumeMount::new("/h", "/s").readonly().to_flag(),
            "/h:/s:ro"
        );
    }

    // --- bounded tail --------------------------------------------------------

    #[test]
    fn bounded_tail_retains_only_the_last_chars() {
        let mut tail = BoundedTail::new(5, "");
        tail.push("abcdef");
        tail.push("gh");
        assert_eq!(tail.into_string(), "defgh");
    }

    #[test]
    fn bounded_tail_joins_lines_with_separator() {
        let mut tail = BoundedTail::new(100, "\n");
        tail.push("one");
        tail.push("two");
        assert_eq!(tail.into_string(), "one\ntwo");
    }

    #[test]
    fn bounded_tail_counts_characters_not_bytes() {
        let mut tail = BoundedTail::new(3, "");
        tail.push("ab\u{e9}\u{e8}\u{ea}");
        assert_eq!(tail.into_string(), "\u{e9}\u{e8}\u{ea}");
    }

    // --- owner marker --------------------------------------------------------

    #[test]
    fn selector_and_marker_share_one_identifier() {
        assert_eq!(OWNER_LABEL_SELECTOR, format!("label={OWNER_LABEL}"));
        assert!(OWNER_LABEL.starts_with("flywheel."));
    }

    fn labels_from_argv(argv: &[String]) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        for (index, token) in argv.iter().enumerate() {
            if token == "--label" {
                let (key, value) = argv[index + 1].split_once('=').unwrap();
                labels.insert(key.to_owned(), value.to_owned());
            }
        }
        labels
    }

    /// Faithful model of `docker ps --filter label=KEY[=VALUE]`.
    fn selects(selector: &str, labels: &BTreeMap<String, String>) -> bool {
        let rest = selector.strip_prefix("label=").unwrap();
        match rest.split_once('=') {
            Some((key, value)) => labels.get(key).is_some_and(|v| v == value),
            None => labels.contains_key(rest),
        }
    }

    #[test]
    fn build_run_argv_emits_owner_label_before_image() {
        let spec = RunSpec::new("box", "img").with_labels(BTreeMap::from([(
            OWNER_LABEL.into(),
            OWNER_LABEL_VALUE.into(),
        )]));
        let argv = build_run_argv(&spec);
        let label_at = argv.iter().position(|t| t == "--label").unwrap();
        let image_at = argv.iter().position(|t| t == "img").unwrap();
        assert!(label_at < image_at);
        assert!(selects(OWNER_LABEL_SELECTOR, &labels_from_argv(&argv)));
    }

    #[test]
    fn start_container_always_carries_owner_marker() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, log) = scripted_docker(dir.path(), HAPPY_BODY);
        let cid = docker
            .start_container(&RunSpec::new("flywheel-task-deadbeef", "img"), FAST)
            .unwrap();
        assert_eq!(cid, "cid-123");
        let calls = recorded_calls(&log);
        let labels = labels_from_argv(&calls[0]);
        assert_eq!(
            labels.get(OWNER_LABEL).map(String::as_str),
            Some(OWNER_LABEL_VALUE)
        );
        assert!(selects(OWNER_LABEL_SELECTOR, &labels));
    }

    #[test]
    fn start_container_owner_marker_survives_caller_labels() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, log) = scripted_docker(dir.path(), HAPPY_BODY);
        let spec = RunSpec::new("flywheel-task-1", "img")
            .with_labels(BTreeMap::from([("com.example.app".into(), "x".into())]));
        docker.start_container(&spec, FAST).unwrap();
        let labels = labels_from_argv(&recorded_calls(&log)[0]);
        assert_eq!(
            labels.get(OWNER_LABEL).map(String::as_str),
            Some(OWNER_LABEL_VALUE)
        );
        assert_eq!(labels.get("com.example.app").map(String::as_str), Some("x"));
    }

    #[test]
    fn unmarked_and_foreign_labelled_containers_are_not_selected() {
        assert!(!selects(
            OWNER_LABEL_SELECTOR,
            &labels_from_argv(&build_run_argv(&RunSpec::new("other", "img")))
        ));
        let foreign = RunSpec::new("foreign", "img")
            .with_labels(BTreeMap::from([("com.example.owner".into(), "x".into())]));
        assert!(!selects(
            OWNER_LABEL_SELECTOR,
            &labels_from_argv(&build_run_argv(&foreign))
        ));
    }

    // --- management deadline (spec 00066 criterion 5) -------------------------

    #[test]
    fn default_management_timeout_is_finite_and_matches_core() {
        let ceiling = default_management_timeout().unwrap();
        assert!(ceiling > Duration::ZERO);
        assert_eq!(
            ceiling,
            Duration::from_secs_f64(DEFAULT_DOCKER_MANAGEMENT_SECONDS)
        );
    }

    fn management_primitives(docker: &Docker) -> Vec<(&'static str, Result<(), DockerError>)> {
        vec![
            ("image_exists", docker.image_exists("img", TINY).map(|_| ())),
            ("check_image_uid", docker.check_image_uid("img", 1000, TINY)),
            (
                "network_exists",
                docker.network_exists("net", TINY).map(|_| ()),
            ),
            (
                "ensure_internal_network",
                docker.ensure_internal_network("net", TINY),
            ),
            ("remove_container", docker.remove_container("box", TINY)),
            (
                "force_remove_container",
                docker.force_remove_container("box", TINY),
            ),
            (
                "list_owned_containers",
                docker.list_owned_containers(TINY).map(|_| ()),
            ),
            (
                "reap_orphan_containers",
                docker
                    .reap_orphan_containers(&BTreeSet::new(), TINY)
                    .map(|_| ()),
            ),
        ]
    }

    #[test]
    fn management_primitives_surface_timeout_when_dockerd_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, _) = scripted_docker(dir.path(), HANG_BODY);
        for (name, outcome) in management_primitives(&docker) {
            let started = Instant::now();
            let err = outcome.expect_err(name);
            assert!(err.is_timeout(), "{name}: {err}");
            assert!(started.elapsed() < WALL_BUDGET, "{name} wedged");
        }
    }

    #[test]
    fn timeout_error_names_the_subcommand_and_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, _) = scripted_docker(dir.path(), HANG_BODY);
        let err = docker.image_exists("img", TINY).unwrap_err();
        assert_eq!(err.to_string(), "docker image timed out after 0.2s");
    }

    #[test]
    fn unbounded_opt_out_runs_without_a_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, _) = scripted_docker(dir.path(), HAPPY_BODY);
        assert!(docker.image_exists("img", None).unwrap());
        assert!(!docker.network_exists("net", None).unwrap());
    }

    #[test]
    fn start_container_timeout_bound_is_120s() {
        assert_eq!(DEFAULT_START_TIMEOUT, Duration::from_secs(120));
    }

    // --- check_image_uid -----------------------------------------------------

    fn uid_docker(dir: &std::path::Path, user: &str) -> Docker {
        let body = format!("case \"$1 $2\" in \"image inspect\") echo \"{user}\" ;; esac");
        scripted_docker(dir, &body).0
    }

    #[test]
    fn check_image_uid_compares_numeric_user() {
        let dir = tempfile::tempdir().unwrap();
        let docker = uid_docker(dir.path(), "1000:1000");
        docker.check_image_uid("img", 1000, FAST).unwrap();
        let err = docker.check_image_uid("img", 1001, FAST).unwrap_err();
        assert!(matches!(
            err,
            DockerError::UidMismatch {
                image_uid: 1000,
                expected_uid: 1001,
                ..
            }
        ));
        assert!(err.to_string().contains("--build-arg AGENT_UID=1001"));
    }

    #[test]
    fn check_image_uid_allows_named_or_absent_user() {
        let dir = tempfile::tempdir().unwrap();
        uid_docker(dir.path(), "agent")
            .check_image_uid("img", 1, FAST)
            .unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        uid_docker(dir2.path(), "")
            .check_image_uid("img", 1, FAST)
            .unwrap();
    }

    #[test]
    fn check_image_uid_missing_image_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, _) = scripted_docker(dir.path(), "exit 1");
        let err = docker.check_image_uid("ghost", 1000, FAST).unwrap_err();
        assert!(matches!(err, DockerError::ImageNotFound { .. }));
        assert_eq!(
            err.to_string(),
            "image \"ghost\" not found locally; build it first"
        );
    }

    #[test]
    fn failed_run_reports_exit_code_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, _) = scripted_docker(dir.path(), "echo boom >&2; exit 7");
        let err = docker
            .run(sv(&["docker", "network", "create", "x"]), FAST)
            .unwrap_err();
        assert_eq!(err.to_string(), "docker network failed (exit 7): boom");
        assert!(!err.is_timeout());
    }

    // --- best-effort callers ---------------------------------------------------

    #[test]
    fn list_owned_containers_is_empty_on_ordinary_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, _) = scripted_docker(dir.path(), "exit 1");
        assert!(docker.list_owned_containers(FAST).unwrap().is_empty());
    }

    #[test]
    fn reap_skips_active_names_and_force_removes_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"case "$1" in ps) printf 'keep\nreap-me\n\n' ;; esac"#;
        let (docker, log) = scripted_docker(dir.path(), body);
        let active = BTreeSet::from(["keep".to_owned()]);
        assert_eq!(
            docker.reap_orphan_containers(&active, FAST).unwrap(),
            vec!["reap-me"]
        );
        let calls = recorded_calls(&log);
        assert_eq!(
            calls[0],
            sv(&[
                "ps",
                "-a",
                "--filter",
                OWNER_LABEL_SELECTOR,
                "--format",
                "{{.Names}}"
            ])
        );
        assert_eq!(calls[1], sv(&["rm", "-f", "reap-me"]));
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn remove_container_issues_stop_then_rm_and_swallows_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, log) = scripted_docker(dir.path(), "exit 1");
        docker.remove_container("box", FAST).unwrap();
        assert_eq!(
            recorded_calls(&log),
            vec![sv(&["stop", "box"]), sv(&["rm", "box"])]
        );
    }

    // --- cleanup registry ------------------------------------------------------

    #[test]
    fn cleanup_registry_flushes_registered_then_clears() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, log) = scripted_docker(dir.path(), "");
        let registry = CleanupRegistry::new();
        registry.register("c1", FAST);
        registry.register("c2", FAST);
        assert_eq!(
            registry.active_names(),
            BTreeSet::from(["c1".to_owned(), "c2".to_owned()])
        );
        let outcomes = registry.flush(&docker);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|(_, outcome)| outcome.is_ok()));
        assert_eq!(
            recorded_calls(&log),
            vec![sv(&["rm", "-f", "c1"]), sv(&["rm", "-f", "c2"])]
        );
        assert!(registry.flush(&docker).is_empty());
        assert!(registry.active_names().is_empty());
    }

    #[test]
    fn unregister_drops_from_registry() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, log) = scripted_docker(dir.path(), "");
        let registry = CleanupRegistry::new();
        registry.register("c3", FAST);
        registry.unregister("c3");
        assert!(registry.flush(&docker).is_empty());
        assert!(recorded_calls(&log).is_empty());
    }

    #[test]
    fn flush_reports_a_timeout_instead_of_raising() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, _) = scripted_docker(dir.path(), HANG_BODY);
        let registry = CleanupRegistry::new();
        registry.register("wedged", TINY);
        let started = Instant::now();
        let outcomes = registry.flush(&docker);
        assert!(started.elapsed() < WALL_BUDGET);
        assert!(outcomes[0].1.as_ref().unwrap_err().is_timeout());
    }

    #[test]
    fn global_registry_is_a_single_instance() {
        let a = CleanupRegistry::global();
        let b = CleanupRegistry::global();
        assert!(Arc::ptr_eq(&a, &b));
        register_container_cleanup("global-probe", FAST);
        assert!(active_container_names().contains("global-probe"));
        a.unregister("global-probe");
        assert!(!active_container_names().contains("global-probe"));
    }

    // --- live daemon -------------------------------------------------------------

    struct LiveContainer {
        docker: Docker,
        name: String,
    }

    impl LiveContainer {
        fn start(spec: RunSpec) -> Self {
            let docker = Docker::new();
            let name = spec.name.clone();
            docker
                .start_container(&spec, Some(DEFAULT_START_TIMEOUT))
                .unwrap();
            Self { docker, name }
        }

        fn alpine() -> Self {
            Self::start(
                RunSpec::new(unique_name("flywheel-test"), ALPINE)
                    .with_command(sv(&["sleep", "infinity"])),
            )
        }

        fn exec(&self, command: &str, options: ExecOptions) -> ExecResult {
            self.docker
                .exec_in_container(&self.name, command, options, None)
                .unwrap()
        }
    }

    impl Drop for LiveContainer {
        fn drop(&mut self) {
            let _ = self
                .docker
                .force_remove_container(&self.name, default_management_timeout());
        }
    }

    #[test]
    fn live_exec_streams_lines_and_returns_exit_code() {
        require_docker!();
        let _daemon = live_lock();
        let container = LiveContainer::alpine();
        let mut lines = Vec::new();
        let mut observe = |line: &str| lines.push(line.to_owned());
        let result = container
            .docker
            .exec_in_container(
                &container.name,
                "printf 'a\\nb\\nc\\n'",
                ExecOptions::default(),
                Some(&mut observe),
            )
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(lines, vec!["a", "b", "c"]);
        assert_eq!(result.stdout, "a\nb\nc");
    }

    #[test]
    fn live_exec_nonzero_exit_is_reported() {
        require_docker!();
        let _daemon = live_lock();
        let container = LiveContainer::alpine();
        assert_eq!(
            container.exec("exit 7", ExecOptions::default()).exit_code,
            7
        );
    }

    #[test]
    fn live_exec_pipes_stdin_and_captures_stderr() {
        require_docker!();
        let _daemon = live_lock();
        let container = LiveContainer::alpine();
        let result = container.exec(
            "cat; echo oops >&2",
            ExecOptions {
                stdin: Some("piped-prompt".into()),
                ..ExecOptions::default()
            },
        );
        assert_eq!(result.stdout, "piped-prompt");
        assert_eq!(result.stderr.trim(), "oops");
    }

    #[test]
    fn live_exec_timeout_bounds_a_silent_hang() {
        require_docker!();
        let _daemon = live_lock();
        let container = LiveContainer::alpine();
        let started = Instant::now();
        let err = container
            .docker
            .exec_in_container(
                &container.name,
                "sleep 30",
                ExecOptions {
                    timeout: Some(Duration::from_secs(2)),
                    ..ExecOptions::default()
                },
                None,
            )
            .unwrap_err();
        assert!(matches!(err, DockerError::ExecTimeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    /// Whether container pid `pid` is gone (or a zombie: the alpine
    /// `sleep infinity` pid 1 never reaps).
    fn container_pid_gone(container: &LiveContainer, pid: &str) -> bool {
        let probe = container.exec(
            &format!("cat /proc/{pid}/stat 2>/dev/null || echo GONE"),
            ExecOptions::default(),
        );
        let out = probe.stdout.trim();
        out == "GONE"
            || out
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.split_whitespace().next() == Some("Z"))
    }

    fn wait_container_pid_gone(container: &LiveContainer, pid: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if container_pid_gone(container, pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// An agent tree inside the container whose grandchild ignores SIGTERM
    /// (an MCP server / Bash tool stand-in); returns the handle and the
    /// grandchild's in-container pid.
    async fn spawn_container_tree(
        name: &str,
    ) -> (Box<dyn flywheel_agents::RunningProcess>, String) {
        use flywheel_agents::{DockerExecHost, ExecutionHost, ProcessPlan};
        let host = DockerExecHost::new(name);
        let mut process = host
            .spawn(ProcessPlan::new(sv(&[
                "/bin/sh",
                "-c",
                "(trap '' TERM; exec sleep 300) & echo $!; wait",
            ])))
            .await
            .unwrap();
        let pid = process.next_line().await.unwrap().unwrap();
        (process, pid.trim().to_owned())
    }

    #[test]
    fn live_docker_exec_kill_terminates_the_in_container_tree() {
        require_docker!();
        let _daemon = live_lock();
        let container = LiveContainer::alpine();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut process, grandchild) = runtime.block_on(spawn_container_tree(&container.name));
        assert!(!container_pid_gone(&container, &grandchild));
        let started = Instant::now();
        let exit = runtime.block_on(async {
            process.kill(Duration::from_secs(1)).await;
            process.wait().await
        });
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(exit.returncode.is_some_and(|code| code != 0), "{exit:?}");
        // Killing only the local `docker exec` client would leave the agent
        // running in the container.
        assert!(container_pid_gone(&container, &grandchild));
        // The KILL sweep removed the pgid file.
        let leftover = container.exec(
            "ls /tmp | grep -c '^flywheel-agent-' || true",
            ExecOptions::default(),
        );
        assert_eq!(leftover.stdout.trim(), "0");
    }

    #[test]
    fn live_dropped_docker_exec_handle_terminates_the_in_container_tree() {
        require_docker!();
        let _daemon = live_lock();
        let container = LiveContainer::alpine();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (process, grandchild) = runtime.block_on(spawn_container_tree(&container.name));
        assert!(!container_pid_gone(&container, &grandchild));
        drop(process);
        assert!(wait_container_pid_gone(&container, &grandchild));
    }

    #[test]
    fn live_runtime_timeout_terminates_the_in_container_agent() {
        require_docker!();
        let _daemon = live_lock();
        let container = LiveContainer::alpine();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // A claude-code "agent" that announces itself, records its tree, and
        // goes silent; the run's own timeout must stop it in the container.
        let script = "printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}'; \
                      (trap '' TERM; exec sleep 300) & echo $! > /tmp/agent-grandchild.pid; \
                      sleep 300";
        let mut configuration = flywheel_agents::AgentConfiguration::new("claude-code");
        configuration.command_override = Some(sv(&["/bin/sh", "-c", script]));
        let request = flywheel_agents::RunRequest {
            prompt: "p".into(),
            working_directory: std::path::PathBuf::from("/tmp"),
            configuration,
            timeout_seconds: Some(1.0),
        };
        let host: std::sync::Arc<dyn flywheel_agents::ExecutionHost> =
            std::sync::Arc::new(flywheel_agents::DockerExecHost::new(&container.name));
        let run = runtime
            .block_on(flywheel_agents::AgentRuntime::new().run(
                request,
                flywheel_agents::RunOptions::default().with_host(host),
            ))
            .unwrap();
        assert_eq!(run.failure.as_ref().unwrap().error_type, "timeout");
        let grandchild = container.exec("cat /tmp/agent-grandchild.pid", ExecOptions::default());
        let grandchild = grandchild.stdout.trim().to_owned();
        assert!(!grandchild.is_empty());
        assert!(container_pid_gone(&container, &grandchild));
    }

    #[test]
    fn live_bind_mount_uid_alignment() {
        require_docker!();
        let _daemon = live_lock();
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        // SAFETY: getuid/getgid are always-safe libc queries.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let container = LiveContainer::start(
            RunSpec::new(unique_name("flywheel-test"), ALPINE)
                .with_mounts(vec![VolumeMount::new(work.to_string_lossy(), "/work")])
                .with_workdir("/work")
                .with_user(format!("{uid}:{gid}"))
                .with_command(sv(&["sleep", "infinity"])),
        );
        let result = container.exec("echo written > /work/out.txt", ExecOptions::default());
        assert_eq!(result.exit_code, 0);
        let produced = work.join("out.txt");
        assert_eq!(
            std::fs::read_to_string(&produced).unwrap().trim(),
            "written"
        );
        assert_eq!(std::fs::metadata(&produced).unwrap().uid(), uid);
    }

    #[test]
    fn live_internal_network_blocks_egress() {
        require_docker!();
        let _daemon = live_lock();
        let docker = Docker::new();
        let net = unique_name("flywheel-test-net");
        docker
            .ensure_internal_network(&net, default_management_timeout())
            .unwrap();
        assert!(
            docker
                .network_exists(&net, default_management_timeout())
                .unwrap()
        );
        // Idempotent: a second ensure is a no-op.
        docker
            .ensure_internal_network(&net, default_management_timeout())
            .unwrap();
        {
            let container = LiveContainer::start(
                RunSpec::new(unique_name("flywheel-test"), ALPINE)
                    .with_networks(vec![net.clone()])
                    .with_command(sv(&["sleep", "infinity"])),
            );
            // An --internal network has no gateway, so any egress attempt
            // fails fast and non-zero regardless of the host's connectivity.
            let result = container.exec(
                "wget -T 3 -t 1 -q -O /dev/null http://1.1.1.1/ ; echo $?",
                ExecOptions::default(),
            );
            assert_eq!(result.exit_code, 0);
            assert_ne!(result.stdout.trim().lines().last().unwrap(), "0");
        }
        let _ = docker.run(
            sv(&["docker", "network", "rm", &net]),
            default_management_timeout(),
        );
    }

    #[test]
    fn live_owned_containers_are_listed_and_reaped_by_label() {
        require_docker!();
        let _daemon = live_lock();
        let docker = Docker::new();
        let timeout = default_management_timeout();
        let name = unique_name("flywheel-test");
        docker
            .start_container(
                &RunSpec::new(&name, ALPINE).with_command(sv(&["sleep", "infinity"])),
                Some(DEFAULT_START_TIMEOUT),
            )
            .unwrap();
        let owned = docker.list_owned_containers(timeout).unwrap();
        assert!(owned.contains(&name));
        // Only this test's container is an orphan here: every other owned
        // container on the daemon is treated as active so a live worker's
        // sandbox on the same machine is never touched.
        let active: BTreeSet<String> = owned.into_iter().filter(|n| *n != name).collect();
        assert_eq!(
            docker.reap_orphan_containers(&active, timeout).unwrap(),
            vec![name.clone()]
        );
        assert!(
            !docker
                .list_owned_containers(timeout)
                .unwrap()
                .contains(&name)
        );
        // remove_container on an already-gone container is a clean no-op.
        docker.remove_container(&name, timeout).unwrap();
    }

    #[test]
    fn live_remove_container_stops_and_removes_a_running_container() {
        require_docker!();
        let _daemon = live_lock();
        let docker = Docker::new();
        let timeout = default_management_timeout();
        let name = unique_name("flywheel-test");
        docker
            .start_container(
                &RunSpec::new(&name, ALPINE).with_command(sv(&["sleep", "infinity"])),
                Some(DEFAULT_START_TIMEOUT),
            )
            .unwrap();
        docker.remove_container(&name, timeout).unwrap();
        let gone = docker
            .run(
                sv(&["docker", "inspect", &name, "--format", "{{.Id}}"]),
                timeout,
            )
            .is_err();
        assert!(gone, "container {name} survived remove_container");
    }

    #[test]
    fn live_dead_container_stays_inspectable_until_teardown_removes_it() {
        require_docker!();
        let _daemon = live_lock();
        let docker = Docker::new();
        let timeout = default_management_timeout();
        let name = unique_name("flywheel-test");
        docker
            .start_container(
                &RunSpec::new(&name, ALPINE).with_command(sv(&["sh", "-c", "echo died; exit 3"])),
                Some(DEFAULT_START_TIMEOUT),
            )
            .unwrap();
        // The process exits on its own; the container lingers as an exited,
        // inspectable record of what happened.
        let format = "{{.State.Status}} {{.State.ExitCode}}";
        let deadline = Instant::now() + Duration::from_secs(30);
        let state = loop {
            let state = docker
                .run(
                    sv(&["docker", "inspect", &name, "--format", format]),
                    timeout,
                )
                .unwrap();
            if state.trim() == "exited 3" || Instant::now() > deadline {
                break state;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(state.trim(), "exited 3");
        let logs = docker.run(sv(&["docker", "logs", &name]), timeout).unwrap();
        assert_eq!(logs.trim(), "died");
        docker.remove_container(&name, timeout).unwrap();
        let gone = docker
            .run(
                sv(&["docker", "inspect", &name, "--format", "{{.Id}}"]),
                timeout,
            )
            .is_err();
        assert!(gone, "container {name} survived remove_container");
    }
}
