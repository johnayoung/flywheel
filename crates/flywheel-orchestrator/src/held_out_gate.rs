//! The execute-time held-out landing gate (`docs/held-out-gate.md`).
//!
//! Grades an agent's *committed* result with operator-declared command
//! graders the agent never saw, then blocks landing if they fail. Two
//! halves live here:
//!
//! * The **engine**: [`HeldOutGraderSource`] (per-task-id registrations
//!   plus standing-invariant oracles), the shipped
//!   [`FilesystemHeldOutGraderSource`], and [`evaluate_held_out_gate`],
//!   which runs the registered graders against the committed tree through
//!   the same command runner the harness uses and computes a
//!   [`GateVerdict`].
//! * The **landing integration**: [`evaluate_landing_gate`] (gate only a
//!   `Done` run, only when a source is wired), the
//!   [`HeldOutGateEvaluated`] / `held-out-gate` [`LandingParked`] ledger
//!   witnesses with bounded output excerpts, and [`gate_landing`], the one
//!   call the per-task drive makes between "run finalized" and
//!   `strategy.submit` to obtain a [`LandingDecision`].
//!
//! Invariants the engine enforces:
//!
//! * **Out-of-band, never agent-facing (D-2).** Held-out graders live in a
//!   source keyed by task id that the orchestrator reads. They are never
//!   merged into the `Task` handed to the harness nor written into the
//!   agent's worktree.
//! * **The verdict is the grader exit code, never the agent's self-report
//!   (D-4).** The engine is not given the agent's terminal status.
//! * **Fail closed (D-3).** A registration that cannot be located, parsed,
//!   or executed yields `Fail`, never a pass or a silent skip. A task with
//!   *no* registration yields the distinct `NoGate` so it lands unchanged.
//! * **Command graders only (D-5).** A non-command registration is a
//!   misconfiguration and fails closed.
//! * **Graded against the committed result.** The committed tree is the
//!   `cwd` of every held-out grader.

use crate::strategy::SubmitRequest;
use chrono::{DateTime, Utc};
use flywheel_core::events::{
    DomainEvent, DomainEventPayload, GATE_EXCERPT_MAX_BYTES, GateGraderReceipt,
    HeldOutGateEvaluated, LandingParked, PARK_KIND_HELD_OUT_GATE,
};
use flywheel_core::grader_command::{
    CommandGraderOptions, Termination, bounded_tail, payload_termination, run_command_graders,
};
use flywheel_core::lifecycle::Status;
use flywheel_core::loaders::load_graders;
use flywheel_core::store::{
    DomainEventStore, GraderResultRecord, GraderResultStore, JsonObject, LifecycleStore, StoreError,
};
use flywheel_core::task::{CommandGrader, Grader, Task};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Subdirectory under the held-out root holding standing-invariant oracles.
pub const STANDING_SUBDIR: &str = "_standing";

/// A held-out registration exists for a task but cannot be loaded.
///
/// Raised by a [`HeldOutGraderSource`] when a registration keyed to the
/// task (or a standing oracle) cannot be turned into runnable command
/// graders: unreadable file, malformed JSON, a non-command grader, an empty
/// grader list, a malformed `match` predicate. The engine maps it to a
/// fail-closed `Fail` verdict.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HeldOutGraderError(pub String);

impl HeldOutGraderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// The three terminal states of a held-out landing gate.
///
/// `NoGate` is deliberately distinct from `Fail`: a task that never
/// registered a held-out grader lands byte-identically to an ungated repo,
/// whereas a task whose registered grader is unrunnable is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateOutcome {
    NoGate,
    Pass,
    Fail,
}

impl GateOutcome {
    /// The stable wire value, as carried on [`HeldOutGateEvaluated::outcome`].
    pub const fn as_str(self) -> &'static str {
        match self {
            GateOutcome::NoGate => "no_gate",
            GateOutcome::Pass => "pass",
            GateOutcome::Fail => "fail",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "no_gate" => Some(GateOutcome::NoGate),
            "pass" => Some(GateOutcome::Pass),
            "fail" => Some(GateOutcome::Fail),
            _ => None,
        }
    }
}

impl fmt::Display for GateOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The engine's landing verdict for one task.
///
/// `outcome` is authoritative; `reason` is the operator-readable summary
/// recorded on the ledger. `results` carries the held-out grader receipts
/// in execution order: empty for `NoGate` and for a load-time fail-closed
/// where no grader ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateVerdict {
    pub outcome: GateOutcome,
    pub reason: String,
    #[serde(default)]
    pub results: Vec<GraderResultRecord>,
}

impl GateVerdict {
    fn new(outcome: GateOutcome, reason: impl Into<String>) -> Self {
        Self {
            outcome,
            reason: reason.into(),
            results: Vec::new(),
        }
    }

    fn fail_closed(detail: impl fmt::Display) -> Self {
        Self::new(
            GateOutcome::Fail,
            format!("held-out gate failed closed: {detail}"),
        )
    }

    /// True only when every registered held-out grader ran and passed.
    pub fn passed(&self) -> bool {
        self.outcome == GateOutcome::Pass
    }

    /// True when the gate must block the land (any fail / fail-closed).
    /// `NoGate` does not block.
    pub fn blocks_landing(&self) -> bool {
        self.outcome == GateOutcome::Fail
    }

    /// One bounded receipt per executed held-out grader, in execution order.
    pub fn receipts(&self) -> Vec<GateGraderReceipt> {
        self.results
            .iter()
            .map(|record| GateGraderReceipt {
                grader_name: record.grader_name.clone(),
                passed: record.passed,
                output_excerpt: gate_grader_excerpt(&record.payload, GATE_EXCERPT_MAX_BYTES),
            })
            .collect()
    }

    /// The ledger witness for this evaluation (emitted for every outcome).
    pub fn to_event(&self) -> HeldOutGateEvaluated {
        HeldOutGateEvaluated {
            outcome: self.outcome.as_str().to_owned(),
            reason: self.reason.clone(),
            receipts: self.receipts(),
        }
    }
}

// --- Sources ---------------------------------------------------------------------------

/// Operator-owned source of held-out command graders.
///
/// `graders_for` returns the per-task-id registration (`None` when the task
/// never registered: the `NoGate` path) and errors when a registration
/// exists but cannot be turned into runnable command graders (the
/// fail-closed path). `standing_graders_for` returns the standing-invariant
/// oracles that apply to `task` by predicate (its tags), ANDed into the
/// verdict; a source with no notion of standing oracles keeps the default.
///
/// Implementations must never expose held-out graders through the
/// agent-facing `Task` or write them into the agent's worktree.
pub trait HeldOutGraderSource: Send + Sync {
    fn graders_for(&self, task_id: &str) -> Result<Option<Vec<CommandGrader>>, HeldOutGraderError>;

    fn standing_graders_for(&self, task: &Task) -> Result<Vec<CommandGrader>, HeldOutGraderError> {
        let _ = task;
        Ok(Vec::new())
    }
}

/// Held-out graders declared on disk under `root`.
///
/// Layout: `<root>/<task_id>.json` holds a task's held-out command graders,
/// either as a bare JSON list of grader entries or as an object with a
/// `graders` key (the same per-entry shape as a `Task`'s `graders`,
/// validated through [`load_graders`]). `<root>/_standing/*.json` holds
/// standing oracles, each `{"match": {...}, "graders": [...]}`. The
/// directory lives outside every agent worktree.
///
/// Fail-closed posture: only a definitely absent entry for the task id
/// yields `None`; an entry that is unreadable (including a dangling symlink
/// or a directory), malformed, non-command, or empty errors. A `root` that
/// is not an existing directory errors for every task ([`Self::check_root`]):
/// a configured gate whose root is missing (a typo, or a `git clean -X` of
/// the git-ignored payload) must never read as "nothing registered".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemHeldOutGraderSource {
    pub root: PathBuf,
}

impl FilesystemHeldOutGraderSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `<root>/<task_id>.json`, or `None` for an id that could escape
    /// `root` (treated as unregistered rather than read from elsewhere).
    pub fn registration_path(&self, task_id: &str) -> Option<PathBuf> {
        registration_file_name(task_id).map(|name| self.root.join(name))
    }

    /// Errors unless `root` is an existing directory (a symlink to one is
    /// fine). A configured source whose root is missing or not a directory
    /// cannot tell an unregistered task from a lost registration, so it
    /// fails closed instead of degrading to `NoGate`.
    ///
    /// # Errors
    ///
    /// Returns [`HeldOutGraderError`] when `root` does not exist, is not a
    /// directory, or cannot be inspected.
    pub fn check_root(&self) -> Result<(), HeldOutGraderError> {
        match std::fs::metadata(&self.root) {
            Ok(meta) if meta.is_dir() => Ok(()),
            Ok(_) => Err(HeldOutGraderError::new(format!(
                "{}: held-out root is not a directory",
                self.root.display()
            ))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(HeldOutGraderError::new(format!(
                    "{}: held-out root does not exist (create it, or remove [held_out] root to disable the gate)",
                    self.root.display()
                )))
            }
            Err(err) => Err(HeldOutGraderError::new(format!(
                "{}: held-out root cannot be inspected: {err}",
                self.root.display()
            ))),
        }
    }

    fn standing_dir(&self) -> PathBuf {
        self.root.join(STANDING_SUBDIR)
    }

    fn load_standing_file(
        &self,
        path: &Path,
        task: &Task,
    ) -> Result<Vec<CommandGrader>, HeldOutGraderError> {
        let data = read_json(path, "standing oracle")?;
        let Value::Object(object) = &data else {
            return Err(HeldOutGraderError::new(format!(
                "{}: standing oracle must be an object with 'match' and 'graders'",
                path.display()
            )));
        };
        if !standing_matches(object.get("match"), task, &path.display().to_string())? {
            return Ok(Vec::new());
        }
        command_graders_only(
            object.get("graders").unwrap_or(&Value::Null),
            path,
            "standing oracle",
        )
    }
}

impl HeldOutGraderSource for FilesystemHeldOutGraderSource {
    fn graders_for(&self, task_id: &str) -> Result<Option<Vec<CommandGrader>>, HeldOutGraderError> {
        self.check_root()?;
        let Some(path) = self.registration_path(task_id) else {
            return Ok(None);
        };
        if !entry_exists(&path, "held-out grader registration")? {
            return Ok(None);
        }
        let data = read_json(&path, "held-out grader registration")?;
        let entries = match &data {
            Value::Object(object) => object.get("graders").unwrap_or(&Value::Null),
            other => other,
        };
        command_graders_only(entries, &path, "held-out grader registration").map(Some)
    }

    fn standing_graders_for(&self, task: &Task) -> Result<Vec<CommandGrader>, HeldOutGraderError> {
        self.check_root()?;
        let dir = self.standing_dir();
        if !entry_exists(&dir, "standing oracle directory")? {
            return Ok(Vec::new());
        }
        let listing_error = |err: std::io::Error| {
            HeldOutGraderError::new(format!(
                "{}: standing oracle directory cannot be listed: {err}",
                dir.display()
            ))
        };
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(listing_error)? {
            let path = entry.map_err(listing_error)?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                files.push(path);
            }
        }
        files.sort();
        let mut out = Vec::new();
        for path in files {
            out.extend(self.load_standing_file(&path, task)?);
        }
        Ok(out)
    }
}

/// The bare filename for a task's registration, or `None` when `task_id`
/// is not a single path-safe component (empty, a separator, `.`/`..`).
///
/// Shared by the source and the writer so the two never disagree about
/// which ids are registrable.
pub fn registration_file_name(task_id: &str) -> Option<String> {
    if task_id.is_empty()
        || task_id.contains('/')
        || task_id.contains('\\')
        || task_id == "."
        || task_id == ".."
    {
        return None;
    }
    Some(format!("{task_id}.json"))
}

/// Whether a directory entry exists at `path`, without following a final
/// symlink. Only a definite `NotFound` means "nothing registered"; any other
/// stat failure errors, and an entry that exists but cannot be read as the
/// expected kind (a dangling symlink, a directory) fails closed at read time
/// rather than being mistaken for an absent registration.
fn entry_exists(path: &Path, what: &str) -> Result<bool, HeldOutGraderError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(HeldOutGraderError::new(format!(
            "{}: {what} cannot be inspected: {err}",
            path.display()
        ))),
    }
}

fn read_json(path: &Path, what: &str) -> Result<Value, HeldOutGraderError> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        HeldOutGraderError::new(format!("{}: {what} cannot be read: {err}", path.display()))
    })?;
    serde_json::from_str(&text).map_err(|err| {
        HeldOutGraderError::new(format!("{}: {what} is invalid JSON: {err}", path.display()))
    })
}

/// Parse `entries` through the core grader loader and insist on a
/// non-empty, command-only list (D-5).
fn command_graders_only(
    entries: &Value,
    path: &Path,
    what: &str,
) -> Result<Vec<CommandGrader>, HeldOutGraderError> {
    let graders = load_graders(entries, &path.display().to_string())
        .map_err(|err| HeldOutGraderError::new(format!("{what} could not be parsed: {err}")))?;
    let total = graders.len();
    let command: Vec<CommandGrader> = graders
        .into_iter()
        .filter_map(|grader| match grader {
            Grader::Command(g) => Some(g),
            _ => None,
        })
        .collect();
    if command.len() != total {
        return Err(HeldOutGraderError::new(format!(
            "{}: {what} graders must be command graders only (D-5); found a non-command grader",
            path.display()
        )));
    }
    if command.is_empty() {
        return Err(HeldOutGraderError::new(format!(
            "{}: {what} declares no command graders; refusing to certify an empty gate as passing",
            path.display()
        )));
    }
    Ok(command)
}

/// Whether a standing oracle's `match` predicate applies to `task`.
///
/// Absent / empty `match` applies to every task (a global oracle).
/// `match.tags` (a list of strings) applies when `task.tags` intersects it.
/// A non-object `match`, or a non-empty one without a valid `tags` list,
/// fails closed so a typo'd predicate never silently becomes global or
/// never-matching.
pub fn standing_matches(
    predicate: Option<&Value>,
    task: &Task,
    source: &str,
) -> Result<bool, HeldOutGraderError> {
    let Some(predicate) = predicate else {
        return Ok(true);
    };
    let Value::Object(object) = predicate else {
        return Err(HeldOutGraderError::new(format!(
            "{source}: standing oracle 'match' must be an object"
        )));
    };
    if object.is_empty() {
        return Ok(true);
    }
    let tags = object
        .get("tags")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<String>>>()
        })
        .ok_or_else(|| {
            HeldOutGraderError::new(format!(
                "{source}: standing oracle 'match' must declare a 'tags' list of strings \
                 (the only supported predicate)"
            ))
        })?;
    Ok(tags.iter().any(|tag| task.tags.contains(tag)))
}

// --- Registration writers --------------------------------------------------------------

/// A registration could not be built or written.
#[derive(Debug, thiserror::Error)]
pub enum OracleRegistrationError {
    /// Under `cwd = committed tree` a relative oracle path resolves into
    /// the agent's worktree, where the oracle does not exist, so the gate
    /// would fail closed on every run (D-2).
    #[error(
        "held-out oracle path must be absolute, not {0:?}: under cwd = committed tree a relative \
         path resolves into the agent's worktree where the oracle does not exist, failing the \
         gate closed on every run (D-2)"
    )]
    RelativeOraclePath(String),
    /// The task id or standing name would escape the held-out root.
    #[error(
        "{0:?} does not resolve to a file directly under the held-out root (path traversal refused)"
    )]
    PathTraversal(String),
    #[error("writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Quote `s` for `/bin/sh`, exactly as Python's `shlex.quote` does: safe
/// words pass through, anything else is single-quoted with embedded quotes
/// spliced as `'"'"'`.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_owned();
    }
    let safe = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn oracle_grader(
    oracle_path: &Path,
    name: String,
    interpreter: Option<&str>,
) -> Result<Value, OracleRegistrationError> {
    if !oracle_path.is_absolute() {
        return Err(OracleRegistrationError::RelativeOraclePath(
            oracle_path.display().to_string(),
        ));
    }
    let quoted = shell_quote(&oracle_path.to_string_lossy());
    let run = match interpreter {
        Some(interpreter) => format!("{interpreter} {quoted}"),
        None => quoted,
    };
    // Keys sorted, mirroring the Python's `sort_keys=True` on disk.
    Ok(json!({"name": name, "run": run, "type": "command"}))
}

/// Express an admitted held-out oracle as the registration body the gate
/// loads: one command grader (D-5) invoking the oracle by its **absolute**
/// operator path. `interpreter` (e.g. `python3`) prefixes the command for
/// oracles that are not self-executable. The body is round-tripped through
/// the gate's own loader before it is returned, so a registration this
/// builds always loads.
pub fn build_oracle_registration(
    oracle_path: impl AsRef<Path>,
    name: Option<&str>,
    interpreter: Option<&str>,
) -> Result<Value, OracleRegistrationError> {
    let oracle_path = oracle_path.as_ref();
    let name = match name {
        Some(name) => name.to_owned(),
        None => format!(
            "held-out-oracle:{}",
            oracle_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
    };
    let grader = oracle_grader(oracle_path, name, interpreter)?;
    let registration = json!({"graders": [grader]});
    // The shape is validated by construction; a loader rejection here would
    // be a programming error in this module, never a caller's input.
    let loaded = load_graders(&registration["graders"], "<oracle registration>")
        .expect("built oracle registration loads through the gate's loader");
    debug_assert!(loaded.iter().all(|g| matches!(g, Grader::Command(_))));
    Ok(registration)
}

fn write_registration(target: &Path, body: &Value) -> Result<(), OracleRegistrationError> {
    let io = |source| OracleRegistrationError::Io {
        path: target.to_path_buf(),
        source,
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let mut text = serde_json::to_string_pretty(body).expect("registration body is JSON");
    text.push('\n');
    std::fs::write(target, text).map_err(io)
}

/// Write an admitted oracle's registration to `<root>/<task_id>.json`,
/// creating `root` and overwriting any existing registration. Refuses a
/// `task_id` the source would not read back (see [`registration_file_name`]).
pub fn write_oracle_registration(
    root: impl AsRef<Path>,
    task_id: &str,
    oracle_path: impl AsRef<Path>,
    name: Option<&str>,
    interpreter: Option<&str>,
) -> Result<PathBuf, OracleRegistrationError> {
    let body = build_oracle_registration(oracle_path, name, interpreter)?;
    let file_name = registration_file_name(task_id)
        .ok_or_else(|| OracleRegistrationError::PathTraversal(task_id.to_owned()))?;
    let target = root.as_ref().join(file_name);
    write_registration(&target, &body)?;
    Ok(target)
}

/// Write a standing-invariant oracle to `<root>/_standing/<name>.json`.
///
/// Instead of keying the oracle to one task id it carries a `match`
/// predicate: `tags` scopes it to tasks carrying any of those tags; empty
/// `tags` writes a GLOBAL oracle gating every landing. The grader shape is
/// [`build_oracle_registration`]'s, so the file always round-trips through
/// [`FilesystemHeldOutGraderSource::standing_graders_for`].
pub fn write_standing_oracle_registration(
    root: impl AsRef<Path>,
    name: &str,
    oracle_path: impl AsRef<Path>,
    tags: &[String],
    interpreter: Option<&str>,
) -> Result<PathBuf, OracleRegistrationError> {
    let file_name = registration_file_name(name)
        .ok_or_else(|| OracleRegistrationError::PathTraversal(name.to_owned()))?;
    let grader = oracle_grader(
        oracle_path.as_ref(),
        format!("standing-oracle:{name}"),
        interpreter,
    )?;
    let mut body = Map::new();
    body.insert("graders".into(), json!([grader]));
    if !tags.is_empty() {
        body.insert("match".into(), json!({"tags": tags}));
    }
    let target = root.as_ref().join(STANDING_SUBDIR).join(file_name);
    write_registration(&target, &Value::Object(body))?;
    Ok(target)
}

// --- The engine ---------------------------------------------------------------------------

/// Runner inputs for a gate evaluation that do not come from the request.
///
/// `env`, when set, is the complete environment the held-out graders run
/// with (the orchestrator's resolved grader env). `per_grader_timeout`
/// bounds each oracle: pass the policy's command-grader deadline so a hung
/// oracle cannot hold the task's lease forever.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateOptions {
    pub env: Option<BTreeMap<String, String>>,
    pub per_grader_timeout: Option<Duration>,
}

/// In-memory [`GraderResultStore`] for a gate run.
///
/// Gate receipts are a landing-decision signal, not lifecycle history: the
/// run's authoritative receipts were persisted in-run under
/// `(run_id, attempt_number, ordinal)`, which has no slot for an
/// out-of-band re-check. The engine returns the receipts in its verdict;
/// persisting them is the integration layer's choice.
#[derive(Debug, Default)]
struct GateGraderRecorder {
    records: Mutex<Vec<GraderResultRecord>>,
}

impl GraderResultStore for GateGraderRecorder {
    fn append_grader_result(
        &self,
        result: &GraderResultRecord,
    ) -> Result<GraderResultRecord, StoreError> {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stored = result.clone();
        stored.id = Some(records.len() as i64 + 1);
        records.push(stored.clone());
        Ok(stored)
    }

    fn list_grader_results(
        &self,
        run_id: &str,
        attempt_number: u32,
    ) -> Result<Vec<GraderResultRecord>, StoreError> {
        Ok(self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|r| r.run_id == run_id && r.attempt_number == attempt_number)
            .cloned()
            .collect())
    }
}

/// Compute the held-out landing verdict for `task`'s committed result.
///
/// Loads `task`'s held-out graders (per-task registration plus matching
/// standing oracles, ANDed) from `source` and runs them with
/// `committed_tree` as `cwd`, out of the agent's turn, through the core
/// command-grader runner. The verdict is the out-of-band exit code, never
/// the agent's reported status (D-4): this function is not given it.
///
/// * `NoGate`: nothing registered for the task; land unchanged.
/// * `Pass`: every registered grader ran and exited zero.
/// * `Fail`: a grader exited non-zero, timed out, or could not be spawned
///   (a missing committed tree), OR the registration could not be loaded
///   (fail closed, D-3).
pub async fn evaluate_held_out_gate<S, F>(
    task: &Task,
    source: &S,
    committed_tree: &Path,
    run_id: &str,
    options: &GateOptions,
    now: F,
) -> GateVerdict
where
    S: HeldOutGraderSource + ?Sized,
    F: FnMut() -> DateTime<Utc>,
{
    let loaded = source
        .graders_for(&task.id)
        .and_then(|per_task| Ok((per_task, source.standing_graders_for(task)?)));
    let (per_task, standing) = match loaded {
        Ok(loaded) => loaded,
        Err(err) => return GateVerdict::fail_closed(err),
    };
    let combined: Vec<Grader> = per_task
        .unwrap_or_default()
        .into_iter()
        .chain(standing)
        .map(Grader::Command)
        .collect();
    if combined.is_empty() {
        return GateVerdict::new(
            GateOutcome::NoGate,
            "no held-out graders registered for this task",
        );
    }
    let expected = combined.len();
    let gate_task = Task::new(format!("held-out landing gate for {}", task.id), combined)
        .with_id(task.id.clone());

    let runner_options = CommandGraderOptions {
        cwd: Some(committed_tree.to_path_buf()),
        env: options.env.clone(),
        per_grader_timeout: options.per_grader_timeout,
        ..CommandGraderOptions::default()
    };
    let recorder = GateGraderRecorder::default();
    // Held-out gate checks sit outside the lifecycle's attempt numbering;
    // the engine returns receipts, it does not persist lifecycle history.
    let records =
        match run_command_graders(&gate_task, &recorder, run_id, 0, &runner_options, now).await {
            Ok(records) => records,
            Err(err) => {
                return GateVerdict::fail_closed(format!("grader runner errored ({err})"));
            }
        };

    let ran_all = records.len() == expected;
    let all_passed = records.iter().all(|record| record.passed);
    if ran_all && all_passed {
        return GateVerdict {
            outcome: GateOutcome::Pass,
            reason: format!(
                "all {expected} held-out grader(s) passed against the committed result"
            ),
            results: records,
        };
    }
    GateVerdict {
        outcome: GateOutcome::Fail,
        reason: fail_reason(&records, expected),
        results: records,
    }
}

/// Summarize why a held-out gate blocked the land.
///
/// The runner stops at the first failing grader, so the failing grader is
/// the last record and later registered graders never ran; both facts are
/// reported so an operator sees the gate caught something rather than a
/// clean DONE-land. A spawn failure carries its OS error and a timeout its
/// ceiling, since neither has an exit code worth reading.
pub fn fail_reason(records: &[GraderResultRecord], expected: usize) -> String {
    let ran = records.len();
    let mut parts = vec![format!(
        "held-out gate FAILED: {ran} of {expected} held-out grader(s) ran"
    )];
    for record in records.iter().filter(|r| !r.passed) {
        let label = record.grader_name.clone().unwrap_or_else(|| {
            record
                .grader_spec
                .get("run")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        });
        let termination = payload_termination(&record.payload);
        let exit_code = record
            .payload
            .get("exit_code")
            .and_then(Value::as_i64)
            .map_or_else(|| "none".to_owned(), |code| code.to_string());
        let mut part = format!("[{label}] {termination} exit_code={exit_code}");
        match termination {
            Termination::SpawnFailure => {
                if let Some(err) = record.payload.get("spawn_error").and_then(Value::as_str) {
                    part.push_str(&format!(" ({err})"));
                }
            }
            Termination::Timeout => {
                if let Some(secs) = record
                    .payload
                    .get("timeout_seconds")
                    .and_then(Value::as_f64)
                {
                    part.push_str(&format!(" (after {secs}s)"));
                }
            }
            Termination::Exited | Termination::Signal => {}
        }
        parts.push(part);
    }
    if ran < expected {
        parts.push(format!(
            "{} held-out grader(s) did not run after the first failure",
            expected - ran
        ));
    }
    parts.join("; ")
}

// --- Ledger witnesses -----------------------------------------------------------------

/// One held-out grader's bounded output tail for a verdict receipt.
///
/// Joins the command grader's captured stdout/stderr tails (and the spawn
/// error, when the subprocess never started) and re-caps the result to
/// `bound` bytes as a *tail*, so the final content survives when a grader
/// emitted more than the bound. Stored raw: redaction is a render-time
/// concern.
pub fn gate_grader_excerpt(payload: &JsonObject, bound: usize) -> String {
    let combined = ["stdout_tail", "stderr_tail", "spawn_error"]
        .iter()
        .filter_map(|key| payload.get(*key).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if combined.len() <= bound {
        return combined;
    }
    bounded_tail(combined.as_bytes(), bound).0
}

/// Append a [`HeldOutGateEvaluated`] witness for `verdict` to `run_id`'s
/// ledger. Emitted for every evaluation (pass, fail, or no-gate) so a
/// gate-decided park is diagnosable from the store alone. Audit-witness
/// only: the fold is the identity and the run stays `Done`.
///
/// Returns `Ok(false)` when the lifecycle does not exist (nothing to witness).
pub fn record_held_out_gate_verdict<S>(
    store: &S,
    run_id: &str,
    verdict: &GateVerdict,
    now: DateTime<Utc>,
) -> Result<bool, StoreError>
where
    S: LifecycleStore + DomainEventStore + ?Sized,
{
    let Some(lifecycle) = store.load_lifecycle(run_id)? else {
        return Ok(false);
    };
    let event = DomainEvent::new(
        run_id,
        now,
        DomainEventPayload::HeldOutGateEvaluated(verdict.to_event()),
    );
    store.append_domain_event(&event, lifecycle.version)?;
    Ok(true)
}

/// Append a `held-out-gate` [`LandingParked`] witness to the blocked DONE
/// run's ledger so an operator can see why the land was suppressed.
///
/// Returns `Ok(false)` when the lifecycle does not exist.
pub fn record_held_out_gate_park<S>(
    store: &S,
    run_id: &str,
    detail: &str,
    now: DateTime<Utc>,
) -> Result<bool, StoreError>
where
    S: LifecycleStore + DomainEventStore + ?Sized,
{
    let Some(lifecycle) = store.load_lifecycle(run_id)? else {
        return Ok(false);
    };
    let event = DomainEvent::new(
        run_id,
        now,
        DomainEventPayload::LandingParked(LandingParked {
            park_kind: PARK_KIND_HELD_OUT_GATE.to_owned(),
            detail: detail.to_owned(),
            ..LandingParked::default()
        }),
    );
    store.append_domain_event(&event, lifecycle.version)?;
    Ok(true)
}

// --- Landing integration -----------------------------------------------------------------

/// What the drive does with a finalized run after the gate: hand it to the
/// strategy's `submit`, or suppress the land and leave the sandbox parked.
///
/// `verdict` is `None` when the gate did not run (no source wired, or the
/// run did not reach `Done`), so an ungated land is distinguishable from a
/// `NoGate` land on the in-process record as well as on the ledger.
#[derive(Debug, Clone, PartialEq)]
pub enum LandingDecision {
    Land {
        verdict: Option<GateVerdict>,
    },
    Park {
        detail: String,
        verdict: GateVerdict,
    },
}

impl LandingDecision {
    pub fn lands(&self) -> bool {
        matches!(self, LandingDecision::Land { .. })
    }

    /// The gate outcome, when the gate ran.
    pub fn gate(&self) -> Option<GateOutcome> {
        match self {
            LandingDecision::Land { verdict } => verdict.as_ref().map(|v| v.outcome),
            LandingDecision::Park { verdict, .. } => Some(verdict.outcome),
        }
    }

    /// The park kind every suppressed land carries.
    pub const fn park_kind(&self) -> Option<&'static str> {
        match self {
            LandingDecision::Land { .. } => None,
            LandingDecision::Park { .. } => Some(PARK_KIND_HELD_OUT_GATE),
        }
    }
}

/// Compute the held-out landing verdict for a finalized run, or `None`.
///
/// `None` (the gate did not run) when no source is wired or the run did
/// not reach the landing status (`Done`): a non-landing terminal is already
/// parked by `submit`, so there is nothing to gate and behavior is
/// byte-identical to an ungated repo.
pub async fn evaluate_landing_gate<F>(
    source: Option<&dyn HeldOutGraderSource>,
    task: &Task,
    status: Status,
    sandbox: &Path,
    run_id: &str,
    options: &GateOptions,
    now: F,
) -> Option<GateVerdict>
where
    F: FnMut() -> DateTime<Utc>,
{
    let source = source?;
    if status != Status::Done {
        return None;
    }
    Some(evaluate_held_out_gate(task, source, sandbox, run_id, options, now).await)
}

/// The one gate call the per-task drive makes between "run finalized" and
/// `strategy.submit`, while the lease is still held.
///
/// Evaluates the gate for `request` (against `request.sandbox`, the
/// committed tree), records the [`HeldOutGateEvaluated`] witness for every
/// evaluation, and on a blocked land additionally records the
/// `held-out-gate` [`LandingParked`] witness. Recording is best-effort: a
/// store failure is reported through `log` and never changes the decision
/// or unwinds the drive. The same path serves a first landing and a
/// landing re-drive, so a park the gate blocked cannot slip in on re-drive
/// through the strategy's own checks alone.
pub async fn gate_landing<S, F>(
    store: &S,
    source: Option<&dyn HeldOutGraderSource>,
    request: &SubmitRequest,
    options: &GateOptions,
    mut now: F,
    log: &mut (dyn FnMut(&str) + Send),
) -> LandingDecision
where
    S: LifecycleStore + DomainEventStore + ?Sized,
    F: FnMut() -> DateTime<Utc>,
{
    let verdict = evaluate_landing_gate(
        source,
        &request.task,
        request.status,
        &request.sandbox,
        &request.run_id,
        options,
        &mut now,
    )
    .await;
    let Some(verdict) = verdict else {
        return LandingDecision::Land { verdict: None };
    };
    if verdict.outcome != GateOutcome::NoGate {
        let marker = if verdict.blocks_landing() {
            "BLOCKED"
        } else {
            "passed"
        };
        log(&format!(
            "[orchestrate] {}: held-out landing gate {marker} ({})",
            request.task_id, verdict.reason
        ));
    }
    if let Err(err) = record_held_out_gate_verdict(store, &request.run_id, &verdict, now()) {
        log(&format!(
            "[orchestrate] {}: failed to record held-out-gate verdict event ({err})",
            request.task_id
        ));
    }
    if !verdict.blocks_landing() {
        return LandingDecision::Land {
            verdict: Some(verdict),
        };
    }
    if let Err(err) = record_held_out_gate_park(store, &request.run_id, &verdict.reason, now()) {
        log(&format!(
            "[orchestrate] {}: failed to record held-out-gate landing-parked event ({err})",
            request.task_id
        ));
    }
    LandingDecision::Park {
        detail: verdict.reason.clone(),
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_core::events::{DomainEventKind, LifecycleInitialized};
    use flywheel_core::store_memory::InMemoryStore;
    use flywheel_core::task::GraderType;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn task(id: &str) -> Task {
        Task::new(
            "do the work",
            vec![Grader::Command(CommandGrader {
                run: "true".into(),
                name: Some("in-run".into()),
            })],
        )
        .with_id(id)
    }

    fn tagged(id: &str, tags: &[&str]) -> Task {
        let mut task = task(id);
        task.tags = tags.iter().map(|t| (*t).to_owned()).collect();
        task
    }

    fn register(root: &Path, task_id: &str, entries: Value) -> PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let path = root.join(format!("{task_id}.json"));
        std::fs::write(&path, serde_json::to_string(&entries).unwrap()).unwrap();
        path
    }

    fn write_standing(root: &Path, name: &str, run: &str, predicate: Option<Value>) {
        let dir = root.join(STANDING_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = json!({"graders": [{"type": "command", "run": run, "name": name}]});
        if let Some(predicate) = predicate {
            body["match"] = predicate;
        }
        std::fs::write(dir.join(format!("{name}.json")), body.to_string()).unwrap();
    }

    fn cmd(run: &str, name: &str) -> Value {
        json!({"type": "command", "run": run, "name": name})
    }

    fn tree(tmp: &TempDir, name: &str) -> PathBuf {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn evaluate(task: &Task, root: &Path, committed: &Path, run_id: &str) -> GateVerdict {
        let source = FilesystemHeldOutGraderSource::new(root);
        evaluate_held_out_gate(
            task,
            &source,
            committed,
            run_id,
            &GateOptions::default(),
            Utc::now,
        )
        .await
    }

    // --- registration presence vs absence ------------------------------------------

    #[tokio::test]
    async fn no_registration_yields_no_gate() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let verdict = evaluate(&task("task-alpha"), tmp.path(), &committed, "run-1").await;
        assert_eq!(verdict.outcome, GateOutcome::NoGate);
        assert!(!verdict.blocks_landing());
        assert!(!verdict.passed());
        assert!(verdict.results.is_empty());
    }

    #[test]
    fn source_returns_none_for_unregistered_task() {
        let tmp = TempDir::new().unwrap();
        let source = FilesystemHeldOutGraderSource::new(tmp.path());
        assert_eq!(source.graders_for("nobody-home"), Ok(None));
    }

    // --- a configured root that is missing never degrades to NoGate --------------

    #[test]
    fn missing_root_fails_closed_for_registrations_and_standing_oracles() {
        let tmp = TempDir::new().unwrap();
        let source = FilesystemHeldOutGraderSource::new(tmp.path().join("held-oot"));
        let err = source.graders_for("task-alpha").unwrap_err();
        assert!(err.0.contains("held-out root does not exist"), "{err}");
        let err = source
            .standing_graders_for(&task("task-alpha"))
            .unwrap_err();
        assert!(err.0.contains("held-out root does not exist"), "{err}");
        assert!(source.check_root().is_err());
    }

    #[test]
    fn root_that_is_a_file_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        std::fs::write(&root, "not a directory").unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let err = source.graders_for("task-alpha").unwrap_err();
        assert!(err.0.contains("held-out root is not a directory"), "{err}");
    }

    #[test]
    fn root_symlinked_to_a_directory_is_accepted() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let source = FilesystemHeldOutGraderSource::new(&link);
        assert_eq!(source.check_root(), Ok(()));
        assert_eq!(source.graders_for("task-alpha"), Ok(None));
    }

    #[tokio::test]
    async fn missing_root_blocks_landing_instead_of_no_gate() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held-oot");
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-1").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.blocks_landing());
        assert!(
            verdict.reason.contains("held-out root does not exist"),
            "{}",
            verdict.reason
        );
    }

    // --- pass path ---------------------------------------------------------------

    #[tokio::test]
    async fn all_passing_held_out_graders_yield_pass() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(
            &root,
            "task-alpha",
            json!([cmd("true", "first"), cmd("exit 0", "second")]),
        );
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-2").await;
        assert_eq!(verdict.outcome, GateOutcome::Pass);
        assert!(verdict.passed());
        assert!(!verdict.blocks_landing());
        assert_eq!(verdict.results.len(), 2);
        assert!(verdict.results.iter().all(|r| r.passed));
        assert!(verdict.reason.contains("all 2 held-out grader(s) passed"));
    }

    // --- fail path ---------------------------------------------------------------

    #[tokio::test]
    async fn failing_held_out_grader_blocks_land() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(&root, "task-alpha", json!([cmd("exit 7", "gate-check")]));
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-3").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.blocks_landing());
        assert!(!verdict.passed());
        assert!(verdict.reason.contains("[gate-check] exited exit_code=7"));
    }

    #[tokio::test]
    async fn first_failure_blocks_even_with_later_passing_grader() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(
            &root,
            "task-alpha",
            json!([cmd("exit 1", "fails"), cmd("true", "would-pass")]),
        );
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-4").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert_eq!(verdict.results.len(), 1);
        assert!(verdict.reason.contains("1 of 2 held-out grader(s) ran"));
        assert!(verdict.reason.contains("1 held-out grader(s) did not run"));
    }

    // --- D-4: agent self-report is never authoritative ----------------------------

    #[tokio::test]
    async fn verdict_ignores_agent_status() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(&root, "task-alpha", json!([cmd("exit 2", "held-out")]));
        let done_looking = Task::new(
            "agent thinks it is done",
            vec![Grader::Command(CommandGrader {
                run: "true".into(),
                name: Some("visible".into()),
            })],
        )
        .with_id("task-alpha");
        let verdict = evaluate(&done_looking, &root, &committed, "run-5").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
    }

    // --- D-3: fail closed ---------------------------------------------------------

    #[tokio::test]
    async fn grader_that_cannot_execute_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(
            &root,
            "task-alpha",
            json!([cmd("./definitely-not-a-real-binary-xyz", "missing")]),
        );
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-6").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.blocks_landing());
    }

    #[tokio::test]
    async fn malformed_registration_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("task-alpha.json"), "{not json").unwrap();
        let committed = tree(&tmp, "tree");
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-7").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.reason.contains("failed closed"));
        assert!(verdict.reason.contains("invalid JSON"));
        assert!(verdict.results.is_empty());
    }

    #[tokio::test]
    async fn empty_registration_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(&root, "task-alpha", json!([]));
        let committed = tree(&tmp, "tree");
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-8").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.reason.contains("declares no command graders"));
    }

    #[tokio::test]
    async fn non_command_held_out_grader_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(
            &root,
            "task-alpha",
            json!([{"type": "rubric", "assertions": ["looks good"], "name": "judge"}]),
        );
        let committed = tree(&tmp, "tree");
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-9").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.reason.contains("command graders only"));
    }

    #[tokio::test]
    async fn missing_committed_tree_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(&root, "task-alpha", json!([cmd("true", "check")]));
        let verdict = evaluate(
            &task("task-alpha"),
            &root,
            &tmp.path().join("does-not-exist"),
            "run-10",
        )
        .await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.blocks_landing());
        assert!(
            verdict
                .reason
                .contains("[check] spawn_failure exit_code=none (")
        );
    }

    #[tokio::test]
    async fn hung_grader_times_out_and_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(&root, "task-alpha", json!([cmd("sleep 30", "hangs")]));
        let source = FilesystemHeldOutGraderSource::new(&root);
        let options = GateOptions {
            env: None,
            per_grader_timeout: Some(Duration::from_millis(200)),
        };
        let verdict = evaluate_held_out_gate(
            &task("task-alpha"),
            &source,
            &committed,
            "run-timeout",
            &options,
            Utc::now,
        )
        .await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.reason.contains("[hangs] timeout"));
        assert!(verdict.reason.contains("after 0.2s"));
    }

    // --- graded against the committed result ---------------------------------------

    #[tokio::test]
    async fn grader_observes_committed_tree() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        std::fs::write(committed.join("result.txt"), "agent-committed-value\n").unwrap();
        let root = tmp.path().join("held_out");
        register(
            &root,
            "task-alpha",
            json!([cmd(
                "grep -q agent-committed-value result.txt",
                "inspects-commit"
            )]),
        );
        let passing = evaluate(&task("task-alpha"), &root, &committed, "run-11").await;
        assert_eq!(passing.outcome, GateOutcome::Pass);
        let other = tree(&tmp, "empty_tree");
        let failing = evaluate(&task("task-alpha"), &root, &other, "run-12").await;
        assert_eq!(failing.outcome, GateOutcome::Fail);
    }

    #[tokio::test]
    async fn grader_env_is_the_supplied_environment() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(
            &root,
            "task-alpha",
            json!([cmd("test \"$GATE_TOKEN\" = secret", "env-check")]),
        );
        let source = FilesystemHeldOutGraderSource::new(&root);
        let options = GateOptions {
            env: Some(BTreeMap::from([
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("GATE_TOKEN".to_string(), "secret".to_string()),
            ])),
            per_grader_timeout: None,
        };
        let verdict = evaluate_held_out_gate(
            &task("task-alpha"),
            &source,
            &committed,
            "run-env",
            &options,
            Utc::now,
        )
        .await;
        assert_eq!(verdict.outcome, GateOutcome::Pass, "{}", verdict.reason);
    }

    // --- D-2: held-out graders are absent from the agent's Task --------------------

    #[tokio::test]
    async fn held_out_graders_absent_from_agent_task() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(&root, "task-alpha", json!([cmd("true", "secret-check")]));
        let agent_task = task("task-alpha");
        let before = agent_task.clone();
        let verdict = evaluate(&agent_task, &root, &committed, "run-13").await;
        assert_eq!(verdict.outcome, GateOutcome::Pass);
        assert!(
            verdict
                .results
                .iter()
                .any(|r| r.grader_name.as_deref() == Some("secret-check"))
        );
        assert_eq!(agent_task, before);
        assert!(
            agent_task
                .graders
                .iter()
                .all(|g| g.name() != Some("secret-check"))
        );
    }

    #[test]
    fn source_path_traversal_is_not_registered() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        std::fs::create_dir_all(&root).unwrap();
        // A registration placed at the traversal target must not be read.
        std::fs::write(tmp.path().join("escape.json"), "[]").unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        assert_eq!(source.graders_for("../escape"), Ok(None));
        assert_eq!(source.graders_for("nested/id"), Ok(None));
        assert_eq!(source.graders_for("back\\slash"), Ok(None));
        assert_eq!(source.graders_for(""), Ok(None));
        assert_eq!(source.graders_for("."), Ok(None));
        assert_eq!(source.graders_for(".."), Ok(None));
        assert_eq!(registration_file_name("task-1"), Some("task-1.json".into()));
    }

    // --- source contract: object form and erroring ---------------------------------

    #[test]
    fn source_accepts_object_form() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(&root, "task-alpha", json!({"graders": [cmd("true", "ok")]}));
        let source = FilesystemHeldOutGraderSource::new(&root);
        let graders = source.graders_for("task-alpha").unwrap().unwrap();
        assert_eq!(
            graders
                .iter()
                .map(|g| g.name.as_deref())
                .collect::<Vec<_>>(),
            [Some("ok")]
        );
    }

    #[test]
    fn source_errors_on_unparseable_registration() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("task-alpha.json"), "not json at all").unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let err = source.graders_for("task-alpha").unwrap_err();
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[test]
    fn source_errors_on_unreadable_registration() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        let path = register(&root, "task-alpha", json!([cmd("true", "ok")]));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let result = source.graders_for("task-alpha");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // Root can read anything; only assert the fail-closed shape when the
        // read actually failed.
        if let Err(err) = result {
            assert!(err.to_string().contains("cannot be read"));
        }
    }

    // A registration entry that exists but whose body cannot be located is
    // not "unregistered": treating it as NoGate would land ungated work.
    #[tokio::test]
    async fn dangling_symlink_registration_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("moved-oracle-store/task-alpha.json"),
            root.join("task-alpha.json"),
        )
        .unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let err = source.graders_for("task-alpha").unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err}");

        let committed = tree(&tmp, "tree");
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-1").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.blocks_landing());
    }

    #[test]
    fn directory_registration_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        std::fs::create_dir_all(root.join("task-alpha.json")).unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let err = source.graders_for("task-alpha").unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err}");
    }

    #[tokio::test]
    async fn dangling_symlink_standing_oracle_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        let dir = root.join(STANDING_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("moved-oracle-store/invariant.json"),
            dir.join("invariant.json"),
        )
        .unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let err = source
            .standing_graders_for(&task("task-alpha"))
            .unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err}");

        let committed = tree(&tmp, "tree");
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-1").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
    }

    #[test]
    fn dangling_symlink_standing_dir_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("moved-standing"),
            root.join(STANDING_SUBDIR),
        )
        .unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let err = source
            .standing_graders_for(&task("task-alpha"))
            .unwrap_err();
        assert!(err.to_string().contains("cannot be listed"), "{err}");
    }

    #[test]
    fn source_is_object_safe() {
        let tmp = TempDir::new().unwrap();
        let source: Box<dyn HeldOutGraderSource> =
            Box::new(FilesystemHeldOutGraderSource::new(tmp.path()));
        assert_eq!(source.graders_for("x"), Ok(None));
        assert_eq!(source.standing_graders_for(&task("x")), Ok(Vec::new()));
    }

    // --- registration shape (spec 00051) ---------------------------------------------

    /// An oracle authored blind: run with the committed tree as cwd, it
    /// inspects the committed file and asserts a discriminating relation.
    fn write_oracle(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let oracle = dir.join("normalize_oracle.sh");
        std::fs::write(
            &oracle,
            "#!/bin/sh\ntest \"$(cat solution.txt)\" = \"1 2 3\"\n",
        )
        .unwrap();
        oracle
    }

    fn committed_tree(dir: &Path, solution: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("solution.txt"), solution).unwrap();
        dir.to_path_buf()
    }

    #[test]
    fn build_registration_yields_loadable_command_grader() {
        let tmp = TempDir::new().unwrap();
        let oracle = write_oracle(&tmp.path().join("oracle dir"));
        let root = tmp.path().join("held_out");
        let written =
            write_oracle_registration(&root, "task-alpha", &oracle, None, Some("sh")).unwrap();
        assert_eq!(written, root.join("task-alpha.json"));
        let text = std::fs::read_to_string(&written).unwrap();
        assert!(text.ends_with('\n'));

        let source = FilesystemHeldOutGraderSource::new(&root);
        let graders = source.graders_for("task-alpha").unwrap().unwrap();
        assert_eq!(graders.len(), 1);
        let grader = &graders[0];
        // Quoted, because the oracle directory carries a space.
        assert_eq!(grader.run, format!("sh '{}'", oracle.display()));
        assert_eq!(
            grader.name.as_deref(),
            Some("held-out-oracle:normalize_oracle.sh")
        );
        assert!(oracle.is_absolute());
    }

    #[test]
    fn build_registration_rejects_relative_oracle_path() {
        let err =
            build_oracle_registration("oracle_dir/normalize_oracle.py", None, None).unwrap_err();
        assert!(matches!(
            err,
            OracleRegistrationError::RelativeOraclePath(_)
        ));
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn build_registration_honors_custom_name_without_interpreter() {
        let body = build_oracle_registration("/abs/oracle.py", Some("custom"), None).unwrap();
        assert_eq!(body["graders"][0]["name"], "custom");
        assert_eq!(body["graders"][0]["run"], "/abs/oracle.py");
        assert_eq!(body["graders"][0]["type"], "command");
    }

    #[test]
    fn non_command_registration_at_task_path_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(
            &root,
            "task-alpha",
            json!([{"type": "rubric", "assertions": ["looks ok"], "name": "judge"}]),
        );
        let source = FilesystemHeldOutGraderSource::new(&root);
        assert!(source.graders_for("task-alpha").is_err());
    }

    #[test]
    fn write_registration_refuses_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let oracle = write_oracle(&tmp.path().join("oracle_dir"));
        let root = tmp.path().join("held_out");
        for bad in ["../escape", "nested/id", ".", ".."] {
            let err = write_oracle_registration(&root, bad, &oracle, None, Some("sh")).unwrap_err();
            assert!(err.to_string().contains("traversal"), "{bad}: {err}");
        }
        assert!(!tmp.path().join("escape.json").exists());
    }

    #[tokio::test]
    async fn registration_reproduces_discrimination_through_gate() {
        let tmp = TempDir::new().unwrap();
        let oracle = write_oracle(&tmp.path().join("oracle_dir"));
        let root = tmp.path().join("held_out");
        write_oracle_registration(&root, "task-alpha", &oracle, None, Some("sh")).unwrap();
        let correct = committed_tree(&tmp.path().join("correct"), "1 2 3\n");
        let wrong = committed_tree(&tmp.path().join("wrong"), "3 1 2\n");
        assert_ne!(oracle.parent().unwrap(), correct);
        assert_ne!(oracle.parent().unwrap(), wrong);

        let passing = evaluate(&task("task-alpha"), &root, &correct, "run-correct").await;
        assert_eq!(passing.outcome, GateOutcome::Pass, "{}", passing.reason);
        let failing = evaluate(&task("task-alpha"), &root, &wrong, "run-wrong").await;
        assert_eq!(failing.outcome, GateOutcome::Fail);
        assert!(failing.blocks_landing());
    }

    #[tokio::test]
    async fn verdict_tracks_committed_tree_not_oracle_directory() {
        let tmp = TempDir::new().unwrap();
        let oracle = write_oracle(&tmp.path().join("oracle_dir"));
        // A wrong decoy beside the oracle would be read instead of the
        // committed file if the gate ran with the oracle's directory as cwd.
        std::fs::write(oracle.parent().unwrap().join("solution.txt"), "3 1 2\n").unwrap();
        let root = tmp.path().join("held_out");
        write_oracle_registration(&root, "task-alpha", &oracle, None, Some("sh")).unwrap();
        let correct = committed_tree(&tmp.path().join("correct"), "1 2 3\n");
        let verdict = evaluate(&task("task-alpha"), &root, &correct, "run-cwd").await;
        assert_eq!(verdict.outcome, GateOutcome::Pass, "{}", verdict.reason);
    }

    // --- standing-invariant oracles -------------------------------------------------

    #[tokio::test]
    async fn standing_oracle_applies_to_a_matching_tagged_task() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        write_standing(&root, "netting", "true", Some(json!({"tags": ["netting"]})));
        let verdict = evaluate(&tagged("gh-42", &["netting"]), &root, tmp.path(), "r1").await;
        assert_eq!(verdict.outcome, GateOutcome::Pass);
    }

    #[tokio::test]
    async fn standing_oracle_blocks_a_matching_task_when_it_fails() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        write_standing(
            &root,
            "netting",
            "false",
            Some(json!({"tags": ["netting"]})),
        );
        let verdict = evaluate(&tagged("gh-42", &["netting"]), &root, tmp.path(), "r1").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.blocks_landing());
    }

    #[tokio::test]
    async fn standing_oracle_does_not_apply_to_an_unmatched_task() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        write_standing(
            &root,
            "netting",
            "false",
            Some(json!({"tags": ["netting"]})),
        );
        let verdict = evaluate(&tagged("gh-99", &["docs"]), &root, tmp.path(), "r1").await;
        assert_eq!(verdict.outcome, GateOutcome::NoGate);
    }

    #[tokio::test]
    async fn global_standing_oracle_applies_to_every_task() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        write_standing(&root, "suite", "true", None);
        let verdict = evaluate(&tagged("any", &[]), &root, tmp.path(), "r1").await;
        assert_eq!(verdict.outcome, GateOutcome::Pass);
        write_standing(&root, "suite", "false", Some(json!({})));
        let verdict = evaluate(&tagged("any", &[]), &root, tmp.path(), "r1").await;
        assert!(verdict.blocks_landing());
    }

    #[tokio::test]
    async fn standing_and_per_task_oracle_both_must_pass() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        register(
            &root,
            "gh-42",
            json!({"graders": [{"type": "command", "run": "true"}]}),
        );
        write_standing(
            &root,
            "netting",
            "false",
            Some(json!({"tags": ["netting"]})),
        );
        let verdict = evaluate(&tagged("gh-42", &["netting"]), &root, tmp.path(), "r1").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        // The per-task oracle ran (and passed) before the standing one failed.
        assert_eq!(verdict.results.len(), 2);
        assert!(verdict.results[0].passed);
        assert!(!verdict.results[1].passed);
    }

    #[tokio::test]
    async fn malformed_standing_match_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        write_standing(&root, "bad", "true", Some(json!({"unknown_predicate": 1})));
        let verdict = evaluate(&tagged("gh-1", &["netting"]), &root, tmp.path(), "r1").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.reason.contains("failed closed"));
        assert!(verdict.reason.contains("'tags' list"));
        write_standing(&root, "bad", "true", Some(json!("global")));
        let verdict = evaluate(&tagged("gh-1", &[]), &root, tmp.path(), "r1").await;
        assert!(verdict.reason.contains("'match' must be an object"));
    }

    #[tokio::test]
    async fn standing_non_command_grader_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        let standing = root.join(STANDING_SUBDIR);
        std::fs::create_dir_all(&standing).unwrap();
        std::fs::write(
            standing.join("bad.json"),
            json!({
                "match": {"tags": ["netting"]},
                "graders": [{"type": "manual", "instruction": "look at it"}],
            })
            .to_string(),
        )
        .unwrap();
        let verdict = evaluate(&tagged("gh-1", &["netting"]), &root, tmp.path(), "r1").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
    }

    #[tokio::test]
    async fn standing_non_object_file_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        let standing = root.join(STANDING_SUBDIR);
        std::fs::create_dir_all(&standing).unwrap();
        std::fs::write(standing.join("list.json"), "[]").unwrap();
        let verdict = evaluate(&tagged("gh-1", &[]), &root, tmp.path(), "r1").await;
        assert_eq!(verdict.outcome, GateOutcome::Fail);
        assert!(verdict.reason.contains("must be an object"));
    }

    #[test]
    fn write_standing_oracle_registration_round_trips() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        let oracle = tmp.path().join("netting_oracle.sh");
        std::fs::write(&oracle, "exit 0\n").unwrap();
        let path = write_standing_oracle_registration(
            &root,
            "netting",
            &oracle,
            &["netting".to_string()],
            Some("sh"),
        )
        .unwrap();
        assert_eq!(path, root.join(STANDING_SUBDIR).join("netting.json"));
        let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(body["match"]["tags"], json!(["netting"]));
        assert_eq!(body["graders"][0]["name"], "standing-oracle:netting");

        let source = FilesystemHeldOutGraderSource::new(&root);
        let graders = source
            .standing_graders_for(&tagged("gh-7", &["netting"]))
            .unwrap();
        assert_eq!(graders.len(), 1);
        assert!(graders[0].run.contains(&oracle.display().to_string()));
        assert!(
            source
                .standing_graders_for(&tagged("gh-8", &["docs"]))
                .unwrap()
                .is_empty()
        );

        // Empty tags writes a global oracle (no match key).
        let global =
            write_standing_oracle_registration(&root, "suite", &oracle, &[], None).unwrap();
        let body: Value = serde_json::from_str(&std::fs::read_to_string(global).unwrap()).unwrap();
        assert!(body.get("match").is_none());
        assert_eq!(
            source
                .standing_graders_for(&tagged("gh-8", &["docs"]))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn write_standing_oracle_refuses_escaping_name() {
        let tmp = TempDir::new().unwrap();
        let err = write_standing_oracle_registration(
            tmp.path(),
            "../escape",
            "/abs/oracle.sh",
            &[],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, OracleRegistrationError::PathTraversal(_)));
    }

    #[test]
    fn no_standing_dir_yields_no_standing_graders() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        std::fs::create_dir_all(&root).unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        assert_eq!(
            source.standing_graders_for(&tagged("gh-1", &["netting"])),
            Ok(Vec::new())
        );
    }

    #[test]
    fn standing_files_load_in_sorted_order_ignoring_non_json() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held-out");
        write_standing(&root, "b-second", "true", None);
        write_standing(&root, "a-first", "true", None);
        std::fs::write(root.join(STANDING_SUBDIR).join("notes.txt"), "ignored").unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let names: Vec<_> = source
            .standing_graders_for(&tagged("t", &[]))
            .unwrap()
            .into_iter()
            .map(|g| g.name.unwrap())
            .collect();
        assert_eq!(names, ["a-first", "b-second"]);
    }

    // --- helpers -----------------------------------------------------------------------

    #[test]
    fn shell_quote_matches_shlex() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("/abs/path-1.py"), "/abs/path-1.py");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote("caf\u{e9}"), "'caf\u{e9}'");
    }

    #[test]
    fn gate_outcome_wire_values_round_trip() {
        for outcome in [GateOutcome::NoGate, GateOutcome::Pass, GateOutcome::Fail] {
            assert_eq!(GateOutcome::parse(outcome.as_str()), Some(outcome));
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                Value::String(outcome.as_str().to_owned())
            );
            assert_eq!(outcome.to_string(), outcome.as_str());
        }
        assert_eq!(GateOutcome::parse("maybe"), None);
    }

    #[test]
    fn gate_grader_excerpt_joins_streams_and_keeps_the_tail() {
        let mut payload = JsonObject::new();
        payload.insert("stdout_tail".into(), json!("out"));
        payload.insert("stderr_tail".into(), json!(""));
        payload.insert("spawn_error".into(), json!("boom"));
        assert_eq!(gate_grader_excerpt(&payload, 8192), "out\nboom");

        let mut long = JsonObject::new();
        long.insert("stdout_tail".into(), json!("x".repeat(20)));
        long.insert("stderr_tail".into(), json!("TAIL"));
        let excerpt = gate_grader_excerpt(&long, 6);
        assert_eq!(excerpt, "x\nTAIL");
        assert!(excerpt.len() <= 6);
        assert_eq!(gate_grader_excerpt(&JsonObject::new(), 8192), "");
    }

    #[tokio::test]
    async fn verdict_event_carries_bounded_receipts() {
        let tmp = TempDir::new().unwrap();
        let committed = tree(&tmp, "tree");
        let root = tmp.path().join("held_out");
        register(
            &root,
            "task-alpha",
            json!([
                cmd("echo ok-out", "first"),
                cmd("echo bad >&2; exit 3", "second")
            ]),
        );
        let verdict = evaluate(&task("task-alpha"), &root, &committed, "run-ev").await;
        let event = verdict.to_event();
        assert_eq!(event.outcome, "fail");
        assert_eq!(event.reason, verdict.reason);
        assert_eq!(
            event.receipts,
            vec![
                GateGraderReceipt {
                    grader_name: Some("first".into()),
                    passed: true,
                    output_excerpt: "ok-out\n".into(),
                },
                GateGraderReceipt {
                    grader_name: Some("second".into()),
                    passed: false,
                    output_excerpt: "bad\n".into(),
                },
            ]
        );
        let no_gate = GateVerdict::new(GateOutcome::NoGate, "nothing");
        assert_eq!(
            no_gate.to_event(),
            HeldOutGateEvaluated {
                outcome: "no_gate".into(),
                reason: "nothing".into(),
                receipts: Vec::new(),
            }
        );
    }

    #[test]
    fn fail_reason_labels_unnamed_graders_by_run() {
        let mut spec = JsonObject::new();
        spec.insert("run".into(), json!("make check"));
        let mut payload = JsonObject::new();
        payload.insert("exit_code".into(), json!(2));
        payload.insert("termination".into(), json!("exited"));
        let record = GraderResultRecord {
            run_id: "r".into(),
            attempt_number: 0,
            ordinal: 0,
            grader_type: GraderType::Command,
            grader_spec: spec,
            passed: false,
            duration_ms: 1,
            payload,
            ts: Utc::now(),
            grader_name: None,
            id: None,
        };
        assert_eq!(
            fail_reason(&[record], 3),
            "held-out gate FAILED: 1 of 3 held-out grader(s) ran; [make check] exited \
             exit_code=2; 2 held-out grader(s) did not run after the first failure"
        );
    }

    // --- ledger witnesses and the landing decision ---------------------------------

    fn seeded_store(run_id: &str, task_id: &str) -> InMemoryStore {
        let store = InMemoryStore::new();
        let seed = DomainEvent::new(
            run_id,
            Utc::now(),
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: task_id.into(),
                ..LifecycleInitialized::default()
            }),
        );
        store.append_domain_event(&seed, 0).unwrap();
        store
    }

    fn kinds(store: &InMemoryStore, run_id: &str) -> Vec<DomainEventKind> {
        store
            .list_domain_events(run_id)
            .unwrap()
            .iter()
            .map(DomainEvent::kind)
            .collect()
    }

    #[test]
    fn record_verdict_and_park_append_witnesses_and_advance_version() {
        let store = seeded_store("run-1", "alpha");
        let before = store.load_lifecycle("run-1").unwrap().unwrap();
        let verdict = GateVerdict::new(GateOutcome::Fail, "held-out gate FAILED");
        assert!(record_held_out_gate_verdict(&store, "run-1", &verdict, Utc::now()).unwrap());
        assert!(record_held_out_gate_park(&store, "run-1", &verdict.reason, Utc::now()).unwrap());
        let after = store.load_lifecycle("run-1").unwrap().unwrap();
        assert_eq!(after.version, before.version + 2);
        assert_eq!(after.status, before.status);

        let events = store.list_domain_events("run-1").unwrap();
        assert_eq!(
            kinds(&store, "run-1"),
            [
                DomainEventKind::LifecycleInitialized,
                DomainEventKind::HeldOutGateEvaluated,
                DomainEventKind::LandingParked,
            ]
        );
        match &events[1].payload {
            DomainEventPayload::HeldOutGateEvaluated(ev) => {
                assert_eq!(ev.outcome, "fail");
                assert_eq!(ev.reason, "held-out gate FAILED");
            }
            other => panic!("unexpected payload {other:?}"),
        }
        match &events[2].payload {
            DomainEventPayload::LandingParked(ev) => {
                assert_eq!(ev.park_kind, PARK_KIND_HELD_OUT_GATE);
                assert_eq!(ev.detail, "held-out gate FAILED");
                assert!(ev.receipts.is_empty());
            }
            other => panic!("unexpected payload {other:?}"),
        }
    }

    #[test]
    fn record_witnesses_skip_a_missing_lifecycle() {
        let store = InMemoryStore::new();
        let verdict = GateVerdict::new(GateOutcome::Pass, "ok");
        assert!(!record_held_out_gate_verdict(&store, "ghost", &verdict, Utc::now()).unwrap());
        assert!(!record_held_out_gate_park(&store, "ghost", "x", Utc::now()).unwrap());
        assert!(store.list_domain_events("ghost").unwrap().is_empty());
    }

    fn submit_request(tmp: &TempDir, task_id: &str, status: Status) -> SubmitRequest {
        SubmitRequest::new(
            task(task_id),
            format!("run-{task_id}"),
            status,
            tree(tmp, task_id),
        )
    }

    #[tokio::test]
    async fn gate_landing_passing_gate_lands_with_verdict_witness() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(&root, "alpha", json!([cmd("true", "ok")]));
        let source = FilesystemHeldOutGraderSource::new(&root);
        let request = submit_request(&tmp, "alpha", Status::Done);
        let store = seeded_store(&request.run_id, "alpha");
        let mut lines = Vec::new();
        let decision = gate_landing(
            &store,
            Some(&source),
            &request,
            &GateOptions::default(),
            Utc::now,
            &mut |line| lines.push(line.to_owned()),
        )
        .await;
        assert!(decision.lands());
        assert_eq!(decision.gate(), Some(GateOutcome::Pass));
        assert_eq!(decision.park_kind(), None);
        assert_eq!(
            kinds(&store, &request.run_id),
            [
                DomainEventKind::LifecycleInitialized,
                DomainEventKind::HeldOutGateEvaluated,
            ]
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("alpha: held-out landing gate passed"));
    }

    #[tokio::test]
    async fn gate_landing_no_registration_lands_as_no_gate() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        std::fs::create_dir_all(&root).unwrap();
        let source = FilesystemHeldOutGraderSource::new(&root);
        let request = submit_request(&tmp, "alpha", Status::Done);
        let store = seeded_store(&request.run_id, "alpha");
        let mut lines = Vec::new();
        let decision = gate_landing(
            &store,
            Some(&source),
            &request,
            &GateOptions::default(),
            Utc::now,
            &mut |line| lines.push(line.to_owned()),
        )
        .await;
        assert!(decision.lands());
        assert_eq!(decision.gate(), Some(GateOutcome::NoGate));
        // NO_GATE is still a positive fact on the ledger, but is not logged.
        assert_eq!(
            kinds(&store, &request.run_id),
            [
                DomainEventKind::LifecycleInitialized,
                DomainEventKind::HeldOutGateEvaluated,
            ]
        );
        assert!(lines.is_empty());
    }

    #[tokio::test]
    async fn gate_landing_without_source_or_non_done_status_does_not_run() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(&root, "alpha", json!([cmd("false", "would-block")]));
        let source = FilesystemHeldOutGraderSource::new(&root);
        let mut lines = Vec::new();

        let request = submit_request(&tmp, "alpha", Status::Done);
        let store = seeded_store(&request.run_id, "alpha");
        let decision = gate_landing(
            &store,
            None,
            &request,
            &GateOptions::default(),
            Utc::now,
            &mut |line| lines.push(line.to_owned()),
        )
        .await;
        assert_eq!(decision, LandingDecision::Land { verdict: None });
        assert_eq!(decision.gate(), None);

        let failed = submit_request(&tmp, "beta", Status::Failed);
        let store = seeded_store(&failed.run_id, "beta");
        let decision = gate_landing(
            &store,
            Some(&source),
            &failed,
            &GateOptions::default(),
            Utc::now,
            &mut |line| lines.push(line.to_owned()),
        )
        .await;
        assert_eq!(decision, LandingDecision::Land { verdict: None });
        assert_eq!(
            kinds(&store, &failed.run_id),
            [DomainEventKind::LifecycleInitialized]
        );
        assert!(lines.is_empty());
    }

    #[tokio::test]
    async fn gate_landing_failing_gate_parks_with_both_witnesses() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(&root, "alpha", json!([cmd("exit 1", "gate-check")]));
        let source = FilesystemHeldOutGraderSource::new(&root);
        let request = submit_request(&tmp, "alpha", Status::Done);
        let store = seeded_store(&request.run_id, "alpha");
        let mut lines = Vec::new();
        let decision = gate_landing(
            &store,
            Some(&source),
            &request,
            &GateOptions::default(),
            Utc::now,
            &mut |line| lines.push(line.to_owned()),
        )
        .await;
        assert!(!decision.lands());
        assert_eq!(decision.gate(), Some(GateOutcome::Fail));
        assert_eq!(decision.park_kind(), Some(PARK_KIND_HELD_OUT_GATE));
        let LandingDecision::Park { detail, verdict } = &decision else {
            panic!("expected a park");
        };
        assert!(detail.contains("gate-check"));
        assert_eq!(detail, &verdict.reason);
        assert_eq!(
            kinds(&store, &request.run_id),
            [
                DomainEventKind::LifecycleInitialized,
                DomainEventKind::HeldOutGateEvaluated,
                DomainEventKind::LandingParked,
            ]
        );
        assert!(lines[0].contains("alpha: held-out landing gate BLOCKED"));
        // The sandbox is left in place for forensics.
        assert!(request.sandbox.is_dir());
    }

    #[tokio::test]
    async fn gate_landing_recording_failure_is_logged_not_fatal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(&root, "alpha", json!([cmd("exit 1", "gate-check")]));
        let source = FilesystemHeldOutGraderSource::new(&root);
        let request = submit_request(&tmp, "alpha", Status::Done);
        // No lifecycle row: the witnesses have nothing to attach to and the
        // decision must still be the gate's.
        let store = InMemoryStore::new();
        let mut lines = Vec::new();
        let decision = gate_landing(
            &store,
            Some(&source),
            &request,
            &GateOptions::default(),
            Utc::now,
            &mut |line| lines.push(line.to_owned()),
        )
        .await;
        assert!(!decision.lands());
        assert!(
            store
                .list_domain_events(&request.run_id)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn three_outcomes_are_distinguishable() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("held_out");
        register(&root, "land-ok", json!([cmd("true", "ok")]));
        register(&root, "gate-fail", json!([cmd("exit 1", "blocks")]));
        let source = FilesystemHeldOutGraderSource::new(&root);
        let mut markers = std::collections::BTreeSet::new();
        for (id, status) in [
            ("land-ok", Status::Done),
            ("agent-fail", Status::Failed),
            ("gate-fail", Status::Done),
        ] {
            let request = submit_request(&tmp, id, status);
            let store = seeded_store(&request.run_id, id);
            let decision = gate_landing(
                &store,
                Some(&source),
                &request,
                &GateOptions::default(),
                Utc::now,
                &mut |_| {},
            )
            .await;
            markers.insert((status, decision.gate().map(|g| g.as_str())));
            assert_eq!(decision.lands(), id != "gate-fail");
        }
        assert_eq!(markers.len(), 3);
        assert!(markers.contains(&(Status::Done, Some("pass"))));
        assert!(markers.contains(&(Status::Failed, None)));
        assert!(markers.contains(&(Status::Done, Some("fail"))));
    }

    mod gate_invariants;
}
