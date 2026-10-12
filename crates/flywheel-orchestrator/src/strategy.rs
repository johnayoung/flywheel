//! The landing seam: how finished work leaves the loop.
//!
//! A *submit strategy* is the consumer code between "agent finished" and
//! "result landed": branches, worktrees, merges, pull requests, review
//! gates, or nothing at all. It stays out of core by design
//! (`docs/strategy.md`): the orchestrator owns selection and execution
//! order; what a terminal status *means* for the repository is the
//! strategy's call.
//!
//! The seam is two hooks the orchestrator invokes around every run:
//!
//! * [`SubmitStrategy::prepare_sandbox`] provisions the directory the task
//!   runs in (a git worktree, a container mount, a plain dir). It may fail:
//!   the task is skipped for the session and peers are never starved.
//! * [`SubmitStrategy::submit`] acts on the run's terminal status while the
//!   task's lease is still held, so two workers never land the same task
//!   concurrently. It cannot fail at the type level: it returns the
//!   [`SubmitOutcome`] it recorded, and [`submit_guarded`] additionally
//!   converts a panicking implementation into a `submit-error` park so a
//!   landing failure never unwinds the orchestrator and abandons peers.
//!
//! The git-worktree, pull-request, phase, and container strategies live in
//! consumer crates. This module ships the contract, the handle shape a
//! backend returns, and [`PlainDirectoryStrategy`], the no-landing default
//! the orchestrator falls back to when no strategy is wired.

use crate::sources::{GraderReceipt, WorkItem};
use flywheel_core::events::PARK_KIND_SUBMIT_ERROR;
use flywheel_core::lifecycle::Status;
use flywheel_core::task::Task;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// --- Requests -----------------------------------------------------------------

/// Whether a sandbox is being provisioned for a fresh run or to continue a
/// parked lifecycle (a resume/recheck), so a git-aware provider can rebase a
/// parked worktree on resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    Fresh,
    Resume,
}

impl SandboxMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            SandboxMode::Fresh => "fresh",
            SandboxMode::Resume => "resume",
        }
    }
}

impl fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the orchestrator needs a consumer to provision a run's sandbox.
///
/// `task_file` is the backing file for file-backed work sources and `None`
/// otherwise (the Python seam carries an empty path there); `source_ref`
/// always carries the source's opaque item handle. `run_id` is `None` for a
/// fresh run and the lifecycle being continued on a resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxRequest {
    pub task_id: String,
    #[serde(default)]
    pub task_file: Option<PathBuf>,
    #[serde(default)]
    pub run_id: Option<String>,
    pub mode: SandboxMode,
    #[serde(default)]
    pub source_ref: String,
}

impl SandboxRequest {
    /// A fresh-run request for `item` (no lifecycle yet).
    pub fn fresh(item: &WorkItem) -> Self {
        Self {
            task_id: item.task.id.clone(),
            task_file: item.local_path.clone(),
            run_id: None,
            mode: SandboxMode::Fresh,
            source_ref: item.source_ref.clone(),
        }
    }

    /// A resume request for `item` continuing lifecycle `run_id`.
    pub fn resume(item: &WorkItem, run_id: impl Into<String>) -> Self {
        Self {
            run_id: Some(run_id.into()),
            mode: SandboxMode::Resume,
            ..Self::fresh(item)
        }
    }
}

/// The terminal outcome of one run, handed to a strategy's submit step.
///
/// `task` is the validated core task the run was driven with: everything a
/// strategy needs to re-verify the work against the tree it is about to
/// land on (the graders) or render the outcome elsewhere (the goal), with
/// no assumption of a file-backed source. `sandbox` is exactly the path the
/// matching `prepare_sandbox` returned. `receipts` are the final attempt's
/// grader verdicts (the same projection the work source's report receives),
/// best-effort: empty when the projection failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitRequest {
    pub task_id: String,
    #[serde(default)]
    pub task_file: Option<PathBuf>,
    pub task: Task,
    pub run_id: String,
    pub status: Status,
    pub sandbox: PathBuf,
    #[serde(default)]
    pub source_ref: String,
    #[serde(default)]
    pub receipts: Vec<GraderReceipt>,
}

impl SubmitRequest {
    /// A request with the required fields; `task_id` is taken from `task`.
    pub fn new(
        task: Task,
        run_id: impl Into<String>,
        status: Status,
        sandbox: impl Into<PathBuf>,
    ) -> Self {
        Self {
            task_id: task.id.clone(),
            task_file: None,
            task,
            run_id: run_id.into(),
            status,
            sandbox: sandbox.into(),
            source_ref: String::new(),
            receipts: Vec::new(),
        }
    }

    /// Stamp the work item's provenance (`local_path`, `source_ref`).
    pub fn with_item(mut self, item: &WorkItem) -> Self {
        self.task_file = item.local_path.clone();
        self.source_ref = item.source_ref.clone();
        self
    }

    pub fn with_receipts(mut self, receipts: Vec<GraderReceipt>) -> Self {
        self.receipts = receipts;
        self
    }
}

// --- Invocation seam --------------------------------------------------------------

/// The invocation seam is owned by core: the request type, the future, and
/// the shareable invoke function are re-exported here so sandbox backends
/// key their [`InvokeWrapper`] on the same types the harness drives.
pub use flywheel_core::agents_invoke::{InvocationRequest, InvokeFn, InvokeFuture};

/// Wraps the run's base invoke so the iteration executes in the backend.
///
/// The base is `None` in normal operation (the orchestrator is driven
/// without an explicit invoke and the agent invoker is built downstream); a
/// replacing wrapper ignores it, an augmenting one composes it.
pub type InvokeWrapper = Arc<dyn Fn(Option<InvokeFn>) -> InvokeFn + Send + Sync>;

/// Disposes a provisioned environment after the run lands. Invoked through
/// [`SandboxHandle::run_teardown`], which contains a panic.
pub type Teardown = Arc<dyn Fn() + Send + Sync>;

/// A teardown panicked. The panic was contained; this carries its message
/// so the worker can log it and move on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("sandbox teardown panicked: {0}")]
pub struct TeardownPanic(pub String);

/// Render a panic payload as the message it was raised with.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

// --- SandboxHandle --------------------------------------------------------------------

/// A provisioned sandbox plus how a run should attach to it.
///
/// A worktree backend needs only `path` (build one via `From<PathBuf>`;
/// the contributions stay empty, byte-identical to the pre-handle
/// behavior). A non-worktree backend (a container) populates the optional
/// fields so the orchestrator runs the agent *inside* the environment:
///
/// * `env_contribution` is merged onto the policy-resolved agent env via
///   [`apply_env`](Self::apply_env); the handle wins on key collision.
/// * `invoke_wrapper` wraps the run's [`InvokeFn`] via
///   [`wrap_invoke`](Self::wrap_invoke); `None` runs in-process.
/// * `teardown` disposes the environment after `submit`, before the lease
///   releases, via [`run_teardown`](Self::run_teardown). It is per-task by
///   construction (the provider captures the container identity in the
///   closure), which is why it lives on the handle rather than the shared
///   strategy. `None` means there is nothing to dispose.
#[derive(Clone)]
pub struct SandboxHandle {
    pub path: PathBuf,
    pub env_contribution: BTreeMap<String, String>,
    pub invoke_wrapper: Option<InvokeWrapper>,
    pub teardown: Option<Teardown>,
}

impl fmt::Debug for SandboxHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxHandle")
            .field("path", &self.path)
            .field("env_contribution", &self.env_contribution)
            .field("invoke_wrapper", &self.invoke_wrapper.is_some())
            .field("teardown", &self.teardown.is_some())
            .finish()
    }
}

impl SandboxHandle {
    /// A bare handle: `path` with no contributions.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            env_contribution: BTreeMap::new(),
            invoke_wrapper: None,
            teardown: None,
        }
    }

    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env_contribution = env;
        self
    }

    pub fn with_invoke_wrapper(mut self, wrapper: InvokeWrapper) -> Self {
        self.invoke_wrapper = Some(wrapper);
        self
    }

    pub fn with_teardown(mut self, teardown: Teardown) -> Self {
        self.teardown = Some(teardown);
        self
    }

    /// True when the handle carries anything beyond its path.
    pub fn has_contributions(&self) -> bool {
        !self.env_contribution.is_empty()
            || self.invoke_wrapper.is_some()
            || self.teardown.is_some()
    }

    /// Merge the handle's env contribution onto `base`: the handle wins on
    /// collision. `base` is never mutated; an empty contribution yields an
    /// equal copy.
    pub fn apply_env(&self, base: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut merged = base.clone();
        for (key, value) in &self.env_contribution {
            merged.insert(key.clone(), value.clone());
        }
        merged
    }

    /// Hand `base` to the handle's wrapper, or pass it through unchanged
    /// when there is none. A replacing wrapper is applied even to a `None`
    /// base (the normal case).
    pub fn wrap_invoke(&self, base: Option<InvokeFn>) -> Option<InvokeFn> {
        match &self.invoke_wrapper {
            Some(wrapper) => Some(wrapper(base)),
            None => base,
        }
    }

    /// Run the teardown, if any, containing a panic. Never unwinds into the
    /// caller; returns the contained panic's message so it can be logged.
    pub fn run_teardown(&self) -> Result<(), TeardownPanic> {
        let Some(teardown) = &self.teardown else {
            return Ok(());
        };
        catch_unwind(AssertUnwindSafe(|| teardown()))
            .map_err(|payload| TeardownPanic(panic_message(payload.as_ref())))
    }
}

impl From<PathBuf> for SandboxHandle {
    fn from(path: PathBuf) -> Self {
        Self::new(path)
    }
}

impl From<&Path> for SandboxHandle {
    fn from(path: &Path) -> Self {
        Self::new(path)
    }
}

// --- Errors and outcomes -----------------------------------------------------------

/// `prepare_sandbox` could not provision the run's directory. The task is
/// skipped for the session; `message` is operator-facing and `source`
/// carries the underlying cause when one exists.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PrepareError {
    pub message: String,
    #[source]
    pub source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl PrepareError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl From<std::io::Error> for PrepareError {
    fn from(err: std::io::Error) -> Self {
        Self::with_source(err.to_string(), err)
    }
}

/// What `submit` did with the run. Every variant is a recorded fact, not an
/// error: a strategy that could not land reports a park, never a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum SubmitOutcome {
    /// The work landed (fast-forward merge, opened PR): `strategy` names
    /// the mechanism and `landed_ref` the commit / PR it landed as.
    Landed {
        strategy: String,
        landed_ref: String,
    },
    /// The work was parked for forensics; `park_kind` is one of the closed
    /// `LANDING_PARK_KINDS` vocabulary and `detail` is operator-readable.
    Parked { park_kind: String, detail: String },
    /// Nothing to land and nothing parked (a non-landing terminal status, an
    /// empty diff, or a strategy with no landing notion).
    NoOp { reason: String },
}

impl SubmitOutcome {
    pub fn landed(strategy: impl Into<String>, landed_ref: impl Into<String>) -> Self {
        SubmitOutcome::Landed {
            strategy: strategy.into(),
            landed_ref: landed_ref.into(),
        }
    }

    pub fn parked(park_kind: impl Into<String>, detail: impl Into<String>) -> Self {
        SubmitOutcome::Parked {
            park_kind: park_kind.into(),
            detail: detail.into(),
        }
    }

    pub fn no_op(reason: impl Into<String>) -> Self {
        SubmitOutcome::NoOp {
            reason: reason.into(),
        }
    }

    pub fn is_landed(&self) -> bool {
        matches!(self, SubmitOutcome::Landed { .. })
    }

    pub fn is_parked(&self) -> bool {
        matches!(self, SubmitOutcome::Parked { .. })
    }
}

/// A landing strategy's read-only verdict on a finished run's change.
///
/// `landable` is true iff the sandbox is clean and there is at least one
/// commit with a non-empty diff against the base (decision D-3 of spec
/// 00061); `reason` names why not and is empty for a landable verdict.
/// Producing it must not mutate, merge, park, rebase, or clean up anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandabilityVerdict {
    pub landable: bool,
    #[serde(default)]
    pub reason: String,
}

impl LandabilityVerdict {
    /// The verdict a strategy with no diff notion yields for everything it
    /// finishes: always landable, so non-code task classes opt out of the
    /// landable-change gate entirely.
    pub fn landable() -> Self {
        Self {
            landable: true,
            reason: String::new(),
        }
    }

    pub fn not_landable(reason: impl Into<String>) -> Self {
        Self {
            landable: false,
            reason: reason.into(),
        }
    }
}

impl Default for LandabilityVerdict {
    fn default() -> Self {
        Self::landable()
    }
}

// --- The strategy contract --------------------------------------------------------

/// One landing policy, bundled: provision the sandbox, land the outcome.
///
/// Every method takes `&self` so one instance can be shared across workers;
/// per-task state belongs in the returned [`SandboxHandle`].
pub trait SubmitStrategy: Send + Sync {
    /// Provision the directory `request`'s task runs in. An error skips the
    /// task for the session.
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError>;

    /// Act on the run's terminal status under the task's lease. Records its
    /// own outcome; it has no error channel by design.
    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome;

    /// Whether the finished run produced a landable change. Read-only. The
    /// default is always-landable, for strategies with no notion of a diff.
    fn is_landable(&self, request: &SubmitRequest) -> LandabilityVerdict {
        let _ = request;
        LandabilityVerdict::landable()
    }
}

/// The standalone provisioning hook, for consumers wiring callables rather
/// than a bundled strategy.
pub type SandboxProvider =
    Arc<dyn Fn(&SandboxRequest) -> Result<SandboxHandle, PrepareError> + Send + Sync>;

/// The standalone landing hook, for consumers wiring callables rather than
/// a bundled strategy.
pub type Submitter = Arc<dyn Fn(&SubmitRequest) -> SubmitOutcome + Send + Sync>;

/// Call `strategy.prepare_sandbox`, converting a panic into a
/// [`PrepareError`] so a misbehaving provider skips the task instead of
/// unwinding the session.
pub fn prepare_sandbox_guarded(
    strategy: &dyn SubmitStrategy,
    request: &SandboxRequest,
) -> Result<SandboxHandle, PrepareError> {
    match catch_unwind(AssertUnwindSafe(|| strategy.prepare_sandbox(request))) {
        Ok(result) => result,
        Err(payload) => Err(PrepareError::new(format!(
            "prepare_sandbox panicked for {}: {}",
            request.task_id,
            panic_message(payload.as_ref())
        ))),
    }
}

/// Call `strategy.submit`, converting a panic into a `submit-error` park so
/// a landing failure never unwinds the orchestrator and abandons peers.
pub fn submit_guarded(strategy: &dyn SubmitStrategy, request: &SubmitRequest) -> SubmitOutcome {
    match catch_unwind(AssertUnwindSafe(|| strategy.submit(request))) {
        Ok(outcome) => outcome,
        Err(payload) => SubmitOutcome::parked(
            PARK_KIND_SUBMIT_ERROR,
            format!(
                "submit panicked for {}: {}",
                request.task_id,
                panic_message(payload.as_ref())
            ),
        ),
    }
}

/// A strategy assembled from the two standalone callables.
#[derive(Clone)]
pub struct CallableStrategy {
    pub prepare: SandboxProvider,
    pub submit: Submitter,
}

impl CallableStrategy {
    pub fn new(prepare: SandboxProvider, submit: Submitter) -> Self {
        Self { prepare, submit }
    }
}

impl fmt::Debug for CallableStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CallableStrategy")
    }
}

impl SubmitStrategy for CallableStrategy {
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
        (self.prepare)(request)
    }

    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome {
        (self.submit)(request)
    }
}

/// The default when no strategy is wired: each task runs in
/// `sandbox_root/<task-id>` (created on demand) and nothing lands.
///
/// Suitable for research / config / non-code task classes with no landing
/// notion; always landable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainDirectoryStrategy {
    pub sandbox_root: PathBuf,
}

impl PlainDirectoryStrategy {
    pub fn new(sandbox_root: impl Into<PathBuf>) -> Self {
        Self {
            sandbox_root: sandbox_root.into(),
        }
    }

    /// The directory a task runs in under this strategy.
    pub fn sandbox_for(&self, task_id: &str) -> PathBuf {
        self.sandbox_root.join(task_id)
    }
}

impl SubmitStrategy for PlainDirectoryStrategy {
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
        let sandbox = self.sandbox_for(&request.task_id);
        std::fs::create_dir_all(&sandbox).map_err(|err| {
            PrepareError::with_source(
                format!("cannot create sandbox {}: {err}", sandbox.display()),
                err,
            )
        })?;
        Ok(SandboxHandle::new(sandbox))
    }

    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome {
        SubmitOutcome::no_op(format!(
            "plain-directory strategy lands nothing (status={}, sandbox={})",
            request.status,
            request.sandbox.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_core::envelope::EnvelopeResult;
    use flywheel_core::invoker::InvocationSignals;
    use flywheel_core::invoker::IterationResult;
    use flywheel_core::task::{CommandGrader, Grader};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn task(id: &str) -> Task {
        Task::new(
            format!("goal for {id}"),
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id(id)
    }

    fn item(id: &str) -> WorkItem {
        let mut item = WorkItem::new(task(id), format!("ref:{id}"));
        item.local_path = Some(PathBuf::from(format!("/tasks/{id}.json")));
        item
    }

    fn request() -> InvocationRequest {
        InvocationRequest {
            attempt_number: 1,
            iteration_number: 1,
            ..InvocationRequest::new("do it")
        }
    }

    fn result(transcript: &str) -> IterationResult {
        IterationResult {
            transcript: transcript.into(),
            messages: Vec::new(),
            envelope: EnvelopeResult::Missing,
            signals: InvocationSignals::default(),
            failure: None,
            usage: None,
        }
    }

    fn invoke(transcript: &'static str) -> InvokeFn {
        Arc::new(move |_req| Box::pin(async move { result(transcript) }))
    }

    // --- SandboxHandle -----------------------------------------------------------

    #[test]
    fn from_path_wraps_bare_path_with_empty_contributions() {
        let dir = tempfile::tempdir().unwrap();
        let handle = SandboxHandle::from(dir.path().to_path_buf());
        assert_eq!(handle.path, dir.path());
        assert!(handle.env_contribution.is_empty());
        assert!(handle.invoke_wrapper.is_none());
        assert!(handle.teardown.is_none());
        assert!(!handle.has_contributions());
        let from_ref = SandboxHandle::from(dir.path());
        assert_eq!(from_ref.path, handle.path);
    }

    #[test]
    fn apply_env_empty_contribution_is_identity() {
        let base = BTreeMap::from([("BASE".to_string(), "b".to_string())]);
        let handle = SandboxHandle::new("/sb");
        assert_eq!(handle.apply_env(&base), base);
    }

    #[test]
    fn apply_env_merges_and_handle_wins_without_mutating_base() {
        let base = BTreeMap::from([
            ("BASE".to_string(), "b".to_string()),
            ("SHARED".to_string(), "policy".to_string()),
        ]);
        let handle = SandboxHandle::new("/sb").with_env(BTreeMap::from([
            ("SHARED".to_string(), "handle".to_string()),
            ("EXTRA".to_string(), "x".to_string()),
        ]));
        let merged = handle.apply_env(&base);
        assert_eq!(
            merged,
            BTreeMap::from([
                ("BASE".to_string(), "b".to_string()),
                ("SHARED".to_string(), "handle".to_string()),
                ("EXTRA".to_string(), "x".to_string()),
            ])
        );
        assert_eq!(base["SHARED"], "policy");
        assert!(handle.has_contributions());
    }

    #[tokio::test]
    async fn wrap_invoke_without_wrapper_passes_base_through() {
        let base = invoke("in-process");
        let handle = SandboxHandle::new("/sb");
        let wrapped = handle.wrap_invoke(Some(base.clone())).unwrap();
        assert!(Arc::ptr_eq(&wrapped, &base));
        assert!(handle.wrap_invoke(None).is_none());
        assert_eq!(wrapped(request()).await.transcript, "in-process");
    }

    #[tokio::test]
    async fn wrap_invoke_hands_base_to_wrapper_and_uses_its_result() {
        let seen: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        let wrapper: InvokeWrapper = Arc::new(move |inner| {
            *sink.lock().unwrap() = Some(inner.is_some());
            invoke("ran-in-sandbox")
        });
        let handle = SandboxHandle::new("/sb").with_invoke_wrapper(wrapper);
        let wrapped = handle.wrap_invoke(Some(invoke("base"))).unwrap();
        assert_eq!(wrapped(request()).await.transcript, "ran-in-sandbox");
        assert_eq!(*seen.lock().unwrap(), Some(true));
    }

    #[tokio::test]
    async fn wrap_invoke_applies_replacing_wrapper_to_none_base() {
        // Normal operation drives with no explicit invoke; a container
        // backend's replacing wrapper must still be applied.
        let seen: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        let wrapper: InvokeWrapper = Arc::new(move |inner| {
            *sink.lock().unwrap() = Some(inner.is_some());
            invoke("in-container")
        });
        let handle = SandboxHandle::new("/sb").with_invoke_wrapper(wrapper);
        let wrapped = handle.wrap_invoke(None).unwrap();
        assert_eq!(wrapped(request()).await.transcript, "in-container");
        assert_eq!(*seen.lock().unwrap(), Some(false));
    }

    #[test]
    fn run_teardown_invokes_closure_and_is_noop_without_one() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let handle = SandboxHandle::new("/sb").with_teardown(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(handle.run_teardown(), Ok(()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(SandboxHandle::new("/sb").run_teardown(), Ok(()));
    }

    #[test]
    fn run_teardown_contains_a_panic() {
        let handle =
            SandboxHandle::new("/sb").with_teardown(Arc::new(|| panic!("docker rm exploded")));
        let err = handle.run_teardown().unwrap_err();
        assert_eq!(err, TeardownPanic("docker rm exploded".into()));
        assert!(err.to_string().contains("docker rm exploded"));
    }

    #[test]
    fn handle_debug_hides_closures() {
        let handle = SandboxHandle::new("/sb").with_teardown(Arc::new(|| {}));
        let rendered = format!("{handle:?}");
        assert!(rendered.contains("teardown: true"));
        assert!(rendered.contains("invoke_wrapper: false"));
    }

    // --- Requests ---------------------------------------------------------------------

    #[test]
    fn sandbox_request_from_item_carries_provenance() {
        let fresh = SandboxRequest::fresh(&item("alpha"));
        assert_eq!(fresh.task_id, "alpha");
        assert_eq!(
            fresh.task_file.as_deref(),
            Some(Path::new("/tasks/alpha.json"))
        );
        assert_eq!(fresh.source_ref, "ref:alpha");
        assert_eq!(fresh.mode, SandboxMode::Fresh);
        assert_eq!(fresh.run_id, None);
        let resume = SandboxRequest::resume(&item("alpha"), "run-1");
        assert_eq!(resume.mode, SandboxMode::Resume);
        assert_eq!(resume.run_id.as_deref(), Some("run-1"));
        let json = serde_json::to_value(&resume).unwrap();
        assert_eq!(json["mode"], "resume");
        let back: SandboxRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, resume);
    }

    #[test]
    fn submit_request_builders_stamp_item_and_receipts() {
        let req = SubmitRequest::new(task("alpha"), "run-1", Status::Done, "/sb/alpha")
            .with_item(&item("alpha"))
            .with_receipts(vec![GraderReceipt {
                ordinal: 0,
                grader_type: flywheel_core::task::GraderType::Command,
                name: None,
                passed: true,
            }]);
        assert_eq!(req.task_id, "alpha");
        assert_eq!(req.source_ref, "ref:alpha");
        assert_eq!(
            req.task_file.as_deref(),
            Some(Path::new("/tasks/alpha.json"))
        );
        assert_eq!(req.receipts.len(), 1);
        let back: SubmitRequest =
            serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    // --- Outcomes and errors --------------------------------------------------------

    #[test]
    fn submit_outcome_serializes_tagged() {
        let landed = SubmitOutcome::landed("merge", "abc123");
        let json = serde_json::to_value(&landed).unwrap();
        assert_eq!(json["result"], "landed");
        assert_eq!(json["landed_ref"], "abc123");
        assert!(landed.is_landed() && !landed.is_parked());
        let parked: SubmitOutcome = serde_json::from_value(serde_json::json!({
            "result": "parked", "park_kind": "divergent-base", "detail": "x"
        }))
        .unwrap();
        assert!(parked.is_parked());
        let noop = SubmitOutcome::no_op("nothing");
        assert_eq!(serde_json::to_value(&noop).unwrap()["result"], "no_op");
    }

    #[test]
    fn landability_default_is_landable() {
        assert_eq!(
            LandabilityVerdict::default(),
            LandabilityVerdict::landable()
        );
        let no = LandabilityVerdict::not_landable("uncommitted tree");
        assert!(!no.landable);
        assert_eq!(no.reason, "uncommitted tree");
    }

    #[test]
    fn prepare_error_keeps_source() {
        let err: PrepareError = std::io::Error::other("disk full").into();
        assert!(err.to_string().contains("disk full"));
        assert!(std::error::Error::source(&err).is_some());
        assert!(std::error::Error::source(&PrepareError::new("plain")).is_none());
    }

    // --- Strategies -----------------------------------------------------------------

    #[test]
    fn plain_directory_strategy_provisions_and_lands_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let strategy = PlainDirectoryStrategy::new(dir.path().join("sandboxes"));
        let handle = strategy
            .prepare_sandbox(&SandboxRequest::fresh(&item("alpha")))
            .unwrap();
        assert_eq!(handle.path, dir.path().join("sandboxes").join("alpha"));
        assert!(handle.path.is_dir());
        assert!(!handle.has_contributions());

        let req = SubmitRequest::new(task("alpha"), "run-1", Status::Done, &handle.path);
        let outcome = strategy.submit(&req);
        assert!(matches!(&outcome, SubmitOutcome::NoOp { reason } if reason.contains("done")));
        assert!(strategy.is_landable(&req).landable);
        assert!(handle.path.is_dir());
    }

    #[test]
    fn plain_directory_strategy_reports_unprovisionable_root() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"file").unwrap();
        let strategy = PlainDirectoryStrategy::new(&blocker);
        let err = strategy
            .prepare_sandbox(&SandboxRequest::fresh(&item("alpha")))
            .unwrap_err();
        assert!(err.message.contains("cannot create sandbox"));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn strategy_is_object_safe_and_callable_strategy_delegates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let submitted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&submitted);
        let strategy: Box<dyn SubmitStrategy> = Box::new(CallableStrategy::new(
            Arc::new(move |req| Ok(SandboxHandle::new(root.join(&req.task_id)))),
            Arc::new(move |req| {
                sink.lock().unwrap().push(req.task_id.clone());
                SubmitOutcome::landed("merge", "deadbeef")
            }),
        ));
        let handle = strategy
            .prepare_sandbox(&SandboxRequest::fresh(&item("beta")))
            .unwrap();
        assert_eq!(handle.path, dir.path().join("beta"));
        let outcome = strategy.submit(&SubmitRequest::new(
            task("beta"),
            "run-2",
            Status::Done,
            &handle.path,
        ));
        assert_eq!(outcome, SubmitOutcome::landed("merge", "deadbeef"));
        assert_eq!(*submitted.lock().unwrap(), vec!["beta".to_string()]);
        assert_eq!(
            format!(
                "{:?}",
                CallableStrategy::new(
                    Arc::new(|_| Err(PrepareError::new("x"))),
                    Arc::new(|_| SubmitOutcome::no_op("y")),
                )
            ),
            "CallableStrategy"
        );
    }

    struct Exploding;

    impl SubmitStrategy for Exploding {
        fn prepare_sandbox(&self, _: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
            panic!("worktree add failed catastrophically")
        }

        fn submit(&self, _: &SubmitRequest) -> SubmitOutcome {
            panic!("git merge segfaulted")
        }
    }

    #[test]
    fn submit_guarded_converts_panic_into_submit_error_park() {
        let req = SubmitRequest::new(task("alpha"), "run-1", Status::Done, "/sb");
        let outcome = submit_guarded(&Exploding, &req);
        match outcome {
            SubmitOutcome::Parked { park_kind, detail } => {
                assert_eq!(park_kind, PARK_KIND_SUBMIT_ERROR);
                assert!(detail.contains("alpha"));
                assert!(detail.contains("git merge segfaulted"));
            }
            other => panic!("expected a park, got {other:?}"),
        }
        // A well-behaved strategy passes through untouched.
        let dir = tempfile::tempdir().unwrap();
        let plain = PlainDirectoryStrategy::new(dir.path());
        assert!(matches!(
            submit_guarded(&plain, &req),
            SubmitOutcome::NoOp { .. }
        ));
    }

    #[test]
    fn prepare_sandbox_guarded_converts_panic_into_prepare_error() {
        let err = prepare_sandbox_guarded(&Exploding, &SandboxRequest::fresh(&item("alpha")))
            .unwrap_err();
        assert!(err.message.contains("alpha"));
        assert!(err.message.contains("worktree add failed"));
    }

    #[test]
    fn panic_message_handles_both_payload_kinds() {
        let s: Box<dyn std::any::Any + Send> = Box::new("static");
        assert_eq!(panic_message(s.as_ref()), "static");
        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("owned"));
        assert_eq!(panic_message(owned.as_ref()), "owned");
        let other: Box<dyn std::any::Any + Send> = Box::new(7u8);
        assert_eq!(panic_message(other.as_ref()), "non-string panic payload");
    }
}
