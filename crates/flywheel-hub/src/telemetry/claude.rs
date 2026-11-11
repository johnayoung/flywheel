//! Claude Code's hooks and status line as telemetry events.
//!
//! Hooks say what the session does (prompts, tool calls with their
//! `tool_use_id` and `duration_ms`, subagents, permission prompts, the
//! plan tools); none of them carries the context window, the cost, or the
//! rate limits. Those come from the status line's input, the only place
//! Claude Code hands the 5-hour and 7-day gauges to another program.

use super::event::{Draft, ItemStatus, Outcome, PlanStep, WaitReason, What};
use super::summary;
use serde_json::Value;

/// The hook events the hub asks Claude Code for, beyond the status hooks
/// ([`crate::agent`]'s `CLAUDE_HOOKS`), for telemetry alone.
pub const EXTRA_HOOKS: &[&str] = &[
    "SubagentStart",
    "SubagentStop",
    "PermissionDenied",
    "PreCompact",
    "PostCompact",
    "TaskCreated",
    "TaskCompleted",
];

/// The pseudo-event the status line shim reports under.
pub const STATUS_LINE_EVENT: &str = "statusline";

/// The hub's own progress tool as Claude Code names it (see
/// [`super::mcp`]).
pub const PROGRESS_TOOL: &str = "mcp__flywheel__report_progress";

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn owned(value: &Value, key: &str) -> Option<String> {
    str_of(value, key).map(str::to_owned)
}

/// The steps of a plan declared as a list (`items`, each naming itself by
/// one of `titles`).
pub fn steps(items: Option<&Value>, titles: &[&str]) -> Vec<PlanStep> {
    items
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let title = match item {
                        Value::String(title) => title.as_str(),
                        other => titles.iter().find_map(|key| str_of(other, key))?,
                    };
                    let title = summary::text(title);
                    (!title.is_empty()).then(|| PlanStep {
                        title,
                        status: str_of(item, "status")
                            .map(ItemStatus::parse)
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What the hook `event` with `payload` (its stdin JSON) says, observed
/// at `ts`.
pub fn hook(event: &str, payload: &Value, ts: i64) -> Vec<Draft> {
    let agent = owned(payload, "agent_id");
    let draft = |what: What| Draft::new(ts, what).by(agent.clone());
    let main = |what: What| Draft::new(ts, what);
    let tool_name = str_of(payload, "tool_name").unwrap_or("tool");
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    // A call without an id still pairs, by its name (see the reducer).
    let call_id = || owned(payload, "tool_use_id").unwrap_or_else(|| format!("{tool_name}?"));
    match event {
        "SessionStart" => vec![main(What::SessionStarted {
            harness: "claude".to_owned(),
            conversation: owned(payload, "session_id"),
            model: owned(payload, "model"),
        })],
        "SessionEnd" => vec![main(What::SessionFinished {
            reason: owned(payload, "reason"),
        })],
        "UserPromptSubmit" => vec![main(What::TurnStarted {
            title: summary::text(str_of(payload, "prompt").unwrap_or_default()),
        })],
        "Stop" => vec![main(What::TurnFinished {})],
        "StopFailure" => {
            let error = owned(payload, "error").unwrap_or_else(|| "error".to_owned());
            let detail = str_of(payload, "error_details").map(summary::text);
            let mut drafts = Vec::new();
            if error == "rate_limit" {
                drafts.push(main(What::RateLimited {
                    detail: detail.clone(),
                }));
            }
            drafts.push(main(What::TurnFailed { error, detail }));
            drafts
        }
        "PreToolUse" => {
            let mut drafts = vec![draft(What::ToolStarted {
                id: call_id(),
                name: tool_name.to_owned(),
                summary: summary::tool(tool_name, input),
                subagent: owned(input, "subagent_type"),
            })];
            drafts.extend(plan_of(tool_name, input).map(draft));
            drafts
        }
        "PostToolUse" => vec![draft(What::ToolFinished {
            id: call_id(),
            name: tool_name.to_owned(),
            outcome: Outcome::Ok,
            duration_ms: payload.get("duration_ms").and_then(Value::as_u64),
            error: None,
            tokens: payload
                .get("tool_response")
                .and_then(|r| r.get("totalTokens"))
                .and_then(Value::as_u64),
        })],
        "PostToolUseFailure" => {
            let interrupted = payload.get("is_interrupt").and_then(Value::as_bool) == Some(true);
            vec![draft(What::ToolFinished {
                id: call_id(),
                name: tool_name.to_owned(),
                outcome: if interrupted {
                    Outcome::Interrupted
                } else {
                    Outcome::Error
                },
                duration_ms: payload.get("duration_ms").and_then(Value::as_u64),
                error: str_of(payload, "error").map(summary::text),
                tokens: None,
            })]
        }
        "PermissionDenied" => vec![draft(What::ToolFinished {
            id: call_id(),
            name: tool_name.to_owned(),
            outcome: Outcome::Denied,
            duration_ms: None,
            error: str_of(payload, "reason").map(summary::text),
            tokens: None,
        })],
        "PermissionRequest" => vec![draft(What::WaitStarted {
            reason: if tool_name == "AskUserQuestion" {
                WaitReason::Question
            } else {
                WaitReason::Permission
            },
            tool: Some(tool_name.to_owned()),
        })],
        "Notification" => match str_of(payload, "notification_type") {
            Some("permission_prompt") => vec![draft(What::WaitStarted {
                reason: WaitReason::Permission,
                tool: None,
            })],
            Some("elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input") => {
                vec![draft(What::WaitStarted {
                    reason: WaitReason::Question,
                    tool: None,
                })]
            }
            _ => Vec::new(),
        },
        // These name the subagent they are about; they are the main
        // agent's observations.
        "SubagentStart" => agent
            .clone()
            .map(|id| {
                main(What::SubagentStarted {
                    id,
                    kind: owned(payload, "agent_type"),
                })
            })
            .into_iter()
            .collect(),
        "SubagentStop" => agent
            .clone()
            .map(|id| main(What::SubagentFinished { id }))
            .into_iter()
            .collect(),
        "PreCompact" | "PostCompact" => vec![main(What::Compact {
            phase: if event == "PreCompact" { "pre" } else { "post" }.to_owned(),
            trigger: owned(payload, "trigger"),
        })],
        "TaskCreated" | "TaskCompleted" => owned(payload, "task_id")
            .map(|id| {
                draft(What::PlanItem {
                    source: "task".to_owned(),
                    id,
                    title: str_of(payload, "task_subject").map(summary::text),
                    status: Some(if event == "TaskCompleted" {
                        ItemStatus::Done
                    } else {
                        ItemStatus::Pending
                    }),
                })
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// The plan a plan tool's call declares.
fn plan_of(tool: &str, input: &Value) -> Option<What> {
    match tool {
        "TodoWrite" => Some(What::PlanSet {
            source: "todo".to_owned(),
            items: steps(input.get("todos"), &["content", "title"]),
            note: None,
        }),
        PROGRESS_TOOL => Some(What::PlanSet {
            source: "declared".to_owned(),
            items: steps(input.get("items"), &["title", "content", "step"]),
            note: str_of(input, "note").map(summary::text),
        }),
        "TaskUpdate" => Some(What::PlanItem {
            source: "task".to_owned(),
            id: owned(input, "taskId")?,
            title: str_of(input, "subject").map(summary::text),
            status: str_of(input, "status").map(ItemStatus::parse),
        }),
        _ => None,
    }
}

/// What a status line input says: the context window, the totals, the
/// rate-limit gauges, and the model when it changed.
pub fn status_line(payload: &Value, ts: i64) -> Vec<Draft> {
    let mut drafts = Vec::new();
    let num = |value: &Value, key: &str| value.get(key).and_then(Value::as_f64);
    if let Some(model) = payload.get("model").and_then(|m| owned(m, "id")) {
        drafts.push(Draft::new(
            ts,
            What::SessionStarted {
                harness: "claude".to_owned(),
                conversation: owned(payload, "session_id"),
                model: Some(model),
            },
        ));
    }
    let window = payload.get("context_window").unwrap_or(&Value::Null);
    if let (Some(pct), Some(size)) = (
        num(window, "used_percentage"),
        num(window, "context_window_size").filter(|s| *s > 0.0),
    ) {
        drafts.push(Draft::new(
            ts,
            What::ContextUsage {
                used: (pct.clamp(0.0, 100.0) / 100.0 * size).round() as u64,
                window: size as u64,
                exact: true,
            },
        ));
    }
    let cost = payload
        .get("cost")
        .and_then(|c| num(c, "total_cost_usd"))
        .filter(|c| c.is_finite() && *c >= 0.0);
    let input = num(window, "total_input_tokens");
    let output = num(window, "total_output_tokens");
    if cost.is_some() || input.is_some() || output.is_some() {
        drafts.push(Draft::new(
            ts,
            What::Usage {
                input: input.unwrap_or(0.0) as u64,
                output: output.unwrap_or(0.0) as u64,
                cache_read: 0,
                cache_write: 0,
                cost_usd: cost,
            },
        ));
    }
    if let Some(Value::Object(limits)) = payload.get("rate_limits") {
        for (name, reading) in limits {
            let Some(used_pct) = num(reading, "used_percentage").filter(|p| p.is_finite()) else {
                continue;
            };
            drafts.push(Draft::new(
                ts,
                What::RateLimit {
                    window: summary::text(name),
                    used_pct,
                    resets_at: reading.get("resets_at").and_then(Value::as_i64),
                },
            ));
        }
    }
    drafts
}

/// The line the shim prints when the user has no status line of their
/// own: the model, the context window, the cost.
pub fn default_line(payload: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(name) = payload
        .get("model")
        .and_then(|m| str_of(m, "display_name").or_else(|| str_of(m, "id")))
    {
        parts.push(name.to_owned());
    }
    if let Some(pct) = payload
        .get("context_window")
        .and_then(|w| w.get("used_percentage"))
        .and_then(Value::as_f64)
    {
        parts.push(format!("ctx {pct:.0}%"));
    }
    if let Some(cost) = payload
        .get("cost")
        .and_then(|c| c.get("total_cost_usd"))
        .and_then(Value::as_f64)
    {
        parts.push(format!("${cost:.2}"));
    }
    parts.join(" \u{b7} ")
}
