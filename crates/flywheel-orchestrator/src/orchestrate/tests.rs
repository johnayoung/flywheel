//! Scenario tests for the orchestrate loop, driven by a scripted `RunTask`
//! runner, real tempdir sandboxes, a scripted `SubmitStrategy`, and the
//! in-memory (and, for the double-claim race, SQLite) stores.

use super::*;
use crate::claims::{
    EVENT_ACQUIRED, EVENT_RELEASED, EVENT_RENEWED, InMemoryClaimStore, STOP_PREPARE_SKIP,
    SqliteClaimStore,
};
use crate::held_out_gate::FilesystemHeldOutGraderSource;
use crate::sources::{WorkReport, WorkSourceError};
use crate::strategy::{LandabilityVerdict, PrepareError, SandboxMode, SubmitOutcome, Teardown};
use crate::work_graph::GraphValidationIssue;
use chrono::TimeZone;
use flywheel_core::envelope::BlockedRequirement;
use flywheel_core::events::{
    DomainEvent, DomainEventPayload, Landed, LandingParked, PARK_KIND_DIVERGENT_BASE,
    PARK_KIND_HELD_OUT_GATE,
};
use flywheel_core::harness::{HarnessOutcome, requires_json, serialize_requires};
use flywheel_core::lifecycle::{Attempt, Lifecycle, Outcome};
use flywheel_core::store::{
    AttemptStore, ControlCommandStore, DomainEventStore, LifecycleFilter, LifecycleStore,
    StoreError,
};
use flywheel_core::store_memory::InMemoryStore;
use flywheel_core::store_sqlite::SqliteStore;
use flywheel_core::task::{CommandGrader, Grader, ManualGrader, Task};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use tempfile::TempDir;

// --- fixtures ------------------------------------------------------------------------

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn fixed(at: DateTime<Utc>) -> Clock {
    Arc::new(move || at)
}

fn task(id: &str) -> Task {
    Task::new(
        format!("Goal for {id}."),
        vec![Grader::Command(CommandGrader::new("true").unwrap())],
    )
    .with_id(id)
}

fn gated_task(id: &str) -> Task {
    Task::new(
        format!("Goal for {id}."),
        vec![
            Grader::Command(CommandGrader::new("true").unwrap()),
            Grader::Manual(ManualGrader::new("Confirm the rollout.").unwrap()),
        ],
    )
    .with_id(id)
}

fn item(id: &str) -> WorkItem {
    WorkItem::new(task(id), format!("ref:{id}"))
}

#[derive(Clone, Default)]
struct Flag(Arc<(Mutex<bool>, Condvar)>);

impl Flag {
    fn set(&self) {
        let (m, cv) = &*self.0;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }

    fn is_set(&self) -> bool {
        *self.0.0.lock().unwrap()
    }

    fn wait(&self, timeout: Duration) -> bool {
        let (m, cv) = &*self.0;
        let guard = m.lock().unwrap();
        let (guard, _) = cv.wait_timeout_while(guard, timeout, |set| !*set).unwrap();
        *guard
    }

    async fn wait_async(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while !self.is_set() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        true
    }
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn sink(&self) -> LogSink {
        let lines = self.0.clone();
        Arc::new(move |line: &str| lines.lock().unwrap().push(line.to_owned()))
    }

    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn contains(&self, needle: &str) -> bool {
        self.lines().iter().any(|l| l.contains(needle))
    }
}

// --- memory work source ---------------------------------------------------------------

type ListHook = Box<dyn Fn(usize) + Send + Sync>;

#[derive(Default)]
struct MemorySource {
    items: Mutex<Vec<WorkItem>>,
    reports: Mutex<Vec<WorkReport>>,
    fail_report: AtomicBool,
    fail_list: AtomicBool,
    list_calls: AtomicUsize,
    on_list: Mutex<Option<ListHook>>,
}

impl MemorySource {
    fn new(items: Vec<WorkItem>) -> Arc<Self> {
        Arc::new(Self {
            items: Mutex::new(items),
            ..Self::default()
        })
    }

    fn remove(&self, task_id: &str) {
        self.items.lock().unwrap().retain(|i| i.task.id != task_id);
    }

    fn reports(&self) -> Vec<WorkReport> {
        self.reports.lock().unwrap().clone()
    }
}

impl WorkSource for MemorySource {
    fn source_kind(&self) -> &str {
        "memory"
    }

    fn source_name(&self) -> String {
        "memory".into()
    }

    fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError> {
        let call = self.list_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(WorkSourceError::new("tracker unavailable"));
        }
        if let Some(hook) = self.on_list.lock().unwrap().as_ref() {
            hook(call);
        }
        Ok(self.items.lock().unwrap().clone())
    }

    fn report(&self, report: &WorkReport) -> Result<(), WorkSourceError> {
        self.reports.lock().unwrap().push(report.clone());
        if self.fail_report.load(Ordering::SeqCst) {
            return Err(WorkSourceError::new("report endpoint down"));
        }
        Ok(())
    }
}

// --- scripted runner ------------------------------------------------------------------

#[derive(Clone)]
enum Scripted {
    /// Walk the lifecycle to `status`, optionally waiting on a flag and/or
    /// sleeping first, then return the outcome.
    Finish {
        status: Status,
        error: String,
        wait: Option<Flag>,
        sleep_ms: u64,
    },
    /// Park `INTERRUPTED` with a persisted `blocked_requires` snapshot.
    Blocked { requires_json: String },
    /// Park `AWAITING_APPROVAL` on the manual gate at `ordinal`.
    Await { ordinal: u32 },
    /// Park `INTERRUPTED` and re-raise as the harness does on an in-band
    /// `interrupt` control command.
    InBandInterrupt,
    /// Wait for the shutdown signal, park `INTERRUPTED`, re-raise.
    WaitShutdown { started: Flag },
    /// Sleep forever (for cancellation tests); `started` fires first.
    Hang { started: Flag },
    /// Fail the run and surface a derived session-limit reset.
    SessionLimit { reset_at: DateTime<Utc> },
    /// Record `RUNNING`, then poll control commands until an `interrupt`
    /// arrives; park `INTERRUPTED` and re-raise.
    PollInterrupt,
    /// Run an arbitrary hook mid-run (after recording RUNNING), then finish
    /// DONE.
    Hook {
        hook: Arc<dyn Fn() + Send + Sync>,
        sleep_ms: u64,
    },
}

fn done() -> Scripted {
    Scripted::Finish {
        status: Status::Done,
        error: String::new(),
        wait: None,
        sleep_ms: 0,
    }
}

fn failed(error: &str) -> Scripted {
    Scripted::Finish {
        status: Status::Failed,
        error: error.to_owned(),
        wait: None,
        sleep_ms: 0,
    }
}

fn done_after(wait: Flag) -> Scripted {
    Scripted::Finish {
        status: Status::Done,
        error: String::new(),
        wait: Some(wait),
        sleep_ms: 0,
    }
}

fn done_slow(sleep_ms: u64) -> Scripted {
    Scripted::Finish {
        status: Status::Done,
        error: String::new(),
        wait: None,
        sleep_ms,
    }
}

#[derive(Clone, Debug)]
struct Call {
    task_id: String,
    run_id: String,
    mode: RunMode,
    sandbox: PathBuf,
    agent_env: BTreeMap<String, String>,
    worker_id: String,
    source_ref: String,
    invoke_present: bool,
    max_retries: u32,
}

#[derive(Default)]
struct ScriptedRunner {
    scripts: Mutex<HashMap<String, VecDeque<Scripted>>>,
    calls: Mutex<Vec<Call>>,
}

impl ScriptedRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn script(self: &Arc<Self>, task_id: &str, steps: Vec<Scripted>) -> Arc<Self> {
        self.scripts
            .lock()
            .unwrap()
            .insert(task_id.to_owned(), steps.into());
        self.clone()
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn next(&self, task_id: &str) -> Scripted {
        let mut scripts = self.scripts.lock().unwrap();
        match scripts.get_mut(task_id) {
            Some(queue) if queue.len() > 1 => queue.pop_front().unwrap(),
            Some(queue) if queue.len() == 1 => queue.front().unwrap().clone(),
            _ => done(),
        }
    }
}

fn walk(lc: &mut Lifecycle, path: &[(Status, &str)], now: DateTime<Utc>) {
    for (status, error) in path {
        lc.transition_to(*status, error, Some(now)).unwrap();
    }
}

fn path_to(target: Status, error: &str) -> Vec<(Status, String)> {
    let e = error.to_owned();
    match target {
        Status::Done => vec![
            (Status::Ready, String::new()),
            (Status::Running, String::new()),
            (Status::Validating, String::new()),
            (Status::Done, String::new()),
        ],
        Status::Failed => vec![
            (Status::Ready, String::new()),
            (Status::Running, String::new()),
            (Status::Failed, e),
        ],
        Status::FailedValidation => vec![
            (Status::Ready, String::new()),
            (Status::Running, String::new()),
            (Status::Validating, String::new()),
            (Status::FailedValidation, e),
        ],
        Status::Interrupted => vec![
            (Status::Ready, String::new()),
            (Status::Running, String::new()),
            (Status::Interrupted, String::new()),
        ],
        Status::AwaitingApproval => vec![
            (Status::Ready, String::new()),
            (Status::Running, String::new()),
            (Status::Validating, String::new()),
            (Status::AwaitingApproval, String::new()),
        ],
        Status::Running => vec![
            (Status::Ready, String::new()),
            (Status::Running, String::new()),
        ],
        Status::Validating => vec![
            (Status::Ready, String::new()),
            (Status::Running, String::new()),
            (Status::Validating, String::new()),
        ],
        other => panic!("unsupported scripted target {other}"),
    }
}

/// Seed (fresh) or advance (resume) the request's lifecycle to `target`
/// in the store, mirroring the harness's own writes.
fn finalize(
    req: &RunTaskRequest,
    target: Status,
    error: &str,
    outcome: Option<Outcome>,
) -> Lifecycle {
    let now = (req.now)();
    let store = req.store.as_ref();
    let mut lc = match req.mode {
        RunMode::Fresh => {
            store.save_task(&req.task, now).unwrap();
            req.lifecycle.clone()
        }
        RunMode::Resume => store.load_lifecycle(req.run_id()).unwrap().unwrap(),
    };
    let prev = lc.version;
    let mut path = path_to(target, error);
    if lc.status == Status::Interrupted {
        // Entry-time normalization: interrupted -> ready.
        path.retain(|(s, _)| *s != Status::Ready);
        lc.transition_to(Status::Ready, "", Some(now)).unwrap();
    } else if lc.status == Status::Ready {
        // The recheck already drove interrupted -> ready.
        path.retain(|(s, _)| *s != Status::Ready);
    } else if lc.status == Status::Running {
        path.retain(|(s, _)| !matches!(s, Status::Ready | Status::Running));
    }
    let owned: Vec<(Status, &str)> = path.iter().map(|(s, e)| (*s, e.as_str())).collect();
    walk(&mut lc, &owned, now);
    match req.mode {
        RunMode::Fresh => store.create_lifecycle(&lc).unwrap(),
        RunMode::Resume => store.update_lifecycle(&lc, prev).unwrap(),
    }
    if let Some(outcome) = outcome {
        let number = store.list_attempts(&lc.run_id).unwrap().len() as u32 + 1;
        let mut attempt = Attempt::new(number, now, lc.run_id.clone());
        attempt.ended_at = Some(now);
        attempt.outcome = Some(outcome);
        attempt.error = error.to_owned();
        store.save_attempt(&lc.run_id, &attempt, None).unwrap();
    }
    lc
}

fn outcome_of(lifecycle: Lifecycle, reset: Option<DateTime<Utc>>) -> HarnessOutcome {
    HarnessOutcome {
        lifecycle,
        attempts: Vec::new(),
        session_limit_reset: reset,
    }
}

impl RunTask for ScriptedRunner {
    fn run(&self, req: RunTaskRequest) -> RunTaskFuture {
        self.calls.lock().unwrap().push(Call {
            task_id: req.task.id.clone(),
            run_id: req.run_id().to_owned(),
            mode: req.mode,
            sandbox: req.sandbox.clone(),
            agent_env: req.agent.agent_env.clone(),
            worker_id: req.worker_id.clone(),
            source_ref: req.source_ref.clone(),
            invoke_present: req.invoke.is_some(),
            max_retries: req.max_retries,
        });
        let step = self.next(&req.task.id);
        Box::pin(async move {
            match step {
                Scripted::Finish {
                    status,
                    error,
                    wait,
                    sleep_ms,
                } => {
                    if let Some(flag) = wait {
                        assert!(
                            flag.wait_async(Duration::from_secs(10)).await,
                            "flag never set"
                        );
                    }
                    if sleep_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                    }
                    let outcome = match status {
                        Status::Done => Outcome::Succeeded,
                        Status::Failed | Status::FailedValidation => Outcome::ValidationFailed,
                        _ => Outcome::InternalError,
                    };
                    Ok(outcome_of(
                        finalize(&req, status, &error, Some(outcome)),
                        None,
                    ))
                }
                Scripted::Blocked { requires_json } => {
                    let mut lc = finalize(&req, Status::Interrupted, "", Some(Outcome::Succeeded));
                    let prev = lc.version;
                    lc.blocked_requires_json = Some(requires_json);
                    lc.version += 1;
                    req.store.update_lifecycle(&lc, prev).unwrap();
                    Ok(outcome_of(lc, None))
                }
                Scripted::Await { ordinal } => {
                    let mut lc =
                        finalize(&req, Status::AwaitingApproval, "", Some(Outcome::Succeeded));
                    let prev = lc.version;
                    lc.awaiting_manual_ordinal = Some(ordinal);
                    lc.version += 1;
                    req.store.update_lifecycle(&lc, prev).unwrap();
                    Ok(outcome_of(lc, None))
                }
                Scripted::InBandInterrupt => {
                    finalize(&req, Status::Interrupted, "", Some(Outcome::InternalError));
                    Err(RunTaskError::Interrupted)
                }
                Scripted::WaitShutdown { started } => {
                    finalize(&req, Status::Running, "", None);
                    started.set();
                    req.shutdown
                        .clone()
                        .expect("shutdown wired")
                        .triggered()
                        .await;
                    let store = req.store.clone();
                    let mut lc = store.load_lifecycle(req.run_id()).unwrap().unwrap();
                    let prev = lc.version;
                    lc.transition_to(Status::Interrupted, "", Some((req.now)()))
                        .unwrap();
                    store.update_lifecycle(&lc, prev).unwrap();
                    Err(RunTaskError::Interrupted)
                }
                Scripted::Hang { started } => {
                    finalize(&req, Status::Running, "", None);
                    started.set();
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Err(RunTaskError::Other("hang ended".into()))
                }
                Scripted::SessionLimit { reset_at } => {
                    let lc = finalize(
                        &req,
                        Status::Failed,
                        "session limit",
                        Some(Outcome::AgentError),
                    );
                    Ok(outcome_of(lc, Some(reset_at)))
                }
                Scripted::PollInterrupt => {
                    finalize(&req, Status::Running, "", None);
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    loop {
                        let commands = req.store.claim_commands(req.run_id(), (req.now)()).unwrap();
                        if commands.iter().any(|c| c.kind == "interrupt") {
                            break;
                        }
                        assert!(std::time::Instant::now() < deadline, "no interrupt arrived");
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    let mut lc = req.store.load_lifecycle(req.run_id()).unwrap().unwrap();
                    let prev = lc.version;
                    lc.transition_to(Status::Interrupted, "", Some((req.now)()))
                        .unwrap();
                    req.store.update_lifecycle(&lc, prev).unwrap();
                    Err(RunTaskError::Interrupted)
                }
                Scripted::Hook { hook, sleep_ms } => {
                    finalize(&req, Status::Running, "", None);
                    hook();
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                    let store = req.store.clone();
                    let mut lc = store.load_lifecycle(req.run_id()).unwrap().unwrap();
                    let prev = lc.version;
                    walk(
                        &mut lc,
                        &[(Status::Validating, ""), (Status::Done, "")],
                        (req.now)(),
                    );
                    store.update_lifecycle(&lc, prev).unwrap();
                    Ok(outcome_of(lc, None))
                }
            }
        })
    }
}

// --- scripted strategy ----------------------------------------------------------------

#[derive(Clone, Debug)]
struct SubmitSeen {
    task_id: String,
    status: Status,
    claim_held_by: Option<String>,
    sandbox: PathBuf,
}

struct ScriptedStrategy {
    root: PathBuf,
    claims: Arc<dyn ClaimStore>,
    control: Arc<dyn HarnessStore>,
    prepare_fail: Mutex<HashSet<String>>,
    prepares: Mutex<Vec<(String, SandboxMode)>>,
    submits: Mutex<Vec<SubmitSeen>>,
    teardowns: Arc<AtomicUsize>,
    /// Per task: the park kinds successive submits should record (`None`
    /// lands).
    parks: Mutex<HashMap<String, VecDeque<Option<String>>>>,
    landable: Mutex<HashMap<String, bool>>,
    /// Tasks whose parks after the first return `Parked` without appending
    /// a `LandingParked` event (a swallowed witness write).
    unrecorded_reparks: Mutex<HashSet<String>>,
}

impl ScriptedStrategy {
    fn new(root: &Path, claims: Arc<dyn ClaimStore>, control: Arc<dyn HarnessStore>) -> Arc<Self> {
        Arc::new(Self {
            root: root.to_path_buf(),
            claims,
            control,
            prepare_fail: Mutex::new(HashSet::new()),
            prepares: Mutex::new(Vec::new()),
            submits: Mutex::new(Vec::new()),
            teardowns: Arc::new(AtomicUsize::new(0)),
            parks: Mutex::new(HashMap::new()),
            landable: Mutex::new(HashMap::new()),
            unrecorded_reparks: Mutex::new(HashSet::new()),
        })
    }

    fn fail_prepare(self: &Arc<Self>, task_id: &str) -> Arc<Self> {
        self.prepare_fail.lock().unwrap().insert(task_id.to_owned());
        self.clone()
    }

    fn park_sequence(self: &Arc<Self>, task_id: &str, parks: Vec<Option<&str>>) -> Arc<Self> {
        self.parks.lock().unwrap().insert(
            task_id.to_owned(),
            parks.into_iter().map(|p| p.map(str::to_owned)).collect(),
        );
        self.clone()
    }

    fn unrecorded_reparks(self: &Arc<Self>, task_id: &str) -> Arc<Self> {
        self.unrecorded_reparks
            .lock()
            .unwrap()
            .insert(task_id.to_owned());
        self.clone()
    }

    fn set_landable(self: &Arc<Self>, task_id: &str, landable: bool) -> Arc<Self> {
        self.landable
            .lock()
            .unwrap()
            .insert(task_id.to_owned(), landable);
        self.clone()
    }

    fn submits(&self) -> Vec<SubmitSeen> {
        self.submits.lock().unwrap().clone()
    }

    fn prepares(&self) -> Vec<(String, SandboxMode)> {
        self.prepares.lock().unwrap().clone()
    }

    fn teardowns(&self) -> usize {
        self.teardowns.load(Ordering::SeqCst)
    }

    fn append(&self, run_id: &str, payload: DomainEventPayload) {
        let lc = self.control.load_lifecycle(run_id).unwrap().unwrap();
        self.control
            .append_domain_event(&DomainEvent::new(run_id, Utc::now(), payload), lc.version)
            .unwrap();
    }
}

impl SubmitStrategy for ScriptedStrategy {
    fn prepare_sandbox(&self, request: &SandboxRequest) -> Result<SandboxHandle, PrepareError> {
        self.prepares
            .lock()
            .unwrap()
            .push((request.task_id.clone(), request.mode));
        if self.prepare_fail.lock().unwrap().contains(&request.task_id) {
            return Err(PrepareError::new(format!(
                "cannot provision {}",
                request.task_id
            )));
        }
        let path = self.root.join(&request.task_id);
        std::fs::create_dir_all(&path)?;
        let counter = self.teardowns.clone();
        let teardown: Teardown = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        Ok(SandboxHandle::new(path)
            .with_env(BTreeMap::from([(
                "SANDBOX_KIND".to_owned(),
                "scripted".to_owned(),
            )]))
            .with_teardown(teardown))
    }

    fn submit(&self, request: &SubmitRequest) -> SubmitOutcome {
        let held = self
            .claims
            .load_claim(&request.task_id)
            .unwrap()
            .map(|c| c.worker_id);
        self.submits.lock().unwrap().push(SubmitSeen {
            task_id: request.task_id.clone(),
            status: request.status,
            claim_held_by: held,
            sandbox: request.sandbox.clone(),
        });
        if request.status != Status::Done {
            return SubmitOutcome::no_op("non-landing status");
        }
        let next = self
            .parks
            .lock()
            .unwrap()
            .get_mut(&request.task_id)
            .and_then(|q| {
                if q.len() > 1 {
                    q.pop_front()
                } else {
                    q.front().cloned()
                }
            });
        match next {
            Some(Some(kind)) => {
                if self
                    .unrecorded_reparks
                    .lock()
                    .unwrap()
                    .contains(&request.task_id)
                    && !landing_parks(self.control.as_ref(), &request.run_id)
                        .unwrap()
                        .is_empty()
                {
                    return SubmitOutcome::parked(kind, "scripted unrecorded park");
                }
                self.append(
                    &request.run_id,
                    DomainEventPayload::LandingParked(LandingParked {
                        park_kind: kind.clone(),
                        detail: format!("scripted park {kind}"),
                        ..LandingParked::default()
                    }),
                );
                SubmitOutcome::parked(kind, "scripted park")
            }
            Some(None) => {
                self.append(
                    &request.run_id,
                    DomainEventPayload::Landed(Landed {
                        strategy: "scripted".into(),
                        landed_ref: "abc123".into(),
                        rung: String::new(),
                        agent_turns: None,
                        agent_wall_seconds: None,
                    }),
                );
                // A landed branch has no commits beyond the base any more:
                // the probe reports it not landable from here on, exactly as
                // a git strategy does once the merge is in.
                self.landable
                    .lock()
                    .unwrap()
                    .insert(request.task_id.clone(), false);
                SubmitOutcome::landed("scripted", "abc123")
            }
            None => SubmitOutcome::no_op("nothing scripted"),
        }
    }

    fn is_landable(&self, request: &SubmitRequest) -> LandabilityVerdict {
        match self.landable.lock().unwrap().get(&request.task_id) {
            Some(false) => LandabilityVerdict::not_landable("scripted: not landable"),
            _ => LandabilityVerdict::landable(),
        }
    }
}

// --- harness ------------------------------------------------------------------------

struct Fixture {
    dir: TempDir,
    control: Arc<InMemoryStore>,
    claims: Arc<InMemoryClaimStore>,
    runner: Arc<ScriptedRunner>,
    log: Log,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            control: Arc::new(InMemoryStore::new()),
            claims: Arc::new(InMemoryClaimStore::new()),
            runner: ScriptedRunner::new(),
            log: Log::default(),
        }
    }

    fn sandbox_root(&self) -> PathBuf {
        self.dir.path().join("sandboxes")
    }

    fn request(&self, source: Arc<MemorySource>) -> OrchestrateRequest {
        let mut req = OrchestrateRequest::new(
            source,
            self.control.clone(),
            self.claims.clone(),
            self.runner.clone(),
            self.sandbox_root(),
        );
        req.max_retries = 0;
        req.max_turns = 4;
        req.log = Some(self.log.sink());
        req.now = Some(fixed(t0()));
        req.worker_id = Some("worker-a".into());
        req.environ = Some(BTreeMap::new());
        req
    }

    fn strategy(&self) -> Arc<ScriptedStrategy> {
        ScriptedStrategy::new(
            &self.sandbox_root(),
            self.claims.clone(),
            self.control.clone(),
        )
    }

    fn lifecycle(&self, run_id: &str) -> Lifecycle {
        self.control.load_lifecycle(run_id).unwrap().unwrap()
    }

    fn seed(&self, task_id: &str, run_id: &str, target: Status, error: &str) -> Lifecycle {
        let mut lc = Lifecycle::new(task_id).with_run_id(run_id);
        let path = path_to(target, error);
        let owned: Vec<(Status, &str)> = path.iter().map(|(s, e)| (*s, e.as_str())).collect();
        walk(&mut lc, &owned, t0());
        self.control.create_lifecycle(&lc).unwrap();
        lc
    }
}

async fn run(req: OrchestrateRequest) -> OrchestratorReport {
    orchestrate(req).await.unwrap()
}

fn ids(report: &OrchestratorReport) -> Vec<&str> {
    report.runs.iter().map(|r| r.task_id.as_str()).collect()
}

fn blocked_requires(path: &Path, present: bool) -> String {
    requires_json(&serialize_requires(&[BlockedRequirement::FileExists {
        path: path.display().to_string(),
        present,
    }]))
}

// --- dispatch order, prerequisites, capabilities ---------------------------------

#[tokio::test]
async fn dispatch_order_follows_priority_then_walk_order_and_capabilities() {
    let f = Fixture::new();
    let mut needs_gpu = item("gpu");
    needs_gpu.required_capabilities = BTreeSet::from(["gpu".to_owned()]);
    let source = MemorySource::new(vec![
        item("low").with_priority(0),
        item("high").with_priority(5),
        item("mid").with_priority(2),
        item("mid-later").with_priority(2),
        needs_gpu,
    ]);
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["high", "mid", "mid-later", "low"]);
    assert!(
        report
            .runs
            .iter()
            .all(|r| r.status == Status::Done && r.mode == RunMode::Fresh)
    );
    // A worker advertising the capability runs the withheld item.
    let f2 = Fixture::new();
    let mut needs_gpu = item("gpu");
    needs_gpu.required_capabilities = BTreeSet::from(["gpu".to_owned()]);
    let mut req = f2.request(MemorySource::new(vec![needs_gpu]));
    req.policy = Some(WorkPolicy {
        execution_capabilities: BTreeSet::from(["gpu".to_owned(), "x".to_owned()]),
        ..WorkPolicy::default()
    });
    assert_eq!(ids(&run(req).await), ["gpu"]);
}

#[tokio::test]
async fn prerequisite_promotion_runs_dependent_after_dependency() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![
        item("child").with_prerequisites(["parent"]),
        item("parent"),
    ]);
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["parent", "child"]);
}

#[tokio::test]
async fn undone_prerequisite_never_runs_the_dependent() {
    let f = Fixture::new();
    f.runner.script("parent", vec![failed("boom")]);
    let source = MemorySource::new(vec![
        item("parent"),
        item("child").with_prerequisites(["parent"]),
    ]);
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["parent"]);
    assert_eq!(report.runs[0].status, Status::Failed);
}

#[tokio::test]
async fn store_done_unlisted_prerequisite_dispatches_dependent_without_witness() {
    let f = Fixture::new();
    f.seed("archived", "run-archived", Status::Done, "");
    let source = MemorySource::new(vec![item("child").with_prerequisites(["archived"])]);
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["child"]);
    assert!(f.claims.list_stop_events().unwrap().is_empty());
    // Only DONE satisfies: a FAILED unlisted prerequisite still dangles.
    let g = Fixture::new();
    g.seed("broken", "run-broken", Status::Failed, "x");
    let report = run(g.request(MemorySource::new(vec![
        item("child").with_prerequisites(["broken"]),
    ])))
    .await;
    assert!(report.runs.is_empty());
    assert_eq!(
        g.claims.list_stop_events().unwrap()[0].kind,
        STOP_DANGLING_PREREQUISITE_STR
    );
}

const STOP_DANGLING_PREREQUISITE_STR: &str = crate::claims::STOP_DANGLING_PREREQUISITE;

#[tokio::test]
async fn dangling_prerequisite_never_runs_and_routes_once_after_bound() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![item("orphan").with_prerequisites(["ghost"])]);
    let mut req = f.request(source.clone());
    req.prereq_redrive_bound = 2;
    for _ in 0..4 {
        let report = run(req.clone()).await;
        assert!(report.runs.is_empty());
    }
    let witnesses = f
        .claims
        .list_subject_stop_events("orphan")
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == STOP_DANGLING_PREREQUISITE_STR)
        .count();
    assert_eq!(witnesses, 2, "no witness past the routing");
    let queue = f.claims.list_human_review_queue().unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].reason, "prerequisite-missing");
    assert!(queue[0].detail.contains("\"ghost\""));
    assert!(
        f.log
            .contains("prerequisite \"ghost\" missing past 2 cycle(s)")
    );
    // The prerequisite appearing resolves the edge and the task is driven.
    source.items.lock().unwrap().push(item("ghost"));
    let report = run(req).await;
    assert_eq!(ids(&report), ["ghost", "orphan"]);
}

#[tokio::test]
async fn structurally_corrupt_tasks_are_isolated_and_peers_still_run() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![
        item("dup"),
        item("dup"),
        item("selfie").with_prerequisites(["selfie"]),
        item("fine"),
    ]);
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["fine"]);
    assert!(
        f.log
            .contains("dup: isolated from this pass's graph (duplicate task id")
    );
    assert!(
        f.log
            .contains("selfie: isolated from this pass's graph (self-dependency")
    );
}

// --- snapshots --------------------------------------------------------------------

#[tokio::test]
async fn each_scheduling_pass_records_exactly_one_snapshot() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![item("a"), item("b").with_prerequisites(["a"])]);
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["a", "b"]);
    let snapshots = f.claims.list_graph_snapshots().unwrap();
    // Two driven passes plus the terminal no-progress pass.
    assert_eq!(snapshots.len(), 3);
    assert!(snapshots.iter().all(|s| s.captured_at == t0()));
    let first = f.claims.list_graph_snapshot_items(snapshots[0].id).unwrap();
    assert_eq!(
        first
            .iter()
            .map(|i| (i.task_id.as_str(), i.state.as_str(), i.ready))
            .collect::<Vec<_>>(),
        [("a", "fresh", true), ("b", "fresh", false)]
    );
    let last = f.claims.list_graph_snapshot_items(snapshots[2].id).unwrap();
    assert!(
        last.iter()
            .all(|i| i.state == "done" && !i.ready && i.claim_holder.is_none())
    );
    assert_eq!(
        last[1].resolved_prerequisites,
        BTreeSet::from(["a".to_owned()])
    );
}

#[tokio::test]
async fn empty_source_records_a_terminal_snapshot_and_skip_unchanged_dedupes() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![]);
    run(f.request(source.clone())).await;
    assert_eq!(f.claims.list_graph_snapshots().unwrap().len(), 1);
    run(f.request(source.clone())).await;
    assert_eq!(f.claims.list_graph_snapshots().unwrap().len(), 2);
    let mut req = f.request(source);
    req.snapshot_policy = GraphSnapshotPolicy::SkipUnchanged;
    run(req.clone()).await;
    run(req).await;
    assert_eq!(
        f.claims.list_graph_snapshots().unwrap().len(),
        2,
        "identical cross-sections are not re-recorded"
    );
}

// --- claims, leases, conflict keys ----------------------------------------------

#[tokio::test]
async fn lease_is_acquired_renewed_and_released_around_a_run() {
    let f = Fixture::new();
    f.runner.script("solo", vec![done_slow(80)]);
    let mut req = f.request(MemorySource::new(vec![item("solo")]));
    req.lease_seconds = 0.03; // heartbeat every 10ms
    req.now = Some(Arc::new(Utc::now));
    let report = run(req).await;
    assert_eq!(ids(&report), ["solo"]);
    assert!(
        f.claims.load_claim("solo").unwrap().is_none(),
        "released after the run"
    );
    let kinds: Vec<String> = f
        .claims
        .list_task_events("solo")
        .unwrap()
        .into_iter()
        .map(|e| e.event_type)
        .collect();
    assert_eq!(kinds.first().map(String::as_str), Some(EVENT_ACQUIRED));
    assert_eq!(kinds.last().map(String::as_str), Some(EVENT_RELEASED));
    assert!(
        kinds.iter().filter(|k| k.as_str() == EVENT_RENEWED).count() >= 2,
        "{kinds:?}"
    );
}

#[tokio::test]
async fn peer_held_claim_is_skipped_not_consumed() {
    let f = Fixture::new();
    let claim = f
        .claims
        .acquire_claim("held", "other-worker", t0(), 3600.0, &BTreeSet::new())
        .unwrap()
        .unwrap();
    let source = MemorySource::new(vec![item("held"), item("free")]);
    let report = run(f.request(source.clone())).await;
    assert_eq!(ids(&report), ["free"]);
    f.claims.release_claim(&claim, Some(t0())).unwrap();
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["held"]);
}

#[tokio::test]
async fn conflict_key_refusal_is_a_skip_not_a_consume() {
    let f = Fixture::new();
    let keys = BTreeSet::from(["src/hot.rs".to_owned()]);
    let claim = f
        .claims
        .acquire_claim("peer-task", "other-worker", t0(), 3600.0, &keys)
        .unwrap()
        .unwrap();
    let source = MemorySource::new(vec![
        item("overlap").with_conflict_keys(["src/hot.rs"]),
        item("free"),
    ]);
    let report = run(f.request(source.clone())).await;
    assert_eq!(ids(&report), ["free"]);
    assert!(
        f.claims
            .list_subject_stop_events("overlap")
            .unwrap()
            .is_empty(),
        "not consumed"
    );
    f.claims.release_claim(&claim, Some(t0())).unwrap();
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["overlap"]);
}

#[tokio::test]
async fn conflicting_tasks_serialize_within_one_session() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![
        item("first").with_conflict_keys(["src/hot.rs"]),
        item("second").with_conflict_keys(["src/hot.rs"]),
    ]);
    let report = run(f.request(source)).await;
    assert_eq!(ids(&report), ["first", "second"]);
}

#[test]
fn acquire_with_grace_defers_a_recent_foreign_lapse_but_not_own_or_old() {
    let claims = InMemoryClaimStore::new();
    let keys = BTreeSet::new();
    claims
        .acquire_claim("t", "dead", t0(), 10.0, &keys)
        .unwrap()
        .unwrap();
    let lapsed = t0() + ChronoDuration::seconds(12);
    // Lapsed 2s ago, grace 5s: treated as held.
    assert!(
        acquire_with_grace(&claims, "t", "w", lapsed, 10.0, &keys, 5.0)
            .unwrap()
            .is_none()
    );
    // The owner re-acquires its own row regardless.
    assert!(
        acquire_with_grace(&claims, "t", "dead", lapsed, 10.0, &keys, 5.0)
            .unwrap()
            .is_some()
    );
    // Past the grace a different worker steals.
    let old = t0() + ChronoDuration::seconds(40);
    let stolen = acquire_with_grace(&claims, "t", "w", old, 10.0, &keys, 5.0)
        .unwrap()
        .unwrap();
    assert_eq!(stolen.worker_id, "w");
    // Grace 0 is the plain store steal.
    claims
        .acquire_claim("u", "dead", t0(), 10.0, &keys)
        .unwrap()
        .unwrap();
    assert!(
        acquire_with_grace(&claims, "u", "w", lapsed, 10.0, &keys, 0.0)
            .unwrap()
            .is_some()
    );
}

// --- the two-worker double-claim race -------------------------------------------

/// Worker B snapshots while every task is fresh, then parks between its
/// snapshot and its first claim until worker A has driven and released the
/// whole set. `release_claim` deletes the row, so B re-acquires each freed
/// claim off its stale snapshot; the post-claim recheck must decline.
fn two_worker_race(
    control_a: Arc<dyn HarnessStore>,
    claims_a: Arc<dyn ClaimStore>,
    control_b: Arc<dyn HarnessStore>,
    claims_b: Arc<dyn ClaimStore>,
    task_ids: &[&str],
    sandbox: &Path,
) -> (OrchestratorReport, OrchestratorReport) {
    let items: Vec<WorkItem> = task_ids.iter().map(|id| item(id)).collect();
    let b_snapshot_taken = Flag::default();
    let a_finished_all = Flag::default();

    let source_a = MemorySource::new(items.clone());
    let n = task_ids.len();
    let signal = a_finished_all.clone();
    *source_a.on_list.lock().unwrap() = Some(Box::new(move |call| {
        if call == n + 1 {
            signal.set();
        }
    }));
    let runner_a = ScriptedRunner::new();
    runner_a.script(task_ids[0], vec![done_after(b_snapshot_taken.clone())]);
    let runner_b = ScriptedRunner::new();

    let flag_b = b_snapshot_taken.clone();
    let wait_a = a_finished_all.clone();
    let clock_b: Clock = Arc::new(move || {
        if !flag_b.is_set() {
            flag_b.set();
            return t0();
        }
        assert!(wait_a.wait(Duration::from_secs(10)), "A never drained");
        t0()
    });

    let mut req_a =
        OrchestrateRequest::new(source_a, control_a, claims_a, runner_a, sandbox.join("a"));
    req_a.worker_id = Some("worker-a".into());
    req_a.lease_seconds = 60.0;
    req_a.max_retries = 0;
    let mut req_b = OrchestrateRequest::new(
        MemorySource::new(items),
        control_b,
        claims_b,
        runner_b,
        sandbox.join("b"),
    );
    req_b.worker_id = Some("worker-b".into());
    req_b.lease_seconds = 60.0;
    req_b.max_retries = 0;
    req_b.now = Some(clock_b);

    let spawn = |req: OrchestrateRequest| {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(orchestrate(req))
                .unwrap()
        })
    };
    let tb = spawn(req_b);
    let ta = spawn(req_a);
    (ta.join().unwrap(), tb.join().unwrap())
}

#[test]
fn two_workers_run_each_task_exactly_once_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let control: Arc<dyn HarnessStore> = Arc::new(InMemoryStore::new());
    let claims: Arc<dyn ClaimStore> = Arc::new(InMemoryClaimStore::new());
    let ids = ["t1", "t2", "t3", "t4"];
    let (a, b) = two_worker_race(
        control.clone(),
        claims.clone(),
        control,
        claims,
        &ids,
        dir.path(),
    );
    let ran_a: BTreeSet<&str> = a.runs.iter().map(|r| r.task_id.as_str()).collect();
    let ran_b: BTreeSet<&str> = b.runs.iter().map(|r| r.task_id.as_str()).collect();
    assert!(ran_a.is_disjoint(&ran_b));
    assert_eq!(&ran_a | &ran_b, ids.iter().copied().collect());
    assert_eq!(a.runs.len() + b.runs.len(), ids.len());
    assert!(
        a.runs
            .iter()
            .chain(b.runs.iter())
            .all(|r| r.status == Status::Done)
    );
}

#[test]
fn fresh_selection_rechecks_terminal_state_under_claim_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("flywheel.sqlite");
    let open = || -> (Arc<dyn HarnessStore>, Arc<dyn ClaimStore>) {
        (
            Arc::new(SqliteStore::open(&db).unwrap()),
            Arc::new(SqliteClaimStore::open(&db).unwrap()),
        )
    };
    let (control_a, claims_a) = open();
    let (control_b, claims_b) = open();
    let (a, b) = two_worker_race(
        control_a,
        claims_a,
        control_b,
        claims_b,
        &["t1"],
        dir.path(),
    );
    assert_eq!(ids(&a), ["t1"]);
    assert!(
        b.runs.is_empty(),
        "B observed the committed DONE under its claim and declined"
    );
    let inspect = SqliteStore::open(&db).unwrap();
    assert_eq!(
        inspect
            .list_lifecycles(&LifecycleFilter::default().with_task_id("t1"))
            .unwrap()
            .len(),
        1,
        "exactly one lifecycle was minted"
    );
}

// --- sandbox, submit, teardown -----------------------------------------------------

#[tokio::test]
async fn prepare_failure_skips_task_for_session_and_witnesses_it() {
    let f = Fixture::new();
    let strategy = f.strategy().fail_prepare("broken");
    let source = MemorySource::new(vec![item("broken"), item("fine")]);
    let mut req = f.request(source);
    req.strategy = Some(strategy.clone());
    let report = run(req).await;
    assert_eq!(ids(&report), ["fine"]);
    assert!(f.claims.load_claim("broken").unwrap().is_none());
    let stops = f.claims.list_subject_stop_events("broken").unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].kind, STOP_PREPARE_SKIP);
    assert!(stops[0].detail.contains("cannot provision broken"));
    assert!(
        f.log
            .contains("broken: prepare failed (PrepareError: cannot provision broken); skipping")
    );
    assert_eq!(
        strategy
            .prepares()
            .iter()
            .filter(|(id, _)| id == "broken")
            .count(),
        1
    );
}

#[tokio::test]
async fn submit_runs_under_the_lease_and_teardown_always_runs() {
    let f = Fixture::new();
    let strategy = f.strategy();
    f.runner.script("fails", vec![failed("boom")]);
    let source = MemorySource::new(vec![item("ok"), item("fails")]);
    let mut req = f.request(source.clone());
    req.strategy = Some(strategy.clone());
    let report = run(req).await;
    assert_eq!(ids(&report), ["ok", "fails"]);
    let submits = strategy.submits();
    assert_eq!(submits.len(), 2);
    for seen in &submits {
        assert_eq!(
            seen.claim_held_by.as_deref(),
            Some("worker-a"),
            "submit ran under the lease"
        );
        assert_eq!(seen.sandbox, f.sandbox_root().join(&seen.task_id));
    }
    assert_eq!(submits[1].status, Status::Failed);
    assert_eq!(strategy.teardowns(), 2);
    assert!(f.claims.list_claims().unwrap().is_empty());
    // The handle's env contribution reached the runner, and the report
    // reached the source after submit.
    let calls = f.runner.calls();
    assert_eq!(calls[0].task_id, "ok");
    assert_eq!(calls[0].run_id, report.runs[0].run_id);
    assert_eq!(calls[0].sandbox, f.sandbox_root().join("ok"));
    assert_eq!(
        calls[0].agent_env.get("SANDBOX_KIND").map(String::as_str),
        Some("scripted")
    );
    assert_eq!(calls[0].worker_id, "worker-a");
    assert_eq!(calls[0].source_ref, "ref:ok");
    assert_eq!(calls[0].max_retries, 0);
    assert!(!calls[0].invoke_present);
    let reports = source.reports();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[1].status, Status::Failed);
    assert_eq!(reports[1].error, "boom");
}

#[tokio::test]
async fn teardown_runs_when_the_runner_fails_and_the_error_propagates() {
    let f = Fixture::new();
    let strategy = f.strategy();
    let runner: Arc<dyn RunTask> = Arc::new(|req: RunTaskRequest| -> RunTaskFuture {
        Box::pin(async move {
            finalize(&req, Status::Failed, "crash", Some(Outcome::InternalError));
            Err(RunTaskError::Other("entry crash".into()))
        })
    });
    let mut req = f.request(MemorySource::new(vec![item("crashy")]));
    req.run_task = runner;
    req.strategy = Some(strategy.clone());
    let err = orchestrate(req).await.unwrap_err();
    assert!(
        matches!(err, OrchestrateError::RunTask(RunTaskError::Other(_))),
        "{err}"
    );
    assert_eq!(strategy.teardowns(), 1);
    assert!(
        f.claims.load_claim("crashy").unwrap().is_none(),
        "lease released on the error path"
    );
}

#[tokio::test]
async fn report_failure_is_contained() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![item("a"), item("b")]);
    source.fail_report.store(true, Ordering::SeqCst);
    let report = run(f.request(source.clone())).await;
    assert_eq!(ids(&report), ["a", "b"]);
    assert_eq!(source.reports().len(), 2);
    assert!(f.log.contains(
        "a: work-source report failed (WorkSourceError: report endpoint down); continuing"
    ));
}

#[tokio::test]
async fn raising_source_ends_the_session_without_crashing() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![item("a")]);
    source.fail_list.store(true, Ordering::SeqCst);
    let report = run(f.request(source)).await;
    assert!(report.runs.is_empty());
    assert!(f.log.contains(
        "work-source listing failed (WorkSourceError: tracker unavailable); no work this pass"
    ));
}

#[tokio::test]
async fn statically_invalid_task_is_skipped_and_peer_runs() {
    let f = Fixture::new();
    let mut bad = item("bad");
    bad.task.graders = vec![Grader::Command(CommandGrader {
        run: String::new(),
        name: None,
    })];
    let mut req = f.request(MemorySource::new(vec![bad, item("good")]));
    req.repo_root = Some(f.dir.path().to_path_buf());
    let report = run(req).await;
    assert_eq!(ids(&report), ["good"]);
    assert!(f.log.contains("bad: invalid task definition ("));
    assert!(f.claims.load_claim("bad").unwrap().is_none());
}

// --- held-out gate ----------------------------------------------------------------

#[tokio::test]
async fn failing_held_out_gate_parks_and_suppresses_submit() {
    let f = Fixture::new();
    let strategy = f.strategy();
    let root = f.dir.path().join("held-out");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("gated.json"),
        r#"{"graders":[{"type":"command","run":"false","name":"oracle"}]}"#,
    )
    .unwrap();
    let mut req = f.request(MemorySource::new(vec![item("gated"), item("plain")]));
    req.strategy = Some(strategy.clone());
    req.held_out_source = Some(Arc::new(FilesystemHeldOutGraderSource::new(root)));
    let report = run(req).await;
    assert_eq!(ids(&report), ["gated", "plain"]);
    let gated = &report.runs[0];
    assert_eq!(gated.status, Status::Done);
    assert_eq!(gated.gate, Some(GateOutcome::Fail));
    assert!(gated.gate_reason.contains("held-out gate FAILED"));
    assert_eq!(report.runs[1].gate, Some(GateOutcome::NoGate));
    assert_eq!(
        strategy
            .submits()
            .iter()
            .map(|s| s.task_id.as_str())
            .collect::<Vec<_>>(),
        ["plain"],
        "the blocked land never reached submit"
    );
    let parks = landing_parks(f.control.as_ref(), &gated.run_id).unwrap();
    assert_eq!(parks.len(), 1);
    assert_eq!(parks[0].park_kind, PARK_KIND_HELD_OUT_GATE);
    assert!(f.log.contains("gated: held-out landing gate BLOCKED ("));
    assert_eq!(strategy.teardowns(), 2);
}

// --- stranded recovery, blocked resume, bare interrupt -----------------------------

#[tokio::test]
async fn stranded_lifecycle_is_recovered_then_task_runs() {
    let f = Fixture::new();
    f.seed("strand", "run-strand", Status::Running, "");
    let report = run(f.request(MemorySource::new(vec![item("strand")]))).await;
    assert_eq!(report.recovered, ["run-strand"]);
    assert_eq!(ids(&report), ["strand"]);
    // The finalized strand is a bare INTERRUPTED, resumed on its own run id.
    assert_eq!(report.runs[0].mode, RunMode::Resume);
    assert_eq!(report.runs[0].run_id, "run-strand");
    assert_eq!(f.lifecycle("run-strand").status, Status::Done);
}

#[tokio::test]
async fn peer_live_claim_protects_its_run_from_stranded_recovery() {
    let f = Fixture::new();
    f.seed("live", "run-live", Status::Running, "");
    f.claims
        .acquire_claim("live", "peer", t0(), 3600.0, &BTreeSet::new())
        .unwrap();
    let report = run(f.request(MemorySource::new(vec![item("live")]))).await;
    assert!(report.recovered.is_empty());
    assert!(report.runs.is_empty());
    assert_eq!(f.lifecycle("run-live").status, Status::Running);
}

#[tokio::test]
async fn blocked_then_unblocked_resumes_on_the_same_run_id() {
    let f = Fixture::new();
    let strategy = f.strategy();
    let sentinel = f.dir.path().join("unblock-me");
    std::fs::write(&sentinel, "ready").unwrap();
    f.runner.script(
        "gated",
        vec![
            Scripted::Blocked {
                requires_json: blocked_requires(&sentinel, true),
            },
            done(),
        ],
    );
    let mut req = f.request(MemorySource::new(vec![item("gated")]));
    req.strategy = Some(strategy.clone());
    let report = run(req).await;
    let modes: Vec<(RunMode, Status)> = report.runs.iter().map(|r| (r.mode, r.status)).collect();
    assert_eq!(
        modes,
        [
            (RunMode::Fresh, Status::Interrupted),
            (RunMode::Resume, Status::Done)
        ]
    );
    assert_eq!(report.runs[0].run_id, report.runs[1].run_id);
    assert_eq!(f.lifecycle(&report.runs[1].run_id).status, Status::Done);
    assert_eq!(
        strategy.prepares(),
        [
            ("gated".to_owned(), SandboxMode::Fresh),
            ("gated".to_owned(), SandboxMode::Resume)
        ]
    );
    assert_eq!(strategy.teardowns(), 2);
}

#[tokio::test]
async fn still_blocked_task_is_not_rerun_and_its_recheck_sandbox_is_torn_down() {
    let f = Fixture::new();
    let strategy = f.strategy();
    let missing = f.dir.path().join("never-created");
    f.runner.script(
        "stuck",
        vec![Scripted::Blocked {
            requires_json: blocked_requires(&missing, true),
        }],
    );
    let mut req = f.request(MemorySource::new(vec![item("stuck")]));
    req.strategy = Some(strategy.clone());
    let report = run(req).await;
    assert_eq!(report.runs.len(), 1);
    assert_eq!(report.runs[0].status, Status::Interrupted);
    // One run prepare, one recheck prepare; both torn down; claim released.
    assert_eq!(strategy.prepares().len(), 2);
    assert_eq!(strategy.teardowns(), 2);
    assert!(f.claims.load_claim("stuck").unwrap().is_none());
}

#[tokio::test]
async fn bare_interrupted_task_resumes_on_its_own_run_id() {
    let f = Fixture::new();
    f.seed("paused", "run-paused", Status::Interrupted, "");
    let report = run(f.request(MemorySource::new(vec![item("paused")]))).await;
    assert_eq!(report.runs.len(), 1);
    assert_eq!(report.runs[0].mode, RunMode::Resume);
    assert_eq!(report.runs[0].run_id, "run-paused");
    assert_eq!(f.runner.calls()[0].mode, RunMode::Resume);
    assert_eq!(f.lifecycle("run-paused").status, Status::Done);
}

#[tokio::test]
async fn perpetually_failing_task_runs_once_and_stops() {
    let f = Fixture::new();
    f.runner.script("flaky", vec![failed("nope")]);
    let report = run(f.request(MemorySource::new(vec![item("flaky")]))).await;
    assert_eq!(report.runs.len(), 1);
    assert_eq!(report.runs[0].status, Status::Failed);
}

// --- approval resolve -----------------------------------------------------------

#[tokio::test]
async fn pending_approve_resolves_in_place_and_lands_through_the_strategy() {
    let f = Fixture::new();
    let strategy = f.strategy().park_sequence("gated", vec![None]);
    let mut gated = item("gated");
    gated.task = gated_task("gated");
    f.runner
        .script("gated", vec![Scripted::Await { ordinal: 1 }]);
    let mut req = f.request(MemorySource::new(vec![gated]));
    req.strategy = Some(strategy.clone());
    let first = run(req.clone()).await;
    assert_eq!(first.runs.len(), 1);
    assert_eq!(first.runs[0].status, Status::AwaitingApproval);
    let run_id = first.runs[0].run_id.clone();
    // A parked gate is surfaced to the human-review queue once, untouched.
    let queue = f.claims.list_human_review_queue().unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].reason, "awaiting-approval");
    // Submit for the non-landing park status only.
    assert_eq!(strategy.submits().len(), 1);
    assert_eq!(strategy.submits()[0].status, Status::AwaitingApproval);

    f.control
        .enqueue_command(&run_id, "approve", serde_json::Map::new(), t0())
        .unwrap();
    let second = run(req.clone()).await;
    assert!(
        second.runs.is_empty(),
        "resolution is in place, not a fresh drive"
    );
    let final_lc = f.lifecycle(&run_id);
    assert_eq!(final_lc.status, Status::Done);
    assert_eq!(final_lc.awaiting_manual_ordinal, None);
    let landed = strategy.submits();
    assert_eq!(landed.len(), 2);
    assert_eq!(landed[1].status, Status::Done);
    assert_eq!(landed[1].claim_held_by.as_deref(), Some("worker-a"));
    assert!(f.claims.list_claims().unwrap().is_empty());
    assert_eq!(
        f.claims.list_human_review_queue().unwrap().len(),
        1,
        "not re-queued"
    );
}

#[tokio::test]
async fn awaiting_approval_without_a_command_stays_parked() {
    let f = Fixture::new();
    let mut gated = item("gated");
    gated.task = gated_task("gated");
    f.runner
        .script("gated", vec![Scripted::Await { ordinal: 1 }]);
    let req = f.request(MemorySource::new(vec![gated]));
    let first = run(req.clone()).await;
    let run_id = first.runs[0].run_id.clone();
    let second = run(req).await;
    assert!(second.runs.is_empty());
    assert!(
        second.recovered.is_empty(),
        "AWAITING_APPROVAL is exempt from stranded recovery"
    );
    let lc = f.lifecycle(&run_id);
    assert_eq!(lc.status, Status::AwaitingApproval);
    assert_eq!(lc.awaiting_manual_ordinal, Some(1));
}

#[tokio::test]
async fn empty_approval_mark_expires_and_is_reswept_in_the_same_session() {
    let f = Fixture::new();
    let clock_state = Arc::new(Mutex::new(t0()));
    let now: Clock = {
        let state = clock_state.clone();
        Arc::new(move || *state.lock().unwrap())
    };
    let mut gated = item("gated").with_priority(1);
    gated.task = gated_task("gated");
    let keepalive = item("keepalive").with_priority(0);
    f.runner
        .script("gated", vec![Scripted::Await { ordinal: 1 }]);
    let control = f.control.clone();
    let state = clock_state.clone();
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let parked = control
            .list_lifecycles(
                &LifecycleFilter::default()
                    .with_statuses([Status::AwaitingApproval])
                    .with_task_id("gated"),
            )
            .unwrap();
        let now = *state.lock().unwrap();
        control
            .enqueue_command(&parked[0].run_id, "approve", serde_json::Map::new(), now)
            .unwrap();
        *state.lock().unwrap() =
            now + ChronoDuration::seconds(APPROVAL_SWEEP_MARK_TTL_SECONDS as i64 + 5);
    });
    f.runner
        .script("keepalive", vec![Scripted::Hook { hook, sleep_ms: 0 }]);
    let mut req = f.request(MemorySource::new(vec![gated, keepalive]));
    req.now = Some(now);
    let report = run(req).await;
    let gated_runs: Vec<&RunRecord> = report
        .runs
        .iter()
        .filter(|r| r.task_id == "gated")
        .collect();
    assert_eq!(gated_runs.len(), 1);
    assert_eq!(gated_runs[0].status, Status::AwaitingApproval);
    let lc = f.lifecycle(&gated_runs[0].run_id);
    assert_eq!(
        lc.status,
        Status::Done,
        "the mid-session approve was consumed"
    );
    assert_eq!(lc.awaiting_manual_ordinal, None);
}

// --- landing re-drive --------------------------------------------------------------

#[tokio::test]
async fn landing_redrive_is_bounded_and_routes_to_the_queue_once() {
    let f = Fixture::new();
    // The first land parks; the re-drives park again; never lands.
    let strategy = f
        .strategy()
        .park_sequence("parked", vec![Some(PARK_KIND_DIVERGENT_BASE)]);
    let mut req = f.request(MemorySource::new(vec![item("parked")]));
    req.strategy = Some(strategy.clone());
    req.landing_redrive_bound = Some(2);
    let report = run(req.clone()).await;
    let run_id = report.runs[0].run_id.clone();
    // 1 original park + 2 re-attempts.
    assert_eq!(landing_parks(f.control.as_ref(), &run_id).unwrap().len(), 3);
    let queue = f.claims.list_human_review_queue().unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].reason, PARK_KIND_DIVERGENT_BASE);
    assert_eq!(queue[0].run_id, run_id);
    assert!(queue[0].detail.contains("exhausted 2 re-attempt(s)"));
    // Submits: first land + 2 re-drives, every one under the lease.
    let submits = strategy.submits();
    assert_eq!(submits.len(), 3);
    assert!(
        submits
            .iter()
            .all(|s| s.claim_held_by.as_deref() == Some("worker-a"))
    );
    assert!(
        f.log.contains(
            "parked: parked landing routed to the human-review queue after 2 re-attempt(s)"
        )
    );
    // Paired audit records: two re-parked, one routed.
    let redriven: Vec<String> = f
        .control
        .list_domain_events(&run_id)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            DomainEventPayload::LandingRedriven(r) => Some(r.result),
            _ => None,
        })
        .collect();
    assert_eq!(redriven, ["re-parked", "re-parked", "routed"]);
    // A later session makes no further attempt and does not re-queue.
    run(req).await;
    assert_eq!(strategy.submits().len(), 3);
    assert_eq!(f.claims.list_human_review_queue().unwrap().len(), 1);
}

#[tokio::test]
async fn landing_redrive_lands_when_the_cause_clears_and_skips_unparked_runs() {
    let f = Fixture::new();
    let strategy = f
        .strategy()
        .park_sequence("parked", vec![Some(PARK_KIND_DIVERGENT_BASE), None]);
    let mut req = f.request(MemorySource::new(vec![item("parked"), item("clean")]));
    req.strategy = Some(strategy.clone());
    req.landing_redrive_bound = Some(3);
    let report = run(req).await;
    let parked_run = report
        .runs
        .iter()
        .find(|r| r.task_id == "parked")
        .unwrap()
        .run_id
        .clone();
    let events = f.control.list_domain_events(&parked_run).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.payload, DomainEventPayload::Landed(_)))
    );
    let redriven: Vec<String> = events
        .into_iter()
        .filter_map(|e| match e.payload {
            DomainEventPayload::LandingRedriven(r) => Some(r.result),
            _ => None,
        })
        .collect();
    assert_eq!(redriven, ["landed"]);
    assert!(f.claims.list_human_review_queue().unwrap().is_empty());
    assert!(
        f.log
            .contains("parked: re-drove parked landing to a merge after 1 re-attempt(s)")
    );
    // "clean" never parked: exactly one submit, no redrive record.
    assert_eq!(
        strategy
            .submits()
            .iter()
            .filter(|s| s.task_id == "clean")
            .count(),
        1
    );
}

#[tokio::test]
async fn landing_redrive_terminates_when_reparks_are_never_recorded() {
    let f = Fixture::new();
    // The first land records its park; every re-attempt then parks without
    // writing one, so the ledger's park count never advances.
    let strategy = f
        .strategy()
        .park_sequence("parked", vec![Some(PARK_KIND_DIVERGENT_BASE)])
        .unrecorded_reparks("parked");
    let mut req = f.request(MemorySource::new(vec![item("parked")]));
    req.strategy = Some(strategy.clone());
    req.landing_redrive_bound = Some(2);
    let report = tokio::time::timeout(Duration::from_secs(30), run(req.clone()))
        .await
        .expect("the re-drive loop must terminate within its bound");
    let run_id = report.runs[0].run_id.clone();
    assert_eq!(landing_parks(f.control.as_ref(), &run_id).unwrap().len(), 1);
    // First land + exactly `bound` re-attempts, never more.
    assert_eq!(strategy.submits().len(), 3);
    let events = f.control.list_domain_events(&run_id).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.payload, DomainEventPayload::Landed(_))),
        "nothing landed"
    );
    let redriven: Vec<String> = events
        .into_iter()
        .filter_map(|e| match e.payload {
            DomainEventPayload::LandingRedriven(r) => Some(r.result),
            _ => None,
        })
        .collect();
    assert_eq!(redriven, ["re-parked", "re-parked", "routed"]);
    let queue = f.claims.list_human_review_queue().unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].reason, PARK_KIND_DIVERGENT_BASE);
    assert!(
        queue[0]
            .detail
            .contains("2 re-attempt(s) parked without a recorded landing-parked event")
    );
    assert!(f.log.contains("no landing-parked event was recorded"));
    assert!(!f.log.contains("re-drove parked landing to a merge"));
    // A later session sees the queue entry and makes no further attempt.
    run(req).await;
    assert_eq!(strategy.submits().len(), 3);
}

#[tokio::test]
async fn landing_redrive_reports_parked_not_landed_for_an_unrecorded_repark() {
    let f = Fixture::new();
    let strategy = f
        .strategy()
        .park_sequence("parked", vec![Some(PARK_KIND_DIVERGENT_BASE)])
        .unrecorded_reparks("parked");
    let mut req = f.request(MemorySource::new(vec![item("parked")]));
    req.strategy = Some(strategy.clone());
    req.landing_redrive_bound = Some(0);
    let report = run(req).await;
    let run_id = report.runs[0].run_id.clone();
    let control: Arc<dyn HarnessStore> = f.control.clone();
    let claims: Arc<dyn ClaimStore> = f.claims.clone();
    let clock = fixed(t0());
    let gate_options = GateOptions::default();
    let ctx = LandingRedriveContext {
        control: &control,
        claims: &claims,
        strategy: strategy.as_ref(),
        worker_id: "worker-a",
        bound: 2,
        held_out_source: None,
        gate_options: &gate_options,
        lease_seconds: 60.0,
        heartbeat_interval: 20.0,
        steal_grace_seconds: 0.0,
        now: &clock,
        log: None,
    };
    let request = SubmitRequest::new(
        task("parked"),
        run_id.clone(),
        Status::Done,
        f.sandbox_root().join("parked"),
    );
    let outcomes = tokio::time::timeout(
        Duration::from_secs(30),
        redrive_parked_landings(&ctx, &[request], &mut BTreeSet::new()),
    )
    .await
    .expect("the re-drive loop must terminate within its bound")
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].result, RedriveResult::Queued);
    assert_eq!(outcomes[0].attempts, 2);
    assert_eq!(outcomes[0].park_kind, PARK_KIND_DIVERGENT_BASE);
}

#[tokio::test]
async fn landing_redrive_skips_a_run_the_probe_reports_not_landable() {
    let f = Fixture::new();
    let strategy = f
        .strategy()
        .park_sequence("parked", vec![Some(PARK_KIND_DIVERGENT_BASE)])
        .set_landable("parked", false);
    let mut req = f.request(MemorySource::new(vec![item("parked")]));
    req.strategy = Some(strategy.clone());
    req.landing_redrive_bound = Some(3);
    // Not landable at the harness gate: the run is re-driven to FAILED by
    // the harness in production; the scripted runner still reports DONE, so
    // the first submit parks and the re-driver's probe then skips it.
    let report = run(req).await;
    let run_id = report.runs[0].run_id.clone();
    assert_eq!(landing_parks(f.control.as_ref(), &run_id).unwrap().len(), 1);
    assert_eq!(
        strategy.submits().len(),
        1,
        "no re-attempt for a not-landable run"
    );
    assert!(f.claims.list_human_review_queue().unwrap().is_empty());
}

// --- session pause ------------------------------------------------------------------

#[tokio::test]
async fn session_pause_blocks_peers_pool_wide_until_the_clamped_horizon() {
    let f = Fixture::new();
    let far_reset = t0() + ChronoDuration::hours(5);
    f.runner.script(
        "limit",
        vec![Scripted::SessionLimit {
            reset_at: far_reset,
        }],
    );
    let ceiling = 100.0;
    let mut req_a = f.request(MemorySource::new(vec![item("limit")]));
    req_a.session_pause_ceiling_seconds = ceiling;
    let a = run(req_a).await;
    assert_eq!(ids(&a), ["limit"]);
    assert_eq!(a.runs[0].status, Status::Failed);
    assert!(f.log.contains("limit: session limit reached (reset"));
    assert!(f.log.contains("pausing pool-wide claiming until"));
    let pauses = f.claims.list_session_pauses().unwrap();
    assert_eq!(pauses.len(), 1);
    assert_eq!(pauses[0].run_id, a.runs[0].run_id);
    assert_eq!(pauses[0].reset_at, far_reset);
    assert_eq!(pauses[0].pause_until, t0() + ChronoDuration::seconds(100));

    let work = MemorySource::new(vec![item("work")]);
    let mut req_b = f.request(work.clone());
    req_b.worker_id = Some("worker-b".into());
    req_b.now = Some(fixed(t0() + ChronoDuration::seconds(10)));
    req_b.session_pause_ceiling_seconds = ceiling;
    let b = run(req_b).await;
    assert!(b.runs.is_empty(), "declined every fresh claim while paused");
    assert!(f.log.contains("session-limit pause active until"));

    let mut req_c = f.request(work);
    req_c.worker_id = Some("worker-c".into());
    req_c.now = Some(fixed(t0() + ChronoDuration::seconds(200)));
    req_c.session_pause_ceiling_seconds = ceiling;
    let c = run(req_c).await;
    assert_eq!(ids(&c), ["work"]);
    assert_eq!(c.runs[0].status, Status::Done);
}

#[tokio::test]
async fn session_pause_ceiling_zero_never_pauses() {
    let f = Fixture::new();
    f.runner.script(
        "limit",
        vec![Scripted::SessionLimit {
            reset_at: t0() + ChronoDuration::hours(5),
        }],
    );
    let a = run(f.request(MemorySource::new(vec![item("limit")]))).await;
    assert_eq!(ids(&a), ["limit"]);
    assert!(f.claims.list_session_pauses().unwrap().is_empty());
    assert!(f.claims.session_pause_until(t0()).unwrap().is_none());
    let mut req_b = f.request(MemorySource::new(vec![item("work")]));
    req_b.now = Some(fixed(t0() + ChronoDuration::seconds(10)));
    let b = run(req_b).await;
    assert_eq!(ids(&b), ["work"]);
}

// --- shutdown, cancellation, claim loss, in-band interrupt ------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_signal_releases_claims_tears_down_and_returns_the_partial_report() {
    let f = Fixture::new();
    let strategy = f.strategy();
    let started = Flag::default();
    f.runner.script(
        "long",
        vec![Scripted::WaitShutdown {
            started: started.clone(),
        }],
    );
    let shutdown = ShutdownSignal::new();
    let mut req = f.request(MemorySource::new(vec![
        item("first"),
        item("long"),
        item("never"),
    ]));
    req.strategy = Some(strategy.clone());
    req.shutdown = Some(shutdown.clone());
    let handle = tokio::spawn(orchestrate(req));
    assert!(started.wait_async(Duration::from_secs(5)).await);
    assert!(
        f.claims.load_claim("long").unwrap().is_some(),
        "lease held while in flight"
    );
    shutdown.trigger();
    let err = handle.await.unwrap().unwrap_err();
    let OrchestrateError::Interrupted { report } = err else {
        panic!("expected Interrupted, got {err}");
    };
    assert_eq!(
        ids(&report),
        ["first"],
        "the interrupted run is not recorded; later tasks never start"
    );
    assert!(
        f.claims.list_claims().unwrap().is_empty(),
        "every claim released"
    );
    assert_eq!(strategy.teardowns(), 2, "first + long torn down");
    let long = f
        .control
        .list_lifecycles(&LifecycleFilter::default().with_task_id("long"))
        .unwrap();
    assert_eq!(long[0].status, Status::Interrupted);
    assert!(
        f.control
            .list_lifecycles(&LifecycleFilter::default().with_task_id("never"))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn dropping_the_orchestrate_future_releases_the_lease_and_tears_down() {
    let f = Fixture::new();
    let strategy = f.strategy();
    let started = Flag::default();
    f.runner.script(
        "hang",
        vec![Scripted::Hang {
            started: started.clone(),
        }],
    );
    let mut req = f.request(MemorySource::new(vec![item("hang")]));
    req.strategy = Some(strategy.clone());
    let fut = orchestrate(req);
    let result = tokio::time::timeout(Duration::from_millis(300), fut).await;
    assert!(result.is_err(), "the future was cancelled by the timeout");
    assert!(started.is_set());
    assert!(
        f.claims.load_claim("hang").unwrap().is_none(),
        "lease released by the guard"
    );
    assert_eq!(strategy.teardowns(), 1);
}

#[tokio::test]
async fn claim_lost_mid_run_relinquishes_without_landing_or_killing_the_worker() {
    let f = Fixture::new();
    let strategy = f.strategy();
    let claims = f.claims.clone();
    let thief: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        // A peer steals the lease (its clock is far ahead); the next renew
        // observes the loss.
        claims
            .acquire_claim(
                "solo",
                "thief",
                Utc::now() + ChronoDuration::hours(1),
                3600.0,
                &BTreeSet::new(),
            )
            .unwrap()
            .expect("stolen");
    });
    f.runner.script(
        "solo",
        vec![Scripted::Hook {
            hook: thief,
            sleep_ms: 80,
        }],
    );
    let source = MemorySource::new(vec![item("solo"), item("peer-work")]);
    let mut req = f.request(source.clone());
    req.strategy = Some(strategy.clone());
    req.lease_seconds = 0.03;
    req.now = Some(Arc::new(Utc::now));
    let report = run(req).await;
    assert_eq!(
        ids(&report),
        ["peer-work"],
        "the relinquished run is not recorded; the worker keeps draining"
    );
    assert!(
        f.log
            .contains("solo: claim lost mid-run (ClaimLostError); a peer took over, re-evaluating")
    );
    assert!(
        strategy.submits().iter().all(|s| s.task_id != "solo"),
        "never landed"
    );
    assert!(
        source.reports().iter().all(|r| r.task_id != "solo"),
        "never reported"
    );
    assert_eq!(
        f.claims.load_claim("solo").unwrap().unwrap().worker_id,
        "thief",
        "stale release is a no-op"
    );
    assert_eq!(strategy.teardowns(), 2);
}

#[tokio::test]
async fn optimistic_concurrency_loss_is_relinquished() {
    let f = Fixture::new();
    let runner: Arc<dyn RunTask> = Arc::new(|req: RunTaskRequest| -> RunTaskFuture {
        Box::pin(async move {
            if req.task.id == "raced" {
                return Err(RunTaskError::Store(StoreError::optimistic_concurrency(
                    req.run_id(),
                    1,
                    2,
                )));
            }
            Ok(outcome_of(
                finalize(&req, Status::Done, "", Some(Outcome::Succeeded)),
                None,
            ))
        })
    });
    let mut req = f.request(MemorySource::new(vec![item("raced"), item("other")]));
    req.run_task = runner;
    let report = run(req).await;
    assert_eq!(ids(&report), ["other"]);
    assert!(
        f.log
            .contains("raced: claim lost mid-run (OptimisticConcurrencyError)")
    );
}

#[tokio::test]
async fn in_band_interrupt_is_contained_to_one_task() {
    let f = Fixture::new();
    f.runner.script("doomed", vec![Scripted::InBandInterrupt]);
    let report = run(f.request(MemorySource::new(vec![item("doomed"), item("survivor")]))).await;
    let by_task: HashMap<&str, &RunRecord> = report
        .runs
        .iter()
        .map(|r| (r.task_id.as_str(), r))
        .collect();
    assert_eq!(by_task["doomed"].status, Status::Interrupted);
    assert_eq!(by_task["doomed"].mode, RunMode::Fresh);
    assert_eq!(by_task["survivor"].status, Status::Done);
    assert!(f.log.contains("doomed: run "));
    assert!(
        f.log
            .contains("interrupted in-band (control command); continuing with remaining tasks")
    );
    assert!(f.claims.list_claims().unwrap().is_empty());
}

#[tokio::test]
async fn lease_stolen_between_heartbeat_ticks_is_never_landed_or_reported() {
    // The heartbeat interval (lease / 3 = 20 minutes) never elapses during
    // the run, so its `lost` flag cannot observe the steal: only a
    // synchronous ownership check before landing can.
    let f = Fixture::new();
    let strategy = f.strategy();
    let claims = f.claims.clone();
    let thief: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        claims
            .acquire_claim(
                "solo",
                "thief",
                Utc::now() + ChronoDuration::hours(2),
                3600.0,
                &BTreeSet::new(),
            )
            .unwrap()
            .expect("stolen");
    });
    f.runner.script(
        "solo",
        vec![Scripted::Hook {
            hook: thief,
            sleep_ms: 0,
        }],
    );
    let source = MemorySource::new(vec![item("solo"), item("peer-work")]);
    let mut req = f.request(source.clone());
    req.strategy = Some(strategy.clone());
    req.lease_seconds = 3600.0;
    req.now = Some(Arc::new(Utc::now));
    let report = run(req).await;
    assert!(
        strategy.submits().iter().all(|s| s.task_id != "solo"),
        "a run whose lease a peer now owns must never land"
    );
    assert!(
        source.reports().iter().all(|r| r.task_id != "solo"),
        "never reported"
    );
    assert_eq!(ids(&report), ["peer-work"]);
    assert!(
        f.log
            .contains("solo: claim lost mid-run (ClaimLostError); a peer took over, re-evaluating")
    );
    assert_eq!(
        f.claims.load_claim("solo").unwrap().unwrap().worker_id,
        "thief",
        "stale release is a no-op"
    );
}

#[test]
fn heartbeat_confirm_renews_a_held_lease_and_reports_a_stolen_one() {
    let store: Arc<dyn ClaimStore> = Arc::new(InMemoryClaimStore::new());
    // Whole seconds: the store keeps microsecond precision.
    let start = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
    let claim = store
        .acquire_claim("t", "w", start, 10.0, &BTreeSet::new())
        .unwrap()
        .unwrap();
    let heartbeat = ClaimHeartbeat::start(
        store.clone(),
        claim.clone(),
        10.0,
        3600.0,
        Arc::new(move || start + ChronoDuration::seconds(5)),
    )
    .unwrap();
    assert!(heartbeat.confirm().unwrap(), "held lease confirms");
    let renewed = store.load_claim("t").unwrap().unwrap();
    assert_eq!(renewed.version, claim.version + 1);
    assert_eq!(
        renewed.lease_expires_at,
        start + ChronoDuration::seconds(15),
        "confirm extends the lease"
    );
    assert_eq!(heartbeat.latest(), renewed);

    store
        .acquire_claim(
            "t",
            "w2",
            start + ChronoDuration::seconds(60),
            10.0,
            &BTreeSet::new(),
        )
        .unwrap()
        .expect("stolen after lapse");
    assert!(
        !heartbeat.confirm().unwrap(),
        "stolen lease does not confirm"
    );
    assert!(heartbeat.lost());
    heartbeat.stop();
}

#[tokio::test]
async fn heartbeat_renews_the_lease_and_signals_a_stolen_lease() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ClaimStore> =
        Arc::new(SqliteClaimStore::open(dir.path().join("c.sqlite")).unwrap());
    let start = Utc::now();
    let claim = store
        .acquire_claim("t", "w", start, 10.0, &BTreeSet::new())
        .unwrap()
        .unwrap();
    let heartbeat =
        ClaimHeartbeat::start(store.clone(), claim.clone(), 10.0, 0.02, Arc::new(Utc::now))
            .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    assert!(!heartbeat.lost());
    let latest = heartbeat.stop();
    assert!(latest.version > claim.version);
    assert!(store.load_claim("t").unwrap().unwrap().version >= 2);

    // A peer steals the lapsed lease: renewal against the stale token must
    // flag the loss.
    let claim = store.load_claim("t").unwrap().unwrap();
    let stolen = store
        .acquire_claim(
            "t",
            "w2",
            start + ChronoDuration::seconds(60),
            10.0,
            &BTreeSet::new(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(stolen.worker_id, "w2");
    let heartbeat = ClaimHeartbeat::start(
        store.clone(),
        claim,
        10.0,
        0.02,
        Arc::new(move || start + ChronoDuration::seconds(61)),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !heartbeat.lost() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(heartbeat.lost());
    heartbeat.stop();
}

// --- steering reconciler and lease sweep -----------------------------------------

#[test]
fn reconcile_live_runs_interrupts_vanished_runs_once_and_leaves_parked_gates() {
    let f = Fixture::new();
    f.seed("gone", "run-gone", Status::Running, "");
    f.seed("listed", "run-listed", Status::Running, "");
    f.seed("parked", "run-parked", Status::AwaitingApproval, "");
    let wanted = BTreeSet::from(["listed".to_owned()]);
    let mut signaled = HashSet::new();
    let newly = reconcile_live_runs(f.control.as_ref(), &wanted, &mut signaled, t0()).unwrap();
    assert_eq!(newly, ["run-gone"]);
    let again = reconcile_live_runs(f.control.as_ref(), &wanted, &mut signaled, t0()).unwrap();
    assert!(again.is_empty(), "signaled at most once per session");
    let cmds = f.control.claim_commands("run-gone", t0()).unwrap();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].kind, "interrupt");
    assert_eq!(cmds[0].payload["origin"], "source-reconciler");
    assert!(
        f.control
            .claim_commands("run-listed", t0())
            .unwrap()
            .is_empty()
    );
    assert!(
        f.control
            .claim_commands("run-parked", t0())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciler_interrupts_a_vanished_item_end_to_end() {
    let f = Fixture::new();
    f.runner.script("vanishing", vec![Scripted::PollInterrupt]);
    let source = MemorySource::new(vec![item("vanishing"), item("survivor")]);
    let src = source.clone();
    *source.on_list.lock().unwrap() = Some(Box::new(move |call| {
        if call == 1 {
            // Pulled from the source the moment the loop first lists it;
            // the reconciler's later listings no longer see it.
            let items = src.items.lock().unwrap().clone();
            if items.iter().any(|i| i.task.id == "vanishing") {
                // Keep it for this first listing; drop it afterwards.
            }
        }
    }));
    let mut req = f.request(source.clone());
    req.reconcile_seconds = Some(0.01);
    req.now = Some(Arc::new(Utc::now));
    let remover = source.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        remover.remove("vanishing");
    });
    let report = run(req).await;
    handle.await.unwrap();
    let by_task: HashMap<&str, &RunRecord> = report
        .runs
        .iter()
        .map(|r| (r.task_id.as_str(), r))
        .collect();
    assert_eq!(by_task["vanishing"].status, Status::Interrupted);
    assert_eq!(by_task["survivor"].status, Status::Done);
    assert!(f.log.contains("steering: run "));
    assert!(
        f.log
            .contains("'s item vanished from the work source; interrupt enqueued")
    );
}

#[test]
fn sweep_finalizes_claimable_strands_and_reaps_orphans_but_never_a_live_claim() {
    let f = Fixture::new();
    let now = t0() + ChronoDuration::seconds(1000);
    let clock = fixed(now);
    // A dead worker's strand with a lapsed lease.
    f.seed("dead", "run-dead", Status::Running, "");
    f.claims
        .acquire_claim("dead", "w-dead", t0(), 10.0, &BTreeSet::new())
        .unwrap();
    // An orphan lapsed lease with no lifecycle behind it.
    f.claims
        .acquire_claim("orphan", "w-dead", t0(), 10.0, &BTreeSet::new())
        .unwrap();
    // A live peer's run.
    f.seed("live", "run-live", Status::Validating, "");
    f.claims
        .acquire_claim("live", "w-live", now, 3600.0, &BTreeSet::new())
        .unwrap();
    let before = f.lifecycle("run-live");
    let control: Arc<dyn HarnessStore> = f.control.clone();
    let (recovered, released) = sweep_expired_leases(
        &control,
        f.claims.as_ref(),
        "sweeper",
        60.0,
        0.0,
        &clock,
        None,
    )
    .unwrap();
    assert_eq!(recovered, ["run-dead"]);
    assert_eq!(released, ["orphan"]);
    assert_eq!(f.lifecycle("run-dead").status, Status::Interrupted);
    assert_eq!(f.lifecycle("run-live"), before);
    assert_eq!(
        f.claims.load_claim("live").unwrap().unwrap().worker_id,
        "w-live"
    );
    assert!(f.claims.load_claim("dead").unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_loop_lease_sweep_finalizes_a_strand_mid_drive() {
    let f = Fixture::new();
    let control = f.control.clone();
    let claims = f.claims.clone();
    let strand: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        // A peer dies mid-run while this worker drives "long": its lifecycle
        // is stranded RUNNING behind a lease that lapsed long ago.
        let mut lc = Lifecycle::new("strand").with_run_id("run-strand");
        walk(
            &mut lc,
            &[(Status::Ready, ""), (Status::Running, "")],
            Utc::now(),
        );
        control.create_lifecycle(&lc).unwrap();
        claims
            .acquire_claim(
                "strand",
                "w-dead",
                Utc::now() - ChronoDuration::hours(2),
                10.0,
                &BTreeSet::new(),
            )
            .unwrap();
    });
    f.runner.script(
        "long",
        vec![Scripted::Hook {
            hook: strand,
            sleep_ms: 150,
        }],
    );
    let source = MemorySource::new(vec![item("long"), item("strand")]);
    let mut req = f.request(source);
    req.sweep_seconds = Some(0.01);
    req.now = Some(Arc::new(Utc::now));
    let report = run(req).await;
    assert!(
        f.log
            .contains("lease-sweep: finalized stranded run run-strand; its task is eligible again")
    );
    let strand_run = report
        .runs
        .iter()
        .find(|r| r.task_id == "strand")
        .expect("strand driven");
    assert_eq!(strand_run.mode, RunMode::Resume);
    assert_eq!(strand_run.run_id, "run-strand");
    assert_eq!(f.lifecycle("run-strand").status, Status::Done);
}

#[test]
fn sync_work_source_marks_nothing_disappeared_on_a_failed_listing() {
    let f = Fixture::new();
    let source = MemorySource::new(vec![item("a").with_prerequisites(["b"]), item("b")]);
    let ok = sync_work_source(source.as_ref(), f.claims.as_ref(), t0()).unwrap();
    assert_eq!(ok.status, "ok");
    assert_eq!(ok.observed_count, 2);
    assert_eq!(f.claims.load_work_item_dependencies("a").unwrap(), ["b"]);
    source.remove("b");
    source.fail_list.store(true, Ordering::SeqCst);
    let failed = sync_work_source(
        source.as_ref(),
        f.claims.as_ref(),
        t0() + ChronoDuration::seconds(1),
    )
    .unwrap();
    assert_eq!(failed.status, "error");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("tracker unavailable")
    );
    assert!(
        f.claims
            .load_work_item("b")
            .unwrap()
            .unwrap()
            .disappeared_at
            .is_none()
    );
    source.fail_list.store(false, Ordering::SeqCst);
    sync_work_source(
        source.as_ref(),
        f.claims.as_ref(),
        t0() + ChronoDuration::seconds(2),
    )
    .unwrap();
    assert!(
        f.claims
            .load_work_item("b")
            .unwrap()
            .unwrap()
            .disappeared_at
            .is_some()
    );
}

// --- re-drivers (unit) --------------------------------------------------------------

#[test]
fn no_progress_redriver_is_bounded_resets_on_progress_and_queues_once() {
    let f = Fixture::new();
    let clock = fixed(t0());
    let log = f.log.sink();
    let idle = |unit: &str| NoProgressObservation::new(unit, false);
    let busy = |unit: &str| NoProgressObservation::new(unit, true);
    let out = redrive_no_progress(
        f.claims.as_ref(),
        &[idle("phase-1"), idle("phase-2")],
        3,
        &clock,
        Some(&log),
    )
    .unwrap();
    assert!(
        out.iter()
            .all(|o| o.result == NoProgressResult::Waiting && o.cycles == 1)
    );
    // Progress resets phase-2's streak with a delimiter; phase-1 keeps counting.
    let out = redrive_no_progress(
        f.claims.as_ref(),
        &[idle("phase-1"), busy("phase-2")],
        3,
        &clock,
        None,
    )
    .unwrap();
    assert_eq!(out[0].cycles, 2);
    assert_eq!(
        (out[1].result, out[1].cycles),
        (NoProgressResult::Progressed, 0)
    );
    assert_eq!(no_progress_streak(f.claims.as_ref(), "phase-2").unwrap(), 0);
    // A perpetually progressing unit never grows the ledger.
    redrive_no_progress(f.claims.as_ref(), &[busy("phase-2")], 3, &clock, None).unwrap();
    assert_eq!(
        f.claims.list_subject_stop_events("phase-2").unwrap().len(),
        2
    );
    // phase-1 hits the bound: backed off and queued exactly once.
    let out =
        redrive_no_progress(f.claims.as_ref(), &[idle("phase-1")], 3, &clock, Some(&log)).unwrap();
    assert_eq!(
        (out[0].result, out[0].cycles),
        (NoProgressResult::Queued, 3)
    );
    assert!(
        f.log
            .contains("phase-1: no progress for 3 consecutive cycle(s); backed off")
    );
    for _ in 0..3 {
        let out =
            redrive_no_progress(f.claims.as_ref(), &[idle("phase-1")], 3, &clock, None).unwrap();
        assert_eq!(out[0].result, NoProgressResult::Queued);
    }
    let queue = f.claims.list_human_review_queue().unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(
        (queue[0].reason.as_str(), queue[0].task_id.as_str()),
        ("no-progress", "phase-1")
    );
    assert_eq!(
        no_progress_streak(f.claims.as_ref(), "phase-1").unwrap(),
        3,
        "no witness past the bound"
    );
    assert_eq!(DEFAULT_NO_PROGRESS_BOUND, 3);
}

#[test]
fn missing_prerequisite_redriver_bounds_each_edge_independently() {
    let f = Fixture::new();
    let clock = fixed(t0());
    let issue = |m: &str| GraphValidationIssue {
        referencing_id: "dep".into(),
        missing_id: m.into(),
    };
    for _ in 0..2 {
        let out = redrive_missing_prerequisites(
            f.claims.as_ref(),
            &[issue("x"), issue("y")],
            3,
            &clock,
            None,
        )
        .unwrap();
        assert!(out.iter().all(|o| o.result == PrereqRedriveResult::Waiting));
    }
    let out =
        redrive_missing_prerequisites(f.claims.as_ref(), &[issue("x")], 3, &clock, None).unwrap();
    assert_eq!(
        (out[0].result, out[0].cycles),
        (PrereqRedriveResult::Queued, 3)
    );
    assert_eq!(f.claims.list_human_review_queue().unwrap().len(), 1);
    // "x" is terminal (no further witness, no re-queue); "y" reaches its own
    // third witness on this pass and is routed independently.
    let out = redrive_missing_prerequisites(
        f.claims.as_ref(),
        &[issue("x"), issue("y")],
        3,
        &clock,
        None,
    )
    .unwrap();
    assert_eq!(
        (out[0].result, out[0].cycles),
        (PrereqRedriveResult::Queued, 3)
    );
    assert_eq!(
        (out[1].result, out[1].cycles),
        (PrereqRedriveResult::Queued, 3)
    );
    let queue = f.claims.list_human_review_queue().unwrap();
    assert_eq!(queue.len(), 2);
    assert!(queue[0].detail.contains("\"x\""));
    assert!(queue[1].detail.contains("\"y\""));
    let witnesses = f
        .claims
        .list_subject_stop_events("dep")
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == STOP_DANGLING_PREREQUISITE_STR)
        .count();
    assert_eq!(witnesses, 6, "three per edge, none past the routing");
}

#[tokio::test]
async fn retry_escalation_escalates_once_then_queues() {
    let f = Fixture::new();
    let clock = fixed(t0());
    let mut lc = Lifecycle::new("hard").with_run_id("run-1");
    walk(
        &mut lc,
        &[
            (Status::Ready, ""),
            (Status::Running, ""),
            (Status::Validating, ""),
            (Status::FailedValidation, "red"),
            (Status::Failed, "red"),
        ],
        t0(),
    );
    lc.retries = 1;
    f.control.create_lifecycle(&lc).unwrap();
    let mut attempt = Attempt::new(1, t0(), "run-1");
    attempt.outcome = Some(Outcome::ValidationFailed);
    attempt.ended_at = Some(t0());
    f.control.save_attempt("run-1", &attempt, None).unwrap();
    assert!(is_retry_exhausted(&f.lifecycle("run-1"), 1));
    assert!(!is_retry_exhausted(&f.lifecycle("run-1"), 2));

    let driven = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
    let seen = driven.clone();
    let drive: EscalationDriver = Arc::new(move |req, model| {
        seen.lock().unwrap().push((req.task_id.clone(), model));
        Box::pin(async { Some("run-escalated".to_owned()) })
    });
    let request = EscalationRequest {
        task_id: "hard".into(),
        task: task("hard"),
        run_id: "run-1".into(),
    };
    let ctx = EscalationContext {
        control: f.control.as_ref(),
        claims: f.claims.as_ref(),
        worker_id: "worker-a",
        drive: &drive,
        escalation_model: Some("opus".into()),
        max_retries: 1,
        bound: DEFAULT_ESCALATION_BOUND,
        lease_seconds: 60.0,
        steal_grace_seconds: 0.0,
        now: &clock,
        log: None,
    };
    let out = redrive_exhausted_retries(&ctx, std::slice::from_ref(&request))
        .await
        .unwrap();
    assert_eq!(out[0].result, EscalationResult::Escalated);
    assert_eq!(out[0].escalated_run_id, "run-escalated");
    assert_eq!(out[0].escalations, 1);
    assert_eq!(
        driven.lock().unwrap()[0],
        ("hard".to_owned(), Some("opus".to_owned()))
    );
    assert!(f.claims.load_claim("hard").unwrap().is_none());
    // The escalated run also exhausts: routed to the queue, never re-escalated.
    let out = redrive_exhausted_retries(&ctx, std::slice::from_ref(&request))
        .await
        .unwrap();
    assert_eq!(out[0].result, EscalationResult::Queued);
    let out = redrive_exhausted_retries(&ctx, &[request]).await.unwrap();
    assert_eq!(out[0].result, EscalationResult::Queued);
    assert_eq!(driven.lock().unwrap().len(), 1);
    let queue = f.claims.list_human_review_queue().unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].reason, "retries-exhausted-after-escalation");
}

#[test]
fn human_gate_redriver_classifies_and_queues_each_stop_once() {
    let f = Fixture::new();
    let clock = fixed(t0());
    f.seed("gate", "run-gate", Status::AwaitingApproval, "");
    let seed_failed = |task: &str, run: &str, error: &str| {
        f.seed(task, run, Status::Failed, error);
        let mut attempt = Attempt::new(1, t0(), run);
        attempt.outcome = Some(Outcome::AgentError);
        attempt.error = error.to_owned();
        attempt.ended_at = Some(t0());
        f.control.save_attempt(run, &attempt, None).unwrap();
    };
    seed_failed("aborted", "run-abort", "agent gave up");
    seed_failed("pricey", "run-budget", "budget ceiling breached: cost");
    f.seed("exhausted", "run-exh", Status::Failed, "red");
    let mut attempt = Attempt::new(1, t0(), "run-exh");
    attempt.outcome = Some(Outcome::ValidationFailed);
    f.control.save_attempt("run-exh", &attempt, None).unwrap();
    let req = |task: &str, run: &str| HumanGateRequest {
        task_id: task.into(),
        run_id: run.into(),
    };
    let requests = [
        req("gate", "run-gate"),
        req("aborted", "run-abort"),
        req("pricey", "run-budget"),
        req("exhausted", "run-exh"),
    ];
    let mut routed = BTreeSet::new();
    let out = redrive_human_gates(
        f.control.as_ref(),
        f.claims.as_ref(),
        &requests,
        &clock,
        None,
        &mut routed,
    )
    .unwrap();
    let reasons: Vec<(HumanGateResult, &str)> =
        out.iter().map(|o| (o.result, o.reason.as_str())).collect();
    assert_eq!(
        reasons,
        [
            (HumanGateResult::Queued, "awaiting-approval"),
            (HumanGateResult::Queued, "abort"),
            (HumanGateResult::Queued, "budget-ceiling"),
            (HumanGateResult::Skipped, ""),
        ]
    );
    let out = redrive_human_gates(
        f.control.as_ref(),
        f.claims.as_ref(),
        &requests,
        &clock,
        None,
        &mut BTreeSet::new(),
    )
    .unwrap();
    assert!(
        out[..3]
            .iter()
            .all(|o| o.result == HumanGateResult::AlreadyQueued)
    );
    assert_eq!(f.claims.list_human_review_queue().unwrap().len(), 3);
    assert_eq!(
        f.lifecycle("run-gate").status,
        Status::AwaitingApproval,
        "never resolved"
    );
}

// --- workflow shims --------------------------------------------------------------

#[test]
fn latest_lifecycle_row_and_stranded_run_ids() {
    let f = Fixture::new();
    assert!(
        latest_lifecycle_row(f.control.as_ref(), "none")
            .unwrap()
            .is_none()
    );
    f.seed("t", "run-old", Status::Running, "");
    f.seed("t", "run-new", Status::Validating, "");
    f.seed("u", "run-u", Status::Done, "");
    let latest = latest_lifecycle_row(f.control.as_ref(), "t")
        .unwrap()
        .unwrap();
    assert_eq!(latest.run_id, "run-new");
    assert_eq!(latest.status, Status::Validating);
    assert_eq!(
        stranded_run_ids(f.control.as_ref(), None).unwrap(),
        ["run-old", "run-new"]
    );
    assert_eq!(
        stranded_run_ids(f.control.as_ref(), Some("u")).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!((DEFAULT_MAX_TURNS, DEFAULT_MAX_RETRIES), (500, 1));
}

#[test]
fn primitives_decompose_fast_defaults_and_policy_overrides() {
    let environ = BTreeMap::from([
        ("HOME".to_owned(), "/h".to_owned()),
        ("TOKEN".to_owned(), "s3".to_owned()),
    ]);
    let fast = sandbox_agent_primitives(None, &environ);
    assert!(fast.agent_env.is_empty());
    assert!(fast.grader_env.is_none());
    assert_eq!(
        fast.permission_mode,
        SandboxPolicy_default_permission_mode()
    );
    let limits = sandbox_limit_primitives(None);
    assert_eq!(limits, SandboxLimitPrimitives::default());
    assert_eq!(checkpoint_nudge_seconds(None), 300.0);
    let config = harness_config_from_policy(None, 2, None);
    assert_eq!(config.max_retries, 2);
    assert_eq!(config.rubric_judge_max_turns, 32);

    let mut policy = WorkPolicy {
        worker_checkpoint_nudge_seconds: 0.0,
        agent_id: Some("codex".into()),
        ..WorkPolicy::default()
    };
    policy.sandbox.env.passthrough = vec!["TOKEN".into(), "ABSENT".into()];
    policy.sandbox.env.set_values = BTreeMap::from([("TOKEN".to_owned(), "override".to_owned())]);
    policy.sandbox.limits.max_cost_usd = 1.5;
    policy.sandbox.limits.max_tokens = 42;
    policy.sandbox.limits.rubric_judge_max_turns = Some(8);
    let agent = sandbox_agent_primitives(Some(&policy), &environ);
    assert_eq!(
        agent.agent_env,
        BTreeMap::from([("TOKEN".to_owned(), "override".to_owned())])
    );
    let grader_env = agent.grader_env.clone().unwrap();
    assert_eq!(grader_env["TOKEN"], "override");
    assert_eq!(grader_env["HOME"], "/h");
    assert_eq!(agent.agent_id.as_deref(), Some("codex"));
    let config = harness_config_from_policy(Some(&policy), 1, agent.grader_env);
    assert_eq!(config.max_cost_usd, 1.5);
    assert_eq!(config.max_tokens, 42);
    assert_eq!(config.rubric_judge_max_turns, 8);
    assert_eq!(config.checkpoint_nudge_seconds, 0.0);
    assert_eq!(config.agent_id, "codex");
    assert_eq!(
        attempt_budget_primitives(Some(&policy)).rubric_judge_max_turns,
        Some(8)
    );
}

#[allow(non_snake_case)]
fn SandboxPolicy_default_permission_mode() -> String {
    crate::policy::SandboxPolicy::default().permission_mode
}

fn git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn reachability_hold_withholds_a_cross_phase_dependent_until_the_phase_merges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("base.txt"), "base").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "base"]);
    // No phase branches: a total no-op.
    let mut a = TaskStatusRow::fresh("a");
    a.task_file = Some(root.join("tasks/active/02-b/a.json"));
    a.state = TaskState::Done;
    let mut c = TaskStatusRow::fresh("c");
    c.task_file = Some(root.join("tasks/active/03-c/c.json"));
    c.prerequisites = vec!["a".into()];
    let rows = vec![a, c];
    assert!(reachability_held_prerequisites(&rows, Some(root), Some("HEAD")).is_empty());
    // Phase 02-b landed its work on an unmerged integration branch.
    git(root, &["checkout", "-q", "-b", "flywheel/phase/02-b"]);
    std::fs::write(root.join("a.txt"), "a").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "a"]);
    git(root, &["checkout", "-q", "main"]);
    let holds = reachability_held_prerequisites(&rows, Some(root), Some("HEAD"));
    assert_eq!(holds.len(), 1);
    assert_eq!(holds["c"].blocking_phase, "02-b");
    assert_eq!(holds["c"].held_by, ["a"]);
    assert!(reachability_held_prerequisites(&rows, None, Some("HEAD")).is_empty());
    // The phase merges: the hold clears with no operator action.
    git(root, &["merge", "-q", "flywheel/phase/02-b"]);
    assert!(reachability_held_prerequisites(&rows, Some(root), Some("HEAD")).is_empty());
}

#[test]
fn run_task_error_classifies_optimistic_concurrency() {
    let occ = RunTaskError::Store(StoreError::optimistic_concurrency("r", 1, 2));
    assert!(occ.is_optimistic_concurrency());
    assert!(!RunTaskError::Interrupted.is_optimistic_concurrency());
    assert!(!RunTaskError::Other("x".into()).is_optimistic_concurrency());
    assert!(matches!(
        RunTaskError::from(flywheel_core::harness::HarnessError::Interrupted),
        RunTaskError::Interrupted
    ));
}

#[test]
fn default_constants_match_the_python_module() {
    assert_eq!(DEFAULT_LEASE_SECONDS, 300.0);
    assert_eq!(DEFAULT_RECONCILE_SECONDS, 15.0);
    assert_eq!(DEFAULT_SWEEP_SECONDS, 15.0);
    assert_eq!(DEFAULT_LANDING_REDRIVE_BOUND, 3);
    assert_eq!(DEFAULT_ESCALATION_BOUND, 1);
    assert_eq!(DEFAULT_PREREQ_REDRIVE_BOUND, 3);
    assert_eq!(DEFAULT_SESSION_PAUSE_CEILING_SECONDS, 21600.0);
    assert_eq!(DEFAULT_WORKER_CONCURRENCY, 1);
    assert_eq!(APPROVAL_SWEEP_MARK_TTL_SECONDS, 30.0);
    assert_eq!(STRANDED_STATUSES, [Status::Running, Status::Validating]);
    assert_eq!(RunMode::Resume.to_string(), "resume");
    assert_eq!(RedriveResult::InProgress.as_str(), "in-progress");
}
