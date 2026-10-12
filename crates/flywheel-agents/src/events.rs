//! Normalized and raw event models plus the sink seam.
//!
//! The normalization guarantee: every native event becomes either a known
//! normalized [`AgentEvent`] or an [`EventType::Raw`] event, never silently
//! discarded. Raw envelopes are delivered to the sink *before* the normalized
//! events derived from them.
//!
//! This crate persists nothing. Durable storage belongs to the caller via
//! [`EventSink`].
//!
//! `sequence` is runtime-assigned: one monotonic counter per run covering raw
//! and normalized events together. Adapters leave the default `0`; the
//! runtime re-stamps every event it forwards.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "session.finished")]
    SessionFinished,

    #[serde(rename = "message.assistant")]
    AssistantMessage,
    #[serde(rename = "message.assistant.delta")]
    AssistantDelta,
    #[serde(rename = "message.user")]
    UserMessage,
    #[serde(rename = "thought")]
    Thought,

    #[serde(rename = "tool.started")]
    ToolCallStarted,
    #[serde(rename = "tool.updated")]
    ToolCallUpdated,
    #[serde(rename = "tool.finished")]
    ToolCallFinished,

    #[serde(rename = "file.changed")]
    FileChanged,
    #[serde(rename = "command.started")]
    CommandStarted,
    #[serde(rename = "command.finished")]
    CommandFinished,

    #[serde(rename = "plan.updated")]
    PlanUpdated,

    #[serde(rename = "approval.requested")]
    ApprovalRequested,
    #[serde(rename = "approval.resolved")]
    ApprovalResolved,
    #[serde(rename = "permission.denied")]
    PermissionDenied,

    #[serde(rename = "context.usage")]
    ContextUsage,
    #[serde(rename = "rate.limited")]
    RateLimited,
    #[serde(rename = "mode.changed")]
    ModeChanged,

    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "raw")]
    Raw,
}

impl EventType {
    pub const fn as_str(self) -> &'static str {
        match self {
            EventType::SessionStarted => "session.started",
            EventType::SessionFinished => "session.finished",
            EventType::AssistantMessage => "message.assistant",
            EventType::AssistantDelta => "message.assistant.delta",
            EventType::UserMessage => "message.user",
            EventType::Thought => "thought",
            EventType::ToolCallStarted => "tool.started",
            EventType::ToolCallUpdated => "tool.updated",
            EventType::ToolCallFinished => "tool.finished",
            EventType::FileChanged => "file.changed",
            EventType::CommandStarted => "command.started",
            EventType::CommandFinished => "command.finished",
            EventType::PlanUpdated => "plan.updated",
            EventType::ApprovalRequested => "approval.requested",
            EventType::ApprovalResolved => "approval.resolved",
            EventType::PermissionDenied => "permission.denied",
            EventType::ContextUsage => "context.usage",
            EventType::RateLimited => "rate.limited",
            EventType::ModeChanged => "mode.changed",
            EventType::Warning => "warning",
            EventType::Error => "error",
            EventType::Raw => "raw",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Event payload: event-type-specific keys, documented on each normalizer.
pub type Payload = Map<String, Value>;

/// Coerce a JSON value into a payload map; non-objects become an empty map.
pub fn payload_from(value: Value) -> Payload {
    match value {
        Value::Object(map) => map,
        _ => Payload::new(),
    }
}

/// One normalized event.
///
/// `payload` keys are event-type specific. `native_type` preserves the
/// vendor's own event name so a consumer can always trace an event back to
/// its raw envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    #[serde(rename = "type")]
    pub kind: EventType,
    #[serde(default)]
    pub payload: Payload,
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_type: Option<String>,
    #[serde(default)]
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
}

impl AgentEvent {
    pub fn new(kind: EventType, payload: Value) -> Self {
        Self {
            kind,
            payload: payload_from(payload),
            source: String::new(),
            native_type: None,
            sequence: 0,
            timestamp: Utc::now(),
        }
    }

    pub fn with_native(kind: EventType, native_type: impl Into<String>, payload: Value) -> Self {
        let mut event = Self::new(kind, payload);
        event.native_type = Some(native_type.into());
        event
    }

    /// Payload accessor for a string-valued key.
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(Value::as_str)
    }
}

/// One native envelope, retained verbatim before normalization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawAgentEvent {
    pub adapter_id: String,
    /// `"jsonl"` for every shipped transport.
    pub protocol: String,
    /// `"stdout"`, `"stderr"`, `"stdin"`, or `"adapter"`.
    pub stream: String,
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_type: Option<String>,
    #[serde(default)]
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
}

/// Stamps one shared monotonic sequence across raw and normalized events
/// for a single run and forwards both to the sink.
pub struct RunRecorder {
    sink: Arc<dyn EventSink>,
    adapter_id: String,
    sequence: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for RunRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunRecorder")
            .field("adapter_id", &self.adapter_id)
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}

impl RunRecorder {
    pub fn new(sink: Arc<dyn EventSink>, adapter_id: impl Into<String>) -> Self {
        Self {
            sink,
            adapter_id: adapter_id.into(),
            sequence: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }

    fn next_sequence(&self) -> u64 {
        self.sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Record one native envelope. Adapters call this once per envelope,
    /// *before* yielding the normalized events derived from it.
    pub fn emit_raw(&self, protocol: &str, stream: &str, data: Value, native_type: Option<&str>) {
        let event = RawAgentEvent {
            adapter_id: self.adapter_id.clone(),
            protocol: protocol.to_string(),
            stream: stream.to_string(),
            data,
            native_type: native_type.map(str::to_string),
            sequence: self.next_sequence(),
            timestamp: Utc::now(),
        };
        self.sink.on_raw(&event);
    }

    /// Assign the next sequence and source, forward to the sink, return the
    /// stamped event.
    pub fn stamp(&self, mut event: AgentEvent) -> AgentEvent {
        event.sequence = self.next_sequence();
        event.source = self.adapter_id.clone();
        self.sink.on_event(&event);
        event
    }
}

/// Caller-supplied event receiver. Implementations must not panic.
pub trait EventSink: Send + Sync {
    fn on_raw(&self, event: &RawAgentEvent);
    fn on_event(&self, event: &AgentEvent);
}

/// Discards everything. The default when no sink is supplied.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn on_raw(&self, _event: &RawAgentEvent) {}
    fn on_event(&self, _event: &AgentEvent) {}
}

/// Accumulates events in memory. Cloneable so a test keeps a handle while the
/// runtime owns another; intended for tests and fixtures.
#[derive(Debug, Default, Clone)]
pub struct MemorySink {
    raw: Arc<Mutex<Vec<RawAgentEvent>>>,
    events: Arc<Mutex<Vec<AgentEvent>>>,
}

impl MemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn raw(&self) -> Vec<RawAgentEvent> {
        self.raw.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn events_of(&self, kind: EventType) -> Vec<AgentEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }
}

impl EventSink for MemorySink {
    fn on_raw(&self, event: &RawAgentEvent) {
        if let Ok(mut raw) = self.raw.lock() {
            raw.push(event.clone());
        }
    }

    fn on_event(&self, event: &AgentEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_type_serializes_to_dotted_names() {
        assert_eq!(
            serde_json::to_string(&EventType::ToolCallFinished).unwrap(),
            "\"tool.finished\""
        );
        assert_eq!(EventType::Raw.to_string(), "raw");
        let back: EventType = serde_json::from_str("\"rate.limited\"").unwrap();
        assert_eq!(back, EventType::RateLimited);
    }

    #[test]
    fn payload_from_non_object_is_empty() {
        assert!(payload_from(json!([1, 2])).is_empty());
        assert_eq!(payload_from(json!({"a": 1})).get("a"), Some(&json!(1)));
    }

    #[test]
    fn recorder_shares_one_sequence_across_raw_and_normalized() {
        let sink = MemorySink::new();
        let recorder = RunRecorder::new(Arc::new(sink.clone()), "fake");
        recorder.emit_raw("jsonl", "stdout", json!("line"), Some("assistant"));
        let stamped = recorder.stamp(AgentEvent::new(
            EventType::AssistantMessage,
            json!({"text": "hi"}),
        ));
        recorder.emit_raw("jsonl", "stdout", json!("line2"), None);
        assert_eq!(stamped.sequence, 2);
        assert_eq!(stamped.source, "fake");
        let raw = sink.raw();
        assert_eq!(raw[0].sequence, 1);
        assert_eq!(raw[0].native_type.as_deref(), Some("assistant"));
        assert_eq!(raw[1].sequence, 3);
        assert_eq!(sink.events()[0].sequence, 2);
    }

    #[test]
    fn memory_sink_accumulates_through_clones() {
        let sink = MemorySink::new();
        let handle = sink.clone();
        handle.on_event(&AgentEvent::new(
            EventType::Thought,
            json!({"thinking": "hm"}),
        ));
        assert_eq!(sink.events().len(), 1);
        assert_eq!(
            sink.events_of(EventType::Thought)[0].str_field("thinking"),
            Some("hm")
        );
        assert!(sink.events_of(EventType::Raw).is_empty());
    }
}
