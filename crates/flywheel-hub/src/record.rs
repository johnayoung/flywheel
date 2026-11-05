//! Session recording: opt-in per session (`fw hub record start|stop`, the
//! UI's record toggle), written as asciinema v2 `.cast` files under
//! `<state-dir>/recordings/` (0600).
//!
//! A recording starts with the screen as it is (a screen-only snapshot,
//! as an output event at t=0), then every byte of output with its time,
//! and every resize (`"r"` events). A marker (`"m"`) notes the process's
//! exit, a resync after falling behind, and the hand-on to the next part.
//! Each file is capped ([`RecordLimits::max_file_bytes`]): past it the
//! recording rotates to a new part (`<base>.part<N>.cast`, starting from a
//! fresh snapshot); the directory is capped too
//! ([`RecordLimits::max_total_bytes`]): the oldest finished recordings are
//! deleted to make room. A recording ends when its process exits, when it
//! is stopped, or when the hub stops; a live handoff (`fw hub upgrade`)
//! keeps it going in the same file (the new hub reopens it in append mode,
//! from `recordings/active.json`).

use crate::hub::{Hub, HubError, HubEvent};
use crate::pty::{Attachment, PtySession, SessionEvent};
use crate::state::StateError;
use crate::vt::Emulator;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// The recordings directory inside the state dir.
pub const RECORDINGS_DIR: &str = "recordings";
/// What a live handoff passes on (inside [`RECORDINGS_DIR`]).
const ACTIVE_FILE: &str = "active.json";
/// Default per-file cap: past it a recording rotates to a new part.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Default cap on the whole directory: the oldest finished recordings go.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
/// How often an idle recorder looks for output (the timing resolution of
/// output arriving after a pause).
const POLL: Duration = Duration::from_millis(10);
/// Bytes an event line takes beyond its data (time, code, quoting).
const EVENT_OVERHEAD: u64 = 64;
/// Room kept at the end of a part for the marker that ends it
/// (`continued in <next part>`, or the exit).
const END_RESERVE: u64 = 512;
/// Longest header line read when listing.
const MAX_HEADER: u64 = 64 * 1024;
/// How much of a file's end is read to find its last event time.
const TAIL: u64 = 256 * 1024;

/// Bounds of a part cap (`hub.toml` `recordings.part_cap`): below the
/// floor a part would hold little beyond its opening snapshot.
pub const MIN_PART_BYTES: u64 = 64 * 1024;
pub const MAX_PART_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest directory cap (`recordings.dir_cap`).
pub const MAX_DIR_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

const SIZE_UNITS: &[(&str, u64)] = &[
    ("tib", 1 << 40),
    ("gib", 1 << 30),
    ("mib", 1 << 20),
    ("kib", 1 << 10),
    ("tb", 1 << 40),
    ("gb", 1 << 30),
    ("mb", 1 << 20),
    ("kb", 1 << 10),
    ("t", 1 << 40),
    ("g", 1 << 30),
    ("m", 1 << 20),
    ("k", 1 << 10),
    ("b", 1),
];

/// Parse a size: plain bytes (`65536`) or a number with a binary unit
/// (`512KiB`, `64MiB`, `1GiB`; `K`/`KB`, `M`/`MB`, `G`/`GB`, `T`/`TB`
/// mean the same, case aside).
pub fn parse_size(raw: &str) -> Result<u64, String> {
    let text = raw.trim().to_ascii_lowercase().replace(' ', "");
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    let bad =
        || format!("bad size {raw:?}: a number of bytes, or with a unit like 512KiB, 64MiB, 1GiB");
    let n: u64 = number.parse().map_err(|_| bad())?;
    let scale = if unit.is_empty() {
        1
    } else {
        SIZE_UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, scale)| *scale)
            .ok_or_else(bad)?
    };
    n.checked_mul(scale).ok_or_else(bad)
}

/// A size written back the way [`parse_size`] reads it: the largest
/// binary unit that divides it (`64MiB`), else bytes.
pub fn format_size(bytes: u64) -> String {
    for (name, scale) in [
        ("TiB", 1u64 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ] {
        if bytes >= scale && bytes.is_multiple_of(scale) {
            return format!("{}{name}", bytes / scale);
        }
    }
    bytes.to_string()
}

const RUN: u8 = 0;
/// Drain what was output, then end the recording.
const STOP: u8 = 1;
/// Drain what was output, then let go of the file (a handoff): the
/// recording carries on in whichever process resumes it.
const FREEZE: u8 = 2;

/// Size caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLimits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for RecordLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

/// Recording failures.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("recording {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn io_error(path: &Path) -> impl Fn(std::io::Error) -> RecordError + '_ {
    move |source| RecordError::Io {
        path: path.to_owned(),
        source,
    }
}

/// A recording as listed.
#[derive(Debug, Clone, Serialize)]
pub struct RecordingInfo {
    /// The file name (`<session>-<when>[.partN].cast`).
    pub name: String,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub part: u32,
    /// Bytes on disk.
    pub size: u64,
    /// Unix seconds it started (the header's timestamp).
    pub started: Option<u64>,
    /// Unix seconds it was last written.
    pub modified: Option<u64>,
    pub cols: Option<u64>,
    pub rows: Option<u64>,
    /// Still being written.
    pub active: bool,
}

/// One recording being written, as a handoff passes it on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ActiveEntry {
    session_id: String,
    /// The current part's file name.
    file: String,
    /// The name every part shares (`<base>.cast`, `<base>.partN.cast`).
    base: String,
    part: u32,
    /// Unix milliseconds the current part started (event times count
    /// from here).
    started_ms: u64,
    title: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ActiveFile {
    recordings: Vec<ActiveEntry>,
}

/// Told when a recording ended by itself (its process exited, or writing
/// failed), with the session's id and the last part's file name.
pub type EndNotice = Arc<dyn Fn(&str, &str) + Send + Sync>;

struct Recorder {
    entry: Arc<Mutex<ActiveEntry>>,
    generation: u64,
    control: Arc<AtomicU8>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A hub's recordings: the directory, and a recorder thread per session
/// being recorded.
pub struct Recordings {
    dir: PathBuf,
    /// Changeable while recordings run (`hub.toml` `[recordings]`): a
    /// recorder reads the part cap before every event, sweeps the
    /// directory cap.
    limits: Mutex<RecordLimits>,
    recorders: Mutex<HashMap<String, Recorder>>,
    generation: std::sync::atomic::AtomicU64,
    /// Serializes rotations' and starts' directory sweeps.
    sweeping: Mutex<()>,
}

impl std::fmt::Debug for Recordings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recordings")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Whether `name` is a recording file name (no path, no dot-dot).
pub fn valid_name(name: &str) -> bool {
    name.len() <= 200
        && name.ends_with(".cast")
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !name.contains("..")
}

/// A session name as a file name part: letters, digits, `-` and `_`
/// (anything else a `-`), at most 48 characters; the session id's start
/// when nothing is left.
fn file_label(name: &str, session_id: &str) -> String {
    let mut label = String::new();
    for c in name.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '-'
        };
        if c == '-' && (label.is_empty() || label.ends_with('-')) {
            continue;
        }
        label.push(c);
        if label.len() >= 48 {
            break;
        }
    }
    let label = label.trim_end_matches('-');
    if label.is_empty() {
        session_id.chars().take(12).collect()
    } else {
        label.to_owned()
    }
}

/// The name every part of the recording `name` shares.
fn base_of(name: &str) -> &str {
    let stem = name.strip_suffix(".cast").unwrap_or(name);
    match stem.rsplit_once(".part") {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => stem,
    }
}

fn part_file(base: &str, part: u32) -> String {
    if part <= 1 {
        format!("{base}.cast")
    } else {
        format!("{base}.part{part}.cast")
    }
}

impl Recordings {
    /// The recordings of `state_dir` (nothing is created until one starts).
    pub fn new(state_dir: &Path, limits: RecordLimits) -> Self {
        Self {
            dir: state_dir.join(RECORDINGS_DIR),
            limits: Mutex::new(limits),
            recorders: Mutex::new(HashMap::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
            sweeping: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn limits(&self) -> RecordLimits {
        *self.limits.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Put new caps in force: running recordings rotate at the new part
    /// cap from their next event; the directory cap applies at the next
    /// sweep (a start or a rotation).
    pub fn set_limits(&self, limits: RecordLimits) {
        *self.limits.lock().unwrap_or_else(PoisonError::into_inner) = limits;
    }

    fn recorders(&self) -> MutexGuard<'_, HashMap<String, Recorder>> {
        self.recorders
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn ensure_dir(&self) -> Result<(), RecordError> {
        crate::paths::ensure_private_dir(&self.dir).map_err(io_error(&self.dir))
    }

    /// A normal start: recordings a previous hub left marked active ended
    /// with it (their processes did not survive); forget them.
    pub fn clear_stale(&self) {
        let path = self.dir.join(ACTIVE_FILE);
        if path.exists() {
            let _ = fs::remove_file(path);
        }
    }

    /// Persist the active recordings (for a handoff to pick up).
    fn save_active(&self) {
        let recordings: Vec<ActiveEntry> = self
            .recorders()
            .values()
            .map(|r| {
                r.entry
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            })
            .collect();
        let path = self.dir.join(ACTIVE_FILE);
        if recordings.is_empty() {
            let _ = fs::remove_file(&path);
            return;
        }
        if self.ensure_dir().is_err() {
            return;
        }
        if let Ok(json) = serde_json::to_vec_pretty(&ActiveFile { recordings })
            && let Err(e) = crate::auth::write_private(&path, &json)
        {
            eprintln!("fw hub: {}: {e}", path.display());
        }
    }

    /// The file session `id` is being recorded to.
    pub fn active_file(&self, id: &str) -> Option<String> {
        self.recorders().get(id).map(|r| {
            r.entry
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .file
                .clone()
        })
    }

    /// Whether session `id` is being recorded (its recorder subscribes to
    /// the session's output: not a viewer).
    pub fn is_recording(&self, id: &str) -> bool {
        self.recorders().get(id).is_some_and(|r| r.thread.is_some())
    }

    /// Start recording session `session_id` (live, running) under `title`,
    /// in a file named for `label` (its session name) and the UTC time.
    /// Already recording: that recording, `false`.
    pub fn start(
        self: &Arc<Self>,
        session_id: &str,
        label: &str,
        title: &str,
        session: &Arc<PtySession>,
        ended: EndNotice,
    ) -> Result<(RecordingInfo, bool), RecordError> {
        if let Some(file) = self.active_file(session_id) {
            return Ok((self.info_of(&file)?, false));
        }
        if session.exit_info().is_some() || session.is_released() {
            return Err(RecordError::Conflict(format!(
                "session {session_id} is not running: nothing to record"
            )));
        }
        self.ensure_dir()?;
        let started_ms = now_ms();
        let stamp =
            chrono::DateTime::<chrono::Utc>::from(UNIX_EPOCH + Duration::from_millis(started_ms))
                .format("%Y%m%dT%H%M%S%3fZ")
                .to_string();
        let mut base = format!("{}-{stamp}", file_label(label, session_id));
        if self.dir.join(part_file(&base, 1)).exists() {
            // Another session of that name started this millisecond.
            let short: String = session_id.chars().take(8).collect();
            base = format!("{base}-{short}");
        }
        let entry = ActiveEntry {
            session_id: session_id.to_owned(),
            file: part_file(&base, 1),
            base,
            part: 1,
            started_ms,
            title: title.to_owned(),
        };
        self.sweep();
        let attachment = session.attach_with(0);
        // Exited since the check above: its exit event went out before
        // this subscription, so a recorder would never see it.
        if attachment.exit.is_some() {
            return Err(RecordError::Conflict(format!(
                "session {session_id} is not running: nothing to record"
            )));
        }
        let path = self.dir.join(&entry.file);
        let mut writer = CastWriter::create(&path, attachment.cols, attachment.rows, &entry)
            .map_err(io_error(&path))?;
        writer
            .snapshot(&attachment.replay)
            .map_err(io_error(&path))?;
        writer.flush().map_err(io_error(&path))?;
        let file = entry.file.clone();
        let started = {
            let mut recorders = self.recorders();
            if recorders.contains_key(session_id) {
                // Raced another start: keep that one.
                drop(recorders);
                let _ = fs::remove_file(&path);
                return Ok((
                    self.info_of(&self.active_file(session_id).unwrap_or_default())?,
                    false,
                ));
            }
            let recorder = self.spawn(entry, writer, attachment, session, ended);
            recorders.insert(session_id.to_owned(), recorder);
            true
        };
        self.save_active();
        Ok((self.info_of(&file)?, started))
    }

    fn spawn(
        self: &Arc<Self>,
        entry: ActiveEntry,
        writer: CastWriter,
        attachment: Attachment,
        session: &Arc<PtySession>,
        ended: EndNotice,
    ) -> Recorder {
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entry = Arc::new(Mutex::new(entry));
        let control = Arc::new(AtomicU8::new(RUN));
        let worker = Worker {
            owner: Arc::downgrade(self),
            entry: Arc::clone(&entry),
            generation,
            control: Arc::clone(&control),
            session: Arc::downgrade(session),
            mirror: mirror_of(attachment.cols, attachment.rows, &attachment.replay),
            events: attachment.events,
            writer,
            ended,
        };
        let thread = std::thread::Builder::new()
            .name("hub-recorder".into())
            .spawn(move || worker.run())
            .ok();
        Recorder {
            entry,
            generation,
            control,
            thread,
        }
    }

    /// Stop recording session `id`: what was output so far is written out
    /// and the file closed.
    pub fn stop(&self, id: &str) -> Result<RecordingInfo, RecordError> {
        // Listed (its file active: no sweep deletes it) until its thread
        // has written out and closed the file.
        let (entry, generation, thread) = {
            let mut recorders = self.recorders();
            let Some(recorder) = recorders.get_mut(id) else {
                return Err(RecordError::Conflict(format!(
                    "session {id} is not being recorded"
                )));
            };
            recorder.control.store(STOP, Ordering::Release);
            (
                Arc::clone(&recorder.entry),
                recorder.generation,
                recorder.thread.take(),
            )
        };
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        {
            let mut recorders = self.recorders();
            if recorders
                .get(id)
                .is_some_and(|r| r.generation == generation)
            {
                recorders.remove(id);
            }
        }
        self.save_active();
        let file = entry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .file
            .clone();
        self.info_of(&file)
    }

    /// The hub is stopping: finish every recording.
    pub fn stop_all(&self) {
        let recorders: Vec<Recorder> = self.recorders().drain().map(|(_, r)| r).collect();
        for recorder in &recorders {
            recorder.control.store(STOP, Ordering::Release);
        }
        for mut recorder in recorders {
            if let Some(thread) = recorder.thread.take() {
                let _ = thread.join();
            }
        }
        self.clear_stale();
    }

    /// A live handoff froze the sessions: write out what they output and
    /// let go of the files, keeping the recordings active (in
    /// `active.json`) for whichever process carries on.
    pub fn freeze(&self) {
        let threads: Vec<std::thread::JoinHandle<()>> = {
            let mut recorders = self.recorders();
            recorders
                .values_mut()
                .filter_map(|r| {
                    r.control.store(FREEZE, Ordering::Release);
                    r.thread.take()
                })
                .collect()
        };
        for thread in threads {
            let _ = thread.join();
        }
        self.save_active();
    }

    /// The new hub of a handoff: take over the recordings the old one
    /// left active (not yet writing; see [`Recordings::resume`]).
    pub fn load_active(&self) {
        let path = self.dir.join(ACTIVE_FILE);
        let Ok(bytes) = fs::read(&path) else {
            return;
        };
        let Ok(file) = serde_json::from_slice::<ActiveFile>(&bytes) else {
            eprintln!(
                "fw hub: {}: unreadable; recordings not resumed",
                path.display()
            );
            return;
        };
        let mut recorders = self.recorders();
        for entry in file.recordings {
            if !valid_name(&entry.file) || recorders.contains_key(&entry.session_id) {
                continue;
            }
            let generation = self
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            recorders.insert(
                entry.session_id.clone(),
                Recorder {
                    entry: Arc::new(Mutex::new(entry)),
                    generation,
                    control: Arc::new(AtomicU8::new(RUN)),
                    thread: None,
                },
            );
        }
    }

    /// Carry on every frozen recording in its file (append mode), against
    /// the live session `live` finds -- before the sessions resume, so no
    /// output is missed. A session gone or ended meanwhile ends its
    /// recording.
    pub fn resume(
        self: &Arc<Self>,
        live: impl Fn(&str) -> Option<Arc<PtySession>>,
        ended: &EndNotice,
    ) {
        let ids: Vec<String> = self
            .recorders()
            .iter()
            .filter(|(_, r)| r.thread.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let Some(old) = self.recorders().remove(&id) else {
                continue;
            };
            let entry = old
                .entry
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let path = self.dir.join(&entry.file);
            let Some(session) = live(&id) else {
                continue;
            };
            let attachment = session.attach_with(0);
            let mut writer = match CastWriter::append(&path, entry.started_ms) {
                Ok(writer) => writer,
                Err(e) => {
                    eprintln!("fw hub: recording {}: {e}; not resumed", path.display());
                    continue;
                }
            };
            if let Some(exit) = &attachment.exit {
                // Ended during the handoff.
                let _ = writer.marker(&exit_label(exit.code, exit.signal.as_deref()));
                let _ = writer.flush();
                continue;
            }
            let recorder = self.spawn(entry, writer, attachment, &session, Arc::clone(ended));
            self.recorders().insert(id, recorder);
        }
        self.save_active();
    }

    /// The thread of a recording that ended by itself drops its entry.
    fn ended(&self, id: &str, generation: u64) -> bool {
        let removed = {
            let mut recorders = self.recorders();
            if recorders
                .get(id)
                .is_some_and(|r| r.generation == generation)
            {
                recorders.remove(id);
                true
            } else {
                false
            }
        };
        if removed {
            self.save_active();
        }
        removed
    }

    /// Delete the oldest finished recordings until the directory fits
    /// [`RecordLimits::max_total_bytes`].
    fn sweep(&self) {
        let _sweeping = self.sweeping.lock().unwrap_or_else(PoisonError::into_inner);
        let active: HashSet<String> = self
            .recorders()
            .values()
            .map(|r| {
                r.entry
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .file
                    .clone()
            })
            .collect();
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(SystemTime, u64, PathBuf, String)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !valid_name(&name) {
                    return None;
                }
                let meta = e.metadata().ok()?;
                meta.is_file().then(|| {
                    (
                        meta.modified().unwrap_or(UNIX_EPOCH),
                        meta.len(),
                        e.path(),
                        name,
                    )
                })
            })
            .collect();
        let max_total = self.limits().max_total_bytes;
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by_key(|f| f.0);
        for (_, size, path, name) in files {
            if total <= max_total {
                break;
            }
            if active.contains(&name) {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }

    /// Every recording, newest first.
    pub fn list(&self) -> Result<Vec<RecordingInfo>, RecordError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(RecordError::Io {
                    path: self.dir.clone(),
                    source,
                });
            }
        };
        let mut list: Vec<RecordingInfo> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                valid_name(&name)
                    .then(|| self.info_of(&name).ok())
                    .flatten()
            })
            .collect();
        // A recording's parts together, in order; the recording with the
        // latest part first.
        let mut latest: HashMap<String, (Option<u64>, Option<u64>)> = HashMap::new();
        for info in &list {
            let slot = latest.entry(base_of(&info.name).to_owned()).or_default();
            *slot = (*slot).max((info.started, info.modified));
        }
        list.sort_by(|a, b| {
            let (ka, kb) = (base_of(&a.name), base_of(&b.name));
            latest[kb]
                .cmp(&latest[ka])
                .then(kb.cmp(ka))
                .then(a.part.cmp(&b.part))
        });
        Ok(list)
    }

    /// A recording's path (it exists).
    pub fn path_of(&self, name: &str) -> Result<PathBuf, RecordError> {
        if !valid_name(name) {
            return Err(RecordError::Invalid(format!(
                "{name:?} is not a recording name"
            )));
        }
        let path = self.dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => Ok(path),
            _ => Err(RecordError::NotFound(format!("recording {name}"))),
        }
    }

    /// Delete a finished recording (an active one is refused: stop it).
    pub fn delete(&self, name: &str) -> Result<(), RecordError> {
        let path = self.path_of(name)?;
        let active = self
            .recorders()
            .values()
            .any(|r| r.entry.lock().unwrap_or_else(PoisonError::into_inner).file == name);
        if active {
            return Err(RecordError::Conflict(format!(
                "{name} is still being recorded: stop it first"
            )));
        }
        fs::remove_file(&path).map_err(io_error(&path))
    }

    fn info_of(&self, name: &str) -> Result<RecordingInfo, RecordError> {
        let path = self.path_of(name)?;
        let meta = fs::metadata(&path).map_err(io_error(&path))?;
        let header = read_header(&path).unwrap_or(Value::Null);
        let active = self
            .recorders()
            .values()
            .any(|r| r.entry.lock().unwrap_or_else(PoisonError::into_inner).file == name);
        let hub = &header["x_fw_hub"];
        Ok(RecordingInfo {
            name: name.to_owned(),
            session_id: hub["session_id"].as_str().map(str::to_owned),
            title: header["title"].as_str().map(str::to_owned),
            part: hub["part"]
                .as_u64()
                .and_then(|p| u32::try_from(p).ok())
                .unwrap_or(1),
            size: meta.len(),
            started: header["timestamp"].as_u64(),
            modified: meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            cols: header["width"].as_u64(),
            rows: header["height"].as_u64(),
            active,
        })
    }
}

fn read_header(path: &Path) -> Option<Value> {
    let mut file = fs::File::open(path).ok()?;
    let mut head = Vec::new();
    (&mut file).take(MAX_HEADER).read_to_end(&mut head).ok()?;
    let line = head.split(|&b| b == b'\n').next()?;
    serde_json::from_slice(line).ok()
}

fn exit_label(code: Option<i32>, signal: Option<&str>) -> String {
    match (code, signal) {
        (_, Some(signal)) => format!("exit: killed by {signal}"),
        (Some(code), None) => format!("exit: {code}"),
        (None, None) => "exit".to_owned(),
    }
}

/// A recorder thread.
struct Worker {
    owner: Weak<Recordings>,
    entry: Arc<Mutex<ActiveEntry>>,
    generation: u64,
    control: Arc<AtomicU8>,
    session: Weak<PtySession>,
    /// The terminal as the recording so far draws it: fed exactly what is
    /// written, so a new part opens on the screen where the last one
    /// ended -- not on the session's screen, which may be ahead by the
    /// output still queued here.
    mirror: Emulator,
    events: broadcast::Receiver<SessionEvent>,
    writer: CastWriter,
    ended: EndNotice,
}

/// A recorder's [`Worker::mirror`], starting on the screen `replay` (a
/// screen-only snapshot) rebuilds.
fn mirror_of(cols: u16, rows: u16, replay: &[u8]) -> Emulator {
    let mut mirror = Emulator::new(cols, rows, 0);
    mirror.process(replay);
    drop(mirror.take_replies());
    mirror
}

impl Worker {
    fn run(mut self) {
        // Once stopping: how many more events to write (those queued when
        // the stop was seen). Checked before every event, as under
        // sustained output the queue never runs empty.
        let mut stopping: Option<usize> = None;
        let finished = loop {
            match (stopping, self.control.load(Ordering::Acquire)) {
                (Some(0), _) => break false,
                (Some(left), _) => stopping = Some(left - 1),
                (None, STOP) => {
                    stopping = Some(self.events.len());
                    continue;
                }
                (None, FREEZE) => {
                    // The next process resumes from here.
                    let _ = self.writer.flush();
                    return;
                }
                (None, _) => {}
            }
            match self.events.try_recv() {
                Ok(SessionEvent::Output(bytes)) => {
                    let text = self.writer.output_text(&bytes);
                    if !text.is_empty() && !self.record_output(&text) {
                        break true;
                    }
                }
                Ok(SessionEvent::Resize { cols, rows }) => {
                    // `[t,"r","65535x65535"]`: inside the overhead.
                    if !self.room_for(0) && !self.fresh() && !self.rotate() {
                        break true;
                    }
                    if let Err(e) = self.writer.resize(cols, rows) {
                        break self.failed(&e);
                    }
                    self.mirror.resize(cols, rows);
                }
                Ok(SessionEvent::Exit(info)) => {
                    let _ = self
                        .writer
                        .marker(&exit_label(info.code, info.signal.as_deref()));
                    break true;
                }
                Err(broadcast::error::TryRecvError::Closed) => break true,
                Err(broadcast::error::TryRecvError::Lagged(_)) if stopping.is_some() => {
                    break false;
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    // Output was lost: carry on from the screen as it is
                    // (in the next part when the screen would not fit),
                    // marked as a resync.
                    let room = self.fresh()
                        || self.room_for(self.screen_estimate().saturating_mul(6))
                        || self.rotate();
                    if !room || !self.resync() {
                        break true;
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    if stopping.is_some() {
                        break false;
                    }
                    if let Err(e) = self.writer.flush() {
                        break self.failed(&e);
                    }
                    std::thread::sleep(POLL);
                }
            }
        };
        let _ = self.writer.finish();
        if finished && let Some(owner) = self.owner.upgrade() {
            let (id, file) = {
                let entry = self.entry.lock().unwrap_or_else(PoisonError::into_inner);
                (entry.session_id.clone(), entry.file.clone())
            };
            if owner.ended(&id, self.generation) {
                (self.ended)(&id, &file);
            }
        }
    }

    /// Write `text` (output) as events: in this part while it fits, else
    /// from the start of the next one, which opens on the screen as it
    /// was just before `text`. Text too big for even a fresh part (a PTY
    /// read is up to 64 KiB, more once escaped) is cut: what fits fills a
    /// part, the rest goes on in the next. False when the recording ends
    /// here.
    fn record_output(&mut self, text: &str) -> bool {
        let mut rest = text;
        loop {
            let line = match self.writer.text_line(rest) {
                Ok(line) => line,
                Err(e) => return !self.failed(&e),
            };
            if self.room_for(line.len()) {
                if let Err(e) = self.writer.line(&line) {
                    return !self.failed(&e);
                }
                self.mirror_output(rest);
                return true;
            }
            if self.fresh() {
                let budget = self.room_left().saturating_sub(EVENT_OVERHEAD);
                let head = fitting_prefix(rest, budget);
                let wrote = self
                    .writer
                    .text_line(head)
                    .and_then(|line| self.writer.line(&line));
                if let Err(e) = wrote {
                    return !self.failed(&e);
                }
                self.mirror_output(head);
                rest = &rest[head.len()..];
                if rest.is_empty() {
                    return true;
                }
            }
            if !self.rotate() {
                return false;
            }
        }
    }

    fn mirror_output(&mut self, text: &str) {
        self.mirror.process(text.as_bytes());
        // Queries are the session's to answer (its own emulator does).
        drop(self.mirror.take_replies());
    }

    /// Whether `len` more bytes still fit the part, leaving room for the
    /// marker that ends it.
    fn room_for(&self, len: usize) -> bool {
        u64::try_from(len).is_ok_and(|len| len <= self.room_left())
    }

    /// Bytes an event line can still take, leaving room for the marker
    /// that ends the part.
    fn room_left(&self) -> u64 {
        self.max_file_bytes()
            .saturating_sub(self.writer.bytes)
            .saturating_sub(1 + EVENT_OVERHEAD + END_RESERVE)
    }

    /// Nothing but the header and snapshot yet: rotating again would make
    /// no progress.
    fn fresh(&self) -> bool {
        self.writer.bytes <= self.writer.opening
    }

    /// An upper bound on a screen-only snapshot of the session.
    fn screen_estimate(&self) -> usize {
        self.session.upgrade().map_or(0, |s| {
            let (cols, rows) = s.size();
            // A cell is a character plus its attribute escapes.
            usize::from(cols) * usize::from(rows) * 24
        })
    }

    fn max_file_bytes(&self) -> u64 {
        self.owner
            .upgrade()
            .map_or(DEFAULT_MAX_FILE_BYTES, |o| o.limits().max_file_bytes)
    }

    fn failed(&self, e: &std::io::Error) -> bool {
        eprintln!(
            "fw hub: recording {}: {e}; recording stopped",
            self.writer.path.display()
        );
        true
    }

    /// Re-subscribe from a fresh screen snapshot (after falling behind).
    fn resync(&mut self) -> bool {
        let Some(session) = self.session.upgrade() else {
            return false;
        };
        let attachment = session.attach_with(0);
        self.events = attachment.events;
        self.mirror = mirror_of(attachment.cols, attachment.rows, &attachment.replay);
        let wrote = self.writer.marker("resync").and_then(|()| {
            self.writer.resize(attachment.cols, attachment.rows)?;
            match self.writer.output_line(&attachment.replay)? {
                Some(line) => self.writer.line(&line),
                None => Ok(()),
            }
        });
        wrote.is_ok() && !self.ended_meanwhile(attachment.exit.as_ref())
    }

    /// After re-subscribing: a process that had already exited sends no
    /// exit event to the new receiver, so its marker is written here and
    /// the recording ends (true).
    fn ended_meanwhile(&mut self, exit: Option<&crate::pty::ExitInfo>) -> bool {
        let Some(info) = exit else {
            return false;
        };
        let _ = self
            .writer
            .marker(&exit_label(info.code, info.signal.as_deref()));
        true
    }

    /// The file reached its cap: carry on in the next part, opening on
    /// the screen the recording has drawn so far (the same receiver goes
    /// on: no queued output is skipped). False when the recording ends
    /// here (it failed).
    fn rotate(&mut self) -> bool {
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        let snapshot = self.mirror.snapshot(0);
        let (cols, rows) = self.mirror.size();
        let cols = u16::try_from(cols).unwrap_or(u16::MAX);
        let rows = u16::try_from(rows).unwrap_or(u16::MAX);
        let next = {
            let entry = self.entry.lock().unwrap_or_else(PoisonError::into_inner);
            let part = entry.part + 1;
            ActiveEntry {
                file: part_file(&entry.base, part),
                part,
                started_ms: now_ms(),
                ..entry.clone()
            }
        };
        // A UTF-8 sequence the last output began goes on in the next part.
        let pending = std::mem::take(&mut self.writer.pending);
        let _ = self.writer.marker(&format!("continued in {}", next.file));
        let _ = self.writer.finish();
        let path = owner.dir.join(&next.file);
        let created = CastWriter::create(&path, cols, rows, &next).and_then(|mut writer| {
            writer.snapshot(&snapshot)?;
            writer.flush()?;
            Ok(writer)
        });
        match created {
            Ok(writer) => {
                self.writer = writer;
                self.writer.pending = pending;
                *self.entry.lock().unwrap_or_else(PoisonError::into_inner) = next;
                owner.save_active();
                owner.sweep();
                true
            }
            Err(e) => {
                self.failed(&e);
                false
            }
        }
    }
}

/// The longest prefix of `text` whose JSON-escaped form takes at most
/// `budget` bytes -- never empty, so a part always makes progress.
fn fitting_prefix(text: &str, budget: u64) -> &str {
    let mut used = 0u64;
    for (at, c) in text.char_indices() {
        let escaped = match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if u32::from(c) < 0x20 => 6,
            c => c.len_utf8() as u64,
        };
        used += escaped;
        if used > budget {
            let end = if at == 0 { c.len_utf8() } else { at };
            return &text[..end];
        }
    }
    text
}

/// An asciinema v2 file being written.
struct CastWriter {
    path: PathBuf,
    file: BufWriter<fs::File>,
    /// Unix milliseconds event times count from.
    started_ms: u64,
    /// Seconds of the last event (times never go backwards).
    last: f64,
    /// Bytes in the file.
    bytes: u64,
    /// Bytes its header and opening snapshot take.
    opening: u64,
    /// The start of a UTF-8 sequence the next output completes.
    pending: Vec<u8>,
}

impl CastWriter {
    fn create(path: &Path, cols: u16, rows: u16, entry: &ActiveEntry) -> std::io::Result<Self> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let header = json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": entry.started_ms / 1000,
            "title": entry.title,
            "env": { "TERM": "xterm-256color" },
            "x_fw_hub": { "session_id": entry.session_id, "part": entry.part },
        });
        let mut writer = Self {
            path: path.to_owned(),
            file: BufWriter::new(file),
            started_ms: entry.started_ms,
            last: 0.0,
            bytes: 0,
            opening: 0,
            pending: Vec::new(),
        };
        writer.line(&header.to_string())?;
        Ok(writer)
    }

    /// Reopen a recording to carry on (a live handoff): times continue
    /// from its last event.
    fn append(path: &Path, started_ms: u64) -> std::io::Result<Self> {
        let mut file = fs::OpenOptions::new().read(true).append(true).open(path)?;
        let len = file.metadata()?.len();
        let last = last_event_time(&mut file, len).unwrap_or(0.0);
        // A torn last line (the writer died mid-write) is ended so the next
        // event starts a line of its own.
        let torn = len > 0 && {
            file.seek(SeekFrom::Start(len - 1))?;
            let mut byte = [0u8; 1];
            file.read_exact(&mut byte)?;
            byte[0] != b'\n'
        };
        let mut writer = Self {
            path: path.to_owned(),
            file: BufWriter::new(file),
            started_ms,
            last,
            bytes: len,
            opening: 0,
            pending: Vec::new(),
        };
        if torn {
            writer.file.write_all(b"\n")?;
            writer.bytes += 1;
        }
        Ok(writer)
    }

    fn time(&mut self) -> f64 {
        let elapsed = now_ms().saturating_sub(self.started_ms);
        // Milliseconds as seconds: exact in an f64 for any recording length.
        #[allow(clippy::cast_precision_loss)]
        let t = elapsed as f64 / 1000.0;
        self.last = self.last.max(t);
        self.last
    }

    fn line(&mut self, text: &str) -> std::io::Result<()> {
        self.file.write_all(text.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.bytes += text.len() as u64 + 1;
        Ok(())
    }

    fn event(&mut self, code: &str, data: &str) -> std::io::Result<()> {
        let t = self.time();
        let line = serde_json::to_string(&(t, code, data)).map_err(std::io::Error::other)?;
        self.line(&line)
    }

    #[cfg(test)]
    fn output(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.output_line(bytes)? {
            Some(line) => self.line(&line),
            None => Ok(()),
        }
    }

    /// The event line for `bytes` of output, not yet written (`None`: it
    /// only began a UTF-8 sequence).
    fn output_line(&mut self, bytes: &[u8]) -> std::io::Result<Option<String>> {
        let text = self.output_text(bytes);
        if text.is_empty() {
            return Ok(None);
        }
        self.text_line(&text).map(Some)
    }

    /// The text of `bytes` of output, a UTF-8 sequence they begin held
    /// back for the next.
    fn output_text(&mut self, bytes: &[u8]) -> String {
        take_text(&mut self.pending, bytes)
    }

    /// The output event line for `text`, timed now.
    fn text_line(&mut self, text: &str) -> std::io::Result<String> {
        let t = self.time();
        serde_json::to_string(&(t, "o", text)).map_err(std::io::Error::other)
    }

    /// The screen the recording opens on, at t=0 (the moment its part
    /// started).
    fn snapshot(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let line = serde_json::to_string(&(0.0, "o", text)).map_err(std::io::Error::other)?;
        self.line(&line)?;
        self.opening = self.bytes;
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.flush_pending()?;
        self.event("r", &format!("{cols}x{rows}"))
    }

    fn marker(&mut self, label: &str) -> std::io::Result<()> {
        self.flush_pending()?;
        self.event("m", label)
    }

    fn flush_pending(&mut self) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
        self.event("o", &text)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }

    fn finish(&mut self) -> std::io::Result<()> {
        self.flush_pending()?;
        self.file.flush()?;
        self.file.get_ref().sync_data()
    }
}

/// The valid UTF-8 of `pending` + `bytes` (invalid bytes as U+FFFD),
/// keeping an incomplete sequence at the end in `pending`.
fn take_text(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    let mut out = String::new();
    let mut rest: &[u8] = pending;
    let keep = loop {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                break Vec::new();
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                match e.error_len() {
                    Some(n) => {
                        out.push('\u{FFFD}');
                        rest = &after[n..];
                    }
                    None => break after.to_vec(),
                }
            }
        }
    };
    *pending = keep;
    out
}

/// The time of the last complete event line in the file's tail.
fn last_event_time(file: &mut fs::File, len: u64) -> Option<f64> {
    let from = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut tail = Vec::new();
    file.take(TAIL).read_to_end(&mut tail).ok()?;
    tail.split(|&b| b == b'\n')
        .rev()
        .filter(|line| line.first() == Some(&b'['))
        .find_map(|line| {
            let value: Value = serde_json::from_slice(line).ok()?;
            value.get(0)?.as_f64()
        })
}

impl From<RecordError> for HubError {
    fn from(e: RecordError) -> Self {
        match e {
            RecordError::Invalid(m) => StateError::Invalid(m).into(),
            RecordError::NotFound(m) => StateError::NotFound(m).into(),
            RecordError::Conflict(m) => StateError::Conflict(m).into(),
            e @ RecordError::Io { .. } => HubError::Internal(e.to_string()),
        }
    }
}

impl Hub {
    /// Start recording session `id` (it must be running here): the
    /// recording, and whether it started now (`false`: already recording).
    pub fn start_recording(self: &Arc<Self>, id: &str) -> Result<(RecordingInfo, bool), HubError> {
        let view = self.session(id)?;
        let Some(session) = self.live_session(&view.record.id) else {
            return Err(StateError::Conflict(format!(
                "session {} has no terminal on this hub: nothing to record",
                view.record.name
            ))
            .into());
        };
        let workspace = self
            .workspace(&view.record.workspace_id)
            .map(|w| w.name)
            .unwrap_or_default();
        let title = format!("{workspace}/{}", view.record.name);
        let started = self.recordings().start(
            &view.record.id,
            &view.record.name,
            &title,
            &session,
            self.recording_ended_notice(),
        )?;
        self.emit(HubEvent::Sessions);
        if started.1 {
            self.emit_lifecycle(
                "record.started",
                json!({
                    "session": self.event_session(&view.record.id),
                    "recording": started.0,
                }),
            );
        }
        Ok(started)
    }

    /// Stop recording session `id`.
    pub fn stop_recording(&self, id: &str) -> Result<RecordingInfo, HubError> {
        let view = self.session(id)?;
        let info = self.recordings().stop(&view.record.id)?;
        self.emit(HubEvent::Sessions);
        self.emit_lifecycle(
            "record.stopped",
            json!({
                "session": self.event_session(&view.record.id),
                "recording": info,
                "reason": "stopped",
            }),
        );
        Ok(info)
    }

    /// What a recording that ends by itself (its process exited, or
    /// writing failed) tells: the UIs, and plugins (`record.stopped`,
    /// reason `ended`).
    pub(crate) fn recording_ended_notice(self: &Arc<Self>) -> EndNotice {
        let weak = Arc::downgrade(self);
        Arc::new(move |id: &str, file: &str| {
            if let Some(hub) = weak.upgrade() {
                hub.emit(HubEvent::Sessions);
                let recording = hub
                    .recordings()
                    .info_of(file)
                    .map_or(Value::Null, |info| json!(info));
                hub.emit_lifecycle(
                    "record.stopped",
                    json!({
                        "session": hub.event_session(id),
                        "recording": recording,
                        "reason": "ended",
                    }),
                );
            }
        })
    }

    /// Carry on the recordings a handoff froze (this hub's own after a
    /// rollback, or the old hub's in the new one), before the sessions
    /// resume.
    pub(crate) fn resume_recordings(self: &Arc<Self>) {
        let notice = self.recording_ended_notice();
        let hub = Arc::clone(self);
        self.recordings()
            .resume(move |id| hub.live_session(id), &notice);
    }
}

/// A parsed asciinema v2 recording.
#[derive(Debug, Clone)]
pub struct Cast {
    pub header: Value,
    /// `(time, code, data)` in file order.
    pub events: Vec<(f64, String, String)>,
}

impl Cast {
    /// Every output event's data, joined.
    pub fn output(&self) -> String {
        self.events
            .iter()
            .filter(|e| e.1 == "o")
            .map(|e| e.2.as_str())
            .collect()
    }
}

/// Parse an asciinema v2 file: a header object line, then one
/// `[time, code, data]` array per line.
pub fn parse_cast(text: &str) -> Result<Cast, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header: Value = serde_json::from_str(lines.next().ok_or("empty recording")?)
        .map_err(|e| format!("header: {e}"))?;
    if header["version"] != 2 {
        return Err(format!("not asciinema v2: version {}", header["version"]));
    }
    for key in ["width", "height"] {
        if header[key].as_u64().is_none() {
            return Err(format!("header has no {key}"));
        }
    }
    let mut events = Vec::new();
    for (n, line) in lines.enumerate() {
        let (t, code, data): (f64, String, String) =
            serde_json::from_str(line).map_err(|e| format!("event {}: {e}", n + 1))?;
        events.push((t, code, data));
    }
    Ok(Cast { header, events })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_and_round_trip() {
        assert_eq!(parse_size("65536").unwrap(), 65536);
        assert_eq!(parse_size("64KiB").unwrap(), 64 * 1024);
        assert_eq!(parse_size("64 mb").unwrap(), 64 << 20);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        for bad in ["", "MiB", "12xb", "-1", "1.5MiB", "99999999999999TiB"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
        for bytes in [65536, 64 << 20, 3 << 30, 1 << 40, 100_000] {
            assert_eq!(parse_size(&format_size(bytes)).unwrap(), bytes);
        }
        assert_eq!(format_size(64 << 20), "64MiB");
        assert_eq!(format_size(100_000), "100000");
    }

    #[test]
    fn limits_change_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = Recordings::new(dir.path(), RecordLimits::default());
        let tighter = RecordLimits {
            max_file_bytes: MIN_PART_BYTES,
            max_total_bytes: 4 * MIN_PART_BYTES,
        };
        recordings.set_limits(tighter);
        assert_eq!(recordings.limits(), tighter);
    }

    #[test]
    fn split_utf8_sequences_are_joined_and_invalid_bytes_replaced() {
        let mut pending = Vec::new();
        let euro = "\u{20ac}".as_bytes();
        assert_eq!(take_text(&mut pending, &[b'a', euro[0]]), "a");
        assert_eq!(pending, vec![euro[0]]);
        assert_eq!(take_text(&mut pending, &euro[1..]), "\u{20ac}");
        assert!(pending.is_empty());
        assert_eq!(take_text(&mut pending, b"x\xffy"), "x\u{FFFD}y");
    }

    #[test]
    fn files_are_named_for_their_session() {
        assert_eq!(file_label("shell1", "abc"), "shell1");
        assert_eq!(file_label("fix: the ../bug!", "abc"), "fix-the-bug");
        assert_eq!(file_label("...", "0123456789abcdef"), "0123456789ab");
        assert!(valid_name(&part_file(&file_label("a b/c", "x"), 2)));
        assert_eq!(file_label(&"x".repeat(100), "id").len(), 48);
    }

    /// A stop is seen between events, not only once the queue drains:
    /// under sustained output it never does. What was queued when it was
    /// seen is still written.
    #[test]
    fn a_stop_ends_a_recording_under_sustained_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.cast");
        let entry = ActiveEntry {
            session_id: "s".into(),
            file: "x.cast".into(),
            base: "x".into(),
            part: 1,
            started_ms: now_ms(),
            title: "t".into(),
        };
        let mut writer = CastWriter::create(&path, 80, 24, &entry).unwrap();
        writer.snapshot(b"").unwrap();
        let (tx, events) = broadcast::channel(1 << 16);
        let control = Arc::new(AtomicU8::new(RUN));
        let worker = Worker {
            owner: Weak::new(),
            entry: Arc::new(Mutex::new(entry)),
            generation: 0,
            control: Arc::clone(&control),
            session: Weak::new(),
            mirror: Emulator::new(80, 24, 0),
            events,
            writer,
            ended: Arc::new(|_: &str, _: &str| {}),
        };
        let (done_tx, done) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            worker.run();
            let _ = done_tx.send(());
        });
        // A producer that keeps the queue from ever running empty.
        let producing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let producer = {
            let producing = Arc::clone(&producing);
            std::thread::spawn(move || {
                let mut n = 0u64;
                while producing.load(Ordering::Acquire) {
                    if tx.len() < 2000 {
                        n += 1;
                        let _ = tx.send(SessionEvent::Output(axum::body::Bytes::from(format!(
                            "line {n}\n"
                        ))));
                    } else {
                        std::hint::spin_loop();
                    }
                }
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        control.store(STOP, Ordering::Release);
        let stopped = done.recv_timeout(Duration::from_secs(10));
        producing.store(false, Ordering::Release);
        producer.join().unwrap();
        assert!(stopped.is_ok(), "the recorder never saw its stop");
        thread.join().unwrap();
        let cast = parse_cast(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(cast.output().contains("line 1\n"));
    }

    #[test]
    fn a_prefix_fits_its_escaped_budget_and_always_makes_progress() {
        assert_eq!(fitting_prefix("abcdef", 3), "abc");
        assert_eq!(fitting_prefix("a\"b", 3), "a\"");
        assert_eq!(fitting_prefix("\u{1}\u{1}x", 7), "\u{1}");
        assert_eq!(fitting_prefix("\u{20ac}\u{20ac}", 4), "\u{20ac}");
        assert_eq!(fitting_prefix("\u{1}x", 0), "\u{1}", "never empty");
        assert_eq!(fitting_prefix("short", 100), "short");
        let text = "\u{1}".repeat(1000);
        let cut = fitting_prefix(&text, 600);
        assert!(serde_json::to_string(cut).unwrap().len() <= 602);
    }

    /// The parts of the one recording in `recordings`, in part order (by
    /// file name: a part just created has no header on disk yet).
    fn parts_of(recordings: &Recordings) -> Vec<RecordingInfo> {
        let number = |name: &str| {
            name.strip_suffix(".cast")
                .and_then(|stem| stem.rsplit_once(".part"))
                .and_then(|(_, n)| n.parse::<u32>().ok())
                .unwrap_or(1)
        };
        let mut parts = recordings.list().unwrap();
        parts.sort_by_key(|p| number(&p.name));
        parts
    }

    /// The parts read back: `None` while one is still being written (no
    /// header yet, or a torn last line).
    fn read_parts(recordings: &Recordings) -> Option<Vec<(RecordingInfo, Cast)>> {
        parts_of(recordings)
            .into_iter()
            .map(|info| {
                let text = fs::read_to_string(recordings.path_of(&info.name).ok()?).ok()?;
                if !text.ends_with('\n') {
                    return None;
                }
                let cast = parse_cast(&text).ok()?;
                (!cast.events.is_empty()).then_some((info, cast))
            })
            .collect()
    }

    /// The output each part's events carry, after its opening snapshot.
    fn recorded(parts: &[(RecordingInfo, Cast)]) -> String {
        parts
            .iter()
            .flat_map(|(_, cast)| &cast.events[1..])
            .filter(|e| e.1 == "o")
            .map(|e| e.2.as_str())
            .collect()
    }

    /// Wait until the recording's events carry exactly `want`; the parts.
    fn wait_recorded(recordings: &Recordings, want: &str) -> Vec<(RecordingInfo, Cast)> {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(parts) = read_parts(recordings) {
                let got = recorded(&parts);
                if got == want {
                    return parts;
                }
                assert!(
                    want.starts_with(&got),
                    "recorded output is not a prefix of what was fed ({} of {} bytes)",
                    got.len(),
                    want.len()
                );
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the recording never caught up with {} bytes",
                want.len()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The screen a player shows at the end of `cast` (its rows, cursor).
    fn screen_at_end(cast: &Cast, opening_only: bool) -> crate::vt::ScreenText {
        let size = |key: &str| u16::try_from(cast.header[key].as_u64().unwrap()).unwrap();
        let mut term = Emulator::new(size("width"), size("height"), 0);
        let events = if opening_only {
            &cast.events[..1]
        } else {
            &cast.events[..]
        };
        for (_, code, data) in events {
            match code.as_str() {
                "o" => term.process(data.as_bytes()),
                "r" => {
                    let (cols, rows) = data.split_once('x').unwrap();
                    term.resize(cols.parse().unwrap(), rows.parse().unwrap());
                }
                _ => {}
            }
        }
        term.text(crate::vt::TextSource::Visible)
    }

    /// What every recording's parts must hold: the cap, times that never
    /// go back, an opening snapshot at t=0, an end marker naming the next
    /// part (the last: the exit), no resync (nothing was lost), and each
    /// part opening on exactly the screen the one before ended on.
    fn assert_parts_hold(parts: &[(RecordingInfo, Cast)], limits: RecordLimits, exit: &str) {
        let base = base_of(&parts[0].0.name).to_owned();
        let numbers: Vec<u32> = parts.iter().map(|(p, _)| p.part).collect();
        let expected: Vec<u32> = (1..=u32::try_from(parts.len()).unwrap()).collect();
        assert_eq!(numbers, expected);
        for (n, (info, cast)) in parts.iter().enumerate() {
            assert_eq!(base_of(&info.name), base, "{info:?}");
            assert!(info.size <= limits.max_file_bytes, "{info:?}");
            assert!(
                cast.events.windows(2).all(|w| w[0].0 <= w[1].0),
                "{}: times go back",
                info.name
            );
            let opening = &cast.events[0];
            assert_eq!((opening.0, opening.1.as_str()), (0.0, "o"), "{}", info.name);
            let markers: Vec<&str> = cast
                .events
                .iter()
                .filter(|e| e.1 == "m")
                .map(|e| e.2.as_str())
                .collect();
            let label = match parts.get(n + 1) {
                Some((next, _)) => format!("continued in {}", next.name),
                None => exit.to_owned(),
            };
            assert_eq!(markers, vec![label.as_str()], "{}", info.name);
            if let Some((_, next)) = parts.get(n + 1) {
                assert_eq!(
                    screen_at_end(cast, false),
                    screen_at_end(next, true),
                    "{} does not open where {} ended",
                    parts[n + 1].0.name,
                    info.name
                );
            }
        }
    }

    /// A silent child on a 20x5 screen with no history (a snapshot is the
    /// 100 cells of the screen): everything recorded is what the test
    /// feeds it as reads of its PTY. It exits 7 on a line of input.
    fn silent_session() -> Arc<PtySession> {
        PtySession::spawn(
            crate::pty::SpawnSpec {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "stty -echo; read go; exit 7".into(),
                ],
                cwd: std::env::temp_dir(),
                env: Vec::new(),
                env_remove: Vec::new(),
                cols: 20,
                rows: 5,
                scrollback_lines: 0,
            },
            |_| {},
        )
        .unwrap()
    }

    /// Start recording `session`; a receiver of the recording's end.
    fn record(
        recordings: &Arc<Recordings>,
        session: &Arc<PtySession>,
    ) -> std::sync::mpsc::Receiver<()> {
        let (ended_tx, ended) = std::sync::mpsc::channel::<()>();
        let ended_tx = Mutex::new(ended_tx);
        let notice: EndNotice = Arc::new(move |_: &str, _: &str| {
            let _ = ended_tx.lock().unwrap().send(());
        });
        recordings
            .start("s", "big", "big", session, notice)
            .unwrap();
        ended
    }

    fn exit_and_end(session: &PtySession, ended: &std::sync::mpsc::Receiver<()>) {
        session.write(b"go\r".to_vec());
        assert!(session.wait_exit(Duration::from_secs(20)));
        ended
            .recv_timeout(Duration::from_secs(20))
            .expect("the recording ends with the process");
    }

    /// Every part holds its cap, even when one PTY read escapes to more
    /// than a whole part (control bytes take six bytes each in JSON), and
    /// every byte of output is an event of some part, in order: a chunk
    /// that would cross the cap opens the next part (on the screen as it
    /// was before the chunk) and is its first event. Chunks of known
    /// sizes, each handled before the next: the part counts are exact.
    #[test]
    fn output_bigger_than_a_part_is_cut_to_fit() {
        let dir = tempfile::tempdir().unwrap();
        let limits = RecordLimits {
            max_file_bytes: 8 * 1024,
            max_total_bytes: 1 << 30,
        };
        let recordings = Arc::new(Recordings::new(dir.path(), limits));
        let session = silent_session();
        let ended = record(&recordings, &session);
        // Printable chunks of 3000: two fit a part (a small screen keeps
        // its snapshot short); the next opens a part. Then control bytes:
        // 2000 escape to 12000, more than a whole part -- cut to fill one,
        // the rest going on in the next.
        let mut chunks: Vec<String> = "abcdefgh"
            .chars()
            .map(|c| c.to_string().repeat(3000))
            .collect();
        chunks.extend(std::iter::repeat_n("\u{1}".repeat(2000), 3));
        let mut fed = String::new();
        let mut counts = Vec::new();
        for chunk in &chunks {
            session.ingest(axum::body::Bytes::copy_from_slice(chunk.as_bytes()));
            fed.push_str(chunk);
            counts.push(wait_recorded(&recordings, &fed).len());
        }
        exit_and_end(&session, &ended);
        let parts = wait_recorded(&recordings, &fed);
        // Each control chunk meets a part with events (it opens the next),
        // fills that fresh part with its head (about 1200 of its 2000
        // bytes) and goes on in another.
        assert_eq!(counts, vec![1, 1, 2, 2, 3, 3, 4, 4, 6, 8, 10]);
        assert_eq!(parts.len(), 10);
        assert_parts_hold(&parts, limits, "exit: 7");
        // Byte-exact per part: whole chunks while they fit, then cuts.
        for (n, pair) in ["ab", "cd", "ef", "gh"].iter().enumerate() {
            let want: String = pair.chars().map(|c| c.to_string().repeat(3000)).collect();
            assert_eq!(recorded(&parts[n..=n]), want, "part {}", n + 1);
        }
        for (n, pair) in parts[4..].chunks(2).enumerate() {
            assert_eq!(recorded(pair), chunks[8 + n], "control chunk {n}");
            let (head, _) = &pair[0];
            // Filled to the cap, less the room kept for its end marker.
            assert!(
                head.size + 1 + EVENT_OVERHEAD + END_RESERVE + 64 >= limits.max_file_bytes,
                "{head:?}"
            );
            let tail = recorded(&pair[1..]);
            assert!(!tail.is_empty() && tail.len() < 1000, "{}", tail.len());
        }
    }

    /// Output that queues up behind the recorder while it rotates is not
    /// lost: the backlog goes on in the next part, which opens on the
    /// screen the last one ended on (once, the backlog was thrown away and
    /// the next part opened on the session's later screen instead).
    #[test]
    fn a_backlog_goes_on_across_parts_byte_exact() {
        let dir = tempfile::tempdir().unwrap();
        let limits = RecordLimits {
            max_file_bytes: 8 * 1024,
            max_total_bytes: 1 << 30,
        };
        let recordings = Arc::new(Recordings::new(dir.path(), limits));
        let session = silent_session();
        let ended = record(&recordings, &session);
        // `seq 1 40000` (about 230 KB, dozens of parts), fed as fast as
        // it goes in 1000-byte reads (fewer than the session's queue
        // holds: the recorder cannot lag, only fall behind).
        let fed: String = (1..=40_000).map(|n| format!("{n}\r\n")).collect();
        for chunk in fed.as_bytes().chunks(1000) {
            session.ingest(axum::body::Bytes::copy_from_slice(chunk));
        }
        exit_and_end(&session, &ended);
        let parts = wait_recorded(&recordings, &fed);
        assert!(parts.len() > 20, "{}", parts.len());
        assert_parts_hold(&parts, limits, "exit: 7");
        // Every line is in the parts' events, in order.
        let all = recorded(&parts);
        let lines: Vec<&str> = all.split("\r\n").collect();
        assert_eq!(lines.len(), 40_001);
        assert!(
            lines[..40_000]
                .iter()
                .enumerate()
                .all(|(n, l)| *l == (n + 1).to_string())
        );
    }

    #[test]
    fn parts_share_their_base() {
        assert_eq!(base_of("a-1Z.cast"), "a-1Z");
        assert_eq!(base_of("a-1Z.part12.cast"), "a-1Z");
        assert_eq!(base_of(&part_file("x", 3)), "x");
        assert_eq!(base_of("a.partx.cast"), "a.partx");
    }

    #[test]
    fn names_are_plain_cast_files() {
        assert!(valid_name("abc-20261103T101010123Z.cast"));
        assert!(valid_name("abc-1.part2.cast"));
        for bad in ["../x.cast", "x/y.cast", ".cast", "x.txt", "a..b.cast", ""] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_reopened_file_continues_after_its_last_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.cast");
        let entry = ActiveEntry {
            session_id: "s".into(),
            file: "x.cast".into(),
            base: "x".into(),
            part: 1,
            // Started in the future: wall-clock times would be negative.
            started_ms: now_ms() + 60_000,
            title: "t".into(),
        };
        let mut writer = CastWriter::create(&path, 80, 24, &entry).unwrap();
        writer.last = 5.5;
        writer.output(b"one").unwrap();
        writer.finish().unwrap();
        drop(writer);
        let mut again = CastWriter::append(&path, entry.started_ms).unwrap();
        assert!((again.last - 5.5).abs() < f64::EPSILON);
        again.resize(100, 30).unwrap();
        again.output(b"two").unwrap();
        again.finish().unwrap();
        let cast = parse_cast(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cast.header["width"], 80);
        assert_eq!(cast.output(), "onetwo");
        assert!(cast.events.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(cast.events.iter().any(|e| e.1 == "r" && e.2 == "100x30"));
    }
}
