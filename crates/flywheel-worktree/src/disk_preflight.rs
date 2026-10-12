//! Disk/inode preflight for authoritative store writes (spec: phase 07).
//!
//! An authoritative store write that runs out of bytes or inodes mid-flush
//! hard-crashes on `ENOSPC` and can leave the store row half-written. This
//! module gives the worker a cheap, guarded probe it runs *ahead* of such a
//! write: when free space or free inodes are below the configured threshold
//! it records a queryable [`DegradedSpaceRecord`] and declines the write,
//! rather than attempting it and catching the crash after (which is what
//! would leave torn state).
//!
//! The probe (`statvfs(3)` for both bytes and inodes) lives here, in the
//! worker layer -- never in `flywheel_core::task` / `::lifecycle`, which stay
//! pure. The record type it produces is the pure data shape defined in
//! `flywheel_core::store` so the decline surfaces on the same
//! status/telemetry surface every other store record does.
//!
//! Fail-open by construction: if the probe itself cannot run (an unstattable
//! path), the guard proceeds with the write -- inability to *measure* free
//! space must never be mistaken for *lack* of it and silently drop an
//! authoritative write.
//!
//! Port of the Python `flywheel_worktree._disk_preflight` module. The
//! Python issued two system calls per probe (`shutil.disk_usage` plus
//! `os.statvfs`); one `statvfs` carries both dimensions, so this port issues
//! one. [`DiskPreflight`] implements the submitter's [`LedgerWriteGuard`]
//! seam, which is how the merge, phase, pr, and tier-routing strategies wire
//! it in front of every ledger append.

use crate::submitter::{LedgerWriteError, LedgerWriteGuard, Logger};
use chrono::{DateTime, Utc};
use flywheel_core::store::{
    DegradedSpaceRecord, JsonObject, StoreError, TelemetryRecord, TelemetrySink,
};
use serde_json::Value;
use std::ffi::CString;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Default free-byte floor. Deliberately modest -- magnitude is not
/// load-bearing, only the below/above split is. On any healthy host there are
/// orders of magnitude more than 32 MiB and 1024 inodes free, so a default-on
/// preflight never blocks normal operation; the values only trip when a disk
/// is genuinely near-full.
pub const DEFAULT_MIN_FREE_BYTES: u64 = 32 * 1024 * 1024;
/// Default free-inode floor.
pub const DEFAULT_MIN_FREE_INODES: u64 = 1024;

/// Telemetry kind emitted (best-effort) when the preflight declines a write,
/// so the decline also appears in a run's telemetry stream when a sink is
/// wired.
pub const DEGRADED_SPACE_KIND: &str = "worker.disk_space_degraded";

/// The free-inode count reported for a filesystem with no inode concept
/// (`f_files == 0`), so the inode dimension never trips there rather than
/// spuriously degrading. The Python used `sys.maxsize` for the same purpose.
pub const UNLIMITED_INODES: u64 = u64::MAX;

/// Observed free capacity on a filesystem: bytes and inodes.
///
/// `free_inodes` is [`UNLIMITED_INODES`] on filesystems that report no inode
/// total, so the inode dimension simply never trips there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceProbe {
    pub free_bytes: u64,
    pub free_inodes: u64,
}

/// Configurable floor below which a filesystem is considered degraded.
///
/// A write is declined when free bytes fall below `min_free_bytes` OR free
/// inodes fall below `min_free_inodes` -- either dimension is sufficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskThreshold {
    pub min_free_bytes: u64,
    pub min_free_inodes: u64,
}

impl Default for DiskThreshold {
    fn default() -> Self {
        Self {
            min_free_bytes: DEFAULT_MIN_FREE_BYTES,
            min_free_inodes: DEFAULT_MIN_FREE_INODES,
        }
    }
}

/// Probe free bytes and free inodes for the filesystem backing `path` with
/// one `statvfs(3)` call.
///
/// Free bytes are `f_bavail * f_frsize` (the space available to an
/// unprivileged caller, exactly what Python's `shutil.disk_usage().free`
/// reports). Inode reporting degrades gracefully: a filesystem that reports
/// no inode total (`f_files == 0`) yields [`UNLIMITED_INODES`], disabling the
/// inode dimension rather than tripping it. An unstattable path is an
/// `Err`; callers that must fail open catch it.
pub fn probe_path(path: &Path) -> std::io::Result<SpaceProbe> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains an interior NUL byte",
        )
    })?;
    // SAFETY: `statvfs` writes into a caller-provided, correctly sized
    // `statvfs` struct; a zeroed struct is a valid initial value for the
    // out-parameter and the path pointer is a valid NUL-terminated string
    // that outlives the call.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let blocks = widen(st.f_bavail);
    let block_size = widen(st.f_frsize);
    let free_bytes = blocks.saturating_mul(block_size);
    let total_inodes = widen(st.f_files);
    let free_inodes = if total_inodes == 0 {
        UNLIMITED_INODES
    } else {
        widen(st.f_favail)
    };
    Ok(SpaceProbe {
        free_bytes,
        free_inodes,
    })
}

/// A `statvfs` field as `u64`. The field types vary by libc target
/// (`u64`, `u32`, or a signed word), so the widening goes through `TryFrom`
/// and a negative or oversized value reads as zero free capacity.
fn widen<T: TryInto<u64>>(value: T) -> u64 {
    value.try_into().unwrap_or(0)
}

/// Return a [`DegradedSpaceRecord`] when `probe` is below either dimension
/// of `threshold`, else `None` (healthy -- proceed with the write).
pub fn classify(
    probe: SpaceProbe,
    threshold: DiskThreshold,
    path: &Path,
    now: DateTime<Utc>,
    run_id: Option<&str>,
) -> Option<DegradedSpaceRecord> {
    let below_bytes = probe.free_bytes < threshold.min_free_bytes;
    let below_inodes = probe.free_inodes < threshold.min_free_inodes;
    if !(below_bytes || below_inodes) {
        return None;
    }
    Some(DegradedSpaceRecord {
        path: path.to_string_lossy().into_owned(),
        free_bytes: probe.free_bytes,
        free_inodes: probe.free_inodes,
        min_free_bytes: threshold.min_free_bytes,
        min_free_inodes: threshold.min_free_inodes,
        below_bytes,
        below_inodes,
        ts: now,
        run_id: run_id.map(str::to_owned),
    })
}

/// The injectable probe: a path to its observed free capacity.
pub type SpaceProber = Arc<dyn Fn(&Path) -> std::io::Result<SpaceProbe> + Send + Sync>;

/// The injectable clock the degraded-space witness is stamped with.
pub type PreflightClock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Guards an authoritative store write behind a disk/inode probe.
///
/// Default-on and configurable: constructed with [`DiskPreflight::new`] it
/// uses [`DiskThreshold`]'s defaults and the real [`probe_path`]. Tests (and
/// callers wanting a different floor) inject a threshold and/or a probe.
///
/// The declined records accumulate on [`records`](Self::records) -- the
/// queryable status surface a caller reads back -- and are mirrored to the
/// wired [`TelemetrySink`] as a [`TelemetryRecord`] when one is set, so the
/// decline also lands in the run's telemetry stream.
pub struct DiskPreflight {
    pub threshold: DiskThreshold,
    probe: SpaceProber,
    clock: PreflightClock,
    sink: Option<Arc<dyn TelemetrySink + Send + Sync>>,
    log: Option<Logger>,
    records: Mutex<Vec<DegradedSpaceRecord>>,
}

impl Default for DiskPreflight {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DiskPreflight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskPreflight")
            .field("threshold", &self.threshold)
            .field("sink", &self.sink.is_some())
            .field("log", &self.log.is_some())
            .field("records", &self.records())
            .finish_non_exhaustive()
    }
}

impl DiskPreflight {
    /// The default-on preflight: default thresholds, the real probe, the
    /// wall clock, no sink, no log.
    pub fn new() -> Self {
        Self {
            threshold: DiskThreshold::default(),
            probe: Arc::new(probe_path),
            clock: Arc::new(Utc::now),
            sink: None,
            log: None,
            records: Mutex::new(Vec::new()),
        }
    }

    pub fn with_threshold(mut self, threshold: DiskThreshold) -> Self {
        self.threshold = threshold;
        self
    }

    pub fn with_probe(mut self, probe: SpaceProber) -> Self {
        self.probe = probe;
        self
    }

    pub fn with_clock(mut self, clock: PreflightClock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_sink(mut self, sink: Arc<dyn TelemetrySink + Send + Sync>) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_log(mut self, log: Logger) -> Self {
        self.log = Some(log);
        self
    }

    /// Every degraded-space witness this preflight has recorded, oldest
    /// first.
    pub fn records(&self) -> Vec<DegradedSpaceRecord> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn log(&self, message: String) {
        if let Some(log) = &self.log {
            log(&message);
        }
    }

    /// Probe `path` and, if degraded, record + return the witness; otherwise
    /// return `None`.
    ///
    /// Fail-open: a probe that fails (an unstattable path) is treated as
    /// "cannot measure, so do not block" and returns `None` -- the write
    /// proceeds.
    pub fn check(&self, path: &Path, run_id: Option<&str>) -> Option<DegradedSpaceRecord> {
        let probe = match (self.probe)(path) {
            Ok(probe) => probe,
            Err(err) => {
                self.log(format!(
                    "disk preflight could not probe {} ({}: {err}); proceeding with write",
                    py_repr(&path.to_string_lossy()),
                    os_error_name(&err)
                ));
                return None;
            }
        };
        let record = classify(probe, self.threshold, path, (self.clock)(), run_id)?;
        self.record(&record);
        Some(record)
    }

    /// Run `do_write` only when `path` has adequate free space/inodes.
    ///
    /// Returns `Some(do_write())` when healthy. When degraded it records the
    /// witness, skips `do_write` entirely (so no torn store row is possible),
    /// and returns `None`.
    pub fn guard_write<T>(
        &self,
        path: &Path,
        run_id: Option<&str>,
        do_write: impl FnOnce() -> T,
    ) -> Option<T> {
        if self.check(path, run_id).is_some() {
            return None;
        }
        Some(do_write())
    }

    fn record(&self, record: &DegradedSpaceRecord) {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(record.clone());
        let dims = [
            ("bytes", record.below_bytes),
            ("inodes", record.below_inodes),
        ]
        .iter()
        .filter(|(_, tripped)| *tripped)
        .map(|(dim, _)| *dim)
        .collect::<Vec<_>>()
        .join(", ");
        self.log(format!(
            "disk preflight: declining authoritative store write for {} (degraded: {dims}; \
             free_bytes={} < {}, free_inodes={} < {})",
            py_repr(&record.path),
            record.free_bytes,
            record.min_free_bytes,
            record.free_inodes,
            record.min_free_inodes
        ));
        if let Some(sink) = &self.sink
            && let Err(err) = sink.append_telemetry(&telemetry_record(record))
        {
            self.log(format!(
                "disk preflight: could not mirror the degraded-space witness to telemetry ({err})"
            ));
        }
    }
}

impl LedgerWriteGuard for DiskPreflight {
    /// Decline the append when `repo_root`'s filesystem is degraded,
    /// recording the witness and never invoking `append`; otherwise run it.
    fn guard(
        &self,
        repo_root: &Path,
        run_id: &str,
        append: &mut dyn FnMut() -> Result<(), StoreError>,
    ) -> Result<(), LedgerWriteError> {
        if let Some(record) = self.check(repo_root, Some(run_id)) {
            let dims = [
                ("bytes", record.below_bytes),
                ("inodes", record.below_inodes),
            ]
            .iter()
            .filter(|(_, tripped)| *tripped)
            .map(|(dim, _)| *dim)
            .collect::<Vec<_>>()
            .join(", ");
            return Err(LedgerWriteError::Declined(format!(
                "free space on {} is degraded ({dims}): free_bytes={} < {}, free_inodes={} < {}",
                py_repr(&record.path),
                record.free_bytes,
                record.min_free_bytes,
                record.free_inodes,
                record.min_free_inodes
            )));
        }
        append().map_err(LedgerWriteError::Store)
    }
}

/// The telemetry mirror of a degraded-space witness: the same fields under
/// [`DEGRADED_SPACE_KIND`], keyed by the declined write's run (empty when the
/// write was not run-scoped).
pub fn telemetry_record(record: &DegradedSpaceRecord) -> TelemetryRecord {
    let mut payload = JsonObject::new();
    payload.insert("path".into(), Value::String(record.path.clone()));
    payload.insert("free_bytes".into(), Value::from(record.free_bytes));
    payload.insert("free_inodes".into(), Value::from(record.free_inodes));
    payload.insert("min_free_bytes".into(), Value::from(record.min_free_bytes));
    payload.insert(
        "min_free_inodes".into(),
        Value::from(record.min_free_inodes),
    );
    payload.insert("below_bytes".into(), Value::Bool(record.below_bytes));
    payload.insert("below_inodes".into(), Value::Bool(record.below_inodes));
    let mut telemetry = TelemetryRecord::new(
        record.run_id.clone().unwrap_or_default(),
        record.ts,
        DEGRADED_SPACE_KIND,
    );
    telemetry.payload = payload;
    telemetry
}

/// Python's `repr()` of a `str`, so the preflight's log lines match the
/// Python worker's byte for byte: single-quoted unless the text contains a
/// single quote and no double quote, with backslashes, the chosen quote, and
/// control characters escaped.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The Python exception class name `shutil.disk_usage` would have raised
/// for an I/O error of this kind.
fn os_error_name(err: &std::io::Error) -> &'static str {
    match err.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        std::io::ErrorKind::NotADirectory => "NotADirectoryError",
        _ => "OSError",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submitter::GitWorktreeSubmitter;
    use flywheel_core::events::{
        DomainEvent, DomainEventPayload, LandingParked, LifecycleInitialized,
    };
    use flywheel_core::store::DomainEventStore;
    use flywheel_core::store_memory::InMemoryStore;
    use std::path::PathBuf;

    /// Comfortably-above-threshold capacity used by the "healthy" probes.
    const AMPLE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
    const AMPLE_INODES: u64 = 10_000_000;

    /// A probe that ignores the path and reports fixed free capacity, so a
    /// test can simulate a near-full disk without actually filling one.
    fn fixed_probe(free_bytes: u64, free_inodes: u64) -> SpaceProber {
        Arc::new(move |_path: &Path| {
            Ok(SpaceProbe {
                free_bytes,
                free_inodes,
            })
        })
    }

    fn capture_log() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: Logger = Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned()));
        (log, lines)
    }

    /// A ledger seeded with a lifecycle for `run_id` so a witness can append.
    fn ledger(run_id: &str) -> Arc<InMemoryStore> {
        let store = InMemoryStore::new();
        let seed = DomainEvent::new(
            run_id,
            Utc::now(),
            DomainEventPayload::LifecycleInitialized(LifecycleInitialized {
                task_id: "t1".into(),
                ..LifecycleInitialized::default()
            }),
        );
        store.append_domain_event(&seed, 0).unwrap();
        Arc::new(store)
    }

    fn parked(store: &InMemoryStore, run_id: &str) -> Vec<LandingParked> {
        store
            .list_domain_events(run_id)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                DomainEventPayload::LandingParked(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    /// A submitter wired to a ledger and an injected preflight.
    /// `record_landing_park` touches only the store, the guard, `repo_root`,
    /// and the log -- no git -- so bare temp paths suffice.
    fn submitter(
        root: &Path,
        store: Arc<InMemoryStore>,
        preflight: Arc<DiskPreflight>,
    ) -> GitWorktreeSubmitter {
        GitWorktreeSubmitter::new(
            root,
            root.join("tasks"),
            root.join("worktrees"),
            "main",
            root.join(".merge.lock"),
            Arc::new(|_m: &str| {}),
        )
        .with_store(store)
        .with_ledger_guard(preflight)
    }

    fn park(kind: &str) -> LandingParked {
        LandingParked {
            park_kind: kind.to_owned(),
            detail: "base diverged".to_owned(),
            ..LandingParked::default()
        }
    }

    // --- graded cases --------------------------------------------------------------

    #[test]
    fn below_threshold_degrades_and_skips_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ledger("run-below");
        let preflight =
            Arc::new(DiskPreflight::new().with_probe(fixed_probe(1024 * 1024, AMPLE_INODES)));
        let s = submitter(tmp.path(), Arc::clone(&store), Arc::clone(&preflight));

        s.record_landing_park("run-below", &park("divergent-base"));

        // The crashing write was declined BEFORE it was attempted.
        assert!(parked(&store, "run-below").is_empty());
        assert_eq!(store.list_domain_events("run-below").unwrap().len(), 1);
        // ... and the decline is queryable on the worker's status surface.
        let records = preflight.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].below_bytes);
        assert!(!records[0].below_inodes);
        assert_eq!(records[0].run_id.as_deref(), Some("run-below"));
        assert_eq!(records[0].path, tmp.path().to_string_lossy());
    }

    #[test]
    fn above_threshold_proceeds() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ledger("run-above");
        let preflight =
            Arc::new(DiskPreflight::new().with_probe(fixed_probe(AMPLE_BYTES, AMPLE_INODES)));
        let s = submitter(tmp.path(), Arc::clone(&store), Arc::clone(&preflight));

        s.record_landing_park("run-above", &park("divergent-base"));

        let parks = parked(&store, "run-above");
        assert_eq!(parks.len(), 1);
        assert_eq!(parks[0].park_kind, "divergent-base");
        assert!(preflight.records().is_empty());
    }

    // --- classifier edges: either dimension trips ----------------------------------

    #[test]
    fn inode_exhaustion_with_bytes_free_degrades() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ledger("run-inode");
        let preflight = Arc::new(DiskPreflight::new().with_probe(fixed_probe(AMPLE_BYTES, 8)));
        let s = submitter(tmp.path(), Arc::clone(&store), Arc::clone(&preflight));

        s.record_landing_park("run-inode", &park("divergent-base"));

        assert!(parked(&store, "run-inode").is_empty());
        let records = preflight.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].below_inodes);
        assert!(!records[0].below_bytes);
    }

    #[test]
    fn bytes_exhaustion_with_inodes_free_degrades() {
        let record = classify(
            SpaceProbe {
                free_bytes: 16,
                free_inodes: AMPLE_INODES,
            },
            DiskThreshold::default(),
            Path::new("/data"),
            Utc::now(),
            None,
        )
        .unwrap();
        assert!(record.below_bytes);
        assert!(!record.below_inodes);
        assert_eq!(record.path, "/data");
        assert_eq!(record.run_id, None);
        assert_eq!(record.min_free_bytes, DEFAULT_MIN_FREE_BYTES);
    }

    #[test]
    fn healthy_probe_classifies_as_none() {
        assert!(
            classify(
                SpaceProbe {
                    free_bytes: AMPLE_BYTES,
                    free_inodes: AMPLE_INODES,
                },
                DiskThreshold::default(),
                Path::new("/data"),
                Utc::now(),
                None,
            )
            .is_none()
        );
        // Exactly at threshold is healthy: the comparison is strictly below.
        assert!(
            classify(
                SpaceProbe {
                    free_bytes: DEFAULT_MIN_FREE_BYTES,
                    free_inodes: DEFAULT_MIN_FREE_INODES,
                },
                DiskThreshold::default(),
                Path::new("/data"),
                Utc::now(),
                None,
            )
            .is_none()
        );
    }

    // --- direct guard contract -------------------------------------------------------

    #[test]
    fn guard_skips_write_below_threshold() {
        let preflight = DiskPreflight::new().with_probe(fixed_probe(1, AMPLE_INODES));
        let calls = Mutex::new(Vec::new());
        let result = preflight.guard_write(Path::new("/x"), None, || {
            calls.lock().unwrap().push("wrote");
            "ok"
        });
        assert_eq!(result, None);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(preflight.records().len(), 1);
    }

    #[test]
    fn guard_reaches_write_above_threshold() {
        let preflight = DiskPreflight::new().with_probe(fixed_probe(AMPLE_BYTES, AMPLE_INODES));
        let calls = Mutex::new(Vec::new());
        let result = preflight.guard_write(Path::new("/x"), None, || {
            calls.lock().unwrap().push("wrote");
            "ok"
        });
        assert_eq!(result, Some("ok"));
        assert_eq!(*calls.lock().unwrap(), vec!["wrote"]);
        assert!(preflight.records().is_empty());
    }

    #[test]
    fn ledger_guard_declines_with_a_branded_error_and_never_appends() {
        let (log, lines) = capture_log();
        let preflight = DiskPreflight::new()
            .with_probe(fixed_probe(1, 2))
            .with_log(log);
        let mut appended = false;
        let err = LedgerWriteGuard::guard(&preflight, Path::new("/x"), "run-1", &mut || {
            appended = true;
            Ok(())
        })
        .unwrap_err();
        assert!(!appended);
        let message = err.to_string();
        assert!(message.starts_with("ledger write declined: "), "{message}");
        assert!(message.contains("bytes, inodes"), "{message}");
        let logged = lines.lock().unwrap().join("\n");
        assert_eq!(
            logged,
            format!(
                "disk preflight: declining authoritative store write for '/x' (degraded: bytes, \
                 inodes; free_bytes=1 < {DEFAULT_MIN_FREE_BYTES}, free_inodes=2 < \
                 {DEFAULT_MIN_FREE_INODES})"
            )
        );
        // Healthy: the append runs and its own error passes through.
        let healthy = DiskPreflight::new().with_probe(fixed_probe(AMPLE_BYTES, AMPLE_INODES));
        let mut appended = false;
        LedgerWriteGuard::guard(&healthy, Path::new("/x"), "run-1", &mut || {
            appended = true;
            Ok(())
        })
        .unwrap();
        assert!(appended);
        let store_err = LedgerWriteGuard::guard(&healthy, Path::new("/x"), "run-1", &mut || {
            Err(StoreError::Io(std::io::Error::other("boom")))
        })
        .unwrap_err();
        assert!(
            matches!(store_err, LedgerWriteError::Store(_)),
            "{store_err}"
        );
    }

    #[test]
    fn probe_failure_is_fail_open() {
        let (log, lines) = capture_log();
        let boom: SpaceProber = Arc::new(|_path: &Path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot stat",
            ))
        });
        let preflight = DiskPreflight::new().with_probe(boom).with_log(log);
        let calls = Mutex::new(Vec::new());
        let result = preflight.guard_write(Path::new("/gone"), None, || {
            calls.lock().unwrap().push("wrote");
            "ok"
        });
        assert_eq!(result, Some("ok"));
        assert_eq!(*calls.lock().unwrap(), vec!["wrote"]);
        assert!(preflight.records().is_empty());
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            [
                "disk preflight could not probe '/gone' (FileNotFoundError: cannot stat); \
              proceeding with write"
            ]
        );
    }

    // --- telemetry mirror + real probe -------------------------------------------------

    #[test]
    fn degraded_record_mirrors_to_telemetry_sink() {
        struct Sink(Mutex<Vec<TelemetryRecord>>);
        impl TelemetrySink for Sink {
            fn append_telemetry(&self, record: &TelemetryRecord) -> Result<(), StoreError> {
                self.0.lock().unwrap().push(record.clone());
                Ok(())
            }
        }
        let sink = Arc::new(Sink(Mutex::new(Vec::new())));
        let fixed_now = DateTime::parse_from_rfc3339("2026-07-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let preflight = DiskPreflight::new()
            .with_probe(fixed_probe(1, AMPLE_INODES))
            .with_clock(Arc::new(move || fixed_now))
            .with_sink(Arc::clone(&sink) as Arc<dyn TelemetrySink + Send + Sync>);

        let record = preflight.check(Path::new("/x"), Some("run-1")).unwrap();
        assert_eq!(record.ts, fixed_now);

        let emitted = sink.0.lock().unwrap();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].kind, DEGRADED_SPACE_KIND);
        assert_eq!(emitted[0].run_id, "run-1");
        assert_eq!(emitted[0].ts, fixed_now);
        assert_eq!(emitted[0].payload["below_bytes"], Value::Bool(true));
        assert_eq!(emitted[0].payload["below_inodes"], Value::Bool(false));
        assert_eq!(emitted[0].payload["free_bytes"], Value::from(1u64));
        assert_eq!(emitted[0].payload["path"], Value::String("/x".into()));
        assert_eq!(
            emitted[0].payload["min_free_inodes"],
            Value::from(DEFAULT_MIN_FREE_INODES)
        );
        // A write not tied to a run mirrors with an empty run id.
        assert_eq!(telemetry_record(&classify_fixed(None)).run_id, "");
    }

    fn classify_fixed(run_id: Option<&str>) -> DegradedSpaceRecord {
        classify(
            SpaceProbe {
                free_bytes: 1,
                free_inodes: 1,
            },
            DiskThreshold::default(),
            Path::new("/x"),
            Utc::now(),
            run_id,
        )
        .unwrap()
    }

    #[test]
    fn probe_path_returns_positive_capacity() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = probe_path(tmp.path()).unwrap();
        assert!(probe.free_bytes > 0);
        assert!(probe.free_inodes > 0);
        // The real probe on a live tmp dir never trips the default floor.
        let preflight = DiskPreflight::new();
        assert!(preflight.check(tmp.path(), None).is_none());
    }

    #[test]
    fn probe_path_on_an_absent_path_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = probe_path(&tmp.path().join("absent")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let nul = PathBuf::from("a\0b");
        assert_eq!(
            probe_path(&nul).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn default_thresholds_are_modest() {
        assert_eq!(DEFAULT_MIN_FREE_BYTES, 32 * 1024 * 1024);
        assert_eq!(DEFAULT_MIN_FREE_INODES, 1024);
        let threshold = DiskThreshold::default();
        assert_eq!(threshold.min_free_bytes, DEFAULT_MIN_FREE_BYTES);
        assert_eq!(threshold.min_free_inodes, DEFAULT_MIN_FREE_INODES);
        assert_eq!(DiskPreflight::new().threshold, threshold);
        let custom = DiskPreflight::new().with_threshold(DiskThreshold {
            min_free_bytes: 1,
            min_free_inodes: 1,
        });
        assert!(format!("{custom:?}").contains("min_free_bytes: 1"));
    }

    #[test]
    fn py_repr_matches_python_quoting() {
        assert_eq!(py_repr("/x"), "'/x'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("say \"hi\""), "'say \"hi\"'");
        assert_eq!(py_repr("both ' and \""), "'both \\' and \"'");
        assert_eq!(py_repr("a\\b\n"), "'a\\\\b\\n'");
    }
}
