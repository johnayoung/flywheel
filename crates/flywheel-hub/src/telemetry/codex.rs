//! A Codex rollout's lines as telemetry events.
//!
//! Codex has no hooks: what a session does is read from the rollout the
//! hub already tails ([`crate::transcript`]). A line carries its own time,
//! so durations are measured between a call and its output, and a rollout
//! read again from the start records nothing twice (the recorder skips
//! lines older than the session's last event).

use super::claude::steps;
use super::event::{Draft, Outcome, What};
use super::summary;
use serde_json::Value;

/// The markers of a line worth parsing (a cheap filter before JSON).
const MARKERS: &[&[u8]] = &[
    b"\"function_call",
    b"\"custom_tool_call",
    b"\"local_shell_call",
    b"\"task_started\"",
    b"\"task_complete\"",
    b"\"turn_aborted\"",
    b"\"token_count\"",
    b"\"session_meta\"",
    b"\"turn_context\"",
    b"\"UserMessage\"",
    b"\"user_message\"",
];

/// Whether a rollout line may say something [`line`] reads.
pub fn relevant(bytes: &[u8]) -> bool {
    MARKERS
        .iter()
        .any(|marker| bytes.windows(marker.len()).any(|w| w == *marker))
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// What one rollout line says (`now`: the time of a line without one).
pub fn line(value: &Value, now: i64) -> Vec<Draft> {
    let ts = str_of(value, "timestamp")
        .and_then(super::ms_of)
        .unwrap_or(now);
    let draft = |what: What| Draft::new(ts, what);
    let payload = value.get("payload").unwrap_or(&Value::Null);
    match str_of(value, "type") {
        Some("session_meta") => vec![draft(What::SessionStarted {
            harness: "codex".to_owned(),
            conversation: str_of(payload, "id").map(str::to_owned),
            model: None,
            label: None,
            dir: None,
        })],
        Some("turn_context") => str_of(payload, "model")
            .map(|model| {
                draft(What::SessionStarted {
                    harness: "codex".to_owned(),
                    conversation: None,
                    model: Some(model.to_owned()),
                    label: None,
                    dir: None,
                })
            })
            .into_iter()
            .collect(),
        Some("event_msg") => event_msg(payload, ts),
        Some("response_item") => response_item(payload, ts),
        // Older rollouts write the item bare.
        _ => response_item(value, ts),
    }
}

fn event_msg(payload: &Value, ts: i64) -> Vec<Draft> {
    let draft = |what: What| Draft::new(ts, what);
    match str_of(payload, "type") {
        // The prompt's text follows in its own line and titles the turn.
        Some("task_started") => vec![draft(What::TurnStarted {
            title: String::new(),
        })],
        Some("task_complete") => vec![draft(What::TurnFinished {
            ending: str_of(payload, "last_agent_message").and_then(summary::ending),
        })],
        Some("turn_aborted") => vec![draft(What::TurnFinished { ending: None })],
        Some("user_message") => str_of(payload, "message")
            .map(|text| {
                draft(What::TurnStarted {
                    title: summary::text(text),
                })
            })
            .into_iter()
            .collect(),
        Some("item_completed") => {
            let item = payload.get("item").unwrap_or(&Value::Null);
            if str_of(item, "type") != Some("UserMessage") {
                return Vec::new();
            }
            let text: String = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| str_of(part, "text"))
                .collect::<Vec<_>>()
                .join(" ");
            vec![draft(What::TurnStarted {
                title: summary::text(&text),
            })]
        }
        Some("token_count") => token_count(payload, ts),
        _ => Vec::new(),
    }
}

fn token_count(payload: &Value, ts: i64) -> Vec<Draft> {
    let mut drafts = Vec::new();
    let info = payload.get("info").unwrap_or(&Value::Null);
    if let Some(total) = info.get("total_token_usage").filter(|t| t.is_object()) {
        let cached = count(total, "cached_input_tokens");
        drafts.push(Draft::new(
            ts,
            What::Usage {
                // Codex counts cached input inside its input.
                input: count(total, "input_tokens").saturating_sub(cached),
                output: count(total, "output_tokens"),
                cache_read: cached,
                cache_write: count(total, "cache_write_input_tokens"),
                cost_usd: None,
            },
        ));
    }
    let window = count(info, "model_context_window");
    if let Some(last) = info.get("last_token_usage").filter(|_| window > 0) {
        drafts.push(Draft::new(
            ts,
            What::ContextUsage {
                used: count(last, "input_tokens"),
                window,
                exact: true,
            },
        ));
    }
    let limits = payload.get("rate_limits").unwrap_or(&Value::Null);
    for slot in ["primary", "secondary"] {
        let Some(reading) = limits.get(slot).filter(|r| r.is_object()) else {
            continue;
        };
        let Some(used_pct) = reading.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let window = match count(reading, "window_minutes") {
            300 => "five_hour".to_owned(),
            10080 => "seven_day".to_owned(),
            0 => slot.to_owned(),
            minutes => format!("{minutes}m"),
        };
        drafts.push(Draft::new(
            ts,
            What::RateLimit {
                window,
                used_pct,
                resets_at: reading.get("resets_at").and_then(Value::as_i64),
            },
        ));
    }
    if str_of(limits, "rate_limit_reached_type").is_some() {
        drafts.push(Draft::new(
            ts,
            What::RateLimited {
                detail: str_of(limits, "rate_limit_reached_type").map(summary::text),
            },
        ));
    }
    drafts
}

fn response_item(item: &Value, ts: i64) -> Vec<Draft> {
    let draft = |what: What| Draft::new(ts, what);
    let Some(call_id) = str_of(item, "call_id") else {
        return Vec::new();
    };
    match str_of(item, "type") {
        Some("function_call") => {
            let name = str_of(item, "name").unwrap_or("tool");
            let arguments = item.get("arguments").unwrap_or(&Value::Null);
            let mut drafts = vec![draft(What::ToolStarted {
                id: call_id.to_owned(),
                name: name.to_owned(),
                summary: summary::tool(name, arguments),
                subagent: None,
            })];
            if name == "update_plan" {
                let parsed = match arguments {
                    Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
                    other => other.clone(),
                };
                drafts.push(draft(What::PlanSet {
                    source: "codex".to_owned(),
                    items: steps(parsed.get("plan"), &["step", "title"]),
                    note: str_of(&parsed, "explanation").map(summary::text),
                }));
            }
            drafts
        }
        Some("custom_tool_call") => {
            let name = str_of(item, "name").unwrap_or("tool");
            let input = item.get("input").unwrap_or(&Value::Null);
            let summary = match (name, input) {
                ("apply_patch", Value::String(patch)) => {
                    summary::tool(name, &serde_json::json!({ "input": patch }))
                }
                (_, Value::String(text)) => Some(summary::text(text)).filter(|s| !s.is_empty()),
                (_, other) => summary::tool(name, other),
            };
            vec![draft(What::ToolStarted {
                id: call_id.to_owned(),
                name: name.to_owned(),
                summary,
                subagent: None,
            })]
        }
        Some("local_shell_call") => vec![draft(What::ToolStarted {
            id: call_id.to_owned(),
            name: "shell".to_owned(),
            summary: item
                .get("action")
                .and_then(|action| summary::tool("shell", action)),
            subagent: None,
        })],
        Some("function_call_output" | "custom_tool_call_output" | "local_shell_call_output") => {
            let failed =
                exit_code(item.get("output").unwrap_or(&Value::Null)).is_some_and(|code| code != 0);
            vec![draft(What::ToolFinished {
                id: call_id.to_owned(),
                // The output names no tool: the reducer pairs it by id.
                name: String::new(),
                outcome: if failed { Outcome::Error } else { Outcome::Ok },
                duration_ms: None,
                error: failed.then(|| "exit code not 0".to_owned()),
                tokens: None,
            })]
        }
        _ => Vec::new(),
    }
}

/// The exit code a tool output reports (`"exit_code":N`, in its text or
/// its parts' texts).
fn exit_code(output: &Value) -> Option<i64> {
    fn in_text(text: &str) -> Option<i64> {
        // Escaped inside a JSON string, or bare.
        let at = text.find("exit_code")?;
        let rest = text[at + "exit_code".len()..].trim_start_matches(['\\', '"', ':', ' ']);
        let digits: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        digits.parse().ok()
    }
    match output {
        Value::String(text) => in_text(text),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| str_of(part, "text"))
            .find_map(in_text),
        other => other
            .get("metadata")
            .and_then(|m| m.get("exit_code"))
            .and_then(Value::as_i64),
    }
}
