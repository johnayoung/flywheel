//! Shared JSON-lines transport: line-at-a-time normalization plus the
//! optional bidirectional control protocol.
//!
//! Both shipped adapters read newline-delimited JSON on stdout. A
//! [`LineNormalizer`] turns one line into normalized events (never dropping
//! anything: non-JSON lines become RAW); [`JsonLinesAgent`] drives a
//! [`RunningProcess`] through it, records every envelope raw before its
//! normalized events, and, when the plan kept stdin open, services the
//! control protocol: outbound `control_request` frames awaiting their
//! `control_response`, and inbound `control_request` frames (tool
//! permission prompts) answered through the approval resolver.

use crate::BoxFuture;
use crate::adapter::{
    AgentControl, ApprovalDecision, ApprovalRequest, ApprovalResolver, RunningAgent,
};
use crate::errors::AgentError;
use crate::events::{AgentEvent, EventType, RunRecorder};
use crate::hosts::{DEFAULT_KILL_GRACE, RunningProcess, StdinWriter};
use crate::models::AgentExit;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// Protocol tag recorded on every raw envelope of a JSON-lines transport.
pub const PROTOCOL: &str = "jsonl";

/// A control-protocol frame peeled off the output stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlFrame {
    /// Reply to one of our requests.
    Response {
        request_id: String,
        result: Result<Value, String>,
    },
    /// A request the agent is making of us (e.g. `can_use_tool`).
    Request {
        request_id: String,
        subtype: String,
        request: Map<String, Value>,
    },
    /// The agent withdrew an earlier request.
    CancelRequest { request_id: String },
}

/// What one stdout line normalized to.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LineOutcome {
    pub native_type: Option<String>,
    pub events: Vec<AgentEvent>,
    pub control: Option<ControlFrame>,
    /// The agent emitted its terminal envelope for this run.
    pub finished: bool,
}

impl LineOutcome {
    pub fn events(native_type: Option<String>, events: Vec<AgentEvent>) -> Self {
        Self {
            native_type,
            events,
            control: None,
            finished: false,
        }
    }

    pub fn raw_line(line: &str) -> Self {
        Self::events(None, vec![raw_line_event(line)])
    }
}

/// Stateful per-run normalizer for one vendor's line protocol.
pub trait LineNormalizer: Send {
    fn feed_line(&mut self, line: &str) -> LineOutcome;
    fn session_id(&self) -> Option<&str>;
}

/// RAW event for a line that was not a JSON object.
pub fn raw_line_event(line: &str) -> AgentEvent {
    AgentEvent::new(EventType::Raw, json!({"data": {"line": line}}))
}

/// RAW event for a JSON object the normalizer did not recognize.
pub fn raw_object_event(obj: &Map<String, Value>, native_type: Option<&str>) -> AgentEvent {
    let mut event = AgentEvent::new(EventType::Raw, json!({"data": Value::Object(obj.clone())}));
    event.native_type = native_type.map(str::to_string);
    event
}

/// Parse one line as a JSON object. Blank lines yield `Ok(None)`; anything
/// that is not a JSON object (including malformed or too deeply nested
/// JSON) yields `Err(())` so the caller records it RAW.
#[allow(clippy::result_unit_err)]
pub fn parse_object_line(line: &str) -> Result<Option<Map<String, Value>>, ()> {
    let stripped = line.trim();
    if stripped.is_empty() {
        return Ok(None);
    }
    if !stripped.starts_with('{') {
        return Err(());
    }
    match serde_json::from_str::<Value>(stripped) {
        Ok(Value::Object(obj)) => Ok(Some(obj)),
        _ => Err(()),
    }
}

/// Outbound control requests over the agent's stdin, matched to their
/// responses by request id.
pub struct ControlChannel {
    writer: StdinWriter,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    counter: AtomicU64,
    timeout: Duration,
    closed: AtomicBool,
}

impl std::fmt::Debug for ControlChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlChannel")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl ControlChannel {
    pub fn new(writer: StdinWriter, timeout: Duration) -> Self {
        Self {
            writer,
            pending: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            timeout,
            closed: AtomicBool::new(false),
        }
    }

    pub fn writer(&self) -> &StdinWriter {
        &self.writer
    }

    fn next_request_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        format!("req_{n}_{}", &nonce[..8])
    }

    /// Send `{"type":"control_request",...}` and await the matching
    /// `control_response`, which the agent's reader delivers via
    /// [`ControlChannel::resolve`].
    pub async fn request(&self, request: Value) -> Result<Value, AgentError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(AgentError::ControlRequest(
                "agent control channel is closed".into(),
            ));
        }
        let subtype = request
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let request_id = self.next_request_id();
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request_id.clone(), tx);
        }
        let frame =
            json!({"type": "control_request", "request_id": request_id, "request": request});
        if let Err(err) = self.writer.write_line(frame.to_string()) {
            self.remove_pending(&request_id);
            return Err(AgentError::ControlRequest(format!(
                "could not send {subtype}: {err}"
            )));
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(AgentError::ControlRequest(format!(
                "{subtype} rejected: {message}"
            ))),
            Ok(Err(_)) => Err(AgentError::ControlRequest(format!(
                "agent exited before answering {subtype}"
            ))),
            Err(_) => {
                self.remove_pending(&request_id);
                Err(AgentError::ControlRequest(format!(
                    "control request timeout: {subtype} ({}s)",
                    self.timeout.as_secs_f64()
                )))
            }
        }
    }

    fn remove_pending(&self, request_id: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(request_id);
        }
    }

    /// Deliver a `control_response` frame to its waiter.
    pub fn resolve(&self, request_id: &str, result: Result<Value, String>) {
        let waiter = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(request_id));
        if let Some(tx) = waiter {
            let _ = tx.send(result);
        }
    }

    /// Fail every outstanding request (the stream ended).
    pub fn close(&self, reason: &str) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut pending) = self.pending.lock() {
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(reason.to_string()));
            }
        }
    }

    /// Answer an inbound control request.
    pub fn respond(
        &self,
        request_id: &str,
        result: Result<Value, String>,
    ) -> Result<(), AgentError> {
        let frame = match result {
            Ok(response) => json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id, "response": response},
            }),
            Err(error) => json!({
                "type": "control_response",
                "response": {"subtype": "error", "request_id": request_id, "error": error},
            }),
        };
        self.writer.write_line(frame.to_string())
    }

    /// Queue a user message on the live conversation.
    pub fn send_user_message(&self, text: &str) -> Result<(), AgentError> {
        let frame = json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "parent_tool_use_id": Value::Null,
            "session_id": "default",
        });
        self.writer.write_line(frame.to_string())
    }
}

/// Tunables for one [`JsonLinesAgent`].
#[derive(Debug, Clone)]
pub struct JsonLinesSettings {
    /// How long an approval resolver may take before the request is denied.
    pub approval_timeout: Duration,
    /// After the terminal envelope, how long the agent may take to exit on
    /// its own before its process group is terminated.
    pub exit_grace: Duration,
}

impl Default for JsonLinesSettings {
    fn default() -> Self {
        Self {
            approval_timeout: Duration::from_secs(60),
            exit_grace: Duration::from_secs(30),
        }
    }
}

/// A running agent over a JSON-lines process.
pub struct JsonLinesAgent<N: LineNormalizer> {
    process: Box<dyn RunningProcess>,
    recorder: Arc<RunRecorder>,
    normalizer: N,
    queue: VecDeque<AgentEvent>,
    control: Option<Arc<ControlChannel>>,
    control_surface: Option<Arc<dyn AgentControl>>,
    approval_resolver: ApprovalResolver,
    settings: JsonLinesSettings,
    finished: bool,
    eof: bool,
}

impl<N: LineNormalizer> JsonLinesAgent<N> {
    pub fn new(
        process: Box<dyn RunningProcess>,
        recorder: Arc<RunRecorder>,
        normalizer: N,
        approval_resolver: ApprovalResolver,
        settings: JsonLinesSettings,
    ) -> Self {
        Self {
            process,
            recorder,
            normalizer,
            queue: VecDeque::new(),
            control: None,
            control_surface: None,
            approval_resolver,
            settings,
            finished: false,
            eof: false,
        }
    }

    /// Attach the control protocol over the process's open stdin.
    pub fn with_control(
        mut self,
        channel: Arc<ControlChannel>,
        surface: Arc<dyn AgentControl>,
    ) -> Self {
        self.control = Some(channel);
        self.control_surface = Some(surface);
        self
    }

    pub fn control_channel(&self) -> Option<&Arc<ControlChannel>> {
        self.control.as_ref()
    }

    /// The process's stdin writer, when the plan kept it open.
    pub fn process_stdin(&self) -> Option<StdinWriter> {
        self.process.stdin()
    }

    async fn pull(&mut self) -> Result<Option<AgentEvent>, AgentError> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Ok(Some(event));
            }
            if self.eof {
                return Ok(None);
            }
            let line = match self.process.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    self.eof = true;
                    self.shut_control("agent stdout closed");
                    return Ok(None);
                }
                Err(err) => {
                    self.eof = true;
                    self.shut_control("agent stream failed");
                    return Err(err);
                }
            };
            let stripped = line.trim();
            if stripped.is_empty() {
                continue;
            }
            let outcome = self.normalizer.feed_line(stripped);
            self.recorder.emit_raw(
                PROTOCOL,
                "stdout",
                Value::String(stripped.to_string()),
                outcome.native_type.as_deref(),
            );
            self.queue.extend(outcome.events);
            if let Some(frame) = outcome.control {
                self.handle_control(frame).await;
            }
            if outcome.finished && !self.finished {
                self.finished = true;
                // The CLI exits on stdin EOF in streaming-input mode.
                if let Some(channel) = &self.control {
                    channel.writer().close();
                } else if let Some(writer) = self.process.stdin() {
                    writer.close();
                }
            }
        }
    }

    /// Fail outstanding control requests and release the agent's stdin.
    fn shut_control(&self, reason: &str) {
        if let Some(channel) = &self.control {
            channel.close(reason);
            channel.writer().close();
        } else if let Some(writer) = self.process.stdin() {
            writer.close();
        }
    }

    async fn handle_control(&mut self, frame: ControlFrame) {
        match frame {
            ControlFrame::Response { request_id, result } => {
                if let Some(channel) = &self.control {
                    channel.resolve(&request_id, result);
                }
            }
            ControlFrame::Request {
                request_id,
                subtype,
                request,
            } => {
                if subtype == "can_use_tool" {
                    self.handle_approval(request_id, request).await;
                } else if let Some(channel) = &self.control {
                    let _ = channel.respond(
                        &request_id,
                        Err(format!(
                            "control request subtype {subtype:?} is not supported by this host"
                        )),
                    );
                }
            }
            // Approvals are resolved inline before the next line is read,
            // so a cancellation can never catch one in flight.
            ControlFrame::CancelRequest { .. } => {}
        }
    }

    async fn handle_approval(&mut self, request_id: String, request: Map<String, Value>) {
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let tool_input = request.get("input").cloned().unwrap_or(Value::Null);
        let tool_use_id = request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let approval = ApprovalRequest {
            request_id: request_id.clone(),
            tool_name: tool_name.clone(),
            tool_input: tool_input.clone(),
            tool_use_id: tool_use_id.clone(),
            detail: Value::Object(request),
        };
        self.queue.push_back(AgentEvent::with_native(
            EventType::ApprovalRequested,
            "control_request",
            json!({
                "request_id": request_id,
                "tool_name": tool_name,
                "tool_input": tool_input,
                "tool_use_id": tool_use_id,
                "detail": approval.detail,
            }),
        ));
        let resolver = self.approval_resolver.clone();
        let (decision, resolution) =
            match tokio::time::timeout(self.settings.approval_timeout, resolver(approval)).await {
                Ok(decision) => (decision, "resolver"),
                Err(_) => (
                    ApprovalDecision::Deny {
                        message: format!(
                            "approval resolver timed out after {}s; denied",
                            self.settings.approval_timeout.as_secs_f64()
                        ),
                        interrupt: false,
                    },
                    "timeout",
                ),
            };
        let (behavior, message, response) = match &decision {
            ApprovalDecision::Allow { updated_input } => (
                "allow",
                Value::Null,
                json!({"behavior": "allow", "updatedInput": updated_input.clone().unwrap_or(tool_input)}),
            ),
            ApprovalDecision::Deny { message, interrupt } => {
                let mut response = json!({"behavior": "deny", "message": message});
                if *interrupt {
                    response["interrupt"] = Value::Bool(true);
                }
                ("deny", Value::String(message.clone()), response)
            }
        };
        self.queue.push_back(AgentEvent::with_native(
            EventType::ApprovalResolved,
            "control_request",
            json!({
                "request_id": request_id,
                "tool_name": tool_name,
                "behavior": behavior,
                "message": message,
                "resolution": resolution,
            }),
        ));
        let delivered = match &self.control {
            Some(channel) => channel.respond(&request_id, Ok(response)),
            None => Err(AgentError::ControlRequest(
                "no control channel to answer the approval".into(),
            )),
        };
        if let Err(err) = delivered {
            self.queue.push_back(AgentEvent::with_native(
                EventType::Warning,
                "control_request",
                json!({"message": format!("approval response for {request_id} not delivered: {err}")}),
            ));
        }
    }

    async fn finish(&mut self) -> AgentExit {
        if self.finished && self.control.is_some() {
            match tokio::time::timeout(self.settings.exit_grace, self.process.wait()).await {
                Ok(exit) => return exit,
                Err(_) => self.process.kill(DEFAULT_KILL_GRACE).await,
            }
        }
        self.process.wait().await
    }
}

impl<N: LineNormalizer> RunningAgent for JsonLinesAgent<N> {
    fn native_session_id(&self) -> Option<String> {
        self.normalizer.session_id().map(str::to_string)
    }

    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<AgentEvent>, AgentError>> {
        Box::pin(self.pull())
    }

    fn control(&self) -> Option<Arc<dyn AgentControl>> {
        self.control_surface.clone()
    }

    fn cancel(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.shut_control("run cancelled");
            self.process.kill(DEFAULT_KILL_GRACE).await;
        })
    }

    fn wait(&mut self) -> BoxFuture<'_, AgentExit> {
        Box::pin(self.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_object_line_classifies_inputs() {
        assert_eq!(parse_object_line("   "), Ok(None));
        assert_eq!(parse_object_line("plain text"), Err(()));
        assert_eq!(parse_object_line("{not json"), Err(()));
        assert_eq!(parse_object_line("[1,2,3]"), Err(()));
        let depth = 10_000;
        let nested = format!("{}null{}", "{\"a\":".repeat(depth), "}".repeat(depth));
        assert_eq!(parse_object_line(&nested), Err(()));
        let obj = parse_object_line(" {\"type\": \"x\"} ").unwrap().unwrap();
        assert_eq!(obj.get("type"), Some(&Value::String("x".into())));
    }

    #[test]
    fn raw_events_carry_line_or_object() {
        let line = raw_line_event("garbage");
        assert_eq!(line.kind, EventType::Raw);
        assert_eq!(line.payload["data"]["line"], "garbage");
        let mut obj = Map::new();
        obj.insert("type".into(), Value::String("mystery".into()));
        let event = raw_object_event(&obj, Some("mystery"));
        assert_eq!(event.native_type.as_deref(), Some("mystery"));
        assert_eq!(event.payload["data"]["type"], "mystery");
    }
}
