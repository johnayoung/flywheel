//! The hub's Flywheel panel data: [`HubFlywheel`] implements
//! `flywheel_hub::flywheel::FlywheelProvider` over the same code paths the
//! operator console and `fw status` use, for one adopted folder at a time.
//!
//! Why here and not in `flywheel-hub`: the read model (`snapshot`, the
//! console's store resolution, the transcript tailer and its renderer)
//! lives in this crate, which sits above the hub; the hub only defines the
//! seam and plain data, so it never compiles the store backends.
//!
//! Per folder: the folder's own `flywheel.toml` (never the process's
//! working directory -- one hub serves many repos) with its relative
//! paths anchored at the folder; the stores it selects, opened once and
//! kept; the work listing behind the console's fingerprint cache. Then:
//!
//! * tasks and alerts: `fw status --json` itself ([`cmd_status`]), so a
//!   strand, stop, or failure reads exactly as the CLI reports it;
//! * in-flight runs: the console's live rows ([`collect_live_rows`]);
//! * recent runs: the newest lifecycles with their latest attempt's grader
//!   receipts from `grader_results`;
//! * worker liveness: a live lease in `task_claims` (as the console's
//!   supervisor reads it); autopilot liveness: its activity record;
//! * run output: `fw show <run>` ([`cmd_show`]) plus the console's
//!   redacted transcript ([`TranscriptTailer`]), tailed incrementally.
//!
//! Read-only: nothing here writes the store or the task tree (opening a
//! store runs its idempotent schema bootstrap, as every `fw` read verb
//! does). A missing SQLite store is never created: the panel lists the
//! tasks as pending against an empty in-memory store instead.
//!
//! Postgres: like every `fw` verb, the DSN comes from the environment
//! (`FLYWHEEL_PG_DSN` / `DATABASE_URL`) -- here the hub process's, the same
//! for every folder (flywheel.toml never carries one). The store label
//! names the database read (`postgres host:port/db`, no credentials) so a
//! folder read from the wrong one shows it. A failed read drops the
//! connection; the next read reconnects.

use crate::session::TranscriptTailer;
use crate::session_screen::is_error_entry;
use crate::snapshot::{CachedWorkSource, collect_live_rows, directory_fingerprint};
use crate::tui::{DEFAULT_STORE_PATH, open_store_bundle, process_environ};
use chrono::{DateTime, Utc};
use flywheel_core::audit::run_file_path;
use flywheel_core::lifecycle::{Lifecycle, Status};
use flywheel_core::store::{HarnessStore, LifecycleFilter};
use flywheel_core::store_memory::InMemoryStore;
use flywheel_hub::flywheel::{
    Alert, AlertKind, DaemonState, FlywheelPanel, FlywheelProvider, GraderOutcome, LiveRun, Note,
    PanelClaim, PanelPhase, PanelTask, RecentRun, RunEntry, RunOutput, TaskLight,
};
use flywheel_orchestrator::autopilot_activity::{epoch_now, is_record_live, read_activity};
use flywheel_orchestrator::claims::{ClaimStore, InMemoryClaimStore};
use flywheel_orchestrator::pg_preflight::dsn_target;
use flywheel_orchestrator::policy::{
    DEFAULT_POLICY_FILENAME, DEFAULT_TASKS_DIR, WorkPolicy, load_policy,
};
use flywheel_orchestrator::sources::{DirectoryWorkSource, WorkSource};
use flywheel_orchestrator::store_factory::{StoreBundle, resolve_postgres_dsn};
use flywheel_orchestrator::workflow::status::format_blocked_on;
use flywheel_orchestrator::workflow::{
    CommonArgs, ShowArgs, StatusArgs, cmd_show, cmd_status, resolve_fallback_phases,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// How many recent runs the panel lists.
pub const RECENT_RUNS: usize = 12;
/// Where `fw autopilot` keeps its activity record by default (under the
/// repo root).
pub const AUTOPILOT_ACTIVITY: &str = ".flywheel/logs/autopilot/activity.json";
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

/// `path` anchored at `root` when relative.
fn anchored(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// `path` relative to `root` for display, else as is.
fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// What one folder's policy resolves to, plus the stores and listing kept
/// open across reads.
#[derive(Default)]
struct Folder {
    /// The `flywheel.toml` stamp `resolved` was read under.
    policy_stamp: Option<Option<(u64, u128)>>,
    resolved: Option<Result<Resolved, String>>,
    /// The main checkout when the folder is a linked git worktree; read
    /// once (`Some(None)`: not one).
    worktree_of: Option<Option<PathBuf>>,
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
    stores: Option<OpenStores>,
}

struct OpenStores {
    bundle: StoreBundle,
    /// The SQLite file's identity when opened: a replaced file reopens.
    identity: Option<(u64, u64)>,
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.len(), 0))
}

impl Folder {
    fn worktree_of(&mut self, root: &Path) -> Option<PathBuf> {
        self.worktree_of
            .get_or_insert_with(|| flywheel_hub::git::linked_worktree_main(root))
            .clone()
    }

    /// Re-read the policy when `flywheel.toml` changed (or on first use).
    fn resolve(&mut self, root: &Path) -> Result<&mut Resolved, String> {
        let policy_file = root.join(DEFAULT_POLICY_FILENAME);
        let current = stamp(&policy_file);
        if self.policy_stamp != Some(current) || self.resolved.is_none() {
            self.policy_stamp = Some(current);
            self.resolved = Some(Resolved::read(root, &policy_file, current.is_some()));
        }
        match self.resolved.as_mut().expect("just set") {
            Ok(resolved) => Ok(resolved),
            Err(message) => Err(message.clone()),
        }
    }
}

impl Resolved {
    fn read(root: &Path, policy_file: &Path, has_policy: bool) -> Result<Self, String> {
        let mut policy = if has_policy {
            Some(load_policy(policy_file).map_err(|e| format!("{DEFAULT_POLICY_FILENAME}: {e}"))?)
        } else {
            None
        };
        if let Some(policy) = &mut policy {
            policy.db_path = policy.db_path.as_deref().map(|p| anchored(root, p));
            policy.tasks_dir = policy.tasks_dir.as_deref().map(|p| anchored(root, p));
        }
        let db_path = policy
            .as_ref()
            .and_then(|p| p.db_path.clone())
            .unwrap_or_else(|| root.join(DEFAULT_STORE_PATH));
        let backend = policy
            .as_ref()
            .map_or_else(|| "sqlite".to_owned(), |p| p.store_backend.clone());
        let source_kind = policy
            .as_ref()
            .map_or_else(|| "directory".to_owned(), |p| p.source_kind.clone());
        let (tasks_dir, source, source_label) = if source_kind == "directory" {
            let dir = policy
                .as_ref()
                .and_then(|p| p.tasks_dir.clone())
                .unwrap_or_else(|| root.join(DEFAULT_TASKS_DIR));
            let source: Box<dyn WorkSource> = Box::new(CachedWorkSource::for_directory(
                Box::new(DirectoryWorkSource::new(&dir)),
                dir.clone(),
            ));
            let label = format!("directory {}", shown(root, &dir));
            (Some(dir), Some(source), label)
        } else {
            (None, None, source_kind)
        };
        Ok(Self {
            policy,
            db_path,
            backend,
            tasks_dir,
            source,
            source_label,
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
        if let Some(schema) = self.policy.as_ref().and_then(|p| p.store_schema.as_deref()) {
            label.push_str(&format!(" (schema {schema})"));
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
            if !sqlite && resolve_postgres_dsn(&process_environ()).is_none() {
                return Err(format!(
                    "the store backend is {} but the hub's environment sets neither \
                     FLYWHEEL_PG_DSN nor DATABASE_URL: start `fw hub serve` from a shell \
                     that exports the DSN this repo's `fw` verbs use",
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
            stopped: entry.stopped.as_ref().map(|s| Note {
                kind: s.kind.clone(),
                detail: s.detail.clone(),
            }),
            awaiting: entry.awaiting_on.as_ref().map(|a| a.instruction.clone()),
            blocked_on: entry
                .blocked_requires
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(format_blocked_on),
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
        if let Some(stop) = &entry.stopped {
            alert(AlertKind::Stopped, joined(&stop.kind, &stop.detail));
        }
        phases.entry(phase_of(&entry)).or_default().push(task);
    }
    panel.phases = phases
        .into_iter()
        .map(|(name, tasks)| PanelPhase { name, tasks })
        .collect();
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
        })
        .collect())
}

fn recent_run(core: &dyn HarnessStore, lc: &Lifecycle) -> Result<RecentRun, String> {
    let latest = lc.attempts.last();
    let graders = match latest {
        Some(attempt) => core
            .list_grader_results(&lc.run_id, attempt.number)
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
            .collect(),
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

fn recent_runs(core: &dyn HarnessStore) -> Result<Vec<RecentRun>, String> {
    // Newest first: the store orders by (updated_at DESC, run_id DESC).
    let lifecycles = core
        .list_lifecycles(&LifecycleFilter::default())
        .map_err(|e| format!("reading runs: {e}"))?;
    lifecycles
        .iter()
        .take(RECENT_RUNS)
        .map(|lc| recent_run(core, lc))
        .collect()
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

fn autopilot_state(root: &Path) -> DaemonState {
    let Some(activity) = read_activity(Some(&root.join(AUTOPILOT_ACTIVITY))) else {
        return DaemonState {
            live: false,
            detail: "no activity record (not running here)".into(),
        };
    };
    let now = epoch_now();
    if is_record_live(Some(&activity), now) {
        DaemonState {
            live: true,
            detail: format!(
                "pid {}, {} (cycle {})",
                activity.pid, activity.phase, activity.cycle_index
            ),
        }
    } else {
        let ago = (now - activity.updated_at).max(0.0) as i64;
        DaemonState {
            live: false,
            detail: format!(
                "stopped (last seen {} ago, pid {})",
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

impl FlywheelProvider for HubFlywheel {
    fn adopted(&self, folder: &Path) -> bool {
        // A linked git worktree of an adopted repo counts too: flywheel
        // keeps a store per checkout (`fw worker` and `fw status` resolve
        // `.flywheel/` under the worktree itself, never the main
        // checkout's), so its panel reads its own store and says so
        // (`worktree_of`).
        folder.join(DEFAULT_POLICY_FILENAME).is_file() || folder.join(".flywheel").is_dir()
    }

    fn fingerprint(&self, folder: &Path) -> u64 {
        let mut hasher = DefaultHasher::new();
        stamp(&folder.join(DEFAULT_POLICY_FILENAME)).hash(&mut hasher);
        stamp(&folder.join(AUTOPILOT_ACTIVITY)).hash(&mut hasher);
        let state = self.folder(folder);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(resolved) = state.resolve(folder) {
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
        hasher.finish()
    }

    fn panel(&self, folder: &Path) -> FlywheelPanel {
        let now = Utc::now();
        let mut panel = FlywheelPanel::empty(folder.to_path_buf(), now);
        panel.autopilot = autopilot_state(folder);
        let state = self.folder(folder);
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        panel.worktree_of = state.worktree_of(folder);
        let resolved = match state.resolve(folder) {
            Ok(resolved) => resolved,
            Err(message) => {
                panel.problem = Some(message);
                return panel;
            }
        };
        panel.source = resolved.source_label.clone();
        panel.store = resolved.store_label(folder);
        let mut problems: Vec<String> = Vec::new();
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
        match recent_runs(core.as_ref()) {
            Ok(runs) => panel.runs = runs,
            Err(message) => problems.push(message),
        }
        match claims_and_worker(claims.as_ref(), now) {
            Ok((rows, worker)) => {
                panel.claims = rows;
                panel.worker = worker;
            }
            Err(message) => problems.push(message),
        }
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
        let resolved = state.resolve(folder)?;
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
        let mut summary: Vec<u8> = Vec::new();
        cmd_show(&args, core.as_ref(), folder, &fallback, &mut summary)
            .map_err(|e| format!("reading run {run_id}: {e}"))?;
        // The console's logs root: `<store dir>/logs`.
        let logs_root = db_path
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf)
            .join("logs");
        let transcript = self.transcript(folder, run_id, run_file_path(&logs_root, run_id));
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

    fn worker_command(&self, _folder: &Path) -> Vec<String> {
        vec![
            self.program.to_string_lossy().into_owned(),
            "worker".to_owned(),
        ]
    }
}
