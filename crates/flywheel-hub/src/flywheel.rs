//! The Flywheel panel: verified work beside the terminals.
//!
//! For every workspace folder that is a flywheel-adopted repo (a
//! `flywheel.toml` or a `.flywheel/` dir) the hub shows what the loop is
//! doing there -- active phases and their tasks with lifecycle status,
//! recent runs with their grader receipts, worker / autopilot liveness,
//! claims, and the stop / strand alerts `fw status` surfaces -- read-only.
//!
//! The data comes from a [`FlywheelProvider`] the embedding binary
//! injects (`flywheel::hub_flywheel` in the `flywheel` crate, which
//! reuses the console's store and snapshot code): this crate stays a
//! terminal workspace with no dependency on the loop's crates, and the
//! seam is plain data.
//!
//! [`FlywheelMonitor`] keeps one cached [`FlywheelPanel`] per adopted
//! folder, recomputed off the async runtime on a dedicated thread per
//! folder (a slow store never stalls the others or the hub) when the
//! provider's cheap fingerprint changes, and at least every
//! [`MonitorTiming::refresh`] (liveness is a matter of time, not files).
//! Changes reach browsers as `{"type":"flywheel"}` messages on
//! `/api/events`.

use crate::fanout::promote::{TaskDraft, TaskFilePlan, TaskSeed, TaskWritten};
use crate::hub::{Hub, SessionView};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// A task's state in the panel's vocabulary.
///
/// Mapped from the orchestrator's task states: never attempted is
/// `pending`; an active lifecycle is `running`; a DONE lifecycle (its
/// graders passed) is `verified` unless its landing parked (`parked`); a
/// failed latest run is `failed`; a manual gate is `awaiting`; a pause on
/// unmet predicates is `blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLight {
    Pending,
    Running,
    Verified,
    Failed,
    Parked,
    Awaiting,
    Blocked,
}

impl TaskLight {
    pub const ALL: [TaskLight; 7] = [
        TaskLight::Pending,
        TaskLight::Running,
        TaskLight::Verified,
        TaskLight::Failed,
        TaskLight::Parked,
        TaskLight::Awaiting,
        TaskLight::Blocked,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            TaskLight::Pending => "pending",
            TaskLight::Running => "running",
            TaskLight::Verified => "verified",
            TaskLight::Failed => "failed",
            TaskLight::Parked => "parked",
            TaskLight::Awaiting => "awaiting",
            TaskLight::Blocked => "blocked",
        }
    }
}

/// A `{kind, detail}` pair: a landing park or a pre-run stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub kind: String,
    pub detail: String,
}

/// One task of an active phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelTask {
    pub id: String,
    pub state: TaskLight,
    /// The orchestrator's own state name (`fresh`, `in_progress`,
    /// `retryable`, ...), as `fw status` prints it.
    pub flywheel_state: String,
    pub run_id: Option<String>,
    pub run_status: Option<String>,
    /// The latest run's error, if any.
    #[serde(default)]
    pub error: String,
    pub worker_id: Option<String>,
    #[serde(default)]
    pub prerequisites: Vec<String>,
    /// The DONE run's landing parked (it never landed).
    pub stranded: Option<Note>,
    /// A pre-run stop keyed to the task.
    pub stopped: Option<Note>,
    /// The parked manual gate's instruction.
    pub awaiting: Option<String>,
    /// The predicates an interrupted run waits on (`type=name, ...`).
    pub blocked_on: Option<String>,
}

/// An active phase (a directory under `tasks/active/`) and its tasks in
/// listing order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelPhase {
    pub name: String,
    pub tasks: Vec<PanelTask>,
}

/// An in-flight run (RUNNING / VALIDATING / AWAITING_APPROVAL), as the
/// console's live rows show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveRun {
    pub run_id: String,
    pub task_id: String,
    pub status: String,
    pub attempt: Option<u32>,
    pub iteration: Option<u64>,
    pub age_seconds: Option<i64>,
    pub idle_seconds: Option<i64>,
    pub tokens: u64,
    pub cost_usd: f64,
    pub last_detail: String,
    pub worker_id: Option<String>,
    pub awaiting: Option<String>,
}

/// One grader's receipt on a run's latest attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraderOutcome {
    pub name: String,
    pub kind: String,
    pub passed: bool,
    pub duration_ms: u64,
}

/// A recent run (newest first) with its latest attempt's grader receipts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentRun {
    pub run_id: String,
    pub task_id: String,
    pub status: String,
    /// The latest attempt's outcome (`succeeded`, `failed_validation`, ...).
    pub outcome: Option<String>,
    pub attempts: u32,
    pub started_at: Option<DateTime<Utc>>,
    /// The latest recorded lifecycle transition.
    pub updated_at: Option<DateTime<Utc>>,
    pub tokens: u64,
    pub cost_usd: f64,
    #[serde(default)]
    pub error: String,
    pub graders: Vec<GraderOutcome>,
}

/// A daemon's liveness, read from what it leaves in the store or on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
    pub live: bool,
    pub detail: String,
}

/// A task claim in the orchestrator's `task_claims`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelClaim {
    pub task_id: String,
    pub worker_id: String,
    pub claimed_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
    /// The lease has not lapsed.
    pub live: bool,
}

/// Why a folder needs the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// The task's latest run failed.
    Failed,
    /// A DONE run's landing parked: verified work that never landed.
    Stranded,
    /// A pre-run stop (the orchestrator refused to start it).
    Stopped,
    /// Parked on a manual gate.
    Awaiting,
    /// Interrupted on unmet predicates.
    Blocked,
}

/// One entry of the folder's "needs you" list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    pub kind: AlertKind,
    /// The task id (or the stop's subject).
    pub subject: String,
    pub detail: String,
    pub run_id: Option<String>,
}

/// Everything the panel shows for one adopted folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlywheelPanel {
    pub folder: PathBuf,
    pub generated_at: DateTime<Utc>,
    /// Where tasks come from (`directory .flywheel/tasks`, `github`, ...).
    pub source: String,
    /// The store read (`sqlite .flywheel/flywheel.sqlite`, `postgres`).
    pub store: String,
    /// The main checkout, when the folder is a linked git worktree: flywheel
    /// keeps a store per checkout, so this panel reads the worktree's own
    /// (not the main checkout's).
    #[serde(default)]
    pub worktree_of: Option<PathBuf>,
    /// Why some of the panel is missing (no store yet, a policy or store
    /// error); what could be read is still shown.
    pub problem: Option<String>,
    pub phases: Vec<PanelPhase>,
    /// Said instead of tasks when the source is not listed here.
    pub tasks_note: Option<String>,
    pub live: Vec<LiveRun>,
    pub runs: Vec<RecentRun>,
    pub worker: DaemonState,
    pub autopilot: DaemonState,
    pub claims: Vec<PanelClaim>,
    pub alerts: Vec<Alert>,
}

impl FlywheelPanel {
    /// A panel with nothing read yet (for `problem`-only answers).
    pub fn empty(folder: PathBuf, generated_at: DateTime<Utc>) -> Self {
        Self {
            folder,
            generated_at,
            source: String::new(),
            store: String::new(),
            worktree_of: None,
            problem: None,
            phases: Vec::new(),
            tasks_note: None,
            live: Vec::new(),
            runs: Vec::new(),
            worker: DaemonState {
                live: false,
                detail: "unknown".into(),
            },
            autopilot: DaemonState {
                live: false,
                detail: "unknown".into(),
            },
            claims: Vec::new(),
            alerts: Vec::new(),
        }
    }

    /// The small form pushed to every browser (sidebar badges, the
    /// attention list).
    pub fn summary(&self) -> FlywheelSummary {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for task in self.phases.iter().flat_map(|p| &p.tasks) {
            *counts.entry(task.state.as_str().to_owned()).or_default() += 1;
        }
        FlywheelSummary {
            folder: self.folder.clone(),
            generated_at: self.generated_at,
            phases: self.phases.iter().map(|p| p.name.clone()).collect(),
            counts,
            alerts: self.alerts.clone(),
            live_runs: self.live.len(),
            worker_live: self.worker.live,
            autopilot_live: self.autopilot.live,
            problem: self.problem.clone(),
            worktree_of: self.worktree_of.clone(),
        }
    }

    /// The same panel ignoring when it was read: two reads that differ
    /// only in `generated_at` (and the ages of in-flight runs) are not a
    /// change worth pushing.
    fn same_content(&self, other: &FlywheelPanel) -> bool {
        let strip = |panel: &FlywheelPanel| {
            let mut copy = panel.clone();
            copy.generated_at = DateTime::<Utc>::UNIX_EPOCH;
            for run in &mut copy.live {
                run.age_seconds = None;
                run.idle_seconds = None;
            }
            copy
        };
        strip(self) == strip(other)
    }
}

/// A folder's panel in brief.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlywheelSummary {
    pub folder: PathBuf,
    pub generated_at: DateTime<Utc>,
    pub phases: Vec<String>,
    /// Tasks per [`TaskLight`] name (absent states have none).
    pub counts: BTreeMap<String, usize>,
    pub alerts: Vec<Alert>,
    pub live_runs: usize,
    pub worker_live: bool,
    pub autopilot_live: bool,
    pub problem: Option<String>,
    /// See [`FlywheelPanel::worktree_of`].
    #[serde(default)]
    pub worktree_of: Option<PathBuf>,
}

/// One line of a run's agent transcript (redacted), in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEntry {
    pub ts: DateTime<Utc>,
    /// `agent_text`, `tool_call`, `tool_result`, `user_text`,
    /// `operator_say`, `system`, `result`, `lifecycle`, `gate`.
    pub kind: String,
    pub header: String,
    pub body: String,
    /// An erroring tool result or a failed control command.
    pub error: bool,
}

/// A run's output: its `fw show` summary and its transcript from entry
/// `from` on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunOutput {
    pub run_id: String,
    pub task_id: Option<String>,
    pub status: Option<String>,
    /// Still in flight: more output may come (poll again with `since =
    /// next`).
    pub active: bool,
    /// What `fw show <run>` prints (lifecycle, attempts, grader receipts,
    /// decisions, final agent output).
    pub summary: String,
    /// The index of `entries[0]` in the whole transcript (entries older
    /// than the retained window are dropped).
    pub from: usize,
    pub entries: Vec<RunEntry>,
    /// The cursor for the next poll.
    pub next: usize,
}

/// Reads flywheel state for a folder. Implemented by the embedding binary
/// (see the module doc); every method may block (filesystem, store) and is
/// only ever called off the async runtime.
pub trait FlywheelProvider: Send + Sync + 'static {
    /// Whether `folder` is a flywheel-adopted repo. Cheap (a few stats):
    /// called for every workspace folder on every monitor tick.
    fn adopted(&self, folder: &Path) -> bool;

    /// A cheap digest of the files the panel derives from (policy, store
    /// files, task tree, daemon records); a change triggers a re-read.
    fn fingerprint(&self, folder: &Path) -> u64;

    /// Read the folder's panel. Never fails: what could not be read is
    /// named in [`FlywheelPanel::problem`].
    fn panel(&self, folder: &Path) -> FlywheelPanel;

    /// A run's output, its transcript from entry `since` on.
    fn run_output(&self, folder: &Path, run_id: &str, since: usize) -> Result<RunOutput, String>;

    /// The argv that runs a worker for `folder` (run with `folder` as the
    /// working directory), e.g. `[/path/to/fw, worker]`; restricted to
    /// `scope`'s tasks when it names any (`[.., worker, --task=ID]`).
    fn worker_command(&self, folder: &Path, scope: &WorkerScope) -> Vec<String>;

    /// Drop whatever is kept open for `folder` (stores, listings,
    /// transcripts): it left every workspace. A later read reopens.
    fn forget(&self, folder: &Path);

    /// What writing `draft` as a task of `folder`'s directory work source
    /// would write: the path and the exact bytes, validated through the
    /// loop's own task loader. Refused for a non-directory source, an
    /// invalid definition, or a task id already in use there.
    fn plan_task(&self, folder: &Path, draft: &TaskDraft) -> Result<TaskFilePlan, String>;

    /// Write `draft` (as [`FlywheelProvider::plan_task`] plans it, which
    /// must still produce exactly `contents`) as a new file -- never over
    /// an existing one -- and read it back through the work source.
    fn write_task(
        &self,
        folder: &Path,
        draft: &TaskDraft,
        contents: &str,
    ) -> Result<TaskWritten, String>;

    /// Task `task_id` of `folder`'s directory work source as a fan-out
    /// seed, with its held-out graders (failing closed: a configured
    /// held-out registry that cannot be read is an error, never "none").
    fn task_seed(&self, folder: &Path, task_id: &str) -> Result<TaskSeed, String>;
}

/// What a browser hears about a folder.
#[derive(Debug, Clone, PartialEq)]
pub enum FlywheelEvent {
    /// The folder's panel changed (or was read for the first time).
    Changed(Box<FlywheelSummary>),
    /// The folder left every workspace (or stopped being adopted).
    Removed(PathBuf),
}

/// How often the monitor looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorTiming {
    /// How often each folder's fingerprint is checked (and the workspace
    /// list is re-read for added / removed folders).
    pub poll: Duration,
    /// The longest a panel goes without a re-read (leases and daemon
    /// records lapse with time, not with a file change).
    pub refresh: Duration,
}

impl Default for MonitorTiming {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(2),
            refresh: Duration::from_secs(15),
        }
    }
}

const EVENT_DEPTH: usize = 64;

/// A folder's watcher thread and its latest panel.
struct FolderSlot {
    stop: AtomicBool,
    /// Poked to stop (or re-read) without waiting out the poll.
    wake: (Mutex<bool>, Condvar),
    panel: Mutex<Option<Arc<FlywheelPanel>>>,
}

impl FolderSlot {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            wake: (Mutex::new(false), Condvar::new()),
            panel: Mutex::new(None),
        }
    }

    fn poke(&self) {
        let (lock, cvar) = &self.wake;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cvar.notify_all();
    }

    /// Sleep up to `timeout`; true when poked (and clears the poke).
    fn sleep(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &self.wake;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut guard, _) = cvar
            .wait_timeout_while(guard, timeout, |poked| !*poked)
            .unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *guard)
    }

    fn panel(&self) -> Option<Arc<FlywheelPanel>> {
        self.panel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Keeps every adopted workspace folder's panel current (see the module
/// doc).
pub struct FlywheelMonitor {
    provider: Arc<dyn FlywheelProvider>,
    timing: MonitorTiming,
    folders: Mutex<HashMap<PathBuf, Arc<FolderSlot>>>,
    events: broadcast::Sender<FlywheelEvent>,
    /// Held while a worker start checks for a running one and creates its
    /// session, so two starts cannot both pass the check.
    starting: Mutex<()>,
    /// Blocking reads still running (see [`FlywheelMonitor::begin_read`]).
    reads: Arc<ReadGate>,
}

/// The keys of blocking reads in flight.
#[derive(Debug, Default)]
pub struct ReadGate {
    keys: Mutex<HashSet<String>>,
}

impl ReadGate {
    /// Claim `key`; `None` while an earlier claim of it is held.
    pub fn begin(self: &Arc<Self>, key: String) -> Option<ReadGuard> {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        keys.insert(key.clone()).then(|| ReadGuard {
            gate: Arc::clone(self),
            key,
        })
    }
}

/// A blocking read in flight; its key is released on drop (when the read
/// really finishes, not when the request that asked for it gave up).
pub struct ReadGuard {
    gate: Arc<ReadGate>,
    key: String,
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        self.gate
            .keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

impl std::fmt::Debug for FlywheelMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlywheelMonitor")
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

impl FlywheelMonitor {
    pub fn new(provider: Arc<dyn FlywheelProvider>, timing: MonitorTiming) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_DEPTH);
        Arc::new(Self {
            provider,
            timing,
            folders: Mutex::new(HashMap::new()),
            events,
            starting: Mutex::new(()),
            reads: Arc::default(),
        })
    }

    /// Claim the read named `key`; `None` while an earlier one with that
    /// key is still running. A read that outlives its request's timeout
    /// keeps its blocking thread until the store answers: refusing a
    /// repeat (a run dialog polls every 2s) bounds those threads to one per
    /// key instead of one per poll.
    pub fn begin_read(&self, key: String) -> Option<ReadGuard> {
        self.reads.begin(key)
    }

    /// Serialize worker starts (held across check-then-create).
    pub fn starting(&self) -> std::sync::MutexGuard<'_, ()> {
        self.starting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn provider(&self) -> &Arc<dyn FlywheelProvider> {
        &self.provider
    }

    /// Panel changes, for the events socket.
    pub fn subscribe(&self) -> broadcast::Receiver<FlywheelEvent> {
        self.events.subscribe()
    }

    /// Whether `folder` is watched (an adopted folder of some workspace).
    pub fn watches(&self, folder: &Path) -> bool {
        self.folders().contains_key(folder)
    }

    /// The latest panel read for `folder`, if it is watched and was read.
    pub fn panel(&self, folder: &Path) -> Option<Arc<FlywheelPanel>> {
        self.folders().get(folder).and_then(|slot| slot.panel())
    }

    /// Every watched folder's latest summary, by folder.
    pub fn summaries(&self) -> Vec<FlywheelSummary> {
        let mut out: Vec<FlywheelSummary> = self
            .folders()
            .values()
            .filter_map(|slot| slot.panel().map(|p| p.summary()))
            .collect();
        out.sort_by(|a, b| a.folder.cmp(&b.folder));
        out
    }

    /// Ask `folder`'s watcher to re-read now (after an action that changes
    /// what it shows).
    pub fn refresh(&self, folder: &Path) {
        if let Some(slot) = self.folders().get(folder) {
            slot.poke();
        }
    }

    fn folders(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Arc<FolderSlot>>> {
        self.folders.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start watching `hub`'s workspaces: a coordinator thread re-reads
    /// the workspace list every poll and starts / stops one watcher thread
    /// per adopted folder. Everything stops once the hub is dropped.
    pub fn watch(self: &Arc<Self>, hub: &Arc<Hub>) -> std::io::Result<()> {
        let weak = Arc::downgrade(hub);
        let monitor = Arc::clone(self);
        std::thread::Builder::new()
            .name("fw-hub-flywheel".into())
            .spawn(move || monitor.coordinate(weak))
            .map(|_| ())
    }

    fn coordinate(self: Arc<Self>, hub: Weak<Hub>) {
        loop {
            let Some(strong) = hub.upgrade() else { break };
            let folders: BTreeSet<PathBuf> = strong
                .workspaces()
                .into_iter()
                .flat_map(|w| w.folders)
                .filter(|f| !f.missing)
                .map(|f| f.path)
                .collect();
            drop(strong);
            let adopted: BTreeSet<PathBuf> = folders
                .into_iter()
                .filter(|f| self.provider.adopted(f))
                .collect();
            self.reconcile(&adopted);
            std::thread::sleep(self.timing.poll);
        }
        let slots: Vec<Arc<FolderSlot>> = self.folders().drain().map(|(_, s)| s).collect();
        for slot in slots {
            slot.stop.store(true, Ordering::SeqCst);
            slot.poke();
        }
    }

    /// Watch exactly `adopted`: start watchers for new folders, stop (and
    /// announce) the ones that left.
    fn reconcile(self: &Arc<Self>, adopted: &BTreeSet<PathBuf>) {
        let mut added = Vec::new();
        {
            // Removals are announced under the map lock: a watcher sends
            // its `Changed` under the same lock after re-checking its stop
            // flag, so no `Changed` can follow a folder's `Removed`.
            let mut folders = self.folders();
            folders.retain(|path, slot| {
                let keep = adopted.contains(path);
                if !keep {
                    slot.stop.store(true, Ordering::SeqCst);
                    slot.poke();
                    let _ = self.events.send(FlywheelEvent::Removed(path.clone()));
                }
                keep
            });
            for path in adopted {
                if !folders.contains_key(path) {
                    let slot = Arc::new(FolderSlot::new());
                    folders.insert(path.clone(), Arc::clone(&slot));
                    added.push((path.clone(), slot));
                }
            }
        }
        for (path, slot) in added {
            let monitor = Arc::clone(self);
            let watched = path.clone();
            let spawned = std::thread::Builder::new()
                .name("fw-hub-flywheel-folder".into())
                .spawn(move || monitor.watch_folder(&watched, &slot));
            if spawned.is_err() {
                // Out of threads: try again on the next tick.
                self.folders().remove(&path);
            }
        }
    }

    fn watch_folder(&self, folder: &Path, slot: &FolderSlot) {
        let mut last_print: Option<u64> = None;
        let mut last_read: Option<Instant> = None;
        let mut forced = false;
        while !slot.stop.load(Ordering::SeqCst) {
            let print = self.provider.fingerprint(folder);
            let stale = last_read.is_none_or(|at| at.elapsed() >= self.timing.refresh);
            if forced || stale || last_print != Some(print) {
                let panel = self.provider.panel(folder);
                last_print = Some(print);
                last_read = Some(Instant::now());
                if slot.stop.load(Ordering::SeqCst) {
                    break;
                }
                let changed = {
                    let mut current = slot.panel.lock().unwrap_or_else(PoisonError::into_inner);
                    let changed = current.as_ref().is_none_or(|old| !old.same_content(&panel));
                    let summary = panel.summary();
                    *current = Some(Arc::new(panel));
                    changed.then_some(summary)
                };
                if let Some(summary) = changed {
                    let folders = self.folders();
                    if slot.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = self.events.send(FlywheelEvent::Changed(Box::new(summary)));
                    drop(folders);
                }
            }
            forced = slot.sleep(self.timing.poll);
        }
        // Gone from every workspace (unless it was re-added meanwhile):
        // close what the provider keeps open for it.
        if !self.watches(folder) {
            self.provider.forget(folder);
        }
    }
}

/// The argv a hub shell session runs to start a worker: `worker` (the
/// provider's argv) under `/bin/sh`, then the user's shell once it exits,
/// so the terminal stays open on the worker's last words. Restored after
/// a hub restart it comes back as a plain shell (a one-off command is
/// never re-run; see `restore`). The wrapper traps Ctrl-C/Ctrl-\ with a
/// no-op rather than ignoring them, so the worker (which execs with default
/// handlers) still gets the signal while the wrapper -- in the same
/// foreground group -- survives it and goes on to the shell. The shell
/// is exec'd with [`WORKER_EXIT_ENV`] set to the worker's exit status, so
/// the hub can read how it ended from the process (see [`worker_run`]).
pub fn worker_session_command(worker: &[String]) -> Vec<String> {
    let script = "trap : INT QUIT; \"$@\"; status=$?; printf '\\n[fw worker exited with status %s]\\n' \"$status\"; \
                  FLYWHEEL_WORKER_EXIT=$status exec \"${SHELL:-/bin/sh}\"";
    let mut argv = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        script.to_owned(),
        WORKER_MARKER.to_owned(),
    ];
    argv.extend(worker.iter().cloned());
    argv
}

/// The marker argv[0] of `$@` in [`worker_session_command`]'s script.
pub const WORKER_MARKER: &str = "fw-worker";
/// The variable the shell a finished worker leaves behind carries its
/// exit status in.
pub const WORKER_EXIT_ENV: &str = "FLYWHEEL_WORKER_EXIT";

/// How a "Start worker here" / "Run <id>" session's worker stands: still
/// running, or exited (with its status when the platform shows it). The
/// panel counts only running ones; the sidebar shows how a finished one
/// ended instead of `live`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkerRun {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
}

/// Whether `record` is a worker session (started by the hub, not a plain
/// shell restored after a restart).
fn is_worker_session(record: &crate::state::SessionRecord) -> bool {
    record.fallback_command.is_none()
        && record.command.get(3).map(String::as_str) == Some(WORKER_MARKER)
}

/// The worker state of a worker session whose process is `pid` (`None`
/// once the session's process is gone); `None` for any other session.
pub fn worker_run(record: &crate::state::SessionRecord, pid: Option<u32>) -> Option<WorkerRun> {
    if !is_worker_session(record) {
        return None;
    }
    Some(match pid {
        Some(pid) if process_runs_worker(pid) => WorkerRun {
            running: true,
            exit_status: None,
        },
        Some(pid) => WorkerRun {
            running: false,
            exit_status: process_worker_exit(pid),
        },
        None => WorkerRun {
            running: false,
            exit_status: None,
        },
    })
}

/// The exit status a finished worker's shell was exec'd with.
#[cfg(target_os = "linux")]
fn process_worker_exit(pid: u32) -> Option<i32> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{WORKER_EXIT_ENV}=");
    environ
        .split(|b| *b == 0)
        .filter_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .next_back()
        .and_then(|value| std::str::from_utf8(value).ok()?.parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn process_worker_exit(_pid: u32) -> Option<i32> {
    None
}
/// The name a started worker's session gets (numbered when taken).
pub const WORKER_SESSION_NAME: &str = "fw worker";

/// Which tasks a started worker runs: empty `tasks` drains the whole
/// queue (`fw worker`); else only those (`fw worker --task ID ...`),
/// plus their not-done prerequisites with `with_prereqs`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerScope {
    pub tasks: Vec<String>,
    pub with_prereqs: bool,
}

impl WorkerScope {
    /// The longest task id a scope accepts.
    pub const MAX_ID: usize = 200;
    /// The most tasks one scoped start names.
    pub const MAX_TASKS: usize = 50;

    /// A scope over `tasks` (validated: non-empty printable ids without
    /// whitespace that do not start with `-`, deduplicated, bounded), or
    /// why it is refused. The flags spell `--task=<id>` anyway; refusing a
    /// dash-led id here answers 400 instead of starting a worker that only
    /// exits 2 on an id like `--once`.
    pub fn new(tasks: Vec<String>, with_prereqs: bool) -> Result<Self, String> {
        if with_prereqs && tasks.is_empty() {
            return Err("with_prereqs needs at least one task".into());
        }
        if tasks.len() > Self::MAX_TASKS {
            return Err(format!("at most {} tasks per worker", Self::MAX_TASKS));
        }
        let mut unique: Vec<String> = Vec::new();
        for id in tasks {
            if id.is_empty()
                || id.starts_with('-')
                || id.len() > Self::MAX_ID
                || id.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(format!("{id:?} is not a task id"));
            }
            if !unique.contains(&id) {
                unique.push(id);
            }
        }
        Ok(Self {
            tasks: unique,
            with_prereqs,
        })
    }

    /// The `fw worker` flags for this scope (the `=` spelling, so an id
    /// can never read as an option).
    pub fn flags(&self) -> Vec<String> {
        let mut flags: Vec<String> = self.tasks.iter().map(|t| format!("--task={t}")).collect();
        if self.with_prereqs {
            flags.push("--with-prereqs".to_owned());
        }
        flags
    }

    /// The session-name base: `fw worker`, or `fw worker <ids>`.
    fn session_base(&self) -> String {
        if self.tasks.is_empty() {
            return WORKER_SESSION_NAME.to_owned();
        }
        let ids = self.tasks.join(", ");
        let ids = if ids.chars().count() > 40 {
            format!("{}...", ids.chars().take(37).collect::<String>())
        } else {
            ids
        };
        format!("{WORKER_SESSION_NAME} {ids}")
    }
}

/// Whether `view` is a "Start worker here" session in `folder` whose
/// worker is still running (not yet the shell it leaves behind). On Linux
/// that is read from the session process's argv: its `/bin/sh` keeps the
/// worker marker until it execs the shell (and a session still between
/// fork and exec counts as starting one). Elsewhere a running session
/// counts.
pub fn runs_worker(view: &SessionView, folder: &Path) -> bool {
    let record = &view.record;
    if record.cwd != folder || !is_worker_session(record) {
        return false;
    }
    let Some(pid) = view.live.as_ref().and_then(|l| l.pid) else {
        return false;
    };
    process_runs_worker(pid)
}

#[cfg(target_os = "linux")]
fn process_runs_worker(pid: u32) -> bool {
    let Ok(argv) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    if argv
        .split(|b| *b == 0)
        .any(|arg| arg == WORKER_MARKER.as_bytes())
    {
        return true;
    }
    // Forked but not yet exec'd (a session created a moment ago): still
    // this binary, about to become the worker's `/bin/sh`.
    let exe = |path: String| std::fs::read_link(path).ok();
    exe(format!("/proc/{pid}/exe")).is_some_and(|e| Some(e) == exe("/proc/self/exe".into()))
}

#[cfg(not(target_os = "linux"))]
fn process_runs_worker(_pid: u32) -> bool {
    true
}

/// A worker session name no session has: [`WORKER_SESSION_NAME`], else
/// `fw worker 2`, `fw worker 3`, ... (names address sessions in `fw hub`
/// verbs, so they stay unique).
pub fn worker_session_name<'a>(
    scope: &WorkerScope,
    taken: impl IntoIterator<Item = &'a str>,
) -> String {
    let taken: HashSet<&str> = taken.into_iter().map(str::trim).collect();
    let base = scope.session_base();
    if !taken.contains(base.as_str()) {
        return base;
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|name| !taken.contains(name.as_str()))
        .expect("an unbounded range has a free name")
}

/// A run id the transcript reader accepts: `[A-Za-z0-9_-]`, 1..=128 (it
/// names a file under the logs dir).
pub fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 128
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, state: TaskLight) -> PanelTask {
        PanelTask {
            id: id.into(),
            state,
            flywheel_state: String::new(),
            run_id: None,
            run_status: None,
            error: String::new(),
            worker_id: None,
            prerequisites: Vec::new(),
            stranded: None,
            stopped: None,
            awaiting: None,
            blocked_on: None,
        }
    }

    #[test]
    fn summary_counts_tasks_by_light_across_phases() {
        let mut panel = FlywheelPanel::empty("/r".into(), Utc::now());
        panel.phases = vec![
            PanelPhase {
                name: "01-a".into(),
                tasks: vec![task("a", TaskLight::Verified), task("b", TaskLight::Failed)],
            },
            PanelPhase {
                name: "02-b".into(),
                tasks: vec![task("c", TaskLight::Verified)],
            },
        ];
        panel.worker.live = true;
        let summary = panel.summary();
        assert_eq!(summary.phases, ["01-a", "02-b"]);
        assert_eq!(summary.counts["verified"], 2);
        assert_eq!(summary.counts["failed"], 1);
        assert!(!summary.counts.contains_key("pending"));
        assert!(summary.worker_live && !summary.autopilot_live);
    }

    #[test]
    fn content_comparison_ignores_read_time_and_run_ages() {
        let mut a = FlywheelPanel::empty("/r".into(), Utc::now());
        a.live.push(LiveRun {
            run_id: "run-1".into(),
            task_id: "t".into(),
            status: "running".into(),
            attempt: Some(1),
            iteration: None,
            age_seconds: Some(5),
            idle_seconds: Some(1),
            tokens: 10,
            cost_usd: 0.0,
            last_detail: String::new(),
            worker_id: None,
            awaiting: None,
        });
        let mut b = a.clone();
        b.generated_at = a.generated_at + chrono::Duration::seconds(30);
        b.live[0].age_seconds = Some(35);
        assert!(a.same_content(&b));
        b.live[0].tokens = 11;
        assert!(!a.same_content(&b));
    }

    #[test]
    fn worker_session_runs_the_worker_then_a_shell() {
        let argv = worker_session_command(&["/opt/fw".into(), "worker".into()]);
        assert_eq!(argv[..2], ["/bin/sh", "-c"]);
        assert_eq!(argv[3..], ["fw-worker", "/opt/fw", "worker"]);
        // Through a real shell: "$@" is the worker argv, verbatim.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let script = argv[2].replace("exec \"${SHELL:-/bin/sh}\"", "true");
        let out = std::process::Command::new(&argv[0])
            .args(["-c", &script, "fw-worker", "touch"])
            .arg(&marker)
            .output()
            .unwrap();
        assert!(marker.exists());
        assert!(String::from_utf8_lossy(&out.stdout).contains("exited with status 0"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ctrl_c_stops_the_worker_but_leaves_the_shell() {
        use crate::pty::{PtySession, SpawnSpec};
        use crate::vt::TextSource;
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("shell");
        crate::test_support::write_script(&shell, "#!/bin/sh\necho shell-up\nexec sleep 30\n");
        let argv = worker_session_command(&[
            "/bin/sh".into(),
            "-c".into(),
            "echo worker-up; exec sleep 30".into(),
        ]);
        let session = PtySession::spawn(
            SpawnSpec {
                argv,
                cwd: dir.path().to_path_buf(),
                env: vec![("SHELL".into(), shell.display().to_string())],
                env_remove: Vec::new(),
                cols: 80,
                rows: 24,
                scrollback_lines: 100,
            },
            |_| {},
        )
        .unwrap();
        let screen = || {
            session
                .screen_text(TextSource::Recent { lines: 20 })
                .lines
                .join("\n")
        };
        let until = |needle: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !screen().contains(needle) {
                assert!(Instant::now() < deadline, "no {needle:?} in {:?}", screen());
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        until("worker-up");
        let _ = session.write(b"\x03".to_vec());
        until("shell-up");
        assert!(
            screen().contains("[fw worker exited with status 130]"),
            "{}",
            screen()
        );
        assert!(session.exit_info().is_none(), "the session must survive ^C");
        session.kill(Duration::from_millis(200));
        assert!(session.wait_exit(Duration::from_secs(10)));
    }

    #[test]
    fn worker_session_names_stay_unique() {
        let all = WorkerScope::default();
        assert_eq!(worker_session_name(&all, ["shell", "claude"]), "fw worker");
        assert_eq!(worker_session_name(&all, ["fw worker"]), "fw worker 2");
        assert_eq!(
            worker_session_name(&all, [" fw worker ", "fw worker 2", "fw worker 4"]),
            "fw worker 3"
        );
        let one = WorkerScope::new(vec!["beta".into()], false).unwrap();
        assert_eq!(worker_session_name(&one, ["fw worker"]), "fw worker beta");
        assert_eq!(
            worker_session_name(&one, ["fw worker beta"]),
            "fw worker beta 2"
        );
        let long = WorkerScope::new((0..9).map(|n| format!("task-{n}")).collect(), false).unwrap();
        assert_eq!(
            worker_session_name(&long, []),
            "fw worker task-0, task-1, task-2, task-3, task-..."
        );
    }

    #[test]
    fn worker_scopes_validate_ids_and_render_safe_flags() {
        let scope = WorkerScope::new(vec!["a-1".into(), "b".into(), "a-1".into()], true).unwrap();
        assert_eq!(scope.tasks, ["a-1", "b"]);
        assert_eq!(scope.flags(), ["--task=a-1", "--task=b", "--with-prereqs"]);
        assert!(WorkerScope::default().flags().is_empty());
        for bad in ["", "a b", "a\nb", "a\0b", "--once", "-a"] {
            assert!(
                WorkerScope::new(vec![bad.into()], false).is_err(),
                "{bad:?}"
            );
        }
        assert!(WorkerScope::new(vec!["x".repeat(201)], false).is_err());
        assert!(WorkerScope::new(Vec::new(), true).is_err());
        assert!(WorkerScope::new((0..51).map(|n| n.to_string()).collect(), false).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_worker_session_counts_as_running_until_its_shell_takes_over() {
        // Kills the session process however the test ends.
        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let until = |what: &str, probe: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !probe() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        // The real session script with a worker that waits on a file (the
        // test says when it "exits"), and a shell that stays up after.
        let dir = tempfile::tempdir().unwrap();
        let gate = dir.path().join("go");
        let shell = dir.path().join("shell");
        crate::test_support::write_script(&shell, "#!/bin/sh\nexec sleep 30\n");
        let wait = format!("while [ ! -e {} ]; do sleep 0.05; done", gate.display());
        let argv = worker_session_command(&["/bin/sh".into(), "-c".into(), wait]);
        let child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("SHELL", &shell)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let child = Reap(child);
        until("the worker to start", &|| process_runs_worker(pid));
        std::fs::write(&gate, "").unwrap();
        until("the shell to take over", &|| !process_runs_worker(pid));
        // The same live process becomes the shell (whose script then execs
        // `sleep`: wait for that second exec, not just the first), and is
        // no longer a worker.
        let comm = || std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        until("the shell to exec its sleep", &|| comm().trim() == "sleep");
        assert!(!process_runs_worker(pid));
        // The shell carries the worker's exit status (the sleep it exec'd
        // inherited it).
        assert_eq!(process_worker_exit(pid), Some(0));
        drop(child);
        assert!(!process_runs_worker(pid), "a gone process runs nothing");
    }

    #[test]
    fn a_read_still_running_refuses_its_repeat_until_it_finishes() {
        let gate = Arc::new(ReadGate::default());
        let first = gate.begin("run\0/r\0run-1".into()).expect("free");
        assert!(gate.begin("run\0/r\0run-1".into()).is_none());
        let other = gate.begin("run\0/r\0run-2".into());
        assert!(other.is_some(), "other keys are independent");
        drop(first);
        assert!(gate.begin("run\0/r\0run-1".into()).is_some());
    }

    #[test]
    fn run_ids_cannot_name_other_files() {
        assert!(valid_run_id("run-393c1140eaaf432c879b57e8a08c5c33"));
        assert!(!valid_run_id(""));
        assert!(!valid_run_id("../x"));
        assert!(!valid_run_id("run/1"));
        assert!(!valid_run_id(&"a".repeat(129)));
    }
}
