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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_invoke::InvocationRequest;
    use crate::agents_invoke::test_support::{SCENARIO_VAR, write_fake_claude};
    use crate::envelope::{EnvelopeResult, Intent, ValidEnvelope};
    use crate::events::DomainEventKind;
    use crate::invoker::{InvocationSignals, IterationResult};
    use crate::lifecycle::{Attempt, Outcome};
    use crate::store::{AttemptStore, DomainEventStore, GraderResultStore, TaskStore};
    use crate::store_memory::InMemoryStore;
    use crate::task::{GraderType, Seconds, TaskBudgets};
    use chrono::TimeZone;
    use serde_json::Map;
    use std::sync::Mutex;
    use std::time::Instant;

    // --- Fixtures ------------------------------------------------------------

    #[derive(Default)]
    struct ListSink {
        records: Mutex<Vec<TelemetryRecord>>,
    }

    impl ListSink {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn kinds(&self) -> Vec<String> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.kind.clone())
                .collect()
        }
    }

    impl TelemetrySink for ListSink {
        fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError> {
            self.records.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    struct FailingSink;

    impl TelemetrySink for FailingSink {
        fn append_telemetry(&self, _record: &TelemetryRecord) -> Result<(), StoreError> {
            Err(StoreError::backend(std::io::Error::other("sink down")))
        }
    }

    type Lines = Arc<Mutex<Vec<String>>>;

    fn capture() -> (LineSink, Lines) {
        let lines: Lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let line_sink: LineSink =
            Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_owned()));
        (line_sink, lines)
    }

    fn lines_of(lines: &Lines) -> Vec<String> {
        lines.lock().unwrap().clone()
    }

    fn verify_result() -> IterationResult {
        let transcript = format!(
            "{}\n{{\"intent\": \"verify\", \"reason\": \"done\"}}\n{}",
            crate::envelope::OPENING_FENCE,
            crate::envelope::CLOSING_FENCE
        );
        IterationResult {
            transcript,
            messages: vec![
                json!({"type": "message.assistant", "payload": {"text": "ok"}}),
                json!({"type": "session.finished", "payload": {"subtype": "success", "num_turns": 1, "total_cost_usd": 0.01}}),
            ],
            envelope: EnvelopeResult::Valid(ValidEnvelope {
                intent: Intent::Verify,
                reason: Some("done".into()),
                requires: Vec::new(),
            }),
            signals: InvocationSignals {
                stop_reason: Some("end_turn".into()),
                num_turns: Some(1),
                total_cost_usd: Some(0.01),
                result_is_error: Some(false),
                result_subtype: Some("success".into()),
                session_id: Some("sess-1".into()),
                ..InvocationSignals::default()
            },
            failure: None,
            usage: None,
        }
    }

    /// A one-shot invoker returning a `verify` iteration; honors the
    /// per-message observer contract exactly as the production invoker does.
    fn verify_invoke() -> InvokeFn {
        Arc::new(|request: InvocationRequest| {
            Box::pin(async move {
                let result = verify_result();
                if let Some(observer) = &request.observers.on_message {
                    for msg in &result.messages {
                        let _ = catch_unwind(AssertUnwindSafe(|| observer(msg)));
                    }
                }
                result
            })
        })
    }

    /// An invoker that hangs forever, so only a deadline or a shutdown can
    /// end it.
    fn never_returns() -> InvokeFn {
        Arc::new(|_request: InvocationRequest| {
            Box::pin(async move {
                std::future::pending::<()>().await;
                unreachable!("the pending future never resolves")
            })
        })
    }

    fn task(goal: &str) -> Task {
        Task::new(goal, Vec::new())
    }

    fn memory_store() -> Arc<InMemoryStore> {
        Arc::new(InMemoryStore::new())
    }

    fn seed_open(store: &InMemoryStore, task_id: &str, run_id: &str, status: Status) {
        let now = Utc::now();
        let mut lc = Lifecycle::new(task_id);
        lc.run_id = run_id.to_owned();
        lc.transition_to(Status::Ready, "", Some(now)).unwrap();
        if status != Status::Ready {
            lc.transition_to(Status::Running, "", Some(now)).unwrap();
        }
        if status == Status::Validating {
            lc.transition_to(Status::Validating, "", Some(now)).unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        if matches!(status, Status::Running | Status::Validating) {
            store
                .save_attempt(run_id, &Attempt::new(1, now, run_id), None)
                .unwrap();
        }
    }

    fn base_options(store: &Arc<InMemoryStore>, sink: &Arc<ListSink>) -> RunTaskOptions {
        let store: Arc<dyn RunStore> = store.clone();
        let sink: SharedSink = sink.clone();
        RunTaskOptions::default()
            .with_invoke(verify_invoke())
            .with_store(store)
            .with_sink(sink)
            .with_stream(Arc::new(|_line: &str| {}))
    }

    fn ts_micro() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap() + chrono::Duration::microseconds(123456)
    }

    fn ts_whole() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap()
    }

    fn record(
        kind: &str,
        ts: DateTime<Utc>,
        payload: Value,
        attempt: Option<u32>,
        iteration: Option<u32>,
    ) -> TelemetryRecord {
        let mut record = TelemetryRecord::new("run-1", ts, kind);
        record.payload = payload.as_object().cloned().unwrap_or_default();
        record.attempt_number = attempt;
        record.iteration_number = iteration;
        record
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    // --- Formatting (fixtures generated from the Python module) -----------------

    #[test]
    fn short_matches_python() {
        assert_eq!(short("hello", 60), "hello");
        assert_eq!(
            short(&"a".repeat(61), 60),
            format!("{}\u{2026}", "a".repeat(59))
        );
        assert_eq!(short(&"a".repeat(60), 60), "a".repeat(60));
        assert_eq!(short("line1\nline2\rx", 60), "line1 line2 x");
        assert_eq!(short("x", 1), "x");
        assert_eq!(short("xy", 1), "x\u{2026}");
        assert_eq!(
            short(&"h\u{e9}llo w\u{f6}rld ".repeat(10), 40),
            "h\u{e9}llo w\u{f6}rld h\u{e9}llo w\u{f6}rld h\u{e9}llo w\u{f6}rld h\u{e9}l\u{2026}"
        );
    }

    #[test]
    fn py_str_matches_python_str() {
        assert_eq!(py_str(&Value::Null), "None");
        assert_eq!(py_str(&json!(3.5)), "3.5");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(1.0)), "1.0");
        assert_eq!(py_str(&json!([1, "a"])), "[1, 'a']");
        assert_eq!(py_str(&json!({"k": "v"})), "{'k': 'v'}");
        assert_eq!(py_str(&json!("it's")), "it's");
        assert_eq!(py_repr(&json!("it's")), "\"it's\"");
        assert_eq!(py_repr(&json!("a\nb")), "'a\\nb'");
    }

    #[test]
    fn payload_summary_matches_python() {
        assert_eq!(event_payload_summary(&Map::new(), 100), "");
        let payload = json!({
            "a": 1, "b": "two\nlines", "c": "x".repeat(45), "d": null, "e": 1.0,
            "f": [1, 2], "g": {"k": "v"}, "h": true,
        });
        assert_eq!(
            event_payload_summary(payload.as_object().unwrap(), 100),
            format!(
                "a=1 b=two lines c={}\u{2026} d=None e=1.0 f=[1, 2] g={{'k': 'v'}} h=True",
                "x".repeat(39)
            )
        );
        let mut wide = Map::new();
        for i in 0..6 {
            wide.insert(format!("k{i}"), json!("v".repeat(30)));
        }
        assert_eq!(
            event_payload_summary(&wide, 100),
            format!(
                "k0={} k1={} k2={}\u{2026}",
                "v".repeat(30),
                "v".repeat(30),
                "v".repeat(28)
            )
        );
    }

    #[test]
    fn event_lines_match_python() {
        let r1 = record(
            "harness.attempt_started",
            ts_micro(),
            json!({"number": 1, "model_id": "claude-x"}),
            Some(1),
            None,
        );
        assert_eq!(
            format_event_line(&r1),
            "[03:04:05] harness.attempt_started attempt=1  number=1 model_id=claude-x"
        );
        assert_eq!(
            event_json_line(&r1),
            "{\"attempt_number\": 1, \"iteration_number\": null, \"kind\": \"harness.attempt_started\", \"payload\": {\"model_id\": \"claude-x\", \"number\": 1}, \"run_id\": \"run-1\", \"ts\": \"2024-01-02T03:04:05.123456+00:00\"}"
        );
        let r2 = record(
            "harness.lifecycle_initialized",
            ts_whole(),
            json!({}),
            None,
            None,
        );
        assert_eq!(
            format_event_line(&r2),
            "[03:04:05] harness.lifecycle_initialized"
        );
        assert_eq!(
            event_json_line(&r2),
            "{\"attempt_number\": null, \"iteration_number\": null, \"kind\": \"harness.lifecycle_initialized\", \"payload\": {}, \"run_id\": \"run-1\", \"ts\": \"2024-01-02T03:04:05+00:00\"}"
        );
        let r3 = record(
            "harness.z",
            ts_micro(),
            json!({"text": "h\u{e9}llo\nw\u{f6}rld", "f": 1.5, "n": null, "l": [1, "a"], "o": {"b": 1, "a": 2}}),
            Some(2),
            Some(3),
        );
        assert_eq!(
            format_event_line(&r3),
            "[03:04:05] harness.z attempt=2  text=h\u{e9}llo w\u{f6}rld f=1.5 n=None l=[1, 'a'] o={'b': 1, 'a': 2}"
        );
        assert_eq!(
            event_json_line(&r3),
            "{\"attempt_number\": 2, \"iteration_number\": 3, \"kind\": \"harness.z\", \"payload\": {\"f\": 1.5, \"l\": [1, \"a\"], \"n\": null, \"o\": {\"a\": 2, \"b\": 1}, \"text\": \"h\\u00e9llo\\nw\\u00f6rld\"}, \"run_id\": \"run-1\", \"ts\": \"2024-01-02T03:04:05.123456+00:00\"}"
        );
    }

    #[test]
    fn tool_use_matches_python() {
        let input = json!({"file_path": "/tmp/x.py", "content": "a".repeat(50), "c": 1, "d": 2});
        assert_eq!(
            format_tool_use("Write", input.as_object().unwrap()),
            format!(
                "Write(file_path=/tmp/x.py, content={}\u{2026}, c=1)",
                "a".repeat(39)
            )
        );
        assert_eq!(format_tool_use("Bash", &Map::new()), "Bash()");
        let edit = json!({"old": "x\ny"});
        assert_eq!(
            format_tool_use("Edit", edit.as_object().unwrap()),
            "Edit(old=x y)"
        );
    }

    #[test]
    fn live_message_summaries() {
        let cases: Vec<(Value, (&str, String))> = vec![
            (
                json!({"type": "message.assistant", "payload": {"text": "  hello\nworld  "}}),
                ("ASSISTANT", "hello world".into()),
            ),
            (
                json!({"type": "message.assistant", "payload": {"text": "   "}}),
                ("ASSISTANT", "(no content)".into()),
            ),
            (
                json!({"type": "tool.started", "payload": {"tool_use_id": "t1", "tool_name": "Write", "tool_input": {"file_path": "/a"}}}),
                ("ASSISTANT", "Write(file_path=/a)".into()),
            ),
            (
                json!({"type": "tool.finished", "payload": {"tool_use_id": "t1", "is_error": false, "content": "abcd"}}),
                ("USER", "tool_result(4B)".into()),
            ),
            (
                json!({"type": "tool.finished", "payload": {"tool_use_id": "t1", "is_error": true, "content": null}}),
                ("USER", "tool_result(0B ERR)".into()),
            ),
            (
                json!({"type": "tool.finished", "payload": {"tool_use_id": "t1", "is_error": null, "content": [{"type": "text", "text": "x"}]}}),
                ("USER", "tool_result(31B)".into()),
            ),
            (
                json!({"type": "message.user", "payload": {"text": "echo"}}),
                ("USER", "echo".into()),
            ),
            (
                json!({"type": "message.user", "payload": {}}),
                ("USER", "(empty)".into()),
            ),
            (
                json!({"type": "session.finished", "payload": {"subtype": "success", "num_turns": 3, "total_cost_usd": 0.0123456}}),
                ("RESULT", "subtype=success turns=3 cost=$0.0123".into()),
            ),
            (
                json!({"type": "session.finished", "payload": {"subtype": null, "num_turns": null, "total_cost_usd": null}}),
                ("RESULT", "subtype=None turns=None".into()),
            ),
            (
                json!({"type": "session.started", "payload": {"session_id": "s", "model": "m"}}),
                ("SYSTEM", "init".into()),
            ),
            (
                json!({"type": "context.usage", "payload": {"usage": {}}}),
                ("CONTEXT.USAGE", String::new()),
            ),
        ];
        for (msg, (label, detail)) in cases {
            assert_eq!(
                summarize_live_message(&msg),
                (label.to_owned(), detail),
                "{msg}"
            );
        }
        let line = format_live_message(
            &json!({"type": "message.assistant", "payload": {"text": "hi"}}),
            ts_whole(),
        );
        assert_eq!(line, "[03:04:05] ASSISTANT  hi");
        let bare = format_live_message(&json!({"type": "warning", "payload": {}}), ts_whole());
        assert_eq!(bare, "[03:04:05] WARNING");
    }

    #[test]
    fn message_json_line_sorts_keys_like_python() {
        let msg =
            json!({"type": "message.assistant", "payload": {"text": "h\u{e9}"}, "sequence": 2});
        assert_eq!(
            message_json_line(&msg),
            "{\"payload\": {\"text\": \"h\\u00e9\"}, \"sequence\": 2, \"type\": \"message.assistant\"}"
        );
    }

    #[test]
    fn events_mode_names_round_trip() {
        for mode in [EventsMode::Plain, EventsMode::Json, EventsMode::None] {
            assert_eq!(EventsMode::from_name(mode.as_str()), Some(mode));
            assert_eq!(mode.to_string(), mode.as_str());
        }
        assert_eq!(EventsMode::from_name("loud"), None);
        assert_eq!(resolve_events_mode(false, false), EventsMode::Plain);
        assert_eq!(resolve_events_mode(true, false), EventsMode::Json);
        assert_eq!(resolve_events_mode(true, true), EventsMode::None);
        assert_eq!(EventsMode::default(), EventsMode::None);
    }

    #[test]
    fn observers_and_emitters_follow_the_mode() {
        let (out, lines) = capture();
        assert!(make_message_observer(EventsMode::None, out.clone()).is_none());
        assert!(make_event_emitter(EventsMode::None, out.clone()).is_none());

        let msg = json!({"type": "message.assistant", "payload": {"text": "hi"}});
        make_message_observer(EventsMode::Plain, out.clone()).unwrap()(&msg);
        make_message_observer(EventsMode::Json, out.clone()).unwrap()(&msg);
        let rec = record("harness.x", ts_whole(), json!({"a": 1}), None, None);
        make_event_emitter(EventsMode::Plain, out.clone()).unwrap()(&rec);
        make_event_emitter(EventsMode::Json, out).unwrap()(&rec);

        let lines = lines_of(&lines);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].ends_with("] ASSISTANT  hi"), "{}", lines[0]);
        assert_eq!(lines[1], message_json_line(&msg));
        assert_eq!(lines[2], "[03:04:05] harness.x  a=1");
        assert_eq!(lines[3], event_json_line(&rec));
    }

    #[test]
    fn streaming_sink_appends_first_and_emits_only_harness_records() {
        let wrapped = ListSink::new();
        let (out, lines) = capture();
        let sink = StreamingTelemetrySink::new(
            wrapped.clone(),
            make_event_emitter(EventsMode::Plain, out).unwrap(),
        );
        for kind in [
            "harness.a",
            "message.assistant",
            "domain.transitioned_to",
            "harness.b",
        ] {
            sink.append_telemetry(&record(kind, ts_whole(), json!({}), None, None))
                .unwrap();
        }
        assert_eq!(
            wrapped.kinds(),
            strings(&[
                "harness.a",
                "message.assistant",
                "domain.transitioned_to",
                "harness.b"
            ])
        );
        assert_eq!(
            lines_of(&lines),
            strings(&["[03:04:05] harness.a", "[03:04:05] harness.b"])
        );

        let (out, lines) = capture();
        let failing = StreamingTelemetrySink::new(
            Arc::new(FailingSink),
            make_event_emitter(EventsMode::Plain, out).unwrap(),
        );
        let err = failing
            .append_telemetry(&record("harness.a", ts_whole(), json!({}), None, None))
            .unwrap_err();
        assert!(err.to_string().contains("sink down"));
        assert!(lines_of(&lines).is_empty(), "a failed append must not emit");
    }

    // --- Inline tasks and observers ----------------------------------------------

    #[test]
    fn build_inline_task_shapes_graders() {
        let task = build_inline_task(
            "do it",
            &strings(&["cargo test", "cargo clippy"]),
            &strings(&["it works", "it is tested"]),
            Some("inline-1"),
        )
        .unwrap();
        assert_eq!(task.id, "inline-1");
        assert_eq!(task.goal, "do it");
        assert_eq!(task.graders.len(), 3);
        match (&task.graders[0], &task.graders[1], &task.graders[2]) {
            (Grader::Command(a), Grader::Command(b), Grader::Rubric(r)) => {
                assert_eq!(
                    (a.run.as_str(), a.name.as_deref()),
                    ("cargo test", Some("check-1"))
                );
                assert_eq!(
                    (b.run.as_str(), b.name.as_deref()),
                    ("cargo clippy", Some("check-2"))
                );
                assert_eq!(r.assertions, strings(&["it works", "it is tested"]));
            }
            other => panic!("unexpected graders: {other:?}"),
        }
        let bare = build_inline_task("goal only", &[], &[], None).unwrap();
        assert!(bare.graders.is_empty());
        assert!(!bare.id.is_empty());
        assert!(build_inline_task("   ", &[], &[], None).is_err());
        assert!(build_inline_task("g", &strings(&[""]), &[], None).is_err());
    }

    #[test]
    fn compose_message_observers_filters_and_contains_panics() {
        assert!(compose_message_observers([None, None]).is_none());
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let record_as = |tag: &'static str| -> MessageObserver {
            let seen = seen.clone();
            Arc::new(move |msg: &Value| {
                seen.lock().unwrap().push(format!("{tag}:{}", msg["n"]));
            })
        };
        let single = record_as("solo");
        let composed = compose_message_observers([None, Some(single.clone())]).unwrap();
        assert!(
            Arc::ptr_eq(&composed, &single),
            "a lone observer is returned unwrapped"
        );

        let panicking: MessageObserver = Arc::new(|_msg: &Value| panic!("renderer broke"));
        let composed = compose_message_observers([
            Some(record_as("a")),
            Some(panicking),
            Some(record_as("b")),
        ])
        .unwrap();
        composed(&json!({"n": 1}));
        composed(&json!({"n": 2}));
        assert_eq!(
            seen.lock().unwrap().clone(),
            strings(&["a:1", "b:1", "a:2", "b:2"])
        );
    }

    // --- Agent options -------------------------------------------------------------

    #[test]
    fn permission_vocabulary_mapping_never_defaults_to_bypass() {
        let table: BTreeMap<&str, &str> = PERMISSION_POLICIES.into_iter().collect();
        assert_eq!(table["bypassPermissions"], "auto");
        assert_eq!(table["plan"], "plan");
        assert_eq!(table["default"], "supervised");
        assert_eq!(table["acceptEdits"], "supervised");
        for (mode, policy) in PERMISSION_POLICIES {
            assert_eq!(permission_policy_for(mode).as_str(), policy);
        }
        assert_eq!(
            permission_policy_for("anythingElse"),
            PermissionPolicy::Supervised
        );
    }

    #[test]
    fn fast_primitives_reproduce_the_bare_configuration() {
        let configuration = build_agent_configuration("claude-code", &AgentOptions::default());
        assert_eq!(configuration.agent_id, "claude-code");
        assert_eq!(configuration.model_id, None);
        assert_eq!(configuration.max_turns, Some(500));
        assert_eq!(configuration.permission_policy, PermissionPolicy::Auto);
        assert!(configuration.environment.is_empty());
        assert_eq!(configuration.command_override, None);
        let keys: Vec<&String> = configuration.adapter_options.keys().collect();
        assert_eq!(keys, vec!["skills"]);
        assert_eq!(configuration.adapter_options["skills"], json!("all"));
        // Spec 00063: the co-author suppression layer is always on.
        let settings =
            flywheel_agents::claude_code::cli::build_settings_value(&configuration).unwrap();
        let settings: Value = serde_json::from_str(&settings.unwrap()).unwrap();
        assert_eq!(settings["includeCoAuthoredBy"], json!(false));
        assert!(settings.get("sandbox").is_none());
    }

    #[test]
    fn changed_primitives_change_the_configuration() {
        let options = AgentOptions {
            model: Some("claude-sonnet-4-5".into()),
            max_turns: 7,
            permission_mode: "default".into(),
            skills: Skills::Named(strings(&["fw-spec"])),
            allowed_tools: strings(&["Bash", "Edit"]),
            denied_tools: strings(&["WebFetch"]),
            setting_sources: Some(strings(&["project"])),
            mcp_servers: strings(&["serena"]),
            mcp_strict: true,
            exec_enabled: true,
            exec_auto_allow: false,
            agent_env: BTreeMap::from([("FOO".to_owned(), "bar".to_owned())]),
            transport: Some("cli".into()),
            command_override: Some(strings(&["/bin/true"])),
        };
        let configuration = build_agent_configuration("claude-code", &options);
        assert_eq!(configuration.model_id.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(configuration.max_turns, Some(7));
        assert_eq!(
            configuration.permission_policy,
            PermissionPolicy::Supervised
        );
        assert_eq!(configuration.environment["FOO"], "bar");
        assert_eq!(
            configuration.command_override,
            Some(strings(&["/bin/true"]))
        );
        let adapter = &configuration.adapter_options;
        assert_eq!(adapter["skills"], json!(["fw-spec"]));
        assert_eq!(adapter["allowed_tools"], json!(["Bash", "Edit"]));
        assert_eq!(adapter["disallowed_tools"], json!(["WebFetch"]));
        assert_eq!(adapter["setting_sources"], json!(["project"]));
        assert_eq!(adapter["mcp_servers"], json!(["serena"]));
        assert_eq!(adapter["mcp_strict"], json!(true));
        assert_eq!(
            adapter["sandbox_exec"],
            json!({"enabled": true, "autoAllowBashIfSandboxed": false})
        );
        assert_eq!(adapter["transport"], json!("cli"));
        let settings =
            flywheel_agents::claude_code::cli::build_settings_value(&configuration).unwrap();
        let settings: Value = serde_json::from_str(&settings.unwrap()).unwrap();
        assert_eq!(
            settings["sandbox"]["autoAllowBashIfSandboxed"],
            json!(false)
        );
    }

    // --- Stranded recovery -----------------------------------------------------------

    #[test]
    fn stranded_run_ids_are_filtered_and_oldest_first() {
        let store = memory_store();
        seed_open(&store, "t1", "run-a", Status::Running);
        seed_open(&store, "t1", "run-b", Status::Validating);
        seed_open(&store, "t1", "run-c", Status::Ready);
        seed_open(&store, "t2", "run-d", Status::Running);

        let listed = store
            .list_lifecycles(&LifecycleFilter::default().with_statuses(STRANDED_STATUSES))
            .unwrap();
        let expected: Vec<String> = listed.into_iter().rev().map(|lc| lc.run_id).collect();
        assert_eq!(expected.len(), 3);
        assert_eq!(stranded_run_ids(store.as_ref(), None).unwrap(), expected);
        assert_eq!(
            stranded_run_ids(store.as_ref(), Some("t2")).unwrap(),
            strings(&["run-d"])
        );
        assert!(
            stranded_run_ids(store.as_ref(), Some("t9"))
                .unwrap()
                .is_empty()
        );
        assert!(!has_done_lifecycle(store.as_ref(), "t1").unwrap());
    }

    #[test]
    fn recover_stranded_lifecycles_finalizes_each_match() {
        let store = memory_store();
        let sink = ListSink::new();
        seed_open(&store, "t1", "run-a", Status::Running);
        seed_open(&store, "t1", "run-b", Status::Validating);
        seed_open(&store, "t1", "run-c", Status::Ready);
        let backend: Arc<dyn HarnessStore> = store.clone();
        let sink_shared: SharedSink = sink.clone();
        let mut finalized =
            recover_stranded_lifecycles(backend.clone(), Some("t1"), Some(sink_shared)).unwrap();
        finalized.sort();
        assert_eq!(finalized, strings(&["run-a", "run-b"]));
        for run_id in ["run-a", "run-b"] {
            let lc = store.load_lifecycle(run_id).unwrap().unwrap();
            assert_eq!(lc.status, Status::Interrupted);
        }
        assert_eq!(
            store.load_lifecycle("run-c").unwrap().unwrap().status,
            Status::Ready
        );
        assert!(sink.kinds().iter().any(|k| k == "harness.crash"));
        assert!(
            recover_stranded_lifecycles(backend, Some("t1"), None)
                .unwrap()
                .is_empty()
        );
    }

    // --- run_task_object ----------------------------------------------------------------

    #[tokio::test]
    async fn injected_store_holds_complete_run_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let db_path = dir.path().join("nested").join("flywheel.sqlite");
        let sandbox = dir.path().join("sandbox");
        let task = Task::new(
            "produce a green run",
            vec![Grader::Command(CommandGrader {
                run: "true".into(),
                name: Some("ok".into()),
            })],
        );

        let outcome = run_task_object(&task, &db_path, &sandbox, base_options(&store, &sink))
            .await
            .unwrap();
        let run_id = outcome.lifecycle.run_id.clone();

        assert!(!db_path.exists(), "no sqlite file when a store is injected");
        assert!(!db_path.parent().unwrap().exists());
        assert!(sandbox.is_dir(), "the sandbox is created");
        assert_eq!(outcome.lifecycle.status, Status::Done);
        let loaded = store.load_lifecycle(&run_id).unwrap().unwrap();
        assert_eq!(loaded.status, Status::Done);
        assert!(!loaded.attempts.is_empty());
        assert_eq!(
            store.load_task_for_run(&run_id).unwrap().unwrap().goal,
            task.goal
        );
        assert!(!store.list_attempts(&run_id).unwrap().is_empty());
        assert!(!store.list_domain_events(&run_id).unwrap().is_empty());
        let grader_rows = store.list_grader_results(&run_id, 1).unwrap();
        assert_eq!(grader_rows.len(), 1);
        assert_eq!(grader_rows[0].grader_type, GraderType::Command);
        assert!(grader_rows.iter().all(|r| r.passed));
        assert!(sink.kinds().iter().any(|k| k == "harness.attempt_started"));
    }

    #[tokio::test]
    async fn injected_store_receives_run_events_stream() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let db_path = dir.path().join("db").join("flywheel.sqlite");
        let outcome = run_task_object(
            &task("unverified but recorded"),
            &db_path,
            &dir.path().join("sandbox"),
            base_options(&store, &sink),
        )
        .await
        .unwrap();
        let events = store.list_domain_events(&outcome.lifecycle.run_id).unwrap();
        assert!(events.len() >= 2);
        assert_eq!(events[0].kind(), DomainEventKind::LifecycleInitialized);
        assert!(!db_path.exists());
    }

    #[tokio::test]
    async fn default_store_and_sink_materialize_under_the_db_directory() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".flywheel").join("flywheel.sqlite");
        let options = RunTaskOptions::default()
            .with_invoke(verify_invoke())
            .with_stream(Arc::new(|_line: &str| {}));
        let outcome = run_task_object(
            &task("persist me"),
            &db_path,
            &dir.path().join("sb"),
            options,
        )
        .await
        .unwrap();
        assert_eq!(outcome.lifecycle.status, Status::Done);
        assert!(db_path.is_file(), "the sqlite store is opened at db_path");
        let logs = FileTelemetrySink::new(dir.path().join(".flywheel").join("logs"));
        let run_file = logs.path_for(&outcome.lifecycle.run_id).unwrap();
        assert!(run_file.is_file(), "one JSONL file per run under logs/runs");
        let text = std::fs::read_to_string(run_file).unwrap();
        assert!(text.contains("harness.attempt_started"));

        let store = SqliteStore::open(&db_path).unwrap();
        assert!(has_done_lifecycle(&store, &outcome.lifecycle.task_id).unwrap());
    }

    #[tokio::test]
    async fn deadlines_override_cancels_a_never_returning_invoke() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let deadlines = DeadlineConfig {
            agent_iteration_seconds: Some(0.05),
            ..DeadlineConfig::default()
        };
        let options = base_options(&store, &sink)
            .with_invoke(never_returns())
            .with_max_retries(0)
            .with_deadlines(deadlines);
        let started = Instant::now();
        let outcome = run_task_object(
            &task("hang forever"),
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            options,
        )
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(outcome.lifecycle.status, Status::Failed);
        assert_eq!(outcome.attempts.len(), 1);
        assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
        assert!(
            outcome.attempts[0].error.contains("deadline"),
            "{}",
            outcome.attempts[0].error
        );
    }

    #[tokio::test]
    async fn task_budget_overrides_the_iteration_ceiling_per_task() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let mut task = task("hang forever");
        task.budgets = TaskBudgets {
            agent_iteration_seconds: Some(Seconds::Float(0.05)),
            ..TaskBudgets::default()
        };
        let options = base_options(&store, &sink)
            .with_invoke(never_returns())
            .with_max_retries(0);
        let started = Instant::now();
        let outcome = run_task_object(
            &task,
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            options,
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the task's own ceiling governs"
        );
        assert_eq!(outcome.lifecycle.status, Status::Failed);
        assert_eq!(outcome.attempts[0].outcome, Some(Outcome::InternalError));
        assert!(outcome.attempts[0].error.contains("deadline"));
    }

    #[test]
    fn harness_config_forwards_budgets_and_preserves_defaults() {
        let sandbox = Path::new("/tmp/sbx");
        let context = BTreeMap::from([("model_id".to_owned(), "m".to_owned())]);
        let overridden = RunTaskOptions::default()
            .with_rubric_judge_max_turns(8)
            .with_deadlines(DeadlineConfig {
                rubric_judge_seconds: Some(123.0),
                ..DeadlineConfig::default()
            });
        let config = harness_config_for(&overridden, sandbox, context.clone(), None, None);
        assert_eq!(config.rubric_judge_max_turns, 8);
        assert_eq!(config.deadlines.rubric_judge_seconds, Some(123.0));
        assert_eq!(config.worktree.as_deref(), Some(sandbox));
        assert_eq!(config.agent_context, context);
        assert_eq!(config.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(config.agent_id, DEFAULT_AGENT_ID);

        let defaults = harness_config_for(&RunTaskOptions::default(), sandbox, context, None, None);
        assert_eq!(defaults.rubric_judge_max_turns, 32);
        assert_eq!(defaults.deadlines, DeadlineConfig::default());
        assert_eq!(defaults.checkpoint_nudge_seconds, 300.0);
        assert!(defaults.rubric_judge_invoke.is_none());
        assert!(defaults.recovery_summarizer_invoke.is_none());

        let mut tuned = RunTaskOptions::default().with_agent_id("codex");
        tuned.max_cost_usd = 2.5;
        tuned.max_tokens = 10;
        tuned.wall_clock_seconds = 60;
        tuned.checkpoint_nudge_seconds = Some(0.0);
        tuned.artifacts_root = Some(PathBuf::from("/tmp/art"));
        tuned.grader_env = Some(BTreeMap::from([("A".to_owned(), "b".to_owned())]));
        let config = harness_config_for(&tuned, sandbox, BTreeMap::new(), None, None);
        assert_eq!(config.max_cost_usd, 2.5);
        assert_eq!(config.max_tokens, 10);
        assert_eq!(config.wall_clock_seconds, 60);
        assert_eq!(config.checkpoint_nudge_seconds, 0.0);
        assert_eq!(
            config.artifacts_root.as_deref(),
            Some(Path::new("/tmp/art"))
        );
        assert_eq!(config.grader_env.unwrap()["A"], "b");
        assert_eq!(config.agent_id, "codex");
    }

    #[tokio::test]
    async fn agent_context_pins_model_runtime_and_base_sha() {
        let dir = tempfile::tempdir().unwrap();
        let plain = build_agent_context(&RunTaskOptions::default(), dir.path()).await;
        assert_eq!(plain["model_id"], "claude-code-default");
        assert_eq!(plain["agent_sdk"], "flywheel_agents");
        assert_eq!(plain["agent_id"], DEFAULT_AGENT_ID);
        assert_eq!(plain["sandbox"], dir.path().display().to_string());
        assert!(
            !plain.contains_key("base_commit_sha"),
            "no pin outside a git checkout"
        );

        let mut options = RunTaskOptions::default().with_agent_id("codex");
        options.agent.model = Some("claude-opus-4-8".into());
        let repo = git_repo_with_commit();
        let pinned = build_agent_context(&options, repo.path()).await;
        assert_eq!(pinned["model_id"], "claude-opus-4-8");
        assert_eq!(pinned["agent_id"], "codex");
        assert_eq!(pinned["base_commit_sha"].len(), 40);
    }

    #[tokio::test]
    async fn diagnostics_and_plain_event_stream() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let (stream, diagnostics) = capture();
        let (events_out, events) = capture();
        let task = task("observe me").with_id("task-obs");
        let options = base_options(&store, &sink)
            .with_stream(stream)
            .with_source("tests/obs.json")
            .with_events(EventsMode::Plain, events_out);
        let outcome = run_task_object(
            &task,
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            options,
        )
        .await
        .unwrap();
        let diagnostics = lines_of(&diagnostics);
        assert_eq!(
            diagnostics,
            vec![
                "[workflow] task    : task-obs".to_owned(),
                "[workflow] source  : tests/obs.json".to_owned(),
                "[workflow] graders : none (unverified run \u{2014} DONE reflects the agent's own claim)"
                    .to_owned(),
                format!("[workflow] run_id  : {}", outcome.lifecycle.run_id),
                "[workflow] status  : done".to_owned(),
            ]
        );
        let events = lines_of(&events);
        assert!(!events.is_empty());
        assert!(
            events
                .iter()
                .all(|line| line.starts_with('[') && line.contains("harness."))
        );
        assert!(
            events
                .iter()
                .any(|line| line.contains("harness.attempt_started attempt=1"))
        );
        assert!(
            !events.iter().any(|line| line.contains("domain.")),
            "domain mirrors stay off the live stream"
        );
        // The wrapped sink still carries every record, mirrors included.
        assert!(sink.kinds().iter().any(|k| k.starts_with("domain.")));
    }

    #[tokio::test]
    async fn json_event_stream_emits_sorted_ndjson() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let (events_out, events) = capture();
        let options = base_options(&store, &sink).with_events(EventsMode::Json, events_out);
        let outcome = run_task_object(
            &task("json me"),
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            options,
        )
        .await
        .unwrap();
        let events = lines_of(&events);
        assert!(!events.is_empty());
        for line in &events {
            let parsed: Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["run_id"], json!(outcome.lifecycle.run_id));
            let keys: Vec<&String> = parsed.as_object().unwrap().keys().collect();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(keys, sorted, "{line}");
            assert!(parsed["kind"].as_str().unwrap().starts_with("harness."));
        }
    }

    #[tokio::test]
    async fn run_id_resume_and_error_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let (stream, diagnostics) = capture();
        let failing = Task::new(
            "fail validation",
            vec![Grader::Command(CommandGrader {
                run: "false".into(),
                name: Some("red".into()),
            })],
        );
        let options = base_options(&store, &sink)
            .with_stream(stream)
            .with_run_id("run-fixed")
            .with_max_retries(0);
        let mut worker = options;
        worker.worker_id = Some("worker-7".into());
        let outcome = run_task_object(
            &failing,
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            worker,
        )
        .await
        .unwrap();
        assert_eq!(outcome.lifecycle.run_id, "run-fixed");
        assert_eq!(outcome.lifecycle.worker_id, "worker-7");
        assert_eq!(outcome.lifecycle.status, Status::Failed);
        let diagnostics = lines_of(&diagnostics);
        assert!(diagnostics.contains(&"[workflow] run_id  : run-fixed".to_owned()));
        assert!(diagnostics.contains(&"[workflow] status  : failed".to_owned()));
        assert!(
            diagnostics
                .iter()
                .any(|l| l.starts_with("[workflow] error   : "))
        );
        assert_eq!(
            store.load_lifecycle("run-fixed").unwrap().unwrap().status,
            Status::Failed
        );
    }

    #[tokio::test]
    async fn stranded_prior_run_is_recovered_before_the_new_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let (stream, diagnostics) = capture();
        let task = task("again").with_id("task-strand");
        seed_open(&store, "task-strand", "run-old", Status::Running);
        seed_open(&store, "other-task", "run-other", Status::Running);
        let options = base_options(&store, &sink).with_stream(stream);
        let outcome = run_task_object(
            &task,
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            options,
        )
        .await
        .unwrap();
        assert_eq!(outcome.lifecycle.status, Status::Done);
        assert_eq!(
            lines_of(&diagnostics)[0],
            "[workflow] recovered: stranded run run-old -> interrupted"
        );
        assert_eq!(
            store.load_lifecycle("run-old").unwrap().unwrap().status,
            Status::Interrupted
        );
        assert_eq!(
            store.load_lifecycle("run-other").unwrap().unwrap().status,
            Status::Running,
            "only the task's own stranded runs are swept"
        );
    }

    #[tokio::test]
    async fn shutdown_signal_parks_the_lifecycle_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let (stream, diagnostics) = capture();
        let signal = ShutdownSignal::new();
        let mut options = base_options(&store, &sink)
            .with_stream(stream)
            .with_invoke(never_returns())
            .with_run_id("run-sig");
        options.shutdown = Some(signal.clone());
        let trigger = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.trigger();
        });
        let err = run_task_object(
            &task("interrupt me"),
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            options,
        )
        .await
        .unwrap_err();
        assert!(err.is_interrupted(), "{err}");
        assert_eq!(
            store.load_lifecycle("run-sig").unwrap().unwrap().status,
            Status::Interrupted
        );
        assert_eq!(
            lines_of(&diagnostics).last().unwrap(),
            "[workflow] status  : interrupted (worker received signal)"
        );
    }

    #[tokio::test]
    async fn install_cancel_on_signal_returns_an_abortable_listener() {
        let signal = ShutdownSignal::new();
        let listener = install_cancel_on_signal(&signal);
        if cfg!(unix) {
            let handle = listener.expect("unix installs a listener");
            assert!(!signal.is_triggered());
            handle.abort();
            assert!(handle.await.unwrap_err().is_cancelled());
        } else {
            assert!(listener.is_none());
        }
    }

    #[tokio::test]
    async fn run_task_file_loads_the_task_and_sets_source() {
        let dir = tempfile::tempdir().unwrap();
        let task_file = dir.path().join("task.json");
        std::fs::write(
            &task_file,
            r#"{"id": "from-file", "goal": "Load me.", "graders": []}"#,
        )
        .unwrap();
        let store = memory_store();
        let sink = ListSink::new();
        let (stream, diagnostics) = capture();
        let options = base_options(&store, &sink)
            .with_stream(stream)
            .with_source("ignored");
        let outcome = run_task_file(
            &task_file,
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            options,
        )
        .await
        .unwrap();
        assert_eq!(outcome.lifecycle.task_id, "from-file");
        assert_eq!(outcome.lifecycle.source, task_file.display().to_string());
        assert!(
            lines_of(&diagnostics)
                .contains(&format!("[workflow] source  : {}", task_file.display()))
        );

        let missing = dir.path().join("missing.json");
        let err = run_task_file(
            &missing,
            &dir.path().join("db").join("flywheel.sqlite"),
            &dir.path().join("sandbox"),
            base_options(&store, &sink),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WorkflowError::TaskLoad(_)), "{err}");
        assert!(err.to_string().contains("cannot read file"));
    }

    #[tokio::test]
    async fn production_invoker_runs_the_agent_and_streams_turns() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_fake_claude(dir.path());
        let store = memory_store();
        let sink = ListSink::new();
        let (events_out, events) = capture();
        let mut options = RunTaskOptions::default()
            .with_store(store.clone() as Arc<dyn RunStore>)
            .with_sink(sink.clone() as SharedSink)
            .with_stream(Arc::new(|_line: &str| {}))
            .with_events(EventsMode::Plain, events_out)
            .with_agent_id("claude-code");
        options.agent.command_override = Some(vec!["/bin/sh".into(), script.display().to_string()]);
        options
            .agent
            .agent_env
            .insert(SCENARIO_VAR.into(), "envelope".into());
        options.agent.transport = Some("cli".into());
        let outcome = tokio::time::timeout(
            Duration::from_secs(60),
            run_task_object(
                &task("real agent path"),
                &dir.path().join("db").join("flywheel.sqlite"),
                &dir.path().join("sandbox"),
                options,
            ),
        )
        .await
        .expect("the fake agent finishes promptly")
        .unwrap();
        assert_eq!(outcome.lifecycle.status, Status::Done);
        assert_eq!(outcome.attempts[0].agent_context["agent_id"], "claude-code");
        let events = lines_of(&events);
        assert!(
            events.iter().any(|l| l.contains("] SYSTEM  init")),
            "{events:?}"
        );
        assert!(
            events.iter().any(|l| l.contains("] ASSISTANT  done")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|l| l.contains("] RESULT  subtype=success turns=1 cost=$0.0100")),
            "{events:?}"
        );
        assert!(events.iter().any(|l| l.contains("harness.attempt_started")));
        // The persistence observer still fired alongside the renderer.
        assert!(
            sink.kinds().iter().any(|k| k == "message.assistant"),
            "{:?}",
            sink.kinds()
        );
    }

    #[test]
    fn claude_code_invoke_stamps_control_audit_records_with_the_attempt() {
        let store = memory_store();
        let sink = ListSink::new();
        let (renderer, rendered) = capture();
        let observer: MessageObserver =
            Arc::new(move |msg: &Value| renderer(&msg["type"].to_string()));
        let invoke = make_claude_code_invoke(
            Path::new("/tmp"),
            "claude-code",
            &AgentOptions::default(),
            Some(observer),
            Some(ControlRoute {
                store: store.clone(),
                run_id: "run-ctl".into(),
            }),
            Some(sink.clone()),
        );
        drop(invoke);
        assert!(rendered.lock().unwrap().is_empty());
        assert!(sink.kinds().is_empty());
    }

    fn git_repo_with_commit() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("f"), "x").unwrap();
        run(&["add", "f"]);
        run(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"]);
        dir
    }

    #[tokio::test]
    async fn resolve_base_commit_sha_in_and_out_of_git() {
        let repo = git_repo_with_commit();
        let sha = resolve_base_commit_sha(repo.path()).await.unwrap();
        assert_eq!(sha.len(), 40);
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(resolve_base_commit_sha(plain.path()).await, None);
        assert_eq!(
            resolve_base_commit_sha(Path::new("/nonexistent/dir")).await,
            None
        );
    }

    // --- CLI ----------------------------------------------------------------------------

    fn argv(items: &[&str]) -> Vec<String> {
        strings(items)
    }

    fn expect_error(items: &[&str], usage: &str, prog: &str, message: &str) {
        let exit = parse_args(&argv(items)).unwrap_err();
        assert_eq!(
            exit,
            CliExit::Error {
                usage: usage.to_owned(),
                prog: prog.to_owned(),
                message: message.to_owned(),
            },
            "{items:?}"
        );
    }

    #[test]
    fn parse_errors_match_argparse() {
        let run_prog = "python -m flywheel_core.workflow run";
        expect_error(
            &[],
            MAIN_USAGE,
            PROG,
            "the following arguments are required: command",
        );
        expect_error(
            &["bogus"],
            MAIN_USAGE,
            PROG,
            "argument command: invalid choice: 'bogus' (choose from run, is-done, interrupt, steer, set-model, approve, reject)",
        );
        expect_error(
            &["run"],
            RUN_USAGE,
            run_prog,
            "the following arguments are required: GOAL_OR_FILE",
        );
        expect_error(
            &["run", "g", "--max-turns", "abc"],
            RUN_USAGE,
            run_prog,
            "argument --max-turns: invalid int value: 'abc'",
        );
        expect_error(
            &["run", "g", "--max-retries", "1.5"],
            RUN_USAGE,
            run_prog,
            "argument --max-retries: invalid int value: '1.5'",
        );
        expect_error(
            &["run", "g", "--json", "--quiet"],
            RUN_USAGE,
            run_prog,
            "argument --quiet: not allowed with argument --json",
        );
        expect_error(
            &["run", "g", "--quiet", "--json"],
            RUN_USAGE,
            run_prog,
            "argument --json: not allowed with argument --quiet",
        );
        expect_error(
            &["run", "g", "--bogus"],
            MAIN_USAGE,
            PROG,
            "unrecognized arguments: --bogus",
        );
        expect_error(
            &["run", "g", "extra"],
            MAIN_USAGE,
            PROG,
            "unrecognized arguments: extra",
        );
        expect_error(
            &["interrupt", "--db", "x", "r1", "extra"],
            MAIN_USAGE,
            PROG,
            "unrecognized arguments: extra",
        );
        expect_error(
            &["run", "g", "--db"],
            RUN_USAGE,
            run_prog,
            "argument --db: expected one argument",
        );
        expect_error(
            &["run", "g", "--db", "--json"],
            RUN_USAGE,
            run_prog,
            "argument --db: expected one argument",
        );
        expect_error(
            &["run", "--check"],
            RUN_USAGE,
            run_prog,
            "argument --check: expected one argument",
        );
        expect_error(
            &["run", "g", "--ma", "1"],
            RUN_USAGE,
            run_prog,
            "ambiguous option: --ma could match --max-turns, --max-retries",
        );
        expect_error(
            &["run", "g", "--json=1"],
            RUN_USAGE,
            run_prog,
            "argument --json: ignored explicit argument '1'",
        );
        expect_error(
            &["reject", "r1", "--feedback"],
            REJECT_USAGE,
            "python -m flywheel_core.workflow reject",
            "argument --feedback: expected one argument",
        );
        expect_error(
            &["is-done"],
            IS_DONE_USAGE,
            "python -m flywheel_core.workflow is-done",
            "the following arguments are required: task_file",
        );
        expect_error(
            &["steer", "r1"],
            STEER_USAGE,
            "python -m flywheel_core.workflow steer",
            "the following arguments are required: MESSAGE",
        );
        expect_error(
            &["steer"],
            STEER_USAGE,
            "python -m flywheel_core.workflow steer",
            "the following arguments are required: RUN_ID, MESSAGE",
        );
        expect_error(
            &["set-model", "r1"],
            SET_MODEL_USAGE,
            "python -m flywheel_core.workflow set-model",
            "the following arguments are required: MODEL",
        );
        expect_error(
            &["approve"],
            APPROVE_USAGE,
            "python -m flywheel_core.workflow approve",
            "the following arguments are required: RUN_ID",
        );
        expect_error(
            &["interrupt"],
            INTERRUPT_USAGE,
            "python -m flywheel_core.workflow interrupt",
            "the following arguments are required: RUN_ID",
        );
    }

    #[test]
    fn help_exits_match_argparse() {
        let cases: [(&[&str], &str); 10] = [
            (&["-h"], MAIN_HELP),
            (&["--help"], MAIN_HELP),
            (&["--he"], MAIN_HELP),
            (&["run", "-h"], RUN_HELP),
            (&["run", "-h", "g"], RUN_HELP),
            (&["is-done", "--help"], IS_DONE_HELP),
            (&["interrupt", "-h"], INTERRUPT_HELP),
            (&["steer", "-h"], STEER_HELP),
            (&["set-model", "-h"], SET_MODEL_HELP),
            (&["approve", "-h"], APPROVE_HELP),
        ];
        for (items, help) in cases {
            assert_eq!(
                parse_args(&argv(items)).unwrap_err(),
                CliExit::Help(help.to_owned()),
                "{items:?}"
            );
        }
        assert_eq!(
            parse_args(&argv(&["reject", "r", "-h"])).unwrap_err(),
            CliExit::Help(REJECT_HELP.to_owned())
        );
        assert_eq!(CliExit::Help(MAIN_HELP.to_owned()).report(), 0);
        assert_eq!(
            CliExit::error(
                MAIN_USAGE,
                PROG,
                "the following arguments are required: command"
            )
            .report(),
            2
        );
    }

    #[test]
    fn parse_run_defaults_options_and_prefixes() {
        let parsed = parse_args(&argv(&["run", "do the thing"])).unwrap();
        assert_eq!(
            parsed,
            Command::Run(RunArgs {
                target: "do the thing".into(),
                db: None,
                check: Vec::new(),
                rubric: Vec::new(),
                sandbox: None,
                model: None,
                max_turns: DEFAULT_MAX_TURNS,
                max_retries: DEFAULT_MAX_RETRIES,
                json: false,
                quiet: false,
            })
        );
        let parsed = parse_args(&argv(&[
            "run",
            "--check",
            "a",
            "g",
            "--check",
            "b",
            "--rubric",
            "r",
            "--max-turns=7",
            "--max-r",
            "2",
            "--db",
            "x.sqlite",
            "--sandbox",
            "sb",
            "--mod",
            "m",
            "--quiet",
            "--quiet",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Command::Run(RunArgs {
                target: "g".into(),
                db: Some("x.sqlite".into()),
                check: strings(&["a", "b"]),
                rubric: strings(&["r"]),
                sandbox: Some("sb".into()),
                model: Some("m".into()),
                max_turns: 7,
                max_retries: 2,
                json: false,
                quiet: true,
            })
        );
        // `--` ends option parsing; a negative number is a value, not a flag.
        let parsed = parse_args(&argv(&["run", "--", "-dash goal"])).unwrap();
        assert!(
            matches!(parsed, Command::Run(RunArgs { ref target, .. }) if target == "-dash goal")
        );
        let parsed = parse_args(&argv(&["run", "-1"])).unwrap();
        assert!(matches!(parsed, Command::Run(RunArgs { ref target, .. }) if target == "-1"));
        let parsed = parse_args(&argv(&["run", "g", "--max-turns", " +12 ", "--json"])).unwrap();
        assert!(matches!(
            parsed,
            Command::Run(RunArgs {
                max_turns: 12,
                json: true,
                ..
            })
        ));
    }

    #[test]
    fn parse_steering_verbs() {
        assert_eq!(
            parse_args(&argv(&["interrupt", "r1", "--db", "d"])).unwrap(),
            Command::Interrupt {
                run_id: "r1".into(),
                db: Some("d".into())
            }
        );
        assert_eq!(
            parse_args(&argv(&["steer", "r1", "hello there"])).unwrap(),
            Command::Steer {
                run_id: "r1".into(),
                message: "hello there".into(),
                db: None
            }
        );
        assert_eq!(
            parse_args(&argv(&["set-model", "r1", "claude-x"])).unwrap(),
            Command::SetModel {
                run_id: "r1".into(),
                model: "claude-x".into(),
                db: None
            }
        );
        assert_eq!(
            parse_args(&argv(&["approve", "r1"])).unwrap(),
            Command::Approve {
                run_id: "r1".into(),
                db: None
            }
        );
        assert_eq!(
            parse_args(&argv(&["reject", "r1", "--feedback", "needs tests"])).unwrap(),
            Command::Reject {
                run_id: "r1".into(),
                feedback: Some("needs tests".into()),
                db: None
            }
        );
        assert_eq!(
            parse_args(&argv(&["is-done", "t.json"])).unwrap(),
            Command::IsDone {
                task_file: "t.json".into(),
                db: None
            }
        );
        assert_eq!(resolve_db(None), PathBuf::from(DEFAULT_DB_PATH));
        assert_eq!(resolve_db(Some("")), PathBuf::from(DEFAULT_DB_PATH));
        assert_eq!(resolve_db(Some("x.db")), PathBuf::from("x.db"));
    }

    #[test]
    fn main_is_done_and_sqlite_pinned_verbs() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state").join("flywheel.sqlite");
        let db_arg = db.display().to_string();
        let task_file = dir.path().join("task.json");
        std::fs::write(
            &task_file,
            r#"{"id": "cli-task", "goal": "CLI.", "graders": []}"#,
        )
        .unwrap();
        let task_arg = task_file.display().to_string();

        assert_eq!(
            main(&argv(&["is-done", "missing.json", "--db", &db_arg])),
            2
        );
        assert_eq!(main(&argv(&["interrupt", "run-none", "--db", &db_arg])), 2);
        assert!(
            db.is_file(),
            "the producer verb opens the sqlite store at --db"
        );
        assert_eq!(main(&argv(&["is-done", &task_arg, "--db", &db_arg])), 1);

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let options = RunTaskOptions::default()
            .with_invoke(verify_invoke())
            .with_stream(Arc::new(|_line: &str| {}));
        let outcome = runtime
            .block_on(run_task_file(
                &task_file,
                &db,
                &dir.path().join("sb"),
                options,
            ))
            .unwrap();
        assert_eq!(outcome.lifecycle.status, Status::Done);
        assert_eq!(main(&argv(&["is-done", &task_arg, "--db", &db_arg])), 0);
        // A done run is not in-flight: the command is enqueued with the stale note.
        assert_eq!(
            main(&argv(&[
                "steer",
                &outcome.lifecycle.run_id,
                "hi",
                "--db",
                &db_arg
            ])),
            0
        );
        assert_eq!(
            main(&argv(&[
                "steer",
                &outcome.lifecycle.run_id,
                "",
                "--db",
                &db_arg
            ])),
            2
        );
        let store = SqliteStore::open(&db).unwrap();
        let commands = store
            .claim_commands(&outcome.lifecycle.run_id, Utc::now())
            .unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].kind, CONTROL_COMMAND_SAY);
        assert_eq!(commands[0].payload["text"], json!("hi"));
        assert_eq!(main(&argv(&["bogus"])), 2);
        assert_eq!(main(&argv(&["run", &task_arg, "--check", "true"])), 2);
        assert_eq!(main(&argv(&["run", "   ", "--db", &db_arg])), 2);
    }

    #[test]
    fn injected_store_lands_every_producer_verb() {
        let store = memory_store();
        seed_open(&store, "t1", "run-live", Status::Running);
        seed_open(&store, "t1", "run-ready", Status::Ready);
        let mut parked = Lifecycle::new("t1");
        parked.run_id = "run-parked".into();
        let now = Utc::now();
        for status in [
            Status::Ready,
            Status::Running,
            Status::Validating,
            Status::AwaitingApproval,
        ] {
            parked.transition_to(status, "", Some(now)).unwrap();
        }
        store.create_lifecycle(&parked).unwrap();
        let injected: Arc<dyn RunStore> = store.clone();
        let unused = Path::new("/nonexistent/never-created.sqlite");

        let verbs: [(&[&str], &str, Value); 5] = [
            (
                &["interrupt", "run-live"],
                CONTROL_COMMAND_INTERRUPT,
                json!({}),
            ),
            (
                &["steer", "run-live", "focus"],
                CONTROL_COMMAND_SAY,
                json!({"text": "focus"}),
            ),
            (
                &["set-model", "run-ready", "m2"],
                CONTROL_COMMAND_SET_MODEL,
                json!({"model": "m2"}),
            ),
            (
                &["approve", "run-parked"],
                CONTROL_COMMAND_APPROVE,
                json!({}),
            ),
            (
                &["reject", "run-parked", "--feedback", "no"],
                CONTROL_COMMAND_REJECT,
                json!({"feedback": "no"}),
            ),
        ];
        for (items, kind, payload) in verbs {
            assert_eq!(
                main_with_store(&argv(items), Some(injected.clone())),
                0,
                "{items:?}"
            );
            let run_id = items[1];
            let claimed = store.claim_commands(run_id, Utc::now()).unwrap();
            let last = claimed.last().unwrap();
            assert_eq!(last.kind, kind);
            assert_eq!(Value::Object(last.payload.clone()), payload);
        }
        assert_eq!(
            main_with_store(&argv(&["reject", "run-parked"]), Some(injected.clone())),
            0
        );
        let claimed = store.claim_commands("run-parked", Utc::now()).unwrap();
        assert!(claimed.last().unwrap().payload.is_empty());
        assert_eq!(
            main_with_store(&argv(&["approve", "run-none"]), Some(injected)),
            2
        );
        assert!(!unused.exists());
        assert_eq!(
            enqueue_control_command(
                unused,
                "run-live",
                CONTROL_COMMAND_SAY,
                JsonObject::new(),
                Some(store.clone())
            ),
            0
        );
        assert!(
            !unused.exists(),
            "an injected store never materializes the sqlite path"
        );
    }
}
