//! Fold a normalized event stream into a [`CompletedRun`].
//!
//! Adapters own vendor semantics by shaping event payloads; the folder only
//! aggregates. Payload keys the folder reads:
//!
//! - `session.started`: `session_id`
//! - `message.assistant`: `text`, `usage` (turn-cumulative token mapping)
//! - `context.usage`: `usage` (same shape; lets adapters report usage on
//!   assistant messages that carry no text block, e.g. tool-use-only turns)
//! - `tool.started`: `tool_use_id`, `tool_name`, `tool_input`
//! - `tool.finished`: `tool_use_id`, `is_error`, `content`
//! - `permission.denied`: `tool_name` (rest kept as detail)
//! - `rate.limited`: `resets_at_epoch` (rest kept as detail)
//! - `session.finished`: `normalized_stop` (a [`StopReason`] string,
//!   adapter-computed), `stop_reason` (native), `subtype`, `is_error`,
//!   `num_turns`, `total_cost_usd`, `usage` (authoritative when present),
//!   `result_text` (transcript fallback), `session_id`

use crate::events::{AgentEvent, EventType, Payload};
use crate::models::{
    AgentExit, AgentFault, CompletedRun, PermissionDenial, RateLimitInfo, RunFailure, StopInfo,
    StopReason, ToolInteraction, ToolResult, Usage,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Project a JSON object of numeric counters onto a [`Usage`] map. Bools and
/// non-numbers are skipped; floats are truncated.
pub fn usage_from(payload: &Payload, key: &str) -> Usage {
    let mut out = Usage::new();
    if let Some(Value::Object(raw)) = payload.get(key) {
        for (k, v) in raw {
            if let Some(n) = number_as_i64(v) {
                out.insert(k.clone(), n);
            }
        }
    }
    out
}

/// Integer view of a JSON number (floats truncate); `None` for anything else.
pub fn number_as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| i64::try_from(u).unwrap_or(i64::MAX)))
            .or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Float view of a JSON number; `None` for anything else (bools included).
pub fn number_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Accumulates one run's events. Single-threaded; feed in arrival order.
#[derive(Debug, Default)]
pub struct EventFolder {
    texts: Vec<String>,
    session_id: Option<String>,
    tool_order: Vec<String>,
    tool_calls: BTreeMap<String, (String, serde_json::Map<String, Value>)>,
    tool_results: BTreeMap<String, ToolResult>,
    denials: Vec<PermissionDenial>,
    rate_limits: Vec<RateLimitInfo>,
    assistant_usage: Usage,
    finished: Option<Payload>,
    event_count: u64,
}

impl EventFolder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, event: &AgentEvent) {
        self.event_count += 1;
        let payload = &event.payload;
        match event.kind {
            EventType::SessionStarted => {
                if let Some(sid) = payload.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(sid.to_string());
                }
            }
            EventType::AssistantMessage => {
                if let Some(text) = payload.get("text").and_then(Value::as_str) {
                    self.texts.push(text.to_string());
                }
                let usage = usage_from(payload, "usage");
                if !usage.is_empty() {
                    self.assistant_usage = usage;
                }
            }
            EventType::ContextUsage => {
                let usage = usage_from(payload, "usage");
                if !usage.is_empty() {
                    self.assistant_usage = usage;
                }
            }
            EventType::ToolCallStarted => {
                let id = payload.get("tool_use_id").and_then(Value::as_str);
                let name = payload.get("tool_name").and_then(Value::as_str);
                if let (Some(id), Some(name)) = (id, name) {
                    let input = match payload.get("tool_input") {
                        Some(Value::Object(map)) => map.clone(),
                        _ => serde_json::Map::new(),
                    };
                    self.tool_order.push(id.to_string());
                    self.tool_calls
                        .insert(id.to_string(), (name.to_string(), input));
                }
            }
            EventType::ToolCallFinished => {
                if let Some(id) = payload.get("tool_use_id").and_then(Value::as_str) {
                    self.tool_results.insert(
                        id.to_string(),
                        ToolResult {
                            tool_use_id: id.to_string(),
                            is_error: payload.get("is_error").and_then(Value::as_bool),
                            content: payload.get("content").cloned(),
                        },
                    );
                }
            }
            EventType::PermissionDenied => {
                self.denials.push(PermissionDenial {
                    tool_name: payload
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    detail: payload.clone(),
                });
            }
            EventType::RateLimited => {
                self.rate_limits.push(RateLimitInfo {
                    resets_at_epoch: payload.get("resets_at_epoch").and_then(number_as_f64),
                    status: payload
                        .get("status")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    limit_type: payload
                        .get("limit_type")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    detail: payload.clone(),
                });
            }
            EventType::SessionFinished => {
                if let Some(sid) = payload.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(sid.to_string());
                }
                self.finished = Some(payload.clone());
            }
            _ => {}
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    fn final_text(&self) -> String {
        if !self.texts.is_empty() {
            return self.texts.concat();
        }
        self.finished
            .as_ref()
            .and_then(|f| f.get("result_text"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    }

    fn stop(&self) -> StopInfo {
        let pending = self
            .tool_order
            .iter()
            .any(|id| !self.tool_results.contains_key(id));
        let Some(finished) = &self.finished else {
            return StopInfo {
                reason: StopReason::Unknown,
                pending_tool_use: pending,
                ..StopInfo::default()
            };
        };
        let is_error = finished
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let reason = finished
            .get("normalized_stop")
            .and_then(Value::as_str)
            .and_then(StopReason::parse)
            .unwrap_or(if is_error {
                StopReason::Error
            } else {
                StopReason::Unknown
            });
        StopInfo {
            reason,
            native: finished
                .get("stop_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            native_subtype: finished
                .get("subtype")
                .and_then(Value::as_str)
                .map(str::to_string),
            is_error,
            pending_tool_use: pending,
            finished: true,
        }
    }

    pub fn completed(
        &self,
        exit: AgentExit,
        failure: Option<RunFailure>,
        fault: Option<AgentFault>,
    ) -> CompletedRun {
        let empty = Payload::new();
        let finished = self.finished.as_ref().unwrap_or(&empty);
        let mut usage = usage_from(finished, "usage");
        if usage.is_empty() {
            usage = self.assistant_usage.clone();
        }
        let interactions = self
            .tool_order
            .iter()
            .filter_map(|id| {
                let (name, input) = self.tool_calls.get(id)?;
                Some(ToolInteraction {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    tool_input: input.clone(),
                    result: self.tool_results.get(id).cloned(),
                })
            })
            .collect();
        CompletedRun {
            final_text: self.final_text(),
            stop: self.stop(),
            usage: if usage.is_empty() { None } else { Some(usage) },
            total_cost_usd: finished.get("total_cost_usd").and_then(number_as_f64),
            num_turns: finished.get("num_turns").and_then(Value::as_i64),
            native_session_id: self.session_id.clone(),
            tool_interactions: interactions,
            permission_denials: self.denials.clone(),
            rate_limit_events: self.rate_limits.clone(),
            fault,
            failure,
            exit,
            event_count: self.event_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exit() -> AgentExit {
        AgentExit {
            returncode: Some(0),
            stderr_tail: None,
            duration_seconds: Some(0.1),
        }
    }

    fn event(kind: EventType, payload: Value) -> AgentEvent {
        AgentEvent::new(kind, payload)
    }

    fn finished(extra: Value) -> AgentEvent {
        let mut base = payload_base();
        if let Value::Object(map) = extra {
            for (k, v) in map {
                base.insert(k, v);
            }
        }
        AgentEvent::new(EventType::SessionFinished, Value::Object(base))
    }

    fn payload_base() -> Payload {
        let mut base = Payload::new();
        base.insert("normalized_stop".into(), json!("completed"));
        base.insert("subtype".into(), json!("success"));
        base.insert("is_error".into(), json!(false));
        base
    }

    #[test]
    fn pending_tool_use_true_without_result() {
        let mut folder = EventFolder::new();
        folder.feed(&event(
            EventType::ToolCallStarted,
            json!({"tool_use_id": "t-1", "tool_name": "Bash", "tool_input": {"command": "ls"}}),
        ));
        let run = folder.completed(exit(), None, None);
        assert!(run.stop.pending_tool_use);
        assert_eq!(run.stop.reason, StopReason::Unknown);
        assert_eq!(run.tool_interactions.len(), 1);
        assert!(run.tool_interactions[0].result.is_none());
    }

    #[test]
    fn tool_result_clears_pending() {
        let mut folder = EventFolder::new();
        folder.feed(&event(
            EventType::ToolCallStarted,
            json!({"tool_use_id": "t-1", "tool_name": "Bash", "tool_input": {"command": "ls"}}),
        ));
        folder.feed(&event(
            EventType::ToolCallFinished,
            json!({"tool_use_id": "t-1", "is_error": false, "content": "src tests"}),
        ));
        folder.feed(&finished(json!({})));
        let run = folder.completed(exit(), None, None);
        assert!(!run.stop.pending_tool_use);
        assert_eq!(run.stop.reason, StopReason::Completed);
        let interaction = &run.tool_interactions[0];
        assert_eq!(interaction.tool_name, "Bash");
        let result = interaction.result.as_ref().unwrap();
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.content, Some(json!("src tests")));
    }

    #[test]
    fn stop_unknown_without_finished() {
        let mut folder = EventFolder::new();
        folder.feed(&event(
            EventType::AssistantMessage,
            json!({"text": "working"}),
        ));
        let run = folder.completed(exit(), None, None);
        assert_eq!(run.stop.reason, StopReason::Unknown);
        assert!(!run.stop.is_error);
        assert!(!run.stop.pending_tool_use);
        assert!(!run.stop.finished);
    }

    #[test]
    fn usage_from_context_usage_when_finished_carries_none() {
        let usage = json!({"input_tokens": 10, "output_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
        let mut folder = EventFolder::new();
        folder.feed(&event(EventType::ContextUsage, json!({"usage": usage})));
        folder.feed(&finished(json!({})));
        let run = folder.completed(exit(), None, None);
        let got = run.usage.unwrap();
        assert_eq!(got["input_tokens"], 10);
        assert_eq!(got["output_tokens"], 2);
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn finished_usage_wins_over_context_usage() {
        let mut folder = EventFolder::new();
        folder.feed(&event(
            EventType::ContextUsage,
            json!({"usage": {"input_tokens": 10, "output_tokens": 2}}),
        ));
        folder.feed(&finished(
            json!({"usage": {"input_tokens": 99, "output_tokens": 44}}),
        ));
        let run = folder.completed(exit(), None, None);
        let got = run.usage.unwrap();
        assert_eq!(got["input_tokens"], 99);
        assert_eq!(got["output_tokens"], 44);
    }

    #[test]
    fn final_text_falls_back_to_result_text() {
        let mut folder = EventFolder::new();
        folder.feed(&finished(
            json!({"result_text": "from the result envelope"}),
        ));
        assert_eq!(
            folder.completed(exit(), None, None).final_text,
            "from the result envelope"
        );
    }

    #[test]
    fn assistant_text_wins_over_result_text() {
        let mut folder = EventFolder::new();
        folder.feed(&event(
            EventType::AssistantMessage,
            json!({"text": "real "}),
        ));
        folder.feed(&event(EventType::AssistantMessage, json!({"text": "text"})));
        folder.feed(&finished(json!({"result_text": "ignored fallback"})));
        assert_eq!(folder.completed(exit(), None, None).final_text, "real text");
    }

    #[test]
    fn permission_denied_and_rate_limited_accumulate() {
        let mut folder = EventFolder::new();
        folder.feed(&event(
            EventType::PermissionDenied,
            json!({"tool_name": "Bash"}),
        ));
        folder.feed(&event(EventType::PermissionDenied, json!({})));
        folder.feed(&event(
            EventType::RateLimited,
            json!({"resets_at_epoch": 123}),
        ));
        folder.feed(&event(EventType::RateLimited, json!({})));
        let run = folder.completed(exit(), None, None);
        assert_eq!(run.permission_denials.len(), 2);
        assert_eq!(run.permission_denials[0].tool_name.as_deref(), Some("Bash"));
        assert_eq!(run.permission_denials[1].tool_name, None);
        assert_eq!(run.rate_limit_events.len(), 2);
        assert_eq!(run.rate_limit_events[0].resets_at_epoch, Some(123.0));
        assert_eq!(run.rate_limit_events[1].resets_at_epoch, None);
    }

    #[test]
    fn session_id_from_started_and_finished() {
        let mut folder = EventFolder::new();
        folder.feed(&event(
            EventType::SessionStarted,
            json!({"session_id": "sess-a"}),
        ));
        assert_eq!(folder.session_id(), Some("sess-a"));
        folder.feed(&finished(json!({"session_id": "sess-b"})));
        assert_eq!(
            folder
                .completed(exit(), None, None)
                .native_session_id
                .as_deref(),
            Some("sess-b")
        );
    }

    #[test]
    fn event_count_counts_every_event() {
        let mut folder = EventFolder::new();
        folder.feed(&event(EventType::Raw, json!({"data": {"line": "garbage"}})));
        folder.feed(&event(EventType::Thought, json!({"thinking": "hm"})));
        folder.feed(&event(EventType::AssistantMessage, json!({"text": "hi"})));
        folder.feed(&event(
            EventType::ContextUsage,
            json!({"usage": {"input_tokens": 1}}),
        ));
        folder.feed(&finished(json!({})));
        assert_eq!(folder.completed(exit(), None, None).event_count, 5);
    }

    #[test]
    fn is_error_without_normalized_stop_maps_to_error() {
        let mut folder = EventFolder::new();
        folder.feed(&AgentEvent::new(
            EventType::SessionFinished,
            json!({"is_error": true, "subtype": "weird"}),
        ));
        let run = folder.completed(exit(), None, None);
        assert_eq!(run.stop.reason, StopReason::Error);
        assert!(run.stop.is_error);
        assert_eq!(run.stop.native_subtype.as_deref(), Some("weird"));
    }

    #[test]
    fn usage_from_skips_bools_and_truncates_floats() {
        let payload = payload_from(json!({"usage": {"a": true, "b": 2.9, "c": "x", "d": 4}}));
        let usage = usage_from(&payload, "usage");
        assert_eq!(usage.get("a"), None);
        assert_eq!(usage["b"], 2);
        assert_eq!(usage.get("c"), None);
        assert_eq!(usage["d"], 4);
    }

    use crate::events::payload_from;
}
