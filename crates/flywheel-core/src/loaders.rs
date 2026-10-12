//! Task loaders: JSON mapping / file / directory / JSONL sources, the
//! `Task -> JSON` projection, and the content digest.
//!
//! `Task` is input-source agnostic; everything here is an optional
//! convenience that lives outside the purity-enforced `crate::task` because
//! it touches the filesystem and JSON.
//!
//! Error messages always identify the offending source (a file path, or
//! `<path>:<line>` for JSONL) so callers see actionable errors, never raw
//! parser output.
//!
//! The digest canonicalization reproduces Python's
//! `json.dumps(obj, sort_keys=True, separators=(",", ":"))` byte-for-byte
//! (ASCII-only output with `\uXXXX` escapes, Python float `repr`) so Rust
//! and Python stores pin identical `task_versions` rows for the same task.

use crate::task::{
    CommandGrader, Context, Grader, ManualGrader, RubricGrader, Seconds, Task, TaskBudgets,
    TranscriptGrader, python_float_repr,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;

/// Raised when a task source cannot be parsed or fails validation.
///
/// The message always identifies the offending source (file path, or
/// `<path>:<line>` for JSONL).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TaskLoadError(pub String);

impl TaskLoadError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// The exact `budgets` keys a task source may carry.
pub const BUDGET_KEYS: [&str; 3] = [
    "agent_iteration_seconds",
    "rubric_judge_seconds",
    "rubric_judge_max_turns",
];

/// Source label used by [`load_task_data`] when the caller gives none.
pub const DEFAULT_DATA_SOURCE: &str = "<data>";
/// Source label used by [`load_graders`] when the caller gives none.
pub const DEFAULT_GRADERS_SOURCE: &str = "<graders>";
/// Source label used by [`deserialize_task`].
pub const SERIALIZED_TASK_SOURCE: &str = "<serialized task>";
/// Source label used by the JSONL reader/str loaders when none is given.
pub const DEFAULT_STREAM_SOURCE: &str = "<stream>";

// --- Loading (JSON -> Task) -------------------------------------------------

/// Build a validated [`Task`] from an already-decoded JSON value.
///
/// The non-file twin of [`load_task_file`] for callers that obtained the
/// task object from somewhere other than disk (an API payload, an issue
/// body, an inline literal). `source` labels error messages so failures
/// stay actionable.
pub fn load_task_data(data: &Value, source: &str) -> Result<Task, TaskLoadError> {
    task_from_value(data, source)
}

/// Build a validated [`Task`] from JSON text.
///
/// `source` labels both JSON-syntax and schema errors.
pub fn load_task_str(text: &str, source: &str) -> Result<Task, TaskLoadError> {
    let data = parse_json(text, source)?;
    task_from_value(&data, source)
}

/// Build validated graders from a decoded `graders`-shaped list.
///
/// Applies exactly the per-entry validation [`load_task_file`] applies to a
/// task's `graders` field. Useful for callers that assemble graders
/// separately from a full task definition (e.g. a default-grader policy
/// applied to externally-sourced work items).
pub fn load_graders(entries: &Value, source: &str) -> Result<Vec<Grader>, TaskLoadError> {
    let Value::Array(entries) = entries else {
        return Err(TaskLoadError::new(format!(
            "{source}: 'graders' must be a list, got {}",
            py_type_name(entries)
        )));
    };
    entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| match entry {
            Value::Object(obj) => build_grader(obj, source, idx),
            other => Err(TaskLoadError::new(format!(
                "{source}: graders[{idx}] must be an object, got {}",
                py_type_name(other)
            ))),
        })
        .collect()
}

/// Build a validated [`Task`] from a single JSON file.
pub fn load_task_file(path: impl AsRef<Path>) -> Result<Task, TaskLoadError> {
    let path = path.as_ref();
    let source = path.display().to_string();
    let text = read_file(path, &source)?;
    load_task_str(&text, &source)
}

/// Build validated [`Task`]s from every `.json` file directly under `path`.
///
/// Non-`.json` entries and subdirectories are ignored; files are visited in
/// name order. An empty (or JSON-less) directory returns an empty list.
pub fn load_task_directory(path: impl AsRef<Path>) -> Result<Vec<Task>, TaskLoadError> {
    let dir = path.as_ref();
    if !dir.is_dir() {
        return Err(TaskLoadError::new(format!(
            "{}: not a directory",
            dir.display()
        )));
    }
    let entries = std::fs::read_dir(dir).map_err(|err| {
        TaskLoadError::new(format!("{}: cannot read directory: {err}", dir.display()))
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            TaskLoadError::new(format!("{}: cannot read directory: {err}", dir.display()))
        })?;
        paths.push(entry.path());
    }
    paths.sort();
    paths
        .into_iter()
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "json"))
        .map(load_task_file)
        .collect()
}

/// Build validated [`Task`]s from a JSONL file.
///
/// Blank lines and lines whose first non-whitespace character is `#` are
/// skipped. Trailing newlines are tolerated. Malformed lines raise a
/// [`TaskLoadError`] citing the path and 1-based line number.
pub fn load_tasks_jsonl_file(path: impl AsRef<Path>) -> Result<Vec<Task>, TaskLoadError> {
    let path = path.as_ref();
    let label = path.display().to_string();
    let text = read_file(path, &label)?;
    load_tasks_jsonl_str(&text, &label)
}

/// Build validated [`Task`]s from a JSONL stream; `label` names the stream
/// in error messages (see [`DEFAULT_STREAM_SOURCE`]).
pub fn load_tasks_jsonl_reader(
    mut reader: impl Read,
    label: &str,
) -> Result<Vec<Task>, TaskLoadError> {
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|err| TaskLoadError::new(format!("{label}: cannot read stream: {err}")))?;
    load_tasks_jsonl_str(&text, label)
}

/// Build validated [`Task`]s from in-memory JSONL text; `label` names the
/// source in error messages.
pub fn load_tasks_jsonl_str(text: &str, label: &str) -> Result<Vec<Task>, TaskLoadError> {
    let mut tasks = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        let stripped = raw.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let source = format!("{label}:{lineno}");
        let data = parse_json(stripped, &source)?;
        tasks.push(task_from_value(&data, &source)?);
    }
    Ok(tasks)
}

fn read_file(path: &Path, source: &str) -> Result<String, TaskLoadError> {
    std::fs::read_to_string(path)
        .map_err(|err| TaskLoadError::new(format!("{source}: cannot read file: {err}")))
}

fn parse_json(text: &str, source: &str) -> Result<Value, TaskLoadError> {
    serde_json::from_str(text)
        .map_err(|err| TaskLoadError::new(format!("{source}: invalid JSON: {err}")))
}

/// `dict.get(key)` semantics: an explicit JSON `null` reads as absent.
fn get<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

fn opt_string(
    obj: &Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<Option<String>, TaskLoadError> {
    match get(obj, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(TaskLoadError::new(format!(
            "{what} '{key}' must be a string or None"
        ))),
    }
}

/// A JSON integer that fits `u64`; floats, bools, and negatives are rejected.
fn opt_u64(obj: &Map<String, Value>, key: &str, what: &str) -> Result<Option<u64>, TaskLoadError> {
    match get(obj, key) {
        None => Ok(None),
        Some(Value::Number(n)) if n.as_u64().is_some() => Ok(n.as_u64()),
        Some(_) => Err(TaskLoadError::new(format!(
            "{what} '{key}' must be a non-negative integer or None"
        ))),
    }
}

fn number_to_seconds(n: &serde_json::Number) -> Option<Seconds> {
    if let Some(i) = n.as_i64() {
        Some(Seconds::Int(i))
    } else {
        n.as_f64().map(Seconds::Float)
    }
}

fn opt_seconds(
    obj: &Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<Option<Seconds>, TaskLoadError> {
    match get(obj, key) {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(number_to_seconds(n)),
        Some(_) => Err(TaskLoadError::new(format!(
            "{what} '{key}' must be a number or None"
        ))),
    }
}

fn build_grader(
    entry: &Map<String, Value>,
    source: &str,
    idx: usize,
) -> Result<Grader, TaskLoadError> {
    let prefix = format!("{source}: graders[{idx}]");
    let what = |kind: &str| format!("{prefix} {kind} grader");
    let grader_type = get(entry, "type").and_then(Value::as_str);
    let grader = match grader_type {
        Some("command") => {
            let name = opt_string(entry, "name", &what("command"))?;
            let run = match get(entry, "run") {
                Some(Value::String(s)) if !s.is_empty() => s.clone(),
                _ => {
                    return Err(TaskLoadError::new(format!(
                        "{prefix} command grader requires a non-empty 'run'"
                    )));
                }
            };
            Grader::Command(CommandGrader { run, name })
        }
        Some("rubric") => {
            let assertions = match get(entry, "assertions") {
                Some(Value::Array(items)) if !items.is_empty() => items
                    .iter()
                    .map(|item| {
                        item.as_str().map(str::to_owned).ok_or_else(|| {
                            TaskLoadError::new(format!(
                                "{prefix} rubric grader 'assertions' entries must all be strings"
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => {
                    return Err(TaskLoadError::new(format!(
                        "{prefix} rubric grader requires a non-empty 'assertions' list"
                    )));
                }
            };
            let rubric = opt_string(entry, "rubric", &what("rubric"))?;
            let name = opt_string(entry, "name", &what("rubric"))?;
            let judge_model = opt_string(entry, "judge_model", &what("rubric"))?;
            let retry_on_fail = match entry.get("retry_on_fail") {
                None => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(TaskLoadError::new(format!(
                        "{prefix} rubric grader 'retry_on_fail' must be a bool"
                    )));
                }
            };
            Grader::Rubric(RubricGrader {
                assertions,
                rubric,
                name,
                judge_model,
                retry_on_fail,
            })
        }
        Some("manual") => {
            let name = opt_string(entry, "name", &what("manual"))?;
            let instruction = match get(entry, "instruction") {
                Some(Value::String(s)) if !s.is_empty() => s.clone(),
                _ => {
                    return Err(TaskLoadError::new(format!(
                        "{prefix} manual grader requires a non-empty 'instruction'"
                    )));
                }
            };
            Grader::Manual(ManualGrader { instruction, name })
        }
        Some("transcript") => Grader::Transcript(TranscriptGrader {
            max_turns: opt_u64(entry, "max_turns", &what("transcript"))?,
            max_total_tokens: opt_u64(entry, "max_total_tokens", &what("transcript"))?,
            max_wall_seconds: opt_seconds(entry, "max_wall_seconds", &what("transcript"))?,
            name: opt_string(entry, "name", &what("transcript"))?,
        }),
        _ => {
            return Err(TaskLoadError::new(format!(
                "{prefix} has unknown type {}; expected one of \
                 ('command', 'rubric', 'manual', 'transcript')",
                py_repr(entry.get("type").unwrap_or(&Value::Null))
            )));
        }
    };
    grader
        .validate()
        .map_err(|err| TaskLoadError::new(format!("{prefix} {err}")))?;
    Ok(grader)
}

/// Coerce an optional JSON list-of-strings field.
///
/// Absent/`null` yields `[]`. A non-list (notably a bare string, the common
/// authoring typo `"tags": "http"`) is rejected rather than silently
/// iterated into a per-character list. Every entry must be a string.
fn str_list_field(
    value: Option<&Value>,
    field: &str,
    source: &str,
) -> Result<Vec<String>, TaskLoadError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err(TaskLoadError::new(format!(
            "{source}: '{field}' must be a list of strings, got {}",
            py_type_name(value)
        )));
    };
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            item.as_str().map(str::to_owned).ok_or_else(|| {
                TaskLoadError::new(format!(
                    "{source}: '{field}' must be a list of strings, got {} at index {idx}",
                    py_type_name(item)
                ))
            })
        })
        .collect()
}

/// Coerce an optional `context.notes` field to a string.
///
/// Absent/`null` yields `""`. A string passes through. A list of strings (a
/// common multi-paragraph authoring convention) is joined with blank lines
/// so `Context::notes` is always the single string the prompt builder
/// requires. Any other type, or a list with a non-string entry, is rejected
/// at load time.
fn notes_field(value: Option<&Value>, source: &str) -> Result<String, TaskLoadError> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(items)) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) => parts.push(s.as_str()),
                    _ => {
                        return Err(TaskLoadError::new(format!(
                            "{source}: 'context.notes' list entries must all be strings"
                        )));
                    }
                }
            }
            Ok(parts.join("\n\n"))
        }
        Some(other) => Err(TaskLoadError::new(format!(
            "{source}: 'context.notes' must be a string or list of strings, got {}",
            py_type_name(other)
        ))),
    }
}

/// Parse the optional per-task `budgets` object (execution overrides).
///
/// Exactly the [`BUDGET_KEYS`] are accepted; an unknown key is rejected
/// loudly so a typo (`iteration_seconds`) never silently leaves a
/// heavyweight task on the repo-wide ceiling it declared itself out of.
/// Range validation is `TaskBudgets::validate`'s job, reached via
/// `Task::validate`.
fn budgets_field(value: &Value, source: &str) -> Result<TaskBudgets, TaskLoadError> {
    let Value::Object(obj) = value else {
        return Err(TaskLoadError::new(format!(
            "{source}: 'budgets' must be an object, got {}",
            py_type_name(value)
        )));
    };
    let mut unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !BUDGET_KEYS.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(TaskLoadError::new(format!(
            "{source}: 'budgets' has unknown key(s) {}; valid keys: {}",
            py_str_list(&unknown),
            py_str_list(&BUDGET_KEYS)
        )));
    }
    let seconds = |key: &str| -> Result<Option<Seconds>, TaskLoadError> {
        match get(obj, key) {
            None => Ok(None),
            Some(Value::Number(n)) => Ok(number_to_seconds(n)),
            Some(_) => Err(TaskLoadError::new(format!(
                "{source}: budgets.{key} must be a number of seconds"
            ))),
        }
    };
    let rubric_judge_max_turns = match get(obj, "rubric_judge_max_turns") {
        None => None,
        Some(Value::Number(n)) if n.as_u64().is_some() => n.as_u64(),
        Some(_) => {
            return Err(TaskLoadError::new(format!(
                "{source}: budgets.rubric_judge_max_turns must be a positive integer"
            )));
        }
    };
    Ok(TaskBudgets {
        agent_iteration_seconds: seconds("agent_iteration_seconds")?,
        rubric_judge_seconds: seconds("rubric_judge_seconds")?,
        rubric_judge_max_turns,
    })
}

/// Python truthiness of a JSON value (`data.get("context") or {}`).
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn task_from_value(data: &Value, source: &str) -> Result<Task, TaskLoadError> {
    let Value::Object(data) = data else {
        return Err(TaskLoadError::new(format!(
            "{source}: expected JSON object, got {}",
            py_type_name(data)
        )));
    };

    let empty = Map::new();
    let context_data = match data.get("context") {
        Some(v) if !is_falsy(v) => match v {
            Value::Object(obj) => obj,
            other => {
                return Err(TaskLoadError::new(format!(
                    "{source}: 'context' must be an object, got {}",
                    py_type_name(other)
                )));
            }
        },
        _ => &empty,
    };
    let list = |key: &str| str_list_field(context_data.get(key), &format!("context.{key}"), source);
    let context = Context {
        relevant: list("relevant")?,
        references: list("references")?,
        constraints: list("constraints")?,
        non_goals: list("non_goals")?,
        edge_cases: list("edge_cases")?,
        notes: notes_field(context_data.get("notes"), source)?,
    };

    let no_graders = Value::Array(Vec::new());
    let graders = load_graders(data.get("graders").unwrap_or(&no_graders), source)?;

    let goal = match data.get("goal") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    // A non-string `id` (including an explicit null) must fail validation
    // exactly as in the Python loader; an empty string is the sentinel that
    // `Task::validate` rejects with the same message.
    let id = match data.get("id") {
        None => crate::task::default_task_id(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => String::new(),
    };
    let tags = match data.get("tags") {
        None => Vec::new(),
        Some(v) => str_list_field(Some(v), "tags", source)?,
    };
    let budgets = match data.get("budgets") {
        None => TaskBudgets::default(),
        Some(v) => budgets_field(v, source)?,
    };
    // `prerequisites` is an orchestration-layer concept (the inter-task
    // DAG), not part of a single task's definition: core ignores it here.

    let task = Task {
        id,
        goal,
        graders,
        tags,
        context,
        budgets,
    };
    task.validate()
        .map_err(|err| TaskLoadError::new(format!("{source}: {err}")))?;
    Ok(task)
}

// --- Serialization (Task -> JSON / digest) ------------------------------------
//
// The inverse of `task_from_value`. Concrete stores call these to persist a
// Task and to content-address it for the `task_versions` table.

/// Project a grader back into the JSON object [`load_graders`] consumes.
///
/// Optional fields that are `None` are omitted so the output matches the
/// on-disk file shape.
pub fn grader_to_value(grader: &Grader) -> Value {
    let mut spec = Map::new();
    spec.insert("type".into(), grader.grader_type().as_str().into());
    match grader {
        Grader::Command(g) => {
            spec.insert("run".into(), g.run.clone().into());
            if let Some(name) = &g.name {
                spec.insert("name".into(), name.clone().into());
            }
        }
        Grader::Rubric(g) => {
            spec.insert("assertions".into(), g.assertions.clone().into());
            spec.insert("retry_on_fail".into(), g.retry_on_fail.into());
            if let Some(rubric) = &g.rubric {
                spec.insert("rubric".into(), rubric.clone().into());
            }
            if let Some(name) = &g.name {
                spec.insert("name".into(), name.clone().into());
            }
            if let Some(judge_model) = &g.judge_model {
                spec.insert("judge_model".into(), judge_model.clone().into());
            }
        }
        Grader::Manual(g) => {
            spec.insert("instruction".into(), g.instruction.clone().into());
            if let Some(name) = &g.name {
                spec.insert("name".into(), name.clone().into());
            }
        }
        Grader::Transcript(g) => {
            if let Some(v) = g.max_turns {
                spec.insert("max_turns".into(), v.into());
            }
            if let Some(v) = g.max_total_tokens {
                spec.insert("max_total_tokens".into(), v.into());
            }
            if let Some(v) = g.max_wall_seconds {
                spec.insert("max_wall_seconds".into(), v.into());
            }
            if let Some(name) = &g.name {
                spec.insert("name".into(), name.clone().into());
            }
        }
    }
    Value::Object(spec)
}

fn context_to_value(context: &Context) -> Value {
    let mut obj = Map::new();
    obj.insert("relevant".into(), context.relevant.clone().into());
    obj.insert("references".into(), context.references.clone().into());
    obj.insert("constraints".into(), context.constraints.clone().into());
    obj.insert("non_goals".into(), context.non_goals.clone().into());
    obj.insert("edge_cases".into(), context.edge_cases.clone().into());
    obj.insert("notes".into(), context.notes.clone().into());
    Value::Object(obj)
}

/// The set fields of `budgets`, omitting the inherit-`None` defaults.
///
/// Empty for a default-constructed instance, so tasks that declare no
/// overrides serialize (and digest) byte-identically to before the field
/// existed.
fn budgets_to_map(budgets: &TaskBudgets) -> Map<String, Value> {
    let mut obj = Map::new();
    if let Some(v) = budgets.agent_iteration_seconds {
        obj.insert("agent_iteration_seconds".into(), v.into());
    }
    if let Some(v) = budgets.rubric_judge_seconds {
        obj.insert("rubric_judge_seconds".into(), v.into());
    }
    if let Some(v) = budgets.rubric_judge_max_turns {
        obj.insert("rubric_judge_max_turns".into(), v.into());
    }
    obj
}

/// The definition fields a digest covers: goal, graders, tags, context, and
/// budgets only when any override is set.
fn definition_value(task: &Task) -> Map<String, Value> {
    let mut def = Map::new();
    def.insert("goal".into(), task.goal.clone().into());
    def.insert(
        "graders".into(),
        Value::Array(task.graders.iter().map(grader_to_value).collect()),
    );
    def.insert("tags".into(), task.tags.clone().into());
    def.insert("context".into(), context_to_value(&task.context));
    let budgets = budgets_to_map(&task.budgets);
    if !budgets.is_empty() {
        def.insert("budgets".into(), Value::Object(budgets));
    }
    def
}

/// Return the JSON form of `task`.
///
/// Exact inverse of [`load_task_data`]: feeding the result to
/// [`deserialize_task`] reconstructs an equal [`Task`].
pub fn serialize_task(task: &Task) -> Value {
    let mut payload = Map::new();
    payload.insert("id".into(), task.id.clone().into());
    for (key, value) in definition_value(task) {
        payload.insert(key, value);
    }
    Value::Object(payload)
}

/// Reconstruct a validated [`Task`] from its serialized JSON form.
pub fn deserialize_task(data: &Value) -> Result<Task, TaskLoadError> {
    task_from_value(data, SERIALIZED_TASK_SOURCE)
}

/// Content hash of a task's definition: goal, graders, tags, context (and
/// budgets when any override is set).
///
/// Two tasks with the same definition share a digest regardless of `id`.
/// Editing the goal, a grader, a tag, or the context yields a new digest (a
/// new immutable version a run can pin). `prerequisites` is excluded because
/// it is not a core concept at all. Byte-identical to the Python
/// `task_digest` for the same definition (see [`canonical_json`]).
pub fn task_digest(task: &Task) -> String {
    let canonical = canonical_json(&Value::Object(definition_value(task)));
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

// --- Canonical JSON (Python json.dumps parity) -------------------------------

/// Serialize `value` exactly as Python's
/// `json.dumps(value, sort_keys=True, separators=(",", ":"))` would.
///
/// Object keys are emitted in code-point order, output is pure ASCII (every
/// non-ASCII or control character becomes a `\uXXXX` escape, astral
/// characters as surrogate pairs), and floats use Python's `repr` form
/// (`1e+16`, `1e-05`, `1000000000000000.0`). This is the shape the content
/// digest hashes, so it must never drift.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if n.is_f64() {
                out.push_str(&python_float_repr(n.as_f64().unwrap_or(0.0)));
            } else {
                let _ = write!(out, "{n}");
            }
        }
        Value::String(s) => write_python_escaped(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(obj) => {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_python_escaped(key, out);
                out.push(':');
                write_canonical(&obj[key], out);
            }
            out.push('}');
        }
    }
}

/// Python `json.dumps` string encoding with `ensure_ascii=True`.
fn write_python_escaped(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

// --- Python-flavored diagnostics ----------------------------------------------

/// `type(x).__name__` for the Python value a JSON value decodes to.
fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `repr(x)` for the Python value a JSON value decodes to.
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(n) if n.is_f64() => python_float_repr(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_str_repr(s),
        Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
            )
        }
        Value::Object(obj) => format!(
            "{{{}}}",
            obj.iter()
                .map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python's `repr()` of a `str`: single-quoted unless the text holds a
/// single quote and no double quote, with backslash escapes for the quote,
/// backslashes, and control characters. Operator-facing messages that the
/// Python implementation renders with `{value!r}` use this for parity.
pub fn py_str_repr(s: &str) -> String {
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
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn py_str_list(items: &[&str]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| py_str_repr(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn well_formed() -> Value {
        json!({"id": "demo", "goal": "Demo goal.", "graders": [{"type": "command", "run": "true"}]})
    }

    fn fully_briefed() -> Value {
        json!({
            "id": "full-demo",
            "goal": "Full demo.",
            "prerequisites": ["setup"],
            "tags": ["x"],
            "context": {
                "relevant": ["src/foo.py"],
                "references": ["src/bar.py"],
                "constraints": ["no new deps"],
                "non_goals": ["docs"],
                "edge_cases": ["empty input"],
                "notes": "see ADR",
            },
            "graders": [
                {"type": "command", "run": "true", "name": "tests"},
                {"type": "rubric", "assertions": ["does the thing"]},
                {"type": "transcript", "max_turns": 5},
                {"type": "manual", "instruction": "check it"},
            ],
        })
    }

    fn with(base: Value, key: &str, value: Value) -> Value {
        let mut base = base;
        base[key] = value;
        base
    }

    fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn err(result: Result<impl std::fmt::Debug, TaskLoadError>) -> String {
        result.unwrap_err().0
    }

    // ---------- load_task_file ----------

    #[test]
    fn load_task_file_returns_validated_task() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "t.json", &well_formed().to_string());
        let task = load_task_file(&p).unwrap();
        assert_eq!(task.id, "demo");
        assert_eq!(task.goal, "Demo goal.");
        assert!(matches!(&task.graders[0], Grader::Command(g) if g.run == "true"));
    }

    #[test]
    fn load_task_file_dispatches_each_grader_variant_and_ignores_prerequisites() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "full.json", &fully_briefed().to_string());
        let task = load_task_file(&p).unwrap();
        let types: Vec<_> = task
            .graders
            .iter()
            .map(|g| g.grader_type().as_str())
            .collect();
        assert_eq!(types, ["command", "rubric", "transcript", "manual"]);
        assert!(
            matches!(&task.graders[1], Grader::Rubric(g) if g.assertions == ["does the thing"])
        );
        assert!(matches!(&task.graders[2], Grader::Transcript(g) if g.max_turns == Some(5)));
        assert!(matches!(&task.graders[3], Grader::Manual(g) if g.instruction == "check it"));
        assert_eq!(task.context.relevant, ["src/foo.py"]);
        assert_eq!(task.context.notes, "see ADR");
        assert_eq!(task.tags, ["x"]);
    }

    #[test]
    fn load_task_file_unknown_grader_type_names_index() {
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "bogus", "run": "x"}]),
        );
        let msg = err(load_task_data(&payload, "S"));
        assert_eq!(
            msg,
            "S: graders[0] has unknown type 'bogus'; expected one of \
             ('command', 'rubric', 'manual', 'transcript')"
        );
        let payload = with(well_formed(), "graders", json!([{"type": null}]));
        assert!(err(load_task_data(&payload, "S")).contains("unknown type None;"));
        let payload = with(well_formed(), "graders", json!([{"type": "it's"}]));
        assert!(err(load_task_data(&payload, "S")).contains("unknown type \"it's\";"));
    }

    #[test]
    fn load_task_file_grader_field_violation_names_index() {
        let payload = with(well_formed(), "graders", json!([{"type": "command"}]));
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: graders[0] command grader requires a non-empty 'run'"
        );
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "command", "run": 5}]),
        );
        assert!(err(load_task_data(&payload, "S")).contains("non-empty 'run'"));
    }

    #[test]
    fn load_task_file_malformed_json_names_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "bad.json", "{not valid json");
        let msg = err(load_task_file(&p));
        assert!(msg.contains(&p.display().to_string()));
        assert!(msg.contains("invalid JSON"));
    }

    #[test]
    fn load_task_file_schema_violation_names_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "bad-schema.json",
            r#"{"goal": "bad graders", "graders": "nope"}"#,
        );
        let msg = err(load_task_file(&p));
        assert_eq!(
            msg,
            format!("{}: 'graders' must be a list, got str", p.display())
        );
    }

    #[test]
    fn load_task_file_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("missing.json");
        let msg = err(load_task_file(&p));
        assert!(msg.starts_with(&format!("{}: cannot read file: ", p.display())));
    }

    #[test]
    fn load_task_file_non_object_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "array.json", "[1, 2, 3]");
        assert_eq!(
            err(load_task_file(&p)),
            format!("{}: expected JSON object, got list", p.display())
        );
        assert_eq!(
            err(load_task_data(&json!(5), DEFAULT_DATA_SOURCE)),
            "<data>: expected JSON object, got int"
        );
        assert_eq!(
            err(load_task_data(&json!(null), "S")),
            "S: expected JSON object, got NoneType"
        );
    }

    // ---------- load_task_directory ----------

    #[test]
    fn load_task_directory_returns_all_json_tasks_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "b.json",
            &with(well_formed(), "id", json!("b")).to_string(),
        );
        write(
            dir.path(),
            "a.json",
            &with(well_formed(), "id", json!("a")).to_string(),
        );
        let ids: Vec<_> = load_task_directory(dir.path())
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn load_task_directory_ignores_non_json_entries() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.json",
            &with(well_formed(), "id", json!("a")).to_string(),
        );
        write(dir.path(), "notes.txt", "just text, not a task");
        write(dir.path(), "README.md", "# heading");
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        write(
            &dir.path().join("subdir"),
            "nested.json",
            &with(well_formed(), "id", json!("nested")).to_string(),
        );
        let ids: Vec<_> = load_task_directory(dir.path())
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn load_task_directory_empty_returns_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_task_directory(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn load_task_directory_malformed_member_names_offending_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "good.json", &well_formed().to_string());
        let bad = write(dir.path(), "bad.json", "{not json");
        assert!(err(load_task_directory(dir.path())).contains(&bad.display().to_string()));
    }

    #[test]
    fn load_task_directory_rejects_non_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "x.json", &well_formed().to_string());
        assert_eq!(
            err(load_task_directory(&p)),
            format!("{}: not a directory", p.display())
        );
    }

    // ---------- load_tasks_jsonl ----------

    #[test]
    fn load_tasks_jsonl_from_path_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "{}\n{}\n",
            with(well_formed(), "id", json!("a")),
            with(well_formed(), "id", json!("b"))
        );
        let p = write(dir.path(), "stream.jsonl", &text);
        let ids: Vec<_> = load_tasks_jsonl_file(&p)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn load_tasks_jsonl_from_reader() {
        let text = format!(
            "{}\r\n{}\n",
            with(well_formed(), "id", json!("a")),
            with(well_formed(), "id", json!("b"))
        );
        let tasks = load_tasks_jsonl_reader(text.as_bytes(), DEFAULT_STREAM_SOURCE).unwrap();
        let ids: Vec<_> = tasks.into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn load_tasks_jsonl_skips_blank_and_comment_lines() {
        let text = format!(
            "\n# a leading comment\n{}\n\n   \n# another comment\n{}\n\n",
            with(well_formed(), "id", json!("a")),
            with(well_formed(), "id", json!("b"))
        );
        let ids: Vec<_> = load_tasks_jsonl_str(&text, "s")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn load_tasks_jsonl_empty_or_only_comments_returns_empty_list() {
        assert!(load_tasks_jsonl_str("", "s").unwrap().is_empty());
        assert!(
            load_tasks_jsonl_str("# just\n# comments\n\n", "s")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn load_tasks_jsonl_malformed_line_names_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "{}\n{{not json on line two\n{}\n",
            with(well_formed(), "id", json!("a")),
            with(well_formed(), "id", json!("c"))
        );
        let p = write(dir.path(), "bad.jsonl", &text);
        let msg = err(load_tasks_jsonl_file(&p));
        assert!(
            msg.starts_with(&format!("{}:2: invalid JSON: ", p.display())),
            "{msg}"
        );
    }

    #[test]
    fn load_tasks_jsonl_schema_violation_names_line_number() {
        let text = format!(
            "{}\n{}\n",
            with(well_formed(), "id", json!("a")),
            json!({"goal": "x", "graders": "nope"})
        );
        assert_eq!(
            err(load_tasks_jsonl_str(&text, "src")),
            "src:2: 'graders' must be a list, got str"
        );
    }

    #[test]
    fn load_tasks_jsonl_stream_without_name_uses_stream_label() {
        let msg = err(load_tasks_jsonl_reader(
            "{not json\n".as_bytes(),
            DEFAULT_STREAM_SOURCE,
        ));
        assert!(msg.starts_with("<stream>:1: invalid JSON"), "{msg}");
    }

    // ---------- rubric grader optional fields ----------

    #[test]
    fn rubric_judge_model_and_retry_on_fail_parse_and_default() {
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "rubric", "assertions": ["does the thing"], "judge_model": "claude-haiku-4-5", "retry_on_fail": false}]),
        );
        let task = load_task_data(&payload, "S").unwrap();
        let Grader::Rubric(g) = &task.graders[0] else {
            panic!()
        };
        assert_eq!(g.judge_model.as_deref(), Some("claude-haiku-4-5"));
        assert!(!g.retry_on_fail);

        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "rubric", "assertions": ["does the thing"]}]),
        );
        let task = load_task_data(&payload, "S").unwrap();
        let Grader::Rubric(g) = &task.graders[0] else {
            panic!()
        };
        assert!(g.judge_model.is_none());
        assert!(g.retry_on_fail);
    }

    #[test]
    fn rubric_rejects_bad_optional_field_types() {
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "rubric", "assertions": ["a"], "retry_on_fail": "false"}]),
        );
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: graders[0] rubric grader 'retry_on_fail' must be a bool"
        );
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "rubric", "assertions": ["a"], "judge_model": 123}]),
        );
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: graders[0] rubric grader 'judge_model' must be a string or None"
        );
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "rubric", "assertions": "x"}]),
        );
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: graders[0] rubric grader requires a non-empty 'assertions' list"
        );
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "rubric", "assertions": ["a", 1]}]),
        );
        assert!(
            err(load_task_data(&payload, "S")).contains("'assertions' entries must all be strings")
        );
    }

    #[test]
    fn manual_and_transcript_shape_errors() {
        let payload = with(well_formed(), "graders", json!([{"type": "manual"}]));
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: graders[0] manual grader requires a non-empty 'instruction'"
        );
        let payload = with(well_formed(), "graders", json!([{"type": "transcript"}]));
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: graders[0] transcript grader requires at least one of max_turns, max_total_tokens, max_wall_seconds"
        );
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "transcript", "max_turns": "5"}]),
        );
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: graders[0] transcript grader 'max_turns' must be a non-negative integer or None"
        );
        let payload = with(
            well_formed(),
            "graders",
            json!([{"type": "transcript", "max_wall_seconds": 30}]),
        );
        let task = load_task_data(&payload, "S").unwrap();
        assert!(
            matches!(&task.graders[0], Grader::Transcript(g) if g.max_wall_seconds == Some(Seconds::Int(30)))
        );
        let payload = with(well_formed(), "graders", json!([5]));
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: graders[0] must be an object, got int"
        );
        let payload = with(well_formed(), "graders", json!(null));
        assert_eq!(
            err(load_task_data(&payload, "S")),
            "S: 'graders' must be a list, got NoneType"
        );
    }

    // ---------- serialize / deserialize / digest ----------

    fn minimal_task() -> Task {
        Task::new(
            "Demo goal.",
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id("demo")
    }

    fn briefed_task() -> Task {
        deserialize_task(&fully_briefed()).unwrap()
    }

    #[test]
    fn serialize_then_deserialize_round_trips() {
        let task = minimal_task();
        assert_eq!(deserialize_task(&serialize_task(&task)).unwrap(), task);
        let task = briefed_task();
        let restored = deserialize_task(&serialize_task(&task)).unwrap();
        assert_eq!(restored, task);
        let types: Vec<_> = restored.graders.iter().map(Grader::grader_type).collect();
        assert_eq!(
            types,
            [
                crate::task::GraderType::Command,
                crate::task::GraderType::Rubric,
                crate::task::GraderType::Transcript,
                crate::task::GraderType::Manual
            ]
        );
    }

    #[test]
    fn serialize_task_omits_none_optionals_and_default_budgets() {
        let v = serialize_task(&minimal_task());
        assert_eq!(
            v,
            json!({
                "id": "demo", "goal": "Demo goal.",
                "graders": [{"type": "command", "run": "true"}],
                "tags": [],
                "context": {"relevant": [], "references": [], "constraints": [], "non_goals": [], "edge_cases": [], "notes": ""}
            })
        );
    }

    #[test]
    fn task_digest_is_stable_and_ignores_id() {
        let task = briefed_task();
        assert_eq!(
            task_digest(&task),
            task_digest(&deserialize_task(&serialize_task(&task)).unwrap())
        );
        let a = minimal_task().with_id("one");
        let b = minimal_task().with_id("two");
        assert_eq!(task_digest(&a), task_digest(&b));
    }

    #[test]
    fn task_digest_changes_when_definition_changes() {
        let base = minimal_task();
        let digest = task_digest(&base);
        let mut t = base.clone();
        t.goal = "g2".into();
        assert_ne!(task_digest(&t), digest);
        let mut t = base.clone();
        t.graders = vec![Grader::Command(CommandGrader::new("false").unwrap())];
        assert_ne!(task_digest(&t), digest);
        let mut t = base.clone();
        t.tags = vec!["a".into()];
        assert_ne!(task_digest(&t), digest);
        let mut t = base.clone();
        t.context.notes = "changed".into();
        assert_ne!(task_digest(&t), digest);
    }

    /// Hard-coded hashes computed with the Python implementation
    /// (`flywheel_core.loaders.task_digest`) for the same definitions.
    #[test]
    fn task_digest_matches_python_byte_for_byte() {
        let minimal = load_task_data(&well_formed(), "S").unwrap();
        assert_eq!(
            task_digest(&minimal),
            "42506cb17aeab1f8f17c51806e57f935d141bfeeeb6465822c3c96f3d7cb355c"
        );

        let briefed = load_task_data(&fully_briefed(), "S").unwrap();
        assert_eq!(
            task_digest(&briefed),
            "91aca8e6961f7b6f6827b248030c75096e0ce922e6f5510af204fcdec65fd186"
        );

        let budgets = load_task_data(
            &json!({"id": "heavy", "goal": "g.", "graders": [{"type": "command", "run": "true"}],
                    "budgets": {"agent_iteration_seconds": 7200.5, "rubric_judge_max_turns": 64}}),
            "S",
        )
        .unwrap();
        assert_eq!(
            task_digest(&budgets),
            "ff6e0ab7dfff49a5607d59841146baaca7e8d8bc4a39698d5d226f433ad8953c"
        );

        let unicode = load_task_data(
            &json!({
                "id": "u",
                "goal": "caf\u{e9} \u{2014} \u{65e5}\u{672c} \u{1D11E} tab\there \"q\" \\ \u{7f} \u{01} /slash",
                "graders": [
                    {"type": "transcript", "max_wall_seconds": 1.5},
                    {"type": "transcript", "max_wall_seconds": 1e16},
                    {"type": "transcript", "max_wall_seconds": 0.00001, "max_total_tokens": 123456789012u64},
                    {"type": "transcript", "max_wall_seconds": 1e15},
                    {"type": "transcript", "max_wall_seconds": 123.456789},
                    {"type": "rubric", "assertions": ["a", "b"], "rubric": "r", "judge_model": "m", "retry_on_fail": false},
                ],
                "tags": ["z", "a"],
                "context": {"notes": ["p1", "p2"], "relevant": ["\u{fc}"]},
            }),
            "S",
        )
        .unwrap();
        assert_eq!(
            canonical_json(&Value::Object(definition_value(&unicode))),
            r#"{"context":{"constraints":[],"edge_cases":[],"non_goals":[],"notes":"p1\n\np2","references":[],"relevant":["\u00fc"]},"goal":"caf\u00e9 \u2014 \u65e5\u672c \ud834\udd1e tab\there \"q\" \\ \u007f \u0001 /slash","graders":[{"max_wall_seconds":1.5,"type":"transcript"},{"max_wall_seconds":1e+16,"type":"transcript"},{"max_total_tokens":123456789012,"max_wall_seconds":1e-05,"type":"transcript"},{"max_wall_seconds":1000000000000000.0,"type":"transcript"},{"max_wall_seconds":123.456789,"type":"transcript"},{"assertions":["a","b"],"judge_model":"m","retry_on_fail":false,"rubric":"r","type":"rubric"}],"tags":["z","a"]}"#
        );
        assert_eq!(
            task_digest(&unicode),
            "91c54d3e029d1a016522a4a8384d5421a1172291c4e20200b6affe64ae81ee11"
        );
    }

    #[test]
    fn python_float_repr_matches_cpython() {
        for (x, expected) in [
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (123456789.123, "123456789.123"),
            (1e22, "1e+22"),
            (2.5e300, "2.5e+300"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (1e100, "1e+100"),
            (9007199254740993.0, "9007199254740992.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (12345678901234567890.0, "1.2345678901234567e+19"),
            (f64::MAX, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (7200.5, "7200.5"),
            (-30.0, "-30.0"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(python_float_repr(x), expected, "{x:?}");
        }
    }

    #[test]
    fn canonical_json_sorts_keys_and_escapes_like_python() {
        let v = json!({"b": [1, 2.0, true, null], "a": {"z": "x", "y": "\u{8}\u{c}\n"}});
        assert_eq!(
            canonical_json(&v),
            r#"{"a":{"y":"\b\f\n","z":"x"},"b":[1,2.0,true,null]}"#
        );
    }

    // ---------- load_task_data / load_graders ----------

    #[test]
    fn load_task_data_errors_cite_the_source_label() {
        let task = load_task_data(
            &json!({"id": "from-data", "goal": "Build from a decoded payload.", "graders": [{"type": "command", "run": "true"}]}),
            "api-payload",
        )
        .unwrap();
        assert_eq!(task.id, "from-data");
        assert_eq!(
            err(load_task_data(
                &json!({"goal": "", "graders": []}),
                "issue#42"
            )),
            "issue#42: goal must be a non-empty string"
        );
        assert_eq!(
            err(load_task_data(&json!({"goal": 5}), "S")),
            "S: goal must be a non-empty string"
        );
        assert_eq!(
            err(load_task_data(&json!({"goal": "g", "id": null}), "S")),
            "S: id must be a non-empty string"
        );
        assert_eq!(
            err(load_task_data(&json!({"goal": "g", "id": 5}), "S")),
            "S: id must be a non-empty string"
        );
        assert_eq!(
            err(load_task_data(&json!({"goal": "g", "id": "a b"}), "S")),
            "S: id \"a b\" must not contain whitespace"
        );
        let task = load_task_data(&json!({"goal": "g"}), "S").unwrap();
        assert!(task.id.starts_with("task-"));
    }

    #[test]
    fn load_graders_applies_grader_validation() {
        let graders = load_graders(
            &json!([{"type": "command", "run": "uv run pytest"}, {"type": "transcript", "max_turns": 9}]),
            "policy",
        )
        .unwrap();
        assert_eq!(graders.len(), 2);
        assert!(matches!(graders[0], Grader::Command(_)));
        let msg = err(load_graders(&json!([{"type": "vibes"}]), "policy"));
        assert!(msg.starts_with("policy: graders[0] has unknown type 'vibes'"));
        assert_eq!(
            err(load_graders(&json!({"type": "command"}), "policy")),
            "policy: 'graders' must be a list, got dict"
        );
    }

    #[test]
    fn string_tags_and_context_fields_rejected_not_exploded() {
        assert_eq!(
            err(load_task_data(
                &json!({"goal": "g", "graders": [], "tags": "http"}),
                "S"
            )),
            "S: 'tags' must be a list of strings, got str"
        );
        assert_eq!(
            err(load_task_data(
                &json!({"goal": "g", "graders": [], "tags": ["a", 1]}),
                "S"
            )),
            "S: 'tags' must be a list of strings, got int at index 1"
        );
        assert_eq!(
            err(load_task_data(
                &json!({"goal": "g", "graders": [], "context": {"relevant": "src/foo.py"}}),
                "S"
            )),
            "S: 'context.relevant' must be a list of strings, got str"
        );
        assert_eq!(
            err(load_task_data(&json!({"goal": "g", "context": "x"}), "S")),
            "S: 'context' must be an object, got str"
        );
        assert_eq!(
            err(load_task_data(&json!({"goal": "g", "context": [1]}), "S")),
            "S: 'context' must be an object, got list"
        );
    }

    #[test]
    fn falsy_context_reads_as_empty() {
        for falsy in [
            json!(null),
            json!(0),
            json!([]),
            json!({}),
            json!(""),
            json!(false),
        ] {
            let task = load_task_data(&json!({"goal": "g", "context": falsy}), "S").unwrap();
            assert!(task.context.is_empty());
        }
        let task = load_task_data(
            &json!({"goal": "g", "context": {"relevant": null, "notes": null}}),
            "S",
        )
        .unwrap();
        assert!(task.context.is_empty());
        let task = load_task_data(&json!({"goal": "g", "tags": null}), "S").unwrap();
        assert!(task.tags.is_empty());
    }

    #[test]
    fn invalid_notes_type_rejected_at_load_time() {
        assert_eq!(
            err(load_task_data(
                &json!({"goal": "g", "context": {"notes": 123}}),
                "S"
            )),
            "S: 'context.notes' must be a string or list of strings, got int"
        );
        assert_eq!(
            err(load_task_data(
                &json!({"goal": "g", "context": {"notes": {"a": 1}}}),
                "S"
            )),
            "S: 'context.notes' must be a string or list of strings, got dict"
        );
        assert_eq!(
            err(load_task_data(
                &json!({"goal": "g", "context": {"notes": ["ok", 5]}}),
                "S"
            )),
            "S: 'context.notes' list entries must all be strings"
        );
    }

    #[test]
    fn list_notes_is_joined_into_a_string() {
        let task = load_task_data(
            &json!({"goal": "g", "context": {"notes": ["first para", "second para"]}}),
            "S",
        )
        .unwrap();
        assert_eq!(task.context.notes, "first para\n\nsecond para");
    }

    // ---------- per-task budgets ----------

    #[test]
    fn budgets_parse_and_round_trip() {
        let data = json!({
            "id": "heavy", "goal": "Golden-record run that legitimately needs 2h.",
            "graders": [{"type": "command", "run": "true"}],
            "budgets": {"agent_iteration_seconds": 7200, "rubric_judge_max_turns": 64},
        });
        let task = load_task_data(&data, "S").unwrap();
        assert_eq!(
            task.budgets.agent_iteration_seconds,
            Some(Seconds::Int(7200))
        );
        assert_eq!(task.budgets.rubric_judge_max_turns, Some(64));
        assert_eq!(task.budgets.rubric_judge_seconds, None);
        let reloaded = deserialize_task(&serialize_task(&task)).unwrap();
        assert_eq!(reloaded, task);
        assert!(serialize_task(&task).get("budgets").is_some());
    }

    #[test]
    fn budgets_shape_and_range_errors() {
        let base =
            json!({"id": "t", "goal": "g.", "graders": [{"type": "command", "run": "true"}]});
        assert_eq!(
            err(load_task_data(
                &with(
                    base.clone(),
                    "budgets",
                    json!({"zzz": 2, "iteration_seconds": 7200})
                ),
                "S"
            )),
            "S: 'budgets' has unknown key(s) ['iteration_seconds', 'zzz']; valid keys: \
             ['agent_iteration_seconds', 'rubric_judge_seconds', 'rubric_judge_max_turns']"
        );
        assert_eq!(
            err(load_task_data(
                &with(base.clone(), "budgets", json!(null)),
                "S"
            )),
            "S: 'budgets' must be an object, got NoneType"
        );
        assert_eq!(
            err(load_task_data(
                &with(
                    base.clone(),
                    "budgets",
                    json!({"rubric_judge_max_turns": 0})
                ),
                "S"
            )),
            "S: budgets.rubric_judge_max_turns must be a positive integer"
        );
        assert_eq!(
            err(load_task_data(
                &with(
                    base.clone(),
                    "budgets",
                    json!({"rubric_judge_max_turns": 2.0})
                ),
                "S"
            )),
            "S: budgets.rubric_judge_max_turns must be a positive integer"
        );
        for bad in [json!("7200"), json!(true)] {
            assert_eq!(
                err(load_task_data(
                    &with(
                        base.clone(),
                        "budgets",
                        json!({"agent_iteration_seconds": bad})
                    ),
                    "S"
                )),
                "S: budgets.agent_iteration_seconds must be a number of seconds"
            );
        }
        assert_eq!(
            err(load_task_data(
                &with(base, "budgets", json!({"agent_iteration_seconds": -1})),
                "S"
            )),
            "S: budgets.agent_iteration_seconds must be >= 0 (0 opts this task out of the ceiling entirely)"
        );
    }

    #[test]
    fn budgets_absent_keeps_digest_and_serialization_stable() {
        let data =
            json!({"id": "t", "goal": "g.", "graders": [{"type": "command", "run": "true"}]});
        let task = load_task_data(&data, "S").unwrap();
        assert!(serialize_task(&task).get("budgets").is_none());
        let with_budgets = load_task_data(
            &with(
                data.clone(),
                "budgets",
                json!({"agent_iteration_seconds": 7200}),
            ),
            "S",
        )
        .unwrap();
        assert_ne!(task_digest(&task), task_digest(&with_budgets));
        assert_eq!(
            task_digest(&task),
            task_digest(&load_task_data(&data, "S").unwrap())
        );
    }
}
