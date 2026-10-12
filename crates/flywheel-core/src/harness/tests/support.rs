//! Shared fixtures for the harness tests.

use crate::agents_invoke::{InvocationRequest, InvokeFn};
use crate::envelope::{CLOSING_FENCE, EnvelopeResult, Intent, OPENING_FENCE, ValidEnvelope};
use crate::events::{DomainEvent, DomainEventKind};
use crate::grader_rubric::{JudgeInvoke, JudgeInvokeError, JudgeResponse, RubricJudgeError};
use crate::harness::{HarnessConfig, HarnessError, HarnessOutcome, RunOptions, run_task};
use crate::invoker::{
    InvocationFailure, InvocationSignals, IterationResult, ToolInteraction, ToolResultObservation,
};
use crate::lifecycle::{Attempt, Lifecycle};
use crate::prompt::RecoveryHandoff;
use crate::recovery_summarizer::{
    CLOSING_FENCE as RECOVERY_CLOSING_FENCE, OPENING_FENCE as RECOVERY_OPENING_FENCE,
    SummarizerInvoke, SummarizerInvokeError,
};
use crate::store::{
    AttemptStore, ControlCommandRecord, ControlCommandStore, DomainEventStore, GraderResultRecord,
    GraderResultStore, HarnessStore, JsonObject, LifecycleFilter, LifecycleStore, SpendSummary,
    SpendWindow, StoreError, TaskStore, TelemetryRecord, TelemetrySink,
};
use crate::store_memory::InMemoryStore;
use crate::task::{CommandGrader, Grader, Task};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// --- Telemetry sinks ------------------------------------------------------

/// In-memory sink capturing records in emission order.
#[derive(Default)]
pub struct ListSink {
    pub records: Mutex<Vec<TelemetryRecord>>,
}

impl ListSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn all(&self) -> Vec<TelemetryRecord> {
        self.records.lock().unwrap().clone()
    }

    /// Harness telemetry records.
    pub fn events(&self, run_id: &str) -> Vec<TelemetryRecord> {
        self.all()
            .into_iter()
            .filter(|r| r.run_id == run_id && r.kind.starts_with("harness."))
            .collect()
    }

    pub fn kinds(&self, run_id: &str) -> Vec<String> {
        self.events(run_id).into_iter().map(|r| r.kind).collect()
    }

    pub fn of_kind(&self, run_id: &str, kind: &str) -> Vec<TelemetryRecord> {
        self.events(run_id)
            .into_iter()
            .filter(|r| r.kind == kind)
            .collect()
    }

    /// Agent message records.
    pub fn messages(&self, run_id: &str) -> Vec<TelemetryRecord> {
        self.all()
            .into_iter()
            .filter(|r| {
                r.run_id == run_id && !r.kind.starts_with("harness.") && !r.kind.starts_with("domain.")
            })
            .collect()
    }

    /// `domain.*` ledger mirror lines, in emission order.
    pub fn domain_mirrors(&self, run_id: &str) -> Vec<TelemetryRecord> {
        self.all()
            .into_iter()
            .filter(|r| r.run_id == run_id && r.kind.starts_with("domain."))
            .collect()
    }
}

impl TelemetrySink for ListSink {
    fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError> {
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }
}

/// A sink that fails on every append (or only the first `fail_first`).
pub struct RaisingSink {
    pub attempts: Mutex<u32>,
    pub fail_first: Option<u32>,
    pub records: Mutex<Vec<TelemetryRecord>>,
}

impl RaisingSink {
    pub fn new(fail_first: Option<u32>) -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(0),
            fail_first,
            records: Mutex::new(Vec::new()),
        })
    }

    pub fn attempts(&self) -> u32 {
        *self.attempts.lock().unwrap()
    }
}

impl TelemetrySink for RaisingSink {
    fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError> {
        let mut attempts = self.attempts.lock().unwrap();
        *attempts += 1;
        if self.fail_first.is_none_or(|n| *attempts <= n) {
            return Err(StoreError::backend(std::io::Error::other(
                "simulated sink append failure",
            )));
        }
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }
}

// --- Failure-injecting store -----------------------------------------------

/// An in-memory store with injectable failures on the ledger append (by
/// event kind) and on the attempt rollup write.
pub struct FailingStore {
    pub inner: InMemoryStore,
    pub fail_event_kind: Option<DomainEventKind>,
    pub fail_save_attempt: bool,
}

impl FailingStore {
    pub fn on_event(kind: DomainEventKind) -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryStore::new(),
            fail_event_kind: Some(kind),
            fail_save_attempt: false,
        })
    }

    pub fn on_save_attempt() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryStore::new(),
            fail_event_kind: None,
            fail_save_attempt: true,
        })
    }
}

fn simulated(msg: &str) -> StoreError {
    StoreError::backend(std::io::Error::other(msg.to_owned()))
}

impl LifecycleStore for FailingStore {
    fn create_lifecycle(&self, lifecycle: &Lifecycle) -> Result<(), StoreError> {
        self.inner.create_lifecycle(lifecycle)
    }
    fn update_lifecycle(&self, lifecycle: &Lifecycle, expected_version: u64) -> Result<(), StoreError> {
        self.inner.update_lifecycle(lifecycle, expected_version)
    }
    fn load_lifecycle(&self, run_id: &str) -> Result<Option<Lifecycle>, StoreError> {
        self.inner.load_lifecycle(run_id)
    }
    fn list_lifecycles(&self, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, StoreError> {
        self.inner.list_lifecycles(filter)
    }
    fn summarize_spend(&self, window: &SpendWindow) -> Result<SpendSummary, StoreError> {
        self.inner.summarize_spend(window)
    }
}

impl TaskStore for FailingStore {
    fn save_task(&self, task: &Task, now: DateTime<Utc>) -> Result<String, StoreError> {
        self.inner.save_task(task, now)
    }
    fn load_task(&self, task_id: &str, content_hash: Option<&str>) -> Result<Option<Task>, StoreError> {
        self.inner.load_task(task_id, content_hash)
    }
    fn load_task_for_run(&self, run_id: &str) -> Result<Option<Task>, StoreError> {
        self.inner.load_task_for_run(run_id)
    }
}

impl DomainEventStore for FailingStore {
    fn append_domain_event(&self, event: &DomainEvent, expected_version: u64) -> Result<Lifecycle, StoreError> {
        if self.fail_event_kind == Some(event.kind()) {
            return Err(simulated("simulated ledger append failure"));
        }
        self.inner.append_domain_event(event, expected_version)
    }
    fn list_domain_events(&self, run_id: &str) -> Result<Vec<DomainEvent>, StoreError> {
        self.inner.list_domain_events(run_id)
    }
}

impl AttemptStore for FailingStore {
    fn save_attempt(&self, run_id: &str, attempt: &Attempt, expected_version: Option<u64>) -> Result<(), StoreError> {
        if self.fail_save_attempt {
            return Err(simulated("simulated rollup write failure"));
        }
        self.inner.save_attempt(run_id, attempt, expected_version)
    }
    fn load_attempt(&self, run_id: &str, number: u32) -> Result<Option<Attempt>, StoreError> {
        self.inner.load_attempt(run_id, number)
    }
    fn list_attempts(&self, run_id: &str) -> Result<Vec<Attempt>, StoreError> {
        self.inner.list_attempts(run_id)
    }
}

impl GraderResultStore for FailingStore {
    fn append_grader_result(&self, result: &GraderResultRecord) -> Result<GraderResultRecord, StoreError> {
        self.inner.append_grader_result(result)
    }
    fn list_grader_results(&self, run_id: &str, attempt_number: u32) -> Result<Vec<GraderResultRecord>, StoreError> {
        self.inner.list_grader_results(run_id, attempt_number)
    }
}

impl ControlCommandStore for FailingStore {
    fn enqueue_command(&self, run_id: &str, kind: &str, payload: JsonObject, now: DateTime<Utc>) -> Result<ControlCommandRecord, StoreError> {
        self.inner.enqueue_command(run_id, kind, payload, now)
    }
    fn claim_commands(&self, run_id: &str, now: DateTime<Utc>) -> Result<Vec<ControlCommandRecord>, StoreError> {
        self.inner.claim_commands(run_id, now)
    }
    fn delete_command(&self, command_id: i64) -> Result<(), StoreError> {
        self.inner.delete_command(command_id)
    }
}

// --- Messages and iterations ----------------------------------------------

pub fn signals() -> InvocationSignals {
    InvocationSignals {
        stop_reason: Some("end_turn".into()),
        num_turns: Some(1),
        total_cost_usd: Some(0.01),
        result_is_error: Some(false),
        result_subtype: Some("success".into()),
        api_error_status: None,
        session_id: Some("sess-1".into()),
        ..InvocationSignals::default()
    }
}

pub fn usage_dict(input_tokens: u64) -> Value {
    json!({
        "input_tokens": input_tokens,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
}

/// An SDK-shaped assistant message.
pub fn assistant_with(text: &str, usage: Option<Value>) -> Value {
    json!({
        "message_type": "AssistantMessage",
        "content": [{"type": "text", "text": text}],
        "model": "claude-test",
        "stop_reason": "end_turn",
        "session_id": "sess-1",
        "usage": usage,
    })
}

pub fn assistant() -> Value {
    assistant_with("ok", None)
}

pub fn assistant_usage(usage: Value) -> Value {
    assistant_with("ok", Some(usage))
}

/// An SDK-shaped result message.
pub fn result_msg_with(num_turns: u64, usage: Option<Value>) -> Value {
    json!({
        "message_type": "ResultMessage",
        "subtype": "success",
        "duration_ms": 10,
        "is_error": false,
        "num_turns": num_turns,
        "session_id": "sess-1",
        "stop_reason": "end_turn",
        "total_cost_usd": 0.01,
        "usage": usage,
    })
}

pub fn result_msg() -> Value {
    result_msg_with(1, None)
}

pub fn valid(intent: Intent) -> EnvelopeResult {
    EnvelopeResult::Valid(ValidEnvelope {
        intent,
        reason: None,
        requires: Vec::new(),
    })
}

pub fn valid_reason(intent: Intent, reason: &str) -> EnvelopeResult {
    EnvelopeResult::Valid(ValidEnvelope {
        intent,
        reason: Some(reason.to_owned()),
        requires: Vec::new(),
    })
}

pub fn iteration(envelope: EnvelopeResult, messages: Vec<Value>) -> IterationResult {
    IterationResult {
        transcript: String::new(),
        messages,
        envelope,
        signals: signals(),
        failure: None,
        usage: None,
    }
}

pub fn verify_iteration() -> IterationResult {
    iteration(valid(Intent::Verify), vec![assistant(), result_msg()])
}

pub fn wrap(payload: &str) -> String {
    format!("{OPENING_FENCE}\n{payload}\n{CLOSING_FENCE}")
}

pub fn failure(error_type: &str, message: &str) -> InvocationFailure {
    InvocationFailure {
        error_type: error_type.into(),
        message: message.into(),
        exit_code: None,
        stderr: None,
    }
}

/// One tool interaction for a loop-guard stream.
pub fn tool_call(id: &str, is_error: bool) -> ToolInteraction {
    let mut input = Map::new();
    input.insert("command".into(), json!("ls"));
    ToolInteraction {
        tool_use_id: id.into(),
        tool_name: "Bash".into(),
        tool_input: input,
        result: Some(ToolResultObservation {
            tool_use_id: id.into(),
            is_error: Some(is_error),
            content: None,
        }),
    }
}

pub fn signals_with_tools(interactions: Vec<ToolInteraction>) -> InvocationSignals {
    InvocationSignals {
        tool_interactions: interactions,
        ..signals()
    }
}

// --- Graders --------------------------------------------------------------

pub fn command(run: &str, name: Option<&str>) -> Grader {
    Grader::Command(CommandGrader {
        run: run.into(),
        name: name.map(str::to_owned),
    })
}

pub fn ok_command() -> Grader {
    command("true", Some("ok"))
}

pub fn fail_command() -> Grader {
    command("false", None)
}

pub fn task(graders: Vec<Grader>) -> Task {
    Task::new("g", graders)
}

pub fn lifecycle(run_id: &str) -> Lifecycle {
    Lifecycle::new("t1").with_run_id(run_id)
}

/// Zero the boundary-rolled aggregate counters (OLTP rollups replay cannot
/// reproduce) so oracle comparisons stay on the ledger-folded fields.
pub fn strip_attempt_aggregates(lifecycle: &mut Lifecycle) {
    for attempt in &mut lifecycle.attempts {
        attempt.input_tokens = 0;
        attempt.output_tokens = 0;
        attempt.cache_creation_input_tokens = 0;
        attempt.cache_read_input_tokens = 0;
        attempt.iterations_completed = 0;
        attempt.turns = 0;
        attempt.total_cost_usd = 0.0;
        attempt.last_activity_at = None;
    }
}

// --- Invokers -------------------------------------------------------------

pub type Calls = Arc<Mutex<Vec<InvocationRequest>>>;

/// An invoker returning `results` in order, recording every request and
/// honoring the per-message observer contract.
pub fn scripted_invoker(results: Vec<IterationResult>) -> (InvokeFn, Calls) {
    let queue = Arc::new(Mutex::new(VecDeque::from(results)));
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let invoke: InvokeFn = Arc::new(move |request: InvocationRequest| {
        let queue = queue.clone();
        let recorded = recorded.clone();
        Box::pin(async move {
            recorded.lock().unwrap().push(request.clone());
            let result = queue
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted invoker ran out of results");
            if let Some(on_message) = &request.observers.on_message {
                for msg in &result.messages {
                    on_message(msg);
                }
            }
            result
        })
    });
    (invoke, calls)
}

pub fn call_count(calls: &Calls) -> usize {
    calls.lock().unwrap().len()
}

pub fn prompt_of(calls: &Calls, index: usize) -> String {
    calls.lock().unwrap()[index].prompt.clone()
}

// --- Running --------------------------------------------------------------

pub struct Harness {
    pub store: Arc<InMemoryStore>,
    pub sink: Arc<ListSink>,
}

impl Harness {
    pub fn new() -> Self {
        Self {
            store: Arc::new(InMemoryStore::new()),
            sink: ListSink::new(),
        }
    }

    pub fn options(&self) -> RunOptions {
        RunOptions::default().with_sink(self.sink.clone())
    }

    pub async fn run(
        &self,
        task: &Task,
        lifecycle: &mut Lifecycle,
        config: &HarnessConfig,
        invoke: InvokeFn,
    ) -> HarnessOutcome {
        run_task(
            task,
            lifecycle,
            self.store.clone(),
            config,
            invoke,
            self.options(),
        )
        .await
        .expect("run_task succeeds")
    }

    pub async fn run_with(
        &self,
        task: &Task,
        lifecycle: &mut Lifecycle,
        config: &HarnessConfig,
        invoke: InvokeFn,
        options: RunOptions,
    ) -> Result<HarnessOutcome, HarnessError> {
        run_task(task, lifecycle, self.store.clone(), config, invoke, options).await
    }
}

pub fn worktree_config(dir: &tempfile::TempDir) -> HarnessConfig {
    HarnessConfig {
        worktree: Some(dir.path().to_path_buf()),
        ..HarnessConfig::default()
    }
}

// --- Rubric judge and summarizer seams --------------------------------------

pub fn rubric_wrap(payload: &str) -> String {
    format!(
        "{}\n{payload}\n{}",
        crate::grader_rubric::OPENING_FENCE,
        crate::grader_rubric::CLOSING_FENCE
    )
}

/// A scripted judge: each call pops the next response (text or error).
pub struct ScriptedJudge {
    responses: Mutex<VecDeque<Result<String, RubricJudgeError>>>,
    pub calls: Mutex<Vec<(String, PathBuf)>>,
}

impl ScriptedJudge {
    pub fn new(responses: Vec<Result<String, RubricJudgeError>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(VecDeque::from(responses)),
            calls: Mutex::new(Vec::new()),
        })
    }

    pub fn passing(summary: &str) -> Arc<Self> {
        Self::new(vec![Ok(rubric_wrap(&format!(
            "{{\"passed\": true, \"summary\": \"{summary}\"}}"
        )))])
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl JudgeInvoke for ScriptedJudge {
    fn invoke<'a>(
        &'a self,
        prompt: &'a str,
        _grader: &'a crate::task::RubricGrader,
        worktree: &'a std::path::Path,
    ) -> crate::grader_rubric::JudgeFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((prompt.to_owned(), worktree.to_path_buf()));
            let head = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted judge ran out of responses");
            match head {
                Ok(text) => Ok(JudgeResponse::from(text)),
                Err(err) => Err(Box::new(err) as JudgeInvokeError),
            }
        })
    }
}

/// A scripted summarizer returning one handoff envelope (or failing).
pub struct ScriptedSummarizer {
    handoff: Result<RecoveryHandoff, String>,
    pub calls: Mutex<Vec<(String, PathBuf)>>,
}

impl ScriptedSummarizer {
    pub fn new(handoff: RecoveryHandoff) -> Arc<Self> {
        Arc::new(Self {
            handoff: Ok(handoff),
            calls: Mutex::new(Vec::new()),
        })
    }

    pub fn failing(message: &str) -> Arc<Self> {
        Arc::new(Self {
            handoff: Err(message.to_owned()),
            calls: Mutex::new(Vec::new()),
        })
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl SummarizerInvoke for ScriptedSummarizer {
    fn invoke<'a>(
        &'a self,
        prompt: &'a str,
        worktree: &'a std::path::Path,
    ) -> crate::recovery_summarizer::SummarizerFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((prompt.to_owned(), worktree.to_path_buf()));
            match &self.handoff {
                Ok(handoff) => Ok(format!(
                    "{RECOVERY_OPENING_FENCE}\n{}\n{RECOVERY_CLOSING_FENCE}\n",
                    serde_json::to_string(&json!({
                        "work_done": handoff.work_done,
                        "work_remaining": handoff.work_remaining,
                        "key_decisions": handoff.key_decisions,
                        "suggested_next_step": handoff.suggested_next_step,
                    }))
                    .unwrap()
                )),
                Err(message) => Err(Box::new(std::io::Error::other(message.clone()))
                    as SummarizerInvokeError),
            }
        })
    }
}

pub fn handoff(a: &str, b: &str, c: &str, d: &str) -> RecoveryHandoff {
    RecoveryHandoff {
        work_done: a.into(),
        work_remaining: b.into(),
        key_decisions: c.into(),
        suggested_next_step: d.into(),
    }
}

pub fn fixed_clock(ts: DateTime<Utc>) -> crate::invoker_client::Clock {
    Arc::new(move || ts)
}

pub fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    use chrono::TimeZone;
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
}
