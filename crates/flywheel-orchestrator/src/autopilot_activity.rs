//! Cross-process activity snapshot for the autopilot daemon.
//!
//! The autopilot daemon runs detached from the console that spawned it, so
//! the only channel back to the console's status surface is a file. Each
//! cycle the daemon writes an [`AutopilotActivity`] snapshot here; the
//! console-side supervisor reads it best-effort to show what the daemon is
//! doing *right now* -- which cycle it is on, the last cycle's
//! emitted/dropped counts, and when the next cycle fires.
//!
//! The snapshot doubles as the daemon's cross-process **liveness record**:
//! it carries the daemon `pid` plus an `expires_at` freshness deadline
//! (epoch seconds) that the daemon pushes forward on every write. A second
//! supervisor reads that deadline through [`read_live_activity`] to decide
//! whether a live daemon is already running -- if so it adopts it instead of
//! spawning a duplicate. The staleness rule mirrors the worker lease
//! exactly: a record whose `expires_at` is at/before `now` reads as
//! not-live, just like a lapsed `lease_expires_at`, so a truly dead daemon
//! is respawned rather than adopted forever.
//!
//! This file is a live activity surface only; it is NOT authoritative
//! lifecycle state. A stale file left by a previous daemon is ignored on the
//! read side by pid mismatch, and any malformed/missing/old-schema file
//! reads back as `None`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bump when the on-disk shape changes incompatibly; a reader that sees an
/// unrecognized version treats the file as absent rather than guessing.
pub const ACTIVITY_SCHEMA_VERSION: u64 = 1;

/// The phases the daemon reports. Coarse by design: the console shows
/// per-cycle activity, not intra-pass steps (a single refill pass is one
/// opaque async call). `starting` is the brief window before the first
/// cycle records.
pub const PHASE_STARTING: &str = "starting";
pub const PHASE_RUNNING: &str = "running";
pub const PHASE_IDLE: &str = "idle";
/// Held between cycles by the operator's pause marker
/// ([`pause_marker_path`]): no discovery, no authoring, no emit; the
/// record stays fresh so the daemon still reads live.
pub const PHASE_PAUSED: &str = "paused";
/// Written once on a graceful exit with an already-lapsed deadline, so a
/// stopped daemon reads not-live at once instead of after its grace.
pub const PHASE_STOPPED: &str = "stopped";

/// The most dropped findings a record keeps (the first ones of the cycle).
pub const MAX_DROPPED_SUMMARIES: usize = 20;
/// A dropped finding's reason is cut past this many characters.
pub const MAX_DROPPED_REASON_CHARS: usize = 300;

/// One emitted task's id and tier, for the status line's breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmittedSummary {
    pub task_id: String,
    pub tier: u8,
}

/// One finding the last cycle could not compile into a task, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedSummary {
    pub finding_id: String,
    pub tier: u8,
    pub title: String,
    pub reason: String,
}

impl DroppedSummary {
    /// `reason` cut to [`MAX_DROPPED_REASON_CHARS`] characters.
    pub fn new(finding_id: &str, tier: u8, title: &str, reason: &str) -> Self {
        let cut = |text: &str| -> String {
            if text.chars().count() <= MAX_DROPPED_REASON_CHARS {
                text.to_owned()
            } else {
                let mut kept: String = text.chars().take(MAX_DROPPED_REASON_CHARS).collect();
                kept.push_str("...");
                kept
            }
        };
        Self {
            finding_id: finding_id.to_owned(),
            tier,
            title: cut(title),
            reason: cut(reason),
        }
    }
}

/// One cycle's decision, kept whole: what it emitted and dropped (with
/// reasons) and its tier verdicts. The record carries the last cycle that
/// actually decided something ([`AutopilotActivity::last_decision`]), so a
/// later cycle that only finds the queue at its target does not erase why
/// the last tasks were emitted or findings dropped.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CycleSummary {
    /// The cycle's index (`cycle_index` when it ran; 1-based).
    pub cycle: i64,
    /// When it finished (epoch seconds).
    pub at: f64,
    #[serde(default)]
    pub emitted: i64,
    #[serde(default)]
    pub dropped: i64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub relevant_tiers: Vec<u8>,
    #[serde(default)]
    pub emitted_tasks: Vec<EmittedSummary>,
    #[serde(default)]
    pub prefiltered_tiers: Vec<u8>,
    #[serde(default)]
    pub discovery_cache: String,
    #[serde(default)]
    pub dropped_findings: Vec<DroppedSummary>,
}

/// The discovery-cache wording of a cycle that never reached discovery
/// (the queue was already at its target).
pub const DISCOVERY_NOT_CONSULTED: &str = "not consulted";

impl CycleSummary {
    /// Whether the cycle decided anything: it ran discovery, or emitted or
    /// dropped work.
    pub fn decided(&self) -> bool {
        self.emitted > 0 || self.dropped > 0 || self.discovery_cache != DISCOVERY_NOT_CONSULTED
    }
}

/// What the autopilot daemon is doing right now, for the status surface.
///
/// Timestamps are epoch seconds so the reader can compute a countdown
/// without parsing -- both processes share one machine clock.
/// `next_cycle_at` is set only while `phase == PHASE_IDLE` (sleeping
/// between cycles); during a cycle it is `None` (the daemon is busy, not
/// waiting).
///
/// `expires_at` is the liveness-record freshness deadline (epoch seconds):
/// the wall-clock instant at/after which this record reads as not-live. The
/// daemon pushes it forward on every write to `now + interval + grace` so a
/// healthy daemon -- idling between cycles or busy inside one -- always
/// reads live, while a dead daemon's last record lapses and reads not-live.
/// It is the autopilot analogue of the worker lease's `lease_expires_at`;
/// the read is the identical `deadline > now` comparison (see
/// [`is_record_live`]). A record with `expires_at == None` (e.g. an older
/// writer) reads not-live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutopilotActivity {
    pub pid: i64,
    pub phase: String,
    pub cycle_index: i64,
    pub updated_at: f64,
    pub interval_seconds: f64,
    #[serde(default)]
    pub next_cycle_at: Option<f64>,
    #[serde(default)]
    pub expires_at: Option<f64>,
    #[serde(default)]
    pub last_emitted: i64,
    #[serde(default)]
    pub last_dropped: i64,
    #[serde(default)]
    pub last_reason: String,
    #[serde(default)]
    pub last_relevant_tiers: Vec<u8>,
    #[serde(default)]
    pub last_emitted_tasks: Vec<EmittedSummary>,
    /// Tiers the last cycle's structural pre-filter skipped before discovery.
    #[serde(default)]
    pub last_prefiltered_tiers: Vec<u8>,
    /// How the last cycle used the discovery-verdict cache (`hit`, `miss`,
    /// `partial (...)`, `disabled (...)`, `not consulted`).
    #[serde(default)]
    pub last_discovery_cache: String,
    /// The last cycle's dropped findings with their reasons (at most
    /// [`MAX_DROPPED_SUMMARIES`]; `last_dropped` is the full count).
    #[serde(default)]
    pub last_dropped_findings: Vec<DroppedSummary>,
    /// The cycle the `last_*` summary describes (0: none recorded yet, or
    /// a record from before this field). A failed cycle records no
    /// summary, so this can trail `cycle_index` by more than one.
    #[serde(default)]
    pub last_cycle: i64,
    /// The newest cycle that decided anything ([`CycleSummary::decided`]),
    /// kept across later no-op cycles.
    #[serde(default)]
    pub last_decision: Option<CycleSummary>,
}

impl AutopilotActivity {
    /// A snapshot with no last-cycle summary and no freshness deadline.
    pub fn new(
        pid: i64,
        phase: impl Into<String>,
        cycle_index: i64,
        updated_at: f64,
        interval_seconds: f64,
    ) -> Self {
        Self {
            pid,
            phase: phase.into(),
            cycle_index,
            updated_at,
            interval_seconds,
            next_cycle_at: None,
            expires_at: None,
            last_emitted: 0,
            last_dropped: 0,
            last_reason: String::new(),
            last_relevant_tiers: Vec::new(),
            last_emitted_tasks: Vec::new(),
            last_prefiltered_tiers: Vec::new(),
            last_discovery_cache: String::new(),
            last_dropped_findings: Vec::new(),
            last_cycle: 0,
            last_decision: None,
        }
    }
}

fn to_value(activity: &AutopilotActivity) -> Value {
    json!({
        "schema_version": ACTIVITY_SCHEMA_VERSION,
        "pid": activity.pid,
        "phase": activity.phase,
        "cycle_index": activity.cycle_index,
        "updated_at": activity.updated_at,
        "interval_seconds": activity.interval_seconds,
        "next_cycle_at": activity.next_cycle_at,
        "expires_at": activity.expires_at,
        "last_emitted": activity.last_emitted,
        "last_dropped": activity.last_dropped,
        "last_reason": activity.last_reason,
        "last_relevant_tiers": activity.last_relevant_tiers,
        "last_emitted_tasks": activity.last_emitted_tasks,
        "last_prefiltered_tiers": activity.last_prefiltered_tiers,
        "last_discovery_cache": activity.last_discovery_cache,
        "last_dropped_findings": activity.last_dropped_findings,
        "last_cycle": activity.last_cycle,
        "last_decision": activity.last_decision,
    })
}

/// Python `int(value)`: an integral number, a float truncated toward zero,
/// or a numeric string.
fn as_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f.trunc() as i64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// Python `float(value)`: any number, or a numeric string.
fn as_float(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Python `str(value)` for the string-typed fields.
fn as_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Null => Some("None".to_owned()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.to_owned()),
        other => Some(other.to_string()),
    }
}

/// An optional epoch field: absent/null or unparseable reads as `None`.
fn optional_float(value: Option<&Value>) -> Option<f64> {
    match value {
        None | Some(Value::Null) => None,
        other => as_float(other),
    }
}

fn from_value(data: &Value) -> Option<AutopilotActivity> {
    let Value::Object(data) = data else {
        return None;
    };
    if data.get("schema_version").and_then(Value::as_u64) != Some(ACTIVITY_SCHEMA_VERSION) {
        return None;
    }
    let pid = as_int(data.get("pid"))?;
    let phase = as_text(data.get("phase"))?;
    let cycle_index = as_int(data.get("cycle_index"))?;
    let updated_at = as_float(data.get("updated_at"))?;
    let interval_seconds = as_float(data.get("interval_seconds"))?;

    let mut tasks = Vec::new();
    if let Some(Value::Array(raw_tasks)) = data.get("last_emitted_tasks") {
        for entry in raw_tasks {
            let Value::Object(entry) = entry else {
                continue;
            };
            let (Some(task_id), Some(tier)) =
                (as_text(entry.get("task_id")), as_int(entry.get("tier")))
            else {
                continue;
            };
            let Ok(tier) = u8::try_from(tier) else {
                continue;
            };
            tasks.push(EmittedSummary { task_id, tier });
        }
    }

    let mut dropped = Vec::new();
    if let Some(Value::Array(raw_dropped)) = data.get("last_dropped_findings") {
        for entry in raw_dropped {
            let Value::Object(entry) = entry else {
                continue;
            };
            let (Some(finding_id), Some(tier)) =
                (as_text(entry.get("finding_id")), as_int(entry.get("tier")))
            else {
                continue;
            };
            let Ok(tier) = u8::try_from(tier) else {
                continue;
            };
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(|v| as_text(Some(v)))
                    .unwrap_or_default()
            };
            dropped.push(DroppedSummary {
                finding_id,
                tier,
                title: text("title"),
                reason: text("reason"),
            });
        }
    }

    let tier_list = |key: &str| -> Vec<u8> {
        match data.get(key) {
            Some(Value::Array(raw_tiers)) => raw_tiers
                .iter()
                .filter_map(|t| as_int(Some(t)))
                .filter_map(|t| u8::try_from(t).ok())
                .collect(),
            _ => Vec::new(),
        }
    };

    Some(AutopilotActivity {
        pid,
        phase,
        cycle_index,
        updated_at,
        interval_seconds,
        next_cycle_at: optional_float(data.get("next_cycle_at")),
        expires_at: optional_float(data.get("expires_at")),
        last_emitted: as_int(data.get("last_emitted")).unwrap_or(0),
        last_dropped: as_int(data.get("last_dropped")).unwrap_or(0),
        last_reason: data
            .get("last_reason")
            .and_then(|v| as_text(Some(v)))
            .unwrap_or_default(),
        last_relevant_tiers: tier_list("last_relevant_tiers"),
        last_emitted_tasks: tasks,
        last_prefiltered_tiers: tier_list("last_prefiltered_tiers"),
        last_discovery_cache: data
            .get("last_discovery_cache")
            .and_then(|v| as_text(Some(v)))
            .unwrap_or_default(),
        last_dropped_findings: dropped,
        last_cycle: as_int(data.get("last_cycle")).unwrap_or(0),
        // Lenient: a malformed nested summary reads as none, never as a
        // missing record.
        last_decision: data
            .get("last_decision")
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
    })
}

/// Atomically write the activity snapshot to `path`.
///
/// Writes a sibling temp file and renames it over the target so a reader
/// never observes a half-written JSON document. The parent directory is
/// created if missing. Best-effort by contract on the daemon side: the
/// caller should not let an activity-write failure crash a refill cycle.
pub fn write_activity(path: &Path, activity: &AutopilotActivity) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "activity".to_owned());
    let tmp = parent.join(format!(
        ".activity-{file_name}-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let body = serde_json::to_string(&to_value(activity)).map_err(std::io::Error::other)?;
    let written = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Read the activity snapshot, or `None` if absent/malformed.
///
/// Never fails: a missing file, an unreadable file, malformed JSON, or an
/// unrecognized schema version all read back as `None` so the status
/// surface degrades to plain liveness rather than erroring.
pub fn read_activity(path: Option<&Path>) -> Option<AutopilotActivity> {
    let text = std::fs::read_to_string(path?).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    from_value(&data)
}

/// Whether the record represents a live daemon at `now` (epoch seconds).
///
/// The autopilot analogue of the worker lease's `lease_expires_at > now`: a
/// record whose `expires_at` freshness deadline is strictly in the future
/// reads as live; a record at/before `now` (a lapsed record) reads as
/// not-live, so a truly dead daemon is respawned rather than adopted
/// forever. A missing record (`None`) or one without a freshness deadline
/// (`expires_at == None`) is not-live -- the same "no lease, no live worker"
/// default the worker takes for an absent `task_claims` row.
pub fn is_record_live(activity: Option<&AutopilotActivity>, now: f64) -> bool {
    activity
        .and_then(|a| a.expires_at)
        .is_some_and(|expires_at| expires_at > now)
}

/// The operator's pause marker for the daemon whose activity record is
/// `activity_path`: `<activity_path>.paused`. While it exists the daemon
/// runs no refill pass (see `autopilot_run`); removing it resumes.
pub fn pause_marker_path(activity_path: &Path) -> std::path::PathBuf {
    let mut name = activity_path.as_os_str().to_owned();
    name.push(".paused");
    std::path::PathBuf::from(name)
}

/// Whether the daemon recording to `activity_path` is held by its pause
/// marker.
pub fn is_paused(activity_path: &Path) -> bool {
    pause_marker_path(activity_path).is_file()
}

/// Pause (`true`: write the marker) or resume (`false`: remove it) the
/// daemon recording to `activity_path`. Returns whether anything changed.
/// Takes effect at the daemon's next cycle boundary: a running pass
/// finishes first.
pub fn set_paused(activity_path: &Path, paused: bool) -> std::io::Result<bool> {
    let marker = pause_marker_path(activity_path);
    if paused {
        if marker.is_file() {
            return Ok(false);
        }
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&marker, format!("paused at {}\n", epoch_now()))?;
        Ok(true)
    } else {
        match std::fs::remove_file(&marker) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// The shared machine clock, in epoch seconds.
pub fn epoch_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Read the liveness record and return it only if it reads live at `now`.
///
/// Composes [`read_activity`] with [`is_record_live`] so a caller (a second
/// supervisor) gets the live daemon's record -- `pid` and all -- to adopt,
/// or `None` when there is no live daemon to adopt (missing, malformed, or
/// stale record). `now` defaults to the shared machine clock; tests inject
/// a fixed instant. The returned record's `pid` lets the caller distinguish
/// a foreign daemon from a child it owns.
pub fn read_live_activity(path: Option<&Path>, now: Option<f64>) -> Option<AutopilotActivity> {
    let activity = read_activity(path)?;
    let moment = now.unwrap_or_else(epoch_now);
    is_record_live(Some(&activity), moment).then_some(activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn full_activity() -> AutopilotActivity {
        AutopilotActivity {
            pid: 4321,
            phase: PHASE_IDLE.to_owned(),
            cycle_index: 7,
            updated_at: 1000.0,
            interval_seconds: 300.0,
            next_cycle_at: Some(1300.0),
            expires_at: Some(1400.0),
            last_emitted: 2,
            last_dropped: 1,
            last_reason: "emitted 2, dropped 1".to_owned(),
            last_relevant_tiers: vec![1, 3],
            last_emitted_tasks: vec![
                EmittedSummary {
                    task_id: "fix-a".to_owned(),
                    tier: 1,
                },
                EmittedSummary {
                    task_id: "fix-b".to_owned(),
                    tier: 3,
                },
            ],
            last_prefiltered_tiers: vec![1],
            last_discovery_cache: "hit".to_owned(),
            last_dropped_findings: vec![DroppedSummary::new(
                "t10-readme",
                10,
                "Document the CLI",
                "no out-of-band grader",
            )],
            last_cycle: 7,
            last_decision: Some(CycleSummary {
                cycle: 6,
                at: 900.0,
                emitted: 1,
                dropped: 1,
                reason: "emitted 1".to_owned(),
                relevant_tiers: vec![10],
                emitted_tasks: vec![],
                prefiltered_tiers: vec![1, 2],
                discovery_cache: "miss".to_owned(),
                dropped_findings: vec![DroppedSummary::new("t10-x", 10, "x", "why")],
            }),
        }
    }

    #[test]
    fn a_cycle_decided_when_it_ran_discovery_or_moved_work() {
        let mut summary = CycleSummary {
            discovery_cache: DISCOVERY_NOT_CONSULTED.to_owned(),
            ..CycleSummary::default()
        };
        assert!(!summary.decided());
        summary.dropped = 1;
        assert!(summary.decided());
        summary.dropped = 0;
        summary.discovery_cache = "miss".to_owned();
        assert!(summary.decided());
    }

    #[test]
    fn a_malformed_last_decision_reads_as_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        std::fs::write(
            &path,
            json!({
                "schema_version": ACTIVITY_SCHEMA_VERSION,
                "pid": 1, "phase": "idle", "cycle_index": 3,
                "updated_at": 1.0, "interval_seconds": 5.0,
                "last_cycle": 2,
                "last_decision": {"cycle": "x"},
            })
            .to_string(),
        )
        .unwrap();
        let back = read_activity(Some(&path)).unwrap();
        assert_eq!(back.last_cycle, 2);
        assert_eq!(back.last_decision, None);
    }

    #[test]
    fn the_pause_marker_sits_beside_the_record_and_toggles_idempotently() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("logs/activity.json");
        assert_eq!(
            pause_marker_path(&path),
            tmp.path().join("logs/activity.json.paused")
        );
        assert!(!is_paused(&path));
        assert!(set_paused(&path, true).unwrap());
        assert!(is_paused(&path));
        assert!(!set_paused(&path, true).unwrap());
        assert!(set_paused(&path, false).unwrap());
        assert!(!is_paused(&path));
        assert!(!set_paused(&path, false).unwrap());
    }

    #[test]
    fn a_long_dropped_reason_is_cut() {
        let long = "x".repeat(MAX_DROPPED_REASON_CHARS + 50);
        let summary = DroppedSummary::new("f", 7, "t", &long);
        assert_eq!(summary.reason.chars().count(), MAX_DROPPED_REASON_CHARS + 3);
    }

    // --- on-disk round-trip ----------------------------------------------------------

    #[test]
    fn write_read_round_trip_preserves_all_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        let activity = full_activity();
        write_activity(&path, &activity).unwrap();
        assert_eq!(read_activity(Some(&path)), Some(activity));
    }

    #[test]
    fn write_creates_missing_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("deep").join("nested").join("activity.json");
        write_activity(
            &path,
            &AutopilotActivity::new(1, PHASE_STARTING, 0, 0.0, 300.0),
        )
        .unwrap();
        assert!(read_activity(Some(&path)).is_some());
    }

    #[test]
    fn write_overwrites_prior_snapshot_and_leaves_no_temp_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        write_activity(
            &path,
            &AutopilotActivity::new(1, PHASE_RUNNING, 1, 0.0, 300.0),
        )
        .unwrap();
        write_activity(&path, &AutopilotActivity::new(1, PHASE_IDLE, 1, 0.0, 300.0)).unwrap();
        let back = read_activity(Some(&path)).unwrap();
        assert_eq!(back.phase, PHASE_IDLE);
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["activity.json".to_owned()]);
    }

    #[test]
    fn written_document_carries_the_schema_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        write_activity(&path, &full_activity()).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["schema_version"], json!(ACTIVITY_SCHEMA_VERSION));
        assert_eq!(raw["last_emitted_tasks"][0]["task_id"], json!("fix-a"));
        assert_eq!(raw["next_cycle_at"], json!(1300.0));
    }

    // --- degrade-to-None reads -------------------------------------------------------

    #[test]
    fn read_missing_file_is_none() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read_activity(Some(&tmp.path().join("nope.json"))), None);
    }

    #[test]
    fn read_none_path_is_none() {
        assert_eq!(read_activity(None), None);
    }

    #[test]
    fn read_malformed_json_is_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(read_activity(Some(&path)), None);
    }

    #[test]
    fn read_wrong_schema_version_is_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version": {}, "pid": 1}}"#,
                ACTIVITY_SCHEMA_VERSION + 99
            ),
        )
        .unwrap();
        assert_eq!(read_activity(Some(&path)), None);
    }

    #[test]
    fn read_missing_required_field_is_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        std::fs::write(
            &path,
            json!({"schema_version": ACTIVITY_SCHEMA_VERSION, "pid": 1, "phase": "idle"})
                .to_string(),
        )
        .unwrap();
        assert_eq!(read_activity(Some(&path)), None);
    }

    #[test]
    fn read_tolerates_malformed_optional_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        std::fs::write(
            &path,
            json!({
                "schema_version": ACTIVITY_SCHEMA_VERSION,
                "pid": "77",
                "phase": "idle",
                "cycle_index": 2.0,
                "updated_at": 10,
                "interval_seconds": "30",
                "next_cycle_at": "soon",
                "expires_at": null,
                "last_emitted": null,
                "last_relevant_tiers": [1, "x", 3],
                "last_emitted_tasks": [{"task_id": "a", "tier": 2}, "junk", {"task_id": "b"}],
            })
            .to_string(),
        )
        .unwrap();
        let back = read_activity(Some(&path)).unwrap();
        assert_eq!(back.pid, 77);
        assert_eq!(back.cycle_index, 2);
        assert_eq!(back.interval_seconds, 30.0);
        assert_eq!(back.next_cycle_at, None);
        assert_eq!(back.expires_at, None);
        assert_eq!(back.last_emitted, 0);
        assert_eq!(back.last_relevant_tiers, vec![1, 3]);
        assert_eq!(
            back.last_emitted_tasks,
            vec![EmittedSummary {
                task_id: "a".to_owned(),
                tier: 2
            }]
        );
    }

    // --- liveness --------------------------------------------------------------------

    #[test]
    fn record_is_live_only_while_its_deadline_is_in_the_future() {
        let mut activity = AutopilotActivity::new(1, PHASE_IDLE, 1, 0.0, 300.0);
        assert!(!is_record_live(Some(&activity), 10.0));
        activity.expires_at = Some(20.0);
        assert!(is_record_live(Some(&activity), 10.0));
        assert!(!is_record_live(Some(&activity), 20.0));
        assert!(!is_record_live(Some(&activity), 25.0));
        assert!(!is_record_live(None, 0.0));
    }

    #[test]
    fn read_live_activity_returns_only_a_live_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        let activity = full_activity();
        write_activity(&path, &activity).unwrap();
        assert_eq!(
            read_live_activity(Some(&path), Some(1399.0)),
            Some(activity)
        );
        assert_eq!(read_live_activity(Some(&path), Some(1400.0)), None);
        assert_eq!(
            read_live_activity(Some(&tmp.path().join("missing.json")), Some(0.0)),
            None
        );
        assert_eq!(read_live_activity(None, Some(0.0)), None);
    }

    #[test]
    fn read_live_activity_defaults_to_the_machine_clock() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("activity.json");
        let mut activity = full_activity();
        activity.expires_at = Some(epoch_now() + 3600.0);
        write_activity(&path, &activity).unwrap();
        assert!(read_live_activity(Some(&path), None).is_some());
        activity.expires_at = Some(epoch_now() - 1.0);
        write_activity(&path, &activity).unwrap();
        assert!(read_live_activity(Some(&path), None).is_none());
    }
}
