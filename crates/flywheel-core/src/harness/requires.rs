//! Blocked-`requires` serialization, task-aware validation, and the
//! round-trip parser for the persisted `blocked_requires_json` column.
//!
//! The serialized shape mirrors the envelope parser's input shape (each
//! predicate as `{type, ...fields}`) so a round trip through the persisted
//! column reconstructs identical [`BlockedRequirement`] values. This is the
//! single chokepoint for that shape so the harness, the event payload, and
//! recheck-time parsing cannot drift.

use crate::envelope::BlockedRequirement;
use crate::task::{Grader, Task};
use crate::telemetry_file::{JsonStyle, python_json_dumps};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Render `requires` as a stable, JSON-ready list of objects (order
/// preserved).
pub fn serialize_requires(requires: &[BlockedRequirement]) -> Vec<Value> {
    requires
        .iter()
        .map(|req| match req {
            BlockedRequirement::CommandGrader { name } => {
                json!({"type": "command_grader", "name": name})
            }
            BlockedRequirement::FileExists { path, present } => {
                json!({"type": "file_exists", "path": path, "present": present})
            }
            BlockedRequirement::EnvVarSet { name } => json!({"type": "env_var_set", "name": name}),
        })
        .collect()
}

/// The `blocked_requires_json` column text for an already-serialized
/// predicate list: Python's `json.dumps(list)` byte-for-byte.
pub fn requires_json(serialized: &[Value]) -> String {
    python_json_dumps(
        &Value::Array(serialized.to_vec()),
        &JsonStyle::PYTHON_DEFAULT,
    )
}

/// The named graders of `task`, keyed by name (unnamed graders are skipped).
pub fn graders_by_name(task: &Task) -> BTreeMap<&str, &Grader> {
    task.graders
        .iter()
        .filter_map(|grader| grader.name().filter(|n| !n.is_empty()).map(|n| (n, grader)))
        .collect()
}

/// Return an error string when any predicate is unresolvable against
/// `task`; otherwise `None`.
///
/// The envelope parser cannot validate `command_grader` predicates in
/// isolation (it has no [`Task`]); a predicate that names a grader missing
/// from `task.graders`, or present but of the wrong type, is a protocol
/// failure.
pub fn validate_blocked_requires_against_task(
    requires: &[BlockedRequirement],
    task: &Task,
) -> Option<String> {
    let by_name = graders_by_name(task);
    for req in requires {
        if let BlockedRequirement::CommandGrader { name } = req {
            match by_name.get(name.as_str()) {
                None => {
                    return Some(format!(
                        "command_grader predicate references unknown grader name {name:?}"
                    ));
                }
                Some(Grader::Command(_)) => {}
                Some(other) => {
                    return Some(format!(
                        "command_grader predicate {name:?} resolves to a {:?} grader, not 'command'",
                        other.grader_type().as_str()
                    ));
                }
            }
        }
    }
    None
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn non_empty_string(entry: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Round-trip the persisted `blocked_requires_json` back into typed
/// predicates, or return an error string describing the corruption.
pub fn parse_blocked_requires_json(payload: &str) -> Result<Vec<BlockedRequirement>, String> {
    let data: Value = serde_json::from_str(payload)
        .map_err(|err| format!("blocked_requires_json is not valid JSON: {err}"))?;
    let Value::Array(entries) = data else {
        return Err(format!(
            "blocked_requires_json must decode to a list, got {}",
            json_type_name(&data)
        ));
    };
    let mut parsed = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Value::Object(entry) = entry else {
            return Err(format!(
                "blocked_requires_json[{index}] is not a JSON object (got {})",
                json_type_name(entry)
            ));
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("command_grader") => {
                let name = non_empty_string(entry, "name").ok_or_else(|| {
                    format!(
                        "blocked_requires_json[{index}] command_grader 'name' must be a non-empty string"
                    )
                })?;
                parsed.push(BlockedRequirement::CommandGrader { name });
            }
            Some("file_exists") => {
                let path = non_empty_string(entry, "path").ok_or_else(|| {
                    format!(
                        "blocked_requires_json[{index}] file_exists 'path' must be a non-empty string"
                    )
                })?;
                let present = match entry.get("present") {
                    None => true,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => {
                        return Err(format!(
                            "blocked_requires_json[{index}] file_exists 'present' must be a bool"
                        ));
                    }
                };
                parsed.push(BlockedRequirement::FileExists { path, present });
            }
            Some("env_var_set") => {
                let name = non_empty_string(entry, "name").ok_or_else(|| {
                    format!(
                        "blocked_requires_json[{index}] env_var_set 'name' must be a non-empty string"
                    )
                })?;
                parsed.push(BlockedRequirement::EnvVarSet { name });
            }
            other => {
                let rendered = match entry.get("type") {
                    Some(Value::String(s)) => format!("{s:?}"),
                    Some(value) => value.to_string(),
                    None => "None".to_owned(),
                };
                let _ = other;
                return Err(format!(
                    "blocked_requires_json[{index}] has unknown 'type' {rendered}"
                ));
            }
        }
    }
    Ok(parsed)
}
