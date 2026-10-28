//! Claude Code CLI transport: headless `--print --output-format stream-json`.
//!
//! Works under any [`ExecutionHost`](crate::hosts::ExecutionHost): a local
//! process group or `docker exec` against a bind-mounted worktree.
//!
//! Two input modes, selected by adapter option `input_mode`:
//!
//! - `"stream-json"` (default): `--input-format stream-json`. The prompt is
//!   written as a `user` message, stdin stays open for the control protocol
//!   (interrupt, set_model, set_permission_mode, user-message injection,
//!   get_context_usage, inbound `can_use_tool` approvals), and stdin is
//!   closed once the terminal `result` envelope arrives so the CLI exits.
//! - `"text"`: the historical container command: prompt piped as plain text
//!   (`-p -`), stdin closed immediately. No mid-turn control.
//!
//! Adapter options consumed by the plan builder (all optional):
//! `suppress_coauthor` (bool, default true), `settings` (object or
//! file-path/JSON string), `sandbox_exec` (object), `hooks` (object, the
//! settings-file hook schema), `skills` (`"all"` default | list | null),
//! `allowed_tools`, `disallowed_tools`, `tools`, `setting_sources`,
//! `add_dirs`, `mcp_servers` (object, string, or a list of server names
//! selected from the working directory's `.mcp.json`), `mcp_strict`, `agents`
//! (object), `resume`, `session_id`, `fork_session`,
//! `continue_conversation`, `fallback_model`, `max_budget_usd`, `effort`,
//! `max_thinking_tokens`, `system_prompt`, `append_system_prompt`,
//! `include_partial_messages`, `include_hook_events`, `extra_cli_args`,
//! `control_timeout_seconds`, `approval_timeout_seconds`,
//! `exit_grace_seconds`.

use crate::adapter::{AdapterServices, RunningAgent};
use crate::claude_code::common::{coerce_usage, normalize_stop};
use crate::claude_code::control::{ClaudeControl, permission_mode_for};
use crate::config::{AgentConfiguration, AuthenticationPolicy, PermissionPolicy};
use crate::errors::AgentError;
use crate::events::{AgentEvent, EventType};
use crate::fold::{number_as_f64, number_as_i64};
use crate::hosts::{ProcessPlan, StdinPlan};
use crate::jsonl::{
    ControlChannel, ControlFrame, JsonLinesAgent, JsonLinesSettings, LineNormalizer, LineOutcome,
    parse_object_line, raw_object_event,
};
use crate::models::RunRequest;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    StreamJson,
    Text,
}

impl InputMode {
    pub fn from_configuration(cfg: &AgentConfiguration) -> Result<Self, AgentError> {
        match cfg.option_str("input_mode").unwrap_or("stream-json") {
            "stream-json" => Ok(InputMode::StreamJson),
            "text" => Ok(InputMode::Text),
            other => Err(AgentError::UnsupportedCapability(format!(
                "unknown claude-code input_mode {other:?} (expected stream-json or text)"
            ))),
        }
    }
}

fn option_duration(cfg: &AgentConfiguration, key: &str, default: Duration) -> Duration {
    cfg.option(key)
        .and_then(number_as_f64)
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(default)
}

/// The project MCP config file the CLI auto-loads from the working directory.
pub const PROJECT_MCP_CONFIG: &str = ".mcp.json";

/// Resolve `mcp_servers` given as a list of server *names* (the
/// `[sandbox.capabilities.mcp] servers` shape) against the project's
/// `.mcp.json` in `working_directory`, returning the selected
/// `mcpServers` entries keyed by name.
///
/// The selection is passed to the CLI inline via `--mcp-config`, so under
/// `mcp_strict` (`--strict-mcp-config`) exactly the named servers load. A
/// name the project file does not declare, a missing or malformed file, or
/// a non-string entry is a configuration error: an explicitly requested
/// server must never be dropped silently.
pub fn select_project_mcp_servers(
    working_directory: &std::path::Path,
    names: &[Value],
) -> Result<Map<String, Value>, AgentError> {
    let path = working_directory.join(PROJECT_MCP_CONFIG);
    let unusable = |detail: String| {
        AgentError::UnsupportedCapability(format!(
            "mcp_servers names MCP servers to load from {}, but {detail}",
            path.display()
        ))
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| unusable(format!("it cannot be read: {err}")))?;
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|err| unusable(format!("it is not valid JSON: {err}")))?;
    let Some(Value::Object(declared)) = parsed.get("mcpServers") else {
        return Err(unusable("it has no \"mcpServers\" object".to_string()));
    };
    let mut selected = Map::new();
    let mut missing = Vec::new();
    for name in names {
        let Some(name) = name.as_str().filter(|n| !n.is_empty()) else {
            return Err(unusable(format!(
                "the entry {name} is not a server name string"
            )));
        };
        match declared.get(name) {
            Some(server) => {
                selected.insert(name.to_string(), server.clone());
            }
            None => missing.push(name.to_string()),
        }
    }
    if !missing.is_empty() {
        let mut available: Vec<&str> = declared.keys().map(String::as_str).collect();
        available.sort_unstable();
        return Err(unusable(format!(
            "it does not declare {} (declared: {})",
            missing.join(", "),
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        )));
    }
    Ok(selected)
}

/// Build the headless stream-json invocation for one run.
pub fn build_cli_plan(request: &RunRequest) -> Result<ProcessPlan, AgentError> {
    let cfg = &request.configuration;
    let input_mode = InputMode::from_configuration(cfg)?;
    let mut argv: Vec<String> = cfg
        .command_override
        .clone()
        .unwrap_or_else(|| vec!["claude".to_string()]);
    argv.extend(["--print", "--verbose", "--output-format", "stream-json"].map(String::from));

    if let Some(model) = cfg.model_id.as_deref().filter(|m| !m.is_empty()) {
        argv.extend(["--model".to_string(), model.to_string()]);
    }
    if let Some(fallback) = cfg.option_str("fallback_model").filter(|m| !m.is_empty()) {
        argv.extend(["--fallback-model".to_string(), fallback.to_string()]);
    }
    if let Some(max_turns) = cfg.max_turns {
        argv.extend(["--max-turns".to_string(), max_turns.to_string()]);
    }
    if let Some(budget) = cfg.option("max_budget_usd").and_then(number_as_f64) {
        argv.extend(["--max-budget-usd".to_string(), budget.to_string()]);
    }
    // Headless --print has no TTY to approve tool use; AUTO maps to the
    // CLI's bypass mode. SUPERVISED is the CLI default, so no flag.
    match cfg.permission_policy {
        PermissionPolicy::Supervised => {}
        policy => {
            let mode = permission_mode_for(policy)?;
            argv.extend(["--permission-mode".to_string(), mode.to_string()]);
        }
    }
    argv.extend([
        "--add-dir".to_string(),
        request.working_directory.to_string_lossy().into_owned(),
    ]);
    for dir in cfg.option_string_list("add_dirs").unwrap_or_default() {
        argv.extend(["--add-dir".to_string(), dir]);
    }
    if let Some(settings) = build_settings_value(cfg)? {
        argv.extend(["--settings".to_string(), settings]);
    }

    let mut allowed_tools = cfg.option_string_list("allowed_tools").unwrap_or_default();
    let mut setting_sources = cfg.option_string_list("setting_sources");
    apply_skills_defaults(cfg, &mut allowed_tools, &mut setting_sources)?;
    if let Some(tools) = cfg.option_string_list("tools") {
        argv.extend(["--tools".to_string(), tools.join(",")]);
    }
    if !allowed_tools.is_empty() {
        argv.extend(["--allowedTools".to_string(), allowed_tools.join(",")]);
    }
    if let Some(disallowed) = cfg
        .option_string_list("disallowed_tools")
        .filter(|l| !l.is_empty())
    {
        argv.extend(["--disallowedTools".to_string(), disallowed.join(",")]);
    }
    if let Some(sources) = setting_sources.filter(|l| !l.is_empty()) {
        argv.push(format!("--setting-sources={}", sources.join(",")));
    }
    match cfg.option("mcp_servers") {
        Some(Value::Object(servers)) if !servers.is_empty() => {
            argv.extend([
                "--mcp-config".to_string(),
                json!({"mcpServers": servers}).to_string(),
            ]);
        }
        Some(Value::String(path)) if !path.is_empty() => {
            argv.extend(["--mcp-config".to_string(), path.clone()]);
        }
        Some(Value::Array(names)) if !names.is_empty() => {
            let selected = select_project_mcp_servers(&request.working_directory, names)?;
            argv.extend([
                "--mcp-config".to_string(),
                json!({"mcpServers": selected}).to_string(),
            ]);
        }
        _ => {}
    }
    if cfg.option_bool("mcp_strict", false) {
        argv.push("--strict-mcp-config".to_string());
    }
    if let Some(agents @ Value::Object(_)) = cfg.option("agents") {
        argv.extend(["--agents".to_string(), agents.to_string()]);
    }
    // Equals form: the CLI declares --resume with an optional value, so a
    // dash-leading value in two-token form would parse as a separate flag.
    if let Some(resume) = cfg.option_str("resume").filter(|r| !r.is_empty()) {
        argv.push(format!("--resume={resume}"));
    }
    if let Some(session_id) = cfg.option_str("session_id").filter(|s| !s.is_empty()) {
        argv.push(format!("--session-id={session_id}"));
    }
    if cfg.option_bool("fork_session", false) {
        argv.push("--fork-session".to_string());
    }
    if cfg.option_bool("continue_conversation", false) {
        argv.push("--continue".to_string());
    }
    if let Some(effort) = cfg
        .reasoning_id
        .as_deref()
        .or(cfg.option_str("effort"))
        .filter(|e| !e.is_empty())
    {
        argv.extend(["--effort".to_string(), effort.to_string()]);
    }
    if let Some(tokens) = cfg.option("max_thinking_tokens").and_then(number_as_i64) {
        argv.extend(["--max-thinking-tokens".to_string(), tokens.to_string()]);
    }
    if let Some(prompt) = cfg.option_str("system_prompt") {
        argv.extend(["--system-prompt".to_string(), prompt.to_string()]);
    }
    if let Some(prompt) = cfg.option_str("append_system_prompt") {
        argv.extend(["--append-system-prompt".to_string(), prompt.to_string()]);
    }
    if cfg.option_bool("include_partial_messages", false) {
        argv.push("--include-partial-messages".to_string());
    }
    if cfg.option_bool("include_hook_events", false) {
        argv.push("--include-hook-events".to_string());
    }
    argv.extend(cfg.option_string_list("extra_cli_args").unwrap_or_default());

    let stdin = match input_mode {
        InputMode::StreamJson => {
            argv.extend(["--input-format", "stream-json"].map(String::from));
            StdinPlan::Open
        }
        InputMode::Text => {
            argv.extend(["-p", "-"].map(String::from));
            StdinPlan::Payload(request.prompt.clone())
        }
    };
    let denied_environment = if cfg.authentication_policy == AuthenticationPolicy::AccountOnly {
        vec![ANTHROPIC_API_KEY.to_string()]
    } else {
        Vec::new()
    };
    Ok(ProcessPlan {
        argv,
        cwd: Some(request.working_directory.clone()),
        environment: cfg.environment.clone(),
        stdin,
        denied_environment,
    })
}

/// The `--settings` value: the caller's settings merged with co-author
/// suppression, `sandbox_exec`, and `hooks`.
pub fn build_settings_value(cfg: &AgentConfiguration) -> Result<Option<String>, AgentError> {
    let suppress = cfg.option_bool("suppress_coauthor", true);
    let sandbox = cfg
        .option("sandbox_exec")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty());
    let hooks = cfg
        .option("hooks")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty());
    let settings = cfg.option("settings");
    if !suppress && sandbox.is_none() && hooks.is_none() {
        return Ok(match settings {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.trim().is_empty() => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(object @ Value::Object(_)) => Some(object.to_string()),
            Some(other) => {
                return Err(AgentError::AgentStartup(format!(
                    "adapter option settings must be an object or a string, got {other}"
                )));
            }
        });
    }
    let mut merged = match settings {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(Value::String(s)) => load_settings_string(s)?,
        Some(other) => {
            return Err(AgentError::AgentStartup(format!(
                "adapter option settings must be an object or a string, got {other}"
            )));
        }
    };
    if suppress {
        merged.insert("includeCoAuthoredBy".to_string(), Value::Bool(false));
    }
    if let Some(sandbox) = sandbox {
        merged.insert("sandbox".to_string(), Value::Object(sandbox.clone()));
    }
    if let Some(hooks) = hooks {
        merged.insert("hooks".to_string(), Value::Object(hooks.clone()));
    }
    Ok(Some(Value::Object(merged).to_string()))
}

fn load_settings_string(raw: &str) -> Result<Map<String, Value>, AgentError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Map::new());
    }
    let text = if trimmed.starts_with('{') && trimmed.ends_with('}') {
        trimmed.to_string()
    } else {
        std::fs::read_to_string(trimmed).map_err(|err| {
            AgentError::AgentStartup(format!("settings file {trimmed:?} unreadable: {err}"))
        })?
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(AgentError::AgentStartup(
            "settings JSON must be an object".into(),
        )),
        Err(err) => Err(AgentError::AgentStartup(format!(
            "settings JSON invalid: {err}"
        ))),
    }
}

/// Skill names become `Skill(name)` permission rules; the rule syntax
/// cannot carry parentheses, commas, or whitespace.
fn validate_skill_name(name: &str) -> Result<(), AgentError> {
    if name.is_empty()
        || name
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | ','))
    {
        return Err(AgentError::AgentStartup(format!(
            "invalid skill name {name:?}"
        )));
    }
    Ok(())
}

/// Effective allowed tools and setting sources for skills: `"all"` (the
/// default) injects the bare `Skill` tool; a list injects `Skill(name)` per
/// entry; `null`/`false` disables. Either enabled form defaults
/// `setting_sources` to `user,project` so the CLI discovers installed skills.
fn apply_skills_defaults(
    cfg: &AgentConfiguration,
    allowed_tools: &mut Vec<String>,
    setting_sources: &mut Option<Vec<String>>,
) -> Result<(), AgentError> {
    let skills: Vec<String> = match cfg.option("skills") {
        None => vec!["Skill".to_string()],
        Some(Value::Null) | Some(Value::Bool(false)) => return Ok(()),
        Some(Value::String(s)) if s == "all" => vec!["Skill".to_string()],
        Some(Value::Array(items)) => {
            let mut rules = Vec::with_capacity(items.len());
            for item in items {
                let name = item.as_str().ok_or_else(|| {
                    AgentError::AgentStartup(format!("skills entries must be strings, got {item}"))
                })?;
                validate_skill_name(name)?;
                rules.push(format!("Skill({name})"));
            }
            rules
        }
        Some(other) => {
            return Err(AgentError::AgentStartup(format!(
                "adapter option skills must be \"all\", a list, or null, got {other}"
            )));
        }
    };
    for rule in skills {
        if !allowed_tools.contains(&rule) {
            allowed_tools.push(rule);
        }
    }
    if setting_sources.is_none() {
        *setting_sources = Some(vec!["user".to_string(), "project".to_string()]);
    }
    Ok(())
}

/// Stateful stream-json line normalizer.
///
/// Stateful because the CLI splits signals across envelopes: assistant
/// messages carry `stop_reason`/`usage` while the terminal `result`
/// envelope carries turns/cost/subtype; the `session.finished` payload
/// reunites them.
#[derive(Debug, Default)]
pub struct ClaudeStreamNormalizer {
    session_id: Option<String>,
    last_stop_reason: Option<String>,
}

impl ClaudeStreamNormalizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Normalize one stdout line. Non-JSON and malformed lines become RAW
    /// events (never discarded).
    pub fn feed_line(&mut self, line: &str) -> LineOutcome {
        let obj = match parse_object_line(line) {
            Ok(None) => return LineOutcome::default(),
            Ok(Some(obj)) => obj,
            Err(()) => return LineOutcome::raw_line(line.trim()),
        };
        let native_type = obj.get("type").and_then(Value::as_str).map(str::to_string);
        self.capture_session_id(&obj);
        match native_type.as_deref() {
            Some("system") => self.system(&obj),
            Some("assistant") => self.assistant(&obj),
            Some("user") => self.user(&obj),
            Some("result") => self.result(&obj),
            Some("rate_limit_event") => self.rate_limit(&obj),
            Some("stream_event") => self.stream_event(&obj),
            Some("control_response") => Self::control_response(&obj),
            Some("control_request") => Self::control_request(&obj),
            Some("control_cancel_request") => LineOutcome {
                native_type,
                control: obj.get("request_id").and_then(Value::as_str).map(|id| {
                    ControlFrame::CancelRequest {
                        request_id: id.to_string(),
                    }
                }),
                ..LineOutcome::default()
            },
            _ => LineOutcome::events(
                native_type.clone(),
                vec![raw_object_event(&obj, native_type.as_deref())],
            ),
        }
    }

    fn capture_session_id(&mut self, obj: &Map<String, Value>) {
        if let Some(sid) = obj
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.session_id = Some(sid.to_string());
        }
    }

    fn system(&mut self, obj: &Map<String, Value>) -> LineOutcome {
        if obj.get("subtype").and_then(Value::as_str) == Some("init") {
            return LineOutcome::events(
                Some("system".into()),
                vec![AgentEvent::with_native(
                    EventType::SessionStarted,
                    "system",
                    json!({"session_id": self.session_id, "model": obj.get("model").cloned().unwrap_or(Value::Null)}),
                )],
            );
        }
        LineOutcome::events(
            Some("system".into()),
            vec![raw_object_event(obj, Some("system"))],
        )
    }

    fn assistant(&mut self, obj: &Map<String, Value>) -> LineOutcome {
        let Some(Value::Object(message)) = obj.get("message") else {
            return LineOutcome::events(
                Some("assistant".into()),
                vec![raw_object_event(obj, Some("assistant"))],
            );
        };
        let mut events = Vec::new();
        if let Some(stop) = message.get("stop_reason").and_then(Value::as_str) {
            self.last_stop_reason = Some(stop.to_string());
        }
        if let Some(Value::Array(blocks)) = message.get("content") {
            for block in blocks.iter().filter_map(Value::as_object) {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            events.push(AgentEvent::with_native(
                                EventType::AssistantMessage,
                                "assistant",
                                json!({"text": text}),
                            ));
                        }
                    }
                    Some("thinking") => events.push(AgentEvent::with_native(
                        EventType::Thought,
                        "assistant",
                        json!({"thinking": block.get("thinking").cloned().unwrap_or(Value::Null)}),
                    )),
                    Some("tool_use") => {
                        let id = block.get("id").and_then(Value::as_str);
                        let name = block.get("name").and_then(Value::as_str);
                        if let (Some(id), Some(name)) = (id, name) {
                            let input = match block.get("input") {
                                Some(Value::Object(map)) => Value::Object(map.clone()),
                                _ => json!({}),
                            };
                            events.push(AgentEvent::with_native(
                                EventType::ToolCallStarted,
                                "assistant",
                                json!({"tool_use_id": id, "tool_name": name, "tool_input": input}),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        let usage = coerce_usage(message.get("usage"));
        if !usage.is_empty() {
            events.push(AgentEvent::with_native(
                EventType::ContextUsage,
                "assistant",
                json!({"usage": usage}),
            ));
        }
        if let Some(error) = obj.get("error").filter(|e| !e.is_null()) {
            events.push(AgentEvent::with_native(
                EventType::Error,
                "assistant",
                json!({"message": error.as_str().map(str::to_string).unwrap_or_else(|| error.to_string()), "error": error}),
            ));
        }
        if events.is_empty() {
            return LineOutcome::events(
                Some("assistant".into()),
                vec![raw_object_event(obj, Some("assistant"))],
            );
        }
        LineOutcome::events(Some("assistant".into()), events)
    }

    fn user(&mut self, obj: &Map<String, Value>) -> LineOutcome {
        let Some(Value::Object(message)) = obj.get("message") else {
            return LineOutcome::events(
                Some("user".into()),
                vec![raw_object_event(obj, Some("user"))],
            );
        };
        let mut events = Vec::new();
        if let Some(Value::Array(blocks)) = message.get("content") {
            for block in blocks.iter().filter_map(Value::as_object) {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                events.push(AgentEvent::with_native(
                    EventType::ToolCallFinished,
                    "user",
                    json!({
                        "tool_use_id": tool_use_id,
                        "is_error": block.get("is_error").and_then(Value::as_bool),
                        "content": block.get("content").cloned().unwrap_or(Value::Null),
                    }),
                ));
            }
        }
        if events.is_empty() {
            return LineOutcome::events(
                Some("user".into()),
                vec![raw_object_event(obj, Some("user"))],
            );
        }
        LineOutcome::events(Some("user".into()), events)
    }

    fn result(&mut self, obj: &Map<String, Value>) -> LineOutcome {
        let mut events = Vec::new();
        if let Some(Value::Array(denials)) = obj.get("permission_denials") {
            for denial in denials.iter().filter_map(Value::as_object) {
                events.push(AgentEvent::with_native(
                    EventType::PermissionDenied,
                    "result",
                    json!({
                        "tool_name": denial.get("tool_name").and_then(Value::as_str),
                        "denial": Value::Object(denial.clone()),
                    }),
                ));
            }
        }
        let subtype = obj.get("subtype").and_then(Value::as_str);
        let is_error = obj
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let native_stop = self.last_stop_reason.clone().or_else(|| {
            obj.get("stop_reason")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let mut payload = json!({
            "normalized_stop": normalize_stop(subtype, native_stop.as_deref(), is_error).as_str(),
            "stop_reason": native_stop,
            "subtype": subtype,
            "is_error": is_error,
            "num_turns": obj.get("num_turns").and_then(Value::as_i64),
            "total_cost_usd": obj.get("total_cost_usd").and_then(number_as_f64),
            "result_text": obj.get("result").and_then(Value::as_str),
            "session_id": self.session_id,
        });
        let usage = coerce_usage(obj.get("usage"));
        if !usage.is_empty() {
            payload["usage"] = json!(usage);
        }
        if let Some(errors @ Value::Array(_)) = obj.get("errors") {
            payload["errors"] = errors.clone();
        }
        if let Some(status) = obj.get("api_error_status").filter(|s| !s.is_null()) {
            payload["api_error_status"] = status.clone();
        }
        events.push(AgentEvent::with_native(
            EventType::SessionFinished,
            "result",
            payload,
        ));
        LineOutcome {
            native_type: Some("result".into()),
            events,
            control: None,
            finished: true,
        }
    }

    fn rate_limit(&mut self, obj: &Map<String, Value>) -> LineOutcome {
        let info = obj.get("rate_limit_info").cloned().unwrap_or(Value::Null);
        let resets_at = info.get("resetsAt").and_then(number_as_f64);
        LineOutcome::events(
            Some("rate_limit_event".into()),
            vec![AgentEvent::with_native(
                EventType::RateLimited,
                "rate_limit_event",
                json!({"resets_at_epoch": resets_at, "status": info.get("status").cloned().unwrap_or(Value::Null), "detail": info}),
            )],
        )
    }

    fn stream_event(&mut self, obj: &Map<String, Value>) -> LineOutcome {
        let event = obj.get("event").cloned().unwrap_or(Value::Null);
        if event.get("type").and_then(Value::as_str) != Some("content_block_delta") {
            return LineOutcome::events(
                Some("stream_event".into()),
                vec![raw_object_event(obj, Some("stream_event"))],
            );
        }
        let delta = event.get("delta").cloned().unwrap_or(Value::Null);
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        LineOutcome::events(
            Some("stream_event".into()),
            vec![AgentEvent::with_native(
                EventType::AssistantDelta,
                "stream_event",
                json!({
                    "delta_type": delta_type,
                    "text": delta.get("text").and_then(Value::as_str),
                    "thinking": delta.get("thinking").and_then(Value::as_str),
                    "index": event.get("index").cloned().unwrap_or(Value::Null),
                    "parent_tool_use_id": obj.get("parent_tool_use_id").cloned().unwrap_or(Value::Null),
                }),
            )],
        )
    }

    fn control_response(obj: &Map<String, Value>) -> LineOutcome {
        let response = obj.get("response").and_then(Value::as_object);
        let request_id = response
            .and_then(|r| r.get("request_id"))
            .and_then(Value::as_str);
        let control = match (response, request_id) {
            (Some(response), Some(request_id)) => {
                let result = if response.get("subtype").and_then(Value::as_str) == Some("error") {
                    Err(response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string())
                } else {
                    Ok(response.get("response").cloned().unwrap_or(json!({})))
                };
                Some(ControlFrame::Response {
                    request_id: request_id.to_string(),
                    result,
                })
            }
            _ => None,
        };
        let events = if control.is_none() {
            vec![raw_object_event(obj, Some("control_response"))]
        } else {
            Vec::new()
        };
        LineOutcome {
            native_type: Some("control_response".into()),
            events,
            control,
            finished: false,
        }
    }

    fn control_request(obj: &Map<String, Value>) -> LineOutcome {
        let request_id = obj.get("request_id").and_then(Value::as_str);
        let request = obj.get("request").and_then(Value::as_object);
        let subtype = request
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str);
        let (Some(request_id), Some(request), Some(subtype)) = (request_id, request, subtype)
        else {
            return LineOutcome::events(
                Some("control_request".into()),
                vec![raw_object_event(obj, Some("control_request"))],
            );
        };
        let events = if subtype == "can_use_tool" {
            Vec::new()
        } else {
            vec![raw_object_event(obj, Some("control_request"))]
        };
        LineOutcome {
            native_type: Some("control_request".into()),
            events,
            control: Some(ControlFrame::Request {
                request_id: request_id.to_string(),
                subtype: subtype.to_string(),
                request: request.clone(),
            }),
            finished: false,
        }
    }
}

impl LineNormalizer for ClaudeStreamNormalizer {
    fn feed_line(&mut self, line: &str) -> LineOutcome {
        ClaudeStreamNormalizer::feed_line(self, line)
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Spawn the CLI under the services' host and return the running agent.
pub async fn start_cli_agent(
    request: &RunRequest,
    services: &AdapterServices,
) -> Result<Box<dyn RunningAgent>, AgentError> {
    let plan = build_cli_plan(request)?;
    let cfg = &request.configuration;
    let settings = JsonLinesSettings {
        approval_timeout: option_duration(
            cfg,
            "approval_timeout_seconds",
            JsonLinesSettings::default().approval_timeout,
        ),
        exit_grace: option_duration(
            cfg,
            "exit_grace_seconds",
            JsonLinesSettings::default().exit_grace,
        ),
    };
    let control_timeout = option_duration(cfg, "control_timeout_seconds", DEFAULT_CONTROL_TIMEOUT);
    let interactive = plan.stdin == StdinPlan::Open;
    let process = services.host.spawn(plan).await?;
    let mut agent = JsonLinesAgent::new(
        process,
        services.recorder.clone(),
        ClaudeStreamNormalizer::new(),
        services.approval_resolver.clone(),
        settings,
    );
    if interactive {
        let Some(writer) = agent.process_stdin() else {
            return Err(AgentError::AgentStartup(
                "host did not expose the agent's stdin for stream-json input".into(),
            ));
        };
        let channel = Arc::new(ControlChannel::new(writer, control_timeout));
        channel.send_user_message(&request.prompt)?;
        services.recorder.emit_raw(
            crate::jsonl::PROTOCOL,
            "stdin",
            json!({"type": "user", "message": {"role": "user", "content": request.prompt}}),
            Some("user"),
        );
        let surface: Arc<dyn crate::adapter::AgentControl> =
            Arc::new(ClaudeControl::new(channel.clone()));
        agent = agent.with_control(channel, surface);
    }
    Ok(Box::new(agent))
}
