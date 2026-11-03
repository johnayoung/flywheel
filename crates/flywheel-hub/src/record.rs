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
use crate::pty::{PtySession, SessionEvent};
use crate::state::StateError;
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
/// Longest header line read when listing.
const MAX_HEADER: u64 = 64 * 1024;
/// How much of a file's end is read to find its last event time.
const TAIL: u64 = 256 * 1024;

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

/// Told when a recording ended by itself (its process exited), with the
/// session's id.
pub type EndNotice = Arc<dyn Fn(&str) + Send + Sync>;

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
    limits: RecordLimits,
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
            limits,
            recorders: Mutex::new(HashMap::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
            sweeping: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn limits(&self) -> RecordLimits {
        self.limits
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

    /// Start recording session `session_id` (live, running) under `title`.
    /// Already recording: that recording, `false`.
    pub fn start(
        self: &Arc<Self>,
        session_id: &str,
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
        let base = format!("{session_id}-{stamp}");
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
            let recorder = self.spawn(entry, writer, attachment.events, session, ended);
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
        events: broadcast::Receiver<SessionEvent>,
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
            events,
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
        let Some(mut recorder) = self.recorders().remove(id) else {
            return Err(RecordError::Conflict(format!(
                "session {id} is not being recorded"
            )));
        };
        recorder.control.store(STOP, Ordering::Release);
        if let Some(thread) = recorder.thread.take() {
            let _ = thread.join();
        }
        self.save_active();
        let file = recorder
            .entry
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
            let recorder = self.spawn(
                entry,
                writer,
                attachment.events,
                &session,
                Arc::clone(ended),
            );
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
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by_key(|f| f.0);
        for (_, size, path, name) in files {
            if total <= self.limits.max_total_bytes {
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
        list.sort_by(|a, b| {
            b.started
                .cmp(&a.started)
                .then(b.modified.cmp(&a.modified))
                .then(b.name.cmp(&a.name))
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
    events: broadcast::Receiver<SessionEvent>,
    writer: CastWriter,
    ended: EndNotice,
}

impl Worker {
    fn run(mut self) {
        let finished = loop {
            match self.events.try_recv() {
                Ok(SessionEvent::Output(bytes)) => {
                    if let Err(e) = self.writer.output(&bytes) {
                        break self.failed(&e);
                    }
                }
                Ok(SessionEvent::Resize { cols, rows }) => {
                    if let Err(e) = self.writer.resize(cols, rows) {
                        break self.failed(&e);
                    }
                }
                Ok(SessionEvent::Exit(info)) => {
                    let _ = self
                        .writer
                        .marker(&exit_label(info.code, info.signal.as_deref()));
                    break true;
                }
                Err(broadcast::error::TryRecvError::Closed) => break true,
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    // Output was lost: carry on from the screen as it is.
                    if !self.resync() {
                        break true;
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    if let Err(e) = self.writer.flush() {
                        break self.failed(&e);
                    }
                    match self.control.load(Ordering::Acquire) {
                        STOP => break false,
                        FREEZE => {
                            // The next process resumes from here.
                            return;
                        }
                        _ => std::thread::sleep(POLL),
                    }
                }
            }
            if self.writer.bytes >= self.max_file_bytes() && !self.rotate() {
                break true;
            }
        };
        let _ = self.writer.finish();
        if finished && let Some(owner) = self.owner.upgrade() {
            let id = self
                .entry
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .session_id
                .clone();
            if owner.ended(&id, self.generation) {
                (self.ended)(&id);
            }
        }
    }

    fn max_file_bytes(&self) -> u64 {
        self.owner
            .upgrade()
            .map_or(DEFAULT_MAX_FILE_BYTES, |o| o.limits.max_file_bytes)
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
        let wrote = self.writer.marker("resync").and_then(|()| {
            self.writer.resize(attachment.cols, attachment.rows)?;
            self.writer.output(&attachment.replay)
        });
        wrote.is_ok()
    }

    /// The file reached its cap: carry on in the next part, from a fresh
    /// snapshot.
    fn rotate(&mut self) -> bool {
        let (Some(owner), Some(session)) = (self.owner.upgrade(), self.session.upgrade()) else {
            return false;
        };
        let attachment = session.attach_with(0);
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
        let _ = self.writer.marker(&format!("continued in {}", next.file));
        let _ = self.writer.finish();
        let path = owner.dir.join(&next.file);
        let created = CastWriter::create(&path, attachment.cols, attachment.rows, &next).and_then(
            |mut writer| {
                writer.snapshot(&attachment.replay)?;
                writer.flush()?;
                Ok(writer)
            },
        );
        match created {
            Ok(writer) => {
                self.writer = writer;
                self.events = attachment.events;
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

    fn output(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let text = take_text(&mut self.pending, bytes);
        if text.is_empty() {
            return Ok(());
        }
        self.event("o", &text)
    }

    /// The screen the recording opens on, at t=0 (the moment its part
    /// started).
    fn snapshot(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let line = serde_json::to_string(&(0.0, "o", text)).map_err(std::io::Error::other)?;
        self.line(&line)
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
            &title,
            &session,
            self.recording_ended_notice(),
        )?;
        self.emit(HubEvent::Sessions);
        Ok(started)
    }

    /// Stop recording session `id`.
    pub fn stop_recording(&self, id: &str) -> Result<RecordingInfo, HubError> {
        let view = self.session(id)?;
        let info = self.recordings().stop(&view.record.id)?;
        self.emit(HubEvent::Sessions);
        Ok(info)
    }

    /// What a recording that ends by itself (its process exited) tells.
    pub(crate) fn recording_ended_notice(self: &Arc<Self>) -> EndNotice {
        let weak = Arc::downgrade(self);
        Arc::new(move |_id: &str| {
            if let Some(hub) = weak.upgrade() {
                hub.emit(HubEvent::Sessions);
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
