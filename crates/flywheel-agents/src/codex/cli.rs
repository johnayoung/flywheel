//! Codex CLI transport: headless `codex exec --json`.
//!
//! Works under any execution host. The argv shape mirrors the claude-code
//! text mode: the prompt is piped on stdin (the positional prompt argument
//! `-` tells `codex exec` to read it from stdin), dodging ARG_MAX.
//!
//! Capability honesty: `codex exec` has no turn-cap flag, so
//! `AgentConfiguration.max_turns` is ignored by [`build_cli_plan`]; the
//! wall-clock ceiling on the run request is the bound.

use crate::adapter::{AdapterServices, RunningAgent};
use crate::config::{AuthenticationPolicy, PermissionPolicy};
use crate::errors::AgentError;
use crate::events::{AgentEvent, EventType};
use crate::fold::number_as_i64;
use crate::hosts::{ProcessPlan, StdinPlan};
use crate::jsonl::{
    JsonLinesAgent, JsonLinesSettings, LineNormalizer, LineOutcome, parse_object_line,
    raw_object_event,
};
use crate::models::{RunRequest, StopReason, Usage};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub const OPENAI_API_KEY: &str = "OPENAI_API_KEY";

const ITEM_ENVELOPES: [&str; 3] = ["item.started", "item.updated", "item.completed"];

/// Build the headless `codex exec --json` invocation for one run.
pub fn build_cli_plan(request: &RunRequest) -> Result<ProcessPlan, AgentError> {
    let cfg = &request.configuration;
    let mut argv: Vec<String> = cfg
        .command_override
        .clone()
        .unwrap_or_else(|| vec!["codex".to_string()]);
    argv.extend(["exec", "--json"].map(String::from));
    if let Some(model) = cfg.model_id.as_deref().filter(|m| !m.is_empty()) {
        argv.extend(["--model".to_string(), model.to_string()]);
    }
    match cfg.permission_policy {
        // flywheel already isolates the run via worktree/container; AUTO
        // maps to codex's bypass flag, the exact analog of claude's bypass.
        PermissionPolicy::Auto => {
            argv.push("--dangerously-bypass-approvals-and-sandbox".to_string())
        }
        PermissionPolicy::Supervised => {
            argv.extend(["--sandbox", "workspace-write"].map(String::from))
        }
        PermissionPolicy::ReadOnly => argv.extend(["--sandbox", "read-only"].map(String::from)),
        PermissionPolicy::Plan => {
            return Err(AgentError::UnsupportedCapability(
                "codex has no plan mode; use read-only".into(),
            ));
        }
    }
    argv.extend(cfg.option_string_list("extra_cli_args").unwrap_or_default());
    if cfg.option_bool("skip_git_repo_check", false) {
        argv.push("--skip-git-repo-check".to_string());
    }
    // The positional prompt "-" tells codex exec to read the prompt on stdin.
    argv.push("-".to_string());
    let denied_environment = if cfg.authentication_policy == AuthenticationPolicy::AccountOnly {
        vec![OPENAI_API_KEY.to_string()]
    } else {
        Vec::new()
    };
    Ok(ProcessPlan {
        argv,
        cwd: Some(request.working_directory.clone()),
        environment: cfg.environment.clone(),
        stdin: StdinPlan::Payload(request.prompt.clone()),
        denied_environment,
    })
}

/// Project codex usage onto flywheel's four canonical counters.
///
/// `None` when the native usage mapping is absent (the payload then omits
/// its `usage` key). Codex's `input_tokens` INCLUDES its
/// `cached_input_tokens` (codex's own `total_tokens` is input + output),
/// whereas flywheel's canonical counters are disjoint (Claude semantics:
/// `input_tokens` excludes cache reads, and the four keys sum to the run
/// total). So `cached_input_tokens` maps to `cache_read_input_tokens` and
/// `input_tokens` carries only the uncached remainder (saturating at 0).
/// Codex reports no cache-creation counter, so that key is always 0. Bools
/// are excluded.
fn coerce_usage(raw: Option<&Value>) -> Option<Usage> {
    let Some(Value::Object(map)) = raw else {
        return None;
    };
    let int = |key: &str| map.get(key).and_then(number_as_i64).unwrap_or(0);
    let cached = int("cached_input_tokens");
    let uncached = int("input_tokens").saturating_sub(cached).max(0);
    let mut usage = Usage::new();
    usage.insert("input_tokens".into(), uncached);
    usage.insert("output_tokens".into(), int("output_tokens"));
    usage.insert("cache_read_input_tokens".into(), cached);
    usage.insert("cache_creation_input_tokens".into(), 0);
    Some(usage)
}

/// Stateful `codex exec --json` JSONL normalizer.
///
/// Stateful because codex splits signals across envelopes: `thread.started`
/// carries the session id, item envelopes repeat one item id across
/// started/updated/completed (tool pairing and message dedupe key on it),
/// and the terminal `turn.completed`/`turn.failed` envelopes carry usage.
/// `num_turns` counts terminal turn envelopes seen; multiple terminal
/// envelopes each emit a `session.finished` event and the fold keeps the
/// last.
#[derive(Debug, Default)]
pub struct CodexStreamNormalizer {
    session_id: Option<String>,
    turns: i64,
    emitted_messages: HashSet<String>,
    started_tools: HashSet<String>,
}

impl CodexStreamNormalizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Normalize one stdout line. Non-JSON, malformed, deeply nested, and
    /// non-object lines become RAW events (never discarded). Envelopes that
    /// carry no normalizable signal (e.g. `turn.started`) yield no events;
    /// the caller raw-records every envelope anyway.
    pub fn feed_line(&mut self, line: &str) -> LineOutcome {
        let obj = match parse_object_line(line) {
            Ok(None) => return LineOutcome::default(),
            Ok(Some(obj)) => obj,
            Err(()) => return LineOutcome::raw_line(line.trim()),
        };
        let native_type = obj.get("type").and_then(Value::as_str).map(str::to_string);
        match native_type.as_deref() {
            Some("thread.started") => {
                if let Some(sid) = obj.get("thread_id").and_then(Value::as_str) {
                    self.session_id = Some(sid.to_string());
                }
                LineOutcome::events(
                    native_type.clone(),
                    vec![AgentEvent::with_native(
                        EventType::SessionStarted,
                        "thread.started",
                        json!({"session_id": self.session_id}),
                    )],
                )
            }
            Some("turn.started") => LineOutcome::events(native_type, Vec::new()),
            Some("turn.completed") => self.turn_completed(&obj),
            Some("turn.failed") => self.turn_failed(&obj),
            Some("error") => LineOutcome::events(
                native_type.clone(),
                vec![AgentEvent::with_native(
                    EventType::Error,
                    "error",
                    json!({"message": obj.get("message").cloned().unwrap_or(Value::Null)}),
                )],
            ),
            Some(kind) if ITEM_ENVELOPES.contains(&kind) => {
                let events = self.item(&obj, kind);
                LineOutcome::events(native_type, events)
            }
            _ => LineOutcome::events(
                native_type.clone(),
                vec![raw_object_event(&obj, native_type.as_deref())],
            ),
        }
    }

    fn item(&mut self, obj: &Map<String, Value>, native_type: &str) -> Vec<AgentEvent> {
        let raw = || vec![raw_object_event(obj, Some(native_type))];
        let Some(Value::Object(item)) = obj.get("item") else {
            return raw();
        };
        // The item's type key drifted across codex releases: accept both
        // "type" and "item_type" ("type" wins when both are present).
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| item.get("item_type").and_then(Value::as_str));
        let Some(item_type) = item_type else {
            return raw();
        };
        let item_id = item.get("id").and_then(Value::as_str).map(str::to_string);
        let completed = native_type == "item.completed";
        let status_failed = item.get("status").and_then(Value::as_str) == Some("failed");
        let started = |id: &str, name: &str, input: Value| {
            AgentEvent::with_native(
                EventType::ToolCallStarted,
                native_type,
                json!({"tool_use_id": id, "tool_name": name, "tool_input": input}),
            )
        };
        let finished = |id: &str, is_error: bool, content: Value| {
            AgentEvent::with_native(
                EventType::ToolCallFinished,
                native_type,
                json!({"tool_use_id": id, "is_error": is_error, "content": content}),
            )
        };
        match item_type {
            "agent_message" => {
                if !completed {
                    return Vec::new();
                }
                if let Some(id) = &item_id {
                    // Repeated completed envelope: never re-emit.
                    if !self.emitted_messages.insert(id.clone()) {
                        return Vec::new();
                    }
                }
                match item.get("text").and_then(Value::as_str) {
                    Some(text) => vec![AgentEvent::with_native(
                        EventType::AssistantMessage,
                        native_type,
                        json!({"text": text}),
                    )],
                    None => raw(),
                }
            }
            "reasoning" => {
                if !completed {
                    return Vec::new();
                }
                vec![AgentEvent::with_native(
                    EventType::Thought,
                    native_type,
                    json!({"thinking": item.get("text").cloned().unwrap_or(Value::Null)}),
                )]
            }
            "command_execution" => {
                let Some(id) = item_id else {
                    return raw();
                };
                let mut events = Vec::new();
                if self.started_tools.insert(id.clone()) {
                    events.push(started(
                        &id,
                        "command_execution",
                        json!({"command": item.get("command").cloned().unwrap_or(Value::Null)}),
                    ));
                }
                if completed {
                    let exit_code = item.get("exit_code").and_then(Value::as_i64);
                    let is_error = status_failed || !matches!(exit_code, Some(0) | None);
                    events.push(finished(
                        &id,
                        is_error,
                        item.get("aggregated_output")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ));
                }
                events
            }
            "file_change" => {
                if !completed {
                    return Vec::new();
                }
                let Some(id) = item_id else {
                    return raw();
                };
                let changes = match item.get("changes") {
                    Some(Value::Array(list)) => list.clone(),
                    _ => Vec::new(),
                };
                let mut events = vec![
                    started(&id, "file_change", json!({"changes": changes})),
                    finished(&id, status_failed, Value::Null),
                ];
                for change in changes.iter().filter_map(Value::as_object) {
                    events.push(AgentEvent::with_native(
                        EventType::FileChanged,
                        native_type,
                        json!({
                            "path": change.get("path").cloned().unwrap_or(Value::Null),
                            "kind": change.get("kind").cloned().unwrap_or(Value::Null),
                        }),
                    ));
                }
                events
            }
            "mcp_tool_call" => {
                if !completed {
                    return Vec::new();
                }
                let Some(id) = item_id else {
                    return raw();
                };
                let name = format!(
                    "mcp:{}.{}",
                    value_text(item.get("server")),
                    value_text(item.get("tool"))
                );
                vec![
                    started(&id, &name, json!({})),
                    finished(&id, status_failed, Value::Null),
                ]
            }
            "web_search" => {
                if !completed {
                    return Vec::new();
                }
                let Some(id) = item_id else {
                    return raw();
                };
                vec![
                    started(
                        &id,
                        "web_search",
                        json!({"query": item.get("query").cloned().unwrap_or(Value::Null)}),
                    ),
                    finished(&id, status_failed, Value::Null),
                ]
            }
            "todo_list" => {
                // Emit on updated AND completed; the fold keeps the latest.
                if native_type == "item.updated" || native_type == "item.completed" {
                    return vec![AgentEvent::with_native(
                        EventType::PlanUpdated,
                        native_type,
                        json!({"items": item.get("items").cloned().unwrap_or(Value::Null)}),
                    )];
                }
                Vec::new()
            }
            "error" => vec![AgentEvent::with_native(
                EventType::Error,
                native_type,
                json!({"message": item.get("message").cloned().unwrap_or(Value::Null)}),
            )],
            _ => raw(),
        }
    }

    fn finished_payload(
        &self,
        stop: StopReason,
        native: &str,
        is_error: bool,
        usage: Option<Usage>,
    ) -> Value {
        let mut payload = json!({
            "normalized_stop": stop.as_str(),
            "stop_reason": native,
            "subtype": Value::Null,
            "is_error": is_error,
            "num_turns": self.turns,
            "total_cost_usd": Value::Null,
            "result_text": Value::Null,
            "session_id": self.session_id,
        });
        if let Some(usage) = usage {
            payload["usage"] = json!(usage);
        }
        payload
    }

    fn turn_completed(&mut self, obj: &Map<String, Value>) -> LineOutcome {
        self.turns += 1;
        let payload = self.finished_payload(
            StopReason::Completed,
            "turn.completed",
            false,
            coerce_usage(obj.get("usage")),
        );
        LineOutcome {
            native_type: Some("turn.completed".into()),
            events: vec![AgentEvent::with_native(
                EventType::SessionFinished,
                "turn.completed",
                payload,
            )],
            control: None,
            finished: true,
        }
    }

    fn turn_failed(&mut self, obj: &Map<String, Value>) -> LineOutcome {
        self.turns += 1;
        let message = obj
            .get("error")
            .and_then(|e| e.get("message"))
            .cloned()
            .unwrap_or(Value::Null);
        let payload = self.finished_payload(
            StopReason::Error,
            "turn.failed",
            true,
            coerce_usage(obj.get("usage")),
        );
        LineOutcome {
            native_type: Some("turn.failed".into()),
            events: vec![
                AgentEvent::with_native(
                    EventType::Error,
                    "turn.failed",
                    json!({"message": message}),
                ),
                AgentEvent::with_native(EventType::SessionFinished, "turn.failed", payload),
            ],
            control: None,
            finished: true,
        }
    }
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

impl LineNormalizer for CodexStreamNormalizer {
    fn feed_line(&mut self, line: &str) -> LineOutcome {
        CodexStreamNormalizer::feed_line(self, line)
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Spawn `codex exec --json` under the services' host.
pub async fn start_cli_agent(
    request: &RunRequest,
    services: &AdapterServices,
) -> Result<Box<dyn RunningAgent>, AgentError> {
    let plan = build_cli_plan(request)?;
    let process = services.host.spawn(plan).await?;
    Ok(Box::new(JsonLinesAgent::new(
        process,
        services.recorder.clone(),
        CodexStreamNormalizer::new(),
        services.approval_resolver.clone(),
        JsonLinesSettings::default(),
    )))
}
