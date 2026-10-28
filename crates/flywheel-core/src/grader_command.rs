//! Command grader runner.
//!
//! Executes the `command` graders declared on a [`Task`] in list order,
//! persisting one row per execution to a [`GraderResultStore`] in the
//! documented append-only shape.
//!
//! Contract surface (see `docs/task-schema.md` and the schema header in
//! `persistence-schema.sql`):
//!
//! * Graders run in the order they appear on `task.graders`. On the first
//!   failure, later command graders are skipped (the cost-order contract).
//!   Non-`command` graders are never executed; their ordinals are still
//!   preserved so audits can reconstruct the original list from
//!   `grader_results.ordinal` alone.
//! * Pass is strictly `exit_code == 0`. Non-zero exit, signal termination,
//!   timeout, and a subprocess that never started are all failures and are
//!   distinguishable in `payload` via the [`Termination`] discriminator
//!   (`exited` | `signal` | `timeout` | `spawn_failure`). A `spawn_failure`
//!   is an infra failure, not the code under test asserting false; the
//!   harness routes it to the retryable internal-error class. A grader
//!   killed by SIGINT/SIGTERM ([`is_operator_signal`]) is operator-induced
//!   interruption, which the harness classifies as `grader_signaled`.
//! * `grader_spec` snapshots the input grader verbatim at run time so later
//!   edits to the task definition do not rewrite historical truth.
//! * stdout/stderr capture is bounded (tail-only) with a deterministic byte
//!   boundary. Full streams may optionally be written to `artifacts_dir`;
//!   when written, their paths are surfaced via `stdout_path` /
//!   `stderr_path` in the payload.
//!
//! The payload is a documented superset of
//! `{run, exit_code, stdout_tail, stderr_tail, stdout_path?, stderr_path?}`:
//! it also carries `stdout_truncated`, `stderr_truncated`, `termination`,
//! and the per-termination detail (`signal`, `timeout_seconds`,
//! `spawn_error`).
//!
//! Trust boundary: `grader.run` is executed under `/bin/sh -c` with the
//! caller-supplied `cwd` / `env` (the harness passes the task sandbox as
//! `cwd`). A task file is therefore arbitrary code run as the worker, and
//! the captured stdout/stderr are persisted verbatim. The runner does not
//! sandbox or redact; that is a deployment concern.
//!
//! Each grader is spawned in its own process group (`setsid`-equivalent).
//! On a wall-clock timeout the *whole group* is SIGKILLed, not just the
//! `/bin/sh` wrapper, so grandchildren (the actual command behind the
//! shell) do not survive and hold the output pipes open. While the shell is
//! unreaped the group is registered in
//! [`flywheel_agents::process_groups`]: a daemon's shutdown sweep reaches
//! it, and a grading future dropped mid-run SIGKILLs it.

use crate::deadline::Deadline;
use crate::store::{GraderResultRecord, GraderResultStore, JsonObject, StoreError};
use crate::task::{CommandGrader, Grader, GraderType, Task};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// Default byte cap for each captured stdout/stderr tail.
pub const DEFAULT_TAIL_BYTES: usize = 8192;

/// Signal numbers that mark operator-induced interruption of a grader:
/// SIGINT (2) and SIGTERM (15). The harness routes a grader killed by one
/// of these to `INTERRUPTED` (classification `grader_signaled`) so the retry
/// budget is preserved, instead of treating it as a validation failure.
pub const OPERATOR_SIGNALS: [i32; 2] = [2, 15];

/// The shell every `command` grader runs under.
pub const SHELL: &str = "/bin/sh";

const SIGKILL: i32 = 9;
const READ_CHUNK_BYTES: usize = 8192;

/// How a command grader's subprocess ended. Persisted as the payload's
/// `termination` key; audits must be able to tell the four classes apart
/// from the payload alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Termination {
    /// The process ran to completion and returned an exit code.
    Exited,
    /// The process was terminated by a signal (not by the runner's timeout).
    Signal,
    /// The runner's wall-clock ceiling fired and the process group was
    /// SIGKILLed.
    Timeout,
    /// The subprocess never started (bad cwd, exec failure, resource
    /// exhaustion). An infra failure, retryable.
    SpawnFailure,
}

impl Termination {
    pub const fn as_str(self) -> &'static str {
        match self {
            Termination::Exited => "exited",
            Termination::Signal => "signal",
            Termination::Timeout => "timeout",
            Termination::SpawnFailure => "spawn_failure",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "exited" => Some(Termination::Exited),
            "signal" => Some(Termination::Signal),
            "timeout" => Some(Termination::Timeout),
            "spawn_failure" => Some(Termination::SpawnFailure),
            _ => None,
        }
    }
}

impl fmt::Display for Termination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runner inputs that do not vary per grader.
///
/// `env`, when set, is the *complete* environment the grader runs with --
/// it replaces the inherited environment (the orchestrator resolves ambient
/// env plus `[sandbox.env]` overrides into one mapping). `None` inherits the
/// worker process environment unchanged, so a run with no overrides is
/// byte-identical to running the command by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandGraderOptions {
    /// Working directory for every grader; `None` inherits the worker's.
    pub cwd: Option<PathBuf>,
    /// Full replacement environment; `None` inherits the worker's.
    pub env: Option<BTreeMap<String, String>>,
    /// Wall-clock ceiling per grader; `None` waits unbounded.
    pub per_grader_timeout: Option<Duration>,
    pub stdout_tail_bytes: usize,
    pub stderr_tail_bytes: usize,
    /// When set, the full stdout/stderr of every grader is written under
    /// this directory as `grader-NNN-<label>.stdout` / `.stderr` and the
    /// paths are surfaced in the payload.
    pub artifacts_dir: Option<PathBuf>,
}

impl Default for CommandGraderOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            env: None,
            per_grader_timeout: None,
            stdout_tail_bytes: DEFAULT_TAIL_BYTES,
            stderr_tail_bytes: DEFAULT_TAIL_BYTES,
            artifacts_dir: None,
        }
    }
}

/// Failures the runner itself cannot absorb into a grader row.
///
/// A grader that fails to *run* is recorded (as `spawn_failure`), never
/// raised. Only persistence and artifact-file failures surface here.
#[derive(Debug, thiserror::Error)]
pub enum CommandGraderError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("writing grader artifact {path}: {source}")]
    Artifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Return the last `max_bytes` of `data` decoded as UTF-8 (lossily) plus a
/// truncation flag.
///
/// The boundary is byte-deterministic: identical input bytes produce an
/// identical tail string regardless of locale or process state. Multi-byte
/// characters cut by the boundary are replaced with U+FFFD.
pub fn bounded_tail(data: &[u8], max_bytes: usize) -> (String, bool) {
    if max_bytes == 0 || data.is_empty() {
        return (String::new(), !data.is_empty());
    }
    if data.len() <= max_bytes {
        return (String::from_utf8_lossy(data).into_owned(), false);
    }
    let tail = &data[data.len() - max_bytes..];
    (String::from_utf8_lossy(tail).into_owned(), true)
}

/// True when `signal` is one of the operator-interruption signals
/// ([`OPERATOR_SIGNALS`]).
pub fn is_operator_signal(signal: i32) -> bool {
    OPERATOR_SIGNALS.contains(&signal)
}

/// Read the `termination` discriminator back out of a persisted command
/// payload. A row written before the discriminator existed reads as
/// `Exited`, matching the harness's `payload.get("termination", "exited")`.
pub fn payload_termination(payload: &JsonObject) -> Termination {
    payload
        .get("termination")
        .and_then(Value::as_str)
        .and_then(Termination::parse)
        .unwrap_or(Termination::Exited)
}

/// The `signal` number from a persisted command payload, if present.
pub fn payload_signal(payload: &JsonObject) -> Option<i32> {
    payload
        .get("signal")
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
}

/// True when `record` is a failed command grader killed by an operator
/// signal (SIGINT/SIGTERM) -- the harness's `grader_signaled` class.
pub fn is_operator_signaled(record: &GraderResultRecord) -> bool {
    !record.passed
        && record.grader_type == GraderType::Command
        && payload_termination(&record.payload) == Termination::Signal
        && payload_signal(&record.payload).is_some_and(is_operator_signal)
}

/// True when `record` is a failed command grader whose subprocess never
/// started -- the harness's `grader_spawn_failure` class.
pub fn is_spawn_failed(record: &GraderResultRecord) -> bool {
    !record.passed
        && record.grader_type == GraderType::Command
        && payload_termination(&record.payload) == Termination::SpawnFailure
}

/// Snapshot the grader object as it appeared in the task at run time.
pub fn grader_spec_snapshot(grader: &CommandGrader) -> JsonObject {
    let mut spec = JsonObject::new();
    spec.insert("type".into(), json!("command"));
    spec.insert("run".into(), json!(grader.run));
    if let Some(name) = &grader.name {
        spec.insert("name".into(), json!(name));
    }
    spec
}

fn safe_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// One captured output stream: the retained bytes plus the total number of
/// bytes the process actually wrote. When the capture was capped, `data`
/// holds only the last `cap` bytes; otherwise it holds everything.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CapturedStream {
    data: Vec<u8>,
    total_bytes: usize,
}

impl CapturedStream {
    fn tail(&self, max_bytes: usize) -> (String, bool) {
        let (text, _) = bounded_tail(&self.data, max_bytes);
        (text, self.total_bytes > max_bytes)
    }
}

/// Drain `reader` to EOF, retaining at most `cap` trailing bytes when a cap
/// is given. A read error ends the capture with whatever was collected: the
/// grader's verdict comes from its exit status, not from a pipe failure.
async fn capture_stream<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    cap: Option<usize>,
) -> CapturedStream {
    let mut captured = CapturedStream::default();
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        captured.total_bytes += n;
        captured.data.extend_from_slice(&chunk[..n]);
        if let Some(cap) = cap
            && captured.data.len() > cap
        {
            let excess = captured.data.len() - cap;
            captured.data.drain(..excess);
        }
    }
    captured
}

fn write_artifact(path: &Path, content: &[u8]) -> Result<(), CommandGraderError> {
    let io = |source| CommandGraderError::Artifact {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    std::fs::write(path, content).map_err(io)
}

#[cfg(unix)]
unsafe extern "C" {
    fn killpg(pgrp: i32, sig: i32) -> i32;
}

/// SIGKILL the grader's whole process group, falling back to the direct
/// child when the group is already gone. `pgid` is captured at spawn time
/// because `Child::id()` is `None` once the shell has been reaped -- and a
/// reaped shell with a live grandchild holding the pipes is exactly the
/// case the group kill exists for.
async fn kill_process_group(child: &mut Child, pgid: Option<i32>) {
    #[cfg(unix)]
    if let Some(pgid) = pgid {
        // SAFETY: killpg(2) takes two plain integers and touches no memory.
        // The group id is the child's own pid because the child was spawned
        // with process_group(0), so no unrelated process group can be
        // targeted.
        let rc = unsafe { killpg(pgid, SIGKILL) };
        if rc == 0 {
            return;
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
    // Best effort: a kill on an already-reaped child reports an error we
    // cannot act on; the subsequent wait() observes the real status.
    let _ = child.kill().await;
}

/// `exit_code` as Python reports it: the exit status, or the negated
/// signal number when the process was signal-terminated.
fn python_exit_code(status: &std::process::ExitStatus) -> Option<i32> {
    if let Some(code) = status.code() {
        return Some(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Some(-signal);
        }
    }
    None
}

fn build_command(run: &str, options: &CommandGraderOptions) -> Command {
    let mut command = Command::new(SHELL);
    command
        .arg("-c")
        .arg(run)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }
    if let Some(env) = &options.env {
        command.env_clear();
        command.envs(env);
    }
    #[cfg(unix)]
    command.process_group(0);
    command
}

struct Execution {
    exit_code: Option<i32>,
    termination: Termination,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

type CaptureSlot = Arc<Mutex<Option<CapturedStream>>>;

/// Drain one pipe on its own task, depositing the capture into `slot` so
/// the result survives however many times the supervisor polls the handle.
fn spawn_capture<R>(pipe: Option<R>, cap: Option<usize>) -> (JoinHandle<()>, CaptureSlot)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let slot: CaptureSlot = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&slot);
    let handle = tokio::spawn(async move {
        let captured = match pipe {
            Some(pipe) => capture_stream(pipe, cap).await,
            None => CapturedStream::default(),
        };
        *sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(captured);
    });
    (handle, slot)
}

fn take_capture(slot: &CaptureSlot) -> CapturedStream {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .unwrap_or_default()
}

/// Drive a spawned child to completion under the wall-clock ceiling,
/// draining both pipes concurrently. "Completion" means the shell has
/// exited *and* both pipes have reached EOF, so a detached grandchild that
/// keeps a pipe open is treated like the Python `communicate()` -- it times
/// out and the whole group is killed.
async fn supervise(mut child: Child, deadline: &Deadline, capture_cap: Option<usize>) -> Execution {
    let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
    // Registered for the life of the unreaped shell so a daemon shutdown
    // sweep reaches the grader's own process group, and so a supervise
    // future dropped mid-run SIGKILLs the group rather than orphaning it.
    let mut group = pgid
        .filter(|_| cfg!(unix))
        .map(flywheel_agents::process_groups::GroupRegistration::register);
    let (mut stdout_task, stdout_slot) = spawn_capture(child.stdout.take(), capture_cap);
    let (mut stderr_task, stderr_slot) = spawn_capture(child.stderr.take(), capture_cap);

    let finished = deadline
        .bound(async {
            let status = child.wait().await;
            (&mut stdout_task).await.ok();
            (&mut stderr_task).await.ok();
            status
        })
        .await;

    let (status, timed_out) = match finished {
        Ok(status) => (status, false),
        Err(_) => {
            kill_process_group(&mut child, pgid).await;
            let status = child.wait().await;
            // A handle already driven to completion above must not be
            // polled again; its capture is in the slot either way.
            if !stdout_task.is_finished() {
                stdout_task.await.ok();
            }
            if !stderr_task.is_finished() {
                stderr_task.await.ok();
            }
            (status, true)
        }
    };
    if let Some(group) = group.as_mut() {
        group.release();
    }
    let stdout = take_capture(&stdout_slot);
    let stderr = take_capture(&stderr_slot);

    let exit_code = status.as_ref().ok().and_then(python_exit_code);
    let termination = if timed_out {
        Termination::Timeout
    } else if exit_code.is_some_and(|code| code < 0) {
        Termination::Signal
    } else {
        Termination::Exited
    };
    Execution {
        exit_code,
        termination,
        stdout,
        stderr,
    }
}

fn base_payload(
    run: &str,
    exit_code: Option<i32>,
    stdout: (String, bool),
    stderr: (String, bool),
    termination: Termination,
) -> JsonObject {
    let mut payload = JsonObject::new();
    payload.insert("run".into(), json!(run));
    payload.insert("exit_code".into(), json!(exit_code));
    payload.insert("stdout_tail".into(), json!(stdout.0));
    payload.insert("stderr_tail".into(), json!(stderr.0));
    payload.insert("stdout_truncated".into(), json!(stdout.1));
    payload.insert("stderr_truncated".into(), json!(stderr.1));
    payload.insert("termination".into(), json!(termination.as_str()));
    payload
}

/// The identity of one grader row: everything a receipt carries that is
/// fixed before the grader runs.
struct RowScope<'a> {
    run_id: &'a str,
    attempt_number: u32,
    ordinal: u32,
    grader: &'a CommandGrader,
    ts: DateTime<Utc>,
}

impl RowScope<'_> {
    fn record(&self, passed: bool, duration_ms: u64, payload: JsonObject) -> GraderResultRecord {
        GraderResultRecord {
            run_id: self.run_id.to_owned(),
            attempt_number: self.attempt_number,
            ordinal: self.ordinal,
            grader_type: GraderType::Command,
            grader_spec: grader_spec_snapshot(self.grader),
            passed,
            duration_ms,
            payload,
            ts: self.ts,
            grader_name: self.grader.name.clone(),
            id: None,
        }
    }
}

/// Run every `command` grader on `task.graders` in list order.
///
/// For each command grader:
///
/// 1. Spawn `/bin/sh -c <run>` in its own process group with the supplied
///    `cwd` / `env`.
/// 2. Wait for completion (shell exit plus pipe EOF), enforcing
///    `per_grader_timeout` if set; on timeout SIGKILL the whole group.
/// 3. Capture bounded tails of stdout/stderr. If `artifacts_dir` is set,
///    also persist the full streams alongside their tail in the payload.
/// 4. Append one [`GraderResultRecord`] to `store` whose `grader_spec`
///    snapshots the grader, `payload` matches the documented command shape,
///    and `ordinal` is the grader's index in `task.graders`.
///
/// On the first failing command grader (including a spawn failure), later
/// command graders are not executed. Non-command graders are skipped
/// without persisting any row.
///
/// `now` is the receipt clock, injected so a test can pin timestamps.
/// Returns the persisted records in execution order with store-assigned
/// `id`s.
pub async fn run_command_graders<S, F>(
    task: &Task,
    store: &S,
    run_id: &str,
    attempt_number: u32,
    options: &CommandGraderOptions,
    mut now: F,
) -> Result<Vec<GraderResultRecord>, CommandGraderError>
where
    S: GraderResultStore + ?Sized,
    F: FnMut() -> DateTime<Utc>,
{
    // Without an artifacts dir only the tails are ever read, so there is no
    // reason to hold a chatty grader's whole output in memory.
    let capture_cap = match options.artifacts_dir {
        Some(_) => None,
        None => Some(options.stdout_tail_bytes.max(options.stderr_tail_bytes)),
    };
    let mut persisted = Vec::new();

    for (index, grader) in task.graders.iter().enumerate() {
        let Grader::Command(grader) = grader else {
            continue;
        };
        let scope = RowScope {
            run_id,
            attempt_number,
            ordinal: u32::try_from(index).expect("grader list index fits in u32"),
            grader,
            ts: now(),
        };
        let ordinal = scope.ordinal;
        let started = Instant::now();

        let child = match build_command(&grader.run, options).spawn() {
            Ok(child) => child,
            Err(err) => {
                let mut payload = base_payload(
                    &grader.run,
                    None,
                    (String::new(), false),
                    (String::new(), false),
                    Termination::SpawnFailure,
                );
                payload.insert("spawn_error".into(), json!(err.to_string()));
                let duration_ms = started.elapsed().as_millis() as u64;
                let row = scope.record(false, duration_ms, payload);
                persisted.push(store.append_grader_result(&row)?);
                break;
            }
        };

        let deadline = Deadline::from_ceiling(options.per_grader_timeout);
        let execution = supervise(child, &deadline, capture_cap).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        let mut payload = base_payload(
            &grader.run,
            execution.exit_code,
            execution.stdout.tail(options.stdout_tail_bytes),
            execution.stderr.tail(options.stderr_tail_bytes),
            execution.termination,
        );
        match execution.termination {
            Termination::Timeout => {
                if let Some(ceiling) = options.per_grader_timeout {
                    payload.insert("timeout_seconds".into(), json!(ceiling.as_secs_f64()));
                }
            }
            Termination::Signal => {
                let signal = -execution.exit_code.unwrap_or(0);
                payload.insert("signal".into(), json!(signal));
            }
            Termination::Exited | Termination::SpawnFailure => {}
        }

        if let Some(dir) = &options.artifacts_dir {
            let label = safe_label(grader.name.as_deref().unwrap_or("command"));
            let stem = format!("grader-{ordinal:03}-{label}");
            let stdout_path = dir.join(format!("{stem}.stdout"));
            let stderr_path = dir.join(format!("{stem}.stderr"));
            write_artifact(&stdout_path, &execution.stdout.data)?;
            write_artifact(&stderr_path, &execution.stderr.data)?;
            payload.insert("stdout_path".into(), json!(stdout_path.to_string_lossy()));
            payload.insert("stderr_path".into(), json!(stderr_path.to_string_lossy()));
        }

        let passed =
            execution.termination != Termination::Timeout && execution.exit_code == Some(0);
        let row = scope.record(passed, duration_ms, payload);
        persisted.push(store.append_grader_result(&row)?);

        if !passed {
            break;
        }
    }

    Ok(persisted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{ManualGrader, RubricGrader, TranscriptGrader};
    use std::sync::Mutex;

    /// A real, minimal append-only grader-result store.
    #[derive(Default)]
    struct RecordingStore {
        rows: Mutex<Vec<GraderResultRecord>>,
    }

    impl GraderResultStore for RecordingStore {
        fn append_grader_result(
            &self,
            result: &GraderResultRecord,
        ) -> Result<GraderResultRecord, StoreError> {
            let mut rows = self.rows.lock().unwrap();
            let mut stored = result.clone();
            stored.id = Some(rows.len() as i64 + 1);
            rows.push(stored.clone());
            Ok(stored)
        }

        fn list_grader_results(
            &self,
            run_id: &str,
            attempt_number: u32,
        ) -> Result<Vec<GraderResultRecord>, StoreError> {
            let mut rows: Vec<_> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.run_id == run_id && r.attempt_number == attempt_number)
                .cloned()
                .collect();
            rows.sort_by_key(|r| r.ordinal);
            Ok(rows)
        }
    }

    fn cmd(run: &str, name: Option<&str>) -> Grader {
        Grader::Command(CommandGrader {
            run: run.into(),
            name: name.map(str::to_owned),
        })
    }

    fn task(graders: Vec<Grader>) -> Task {
        Task::new("g", graders)
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_780_000_000 + secs, 0).unwrap()
    }

    async fn run(
        task: &Task,
        store: &RecordingStore,
        options: &CommandGraderOptions,
    ) -> Vec<GraderResultRecord> {
        run_command_graders(task, store, "r1", 1, options, || at(0))
            .await
            .unwrap()
    }

    fn termination(row: &GraderResultRecord) -> &str {
        row.payload["termination"].as_str().unwrap()
    }

    fn tail<'a>(row: &'a GraderResultRecord, key: &str) -> &'a str {
        row.payload[key].as_str().unwrap()
    }

    // --- Pass / order ------------------------------------------------------

    #[tokio::test]
    async fn passing_grader_records_exit_zero_and_exited_termination() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("true", Some("ok"))]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(row.passed);
        assert_eq!(row.grader_type, GraderType::Command);
        assert_eq!(row.grader_name.as_deref(), Some("ok"));
        assert_eq!(row.ordinal, 0);
        assert_eq!(row.id, Some(1));
        assert_eq!(row.ts, at(0));
        assert_eq!(row.payload["run"], "true");
        assert_eq!(row.payload["exit_code"], 0);
        assert_eq!(termination(row), "exited");
        assert!(!row.payload.contains_key("signal"));
        assert!(!row.payload.contains_key("timeout_seconds"));
        assert_eq!(
            row.payload.keys().collect::<Vec<_>>(),
            vec![
                "run",
                "exit_code",
                "stdout_tail",
                "stderr_tail",
                "stdout_truncated",
                "stderr_truncated",
                "termination"
            ]
        );
    }

    #[tokio::test]
    async fn runs_command_graders_in_list_order() {
        let store = RecordingStore::default();
        let t = task(vec![
            cmd("echo first", Some("a")),
            cmd("echo second", Some("b")),
            cmd("echo third", Some("c")),
        ]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        let names: Vec<_> = rows
            .iter()
            .map(|r| r.grader_name.clone().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(
            rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let outs: Vec<_> = rows
            .iter()
            .map(|r| tail(r, "stdout_tail").trim().to_owned())
            .collect();
        assert_eq!(outs, vec!["first", "second", "third"]);
    }

    // --- Abort after first failure -----------------------------------------

    #[tokio::test]
    async fn first_failure_aborts_later_command_graders() {
        let store = RecordingStore::default();
        let t = task(vec![
            cmd("true", Some("ok")),
            cmd("exit 7", Some("boom")),
            cmd("echo never", Some("skipped")),
        ]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        let names: Vec<_> = rows
            .iter()
            .map(|r| r.grader_name.clone().unwrap())
            .collect();
        assert_eq!(names, vec!["ok", "boom"]);
        assert_eq!(
            rows.iter().map(|r| r.passed).collect::<Vec<_>>(),
            vec![true, false]
        );
        assert_eq!(rows[1].payload["exit_code"], 7);
        assert_eq!(termination(&rows[1]), "exited");
        let listed = store.list_grader_results("r1", 1).unwrap();
        assert_eq!(listed.len(), 2);
    }

    // --- Non-command graders are skipped, ordinals preserved ---------------

    #[tokio::test]
    async fn skips_non_command_graders_and_preserves_ordinals() {
        let store = RecordingStore::default();
        let t = task(vec![
            Grader::Rubric(RubricGrader::new(vec!["x".into()]).unwrap()),
            cmd("true", Some("cmd0")),
            Grader::Transcript(TranscriptGrader {
                max_turns: Some(5),
                ..Default::default()
            }),
            cmd("true", Some("cmd1")),
            Grader::Manual(ManualGrader::new("approve").unwrap()),
        ]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        let names: Vec<_> = rows
            .iter()
            .map(|r| r.grader_name.clone().unwrap())
            .collect();
        assert_eq!(names, vec!["cmd0", "cmd1"]);
        assert_eq!(
            rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(rows.iter().all(|r| r.grader_type == GraderType::Command));
    }

    #[tokio::test]
    async fn only_non_command_graders_yields_no_executions() {
        let store = RecordingStore::default();
        let t = task(vec![
            Grader::Rubric(RubricGrader::new(vec!["x".into()]).unwrap()),
            Grader::Manual(ManualGrader::new("approve").unwrap()),
        ]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        assert!(rows.is_empty());
        assert!(store.list_grader_results("r1", 1).unwrap().is_empty());
    }

    // --- Failure modes are distinguishable ---------------------------------

    #[tokio::test]
    async fn non_zero_exit_recorded_as_failure_with_exit_code() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("exit 42", None)]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        let row = &rows[0];
        assert!(!row.passed);
        assert_eq!(row.payload["exit_code"], 42);
        assert_eq!(termination(row), "exited");
        assert!(row.grader_name.is_none());
        assert!(!row.grader_spec.contains_key("name"));
    }

    #[tokio::test]
    async fn signal_termination_recorded_distinguishably() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("kill -KILL $$", None)]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        let row = &rows[0];
        assert!(!row.passed);
        assert_eq!(termination(row), "signal");
        assert_eq!(row.payload["signal"], 9);
        assert_eq!(row.payload["exit_code"], -9);
        assert!(!row.payload.contains_key("timeout_seconds"));
        assert_eq!(payload_termination(&row.payload), Termination::Signal);
        assert_eq!(payload_signal(&row.payload), Some(9));
        assert!(!is_operator_signaled(row));
        assert!(!is_spawn_failed(row));
    }

    #[tokio::test]
    async fn operator_signal_is_recognized_as_grader_signaled() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("kill -TERM $$", Some("interrupted"))]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        let row = &rows[0];
        assert_eq!(termination(row), "signal");
        assert_eq!(row.payload["signal"], 15);
        assert!(is_operator_signaled(row));
        assert!(is_operator_signal(2));
        assert!(!is_operator_signal(9));
    }

    #[tokio::test]
    async fn timeout_recorded_distinguishably() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("sleep 30", Some("slow"))]);
        let options = CommandGraderOptions {
            per_grader_timeout: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let started = Instant::now();
        let rows = run(&t, &store, &options).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        let row = &rows[0];
        assert!(!row.passed);
        assert_eq!(termination(row), "timeout");
        assert_eq!(row.payload["timeout_seconds"], 0.2);
        assert_ne!(row.payload["exit_code"], 0);
        assert_eq!(row.payload["exit_code"], -9);
    }

    /// Reads `/proc/<pid>/stat` and reports whether the process is still a
    /// live (non-zombie) process.
    #[cfg(target_os = "linux")]
    fn is_alive(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                // "<pid> (<comm>) <state> ..." -- comm may contain spaces, so
                // take the state after the closing paren.
                let state = stat
                    .rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().chars().next())
                    .unwrap_or(None);
                !matches!(state, Some('Z') | Some('X') | None)
            }
            Err(_) => false,
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_grader_grandchild_is_killed_with_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        // `sleep` runs as a child of the shell, i.e. a grandchild of this
        // process. Without a group kill it would be reparented to init and
        // keep stdout open after the shell dies.
        let script = format!("sleep 30 & echo $! > {}; wait", pid_file.to_string_lossy());
        let store = RecordingStore::default();
        let t = task(vec![cmd(&script, Some("hang"))]);
        let options = CommandGraderOptions {
            per_grader_timeout: Some(Duration::from_millis(300)),
            ..Default::default()
        };
        let started = Instant::now();
        let rows = run(&t, &store, &options).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(termination(&rows[0]), "timeout");

        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        assert!(!pid.is_empty());
        let mut dead = false;
        for _ in 0..40 {
            if !is_alive(&pid) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(dead, "grandchild {pid} survived the process-group kill");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropped_grading_future_kills_the_grader_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {}; wait", pid_file.to_string_lossy());
        let store = RecordingStore::default();
        let t = task(vec![cmd(&script, Some("abandoned"))]);
        let options = CommandGraderOptions::default();
        let pid = {
            let grading = run(&t, &store, &options);
            tokio::pin!(grading);
            let wait_pid = async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&pid_file)
                        && !text.trim().is_empty()
                    {
                        return text.trim().to_owned();
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };
            tokio::select! {
                _ = &mut grading => panic!("the grader cannot finish on its own"),
                pid = wait_pid => pid,
            }
            // The grading future (e.g. a harness shutdown) drops here.
        };
        let mut dead = false;
        for _ in 0..40 {
            if !is_alive(&pid) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(dead, "grandchild {pid} outlived its dropped grader");
    }

    #[tokio::test]
    async fn detached_grandchild_holding_the_pipe_is_bounded_by_the_ceiling() {
        // The shell exits immediately but its background child keeps stdout
        // open; completion must still be bounded and the group killed.
        let store = RecordingStore::default();
        let t = task(vec![cmd("sleep 30 &", Some("leaky"))]);
        let options = CommandGraderOptions {
            per_grader_timeout: Some(Duration::from_millis(300)),
            ..Default::default()
        };
        let started = Instant::now();
        let rows = run(&t, &store, &options).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(termination(&rows[0]), "timeout");
        assert!(!rows[0].passed);
    }

    #[tokio::test]
    async fn failure_modes_are_pairwise_distinguishable() {
        let s1 = RecordingStore::default();
        let nonzero = run(
            &task(vec![cmd("exit 1", Some("nonzero"))]),
            &s1,
            &CommandGraderOptions::default(),
        )
        .await;
        let s2 = RecordingStore::default();
        let signaled = run(
            &task(vec![cmd("kill -KILL $$", Some("signaled"))]),
            &s2,
            &CommandGraderOptions::default(),
        )
        .await;
        let s3 = RecordingStore::default();
        let timed_out = run(
            &task(vec![cmd("sleep 30", Some("timed_out"))]),
            &s3,
            &CommandGraderOptions {
                per_grader_timeout: Some(Duration::from_millis(200)),
                ..Default::default()
            },
        )
        .await;
        let s4 = RecordingStore::default();
        let spawn_failed = run(
            &task(vec![cmd("true", Some("spawn"))]),
            &s4,
            &CommandGraderOptions {
                cwd: Some(PathBuf::from("/no/such/dir/flywheel-spawn-failure-fixture")),
                ..Default::default()
            },
        )
        .await;
        let terminations: std::collections::BTreeSet<_> =
            [&nonzero, &signaled, &timed_out, &spawn_failed]
                .iter()
                .map(|rows| termination(&rows[0]).to_owned())
                .collect();
        assert_eq!(
            terminations.into_iter().collect::<Vec<_>>(),
            vec!["exited", "signal", "spawn_failure", "timeout"]
        );
        assert!(
            [&nonzero, &signaled, &timed_out, &spawn_failed]
                .iter()
                .all(|rows| !rows[0].passed)
        );
    }

    // --- Spawn failure -----------------------------------------------------

    #[tokio::test]
    async fn spawn_failure_records_distinct_termination_and_aborts() {
        let store = RecordingStore::default();
        let t = task(vec![
            cmd("true", Some("first")),
            cmd("echo never", Some("skipped")),
        ]);
        let options = CommandGraderOptions {
            cwd: Some(PathBuf::from("/no/such/dir/flywheel-spawn-failure-fixture")),
            ..Default::default()
        };
        let rows = run(&t, &store, &options).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(!row.passed);
        assert_eq!(row.grader_name.as_deref(), Some("first"));
        assert_eq!(termination(row), "spawn_failure");
        assert_eq!(row.payload["exit_code"], Value::Null);
        assert!(!row.payload["spawn_error"].as_str().unwrap().is_empty());
        assert!(is_spawn_failed(row));
        assert!(!is_operator_signaled(row));
    }

    // --- Bounded stdout/stderr tail ----------------------------------------

    #[tokio::test]
    async fn stdout_tail_is_bounded_by_configured_byte_cap() {
        let store = RecordingStore::default();
        let t = task(vec![cmd(
            "head -c 4096 /dev/zero | tr '\\0' x",
            Some("loud"),
        )]);
        let options = CommandGraderOptions {
            stdout_tail_bytes: 256,
            ..Default::default()
        };
        let rows = run(&t, &store, &options).await;
        let row = &rows[0];
        let out = tail(row, "stdout_tail");
        assert_eq!(out.len(), 256);
        assert!(out.ends_with('x'));
        assert_eq!(row.payload["stdout_truncated"], true);
        assert_eq!(row.payload["stderr_truncated"], false);
    }

    #[tokio::test]
    async fn stdout_tail_returns_full_when_under_cap() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("echo small", None)]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        assert_eq!(rows[0].payload["stdout_truncated"], false);
        assert_eq!(tail(&rows[0], "stdout_tail").trim(), "small");
    }

    #[tokio::test]
    async fn stderr_is_captured_independently_from_stdout() {
        let store = RecordingStore::default();
        let t = task(vec![cmd(
            "echo on-out; echo on-err 1>&2; exit 3",
            Some("mixed"),
        )]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        let row = &rows[0];
        assert_eq!(row.payload["exit_code"], 3);
        assert!(tail(row, "stdout_tail").contains("on-out"));
        assert!(tail(row, "stderr_tail").contains("on-err"));
        assert!(!tail(row, "stderr_tail").contains("on-out"));
        assert!(!tail(row, "stdout_tail").contains("on-err"));
    }

    #[test]
    fn bounded_tail_is_byte_deterministic_and_lossy_at_the_cut() {
        assert_eq!(bounded_tail(b"", 10), (String::new(), false));
        assert_eq!(bounded_tail(b"abc", 0), (String::new(), true));
        assert_eq!(bounded_tail(b"abc", 3), ("abc".into(), false));
        assert_eq!(bounded_tail(b"abcdef", 3), ("def".into(), true));
        // Cutting inside a 2-byte UTF-8 sequence replaces the orphan byte.
        let data = "xé".as_bytes();
        let (text, truncated) = bounded_tail(data, 1);
        assert!(truncated);
        assert_eq!(text, "\u{FFFD}");
    }

    // --- Environment and cwd -----------------------------------------------

    // `CARGO_PKG_NAME` is set by cargo for every test binary and never
    // synthesized by a shell (unlike `PATH`, which /bin/sh defaults when
    // absent), so it cleanly distinguishes inherit from replace.
    #[tokio::test]
    async fn inherits_worker_environment_when_env_is_none() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("echo \"${CARGO_PKG_NAME:-unset}\"", None)]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        assert_eq!(tail(&rows[0], "stdout_tail").trim(), env!("CARGO_PKG_NAME"));
    }

    #[tokio::test]
    async fn explicit_env_replaces_the_inherited_environment() {
        let store = RecordingStore::default();
        let t = task(vec![cmd(
            "echo \"$FW_GRADER_VAR:${CARGO_PKG_NAME:-unset}\"",
            None,
        )]);
        let options = CommandGraderOptions {
            env: Some(BTreeMap::from([(
                "FW_GRADER_VAR".to_owned(),
                "hello".to_owned(),
            )])),
            ..Default::default()
        };
        let rows = run(&t, &store, &options).await;
        assert_eq!(tail(&rows[0], "stdout_tail").trim(), "hello:unset");
    }

    #[tokio::test]
    async fn runs_in_the_requested_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let store = RecordingStore::default();
        let t = task(vec![cmd("pwd -P", None)]);
        let options = CommandGraderOptions {
            cwd: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let rows = run(&t, &store, &options).await;
        assert_eq!(
            PathBuf::from(tail(&rows[0], "stdout_tail").trim()),
            canonical
        );
    }

    // --- grader_spec snapshotting ------------------------------------------

    #[tokio::test]
    async fn grader_spec_snapshots_input_verbatim() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("uv run pytest tests/http", Some("tests"))]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        assert_eq!(
            Value::Object(rows[0].grader_spec.clone()),
            json!({"type": "command", "run": "uv run pytest tests/http", "name": "tests"})
        );
    }

    // --- Persistence shape -------------------------------------------------

    #[tokio::test]
    async fn payload_contains_documented_command_shape() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("echo hello", None)]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        for key in ["run", "exit_code", "stdout_tail", "stderr_tail"] {
            assert!(
                rows[0].payload.contains_key(key),
                "missing documented key: {key}"
            );
        }
    }

    #[tokio::test]
    async fn artifacts_dir_persists_full_streams_and_records_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordingStore::default();
        let t = task(vec![cmd(
            "head -c 4096 /dev/zero | tr '\\0' x; head -c 4096 /dev/zero | tr '\\0' y 1>&2",
            Some("loud name/with:odd chars"),
        )]);
        let options = CommandGraderOptions {
            stdout_tail_bytes: 128,
            stderr_tail_bytes: 128,
            artifacts_dir: Some(dir.path().join("nested")),
            ..Default::default()
        };
        let rows = run(&t, &store, &options).await;
        let row = &rows[0];
        let stdout_path = PathBuf::from(tail(row, "stdout_path"));
        let stderr_path = PathBuf::from(tail(row, "stderr_path"));
        assert_eq!(stdout_path.parent().unwrap(), dir.path().join("nested"));
        assert_eq!(
            stdout_path.file_name().unwrap().to_str().unwrap(),
            "grader-000-loud_name_with_odd_chars.stdout"
        );
        assert_eq!(std::fs::read(&stdout_path).unwrap(), vec![b'x'; 4096]);
        assert_eq!(std::fs::read(&stderr_path).unwrap(), vec![b'y'; 4096]);
        assert_eq!(tail(row, "stdout_tail").len(), 128);
        assert_eq!(tail(row, "stderr_tail").len(), 128);
        assert_eq!(row.payload["stdout_truncated"], true);
        assert_eq!(row.payload["stderr_truncated"], true);
    }

    #[tokio::test]
    async fn artifact_write_failure_surfaces_as_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let store = RecordingStore::default();
        let t = task(vec![cmd("true", None)]);
        let options = CommandGraderOptions {
            artifacts_dir: Some(blocker.join("inner")),
            ..Default::default()
        };
        let err = run_command_graders(&t, &store, "r1", 1, &options, || at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, CommandGraderError::Artifact { .. }));
        assert!(store.list_grader_results("r1", 1).unwrap().is_empty());
    }

    #[tokio::test]
    async fn each_row_keys_by_run_id_attempt_number_and_ordinal() {
        let store = RecordingStore::default();
        let t = task(vec![
            cmd("true", Some("a")),
            Grader::Rubric(RubricGrader::new(vec!["x".into()]).unwrap()),
            cmd("true", Some("c")),
        ]);
        run(&t, &store, &CommandGraderOptions::default()).await;
        let rows = store.list_grader_results("r1", 1).unwrap();
        let keys: Vec<_> = rows
            .iter()
            .map(|r| (r.run_id.as_str(), r.attempt_number, r.ordinal))
            .collect();
        assert_eq!(keys, vec![("r1", 1, 0), ("r1", 1, 2)]);
    }

    #[tokio::test]
    async fn records_are_appended_not_updated_on_rerun() {
        let store = RecordingStore::default();
        let t = task(vec![cmd("true", Some("x"))]);
        let options = CommandGraderOptions::default();
        run_command_graders(&t, &store, "r1", 1, &options, || at(0))
            .await
            .unwrap();
        run_command_graders(&t, &store, "r1", 2, &options, || at(1))
            .await
            .unwrap();
        let a1 = store.list_grader_results("r1", 1).unwrap();
        let a2 = store.list_grader_results("r1", 2).unwrap();
        assert_eq!((a1.len(), a2.len()), (1, 1));
        assert_ne!(a1[0].id, a2[0].id);
    }

    #[tokio::test]
    async fn failing_first_command_skips_all_later_graders() {
        let store = RecordingStore::default();
        let t = task(vec![
            cmd("false", Some("initial")),
            cmd("true", Some("second")),
            Grader::Rubric(RubricGrader::new(vec!["x".into()]).unwrap()),
        ]);
        let rows = run(&t, &store, &CommandGraderOptions::default()).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].grader_name.as_deref(), Some("initial"));
        assert!(!rows[0].passed);
    }

    #[test]
    fn termination_round_trips_through_serde_and_parse() {
        for t in [
            Termination::Exited,
            Termination::Signal,
            Termination::Timeout,
            Termination::SpawnFailure,
        ] {
            assert_eq!(serde_json::to_value(t).unwrap(), json!(t.as_str()));
            assert_eq!(Termination::parse(t.as_str()), Some(t));
            assert_eq!(t.to_string(), t.as_str());
        }
        assert_eq!(Termination::parse("nope"), None);
        assert_eq!(payload_termination(&JsonObject::new()), Termination::Exited);
    }
}
