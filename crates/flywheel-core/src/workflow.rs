//! Single-task workflow: run and steer one flywheel task against a real agent.
//!
//! Port of the Python `flywheel_core.workflow`. This is the core single-task
//! entry point: it owns no execution logic of its own -- running a task
//! delegates to [`crate::harness::run_task`] with the production agents-backed
//! invoker, and steering enqueues control commands. It covers exactly one
//! task at a time; multi-task scheduling (selection over a prerequisite DAG,
//! phases, the live dashboard) lives in the orchestrator crate.
//!
//! The operator verbs are reachable through [`main`] (the Rust spelling of
//! `python -m flywheel_core.workflow`), with argparse-identical usage, help,
//! and error text:
//!
//! ```text
//! run GOAL_OR_FILE [--check CMD] [--rubric A] [--db PATH] [--sandbox DIR]
//!     [--model MODEL] [--max-retries N] [--max-turns N] [--json | --quiet]
//! is-done TASK_FILE [--db PATH]
//! interrupt RUN_ID [--db PATH]
//! steer RUN_ID MESSAGE [--db PATH]
//! set-model RUN_ID MODEL [--db PATH]
//! approve RUN_ID [--db PATH]
//! reject RUN_ID [--feedback TEXT] [--db PATH]
//! ```
//!
//! `run` accepts either an inline goal string or a task-file path; an inline
//! goal with no `--check`/`--rubric` is an unverified run (DONE reflects the
//! agent's own claim). Events stream to stdout as they fire (readable by
//! default, NDJSON with `--json`).
//!
//! There is no vendor SDK in the Rust runtime: the production invoker is the
//! `flywheel-agents` runtime under a [`LocalHost`], the same path the Python
//! `agent_id=` opt-in took. The live message stream therefore renders the
//! normalized agent events ([`flywheel_agents::AgentEvent`]) rather than SDK
//! message objects.

use crate::agents_invoke::{AgentsInvoker, ColdSession, InvokeFn, MessageObserver};
use crate::deadline_config::DeadlineConfig;
use crate::event_serde::format_timestamp;
use crate::grader_rubric::{AgentsJudgeInvoke, JudgeInvoke};
use crate::harness::{
    DEFAULT_AGENT_ID, DEFAULT_SUMMARIZER_MAX_TURNS, FinalizeStrandedOptions, HarnessConfig,
    HarnessError, HarnessOutcome, LandabilityGate, RunOptions, SharedSink, ShutdownSignal,
    finalize_stranded_lifecycle, run_task,
};
use crate::invoker_client::{
    CONTROL_COMMAND_APPROVE, CONTROL_COMMAND_INTERRUPT, CONTROL_COMMAND_REJECT,
    CONTROL_COMMAND_SAY, CONTROL_COMMAND_SET_MODEL, ControlPlane, ProgressProbe,
};
use crate::lifecycle::{Lifecycle, Status};
use crate::loaders::{TaskLoadError, load_task_file};
use crate::recovery_summarizer::{AgentsSummarizerInvoke, SummarizerInvoke};
use crate::store::{
    ControlCommandStore, HarnessStore, JsonObject, LifecycleFilter, LifecycleStore, StoreError,
    TelemetryRecord, TelemetrySink,
};
use crate::store_sqlite::SqliteStore;
use crate::task::{CommandGrader, Grader, RubricGrader, Task, ValidationError, python_float_repr};
use crate::telemetry_file::{FileTelemetrySink, JsonStyle, python_json_dumps};
use chrono::{DateTime, Utc};
use flywheel_agents::{AgentConfiguration, LocalHost, PermissionPolicy};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where the verbs persist by default: `.flywheel/flywheel.sqlite`.
pub const DEFAULT_DB_PATH: &str = ".flywheel/flywheel.sqlite";
/// Default agent turn cap per iteration.
pub const DEFAULT_MAX_TURNS: u32 = 500;
/// Default harness retry budget after `failed_validation`.
pub const DEFAULT_MAX_RETRIES: u32 = 1;

/// Composite store contract a single-task run is driven through.
///
/// The union of every store surface [`run_task_object`] touches: the
/// [`HarnessStore`] the harness writes the run record through (task version,
/// lifecycle, attempts, domain events, grader results), the
/// `list_lifecycles` cross-task read stranded recovery needs, and the
/// [`ControlCommandStore`] the control watcher claims from. Every
/// `HarnessStore` already carries all three, so the in-memory, SQLite, and
/// Postgres stores satisfy it automatically and a caller can inject any of
/// them.
pub trait RunStore: HarnessStore {}

impl<T: HarnessStore + ?Sized> RunStore for T {}

/// A line printer: the stdout / stderr seam the workflow writes through.
pub type LineSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The per-record stdout printer the live event stream renders through.
pub type EventEmitter = Arc<dyn Fn(&TelemetryRecord) + Send + Sync>;

/// A sink that prints each line to stdout.
pub fn stdout_sink() -> LineSink {
    Arc::new(|line: &str| println!("{line}"))
}

/// A sink that prints each line to stderr.
pub fn stderr_sink() -> LineSink {
    Arc::new(|line: &str| eprintln!("{line}"))
}

/// Every failure the workflow surfaces to its caller.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Harness(#[from] HarnessError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    TaskLoad(#[from] TaskLoadError),
    #[error(transparent)]
    Validation(#[from] ValidationError),
}

impl WorkflowError {
    /// Whether this is the operator-interrupt exit (the lifecycle was parked
    /// `interrupted` in-band, nothing crashed).
    pub fn is_interrupted(&self) -> bool {
        matches!(self, WorkflowError::Harness(HarnessError::Interrupted))
    }
}

// --- Status queries ---------------------------------------------------------

/// Whether `task_id` has at least one `done` lifecycle in `store`.
pub fn has_done_lifecycle(store: &dyn LifecycleStore, task_id: &str) -> Result<bool, StoreError> {
    let filter = LifecycleFilter::default()
        .with_statuses([Status::Done])
        .with_task_id(task_id);
    Ok(!store.list_lifecycles(&filter)?.is_empty())
}

// --- Stranded-lifecycle recovery -------------------------------------------

/// The statuses a lifecycle is considered stranded in once its worker is gone.
pub const STRANDED_STATUSES: [Status; 2] = [Status::Running, Status::Validating];

/// Return run_ids whose lifecycle is mid-attempt with no live worker.
///
/// A run is considered stranded when its status sits in `running` or
/// `validating` after the worker that owned it exited -- there is no
/// process-level liveness check available here, so the caller uses this only
/// at boundaries where it knows no harness is currently running that
/// lifecycle (worker start, post-interrupt cleanup, `recover`). Ordered
/// oldest-updated first (`list_lifecycles` orders newest first; reversed).
pub fn stranded_run_ids(
    store: &dyn LifecycleStore,
    task_id: Option<&str>,
) -> Result<Vec<String>, StoreError> {
    let mut filter = LifecycleFilter::default().with_statuses(STRANDED_STATUSES);
    if let Some(task_id) = task_id {
        filter = filter.with_task_id(task_id);
    }
    let lifecycles = store.list_lifecycles(&filter)?;
    Ok(lifecycles.into_iter().rev().map(|lc| lc.run_id).collect())
}

/// Finalize every stranded lifecycle, optionally filtered by `task_id`.
///
/// Delegates to [`finalize_stranded_lifecycle`] for each match; returns the
/// run_ids that were actually finalized (i.e. were in `running` /
/// `validating` at the time of the call).
pub fn recover_stranded_lifecycles(
    store: Arc<dyn HarnessStore>,
    task_id: Option<&str>,
    sink: Option<SharedSink>,
) -> Result<Vec<String>, StoreError> {
    let mut finalized = Vec::new();
    for run_id in stranded_run_ids(store.as_ref(), task_id)? {
        let mut options = FinalizeStrandedOptions::default();
        if let Some(sink) = &sink {
            options = options.with_sink(sink.clone());
        }
        if finalize_stranded_lifecycle(store.clone(), &run_id, options)? {
            finalized.push(run_id);
        }
    }
    Ok(finalized)
}

// --- Live event / message formatting ---------------------------------------

/// The live stdout stream mode of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventsMode {
    /// Readable one-line-per-record stream.
    Plain,
    /// NDJSON, one record per line.
    Json,
    /// Silent.
    #[default]
    None,
}

impl EventsMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            EventsMode::Plain => "plain",
            EventsMode::Json => "json",
            EventsMode::None => "none",
        }
    }

    /// Parse the Python spelling (`plain` / `json` / `none`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "plain" => Some(EventsMode::Plain),
            "json" => Some(EventsMode::Json),
            "none" => Some(EventsMode::None),
            _ => None,
        }
    }
}

impl fmt::Display for EventsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

const ELLIPSIS: char = '\u{2026}';

/// Collapse newlines and truncate to `limit` characters (Python `_short`):
/// a longer value keeps `limit - 1` characters plus a single ellipsis.
pub fn short(value: &str, limit: usize) -> String {
    let text: String = value
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if text.chars().count() <= limit {
        return text;
    }
    let keep = limit.saturating_sub(1).max(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push(ELLIPSIS);
    out
}

/// Python `repr()` of a string: single quotes unless the text holds a single
/// quote and no double quote, backslash escapes for the quote, backslashes,
/// and control characters, printable non-ASCII kept verbatim.
fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `repr()` of the value a JSON document decodes to.
fn py_repr(value: &Value) -> String {
    match value {
        Value::String(s) => py_repr_str(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(obj) => {
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr_str(k), py_repr(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        other => py_str(other),
    }
}

/// Python `str()` of the value a JSON document decodes to (`None`, `True`,
/// float `repr`, raw strings, list / dict `repr`).
pub fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => python_float_repr(f),
            _ => n.to_string(),
        },
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => py_repr(value),
    }
}

/// Render a telemetry event's payload as a compact `k=v` tail: each value
/// collapsed to one line and capped at 40 characters, the whole tail capped
/// at `limit`.
pub fn event_payload_summary(payload: &JsonObject, limit: usize) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = payload
        .iter()
        .map(|(key, value)| format!("{key}={}", short(&py_str(value), 40)))
        .collect();
    let joined = parts.join(" ");
    if joined.chars().count() > limit {
        let mut out: String = joined.chars().take(limit.saturating_sub(1)).collect();
        out.push(ELLIPSIS);
        return out;
    }
    joined
}

/// One readable line per telemetry record for the default `run` stream:
/// `[HH:MM:SS] <kind>[ attempt=N][  k=v ...]`.
pub fn format_event_line(record: &TelemetryRecord) -> String {
    let attempt = record
        .attempt_number
        .map(|n| format!(" attempt={n}"))
        .unwrap_or_default();
    let ts = record.ts.format("%H:%M:%S");
    let summary = event_payload_summary(&record.payload, 100);
    let tail = if summary.is_empty() {
        String::new()
    } else {
        format!("  {summary}")
    };
    format!("[{ts}] {}{attempt}{tail}", record.kind)
}

/// One NDJSON line per telemetry record for `--json` consumers
/// (`json.dumps(..., sort_keys=True)`).
pub fn event_json_line(record: &TelemetryRecord) -> String {
    let value = json!({
        "run_id": record.run_id,
        "ts": format_timestamp(&record.ts),
        "kind": record.kind,
        "attempt_number": record.attempt_number,
        "iteration_number": record.iteration_number,
        "payload": Value::Object(record.payload.clone()),
    });
    python_json_dumps(&value, &JsonStyle::SORTED)
}

/// Render a tool call as `name(k=v, ...)` (first three inputs) for the live
/// stream.
pub fn format_tool_use(name: &str, tool_input: &JsonObject) -> String {
    if tool_input.is_empty() {
        return format!("{name}()");
    }
    let kv: Vec<String> = tool_input
        .iter()
        .take(3)
        .map(|(key, value)| format!("{key}={}", short(&py_str(value), 40)))
        .collect();
    format!("{name}({})", kv.join(", "))
}

fn payload_of(msg: &Value) -> JsonObject {
    msg.get("payload")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn payload_str<'a>(payload: &'a JsonObject, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

/// Map one recorded agent message (a serialized normalized event) to a
/// `(LABEL, detail)` pair for the live stream.
///
/// `message.assistant` and `tool.started` render as `ASSISTANT` (text, or
/// `Write(file_path=...)`), `tool.finished` / `message.user` as `USER`
/// (`tool_result(<size>B[ ERR])` or the echoed text), `session.finished` as
/// `RESULT` (`subtype=... turns=...[ cost=$...]`), `session.started` as
/// `SYSTEM init`; any other kind is labelled by its upper-cased event type.
pub fn summarize_live_message(msg: &Value) -> (String, String) {
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = payload_of(msg);
    match kind {
        "message.assistant" => {
            let text = payload_str(&payload, "text").unwrap_or("").trim();
            let detail = if text.is_empty() {
                "(no content)".to_owned()
            } else {
                short(text, 120)
            };
            ("ASSISTANT".to_owned(), detail)
        }
        "tool.started" => {
            let name = payload_str(&payload, "tool_name").unwrap_or("");
            let input = payload
                .get("tool_input")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            ("ASSISTANT".to_owned(), format_tool_use(name, &input))
        }
        "tool.finished" => {
            let size = match payload.get("content") {
                None | Some(Value::Null) => 0,
                Some(Value::String(body)) => body.chars().count(),
                Some(other) => python_json_dumps(other, &JsonStyle::PYTHON_DEFAULT)
                    .chars()
                    .count(),
            };
            let err = if payload.get("is_error").and_then(Value::as_bool) == Some(true) {
                " ERR"
            } else {
                ""
            };
            ("USER".to_owned(), format!("tool_result({size}B{err})"))
        }
        "message.user" => {
            let text = payload_str(&payload, "text").unwrap_or("").trim();
            let detail = if text.is_empty() {
                "(empty)".to_owned()
            } else {
                short(text, 120)
            };
            ("USER".to_owned(), detail)
        }
        "session.finished" => {
            let subtype = payload.get("subtype").map(py_str).unwrap_or("None".into());
            let turns = payload
                .get("num_turns")
                .map(py_str)
                .unwrap_or("None".into());
            let cost = payload
                .get("total_cost_usd")
                .and_then(Value::as_f64)
                .map(|c| format!(" cost=${c:.4}"))
                .unwrap_or_default();
            (
                "RESULT".to_owned(),
                format!("subtype={subtype} turns={turns}{cost}"),
            )
        }
        "session.started" => ("SYSTEM".to_owned(), "init".to_owned()),
        other => (other.to_uppercase(), String::new()),
    }
}

/// One readable line for a live agent message, aligned with event lines.
pub fn format_live_message(msg: &Value, now: DateTime<Utc>) -> String {
    let (label, detail) = summarize_live_message(msg);
    let ts = now.format("%H:%M:%S");
    let tail = if detail.is_empty() {
        String::new()
    } else {
        format!("  {detail}")
    };
    format!("[{ts}] {label}{tail}")
}

/// One NDJSON line for a live agent message (the persisted serialization,
/// keys sorted).
pub fn message_json_line(msg: &Value) -> String {
    python_json_dumps(msg, &JsonStyle::SORTED)
}

/// Build the per-message stdout printer that interleaves with events.
///
/// `None` for [`EventsMode::None`]; a readable formatter for
/// [`EventsMode::Plain`]; an NDJSON formatter for [`EventsMode::Json`].
/// Prints through `out` so messages share the event stream and land in true
/// arrival order.
pub fn make_message_observer(events: EventsMode, out: LineSink) -> Option<MessageObserver> {
    match events {
        EventsMode::None => None,
        EventsMode::Json => Some(Arc::new(move |msg: &Value| out(&message_json_line(msg)))),
        EventsMode::Plain => Some(Arc::new(move |msg: &Value| {
            out(&format_live_message(msg, Utc::now()))
        })),
    }
}

/// Build the per-record stdout printer for [`run_task_object`].
///
/// `None` for [`EventsMode::None`] (no wrapping); a readable line-formatter
/// for [`EventsMode::Plain`]; an NDJSON formatter for [`EventsMode::Json`].
pub fn make_event_emitter(events: EventsMode, out: LineSink) -> Option<EventEmitter> {
    match events {
        EventsMode::None => None,
        EventsMode::Json => Some(Arc::new(move |record: &TelemetryRecord| {
            out(&event_json_line(record))
        })),
        EventsMode::Plain => Some(Arc::new(move |record: &TelemetryRecord| {
            out(&format_event_line(record))
        })),
    }
}

/// Wrap a [`TelemetrySink`] and print harness telemetry live.
///
/// Every record is appended to `wrapped` first (the durable per-run JSONL
/// file), then `harness.*` records are additionally handed to `emit` so the
/// run is observable live on stdout while the agent works. Agent message
/// records are skipped here -- the live message observer already renders the
/// agent's turns -- and `domain.*` mirror lines are skipped to keep the
/// stream's shape unchanged (lifecycle changes still surface via their
/// `harness.*` companions).
pub struct StreamingTelemetrySink {
    wrapped: SharedSink,
    emit: EventEmitter,
}

impl StreamingTelemetrySink {
    pub fn new(wrapped: SharedSink, emit: EventEmitter) -> Self {
        Self { wrapped, emit }
    }
}

impl fmt::Debug for StreamingTelemetrySink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamingTelemetrySink")
            .finish_non_exhaustive()
    }
}

impl TelemetrySink for StreamingTelemetrySink {
    fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError> {
        self.wrapped.append_telemetry(record)?;
        if record.kind.starts_with("harness.") {
            (self.emit)(record);
        }
        Ok(())
    }
}

// --- Inline tasks -------------------------------------------------------------

/// Build an in-memory [`Task`] from a goal string and inline graders.
///
/// `checks` become [`CommandGrader`] entries named `check-1..N` (the strict
/// path); `rubric_assertions` collapse into a single [`RubricGrader`]. With
/// neither, the task is graderless -- an unverified run that records DONE on
/// the agent's own claim. No file is read or written: this is the
/// direct-construction API the CLI uses for an inline goal.
pub fn build_inline_task(
    goal: &str,
    checks: &[String],
    rubric_assertions: &[String],
    task_id: Option<&str>,
) -> Result<Task, ValidationError> {
    let mut graders = Vec::with_capacity(checks.len() + 1);
    for (index, cmd) in checks.iter().enumerate() {
        let mut grader = CommandGrader::new(cmd.clone())?;
        grader.name = Some(format!("check-{}", index + 1));
        graders.push(Grader::Command(grader));
    }
    if !rubric_assertions.is_empty() {
        graders.push(Grader::Rubric(RubricGrader::new(
            rubric_assertions.to_vec(),
        )?));
    }
    let mut task = Task::new(goal, graders);
    if let Some(task_id) = task_id {
        task = task.with_id(task_id);
    }
    task.validate()?;
    Ok(task)
}

// --- Observers ----------------------------------------------------------------

/// Combine multiple per-message callbacks into a single observer.
///
/// Each callback fires independently and is contained so a panicking
/// renderer cannot break the persistence observer (and vice versa). `None`
/// entries are filtered. Returns `None` when no observer remains, the single
/// observer when only one is set (no wrapping overhead), and a composed
/// observer otherwise.
pub fn compose_message_observers<I>(observers: I) -> Option<MessageObserver>
where
    I: IntoIterator<Item = Option<MessageObserver>>,
{
    let mut callbacks: Vec<MessageObserver> = observers.into_iter().flatten().collect();
    match callbacks.len() {
        0 => None,
        1 => callbacks.pop(),
        _ => Some(Arc::new(move |msg: &Value| {
            for cb in &callbacks {
                let _ = catch_unwind(AssertUnwindSafe(|| cb(msg)));
            }
        })),
    }
}

// --- Agent options ------------------------------------------------------------

/// Claude permission-mode vocabulary -> the generic policy name the
/// flywheel-agents runtime takes (docs/agent-harness.md section 15.3).
/// Unknown modes fall back to `supervised` (never silently to bypass).
pub const PERMISSION_POLICIES: [(&str, &str); 4] = [
    ("bypassPermissions", "auto"),
    ("acceptEdits", "supervised"),
    ("default", "supervised"),
    ("plan", "plan"),
];

/// The default Claude permission mode flywheel drives its agents in.
pub const DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";

/// Map a Claude permission mode onto the runtime's [`PermissionPolicy`].
pub fn permission_policy_for(permission_mode: &str) -> PermissionPolicy {
    match permission_mode {
        "bypassPermissions" => PermissionPolicy::Auto,
        "plan" => PermissionPolicy::Plan,
        _ => PermissionPolicy::Supervised,
    }
}

/// The skill surface exposed to the task agent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Skills {
    /// Every installed skill (the `"all"` sentinel).
    #[default]
    All,
    /// An explicit allow-list of skill names.
    Named(Vec<String>),
}

impl Skills {
    fn to_option(&self) -> Value {
        match self {
            Skills::All => json!("all"),
            Skills::Named(names) => json!(names),
        }
    }
}

/// The task-agent construction primitives (the decomposed sandbox policy).
///
/// The resolved sandbox policy is decomposed into plain primitives upstream
/// (the orchestrator -> [`run_task_object`] -> here), exactly as `model`
/// threads, so core never imports the orchestrator's policy type. Fields are
/// omitted from the adapter options at their unset sentinel, so the `fast`
/// primitives reproduce the bare construction: empty tool / MCP lists,
/// `setting_sources = None` (the adapter derives `user,project` from
/// `skills = All`), and `mcp_strict = false` leave the options at their
/// adapter defaults. The always-on exception is the inline `--settings`
/// layer suppressing the AI co-author git trailer (spec 00063), which the
/// claude-code adapter injects by default.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOptions {
    pub model: Option<String>,
    pub max_turns: u32,
    pub permission_mode: String,
    pub skills: Skills,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub setting_sources: Option<Vec<String>>,
    pub mcp_servers: Vec<String>,
    pub mcp_strict: bool,
    /// Maps onto the agent's bash-isolation sandbox setting: when enabled the
    /// settings layer carries `{"enabled": true, "autoAllowBashIfSandboxed":
    /// exec_auto_allow}`; when disabled (the `fast` default) nothing is set.
    pub exec_enabled: bool,
    pub exec_auto_allow: bool,
    /// Extra environment injected onto the agent process (additive over the
    /// inherited environment).
    pub agent_env: BTreeMap<String, String>,
    /// The adapter transport (`cli`); `None` keeps the adapter default.
    pub transport: Option<String>,
    /// Replace the agent's launch argv (the test fixture and container seam).
    pub command_override: Option<Vec<String>>,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            model: None,
            max_turns: DEFAULT_MAX_TURNS,
            permission_mode: DEFAULT_PERMISSION_MODE.to_owned(),
            skills: Skills::All,
            allowed_tools: Vec::new(),
            denied_tools: Vec::new(),
            setting_sources: None,
            mcp_servers: Vec::new(),
            mcp_strict: false,
            exec_enabled: false,
            exec_auto_allow: true,
            agent_env: BTreeMap::new(),
            transport: None,
            command_override: None,
        }
    }
}

/// Construct the task-agent [`AgentConfiguration`] from the sandbox
/// primitives (the Rust spelling of Python's `build_agent_options`).
///
/// The working directory is not part of the configuration: the invoker
/// carries it and the claude-code adapter adds it as `--add-dir` itself.
pub fn build_agent_configuration(agent_id: &str, options: &AgentOptions) -> AgentConfiguration {
    let mut configuration = AgentConfiguration::new(agent_id);
    configuration.model_id = options.model.clone();
    configuration.max_turns = Some(options.max_turns);
    configuration.permission_policy = permission_policy_for(&options.permission_mode);
    configuration.command_override = options.command_override.clone();
    configuration.environment = options.agent_env.clone();
    let adapter = &mut configuration.adapter_options;
    adapter.insert("skills".into(), options.skills.to_option());
    if !options.allowed_tools.is_empty() {
        adapter.insert("allowed_tools".into(), json!(options.allowed_tools));
    }
    if !options.denied_tools.is_empty() {
        adapter.insert("disallowed_tools".into(), json!(options.denied_tools));
    }
    if let Some(sources) = &options.setting_sources {
        adapter.insert("setting_sources".into(), json!(sources));
    }
    if !options.mcp_servers.is_empty() {
        adapter.insert("mcp_servers".into(), json!(options.mcp_servers));
    }
    if options.mcp_strict {
        adapter.insert("mcp_strict".into(), json!(true));
    }
    if options.exec_enabled {
        adapter.insert(
            "sandbox_exec".into(),
            json!({
                "enabled": true,
                "autoAllowBashIfSandboxed": options.exec_auto_allow,
            }),
        );
    }
    if let Some(transport) = &options.transport {
        adapter.insert("transport".into(), json!(transport));
    }
    configuration
}

/// The store-routed control plane of one run: the store the in-run watcher
/// claims operator commands from plus the run they are addressed to.
#[derive(Clone)]
pub struct ControlRoute {
    pub store: Arc<dyn ControlCommandStore + Send + Sync>,
    pub run_id: String,
}

impl fmt::Debug for ControlRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlRoute")
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

/// Production invoker: the real agent CLI spawned in `sandbox` through the
/// flywheel-agents runtime under a [`LocalHost`].
///
/// Each iteration runs with the in-process control watcher claiming
/// operator-issued commands from `control` (interrupt / set_model / say) and
/// applying them live against the open session. Each applied command lands
/// as a `harness.control_command_applied` record on `telemetry_sink`
/// (stamped with the iteration's attempt number); a failed dispatch records
/// `harness.control_command_failed` and the iteration continues. With no
/// `control` the watcher stays dormant (callers without a run identity).
///
/// `on_message` is the static stdout renderer composed with the per-request
/// persistence observer the harness threads through the invocation request.
/// Both fire for every message the instant it arrives, each contained so a
/// panicking renderer cannot break per-message persistence and vice versa.
pub fn make_claude_code_invoke(
    sandbox: &Path,
    agent_id: &str,
    options: &AgentOptions,
    on_message: Option<MessageObserver>,
    control: Option<ControlRoute>,
    telemetry_sink: Option<SharedSink>,
) -> InvokeFn {
    let configuration = build_agent_configuration(agent_id, options);
    let current_attempt: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let mut invoker = AgentsInvoker::new(configuration, sandbox).with_host(Arc::new(LocalHost));
    if let Some(route) = control {
        let mut plane = ControlPlane::new(route.store, route.run_id.clone());
        if let Some(sink) = telemetry_sink {
            let attempt = current_attempt.clone();
            let run_id = route.run_id;
            // Route control-plane events through the same sink the harness
            // streams to so the run file carries them and a live operator
            // sees them exactly like harness.* records. A failing sink never
            // breaks the run: the watcher's emit is best-effort by contract.
            plane = plane.with_audit_emit(Arc::new(move |kind: &str, payload: &JsonObject| {
                let mut record = TelemetryRecord::new(run_id.clone(), Utc::now(), kind);
                record.payload = payload.clone();
                record.attempt_number = *attempt.lock().unwrap_or_else(|e| e.into_inner());
                let _ = sink.append_telemetry(&record);
            }));
        }
        invoker = invoker.with_control_plane(plane);
    }
    let invoker = Arc::new(invoker);
    Arc::new(move |mut request| {
        *current_attempt.lock().unwrap_or_else(|e| e.into_inner()) = Some(request.attempt_number);
        let persistence = request.observers.on_message.take();
        request.observers.on_message = compose_message_observers([on_message.clone(), persistence]);
        let invoker = invoker.clone();
        Box::pin(async move { invoker.invoke(request).await })
    })
}

// --- Run-start helpers -----------------------------------------------------------

/// Resolve the workspace's base commit SHA (spec 00025 FR-11).
///
/// Called at run start against the freshly prepared workspace, so `HEAD`
/// here is the commit the worktree was created from. A sandbox that is not a
/// git checkout -- direct invocations without a worktree -- yields `None` and
/// the pin is omitted cleanly rather than fabricated. `git` is given ten
/// seconds; a hung or missing binary also yields `None`.
pub async fn resolve_base_commit_sha(sandbox: &Path) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(sandbox)
        .args(["rev-parse", "HEAD"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (sha.len() == 40).then_some(sha)
}

/// Route SIGTERM / SIGINT into `signal` for the run's duration.
///
/// Production shutdown -- `docker stop`, `kubectl delete pod`, `systemctl
/// stop` -- sends SIGTERM, whose default disposition terminates the process
/// without unwinding, so an in-flight [`run_task`] never reaches its
/// finalizer and its lifecycle is stranded in `running`. Triggering the
/// harness shutdown signal instead funnels operator shutdown into the same
/// in-band interrupt path, so a graceful terminate leaves a clean, resumable
/// `interrupted` lifecycle. SIGINT is routed the same way for uniformity.
///
/// Returns the listener task so the caller can abort it after the run; on a
/// non-unix host (no SIGTERM) nothing is installed. Tokio keeps its own
/// process-level signal registration alive after the listener is gone, so
/// a long-lived host that drives many runs should install one
/// [`ShutdownSignal`] at process start and pass it through
/// [`RunTaskOptions::shutdown`] instead of relying on this per-run install.
pub fn install_cancel_on_signal(signal: &ShutdownSignal) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal as unix_signal};
        let mut term = unix_signal(SignalKind::terminate()).ok()?;
        let signal = signal.clone();
        Some(tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            signal.trigger();
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
        None
    }
}

// --- run_task_object --------------------------------------------------------------

/// The keyword surface of [`run_task_object`]; every field has the same
/// meaning as its Python keyword and `Default` reproduces the Python
/// defaults.
#[derive(Clone, Default)]
pub struct RunTaskOptions {
    /// The agent to drive the run on; `None` resolves to
    /// [`DEFAULT_AGENT_ID`]. The transport rides [`AgentOptions::transport`].
    pub agent_id: Option<String>,
    /// Model, turn cap, permission mode, skills, tools, MCP, exec isolation,
    /// and environment of the task agent.
    pub agent: AgentOptions,
    /// Harness retry budget after `failed_validation`.
    pub max_retries: Option<u32>,
    /// Full replacement environment for command graders and the judge.
    pub grader_env: Option<BTreeMap<String, String>>,
    pub max_cost_usd: f64,
    pub max_tokens: u64,
    pub wall_clock_seconds: u64,
    /// Repo-owned wall-clock ceilings; `None` keeps the harness defaults.
    pub deadlines: Option<DeadlineConfig>,
    /// Repo-owned per-judge turn budget; `None` keeps the harness default.
    pub rubric_judge_max_turns: Option<u32>,
    /// Inject the iteration transport (tests, alternative agents); `None`
    /// builds the production invoker.
    pub invoke: Option<InvokeFn>,
    /// Diagnostics (`[workflow]` lines); `None` prints to stderr.
    pub stream: Option<LineSink>,
    /// The live event stream destination; `None` prints to stdout.
    pub events_out: Option<LineSink>,
    /// Resume an existing lifecycle instead of starting a new one.
    pub run_id: Option<String>,
    pub events: EventsMode,
    /// Provenance label recorded on the lifecycle seed.
    pub source: Option<String>,
    /// The worker identity recorded on the lifecycle seed.
    pub worker_id: Option<String>,
    /// Telemetry destination; `None` builds a [`FileTelemetrySink`] under
    /// `<db dir>/logs`.
    pub sink: Option<SharedSink>,
    /// Persistence backend; `None` opens the [`SqliteStore`] at `db_path`.
    pub store: Option<Arc<dyn RunStore>>,
    /// Base directory for per-attempt artifact subdirectories.
    pub artifacts_root: Option<PathBuf>,
    pub landability_gate: Option<LandabilityGate>,
    pub checkpoint_nudge_seconds: Option<f64>,
    pub checkpoint_progress_probe: Option<ProgressProbe>,
    /// The operator shutdown signal; `None` installs a per-run SIGINT /
    /// SIGTERM listener.
    pub shutdown: Option<ShutdownSignal>,
}

impl fmt::Debug for RunTaskOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunTaskOptions")
            .field("agent_id", &self.agent_id)
            .field("agent", &self.agent)
            .field("max_retries", &self.max_retries)
            .field("grader_env", &self.grader_env.is_some())
            .field("max_cost_usd", &self.max_cost_usd)
            .field("max_tokens", &self.max_tokens)
            .field("wall_clock_seconds", &self.wall_clock_seconds)
            .field("deadlines", &self.deadlines)
            .field("rubric_judge_max_turns", &self.rubric_judge_max_turns)
            .field("invoke", &self.invoke.is_some())
            .field("run_id", &self.run_id)
            .field("events", &self.events)
            .field("source", &self.source)
            .field("worker_id", &self.worker_id)
            .field("sink", &self.sink.is_some())
            .field("store", &self.store.is_some())
            .field("artifacts_root", &self.artifacts_root)
            .field("landability_gate", &self.landability_gate.is_some())
            .field("checkpoint_nudge_seconds", &self.checkpoint_nudge_seconds)
            .field(
                "checkpoint_progress_probe",
                &self.checkpoint_progress_probe.is_some(),
            )
            .field("shutdown", &self.shutdown.is_some())
            .finish()
    }
}

impl RunTaskOptions {
    pub fn with_invoke(mut self, invoke: InvokeFn) -> Self {
        self.invoke = Some(invoke);
        self
    }

    pub fn with_store(mut self, store: Arc<dyn RunStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_sink(mut self, sink: SharedSink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_stream(mut self, stream: LineSink) -> Self {
        self.stream = Some(stream);
        self
    }

    pub fn with_events(mut self, events: EventsMode, out: LineSink) -> Self {
        self.events = events;
        self.events_out = Some(out);
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    pub fn with_deadlines(mut self, deadlines: DeadlineConfig) -> Self {
        self.deadlines = Some(deadlines);
        self
    }

    pub fn with_rubric_judge_max_turns(mut self, turns: u32) -> Self {
        self.rubric_judge_max_turns = Some(turns);
        self
    }
}

/// The effective `agent_id` of a run.
fn effective_agent_id(options: &RunTaskOptions) -> &str {
    options.agent_id.as_deref().unwrap_or(DEFAULT_AGENT_ID)
}

/// The world-state pin recorded on every attempt's `agent_context` (spec
/// 00025 FR-11): the effective model id (`claude-code-default` when the agent
/// falls through to its own default), the runtime, the agent id, the sandbox,
/// and -- when the sandbox is a git checkout -- the base commit SHA.
pub async fn build_agent_context(
    options: &RunTaskOptions,
    sandbox: &Path,
) -> BTreeMap<String, String> {
    let mut context = BTreeMap::new();
    context.insert(
        "model_id".to_owned(),
        options
            .agent
            .model
            .clone()
            .unwrap_or_else(|| "claude-code-default".to_owned()),
    );
    context.insert("agent_sdk".to_owned(), "flywheel_agents".to_owned());
    context.insert(
        "agent_id".to_owned(),
        effective_agent_id(options).to_owned(),
    );
    context.insert("sandbox".to_owned(), sandbox.display().to_string());
    if let Some(sha) = resolve_base_commit_sha(sandbox).await {
        context.insert("base_commit_sha".to_owned(), sha);
    }
    context
}

/// The [`HarnessConfig`] one [`run_task_object`] call drives the harness with.
///
/// Repo-owned attempt budgets override the harness defaults only when
/// supplied: `deadlines` keeps its finite default-on ceilings and
/// `rubric_judge_max_turns` keeps 32 unless the caller passes them, so a
/// caller passing neither constructs a config identical to the defaults.
pub fn harness_config_for(
    options: &RunTaskOptions,
    sandbox: &Path,
    agent_context: BTreeMap<String, String>,
    rubric_judge_invoke: Option<Arc<dyn JudgeInvoke>>,
    recovery_summarizer_invoke: Option<Arc<dyn SummarizerInvoke>>,
) -> HarnessConfig {
    let defaults = HarnessConfig::default();
    HarnessConfig {
        max_retries: options.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
        max_cost_usd: options.max_cost_usd,
        max_tokens: options.max_tokens,
        wall_clock_seconds: options.wall_clock_seconds,
        artifacts_root: options.artifacts_root.clone(),
        agent_context,
        worktree: Some(sandbox.to_path_buf()),
        grader_env: options.grader_env.clone(),
        rubric_judge_max_turns: options
            .rubric_judge_max_turns
            .unwrap_or(defaults.rubric_judge_max_turns),
        rubric_judge_invoke,
        deadlines: options.deadlines.clone().unwrap_or_default(),
        recovery_summarizer_invoke,
        landability_gate: options.landability_gate.clone(),
        checkpoint_nudge_seconds: options
            .checkpoint_nudge_seconds
            .unwrap_or(defaults.checkpoint_nudge_seconds),
        checkpoint_progress_probe: options.checkpoint_progress_probe.clone(),
        agent_id: effective_agent_id(options).to_owned(),
        ..defaults
    }
}

/// Persist a lifecycle for `task` and drive it via [`run_task`].
///
/// The Task-first entry point: callers pass a [`Task`] they built however
/// they like (loaded from a file, or constructed inline from a goal), so this
/// owns no input-source assumptions. [`run_task_file`] is the thin
/// file-loading wrapper over this.
///
/// `options.invoke` defaults to the real agents-backed invoker; tests inject
/// a scripted one -- same seam, different transport. With the default
/// invoker the rubric judge and the recovery summarizer ride the same runtime
/// (and transport) as the task agent.
///
/// `options.events` selects the live stdout stream: silent, readable lines,
/// or NDJSON. The stream interleaves harness telemetry with the agent's own
/// turns as they arrive. When a caller injects its own `invoke`, only events
/// stream. Diagnostics (`[workflow]` lines) always go to `options.stream`
/// (stderr by default), kept separate from the event stream.
///
/// `options.run_id` selects fresh vs resume: `None` starts a new lifecycle;
/// an existing `run_id` makes the harness resume that lifecycle (its seed
/// append is swallowed and the persisted row wins).
///
/// `options.store` injects the persistence backend the whole run record is
/// driven through (spec 00075); `None` opens the [`SqliteStore`] at
/// `db_path` (creating its parent directory) and closes it on exit. When a
/// store is supplied this call opens no SqliteStore and creates no sqlite
/// file at `db_path` at all; the caller owns the store's lifetime.
/// `options.sink` likewise overrides the telemetry destination; `None`
/// builds a [`FileTelemetrySink`] rooted at `<db dir>/logs`.
///
/// Any prior lifecycle for `task.id` stranded mid-attempt is finalized to
/// `interrupted` before the new one is created. SIGINT / SIGTERM during the
/// run park the lifecycle `interrupted` in-band and surface as
/// [`HarnessError::Interrupted`] (see [`WorkflowError::is_interrupted`]).
pub async fn run_task_object(
    task: &Task,
    db_path: &Path,
    sandbox: &Path,
    options: RunTaskOptions,
) -> Result<HarnessOutcome, WorkflowError> {
    let out = options.stream.clone().unwrap_or_else(stderr_sink);
    // `source` rides the lifecycle into the LifecycleInitialized seed so the
    // run's provenance is queryable after the fact. On resume the persisted
    // row's source wins -- the seed append is swallowed.
    let mut lifecycle = Lifecycle::new(task.id.clone());
    if let Some(run_id) = &options.run_id {
        lifecycle.run_id = run_id.clone();
    }
    lifecycle.source = options.source.clone().unwrap_or_default();
    if let Some(worker_id) = &options.worker_id {
        lifecycle.worker_id = worker_id.clone();
    }

    std::fs::create_dir_all(sandbox)?;

    // Store injection (spec 00075): a caller-supplied store owns the entire
    // run record and this call opens no SqliteStore. Only a store opened
    // here is ours to close afterwards.
    let db_parent = db_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut owned_backend: Option<Arc<SqliteStore>> = None;
    let backend: Arc<dyn HarnessStore> = match &options.store {
        Some(store) => store.clone(),
        None => {
            if !db_parent.as_os_str().is_empty() {
                std::fs::create_dir_all(&db_parent)?;
            }
            let store = Arc::new(SqliteStore::open(db_path)?);
            owned_backend = Some(store.clone());
            store
        }
    };
    let mut owned_sink: Option<Arc<FileTelemetrySink>> = None;
    let sink: SharedSink = match &options.sink {
        Some(sink) => sink.clone(),
        None => {
            let file_sink = Arc::new(FileTelemetrySink::new(db_parent.join("logs")));
            owned_sink = Some(file_sink.clone());
            file_sink
        }
    };

    let result = drive_run(task, sandbox, &options, lifecycle, backend, sink, out).await;

    if let Some(store) = owned_backend
        && let Ok(store) = Arc::try_unwrap(store)
    {
        store.close()?;
    }
    if let Some(sink) = owned_sink {
        sink.close();
    }
    result
}

async fn drive_run(
    task: &Task,
    sandbox: &Path,
    options: &RunTaskOptions,
    mut lifecycle: Lifecycle,
    backend: Arc<dyn HarnessStore>,
    sink: SharedSink,
    out: LineSink,
) -> Result<HarnessOutcome, WorkflowError> {
    let events_out = options.events_out.clone().unwrap_or_else(stdout_sink);
    let run_sink: SharedSink = match make_event_emitter(options.events, events_out.clone()) {
        None => sink,
        Some(emitter) => Arc::new(StreamingTelemetrySink::new(sink, emitter)),
    };

    // The default invoker surfaces the agent's turns live via on_message and
    // runs the control-command watcher against the open session; an injected
    // invoke owns its own transport and the watcher is its responsibility.
    // The two internal LLM calls (rubric judge, recovery summarizer) ride the
    // same runtime and transport as the task agent on the default path;
    // `None` keeps the harness field defaults for an injected invoke.
    let agent_id = effective_agent_id(options);
    let mut rubric_judge_invoke: Option<Arc<dyn JudgeInvoke>> = None;
    let mut recovery_summarizer_invoke: Option<Arc<dyn SummarizerInvoke>> = None;
    let invoker = match &options.invoke {
        Some(invoke) => invoke.clone(),
        None => {
            let judge_max_turns = options
                .rubric_judge_max_turns
                .unwrap_or(HarnessConfig::default().rubric_judge_max_turns);
            let transport = options.agent.transport.as_deref();
            let mut judge_session = ColdSession::new(agent_id, None, judge_max_turns);
            let mut summarizer_session =
                ColdSession::new(agent_id, None, DEFAULT_SUMMARIZER_MAX_TURNS);
            if let Some(transport) = transport {
                judge_session = judge_session.with_transport(transport);
                summarizer_session = summarizer_session.with_transport(transport);
            }
            rubric_judge_invoke = Some(Arc::new(AgentsJudgeInvoke::with_session(
                judge_session,
                None,
            )));
            recovery_summarizer_invoke = Some(Arc::new(AgentsSummarizerInvoke::with_session(
                summarizer_session,
            )));
            let control_store: Arc<dyn ControlCommandStore + Send + Sync> = backend.clone();
            make_claude_code_invoke(
                sandbox,
                agent_id,
                &options.agent,
                make_message_observer(options.events, events_out),
                Some(ControlRoute {
                    store: control_store,
                    run_id: lifecycle.run_id.clone(),
                }),
                Some(run_sink.clone()),
            )
        }
    };

    // Recover any prior lifecycle for this task that was killed mid-attempt
    // before we create a new one. Keeps the audit trail honest (no
    // lifecycles stuck in `running` forever) and frees the retry budget --
    // INTERRUPTED is not a retry-source state.
    for stranded_run_id in
        recover_stranded_lifecycles(backend.clone(), Some(&task.id), Some(run_sink.clone()))?
    {
        out(&format!(
            "[workflow] recovered: stranded run {stranded_run_id} -> interrupted"
        ));
    }

    out(&format!("[workflow] task    : {}", task.id));
    if let Some(source) = &options.source {
        out(&format!("[workflow] source  : {source}"));
    }
    if task.graders.is_empty() {
        out(
            "[workflow] graders : none (unverified run \u{2014} DONE reflects the agent's own claim)",
        );
    }
    out(&format!("[workflow] run_id  : {}", lifecycle.run_id));

    // Take over SIGTERM/SIGINT for the duration of the run so an
    // operator-driven shutdown parks the in-flight lifecycle `interrupted`
    // instead of terminating the process with it stranded in `running`.
    let shutdown = options.shutdown.clone().unwrap_or_default();
    let listener = if options.shutdown.is_none() {
        install_cancel_on_signal(&shutdown)
    } else {
        None
    };

    let agent_context = build_agent_context(options, sandbox).await;
    let config = harness_config_for(
        options,
        sandbox,
        agent_context,
        rubric_judge_invoke,
        recovery_summarizer_invoke,
    );
    let run_options = RunOptions::default()
        .with_sink(run_sink.clone())
        .with_shutdown(shutdown);
    let result = run_task(
        task,
        &mut lifecycle,
        backend.clone(),
        &config,
        invoker,
        run_options,
    )
    .await;
    if let Some(listener) = listener {
        listener.abort();
    }

    match result {
        Ok(outcome) => {
            out(&format!(
                "[workflow] status  : {}",
                outcome.lifecycle.status.as_str()
            ));
            if !outcome.lifecycle.error.is_empty() {
                out(&format!("[workflow] error   : {}", outcome.lifecycle.error));
            }
            Ok(outcome)
        }
        Err(HarnessError::Interrupted) => {
            // Operator killed the worker mid-attempt (SIGTERM/SIGINT routed
            // through the shutdown signal). The harness parked the lifecycle
            // in-band; the sweep below is the belt-and-braces finalizer for
            // an attempt the park could not close.
            let finalize = FinalizeStrandedOptions::default().with_sink(run_sink);
            finalize_stranded_lifecycle(backend, &lifecycle.run_id, finalize)?;
            out("[workflow] status  : interrupted (worker received signal)");
            Err(HarnessError::Interrupted.into())
        }
        Err(err) => Err(err.into()),
    }
}

/// Load `task_file` and drive it via [`run_task_object`].
///
/// Thin convenience over the Task-first [`run_task_object`]: all execution
/// behavior (lifecycle persistence, stranded recovery, event streaming,
/// signal handling) lives there. The task file's path becomes the run's
/// `source`.
pub async fn run_task_file(
    task_file: &Path,
    db_path: &Path,
    sandbox: &Path,
    mut options: RunTaskOptions,
) -> Result<HarnessOutcome, WorkflowError> {
    let task = load_task_file(task_file)?;
    options.source = Some(task_file.display().to_string());
    run_task_object(&task, db_path, sandbox, options).await
}

// --- Steering / control commands ---------------------------------------------------
//
// Producers for the cross-process control plane. Each verb enqueues exactly
// one row into `control_commands` keyed by `run_id`; the in-process watcher
// inside the worker's invoker claims and applies it on its next tick. The CLI
// never talks to the live session directly -- the store is the channel -- so
// steering works against a detached worker daemon. A command is persisted
// unconditionally; if the named run is not currently in-flight the row sits
// pending and is recorded as stale per claim semantics.

/// Persist one control command into the store and report the id.
///
/// Returns `0` after printing the enqueue receipt (`enqueued #<id> kind=...`)
/// plus, when the lifecycle is not currently in-flight, a stderr note
/// explaining the row stays pending per claim semantics. An unknown `run_id`
/// is a producer-side error: exit code `2` with a clear message and nothing
/// enqueued. A store failure prints `error: ...` and returns `1`.
///
/// `store` injects the backend the command lands in; `None` opens the
/// [`SqliteStore`] at `db_path` (creating its parent directory) and closes
/// it on exit. `approve` / `reject` target a parked `awaiting_approval`
/// lifecycle, so the in-flight check accepts that status for those verbs
/// and the standard `running` / `validating` set for everything else.
pub fn enqueue_control_command(
    db_path: &Path,
    run_id: &str,
    kind: &str,
    payload: JsonObject,
    store: Option<Arc<dyn RunStore>>,
) -> i32 {
    match enqueue_control_command_inner(db_path, run_id, kind, payload, store) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}

fn enqueue_control_command_inner(
    db_path: &Path,
    run_id: &str,
    kind: &str,
    payload: JsonObject,
    store: Option<Arc<dyn RunStore>>,
) -> Result<i32, WorkflowError> {
    let mut owned: Option<Arc<SqliteStore>> = None;
    let backend: Arc<dyn RunStore> = match store {
        Some(store) => store,
        None => {
            if let Some(parent) = db_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            let store = Arc::new(SqliteStore::open(db_path)?);
            owned = Some(store.clone());
            store
        }
    };
    let outcome = (|| -> Result<Option<(Lifecycle, bool, Option<i64>)>, StoreError> {
        let Some(lifecycle) = backend.load_lifecycle(run_id)? else {
            return Ok(None);
        };
        let in_flight_statuses: &[Status] =
            if kind == CONTROL_COMMAND_APPROVE || kind == CONTROL_COMMAND_REJECT {
                &[Status::AwaitingApproval]
            } else {
                &[Status::Running, Status::Validating]
            };
        let in_flight = in_flight_statuses.contains(&lifecycle.status);
        let record = backend.enqueue_command(run_id, kind, payload, Utc::now())?;
        Ok(Some((lifecycle, in_flight, record.id)))
    })();
    drop(backend);
    if let Some(store) = owned
        && let Ok(store) = Arc::try_unwrap(store)
    {
        store.close()?;
    }
    let Some((lifecycle, in_flight, id)) = outcome? else {
        eprintln!("error: run {run_id:?} is unknown to this store; no command enqueued");
        return Ok(2);
    };
    let id = id.map_or_else(|| "None".to_owned(), |id| id.to_string());
    println!("enqueued #{id} kind={kind} run_id={run_id}");
    if !in_flight {
        eprintln!(
            "  note: run {run_id} is not in-flight (status={}); the command stays pending \
             and is recorded as stale per claim semantics",
            lifecycle.status.as_str()
        );
    }
    Ok(0)
}

// --- CLI plumbing -------------------------------------------------------------------

const PROG: &str = "python -m flywheel_core.workflow";

const MAIN_USAGE: &str = "usage: python -m flywheel_core.workflow [-h]
                                        {run,is-done,interrupt,steer,set-model,approve,reject} ...";

const MAIN_HELP: &str = "usage: python -m flywheel_core.workflow [-h]
                                        {run,is-done,interrupt,steer,set-model,approve,reject} ...

Run and steer a single flywheel task. Multi-task scheduling lives in the
flywheel-orchestrator package, exposed via the flywheel/fw product shell.

positional arguments:
  {run,is-done,interrupt,steer,set-model,approve,reject}
    run                 Run a goal or task file via flywheel.run_task,
                        streaming events to stdout; exit 0 only on DONE.
    is-done             Exit 0 if the named task has at least one done
                        lifecycle.
    interrupt           Enqueue an interrupt control command against RUN_ID.
                        The in-process watcher claims and applies it on its
                        next tick, driving the lifecycle to INTERRUPTED via
                        the same finalization SIGINT/SIGTERM use.
    steer               Inject an operator MESSAGE into the live conversation
                        for RUN_ID. The watcher dispatches via
                        ClaudeSDKClient.query so the running session sees a
                        new user turn. The task definition is not mutated.
    set-model           Switch the live session for RUN_ID to MODEL.
                        Dispatched via ClaudeSDKClient.set_model. An invalid
                        model id lands as a harness.control_command_failed
                        event; the run continues on the prior model.
    approve             Enqueue an approve control command against RUN_ID. The
                        out-of-band manual-approval resolver claims it on the
                        next reactive tick, writes a passing manual grader
                        receipt for the parked gate, and advances the
                        lifecycle (next gate or DONE).
    reject              Enqueue a reject control command against RUN_ID,
                        optionally carrying operator feedback. The out-of-band
                        resolver writes a failing manual grader receipt and
                        routes the lifecycle through FAILED_VALIDATION;
                        --feedback flows into the next attempt's reviewer-
                        feedback section.

options:
  -h, --help            show this help message and exit";

const RUN_USAGE: &str = "usage: python -m flywheel_core.workflow run [-h] [--db DB] [--check CMD]
                                            [--rubric ASSERTION]
                                            [--sandbox SANDBOX]
                                            [--model MODEL]
                                            [--max-turns MAX_TURNS]
                                            [--max-retries MAX_RETRIES]
                                            [--json | --quiet]
                                            GOAL_OR_FILE";

const RUN_HELP: &str = "usage: python -m flywheel_core.workflow run [-h] [--db DB] [--check CMD]
                                            [--rubric ASSERTION]
                                            [--sandbox SANDBOX]
                                            [--model MODEL]
                                            [--max-turns MAX_TURNS]
                                            [--max-retries MAX_RETRIES]
                                            [--json | --quiet]
                                            GOAL_OR_FILE

TARGET is either an inline goal string (e.g. 'add retries to the http client')
or a path to a task JSON file. An inline goal with no --check/--rubric runs
unverified: it records DONE on the agent's own claim. Events stream to stdout
as they fire.

positional arguments:
  GOAL_OR_FILE          Inline goal string, or path to a flywheel task JSON
                        file.

options:
  -h, --help            show this help message and exit
  --db DB               SQLite database path (default:
                        .flywheel/flywheel.sqlite).
  --check CMD           Add a command grader (pass=exit 0) for an inline goal.
                        Repeatable. Ignored for a task file.
  --rubric ASSERTION    Add a natural-language rubric assertion for an inline
                        goal (LLM-judged). Repeatable. Ignored for a task
                        file.
  --sandbox SANDBOX     Directory the agent operates in (default: current
                        dir).
  --model MODEL         Override the Claude model passed to the SDK.
  --max-turns MAX_TURNS
                        Max agent turns per iteration (default: 500).
  --max-retries MAX_RETRIES
                        Harness retry budget after failed_validation (default:
                        1).
  --json                Stream events as NDJSON instead of readable lines.
  --quiet               Suppress the event stream; print only the final
                        status.";

const IS_DONE_USAGE: &str =
    "usage: python -m flywheel_core.workflow is-done [-h] [--db DB] task_file";

const IS_DONE_HELP: &str =
    "usage: python -m flywheel_core.workflow is-done [-h] [--db DB] task_file

positional arguments:
  task_file

options:
  -h, --help  show this help message and exit
  --db DB     SQLite database path (default: .flywheel/flywheel.sqlite).";

const INTERRUPT_USAGE: &str =
    "usage: python -m flywheel_core.workflow interrupt [-h] [--db DB] RUN_ID";

const INTERRUPT_HELP: &str =
    "usage: python -m flywheel_core.workflow interrupt [-h] [--db DB] RUN_ID

positional arguments:
  RUN_ID      Lifecycle run_id of the in-flight run to interrupt.

options:
  -h, --help  show this help message and exit
  --db DB     SQLite database path (default: .flywheel/flywheel.sqlite).";

const STEER_USAGE: &str =
    "usage: python -m flywheel_core.workflow steer [-h] [--db DB] RUN_ID MESSAGE";

const STEER_HELP: &str =
    "usage: python -m flywheel_core.workflow steer [-h] [--db DB] RUN_ID MESSAGE

positional arguments:
  RUN_ID      Lifecycle run_id of the in-flight run to steer.
  MESSAGE     Operator message text to inject as a user turn.

options:
  -h, --help  show this help message and exit
  --db DB     SQLite database path (default: .flywheel/flywheel.sqlite).";

const SET_MODEL_USAGE: &str =
    "usage: python -m flywheel_core.workflow set-model [-h] [--db DB] RUN_ID MODEL";

const SET_MODEL_HELP: &str =
    "usage: python -m flywheel_core.workflow set-model [-h] [--db DB] RUN_ID MODEL

positional arguments:
  RUN_ID      Lifecycle run_id of the in-flight run to retarget.
  MODEL       Model identifier to switch the live session to.

options:
  -h, --help  show this help message and exit
  --db DB     SQLite database path (default: .flywheel/flywheel.sqlite).";

const APPROVE_USAGE: &str = "usage: python -m flywheel_core.workflow approve [-h] [--db DB] RUN_ID";

const APPROVE_HELP: &str = "usage: python -m flywheel_core.workflow approve [-h] [--db DB] RUN_ID

positional arguments:
  RUN_ID      Lifecycle run_id of the AWAITING_APPROVAL run to approve.

options:
  -h, --help  show this help message and exit
  --db DB     SQLite database path (default: .flywheel/flywheel.sqlite).";

const REJECT_USAGE: &str = "usage: python -m flywheel_core.workflow reject [-h] [--feedback TEXT]
                                               [--db DB]
                                               RUN_ID";

const REJECT_HELP: &str = "usage: python -m flywheel_core.workflow reject [-h] [--feedback TEXT]
                                               [--db DB]
                                               RUN_ID

positional arguments:
  RUN_ID           Lifecycle run_id of the AWAITING_APPROVAL run to reject.

options:
  -h, --help       show this help message and exit
  --feedback TEXT  Optional operator critique to attach to the rejection.
                   Renders in the next attempt's # Reviewer feedback section
                   so the agent can address it. Absent feedback is recorded as
                   \"(no feedback provided)\".
  --db DB          SQLite database path (default: .flywheel/flywheel.sqlite).";

const COMMANDS: [&str; 7] = [
    "run",
    "is-done",
    "interrupt",
    "steer",
    "set-model",
    "approve",
    "reject",
];

/// The static shape of one subcommand parser.
struct SubSpec {
    name: &'static str,
    usage: &'static str,
    help: &'static str,
    /// Positional metavars, in order.
    positionals: &'static [&'static str],
    /// `(long option, takes a value)`, in declaration order.
    options: &'static [(&'static str, bool)],
}

const RUN_SPEC: SubSpec = SubSpec {
    name: "run",
    usage: RUN_USAGE,
    help: RUN_HELP,
    positionals: &["GOAL_OR_FILE"],
    options: &[
        ("--db", true),
        ("--check", true),
        ("--rubric", true),
        ("--sandbox", true),
        ("--model", true),
        ("--max-turns", true),
        ("--max-retries", true),
        ("--json", false),
        ("--quiet", false),
    ],
};

const IS_DONE_SPEC: SubSpec = SubSpec {
    name: "is-done",
    usage: IS_DONE_USAGE,
    help: IS_DONE_HELP,
    positionals: &["task_file"],
    options: &[("--db", true)],
};

const INTERRUPT_SPEC: SubSpec = SubSpec {
    name: "interrupt",
    usage: INTERRUPT_USAGE,
    help: INTERRUPT_HELP,
    positionals: &["RUN_ID"],
    options: &[("--db", true)],
};

const STEER_SPEC: SubSpec = SubSpec {
    name: "steer",
    usage: STEER_USAGE,
    help: STEER_HELP,
    positionals: &["RUN_ID", "MESSAGE"],
    options: &[("--db", true)],
};

const SET_MODEL_SPEC: SubSpec = SubSpec {
    name: "set-model",
    usage: SET_MODEL_USAGE,
    help: SET_MODEL_HELP,
    positionals: &["RUN_ID", "MODEL"],
    options: &[("--db", true)],
};

const APPROVE_SPEC: SubSpec = SubSpec {
    name: "approve",
    usage: APPROVE_USAGE,
    help: APPROVE_HELP,
    positionals: &["RUN_ID"],
    options: &[("--db", true)],
};

const REJECT_SPEC: SubSpec = SubSpec {
    name: "reject",
    usage: REJECT_USAGE,
    help: REJECT_HELP,
    positionals: &["RUN_ID"],
    options: &[("--feedback", true), ("--db", true)],
};

/// How a parse ended short of a dispatchable command: argparse's help exit
/// (`0`, help on stdout) or usage error (`2`, usage plus `prog: error:
/// message` on stderr).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliExit {
    Help(String),
    Error {
        usage: String,
        prog: String,
        message: String,
    },
}

impl CliExit {
    fn error(usage: &str, prog: &str, message: impl Into<String>) -> Self {
        CliExit::Error {
            usage: usage.to_owned(),
            prog: prog.to_owned(),
            message: message.into(),
        }
    }

    /// Print per argparse and return the exit code.
    pub fn report(&self) -> i32 {
        match self {
            CliExit::Help(text) => {
                println!("{text}");
                0
            }
            CliExit::Error {
                usage,
                prog,
                message,
            } => {
                eprintln!("{usage}");
                eprintln!("{prog}: error: {message}");
                2
            }
        }
    }
}

/// The `run` verb's parsed arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    pub target: String,
    pub db: Option<String>,
    pub check: Vec<String>,
    pub rubric: Vec<String>,
    pub sandbox: Option<String>,
    pub model: Option<String>,
    pub max_turns: u32,
    pub max_retries: u32,
    pub json: bool,
    pub quiet: bool,
}

/// One parsed core verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(RunArgs),
    IsDone {
        task_file: String,
        db: Option<String>,
    },
    Interrupt {
        run_id: String,
        db: Option<String>,
    },
    Steer {
        run_id: String,
        message: String,
        db: Option<String>,
    },
    SetModel {
        run_id: String,
        model: String,
        db: Option<String>,
    },
    Approve {
        run_id: String,
        db: Option<String>,
    },
    Reject {
        run_id: String,
        feedback: Option<String>,
        db: Option<String>,
    },
}

/// Whether a token is a negative number (argparse treats those as values,
/// never as options, when no option looks like a negative number).
fn looks_like_negative_number(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('-') else {
        return false;
    };
    !rest.is_empty()
        && rest.parse::<f64>().is_ok()
        && rest
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
}

/// An option-like token: a dash prefix that is not a bare `-` and not a
/// negative number.
fn is_option_like(token: &str) -> bool {
    token.starts_with('-') && token.len() > 1 && !looks_like_negative_number(token)
}

/// Resolve a `--long` token against `longs` the way argparse does: an exact
/// match wins, else a unique prefix; an ambiguous prefix is an error and no
/// match at all is `None`.
fn match_long(
    token: &str,
    longs: &[&'static str],
    usage: &str,
    prog: &str,
) -> Result<Option<&'static str>, CliExit> {
    if let Some(exact) = longs.iter().find(|l| **l == token) {
        return Ok(Some(exact));
    }
    let candidates: Vec<&'static str> = longs
        .iter()
        .copied()
        .filter(|l| l.starts_with(token))
        .collect();
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(Some(candidates[0])),
        _ => Err(CliExit::error(
            usage,
            prog,
            format!(
                "ambiguous option: {token} could match {}",
                candidates.join(", ")
            ),
        )),
    }
}

/// The raw result of one subcommand parse.
struct SubParsed {
    positionals: Vec<String>,
    /// `(long, value)` in argv order; flags carry `None`.
    values: Vec<(&'static str, Option<String>)>,
    extras: Vec<String>,
}

/// Parse `tokens` (everything after the verb) as argparse's subparser would.
fn parse_sub(spec: &SubSpec, tokens: &[String]) -> Result<SubParsed, CliExit> {
    let prog = format!("{PROG} {}", spec.name);
    let longs: Vec<&'static str> = std::iter::once("--help")
        .chain(spec.options.iter().map(|(long, _)| *long))
        .collect();
    let mut parsed = SubParsed {
        positionals: Vec::new(),
        values: Vec::new(),
        extras: Vec::new(),
    };
    let mut index = 0;
    let mut only_positionals = false;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        index += 1;
        if only_positionals || !is_option_like(token) {
            parsed.positionals.push(token.to_owned());
            continue;
        }
        if token == "--" {
            only_positionals = true;
            continue;
        }
        if token == "-h" {
            return Err(CliExit::Help(spec.help.to_owned()));
        }
        if !token.starts_with("--") {
            parsed.extras.push(token.to_owned());
            continue;
        }
        let (flag, inline) = match token.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (token, None),
        };
        let Some(long) = match_long(flag, &longs, spec.usage, &prog)? else {
            parsed.extras.push(token.to_owned());
            continue;
        };
        if long == "--help" {
            return Err(CliExit::Help(spec.help.to_owned()));
        }
        let takes_value = spec
            .options
            .iter()
            .find(|(l, _)| *l == long)
            .map(|(_, takes)| *takes)
            .unwrap_or(false);
        if !takes_value {
            if let Some(inline) = inline {
                return Err(CliExit::error(
                    spec.usage,
                    &prog,
                    format!("argument {long}: ignored explicit argument '{inline}'"),
                ));
            }
            parsed.values.push((long, None));
            continue;
        }
        let value = match inline {
            Some(inline) => inline,
            None => {
                let next = tokens.get(index).filter(|t| !is_option_like(t));
                match next {
                    Some(value) => {
                        index += 1;
                        value.clone()
                    }
                    None => {
                        return Err(CliExit::error(
                            spec.usage,
                            &prog,
                            format!("argument {long}: expected one argument"),
                        ));
                    }
                }
            }
        };
        parsed.values.push((long, Some(value)));
    }
    if parsed.positionals.len() < spec.positionals.len() {
        let missing: Vec<&str> = spec.positionals[parsed.positionals.len()..].to_vec();
        return Err(CliExit::error(
            spec.usage,
            &prog,
            format!(
                "the following arguments are required: {}",
                missing.join(", ")
            ),
        ));
    }
    let surplus = parsed.positionals.split_off(spec.positionals.len());
    parsed.extras.extend(surplus);
    Ok(parsed)
}

/// Python `int()` of an option value (argparse `type=int`).
fn parse_int(spec: &SubSpec, long: &str, raw: &str) -> Result<u32, CliExit> {
    let text = raw.trim();
    let digits = text.strip_prefix('+').unwrap_or(text);
    let parsed = if digits.is_empty() || digits.starts_with('-') {
        None
    } else {
        digits.replace('_', "").parse::<u32>().ok()
    };
    parsed.ok_or_else(|| {
        CliExit::error(
            spec.usage,
            &format!("{PROG} {}", spec.name),
            format!("argument {long}: invalid int value: '{raw}'"),
        )
    })
}

fn last_value(parsed: &SubParsed, long: &str) -> Option<String> {
    parsed
        .values
        .iter()
        .rev()
        .find(|(l, _)| *l == long)
        .and_then(|(_, v)| v.clone())
}

fn all_values(parsed: &SubParsed, long: &str) -> Vec<String> {
    parsed
        .values
        .iter()
        .filter(|(l, _)| *l == long)
        .filter_map(|(_, v)| v.clone())
        .collect()
}

fn parse_run(tokens: &[String]) -> Result<(Command, Vec<String>), CliExit> {
    let parsed = parse_sub(&RUN_SPEC, tokens)?;
    let prog = format!("{PROG} run");
    // The mutually exclusive --json | --quiet group: argparse rejects the
    // second member the moment it is seen, naming the first.
    let mut seen_exclusive: Option<&str> = None;
    for (long, _) in &parsed.values {
        if *long == "--json" || *long == "--quiet" {
            if let Some(other) = seen_exclusive
                && other != *long
            {
                return Err(CliExit::error(
                    RUN_USAGE,
                    &prog,
                    format!("argument {long}: not allowed with argument {other}"),
                ));
            }
            seen_exclusive = Some(long);
        }
    }
    let max_turns = match last_value(&parsed, "--max-turns") {
        Some(raw) => parse_int(&RUN_SPEC, "--max-turns", &raw)?,
        None => DEFAULT_MAX_TURNS,
    };
    let max_retries = match last_value(&parsed, "--max-retries") {
        Some(raw) => parse_int(&RUN_SPEC, "--max-retries", &raw)?,
        None => DEFAULT_MAX_RETRIES,
    };
    let args = RunArgs {
        target: parsed.positionals[0].clone(),
        db: last_value(&parsed, "--db"),
        check: all_values(&parsed, "--check"),
        rubric: all_values(&parsed, "--rubric"),
        sandbox: last_value(&parsed, "--sandbox"),
        model: last_value(&parsed, "--model"),
        max_turns,
        max_retries,
        json: parsed.values.iter().any(|(l, _)| *l == "--json"),
        quiet: parsed.values.iter().any(|(l, _)| *l == "--quiet"),
    };
    Ok((Command::Run(args), parsed.extras))
}

/// Parse `argv` (without the program name) into one core verb exactly as the
/// argparse parser would, including help and usage-error exits.
pub fn parse_args(argv: &[String]) -> Result<Command, CliExit> {
    let mut top_extras: Vec<String> = Vec::new();
    let mut index = 0;
    let mut command: Option<&str> = None;
    while index < argv.len() {
        let token = argv[index].as_str();
        index += 1;
        if is_option_like(token) && token != "--" {
            if token == "-h" {
                return Err(CliExit::Help(MAIN_HELP.to_owned()));
            }
            if token.starts_with("--") {
                let flag = token.split_once('=').map_or(token, |(flag, _)| flag);
                if match_long(flag, &["--help"], MAIN_USAGE, PROG)?.is_some() {
                    return Err(CliExit::Help(MAIN_HELP.to_owned()));
                }
            }
            top_extras.push(token.to_owned());
            continue;
        }
        if token == "--" {
            continue;
        }
        command = Some(token);
        break;
    }
    let Some(name) = command else {
        return Err(CliExit::error(
            MAIN_USAGE,
            PROG,
            "the following arguments are required: command",
        ));
    };
    if !COMMANDS.contains(&name) {
        return Err(CliExit::error(
            MAIN_USAGE,
            PROG,
            format!(
                "argument command: invalid choice: '{name}' (choose from {})",
                COMMANDS.join(", ")
            ),
        ));
    }
    let rest = &argv[index..];
    let (command, extras) = match name {
        "run" => parse_run(rest)?,
        "is-done" => {
            let parsed = parse_sub(&IS_DONE_SPEC, rest)?;
            (
                Command::IsDone {
                    task_file: parsed.positionals[0].clone(),
                    db: last_value(&parsed, "--db"),
                },
                parsed.extras,
            )
        }
        "interrupt" => {
            let parsed = parse_sub(&INTERRUPT_SPEC, rest)?;
            (
                Command::Interrupt {
                    run_id: parsed.positionals[0].clone(),
                    db: last_value(&parsed, "--db"),
                },
                parsed.extras,
            )
        }
        "steer" => {
            let parsed = parse_sub(&STEER_SPEC, rest)?;
            (
                Command::Steer {
                    run_id: parsed.positionals[0].clone(),
                    message: parsed.positionals[1].clone(),
                    db: last_value(&parsed, "--db"),
                },
                parsed.extras,
            )
        }
        "set-model" => {
            let parsed = parse_sub(&SET_MODEL_SPEC, rest)?;
            (
                Command::SetModel {
                    run_id: parsed.positionals[0].clone(),
                    model: parsed.positionals[1].clone(),
                    db: last_value(&parsed, "--db"),
                },
                parsed.extras,
            )
        }
        "approve" => {
            let parsed = parse_sub(&APPROVE_SPEC, rest)?;
            (
                Command::Approve {
                    run_id: parsed.positionals[0].clone(),
                    db: last_value(&parsed, "--db"),
                },
                parsed.extras,
            )
        }
        _ => {
            let parsed = parse_sub(&REJECT_SPEC, rest)?;
            (
                Command::Reject {
                    run_id: parsed.positionals[0].clone(),
                    feedback: last_value(&parsed, "--feedback"),
                    db: last_value(&parsed, "--db"),
                },
                parsed.extras,
            )
        }
    };
    top_extras.extend(extras);
    if !top_extras.is_empty() {
        return Err(CliExit::error(
            MAIN_USAGE,
            PROG,
            format!("unrecognized arguments: {}", top_extras.join(" ")),
        ));
    }
    Ok(command)
}

/// `--db` or the default database path.
pub fn resolve_db(arg: Option<&str>) -> PathBuf {
    match arg {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(DEFAULT_DB_PATH),
    }
}

/// Map `run` output flags to an events mode (default: readable).
pub fn resolve_events_mode(json: bool, quiet: bool) -> EventsMode {
    if quiet {
        EventsMode::None
    } else if json {
        EventsMode::Json
    } else {
        EventsMode::Plain
    }
}

fn exit_code_for(result: Result<HarnessOutcome, WorkflowError>) -> i32 {
    match result {
        Ok(outcome) => {
            if outcome.lifecycle.status == Status::Done {
                0
            } else {
                1
            }
        }
        Err(WorkflowError::TaskLoad(err)) => {
            eprintln!("error: {err}");
            2
        }
        Err(err) if err.is_interrupted() => 1,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}

fn cmd_run(args: RunArgs) -> i32 {
    let db_path = resolve_db(args.db.as_deref());
    let sandbox = match &args.sandbox {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(err) => {
                eprintln!("error: {err}");
                return 1;
            }
        },
    };
    let events = resolve_events_mode(args.json, args.quiet);
    let inline_graders = !args.check.is_empty() || !args.rubric.is_empty();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: {err}");
            return 1;
        }
    };
    let mut options = RunTaskOptions::default();
    options.agent.model = args.model.clone();
    options.agent.max_turns = args.max_turns;
    options.max_retries = Some(args.max_retries);
    options.events = events;

    // File vs inline goal: an existing file is loaded as a task (which
    // carries its own graders); anything else is treated as an inline goal.
    let target = Path::new(&args.target);
    if target.is_file() {
        if inline_graders {
            eprintln!(
                "error: --check/--rubric apply to an inline goal; a task file declares its own \
                 graders"
            );
            return 2;
        }
        let result = runtime.block_on(run_task_file(target, &db_path, &sandbox, options));
        return exit_code_for(result);
    }
    let task = match build_inline_task(&args.target, &args.check, &args.rubric, None) {
        Ok(task) => task,
        Err(err) => {
            eprintln!("error: {err}");
            return 2;
        }
    };
    options.source = Some("(inline goal)".to_owned());
    let result = runtime.block_on(run_task_object(&task, &db_path, &sandbox, options));
    exit_code_for(result)
}

fn cmd_is_done(task_file: &str, db: Option<&str>) -> i32 {
    let task = match load_task_file(task_file) {
        Ok(task) => task,
        Err(err) => {
            eprintln!("error: {err}");
            return 2;
        }
    };
    let db_path = resolve_db(db);
    let outcome = SqliteStore::open(&db_path).and_then(|store| {
        let done = has_done_lifecycle(&store, &task.id);
        store.close()?;
        done
    });
    match outcome {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}

/// Dispatch one parsed verb; `store` injects the backend the producer verbs
/// (interrupt / steer / set-model / approve / reject) enqueue into.
pub fn dispatch(command: Command, store: Option<Arc<dyn RunStore>>) -> i32 {
    match command {
        Command::Run(args) => cmd_run(args),
        Command::IsDone { task_file, db } => cmd_is_done(&task_file, db.as_deref()),
        Command::Interrupt { run_id, db } => enqueue_control_command(
            &resolve_db(db.as_deref()),
            &run_id,
            CONTROL_COMMAND_INTERRUPT,
            JsonObject::new(),
            store,
        ),
        Command::Steer {
            run_id,
            message,
            db,
        } => {
            if message.is_empty() {
                eprintln!("error: steer message must be non-empty");
                return 2;
            }
            let mut payload = JsonObject::new();
            payload.insert("text".into(), json!(message));
            enqueue_control_command(
                &resolve_db(db.as_deref()),
                &run_id,
                CONTROL_COMMAND_SAY,
                payload,
                store,
            )
        }
        Command::SetModel { run_id, model, db } => {
            let mut payload = JsonObject::new();
            payload.insert("model".into(), json!(model));
            enqueue_control_command(
                &resolve_db(db.as_deref()),
                &run_id,
                CONTROL_COMMAND_SET_MODEL,
                payload,
                store,
            )
        }
        Command::Approve { run_id, db } => enqueue_control_command(
            &resolve_db(db.as_deref()),
            &run_id,
            CONTROL_COMMAND_APPROVE,
            JsonObject::new(),
            store,
        ),
        Command::Reject {
            run_id,
            feedback,
            db,
        } => {
            let mut payload = JsonObject::new();
            if let Some(feedback) = feedback {
                payload.insert("feedback".into(), json!(feedback));
            }
            enqueue_control_command(
                &resolve_db(db.as_deref()),
                &run_id,
                CONTROL_COMMAND_REJECT,
                payload,
                store,
            )
        }
    }
}

/// Parse `argv` (without the program name) and dispatch one core verb,
/// returning the process exit code.
///
/// `store` injects the persistence backend the producer verbs enqueue their
/// control command into. `None` (the default, and the only value the bare
/// command line ever passes) keeps every verb sqlite-pinned; the product
/// shell passes a policy-selected store so a postgres backend lands the
/// command in postgres rather than a stray sqlite file.
pub fn main_with_store(argv: &[String], store: Option<Arc<dyn RunStore>>) -> i32 {
    match parse_args(argv) {
        Ok(command) => dispatch(command, store),
        Err(exit) => exit.report(),
    }
}

/// [`main_with_store`] with the sqlite default.
pub fn main(argv: &[String]) -> i32 {
    main_with_store(argv, None)
}
