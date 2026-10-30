//! The runtime: orchestrates one run over any adapter.
//!
//! [`AgentRuntime::run`] is the supported product contract: start the
//! agent, stamp and fan out every event (raw and normalized share one
//! monotonic sequence), fold to a [`CompletedRun`], classify vendor faults,
//! and always terminate the process tree. [`AgentRuntime::start`] is the
//! streaming form: the same drive loop on a background task, events
//! forwarded over a channel, plus handles to cancel and to steer mid-turn.
//!
//! Hang policing is deliberately not here: flywheel's harness owns
//! watchdogs and resets them on every event the sink observes.
//! `RunRequest.timeout_seconds` is a hard wall-clock ceiling.

use crate::adapter::{
    AdapterServices, AgentAdapter, AgentControl, AgentOptions, ApprovalResolver, DiscoveryContext,
    RunningAgent,
};
use crate::errors::AgentError;
use crate::events::{AgentEvent, EventSink, NullSink, RunRecorder};
use crate::fold::EventFolder;
use crate::hosts::{ExecutionHost, LocalHost};
use crate::models::{
    AgentProbeResult, CompletedRun, FaultEvidence, RunFailure, RunRequest, StopReason,
};
use crate::registry::{AdapterRegistry, default_registry};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

/// Per-run overrides. Everything is optional; the runtime defaults apply.
#[derive(Clone, Default)]
pub struct RunOptions {
    pub sink: Option<Arc<dyn EventSink>>,
    pub host: Option<Arc<dyn ExecutionHost>>,
    pub approval_resolver: Option<ApprovalResolver>,
}

impl RunOptions {
    pub fn with_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_host(mut self, host: Arc<dyn ExecutionHost>) -> Self {
        self.host = Some(host);
        self
    }

    pub fn with_approval_resolver(mut self, resolver: ApprovalResolver) -> Self {
        self.approval_resolver = Some(resolver);
        self
    }
}

/// Cooperative cancellation for a run started with [`AgentRuntime::start`].
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    notify: Arc<Notify>,
    flag: Arc<AtomicBool>,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

/// A run in flight: a live event stream plus handles to steer and stop it.
///
/// Dropping an `ActiveRun` cancels the run; call [`ActiveRun::finish`] to
/// wait for the folded [`CompletedRun`] instead.
pub struct ActiveRun {
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    pub control: Option<Arc<dyn AgentControl>>,
    pub cancel: CancelHandle,
    completion: JoinHandle<CompletedRun>,
}

impl ActiveRun {
    /// Wait for the run to drain and fold. Events not yet received are
    /// discarded; drain `events` first to observe them all.
    pub async fn finish(mut self) -> CompletedRun {
        let completion = &mut self.completion;
        match completion.await {
            Ok(run) => run,
            Err(join) => CompletedRun {
                final_text: String::new(),
                stop: Default::default(),
                usage: None,
                total_cost_usd: None,
                num_turns: None,
                native_session_id: None,
                tool_interactions: Vec::new(),
                permission_denials: Vec::new(),
                rate_limit_events: Vec::new(),
                fault: None,
                failure: Some(RunFailure {
                    error_type: "driver_panic".into(),
                    message: format!("run driver task failed: {join}"),
                    exit_code: None,
                    stderr: None,
                }),
                exit: Default::default(),
                event_count: 0,
            },
        }
    }
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        if !self.completion.is_finished() {
            self.cancel.cancel();
        }
    }
}

pub struct AgentRuntime {
    registry: Arc<AdapterRegistry>,
    host: Arc<dyn ExecutionHost>,
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AgentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntime")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl AgentRuntime {
    /// The default registry under a [`LocalHost`].
    pub fn new() -> Self {
        Self {
            registry: Arc::new(default_registry()),
            host: Arc::new(LocalHost),
        }
    }

    pub fn with_registry(registry: AdapterRegistry) -> Self {
        Self {
            registry: Arc::new(registry),
            host: Arc::new(LocalHost),
        }
    }

    pub fn with_host(mut self, host: Arc<dyn ExecutionHost>) -> Self {
        self.host = host;
        self
    }

    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }

    pub fn adapter(&self, agent_id: &str) -> Result<Arc<dyn AgentAdapter>, AgentError> {
        self.registry.get(agent_id)
    }

    pub async fn probe(&self, agent_id: &str) -> Result<AgentProbeResult, AgentError> {
        Ok(self.registry.get(agent_id)?.probe().await)
    }

    pub async fn discover_options(
        &self,
        agent_id: &str,
        context: Option<DiscoveryContext>,
    ) -> Result<AgentOptions, AgentError> {
        Ok(self
            .registry
            .get(agent_id)?
            .discover_options(context.unwrap_or_default())
            .await)
    }

    fn prepare(
        &self,
        request: &RunRequest,
        options: &RunOptions,
    ) -> Result<(Arc<dyn AgentAdapter>, AdapterServices), AgentError> {
        let adapter = self.registry.get(&request.configuration.agent_id)?;
        let sink: Arc<dyn EventSink> = options.sink.clone().unwrap_or_else(|| Arc::new(NullSink));
        let recorder = Arc::new(RunRecorder::new(sink, adapter.descriptor().id.clone()));
        let host = options.host.clone().unwrap_or_else(|| self.host.clone());
        let mut services = AdapterServices::new(host, recorder);
        if let Some(resolver) = options.approval_resolver.clone() {
            services = services.with_approval_resolver(resolver);
        }
        Ok((adapter, services))
    }

    /// Run to completion: the supported product contract.
    pub async fn run(
        &self,
        request: RunRequest,
        options: RunOptions,
    ) -> Result<CompletedRun, AgentError> {
        let (adapter, services) = self.prepare(&request, &options)?;
        let recorder = services.recorder.clone();
        let agent = adapter.start(request.clone(), services).await?;
        let cancel = CancelHandle::new();
        Ok(drive(
            adapter,
            agent,
            recorder,
            timeout_of(&request),
            cancel,
            |_| {},
        )
        .await)
    }

    /// Start a run and stream its events; the drive loop runs on a
    /// background task so mid-turn control requests are answered while the
    /// caller consumes events at its own pace.
    pub async fn start(
        &self,
        request: RunRequest,
        options: RunOptions,
    ) -> Result<ActiveRun, AgentError> {
        let (adapter, services) = self.prepare(&request, &options)?;
        let recorder = services.recorder.clone();
        let agent = adapter.start(request.clone(), services).await?;
        let control = agent.control();
        let cancel = CancelHandle::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let timeout = timeout_of(&request);
        let driver_cancel = cancel.clone();
        let completion = tokio::spawn(async move {
            drive(
                adapter,
                agent,
                recorder,
                timeout,
                driver_cancel,
                move |event| {
                    // A dropped receiver only means the caller stopped
                    // listening; the run still folds to completion.
                    let _ = tx.send(event);
                },
            )
            .await
        });
        Ok(ActiveRun {
            events: rx,
            control,
            cancel,
            completion,
        })
    }
}

fn timeout_of(request: &RunRequest) -> Option<Duration> {
    request
        .timeout_seconds
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
}

enum Halt {
    Eof,
    Cancelled,
    TimedOut,
    Failed(AgentError),
}

async fn drive(
    adapter: Arc<dyn AgentAdapter>,
    mut agent: Box<dyn RunningAgent>,
    recorder: Arc<RunRecorder>,
    timeout: Option<Duration>,
    cancel: CancelHandle,
    mut forward: impl FnMut(AgentEvent),
) -> CompletedRun {
    let mut folder = EventFolder::new();
    let deadline = async {
        match timeout {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);
    let halt = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break Halt::Cancelled,
            _ = &mut deadline => break Halt::TimedOut,
            next = agent.next_event() => match next {
                Ok(Some(event)) => {
                    let stamped = recorder.stamp(event);
                    folder.feed(&stamped);
                    forward(stamped);
                }
                Ok(None) => break Halt::Eof,
                Err(err) => break Halt::Failed(err),
            },
        }
    };
    if !matches!(halt, Halt::Eof) {
        agent.cancel().await;
    }
    let exit = agent.wait().await;
    let failure = match halt {
        Halt::TimedOut => Some(RunFailure {
            error_type: "timeout".into(),
            message: format!(
                "run exceeded {}s wall clock",
                timeout.map(|d| d.as_secs_f64()).unwrap_or(0.0)
            ),
            exit_code: exit.returncode,
            stderr: exit.stderr_tail.clone(),
        }),
        Halt::Cancelled => Some(RunFailure {
            error_type: "cancelled".into(),
            message: "run cancelled by the caller".into(),
            exit_code: exit.returncode,
            stderr: exit.stderr_tail.clone(),
        }),
        // Transport failures fold into a structured RunFailure: raw
        // evidence, no classification.
        Halt::Failed(err) => Some(RunFailure {
            error_type: err.code().into(),
            message: err.to_string(),
            exit_code: exit.returncode,
            stderr: exit.stderr_tail.clone(),
        }),
        Halt::Eof => match exit.returncode {
            Some(0) | None => None,
            Some(code) => Some(RunFailure {
                error_type: "agent_exit".into(),
                message: format!("agent exited {code}"),
                exit_code: Some(code),
                stderr: exit.stderr_tail.clone(),
            }),
        },
    };
    let mut interim = folder.completed(exit, failure, None);
    if interim
        .failure
        .as_ref()
        .is_some_and(|f| f.error_type == "cancelled")
        && !interim.stop.finished
    {
        interim.stop.reason = StopReason::Cancelled;
    }
    let fault = adapter.classify_fault(&FaultEvidence {
        final_text: interim.final_text.clone(),
        stderr: interim.exit.stderr_tail.clone(),
        native_stop: interim.stop.native.clone(),
        native_subtype: interim.stop.native_subtype.clone(),
        is_error: interim.stop.is_error,
    });
    interim.fault = fault;
    interim
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BoxFuture;
    use crate::capabilities::AgentCapabilities;
    use crate::config::AgentConfiguration;
    use crate::events::{EventType, MemorySink};
    use crate::models::{AdapterCompatibility, AgentDescriptor, AgentExit, AgentFault};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    fn request(timeout: Option<f64>) -> RunRequest {
        RunRequest {
            prompt: "p".into(),
            working_directory: PathBuf::from("/tmp"),
            configuration: AgentConfiguration::new("claude-code"),
            timeout_seconds: timeout,
        }
    }

    #[test]
    fn timeout_of_rejects_non_finite_and_negative() {
        assert_eq!(timeout_of(&request(None)), None);
        assert_eq!(timeout_of(&request(Some(-1.0))), None);
        assert_eq!(timeout_of(&request(Some(f64::NAN))), None);
        // Finite but past the Duration range: no ceiling, never a panic.
        assert_eq!(timeout_of(&request(Some(1e20))), None);
        assert_eq!(
            timeout_of(&request(Some(2.5))),
            Some(Duration::from_millis(2500))
        );
    }

    #[tokio::test]
    async fn cancel_handle_wakes_waiters() {
        let handle = CancelHandle::new();
        let waiter = handle.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        handle.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(handle.is_cancelled());
        // Already-cancelled handles resolve immediately.
        tokio::time::timeout(Duration::from_millis(100), handle.cancelled())
            .await
            .unwrap();
    }

    /// One scripted step of a [`ScriptedAgent`] stream.
    enum Step {
        Emit(AgentEvent),
        Fail(AgentError),
        Panic,
    }

    /// What a scripted run observed: how often the runtime terminated the
    /// agent and the evidence it handed the fault classifier.
    #[derive(Default)]
    struct Observed {
        cancels: AtomicUsize,
        evidence: Mutex<Vec<FaultEvidence>>,
    }

    /// An adapter whose single run replays `steps` and exits with `exit`.
    struct ScriptedAdapter {
        descriptor: AgentDescriptor,
        steps: Mutex<Option<Vec<Step>>>,
        exit: AgentExit,
        fault: Option<AgentFault>,
        observed: Arc<Observed>,
    }

    impl ScriptedAdapter {
        fn new(steps: Vec<Step>, exit: AgentExit, fault: Option<AgentFault>) -> Self {
            Self {
                descriptor: AgentDescriptor {
                    id: "scripted".into(),
                    display_name: "Scripted".into(),
                    adapter_version: "0".into(),
                    vendor: None,
                    executable_names: Vec::new(),
                    capabilities: AgentCapabilities::default(),
                    compatibility: AdapterCompatibility::default(),
                },
                steps: Mutex::new(Some(steps)),
                exit,
                fault,
                observed: Arc::new(Observed::default()),
            }
        }
    }

    struct ScriptedAgent {
        steps: VecDeque<Step>,
        exit: AgentExit,
        observed: Arc<Observed>,
    }

    impl RunningAgent for ScriptedAgent {
        fn native_session_id(&self) -> Option<String> {
            None
        }

        fn next_event(&mut self) -> BoxFuture<'_, Result<Option<AgentEvent>, AgentError>> {
            let step = self.steps.pop_front();
            Box::pin(async move {
                match step {
                    Some(Step::Emit(event)) => Ok(Some(event)),
                    Some(Step::Fail(err)) => Err(err),
                    Some(Step::Panic) => panic!("scripted adapter panicked mid-stream"),
                    None => Ok(None),
                }
            })
        }

        fn control(&self) -> Option<Arc<dyn AgentControl>> {
            None
        }

        fn cancel(&mut self) -> BoxFuture<'_, ()> {
            self.observed.cancels.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }

        fn wait(&mut self) -> BoxFuture<'_, AgentExit> {
            let exit = self.exit.clone();
            Box::pin(async move { exit })
        }
    }

    impl AgentAdapter for ScriptedAdapter {
        fn descriptor(&self) -> &AgentDescriptor {
            &self.descriptor
        }

        fn probe(&self) -> BoxFuture<'_, AgentProbeResult> {
            Box::pin(async {
                AgentProbeResult::not_installed("scripted adapter has no executable", Vec::new())
            })
        }

        fn discover_options(&self, _context: DiscoveryContext) -> BoxFuture<'_, AgentOptions> {
            Box::pin(async { AgentOptions::default() })
        }

        fn start(
            &self,
            _request: RunRequest,
            _services: AdapterServices,
        ) -> BoxFuture<'_, Result<Box<dyn RunningAgent>, AgentError>> {
            let steps = self
                .steps
                .lock()
                .unwrap()
                .take()
                .expect("scripted adapter started twice");
            let agent: Box<dyn RunningAgent> = Box::new(ScriptedAgent {
                steps: steps.into(),
                exit: self.exit.clone(),
                observed: self.observed.clone(),
            });
            Box::pin(async move { Ok(agent) })
        }

        fn classify_fault(&self, evidence: &FaultEvidence) -> Option<AgentFault> {
            self.observed
                .evidence
                .lock()
                .unwrap()
                .push(evidence.clone());
            self.fault.clone()
        }
    }

    fn scripted_runtime(adapter: ScriptedAdapter) -> (AgentRuntime, Arc<Observed>) {
        let observed = adapter.observed.clone();
        let mut registry = AdapterRegistry::new();
        registry.register(Arc::new(adapter)).unwrap();
        (AgentRuntime::with_registry(registry), observed)
    }

    fn scripted_request() -> RunRequest {
        let mut req = request(None);
        req.configuration.agent_id = "scripted".into();
        req
    }

    fn assistant(text: &str) -> Step {
        Step::Emit(AgentEvent::new(
            EventType::AssistantMessage,
            json!({"text": text}),
        ))
    }

    #[tokio::test]
    async fn transport_error_mid_stream_folds_into_a_run_failure_and_terminates_the_agent() {
        let exit = AgentExit {
            returncode: Some(-9),
            stderr_tail: Some("killed".into()),
            duration_seconds: None,
        };
        let (runtime, observed) = scripted_runtime(ScriptedAdapter::new(
            vec![
                assistant("partial work"),
                Step::Fail(AgentError::AgentProtocol("line exceeds 1 MiB".into())),
                assistant("never folded"),
            ],
            exit,
            None,
        ));
        let sink = MemorySink::new();
        let run = runtime
            .run(
                scripted_request(),
                RunOptions::default().with_sink(Arc::new(sink.clone())),
            )
            .await
            .unwrap();

        assert_eq!(
            run.failure,
            Some(RunFailure {
                error_type: "agent_protocol_error".into(),
                message: "line exceeds 1 MiB".into(),
                exit_code: Some(-9),
                stderr: Some("killed".into()),
            })
        );
        // A failed stream is not a clean EOF: the process tree is torn down.
        assert_eq!(observed.cancels.load(Ordering::SeqCst), 1);
        // Events before the failure are folded and forwarded; nothing after.
        assert_eq!(run.final_text, "partial work");
        assert_eq!(run.event_count, 1);
        assert_eq!(sink.events().len(), 1);
        // An unfinished transport failure is not relabelled as a cancel.
        assert_eq!(run.stop.reason, StopReason::Unknown);
        assert!(!run.stop.finished);
        assert_eq!(observed.evidence.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn clean_eof_keeps_the_agent_and_hands_folded_stop_evidence_to_the_classifier() {
        let fault = AgentFault {
            kind: "session_limit".into(),
            message: "scripted".into(),
            resets_at_epoch: Some(42.0),
        };
        let exit = AgentExit {
            returncode: Some(0),
            stderr_tail: Some("stderr tail".into()),
            duration_seconds: Some(1.5),
        };
        let (runtime, observed) = scripted_runtime(ScriptedAdapter::new(
            vec![
                assistant("hello "),
                assistant("world"),
                Step::Emit(AgentEvent::new(
                    EventType::SessionFinished,
                    json!({
                        "normalized_stop": "error",
                        "stop_reason": "end_turn",
                        "subtype": "success",
                        "is_error": true,
                    }),
                )),
            ],
            exit.clone(),
            Some(fault.clone()),
        ));
        let run = runtime
            .run(scripted_request(), RunOptions::default())
            .await
            .unwrap();

        assert_eq!(run.failure, None);
        assert_eq!(observed.cancels.load(Ordering::SeqCst), 0);
        assert_eq!(run.exit, exit);
        assert_eq!(run.fault, Some(fault));
        assert_eq!(
            *observed.evidence.lock().unwrap(),
            vec![FaultEvidence {
                final_text: "hello world".into(),
                stderr: Some("stderr tail".into()),
                native_stop: Some("end_turn".into()),
                native_subtype: Some("success".into()),
                is_error: true,
            }]
        );
    }

    #[tokio::test]
    async fn clean_eof_with_a_nonzero_exit_is_an_agent_exit_failure() {
        let (runtime, observed) = scripted_runtime(ScriptedAdapter::new(
            vec![assistant("tried")],
            AgentExit {
                returncode: Some(3),
                stderr_tail: Some("fatal".into()),
                duration_seconds: None,
            },
            None,
        ));
        let run = runtime
            .run(scripted_request(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(
            run.failure,
            Some(RunFailure {
                error_type: "agent_exit".into(),
                message: "agent exited 3".into(),
                exit_code: Some(3),
                stderr: Some("fatal".into()),
            })
        );
        assert_eq!(observed.cancels.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_panicking_driver_finishes_as_a_driver_panic_failure() {
        let (runtime, observed) = scripted_runtime(ScriptedAdapter::new(
            vec![assistant("before"), Step::Panic],
            AgentExit::default(),
            None,
        ));
        let mut active = runtime
            .start(scripted_request(), RunOptions::default())
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), active.events.recv())
            .await
            .unwrap()
            .expect("the event before the panic is forwarded");
        assert_eq!(first.str_field("text"), Some("before"));
        let run = tokio::time::timeout(Duration::from_secs(5), active.finish())
            .await
            .unwrap();
        let failure = run.failure.expect("a panicked driver reports a failure");
        assert_eq!(failure.error_type, "driver_panic");
        assert!(
            failure.message.starts_with("run driver task failed: ")
                && failure.message.contains("panic"),
            "{}",
            failure.message
        );
        assert_eq!(failure.exit_code, None);
        assert_eq!(run.event_count, 0);
        assert_eq!(run.final_text, "");
        assert_eq!(run.fault, None);
        // The driver died before classification: no evidence was taken.
        assert!(observed.evidence.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error_before_spawning() {
        let runtime = AgentRuntime::new();
        let mut req = request(None);
        req.configuration.agent_id = "nope".into();
        let err = runtime.run(req, RunOptions::default()).await.unwrap_err();
        assert_eq!(err.code(), "unknown_agent");
    }
}
