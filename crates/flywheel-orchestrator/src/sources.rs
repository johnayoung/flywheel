//! Work sources: where the orchestrator's work items come from.
//!
//! The orchestrator schedules [`WorkItem`] values; it does not care where
//! they live. A [`WorkSource`] is the inbound/outbound boundary with
//! whatever system owns the work: a directory of task JSON files, an issue
//! tracker, a queue. Everything tool-specific stays in the adapter.
//!
//! * **Inbound**: [`WorkSource::list_work`] enumerates the items that are
//!   candidates right now, each already compiled to a validated core
//!   [`Task`] plus the orchestration-layer `prerequisites` edges. Items
//!   that cannot compile to a runnable task are the adapter's problem to
//!   skip or surface; they never reach the scheduler.
//! * **Outbound**: [`WorkSource::report`] receives a [`WorkReport`] every
//!   time the orchestrator finishes driving a run for one of the source's
//!   items. The store remains flywheel's authoritative record; the report
//!   is a projection of it for the system that owns the work. Reports are
//!   best-effort: a failing `report` is contained by the orchestrator and
//!   never unwinds the scheduling loop.
//!
//! [`DirectoryWorkSource`] is the reference adapter: the historical
//! `<tasks_dir>/active/<phase>/<task>.json` layout. Its `report` is a no-op
//! because the local store *is* the record.
//!
//! Authoritative spec: `docs/work-sources.md`.

use flywheel_core::lifecycle::Status;
use flywheel_core::loaders::{TaskLoadError, load_task_data, task_digest};
use flywheel_core::task::{GraderType, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Provenance tag stamped on every item a [`DirectoryWorkSource`] emits.
pub const DIRECTORY_SOURCE_KIND: &str = "directory";

/// The phase name of a task file sitting directly under `active/`.
pub const ROOT_PHASE: &str = "_root";

/// A source's stop-event sink: `sink(kind, subject, detail)` appends one
/// durable stop record to the orchestrator's append-only ledger without
/// changing what the source lists. `kind` is one of the orchestrator's stop
/// event kinds (`source-truncation` / `zero-grader-drop` for a source),
/// `subject` is the source name (its repo or tasks_dir path), and `detail`
/// names the cause. A source given no sink behaves byte-for-byte as before;
/// the record is an audit witness, never a scheduling input.
pub type StopEventSink = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

/// A source's diagnostic line sink: one human-readable line per skipped
/// item or truncation warning.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Raised when a work source cannot enumerate or compile its items.
///
/// The message always identifies the offending source/item so callers see
/// actionable errors. Adapter-internal transport failures (a tracker API
/// error, an unreadable directory) surface as this type so the CLI can
/// render them uniformly.
#[derive(Debug, thiserror::Error)]
pub enum WorkSourceError {
    /// An adapter-level failure (transport, compile) naming its source.
    #[error("{0}")]
    Source(String),
    /// A task definition that failed to load; the message names the file.
    #[error(transparent)]
    TaskLoad(#[from] TaskLoadError),
    /// A filesystem failure while enumerating a file-backed source.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl WorkSourceError {
    /// An adapter-level error with a message naming the source/item.
    pub fn new(message: impl Into<String>) -> Self {
        Self::Source(message.into())
    }

    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// One schedulable unit handed to the orchestrator by a work source.
///
/// `task` is the compiled, validated core [`Task`], the only thing that
/// crosses into flywheel. `prerequisites` carries the orchestration-layer
/// dependency edges (task ids that must reach DONE first); the core task
/// deliberately has no such field. `source_ref` is the adapter's opaque
/// handle for the item (a file path, `owner/repo#123`); it round-trips
/// into [`WorkReport`] and is persisted as the lifecycle's `source` label.
/// `local_path` is set only by file-backed sources; `phase` is the
/// directory a file-backed item lives in under `active/` (see
/// [`phase_of_task_file`]) and `None` for non-file sources.
///
/// `source_kind`/`source_version`/`source_url` carry the item's provenance
/// for persistence: where it came from (`"directory"`, `"github_issue"`), a
/// stable content hash that changes when the operator edits the underlying
/// definition, and a locator (a file path, an issue URL).
///
/// `priority`/`required_capabilities`/`conflict_keys` are the
/// orchestration-layer scheduling metadata (spec 00049). They are not part
/// of the core single-task definition; file-backed sources read them from
/// the task file's top-level JSON. Every optional field defaults so direct
/// construction via [`WorkItem::new`] still compiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub task: Task,
    #[serde(default)]
    pub prerequisites: Vec<String>,
    pub source_ref: String,
    #[serde(default)]
    pub local_path: Option<PathBuf>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub source_kind: Option<String>,
    #[serde(default)]
    pub source_version: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
    #[serde(default)]
    pub conflict_keys: BTreeSet<String>,
}

impl WorkItem {
    /// An item with the two required fields and every optional field at
    /// its default (no edges, no provenance, priority 0, empty sets).
    pub fn new(task: Task, source_ref: impl Into<String>) -> Self {
        Self {
            task,
            prerequisites: Vec::new(),
            source_ref: source_ref.into(),
            local_path: None,
            phase: None,
            source_kind: None,
            source_version: None,
            source_url: None,
            priority: 0,
            required_capabilities: BTreeSet::new(),
            conflict_keys: BTreeSet::new(),
        }
    }

    pub fn with_prerequisites<I, S>(mut self, prerequisites: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.prerequisites = prerequisites.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_required_capabilities<I, S>(mut self, capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.required_capabilities = capabilities.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_conflict_keys<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.conflict_keys = keys.into_iter().map(Into::into).collect();
        self
    }

    /// The task id, the key every graph and store operation uses.
    pub fn id(&self) -> &str {
        &self.task.id
    }
}

/// One grader execution receipt projected into a [`WorkReport`].
///
/// A flattened view of the store's grader result record carrying only what
/// an external system needs to render "how done was decided": position,
/// type, optional name, and the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraderReceipt {
    pub ordinal: u32,
    pub grader_type: GraderType,
    #[serde(default)]
    pub name: Option<String>,
    pub passed: bool,
}

/// Terminal outcome of one orchestrator-driven run, for the work owner.
///
/// `status` is the lifecycle status the run ended this drive at: `Done` on
/// verified completion, `Failed`/`FailedValidation` on exhausted budgets,
/// `Interrupted`/`AwaitingApproval` when parked. `graders` holds the final
/// attempt's receipts (empty when the run never reached grading). The
/// evidence is a projection of the store; the store row, not this report,
/// is the authoritative record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkReport {
    pub task_id: String,
    pub source_ref: String,
    pub run_id: String,
    pub status: Status,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub graders: Vec<GraderReceipt>,
}

/// The orchestrator's boundary with the system that owns the work.
///
/// `source_kind` and `source_name` are required members here (the Python
/// seam duck-types them) because `sync_work_source` stamps both onto the
/// `source_syncs` row: an adapter that forgot either would fail at sync
/// time rather than at compile time.
pub trait WorkSource: Send + Sync {
    /// The provenance tag stamped on every emitted item (`"directory"`,
    /// `"github_issue"`, `"github_ci"`, `"github_review"`).
    fn source_kind(&self) -> &str;

    /// The source's locus for `source_syncs`: a tasks_dir path or
    /// `owner/repo`.
    fn source_name(&self) -> String;

    /// Enumerate the current candidate items, compiled and validated.
    ///
    /// Called once per scheduling pass, so implementations should be
    /// cheap-ish and deterministic in order (ties in selection break by
    /// enumeration order). A parse or transport break must return an error,
    /// never an empty list: an empty list reads downstream as "all that
    /// work disappeared".
    fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError>;

    /// Receive the outcome of one driven run (best-effort delivery).
    ///
    /// The orchestrator calls this after the consumer `submit` step, while
    /// the task's lease is still held. Errors are contained by the caller:
    /// a failing report costs the notification, never the schedule.
    fn report(&self, report: &WorkReport) -> Result<(), WorkSourceError>;
}

// --- Top-level task-file keys ------------------------------------------------

/// Render one entry of a top-level string list as a string.
///
/// Strings pass through verbatim; any other JSON scalar or container is
/// rendered as its compact JSON text so a hand-written `"prerequisites":
/// [12]` still yields `"12"` rather than vanishing.
fn json_entry_as_string(entry: &Value) -> String {
    match entry {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Coerce a top-level JSON list into a list of strings; absent or
/// non-list yields the empty list.
fn read_str_list(data: &Value, key: &str) -> Vec<String> {
    match data.get(key) {
        Some(Value::Array(entries)) => entries.iter().map(json_entry_as_string).collect(),
        _ => Vec::new(),
    }
}

/// Coerce a top-level JSON list into a string set; absent or non-list
/// yields the empty set.
fn read_str_set(data: &Value, key: &str) -> BTreeSet<String> {
    read_str_list(data, key).into_iter().collect()
}

/// A task file's `prerequisites` edges, read from an already-decoded
/// task object (the orchestration DAG; core flywheel ignores the key).
pub fn prerequisites_from_value(data: &Value) -> Vec<String> {
    read_str_list(data, "prerequisites")
}

/// A task file's `overlap_ok` allow-list, read from an already-decoded
/// task object.
///
/// `overlap_ok` is a validate-time-only key consumed solely by the
/// authoring-time surface-overlap lint: each entry is a repo-relative path
/// exempt from that pairwise check. It never rides on a [`WorkItem`] and
/// plays no role at claim time.
pub fn overlap_ok_from_value(data: &Value) -> Vec<String> {
    read_str_list(data, "overlap_ok")
}

/// The orchestration scheduling metadata a task file carries (spec 00049).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SchedulingMetadata {
    pub priority: i64,
    pub required_capabilities: BTreeSet<String>,
    pub conflict_keys: BTreeSet<String>,
}

/// A task file's scheduling metadata, read from an already-decoded task
/// object.
///
/// `priority` (integer, default 0), `required_capabilities` and
/// `conflict_keys` (string sets, default empty) are orchestration-layer
/// keys core flywheel ignores. An absent or malformed key yields its
/// default; a JSON boolean or non-integral number is never a priority.
pub fn scheduling_metadata_from_value(data: &Value) -> SchedulingMetadata {
    if !data.is_object() {
        return SchedulingMetadata::default();
    }
    let priority = data
        .get("priority")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    SchedulingMetadata {
        priority,
        required_capabilities: read_str_set(data, "required_capabilities"),
        conflict_keys: read_str_set(data, "conflict_keys"),
    }
}

/// Decode a task file as a JSON value; unreadable or malformed yields
/// `None` (the file-reading twin of the `*_from_value` readers).
fn read_task_value(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read a task file's `prerequisites` edges; unreadable or malformed
/// yields the empty list.
pub fn read_prerequisites(path: impl AsRef<Path>) -> Vec<String> {
    read_task_value(path.as_ref())
        .map(|data| prerequisites_from_value(&data))
        .unwrap_or_default()
}

/// Read a task file's `overlap_ok` allow-list; unreadable or malformed
/// yields the empty list.
pub fn read_overlap_ok(path: impl AsRef<Path>) -> Vec<String> {
    read_task_value(path.as_ref())
        .map(|data| overlap_ok_from_value(&data))
        .unwrap_or_default()
}

/// Read a task file's scheduling metadata; unreadable or malformed yields
/// the defaults.
pub fn read_scheduling_metadata(path: impl AsRef<Path>) -> SchedulingMetadata {
    read_task_value(path.as_ref())
        .map(|data| scheduling_metadata_from_value(&data))
        .unwrap_or_default()
}

// --- The active/<phase>/<task>.json walk --------------------------------------

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, WorkSourceError> {
    let read = std::fs::read_dir(dir).map_err(|e| WorkSourceError::io(dir, e))?;
    let mut entries = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| WorkSourceError::io(dir, e))?;
        entries.push(entry.path());
    }
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(entries)
}

fn file_name_str(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// The `active/<phase>` subdirectories of `tasks_dir` in deterministic
/// (name-sorted) order. Hidden directories are skipped; a missing
/// `active/` yields the empty list.
pub fn iter_active_phase_dirs(
    tasks_dir: impl AsRef<Path>,
) -> Result<Vec<PathBuf>, WorkSourceError> {
    let active = tasks_dir.as_ref().join("active");
    if !active.is_dir() {
        return Ok(Vec::new());
    }
    Ok(sorted_entries(&active)?
        .into_iter()
        .filter(|entry| entry.is_dir() && !file_name_str(entry).starts_with('.'))
        .collect())
}

/// Every `active/<phase>/<task>.json` under `tasks_dir` in deterministic
/// (phase-sorted, then name-sorted) walk order. Files whose name starts
/// with `_` or `.`, and non-`.json` entries, are skipped.
pub fn iter_active_task_files(
    tasks_dir: impl AsRef<Path>,
) -> Result<Vec<PathBuf>, WorkSourceError> {
    let mut files = Vec::new();
    for phase in iter_active_phase_dirs(tasks_dir)? {
        for entry in sorted_entries(&phase)? {
            let name = file_name_str(&entry);
            if entry.is_file()
                && entry.extension().is_some_and(|ext| ext == "json")
                && !name.starts_with('_')
                && !name.starts_with('.')
            {
                files.push(entry);
            }
        }
    }
    Ok(files)
}

/// Load every active task; fail on the first bad file.
///
/// The list is in deterministic walk order so selection ties break by
/// filename.
pub fn load_active_tasks(
    tasks_dir: impl AsRef<Path>,
) -> Result<Vec<(PathBuf, Task)>, WorkSourceError> {
    let mut out = Vec::new();
    for path in iter_active_task_files(tasks_dir)? {
        let task = load_task_file_once(&path).map(|(task, _)| task)?;
        out.push((path, task));
    }
    Ok(out)
}

/// Phase of a task file: the directory under `active/` it lives in
/// ([`ROOT_PHASE`] when directly under `active/`, or when the file is not
/// under `active/` at all). Drives the `flywheel/<phase>/<task-id>` branch
/// name.
pub fn phase_of_task_file(task_file: impl AsRef<Path>, tasks_dir: impl AsRef<Path>) -> String {
    let task_file = task_file.as_ref();
    let active_root = tasks_dir.as_ref().join("active");
    phase_from_resolved(
        task_file,
        &active_root,
        std::fs::canonicalize(task_file).ok().as_deref(),
        std::fs::canonicalize(&active_root).ok().as_deref(),
    )
}

/// The [`phase_of_task_file`] rule over already-resolved paths: strip the
/// canonical active root from the canonical file when both resolved, else
/// fall back to the lexical strip.
fn phase_from_resolved(
    task_file: &Path,
    active_root: &Path,
    canonical_file: Option<&Path>,
    canonical_root: Option<&Path>,
) -> String {
    let rel = match (canonical_file, canonical_root) {
        (Some(file), Some(root)) => file.strip_prefix(root).ok(),
        _ => task_file.strip_prefix(active_root).ok(),
    };
    let Some(rel) = rel else {
        return ROOT_PHASE.to_string();
    };
    match rel.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ => ROOT_PHASE.to_string(),
    }
}

/// [`phase_of_task_file`] for every file of one listing, without its two
/// full `canonicalize` calls per file: the active root is resolved once per
/// listing and each phase directory once per run of its files. A file that
/// is not itself a symlink canonicalizes to its canonical parent joined
/// with its name (realpath only rewrites symlink components), so the result
/// is identical; a symlinked file, or any path this shortcut cannot
/// resolve, takes the full `canonicalize`.
struct PhaseResolver {
    active_root: PathBuf,
    canonical_root: io::Result<PathBuf>,
    canonical_parent: Option<(PathBuf, io::Result<PathBuf>)>,
}

impl PhaseResolver {
    fn new(tasks_dir: &Path) -> Self {
        let active_root = tasks_dir.join("active");
        let canonical_root = std::fs::canonicalize(&active_root);
        Self {
            active_root,
            canonical_root,
            canonical_parent: None,
        }
    }

    fn phase_of(&mut self, task_file: &Path) -> String {
        let canonical_file = self.canonical_file(task_file);
        phase_from_resolved(
            task_file,
            &self.active_root,
            canonical_file.ok().as_deref(),
            self.canonical_root.as_deref().ok(),
        )
    }

    fn canonical_file(&mut self, task_file: &Path) -> io::Result<PathBuf> {
        let (Some(parent), Some(name)) = (task_file.parent(), task_file.file_name()) else {
            return std::fs::canonicalize(task_file);
        };
        match std::fs::symlink_metadata(task_file) {
            Ok(meta) if !meta.file_type().is_symlink() => {}
            _ => return std::fs::canonicalize(task_file),
        }
        if self
            .canonical_parent
            .as_ref()
            .is_none_or(|(cached, _)| cached != parent)
        {
            self.canonical_parent = Some((parent.to_path_buf(), std::fs::canonicalize(parent)));
        }
        match &self.canonical_parent {
            Some((_, Ok(canonical))) => Ok(canonical.join(name)),
            _ => std::fs::canonicalize(task_file),
        }
    }
}

/// Read and decode a task file exactly once, returning the validated task
/// and the raw top-level object so the orchestration keys can be read
/// without a second pass over the file.
fn load_task_file_once(path: &Path) -> Result<(Task, Value), TaskLoadError> {
    let source = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .map_err(|e| TaskLoadError(format!("{source}: cannot read file: {e}")))?;
    // Branded exactly as the core loader brands a syntax error, so the
    // skip line reads the same whichever entry point loaded the file.
    let data = serde_json::from_str::<Value>(&text)
        .map_err(|err| TaskLoadError(format!("{source}: invalid JSON: {err}")))?;
    let task = load_task_data(&data, &source)?;
    Ok((task, data))
}

/// The reference [`WorkSource`]: a directory of task JSON files.
///
/// Wraps the historical `<tasks_dir>/active/<phase>/<task>.json` layout.
/// `source_ref` is the file path string (exactly what the lifecycle's
/// `source` label persists), `local_path` is the same path, `phase` is the
/// enclosing directory name.
///
/// `report` is intentionally a no-op: for local files the store is already
/// the durable record and phase archiving is a separate directory-shaped
/// flow driven by the worker.
///
/// One unloadable task file (malformed JSON, a definition that fails
/// validation) is **skipped, not fatal**: `list_work` counts it in
/// [`DirectoryWorkSource::last_skipped_count`] and emits one `log` line
/// naming the file, then returns every other valid item in deterministic
/// walk order. The count is what lets a downstream reconciler tell a skip
/// ("one item was dropped, investigate it") from an empty source ("there
/// is simply no work"); the two must never look alike.
pub struct DirectoryWorkSource {
    tasks_dir: PathBuf,
    log: Option<LogSink>,
    last_skipped_count: AtomicUsize,
}

impl fmt::Debug for DirectoryWorkSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryWorkSource")
            .field("tasks_dir", &self.tasks_dir)
            .field("last_skipped_count", &self.last_skipped_count())
            .finish()
    }
}

impl DirectoryWorkSource {
    pub fn new(tasks_dir: impl Into<PathBuf>) -> Self {
        Self {
            tasks_dir: tasks_dir.into(),
            log: None,
            last_skipped_count: AtomicUsize::new(0),
        }
    }

    /// Attach a diagnostic sink that receives one line per skipped file.
    pub fn with_log(mut self, log: LogSink) -> Self {
        self.log = Some(log);
        self
    }

    pub fn tasks_dir(&self) -> &Path {
        &self.tasks_dir
    }

    /// Number of task files the most recent [`WorkSource::list_work`]
    /// skipped because they could not be loaded. Zero before the first
    /// listing; reset by every listing.
    pub fn last_skipped_count(&self) -> usize {
        self.last_skipped_count.load(Ordering::SeqCst)
    }
}

impl WorkSource for DirectoryWorkSource {
    fn source_kind(&self) -> &str {
        DIRECTORY_SOURCE_KIND
    }

    fn source_name(&self) -> String {
        self.tasks_dir.display().to_string()
    }

    fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError> {
        let mut items = Vec::new();
        let mut skipped = 0usize;
        let mut phases = PhaseResolver::new(&self.tasks_dir);
        for path in iter_active_task_files(&self.tasks_dir)? {
            let (task, data) = match load_task_file_once(&path) {
                Ok(loaded) => loaded,
                Err(exc) => {
                    skipped += 1;
                    if let Some(log) = &self.log {
                        log(&format!(
                            "[directory] skipping {}: cannot load task -- {exc}",
                            path.display()
                        ));
                    }
                    continue;
                }
            };
            let SchedulingMetadata {
                priority,
                required_capabilities,
                conflict_keys,
            } = scheduling_metadata_from_value(&data);
            let source_ref = path.display().to_string();
            let source_version = task_digest(&task);
            items.push(WorkItem {
                task,
                prerequisites: prerequisites_from_value(&data),
                source_ref: source_ref.clone(),
                phase: Some(phases.phase_of(&path)),
                local_path: Some(path),
                source_kind: Some(DIRECTORY_SOURCE_KIND.to_string()),
                source_version: Some(source_version),
                source_url: Some(source_ref),
                priority,
                required_capabilities,
                conflict_keys,
            });
        }
        self.last_skipped_count.store(skipped, Ordering::SeqCst);
        Ok(items)
    }

    fn report(&self, _report: &WorkReport) -> Result<(), WorkSourceError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn write_task(phase: &Path, task_id: &str, extra: Value) -> PathBuf {
        std::fs::create_dir_all(phase).unwrap();
        let mut payload = json!({
            "id": task_id,
            "goal": format!("Goal for {task_id}."),
            "graders": [{"type": "command", "run": "true"}],
        });
        if let (Value::Object(target), Value::Object(extra)) = (&mut payload, extra) {
            target.extend(extra);
        }
        let path = phase.join(format!("{task_id}.json"));
        std::fs::write(&path, serde_json::to_string(&payload).unwrap()).unwrap();
        path
    }

    fn write_bad(phase: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(phase).unwrap();
        let path = phase.join(format!("{name}.json"));
        std::fs::write(&path, "{ this is not valid json").unwrap();
        path
    }

    fn ids(items: &[WorkItem]) -> Vec<&str> {
        items.iter().map(WorkItem::id).collect()
    }

    fn capture_log() -> (LogSink, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = Arc::clone(&lines);
        let sink: LogSink = Arc::new(move |line: &str| {
            sink_lines.lock().unwrap().push(line.to_string());
        });
        (sink, lines)
    }

    #[test]
    fn lists_tasks_in_walk_order_with_prereqs() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        write_task(&tasks_dir.join("active/02-late"), "z-second", json!({}));
        write_task(
            &tasks_dir.join("active/01-early"),
            "a-first",
            json!({"prerequisites": ["z-second"]}),
        );

        let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();

        assert_eq!(ids(&items), ["a-first", "z-second"]);
        assert_eq!(items[0].prerequisites, ["z-second"]);
        assert!(items[1].prerequisites.is_empty());
        let expected = tasks_dir.join("active/01-early/a-first.json");
        assert_eq!(items[0].source_ref, expected.display().to_string());
        assert_eq!(items[0].local_path.as_deref(), Some(expected.as_path()));
        assert_eq!(
            items[0].source_url.as_deref(),
            Some(items[0].source_ref.as_str())
        );
        assert_eq!(items[0].phase.as_deref(), Some("01-early"));
        assert_eq!(items[1].phase.as_deref(), Some("02-late"));
        assert_eq!(items[0].source_kind.as_deref(), Some("directory"));
        assert_eq!(
            items[0].source_version.as_deref(),
            Some(task_digest(&items[0].task).as_str())
        );
    }

    #[test]
    fn reads_scheduling_metadata_and_ignores_overlap_ok() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let path = write_task(
            &tasks_dir.join("active/01-phase"),
            "meta",
            json!({
                "priority": 7,
                "required_capabilities": ["gpu", "docker", "gpu"],
                "conflict_keys": ["schema", 12],
                "overlap_ok": ["src/shared.py"],
            }),
        );
        write_task(
            &tasks_dir.join("active/01-phase"),
            "bool-priority",
            json!({"priority": true, "required_capabilities": "nope"}),
        );
        write_task(
            &tasks_dir.join("active/01-phase"),
            "float-priority",
            json!({"priority": 2.5}),
        );

        let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();
        let by_id = |id: &str| items.iter().find(|i| i.id() == id).unwrap();

        let meta = by_id("meta");
        assert_eq!(meta.priority, 7);
        assert_eq!(
            meta.required_capabilities,
            BTreeSet::from(["docker".to_string(), "gpu".to_string()])
        );
        assert_eq!(
            meta.conflict_keys,
            BTreeSet::from(["12".to_string(), "schema".to_string()])
        );
        assert_eq!(by_id("bool-priority").priority, 0);
        assert!(by_id("bool-priority").required_capabilities.is_empty());
        assert_eq!(by_id("float-priority").priority, 0);
        assert_eq!(read_overlap_ok(&path), ["src/shared.py"]);
        assert_eq!(read_prerequisites(&path), Vec::<String>::new());
        assert_eq!(read_scheduling_metadata(&path).priority, 7);
    }

    #[test]
    fn walk_skips_hidden_dirs_underscore_files_and_non_json() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let phase = tasks_dir.join("active/01-phase");
        write_task(&phase, "keep", json!({}));
        std::fs::write(phase.join("_template.json"), "{}").unwrap();
        std::fs::write(phase.join(".hidden.json"), "{}").unwrap();
        std::fs::write(phase.join("notes.md"), "x").unwrap();
        std::fs::write(phase.join(".loop-base"), "abc").unwrap();
        std::fs::create_dir_all(tasks_dir.join("active/.git")).unwrap();
        write_task(&tasks_dir.join("active/.git"), "ghost", json!({}));
        std::fs::write(tasks_dir.join("active/loose.json"), "{}").unwrap();

        assert_eq!(
            iter_active_phase_dirs(&tasks_dir).unwrap(),
            vec![phase.clone()]
        );
        assert_eq!(
            iter_active_task_files(&tasks_dir).unwrap(),
            vec![phase.join("keep.json")]
        );
        let items = DirectoryWorkSource::new(&tasks_dir).list_work().unwrap();
        assert_eq!(ids(&items), ["keep"]);
    }

    #[test]
    fn missing_active_dir_lists_nothing() {
        let tmp = TempDir::new().unwrap();
        let source = DirectoryWorkSource::new(tmp.path().join("tasks"));
        assert!(source.list_work().unwrap().is_empty());
        assert_eq!(source.last_skipped_count(), 0);
        assert!(
            iter_active_phase_dirs(tmp.path().join("nope"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn report_is_noop() {
        let tmp = TempDir::new().unwrap();
        let source = DirectoryWorkSource::new(tmp.path().join("tasks"));
        let report = WorkReport {
            task_id: "x".into(),
            source_ref: "x.json".into(),
            run_id: "run-x".into(),
            status: Status::Done,
            error: String::new(),
            graders: vec![GraderReceipt {
                ordinal: 0,
                grader_type: GraderType::Command,
                name: None,
                passed: true,
            }],
        };
        source.report(&report).unwrap();
        assert_eq!(source.source_kind(), "directory");
        assert_eq!(
            source.source_name(),
            tmp.path().join("tasks").display().to_string()
        );
    }

    #[test]
    fn one_bad_file_is_skipped_rest_return() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("tasks/active/01-phase");
        write_task(&phase, "alpha", json!({}));
        let bad = write_bad(&phase, "beta-bad");
        write_task(&phase, "gamma", json!({}));

        let (sink, lines) = capture_log();
        let source = DirectoryWorkSource::new(tmp.path().join("tasks")).with_log(sink);
        let items = source.list_work().unwrap();

        assert_eq!(ids(&items), ["alpha", "gamma"]);
        assert!(
            items
                .iter()
                .all(|i| i.local_path.as_deref() != Some(bad.as_path()))
        );
        assert_eq!(source.last_skipped_count(), 1);
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(&bad.display().to_string()));
        assert!(lines[0].starts_with("[directory] skipping "));
    }

    #[test]
    fn invalid_definition_is_skipped_with_branded_message() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("tasks/active/01-phase");
        std::fs::create_dir_all(&phase).unwrap();
        let bad = phase.join("no-goal.json");
        std::fs::write(&bad, r#"{"id": "no-goal", "graders": []}"#).unwrap();
        write_task(&phase, "ok", json!({}));

        let (sink, lines) = capture_log();
        let source = DirectoryWorkSource::new(tmp.path().join("tasks")).with_log(sink);
        let items = source.list_work().unwrap();

        assert_eq!(ids(&items), ["ok"]);
        assert_eq!(source.last_skipped_count(), 1);
        let lines = lines.lock().unwrap();
        assert!(lines[0].contains(&bad.display().to_string()));
        assert!(load_active_tasks(tmp.path().join("tasks")).is_err());
    }

    #[test]
    fn surviving_items_keep_deterministic_walk_order() {
        let tmp = TempDir::new().unwrap();
        let p1 = tmp.path().join("tasks/active/01-first");
        let p2 = tmp.path().join("tasks/active/02-second");
        write_task(&p1, "a-task", json!({}));
        write_bad(&p1, "z-bad");
        write_task(&p1, "b-task", json!({}));
        write_task(&p2, "c-task", json!({}));

        let source = DirectoryWorkSource::new(tmp.path().join("tasks"));
        let items = source.list_work().unwrap();
        assert_eq!(ids(&items), ["a-task", "b-task", "c-task"]);
        assert_eq!(source.last_skipped_count(), 1);
    }

    #[test]
    fn only_bad_files_yields_empty_list_plus_recorded_skips() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("tasks/active/01-phase");
        write_bad(&phase, "one-bad");
        write_bad(&phase, "two-bad");

        let (sink, lines) = capture_log();
        let source = DirectoryWorkSource::new(tmp.path().join("tasks")).with_log(sink);
        let items = source.list_work().unwrap();

        assert!(items.is_empty());
        assert_eq!(source.last_skipped_count(), 2);
        assert_eq!(lines.lock().unwrap().len(), 2);
    }

    #[test]
    fn empty_source_is_distinguishable_from_a_skip() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("tasks/active")).unwrap();
        let source = DirectoryWorkSource::new(tmp.path().join("tasks"));
        assert!(source.list_work().unwrap().is_empty());
        assert_eq!(source.last_skipped_count(), 0);
    }

    #[test]
    fn skip_count_resets_between_calls() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("tasks/active/01-phase");
        write_task(&phase, "alpha", json!({}));
        let bad = write_bad(&phase, "beta-bad");

        let source = DirectoryWorkSource::new(tmp.path().join("tasks"));
        assert_eq!(ids(&source.list_work().unwrap()), ["alpha"]);
        assert_eq!(source.last_skipped_count(), 1);

        std::fs::remove_file(bad).unwrap();
        assert_eq!(ids(&source.list_work().unwrap()), ["alpha"]);
        assert_eq!(source.last_skipped_count(), 0);
    }

    #[test]
    fn load_active_tasks_returns_walk_order_pairs() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let b = write_task(&tasks_dir.join("active/02"), "b", json!({}));
        let a = write_task(&tasks_dir.join("active/01"), "a", json!({}));
        let loaded = load_active_tasks(&tasks_dir).unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|(p, t)| (p.clone(), t.id.clone()))
                .collect::<Vec<_>>(),
            vec![(a, "a".to_string()), (b, "b".to_string())]
        );
    }

    #[test]
    fn phase_of_task_file_derives_from_layout() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let nested = write_task(&tasks_dir.join("active/03-deep"), "deep", json!({}));
        assert_eq!(phase_of_task_file(&nested, &tasks_dir), "03-deep");
        // Directly under active/ and outside active/ both map to the root phase.
        assert_eq!(
            phase_of_task_file(tasks_dir.join("active/loose.json"), &tasks_dir),
            ROOT_PHASE
        );
        assert_eq!(
            phase_of_task_file(tmp.path().join("elsewhere/x.json"), &tasks_dir),
            ROOT_PHASE
        );
        // Lexical fallback when neither path exists yet.
        assert_eq!(
            phase_of_task_file("/virtual/tasks/active/09-x/t.json", "/virtual/tasks"),
            "09-x"
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_work_phases_match_phase_of_task_file_across_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        let active = tasks_dir.join("active");
        write_task(&active.join("01-plain"), "plain-a", json!({}));
        write_task(&active.join("01-plain"), "plain-b", json!({}));
        // A phase directory that is a symlink to a tree outside active/.
        let outside = tmp.path().join("outside-phase");
        write_task(&outside, "via-dir-link", json!({}));
        symlink(&outside, active.join("02-linked-dir")).unwrap();
        // A task file that is a symlink, both into active/ and out of it.
        let elsewhere = write_task(&tmp.path().join("elsewhere"), "via-file-link", json!({}));
        std::fs::create_dir_all(active.join("03-file-links")).unwrap();
        symlink(
            &elsewhere,
            active.join("03-file-links").join("via-file-link.json"),
        )
        .unwrap();
        let inner = write_task(&active.join("04-target"), "inner-target", json!({}));
        std::fs::create_dir_all(active.join("05-inner-link")).unwrap();
        symlink(&inner, active.join("05-inner-link").join("inner-link.json")).unwrap();
        // The tasks dir itself reached through a symlinked ancestor.
        let linked_root = tmp.path().join("linked-root");
        symlink(tmp.path(), &linked_root).unwrap();

        for root in [tasks_dir.clone(), linked_root.join("tasks")] {
            let items = DirectoryWorkSource::new(&root).list_work().unwrap();
            assert_eq!(items.len(), 6);
            for item in &items {
                let path = item.local_path.as_ref().unwrap();
                assert_eq!(
                    item.phase.as_deref(),
                    Some(phase_of_task_file(path, &root).as_str()),
                    "{}",
                    path.display()
                );
            }
            let phases: Vec<&str> = items.iter().filter_map(|i| i.phase.as_deref()).collect();
            // Links resolving outside active/ fall to the root phase; the
            // in-tree file link takes its target's phase.
            assert_eq!(
                phases,
                [
                    "01-plain",
                    "01-plain",
                    ROOT_PHASE,
                    ROOT_PHASE,
                    "04-target",
                    "04-target"
                ]
            );
        }
    }

    #[test]
    fn work_item_builders_default_everything_optional() {
        let task = Task::new("Goal.", vec![]).with_id("t");
        let item = WorkItem::new(task.clone(), "mem://t");
        assert_eq!(item.priority, 0);
        assert!(item.prerequisites.is_empty() && item.phase.is_none());
        let item = WorkItem::new(task, "mem://t")
            .with_prerequisites(["a", "b"])
            .with_priority(3)
            .with_required_capabilities(["gpu"])
            .with_conflict_keys(["k"]);
        assert_eq!(item.prerequisites, ["a", "b"]);
        assert_eq!(item.priority, 3);
        assert!(item.required_capabilities.contains("gpu"));
        assert!(item.conflict_keys.contains("k"));
        let round = serde_json::to_string(&item).unwrap();
        assert_eq!(serde_json::from_str::<WorkItem>(&round).unwrap(), item);
    }

    #[test]
    fn work_source_error_wraps_task_load_error() {
        let err: WorkSourceError = TaskLoadError("x.json: bad".into()).into();
        assert_eq!(err.to_string(), "x.json: bad");
        assert_eq!(
            WorkSourceError::new("gh exited 1").to_string(),
            "gh exited 1"
        );
    }

    // --- parity with the Python DirectoryWorkSource over the real repo tree --

    #[derive(Deserialize)]
    struct ParityItem {
        file: String,
        id: String,
        phase: String,
        prerequisites: Vec<String>,
        priority: i64,
        required_capabilities: Vec<String>,
        conflict_keys: Vec<String>,
        source_version: String,
        source_kind: String,
    }

    #[derive(Deserialize)]
    struct ParityListing {
        items: Vec<ParityItem>,
        skipped_count: usize,
        skipped: Vec<String>,
    }

    #[derive(Deserialize)]
    struct ParityFixture {
        active: ParityListing,
        archive: ParityListing,
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn assert_listing_matches(expected: &ParityListing, tasks_dir: &Path) {
        let (sink, lines) = capture_log();
        let source = DirectoryWorkSource::new(tasks_dir).with_log(sink);
        let items = source.list_work().unwrap();
        let active = tasks_dir.join("active");

        assert_eq!(
            items.len(),
            expected.items.len(),
            "item count differs from Python"
        );
        for (got, want) in items.iter().zip(&expected.items) {
            let rel = got
                .local_path
                .as_ref()
                .unwrap()
                .strip_prefix(&active)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert_eq!(rel, want.file, "walk order differs from Python");
            assert_eq!(got.id(), want.id, "{rel}: id");
            assert_eq!(
                got.phase.as_deref(),
                Some(want.phase.as_str()),
                "{rel}: phase"
            );
            assert_eq!(
                got.prerequisites, want.prerequisites,
                "{rel}: prerequisites"
            );
            assert_eq!(got.priority, want.priority, "{rel}: priority");
            assert_eq!(
                got.required_capabilities
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                want.required_capabilities,
                "{rel}: required_capabilities"
            );
            assert_eq!(
                got.conflict_keys.iter().cloned().collect::<Vec<_>>(),
                want.conflict_keys,
                "{rel}: conflict_keys"
            );
            assert_eq!(
                got.source_version.as_deref(),
                Some(want.source_version.as_str()),
                "{rel}: source_version (task_digest)"
            );
            assert_eq!(got.source_kind.as_deref(), Some(want.source_kind.as_str()));
        }
        assert_eq!(source.last_skipped_count(), expected.skipped_count);
        let skipped: Vec<String> = lines
            .lock()
            .unwrap()
            .iter()
            .map(|line| {
                let start = line.find("skipping ").unwrap() + "skipping ".len();
                let end = line.find(": cannot load").unwrap();
                Path::new(&line[start..end])
                    .strip_prefix(&active)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(skipped, expected.skipped);
    }

    #[test]
    fn parity_with_python_listing_of_repo_tasks_tree() {
        let fixture: ParityFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/directory_source_parity.json"
        ))
        .unwrap();
        let repo = repo_root();

        // The live active/ tree, read in place (read-only).
        assert_listing_matches(&fixture.active, &repo.join(".flywheel/tasks"));

        // The archive holds the repo's real task definitions; Python listed
        // it through a temporary tasks_dir whose active/ is a copy of it,
        // and the Rust listing reads an identical copy.
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        copy_tree(
            &repo.join(".flywheel/tasks/archive"),
            &tasks_dir.join("active"),
        );
        assert!(
            fixture.archive.items.len() > 100,
            "fixture must cover the real archive"
        );
        assert_listing_matches(&fixture.archive, &tasks_dir);
    }
}
