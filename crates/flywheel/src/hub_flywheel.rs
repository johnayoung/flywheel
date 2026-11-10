//! The hub's Flywheel panel data: [`HubFlywheel`] implements
//! `flywheel_hub::flywheel::FlywheelProvider` over the same code paths the
//! operator console and `fw status` use, for one adopted folder at a time.
//!
//! Why here and not in `flywheel-hub`: the read model (`snapshot`, the
//! console's store resolution, the transcript tailer and its renderer)
//! lives in this crate, which sits above the hub; the hub only defines the
//! seam and plain data, so it never compiles the store backends.
//!
//! Per folder: the folder's home project (never the process's working
//! directory -- one hub serves many repos): a folder whose git repo
//! carries a project id flywheel already knows ([`known_project`]) is
//! shown, resolved exactly as `fw status` there resolves it (store, tasks,
//! and logs under `$FLYWHEEL_HOME/projects/<id>/`, the effective config
//! applied, an untrusted `.flywheel.toml` ignored); the stores it selects,
//! opened once and kept; the work listing behind the console's
//! fingerprint cache. Then:
//!
//! * tasks and alerts: `fw status --json` itself ([`cmd_status`]), so a
//!   strand, stop, or failure reads exactly as the CLI reports it;
//! * in-flight runs: the console's live rows ([`collect_live_rows`]), each
//!   with the latest tool call its transcript records (tailed per run);
//! * a task's history: every lifecycle of the task with each attempt, and
//!   its checks (held-out ones by name only);
//! * recent runs: the newest lifecycles with their latest attempt's grader
//!   receipts from `grader_results`;
//! * plans: every phase under `tasks/active/` and `tasks/archive/`, folded
//!   from every lifecycle of its tasks ([`fold_plans`]);
//! * worker liveness: a live lease in `task_claims` (as the console's
//!   supervisor reads it); autopilot liveness: its activity record;
//! * run output: `fw show <run>` ([`cmd_show`]) plus the console's
//!   redacted transcript ([`TranscriptTailer`]), tailed incrementally.
//!
//! Read-only but for the operator's commands (gate decisions, and `say` /
//! `interrupt` for a live run, enqueued as `control_commands` rows exactly
//! as the CLI verbs do): nothing else writes the store or the task tree
//! (opening a store runs its idempotent schema bootstrap, as every `fw`
//! read verb does). A missing SQLite store is never created: the panel lists the
//! tasks as pending against an empty in-memory store instead.
//!
//! Postgres: like every `fw` verb, the DSN comes from the effective config
//! (`store = "postgres://..."`) or the environment (`FLYWHEEL_PG_DSN` /
//! `DATABASE_URL`) -- here the hub process's. The store label
//! names the database read (`postgres host:port/db`, no credentials) so a
//! folder read from the wrong one shows it. A failed read drops the
//! connection; the next read reconnects.

use crate::session::{EntryKind, TranscriptEntry, TranscriptTailer};
use crate::session_screen::is_error_entry;
use crate::snapshot::{CachedWorkSource, collect_live_rows, directory_fingerprint};
use crate::tui::{open_store_bundle, process_environ};
use chrono::{DateTime, Utc};
use flywheel_core::audit::run_file_path;
use flywheel_core::invoker_client::{CONTROL_COMMAND_INTERRUPT, CONTROL_COMMAND_SAY};
use flywheel_core::lifecycle::{Lifecycle, Status};
use flywheel_core::store::{HarnessStore, LifecycleFilter};
use flywheel_core::store_memory::InMemoryStore;
use flywheel_hub::autopilot::{GateDecided, GateDecision, GateRefused};
use flywheel_hub::fanout::promote::{TaskDraft, TaskFilePlan, TaskSeed, TaskWritten};
use flywheel_hub::flywheel::{
    Alert, AlertKind, AttemptChecks, DaemonState, FlywheelPanel, FlywheelProvider, GraderOutcome,
    LiveRun, Note, PanelClaim, PanelPhase, PanelPlan, PanelTask, PlanTask, RecentRun, RunAttempt,
    RunEntry, RunOutput, Steer, SteerRefused, Steered, TaskHistory, TaskLight, TaskRun, ToolCall,
    UntrustedRepoFile, WorkerScope, valid_run_id,
};
use flywheel_hub::util::path_identity as file_identity;
use flywheel_orchestrator::autopilot_activity::{
    PHASE_STOPPED, epoch_now, is_paused, is_record_live, read_activity, set_paused,
};
use flywheel_orchestrator::claims::{ClaimStore, InMemoryClaimStore, REASON_AWAITING_APPROVAL};
use flywheel_orchestrator::config::Access;
use flywheel_orchestrator::config::{REPO_CONFIG_FILENAME, read_repo_file};
use flywheel_orchestrator::history::{check_to_json, run_attempt_checks};
use flywheel_orchestrator::home::{Home, stored_project_id};
use flywheel_orchestrator::pg_preflight::dsn_target;
use flywheel_orchestrator::policy::WorkPolicy;
use flywheel_orchestrator::sources::{DirectoryWorkSource, WorkSource, phase_task_files};
use flywheel_orchestrator::store_factory::{StoreBundle, resolve_postgres_dsn};
use flywheel_orchestrator::trust;
use flywheel_orchestrator::workflow::status::format_blocked_on;
use flywheel_orchestrator::workflow::{
    CommonArgs, ShowArgs, StatusArgs, cmd_show, cmd_status, known_project, project_policy_at,
    resolve_fallback_phases,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// How many recent runs the panel lists.
pub const RECENT_RUNS: usize = 12;
/// The activity record's file name under the project's
/// `logs/autopilot/` (where `fw autopilot` keeps it).
pub const AUTOPILOT_ACTIVITY: &str = "activity.json";
/// Transcript entries kept per open run (older ones are dropped).
pub const TRANSCRIPT_WINDOW: usize = 5000;
/// The most transcript entries one answer carries (the newest).
pub const TRANSCRIPT_PAGE: usize = 1500;
/// Runs whose transcript tailers are kept warm.
const OPEN_TRANSCRIPTS: usize = 8;
/// A transcript entry's body is cut past this many characters.
const MAX_BODY_CHARS: usize = 8000;

/// The `flywheel` crate's [`FlywheelProvider`] (see the module doc).
pub struct HubFlywheel {
    /// The `fw` binary a started worker runs (`<program> worker`).
    program: PathBuf,
    folders: Mutex<HashMap<PathBuf, Arc<Mutex<Folder>>>>,
    /// Most recently used last.
    transcripts: Mutex<VecDeque<OpenTranscript>>,
    /// Held across a task file's final check and write
    /// (`crate::hub_tasks::write_task`): two promotions never both pass
    /// the "id not defined yet" check.
    task_writes: Mutex<()>,
    /// The flywheel home projects resolve under; `None` reads
    /// `$FLYWHEEL_HOME` (default `~/.flywheel`) on each use.
    home: Option<PathBuf>,
}

/// A run's transcript kept warm between polls.
struct OpenTranscript {
    root: PathBuf,
    run_id: String,
    transcript: Arc<Mutex<Transcript>>,
}

impl std::fmt::Debug for HubFlywheel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubFlywheel")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl HubFlywheel {
    /// A provider whose started workers run `program worker`.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            folders: Mutex::new(HashMap::new()),
            transcripts: Mutex::new(VecDeque::new()),
            task_writes: Mutex::new(()),
            home: None,
        }
    }

    /// The same provider resolving projects under the home at `root`
    /// instead of `$FLYWHEEL_HOME` (tests, embedders).
    pub fn with_home(mut self, root: impl Into<PathBuf>) -> Self {
        self.home = Some(root.into());
        self
    }

    /// The home projects resolve under.
    fn home(&self) -> Result<Home, String> {
        match &self.home {
            Some(root) => Ok(Home::at(root)),
            None => Home::from_env().map_err(|e| e.to_string()),
        }
    }

    /// One whose workers run this very binary.
    pub fn for_current_exe() -> Self {
        Self::new(crate::worker_supervisor::console_program())
    }

    /// How many folders have state kept open (policy, stores, listing).
    pub fn open_folders(&self) -> usize {
        self.folders
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn folder(&self, root: &Path) -> Arc<Mutex<Folder>> {
        let mut folders = self.folders.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            folders
                .entry(root.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(Folder::default()))),
        )
    }

    /// Close `root`'s stores (kept: its policy and listing).
    fn drop_stores(&self, root: &Path) {
        let state = self.folder(root);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(Ok(resolved)) = state.resolved.as_mut() {
            resolved.stores = None;
        }
    }

    fn transcript(&self, root: &Path, run_id: &str, file: PathBuf) -> Arc<Mutex<Transcript>> {
        let mut open = self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = open
            .iter()
            .position(|o| o.root == root && o.run_id == run_id)
        {
            let entry = open.remove(at).expect("position is in range");
            let transcript = Arc::clone(&entry.transcript);
            open.push_back(entry);
            return transcript;
        }
        let transcript = Arc::new(Mutex::new(Transcript::new(file, run_id)));
        open.push_back(OpenTranscript {
            root: root.to_path_buf(),
            run_id: run_id.to_owned(),
            transcript: Arc::clone(&transcript),
        });
        while open.len() > OPEN_TRANSCRIPTS {
            open.pop_front();
        }
        transcript
    }
}

/// What the task writer and reader (`crate::hub_tasks`) need of a folder:
/// its policy resolved the way the panel reads it.
pub(crate) struct TaskLayout {
    /// The directory source's tasks dir (absolute).
    pub tasks_dir: PathBuf,
    /// The project's held-out registry (`<project>/held-out`), when it holds
    /// registrations.
    pub held_out: Option<flywheel_orchestrator::held_out_gate::FilesystemHeldOutGraderSource>,
    /// The `protected_paths` config key: what a landing may not change.
    pub protected_paths: Vec<String>,
}

impl HubFlywheel {
    /// `root`'s tasks dir and held-out registry; refused when its tasks do
    /// not come from a directory source.
    pub(crate) fn task_layout(&self, root: &Path) -> Result<TaskLayout, String> {
        let state = self.folder(root);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let resolved = state.resolve(&self.home(), root)?;
        let tasks_dir = resolved.tasks_dir.clone().ok_or_else(|| {
            format!(
                "tasks come from the {} source in {}: only a directory source takes task \
                 files",
                resolved.source_label,
                root.display()
            )
        })?;
        let held_out =
            flywheel_worktree::daemon::build_held_out_source(resolved.policy.as_ref(), root);
        let protected_paths = resolved
            .policy
            .as_ref()
            .map(|p| p.protected_paths.clone())
            .unwrap_or_default();
        Ok(TaskLayout {
            tasks_dir,
            held_out,
            protected_paths,
        })
    }

    /// The task file write lock (see `task_writes`).
    pub(crate) fn task_write_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.task_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// `root`'s store's runs matching `filter` (none without a store yet).
    fn lifecycles(&self, root: &Path, filter: &LifecycleFilter) -> Result<Vec<Lifecycle>, String> {
        let state = self.folder(root);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let resolved = state.resolve(&self.home(), root)?;
        let core = match resolved.stores(root)? {
            Some(bundle) => Arc::clone(&bundle.core),
            None => return Ok(Vec::new()),
        };
        drop(state);
        core.list_lifecycles(filter)
            .map_err(|e| format!("reading the store: {e}"))
    }

    /// How many runs `root`'s store holds for `task_id`.
    pub(crate) fn task_runs(&self, root: &Path, task_id: &str) -> Result<usize, String> {
        let filter = LifecycleFilter {
            task_id: Some(task_id.to_owned()),
            ..LifecycleFilter::default()
        };
        Ok(self.lifecycles(root, &filter)?.len())
    }

    /// The task ids with a done run in `root`'s store.
    pub(crate) fn done_tasks(&self, root: &Path) -> Result<HashSet<String>, String> {
        let filter = LifecycleFilter::default().with_statuses([Status::Done]);
        Ok(self
            .lifecycles(root, &filter)?
            .into_iter()
            .map(|l| l.task_id)
            .collect())
    }
}

/// A file's (size, mtime) for change detection; `None` when absent.
fn stamp(path: &Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Some((meta.len(), modified))
}

/// `path` relative to `root` for display, else as is.
fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// What one folder's project resolves to, plus the stores and listing
/// kept open across reads.
#[derive(Default)]
struct Folder {
    /// The config stamps `resolved` was read under.
    config_stamp: Option<u64>,
    resolved: Option<Result<Resolved, String>>,
    /// The main checkout when the folder is a linked git worktree; read
    /// once (`Some(None)`: not one).
    worktree_of: Option<Option<PathBuf>>,
    /// The in-flight runs' transcripts, tailed for their latest tool call
    /// (kept to the runs the last panel read found in flight).
    tool_tails: HashMap<String, ToolTail>,
}

struct Resolved {
    policy: Option<WorkPolicy>,
    db_path: PathBuf,
    backend: String,
    /// The directory source's tasks dir (absolute).
    tasks_dir: Option<PathBuf>,
    /// `None` for tracker sources (their listing is a network call).
    source: Option<Box<dyn WorkSource>>,
    source_label: String,
    /// The project's autopilot activity record.
    activity: PathBuf,
    stores: Option<OpenStores>,
}

struct OpenStores {
    bundle: StoreBundle,
    /// The SQLite file's identity when opened: a replaced file reopens.
    identity: Option<(u64, u64)>,
}

/// A hash of every file the folder's effective config is read from (the
/// repo file, the user config, the project's personal config).
fn config_stamp(home: &Home, folder: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    stamp(&folder.join(REPO_CONFIG_FILENAME)).hash(&mut hasher);
    stamp(&home.config_file()).hash(&mut hasher);
    if let Some(id) = known_project(home, folder) {
        stamp(&home.project(&id).config()).hash(&mut hasher);
    }
    hasher.finish()
}

impl Folder {
    fn worktree_of(&mut self, root: &Path) -> Option<PathBuf> {
        self.worktree_of
            .get_or_insert_with(|| flywheel_hub::git::linked_worktree_main(root))
            .clone()
    }

    /// Re-resolve the project when its config changed (or on first use).
    fn resolve(
        &mut self,
        home: &Result<Home, String>,
        root: &Path,
    ) -> Result<&mut Resolved, String> {
        let home = home.as_ref().map_err(Clone::clone)?;
        let current = config_stamp(home, root);
        if self.config_stamp != Some(current) || self.resolved.is_none() {
            self.config_stamp = Some(current);
            self.resolved = Some(Resolved::read(home, root));
        }
        match self.resolved.as_mut().expect("just set") {
            Ok(resolved) => Ok(resolved),
            Err(message) => Err(message.clone()),
        }
    }

    /// The project's autopilot activity record, when the folder resolves.
    fn activity(&mut self, home: &Result<Home, String>, root: &Path) -> Option<PathBuf> {
        self.resolve(home, root).ok().map(|r| r.activity.clone())
    }
}

impl Resolved {
    fn read(home: &Home, root: &Path) -> Result<Self, String> {
        let (policy, _warnings) =
            project_policy_at(home, root, Access::ReadOnly).map_err(|e| e.to_string())?;
        let project = policy
            .project
            .clone()
            .ok_or_else(|| format!("{} resolves to no project", root.display()))?;
        let paths = project.paths();
        let db_path = policy.db_path.clone().unwrap_or_else(|| paths.db());
        let backend = policy.store_backend.clone();
        let (tasks_dir, source, source_label) = if policy.source_kind == "directory" {
            let dir = policy.tasks_dir.clone().unwrap_or_else(|| paths.tasks());
            let source: Box<dyn WorkSource> = Box::new(CachedWorkSource::for_directory(
                Box::new(DirectoryWorkSource::new(&dir)),
                dir.clone(),
            ));
            let label = format!("directory {}", dir.display());
            (Some(dir), Some(source), label)
        } else {
            (None, None, policy.source_kind.clone())
        };
        Ok(Self {
            policy: Some(policy),
            db_path,
            backend,
            tasks_dir,
            source,
            source_label,
            activity: paths.autopilot_logs().join(AUTOPILOT_ACTIVITY),
            stores: None,
        })
    }

    fn store_label(&self, root: &Path) -> String {
        if self.backend == "sqlite" {
            return format!("sqlite {}", shown(root, &self.db_path));
        }
        let mut label = self.backend.clone();
        if let Some(target) = resolve_postgres_dsn(&process_environ())
            .as_deref()
            .and_then(dsn_target)
        {
            label.push(' ');
            label.push_str(&target);
        }
        label
    }

    /// The policy's stores, opened on first use and kept; `Ok(None)` when
    /// the SQLite file does not exist yet (it is never created here).
    fn stores(&mut self, root: &Path) -> Result<Option<&StoreBundle>, String> {
        let sqlite = self.backend == "sqlite";
        let identity = if sqlite {
            match file_identity(&self.db_path) {
                Some(identity) => Some(identity),
                None => {
                    self.stores = None;
                    return Ok(None);
                }
            }
        } else {
            None
        };
        if self.stores.as_ref().is_some_and(|s| s.identity != identity) {
            self.stores = None;
        }
        if self.stores.is_none() {
            if !sqlite
                && self
                    .policy
                    .as_ref()
                    .and_then(|p| p.store_dsn.as_ref())
                    .is_none()
                && resolve_postgres_dsn(&process_environ()).is_none()
            {
                return Err(format!(
                    "the store backend is {} but no DSN is configured: set `store` in \
                     $FLYWHEEL_HOME/config.toml, or start `fw hub serve` from a shell that \
                     exports FLYWHEEL_PG_DSN",
                    self.backend
                ));
            }
            let bundle = open_store_bundle(self.policy.as_ref(), &self.db_path)
                .map_err(|e| format!("opening {}: {e}", self.store_label(root)))?;
            self.stores = Some(OpenStores { bundle, identity });
        }
        Ok(self.stores.as_ref().map(|s| &s.bundle))
    }
}

// --- fw status --json ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StatusEntry {
    task_id: Option<String>,
    subject: Option<String>,
    task_file: Option<String>,
    #[serde(default)]
    source_ref: String,
    state: Option<String>,
    latest_run_id: Option<String>,
    latest_status: Option<String>,
    #[serde(default)]
    latest_error: String,
    #[serde(default)]
    prerequisites: Vec<String>,
    worker_id: Option<String>,
    blocked_requires: Option<Vec<Map<String, Value>>>,
    awaiting_on: Option<AwaitingOn>,
    stranded: Option<Parked>,
    stopped: Option<Stopped>,
}

#[derive(Debug, Deserialize)]
struct AwaitingOn {
    instruction: String,
}

#[derive(Debug, Deserialize)]
struct Parked {
    park_kind: String,
    #[serde(default)]
    detail: String,
}

#[derive(Debug, Deserialize)]
struct Stopped {
    kind: String,
    #[serde(default)]
    detail: String,
}

/// `fw status --json` against these stores, parsed.
fn status_entries(
    items: &[flywheel_orchestrator::sources::WorkItem],
    core: &dyn HarnessStore,
    claims: &dyn ClaimStore,
    root: &Path,
) -> Result<Vec<StatusEntry>, String> {
    let args = StatusArgs {
        common: CommonArgs::default(),
        json: true,
        rollup: false,
    };
    let mut out: Vec<u8> = Vec::new();
    cmd_status(&args, items, core, claims, Some(root), None, &mut out)
        .map_err(|e| format!("reading task status: {e}"))?;
    serde_json::from_slice(&out).map_err(|e| format!("reading task status: {e}"))
}

fn light(state: &str, stranded: bool) -> TaskLight {
    match state {
        "done" if stranded => TaskLight::Parked,
        "done" => TaskLight::Verified,
        "in_progress" => TaskLight::Running,
        "retryable" => TaskLight::Failed,
        "interrupted" => TaskLight::Blocked,
        "awaiting_approval" => TaskLight::Awaiting,
        _ => TaskLight::Pending,
    }
}

fn joined(kind: &str, detail: &str) -> String {
    if detail.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind} -- {detail}")
    }
}

/// The phase a status entry renders under: its task file's directory,
/// else its source ref, else `external` (as `fw status` labels it).
fn phase_of(entry: &StatusEntry) -> String {
    let from_file = entry
        .task_file
        .as_deref()
        .filter(|f| *f != ".")
        .map(Path::new)
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty());
    from_file
        .or_else(|| (!entry.source_ref.is_empty()).then(|| entry.source_ref.clone()))
        .unwrap_or_else(|| "external".to_owned())
}

/// Fold the status entries into phases and alerts.
fn fold_status(entries: Vec<StatusEntry>, panel: &mut FlywheelPanel) {
    let mut phases: BTreeMap<String, Vec<PanelTask>> = BTreeMap::new();
    for entry in entries {
        let Some(task_id) = entry.task_id.clone() else {
            // A subject with no active row: a source-level stop or a
            // strand whose task left the listing.
            let subject = entry.subject.clone().unwrap_or_default();
            if let Some(stop) = &entry.stopped {
                panel.alerts.push(Alert {
                    kind: AlertKind::Stopped,
                    subject: subject.clone(),
                    detail: joined(&stop.kind, &stop.detail),
                    run_id: None,
                });
            }
            if let Some(park) = &entry.stranded {
                panel.alerts.push(Alert {
                    kind: AlertKind::Stranded,
                    subject,
                    detail: joined(&park.park_kind, &park.detail),
                    run_id: None,
                });
            }
            continue;
        };
        let state = entry.state.clone().unwrap_or_default();
        // A human-gate stop only mirrors a parked gate: while the gate is
        // parked the awaiting alert (with approve / deny) says it, and once
        // decided the stop is history. `fw status` keeps it until `fw
        // resolve`; the hub does not repeat it.
        let stopped = entry
            .stopped
            .as_ref()
            .filter(|s| s.kind != REASON_AWAITING_APPROVAL);
        let task = PanelTask {
            id: task_id.clone(),
            state: light(&state, entry.stranded.is_some()),
            flywheel_state: state.clone(),
            run_id: entry.latest_run_id.clone(),
            run_status: entry.latest_status.clone(),
            error: entry.latest_error.clone(),
            worker_id: entry.worker_id.clone(),
            prerequisites: entry.prerequisites.clone(),
            stranded: entry.stranded.as_ref().map(|p| Note {
                kind: p.park_kind.clone(),
                detail: p.detail.clone(),
            }),
            stopped: stopped.map(|s| Note {
                kind: s.kind.clone(),
                detail: s.detail.clone(),
            }),
            awaiting: entry.awaiting_on.as_ref().map(|a| a.instruction.clone()),
            blocked_on: entry
                .blocked_requires
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(format_blocked_on),
            // The hub's to fill in (its running worker sessions).
            queued_in: None,
        };
        let mut alert = |kind: AlertKind, detail: String| {
            panel.alerts.push(Alert {
                kind,
                subject: task_id.clone(),
                detail,
                run_id: entry.latest_run_id.clone(),
            });
        };
        match task.state {
            TaskLight::Failed => alert(
                AlertKind::Failed,
                if entry.latest_error.is_empty() {
                    format!(
                        "latest run {}",
                        entry.latest_status.as_deref().unwrap_or("failed")
                    )
                } else {
                    entry.latest_error.clone()
                },
            ),
            TaskLight::Awaiting => alert(
                AlertKind::Awaiting,
                task.awaiting
                    .clone()
                    .unwrap_or_else(|| "awaiting approval".into()),
            ),
            TaskLight::Blocked => alert(
                AlertKind::Blocked,
                task.blocked_on
                    .clone()
                    .map(|b| format!("blocked on {b}"))
                    .unwrap_or_else(|| joined("interrupted", &entry.latest_error)),
            ),
            _ => {}
        }
        if let Some(park) = &entry.stranded {
            alert(AlertKind::Stranded, joined(&park.park_kind, &park.detail));
        }
        if let Some(stop) = stopped {
            // The run's own error says why (an abort's reason: "already
            // present on the base"); the stop's detail opens with the run
            // id, all a narrow card would show of it.
            let why = if entry.latest_error.is_empty() {
                &stop.detail
            } else {
                &entry.latest_error
            };
            alert(AlertKind::Stopped, joined(&stop.kind, why));
        }
        phases.entry(phase_of(&entry)).or_default().push(task);
    }
    panel.phases = phases
        .into_iter()
        .map(|(name, tasks)| PanelPhase { name, tasks })
        .collect();
}

// --- plans -------------------------------------------------------------------

/// The window `verified_24h` counts verifying runs in.
const VERIFIED_WINDOW_HOURS: i64 = 24;

/// One plan's task ids, as its task files define them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanFiles {
    name: String,
    archived: bool,
    tasks: BTreeSet<String>,
}

/// The id a task file defines: its `id` field, else its file stem (the
/// rule `task_id_defined` applies).
fn file_task_id(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|data| data.get("id").and_then(Value::as_str).map(str::to_owned))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

/// Every phase directory under `tasks_dir`'s `active/` and `archive/` with
/// its task files' ids (the directory source's file rule); a phase in both
/// is one plan, active.
fn plan_files(tasks_dir: &Path) -> Result<Vec<PlanFiles>, String> {
    let mut plans: BTreeMap<String, PlanFiles> = BTreeMap::new();
    for bucket in ["active", "archive"] {
        let root = tasks_dir.join(bucket);
        if !root.is_dir() {
            continue;
        }
        let entries =
            std::fs::read_dir(&root).map_err(|e| format!("listing {}: {e}", root.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("listing {}: {e}", root.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();
            if name.starts_with('.') || !dir.is_dir() {
                continue;
            }
            let files = phase_task_files(&dir).map_err(|e| format!("listing tasks: {e}"))?;
            let plan = plans.entry(name.clone()).or_insert_with(|| PlanFiles {
                name,
                archived: true,
                tasks: BTreeSet::new(),
            });
            plan.archived &= bucket == "archive";
            plan.tasks.extend(files.iter().map(|f| file_task_id(f)));
        }
    }
    Ok(plans.into_values().collect())
}

/// The plans of a source with no task tree: its listed phases.
fn listed_plans(phases: &[PanelPhase]) -> Vec<PlanFiles> {
    phases
        .iter()
        .map(|p| PlanFiles {
            name: p.name.clone(),
            archived: false,
            tasks: p.tasks.iter().map(|t| t.id.clone()).collect(),
        })
        .collect()
}

/// The light of a task the listing does not carry (an archived one), from
/// its runs newest first: what `fw status` would classify it as, short of
/// a landing park (archived work has landed).
fn unlisted_light(runs: &[&Lifecycle]) -> TaskLight {
    let Some(latest) = runs.first() else {
        return TaskLight::Pending;
    };
    if runs.iter().any(|lc| lc.status == Status::Done) {
        return TaskLight::Verified;
    }
    match latest.status {
        Status::Failed | Status::FailedValidation | Status::InternalError => TaskLight::Failed,
        Status::AwaitingApproval => TaskLight::Awaiting,
        Status::Interrupted => TaskLight::Blocked,
        Status::Pending | Status::Ready | Status::Running | Status::Validating | Status::Done => {
            TaskLight::Running
        }
    }
}

/// When a DONE run ended: its DONE transition, else its last attempt's end.
fn verified_at(lc: &Lifecycle) -> Option<DateTime<Utc>> {
    lc.timestamps
        .get(&Status::Done)
        .copied()
        .or_else(|| lc.attempts.last().and_then(|a| a.ended_at))
}

/// Fold `lifecycles` (every run of the plans' tasks, newest first, as the
/// store lists them) into the plans' progress. `lights`: the listed tasks'
/// panel states (an unlisted task is classified from its runs).
fn fold_plans(
    files: Vec<PlanFiles>,
    lifecycles: &[Lifecycle],
    lights: &HashMap<&str, TaskLight>,
    now: DateTime<Utc>,
) -> Vec<PanelPlan> {
    let mut runs: HashMap<&str, Vec<&Lifecycle>> = HashMap::new();
    for lc in lifecycles {
        runs.entry(lc.task_id.as_str()).or_default().push(lc);
    }
    let since = now - chrono::Duration::hours(VERIFIED_WINDOW_HOURS);
    let mut plans: Vec<PanelPlan> = files
        .into_iter()
        .map(|plan| {
            let mut out = PanelPlan {
                name: plan.name,
                archived: plan.archived,
                total: plan.tasks.len(),
                verified: 0,
                spend_usd: 0.0,
                verified_24h: 0,
                needs: 0,
                running: 0,
                tasks: Vec::new(),
            };
            for task in &plan.tasks {
                let task_runs = runs.get(task.as_str()).map_or(&[][..], Vec::as_slice);
                out.spend_usd += task_runs
                    .iter()
                    .flat_map(|lc| &lc.attempts)
                    .map(|a| a.total_cost_usd)
                    .sum::<f64>();
                if let Some(latest) = task_runs.first()
                    && latest.status == Status::Done
                {
                    out.verified += 1;
                    if verified_at(latest).is_some_and(|at| at > since && at <= now) {
                        out.verified_24h += 1;
                    }
                }
                let light = lights
                    .get(task.as_str())
                    .copied()
                    .unwrap_or_else(|| unlisted_light(task_runs));
                // An interrupted run waits for a person to release it: it needs
                // someone, even though the panel's light reads it as blocked.
                let interrupted = task_runs
                    .first()
                    .is_some_and(|lc| lc.status == Status::Interrupted);
                match light {
                    TaskLight::Failed | TaskLight::Parked | TaskLight::Awaiting => out.needs += 1,
                    _ if interrupted => out.needs += 1,
                    TaskLight::Running => out.running += 1,
                    _ => {}
                }
                out.tasks.push(PlanTask {
                    id: task.clone(),
                    state: light,
                    status: task_runs.first().map(|lc| lc.status.as_str().to_owned()),
                });
            }
            out
        })
        .collect();
    plans.sort_by(|a, b| (a.needs == 0, &a.name).cmp(&(b.needs == 0, &b.name)));
    plans
}

/// The panel's plans: the task tree's phases (else the listed ones) folded
/// from every run of their tasks in `core`. What cannot be read is pushed
/// to `problems`; the plans are still listed from their files.
fn read_plans(
    tasks_dir: Option<&Path>,
    core: &dyn HarnessStore,
    panel: &FlywheelPanel,
    problems: &mut Vec<String>,
) -> Vec<PanelPlan> {
    let files = match tasks_dir.map(plan_files) {
        Some(Ok(files)) => files,
        Some(Err(message)) => {
            problems.push(message);
            return Vec::new();
        }
        None => listed_plans(&panel.phases),
    };
    let ids: BTreeSet<&str> = files
        .iter()
        .flat_map(|p| &p.tasks)
        .map(String::as_str)
        .collect();
    let lifecycles = if ids.is_empty() {
        Vec::new()
    } else {
        core.list_lifecycles(&LifecycleFilter::default().with_task_ids(ids))
            .unwrap_or_else(|e| {
                problems.push(format!("reading plan runs: {e}"));
                Vec::new()
            })
    };
    let lights: HashMap<&str, TaskLight> = panel
        .phases
        .iter()
        .flat_map(|p| &p.tasks)
        .map(|t| (t.id.as_str(), t.state))
        .collect();
    fold_plans(files, &lifecycles, &lights, panel.generated_at)
}

// --- runs, claims, daemons ---------------------------------------------------

fn elapsed(now: DateTime<Utc>, since: DateTime<Utc>) -> i64 {
    (now - since).num_seconds().max(0)
}

fn live_runs(core: &dyn HarnessStore, now: DateTime<Utc>) -> Result<Vec<LiveRun>, String> {
    let rows = collect_live_rows(core).map_err(|e| format!("reading in-flight runs: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|row| LiveRun {
            age_seconds: row.started_at.map(|t| elapsed(now, t)),
            idle_seconds: row.last_ts.map(|t| elapsed(now, t)),
            status: row.status.as_str().to_owned(),
            run_id: row.run_id,
            task_id: row.task_id,
            attempt: row.attempt,
            iteration: row.iteration,
            tokens: row.tokens_total,
            cost_usd: row.cost_usd_total,
            last_detail: row.last_detail,
            worker_id: row.worker_id,
            awaiting: row.awaiting_instruction,
            last_tool: None,
        })
        .collect())
}

/// One attempt's grader receipts, by name (never a grader's command).
fn grader_outcomes(
    core: &dyn HarnessStore,
    run_id: &str,
    attempt: u32,
) -> Result<Vec<GraderOutcome>, String> {
    Ok(core
        .list_grader_results(run_id, attempt)
        .map_err(|e| format!("reading grader receipts: {e}"))?
        .into_iter()
        .map(|r| GraderOutcome {
            name: r
                .grader_name
                .clone()
                .or_else(|| {
                    r.grader_spec
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| format!("#{}", r.ordinal)),
            kind: r.grader_type.as_str().to_owned(),
            passed: r.passed,
            duration_ms: r.duration_ms,
        })
        .collect())
}

fn recent_run(core: &dyn HarnessStore, lc: &Lifecycle) -> Result<RecentRun, String> {
    let latest = lc.attempts.last();
    let graders = match latest {
        Some(attempt) => grader_outcomes(core, &lc.run_id, attempt.number)?,
        None => Vec::new(),
    };
    Ok(RecentRun {
        run_id: lc.run_id.clone(),
        task_id: lc.task_id.clone(),
        status: lc.status.as_str().to_owned(),
        outcome: latest
            .and_then(|a| a.outcome)
            .map(|o| o.as_str().to_owned()),
        attempts: lc.attempts.len() as u32,
        started_at: lc.timestamps.values().min().copied(),
        updated_at: lc.timestamps.values().max().copied(),
        tokens: lc.attempts.iter().map(|a| a.total_tokens()).sum(),
        cost_usd: lc
            .attempts
            .iter()
            .fold(0.0, |total, a| total + a.total_cost_usd),
        error: lc.error.clone(),
        graders,
    })
}

/// Every run of `task_id` (newest first, never windowed) with each
/// attempt's outcome, error, and grader receipts.
fn task_runs(core: &dyn HarnessStore, task_id: &str) -> Result<Vec<TaskRun>, String> {
    let lifecycles = core
        .list_lifecycles(&LifecycleFilter::default().with_task_id(task_id))
        .map_err(|e| format!("reading runs: {e}"))?;
    lifecycles
        .iter()
        .map(|lc| {
            let mut checks = attempt_checks(core, lc)?.into_iter();
            let attempts = lc
                .attempts
                .iter()
                .map(|a| {
                    Ok(RunAttempt {
                        number: a.number,
                        outcome: a.outcome.map(|o| o.as_str().to_owned()),
                        error: a.error.clone(),
                        stop_reason: a
                            .stop_reason
                            .as_ref()
                            .map(serde_json::to_value)
                            .transpose()
                            .map_err(|e| format!("encoding a stop reason: {e}"))?,
                        started_at: a.started_at,
                        ended_at: a.ended_at,
                        tokens: a.total_tokens(),
                        cost_usd: a.total_cost_usd,
                        graders: grader_outcomes(core, &lc.run_id, a.number)?,
                        checks: checks.next().map(|c| c.checks).unwrap_or_default(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(TaskRun {
                run_id: lc.run_id.clone(),
                status: lc.status.as_str().to_owned(),
                started_at: lc.timestamps.values().min().copied(),
                updated_at: lc.timestamps.values().max().copied(),
                error: lc.error.clone(),
                tokens: attempts.iter().map(|a| a.tokens).sum(),
                cost_usd: attempts.iter().fold(0.0, |total, a| total + a.cost_usd),
                attempts,
            })
        })
        .collect()
}

/// Every attempt's `checks` of one run, from the read model `fw show
/// --json` renders, as its JSON.
fn attempt_checks(core: &dyn HarnessStore, lc: &Lifecycle) -> Result<Vec<AttemptChecks>, String> {
    Ok(run_attempt_checks(core, lc)
        .map_err(|e| format!("reading checks of run {}: {e}", lc.run_id))?
        .into_iter()
        .map(|(number, checks)| AttemptChecks {
            number,
            checks: checks.iter().map(check_to_json).collect(),
        })
        .collect())
}

fn recent_runs(core: &dyn HarnessStore) -> Result<Vec<RecentRun>, String> {
    // Newest first: the store orders by (updated_at DESC, run_id DESC) and
    // applies the limit itself, so only the shown runs are folded.
    let lifecycles = core
        .list_lifecycles(&LifecycleFilter::default().with_limit(RECENT_RUNS))
        .map_err(|e| format!("reading runs: {e}"))?;
    lifecycles.iter().map(|lc| recent_run(core, lc)).collect()
}

fn claims_and_worker(
    claims: &dyn ClaimStore,
    now: DateTime<Utc>,
) -> Result<(Vec<PanelClaim>, DaemonState), String> {
    let mut rows: Vec<PanelClaim> = claims
        .list_claims()
        .map_err(|e| format!("reading claims: {e}"))?
        .into_iter()
        .map(|c| PanelClaim {
            live: c.lease_expires_at > now,
            task_id: c.task_id,
            worker_id: c.worker_id,
            claimed_at: c.claimed_at,
            lease_expires_at: c.lease_expires_at,
        })
        .collect();
    rows.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    let live: Vec<&PanelClaim> = rows.iter().filter(|c| c.live).collect();
    let worker = if live.is_empty() {
        DaemonState {
            live: false,
            detail: "no live lease (no worker is holding a task)".into(),
        }
    } else {
        let mut workers: Vec<&str> = live.iter().map(|c| c.worker_id.as_str()).collect();
        workers.sort_unstable();
        workers.dedup();
        DaemonState {
            live: true,
            detail: format!(
                "{} live lease{} held by {}",
                live.len(),
                if live.len() == 1 { "" } else { "s" },
                workers.join(", ")
            ),
        }
    };
    Ok((rows, worker))
}

/// The daemon's liveness for the panel's row; `running`: its pid still
/// runs (false once the intake read found it gone).
fn autopilot_state(path: Option<&Path>, running: bool) -> DaemonState {
    let Some(path) = path else {
        return DaemonState {
            live: false,
            detail: "no activity record (not running here)".into(),
        };
    };
    let Some(activity) = read_activity(Some(path)) else {
        return DaemonState {
            live: false,
            detail: "no activity record (not running here)".into(),
        };
    };
    let now = epoch_now();
    let held = if is_paused(path) { ", paused" } else { "" };
    if running && activity.phase != PHASE_STOPPED && is_record_live(Some(&activity), now) {
        DaemonState {
            live: true,
            detail: format!(
                "pid {}, {} (cycle {}){held}",
                activity.pid, activity.phase, activity.cycle_index
            ),
        }
    } else {
        let ago = (now - activity.updated_at).max(0.0) as i64;
        DaemonState {
            live: false,
            detail: format!(
                "stopped (last seen {} ago, pid {}){held}",
                human_seconds(ago),
                activity.pid
            ),
        }
    }
}

fn human_seconds(seconds: i64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

// --- transcripts ---------------------------------------------------------------

/// A transcript entry's tool call: `tool(<name>)` and its argument.
fn tool_call(entry: &TranscriptEntry) -> Option<ToolCall> {
    if entry.kind != EntryKind::ToolCall {
        return None;
    }
    let tool = entry
        .header
        .strip_prefix("tool(")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(&entry.header);
    Some(ToolCall {
        tool: tool.to_owned(),
        argument: cut(entry.body.clone()),
        ts: entry.ts,
    })
}

/// An in-flight run's transcript tailed only for its latest tool call.
struct ToolTail {
    tailer: TranscriptTailer,
    last: Option<ToolCall>,
}

impl ToolTail {
    fn new(file: PathBuf, run_id: &str) -> Self {
        Self {
            tailer: TranscriptTailer::new(file, run_id),
            last: None,
        }
    }

    /// The latest tool call, after reading what was appended since the
    /// last pull. A missing transcript reads as none; a failed read keeps
    /// the last one seen.
    fn pull(&mut self) -> Option<ToolCall> {
        if let Ok(fresh) = self.tailer.fetch()
            && let Some(call) = fresh.iter().rev().find_map(tool_call)
        {
            self.last = Some(call);
        }
        self.last.clone()
    }
}

/// Fill in each live run's latest tool call from its transcript under
/// `logs_root`, keeping `tails` to exactly those runs.
fn mark_last_tools(tails: &mut HashMap<String, ToolTail>, logs_root: &Path, live: &mut [LiveRun]) {
    tails.retain(|run_id, _| live.iter().any(|run| &run.run_id == run_id));
    for run in live {
        if !valid_run_id(&run.run_id) {
            continue;
        }
        let tail = tails
            .entry(run.run_id.clone())
            .or_insert_with(|| ToolTail::new(run_file_path(logs_root, &run.run_id), &run.run_id));
        run.last_tool = tail.pull();
    }
}

/// The console's logs root: `<store dir>/logs`.
fn logs_root(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf)
        .join("logs")
}

/// One run's transcript, tailed incrementally and windowed.
struct Transcript {
    tailer: TranscriptTailer,
    entries: VecDeque<RunEntry>,
    /// The index of `entries[0]` in the whole transcript.
    base: usize,
}

impl Transcript {
    fn new(file: PathBuf, run_id: &str) -> Self {
        Self {
            tailer: TranscriptTailer::new(file, run_id),
            entries: VecDeque::new(),
            base: 0,
        }
    }

    fn total(&self) -> usize {
        self.base + self.entries.len()
    }

    fn pull(&mut self) -> Result<(), String> {
        let fresh = self
            .tailer
            .fetch()
            .map_err(|e| format!("reading the transcript: {e}"))?;
        for entry in fresh {
            self.entries.push_back(RunEntry {
                ts: entry.ts,
                kind: entry.kind.as_str().to_owned(),
                error: is_error_entry(&entry),
                header: entry.header,
                body: cut(entry.body),
            });
        }
        while self.entries.len() > TRANSCRIPT_WINDOW {
            self.entries.pop_front();
            self.base += 1;
        }
        Ok(())
    }

    /// Entries from `since` on (at most the newest [`TRANSCRIPT_PAGE`]),
    /// with the index of the first one.
    fn from(&self, since: usize) -> (usize, Vec<RunEntry>) {
        let total = self.total();
        let from = since
            .max(self.base)
            .max(total.saturating_sub(TRANSCRIPT_PAGE))
            .min(total);
        let entries = self
            .entries
            .iter()
            .skip(from - self.base)
            .cloned()
            .collect();
        (from, entries)
    }
}

fn cut(body: String) -> String {
    let chars = body.chars().count();
    if chars <= MAX_BODY_CHARS {
        return body;
    }
    let mut kept: String = body.chars().take(MAX_BODY_CHARS).collect();
    kept.push_str(&format!(
        "\n... ({} more characters)",
        chars - MAX_BODY_CHARS
    ));
    kept
}

// --- the provider ----------------------------------------------------------------

/// The folder's `.flywheel.toml` trust state: `Ok(Some)` while it awaits
/// `fw trust` (keyed, like every verb, by the repo's local project id and
/// the file's sha256), `Err` when the file does not load. Never mints an
/// id or writes the home.
fn repo_file_trust(
    home: Result<Home, String>,
    folder: &Path,
) -> Result<Option<UntrustedRepoFile>, String> {
    let repo = match read_repo_file(folder) {
        Ok(Some(repo)) => repo,
        Ok(None) => return Ok(None),
        Err(failure) => return Err(failure.to_string()),
    };
    let trusted = match (home, stored_project_id(folder)) {
        (Ok(home), Ok(Some(id))) => {
            trust::is_trusted(&home, &id, &repo.sha256).map_err(|e| e.to_string())?
        }
        (Err(failure), _) => return Err(failure),
        (_, Err(failure)) => return Err(failure.to_string()),
        (Ok(_), Ok(None)) => false,
    };
    Ok((!trusted).then(|| UntrustedRepoFile {
        file: repo.path,
        command: format!("cd {} && fw trust", folder.display()),
    }))
}

impl FlywheelProvider for HubFlywheel {
    fn adopted(&self, folder: &Path) -> bool {
        // A git repo flywheel already knows (its project id is recorded and
        // its project dir exists under $FLYWHEEL_HOME); a linked worktree
        // maps to its repo's project through the shared git dir, and its
        // panel says so (`worktree_of`). Nothing in the repo tree marks it.
        self.home()
            .is_ok_and(|home| known_project(&home, folder).is_some())
    }

    fn fingerprint(&self, folder: &Path) -> u64 {
        let mut hasher = DefaultHasher::new();
        if let Ok(home) = self.home() {
            config_stamp(&home, folder).hash(&mut hasher);
            // Trust: a new approval re-reads the panel.
            stamp(&home.trust_file()).hash(&mut hasher);
        }
        let state = self.folder(folder);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(resolved) = state.resolve(&self.home(), folder) {
            stamp(&resolved.activity).hash(&mut hasher);
            if resolved.backend == "sqlite" {
                let db = &resolved.db_path;
                stamp(db).hash(&mut hasher);
                let mut wal = db.clone().into_os_string();
                wal.push("-wal");
                stamp(Path::new(&wal)).hash(&mut hasher);
            }
            if let Some(dir) = &resolved.tasks_dir {
                directory_fingerprint(dir).hash(&mut hasher);
            }
        }
        // A tool call appended to an in-flight run's transcript.
        let mut tails: Vec<(&String, &ToolTail)> = state.tool_tails.iter().collect();
        tails.sort_by(|a, b| a.0.cmp(b.0));
        for (_, tail) in tails {
            stamp(tail.tailer.run_file()).hash(&mut hasher);
        }
        hasher.finish()
    }

    fn panel(&self, folder: &Path) -> FlywheelPanel {
        let now = Utc::now();
        let mut panel = FlywheelPanel::empty(folder.to_path_buf(), now);
        let mut trust_problem = None;
        match repo_file_trust(self.home(), folder) {
            Ok(untrusted) => panel.untrusted = untrusted,
            Err(message) => trust_problem = Some(message),
        }
        let state = self.folder(folder);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        panel.worktree_of = state.worktree_of(folder);
        let activity = state.activity(&self.home(), folder);
        panel.autopilot = autopilot_state(activity.as_deref(), true);
        let resolved = match state.resolve(&self.home(), folder) {
            Ok(resolved) => resolved,
            Err(message) => {
                panel.problem = Some(message);
                return panel;
            }
        };
        panel.source = resolved.source_label.clone();
        panel.store = resolved.store_label(folder);
        let mut problems: Vec<String> = trust_problem.into_iter().collect();
        let (core, claims): (Arc<dyn HarnessStore>, Arc<dyn ClaimStore>) =
            match resolved.stores(folder) {
                Ok(Some(bundle)) => (Arc::clone(&bundle.core), Arc::clone(&bundle.claims)),
                Ok(None) => {
                    problems.push(format!(
                        "no store yet at {} (the first worker run creates it)",
                        shown(folder, &resolved.db_path)
                    ));
                    (
                        Arc::new(InMemoryStore::new()),
                        Arc::new(InMemoryClaimStore::new()),
                    )
                }
                Err(message) => {
                    problems.push(message);
                    (
                        Arc::new(InMemoryStore::new()),
                        Arc::new(InMemoryClaimStore::new()),
                    )
                }
            };
        let policy = resolved.policy.clone();
        let tasks_dir = resolved.tasks_dir.clone();
        let db_path = resolved.db_path.clone();
        let items = match &resolved.source {
            Some(source) => match source.list_work() {
                Ok(items) => items,
                Err(e) => {
                    problems.push(format!("listing tasks: {e}"));
                    Vec::new()
                }
            },
            None => {
                panel.tasks_note = Some(format!(
                    "Tasks come from the {} source (not listed here); runs, claims, and \
                     alerts are read from the store.",
                    resolved.source_label
                ));
                Vec::new()
            }
        };
        drop(state);
        let opened = problems.len();
        match status_entries(&items, core.as_ref(), claims.as_ref(), folder) {
            Ok(entries) => fold_status(entries, &mut panel),
            Err(message) => problems.push(message),
        }
        match live_runs(core.as_ref(), now) {
            Ok(live) => panel.live = live,
            Err(message) => problems.push(message),
        }
        {
            let state = self.folder(folder);
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            mark_last_tools(&mut state.tool_tails, &logs_root(&db_path), &mut panel.live);
        }
        match recent_runs(core.as_ref()) {
            Ok(runs) => panel.runs = runs,
            Err(message) => problems.push(message),
        }
        panel.plans = read_plans(tasks_dir.as_deref(), core.as_ref(), &panel, &mut problems);
        match claims_and_worker(claims.as_ref(), now) {
            Ok((rows, worker)) => {
                panel.claims = rows;
                panel.worker = worker;
            }
            Err(message) => problems.push(message),
        }
        let intake = crate::hub_autopilot::read_view(
            activity.as_deref(),
            policy.as_ref(),
            tasks_dir.as_deref(),
            core.as_ref(),
            &panel,
        );
        // The record lapses with time, but a daemon that died (or was
        // stopped) on this machine is gone now: the panel says so at once.
        if panel.autopilot.live && !intake.live {
            panel.autopilot = autopilot_state(activity.as_deref(), false);
        }
        panel.intake = Some(intake);
        if problems.len() > opened {
            // A store read failed (a restarted database, a dropped
            // connection): reopen on the next read rather than keep a dead
            // handle.
            self.drop_stores(folder);
        }
        if !problems.is_empty() {
            panel.problem = Some(problems.join("; "));
        }
        panel
    }

    fn run_output(&self, folder: &Path, run_id: &str, since: usize) -> Result<RunOutput, String> {
        let state = self.folder(folder);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let resolved = state.resolve(&self.home(), folder)?;
        let db_path = resolved.db_path.clone();
        let fallback = resolved
            .tasks_dir
            .as_ref()
            .map(|dir| resolve_fallback_phases(Some(&dir.to_string_lossy()), None))
            .unwrap_or_default();
        let core = match resolved.stores(folder)? {
            Some(bundle) => Arc::clone(&bundle.core),
            None => return Err("this repo has no store yet: no runs were recorded".into()),
        };
        drop(state);
        let lifecycle = core
            .load_lifecycle(run_id)
            .map_err(|e| format!("reading run {run_id}: {e}"))?
            .ok_or_else(|| format!("no run {run_id} in this repo's store"))?;
        let args = ShowArgs {
            run_or_task_id: run_id.to_owned(),
            common: CommonArgs::default(),
            json: false,
            raw: false,
        };
        let attempts = attempt_checks(core.as_ref(), &lifecycle)?;
        let mut summary: Vec<u8> = Vec::new();
        cmd_show(&args, core.as_ref(), folder, &fallback, &mut summary)
            .map_err(|e| format!("reading run {run_id}: {e}"))?;
        let transcript =
            self.transcript(folder, run_id, run_file_path(&logs_root(&db_path), run_id));
        let mut transcript = transcript.lock().unwrap_or_else(PoisonError::into_inner);
        transcript.pull()?;
        let (from, entries) = transcript.from(since);
        Ok(RunOutput {
            run_id: run_id.to_owned(),
            task_id: Some(lifecycle.task_id.clone()),
            status: Some(lifecycle.status.as_str().to_owned()),
            active: matches!(
                lifecycle.status,
                Status::Pending
                    | Status::Ready
                    | Status::Running
                    | Status::Validating
                    | Status::AwaitingApproval
            ),
            summary: String::from_utf8_lossy(&summary).into_owned(),
            next: transcript.total(),
            from,
            entries,
            attempts,
        })
    }

    fn task_history(&self, folder: &Path, task_id: &str) -> Result<TaskHistory, String> {
        let state = self.folder(folder);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let resolved = state.resolve(&self.home(), folder)?;
        let core = match resolved.stores(folder)? {
            Some(bundle) => Some(Arc::clone(&bundle.core)),
            None => None,
        };
        drop(state);
        let runs = match core {
            Some(core) => task_runs(core.as_ref(), task_id).inspect_err(|_| {
                // A dead handle: reopen on the next read.
                self.drop_stores(folder);
            })?,
            None => Vec::new(),
        };
        let (checks, problem) = match crate::hub_tasks::task_checks(self, folder, task_id) {
            Ok(checks) => (checks, None),
            Err(message) => (Vec::new(), Some(message)),
        };
        Ok(TaskHistory {
            task_id: task_id.to_owned(),
            runs,
            checks,
            problem,
        })
    }

    fn forget(&self, folder: &Path) {
        // Unhooked under the map lock; the state itself (stores, pools)
        // drops when the last read still holding it finishes.
        self.folders
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(folder);
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|open| open.root != folder);
    }

    fn worker_command(&self, _folder: &Path, scope: &WorkerScope) -> Vec<String> {
        let mut argv = vec![
            self.program.to_string_lossy().into_owned(),
            "worker".to_owned(),
        ];
        argv.extend(scope.flags());
        argv
    }

    fn plan_task(&self, folder: &Path, draft: &TaskDraft) -> Result<TaskFilePlan, String> {
        crate::hub_tasks::plan_task(self, folder, draft)
    }

    fn write_task(
        &self,
        folder: &Path,
        draft: &TaskDraft,
        contents: &str,
    ) -> Result<TaskWritten, String> {
        crate::hub_tasks::write_task(self, folder, draft, contents)
    }

    fn task_seed(&self, folder: &Path, task_id: &str) -> Result<TaskSeed, String> {
        crate::hub_tasks::task_seed(self, folder, task_id)
    }

    fn autopilot_command(&self, _folder: &Path) -> Vec<String> {
        vec![
            self.program.to_string_lossy().into_owned(),
            "autopilot".to_owned(),
        ]
    }

    fn set_autopilot_paused(&self, folder: &Path, paused: bool) -> Result<bool, String> {
        let activity = {
            let state = self.folder(folder);
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            state.resolve(&self.home(), folder)?.activity.clone()
        };
        set_paused(&activity, paused).map_err(|e| {
            format!(
                "cannot {} autopilot in {}: {e}",
                if paused { "pause" } else { "resume" },
                folder.display()
            )
        })
    }

    fn decide_gate(
        &self,
        folder: &Path,
        decision: &GateDecision,
    ) -> Result<GateDecided, GateRefused> {
        let state = self.folder(folder);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let resolved = state
            .resolve(&self.home(), folder)
            .map_err(GateRefused::Store)?;
        let core = match resolved.stores(folder).map_err(GateRefused::Store)? {
            Some(bundle) => Arc::clone(&bundle.core),
            None => {
                return Err(GateRefused::UnknownRun(
                    "this repo has no store yet: no runs were recorded".into(),
                ));
            }
        };
        drop(state);
        crate::hub_autopilot::decide(core.as_ref(), decision)
    }

    fn steer_run(
        &self,
        folder: &Path,
        run_id: &str,
        steer: &Steer,
    ) -> Result<Steered, SteerRefused> {
        let state = self.folder(folder);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let resolved = state
            .resolve(&self.home(), folder)
            .map_err(SteerRefused::Store)?;
        let core = match resolved.stores(folder).map_err(SteerRefused::Store)? {
            Some(bundle) => Arc::clone(&bundle.core),
            None => {
                return Err(SteerRefused::UnknownRun(
                    "this repo has no store yet: no runs were recorded".into(),
                ));
            }
        };
        drop(state);
        enqueue_steer(core.as_ref(), run_id, steer)
    }
}

/// Enqueue `steer` for `run_id` through the store's `control_commands`
/// queue, as `fw say` / `fw interrupt` do, but only while the run is
/// running or validating: any other state enqueues nothing.
fn enqueue_steer(
    core: &dyn HarnessStore,
    run_id: &str,
    steer: &Steer,
) -> Result<Steered, SteerRefused> {
    let lifecycle = core
        .load_lifecycle(run_id)
        .map_err(|e| SteerRefused::Store(format!("reading run {run_id}: {e}")))?
        .ok_or_else(|| SteerRefused::UnknownRun(format!("no run {run_id} in this repo's store")))?;
    if !matches!(lifecycle.status, Status::Running | Status::Validating) {
        return Err(SteerRefused::NotLive(format!(
            "run {run_id} is {}, not running or validating: nothing enqueued",
            lifecycle.status.as_str()
        )));
    }
    let (kind, payload) = match steer {
        Steer::Say { text } => {
            let mut payload = Map::new();
            payload.insert("text".into(), Value::String(text.clone()));
            (CONTROL_COMMAND_SAY, payload)
        }
        Steer::Interrupt => (CONTROL_COMMAND_INTERRUPT, Map::new()),
    };
    let record = core
        .enqueue_command(run_id, kind, payload, Utc::now())
        .map_err(|e| SteerRefused::Store(format!("enqueueing the {kind}: {e}")))?;
    Ok(Steered {
        run_id: run_id.to_owned(),
        task_id: lifecycle.task_id,
        kind: kind.to_owned(),
        command_id: record.id.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flywheel_core::store::{ControlCommandStore, LifecycleStore};

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn the_panel_names_the_trust_command_for_an_unapproved_repo_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        // No repo file: nothing to trust.
        assert_eq!(repo_file_trust(Ok(home.clone()), &repo), Ok(None));

        std::fs::write(repo.join(REPO_CONFIG_FILENAME), "verify = \"make\"\n").unwrap();
        // No project id yet (never minted here): untrusted.
        let untrusted = repo_file_trust(Ok(home.clone()), &repo).unwrap().unwrap();
        assert_eq!(
            untrusted.command,
            format!("cd {} && fw trust", repo.display())
        );
        assert_eq!(stored_project_id(&repo), Ok(None), "the hub mints no id");
        assert!(!home.root().exists(), "the hub writes no home state");

        // Approved under the local id and these bytes: trusted.
        let project = flywheel_orchestrator::home::resolve_project(&home, &repo).unwrap();
        let bytes = std::fs::read(repo.join(REPO_CONFIG_FILENAME)).unwrap();
        trust::approve(
            &home,
            &project.id,
            &trust::sha256_hex(&bytes),
            &repo.join(REPO_CONFIG_FILENAME),
        )
        .unwrap();
        assert_eq!(repo_file_trust(Ok(home.clone()), &repo), Ok(None));

        // Edited: untrusted again.
        std::fs::write(repo.join(REPO_CONFIG_FILENAME), "verify = \"make all\"\n").unwrap();
        assert!(repo_file_trust(Ok(home.clone()), &repo).unwrap().is_some());

        // A credential-bearing file is a problem, never trusted state.
        std::fs::write(
            repo.join(REPO_CONFIG_FILENAME),
            "dsn = \"postgres://u:p@h/d\"\n",
        )
        .unwrap();
        let problem = repo_file_trust(Ok(home), &repo).unwrap_err();
        assert!(
            problem.contains("dsn") && !problem.contains("u:p@"),
            "{problem}"
        );
    }

    #[test]
    fn a_human_gate_stop_is_not_repeated_open_or_decided() {
        let stop = serde_json::json!({
            "kind": "awaiting-approval",
            "detail": "human gate: \"t\" run \"run-1\" is awaiting manual approval"
        });
        let entries: Vec<StatusEntry> = serde_json::from_value(serde_json::json!([
            {
                "task_id": "open-gate",
                "state": "awaiting_approval",
                "latest_run_id": "run-1",
                "latest_status": "awaiting_approval",
                "awaiting_on": {"ordinal": 1, "instruction": "Confirm"},
                "stopped": stop,
            },
            {
                "task_id": "landed",
                "state": "done",
                "latest_run_id": "run-2",
                "latest_status": "done",
                "stopped": stop,
            },
        ]))
        .unwrap();
        let mut panel = FlywheelPanel::empty(PathBuf::from("/repo"), Utc::now());
        fold_status(entries, &mut panel);
        let kinds: Vec<(AlertKind, &str)> = panel
            .alerts
            .iter()
            .map(|a| (a.kind, a.subject.as_str()))
            .collect();
        assert_eq!(kinds, [(AlertKind::Awaiting, "open-gate")]);
        assert!(panel.phases[0].tasks.iter().all(|t| t.stopped.is_none()));
    }

    #[test]
    fn a_stopped_task_s_alert_says_why_its_run_stopped() {
        let entries: Vec<StatusEntry> = serde_json::from_value(serde_json::json!([{
            "task_id": "calc-ed4c21-abc5",
            "state": "retryable",
            "latest_run_id": "run-6e5c7a",
            "latest_status": "failed",
            "latest_error": "The safe_div(a, b) implementation is already present on the base",
            "stopped": {
                "kind": "abort",
                "detail": "run \"run-6e5c7a\" ended via intent=abort (The safe_div...); routed for a human, never re-dispatched"
            }
        }]))
        .unwrap();
        let mut panel = FlywheelPanel::empty(PathBuf::from("/repo"), Utc::now());
        fold_status(entries, &mut panel);
        let stopped = panel
            .alerts
            .iter()
            .find(|a| a.kind == AlertKind::Stopped)
            .unwrap();
        assert_eq!(
            stopped.detail,
            "abort -- The safe_div(a, b) implementation is already present on the base"
        );
    }

    /// A run of `task` ending in `status` at `at`, its attempts costing
    /// `costs`.
    fn run(
        task: &str,
        run_id: &str,
        status: Status,
        at: DateTime<Utc>,
        costs: &[f64],
    ) -> Lifecycle {
        let mut lc = Lifecycle::new(task);
        lc.run_id = run_id.to_owned();
        lc.status = status;
        lc.timestamps.insert(status, at);
        for (i, cost) in costs.iter().enumerate() {
            let mut attempt = flywheel_core::lifecycle::Attempt::new(i as u32 + 1, at, run_id);
            attempt.total_cost_usd = *cost;
            attempt.ended_at = Some(at);
            lc.attempts.push(attempt);
        }
        lc
    }

    fn files(name: &str, archived: bool, tasks: &[&str]) -> PlanFiles {
        PlanFiles {
            name: name.to_owned(),
            archived,
            tasks: tasks.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    #[test]
    fn portfolio_counts_an_interrupted_run_as_needing_someone() {
        let now = Utc::now();
        // The panel's light calls an interrupted run blocked, like an unmet
        // prerequisite; only the interrupted one waits for a person.
        let lifecycles = vec![run("held", "r-held", Status::Interrupted, now, &[0.5])];
        let lights: HashMap<&str, TaskLight> = [
            ("held", TaskLight::Blocked),
            ("waiting", TaskLight::Blocked),
        ]
        .into();
        let plans = fold_plans(
            vec![files("01-plan", false, &["held", "waiting"])],
            &lifecycles,
            &lights,
            now,
        );
        assert_eq!(
            plans[0].needs, 1,
            "interrupted needs someone; prerequisite-blocked does not"
        );
        let held = plans[0].tasks.iter().find(|t| t.id == "held").unwrap();
        assert_eq!(held.status.as_deref(), Some("interrupted"));
    }

    #[test]
    fn portfolio_plans_fold_every_run_of_their_tasks() {
        let now = Utc::now();
        let hours = |h: i64| now - chrono::Duration::hours(h);
        // Newest first, as the store lists them.
        let lifecycles = vec![
            // redo: a DONE run superseded by a newer failed one.
            run("redo", "r-redo-2", Status::Failed, hours(1), &[0.5]),
            run("fresh", "r-fresh", Status::Done, hours(23), &[1.0, 0.25]),
            run("stale", "r-stale", Status::Done, hours(25), &[2.0]),
            run("redo", "r-redo-1", Status::Done, hours(2), &[0.125]),
            run("old", "r-old", Status::Done, hours(400), &[3.0]),
            run("gate", "r-gate", Status::AwaitingApproval, hours(3), &[]),
            run("stranger", "r-stranger", Status::Done, hours(1), &[9.0]),
        ];
        let lights: HashMap<&str, TaskLight> = [
            ("redo", TaskLight::Verified),
            ("fresh", TaskLight::Verified),
        ]
        .into();
        let plans = fold_plans(
            vec![
                files("01-live", false, &["fresh", "stale", "redo", "idle"]),
                files("00-done", true, &["old", "gate"]),
            ],
            &lifecycles,
            &lights,
            now,
        );
        // Needing someone first (the archived plan's parked gate).
        let names: Vec<&str> = plans.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["00-done", "01-live"]);
        let live = &plans[1];
        assert!(!live.archived);
        assert_eq!(live.total, 4, "a task with no runs still counts");
        assert_eq!(live.verified, 2, "redo's latest run is not DONE");
        assert_eq!(live.verified_24h, 1, "only fresh ended inside 24h");
        assert!((live.spend_usd - 3.875).abs() < 1e-9, "{}", live.spend_usd);
        assert_eq!(live.needs, 0);
        let done = &plans[0];
        assert!(done.archived);
        assert_eq!((done.verified, done.total), (1, 2));
        assert_eq!(done.verified_24h, 0);
        assert_eq!(
            done.needs, 1,
            "the unlisted gate is classified from its run"
        );
        assert!((done.spend_usd - 3.0).abs() < 1e-9);
        let gate = done.tasks.iter().find(|t| t.id == "gate").unwrap();
        assert_eq!(gate.state, TaskLight::Awaiting);
        assert_eq!(gate.status.as_deref(), Some("awaiting_approval"));
        let ids: Vec<&str> = live.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            ["fresh", "idle", "redo", "stale"],
            "every task file, once"
        );
        assert_eq!(live.tasks[1].status, None, "idle never ran");
    }

    #[test]
    fn portfolio_plans_cover_active_and_archived_phase_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let write = |rel: &str, body: &str| {
            let path = tmp.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write("active/02-new/a.json", r#"{"id": "task-a"}"#);
        write("active/02-new/_notes.json", "{}");
        write("active/02-new/readme.md", "");
        write("archive/01-old/b.json", "not json");
        write("archive/02-new/c.json", r#"{"id": "task-c"}"#);
        write("archive/.hidden/d.json", r#"{"id": "task-d"}"#);
        let plans = plan_files(tmp.path()).unwrap();
        assert_eq!(
            plans,
            [
                files("01-old", true, &["b"]),
                files("02-new", false, &["task-a", "task-c"]),
            ]
        );
    }

    #[test]
    fn a_live_run_s_last_tool_comes_from_its_normalized_transcript() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = run_file_path(tmp.path(), "run-1");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let line = |kind: &str, payload: serde_json::Value| {
            let record = serde_json::json!({
                "kind": kind,
                "ts": "2026-11-10T15:30:34.247779+00:00",
                "run_id": "run-1",
                "attempt_number": 1,
                "iteration_number": 1,
                "payload": {"type": kind, "payload": payload, "source": "claude-code"},
            });
            format!("{record}\n")
        };
        let mut transcript = line(
            "tool.started",
            serde_json::json!({"tool_use_id": "t1", "tool_name": "Read", "tool_input": {"file_path": "/a.rs"}}),
        );
        transcript.push_str(&line(
            "tool.finished",
            serde_json::json!({"tool_use_id": "t1", "is_error": false, "content": "fn a() {}"}),
        ));
        transcript.push_str(&line(
            "tool.started",
            serde_json::json!({"tool_use_id": "t2", "tool_name": "Bash", "tool_input": {"command": "cargo test"}}),
        ));
        transcript.push_str(&line(
            "context.usage",
            serde_json::json!({"usage": {"input_tokens": 2}}),
        ));
        std::fs::write(&file, transcript).unwrap();
        let mut tail = ToolTail::new(file.clone(), "run-1");
        let last = tail
            .pull()
            .expect("a normalized tool.started is a tool call");
        assert_eq!(
            (last.tool.as_str(), last.argument.as_str()),
            ("Bash", "cargo test")
        );
        // Nothing new: the last call seen is kept.
        let again = tail.pull().unwrap();
        assert_eq!(again.tool, "Bash");
    }

    /// An in-memory store holding one run of `task-a` through `path`.
    fn store_with_run(run_id: &str, path: &[Status]) -> InMemoryStore {
        let store = InMemoryStore::new();
        let mut lc = Lifecycle::new("task-a");
        lc.run_id = run_id.to_owned();
        for status in path {
            let error = if *status == Status::Failed {
                "boom"
            } else {
                ""
            };
            lc.transition_to(*status, error, None).unwrap();
        }
        store.create_lifecycle(&lc).unwrap();
        store
    }

    #[test]
    fn steer_enqueues_exactly_one_say_or_interrupt_for_a_live_run() {
        for live in [
            &[Status::Ready, Status::Running][..],
            &[Status::Ready, Status::Running, Status::Validating][..],
        ] {
            let store = store_with_run("run-1", live);
            let said = Steer::Say {
                text: "add docstrings".into(),
            };
            let steered = enqueue_steer(&store, "run-1", &said).unwrap();
            assert_eq!(
                (steered.kind.as_str(), steered.task_id.as_str()),
                ("say", "task-a")
            );
            let commands = store.pending_commands("run-1").unwrap();
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].kind, CONTROL_COMMAND_SAY);
            assert_eq!(commands[0].payload["text"], "add docstrings");
            assert_eq!(commands[0].id, Some(steered.command_id));

            let steered = enqueue_steer(&store, "run-1", &Steer::Interrupt).unwrap();
            assert_eq!(steered.kind, "interrupt");
            let commands = store.pending_commands("run-1").unwrap();
            assert_eq!(commands.len(), 2);
            assert_eq!(commands[1].kind, CONTROL_COMMAND_INTERRUPT);
            assert!(commands[1].payload.is_empty());
        }
    }

    #[test]
    fn steer_refuses_runs_not_running_or_validating_and_enqueues_nothing() {
        let not_live: [&[Status]; 4] = [
            &[],
            &[
                Status::Ready,
                Status::Running,
                Status::Validating,
                Status::Done,
            ],
            &[Status::Ready, Status::Running, Status::Failed],
            &[
                Status::Ready,
                Status::Running,
                Status::Validating,
                Status::AwaitingApproval,
            ],
        ];
        for path in not_live {
            let store = store_with_run("run-1", path);
            for steer in [Steer::Say { text: "hi".into() }, Steer::Interrupt] {
                let refused = enqueue_steer(&store, "run-1", &steer).unwrap_err();
                assert!(
                    matches!(refused, SteerRefused::NotLive(_)),
                    "{path:?}: {refused}"
                );
            }
            assert!(
                store.pending_commands("run-1").unwrap().is_empty(),
                "{path:?}"
            );
        }
        let store = store_with_run("run-1", &[Status::Ready, Status::Running]);
        let refused = enqueue_steer(&store, "run-2", &Steer::Interrupt).unwrap_err();
        assert!(matches!(refused, SteerRefused::UnknownRun(_)));
        assert!(store.pending_commands("run-2").unwrap().is_empty());
    }
}
