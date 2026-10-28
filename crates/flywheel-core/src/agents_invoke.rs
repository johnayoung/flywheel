//! Bridge from the `flywheel-agents` runtime to the harness invoke seam.
//!
//! [`AgentsInvoker`] builds the harness [`InvokeFn`]: one call drives one
//! agent iteration through [`AgentRuntime::start`], streams every normalized
//! [`AgentEvent`] to the per-message observer (hang-watchdog liveness and
//! telemetry), enforces transcript hard limits mid-run via
//! [`enforce_transcript_limits`] (cancelling the run on the first breach),
//! runs the in-run control watcher over the live [`AgentControl`]
//! (operator `interrupt` / `say` / `set_model`, the context-usage observer,
//! the recovery request, the checkpoint nudge), and folds the
//! [`CompletedRun`] onto an [`IterationResult`] with
//! [`completed_run_to_iteration_result`] (field mapping specified in
//! `docs/agent-harness.md` section 15.1).
//!
//! A lost closing envelope on a cleanly finished iteration
//! ([`iteration_envelope_is_salvageable`]) is salvaged by resuming the same
//! native session with [`ENVELOPE_SALVAGE_PROMPT`] -- the CLI transport's
//! equivalent of re-prompting the live SDK session -- and only a VALID
//! re-emitted envelope is adopted via [`merge_salvaged_iteration`].
//!
//! [`ColdSession`] is the shared "fresh session, full skill surface, auto
//! permissions, turn cap" runner behind the rubric judge
//! ([`crate::grader_rubric::AgentsJudgeInvoke`]) and the recovery summarizer
//! ([`crate::recovery_summarizer::AgentsSummarizerInvoke`]).
//!
//! There is no vendor SDK in the Rust runtime: every agent runs through the
//! claude-code (or codex) adapter under an [`ExecutionHost`], so this module
//! is the single agent-driving boundary the Python `_sdk` / `agents_invoke`
//! pair used to share.

use crate::envelope::{EnvelopeResult, parse_envelope};
use crate::grader_transcript::{MonotonicClock, TranscriptCounter, first_breach};
use crate::invoker::{
    InvocationFailure, InvocationSignals, IterationResult, ToolInteraction, ToolResultContent,
    ToolResultObservation, Usage, iteration_envelope_is_salvageable, merge_salvaged_iteration,
};
use crate::invoker_client::{
    CheckpointNudge, CommandAppliedObserver, ContextUsageObserver, ControlCommandWatcher,
    ControlPlane, ENVELOPE_SALVAGE_PROMPT, FAILURE_HARNESS_RECOVERY, FAILURE_OPERATOR_INTERRUPT,
    ProgressProbe, RecoverySignal, WatcherStop,
};
use crate::task::TranscriptGrader;
use flywheel_agents::events::EventSink;
use flywheel_agents::runtime::ActiveRun;
use flywheel_agents::{
    AgentConfiguration, AgentError, AgentEvent, AgentRuntime, CompletedRun, EventType,
    ExecutionHost, PermissionPolicy, RawAgentEvent, RunOptions, RunRequest,
};
use serde_json::{Value, json};
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// The runtime's own failure marker for a run it cancelled on request.
const RUNTIME_CANCELLED: &str = "cancelled";

/// Receives each recorded message (the serialized normalized event) the
/// instant it arrives, before the iteration result is assembled. A
/// panicking observer is contained so a faulty live renderer can never break
/// the agent run.
pub type MessageObserver = Arc<dyn Fn(&Value) + Send + Sync>;

/// The per-iteration callbacks the harness threads into an invocation.
///
/// All optional; a default-constructed value leaves every seam dormant.
#[derive(Clone, Default)]
pub struct InvocationObservers {
    /// Per-message persistence / liveness observer.
    pub on_message: Option<MessageObserver>,
    /// Mid-turn context-occupancy readings, once per watcher poll.
    pub context_observer: Option<ContextUsageObserver>,
    /// The harness's mid-turn recovery request.
    pub recovery_interrupt: Option<RecoverySignal>,
    /// Steering-ledger seam: fired once per applied control command.
    pub on_command_applied: Option<CommandAppliedObserver>,
    /// Git-free progress probe for the checkpoint nudge.
    pub checkpoint_progress_probe: Option<ProgressProbe>,
}

impl fmt::Debug for InvocationObservers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvocationObservers")
            .field("on_message", &self.on_message.is_some())
            .field("context_observer", &self.context_observer.is_some())
            .field("recovery_interrupt", &self.recovery_interrupt.is_some())
            .field("on_command_applied", &self.on_command_applied.is_some())
            .field(
                "checkpoint_progress_probe",
                &self.checkpoint_progress_probe.is_some(),
            )
            .finish()
    }
}

/// The harness's request to stop an in-flight invocation and the
/// invoker's acknowledgement that it owns a process whose termination the
/// harness must wait for.
///
/// Dropping an invocation future only *requests* cancellation of a run
/// whose teardown (SIGTERM, grace, SIGKILL, reap) happens elsewhere, so a
/// retry could otherwise start a new agent in the same worktree while the
/// old one is still alive. Instead the harness calls [`abort`] and keeps
/// awaiting the invocation; an invoker that [`bind`]s the handle cancels
/// its run on abort and resolves only once the agent is gone. An invoker
/// that never binds (a scripted test double) owns no process, so the
/// harness drops it immediately.
///
/// [`abort`]: InvocationAbort::abort
/// [`bind`]: InvocationAbort::bind
#[derive(Debug, Clone)]
pub struct InvocationAbort {
    aborted: watch::Sender<bool>,
    bound: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for InvocationAbort {
    fn default() -> Self {
        Self::new()
    }
}

impl InvocationAbort {
    pub fn new() -> Self {
        Self {
            aborted: watch::Sender::new(false),
            bound: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Request the invocation stop. Idempotent.
    pub fn abort(&self) {
        self.aborted.send_replace(true);
    }

    pub fn is_aborted(&self) -> bool {
        *self.aborted.borrow()
    }

    /// Resolve once [`InvocationAbort::abort`] has been called.
    pub async fn aborted(&self) {
        let mut rx = self.aborted.subscribe();
        // The sender lives in `self`, so the channel cannot close here.
        let _ = rx.wait_for(|aborted| *aborted).await;
    }

    /// Declare that this invocation drives a process the harness must
    /// wait on after an abort.
    pub fn bind(&self) {
        self.bound.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_bound(&self) -> bool {
        self.bound.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The data handed to the harness's invoke callable for one iteration.
///
/// Decoupled from the invoker so the harness can hand the transcript graders
/// (enforced mid-run as hard limits), the attempt / iteration context, the
/// checkpoint-nudge inputs, and the session to resume to a substitutable
/// transport without each invoker re-deriving them.
#[derive(Clone, Default)]
pub struct InvocationRequest {
    pub prompt: String,
    pub transcript_graders: Vec<TranscriptGrader>,
    pub attempt_number: u32,
    pub iteration_number: u32,
    /// Remaining-wall-time threshold for the checkpoint nudge; `None` or
    /// non-positive disables it.
    pub checkpoint_nudge_seconds: Option<f64>,
    /// The resolved `AGENT_ITERATION` ceiling the nudge measures against;
    /// `None` (unbounded) disables it.
    pub agent_iteration_ceiling_seconds: Option<f64>,
    /// Native session to resume; `None` starts a fresh session.
    pub resume_session_id: Option<String>,
    pub observers: InvocationObservers,
    /// The harness's stop request for this invocation; `None` when the
    /// caller never aborts (it awaits the invocation to completion).
    pub abort: Option<InvocationAbort>,
}

impl InvocationRequest {
    /// A fresh-session request for `prompt` with every seam dormant.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            ..Self::default()
        }
    }
}

impl fmt::Debug for InvocationRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvocationRequest")
            .field("prompt_len", &self.prompt.len())
            .field("transcript_graders", &self.transcript_graders)
            .field("attempt_number", &self.attempt_number)
            .field("iteration_number", &self.iteration_number)
            .field("checkpoint_nudge_seconds", &self.checkpoint_nudge_seconds)
            .field(
                "agent_iteration_ceiling_seconds",
                &self.agent_iteration_ceiling_seconds,
            )
            .field("resume_session_id", &self.resume_session_id)
            .field("observers", &self.observers)
            .field(
                "abort",
                &self.abort.as_ref().map(InvocationAbort::is_aborted),
            )
            .finish()
    }
}

/// The future one agent iteration resolves to.
pub type InvokeFuture = Pin<Box<dyn Future<Output = IterationResult> + Send>>;

/// One agent iteration as a shareable async function: the Rust spelling of
/// the Python `InvokeFunc` (`Callable[[InvocationRequest], Awaitable[IterationResult]]`).
pub type InvokeFn = Arc<dyn Fn(InvocationRequest) -> InvokeFuture + Send + Sync>;

/// Alias matching the Python name.
pub type InvokeFunc = InvokeFn;

/// Run `f`, containing a panic: observers are best-effort by contract.
fn guarded(f: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(f));
}

// --- The fold ---------------------------------------------------------------------

/// Serialize one normalized event into the message shape the invoker
/// records (`IterationResult::messages`) and the transcript counter reads:
/// `{"type": "<event type>", "payload": {...}, "source", "native_type",
/// "sequence", "timestamp"}`.
pub fn event_to_message(event: &AgentEvent) -> Value {
    serde_json::to_value(event).unwrap_or_else(|_| {
        json!({
            "type": event.kind.as_str(),
            "payload": Value::Object(event.payload.clone()),
            "source": event.source,
            "native_type": event.native_type,
            "sequence": event.sequence,
        })
    })
}

fn coerce_content(content: Option<&Value>) -> Option<ToolResultContent> {
    match content? {
        Value::Null => None,
        Value::String(text) => Some(ToolResultContent::Text(text.clone())),
        Value::Array(items) => Some(ToolResultContent::Blocks(
            items.iter().filter_map(Value::as_object).cloned().collect(),
        )),
        other => Some(ToolResultContent::Text(other.to_string())),
    }
}

fn to_core_usage(usage: &flywheel_agents::Usage) -> Usage {
    usage
        .iter()
        .map(|(k, v)| (k.clone(), u64::try_from(*v).unwrap_or(0)))
        .collect()
}

fn api_error_status(messages: &[Value]) -> Option<u16> {
    messages.iter().rev().find_map(|msg| {
        if msg.get("type").and_then(Value::as_str) != Some(EventType::SessionFinished.as_str()) {
            return None;
        }
        msg.get("payload")
            .and_then(|p| p.get("api_error_status"))
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok())
    })
}

/// Pure fold: `CompletedRun` + the recorded messages -> `IterationResult`.
///
/// `api_error_status` is read from the recorded `session.finished` message
/// (the fold does not carry it); `result_is_error` is `None` when the agent
/// never emitted its terminal envelope. Rate-limit events and permission
/// denials are carried as their JSON forms.
pub fn completed_run_to_iteration_result(
    completed: &CompletedRun,
    messages: Vec<Value>,
) -> IterationResult {
    let mut interactions = Vec::with_capacity(completed.tool_interactions.len());
    let mut blocks = Vec::new();
    for interaction in &completed.tool_interactions {
        let observation = interaction.result.as_ref().map(|result| {
            let observation = ToolResultObservation {
                tool_use_id: result.tool_use_id.clone(),
                is_error: result.is_error,
                content: coerce_content(result.content.as_ref()),
            };
            blocks.push(observation.clone());
            observation
        });
        interactions.push(ToolInteraction {
            tool_use_id: interaction.tool_use_id.clone(),
            tool_name: interaction.tool_name.clone(),
            tool_input: interaction.tool_input.clone(),
            result: observation,
        });
    }
    let finished = completed.stop.finished;
    let signals = InvocationSignals {
        stop_reason: completed.stop.native.clone(),
        num_turns: completed.num_turns.and_then(|n| u64::try_from(n).ok()),
        total_cost_usd: completed.total_cost_usd,
        result_is_error: finished.then_some(completed.stop.is_error),
        result_subtype: completed.stop.native_subtype.clone(),
        api_error_status: api_error_status(&messages),
        session_id: completed.native_session_id.clone(),
        permission_denials: completed
            .permission_denials
            .iter()
            .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
            .collect(),
        rate_limit_events: completed
            .rate_limit_events
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            .collect(),
        tool_interactions: interactions,
        tool_result_blocks: blocks,
        hook_events: Vec::new(),
        pending_tool_use_at_stop: completed.stop.pending_tool_use,
    };
    let failure = completed.failure.as_ref().map(|f| InvocationFailure {
        error_type: f.error_type.clone(),
        message: f.message.clone(),
        exit_code: f.exit_code,
        stderr: f.stderr.clone(),
    });
    IterationResult {
        transcript: completed.final_text.clone(),
        messages,
        envelope: parse_envelope(&completed.final_text),
        signals,
        failure,
        usage: completed.usage.as_ref().map(to_core_usage),
    }
}

/// The result for a run that never started (adapter lookup, spawn, or
/// authentication-policy failure): no transcript, a missing envelope, and
/// the runtime's error code as the failure type.
pub fn startup_failure_result(err: &AgentError) -> IterationResult {
    IterationResult {
        transcript: String::new(),
        messages: Vec::new(),
        envelope: EnvelopeResult::Missing,
        signals: InvocationSignals::default(),
        failure: Some(InvocationFailure {
            error_type: err.code().to_owned(),
            message: err.to_string(),
            exit_code: None,
            stderr: None,
        }),
        usage: None,
    }
}

// --- The invoker -------------------------------------------------------------------

/// The agents-backed harness invoker: one configured agent, one working
/// directory, one optional control plane; [`AgentsInvoker::invoke`] drives
/// one iteration.
pub struct AgentsInvoker {
    runtime: Arc<AgentRuntime>,
    configuration: AgentConfiguration,
    working_directory: PathBuf,
    timeout_seconds: Option<f64>,
    host: Option<Arc<dyn ExecutionHost>>,
    sink: Option<Arc<dyn EventSink>>,
    control_plane: Option<ControlPlane>,
    salvage_lost_envelope: bool,
    transcript_clock: Option<MonotonicClock>,
}

impl fmt::Debug for AgentsInvoker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentsInvoker")
            .field("agent_id", &self.configuration.agent_id)
            .field("model_id", &self.configuration.model_id)
            .field("working_directory", &self.working_directory)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("control_plane", &self.control_plane)
            .field("salvage_lost_envelope", &self.salvage_lost_envelope)
            .finish_non_exhaustive()
    }
}

impl AgentsInvoker {
    /// An invoker for `configuration` running in `working_directory` on the
    /// default runtime (default registry, local host), with lost-envelope
    /// salvage enabled.
    pub fn new(configuration: AgentConfiguration, working_directory: impl Into<PathBuf>) -> Self {
        Self {
            runtime: Arc::new(AgentRuntime::new()),
            configuration,
            working_directory: working_directory.into(),
            timeout_seconds: None,
            host: None,
            sink: None,
            control_plane: None,
            salvage_lost_envelope: true,
            transcript_clock: None,
        }
    }

    pub fn with_runtime(mut self, runtime: Arc<AgentRuntime>) -> Self {
        self.runtime = runtime;
        self
    }

    /// Run under `host` instead of the runtime's default host (the Docker
    /// backend passes its `DockerExecHost` here).
    pub fn with_host(mut self, host: Arc<dyn ExecutionHost>) -> Self {
        self.host = Some(host);
        self
    }

    /// A dedicated sink for raw and normalized events (telemetry of the
    /// native envelopes); the per-message observer sees normalized events
    /// regardless.
    pub fn with_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Hard wall-clock ceiling per run, enforced by the runtime.
    pub fn with_timeout_seconds(mut self, timeout_seconds: Option<f64>) -> Self {
        self.timeout_seconds = timeout_seconds;
        self
    }

    /// Wire the store-routed control plane (operator steering).
    pub fn with_control_plane(mut self, plane: ControlPlane) -> Self {
        self.control_plane = Some(plane);
        self
    }

    /// Enable or disable the lost-envelope salvage re-prompt.
    pub fn with_envelope_salvage(mut self, enabled: bool) -> Self {
        self.salvage_lost_envelope = enabled;
        self
    }

    /// Inject the monotonic clock the transcript hard limits measure
    /// `max_wall_seconds` against.
    pub fn with_transcript_clock(mut self, clock: MonotonicClock) -> Self {
        self.transcript_clock = Some(clock);
        self
    }

    pub fn configuration(&self) -> &AgentConfiguration {
        &self.configuration
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub fn runtime(&self) -> &Arc<AgentRuntime> {
        &self.runtime
    }

    pub fn control_plane(&self) -> Option<&ControlPlane> {
        self.control_plane.as_ref()
    }

    fn supports_resume(&self) -> bool {
        self.runtime
            .adapter(&self.configuration.agent_id)
            .map(|adapter| adapter.descriptor().capabilities.native_resume)
            .unwrap_or(false)
    }

    fn run_options(&self) -> RunOptions {
        RunOptions {
            sink: self.sink.clone(),
            host: self.host.clone(),
            approval_resolver: None,
        }
    }

    fn run_request(&self, prompt: &str, resume: Option<&str>) -> RunRequest {
        let mut configuration = self.configuration.clone();
        if let Some(session_id) = resume {
            configuration
                .adapter_options
                .insert("resume".into(), json!(session_id));
        }
        RunRequest {
            prompt: prompt.to_owned(),
            working_directory: self.working_directory.clone(),
            configuration,
            timeout_seconds: self.timeout_seconds,
        }
    }

    /// Drive exactly one agent iteration and return its structured result.
    ///
    /// Never fails at the type level: a run that could not start, crashed,
    /// timed out, or was cancelled reports through `IterationResult::failure`
    /// (`error_type` is the runtime's code, [`FAILURE_OPERATOR_INTERRUPT`],
    /// or [`FAILURE_HARNESS_RECOVERY`]). A run cut off by a transcript hard
    /// limit ends cleanly (no failure) with whatever was observed; the
    /// grader stage records the breach.
    pub async fn invoke(&self, request: InvocationRequest) -> IterationResult {
        if let Some(abort) = &request.abort {
            abort.bind();
        }
        let base = self
            .run_once(
                &request,
                &request.prompt,
                request.resume_session_id.as_deref(),
            )
            .await;
        if !self.salvage_lost_envelope
            || !iteration_envelope_is_salvageable(&base)
            || request
                .abort
                .as_ref()
                .is_some_and(InvocationAbort::is_aborted)
        {
            return base;
        }
        let Some(session_id) = base.signals.session_id.clone() else {
            return base;
        };
        if !self.supports_resume() {
            return base;
        }
        // Lost-envelope salvage: the agent finished its turn but the closing
        // envelope did not survive. Resume the SAME native session and ask
        // for just the envelope -- one bounded attempt -- so the completed
        // work is not thrown away. Only a VALID re-emitted envelope is
        // adopted; anything else leaves the original result untouched so a
        // genuinely broken agent still finalizes as a protocol failure.
        let salvage = self
            .run_once(&request, ENVELOPE_SALVAGE_PROMPT, Some(&session_id))
            .await;
        if salvage.envelope.as_valid().is_some() {
            merge_salvaged_iteration(base, salvage)
        } else {
            base
        }
    }

    async fn run_once(
        &self,
        request: &InvocationRequest,
        prompt: &str,
        resume: Option<&str>,
    ) -> IterationResult {
        let mut active = match self
            .runtime
            .start(self.run_request(prompt, resume), self.run_options())
            .await
        {
            Ok(active) => active,
            Err(err) => return startup_failure_result(&err),
        };
        let cancel = active.cancel.clone();
        // A harness abort cancels the run; the drain below then ends and
        // `finish` resolves only after the runtime has terminated and
        // reaped the agent's process tree.
        let abort_forward = request.abort.clone().map(|abort| {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                abort.aborted().await;
                cancel.cancel();
            })
        });
        let (stop_tx, stop_rx) = watch::channel(false);
        let watcher = active.control.clone().map(|control| {
            let watcher = self.watcher_for(request);
            tokio::spawn(watcher.run(control, cancel.clone(), stop_rx))
        });

        let (messages, breached) = self.drain_events(&mut active, request).await;
        if breached {
            cancel.cancel();
        }
        let completed = active.finish().await;
        if let Some(forward) = abort_forward {
            forward.abort();
        }
        let _ = stop_tx.send(true);
        let watcher_stop = match watcher {
            Some(handle) => handle.await.unwrap_or(WatcherStop::Finished),
            None => WatcherStop::Finished,
        };

        let mut result = completed_run_to_iteration_result(&completed, messages);
        let cancelled_by_us = result
            .failure
            .as_ref()
            .is_some_and(|f| f.error_type == RUNTIME_CANCELLED);
        if cancelled_by_us {
            match watcher_stop {
                WatcherStop::OperatorInterrupt { command_id } => {
                    override_failure(
                        &mut result,
                        FAILURE_OPERATOR_INTERRUPT,
                        format!(
                            "run interrupted by operator control command {}",
                            command_id.map_or("<unknown>".to_owned(), |id| id.to_string())
                        ),
                    );
                }
                WatcherStop::RecoveryRequested => {
                    override_failure(
                        &mut result,
                        FAILURE_HARNESS_RECOVERY,
                        "run interrupted for a harness-initiated mid-turn recovery".to_owned(),
                    );
                }
                WatcherStop::Finished if breached => {
                    // A transcript hard limit ended the stream: a clean end
                    // of iteration, not a transport failure.
                    result.failure = None;
                }
                WatcherStop::Finished => {}
            }
        }
        result
    }

    fn watcher_for(&self, request: &InvocationRequest) -> ControlCommandWatcher {
        let mut watcher = ControlCommandWatcher::new();
        if let Some(plane) = &self.control_plane {
            watcher = watcher.with_control_plane(plane);
        }
        let observers = &request.observers;
        if let Some(on_applied) = &observers.on_command_applied {
            watcher = watcher.with_on_applied(on_applied.clone());
        }
        if let Some(observer) = &observers.context_observer {
            watcher = watcher.with_context_observer(observer.clone());
        }
        if let Some(signal) = &observers.recovery_interrupt {
            watcher = watcher.with_recovery_signal(signal.clone());
        }
        let started_at = (watcher.clock())();
        let nudge = CheckpointNudge::arm(
            observers.checkpoint_progress_probe.clone(),
            request.checkpoint_nudge_seconds,
            request.agent_iteration_ceiling_seconds,
            started_at,
        );
        watcher.with_checkpoint_nudge(nudge)
    }

    /// Forward every normalized event to the observer and the recorded
    /// message list, stopping the stream (and reporting `true`) at the first
    /// transcript-limit breach. After a breach the remaining buffered events
    /// are still recorded so telemetry stays complete.
    async fn drain_events(
        &self,
        active: &mut ActiveRun,
        request: &InvocationRequest,
    ) -> (Vec<Value>, bool) {
        let on_message = request.observers.on_message.clone();
        let mut messages = Vec::new();
        let record = |msg: Value, messages: &mut Vec<Value>| {
            if let Some(observer) = &on_message {
                guarded(|| observer(&msg));
            }
            messages.push(msg);
        };
        // The same contract as `enforce_transcript_limits`: every forwarded
        // message updates the counter, and the first grader breach ends the
        // stream. Written out as a loop over the channel so the future stays
        // `Send` for the boxed `InvokeFn`.
        let graders = &request.transcript_graders;
        let mut counter = match &self.transcript_clock {
            Some(clock) => TranscriptCounter::start_with_clock(clock.clone()),
            None => TranscriptCounter::start(),
        };
        let mut breached = false;
        while let Some(event) = active.events.recv().await {
            let msg = event_to_message(&event);
            counter.observe(&msg);
            record(msg, &mut messages);
            if graders.is_empty() {
                continue;
            }
            let snapshot = counter.snapshot();
            if graders.iter().any(|g| first_breach(g, &snapshot).is_some()) {
                breached = true;
                break;
            }
        }
        if breached {
            active.cancel.cancel();
            while let Some(event) = active.events.recv().await {
                record(event_to_message(&event), &mut messages);
            }
        }
        (messages, breached)
    }

    /// Package the invoker as the harness [`InvokeFn`].
    pub fn into_invoke_fn(self) -> InvokeFn {
        let invoker = Arc::new(self);
        Arc::new(move |request: InvocationRequest| {
            let invoker = invoker.clone();
            Box::pin(async move { invoker.invoke(request).await })
        })
    }
}

fn override_failure(result: &mut IterationResult, error_type: &str, message: String) {
    let (exit_code, stderr) = result
        .failure
        .take()
        .map(|f| (f.exit_code, f.stderr))
        .unwrap_or((None, None));
    result.failure = Some(InvocationFailure {
        error_type: error_type.to_owned(),
        message,
        exit_code,
        stderr,
    });
}

/// Build the harness [`InvokeFn`] backed by the flywheel-agents runtime.
pub fn make_agents_invoke(invoker: AgentsInvoker) -> InvokeFn {
    invoker.into_invoke_fn()
}

// --- Cold sessions ------------------------------------------------------------------

/// The configuration every cold (judge / summarizer) session starts from:
/// `auto` permissions, the full skill surface, a turn cap, and the model
/// resolved by the caller (`None` falls through to the agent's default).
pub fn cold_session_configuration(
    agent_id: &str,
    model: Option<&str>,
    max_turns: u32,
) -> AgentConfiguration {
    let mut configuration = AgentConfiguration::new(agent_id);
    configuration.model_id = model.map(str::to_owned);
    configuration.permission_policy = PermissionPolicy::Auto;
    configuration.max_turns = Some(max_turns);
    configuration
        .adapter_options
        .insert("skills".into(), json!("all"));
    configuration
}

/// What a cold session produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ColdSessionOutput {
    /// The agent's concatenated assistant text.
    pub text: String,
    /// The model the agent reported on `session.started`, when it did.
    pub model: Option<String>,
    pub usage: Option<Usage>,
    pub session_id: Option<String>,
}

/// A cold session failed before producing its text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ColdSessionError {
    /// The run could not start (unknown agent, spawn failure, policy).
    #[error("{0}")]
    Start(AgentError),
    /// The agent process or transport failed mid-run.
    #[error("{error_type}: {message}")]
    Run {
        error_type: String,
        message: String,
        exit_code: Option<i32>,
        stderr: Option<String>,
    },
}

/// Captures the model the agent announced on `session.started`.
#[derive(Default)]
struct ModelCapture {
    model: Mutex<Option<String>>,
}

impl EventSink for ModelCapture {
    fn on_raw(&self, _event: &RawAgentEvent) {}

    fn on_event(&self, event: &AgentEvent) {
        if event.kind == EventType::SessionStarted
            && let Some(model) = event.str_field("model")
            && let Ok(mut slot) = self.model.lock()
        {
            *slot = Some(model.to_owned());
        }
    }
}

/// A fresh-per-call agent session runner: no session inheritance, cwd set
/// to the worktree supplied per call.
pub struct ColdSession {
    runtime: Arc<AgentRuntime>,
    host: Option<Arc<dyn ExecutionHost>>,
    configuration: AgentConfiguration,
    timeout_seconds: Option<f64>,
}

impl fmt::Debug for ColdSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ColdSession")
            .field("agent_id", &self.configuration.agent_id)
            .field("model_id", &self.configuration.model_id)
            .field("max_turns", &self.configuration.max_turns)
            .field("timeout_seconds", &self.timeout_seconds)
            .finish_non_exhaustive()
    }
}

impl ColdSession {
    /// A cold session for `agent_id` from [`cold_session_configuration`].
    pub fn new(agent_id: &str, model: Option<&str>, max_turns: u32) -> Self {
        Self::with_configuration(cold_session_configuration(agent_id, model, max_turns))
    }

    /// A cold session from an explicit configuration (the caller owns the
    /// permission / skills choices).
    pub fn with_configuration(configuration: AgentConfiguration) -> Self {
        Self {
            runtime: Arc::new(AgentRuntime::new()),
            host: None,
            configuration,
            timeout_seconds: None,
        }
    }

    pub fn with_runtime(mut self, runtime: Arc<AgentRuntime>) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn with_host(mut self, host: Arc<dyn ExecutionHost>) -> Self {
        self.host = Some(host);
        self
    }

    pub fn with_timeout_seconds(mut self, timeout_seconds: Option<f64>) -> Self {
        self.timeout_seconds = timeout_seconds;
        self
    }

    pub fn with_command_override(mut self, argv: Vec<String>) -> Self {
        self.configuration.command_override = Some(argv);
        self
    }

    pub fn with_transport(mut self, transport: &str) -> Self {
        self.configuration
            .adapter_options
            .insert("transport".into(), json!(transport));
        self
    }

    pub fn configuration(&self) -> &AgentConfiguration {
        &self.configuration
    }

    /// Run `prompt` in a fresh session rooted at `worktree`. `model_override`
    /// replaces the configured model for this call only.
    pub async fn run(
        &self,
        prompt: &str,
        worktree: &Path,
        model_override: Option<&str>,
    ) -> Result<ColdSessionOutput, ColdSessionError> {
        let mut configuration = self.configuration.clone();
        if let Some(model) = model_override {
            configuration.model_id = Some(model.to_owned());
        }
        let request = RunRequest {
            prompt: prompt.to_owned(),
            working_directory: worktree.to_path_buf(),
            configuration,
            timeout_seconds: self.timeout_seconds,
        };
        let capture = Arc::new(ModelCapture::default());
        let options = RunOptions {
            sink: Some(capture.clone()),
            host: self.host.clone(),
            approval_resolver: None,
        };
        let completed = self
            .runtime
            .run(request, options)
            .await
            .map_err(ColdSessionError::Start)?;
        if let Some(failure) = &completed.failure {
            return Err(ColdSessionError::Run {
                error_type: failure.error_type.clone(),
                message: failure.message.clone(),
                exit_code: failure.exit_code,
                stderr: failure.stderr.clone(),
            });
        }
        let model = capture.model.lock().ok().and_then(|m| m.clone());
        Ok(ColdSessionOutput {
            text: completed.final_text,
            model,
            usage: completed.usage.as_ref().map(to_core_usage),
            session_id: completed.native_session_id,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A deterministic fake claude-code CLI written to a tempdir at test
    //! time (a POSIX sh script emitting canned stream-json lines), plus the
    //! configuration that runs it through `/bin/sh`.

    use flywheel_agents::{AgentConfiguration, PermissionPolicy};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    pub const SCENARIO_VAR: &str = "CORE_FAKE_SCENARIO";

    /// Scenarios: `envelope` (clean iteration with a valid LOOP_STATUS),
    /// `no_envelope` (clean `end_turn` without an envelope; with
    /// `--resume=sess-core` on argv it re-emits just the envelope),
    /// `turns` (four assistant turns then a long sleep), `inject` (echoes
    /// the second stdin line's content), `control` (acknowledges one
    /// control request and reports its subtype), `interrupt` (acknowledges
    /// one control request then sleeps), `crash` (exit 3), `verdict`
    /// (a RUBRIC_VERDICT naming the `--model` / `--max-turns` it received),
    /// `handoff` (a RECOVERY_HANDOFF), `no_verdict` (prose only), `tree`
    /// (records a graceful SIGTERM in `terminated.marker` and leaves a
    /// SIGTERM-ignoring grandchild whose pid lands in `grandchild.pid`).
    pub const FAKE_CLAUDE: &str = r#"#!/bin/sh
IFS= read -r FIRST_LINE || true
MODEL=default; MAXT=none; PREV=""
for a in "$@"; do
  if [ "$PREV" = "--model" ]; then MODEL="$a"; fi
  if [ "$PREV" = "--max-turns" ]; then MAXT="$a"; fi
  PREV="$a"
done
emit() { printf '%s\n' "$1"; }
init() { emit '{"type":"system","subtype":"init","session_id":"sess-core","model":"claude-fake-1"}'; }
text() { emit '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$1"'"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}}'; }
result() { emit '{"type":"result","subtype":"success","is_error":false,"result":"'"$1"'","num_turns":'"$2"',"total_cost_usd":0.01,"usage":{"input_tokens":7,"output_tokens":2}}'; }
extract() { printf '%s' "$1" | sed -n 's/.*"'"$2"'":"\([^"]*\)".*/\1/p'; }
ack() {
  IFS= read -r CONTROL || true
  REQ=$(extract "$CONTROL" request_id)
  SUB=$(extract "$CONTROL" subtype)
  emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$REQ"'","response":{"ok":true}}}'
}
case "${CORE_FAKE_SCENARIO:-envelope}" in
envelope)
  init
  text 'done\n<!-- LOOP_STATUS -->\n{\"intent\": \"verify\", \"reason\": \"complete\"}\n<!-- /LOOP_STATUS -->'
  result "done" 1
  ;;
no_envelope)
  init
  case "$*" in
  *--resume=sess-core*)
    text '<!-- LOOP_STATUS -->\n{\"intent\": \"verify\", \"reason\": \"salvaged\"}\n<!-- /LOOP_STATUS -->'
    result "salvaged" 1
    ;;
  *)
    text 'did the work, forgot the envelope'
    result "did the work" 1
    ;;
  esac
  ;;
turns)
  init
  text "turn one"
  text "turn two"
  text "turn three"
  text "turn four"
  sleep 20
  result "late" 4
  ;;
inject)
  init
  text "ready"
  IFS= read -r SECOND || true
  text "$(extract "$SECOND" content)"
  text '<!-- LOOP_STATUS -->\n{\"intent\": \"verify\", \"reason\": \"injected\"}\n<!-- /LOOP_STATUS -->'
  result "injected" 2
  ;;
control)
  init
  text "working"
  ack
  text "control:$SUB"
  text '<!-- LOOP_STATUS -->\n{\"intent\": \"verify\", \"reason\": \"acknowledged\"}\n<!-- /LOOP_STATUS -->'
  result "acknowledged" 2
  ;;
interrupt)
  init
  text "working"
  ack
  sleep 20
  result "late" 1
  ;;
crash)
  init
  text "about to crash"
  echo "fake-agent: fatal explosion" >&2
  exit 3
  ;;
tree)
  init
  trap 'echo graceful > terminated.marker; exit 143' TERM
  (trap '' TERM; exec sleep 300) &
  echo $! > grandchild.pid
  sleep 60 &
  wait $!
  ;;
verdict)
  init
  text 'Looks right.\n<!-- RUBRIC_VERDICT -->\n{\"passed\": true, \"summary\": \"model='"$MODEL"' turns='"$MAXT"'\"}\n<!-- /RUBRIC_VERDICT -->'
  result "verdict" 1
  ;;
no_verdict)
  init
  text "I could not decide."
  result "undecided" 1
  ;;
handoff)
  init
  text '<!-- RECOVERY_HANDOFF -->\n{\"work_done\": \"model='"$MODEL"'\", \"work_remaining\": \"wire harness\", \"key_decisions\": \"cold session\", \"suggested_next_step\": \"run tests\"}\n<!-- /RECOVERY_HANDOFF -->'
  result "handoff" 1
  ;;
*)
  echo "fake agent: unknown scenario ${CORE_FAKE_SCENARIO}" >&2
  exit 64
  ;;
esac
"#;

    pub fn write_fake_claude(dir: &Path) -> PathBuf {
        let path = dir.join("fake_claude.sh");
        std::fs::write(&path, FAKE_CLAUDE).expect("write fake agent");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake agent");
        }
        path
    }

    /// A claude-code configuration that runs the fake through `/bin/sh`
    /// (never exec-ing the freshly written file: parallel spawns can briefly
    /// hold the write fd) under `scenario`.
    pub fn fake_configuration(script: &Path, scenario: &str) -> AgentConfiguration {
        let mut configuration = AgentConfiguration::new("claude-code");
        configuration.permission_policy = PermissionPolicy::Auto;
        configuration.command_override = Some(vec![
            "/bin/sh".into(),
            script.to_string_lossy().into_owned(),
        ]);
        configuration
            .environment
            .insert(SCENARIO_VAR.into(), scenario.into());
        configuration
            .adapter_options
            .insert("control_timeout_seconds".into(), json!(5));
        configuration
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{fake_configuration, write_fake_claude};
    use super::*;
    use crate::envelope::Intent;
    use crate::events::CommandApplied;
    use crate::invoker_client::{enqueue_interrupt, enqueue_say, enqueue_set_model};
    use crate::store::{ControlCommandRecord, ControlCommandStore, JsonObject};
    use crate::store_memory::InMemoryStore;
    use chrono::Utc;
    use flywheel_agents::{
        AgentExit, PermissionDenial, RateLimitInfo, RunFailure, StopInfo, StopReason,
        ToolInteraction as AgentToolInteraction, ToolResult,
    };
    use std::time::{Duration, Instant};

    fn completed() -> CompletedRun {
        CompletedRun {
            final_text: "did the work".into(),
            stop: StopInfo {
                reason: StopReason::Completed,
                native: Some("end_turn".into()),
                native_subtype: Some("success".into()),
                is_error: false,
                pending_tool_use: false,
                finished: true,
            },
            usage: Some(flywheel_agents::Usage::from([
                ("input_tokens".to_string(), 10),
                ("output_tokens".to_string(), 5),
            ])),
            total_cost_usd: Some(0.25),
            num_turns: Some(3),
            native_session_id: Some("sess-42".into()),
            tool_interactions: vec![AgentToolInteraction {
                tool_use_id: "t1".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command": "ls"}).as_object().unwrap().clone(),
                result: Some(ToolResult {
                    tool_use_id: "t1".into(),
                    is_error: Some(false),
                    content: Some(json!("ok")),
                }),
            }],
            permission_denials: vec![PermissionDenial {
                tool_name: Some("Write".into()),
                detail: JsonObject::new(),
            }],
            rate_limit_events: vec![RateLimitInfo {
                resets_at_epoch: Some(1751990400.0),
                detail: JsonObject::new(),
            }],
            fault: None,
            failure: None,
            exit: AgentExit {
                returncode: Some(0),
                stderr_tail: None,
                duration_seconds: Some(0.1),
            },
            event_count: 4,
        }
    }

    // --- the fold --------------------------------------------------------------

    #[test]
    fn fold_maps_signals_fields() {
        let result = completed_run_to_iteration_result(&completed(), Vec::new());
        assert_eq!(result.transcript, "did the work");
        assert!(result.messages.is_empty());
        assert_eq!(
            result.usage,
            Some(Usage::from([
                ("input_tokens".to_string(), 10),
                ("output_tokens".to_string(), 5)
            ]))
        );
        let signals = &result.signals;
        assert_eq!(signals.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(signals.result_subtype.as_deref(), Some("success"));
        assert_eq!(signals.result_is_error, Some(false));
        assert_eq!(signals.num_turns, Some(3));
        assert_eq!(signals.total_cost_usd, Some(0.25));
        assert_eq!(signals.session_id.as_deref(), Some("sess-42"));
        assert!(!signals.pending_tool_use_at_stop);
        assert_eq!(signals.api_error_status, None);
        assert_eq!(signals.tool_interactions.len(), 1);
        let interaction = &signals.tool_interactions[0];
        assert_eq!(interaction.tool_name, "Bash");
        assert_eq!(interaction.tool_input.get("command"), Some(&json!("ls")));
        let observed = interaction.result.as_ref().unwrap();
        assert_eq!(observed.is_error, Some(false));
        assert_eq!(observed.content, Some(ToolResultContent::Text("ok".into())));
        assert_eq!(signals.tool_result_blocks[0].tool_use_id, "t1");
        assert_eq!(signals.permission_denials.len(), 1);
        assert_eq!(signals.permission_denials[0]["tool_name"], "Write");
        assert_eq!(signals.rate_limit_events.len(), 1);
        assert_eq!(
            signals.rate_limit_events[0]["resets_at_epoch"],
            1751990400.0
        );
        assert!(signals.hook_events.is_empty());
        assert_eq!(result.failure, None);
        assert_eq!(result.envelope, EnvelopeResult::Missing);
    }

    #[test]
    fn fold_unfinished_run_reports_none_result_error() {
        let mut run = completed();
        run.stop = StopInfo {
            reason: StopReason::Unknown,
            finished: false,
            ..StopInfo::default()
        };
        run.failure = Some(RunFailure {
            error_type: "agent_exit".into(),
            message: "agent exited 3".into(),
            exit_code: Some(3),
            stderr: Some("boom".into()),
        });
        run.num_turns = Some(-1);
        let result = completed_run_to_iteration_result(&run, Vec::new());
        assert_eq!(result.signals.result_is_error, None);
        assert_eq!(result.signals.num_turns, None);
        let failure = result.failure.unwrap();
        assert_eq!(failure.error_type, "agent_exit");
        assert_eq!(failure.exit_code, Some(3));
        assert_eq!(failure.stderr.as_deref(), Some("boom"));
    }

    #[test]
    fn fold_coerces_nonstandard_tool_result_content() {
        let mut run = completed();
        run.tool_interactions = vec![
            AgentToolInteraction {
                tool_use_id: "t2".into(),
                tool_name: "Read".into(),
                tool_input: JsonObject::new(),
                result: Some(ToolResult {
                    tool_use_id: "t2".into(),
                    is_error: None,
                    content: Some(json!([{"type": "text", "text": "x"}, "stray"])),
                }),
            },
            AgentToolInteraction {
                tool_use_id: "t3".into(),
                tool_name: "Read".into(),
                tool_input: JsonObject::new(),
                result: Some(ToolResult {
                    tool_use_id: "t3".into(),
                    is_error: None,
                    content: Some(json!({"nested": true})),
                }),
            },
            AgentToolInteraction {
                tool_use_id: "t4".into(),
                tool_name: "Read".into(),
                tool_input: JsonObject::new(),
                result: None,
            },
        ];
        let result = completed_run_to_iteration_result(&run, Vec::new());
        let blocks = &result.signals.tool_result_blocks;
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[0].content,
            Some(ToolResultContent::Blocks(vec![
                json!({"type": "text", "text": "x"})
                    .as_object()
                    .unwrap()
                    .clone()
            ]))
        );
        assert_eq!(
            blocks[1].content,
            Some(ToolResultContent::Text(r#"{"nested":true}"#.into()))
        );
        assert!(result.signals.tool_interactions[2].result.is_none());
    }

    #[test]
    fn fold_reads_api_error_status_and_envelope_from_run() {
        let mut run = completed();
        run.final_text =
            "x\n<!-- LOOP_STATUS -->\n{\"intent\": \"continue\"}\n<!-- /LOOP_STATUS -->".into();
        run.usage = Some(flywheel_agents::Usage::from([(
            "input_tokens".to_string(),
            -4,
        )]));
        let messages = vec![
            json!({"type": "message.assistant", "payload": {"text": "x"}}),
            json!({"type": "session.finished", "payload": {"api_error_status": 529}}),
        ];
        let result = completed_run_to_iteration_result(&run, messages);
        assert_eq!(result.signals.api_error_status, Some(529));
        assert_eq!(result.envelope.as_valid().unwrap().intent, Intent::Continue);
        assert_eq!(result.usage.unwrap()["input_tokens"], 0);
        assert_eq!(result.messages.len(), 2);
    }

    #[test]
    fn event_to_message_carries_type_and_payload() {
        let event = AgentEvent::with_native(
            EventType::AssistantMessage,
            "assistant",
            json!({"text": "hi"}),
        );
        let message = event_to_message(&event);
        assert_eq!(message["type"], "message.assistant");
        assert_eq!(message["payload"]["text"], "hi");
        assert_eq!(message["native_type"], "assistant");
    }

    #[test]
    fn startup_failure_carries_runtime_code() {
        let result = startup_failure_result(&AgentError::UnknownAgent("no such agent".into()));
        let failure = result.failure.unwrap();
        assert_eq!(failure.error_type, "unknown_agent");
        assert_eq!(failure.message, "no such agent");
        assert_eq!(result.envelope, EnvelopeResult::Missing);
    }

    #[test]
    fn invocation_request_defaults_and_debug() {
        let request = InvocationRequest::new("go");
        assert_eq!(request.prompt, "go");
        assert!(request.transcript_graders.is_empty());
        assert_eq!(request.resume_session_id, None);
        let rendered = format!("{request:?}");
        assert!(rendered.contains("prompt_len: 2"));
        assert!(rendered.contains("on_message: false"));
    }

    #[test]
    fn cold_session_configuration_is_auto_with_all_skills() {
        let cfg = cold_session_configuration("claude-code", Some("opus"), 8);
        assert_eq!(cfg.permission_policy, PermissionPolicy::Auto);
        assert_eq!(cfg.max_turns, Some(8));
        assert_eq!(cfg.model_id.as_deref(), Some("opus"));
        assert_eq!(cfg.option_str("skills"), Some("all"));
    }

    // --- end to end through the real runtime + fake CLI ---------------------------

    struct Fixture {
        _dir: tempfile::TempDir,
        script: PathBuf,
        working_directory: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let script = write_fake_claude(dir.path());
        let working_directory = dir.path().to_path_buf();
        Fixture {
            _dir: dir,
            script,
            working_directory,
        }
    }

    fn invoker(fx: &Fixture, scenario: &str) -> AgentsInvoker {
        AgentsInvoker::new(
            fake_configuration(&fx.script, scenario),
            fx.working_directory.clone(),
        )
        .with_timeout_seconds(Some(30.0))
    }

    type AuditLog = Arc<Mutex<Vec<(String, JsonObject)>>>;

    fn control_plane(store: &Arc<InMemoryStore>) -> (ControlPlane, AuditLog) {
        let log: AuditLog = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let store: Arc<dyn ControlCommandStore + Send + Sync> = store.clone();
        let plane = ControlPlane::new(store, "run-1")
            .with_poll_interval(Duration::from_millis(10))
            .with_audit_emit(Arc::new(move |kind: &str, payload: &JsonObject| {
                sink.lock()
                    .unwrap()
                    .push((kind.to_owned(), payload.clone()));
            }));
        (plane, log)
    }

    #[tokio::test]
    async fn end_to_end_envelope_signals_and_observer() {
        let fx = fixture();
        let observed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = observed.clone();
        let mut request = InvocationRequest::new("do the thing");
        request.observers.on_message = Some(Arc::new(move |msg| {
            sink.lock().unwrap().push(msg.clone());
        }));
        let result = invoker(&fx, "envelope").invoke(request).await;
        assert!(result.failure.is_none(), "{:?}", result.failure);
        assert_eq!(result.envelope.as_valid().unwrap().intent, Intent::Verify);
        assert_eq!(result.signals.session_id.as_deref(), Some("sess-core"));
        assert_eq!(result.signals.result_subtype.as_deref(), Some("success"));
        assert_eq!(result.signals.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(result.signals.result_is_error, Some(false));
        assert_eq!(result.signals.num_turns, Some(1));
        assert_eq!(
            result.usage,
            Some(Usage::from([
                ("input_tokens".to_string(), 7),
                ("output_tokens".to_string(), 2),
                ("cache_creation_input_tokens".to_string(), 0),
                ("cache_read_input_tokens".to_string(), 0),
            ]))
        );
        let observed = observed.lock().unwrap();
        assert!(!observed.is_empty(), "on_message never called");
        assert_eq!(observed.as_slice(), result.messages.as_slice());
        assert!(
            result
                .messages
                .iter()
                .any(|m| m["type"] == "session.finished")
        );
    }

    #[tokio::test]
    async fn transcript_hard_limit_cancels_the_run_mid_stream() {
        let fx = fixture();
        let mut request = InvocationRequest::new("keep going");
        request.transcript_graders = vec![TranscriptGrader {
            max_turns: Some(2),
            ..TranscriptGrader::default()
        }];
        let started = Instant::now();
        let result = invoker(&fx, "turns").invoke(request).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        // A clean end of iteration: no transport failure, no terminal
        // envelope (the agent was cut off before its result line). Events
        // the agent had already buffered past the breach are still recorded
        // for telemetry, so at least the breaching turn is observed.
        assert_eq!(result.failure, None);
        assert_eq!(result.signals.result_subtype, None);
        assert_eq!(result.signals.stop_reason, None);
        assert_eq!(result.envelope, EnvelopeResult::Missing);
        let turns = result
            .messages
            .iter()
            .filter(|m| m["type"] == "message.assistant")
            .count();
        assert!(turns >= 3, "{turns}");
        assert!(result.transcript.contains("turn three"));
        assert!(!result.transcript.contains("late"));
    }

    #[tokio::test]
    async fn say_command_reaches_the_live_agent() {
        let fx = fixture();
        let store = Arc::new(InMemoryStore::new());
        enqueue_say(store.as_ref(), "run-1", "please focus", Utc::now()).unwrap();
        let (plane, log) = control_plane(&store);
        let facts: Arc<Mutex<Vec<(ControlCommandRecord, CommandApplied)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let sink = facts.clone();
        let mut request = InvocationRequest::new("go");
        request.observers.on_command_applied = Some(Arc::new(move |record, fact| {
            sink.lock().unwrap().push((record.clone(), fact.clone()));
        }));
        let result = invoker(&fx, "inject")
            .with_control_plane(plane)
            .invoke(request)
            .await;
        assert!(result.failure.is_none(), "{:?}", result.failure);
        assert!(
            result.transcript.contains("please focus"),
            "{}",
            result.transcript
        );
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, "harness.control_command_applied");
        assert_eq!(log[0].1["kind"], "say");
        let facts = facts.lock().unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1.command_kind, "say");
        assert_eq!(facts[0].1.command_id, Some(1));
        // Claim-once: the row is no longer pending.
        assert!(
            store
                .claim_commands("run-1", Utc::now())
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn set_model_command_reaches_the_live_agent() {
        let fx = fixture();
        let store = Arc::new(InMemoryStore::new());
        enqueue_set_model(store.as_ref(), "run-1", Some("claude-other"), Utc::now()).unwrap();
        let (plane, log) = control_plane(&store);
        let result = invoker(&fx, "control")
            .with_control_plane(plane)
            .invoke(InvocationRequest::new("go"))
            .await;
        assert!(result.failure.is_none(), "{:?}", result.failure);
        assert!(
            result.transcript.contains("control:set_model"),
            "{}",
            result.transcript
        );
        assert_eq!(log.lock().unwrap()[0].1["kind"], "set_model");
    }

    #[tokio::test]
    async fn operator_interrupt_surfaces_as_failure() {
        let fx = fixture();
        let store = Arc::new(InMemoryStore::new());
        enqueue_interrupt(store.as_ref(), "run-1", Utc::now()).unwrap();
        let (plane, log) = control_plane(&store);
        let started = Instant::now();
        let result = invoker(&fx, "interrupt")
            .with_control_plane(plane)
            .invoke(InvocationRequest::new("go"))
            .await;
        assert!(started.elapsed() < Duration::from_secs(10));
        let failure = result.failure.unwrap();
        assert_eq!(failure.error_type, FAILURE_OPERATOR_INTERRUPT);
        assert!(
            failure.message.contains("control command 1"),
            "{}",
            failure.message
        );
        assert_eq!(log.lock().unwrap()[0].0, "harness.control_command_applied");
        assert!(result.transcript.contains("working"));
    }

    #[tokio::test]
    async fn recovery_request_surfaces_as_failure() {
        let fx = fixture();
        let signal = RecoverySignal::new();
        signal.request();
        let mut request = InvocationRequest::new("go");
        request.observers.recovery_interrupt = Some(signal);
        let started = Instant::now();
        let result = invoker(&fx, "interrupt").invoke(request).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(result.failure.unwrap().error_type, FAILURE_HARNESS_RECOVERY);
    }

    #[tokio::test]
    async fn context_observer_receives_live_readings() {
        let fx = fixture();
        let readings: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = readings.clone();
        let mut request = InvocationRequest::new("go");
        request.observers.context_observer = Some(Arc::new(move |reading| {
            sink.lock().unwrap().push(reading.clone());
        }));
        // The `control` scenario answers exactly one control request (the
        // watcher's first context-usage read) and reports its subtype.
        let result = invoker(&fx, "control").invoke(request).await;
        assert!(result.failure.is_none(), "{:?}", result.failure);
        assert!(result.transcript.contains("control:get_context_usage"));
        assert!(result.envelope.as_valid().is_some());
        assert_eq!(readings.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn lost_envelope_is_salvaged_by_resuming_the_session() {
        let fx = fixture();
        let result = invoker(&fx, "no_envelope")
            .invoke(InvocationRequest::new("go"))
            .await;
        assert!(result.failure.is_none(), "{:?}", result.failure);
        let envelope = result.envelope.as_valid().expect("salvaged envelope");
        assert_eq!(envelope.intent, Intent::Verify);
        assert_eq!(envelope.reason.as_deref(), Some("salvaged"));
        assert!(
            result
                .transcript
                .starts_with("did the work, forgot the envelope\n")
        );
        assert_eq!(result.signals.num_turns, Some(2));
        assert_eq!(
            result
                .messages
                .iter()
                .filter(|m| m["type"] == "session.finished")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn salvage_can_be_disabled() {
        let fx = fixture();
        let result = invoker(&fx, "no_envelope")
            .with_envelope_salvage(false)
            .invoke(InvocationRequest::new("go"))
            .await;
        assert_eq!(result.envelope, EnvelopeResult::Missing);
        assert_eq!(result.signals.num_turns, Some(1));
    }

    #[tokio::test]
    async fn crash_surfaces_exit_code_and_stderr() {
        let fx = fixture();
        let result = invoker(&fx, "crash")
            .invoke(InvocationRequest::new("go"))
            .await;
        let failure = result.failure.unwrap();
        assert_eq!(failure.error_type, "agent_exit");
        assert_eq!(failure.exit_code, Some(3));
        assert!(failure.stderr.unwrap().contains("fatal explosion"));
        assert_eq!(result.transcript, "about to crash");
        assert_eq!(result.envelope, EnvelopeResult::Missing);
    }

    #[tokio::test]
    async fn unknown_agent_is_a_startup_failure() {
        let fx = fixture();
        let mut configuration = fake_configuration(&fx.script, "envelope");
        configuration.agent_id = "nope".into();
        let result = AgentsInvoker::new(configuration, fx.working_directory.clone())
            .invoke(InvocationRequest::new("go"))
            .await;
        assert_eq!(result.failure.unwrap().error_type, "unknown_agent");
    }

    #[tokio::test]
    async fn invoke_fn_adapter_drives_an_iteration() {
        let fx = fixture();
        let invoke = make_agents_invoke(invoker(&fx, "envelope"));
        let result = invoke(InvocationRequest::new("go")).await;
        assert!(result.envelope.as_valid().is_some());
    }

    #[tokio::test]
    async fn cold_session_reports_text_model_and_usage() {
        let fx = fixture();
        let session = ColdSession::with_configuration(fake_configuration(&fx.script, "verdict"))
            .with_timeout_seconds(Some(30.0));
        let output = session
            .run("judge this", &fx.working_directory, Some("claude-judge"))
            .await
            .unwrap();
        assert!(output.text.contains("RUBRIC_VERDICT"));
        assert!(
            output.text.contains("model=claude-judge"),
            "{}",
            output.text
        );
        assert_eq!(output.model.as_deref(), Some("claude-fake-1"));
        assert_eq!(output.session_id.as_deref(), Some("sess-core"));
        assert_eq!(output.usage.unwrap()["input_tokens"], 7);
    }

    #[tokio::test]
    async fn cold_session_surfaces_run_and_start_failures() {
        let fx = fixture();
        let session = ColdSession::with_configuration(fake_configuration(&fx.script, "crash"));
        let err = session
            .run("x", &fx.working_directory, None)
            .await
            .unwrap_err();
        match err {
            ColdSessionError::Run {
                error_type,
                exit_code,
                ..
            } => {
                assert_eq!(error_type, "agent_exit");
                assert_eq!(exit_code, Some(3));
            }
            other => panic!("{other:?}"),
        }
        let mut configuration = fake_configuration(&fx.script, "verdict");
        configuration.agent_id = "nope".into();
        let err = ColdSession::with_configuration(configuration)
            .run("x", &fx.working_directory, None)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ColdSessionError::Start(AgentError::UnknownAgent(_))
        ));
        assert!(err.to_string().contains("nope"));
    }
}
